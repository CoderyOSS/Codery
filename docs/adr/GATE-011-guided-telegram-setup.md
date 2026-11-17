---
id: GATE-011
title: "Guided Telegram setup"
domain: gate
rules: false
---

# GATE-011: Guided Telegram setup

## Context

Setup involves a dozen GitHub settings pages (account creation, 2FA,
token creation, collaborator invites). The user should never have to work
out GitHub's UI alone; the gate walks them through every step over
Telegram, verifies what it can via the API, and never takes over the
browser. The walkthrough runs on first start with no token configured,
and re-runs per-step whenever something is missing later (deleted token,
2FA off, `/setup`).

## Decision

**The eight steps:**

0. **Introduction** — what a machine account is, ~10 min estimate,
   `[Start]`.
1. **Email** — plus-address alias of the user's email (suggested, not
   stored after setup); fallback: any address they can receive.
2. **Sign-up** — private/incognito window; username clearly tied to the
   user (e.g. `rob-agent`); password into a password manager; gate
   verifies the username exists via unauthenticated
   `GET /users/{username}` and waits; suggests alternatives if taken.
3. **Profile** — name/bio marking the account as automated, operated by
   the main account.
4. **Private email** — check **Keep my email addresses private** and
   **Block command line pushes that expose my email**.
5. **2FA** — enable, save recovery codes with the password; confirmed
   later via `GET /user` `two_factor_authentication`.
6. **Token** — prefilled link
   `https://github.com/settings/tokens/new?scopes=repo,workflow,read:org&description=GitHub%20agent%20proxy`;
   custom expiry ~6 months; gate promises 14/3/1-day reminders.
7. **Token intake** — preferred: one-time HTTPS form
   `{GATE_PUBLIC_URL}/setup/token?nonce=...` (single-use nonce, 15 min
   expiry, one password field); fallback without a public URL: paste in
   Telegram, message deleted instantly via `deleteMessage` (failure told
   to the user). Then full verification per GATE-004 §6.3 — a token from
   the main account is rejected by name with the fix ("create it signed
   in as **rob-agent** in the private window") and returns to step 6.
8. **Repo access** — user sends `owner/repo` names; each runs the grant
   flow (GATE-010) as an in-place checklist; finish message summarizes
   reachable repos and lists commands, and includes the **ruleset
   backstop** reminder (protected branches with force-push/deletion
   blocked, machine account excluded from bypass — full walkthrough in
   `docs/guides/gitauthgate-setup.md`).

**Rules for every step:** one step per message — what, why, exactly what
to click (GitHub's labels, bold); one link button, prefilled where GitHub
allows; the gate auto-detects completion where the API allows (username
exists, 2FA flag, role change) — the user never types "done" except steps
1–5 which carry a `[Done]` button; every step has `[I'm stuck]` (resends
with more detail + common problems) and `[Cancel setup]`; progress is
durable and resumes at the same step after restart; a step older than 1 h
on resume is resent.

## Do's and Don'ts

- Do keep the machine account's password/2FA entirely on the user's side;
  say so plainly in the walkthrough.
- Do prefer the one-time form whenever `GATE_PUBLIC_URL` is set.
- Don't proceed past step 7 without full token verification.
- Don't store the user's main email beyond the alias suggestion.

## Consequences

- Positive: a first-time user reaches a working agent in one repo using
  only Telegram-guided steps (acceptance test); setup re-runs for any
  missing piece without a rebuild or `.env` edit.
- Trade-off: the walkthrough is stateful code (durable progress, per-step
  reruns) — accepted; it replaces every manual config document.
- Risk: GitHub UI label drift. Mitigation: labels are data (step
  definitions), reviewable and updatable without touching flow logic;
  `[I'm stuck]` catches the gap in the moment.

## Compliance and Enforcement

Review-verified: steps and copy match this ADR; nonce single-use +
expiring; paste-fallback deletion is tested; the acceptance checklist
(lives in the implementation plan: first-run to working agent in one repo
via Telegram only; wrong-account token rejected with a pointed
explanation; role-change detection without user confirmation; "grant
until done then remove"; expiry reminders at 14/3/1 days; zero
token/Authorization strings in logs) is an explicit manual QA gate before
release.

## References

- GATE-004 (verification, custody), GATE-006 (message/security rules),
  GATE-010 (grant machinery in step 8), GATE-009 (`GATE_PUBLIC_URL`)
