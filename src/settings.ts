import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import type { InferenceOverrides } from "./fleet";

export type ThemePreference = "light" | "dark" | "system";

export type HarnessId =
  | "opencode"
  | "opencode_cli"
  | "codex"
  | "claude_code"
  | "copilot"
  | "vscode"
  | "pi"
  | "hermes"
  | "hermes_cli";

export type HarnessVisibility = Record<HarnessId, boolean>;

export type HarnessSetupState =
  | "not_installed"
  | "detected"
  | "configured"
  | "needs_repair";

export interface HarnessSetupStatus {
  id: HarnessId;
  label: string;
  state: HarnessSetupState;
  config_path?: string | null;
  selected_model?: string | null;
  error?: string | null;
}

export interface AppSettings {
  theme: ThemePreference;
  harness_visibility: HarnessVisibility;
  run_on_startup: boolean;
  hermes_context_window: number;
  opencode_context_window: number;
  channel_gateway: {
    primary_host_id: string | null;
    secondary_host_id: string | null;
    automatic_failover: boolean;
    failover_after_seconds: number;
    photon_project_id: string | null;
    allowed_senders: string[];
  };
  photon_credentials_configured: boolean;
  inference_overrides: Record<string, InferenceOverrides>;
}

export function applyTheme(theme: ThemePreference) {
  const resolved =
    theme === "system"
      ? window.matchMedia("(prefers-color-scheme: light)").matches
        ? "light"
        : "dark"
      : theme;
  document.documentElement.dataset.theme = theme;
  document.documentElement.dataset.resolvedTheme = resolved;
  document.documentElement.style.colorScheme = resolved;
}

export function useAppSettings() {
  const [settings, setSettings] = useState<AppSettings | null>(null);

  const refreshSettings = useCallback(async () => {
    const next = await invoke<AppSettings>("get_app_settings");
    applyTheme(next.theme);
    setSettings(next);
    return next;
  }, []);

  useEffect(() => {
    void refreshSettings().catch(() => applyTheme("system"));
    const colorScheme = window.matchMedia("(prefers-color-scheme: light)");
    const followSystem = () => {
      if (document.documentElement.dataset.theme === "system") applyTheme("system");
    };
    colorScheme.addEventListener("change", followSystem);
    let disposed = false;
    let unlisten: (() => void) | undefined;
    listen<ThemePreference>("theme-changed", ({ payload }) => {
      applyTheme(payload);
      setSettings((current) =>
        current ? { ...current, theme: payload } : current,
      );
    }).then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    });
    return () => {
      disposed = true;
      unlisten?.();
      colorScheme.removeEventListener("change", followSystem);
    };
  }, [refreshSettings]);

  return { settings, setSettings, refreshSettings };
}
