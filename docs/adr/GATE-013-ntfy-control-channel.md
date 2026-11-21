---
id: GATE-013
title: "ntfy is the control channel; Telegram and Matrix transports move to the matrix-telegram branch"
domain: gate
rules: true
---

# GATE-013: ntfy is the control channel; Telegram and Matrix transports move to the matrix-telegram branch

## Context

GATE-012 added Matrix as an alternative to Telegram. Real-world setup
friction (bot account, second client, room ids, encryption toggles,
homeserver quotas) made the channel hard to bring live, and the operator's
actual requirement is narrower than a generic chat transport: gate must
reach ONE operator's phone with instant push, and the operator must answer
from the lock screen. The NTF-AI iOS app (an independent fork of the
official ntfy iOS client, speaking the stock ntfy protocol) delivers
exactly this against a self-hosted ntfy server, with HTTP action buttons
mapped to the control plane's existing signed-triple model.

## Decision

ntfy is the control channel. The Telegram and Matrix transports and their
config keys are REMOVED from main and preserved, fully working, on the
`matrix-telegram` branch (cut at cd2d556) for possible future revival.
The channel-agnostic control plane — the transport trait, update model,
router, poll loop, nonce-validated decisions, approvals/grants/elevation/
setup flows, and the fail-closed posture — is unchanged; only transports
and their config keys churn.

**Backend**: the official `binwiederhier/ntfy` server, self-hosted in the
devtools stack under the `gag` profile:

| Key | Value |
|---|---|
| `NTFY_BASE_URL` | the public HTTPS address the app subscribes to (`https://ntfy.<domain>` — MUST match the app exactly) |
| `NTFY_UPSTREAM_BASE_URL` | `https://ntfy-me.com` — NTF-AI's APNs wake-up relay (see privacy note) |
| `NTFY_AUTH_DEFAULT_ACCESS` | `deny-all` — broker auth IS the operator identity |
| `NTFY_CACHE_FILE` / `NTFY_AUTH_FILE` | persisted under the ntfy data volume |

Reach: a Zoraxy vhost `ntfy.<domain>` → `ntfy:80` (real certificate, no
iOS trust prompts). The app must be able to fetch message bodies when a
push wakes it — including on cellular — so the vhost, not a tailnet-only
bind, is the supported shape. The broker is auth-gated (`deny-all` + one
admin user + an access token for gate).

**Transport (gate side)**: a thin ntfy client implementing the same
transport trait as before:

- `send_message` → `POST {base}/{control-topic}` with `Title`/`Tags`
  headers; approval asks carry ntfy `Actions` buttons —
  `Approve`/`Reject`/… POSTing `<nonce-carrying id>:<action>` to the
  answers topic — so decisions happen from the lock screen.
- `get_updates` → JSON-stream subscription of the control topic (and the
  answers topic): operator compositions and button posts arrive as the
  same validated updates every other transport produced.
- `edit_message` → ntfy has no edit; fresh reply send (the transport has
  no ownership problem — nothing is editable).
- Identity: there is no allowlist. The broker token IS the operator
  identity; holding it implies authority over every topic on the broker.

**Config keys** (replacing `TELEGRAM_*`, `MATRIX_*`, `GATE_TOKEN_ENC_KEY`
stays): `NTFY_URL` (broker base URL, identical to the app's server
address), `NTFY_TOPIC` (control topic), `NTFY_ANSWERS_TOPIC` (button
answers — the phone does NOT subscribe to it), `NTFY_TOKEN`.

**Client**: the free NTF-AI iOS/iPadOS app (or any ntfy client; the
broker serves ntfy's own web UI too).

## Privacy and trust (supersedes the GATE-012 E2EE framing)

With the server INSIDE the operator's infrastructure there is no third
party holding message content: bodies travel device ↔ gate's broker only.
Instant push requires one small third-party hop: ntfy-me.com receives a
SHA-256 hash of the topic address, the message ID, and the server IP
(wake-up ping) — never title, body, tags, or attachments; Apple receives
a poll ping. Encrypted topics (ntfy's draft JWE sender) are DEFERRED —
control-plane content is low-sensitivity and bodies never leave the
self-hosted broker.

## Do's and Don'ts

- Do keep the control plane transport-agnostic: one trait, one update
  model, nonce validation shared by every transport.
- Do keep the `matrix-telegram` branch buildable as the revival path.
- Don't reintroduce allowlists on the ntfy path — broker auth is the
  identity; deny-all + token is the whole access model.
- Don't let the control topic be guessable: it lives behind auth, but the
  answers topic is polled by URL — keep both names long and random.
- Don't configure more than one control channel per deployment (a rule
  that survives, even though only one transport now exists).
- Do grant the ANSWERS topic write-only anonymous access
  (`ntfy access everyone <answers> wo`): lock-screen buttons POST from
  the app with no broker credentials, and the unguessable topic URL is
  the authorization. Never widen the control topic the same way.

## Consequences

- Positive: setup collapses to "run container, install app, paste
  credentials"; decisions from the lock screen; store-and-forward via the
  broker cache; no SDK dependencies (binary and musl build shrink back;
  the aws-lc/cmake/recursion-limit work was matrix-sdk-only and goes).
- Trade-off: the ADR's E2EE framing is dropped in favor of the
  no-third-party-holds-content story; encrypted topics deferred.
- Trade-off: Telegram's and Matrix's always-on-pager delivery is gone
  with them; ntfy's push IS the always-on path (cache covers gaps).
- Risk: NTF-AI is a third-party app by Legitimate LLC; the broker
  protocol is stock ntfy, so any ntfy client can replace it.

## References

- GATE-005 (holds/timeout), GATE-006 (the control-plane contract this
  preserves), GATE-010/011 (flows, walkthrough), GATE-012 (the Matrix
  work — preserved on the `matrix-telegram` branch; its identity-mapping
  and edit-ownership rulings die with the transport)
- ntfy config reference: https://docs.ntfy.sh/config/
- NTF-AI self-hosting: https://ntf-ai.app/docs/self-hosting
