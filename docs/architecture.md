# Agent Relay Architecture

CPU embedding-only profiles can run alongside chat/vision using an isolated
persistent llama-swap group. Normal model switches preserve them; explicit
host-wide unload/stop still clears them. See [CPU embeddings](embeddings.md).

Voice workloads use a dedicated WebSocket path rather than chat-completion
adapters. See [Voice runtime](voice-runtime.md) for NVIDIA/MLX protocols,
the microphone UI, session cancellation, and idle-TTL guarantees.

## Goals and boundaries

Agent Relay provides a tray-driven control plane over Tailscale-connected computers. From any online host, the user can inspect inference profiles and start, stop, or clear workloads on another host. Models and inference engines are installed outside this application; it never downloads, copies, or deletes them. The deployed v1 text protocol remains supported while the capability-based adapter contract in [runtime-adapters.md](runtime-adapters.md) adds Ollama, vLLM, and ComfyUI.

`Agent Relay` is the product name and `agent-relay` is the repository/package name.
This identity intentionally starts with fresh application storage and harness
configuration; no compatibility layer for earlier product identities is maintained.

## Per-host application

### Codex CLI connector

The Codex launcher writes only the `model_providers.agentrelay` provider in the
user's Codex configuration, preserving cloud defaults, credentials, and approval
settings. It launches `codex -c model=agentrelay -c model_provider=agentrelay`.
Explicit launch overrides avoid differences between Codex profile formats.
The provider uses the Responses API at `/clients/codex/v1`, with no OpenAI login
required by Relay. The stable `agentrelay` model resolves to the currently selected
Codex target on every request; changes affect subsequent requests, not an active
generation. The selected runtime must implement the Responses API, including
streaming and tool calls; Chat Completions compatibility alone is insufficient.
This connector launches Codex locally, not a remotely controlled Codex session.
For Codex Responses requests, Relay consolidates system/developer text into the
leading instructions field to accommodate templates that reject later system
messages. User and tool messages retain their roles and ordering. Basic shell
tool execution is verified; namespaced MCP tools remain runtime-dependent.
Codex launches receive the selected profile's context size and an 80% compaction
threshold. Existing sessions retain their launch-time context settings.
Each launch also writes a local `codex-model-catalog.json` and passes it as an
explicit catalog override. The catalog describes the Relay alias and its context
window without importing cloud-model marketing text or unsupported capabilities.
It conservatively advertises text input; native vision and namespaced tools still
require separate end-to-end runtime validation.

### CLI project folders

Each CLI row has a native folder picker and up to eight recent local folders.
Selections are stored per harness in this installation's webview storage, not
sent to peers or telemetry. Both direct launch and model-selection launch pass
the folder to the terminal; missing or relative directories are rejected.
Home is the explicit default. Remote project browsing is not implemented.

Every installation runs the same Tauri application with four Rust-side responsibilities:

1. **Supervisor** starts the bundled, pinned `llama-swap` service as an independent process when no compatible listener exists, or adopts the existing listener after a tray restart or upgrade. Release builds enforce one Agent Relay process; the user may register or remove the native login entry from Settings. `llama-swap` starts idle; no profile is preloaded.
2. **Peer API** reports host health, profile inventory, loaded model, runtime, request activity, memory, throughput, uptime, and recent errors. It accepts load, unload, cancel, and restart commands.
3. **Fleet proxy** exposes a general local OpenAI-compatible `/v1` endpoint plus client-scoped Pinned and Auto routes. It maintains the cached fleet catalog and streams requests to the selected host without buffering or SSE re-encoding. The same loopback listener exposes a small `/api/v1` management surface used by the packaged `agentrelayctl` agent CLI.
4. **Tray UI** is the command surface. Left- or right-click opens the same frameless, app-rendered fleet popup; selecting a host opens a second adjacent fly-out window with that node's profiles and lifecycle actions. Its Settings panel persists System, Light, or Dark appearance per host, toggles the native OS login entry, and configures detected harnesses on any online peer. The separate compact, read-only status window opens from the popup. Closing a window or quitting Agent Relay leaves the inference service and loaded model running; **Unload local** and **Stop service** remain explicit actions.

