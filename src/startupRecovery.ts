import { createSignal, type Accessor } from "solid-js";
import type { LoadGraphPathOutcome } from "./graph";
import { safeManagedErrorDetail } from "./managedDiagnostics";
import type { SparseV2CancelResult, StartupProgressEvent } from "./types";

export const STARTUP_LOOKUP_WATCHDOG_MS = 2_000;
export const STARTUP_PROGRESS_VISIBLE_MS = 250;
export const STARTUP_ACTION_WATCHDOG_MS = 15_000;

export type StartupOperation = "lookup" | "graph_open" | "graph_picker" | "cold_return";

export interface StartupRecoverySnapshot {
  mode: "idle" | "working" | "recovery";
  operation: StartupOperation | null;
  /** Frontend continuation generation; user actions invalidate late promises. */
  attempt: number;
  /** Native lookup authority token retained for a cold recovery command. */
  nativeAttempt: number;
  phase: string;
  nativePhase: string | null;
  startedAt: number;
  elapsedMs: number;
  nativeElapsedMs: number | null;
  target: string;
  detail: string | null;
}

export interface StartupRecoveryDeps {
  lookupGraphPath(attempt: number): Promise<string | null>;
  injectedGraphPath(): string;
  persistedGraphPath(): string;
  openGraph(path: string): Promise<LoadGraphPathOutcome>;
  pickGraph(): Promise<LoadGraphPathOutcome>;
  coldReturn(path: string, attempt: number): Promise<SparseV2CancelResult>;
  acceptColdReturn(result: SparseV2CancelResult): void;
  confirmColdReturn(graphName: string): Promise<boolean>;
  copyText(text: string): Promise<void>;
  notify(message: string, kind: "success" | "error"): void;
  completeFirstLoad(): void;
  now?: () => number;
  watchdogMs?: number;
  actionWatchdogMs?: number;
}

const idleSnapshot = (): StartupRecoverySnapshot => ({
  mode: "idle",
  operation: null,
  attempt: 0,
  nativeAttempt: 0,
  phase: "idle",
  nativePhase: null,
  startedAt: 0,
  elapsedMs: 0,
  nativeElapsedMs: null,
  target: "",
  detail: null,
});

export function startupGraphName(path: string): string {
  const trimmed = path.replace(/[\\/]+$/u, "");
  return trimmed.split(/[\\/]/u).at(-1) || "remembered graph";
}

export function startupPhaseLabel(phase: string): string {
  const known: Record<string, string> = {
    "lookup.starting": "Finding the last workspace",
    "lookup.entry": "Starting workspace lookup",
    "lookup.app_data": "Locating app settings",
    "lookup.settings_stat": "Checking app settings",
    "lookup.settings_read": "Reading app settings",
    "lookup.settings_parse": "Reading the remembered workspace",
    "lookup.complete": "Workspace lookup complete",
    "lookup.timeout": "Workspace lookup is not responding",
    "lookup.failed": "Workspace lookup failed",
    "native.unavailable": "Native recovery is not responding",
    "graph.access": "Checking workspace access",
    "graph.failed": "Workspace open failed",
    "picker.open": "Waiting for a workspace selection",
    "direct.confirm": "Waiting for confirmation",
    "direct.archive": "Archiving managed state before Direct Files",
    "cold_return.waiting_for_graph_transition": "Finishing the current storage operation before Direct Files",
    "cold_return.verifying_target": "Verifying the workspace before Direct Files",
    "cold_return.quarantining_managed_selection": "Selecting the current Markdown tree as Direct Files",
    "cold_return.archiving_managed_state": "Archiving managed state before Direct Files",
    "cold_return.opening_direct_files": "Opening Direct Files",
    "cold_return.complete": "Direct Files return complete",
  };
  if (known[phase]) return known[phase];
  if (phase.startsWith("managed_open.")) {
    const tail = phase.slice("managed_open.".length);
    if (tail === "complete") return "Managed storage open complete";
    return `Opening managed storage: ${tail.replace(/[._]+/gu, " ")}`;
  }
  return phase.replace(/[._]+/gu, " ");
}

