// @vitest-environment jsdom
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, expect, it, vi } from "vitest";
import { AutoRoutingSettings } from "./AutoRoutingSettings";
import type { RoutingConfig } from "./routing";
import { testFleet } from "./uiTestFixtures";
afterEach(cleanup);

// Mirrors useRouting: a successful save becomes the new saved routing.
function Harness({ initial, save }: { initial: RoutingConfig; save: (next: RoutingConfig) => Promise<void> }) {
  const [routing, setRouting] = useState(initial);
  return <AutoRoutingSettings fleet={testFleet()} routing={routing}
    save={async (next) => { await save(next); setRouting(next); }} />;
}

it("saves each change immediately without changing other clients", async () => {
  const save = vi.fn().mockResolvedValue(undefined);
  const user = userEvent.setup();
  render(<Harness initial={{ models: [], clients: {} }} save={save} />);
  expect(screen.queryByRole("button", { name: "Save routing" })).toBeNull();
  await user.selectOptions(screen.getByLabelText("Add approved model"), "m1-pro/m1-running");
  await user.click(screen.getByLabelText("Tools tested"));
  await user.selectOptions(screen.getByLabelText("Routing policy"), "hermes");
  await user.selectOptions(screen.getByLabelText("Routing mode"), "auto");
  await user.selectOptions(screen.getByLabelText("Auto minimum context"), "131072");
  expect(save).toHaveBeenLastCalledWith({ models: [{model: "m1-pro/m1-running", tools: true}], clients: {
    hermes: { mode: "auto", models: null, minimum_context: 131072 },
  }});
  await user.selectOptions(screen.getByLabelText("Routing policy"), "openbot");
  await user.selectOptions(screen.getByLabelText("Routing mode"), "auto");
  await user.click(screen.getByLabelText("Use shared approved models"));
  await user.click(screen.getByRole("button", { name: "Remove m1-pro/m1-running" }));
  expect(save.mock.lastCall?.[0].clients.hermes.mode).toBe("auto");
  expect(save.mock.lastCall?.[0].clients.openbot.models).toEqual([]);
  expect(save.mock.lastCall?.[0].models).toEqual([{model: "m1-pro/m1-running", tools: true}]);
});

it("switching a client to pinned persists without a separate save step", async () => {
  const save = vi.fn().mockResolvedValue(undefined);
  const user = userEvent.setup();
  const initial: RoutingConfig = { models: [], clients: { openbot: { mode: "auto", models: null, minimum_context: 65536 } } };
  render(<Harness initial={initial} save={save} />);
  await user.selectOptions(screen.getByLabelText("Routing policy"), "openbot");
  await user.selectOptions(screen.getByLabelText("Routing mode"), "pinned");
  expect(save).toHaveBeenCalledTimes(1);
  expect(save.mock.lastCall?.[0].clients.openbot.mode).toBe("pinned");
});

it("saves preferred order for one client only", async () => {
  const save = vi.fn().mockResolvedValue(undefined);
  const user = userEvent.setup();
  render(<Harness initial={{ models: [], clients: {} }} save={save} />);
  await user.selectOptions(screen.getByLabelText("Routing policy"), "openbot");
  await user.click(screen.getByLabelText("Prefer listed order"));
  expect(save.mock.lastCall?.[0].clients).toEqual({
    openbot: { mode: "pinned", models: null, minimum_context: 65536, prefer_order: true },
  });
});

it("limits one client to one request at a time and can clear the limit", async () => {
  const save = vi.fn().mockResolvedValue(undefined);
  const user = userEvent.setup();
  render(<Harness initial={{ models: [], clients: {} }} save={save} />);
  await user.selectOptions(screen.getByLabelText("Routing policy"), "openbot");
  await user.click(screen.getByLabelText("One request at a time"));
  expect(save.mock.lastCall?.[0].clients.openbot.max_concurrent).toBe(1);
  await user.click(screen.getByLabelText("One request at a time"));
  expect(save.mock.lastCall?.[0].clients.openbot.max_concurrent).toBeNull();
});

it("builds each change from the latest saved routing, not a stale copy", async () => {
  const save = vi.fn().mockResolvedValue(undefined);
  const user = userEvent.setup();
  const { rerender } = render(<AutoRoutingSettings fleet={testFleet()} routing={{ models: [], clients: {} }} save={save} />);
  // Another window pins OpenBot; the open panel then edits Hermes.
  rerender(<AutoRoutingSettings fleet={testFleet()} save={save}
    routing={{ models: [], clients: { openbot: { mode: "pinned", models: null, minimum_context: 65536 } } }} />);
  await user.selectOptions(screen.getByLabelText("Routing policy"), "hermes");
  await user.selectOptions(screen.getByLabelText("Routing mode"), "auto");
  expect(save.mock.lastCall?.[0].clients.openbot.mode).toBe("pinned");
});

it("reports a rejected save instead of claiming the policy changed", async () => {
  const save = vi.fn().mockRejectedValue(new Error("Unable to save settings"));
  const user = userEvent.setup();
  render(<AutoRoutingSettings fleet={testFleet()} routing={{models:[],clients:{}}} save={save} />);
  await user.selectOptions(screen.getByLabelText("Routing policy"), "openbot");
  await user.selectOptions(screen.getByLabelText("Routing mode"), "auto");
  expect((await screen.findByRole("status")).textContent).toContain("Unable to save settings");
});

it("makes reclamation opt-in and edits only local profiles without changing client routes", async () => {
  const save = vi.fn().mockResolvedValue(undefined);
  const user = userEvent.setup();
  render(<Harness initial={{models: [], clients: {}, reclaimable_models: ["m1-pro/old-background"]}} save={save} />);
  expect(screen.queryByLabelText("Allow reclaim workstation/workstation-1")).toBeNull();
  const checks = screen.getAllByRole("checkbox", {name: /^Allow reclaim/});
  expect(checks.length).toBeGreaterThan(0);
  await user.click(checks[0]);
  expect(save.mock.lastCall?.[0].reclaimable_models).toHaveLength(2);
  expect(save.mock.lastCall?.[0].clients).toEqual({});
  await waitFor(() => expect((checks[0] as HTMLInputElement).disabled).toBe(false));
  await user.click(checks[0]);
  expect(save.mock.lastCall?.[0].reclaimable_models).toEqual(["m1-pro/old-background"]);
});
