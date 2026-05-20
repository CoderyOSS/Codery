#!/bin/bash
set -e

USER="gem"
USER_UID="1000"
USER_GID="1000"
CLAUDE_JSON="/home/${USER}/.claude.json"
DEFAULT_MCP="/home/${USER}/.config/claude/default-claude-mcp.json"

# Ensure directory exists
mkdir -p "$(dirname "$CLAUDE_JSON")"
chown "${USER_UID}:${USER_GID}" "$(dirname "$CLAUDE_JSON")"

# Copy default MCP config to .config
mkdir -p "$(dirname "$DEFAULT_MCP")"
cp /usr/local/share/claude/default-claude-mcp.json "$DEFAULT_MCP"
chown -R "${USER_UID}:${USER_GID}" "/home/${USER}/.config/claude"

# Initialize .claude.json if it doesn't exist
if [ ! -f "$CLAUDE_JSON" ]; then
    echo "[sandbox] Initializing ~/.claude.json with default MCP servers"
    cat "$DEFAULT_MCP" > "$CLAUDE_JSON"
    chown "${USER_UID}:${USER_GID}" "$CLAUDE_JSON"
else
    # Merge MCPs if missing (use jq to merge)
    if command -v jq &> /dev/null; then
        echo "[sandbox] Ensuring MCP servers are configured in ~/.claude.json"
        # Add missing MCPs using jq
        jq -s '.[0] * .[1]' "$CLAUDE_JSON" "$DEFAULT_MCP" > "${CLAUDE_JSON}.tmp" && mv "${CLAUDE_JSON}.tmp" "$CLAUDE_JSON"
        chown "${USER_UID}:${USER_GID}" "$CLAUDE_JSON"
    else
        echo "[sandbox] jq not available, skipping MCP merge"
    fi
fi

echo "[sandbox] MCP servers configured: codery, github-app, trailhead"
