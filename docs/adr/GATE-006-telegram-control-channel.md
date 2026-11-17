---
id: GATE-006
title: "Telegram is the P0 control channel"
domain: gate
rules: false
---

# GATE-006: Telegram is the P0 control channel

## Context

The user wants human-in-the-loop control with zero web-frontend surface in
P0: see what the agent is asking, approve/deny dangerous operations, mint
and revoke agent tokens, block repos, and be walked through GitHub
changes (GATE-011). Telegram inline keyboards deliver all of this without
building, hosting, or securing a web UI.

## Decision

A Telegram bot (long polling, no webhook URL needed) is the sole control
channel for P0.

**Security rules:**

- Allowlist of Telegram user IDs from config; messages from anyone else
  are ignored silently.
- State-changing buttons carry the flow's record ID plus a random value
  checked server-side, so stale, replayed, or forwarded buttons cannot
  trigger a different flow.
- No secrets are ever sent to Telegram. The single exception is the token
  paste fallback in setup (GATE-011 §5.2 step 7), which is deleted
  immediately via `deleteMessage`; failure to delete is surfaced to the
  user.
- Buttons act on live state: an approval for an already-expired or
  already-decided pending op replies with the current state, not a
  success.

**Message rules:** say what's happening, why, and exactly what to click
(GitHub's on-screen labels, bold, in link buttons); detect completion via
API instead of asking the user to confirm wherever possible; update
progress messages in place; end every flow with one summary.

**Commands:** `/status`, `/grant {owner}/{repo} [role]`, `/elevate
{owner}/{repo} {role}`, `/revoke {owner}/{repo}`, `/pending`, `/token
mint|revoke|list`, `/block|/unblock {owner}/{repo}`, `/rotate`,
`/disconnect`, `/setup`, plus inline Approve/Deny on held operations.

## Do's and Don'ts

- Do keep every flow resumable: durable records, `/pending` to list and
  resume/cancel.
- Do make approval messages show the operation's full shape (method, path,
  body for API; ref, tips, commit list for pushes).
- Don't send the GitHub PAT, agent tokens, or encryption keys to Telegram.
- Don't build a web dashboard in P0 (explicit scope cut; post-P0).

## Consequences

- Positive: approvals reach the user's phone; no web stack to build;
  setup, elevation, and token life all ride one channel.
- Trade-off: single control channel — if Telegram is down, approvals wait
  (and time out as denies, per GATE-005). Accepted: fail-closed is the
  design's posture.
- Risk: Telegram chat compromise. Mitigations: allowlist, signed buttons,
  no secrets policy; the worst leak is visibility into operations, not
  credentials.

## Compliance and Enforcement

Review-verified: allowlist enforced on every update; button callbacks
validated against live records; a CI grep asserts no secret-bearing
strings in outbound Telegram payloads (tokens are hashed/masked at log and
send sites).

## References

- GATE-005 (what approvals look like), GATE-010 (grant/elevation flows),
  GATE-011 (setup walkthrough), GATE-003 (token mint/revoke)
