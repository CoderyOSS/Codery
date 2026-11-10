# Codery VPS — Outage Playbook & Postmortem

**Incident date:** 2026-10-18 · **Host:** netcup VPS `v2202609421798526076` · **Public IP:** `202.61.247.50` · **Tailnet IP:** `100.89.78.41`
**Scope:** host + Codery orchestrator (codery-ci 0.15.0) + managed services (apps, sandbox, playwright, remi stack)
**Status:** resolved same day. This document is the postmortem, the triage manual, and the automation design for Open Minis.

---

## 0. TL;DR — the 60-second triage

When the box "disappears", work **top to bottom**. Each step is one SSH round-trip:

```bash
# 0) Can we even get in? (works even when Tailscale is dead)
ssh -o BatchMode=yes -o ConnectTimeout=8 root@202.61.247.50 uptime

# 1) One-shot health snapshot (deployed 2026-10-18, read-only)
ssh root@202.61.247.50 /usr/local/bin/codery-triage        # human summary
ssh root@202.61.247.50 /usr/local/bin/codery-triage --json # machine-readable
```

`codery-triage` checks, in order of how today's failure cascaded: Tailscale unit → `/run/tailscale.ip` → supervisord programs → docker containers **plus their live port bindings and network membership** → listeners on the tailnet IP → loopback HTTP upstreams → `codery-ci diagnose --json` → disk/mem. It prints a `verdict` (`ok|degraded|down`) and a `problems[]` list. Interpretation and fixes: §7 and Appendix B.

