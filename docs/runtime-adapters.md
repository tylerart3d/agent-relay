# Runtime Adapter Architecture

The product is evolving from an LLM-only router into a local inference
control plane. A **profile** is an addressable inference target, not necessarily a
language model. Each profile advertises:

- `kind`: `text`, `image`, or `worker` (a supervised HTTP service that is not a language model, reached only through `/api/worker/<host>/<profile>/...`);
- `runtime`: the installed engine, such as `llama_cpp`, `mlx`, `ollama`, `vllm`,
  or `comfyui`;
- `capabilities`: API behaviors such as `chat`, `embeddings`,
  `image_generation`, or `workflow_queue`;
- `lifecycle_adapter`: the component responsible for load, unload, health, and
  cancellation;
- `resource_pool`: the mutually exclusive hardware pool, for example `gpu0` or
  `unified_memory`;
- `inference_controls`: optional, explicitly verified request controls for that
  exact profile, including thinking effort, reasoning-token limits, and
  temperature.

The clients use exact capability filters. OpenCode receives only text profiles
advertising `chat`, `completions`, or `responses`; an embeddings-only profile is
never presented as a generative model. Claude Code accepts
`anthropic_messages`, including profiles that do not also expose an OpenAI
endpoint. Image and workflow profiles stay out of text-client catalogs.

## Adapter contracts

All adapters implement the same internal operations: `discover`, `status`,
`load`, `unload`, `cancel`, and `proxy`. A host may run several adapters, but
only one profile may be active in a resource pool. This replaces the current
one-model-per-host rule without accidentally allowing two oversized workloads
to contend for the same GPU.

### llama-swap, vLLM, and Ollama

The llama-swap supervisor remains the lifecycle adapter for llama.cpp, MLX,
MTP-LX, vLLM, Ollama, and other process-backed OpenAI-compatible servers. vLLM
profiles launch `vllm serve` on `${PORT}` and set `--served-model-name` to the
profile's upstream name. Ollama profiles launch an isolated `ollama serve`
process on a dedicated loopback port and use llama-swap's
`useModelName` setting to map the stable profile ID to the installed Ollama
model name. Stopping or expiring either profile terminates its managed server
and releases its model memory. Agent Relay never pulls, copies, or deletes
models. See [`examples/llama-swap-runtimes.yaml`](../examples/llama-swap-runtimes.yaml).

## Inference controls

Inference controls are opt-in profile metadata. Agent Relay does not infer
thinking support from a model filename. A profile may advertise a verified
adapter, allowed effort levels, budget range and step, and temperature range.
The tray displays controls only for the selected running profile and persists
overrides under its qualified `<host>/<profile>` ID.

The `llama_cpp` adapter authors request-level `reasoning_effort`,
`thinking_budget_tokens`, and `temperature` values. Responses API requests use
the equivalent nested `reasoning.effort` field. Effort `off` sends `none` and a
zero thinking budget; `-1` means unlimited for llama.cpp. Explicit user
overrides replace harness values, while declared profile defaults fill only
settings the harness omitted. Profiles without verified metadata pass request
sampling fields through unchanged.

Models without native effort levels use explicit toggle adapters rather than
mislabeling "thinking on" as `low`. `llama_cpp_toggle` writes
`chat_template_kwargs.enable_thinking` and `thinking_budget_tokens`;
`mlx_toggle` writes the same template flag and MLX's singular
`thinking_token_budget`. The `mtplx` adapter writes `enable_thinking` and adds
`reasoning_effort` only for profiles whose backend descriptor advertises real
levels. MTPLX safety budgets remain launch-time limits and are not exposed as
request controls. `muse_system_prompt` maps Low/Medium/High/XHigh to Muse
Glimmer's documented `Reasoning strength: <level>` system directive across
Chat Completions, Responses, Anthropic Messages, and Completions requests; it
does not expose an unsupported Off mode or token budget. The UI therefore
shows Off/On for template-only models and Low/Medium/High/XHigh only where
those choices alter model behavior.

### ComfyUI

ComfyUI uses a queue route rather than pretending checkpoints are chat models.
A user-authored llama-swap profile launches ComfyUI and advertises `kind:
image` plus the `workflow_queue` capability. The first routing surface is a
transparent, host-qualified native proxy:

```text
/api/comfy/<host>/<profile>/prompt
/api/comfy/<host>/<profile>/history/<prompt_id>
/api/comfy/<host>/<profile>/view
```

The proxy permits ComfyUI's prompt, history, view, queue, interrupt, free, and
system-stat routes and loads the selected profile on first use. WebSocket relay
and workflow-template input binding remain later additions. Unloading the
profile uses the normal Agent Relay lifecycle action and terminates the managed
ComfyUI process. A later OpenAI Images adapter may map
`/v1/images/generations` to selected workflow profiles.

## Compatibility and rollout

Peer protocol v1 fields remain during migration. Profiles received without the
new fields default to a text, llama-swap-managed profile in the `default` pool.
The singular `loaded_model_id` remains until every deployed host understands an
`active_profiles` map keyed by resource pool. Internal provider IDs and config
filenames also remain stable during the user-visible rename so existing Hermes
and OpenCode connections continue to work.

Delivered foundations are capability metadata and catalog filtering,
process-backed vLLM and Ollama switching, and the bounded ComfyUI HTTP proxy.
Remaining runtime work is resource-pool concurrency, WebSocket forwarding,
workflow-template bindings, and optional image-generation compatibility.
