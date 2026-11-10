/**
 * Codery thin Dockhand stack MCP — stdio, zero dependencies (Bun).
 *
 * Exposes a deliberately NARROW surface to agents: stack list/ps/logs/sync/
 * restart/health. No terminal, no exec, no file-browser, no container
 * lifecycle beyond stack restart. Dockhand's own UI/API remains the
 * operator surface (Rob / OpenMinis).
 *
 * Config (container env, see paseo service.yml env_overrides at migration
 * time):
 *   DOCKHAND_URL       default http://dockhand:3000 (codery-net alias)
 *   DOCKHAND_API_TOKEN required; bearer token minted in Dockhand (dh_...)
 *   DOCKHAND_ENV       default "1" (Dockhand local environment id)
 *
 * Missing token => every tool call returns a remediation message; the MCP
 * still lists its tools so the agent knows the sanctioned path exists.
 */
import { createInterface } from "node:readline";

const BASE = (process.env.DOCKHAND_URL ?? "http://dockhand:3000").replace(/\/$/, "");
const ENV_ID = process.env.DOCKHAND_ENV ?? "1";
const TOKEN = process.env.DOCKHAND_API_TOKEN ?? "";

interface RpcRequest {
  jsonrpc: "2.0";
  id: number | string | null;
  method: string;
  params?: any;
}

function send(msg: any) {
  process.stdout.write(JSON.stringify(msg) + "\n");
}

function tool(name: string, description: string, inputSchema: any) {
  return { name, description, inputSchema };
}

const TOOLS = [
  tool(
    "dockhand_list_stacks",
    "List all Dockhand compose stacks with state. Use this to see what is running and what a stack's containers are.",
    { type: "object", properties: {}, additionalProperties: false },
  ),
  tool(
    "dockhand_stack_ps",
    "List the containers belonging to one stack (name, image, state, ports).",
    {
      type: "object",
      properties: { stack: { type: "string", description: "Stack name (from dockhand_list_stacks)" } },
      required: ["stack"],
      additionalProperties: false,
    },
  ),
  tool(
    "dockhand_stack_logs",
    "Fetch recent log lines for a stack container.",
    {
      type: "object",
      properties: {
        stack: { type: "string" },
        container: { type: "string", description: "Optional container name/id; defaults to the stack's first container" },
        tail: { type: "integer", description: "Line count, default 100" },
      },
      required: ["stack"],
      additionalProperties: false,
    },
  ),
  tool(
    "dockhand_stack_sync",
    "Redeploy a stack from its compose file on disk (Dockhand deploy: pull + up). Use after editing the project's docker-compose.yml. Never run while a switch policy forbids it (sessions inactive only).",
    {
      type: "object",
      properties: {
        stack: { type: "string" },
        build: { type: "boolean", description: "Also run compose build (for stacks with build: sections). Default false." },
        force_recreate: { type: "boolean", description: "Force container recreation. Default false." },
      },
      required: ["stack"],
      additionalProperties: false,
    },
  ),
  tool(
    "dockhand_stack_restart",
    "Restart a stack's containers (no pull, no recreate).",
    {
      type: "object",
      properties: { stack: { type: "string" } },
      required: ["stack"],
      additionalProperties: false,
    },
  ),
  tool(
    "dockhand_stack_health",
    "Aggregate health: stack states + per-container running/exited + the Dockhand /api/health probe.",
    { type: "object", properties: {}, additionalProperties: false },
  ),
];

const REMEDIATION =
  "DOCKHAND_API_TOKEN is not present in this container's environment. " +
  "It is wired by an env_override in the paseo service.yml at migration time " +
  "(see docs/runbook/openminis-migration.md phase 0). Until then, ask the " +
  "operator to use the Dockhand UI or API directly.";

function headers(): Record<string, string> {
  return { "Authorization": `Bearer ${TOKEN}`, "Content-Type": "application/json" };
}

async function api(path: string, init?: RequestInit): Promise<Response> {
  return fetch(`${BASE}/api${path}`, { ...init, headers: { ...headers(), ...(init?.headers ?? {}) } });
}

function tokenGuard(): string | null {
  if (!TOKEN) return REMEDIATION;
  return null;
}

async function listStacks(): Promise<any> {
  const r = await api(`/stacks?env=${ENV_ID}`);
  if (!r.ok) throw new Error(`GET /stacks ${r.status}: ${await r.text()}`);
  return r.json();
}

async function listContainers(): Promise<any> {
  const r = await api(`/containers?env=${ENV_ID}`);
  if (!r.ok) throw new Error(`GET /containers ${r.status}: ${await r.text()}`);
  return r.json();
}

