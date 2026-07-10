import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { PdfViewer } from "./PdfViewer";

const getDocumentMock = vi.hoisted(() => vi.fn());

vi.mock("pdfjs-dist", () => ({
  GlobalWorkerOptions: {},
  getDocument: getDocumentMock,
  TextLayer: class {
    render() {
      return Promise.resolve();
    }

    update() {
      return Promise.resolve();
    }
  },
}));

vi.mock("pdfjs-dist/build/pdf.worker.min.mjs?url", () => ({
  default: "pdf.worker.test.js",
}));

async function flush() {
  for (let i = 0; i < 8; i++) await Promise.resolve();
}

class TestIntersectionObserver {
  static instances: TestIntersectionObserver[] = [];
  readonly elements: Element[] = [];

  constructor(private readonly callback: IntersectionObserverCallback) {
    TestIntersectionObserver.instances.push(this);
  }

  observe(element: Element) {
    this.elements.push(element);
  }

  unobserve() {}
  disconnect() {}
  takeRecords() { return []; }

  show(element: Element) {
    this.callback([{ isIntersecting: true, target: element } as IntersectionObserverEntry], this as unknown as IntersectionObserver);
  }
}

function page(width: number, height: number) {
  return {
    getViewport: vi.fn(({ scale }: { scale: number }) => ({ width: width * scale, height: height * scale })),
    getTextContent: vi.fn().mockResolvedValue({ items: [] }),
    render: vi.fn().mockReturnValue({ promise: Promise.resolve(), cancel: vi.fn() }),
  };
}

function documentWithPages(pages: ReturnType<typeof page>[]) {
  return {
    numPages: pages.length,
    getPage: vi.fn((number: number) => Promise.resolve(pages[number - 1])),
    destroy: vi.fn().mockResolvedValue(undefined),
  };
}

describe("PdfViewer resource safety", () => {
  beforeEach(() => {
    getDocumentMock.mockReset();
    TestIntersectionObserver.instances = [];
    vi.stubGlobal("IntersectionObserver", TestIntersectionObserver);
    vi.spyOn(backend(), "readHighlights").mockResolvedValue([]);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
    document.body.replaceChildren();
  });

  it("shows an error and creates no page wrappers when pdf.js rejects the document", async () => {
    vi.spyOn(backend(), "readAsset").mockResolvedValue(new Uint8Array([1, 2, 3]));
    getDocumentMock.mockReturnValue({ promise: Promise.reject(new Error("invalid pdf")) });

    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(() => <PdfViewer filename="bad.pdf" label="Bad PDF" />, host);
    try {
      await flush();

      expect(host.querySelector(".pdf-load-error")?.textContent).toContain("Couldn't load this PDF");
      expect(host.querySelector(".pdf-load-error")?.textContent).toContain("invalid pdf");
      expect(host.querySelector(".pdf-page")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("rejects an oversized PDF before handing its bytes to pdf.js", async () => {
    const byteLength = 256 * 1024 * 1024 + 1;
    vi.spyOn(backend(), "readAsset").mockResolvedValue({ length: byteLength, byteLength } as Uint8Array);

    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(() => <PdfViewer filename="huge.pdf" label="Huge PDF" />, host);
    try {
      await flush();

      expect(host.querySelector(".pdf-load-error")?.textContent).toContain("larger than 256 MiB");
      expect(getDocumentMock).not.toHaveBeenCalled();
    } finally {
      dispose();
    }
  });

  it("rejects a page count that would create too many layout nodes", async () => {
    vi.spyOn(backend(), "readAsset").mockResolvedValue(new Uint8Array([1]));
    const pdf = {
      numPages: 5001,
      getPage: vi.fn(),
      destroy: vi.fn().mockResolvedValue(undefined),
    };
    getDocumentMock.mockReturnValue({ promise: Promise.resolve(pdf) });

    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(() => <PdfViewer filename="many.pdf" label="Many pages" />, host);
    try {
      await flush();

      expect(host.querySelector(".pdf-load-error")?.textContent).toContain("at most 5000 pages");
      expect(host.querySelector(".pdf-page")).toBeNull();
      expect(pdf.getPage).not.toHaveBeenCalled();
      expect(pdf.destroy).toHaveBeenCalledOnce();
    } finally {
      dispose();
    }
  });

  it("rejects unsafe dimensions on the first page before building the layout", async () => {
    vi.spyOn(backend(), "readAsset").mockResolvedValue(new Uint8Array([1]));
    const pdf = documentWithPages([page(14_401, 792)]);
    getDocumentMock.mockReturnValue({ promise: Promise.resolve(pdf) });

    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(() => <PdfViewer filename="wide.pdf" label="Wide PDF" />, host);
    try {
      await flush();

      expect(host.querySelector(".pdf-load-error")?.textContent).toContain("page 1 is too large");
      expect(host.querySelector(".pdf-page")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("rejects unsafe dimensions discovered on a later page", async () => {
    vi.spyOn(backend(), "readAsset").mockResolvedValue(new Uint8Array([1]));
    const pdf = documentWithPages([page(612, 792), page(20_000, 100)]);
    getDocumentMock.mockReturnValue({ promise: Promise.resolve(pdf) });

    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(() => <PdfViewer filename="mixed.pdf" label="Mixed PDF" />, host);
    try {
      await flush();
      const secondPage = host.querySelectorAll(".pdf-page")[1];
      TestIntersectionObserver.instances[0].show(secondPage);
      await flush();

      expect(host.querySelector(".pdf-load-error")?.textContent).toContain("page 2 is too large");
      expect(host.querySelector(".pdf-page")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("downsamples a valid large page to a bounded canvas allocation", async () => {
    vi.spyOn(backend(), "readAsset").mockResolvedValue(new Uint8Array([1]));
    vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(1400);
    vi.spyOn(window, "devicePixelRatio", "get").mockReturnValue(2);
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue({} as CanvasRenderingContext2D);
    const largePage = page(1000, 14_000);
    const pdf = documentWithPages([largePage]);
    getDocumentMock.mockReturnValue({ promise: Promise.resolve(pdf) });

    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(() => <PdfViewer filename="poster.pdf" label="Poster PDF" />, host);
    try {
      await flush();
      const pageElement = host.querySelector(".pdf-page")!;
      TestIntersectionObserver.instances[0].show(pageElement);
      await flush();

      const canvas = pageElement.querySelector("canvas")!;
      expect(host.querySelector(".pdf-load-error")).toBeNull();
      expect(canvas.width).toBeLessThanOrEqual(16_384);
      expect(canvas.height).toBeLessThanOrEqual(16_384);
      expect(canvas.width * canvas.height).toBeLessThanOrEqual(16_777_216);
      expect(largePage.render).toHaveBeenCalledOnce();
      expect(largePage.render.mock.calls[0][0].transform[0]).toBeLessThan(2);
    } finally {
      dispose();
    }
  });
});
