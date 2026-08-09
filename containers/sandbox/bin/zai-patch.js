// Monkey-patches global fetch to inject thinking:{type:"disabled"} into Z.ai
// coding-plan requests. Loaded via NODE_OPTIONS="--require /home/gem/bin/zai-patch.js"
//
// Why: GLM-5.x emits reasoning_content by default. LangChain's stream
// aggregator + deepagents' patchToolCallsMiddleware choke on it.
//
// Auto-loaded for all node processes via sandbox service.yml env_overrides
// (NODE_OPTIONS). Harmless for non-Z.ai traffic: only modifies POSTs whose
// body has a "model" field and whose URL is z.ai.

const originalFetch = globalThis.fetch;
globalThis.fetch = async function patchedFetch(input, init) {
  try {
    const url = typeof input === "string" ? input : input?.url ?? "";
    const isZai = url.includes("z.ai") || url.includes("localhost:8234");
    if (isZai &&
        init?.method?.toUpperCase() === "POST" &&
        typeof init.body === "string" &&
        init.body.includes('"model"')) {
      const json = JSON.parse(init.body);
      if (json && typeof json === "object" && json.thinking === undefined) {
        json.thinking = { type: "disabled" };
        init = { ...init, body: JSON.stringify(json) };
      }
    }
  } catch {
    // Fall through with original init on any parse error.
  }
  return originalFetch.call(this, input, init);
};
