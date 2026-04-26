#!/bin/bash
set -e
echo "[system] Starting system container"
for script in /docker-entrypoint.d/*.sh; do
  if [ -x "$script" ]; then
    echo "[system] Running: $script"
    "$script"
  fi
done
mkdir -p /var/log/supervisor /etc/supervisor/projects.d
exec /usr/bin/supervisord -c /etc/supervisor/supervisord.conf
