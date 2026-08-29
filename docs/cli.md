# Agent Relay CLI

`agentrelayctl` is the machine-readable control surface for local agents and
automation. It is installed beside Agent Relay and talks only to the loopback
endpoint at `http://127.0.0.1:38475`; remote lifecycle requests still travel
through Agent Relay over Tailscale.

Output is one compact JSON object. Add `--pretty` for interactive use. Set
`AGENTRELAY_ENDPOINT` or pass `--endpoint URL` when testing another local build.

```text
agentrelayctl health
agentrelayctl status
agentrelayctl models
agentrelayctl models --host gpu-host --running
agentrelayctl load gpu-host/coding-large
agentrelayctl unload gpu-host
agentrelayctl unload-all --force
agentrelayctl chat gpu-host/coding-large --prompt "Reply with OK"
agentrelayctl channel-routes
agentrelayctl channel-command photon chat-42 --sender +15551234567 --text "!ar status"
```

`load` accepts either `<host>/<model>` or separate `<host> <model>` arguments.
Normal load and unload operations return exit code `4` while a model is in use;
repeat with `--force` only when cancelling those requests is intentional.
`chat` uses Agent Relay's general OpenAI-compatible route and therefore loads an
idle profile on demand. Use `--stdin` for long prompts and `--max-tokens N` to
set the response ceiling.

Exit codes are stable: `0` success, `1` server error, `2` invalid arguments,
`3` connection failure, `4` lifecycle conflict, `5` unknown host or model, and
`6` unavailable host. Agents should inspect both the exit code and the top-level
`ok` field.

## Local management API

The CLI uses these loopback-only endpoints:

- `GET /api/v1/status`
- `POST /api/v1/control/load` with `host_id`, `model_id`, and `force`
- `POST /api/v1/control/unload` with `host_id` and `force`
- `GET /api/v1/channels/routes`
- `POST /api/v1/channels/command` with the channel address, sender, and text

The API is intentionally not exposed on the Tailscale listener. The existing
peer API remains the only remote control surface.
