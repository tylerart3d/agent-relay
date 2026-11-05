# Concurrent document transcription

Available in Agent Relay 0.25.3 and later.

llama.cpp profiles can serve several requests with `--parallel 4
--cont-batching --ctx-size 65536 --slots`. With partitioned KV cache this
provides 16,384 tokens per request. Keep image tokens, instructions and output
within that limit. Parallel slots share model weights; extra slots do not load
four copies of the model. Measure throughput on the actual runtime and GPU:
more simultaneous requests can increase individual response times.

For profiles using a fixed partitioned context, advertise per-request capacity:

```yaml
metadata:
  runtime: llama.cpp
  context_length: 16384
  context_slots: 4
```

`context_slots` must match the command's fixed `--parallel` value. It opts into
multiplying a requested context window by the slot count when Relay rewrites
the total `--ctx-size`. Existing profiles without it retain their behavior.
Do not use this setting for runtimes with different context-allocation rules.
Reducing an existing profile's per-request context also reduces the capacity
available to harnesses routed to that profile.

## Capacity discovery

On the serving machine's loopback proxy, call:

```text
GET http://127.0.0.1:38475/api/v1/models/{local-model-id}/capacity
```

Use the URL-encoded local profile ID, without the `workstation/` prefix. This
endpoint inspects **local llama.cpp models only**, does not load idle models,
does not reset idle TTL, and does not change any harness route. No loopback
authentication is required. A ready response contains:

```json
{
  "model": "document-vision",
  "loaded": true,
  "total_slots": 4,
  "busy_slots": 1,
  "slots": [
    {"id": 0, "context_window": 16384, "busy": true, "speculative": true}
  ]
}
```

The example shortens `slots`; real responses include every slot. Unloaded
models return `loaded: false`, null counts and an empty list. Start with one
normal request (which performs cold loading), then inspect capacity to size
the worker pool. Counts are instantaneous observations, not reservations.
Unknown models return 404, unsupported runtimes 400, and failed or malformed
runtime inspection 503. Inspection is bounded to five seconds and 256 KiB;
the response excludes prompts, generation settings and task identifiers.

Inference continues through llama-swap's normal dispatch path, preserving
streaming, cancellation and idle accounting. Monitoring alone reads the
already-running backend's `/slots`. CPU embeddings remain independently
available. Changing the profile configuration still requires an idle service
restart and can unload all models supervised by that llama-swap instance.

## Reproducible throughput checks

Use a synthetic page without personal information, keep the same page and
generation settings between runs, and benchmark with the model already warm:

```text
python scripts/benchmark-vision.py --image page.png --model HOST/PROFILE --label four-slots-mtp --output results.jsonl
```

This streams through Relay at client concurrency 1, 2, 3 and 4. It caps each
response at 1,500 tokens, disables thinking, and reports wall time, first-token
latency, completion usage, finish reason and runtime decode timings. Aggregate
throughput includes image/prompt processing and queue time. No image, prompt
or generated text is written to the result file. This is a bounded throughput
test, not an OCR quality evaluation or a promise of speed on all documents.
