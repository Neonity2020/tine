import { createEffect, createSignal, on, onCleanup, Show } from "solid-js";
import type { IndexingProgress } from "../backend";
import { graphEpoch } from "../ui";
import { followIndexingProgress, indexingProgressLabel } from "../indexingProgress";

/** A compact toolbar indicator for the launch-time index work (GH #543).
 *  The app stays usable meanwhile; this only says how long the wait is. */
export function IndexingProgressBar() {
  const [progress, setProgress] = createSignal<IndexingProgress | null>(null);
  createEffect(on(graphEpoch, (epoch) => {
    let live = true;
    onCleanup(() => { live = false; });
    void followIndexingProgress(epoch, (next) => { if (live) setProgress(next); });
  }));
  return (
    <Show when={progress()}>
      {(current) => {
        const label = () => indexingProgressLabel(current());
        const fraction = () =>
          current().total > 0 ? Math.min(1, current().done / current().total) : null;
        return (
          <div
            class="indexing-progress"
            role="progressbar"
            aria-label={label()}
            aria-valuemin={0}
            aria-valuemax={current().total > 0 ? current().total : undefined}
            aria-valuenow={current().total > 0 ? current().done : undefined}
            title={label()}
          >
            <span class="indexing-progress-label">{label()}</span>
            <span class="indexing-progress-track">
              <span
                class="indexing-progress-fill"
                classList={{ "indexing-progress-indeterminate": fraction() === null }}
                style={fraction() === null ? undefined : { width: `${(fraction()! * 100).toFixed(1)}%` }}
              />
            </span>
          </div>
        );
      }}
    </Show>
  );
}
