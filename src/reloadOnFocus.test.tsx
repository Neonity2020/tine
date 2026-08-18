import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { backend } from "./backend";
import {
  FOCUS_RESCAN_THROTTLE_MS,
  installReloadOnFocus,
  refreshOnReturnToWindow,
  resetFocusRescanThrottle,
} from "./reloadOnFocus";
import { onPageBecameReplaceable, resetStore, sweepReplaceable } from "./store";

// Concord P5 (L0): when the OS or filesystem gave us no event — a network
// mount, a sync client writing through a path the kernel doesn't report, an app
// the OS suspended — returning to the window is the only signal left. It must
// ask, and it must ask through the EXISTING machinery: the P1 replaceable-page
// net plus one backend stat diff whose findings arrive as ordinary
// graph-changed events. Nothing here touches a page directly.

beforeEach(() => {
  resetFocusRescanThrottle();
});

afterEach(() => {
  vi.restoreAllMocks();
  resetStore();
});

describe("reload on focus", () => {
  it("asks the backend for one rescan and replays what P1 deferred", () => {
    const rescan = vi.spyOn(backend(), "rescanGraphNow").mockResolvedValue(undefined);
    const replayed: string[] = [];
    // A page the store already considers replaceable: exactly the record P1's
    // `deferExternalReload` leaves behind while a block is being edited.
    onPageBecameReplaceable("Externally Edited", (name) => replayed.push(name));

    refreshOnReturnToWindow(100_000);

    expect(rescan).toHaveBeenCalledTimes(1);
    expect(replayed).toEqual(["Externally Edited"]);
  });

  it("throttles the rescan but never the replay", () => {
    const rescan = vi.spyOn(backend(), "rescanGraphNow").mockResolvedValue(undefined);
    const replayed: string[] = [];
    onPageBecameReplaceable("Externally Edited", (name) => replayed.push(name));

    refreshOnReturnToWindow(100_000);
    refreshOnReturnToWindow(100_000 + FOCUS_RESCAN_THROTTLE_MS - 1);
    expect(rescan).toHaveBeenCalledTimes(1);
    expect(replayed.length).toBe(2); // alt-tabbing never costs a stale page

    refreshOnReturnToWindow(100_000 + FOCUS_RESCAN_THROTTLE_MS);
    expect(rescan).toHaveBeenCalledTimes(2);
  });

  it("survives a backend that refuses the rescan", async () => {
    vi.spyOn(backend(), "rescanGraphNow").mockRejectedValue(new Error("no watcher"));
    expect(() => refreshOnReturnToWindow(100_000)).not.toThrow();
    await Promise.resolve();
  });

  it("fires on window focus and on becoming visible, never while hidden", () => {
    const rescan = vi.spyOn(backend(), "rescanGraphNow").mockResolvedValue(undefined);
    installReloadOnFocus();

    window.dispatchEvent(new Event("focus"));
    expect(rescan).toHaveBeenCalledTimes(1);

    resetFocusRescanThrottle();
    const hidden = vi.spyOn(document, "hidden", "get").mockReturnValue(true);
    document.dispatchEvent(new Event("visibilitychange"));
    expect(rescan).toHaveBeenCalledTimes(1); // going away is not a refresh

    hidden.mockReturnValue(false);
    document.dispatchEvent(new Event("visibilitychange"));
    expect(rescan).toHaveBeenCalledTimes(2);
  });

  it("installs exactly once", () => {
    const rescan = vi.spyOn(backend(), "rescanGraphNow").mockResolvedValue(undefined);
    installReloadOnFocus();
    installReloadOnFocus();
    window.dispatchEvent(new Event("focus"));
    expect(rescan).toHaveBeenCalledTimes(1);
  });

  it("does not replace the store's own sweep", () => {
    // The sweep stays a pure store concern; the focus path only calls it.
    const replayed: string[] = [];
    onPageBecameReplaceable("Page", (name) => replayed.push(name));
    sweepReplaceable();
    expect(replayed).toEqual(["Page"]);
  });
});
