// @vitest-environment jsdom
// GH #193: an explicit "System" appearance choice alongside manual Light/Dark.
// When System is selected, Tine follows the WebView/platform color-scheme signal
// (prefers-color-scheme), applies it immediately, and follows live changes;
// Manual selections ignore later system changes. The choice persists through the
// existing localStorage theme owner.
import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";

const native = vi.hoisted(() => ({
  setSystemBarAppearance: vi.fn(async (_dark: boolean) => {}),
}));

vi.mock("./backend", () => ({
  backend: () => native,
  isTauri: () => true,
}));

interface FakeQuery {
  mql: MediaQueryList;
  setDark: (dark: boolean) => void;
  listenerCount: () => number;
}

function fakeColorScheme(initialDark: boolean): FakeQuery {
  let matches = initialDark;
  const listeners = new Set<(e: MediaQueryListEvent) => void>();
  const mql = {
    get matches() {
      return matches;
    },
    media: "(prefers-color-scheme: dark)",
    onchange: null,
    addEventListener(_type: string, fn: (e: MediaQueryListEvent) => void) {
      listeners.add(fn);
    },
    removeEventListener(_type: string, fn: (e: MediaQueryListEvent) => void) {
      listeners.delete(fn);
    },
    addListener(fn: (e: MediaQueryListEvent) => void) {
      listeners.add(fn);
    },
    removeListener(fn: (e: MediaQueryListEvent) => void) {
      listeners.delete(fn);
    },
    dispatchEvent: () => true,
  } as unknown as MediaQueryList;
  return {
    mql,
    setDark: (dark) => {
      matches = dark;
      for (const fn of [...listeners]) fn({ matches } as MediaQueryListEvent);
    },
    listenerCount: () => listeners.size,
  };
}

beforeEach(() => {
  native.setSystemBarAppearance.mockClear();
  localStorage.clear();
  document.documentElement.removeAttribute("data-theme");
  vi.resetModules();
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.unstubAllEnvs();
});

describe("System appearance choice (GH #193)", () => {
  it("resolves the saved manual preference exactly as before", async () => {
    localStorage.setItem("logseq-claude.theme", "dark");
    const ui = await import("./ui");
    expect(ui.appearancePreference()).toBe("dark");
    ui.applyTheme();
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
    expect(native.setSystemBarAppearance).toHaveBeenLastCalledWith(true);
  });

  it("defaults to a deterministic light mode when the OS signal is unavailable", async () => {
    // jsdom has no working matchMedia — this is the "signal unavailable" path.
    localStorage.setItem("logseq-claude.theme", "system");
    const ui = await import("./ui");
    ui.applyTheme();
    expect(document.documentElement.getAttribute("data-theme")).toBe("light");
    expect(native.setSystemBarAppearance).toHaveBeenLastCalledWith(false);
  });

  it("resolves System from the OS signal immediately on startup", async () => {
    const darkOs = fakeColorScheme(true);
    vi.stubGlobal("matchMedia", () => darkOs.mql);
    localStorage.setItem("logseq-claude.theme", "system");
    const ui = await import("./ui");
    ui.applyTheme();
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
    expect(native.setSystemBarAppearance).toHaveBeenLastCalledWith(true);
    expect(darkOs.listenerCount()).toBeGreaterThan(0);
  });

  it("follows live OS changes while System is selected", async () => {
    const os = fakeColorScheme(true);
    vi.stubGlobal("matchMedia", () => os.mql);
    const ui = await import("./ui");
    ui.setAppearancePreference("system");
    expect(localStorage.getItem("logseq-claude.theme")).toBe("system");
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");

    os.setDark(false);
    expect(document.documentElement.getAttribute("data-theme")).toBe("light");
    expect(native.setSystemBarAppearance).toHaveBeenLastCalledWith(false);

    os.setDark(true);
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
  });

  it("a manual Light/Dark selection ignores later OS changes", async () => {
    const os = fakeColorScheme(true);
    vi.stubGlobal("matchMedia", () => os.mql);
    const ui = await import("./ui");
    // Start on System following the dark OS, then pick manual Light.
    ui.setAppearancePreference("system");
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
    ui.setAppearancePreference("light");
    expect(document.documentElement.getAttribute("data-theme")).toBe("light");
    expect(localStorage.getItem("logseq-claude.theme")).toBe("light");
    os.setDark(true);
    expect(document.documentElement.getAttribute("data-theme")).toBe("light");
    // The OS listener is detached while a manual selection is active (cleanup).
    expect(os.listenerCount()).toBe(0);
  });

  it("toggleTheme makes an explicit manual choice out of the System state", async () => {
    const os = fakeColorScheme(true);
    vi.stubGlobal("matchMedia", () => os.mql);
    localStorage.setItem("logseq-claude.theme", "system");
    const ui = await import("./ui");
    ui.applyTheme();
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
    // t t on a dark system resolves to an explicit Light selection.
    ui.toggleTheme();
    expect(ui.appearancePreference()).toBe("light");
    expect(localStorage.getItem("logseq-claude.theme")).toBe("light");
    os.setDark(false);
    expect(document.documentElement.getAttribute("data-theme")).toBe("light");
  });
});
