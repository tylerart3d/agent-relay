import assert from "node:assert/strict";
import { test } from "node:test";
import plugin, { guardCompaction, tagRelaySession, COMPACTION_INSTRUCTION } from "./index.js";

const payload = {
  model: "agentrelay", stream: true, max_tokens: 8192, reasoning_effort: "low",
  messages: [{ role: "tool", tool_call_id: "synthetic", content: "Synthetic result" }],
  tools: [{ type: "function", function: { name: "lookup", parameters: { type: "object" } } }],
  tool_choice: "auto",
};
const summaryMessages = [...payload.messages, { role: "user", content: COMPACTION_INSTRUCTION }];
const event = (kind = "compaction", url = "http://127.0.0.1:38475/clients/openbot/v1/chat/completions", body = payload) => ({
  kind, model: { providerID: "agentrelay", id: "agentrelay" },
  request: new Request(url, { method: "POST", headers: { "content-type": "application/json", "content-length": "999" }, body: JSON.stringify(body) }),
});

test("compaction prohibits new tool calls while preserving history, streaming and controls", async () => {
  const e = event(); const original = e.request;
  await guardCompaction(e);
  assert.notEqual(e.request, original);
  assert.deepEqual(await e.request.json(), { ...payload, messages: summaryMessages, tool_choice: "none" });
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
  assert.deepEqual(await e.request.json(), { ...body, messages: summaryMessages, tool_choice: "none", function_call: "none" });
});

test("requests with no tools are not given an unsupported tool-choice field", async () => {
  const e = event("compaction", undefined, { model: "agentrelay", messages: [] });
  const original = e.request;
  await guardCompaction(e);
  assert.notEqual(e.request, original);
  const body = await e.request.json();
  assert.equal(body.max_tokens, 8192);
  assert.equal(body.tool_choice, undefined);
});

test("registers the native HTTP hook scoped to the Relay provider", async () => {
  const calls = [];
  await plugin.setup({ session: { hook: async (...args) => calls.push(args) } });
  assert.deepEqual(calls, [["http.request", guardCompaction, { providerID: "agentrelay" }], ["http.request", tagRelaySession, { providerID: "agentrelay" }]]);
});


test("session affinity tags native requests without changing model input", async () => {
  const e = { ...event("primary"), sessionID: "session_synthetic" };
  await tagRelaySession(e);
  assert.equal(e.request.headers.get("x-agentrelay-session"), "session_synthetic");
  assert.deepEqual(await e.request.json(), payload);
  for (const e of [{...event(), sessionID: "bad\nheader"}, {...event(), sessionID: "x".repeat(129)},
      {...event("primary", "http://example.com:38475/clients/openbot/v1/chat/completions"),sessionID:"synthetic"}]) {
    const old = e.request; await tagRelaySession(e); assert.equal(e.request, old);
  }
});


test("compaction gets summary headroom without lowering larger caps or multiplying instructions", async () => {
  for (const limits of [{max_tokens:4096}, {max_completion_tokens:4096}, {max_tokens:16384}, {max_tokens:4096,max_completion_tokens:4096}]) {
    const body = {model:"agentrelay",stream:true,messages:[{role:"user",content:"Synthetic history"}], ...limits};
    const e = event("compaction",undefined,body);
    await guardCompaction(e);
    const once = await e.request.clone().json();
    for (const [key,value] of Object.entries(limits)) assert.equal(once[key],Math.max(value,8192));
    assert.deepEqual(once.messages.slice(0,-1),body.messages);
    assert.equal(once.messages.at(-1).content,COMPACTION_INSTRUCTION);
    await guardCompaction(e);
    assert.deepEqual(await e.request.json(),once);
    const normal = event("primary",undefined,body);
    await guardCompaction(normal);
    assert.deepEqual(await normal.request.json(),body);
  }
});
