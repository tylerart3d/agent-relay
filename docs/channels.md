# Agent Relay Channels

Agent Relay Channels is the messaging ingress layer for Photon/iMessage,
Telegram, Discord, and future adapters. It is separate from inference routing:
a channel adapter owns transport credentials and message delivery, while
Agent Relay selects a harness, model, and fleet host for each conversation.

```text
Photon / Telegram / Discord
            |
       channel adapter
            |
   sticky conversation route
            |
 Hermes / OpenCode / Pi / direct model
            |
       Agent Relay proxy
            |
 laptop / studio / GPU host
```

Only one Agent Relay installation should own a channel account at a time. In
Settings, choose a primary gateway and an optional standby. Each companion asks
its local Agent Relay process for an election decision before connecting to
Photon. The standby takes over after the configured failure window (60 seconds
by default), and ownership is sticky: a recovered primary remains on standby
until the active gateway stops. Placement changes are copied to online peers.
Session and route metadata is stored on the gateway host in `channel-routes.json`.
Completed channel prompts and replies are stored in `channel-transcripts.json`
so conversations can move between harnesses. Both are local plaintext files and
contain no adapter credentials. Tokens and API keys belong in the operating
system credential store used by each adapter.

The Settings panel accepts the Photon project ID, secret, and sender allowlist.
The project ID and allowlist are synchronized as ordinary fleet configuration;
the secret is provisioned only to the selected primary and standby over the
private peer connection and stored in Windows Credential Manager or macOS
Keychain. The packaged, self-contained gateway is then supervised by Agent Relay.
It does not require a separate Node.js installation and is stopped when Agent
Relay exits.

Each running adapter sends a loopback heartbeat every ten seconds. Agent Relay
keeps the adapter visible but marks it offline after thirty seconds without a
heartbeat. This lets the tray show a connected Photon account before its first
conversation arrives and distinguish an idle adapter from a stopped gateway.
Gateway role heartbeats are also included in peer status, so every tray can show
the same active/standby view.

The election deliberately favors availability on a private fleet. A complete,
asymmetric network partition can still let both machines believe the other is
unavailable because there is no external fencing service. Transport message IDs
and the durable reply outbox limit duplicate work, but deployments that require
strict single ownership should add an external lease before enabling automatic
failover.

An iMessage or other transport conversation is an inbox, not a harness session.
Agent Relay keeps a flat list of sessions for that inbox and one active session.
The harness host and model host are independent: `opencode@studio` may use a
model served by `gpu-host`. Archived sessions remain resumable, while native Hermes,
OpenCode, and Pi archival is reported separately according to connector capability.

## Command contract

Adapters submit possible control messages to the loopback-only channel API.
Ordinary text returns `handled: false` and continues to the selected harness.
Commands use `!ar` or `!agentrelay`. The legacy slash-prefixed forms remain
accepted, but Photon reserves slash commands and may send an extra error reply:

The normal mobile flow is deliberately short:

```text
!ar attach
1
```

`!ar attach` (or `!ar recent`) returns up to eight recent OpenCode
conversations with an available Agent Relay model route. The numbered reply
attaches the native conversation and its existing machine/model route without
loading or switching anything. If its TTL expired, the same model reloads when
the next real message arrives. `!ar route` reports the current route, and
`!ar cancel` abandons an in-progress chooser. Choosers live only in memory,
expire after ten minutes, and never expose native session IDs or project paths
in their replies.

The complete command form remains available for automation and diagnostics:

```text
!ar status
!ar hosts
!ar models gpu-host
!ar use hermes gpu-host/reasoning-q4
!ar use gpu-host/reasoning-q3
!ar new hermes@laptop laptop/compact-chat
!ar move opencode@studio gpu-host/coding-large project agent-relay
!ar move opencode@studio gpu-host/coding-large project '/Users/me/Game' session ses_abc123
!ar new pi@laptop gpu-host/reasoning project '/Users/example/Code Lab'
!ar add-to agent-relay opencode@studio gpu-host/coding-large
!ar sessions
!ar resume 3
!ar unload gpu-host
```

Add `force` to `use` or `unload` only after Agent Relay reports an active-request
conflict. A successful `use` command loads the exact model through the normal
fleet lifecycle path and then commits the new sticky route. A failed or
conflicting load does not change the conversation route. `use` changes the
active session's model or route. `new` and `move` create a new active session and
archive the previous Agent Relay session. `move` marks a portable context handoff
as pending. The first ordinary message on the destination receives the bounded
prior transcript plus the new prompt; only a successful destination reply marks
the handoff complete. Delivery retries with the same transport message ID return
the journaled reply rather than generating twice. Native harness archival remains
a separate connector capability and is reported as
`native_harness_archive: pending_connector_support`.

Supplying `session` attaches the route to an existing OpenCode conversation on
the selected harness host. Agent Relay verifies that the conversation exists,
is active, and belongs to the selected project before loading a model or changing
the route. Because that conversation already owns its context, an attachment
does not inject the portable Agent Relay transcript. The repository and files
stay on the OpenCode machine; only inference may be routed elsewhere.

## Loopback API

- `GET /api/v1/channels/routes` lists conversation routes.
- `GET /api/v1/channels/adapters` lists live and recently seen adapters.
- `POST /api/v1/channels/adapters/heartbeat` refreshes adapter presence.
- `GET /api/v1/channels/gateway/decision` tells the local companion whether it
  should be active, standby, or disabled.
- `POST /api/v1/channels/gateway/heartbeat` publishes the local gateway role.
- `POST /api/v1/channels/command` accepts `channel`, `account_id`,
  `conversation_id`, `sender_id`, and `text`.

Channel adapters must authenticate and authorize senders before forwarding a
control command. The API remains loopback-only and deliberately does not store
sender credentials.

## Tray controls

The tray exposes the same session operations as channel commands through one
**Message routes** submenu. Its first view lists recent transport conversations
using adapter-provided display labels and shows the active harness/model pair.
Selecting a conversation replaces the submenu contents in place; it does not
open a third floating window. The detail view supports:

- changing only the current model or model host (`use`);
- moving the conversation to a harness, harness host, and optional project
  (`move`);
- selecting an existing OpenCode project and conversation on that harness host;
- starting a context-free session (`new`); and
- listing or resuming archived sessions (`sessions` and `resume`).

The UI calls the same loopback command transaction as a text command. It must
show load conflicts and native-archive capability status explicitly, refresh
when an adapter changes a route, and never maintain a separate UI-only route.
The menu stays visible but disabled when no channel adapter is configured so
setup remains discoverable.

## Delivery milestones

The session foundation provides parsing, backward-compatible route migration,
flat session persistence, model lifecycle orchestration, structured conflicts,
and CLI access. The Photon adapter owns the persistent Spectrum connection,
sender allowlist, deduplication, and outbound replies. Hermes, OpenCode, and Pi
runners create persistent native sessions on the selected harness host while
routing inference to an independently selected model host. OpenCode and Pi project
paths are interpreted on the harness host. The portable transcript journal carries
context across any number of harness moves, retaining up to the 64 most recent
exchanges within a 128 KiB handoff budget. Native harness archival remains a
connector milestone. Telegram and Discord can reuse the same event and session
contracts.
