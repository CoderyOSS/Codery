#!/usr/bin/env bash
# Watch opencode serve health. Two failure modes, both fixed by SIGTERM (the
# supervisor respawns a fresh process; opencode persists sessions to disk, so
# a kill only drops in-flight LLM streams — history survives):
#
# 1. Memory bloat: RSS above threshold → SIGTERM.
# 2. Wedged serve: process alive but port 3000 refuses connections for longer
#    than the liveness grace (seen in the Sep 2026 incident — a serve hung at
#    startup, invisible to any watchdog) → SIGTERM.
#
# Tunables (env):
#   OPENCORE_SERVE_KILL_KB       RSS threshold in KB   (default 1500000 ≈ 1.5 GiB)
#   OPENCORE_SERVE_CHECK_SECS    poll interval         (default 60)
#   OPENCORE_SERVE_LIVENESS_SECS how long the port may stay un-connectable
#                                while the process exists (default 600)

set -u

THRESHOLD_KB="${OPENCORE_SERVE_KILL_KB:-1500000}"
INTERVAL_SECS="${OPENCORE_SERVE_CHECK_SECS:-60}"
LIVENESS_SECS="${OPENCORE_SERVE_LIVENESS_SECS:-600}"

last_kill=0
last_alive=$(date +%s)

port_alive() {
  # Bash built-in TCP connect — no external deps. Closes immediately.
  (exec 3<>"/dev/tcp/127.0.0.1/3000") 2>/dev/null || return 1
  exec 3>&- 3<&- 2>/dev/null
  return 0
}

while true; do
  pid=$(pgrep -f "^opencode serve" | head -1)
  now=$(date +%s)
  if [[ -z "${pid}" ]]; then
    # No serve process — supervisor owns (re)starting it; keep liveness
    # baseline fresh so a slow boot isn't misjudged as a wedge.
    last_alive=${now}
  else
    if port_alive; then
      last_alive=${now}
    else
      stale=$(( now - last_alive ))
      if (( stale > LIVENESS_SECS )); then
        echo "$(date -Iseconds) opencode-serve-guard: pid ${pid} alive but port 3000 unreachable for ${stale}s > ${LIVENESS_SECS}s — SIGTERM"
        kill -TERM "${pid}" 2>/dev/null
        last_alive=${now}
        last_kill=${now}
        sleep "${INTERVAL_SECS}"
        continue
      fi
    fi

    rss=$(ps -o rss= -p "${pid}" 2>/dev/null | tr -d ' ')
    if [[ -n "${rss}" && "${rss}" -gt "${THRESHOLD_KB}" ]]; then
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
