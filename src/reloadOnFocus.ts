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
import { graphBinding } from "./persistence";
import { captureGraphScope, isScopeCurrent, type GraphScope } from "./landAsync";

/** Minimum spacing between focus-driven rescans. Below this, returning to the
 *  window is answered by the sweep alone (which is pure in-memory work). */
export const FOCUS_RESCAN_THROTTLE_MS = 1500;

let installed = false;
let lastRescan = 0;
let activeRefresh: Promise<void> | null = null;
let activeRefreshBinding: number | null = null;
let queuedBindingRefresh = false;
let stateBinding = graphBinding();
let completedSequence = 0;
let completionListener: Promise<void> | null = null;
const completionWaiters = new Map<number, {
  binding: number;
  resolve: () => void;
  reject: (error: Error) => void;
}>();
const graphApplications = new Map<Promise<unknown>, number>();
let verifyPinnedPages: () => Promise<void> = async () => {};

class StaleFocusRefresh extends Error {}

function requireCurrent(scope: GraphScope): void {
  if (!isScopeCurrent(scope)) throw new StaleFocusRefresh();
}

function retireChangedBinding(): boolean {
  const binding = graphBinding();
  if (binding === stateBinding) return false;
  stateBinding = binding;
  lastRescan = 0;
  completedSequence = 0;
  for (const waiter of completionWaiters.values()) {
    waiter.reject(new StaleFocusRefresh());
  }
  completionWaiters.clear();
  graphApplications.clear();
  if (activeRefresh && activeRefreshBinding !== binding) queuedBindingRefresh = true;
  return true;
}

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
      for (const [target, waiter] of completionWaiters) {
        if (waiter.binding === stateBinding && target <= completedSequence) {
          completionWaiters.delete(target);
          waiter.resolve();
        }
      }
    }).then(() => undefined);
  }
  return completionListener;
}

function waitForCompletion(sequence: number): Promise<void> {
  if (!isTauri() || sequence <= completedSequence) return Promise.resolve();
  return new Promise<void>((resolve, reject) => {
    const waiter = { binding: stateBinding, resolve, reject };
    completionWaiters.set(sequence, waiter);
    window.setTimeout(() => {
      if (completionWaiters.get(sequence) !== waiter) return;
      completionWaiters.delete(sequence);
      reject(new Error(`watcher rescan ${sequence} did not complete`));
    }, 30_000);
  });
}

async function drainGraphApplications(scope: GraphScope): Promise<void> {
  for (;;) {
    const current = [...graphApplications]
      .filter(([, binding]) => binding === scope.binding)
      .map(([work]) => work);
    if (!current.length) return;
    await Promise.allSettled(current);
    requireCurrent(scope);
  }
}

/** Track the async application started by one native graph-change event. The
 * rescan completion marker is emitted after those events, but their handlers
 * may still be awaiting page reads; the focus barrier drains them all. */
export function trackGraphChangeApplication(work: Promise<unknown>): void {
  graphApplications.set(work, graphBinding());
  void work.finally(() => graphApplications.delete(work));
}

/** Exported for tests; `installReloadOnFocus` wires it to focus/visibility. */
export function refreshOnReturnToWindow(now = Date.now()): Promise<void> {
  // Always cheap: replay anything already deferred that has become replaceable.
  sweepReplaceable();
  const bindingChanged = retireChangedBinding();
  if (activeRefresh) {
    if (bindingChanged || activeRefreshBinding !== stateBinding) queuedBindingRefresh = true;
    const requestedBinding = stateBinding;
    return activeRefresh.then(() => {
      if (activeRefresh && activeRefreshBinding === requestedBinding) return activeRefresh;
      if (queuedBindingRefresh || activeRefreshBinding !== requestedBinding) {
        return refreshOnReturnToWindow(now + FOCUS_RESCAN_THROTTLE_MS);
      }
    });
  }
  if (now - lastRescan < FOCUS_RESCAN_THROTTLE_MS) return Promise.resolve();
  const scope = captureGraphScope();
  if (!scope) return Promise.resolve();
  lastRescan = now;
  beginFreshnessBarrier();
  activeRefreshBinding = scope.binding;
  let refresh!: Promise<void>;
  refresh = (async () => {
    try {
      await ensureCompletionListener();
      requireCurrent(scope);
      const sequence = await backend().rescanGraphNow();
      requireCurrent(scope);
      await waitForCompletion(sequence);
      requireCurrent(scope);
      await drainGraphApplications(scope);
      requireCurrent(scope);
      await verifyPinnedPages();
      requireCurrent(scope);
      await drainGraphApplications(scope);
      requireCurrent(scope);
      sweepReplaceable();
    } catch (error) {
      if (error instanceof StaleFocusRefresh) return;
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
      if (activeRefresh === refresh) {
        activeRefresh = null;
        activeRefreshBinding = null;
      }
      if (queuedBindingRefresh) {
        queuedBindingRefresh = false;
        queueMicrotask(() => void refreshOnReturnToWindow());
      }
    }
  })();
  activeRefresh = refresh;
  return activeRefresh;
}

/** Reset only the time throttle. Graph switches are detected by graph binding;
 * they also retire completion/application state and queue a non-overlapping
 * refresh for the new graph. */
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
