// @vitest-environment jsdom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { PdfRenderCoordinator } from "./pdfRenderCoordinator";
import {
  PdfTileRenderer,
  pdfVisibleTileRects,
  type PdfTilePage,
  type PdfTileRenderTask,
} from "./pdfTileRenderer";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((done, fail) => {
    resolve = done;
    reject = fail;
  });
  return { promise, resolve, reject };
}

async function turns(count = 6): Promise<void> {
  for (let index = 0; index < count; index += 1) await Promise.resolve();
}

function controlledPage() {
  const renders: Array<{
    options: Parameters<PdfTilePage["render"]>[0];
    done: ReturnType<typeof deferred<unknown>>;
    cancel: ReturnType<typeof vi.fn>;
  }> = [];
  const page: PdfTilePage = {
    getViewport: vi.fn(({ scale }) => ({ width: 1000 * scale, height: 1200 * scale })),
    render: vi.fn((options) => {
      const done = deferred<unknown>();
      const cancel = vi.fn(() => {
        const error = new Error("cancelled");
        error.name = "RenderingCancelledException";
        done.reject(error);
      });
      const task: PdfTileRenderTask = { promise: done.promise, cancel };
      renders.push({ options, done, cancel });
      return task;
    }),
  };
  return { page, renders };
}

describe("high-zoom PDF tile renderer", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    document.body.replaceChildren();
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockImplementation(function (
      this: HTMLCanvasElement,
    ) {
      return { canvas: this } as CanvasRenderingContext2D;
    });
    vi.stubGlobal("devicePixelRatio", 1);
  });

  it("requests only grid rectangles intersecting the visible page region", () => {
    const rects = pdfVisibleTileRects(
      4000,
      6000,
      { left: 1700, top: 2500, width: 500, height: 400 },
      768,
    );

    expect(rects).toEqual([
      { left: 1536, top: 2304, width: 768, height: 768 },
    ]);
    expect(rects.every((rect) =>
      rect.left < 2200 && rect.left + rect.width > 1700
      && rect.top < 2900 && rect.top + rect.height > 2500
    )).toBe(true);
  });

  it("uses a clipped viewport transform and cancels stale in-flight scale work", async () => {
    const { page, renders } = controlledPage();
    const host = document.createElement("div");
    const renderer = new PdfTileRenderer({
      coordinator: new PdfRenderCoordinator(4_000_000, 1_000_000),
      tileSize: 500,
    });
    renderer.requestPage({
      pageNumber: 1,
      page,
      host,
      scale: 4,
      pageWidth: 4000,
      pageHeight: 4800,
      visibleRect: { left: 600, top: 1100, width: 200, height: 200 },
    });
    await turns();

    expect(renders).toHaveLength(1);
    expect(renders[0].options.transform).toEqual([1, 0, 0, 1, -500, -1000]);

    renderer.requestPage({
      pageNumber: 1,
      page,
      host,
      scale: 5,
      pageWidth: 5000,
      pageHeight: 6000,
      visibleRect: { left: 600, top: 1100, width: 200, height: 200 },
    });
    expect(renders[0].cancel).toHaveBeenCalledOnce();
    await turns();
    expect(renders).toHaveLength(2);
    renderer.dispose();
  });

  it("keeps completed lower-scale tiles until the requested sharper set completes", async () => {
    const { page, renders } = controlledPage();
    const host = document.createElement("div");
    const renderer = new PdfTileRenderer({
      coordinator: new PdfRenderCoordinator(4_000_000, 1_000_000),
      tileSize: 400,
    });
    const request = (scale: number) => renderer.requestPage({
      pageNumber: 1,
      page,
      host,
      scale,
      pageWidth: 4000,
      pageHeight: 4800,
      visibleRect: { left: 0, top: 0, width: 300, height: 300 },
    });

    request(4);
    await turns();
    renders[0].done.resolve(undefined);
    await turns();
    const fallback = host.querySelector<HTMLCanvasElement>("canvas")!;
    expect(fallback.dataset.pdfTileKey).toContain("@4.0000");

    request(5);
    await turns();
    expect(host.contains(fallback)).toBe(true);
    expect(fallback.style.width).toBe("500px");
    expect(renders).toHaveLength(2);
    renders[1].done.resolve(undefined);
    await turns();

    expect(host.contains(fallback)).toBe(false);
    expect(fallback.width).toBe(0);
    expect(host.querySelectorAll("canvas")).toHaveLength(1);
    expect(host.querySelector("canvas")?.dataset.pdfTileKey).toContain("@5.0000");
    renderer.dispose();
  });

  it("accounts measured tile backing stores within the shared window budget", async () => {
    const { page, renders } = controlledPage();
    const host = document.createElement("div");
    const coordinator = new PdfRenderCoordinator(100_000, 50_000);
    const renderer = new PdfTileRenderer({ coordinator, tileSize: 500 });
    renderer.requestPage({
      pageNumber: 1,
      page,
      host,
      scale: 4,
      pageWidth: 4000,
      pageHeight: 4800,
      visibleRect: { left: 0, top: 0, width: 900, height: 400 },
    });
    await turns();
    renders[0].done.resolve(undefined);
    await turns();
    renders[1].done.resolve(undefined);
    await turns();

    const measured = [...host.querySelectorAll("canvas")]
      .reduce((sum, canvas) => sum + canvas.width * canvas.height, 0);
    expect(measured).toBe(coordinator.retainedPixels());
    expect(measured).toBeLessThanOrEqual(100_000);
    renderer.dispose();
  });

  it("captures from a dedicated clipped render and tolerates a null PNG blob", async () => {
    const { page, renders } = controlledPage();
    const renderer = new PdfTileRenderer({
      coordinator: new PdfRenderCoordinator(1_000_000, 500_000),
    });
    const toBlob = vi.spyOn(HTMLCanvasElement.prototype, "toBlob")
      .mockImplementation((callback) => callback(null));

    const capture = renderer.captureArea(
      2,
      page,
      3.5,
      { left: 10.25, top: 20.5, width: 30.5, height: 40.25 },
    );
    await turns();
    expect(renders).toHaveLength(1);
    expect(renders[0].options.transform[4]).toBeCloseTo(-35.875);
    expect(renders[0].options.transform[5]).toBeCloseTo(-71.75);
    const captureCanvas = renders[0].options.canvasContext.canvas;
    renders[0].done.resolve(undefined);

    await expect(capture).resolves.toBeNull();
    expect(toBlob).toHaveBeenCalledOnce();
    // The temporary backing store is released regardless of encoder result.
    expect(captureCanvas.width).toBe(0);
    renderer.dispose();
  });
});
