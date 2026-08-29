export type GraphOpenMilestone =
  | "native_binding_ready"
  | "session_restored"
  | "interactive"
  | "first_content";

export interface GraphOpenCommandTrace {
  command: string;
  startedMs: number;
  elapsedMs: number;
  outcome: "completed" | "failed";
}

export interface GraphOpenTrace {
  schemaVersion: 1;
  beganAtMs: number;
  milestones: Partial<Record<GraphOpenMilestone, number>>;
  commands: GraphOpenCommandTrace[];
  droppedCommands: number;
}

declare global {
  interface Window {
    /** Bounded, content-free timing receipt for the current graph open. */
    __TINE_GRAPH_OPEN_TRACE__?: GraphOpenTrace;
  }
}

const COMMAND_LIMIT = 128;
let trace: GraphOpenTrace | undefined;
let observer: MutationObserver | undefined;

function elapsed(now = performance.now()): number {
  return trace ? Math.max(0, now - trace.beganAtMs) : 0;
}

function visibleGraphContent(): boolean {
  if (typeof document === "undefined") return false;
  if (document.querySelector(".ls-block, .journal-day")) return true;
  const title = document.querySelector("h1.page-title");
  return !!title?.textContent?.trim();
}

function observeFirstContent(): void {
  if (!trace || trace.milestones.first_content !== undefined) return;
  if (trace.milestones.native_binding_ready === undefined) return;
  if (!visibleGraphContent()) return;
  trace.milestones.first_content = elapsed();
  observer?.disconnect();
  observer = undefined;
}

export function beginGraphOpenTrace(): void {
  observer?.disconnect();
  trace = {
    schemaVersion: 1,
    beganAtMs: performance.now(),
    milestones: {},
    commands: [],
    droppedCommands: 0,
  };
  if (typeof window !== "undefined") window.__TINE_GRAPH_OPEN_TRACE__ = trace;
  if (typeof MutationObserver !== "undefined" && typeof document !== "undefined") {
    observer = new MutationObserver(observeFirstContent);
    observer.observe(document.documentElement, {
      childList: true,
      subtree: true,
      characterData: true,
    });
  }
}

export function markGraphOpen(milestone: Exclude<GraphOpenMilestone, "first_content">): void {
  if (!trace || trace.milestones[milestone] !== undefined) return;
  trace.milestones[milestone] = elapsed();
  if (milestone === "native_binding_ready") {
    observeFirstContent();
    if (trace.milestones.first_content === undefined && typeof requestAnimationFrame !== "undefined") {
      requestAnimationFrame(observeFirstContent);
    }
  }
}

export function recordGraphOpenCommand(
  command: string,
  startedAtMs: number,
  outcome: "completed" | "failed",
): void {
  if (!trace) return;
  if (trace.commands.length >= COMMAND_LIMIT) {
    trace.droppedCommands += 1;
    return;
  }
  trace.commands.push({
    command,
    startedMs: Math.max(0, startedAtMs - trace.beganAtMs),
    elapsedMs: Math.max(0, performance.now() - startedAtMs),
    outcome,
  });
}

export function resetGraphOpenTraceForTest(): void {
  observer?.disconnect();
  observer = undefined;
  trace = undefined;
  if (typeof window !== "undefined") delete window.__TINE_GRAPH_OPEN_TRACE__;
}
