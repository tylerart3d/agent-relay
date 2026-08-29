// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { testFleet, testSettings } from "./uiTestFixtures";

const testChannelRoutes = [
  {
    channel: "photon",
    account_id: "personal",
    conversation_id: "chat-1",
    session_id: 2,
    conversation_label: "Product brainstorm",
    harness: "hermes",
    harness_host_id: "m1-pro",
    host_id: "m1-pro",
    model_id: "m1-running",
    updated_at_ms: 2,
  },
];

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

function openTrayMenu(menuEpoch = 7) {
  const listener = tauri.listeners.get("tray-menu-opened");
  if (!listener) throw new Error("tray-menu-opened listener was not registered");
  act(() => listener({ payload: { origin: "top-right", menu_epoch: menuEpoch } }));
}

beforeEach(() => {
  tauri.emit.mockReset().mockResolvedValue(undefined);
  tauri.invoke.mockReset().mockImplementation(async (command: string, args?: unknown) => {
    if (command === "get_app_settings") return structuredClone(testSettings);
    if (command === "get_fleet_snapshot") return testFleet();
    if (command === "get_channel_routes") return structuredClone(testChannelRoutes);
    if (command === "get_channel_adapters") return [];
    if (command === "get_harness_setup_statuses") return structuredClone(testHarnessStatuses);
    if (command === "configure_fleet_harness") return structuredClone(testHarnessStatuses[1]);
    if (command === "set_theme") return { ...structuredClone(testSettings), theme: "light" };
    if (command === "set_run_on_startup") return true;
    if (command === "set_channel_gateway") {
      const next = structuredClone(testSettings);
      const request = (args as { request: {
        primaryHostId: string | null;
        secondaryHostId: string | null;
        automaticFailover: boolean;
        failoverAfterSeconds: number;
      } }).request;
      next.channel_gateway = {
        ...next.channel_gateway,
        primary_host_id: request.primaryHostId,
        secondary_host_id: request.secondaryHostId,
        automatic_failover: request.automaticFailover,
        failover_after_seconds: request.failoverAfterSeconds,
      };
      return next;
    }
    if (command === "configure_photon_gateway") return structuredClone(testSettings);
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

  it("shows a connected Photon adapter before its first conversation", async () => {
    tauri.invoke.mockImplementation(async (command: string) => {
      if (command === "get_app_settings") return structuredClone(testSettings);
      if (command === "get_fleet_snapshot") return testFleet();
      if (command === "get_channel_routes") return [];
      if (command === "get_channel_adapters") {
        return [{
          adapter_id: "photon-imessage",
          channel: "imessage",
          display_name: "Photon iMessage",
          state: "connected",
          online: true,
          last_seen_ms: Date.now(),
        }];
      }
      return undefined;
    });
    render(<TrayMenu />);
    const routes = await screen.findByRole("button", { name: /Photon route/ });
    expect(routes.getAttribute("disabled")).toBeNull();
    expect(within(routes).getByText("Photon iMessage · Not routed")).toBeTruthy();
  });

  it("shows the active Photon harness, model, and machines in the main menu", async () => {
    render(<TrayMenu />);
    const route = await screen.findByRole("button", { name: /Photon route/ });
    expect(
      within(route).getByText("Hermes on M1 Pro → M1 Running on M1 Pro"),
    ).toBeTruthy();
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
      [screen.getByRole("button", { name: /Photon route/ }), "channels"],
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
      expect(tauri.invoke).toHaveBeenCalledWith("launch_cli", { client: "codex" }),
    );
    await user.click(screen.getByLabelText("Choose model for Codex CLI"));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith(
        "show_model_menu",
        expect.objectContaining({ hostId: "connector:codex" }),
      ),
    );
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

    await user.selectOptions(screen.getByLabelText("Standby messaging gateway"), "air-m4");
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("set_channel_gateway", {
        request: {
          primaryHostId: "workstation",
          secondaryHostId: "air-m4",
          automaticFailover: true,
          failoverAfterSeconds: 60,
        },
      }),
    );
    await user.click(screen.getByRole("switch", { name: "Automatic failover" }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith(
        "set_channel_gateway",
        { request: expect.objectContaining({ automaticFailover: false }) },
      ),
    );

    await user.click(screen.getByRole("button", { name: /Configure Photon/ }));
    await user.clear(screen.getByLabelText("Photon project ID"));
    await user.type(screen.getByLabelText("Photon project ID"), "updated-project");
    await user.click(screen.getByRole("button", { name: "Save and provision gateways" }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("configure_photon_gateway", {
        projectId: "updated-project",
        projectSecret: null,
        allowedSenders: ["+15551234567"],
      }),
    );

    const harnesses = screen.getByRole("group", { name: "Visible harnesses" });
    await user.click(within(harnesses).getByRole("button", { name: "OpenCode" }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("set_harness_visible", {
        harness: "opencode",
        visible: false,
      }),
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
        expect(tauri.invoke).toHaveBeenCalledWith("launch_cli", { client }),
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
