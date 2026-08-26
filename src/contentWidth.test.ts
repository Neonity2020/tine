// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import {
  MAX_CONTENT_WIDTH,
  MIN_CONTENT_WIDTH,
  applyContentWidths,
  changeStandardContentWidth,
  changeWideContentWidth,
  normalizeContentWidth,
  resetStandardContentWidth,
} from "./contentWidth";

afterEach(() => {
  resetStandardContentWidth();
  changeWideContentWidth(null);
  localStorage.clear();
  document.documentElement.style.removeProperty("--tine-main-content-max-width");
  document.documentElement.style.removeProperty("--tine-wide-content-max-width");
});

describe("device-local content widths", () => {
  it("clamps explicit widths and persists the values", () => {
    changeStandardContentWidth(42);
    changeWideContentWidth(100_000);

    expect(localStorage.getItem("logseq-claude.standard-content-width")).toBe(String(MIN_CONTENT_WIDTH));
    expect(localStorage.getItem("logseq-claude.wide-content-width")).toBe(String(MAX_CONTENT_WIDTH));
    expect(document.documentElement.style.getPropertyValue("--tine-main-content-max-width")).toBe(`${MIN_CONTENT_WIDTH}px`);
    expect(document.documentElement.style.getPropertyValue("--tine-wide-content-max-width")).toBe(`${MAX_CONTENT_WIDTH}px`);
  });

  it("uses the theme and fill-pane defaults after reset", () => {
    changeStandardContentWidth(960);
    changeWideContentWidth(1440);
    resetStandardContentWidth();
    changeWideContentWidth(null);
    applyContentWidths();

    expect(localStorage.getItem("logseq-claude.standard-content-width")).toBeNull();
    expect(localStorage.getItem("logseq-claude.wide-content-width")).toBeNull();
    expect(document.documentElement.style.getPropertyValue("--tine-main-content-max-width")).toBe("");
    expect(document.documentElement.style.getPropertyValue("--tine-wide-content-max-width")).toBe("");
  });

  it("rounds finite values and rejects non-finite input at the public boundary", () => {
    expect(normalizeContentWidth(915.6)).toBe(916);
    changeStandardContentWidth(960);
    changeStandardContentWidth(Number.NaN);
    expect(localStorage.getItem("logseq-claude.standard-content-width")).toBe("960");
  });
});
