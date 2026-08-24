import { createSignal, type Accessor } from "solid-js";
import type { LoadGraphPathOutcome } from "./graph";
import { safeManagedErrorDetail } from "./managedDiagnostics";
import type {
  SparseV2CancelResult,
  StorageTransitionEvent,
  StorageTransitionKind,
} from "./types";

export const STARTUP_PROGRESS_VISIBLE_MS = 250;

export type StartupOperation = "lookup" | "graph_open" | "graph_picker" | "cold_return";

export interface StartupRecoverySnapshot {
  mode: "idle" | "working" | "recovery";
  operation: StartupOperation | null;
  /** Frontend continuation generation only; never native storage authority. */
  attempt: number;
  operationId: number | null;
  transitionKind: StorageTransitionKind | null;
  phase: string;
  startedAt: number;
  elapsedMs: number;
  target: string;
  detail: string | null;
}

export interface StartupRecoveryDeps {
  lookupGraphPath(): Promise<string | null>;
  injectedGraphPath(): string;
  persistedGraphPath(): string;
  openGraph(path: string, supersedeCurrent?: boolean): Promise<LoadGraphPathOutcome>;
  pickGraph(): Promise<LoadGraphPathOutcome>;
  coldReturn(path: string): Promise<SparseV2CancelResult>;
  acceptColdReturn(result: SparseV2CancelResult): void;
  copyText(text: string): Promise<void>;
  notify(message: string, kind: "success" | "error"): void;
  completeFirstLoad(): void;
  now?: () => number;
}

const idleSnapshot = (): StartupRecoverySnapshot => ({
  mode: "idle",
  operation: null,
  attempt: 0,
  operationId: null,
  transitionKind: null,
  phase: "idle",
  startedAt: 0,
  elapsedMs: 0,
  target: "",
  detail: null,
});

export function startupGraphName(path: string): string {
  const trimmed = path.replace(/[\\/]+$/u, "");
  return trimmed.split(/[\\/]/u).at(-1) || "remembered graph";
}

export function startupPhaseLabel(phase: string): string {
  const known: Record<string, string> = {
    requested: "Starting storage operation",
    waiting_for_transition: "Waiting for the current storage operation",
    looking_up_selection: "Finding the last workspace",
    validating_target: "Checking workspace access",
    opening_direct: "Opening Direct Files",
    opening_managed: "Opening managed storage",
    activating_managed: "Enabling managed storage",
    joining_managed: "Joining the synced workspace",
    draining_managed: "Finishing managed-storage edits",
    confirming_projection: "Confirming the Markdown projection",
    quarantining_managed_selection: "Selecting the current Markdown tree as Direct Files",
    publishing_direct: "Opening Direct Files",
    "lookup.starting": "Finding the last workspace",
    "graph.access": "Checking workspace access",
    "graph.failed": "Workspace open failed",
    "picker.open": "Waiting for a workspace selection",
  };
  return known[phase] ?? phase.replace(/[._]+/gu, " ");
}

export function validStorageTransition(event: StorageTransitionEvent): boolean {
  return Number.isSafeInteger(event.operationId)
    && event.operationId > 0
    && Number.isSafeInteger(event.elapsedMs)
    && event.elapsedMs >= 0
    && typeof event.terminal === "boolean";
}

function startupOperation(kind: StorageTransitionKind): StartupOperation {
  switch (kind) {
    case "lookup": return "lookup";
    case "return_emergency":
    case "return_gracefully": return "cold_return";
    default: return "graph_open";
  }
}

