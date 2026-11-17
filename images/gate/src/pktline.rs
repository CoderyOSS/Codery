// receive-pack pkt-line parsing (GATE-005): fail closed on malformed input.

use crate::types::{ActionClass, Classification};

/// A 40-hex-digit git object name (SHA-1), stored as raw bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Oid([u8; 20]);

/// One ref update command from a `git-receive-pack` request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    pub old: Oid,
    pub new: Oid,
    pub refname: String,
}

/// Fail-closed parse failure (GATE-005): malformed input never yields a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Body contains no pkt-lines at all.
    Empty,
    /// A pkt-line length header or payload ends mid-packet.
    Truncated,
    /// The body is structurally invalid for a receive-pack command list.
    Malformed(&'static str),
}

/// True when every oid byte is zero: the all-zeros oid (`000…0`, 40 hex
/// `'0'` chars) that marks ref creation (old) and deletion (new).
pub fn is_zero(oid: &Oid) -> bool {
    oid.0 == [0u8; 20]
}

/// Classify an update by oid shape alone (GATE-005): zero-new on a
/// `refs/heads/`/`refs/tags/` name is a branch/tag delete; zero-old is a
/// creation (an ordinary push); anything else needs the fast-forward check.
pub fn classify_ref_update(update: &RefUpdate) -> Classification {
    if is_zero(&update.new) {
        if update.refname.starts_with("refs/heads/") {
            Classification::Class(ActionClass::BranchDelete)
        } else if update.refname.starts_with("refs/tags/") {
            Classification::Class(ActionClass::TagDelete)
        } else {
            Classification::NeedsAncestry
        }
    } else if is_zero(&update.old) {
        Classification::Class(ActionClass::Push)
    } else {
        Classification::NeedsAncestry
    }
}

impl Oid {
    /// Lowercase hex rendering, for URLs that name objects (upstream
    /// compare API). Parsing is the inverse via [`Oid::from_hex`].
    pub fn to_hex(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::with_capacity(40);
        for byte in self.0 {
            let _ = write!(s, "{byte:02x}");
        }
        s
    }

    /// Parse exactly 40 lowercase hex digits into raw oid bytes.
    pub(crate) fn from_hex(s: &str) -> Result<Oid, ParseError> {
        let b = s.as_bytes();
        if b.len() != 40 {
            return Err(ParseError::Malformed("oid is not 40 hex digits"));
        }
        let mut out = [0u8; 20];
        for (i, slot) in out.iter_mut().enumerate() {
            let hi = hex_val(b[2 * i])?;
            let lo = hex_val(b[2 * i + 1])?;
            *slot = (hi << 4) | lo;
        }
        Ok(Oid(out))
    }
}

fn hex_val(byte: u8) -> Result<u8, ParseError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(ParseError::Malformed("oid is not 40 hex digits")),
    }
}

/// Parse the command list from a `git-receive-pack` request body: pkt-lines
/// of `<old-oid> SP <new-oid> SP <refname>` (the first carries `\0<caps>`),
/// terminated by a `0000` flush; anything after the flush (the packfile) is
/// left unread. Malformed input fails closed with [`ParseError`].
pub fn parse_receive_pack_updates(body: &[u8]) -> Result<Vec<RefUpdate>, ParseError> {
    let mut updates = Vec::new();
    let mut rest = body;
    loop {
        if rest.is_empty() {
            return Err(if updates.is_empty() {
                ParseError::Empty
            } else {
                ParseError::Truncated
            });
        }
        if rest.len() < 4 {
            return Err(ParseError::Truncated);
        }
        let len = pkt_len(&rest[..4])?;
        if len == 0 {
            // Flush-pkt: end of the command list; packfile (if any) follows.
            return Ok(updates);
        }
        if rest.len() < len {
            return Err(ParseError::Truncated);
        }
        let payload = &rest[4..len];
        rest = &rest[len..];

        let first = updates.is_empty();
        let command = if first {
            // Only the first pkt-line carries a NUL-terminated capabilities
            // suffix; without a NUL there simply are no capabilities.
            match payload.iter().position(|&b| b == 0) {
                Some(i) => &payload[..i],
                None => payload,
            }
        } else if payload.contains(&0) {
            return Err(ParseError::Malformed("NUL in non-first pkt-line"));
        } else {
            payload
        };

        updates.push(parse_command(command)?);
    }
}

/// Decode a 4-byte lowercase-hex pkt-line length header. Zero is the flush
/// marker; lengths 1..=3 cannot hold a header and are malformed.
fn pkt_len(hdr: &[u8]) -> Result<usize, ParseError> {
    let mut len = 0usize;
    for &byte in hdr {
        let digit = match byte {
            b'0'..=b'9' => (byte - b'0') as usize,
            b'a'..=b'f' => (byte - b'a' + 10) as usize,
            _ => return Err(ParseError::Malformed("invalid pkt-line length header")),
        };
        len = len * 16 + digit;
    }
    if len != 0 && len < 4 {
        return Err(ParseError::Malformed("pkt-line length below 4"));
    }
    Ok(len)
}

