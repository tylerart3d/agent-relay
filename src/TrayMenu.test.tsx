// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { testFleet, testSettings } from "./uiTestFixtures";


const testHarnessStatuses = [
  {
    id: "hermes",
    label: "Hermes",
    state: "configured",
    selected_model: "m1-pro/m1-running",
  },
  {
    id: "pi",
    label: "Pi",
    state: "detected",
  },
  {
    id: "vscode",
    label: "VS Code",
    state: "not_installed",
  },
];

const tauri = vi.hoisted(() => ({
  emit: vi.fn(),
  invoke: vi.fn(),
  listeners: new Map<string, (event: { payload: unknown }) => unknown>(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: tauri.invoke }));
vi.mock("@tauri-apps/api/event", () => ({
  emit: tauri.emit,
  listen: vi.fn(async (event: string, handler: (event: { payload: unknown }) => void) => {
    tauri.listeners.set(event, handler);
    return () => {
      if (tauri.listeners.get(event) === handler) tauri.listeners.delete(event);
    };
  }),
}));

import { TrayMenu } from "./TrayMenu";

it("shows automatic GPU avoidance without flipping the operator switch", async () => {
  const previous = tauri.invoke.getMockImplementation()!;
  tauri.invoke.mockImplementation(async (command, args) => {
    if (command === "get_fleet_snapshot") {
      const fleet = testFleet();
      fleet.hosts.find((h) => h.id === "workstation")!.gpu_activity = {
        busy: true, external_percent: 85, sampled_at_ms: Date.now(),
      };
      return fleet;
    }
    return previous(command, args);
  });
  render(<TrayMenu />);
  expect(await screen.findByText("GPU busy · Auto uses another machine")).toBeTruthy();
  expect(screen.getByRole("switch", {name:"WORKSTATION available for new work"}).getAttribute("aria-checked")).toBe("true");
  expect(tauri.invoke.mock.calls.some(([command]) => command === "set_host_availability")).toBe(false);
});

it("turns a machine off remotely without unloading it and reports failure inline", async () => {
  const user = userEvent.setup();
  const fleet = testFleet();
  const previous = tauri.invoke.getMockImplementation()!;
  tauri.invoke.mockImplementation(async (command, args) => {
    if (command === "get_fleet_snapshot") return structuredClone(fleet);
    if (command === "set_host_availability") {
      fleet.hosts.find((h) => h.id === "m1-pro")!.available = false;
      return;
    }
    return previous(command, args);
  });
  render(<TrayMenu />);
  const toggle = await screen.findByRole("switch", { name: "M1 Pro available for new work" });
  await user.click(toggle);
  await waitFor(() => expect(toggle.getAttribute("aria-checked")).toBe("false"));
  expect(tauri.invoke).toHaveBeenCalledWith("set_host_availability", {hostId:"m1-pro", available:false});
  expect(tauri.invoke.mock.calls.some(([command]) => command === "unload_host")).toBe(false);
  tauri.invoke.mockImplementation(async (command, args) => {
    if (command === "set_host_availability") throw new Error("Peer unreachable");
    if (command === "get_fleet_snapshot") return structuredClone(fleet);
    return previous(command, args);
  });
  await user.click(toggle);
  expect(await screen.findByText("Error: Peer unreachable")).toBeTruthy();
  expect(toggle.getAttribute("aria-checked")).toBe("false");
});

it("launches an Auto CLI with no model currently loaded", async () => {
  const previous = tauri.invoke.getMockImplementation()!;
  tauri.invoke.mockImplementation(async (command, args) => {
    if (command === "get_routing_config") return { models:[], clients:{pi:{mode:"auto",models:null,minimum_context:65536}} };
    if (command === "get_fleet_snapshot") { const fleet=testFleet(); fleet.pi.selected_model=null; fleet.hosts.forEach((h) => h.loaded_model_id=null); return fleet; }
    return previous(command,args);
  });
  render(<TrayMenu />);
  await screen.findByText("Auto · approved models");
  await userEvent.click(screen.getByRole("button",{name:"Launch Pi CLI"}));
  await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("launch_cli",{client:"pi",directory:null}));
});