export function createStartupRecoveryController(deps: StartupRecoveryDeps): {
  snapshot: Accessor<StartupRecoverySnapshot>;
  start: () => void;
  retry: () => void;
  openAnother: () => Promise<void>;
  returnToDirectFiles: () => Promise<void>;
  copyDetails: () => Promise<void>;
  receiveTransition: (event: StorageTransitionEvent) => void;
  dispose: () => void;
} {
  const now = deps.now ?? Date.now;
  const [snapshot, setSnapshot] = createSignal<StartupRecoverySnapshot>(idleSnapshot());
  let sequence = 0;
  let latestNativeOperation = 0;
  let disposed = false;
  let ticker: ReturnType<typeof setInterval> | undefined;

  const clearTicker = () => {
    if (ticker !== undefined) clearInterval(ticker);
    ticker = undefined;
  };

  const begin = (attempt: number, operation: StartupOperation, phase: string, target: string) => {
    clearTicker();
    setSnapshot({
      mode: "working",
      operation,
      attempt,
      operationId: null,
      transitionKind: null,
      phase,
      startedAt: now(),
      elapsedMs: 0,
      target,
      detail: null,
    });
    ticker = setInterval(() => {
      setSnapshot((current) => current.mode === "idle" ? current : {
        ...current,
        elapsedMs: Math.max(0, now() - current.startedAt),
      });
    }, 100);
  };

  const finish = (attempt: number) => {
    if (attempt !== sequence || disposed) return;
    clearTicker();
    setSnapshot({ ...idleSnapshot(), attempt });
    deps.completeFirstLoad();
  };

  const recover = (attempt: number, target: string, detail: unknown) => {
    if (attempt !== sequence || disposed) return;
    clearTicker();
    setSnapshot((current) => ({
      ...current,
      mode: "recovery",
      phase: "graph.failed",
      target,
      detail: safeManagedErrorDetail(detail),
      elapsedMs: Math.max(0, now() - current.startedAt),
    }));
  };

  const completeOpen = async (attempt: number, target: string, supersedeCurrent = false) => {
    if (attempt !== sequence || disposed) return;
    setSnapshot((current) => ({ ...current, operation: "graph_open", phase: "graph.access", target }));
    try {
      const outcome = supersedeCurrent
        ? await deps.openGraph(target, true)
        : await deps.openGraph(target);
      if (attempt !== sequence || disposed) return;
      if (outcome.kind === "loaded" || outcome.kind === "already_current") finish(attempt);
      else recover(attempt, target, "The workspace open operation was superseded or cancelled.");
    } catch (error) {
      recover(attempt, target, error);
    }
  };

  const runLookup = () => {
    const attempt = ++sequence;
    const fallback = deps.injectedGraphPath() || deps.persistedGraphPath();
    begin(attempt, "lookup", "lookup.starting", fallback);
    void deps.lookupGraphPath().then(
      (remembered) => {
        if (attempt !== sequence || disposed) return;
        const target = deps.injectedGraphPath() || remembered || deps.persistedGraphPath();
        if (target) void completeOpen(attempt, target);
        else finish(attempt);
      },
      (error) => {
        if (attempt !== sequence || disposed) return;
        if (fallback) void completeOpen(attempt, fallback);
        else recover(attempt, "", error);
      },
    );
  };

  const invalidate = () => {
    const attempt = ++sequence;
    clearTicker();
    return attempt;
  };

  const openAnother = async () => {
    const previous = snapshot();
    if (previous.mode === "idle") return;
    const attempt = invalidate();
    begin(attempt, "graph_picker", "picker.open", previous.target);
    try {
      const outcome = await deps.pickGraph();
      if (attempt !== sequence || disposed) return;
      if (outcome.kind === "loaded" || outcome.kind === "already_current") finish(attempt);
      else recover(attempt, previous.target, previous.detail ?? "Workspace selection was cancelled.");
    } catch (error) {
      recover(attempt, previous.target, error);
    }
  };

  const returnToDirectFiles = async () => {
    const previous = snapshot();
    const eligible = previous.mode === "recovery"
      || (previous.mode === "working" && previous.operation === "graph_open");
    if (!eligible || !previous.target) return;
    const attempt = invalidate();
    // The button itself is the explicit emergency action. A native modal can
    // be starved behind the very managed open this path must abandon, which
    // leaves the user trapped before the native supervisor ever sees the
    // request. Managed evidence is preserved, so immediate invocation is both
    // the safer and the reversible failure-mode behavior.
    begin(attempt, "cold_return", "quarantining_managed_selection", previous.target);
    try {
      const result = await deps.coldReturn(previous.target);
      if (attempt !== sequence || disposed) return;
      deps.acceptColdReturn(result);
      await completeOpen(attempt, previous.target, true);
    } catch (error) {
      recover(attempt, previous.target, error);
    }
  };

  const receiveTransition = (event: StorageTransitionEvent) => {
    if (!validStorageTransition(event) || disposed) return;
    if (event.operationId < latestNativeOperation) return;
    latestNativeOperation = event.operationId;
    setSnapshot((current) => {
      if (current.mode === "idle") return current;
      return {
        ...current,
        operation: startupOperation(event.kind),
        operationId: event.operationId,
        transitionKind: event.kind,
        phase: event.phase,
        elapsedMs: Math.max(current.elapsedMs, event.elapsedMs),
      };
    });
  };

  const diagnostics = () => {
    const current = snapshot();
    const lines = [
      `Startup: ${current.phase}`,
      `Operation: ${current.operation ?? "none"}`,
      `Elapsed: ${Math.round(current.elapsedMs)} ms`,
    ];
    if (current.operationId) lines.push(`Native operation: ${current.operationId} (${current.transitionKind})`);
    if (current.detail) lines.push(`Detail: ${safeManagedErrorDetail(current.detail)}`);
    return lines.join("\n");
  };

  return {
    snapshot,
    start: runLookup,
    retry: runLookup,
    openAnother,
    returnToDirectFiles,
    copyDetails: async () => {
      try {
        await deps.copyText(diagnostics());
        deps.notify("Startup recovery details copied.", "success");
      } catch (error) {
        deps.notify(`Couldn't copy startup recovery details: ${safeManagedErrorDetail(error)}`, "error");
      }
    },
    receiveTransition,
    dispose: () => {
      disposed = true;
      sequence++;
      clearTicker();
    },
  };
}
