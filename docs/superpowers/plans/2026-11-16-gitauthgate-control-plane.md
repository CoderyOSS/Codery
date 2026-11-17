# GitAuthGate Control Plane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the human control plane to GitAuthGate: Telegram bot (allowlisted, signed buttons) delivering approvals, minting agent tokens, block/unblock kill switch, the 8-step guided machine-account setup with one-time token form, the access registry with grant/elevation/reduction flows, rotation reminders, and background verification loops.

**Architecture:** Same axum process as the core plan; a tokio task long-polls Telegram; all flows are durable `flows`/`setup_progress` rows so restarts resume; an approval backend implements the core plan's `ApprovalBackend` trait by presenting inline keyboards and resolving via callback. Polling loops: 30-min registry re-check, 5-s in-flow polls, daily token verification, expiry reminders 14/3/1 days.

**Tech Stack:** Rust + reqwest Telegram Bot API (long polling), rusqlite (durable flows), trait fakes for Telegram in tests.

**Spec:** `docs/adr/GATE-004/005/006/010/011` + the source spec sections they encode. **Prereq:** `docs/superpowers/plans/2026-11-16-gitauthgate-core.md` (Tasks 1–9 merged; Interfaces below consume its signatures).

## Global Constraints

- Telegram allowlist from `TELEGRAM_ALLOWLIST` (comma-separated user IDs); other senders ignored with no reply (GATE-006).
- State-changing buttons carry (flow record id, random value) validated server-side against live state; acting on a dead/expired flow replies with current state, never a false success (GATE-006).
- No secret ever sent to Telegram except the paste fallback, which is `deleteMessage`d immediately; deletion failure is surfaced (GATE-006/011).
- One step per message; GitHub on-screen labels in bold; auto-detect completion where the API allows; update messages in place; every flow ends with one summary (GATE-006/011).
- Role changes stay on GitHub; the gate instructs, polls, verifies — it never holds user credentials (GATE-004/010).
- Poll intervals: registry re-check 30 min; in-flow polls 5 s; elevation reminder after 10 min silence; expiry reminders 14/3/1 days; elevation expiry 24 h; grant invitation reminder 1 h (GATE-010/011).

## Review Focus

1. **Restart mid-flow** must resume at the same step without duplicate Telegram asks; a step older than 1 h is resent; pinned in Task 5.
2. **A stale Approve button** on an already-timed-out hold must reply with the current state and not resolve anything; pinned in Task 2.
3. **Wrong-role outcome during VERIFYING** (user picked Triage instead of Maintain) must say exactly what differs and re-ask; pinned in Task 8.
4. **Registry role drop** (user removes the agent on GitHub) must self-update + one short notice, never ask the user to undo; pinned in Task 6.
5. **Token verification failure mid-rotation** must leave the old token live and gate forwarding unchanged; pinned in Task 4.

---

### Task 1: Telegram transport — long poll, allowlist, message helpers

**Files:**
- Create: `images/gate/src/telegram/mod.rs`, `images/gate/src/telegram/send.rs`, `images/gate/src/telegram/fake.rs` (test double)
- Modify: `main.rs` (spawn loop when `TELEGRAM_BOT_TOKEN` set), `images/gate/src/routes.rs` (expose health flag)

**Interfaces:**
- Produces: `TelegramApi` trait: `get_updates(offset) -> Vec<Update>`, `send_message(SendRequest) -> MsgRef`, `edit_message(MsgRef, SendRequest)`, `answer_callback(id)`, `delete_message(MsgRef)`; `TelegramClient` (reqwest, long-poll 50 s timeout, exponential backoff on errors) + `FakeTelegram` (test double with recorded outbound). `Update`/`MsgRef`/`SendRequest` (text, `LinkButtons`, `inline keyboard` with `callback_data = "{flow_id}:{nonce}:{action}"`). Poll loop: allowlist filter first (non-allowlisted update dropped silently), then dispatch to a `TelegramRouter` handler (Tasks 2–5 register handlers). `nonce()` helper: 16 random hex chars per flow, checked on callbacks.

