// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { testFleet, testSettings, testModel } from "./uiTestFixtures";


const tauri = vi.hoisted(() => ({
  invoke: vi.fn(),
  listeners: new Map<string, (event: { payload: unknown }) => void>(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: tauri.invoke }));
vi.mock("@tauri-apps/api/event", () => ({
  emit: vi.fn(async () => undefined),
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

it("opens a voice conversation even when its model is already loaded", async () => {
  const fleet = testFleet();
  const host = fleet.hosts.find((item) => item.id === "m1-pro")!;
  host.models = [{ ...testModel("voice", "VoiceChat"), kind: "voice", capabilities: ["realtime_audio"] }];
  host.loaded_model_id = "voice";
  const previous = tauri.invoke.getMockImplementation()!;
  tauri.invoke.mockImplementation(async (command, args) => command === "get_fleet_snapshot" ? fleet : previous(command, args));
  render(<ModelMenu />);
  await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
  openModelMenu("m1-pro", 1);
  await screen.findByText("Open voice conversation");
  fireEvent.click(screen.getByRole("menuitem", { name: /VoiceChat/ }));
  await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("open_voice_session", { hostId: "m1-pro", modelId: "voice" }));
  expect(tauri.invoke.mock.calls.some(([command]) => command === "load_model")).toBe(false);
});

beforeEach(() => {
  const fleet = testFleet();
  tauri.invoke.mockReset().mockImplementation(async (command: string) => {
    if (command === "get_routing_config") return { models: [], clients: {} };
    if (command === "get_app_settings") return structuredClone(testSettings);
    if (command === "get_fleet_snapshot") return fleet;
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
    if (["load_model", "unload_host", "restart_local_llama_swap"].includes(command)) {
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
      ["openbot", "connect_openbot_model"],
      ["claude_code", "connect_claude_code_model"],
      ["pi", "connect_pi_model"],
      ["copilot", "connect_copilot_model"],
      ["vscode", "connect_vscode_model"],
    ];

    for (const [index, [client, command]] of commands.entries()) {
      openModelMenu(`connector:${client}`, index + 10);
      const choice = await screen.findByRole("menuitem", { name: /M1 Running on M1 Pro/ });
      await user.click(choice);
      // Launch menus only select; their Launch button applies the choice.
      const launch = screen.queryByRole("button", { name: /^Launch / });
      if (launch) {
        await waitFor(() => expect((launch as HTMLButtonElement).disabled).toBe(false));
        await user.click(launch);
      }
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
        force: true,
      }),
    );
    await user.click(screen.getByRole("button", { name: "Restart service" }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("restart_local_llama_swap", { force: false }),
    );
    // Stop service was redundant with Off + Unload and is gone from the menu.
    expect(screen.queryByRole("button", { name: "Stop service" })).toBeNull();
  });

  it("unloads immediately, cancelling running requests, without a confirmation step", async () => {
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("m1-pro", 32);
    await user.click(await screen.findByRole("button", { name: "Unload model" }));
    await waitFor(() =>
      expect(tauri.invoke).toHaveBeenCalledWith("unload_host", { hostId: "m1-pro", force: true }),
    );
    expect(tauri.invoke).not.toHaveBeenCalledWith("unload_host", { hostId: "m1-pro", force: false });
    expect(screen.queryByRole("region", { name: "Confirm model action" })).toBeNull();
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

  it("selects a model, project, and thinking level, then loads, pins, and launches", async () => {
    const fleet = testFleet();
    fleet.codex.selected_model = "m1-pro/m1-running";
    tauri.invoke.mockImplementation(async (command: string) => {
      if (command === "get_app_settings") return structuredClone(testSettings);
      if (command === "get_fleet_snapshot") return structuredClone(fleet);
      if (command === "get_routing_config") return { models: [], clients: { codex: { mode: "auto", models: null, minimum_context: 65536 } } };
      if (command === "load_model") {
        return { state: "applied", host_id: "air-m4", active_requests: 0, loaded_model_id: "air-idle", message: "loaded" };
      }
      return undefined;
    });
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("connector:codex", 40);

    // Every compatible model is offered, not only running ones.
    await user.click(await screen.findByRole("menuitem", { name: "Air Idle on Air-M4" }));
    await screen.findByRole("menuitem", { name: "Air Idle on Air-M4, selected" });
    await user.selectOptions(screen.getByLabelText("Thinking effort"), "medium");
    // Choosing does not load or launch anything.
    expect(tauri.invoke).not.toHaveBeenCalledWith("load_model", expect.anything());
    expect(tauri.invoke).not.toHaveBeenCalledWith("set_routing_config", expect.anything());
    await user.click(screen.getByRole("button", { name: "Launch Codex" }));
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("connect_codex_model", {
      hostId: "air-m4",
      modelId: "air-idle",
    }));
    const order = tauri.invoke.mock.calls.map(([command]) => command);
    expect(order.indexOf("load_model")).toBeLessThan(order.indexOf("set_routing_config"));
    expect(order.indexOf("set_routing_config")).toBeLessThan(order.indexOf("connect_codex_model"));
    expect(tauri.invoke).toHaveBeenCalledWith("load_model", { hostId: "air-m4", modelId: "air-idle", force: false });
    expect(tauri.invoke).toHaveBeenCalledWith("set_routing_config", {
      routing: {
        models: [],
        clients: { codex: { mode: "pinned", models: null, minimum_context: 65536, reasoning_effort: "medium" } },
      },
    });
  });

  it("does not cancel another host's work to load a model", async () => {
    const fleet = testFleet();
    tauri.invoke.mockImplementation(async (command: string) => {
      if (command === "get_app_settings") return structuredClone(testSettings);
      if (command === "get_routing_config") return { models: [], clients: {} };
      if (command === "get_fleet_snapshot") return structuredClone(fleet);
      if (command === "load_model") {
        return { state: "conflict", host_id: "air-m4", active_requests: 2, loaded_model_id: null, message: "busy" };
      }
      return undefined;
    });
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("connector:pi", 41);
    await user.click(await screen.findByRole("menuitem", { name: "Air Idle on Air-M4" }));
    const launch = screen.getByRole("button", { name: "Launch Pi" });
    await waitFor(() => expect((launch as HTMLButtonElement).disabled).toBe(false));
    await user.click(launch);
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("load_model", expect.objectContaining({ force: false })));
    expect(tauri.invoke).not.toHaveBeenCalledWith("load_model", expect.objectContaining({ force: true }));
    expect(tauri.invoke).not.toHaveBeenCalledWith("connect_pi_model", expect.anything());
    expect(tauri.invoke).not.toHaveBeenCalledWith("hide_tray_menus");
  });

  it("launches Auto in the project folder chosen in the launch menu", async () => {
    const previous = tauri.invoke.getMockImplementation()!;
    let picked: string | null = "/projects/My Game";
    tauri.invoke.mockImplementation(async (command: string, args?: unknown) => {
      if (command === "get_routing_config") return { models: [], clients: {} };
      if (command === "pick_cli_project_folder") return picked;
      return previous(command, args);
    });
    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu("connector:hermes_cli", 42);

    await user.click(await screen.findByLabelText("Choose project folder for Hermes CLI"));
    await waitFor(() =>
      expect((screen.getByLabelText("Project folder for Hermes CLI") as HTMLSelectElement).value).toBe(picked));
    picked = null;
    await user.click(screen.getByLabelText("Choose project folder for Hermes CLI"));
    const auto = await screen.findByRole("menuitem", { name: "Auto" });
    await waitFor(() => expect((auto as HTMLButtonElement).disabled).toBe(false));
    await user.click(auto);
    expect(tauri.invoke).not.toHaveBeenCalledWith("launch_cli", expect.anything());
    await user.click(screen.getByRole("button", { name: "Launch Hermes CLI" }));
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("launch_cli", {
      client: "hermes",
      directory: "/projects/My Game",
    }));
    expect(tauri.invoke).toHaveBeenCalledWith("set_routing_config", {
      routing: {
        models: [],
        clients: { hermes_cli: { mode: "auto", models: null, minimum_context: 65536, reasoning_effort: null } },
      },
    });
  });

  it.each([
    ["hermes", "running"], ["hermes", "idle"], ["hermes", "offline"],
    ["opencode", "running"], ["opencode", "idle"], ["opencode", "offline"],
  ] as const)("saves the %s harness thinking level on change when the host is %s", async (connector, state) => {
    const fleet = testFleet();
    const model = fleet.hosts[0].models[0];
    model.inference_controls = {
      thinking: {
        adapter: "llama_cpp",
        efforts: ["off", "low", "medium", "xhigh"],
        default_effort: "low",
        budget_min: -1,
        budget_max: 16_384,
        budget_step: 256,
        default_budget: -1,
      },
      temperature: { default: 0.3 },
    };
    fleet[connector].selected_model = "m1-pro/m1-running";
    if (state === "idle") fleet.hosts[0].loaded_model_id = null;
    if (state === "offline") fleet.hosts[0].connection = "offline";
    tauri.invoke.mockImplementation(async (command: string) => {
      if (command === "get_app_settings") return structuredClone(testSettings);
      if (command === "get_fleet_snapshot") return structuredClone(fleet);
      if (command === "get_routing_config") return { models: [], clients: {} };
      return undefined;
    });

    const user = userEvent.setup();
    render(<ModelMenu />);
    await waitFor(() => expect(tauri.listeners.has("model-menu-opened")).toBe(true));
    openModelMenu(`connector:${connector}`, 43);

    // The limit and temperature are shown, fixed by the model; there is no slider.
    expect(await screen.findByText(/Low · 2,048 reasoning tokens · temperature 0.3/)).toBeTruthy();
    expect(screen.queryByRole("slider")).toBeNull();
    expect(screen.queryByLabelText("Reasoning token limit")).toBeNull();

    // The menu offers the harness levels; High maps to this model's xhigh.
    const select = await screen.findByLabelText("Thinking effort");
    await waitFor(() => expect((select as HTMLSelectElement).disabled).toBe(false));
    await user.selectOptions(select, "high");
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("set_routing_config", {
      routing: {
        models: [],
        clients: {
          [connector]: { mode: "pinned", models: null, minimum_context: 65536, reasoning_effort: "high" },
        },
      },
    }));
    expect(await screen.findByText(/Extra high · unlimited reasoning/)).toBeTruthy();
    expect(tauri.invoke).not.toHaveBeenCalledWith("load_model", expect.anything());
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
    ["Restart service", "restart_local_llama_swap", "Force restart", { force: true }],
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
      if (command === "restart_local_llama_swap") {
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

    await user.click(await screen.findByRole("button", { name: "Restart service" }));
    await user.click(await screen.findByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("region", { name: "Confirm model action" })).toBeNull();
    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(tauri.invoke).toHaveBeenCalledWith("hide_tray_menus"));
  });
});
