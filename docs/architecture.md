# Agent Relay Architecture

## Goals and boundaries

Agent Relay provides a tray-driven control plane over Tailscale-connected computers. From any online host, the user can inspect inference profiles and start, stop, or clear workloads on another host. Models and inference engines are installed outside this application; it never downloads, copies, or deletes them. The deployed v1 text protocol remains supported while the capability-based adapter contract in [runtime-adapters.md](runtime-adapters.md) adds Ollama, vLLM, and ComfyUI.

`Agent Relay` is the product name and `agent-relay` is the repository/package name.
This identity intentionally starts with fresh application storage and harness
configuration; no compatibility layer for earlier product identities is maintained.

## Per-host application

Every installation runs the same Tauri application with four Rust-side responsibilities:

1. **Supervisor** starts the bundled, pinned `llama-swap` service as an independent process when no compatible listener exists, or adopts the existing listener after a tray restart or upgrade. Release builds enforce one Agent Relay process; the user may register or remove the native login entry from Settings. The managed messaging gateway is tied to the tray process lifetime so a crash or upgrade cannot leave a duplicate Photon listener. `llama-swap` starts idle; no profile is preloaded.
2. **Peer API** reports host health, profile inventory, loaded model, runtime, request activity, memory, throughput, uptime, and recent errors. It accepts load, unload, cancel, and restart commands.
3. **Fleet proxy** exposes a general local OpenAI-compatible `/v1` endpoint plus client-scoped Hermes and OpenCode routes. It also dispatches channel messages to persistent Hermes, OpenCode, or Pi sessions. It maintains the cached fleet catalog and streams requests to the selected host without buffering or SSE re-encoding. The same loopback listener exposes a small `/api/v1` management surface used by the packaged `agentrelayctl` agent CLI.
4. **Tray UI** is the command surface. Left- or right-click opens the same frameless, app-rendered fleet popup; selecting a host opens a second adjacent fly-out window with that node's profiles and lifecycle actions. Its Settings panel persists System, Light, or Dark appearance per host, toggles the native OS login entry, and configures detected harnesses on any online peer. The separate compact, read-only status window opens from the popup. Closing a window or quitting Agent Relay leaves the inference service and loaded model running; **Unload local** and **Stop service** remain explicit actions.

Each node also owns a bounded SQLite telemetry store. Generation observers send
privacy-safe timing and token metadata through a nonblocking queue, so database
writes never gate response streaming. Detailed events roll into hourly and daily
totals; prompt text, response text, project paths, and conversation identifiers
are excluded. The status window reads recent summaries locally. Prometheus text
metrics are available on loopback and the Tailscale-bound peer API for optional
Grafana collection. See [telemetry.md](telemetry.md).

The optional **Channel gateway** is a fifth, separately layered responsibility.
Photon/iMessage, Telegram, Discord, and other adapters normalize inbound events
and resolve an active portable session to a harness host plus independently
selected fleet model host. A transport conversation can retain multiple flat,
resumable Agent Relay sessions while exposing exactly one active session. Channel
transport credentials never enter the model proxy or fleet configuration. A
preferred host and standby host use fleet-visible role heartbeats and a delayed,
non-preemptive failover election before opening a persistent connection. See
[channels.md](channels.md).

The Photon companion publishes a loopback heartbeat and maintains a durable
reply outbox. An iMessage send failure can therefore retry the saved response
without repeating a model turn or route-changing command. Work is serialized
within each conversation but runs concurrently across independent conversations;
Spectrum's responding state remains active for the full model turn.

The loopback command handler also owns a ten-minute, in-memory mobile chooser.
`!ar attach` inventories recent OpenCode sessions from online peers, and ordinary
number replies are claimed by the chooser before model delivery. Only sessions
whose Agent Relay route still points to an available model profile are listed;
selecting one reuses that exact harness host, model host, model, project, and
native session without preloading it. The next inference request reloads an idle
model through llama-swap. Restarting Agent Relay or sending `!ar cancel`
discards pending chooser state; no credentials or conversation content are
stored in it.

The gateway host keeps a local portable transcript journal of successful channel
prompts and replies. A cross-harness move creates a pending destination session;
its first prompt receives the bounded transcript chain, and the move becomes
complete only after the destination replies and that exchange is durably saved.
Transport message IDs make completed replies replayable without a second model
turn. Chats entered directly in a harness UI are not visible to this journal.

No separate coordinator is required. Each app derives its network view from the Tailscale peer inventory and keeps manual `fleet.json` entries as authoritative overrides. Online addresses are probed for the Agent Relay protocol marker; verified nodes are cached in `discovered-hosts.json`, so an offline laptop remains visible. Peers communicate over their Tailscale addresses or MagicDNS names; Agent Relay does not modify Tailscale configuration. v1 uses no additional application credentials because the network is private and single-user, and the peer API only binds to the Tailscale interface.

