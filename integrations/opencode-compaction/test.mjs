import assert from "node:assert/strict";
import { test } from "node:test";
import plugin, { guardCompaction } from "./index.js";

const payload = {
  model: "agentrelay", stream: true, max_tokens: 8192, reasoning_effort: "low",
  messages: [{ role: "tool", tool_call_id: "synthetic", content: "Synthetic result" }],
  tools: [{ type: "function", function: { name: "lookup", parameters: { type: "object" } } }],
  tool_choice: "auto",
};
const event = (kind = "compaction", url = "http://127.0.0.1:38475/clients/openbot/v1/chat/completions", body = payload) => ({
  kind, model: { providerID: "agentrelay", id: "agentrelay" },
  request: new Request(url, { method: "POST", headers: { "content-type": "application/json", "content-length": "999" }, body: JSON.stringify(body) }),
});

test("compaction prohibits new tool calls while preserving history, streaming and controls", async () => {
  const e = event(); const original = e.request;
  await guardCompaction(e);
  assert.notEqual(e.request, original);
  assert.deepEqual(await e.request.json(), { ...payload, tool_choice: "none" });
  assert.deepEqual(await original.json(), payload);
  assert.equal(e.request.headers.has("content-length"), false);
});

test("ordinary agent turns, other providers and unrelated endpoints remain untouched", async () => {
  for (const e of [event("primary"), event("title"), event("generate"),
    event("compaction", "http://example.com:38475/clients/openbot/v1/chat/completions"),
    event("compaction", "http://localhost:38475/clients/hermes/v1/chat/completions"),
    event("compaction", "http://localhost:38475/clients/openbot/v1/responses"),
    { ...event(), model: { providerID: "other" } }]) {
    const original = e.request;
    await guardCompaction(e);
    assert.equal(e.request, original);
    assert.equal(original.bodyUsed, false);
  }
});

test("OpenCode route and legacy functions also disable calls only for compaction", async () => {
  const body = { ...payload, functions: [{ name: "lookup" }], function_call: "auto" };
  const e = event("compaction", "http://localhost:38475/clients/opencode/v1/chat/completions", body);
  await guardCompaction(e);
  assert.deepEqual(await e.request.json(), { ...body, tool_choice: "none", function_call: "none" });
});

test("requests with no tools are not given an unsupported tool-choice field", async () => {
  const e = event("compaction", undefined, { model: "agentrelay", messages: [] });
  const original = e.request;
  await guardCompaction(e);
  assert.equal(e.request, original);
});

test("registers the native HTTP hook scoped to the Relay provider", async () => {
  const calls = [];
  await plugin.setup({ session: { hook: async (...args) => calls.push(args) } });
  assert.deepEqual(calls, [["http.request", guardCompaction, { providerID: "agentrelay" }]]);
});
