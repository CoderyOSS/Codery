//! Integration: agent-token lifecycle over Telegram (GATE-003/006).
//! `/token mint {name}` replies with the raw token exactly once (plus a
//! "store this now" note), `/token revoke {name}` confirms with an
//! affected-agent note, `/token list` shows names + timestamps and never
//! hashes or raw tokens. Mint/revoke are audited by the store with the
//! token NAME — the raw token never reaches any other message, the audit
//! log, or a second copy inside the mint reply itself.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gate::audit::Audit;
use gate::state::State as DbState;
use gate::telegram::commands::{register_commands, CommandDeps};
use gate::telegram::fake::FakeTelegram;
use gate::telegram::send::SendRequest;
use gate::telegram::{MsgRef, TelegramApi, TelegramRouter, Update};
use gate::tokens::TokenStore;

const APPROVER: i64 = 42;
const COMMAND_MSG_ID: i32 = 500;

// ---------------------------------------------------------------------------
// Harness: state + audit + router with only the command handler registered.
// Updates are dispatched directly (no poll loop) so every test is
// synchronous and deterministic.
// ---------------------------------------------------------------------------

struct Harness {
    fake: Arc<FakeTelegram>,
    state: Arc<Mutex<DbState>>,
    audit: Arc<Audit>,
    audit_path: PathBuf,
    dir: PathBuf,
    router: TelegramRouter,
    api: Arc<dyn TelegramApi>,
}

