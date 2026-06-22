#!/usr/bin/env bash
# Prune oversized stale opencode session_diff files.
#
# opencode computes a per-session git diff snapshot and stores it at
# ~/.local/share/opencode/storage/session_diff/<session>.json. When the
# working tree has many untracked/generated files (e.g. graphify-out/),
# these files can balloon to tens of MB. Serving a 28MB JSON over SSE to
# a mobile browser wedges the opencode web server.
#
# This loop deletes any session_diff file larger than SIZE_THRESHOLD_BYTES
# that hasn't been touched in STALE_MINUTES. Active sessions keep their
# diffs; only abandoned bloat is pruned.

set -u

DIFF_DIR="${HOME}/.local/share/opencode/storage/session_diff"
SIZE_THRESHOLD_BYTES="${PRUNE_DIFF_BYTES:-5242880}"   # 5 MiB default
STALE_MINUTES="${PRUNE_DIFF_STALE_MINS:-60}"
INTERVAL_SECS="${PRUNE_DIFF_INTERVAL_SECS:-3600}"     # 1h default

if [[ ! -d "${DIFF_DIR}" ]]; then
  echo "prune-opencode-diffs: ${DIFF_DIR} missing, sleeping"
fi

while true; do
  if [[ -d "${DIFF_DIR}" ]]; then
    while IFS= read -r -d '' f; do
      echo "prune-opencode-diffs: removing $(du -h "${f}" | cut -f1) ${f}"
      rm -f "${f}"
    done < <(find "${DIFF_DIR}" -type f -name '*.json' \
              -size "+${SIZE_THRESHOLD_BYTES}c" \
              -mmin "+${STALE_MINUTES}" \
              -print0)
  fi
  sleep "${INTERVAL_SECS}"
done
