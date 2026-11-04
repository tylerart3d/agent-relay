# Nemotron VoiceChat

VoiceChat is a dedicated **voice workload**, not a chat-completions model or
an interface to an existing Hermes/OpenCode session. Text routes stay unchanged.
Click its profile under a host in the tray to open the local microphone page;
click **Start conversation** to grant microphone permission and load the model.
The page uses headphones-friendly full-duplex capture and a bounded transcript.

## Supported engines

| Runtime metadata | Engine | Input / output PCM16 mono | Tools |
| --- | --- | --- | --- |
| `mlx_voicechat` | Pinned mlx-vlm + Relay's single-model launcher | 16,000 / 22,050 Hz | Raw function events only; no result-injection API |
| `nemotron_voicechat` | NVIDIA container | 24,000 / 24,000 Hz on the wire | Native call/result events passed through |

The complete [MLX 4-bit checkpoint](https://huggingface.co/mlx-community/NemotronLabs-VoiceChat-11B-4bit)
is approximately 9.2 GB on disk. It includes speech output and differs from the
similarly named `-mlx-4bit` export for other runtimes. The pinned MLX dependency
requires Apple silicon and macOS 26+. Fitting weights does not establish
real-time speed; one connection per process is supported.

[NVIDIA's container requirements](https://github.com/NVIDIA-NeMo/Speech/blob/nemotron-labs-voicechat/voicechat_realtime_instructions/prerequisites.md)
specify Linux x86_64 and at least 80 GB NVIDIA VRAM. System RAM on a 16 GB GPU
does not meet that requirement. Model weights keep their upstream OpenMDW
license and are not bundled with Relay.

## Install MLX

Use an isolated Python 3.13 environment on the serving Mac:

```sh
python3.13 -m venv /path/to/voice-venv
/path/to/voice-venv/bin/pip install -r /path/to/agent-relay/voice/requirements-mlx.txt
/path/to/voice-venv/bin/hf download mlx-community/NemotronLabs-VoiceChat-11B-4bit \
  --revision dffd203fef7a380081da89479c55848e50b98413 \
  --local-dir /path/to/models/NemotronLabs-VoiceChat-11B-4bit
```

Merge the MLX entry from [llama-swap-voice.yaml](../examples/llama-swap-voice.yaml)
into the host configuration. Replace every placeholder; if a manifest generates
that file, update the manifest/generator as well. Upgrade the serving and
initiating hosts to use voice. Peer discovery negotiates voice support; older
remote peers keep their text inventories but do not see voice profiles.
Runtime installations remain external.

The launcher loads one local model and runs a silent inference frame before
health becomes ready. It binds loopback only and exposes only voice and health
routes. Clients use the fixed alias `agentrelay`; they cannot select arbitrary
models, trigger downloads, or execute remote model code.

## API and session protocol

Default local endpoints:

1. `GET http://127.0.0.1:38475/api/voice/models` discovers targets, online state,
   and protocol without loading models.
2. `POST http://127.0.0.1:38475/api/voice/<host>/<profile>/prepare` loads and warms
   the model; allow ten minutes for cold startup. Busy hosts return a conflict.
3. Connect to `ws://127.0.0.1:38475/api/voice/<host>/<profile>/realtime` after
   preparation succeeds. Encode each host/profile ID as a URL component.

No API key is required on loopback or the existing private peer API. Native
clients omit Origin; browsers must originate from this Relay's local voice
page. Peer paths are `/api/v1/voice/<profile>/{prepare,realtime}` on the private
peer port. Do not expose these unauthenticated interfaces publicly.

For MLX, send `session.update` with `session.model: "agentrelay"`, optional
`system_prompt`, and `seed`. Wait for `session.updated`, then continuously send
`input_audio_buffer.append` with base64 PCM16 and `sample_rate: 16000`, including
silence. Receive `response.audio.delta` at 22,050 Hz and `response.text.delta`.
End using `session.cancel` or disconnect. The page resamples the actual device
rate and sends 80 ms frames. The pinned server has no tool-result injection;
raw `response.function.delta` does not mean tools were executed.

NVIDIA instead uses nested `session.audio.input/output.format` with
`type: audio/pcm`, `rate: 24000`, optional `instructions` and `tools`.
Its model ID is `nemotron-voicechat`; Relay's URL selects the host/profile.
Receive `response.output_audio.delta`. Tool clients handle
`response.function_call_arguments.done` and return `conversation.item.create`
with a `function_call_output` item, matching `call_id`, and result string.
End with `session.close` or disconnect; `response.cancel` is not documented by
this runtime. Keep instructions/tool results ASCII.

## Lifecycle and privacy

The serving host forwards through llama-swap's
`/upstream/<profile>/v1/realtime`. Bundled v250 dispatches this through process
`ServeHTTP`: **keep `compat.ignoreWebsockets: false`** so the full connection
protects the idle TTL, which starts after disconnect. Relay also holds a
cancellable lease for active-request counts, force unload, and upgrade draining.
Voice shares the host's normal active-model slot. Ending a call releases its
session; TTL or unload terminates the server and frees resident weights.

No audio, prompts, transcripts, or tool arguments enter Relay telemetry.
Events are forwarded without decoding audio. Frames are limited to 256 KiB;
blocked writes time out after ten seconds. Neither audio nor tool results are
replayed on errors. The browser does not record audio or execute tools. Native
clients own tool authorization. Automatic resume and harness voice handoff
are not implemented.

## Validation

Tests cover two-hop audio/tool-event transport, errors, origins, health-based
cold loading, active-session unload conflicts, force cancellation, tray launch,
and protocol-specific audio formats. A real bundled llama-swap test holds a
socket past `ttl: 5`, verifies expiry after disconnect, and cold-loads again.

```sh
npm test
cargo test --manifest-path src-tauri/Cargo.toml
python -m unittest discover -s voice -p 'test_*.py'
```

These transport tests do not establish model quality or live microphone speed.
An initial 4-bit M1 Pro (64 GB) smoke test produced speech and text: 11.53 s of
synthetic speech plus silence took 17.68 s, excluding 4.11 s session setup
(real-time factor 1.53; lower is better). Post-warmup process RSS was 7.41 GiB,
which is not a complete unified-memory footprint. This fits but does not keep
pace with sustained live input. The short generated answer also contained a
factual error; this was a transport/performance check, not a quality pass.

Release 0.25.1 was also exercised through WORKSTATION's loopback proxy to the
M1 Pro peer, llama-swap, and the MLX runtime. Cold preparation took 10.38 s;
session setup took 4.39 s. The run returned 145 audio frames (11.6 s) and text.
A second live session reported one active request, rejected normal unload,
closed on force unload, and returned the host to zero active requests with no
loaded model. WORKSTATION's text model remained loaded. Windows and macOS
Rust/CLI/TTL suites and Clippy passed, as did 73 frontend tests and the Python
loader test. Real browser microphone permission/device handling and the NVIDIA
container still need hardware/manual validation.

Reference protocols: [MLX](https://github.com/Blaizzy/mlx-vlm/blob/4bb0fd62e22ba2afeb67725c6ceb0d85c1aba472/mlx_vlm/models/nemotron_voicechat/README.md),
[NVIDIA](https://github.com/NVIDIA-NeMo/Speech/blob/nemotron-labs-voicechat/voicechat_realtime_instructions/api-reference.md).
