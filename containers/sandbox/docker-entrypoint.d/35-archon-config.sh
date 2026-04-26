#!/bin/bash
set -e

ARCHON_DIR="/home/gem/.archon"
ENV_FILE="${ARCHON_DIR}/.env"
CLAUDE_SETTINGS="/home/gem/.claude/settings.json"

mkdir -p "${ARCHON_DIR}"
chown gem:gem "${ARCHON_DIR}" 2>/dev/null || true

GH_TOKEN=$(github-app-token 2>/dev/null) || true

cat > "${ENV_FILE}" <<EOF
GH_TOKEN=${GH_TOKEN:-}
GITHUB_ALLOWED_USERS=obra
DEFAULT_AI_ASSISTANT=opencode
MAX_CONCURRENT_CONVERSATIONS=10
ARCHON_TELEMETRY_DISABLED=1
TMPDIR=/home/gem/.cache/archon-tmp
OPENCODE_BIN_PATH=/usr/bin/opencode
EOF

# Z.ai GLM via Anthropic-compatible endpoint.
# Uses global OAuth auth (from .claude.json) + ANTHROPIC_BASE_URL to route
# requests through Z.ai. Do NOT set CLAUDE_API_KEY — it triggers explicit
# auth mode which uses the wrong (exhausted) ANTHROPIC_API_KEY from the
# host container env.
if [ -n "${ZAI_API_KEY:-}" ]; then
  cat >> "${ENV_FILE}" <<EOF
ANTHROPIC_BASE_URL=https://api.z.ai/api/anthropic
ANTHROPIC_API_KEY=${ZAI_API_KEY}
ANTHROPIC_AUTH_TOKEN=${ZAI_API_KEY}
EOF

  if [ -f "${CLAUDE_SETTINGS}" ]; then
    python3 -c "
import json, sys
with open('${CLAUDE_SETTINGS}') as f:
    s = json.load(f)
s['env'] = s.get('env', {})
s['env']['ANTHROPIC_BASE_URL'] = 'https://api.z.ai/api/anthropic'
s['env']['ANTHROPIC_API_KEY'] = '${ZAI_API_KEY}'
s['env']['ANTHROPIC_AUTH_TOKEN'] = '${ZAI_API_KEY}'
s['env']['API_TIMEOUT_MS'] = '3000000'
with open('${CLAUDE_SETTINGS}', 'w') as f:
    json.dump(s, f, indent=2)
"
  fi
fi

chown gem:gem "${ENV_FILE}" 2>/dev/null || true
chmod 600 "${ENV_FILE}" 2>/dev/null || true
echo "[sandbox] Archon config written to ${ENV_FILE}"
