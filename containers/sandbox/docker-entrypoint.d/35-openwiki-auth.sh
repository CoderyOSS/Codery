#!/bin/bash
set -e

# OpenWiki CLI provider config. Derived from ZAI_API_KEY (already required_env
# in service.yml). Regenerated on every container start — no bind mount needed
# since code-mode wiki lives in the repo (openwiki/), not ~/.openwiki/.

if [ -z "${ZAI_API_KEY:-}" ]; then
  echo "[sandbox] ZAI_API_KEY not set, skipping OpenWiki setup"
  exit 0
fi

mkdir -p /home/gem/.openwiki

cat > /home/gem/.openwiki/.env <<EOF
OPENWIKI_PROVIDER=openai-compatible
OPENWIKI_MODEL_ID=glm-5.2
OPENAI_COMPATIBLE_API_KEY=${ZAI_API_KEY}
OPENAI_COMPATIBLE_BASE_URL=https://open.bigmodel.cn/api/paas/v4
EOF

chown -R 1000:1000 /home/gem/.openwiki
chmod 600 /home/gem/.openwiki/.env
echo "[sandbox] OpenWiki configured for Z.ai glm-5.2"
