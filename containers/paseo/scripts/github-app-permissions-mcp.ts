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
  token?: string,
): Promise<unknown> {
  const authToken = token || generateJWT();
  const res = await fetch(`${API_BASE}${path}`, {
    ...options,
    headers: {
      Authorization: `Bearer ${authToken}`,
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
    note:
      level != null
        ? "Permission exists. To use it: generate an org-scoped token via generate_token({account}), then auth your client with it. GitHub App installation tokens are scoped to ONE installation — cross-org operations need explicit per-org token generation."
        : null,
  };
}

async function generateInstallationToken(account: string): Promise<{
  token: string;
  expires_at: string;
  account: string;
  installation_id: number;
}> {
  const inst = await getInstallation(account);
  if (!inst) {
    throw new Error(`No installation found for account '${account}'`);
  }

  const jwt = generateJWT();
  const data = (await githubAPI(
    `/app/installations/${inst.id}/access_tokens`,
    { method: "POST" },
  )) as { token: string; expires_at: string };

  return {
    token: data.token,
    expires_at: data.expires_at,
    account: inst.account.login,
    installation_id: inst.id,
  };
}

async function createRepo(
  org: string,
  name: string,
  options: { private?: boolean; description?: string; auto_init?: boolean } = {},
) {
  const inst = await getInstallation(org);
  if (!inst) {
    throw new Error(`No installation found for org '${org}'`);
  }

  const jwt = generateJWT();
  const tokenData = (await githubAPI(
    `/app/installations/${inst.id}/access_tokens`,
    { method: "POST" },
  )) as { token: string; expires_at: string };

  const repo = (await githubAPI(
    `/orgs/${org}/repos`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        name,
        private: options.private ?? false,
        description: options.description,
        auto_init: options.auto_init ?? false,
      }),
    },
    tokenData.token,
  )) as { full_name: string; html_url: string; clone_url: string; ssh_url: string };

  return {
    full_name: repo.full_name,
    html_url: repo.html_url,
    clone_url: repo.clone_url,
    ssh_url: repo.ssh_url,
    created: true,
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
      "Check if the GitHub App has a specific permission on an account. Returns a note about token scoping — see generate_token to get an org-specific token.",
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
  {
    name: "generate_token",
    description:
      "Generate an installation access token scoped to a specific account (org or user). Use this before making API calls or gh CLI operations targeting a specific org. Token expires in ~1 hour. Each GitHub App installation is separate — cross-org operations need per-org tokens.",
    inputSchema: {
      type: "object",
      properties: {
        account: {
          type: "string",
          description: "Org or user login name to generate a token for",
        },
      },
      required: ["account"],
    },
  },
  {
    name: "create_repo",
    description:
      "Create a new repository under an organization. Uses the GitHub App to create the repo — generates an org-scoped token automatically. Use this instead of gh repo create, which may use a token for the wrong installation.",
    inputSchema: {
      type: "object",
      properties: {
        org: {
          type: "string",
          description: "Organization name to create the repo under",
        },
        name: {
          type: "string",
          description: "Repository name",
        },
        private: {
          type: "boolean",
          description: "Make the repository private (default: false)",
        },
        description: {
          type: "string",
          description: "Short description of the repository",
        },
        auto_init: {
          type: "boolean",
          description: "Create an initial commit with empty README (default: false)",
        },
      },
      required: ["org", "name"],
    },
  },
  {
    name: "cut_release",
    description:
      "Bump the version in Cargo.toml, commit+push to main, then trigger the build workflow.\n\n" +
      "BEFORE calling this tool: review git commits since the last release tag (trailhead-service-v*) " +
      "to determine the correct semver bump type. Use the GitHub API or git log to inspect recent changes.",
    inputSchema: {
      type: "object",
      properties: {
        account: {
          type: "string",
          description: "GitHub org or user owning the repo (e.g. 'CoderyOSS')",
        },
        repo: {
          type: "string",
          description: "Repository name (e.g. 'Trailhead')",
        },
        bump: {
          type: "string",
          description:
            "Semver bump type. Choose based on changes since the last release:\n" +
            "- 'patch': bug fixes, docs, config changes, dependency updates — no new user-visible features\n" +
            "- 'minor': new features, new API endpoints, new MCP tools — backward compatible\n" +
            "- 'major': breaking changes, removed endpoints, incompatible schema/API changes",
        },
        message: {
          type: "string",
          description: "Short commit message summarising what is being released (used as the git commit message)",
        },
      },
      required: ["account", "repo", "bump", "message"],
    },
  },
];

async function cutRelease(
  account: string,
  repo: string,
  bump: string,
  message: string,
): Promise<{
  old_version: string;
  new_version: string;
  commit_sha: string;
  triggered: boolean;
}> {
  const { token } = await generateInstallationToken(account);
  const owner = account;

  const filePath = "crates/trailhead-service/Cargo.toml";
  const fileData = (await githubAPI(
    `/repos/${owner}/${repo}/contents/${filePath}`,
    {},
    token,
  )) as { content: string; sha: string };

  const content = Buffer.from(fileData.content.replace(/\n/g, ""), "base64").toString("utf-8");
  const sha = fileData.sha;

  const match = content.match(/^version = "(\d+)\.(\d+)\.(\d+)"/m);
  if (!match) {
    throw new Error('Could not find version = "X.Y.Z" in Cargo.toml');
  }
  let [, major, minor, patch] = match.map(Number);
  const oldVersion = `${major}.${minor}.${patch}`;

  if (bump === "major") { major++; minor = 0; patch = 0; }
  else if (bump === "minor") { minor++; patch = 0; }
  else if (bump === "patch") { patch++; }
  else { throw new Error(`Unknown bump type: ${bump}. Must be major, minor, or patch`); }

  const newVersion = `${major}.${minor}.${patch}`;
  const newContent = content.replace(
    /^version = "\d+\.\d+\.\d+"/m,
    `version = "${newVersion}"`,
  );

  const updateData = (await githubAPI(
    `/repos/${owner}/${repo}/contents/${filePath}`,
    {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        message: `${message}\n\nBumps version ${oldVersion} → ${newVersion}`,
        content: Buffer.from(newContent, "utf-8").toString("base64"),
        sha,
        branch: "main",
      }),
    },
    token,
  )) as { commit: { sha: string } };

  const commitSha = updateData.commit.sha;

  await githubAPI(
    `/repos/${owner}/${repo}/actions/workflows/build.yml/dispatches`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ ref: "main" }),
    },
    token,
  );

  return { old_version: oldVersion, new_version: newVersion, commit_sha: commitSha, triggered: true };
}

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
        case "generate_token":
          result = await generateInstallationToken(args.account);
          break;
        case "create_repo":
          result = await createRepo(args.org, args.name, {
            private: args.private,
            description: args.description,
            auto_init: args.auto_init,
          });
          break;
        case "cut_release":
          result = await cutRelease(args.account, args.repo, args.bump, args.message);
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