/// Parse one `<old> SP <new> SP <refname>` command payload.
fn parse_command(command: &[u8]) -> Result<RefUpdate, ParseError> {
    let s = std::str::from_utf8(command)
        .map_err(|_| ParseError::Malformed("non-UTF-8 ref update command"))?;
    let Some((old_s, rest)) = s.split_once(' ') else {
        return Err(ParseError::Malformed("update command is not <old> <new> <ref>"));
    };
    let Some((new_s, refname)) = rest.split_once(' ') else {
        return Err(ParseError::Malformed("update command is not <old> <new> <ref>"));
    };
    if refname.is_empty() || refname.contains(' ') {
        return Err(ParseError::Malformed("refname is empty or contains a space"));
    }
    Ok(RefUpdate {
        old: Oid::from_hex(old_s)?,
        new: Oid::from_hex(new_s)?,
        refname: refname.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ActionClass, Classification};

    const OLD: &str = "1234567890abcdef1234567890abcdef12345678";
    const NEW: &str = "fedcba0987654321fedcba0987654321fedcba09";
    const ZERO: &str = "0000000000000000000000000000000000000000";

    /// Decode a hex string of raw bytes (short hand-written fixtures only).
    fn body(hex: &str) -> Vec<u8> {
        let bytes = hex.as_bytes();
        assert_eq!(bytes.len() % 2, 0, "odd hex fixture");
        (0..bytes.len())
            .step_by(2)
            .map(|i| {
                let hi = (bytes[i] as char).to_digit(16).expect("hex digit");
                let lo = (bytes[i + 1] as char).to_digit(16).expect("hex digit");
                (hi * 16 + lo) as u8
            })
            .collect()
    }

    /// One pkt-line: 4 lowercase-hex length bytes (counting the header)
    /// followed by the payload, exactly as git frames it.
    fn pkt(payload: &str) -> Vec<u8> {
        let mut raw = format!("{:04x}", 4 + payload.len()).into_bytes();
        raw.extend_from_slice(payload.as_bytes());
        raw
    }

    /// The `0000` flush-pkt terminating a receive-pack command list.
    fn flush() -> Vec<u8> {
        b"0000".to_vec()
    }

    /// Raw packfile bytes a real request would carry after the flush.
    fn pack_bytes() -> Vec<u8> {
        let mut pack = b"PACK".to_vec();
        pack.extend_from_slice(&[0xde; 16]);
        pack
    }

    fn oid(hex: &str) -> Oid {
        Oid::from_hex(hex).expect("fixture oid")
    }

    #[test]
    fn pkt_helper_emits_exact_wire_bytes() {
        // Pins the fixture builder: "AB" -> pkt len 6 -> "0006" + "AB".
        assert_eq!(pkt("AB"), b"0006AB");
        assert_eq!(flush(), b"0000");
    }

    #[test]
    fn single_update_with_capabilities() {
        // <old> <new> refs/heads/main\0report-status side-band-64k, then flush.
        let mut raw = pkt(&format!(
            "{OLD} {NEW} refs/heads/main\0report-status side-band-64k"
        ));
        raw.extend(flush());
        let updates = parse_receive_pack_updates(&raw).expect("parses");
        assert_eq!(
            updates,
            vec![RefUpdate {
                old: oid(OLD),
                new: oid(NEW),
                refname: "refs/heads/main".to_string(),
            }]
        );
        assert!(!is_zero(&updates[0].old));
        assert!(!is_zero(&updates[0].new));
    }

    #[test]
    fn first_line_without_nul() {
        // Capabilities (and the NUL) are optional on the first pkt-line.
        let mut raw = pkt(&format!("{OLD} {NEW} refs/heads/main"));
        raw.extend(flush());
        let updates = parse_receive_pack_updates(&raw).expect("parses");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].refname, "refs/heads/main");
    }

    #[test]
    fn multi_ref_body_in_order() {
        let mut raw = Vec::new();
        raw.extend(pkt(&format!("{OLD} {NEW} refs/heads/main")));
        raw.extend(pkt(&format!(
            "{} {} refs/heads/dev",
            "a".repeat(40),
            "b".repeat(40)
        )));
        raw.extend(pkt(&format!(
            "{} {} refs/tags/v2",
            "c".repeat(40),
            "d".repeat(40)
        )));
        raw.extend(flush());
        let updates = parse_receive_pack_updates(&raw).expect("parses");
        assert_eq!(updates.len(), 3);
        assert_eq!(updates[0].refname, "refs/heads/main");
        assert_eq!(updates[0].old, oid(OLD));
        assert_eq!(updates[0].new, oid(NEW));
        assert_eq!(updates[1].refname, "refs/heads/dev");
        assert_eq!(updates[1].old, oid(&"a".repeat(40)));
        assert_eq!(updates[1].new, oid(&"b".repeat(40)));
        assert_eq!(updates[2].refname, "refs/tags/v2");
        assert_eq!(updates[2].old, oid(&"c".repeat(40)));
        assert_eq!(updates[2].new, oid(&"d".repeat(40)));
    }

    #[test]
    fn stops_at_flush_and_ignores_packfile() {
        // Commands, flush, then raw packfile bytes ("PACK" + junk).
        let mut raw = pkt(&format!("{OLD} {NEW} refs/heads/main"));
        raw.extend(flush());
        raw.extend(pack_bytes());
        let updates = parse_receive_pack_updates(&raw).expect("parses");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].refname, "refs/heads/main");
    }

    #[test]
    fn flush_only_body_yields_no_updates() {
        let updates = parse_receive_pack_updates(&flush()).expect("parses");
        assert!(updates.is_empty());
    }

    #[test]
    fn empty_body_is_parse_error() {
        assert_eq!(parse_receive_pack_updates(b""), Err(ParseError::Empty));
    }

    #[test]
    fn short_length_header_is_parse_error() {
        // Only two bytes of the 4-byte length header arrive.
        let raw = body("3030");
        assert_eq!(parse_receive_pack_updates(&raw), Err(ParseError::Truncated));
    }

    #[test]
    fn truncated_payload_is_parse_error() {
        // Header claims 0x65 bytes but the body ends after 15.
        let mut raw = pkt(&format!("{OLD} {NEW} refs/heads/main"));
        raw.truncate(15);
        assert_eq!(parse_receive_pack_updates(&raw), Err(ParseError::Truncated));
    }

    #[test]
    fn missing_flush_is_parse_error() {
        // Command pkt-line then EOF, no flush terminator.
        let raw = pkt(&format!("{OLD} {NEW} refs/heads/main"));
        assert_eq!(parse_receive_pack_updates(&raw), Err(ParseError::Truncated));
    }

    #[test]
    fn invalid_length_header_is_parse_error() {
        let raw = b"zzzzrefs/heads/x0000";
        assert!(matches!(
            parse_receive_pack_updates(raw),
            Err(ParseError::Malformed(_))
        ));
    }

    #[test]
    fn non_hex_oid_is_parse_error() {
        // "zz" * 20 where the new oid belongs: not a guess, an error.
        let mut raw = pkt(&format!("{OLD} {} refs/heads/main", "zz".repeat(20)));
        raw.extend(flush());
        assert!(matches!(
            parse_receive_pack_updates(&raw),
            Err(ParseError::Malformed(_))
        ));
    }

    #[test]
    fn command_with_missing_refname_is_parse_error() {
        // "<old> <new>" — no refname field after the second oid.
        let mut raw = pkt(&format!("{OLD} {NEW}"));
        raw.extend(flush());
        assert!(matches!(
            parse_receive_pack_updates(&raw),
            Err(ParseError::Malformed(_))
        ));
    }

    #[test]
    fn nul_in_non_first_pkt_line_is_parse_error() {
        // Only the first pkt-line may carry the capabilities NUL.
        let mut raw = pkt(&format!("{OLD} {NEW} refs/heads/main"));
        raw.extend(pkt(&format!("{OLD} {NEW} refs/heads/dev\0caps")));
        raw.extend(flush());
        assert!(matches!(
            parse_receive_pack_updates(&raw),
            Err(ParseError::Malformed(_))
        ));
    }

    #[test]
    fn is_zero_detects_all_zero_oid() {
        assert!(is_zero(&oid(ZERO)));
        assert!(!is_zero(&oid(OLD)));
        assert!(!is_zero(&oid(NEW)));
    }

    #[test]
    fn zero_new_on_refs_heads_is_branch_delete() {
        let u = RefUpdate {
            old: oid(OLD),
            new: oid(ZERO),
            refname: "refs/heads/feature".to_string(),
        };
        assert_eq!(
            classify_ref_update(&u),
            Classification::Class(ActionClass::BranchDelete)
        );
    }

    #[test]
    fn zero_new_on_refs_tags_is_tag_delete() {
        let u = RefUpdate {
            old: oid(OLD),
            new: oid(ZERO),
            refname: "refs/tags/v1".to_string(),
        };
        assert_eq!(
            classify_ref_update(&u),
            Classification::Class(ActionClass::TagDelete)
        );
    }

    #[test]
    fn zero_old_is_create_classified_as_push() {
        let u = RefUpdate {
            old: oid(ZERO),
            new: oid(NEW),
            refname: "refs/heads/main".to_string(),
        };
        assert_eq!(
            classify_ref_update(&u),
            Classification::Class(ActionClass::Push)
        );
    }

    #[test]
    fn nonzero_old_and_new_needs_ancestry() {
        let u = RefUpdate {
            old: oid(OLD),
            new: oid(NEW),
            refname: "refs/heads/main".to_string(),
        };
        assert_eq!(classify_ref_update(&u), Classification::NeedsAncestry);
    }
}
