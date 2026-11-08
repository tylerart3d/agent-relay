# Auto capacity routing validation — 2.2.0

## Behavior

Auto reserves an inference slot at the destination before preparing a model or
submitting a prompt. The reservation is single-use and model-bound. Active
stream lifetime owns the slot. Free loaded capacity outranks cold starts, and
cold starts outrank busy resident models. Operator availability, GPU activity,
context, API and tools approval continue to gate selection.

A native OpenCode hook adds a bounded session hint for local OpenCode/OpenBot
requests. Relay keeps only an expiring opaque affinity key in memory, strips the
header before forwarding, and excludes it from telemetry. Different conversations
can prefer different hosts; affinity yields when capacity is occupied.

## Checks

| Check | Windows x64 | macOS Apple Silicon |
| --- | --- | --- |
| Rust library tests | 221 passed | 215 passed |
| CLI tests | 8 passed | 8 passed |
| Bundled llama-swap TTL integration | 3 passed | 3 passed |
| Frontend tests | 53 passed | 53 passed |
| Native OpenCode plugin tests | 6 passed | 6 passed |
| Clippy with warnings denied | Passed | Passed |

Platform-specific tests account for the different library totals. Frontend
production compilation and synchronized version checks pass. Mac tests use
Node 22. No live model was loaded, unloaded, or given a prompt for these checks.

New regression coverage includes two simultaneous origin routers with identical
stale snapshots reserving different destinations; pinned/external work consuming
capacity; parallel-slot admission; single-use, expired and cancelled tickets;
preflight failure advancing only to the next candidate; bounded waiting counts;
per-session affinity separation/expiry; and session-header removal. A local HTTP
stream fixture verifies tool deltas arrive before completion and dropping the
response cancels upstream and releases capacity. Existing mock-runtime admission
and TTL tests verify cold starts, operator exclusion, and idle expiry semantics.

## Deployment and remaining acceptance

Both the initiating Relay and destination hosts need 2.2.0 (`routing_version: 3`)
for Auto. Older peers remain available to Pinned routes. Install native plugin
1.1.0 on the machine hosting OpenBot/OpenCode to enable conversation affinity.
Clients without that plugin still benefit from capacity-aware routing.

The production apps have not been replaced as part of this build. A live two-bot,
two-machine trial is still required after coordinated installation; mock routing
tests are not evidence that a particular pair of production models can run.
The Mac package is ad-hoc signed, not notarized.

This release does not solve OpenBot's hard-coded five-minute agent-event stall
watchdog. Relay queue status is visible in Relay; it is not fabricated assistant
text or an OpenBot progress event. Cold loading or a long silent prefill can
still exceed the harness timeout. No inference is replayed after dispatch.

## Installed follow-up: 2.2.1

WORKSTATION and M1 Pro were upgraded to 2.2.1 with routing protocol 3 and native
OpenCode plugin 1.2.0. M1's loaded Qwen process survived the app replacement.
OpenBot model discovery still returned `agentrelay`, and the affected project's
native plugin registry reported `agentrelay.compaction` active.

The compaction fix raises its output allowance from 4096 to at least 8192 and
adds a concise-summary instruction; ordinary turn limits are unchanged. Seven
plugin regression tests passed on both platforms. A native disposable test
completed on M1 with 473 output tokens and a 1395-character summary. After the
upgrade, Auto selected WORKSTATION for a second compaction, which completed with
437 output tokens and a 1413-character summary. That test-loaded model was
unloaded after confirming it was idle; M1's model remained loaded. Both synthetic
sessions were removed. The user's failed conversation was not retried or changed.
These tests validate the installed path, not all possible long conversations.
