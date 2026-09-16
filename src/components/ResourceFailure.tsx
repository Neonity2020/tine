import { Show, type JSX } from "solid-js";
import type { Resource } from "solid-js";

/**
 * The failure row for a panel whose EMPTINESS WOULD LIE.
 *
 * `readOr` stops a rejected resource throwing into render, which is right for a
 * decorative value: a missing icon or an un-typeset formula degrades to what the
 * site already drew for "not loaded yet". But a references panel, a query, or a
 * page list that renders empty is not degrading — it is asserting "there are
 * none", and that assertion is false. Those panels pair `readOr` with this row,
 * so the user is told the difference between "nothing here" and "we could not
 * find out".
 *
 * Deliberately not a boundary. A boundary sized to one panel would work, but it
 * costs the panel's whole subtree and its state; a panel that knows its own
 * resource failed can keep its header, its controls, and its place on the page.
 * `FailureBoundary` stays for what a component cannot anticipate.
 */
export function ResourceFailure(props: {
  /** Any resource this panel needs; the row shows when one has failed. */
  of: Resource<unknown> | readonly Resource<unknown>[];
  /** What could not be loaded, in the user's words: "references", "this query". */
  what: string;
  /** Refetch, when the panel has one. Omitted for a resource that cannot retry. */
  onRetry?: () => void;
}): JSX.Element {
  const failed = () => {
    const list = Array.isArray(props.of) ? props.of : [props.of as Resource<unknown>];
    return list.some((resource) => resource.error !== undefined);
  };
  return (
    <Show when={failed()}>
      <div class="resource-failure" role="alert">
        <span class="resource-failure-text">Couldn’t load {props.what}.</span>
        <Show when={props.onRetry}>
          <button type="button" class="resource-failure-retry" onClick={() => props.onRetry?.()}>
            Retry
          </button>
        </Show>
      </div>
    </Show>
  );
}
