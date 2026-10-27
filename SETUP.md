# Codery VPS Setup Guide

Complete guide to installing Codery on a fresh VPS. Start here for new installs.

---

## Prerequisites

- A VPS running **Ubuntu 24.04** (2 vCPU / 4GB RAM minimum)
- A **GitHub account** with a fork or copy of this repo
- A **Tailscale account** (free tier works) for VPN access
- (Optional) A domain name pointing to your VPS for HTTPS

---

## Step 1: Create the VPS

Use `hosting/cloud-init.yaml` as the user data / launch script when creating your VPS. This installs Docker, configures the firewall (SSH, HTTP, HTTPS), and creates a `deploy` user.

Provider-specific instructions:
- [DigitalOcean](hosting/examples/digitalocean.md)
- [AWS Lightsail](hosting/examples/aws-lightsail.md)
- [Hetzner](hosting/examples/hetzner.md)

After provisioning, note the VPS public IP.

---

## Step 2: Configure GitHub Secrets

In your GitHub repo, go to **Settings → Secrets and variables → Actions** and add:

| Secret | Value |
|--------|-------|
| `DEPLOY_HOST` | VPS public IP (or Tailscale IP after Step 3) |
| `DEPLOY_SSH_KEY` | Private SSH key matching the public key in `cloud-init.yaml` |
| `ENV_FILE` | Contents of `/opt/codery/.env` (see below) |

### ENV_FILE contents

```
DOMAIN_NAME=yourdomain.com
GHCR_USERNAME=<your-github-username>
GHCR_TOKEN=<github-personal-access-token>
ANTHROPIC_API_KEY=sk-ant-...
ZAI_API_KEY=<zai-key>
GITHUB_APP_ID=<app-id>
GITHUB_APP_SLUG=<app-slug>
TAILSCALE_AUTH_KEY=tskey-auth-...
```

- `GHCR_TOKEN` — GitHub personal access token with `read:packages` scope
- `TAILSCALE_AUTH_KEY` — from [Tailscale Keys](https://login.tailscale.com/admin/settings/keys) (use a reusable key)
- `GITHUB_APP_ID` / `GITHUB_APP_SLUG` — see [GitHub App setup](docs/customizing.md) if using a GitHub App

---

## Step 3: Run Setup Host Workflow

Go to **Actions → Setup Host → Run workflow**. This runs `proxy/host-setup.sh` on your VPS, which installs:

- **Caddy** — reverse proxy with automatic TLS
- **Tailscale** — WireGuard VPN for secure access
- **Supervisord** — process manager for host services

---

## Step 4: Verify Tailscale

After Setup Host completes, Tailscale should be running and authenticated (the auth key from `ENV_FILE` is used automatically).

Check on the VPS:

```bash
tailscale status
```

You should see your VPS listed with a `100.x.x.x` Tailscale IP. From another machine on your tailnet:

```bash
ping <tailscale-ip>
ssh deploy@<tailscale-ip>
```

If Tailscale isn't connected, check the auth key and re-run:

```bash
tailscale up --authkey=tskey-auth-... --ssh
```

Once working, update `DEPLOY_HOST` in GitHub secrets to the Tailscale IP for secure deploys.

---

## Step 5: SSH Access (no firewall changes)

SSH is **Tailscale-only**: the proxy binds the tailnet IP, so no public
firewall rule is needed (do **not** `ufw allow 2222/tcp`). Add your public
key to `/opt/codery/ssh/authorized_keys` on the VPS.

---

## Step 6: First Deploy

Deploy pipelines are **manual** — pushing to `main` never deploys:



- **Actions → Build Paseo → Run workflow** — deploys the AI coding environment
- **Actions → Build Apps → Run workflow** — deploys the apps container (if needed)

The first build takes ~5-8 minutes. Subsequent builds use Docker layer caching.

### What gets deployed

The deploy workflow:
1. Builds a Docker image and pushes to GHCR
2. Syncs service YAML and route configs to `/opt/codery/services/` on the VPS
3. Calls `codery-ci deploy` which does a blue/green deployment (start new container, health check, cutover, stop old)

---

## Step 7: Verify

Check that everything is running:

```bash
# On the VPS
supervisorctl status          # caddy, tailscale, codery-ci-daemon should be RUNNING
docker ps                     # codery-paseo-<color> and codery-apps-<color>
tailscale status              # should show connected
```

### Test Paseo

The paseo daemon serves on port 6767 inside its container, via Caddy:

```
https://code.<yourdomain.com>
```

Or via the Tailscale IP at the mapped host port (blue=16767, green=26767).

### Test SSH

```bash
ssh -p 2222 paseo@<tailscale-ip>
```

Connection chain: `client → :2222 (codery-ci TCP proxy) → :1xx22 (docker publish) → :2022 (sshd)`.
Apps SSH is the same pattern on :3333 (`gem@`).

---

## Next Steps

- **API keys** — Add `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc. to the ENV_FILE secret and redeploy
- **GitHub App** — Set up a GitHub App for container authentication, see [docs/customizing.md](docs/customizing.md)
- **Custom domain** — Set `DOMAIN_NAME` in ENV_FILE and ensure DNS points to your VPS
- **Add web apps** — Declare apps in `.devcontainer/devcontainer.json` under `customizations.codery.apps`

---

## Troubleshooting

### Deploy fails with "image pull failed"
Check that `GHCR_USERNAME` and `GHCR_TOKEN` in the ENV_FILE secret are correct. The token needs `read:packages` scope.

### Port 2222 SSH hangs
SSH binds the Tailscale IP only. Verify you are connecting to the tailscale IP
(`tailscale ip -4` on the VPS) and that your pubkey is in
`/opt/codery/ssh/authorized_keys`. If a stale public `ufw allow 2222/tcp` rule
exists, delete it — the port is not meant to be publicly reachable.

### Tailscale not connecting
Verify the auth key is valid and not expired. Check `supervisorctl status tailscale` on the VPS. If it shows FATAL, try `tailscale down && tailscale up --ssh`.

### Container crash-looping
Check container logs:

```bash
docker logs codery-paseo-<color> --tail 50
```

Find the failing service with
`docker exec codery-apps-<color> /command/s6-svstat /run/service/<name>`,
then read its output in the container logs above.

### CoderyCI MCP tools not responding
The codery-ci daemon must be running on the host:

```bash
supervisorctl status codery-ci-daemon
```

If not running, check `/var/log/supervisor/codery-ci-daemon.log` for errors.
