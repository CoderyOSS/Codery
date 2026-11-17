//! Onboarding pages (GATE-002/004): `GET /` renders the copy-paste agent
//! setup — clone paths (public / private), commit identity, API env — with
//! the live gate host and, when supplied via `?token=`, the minted agent
//! token. `/setup/token` is the ONE-TIME intake form for the machine
//! account's classic PAT: a setup nonce (kv, 15-minute TTL, single use)
//! gates it, NOT the agent auth middleware, because at intake time nothing
//! else is provisioned yet. The submitted token is verified against
//! GitHub (login match + 2FA + scopes + expiry header) and only then
//! stored encrypted (`custody`); verification failure stores nothing and
//! keeps the nonce valid for retry. The token is never logged and never
//! echoed back.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Form, Router};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::custody::MachineTokenStore;
use crate::github::{Platform, VerifyError};
use crate::routes::GateState;
use crate::state::{self, State as DbState};

/// Setup nonce validity window (GATE-004/011 intake): 15 minutes.
pub const NONCE_TTL_SECS: i64 = 900;

/// kv key holding the current setup nonce (hex).
pub const KV_NONCE: &str = "setup_nonce";

/// kv key holding the nonce's unix-seconds expiry.
pub const KV_NONCE_EXPIRES: &str = "setup_nonce_expires";

/// Page + intake dependencies beyond the shared gate state.
pub struct OnePageDeps {
    /// Gate base URL used in the page's commands: `GATE_PUBLIC_URL` when
    /// configured, else the internal codery-net alias
    /// `http://gitauthgate:<port>` (GATE-001 fallback mode).
    pub gate_host: String,
    /// Expected machine account login (GATE-004): a token whose /user
    /// login differs is rejected with a message naming the login found.
    pub machine_login: String,
    pub platform: Arc<dyn Platform>,
    /// `GATE_TOKEN_ENC_KEY` value; custody derives its AES key from it.
    pub enc_key: String,
}

/// Mint a fresh single-use setup nonce: 32 random bytes (hex) with a
/// 15-minute expiry, stored in the kv table. The value is shown exactly
/// once — the control plane hands it to the operator (link shape:
/// `{gate_host}/setup/token?nonce=...`).
pub fn mint_setup_nonce(state: &DbState) -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| anyhow::anyhow!("entropy source failed: {e}"))?;
    let nonce = hex_encode(&bytes);
    state.kv_set(KV_NONCE, &nonce)?;
    state.kv_set(
        KV_NONCE_EXPIRES,
        &(state::now() + NONCE_TTL_SECS).to_string(),
    )?;
    Ok(nonce)
}

/// A nonce is valid when it matches the stored one (constant-time), is
/// unexpired, and has not been consumed. Missing/expired/consumed all
/// collapse to invalid ⇒ 410.
fn nonce_valid(state: &DbState, provided: &str) -> bool {
    let Ok(Some(stored)) = state.kv_get(KV_NONCE) else {
        return false;
    };
    let Ok(Some(expires)) = state.kv_get(KV_NONCE_EXPIRES) else {
        return false;
    };
    let Ok(expires) = expires.parse::<i64>() else {
        return false;
    };
    if state::now() >= expires {
        return false;
    }
    stored.len() == provided.len()
        && bool::from(stored.as_bytes().ct_eq(provided.as_bytes()))
}

/// Consume the nonce (success path): single use.
fn consume_nonce(state: &DbState) {
    let _ = state.kv_delete(KV_NONCE);
    let _ = state.kv_delete(KV_NONCE_EXPIRES);
}

/// The onboarding routes. Mounted OUTSIDE the agent auth middleware: an
/// agent has no gate token at intake time; `/setup/token` is gated by the
/// one-time nonce instead.
pub fn router(state: Arc<GateState>) -> Router<Arc<GateState>> {
    Router::new()
        .route("/", get(index))
        .route("/setup/token", get(token_form).post(token_submit))
        .with_state(state)
}

#[derive(Deserialize, Default)]
struct IndexQuery {
    #[serde(default)]
    token: Option<String>,
}

#[derive(Deserialize, Default)]
struct NonceQuery {
    #[serde(default)]
    nonce: Option<String>,
}

#[derive(Deserialize)]
struct TokenForm {
    #[serde(default)]
    token: String,
}

async fn index(State(gs): State<Arc<GateState>>, Query(q): Query<IndexQuery>) -> Response {
    let Some(deps) = gs.onepage.clone() else {
        return service_unavailable();
    };
    html_page(StatusCode::OK, &instruction_page(&deps, q.token.as_deref()))
}

async fn token_form(
    State(gs): State<Arc<GateState>>,
    Query(q): Query<NonceQuery>,
) -> Response {
    let Some(deps) = gs.onepage.clone() else {
        return service_unavailable();
    };
    let nonce = q.nonce.as_deref().unwrap_or("");
    if !nonce_valid(&gs.state.lock().expect("state lock"), nonce) {
        return gone();
    }
    html_page(StatusCode::OK, &form_page(&deps, &esc(nonce)))
}

