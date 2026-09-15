import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { backend, PublishedExportReadOnlyError } from "./backend";
import {
  FOCUS_RESCAN_THROTTLE_MS,
  installFocusFreshnessVerifier,
  installReloadOnFocus,
  refreshOnReturnToWindow,
  resetFocusRescanThrottle,
} from "./reloadOnFocus";
import { onPageBecameReplaceable, resetStore, sweepReplaceable } from "./store";
import { graphBindingRuntime } from "./graphBindingRuntime";
import { setToasts, toasts } from "./ui";
import { PUBLISHED_META_NAME } from "./publishedBackend";
import { setGraphMeta } from "./ui";
import { resetSaveState } from "./persistence";

// Concord P5 (L0): when the OS or filesystem gave us no event — a network
// mount, a sync client writing through a path the kernel doesn't report, an app
// the OS suspended — returning to the window is the only signal left. It must
// ask, and it must ask through the EXISTING machinery: the P1 replaceable-page
// net plus one backend stat diff whose findings arrive as ordinary
// graph-changed events. Nothing here touches a page directly.

const flushMicrotasks = () => new Promise((resolve) => setTimeout(resolve, 0));

beforeEach(() => {
  resetFocusRescanThrottle();
  setGraphMeta({ root: "/graphs/A" } as never);
  setToasts([]);
});

afterEach(() => {
  graphBindingRuntime.clear();
  setToasts([]);
  setGraphMeta(null);
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

  it("coalesces a same-graph focus during an in-flight rescan into that rescan", async () => {
    // Fail-before (wave-2 review D2): the replacement decision was made after
    // the in-flight refresh settled and nulled its binding, so every coalesced
    // same-graph focus scheduled a second full stat-diff of the graph.
    let finish!: (sequence: number) => void;
    const rescan = vi.spyOn(backend(), "rescanGraphNow")
      .mockImplementation(() => new Promise<number>((resolve) => { finish = resolve; }));

    const first = refreshOnReturnToWindow(100_000);
    await vi.waitFor(() => expect(rescan).toHaveBeenCalledTimes(1));
    const second = refreshOnReturnToWindow(100_001);
    finish(1);
    await first;
    await second;
    await flushMicrotasks();

    expect(rescan).toHaveBeenCalledTimes(1);
  });

  it("drops the old graph tail and queues one non-overlapping refresh for the new binding", async () => {
    let finishOld!: (sequence: number) => void;
    const rescan = vi.spyOn(backend(), "rescanGraphNow")
      .mockImplementationOnce(() => new Promise<number>((resolve) => { finishOld = resolve; }))
      .mockResolvedValueOnce(2);
    const verifier = vi.fn(async () => {});
    installFocusFreshnessVerifier(verifier);

    const old = refreshOnReturnToWindow(100_000);
    await vi.waitFor(() => expect(rescan).toHaveBeenCalledTimes(1));
    resetSaveState();
    setGraphMeta({ root: "/graphs/B" } as never);
    const replacement = refreshOnReturnToWindow(100_001);
    finishOld(1);

    await old;
    await replacement;
    await vi.waitFor(() => expect(rescan).toHaveBeenCalledTimes(2));
    expect(verifier).toHaveBeenCalledTimes(1);
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

  // GH #549: a published export is a read-only snapshot with no watcher behind
  // it, and its backend refuses `rescanGraphNow`. Returning to the tab asked
  // anyway, so every reader got "couldn't finish checking for external changes
  // … Editing is available" on each refocus, in an app with no editing at all.
  it("does nothing on focus in a published export", async () => {
    const meta = document.createElement("meta");
    meta.name = PUBLISHED_META_NAME;
    meta.content = "snapshot.json";
    document.head.append(meta);
    try {
      const rescan = vi.spyOn(backend(), "rescanGraphNow").mockRejectedValue(new PublishedExportReadOnlyError());
      installReloadOnFocus();
      window.dispatchEvent(new Event("focus"));
      await refreshOnReturnToWindow(10_000_000);
      await flushMicrotasks();

      expect(rescan).not.toHaveBeenCalled();
      expect(toasts()).toEqual([]);
    } finally {
      meta.remove();
    }
  });

  it("does not replace the store's own sweep", () => {
    // The sweep stays a pure store concern; the focus path only calls it.
    const replayed: string[] = [];
    onPageBecameReplaceable("Page", (name) => replayed.push(name));
    sweepReplaceable();
    expect(replayed).toEqual(["Page"]);
  });
});
