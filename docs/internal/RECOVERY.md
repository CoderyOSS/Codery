# Hardening rollout — game plan + recovery card (2026-09-21)

Host paths: this file lives on the shared bind at
`/opt/codery/projects/Codery/docs/internal/RECOVERY.md`.
Orchestrator binary: `/opt/codery/codery-ci` (v0.15.0, installed 08:06).
DB backup: `/opt/codery/data/opencode/opencode.db.pre-1.18.31-backup` (1.6GB).

## Where we are

- codery-ci v0.15.0 binary installed on host; daemon NOT yet restarted.
- service.ymls (repo, pushed): sandbox ssh→tailnet, opencode/opendesign→
  loopback+basic_auth; apps ssh→tailnet, 8000-9000→loopback; playwright ws→loopback.
- Sandbox image with opencode 1.18.31: NOT yet built (pin pushed, repo commit ad5f96b).
- apps-green container: STOPPED at 08:13 (SIGTERM — see question below).
- DOCKER-USER stopgap: not yet applied.

## Order of operations (each step has a rollback)

| # | Step | Who | Risk | Rollback |
|---|------|-----|------|----------|
| 1 | `caddy version` — require ≥2.8 for `basic_auth` | you | none | if <2.8: tell me, I switch directive to `basicauth` |
| 2 | DOCKER-USER stopgap rules (script below) | you | low (apps already down) | `iptables -F DOCKER-USER && iptables -I DOCKER-USER -j RETURN` |
| 3 | Append 2 auth lines to /opt/codery/.env (script below) | you | none (unused until reload) | delete the 2 lines |
| 4 | `sudo supervisorctl restart codery-ci-daemon` | you | low (MCP/CI blip ~5s) | binary rollback: download v0.14.2 asset from GH release, `mv` over /opt/codery/codery-ci, restart again |
| 5 | `codery-ci deploy apps s6-restore-oneshot` → `codery-ci cutover apps` | you | low (apps already down; healthcheck gates cutover) | `codery-ci rollback apps` |
| 6 | Verify from phone LTE: `http://178.104.120.61:13000` must NOT connect; `https://carta.rancidgrandmas.online` must work over Tailscale | you | — | — |
| 7 | `codery-ci deploy sandbox opencode-1.18.31` (new image + new binds, staged as green, health-gated) | you | medium | don't cutover → blue (my session) untouched; delete preview |
| 8 | `codery-ci cutover sandbox` — KILLS THE AGENT SESSION (expected) | you | medium | `codery-ci rollback sandbox` (old image, old DB absent-migrations risk: restore DB backup first — see below) |
| 9 | Post checks: `opencode --version`=1.18.31; browser prompts basic auth; `ssh gem@apps` works from sandbox; `docker port codery-sandbox-blue` shows 127.0.0.1/tailscale binds | you+agent | — | — |
| 10 | Persist DOCKER-USER: `netfilter-persistent save` (or systemd unit) | you | none | — |

playwright: stopped; fix lands on its next deploy. No action.

## Recovery commands (copy-paste cards)

DOCKER-USER flush (un-break all forwarding):
```
iptables -F DOCKER-USER
iptables -A DOCKER-USER -j RETURN
```

Caddy auth breaks routes (reload fails / 401 everywhere):
```
sed -i '/OPENCODE_BASIC_AUTH\|OPENDESIGN_BASIC_AUTH/d' /opt/codery/.env
sudo supervisorctl restart codery-ci-daemon   # re-render without auth
# or: codery-ci reload-routes
```

Sandbox broken after cutover (agent won't start):
```
codery-ci rollback sandbox
# if opencode crashes on the migrated DB:
cp /opt/codery/data/opencode/opencode.db /opt/codery/data/opencode/opencode.db.migrated-broken
cp /opt/codery/data/opencode/opencode.db.pre-1.18.31-backup /opt/codery/data/opencode/opencode.db
codery-ci rollback sandbox   # again, with old DB in place
```

codery-ci binary broken:
```
curl -L -o /tmp/codery-ci https://github.com/CoderyOSS/Codery/releases/download/codery-ci-v0.14.2/codery-ci-linux-$(uname -m | sed 's/x86_64/x86_64/;s/aarch64/aarch64/')
sudo mv /tmp/codery-ci /opt/codery/codery-ci && sudo chmod +x /opt/codery/codery-ci
sudo supervisorctl restart codery-ci-daemon
```

SSH to sandbox stops working after step 5 (bind → tailnet):
connect via the Tailscale IP / MagicDNS name instead of the public IP.
Emergency revert: `sed -i 's/bind: tailnet/bind: any/' containers/sandbox/service.yml`
on your dev checkout + redeploy — or iptables flush above (binds still
tailnet after step 5; revert = redeploy with bind: any).

## DOCKER-USER stopgap (step 2 script)

```
for src in tailscale0 docker0 br-+ lo; do
  iptables -C DOCKER-USER -i $src -j RETURN 2>/dev/null || iptables -I DOCKER-USER -i $src -j RETURN
done
iptables -C DOCKER-USER -p tcp -m multiport --dports 13000,17456,10022,23000,27456,20022,2222,3333,18022,28022 -j DROP 2>/dev/null || iptables -I DOCKER-USER -p tcp -m multiport --dports 13000,17456,10022,23000,27456,20022,2222,3333,18022,28022 -j DROP
iptables -C DOCKER-USER -p tcp -m multiport --dports 18000:19000,28000:29000,43000,53000 -j DROP 2>/dev/null || iptables -I DOCKER-USER -p tcp -m multiport --dports 18000:19000,28000:29000,43000,53000 -j DROP
```

## .env additions (step 3 script)

```
cd /opt/codery
cp .env .env.bak-$(date +%s)
printf 'OPENCODE_BASIC_AUTH="gem %s"\n' "$(caddy hash-password --plaintext 'PASSWORD1')" >> .env
printf 'OPENDESIGN_BASIC_AUTH="gem %s"\n' "$(caddy hash-password --plaintext 'PASSWORD2')" >> .env
```