- [ ] **Step 1: Failing tests** (against `FakeTelegram`): allowlisted update reaches handler; non-allowlisted dropped with zero outbound sends; callback with wrong nonce ⇒ `answer_callback` + edit to "This action is no longer valid", no handler dispatch; link button renders `callback_data` triple.
- [ ] **Step 2: Run** `cargo test -p gate telegram` — FAIL.
- [ ] **Step 3: Implement** transport + loop (channel to a mpsc router).
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): telegram transport with allowlist and signed callbacks (GATE-006)`

### Task 2: Approval delivery + /pending + /block /unblock

**Files:**
- Create: `images/gate/src/telegram/approvals.rs`, `images/gate/src/telegram/commands.rs`
- Modify: `images/gate/src/approval.rs` (Telegram impl of `ApprovalBackend`)
- Test: `images/gate/tests/telegram_approvals.rs`

**Interfaces:**
- Consumes: core `ApprovalBackend` trait, `ApprovalRequest/Detail`, `State.pending`, `FakeTelegram`.
- Produces: `TelegramApprovalBackend` implementing `present` (message: repo, class, human-readable detail — git: refs + old→new + commit list; API: method, path, snippet — plus `[Approve]`/`[Deny]` buttons bound to `(pending.id, nonce)`), `wait` (watch channel fed by callback resolution; timeout = `TimedOut`), `resolve` (buttons + `resolve()` both route here). Commands: `/pending` lists unresolved rows with resume/cancel buttons; `/block {repo}` / `/unblock {repo}` flip `blocked_repos` with confirmation edit; `/status` renders machine account name (from custody verification), forwarding state, registry rows with roles, open flows. Approving a timed-out/decided hold ⇒ edit replies current state (`"This request already timed out."`), no state change (Review Focus 2).

- [ ] **Step 1: Failing tests** — approval request ⇒ outbound message contains repo + `[Approve]` `[Deny]`; callback approve with correct nonce ⇒ upstream stub (wiremock receive-pack from core Task 7 harness) receives the push; deny ⇒ push rejected; double-approve after timeout ⇒ state reply, no upstream hit; `/block acme/api` then held push on that repo ⇒ denied without Telegram ask; `/pending` lists one row; `/status` lists registry row + forwarding state.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): telegram approvals, /pending, kill switch (GATE-005/006)`

### Task 3: Agent token commands

**Files:**
- Modify: `images/gate/src/telegram/commands.rs`
- Test: `images/gate/tests/telegram_tokens.rs`

**Interfaces:**
- Consumes: `TokenStore` (core Task 5).
- Produces: `/token mint {name}` ⇒ reply contains the raw token **once** with a "store this now" note; `/token revoke {name}` ⇒ confirmation with affected-agent note; `/token list` ⇒ names + last-used, never hashes or tokens.

- [ ] **Step 1: Failing tests** — mint ⇒ reply contains a token that validates; list shows name, not raw; revoke ⇒ validate fails; revoke unknown name ⇒ graceful message.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): token mint/revoke/list over telegram (GATE-003/006)`

### Task 4: Machine-token lifecycle — startup check, daily verify, reminders, /rotate, /disconnect

**Files:**
- Create: `images/gate/src/telegram/lifecycle.rs`, `images/gate/src/tasks.rs`
- Modify: `main.rs` (spawn tasks)
- Test: `images/gate/tests/lifecycle.rs`

**Interfaces:**
- Consumes: core `custody` + `verify_token` (core Task 9), `State.kv` (`token_expires_at`), Telegram transport.
- Produces: startup task: token present ⇒ verify; fail ⇒ refuse forwarding (global flag `forwarding_enabled=false`, core request path checks it ⇒ `credential_unavailable`) + Telegram notice naming the failed check; absent token ⇒ flag off + "run /setup" prompt. Daily verify (06:00 local or 24 h interval) re-runs checks. Expiry reminders at 14/3/1 days before `token_expires_at` (record `reminder_sent:{days}` in `kv` so restarts don't re-send). `/rotate`: run setup steps 6–7 (reuse Task 5 intake) with old token still live; verify new ⇒ atomic swap in custody; then message with `[Open tokens page]` instructing old-token deletion; **verification failure keeps old token and forwarding unchanged** (Review Focus 5). `/disconnect`: set flag off, `custody.erase()`, instruct manual deletion + offer `/revoke` list.

- [ ] **Step 1: Failing tests** — startup with wrong-login token ⇒ forwarding disabled + message names check; daily verify pass leaves state; 14-day reminder fires once (second run same day ⇒ no duplicate); rotate: wiremock verify-new 200 ⇒ swap + forwarding stays on; verify-new 500 ⇒ old token still loads, forwarding on; disconnect ⇒ custody empty + forwarding off.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): machine-token lifecycle tasks and rotate/disconnect (GATE-004/006)`

### Task 5: Guided setup walkthrough — 8 durable steps

