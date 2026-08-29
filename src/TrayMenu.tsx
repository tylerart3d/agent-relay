import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import "./App.css";
import {
  channelHarnessLabel,
  type ChannelAdapterStatus,
  type ChannelRoute,
} from "./channels";
import type { ControlOutcome, FleetSnapshot, HostStatus } from "./fleet";
import { layoutOffsetTop } from "./modelMenuState";
import {
  applyTheme,
  type AppSettings,
  type HarnessId,
  type HarnessSetupStatus,
  type ThemePreference,
  useAppSettings,
} from "./settings";

function hostSummary(host: HostStatus) {
  if (host.connection === "offline") return "Offline";
  if (host.loaded_model_id) {
    const throughput =
      host.throughput_concurrency > 1 && host.aggregate_tokens_per_second !== null
        ? `${host.aggregate_tokens_per_second.toFixed(1)} tok/s total`
        : host.tokens_per_second === null
          ? ""
          : `${host.tokens_per_second.toFixed(1)} tok/s`;
    return `${host.loaded_model_id}${
      throughput ? ` · ${throughput}` : ""
    }`;
  }
  return "Idle";
}

function connectorSummary(
  selectedModel: string | null | undefined,
  selectedAvailable: boolean,
  runningCount: number,
) {
  if (selectedModel) {
    return selectedAvailable ? selectedModel : `${selectedModel} · unavailable`;
  }
  return runningCount > 0 ? "Choose route" : "No running models";
}

type CliClient = "opencode_cli" | "hermes_cli" | "codex" | "claude_code" | "pi" | "copilot";

const HARNESS_LABELS: Array<{ id: HarnessId; label: string }> = [
  { id: "opencode", label: "OpenCode" },
  { id: "opencode_cli", label: "OpenCode CLI" },
  { id: "codex", label: "Codex" },
  { id: "claude_code", label: "Claude Code" },
  { id: "copilot", label: "Copilot" },
  { id: "vscode", label: "VS Code" },
  { id: "pi", label: "Pi" },
  { id: "hermes", label: "Hermes" },
  { id: "hermes_cli", label: "Hermes CLI" },
];

function CliLauncher({
  client,
  error,
  label,
  runningCount,
  selectedAvailable,
  selectedModel,
  state,
  onChoose,
  onLaunch,
}: {
  client: CliClient;
  error?: string | null;
  label: string;
  runningCount: number;
  selectedAvailable: boolean;
  selectedModel?: string | null;
  state: string;
  onChoose: (client: CliClient, anchor: HTMLElement) => void;
  onLaunch: (
    client: CliClient,
    selectedModel: string | null | undefined,
    selectedAvailable: boolean,
    anchor: HTMLElement,
  ) => void;
}) {
  return (
    <div
      className={`tray-cli-integration ${state} ${
        selectedModel && !selectedAvailable ? "unavailable" : ""
      }`}
      title={error ?? undefined}
    >
      <button
        aria-label={`Launch ${label}`}
        className="tray-integration tray-cli-launch"
        onClick={(event) =>
          onLaunch(client, selectedModel, selectedAvailable, event.currentTarget)
        }
      >
        <span>{label}</span>
        <small>{connectorSummary(selectedModel, selectedAvailable, runningCount)}</small>
        <span className="launch-glyph" aria-hidden="true">▶</span>
      </button>
      <button
        aria-label={`Choose model for ${label}`}
        className="tray-cli-chooser"
        onClick={(event) => onChoose(client, event.currentTarget)}
      >
        ›
      </button>
    </div>
  );
}

