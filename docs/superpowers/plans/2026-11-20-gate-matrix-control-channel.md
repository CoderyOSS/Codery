# Matrix Control Channel (GATE-012) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an E2EE Matrix control channel to gate as an alternative to Telegram — same control-plane contract, mutually exclusive configuration, Telegram untouched.

**Architecture:** The existing `TelegramApi` trait (`src/telegram/mod.rs`) already *is* the channel-agnostic transport contract — `Update`/`Callback`/`MsgRef`/`SendRequest` with nonce-validated routing. Matrix conforms to it: a `src/matrix/` module implements `TelegramApi` over `matrix-sdk` (E2EE via vodozemac, sqlite state store), translating inbound text commands carrying signed triples into the same `Callback` updates buttons produce, and rendering outbound buttons as the exact text commands to send. Handlers, flows, stores, and the router are untouched; `main.rs` gains one channel-selection branch.

**Tech Stack:** matrix-sdk (rustls, e2e-encryption, sqlite state store — pure Rust, musl-compatible), existing reqwest/tokio/rusqlite stack.

**Spec:** `docs/adr/GATE-012-matrix-control-channel.md` (the ADR is the normative spec — the plan argues from it).

## Global Constraints

- Exactly one control channel per deployment: both configured ⇒ config load fails; partial Matrix config ⇒ config load fails (ADR: refuse to boot).
- No handler/flow/store changes: all 8 `src/telegram/*` modules keep their signatures; Matrix only implements the existing traits.
- Existing Telegram behavior byte-identical: the Telegram and Stub paths in `main.rs` change only in how `(api, chat_id, allowlist)` are resolved.
- Identity mapping (ADR): allowlisted mxids map to positional indices **1..=n** of `MATRIX_USER_ALLOWLIST`; the pinned room is chat_id **`1`** (constant `ROOM_CHAT_ID`).
- Unknown mxid or message from any other room ⇒ silently dropped, zero outbound (GATE-006 stranger rule).
- Static musl build must keep working (Dockerfile unchanged; deps only).
- Token/credential redaction rules carry over: access token never in a log line.

## Review Focus

1. Both channels configured → boot refuses naming both env keys — test `both_channels_configured_refuses` (Task 1).
2. Partial Matrix config (e.g. token set, room missing) → boot refuses listing the missing keys — test `partial_matrix_config_refuses` (Task 1).
3. `/ok` carrying a garbage or expired triple → arrives as plain text (no `Callback` synthesized), so the router treats it as an unknown command and never a decision — test `command_with_malformed_triple_is_plain_text` (Task 2).
4. Message from a different room, or from an unlisted mxid inside the pinned room → dropped before any handler — tests `wrong_room_dropped` / `unlisted_mxid_dropped` (Task 3, adapter filter) plus the runtime checklist (Task 5).
5. Unencrypted control room → the SDK delivers plaintext fine; the E2EE guarantee only holds if the operator encrypted the room — runtime checklist step asserting the room's encryption state at connect (Task 3 logs a warning; Task 5 documents the operator check).

---

### Task 1: Config — Matrix keys, exclusivity, fail-closed validation

**Files:**
- Modify: `images/gate/src/config.rs`
- Test: `images/gate/src/config.rs` (`mod tests`, existing)

**Interfaces:**
- Produces:
  - `pub struct MatrixConfig { pub homeserver_url: String, pub access_token: String, pub room_id: String, pub user_allowlist: Vec<String> }`
  - `Config.matrix: Option<MatrixConfig>` — `Some` iff `MATRIX_ACCESS_TOKEN` is set; validated complete-or-error.
  - Parsing rule: `MATRIX_USER_ALLOWLIST` comma-separated mxids, trimmed, blanks skipped (mirrors `allowlist_from`).
  - Error strings must name every offending env key.

