#!/bin/bash
# git credential helper — GitHub App auth for HTTPS git operations.
# Registered in ~/.gitconfig by 20-github-auth.sh:
#   credential.https://github.com.helper = /usr/local/bin/git-credential-codery
# Makes git pull / fetch / clone / push work without prompts.
# Needs GITHUB_APP_ID + GITHUB_APP_PRIVATE_KEY_PATH in env (sshd SetEnv
# passthrough + ~/.bashrc exports cover SSH sessions; launchy children
# inherit container env).
#
# The GitHub App may be installed on multiple orgs — tokens are
# per-installation, so the owner must be resolved from the request
# (path=org/repo) or the origin remote in the cwd.
set -euo pipefail

HOST=
PROTOCOL=
PATH_=
while IFS='=' read -r key value; do
    case "$key" in
        host) HOST="$value" ;;
        protocol) PROTOCOL="$value" ;;
        path) PATH_="$value" ;;
    esac
done

if [ "${HOST:-}" != "github.com" ] || [ "${PROTOCOL:-}" != "https" ]; then
    exit 1
fi

# Owner = first path segment (org/repo), falling back to the origin remote
# in the cwd (credential helpers run from the repo directory).
OWNER=$(printf '%s' "${PATH_:-}" | cut -d/ -f1)
if [ -z "$OWNER" ] || [ "$OWNER" = "$PATH_" ]; then
    OWNER=$(git remote get-url origin 2>/dev/null \
        | sed -E 's|https://([^@]+@)?github.com/||; s|git@github.com:||; s|\.git$||' \
        | cut -d/ -f1)
fi
[ -n "$OWNER" ] || exit 1

TOKEN=$(github-app-token "" "$OWNER")
[ -n "$TOKEN" ] && [ "$TOKEN" != "null" ] || exit 1

echo "username=x-access-token"
echo "password=${TOKEN}"
