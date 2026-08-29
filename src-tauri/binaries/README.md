# Bundled llama-swap

Agent Relay pins [llama-swap v250](https://github.com/mostlygeek/llama-swap/releases/tag/v250), commit `60226b6`, built August 14, 2026.

Included targets:

- `llama-swap-x86_64-pc-windows-msvc.exe` from `llama-swap_250_windows_amd64.zip` (archive SHA-256 `02fa33ffc6e6523989225b80c8e5c10a1ba85b16ed8f417a65b0bbf9d50eca43`)
- `llama-swap-aarch64-apple-darwin` from `llama-swap_250_darwin_arm64.tar.gz` (archive SHA-256 `ebad7fe9beb7b74a6574582b7180dddc6f6bfe905bed38458bf9eb07d3092eef`)

Tauri selects the target-specific file from the shared `binaries/llama-swap` sidecar declaration. Update both artifacts together and verify them against the upstream checksum manifest.

The first-party `agentrelayctl` sidecar is staged for the current Rust host target
by `npm run cli:stage`. Unlike `llama-swap`, it is built from this repository;
do not copy a CLI binary between Windows x64 and macOS Apple Silicon packages.

The first-party Photon channel gateway is compiled into a standalone Bun sidecar
by `npm run gateway:stage`. The generated target binary is ignored by Git and is
rebuilt before every Tauri release, so installed gateways do not require Node.js
or Bun.
