import { describe, expect, it } from "vitest";
import tauriConfig from "../src-tauri/tauri.conf.json";

describe("desktop window configuration", () => {
  it("stages the native CLI and packages only the external model supervisor", () => {
    // Tauri discovers agentrelayctl as a Cargo binary; declaring it again as an
    // external sidecar would insert duplicate files into the installer.
    expect(tauriConfig.bundle.externalBin).toEqual(["binaries/llama-swap"]);
    expect(tauriConfig.build.beforeBuildCommand).toBe("npm run cli:stage && npm run build");
  });
  it("shows every Agent Relay window on the active macOS Space", () => {
    const windows = new Map(
      tauriConfig.app.windows.map((window) => [window.label, window]),
    );

    for (const label of ["main", "tray-menu", "model-menu"]) {
      expect(windows.get(label)?.visibleOnAllWorkspaces).toBe(true);
    }
  });
});
