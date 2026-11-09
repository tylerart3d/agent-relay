# GPU-aware Auto routing — 2.1.0

Auto excludes a Windows host after two outside-GPU samples of at least 30%.
The sampler runs about every 15 seconds, outside the inference path. It excludes
the Relay process, supervised runtime, and descendants; it does not rely on an
executable-name allowlist. A 60-second period below 10% restores eligibility.
The manual availability flag and Pinned routes remain independent.

Superseded thresholds (after 2.3.0): ordinary desktop apps at 24-35% kept the
2.1.0 detector busy indefinitely, because readings never fell below 10%. The
detector now uses a one-minute rolling average: 60% or more marks busy, and below
40% for 60 seconds clears it. See `docs/auto-routing.md`.

## Verification

- Windows: 208 Rust library tests, 8 CLI tests, 3 actual llama-swap TTL tests,
  and 52 UI tests passed. Clippy with warnings denied and rustfmt passed.
- macOS: 202 library tests, 8 CLI tests, 3 TTL tests, and 52 UI tests passed;
  Clippy passed. The first TTL run failed because the Windows-created source
  archive removed the sidecar executable bit. Restoring that permission made
  all three pass; the normal macOS build script already sets it.
- Detector tests cover sustained load, cooldown, missing/stale samples,
  exclusion of model descendants, and aggregation across outside processes.
- Admission tests verify that even a loaded sticky model is excluded, the
  serving host rechecks the hint, and Pinned remains an explicit override.
- UI regression confirms the busy message does not flip the operator switch.
- Live WORKSTATION counters identified the game at approximately 84% GPU use.
  The installed detector subsequently reported `busy: true` with manual
  availability still enabled. No local model was loaded during the final check.

## M1 vision finding

The existing M1 GGUF launch included its vision projector but omitted
`vision_input` from metadata. The host's generator now advertises that capability
for projector-enabled GGUF entries. Relay also recognizes explicit legacy
`vision: true` metadata, used by the WORKSTATION Strata vision profile.

The initial M1 image request reached the correct host but failed with Metal
out-of-memory errors at 256K context. A separate 128K OpenBot vision profile
uses Q8 KV caches, smaller processing batches, and the vision encoder on CPU.
It shares the existing model/projector files; the original profile remains
unchanged. It does not share persistent KV snapshots with the original profile.

Through the OpenBot Auto endpoint, that separate profile returned `Red` for a
small red image in 16.5 seconds, with `finish_reason: stop`. A subsequent
synthetic tool-call/result round trip returned `OK` in 3.7 seconds. Headers
confirmed Auto and the M1 target. These are proxy-contract tests, not proof of
a complete OpenBot desktop agent turn or long-context image stability.

## Remaining manual checks

### Context admission correction (2.1.1)

A real tool conversation exposed an overly pessimistic Auto filter: UTF-8 bytes
were counted one-for-one as tokens. The preceding successful step reported 26,179
input tokens; the following tool result added approximately 51 KB. The failed
request body was not captured, so its exact token count cannot be reconstructed.
The filter now uses a text estimate with explicit output, image, schema, and
framing reservations; rejection messages identify individual exclusion reasons.

Regression coverage includes a 148 KB ordinary-text/tool history accepted by a
128K profile, oversized text/output/Unicode/schema keys still rejected, image
base64 excluded from text estimates, and GPU-busy/context diagnostics. Windows
passed 223 Rust tests and 52 UI tests; macOS passed 217 Rust tests. Clippy passed
on both platforms, and Windows NSIS/macOS DMG packages built successfully.

On the installed Windows 2.1.1 proxy, a separate synthetic 145,789-byte request
with a whitespace-padded tool result traversed `/clients/openbot/v1` in Auto mode
to the already-loaded M1 vision profile. It reported 1,452 prompt tokens, returned
`READY`, and ended with `finish_reason: stop` in 8.42 seconds. This deliberately
isolates byte-versus-token admission and is not a long-context speed benchmark.
Only M1 was approved during the smoke test; the original approvals were restored.
No real tool was executed and the user's failed conversation was not replayed.

The M1 tray process was absent before the smoke test while its model server
remained ready. Reopening the existing M1 app restored peer reachability without
unloading the model; the cause of that app exit was not established. The macOS
2.1.1 installer is prepared, but this validation did not replace the M1 app.

### Interactive follow-up

Run an actual OpenBot conversation while gaming, then stop the game and observe
the cooldown. The already-loaded M1 model can remain selected afterward; Auto
does not move an ongoing reply or migrate hidden harness state. Test the manual
Off override when counters are unavailable. Existing Windows counter instances
can disappear as processes exit; invalid instances are ignored, while a sample
with no valid counters is unknown. No process identity is persisted in telemetry.
