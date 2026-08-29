# Agent Relay Channel Gateway

This small companion process connects Photon/Spectrum iMessage events to Agent Relay's loopback channel API. Agent Relay remains the source of truth for conversation routes; the gateway only normalizes transport events, enforces a sender allowlist, prevents duplicate processing, and sends command replies. Its checkpoint file is also a durable outbound reply queue: if Photon redelivers an event after an iMessage send failure, the gateway resends the saved reply without repeating the model turn or command.

Installed builds configure and supervise this process automatically. Agent Relay
loads the Photon secret from Windows Credential Manager or macOS Keychain and
passes it only through the child-process environment. For standalone development,
set `PHOTON_PROJECT_ID`, `PHOTON_PROJECT_SECRET`, and
`AGENT_RELAY_ALLOWED_SENDERS`. `AGENT_RELAY_ENDPOINT` defaults to
`http://127.0.0.1:38475`, `AGENT_RELAY_CHECKPOINT_PATH` controls the durable reply
file, and `AGENT_RELAY_ADAPTER_ID` distinguishes multiple configured adapters.
While connected, the gateway publishes a loopback heartbeat every ten seconds.

The process remains idle until the local Agent Relay election endpoint grants it
the active role. Configure the same Photon account on the primary and standby
hosts, but keep the secret in each host's environment or operating-system secret
store. The standby does not connect to Photon until the primary exceeds the
configured failover window.

```powershell
npm install
npm test
npm run typecheck
```

Ordinary messages use Agent Relay's transport-independent delivery endpoint. Direct-model routes are supported as stateless, one-turn conversations. Hermes, OpenCode, and Pi routes use persistent native sessions and may run on a different fleet host than the model. OpenCode and Pi projects resolve on the harness host; use an absolute path unless the project is directly beneath that user's home directory. The gateway never silently degrades harness routes to direct inference. Future Telegram and Discord adapters can reuse the same endpoint.

The gateway keeps Spectrum's responding indicator active while Agent Relay is working. Messages are serialized per iMessage conversation so replies stay ordered, while separate conversations run concurrently and cannot block one another during a long model turn.

Agent Relay journals each successful channel prompt and reply. A `/ar move` seeds the destination harness with that portable context on its first message, then marks the handoff complete after a successful reply. The gateway's stable transport message ID lets Agent Relay replay a completed response instead of generating it twice after a delivery retry.

Spectrum telemetry is explicitly disabled. Spectrum 12.8.0 currently pulls an older nested OpenTelemetry exporter family that npm flags for a moderate baggage-allocation advisory; do not enable telemetry until Photon updates those dependencies. Avoid `npm audit fix --force`, which currently proposes downgrading Spectrum.
