#!/bin/sh
# nix-store-sync — run ON THE HOST before deploying a rebuilt apps image
# when the /nix named volume already exists (i.e. every apps deploy after
# the volume's creation). The volume is not empty on redeploy, so Docker
# will NOT re-seed it from the new image; this one-shot copies the new
# image's NEW store paths into the volume and repoints the default profile.
#
# Nix store paths are content-addressed and immutable — a full cp -a is a
# safe merge (existing entries are identical); do NOT use cp -n to "skip":
# partial skips are what broke the volume during the 2026-11-12 incident.
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

echo "nix-store-sync: copying store from $IMG into $VOL ..."
docker run --rm \
  -v "$VOL":/vol \
  --entrypoint /bin/sh \
  "$IMG" -c '
    set -e
    # FULL copy, no skipping: the 2026-11-12 incident showed a partial
    # sync leaving the volume without the new image'"'"'s store paths, which
    # orphaned store-referenced symlinks (/etc/nix/nix.conf -> store) and
    # broke every bare `nix` call. Content-addressed paths make re-copying
    # existing entries a no-op merge.
    cp -a /nix/. /vol/nix/
    # Repoint the default profile at the new image generation
    newdef=$(readlink /nix/var/nix/profiles/default)
    [ -n "$newdef" ] && ln -sfn "$newdef" /vol/nix/var/nix/profiles/default
    # Verify in the same container that wrote the data
    [ -d "/vol$newdef" ] || { echo "nix-store-sync: FAILED — $newdef missing from volume" >&2; exit 1; }
    echo "nix-store-sync: volume store now $(ls /vol/nix/store | wc -l) paths (image has $(ls /nix/store | wc -l))"
    echo "nix-store-sync: done (default profile -> $newdef)"
  '
echo "nix-store-sync: NOTE — if the apps container is already running, recreate it so its /nix view refreshes"
