import { canonicalClient, defaultPolicy, useRouting } from "./routing";
import { invoke } from "@tauri-apps/api/core";
import { readCliProject } from "./cliProjects";
import { listen } from "@tauri-apps/api/event";
import {
  type CSSProperties,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import "./App.css";
import { type AppSettings, useAppSettings } from "./settings";
import type {
  ControlOutcome,
  FleetSnapshot,
  HostStatus,
  InferenceOverrides,
  ReasoningEffort,
} from "./fleet";
import { measureMenuHeight } from "./menuSizing";
import { getModelOptionState } from "./modelMenuState";

const MIN_CONTEXT_WINDOW = 65_536;
const MAX_CONTEXT_WINDOW = 262_144;
const CONTEXT_WINDOW_STEP = 16_384;

type ConnectorClient =
  | "hermes"
  | "hermes_cli"
  | "opencode"
  | "opencode_cli"
  | "codex"
  | "claude_code"
  | "pi"
  | "copilot"
  | "vscode"
  | "openbot";

const CONNECTOR_DETAILS: Record<
  ConnectorClient,
  {
    name: string;
    mark: string;
    command: string;
    stateKey:
      | "hermes"
      | "hermes_cli"
      | "opencode"
      | "codex"
      | "claude_code"
      | "pi"
      | "copilot"
      | "vscode"
      | "openbot";
    contextClient?: "hermes" | "opencode";
    capability?: "chat" | "responses" | "anthropic_messages";
    launches?: boolean;
    subtitle?: string;
  }
> = {
  hermes: {
    name: "Hermes",
    mark: "H",
    command: "connect_hermes_model",
    stateKey: "hermes",
    contextClient: "hermes",
    capability: "chat",
    subtitle: "Route Agent Relay to a running model and open a fresh Hermes session",
  },
  hermes_cli: {
    name: "Hermes CLI",
    mark: "H›",
    command: "connect_hermes_cli_model",
    stateKey: "hermes_cli",
    contextClient: "hermes",
    capability: "chat",
    launches: true,
  },
  opencode: {
    name: "OpenCode",
    mark: "O",
    command: "connect_opencode_model",
    stateKey: "opencode",
    contextClient: "opencode",
    capability: "chat",
    subtitle: "Route OpenCode's Agent Relay model to a running model",
  },
  opencode_cli: {
    name: "OpenCode CLI",
    mark: "O›",
    command: "connect_opencode_cli_model",
    stateKey: "opencode",
    contextClient: "opencode",
    capability: "chat",
    launches: true,
  },
  openbot: {
    name: "OpenBot",
    mark: "B",
    command: "connect_openbot_model",
    stateKey: "openbot",
    capability: "chat",
    subtitle: "Configure OpenBot with http://127.0.0.1:38475/clients/openbot/v1 and model agentrelay (no API key)",
  },
  codex: {
    name: "Codex",
    mark: "C",
    command: "connect_codex_model",
    stateKey: "codex",
    capability: "responses",
    launches: true,
  },
  claude_code: {
    name: "Claude Code",
    mark: "CC",
    command: "connect_claude_code_model",
    stateKey: "claude_code",
    capability: "anthropic_messages",
    launches: true,
  },
  pi: {
    name: "Pi",
    mark: "π",
    command: "connect_pi_model",
    stateKey: "pi",
    capability: "chat",
    launches: true,
  },
  copilot: {
    name: "Copilot CLI",
    mark: "GH",
    command: "connect_copilot_model",
    stateKey: "copilot",
    capability: "chat",
    launches: true,
    subtitle: "Select a running tool-capable model and launch a new terminal session",
  },
  vscode: {
    name: "VS Code",
    mark: "VS",
    command: "connect_vscode_model",
    stateKey: "vscode",
    capability: "chat",
    subtitle: "Add this running model to VS Code Chat; reload VS Code to use it",
  },
};

function formatContextWindow(value: number) {
  return `${Math.round(value / 1024)}K`;
}


type PendingAction =
  | { kind: "load"; host: HostStatus; modelId: string; activeRequests: number }
  | { kind: "unload"; host: HostStatus; activeRequests: number }
  | {
      kind: "service";
      host: HostStatus;
      command: "restart_local_llama_swap" | "stop_local_llama_swap";
      activeRequests: number;
    };

export function ModelMenu() {
  const autoRouting = useRouting();
  const { settings, setSettings, refreshSettings } = useAppSettings();
  const [fleet, setFleet] = useState<FleetSnapshot | null>(null);
  const [snapshotError, setSnapshotError] = useState<string | null>(null);
  const [hostId, setHostId] = useState<string | null>(null);
  const [connectorBusy, setConnectorBusy] = useState(false);
  const [connectorError, setConnectorError] = useState<string | null>(null);
  const [reloadConflict, setReloadConflict] = useState<{ model: string; active: number } | null>(null);
  const [routeChooserOpen, setRouteChooserOpen] = useState(true);
  const [actionBusy, setActionBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [pendingAction, setPendingAction] = useState<PendingAction | null>(null);
  const [menuPhase, setMenuPhase] = useState<"open" | "opening" | "closing">("open");
  const [contextDrafts, setContextDrafts] = useState<
    Partial<Record<"hermes" | "opencode", number>>
  >({});
  const [inferenceDrafts, setInferenceDrafts] = useState<Record<string, InferenceOverrides>>({});
  const shellRef = useRef<HTMLElement>(null);
  const lastHeight = useRef(0);
  const animationTimer = useRef<number | null>(null);
  const measureFrame = useRef<number | null>(null);
  const menuRequest = useRef<number | null>(null);
  const snapshotRequest = useRef(0);
  const snapshotMounted = useRef(true);
  const contextCommitBusy = useRef(false);

  const readSnapshot = useCallback(async () => {
    const request = ++snapshotRequest.current;
    try {
      const next = await invoke<FleetSnapshot>("get_fleet_snapshot");
      if (snapshotMounted.current && request === snapshotRequest.current) {
        setFleet(next);
        setSnapshotError(null);
      }
      return next;
    } catch (reason) {
      if (snapshotMounted.current && request === snapshotRequest.current) {
        setSnapshotError(String(reason));
      }
      return null;
    }
  }, []);

  const resizeMenu = useCallback((height: number, requestId: number) => {
    if (menuRequest.current !== requestId || height === lastHeight.current) return;
    lastHeight.current = height;
    void invoke("resize_model_menu", { height, requestId }).catch((reason) => {
      if (menuRequest.current === requestId) {
        lastHeight.current = 0;
        setActionError(String(reason));
      }
    });
  }, []);

  useEffect(() => {
    snapshotMounted.current = true;
    void readSnapshot();
    let timer: number | undefined;
    const stopPolling = () => {
      if (timer !== undefined) window.clearInterval(timer);
      timer = undefined;
    };
    const startPolling = () => {
      stopPolling();
      timer = window.setInterval(readSnapshot, 2_000);
    };
    let disposed = false;
    let unlistenOpen: (() => void) | undefined;
    let unlistenClose: (() => void) | undefined;
    listen<{ host_id: string; request_id: number }>("model-menu-opened", ({ payload }) => {
      if (animationTimer.current !== null) window.clearTimeout(animationTimer.current);
      if (measureFrame.current !== null) window.cancelAnimationFrame(measureFrame.current);
      menuRequest.current = payload.request_id;
      setMenuPhase("opening");
      animationTimer.current = window.setTimeout(() => setMenuPhase("open"), 145);
      setHostId(payload.host_id);
      lastHeight.current = 0;
      setContextDrafts({});
      setInferenceDrafts({});
      setConnectorError(null);
      setReloadConflict(null);
      setRouteChooserOpen(true);
      setActionError(null);
      setPendingAction(null);
      void readSnapshot();
      startPolling();
      measureFrame.current = window.requestAnimationFrame(() => {
        if (menuRequest.current !== payload.request_id) return;
        const shell = shellRef.current;
        if (!shell) return;
        const height = measureMenuHeight(shell);
        resizeMenu(height, payload.request_id);
      });
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlistenOpen = cleanup;
    });
    listen("tray-menus-closing", () => {
      if (animationTimer.current !== null) window.clearTimeout(animationTimer.current);
      if (measureFrame.current !== null) window.cancelAnimationFrame(measureFrame.current);
      menuRequest.current = null;
      setMenuPhase("closing");
      stopPolling();
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlistenClose = cleanup;
    });
    return () => {
      disposed = true;
      snapshotMounted.current = false;
      snapshotRequest.current += 1;
      stopPolling();
      if (animationTimer.current !== null) window.clearTimeout(animationTimer.current);
      if (measureFrame.current !== null) window.cancelAnimationFrame(measureFrame.current);
      unlistenOpen?.();
      unlistenClose?.();
    };
  }, [readSnapshot, resizeMenu]);

  useEffect(() => {
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") void invoke("hide_tray_menus");
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, []);

  useLayoutEffect(() => {
    const shell = shellRef.current;
    if (!shell) return;
    const measure = () => {
      const requestId = menuRequest.current;
      if (requestId === null) return;
      const height = measureMenuHeight(shell);
      resizeMenu(height, requestId);
    };
    const contentObserver = new MutationObserver(measure);
    contentObserver.observe(shell, {
      childList: true,
      characterData: true,
      subtree: true,
    });
    measure();
    return () => contentObserver.disconnect();
  }, [resizeMenu]);

  const host = useMemo(
    () => fleet?.hosts.find((candidate) => candidate.id === hostId),
    [fleet, hostId],
  );

  const connector = hostId?.startsWith("connector:")
    ? (hostId.slice("connector:".length) as ConnectorClient)
    : null;
  const shellClass = `model-menu-shell menu-${menuPhase}`;
  const runningModels = useMemo(
    () =>
      fleet?.hosts.flatMap((candidate) => {
        if (candidate.connection === "offline" || !candidate.loaded_model_id) return [];
        const profile = candidate.models.find(
          (model) => model.id === candidate.loaded_model_id,
        );
        if (
          !profile ||
          profile.kind !== "text" ||
          !profile.capabilities.some((capability) =>
            ["chat", "completions", "responses", "anthropic_messages"].includes(capability),
          )
        ) {
          return [];
        }
        return [
          {
            hostId: candidate.id,
            hostName: candidate.display_name,
            modelId: candidate.loaded_model_id,
            displayName: profile.display_name,
            runtime: profile.runtime,
            capabilities: profile.capabilities,
            inferenceControls: profile.inference_controls,
          },
        ];
      }) ?? [],
    [fleet],
  );

  const connectClient = useCallback(
    async (client: ConnectorClient, targetHostId: string, modelId: string) => {
      const requestId = menuRequest.current;
      setConnectorBusy(true);
      try {
        await invoke(CONNECTOR_DETAILS[client].command, {
          ...(readCliProject(client).selected ? { directory: readCliProject(client).selected } : {}),
          hostId: targetHostId,
          modelId,
        });
        if (requestId !== null && menuRequest.current === requestId) {
          setConnectorError(null);
          await invoke("hide_tray_menus");
        }
      } catch (reason) {
        if (requestId !== null && menuRequest.current === requestId) {
          setConnectorError(String(reason));
        }
        await readSnapshot();
      } finally {
        setConnectorBusy(false);
      }
    },
    [readSnapshot],
  );

  const setContextDraft = useCallback(
    (client: "hermes" | "opencode", contextWindow: number) => {
      setContextDrafts((current) => ({ ...current, [client]: contextWindow }));
    },
    [],
  );

  const commitContextWindow = useCallback(
    async (client: "hermes" | "opencode", contextWindow: number) => {
      const persisted =
        client === "hermes"
          ? settings?.hermes_context_window
          : settings?.opencode_context_window;
      if (persisted === contextWindow) {
        setContextDrafts((current) => {
          const next = { ...current };
          delete next[client];
          return next;
        });
        return;
      }
      if (connectorBusy || contextCommitBusy.current) return;
      contextCommitBusy.current = true;
      setConnectorBusy(true);
      try {
        setSettings(
          await invoke<AppSettings>("set_client_context_window", {
            client,
            contextWindow,
          }),
        );
        setContextDrafts((current) => {
          const next = { ...current };
          delete next[client];
          return next;
        });
        setConnectorError(null);
        await readSnapshot();
      } catch (reason) {
        setConnectorError(String(reason));
        setContextDrafts((current) => {
          const next = { ...current };
          delete next[client];
          return next;
        });
        try {
          await refreshSettings();
        } catch {
          // The persisted value remains the source of truth when refresh fails.
        }
      } finally {
        contextCommitBusy.current = false;
        setConnectorBusy(false);
      }
    },
    [connectorBusy, readSnapshot, refreshSettings, setSettings, settings],
  );

  const commitInferenceOverride = useCallback(
    async (qualifiedModel: string, inferenceOverride: InferenceOverrides) => {
      if (connectorBusy) return;
      setConnectorBusy(true);
      try {
        setSettings(await invoke<AppSettings>("set_model_inference_override", {
          qualifiedModel,
          inferenceOverride,
        }));
        setInferenceDrafts((current) => {
          const next = { ...current };
          delete next[qualifiedModel];
          return next;
        });
        setConnectorError(null);
      } catch (reason) {
        setConnectorError(String(reason));
      } finally {
        setConnectorBusy(false);
      }
    },
    [connectorBusy, setSettings],
  );

  const manageRoute = async (client: "hermes" | "opencode" | "openbot", model: string,
    action: "reload" | "disconnect", force = false) => {
    if (connectorBusy) return;
    setConnectorBusy(true);
    setConnectorError(null);
    try {
      if (action === "disconnect") {
        await invoke("disconnect_client_route", { client });
      } else {
        const outcome = await invoke<ControlOutcome>("reload_client_model", {
          client, expectedModel: model, force,
        });
        if (outcome.state === "conflict") {
          setReloadConflict({ model, active: outcome.active_requests });
          return;
        }
        if (outcome.state !== "applied" && outcome.state !== "noop") {
          throw new Error(outcome.message);
        }
      }
      setReloadConflict(null);
      await readSnapshot();
    } catch (reason) {
      setConnectorError(String(reason));
    } finally {
      setConnectorBusy(false);
    }
  };

  const loadModel = useCallback(
    async (host: HostStatus, modelId: string) => {
      setActionBusy(true);
      try {
        const outcome = await invoke<ControlOutcome>("load_model", {
          hostId: host.id,
          modelId,
          force: false,
        });
        if (outcome.state === "conflict") {
          setPendingAction({
            kind: "load",
            host,
            modelId,
            activeRequests: outcome.active_requests,
          });
          return;
        }
        setActionError(null);
        await readSnapshot();
        await invoke("hide_tray_menus");
      } catch (reason) {
        setActionError(String(reason));
      } finally {
        setActionBusy(false);
      }
    },
    [readSnapshot],
  );

  const unloadModel = useCallback(
    async (host: HostStatus) => {
      setActionBusy(true);
      try {
        const outcome = await invoke<ControlOutcome>("unload_host", {
          hostId: host.id,
          force: false,
        });
        if (outcome.state === "conflict") {
          setPendingAction({
            kind: "unload",
            host,
            activeRequests: outcome.active_requests,
          });
          return;
        }
        setActionError(null);
        await readSnapshot();
        await invoke("hide_tray_menus");
      } catch (reason) {
        setActionError(String(reason));
      } finally {
        setActionBusy(false);
      }
    },
    [readSnapshot],
  );

  const controlLocalService = useCallback(
    async (host: HostStatus, command: "restart_local_llama_swap" | "stop_local_llama_swap") => {
      setActionBusy(true);
      try {
        const outcome = await invoke<ControlOutcome>(command, { force: false });
        if (outcome.state === "conflict") {
          setPendingAction({
            kind: "service",
            host,
            command,
            activeRequests: outcome.active_requests,
          });
          return;
        }
        setActionError(null);
        await readSnapshot();
        await invoke("hide_tray_menus");
      } catch (reason) {
        setActionError(String(reason));
      } finally {
        setActionBusy(false);
      }
    },
    [readSnapshot],
  );

  const confirmPendingAction = useCallback(async () => {
    if (!pendingAction || actionBusy) return;
    setActionBusy(true);
    try {
      if (pendingAction.kind === "load") {
        await invoke<ControlOutcome>("load_model", {
          hostId: pendingAction.host.id,
          modelId: pendingAction.modelId,
          force: true,
        });
      } else if (pendingAction.kind === "unload") {
        await invoke<ControlOutcome>("unload_host", {
          hostId: pendingAction.host.id,
          force: true,
        });
      } else {
        const outcome = await invoke<ControlOutcome>(pendingAction.command, { force: true });
        if (outcome.state === "conflict") {
          setActionError("The local service remained busy and was not changed.");
          return;
        }
      }
      setPendingAction(null);
      setActionError(null);
      await readSnapshot();
      await invoke("hide_tray_menus");
    } catch (reason) {
      setActionError(String(reason));
    } finally {
      setActionBusy(false);
    }
  }, [actionBusy, pendingAction, readSnapshot]);

  if (connector) {
    const routingClient = canonicalClient(connector);
    const policy = autoRouting.routing.clients[routingClient] ?? defaultPolicy;
    const auto = policy.mode === "auto";
    const details = CONNECTOR_DETAILS[connector];
    const selectedModel = fleet?.[details.stateKey].selected_model;
    const clientName = details.name;
    const contextClient = details.contextClient;
    const hasContextControl = contextClient !== undefined;
    const contextWindow = contextClient
      ? contextDrafts[contextClient] ??
        (contextClient === "hermes"
          ? settings?.hermes_context_window
          : settings?.opencode_context_window) ??
        MIN_CONTEXT_WINDOW
      : MIN_CONTEXT_WINDOW;
    const compatibleModels = runningModels.filter(
      (model) => !details.capability || model.capabilities.includes(details.capability),
    );
    const selectedRunningModel = compatibleModels.find(
      (model) => `${model.hostId}/${model.modelId}` === selectedModel,
    );
    // A saved route outlives its loaded process (idle TTL, unload, or host sleep).
    // Read capabilities from the catalog, not just currently running models.
    const selectedProfile = fleet?.hosts.flatMap((host) =>
      host.models.filter((model) => `${host.id}/${model.id}` === selectedModel),
    ).find((model) => !details.capability || model.capabilities.includes(details.capability));
    const selectedInferenceControls = selectedProfile?.inference_controls;
    const savedInferenceOverride = selectedModel
      ? settings?.inference_overrides[selectedModel] ?? {}
      : {};
    const inferenceOverride = selectedModel
      ? inferenceDrafts[selectedModel] ?? savedInferenceOverride
      : {};
    const selectedThinking = selectedInferenceControls?.thinking;
    const selectedTemperature = selectedInferenceControls?.temperature;
    const reasoningEffort = inferenceOverride.reasoning_effort
      ?? selectedThinking?.default_effort
      ?? "";
    const reasoningBudget = inferenceOverride.reasoning_budget
      ?? selectedThinking?.default_budget
      ?? "";
    const temperature = inferenceOverride.temperature
      ?? selectedTemperature?.default
      ?? selectedTemperature?.min
      ?? 0;
    const updateInferenceDraft = (updates: Partial<InferenceOverrides>) => {
      if (!selectedModel) return;
      setInferenceDrafts((current) => ({
        ...current,
        [selectedModel]: { ...inferenceOverride, ...updates },
      }));
    };
    const action = details.launches
      ? "Launch"
      : connector === "hermes" || connector === "opencode" || connector === "openbot"
        ? "Route"
        : "Connect";
    return (
      <main className={shellClass} ref={shellRef}>
        <header className="model-menu-header connector-header">
          <span className="connector-mark" aria-hidden="true">
            {details.mark}
          </span>
          <div>
            <strong>{action} {clientName}</strong>
            <span>
              {details.subtitle ??
                (details.launches
                  ? "Select a running model and launch a new terminal session"
                  : "Select a model that is already running")}
            </span>
          </div>
        </header>

        <section className="connector-context">
          <label>Routing<select aria-label="Harness routing mode" disabled={!autoRouting.loaded || autoRouting.saving} value={policy.mode} onChange={async (e) => {
            try { await autoRouting.save({ ...autoRouting.routing, clients: { ...autoRouting.routing.clients,
              [routingClient]: { ...policy, mode: e.target.value as "auto" | "pinned" } } }); }
            catch (error) { setConnectorError(String(error)); }
          }}><option value="pinned">Pinned model</option><option value="auto">Auto</option></select></label>
          {auto && <p>Uses approved models from Settings → Auto routing.<br />{autoRouting.choices[routingClient] ? `Last assigned: ${autoRouting.choices[routingClient]}` : "Model chosen when the next request arrives."}</p>}
        </section>
        {!auto && <>
        {hasContextControl && (
          <section className="connector-context" aria-label={`${clientName} context window`}>
          <label htmlFor={`${connector}-context-window`}>
            <span>Context window</span>
            <output>{formatContextWindow(contextWindow)}</output>
          </label>
          <input
            aria-valuetext={`${formatContextWindow(contextWindow)} tokens`}
            disabled={!settings || connectorBusy}
            id={`${connector}-context-window`}
            max={MAX_CONTEXT_WINDOW}
            min={MIN_CONTEXT_WINDOW}
            step={CONTEXT_WINDOW_STEP}
            style={{
              "--range-fill": `${
                ((contextWindow - MIN_CONTEXT_WINDOW) /
                  (MAX_CONTEXT_WINDOW - MIN_CONTEXT_WINDOW)) * 100
              }%`,
            } as CSSProperties}
            type="range"
            value={contextWindow}
            onChange={(event) =>
              setContextDraft(
                contextClient!,
                Number(event.currentTarget.value),
              )
            }
            onPointerUp={(event) =>
              void commitContextWindow(
                contextClient!,
                Number(event.currentTarget.value),
              )
            }
            onKeyUp={(event) => {
              if (
                event.key.startsWith("Arrow") ||
                event.key === "Home" ||
                event.key === "End" ||
                event.key === "PageUp" ||
                event.key === "PageDown"
              ) {
                void commitContextWindow(
                  contextClient!,
                  Number(event.currentTarget.value),
                );
              }
            }}
            onBlur={(event) =>
              void commitContextWindow(
                contextClient!,
                Number(event.currentTarget.value),
              )
            }
          />
          <small>Saved client history limit. Apply &amp; reload restarts the serving model with this window.</small>
          </section>
        )}

        {selectedModel && selectedProfile && (selectedThinking || selectedTemperature) && (
          <section className="connector-inference" aria-label={`${selectedProfile.display_name} inference controls`}>
            <div className="connector-inference-title">
              <strong>Model controls</strong>
              <small>{selectedProfile.display_name}{!selectedRunningModel && " · Not loaded"}</small>
            </div>
            {selectedThinking && (
              <>
                <label>
                  <span>Thinking</span>
                  <select
                    aria-label="Thinking effort"
                    disabled={connectorBusy}
                    value={reasoningEffort}
                    onChange={(event) => updateInferenceDraft({
                      reasoning_effort: event.currentTarget.value as ReasoningEffort,
                    })}
                  >
                    {selectedThinking.efforts.map((effort) => (
                      <option key={effort} value={effort}>
                        {effort === "xhigh" ? "Extra high" : effort[0].toUpperCase() + effort.slice(1)}
                      </option>
                    ))}
                  </select>
                </label>
                {selectedThinking.budget_min !== undefined
                  && selectedThinking.budget_min !== null
                  && selectedThinking.budget_max !== undefined
                  && selectedThinking.budget_max !== null && (
                  <label>
                    <span>Reasoning limit</span>
                    <input
                      aria-label="Reasoning token limit"
                      disabled={connectorBusy || reasoningEffort === "off"}
                      max={selectedThinking.budget_max}
                      min={selectedThinking.budget_min}
                      step={selectedThinking.budget_step ?? 1}
                      type="number"
                      value={reasoningBudget}
                      onChange={(event) => updateInferenceDraft({
                        reasoning_budget: event.currentTarget.value === ""
                          ? null
                          : Number(event.currentTarget.value),
                      })}
                    />
                    <small>{selectedThinking.adapter === "strata"
                      ? "0 is unlimited; select Off to disable thinking."
                      : "-1 is unlimited; 0 ends thinking immediately."}</small>
                  </label>
                )}
              </>
            )}
            {selectedTemperature && (
              <label>
                <span>Temperature <output>{Number(temperature).toFixed(2)}</output></span>
                <input
                  aria-label="Temperature"
                  disabled={connectorBusy}
                  max={selectedTemperature.max}
                  min={selectedTemperature.min}
                  step={selectedTemperature.step}
                  type="range"
                  value={temperature}
                  onChange={(event) => updateInferenceDraft({
                    temperature: Number(event.currentTarget.value),
                  })}
                />
              </label>
            )}
            <div className="connector-inference-actions">
              <button
                disabled={connectorBusy || !inferenceDrafts[selectedModel]}
                onClick={() => void commitInferenceOverride(selectedModel, inferenceOverride)}
              >
                Save controls
              </button>
              <button
                disabled={connectorBusy || !settings?.inference_overrides[selectedModel]}
                onClick={() => void commitInferenceOverride(selectedModel, {})}
              >
                Use model defaults
              </button>
            </div>
            <small>Saved thinking and temperature controls apply to the next request; no reload needed.</small>
          </section>
        )}

        {(connector === "hermes" || connector === "opencode" || connector === "openbot") && selectedModel && (
          <section className="connector-context" aria-label="Route actions">
            <small>Reload affects every harness using this model. Saved server profile changes are picked up on restart.</small>
            <div className="connector-inference-actions">
              <button disabled={connectorBusy} aria-expanded={routeChooserOpen}
                onClick={() => setRouteChooserOpen(!routeChooserOpen)}>Reroute…</button>
              <button disabled={connectorBusy || !selectedProfile}
                onClick={() => void manageRoute(connector, selectedModel, "reload")}>Apply &amp; reload</button>
              <button disabled={connectorBusy}
                onClick={() => void manageRoute(connector, selectedModel, "disconnect")}>Disconnect route</button>
            </div>
            <small>Disconnect blocks new routed requests; it does not unload models or delete conversations.</small>
            {reloadConflict && reloadConflict.model === selectedModel && (
              <div role="alert">
                <p>{reloadConflict.active} active request(s). Force reload cancels them.</p>
                <button disabled={connectorBusy} onClick={() => setReloadConflict(null)}>Cancel reload</button>
                <button disabled={connectorBusy}
                  onClick={() => void manageRoute(connector, selectedModel, "reload", true)}>Force reload</button>
              </div>
            )}
          </section>
        )}

        {(routeChooserOpen || !selectedModel || (connector !== "hermes" && connector !== "opencode" && connector !== "openbot")) && <div className="model-menu-list" role="menu">
          {compatibleModels.length === 0 && (
            <p className="empty-models">
              {runningModels.length === 0
                ? "Start a model on any online host first."
                : `${clientName} needs a model served through its required API.`}
            </p>
          )}
          {compatibleModels.map((model) => {
            const qualified = `${model.hostId}/${model.modelId}`;
            const selected = selectedModel === qualified;
            return (
              <button
                aria-label={`${model.displayName} on ${model.hostName}${
                  selected ? `, ${details.launches ? "configured" : "routed"}` : ""
                }`}
                className={`model-option running ${selected ? "client-selected" : ""}`}
                disabled={connectorBusy}
                key={qualified}
                role="menuitem"
                onClick={() => connectClient(connector, model.hostId, model.modelId)}
              >
                <span>
                  <strong>{model.displayName}</strong>
                  <small className="model-meta">
                    <span>{model.hostName}</span>
                    <span>{model.runtime}</span>
                  </small>
                </span>
                {selected && (
                  <span className="model-state connected">
                    {details.launches ? "Launch" : "Routed"}
                  </span>
                )}
              </button>
            );
          })}
        </div>}
        </>}
        {connectorError && <p className="connector-error">{connectorError}</p>}
        {snapshotError && <p className="connector-error">Fleet status: {snapshotError}</p>}
      </main>
    );
  }

  if (!host) {
    return (
      <main className={shellClass} ref={shellRef}>
        {snapshotError && <p className="model-action-error">Fleet status: {snapshotError}</p>}
      </main>
    );
  }

  const online = host.connection !== "offline";
  const available = host.available !== false;
  const localHost = host.id === fleet?.local_host_id;
  const stopped = host.llama_swap.state === "stopped";

  return (
    <main className={shellClass} ref={shellRef}>
      <header className="model-menu-header">
        <span className={`indicator ${host.connection}`} aria-hidden="true" />
        <div>
          <strong>{host.display_name}</strong>
          <span>
            {online
              ? `${host.loaded_model_id ?? "Idle"}${
                  host.loaded_model_id &&
                  host.throughput_concurrency > 1 &&
                  host.aggregate_tokens_per_second !== null
                    ? ` · ${host.aggregate_tokens_per_second.toFixed(1)} tok/s total`
                    : host.loaded_model_id && host.tokens_per_second !== null
                      ? ` · ${host.tokens_per_second.toFixed(1)} tok/s`
                    : ""
                }`
              : "Offline"}
          </span>
        </div>
      </header>

      <div className="model-menu-list" role="menu">
        {host.models.length === 0 && <p className="empty-models">No models configured</p>}
        {host.models.map((model) => {
          const voice = model.kind === "voice" && model.capabilities.includes("realtime_audio");
          const { cachedLoaded, disabled, loaded, selectedInHermes } = getModelOptionState(
            online,
            host.id,
            model.id,
            host.loaded_model_id,
            fleet?.hermes.selected_model ?? null,
          );
          return (
            <button
              aria-label={`${model.display_name}${loaded ? ", running" : ""}${
                cachedLoaded ? ", last seen running while host was online" : ""
              }${
                selectedInHermes ? ", selected in Hermes" : ""
              }`}
              className={`model-option ${loaded ? "running" : ""} ${
                cachedLoaded ? "cached-loaded" : ""
              } ${
                selectedInHermes ? "hermes-selected" : ""
              }`}
              disabled={!available || (voice ? !online : disabled) || actionBusy || pendingAction !== null}
              key={model.id}
              role="menuitem"
              onClick={async () => {
                if (!voice) { await loadModel(host, model.id); return; }
                setActionBusy(true); setActionError(null);
                try { await invoke("open_voice_session", { hostId: host.id, modelId: model.id }); }
                catch (error) { setActionError(String(error)); }
                finally { setActionBusy(false); }
              }}
            >
              <span>
                <strong>{model.display_name}</strong>
                <small className="model-meta">
                  <span>{model.runtime}</span>
                  {voice && <span>Open voice conversation</span>}
                  {loaded && <span className="model-state running">Running</span>}
                  {cachedLoaded && <span className="model-state cached">Last seen</span>}
                  {selectedInHermes && <span className="model-state hermes">Hermes</span>}
                </small>
              </span>
              <span className="model-indicators" aria-hidden="true">
                {selectedInHermes && <span className="hermes-target">H</span>}
                {loaded && <span className="model-check">✓</span>}
              </span>
            </button>
          );
        })}
      </div>

      {pendingAction && (
        <section className="model-confirm" aria-label="Confirm model action">
          <p>
            {pendingAction.host.display_name} has {pendingAction.activeRequests} active request(s).
            This will cancel {pendingAction.activeRequests === 1 ? "it" : "them"}.
          </p>
          <div>
            <button disabled={actionBusy} onClick={() => setPendingAction(null)}>
              Cancel
            </button>
            <button className="danger" disabled={actionBusy} onClick={confirmPendingAction}>
              {pendingAction.kind === "load"
                ? "Force switch"
                : pendingAction.kind === "unload"
                  ? "Force unload"
                  : pendingAction.command === "stop_local_llama_swap"
                    ? "Force stop"
                    : "Force restart"}
            </button>
          </div>
        </section>
      )}

      <div className="model-menu-actions">
        <button
          disabled={!online || !host.loaded_model_id || actionBusy || pendingAction !== null}
          onClick={() => unloadModel(host)}
        >
          Unload model
        </button>
        {localHost && (
          <>
            <button
              disabled={actionBusy || pendingAction !== null}
              onClick={() => controlLocalService(host, "restart_local_llama_swap")}
            >
              {stopped ? "Start service" : "Restart service"}
            </button>
            <button
              disabled={stopped || actionBusy || pendingAction !== null}
              onClick={() => controlLocalService(host, "stop_local_llama_swap")}
            >
              Stop service
            </button>
          </>
        )}
      </div>
      {actionError && <p className="model-action-error">{actionError}</p>}
      {snapshotError && <p className="model-action-error">Fleet status: {snapshotError}</p>}
    </main>
  );
}

export default ModelMenu;
