#!/bin/bash
# Generate a GitHub App installation access token.
# Usage: github-app-token [installation_id]
# If installation_id is omitted, uses the first installation found.
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

# Resolve installation ID
if [ -n "${1:-}" ]; then
  INSTALLATION_ID="$1"
else
  INSTALLATION_ID=$(curl -sf \
    -H "Authorization: Bearer ${JWT}" \
    -H "Accept: application/vnd.github+json" \
    -H "X-GitHub-Api-Version: 2022-11-28" \
    "https://api.github.com/app/installations" | jq -r '.[0].id')
fi

if [ -z "$INSTALLATION_ID" ] || [ "$INSTALLATION_ID" = "null" ]; then
  echo "Error: no installation found. Install the app at github.com/settings/apps/${GITHUB_APP_SLUG:-}/installations" >&2
  exit 1
fi

# Exchange JWT for a short-lived installation access token (~1h)
curl -sf -X POST \
  -H "Authorization: Bearer ${JWT}" \
  -H "Accept: application/vnd.github+json" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  "https://api.github.com/app/installations/${INSTALLATION_ID}/access_tokens" \
  | jq -r '.token'
