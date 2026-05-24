#!/bin/bash
set -e
echo "[apps] Starting apps container"
for script in /docker-entrypoint.d/*.sh; do
  if [ -x "$script" ]; then
    echo "[apps] Running: $script"
    "$script"
  fi
done
echo "[apps] Starting Launchy process manager"
exec /sbin/launchy /etc/launchy/config.json
