// Pointer-based vertical row reorder shared by the sidebar lists (GH
// #211: right-sidebar items, left-sidebar favorites).
//
// HTML5 DnD is unreliable in WebKitGTK (see TabBar/Block), so this follows
// the repo's hand-rolled pattern: the row gets onPointerDown, a 4px movement
// threshold separates a reorder drag from an ordinary click, and the click
// that ends a drag is swallowed via rowReorderClickSuppressed() so the row's
// own click handler (navigate/activate) does not also fire. Rows must carry
// `data-row-index` with their current index so the drop target can be found
// with document.elementFromPoint.
const DRAG_THRESHOLD_PX = 4;

let suppressClick = false;

// Martin, 2026-08-25: dragging a favorite also started a text selection on the
// row's title, so the label smeared blue under the cursor. A reorder is not a
// text gesture. Rather than preventDefault() on pointerdown — which would also
// swallow focus and the ordinary click this helper deliberately preserves — the
// document is made unselectable only once the drag threshold is crossed, and
// any selection the pointerdown already began is dropped. Released on drop and
// on pointercancel.
const DRAGGING_CLASS = "row-reorder-dragging";

function setSelectionSuppressed(on: boolean): void {
  document.documentElement.classList.toggle(DRAGGING_CLASS, on);
  if (on) dropSelection();
}

function dropSelection(): void {
  const selection = document.getSelection?.();
  if (selection && selection.rangeCount > 0) selection.removeAllRanges();
}

/** True for the click that terminates a reorder drag — row click handlers
 *  must bail out when this returns true. */
export function rowReorderClickSuppressed(): boolean {
  return suppressClick;
}

export interface RowDropTarget {
  index: number;
  before: boolean;
}

/** Attach a reorder drag to a row's pointerdown. `commit(from, to)` receives
 *  the final `to` index in the post-removal array (ready for splice-insert);
 *  self-drops and no-op placements never call it. `setIndicator` reports the
 *  live half-row drop target (or null) for the indicator classes. */
export function beginRowReorderDrag(
  event: PointerEvent,
  from: number,
  rowSelector: string,
  setIndicator: (target: RowDropTarget | null) => void,
  commit: (from: number, to: number) => void,
): void {
  if (event.button !== 0) return;
  const startX = event.clientX;
  const startY = event.clientY;
  let dragging = false;
  let target: RowDropTarget | null = null;

  const onMove = (ev: PointerEvent) => {
    if (!dragging) {
      if (Math.hypot(ev.clientX - startX, ev.clientY - startY) < DRAG_THRESHOLD_PX) return;
      dragging = true;
      setSelectionSuppressed(true);
    }
    // WebKit can re-establish the selection mid-drag; the class alone is not
    // enough once a selection is already anchored.
    dropSelection();
    const row = document
      .elementFromPoint(ev.clientX, ev.clientY)
      ?.closest<HTMLElement>(rowSelector);
    if (row && row.dataset.rowIndex !== undefined) {
      const index = Number(row.dataset.rowIndex);
      const rect = row.getBoundingClientRect();
      target = { index, before: ev.clientY < rect.top + rect.height / 2 };
    } else {
      target = null;
    }
    setIndicator(target);
  };

  const cleanup = () => {
    document.removeEventListener("pointermove", onMove);
    document.removeEventListener("pointerup", onUp);
    document.removeEventListener("pointercancel", cleanup);
    setSelectionSuppressed(false);
    setIndicator(null);
  };
  const onUp = () => {
    cleanup();
    if (!dragging) return;
    // Swallow the click that follows a real reorder drag so rows whose
    // primary action is a click (navigation) don't fire it as the drop lands.
    suppressClick = true;
    setTimeout(() => {
      suppressClick = false;
    }, 0);
    if (!target) return;
    const to = target.index + (target.before ? 0 : 1);
    const adjusted = from < to ? to - 1 : to;
    if (adjusted !== from) commit(from, adjusted);
  };

  document.addEventListener("pointermove", onMove);
  document.addEventListener("pointerup", onUp);
  document.addEventListener("pointercancel", cleanup);
}