async fn token_submit(
    State(gs): State<Arc<GateState>>,
    Query(q): Query<NonceQuery>,
    Form(form): Form<TokenForm>,
) -> Response {
    let Some(deps) = gs.onepage.clone() else {
        return service_unavailable();
    };
    let nonce = q.nonce.as_deref().unwrap_or("");
    if !nonce_valid(&gs.state.lock().expect("state lock"), nonce) {
        return gone();
    }

    // Verify BEFORE anything is stored; a failure consumes nothing.
    let verified = deps
        .platform
        .verify_token(form.token.trim(), &deps.machine_login)
        .await;

    match verified {
        Ok(vt) => {
            // Store BEFORE consuming: a custody failure must leave the
            // nonce alive so the setup can be retried. Store errors go to
            // stderr (never the token) and fail the request — nothing
            // half-stored, nothing consumed.
            let stored = {
                let state = gs.state.lock().expect("state lock");
                MachineTokenStore::new(&state, &deps.enc_key)
                    .store(form.token.trim())
                    .map(|()| consume_nonce(&state))
            };
            match stored {
                Ok(()) => html_page(StatusCode::OK, &success_page(&deps, &vt)),
                Err(e) => {
                    eprintln!("custody store failed — setup incomplete: {e}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "token could not be stored — setup not completed",
                    )
                        .into_response()
                }
            }
        }
        Err(e) => html_page(StatusCode::OK, &failure_page(&deps, nonce, &e)),
    }
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

fn instruction_page(deps: &OnePageDeps, token: Option<&str>) -> String {
    let host = &deps.gate_host;
    let (scheme, host_only) = split_scheme(host);
    let login = esc(&deps.machine_login);
    let (token_param, token_display, token_present) = match token.map(str::trim) {
        Some(t) if !t.is_empty() => (
            t.to_string(),
            esc(t),
            true,
        ),
        _ => (
            "{agent_token}".to_string(),
            "{agent_token}".to_string(),
            false,
        ),
    };
    // Block 1's remote add is plain unless a token was supplied (embed it
    // only when given); block 2's private clone always authenticates, so
    // it shows the minted token or the placeholder.
    let gag_remote = if token_present {
        esc(&format!(
            "{scheme}://agent:{token_param}@{host_only}/git/{{owner}}/{{repo}}.git"
        ))
    } else {
        esc(&format!("{host}/git/{{owner}}/{{repo}}.git"))
    };
    let gag_clone =
        esc(&format!("{scheme}://agent:{token_param}@{host_only}/git/{{owner}}/{{repo}}.git"));
    let gh_host = esc(host_only);

    format!(
        r##"<!doctype html>
<html>
<head><meta charset="utf-8"><title>GitAuthGate — agent setup</title></head>
<body>
<h1>GitAuthGate agent setup</h1>
<p>Copy-paste the blocks in order. Replace <code>{{owner}}</code> and
<code>{{repo}}</code> with the repository you work on.</p>

<h2>1. Clone and remotes — public repo</h2>
<pre><code>git clone https://github.com/{{owner}}/{{repo}}.git
cd {{repo}}
git remote add gag {gag_remote}
git config remote.pushDefault gag</code></pre>

<h2>2. Clone and remotes — private repo</h2>
<p>Private repos cannot be cloned from github.com without a credential —
clone through the gate, then converge:</p>
<pre><code>git clone {gag_clone}
cd {{repo}}
git remote rename origin gag
git remote add origin https://github.com/{{owner}}/{{repo}}.git
git config remote.pushDefault gag</code></pre>

<h2>3. Commit identity</h2>
<pre><code>git config user.name "{login}"
git config user.email {{id}}+{login}@users.noreply.github.com</code></pre>
<p>The gate fills in <code>{{id}}</code> with the machine account's id
after setup completes (GATE-004 §8.4); until then use the placeholder.</p>

<h2>4. API access</h2>
<pre><code>export GH_HOST={gh_host}
export GITHUB_ENTERPRISE_TOKEN={token_display}</code></pre>
</body>
</html>
"##
    )
}

fn form_page(deps: &OnePageDeps, nonce_html: &str) -> String {
    let login = esc(&deps.machine_login);
    format!(
        r#"<!doctype html>
<html>
<head><meta charset="utf-8"><title>GitAuthGate — machine token setup</title></head>
<body>
<h1>Machine-account token setup</h1>
<p>Paste the classic PAT of the machine account <strong>{login}</strong>
(scopes: repo, workflow, read:org). It is verified against GitHub and
stored encrypted; it is never shown again.</p>
<form method="post" action="/setup/token?nonce={nonce_html}">
<input type="password" name="token" placeholder="ghp_..." autofocus>
<button type="submit">Store token</button>
</form>
</body>
</html>
"#
    )
}

fn success_page(deps: &OnePageDeps, vt: &crate::github::VerifiedToken) -> String {
    let login = esc(&vt.login);
    let expiry = match &vt.expires {
        Some(e) => esc(e),
        None => String::from(
            "no expiry — this token never expires (consider setting one when you rotate)",
        ),
    };
    let expected = esc(&deps.machine_login);
    format!(
        r#"<!doctype html>
<html>
<head><meta charset="utf-8"><title>GitAuthGate — token stored</title></head>
<body>
<h1>Machine token stored</h1>
<p>Verified as <strong>{login}</strong> (machine account <strong>{expected}</strong>);
commits will carry this identity.</p>
<p>Token expires: {expiry}</p>
<p>The credential is stored encrypted; see the <a href="/">setup commands</a>
for wiring the agent up.</p>
</body>
</html>
"#
    )
}

fn failure_page(deps: &OnePageDeps, nonce: &str, err: &VerifyError) -> String {
    let (check, explanation) = match err {
        VerifyError::WrongLogin(found) => (
            String::from("Wrong account"),
            format!(
                "This key belongs to <strong>{}</strong> — your main account, not the \
                 machine account. Create a classic PAT (scopes repo, workflow, read:org) \
                 on <strong>{}</strong> and paste that one.",
                esc(found),
                esc(&deps.machine_login)
            ),
        ),
        VerifyError::TwoFactorOff => (
            String::from("Two-factor authentication off"),
            format!(
                "The machine account <strong>{}</strong> does not have two-factor \
                 authentication enabled. Enable 2FA for it on github.com, then retry.",
                esc(&deps.machine_login)
            ),
        ),
        VerifyError::MissingScope(scope) => (
            String::from("Missing scope"),
            format!(
                "The token is missing the <code>{}</code> scope. Edit the classic PAT to \
                 include repo, workflow and read:org, then retry.",
                esc(scope)
            ),
        ),
        VerifyError::Upstream(code) => (
            String::from("GitHub error"),
            format!(
                "GitHub answered HTTP {code} — nothing was stored. Try again in a moment."
            ),
        ),
    };
    format!(
        r#"<!doctype html>
<html>
<head><meta charset="utf-8"><title>GitAuthGate — token rejected</title></head>
<body>
<h1>Token check failed — nothing stored</h1>
<p><strong>{check}:</strong> {explanation}</p>
<form method="post" action="/setup/token?nonce={}">
<input type="password" name="token" placeholder="ghp_..." autofocus>
<button type="submit">Try again</button>
</form>
</body>
</html>
"#,
        esc(nonce),
        check = check,
        explanation = explanation,
    )
}

fn gone() -> Response {
    html_page(
        StatusCode::GONE,
        r#"<!doctype html>
<html>
<head><meta charset="utf-8"><title>GitAuthGate — link expired</title></head>
<body>
<h1>410 Gone</h1>
<p>This setup link is unknown, expired, or already used. Request a new one
from the control plane.</p>
</body>
</html>
"#,
    )
}

fn service_unavailable() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "setup is not wired on this instance",
    )
        .into_response()
}

