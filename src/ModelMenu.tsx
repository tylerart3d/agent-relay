import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import "./App.css";
import {
  channelConversationKey,
  channelConversationLabel,
  channelHarnessLabel,
  type ChannelCommandResult,
  type ChannelHarness,
  type ChannelRoute,
  type OpenCodeSessionInfo,
} from "./channels";
import { type AppSettings, useAppSettings } from "./settings";
import type {
  ControlOutcome,
  FleetSnapshot,
  HostStatus,
  InferenceOverrides,
  ReasoningEffort,
} from "./fleet";
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
  | "vscode";

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
      | "vscode";
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

function quoteChannelArgument(value: string) {
  return `"${value.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
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
  const { settings, setSettings, refreshSettings } = useAppSettings();
  const [fleet, setFleet] = useState<FleetSnapshot | null>(null);
  const [channelRoutes, setChannelRoutes] = useState<ChannelRoute[]>([]);
  const [selectedConversation, setSelectedConversation] = useState<string | null>(null);
  const [channelHarness, setChannelHarness] = useState<ChannelHarness>("hermes");
  const [channelHarnessHost, setChannelHarnessHost] = useState("");
  const [channelModelTarget, setChannelModelTarget] = useState("");
  const [channelProject, setChannelProject] = useState("");
  const [channelNativeSession, setChannelNativeSession] = useState("");
  const [openCodeSessions, setOpenCodeSessions] = useState<OpenCodeSessionInfo[]>([]);
  const [openCodeSessionsBusy, setOpenCodeSessionsBusy] = useState(false);
  const [channelBusy, setChannelBusy] = useState(false);
  const [channelError, setChannelError] = useState<string | null>(null);
  const [snapshotError, setSnapshotError] = useState<string | null>(null);
  const [hostId, setHostId] = useState<string | null>(null);
  const [connectorBusy, setConnectorBusy] = useState(false);
  const [connectorError, setConnectorError] = useState<string | null>(null);
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

  const readChannelRoutes = useCallback(async () => {
    try {
      const response = await invoke<ChannelRoute[]>("get_channel_routes");
      const routes = Array.isArray(response) ? response : [];
      if (snapshotMounted.current) setChannelRoutes(routes);
      return routes;
    } catch (reason) {
      if (snapshotMounted.current) setChannelError(String(reason));
      return [];
    }
  }, []);

  const readOpenCodeSessions = useCallback(async (host: string) => {
    setOpenCodeSessionsBusy(true);
    try {
      const response = await invoke<OpenCodeSessionInfo[]>("get_opencode_sessions", {
        hostId: host,
      });
      const sessions = Array.isArray(response) ? response : [];
      if (snapshotMounted.current) {
        setOpenCodeSessions(sessions.filter((session) => !session.archived));
        setChannelError(null);
      }
    } catch (reason) {
      if (snapshotMounted.current) {
        setOpenCodeSessions([]);
        setChannelError(String(reason));
      }
    } finally {
      if (snapshotMounted.current) setOpenCodeSessionsBusy(false);
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
      setSelectedConversation(null);
      setChannelNativeSession("");
      setOpenCodeSessions([]);
      setChannelError(null);
      lastHeight.current = 0;
      setContextDrafts({});
      setInferenceDrafts({});
      setConnectorError(null);
      setActionError(null);
      setPendingAction(null);
      void readSnapshot();
      startPolling();
      if (payload.host_id === "channels") void readChannelRoutes();
      measureFrame.current = window.requestAnimationFrame(() => {
        if (menuRequest.current !== payload.request_id) return;
        const shell = shellRef.current;
        if (!shell) return;
        const height = Math.ceil(shell.scrollHeight);
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
  }, [readChannelRoutes, readSnapshot, resizeMenu]);

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
      const height = Math.ceil(shell.scrollHeight);
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

  const activeChannelRoutes = useMemo(
    () =>
      channelRoutes
        .filter((route) => route.archived_at_ms === undefined)
        .sort((left, right) => right.updated_at_ms - left.updated_at_ms),
    [channelRoutes],
  );
  const selectedChannelRoute = useMemo(
    () =>
      activeChannelRoutes.find(
        (route) => channelConversationKey(route) === selectedConversation,
      ),
    [activeChannelRoutes, selectedConversation],
  );
  const selectedConversationSessions = useMemo(
    () =>
      channelRoutes
        .filter(
          (route) => channelConversationKey(route) === selectedConversation,
        )
        .sort((left, right) => right.updated_at_ms - left.updated_at_ms),
    [channelRoutes, selectedConversation],
  );
  const channelModels = useMemo(
    () =>
      fleet?.hosts.flatMap((candidate) =>
        candidate.connection === "offline"
          ? []
          : candidate.models
              .filter(
                (model) =>
                  model.kind === "text" &&
                  model.capabilities.some((capability) =>
                    ["chat", "completions", "responses", "anthropic_messages"].includes(
                      capability,
                    ),
                  ),
              )
              .map((model) => ({
                value: `${candidate.id}/${model.id}`,
                label: `${candidate.display_name} · ${model.display_name} · ${
                  candidate.loaded_model_id === model.id ? "Running" : "Idle"
                }`,
              })),
      ) ?? [],
    [fleet],
  );
  const channelHarnessHosts = useMemo(
    () => fleet?.hosts.filter((candidate) => candidate.connection !== "offline") ?? [],
    [fleet],
  );
  const localGatewayName =
    fleet?.hosts.find((candidate) => candidate.id === fleet.local_host_id)?.display_name ??
    "This gateway";
  const hostName = useCallback(
    (id: string | null | undefined) =>
      fleet?.hosts.find((candidate) => candidate.id === id)?.display_name ?? id ?? localGatewayName,
    [fleet, localGatewayName],
  );

  const selectChannelConversation = useCallback((route: ChannelRoute) => {
    setSelectedConversation(channelConversationKey(route));
    setChannelHarness(route.harness);
    setChannelHarnessHost(route.harness_host_id ?? "");
    setChannelModelTarget(`${route.host_id}/${route.model_id}`);
    setChannelProject(route.project ?? "");
    setChannelNativeSession(route.native_session_id ?? "");
    setChannelError(null);
  }, []);

  const openCodeSessionHost =
    channelHarnessHost || fleet?.local_host_id || "";
  const openCodeProjects = useMemo(() => {
    const projects = new Map<string, string>();
    for (const session of openCodeSessions) {
      if (!projects.has(session.directory)) {
        projects.set(session.directory, session.project_name || session.directory);
      }
    }
    return [...projects.entries()].map(([directory, label]) => ({ directory, label }));
  }, [openCodeSessions]);
  const projectOpenCodeSessions = useMemo(
    () => openCodeSessions.filter((session) => session.directory === channelProject),
    [channelProject, openCodeSessions],
  );
  const selectedModelAvailable = channelModels.some(
    (model) => model.value === channelModelTarget,
  );
  const currentRouteModelHost = selectedChannelRoute
    ? fleet?.hosts.find((host) => host.id === selectedChannelRoute.host_id)
    : undefined;
  const currentRouteModel = currentRouteModelHost?.models.find(
    (model) => model.id === selectedChannelRoute?.model_id,
  );
  const currentRouteState = !currentRouteModelHost || currentRouteModelHost.connection === "offline"
    ? "Offline"
    : currentRouteModelHost.loaded_model_id === selectedChannelRoute?.model_id
      ? "Running"
      : currentRouteModel
        ? "Idle · reloads on next message"
        : "Unavailable";

  useEffect(() => {
    if (!selectedChannelRoute || channelHarness !== "open_code" || !openCodeSessionHost) {
      setOpenCodeSessions([]);
      return;
    }
    void readOpenCodeSessions(openCodeSessionHost);
  }, [
    channelHarness,
    openCodeSessionHost,
    readOpenCodeSessions,
    selectedChannelRoute,
  ]);

  const submitChannelCommand = useCallback(
    async (route: ChannelRoute, text: string) => {
      const execute = (command: string) =>
        invoke<ChannelCommandResult>("execute_channel_command", {
          channel: route.channel,
          accountId: route.account_id,
          conversationId: route.conversation_id,
          conversationLabel: route.conversation_label ?? null,
          text: command,
        });
      setChannelBusy(true);
      try {
        let result = await execute(text);
        if (result.confirmation_required && result.retry_command) {
          const accepted = window.confirm(
            `${result.message ?? "The destination is busy."}\n\nCancel the active request and continue?`,
          );
          if (!accepted) return;
          result = await execute(result.retry_command);
        }
        if (!result.ok) {
          throw new Error(result.error ?? result.message ?? "Channel route was not changed");
        }
        const routes = await readChannelRoutes();
        const active = routes.find(
          (candidate) =>
            candidate.archived_at_ms === undefined &&
            channelConversationKey(candidate) === channelConversationKey(route),
        );
        if (active) selectChannelConversation(active);
        setChannelError(null);
        await readSnapshot();
      } catch (reason) {
        setChannelError(String(reason));
      } finally {
        setChannelBusy(false);
      }
    },
    [readChannelRoutes, readSnapshot, selectChannelConversation],
  );

  const applyChannelRoute = useCallback(
    (action: "use" | "move" | "new") => {
      if (!selectedChannelRoute || !channelModelTarget) return;
      const harnessName = channelHarness === "open_code" ? "opencode" : channelHarness;
      const harnessTarget =
        channelHarness === "direct" || !channelHarnessHost
          ? harnessName
          : `${harnessName}@${channelHarnessHost}`;
      const project =
        (channelHarness === "open_code" || channelHarness === "pi") && channelProject.trim()
          ? ` project ${quoteChannelArgument(channelProject.trim())}`
          : "";
      const nativeSession =
        channelHarness === "open_code" && channelNativeSession
          ? ` session ${quoteChannelArgument(channelNativeSession)}`
          : "";
      void submitChannelCommand(
        selectedChannelRoute,
        `!ar ${action} ${harnessTarget} ${channelModelTarget}${project}${nativeSession}`,
      );
    },
    [
      channelHarness,
      channelHarnessHost,
      channelModelTarget,
      channelNativeSession,
      channelProject,
      selectedChannelRoute,
      submitChannelCommand,
    ],
  );

  if (hostId === "channels") {
    if (!selectedChannelRoute) {
      return (
        <main className={shellClass} ref={shellRef}>
          <header className="model-menu-header connector-header">
            <span className="connector-mark" aria-hidden="true">↔</span>
            <div>
              <strong>Message routes</strong>
              <span>Select a conversation to change its active path</span>
            </div>
          </header>
          <div className="model-menu-list" role="menu">
            {activeChannelRoutes.length === 0 && (
              <p className="empty-models">No messaging conversations yet.</p>
            )}
            {activeChannelRoutes.map((route) => (
              <button
                className="model-option channel-route-option"
                key={channelConversationKey(route)}
                onClick={() => selectChannelConversation(route)}
                role="menuitem"
              >
                <span>
                  <strong>{channelConversationLabel(route)}</strong>
                  <small className="model-meta">
                    <span>{route.channel}</span>
                    <span>{channelHarnessLabel(route.harness)}</span>
                    <span>{route.host_id}/{route.model_id}</span>
                    {route.handoff_status === "pending" && <span>Transfer pending</span>}
                    {route.native_archive_status === "pending" && <span>Source archive pending</span>}
                    {route.native_archive_status === "failed" && <span>Source archive retrying</span>}
                  </small>
                </span>
                <span className="chevron" aria-hidden="true">›</span>
              </button>
            ))}
            {channelError && <p className="connector-error">{channelError}</p>}
          </div>
        </main>
      );
    }

    return (
      <main className={shellClass} ref={shellRef}>
        <header className="model-menu-header channel-route-header">
          <button aria-label="Back to conversations" onClick={() => setSelectedConversation(null)}>‹</button>
          <div>
            <strong>{channelConversationLabel(selectedChannelRoute)}</strong>
            <span>
              Session #{selectedChannelRoute.session_id} · {selectedChannelRoute.channel}
              {selectedChannelRoute.handoff_status === "pending" && " · Context transfer pending"}
              {selectedChannelRoute.handoff_status === "completed" && " · Context transferred"}
              {selectedChannelRoute.native_archive_status === "pending" && " · Source archive pending"}
              {selectedChannelRoute.native_archive_status === "completed" && " · Source archived"}
              {selectedChannelRoute.native_archive_status === "failed" && " · Source archive retrying"}
            </span>
          </div>
        </header>
        <section className="channel-route-current" aria-label="Current message route">
          <span className="channel-route-kicker">Current route</span>
          <div className="channel-route-path">
            <strong>{selectedChannelRoute.channel}</strong>
            <span aria-hidden="true">→</span>
            <strong>
              {channelHarnessLabel(selectedChannelRoute.harness)}
              {selectedChannelRoute.harness !== "direct" && (
                <small>{hostName(selectedChannelRoute.harness_host_id)}</small>
              )}
            </strong>
            <span aria-hidden="true">→</span>
            <strong>
              {currentRouteModel?.display_name ?? selectedChannelRoute.model_id}
              <small>{hostName(selectedChannelRoute.host_id)}</small>
            </strong>
          </div>
          <span className={`channel-route-health ${currentRouteState.startsWith("Running") ? "running" : ""}`}>
            {currentRouteState}
          </span>
          {selectedChannelRoute.native_archive_status === "failed" && (
            <small className="connector-error">
              {selectedChannelRoute.native_archive_error ?? "The prior native conversation could not be archived yet. Agent Relay will retry after the next reply."}
            </small>
          )}
        </section>
        <section className="channel-route-editor" aria-label="Message route editor">
          <label>
            <span>Harness</span>
            <select
              disabled={channelBusy}
              value={channelHarness}
              onChange={(event) => setChannelHarness(event.currentTarget.value as ChannelHarness)}
            >
              <option value="direct">Direct model</option>
              <option value="hermes">Hermes</option>
              <option value="open_code">OpenCode</option>
              <option value="pi">Pi</option>
            </select>
          </label>
          <label>
            <span>Harness machine</span>
            <select
              disabled={channelBusy || channelHarness === "direct"}
              value={channelHarnessHost}
              onChange={(event) => setChannelHarnessHost(event.currentTarget.value)}
            >
              <option value="">{localGatewayName} (gateway)</option>
              {channelHarnessHosts.map((host) => (
                <option key={host.id} value={host.id}>{host.display_name}</option>
              ))}
            </select>
          </label>
          {channelHarness === "open_code" && (
            <>
              <label>
                <span>OpenCode project</span>
                <select
                  aria-label="OpenCode project"
                  disabled={channelBusy || openCodeSessionsBusy}
                  value={openCodeProjects.some((project) => project.directory === channelProject)
                    ? channelProject
                    : ""}
                  onChange={(event) => {
                    setChannelProject(event.currentTarget.value);
                    setChannelNativeSession("");
                  }}
                >
                  <option value="">
                    {openCodeSessionsBusy
                      ? "Reading OpenCode projects…"
                      : "Choose an existing project"}
                  </option>
                  {openCodeProjects.map((project) => (
                    <option key={project.directory} value={project.directory}>
                      {project.label}
                    </option>
                  ))}
                </select>
              </label>
              <label>
                <span>OpenCode conversation</span>
                <select
                  aria-label="OpenCode conversation"
                  disabled={channelBusy || openCodeSessionsBusy || !channelProject}
                  value={channelNativeSession}
                  onChange={(event) => setChannelNativeSession(event.currentTarget.value)}
                >
                  <option value="">Start a new OpenCode conversation</option>
                  {projectOpenCodeSessions.map((session) => (
                    <option key={session.id} value={session.id}>{session.title}</option>
                  ))}
                </select>
              </label>
            </>
          )}
          {(channelHarness === "open_code" || channelHarness === "pi") && (
            <label>
              <span>Project</span>
              <input
                disabled={channelBusy}
                placeholder="Optional project name or path"
                value={channelProject}
                onChange={(event) => setChannelProject(event.currentTarget.value)}
              />
            </label>
          )}
          <label>
            <span>Model</span>
            <select
              disabled={channelBusy}
              value={channelModelTarget}
              onChange={(event) => setChannelModelTarget(event.currentTarget.value)}
            >
              {!selectedModelAvailable && channelModelTarget && (
                <option value={channelModelTarget} disabled>
                  {channelModelTarget} · Unavailable
                </option>
              )}
              {channelModels.map((model) => (
                <option key={model.value} value={model.value}>{model.label}</option>
              ))}
            </select>
          </label>
          <div className="channel-route-preview" aria-label="Proposed message route">
            <span>Destination</span>
            <strong>
              {channelHarnessLabel(channelHarness)}
              {channelHarness !== "direct" && ` on ${hostName(channelHarnessHost || fleet?.local_host_id)}`}
              {channelModelTarget && ` → ${channelModelTarget}`}
            </strong>
          </div>
          <div className="channel-route-actions">
            <button disabled={channelBusy || !channelModelTarget} onClick={() => applyChannelRoute("use")}>
              Update current
            </button>
            <button disabled={channelBusy || !channelModelTarget} onClick={() => applyChannelRoute("move")}>
              Move with context
            </button>
            <button disabled={channelBusy || !channelModelTarget} onClick={() => applyChannelRoute("new")}>
              Start without context
            </button>
          </div>
        </section>
        {selectedConversationSessions.some((route) => route.archived_at_ms !== undefined) && (
          <section className="channel-session-history" aria-label="Archived sessions">
            <strong>Archived sessions</strong>
            {selectedConversationSessions
              .filter((route) => route.archived_at_ms !== undefined)
              .map((route) => (
                <button
                  disabled={channelBusy}
                  key={route.session_id}
                  onClick={() => submitChannelCommand(selectedChannelRoute, `!ar resume ${route.session_id}`)}
                >
                  <span>Session #{route.session_id}</span>
                  <small>{channelHarnessLabel(route.harness)} · {route.host_id}/{route.model_id}</small>
                </button>
              ))}
          </section>
        )}
        {channelError && <p className="connector-error">{channelError}</p>}
      </main>
    );
  }

  if (connector) {
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
    const selectedInferenceControls = selectedRunningModel?.inferenceControls;
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
      : connector === "hermes" || connector === "opencode"
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
          <small>Client history limit; the serving profile must support the same window.</small>
          </section>
        )}

        {selectedModel && selectedRunningModel && (selectedThinking || selectedTemperature) && (
          <section className="connector-inference" aria-label={`${selectedRunningModel.displayName} inference controls`}>
            <div className="connector-inference-title">
              <strong>Model controls</strong>
              <small>{selectedRunningModel.displayName}</small>
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
                    <small>-1 is unlimited; 0 ends thinking immediately.</small>
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
          </section>
        )}

        <div className="model-menu-list" role="menu">
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
          {connectorError && <p className="connector-error">{connectorError}</p>}
          {snapshotError && <p className="connector-error">Fleet status: {snapshotError}</p>}
        </div>
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
              disabled={disabled || actionBusy || pendingAction !== null}
              key={model.id}
              role="menuitem"
              onClick={() => loadModel(host, model.id)}
            >
              <span>
                <strong>{model.display_name}</strong>
                <small className="model-meta">
                  <span>{model.runtime}</span>
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
