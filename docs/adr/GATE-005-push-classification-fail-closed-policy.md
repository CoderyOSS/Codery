---
id: GATE-005
title: "Push classification and fail-closed policy"
domain: gate
rules: false
---

# GATE-005: Push classification and fail-closed policy

## Context

The threat model is a prompt-injected agent doing things the user never
asked: `git push --force`, branch deletion, repo-settings changes. The gate
must detect dangerous git operations before anything reaches GitHub, and
every undecided case must fail closed. Local bare mirrors (the git-proxy
approach) give local ancestry checks but cost storage and sync complexity.

## Decision

**Classification** — parse `git-receive-pack` pkt-lines to extract each
ref update `(old-oid, new-oid, refname)`:

- `new-oid` all-zeros → **branch-delete** or **tag-delete** (by ref
  namespace).
- `old-oid` all-zeros → create (treated as ordinary **push**).
- otherwise → **force-push** iff `old-oid` is not an ancestor of `new-oid`,
  checked via `GET /repos/{owner}/{repo}/compare/{old}...{new}`:
  `status: ahead` ⇒ fast-forward; `behind` or `diverged` ⇒ force; `404`
  (old tip unreachable) ⇒ **treat as force**; API failure ⇒ hold for
  approval (never assume safe).

**Policy** — a bind-mounted config file (global defaults + per-repo
overrides) maps each action class — `push`, `force-push`, `branch-delete`,
`tag-delete` — to `allow` | `approve` | `deny`. Defaults: `push: allow`;
`force-push`, `branch-delete`, `tag-delete: approve`. Reads (fetch,
upload-pack) pass through and are logged.

**Own-sphere vs third-party** (builder check settled): a repo is
*own-sphere* iff it has a registry entry (the user granted it, GATE-010)
or the machine account has an active membership in the owning org (only
possible via the user's invitation). Everything else is *third-party*.
Own-sphere repos follow the per-class policy above. **Mutations on
third-party repos always require approval in P0** — expected behavior
(contributing upstream: issues, PRs via fork), and the approval message is
how the user stays informed. Third-party private repos are unreachable
anyway (no access), so the rule is deterministic: registry-or-membership =
per-class policy; else mutations = approve.

**Approval semantics** — `approve` holds the operation; Telegram presents
repo, ref, old→new tips, and the commits being introduced (compare API's
`commits` list); approval is **one-shot for that exact operation** (repo +
ref + SHA pair). The agent's HTTP request waits; timeout (default 300 s) =
deny. A multi-ref push is held if any ref is non-`allow`; on approval the
push forwards exactly as received. Held requests bound the wait at the
approval timeout — role elevations (GATE-010) never extend it; a role gap
fails the request immediately with `permission_pending` and the agent
retries after the elevation completes.

**Kill switch** — Telegram `/block {owner}/{repo}` sets deny-all for that
repo (overrides config); `/unblock` restores. State persists in SQLite.

Anything not covered by config: deny (P0), never guess.

## Do's and Don'ts

- Do hold the full request until the decision; never forward-then-revert.
- Do verify classification inputs: malformed pkt-lines = hold, not pass.
- Don't extend an approval to a different SHA, ref, or repo.
- Don't add session unlocks ("force-push allowed for an hour") in P0 —
  explicit user decision; time-window rules are a post-P0 iteration.
- Don't treat `404`/`5xx` from the compare API as "safe".

## Consequences

- Positive: the two named harmful ops are impossible without a human tap;
  no local mirror infrastructure; classification needs only the compare API.
- Trade-off: an ancestry check per push costs one API call (rate limits:
  5,000/h on the machine token — fine for a single agent).
- Risk: policy file drift from reality. Mitigation: `/status` shows the
  effective policy per repo; config is re-read on change (mtime check).

## Compliance and Enforcement

Review-verified: every `receive-pack` path goes through classification;
tests cover zero-oid detection, compare-API mapping (ahead/behind/diverged/
404/error), timeout-deny, and one-shot scope; audit JSONL records decision,
basis (config rule vs approval vs kill switch), and approver.

## References

- GATE-006 (approval delivery), GATE-010 (role pre-flight after policy
  allows; grant-offer triggers; elevation fail-fast), GATE-001
  (ruleset backstop: protected branches block force-push/deletion
  server-side with the machine account excluded from bypass — deployed
  per `docs/guides/gitauthgate-setup.md` and noted in the post-setup
  Telegram message, GATE-011)