The peer contract is served on port `38473`: `GET /api/v1/status` reports state and the `agent-relay-peer-v1` marker, `POST /api/v1/control/load` and `POST /api/v1/control/unload` perform host-local lifecycle actions, `GET /api/v1/harnesses` and `POST /api/v1/harness/configure` manage local client connections, `GET /api/v1/harness/opencode/sessions` returns a read-only inventory of root OpenCode conversations, and `/api/v1/harness/{hermes|opencode|pi}/deliver` runs a native harness session on that peer. At startup, the app obtains its interface address from `tailscale ip -4` and binds directly to that Tailscale IPv4 address. Discovery checks `TAILSCALE_CLI_PATH`, the inherited `PATH`, standard installed locations, and the CLI bundled in the macOS app; it forces the bundled app executable into CLI mode. If discovery or binding fails while Tailscale is still starting, Fleet reports the error and retries every five seconds. Binding health is included in the local fleet snapshot instead of failing silently. Peer failures retain the last known catalog and last-seen time while marking the host offline.

The supervised llama-swap endpoint is loopback-only on port `38474`, and the stable client endpoint is loopback-only on port `38475`. Port `38475` also serves fleet-wide status and lifecycle commands under `/api/v1`; these management routes are never bound to Tailscale directly. Remote inference and control are delegated through the existing Tailscale-bound peer API. Agent Relay creates an empty no-preload configuration with a 1,800-second idle TTL unless `fleet.json` names another relative or absolute profile file. When llama-swap expires a model, the next inventory poll clears the loaded model, the tray shows the host as **Idle**, and the profile remains selectable for reload. The pinned service version is reported in peer status. Agent Relay launches the service with detached standard streams and a separate process group so tray replacement cannot terminate a loaded model. Explicit stop and restart controls still cancel, unload, and terminate the verified listener.

The on-demand OpenCode harness runner is loopback-only on port `38476`. Agent
Relay starts it only when an OpenCode-routed channel message arrives, then creates
or resumes the native OpenCode session in the route's project directory. The
route editor may also attach to a root conversation already recorded in that
host's OpenCode database. Inventory access is read-only; Agent Relay validates
the project/session pair and sends subsequent messages to the existing native
session without copying or relocating repository files.
Pi-routed messages launch Pi in noninteractive JSON mode with a deterministic
session UUID. Pi resumes that native session on every message, uses the selected
project directory, and reaches the independently selected model through the local
Agent Relay proxy. Deliveries are serialized on each harness host.

Channel commands are accepted only on the loopback management listener. The
`!ar use`, `new`, `move`, and `resume` transactions validate the exact
host/profile, harness host, and required capability,
uses the same local or peer lifecycle operation as the tray, and persists the
session route only after a successful load. `use` mutates the active route;
`new` and `move` create a new session and archive the prior Agent Relay session;
`resume` reactivates an archived route. Portable context remains pending until
the destination's first successful reply. Agent Relay then archives the source
Hermes or OpenCode conversation; Pi's one-shot process is already closed while
its transcript remains resumable. A native archive failure is retained as a
retryable route state rather than rolling back a reply that was already
delivered. Resuming a route restores its archived native conversation before the
route becomes active.
Conflicts return an explicit force-confirmation command
without mutating the route. Ordinary messages are not claimed by the command
parser.

## Profiles and routing

Profiles are displayed as a flat, host-first list. Runtime variants remain distinct. Text model IDs use `<host>/<profile>`, for example `gpu-host/coding-vllm` or `studio/reasoning-mlx`. Every profile advertises its workload kind, capabilities, lifecycle adapter, and resource pool. Hermes and OpenCode only see compatible text profiles; ComfyUI workflow profiles use a separate queue-oriented route.

`GET /v1/models` returns the cached union of compatible text profiles. The client-scoped `/clients/hermes/v1/models` and `/clients/opencode/v1/models` endpoints each return one virtual `agentrelay` model. Agent Relay resolves that alias through the initiating machine's saved route; an unavailable target fails explicitly instead of silently choosing another model.

The proxy removes the host qualifier when forwarding a request to that host's `llama-swap` endpoint. Streaming bodies pass through with connection reuse, backpressure, and cancellation propagation. Release measurements compare direct `llama-swap`, local-proxy, and remote-over-Tailscale latency and throughput.

Memory telemetry samples every five seconds. NVIDIA hosts publish framebuffer usage from `nvidia-smi`; Apple Silicon hosts publish unified-memory usage from the operating system. Other hosts fall back to system RAM. The streaming proxy observes runtime timing metadata without altering response bytes; when a runtime omits timings, it estimates throughput from OpenAI usage totals and elapsed request time. Peer status carries both the latest request rate and the sum of completed requests whose generation windows overlapped, allowing the UI to report aggregate throughput and concurrency without extra inference or GPU polling. Throughput history clears when the loaded profile changes.

