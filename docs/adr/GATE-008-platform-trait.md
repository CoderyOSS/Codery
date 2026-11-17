---
id: GATE-008
title: "Platform trait; GitHub adapter only in P0"
domain: gate
rules: false
---

# GATE-008: Platform trait — GitHub adapter only in P0

## Context

GitLab and Bitbucket support is a future requirement, but building two
adapters at once triples P0 surface. The platform-specific facts are small
and enumerable: the upstream git remote URL shape, the ancestry/compare
check, and the API base + auth style.

## Decision

Internally, everything platform-specific sits behind one trait:

```
trait Platform {
    fn git_upstream_url(&self, owner: &str, repo: &str) -> Url;   // where pushes/fetches go
    fn is_ancestor(&self, owner: &str, repo: &str, old: &Oid, new: &Oid) -> Ancestry;  // FF? force? unknown?
    fn api_base(&self) -> Url;                                    // where /api/v3 maps to
    fn apply_auth(&self, req: &mut Request, token: &MachineToken);
    fn role_of(&self, permissions: &PermissionsObj) -> Role;      // registry mapping
}
```

P0 ships the **GitHub adapter only** (compare API for ancestry, Basic/Bearer
auth, `permissions`-object role mapping). GitLab (`/api/v4`, its compare
endpoint) and Bitbucket slot in later behind the same trait. External URL
scheme stays `gag.{host}/git/{owner}/{repo}.git` for GitHub in P0; when a
second platform lands, paths dispatch per platform (`/git/{owner}/...` vs
prefixed or per-host variants) — a one-line change in the instruction page
(GATE-002), not a protocol change.

## Do's and Don'ts

- Do route all classification (GATE-005) and registry checks (GATE-010)
  through the trait, never call `api.github.com` from shared code.
- Do give `is_ancestor` an explicit `unknown` state — adapters must map
  API failures to `unknown`, and policy treats `unknown` as hold (fail
  closed).
- Don't stub or half-build GitLab/Bitbucket code paths in P0.
- Don't let platform assumptions (e.g., `diverged` enum strings) leak out
  of the adapter.

## Consequences

- Positive: P0 stays single-platform; multi-platform later is an adapter +
  URL-scheme decision, not a rewrite.
- Trade-off: one layer of indirection around every upstream call — trivial
  cost.
- Risk: trait shaped wrong for a future platform (e.g., Bitbucket Cloud's
  sparse compare support). Mitigation: the `unknown` escape hatch keeps
  policy safe with any adapter.

## Compliance and Enforcement

Review-verified: `github.com`/`api.github.com` literals appear only inside
the GitHub adapter module; policy code consumes `Ancestry`/`Role`, never
raw upstream responses.

## References

- GATE-005 (classification consumes the trait), GATE-010 (registry roles),
  GATE-007 (API base mapping)
