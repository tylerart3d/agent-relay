# OpenCode compaction compatibility

Native OpenCode 2 plugin for Agent Relay's OpenCode and OpenBot connectors.
OpenBot itself and its bundled OpenCode executable remain unmodified.

OpenCode 2.2.6 can send tools with compaction requests without disabling new
tool calls. Some Qwen replies then contain reasoning and a tool call but no
summary text, producing `Compaction produced no summary`.
See [upstream issue #53242](https://github.com/anomalyco/opencode/issues/53242)
and [proposed fix #53370](https://github.com/anomalyco/opencode/pull/53370).

## Behavior

The native `http.request` hook checks OpenCode's explicit `kind: compaction`,
provider `agentrelay`, and the local Relay OpenCode/OpenBot chat-completions URL.
It sets `tool_choice: none` (or legacy `function_call: none`) while retaining
tool definitions, conversation history, and thinking settings.
Plugin 1.2.0 also raises compaction-only `max_tokens` / `max_completion_tokens`
to at least 8192, preserving larger budgets. It appends one short instruction to
summarize within 1200 words and omit raw tool-output dumps. The extra room covers
reasoning plus summary text; the word target encourages a natural stop before
the hard limit. Normal chat limits are unchanged. The hook is idempotent on retry.
It removes a stale content-length header when replacing the request body.
Normal agent turn bodies, other providers, and response streams are untouched.
No prompts, results, or conversation identifiers are logged or stored.

This does not repair existing summaries or automatically retry failed tasks.
It does not guarantee that every model produces a correctly formatted summary,
nor recover a reply containing only reasoning. Responses and WebSocket protocols
are outside this narrowly scoped chat-completions workaround.

## Conversation affinity (plugin 1.1.0)

A second native `http.request` hook adds `X-AgentRelay-Session` using the hook's
session ID, only for the local Agent Relay OpenCode/OpenBot endpoint. Agent Relay
2.2.0 hashes this bounded hint into an ephemeral in-memory routing preference;
it strips the header before forwarding and never records it in telemetry.
This uses the [documented native session hook](https://opencode.ai/docs/build/plugins/server#native-http),
not an OpenBot fork. Missing or invalid hints simply leave affinity disabled.
The hint does not transfer chat history, override model approval, or guarantee
that a conversation stays on a busy host. It does not reset OpenBot's timeout.

## Install

Run `opencode debug paths` using the executable that actually serves the client.
Copy `index.js` and `package.json` into
`<config directory>/plugins/agentrelay-compaction/`. No dependency download is
required. The OpenCode watcher reloads this directory; a fresh location can take
one registry read to initialize. Query `opencode api plugin.list` and verify
`agentrelay.compaction` is active. `opencode plugin list` may only show configured
package entries rather than this automatically discovered local plugin.

Install on the machine hosting OpenBot's OpenCode service. A Mac viewing remote
WORKSTATION bots does not need its own copy. For local Mac OpenCode sessions,
install under the Mac's resolved OpenCode config directory too.

To remove the workaround, remove only its plugin directory and let the watcher
reload. Reassess it once the upstream fix ships. See the official
[native HTTP hook documentation](https://opencode.ai/docs/build/plugins/server#native-http).

## Verify

Run `npm test --prefix integrations/opencode-compaction` from the Relay repository.
Tests cover compaction-only scope, preserved history and settings, unrelated
providers/endpoints, empty-tool requests, legacy functions, and hook registration.
Test native compaction in a disposable conversation before retrying a real task;
deny all tool permissions in that test and never replay real tools for validation.

The original compaction guard was validated with OpenBot's bundled OpenCode 2.2.6 on Windows and Agent Relay 2.1.1
routing to an M1 Qwen vision profile. All five regression tests passed. A native,
disposable OpenCode conversation compacted successfully into 578 characters
(275 generated tokens), then was removed. The affected real conversation's
location reported the plugin active. The real failed conversation was not
retried or edited; the original failed responses were not retained, so their
precise reasoning-versus-tool-call split could not be established.

## Output-limit regression (1.2.0)

A production compaction reported `Compaction summary reached the output token
limit` with exactly 4096 output tokens and approximately 51.7K input/cache tokens.
The installed 1.0.0 tool guard preserved that cap; the uninstalled Relay 2.2.0
build could not have introduced it. Raising the summary allowance and asking for
a concise summary addresses this failure separately from tool-call-only replies.
Seven automated tests cover both token-limit fields, larger budgets, no-tool
compactions, idempotency, session hints, and unchanged ordinary requests.
No truncated output is relabeled as successful. Real failed chats are not retried.

The 1.2.0 native smoke test used a disposable conversation containing synthetic
inventory decisions with all tool permissions denied. Compaction completed with
5918 input/cache tokens, 473 output tokens, and a 1395-character summary. Only the
test session was removed. This validates the native path, not the contents of the
failed production summary; that conversation remains untouched for the user to
retry. Larger or pathological outputs can still hit a finite limit.
