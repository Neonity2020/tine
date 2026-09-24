import { getOwner, onCleanup } from "solid-js";
import { OperationCancelledError, QueryNotReadyError } from "./backend";
import { graphMeta, graphTransitioning } from "./ui";

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

declare const lifetimeBrand: unique symbol;

/** The component a readiness retry belongs to. Only `componentLifetime` and
 * `manualLifetime` make one, so a retry cannot be started without an end. */
export interface Lifetime {
  readonly [lifetimeBrand]: true;
  readonly ended: () => boolean;
}

/** Call in a component's setup: the lifetime ends in its `onCleanup`. */
export function componentLifetime(): Lifetime {
  if (!getOwner()) throw new Error("componentLifetime() outside a component: nothing would end it");
  let ended = false;
  onCleanup(() => {
    ended = true;
  });
  return { ended: () => ended } as Lifetime;
}

/** A lifetime for an owner that is not a component; `end` stops its retries. */
export function manualLifetime(): { lifetime: Lifetime; end: () => void } {
  let ended = false;
  return { lifetime: { ended: () => ended } as Lifetime, end: () => { ended = true; } };
}

/** The same owner, for a caller that already has a monotonic revision instead of
 * a resource: an imperative read (`query_parse` inside an authoring session, an
 * export warm-up) publishes only while `isCurrent()` holds, so the readiness
 * retry reuses THAT gate rather than growing a second cancellation policy
 * beside it. Readiness policy stays in `runQueryWhenReady` and is not
 * duplicated, and nothing here is mode-specific.
 *
 * Whose is the retry? Three halves, each owned once:
 * - the component: `lifetime`, which ends when it is disposed (GH #543, audit
 *   R12-06: a removed block polled for as long as the index was not ready);
 * - the graph: the root it was asked of. A switch to another graph ends it,
 *   or it retried into the next graph (R12-06). A reopen of the SAME graph
 *   (a `config.edn` change) does not: each attempt asks the backend afresh, so
 *   the retry simply reads the reopened graph. Ending it there left a query
 *   block waiting forever, since nothing asks again (audit R13-01);
 * - the caller's own supersession: `callerIsCurrent`.
 */
export function runQueryWhenCurrent<T>(
  lifetime: Lifetime,
  read: () => Promise<T>,
  callerIsCurrent: () => boolean = () => true,
  onPending: (error: QueryNotReadyError | null) => void = () => {},
): Promise<T> {
  const root = graphMeta()?.root;
  const isCurrent = () => !lifetime.ended() && graphMeta()?.root === root && callerIsCurrent();
  // While a graph is being opened, the backend may already serve the next
  // graph while `graphMeta` still names this one: such an answer is not
  // taken, and the retry waits until the switch settles which graph it is.
  const load = (): Promise<T> => {
    if (graphTransitioning()) return Promise.reject(new QueryNotReadyError("busy"));
    return read().then((value) => {
      if (graphTransitioning()) throw new QueryNotReadyError("busy");
      return value;
    });
  };
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
      // Capped low: the wait ends when the index turns ready, and a longer
      // step kept search and references on "indexing" up to 0.8 s after
      // that (GH #543 Ctrl-K).
      waitMs = Math.min(waitMs * 2, 250);
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
