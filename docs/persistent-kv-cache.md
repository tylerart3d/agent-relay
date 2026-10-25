# Persistent Qwen context cache

Status: **capability pilot; not enabled in the installed application**. The
retention core and slot adapter are implemented, with synthetic tests. No
production launch flags, idle TTLs, routes or running models have been changed.

## Retention contract

- At most **three snapshots total per host**, shared across profiles; one latest
  completed snapshot per profile in this first iteration. Pending writes also
  consume a slot. A fourth reservation evicts the least recently used eligible
  snapshot before writing. Prefer preserving the profile being replaced until
  its new save succeeds.
- Expire after **ten days without successful use**. A new snapshot starts its
  lifetime at save time; a verified restore refreshes it. Listing, failed restore
  attempts and maintenance do not. Run cleanup before operations and hourly when
  the application lifecycle integration is enabled; overdue files are removed on
  the next run if the app was stopped.
- Store files in a private, host-local cache directory, outside repositories and
  telemetry. These files contain conversation state, including token IDs.
- An OS lock allows one catalog owner. UUID filenames, SQLite publication and
  exact file-size checks keep partial writes out of the usable catalog. Do not
  delete a pending file after an HTTP timeout: native serialization may still be
  running. Recover pending/orphan files only after confirming runtime shutdown.
- Match the model, projector/draft, runtime build and effective state-affecting
  settings. A display name or profile ID alone is insufficient. Never assume
  llama.cpp and Strata formats, quantizations or machines are interchangeable.

## Runtime findings

llama.cpp exposes `--slot-save-path` and `POST /slots/0?action=save|restore`.
Hybrid/recurrent models also need their context checkpoints. Upstream
[PR 26004](https://github.com/ggml-org/llama.cpp/pull/26004) addresses that path.
The inspected M1 source checkout does not include the checkpoint appendix; its
installed binary reports a different build from the checkout. Neither source
inspection nor a successful restore response certifies the deployed binary.
It must pass the process-restart reuse test below.

The local Strata source implements the same HTTP operations with its own session
format. It includes recurrent state and draft state. The installed Windows
binary contains that feature's markers, but this is not a functional result.
The initial isolated Qwen test could not start because other GPU applications
left insufficient VRAM. The user deferred GPU testing until the accelerator is
free. The M1's existing populated slot has been left intact.

## Integration boundary

`kv_cache.rs` owns retention; `kv_cache_runtime.rs` owns bounded administrative
HTTP and save/restore validation. They are callable through the diagnostic
example, but **the production supervisor does not invoke them yet**.

The production integration must:

1. Enable only runtime builds/profiles that pass the capability benchmark. Use
   one shared cache directory per host; initially require a single inference slot.
2. Acquire an exclusive lifecycle lease covering save/restore and new inference
   admission. Do not overwrite a populated slot or serialize active inference.
3. Use llama-swap v250's `cmdStop` for pre-stop saving on TTL expiry, ordinary
   unload, model switch and service shutdown. Keep llama-swap's idle timer and
   in-flight accounting. The stop helper must terminate its exact supervised
   process even if Relay is unavailable or caching fails; never kill by name.
   Give the hook a bounded grace period, coordinated with Relay's unload timeout.
4. Restore before allowing the first request into a newly started model. Requests
   still provide their conversation; the runtime checks the matching prefix.
   A corrupt/refused/timed-out restore requires a clean runtime before cold
   inference. Force-cancel must remain immediate and may skip saving.
5. Compute real compatibility digests, schedule expiry, expose cache status and
   clear-cache controls, and test the whole lifecycle before deployment.

The cache cannot remove the first-ever prefill, accelerate an unrelated prompt,
or eliminate model-weight loading. Memory is released after serialization and
unload. Exact sampled output across restarts is not a promised invariant.

## Reproducible capability test

Build the diagnostic runner (normal sidecar staging prerequisites apply):

```sh
cargo build --manifest-path src-tauri/Cargo.toml --example kv-cache-probe
python scripts/test-kv-restart.py /private/benchmark-spec.json --report /private/result.json
```

The untracked spec contains `endpoint` (unused `http://127.0.0.1:PORT`), `model`,
`cwd`, `cache_dir`, `argv` (the native runtime command, with `{cache_dir}` used
for `--slot-save-path`), and optionally `probe_binary` (the built diagnostic
runner). Use the runner to exercise the real retention core. Its compatibility
argument is a benchmark-only key; production needs the full fingerprint above.

The script starts its own process, generates a synthetic common prefix, measures
cold and warm requests, saves state, terminates only its own process tree,
starts a fresh process, restores, and sends a different continuation. Passing
requires nonzero warm reuse and restored reuse of at least 90% of the warm
baseline. It records token counts, timings, native save/restore results and PIDs;
HTTP success and fast restore alone do not count. Logs and snapshots stay private.

After that passes, repeat at representative long-context lengths, then test
streaming, cancellation, a short TTL, a request crossing the TTL boundary,
restart failure, disk full, changed configuration and stale/corrupt snapshots.
No GPU test is run automatically as part of ordinary unit tests.

## Validation so far

- Windows: 210 library tests pass, including 15 new retention/slot tests;
  diagnostic example builds; all-target Clippy and formatting pass.
- M1 macOS: the same 15 cache tests pass in an isolated build directory. The
  installed application and its resident model were not replaced or restarted.
- Live Strata restart benchmark: **not passed yet**. Initial startup failed for
  lack of free VRAM while other GPU applications were running; further GPU
  testing is explicitly deferred until the user releases the accelerator.
- Real M1 restart, full-size snapshots, TTL hook integration, production
  fingerprints, maintenance scheduling and UI controls remain acceptance gates.
