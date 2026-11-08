#!/bin/sh
# codery-backup — in-band FULL backup of the Codery VPS (run on the host).
#
# Purpose: the migration backout safety net. Captures everything needed to
# rebuild the host from scratch without a netcup hypervisor snapshot:
#   - active docker images (docker save) for all managed services
#   - /opt/codery state: .env, state/, services/, ssh/, codery.db, proxy/, bin/
#   - Caddy + supervisord configs
#   - named volumes that are NOT re-seedable (paseo home, ssh host keys,
#     dockhand data; NOT codery-apps-nix — re-seeded from image + manifest)
#   - projects tree (repos with .git, minus heavy build artifacts)
#
# Usage:    /opt/codery/bin/codery-backup.sh [--out DIR]
# Output:   /opt/backups/codery-full-<ts>/ + SHA256SUMS, chmod 600
# Optional: rclone copy to B2 if `rclone` is on PATH (remote named in $RCLONE_REMOTE)
set -eu

TS=$(date -u +%Y%m%dT%H%M%SZ)
OUT="${2:-/opt/backups/codery-full-$TS}"
mkdir -p "$OUT"
chmod 700 "$OUT"
echo "== codery-backup -> $OUT"

# --- 1. Docker images ---------------------------------------------------
mkdir -p "$OUT/images"
IMAGES=""
for f in /opt/codery/services/*.yml; do
  svc=$(sed -n 's/^service: *//p' "$f" | head -1)
  sha=$(cat "/opt/codery/state/$svc.sha" 2>/dev/null || true)
  [ -n "$svc" ] && [ -n "$sha" ] && IMAGES="$IMAGES ghcr.io/coderyoss/codery:${svc}-${sha}"
done
IMAGES="$IMAGES mcr.microsoft.com/playwright:v1.54.1-noble fnsys/dockhand:latest"
for img in $IMAGES; do
  if docker image inspect "$img" >/dev/null 2>&1; then
    f="$OUT/images/$(echo "$img" | tr '/:' '__').tar"
    echo "  image: $img"
    docker save "$img" -o "$f"
  else
    echo "  image (absent, skipped): $img"
  fi
done

# --- 2. Codery state + configs -----------------------------------------
mkdir -p "$OUT/files"
for p in /opt/codery/.env /opt/codery/codery.db; do
  [ -f "$p" ] && cp -a "$p" "$OUT/files/$(basename "$p")"
done
for d in /opt/codery/state /opt/codery/services /opt/codery/ssh /opt/codery/proxy /opt/codery/apps-s6.d /opt/codery/bin; do
  [ -d "$d" ] && tar -C "$(dirname "$d")" -cf "$OUT/files/$(basename "$d").tar" "$(basename "$d")"
done
[ -f /etc/caddy/Caddyfile ] && cp -a /etc/caddy/Caddyfile "$OUT/files/Caddyfile"
[ -d /etc/supervisor ] && tar -C /etc -cf "$OUT/files/supervisor.tar" supervisor
[ -f /usr/local/bin/codery-triage ] && cp -a /usr/local/bin/codery-triage "$OUT/files/"
tailscale status --json > "$OUT/files/tailscale-status.json" 2>/dev/null || true
cp /run/tailscale.ip "$OUT/files/" 2>/dev/null || true

# --- 3. Named volumes that are not re-seedable --------------------------
mkdir -p "$OUT/volumes"
for vol in codery-paseo-home codery-paseo-ssh-host-keys codery-dockhand-data; do
  if docker volume inspect "$vol" >/dev/null 2>&1; then
    echo "  volume: $vol"
    docker run --rm -v "$vol":/v -v "$OUT/volumes":/out alpine \
      sh -c "tar -cf /out/$vol.tar -C /v ."
  fi
done
# codery-apps-nix deliberately NOT saved: multi-GB, re-seeded from the image
# + tools manifest replay (see docs/runbook/openminis-migration.md).

# --- 4. Projects tree (repos, minus build artifacts) --------------------
echo "  projects tree (excludes build artifacts; keeps .git)"
tar -C /opt/codery -cf "$OUT/projects.tar" \
  --exclude='projects/flutter' \
  --exclude='projects/.pub-cache' \
  --exclude='*/node_modules' \
  --exclude='*/.worktrees' \
  --exclude='*/target' \
  --exclude='*/__pycache__' \
  --exclude='*/.venv' \
  --exclude='*/venv' \
  --exclude='*/.next' \
  --exclude='*/dist' \
  --exclude='*/.cache' \
  projects

# --- 5. Manifest + optional offsite -------------------------------------
cd "$OUT"
find . -type f ! -name SHA256SUMS -exec sha256sum {} \; > SHA256SUMS
chmod -R go-rwx "$OUT"
du -sh "$OUT" "$OUT"/* 2>/dev/null || true
echo "== backup complete: $OUT"
echo "   restore instructions: docs/runbook/openminis-migration.md (FULL ABORT section)"

if command -v rclone >/dev/null 2>&1 && [ -n "${RCLONE_REMOTE:-}" ]; then
  echo "== rclone -> $RCLONE_REMOTE"
  rclone copy "$OUT" "$RCLONE_REMOTE/codery-full-$TS"
fi
