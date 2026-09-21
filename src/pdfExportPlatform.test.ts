// @vitest-environment jsdom
//
// "Export to PDF…" is desktop-only, and the reason is the platform, not Tine:
// a mobile WebView cannot print. Android's renderer disables scripted printing
// (AwPrintRenderFrameHelperDelegate::IsScriptedPrintEnabled returns false), and
// on iOS WebKit forwards window.print() only into the private WKUIDelegate SPI
// `_webView:printFrame:`, which wry does not implement. Neither throws, so the
// option used to open a dialog, call print(), and silently do nothing — which
// is how it was reported (GH #560, Android).
//
// `openPdfExport` is the single funnel for the page context menu, the
// "Export current page to PDF…" command and its keybinding, so the gate is
// pinned here, in both directions.
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("./backend", () => ({
  backend: () => ({ getAppBool: vi.fn(), setAppBool: vi.fn() }),
  isTauri: () => true,
  cachedConflictCapsules: () => [],
  loadConflictCapsules: vi.fn(async () => {}),
  retireConflictCapsule: vi.fn(),
  storeConflictCapsule: vi.fn(),
}));

async function loadUi(platform: "android" | "ios" | "desktop") {
  vi.resetModules();
  globalThis.__TINE_PLATFORM__ = platform;
  return await import("./ui");
}

afterEach(() => {
  delete globalThis.__TINE_PLATFORM__;
});

describe("PDF export is offered only where printing exists (GH #560)", () => {
  it("opens the export dialog on desktop", async () => {
    const ui = await loadUi("desktop");

    ui.openPdfExport("Alpha");

    expect(ui.pdfExportPage()).toBe("Alpha");
    expect(ui.toasts()).toHaveLength(0);
  });

  // Spelled out rather than generated: the regression catalog matches test
  // titles literally, so a template-literal title is unfindable coverage.
  it("explains itself instead of opening a dead dialog on android", async () => {
    const ui = await loadUi("android");

    ui.openPdfExport("Alpha");

    // The user-visible outcome: no dialog that leads nowhere, and a reason.
    expect(ui.pdfExportPage()).toBeNull();
    const messages = ui.toasts().map((toast) => toast.message);
    expect(messages).toHaveLength(1);
    expect(messages[0]).toContain("desktop app");
  });

  it("explains itself instead of opening a dead dialog on ios", async () => {
    const ui = await loadUi("ios");

    ui.openPdfExport("Alpha");

    expect(ui.pdfExportPage()).toBeNull();
    const messages = ui.toasts().map((toast) => toast.message);
    expect(messages).toHaveLength(1);
    expect(messages[0]).toContain("desktop app");
  });
});