Each node also owns a bounded SQLite telemetry store. Generation observers send
privacy-safe timing and token metadata through a nonblocking queue, so database
writes never gate response streaming. Detailed events roll into hourly and daily
totals; prompt text, response text, project paths, and conversation identifiers
are excluded. The status window reads recent summaries locally. Prometheus text
metrics are available on loopback and the Tailscale-bound peer API for optional
Grafana collection. See [telemetry.md](telemetry.md).

No separate coordinator is required. Each app derives its network view from the Tailscale peer inventory and keeps manual `fleet.json` entries as authoritative overrides. Online addresses are probed for the Agent Relay protocol marker; verified nodes are cached in `discovered-hosts.json`, so an offline laptop remains visible. Peers communicate over their Tailscale addresses or MagicDNS names; Agent Relay does not modify Tailscale configuration. The peer transport uses no additional application credentials because the network is private and single-user, and the peer API only binds to the Tailscale interface.

The peer contract is served on port `38473`: `GET /api/v1/status` reports state and the `agent-relay-peer-v1` marker, `POST /api/v1/control/load` and `POST /api/v1/control/unload` perform host-local lifecycle actions, `GET /api/v1/harnesses` and `POST /api/v1/harness/configure` manage local client connections, `GET /api/v1/harness/opencode/sessions` returns a read-only inventory of root OpenCode conversations, and `/api/v1/harness/{hermes|opencode|pi}/deliver` runs a native harness session on that peer. At startup, the app obtains its interface address from `tailscale ip -4` and binds directly to that Tailscale IPv4 address. Discovery checks `TAILSCALE_CLI_PATH`, the inherited `PATH`, standard installed locations, and the CLI bundled in the macOS app; it forces the bundled app executable into CLI mode. If discovery or binding fails while Tailscale is still starting, Fleet reports the error and retries every five seconds. Binding health is included in the local fleet snapshot instead of failing silently. Peer failures retain the last known catalog and last-seen time while marking the host offline.

The supervised llama-swap endpoint is loopback-only on port `38474`, and the stable client endpoint is loopback-only on port `38475`. Port `38475` also serves fleet-wide status and lifecycle commands under `/api/v1`; these management routes are never bound to Tailscale directly. Remote inference and control are delegated through the existing Tailscale-bound peer API. Agent Relay creates an empty no-preload configuration with a 1,800-second idle TTL unless `fleet.json` names another relative or absolute profile file. When llama-swap expires a model, the next inventory poll clears the loaded model, the tray shows the host as **Idle**, and the profile remains selectable for reload. The pinned service version is reported in peer status. Agent Relay launches the service with detached standard streams and a separate process group so tray replacement cannot terminate a loaded model. Explicit stop and restart controls still cancel, unload, and terminate the verified listener.

The native harness delivery API remains available independently of the removed
Photon gateway. Its on-demand OpenCode runner is loopback-only on port `38476`;
delivery calls create or resume a native session in the specified project.
Inventory access is read-only. Pi delivery uses noninteractive JSON mode and a
deterministic session UUID. Deliveries are serialized on each harness host and
do not copy or relocate repository files. Auto routing selects inference models;
it does not attach to, transfer, or archive harness conversations.

## Auto and host admission (v2)

See [Auto routing](auto-routing.md) for configuration and compatibility rules.
Policy belongs to the initiating Relay instance; machine availability belongs to
and persists on the target host. A shared ordered approval list can be overridden
per client. Existing clients default to Pinned. Auto can cold-start on idle hosts,
but cannot evict another loaded foreground profile. Target-side lifecycle locks
serialize readiness and active-request registration. Off prevents future
admissions without cancelling existing streams or unloading memory.

Context admission uses a text-token estimate rather than the UTF-8 byte upper
bound: three ASCII characters per token, non-ASCII byte reservations, tool-schema
and framing overhead, requested output, and a fixed image allowance. It also
enforces the client's configured minimum capacity. The runtime remains the final
authority on actual tokenization; Relay does not replay context failures. Empty
candidate sets return bounded per-model exclusion reasons without request content.

The peer status advertises `available` and `routing_version: 3`. Auto requires
version 3 at both ends: `control/reserve-auto` returns a model-bound single-use
reservation after readiness; inference consumes it through
`X-AgentRelay-Reservation`. `control/release-auto` releases unused tickets.
Destination-owned leases account for pinned work and native slot activity,
preventing concurrent origin Relays from overbooking a host. Runtime slot limits
remain authoritative. CPU embedding services use a separate resource pool.
Only preflight failures may try another host; inference is never replayed.

