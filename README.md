# Agent Relay

## Engineering Report

Agent Relay is a cross-platform control and routing layer for private AI
inference. The system coordinates model servers across Windows x64 and Apple
Silicon macOS hosts connected by Tailscale, while presenting stable local
endpoints to AI clients. It is designed for a small, trusted fleet in which
models and runtimes are installed and managed independently on each computer.

Current release: **0.16.14**. The project is functional but pre-1.0: packaging,
client integrations, and peer behavior are actively evolving.

## System Design

Every host runs the same Tauri 2 application. The React/TypeScript frontend
provides the tray-first interface; the Rust backend owns process supervision,
peer discovery, lifecycle operations, proxying, metrics, and client setup.
Tailscale supplies addressing and network isolation. Agent Relay does not alter
Tailscale configuration or expose the local model service directly to the LAN.

The runtime path is:

```text
AI client -> stable loopback endpoint -> Agent Relay -> selected Tailscale host
          -> llama-swap -> installed inference runtime -> model
```

Each node advertises a verified `agent-relay-peer-v1` status endpoint. Manual
host definitions override discovery, while verified peers are cached so an
offline laptop remains visible. A host may load one configured profile at a
time; separate hosts can serve concurrently.

## Implemented Capabilities

- Discover and monitor Agent Relay peers over Tailscale.
- Load, unload, restart, and force-cancel text-model workloads remotely.
- Keep model servers alive across tray-app upgrades and restarts.
- Route OpenAI-compatible streaming responses without buffering or re-encoding.
- Report memory, request activity, recent failures, and generation throughput.
- Retain privacy-safe request and lifecycle history in local SQLite storage,
  summarize it in the status window, and export Prometheus metrics for Grafana.
- Expose one stable virtual model to Hermes and OpenCode, then retarget it from
  the tray without rewriting the client for every switch.
- Configure Hermes, OpenCode, Codex, Claude Code, Pi, Copilot CLI, and VS Code
  integrations while preserving unrelated settings and rollback backups.
- Provide `agentrelayctl`, a JSON-oriented local administration CLI.
- Maintain portable channel sessions and route them between Hermes, OpenCode,
  and Pi. Photon/iMessage transport is under active integration; Telegram and
  Discord adapters remain planned.
- Attach a Photon conversation to an existing OpenCode project and conversation
  without moving its repository or changing its running model.
- Provide a mobile-first `!ar attach` chooser that uses numbered replies and
  keeps host IDs, filesystem paths, and native session IDs out of the chat. The
  existing model route is preserved, and an idle model reloads on the next turn.
- Select primary and standby messaging-gateway hosts, with delayed automatic
  failover and fleet-visible active/standby state.

The bundled `llama-swap` path supports process-backed llama.cpp, MLX, vLLM,
Ollama, and ComfyUI profiles through one lifecycle owner. Text runtimes use the
stable OpenAI-compatible proxy. Image workflow profiles use Agent Relay's
bounded ComfyUI prompt/history/view/queue route; WebSocket forwarding and
workflow-template bindings remain planned.

## Repository Layout

| Path | Responsibility |
| --- | --- |
| `src/` | React tray menus, status window, settings, and UI tests |
| `src-tauri/src/` | Rust control plane, proxy, discovery, harnesses, and CLI |
| `channel-gateway/` | Optional messaging transport gateway foundation |
| `integrations/` | Client-side integration components, including Hermes |
| `examples/` | Runtime profile templates for vLLM, Ollama, and ComfyUI |
| `observability/` | Deployable Prometheus and provisioned Grafana dashboard |
| `docs/` | Architecture, onboarding, runtime, CLI, and channel contracts |
| `scripts/` | Version synchronization and release staging utilities |

Model files, inference runtimes, credentials, host configuration, logs, and
generated installers do not belong in the repository.

## Build and Verification

Prerequisites are Node.js, Rust, and the platform-specific
[Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/).

```powershell
npm install
npm install --prefix channel-gateway
npm run gateway:stage
npm run cli:stage
npm test
npm run build
npm run version:check
cargo test --manifest-path src-tauri/Cargo.toml
cargo fmt --check --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
```

Use `npm run tauri dev` for the complete desktop application. Release changes
that affect processes, networking, tray behavior, or packaging require smoke
tests on both Windows x64 and Apple Silicon macOS.

## Operational and Security Notes

Agent Relay assumes a private, single-user tailnet. The peer API binds to the
host's Tailscale address; client and management endpoints bind to loopback.
There is currently no second application-level credential between peers.
The installed Photon gateway stores its project secret in Windows Credential
Manager or macOS Keychain and receives it only through its child-process
environment. Do not publish `fleet.json`, `llama-swap.yaml`, model paths,
transcripts, API keys, or generated app data.

See [architecture](docs/architecture.md), [host onboarding](docs/host-onboarding.md),
[client configuration](docs/client-configuration.md), [CLI](docs/cli.md), and
[channels](docs/channels.md) for detailed contracts and current limitations.
See [telemetry](docs/telemetry.md) for retention, privacy, and Grafana setup.

## License

Copyright (c) 2026 Brent Tyler. **All rights reserved.** The repository is
source-visible but is not open source. See [LICENSE](LICENSE). Third-party
components retain their own licenses.