impl Harness {
    async fn open() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "gate-tg-tokens-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let audit_path = dir.join("audit.jsonl");
        let audit = Arc::new(Audit::open(&audit_path));
        let state = Arc::new(Mutex::new(DbState::open(":memory:").unwrap()));
        let fake = Arc::new(FakeTelegram::new());
        let api: Arc<dyn TelegramApi> = fake.clone();
        let machine_token: Arc<dyn Fn() -> Option<String> + Send + Sync> =
            Arc::new(|| None);
        let mut router = TelegramRouter::new();
        register_commands(
            &mut router,
            Arc::new(CommandDeps {
                state: state.clone(),
                audit: audit.clone(),
                machine_token,
                lifecycle: None,
            grant: None,
            }),
        );
        Self {
            fake,
            state,
            audit,
            audit_path,
            dir,
            router,
            api,
        }
    }

    /// Run one operator command through the router; return the single edit
    /// it produced (reply to the command message).
    async fn command(&self, text: &str) -> SendRequest {
        let before = self.fake.edits().len();
        let update = Update {
            sender_id: Some(APPROVER),
            text: Some(text.to_string()),
            message_ref: Some(MsgRef {
                chat_id: 100_000,
                message_id: COMMAND_MSG_ID,
            }),
            ..Default::default()
        };
        self.router.dispatch(&update, &self.api).await;
        let edits = self.fake.edits();
        assert_eq!(
            edits.len(),
            before + 1,
            "one command ⇒ exactly one reply edit: {edits:?}"
        );
        edits.last().unwrap().1.clone()
    }

    /// Mint through the command and extract the raw token from the reply
    /// (it rides on its own line, 42-44 base62 chars).
    async fn mint_via_command(&self, name: &str) -> (SendRequest, String) {
        let reply = self.command(&format!("/token mint {name}")).await;
        (reply.clone(), raw_token_in(&reply))
    }

    fn validate(&self, raw: &str) -> Option<String> {
        let guard = self.state.lock().unwrap();
        TokenStore::new(&guard, &self.audit)
            .validate(raw)
            .map(|i| i.name)
    }

    fn listing(&self) -> Vec<(String, i64, Option<i64>)> {
        let guard = self.state.lock().unwrap();
        TokenStore::new(&guard, &self.audit)
            .list()
            .into_iter()
            .map(|t| (t.name, t.created_at, t.last_used_at))
            .collect()
    }

    fn audit_contents(&self) -> String {
        std::fs::read_to_string(&self.audit_path).unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The raw token line in a mint reply: a standalone base62 run of 42-44
/// characters (256 random bits in base62).
fn raw_token_in(reply: &SendRequest) -> String {
    reply
        .text
        .lines()
        .find(|l| {
            (40..=44).contains(&l.chars().count())
                && l.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .expect("mint reply carries the raw token on its own line")
        .to_string()
}

/// Longest alphanumeric run in a text — anything ≥40 would be
/// token/hash-shaped material that must never render.
fn longest_alnum_run(text: &str) -> usize {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Mint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mint_reply_shows_token_exactly_once_and_it_validates() {
    let h = Harness::open().await;
    let (reply, raw) = h.mint_via_command("claude-code").await;

    // The exposed token is the real thing: it validates to the agent name.
    assert_eq!(
        h.validate(&raw),
        Some("claude-code".to_string()),
        "the token in the mint reply must authenticate"
    );
    // Exactly once: one occurrence in the text, no button copy.
    assert_eq!(
        reply.text.matches(&raw).count(),
        1,
        "raw token must appear exactly once in the reply: {}",
        reply.text
    );
    assert!(
        reply.inline_buttons.is_empty(),
        "raw token must never ride in callback data: {:?}",
        reply.inline_buttons
    );
    // The once-only warning is part of the reply.
    assert!(
        reply.text.contains("only once"),
        "mint reply carries the store-this-now note: {}",
        reply.text
    );
}

#[tokio::test]
async fn mint_duplicate_name_rotates_and_the_reply_says_so() {
    let h = Harness::open().await;
    let (_r1, old) = h.mint_via_command("rotating-agent").await;

    let (reply, new) = h.mint_via_command("rotating-agent").await;

    // Documented behaviour (core Task 5): same name ⇒ ROTATE — fresh raw
    // token, previous one invalidated.
    assert_ne!(old, new, "rotation mints fresh randomness");
    assert_eq!(h.validate(&new), Some("rotating-agent".to_string()));
    assert!(
        h.validate(&old).is_none(),
        "rotation invalidates the previous token"
    );
    // The reply announces the rotation, not a silent fresh mint.
    assert!(
        reply.text.contains("ROTATED") || reply.text.contains("rotated"),
        "duplicate-name mint reply must say it rotated: {}",
        reply.text
    );
    assert_eq!(reply.text.matches(&new).count(), 1);
}

#[tokio::test]
async fn mint_without_or_with_bad_name_gets_usage_reply() {
    let h = Harness::open().await;

    for bad in [
        "/token",                // no subcommand
        "/token mint",           // no name
        "/token mint bad name!", // extra words
        "/token mint bad$name",  // invalid character
        "/token mint -bad name", // space splits ⇒ extra word
    ] {
        let reply = h.command(bad).await;
        assert!(
            reply.text.contains("Usage:"),
            "'{bad}' must answer with usage: {}",
            reply.text
        );
    }
    // 65 characters is one too many.
    let too_long = format!("/token mint {}", "a".repeat(65));
    let reply = h.command(&too_long).await;
    assert!(
        reply.text.contains("Usage:"),
        "65-char name rejected: {}",
        reply.text
    );

    // Nothing was minted by any of the rejected commands.
    assert!(
        h.listing().is_empty(),
        "rejected commands must not mint: {:?}",
        h.listing()
    );
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_shows_names_and_timestamps_never_tokens_or_hashes() {
    let h = Harness::open().await;
    let (_, raw_a) = h.mint_via_command("alpha").await;
    let (_, raw_b) = h.mint_via_command("beta_2").await;
    // Use alpha once so its last_used_at stamps; beta stays unused.
    assert!(h.validate(&raw_a).is_some());
    let rows = h.listing();
    assert_eq!(rows.len(), 2);

    let reply = h.command("/token list").await;
    let text = reply.text;

    assert!(text.contains("alpha"), "list shows the name: {text}");
    assert!(text.contains("beta_2"), "list shows the name: {text}");
    assert!(text.contains("created"), "list shows creation: {text}");
    assert!(text.contains("never"), "unused token's last-used renders: {text}");
    // The used token shows its last-used timestamp (unix seconds).
    let used_at = rows.iter().find(|(n, ..)| n == "alpha").unwrap().2.unwrap();
    assert!(
        text.contains(&used_at.to_string()),
        "list shows last-used timestamp: {text}"
    );

    // NEVER secret-shaped material: no raw token, and no ≥40-char
    // alphanumeric run anywhere (covers raw tokens AND 64-char sha-256
    // hashes in one check).
    assert!(!text.contains(&raw_a), "raw token must not render: {text}");
    assert!(!text.contains(&raw_b), "raw token must not render: {text}");
    assert!(
        longest_alnum_run(&text) < 40,
        "no 40+-char token-looking substring may appear: {text}"
    );
}

#[tokio::test]
async fn empty_list_replies_no_tokens() {
    let h = Harness::open().await;
    let reply = h.command("/token list").await;
    assert!(
        reply.text.contains("No agent tokens"),
        "empty list is graceful: {}",
        reply.text
    );
}

// ---------------------------------------------------------------------------
// Revoke
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revoke_kills_the_token_and_confirms_agent_impact() {
    let h = Harness::open().await;
    let (_, raw) = h.mint_via_command("claude-code").await;
    assert!(h.validate(&raw).is_some());

    let reply = h.command("/token revoke claude-code").await;
    assert!(
        reply.text.contains("claude-code") && reply.text.contains("revoked"),
        "confirmation names the token: {}",
        reply.text
    );
    assert!(
        reply.text.contains("agent"),
        "confirmation notes the affected agent: {}",
        reply.text
    );
    assert!(
        h.validate(&raw).is_none(),
        "revoked token must stop validating"
    );
    // The raw token itself never renders in the revoke reply.
    assert!(!reply.text.contains(&raw), "no raw in revoke reply: {}", reply.text);
}

#[tokio::test]
async fn revoke_unknown_name_replies_graceful_no_such_token() {
    let h = Harness::open().await;
    let reply = h.command("/token revoke ghost").await;
    assert!(
        reply.text.contains("No such token") && reply.text.contains("ghost"),
        "graceful unknown-name reply: {}",
        reply.text
    );
    // And it did not fabricate a row.
    assert!(h.listing().is_empty());
}

// ---------------------------------------------------------------------------
// Audit: name yes, raw token never
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mint_and_revoke_audited_with_name_but_never_the_raw_token() {
    let h = Harness::open().await;
    let (_reply, raw) = h.mint_via_command("audited-agent").await;
    h.command("/token revoke audited-agent").await;

    let audit = h.audit_contents();
    assert!(audit.contains("token.mint"), "mint audited: {audit}");
    assert!(audit.contains("token.revoke"), "revoke audited: {audit}");
    assert!(
        audit.contains("audited-agent"),
        "audit entries carry the token NAME: {audit}"
    );
    assert!(
        !audit.contains(&raw),
        "raw token must never reach the audit log: {audit}"
    );
    // One audit entry per lifecycle event — the command layer must not
    // double-record what the store already audits.
    assert_eq!(audit.matches("token.mint").count(), 1, "{audit}");
    assert_eq!(audit.matches("token.revoke").count(), 1, "{audit}");
}

/// The raw token from mint never leaks into ANY later reply (list, status)
/// — the mint message is the single plaintext exposure.
#[tokio::test]
async fn raw_token_never_reappears_in_later_replies() {
    let h = Harness::open().await;
    let (_, raw) = h.mint_via_command("claude-code").await;

    let list_reply = h.command("/token list").await;
    let status_reply = h.command("/status").await;
    let revoke_reply = h.command("/token revoke claude-code").await;
    for reply in [list_reply, status_reply, revoke_reply] {
        assert!(
            !reply.text.contains(&raw),
            "raw token must not reappear: {}",
            reply.text
        );
        assert!(
            longest_alnum_run(&reply.text) < 40,
            "no token-shaped substring in later replies: {}",
            reply.text
        );
    }
}