Free loaded capacity precedes idle cold starts and busy resident models. A
bounded 30-second admission queue exposes aggregate client counts to the tray
and `/api/v1/routing/waiting`. Optional `X-AgentRelay-Session` hints provide
bounded, expiring, in-memory conversation affinity and are stripped before
forwarding. No session identifiers enter telemetry. Responses still include
`X-AgentRelay-Model` and `X-AgentRelay-Route-Mode`. Response bytes remain streamed
unchanged: Relay does not invent assistant tokens to reset a harness watchdog.

## Profiles and routing

Profiles are displayed as a flat, host-first list. Runtime variants remain distinct. Text model IDs use `<host>/<profile>`, for example `gpu-host/coding-vllm` or `studio/reasoning-mlx`. Every profile advertises its workload kind, capabilities, lifecycle adapter, and resource pool. Hermes and OpenCode only see compatible text profiles; ComfyUI workflow profiles use a separate queue-oriented route.

`GET /v1/models` returns the cached union of compatible text profiles. The client-scoped `/clients/hermes/v1/models` and `/clients/opencode/v1/models` endpoints each return one virtual `agentrelay` model. Agent Relay resolves that alias through the initiating machine's saved route; Pinned routes fail explicitly when their target is unavailable. Auto routes use the approved-model policy described in [Auto routing](auto-routing.md).

The proxy removes the host qualifier and forwards text inference through
llama-swap's normal model-dispatch endpoint, retaining the rewritten local
profile ID in the request body. It never sends Relay inference directly to the
child server URL reported by `/running`, nor through the generic `/upstream`
passthrough: only normal dispatch calls the process `ServeHTTP` path that owns
the per-model in-flight guard and last-use timestamp used by idle TTL expiry.
Consequently, a streaming request cannot be unloaded at the TTL boundary, and
the idle countdown begins only after that request completes. Non-OpenAI workflow
paths such as ComfyUI continue to use the model-qualified passthrough. Streaming
bodies pass through both proxies without buffering or SSE re-encoding,
preserving connection backpressure and cancellation propagation. Release
measurements compare direct `llama-swap`, local-proxy, and
remote-over-Tailscale latency and throughput.

Memory telemetry samples every five seconds. NVIDIA hosts publish framebuffer usage from `nvidia-smi`; Apple Silicon hosts publish unified-memory usage from the operating system. Other hosts fall back to system RAM. The streaming proxy observes runtime timing metadata without altering response bytes; when a runtime omits timings, it estimates throughput from OpenAI usage totals and elapsed request time. Peer status carries both the latest request rate and the sum of completed requests whose generation windows overlapped, allowing the UI to report aggregate throughput and concurrency without extra inference or GPU polling. Throughput history clears when the loaded profile changes.

## Lifecycle and conflicts

Each host may have zero or one loaded profile. Different hosts may run models concurrently. Loading onto an idle host happens immediately. If a host is actively generating, the initiating machine asks whether to cancel and force the switch. Confirmation terminates the active stream, unloads the current profile, verifies memory release, then loads the requested profile; there is no grace countdown.

Agent Relay consumes adapter in-flight state and retains cancellation IDs only in memory. A normal control request returns a conflict while any request is active. A forced request cancels those IDs, explicitly unloads the current profile, then applies the requested operation. Loading always unloads a different profile in the same resource pool first.

The styled primary tray popup stacks one row per host and opens from either tray button. Selecting a row opens an adjacent profile submenu showing current state, available profiles, and unload action; the local node also exposes start, restart, and stop for `llama-swap`. Offline nodes and their cached profiles remain visible but disabled. Separate app and CLI actions for Hermes and OpenCode reuse the adjacent submenu to list only compatible text models currently running on online hosts. App actions connect desktop clients; CLI actions additionally open a terminal. Client connection is independent of profile loading. The parent and submenu dismiss together when focus leaves both windows. Settings always opens collapsed on a new tray invocation. Global actions provide refresh, `Unload local`, and confirmed `Unload all`; the footer opens Agent Relay Status. A debounced content watcher detects profile configuration changes, opens the tray popup, and prompts before restarting the affected `llama-swap`; it never applies a disruptive restart silently. If requests are active, the prompt states that accepting will cancel them.

