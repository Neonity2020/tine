// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { QueryNotReadyError, QueryUnavailableError } from "./backend";
import { exportPagePdf } from "./print";

const mocks = vi.hoisted(() => ({ load: vi.fn(), toast: vi.fn(), rebound: new Set<() => void>(), binding: 1 }));
vi.mock("./backend", async (original) => ({
  ...await original<typeof import("./backend")>(),
  backend: () => ({ pagePrintHtml: mocks.load }),
}));
vi.mock("./ui", () => ({ pushToast: mocks.toast, graphMeta: () => ({ root: "/fixture" }), graphTransitioning: () => false }));
vi.mock("./persistence", () => ({ graphBinding: () => mocks.binding }));
vi.mock("./modeHooks", () => ({ onGraphRebound: (callback: () => void) => {
  mocks.rebound.add(callback);
  return () => mocks.rebound.delete(callback);
} }));

describe("print_preparation_retries_only_readiness_and_never_prints_partial_output", () => {
  afterEach(() => {
    expect(mocks.rebound.size).toBe(0);
    vi.useRealTimers(); vi.resetAllMocks(); document.querySelectorAll("iframe").forEach(f => f.remove());
  });

  it("retries readiness then displays the terminal Print budget message without an iframe", async () => {
    vi.useFakeTimers();
    const message = "Couldn't prepare this page for PDF: a query exceeds the Print limit. Narrow the query and try again.";
    mocks.load.mockRejectedValueOnce(new QueryNotReadyError("indexing"))
      .mockRejectedValueOnce(new QueryUnavailableError("print_query_budget_exceeded", message));
    const result = exportPagePdf("Print");
    await vi.advanceTimersByTimeAsync(1000);
    await result;
    expect(mocks.load).toHaveBeenCalledTimes(2);
    expect(mocks.toast).toHaveBeenCalledWith(message, "error");
    expect(document.querySelector("iframe")).toBeNull();
  });

  it("a graph rebound stops pending preparation and ignores late HTML", async () => {
    let finish!: (html: string) => void;
    mocks.load.mockImplementationOnce(() => new Promise<string>(resolve => { finish = resolve; }));
    const result = exportPagePdf("Old graph");
    await Promise.resolve();
    mocks.binding++;
    for (const callback of mocks.rebound) callback();
    await result;
    finish("<html><body>late output</body></html>");
    await Promise.resolve();
    expect(document.querySelector("iframe")).toBeNull();
    expect(mocks.toast).not.toHaveBeenCalled();
  });

  it("a newer Print cancels the previous owner and teardown cancels its successor", async () => {
    mocks.load.mockImplementation(() => new Promise<string>(() => {}));
    const first = exportPagePdf("First");
    await Promise.resolve();
    const second = exportPagePdf("Second");
    await first;
    window.dispatchEvent(new Event("pagehide"));
    await second;
    expect(document.querySelector("iframe")).toBeNull();
    expect(mocks.toast).not.toHaveBeenCalled();
  });
});
