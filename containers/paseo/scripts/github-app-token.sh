#!/bin/bash
# Generate a GitHub App installation access token.
# Usage:
#   github-app-token                        # auto (only works with exactly 1 installation)
#   github-app-token <installation_id>      # explicit ID — most reliable
#   github-app-token "" <account_login>     # match by org/user login (case-insensitive)
#   github-app-token --list                 # list all installations (id + account)
#
# Required env vars:
#   GITHUB_APP_ID                - numeric App ID
#   GITHUB_APP_PRIVATE_KEY_PATH  - path to the .pem private key file

set -euo pipefail

APP_ID="${GITHUB_APP_ID:?GITHUB_APP_ID not set}"
PEM_FILE="${GITHUB_APP_PRIVATE_KEY_PATH:?GITHUB_APP_PRIVATE_KEY_PATH not set}"

if [ ! -f "$PEM_FILE" ]; then
  echo "Error: private key not found at $PEM_FILE" >&2
  exit 1
fi

# Build a JWT signed with RS256
now=$(date +%s)
iat=$((now - 60))   # allow 60s clock skew
exp=$((now + 540))  # 9 min (GitHub max is 10)

b64url() { base64 | tr -d '=' | tr '/+' '_-' | tr -d '\n'; }

header=$(printf '{"alg":"RS256","typ":"JWT"}' | b64url)
payload=$(printf '{"iat":%d,"exp":%d,"iss":"%s"}' "$iat" "$exp" "$APP_ID" | b64url)
sig=$(printf '%s.%s' "$header" "$payload" | openssl dgst -sha256 -sign "$PEM_FILE" | b64url)

JWT="${header}.${payload}.${sig}"

INSTALLATIONS=$(curl -sSf \
  -H "Authorization: Bearer ${JWT}" \
  -H "Accept: application/vnd.github+json" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  "https://api.github.com/app/installations")

list_installations() {
  echo "$INSTALLATIONS" | jq -r '.[] | "  \(.id)\t\(.account.login) (\(.account.type))"'
}

if [ "${1:-}" = "--list" ]; then
  echo "GitHub App installations:" >&2
  list_installations >&2
  exit 0
fi

if [ -n "${1:-}" ]; then
  INSTALLATION_ID="$1"
elif [ -n "${2:-}" ]; then
  # Case-insensitive account login match
  INSTALLATION_ID=$(echo "$INSTALLATIONS" | jq -r --arg acct "$2" \
    '.[] | select(.account.login | ascii_downcase == ($acct | ascii_downcase)) | .id')
else
  COUNT=$(echo "$INSTALLATIONS" | jq 'length')
  if [ "$COUNT" = "1" ]; then
    INSTALLATION_ID=$(echo "$INSTALLATIONS" | jq -r '.[0].id')
  else
    echo "Error: multiple installations found — refusing to guess." >&2
    echo "Available installations:" >&2
    list_installations >&2
    echo "" >&2
    echo "Re-run with an explicit ID:  github-app-token <installation_id>" >&2
    echo "Or by account login:         github-app-token '' <account_login>" >&2
    exit 1
  fi
fi

if [ -z "$INSTALLATION_ID" ] || [ "$INSTALLATION_ID" = "null" ]; then
  echo "Error: no installation found for account '${2:-}'." >&2
  echo "Available installations:" >&2
  list_installations >&2
  echo "" >&2
  echo "Re-run with an explicit ID:  github-app-token <installation_id>" >&2
  echo "Or check the login spelling (matching is case-insensitive but must be exact otherwise)." >&2
  exit 1
fi

# Exchange JWT for a short-lived installation access token (~1h)
curl -sSf -X POST \
  -H "Authorization: Bearer ${JWT}" \
  -H "Accept: application/vnd.github+json" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  "https://api.github.com/app/installations/${INSTALLATION_ID}/access_tokens" \
  | jq -r '.token'
