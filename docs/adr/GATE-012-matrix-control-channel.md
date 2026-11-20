---
id: GATE-012
title: "Matrix is an alternative control channel (E2EE); Telegram and Matrix are mutually exclusive"
domain: gate
rules: true
---

# GATE-012: Matrix is an alternative control channel (E2EE); Telegram and Matrix are mutually exclusive

## Context

GATE-006 chose Telegram as the P0 control channel. Telegram's cloud chats
are client–server encrypted only: the server holds plaintext, and chat
history is visible to the provider. For an approval channel that sees
every dangerous operation the agent attempts, E2EE is materially better.
Matrix rooms with encryption enabled (Megolm) give true end-to-end
encryption between operator and bot, with the homeserver holding only
ciphertext. Telegram support stays; the two channels are interchangeable
implementations of one control-plane transport.

## Decision

Matrix is a supported control channel alongside Telegram. Exactly one is
configured per deployment; both configured ⇒ **refuse to boot** (fail
closed at config load, GATE-009 posture). The existing control-plane
transport API (`TelegramApi` trait, GATE-006-shaped `Update`/`Callback`
model) remains the single abstraction; Matrix conforms to it. Handlers,
flows, stores, and the nonce-validated router are unchanged and
channel-agnostic.

**Transport:**

- `matrix-sdk` with encryption (vodozemac, pure Rust — musl-compatible).
- A dedicated bot account on the operator's chosen homeserver, staged as
  `MATRIX_HOMESERVER_URL` + `MATRIX_ACCESS_TOKEN`; the gate pins one
  encrypted control room via `MATRIX_ROOM_ID`.
- The gate long-polls `/sync` through the SDK; decrypted room messages map
  onto the same `Update` model Telegram uses.
- SQLite state store under `/data/matrix-state/` — device keys and
  Megolm sessions persist across restarts.

**Identity mapping:** Matrix identifiers are strings; the transport's
identifiers are numeric. Each allowlisted mxid maps to its positional
index in `MATRIX_USER_ALLOWLIST` (stable across boots); the configured
room maps to a constant. The mapping is transport-internal and
documented where operators read audit output.

**Interaction rules (mirroring GATE-006):**

- Allowlist by mxid + pinned room: anything else is dropped silently —
  never acknowledged, never replied to.
- No inline buttons. Approval/decision buttons become **text commands
  that embed the same signed callback triple**: the ask renders the
  exact line to send (e.g. `` `/ok elev1:<nonce>:approve` ``); an
  inbound `/ok`/`/no` message carrying a well-formed triple is
  translated into the same validated `Callback` update a Telegram
  button would produce. Nonce validation, replay/stale rejection, and
  "action no longer valid" handling are byte-identical to GATE-006.
- No secrets are ever sent into the room. The GATE-011 token-paste
  fallback behaves as on Telegram — posted into the (encrypted) room and
  deleted immediately after intake; failure to delete is surfaced.
- Progress updates edit in place (`m.room.message` with
  `m.new_content`/`rel_type: m.replace`); message deletion redacts.
- Buttons act on live state → commands act on live state: deciding an
  expired or already-decided op replies with the current state.

**Configuration (GATE-009 env keys):**

| Key | Meaning |
|---|---|
| `MATRIX_HOMESERVER_URL` | bot account's homeserver base URL (required with any other Matrix key) |
| `MATRIX_ACCESS_TOKEN` | bot account access token (required with any other Matrix key) |
| `MATRIX_ROOM_ID` | the one encrypted control room (required with any other Matrix key) |
| `MATRIX_USER_ALLOWLIST` | comma-separated mxids allowed to command the gate; empty ⇒ drop everything (fail closed) |

Partial Matrix configuration (any Matrix key set without the required
ones) is a config error: refuse to boot, never half-wire a channel.

**Onboarding:** the operator creates the room encrypted, invites the bot,
and the gate auto-joins on first sync. The bot's device is unverified by
design (single-device bot, keys never leave `/data`); operator clients
may show an unverified-device notice — expected, documented in the setup
guide.

## Do's and Don'ts

- Do keep every Telegram behavior, ruling, and test shape honored by the
  Matrix transport — one behavioral contract, two transports.
- Do leave `TELEGRAM_*` keys working for deployments that prefer them.
- Don't configure both channels in one deployment — refuse to boot.
- Don't send secrets into the control room (GATE-006 rule carries over;
  the paste fallback, when used, is deleted immediately).
- Don't implement Matrix webhooks/appservices — polling only, same as
  Telegram (no inbound reachability requirement).

## Consequences

- Positive: approvals live in an E2EE room; no provider-side plaintext;
  both channels share one tested control-plane contract.
- Trade-off: `matrix-sdk` grows the static binary and musl build time
  materially (ruma + crypto stack). Accepted.
- Trade-off: event-id ↔ numeric message-ref mapping is in-memory; edits
  targeting pre-restart messages fall back to fresh sends. Ask lifetimes
  (≤ 300 s) make this invisible in practice.
- Risk: bot homeserver availability. Mitigated by the same fail-closed
  timeout posture as Telegram (GATE-005): unreachable channel ⇒ holds
  time out as denies, never as silent forwards.

## Compliance and Enforcement

Review-verified: config loader rejects dual-channel and partial Matrix
configurations; allowlist + room pin enforced on every update; the GATE-006
secret-redaction grep applies to Matrix outbound payloads equally; nonce
validation is shared code with Telegram (same tests drive both paths via
the transport doubles).

## References

- GATE-006 (control channel contract this conforms to), GATE-011 (setup
  walkthrough — channel-agnostic after intake), GATE-005 (holds/timeout
  semantics), GATE-009 (config keys)
- matrix-sdk (Rust, vodozemac E2EE): https://github.com/matrix-org/matrix-rust-sdk