**Files:**
- Create: `images/gate/src/telegram/setup.rs`
- Modify: `commands.rs` (`/setup`)
- Test: `images/gate/tests/setup_flow.rs`

**Interfaces:**
- Consumes: Telegram transport (FakeTelegram), custody intake (core Task 9), adapter `GET /users/{name}` + `verify_token`, grant flow hooks (Task 7 of this plan).
- Produces: `SetupFlow` state machine persisted in `setup_progress` (`step`, `username`, `main_email_hint`, `nonce`), steps exactly per GATE-011 §5.2 (0 intro `[Start]`; 1 email plus-address suggestion; 2 signup — text reply with username polled via unauthenticated `GET /users/{name}` until 200, taken-username alternatives `{main}-agent-1`/`{main}-bot`; 3 profile; 4 private email; 5 2FA `[Done]`; 6 prefilled token link; 7 intake via one-time form link (or paste fallback + `delete_message` + failure-to-delete notice) → `verify_token` → wrong-login error names the login and returns to 6; 8 repo checklist → grant flow per repo, in-place ⏳/✅ edits; finish summary + command list + ruleset-backstop reminder linking the guide). `[I'm stuck]` resends with extended detail; `[Cancel setup]` clears progress; resume: load `step`, resend only if `updated_at` older than 1 h.

- [ ] **Step 1: Failing tests** — full happy path through steps 0–7 against fakes + wiremock (`/users/rob-agent` 200 after N 404s; verify-token 200) ⇒ reaches step 8; username wrong until exists ⇒ repeats ask, no advance; intake wrong login ⇒ message names `robsmith`, back at 6, nonce still valid; paste fallback deletes message (fake records `delete_message`); cancel clears rows; **restart**: mid-flow state reload ⇒ same step, no duplicate message when <1 h old, resend when >1 h (Review Focus 1); step 8 checklist updates in place per repo result.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): guided telegram setup walkthrough (GATE-011)`

### Task 6: Access registry + role map + 30-min re-check

**Files:**
- Create: `images/gate/src/registry.rs`, `images/gate/src/rolemapping.rs`
- Modify: `tasks.rs`
- Test: modules

**Interfaces:**
- Consumes: `Platform::repo_permissions`, `State.registry`.
- Produces: `rolemapping.rs`: `min_role(class: ApiActionClass|GitClass) -> Role` table per GATE-010 (read ⇒ Read; label/close/assign ⇒ Triage; push/PR-merge/release/workflow ⇒ Write; topics/Pages ⇒ Maintain; settings/rulesets/collaborators/delete ⇒ Admin) — API classification for P0 = path+method heuristics table (`METHOD /repos/{o}/{r}/...` suffix match), documented in the module. `registry.rs`: `refresh(repo)` — call `repo_permissions`, upsert `granted_role`, compare to previous; role drop ⇒ one Telegram notice ("rob-agent now has None on acme-org/api"), no fix request; background task sweeps all rows every 30 min.

- [ ] **Step 1: Failing tests** — map: create-issue ⇒ Read? No — create ⇒ Triage for label path only; assert exact table entries for the six rows in GATE-010; refresh: wiremock permissions drop Write→None ⇒ registry updated + exactly one notice; refresh unchanged ⇒ no notice; sweep task ticks call refresh for every row.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): access registry, role map, periodic re-check (GATE-010)`

### Task 7: Grant flow — instructions, auto-invite-accept, confirm

**Files:**
- Create: `images/gate/src/telegram/grant.rs`
- Modify: `commands.rs` (`/grant`), `setup.rs` (step 8 delegates here)
- Test: `images/gate/tests/grant_flow.rs`

**Interfaces:**
- Consumes: adapter (`repo_permissions`, `GET /user/repository_invitations`, `PATCH /user/repository_invitations/{id}`, `GET /user/memberships/orgs` for plan note), registry, Telegram.
- Produces: `start_grant(repo, intended_role=Write)`: personal vs org instructions (GATE-010 wording, `[Open collaborator settings]` link), poll invitations every 5 s ⇒ auto-accept, then poll `repo_permissions` until role ⇒ registry + in-place ✅; wrong lower role ⇒ exact fix message; reminder after 1 h, give-up note at 7 d; org with `plan` visible ⇒ one seat note; trigger per GATE-010: mutation + `RepoAccessError::NoAccess` ⇒ offer once per repo per 24 h (`kv` `grant_offer:{repo}:{date}`).

