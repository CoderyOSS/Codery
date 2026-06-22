#!/usr/bin/env bash
# Watch opencode serve RSS. SIGTERM when it exceeds threshold so launchy
# respawns a fresh process. opencode persists sessions to disk, so a kill
# only drops in-flight LLM streams — history survives.
#
# Tunables (env):
#   OPENCORE_SERVE_KILL_KB    RSS threshold in KB     (default 1500000  ≈ 1.5 GiB)
#   OPENCORE_SERVE_CHECK_SECS poll interval           (default 60)

set -u

THRESHOLD_KB="${OPENCORE_SERVE_KILL_KB:-1500000}"
INTERVAL_SECS="${OPENCORE_SERVE_CHECK_SECS:-60}"

last_kill=0

while true; do
  pid=$(pgrep -f "^opencode serve" | head -1)
  if [[ -n "${pid}" ]]; then
    rss=$(ps -o rss= -p "${pid}" 2>/dev/null | tr -d ' ')
    if [[ -n "${rss}" && "${rss}" -gt "${THRESHOLD_KB}" ]]; then
      now=$(date +%s)
      # Rate-limit: don't kill more than once per 5 min
      if (( now - last_kill > 300 )); then
        mb=$(( rss / 1024 ))
        thresh_mb=$(( THRESHOLD_KB / 1024 ))
        echo "$(date -Iseconds) opencode-serve-guard: pid ${pid} RSS ${mb} MiB > ${thresh_mb} MiB threshold — SIGTERM"
        kill -TERM "${pid}"
        last_kill="${now}"
      fi
    fi
  fi
  sleep "${INTERVAL_SECS}"
done
