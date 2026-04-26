#!/bin/bash
set -e
echo "[apps] Starting apps container"
for script in /docker-entrypoint.d/*.sh; do
  if [ -x "$script" ]; then
    echo "[apps] Running: $script"
    "$script"
  fi
done
exec /usr/bin/supervisord -c /etc/supervisor/supervisord.conf
