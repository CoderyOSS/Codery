---
id: GATE-004
title: "Machine account credential custody: one classic PAT, held only by the gate"
domain: gate
rules: false
---

# GATE-004: Machine account credential custody — one classic PAT, held only by the gate

## Context

The agent acts on GitHub as a **machine account** — a dedicated GitHub user
account used only for automation. GitHub's Terms of Service explicitly allow
one free machine account per person; its actions are the creator's
responsibility. One identity covers all the user's repos and orgs; changing
access is a single role change that takes effect immediately.

Rejected alternatives (for the record):

- **GitHub App** (the previous design): permission edits need a manual
  settings change plus approval in every installed account; private Apps
  install only on their owning account, so multi-org coverage needs one App
  per org or a public App. Apps are built for products installed by many
  customers; for one person's agent the friction outweighs the benefits.
- **Fine-grained PAT**: cannot access repos where the account is only a
  collaborator on another user's *personal* account — which rules out the
  user's personal repos, the most common case. Org usage can also require
  org approval.
- **The gate holding the user's main-account token** so it could change
  roles itself: rejected — it would put the user's full GitHub power inside
  the gate; a bug or leak would expose everything. Role changes stay on
  GitHub, done by the user, guided by the gate (GATE-010/GATE-011).

## Decision

- **Credential:** one classic personal access token on the machine account,
  scopes `repo`, `workflow`, `read:org`. Classic tokens work everywhere the
  machine account has access, including collaborator seats on personal
  repos. Custom expiry ~6 months; the gate reminds at 14/3/1 days and runs
  `/rotate`.
- **Custody:** the token lives only in the gate. Encrypt at rest with
  AES-256-GCM; the encryption key comes from the environment
  (`GATE_TOKEN_ENC_KEY` in the stack `.env`) and is never stored with the
  ciphertext, in the database, or in backups of it. Decrypt only in memory,
  only in the request path. Never log the token or any `Authorization`
  header; scrub them from errors and stored GitHub responses.
- **Usage:** `Authorization: Bearer {token}` for REST/GraphQL;
  `Authorization: Basic base64("{machine_login}:{token}")` for git over
  HTTPS.
- **Verification** (intake, startup, daily, and `/rotate`):
  1. `GET /user` — `login` must equal the machine account username (a
     token from the main account is rejected with a pointed explanation);
     `two_factor_authentication` must be `true`; store `id` for commit
     identity.
  2. `X-OAuth-Scopes` must include all three scopes; report missing ones by
     name; warn on extras.
  3. `github-authentication-token-expiration` header stored; missing header
     (no expiry) is accepted with a warning.
- **Lifecycle:** `/rotate` mints a replacement with the old token live,
  verifies, swaps, then instructs deleting the old one on GitHub;
  `/disconnect` stops forwarding, erases the stored token, and instructs
  manual deletion. An expired/revoked token yields `credential_unavailable`
  to agents (GATE-010 error shapes) plus a rotation prompt.
- **Commit identity:** the agent's git environment sets author/committer to
  `{id}+{machine_login}@users.noreply.github.com` so GitHub links commits
  to the machine account profile and the private email never appears.
  Commit signing is out of scope for P0.
- The user keeps the machine account's password and 2FA secrets (password
  manager, recommended in setup); the gate never logs in to the GitHub
  website as the machine account.

## Do's and Don'ts

- Do treat the token as the crown jewel: one cipher, separate key, no logs,
  no echoes in errors.
- Do verify at startup; refuse to forward with an unverified credential.
- Don't widen scopes automatically when GitHub reports a scope gap — tell
  the user what's needed and let `/rotate` carry the decision.
- Don't store the machine account password anywhere; the gate has no
  interactive-login capability.

## Consequences

- Positive: classic PAT works across personal and org repos; blast radius
  is bounded by the machine account's roles; GitHub auto-revokes tokens
  leaked to public repos (gate then prompts `/rotate`).
- Trade-off: `repo` scope is broad, but every request still passes the
  policy engine (GATE-005) and registry (GATE-010); the credential cannot
  act outside the machine account's reach.
- Risk: token stolen from the gate. Mitigations: encryption with a
  separate key, no-log discipline, expiry + rotation, ruleset backstop on
  protected branches, and `/disconnect` for immediate shutdown.

## Compliance and Enforcement

Review-verified: AES-256-GCM at rest, key from env only, zero
token/Authorization occurrences in logs or stored responses (automated
grep in CI can check the audit JSONL). Startup refuses unverified tokens.

## References

- GATE-003 (what agents hold instead), GATE-010 (registry, errors),
  GATE-011 (guided setup incl. intake), GATE-006 (reminders, commands)
