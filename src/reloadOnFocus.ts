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

import { backend, isTauri } from "./backend";
import {
  beginFreshnessBarrier,
  endFreshnessBarrier,
  installFreshnessInputGate,
} from "./freshnessBarrier";
import { managedStorageRuntime } from "./managedStorageRuntime";
import { sweepReplaceable } from "./store";
import { pushToast } from "./ui";

/** Minimum spacing between focus-driven rescans. Below this, returning to the
 *  window is answered by the sweep alone (which is pure in-memory work). */
export const FOCUS_RESCAN_THROTTLE_MS = 1500;

let installed = false;
let lastRescan = 0;
let activeRefresh: Promise<void> | null = null;
let completedSequence = 0;
let completionListener: Promise<void> | null = null;
const completionWaiters = new Map<number, () => void>();
const graphApplications = new Set<Promise<unknown>>();
let verifyPinnedPages: () => Promise<void> = async () => {};

/** App-level final verifier. Native completion proves the backend cache is
 * current, but Tauri does not promise that earlier event callbacks have
 * finished applying to Solid before the completion callback runs. */
export function installFocusFreshnessVerifier(verifier: () => Promise<void>): void {
  verifyPinnedPages = verifier;
}

function ensureCompletionListener(): Promise<void> {
  if (!isTauri()) return Promise.resolve();
  if (!completionListener) {
    completionListener = backend().onGraphRescanComplete((sequence) => {
      completedSequence = Math.max(completedSequence, sequence);
      for (const [target, resolve] of completionWaiters) {
        if (target <= completedSequence) {
          completionWaiters.delete(target);
          resolve();
        }
      }
    }).then(() => undefined);
  }
  return completionListener;
}

function waitForCompletion(sequence: number): Promise<void> {
  if (!isTauri() || sequence <= completedSequence) return Promise.resolve();
  return new Promise<void>((resolve, reject) => {
    completionWaiters.set(sequence, resolve);
    window.setTimeout(() => {
      if (!completionWaiters.delete(sequence)) return;
      reject(new Error(`watcher rescan ${sequence} did not complete`));
    }, 30_000);
  });
}

async function drainGraphApplications(): Promise<void> {
  while (graphApplications.size) {
    await Promise.allSettled([...graphApplications]);
  }
}

/** Track the async application started by one native graph-change event. The
 * rescan completion marker is emitted after those events, but their handlers
 * may still be awaiting page reads; the focus barrier drains them all. */
export function trackGraphChangeApplication(work: Promise<unknown>): void {
  graphApplications.add(work);
  void work.finally(() => graphApplications.delete(work));
}

/** Exported for tests; `installReloadOnFocus` wires it to focus/visibility. */
export function refreshOnReturnToWindow(now = Date.now()): Promise<void> {
  // Always cheap: replay anything already deferred that has become replaceable.
  sweepReplaceable();
  if (activeRefresh) return activeRefresh;
  if (now - lastRescan < FOCUS_RESCAN_THROTTLE_MS) return Promise.resolve();
  lastRescan = now;
  beginFreshnessBarrier();
  activeRefresh = (async () => {
    try {
      await ensureCompletionListener();
      const sequence = await backend().rescanGraphNow();
      await waitForCompletion(sequence);
      await drainGraphApplications();
      await verifyPinnedPages();
      await drainGraphApplications();
      sweepReplaceable();
    } catch (error) {
      // The watcher remains the primary path. Most importantly, a failed
      // fallback must release the input gate rather than strand the editor.
      // Share/join deliberately retires and republishes the managed actor.
      // During that window this rescan is a subordinate probe, not a second
      // operation with its own terminal outcome: the owning Settings command
      // reports exactly one success or failure after the actor is reopened.
      if (!managedStorageRuntime.transitioning()) {
        pushToast(
          `Tine couldn't finish checking for external changes. Editing is available, but reopen the page before relying on it being current. (${String(error)})`,
          "error",
        );
      }
    } finally {
      endFreshnessBarrier();
      activeRefresh = null;
    }
  })();
  return activeRefresh;
}

/** Reset the throttle (tests, and a graph switch — a fresh graph deserves one). */
export function resetFocusRescanThrottle(): void {
  lastRescan = 0;
}

export function installReloadOnFocus(): void {
  if (installed || typeof window === "undefined") return;
  installed = true;
  installFreshnessInputGate();
  window.addEventListener("focus", () => void refreshOnReturnToWindow());
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) void refreshOnReturnToWindow();
  });
}
