# Playwright — Browser Runtime (Separate from the Sandbox)

## The boundary

Codery runs browser automation in **Microsoft's official Playwright image**,
not in the Sandbox.

```
Sandbox (Nix rootfs — dev tools only)      Playwright (mcr.microsoft.com/playwright)
  OpenCode                                     ├── Chromium (headless, no X)
    └── MCP @playwright/mcp                    └── npx playwright run-server :3000
          │                                               │
          └── ws://playwright:3000/ (codery-net) ─────────┘
                                                    │ HTTP
                                                    ▼
                                        Apps (alias "apps", user apps on 0.0.0.0)
```

- **Nix owns development tools.** Adding a CLI tool to the sandbox = adding
  one package to `containers/sandbox/nixos/configuration.nix`.
- **Microsoft owns Chromium's Linux dependencies.** Codery owns only: image
  version, startup argv, networking, security flags
  (`containers/playwright/service.yml`).
- **Never** add Chromium shared-library packages (libnss, libgbm, gtk, …) to
  the sandbox to make a browser start. That is the anti-pattern this boundary
  eliminates.

## Version coupling (upgrade all in one commit)

| Component | Pin | Where |
|---|---|---|
| MCP client | `@playwright/mcp@0.0.30` | `opencode.json` |
| zod | `zod@3.24.1` | `opencode.json` (`npm exec --package`) |
| zod-to-json-schema | `zod-to-json-schema@3.24.4` | `opencode.json` (`npm exec --package`) |
| Playwright protocol | `playwright@1.54.1` (run-server argv) | `containers/playwright/service.yml` |
| Browser image | `mcr.microsoft.com/playwright:v1.54.1-noble` | `containers/playwright/service.yml` |

The MCP is launched as
`npm exec -y --package=@playwright/mcp@0.0.30 --package=zod@3.24.1
--package=zod-to-json-schema@3.24.4 -- mcp-server-playwright --config ...`
instead of `npx @playwright/mcp@0.0.30`. The two extra `--package` pins are
required: `@playwright/mcp@0.0.30` does not declare `zod` as a dependency, so
npx resolves the latest (zod 4.x). `zod-to-json-schema@3.x` cannot introspect
zod 4 schemas and emits `{"$schema": ...}` stubs with no `type`/`properties`
for every tool. OpenCode's strict tool-schema validation then rejects all 25
tools and the MCP shows `failed: "Failed to get tools"`. Pinning zod to the
3.x line restores full `inputSchema`s. If a future `@playwright/mcp` release
properly pins its own zod, the extra pins can be dropped.

Mechanism: `@playwright/mcp@0.0.30` connects to the remote browser server via
its config file `remoteEndpoint` option (`containers/sandbox/
playwright-mcp-config.json`, baked to
`/home/gem/.config/opencode/playwright-mcp-config.json`). Note: 0.0.30 does
NOT support a `--endpoint` CLI flag — `remoteEndpoint` is the supported
remote-server mechanism at this version, so the MCP *client* runs in the
sandbox while Chromium itself runs in the Playwright container.

Playwright refuses to run when client and server versions mismatch. All three
pins must move together — a Playwright upgrade is a deliberate, single change:

1. Pick a stable `@playwright/mcp` release and find the stable `playwright`
   version it bundles (`npm view @playwright/mcp@<ver> dependencies`).
2. Confirm a matching MCR tag exists
   (`https://mcr.microsoft.com/v2/playwright/tags/list`, e.g.
   `v1.54.1-noble`). No stable MCR tag → that MCP release cannot be used.
3. Update `opencode.json` (MCP pin + zod/zod-to-json-schema pins), the
   `command` and `image` in `containers/playwright/service.yml`, and the
   default in `.github/workflows/deploy-playwright.yml` — one commit.
4. Deploy: `codery-ci deploy playwright vX.Y.Z` (or the Deploy Playwright
   workflow), then deploy the sandbox image (its MCP pin changed).

## Sandboxing note

`playwright-mcp-config.json` sets `chromiumSandbox: false` (Playwright's
`--no-sandbox` equivalent). Docker's default seccomp profile blocks the user
namespaces Chromium's sandbox needs, and the Codery Playwright service runs
with `no-new-privileges` — the browser process would fail to start otherwise.
This is the standard, documented setting for Chromium in CI containers: the
container itself is unprivileged, internal-only (no public route), and runs
no user data. It is a security *flag*, not a library dependency — do not
confuse it with installing Chromium dependencies into the sandbox.

## Networking semantics

- All three containers share `codery-net`. The Playwright service exposes the
  Docker DNS alias **`playwright`**; Apps already exposes **`apps`**.
- `localhost` is container-local:
  - `localhost` in the Sandbox = the Sandbox
  - `localhost` in Playwright/Chromium = the Playwright container
  - `localhost` in Apps = the Apps container
- Chromium must address user applications by DNS identity:
  `http://apps:<port>`. Apps intended for browser access bind to
  `0.0.0.0:<port>` inside the Apps container.
- The Sandbox proxies no application HTTP traffic — it is control-plane only
  (OpenCode → MCP → Playwright server → Chromium).
- `PLAYWRIGHT_WS_ENDPOINT=ws://playwright:3000/` is exported in the sandbox
  for user/test code: `chromium.connect(process.env.PLAYWRIGHT_WS_ENDPOINT)`.

## Operations

- **Deploy/upgrade:** `codery-ci deploy playwright <version>` (host shell) or
  the manual `Deploy Playwright` workflow. No image build — the image is
  pulled from MCR. Blue/green applies as usual; there are no public routes.
- **Rollback:** revert the pins and redeploy (pull-only; `codery-ci rollback`
  works only for GHCR-built images).
- **Inspect:** `docker logs codery-playwright-<color>` on the host;
  `get_status`/`get_container_info` via MCP.

## Troubleshooting

1. **Version mismatch** — first check: does the MCP's bundled `playwright`
   version equal the image tag? Mismatch errors surface in the MCP server
   logs (`read_container_file service='sandbox' path='/tmp/opencode.log'`).
2. **OpenCode reports `playwright: failed — "Failed to get tools"`** — the
   MCP is sending `inputSchema` stubs without `type: "object"`. Check
   `npm view zod version` behavior: this happens when zod 4 resolves instead
   of the pinned zod 3 (`zod-to-json-schema` 3.x cannot read zod 4 schemas).
   Verify the `npm exec --package=zod@3.24.1 ...` pins are intact in
   `opencode.json`, then reconnect via `POST /mcp/playwright/connect` or
   restart `opencode serve`.
3. **WS unreachable** — from the sandbox:
   `node -e "new WebSocket('ws://playwright:3000/')"` or check
   `get_container_info service='playwright'`.
4. **Chromium fails to launch** — check `docker logs` of the Playwright
   container; verify `ipc: host` and `--headless` are in effect. Do **not**
   install libraries into the Sandbox as a fix.