## Client integration

Hermes uses its `custom` provider pointed at `/clients/hermes/v1` with `agentrelay` as its only model. Choosing a running model from **Route Hermes** changes the private route and publishes a short-lived intent on a loopback-only bridge. A supported Hermes Desktop runtime plugin sends that intent to one pinned, recently focused Hermes window and acknowledges only after it verifies a fresh draft with the virtual model; deferred and error acknowledgements remain distinguishable from success. Existing chats remain attached to the model they started with. Agent Relay installs and hot-updates this plugin under the local Hermes `desktop-plugins/agent-relay/` directory without patching Hermes core. Browser access to the bridge is limited to packaged-app and loopback origins.

Each client connector owns an independent context-window preference. The UI
persists 64K–256K values in `fleet.json`; Hermes receives
`model.context_length`, while OpenCode receives `limit.context` metadata for
every Fleet-owned model. These values guide client history and compaction but
cannot exceed the context actually allocated by the selected serving profile.

OpenCode uses `/clients/opencode/v1`; its managed `agentrelay` provider contains only the `agentrelay` model and the root default remains `agentrelay/agentrelay`. Choosing a running model afterward changes only Agent Relay's saved target, so later switches do not require a configuration rewrite or app restart. The GUI connector uses Desktop's embedded backend and does not require a separate CLI installation. Connecting restarts Desktop after saving its model selection and restores Relay-managed tabs to the local backend; unrelated remote tabs are preserved. CLI launch and channel-delivery API services remain separate integrations. Configuration discovery respects `OPENCODE_CONFIG`, then the standard cross-platform `~/.config/opencode/opencode.json(c)` location; an explicit fleet setting can override it. Existing files receive a sibling rollback backup before modification, and integration health is included in the fleet snapshot. OpenNotebook integration is deferred and is expected to remain local to its host.

Hermes and OpenCode context settings are applied end to end when a connector is
selected. Profiles that advertise a fixed context length, such as llama.cpp,
are rewritten and reloaded when their launch-time context differs from the
client setting. Agent Relay starts llama-swap with configuration watching and
automatically restarts an older adopted control service if it does not observe
the update. Fixed per-profile choices are also persisted in `fleet.json` and
reapplied when a generated llama-swap configuration changes. Managed command
rewrites recognize `--ctx-size`, `-c`, and `LLAMA_ARG_CTX_SIZE`. Dynamic-context
runtimes such as MLX and MTPLX retain their native launch commands while the
client receives the configured limit. An active request prevents an automatic
reload unless the user explicitly confirms a force switch.

Inference controls follow each profile's declared adapter. For Qwen-style
`llama_cpp` profiles, **Off** omits `reasoning_effort`, disables thinking through
`chat_template_kwargs.enable_thinking`, and sets the thinking budget to zero;
the proxy never assumes that the model-specific effort name `none` is valid.
The `strata` adapter instead maps Off to `none`, supports Low/Medium/High,
and sends its budget as `reasoning_budget_tokens` (zero means unlimited).
Profiles advertise supported choices, budget bounds, temperature bounds, and
defaults through `metadata.inference_controls`; these controls are not inferred
from model names. Explicit route overrides take precedence over client values.
Harness route menus read these controls from the saved target's catalog entry,
even when it is unloaded or its host is offline. Such targets are marked
"Not loaded"; editing controls does not load a model or change the saved route.

## Platform and packaging

The shared stack is Tauri 2, Rust, React, and TypeScript. Windows x64 and macOS Apple Silicon receive separate artifacts from the same source tree. The pinned `llama-swap` executable and the first-party `agentrelayctl` agent CLI are packaged as platform-specific Tauri sidecars; inference runtimes and model files remain external. macOS releases will require signing and notarization before routine distribution.

## Live harness route actions

Hermes Desktop and OpenCode expose **Reroute**, **Apply & reload**, and
**Disconnect route** in their route menus. Reroute can select the same running
model again. Disconnect clears the persisted and in-memory selection without
unloading the model, modifying conversations, or removing the virtual provider.
Subsequent requests to the client endpoint return `409 route_not_selected`;
already-dispatched requests continue on their original target.

