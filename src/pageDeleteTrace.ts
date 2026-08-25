import { dbg } from "./debug";

export type PageDeleteTrace = {
  id: number;
  phase(name: string): void;
  armFallback(): void;
  finish(name: string): void;
};

type ActiveFallback = {
  id: number;
  startedAt: number;
  fetchStarted: boolean;
};

let nextId = 1;
let activeFallback: ActiveFallback | null = null;

const now = () => typeof performance !== "undefined" ? performance.now() : Date.now();

/**
 * Debug-only timing for the Android page-delete handoff (GH #376). It records no
 * page names or paths: a remote debug log can identify the slow boundary without
 * leaking graph contents. The trace is intentionally observational; deletion
 * safety never depends on it.
 */
export function beginPageDeleteTrace(kind: "journal" | "page"): PageDeleteTrace {
  const id = nextId++;
  const startedAt = now();
  let finished = false;
  const emit = (name: string) => {
    if (finished) return;
    dbg(`page-delete ${id} ${kind} ${name} +${Math.max(0, now() - startedAt).toFixed(1)}ms`);
  };
  emit("confirm-start");
  return {
    id,
    phase: emit,
    armFallback() {
      activeFallback = { id, startedAt, fetchStarted: false };
      emit("fallback-armed");
    },
    finish(name: string) {
      emit(name);
      finished = true;
      if (activeFallback?.id === id) activeFallback = null;
    },
  };
}

/** Returns a trace token only for the first route fetch caused by a durable delete. */
export function markPageDeleteFallbackFetch(paneId: string, routeKind: string): number | null {
  const trace = activeFallback;
  if (!trace || trace.fetchStarted) return null;
  trace.fetchStarted = true;
  dbg(
    `page-delete ${trace.id} fallback-fetch-start pane=${paneId} route=${routeKind} `
      + `+${Math.max(0, now() - trace.startedAt).toFixed(1)}ms`,
  );
  return trace.id;
}

/** Called from requestAnimationFrame, after the fallback readiness DOM has landed. */
export function markPageDeleteFallbackFirstPaint(traceId: number, outcome: "ready" | "error"): void {
  const trace = activeFallback;
  if (!trace || trace.id !== traceId) return;
  dbg(
    `page-delete ${trace.id} fallback-first-paint outcome=${outcome} `
      + `+${Math.max(0, now() - trace.startedAt).toFixed(1)}ms`,
  );
  activeFallback = null;
}
