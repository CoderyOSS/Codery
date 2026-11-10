#!/bin/sh
# zoraxy state bootstrap — restores admin, ACME, and proxy rules after a state
# wipe (known hazard: deploy-time volume lifecycle deletes declared named
# volumes; see docs/runbook). Idempotent.
# Run on the host:  /opt/codery/stacks/zoraxy/bootstrap-state.sh
#
# Reads from /opt/codery/.env: DOMAIN_NAME, ACME_EMAIL (operator contact),
# ZORAXY_ADMIN_USER, ZORAXY_ADMIN_PASSWORD, CLOUDFLARE_API_TOKEN.
# Vhost inventory at the bottom — add one `add` line per proxy-host.
set -eu
IP=$(cat /run/tailscale.ip)
B="http://$IP:8000"
C=/opt/codery/stacks/zoraxy/.admin.cookie
ENVF=/opt/codery/.env
kv() { grep "^$1=" "$ENVF" | head -1 | cut -d= -f2-; }

DOMAIN=$(kv DOMAIN_NAME); ACME_EMAIL=$(kv ACME_EMAIL)
ZU=$(kv ZORAXY_ADMIN_USER); ZP=$(kv ZORAXY_ADMIN_PASSWORD)
csrf() { curl -s -c "$C" -b "$C" "$B/login.html" | grep -oE 'zoraxy.csrf.Token" content="[^"]*' | sed 's/.*content="//'; }
T=$(csrf)

echo "== register admin (idempotent) =="
curl -s -b "$C" -c "$C" -H "X-CSRF-Token: $T" \
  -d "username=$ZU&password=$ZP&passwordconfirm=$ZP" "$B/api/auth/register" | head -c 80; echo
echo "== login =="
T=$(csrf)
curl -s -b "$C" -c "$C" -H "X-CSRF-Token: $T" \
  -d "username=$ZU&password=$ZP" "$B/api/auth/login" | head -c 80; echo
chmod 600 "$C"

echo "== acme email =="
T=$(csrf)
curl -s -b "$C" -H "X-CSRF-Token: $T" -d "set=$ACME_EMAIL" "$B/api/acme/autoRenew/email"; echo

echo "== cloudflare dns credentials (wildcard filename) =="
T=$(csrf)
CF=$(kv CLOUDFLARE_API_TOKEN)
curl -s -b "$C" -H "X-CSRF-Token: $T" \
  -d "filename=_.$DOMAIN&dnsProvider=cloudflare&dnsCredentials={\"AuthToken\":\"$CF\"}" \
  "$B/api/acme/autoRenew/setDNS"; echo

echo "== obtain wildcard cert (dns challenge) =="
T=$(csrf)
curl -s -b "$C" -H "X-CSRF-Token: $T" --max-time 120 \
  "$B/api/acme/obtainCert?domains=*.$DOMAIN&filename=_.$DOMAIN&email=$ACME_EMAIL&ca=Let%27s%20Encrypt&caURL=&skipTLS=false&dns=true&dnsServers="; echo

echo "== proxy rules =="
T=$(csrf)
add() { curl -s -b "$C" -H "X-CSRF-Token: $T" \
  -d "type=host&rootname=$1&tls=false&ep=$2&tlsval=false&bpwsorg=true&bypassGlobalTLS=false&enableConnectSupport=false&bauth=false&rate=false&ratenum=0&cred=&access=default" \
  "$B/api/proxy/add"; echo "  $1 -> $2"; }
# Core + tools (adjust to the tools you enabled in COMPOSE_PROFILES):
add "code.$DOMAIN"        paseo:6767
add "dockhand.$DOMAIN"    dockhand:3000
# add "opendesign.$DOMAIN" opendesign:7456      # profile: design
# add "dbgate.$DOMAIN"     dbgate:3000          # profile: db
# Projects — add your own:
# add "webapp.$DOMAIN"     webapp:3000

echo "== verify =="
sleep 2
curl -s -b "$C" -H "X-CSRF-Token: $T" "$B/api/proxy/list?type=host" \
  | python3 -c "import json,sys; [print(' ', r['RootOrMatchingDomain'], '->', r['ActiveOrigins'][0]['OriginIpOrDomain']) for r in json.load(sys.stdin)]"
curl -s -b "$C" -H "X-CSRF-Token: $T" "$B/api/cert/list?date=true" | head -c 200; echo
echo "== bootstrap complete =="
