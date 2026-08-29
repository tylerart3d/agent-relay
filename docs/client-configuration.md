# Client Configuration

Agent Relay exposes a general OpenAI-compatible text API on every device:

```text
http://127.0.0.1:38475/v1
```

Model IDs on that general endpoint include their host, such as
`gpu-host/coding-large`. Hermes Desktop and OpenCode instead use dedicated
client routes that expose exactly one stable model named `agentrelay`.

## Hermes

Use Hermes' built-in custom provider and keep any placeholder API key already
configured:

```yaml
model:
  provider: custom
  base_url: http://127.0.0.1:38475/clients/hermes/v1
  default: agentrelay
  context_length: 65536
```

Select **Route Hermes** in the tray and choose a running target. Agent Relay saves
that route while Hermes continues to use the same `agentrelay` model ID. The
desktop bridge requests a fresh draft and acknowledges the switch only after
Hermes exposes that draft with the virtual model. A deferred or failed switch
is reported instead of being presented as successful. Existing chats keep the
route they started with; the newly opened session uses the new route. Set
`hermes.executable_path` in `fleet.json`
only when the CLI is not discoverable from its standard installation path or
`PATH`.

**Hermes CLI** remains a direct host-qualified launcher and does not require the
Desktop bridge plugin.

The Hermes chooser includes a context slider from 64K through 256K. Fleet
writes the selected value to `model.context_length`; current Hermes releases
hot-reload that value on the next message.

The context slider is authoritative for Hermes and OpenCode. Connecting either
client verifies the selected profile's serving context. A fixed-context
llama.cpp profile is safely reloaded with the selected value when necessary;
MLX and MTPLX profiles without a fixed advertised context keep their
runtime-managed launch behavior. Use 262144 for a 256K context window.

## OpenCode

Add an OpenAI-compatible provider to `opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "agentrelay": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "Agent Relay",
      "options": {
        "baseURL": "http://127.0.0.1:38475/clients/opencode/v1"
      },
      "models": {
        "agentrelay": {
          "name": "Agent Relay",
          "limit": { "context": 65536, "output": 16384 }
        }
      }
    }
  }
}
```

To have Agent Relay create and maintain this provider, select **Route OpenCode**
and choose a running target. OpenCode keeps `agentrelay/agentrelay` selected while
Agent Relay changes the target behind it. **OpenCode CLI** uses the same stable
route and also launches a visible terminal. The equivalent Agent Relay settings are:

```json
"opencode": {
  "enabled": true,
  "config_path": null,
  "selected_model": "gpu-host/coding-large",
  "context_window": 65536
}
```

The default target is `~/.config/opencode/opencode.json`; an existing
`opencode.jsonc` or `OPENCODE_CONFIG` value is detected. An explicit relative
`config_path` is resolved beside `fleet.json`. The synchronizer owns only
`provider.agentrelay.models` after creating the provider, preserves unrelated
settings, exposes only the virtual model, and writes the root `model` default as
`agentrelay/agentrelay` while leaving unrelated custom providers intact. Before
changing an existing file, Agent Relay preserves the first sibling `.agent-relay.bak` rollback
copy; later syncs never overwrite that pristine backup. JSONC input is
accepted, though rewritten files are normalized to JSON formatting.

The OpenCode chooser's context slider writes the selected value to the virtual
model's `limit.context`. Agent Relay supplies a compatible output limit
of at most 16K because some OpenCode configuration versions require both limit
fields. The setting affects OpenCode's history accounting and compaction, not
the context allocated by the inference server.

## Codex

Select **Codex CLI** to write a `agentrelay` custom provider to
`~/.codex/config.toml` (or `$CODEX_HOME/config.toml`) and make the chosen
host-qualified model the default. Codex custom providers require the OpenAI
Responses API, so only compatible running profiles appear. New Codex sessions
use the selection. Agent Relay opens a visible terminal in the home directory and
starts `codex` after selection.

## Claude Code

Select **Claude Code** to update `~/.claude/settings.json` with the
loopback `ANTHROPIC_BASE_URL`, a local placeholder token, and the selected
model for the default, Opus, Sonnet, Haiku, and subagent slots. Prompt caching
is disabled because local runtimes do not implement Anthropic cache controls.
Only profiles advertising the Anthropic Messages API appear. Agent Relay starts
`claude` in a new visible terminal after switching.

## Pi

Select **Pi CLI** to add a `agentrelay` provider to
`~/.pi/agent/models.json` (or `$PI_CODING_AGENT_DIR/models.json`) and select it
in the adjacent `settings.json`. Pi uses Agent Relay's OpenAI Chat Completions
route and starts `pi` in a new visible terminal.

Channel routes can also select `pi` or `pi@<harness-host>`. Agent Relay starts Pi
in JSON mode, assigns one deterministic Pi session ID to the Agent Relay session,
and reuses it for later messages and model-host changes. A project path is resolved
on the Pi harness host; relative paths are interpreted beneath that user's home
directory.

Codex, Claude Code, and Pi files preserve their first sibling
`.agent-relay.bak` copies before modification. Agent Relay preserves unrelated
providers and settings.

## GitHub Copilot CLI

Select **Copilot CLI** to persist the provider variables required by Copilot's
BYOK mode and start `copilot` in a visible terminal:

```text
COPILOT_PROVIDER_BASE_URL=http://127.0.0.1:38475/v1
COPILOT_PROVIDER_TYPE=openai
COPILOT_PROVIDER_API_KEY=agentrelay-local
COPILOT_MODEL=<host>/<profile>
```

On Windows, Agent Relay stores these as user environment variables and injects
them into the launched session. On macOS, Agent Relay writes
`~/.copilot/agentrelay.env`, sources it from a managed block in `~/.zshenv`, and
updates the current launch environment. Existing `.zshenv` files receive a
`.agent-relay.bak` backup.

Copilot CLI requires streaming and tool calling. Agent Relay restricts the chooser
to running Chat Completions profiles, but the model itself must still produce
valid tool calls; protocol compatibility alone cannot guarantee agent quality.

For every CLI integration, the main tray button relaunches the configured agent
and the adjacent chevron changes its model. If no model has been selected, the
main button opens the chooser first. Agent Relay refuses to launch a saved target
that is currently unloaded or offline, and resolves the CLI executable before
opening a terminal so a missing command is reported immediately.

## VS Code Chat and Agents

Select **Connect VS Code** to add an Agent Relay Custom Endpoint provider to the
default VS Code profile's `chatLanguageModels.json`. Agent Relay also enables the
experimental `chat.agentHost.byokModels.enabled` setting in the adjacent user
`settings.json`, preserving other providers and settings and backing up both
files before modification.

Reload VS Code, then select the Agent Relay model from the Chat model picker. The
configuration supports chat and agent sessions through Chat Completions. VS
Code does not currently allow local BYOK models to replace Copilot inline code
suggestions, semantic search, or embedding-dependent features.
