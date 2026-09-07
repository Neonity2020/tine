import { OperationCancelledError, QueryNotReadyError } from "./backend";

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
