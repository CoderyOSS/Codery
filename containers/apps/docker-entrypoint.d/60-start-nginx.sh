#!/bin/bash
# Start Nginx internal reverse proxy on port 8080.
# /etc/nginx/conf.d/apps.conf is bind-mounted from the host by the orchestrator.
# If not present yet (first boot before any apps deployed), write a placeholder.

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

nginx -t -q || { echo "[apps] ERROR: Nginx config test failed"; exit 1; }
nginx -g 'daemon off;' &
echo "[apps] Nginx started on port 8080 (pid $!)"
