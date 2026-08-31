import {
  PDF_RENDERING_FINISHED,
  PDF_RENDERING_INITIAL,
  PDF_RENDERING_PAUSED,
  PDF_RENDERING_RUNNING,
  PdfRenderCoordinator,
  TinePdfRenderingQueue,
  type PdfRenderableView,
  type PdfVisiblePages,
} from "./pdfRenderCoordinator";

export interface PdfTileRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

export interface PdfTileViewport {
  width: number;
  height: number;
}

export interface PdfTileRenderTask {
  promise: Promise<unknown>;
  cancel(): void;
  onContinue?: Function;
}

export interface PdfTilePage {
  getViewport(options: { scale: number }): PdfTileViewport;
  render(options: {
    canvasContext: CanvasRenderingContext2D;
    viewport: PdfTileViewport;
    transform: number[];
  }): PdfTileRenderTask;
}

export interface PdfTileRequest {
  pageNumber: number;
  page: PdfTilePage;
  host: HTMLElement;
  scale: number;
  pageWidth: number;
  pageHeight: number;
  visibleRect: PdfTileRect;
}

interface PageGeneration {
  requestedKeys: Set<string>;
  readyNotified: boolean;
}

interface TileTaskOptions {
  id: number;
  key: string;
  pageNumber: number;
  page: PdfTilePage;
  scale: number;
  rect: PdfTileRect;
  host: HTMLElement | null;
  attach: boolean;
  maxCanvasDimension: number;
  isHighestPriority: (view: PdfRenderableView) => boolean;
  isCurrent: () => boolean;
  onComplete: (task: PdfTileView) => void;
  onDispose?: () => void;
}

function finitePositive(value: number, label: string): void {
  if (!Number.isFinite(value) || value <= 0) throw new Error(`${label} must be positive and finite`);
}

function zeroCanvas(canvas: HTMLCanvasElement | null): void {
  if (!canvas) return;
  canvas.width = 0;
  canvas.height = 0;
  canvas.remove();
}

function safeDevicePixelRatio(): number {
  const ratio = globalThis.devicePixelRatio;
  return Number.isFinite(ratio) && ratio > 0 ? Math.min(ratio, 2) : 1;
}

class PdfTileView implements PdfRenderableView {
  renderingState = PDF_RENDERING_INITIAL;
  renderingId: string;
  resume: (() => void) | null = null;
  maxCanvasPixels?: number;
  canvasPixelLimit: number;
  canvas: HTMLCanvasElement | null = null;
  private renderTask: PdfTileRenderTask | null = null;
  private disposed = false;

  constructor(readonly options: TileTaskOptions) {
    this.renderingId = `tile${options.id}`;
    this.canvasPixelLimit = Math.max(1, Math.floor(
      Math.min(options.rect.width, options.maxCanvasDimension)
      * Math.min(options.rect.height, options.maxCanvasDimension)
      * safeDevicePixelRatio() ** 2,
    ));
  }

  get id(): number {
    return this.options.id;
  }

  get key(): string {
    return this.options.key;
  }

  get pageNumber(): number {
    return this.options.pageNumber;
  }

