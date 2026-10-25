# Opt-in llama.cpp disk cache adapter

This POSIX adapter currently supports tested, single-slot llama.cpp profiles.
It uses Python 3.9+ (standard library only) plus Agent Relay's native retention
helper. It is deployed separately from the tray app. Windows Strata is not yet
enabled. Read [the retention contract](../docs/persistent-kv-cache.md) first.

## Installation

1. Stage the normal platform sidecars, then build the helper:
   `cargo build --manifest-path src-tauri/Cargo.toml --release --example kv-cache-probe`.
2. Copy `llama_cache.py` and the helper into a stable, host-local runtime folder.
   Use one private cache directory per machine for all enabled profiles.
3. Run `scripts/test-kv-restart.py` against the exact native build/model/settings
   before enabling. Require substantial prefix reuse after process restart, not
   just a successful save/restore response. Revalidate runtime upgrades.
4. Compute the native runtime approval hash by importing `llama_cache.py` and
   calling `runtime_hash('/absolute/path/to/llama-server')`. It includes sibling
   dylibs. Do not substitute a version string or hash of the Python launcher.
5. Wrap the model's existing command in its source manifest/generator. Preserve
   context, sampling, model/projector paths and TTL. The native command must use
   `--host 127.0.0.1 --port {upstream_port} --parallel 1` and add
   `--slot-save-path {cache_dir}`. Keep `--ctx-size` visible for Relay overrides.

Example (replace illustrative absolute paths and `TESTED_RUNTIME_DIGEST`):

```yaml
models:
  qwen:
    cmd: >-
      /usr/bin/python3 /opt/agentrelay-cache/llama_cache.py
      --listen-port ${PORT} --cache-dir /private/agentrelay-cache
      --profile qwen --probe /opt/agentrelay-cache/kv-cache-probe
      --verified-runtime TESTED_RUNTIME_DIGEST --
      /opt/llama.cpp/llama-server --model /models/qwen.gguf
      --host 127.0.0.1 --port {upstream_port} --ctx-size 131072
      --parallel 1 --slot-save-path {cache_dir}
    proxy: http://127.0.0.1:${PORT}
    ttl: 1800
    cmdStop: /bin/kill -TERM ${PID}
    unloadTimeout: 10
```

`${PORT}` and `${PID}` are llama-swap substitutions. `{upstream_port}` and
`{cache_dir}` are wrapper substitutions. `cmdStop` must signal the wrapper PID
only, so the child can serialize before exiting. The child stays in llama-swap's
process group for forced cleanup. Do not add `--no-slots` or multiple slots.

6. Schedule the following through a user LaunchAgent (`StartInterval: 3600`,
   `RunAtLoad: true`), using a `ProgramArguments` array with absolute paths:

```sh
/usr/bin/python3 /opt/agentrelay-cache/llama_cache.py \
  --maintenance --cache-dir /private/agentrelay-cache \
  --probe /opt/agentrelay-cache/kv-cache-probe
```

Maintenance never starts a model. It exits while the wrapper holds the runtime
lock; the active wrapper prunes hourly itself. Following a crash, recovery refuses
to delete pending writes while the recorded child PID is still alive. Retention
resumes on login/wake; snapshots older than ten unused days cannot be restored.

7. With incoming requests paused, restart llama-swap idle, then verify a normal
   Relay request, unload, automatic reload, and actual cached-token reuse. Leave
   client endpoints and model aliases unchanged.

## Validation

```sh
python -m unittest discover -s runtime-cache -p test_llama_cache.py
cargo test --manifest-path src-tauri/Cargo.toml --lib kv_cache
python runtime-cache/test_lifecycle.py --swap /path/to/llama-swap \
  --probe /absolute/path/to/kv-cache-probe
```

The POSIX lifecycle regression uses the real llama-swap binary and a synthetic
CPU-only server, including llama.cpp's missing fresh-slot token count. It covers
TTL save/reload, streaming across TTL, cancellation, corrupt restore and bounded
manual unload. The real-model restart benchmark requires an available accelerator.

`kv-cache-probe list ROOT http://127.0.0.1:1 PROFILE inspection` inspects the
catalog. Wrapper events report `ready`, `saved`, `restored` or fallback without
prompt content. `restore-fresh`, `recover` and `discard:UUID` are lifecycle-owner
operations: never call them against a populated/live runtime by hand.

## Rollback and limits

Pause requests and unload normally, restore the original native model command,
remove `cmdStop`/`unloadTimeout` overrides and the source manifest's cache settings,
then restart llama-swap idle. Boot out the exact cache-maintenance LaunchAgent.
Cache deletion is safe only after its wrapper and native child have both stopped.

The store retains at most three ready/pending snapshots total, one latest ready
snapshot per profile. Saving is best effort within seven seconds. Force-kill,
disk failure, active inference or oversized saves may require a cold restart.
Restoring a prefix does not load model weights, supply conversation history, or
make unrelated prompts faster. Changed contexts/configurations do not reuse an
incompatible snapshot. Runtime changes require a fresh capability benchmark.
