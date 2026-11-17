---
id: GATE-003
title: "Agent-to-gate authentication: revocable bearer tokens"
domain: gate
rules: false
---

# GATE-003: Agent-to-gate authentication — revocable bearer tokens

## Context

The agent must authenticate to the gate, but the credential it holds must be
worthless against `github.com` directly — otherwise a prompt-injected agent
bypasses the gate entirely. Candidates:

- **SSH keys** (traditional): key in `~/.ssh`, no secret in URLs, but
  requires an SSH server stack (russh), a Telegram-approved pubkey
  enrollment flow (an open "send pubkey" endpoint is an auth bypass), and
  host-key bootstrap friction in non-interactive agent sessions.
- **Bearer token over the gate's existing HTTPS listener**: zero new
  protocol, git's native transport, credential machinery the agent already
  knows.

## Decision

The gate issues **opaque random bearer tokens** to agents:

- Minted and revoked over Telegram: `/token mint <name>`, `/token revoke <name>`
  (GATE-006). Revocation takes effect on the next request — every request
  validates the token.
- Stored **hashed** (SHA-256) in SQLite with a name and created/last-used
  timestamps; the raw token exists only at mint time.
- Transport: the token is embedded in the `gag` remote URL
  (`https://agent:{token}@gag.{domain}/...`) or sent as
  `Authorization: Bearer {token}` for API calls. P0 accepts URL embedding;
  it is gate-only power, instantly revocable.
- Gate responses to a bad token mirror GitHub's `401` shape so git clients
  behave normally.
- Multiple tokens may exist (one per agent instance); each is audited by
  name.

## Do's and Don'ts

- Do store only hashes; the mint reply over Telegram is the single
  plaintext exposure, in the user's own chat.
- Do rate-limit token validation failures to slow brute force.
- Don't reuse GitHub PATs, GitHub App tokens, or any credential that
  resolves against github.com as an agent token.
- Don't build self-service enrollment endpoints in P0.

## Consequences

- Positive: minimal P0 code, no second server stack, instant revocation,
  per-agent attribution in the audit log.
- Trade-off: token visible in remote URLs and mint-chat — accepted; the
  token's power is bounded by the gate's policy engine and holds no GitHub
  authority whatsoever.
- Risk: a leaked agent token lets an attacker push *through the gate* —
  subject to policy (GATE-005), approvals (GATE-006), and the registry
  (GATE-010). Mitigation: revocation is one Telegram command; rulesets
  (backstop, see GATE-005 References) block the worst ops even so.

## Compliance and Enforcement

Review-verified: token store contains hashes only; every mux route except
`GET /`, the one-time setup form (nonce-gated), and health checks requires
a valid token; audit log records the token name, never the token.

## References

- GATE-002 (where the token lives), GATE-006 (mint/revoke commands),
  GATE-004 (the credential the agent must never see)
