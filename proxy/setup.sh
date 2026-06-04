#!/bin/bash
# proxy/setup.sh — Ensure Caddy, Tailscale, and supervisor configs are installed on the host
# Run as root. Idempotent — safe to run multiple times.
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
echo "[proxy-setup] Script directory: $SCRIPT_DIR"

# --- Install scripts ---
for script in caddy-start.sh tailscale-up.sh dns-update.sh; do
  SRC="$SCRIPT_DIR/scripts/$script"
  DST="/usr/local/bin/$script"
  if ! cmp -s "$SRC" "$DST" 2>/dev/null; then
    echo "[proxy-setup] Installing $script -> $DST"
    cp "$SRC" "$DST"
    chmod +x "$DST"
  else
    echo "[proxy-setup] $DST already up to date"
  fi
done

# --- Install supervisor configs ---
for conf in "$SCRIPT_DIR"/supervisor/conf.d/*.conf; do
  NAME=$(basename "$conf")
  DST="/etc/supervisor/conf.d/$NAME"
  if ! cmp -s "$conf" "$DST" 2>/dev/null; then
    echo "[proxy-setup] Installing supervisor config: $NAME"
    cp "$conf" "$DST"
  else
    echo "[proxy-setup] Supervisor config $NAME already up to date"
  fi
done

# --- Install Caddyfile (only if no custom one exists) ---
CADDYFILE="/etc/caddy/Caddyfile"
DEFAULT="$SCRIPT_DIR/Caddyfile.default"
mkdir -p /etc/caddy
if [ ! -f "$CADDYFILE" ]; then
  echo "[proxy-setup] No Caddyfile found — installing default"
  cp "$DEFAULT" "$CADDYFILE"
else
  echo "[proxy-setup] Caddyfile already exists at $CADDYFILE — leaving unchanged"
  echo "[proxy-setup] To update routes, edit $CADDYFILE manually or delete it and re-run"
fi

# --- Ensure Tailscale state directory ---
mkdir -p /var/lib/tailscale

# --- Ensure supervisor.service loads /opt/codery/.env ---
# Without this drop-in, supervisord starts without CLOUDFLARE_API_TOKEN /
# CLOUDFLARE_ZONE_ID in its environment, causing %(ENV_...)s interpolation in
# caddy.conf and dns-update.conf to fail with INVALIDARGUMENT, which prevents
# supervisord from starting at all.
DROP_IN_DIR="/etc/systemd/system/supervisor.service.d"
DROP_IN_FILE="$DROP_IN_DIR/env.conf"
DROP_IN_WANT="[Service]
EnvironmentFile=/opt/codery/.env"
if [ ! -f "$DROP_IN_FILE" ] || [ "$(cat "$DROP_IN_FILE")" != "$DROP_IN_WANT" ]; then
  echo "[proxy-setup] Installing systemd drop-in: $DROP_IN_FILE"
  mkdir -p "$DROP_IN_DIR"
  printf '%s\n' "$DROP_IN_WANT" > "$DROP_IN_FILE"
  systemctl daemon-reload
else
  echo "[proxy-setup] Systemd drop-in already up to date"
fi

# --- Ensure SSH authorized_keys for sandbox container ---
# The sandbox container bind-mounts this file read-only at /run/secrets/authorized_keys.
# sshd inside the container reads it directly. An empty file means SSH pubkey auth
# will always fail with "authFailed" — there is no other way to add keys.
#
# ACTION REQUIRED on a fresh VPS:
#   echo "ssh-ed25519 AAAA..." >> /opt/codery/ssh/authorized_keys
#
SSH_DIR=/opt/codery/ssh
if [ ! -f "$SSH_DIR/authorized_keys" ]; then
  echo "[proxy-setup] Creating $SSH_DIR/authorized_keys (empty)"
  mkdir -p "$SSH_DIR"
  touch "$SSH_DIR/authorized_keys"
  chmod 600 "$SSH_DIR/authorized_keys"
  chown root:root "$SSH_DIR/authorized_keys"
  echo ""
  echo "  *** ACTION REQUIRED ***"
  echo "  $SSH_DIR/authorized_keys is empty."
  echo "  SSH into the sandbox will fail until you add your public key:"
  echo "    echo 'ssh-ed25519 AAAA...' >> $SSH_DIR/authorized_keys"
  echo ""
else
  if [ ! -s "$SSH_DIR/authorized_keys" ]; then
    echo ""
    echo "  *** WARNING ***"
    echo "  $SSH_DIR/authorized_keys exists but is empty."
    echo "  SSH into the sandbox will fail until you add your public key:"
    echo "    echo 'ssh-ed25519 AAAA...' >> $SSH_DIR/authorized_keys"
    echo ""
  else
    echo "[proxy-setup] $SSH_DIR/authorized_keys already exists and has keys"
  fi
fi

# --- Ensure supervisor.service is enabled and running ---
echo "[proxy-setup] Ensuring supervisor.service is enabled..."
systemctl enable supervisor 2>/dev/null || true

if systemctl is-active --quiet supervisor; then
  echo "[proxy-setup] Supervisord active — reloading config..."
  supervisorctl -c /etc/supervisor/supervisord.conf reread 2>/dev/null || true
  supervisorctl -c /etc/supervisor/supervisord.conf update 2>/dev/null || true
else
  echo "[proxy-setup] Starting supervisor.service..."
  systemctl restart supervisor
  sleep 2
fi

echo "[proxy-setup] Proxy service status:"
supervisorctl -c /etc/supervisor/supervisord.conf status 2>/dev/null || \
  systemctl status supervisor --no-pager -l | tail -15

echo "[proxy-setup] Done. Proxy services are managed by host supervisord."
