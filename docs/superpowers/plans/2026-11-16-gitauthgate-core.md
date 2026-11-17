# GitAuthGate Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the GitAuthGate proxy core: a Rust HTTPS gate that authenticates agents with bearer tokens, classifies git pushes (pkt-line parsing + compare-API ancestry), enforces a fail-closed policy, and proxies GitHub API traffic with held mutations — working end-to-end with a stub approval backend.

**Architecture:** Single axum binary; git smart-HTTP routes stream bytes through (upload-pack) or parse-and-decide (receive-pack); a `Platform` trait isolates GitHub specifics; an `ApprovalBackend` trait isolates human approval (stub in this plan, Telegram in the control-plane plan). State = SQLite + audit JSONL in a bind mount. ADRs are the spec: `docs/adr/GATE-001…009`.

**Tech Stack:** Rust, axum, tokio, reqwest, rusqlite, serde, toml, aes-gcm, wiremock (tests).

**Spec:** `docs/adr/` (GATE-001..009; 010/011 are the control-plane plan's spec). Plan B: `docs/superpowers/plans/2026-11-16-gitauthgate-control-plane.md`.

## Global Constraints

- Fail closed everywhere: malformed input = hold, compare-API `404`/error = not provably-fast-forward, approval timeout = deny, uncovered policy case = deny (GATE-005).
- One-shot approvals scoped to (repo, class, ref, old-oid, new-oid) for git and (repo, method, path, body-hash) for API (GATE-005).
- The agent token must never resolve against github.com; the machine PAT never appears in logs, audit, or error bodies (GATE-003/004).
- Approval hold timeout default 300 s (GATE-005).
- Third-party repos (no registry entry AND no machine-account org membership): all mutations → Approve regardless of policy file (GATE-005).
- No `github.com`/`api.github.com` literals outside `github.rs` (GATE-008).
- URL scheme: `/git/{owner}/{repo}.git` for git, `/api/v3/*` for API (GATE-002/007).

## Review Focus

1. **GraphQL POSTs are reads** when the body has no `mutation` — a query must pass through; pinned by test in Task 8.
2. **Multi-ref pushes** where one ref is `allow` and another is `approve` — whole push must hold, and forward byte-identical on approval; pinned in Task 7.
3. **Malformed pkt-line bodies** must hold/deny, never forward raw; pinned in Task 2 + Task 7.
4. **Compare-API failure (`500`) during ancestry** must hold for approval, not assume FF; pinned in Task 4 + Task 7.
5. **Empty `Authorization` on the machine-token injection path** (gate starts without a verified token) must yield `credential_unavailable`, not a forward; pinned in Task 8.

---

### Task 1: Crate scaffold, config, policy engine

**Files:**
- Create: `images/gate/Cargo.toml`, `images/gate/src/main.rs`, `images/gate/src/config.rs`, `images/gate/src/types.rs`, `images/gate/src/policy.rs`, `images/gate/Dockerfile`
- Test: `images/gate/src/policy.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `ActionClass { Push, ForcePush, BranchDelete, TagDelete }`; `Decision { Allow, Approve, Deny }` (both in `types.rs`); `PolicyFile` (serde from TOML: `defaults` + `[repos."owner/repo"]` overrides); `PolicyEngine::decide(&self, repo: &str, class: ActionClass, own_sphere: bool) -> Decision` — own_sphere=false + any mutation class ⇒ `Approve` regardless of file; missing/unset ⇒ `Deny`. `Config::from_env()` reads `GATE_PUBLIC_URL`, `GATE_TOKEN_ENC_KEY`, `GATE_LISTEN_ADDR` (default `0.0.0.0:8080`), `GATE_POLICY_PATH` (default `/data/policy.toml`).

- [ ] **Step 1: Write failing tests** — `policy.rs` tests: defaults give `Allow` for `Push`/own-sphere, `Approve` for `ForcePush`/own-sphere; per-repo override `tag-delete = "deny"` wins; `own_sphere=false` forces `Approve` for every mutation class even when file says `allow`; unknown class (exhaustive enum, so compile-time) — instead test `decide` with `repo` missing and `own_sphere=false` ⇒ `Approve`; config parse of a sample TOML with defaults + one override.

- [ ] **Step 2: Run** `cargo test -p gate policy` — expect FAIL (types/impl missing).

- [ ] **Step 3: Implement** types, `PolicyFile` serde, `PolicyEngine` (HashMap lookup → per-class decision; `own_sphere` gate first). `main.rs`: load config, log version, exit. `Dockerfile`: multi-stage rust:1-slim build → distroless-style runtime copy of the binary; no git.

- [ ] **Step 4: Run** `cargo test -p gate policy && docker build images/gate` — expect PASS / build OK.

- [ ] **Step 5: Commit** `feat(gate): scaffold crate, config, policy engine (GATE-005)`

### Task 2: pkt-line parsing — receive-pack ref updates

**Files:**
- Create: `images/gate/src/pktline.rs`
- Test: `images/gate/src/pktline.rs`

**Interfaces:**
- Produces: `RefUpdate { old: Oid, new: Oid, refname: String }`; `parse_receive_pack_updates(body: &[u8]) -> Result<Vec<RefUpdate>, ParseError>`; `is_zero(&Oid) -> bool`. Handles first-line `\0capabilities` suffix, `0000` flush, multi-ref bodies, and the all-zeros oid form.

- [ ] **Step 1: Write failing tests** — hand-built byte fixtures (hex strings in test source): single update `old new refs/heads/main\0report-status`; branch delete (new = `00…0`, 40 hex zeros) → `BranchDelete` via `classify_ref_update(u)`; tag delete (`refs/tags/v1`); multi-ref body → 3 updates in order; missing NUL handling; truncated pkt-line → `ParseError`; empty body → `ParseError`.
- [ ] **Step 2: Run** `cargo test -p gate pktline` — expect FAIL.
- [ ] **Step 3: Implement** `classify_ref_update(&RefUpdate) -> Classification` where `enum Classification { Class(ActionClass), NeedsAncestry }` lives in `types.rs` (zero-new ⇒ Branch/TagDelete by `refs/heads/|refs/tags/` prefix; zero-old ⇒ `Class(Push)`; else `NeedsAncestry`).
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): receive-pack pkt-line parsing and ref classification (GATE-005)`

### Task 3: SQLite state + audit JSONL

**Files:**
- Create: `images/gate/src/state.rs`, `images/gate/src/audit.rs`
- Test: both modules' `#[cfg(test)]`

**Interfaces:**
- Produces: `State` (rusqlite handle wrapper, `State::open(path) -> Result<Self>` with `PRAGMA journal_mode=WAL` + migrations v1): tables `agent_tokens(name PK, hash, created_at, last_used_at)`, `registry(repo PK, granted_role, intended_role, via, last_verified_at)`, `blocked_repos(repo PK, blocked_at)`, `pending(id PK, kind, payload_json, created_at, decided_at NULL, outcome NULL)`, `setup_progress(key PK, value, updated_at)`, `flows(id PK, kind, state, payload_json, updated_at)`, `kv(key PK, value)`. Methods used later: `is_blocked(repo) -> bool`, `set_blocked(repo, bool)`, `registry_get(repo) -> Option<RegistryRow>`, `registry_upsert(...)`. Audit: `Audit::open(path) -> Audit; Audit::record(&self, entry: AuditEntry)` — JSONL, one line per event; `AuditEntry` carries `ts, agent: Option<String>, repo, action, decision, basis, approver: Option<String>`; **`AuditEntry` implements a `redact()` pass and any field serialized containing `ghp_`, `Authorization`, or a 40-hex token shape is replaced with `[REDACTED]`**.
- Consumes: nothing.

- [ ] **Step 1: Failing tests** — open in-memory SQLite (`State::open(":memory:")`), run migrations twice (idempotent), blocked set/get roundtrip, registry upsert/get; audit: write entry containing `"Authorization: Bearer ghp_secret"` in a detail field → read file back → line contains `[REDACTED]`, not `ghp_secret`.
- [ ] **Step 2: Run** `cargo test -p gate state audit` — FAIL.
- [ ] **Step 3: Implement** (rusqlite, `include_str!` migrations).
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): sqlite state, registry, kill-switch, audit jsonl with redaction (GATE-009)`

### Task 4: Platform trait + GitHub adapter (compare API, permissions)

**Files:**
- Create: `images/gate/src/github.rs`
- Test: `images/gate/src/github.rs` (wiremock)

**Interfaces:**
- Produces: `trait Platform` per GATE-008 with `async fn ancestry(&self, owner, repo, old: &Oid, new: &Oid) -> Ancestry`; `async fn repo_permissions(&self, owner, repo) -> Result<RepoAccess, RepoAccessError>` where `RepoAccess { role: Role, private: bool }` and `RepoAccessError::NoAccess` (404) / `Upstream(status)`; `fn git_upstream_url(&self, owner, repo) -> Url`; `fn api_base(&self) -> Url`; `pub struct GitHubPlatform { client: reqwest::Client }` with machine-token injection per-request (`Authorization` set by caller-supplied token getter `Arc<dyn Fn() -> Option<String> + Send + Sync>`). `role_of(&PermissionsObj) -> Role` (highest true of admin/maintain/push/triage/pull; none ⇒ `Role::None`). Compare mapping: `status=="ahead"` ⇒ `Ancestry::FastForward`; `behind`/`diverged` ⇒ `Force`; `404` or non-200 ⇒ `Unknown`. **The only module allowed to contain `api.github.com` / `github.com` literals.**

- [ ] **Step 1: Failing wiremock tests** — mock `/repos/o/r/compare/{old}...{new}` returning `{"status":"ahead"}` ⇒ FastForward; `"behind"` ⇒ Force; `"diverged"` ⇒ Force; 404 ⇒ `Unknown`; 500 ⇒ `Unknown`. Mock `/repos/o/r` returning `{"permissions":{"push":true,"pull":true,...}}` ⇒ `Role::Write`; 404 ⇒ `NoAccess`. `permissions` all false ⇒ `Role::None`.
- [ ] **Step 2: Run** `cargo test -p gate github` — FAIL.
- [ ] **Step 3: Implement** adapter; `Ancestry`/`Role` in `types.rs`.
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): platform trait and github adapter (GATE-004/008)`

