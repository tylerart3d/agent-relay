# Recall background OCR handoff — Agent Relay 2.3.0

## Deployed behavior

WORKSTATION, M1 Pro, and MINI run 2.3.0 with routing protocol 4. M1's document
profile is designated `priority: background` in its source manifest; its generator
preserves that flag. The separate embedding group is explicitly marked as the
`embeddings` resource pool. Existing launch settings and client routes are unchanged.

An eligible interactive Auto request may reclaim a background host. Relay blocks
new background requests immediately, allows admitted pages up to 90 seconds to
finish, then cancels any remaining work on that profile and loads the approved
interactive model. Normal availability and GPU guards still apply. Loading and
prefill take additional time; 90 seconds is not an end-to-end response deadline.

OCR cannot replace a resident foreground model, even between chat requests. After
the foreground model actually unloads (normal TTL or explicit unload), new OCR
requests can load the background profile again. Relay does not replay OCR jobs.

2.3.1 (deployed 2026-11-09 to WORKSTATION, M1 Pro and MINI): when OCR has been refused with `host_occupied` for 3
minutes, the next refusal unloads the foreground model if it is idle. This stops
a polling client (for example OpenBot's once-a-minute job status checks) from
holding the foreground model past its TTL forever. Recall needs no change: keep
retrying on `Retry-After`. See `docs/auto-routing.md`.

2.3.1 also adds a WORKSTATION background OCR lane,
`workstation/Qwen3.8-Flash-Next-IQ3_XXS-MTP-Vision-OCR` (4 slots × 16K, reasoning
off), and OpenBot Auto with `prefer_order`: WORKSTATION first, M1 as fallback.
Recall probes each waiting lane once per `Retry-After`/10 seconds and skips hosts
whose `gpu_activity.busy` is true.

2.3.1 validation: Windows 236 Rust library tests, macOS 230; 57 frontend tests on
both (macOS with Node 26 needs `NODE_OPTIONS=--no-experimental-webstorage`, since
Node's built-in `localStorage` hides jsdom's); Clippy with warnings denied on both;
NSIS and DMG built. No live reclaim or release cycle has been run on 2.3.1 yet.

## Required recall changes

1. Remove `RECALL_OCR_SWAP_HOSTS=m1-pro`; never unload the bot's model for OCR.
2. Preserve the JSON error code and `Retry-After` header from HTTP errors.
   `503 host_reclaimed` means draining/reserved/cancelled background work;
   `503 host_occupied` means a foreground model remains resident.
3. Treat both as lane backpressure: park and recheck, retaining the page and
   backlog without consuming a permanent page-failure retry budget. Relay sends
   `Retry-After: 5`; polling every 10 seconds is also appropriate.
4. Keep waiting when every lane is parked. The current all-hosts-occupied path
   can otherwise end ingestion. Handle repeated refusals, not just one retry.
5. Discard incomplete OCR on cancellation. Before response headers, cancellation
   returns 503. Once streaming headers have been sent, HTTP status cannot change;
   Relay terminates the stream with an error instead of a successful finish.
6. Use `GET /api/v1/status` for `hosts[].reclaim` and `models[].priority`.
   `reclaim.reserved` includes a resident foreground model, not only active
   requests. Recheck actual admission; snapshots are advisory and can race.

Recall code and its environment have not been modified by this release.

## Validation

- Windows: 229 Rust library tests; macOS: 223. Frontend/voice: 54 tests.
- Both platforms: Clippy with warnings denied and packaged release builds.
- Regression coverage: active background selection, 90-second policy deadline
  (shortened under tests), selective cancellation, stream termination, foreground
  protection, reservation cleanup, independent embeddings, and resumed admission.
- Live WORKSTATION → M1: loaded OCR plus embeddings, reserved the interactive
  model through the peer Auto endpoint (5.8 seconds with OCR idle), received
  `503 host_occupied` for OCR while foreground was resident, and obtained a
  1,024-dimensional embedding. After explicit foreground unload, OCR loaded again.
  Test-owned models were unloaded afterward, restoring the initial idle state.
- A live six-page OCR cancellation at the 90-second deadline and a complete
  recall/OpenBot ingestion cycle remain to be exercised after recall's changes.

All three hosts advertise protocol 4. Installations and M1 source/config edits
have timestamped backups. Air-M4 remains excluded from the fleet.
