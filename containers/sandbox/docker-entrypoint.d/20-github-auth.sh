#!/bin/bash
set -e

# This script runs as root (so it can read the root-owned PEM).
# gh credentials and git config are stored under the gem user via `su`.

if [ -z "${GITHUB_APP_ID:-}" ] || [ ! -f "${GITHUB_APP_PRIVATE_KEY_PATH:-}" ]; then
  echo "[sandbox] GitHub App credentials not configured, skipping"
  exit 0
fi

GH_TOKEN=$(github-app-token 2>/dev/null) || true
if [ -z "$GH_TOKEN" ] || [ "$GH_TOKEN" = "null" ]; then
  echo "[sandbox] Warning: GitHub App token generation failed"
  exit 0
fi

APP_SLUG="${GITHUB_APP_SLUG:-}"

if echo "$GH_TOKEN" | su -s /bin/bash gem -c "gh auth login --with-token" 2>/dev/null; then
  echo "[sandbox] GitHub App authenticated as ${APP_SLUG}[bot]"
  su -s /bin/bash gem -c "
    git config --global user.name '${APP_SLUG}[bot]'
    git config --global user.email '${GITHUB_APP_ID}+${APP_SLUG}[bot]@users.noreply.github.com'
  "
  echo "[sandbox] Git identity: ${APP_SLUG}[bot]"
else
  echo "[sandbox] Warning: gh auth login failed"
fi
