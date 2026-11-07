import { AutoRoutingSettings } from "./AutoRoutingSettings";
import { canonicalClient, useRouting } from "./routing";
import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import "./App.css";
import { readCliProject, saveCliProject } from "./cliProjects";
import type { ControlOutcome, FleetSnapshot, HostStatus } from "./fleet";
import { measureMenuHeight } from "./menuSizing";
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
  if (host.available === false) return host.active_requests ? `Unavailable · finishing ${host.active_requests}` : "Unavailable for new work";
  if (host.connection === "offline") return "Offline";
  if (host.gpu_activity?.busy && Date.now() - host.gpu_activity.sampled_at_ms <= 90_000) return "GPU busy · Auto uses another machine";
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
  { id: "codex", label: "Codex CLI" },
  { id: "openbot", label: "OpenBot" },
  { id: "claude_code", label: "Claude Code" },
  { id: "copilot", label: "Copilot" },
  { id: "vscode", label: "VS Code" },
  { id: "pi", label: "Pi" },
  { id: "hermes", label: "Hermes" },
  { id: "hermes_cli", label: "Hermes CLI" },
];

function CliLauncher({
  auto = false,
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
  auto?: boolean;
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
  const [project, setProject] = useState(() => readCliProject(client));
  const [projectError, setProjectError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);
  const chooseProject = async () => {
    setPicking(true);
    try {
      const path = await invoke<string | null>("pick_cli_project_folder");
      if (path) setProject(saveCliProject(client, path));
      setProjectError(null);
    } catch (error) { setProjectError(String(error)); }
    finally { setPicking(false); }
  };
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
        <small>{auto ? "Auto · approved models" : connectorSummary(selectedModel, selectedAvailable, runningCount)}</small>
        <span className="launch-glyph" aria-hidden="true">▶</span>
      </button>
      <button
        aria-label={`Choose model for ${label}`}
        className="tray-cli-chooser"
        onClick={(event) => onChoose(client, event.currentTarget)}
      >
        ›
      </button>
      <div className="cli-project-controls">
        <select aria-label={`Project folder for ${label}`} title={project.selected ?? "Home folder"}
          value={project.selected ?? ""} disabled={picking}
          onChange={(event) => {
            try { setProject(saveCliProject(client, event.target.value || null)); setProjectError(null); }
            catch (error) { setProjectError(String(error)); }
          }}>
          <option value="">Home folder</option>
          {project.recent.map((path) => <option key={path} value={path}>{path}</option>)}
        </select>
        <button disabled={picking} aria-label={`Choose project folder for ${label}`}
          onClick={() => void chooseProject()}>{picking ? "Choosing…" : "Folder…"}</button>
      </div>
      {projectError && <small className="cli-project-error" role="alert">{projectError}</small>}
    </div>
  );
}

