#!/bin/bash
# tailscale-up.sh — Authenticate with Tailscale (auth only — Caddy handles HTTPS routing)
# Called by supervisord as a one-shot after tailscaled starts

set -e

TAILSCALE_HOSTNAME="${DOMAIN_NAME%%.*}"
SOCKET=/var/run/tailscale/tailscaled.sock

echo "[tailscale-up] Waiting for tailscaled socket..."
for i in $(seq 1 60); do
  if [ -S "$SOCKET" ]; then
    break
  fi
  sleep 0.5
done

if [ ! -S "$SOCKET" ]; then
  echo "[tailscale-up] ERROR: tailscaled socket not found after 30s"
  exit 1
fi

echo "[tailscale-up] tailscaled is ready"

BACKEND_STATE=$(tailscale status --json 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('BackendState',''))" 2>/dev/null || echo "")

if [ "$BACKEND_STATE" = "Running" ]; then
  echo "[tailscale-up] Already authenticated — reconnecting with existing identity"
  tailscale up --accept-dns=false --hostname="${TAILSCALE_HOSTNAME:-codery}" --ssh
elif [ -z "${TAILSCALE_AUTH_KEY:-}" ]; then
  echo "[tailscale-up] WARNING: TAILSCALE_AUTH_KEY not set, skipping authentication"
  exit 0
else
  echo "[tailscale-up] Authenticating for the first time..."
  tailscale up \
    --authkey="${TAILSCALE_AUTH_KEY}" \
    --hostname="${TAILSCALE_HOSTNAME:-codery}" \
    --accept-dns=false \
    --ssh
fi

echo "[tailscale-up] Connected to tailnet as ${TAILSCALE_HOSTNAME:-codery}"

# Clear any persisted tailscale serve config — Caddy owns ports 80/443
echo "[tailscale-up] Clearing tailscale serve config (Caddy handles routing)"
tailscale serve reset 2>/dev/null || true

echo "[tailscale-up] HTTPS routing is handled by Caddy — see /etc/caddy/Caddyfile