function matchStack(containers: any, stack: string): any[] {
  const arr = Array.isArray(containers) ? containers : containers?.containers ?? [];
  const needle = stack.toLowerCase();
  return arr.filter((c: any) =>
    (c.stack ?? c.stackName ?? c.composeProject ?? "").toLowerCase() === needle ||
    (c.name ?? c.names?.[0] ?? "").toLowerCase().includes(needle),
  );
}

async function callTool(name: string, args: any): Promise<any> {
  const guard = tokenGuard();
  if (guard) return { content: [{ type: "text", text: guard }], isError: true };
  const q = `?env=${ENV_ID}`;
  switch (name) {
    case "dockhand_list_stacks": {
      const stacks = await listStacks();
      return { content: [{ type: "text", text: JSON.stringify(stacks, null, 2) }] };
    }
    case "dockhand_stack_ps": {
      const all = await listContainers();
      const mine = matchStack(all, args.stack);
      return { content: [{ type: "text", text: JSON.stringify(mine, null, 2) }] };
    }
    case "dockhand_stack_logs": {
      const all = await listContainers();
      let target = args.container
        ? matchStack(all, args.stack).find((c: any) => (c.name ?? "").includes(args.container))
        : matchStack(all, args.stack)[0];
      if (!target) throw new Error(`No container found for stack '${args.stack}'`);
      const id = target.id ?? target.Id ?? target.name;
      const tail = args.tail ?? 100;
      const r = await api(`/containers/${id}/logs?env=${ENV_ID}&tail=${tail}`);
      if (!r.ok) throw new Error(`GET logs ${r.status}: ${await r.text()}`);
      return { content: [{ type: "text", text: await r.text() }] };
    }
    case "dockhand_stack_sync": {
      const body: any = { pull: true, build: !!args.build, respectPullPolicy: false, pullBase: false, forceRecreate: !!args.force_recreate };
      const r = await api(`/stacks/${encodeURIComponent(args.stack)}/deploy${q}`, { method: "POST", body: JSON.stringify(body) });
      if (!r.ok) throw new Error(`POST deploy ${r.status}: ${await r.text()}`);
      return { content: [{ type: "text", text: `Deploy of '${args.stack}' accepted: ${await r.text()}` }] };
    }
    case "dockhand_stack_restart": {
      const r = await api(`/stacks/${encodeURIComponent(args.stack)}/restart${q}`, { method: "POST" });
      if (!r.ok) throw new Error(`POST restart ${r.status}: ${await r.text()}`);
      return { content: [{ type: "text", text: `Restart of '${args.stack}' accepted: ${await r.text()}` }] };
    }
    case "dockhand_stack_health": {
      const [stacks, containers] = await Promise.all([listStacks(), listContainers()]);
      const probe = await fetch(`${BASE}/api/health`).then((r) => `dockhand /api/health: ${r.status}`).catch((e) => `dockhand /api/health: unreachable (${e.message})`);
      return { content: [{ type: "text", text: JSON.stringify({ probe, stacks, containers: Array.isArray(containers) ? containers.map((c: any) => ({ name: c.name, state: c.state ?? c.status, stack: c.stack ?? c.stackName })) : containers }, null, 2) }] };
    }
    default:
      throw new Error(`Unknown tool ${name}`);
  }
}

const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
rl.on("line", (line) => {
  if (!line.trim()) return;
  let req: RpcRequest;
  try {
    req = JSON.parse(line);
  } catch {
    return;
  }
  void (async () => {
    try {
      if (req.method === "initialize") {
        send({
          jsonrpc: "2.0", id: req.id, result: {
            protocolVersion: "2024-11-05",
            capabilities: { tools: {} },
            serverInfo: { name: "codery-dockhand-stack-mcp", version: "0.1.0" },
          },
        });
      } else if (req.method === "notifications/initialized" || req.method?.startsWith("notifications/")) {
        // notifications need no response
      } else if (req.method === "tools/list") {
        send({ jsonrpc: "2.0", id: req.id, result: { tools: TOOLS } });
      } else if (req.method === "tools/call") {
        try {
          const result = await callTool(req.params.name, req.params.arguments ?? {});
          send({ jsonrpc: "2.0", id: req.id, result });
        } catch (e: any) {
          send({ jsonrpc: "2.0", id: req.id, result: { content: [{ type: "text", text: `Dockhand MCP error: ${e.message}` }], isError: true } });
        }
      } else {
        send({ jsonrpc: "2.0", id: req.id, error: { code: -32601, message: `Method not found: ${req.method}` } });
      }
    } catch (e: any) {
      send({ jsonrpc: "2.0", id: req.id, error: { code: -32603, message: e.message } });
    }
  })();
});