  async draw(): Promise<void> {
    if (this.disposed || !this.options.isCurrent()) return;
    this.renderingState = PDF_RENDERING_RUNNING;
    const canvas = document.createElement("canvas");
    const context = canvas.getContext("2d", { alpha: false });
    if (!context) throw new Error("Unable to create PDF tile canvas context");

    const desiredRatio = safeDevicePixelRatio();
    const admittedPixels = Math.max(1, this.maxCanvasPixels ?? this.canvasPixelLimit);
    const pixelRatio = Math.min(
      desiredRatio,
      this.options.maxCanvasDimension / this.options.rect.width,
      this.options.maxCanvasDimension / this.options.rect.height,
      Math.sqrt(admittedPixels / (this.options.rect.width * this.options.rect.height)),
    );
    canvas.width = Math.max(1, Math.floor(this.options.rect.width * pixelRatio));
    canvas.height = Math.max(1, Math.floor(this.options.rect.height * pixelRatio));
    canvas.style.position = "absolute";
    canvas.style.left = `${this.options.rect.left}px`;
    canvas.style.top = `${this.options.rect.top}px`;
    canvas.style.width = `${this.options.rect.width}px`;
    canvas.style.height = `${this.options.rect.height}px`;
    canvas.dataset.pdfTileKey = this.key;
    canvas.dataset.pdfTileScale = String(this.options.scale);
    canvas.dataset.pdfTileLeft = String(this.options.rect.left);
    canvas.dataset.pdfTileTop = String(this.options.rect.top);
    canvas.dataset.pdfTileWidth = String(this.options.rect.width);
    canvas.dataset.pdfTileHeight = String(this.options.rect.height);
    this.canvas = canvas;

    const viewport = this.options.page.getViewport({ scale: this.options.scale });
    const renderTask = this.options.page.render({
      canvasContext: context,
      viewport,
      transform: [
        pixelRatio,
        0,
        0,
        pixelRatio,
        -this.options.rect.left * pixelRatio,
        -this.options.rect.top * pixelRatio,
      ],
    });
    this.renderTask = renderTask;
    renderTask.onContinue = (continueCallback: () => void) => {
      if (this.disposed || !this.options.isCurrent()) return;
      if (this.options.isHighestPriority(this)) continueCallback();
      else {
        this.renderingState = PDF_RENDERING_PAUSED;
        this.resume = () => {
          this.renderingState = PDF_RENDERING_RUNNING;
          continueCallback();
        };
      }
    };
    try {
      await renderTask.promise;
    } catch (error) {
      this.renderTask = null;
      this.resume = null;
      zeroCanvas(canvas);
      if (this.canvas === canvas) this.canvas = null;
      // A permanent failure must not leave the shared coordinator repeatedly
      // admitting a RUNNING task whose promise has already rejected.
      if (!this.disposed) this.renderingState = PDF_RENDERING_FINISHED;
      throw error;
    }
    this.renderTask = null;
    this.resume = null;
    if (this.disposed || !this.options.isCurrent()) {
      zeroCanvas(canvas);
      if (this.canvas === canvas) this.canvas = null;
      return;
    }
    this.renderingState = PDF_RENDERING_FINISHED;
    if (this.options.attach && this.options.host) this.options.host.appendChild(canvas);
    this.options.onComplete(this);
  }

  reset(): void {
    this.renderTask?.cancel();
    this.renderTask = null;
    this.resume = null;
    zeroCanvas(this.canvas);
    this.canvas = null;
    if (!this.disposed) this.renderingState = PDF_RENDERING_INITIAL;
  }

  scaleFallback(targetScale: number): void {
    if (!this.canvas || this.renderingState !== PDF_RENDERING_FINISHED) return;
    const ratio = targetScale / this.options.scale;
    this.canvas.style.left = `${this.options.rect.left * ratio}px`;
    this.canvas.style.top = `${this.options.rect.top * ratio}px`;
    this.canvas.style.width = `${this.options.rect.width * ratio}px`;
    this.canvas.style.height = `${this.options.rect.height * ratio}px`;
    this.canvas.style.transform = "";
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.options.onDispose?.();
    this.reset();
  }
}

function tileKey(pageNumber: number, scale: number, rect: PdfTileRect): string {
  return `${pageNumber}@${scale.toFixed(4)}:${rect.left},${rect.top},${rect.width},${rect.height}`;
}

export function pdfVisibleTileRects(
  pageWidth: number,
  pageHeight: number,
  visibleRect: PdfTileRect,
  tileSize = 768,
): PdfTileRect[] {
  finitePositive(pageWidth, "PDF page width");
  finitePositive(pageHeight, "PDF page height");
  finitePositive(tileSize, "PDF tile size");
  const left = Math.max(0, Math.min(pageWidth, visibleRect.left));
  const top = Math.max(0, Math.min(pageHeight, visibleRect.top));
  const right = Math.max(left, Math.min(pageWidth, visibleRect.left + visibleRect.width));
  const bottom = Math.max(top, Math.min(pageHeight, visibleRect.top + visibleRect.height));
  if (right <= left || bottom <= top) return [];

  const rects: PdfTileRect[] = [];
  const firstX = Math.floor(left / tileSize) * tileSize;
  const firstY = Math.floor(top / tileSize) * tileSize;
  for (let y = firstY; y < bottom; y += tileSize) {
    for (let x = firstX; x < right; x += tileSize) {
      rects.push({
        left: x,
        top: y,
        width: Math.min(tileSize, pageWidth - x),
        height: Math.min(tileSize, pageHeight - y),
      });
    }
  }
  return rects;
}

