import { describe, expect, it } from "vitest";
import main from "./main.tsx?raw";
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

    for (const label of ["main", "tray-menu", "model-menu", "notice"]) {
      expect(windows.get(label)?.visibleOnAllWorkspaces).toBe(true);
    }
  });
  it("makes floating windows truly transparent on macOS and keeps notices out of the way", () => {
    // Without the private API flag macOS ignores `transparent`, which paints the
    // rounded corners black.
    expect(tauriConfig.app.macOSPrivateApi).toBe(true);
    const windows = new Map(tauriConfig.app.windows.map((window) => [window.label, window]));
    for (const label of ["tray-menu", "model-menu", "notice"]) {
      const window = windows.get(label) as Record<string, unknown>;
      expect(window.transparent).toBe(true);
      expect(window.decorations).toBe(false);
      expect(window.skipTaskbar).toBe(true);
    }
    expect((windows.get("notice") as Record<string, unknown>).focus).toBe(false);
  });
  it("clears the root background in floating windows in both themes", () => {
    // :root paints the whole window; an opaque root shows as dark corners on macOS
    // even when the window and <body> are transparent.
    expect(main).toMatch(/if \(floating\) document\.documentElement\.style\.background = "transparent"/);
  });
});
