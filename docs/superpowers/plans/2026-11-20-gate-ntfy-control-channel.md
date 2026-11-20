# NTFY Control Channel (GATE-013) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Telegram and Matrix transports with an ntfy client transport (NTF-AI app on the operator side), preserving the channel-agnostic control plane byte-for-byte in behavior; Telegram/Matrix preserved on the `matrix-telegram` branch.

**Architecture:** `src/telegram/` is renamed `src/control/` — it always WAS the channel-agnostic control plane (GATE-012 proved it); only names churn. The TelegramClient transport and the matrix module are deleted from main; a new `src/ntfy/` module implements the transport trait over plain reqwest (publish + JSON-stream subscribe + Actions-header buttons). Config loses `TELEGRAM_*`/`MATRIX_*` and gains `NTFY_URL/NTFY_TOPIC/NTFY_ANSWERS_TOPIC/NTFY_TOKEN`; broker auth (`deny-all` + token) replaces allowlists. matrix-sdk leaves the dependency tree (recursion_limit and the musl cmake/perl work go with it).

**Tech Stack:** reqwest (already a dep, `stream` feature on), tokio, serde_json — zero new dependencies.

**Spec:** `docs/adr/GATE-013-ntfy-control-channel.md` (normative).

## Global Constraints

- Control-plane behavior unchanged: nonce validation, fail-closed posture, router semantics, poll-loop backoff — the suite proves it.
- `main` keeps NO telegram/matrix code paths or config keys; `matrix-telegram` branch (cd2d556) is the revival path and must stay buildable.
- Zero new crate dependencies; matrix-sdk, its `recursion_limit = "512"`, and the Dockerfile cmake/perl (aws-lc-sys only) are REMOVED.
- The poll loop is untouched; ntfy updates synthesize `sender_id: Some(1)` and the loop's allowlist is `[1]` (the token-gated broker is the identity).
- NtfyConfig is all-or-nothing: any `NTFY_*` key set without all four ⇒ refuse boot (GATE-012's fail-closed config posture carries over).

## Review Focus

1. `NTFY_URL` trailing-slash or scheme mismatch vs `NTFY_BASE_URL` the app subscribes to → instant push silently dies (ntfy's #1 self-hosting gotcha). Test: url normalization pinned (Task 5).
2. Answers-topic messages must synthesize callback updates with `text: None, sender_id: Some(1)` (commands.rs fires on text presence) — Task 5 test.
3. Actions header format (`http, Label, URL, body=…` joined by `;`) must be exactly ntfy's — malformed header = buttons missing on lock screen. Test: golden string (Task 4).
4. Stream reconnect: ntfy `/json?since=…` supports `since` cursors — a dropped stream must resume without losing updates (Task 5: cursor advanced per event, reconnect passes last `since`).
5. Config partial-set refusal (any NTFY key without all four) — Task 3 test.

---

### Task 0: Ledger + branch verification

**Files:** `.superpowers/sdd/2026-11-20-gate-ntfy-control-channel/progress.md`

- [ ] Verify `matrix-telegram` exists at cd2d556 on the remote (done in-session; record in ledger).

### Task 1: Rename `src/telegram` → `src/control`; drop `TelegramClient`

**Files:** `git mv images/gate/src/telegram images/gate/src/control`; sed sweep across `images/gate/src/**` + `images/gate/tests/**`:
- `TelegramApi` → `ControlApi`; `TelegramRouter` → `ControlRouter`; `TelegramApprovalBackend` → `ControlApprovalBackend`; `FakeTelegram` → `FakeTransport`; `telegram::` → `control::`; `mod telegram` → `mod control`.
- DELETE `TelegramClient` + its tests (`send_body`, redaction-client tests) from `control/mod.rs` — the Bot-API transport dies on main.
- `kind_of`/`transport_kind`/`redacted_error` move with TelegramClient? NO — `redacted_error` is transport-side; DELETE; the poll loop uses plain `eprintln!` of error kinds via `{e}` — keep loop untouched.
- User-facing copy inside flows (`setup.rs`, `commands.rs`, `lifecycle.rs` help text) mentions "Telegram" — sweep to "your control app"/channel-neutral wording; grep asserts zero `Telegram|telegram` in `src/` afterwards (test: a `#[test]` is overkill — a plan-check grep step).

**Steps:** sweep → `cargo test` green (expect a few compile fixes in doc-refs) → grep gate → commit `gate: control-plane rename — telegram module is the control module (GATE-013)`.

### Task 2: Delete the matrix module and matrix-sdk

**Files:** delete `images/gate/src/matrix/`; `images/gate/src/lib.rs`; `Cargo.toml` (remove matrix-sdk dep block; remove `#![recursion_limit]` from lib.rs); main.rs matrix branch; config.rs matrix fields/tests.

**Steps:** delete + compile (main.rs channel selection shrinks to Telegram-only still) → full suite green → commit `gate: remove matrix transport (preserved on matrix-telegram branch) (GATE-013)`.

### Task 3: Config — NtfyConfig replaces TELEGRAM_*/MATRIX_*

**Files:** `images/gate/src/config.rs` (tests in-file).

**Interfaces (Produces):**
- `pub struct NtfyConfig { pub url: String, pub topic: String, pub answers_topic: String, pub token: String }` — url normalized (trim, trim trailing `/`).
- `Config.ntfy: Option<NtfyConfig>`; helper `ntfy_config_from(url, topic, answers, token) -> Result<Option<NtfyConfig>>` — all-or-nothing (any set ⇒ all required, error names missing); url/topic non-empty trimmed.
- TELEGRAM_*/MATRIX_* keys and helpers deleted (tests too).

**Steps:** RED (new tests: full parse+normalization; partial refusal naming missing keys; absent ⇒ None) → GREEN → commit `gate: ntfy config keys, all-or-nothing (GATE-013)`.

### Task 4: ntfy pure layer — Actions header + command translation

**Files:** create `images/gate/src/ntfy/mod.rs` (+ `pub mod ntfy;` in lib.rs); tests in-file.

**Interfaces (Produces):**
- `pub const CONTROL_TOPIC_ID: i64 = 1;` (MsgRef.chat_id for synthesized updates)
- `pub fn actions_header(buttons: &[(String, String)]) -> Option<String>` — ntfy Actions value: per button `http, {label}, {answers_base_url}, body={data}` joined with `; ` where data is the signed triple. Golden test pins: `Actions: http, Approve, https://ntfy.example.com/answers, body=elev1:abcd0123456789ef:approve; http, Reject, https://ntfy.example.com/answers, body=elev1:abcd0123456789ef:deny`.
- `pub fn command_to_callback(text: &str) -> Option<String>` — moved verbatim from matrix/mod.rs (written-reply path).
- `pub fn update_from_line(state: &SharedState, line: &str) -> Option<Update>` — parse one ntfy `/json` stream event: `{"id","event":"message","message":…}` (skip non-message events; answers-topic and control-topic messages both flow through): `command_to_callback(message)` hits ⇒ `Update{sender_id: Some(1), text: None, callback: Some(Callback{id:"ntfy", data, from_id:1}), message_ref: None}`; misses ⇒ text update `sender_id: Some(1), text: Some(message)`; empty message ⇒ None.
- `pub struct SharedState` — per-process update-id counter (poll-loop bookkeeping), like matrix's minus event bindings (ntfy has no editable events): `next_update_id()`.

**Steps:** move+adapt with RED tests first (golden Actions header; callback/text/None synthesis; id sequencing) → GREEN → commit `gate: ntfy pure layer — Actions header + stream-event translation (GATE-013)`.

### Task 5: NtfyClient transport + main.rs wiring

**Files:** create `images/gate/src/ntfy/client.rs`; modify `images/gate/src/ntfy/mod.rs` (re-export), `src/main.rs`, `Cargo.toml` (confirm `stream` feature on reqwest — already on).

**Interfaces (Produces):**
- `pub async fn connect(cfg: &NtfyConfig) -> Result<NtfyClient>` — health-check `GET {url}/v1/health` (fail ⇒ boot aborts), token auth via Bearer.
- `impl ControlApi for NtfyClient`:
  - `send_message` → `POST {url}/{topic}` JSON body `{"topic","message","title","priority","actions":[…]}` using token auth; returns `MsgRef{chat_id: CONTROL_TOPIC_ID, message_id: 0}`.
  - `get_updates(_offset)` → `GET {url}/{topic}/json?since={cursor}&poll=1`… NO: long-poll semantics — `GET {url}/{answers_topic}/json?since={cursor}&scheduled=1` held open (ntfy streams until `since=all` exhausted; use no timeout on read, rely on reconnect); parse lines via `update_from_line`; cursor = last event id; on stream error ⇒ Err (poll loop backs off, reconnect passes cursor).
  - `edit_message`/`delete_message`/`answer_callback` — edit ⇒ fresh send with `reply_to` ignored (no edits on ntfy); delete/answer ⇒ `Ok(())`.
- The asks side: `TelegramApprovalBackend::new(state, audit, api, Some(CONTROL_TOPIC_ID))` — asks publish to the control topic with Actions headers built by the approval layer? NO — actions are SendRequest-level: extend `send.rs`'s `SendRequest` with an `actions: Option<String>` field set by the approval/ask paths (Task 4's `actions_header`), serialized by the ntfy transport only.
- main.rs: channel branch = ntfy-only (`cfg.ntfy.map(connect)`), triple `(api, Some(CONTROL_TOPIC_ID), vec![1], "ntfy")`; Stub path unchanged; log line `{channel} control plane live`.

**Steps:** RED on pure pieces (jsonl parse incl. garbage lines; actions serialization on SendRequest) → implement client (SDK-free; all HTTP via reqwest) → `cargo test` full suite → runtime probes: (a) no keys ⇒ stub boots; (b) partial NTFY keys ⇒ refuses naming missing; (c) fake-broker probe: `nc`/python http server returning a health 200 + one jsonl line — get_updates returns the synthesized update (script in /tmp, gated on availability) → commit `gate: ntfy transport — publish, stream subscribe, Actions buttons (GATE-013)`.

### Task 6: Compose + docs + version

**Files:** `stacks/tools/devtools/compose.yml` (add `ntfy` service under `gag` profile: image `binwiederhier/ntfy`, env per ADR, volume `codery-ntfy-data:/var/lib/ntfy`, alias `ntfy`, NO host ports — Zoraxy vhost by name; gitauthgate comment: NTFY_* via env_file), `images/gate/compose.yml` + `README.md` (ntfy + NTF-AI walk-through), `host/templates/env.example` + `images/gate/.env.example` (NTFY keys, TELEGRAM/MATRIX removed), `docs/guides/gitauthgate-setup.md` (ntfy sections replace Telegram/Matrix; QA checklist: banner within seconds on locked phone, button answer lands as `data` on answers topic, written `/status` answered, stranger-without-token gets 403), compose image tags `codery-gitgate:0.2.0` + README bump.

**Steps:** edits → `docker compose config`-equivalent eyeball → commit `gate: ntfy deployment + docs (GATE-013)`; push.

### Task 7: Handoff (operator)

- `docker build -t codery-gitgate:0.2.0 images/gate` (musl risk now LOW — no aws-lc; ring only).
- Stage `NTFY_URL/NTFY_TOPIC/NTFY_ANSWERS_TOPIC/NTFY_TOKEN` in `/opt/codery/.env`; create ntfy admin user + token; Dockhand deploy; Zoraxy vhost `ntfy.<domain>` → `ntfy:80`; install NTF-AI, subscribe, QA.
