// Concord L0 — the reload-on-focus fallback (IntelliJ/VS Code's trick).
//
// The watcher is the primary freshness path and stays so. But some filesystems
// and sync clients deliver no event at all — network mounts, a client writing
// through a path the kernel does not report, an app the OS suspended while the
// user was elsewhere — and then a page can sit stale indefinitely. The one
// signal always available is the user coming back to the window.
//
// Two things happen on return, and NEITHER is a new freshness path:
//
//  1. `sweepReplaceable()` — the P1 net. A reload deferred while a block was
//     being edited fires when the page announces it became replaceable; if the
//     user alt-tabbed away mid-edit, that announcement may never have come.
//     The sweep re-checks every watched page against the same gate.
//  2. `rescanGraphNow()` — asks the BACKEND watcher for one full stat diff.
//     Anything it finds is emitted as ordinary `graph-changed` events, so the
//     disposition, the divergence proof and the deferred replay all apply
//     exactly as for a live event. A caret is never stolen here.
//
// Throttled, because window focus is a gesture a user makes constantly, and a
// rescan is one stat per graph-text file.

import { backend } from "./backend";
import { sweepReplaceable } from "./store";

/** Minimum spacing between focus-driven rescans. Below this, returning to the
 *  window is answered by the sweep alone (which is pure in-memory work). */
export const FOCUS_RESCAN_THROTTLE_MS = 1500;

let installed = false;
let lastRescan = 0;

/** Exported for tests; `installReloadOnFocus` wires it to focus/visibility. */
export function refreshOnReturnToWindow(now = Date.now()): void {
  // Always cheap: replay anything already deferred that has become replaceable.
  sweepReplaceable();
  if (now - lastRescan < FOCUS_RESCAN_THROTTLE_MS) return;
  lastRescan = now;
  void backend().rescanGraphNow().catch(() => {
    /* best-effort: the watcher is still the primary path */
  });
}

/** Reset the throttle (tests, and a graph switch — a fresh graph deserves one). */
export function resetFocusRescanThrottle(): void {
  lastRescan = 0;
}

export function installReloadOnFocus(): void {
  if (installed || typeof window === "undefined") return;
  installed = true;
  window.addEventListener("focus", () => refreshOnReturnToWindow());
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) refreshOnReturnToWindow();
  });
}
