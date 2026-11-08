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
host availability, and peer routing version. Text admission estimates use three
ASCII characters per token, with a byte reservation for non-ASCII text, including
tool-schema keys and JSON framing. Counting every byte as one token falsely
rejected ordinary tool conversations; this was corrected in 2.1.1. Relay reserves
requested output (8K when unspecified), 16K per image, and 2K framing headroom.
Encoded image data is not counted as text. The configured minimum context remains
a hard floor. This is a heuristic, not exact runtime tokenization: unusual text,
templates, or image processing can still exceed capacity. Runtime context errors
are passed back without replaying the request; unsupported media requires a
pinned route.

Auto prefers free loaded slots, then cold-startable hosts, then busy resident
models. Within the free loaded tier it prefers this conversation's last host,
then approval order. The destination reserves capacity atomically before loading
or receiving a prompt, so two Relay instances cannot both take the same slot.
Pinned traffic counts against Auto capacity but retains its runtime queueing.
CPU embedding services remain independent.

Profiles may declare `meta.llamaswap.context_slots` (1–256). Without it Auto
conservatively admits one request. A loaded runtime's `/slots` capacity can
reduce that limit, and observed busy slots count alongside Relay reservations.
This does not change the runtime's launch flags or per-slot context.

The native [OpenCode plugin](../integrations/opencode-compaction/README.md)
adds an `X-AgentRelay-Session` hint for OpenCode and OpenBot conversations.
Relay strips it before forwarding and retains only a bounded opaque key in
memory (256 entries, 30 minutes since last assignment). Clients without a hint
still get capacity-aware routing, without conversation affinity. No conversation
content is stored or transferred, and KV caches are not portable between models.
Affinity is a preference, not a guarantee that two bots always use separate hosts:
a multi-slot host may accept both, or only one approved host may be available.

When all eligible capacity is reserved, requests wait up to 30 seconds, retrying
admission every 500 ms. The bounded queue holds at most 64 requests per initiating
Relay. The tray shows **Waiting for a free model · N queued**; loopback
`GET /api/v1/routing/waiting` exposes only aggregate counts by client. Saturation
returns HTTP 429 `models_busy` with `Retry-After: 5`; a full queue returns
`routing_queue_full`. Cold-start attempts have their own timeout and can extend
the total wait beyond this admission retry window.
Changing policy, capability requirements, or availability is checked again on
each retry. Cancellation releases local leases; unused remote tickets expire
30 seconds after readiness, with best-effort explicit release. Preparation has a
10-minute deadline and pending reservations have a 630-second safety expiry.
Active leases last until the forwarded response completes or is dropped.

Queue status is shown in Relay, not injected into assistant output. OpenBot's
agent-event stall watchdog does not treat SSE comments as model progress. Long
silent prefills can still time out; this update neither modifies OpenBot nor
fabricates progress messages.

Cold starts require no foreground model (including one still starting) and no
active foreground requests on the target. CPU embedding services may continue
alongside the new model. The target checks again under its lifecycle lock. Auto does not
evict another loaded model. It may try another candidate after a readiness
failure, before submitting inference. Once it sends the prompt, Relay does not
automatically replay a failed or partially streamed response. Streaming remains
unbuffered. An empty or exhausted candidate list returns HTTP 503 with
`no_eligible_model` and per-model reasons (up to eight), such as GPU busy, missing
tool approval, or declared context below the admission estimate/minimum. These
diagnostics include configuration metadata only, never prompt or tool-result text.

## Machine availability

On Windows, Auto also avoids machines with sustained GPU activity from other
applications. A background sampler checks GPU engine counters approximately
every 15 seconds. Two samples at 30% or more mark the machine busy; 60 seconds
below 10% clear it. Relay, its supervised model processes, and their descendants
are excluded by process ancestry. Only the aggregate percentage is exposed;
process names and IDs are not persisted or sent to peers.

The tray shows **GPU busy · Auto uses another machine**. This never flips the
operator's On/Off switch, unloads a model, interrupts an admitted reply, or
changes Pinned routes. A model already resident may still occupy GPU memory
until idle expiry or explicit unload. After gaming ends, an eligible loaded M1
model can remain selected through conversation affinity; Auto does not bounce back
just because WORKSTATION becomes eligible again.

Counters cover the Windows GPU engines available to the current user. Failed
samples are unknown, not proof of idle; an old busy hint expires after 90 seconds
without a valid sample. The manual Off switch is the reliable exclusion if
hardware counters are unavailable. macOS peers can consume the Windows hint,
but do not currently collect their own GPU-utilization counters.

The **On/Off** switch beside each host persists on that host. Off blocks new
inference and model loads, including forced CLI loads. Already admitted work
finishes normally; switching off does not unload its model. Unload remains an
explicit action. A second Relay GUI can change the switch remotely. There is no
availability mutation in `agentrelayctl` or the loopback management API.

This is an operator workflow boundary, not authorization against malicious code:
the existing trusted peer control network has no independent human identity.
Peers advertise `routing_version: 3` for destination-owned reservations.
Upgrade both the initiating Relay and inference hosts to 2.2.0 before using Auto.
Older peers remain usable with Pinned routes; a version-2 origin cannot submit
its old unreserved Auto requests to a version-3 destination. Availability control
remains compatible with version-2 peers.

## Version 1 retirement

Version 2 removes Photon adapters, gateway packaging, message-route UI, channel
commands, and channel persistence code. Native harness applications own their
conversations. Old gateway configuration is discarded when migrating
`fleet.json`; old channel databases and OS credential entries are left inert,
not opened or deleted. Stop an independently launched legacy gateway before
upgrading. NVIDIA routing components are not required for this deterministic
availability policy.
