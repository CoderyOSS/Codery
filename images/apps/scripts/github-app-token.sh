#!/usr/bin/env bash
set -e

# github-app-token.sh — Generate a GitHub App installation token
# Usage: source github-app-token && echo $GH_TOKEN

if [ -z "${GITHUB_APP_ID:-}" ] || [ -z "${GITHUB_APP_PRIVATE_KEY_PATH:-}" ]; then
  echo "ERROR: GITHUB_APP_ID and GITHUB_APP_PRIVATE_KEY_PATH must be set" >&2
  exit 1
fi

if [ ! -f "${GITHUB_APP_PRIVATE_KEY_PATH}" ]; then
  echo "ERROR: PEM file not found at $GITHUB_APP_PRIVATE_KEY_PATH" >&2
  exit 1
fi

JWT=$(python3 -c "
import jwt, time, os
app_id = os.environ['GITHUB_APP_ID']
with open(os.environ['GITHUB_APP_PRIVATE_KEY_PATH']) as f:
    pem = f.read()
print(jwt.encode({'iat': int(time.time()), 'exp': int(time.time()) + 600, 'iss': app_id}, pem, algorithm='RS256'))
")

INSTALL_ID=$(curl -sf \
  -H "Authorization: Bearer $JWT" \
  -H "Accept: application/vnd.github+json" \
  https://api.github.com/app/installations | jq '.[0].id')

if [ -z "$INSTALL_ID" ] || [ "$INSTALL_ID" = "null" ]; then
  echo "ERROR: Could not retrieve installation ID" >&2
  exit 1
fi

TOKEN=$(curl -sf -X POST \
  -H "Authorization: Bearer $JWT" \
  -H "Accept: application/vnd.github+json" \
  "https://api.github.com/app/installations/$INSTALL_ID/access_tokens" \
  | jq -r '.token')

if [ -z "$TOKEN" ] || [ "$TOKEN" = "null" ]; then
  echo "ERROR: Could not generate access token" >&2
  exit 1
fi

echo "$TOKEN"