export interface PdfTileRendererOptions {
  coordinator: PdfRenderCoordinator;
  priority?: () => number;
  tileSize?: number;
  maxCanvasDimension?: number;
  onPageReady?: (pageNumber: number) => void;
  onRenderError?: (pageNumber: number, error: unknown) => void;
}

/** Window-coordinated clipped raster tiles. PDFPageView continues to own the
 * page fallback and text layer; this class owns only its replaceable canvases. */
export class PdfTileRenderer {
  private readonly queue: TinePdfRenderingQueue;
  private readonly tasks = new Map<string, PdfTileView>();
  private readonly generations = new Map<number, PageGeneration>();
  private requestedOrder: PdfTileView[] = [];
  private readonly captureTasks = new Set<PdfTileView>();
  private readonly captureRejectors = new Map<number, (error: unknown) => void>();
  private nextTaskId = 1_000_000;
  private disposed = false;

  constructor(private readonly options: PdfTileRendererOptions) {
    this.queue = new TinePdfRenderingQueue({
      coordinator: options.coordinator,
      priority: options.priority,
      cachedViews: () => this.tasks.values(),
      onRenderError: (view, error) => {
        const task = view as PdfTileView;
        this.captureRejectors.get(task.id)?.(error);
        this.options.onRenderError?.(task.pageNumber, error);
      },
    });
    this.queue.setViewer(this);
  }

  requestPage(request: PdfTileRequest): void {
    if (this.disposed) return;
    finitePositive(request.scale, "PDF tile scale");
    for (const task of this.tasks.values()) {
      if (task.pageNumber === request.pageNumber) task.scaleFallback(request.scale);
    }
    const rects = pdfVisibleTileRects(
      request.pageWidth,
      request.pageHeight,
      request.visibleRect,
      this.options.tileSize,
    );
    const requestedKeys = new Set(rects.map((rect) => tileKey(request.pageNumber, request.scale, rect)));
    const current = this.generations.get(request.pageNumber);
    if (
      current
      && current.requestedKeys.size === requestedKeys.size
      && [...requestedKeys].every((key) => current.requestedKeys.has(key))
    ) {
      this.rebuildRequestedOrder();
      this.requestRendering();
      return;
    }
    this.generations.set(request.pageNumber, { requestedKeys, readyNotified: false });

    for (const [key, task] of this.tasks) {
      if (task.pageNumber !== request.pageNumber || requestedKeys.has(key)) continue;
      if (task.renderingState !== PDF_RENDERING_FINISHED) {
        task.dispose();
        this.tasks.delete(key);
      }
    }
    for (const rect of rects) {
      const key = tileKey(request.pageNumber, request.scale, rect);
      if (this.tasks.has(key)) continue;
      let task!: PdfTileView;
      task = new PdfTileView({
        id: this.nextTaskId++,
        key,
        pageNumber: request.pageNumber,
        page: request.page,
        scale: request.scale,
        rect,
        host: request.host,
        attach: true,
        maxCanvasDimension: this.options.maxCanvasDimension ?? 16_384,
        isHighestPriority: (view) => this.queue.isHighestPriority(view),
        isCurrent: () => this.tasks.get(key) === task
          && this.generations.get(request.pageNumber)?.requestedKeys.has(key) === true,
        onComplete: () => this.finishPageIfReady(request.pageNumber),
      });
      this.tasks.set(key, task);
    }
    this.rebuildRequestedOrder();
    this.finishPageIfReady(request.pageNumber);
    this.requestRendering();
  }

  clearPage(pageNumber: number): void {
    this.generations.delete(pageNumber);
    for (const [key, task] of this.tasks) {
      if (task.pageNumber !== pageNumber) continue;
      task.dispose();
      this.captureTasks.delete(task);
      this.captureRejectors.delete(task.id);
      this.tasks.delete(key);
    }
    this.rebuildRequestedOrder();
  }

  isPageReady(pageNumber: number): boolean {
    const generation = this.generations.get(pageNumber);
    if (!generation || !generation.requestedKeys.size) return false;
    return [...generation.requestedKeys].every((key) =>
      this.tasks.get(key)?.renderingState === PDF_RENDERING_FINISHED
      && this.tasks.get(key)?.canvas !== null
    );
  }

