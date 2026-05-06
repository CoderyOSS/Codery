#!/usr/bin/env bash
set -e

BRANCH="${1:-$(git branch --show-current 2>/dev/null || echo main)}"

REMOTE_URL=$(git remote get-url origin 2>/dev/null)
if [ -z "$REMOTE_URL" ]; then
  echo "Error: No git remote 'origin' found" >&2
  exit 1
fi

REPO=$(echo "$REMOTE_URL" | sed -E 's|https://(x-access-token:[^@]+@)?github.com/||; s|git@github.com:||; s|\.git$||')
if [ -z "$REPO" ]; then
  echo "Error: Could not parse repo from remote URL: $REMOTE_URL" >&2
  exit 1
fi

REPO_OWNER=$(echo "$REPO" | cut -d'/' -f1)

TOKEN=$(github-app-token "" "$REPO_OWNER")

if [ -z "$TOKEN" ] || [ "$TOKEN" = "null" ]; then
  echo "Error: Could not generate GitHub App token" >&2
  exit 1
fi

git remote set-url origin "https://x-access-token:${TOKEN}@github.com/${REPO}.git"
git push origin "$BRANCH"
git remote set-url origin "https://github.com/${REPO}.git"
