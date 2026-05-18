# GitHub App MCP Server — Design Spec

## Problem

AI agents in the sandbox make incorrect claims about what the GitHub App (agentsgonewild) can do — e.g., claiming the bot lacks workflow permissions when it has them. Agents have no API-level visibility into App installation permissions across orgs.

## Solution

Add a local MCP server to the sandbox that exposes GitHub App installation permissions as MCP tools. TypeScript, single file, zero dependencies, runs via Bun (already in sandbox).

## Architecture

```
Agent (OpenCode)
   │ MCP stdio (JSON-RPC)
   ▼
github-app-permissions-mcp.ts  ← Bun + built-in fetch/crypto
   │
   ├─ JWT signing: openssl dgst -sha256 -sign /run/secrets/github-app.pem
   │  (reuses exact same mechanism as github-app-token.sh)
   │
   └─ API:  GET https://api.github.com/app/installations (JWT auth)
             POST /app/installations/{id}/access_tokens
```

## MCP Protocol

- Transport: stdio (JSON-RPC 2.0 messages over stdin/stdout)
- Methods: `initialize`, `notifications/initialized`, `tools/list`, `tools/call`
- JSON parsed/generated with native `JSON.parse`/`stringify`
- Stderr used for logging; stdout reserved for MCP transport

## Tools

### `get_installations`
List all GitHub App installations with full permissions.

**Parameters:** none

**Returns:**
```json
[{
  "id": 127142249,
  "account": { "login": "CoderyOSS", "type": "Organization" },
  "permissions": { "actions": "write", "contents": "write", "workflows": "write", ... },
  "events": ["push", "pull_request", ...],
  "repository_selection": "all",
  "suspended": false,
  "created_at": "2026-01-15T...",
  "updated_at": "2026-05-10T..."
}]
```

### `get_installation`
Get a single installation by org/user name.

**Parameters:** `account` (string, required)

### `check_permission`
Fast yes/no check for specific permission.

**Parameters:** `account` (string, required), `permission` (string, required)

**Returns:** `{"account": "CoderyOSS", "permission": "actions", "has_permission": true, "level": "write"}`

## JWT Generation

```
header: {"alg":"RS256","typ":"JWT"}
payload: {"iat":<now-60s>,"exp":<now+540s>,"iss":"<APP_ID>"}
signature: openssl dgst -sha256 -sign /run/secrets/github-app.pem
```

JWT cached in memory, regenerated when <60s remaining.

## Environment

Reads `GITHUB_APP_ID` from env (already set in service.yml). PEM path hardcoded to `/run/secrets/github-app.pem`.

## Configuration

Added to `opencode.json` under `mcp`:
```json
"github-app": {
  "type": "local",
  "command": ["bun", "/usr/local/bin/github-app-permissions-mcp.ts"]
}
```

## Files Changed

| File | Action |
|------|--------|
| `containers/sandbox/bin/github-app-permissions-mcp.ts` | New |
| `opencode.json` | Add MCP config entry |

No Dockerfile changes. No new dependencies. Bun already in image.

## Testing

1. Direct stdio test: `echo '{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0.0"}},"id":1}' | bun run /usr/local/bin/github-app-permissions-mcp.ts`
2. Tool list test: verify `tools/list` returns 3 tools
3. `get_installations` call: verify returns real installation data
4. `check_permission` call: verify returns correct permission level
5. Restart opencode, verify agents discover the tool automatically