fn html_page(status: StatusCode, body: &str) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            "text/html; charset=utf-8",
        )],
        body.to_string(),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Split `scheme://host[:port]`; a value without a scheme is treated as
/// https host.
fn split_scheme(url: &str) -> (&str, &str) {
    match url.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => ("https", url),
    }
}

/// Minimal HTML-escaping for interpolated values (config strings, query
/// params, upstream data).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::state::{self, State};

    use super::*;

    #[test]
    fn nonce_ttl_is_fifteen_minutes() {
        assert_eq!(NONCE_TTL_SECS, 900, "GATE-004/011 intake: 15-min expiry");
    }

    #[test]
    fn minted_nonce_validates_then_single_use_after_consume() {
        let s = State::open(":memory:").unwrap();
        let n = mint_setup_nonce(&s).unwrap();
        assert_eq!(n.len(), 64, "32 random bytes, hex");
        assert!(nonce_valid(&s, &n), "freshly minted nonce is valid");
        assert!(
            !nonce_valid(&s, &"0".repeat(64)),
            "a different nonce does not validate"
        );
        consume_nonce(&s);
        assert!(!nonce_valid(&s, &n), "consumed nonce never validates again");
    }

    #[test]
    fn expired_nonce_is_invalid() {
        let s = State::open(":memory:").unwrap();
        let n = mint_setup_nonce(&s).unwrap();
        s.kv_set(KV_NONCE_EXPIRES, &(state::now() - 1).to_string())
            .unwrap();
        assert!(!nonce_valid(&s, &n), "expiry already passed");
    }

    #[test]
    fn unknown_nonce_is_invalid() {
        let s = State::open(":memory:").unwrap();
        assert!(!nonce_valid(&s, &"a".repeat(64)));
    }

    #[test]
    fn scheme_split_handles_both_host_shapes() {
        assert_eq!(split_scheme("https://gag.example.com"), ("https", "gag.example.com"));
        assert_eq!(split_scheme("http://gitauthgate:8080"), ("http", "gitauthgate:8080"));
    }

    #[test]
    fn html_escaping_neutralizes_markup() {
        assert_eq!(esc("<b>&\""), "&lt;b&gt;&amp;&quot;");
    }
}
