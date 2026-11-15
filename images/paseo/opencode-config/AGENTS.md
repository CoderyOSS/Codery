# AI Agent Instructions — Paseo container

You are a coding agent inside the Codery (github.com/CoderyOSS/Codery) **paseo
container** on the VPS, spawned by the Paseo daemon (the iPad Paseo app talks
to this daemon; it owns the agent processes it launches). You have MCPs for
container stack operations (`dockhand`), GitHub App operations (`github-app`),
and — if deployed — browser automation (`playwright`; it is an opt-in tool:
check `dockhand_stack_ps` before assuming browser tools exist).

## Your Capabilities

- **Full bash/shell access** — run any command in the terminal
- **File access** — read/write under `/workspace/` (bind-mounted from host
  `/opt/codery/projects` — the shared projects tree)
- **GitHub access** — GitHub App auth via the git credential helper. **Do not
  say you lack GitHub access** — you have it. There is no `gh` CLI in this
  image; for API calls mint a token with `github-app-token` and use `curl`.
- **Code push** — use `github-push` to push branches and tags (never
  `git push` directly)

Verify GitHub auth:
```bash
git ls-remote https://github.com/CoderyOSS/Codery HEAD && echo AUTH-OK
```

---

## Environment

Host runs Zoraxy (routing), Tailscale (the perimeter), supervisord, and
Dockhand (stack management). All HTTP services live in Dockhand stacks
attached to the shared `codery-net` docker network; Zoraxy proxies to them
by container name — HTTP services need no host port publishes.

### What you cannot do
- Cannot modify Zoraxy, Tailscale, or host supervisor config directly
- Cannot access the Docker socket — container control goes through the
  `dockhand.*` MCP tools
- Route changes happen in the Zoraxy UI/API — request them from the user,
  never invent routing

### Container hygiene (hard-won rules)
- **Never run `opencode` as root inside this container.** The image's default
  `docker exec` user is root; a root opencode run creates root-owned state
  under `~/.local/state/opencode/`, and every paseo-user agent then fails to
  load MCPs with EACCES on `locks/` (incident 2026-10-19). Always exec as the
  paseo user: `docker exec -it -u paseo paseo bash`
- **Never `npm install -g` or in-place upgrade opencode** — upgrades happen
  by rebuilding the image, never inside a running container.
- Provider credentials live in `~/.local/share/opencode/auth.json` (host bind
  mount) plus container env (`ANTHROPIC_API_KEY`,
  `ZAI_API_KEY`). The `permission` config denies reading all of these.

---

## The Codery System (Infrastructure)

Repo: `CoderyOSS/Codery` — at `/workspace/Codery`. Three layers, do not
cross them: **core** (dockhand, zoraxy, tailscale — `stacks/core/`, `host/`),
**tools** (paseo, apps, playwright, opendesign, dbgate — `stacks/tools/
devtools/`, `images/`; opt-in via `COMPOSE_PROFILES` in the stack `.env`),
**projects** (your applications — compose in each project repo). Core runs
stock images; tools are Codery-built. The codery-ci machinery is DISABLED (retained
on the host for revert) — do not try to use it.

| Path | Purpose |
|---|---|
| `stacks/tools/devtools/compose.yml` | paseo + apps + opt-in tools stack (canonical mirror; deployed copy in Dockhand) |
| `stacks/core/{dockhand,zoraxy}/compose.yml` | core stack definitions (dockhand deploys host-side) |
| `images/apps/` | apps image (Dockerfile, flake, s6, tooladd, tools-manifest) |
| `images/paseo/opencode-config/` | THIS container's global opencode config (git-managed) |
| `host/` | host layer: supervisor confs, tailscale/dns/backup scripts, bootstrap.sh |
| `RUNBOOK.md` | fresh-VPS bootstrap runbook (agent-facing) |

### Stacks and hosting

Everything HTTP runs in Dockhand stacks on the shared `codery-net` network;
**Zoraxy** proxies subdomains to container names — no host port publishes for
HTTP. Stacks: `devtools` (paseo, apps always on; playwright/opendesign/dbgate
opt-in) plus one stack per project. Project stacks keep their compose in the
project root; core/tool stack dirs live in `/opt/codery/stacks/`. Once the
visibility-mount deploy lands, you can READ stack truth at
`/opt/codery/stacks` and tool data at `/opt/codery/data` (both read-only,
mounts may not exist yet); the `.env` is never visible to you.

