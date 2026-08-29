import { describe, expect, it } from "vitest";
import {
  dispatchTrayAction,
  getModelOptionState,
  layoutOffsetTop,
} from "./modelMenuState";

describe("getModelOptionState", () => {
  it("keeps a loaded model selectable so it can retarget Hermes", () => {
    expect(
      getModelOptionState(
        true,
        "workstation",
        "bonsai",
        "bonsai",
        "m1-pro/ornith",
      ),
    ).toEqual({
      loaded: true,
      cachedLoaded: false,
      selectedInHermes: false,
      disabled: false,
    });
  });

  it("tracks the Hermes target independently from the running model", () => {
    expect(
      getModelOptionState(
        true,
        "workstation",
        "bonsai",
        "ornith",
        "workstation/bonsai",
      ),
    ).toEqual({
      loaded: false,
      cachedLoaded: false,
      selectedInHermes: true,
      disabled: false,
    });
  });

  it("disables every model on an offline host", () => {
    expect(getModelOptionState(false, "air-m4", "qwen", "qwen", null)).toEqual({
      loaded: false,
      cachedLoaded: true,
      selectedInHermes: false,
      disabled: true,
    });
  });

  it("measures an anchor from layout offsets instead of transformed bounds", () => {
    const root = { offsetTop: 4, offsetParent: null };
    const scrollPane = { parentElement: null, scrollTop: 20 };
    const section = { offsetTop: 42, offsetParent: root, parentElement: scrollPane };
    const button = { offsetTop: 7, offsetParent: section, parentElement: section };

    expect(layoutOffsetTop(button)).toBe(33);
  });

  it("emits the action before hiding the submenu webview", async () => {
    const calls: string[] = [];

    await dispatchTrayAction(
      "load_model::workstation::qwen",
      async (action) => {
        calls.push(`emit:${action}`);
      },
      async () => {
        calls.push("hide");
      },
    );

    expect(calls).toEqual(["emit:load_model::workstation::qwen", "hide"]);
  });
});
