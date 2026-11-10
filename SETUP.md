# Codery VPS Setup Guide (human version)

The agent-executable path is [RUNBOOK.md](RUNBOOK.md) — if an OpenMINIS agent
is available, use it. This guide is the same path, done by hand.

## Prerequisites

- A fresh Ubuntu/Debian 24.x VPS with root SSH
- A domain with Cloudflare DNS
- Secrets — see [`host/templates/env.example`](host/templates/env.example) for
  the full key list; at minimum: Cloudflare API token (zone DNS edit),
  Tailscale auth key, GitHub App PEM/ID/slug, GHCR pull token, provider API
  keys

## Install

1. **Host layer** — clone the repo, stage secrets, run the bootstrap:

   ```bash
   git clone https://github.com/CoderyOSS/Codery /root/Codery
   sudo bash /root/Codery/host/bootstrap.sh /root/secrets.env
   ```

   Verify: `tailscale ip` returns an IP, `docker info` works.

2. **Core stacks** — follow [RUNBOOK.md](RUNBOOK.md) Phase 1 (dockhand →
   zoraxy → codery-net), gates included.

3. **Tools** — Phase 2 of the same runbook: choose your `COMPOSE_PROFILES`,
   bring up the devtools stack, verify SSH/mosh/agent access.

4. **Projects** — Phase 3: clone project repos into `/opt/codery/projects/`,
   register each compose in Dockhand, add Zoraxy routes.

## After install

- Change Dockhand + Zoraxy admin passwords
- Review `/opt/codery/README.md` (the host tree) once
- Schedule `codery-backup.sh` (cron) and test a restore
