// Share identical query IPC work across split panes without turning results into
// another graph-lifetime cache. In-flight promises are strongly held; completed
// DTO objects are held weakly, so mounted consumers can share the same object but
// the GC may reclaim it once no view uses it.

import { OperationCancelledError } from "./backend";

interface SharedAttempt {
  promise: Promise<object>;
  controller: AbortController;
  subscribers: number;
  unowned: boolean;
  settled: boolean;
}

const inFlight = new Map<string, SharedAttempt>();
const resolved = new Map<string, WeakRef<object>>();
const MAX_RESOLVED_KEYS = 128;
let currentScope = "";
let scopeGeneration = 0;

export function sharedQueryScope(root: string | undefined, epoch: number, binding: number): string {
  return `${root ?? ""}\0${epoch}\0${binding}`;
}

function enterScope(scope: string): number {
  if (scope !== currentScope) {
    currentScope = scope;
    scopeGeneration++;
    inFlight.clear();
    resolved.clear();
  }
  return scopeGeneration;
}

export function sharedQueryResult<T extends object>(
  scope: string,
  key: string,
  load: (signal: AbortSignal) => Promise<T>,
  signal?: AbortSignal,
): Promise<T> {
  if (signal?.aborted) return Promise.reject(new OperationCancelledError());
  const generation = enterScope(scope);
  const cacheKey = `${scope}\0${key}`;
  const prior = resolved.get(cacheKey)?.deref() as T | undefined;
  if (prior) {
    const ref = resolved.get(cacheKey)!;
    resolved.delete(cacheKey);
    resolved.set(cacheKey, ref);
    return Promise.resolve(prior);
  }
  // Do not accumulate dead WeakRef keys over a long editing session.
  if (resolved.has(cacheKey)) resolved.delete(cacheKey);
  const running = inFlight.get(cacheKey);
  if (running) return subscribe<T>(cacheKey, running, signal);

  const controller = new AbortController();
  let loading: Promise<T>;
  try { loading = load(controller.signal); }
  catch (error) { loading = Promise.reject(error); }
  let attempt: SharedAttempt;
  const promise = loading.then((value) => {
    attempt.settled = true;
    if (!controller.signal.aborted && generation === scopeGeneration && scope === currentScope) {
      resolved.set(cacheKey, new WeakRef(value));
      while (resolved.size > MAX_RESOLVED_KEYS) {
        const oldest = resolved.keys().next().value as string | undefined;
        if (oldest === undefined) break;
        resolved.delete(oldest);
      }
    }
    return value;
  }, error => { attempt.settled = true; throw error; });
  attempt = { promise, controller, subscribers: 0, unowned: false, settled: false };
  inFlight.set(cacheKey, attempt);
  void promise.finally(() => {
    if (inFlight.get(cacheKey) === attempt) inFlight.delete(cacheKey);
  }).catch(() => {});
  return subscribe<T>(cacheKey, attempt, signal);
}

function subscribe<T extends object>(key: string, attempt: SharedAttempt, signal?: AbortSignal): Promise<T> {
  if (!signal) {
    // Existing metadata consumers own the attempt until it settles.
    attempt.unowned = true;
    return attempt.promise as Promise<T>;
  }
  const cancelIfUnused = () => {
    if (!attempt.settled && !attempt.unowned && attempt.subscribers === 0) {
      attempt.controller.abort();
      if (inFlight.get(key) === attempt) inFlight.delete(key);
    }
  };
  if (signal.aborted) {
    cancelIfUnused();
    return Promise.reject(new OperationCancelledError());
  }
  attempt.subscribers++;
  return new Promise<T>((resolve, reject) => {
    let active = true;
    const release = () => {
      if (!active) return;
      active = false;
      signal.removeEventListener("abort", abort);
      attempt.subscribers--;
      cancelIfUnused();
    };
    const abort = () => { release(); reject(new OperationCancelledError()); };
    signal.addEventListener("abort", abort, { once: true });
    attempt.promise.then(value => { release(); resolve(value as T); }, error => { release(); reject(error); });
  });
}

export function resetSharedQueryResultsForTests(): void {
  currentScope = "";
  scopeGeneration++;
  for (const attempt of inFlight.values()) attempt.controller.abort();
  inFlight.clear();
  resolved.clear();
}
