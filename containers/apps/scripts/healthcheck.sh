#!/bin/bash
# healthcheck.sh — Exits 0 only if the apps container is correctly configured.
set -e

fail() {
  echo "HEALTHCHECK FAILED: $1" >&2
  exit 1
}

# Required tools
bun --version > /dev/null 2>&1    || fail "bun not found"
node --version > /dev/null 2>&1   || fail "node not found"
git --version > /dev/null 2>&1    || fail "git not found"
python3 --version > /dev/null 2>&1 || fail "python3 not found"

# Required env vars
[ -n "${GITHUB_APP_ID}" ]   || fail "GITHUB_APP_ID not set"
[ -n "${GITHUB_APP_SLUG}" ] || fail "GITHUB_APP_SLUG not set"

# GitHub App PEM file
PEM="${GITHUB_APP_PRIVATE_KEY_PATH:-/run/secrets/github-app.pem}"
[ -f "$PEM" ]        || fail "PEM file not found: $PEM"
[ -s "$PEM" ]        || fail "PEM file is empty: $PEM"

# Projects volume
[ -d "/home/gem/projects" ]      || fail "/home/gem/projects not mounted"
[ -w "/home/gem/projects" ]      || fail "/home/gem/projects not writable"

echo "HEALTHCHECK OK"
exit 0
