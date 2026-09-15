import { createSignal } from "solid-js";
import { blockDropPosition, type BlockDropPosition } from "../../editor/blockDrag";
import { dropSelection, setDragSelectionSuppressed } from "../../dragSelectionGuard";
import { endEdit } from "../../editorController";
import { doc, moveBlocksRelative, selectedIds } from "../../store";

// Pointer-based drag reorder (HTML5 DnD is unreliable in WebKitGTK).
export const [dragId, setDragId] = createSignal<string | null>(null);
export const [dropInd, setDropInd] = createSignal<{ id: string; position: BlockDropPosition } | null>(null);
export let dragMoved = false;

export function beginDrag(id: string, e: MouseEvent) {
  const startX = e.clientX;
  const startY = e.clientY;
  let capturedIds: string[] | null = null;
  dragMoved = false;
  const onMove = (ev: MouseEvent) => {
    if (!dragMoved && Math.hypot(ev.clientX - startX, ev.clientY - startY) < 4) return;
    if (!dragMoved) {
      dragMoved = true;
      const selected = selectedIds();
      capturedIds = selected.length ? [...selected] : [id];
      setDragId(id);
      endEdit("drag-start");
      // Moving a block is not a text gesture. WebKit otherwise runs its own
      // selection drag from the bullet and paints every block the pointer
      // crosses blue (GH #424, macOS; Chromium does not do this, which is why
      // the same build looked clean on Windows).
      setDragSelectionSuppressed(true);
    }
    // WebKit can re-anchor a selection mid-drag; the class alone is not enough.
    dropSelection();
    const el = (document.elementFromPoint(ev.clientX, ev.clientY) as HTMLElement | null)?.closest(
      ".ls-block"
    ) as HTMLElement | null;
    const tid = el?.dataset.blockId;
    if (tid) {
      const main = el!.querySelector(".block-main")!.getBoundingClientRect();
      setDropInd({
        id: tid,
        position: blockDropPosition(ev.clientX, ev.clientY, el!.getBoundingClientRect(), main),
      });
    } else {
      setDropInd(null);
    }
  };
  const onUp = () => {
    document.removeEventListener("mousemove", onMove);
    document.removeEventListener("mouseup", onUp);
    setDragSelectionSuppressed(false);
    const ind = dropInd();
    if (dragMoved && ind && doc.byId[ind.id]) {
      void moveBlocksRelative(capturedIds ?? [id], ind.id, ind.position);
    }
    setDragId(null);
    setDropInd(null);
    setTimeout(() => (dragMoved = false), 0);
  };
  document.addEventListener("mousemove", onMove);
  document.addEventListener("mouseup", onUp);
}
