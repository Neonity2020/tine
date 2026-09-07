import { createComputed, createMemo, createResource, createSignal, onCleanup, type Accessor, type Resource } from "solid-js";
import type { QueryNotReadyError } from "./backend";
import { graphBinding } from "./persistence";
import { graphEpoch, graphMeta, graphTransitioning } from "./ui";
import { onGraphRebound } from "./modeHooks";
import { runQueryWhenReady } from "./queryReadiness";

/** A query resource owns retries for its current source and graph binding.
 * Solid retains the previous successful value while its replacement is pending. */
export function createReadyQueryResource<K, T>(
  source: () => K | undefined | null | false,
  load: (key: K) => Promise<T>,
): [Resource<T>, Accessor<QueryNotReadyError | null>] {
  const [pending, setPending] = createSignal<QueryNotReadyError | null>(null);
  const root = createMemo(() => graphMeta()?.root);
  // graphBinding() is a plain counter. Observe its lifecycle notification;
  // an epoch repaint only triggers a check and is not itself a new identity.
  const [rebound, setRebound] = createSignal(graphBinding());
  onCleanup(onGraphRebound(() => setRebound(graphBinding())));
  const binding = createMemo(() => { rebound(); graphEpoch(); root(); graphTransitioning(); return graphBinding(); });
  type Request = { key: K; controller: AbortController; binding: number; root: string | undefined };
  const request = createMemo<Request | undefined>((previous) => {
    const key = source();
    const currentBinding = binding();
    const currentRoot = root();
    const transitioning = graphTransitioning();
    previous?.controller.abort();
    setPending(null);
    if (key === undefined || key === null || key === false || transitioning) return undefined;
    return { key, controller: new AbortController(), binding: currentBinding, root: currentRoot };
  });
  onCleanup(() => request()?.controller.abort());
  const [result, { mutate }] = createResource(request, (current) => runQueryWhenReady(
    () => load(current.key),
    {
      signal: current.controller.signal,
      isCurrent: () => request() === current && graphBinding() === current.binding
        && graphMeta()?.root === current.root && !graphTransitioning(),
      onPending: setPending,
    },
  ));
  // Retaining rows is safe only within one graph binding, not across a switch.
  createComputed<{ binding: number; root: string | undefined }>((previous) => {
    const next = { binding: binding(), root: root() };
    const transitioning = graphTransitioning();
    if (previous && (previous.binding !== next.binding || previous.root !== next.root || transitioning)) mutate(undefined);
    return next;
  });
  return [result, pending];
}
