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

## Messaging route

1. Start a new OpenCode conversation and exchange one recognizable message.
2. Send `!ar attach` through Photon and select that conversation by number.
3. Send another message and confirm it appears in the same OpenCode session.
4. In **Message routes**, change only the model host with **Apply here**. Confirm
   that the OpenCode session remains the same and recalls the earlier exchange.
5. Use **Move & archive** to move the conversation to Pi and a selected project.
   Confirm the first reply uses the transferred transcript and the original
   Agent Relay session is archived. For a Hermes or OpenCode source, confirm its
   native conversation is archived only after that successful reply.
6. Resume the archived session from the tray and confirm it becomes active and
   its native Hermes or OpenCode conversation is restored.
7. Use **Start fresh** and confirm no prior transcript is injected.

## Lifecycle and recovery

1. Unload the routed model, then send a Photon message and confirm the same
   model reloads automatically.
2. Begin a generation and attempt a model change. Cancel once, then repeat and
   approve the force prompt; verify only the selected host is interrupted.
3. Restart the active gateway host and confirm the durable outbox does not send
   a duplicate reply.
4. If a standby gateway is configured, stop the primary and confirm takeover
   occurs only after the configured failure window.

Record the Agent Relay versions, host/model pair, time to first token, final
result, and any visible UI error for each failure.
