import { backend, type IndexingProgress } from "./backend";
import { graphBinding } from "./persistence";
import { graphEpoch } from "./ui";
import { waitForWarmCache } from "./warmCache";

/** Polling cadence while graph-sized index work runs. */
const POLL_MS = 500;
/** Polling cadence once launch indexing has settled: a later pass in the
 *  same graph session (a damaged-index repair) still shows, within seconds. */
const WATCH_MS = 5_000;
/** A graph that finishes indexing faster than this never shows the bar. */
const SHOW_AFTER_MS = 700;

export function indexingProgressLabel(progress: IndexingProgress): string {
  const subject =
    progress.phase === "checking"
      ? "Checking search index"
      : progress.phase === "reading"
        ? "Reading pages"
        : "Building search index";
  if (progress.total === 0) return `${subject}…`;
  const format = (n: number) => n.toLocaleString();
  return `${subject} · ${format(progress.done)} / ${format(progress.total)} pages`;
}

export interface IndexingProgressDeps {
  /** The graph binding followed; the follower ends when it moves. */
  binding(): number;
  progress(): Promise<IndexingProgress | null>;
  warmDone(): Promise<boolean>;
  now(): number;
  sleep(ms: number): Promise<void>;
  /** A hidden window skips its settled-state polls. */
  hidden?(): boolean;
}

const defaultDeps: IndexingProgressDeps = {
  binding: graphBinding,
  progress: () => backend().indexingProgress(),
  warmDone: () => waitForWarmCache(graphEpoch()),
  now: () => Date.now(),
  sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
  hidden: () => typeof document !== "undefined" && document.hidden,
};

/** Follow the index work of the graph bound at `binding` while it is bound.
 *
 * The graph BINDING, not the render epoch: a repaint (typography, the journal
 * title format) bumps the epoch without moving the graph, and a follower
 * keyed on it restarted with a fresh no-flash grace, hiding a showing bar
 * for about a second (GH #543, audit R10-11).
 *
 * Launch indexing has settled once the whole-graph warm has finished AND two
 * polls in a row found nothing graph-sized running: the warm can finish
 * before the fresh index build does, and a single empty poll can fall
 * between two passes. After that the follower keeps watching at a slower
 * cadence, because a later pass in the same session (a repair after a
 * damaged index) is just as graph-sized and moves no graph epoch (GH #543). */
export async function followIndexingProgress(
  binding: number,
  publish: (progress: IndexingProgress | null) => void,
  deps: IndexingProgressDeps = defaultDeps,
  signal?: AbortSignal,
): Promise<void> {
  let warmed = false;
  void deps.warmDone().then(() => { warmed = true; }, () => { warmed = true; });
  let started = deps.now();
  let idlePolls = 0;
  let failedPolls = 0;
  let settled = false;
  try {
    while (deps.binding() === binding && !signal?.aborted) {
      if (settled && deps.hidden?.()) {
        await deps.sleep(WATCH_MS);
        continue;
      }
      let progress: IndexingProgress | null = null;
      try {
        progress = await deps.progress();
        failedPolls = 0;
      } catch {
        // A transient IPC failure (the graph rebinding under us) is not
        // progress. A persistent one slows the polls to the watch cadence;
        // it does not end the follower, which nothing would restart while
        // the binding stands (GH #543, audit R11-12).
        failedPolls += 1;
        if (failedPolls >= 10) {
          publish(null);
          await deps.sleep(WATCH_MS);
          continue;
        }
      }
      if (deps.binding() !== binding || signal?.aborted) return;
      if (progress && settled) {
        // A new pass: it gets the same no-flash grace as the launch one.
        settled = false;
        started = deps.now();
      }
      idlePolls = progress ? 0 : idlePolls + 1;
      publish(progress && deps.now() - started >= SHOW_AFTER_MS ? progress : null);
      if (warmed && idlePolls >= 2) settled = true;
      await deps.sleep(settled ? WATCH_MS : POLL_MS);
    }
  } finally {
    publish(null);
  }
}