**Golden rule learned today: never reboot first.** The machine was healthy the whole time; a reboot could not have fixed the actual problem and (before today's hardening) made everything worse.

---

## 1. Incident summary

| Time (CEST) | Event |
|---|---|
| 17:26 | Clean shutdown #1 (netcup-side; no OOM, no panic in journal) |
| 17:38 | Boot #2. `tailscaled` is `disabled` → never starts. Caddy/ci-daemon/dns-update wait-loops time out after 120 s. **Full tailnet outage begins** |
| 17:53 | Docker starts managed containers. Docker 29.8.1 silently applies **zero** port bindings (tailnet-anchored binds reference a nonexistent `tailscale0`) and drops the sandbox's network endpoint entirely |
| 19:43 / 19:52 | Two clean reboots (user via SCP, trying to fix perceived hang) — no effect; problem persists identically |
| 21:08 | Root cause found via public-IP SSH; `systemctl enable --now tailscaled` → tailnet restored in seconds |
| 21:10–21:17 | `dns-update` re-run (writes `/run/tailscale.ip`, upserts Cloudflare wildcard); ci-daemon conf given a wait-loop; ci-daemon started |
| 21:25–21:45 | `codery-ci recreate apps` → 1002 bindings restored; loopback upstreams live again |
| 21:50–22:10 | Sandbox recreated (host keys preserved) → :2222 chain restored; `runtime-apps-restore` re-run → qdrant/ideas/booksearch/labeler up; REMI frontend fixed (stale astro lockfile); playwright redeployed v1.54.1 |

**Impact:** ~3 h 40 min of full tailnet unreachability (SSH, all 12 vhosts, sandbox/apps SSH proxies), plus ~1 h of partial degradation (qdrant/ideas/booksearch 502, :2222 resets) while the host itself was serving SSH normally.

**Not the cause:** OpenCode OOM (verified: zero `oom-killer` events in any of the last 4 boots; the one grep hit was containerd's own JSON config containing the string "OOMScore"). Past OOM history made this a red herring and cost diagnosis time.

---

## 2. Root causes

1. **RC1 — `tailscaled` was installed but `disabled` (systemd).** A deliberate "divergence" (tailscale managed outside the s6/supervisor world) was never closed by enabling the unit. Result: *every reboot silently killed the tailnet* until someone ran the chain by hand. This was the single root cause of the outage window.
2. **RC2 — Docker 29.8.1 atomic, silent port-binding failure.** Containers whose bindings reference `100.89.78.41` (apps: 1000+ loopback binds + `:2022`; sandbox: `:20022`, `:23000`, `:27456`) started while `tailscale0` did not exist → Docker applied **no** bindings at all (all-or-nothing) and logged only warnings. `docker restart` and even a full `dockerd` restart did **not** heal it; only a fresh `docker create` (i.e. `codery-ci recreate`) did.
3. **RC3 — Sandbox lost its network endpoint.** Worse than RC2: dockerd's boot-time restore ("Removing stale sandbox … not found") tore down the sandbox's codery-net endpoint and never rebuilt it on any restart — the container ran with **no network at all** until recreate. Plain restarts leave this state broken forever.
4. **RC4 — `codery-ci-daemon` FATAL boot race.** The daemon binds TCP proxies to the tailnet IP read from `/run/tailscale.ip`; started before that file exists, it exited too quickly and exhausted supervisor retries → FATAL even after Tailscale recovered. (Fixed today: wait-loop wrapper in its conf.)
5. **RC5 — `runtime-apps` s6 oneshot did not run after container recreate.** qdrant/ideas/booksearch/labeler live in `/etc/s6-overlay/apps.d` and are linked into `/run/service` by the oneshot; after `codery-ci recreate apps` nothing linked them → apps nginx 502 for every runtime-managed vhost. Manual re-run of `/usr/local/bin/runtime-apps-restore` fixed it (it is idempotent by design).
6. **RC6 — REMI frontend stale astro lockfile (found during sweep, not outage-causing).** `.astro/dev.json` pinned `pid: 22`; every fresh container assigns early PIDs starting near 22 → astro falsely believed another dev server was running → crash-loop since boot. Deleting `dev.json` fixed it.

---

## 3. Why the first responses didn't work (detection & response gaps)

- **Detection gap:** nothing external noticed for ~3.5 h. The only signals were human ("dashboard down", "ssh hangs") and both pointed at the wrong layer (both are tailnet-dependent, so *any* tailnet failure mimics a dead box).
- **Wrong reflex:** rebooting. Clean-shutdown journals show the box was fine; reboots only re-rolled the same broken boot sequence. Postmortem rule: **determine clean-vs-crash from the journal before touching power** (`journalctl --list-boots`, then `journalctl -b -N -n 20` — look for `Reached target poweroff` = clean; `oom-killer`/panic = crash).
- **Access fragility:** the habit `ssh root@netcup` resolves via Tailscale MagicDNS only — during a tailnet outage the "obvious" path is exactly the broken one. The public-IP root key path existed but wasn't muscle memory.
- **Layered opacity:** three independent breakages (bindings, endpoint, oneshot) all presented as "site down". Without `docker port` / `.NetworkSettings.Networks` checks, each looks identical from outside.

---

## 4. Remediations applied on 2026-10-18

| # | Fix | Where | Notes |
|---|---|---|---|
| 1 | `systemctl enable --now tailscaled` | host | **The** root-cause fix; survives reboots now |
| 2 | `supervisorctl start dns-update` | host | Re-wrote `/run/tailscale.ip` + upserted CF wildcard `*.rancidgrandmas.online → 100.89.78.41` |
| 3 | ci-daemon conf: `command=/bin/bash -c 'while [ ! -s /run/tailscale.ip ]; do sleep 2; done; exec /opt/codery/codery-ci daemon'` | `/etc/supervisor/conf.d/codery-ci-daemon.conf` | Backup: `codery-ci-daemon.conf.bak-20261018`. Mirrors the existing wait-loop pattern in `caddy-start.sh` / `dns-update.sh` |
| 4 | `codery-ci recreate apps` | host | Restored 1002 bindings. Image unchanged (`apps-sqlite3`) |
| 5 | Sandbox: `docker cp /etc/ssh → /opt/codery/state/sandbox-ssh-backup-20261018` → `codery-ci recreate sandbox` → `docker cp` back → `s6-svc -r /run/service/sshd` | host | **Host keys preserved** — fingerprint `SHA256:97HjIHqqiQe+Q9cFDA0ue4Wx/22UB7+y1RLEq2PPVRk` unchanged → Byson/Blink/known_hosts need no re-pin |
| 6 | `docker exec codery-apps-blue /usr/local/bin/runtime-apps-restore` | apps container | Idempotent; relinked qdrant/ideas/booksearch/labeler into `/run/service` |
| 7 | `mv .astro/dev.json{,.stale-20261018}` + `docker compose up -d frontend` | `/root/projects/REMI` | REMI frontend 200 on :4321 |
| 8 | `codery-ci deploy playwright v1.54.1` | host | Playwright was DEAD since boot |
| 9 | Deployed `/usr/local/bin/codery-triage` (read-only snapshot, Appendix A) | host | One-SSH-round-trip health check for humans and Open Minis |

**Known cosmetic residue:** `codery-ci diagnose` still lists `remi-frontend DEAD` (false positive — the service is compose-owned by design, its own yml forbids `codery-ci deploy remi-frontend`) and a stale `apps` preview marker (optional cleanup: `codery-ci cancel-preview apps`).

---

## 5. Residual risks (honest list)

1. **The Docker-binding trap is still armed for any boot where `tailscale0` comes up *after* docker starts containers.** Today's service-level wait-loops (caddy, dns-update, ci-daemon) cover the host processes, but container bindings depend on boot ordering between `docker.service` container-restore and `tailscaled.service`. With tailscaled now enabled it should win the race — but "should" is not a guarantee. → P1 in §6.
2. **dockerd endpoint-restore bug (RC3) can strand any container**, not just sandbox. Only recreate reliably healed it.
3. **`runtime-apps` is a oneshot** — anything that skips it (fast recreate, manual docker start) silently leaves runtime apps down. Diagnose does not currently flag "apps.d bundle not linked".
4. **codery MCP daemon binds `0.0.0.0:4040`.** Tailnet-only in practice (firewall/NAT), but the `mcp-exec` gate should stay enabled and any public exposure must stay behind caddy + basic auth.
5. **Apex `rancidgrandmas.online` → Cloudflare 521.** Pre-existing (origin not this box's tailnet path); not investigated. All real services are subdomains via the tailnet wildcard.
6. **Single external probe point:** everything healthy today was verified from one device. See §9 for making the watch independent.

---

## 6. Prevention roadmap (prioritized)

**P0 — External watch (hours, no code)**
A probe from *outside the box* that runs `codery-triage --json` and alerts on `verdict != ok`. Two carriers:
- **iOS Shortcuts automation** ("Run script over SSH" action → `ssh root@202.61.247.50 /usr/local/bin/codery-triage --json`, notify if `"verdict":"down|degraded"`, run every 15–30 min). Survives Minis being closed; guaranteed delivery via notification.
- **Open Minis scheduled task** (in-app, best-effort while the app is alive): `minis-scheduled create --label codery-probe --interval 15m --command "ssh -o BatchMode=yes root@202.61.247.50 /usr/local/bin/codery-triage --json" --prompt "Interpret codery-triage output; if verdict is not ok, follow PLAYBOOK.md §7 on the server and report; else reply 'ok'."` — the `--command` prefilled-tool-call feature means the probe costs one SSH + one short model turn.

**P1 — Boot self-verification (small codery-ci feature)**
`codery-ci boot-verify` as a `Type=oneshot` systemd unit `After=docker.service tailscaled.service`:
- wait up to e.g. 180 s for `/run/tailscale.ip`
- verify per managed service: container running & healthy, `NetworkSettings.Ports` non-empty (count > 0), codery-net membership present, expected tailnet listeners bound
- re-run `/usr/local/bin/runtime-apps-restore` inside apps (idempotent)
- anything broken → log loudly, write `/run/codery-boot-degraded`, and (flag-gated) auto-run `codery-ci recreate <service>`

**P2 — `codery-ci doctor` (self-heal command)**
Today's incident repairs, codified:
```
codery-ci doctor            # read-only: everything diagnose checks PLUS live bindings, endpoints, apps.d linkage, /run/tailscale.ip, CF wildcard vs tailscale IP
codery-ci doctor --fix      # dns-update re-run → runtime-apps-restore → supervisorctl restart for FATAL units with valid wait-loops → recreate services with zero live bindings
codery-ci recreate <svc> --preserve-host-keys   # automates the manual docker-cp dance from §4.5
```

**P3 — Hygiene**
- diagnose additions: flag empty `NetworkSettings.Ports` on managed containers, missing network membership, unlinked `apps.d` bundles, `dns-update` last-success age (CF record IP vs current tailnet IP).
- REMI: entrypoint should delete stale `.astro/dev.json` (or run `astro dev --force`) instead of crash-looping.
- Consider `tailscale-up` supervisor program autostart=true (it waits for the socket) so the whole chain is boot-safe even if someone disables the systemd unit again.
- Keep `mcp-exec` gate enabled; audit `0.0.0.0:4040` binding before ever exposing it beyond the tailnet.

---

## 7. Future investigation playbook (step by step)

1. **Scope it:** one service or everything? One vhost 502 → upstream layer (§7.6). Everything + SSH dead → continue.
2. **Clean or crash?** `journalctl --list-boots | tail`; for the previous boot `journalctl -b -N -n 25` — `Reached target poweroff` = clean (netcup maintenance / human), `oom-killer`/`Segfault`/no shutdown trail = crash. *Do not reboot a cleanly-shut-down box — fix the actual layer.*
3. **Get in:** public IP first (`ssh root@202.61.247.50`) — it works even with Tailscale dead. netcup SCP console is the fallback of last resort.
4. **Tailscale layer:** `systemctl is-active is-enabled tailscaled` (both must be `active`/`enabled`), `tailscale status`, `cat /run/tailscale.ip`. Fix: `systemctl enable --now tailscaled` → `supervisorctl start dns-update` → confirm `/run/tailscale.ip` + CF wildcard.
5. **Orchestrator layer:** `supervisorctl status` — expect caddy RUNNING, codery-ci-daemon RUNNING, dns-update EXITED (one-shot semantics; EXITED is healthy *if* `/run/tailscale.ip` exists and the CF upsert succeeded in `/var/log/supervisor/dns-update.log`). FATAL/STOPPED on caddy/ci-daemon = problem.
6. **Container layer:** `docker ps` states; **`docker port codery-apps-blue | wc -l` must be ≈1002 and codery-sandbox-green must list 3 bindings**; `docker inspect <c> --format '{{json .NetworkSettings.Networks}}'` must show `codery-net`. Any zero → `codery-ci recreate <svc>` (restarts do NOT heal Docker 29 binding/endpoint loss). Then `runtime-apps-restore` in apps.
7. **Routing layer:** listeners on the tailnet IP (:80/:443 caddy, :2222 ci-daemon→sandbox:20022, :3333 ci-daemon→apps:2022) and loopback upstreams (:8080 apps nginx, :23000 sandbox opencode, :27456 opendesign, :4321 remi). 401 from qdrant/opencode is *healthy* (auth gates).
8. **Machine check:** `codery-triage --json` (wraps all of the above) and `codery-ci diagnose --json`.
9. **Log sources worth pulling (in order):** `/var/log/supervisor/*.log`, `journalctl -u docker` around the boot timestamp, `journalctl -u tailscaled -b`, `docker logs --tail` per container, `codery-ci diagnose`.
10. **Write it down:** append findings + fixes to the Minis memory log (or this file) — today's RC2/RC3/RC5 were only recognizable because prior-session notes existed.

**Decision shortcut:** tailnet dead but public-IP SSH alive = **RC1 path (Tailscale), not a crash**. Tailnet alive but vhost 502 = **container/route layer** (`docker port` first). `:2222` resets but `:3333` works = sandbox container network/bindings (RC3).

---

## 8. CoderyCI feature proposals (ranked by outage-prevention value)

1. **`codery-ci doctor [--fix]`** (§6 P2) — turns today's 2-hour manual sweep into one command. The building blocks already exist: `diagnose` (state/routes/previews), `preflight` (supervisord/Tailscale/Caddy admin). Missing pieces: live port bindings, network membership, `apps.d` linkage, `/run/tailscale.ip`, CF-record-vs-actual-IP.
2. **`boot-verify` oneshot** (§6 P1) — the only thing that actually closes RC2/RC3's boot window.
3. **`recreate --preserve-host-keys`** — automates §4.5; removes the one scary manual step in sandbox recovery and makes "just recreate it" a safe reflex instead of a last resort.
4. **diagnose: flag zero-binding / no-endpoint containers** — today Docker failed *silently*; the orchestrator's own state (`color`/`sha` files) says healthy while `NetworkSettings.Ports = {}`. This gap cost the most diagnosis time after RC1.
5. **`runtime-apps` as a supervised longrun** (watch `apps.d`, re-link on change) or guaranteed re-run inside boot-verify — closes RC5 permanently.
6. **`codery-ci status --json`** — a single machine-readable summary (supervisord + docker + listeners + tailnet) for external probers. (Until it exists, `/usr/local/bin/codery-triage` plays this role.)
7. **Clean-shutdown vs crash annotation** in `diagnose`: parse `journalctl --list-boots` + previous-boot tail; one line like `last exit: clean poweroff (netcup maintenance likely)` kills the OOM red herring immediately.

---

## 9. Open Minis integration (verified against openminis.app docs + on-device capabilities)

Open Minis is an on-device agent: full **Alpine Linux shell** (real `ssh`, persistent packages), **custom Skills (SKILL.md format)**, **MCP client (HTTP/SSE + STDIO, per-session toggles, secret injection via env vars)**, **scheduled tasks**, **persistent memory**, **subagents (Agent Loop models)**, and a **browser** (netcup SCP etc.). That menu maps directly onto this VPS:

### T1 — Server-side triage endpoint (deployed ✅)
`/usr/local/bin/codery-triage [--json]` — read-only, no auth beyond SSH, one round-trip.
**Why SSH-first:** it works over the public IP even while the tailnet is down — i.e., in exactly the state where every other Codery surface (MCP, UI, caddy) is unreachable. SSH + a public key is the outage-proof control channel.

### T2 — `codery-ops` skill (created ✅, Minis side)
`/var/minis/skills/codery-ops/SKILL.md` — encodes §0/§7 as instructions: run triage, read `verdict`/`problems[]`, map to fixes, escalate per decision shortcut. Skills are the right Minis primitive for *procedure*; the playbook (this file) stays the source of truth on the server and the skill points at it (`/opt/codery/projects/Codery/PLAYBOOK.md`).

### T3 — MCP to the Codery daemon (configure once, highest day-2 leverage)
The daemon already speaks MCP (`codery-ci daemon` → `:4040/sse`, deploy UI on `:4041`). Open Minis natively supports MCP HTTP/SSE servers with per-session toggles, so Codery tools (deploy, cutover, restart_service, diagnose, mcp-exec) become native Minis tools:
- **Tailnet mode:** MCP server URL `http://100.89.78.41:4040/sse` — simple, private.
- **Outage-proof mode:** run an SSH tunnel from the Minis shell (`ssh -N -L 4040:127.0.0.1:4040 root@202.61.247.50 &`) and point Minis at `http://127.0.0.1:4040/sse` — works even when the tailnet is down (T1's channel carrying T3's tools).
- Keep `codery-ci mcp-exec status` = enabled-allowlist; MCP gives write power (deploys), so gate it deliberately. Note the lesson: MCP rides the tailnet by default — during a tailnet outage use the tunnel mode; that's why T1 stays the incident entry point.

### T4 — Watch & wake (two carriers, one contract)
- **iOS Shortcuts automation (guaranteed):** "Run script over SSH" → `ssh root@202.61.247.50 /usr/local/bin/codery-triage --json` every 15–30 min → `Get values from JSON` (`verdict`) → `If ≠ ok` → notify. Runs even with Minis closed; this is the true 24/7 probe.
- **Minis scheduled probe (in-app, best effort):** the exact command in §6 P0 (`--command` prefilled → one model turn per fire). Also useful: `--trigger on-completion` chains for post-repair verification.

### T5 — Memory conventions
- **Stable facts → GLOBAL.md:** public IP `202.61.247.50`, tailnet IP, port map (:2222 sandbox / :3333 apps, `gem@`), host-key fingerprint, "tailscaled must stay enabled", "never `codery-ci deploy remi-frontend`".
- **Incidents → daily log:** what broke, what fixed it (this doc is the template; RC numbering keeps future postmortems comparable).
- **Secrets:** never in memory files — Minis env vars live in the Keychain-backed store; reference by `$VAR` in scripts.

### What Open Minis does *not* need
No custom daemon, no agent installed on the VPS, no extra exposed ports. The VPS already speaks: SSH (control), HTTP (observability via caddy), MCP (day-2 ops). Minis speaks all three natively. The only server-side artifact this integration adds is the ~150-line read-only `codery-triage` script — everything else is configuration.

---

## Appendix A — `/usr/local/bin/codery-triage`

Read-only snapshot (deployed 2026-10-18). `--json` for machines. Checks ≈ this document's triage order; emits `verdict` + `problems[]`. Source kept alongside this playbook in the repo (`system/codery-triage` symlink target documented in git history). Safe to run at any frequency.

## Appendix B — fix cheat sheet (the whole playbook in 10 lines)

```bash
systemctl enable --now tailscaled && supervisorctl start dns-update   # RC1
supervisorctl start codery-ci-daemon                                  # RC4 (post-wait-loop)
codery-ci recreate apps && docker exec codery-apps-blue /usr/local/bin/runtime-apps-restore  # RC2+RC5
# sandbox with key preservation: see §4.5 (docker cp ssh keys → recreate → cp back → s6-svc -r sshd)
cd /root/projects/REMI && docker compose up -d frontend               # RC6 (after clearing .astro/dev.json)
codery-ci deploy playwright <sha>                                     # dead browser runtime
codery-ci diagnose && codery-ci preflight && codery-triage            # verify
```

## Appendix C — known-good constants

| Item | Value |
|---|---|
| Public SSH | `root@202.61.247.50` (Minis key authorized) |
| Tailnet | `100.89.78.41` (`netcup` in MagicDNS) |
| Sandbox SSH | `ssh -p 2222 gem@100.89.78.41` → ci-daemon → sandbox:22 |
| Apps SSH | `ssh -p 3333 gem@100.89.78.41` → ci-daemon → apps:2022 |
| Sandbox host key (ed25519) | `SHA256:97HjIHqqiQe+Q9cFDA0ue4Wx/22UB7+y1RLEq2PPVRk` |
| Service defs | `/opt/codery/services/{apps,sandbox,playwright,remi-frontend}.yml` |
| Orchestrator state | `/opt/codery/state/{apps,sandbox,playwright}.{color,sha}` |
| Logs | `/var/log/supervisor/*.log` · `journalctl -u {docker,tailscaled}` |
| MCP | `:4040/sse` (daemon), UI `:4041`, Caddy admin `:2019` |
