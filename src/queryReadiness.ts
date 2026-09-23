import { OperationCancelledError, QueryNotReadyError } from "./backend";
import { graphBinding } from "./persistence";

export interface QueryReadinessOwner {
  signal: AbortSignal;
  /** Compare a captured monotonic request/binding revision, not source text. */
  isCurrent: () => boolean;
  /** Called only while this owner is current. A new owner clears its own state. */
  onPending: (error: QueryNotReadyError | null) => void;
}

function requireCurrent(owner: QueryReadinessOwner): void {
  if (owner.signal.aborted || !owner.isCurrent()) throw new OperationCancelledError();
}

/** Stop awaiting a shared attempt without cancelling other subscribers. Native
 * job cancellation is owned separately by the command's consumer identity. */
function ownedAttempt<T>(load: () => Promise<T>, signal: AbortSignal): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const abort = () => reject(new OperationCancelledError());
    if (signal.aborted) { abort(); return; }
    signal.addEventListener("abort", abort, { once: true });
    Promise.resolve().then(() => {
      if (signal.aborted) throw new OperationCancelledError();
      return load();
    }).then(resolve, reject).finally(() => signal.removeEventListener("abort", abort));
  });
}

function delay(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    const abort = () => {
      clearTimeout(timer);
      signal.removeEventListener("abort", abort);
      reject(new OperationCancelledError());
    };
    const timer = setTimeout(() => {
      signal.removeEventListener("abort", abort);
      resolve();
    }, ms);
    if (signal.aborted) { abort(); return; }
    signal.addEventListener("abort", abort, { once: true });
  });
}

/** The same owner, for a caller that already has a monotonic revision instead of
 * a resource: an imperative read (`query_parse` inside an authoring session, an
 * export warm-up) publishes only while `isCurrent()` holds, so the readiness
 * retry reuses THAT gate rather than growing a second cancellation policy
 * beside it. Readiness policy stays in `runQueryWhenReady` and is not
 * duplicated, and nothing here is mode-specific. */
/**
 * A read also belongs to the graph it was asked of: `isCurrent` holds only
 * while the binding it started on does. That half is the same for every
 * caller, so it lives here; each caller's `isCurrent` still owns the other
 * half, its own disposal and supersession. A caller that forgot the binding
 * kept retrying into the next graph (GH #543, audit R12-06).
 */
export function runQueryWhenCurrent<T>(
  load: () => Promise<T>,
  callerIsCurrent: () => boolean,
  onPending: (error: QueryNotReadyError | null) => void = () => {},
): Promise<T> {
  const binding = graphBinding();
  const isCurrent = () => graphBinding() === binding && callerIsCurrent();
  // The FIRST attempt is eager. `runQueryWhenReady` defers every attempt by a
  // microtask so a synchronous abort can win the race, which is right for a
  // resource that owns an `AbortController` — but an imperative caller has no
  // controller to race, and the deferral lets a superseded intermediate state
  // issue a read of its own before this one has even started. Readiness policy
  // is still `runQueryWhenReady`'s alone: it owns every retry after the first
  // refusal, and nothing here is mode-specific.
  //
  // The eager attempt settles under the same ownership rule as every retry: a
  // caller that is no longer current gets cancellation, whatever the attempt
  // returned. Rethrowing its own refusal let a superseded references read mark
  // the replacement panel failed (GH #543, audit R5-04), and a late success
  // could feed a stale reading to a caller that no longer asked for it.
  return load().then(
    (value) => {
      if (!isCurrent()) throw new OperationCancelledError();
      return value;
    },
    (error) => {
      if (!isCurrent()) throw new OperationCancelledError();
      if (!(error instanceof QueryNotReadyError)) throw error;
      onPending(error);
      return runQueryWhenReady(load, {
        signal: new AbortController().signal,
        isCurrent,
        onPending,
      });
    },
  );
}

/** Retry typed readiness only. The native producer must report a failed rebuild
 * as terminal; this operation does not convert a real failure into indexing. */
export async function runQueryWhenReady<T>(
  load: () => Promise<T>,
  owner: QueryReadinessOwner,
): Promise<T> {
  let waitMs = 100;
  try {
    while (true) {
      requireCurrent(owner);
      try {
        const value = await ownedAttempt(() => {
          requireCurrent(owner);
          return load();
        }, owner.signal);
        requireCurrent(owner);
        return value;
      } catch (error) {
        requireCurrent(owner);
        if (!(error instanceof QueryNotReadyError)) throw error;
        owner.onPending(error);
      }
      await delay(waitMs, owner.signal);
      waitMs = Math.min(waitMs * 2, 800);
    }
  } finally {
    if (!owner.signal.aborted && owner.isCurrent()) owner.onPending(null);
  }
}

/** What the SEARCH surfaces say while they wait for the query index.
 *
 * Sibling of `referenceIndexPendingMessage` (`src/lib/referenceFetch.ts`),
 * worded for this subject — a Quick Switcher / search-tab status line, not a
 * reference count and not a query block's "Updating query results…".
 *
 * `recovering` is the one reason worth naming, because a rebuild takes
 * noticeably longer than a catch-up and the user is deciding whether to keep
 * waiting. `indexing`, `pending_edits` and `busy` all read as indexing: that is
 * the same editorial choice the reference panel makes and pins in its own test,
 * not an oversight. Both call sites previously hardcoded the indexing sentence
 * and so lost the rebuild distinction entirely. */
export function searchIndexPendingMessage(error: QueryNotReadyError | null): string | null {
  if (!error) return null;
  return error.reasonCode === "recovering"
    ? "Rebuilding the search index — waiting for search to be ready…"
    : "Indexing — waiting for search to be ready…";
}
