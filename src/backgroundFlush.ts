// Durability barrier for "the app is going away and nobody asked us politely".
//
// GH #255 ("Notes are randomly lost on Android and Windows 11"). Until now the
// ONLY thing that flushed pending edits was `onCloseRequested` — a clean desktop
// window close. Android and iOS do not send one: the OS backgrounds the app and
// may reclaim it at any point afterwards, without notice. Desktop has narrower
// versions of the same hole (SIGKILL, OOM, session teardown, a Windows shutdown
// that never delivers a close request).
//
// Combined with the 400 ms save debounce, that means everything the user typed
// since their last pause is in RAM only when the process dies. The 2026-08-09
// Direct Files data-safety audit called this the plainest available explanation
// for the Android half of #255.
//
// The three `visibilitychange` listeners already in the tree all act on becoming
// *visible* (refreshing assets, journals, the page). This is the other edge.

export interface BackgroundFlushDeps {
  /** Commit whatever the focused editor is holding into the store first, so the
   *  in-flight keystroke is part of what gets written. */
  endEdit(): void;
  /** Persist every dirty page. Resolves false if something could not be saved. */
  flushAll(): Promise<boolean>;
  /** True when a flush is already running for a close transaction — the close
   *  path owns the outcome (it can prompt); we must not race it. */
  closeInFlight(): boolean;
  /** Injected so this module owns no globals and is testable in the node pool.
   *  `visibilitychange` fires on both edges; only hiding is a durability event. */
  isHidden?(): boolean;
  addEventListener?: typeof document.addEventListener;
  removeEventListener?: typeof document.removeEventListener;
}

/** Hidden means "may never come back". `pagehide` fires on teardown paths where
 *  `visibilitychange` does not, and Android/iOS WebViews are inconsistent about
 *  which they deliver, so listen for both and let the in-flight guard dedupe. */
const TRIGGERS = ["visibilitychange", "pagehide", "freeze"] as const;

export function installBackgroundFlush(deps: BackgroundFlushDeps): () => void {
  const add = deps.addEventListener ?? document.addEventListener.bind(document);
  const remove = deps.removeEventListener ?? document.removeEventListener.bind(document);
  const isHidden = deps.isHidden
    ?? (() => typeof document === "undefined" || document.visibilityState !== "visible");
  let inFlight = false;

  const flush = () => {
    if (!isHidden()) return;
    if (inFlight || deps.closeInFlight()) return;
    inFlight = true;
    // Deliberately fire-and-forget. There is no way to hold a WebView open while
    // a promise settles, and blocking the event would not buy time — the value
    // is entirely in *starting* the write before the process is reclaimed.
    // Failures stay dirty and are reported by the ordinary save path.
    try {
      deps.endEdit();
      void deps.flushAll().catch(() => {}).finally(() => { inFlight = false; });
    } catch {
      inFlight = false;
    }
  };

  for (const event of TRIGGERS) add(event, flush);
  return () => {
    for (const event of TRIGGERS) remove(event, flush);
  };
}