- [ ] **Step 1: Failing tests** — invite appears after 2 polls ⇒ PATCH called with right id ⇒ role confirmed ⇒ ✅ edit + registry row Write; user picks Triage ⇒ fix message names both roles; no invite for 7 d ⇒ give-up; offer debounce: second triggering mutation same day ⇒ no second message.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): repo grant flow with invitation auto-accept (GATE-010)`

### Task 8: Elevation — pre-flight, state machine, temp grants

**Files:**
- Create: `images/gate/src/telegram/elevation.rs`
- Modify: `apiproxy.rs` + `gitproxy.rs` (pre-flight hook after policy approve/allow: `min_role(class)` > `registry role` ⇒ `permission_pending` + start elevation), `errors.rs` (`AgentError::PermissionPending`), add `GET /proxy/elevations/{id}` handler
- Test: `images/gate/tests/elevation_flow.rs`

**Interfaces:**
- Consumes: rolemapping, registry, adapter `repo_permissions`, Telegram, `flows` table.
- Produces: `ElevationFlow` state machine `DETECTED→AWAITING_USER_CONSENT→AWAITING_ROLE_CHANGE→VERIFYING→DONE|CANCELLED|EXPIRED(24h)` durable in `flows`; consent message (why / needs / has / one-line what new role adds / `[Grant it] [Grant until done, then remove] [Not now]`); 10-s collection window batches concurrent gaps into one message; VERIFYING polls 5 s; wrong role ⇒ exact-difference message + re-ask (Review Focus 3); 10-min silence ⇒ one reminder; personal-repo ceiling (needs Maintain/Admin, repo personal) ⇒ `role_unavailable` + DIY instructions, no flow; temporary flag ⇒ after triggering request succeeds, auto-start reduction (Task 9) to `intended_role`; API 403-admin-rights mid-flight maps into an elevation if none exists (errors.rs hook).

- [ ] **Step 1: Failing tests** — held API mutation needing Maintain ⇒ immediate `permission_pending` JSON with `elevation_id`, upstream NOT hit; consent ⇒ role change mock ⇒ DONE + registry Maintain + retried request hits upstream; wrong role Triage ⇒ difference message; `[Not now]` ⇒ CANCELLED; temp grant: request succeeds ⇒ reduction flow auto-started; two concurrent gaps 2 s apart ⇒ one combined message; `/proxy/elevations/{id}` reflects state transitions.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): elevation flows with fail-fast pre-flight and temp grants (GATE-010)`

### Task 9: Reduction, /revoke, /disconnect completion

**Files:**
- Modify: `telegram/grant.rs` (reduction flow), `commands.rs`
- Test: extend `grant_flow.rs`

**Interfaces:**
- Consumes: Task 7/8 machinery.
- Produces: reduction message ("back to Write: click its role and choose Write"), poll-verify, registry update; `/revoke {repo}` ⇒ lower-or-remove menu (role change vs removal via trash icon), verify (`404` ⇒ removed), registry update; `/disconnect` completes with per-repo removal links (GATE-010).

- [ ] **Step 1: Failing tests** — temp-grant reduction: role returns to Write ⇒ registry + ✅; `/revoke` removal path: permissions 404 ⇒ registry row deleted + confirmation; lower path ⇒ role updated.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): access reduction and revoke flows (GATE-010)`

### Task 10: Deployment finalize — .env contract, paseo wiring check, acceptance QA

**Files:**
- Modify: `host/templates/env.example` (final key list), `docs/guides/gitauthgate-setup.md` (acceptance QA checklist appended)
- Test: manual QA per checklist (documented, not CI)

**Interfaces:**
- Consumes: everything above.
- Produces: release-ready deployment.

- [ ] **Step 1:** `.env` keys final review: `GATE_IMAGE, GATE_PUBLIC_URL, GATE_TOKEN_ENC_KEY, TELEGRAM_BOT_TOKEN, TELEGRAM_ALLOWLIST, GATE_AGENT_HOST, GATE_AGENT_TOKEN` (last two only when `gag` profile on).
- [ ] **Step 2:** acceptance QA (from GATE-011 + source spec §13) run against a scratch GitHub account and repo; record results in the guide: first-run setup to working agent in one repo via Telegram only; wrong-account token rejected by name; force-push hold → approve → forwarded; `/block` kill; elevation fail-fast + retry; rotate with no failed requests; `grep -r "ghp_\|Authorization" /opt/codery/data/gate/audit.jsonl` ⇒ zero hits beyond redactions.
- [ ] **Step 3: Commit** `feat(gate): env contract, acceptance QA checklist (GATE-009/011)`
