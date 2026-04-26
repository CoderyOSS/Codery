#!/bin/bash
set -e
mkdir -p /var/lib/tailscale
if [ -z "${TAILSCALE_AUTH_KEY:-}" ]; then
  echo "[system] WARNING: TAILSCALE_AUTH_KEY not set"
else
  echo "[system] TAILSCALE_AUTH_KEY is set"
fi
