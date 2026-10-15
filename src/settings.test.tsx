// @vitest-environment jsdom
import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { testSettings } from "./uiTestFixtures";
const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
import { useAppSettings } from "./settings";

afterEach(() => { cleanup(); vi.useRealTimers(); });

it("recovers saved theme and harness visibility after a startup IPC failure", async () => {
  vi.useFakeTimers();
  window.matchMedia = vi.fn().mockReturnValue({ matches: true, addEventListener: vi.fn(), removeEventListener: vi.fn() });
  mocks.invoke.mockRejectedValueOnce(new Error("managed state not ready"))
    .mockResolvedValue(structuredClone(testSettings));
  const { result } = renderHook(() => useAppSettings());
  await act(async () => {});
  expect(result.current.settingsLoadError).toContain("managed state not ready");
  await act(async () => { await vi.advanceTimersByTimeAsync(1000); });
  expect(result.current.settings?.harness_visibility).toEqual(testSettings.harness_visibility);
  expect(document.documentElement.dataset.resolvedTheme).toBe("dark");
  expect(result.current.settingsLoadError).toBeNull();
});

it("cancels retry when the window unmounts", async () => {
  vi.useFakeTimers();
  mocks.invoke.mockReset().mockRejectedValue(new Error("not ready"));
  const { unmount } = renderHook(() => useAppSettings());
  await act(async () => {});
  unmount();
  await vi.advanceTimersByTimeAsync(2000);
  expect(mocks.invoke).toHaveBeenCalledTimes(1);
});
