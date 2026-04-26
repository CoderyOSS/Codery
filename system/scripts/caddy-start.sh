#!/bin/bash
# caddy-start.sh — Wait for Tailscale IP from dns-update, then start Caddy
set -e

IP_FILE="/run/tailscale.ip"

echo "[caddy-start] Waiting for Tailscale IP in $IP_FILE..."
for i in $(seq 1 120); do
  if [ -f "$IP_FILE" ] && [ -s "$IP_FILE" ]; then
    break
  fi
  sleep 1
done

if [ ! -f "$IP_FILE" ] || [ ! -s "$IP_FILE" ]; then
  echo "[caddy-start] ERROR: $IP_FILE not available after 120s — Tailscale may not be authenticated"
  exit 1
fi

export TAILSCALE_IP
TAILSCALE_IP=$(cat "$IP_FILE")
echo "[caddy-start] Binding Caddy to Tailscale IP: $TAILSCALE_IP"

# Load env vars from .env so Caddy can resolve {$CLOUDFLARE_API_TOKEN} etc.
if [ -f /opt/codery/.env ]; then
  set -a
  source /opt/codery/.env
  set +a
fi

exec caddy run --config /etc/caddy/Caddyfile --adapter caddyfile
