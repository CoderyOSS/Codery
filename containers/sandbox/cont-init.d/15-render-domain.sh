#!/command/with-contenv bash
set -e
DOMAIN="${DOMAIN_NAME:-example.com}"
echo "[render-domain] writing /run/env/opendesign.env (domain: ${DOMAIN})"
mkdir -p /run/env
cat > /run/env/opendesign.env <<EOF
NODE_ENV=production
NODE_OPTIONS=--max-old-space-size=192
OD_BIND_HOST=0.0.0.0
OD_PORT=7456
OD_DISABLE_API_AUTH=1
OD_ALLOWED_ORIGINS=https://opendesign.${DOMAIN}
EOF
