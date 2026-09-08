// **Reordering a condition inside the query sheet (SPEC §7.4, P6).**
//
// The pointer loop itself is NOT here: `rowReorder.ts` owns "drag a row up and
// down a list", the sidebar lists already use it, and a second threshold + a
// second `elementFromPoint` walk + a second text-selection guard is exactly the
// twin D-14 forbids. What IS here is the three things the sheet needs that a
// flat sidebar list does not:
//
//  - **The list a drop may land in is the DRAGGED ROW'S OWN.** The sheet draws a
//    tree, so "the row under the pointer" is ambiguous in a way a sidebar's flat
//    list never is: the pointer can be over a row two levels down. Every item
//    carries `data-qs-parent` — the loc of the boolean node it is a child of —
//    and the drop selector names that one value, so `closest()` resolves a
//    pointer inside a nested group to the GROUP's own item in this list and
//    resolves a pointer in an unrelated list to nothing at all. A foreign-parent
//    target is rejected by the selector rather than by arithmetic after the
//    fact; this is reorder, not the deferred drag-to-group card, and no gesture
//    here can move a condition between lists.
//  - **Escape and Android Back cancel it**, through the app's ONE dismissal
//    ladder (`transientLayers.ts`) rather than a keydown listener of its own.
//    An in-flight drag registers a layer, so Escape reaches the drag first and
//    the sheet under it stays open; nothing is applied.
//  - **A cancelled drag unwinds the shared loop**, so the document-wide
//    selection suppression is released at once instead of at the pointer-up
//    that may never come. The cancellation route is the `pointercancel` the
//    platform itself sends — `rowReorder`'s own cleanup already listens for it,
//    so there is one cancellation path and not two.
//
// There is no pointer CAPTURE to lose: the shared loop listens on the document,
// which is what keeps a drag alive when the pointer leaves the row. The
// `lostpointercapture` route is wired anyway, because a host that does capture
// the pointer (a WebView's own long-press takeover) must not leave a drag
// running with nothing driving it.
import { registerTransientLayer } from "../transientLayers";
import { beginRowReorderDrag } from "./rowReorder";

/** The live drop position: which list, which sibling, and which side of it. */
export interface QuerySheetDropTarget {
  /** `data-qs-parent` of the list — a drop indicator in one list must never be
   *  drawn by an item that happens to share an index in another. */
  parent: string;
  index: number;
  before: boolean;
}

/** The selector that matches the items of ONE list, and nothing else. */
export function querySheetSiblingSelector(parent: string): string {
  return `[data-qs-parent="${parent}"]`;
}

let cancelInFlight: (() => void) | null = null;

/** Abandon the drag in flight, if any, without applying it. Called by the
 *  dismissal ladder, and by the sheet when the tree under the drag is replaced
 *  or the sheet goes away. */
export function cancelQuerySheetReorder(): void {
  cancelInFlight?.();
}

export interface QuerySheetReorderRequest {
  /** `data-qs-parent` of the list this drag may reorder. */
  parent: string;
  from: number;
  /** Is the captured tree still the one on screen? A locator is a path into a
   *  ROOT REVISION, not a durable identity, so a drop that lands after the
   *  query was replaced addresses positions that no longer mean what they did.
   *  Such a drop is refused, not remapped. */
  isCurrent: () => boolean;
  setTarget: (target: QuerySheetDropTarget | null) => void;
  /** The index the dragged item ends at, in its own list. Called at most once,
   *  and only for a drop that actually moves it. */
  commit: (to: number) => void;
}

/** Start a reorder drag from an item's HANDLE. Nothing else in the row starts
 *  one: typing in a value, pressing a menu, scrolling and selecting text all
 *  reach their own targets untouched. */
export function beginQuerySheetReorder(event: PointerEvent, request: QuerySheetReorderRequest): void {
  if (event.button !== 0) return;
  cancelQuerySheetReorder();

  let finished = false;
  const finish = () => {
    if (finished) return;
    finished = true;
    if (cancelInFlight === cancel) cancelInFlight = null;
    unregister();
    document.removeEventListener("pointerup", finish);
    document.removeEventListener("pointercancel", finish);
    document.removeEventListener("lostpointercapture", cancel);
    request.setTarget(null);
  };
  const cancel = () => {
    if (finished) return;
    // The same event a real cancellation sends, so the shared loop drops its
    // own listeners and releases the selection guard through its one cleanup.
    document.dispatchEvent(new Event("pointercancel"));
    finish();
  };
  const unregister = registerTransientLayer({
    id: "query-sheet-reorder",
    dismiss: () => {
      cancel();
      return true;
    },
  });
  cancelInFlight = cancel;

  beginRowReorderDrag(
    event,
    request.from,
    querySheetSiblingSelector(request.parent),
    (target) =>
      request.setTarget(
        target ? { parent: request.parent, index: target.index, before: target.before } : null,
      ),
    (_from, to) => {
      if (finished || !request.isCurrent()) return;
      request.commit(to);
    },
  );

  // Registered AFTER the shared loop, on the same target and phase, so its own
  // `pointerup` — which is where the drop is computed and committed — runs
  // first and this only tidies up behind it. The other order silently ate every
  // drop: teardown ran, and the commit that followed found the drag "finished".
  document.addEventListener("pointerup", finish);
  document.addEventListener("pointercancel", finish);
  document.addEventListener("lostpointercapture", cancel);
}
