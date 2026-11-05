# Vision batching validation — 2026-11-05

For the separate Mac document profile, see the
[M1 Pro validation report](vision-batching-m1-validation.md).

Validated on Windows WORKSTATION, RTX 4080 16 GB, 96 GB system RAM,
Agent Relay 0.25.3, external unsloth-b10840-cuda13 llama-server.

## Selected configuration

The existing Qwen3.8-Flash-Next-IQ3_XXS-MTP-Vision-128K profile ID is retained
for caller compatibility. Its display name now ends in **Vision 4x16K**.
The legacy ID's `128K` suffix no longer describes request capacity.

* `--parallel 4 --cont-batching --ctx-size 65536 --slots`
* 16,384 tokens per slot; metadata `context_length: 16384`, `context_slots: 4`.
* MTP shared Q8 draft retained: `--spec-type draft-mtp --spec-draft-n-max 2`.
* Existing IQ3_XXS weights, projector, 1,024-token image budget, CPU expert
  placement and Q8 KV cache unchanged.
* Observed GPU use during four callers: about 13.4 GiB, including desktop use;
  about 2.3 GiB remained free. This is an observation, not a worst-case guarantee
  for four fully occupied 16K contexts.

## Matched page workload

One synthetic operations report, 1,700 × 2,200 pixels, 200 dpi, sent as a PNG
data URL through Relay `/v1/chat/completions`. Each caller requested an exact
transcription with temperature 0, seed 42, thinking disabled, streaming enabled,
prompt reuse disabled and a 1,500-token safety ceiling. The final runs each
produced 778 output tokens and stopped naturally (`finish_reason: stop`),
with zero reasoning characters and 1,050 input tokens including the image.

The table uses the same four-slot MTP configuration at every client concurrency.
Aggregate throughput divides all returned output tokens by batch wall time,
including image processing, prefill and delivery. Request latency is the mean
from submission to completion; it is not batch time divided by concurrency.

| Concurrent callers | Aggregate output tok/sec | Mean seconds/request | Batch seconds |
| ---: | ---: | ---: | ---: |
| 1 | 16.5 | 47.1 | 47.1 |
| 2 | 27.8 | 55.9 | 56.0 |
| 3 | 29.4 | 79.3 | 79.3 |
| 4 | 33.3 | 93.0 | 93.3 |

Four callers without MTP returned the same 778 tokens per request in 101.3
seconds, or 30.7 output tok/sec aggregate. MTP was about 8.6% faster overall
in this comparison. Runtime-reported decode-only rates with MTP were roughly
25.0 tok/sec per request at concurrency one and 11.6 per request at concurrency
four (about 46.4 combined during concurrent decoding).

An earlier repetitive fixture, capped at 1,500 output tokens per request,
measured 21.0 aggregate tok/sec on the original single-slot 128K profile and
39.5 on the four-slot MTP profile. A repeat four-slot run measured 37.8. Those
figures are separate from the more representative naturally stopping page
above. They illustrate sensitivity to document content and output length.

These are small throughput samples, not a full OCR accuracy evaluation or
long-duration stress test. Use four recall workers for backlog throughput;
use two when individual response latency matters more.

## HTTP integration

Use the existing qualified inference model ID. Capacity is available on the
serving machine's loopback Relay:

```text
GET /api/v1/models/Qwen3.8-Flash-Next-IQ3_XXS-MTP-Vision-128K/capacity
```

The endpoint returns `total_slots`, `busy_slots`, and each slot's
`context_window`. It is read-only, local-only and requires no loopback key.
See [the API behavior and benchmark command](vision-batching.md).

Five simultaneous short requests produced four occupied runtime slots and
five active Relay requests. All five completed with HTTP 200; none was rejected
for lack of a slot. The queued caller's first token arrived at 46.4 seconds,
after the initial four finished at approximately 38.7–39.9 seconds; it finished
at 51.0 seconds. Clients should retain a sufficiently long HTTP timeout for
queueing plus inference. Capacity counts do not reserve slots.

CPU embeddings retain their independent persistent group. OpenBot's selected
Strata profile is unchanged. Different GPU profiles still share the host's
single-active-model policy: OpenBot and OCR can switch profiles while idle;
they cannot run both GPU profiles simultaneously. Concurrent callers targeting
the same four-slot profile can share it.

Live checks returned a 1,024-dimensional embedding while vision requests were
active, a successful `READY` answer through `/clients/openbot/v1`, and another
successful `READY` after cold-loading the final vision profile again. The
OpenBot cold switch took 86.4 seconds; the vision cold switch took 60.3 seconds.
Embedding requests remained successful after both switches. The unloaded
vision capacity response stayed unloaded during the OpenBot check. The final
state has vision and CPU embeddings loaded, four idle MTP slots, and unchanged
harness selections. Unknown capacity IDs returned 404 and Strata inspection
returned the documented 400 unsupported-runtime response.

## Software checks

Windows frontend build, 223 backend tests, formatting, strict Clippy and release
build passed. The three new capacity/context regression tests also compiled
and passed on macOS Apple Silicon in an isolated checkout; no Mac deployment
was needed. The installed WORKSTATION executable is version 0.25.3.
