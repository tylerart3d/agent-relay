import { canonicalClient, defaultPolicy, useRouting } from "./routing";
import { invoke } from "@tauri-apps/api/core";
import { readCliProject, saveCliProject } from "./cliProjects";
import { listen } from "@tauri-apps/api/event";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import "./App.css";
import { useAppSettings } from "./settings";
import type {
  ControlOutcome,
  FleetSnapshot,
  HostStatus,
  ModelProfile,
  ReasoningEffort,
} from "./fleet";
import { measureMenuHeight } from "./menuSizing";
import { useNotice } from "./notices";
import { getModelOptionState } from "./modelMenuState";
import { HARNESS_EFFORTS, effortBudget, effortLabel, formatBudget, resolveEffort } from "./inference";

// Backend client names for the CLI launchers.
const LAUNCH_CLIENT: Record<string, string> = { opencode_cli: "opencode", hermes_cli: "hermes" };

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


type PendingAction =
  | { kind: "load"; host: HostStatus; modelId: string; activeRequests: number }
  | {
      kind: "service";
      host: HostStatus;
      command: "restart_local_llama_swap";
      activeRequests: number;
    };

export function ModelMenu() {
  const autoRouting = useRouting();
  useAppSettings();
  const [fleet, setFleet] = useState<FleetSnapshot | null>(null);
  const [snapshotError, setSnapshotError] = useState<string | null>(null);
  const [hostId, setHostId] = useState<string | null>(null);
  const [connectorBusy, setConnectorBusy] = useState(false);
  const [connectorError, setConnectorError] = useState<string | null>(null);
  const [connectorProgress, setConnectorProgress] = useState<string | null>(null);
  const [project, setProject] = useState(() => readCliProject(""));
  // Launch menus collect a model, project, and thinking level before launching.
  // Null fields fall back to the harness's saved route and level.
  const [launchDraft, setLaunchDraft] = useState<{
    target: string | null;
    effort: ReasoningEffort | "" | null;
  }>({ target: null, effort: null });
  const [actionBusy, setActionBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [pendingAction, setPendingAction] = useState<PendingAction | null>(null);
  // Errors open beside the popup in their own window instead of reflowing it.
  useNotice("fleet-status", "Fleet status", snapshotError);
  useNotice("model.connector", "Route", connectorError);
  useNotice("model.action", "Model", actionError);
  const [menuPhase, setMenuPhase] = useState<"open" | "opening" | "closing">("open");
  const shellRef = useRef<HTMLElement>(null);
  const lastHeight = useRef(0);
  const animationTimer = useRef<number | null>(null);
  const measureFrame = useRef<number | null>(null);
  const menuRequest = useRef<number | null>(null);
  const snapshotRequest = useRef(0);
  const snapshotMounted = useRef(true);

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
      setConnectorError(null);
      setConnectorProgress(null);
      if (payload.host_id.startsWith("connector:")) {
        setProject(readCliProject(payload.host_id.slice("connector:".length)));
      }
      setLaunchDraft({ target: null, effort: null });
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
  // Runs one connector step; errors open in the notice window and leave the menu open.
  const runConnector = useCallback(
    async (step: () => Promise<void>) => {
      if (connectorBusy) return;
      const requestId = menuRequest.current;
      setConnectorBusy(true);
      setConnectorError(null);
      try {
        await step();
        if (requestId !== null && menuRequest.current === requestId) {
          await invoke("hide_tray_menus");
        }
      } catch (reason) {
        if (requestId !== null && menuRequest.current === requestId) {
          setConnectorError(reason instanceof Error ? reason.message : String(reason));
        }
        await readSnapshot();
      } finally {
        setConnectorProgress(null);
        setConnectorBusy(false);
      }
    },
    [connectorBusy, readSnapshot],
  );

  const setRouteMode = useCallback(
    async (client: ConnectorClient, mode: "auto" | "pinned") => {
      const routingClient = canonicalClient(client);
      const policy = autoRouting.routing.clients[routingClient] ?? defaultPolicy;
      if (policy.mode === mode) return;
      await autoRouting.save({
        ...autoRouting.routing,
        clients: { ...autoRouting.routing.clients, [routingClient]: { ...policy, mode } },
      });
    },
    [autoRouting],
  );

  const chooseAuto = useCallback(
    (client: ConnectorClient) => runConnector(() => setRouteMode(client, "auto")),
    [runConnector, setRouteMode],
  );

  // Loads a model that is not running; never cancels another host's work.
  const ensureLoaded = useCallback(
    async (target: HostStatus, model: ModelProfile) => {
      if (target.loaded_model_id === model.id) return;
      setConnectorProgress(`Loading ${model.display_name} on ${target.display_name}…`);
      const outcome = await invoke<ControlOutcome>("load_model", {
        hostId: target.id,
        modelId: model.id,
        force: false,
      });
      if (outcome.state === "conflict") {
        throw new Error(
          `${target.display_name} is finishing ${outcome.active_requests} request(s). ` +
            "Try again when it is idle, or choose another model.",
        );
      }
      await readSnapshot();
    },
    [readSnapshot],
  );

  // Routes the harness to one model, loading it first when it is not running.
  const chooseModel = useCallback(
    (client: ConnectorClient, target: HostStatus, model: ModelProfile) =>
      runConnector(async () => {
        await ensureLoaded(target, model);
        await setRouteMode(client, "pinned");
        await invoke(CONNECTOR_DETAILS[client].command, { hostId: target.id, modelId: model.id });
      }),
    [ensureLoaded, runConnector, setRouteMode],
  );

  // Applies the launch menu's choices in one routing save, then launches.
  const launchSelection = useCallback(
    (client: ConnectorClient, target: string, effort: ReasoningEffort | null) =>
      runConnector(async () => {
        const auto = target === "auto";
        let pinned: { host: HostStatus; model: ModelProfile } | null = null;
        if (!auto) {
          const host = fleet?.hosts.find((candidate) => target.startsWith(`${candidate.id}/`));
          const model = host?.models.find((candidate) => `${host.id}/${candidate.id}` === target);
          if (!host || !model) throw new Error(`${target} is no longer in the fleet catalog`);
          pinned = { host, model };
          await ensureLoaded(host, model);
        }
        const routingClient = canonicalClient(client);
        const policy = autoRouting.routing.clients[routingClient] ?? defaultPolicy;
        await autoRouting.save({
          ...autoRouting.routing,
          clients: {
            ...autoRouting.routing.clients,
            [routingClient]: { ...policy, mode: auto ? "auto" : "pinned", reasoning_effort: effort },
          },
        });
        setConnectorProgress(`Launching ${CONNECTOR_DETAILS[client].name}…`);
        const directory = readCliProject(client).selected;
        if (pinned) {
          await invoke(CONNECTOR_DETAILS[client].command, {
            ...(directory ? { directory } : {}),
            hostId: pinned.host.id,
            modelId: pinned.model.id,
          });
        } else {
          await invoke("launch_cli", { client: LAUNCH_CLIENT[client] ?? client, directory });
        }
      }),
    [autoRouting, ensureLoaded, fleet, runConnector],
  );

  const saveEffort = useCallback(
    async (client: ConnectorClient, effort: ReasoningEffort | "") => {
      const routingClient = canonicalClient(client);
      const policy = autoRouting.routing.clients[routingClient] ?? defaultPolicy;
      setConnectorBusy(true);
      try {
        await autoRouting.save({
          ...autoRouting.routing,
          clients: {
            ...autoRouting.routing.clients,
            [routingClient]: { ...policy, reasoning_effort: effort || null },
          },
        });
        setConnectorError(null);
      } catch (reason) {
        setConnectorError(String(reason));
      } finally {
        setConnectorBusy(false);
      }
    },
    [autoRouting],
  );

  const disconnectRoute = useCallback(
    async (client: "hermes" | "opencode" | "openbot") => {
      setConnectorBusy(true);
      try {
        await invoke("disconnect_client_route", { client });
        setConnectorError(null);
        await readSnapshot();
      } catch (reason) {
        setConnectorError(String(reason));
      } finally {
        setConnectorBusy(false);
      }
    },
    [readSnapshot],
  );

  const chooseProject = useCallback(async (client: ConnectorClient, path: string | null) => {
    try {
      setProject(saveCliProject(client, path));
      setConnectorError(null);
    } catch (reason) {
      setConnectorError(String(reason));
    }
  }, []);

  const pickProject = useCallback(async (client: ConnectorClient) => {
    setConnectorBusy(true);
    try {
      const path = await invoke<string | null>("pick_cli_project_folder");
      if (path) await chooseProject(client, path);
    } catch (reason) {
      setConnectorError(String(reason));
    } finally {
      setConnectorBusy(false);
    }
  }, [chooseProject]);

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
        // Unload means "free the memory now": it cancels any running request.
        await invoke<ControlOutcome>("unload_host", {
          hostId: host.id,
          force: true,
        });
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
    async (host: HostStatus, command: "restart_local_llama_swap") => {
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
    const savedModel = fleet?.[details.stateKey].selected_model ?? null;
    const launches = Boolean(details.launches);
    // Launch menus show the draft choice; route menus show the saved route.
    const draftTarget = launches ? launchDraft.target ?? (auto ? "auto" : savedModel) : null;
    const showAuto = launches ? draftTarget === "auto" : auto;
    const selectedModel = launches ? (draftTarget === "auto" ? null : draftTarget) : savedModel;
    const clientName = details.name;
    const routeClient =
      connector === "hermes" || connector === "opencode" || connector === "openbot"
        ? connector
        : null;
    const compatible = (model: ModelProfile) =>
      model.kind === "text" && (!details.capability || model.capabilities.includes(details.capability));
    const machines = (fleet?.hosts ?? [])
      .map((candidate) => ({ host: candidate, models: candidate.models.filter(compatible) }))
      .filter((entry) => entry.models.length > 0);
    // A saved route outlives its loaded process; read controls from the catalog.
    const selectedProfile = fleet?.hosts
      .flatMap((candidate) =>
        candidate.models.filter((model) => `${candidate.id}/${model.id}` === selectedModel),
      )
      .find(compatible);
    const thinking = selectedProfile?.inference_controls.thinking ?? null;
    // Saved per harness; each model maps it to its nearest level.
    const savedEffort = policy.reasoning_effort ?? null;
    const harnessEffort =
      launches && launchDraft.effort !== null ? launchDraft.effort || null : savedEffort;
    const requested = harnessEffort ?? thinking?.default_effort ?? null;
    const effort = thinking && requested ? resolveEffort(thinking, requested) : requested;
    const temperature = selectedProfile?.inference_controls.temperature?.default;
    const autoStatus = autoRouting.waiting[routingClient]
      ? `Waiting for a free model · ${autoRouting.waiting[routingClient]} queued`
      : autoRouting.choices[routingClient]
        ? `Last assigned: ${autoRouting.choices[routingClient]}`
        : "Model chosen when the next request arrives";
    const verb = launches ? "Launch" : routeClient ? "Route" : "Connect";
    const draftHost = selectedModel
      ? fleet?.hosts.find((candidate) => selectedModel.startsWith(`${candidate.id}/`)) ?? null
      : null;
    const canLaunch =
      launches &&
      autoRouting.loaded &&
      (draftTarget === "auto" ||
        (Boolean(selectedProfile) &&
          draftHost !== null &&
          draftHost.connection !== "offline" &&
          draftHost.available !== false));
    return (
      <main className={shellClass} ref={shellRef}>
        <header className="model-menu-header connector-header">
          <span className="connector-mark" aria-hidden="true">
            {details.mark}
          </span>
          <div>
            <strong>{verb} {clientName}</strong>
            <span>
              {details.subtitle ??
                (launches
                  ? "Choose a model, project, and thinking level, then launch"
                  : "Pick Auto or a model; a model that is not running loads first")}
            </span>
          </div>
        </header>

        <div className="model-menu-list" role="menu" aria-busy={connectorBusy}>
          {launches && <h3 className="connector-step">Model</h3>}
          <button
            aria-label={`Auto${showAuto ? ", selected" : ""}`}
            className={`model-option auto-option ${showAuto ? "client-selected" : ""}`}
            disabled={connectorBusy || !autoRouting.loaded}
            role="menuitem"
            onClick={() =>
              launches
                ? setLaunchDraft((draft) => ({ ...draft, target: "auto" }))
                : void chooseAuto(connector)}
          >
            <span>
              <strong>Auto</strong>
              <small className="model-meta">
                <span>{auto ? autoStatus : "Approved models from Settings → Auto routing"}</span>
              </small>
            </span>
            {showAuto && <span className="model-state connected">{launches ? "Selected" : "On"}</span>}
          </button>

          {machines.length === 0 && (
            <p className="empty-models">{clientName} needs a model served through its required API.</p>
          )}
          {machines.map(({ host: machine, models }) => {
            const online = machine.connection !== "offline";
            const usable = online && machine.available !== false;
            return (
              <section className="connector-machine" key={machine.id} aria-label={machine.display_name}>
                <h3>
                  {machine.display_name}
                  <small>{!online ? "Offline" : machine.available === false ? "Off" : ""}</small>
                </h3>
                {models.map((model) => {
                  const qualified = `${machine.id}/${model.id}`;
                  const running = online && machine.loaded_model_id === model.id;
                  const selected = !showAuto && selectedModel === qualified;
                  return (
                    <button
                      aria-label={`${model.display_name} on ${machine.display_name}${
                        running ? ", running" : ""
                      }${selected ? `, ${launches ? "selected" : "routed"}` : ""}`}
                      className={`model-option ${running ? "running" : ""} ${selected ? "client-selected" : ""}`}
                      disabled={connectorBusy || !usable}
                      key={qualified}
                      role="menuitem"
                      onClick={() =>
                        launches
                          ? setLaunchDraft((draft) => ({ ...draft, target: qualified }))
                          : void chooseModel(connector, machine, model)}
                    >
                      <span>
                        <strong>{model.display_name}</strong>
                        <small className="model-meta">
                          <span>{model.runtime}</span>
                          {model.context_length ? <span>{Math.round(model.context_length / 1024)}K</span> : null}
                          {running ? <span className="model-state running">Running</span> : <span>Loads first</span>}
                        </small>
                      </span>
                      {selected && (
                        <span className="model-state connected">
                          {launches ? "Selected" : "Routed"}
                        </span>
                      )}
                    </button>
                  );
                })}
              </section>
            );
          })}
        </div>

        {launches && (
          <section className="connector-project" aria-label={`${clientName} project folder`}>
            <h3 className="connector-step">Project</h3>
            <div>
              <select
                aria-label={`Project folder for ${clientName}`}
                disabled={connectorBusy}
                title={project.selected ?? "Home folder"}
                value={project.selected ?? ""}
                onChange={(event) => void chooseProject(connector, event.target.value || null)}
              >
                <option value="">Home folder</option>
                {project.recent.map((path) => <option key={path} value={path}>{path}</option>)}
              </select>
              <button
                aria-label={`Choose project folder for ${clientName}`}
                disabled={connectorBusy}
                onClick={() => void pickProject(connector)}
              >
                Folder…
              </button>
            </div>
          </section>
        )}

        <section className="connector-inference" aria-label={`${clientName} thinking`}>
          {launches && <h3 className="connector-step">Thinking</h3>}
          <label>
            <span>Thinking</span>
            <select
              aria-label="Thinking effort"
              disabled={connectorBusy || !autoRouting.loaded}
              value={harnessEffort ?? ""}
              onChange={(event) => {
                const value = event.currentTarget.value as ReasoningEffort | "";
                if (launches) setLaunchDraft((draft) => ({ ...draft, effort: value }));
                else void saveEffort(connector, value);
              }}
            >
              <option value="">Model default</option>
              {HARNESS_EFFORTS.map((level) => (
                <option key={level} value={level}>
                  {effortLabel(level)}
                </option>
              ))}
            </select>
          </label>
          <small>
            {!showAuto && selectedProfile
              ? [
                  thinking && effort
                    ? `${selectedProfile.display_name}: ${effortLabel(effort)}`
                    : `${selectedProfile.display_name} has no thinking control`,
                  thinking && effort && effort !== "off"
                    ? formatBudget(thinking, effortBudget(thinking, effort))
                    : null,
                  temperature !== undefined && temperature !== null ? `temperature ${temperature}` : null,
                ].filter(Boolean).join(" · ")
              : "Each model uses its nearest level"}
            . An effort chosen in {clientName} wins.
          </small>
        </section>

        {connectorProgress && <p className="connector-progress" role="status">{connectorProgress}</p>}

        {launches && (
          <div className="connector-launch">
            <button
              disabled={connectorBusy || !canLaunch}
              onClick={() => {
                if (draftTarget) void launchSelection(connector, draftTarget, harnessEffort);
              }}
            >
              Launch {clientName}
            </button>
          </div>
        )}

        {routeClient && !auto && selectedModel && (
          <div className="connector-inference-actions">
            <button disabled={connectorBusy} onClick={() => void disconnectRoute(routeClient)}>
              Disconnect route
            </button>
          </div>
        )}
      </main>
    );
  }

  if (!host) {
    return (
      <main className={shellClass} ref={shellRef}>
        {snapshotError && <p className="tray-loading">Fleet status unavailable</p>}
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
              {pendingAction.kind === "load" ? "Force switch" : "Force restart"}
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
          </>
        )}
      </div>
    </main>
  );
}

export default ModelMenu;
