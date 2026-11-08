#!/bin/sh
# tools-replay — boot-time best-effort replay of the tools manifest into the
# shared nix "tools" profile.
#
# Normally a no-op: /nix is a named volume, so the profile (and its store
# paths) survive every redeploy. This matters only when the volume was lost
# or a manifest arrived from another machine. Substituter downloads need
# network; failures are logged loudly and are never fatal.
set -u

MANIFEST=/opt/codery/projects/Codery/containers/apps/tools-manifest.txt
PROFILE=/nix/var/nix/profiles/tools
NIX="nix --extra-experimental-features nix-command flakes"

[ -f "$MANIFEST" ] || exit 0
mkdir -p "$PROFILE"

have="$($NIX profile list --profile "$PROFILE" --json 2>/dev/null \
        | jq -r '.elements[]?.attrPath // empty' | sort -u || true)"

while IFS= read -r pkg; do
  case "$pkg" in ''|'#'*) continue ;; esac
  if grep -Fxq "$pkg" <<<"$have"; then continue; fi
  echo "tools-replay: installing nixpkgs#$pkg"
  $NIX profile install --profile "$PROFILE" "nixpkgs#$pkg" \
    || echo "tools-replay: FAILED $pkg (network? retry via: ssh gem@apps tooladd $pkg)"
done < "$MANIFEST"
exit 0
