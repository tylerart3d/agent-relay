import { describe, expect, it } from "vitest";
import tauriConfig from "../src-tauri/tauri.conf.json";

describe("desktop window configuration", () => {
  it("shows every Agent Relay window on the active macOS Space", () => {
    const windows = new Map(
      tauriConfig.app.windows.map((window) => [window.label, window]),
    );

    for (const label of ["main", "tray-menu", "model-menu"]) {
      expect(windows.get(label)?.visibleOnAllWorkspaces).toBe(true);
    }
  });
});
