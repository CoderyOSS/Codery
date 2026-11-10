#!/bin/sh
# nix-store-sync — run ON THE HOST before deploying a rebuilt apps image
# when the /nix named volume already exists (i.e. every apps deploy after
# the volume's creation). The volume is not empty on redeploy, so Docker
# will NOT re-seed it from the new image; this one-shot copies the new
# image's NEW store paths into the volume and repoints the default profile.
#
# Nix store paths are content-addressed and immutable — skipping existing
# paths is safe; only new paths are copied.
#
# Usage: nix-store-sync.sh <new-apps-image-ref> [volume-name]
#   e.g.  ./nix-store-sync.sh ghcr.io/coderyoss/codery:apps-<sha> codery-apps-nix
set -eu

IMG="${1:?usage: nix-store-sync.sh <image-ref> [volume]}"
VOL="${2:-codery-apps-nix}"

docker volume inspect "$VOL" >/dev/null 2>&1 || {
  echo "volume $VOL does not exist yet — nothing to sync (Docker will seed it on first boot)"
  exit 0
}

echo "nix-store-sync: copying new store paths from $IMG into $VOL ..."
docker run --rm \
  -v "$VOL":/vol \
  --entrypoint /bin/sh \
  "$IMG" -c '
    set -e
    # Copy everything not already present (content-addressed => skip is safe)
    cp -a -n /nix/. /vol/nix/
    # Repoint the default profile at the new image generation
    newdef=$(readlink /nix/var/nix/profiles/default)
    [ -n "$newdef" ] && ln -sfn "$newdef" /vol/nix/var/nix/profiles/default
    echo "nix-store-sync: done (default profile -> $newdef)"
  '
