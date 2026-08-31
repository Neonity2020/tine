// When does a session END, for the purposes of "Tine did not close cleanly"?
//
// GH #426. The recorder writes a `session-active` marker at startup and removes
// it at the one orderly end it knows about — the desktop event loop's
// `RunEvent::Exit`. Mobile has no such moment: iOS and Android suspend a
// backgrounded app and reap it later without notice, so the marker always
// survived and every single launch opened with the sticky warning. A reporter
// put it exactly: "It does happen every time in iOS/iPadOS. The OS kills the
// app when in background so I am having this issue all the time."
//
// So on mobile the recorded session follows visibility: it ends when the app is
// hidden (from then on, being reaped is expected) and restarts when the user
// comes back (a crash they actually witness is still worth reporting). This is
// the same lifecycle edge `backgroundFlush.ts` uses to get the user's typing to
// disk, and for the same reason — it is the last moment we are certain to get.
//
// Desktop is deliberately excluded: a minimised or occluded window is still a
// running session, and honouring visibility there would silently hide the
// background crashes the recorder exists to catch.

export interface SessionActivityDeps {
  /** Tell the backend whether the session should count as live from now on. */
  setActive(active: boolean): void;
  /** Android or iOS. Desktop installs nothing at all. */
  isMobile: boolean;
  isHidden?(): boolean;
  addEventListener?: typeof document.addEventListener;
  removeEventListener?: typeof document.removeEventListener;
}

/** `pagehide`/`freeze` can fire while `visibilityState` still reads "visible",
 *  and mobile WebViews are inconsistent about which of the three they deliver,
 *  so each event carries its own verdict instead of re-reading the document. */
const HIDE_EVENTS = ["pagehide", "freeze"] as const;
const SHOW_EVENTS = ["pageshow", "resume"] as const;

export function installSessionActivity(deps: SessionActivityDeps): () => void {
  if (!deps.isMobile) return () => {};
  const add = deps.addEventListener ?? document.addEventListener.bind(document);
  const remove = deps.removeEventListener ?? document.removeEventListener.bind(document);
  const isHidden = deps.isHidden
    ?? (() => typeof document === "undefined" || document.visibilityState !== "visible");

  // The backend already armed the marker at startup, and these events fire far
  // more often than the state changes (a single background can deliver
  // `visibilitychange` and `pagehide`). Only edges are worth an IPC round trip.
  let active = true;
  const set = (next: boolean) => {
    if (next === active) return;
    active = next;
    deps.setActive(next);
  };

  const onVisibility = () => set(!isHidden());
  const onHide = () => set(false);
  const onShow = () => set(true);

  add("visibilitychange", onVisibility);
  for (const event of HIDE_EVENTS) add(event, onHide);
  for (const event of SHOW_EVENTS) add(event, onShow);
  return () => {
    remove("visibilitychange", onVisibility);
    for (const event of HIDE_EVENTS) remove(event, onHide);
    for (const event of SHOW_EVENTS) remove(event, onShow);
  };
}