### Task 5: Agent token store + auth middleware

**Files:**
- Create: `images/gate/src/tokens.rs`, `images/gate/src/routes.rs`
- Modify: `images/gate/src/main.rs` (wire middleware)
- Test: both modules

**Interfaces:**
- Consumes: `State` (Task 3).
- Produces: `TokenStore` on `State`: `mint(name) -> Result<String>` (32 random bytes base62, returned once; SHA-256 stored), `revoke(name) -> bool`, `validate(raw) -> Option<AgentIdentity{name}>` (timing-safe hash compare; updates `last_used_at`). Middleware `require_agent` in `routes.rs`: extracts Basic (`user:token`) or `Bearer`, validates, injects `AgentIdentity` extension; failure ⇒ GitHub-shaped `401` JSON `{"message":"Bad credentials"}`; ≥5 failures/min per source ⇒ 429 for that source for 60 s.

- [ ] **Step 1: Failing tests** — mint→validate roundtrip; validate rejects wrong token; revoke then validate ⇒ None; token store contains hash (64 hex) not raw; middleware: no auth ⇒ 401 shape; Basic with valid ⇒ extension present; Bearer valid ⇒ ok; 6 rapid bad requests ⇒ 429.
- [ ] **Step 2: Run** — FAIL.
- [ ] **Step 3: Implement**.
- [ ] **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): agent token store and auth middleware (GATE-003)`

### Task 6: Approval backend trait + pending ops

**Files:**
- Create: `images/gate/src/approval.rs`
- Test: `images/gate/src/approval.rs`

**Interfaces:**
- Consumes: `State.pending` table (Task 3), `types::ActionClass`, `RefUpdate`.
- Produces:
```rust
pub enum ApprovalDetail { Push { updates: Vec<RefUpdate>, commits: Vec<String> }, Api { method: String, path: String, body_snippet: String, body_sha: String } }
pub struct ApprovalRequest { pub id: String, pub agent: String, pub repo: String, pub class: ActionClass, pub detail: ApprovalDetail, pub timeout: Duration }
pub enum ApprovalOutcome { Approved, Denied, TimedOut }
#[async_trait] pub trait ApprovalBackend: Send + Sync {
    async fn present(&self, req: &ApprovalRequest) -> anyhow::Result<()>;      // notify human
    async fn wait(&self, id: &str) -> ApprovalOutcome;                          // block until decide/timeout
    async fn resolve(&self, id: &str, outcome: ApprovalOutcome) -> anyhow::Result<()>; // control-plane calls this
}
pub struct StubBackend { pub auto: ApprovalOutcome }  // test double: present=noop, wait=persist pending row, sleep(timeout*0.1 max 1s), return auto
```
- Pending rows persist (restart ⇒ unresolved rows resolve as `TimedOut` at startup — fail closed).

- [ ] **Step 1: Failing tests** — stub approve ⇒ `Approved`; stub deny ⇒ `Denied`; timeout=200ms auto-deny stub ⇒ `TimedOut`; pending row created then resolved with outcome; restart with unresolved row ⇒ resolves TimedOut on init.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): approval backend abstraction with fail-closed pending ops (GATE-005/006)`

