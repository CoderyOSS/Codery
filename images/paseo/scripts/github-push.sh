#!/usr/bin/env bash
# Push a branch or tag using GitHub App auth.
# Usage:
#   github-push                          # push current branch, repo from origin URL
#   github-push <branch-or-tag>          # push named ref, repo from origin URL
#   github-push <branch-or-tag> <org/repo>  # explicit repo
#
# This script NEVER modifies git remote configuration. It pushes to an
# explicit URL built from the target repo, leaving `origin` (and every other
# remote) exactly as found. Verification after a push: `git ls-remote
# https://github.com/<org>/<repo>.git <ref>` — always against the URL, never
# against a local remote.
#
# If push fails with a permissions/installation error, the token was likely
# generated for the wrong GitHub App installation. Do NOT keep retrying —
# run `github-app-token --list` to see installations, then re-run with the
# explicit <org/repo> argument.
set -e

die() { echo "Error: $*" >&2; exit 1; }

hint() {
  echo "" >&2
  echo "If this is an installation/permissions problem, the GitHub App token was" >&2
  echo "probably generated for the wrong installation. Do NOT guess repeatedly:" >&2
  echo "  1. Run:  github-app-token --list" >&2
  echo "  2. Identify the correct org/account for this repo" >&2
  echo "  3. Re-run: github-push ${BRANCH} <org>/<repo>" >&2
}

BRANCH="${1:-$(git branch --show-current 2>/dev/null || echo main)}"
EXPLICIT_REPO="${2:-${GITHUB_REPO:-}}"

if [ -n "$EXPLICIT_REPO" ]; then
  REPO="$EXPLICIT_REPO"
  echo "Using explicit repo: $REPO"
else
  REMOTE_URL=$(git remote get-url origin 2>/dev/null) || true
  [ -n "$REMOTE_URL" ] || die "No git remote 'origin' and no explicit <org/repo> given. Usage: github-push <branch> <org/repo>"
  REPO=$(echo "$REMOTE_URL" | sed -E 's|https://(x-access-token:[^@]+@)?github.com/||; s|git@github.com:||; s|\.git$||')
  [ -n "$REPO" ] || die "Could not parse repo from remote URL: $REMOTE_URL — pass it explicitly: github-push $BRANCH <org/repo>"
  echo "Using origin: $REPO"
fi

REPO_OWNER=$(echo "$REPO" | cut -d'/' -f1)

if ! TOKEN=$(github-app-token "" "$REPO_OWNER"); then
  echo "Error: Could not generate GitHub App token for owner '$REPO_OWNER'" >&2
  hint
  exit 1
fi
[ -n "$TOKEN" ] && [ "$TOKEN" != "null" ] || { echo "Error: empty token returned" >&2; hint; exit 1; }

PUSH_URL="https://x-access-token:${TOKEN}@github.com/${REPO}.git"
set +e
OUT=$(git push "$PUSH_URL" "$BRANCH" 2>&1)
RC=$?
set -e
# redact the credential from any git output before printing
echo "$OUT" | sed "s|x-access-token:[^@]*@|x-access-token:***@|g"

if [ $RC -ne 0 ]; then
  echo "Error: push failed for ${REPO} (ref ${BRANCH})" >&2
  hint
  exit $RC
fi