const LOOKUP_PROGRESS_PHASES = new Set([
  "lookup.entry",
  "lookup.app_data",
  "lookup.settings_stat",
  "lookup.settings_read",
  "lookup.settings_parse",
  "lookup.complete",
]);

/** Runtime boundary for the native event bus: phase text is displayed/copied. */
export function validStartupProgress(progress: StartupProgressEvent): boolean {
  const phase = progress.phase as string;
  const validPhase = LOOKUP_PROGRESS_PHASES.has(phase)
    || /^managed_open\.[a-z0-9_]{1,64}$/u.test(phase)
    || /^cold_return\.[a-z0-9_]{1,64}$/u.test(phase);
  return validPhase
    && Number.isSafeInteger(progress.elapsed_ms)
    && progress.elapsed_ms >= 0
    && progress.elapsed_ms <= 86_400_000
    && typeof progress.terminal === "boolean"
    && (progress.outcome === undefined || progress.outcome === "ok" || progress.outcome === "error");
}

export function createStartupRecoveryController(deps: StartupRecoveryDeps): {
  snapshot: Accessor<StartupRecoverySnapshot>;
  start: () => void;
  retry: () => void;
  openAnother: () => Promise<void>;
  returnToDirectFiles: () => Promise<void>;
  copyDetails: () => Promise<void>;
  receiveProgress: (progress: StartupProgressEvent) => void;
  dispose: () => void;
} {
  const now = deps.now ?? Date.now;
  const watchdogMs = deps.watchdogMs ?? STARTUP_LOOKUP_WATCHDOG_MS;
  const actionWatchdogMs = deps.actionWatchdogMs ?? STARTUP_ACTION_WATCHDOG_MS;
  const [snapshot, setSnapshot] = createSignal<StartupRecoverySnapshot>(idleSnapshot());
  let sequence = 0;
  let disposed = false;
  let watchdog: ReturnType<typeof setTimeout> | undefined;
  let ticker: ReturnType<typeof setInterval> | undefined;

  const clearTimers = () => {
    if (watchdog !== undefined) clearTimeout(watchdog);
    if (ticker !== undefined) clearInterval(ticker);
    watchdog = undefined;
    ticker = undefined;
  };

  const tickElapsed = () => {
    setSnapshot((current) => current.mode === "idle" ? current : {
      ...current,
      elapsedMs: Math.max(0, now() - current.startedAt),
    });
  };

  const begin = (
    attempt: number,
    operation: StartupOperation,
    phase: string,
    target: string,
    startedAt = now(),
    nativeAttempt = attempt,
  ) => {
    clearTimers();
    setSnapshot({
      mode: "working",
      operation,
      attempt,
      nativeAttempt,
      phase,
      nativePhase: null,
      startedAt,
      elapsedMs: 0,
      nativeElapsedMs: null,
      target,
      detail: null,
    });
    ticker = setInterval(tickElapsed, 100);
  };

  const recover = (
    attempt: number,
    phase: string,
    target: string,
    detail: string,
    startedAt: number,
  ) => {
    if (attempt !== sequence || disposed) return;
    if (watchdog !== undefined) clearTimeout(watchdog);
    watchdog = undefined;
    setSnapshot((current) => ({
      ...current,
      mode: "recovery",
      attempt,
      phase,
      target,
      detail: safeManagedErrorDetail(detail),
      startedAt,
      elapsedMs: Math.max(0, now() - startedAt),
    }));
  };

  const armActionWatchdog = (
    attempt: number,
    target: string,
    startedAt: number,
    detail: string,
  ) => {
    if (watchdog !== undefined) clearTimeout(watchdog);
    watchdog = setTimeout(() => {
      recover(attempt, "native.unavailable", target, detail, startedAt);
    }, actionWatchdogMs);
  };

  const finish = (attempt: number) => {
    if (attempt !== sequence || disposed) return;
    clearTimers();
    setSnapshot({ ...idleSnapshot(), attempt });
    deps.completeFirstLoad();
  };

  const completeOpen = async (attempt: number, target: string, startedAt: number) => {
    if (attempt !== sequence || disposed) return;
    setSnapshot((current) => ({
      ...current,
      mode: "working",
      operation: "graph_open",
      phase: "graph.access",
      target,
      detail: null,
    }));
    armActionWatchdog(
      attempt,
      target,
      startedAt,
      "Native recovery is unavailable or has stopped reporting progress. You can retry, choose another graph, or close and relaunch Tine; managed files have not been discarded.",
    );
    try {
      const outcome = await deps.openGraph(target);
      if (attempt !== sequence || disposed) return;
      if (outcome.kind === "loaded" || outcome.kind === "already_current") {
        finish(attempt);
      } else {
        recover(
          attempt,
          "graph.failed",
          target,
          "The workspace open operation was aborted before a graph became available.",
          startedAt,
        );
      }
    } catch (error) {
      recover(attempt, "graph.failed", target, error instanceof Error ? error.message : String(error), startedAt);
    }
  };

  const runLookup = () => {
    const attempt = ++sequence;
    const startedAt = now();
    const fallback = deps.injectedGraphPath() || deps.persistedGraphPath();
    begin(attempt, "lookup", "lookup.starting", fallback, startedAt);
    watchdog = setTimeout(() => {
      recover(
        attempt,
        "lookup.timeout",
        fallback,
        "The native workspace lookup is unresponsive. No storage operation has started.",
        startedAt,
      );
    }, watchdogMs);

    void deps.lookupGraphPath(attempt).then(
      (remembered) => {
        if (attempt !== sequence || disposed) return;
        if (watchdog !== undefined) clearTimeout(watchdog);
        watchdog = undefined;
        const target = deps.injectedGraphPath() || remembered || deps.persistedGraphPath();
        if (!target) {
          finish(attempt);
          return;
        }
        void completeOpen(attempt, target, startedAt);
      },
      (error) => {
        if (attempt !== sequence || disposed) return;
        if (watchdog !== undefined) clearTimeout(watchdog);
        watchdog = undefined;
        if (fallback) {
          void completeOpen(attempt, fallback, startedAt);
        } else {
          recover(
            attempt,
            "lookup.failed",
            "",
            error instanceof Error ? error.message : String(error),
            startedAt,
          );
        }
      },
    );
  };

  const invalidate = () => {
    const attempt = ++sequence;
    clearTimers();
    return attempt;
  };

  const openAnother = async () => {
    const previous = snapshot();
    if (previous.mode !== "recovery") return;
    const attempt = invalidate();
    const startedAt = now();
    begin(attempt, "graph_picker", "picker.open", previous.target, startedAt, previous.nativeAttempt);
    armActionWatchdog(
      attempt,
      previous.target,
      startedAt,
      "The native workspace picker is unavailable. Close and relaunch Tine if it did not appear.",
    );
    try {
      const outcome = await deps.pickGraph();
      if (attempt !== sequence || disposed) return;
      if (outcome.kind === "loaded" || outcome.kind === "already_current") {
        finish(attempt);
      } else {
        recover(attempt, previous.phase, previous.target, previous.detail ?? "Workspace selection was cancelled.", startedAt);
      }
    } catch (error) {
      recover(attempt, "graph.failed", previous.target, error instanceof Error ? error.message : String(error), startedAt);
    }
  };

  const returnToDirectFiles = async () => {
    const previous = snapshot();
    const eligible = previous.mode === "recovery"
      || (previous.mode === "working" && previous.operation === "graph_open");
    if (!eligible || !previous.target || previous.nativeAttempt <= 0) return;
    const attempt = invalidate();
    const startedAt = now();
    begin(attempt, "cold_return", "direct.confirm", previous.target, startedAt, previous.nativeAttempt);
    armActionWatchdog(
      attempt,
      previous.target,
      startedAt,
      "The native recovery confirmation is unavailable. Managed files remain preserved; retry or close and relaunch Tine.",
    );
    const approved = await deps.confirmColdReturn(startupGraphName(previous.target));
    if (attempt !== sequence || disposed) return;
    if (!approved) {
      if (previous.mode === "working" && previous.operation === "graph_open") {
        await completeOpen(attempt, previous.target, previous.startedAt);
        return;
      }
      if (watchdog !== undefined) clearTimeout(watchdog);
      watchdog = undefined;
      setSnapshot({ ...previous, attempt, startedAt, elapsedMs: Math.max(0, now() - startedAt) });
      return;
    }
    begin(attempt, "cold_return", "direct.archive", previous.target, startedAt, previous.nativeAttempt);
    armActionWatchdog(
      attempt,
      previous.target,
      startedAt,
      "Native recovery is unavailable or has stopped reporting progress. Managed files remain preserved; close and relaunch Tine before trying manual recovery.",
    );
    try {
      const result = await deps.coldReturn(previous.target, previous.nativeAttempt);
      if (attempt !== sequence || disposed) return;
      deps.acceptColdReturn(result);
      await completeOpen(attempt, previous.target, startedAt);
    } catch (error) {
      recover(attempt, "graph.failed", previous.target, error instanceof Error ? error.message : String(error), startedAt);
    }
  };

  const diagnostics = () => {
    const current = snapshot();
    const entries = [
      `Startup: ${current.phase}`,
      `Operation: ${current.operation ?? "none"}`,
      `Elapsed: ${Math.round(current.elapsedMs)} ms`,
    ];
    if (current.nativePhase) entries.push(`Native phase: ${current.nativePhase} (${current.nativeElapsedMs ?? 0} ms)`);
    if (current.detail) entries.push(`Detail: ${safeManagedErrorDetail(current.detail)}`);
    return entries.join("\n");
  };

  const copyDetails = async () => {
    try {
      await deps.copyText(diagnostics());
      deps.notify("Startup recovery details copied.", "success");
    } catch (error) {
      deps.notify(`Couldn't copy startup recovery details: ${safeManagedErrorDetail(error)}`, "error");
    }
  };

  const receiveProgress = (progress: StartupProgressEvent) => {
    if (!validStartupProgress(progress)) return;
    setSnapshot((current) => {
      if (current.mode === "idle") return current;
      const lookup = progress.phase.startsWith("lookup.");
      const managed = progress.phase.startsWith("managed_open.");
      const coldReturn = progress.phase.startsWith("cold_return.");
      if (lookup && current.operation !== "lookup") return current;
      if (managed && current.operation !== "graph_open") return current;
      if (coldReturn && current.operation !== "cold_return") return current;
      if (managed || coldReturn) {
        armActionWatchdog(
          current.attempt,
          current.target,
          current.startedAt,
          "Native recovery is unavailable or has stopped reporting progress. Managed files remain preserved; close and relaunch Tine before trying manual recovery.",
        );
      }
      return {
        ...current,
        mode: (managed || coldReturn) && current.mode === "recovery" && current.phase === "native.unavailable"
          ? "working"
          : current.mode,
        nativePhase: progress.phase,
        nativeElapsedMs: Math.max(0, progress.elapsed_ms),
      };
    });
  };

  return {
    snapshot,
    start: runLookup,
    retry: runLookup,
    openAnother,
    returnToDirectFiles,
    copyDetails,
    receiveProgress,
    dispose: () => {
      disposed = true;
      sequence++;
      clearTimers();
    },
  };
}