  async captureArea(
    pageNumber: number,
    page: PdfTilePage,
    scale: number,
    sourceRect: PdfTileRect,
  ): Promise<Uint8Array | null> {
    if (this.disposed) return null;
    const rect: PdfTileRect = {
      left: sourceRect.left * scale,
      top: sourceRect.top * scale,
      width: sourceRect.width * scale,
      height: sourceRect.height * scale,
    };
    if (rect.width <= 0 || rect.height <= 0) return null;
    const key = `capture:${pageNumber}:${this.nextTaskId}`;
    let resolveTask!: (canvas: HTMLCanvasElement) => void;
    let rejectTask!: (error: unknown) => void;
    const rendered = new Promise<HTMLCanvasElement>((resolve, reject) => {
      resolveTask = resolve;
      rejectTask = reject;
    });
    let task!: PdfTileView;
    task = new PdfTileView({
      id: this.nextTaskId++,
      key,
      pageNumber,
      page,
      scale,
      rect,
      host: null,
      attach: false,
      maxCanvasDimension: this.options.maxCanvasDimension ?? 16_384,
      isHighestPriority: (view) => this.queue.isHighestPriority(view),
      isCurrent: () => this.tasks.get(key) === task,
      onComplete: (completed) => {
        if (completed.canvas) resolveTask(completed.canvas);
        else rejectTask(new Error("PDF area capture produced no canvas"));
      },
      onDispose: () => {
        const error = new Error("PDF area capture was cancelled");
        error.name = "RenderingCancelledException";
        rejectTask(error);
      },
    });
    this.tasks.set(key, task);
    this.captureRejectors.set(task.id, rejectTask);
    this.captureTasks.add(task);
    this.rebuildRequestedOrder();
    this.requestRendering();
    try {
      const canvas = await rendered;
      const blob = await new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, "image/png"));
      return blob ? new Uint8Array(await blob.arrayBuffer()) : null;
    } finally {
      task.dispose();
      this.captureRejectors.delete(task.id);
      this.captureTasks.delete(task);
      this.tasks.delete(key);
      this.rebuildRequestedOrder();
      this.requestRendering();
    }
  }

  forceRendering(): boolean {
    if (this.disposed) return false;
    const next = this.requestedOrder.find((task) => task.renderingState !== PDF_RENDERING_FINISHED);
    return next ? this.queue.renderView(next) : false;
  }

  getCachedPageViews(): Set<PdfRenderableView> {
    return new Set(this.tasks.values());
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    for (const task of this.tasks.values()) task.dispose();
    this.tasks.clear();
    this.captureRejectors.clear();
    this.captureTasks.clear();
    this.generations.clear();
    this.requestedOrder = [];
    this.queue.dispose();
  }

  private finishPageIfReady(pageNumber: number): void {
    const generation = this.generations.get(pageNumber);
    if (!generation) return;
    for (const key of generation.requestedKeys) {
      const task = this.tasks.get(key);
      if (task?.renderingState !== PDF_RENDERING_FINISHED || !task.canvas) return;
    }
    // The sharper requested set is now complete. Only now remove lower-scale
    // and offscreen tiles, so zoom and scroll never replace content with blank.
    for (const [key, task] of this.tasks) {
      if (task.pageNumber !== pageNumber || generation.requestedKeys.has(key)) continue;
      task.dispose();
      this.tasks.delete(key);
    }
    this.rebuildRequestedOrder();
    if (!generation.readyNotified) {
      generation.readyNotified = true;
      this.options.onPageReady?.(pageNumber);
    }
  }

  private rebuildRequestedOrder(): void {
    this.requestedOrder = [...this.captureTasks];
    for (const [pageNumber, generation] of this.generations) {
      for (const key of generation.requestedKeys) {
        const task = this.tasks.get(key);
        if (task && task.pageNumber === pageNumber) this.requestedOrder.push(task);
      }
    }
  }

  private requestRendering(): void {
    const views = this.requestedOrder.map((view) => ({ id: view.id, view }));
    if (!views.length) return;
    const visible: PdfVisiblePages = {
      first: views[0],
      last: views.at(-1)!,
      views,
      ids: new Set(views.map(({ id }) => id)),
    };
    this.queue.renderHighestPriority(visible);
  }
}
