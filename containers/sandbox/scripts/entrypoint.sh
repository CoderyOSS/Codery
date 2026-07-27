#!/bin/bash
echo "[sandbox] Starting sandbox container"
for script in /docker-entrypoint.d/*.sh; do
  if [ -x "$script" ]; then
    echo "[sandbox] Running: $script"
    "$script" || echo "[sandbox] WARNING: $script exited with $? — continuing"
  fi
done
# BuildKit COPY does not reliably preserve the sticky bit on /tmp.
chmod 1777 /tmp
echo "[sandbox] Handing off to launchy"
exec /sbin/launchy /etc/launchy.json
