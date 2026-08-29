# Repository Guidelines

## Project Structure & Architecture

`src/` contains the React/TypeScript tray-window UI; keep components small and move shared state or API types into focused modules as they emerge. `src-tauri/src/` is the Rust control plane for peer health, `llama-swap` supervision, streaming proxying, metrics, and tray integration. Tauri capabilities and packaging live in `src-tauri/capabilities/` and `src-tauri/tauri.conf.json`. Architecture decisions are recorded in `docs/architecture.md`; update it when behavior or protocol contracts change.

Operational telemetry is stored locally in SQLite through `src-tauri/src/telemetry.rs` and exported in Prometheus format. Never add prompt or response content, conversation IDs, sender identities, project paths, credentials, or unbounded labels to telemetry. Keep inference-side recording nonblocking. Grafana and Prometheus examples live in `observability/`.

The app targets Windows x64 and macOS Apple Silicon. Do not bundle Ollama, vLLM, ComfyUI, or model files; integrate installed runtimes through adapters and keep host-specific profiles external and read-only. Never commit models, credentials, logs, generated bundles, or machine-specific paths.

## Build, Test, and Development Commands

```powershell
npm install          # Install JavaScript dependencies.
npm install --prefix channel-gateway # Install Photon gateway build dependencies.
npm run cli:stage    # Build the target-specific agentrelayctl sidecar.
npm run gateway:stage # Build the self-contained Photon gateway sidecar.
npm run dev          # Run the Vite UI in a browser.
npm run build        # Type-check and build the frontend.
npm run version:check # Verify all package versions agree.
npm run version:bump -- patch # Bump synchronized release metadata.
npm run tauri dev    # Run the complete desktop application.
cargo test --manifest-path src-tauri/Cargo.toml
```

Run `npm run cli:stage` and `npm run gateway:stage` once on a clean platform
checkout before direct Cargo commands; Tauri validates every declared
target-specific sidecar during its build script.

Run `cargo fmt --check --manifest-path src-tauri/Cargo.toml` and `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings` before merging Rust changes. Build and smoke-test on both operating systems when changing process management, packaging, networking, or tray behavior.

Use `major.minor.patch` release versions. Agents may choose patch releases for fixes and backward-compatible refinements and minor releases for new backward-compatible capabilities. Never change the major version without explicit user direction. Keep `package.json`, both lock files, `Cargo.toml`, and `tauri.conf.json` synchronized through `scripts/version.mjs`; do not edit release versions individually.

## Coding Style & Naming

Use two-space indentation in TypeScript/CSS and standard `rustfmt` formatting in Rust. React components and TypeScript types use `PascalCase`; functions, hooks, and variables use `camelCase`; Rust modules and functions use `snake_case`. Prefer explicit domain names such as `HostStatus` and `unload_all_hosts`. Keep the proxy streaming: do not buffer or re-encode response bodies.

## Testing Guidelines

Place frontend tests beside source as `*.test.ts(x)` and Rust unit tests in the owning module. Integration tests belong in `src-tauri/tests/`. Cover model-ID routing, offline peers, load conflicts, forced cancellation, config-change prompts, and partial peer failure. No coverage threshold exists yet; every behavior change needs a regression test.

## Commits & Pull Requests

Use short imperative Conventional Commit subjects, for example `feat(proxy): route host-qualified model ids`. Keep commits focused. Pull requests must explain user-visible behavior, list tests run on Windows and macOS, note protocol/config changes, and include a screenshot for tray-window changes. Never commit secrets or real model paths.