### Task 7: Git proxy routes — upload-pack passthrough, receive-pack pipeline

**Files:**
- Create: `images/gate/src/gitproxy.rs`, `images/gate/src/sphere.rs`
- Modify: `main.rs` (mount routes)
- Test: `images/gate/tests/git_proxy.rs` (integration) + module unit tests

**Interfaces:**
- Consumes: Tasks 1–6.
- Produces: routes `GET /git/{owner}/{repo}.git/info/refs` and `POST /git/{owner}/{repo}.git/git-{upload,receive}-pack`. `SphereResolver::is_own_sphere(&self, owner, repo) -> bool`: registry hit ⇒ true; org membership via `Platform::org_member(owner)` (add to trait: `GET /user/memberships/orgs/{org}` `state=="active"` ⇒ true). Receive-pack pipeline: parse pkt-lines → per-ref classify (NeedsAncestry ⇒ adapter) → kill-switch check → `PolicyEngine::decide` → all-`Allow`: stream body byte-identical to upstream with Basic machine auth; any `Approve`: build `ApprovalRequest`, one-shot scope key, hold response until outcome; any `Deny` or parse error: respond git-friendly `403` + pkt-line ERR payload (no upstream call). Upload-pack + info/refs: stream both directions, no parsing.
- Commits list for Push approvals: `GET /repos/{o}/{r}/compare/{old}...{new}` `commits[].sha + commit.message` first line (adapter method `async fn commits_between(...) -> Vec<String>`).