## Lifecycle and conflicts

Each host may have zero or one loaded profile. Different hosts may run models concurrently. Loading onto an idle host happens immediately. If a host is actively generating, the initiating machine asks whether to cancel and force the switch. Confirmation terminates the active stream, unloads the current profile, verifies memory release, then loads the requested profile; there is no grace countdown.

Agent Relay consumes adapter in-flight state and retains cancellation IDs only in memory. A normal control request returns a conflict while any request is active. A forced request cancels those IDs, explicitly unloads the current profile, then applies the requested operation. Loading always unloads a different profile in the same resource pool first.

The styled primary tray popup stacks one row per host and opens from either tray button. Selecting a row opens an adjacent profile submenu showing current state, available profiles, and unload action; the local node also exposes start, restart, and stop for `llama-swap`. Offline nodes and their cached profiles remain visible but disabled. Separate app and CLI actions for Hermes and OpenCode reuse the adjacent submenu to list only compatible text models currently running on online hosts. App actions connect desktop clients; CLI actions additionally open a terminal. Client connection is independent of profile loading. The parent and submenu dismiss together when focus leaves both windows. Settings always opens collapsed on a new tray invocation. Global actions provide refresh, `Unload local`, and confirmed `Unload all`; the footer opens Agent Relay Status. A debounced content watcher detects profile configuration changes, opens the tray popup, and prompts before restarting the affected `llama-swap`; it never applies a disruptive restart silently. If requests are active, the prompt states that accepting will cancel them.

The tray also includes one **Message routes** entry. Its adjacent submenu lists
recent channel conversations and then reuses that same window as a route editor,
rather than opening a third fly-out. It can change the active model, move to a
different harness and harness host, select an existing OpenCode project and
conversation, start a clean session, or resume an archived session. These controls invoke the same loopback
channel transaction used by `!ar` commands, so the tray and messaging surfaces
cannot drift into independent routing state. The editor displays whether a moved
session is awaiting its first destination reply or has accepted the portable
context.

## Client integration

Hermes uses its `custom` provider pointed at `/clients/hermes/v1` with `agentrelay` as its only model. Choosing a running model from **Route Hermes** changes the private route and publishes a short-lived intent on a loopback-only bridge. A supported Hermes Desktop runtime plugin sends that intent to one pinned, recently focused Hermes window and acknowledges only after it verifies a fresh draft with the virtual model; deferred and error acknowledgements remain distinguishable from success. Existing chats remain attached to the model they started with. Agent Relay installs and hot-updates this plugin under the local Hermes `desktop-plugins/agent-relay/` directory without patching Hermes core. Browser access to the bridge is limited to packaged-app and loopback origins.

Each client connector owns an independent context-window preference. The UI
persists 64K–256K values in `fleet.json`; Hermes receives
`model.context_length`, while OpenCode receives `limit.context` metadata for
every Fleet-owned model. These values guide client history and compaction but
cannot exceed the context actually allocated by the selected serving profile.

OpenCode uses `/clients/opencode/v1`; its managed `agentrelay` provider contains only the `agentrelay` model and the root default remains `agentrelay/agentrelay`. Choosing a running model afterward changes only Agent Relay's saved target, so later switches do not require a configuration rewrite or app restart. Agent Relay starts OpenCode's loopback API server before connecting the desktop and restores it when an already-running desktop is discovered after Agent Relay restarts. Other configuration is preserved. Configuration discovery respects `OPENCODE_CONFIG`, then the standard cross-platform `~/.config/opencode/opencode.json(c)` location; an explicit fleet setting can override it. Existing files receive a sibling rollback backup before modification, and integration health is included in the fleet snapshot. OpenNotebook integration is deferred and is expected to remain local to its host.

Hermes and OpenCode context settings are applied end to end when a connector is
selected. Profiles that advertise a fixed context length, such as llama.cpp,
are rewritten and reloaded when their launch-time context differs from the
client setting. Agent Relay starts llama-swap with configuration watching and
automatically restarts an older adopted control service if it does not observe
the update. Dynamic-context runtimes such as MLX and MTPLX retain their native
launch commands while the client receives the configured limit. An active
request prevents an automatic reload unless the user explicitly confirms a
force switch.

## Platform and packaging

The shared stack is Tauri 2, Rust, React, and TypeScript. Windows x64 and macOS Apple Silicon receive separate artifacts from the same source tree. The pinned `llama-swap` executable and the first-party `agentrelayctl` agent CLI are packaged as platform-specific Tauri sidecars; inference runtimes and model files remain external. macOS releases will require signing and notarization before routine distribution.
