import { createMemo, createResource, createSignal, onCleanup, type Accessor, type Resource } from "solid-js";
import type { QueryNotReadyError } from "./backend";
import { graphBinding } from "./persistence";
import { graphMeta, graphTransitioning } from "./ui";
import { runQueryWhenReady } from "./queryReadiness";

/** A query resource owns retries for its current source and graph binding.
 * Solid retains the previous successful value while its replacement is pending. */
export function createReadyQueryResource<K, T>(
  source: () => K | undefined | null | false,
  load: (key: K) => Promise<T>,
): [Resource<T>, Accessor<QueryNotReadyError | null>] {
  const [pending, setPending] = createSignal<QueryNotReadyError | null>(null);
  const root = createMemo(() => graphMeta()?.root);
  type Request = { key: K; controller: AbortController };
  const request = createMemo<Request | undefined>((previous) => {
    const key = source();
    graphBinding();
    root();
    const transitioning = graphTransitioning();
    previous?.controller.abort();
    setPending(null);
    if (key === undefined || key === null || key === false || transitioning) return undefined;
    return { key, controller: new AbortController() };
  });
  onCleanup(() => request()?.controller.abort());
  const [result] = createResource(request, (current) => runQueryWhenReady(
    () => load(current.key),
    {
      signal: current.controller.signal,
      isCurrent: () => request() === current,
      onPending: setPending,
    },
  ));
  return [result, pending];
}
