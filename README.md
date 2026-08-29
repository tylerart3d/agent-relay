<p align="center">
  <img src="src-tauri/icons/icon.png" alt="Agent Relay" width="128" />
</p>

# Agent Relay

**One control surface for local AI models, agent harnesses, and private hardware.**

Agent Relay is a cross-platform routing layer for running AI workloads across a
small fleet of Windows and Apple Silicon computers. It gives every client a
stable local endpoint while models can be loaded, unloaded, and redirected to
another machine from the tray, CLI, or a connected messaging channel.

The project is built for personal infrastructure: a workstation at home, a
laptop on the road, or a friend's gaming hardware can contribute inference
without moving model files or exposing raw model servers to clients.

> **Project status:** Current release: **0.18.2**. Agent Relay is functional,
> actively used, and still pre-1.0. Packaging, remote discovery, and client
> integrations may change.

## How It Works

Each machine runs the same Agent Relay application. The local app supervises
its runtime, advertises its model catalog, reports health and performance, and
accepts control requests from trusted peers. Clients connect only to Agent
Relay's stable loopback endpoint.

```text
Hermes / OpenCode / Pi / CLI / messaging
                    │
                    ▼
          Stable Agent Relay endpoint
                    │
          Select host + model profile
                    │
                    ▼
     Agent Relay peer → llama-swap → runtime → model
```

Only one model profile is active per host, but different hosts can serve models
simultaneously. Switching a route does not require rewriting every client or
moving the underlying model.

## Capabilities

- Discover online peers and retain offline machines in the fleet view.
- Load, unload, restart, and force-cancel model workloads from any node.
- Keep an existing model server alive while the tray application is upgraded.
- Stream OpenAI-compatible responses without buffering the generated output.
- Route a single virtual model to different hosts and physical models.
- Control verified per-model thinking effort, reasoning limits, and temperature.
- Configure supported harnesses while preserving unrelated user settings and
  rollback copies.
- Move portable conversation context between supported harnesses and projects,
  then archive and restore the corresponding native conversation safely.
- Attach messaging conversations to an existing OpenCode session and model.
- Track memory, active requests, failures, and generation throughput locally.
- Export Prometheus metrics to the included Grafana dashboard.
- Operate through the tray UI or the JSON-oriented `agentrelayctl` CLI.

## Supported Integrations

| Area | Current support |
| --- | --- |
| Agent harnesses | Hermes Desktop and CLI, OpenCode Desktop and CLI, Pi |
| Additional clients | Codex CLI, Claude Code, Copilot CLI, VS Code |
| Text runtimes | llama.cpp, MLX, vLLM, Ollama through llama-swap profiles |
| Image workflows | Bounded ComfyUI prompt, history, view, queue, interrupt, and memory routes |
| Messaging | Photon/iMessage foundation with routed sessions and gateway failover |
| Observability | Local SQLite history, Prometheus metrics, provisioned Grafana dashboard |

Telegram, Discord, ComfyUI WebSocket forwarding, and workflow-template bindings
are planned rather than complete.

## Architecture

The desktop application uses Tauri 2 with a React/TypeScript interface and a
Rust service layer. Rust owns peer discovery, process supervision, routing,
proxying, metrics, harness configuration, and the local CLI contract.
`llama-swap` remains the single lifecycle owner for inference runtimes.

Agent Relay currently uses Tailscale for private addressing and peer reachability.
It does not modify the user's tailnet configuration. Manual host definitions
override discovery, and verified peers are cached so intermittently connected
laptops remain visible while offline.

For deeper design details, see [Architecture](docs/architecture.md),
[Runtime adapters](docs/runtime-adapters.md), and [Channels](docs/channels.md).

## Repository Map

| Path | Purpose |
| --- | --- |
| `src/` | Tray menus, status window, settings, and UI tests |
| `src-tauri/src/` | Rust service, proxy, discovery, harness integrations, and CLI |
| `channel-gateway/` | Messaging transport gateway |
| `integrations/` | Client-side integration components |
| `examples/` | Example llama-swap runtime profiles |
| `observability/` | Prometheus and Grafana deployment files |
| `docs/` | Architecture, onboarding, configuration, CLI, and operations |
| `scripts/` | Version synchronization and release staging |

Models, credentials, host configuration, logs, generated installers, and user
conversation data are deliberately excluded from the repository.

## Building and Testing

Install Node.js, Rust, and the platform-specific
[Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/).

```powershell
npm install
npm install --prefix channel-gateway
npm run gateway:stage
npm run cli:stage
npm test
npm run build
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri dev
```

Before packaging a release, also run:

```powershell
npm run version:check
cargo fmt --check --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
```

Changes involving processes, networking, tray behavior, or packaging require
smoke tests on Windows x64 and Apple Silicon macOS. See the
[manual test checklist](docs/manual-test-checklist.md).

## Prometheus and Grafana

Agent Relay exposes privacy-safe Prometheus metrics for host availability,
loaded models, memory use, request activity, failures, and generation speed.
The `observability/` directory includes a ready-to-run Prometheus configuration
and provisioned Grafana dashboard for viewing fleet health and performance over
time. Prompts, responses, and conversation content are never exported as
metrics. See [Telemetry and Grafana](docs/telemetry.md) for setup and retention
details.

## Deployment and Configuration

- [Host onboarding](docs/host-onboarding.md)
- [Client configuration](docs/client-configuration.md)
- [CLI reference](docs/cli.md)
- [Telemetry and Grafana](docs/telemetry.md)

Agent Relay assumes a small, trusted private network. Peer traffic currently
has no second application-level credential beyond the private network boundary.
The Photon project secret is stored in Windows Credential Manager or macOS
Keychain and is passed only to the gateway child process. Never commit runtime
configuration, model paths, transcripts, API keys, or generated application
data.

## License

Copyright © 2026 Brent Tyler. **All rights reserved.**

Agent Relay is source-visible for evaluation, but it is not open-source
software and no permission to use, copy, modify, or redistribute its original
code or assets is granted. See [LICENSE](LICENSE). Bundled and referenced
third-party components remain governed by their respective licenses.
