#!/usr/bin/env bash
# host/bootstrap.sh — CORE layer, step 1: turn a fresh VPS into the Codery host.
#
# Usage:  sudo ./bootstrap.sh /path/to/secrets.env
#
# Idempotent: safe to re-run; each step checks before acting and prints a
# GATE line on success. Full procedure: RUNBOOK.md Phase 1.
set -euo pipefail

SECRETS="${1:?usage: sudo ./bootstrap.sh <secrets.env>}"
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }
[ -f "$SECRETS" ] || { echo "secrets file not found: $SECRETS" >&2; exit 1; }
REPO="$(cd "$(dirname "$0")/.." && pwd)"          # the Codery repo checkout

# ---------------------------------------------------------------- 1. packages
if ! command -v docker >/dev/null; then
  curl -fsSL https://get.docker.com | sh
fi
apt-get update -qq
apt-get install -y -qq supervisor jq >/dev/null
echo "GATE 1.1 packages: docker $(docker --version | awk '{print $3}') / supervisor $(supervisord --version)"

# ------------------------------------------------------------- 2. sysctl bind
SYSCTL_CONF=/etc/sysctl.d/99-tailnet-binds.conf
grep -q ip_nonlocal_bind "$SYSCTL_CONF" 2>/dev/null || \
  echo 'net.ipv4.ip_nonlocal_bind=1' > "$SYSCTL_CONF"
sysctl -w net.ipv4.ip_nonlocal_bind=1 >/dev/null
echo "GATE 1.2 sysctl: tailnet-IP binds allowed before tailscale0 exists"

# ------------------------------------------------------------ 3. /opt/codery
for d in stacks projects data ssh state backups; do mkdir -p "/opt/codery/$d"; done
mkdir -p /opt/codery/data/{opencode,opendesign,dbgate}
# Tool-layer data + stack dirs are edited by uid 1000 (apps container user);
# everything else stays root-owned. Secrets are 0600.
chown -R 1000:1000 /opt/codery/stacks
chmod 600 /opt/codery/.env 2>/dev/null || true
echo "GATE 1.3 tree: /opt/codery/{stacks,projects,data,ssh,state,backups}"

# ------------------------------------------------------------------ 4. .env
if [ ! -f /opt/codery/.env ]; then
  install -m 600 "$SECRETS" /opt/codery/.env
  echo "GATE 1.4 .env: installed from $(basename "$SECRETS") (0600)"
else
  echo "GATE 1.4 .env: exists — left untouched (merge new keys manually)"
fi
# shellcheck disable=SC1091
set -a; . /opt/codery/.env; set +a
: "${TAILSCALE_AUTH_KEY:?missing in .env}" "${CLOUDFLARE_API_TOKEN:?missing in .env}"
: "${DOMAIN_NAME:?missing in .env}"

# ------------------------------------------------------- 5. host scripts+svc
install -m 755 "$REPO"/host/scripts/*.sh /usr/local/bin/
mkdir -p /etc/supervisor/conf.d
cp "$REPO"/host/supervisor/conf.d/*.conf /etc/supervisor/conf.d/
systemctl enable --now supervisor >/dev/null 2>&1 || supervisord
echo "GATE 1.5 host layer: scripts in /usr/local/bin, supervisor confs installed"

# ---------------------------------------------------------------- 6. tailscale
if ! command -v tailscale >/dev/null; then
  curl -fsSL https://tailscale.com/install.sh | sh
fi
systemctl enable --now tailscaled >/dev/null 2>&1
if ! tailscale status >/dev/null 2>&1; then
  tailscale up --authkey "$TAILSCALE_AUTH_KEY" --hostname codery
fi
TAILNET_IP=$(tailscale ip -4)
echo "$TAILNET_IP" > /run/tailscale.ip
echo "GATE 1.6 perimeter: tailscale up — $TAILNET_IP (recorded in /run/tailscale.ip)"

# ------------------------------------------------------------------ 7. ssh keys
[ -f /opt/codery/ssh/authorized_keys ] || {
  echo "NOTE: /opt/codery/ssh/authorized_keys missing — add the operator key +"
  echo "      agent-to-apps keypair now (see host/templates/env.example SSH_DIR)."
}
echo "BOOTSTRAP DONE — continue with RUNBOOK.md Phase 1.2 (Dockhand)."
