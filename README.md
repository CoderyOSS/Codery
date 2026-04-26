# Codery

[![Project Status: Pre-Alpha](https://img.shields.io/badge/status-pre--alpha-orange)](https://github.com/CoderyOSS/Codery)

Infrastructure for a VPS-hosted AI development environment. Two Docker containers — a sandbox (OpenCode + VS Code) and an apps container (project web servers) — deployed via CoderyCI with blue/green zero-downtime deployments.

> **Status:** This project is pre-1.0 and under active development. All components follow [semantic versioning](https://semver.org). APIs, configuration formats, and deployment workflows may change without notice. Not recommended for production use yet.

## Components

| Component | Description |
|-----------|-------------|
| **Sandbox** | AI-assisted development environment (OpenCode + VS Code in browser) |
| **Apps** | Host project web servers (Bun/Node/etc.) |
| **CoderyCI** | Rust orchestrator binary — handles blue/green deploys on the host |
| **ShellGate** | Secure shell access component (in development) |

## Quickstart

### 1. Provision a VPS

Create an Ubuntu 24.04 server on any provider. Apply `hosting/cloud-init.yaml` as user data. See `hosting/examples/` for provider-specific instructions:

- [Hetzner](hosting/examples/hetzner.md)
- [DigitalOcean](hosting/examples/digitalocean.md)
- [AWS Lightsail / EC2](hosting/examples/aws-lightsail.md)

### 2. Configure GitHub secrets

Add to your fork's **Settings > Secrets and variables > Actions**:

| Secret | Description |
|--------|-------------|
| `DEPLOY_HOST` | VPS IP address or hostname |
| `DEPLOY_SSH_KEY` | SSH private key for the `deploy` user |
| `ENV_FILE` | Full contents of your `.env` file (API keys, domain, etc.) |

### 3. Deploy

```bash
# One-time host setup (Caddy, Tailscale, supervisord)
gh workflow run "Setup Host" --ref main

# Deploy containers
gh workflow run "Build Apps" --ref main
gh workflow run "Build Sandbox" --ref main
```

## Adding a web service

1. Write a server listening on port **8000-9000** (inside the apps container)
2. Add `containers/apps/supervisor/conf.d/myapp.conf`
3. Add entry to `proxy/apps-routes.json`: `{"subdomain": "...", "port": PORT}`
4. Push to `main`

## Access

- OpenCode: `https://opencode.yourdomain.com`
- VS Code: `https://vscode.yourdomain.com`

## Troubleshooting

```bash
/opt/codery/codery-ci --version
cat /opt/codery/state/sandbox    # active color
cat /opt/codery/state/apps
```

## Documentation

- [CLAUDE.md](CLAUDE.md) — Full architecture reference
- [hosting/examples/](hosting/examples/) — Provider-specific setup guides
- [docs/customizing.md](docs/customizing.md) — Customization guide

## Releasing

All components use [semver](https://semver.org). Cut a release by pushing a tag:

```bash
# CoderyCI (bump Cargo.toml version first)
git tag codery-ci-v0.1.0
git push origin codery-ci-v0.1.0

# Sandbox
git tag sandbox-v0.1.0
git push origin sandbox-v0.1.0

# Apps
git tag apps-v0.1.0
git push origin apps-v0.1.0
```

CI builds and publishes artifacts automatically. See [CLAUDE.md](CLAUDE.md) for full release instructions.
