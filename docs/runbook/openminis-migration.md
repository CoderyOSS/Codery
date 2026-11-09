# Codery Runtime-Ops Migration — OpenMinis Execution Runbook

**Executor:** OpenMinis (on-device ops harness), over SSH to the VPS.
**Spec:** `docs/superpowers/specs/2026-11-07-codery-runtime-ops-design.md`
**Implementation plan (pre-handover work):** `docs/superpowers/plans/2026-11-07-runtime-ops-migration.md`
**Branch:** `migration/runtime-ops` (CoderyOSS/Codery)

You (the executor) are the operations harness. You have full host access via
SSH and you are trusted — but you follow this runbook step-by-step, verify
every gate, and NEVER improvise architecture. If a gate fails, STOP and
apply that phase's fallback. If nothing fits, run the FULL ABORT and report.

---

## 0. Rules that override everything

1. **Never recreate/redeploy the paseo container while any agent session is
   active.** Ask Rob first if unsure. Paseo hosts live coding agents.
2. **Never run work-in-progress container switches** when sessions could be
   active on the target (check `who`, `tmux ls` for apps; the Paseo app's
   session list for paseo). Policy: switching happens only when inactive.
3. Gates are hard. A failed gate = stop → fallback → report. Do not "try
   once more" without a documented reason.
4. Never commit personal-project config into the Codery repo.
   Ownership test: is it under `containers/`? No → it is a project.
5. Secrets: NEVER print, log, or commit secret values. `test -f` / `grep -c`
   for existence checks only.

### 0.1 Known codery-ci defects (observed 2026-11-08/09, W2/W3 execution)

These bit us during the migration; they are orchestrator fix-items (phase 4)
and manual workarounds until then:

| Defect | Symptom | Workaround |
|---|---|---|
| Docker client request timeout too short | `create_container`/`start` on the apps container (1001 port bindings) → `Timeout error` while the daemon actually finishes | Check `docker ps -a`: if the container exists `Created`/running, complete manually (see §5 phase 3 note) |
| cutover `stop` timeout | `Error: failed to stop container codery-apps-<old>` — state + Caddy already flipped | Traffic is fine; just `docker rm -f codery-apps-<old>` afterwards |
| Deploy-time volume lifecycle | A declared named volume was deleted+recreated empty across a deploy window (`Creating missing named volume:` in validate = real signal!) | Runtime tools lost → re-run `tooladd <pkg>` per `tools-manifest.txt`. Orchestrator fix: prune must never touch declared volumes |
| `runtime-apps` oneshot never wired | Runtime apps (ideas/qdrant/graph/bolt/label/booksearch) 502 after any apps container boot | FIXED in image `runtime-apps-boot`+ (contents.d link). Older images: `docker exec codery-apps-<color> /usr/local/bin/runtime-apps-restore` |
| SSH host keys rotated per deploy | All clients' known_hosts break on every apps swap | FIXED in image `runtime-apps-boot`+ (`/etc/ssh` volume). One final rotation on first boot of that image; stable afterwards |
| Tooladd word-split + sudo | `nix profile install` failed (`'flakes' is not a recognised command`, EACCES) | FIXED in image `runtime-apps-boot`+ |

---

## 1. Where to find everything (paths you will need)

### On the VPS (host)

