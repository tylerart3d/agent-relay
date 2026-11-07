# Agent Relay 2.0.0 validation

## Scope

Version 2 removes the Photon gateway and introduces client-specific Auto routing,
ordered model approvals, and persistent host availability. Existing routes
migrate to Pinned. No production installation or loaded model was changed during
this build.

## Automated checks

| Check | Windows x64 | macOS Apple Silicon |
| --- | --- | --- |
| UI tests | 51 passed | 51 passed |
| Rust library tests | 203 passed | 197 passed |
| CLI tests | 8 passed | 8 passed |
| llama-swap TTL integration tests | 3 passed | 3 passed |
| Clippy, warnings denied | Passed | Passed |

The library totals differ because Windows-specific tests are conditionally
compiled. Formatting and synchronized version checks also pass. The Mac build
uses Node 22; Node 26's changed localStorage behavior conflicts with the current
UI test environment.

Regression coverage includes approval order, loaded-model affinity, unavailable
and old peers, context and capability filtering, independent empty allowlists,
CPU embeddings alongside foreground models, and another model still starting.
Admission tests use a mock runtime to check cold loading, Off persistence,
force-load rejection, and no unintended unload or cancellation. TTL tests use
the bundled llama-swap with a controlled streaming upstream.

## Release acceptance still required

The Windows installer and Mac disk image are build artifacts, not deployed
releases. The Mac app uses ad-hoc signing and is not notarized. Run the
[manual acceptance checklist](manual-test-checklist.md) on upgraded test hosts
before merging/deploying fleet-wide: real harness tool calls, streaming,
cancellation, cross-host fallback, tray layout, and cold loading must be checked
with actual models. An older peer remains usable for Pinned routes but cannot
participate in Auto until upgraded.

Auto retains the previous eligible model per client, not per conversation.
It never retries inference after dispatch. Context admission is conservative
and does not replace runtime tokenization. Availability is an operator control
on the existing trusted peer network, not a separate authentication boundary.