Legacy note: in some deployments the apps container still carries
project-specific helper processes — a layer violation (project services belong
in the project stack). Migrate them; see the instance docs on the private
overlay branch if present.

### Dev tools

Install CLI tools at runtime — no image rebuild:
`ssh gem@apps tooladd <nixpkgs-pkg>` (persists across redeploys; manifest
auto-updated in `images/apps/tools-manifest.txt`).

---

## GitHub

Authenticated via GitHub App with write access to configured repositories.

**Pushing code** — always use `github-push`:
```bash
github-push                              # push current branch (repo from origin)
github-push my-branch                    # push named branch
github-push v1.0.0                       # push a tag (same command)
github-push my-branch org/repo           # explicit repo when auto-detect fails
```
If push fails with a permissions/installation error, the token was probably
generated for the wrong GitHub App installation. Do NOT keep retrying — run
`github-app-token --list` to see installations, then push with the explicit
`<org/repo>` argument.

**API calls (no `gh` CLI here)**:
```bash
TOKEN=$(github-app-token org/repo)       # installation token, ~1h expiry
curl -s -H "Authorization: Bearer $TOKEN" https://api.github.com/repos/org/repo/pulls
```

**Cloning** — always clone into `/workspace/` (persistent bind mount).

---

## Sensitive Files — Do Not Read or Expose

NEVER read, cat, print, or include contents in output:

- `.env`, `*.env.*` — API keys, tokens, credentials
- `/run/secrets/*` — GitHub App PEM key (bind-mounted)
- `~/.ssh/*` — SSH private keys
- `~/.local/share/opencode/auth.json` — provider API keys
- `*.pem`, `*.key` — any private key files

Check secret exists: test file existence only (`test -f`), never read contents.

---

## Bun bcrypt pitfall: `Bun.password.verify` silently fails on `$2a$`/`$2y$` hashes

`Bun.password.verify()` (confirmed on Bun 1.2.13, linux/x64) returns `false` — it does
not throw — for every bcrypt hash prefixed `$2a$` or `$2y$`, at any cost factor. Only
`$2b$` hashes verify. Since `Bun.password.hash()` itself emits `$2b$`, Bun-internal
hash→verify roundtrips work and mask the bug; but hashes from Go
(`golang.org/x/crypto/bcrypt`, `caddy hash-password`), PHP `password_hash()`,
`htpasswd -B`, OpenBSD, passlib, etc. are usually `$2a$`/`$2y$` and will ALWAYS fail
verification under Bun — indistinguishably from a wrong password.

Workaround — normalize the prefix before verifying (2a/2b/2y are algorithmically
identical for normal-length passwords):

    const ok = await Bun.password.verify(pw, hash.replace(/^\$2[ay]\$/, "$2b$"));

Never assume "verify returned false ⇒ password wrong" under Bun; check the hash prefix
first, and never write bcrypt test fixtures using only `Bun.password.hash()` output.

---

## Flutter / Dart

Flutter SDK lives at `/workspace/flutter` (persistent projects tree).
`PUB_CACHE=/workspace/.pub-cache` — persistent, never re-download cached packages.

Rules:
- This container is Debian/glibc — dart binaries run fine here. NEVER run
  flutter/dart in the apps container (nixos base; glibc binaries fail with
  "required file not found").
- `pub get` works directly against pub.dev. Do NOT set `PUB_HOSTED_URL`, do
  NOT use a local mirror.
- If dart TLS ever fails: check `ls -la /etc/ssl/certs/ca-certificates.crt`
  first — dart ignores SSL_CERT_FILE env and probes that fixed path.
- Always use a timeout: `timeout 180 flutter pub get` / `timeout 600 flutter
  test`. Hung pub/flutter_tester processes have burned 50-minute sessions.

---

## Style

- **Languages**: Rust, TypeScript, Python (in that order of preference)
- Concise; show commands before running
- Commit frequently with clear messages
- Always use `github-push` for pushing; never `git push` directly
