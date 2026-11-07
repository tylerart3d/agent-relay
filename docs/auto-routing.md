# Auto routing in Agent Relay 2

Each harness has a **Pinned** or **Auto** mode. Existing installations migrate to
Pinned, preserving their routes. No models load during migration or startup.

## Setup

1. Open Settings → Auto routing. Add host/model pairs to the shared approved list
   in preference order. Mark **Tools tested** only after testing that model with
   the harness's tool calls.
2. Choose a harness, enable Auto, and either inherit the shared list or supply
   an independent list. An empty independent list deliberately allows nothing.
3. Set the minimum context to at least the context advertised to your harness.
   Profiles without a declared context size cannot participate in Auto.
4. Save. Existing scoped connections need no endpoint change. Use Configure
   harnesses again for old Pi, Claude Code, Copilot, VS Code, or Hermes CLI
   configurations, which previously used host-qualified model IDs.

All managed clients use model **`agentrelay`**. OpenAI-compatible base URLs are
`http://127.0.0.1:38475/clients/<client>/v1`, with client IDs `hermes`,
`hermes_cli`, `opencode`, `openbot`, `codex`, `pi`, `copilot`, and `vscode`.
OpenCode GUI and CLI share the `opencode` policy. Claude Code uses
`http://127.0.0.1:38475/clients/claude_code` as its Anthropic base URL.
Loopback does not require authentication; a required client key can use
`agentrelay-local`. Direct `/v1` host/model requests remain explicitly pinned.

## Selection and recovery

Relay checks the API type, image capability, tool approval, context capacity,
host availability, and peer routing version. It conservatively estimates text
size from UTF-8 bytes, reserves requested output (8K when unspecified), and
reserves 16K per image plus framing overhead. This is an admission estimate,
not a runtime tokenization guarantee; unsupported media requires a pinned route.

An eligible loaded model wins over a cold model. Within that tier, Relay keeps
the last choice for that client while it remains eligible, then uses approval
order. This is **client affinity, not conversation identity**: independent
chats in one harness share a routing policy. No conversation content is stored
or transferred by Auto, and KV caches are not portable between different models.

Cold starts require no foreground model (including one still starting) and no
active foreground requests on the target. CPU embedding services may continue
alongside the new model. The target checks again under its lifecycle lock. Auto does not
evict another loaded model. It may try another candidate after a readiness
failure, before submitting inference. Once it sends the prompt, Relay does not
automatically replay a failed or partially streamed response. Streaming remains
unbuffered. An empty or exhausted candidate list returns HTTP 503 with
`no_eligible_model` and an explanation.

## Machine availability

The **On/Off** switch beside each host persists on that host. Off blocks new
inference and model loads, including forced CLI loads. Already admitted work
finishes normally; switching off does not unload its model. Unload remains an
explicit action. A second Relay GUI can change the switch remotely. There is no
availability mutation in `agentrelayctl` or the loopback management API.

This is an operator workflow boundary, not authorization against malicious code:
the existing trusted peer control network has no independent human identity.
Peers advertise `routing_version: 2`; older peers can still use pinned routes
but cannot participate in Auto or remote availability control.

## Version 1 retirement

Version 2 removes Photon adapters, gateway packaging, message-route UI, channel
commands, and channel persistence code. Native harness applications own their
conversations. Old gateway configuration is discarded when migrating
`fleet.json`; old channel databases and OS credential entries are left inert,
not opened or deleted. Stop an independently launched legacy gateway before
upgrading. NVIDIA routing components are not required for this deterministic
availability policy.
