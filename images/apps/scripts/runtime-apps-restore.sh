#!/bin/bash
# runtime-apps-restore — link orchestrator-managed runtime apps into s6
# supervision after container (re)start or blue/green redeploy.
#
# Bundles live in /etc/s6-overlay/apps.d (host /opt/codery/apps-s6.d), rendered
# by codery-ci's sync_s6 from SQLite. /run/service is ephemeral, so links are
# recreated here at every boot. One bad bundle must not block the others.
shopt -s nullglob
found=0
for d in /etc/s6-overlay/apps.d/*/; do
  found=1
  if /command/s6-svlink /run/service "$d"; then
    echo "[runtime-apps] linked ${d}"
  else
    echo "[runtime-apps] WARN: failed to link ${d}" >&2
  fi
done
[ "$found" = "0" ] && echo "[runtime-apps] no runtime apps to restore"
exit 0
