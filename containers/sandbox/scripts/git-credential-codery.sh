#!/bin/bash
# git credential helper — GitHub App auth for HTTPS git operations.
# Registered in ~/.gitconfig by 20-github-auth.sh:
#   credential.https://github.com.helper = /usr/local/bin/git-credential-codery
# Makes git pull / fetch / clone / push work without prompts.
# Needs GITHUB_APP_ID + GITHUB_APP_PRIVATE_KEY_PATH in env (sshd SetEnv
# passthrough covers SSH sessions; launchy children inherit container env).
set -euo pipefail

# Read the credential request (protocol=, host=, ...) — we only serve github.com.
while IFS='=' read -r key value; do
    [ "$key" = "host" ] && HOST="$value"
    [ "$key" = "protocol" ] && PROTOCOL="$value"
done

if [ "${HOST:-}" != "github.com" ] || [ "${PROTOCOL:-}" != "https" ]; then
    exit 1
fi

TOKEN=$(github-app-token)
[ -n "$TOKEN" ] && [ "$TOKEN" != "null" ] || exit 1

echo "username=x-access-token"
echo "password=${TOKEN}"