- [ ] **Step 1: Failing unit tests** — sphere: registry hit ⇒ true; membership mock 200 active ⇒ true; 404/404-member ⇒ false. Pipeline decision matrix tests (pure fn `plan_push(updates, ancestry_fn, policy, blocked, sphere) -> Plan` where `Plan::Forward | Plan::Hold(req) | Plan::Deny(reason)`): all-FF own-sphere allow ⇒ Forward; one force + one FF multi-ref ⇒ Hold containing both updates; blocked repo ⇒ Deny; parse error input ⇒ Deny.
- [ ] **Step 2: Failing integration tests** (wiremock upstream serving canned smart-HTTP responses + real `git` client via `std::process::Command`, env `GIT_TERMINAL_PROMPT=0`): (a) clone through gate of a wiremock-served repo ⇒ clone succeeds, upstream received no machine PAT in path (assert auth header was Basic machine:token); (b) push FF to `policy.toml`-allowed repo ⇒ upstream receive-pack hit; (c) push force ⇒ held, StubBackend(auto=Denied) ⇒ push rejected, upstream receive-pack NOT hit; (d) same force with auto=Approved ⇒ upstream hit, body byte-identical (assert upstream saw same bytes gate received); (e) push with unparseable body ⇒ 403, upstream not hit.
- [ ] **Step 3: Run** — FAIL. **Step 4: Implement.** **Step 5: Run** — PASS.
- [ ] **Step 6: Commit** `feat(gate): git proxy with classification pipeline and held force-pushes (GATE-002/005)`

