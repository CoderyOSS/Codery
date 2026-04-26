#!/bin/bash
# caddy-route — add or remove a subdomain → port mapping in the Caddyfile
#
# Usage:
#   caddy-route add <subdomain> <port>     # e.g. caddy-route add myapp 5000
#   caddy-route remove <subdomain>         # e.g. caddy-route remove myapp
#   caddy-route list                       # show current routes

set -e

CADDYFILE=/etc/caddy/Caddyfile
DOMAIN="${DOMAIN_NAME:-example.com}"

usage() {
  echo "Usage:"
  echo "  caddy-route add <subdomain> <port>"
  echo "  caddy-route remove <subdomain>"
  echo "  caddy-route list"
  exit 1
}

cmd="${1:-}"
[ -z "$cmd" ] && usage

case "$cmd" in
  list)
    echo "Current routes in $CADDYFILE:"
    grep -E "^\S+\.$DOMAIN \{" "$CADDYFILE" | sed "s/ {//" | while read -r host; do
      port=$(grep -A3 "^${host} {" "$CADDYFILE" | grep "reverse_proxy" | grep -oE '[0-9]+$')
      printf "  %-45s -> localhost:%s\n" "$host" "$port"
    done
    ;;

  add)
    subdomain="${2:-}"
    port="${3:-}"
    [ -z "$subdomain" ] || [ -z "$port" ] && usage

    host="${subdomain}.${DOMAIN}"

    if grep -q "^${host} {" "$CADDYFILE"; then
      echo "ERROR: route for $host already exists. Use 'caddy-route remove $subdomain' first."
      exit 1
    fi

    cat >> "$CADDYFILE" <<EOF

${host} {
    bind {\$TAILSCALE_IP}
    reverse_proxy localhost:${port}
}
EOF

    echo "Added route: $host -> localhost:$port"
    echo "Reloading Caddy..."
    export TAILSCALE_IP=$(cat /run/tailscale.ip)
    caddy reload --config "$CADDYFILE" --adapter caddyfile
    echo "Done. Access at: https://$host"
    ;;

  remove)
    subdomain="${2:-}"
    [ -z "$subdomain" ] && usage

    host="${subdomain}.${DOMAIN}"

    if ! grep -q "^${host} {" "$CADDYFILE"; then
      echo "ERROR: no route found for $host"
      exit 1
    fi

    python3 - "$CADDYFILE" "$host" <<'PYEOF'
import sys

caddyfile, host = sys.argv[1], sys.argv[2]
with open(caddyfile) as f:
    lines = f.readlines()

out = []
skip = False
depth = 0
for line in lines:
    if not skip and line.startswith(host + ' {'):
        skip = True
        depth = 1
        continue
    if skip:
        depth += line.count('{') - line.count('}')
        if depth <= 0:
            skip = False
        continue
    out.append(line)

# Remove any blank lines that pile up at the end
content = ''.join(out).strip() + '\n'
with open(caddyfile, 'w') as f:
    f.write(content)
PYEOF

    echo "Removed route: $host"
    echo "Reloading Caddy..."
    export TAILSCALE_IP=$(cat /run/tailscale.ip)
    caddy reload --config "$CADDYFILE" --adapter caddyfile
    echo "Done."
    ;;

  *)
    usage
    ;;
esac
