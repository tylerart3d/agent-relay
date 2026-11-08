// OpenCode 2's native plugin contract. Plugin.define is an identity helper;
// exporting the contract directly avoids downloading an SDK at app startup.
export const COMPACTION_OUTPUT_FLOOR = 8192;
export const COMPACTION_INSTRUCTION = "Write only a concise continuation summary, at most 1200 words. Preserve the user goal, constraints, decisions, completed work, unresolved issues, and next actions. Refer to large tool outputs or files by short descriptions instead of copying them. Do not repeat earlier sections, continue the task, or call tools. Finish the summary within this length.";

export async function guardCompaction(event) {
  if (event.kind !== "compaction" || event.model?.providerID !== "agentrelay") return;
  if (!isRelayRequest(event.request)) return;
  const request = event.request;

  const body = await request.clone().json();
  if (!body || typeof body !== "object" || Array.isArray(body)) return;
  const tools = Array.isArray(body.tools) && body.tools.length > 0;
  const functions = Array.isArray(body.functions) && body.functions.length > 0;
  // OpenCode compaction can impose a 4096-token cap independently of the
  // provider's advertised output limit. Leave room for reasoning + a summary.
  let hasLimit = false;
  for (const key of ["max_tokens", "max_completion_tokens"]) {
    if (Number.isSafeInteger(body[key]) && body[key] > 0) {
      body[key] = Math.max(body[key], COMPACTION_OUTPUT_FLOOR);
      hasLimit = true;
    }
  }
  if (!hasLimit) body.max_tokens = COMPACTION_OUTPUT_FLOOR;
  if (Array.isArray(body.messages)
      && body.messages.at(-1)?.content !== COMPACTION_INSTRUCTION) {
    body.messages.push({ role: "user", content: COMPACTION_INSTRUCTION });
  }
  // Keep definitions and history for template validity and prefix-cache reuse.
  // Only auxiliary summary calls lose permission to generate NEW tool calls.
  if (tools) body.tool_choice = "none";
  if (functions) body.function_call = "none";
  const headers = new Headers(request.headers);
  headers.delete("content-length");
  event.request = new Request(request, { headers, body: JSON.stringify(body) });
}

function isRelayRequest(request) {
  const url = new URL(request.url);
  return request.method === "POST" && url.protocol === "http:" && url.port === "38475"
    && ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname)
    && /^\/clients\/(openbot|opencode)\/v1\/chat\/completions$/.test(url.pathname);
}

// Session IDs remain request-only: Relay keeps a bounded opaque affinity key in memory,
// strips this header before forwarding, and never puts it in operational telemetry.
export async function tagRelaySession(event) {
  if (event.model?.providerID !== "agentrelay" || !isRelayRequest(event.request)) return;
  if (typeof event.sessionID !== "string" || !/^[a-zA-Z0-9_-]{1,128}$/.test(event.sessionID)) return;
  const headers = new Headers(event.request.headers);
  headers.set("x-agentrelay-session", event.sessionID);
  event.request = new Request(event.request, { headers });
}

export default {
  id: "agentrelay.compaction",
  async setup(ctx) {
    await ctx.session.hook("http.request", guardCompaction, { providerID: "agentrelay" });
    await ctx.session.hook("http.request", tagRelaySession, { providerID: "agentrelay" });
  },
};