export function TrayMenu() {
  const { settings, setSettings } = useAppSettings();
  const [fleet, setFleet] = useState<FleetSnapshot | null>(null);
  const [channelRoutes, setChannelRoutes] = useState<ChannelRoute[]>([]);
  const [channelAdapters, setChannelAdapters] = useState<ChannelAdapterStatus[]>([]);
  const [snapshotError, setSnapshotError] = useState<string | null>(null);
  const [selectedHost, setSelectedHost] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [settingsBusy, setSettingsBusy] = useState(false);
  const [settingsError, setSettingsError] = useState<string | null>(null);
  const [harnessSetupOpen, setHarnessSetupOpen] = useState(false);
  const [harnessSetupHost, setHarnessSetupHost] = useState<string | null>(null);
  const [harnessSetupStatuses, setHarnessSetupStatuses] = useState<HarnessSetupStatus[]>([]);
  const [harnessSetupBusy, setHarnessSetupBusy] = useState<string | null>(null);
  const [harnessSetupError, setHarnessSetupError] = useState<string | null>(null);
  const [photonSetupOpen, setPhotonSetupOpen] = useState(false);
  const [photonProjectId, setPhotonProjectId] = useState("");
  const [photonProjectSecret, setPhotonProjectSecret] = useState("");
  const [photonAllowedSenders, setPhotonAllowedSenders] = useState("");
  const [launchError, setLaunchError] = useState<string | null>(null);
  const [menuPhase, setMenuPhase] = useState<"open" | "opening" | "closing">("open");
  const [menuOrigin, setMenuOrigin] = useState<"top-right" | "bottom-right">(
    "bottom-right",
  );
  const shellRef = useRef<HTMLElement>(null);
  const lastHeight = useRef(0);
  const configChangeHandling = useRef(false);
  const animationTimer = useRef<number | null>(null);
  const snapshotRequest = useRef(0);
  const snapshotMounted = useRef(true);
  const submenuRequest = useRef(Date.now());
  const menuEpoch = useRef<number | null>(null);

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
    } catch {
      if (snapshotMounted.current) setChannelRoutes([]);
      return [];
    }
  }, []);

  const readChannelAdapters = useCallback(async () => {
    try {
      const response = await invoke<ChannelAdapterStatus[]>("get_channel_adapters");
      const adapters = Array.isArray(response) ? response : [];
      if (snapshotMounted.current) setChannelAdapters(adapters);
      return adapters;
    } catch {
      if (snapshotMounted.current) setChannelAdapters([]);
      return [];
    }
  }, []);

  const readHarnessSetupStatuses = useCallback(async (hostId: string) => {
    setHarnessSetupBusy("refresh");
    try {
      const statuses = await invoke<HarnessSetupStatus[]>("get_harness_setup_statuses", {
        hostId,
      });
      setHarnessSetupStatuses(statuses);
      setHarnessSetupError(null);
    } catch (reason) {
      setHarnessSetupStatuses([]);
      setHarnessSetupError(String(reason));
    } finally {
      setHarnessSetupBusy(null);
    }
  }, []);

  const configureHarness = useCallback(
    async (harness: HarnessId) => {
      if (!harnessSetupHost || harnessSetupBusy) return;
      setHarnessSetupBusy(harness);
      try {
        await invoke<HarnessSetupStatus>("configure_fleet_harness", {
          hostId: harnessSetupHost,
          harness,
        });
        await readHarnessSetupStatuses(harnessSetupHost);
        await readSnapshot();
        setHarnessSetupError(null);
      } catch (reason) {
        setHarnessSetupError(String(reason));
      } finally {
        setHarnessSetupBusy(null);
      }
    },
    [harnessSetupBusy, harnessSetupHost, readHarnessSetupStatuses, readSnapshot],
  );

  useEffect(() => {
    if (!fleet || harnessSetupHost) return;
    setHarnessSetupHost(fleet.local_host_id);
  }, [fleet, harnessSetupHost]);

  useEffect(() => {
    if (settingsOpen && harnessSetupOpen && harnessSetupHost) {
      void readHarnessSetupStatuses(harnessSetupHost);
    }
  }, [harnessSetupHost, harnessSetupOpen, readHarnessSetupStatuses, settingsOpen]);

  useEffect(() => {
    if (!settings) return;
    setPhotonProjectId(settings.channel_gateway.photon_project_id ?? "");
    setPhotonAllowedSenders(settings.channel_gateway.allowed_senders.join(", "));
  }, [settings]);

  const dispatch = useCallback(async (action: string) => {
    await emit("tray-action", action);
    await invoke("hide_tray_menus");
  }, []);

  const openStatus = useCallback(async () => {
    await invoke("hide_tray_menus");
    await invoke("show_status_window");
  }, []);

  const quit = useCallback(async () => {
    try {
      const outcome = await invoke<ControlOutcome | null>("quit_app", { force: false });
      if (outcome?.state !== "conflict") return;
      const accepted = window.confirm(
        `Agent Relay has ${outcome.active_requests} active request(s). ` +
          "Cancel them, stop the local service, and quit?",
      );
      if (!accepted) return;
      try {
        await invoke("quit_app", { force: true });
      } catch {
        // A successful forced exit can close the IPC channel before it replies.
      }
    } catch (reason) {
      setLaunchError(`Unable to quit Agent Relay: ${String(reason)}`);
    }
  }, []);

  const selectTheme = useCallback(
    async (theme: ThemePreference) => {
      if (!settings || settingsBusy || settings.theme === theme) return;
      applyTheme(theme);
      setSettings((current) => (current ? { ...current, theme } : current));
      setSettingsBusy(true);
      try {
        setSettings(await invoke<AppSettings>("set_theme", { theme }));
        setSettingsError(null);
      } catch (reason) {
        setSettingsError(String(reason));
        applyTheme(settings.theme);
        setSettings(settings);
      } finally {
        setSettingsBusy(false);
      }
    },
    [setSettings, settings, settingsBusy],
  );

  const toggleStartup = useCallback(async () => {
    if (!settings || settingsBusy) return;
    const enabled = !settings.run_on_startup;
    setSettingsBusy(true);
    try {
      const actual = await invoke<boolean>("set_run_on_startup", { enabled });
      setSettings((current) =>
        current ? { ...current, run_on_startup: actual } : current,
      );
      setSettingsError(null);
    } catch (reason) {
      setSettingsError(String(reason));
    } finally {
      setSettingsBusy(false);
    }
  }, [setSettings, settings, settingsBusy]);

  const toggleHarness = useCallback(
    async (harness: HarnessId) => {
      if (!settings || settingsBusy) return;
      const visible = !settings.harness_visibility[harness];
      setSettingsBusy(true);
      try {
        setSettings(
          await invoke<AppSettings>("set_harness_visible", { harness, visible }),
        );
        setSettingsError(null);
      } catch (reason) {
        setSettingsError(String(reason));
      } finally {
        setSettingsBusy(false);
      }
    },
    [setSettings, settings, settingsBusy],
  );

  const updateGateway = useCallback(
    async (updates: Partial<AppSettings["channel_gateway"]>) => {
      if (!settings || settingsBusy) return;
      const next = { ...settings.channel_gateway, ...updates };
      if (next.primary_host_id && next.primary_host_id === next.secondary_host_id) {
        next.secondary_host_id = null;
      }
      setSettingsBusy(true);
      try {
        setSettings(
          await invoke<AppSettings>("set_channel_gateway", {
            request: {
              primaryHostId: next.primary_host_id,
              secondaryHostId: next.secondary_host_id,
              automaticFailover: next.automatic_failover,
              failoverAfterSeconds: next.failover_after_seconds,
            },
          }),
        );
        setSettingsError(null);
      } catch (reason) {
        setSettingsError(String(reason));
      } finally {
        setSettingsBusy(false);
      }
    },
    [setSettings, settings, settingsBusy],
  );

  const configurePhoton = useCallback(async () => {
    if (!settings || settingsBusy) return;
    setSettingsBusy(true);
    try {
      setSettings(
        await invoke<AppSettings>("configure_photon_gateway", {
          projectId: photonProjectId,
          projectSecret: photonProjectSecret || null,
          allowedSenders: photonAllowedSenders
            .split(",")
            .map((sender) => sender.trim())
            .filter(Boolean),
        }),
      );
      setPhotonProjectSecret("");
      setSettingsError(null);
    } catch (reason) {
      setSettingsError(String(reason));
    } finally {
      setSettingsBusy(false);
    }
  }, [
    photonAllowedSenders,
    photonProjectId,
    photonProjectSecret,
    setSettings,
    settings,
    settingsBusy,
  ]);

  const clearPhotonCredentials = useCallback(async () => {
    if (!settings?.photon_credentials_configured || settingsBusy) return;
    setSettingsBusy(true);
    try {
      setSettings(await invoke<AppSettings>("clear_photon_gateway_credentials"));
      setPhotonProjectSecret("");
      setSettingsError(null);
    } catch (reason) {
      setSettingsError(String(reason));
    } finally {
      setSettingsBusy(false);
    }
  }, [setSettings, settings, settingsBusy]);

  const openConnector = useCallback(
    (
      client:
        | "hermes"
        | "hermes_cli"
        | "opencode"
        | "opencode_cli"
        | "codex"
        | "claude_code"
        | "pi"
        | "copilot"
        | "vscode",
      anchor: HTMLElement,
    ) => {
      setSelectedHost(null);
      const epoch = menuEpoch.current;
      if (epoch === null) return;
      const requestId = ++submenuRequest.current;
      void invoke("show_model_menu", {
        hostId: `connector:${client}`,
        anchorY: layoutOffsetTop(anchor),
        requestId,
        menuEpoch: epoch,
      }).catch((reason) => setLaunchError(String(reason)));
    },
    [],
  );

  const launchCli = useCallback(
    async (
      client: CliClient,
      selectedModel: string | null | undefined,
      selectedAvailable: boolean,
      anchor: HTMLElement,
    ) => {
      if (!selectedModel || !selectedAvailable) {
        openConnector(client, anchor);
        return;
      }
      try {
        const launchClient = client === "opencode_cli"
          ? "opencode"
          : client === "hermes_cli"
            ? "hermes"
            : client;
        await invoke("launch_cli", { client: launchClient });
        setLaunchError(null);
        await invoke("hide_tray_menus");
      } catch (reason) {
        setLaunchError(String(reason));
      }
    },
    [openConnector],
  );

  useEffect(() => {
    snapshotMounted.current = true;
    void readSnapshot();
    void readChannelRoutes();
    void readChannelAdapters();
    let timer: number | undefined;
    const stopPolling = () => {
      if (timer !== undefined) window.clearInterval(timer);
      timer = undefined;
    };
    const startPolling = () => {
      stopPolling();
      timer = window.setInterval(() => {
        void readSnapshot();
        void readChannelRoutes();
        void readChannelAdapters();
      }, 2_000);
    };
    let disposed = false;
    let unlisten: (() => void) | undefined;
    let unlistenClose: (() => void) | undefined;
    listen<{ origin: string; menu_epoch: number }>("tray-menu-opened", ({ payload }) => {
      if (animationTimer.current !== null) window.clearTimeout(animationTimer.current);
      menuEpoch.current = payload.menu_epoch;
      setMenuOrigin(payload.origin === "top-right" ? "top-right" : "bottom-right");
      setMenuPhase("opening");
      animationTimer.current = window.setTimeout(() => setMenuPhase("open"), 175);
      setSelectedHost(null);
      setSettingsOpen(false);
      setLaunchError(null);
      void readSnapshot();
      void readChannelRoutes();
      void readChannelAdapters();
      startPolling();
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    });
    listen("tray-menus-closing", stopPolling).then((cleanup) => {
      if (disposed) cleanup();
      else unlistenClose = cleanup;
    });
    return () => {
      disposed = true;
      snapshotMounted.current = false;
      snapshotRequest.current += 1;
      stopPolling();
      unlisten?.();
      unlistenClose?.();
    };
  }, [readChannelAdapters, readChannelRoutes, readSnapshot]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    listen("tray-menus-closing", () => {
      if (animationTimer.current !== null) window.clearTimeout(animationTimer.current);
      menuEpoch.current = null;
      setMenuPhase("closing");
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    });
    return () => {
      disposed = true;
      if (animationTimer.current !== null) window.clearTimeout(animationTimer.current);
      unlisten?.();
    };
  }, []);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    listen<{ path: string }>("llama-swap-config-changed", async ({ payload }) => {
      if (configChangeHandling.current) return;
      configChangeHandling.current = true;
      try {
        const accepted = window.confirm(
          "The local llama-swap profile configuration changed.\n\n" +
            `Restart llama-swap now?\n${payload.path}`,
        );
        if (!accepted) return;
        let outcome = await invoke<ControlOutcome>("restart_local_llama_swap", {
          force: false,
        });
        if (outcome.state === "conflict") {
          const force = window.confirm(
            `${outcome.active_requests} active request(s) are using the local host. ` +
              "Cancel them and restart llama-swap?",
          );
          if (!force) return;
          outcome = await invoke<ControlOutcome>("restart_local_llama_swap", {
            force: true,
          });
        }
        if (outcome.state === "conflict") {
          throw new Error("The local service remained busy and was not restarted.");
        }
        await readSnapshot();
      } catch (reason) {
        window.alert(`Unable to restart llama-swap: ${String(reason)}`);
      } finally {
        configChangeHandling.current = false;
      }
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [readSnapshot]);

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
      const height = Math.ceil(shell.scrollHeight);
      if (height === lastHeight.current) return;
      lastHeight.current = height;
      void invoke("resize_tray_menu", { height });
    };
    const observer = new MutationObserver(measure);
    observer.observe(shell, {
      childList: true,
      characterData: true,
      subtree: true,
    });
    measure();
    return () => observer.disconnect();
  }, []);

  const local = useMemo(
    () => fleet?.hosts.find((host) => host.id === fleet.local_host_id),
    [fleet],
  );
  const anyLoaded = useMemo(
    () =>
      fleet?.hosts.some(
        (host) => host.connection !== "offline" && host.loaded_model_id !== null,
      ) ?? false,
    [fleet],
  );
  const runningCount = useMemo(
    () => {
      if (!fleet) return 0;
      return fleet.hosts.filter((host) => {
        if (host.connection === "offline" || !host.loaded_model_id) return false;
        const profile = host.models.find((model) => model.id === host.loaded_model_id);
        return Boolean(
          profile &&
            profile.kind === "text" &&
            profile.capabilities.some((capability) =>
              ["chat", "completions", "responses", "anthropic_messages"].includes(capability),
            ),
        );
      }).length;
    },
    [fleet],
  );
  const runningModels = useMemo(() => {
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
  const selectedAvailable = useCallback(
    (selectedModel: string | null | undefined) =>
      !selectedModel || runningModels.has(selectedModel),
    [runningModels],
  );
  const photonChannelRoutes = useMemo(
    () => channelRoutes.filter((route) => ["photon", "imessage"].includes(route.channel)),
    [channelRoutes],
  );
  const activeChannelConversations = useMemo(
    () => photonChannelRoutes.filter((route) => route.archived_at_ms === undefined).length,
    [photonChannelRoutes],
  );
  const latestChannelRoute = useMemo(
    () => photonChannelRoutes
      .filter((route) => route.archived_at_ms === undefined)
      .reduce<ChannelRoute | null>(
        (latest, route) => !latest || route.updated_at_ms > latest.updated_at_ms ? route : latest,
        null,
      ),
    [photonChannelRoutes],
  );
  const onlineChannelAdapters = useMemo(
    () => channelAdapters.filter((adapter) => adapter.online),
    [channelAdapters],
  );
  const latestChannelRouteSummary = useMemo(() => {
    if (!latestChannelRoute) {
      return onlineChannelAdapters.length > 0
        ? `${onlineChannelAdapters[0].display_name} · Not routed`
        : "Configure a messaging adapter";
    }
    if (activeChannelConversations > 1) {
      return `${activeChannelConversations} routed conversations`;
    }
    const modelHost = fleet?.hosts.find((host) => host.id === latestChannelRoute.host_id);
    const harnessHost = latestChannelRoute.harness_host_id
      ? fleet?.hosts.find((host) => host.id === latestChannelRoute.harness_host_id)
      : undefined;
    const model = modelHost?.models.find((profile) => profile.id === latestChannelRoute.model_id);
    const harness = `${channelHarnessLabel(latestChannelRoute.harness)}${
      harnessHost ? ` on ${harnessHost.display_name}` : ""
    }`;
    const target = `${model?.display_name ?? latestChannelRoute.model_id} on ${
      modelHost?.display_name ?? latestChannelRoute.host_id
    }`;
    return `${harness} → ${target}`;
  }, [activeChannelConversations, fleet, latestChannelRoute, onlineChannelAdapters]);

  return (
    <main
      className={`tray-shell menu-${menuPhase} origin-${menuOrigin}`}
      ref={shellRef}
    >
      <header className="tray-header">
        <div className="tray-brand">
          <span className="tray-mark" aria-hidden="true" />
          <div>
            <strong>Agent Relay - {__APP_VERSION__}</strong>
            <span>Local inference</span>
          </div>
        </div>
        <div className="tray-header-actions">
          <button
            className={`icon-button settings-button ${settingsOpen ? "active" : ""}`}
            aria-label="Settings"
            aria-expanded={settingsOpen}
            onClick={() => setSettingsOpen((open) => !open)}
          >
            ⚙
          </button>
          <button
            className="icon-button"
            aria-label="Close menu"
            onClick={() => invoke("hide_tray_menus")}
          >
            ×
          </button>
        </div>
      </header>

      {settingsOpen && (
        <section className="tray-settings" aria-label="Settings">
          <div className="setting-row theme-setting">
            <div>
              <strong>Appearance</strong>
              <span>Choose a theme for every Agent Relay window.</span>
            </div>
            <div className="theme-options" role="group" aria-label="Appearance">
              {(["system", "light", "dark"] as ThemePreference[]).map((theme) => (
                <button
                  className={settings?.theme === theme ? "active" : ""}
                  disabled={!settings || settingsBusy}
                  key={theme}
                  onClick={() => selectTheme(theme)}
                >
                  {theme[0].toUpperCase() + theme.slice(1)}
                </button>
              ))}
            </div>
          </div>
          <button
            className="setting-row startup-setting"
            disabled={!settings || settingsBusy}
            onClick={toggleStartup}
          >
            <span>
              <strong>Run on startup</strong>
              <small>Start Agent Relay and llama-swap idle when you sign in.</small>
            </span>
            <span
              className={`switch ${settings?.run_on_startup ? "on" : ""}`}
              role="switch"
              aria-checked={settings?.run_on_startup ?? false}
              aria-label="Run on startup"
            >
              <span />
            </span>
          </button>
          <div className="gateway-setting">
            <div>
              <strong>Messaging gateway</strong>
              <span>Choose the Photon connection hosts. The standby takes over automatically.</span>
            </div>
            <div className="gateway-host-options">
              <label>
                Primary
                <select
                  aria-label="Primary messaging gateway"
                  disabled={!settings || settingsBusy || !fleet}
                  value={settings?.channel_gateway.primary_host_id ?? ""}
                  onChange={(event) =>
                    void updateGateway({ primary_host_id: event.target.value || null })
                  }
                >
                  <option value="">Not configured</option>
                  {fleet?.hosts.map((host) => (
                    <option key={host.id} value={host.id}>{host.display_name}</option>
                  ))}
                </select>
              </label>
              <label>
                Standby
                <select
                  aria-label="Standby messaging gateway"
                  disabled={!settings || settingsBusy || !fleet}
                  value={settings?.channel_gateway.secondary_host_id ?? ""}
                  onChange={(event) =>
                    void updateGateway({ secondary_host_id: event.target.value || null })
                  }
                >
                  <option value="">None</option>
                  {fleet?.hosts
                    .filter((host) => host.id !== settings?.channel_gateway.primary_host_id)
                    .map((host) => (
                      <option key={host.id} value={host.id}>{host.display_name}</option>
                    ))}
                </select>
              </label>
            </div>
            <button
              className="gateway-failover-toggle"
              disabled={!settings || settingsBusy}
              role="switch"
              aria-label="Automatic failover"
              aria-checked={settings?.channel_gateway.automatic_failover ?? false}
              onClick={() =>
                void updateGateway({
                  automatic_failover: !settings?.channel_gateway.automatic_failover,
                })
              }
            >
              <span>
                <strong>Automatic failover</strong>
                <small>{settings?.channel_gateway.failover_after_seconds ?? 60}-second failure window</small>
              </span>
              <span
                className={`switch ${settings?.channel_gateway.automatic_failover ? "on" : ""}`}
                aria-hidden="true"
              >
                <span />
              </span>
            </button>
            <div className="gateway-status-list">
              {fleet?.hosts
                .filter((host) =>
                  host.id === settings?.channel_gateway.primary_host_id ||
                  host.id === settings?.channel_gateway.secondary_host_id,
                )
                .map((host) => (
                  <span key={host.id}>
                    {host.display_name}: {host.channel_gateway?.state ??
                      (host.connection === "offline" ? "offline" : "waiting for gateway")}
                  </span>
                ))}
            </div>
            <button
              aria-expanded={photonSetupOpen}
              className="photon-setup-toggle"
              onClick={() => setPhotonSetupOpen((open) => !open)}
            >
              <span>
                <strong>Configure Photon</strong>
                <small>
                  {settings?.photon_credentials_configured
                    ? "Credentials stored on this machine"
                    : "Project credentials required"}
                </small>
              </span>
              <span aria-hidden="true">{photonSetupOpen ? "⌃" : "⌄"}</span>
            </button>
            {photonSetupOpen && (
              <div className="photon-setup-panel">
                <label>
                  Project ID
                  <input
                    aria-label="Photon project ID"
                    disabled={settingsBusy}
                    value={photonProjectId}
                    onChange={(event) => setPhotonProjectId(event.target.value)}
                  />
                </label>
                <label>
                  Project secret
                  <input
                    aria-label="Photon project secret"
                    autoComplete="off"
                    disabled={settingsBusy}
                    placeholder={settings?.photon_credentials_configured ? "Leave blank to keep saved secret" : "Required"}
                    type="password"
                    value={photonProjectSecret}
                    onChange={(event) => setPhotonProjectSecret(event.target.value)}
                  />
                </label>
                <label>
                  Allowed senders
                  <input
                    aria-label="Photon allowed senders"
                    disabled={settingsBusy}
                    placeholder="+15551234567, +15557654321"
                    value={photonAllowedSenders}
                    onChange={(event) => setPhotonAllowedSenders(event.target.value)}
                  />
                </label>
                <button
                  className="photon-save-button"
                  disabled={
                    settingsBusy ||
                    !photonProjectId.trim() ||
                    !photonAllowedSenders.trim() ||
                    (!settings?.photon_credentials_configured && !photonProjectSecret.trim())
                  }
                  onClick={() => void configurePhoton()}
                >
                  Save and provision gateways
                </button>
                {settings?.photon_credentials_configured && (
                  <button
                    className="photon-clear-button"
                    disabled={settingsBusy}
                    onClick={() => void clearPhotonCredentials()}
                  >
                    Remove credentials from this machine
                  </button>
                )}
                <small>
                  The secret is stored in Windows Credential Manager or macOS Keychain,
                  never in fleet.json.
                </small>
              </div>
            )}
          </div>
          <div className="harness-visibility-setting">
            <div>
              <strong>Show harnesses</strong>
              <span>Choose which clients appear in the main menu.</span>
            </div>
            <div className="harness-visibility-options" role="group" aria-label="Visible harnesses">
              {HARNESS_LABELS.map(({ id, label }) => (
                <button
                  aria-pressed={settings?.harness_visibility[id] ?? true}
                  className={settings?.harness_visibility[id] === false ? "" : "active"}
                  disabled={!settings || settingsBusy}
                  key={id}
                  onClick={() => toggleHarness(id)}
                >
                  {label}
                </button>
              ))}
            </div>
          </div>
          <div className="harness-setup-setting">
            <button
              aria-expanded={harnessSetupOpen}
              className="harness-setup-toggle"
              onClick={() => setHarnessSetupOpen((open) => !open)}
            >
              <span>
                <strong>Configure harnesses</strong>
                <small>Detect and connect clients on any online machine.</small>
              </span>
              <span aria-hidden="true">{harnessSetupOpen ? "⌃" : "⌄"}</span>
            </button>
            {harnessSetupOpen && (
              <div className="harness-setup-panel">
                <label>
                  Machine
                  <select
                    aria-label="Harness setup machine"
                    disabled={!fleet || harnessSetupBusy !== null}
                    value={harnessSetupHost ?? ""}
                    onChange={(event) => setHarnessSetupHost(event.target.value)}
                  >
                    {fleet?.hosts.map((host) => (
                      <option
                        disabled={host.connection === "offline"}
                        key={host.id}
                        value={host.id}
                      >
                        {host.display_name}{host.id === fleet.local_host_id ? " (Local)" : ""}
                        {host.connection === "offline" ? " — Offline" : ""}
                      </option>
                    ))}
                  </select>
                </label>
                <div className="harness-setup-list" aria-busy={harnessSetupBusy !== null}>
                  {harnessSetupStatuses.map((status) => (
                    <div className="harness-setup-row" key={status.id} title={status.error ?? undefined}>
                      <span>
                        <strong>{status.label}</strong>
                        <small>{status.state.replace("_", " ")}</small>
                      </span>
                      <button
                        disabled={
                          status.state === "not_installed" || harnessSetupBusy !== null
                        }
                        onClick={() => configureHarness(status.id)}
                      >
                        {harnessSetupBusy === status.id
                          ? "Working…"
                          : status.state === "configured"
                            ? "Reconfigure"
                            : status.state === "needs_repair"
                              ? "Repair"
                              : status.state === "not_installed"
                                ? "Not installed"
                                : "Configure"}
                      </button>
                    </div>
                  ))}
                  {harnessSetupBusy === "refresh" && (
                    <p className="harness-setup-loading">Checking this machine…</p>
                  )}
                </div>
                {harnessSetupError && <p className="settings-error">{harnessSetupError}</p>}
              </div>
            )}
          </div>
          {settingsError && <p className="settings-error">{settingsError}</p>}
        </section>
      )}

      <div className="tray-host-list" aria-busy={!fleet}>
        {!fleet && !snapshotError && <p className="tray-loading">Reading fleet…</p>}
        {snapshotError && <p className="tray-snapshot-error">Fleet status: {snapshotError}</p>}
        {fleet?.hosts.map((host) => {
          const localHost = host.id === fleet.local_host_id;
          return (
            <section
              className={`tray-host ${selectedHost === host.id ? "submenu-open" : ""}`}
              key={host.id}
            >
              <button
                className="tray-host-toggle"
                aria-haspopup="menu"
                aria-expanded={selectedHost === host.id}
                onClick={(event) => {
                  setSelectedHost(host.id);
                  const epoch = menuEpoch.current;
                  if (epoch === null) return;
                  const requestId = ++submenuRequest.current;
                  void invoke("show_model_menu", {
                    hostId: host.id,
                    anchorY: layoutOffsetTop(event.currentTarget),
                    requestId,
                    menuEpoch: epoch,
                  }).catch((reason) => setLaunchError(String(reason)));
                }}
              >
                <span className={`indicator ${host.connection}`} aria-hidden="true" />
                <span className="tray-host-copy">
                  <strong>
                    {host.display_name}
                    {localHost && <small>Local</small>}
                  </strong>
                  <span>{hostSummary(host)}</span>
                </span>
                <span className="chevron" aria-hidden="true">
                  ›
                </span>
              </button>
            </section>
          );
        })}
      </div>

      <div className="tray-global-actions">
        <button disabled={!local?.loaded_model_id} onClick={() => dispatch("unload_local")}>
          Unload local
        </button>
        <button disabled={!anyLoaded} onClick={() => dispatch("unload_all")}>
          Unload all
        </button>
        <button onClick={() => dispatch("refresh")}>Refresh</button>
      </div>

      <button
        aria-haspopup="menu"
        className="tray-integration channel-routes-entry"
        disabled={activeChannelConversations === 0 && onlineChannelAdapters.length === 0}
        onClick={(event) => {
          const epoch = menuEpoch.current;
          if (epoch === null) return;
          const requestId = ++submenuRequest.current;
          void invoke("show_model_menu", {
            hostId: "channels",
            anchorY: layoutOffsetTop(event.currentTarget),
            requestId,
            menuEpoch: epoch,
          }).catch((reason) => setLaunchError(String(reason)));
        }}
      >
        <span>Photon route</span>
        <small title={latestChannelRouteSummary}>{latestChannelRouteSummary}</small>
        <span className="chevron" aria-hidden="true">›</span>
      </button>

      {settings?.harness_visibility.opencode !== false && <button
        className={`tray-integration ${fleet?.opencode.state ?? "disabled"} ${
          fleet?.opencode.selected_model &&
          !selectedAvailable(fleet.opencode.selected_model)
            ? "unavailable"
            : ""
        }`}
        title={fleet?.opencode.error ?? undefined}
        disabled={!fleet}
        onClick={(event) =>
          openConnector("opencode", event.currentTarget)
        }
      >
        <span>Route OpenCode</span>
        <small>
          {connectorSummary(
            fleet?.opencode.selected_model,
            selectedAvailable(fleet?.opencode.selected_model),
            runningCount,
          )}
        </small>
        <span className="chevron" aria-hidden="true">›</span>
      </button>}

      {settings?.harness_visibility.opencode_cli !== false && <CliLauncher
        client="opencode_cli"
        error={fleet?.opencode.error}
        label="OpenCode CLI"
        onChoose={openConnector}
        onLaunch={launchCli}
        runningCount={runningCount}
        selectedAvailable={selectedAvailable(fleet?.opencode.selected_model)}
        selectedModel={fleet?.opencode.selected_model}
        state={fleet?.opencode.state ?? "disabled"}
      />}

      {settings?.harness_visibility.codex !== false && <CliLauncher
        client="codex"
        error={fleet?.codex.error}
        label="Codex CLI"
        onChoose={openConnector}
        onLaunch={launchCli}
        runningCount={runningCount}
        selectedAvailable={selectedAvailable(fleet?.codex.selected_model)}
        selectedModel={fleet?.codex.selected_model}
        state={fleet?.codex.state ?? "disabled"}
      />}

      {settings?.harness_visibility.claude_code !== false && <CliLauncher
        client="claude_code"
        error={fleet?.claude_code.error}
        label="Claude Code"
        onChoose={openConnector}
        onLaunch={launchCli}
        runningCount={runningCount}
        selectedAvailable={selectedAvailable(fleet?.claude_code.selected_model)}
        selectedModel={fleet?.claude_code.selected_model}
        state={fleet?.claude_code.state ?? "disabled"}
      />}

      {settings?.harness_visibility.copilot !== false && <CliLauncher
        client="copilot"
        error={fleet?.copilot.error}
        label="Copilot CLI"
        onChoose={openConnector}
        onLaunch={launchCli}
        runningCount={runningCount}
        selectedAvailable={selectedAvailable(fleet?.copilot.selected_model)}
        selectedModel={fleet?.copilot.selected_model}
        state={fleet?.copilot.state ?? "disabled"}
      />}

      {settings?.harness_visibility.vscode !== false && <button
        className={`tray-integration ${fleet?.vscode.state ?? "disabled"} ${
          fleet?.vscode.selected_model && !selectedAvailable(fleet.vscode.selected_model)
            ? "unavailable"
            : ""
        }`}
        title={fleet?.vscode.error ?? undefined}
        disabled={!fleet}
        onClick={(event) =>
          openConnector("vscode", event.currentTarget)
        }
      >
        <span>Connect VS Code</span>
        <small>
          {connectorSummary(
            fleet?.vscode.selected_model,
            selectedAvailable(fleet?.vscode.selected_model),
            runningCount,
          )}
        </small>
        <span className="chevron" aria-hidden="true">›</span>
      </button>}

      {settings?.harness_visibility.pi !== false && <CliLauncher
        client="pi"
        error={fleet?.pi.error}
        label="Pi CLI"
        onChoose={openConnector}
        onLaunch={launchCli}
        runningCount={runningCount}
        selectedAvailable={selectedAvailable(fleet?.pi.selected_model)}
        selectedModel={fleet?.pi.selected_model}
        state={fleet?.pi.state ?? "disabled"}
      />}

      {launchError && <p className="tray-launch-error">{launchError}</p>}

      {settings?.harness_visibility.hermes !== false && <button
        className={`tray-integration ${fleet?.hermes.state ?? "pending"} ${
          fleet?.hermes.selected_model && !selectedAvailable(fleet.hermes.selected_model)
            ? "unavailable"
            : ""
        }`}
        title={fleet?.hermes.error ?? undefined}
        disabled={!fleet}
        onClick={(event) =>
          openConnector("hermes", event.currentTarget)
        }
      >
        <span>Route Hermes</span>
        <small>
          {connectorSummary(
            fleet?.hermes.selected_model,
            selectedAvailable(fleet?.hermes.selected_model),
            runningCount,
          )}
        </small>
        <span className="chevron" aria-hidden="true">›</span>
      </button>}

      {settings?.harness_visibility.hermes_cli !== false && <CliLauncher
        client="hermes_cli"
        error={fleet?.hermes_cli.error}
        label="Hermes CLI"
        onChoose={openConnector}
        onLaunch={launchCli}
        runningCount={runningCount}
        selectedAvailable={selectedAvailable(fleet?.hermes_cli.selected_model)}
        selectedModel={fleet?.hermes_cli.selected_model}
        state={fleet?.hermes_cli.state ?? "disabled"}
      />}

      <footer className="tray-footer">
        <button onClick={openStatus}>Agent Relay status</button>
        <button className="quit-button" onClick={quit}>
          Quit
        </button>
      </footer>
    </main>
  );
}

export default TrayMenu;
