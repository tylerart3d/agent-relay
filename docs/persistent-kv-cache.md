# Persistent Qwen context cache

Status: **opt-in macOS runtime adapter**. The cache core is shared Rust code;
`runtime-cache/llama_cache.py` integrates a capability-tested llama.cpp profile
with llama-swap. It is installed separately from the tray app, so enabling it
does not require a new app installer or changes to harness routes. Windows
Strata activation remains gated on its deferred GPU benchmark.

## Retention contract

- At most **three snapshots total per host**, shared across profiles; one latest
  completed snapshot per profile in this first iteration. Pending writes also
  consume a slot. A fourth reservation evicts the least recently used eligible
  snapshot before writing. Prefer preserving the profile being replaced until
  its new save succeeds.
- Expire after **ten days without successful use**. A new snapshot starts its
  lifetime at save time; a verified restore refreshes it. Listing, failed restore
  attempts and maintenance do not. Cleanup runs before operations and hourly
  through the wrapper or a user LaunchAgent. Expired snapshots cannot be restored;
  files are removed on the next maintenance run (after wake if the host slept).
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
The inspected M1 source checkout does not include the checkpoint appendix, but
the installed **b11756 / d80619397** binary does. Its real process-restart test
passed with checkpoint reuse. Neither source inspection nor a successful
restore response alone certifies a deployed binary.

The local Strata source implements the same HTTP operations with its own session
format. It includes recurrent state and draft state. The installed Windows
binary contains that feature's markers, but this is not a functional result.
The initial isolated Qwen test could not start because other GPU applications
left insufficient VRAM. The user deferred GPU testing until the accelerator is
free. No WORKSTATION model was started or changed during the M1 rollout.

## Integration boundary

`kv_cache.rs` owns retention; `kv_cache_runtime.rs` owns bounded administrative
HTTP and save/restore validation. The native `kv-cache-probe` executable exposes
these to the POSIX wrapper. The wrapper owns one child and an exclusive runtime
lease; it binds its public loopback socket only after optional restoration.
Inference still passes through llama-swap, then a raw TCP bridge, preserving SSE,
cancellation and llama-swap's idle timer/in-flight accounting.

`cmdStop: /bin/kill -TERM ${PID}` signals only the wrapper. It closes admission,
saves a populated idle slot, then terminates its exact child. Save has a maximum
seven-second deadline; `unloadTimeout: 10` lets llama-swap force cleanup if needed.
Busy slots, force termination and timeouts may skip saving. Interrupted writes
remain pending until runtime death is verified. No shutdown depends on Relay's
tray process being alive.

The runtime executable and sibling dylibs are fully hashed. Command-line state,
all model shards, projector/draft/template files, file size and modification time
contribute to the compatibility digest. Large files use first/last MiB samples;
models must be immutable, not silently edited while preserving timestamps.
Changing context or other launch settings causes a miss. Changing the runtime
requires revalidation and updating its approved hash before launch.

A refused or partial restore kills the fresh child, discards the unusable snapshot
and starts clean before exposing inference. llama.cpp can omit `n_prompt_tokens`
until its first generation, even after a restore. The wrapper explicitly uses
`restore-fresh` only for a new child with no admitted requests. That path verifies
the native restore's filename, slot and token count plus post-restore idle state;
generic `restore` refuses an unknown token count. Known occupied/busy slots are
always refused. Real prefix reuse is separately required by the benchmark.

See [adapter installation and rollback](../runtime-cache/README.md). Keep the
wrapper configuration in the host's source model manifest/generator. Do not rely
on a one-off edit of generated YAML. Cache UI controls and Windows lifecycle
integration are still future work.

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
An optional `image_path` pointing to a synthetic PNG repeats the same image in
all three requests, testing multimodal prefix reuse without recording image or
answer content in the report. Report cached text separately from image work:
restoring a slot does not guarantee that the vision encoder skips the image.

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
- M1 macOS: the same cache tests and real llama-swap lifecycle regression cover
  save-on-TTL, request duration exceeding TTL, cancellation, corrupt-cache cold
  fallback and missing fresh-slot metadata.
- Live Strata restart benchmark: **not passed yet**. Initial startup failed for
  lack of free VRAM while other GPU applications were running; further GPU
  testing is explicitly deferred until the user releases the accelerator.
- M1 Qwen, 128K context, synthetic 2,728-token prompt: cold request **34.910 s**,
  resident warm request **0.416 s**, request after native restart/restore
  **0.926 s**, reusing **2,712 tokens**. These timings exclude model-weight loading.
  The **556,135,132-byte** snapshot included three recurrent checkpoints; save
  took **198 ms**, restore **288 ms**. The capability test disabled the runtime's
  RAM prompt cache to ensure disk restore was responsible for reuse.
- Production Relay path on M1: manual unload saved the snapshot in **1.579 s**;
  the next request automatically loaded a different native PID, restored **2,712
  cached tokens**, and replied in **9.348 s including model startup**. The snapshot
  use timestamp advanced, Qwen remained loaded, and client configuration was
  byte-for-byte unchanged. Hourly maintenance is enabled independently of the tray.
- Longer real conversations can produce much larger snapshots and exceed the
  bounded save deadline; those unloads fall back to cold startup. Long-context
  latency/disk-full acceptance and a cache-status/clear-cache UI remain follow-up
  work. No GPU test runs automatically in the ordinary unit-test suite.

### OpenBot vision profile — 2026-11-07

The M1's dedicated single-slot 128K OpenBot vision profile now uses the tested
cache wrapper. The source manifest carries its cache settings and 1,024-token
checkpoint spacing. Other profiles, client routes, and the production
**1,800-second idle timeout** are preserved. Retention remains three snapshots
per host and ten unused days; WORKSTATION Strata is still not enabled.

- Text-only input on this vision-capable profile: 11,173 prompt tokens took
  **80.631 s** cold, **0.320 s** resident warm, and **0.356 s** after a native
  process restart and restore. Both warm requests reused **11,157 tokens**.
  The saved state was 655,099,072 bytes. These timings exclude weight loading.
- Synthetic image input: 3,754 prompt tokens took **35.780 s** cold,
  **20.305 s** resident warm, and **20.149 s** after restart/restore. Both warm
  runs reused **2,712 text-prefix tokens** and reprocessed the image portion.
  The snapshot was 415,595,904 bytes. This is prefix reuse, not cached vision
  encoder output or evidence that arbitrary long image histories fit a deadline.
- Live Relay lifecycle with a temporary five-second TTL: streaming image
  requests ran **38.854 s** cold and **19.950 s** warm without being unloaded
  mid-request. The host unloaded after **6.650 s** idle, including serialization
  and shutdown. The next request cold-loaded the model, restored **2,712 cached
  tokens**, and completed in **24.569 s**, including startup. Each request had
  answer content, `finish_reason: stop`, and the final SSE completion marker.
  The production 1,800-second timeout was restored after the test.

Disk caching addresses unload/reload overhead. It does not remove single-slot
queueing, an unrelated conversation's prefill, or OpenBot's five-minute agent
silence limit; see [OpenBot timeout findings](openbot.md).
