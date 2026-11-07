# GPU-aware Auto routing — 2.1.0

Auto excludes a Windows host after two outside-GPU samples of at least 30%.
The sampler runs about every 15 seconds, outside the inference path. It excludes
the Relay process, supervised runtime, and descendants; it does not rely on an
executable-name allowlist. A 60-second period below 10% restores eligibility.
The manual availability flag and Pinned routes remain independent.

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

Run an actual OpenBot conversation while gaming, then stop the game and observe
the cooldown. The already-loaded M1 model can remain selected afterward; Auto
does not move an ongoing reply or migrate hidden harness state. Test the manual
Off override when counters are unavailable. Existing Windows counter instances
can disappear as processes exit; invalid instances are ignored, while a sample
with no valid counters is unknown. No process identity is persisted in telemetry.
