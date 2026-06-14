import { For, Show, createEffect, createSignal, on, onMount, type JSX } from "solid-js";
import * as pdfjs from "pdfjs-dist";
import workerUrl from "pdfjs-dist/build/pdf.worker.min.mjs?url";
import { backend } from "../backend";
import { closePdf } from "../ui";
import { openPage } from "../router";
import { hlsPageName } from "../pdf";
import type { Highlight, Rect } from "../types";

pdfjs.GlobalWorkerOptions.workerSrc = workerUrl;

const COLORS = ["yellow", "green", "blue", "red", "purple"];
const COLOR_RGBA: Record<string, string> = {
  yellow: "rgba(255, 226, 86, 0.4)",
  green: "rgba(116, 226, 130, 0.4)",
  blue: "rgba(110, 176, 246, 0.4)",
  red: "rgba(246, 130, 130, 0.4)",
  purple: "rgba(190, 140, 246, 0.4)",
};

interface Pending {
  page: number;
  rects: Rect[];
  bounding: Rect;
  text: string;
}

export function PdfViewer(props: { filename: string; label: string; page?: number }): JSX.Element {
  let scrollRef!: HTMLDivElement;
  const pageEls: Record<number, HTMLDivElement> = {};
  const [highlights, setHighlights] = createSignal<Highlight[]>([]);
  const [menu, setMenu] = createSignal<{ x: number; y: number } | null>(null);
  let pending: Pending | null = null;
  let scale = 1.4;
  const hlLayers: Record<number, HTMLDivElement> = {};

  const persist = () => void backend().writeHighlights(props.filename, props.label, highlights());

  onMount(async () => {
    setHighlights(await backend().readHighlights(props.filename));
    let bytes: Uint8Array;
    try {
      bytes = await backend().readAsset(props.filename);
    } catch {
      return;
    }
    if (!bytes.length) return;
    const pdf = await pdfjs.getDocument({ data: bytes }).promise;
    // Fit pages to the pane width.
    const first = await pdf.getPage(1);
    const baseWidth = first.getViewport({ scale: 1 }).width;
    const avail = (scrollRef.clientWidth || 700) - 32;
    scale = Math.min(2, Math.max(0.6, avail / baseWidth));
    for (let n = 1; n <= pdf.numPages; n++) {
      const page = await pdf.getPage(n);
      const viewport = page.getViewport({ scale });
      const wrap = document.createElement("div");
      wrap.className = "pdf-page";
      wrap.style.width = `${viewport.width}px`;
      wrap.style.height = `${viewport.height}px`;
      wrap.style.setProperty("--scale-factor", String(scale));

      const canvas = document.createElement("canvas");
      canvas.width = viewport.width;
      canvas.height = viewport.height;
      wrap.appendChild(canvas);

      const textLayer = document.createElement("div");
      textLayer.className = "textLayer";
      wrap.appendChild(textLayer);

      const hl = document.createElement("div");
      hl.className = "pdf-hl-layer";
      wrap.appendChild(hl);
      hlLayers[n] = hl;

      scrollRef.appendChild(wrap);
      wrap.dataset.page = String(n);
      pageEls[n] = wrap;

      await page.render({ canvasContext: canvas.getContext("2d")!, viewport }).promise;
      const textContent = await page.getTextContent();
      const tl = new (pdfjs as any).TextLayer({ textContentSource: textContent, container: textLayer, viewport });
      await tl.render();
    }
    repaint();
    if (props.page && pageEls[props.page]) {
      pageEls[props.page].scrollIntoView({ block: "start" });
    }
  });

  // Repaint highlight overlays whenever the set changes.
  createEffect(on(highlights, repaint));

  function repaint() {
    for (const n of Object.keys(hlLayers)) {
      const layer = hlLayers[Number(n)];
      layer.innerHTML = "";
      for (const h of highlights().filter((x) => x.page === Number(n))) {
        for (const r of h.position.rects) {
          const div = document.createElement("div");
          div.className = "pdf-hl";
          div.style.left = `${r.left * scale}px`;
          div.style.top = `${r.top * scale}px`;
          div.style.width = `${r.width * scale}px`;
          div.style.height = `${r.height * scale}px`;
          div.style.background = COLOR_RGBA[h.color] ?? COLOR_RGBA.yellow;
          layer.appendChild(div);
        }
      }
    }
  }

  const onMouseUp = (e: MouseEvent) => {
    const sel = window.getSelection();
    if (!sel || sel.isCollapsed || !sel.toString().trim()) {
      setMenu(null);
      return;
    }
    const range = sel.getRangeAt(0);
    const clientRects = Array.from(range.getClientRects()).filter((r) => r.width > 0 && r.height > 0);
    if (!clientRects.length) return;

    // Find the page wrapper containing the selection.
    const first = clientRects[0];
    const wrap = (e.target as HTMLElement).closest(".pdf-page") as HTMLElement | null;
    const pageWrap =
      wrap ?? document.elementFromPoint(first.left, first.top)?.closest(".pdf-page") as HTMLElement | null;
    if (!pageWrap) return;
    const pageNum = Number(pageWrap.dataset.page);
    const base = pageWrap.getBoundingClientRect();

    const rects: Rect[] = clientRects.map((r) => ({
      left: (r.left - base.left) / scale,
      top: (r.top - base.top) / scale,
      width: r.width / scale,
      height: r.height / scale,
    }));
    const left = Math.min(...rects.map((r) => r.left));
    const top = Math.min(...rects.map((r) => r.top));
    const right = Math.max(...rects.map((r) => r.left + r.width));
    const bottom = Math.max(...rects.map((r) => r.top + r.height));
    pending = {
      page: pageNum,
      rects,
      bounding: { left, top, width: right - left, height: bottom - top },
      text: sel.toString(),
    };
    setMenu({ x: e.clientX, y: e.clientY });
  };

  const createHighlight = (color: string) => {
    if (!pending) return;
    const h: Highlight = {
      id: crypto.randomUUID(),
      page: pending.page,
      position: { page: pending.page, bounding: pending.bounding, rects: pending.rects },
      color,
      text: pending.text,
      image: null,
    };
    setHighlights([...highlights(), h]);
    persist();
    window.getSelection()?.removeAllRanges();
    setMenu(null);
    pending = null;
  };

  return (
    <div class="pdf-viewer">
      <div class="pdf-toolbar">
        <span class="pdf-title">{props.label}</span>
        <div class="pdf-toolbar-actions">
          <button
            class="pdf-notes-btn"
            title="Open highlights & notes page"
            onClick={() => openPage(hlsPageName(props.filename), "page")}
          >
            Notes
          </button>
          <button class="icon-btn" title="Close PDF" onClick={closePdf}>
            ✕
          </button>
        </div>
      </div>
      <div class="pdf-scroll" ref={scrollRef} onMouseUp={onMouseUp} />
      <Show when={menu()}>
        <div class="pdf-color-menu" style={{ left: `${menu()!.x}px`, top: `${menu()!.y + 8}px` }}>
          <For each={COLORS}>
            {(c) => (
              <button
                class="pdf-color-swatch"
                style={{ background: COLOR_RGBA[c] }}
                onMouseDown={(e) => {
                  e.preventDefault();
                  createHighlight(c);
                }}
              />
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}
