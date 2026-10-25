# OpenBot

Agent Relay supports [nightly-labs/OpenBot](https://github.com/nightly-labs/openbot)
through OpenBot's native custom OpenAI-compatible provider. No fork, plugin, or
model process inside OpenBot is required. Implementation inspected against
OpenBot 0.49.0; its custom provider uses an OpenCode ACP agent internally, with
provider settings supplied only to that process. Standalone OpenCode settings
and the Hermes route are independent.

## Setup on the computer running OpenBot

1. Start Agent Relay and load a chat/tool-capable model from a host menu.
2. Enable **OpenBot** under **Settings → Show Harnesses** if hidden. Choose
   **Route OpenBot**, then that host's running model. This selection persists.
3. In OpenBot's provider settings, add a custom OpenAI-compatible endpoint:

   | Field | Value |
   | --- | --- |
   | Provider ID | `agentrelay` |
   | Provider name | `Agent Relay` |
   | Base URL | `http://127.0.0.1:38475/clients/openbot/v1` |
   | Model ID | `agentrelay` |
   | Model display name | `Agent Relay` |
   | API key | Leave empty |
   | Extra headers | None |

4. Select **Agent Relay** for the OpenBot agent. OpenBot/OpenCode may display the
   provider-qualified selection as `agentrelay/agentrelay`; the HTTP request
   model remains `agentrelay`.
5. Start a fresh test conversation. If adding the provider while an agent is
   busy, let it finish before refreshing/restarting that agent: OpenBot reads
   its custom provider configuration when its OpenCode process starts.

Use the loopback address of the **OpenBot machine**, even when the selected
model runs elsewhere. Relay handles peer authentication and routing; do not
copy peer tokens into OpenBot or point it directly at a model server.
The local proxy does not require an inference API key. Do not expose this
keyless listener publicly.

## Behavior

- `GET /clients/openbot/v1/models` exposes one stable `agentrelay` model, even
  when unloaded. Its `online` field is a loaded-state hint, not a load command.
- Chat Completions, streamed tool-call deltas, and tool results use the existing
  Relay proxy. Response bytes are streamed without buffering or re-encoding.
- A request after idle eviction loads the selected model through Relay and
  llama-swap. Requests traverse llama-swap so its idle timer tracks real use.
- Stopping a response closes its upstream stream; it does not unload the model.
- Rerouting affects subsequent requests. An already-running request retains
  the target selected when it began. Reload is explicit and uses existing
  conflict/force handling. Disconnect blocks new requests without unloading.
- No selected route produces HTTP 409 `route_not_selected`; an unknown client
  produces 404 `unknown_client`. Offline hosts, load conflicts, and backend
  failures use Relay's existing error responses. An HTTP 200 alone does not
  establish that generation completed successfully.

CLI equivalents (select a running target first):

```sh
agentrelayctl route openbot '<host>/<profile>'
agentrelayctl disconnect openbot
agentrelayctl reload openbot '<host>/<profile>'
curl http://127.0.0.1:38475/clients/openbot/v1/models
```

Thinking, temperature, serving context, model availability, and idle unloading
remain Agent Relay/profile responsibilities. OpenBot does not download or
start model servers. Tool correctness and effective context capacity still
depend on the chosen model and backend.

## Validation

Regression tests cover independent route persistence, legacy config defaults,
connector dispatch, unchanged tool-call/result payloads, early SSE delivery,
and dropping an unfinished stream to close the upstream connection. Live
validation must additionally distinguish proxy-level checks from an actual
OpenBot agent turn; installation or model discovery alone is not an end-to-end
agent test.

### Validation record — 2026-10-24

- Windows: frontend build, 64 Vitest tests, 195 Rust library tests, formatting,
  and Clippy passed. macOS: application build and three OpenBot-specific Rust
  regression tests passed.
- M1 Pro: installed OpenBot 0.49.0 after SHA-256, signature, and Gatekeeper
  notarization checks. Installed Agent Relay 0.24.0 with a previous-app backup;
  retained the installed llama-swap runtime. Configured OpenBot's native
  custom-provider file, without credentials or other provider edits.
- Live M1 proxy → MINI Gemma: discovery, incremental streaming, a synthetic
  `relay_probe(21)` call, tool result `42`, final answer, and cancellation passed.
  The cold run completed in 7.86 s, with first delta at 5.62 s. The warm run
  completed in 4.38 s. The test explicitly requests a final answer after the tool
  returns; merely asking for a tool call can legitimately yield no final text.
- Live errors: unselected route returned 409; unknown client returned 404;
  malformed request returned 400. Existing client configuration was compared
  before and after deployment/testing and stayed unchanged.
- M1 Qwen was loaded and selected successfully, but its busy host prevented
  disruptive cold-load testing. A WORKSTATION profile load returned 502, so
  that backend was not used to claim a successful OpenBot test.
- **Pending:** a complete OpenBot agent turn. Its native local automation CLI
  reported no agent with Local scripts enabled. Complete onboarding, select
  the Agent Relay provider for a test agent, then enable that agent's
  **Permissions → Local scripts** to allow a controlled desktop-agent test.

Repeat the proxy checks from the computer running the configured Relay client:

```sh
python3 scripts/test-openbot-route.py --cold-load
```

This test submits synthetic prompts and explicitly unloads the selected idle
model when `--cold-load` is present. It refuses a busy host or a different loaded
model. It is distinct from the OpenBot desktop acceptance test above.
