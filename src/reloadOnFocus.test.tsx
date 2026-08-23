import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { backend } from "./backend";
import {
  FOCUS_RESCAN_THROTTLE_MS,
  installFocusFreshnessVerifier,
  installReloadOnFocus,
  refreshOnReturnToWindow,
  resetFocusRescanThrottle,
} from "./reloadOnFocus";
import { onPageBecameReplaceable, resetStore, sweepReplaceable } from "./store";
import { managedStorageRuntime } from "./managedStorageRuntime";
import { setToasts, toasts } from "./ui";

// Concord P5 (L0): when the OS or filesystem gave us no event — a network
// mount, a sync client writing through a path the kernel doesn't report, an app
// the OS suspended — returning to the window is the only signal left. It must
// ask, and it must ask through the EXISTING machinery: the P1 replaceable-page
// net plus one backend stat diff whose findings arrive as ordinary
// graph-changed events. Nothing here touches a page directly.

beforeEach(() => {
  resetFocusRescanThrottle();
  setToasts([]);
});

afterEach(() => {
  managedStorageRuntime.clear();
  setToasts([]);
  installFocusFreshnessVerifier(async () => {});
  vi.restoreAllMocks();
  resetStore();
});

describe("reload on focus", () => {
  it("asks the backend for one rescan and replays what P1 deferred", async () => {
    const rescan = vi.spyOn(backend(), "rescanGraphNow").mockResolvedValue(1);
    const replayed: string[] = [];
    // A page the store already considers replaceable: exactly the record P1's
    // `deferExternalReload` leaves behind while a block is being edited.
    onPageBecameReplaceable("Externally Edited", (name) => replayed.push(name));

    await refreshOnReturnToWindow(100_000);

    expect(rescan).toHaveBeenCalledTimes(1);
    expect(replayed).toEqual(["Externally Edited", "Externally Edited"]);
  });

  it("throttles the rescan but never the replay", async () => {
    const rescan = vi.spyOn(backend(), "rescanGraphNow").mockResolvedValue(1);
    const replayed: string[] = [];
    onPageBecameReplaceable("Externally Edited", (name) => replayed.push(name));

    await refreshOnReturnToWindow(100_000);
    await refreshOnReturnToWindow(100_000 + FOCUS_RESCAN_THROTTLE_MS - 1);
    expect(rescan).toHaveBeenCalledTimes(1);
    expect(replayed.length).toBe(3); // full refresh sweeps before and after; throttled refresh still sweeps

    await refreshOnReturnToWindow(100_000 + FOCUS_RESCAN_THROTTLE_MS);
    expect(rescan).toHaveBeenCalledTimes(2);
  });

  it("survives a backend that refuses the rescan", async () => {
    vi.spyOn(backend(), "rescanGraphNow").mockRejectedValue(new Error("no watcher"));
    await expect(refreshOnReturnToWindow(100_000)).resolves.toBeUndefined();
  });

  it("leaves the terminal outcome to an active storage transition", async () => {
    vi.spyOn(backend(), "rescanGraphNow").mockRejectedValue(
      new Error("sync actor is unavailable"),
    );
    managedStorageRuntime.beginTransition();
    await expect(refreshOnReturnToWindow(100_000)).resolves.toBeUndefined();
    expect(toasts()).toEqual([]);
    managedStorageRuntime.endTransition();
  });

  it("awaits the bounded visible-page verifier before completing", async () => {
    vi.spyOn(backend(), "rescanGraphNow").mockResolvedValue(1);
    let release = () => {};
    const pending = new Promise<void>((resolve) => { release = resolve; });
    const verifier = vi.fn(() => pending);
    installFocusFreshnessVerifier(verifier);

    let completed = false;
    const refresh = refreshOnReturnToWindow(100_000).then(() => { completed = true; });
    await vi.waitFor(() => expect(verifier).toHaveBeenCalledTimes(1));
    expect(completed).toBe(false);

    release();
    await refresh;
    expect(completed).toBe(true);
  });

  it("fires on window focus and on becoming visible, never while hidden", async () => {
    const rescan = vi.spyOn(backend(), "rescanGraphNow").mockResolvedValue(1);
    installReloadOnFocus();

    window.dispatchEvent(new Event("focus"));
    await vi.waitFor(() => expect(rescan).toHaveBeenCalledTimes(1));
    await Promise.resolve();

    resetFocusRescanThrottle();
    const hidden = vi.spyOn(document, "hidden", "get").mockReturnValue(true);
    document.dispatchEvent(new Event("visibilitychange"));
    expect(rescan).toHaveBeenCalledTimes(1); // going away is not a refresh

    hidden.mockReturnValue(false);
    document.dispatchEvent(new Event("visibilitychange"));
    await vi.waitFor(() => expect(rescan).toHaveBeenCalledTimes(2));
  });

  it("installs exactly once", async () => {
    const rescan = vi.spyOn(backend(), "rescanGraphNow").mockResolvedValue(1);
    installReloadOnFocus();
    installReloadOnFocus();
    window.dispatchEvent(new Event("focus"));
    await vi.waitFor(() => expect(rescan).toHaveBeenCalledTimes(1));
  });

  it("does not replace the store's own sweep", () => {
    // The sweep stays a pure store concern; the focus path only calls it.
    const replayed: string[] = [];
    onPageBecameReplaceable("Page", (name) => replayed.push(name));
    sweepReplaceable();
    expect(replayed).toEqual(["Page"]);
  });
});
