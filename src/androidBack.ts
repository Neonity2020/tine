import type { SafeCloseCoordinator, SafeClosePrepareResult } from "./safeClose";

export interface AndroidBackPayload {
  canGoBack: boolean;
}

export interface AndroidBackListener {
  unregister(): Promise<void> | void;
}

type AndroidProcessApi = { exit(code?: number): Promise<void> };

/** Exit only after the safe-close coordinator has made graph state durable.
 * Tauri exposes Activity/process exit through plugin-process; plugin:app has no
 * exit command and leaves the transition shield active on Android. */
export async function exitAndroidActivity(
  loadProcess: () => Promise<AndroidProcessApi> = () => import("@tauri-apps/plugin-process"),
): Promise<void> {
  const { exit } = await loadProcess();
  await exit(0);
}

export interface AndroidBackDispatchDeps {
  dismissTransient(): boolean;
  dismissDrawer(): boolean;
  restoreDrawerFocus(): void;
  /** Whether Tine actually went back. The WebView's own `canGoBack` cannot
   * answer this: the mobile router pushes same-URL entries, so its history
   * moves without the address or the entry count changing, and entries that
   * are not Tine's can sit in the same stack. Only the router knows. */
  historyBack(): boolean;
  closeRoot(): void;
}

export type AndroidBackDisposition = "transient" | "drawer" | "history" | "root";

/** Synchronous ordering matters: a hardware Back gesture selects exactly one
 * rung and never synthesizes a KeyboardEvent or a second router back action. */
export function dispatchAndroidBack(
  _payload: AndroidBackPayload,
  deps: AndroidBackDispatchDeps,
): AndroidBackDisposition {
  if (deps.dismissTransient()) return "transient";
  if (deps.dismissDrawer()) {
    deps.restoreDrawerFocus();
    return "drawer";
  }
  // The rung is chosen by whether the router moved, not by the WebView's
  // opinion of its own stack. `canGoBack` was true on a phone whose router had
  // nothing to pop, so Back landed here and silently did nothing, forever.
  if (deps.historyBack()) return "history";
  deps.closeRoot();
  return "root";
}

export interface AndroidBackInstallDeps extends AndroidBackDispatchDeps {
  platform(): Promise<"android" | "ios" | "desktop">;
  subscribe(handler: (payload: AndroidBackPayload) => void): Promise<AndroidBackListener>;
  setupFailed?(error: unknown): void;
}

/** Installs exactly one listener owned by Tine's native SafeBackPlugin. Until
 * setup resolves, after setup rejection, and after cleanup, that native owner
 * consumes Back rather than falling through to WebView history/activity exit. */
export function installAndroidBackHandler(deps: AndroidBackInstallDeps): () => void {
  let disposed = false;
  let listener: AndroidBackListener | null = null;

  void deps.platform()
    .then(async (platform) => {
      if (platform !== "android" || disposed) return null;
      return deps.subscribe((payload) => { dispatchAndroidBack(payload, deps); });
    })
    .then((installed) => {
      if (!installed) return;
      if (disposed) void installed.unregister();
      else listener = installed;
    })
    .catch((error) => deps.setupFailed?.(error));

  return () => {
    if (disposed) return;
    disposed = true;
    const installed = listener;
    listener = null;
    if (installed) void installed.unregister();
  };
}

export type AndroidRootCloseResult =
  | SafeClosePrepareResult
  | "exit_requested"
  | "exit_failed";

/** Once frontend preparation is accepted the graph is durable, and the only
 * remaining step is the activity exit. Direct Files has no native runtime to
 * drain before it (ADR 0066). */
export enum AndroidRootClosePhase {
  Idle = "Idle",
  PreparingFrontend = "PreparingFrontend",
  PreparedAwaitingExit = "PreparedAwaitingExit",
}

interface AndroidRootCloseState {
  phase: AndroidRootClosePhase;
}

export interface AndroidRootCloseCoordinator {
  request(): Promise<AndroidRootCloseResult>;
  phase(): AndroidRootClosePhase;
}

/** Android's root close: the shared safe-close transaction, then the activity
 * exit. A failed exit keeps the transition shield, and a later Back retries
 * only the exit, never the flush. */
export async function requestAndroidRootClose(
  safeClose: SafeCloseCoordinator,
  state: AndroidRootCloseState,
  finishActivity: () => Promise<void>,
  finishActivityFailed: () => void,
): Promise<AndroidRootCloseResult> {
  if (state.phase === AndroidRootClosePhase.PreparedAwaitingExit) {
    return requestAndroidActivityExit(finishActivity, finishActivityFailed);
  }
  if (state.phase !== AndroidRootClosePhase.Idle) return "in_flight";

  state.phase = AndroidRootClosePhase.PreparingFrontend;
  let prepared: SafeClosePrepareResult;
  try {
    prepared = await safeClose.prepare();
  } catch {
    // SafeClose itself releases its shield in its finally block. Keep this
    // coordinator retryable too if a frontend dependency throws unexpectedly.
    state.phase = AndroidRootClosePhase.Idle;
    return "rejected";
  }
  if (prepared !== "accepted") {
    state.phase = AndroidRootClosePhase.Idle;
    return prepared;
  }

  state.phase = AndroidRootClosePhase.PreparedAwaitingExit;
  return requestAndroidActivityExit(finishActivity, finishActivityFailed);
}

async function requestAndroidActivityExit(
  finishActivity: () => Promise<void>,
  finishActivityFailed: () => void,
): Promise<"exit_requested" | "exit_failed"> {
  try {
    await finishActivity();
    return "exit_requested";
  } catch {
    // The graph is already durable. Keep the transition shield in place: a
    // later Back must only retry this activity-exit handoff.
    finishActivityFailed();
    return "exit_failed";
  }
}

export function createAndroidRootCloseCoordinator(
  safeClose: SafeCloseCoordinator,
  {
    finishActivity,
    finishActivityFailed,
  }: {
    finishActivity: () => Promise<void>;
    finishActivityFailed: () => void;
  },
): AndroidRootCloseCoordinator {
  const state: AndroidRootCloseState = { phase: AndroidRootClosePhase.Idle };
  return {
    request: () => requestAndroidRootClose(safeClose, state, finishActivity, finishActivityFailed),
    phase: () => state.phase,
  };
}
