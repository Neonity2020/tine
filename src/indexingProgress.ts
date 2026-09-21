import { backend, type IndexingProgress } from "./backend";
import { graphEpoch } from "./ui";
import { waitForWarmCache } from "./warmCache";

/** Polling cadence while graph-sized index work runs. */
const POLL_MS = 500;
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
  epoch(): number;
  progress(): Promise<IndexingProgress | null>;
  warmDone(epoch: number): Promise<boolean>;
  now(): number;
  sleep(ms: number): Promise<void>;
}

const defaultDeps: IndexingProgressDeps = {
  epoch: graphEpoch,
  progress: () => backend().indexingProgress(),
  warmDone: (epoch) => waitForWarmCache(epoch),
  now: () => Date.now(),
  sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
};

/** Follow the index work of the graph opened at `epoch` until it is done.
 *
 * Done means the whole-graph warm has finished AND two polls in a row found
 * nothing graph-sized running: the warm can finish before the fresh index
 * build does, and a single empty poll can fall between two passes. */
export async function followIndexingProgress(
  epoch: number,
  publish: (progress: IndexingProgress | null) => void,
  deps: IndexingProgressDeps = defaultDeps,
): Promise<void> {
  let warmed = false;
  void deps.warmDone(epoch).then(() => { warmed = true; }, () => { warmed = true; });
  const started = deps.now();
  let idlePolls = 0;
  let failedPolls = 0;
  try {
    while (deps.epoch() === epoch) {
      let progress: IndexingProgress | null = null;
      try {
        progress = await deps.progress();
        failedPolls = 0;
      } catch {
        // A transient IPC failure (the graph rebinding under us) is not
        // progress. A persistent one means there is no graph to follow.
        failedPolls += 1;
        if (failedPolls >= 10) return;
      }
      if (deps.epoch() !== epoch) return;
      idlePolls = progress ? 0 : idlePolls + 1;
      publish(progress && deps.now() - started >= SHOW_AFTER_MS ? progress : null);
      if (warmed && idlePolls >= 2) return;
      await deps.sleep(POLL_MS);
    }
  } finally {
    publish(null);
  }
}
