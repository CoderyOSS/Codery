#!/bin/bash
set -e

su -s /bin/bash gem -c "
  claude mcp remove codery     2>/dev/null || true
  claude mcp remove github-app 2>/dev/null || true
  claude mcp remove trailhead  2>/dev/null || true
  claude mcp add -s user --transport http codery    http://host.docker.internal:4040/sse
  claude mcp add -s user       github-app bun       /usr/local/bin/github-app-permissions-mcp.ts
  claude mcp add -s user --transport http trailhead http://host.docker.internal:4050/mcp/sse
"
echo "[sandbox] Claude MCP servers configured"
