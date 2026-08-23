// GH #231: long-press → context menu for touch input. A still hold dispatches
// a SYNTHETIC contextmenu at the held point, so the gesture goes through the
// exact desktop menu path (which preventDefaults — that also suppresses the
// browser's own long-press text-selection, the reporter's complaint). Only
// touch/pen arm it: desktop mice already have right-click.

export const LONG_PRESS_DELAY = 500; // ms — the conventional hold time
export const LONG_PRESS_MOVE_TOLERANCE = 10; // px — beyond it, the hold is a scroll/drag

export interface LongPressHandlers {
  onPointerDown(e: PointerEvent): void;
  onPointerMove(e: PointerEvent): void;
  onPointerUp(e: PointerEvent): void;
  onPointerCancel(e: PointerEvent): void;
  /** Consume the compatibility click emitted when a completed hold releases. */
  consumeClick(): boolean;
  dispose(): void;
}

/** Attach the returned handlers to the anchor/interactive element. When a
 *  primary touch/pen press stays within tolerance for the full delay, a
 *  contextmenu event fires AT THAT ELEMENT with the press coordinates, so a
 *  listening menu handler runs its ordinary desktop behavior (including
 *  preventDefault, which suppresses native selection/callout for this hold).
 *  A quick tap, any larger movement, pointer-cancel, or unmount cancels the
 *  gesture — ordinary scroll, tap, and text selection elsewhere are untouched
 *  because nothing is bound until the press starts, and everything clears the
 *  moment it ends. */
export function createLongPress(target: () => HTMLElement | undefined): LongPressHandlers {
  let armed: { id: number; x: number; y: number } | null = null;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let firedPointer: number | null = null;
  let clearSuppressionTimer: ReturnType<typeof setTimeout> | null = null;
  let suppressClick = false;
  const cancel = () => {
    armed = null;
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  };
  const clearSuppression = () => {
    suppressClick = false;
    if (clearSuppressionTimer !== null) {
      clearTimeout(clearSuppressionTimer);
      clearSuppressionTimer = null;
    }
  };
  return {
    onPointerDown(e: PointerEvent) {
      if (!e.isPrimary || (e.pointerType !== "touch" && e.pointerType !== "pen")) return;
      cancel();
      const armedNow = { id: e.pointerId, x: e.clientX, y: e.clientY };
      armed = armedNow;
      timer = setTimeout(() => {
        // Firing is terminal for THIS gesture; release later does nothing more.
        cancel();
        firedPointer = armedNow.id;
        const el = target();
        if (!el) return;
        el.dispatchEvent(
          new MouseEvent("contextmenu", {
            bubbles: true,
            cancelable: true,
            clientX: armedNow.x,
            clientY: armedNow.y,
          }),
        );
      }, LONG_PRESS_DELAY);
    },
    onPointerMove(e: PointerEvent) {
      if (!armed || e.pointerId !== armed.id) return;
      const dx = e.clientX - armed.x;
      const dy = e.clientY - armed.y;
      if (Math.abs(dx) > LONG_PRESS_MOVE_TOLERANCE || Math.abs(dy) > LONG_PRESS_MOVE_TOLERANCE) cancel();
    },
    onPointerUp(e: PointerEvent) {
      if (firedPointer === e.pointerId) {
        firedPointer = null;
        suppressClick = true;
        // Compatibility `click` follows pointerup immediately. Keep the guard
        // briefly, then release it if this WebView emits no click.
        clearSuppressionTimer = setTimeout(clearSuppression, 250);
        e.preventDefault();
        e.stopPropagation();
      }
      cancel();
    },
    onPointerCancel(e: PointerEvent) {
      if (firedPointer === e.pointerId) firedPointer = null;
      cancel();
    },
    consumeClick() {
      if (!suppressClick) return false;
      clearSuppression();
      return true;
    },
    dispose() {
      cancel();
      firedPointer = null;
      clearSuppression();
    },
  };
}