function openTrayMenu(menuEpoch = 7) {
  const listener = tauri.listeners.get("tray-menu-opened");
  if (!listener) throw new Error("tray-menu-opened listener was not registered");
  act(() => listener({ payload: { origin: "top-right", menu_epoch: menuEpoch } }));
}

beforeEach(() => {
  localStorage.clear();
  tauri.emit.mockReset().mockResolvedValue(undefined);
  tauri.invoke.mockReset().mockImplementation(async (command: string, args?: unknown) => {
    if (command === "get_app_settings") return structuredClone(testSettings);
    if (command === "get_fleet_snapshot") return testFleet();
    if (command === "get_harness_setup_statuses") return structuredClone(testHarnessStatuses);
    if (command === "configure_fleet_harness") return structuredClone(testHarnessStatuses[1]);
    if (command === "set_theme") return { ...structuredClone(testSettings), theme: "light" };
    if (command === "set_run_on_startup") return true;
    if (command === "set_harness_visible") {
      const { harness, visible } = args as { harness: keyof typeof testSettings.harness_visibility; visible: boolean };
      const next = structuredClone(testSettings);
      next.harness_visibility[harness] = visible;
      return next;
    }
    return undefined;
  });
  tauri.listeners.clear();
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: vi.fn(() => ({
      matches: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    })),
  });
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("TrayMenu controls", () => {
  it("passes a chosen project to launch and keeps it on picker cancel", async () => {
    const fleet = testFleet();
    fleet.codex.selected_model = "m1-pro/m1-running";
    let picked: string | null = "/projects/My Game";
    tauri.invoke.mockImplementation(async (command: string) => {
      if (command === "get_app_settings") return structuredClone(testSettings);
      if (command === "get_fleet_snapshot") return fleet;
      if (command === "pick_cli_project_folder") return picked;
      return undefined;
    });
    const user = userEvent.setup();
    render(<TrayMenu />);
    await screen.findByText("WORKSTATION");
    await user.click(screen.getByLabelText("Choose project folder for Codex CLI"));
    await waitFor(() => expect((screen.getByLabelText("Project folder for Codex CLI") as HTMLSelectElement).value).toBe(picked));
    picked = null;
    await user.click(screen.getByLabelText("Choose project folder for Codex CLI"));
    await user.click(screen.getByLabelText("Launch Codex CLI"));
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("launch_cli", { client: "codex", directory: "/projects/My Game" }));
  });
  it("renders config-restart failures inline instead of opening a blocking alert", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    const alert = vi.spyOn(window, "alert").mockImplementation(() => undefined);
    tauri.invoke.mockImplementation(async (command: string) => {
      if (command === "get_app_settings") return structuredClone(testSettings);
      if (command === "get_fleet_snapshot") return testFleet();
      if (command === "get_channel_routes") return [];
      if (command === "get_channel_adapters") return [];
      if (command === "restart_local_llama_swap") throw new Error("taskkill exited with code 128");
      return undefined;
    });
    render(<TrayMenu />);
    await waitFor(() => expect(tauri.listeners.has("llama-swap-config-changed")).toBe(true));
    const listener = tauri.listeners.get("llama-swap-config-changed")!;
    await act(async () => {
      await listener({ payload: { path: "llama-swap.yaml" } });
    });

    expect(await screen.findByText(/taskkill exited with code 128/)).toBeTruthy();
    expect(confirm).toHaveBeenCalled();
    expect(alert).not.toHaveBeenCalled();
  });







  it("keeps launch buttons separate from CLI model choosers", async () => {
    const fleet = testFleet();
    fleet.codex.selected_model = "m1-pro/m1-running";
    tauri.invoke.mockImplementation(async (command: string) => {
      if (command === "get_app_settings") return structuredClone(testSettings);
      if (command === "get_fleet_snapshot") return fleet;
      return undefined;
    });
    const user = userEvent.setup();
    render(<TrayMenu />);
    await screen.findByText("WORKSTATION");
    await waitFor(() => expect(tauri.listeners.has("tray-menu-opened")).toBe(true));
    openTrayMenu();

    await user.click(screen.getByRole("button", { name: "Launch Codex CLI" }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("launch_cli", { client: "codex", directory: null }),
    );
    await user.click(screen.getByLabelText("Choose model for Codex CLI"));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith(
        "show_model_menu",
        expect.objectContaining({ hostId: "connector:codex" }),
      ),
    );
  });



  it("configures detected harnesses on the selected fleet machine", async () => {
    const user = userEvent.setup();
    render(<TrayMenu />);
    await screen.findByText("WORKSTATION");

    await user.click(screen.getByLabelText("Settings"));
    const setupToggle = screen.getByRole("button", { name: /Configure harnesses/ });
    expect(setupToggle.getAttribute("aria-expanded")).toBe("false");
    await user.click(setupToggle);

    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("get_harness_setup_statuses", {
        hostId: "m1-pro",
      }),
    );
    const machine = screen.getByLabelText("Harness setup machine");
    await user.selectOptions(machine, "workstation");
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("get_harness_setup_statuses", {
        hostId: "workstation",
      }),
    );

    await user.click(screen.getByRole("button", { name: "Configure" }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("configure_fleet_harness", {
        hostId: "workstation",
        harness: "pi",
      }),
    );
    expect(
      (screen.getByRole("button", { name: "Not installed" }) as HTMLButtonElement).disabled,
    ).toBe(true);
  });

  it("launches every configured CLI with its backend client name", async () => {
    const fleet = testFleet();
    const selected = "m1-pro/m1-running";
    fleet.opencode.selected_model = selected;
    fleet.hermes_cli.selected_model = selected;
    fleet.codex.selected_model = selected;
    fleet.claude_code.selected_model = selected;
    fleet.copilot.selected_model = selected;
    fleet.pi.selected_model = selected;
    tauri.invoke.mockImplementation(async (command: string) => {
      if (command === "get_app_settings") return structuredClone(testSettings);
      if (command === "get_fleet_snapshot") return fleet;
      return undefined;
    });
    const user = userEvent.setup();
    render(<TrayMenu />);
    await screen.findByText("WORKSTATION");

    const launchers: Array<[string, string]> = [
      ["Launch OpenCode CLI", "opencode"],
      ["Launch Codex CLI", "codex"],
      ["Launch Claude Code", "claude_code"],
      ["Launch Copilot CLI", "copilot"],
      ["Launch Pi CLI", "pi"],
      ["Launch Hermes CLI", "hermes"],
    ];
    for (const [label, client] of launchers) {
      await user.click(screen.getByRole("button", { name: label }));
      await waitFor(() =>
        expect(tauri.invoke).toHaveBeenCalledWith("launch_cli", { client, directory: null }),
      );
    }
  });

  it("routes close, status, and quit controls", async () => {
    const user = userEvent.setup();
    render(<TrayMenu />);
    await screen.findByText("WORKSTATION");

    await user.click(screen.getByRole("button", { name: "Agent Relay status" }));
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("show_status_window"));
    await user.click(screen.getByLabelText("Close menu"));
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("hide_tray_menus"));
    await user.click(screen.getByRole("button", { name: "Quit" }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("quit_app", { force: false }),
    );
  });

  it("requires confirmation before quitting with active requests and closes from Escape", async () => {
    tauri.invoke.mockImplementation(async (command: string, args?: unknown) => {
      if (command === "get_app_settings") return structuredClone(testSettings);
      if (command === "get_fleet_snapshot") return testFleet();
      if (command === "quit_app" && !(args as { force?: boolean })?.force) {
        return {
          state: "conflict",
          host_id: "workstation",
          active_requests: 2,
          loaded_model_id: "workstation-1",
          message: "busy",
        };
      }
      return undefined;
    });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    const user = userEvent.setup();
    render(<TrayMenu />);
    await screen.findByText("WORKSTATION");

    await user.click(screen.getByRole("button", { name: "Quit" }));
    expect(confirm).toHaveBeenCalledOnce();
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("quit_app", { force: true }),
    );
    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("hide_tray_menus"));
  });
});

  it("opens every host and connector submenu with the current tray epoch", async () => {
    const user = userEvent.setup();
    render(<TrayMenu />);
    expect(await screen.findByText(/Agent Relay - \d+\.\d+\.\d+/)).toBeTruthy();
    await screen.findByText("WORKSTATION");
    await waitFor(() => expect(tauri.listeners.has("tray-menu-opened")).toBe(true));
    openTrayMenu(19);

    const controls: Array<[HTMLElement, string]> = [
      [screen.getByText("M1 Pro").closest("button")!, "m1-pro"],
      [screen.getByText("WORKSTATION").closest("button")!, "workstation"],
      [screen.getByText("Air-M4").closest("button")!, "air-m4"],
      [screen.getByRole("button", { name: /Route OpenCode/ }), "connector:opencode"],
      [screen.getByLabelText("Choose model for OpenCode CLI"), "connector:opencode_cli"],
      [screen.getByLabelText("Choose model for Codex CLI"), "connector:codex"],
      [screen.getByLabelText("Choose model for Claude Code"), "connector:claude_code"],
      [screen.getByLabelText("Choose model for Copilot CLI"), "connector:copilot"],
      [screen.getByRole("button", { name: /Connect VS Code/ }), "connector:vscode"],
      [screen.getByLabelText("Choose model for Pi CLI"), "connector:pi"],
      [screen.getByRole("button", { name: /Route Hermes/ }), "connector:hermes"],
      [screen.getByLabelText("Choose model for Hermes CLI"), "connector:hermes_cli"],
    ];

    for (const [control, hostId] of controls) {
      await user.click(control);
      await waitFor(() =>
        expect(tauri.invoke).toHaveBeenCalledWith(
          "show_model_menu",
          expect.objectContaining({ hostId, menuEpoch: 19 }),
        ),
      );
    }
  });
  it("routes global actions and settings controls", async () => {
    const user = userEvent.setup();
    render(<TrayMenu />);
    await screen.findByText("WORKSTATION");

    await user.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(tauri.emit).toHaveBeenCalledWith("tray-action", "refresh"));
    expect(tauri.invoke).toHaveBeenCalledWith("hide_tray_menus");
    await user.click(screen.getByRole("button", { name: "Unload local" }));
    await waitFor(() => expect(tauri.emit).toHaveBeenCalledWith("tray-action", "unload_local"));
    await user.click(screen.getByRole("button", { name: "Unload all" }));
    await waitFor(() => expect(tauri.emit).toHaveBeenCalledWith("tray-action", "unload_all"));

    await user.click(screen.getByLabelText("Settings"));
    const appearance = screen.getByRole("group", { name: "Appearance" });
    await user.click(within(appearance).getByRole("button", { name: "Light" }));
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("set_theme", { theme: "light" }));

    await user.click(screen.getByRole("button", { name: /Run on startup/ }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("set_run_on_startup", { enabled: true }),
    );

  });


it("shows truthful Auto queue counts without changing a selected route", async () => {
  const previous = tauri.invoke.getMockImplementation()!;
  tauri.invoke.mockImplementation(async (command, args) => {
    if (command === "get_routing_config") return { models:[], clients:{hermes:{mode:"auto",models:null,minimum_context:65536}} };
    if (command === "get_routing_waiting") return {hermes:2};
    return previous(command, args);
  });
  render(<TrayMenu />);
  expect(await screen.findByText("Waiting for a free model · 2 queued")).toBeTruthy();
  expect(tauri.invoke.mock.calls.some(([command]) => command === "set_routing_config")).toBe(false);
});
