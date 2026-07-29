#!/bin/bash
set -e
CONF=/etc/nginx/conf.d/apps.conf
if [ ! -f "$CONF" ]; then
    cat > "$CONF" <<'EOF'
server {
    listen 8080 default_server;
    return 503 "No apps configured yet";
}
EOF
    echo "[apps] Wrote placeholder Nginx config (no apps deployed yet)"
fi
# nix nginx defaults to its store config, not /etc/nginx/nginx.conf —
# point it at ours explicitly.
nginx -t -q -c /etc/nginx/nginx.conf || { echo "[apps] ERROR: Nginx config test failed"; exit 1; }
echo "[apps] Nginx config validated — Launchy will manage nginx"
