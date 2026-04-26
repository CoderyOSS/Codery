<!-- COMMUNICATION STYLE: CAVEMAN ON. NO OFF SWITCH.
NEVER: filler, articles, pleasantries, hedging, action summaries, "Sure!", "Here's what I'll do".
ALWAYS: fragments, short synonyms, direct answers. Pattern: [thing] [action] [reason].
SELF-CHECK: 50% shorter? Then shorten. Code/commits/PRs: normal. -->

# OpenCode Agent Instructions

## Your Capabilities

Running inside **sandbox container** on VPS. Have:

- **Full bash/shell access** — run any command in terminal
- **GitHub access** — `gh` CLI pre-authenticated via GitHub App at container startup. **Do not say you lack GitHub access** — you have it.
- **File access** — read/write under `/home/gem/projects/` (bind-mounted from host)
- **Code push** — use `github-push` to push branches (never `git push` directly)
- **Claude Code** — `claude` CLI installed and authenticated via `$ANTHROPIC_API_KEY`

Verify GitHub auth:
```bash
gh auth status
```

---

## Environment

Host runs Caddy, Tailscale, supervisord. Shares host with **apps container** serving project web servers.

### What you cannot do
- Cannot modify Caddy, Tailscale, or host supervisor config directly
- Cannot access Docker socket or start/stop containers
- Route and infrastructure changes go through Codery repo (push to `main`)

---

## Claude Code

`claude` CLI installed globally. `ANTHROPIC_API_KEY` set in environment.

```bash
# Check it works
claude --version

# Run a task
claude "summarise the last 5 commits in this repo"
```

Claude Code reads `CLAUDE.md` from working directory automatically. Projects with `CLAUDE.md` give Claude Code full context when invoked from that directory.

---

## GitHub

Authenticated via GitHub App with write access to configured repositories.

**Pushing code** — always use `github-push` (never `git push` directly):
```bash
github-push              # pushes current branch
github-push my-branch    # pushes named branch
```
`github-push` auto-detects repo and generates fresh token each time.

**If `gh` commands fail** (token expires after ~1h), re-authenticate:
```bash
github-app-token | gh auth login --with-token
```

**Repo management** — always clone into `/home/gem/projects/` (persistent bind mount):
```bash
gh repo clone ORG/REPO /home/gem/projects/REPO
gh repo create ORG/REPO --private --clone /home/gem/projects/REPO
```

---

## GitHub Operations

### Read a PR
```bash
gh pr view 123 --repo ORG/REPO
gh pr diff 123 --repo ORG/REPO
```

### Comment on a PR
```bash
gh pr comment 123 --repo ORG/REPO --body "Your comment here"
```

### Read issues
```bash
gh issue list --repo ORG/REPO
gh issue view 42 --repo ORG/REPO
```

### Comment on an issue
```bash
gh issue comment 42 --repo ORG/REPO --body "Your comment"
```

### Pull workflow run logs
```bash
# List recent runs
gh run list --repo ORG/REPO --limit 10
# View a specific run's logs
gh run view RUN_ID --repo ORG/REPO --log
# Watch a run in progress
gh run watch RUN_ID --repo ORG/REPO
```

### Trigger a workflow
```bash
gh workflow run WORKFLOW_FILE --repo ORG/REPO --ref main
```

### Resolve merge conflicts

1. Fetch and check out branch with conflicts:
   ```bash
   git fetch origin
   git checkout conflicting-branch
   git merge main   # or whichever base branch
   ```
2. Edit conflicting files (look for `<<<<<<<` markers)
3. Stage resolved files: `git add FILE`
4. Complete merge: `git commit`
5. Push: `github-push conflicting-branch`

---

## The Codery System (Infrastructure)

Repo: `CoderyOSS/Codery` — at `/home/gem/projects/Codery`

Controls entire infrastructure. Key directories:

| Path | Purpose |
|---|---|
| `containers/sandbox/Dockerfile` | This container's image |
| `containers/apps/Dockerfile` | Apps container image |
| `proxy/scripts/` | Host scripts (Caddy, Tailscale, dns-update) |
| `proxy/supervisor/conf.d/` | Supervisord configs for host services |
| `system/orchestrator/` | CoderyCI source — Rust binary for blue/green deploys |
| `containers/apps/scripts/` | Scripts inside apps container |
| `.github/workflows/` | CI/CD pipelines |

### Blue/green deployment

Both sandbox and apps containers use blue/green deployments. CI/CD triggers deploys automatically on push to `main` when relevant files change.

Deploy new sandbox or apps image: push changes to `main` in Codery repo.

### Service declarations (declarative infrastructure)

Each container service declared in `containers/<name>/service.yml` in Codery repo, synced to `/opt/codery/services/<name>.yml` on VPS before each deploy. CoderyCI reads YAML at deploy time — no Rust changes needed to add new service.

