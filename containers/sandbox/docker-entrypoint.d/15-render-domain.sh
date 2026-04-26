#!/bin/bash
set -e
DOMAIN="${DOMAIN_NAME:-example.com}"
echo "[render-domain] Substituting __DOMAIN_NAME__ -> ${DOMAIN} in supervisor configs"
sed -i "s/__DOMAIN_NAME__/${DOMAIN}/g" /etc/supervisor/conf.d/*.conf
