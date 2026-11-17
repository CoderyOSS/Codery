use serde::Deserialize;

/// Classification of a git receive-pack ref update (GATE-005).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionClass {
    /// Ordinary push (including ref creation).
    Push,
    /// Non-fast-forward update.
    ForcePush,
    /// Deletion of a branch ref.
    BranchDelete,
    /// Deletion of a tag ref.
    TagDelete,
}

/// Ancestry-dependent refinement of an update class (GATE-005): the parser
/// can decide deletions and creations on oid shape alone; an ordinary update
/// needs the fast-forward check before it is Push or ForcePush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    Class(ActionClass),
    NeedsAncestry,
}

/// Fast-forward verdict for an ordinary update (GATE-005). `Unknown` covers
/// every case the upstream could not answer definitively and fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ancestry {
    /// `new` is a strict descendant of `old`.
    FastForward,
    /// `new` is not a descendant of `old` (behind or diverged).
    Force,
    /// The upstream compare could not be trusted; deny.
    Unknown,
}

/// The authenticated caller's permission tier on a repo (GATE-008), ordered
/// highest first so tiers compare directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Admin,
    Maintain,
    Write,
    Triage,
    Read,
    None,
}

/// Policy outcome for an action (GATE-005). Unconfigured defaults to Deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Decision {
    #[default]
    Deny,
    Allow,
    Approve,
}

impl<'de> Deserialize<'de> for Decision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        // Anything the config does not spell out explicitly fails closed.
        Ok(match s.as_str() {
            "allow" => Decision::Allow,
            "approve" => Decision::Approve,
            _ => Decision::Deny,
        })
    }
}
