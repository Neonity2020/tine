import { Show, createUniqueId, type JSX } from "solid-js";

// **One mixed result, two independently controlled families** (SPEC §7.6, Q3).
//
// A Friendly search answers two questions at once — which PAGES match, and
// which BLOCKS match — and before this they arrived in one flat list under one
// presentation. That made "show the pages as a table and the blocks as a list"
// unsayable, and it made a page's own Display settings unreachable.
//
// This component owns only the SHELL: the two family boundaries, their
// headings, their controls, their pending/failure/empty states and their
// truncation notes. What goes inside each family is the caller's — the page
// renderers in `QueryPageResults.tsx`, and the host's existing block
// renderers, which keep their ordinary editing behaviour untouched.
//
// **An empty family still shows its controls.** A section that vanished when it
// had no rows would take the only way to change what it selects with it — the
// user would be stuck in a state with no way out (I-10).

export type QueryResultKind = "page" | "block";

const HEADING: Record<QueryResultKind, string> = { page: "Pages", block: "Blocks" };

export interface QueryResultFamily {
  kind: QueryResultKind;
  /** The family's Display control, rendered beside its heading. */
  control?: JSX.Element;
  /** The rows. Rendered only when there are some. */
  body: () => JSX.Element;
  /** Whether this family has any row at all, asked separately from `body` so an
   *  empty family can state its empty case rather than render nothing. */
  empty: () => boolean;
  /** The backend's own truncation flag for this family. It is NOT inferred from
   *  the rendered length: a section of exactly 40 rows out of 40 matches and one
   *  out of 4,000 look identical from here. */
  hasMore?: () => boolean;
  /** Text for the shown-row count. Describes what IS shown; it never claims
   *  completeness. */
  countLabel?: () => string;
}

export interface QueryResultSectionsProps {
  /** Mounted in this order, always: Pages before Blocks. */
  families: QueryResultFamily[];
  /** A read is in flight. The family container says so with `aria-busy`, and
   *  the pending text is a live status, not a result. */
  pending?: () => boolean;
  pendingMessage?: () => string;
  /** A read FAILED. It is an alert, and it is deliberately not the empty state:
   *  "no matching pages" and "this read did not complete" are different facts
   *  and a user who cannot tell them apart cannot decide what to do (I-9). */
  failure?: () => string | null;
}

export function QueryResultSections(props: QueryResultSectionsProps): JSX.Element {
  // Unique per MOUNT: the same query can be open in two panes, and a
  // block-derived id would make one section's heading label the other's list.
  const mount = createUniqueId();
  const headingId = (kind: QueryResultKind) => `query-results-${mount}-${kind}`;
  const busy = () => !!props.pending?.();

  return (
    <div class="query-result-sections">
      {props.families.map((family) => (
        <section
          class="query-result-section"
          data-query-result-kind={family.kind}
          aria-labelledby={headingId(family.kind)}
          aria-busy={busy() ? "true" : "false"}
        >
          <header class="query-result-section-header">
            <h3 id={headingId(family.kind)}>{HEADING[family.kind]}</h3>
            <Show when={family.countLabel?.()}>{(label) => (
              <span class="query-result-section-count">{label()}</span>
            )}</Show>
            {family.control}
          </header>

          <Show when={props.failure?.()}>{(message) => (
            <p class="query-result-section-error" role="alert">{message()}</p>
          )}</Show>

          <Show when={busy() && !props.failure?.()}>
            <p class="query-result-section-pending" role="status">
              {props.pendingMessage?.() ?? "Searching…"}
            </p>
          </Show>

          <Show when={!props.failure?.()}>
            <Show
              when={!family.empty()}
              fallback={
                <Show when={!busy()}>
                  <p class="query-result-section-empty">
                    {family.kind === "page" ? "No matching pages." : "No matching blocks."}
                  </p>
                </Show>
              }
            >
              {family.body()}
            </Show>
          </Show>

          <Show when={family.hasMore?.()}>
            <p class="query-result-section-truncated">
              {family.kind === "page"
                ? "More pages match than are shown."
                : "More blocks match than are shown."}
            </p>
          </Show>
        </section>
      ))}
    </div>
  );
}
