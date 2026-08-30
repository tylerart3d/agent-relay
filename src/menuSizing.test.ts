// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { measureMenuHeight } from "./menuSizing";

afterEach(() => vi.restoreAllMocks());

describe("measureMenuHeight", () => {
  it("includes shell borders and a device-scale rounding pixel", () => {
    const shell = document.createElement("main");
    Object.defineProperty(shell, "scrollHeight", { value: 318 });
    vi.spyOn(window, "getComputedStyle").mockReturnValue({
      borderTopWidth: "1px",
      borderBottomWidth: "1px",
    } as CSSStyleDeclaration);

    expect(measureMenuHeight(shell)).toBe(321);
  });
});