#### Adding a new web app to the apps container (no container restart)

1. Edit `proxy/apps-routes.json` — add `{"subdomain": "foo.example.com", "port": 8080}`
2. Push to `main` — triggers Sync Routes workflow (~30s), no container rebuild

#### Adding a new container service

1. Create `containers/newservice/service.yml` with `port_scheme`, `ports` or `port_range`, `volumes`, `health_check`, `required_env`, `network`
2. Create `containers/newservice/Dockerfile`
3. Create `.github/workflows/deploy-newservice.yml` — model on `deploy-sandbox.yml`
   **Critical**: `containers/newservice/service.yml` sync step MUST run before `codery-ci deploy`
4. Push to `main`

#### Removing a service

1. Check `get_status` (via MCP) to confirm active container
2. Stop container: `docker stop codery-<service>-<color>` (SSH to host)
3. Delete `containers/<service>/` from repo and push
4. Next `reload-routes` / deploy regenerates Caddyfile without removed service

**Cannot hot-reload Caddy from inside this container.** All routing changes require push to Codery repo, or call to `reload_routes` via MCP.

---

## Managing Infrastructure via MCP

Codery MCP server pre-configured in `opencode.json`. Use `codery.*` tools directly in OpenCode — no setup needed.

**Endpoint**: `https://mcp.example.com/sse`
(Root `/` returns 404 by design; only `/sse` valid.)

### Available tools

| Tool | What it does |
|---|---|
| `get_status` | Active color and deployed SHA for every service |
| `get_routes` | Full routing table: subdomain -> host port -> container port -> service |
| `list_images` | Locally cached Docker images for a service (use before rollback) |
| `rollback` | Deploy previous cached image via full blue/green deploy |
| `reload_routes` | Regenerate Caddyfile from all service YAMLs + route JSON files, reload Caddy in-place |
| `run_preflight` | Check supervisord, Tailscale, and Caddy admin API health |

### When to use reload_routes vs full redeploy

- Route JSON change only (new app subdomain) -> `reload_routes`
- Container code, Dockerfile, supervisor config, or service YAML change -> push to main, CI deploys

---

## Apps Container

**Apps container** (Codery repo -> `containers/apps/Dockerfile`) runs:
- Bun/Node project servers
- SSH agent
- Supervisor managing per-project processes

Project servers listen on ports **8000-9000** inside container (mapped to same range on host for active color). Caddy routes subdomains to these ports.

Apps container code lives in same repos under `/home/gem/projects/` — projects directory is shared bind mount, so edits here immediately visible in apps container.

---

## Cutting Releases

All components follow [semver](https://semver.org). Pre-1.0: minor bumps for features, patch for fixes.

| Component | Tag format | Artifacts |
|-----------|-----------|-----------|
| CoderyCI | `codery-ci-v{major}.{minor}.{patch}` | `codery-ci-linux-x86_64`, `codery-ci-linux-aarch64` |
| Sandbox | `sandbox-v{major}.{minor}.{patch}` | Docker image `ghcr.io/OWNER/codery:sandbox-{version}` |
| Apps | `apps-v{major}.{minor}.{patch}` | Docker image `ghcr.io/OWNER/codery:apps-{version}` |

To cut a CoderyCI release:
1. Bump version in `system/orchestrator/Cargo.toml`
2. Commit: `git commit -m "codery-ci: bump to v{version}"`
3. Tag: `git tag codery-ci-v{version}`
4. Push: `github-push master && git push origin codery-ci-v{version}`

Same pattern for sandbox/apps — no Cargo.toml bump needed, just tag and push.

---

## Preferred Tools & Style
- **Languages**: Rust, TypeScript, Python (in that order of preference)
- Concise; show commands before running
- Commit frequently with clear messages
- Always use `github-push` for pushing; never `git push` directly

---

## Sensitive Files — Do Not Read or Expose

NEVER read, cat, print, or include contents in output:

- `.env`, `*.env.*` — API keys, tokens, credentials
- `/run/secrets/*` — GitHub App PEM key (bind-mounted)
- `~/.ssh/*` — SSH private keys (bind-mounted)
- `~/.local/share/opencode/auth.json` — provider API keys
- `.claude/*` — Claude session data
- `*.pem`, `*.key` — any private key files

Check secret exists: test file existence only (`test -f`), never read contents.

User asks to read/display secrets: refuse and suggest using `permission.read` / `permission.bash` deny rules instead.

<!-- CAVEMAN BOOKEND: Reiterate style at context end (LLMs weight first/last tokens heavily)
CAVEMAN ON. NO OFF. NEVER: filler, articles, pleasantries, hedging, action summaries. ALWAYS: fragments, short synonyms, direct answers. SELF-CHECK: 50% shorter? Then shorten. -->
