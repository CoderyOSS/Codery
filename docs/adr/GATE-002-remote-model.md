---
id: GATE-002
title: "Remote model: canonical origin + gag remote, onboarding by instruction page"
domain: gate
rules: false
---

# GATE-002: Remote model — canonical `origin` + `gag` remote, onboarding by instruction page

## Context

The agent's git operations must pass through the gate, but git is
convention-driven and an agent that "gets confused" must not bypass it by
habit. Two shapes were considered:

1. Transparent URL rewriting (`url.insteadOf` mapping `https://github.com/`
   to the gate). Rejected: it hides the mechanism from the user and the
   agent, breaks non-git HTTPS traffic expectations, and is one global
   config away from surprising everyone.
2. Git's native remotes: keep `origin` pointing at the real GitHub URL and
   operate through a second remote. Chosen.

The end state must be identical whether the repo was cloned from GitHub
directly (public repo) or through the gate (private repo): `origin` tracks
GitHub; only `gag` touches the gate.

## Decision

In every agent-worked repo:

```
remote origin → https://github.com/{owner}/{repo}        # canonical, pristine, no credentials
remote gag    → https://gag.{domain}/git/{owner}/{repo}  # gate URL, agent token credential
git config remote.pushDefault gag                        # plain `git push` goes through the gate
```

Enforcement layers:

1. `remote.pushDefault = gag` — the default push destination is the gate.
2. Explicit pushes to `origin` fail on their own: the agent holds no GitHub
   credential (GATE-003/GATE-004), credential prompting is disabled
   (`GIT_TERMINAL_PROMPT=0`, no credential helper for github.com), so
   GitHub answers 401 and nothing happens.
3. The instruction page (`GET /` on the gate, rendered with the live gate
   hostname and a minted-token placeholder) serves the exact copy-paste
   setup for both clone paths. No shipped `gag`/`gag init`/`gag clone`
   scripts in P0 — instructions only.

Clone paths served by the instruction page:

- **Public repo:** clone from GitHub normally, then add the `gag` remote,
  set `remote.pushDefault`, and configure commit identity (GATE-004 §8.4).
- **Private repo:** the agent cannot clone from GitHub at all (no
  credential); it clones through the gate, then converges:

  ```bash
  git clone https://agent:{token}@gag.{domain}/git/{owner}/{repo}.git
  cd repo
  git remote rename origin gag
  git remote add origin https://github.com/{owner}/{repo}.git
  git config remote.pushDefault gag
  ```

When no public URL is configured, agents use the internal codery-net alias
instead of `gag.{domain}` and the same instructions are delivered over
Telegram rather than `GET /` (GATE-001 fallback mode).

Fetches: `git fetch` on `origin` works for public repos without
credentials — read-only and accepted as unaudited (explicit user decision).
Private-repo fetches fail on `origin` and go through `git fetch gag`.
Branch tracking stays on `origin` (explicit user decision).

## Do's and Don'ts

- Do leave `origin` byte-identical to a normal clone; never rewrite its URL.
- Do include commit identity setup in the instruction page; commits are
  built client-side.
- Don't use `url.insteadOf` or any transparent rewrite.
- Don't ship helper scripts in P0; the page is the onboarding.

## Consequences

- Positive: the mechanism is visible and standard git; bypassing requires a
  deliberate act (`git push origin` plus stolen credentials that don't
  exist); both clone paths converge.
- Trade-off: the agent token sits in the `gag` remote URL (visible in
  `git remote -v`) — accepted for P0; tokens are revocable instantly
  (GATE-003) and scoped to gate-only power.
- Risk: an agent pushes explicitly to `origin`. It fails closed — no
  credential exists to make it succeed.

## Compliance and Enforcement

Review-verified: instruction page content matches this ADR; `GIT_TERMINAL_PROMPT=0`
set in the agent environment wiring; no `insteadOf` anywhere in shipped config.

## References

- GATE-003 (agent auth), GATE-004 (commit identity), GATE-007 (API env)
