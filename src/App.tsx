import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import "./App.css";
import type { ControlOutcome, FleetSnapshot, HostStatus, TelemetrySummary } from "./fleet";
import { useAppSettings } from "./settings";

function hostDetail(host: HostStatus) {
  if (host.connection === "offline") {
    if (host.loaded_model_id) return `${host.loaded_model_id} (last seen)`;
    return host.models.length > 0
      ? `${host.models.length} cached model${host.models.length === 1 ? "" : "s"}`
      : "Unavailable";
  }
  if (host.loaded_model_id) return host.loaded_model_id;
  return "Idle";
}

function memoryDetail(host: HostStatus) {
  if (host.memory_used_bytes === null || host.memory_total_bytes === null) return null;
  const gib = 1024 ** 3;
  return `${(host.memory_used_bytes / gib).toFixed(1)} / ${(
    host.memory_total_bytes / gib
  ).toFixed(1)} GB`;
}

async function revealWindow() {
  const appWindow = getCurrentWindow();
  await appWindow.show();
  await appWindow.setFocus();
}

function App() {
  useAppSettings();
  const [fleet, setFleet] = useState<FleetSnapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [snapshotError, setSnapshotError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [telemetry, setTelemetry] = useState<TelemetrySummary | null>(null);
  const [telemetryRange, setTelemetryRange] = useState<24 | 168>(24);
  const [statusVisible, setStatusVisible] = useState(false);
  const snapshotRequest = useRef(0);
  const snapshotMounted = useRef(true);
  const trayActionHandler = useRef<(payload: string) => void>(() => {});

  const requestSnapshot = useCallback(async (command: "get_fleet_snapshot" | "refresh_fleet") => {
    const request = ++snapshotRequest.current;
    try {
      const next = await invoke<FleetSnapshot>(command);
      if (snapshotMounted.current && request === snapshotRequest.current) {
        setFleet(next);
        setSnapshotError(null);
      }
      return next;
    } catch (reason) {
      if (snapshotMounted.current && request === snapshotRequest.current) {
        setSnapshotError(String(reason));
      }
      throw reason;
    }
  }, []);

  const readSnapshot = useCallback(async () => {
    try {
      await requestSnapshot("get_fleet_snapshot");
    } catch {
      // The latest request records the error; the next poll retries.
    }
  }, [requestSnapshot]);

  const refresh = useCallback(async () => {
    setBusy(true);
    try {
      await requestSnapshot("refresh_fleet");
      setError(null);
    } catch (reason) {
      setError(String(reason));
      await revealWindow();
    } finally {
      setBusy(false);
    }
  }, [requestSnapshot]);

  const controlService = useCallback(
    async (command: "restart_local_llama_swap" | "stop_local_llama_swap") => {
      setBusy(true);
      try {
        let outcome = await invoke<ControlOutcome>(command, { force: false });
        if (outcome.state === "conflict") {
          await revealWindow();
          const verb = command === "stop_local_llama_swap" ? "stop" : "restart";
          if (
            !window.confirm(
              `${outcome.active_requests} active request(s) are using the local host. ` +
                `Cancel them and ${verb} llama-swap?`,
            )
          ) {
            return;
          }
          outcome = await invoke<ControlOutcome>(command, { force: true });
        }
        if (outcome.state === "conflict") {
          setError("The local service remained busy and was not changed.");
          return;
        }
        setError(null);
        await readSnapshot();
      } catch (reason) {
        setError(String(reason));
        await revealWindow();
      } finally {
        setBusy(false);
      }
    },
    [readSnapshot],
  );

  const loadModel = useCallback(
    async (host: HostStatus, modelId: string) => {
      setBusy(true);
      try {
        let outcome = await invoke<ControlOutcome>("load_model", {
          hostId: host.id,
          modelId,
          force: false,
        });
        if (outcome.state === "conflict") {
          await revealWindow();
          if (
            window.confirm(
              `${host.display_name} has ${outcome.active_requests} active request(s). ` +
                "Cancel them, unload the current model, and continue?",
            )
          ) {
            outcome = await invoke<ControlOutcome>("load_model", {
              hostId: host.id,
              modelId,
              force: true,
            });
          }
        }
        if (outcome.state !== "conflict") setError(null);
        await refresh();
      } catch (reason) {
        setError(String(reason));
        await revealWindow();
      } finally {
        setBusy(false);
      }
    },
    [refresh],
  );

  const unloadHost = useCallback(async (host: HostStatus) => {
    let outcome = await invoke<ControlOutcome>("unload_host", {
      hostId: host.id,
      force: false,
    });
    if (outcome.state === "conflict") {
      await revealWindow();
      if (
        window.confirm(
          `${host.display_name} has ${outcome.active_requests} active request(s). ` +
            "Cancel them and force unload?",
        )
      ) {
        outcome = await invoke<ControlOutcome>("unload_host", {
          hostId: host.id,
          force: true,
        });
      }
    }
    return outcome;
  }, []);

  const runUnloadHost = useCallback(
    async (host: HostStatus) => {
      setBusy(true);
      try {
        const outcome = await unloadHost(host);
        if (outcome.state !== "conflict") setError(null);
        await refresh();
      } catch (reason) {
        setError(String(reason));
        await revealWindow();
      } finally {
        setBusy(false);
      }
    },
    [refresh, unloadHost],
  );

  const unloadAll = useCallback(async () => {
    setBusy(true);
    try {
      let current: FleetSnapshot;
      try {
        current = await requestSnapshot("refresh_fleet");
      } catch (reason) {
        await revealWindow();
        setError(`Unable to refresh the fleet before unloading: ${String(reason)}`);
        return;
      }

      const targets = current.hosts.filter(
        (candidate) => candidate.connection !== "offline",
      );
      await revealWindow();
      if (targets.length === 0) {
        setError("No online hosts are available to unload.");
        return;
      }
      if (
        !window.confirm(
          `Unload models on all ${targets.length} online host${targets.length === 1 ? "" : "s"}?`,
        )
      ) {
        return;
      }

      const issues: string[] = [];
      for (const host of targets) {
        try {
          const outcome = await unloadHost(host);
          if (outcome.state === "conflict") {
            issues.push(`${host.display_name}: force unload declined`);
          }
        } catch (reason) {
          issues.push(`${host.display_name}: ${String(reason)}`);
        }
      }

      try {
        await requestSnapshot("refresh_fleet");
      } catch (reason) {
        issues.push(`final refresh: ${String(reason)}`);
      }
      if (issues.length > 0) {
        setError(`Unload all incomplete — ${issues.join("; ")}`);
      } else {
        setError(null);
      }
    } catch (reason) {
      setError(String(reason));
      await revealWindow();
    } finally {
      setBusy(false);
    }
  }, [requestSnapshot, unloadHost]);

  const unloadLocal = useCallback(async () => {
    const local = fleet?.hosts.find((host) => host.id === fleet.local_host_id);
    if (local) await runUnloadHost(local);
  }, [fleet, runUnloadHost]);

  useEffect(() => {
    let disposed = false;
    let unlistenOpen: (() => void) | undefined;
    let unlistenClose: (() => void) | undefined;
    listen("status-window-opened", () => setStatusVisible(true)).then((cleanup) => {
      if (disposed) cleanup();
      else unlistenOpen = cleanup;
    });
    listen("status-window-closing", () => setStatusVisible(false)).then((cleanup) => {
      if (disposed) cleanup();
      else unlistenClose = cleanup;
    });
    return () => {
      disposed = true;
      unlistenOpen?.();
      unlistenClose?.();
    };
  }, []);

  useEffect(() => {
    snapshotMounted.current = true;
    void readSnapshot();
    const timer = statusVisible ? window.setInterval(readSnapshot, 5_000) : undefined;
    return () => {
      snapshotMounted.current = false;
      snapshotRequest.current += 1;
      if (timer !== undefined) window.clearInterval(timer);
    };
  }, [readSnapshot, statusVisible]);

  useEffect(() => {
    let disposed = false;
    const readTelemetry = async () => {
      try {
        const summary = await invoke<TelemetrySummary>("get_telemetry_summary", {
          rangeHours: telemetryRange,
        });
        if (!disposed) setTelemetry(summary);
      } catch {
        // Telemetry is supplemental; fleet controls remain available if history is unreadable.
      }
    };
    void readTelemetry();
    const timer = statusVisible ? window.setInterval(readTelemetry, 30_000) : undefined;
    return () => {
      disposed = true;
      if (timer !== undefined) window.clearInterval(timer);
    };
  }, [statusVisible, telemetryRange]);

  trayActionHandler.current = (payload: string) => {
    if (payload === "unload_local") return void unloadLocal();
    if (payload === "unload_all") return void unloadAll();
    if (payload === "refresh") return void refresh();
    if (payload === "service_restart") {
      return void controlService("restart_local_llama_swap");
    }
    if (payload === "service_stop") return void controlService("stop_local_llama_swap");

    try {
      const [action, encodedHostId, encodedModelId] = payload.split("::");
      if (!encodedHostId) return;
      const hostId = decodeURIComponent(encodedHostId);
      const host = fleet?.hosts.find((candidate) => candidate.id === hostId);
      if (!host) return;
      if (action === "unload_host") return void runUnloadHost(host);
      if (action === "load_model" && encodedModelId) {
        return void loadModel(host, decodeURIComponent(encodedModelId));
      }
    } catch (reason) {
      setError(`Invalid tray action: ${String(reason)}`);
    }
  };

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    listen<string>("tray-action", ({ payload }) => {
      trayActionHandler.current(payload);
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const onlineCount = useMemo(
    () => fleet?.hosts.filter((host) => host.connection !== "offline").length ?? 0,
    [fleet],
  );
  const activeRequests = useMemo(
    () =>
      fleet?.hosts.reduce(
        (total, host) =>
          host.connection === "offline" ? total : total + host.active_requests,
        0,
      ) ?? 0,
    [fleet],
  );
  const availableTextModels = useMemo(() => {
    const models = new Set<string>();
    for (const host of fleet?.hosts ?? []) {
      if (host.connection === "offline" || !host.loaded_model_id) continue;
      const profile = host.models.find((model) => model.id === host.loaded_model_id);
      const servesText =
        profile?.kind === "text" &&
        profile.capabilities.some((capability) =>
          ["chat", "completions", "responses", "anthropic_messages"].includes(capability),
        );
      if (servesText) models.add(`${host.id}/${host.loaded_model_id}`);
    }
    return models;
  }, [fleet]);

  return (
    <main className="shell">
      <header>
        <div>
          <h1>Agent Relay Status</h1>
        </div>
        <p className="fleet-summary">
          {onlineCount}/{fleet?.hosts.length ?? 0} online
          {activeRequests > 0 && ` · ${activeRequests} active`}
          {busy && " · updating…"}
        </p>
      </header>

      <section aria-labelledby="fleet-heading">
        <h2 id="fleet-heading">Hosts</h2>
        <div className="host-list" aria-busy={!fleet || busy}>
          {!fleet && <p className="loading">Reading fleet configuration…</p>}
          {fleet?.hosts.map((host) => (
            <article className="host" key={host.id} title={host.error ?? undefined}>
              <span className={`indicator ${host.connection}`} aria-hidden="true" />
              <div className="host-copy">
                <div className="host-title">
                  <h3>{host.display_name}</h3>
                  {host.id === fleet.local_host_id && <span className="local-badge">Local</span>}
                </div>
                <p>{host.hardware}</p>
              </div>
              <div className="host-state">
                <strong>{hostDetail(host)}</strong>
                <span>
                  {host.connection}
                  {host.connection !== "offline" &&
                    host.active_requests > 0 &&
                    ` · ${host.active_requests} active`}
                </span>
                {host.connection !== "offline" && memoryDetail(host) && (
                  <span>{memoryDetail(host)}</span>
                )}
                {host.connection !== "offline" && host.tokens_per_second !== null && (
                  <>
                    <span>{host.tokens_per_second.toFixed(1)} tok/s latest request</span>
                    {host.throughput_concurrency > 1 &&
                      host.aggregate_tokens_per_second !== null && (
                        <span>
                          {host.aggregate_tokens_per_second.toFixed(1)} tok/s aggregate ·{" "}
                          {host.throughput_concurrency} concurrent
                        </span>
                      )}
                  </>
                )}
              </div>
            </article>
          ))}
        </div>
      </section>

      <section className="telemetry-section" aria-labelledby="activity-heading">
        <div className="section-title-row">
          <h2 id="activity-heading">Activity</h2>
          <div className="range-picker" aria-label="Telemetry range">
            <button
              className={telemetryRange === 24 ? "active" : ""}
              onClick={() => setTelemetryRange(24)}
              type="button"
            >
              24h
            </button>
            <button
              className={telemetryRange === 168 ? "active" : ""}
              onClick={() => setTelemetryRange(168)}
              type="button"
            >
              7d
            </button>
          </div>
        </div>
        <div className="telemetry-grid">
          <div className="telemetry-stat">
            <strong>{telemetry?.request_count.toLocaleString() ?? "—"}</strong>
            <span>Requests</span>
          </div>
          <div className="telemetry-stat">
            <strong>{telemetry?.output_tokens.toLocaleString() ?? "—"}</strong>
            <span>Output tokens</span>
          </div>
          <div className="telemetry-stat">
            <strong>
              {telemetry?.average_tokens_per_second == null
                ? "—"
                : telemetry.average_tokens_per_second.toFixed(1)}
            </strong>
            <span>Avg tok/s</span>
          </div>
          <div className="telemetry-stat">
            <strong>
              {telemetry?.average_ttft_ms == null
                ? "—"
                : `${Math.round(telemetry.average_ttft_ms)} ms`}
            </strong>
            <span>Avg first token</span>
          </div>
        </div>
        {telemetry && telemetry.models.length > 0 ? (
          <div className="model-activity-list">
            {telemetry.models.slice(0, 5).map((model) => (
              <div className="model-activity" key={`${model.host_id}/${model.model_id}`}>
                <span>
                  <strong>{model.model_id}</strong>
                  <small>{model.host_id}</small>
                </span>
                <span>
                  <strong>{model.request_count} req</strong>
                  <small>
                    {model.average_tokens_per_second == null
                      ? "No timing"
                      : `${model.average_tokens_per_second.toFixed(1)} tok/s`}
                    {model.failed_requests > 0 && ` · ${model.failed_requests} failed`}
                  </small>
                </span>
              </div>
            ))}
          </div>
        ) : (
          <p className="telemetry-empty">Completed generations will appear here.</p>
        )}
      </section>

      {error || snapshotError || fleet?.peer_api.error || fleet?.opencode.error || fleet?.hermes.error ? (
        <p className="notice error">
          {error
            ? `Fleet error: ${error}`
            : snapshotError
              ? `Fleet status: ${snapshotError}`
            : fleet?.peer_api.error
              ? `Tailscale peer API: ${fleet.peer_api.error}`
            : fleet?.hermes.error
              ? `Hermes sync: ${fleet.hermes.error}`
              : `OpenCode sync: ${fleet?.opencode.error}`}
        </p>
      ) : (
        <p className="notice">
          Manage hosts and models from the tray menu. Endpoint: {" "}
          <code>{fleet?.proxy_endpoint ?? "starting…"}</code>
          {fleet?.opencode.state !== "disabled" &&
            ` · OpenCode: ${fleet?.opencode.model_count ?? 0} models`}
          {fleet?.hermes.state === "synced" &&
            fleet.hermes.selected_model &&
            ` · Hermes: ${fleet.hermes.selected_model}${
              availableTextModels.has(fleet.hermes.selected_model)
                ? ""
                : " (unavailable)"
            }`}
          {fleet?.peer_api.state === "listening" && ` · Peer: ${fleet.peer_api.address}`}
        </p>
      )}
    </main>
  );
}

export default App;
