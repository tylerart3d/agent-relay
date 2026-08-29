// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
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
    handoff_from_session_id: 1,
    handoff_status: "pending",
    updated_at_ms: 2,
  },
  {
    channel: "photon",
    account_id: "personal",
    conversation_id: "chat-1",
    session_id: 1,
    conversation_label: "Product brainstorm",
    harness: "hermes",
    harness_host_id: "m1-pro",
    host_id: "m1-pro",
    model_id: "m1-running",
    archived_at_ms: 2,
    updated_at_ms: 1,
  },
];

const tauri = vi.hoisted(() => ({
  invoke: vi.fn(),
  listeners: new Map<string, (event: { payload: unknown }) => void>(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: tauri.invoke }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string, handler: (event: { payload: unknown }) => void) => {
    tauri.listeners.set(event, handler);
    return () => {
      if (tauri.listeners.get(event) === handler) tauri.listeners.delete(event);
    };
  }),
}));

import { ModelMenu } from "./ModelMenu";

function openModelMenu(hostId: string, requestId: number) {
  const listener = tauri.listeners.get("model-menu-opened");
  if (!listener) throw new Error("model-menu-opened listener was not registered");
  act(() => listener({ payload: { host_id: hostId, request_id: requestId } }));
}

beforeEach(() => {
  const fleet = testFleet();
  tauri.invoke.mockReset().mockImplementation(async (command: string, args?: unknown) => {
    if (command === "get_app_settings") return structuredClone(testSettings);
    if (command === "get_fleet_snapshot") return fleet;
    if (command === "get_channel_routes") return structuredClone(testChannelRoutes);
    if (command === "get_opencode_sessions") {
      return [{
        id: "ses_game",
        title: "Continue the tower-defense game",
        project_id: "tower-defense",
        project_name: "Tower Defense",
        directory: "P:/projects-code/Tower Defense",
        updated_at_ms: 10,
        archived: false,
      }];
    }
    if (command === "execute_channel_command") {
      return { ok: true, handled: true, http_status: 200, message: "updated" };
    }
    if (command === "set_client_context_window") {
      const { client, contextWindow } = args as { client: "hermes" | "opencode"; contextWindow: number };
      return {
        ...structuredClone(testSettings),
        [`${client}_context_window`]: contextWindow,
      };
    }
    if (["load_model", "unload_host", "restart_local_llama_swap", "stop_local_llama_swap"].includes(command)) {
      return {
        state: "applied",
        host_id: "m1-pro",
        active_requests: 0,
        loaded_model_id: null,
        message: "applied",
      };
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
  Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
    configurable: true,
    get() {
      return 160 + this.querySelectorAll("button").length * 36;
    },
  });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("ModelMenu controls", () => {
  it("edits a messaging route through the shared channel command transaction", async () => {
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("channels", 5);

    await user.click(await screen.findByRole("menuitem", { name: /Product brainstorm/ }));
    expect(screen.getByText(/Context transfer pending/)).toBeTruthy();
    await user.selectOptions(screen.getByLabelText("Harness"), "open_code");
    await user.selectOptions(screen.getByLabelText("Harness machine"), "workstation");
    await user.selectOptions(
      await screen.findByLabelText("OpenCode project"),
      "P:/projects-code/Tower Defense",
    );
    await user.selectOptions(
      await screen.findByLabelText("OpenCode conversation"),
      "ses_game",
    );
    await user.selectOptions(screen.getByLabelText("Model"), "workstation/workstation-2");
    expect((screen.getByLabelText("Project") as HTMLInputElement).value).toBe(
      "P:/projects-code/Tower Defense",
    );
    expect(screen.getByLabelText("Current message route").textContent).toContain(
      "HermesM1 Pro→M1 RunningM1 ProRunning",
    );
    expect(screen.getByLabelText("Proposed message route").textContent).toContain(
      "OpenCode on WORKSTATION",
    );
    await user.click(screen.getByRole("button", { name: "Move with context" }));

    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("execute_channel_command", {
        channel: "photon",
        accountId: "personal",
        conversationId: "chat-1",
        conversationLabel: "Product brainstorm",
        text: "!ar move opencode@workstation workstation/workstation-2 project \"P:/projects-code/Tower Defense\" session \"ses_game\"",
      }),
    );
    expect(tauri.invoke).toHaveBeenCalledWith("get_opencode_sessions", {
      hostId: "workstation",
    });
    expect(screen.getByRole("button", { name: /Session #1/ })).toBeTruthy();
  });

  it.each([
    ["Update current", "!ar use hermes@m1-pro m1-pro/m1-running"],
    ["Start without context", "!ar new hermes@m1-pro m1-pro/m1-running"],
  ])("maps %s to the matching channel command", async (button, text) => {
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("channels", 6);

    await user.click(await screen.findByRole("menuitem", { name: /Product brainstorm/ }));
    await user.click(screen.getByRole("button", { name: button }));

    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("execute_channel_command", {
        channel: "photon",
        accountId: "personal",
        conversationId: "chat-1",
        conversationLabel: "Product brainstorm",
        text,
      }),
    );
  });

  it("resumes an archived messaging session from the route editor", async () => {
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("channels", 7);

    await user.click(await screen.findByRole("menuitem", { name: /Product brainstorm/ }));
    await user.click(screen.getByRole("button", { name: /Session #1/ }));

    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("execute_channel_command", {
        channel: "photon",
        accountId: "personal",
        conversationId: "chat-1",
        conversationLabel: "Product brainstorm",
        text: "!ar resume 1",
      }),
    );
  });

  it("asks before forcing a busy messaging route change", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    let attempts = 0;
    tauri.invoke.mockImplementation(async (command: string) => {
      if (command === "get_app_settings") return structuredClone(testSettings);
      if (command === "get_fleet_snapshot") return testFleet();
      if (command === "get_channel_routes") return structuredClone(testChannelRoutes);
      if (command === "execute_channel_command") {
        attempts += 1;
        if (attempts === 1) {
          return {
            ok: false,
            handled: true,
            http_status: 409,
            message: "one request is active",
            confirmation_required: true,
            retry_command: "!ar use hermes@m1-pro m1-pro/m1-running force",
          };
        }
        return { ok: true, handled: true, http_status: 200, message: "updated" };
      }
      return undefined;
    });
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("channels", 8);

    await user.click(await screen.findByRole("menuitem", { name: /Product brainstorm/ }));
    await user.click(screen.getByRole("button", { name: "Update current" }));

    await waitFor(() => expect(attempts).toBe(2));
    expect(confirm).toHaveBeenCalledWith(expect.stringContaining("Cancel the active request"));
    expect(tauri.invoke).toHaveBeenCalledWith(
      "execute_channel_command",
      expect.objectContaining({
        text: "!ar use hermes@m1-pro m1-pro/m1-running force",
      }),
    );
  });

  it("renders the complete catalog when switching from a short host to WORKSTATION", async () => {
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));

    openModelMenu("air-m4", 1);
    await screen.findByText("Air Idle");
    openModelMenu("workstation", 2);

    await screen.findByText("Workstation Model 9");
    expect(screen.getAllByRole("menuitem")).toHaveLength(9);
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith(
        "resize_model_menu",
        expect.objectContaining({ requestId: 2 }),
      ),
    );
  });

  it("routes every connector selection to its matching backend command", async () => {
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    const commands: Array<[string, string]> = [
      ["hermes", "connect_hermes_model"],
      ["hermes_cli", "connect_hermes_cli_model"],
      ["opencode", "connect_opencode_model"],
      ["opencode_cli", "connect_opencode_cli_model"],
      ["codex", "connect_codex_model"],
      ["claude_code", "connect_claude_code_model"],
      ["pi", "connect_pi_model"],
      ["copilot", "connect_copilot_model"],
      ["vscode", "connect_vscode_model"],
    ];

    for (const [index, [client, command]] of commands.entries()) {
      openModelMenu(`connector:${client}`, index + 10);
      const choice = await screen.findByRole("menuitem", { name: /M1 Running on M1 Pro/ });
      await user.click(choice);
      await waitFor(() =>
        expect(tauri.invoke).toHaveBeenCalledWith(command, {
          hostId: "m1-pro",
          modelId: "m1-running",
        }),
      );
    }
  });

  it("switches the OpenCode route without restarting the desktop app", async () => {
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("connector:opencode", 25);

    await user.click(await screen.findByRole("menuitem", { name: /M1 Running on M1 Pro/ }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("connect_opencode_model", {
        hostId: "m1-pro",
        modelId: "m1-running",
      }),
    );
    expect(tauri.invoke).not.toHaveBeenCalledWith(
      "relaunch_opencode_desktop",
      expect.anything(),
    );
    expect(tauri.invoke).toHaveBeenCalledWith("hide_tray_menus");
  });

  it("loads, unloads, and controls the local service through explicit commands", async () => {
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));

    openModelMenu("workstation", 30);
    await user.click(await screen.findByRole("menuitem", { name: "Workstation Model 2" }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("load_model", {
        hostId: "workstation",
        modelId: "workstation-2",
        force: false,
      }),
    );

    openModelMenu("m1-pro", 31);
    await user.click(await screen.findByRole("button", { name: "Unload model" }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("unload_host", {
        hostId: "m1-pro",
        force: false,
      }),
    );
    await user.click(screen.getByRole("button", { name: "Restart service" }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("restart_local_llama_swap", { force: false }),
    );
    await user.click(screen.getByRole("button", { name: "Stop service" }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("stop_local_llama_swap", { force: false }),
    );
  });

  it("shows a timed-out host as idle and leaves its model available to reload", async () => {
    const fleet = testFleet();
    const m1 = fleet.hosts.find((host) => host.id === "m1-pro")!;
    m1.loaded_model_id = null;
    tauri.invoke.mockImplementation(async (command: string) => {
      if (command === "get_app_settings") return structuredClone(testSettings);
      if (command === "get_fleet_snapshot") return structuredClone(fleet);
      if (command === "load_model") {
        return {
          state: "applied",
          host_id: "m1-pro",
          active_requests: 0,
          loaded_model_id: "m1-running",
          message: "loaded m1-running",
        };
      }
      return undefined;
    });

    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("m1-pro", 35);

    await screen.findByText("Idle");
    const model = await screen.findByRole("menuitem", { name: "M1 Running" });
    expect((model as HTMLButtonElement).disabled).toBe(false);
    expect(screen.queryByText("Running")).toBeNull();
    await user.click(model);
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("load_model", {
        hostId: "m1-pro",
        modelId: "m1-running",
        force: false,
      }),
    );
  });

  it("persists the OpenCode context slider", async () => {
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("connector:opencode", 40);

    const slider = await screen.findByRole("slider", { name: /Context window/ });
    fireEvent.change(slider, { target: { value: "81920" } });
    fireEvent.blur(slider);
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("set_client_context_window", {
        client: "opencode",
        contextWindow: 81_920,
      }),
    );
  });

  it("requires an explicit force confirmation before replacing an active model", async () => {
    const fleet = testFleet();
    tauri.invoke.mockImplementation(async (command: string, args?: unknown) => {
      if (command === "get_app_settings") return structuredClone(testSettings);
      if (command === "get_fleet_snapshot") return fleet;
      if (command === "load_model") {
        const { force } = args as { force: boolean };
        return {
          state: force ? "applied" : "conflict",
          host_id: "workstation",
          active_requests: 2,
          loaded_model_id: "workstation-1",
          message: "busy",
        };
      }
      return undefined;
    });
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("workstation", 50);

    await user.click(await screen.findByRole("menuitem", { name: "Workstation Model 2" }));
    const confirmation = await screen.findByRole("region", { name: "Confirm model action" });
    expect(confirmation.textContent).toContain("2 active request(s)");
    expect(tauri.invoke).not.toHaveBeenCalledWith(
      "load_model",
      expect.objectContaining({ force: true }),
    );
    await user.click(screen.getByRole("button", { name: "Force switch" }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("load_model", {
        hostId: "workstation",
        modelId: "workstation-2",
        force: true,
      }),
    );
  });

  it.each([
    ["Unload model", "unload_host", "Force unload", { hostId: "m1-pro", force: true }],
    ["Restart service", "restart_local_llama_swap", "Force restart", { force: true }],
    ["Stop service", "stop_local_llama_swap", "Force stop", { force: true }],
  ])("requires confirmation before %s cancels active requests", async (button, command, forceLabel, forceArgs) => {
    const fleet = testFleet();
    tauri.invoke.mockImplementation(async (invokedCommand: string, args?: unknown) => {
      if (invokedCommand === "get_app_settings") return structuredClone(testSettings);
      if (invokedCommand === "get_fleet_snapshot") return fleet;
      if (invokedCommand === command) {
        const { force } = args as { force: boolean };
        return {
          state: force ? "applied" : "conflict",
          host_id: "m1-pro",
          active_requests: 1,
          loaded_model_id: "m1-running",
          message: "busy",
        };
      }
      return undefined;
    });
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("m1-pro", 60);

    await user.click(await screen.findByRole("button", { name: button }));
    expect(await screen.findByRole("region", { name: "Confirm model action" })).toBeTruthy();
    await user.click(screen.getByRole("button", { name: forceLabel }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith(command, forceArgs),
    );
  });

  it("cancels a pending destructive action and closes from Escape", async () => {
    const fleet = testFleet();
    tauri.invoke.mockImplementation(async (command: string) => {
      if (command === "get_app_settings") return structuredClone(testSettings);
      if (command === "get_fleet_snapshot") return fleet;
      if (command === "unload_host") {
        return {
          state: "conflict",
          host_id: "m1-pro",
          active_requests: 1,
          loaded_model_id: "m1-running",
          message: "busy",
        };
      }
      return undefined;
    });
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("m1-pro", 70);

    await user.click(await screen.findByRole("button", { name: "Unload model" }));
    await user.click(await screen.findByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("region", { name: "Confirm model action" })).toBeNull();
    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("hide_tray_menus"));
  });
});
