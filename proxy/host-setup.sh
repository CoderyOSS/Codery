#!/bin/bash
# proxy/host-setup.sh — One-time host setup: install caddy (from codery-system),
# tailscale, and supervisor on the host. Safe to re-run.
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "[host-setup] ERROR: must be run as root (use sudo)" >&2
  exit 1
fi

echo "[host-setup] Installing packages..."
apt-get update -qq
apt-get install -y --no-install-recommends supervisor mosh

echo "[host-setup] Installing Tailscale..."
if ! command -v tailscale &>/dev/null; then
  . /etc/os-release
  curl -fsSL "https://pkgs.tailscale.com/stable/${ID}/${VERSION_CODENAME}.noarmor.gpg" \
    | dd of=/usr/share/keyrings/tailscale-archive-keyring.gpg 2>/dev/null
  echo "deb [signed-by=/usr/share/keyrings/tailscale-archive-keyring.gpg] \
    https://pkgs.tailscale.com/stable/${ID} ${VERSION_CODENAME} main" \
    > /etc/apt/sources.list.d/tailscale.list
  apt-get update -qq
  apt-get install -y --no-install-recommends tailscale
  echo "[host-setup] Tailscale installed"
else
  echo "[host-setup] Tailscale already installed"
fi

echo "[host-setup] Copying Caddy from codery-system container..."
if [ ! -x /usr/local/bin/caddy ]; then
  if docker ps --format '{{.Names}}' | grep -q '^codery-system$'; then
    docker cp codery-system:/usr/local/bin/caddy /usr/local/bin/caddy
    chmod +x /usr/local/bin/caddy
    echo "[host-setup] Caddy copied"
  else
    echo "[host-setup] ERROR: /usr/local/bin/caddy not found and codery-system container not running" >&2
    exit 1
  fi
else
  echo "[host-setup] Caddy already installed at /usr/local/bin/caddy"
fi

echo "[host-setup] Setting up /etc/caddy..."
mkdir -p /etc/caddy
chown deploy:deploy /etc/caddy
# Ensure existing Caddyfile (if any) is also deploy-owned so the orchestrator can write it
[ -f /etc/caddy/Caddyfile ] && chown deploy:deploy /etc/caddy/Caddyfile

echo "[host-setup] Setting up /var/lib/tailscale state dir..."
mkdir -p /var/lib/tailscale

echo "[host-setup] Opening Mosh UDP ports (60000-61000)..."
ufw allow 60000:61000/udp

echo "[host-setup] Enabling Tailscale SSH (keyless SSH via Tailscale identity)..."
tailscale up --ssh 2>/dev/null || echo "[host-setup] NOTE: tailscale up --ssh failed (may not be authenticated yet — tailscale-up.sh will enable it on next supervisord start)"

echo "[host-setup] Configuring supervisord to allow deploy user control..."
# Patch the default supervisord.conf so deploy group can use supervisorctl without sudo.
# Uses a single sed pass: delete the existing chmod=0700 line within the [unix_http_server]
# block, then append chmod=0770 and chown=root:deploy after the section header.
if ! grep -q 'chown=root:deploy' /etc/supervisor/supervisord.conf; then
  sed -i \
    -e '/^\[unix_http_server\]/a chmod=0770\nchown=root:deploy' \
    -e '/^\[unix_http_server\]/,/^\[/{/^chmod=0700/d}' \
    /etc/supervisor/supervisord.conf
fi

echo "[host-setup] Done. Run proxy/setup.sh next to install service configs."
