---
id: GATE-010
title: "Access registry, granting, and elevation"
domain: gate
rules: false
---

# GATE-010: Access registry, granting, and elevation

## Context

The machine account's power on GitHub is exactly its roles per repo. The
user changes roles on GitHub (the gate never holds the user's own
credentials); the gate's job is to know the current state, tell the user
exactly where to click when more access is needed, verify the change took
effect, and hand agents structured errors they can react to.

## Decision

**Registry** (SQLite, per repo):

```
owner/repo
granted_role     // what GitHub reports: admin|maintain|write|triage|read|none
intended_role    // what the user wants (usually write)
via              // collaborator | org_team | org_member
last_verified_at
```

Actual role comes from `GET /repos/{owner}/{repo}` `permissions` object
(`admin/maintain/push/triage/pull` → highest true = role); `404` = no
access. Re-check every 30 minutes, every 5 seconds during a grant or
elevation flow. Unexpected role drops update the registry and notify the
user as a short notice — never ask them to undo it.

**Granting** (`/grant`, setup step 8, or an offered grant): instructions
differ for personal repos (no role choice; collaborator ≈ Write) and org
repos (role menu; outside collaborator); one link button to the
collaborator settings page. The gate accepts the invitation
automatically: poll `GET /user/repository_invitations` every 5 s,
`PATCH /user/repository_invitations/{id}`; one reminder after 1 h;
invitations expire after 7 days (re-invite). Confirm via `GET
/repos/...` until permissions match, update registry + checklist message;
if the user picked a lower role, say exactly what's wrong and where to
fix it. Mention paid-plan seat usage once per org.

**Grant-offer trigger** (builder check settled): only mutations trigger
it, only when pre-flight finds the machine account has **no access** to
the repo (`GET /repos/{owner}/{repo}` → `404`, which GitHub returns for
both "doesn't exist" and "private, no access" — indistinguishable), and
only once per repo per 24 h (debounced). A mutation on a public repo the
machine account *can* reach is third-party (GATE-005: approval flow,
never a grant offer); reads never trigger offers — unregistered reads
forward upstream and pass through whatever GitHub says, without Telegram
noise.

**Role map:** action type → minimum role (Read for reads; Triage for
label/assign; Write for push/PR-merge/release/workflow files; Maintain for
topics/Pages; Admin for settings/rulesets/collaborators/delete), built
from GitHub's repository-roles documentation. Used for pre-flight before
forwarding; a GitHub rejection contradicting the map is logged for map
correction.

**Elevation:** triggered by pre-flight role gap (preferred), a GitHub 403
role error, or `/elevate`. Collect gaps for ~10 s before asking, present
together. State machine (durable in SQLite):

```
DETECTED → AWAITING_USER_CONSENT → AWAITING_ROLE_CHANGE → VERIFYING → DONE
any → CANCELLED | EXPIRED (24 h no progress)
```

- Consent message: why, needed role, current role, one line on what the
  new role adds; buttons `[Grant it] [Grant until done, then remove] [Not
  now]`.
- Role-change message: exact clicks, link button; `via: org_team` points
  at the team page and warns it affects the whole team.
- VERIFYING: poll every 5 s; wrong role → say what it is vs needed;
  10 min silence → one reminder.
- **Temporary elevation** auto-triggers reduction (back to intended role)
  once the triggering request succeeds.
- Personal repos have no Maintain/Admin for collaborators →
  `role_unavailable` + DIY instructions for the user; suggest moving the
  repo to an org if it happens often.

**Reduction/removal** (`/revoke`, temporary-elevation completion,
`/disconnect`): instructions + verification (`404` = fully removed),
registry updated.

**Agent-facing errors** (fixed JSON shapes):
`access_pending`, `permission_pending` (with `elevation_id`,
`needs_role`), `role_unavailable`, `credential_unavailable` — plus
`GET /proxy/elevations/{id}` for polling. GitHub error mapping: 404 =
access removed (re-check + notify), 403 "must have admin rights" = role
gap → elevation, 403 scope header gap = user decides via `/rotate`, 401 =
`credential_unavailable` + rotation prompt.

## Do's and Don'ts

- Do run the full grant/elevation conversation on Telegram; the agent
  never sees GitHub's settings pages.
- Do keep every flow record durable and resumable after restart; resend a
  step only if it is >1 h old.
- Don't retry an elevated action without confirming the registry moved.
- Don't silently widen anything: every role change originates from the
  user's GitHub click.

## Consequences

- Positive: the agent requests access like a colleague would; the user
  makes every access decision on GitHub with exact instructions; roles
  self-heal (30-min re-checks) without user maintenance.
- Trade-off: an action needing more role **fails immediately** with
  `permission_pending` (+ `elevation_id`) — requests never hang on a role
  change (role changes routinely exceed any reasonable HTTP hold); the
  agent backs off, polls `GET /proxy/elevations/{id}`, and retries after
  DONE. Only approval-holds (GATE-005, bounded by the approval timeout)
  ever wait on a human.
- Risk: role map drift vs GitHub reality. Mitigation: contradictions are
  logged with method/path for map fixes.

## Compliance and Enforcement

Review-verified: state-machine transitions have unit tests incl. restart
resume; polling intervals match this ADR; audit JSONL records each
elevation (trigger, role, duration, outcome) and temporary reductions.

## References

- GATE-004 (why the gate can't change roles itself), GATE-006 (message
  rules, commands), GATE-007 (error surface + elevations endpoint),
  GATE-011 (first-time setup uses the same grant machinery)
