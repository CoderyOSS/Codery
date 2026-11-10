#!/bin/sh
# zoraxy state bootstrap — restores admin, ACME, and all proxy rules after a
# state wipe (known hazard: Dockhand deploy-time volume lifecycle deletes
# declared named volumes; see codery runbook §0.1). Idempotent.
# Run on the host:  /opt/codery/stacks/zoraxy/bootstrap-state.sh
set -eu
B=http://100.89.78.41:8000
C=/opt/codery/stacks/zoraxy/.admin.cookie
ENVF=/opt/codery/.env
kv() { grep "^$1=" "$ENVF" | head -1 | cut -d= -f2-; }

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
curl -s -b "$C" -H "X-CSRF-Token: $T" -d "set=rob@rancidgrandmas.online" "$B/api/acme/autoRenew/email"; echo

echo "== cloudflare dns credentials (wildcard filename) =="
T=$(csrf)
CF=$(kv CLOUDFLARE_API_TOKEN)
curl -s -b "$C" -H "X-CSRF-Token: $T" \
  -d "filename=_.rancidgrandmas.online&dnsProvider=cloudflare&dnsCredentials={\"AuthToken\":\"$CF\"}" \
  "$B/api/acme/autoRenew/setDNS"; echo

echo "== obtain wildcard cert (dns challenge) =="
T=$(csrf)
curl -s -b "$C" -H "X-CSRF-Token: $T" --max-time 120 \
  "$B/api/acme/obtainCert?domains=*.rancidgrandmas.online&filename=_.rancidgrandmas.online&email=rob@rancidgrandmas.online&ca=Let%27s%20Encrypt&caURL=&skipTLS=false&dns=true&dnsServers="; echo

echo "== proxy rules =="
T=$(csrf)
add() { curl -s -b "$C" -H "X-CSRF-Token: $T" \
  -d "type=host&rootname=$1&tls=false&ep=$2&tlsval=false&bpwsorg=true&bypassGlobalTLS=false&enableConnectSupport=false&bauth=false&rate=false&ratenum=0&cred=&access=default" \
  "$B/api/proxy/add"; echo "  $1 -> $2"; }
add code.rancidgrandmas.online        paseo:6767
add dockhand.rancidgrandmas.online    dockhand:3000
add remi.rancidgrandmas.online        remi_backend-frontend-1:4321
add opendesign.rancidgrandmas.online  opendesign:7456
add qdrant.rancidgrandmas.online      apps:8125
add ideas.rancidgrandmas.online       apps:8124
add label.rancidgrandmas.online       apps:8123
add booksearch.rancidgrandmas.online  apps:8126
add graph.rancidgrandmas.online       apps:8127
add bolt.rancidgrandmas.online        apps:8128

echo "== verify =="
sleep 2
curl -s -b "$C" -H "X-CSRF-Token: $T" "$B/api/proxy/list?type=host" \
  | python3 -c "import json,sys; [print(' ', r['RootOrMatchingDomain'], '->', r['ActiveOrigins'][0]['OriginIpOrDomain']) for r in json.load(sys.stdin)]"
curl -s -b "$C" -H "X-CSRF-Token: $T" "$B/api/cert/list?date=true" | head -c 200; echo
echo "== bootstrap complete =="
