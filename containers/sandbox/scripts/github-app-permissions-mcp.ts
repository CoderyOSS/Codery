#!/usr/bin/env bun

const PEM_PATH = "/run/secrets/github-app.pem";
const APP_ID = process.env.GITHUB_APP_ID;
const API_BASE = "https://api.github.com";

let jwtCache: { token: string; expiresAt: number } | null = null;

function b64url(data: Buffer): string {
  return data.toString("base64url");
}

function generateJWT(): string {
  const now = Math.floor(Date.now() / 1000);
  if (jwtCache && jwtCache.expiresAt > now + 60) {
    return jwtCache.token;
  }

  const header = b64url(Buffer.from(JSON.stringify({ alg: "RS256", typ: "JWT" })));
  const payload = b64url(Buffer.from(JSON.stringify({ iat: now - 60, exp: now + 540, iss: APP_ID })));
  const signingInput = `${header}.${payload}`;

  const proc = Bun.spawnSync({
    cmd: ["openssl", "dgst", "-sha256", "-sign", PEM_PATH],
    stdin: Buffer.from(signingInput, "utf-8"),
  });

  if (proc.exitCode !== 0) {
    const errMsg = new TextDecoder().decode(proc.stderr);
    throw new Error(`openssl failed: ${errMsg}`);
  }

  const sig = b64url(proc.stdout);
  const jwt = `${signingInput}.${sig}`;

  jwtCache = { token: jwt, expiresAt: now + 480 };
  return jwt;
}

async function githubAPI(
  path: string,
  options: RequestInit = {},
): Promise<unknown> {
  const jwt = generateJWT();
  const res = await fetch(`${API_BASE}${path}`, {
    ...options,
    headers: {
      Authorization: `Bearer ${jwt}`,
      Accept: "application/vnd.github+json",
      "X-GitHub-Api-Version": "2022-11-28",
      ...(options.headers as Record<string, string> | undefined),
    },
  });

  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`GitHub API ${path}: ${res.status} ${res.statusText} — ${body.slice(0, 200)}`);
  }

  return res.json();
}

async function getInstallations() {
  const data = (await githubAPI("/app/installations")) as Array<Record<string, unknown>>;
  return data.map((inst: any) => ({
    id: inst.id,
    account: { login: inst.account.login, type: inst.account.type },
    permissions: inst.permissions,
    events: inst.events || [],
    repository_selection: inst.repository_selection,
    suspended: inst.suspended_at != null,
    suspended_at: inst.suspended_at,
    created_at: inst.created_at,
    updated_at: inst.updated_at,
  }));
}

async function getInstallation(accountName: string) {
  const all = await getInstallations();
  return (
    all.find(
      (inst) => inst.account.login.toLowerCase() === accountName.toLowerCase(),
    ) || null
  );
}

async function checkPermission(account: string, permission: string) {
  const inst = await getInstallation(account);
  if (!inst) {
    return {
      account,
      permission,
      found: false,
      has_permission: false,
      level: null,
      error: `No installation found for account '${account}'`,
    };
  }
  const level = inst.permissions[permission];
  return {
    account: inst.account.login,
    permission,
    has_permission: level != null,
    level: level || null,
  };
}

function send(id: number | string, payload: Record<string, unknown>) {
  const msg = JSON.stringify({ jsonrpc: "2.0", id, ...payload });
  process.stdout.write(msg + "\n");
}

function sendResult(id: number | string, result: unknown) {
  send(id, { result });
}

function sendError(id: number | string, code: number, message: string) {
  send(id, { error: { code, message } });
}

const TOOLS = [
  {
    name: "get_installations",
    description:
      "List all GitHub App installations with full permissions, events, and metadata across all orgs",
    inputSchema: { type: "object", properties: {} },
  },
  {
    name: "get_installation",
    description: "Get a single installation by account (org or user) name",
    inputSchema: {
      type: "object",
      properties: {
        account: {
          type: "string",
          description: "Org or user login name",
        },
      },
      required: ["account"],
    },
  },
  {
    name: "check_permission",
    description:
      "Check if the GitHub App has a specific permission on an account. Use this before making claims about what the bot can or cannot do.",
    inputSchema: {
      type: "object",
      properties: {
        account: {
          type: "string",
          description: "Org or user login name",
        },
        permission: {
          type: "string",
          description:
            "Permission to check, e.g. 'actions', 'contents', 'workflows', 'issues', 'pull_requests', 'metadata', 'administration', 'secrets', 'pages'",
        },
      },
      required: ["account", "permission"],
    },
  },
];

async function handleMessage(msg: any) {
  const { id, method, params } = msg;

  if (method === "initialize") {
    return sendResult(id, {
      protocolVersion: "2024-11-05",
      capabilities: { tools: {} },
      serverInfo: { name: "github-app-permissions", version: "1.0.0" },
    });
  }

  if (method === "notifications/initialized") {
    return;
  }

  if (method === "tools/list") {
    return sendResult(id, { tools: TOOLS });
  }

  if (method === "tools/call") {
    const { name, arguments: args } = params || {};
    try {
      let result: unknown;
      switch (name) {
        case "get_installations":
          result = await getInstallations();
          break;
        case "get_installation":
          result = await getInstallation(args.account);
          break;
        case "check_permission":
          result = await checkPermission(args.account, args.permission);
          break;
        default:
          return sendError(id, -32601, `Unknown tool: ${name}`);
      }
      sendResult(id, {
        content: [{ type: "text", text: JSON.stringify(result, null, 2) }],
      });
    } catch (err: any) {
      sendError(id, -32000, err.message || String(err));
    }
    return;
  }

  sendError(id, -32601, `Unknown method: ${method}`);
}

async function main() {
  if (!APP_ID) {
    console.error("[github-app-mcp] GITHUB_APP_ID not set");
    process.exit(1);
  }

  const reader = Bun.stdin.stream().getReader();
  let buf = "";

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += new TextDecoder().decode(value);

    while (true) {
      // Content-Length framing (MCP spec standard)
      const m = buf.match(/Content-Length: (\d+)\r\n\r\n/);
      if (m) {
        const length = parseInt(m[1]);
        const headerEnd = m.index! + m[0].length;
        if (buf.length < headerEnd + length) break;
        const body = buf.slice(headerEnd, headerEnd + length);
        buf = buf.slice(headerEnd + length);
        await processMessage(body);
        continue;
      }

      // Newline-delimited JSON (opencode format)
      const nl = buf.indexOf("\n");
      if (nl >= 0) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (line.length > 0) {
          await processMessage(line);
        }
        continue;
      }

      break;
    }
  }
}

async function processMessage(raw: string) {
  try {
    const msg = JSON.parse(raw);
    await handleMessage(msg);
  } catch (err: any) {
    console.error("[github-app-mcp] Parse error:", err.message);
  }
}

main().catch((err) => {
  console.error("[github-app-mcp] Fatal:", err.message);
  process.exit(1);
});
