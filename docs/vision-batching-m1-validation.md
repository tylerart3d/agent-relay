# M1 Pro document batching validation — 2026-11-05

Validated on M1 Pro with 64 GB unified memory, Agent Relay 0.25.3 and
llama.cpp b11756 (d80619397). This is a separate document profile; the existing
128K chat profile, its persistent single-slot KV cache, and harness selections
remain unchanged. No model weights were duplicated or downloaded.

## Configuration

Inference model ID: `m1-pro/qwen3.8-flash-next-iq4xs-ocr`.

* Existing Qwen3.8 Flash Next IQ4_XS GGUF shards and vision projector.
* Final configuration after the six-slot trial below:
  `--parallel 6 --cont-batching --ctx-size 98304 --slots`: 16,384 tokens per slot.
* Metadata: `context_slots: 6`, `context_length: 16384`, runtime `llama.cpp`.
* Q8 K/V cache, all layers on Metal, batch 1,024, microbatch 256.
* Image budget 1,024 tokens, temperature 0, thinking disabled, idle TTL 1,800s.
* No MTP draft configured on this Mac. This profile launches llama-server
  directly; it does not use the chat profile's single-slot disk-cache wrapper.

The host's model manifest and generator now support a shared GGUF destination
and explicit slot count. Generation validates divisible total context, matching
`--parallel`, and rejects multi-slot persistent snapshots. A host-side regression
test verifies those guards, shared weights, and preservation of the chat cache.
All pre-existing generated model definitions were compared with the backup and
are unchanged.

## Initial four-slot results

Used the same synthetic 1,700 × 2,200 pixel, 200 dpi operations-report page as
the [WORKSTATION comparison](vision-batching-validation.md). Requests streamed
through Relay with temperature 0, seed 42, thinking disabled, prompt reuse off,
and a 1,500-token output ceiling. Each returned 778 output tokens, 1,050 prompt
tokens, zero reasoning characters, and natural `stop` completion.

Aggregate output throughput includes image processing, prefill and delivery.
Mean latency is measured per request, not batch duration divided by callers.

| Concurrent callers | Aggregate output tok/sec | Mean seconds/request |
| ---: | ---: | ---: |
| 1 | 19.0 | 41.0 |
| 2 | 23.0 | 67.7 |
| 3 | 27.0 | 86.3 |
| 4 | 27.9 | 111.5 |

Four callers improved throughput about 47% over one. Three callers deliver
nearly the same throughput with lower individual latency; use four for maximum
measured backlog throughput, or three for that tradeoff. An initial two-slot
32K configuration reached 21.8 aggregate tok/sec with two callers.

Observed model-process resident memory was about 46.6 GiB under four callers.
System swap was already around 3.8 GiB before testing and did not increase
during the measured runs. These short-page results do not establish worst-case
memory use with four fully occupied 16K contexts, sustained performance, or OCR
accuracy across different documents.

## Six-slot follow-up

At the user's request, increased only the document profile to six slots with
98,304 total context. All other generated profiles were compared with the
immediate backup and remained identical; the generator regression test passed.
Repeated the same page workload with four and six callers on this configuration:

| Concurrent callers | Aggregate output tok/sec | Mean seconds/request |
| ---: | ---: | ---: |
| 4 | 30.1 | 103.3 |
| 6 | 32.5 | 143.6 |

All requests completed with HTTP 200, natural `stop`, 778 output tokens and zero
reasoning characters. Capacity inspection confirmed six simultaneously busy
slots, each retaining 16,384 tokens. Six callers gained 7.9% throughput over
four in this paired run, with 39% higher individual latency. The four-caller
repeat was faster than the earlier 27.9 tok/sec measurement, so do not attribute
the entire difference from that earlier run to adding slots. Each measurement
is a single batch, not a statistical or sustained-load benchmark.

Observed process RSS peaked at 47.1 GiB. Existing system swap remained exactly
3,728.06 MiB throughout the sampled run, with reported free memory around
10–12%. This does not prove capacity for six fully occupied 16K contexts.
Six slots are left configured and loaded idle for backlog throughput; callers
can still choose three or four workers to reduce per-request latency. Chat's
128K context and single-slot cache remain unchanged.

## Initial four-slot queue and routing checks

Five simultaneous requests with a 128-token ceiling produced four occupied
slots and five active Relay requests. All completed with HTTP 200. The first
four finished at 45.3–45.8 seconds; the queued request produced its first token
at 54.0 seconds and finished at 60.0 seconds. Their `length` finish reasons were
intentional for this queue test. Clients must allow time for queueing and work.

The serving Mac exposes capacity locally, without a loopback API key:

```text
GET http://127.0.0.1:38475/api/v1/models/qwen3.8-flash-next-iq4xs-ocr/capacity
```

It initially reported four slots (now six), each with `context_window: 16384`
and `speculative: false`. This is a local-only inspection endpoint, not remote capacity forwarding
through another host's Relay. See [API semantics](vision-batching.md).

A vision request through WORKSTATION's loopback `/v1/chat/completions` using
the qualified M1 model ID successfully returned the synthetic page's title in
8.0 seconds. Final status showed the document profile loaded, four idle slots,
zero active requests and no host error.

Chat and document profiles still share one active model per host. This change
enables concurrent requests to the document profile; it does not enable two
separate chat/document model processes simultaneously. Existing chat routes
continue to select their original 128K profile when used.
