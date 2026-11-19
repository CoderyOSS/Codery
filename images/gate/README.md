# GitAuthGate (gag)

GitHub auth gate for agent-driven git and API access: a machine account's
PAT is custodied (AES-GCM-encrypted at rest), agents proxy git and API
traffic through gate, and the policy engine decides per action class —
`allow`, `deny`, or `approve` (Telegram hold; fail-closed without a bot).

Single static musl binary; runtime is `scratch` (no shell, no git) running
as uid 65532. State lives in `/data` (`gate.db` + `audit.jsonl`), owned by
the image itself — a fresh named volume auto-seeds with correct ownership
on first boot.

## Setup

1. **Get the image** (DockerHub once published; until then build locally):
   ```bash
   docker pull coderyoss/codery-gitgate:0.1.0
   # today, pre-publish:
   docker build -t codery-gitgate:0.1.0 <repo>/images/gate
   ```
2. **Fill the env** (secrets stay beside the compose; `.env` is gitignored):
   ```bash
   cp .env.example .env
   # GATE_PUBLIC_URL      your reverse-proxy URL, https://gag.example.com
   # GATE_TOKEN_ENC_KEY   openssl rand -base64 32
   # GATE_MACHINE_LOGIN   the GitHub machine account's login
   ```
   Telegram trio is optional — unset means dangerous holds fail closed.
3. **Run it**
   - *Codery host*: Dockhand → import this directory's `compose.yml`.
   - *Plain host*: `docker network create codery-net` (if absent),
     then `docker compose up -d`.
4. **First run**: browse to the gate page through your reverse proxy route
   (Zoraxy on Codery hosts: `gag.<domain>` → `gitauthgate:8080`) and follow
   the rendered instructions — `/setup/token` stores the machine account's
   PAT, encrypted with `GATE_TOKEN_ENC_KEY` into `/data/gate.db`.

From then on, agents use gate as their GitHub origin:
`https://<gate-host>/git/<owner>/<repo>` (git smart HTTP) and
`/api/v3/...` (API proxy). `/whoami` shows the authenticated sphere.

## Configuration

- **Policy** (`policy.toml`, bind-mounted read-only): per-class decisions
  for `push`, `force-push`, `branch-delete`, `tag-delete` plus `[api]`
  mutations; `[repos."owner/repo"]` overrides inherit `[defaults]`.
  Unset = deny. Non-own-sphere mutations always require approval.
- **Cadences**: `GATE_*_SECS` env overrides (see `src/config.rs`); sane
  defaults, no need to set them.

## Operations

- **Upgrade**: bump the tag in the `compose.yml` `image:` line,
  `docker build -t codery-gitgate:<newtag> .`, then redeploy the stack —
  the version bump is a reviewed, in-history change.
- **Rollback**: flip `image:` back to the previous tag, redeploy.