export function TrayMenu() {
  const autoRouting = useRouting();
  const [routingOpen, setRoutingOpen] = useState(false);
  const [availabilityBusy, setAvailabilityBusy] = useState<string | null>(null);
  const { settings, setSettings, settingsLoadError } = useAppSettings();
  const [fleet, setFleet] = useState<FleetSnapshot | null>(null);
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

  const openConnector = useCallback(
    (
      client:
        | "hermes"
        | "hermes_cli"
        | "opencode"
        | "opencode_cli"
        | "openbot"
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
      if (autoRouting.routing.clients[canonicalClient(client)]?.mode !== "auto" && (!selectedModel || !selectedAvailable)) {
        openConnector(client, anchor);
        return;
      }
      try {
        const launchClient = client === "opencode_cli"
          ? "opencode"
          : client === "hermes_cli"
            ? "hermes"
            : client;
        await invoke("launch_cli", { client: launchClient, directory: readCliProject(client).selected });
        setLaunchError(null);
        await invoke("hide_tray_menus");
      } catch (reason) {
        setLaunchError(String(reason));
      }
    },
    [openConnector, autoRouting.routing],
  );

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
      timer = window.setInterval(() => {
        void readSnapshot();
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
  }, [readSnapshot]);

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
        setLaunchError(`Unable to restart llama-swap: ${String(reason)}`);
        lastHeight.current = 0;
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
      const height = measureMenuHeight(shell);
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
        if (host.available === false || host.connection === "offline" || !host.loaded_model_id) return false;
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
      if (host.available === false || host.connection === "offline" || !host.loaded_model_id) continue;
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
          {settingsLoadError && <p className="settings-error" role="alert">Settings unavailable; retrying: {settingsLoadError}</p>}
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
          <button className="setting-row" disabled={!autoRouting.loaded} aria-expanded={routingOpen} onClick={() => setRoutingOpen((v) => !v)}>Auto routing {routingOpen ? "⌃" : "⌄"}</button>
          {routingOpen && <AutoRoutingSettings fleet={fleet} routing={autoRouting.routing} save={autoRouting.save} />}
          {autoRouting.error && <p role="alert">{autoRouting.error}</p>}
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
              <button className="host-availability" role="switch" aria-label={`${host.display_name} available for new work`}
                aria-checked={host.available !== false}
                disabled={host.connection === "offline" || availabilityBusy !== null || (host.routing_version ?? 0) < 2}
                title="Let current requests finish; block new work while off"
                onClick={async () => {
                  setAvailabilityBusy(host.id);
                  try {
                    await invoke("set_host_availability", { hostId: host.id, available: host.available === false });
                    setFleet(await invoke<FleetSnapshot>("get_fleet_snapshot"));
                    setLaunchError(null);
                  } catch (e) { setLaunchError(String(e)); } finally { setAvailabilityBusy(null); }
                }}>{availabilityBusy === host.id ? "…" : host.available === false ? "Off" : "On"}</button>
            </section>
          );
        })}
      </div>

      {Object.entries(autoRouting.routing.clients).filter(([, p]) => p.mode === "auto").map(([client]) =>
        <div className="auto-route-status" key={client}><strong>{client.replace(/_/g, " ")} · Auto</strong>
          <small>{autoRouting.choices[client] ? `Last assigned: ${autoRouting.choices[client]}` : "Waiting for a request"}</small></div>)}
      <div className="tray-global-actions">
        <button disabled={!local?.loaded_model_id} onClick={() => dispatch("unload_local")}>
          Unload local
        </button>
        <button disabled={!anyLoaded} onClick={() => dispatch("unload_all")}>
          Unload all
        </button>
        <button onClick={() => dispatch("refresh")}>Refresh</button>
      </div>



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
          {autoRouting.routing.clients["opencode"]?.mode === "auto" ? "Auto · approved models" : connectorSummary(
            fleet?.opencode.selected_model, selectedAvailable(fleet?.opencode.selected_model), runningCount)}
        </small>
        <span className="chevron" aria-hidden="true">›</span>
      </button>}

      {settings?.harness_visibility.opencode_cli !== false && <CliLauncher
        client="opencode_cli"
        auto={autoRouting.routing.clients[canonicalClient("opencode_cli")]?.mode === "auto"}
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
        auto={autoRouting.routing.clients[canonicalClient("codex")]?.mode === "auto"}
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
        auto={autoRouting.routing.clients[canonicalClient("claude_code")]?.mode === "auto"}
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
        auto={autoRouting.routing.clients[canonicalClient("copilot")]?.mode === "auto"}
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
          {autoRouting.routing.clients["vscode"]?.mode === "auto" ? "Auto · approved models" : connectorSummary(
            fleet?.vscode.selected_model, selectedAvailable(fleet?.vscode.selected_model), runningCount)}
        </small>
        <span className="chevron" aria-hidden="true">›</span>
      </button>}

      {settings?.harness_visibility.pi !== false && <CliLauncher
        client="pi"
        auto={autoRouting.routing.clients[canonicalClient("pi")]?.mode === "auto"}
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

      {settings?.harness_visibility.openbot !== false && <button
        className={`tray-integration ${fleet?.openbot.state ?? "pending"} ${
          fleet?.openbot.selected_model && !selectedAvailable(fleet.openbot.selected_model)
            ? "unavailable"
            : ""
        }`}
        title={fleet?.openbot.error ?? undefined}
        disabled={!fleet}
        onClick={(event) =>
          openConnector("openbot", event.currentTarget)
        }
      >
        <span>Route OpenBot</span>
        <small>
          {autoRouting.routing.clients["openbot"]?.mode === "auto" ? "Auto · approved models" : connectorSummary(
            fleet?.openbot.selected_model, selectedAvailable(fleet?.openbot.selected_model), runningCount)}
        </small>
        <span className="chevron" aria-hidden="true">›</span>
      </button>}

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
          {autoRouting.routing.clients["hermes"]?.mode === "auto" ? "Auto · approved models" : connectorSummary(
            fleet?.hermes.selected_model, selectedAvailable(fleet?.hermes.selected_model), runningCount)}
        </small>
        <span className="chevron" aria-hidden="true">›</span>
      </button>}

      {settings?.harness_visibility.hermes_cli !== false && <CliLauncher
        client="hermes_cli"
        auto={autoRouting.routing.clients[canonicalClient("hermes_cli")]?.mode === "auto"}
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