Thinking/temperature controls are saved for subsequent requests. Context is a
saved client setting; Apply & reload uses that setting plus the serving host's
current profile. GPU placement and batch sizes remain host-profile settings,
not new controls in this UI. Unsaved inference edits still require Save controls.

Reload validates the displayed route, rejects a host now serving a different
model, and runs under the host's lifecycle lock. Active work returns a conflict
until the user explicitly chooses Force reload. Restarting the shared model can
affect other harnesses. The peer endpoint `POST /api/v1/control/reload` accepts
the load-request schema; both peers must support it (0.22.0+). Older peers fail
without falling back to a destructive unload/load sequence.

## Worker profile behavior

The optional `workers/adaface` CPU service exposes face-crop embeddings for Bokeh
through this worker route. It requires externally installed AdaFace IR18 weights
and source. It accepts aligned crops only and never stores image or embedding
content. See its README for the versioned API and input limits.

The loopback `POST /api/v1/manage` endpoint is the typed CLI bridge for harness
setup/routing, saved generation controls, explicit reload/disconnect, local CLI
launch, and local service lifecycle. It reuses application validation and conflict
handling, never exposes arbitrary UI commands, and has no availability override.

`kind: worker` marks a llama-swap profile as a supervised HTTP service rather than a
language model (capability `http_service`). llama-swap still launches, health-checks, and
TTL-unloads it. Workers never appear in `/v1/models` and are never text-client targets.
The fleet proxy exposes them at `/api/worker/{host_id}/{model_id}/{path}` and the peer API
at `/api/v1/worker/{model_id}/{path}`; only `GET health` and `GET|POST v1/<segment>/...`
pass, bodies are streamed, WebSocket upgrades are refused. `WorkloadKind` also gained an
`Unknown` fallback (`#[serde(other)]`) so peers keep decoding status from newer builds.

## Local inference capacity

`GET /api/v1/models/{model_id}/capacity` on the loopback fleet proxy reports
sanitized live llama.cpp slot counts and per-slot context without loading a
model or refreshing its idle timer. Only this monitoring lookup reads the
backend directly; inference remains on llama-swap's tracked dispatch path.
Opt-in `context_slots` metadata keeps context controls expressed per request
while rewriting the total launch context for fixed parallel slots. See
[concurrent document transcription](vision-batching.md).

## OpenBot client route

OpenBot has an independent persisted route at `/clients/openbot/v1`, with the
stable model alias `agentrelay`. It uses native custom-provider configuration;
Hermes and standalone OpenCode selections are unchanged. The shared streaming
proxy retains cancellation, cold loading, and llama-swap idle tracking. See
[OpenBot setup and validation](openbot.md).

## Persistent inference state

The opt-in persistence core in `kv_cache.rs` and `kv_cache_runtime.rs` retains at
most three snapshots per host and expires them after ten unused days. On tested
macOS llama.cpp profiles, `runtime-cache/llama_cache.py` saves through llama-swap's
`cmdStop` lifecycle and restores before admitting requests. A raw TCP bridge
preserves streaming; inference still traverses llama-swap so TTL measures actual
idle time. A failed restore restarts the native child cleanly. An exclusive
runtime lease and hourly user maintenance job protect recovery and retention.
See [the cache contract and runtime validation](persistent-kv-cache.md) before
enabling a runtime. Cache payloads are private local data, never telemetry.

### Background reclaim admission

Since 2.3.0, serving hosts advertise opt-in background profiles (`priority:
background` metadata or local reclaim settings) and aggregate state with routing
protocol 4. A destination-owned guard closes admission before draining Relay
leases and native llama.cpp slots. After 90 seconds it cancels only the background
profile, unloads it, and verifies removal before starting the foreground model.
Availability and GPU policy are rechecked under the lifecycle lock. Independent
CPU/Metal embedding pools remain untouched. New background requests receive 503
`host_reclaimed` or `host_occupied` with Retry-After and remain blocked until the
foreground model's actual unload. Already-streaming cancellation ends with a
stream error, never a fabricated success. See [Auto routing](auto-routing.md)
for the client retry contract, cancellation bounds and compatibility.
