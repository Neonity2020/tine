export interface SerializedWrites {
  readonly scope: string;
  run<T>(operation: () => Promise<T>): Promise<T>;
}

/**
 * Serialize mutations that share one durable key.
 *
 * Callers compute their next value inside `operation`, persist it before
 * publishing it, and receive the operation's original result or error. The
 * private tail swallows only for queue continuity, never for the caller.
 */
export function serializedWrites(scope: string): SerializedWrites {
  let tail: Promise<void> = Promise.resolve();
  return {
    scope,
    run<T>(operation: () => Promise<T>): Promise<T> {
      const result = tail.then(operation, operation);
      tail = result.then(() => undefined, () => undefined);
      return result;
    },
  };
}
