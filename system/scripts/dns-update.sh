#!/bin/bash
# dns-update.sh — Poll for Tailscale IP, write to /run/tailscale.ip,
# and upsert a Cloudflare wildcard A record for *.$DOMAIN_NAME
set -e

DOMAIN_NAME="${DOMAIN_NAME:-example.com}"
RECORD_NAME="*.${DOMAIN_NAME}"
IP_FILE="/run/tailscale.ip"

echo "[dns-update] Waiting for Tailscale IP..."
TS_IP=""
for i in $(seq 1 120); do
  TS_IP=$(tailscale status --json 2>/dev/null \
    | python3 -c "
import sys, json
data = json.load(sys.stdin)
addrs = data.get('Self', {}).get('TailscaleIPs', [])
print(next((ip for ip in addrs if ':' not in ip), ''))
" 2>/dev/null || echo "")
  if [ -n "$TS_IP" ]; then
    break
  fi
  sleep 1
done

if [ -z "$TS_IP" ]; then
  echo "[dns-update] ERROR: Could not obtain Tailscale IP after 120s"
  exit 1
fi

echo "[dns-update] Tailscale IP: $TS_IP"
echo "$TS_IP" > "$IP_FILE"
echo "[dns-update] Wrote $TS_IP to $IP_FILE"

# Skip Cloudflare update if credentials are not set
if [ -z "${CLOUDFLARE_API_TOKEN:-}" ] || [ -z "${CLOUDFLARE_ZONE_ID:-}" ]; then
  echo "[dns-update] WARNING: CLOUDFLARE_API_TOKEN or CLOUDFLARE_ZONE_ID not set — skipping DNS update"
  exit 0
fi

echo "[dns-update] Upserting Cloudflare A record: $RECORD_NAME -> $TS_IP"

# Look up existing record ID
RECORD_ID=$(curl -sf -X GET \
  "https://api.cloudflare.com/client/v4/zones/${CLOUDFLARE_ZONE_ID}/dns_records?type=A&name=${RECORD_NAME}" \
  -H "Authorization: Bearer ${CLOUDFLARE_API_TOKEN}" \
  -H "Content-Type: application/json" \
  | python3 -c "
import sys, json
records = json.load(sys.stdin).get('result', [])
print(records[0]['id'] if records else '')
" 2>/dev/null || echo "")

PAYLOAD="{\"type\":\"A\",\"name\":\"${RECORD_NAME}\",\"content\":\"${TS_IP}\",\"ttl\":60,\"proxied\":false}"

if [ -n "$RECORD_ID" ]; then
  curl -sf -X PUT \
    "https://api.cloudflare.com/client/v4/zones/${CLOUDFLARE_ZONE_ID}/dns_records/${RECORD_ID}" \
    -H "Authorization: Bearer ${CLOUDFLARE_API_TOKEN}" \
    -H "Content-Type: application/json" \
    --data "$PAYLOAD" > /dev/null
  echo "[dns-update] Updated existing A record $RECORD_NAME -> $TS_IP"
else
  curl -sf -X POST \
    "https://api.cloudflare.com/client/v4/zones/${CLOUDFLARE_ZONE_ID}/dns_records" \
    -H "Authorization: Bearer ${CLOUDFLARE_API_TOKEN}" \
    -H "Content-Type: application/json" \
    --data "$PAYLOAD" > /dev/null
  echo "[dns-update] Created new A record $RECORD_NAME -> $TS_IP"
fi