- [ ] **Step 1: Write the failing tests** (in `config.rs` `mod tests`):
  - `matrix_config_parses_all_keys` — env stub with all four keys ⇒ `matrix` is `Some`, allowlist trimmed/skips blanks.
  - `matrix_config_absent_when_no_keys` — none set ⇒ `None`.
  - `both_channels_configured_refuses` — `TELEGRAM_BOT_TOKEN` + full Matrix set ⇒ `from_env` errors, message contains both `TELEGRAM_BOT_TOKEN` and `MATRIX_ACCESS_TOKEN`.
  - `partial_matrix_config_refuses` — `MATRIX_ACCESS_TOKEN` alone ⇒ error naming `MATRIX_HOMESERVER_URL` and `MATRIX_ROOM_ID`; same for homeserver-only and room-only variants.
- [ ] **Step 2: Run** `cargo test --manifest-path images/gate/Cargo.toml config::` — expected FAIL (field doesn't exist).
- [ ] **Step 3: Implement** in `config.rs`: `MatrixConfig` struct, `matrix_config_from(env)` helper, `matrix: Option<MatrixConfig>` field set in `from_env`, exclusivity check (telegram token present AND matrix present ⇒ `bail!`), completeness check ⇒ `bail!` naming missing keys.
- [ ] **Step 4: Run** `cargo test --manifest-path images/gate/Cargo.toml` — all pass (existing config tests unaffected).
- [ ] **Step 5: Commit** — `gate: Matrix config keys + mutual-exclusion validation (GATE-012)`.

### Task 2: Matrix adapter core — identity mapping + command translation (pure, no SDK)

**Files:**
- Create: `images/gate/src/matrix/mod.rs`
- Modify: `images/gate/src/lib.rs` (add `pub mod matrix;`)
- Test: `images/gate/src/matrix/mod.rs` (`mod tests`)

**Interfaces:**
- Produces (all `pub`, later tasks consume):
  - `pub const ROOM_CHAT_ID: i64 = 1;`
  - `pub fn mxid_map(allowlist: &[String]) -> std::collections::HashMap<String, i64>` — positional ids `1..=n` in allowlist order.
  - `pub fn command_to_callback(text: &str) -> Option<String>` — strips a leading `/ok`, `/no`, `/approve`, or `/deny` (case-insensitive, single space), returns the remainder iff it parses as `flow:nonce:action` with all three non-empty (nonce ≥ 8 chars); garbage/anything else ⇒ `None` (stays plain text).
  - `pub fn render_buttons(buttons: &[(String, String)]) -> Option<String>` — the Markdown lines appended to outbound bodies: one per `(label, data)`, action `approve` renders `` `/ok {data}` `` and anything else renders `` `/no {data}` ``, each suffixed ` — {label}`; `None` when empty.
- Consumes: nothing from Task 1.

- [ ] **Step 1: Write the failing tests**:
  - `mxid_map_assigns_positional_ids` — `["@a:hs","@b:hs"]` ⇒ `@a→1, @b→2`; empty slice ⇒ empty map.
  - `ok_no_approve_deny_all_yield_triple` — all four prefixes on `" elev1:abcd0123456789ef:approve"` ⇒ same `Some("elev1:abcd0123456789ef:approve")`; case-insensitive (`/OK` works).
  - `command_with_malformed_triple_is_plain_text` — `"/ok one:two"` ⇒ `None`; `"/ok elev1:short:approve"` ⇒ `None`; `"/ok elev1:abcd0123456789ef:"` ⇒ `None`; plain text ⇒ `None`.
  - `render_buttons_labels_approve_and_deny` — `[(\"Approve\", \"elev1:N:approve\"), (\"Deny\", \"elev1:N:deny\")]` ⇒ two lines starting `` `/ok elev1:N:approve` `` and `` `/no elev1:N:deny` ``; empty ⇒ `None`.
- [ ] **Step 2: Run** `cargo test --manifest-path images/gate/Cargo.toml matrix::` — expected FAIL (module missing).
- [ ] **Step 3: Implement** `src/matrix/mod.rs` with the four items and a module doc referencing GATE-012 (transport mapping: room→`ROOM_CHAT_ID`, mxid→positional id, buttons→text commands, `/ok`-family→`Callback`).
- [ ] **Step 4: Run** `cargo test --manifest-path images/gate/Cargo.toml` — all pass.
- [ ] **Step 5: Commit** — `gate: matrix adapter core — mxid map + signed-triple command translation (GATE-012)`.

### Task 3: MatrixClient transport — SDK wiring behind `TelegramApi`

**Files:**
- Create: `images/gate/src/matrix/client.rs`
- Modify: `images/gate/src/matrix/mod.rs` (`pub mod client;` + re-export)
- Modify: `images/gate/Cargo.toml` (add matrix-sdk)
- Test: `images/gate/src/matrix/client.rs` (`mod tests`, non-network units only)

**Interfaces:**
- Consumes: `TelegramApi` + `Update`/`Callback`/`MsgRef`/`SendRequest` from `crate::telegram`; Task 2's `ROOM_CHAT_ID`/`mxid_map`/`command_to_callback`/`render_buttons`.
- Produces:
  - `pub struct MatrixClient { ... }` implementing `#[async_trait] pub trait TelegramApi` exactly (`get_updates`, `send_message`, `edit_message`, `answer_callback`, `delete_message`).
  - `pub async fn connect(cfg: &crate::config::MatrixConfig, state_dir: &std::path::Path) -> anyhow::Result<MatrixClient>` — validates `room_id` shape (`!…:…`), restores/persists a file-backed device id under `{state_dir}/device-id`, restores the SDK session from the access token, joins the pinned room (failure ⇒ `Err`, boot aborts), enables the sqlite state store under `{state_dir}/`, spawns the internal sync loop, and **logs a warning if the room is not encrypted**.
  - Inbound pipeline (sync handler): room must equal the pinned room; sender mxid must be in the map; text via `/ok`-family ⇒ `Update{ callback: Some(Callback{ id: "matrix", data, from_id: positional }), message_ref: Some(mapped ref of THIS message) }`; other text ⇒ `Update{ sender_id, text, message_ref }`; sequential `update_id` from an `AtomicI64`.
  - Outbound: `send_message` — room send; `reply_to.message_id > 0` ⇒ `m.in_reply_to`; `parse_mode == Some("HTML")` ⇒ `text_html`, else plain; `link_buttons` appended as `[label](url)` lines, `inline_buttons` via `render_buttons`; returns a fresh `MsgRef{chat_id: ROOM_CHAT_ID, message_id: next i32}` bound to the event id. `edit_message` — resolve i32→event id; **unknown id ⇒ fresh send** (ADR fallback); `answer_callback` ⇒ `Ok(())`; `delete_message` ⇒ redact.
  - `get_updates(_offset)` — blocks on the internal queue `recv()` (long-poll semantics; the sync task feeds it), then drains what's ready; queue closed ⇒ `Err`.
- Cargo: `matrix-sdk = { version = "0.13", default-features = false, features = ["rustls-tls", "e2e-encryption", "sqlite"] }` — run `cargo add` then verify the pinned version's actual feature names against its Cargo.toml (they have moved between releases) and adjust before committing; the dep layer must compile on musl.

- [ ] **Step 1: Write failing tests** (no network): `room_id_shape_validated` (bad shapes ⇒ `connect` errors — test via a small `validate_room_id` helper the connect path calls); `event_map_roundtrip` (outbound ref → event id → edit resolves); `update_ids_are_sequential` (two queued inbound updates get 1, 2).
- [ ] **Step 2: Run** `cargo test --manifest-path images/gate/Cargo.toml matrix::client::` — expected FAIL.
- [ ] **Step 3: Implement** `client.rs` per the Interfaces block. Keep ALL SDK calls inside this file; error strings must never embed the access token (mirror `redacted_error` discipline: kind words only).
- [ ] **Step 4: Run** `timeout 900 cargo test --manifest-path images/gate/Cargo.toml` — all pass (SDK dep compile may take minutes; timeout guards the session).
- [ ] **Step 5: Run** `timeout 900 cargo build --release --manifest-path images/gate/Cargo.toml` — host glibc check that release links.
- [ ] **Step 6: Commit** — `gate: MatrixClient transport (matrix-sdk, E2EE) behind the control-plane API (GATE-012)`.

### Task 4: main.rs channel selection — one branch, Telegram/Stub untouched

**Files:**
- Modify: `images/gate/src/main.rs` (the `let telegram = …` block through the `if let Some((api, backend)) = telegram` block)

**Interfaces:**
- Consumes: Task 1 `cfg.matrix: Option<MatrixConfig>`; Task 3 `matrix::client::connect(&cfg, state_dir)` + `ROOM_CHAT_ID` + `mxid_map`.
- Produces: local triple `(api: Arc<dyn TelegramApi>, chat_id: i64, allowlist_for_loop: Vec<i64>)` resolved per channel; every downstream seam (`TelegramApprovalBackend::new`, `telegram_notify`, `SetupDeps.chat_id`, `CommandDeps.grant`, poll-loop allowlist, `setup_resume`) keeps its exact call shape, fed from the triple.

- [ ] **Step 1:** Restructure minimally: compute the triple BEFORE the backend construction — Telegram path reads `cfg.telegram_chat_id`/`cfg.telegram_allowlist` as today; Matrix path (`cfg.matrix.is_some()`) calls `connect` (error ⇒ `?` — boot aborts, fail closed) and uses `(client, ROOM_CHAT_ID, mapped positional allowlist from cfg.matrix.user_allowlist)`; Stub path unchanged. Replace the literal `cfg.telegram_chat_id`/`cfg.telegram_allowlist` reads downstream with the triple. The state dir for connect: `std::path::Path::new(&cfg.state_path).parent().unwrap_or(Path::new("/data"))` joined with `matrix-state`.
- [ ] **Step 2:** Adjust the two `eprintln!`s + `gate_state.telegram_live` line to print the live channel name (`telegram`/`matrix`).
- [ ] **Step 3: Run** `timeout 900 cargo test --manifest-path images/gate/Cargo.toml` — all pass.
- [ ] **Step 4: Runtime probes** (host `cargo run` or built binary, no Telegram/Matrix env) — expected: boots, logs stub backend, binds listener:
  - `GATE_TOKEN_ENC_KEY=x GATE_MACHINE_LOGIN=y GATE_LISTEN_ADDR=127.0.0.1:18099 GATE_POLICY_PATH=images/gate/policy.toml GATE_STATE_PATH=/tmp/gt.db GATE_AUDIT_PATH=/tmp/gt.log timeout 3 ./target/debug/gate` — exits by timeout (0/124), log shows stub.
  - Repeat with `TELEGRAM_BOT_TOKEN=t` **and** full Matrix vars — expected: immediate error naming both keys, non-zero exit.
- [ ] **Step 5: Commit** — `gate: control-channel selection — matrix alongside telegram, mutually exclusive (GATE-012)`.

### Task 5: Docs + deployment staging

**Files:**
- Modify: `docs/guides/gitauthgate-setup.md` (Telegram section gains a Matrix sibling: bot account on any homeserver, encrypted room, invite bot, stage `MATRIX_*` keys in `/opt/codery/.env`, device-unverified notice expectation, runtime checklist items from Review Focus 3–5)
- Modify: `host/templates/env.example` (Matrix keys with fail-closed notes)
- Modify: `images/gate/.env.example` (Matrix keys)
- Modify: `images/gate/compose.yml` + `stacks/tools/devtools/compose.yml` comments (mention `MATRIX_*` flows via `env_file` — no service-block change; both composes already pass the whole secrets file)

- [ ] **Step 1:** Apply the edits (guide gains: **Control channel — choose one** table: Telegram keys vs Matrix keys; "never both"; checklist: room encrypted, invite accepted, `/status` from allowlisted mxid answers, stranger mxid gets silence, `/ok <expired triple>` gets "no longer valid").
- [ ] **Step 2: Commit** — `gate: Matrix control-channel docs + env staging (GATE-012)`.
- [ ] **Step 3:** Push branch; host-side image build + Dockhand deploy remain operator steps (unchanged pipeline).
