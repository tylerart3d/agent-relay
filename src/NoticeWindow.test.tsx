// @vitest-environment jsdom
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

const tauri = vi.hoisted(() => ({
  invoke: vi.fn(),
  listeners: new Map<string, () => void>(),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: tauri.invoke }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string, handler: () => void) => {
    tauri.listeners.set(event, handler);
    return () => tauri.listeners.delete(event);
  }),
}));

import NoticeWindow from "./NoticeWindow";

let notices = [
  { key: "tray.launch", title: "Launch", message: "OpenCode exited", at_ms: 1 },
  { key: "fleet-status", title: "Fleet status", message: "peer timed out", at_ms: 2 },
];

beforeEach(() => {
  tauri.invoke.mockImplementation(async (command: string, args?: { key?: string | null }) => {
    if (command === "get_notices") return notices;
    if (command === "dismiss_notice") {
      notices = args?.key ? notices.filter((n) => n.key !== args.key) : [];
    }
    return undefined;
  });
});
afterEach(() => { cleanup(); tauri.invoke.mockReset(); tauri.listeners.clear(); });

it("lists each notice and dismisses one at a time", async () => {
  const user = userEvent.setup();
  render(<NoticeWindow />);
  expect(await screen.findByText("OpenCode exited")).toBeTruthy();
  expect(screen.getAllByRole("alert")).toHaveLength(2);
  await user.click(screen.getByRole("button", { name: "Dismiss Launch" }));
  expect(tauri.invoke).toHaveBeenCalledWith("dismiss_notice", { key: "tray.launch" });
  await act(async () => { tauri.listeners.get("notices-changed")?.(); });
  await waitFor(() => expect(screen.queryByText("OpenCode exited")).toBeNull());
  // "Dismiss all" appears only when more than one notice is open.
  expect(screen.queryByRole("button", { name: "Dismiss all" })).toBeNull();
});
