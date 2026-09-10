import { onCleanup } from "solid-js";
import { OperationCancelledError, type QueryNotReadyError } from "../backend";
import { runQueryWhenCurrent } from "../queryReadiness";
import { classifyReferenceLoadError, type ReferenceLoadError } from "./referenceLoadError";

/**
 * Fetch one references panel, waiting out a projection that is only mid-turn.
 *
 * The backend used to answer a reference read during indexing by parsing every
 * page in the graph, which is both slow and pointless: the same read a moment
 * later is served from the index. It now reports that state instead, the same
 * way a query block's read does, and the waiting happens here.
 *
 * Both panels share this so the cancellation policy has ONE definition. A
 * retry stops when the pane routes to another page or the section unmounts;
 * without the unmount half, a disposed panel would keep retrying forever
 * because its captured page name never changes.
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

export function createReferenceFetcher(options: {
  /** The page the panel is currently showing, read live. */
  currentName: () => string;
  setLoadError: (error: ReferenceLoadError | null) => void;
  /** Non-null while the read is waiting for the index rather than failing. */
  setIndexPending: (error: QueryNotReadyError | null) => void;
}): <T>(name: string, load: () => Promise<T[]>) => Promise<T[]> {
  let disposed = false;
  onCleanup(() => {
    disposed = true;
  });
  return async <T>(name: string, load: () => Promise<T[]>): Promise<T[]> => {
    options.setLoadError(null);
    try {
      return await runQueryWhenCurrent(
        load,
        () => !disposed && options.currentName() === name,
        options.setIndexPending,
      );
    } catch (error) {
      // A superseded read is not a failure the user should see; the resource
      // for the new page is already running.
      if (error instanceof OperationCancelledError) return [];
      options.setLoadError(classifyReferenceLoadError(error));
      return [];
    }
  };
}