### Task 8: API proxy — /api/v3 with held mutations

**Files:**
- Create: `images/gate/src/apiproxy.rs`, `images/gate/src/errors.rs`
- Modify: `main.rs` (mount), `github.rs` (`user_login()` for credential check)
- Test: `images/gate/tests/api_proxy.rs`

**Interfaces:**
- Consumes: Tasks 3–7 (`ApprovalBackend`, `errors`, sphere, policy — API mutations: own-sphere mutation classes not in git policy file use `Decision::Approve` unless a future `api` section says otherwise; P0: own-sphere API mutations ⇒ per policy `api.mutations` default `approve`).
- Produces: route `ANY /api/v3/*path` (+ `/api/v3/graphql` special case) and `GET /proxy/elevations/{id}` (returns `{"state":"..."}` from `flows` table; Task 10 of Plan B writes rows). `AgentError` JSON shapes (exact, per GATE-010):
```json
{"error":"access_pending","repo":"...","message":"..."}
{"error":"permission_pending","elevation_id":"...","needs_role":"maintain","message":"..."}
{"error":"role_unavailable","message":"..."}
{"error":"credential_unavailable","message":"..."}
```
GitHub error mapping fn `map_upstream_error(status, headers, body) -> Result<(), AgentError>`: 404 ⇒ `access_pending` (registry re-check triggered); 403 + body contains "admin rights" ⇒ role-gap path (returns `permission_pending` with a started elevation if control plane active, else `role_unavailable`); 403 + `X-Accepted-OAuth-Scopes` missing scope ⇒ `role_unavailable` variant naming the scope; 401 ⇒ `credential_unavailable`. Behavior: GET (and GraphQL POST without `"mutation"` in body) ⇒ passthrough + audit; non-GET ⇒ policy `Approve`/`Deny` gate → `ApprovalBackend` hold → forward byte-identical; **no verified machine token at request time ⇒ `credential_unavailable`, nothing forwarded**. Body echo cap: 4 KiB to `body_snippet` (+ `body_sha` of full body).

