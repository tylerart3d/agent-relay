# Telemetry and Grafana

Agent Relay records lightweight operational history in `telemetry.sqlite3` in
the application configuration directory. Collection is enabled by default and
uses a bounded, nonblocking writer queue: inference streaming never waits for a
database write. If the queue is saturated, new samples are dropped instead of
slowing a model response.

## Data policy

Recorded request fields are completion time, serving host, model profile,
client route, outcome, duration, time to first token, token totals, and reported
generation rate. Lifecycle records contain load, unload, stop, and restart
outcomes. Host samples contain reachability, loaded profile, active requests,
memory, and throughput.

Agent Relay does **not** store prompt or response text, tool calls, conversation
IDs, sender identities, project paths, API keys, or channel credentials in the
telemetry database.

Detailed request events remain for seven days. Older detail is rolled into
hourly totals for 30 days and then into indefinite daily totals. Lifecycle
events remain for 30 days. One host sample per minute remains for seven days.
The status window summarizes detailed requests over the most recent 24 hours or
seven days.

## Auto routing decisions

Each Auto generation request records one decision: time, client route, outcome
(`routed`, `models_busy`, `no_eligible_model`, `client_busy`,
`routing_changed`), the chosen `host/model`, time spent waiting, and a detail
line of at most 1,024 bytes. The detail lists approved models that were passed
over and Relay's own reason for each (for example GPU busy, capacity busy, load
timeout). It never contains request text, tool data, session keys, or sender
identities. Decisions remain for seven days. Read them on the loopback listener:

```text
GET http://127.0.0.1:38475/api/v1/routing/decisions?since_ms=<epoch ms>&until_ms=<epoch ms>&limit=200
```

The defaults are the last hour, newest first, and 200 rows (at most 500).
Unloads made by the idle-foreground release rule are lifecycle events with action
`release_for_background`.

## Damaged database recovery

At startup Relay runs SQLite `quick_check` on the database, opened read-only. A
damaged file accepts connections but fails every later write, so telemetry would
otherwise stop silently. If the check fails, Relay moves the file and its `-wal`
and `-shm` journals to `telemetry-corrupt-<epoch ms>.sqlite3` and starts a fresh
database. The moved file is kept for manual recovery, for example with
`sqlite3 <file> .recover`. The telemetry summary reports `quarantined` (the moved
file name) and `write_failures` (failed writes since start; nonzero means data is
being lost).

Run Relay from one place per machine. On Windows, a Relay started inside an
MSIX-packaged app (for example from Codex) reads and writes a redirected copy of
the configuration directory. Running it both inside and outside such a container
is the likely cause of the November 2026 corruption.

## Prometheus endpoint

Every node exposes Prometheus text metrics at both endpoints:

```text
http://127.0.0.1:38475/metrics
http://<tailscale-host>:38473/metrics
```

The peer endpoint inherits the current tailnet trust boundary. It must not be
forwarded to a public interface. Metrics contain bounded host and model labels;
prompts, sessions, projects, and raw errors are never labels.

Start from [`observability/prometheus.yml.example`](../observability/prometheus.yml.example)
and import [`observability/grafana-dashboard.json`](../observability/grafana-dashboard.json)
into Grafana. Change the example target names to the MagicDNS names or Tailscale
addresses of the Agent Relay machines. Grafana's Prometheus data source should
point to the Prometheus server, not directly to Agent Relay.

The maintained WORKSTATION deployment lives in `observability/compose.yaml`.
It binds both services to loopback, retains Prometheus data for 30 days, and
provisions the data source and dashboard automatically:

```powershell
Copy-Item .\observability\prometheus.yml.example .\observability\prometheus.yml
# Edit prometheus.yml with this fleet's private host names or addresses.
docker compose -f .\observability\compose.yaml up -d
docker compose -f .\observability\compose.yaml ps
```

Open Prometheus at `http://127.0.0.1:9090` and the read-only Grafana dashboard
at `http://127.0.0.1:3000/d/agent-relay-fleet`. Named Docker volumes preserve
both databases across container replacement. Update `prometheus.yml` if a
node's address changes. The live target file is ignored by Git so private
network details are never published with the repository.

## Power and idle behavior

Agent Relay automatically enters a low-frequency idle cadence when no model is
loaded anywhere in the fleet and no control window is visible. Peer refreshes
drop from the configured active interval (five seconds by default) to 30
seconds. Local memory sampling also drops to 30 seconds when that machine has no
loaded model, avoiding repeated `nvidia-smi` launches on an idle Windows GPU.
Opening a control window or loading a model restores the active cadence; request
handling never slow down.

Direct power and energy collection remains deferred until its overhead can be
measured on both operating systems. A future implementation may record native
OS energy counters and NVIDIA board power, then correlate them with loaded
profiles and request windows.
