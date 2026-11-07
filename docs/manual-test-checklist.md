# Manual Fleet Test Checklist

Use this checklist after automated tests pass on Windows and Apple Silicon.
Keep prompts short and avoid changing more than one routing dimension per step.

## Connector context

1. Set Hermes or OpenCode to 262144 tokens in Agent Relay Settings.
2. Connect the client to a loaded fixed-context model on another host.
3. Confirm the serving runtime reports 262144, not only the client config.
4. Connect again and verify the model is not reloaded when the context matches.
5. Repeat with an MLX or MTPLX profile and verify its native launch command is
   unchanged.

## Auto routing and availability

1. Approve two model/host pairs, enable Auto for one harness, and leave a second
   harness pinned. Confirm their model lists expose the stable `agentrelay` alias.
2. Send a short streamed prompt. Verify the loaded approved model wins over a
   preferred but cold model and the tray reports the actual last assignment.
3. Turn that host Off during a reply. Confirm the reply finishes, the model stays
   loaded, and the next Auto request uses the other eligible host. A pinned
   request to the disabled host must fail clearly.
4. Leave another model loaded on an approved host. Confirm Auto cannot replace
   it. Repeat with an unapproved tools model, insufficient context, and an image
   sent to a text-only model; none may be selected.
5. Unload both approved models, then send one request. Confirm one idle host
   cold-starts, streaming is incremental, and cancellation releases the request.
6. Switch a host Off remotely and restart its Relay app. Confirm Off persists.
   Verify CLI force-load cannot bypass Off and the CLI cannot toggle it.
7. Disconnect an upstream after its first tool delta. Confirm Relay reports the
   failure without replaying the request on another model.
8. Test OpenCode GUI/CLI, Hermes GUI/CLI, OpenBot, Codex, Pi, Claude Code,
   Copilot and VS Code. Existing v1 configurations may need Configure harnesses
   once to move to client-scoped URLs. Check the working folder on CLI launch.

## Lifecycle and recovery

1. Let idle TTL unload a pinned model, then request it and verify cold loading.
2. Attempt a manual model change during generation; test cancel and force prompts.
3. Verify CPU embeddings continue alongside a foreground chat/vision model.
4. Confirm no gateway process or message-route control is packaged in v2.

Record versions, host/model pair, time to first token, final result, and visible
errors. Mock-based tests do not replace this hardware acceptance pass.
