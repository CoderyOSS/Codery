---
id: GATE-007
title: "GitHub API proxy surface: /api/v3 with held mutations"
domain: gate
rules: false
---

# GATE-007: GitHub API proxy surface — `/api/v3` with held mutations

## Context

The user wants *every* agent action behind the gate — not just git. The
GitHub API surface is huge, so the gate cannot hand-write policy per
endpoint; it needs a small, uniform rule plus structured visibility. Stock
tools must work unmodified: `gh` and other clients speak GitHub-Enterprise
style `/api/v3` on a configurable host.

## Decision

The gate exposes `https://gag.{domain}/api/v3/*` and translates to
`https://api.github.com/` with the machine token injected.

- **GET (reads):** pass through, log method/path/status in the audit.
  GraphQL *queries* are POSTs — `POST /api/v3/graphql` is inspected: if
  the body contains no `mutation`, treat as a read; if it does, treat as a
  mutation.
- **Non-GET (mutations):** held. Telegram presents method, path, and body
  (JSON pretty-printed; issue/PR bodies are human-readable by design).
  Forwarded verbatim on approval; request fails closed on timeout/denial.
- **Agent wiring:** `GH_HOST=gag.{domain}`,
  `GITHUB_ENTERPRISE_TOKEN={agent token}` for `gh`; base-URL override for
  other clients. Exact `gh` env behavior is confirmed by the M1 spike.
  These vars are set only when the `gag` profile is enabled, so opt-out
  degrades cleanly (GATE-001).
- **Policy interaction:** after policy `allow`/approval (GATE-005), the
  access layer runs role pre-flight (GATE-010) before forwarding.
- **Agent-facing errors** use fixed shapes (defined in GATE-010) so agents
  can react programmatically; GitHub's own permission errors map per the
  error-handling table (404 = access removed, 403 + "admin rights" = role
  gap → elevation, 403 + `X-Accepted-OAuth-Scopes` gap = scope decision
  for the user, 401 = credential dead → `/rotate` prompt).
- `GET /proxy/elevations/{id}` lets an agent poll elevation state.

## Do's and Don'ts

- Do forward the request body byte-for-byte after approval; never modify
  semantics.
- Do cap the size of bodies echoed to Telegram (truncate with a pointer to
  the full body in the audit log).
- Don't parse or whitelist individual endpoints in P0; uniform mutation
  policy now, action families later.
- Don't leak the injected credential into error bodies or audit entries.

## Consequences

- Positive: complete coverage of agent activity with one rule; stock tool
  compatibility; bodies on approval messages give the user exactly the
  "verify the command matches expectations" moment they asked for.
- Trade-off: every mutation waits on a human tap in P0 — deliberately
  annoying until time-window rules land (post-P0, explicit user plan).
- Risk: huge bodies slow the hold path. Mitigation: size cap + audit
  reference; timeouts keep requests from hanging forever.

## Compliance and Enforcement

Review-verified: GET passthrough never runs a mutation test incorrectly
(GraphQL body scan has tests); held requests forward byte-identical on
approval; no Authorization header appears in audit entries.

## References

- GATE-005 (policy layer feeds this), GATE-010 (pre-flight + errors),
  GATE-006 (approval messages), GATE-002 (agent env wiring principle)
