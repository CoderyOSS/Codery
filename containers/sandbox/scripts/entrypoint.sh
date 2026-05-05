#!/bin/bash
set -e
echo "[sandbox] Starting sandbox container"
for script in /docker-entrypoint.d/*.sh; do
  if [ -x "$script" ]; then
    echo "[sandbox] Running: $script"
    "$script"
  fi
done
exec /sbin/launchy /etc/launchy.json
