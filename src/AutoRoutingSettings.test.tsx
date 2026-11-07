// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import { AutoRoutingSettings } from "./AutoRoutingSettings";
import { testFleet } from "./uiTestFixtures";
afterEach(cleanup);

it("saves shared approval and an independent empty deny-list without changing other clients", async () => {
  const save = vi.fn().mockResolvedValue(undefined);
  const user = userEvent.setup();
  render(<AutoRoutingSettings fleet={testFleet()} routing={{ models: [], clients: {} }} save={save} />);
  await user.selectOptions(screen.getByLabelText("Add approved model"), "m1-pro/m1-running");
  await user.click(screen.getByLabelText("Tools tested"));
  await user.selectOptions(screen.getByLabelText("Routing policy"), "hermes");
  await user.selectOptions(screen.getByLabelText("Routing mode"), "auto");
  await user.selectOptions(screen.getByLabelText("Auto minimum context"), "131072");
  await user.click(screen.getByRole("button", { name: "Save routing" }));
  expect(save).toHaveBeenLastCalledWith({ models: [{model: "m1-pro/m1-running", tools: true}], clients: {
    hermes: { mode: "auto", models: null, minimum_context: 131072 },
  }});
  await user.selectOptions(screen.getByLabelText("Routing policy"), "openbot");
  await user.selectOptions(screen.getByLabelText("Routing mode"), "auto");
  await user.click(screen.getByLabelText("Use shared approved models"));
  await user.click(screen.getByRole("button", { name: "Remove m1-pro/m1-running" }));
  await user.click(screen.getByRole("button", { name: "Save routing" }));
  expect(save.mock.lastCall?.[0].clients.hermes.mode).toBe("auto");
  expect(save.mock.lastCall?.[0].clients.openbot.models).toEqual([]);
});

it("reports a rejected save instead of claiming the policy changed", async () => {
  const save = vi.fn().mockRejectedValue(new Error("Unable to save settings"));
  render(<AutoRoutingSettings fleet={testFleet()} routing={{models:[],clients:{}}} save={save} />);
  await userEvent.click(screen.getByRole("button", {name:"Save routing"}));
  expect((await screen.findByRole("status")).textContent).toContain("Unable to save settings");
});
