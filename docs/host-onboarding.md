# Host Onboarding

Each computer owns its inference inventory. Agent Relay reads text profiles from the host's `llama-swap.yaml`; model binaries, runtime binaries, and absolute paths stay outside this repository.

## Configure the host

Launch Agent Relay once to create its configuration directory:

- Windows: `%APPDATA%\com.brent.agentrelay`
- macOS: `~/Library/Application Support/com.brent.agentrelay`

Check `fleet.json` before adding profiles. The current hostname is normalized
and inserted as the only initial host. Agent Relay discovers other running nodes from Tailscale
and saves verified peers in `discovered-hosts.json`; no shared catalog copy is
required. Add a manual `fleet.json` entry only when overriding a display name,
address, or hardware label. Agent Relay binds peer control to the host's
Tailscale address on port `38473`; it does not modify Tailscale configuration.

On an Apple Silicon Mac, build the local app and DMG from the repository root:

```bash
./scripts/build-macos.sh
```

The preflight requires Node/npm, Rust/Cargo, Xcode Command Line Tools, the bundled arm64 `llama-swap`, and a live Tailscale IPv4 address. It supports both a `tailscale` command in `PATH` and the CLI inside `/Applications/Tailscale.app`. Output is written under `src-tauri/target/release/bundle/`; install the `.app` locally for initial testing. Signing and notarization remain a later distribution step.

## Add model profiles

Add one entry per servable model under `models` in `llama-swap.yaml`. Use a stable ID without the model file extension:

```yaml
globalTTL: 1800
models:
  example-model-q4:
    name: Example Model Q4
    cmd: >-
      "/path/to/runtime" --model "/path/to/model.gguf"
      --host 127.0.0.1 --port ${PORT} --alias ${MODEL_ID}
    metadata:
      runtime: llama.cpp
```

Keep `hooks.on_startup.preload` absent so the host starts idle. Use `${PORT}` instead of fixed upstream ports. Preserve runtime-specific settings such as context length, MLX launch arguments, projectors, or CPU expert offload in the host file. Do not expose the runtime directly on the tailnet; llama-swap should remain loopback-only.

## Validate and adopt changes

Before restarting, verify every runtime, model, and projector path exists. Start llama-swap temporarily on an unused loopback port and query its catalog:

```powershell
llama-swap.exe -config $env:APPDATA\com.brent.agentrelay\llama-swap.yaml -listen 127.0.0.1:38476
Invoke-RestMethod http://127.0.0.1:38476/v1/models
```

Every profile should appear as `unloaded`. Stop the validator, then accept Agent Relay's local configuration-change prompt. Confirm the stable text catalog at `http://127.0.0.1:38475/v1/models`; IDs will be qualified as `<host>/<profile>`. Finally, test load, streaming inference, and unload only when the host has enough free memory.