- [ ] **Step 1: Failing tests** — GraphQL query passthrough (no `mutation` in body ⇒ upstream hit, no hold); GraphQL mutation ⇒ held (stub Approve ⇒ upstream hit; Denied ⇒ 403 JSON); POST create-issue held→forwarded byte-identical; GET passthrough logged in audit; no-machine-token ⇒ `credential_unavailable` shape exact; upstream 404 ⇒ `access_pending`; upstream 403 admin-rights ⇒ `permission_pending` shape with `elevation_id` (stub flow row) ; upstream 401 ⇒ `credential_unavailable`; 5 KiB body ⇒ snippet ≤4 KiB + sha of full body.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): /api/v3 proxy with held mutations and agent error shapes (GATE-007/010)`

### Task 9: Instruction page + one-time token form + custody storage

**Files:**
- Create: `images/gate/src/onepage.rs`, `images/gate/src/custody.rs`
- Modify: `main.rs`, `routes.rs` (form route is nonce-gated, not agent-token-gated)
- Test: modules + `tests/onepage.rs`

**Interfaces:**
- Produces: `GET /` → HTML instruction page (GATE-002 exact copy blocks: public clone path, private clone path with rename/add/pushDefault, commit identity `git config user.name/user.email` with noreply address placeholder, gag-remote + pushDefault commands) — rendered from templates with `GATE_PUBLIC_URL` (or internal alias when absent, per GATE-001 fallback) and an agent token parameter. Custody: `MachineTokenStore::store(plaintext) -> Result` (AES-256-GCM, key from `GATE_TOKEN_ENC_KEY`; ciphertext + nonce in `kv` table), `load() -> Option<String>`, `erase()`. `POST /setup/token?nonce=...`: form render (GET) + submit (POST) — nonce from `setup_progress` (`nonce`, `nonce_expires`), single-use (delete on success), 15-min expiry; on store: run `verify_machine_token` (Task 10 of Plan B completes full checks; core implements the HTTP checks here: `GET /user` login match + `two_factor_authentication` + `X-OAuth-Scopes` + expiry header, via adapter `async fn verify_token(&self, token) -> Result<VerifiedToken, VerifyError>` with `VerifyError::WrongLogin(login) | TwoFactorOff | MissingScope(s) | Upstream(status)`); store only on pass; response page states which check failed.
- Errors page copy per GATE-004: wrong-login message names the login found.

- [ ] **Step 1: Failing tests** — custody roundtrip (store/load/erase; wrong key ⇒ error; ciphertext bytes ≠ plaintext); nonce: GET form with valid nonce ⇒ 200 page; expired nonce ⇒ 410; reused nonce ⇒ 410; verify_token wiremock: login mismatch ⇒ `WrongLogin("robsmith")`; 2FA false ⇒ `TwoFactorOff`; missing `workflow` scope ⇒ `MissingScope("workflow")`; expiry header captured; success stores ciphertext + marks nonce consumed; failed verify does NOT consume nonce (retry allowed) but DOES it never store the token — assert `load()` is None.
- [ ] **Step 2: Run** — FAIL. **Step 3: Implement.** **Step 4: Run** — PASS.
- [ ] **Step 5: Commit** `feat(gate): instruction page, one-time token form, encrypted machine-token custody (GATE-002/004)`

### Task 10: Compose service + image wiring + deployment verification

**Files:**
- Modify: `stacks/tools/devtools/compose.yml` (add `gitauthgate` service, `profiles: [gag]`, per GATE-001: `GATE_IMAGE`, env_file `/opt/codery/.env`, `GATE_PUBLIC_URL`, `GATE_TOKEN_ENC_KEY`, `TELEGRAM_BOT_TOKEN`, `TELEGRAM_ALLOWLIST` placeholders, loopback port `127.0.0.1:23080:8080`, bind mount `/opt/codery/data/gate:/data`, codery-net alias `gitauthgate`), `host/templates/env.example` (document new keys)
- Create: `docs/guides/gitauthgate-setup.md` (deploy: build image, .env keys, Zoraxy route request incl. timeout ≥ 300 s, profile enable, ruleset backstop walkthrough, agent env wiring for paseo per GATE-001 empty-default interpolation)
- Modify: `stacks/tools/devtools/compose.yml` paseo service env: `GH_HOST: ${GATE_AGENT_HOST:-}`, `GITHUB_ENTERPRISE_TOKEN: ${GATE_AGENT_TOKEN:-}`, `GIT_TERMINAL_PROMPT: "0"`

**Interfaces:**
- Consumes: image built per Task 1 Dockerfile.
- Produces: deployable opt-in stack service; documented verification steps.

- [ ] **Step 1:** compose config check: `docker compose -f stacks/tools/devtools/compose.yml config` renders with `COMPOSE_PROFILES=gag` and without (service absent) — both must succeed.
- [ ] **Step 2:** verify empty-default behavior of `gh`: with `GH_HOST=""` exported, `gh auth status` must not attempt the gate (M1 spike deliverable; if it fails, flip to commented env block per GATE-001 and note in the guide).
- [ ] **Step 3:** deploy-time checklist in guide: from a codery-net container, `curl -sI https://gag.<domain>/` reaches the gate (DNS+TLS via Zoraxy); Zoraxy route timeout ≥ 300 s confirmed.
- [ ] **Step 4: Commit** `feat(gate): compose service, agent env wiring, setup guide (GATE-001/009)`
