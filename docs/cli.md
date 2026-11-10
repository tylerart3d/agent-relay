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

## Harness and service controls (0.23.0)

```text
agentrelayctl settings
agentrelayctl harnesses mini
agentrelayctl configure mini pi
agentrelayctl route hermes mini/coding-model
agentrelayctl context hermes 131072
agentrelayctl inference mini/coding-model --thinking low --thinking-tokens 1024 --temperature 0.3
agentrelayctl reload hermes mini/coding-model
agentrelayctl disconnect hermes
agentrelayctl launch pi --directory /absolute/existing/project
agentrelayctl service start
agentrelayctl service restart
agentrelayctl service stop
```

Route selects an already running compatible model without launching or restarting
the harness. Use `load` first when necessary. Hermes/OpenCode live routes follow
later selections; other harnesses may need a new launch to read rewritten config.
Route clients: `hermes`, `hermes_cli`, `opencode`, `opencode_cli`, `codex`,
`claude_code`, `pi`, `copilot`, `vscode`. Setup IDs use `open_code` and
`open_code_cli` instead. Configure modifies the installed harness configuration;
it does not install the harness software.

Context, disconnect and reload currently support the Hermes/OpenCode live routes.
Context saves the client limit; explicit reload applies it to the serving model.
Reload requires the expected qualified model, rejects changed routes, and returns
conflict while busy unless `--force` is supplied. Disconnect leaves models loaded.
Inference controls are validated against each profile's declared capabilities;
unspecified controls are preserved. `--reset` clears all saved overrides for that
model. Saved generation controls affect subsequent requests through this Relay.

Launch requires an existing folder **on the machine running this CLI**. It opens
a visible terminal with the selected harness; it is not remote code execution.
Use `hermes`/`opencode` as launch names for their CLI variants. Service operations
affect only local llama-swap. Stop/restart accept explicit `--force` for busy models.
No command changes human-controlled machine availability. A machine switched Off
refuses loads and new inference, but `unload` still works on it, so its model can
be freed without switching it back on.

The new operations use `POST /api/v1/manage`, an allowlisted, typed loopback-only
API—not arbitrary Tauri command invocation. Tests cover argument construction,
invalid operations, expected-route requirements, and HTTP conflict propagation.

## Local management API endpoints

The CLI uses these loopback-only endpoints:

- `GET /api/v1/status`
- `POST /api/v1/control/load` with `host_id`, `model_id`, and `force`
- `POST /api/v1/control/unload` with `host_id` and `force`

The API is intentionally not exposed on the Tailscale listener. The existing
peer API remains the only remote control surface.
