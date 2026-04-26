#!/bin/bash
set -e

if [ -z "${OPENROUTER_KEY:-}" ]; then
  echo "[sandbox] OPENROUTER_KEY not set, skipping OpenRouter auth setup"
  exit 0
fi

AUTH_JSON="/home/gem/.local/share/opencode/auth.json"
mkdir -p "$(dirname "$AUTH_JSON")"

# Write the openrouter key into auth.json, preserving any other credentials.
if [ -f "$AUTH_JSON" ] && command -v jq &>/dev/null; then
  jq --arg key "${OPENROUTER_KEY}" \
    '. + {"openrouter": {"type": "api", "key": $key}}' \
    "$AUTH_JSON" > /tmp/auth-new.json && mv /tmp/auth-new.json "$AUTH_JSON"
else
  printf '{"openrouter":{"type":"api","key":"%s"}}\n' "${OPENROUTER_KEY}" > "$AUTH_JSON"
fi

chown 1000:1000 "$AUTH_JSON"
chmod 600 "$AUTH_JSON"
echo "[sandbox] OpenRouter API key written to auth.json"