| Path / thing | What |
|---|---|
| `/opt/codery/.env` | ALL host secrets + config (DO NOT print; append-only edits) |
| `/opt/codery/state/<svc>.color` / `<svc>.sha` | codery-ci active color + deployed sha per service |
| `/opt/codery/services/<name>.yml` | live service declarations (synced from repo before deploys) |
| `/opt/codery/codery.db` | codery-ci SQLite (runtime apps: qdrant/graph/bolt-proxy routes) |
| `/opt/codery/projects/` | shared projects tree (= `/workspace` in paseo/apps; contains `Codery/` repo checkout + REMI + …) |
| `/opt/codery/ssh/authorized_keys` | tailnet SSH keys (append-only) |
| `/opt/codery/ssh/agent-to-apps` | paseo→apps client key |
| `/opt/codery/data/opencode` | OpenCode state (agents' auth.json) |
| `/opt/codery/apps-s6.d` | rendered s6 bundles for runtime apps |
| `/opt/codery/stacks/` | Dockhand local stack dir (core services' compose copies; created in phase 0) |
| `/opt/codery/bin/` | host helper scripts (`codery-triage`, `codery-backup.sh`, `nix-store-sync.sh`) |
| `/etc/caddy/Caddyfile` | live Caddy config (written by codery-ci `reload-routes`) |
| `/run/tailscale.ip` | host tailnet IP |
| `supervisorctl status` | host services: caddy, tailscale, codery-ci, codery-ci-mcp |

### In the Codery repo (`/opt/codery/projects/Codery`, branch `migration/runtime-ops`)

| Path | What |
|---|---|
| `containers/<name>/service.yml` | service declarations (source of truth until each service migrates) |
| `containers/dockhand/service.yml` | Dockhand container declaration |
| `containers/paseo/compose.yml` · `containers/opendesign/compose.yml` · `containers/playwright/compose.yml` | STAGED Dockhand stack definitions (used in phases 1–2) |
| `containers/apps/scripts/tooladd` | dev-tool installer (runs inside apps) |
| `containers/apps/tools-manifest.txt` | installed-tools record (auto-updated by tooladd) |
| `containers/paseo/opencode-config/opencode.json` | agent MCP + permission config |
| `proxy/routes.yaml` | host routes (the flip lever) |
| `proxy/scripts/codery-backup.sh` | in-band full backup (phase 0) |
| `proxy/scripts/nix-store-sync.sh` | run before ANY apps image deploy once the /nix volume exists |

### Docker containers (target end-state names)

`paseo`, `apps`, `dockhand`, `opendesign`, `playwright`, project stacks by
compose project name (`remi_backend` for REMI's services).

### Network addresses

| Thing | Address |
|---|---|
| Host tailnet IP | `$(cat /run/tailscale.ip)` |
| Dockhand UI/API (OpenMinis, from iPad) | `https://dockhand.rancidgrandmas.online` (tailnet-only; Caddy basic auth + Dockhand auth) |
| Dockhand UI/API (paseo agent MCP, in-container) | `http://dockhand:3000` (codery-net alias) |
| Paseo daemon | `https://code.rancidgrandmas.online` (WS) |
| Apps SSH / mosh | tailnet `:3333` / UDP `:61000` (unchanged by this migration until phase 3) |
| Paseo SSH | tailnet `:2222` (retargeted in phase 1 — verify!) |

### Secrets inventory (existence checks only)

| Secret | Lives at | Used by |
|---|---|---|
| `DOCKHAND_BASIC_AUTH` | `/opt/codery/.env` (`"<user> <bcrypt-hash>"`) | Caddy gate on dockhand. |
| `DOCKHAND_API_TOKEN` | `/opt/codery/.env` (`dh_...`) → paseo env | stack MCP in paseo + OpenMinis curl |
| Dockhand admin login | Rob's password manager | Dockhand UI |
| `PASEO_PASSWORD`, `GITHUB_APP_ID`, `GITHUB_APP_SLUG`, `ANTHROPIC_API_KEY`, `ZAI_API_KEY` | `/opt/codery/.env` | paseo (unchanged) |
| GitHub App PEM | `/opt/codery/github-app.pem` | code pushes (unchanged) |
| OpenMinis local secrets | `~/.codery/secrets.env` in the Minis sandbox | OpenMinis only |

**OpenMinis `~/.codery/secrets.env` (create before phase 0; populate from
LastPass):**

```
CODEMY_HOST=root@<tailscale-ip>
DOCKHAND_URL=https://dockhand.rancidgrandmas.online
DOCKHAND_BASIC_AUTH=<user>:<password>
DOCKHAND_API_TOKEN=dh_...
GITHUB_TOKEN=<fine-grained token only if you push repo changes>
SSH_KEY_PATH=~/.ssh/id_ed25519
```

---

## 2. Phase 0 — Inventory + pre-flight (read-only except phase-0 steps)

### 2.1 Baseline inventory (record output; refuse to proceed if wrong)

```bash
ssh $CODEMY_HOST /usr/local/bin/codery-triage --json      # expect verdict "ok"
ssh $CODEMY_HOST docker ps --format '{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}'
ssh $CODEMY_HOST supervisorctl status                      # caddy/tailscale/codery-ci RUNNING
ssh $CODEMY_HOST cat /opt/codery/state/*.color
ssh $CODEMY_HOST docker volume ls
ssh $CODEMY_HOST df -h / && free -m                        # disk > 5GiB free; note RAM
ssh $CODEMY_HOST git -C /opt/codery/projects/Codery log --oneline -1   # branch migration/runtime-ops
```

**GATE 0-A:** triage verdict = `ok`, expected containers up, branch checked
out = `migration/runtime-ops`. Record `docker ps` output as the rollback
reference.

### 2.2 Backup + outer safety net

```bash
# Rob (once): netcup control panel -> full VPS snapshot.  [Rob confirmed]
ssh $CODEMY_HOST 'mkdir -p /opt/codery/bin /opt/codery/stacks'
scp /opt/codery/projects/Codery/proxy/scripts/codery-backup.sh \
    /opt/codery/projects/Codery/proxy/scripts/nix-store-sync.sh \
    $CODEMY_HOST:/opt/codery/bin/
ssh $CODEMY_HOST chmod +x /opt/codery/bin/*.sh
ssh $CODEMY_HOST /opt/codery/bin/codery-backup.sh        # verify SHA256SUMS written
```

**GATE 0-B:** netcup snapshot taken AND in-band backup completed
(`SHA256SUMS` exists, size > 0). Do not proceed to phase 1 without BOTH.

### 2.3 Wire the Dockhand token into paseo (OpenMinis executes — the working
agent must never recreate its own container)

```bash
# 1) Edit /opt/codery/services/paseo.yml — append to env_overrides:
#      DOCKHAND_API_TOKEN: DOCKHAND_API_TOKEN
#    (repo copy already staged on the migration branch; sync if not applied)
# 2) Recreate paseo from the CURRENT image + updated YAML:
ssh $CODEMY_HOST codery-ci recreate paseo
# 3) Verify daemon is back and agents can spawn:
ssh $CODEMY_HOST curl -fsS http://127.0.0.1:16767/api/health   # or the active-color port
```

**GATE 0-C:** paseo daemon healthy after recreate; `docker exec paseo env`
shows `DOCKHAND_API_TOKEN` (existence only: `docker exec paseo sh -c
'test -n "$DOCKHAND_API_TOKEN" && echo SET'`).
**Fallback:** `git checkout` the previous service.yml, `codery-ci recreate
paseo` again.

---

## 3. Phase 1 — paseo exits blue/green

**Pre:** sessions inactive (check the Paseo app's session list; ask Rob).

```bash
# 1) Stage the stack + image tag
ssh $CODEMY_HOST 'mkdir -p /opt/codery/stacks/paseo'
scp /opt/codery/projects/Codery/containers/paseo/compose.yml \
    $CODEMY_HOST:/opt/codery/stacks/paseo/compose.yml
PASEO_SHA=$(cat /opt/codery/state/paseo.sha)
ssh $CODEMY_HOST "printf 'PASEO_IMAGE=ghcr.io/coderyoss/codery:paseo-$PASEO_SHA\nTAILNET_IP=%s\n' \$(cat /run/tailscale.ip) > /opt/codery/stacks/paseo/.env"

# 2) Adopt in Dockhand: Stacks -> Import -> /opt/codery/stacks/paseo/compose.yml
#    (via UI, or API: see §6 curl examples). Dockhand will CREATE a second
#    container 'paseo' — STOP: the port 6767 collides with the running blue.
#    INSTEAD: run the switch as cutover: stop blue first.
```

**Correct switch order (no-downtime impossible here; sequence matters):**

```bash
# a) Note current route: Caddy code. -> localhost:16767 (blue)
# b) Verify the compose config dry (no start):
ssh $CODEMY_HOST 'docker compose -f /opt/codery/stacks/paseo/compose.yml --env-file /opt/codery/stacks/paseo/.env config -q && echo COMPOSE-OK'
# c) Stop the codery-ci-managed blue container:
ssh $CODEMY_HOST 'docker stop codery-paseo-blue'
# d) Up the stack (Dockhand UI Deploy, or):
ssh $CODEMY_HOST 'cd /opt/codery/stacks/paseo && docker compose --env-file .env up -d'
# e) Verify:
ssh $CODEMY_HOST 'curl -fsS http://127.0.0.1:6767/api/health'
ssh $CODEMY_HOST 'ss -tlnp | grep 2222'        # tailnet publish listening
# f) Flip the route: edit proxy/routes.yaml (repo) — code. -> http://127.0.0.1:6767
#    then:  ssh $CODEMY_HOST 'codery-ci reload-routes'
# g) Update /opt/codery/services/ = remove paseo.yml (repo: retire file) so
#    codery-ci stops managing it. Keep the file in git history.
```

**Verify (gate 1):** iPad Paseo app connects (code.<domain>); ssh -p 2222 to
the tailnet IP works (host key UNCHANGED — volume preserved); an agent can
run a trivial command.
**Fallback (if broken):** `docker compose down` (stack) → `docker start
codery-paseo-blue` → restore paseo.yml + routes.yaml → `codery-ci
reload-routes` → `codery-ci recreate paseo`. Restore time < 5 min.

## 4. Phase 2 — opendesign + playwright (same pattern, trivial)

Same as phase 1 with their compose files (`/opt/codery/stacks/{opendesign,playwright}`),
ports `7456` (loopback) and playwright internal-only. Adopt via Dockhand,
stop the codery-ci containers, up stacks, flip `opendesign.` route to
`localhost:7456`, retire their service.yml entries, `reload-routes`.
**Verify:** opendesign UI via tailnet; `docker exec paseo curl -fsS
ws://playwright:3000` reachable from paseo (use `wget -qO-` handshake or a
playwright MCP call).
**Fallback:** symmetric — stop stacks, restart codery-ci containers
(`codery-ci recreate opendesign` / `playwright`), restore routes.

## 5. Phase 3 — apps exits blue/green (LAST; only after tools persistence
has been in use for a while)

1. Pre-deploy image sync: `/opt/codery/bin/nix-store-sync.sh <new-apps-image> codery-apps-nix`
2. Stage `/opt/codery/stacks/apps/compose.yml` (translate
   `containers/apps/service.yml`: port_range 8000-9000 loopback, ssh
   `:3333`, mosh UDP 61000 publish `TAILNET_IP:61000/udp`, all volumes
   incl. `codery-apps-nix`).
3. Same switch order as phase 1 (stop green/blue → up stack → verify
   nginx :8080, sshd :3333, mosh, runtime apps re-linked via
   `runtime-apps-restore` inside the new container → flip apps nginx routes
   if any are Caddy-direct).
4. Retire `services/apps.yml`; codery-ci mosh relay + TCP proxy for 3333
   are then unused.

**Verify:** ssh gem@tailnet:3333; mosh session; one runtime app (e.g.
qdrant.<domain>) serving; `tooladd` installs persist.
**Fallback:** stack down → `codery-ci recreate apps` (old color) → restore
services/apps.yml.

## 6. Dockhand API quick reference (OpenMinis curl, from iPad via tailnet)

```bash
source ~/.codery/secrets.env
# Auth layers: Caddy basic auth (-u) + Dockhand bearer token.
curl -su "$DOCKHAND_BASIC_AUTH" -H "Authorization: Bearer $DOCKHAND_API_TOKEN" \
  "$DOCKHAND_URL/api/stacks?env=1"
curl -su "$DOCKHAND_BASIC_AUTH" -H "Authorization: Bearer $DOCKHAND_API_TOKEN" \
  -X POST "$DOCKHAND_URL/api/stacks/remi_backend/deploy?env=1" -d '{"pull":true}'
# Containers: /api/containers · Logs: /api/containers/<id>/logs?tail=100
# Start/Stop/Restart stack: POST /api/stacks/<name>/{start,stop,restart}
```

The paseo agent uses the same API through the `dockhand.*` MCP tools
(http://dockhand:3000, token from its env).

## 7. Phase 4 (joint, after 1–3 verified) — codery-ci slim + dashboard

- Remove build/deploy/cutover/rollback + color machinery + TCP/UDP relays
  from codery-ci (Rust); keep routes/health/MCP/DNS.
- Routes dashboard: all-layers route inventory + health probes (spec §7).
- Playbook P1/P2: `codery-ci boot-verify` (systemd oneshot), `doctor --fix`.
- Deploy via build-orchestrator workflow (manual dispatch) or host build.

## 8. FULL ABORT (scripted escape hatch)

```bash
# Restores today's layout from the in-band backup (phase 0.2):
ssh $CODEMY_HOST '
  set -e
  LATEST=$(ls -dt /opt/backups/codery-full-* | head -1)
  docker compose -f /opt/codery/stacks/*/compose.yml down 2>/dev/null || true
  # 1) state + services + proxy + configs
  tar -xf $LATEST/files/state.tar        -C /opt/codery
  tar -xf $LATEST/files/services.tar     -C /opt/codery
  tar -xf $LATEST/files/proxy.tar        -C /opt/codery
  tar -xf $LATEST/files/supervisor.tar   -C /etc
  cp -a $LATEST/files/Caddyfile /etc/caddy/Caddyfile
  cp -a $LATEST/files/codery.db  /opt/codery/codery.db
  cp -a $LATEST/files/.env       /opt/codery/.env
  # 2) volumes back
  for v in codery-paseo-home codery-paseo-ssh-host-keys; do
    [ -f $LATEST/volumes/$v.tar ] && docker run --rm -v $v:/v -v $LATEST/volumes:/b alpine sh -c "rm -rf /v/* ; tar -xf /b/$v.tar -C /v"
  done
  # 3) images back + containers up (per codery-ci, active colors)
  for t in $LATEST/images/*.tar; do docker load -i $t; done
  for svc in $(ls /opt/codery/state/*.color | xargs -n1 basename | sed s/.color//); do
    codery-ci recreate $svc || docker compose -f /opt/codery/projects/REMI/docker-compose.yml up -d || true
  done
  supervisorctl reload
  codery-ci reload-routes
'
# 4) Repo: git checkout main (or revert the migration merges).
# 5) Verify: codery-triage --json == ok; Paseo app connects; ssh :2222 works.
```

The netcup hypervisor snapshot (phase 0.2) is the outer net if the host
itself is broken: restore snapshot via the netcup panel, then re-apply
nothing — it is a full host image.

---

## 9. Post-migration verification checklist

- [ ] `codery-triage --json` verdict ok
- [ ] Paseo app connects; agent spawn + MCP tools load (incl. `dockhand.*`)
- [ ] `ssh -p 2222` + `ssh -p 3333` + mosh over tailnet
- [ ] Dockhand UI reachable only via tailnet; basic auth + login work;
      public-IP probe fails (connection refused/timeout)
- [ ] REMI stack sync via Dockhand works (edit compose → deploy → healthy)
- [ ] `tooladd` install persists across a container recreate
- [ ] Routes dashboard lists all routes + health (phase 4)
- [ ] `tools-manifest.txt` committed on the migration branch; branch merged
      to main at phase gates
