#!/bin/bash
set -e

# This script runs as root (so it can read the root-owned PEM).
# gh credentials and git config are stored under the gem user via `sudo -u`.

APP_SLUG="${GITHUB_APP_SLUG:-}"

# Git identity + credential helper need no token — register unconditionally
# so git pull/push over HTTPS work even if gh auth fails at boot.
if [ -n "${GITHUB_APP_ID:-}" ] && [ -n "$APP_SLUG" ]; then
  sudo -Hu gem /bin/bash -c "
    git config --global user.name '${APP_SLUG}[bot]'
    git config --global user.email '${GITHUB_APP_ID}+${APP_SLUG}[bot]@users.noreply.github.com'
    git config --global credential.https://github.com.helper /usr/local/bin/git-credential-codery
  "
  echo "[sandbox] Git identity: ${APP_SLUG}[bot]; credential helper registered"
fi

if [ -z "${GITHUB_APP_ID:-}" ] || [ ! -f "${GITHUB_APP_PRIVATE_KEY_PATH:-}" ]; then
  echo "[sandbox] GitHub App credentials not configured, skipping gh auth"
  exit 0
fi

# Token generation can race container networking at boot — retry.
# The App may be installed on multiple orgs; tokens are per-installation,
# so pass an explicit owner (GITHUB_APP_DEFAULT_OWNER from .env, or the
# first single-installation fallback).
GH_TOKEN=""
OWNER="${GITHUB_APP_DEFAULT_OWNER:-}"
if [ -z "$OWNER" ]; then
    INSTALLS=$(github-app-token --list 2>/dev/null | awk '{print $1}' | grep -v '^$' || true)
    N=$(printf '%s\n' "$INSTALLS" | grep -c . || true)
    [ "$N" = "1" ] && OWNER=$(github-app-token --list 2>/dev/null | awk 'NR==1 {print $2}')
fi
TOKEN_ARGS=()
[ -n "$OWNER" ] && TOKEN_ARGS=("" "$OWNER")

for _ in 1 2 3 4 5; do
    GH_TOKEN=$(github-app-token "${TOKEN_ARGS[@]}" 2>/dev/null || true)
    if [ -n "$GH_TOKEN" ] && [ "$GH_TOKEN" != "null" ]; then break; fi
    sleep 3
done

if [ -z "$GH_TOKEN" ] || [ "$GH_TOKEN" = "null" ]; then
  echo "[sandbox] Warning: GitHub App token generation failed after retries"
  echo "[sandbox]   (multiple org installations? set GITHUB_APP_DEFAULT_OWNER in /opt/codery/.env)"
  exit 0
fi

if echo "$GH_TOKEN" | sudo -Hu gem /bin/bash -c "gh auth login --with-token" 2>/dev/null; then
  echo "[sandbox] GitHub App authenticated as ${APP_SLUG}[bot]"
else
  echo "[sandbox] Warning: gh auth login failed"
fi
