# Design: Nix Sandbox + Standalone Playwright Runtime

Date: 2026-09-02
Status: approved

## Goal

Make the Sandbox environment simple, declarative, reproducible, and
maintainable. Nix manages development tools. Microsoft's official Playwright
image manages the Chromium Linux dependency mess. Neither side reimplements
the other.

## Architecture boundary

```
Codery Sandbox (Nix rootfs — tools only)
  ├── OpenCode + MCP clients (@playwright/mcp)
  │
  │  ws://playwright:3000/  (codery-net, Docker DNS alias "playwright")
  ▼
Playwright container (mcr.microsoft.com/playwright:v1.54.1-noble)
  ├── Chromium (headless, no X server, no desktop environment)
  │
  │  HTTP http://apps:<port>  (codery-net, existing alias "apps")
  ▼
Apps container (user applications, bound to 0.0.0.0)
```

- The Sandbox is a **control plane** for browsing: OpenCode → MCP → Playwright
  server → Chromium. It never proxies application HTTP traffic.
- `localhost` is container-local. Chromium addresses user applications by the
  Apps container's Docker DNS identity (`http://apps:<port>`), and apps
  intended for browser access must bind to `0.0.0.0:<port>`.
- Codery owns only: image version, startup argv, networking, resource/security
  configuration. Microsoft owns Chromium's dependency set.

## Version coupling (single deliberate change)

| Component | Pin | Where |
|---|---|---|
| `@playwright/mcp` | `0.0.30` | `opencode.json` |
| Playwright protocol (client) | `1.54.1` | bundled by `@playwright/mcp@0.0.30`; re-pinned in run-server argv |
| Browser image | `mcr.microsoft.com/playwright:v1.54.1-noble` | `containers/playwright/service.yml` |

Rules:

- Never `@playwright/mcp@latest` and never an alpha Playwright build. Browser
  infrastructure stability beats newest MCP features.
- Playwright client and server versions must match exactly (Playwright errors
  on mismatch). Upgrades change all three rows above in one commit.
- `0.0.30` is the newest MCP release that bundles a *stable* Playwright
  (1.54.1). Its tool list is identical to newer MCP releases
  (`browser_find`, `browser_snapshot`, `browser_drop`, etc. all present).
- If Playwright misbehaves: check client/server version match and the WS
  connection first. **Never** respond by adding more Linux libraries to the
  Sandbox.

## CoderyCI changes (generic container capabilities)

No Playwright-specific behavior enters CoderyCI.

1. **Opaque image references** — `service.yml image` accepts any full OCI ref
   (e.g. `mcr.microsoft.com/playwright:v1.54.1-noble`). The pull path no
   longer constructs `ghcr.io/...` refs; `{sha}` substitution remains a no-op
   for fixed tags.
2. **Registry auth by hostname** — `ghcr.io` uses the existing GHCR creds from
   `/opt/codery/.env`; `mcr.microsoft.com` and everything else pull
   anonymously.
3. **Generic process config** — new optional `service.yml` fields:
   - `command: [argv]` (argv, never a shell string)
   - `entrypoint: [argv]`
   - `user`, `workdir`
   - `init: true|false` (Docker `--init` / tini)
   - `ipc: host|private` (Docker `--ipc`)

## Playwright service

`containers/playwright/service.yml` (no Dockerfile — image pulled directly):

- `image: mcr.microsoft.com/playwright:v1.54.1-noble` (pinned)
- `command: ["npx","-y","playwright@1.54.1","run-server","--port","3000","--host","0.0.0.0"]`
  — the explicit npx pin re-verifies the protocol version at container start;
  known cost: downloads the package on first start of a fresh container.
- `user: pwuser`, `workdir: /home/pwuser`, `init: true`, `ipc: host`
- one named port `ws:3000`, no public subdomain; offsets blue 40000 / green
  50000 (host 43000/53000 — clash-free vs sandbox 10000/20000 and apps
  0/10000 schemes)
- `health_check: tcp` on `ws`
- `network: codery-net`, `network_aliases: [playwright]`

Deployment: `.github/workflows/deploy-playwright.yml` — manual-only, no build
step; syncs the YAML and runs `codery-ci deploy playwright <version>`.
Playwright is pull-only: rollback = revert the pin and redeploy.

## Sandbox changes

- `opencode.json` playwright MCP:
  `["npx","-y","@playwright/mcp@0.0.30","--endpoint","ws://playwright:3000/","--headless","--browser","chromium"]`
- `containers/sandbox/service.yml`: `env_overrides.PLAYWRIGHT_WS_ENDPOINT:
  ws://playwright:3000/`
- `examples/Dockerfile.sandbox`: export `PLAYWRIGHT_WS_ENDPOINT` in `.bashrc`
  (sshd login shells strip parent env)
- `containers/sandbox/nixos/configuration.nix`: comment-only fix on the FHS
  dynamic-loader block (drop the "Playwright browsers" rationale; the loader
  stays — opencode/claude npm bins hardcode `/lib64/ld-linux-x86-64.so.2`).
  **No package additions.**
- Delete `containers/sandbox/Dockerfile.base` (legacy apt image built FROM the
  MS Playwright image — the pattern being eliminated) and
  `.github/workflows/build-sandbox-base.yml`.

## Verification

1. Fresh sandbox version sweep: `node bun python uv git gh rg opencode --version`.
2. Cross-container E2E — exercises Sandbox/OpenCode → MCP → Playwright →
   Chromium → Apps:
   - start a throwaway app **in the Apps container** bound to
     `0.0.0.0:8099` serving a page with a distinctive title
   - from the Sandbox, drive the pinned MCP config over stdio JSON-RPC:
     `browser_navigate http://apps:8099` → title/DOM → screenshot → clean exit
   - belt-and-braces: direct `chromium.connect(ws://playwright:3000/)` check
3. Chromium processes exist only in `codery-playwright-*` (host `docker top`);
   none in the Sandbox; no X11 anywhere.
4. Interactive: OpenCode agent uses the browser tools against `http://apps:8099`
   post-cutover.

## Acceptance criteria

- Sandbox builds reproducibly (Nix closure → OCI).
- Normal CLI/runtime dependencies declared through Nix (`toolEnv`).
- Python/uv, Node/Bun, OpenCode work.
- Playwright controls Chromium; Chromium runs in Microsoft's official image.
- No custom Chromium library list exists in Codery; no desktop environment.
- The Sandbox Dockerfile has no apt/curl-installer package logic.
- Adding a normal CLI tool = adding one Nix package.
- Playwright upgrades deliberately move client + image versions together.
- Documentation explains the boundary (this doc, `docs/playwright.md`, AGENTS.md).
