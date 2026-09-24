import { OperationCancelledError, type QueryNotReadyError } from "../backend";
import { componentLifetime, runQueryWhenCurrent } from "../queryReadiness";
import { classifyReferenceLoadError, type ReferenceLoadError } from "./referenceLoadError";
import { graphEpoch, indexCorrectionRev } from "../ui";
import { graphBinding } from "../persistence";

/**
 * Fetch one references panel, waiting out a projection that is only mid-turn.
 *
 * The backend used to answer a reference read during indexing by parsing every
 * page in the graph, which is both slow and pointless: the same read a moment
 * later is served from the index. It now reports that state instead, the same
 * way a query block's read does, and the waiting happens here.
 *
 * Both panels share this so the cancellation policy has ONE definition. A
 * retry stops when the pane routes to another page, the graph is rebound or
 * repainted, or the section unmounts; without the unmount half, a disposed
 * panel would keep retrying forever because its captured page name never
 * changes.
 */
/** What a references panel says while it waits for the index.
 *
 * `QueryNotReadyError.message` is worded for a query block ("Updating query
 * results…"), which reads as the wrong subject beside a reference count. The
 * distinction it carries — a rebuild takes noticeably longer than a catch-up —
 * is worth keeping, so restate it rather than dropping it. */
export function referenceIndexPendingMessage(error: QueryNotReadyError | null): string | null {
  if (!error) return null;
  return error.reasonCode === "recovering" ? "rebuilding the index…" : "indexing…";
}

/** Which read a references panel shows: the page, and the graph binding and
 *  render epoch it was asked of. The panel's resource is keyed on all three,
 *  so a rebind refetches and an answer asked of the previous binding is
 *  dropped, wherever the panel is mounted (GH #543, audit R6-06: the right
 *  sidebar never remounts its panels, so keying on the name alone kept the
 *  old graph's rows there). */
export interface ReferenceRead {
  readonly name: string;
  readonly graphEpoch: number;
  readonly graphBinding: number;
  /** `indexCorrectionRev`: an answer given from the index as the last session
   *  left it is asked again once the launch check lands (GH #550). */
  readonly correction: number;
}

export function referenceRead(name: string): ReferenceRead {
  return {
    name,
    graphEpoch: graphEpoch(),
    graphBinding: graphBinding(),
    correction: indexCorrectionRev(),
  };
}

function sameReferenceRead(a: ReferenceRead, b: ReferenceRead): boolean {
  return (
    a.name === b.name &&
    a.graphEpoch === b.graphEpoch &&
    a.graphBinding === b.graphBinding &&
    a.correction === b.correction
  );
}

export function createReferenceFetcher(options: {
  /** The read the panel currently shows, read live. */
  currentRead: () => ReferenceRead;
  setLoadError: (error: ReferenceLoadError | null) => void;
  /** Non-null while the read is waiting for the index rather than failing. */
  setIndexPending: (error: QueryNotReadyError | null) => void;
}): <T>(read: ReferenceRead, load: () => Promise<T[]>) => Promise<T[]> {
  const lifetime = componentLifetime();
  return async <T>(read: ReferenceRead, load: () => Promise<T[]>): Promise<T[]> => {
    const current = () => !lifetime.ended() && sameReferenceRead(options.currentRead(), read);
    options.setLoadError(null);
    try {
      return await runQueryWhenCurrent(lifetime, load, current, options.setIndexPending);
    } catch (error) {
      // A superseded read is not a failure the user should see; the resource
      // for the new page is already running, and the panel's error state is
      // its to set (GH #543, audit R5-04).
      if (error instanceof OperationCancelledError || !current()) return [];
      options.setLoadError(classifyReferenceLoadError(error));
      return [];
    }
  };
}
