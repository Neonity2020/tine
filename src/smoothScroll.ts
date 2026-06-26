// EXPERIMENTAL, opt-in (Settings → Appearance, default OFF). WebKitGTK never
// synthesizes scroll momentum — each mouse-wheel notch jumps a fixed ~90px with
// no acceleration and no coast (see tine-rendering-perf / webkit-scroll-research).
// Lenis re-animates those discrete jumps into continuous motion. We tune it for
// *smoothing*, not inertia (high lerp ⇒ it catches up fast, minimal coast), since
// the complaint is the stepped feel, not a wish for long gliding.
//
// Scoped to the journal feed scroller (`.main-content`) so it never hijacks the
// sidebar / modals / PDF pane (all siblings, outside the wrapper). The one
// in-feed scroller — the autocomplete dropdown — carries `data-lenis-prevent`.
// Designed to be trivially revertible: delete this file + its Settings toggle +
// the `lenis` dep, and Tine is back to native scrolling.

import { createSignal } from "solid-js";
import Lenis from "lenis";
import { backend } from "./backend";

const [enabled, setEnabled] = createSignal(false);
/** Reactive: is smooth scrolling currently on? (drives the Settings toggle) */
export const smoothScrollEnabled = enabled;

let lenis: Lenis | null = null;
let rafId = 0;

function install(): void {
  if (lenis) return;
  const wrapper = document.querySelector<HTMLElement>(".main-content");
  const content = wrapper?.querySelector<HTMLElement>(".main-content-inner");
  if (!wrapper || !content) return; // feed not mounted yet — apply() retries on next toggle
  lenis = new Lenis({
    wrapper,
    content,
    smoothWheel: true,
    // lerp = per-frame catch-up toward the target (fps-normalized). Higher =
    // snappier / less coast. 0.2 smooths the ~90px steps but settles quickly —
    // this is THE knob to tune if it feels too floaty (raise) or too abrupt (lower).
    lerp: 0.2,
    wheelMultiplier: 1, // don't change scroll *speed*, only smooth it
  });
  const loop = (t: number) => {
    lenis?.raf(t);
    rafId = requestAnimationFrame(loop);
  };
  rafId = requestAnimationFrame(loop);
}

function destroy(): void {
  if (rafId) cancelAnimationFrame(rafId);
  rafId = 0;
  lenis?.destroy();
  lenis = null;
}

function apply(on: boolean): void {
  setEnabled(on);
  if (on) install();
  else destroy();
}

/** Toggle from the UI: persist the choice, then apply it live. */
export function setSmoothScroll(on: boolean): void {
  apply(on);
  void backend().setSmoothScroll(on).catch(() => {});
}

/** Read the persisted preference at startup and apply it. Default OFF. */
export async function initSmoothScroll(): Promise<void> {
  try {
    apply(await backend().getSmoothScroll());
  } catch {
    /* default off */
  }
}
