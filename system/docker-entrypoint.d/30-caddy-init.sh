#!/bin/bash
set -e
CADDYFILE="/etc/caddy/Caddyfile"
DEFAULT="/etc/caddy-default/Caddyfile"
mkdir -p /etc/caddy
if [ ! -f "$CADDYFILE" ]; then
  echo "[system] No Caddyfile found — copying default"
  cp "$DEFAULT" "$CADDYFILE"
else
  echo "[system] Caddyfile already exists — leaving unchanged"
fi
