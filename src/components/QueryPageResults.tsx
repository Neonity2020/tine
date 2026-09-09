import { For, Show, Switch, Match, createMemo, type JSX } from "solid-js";
import type { MatchSpan, PageKind, QueryHit } from "../types";
import type { PageRow, ViewSettings } from "../editor/queryIr";
import { groupingFromViewValue } from "../editor/queryViewProperties";

// **The Pages half of a mixed result** (SPEC §7.6, Q3).
//
// It exists because a page is not a block. A block result renders through the
// ordinary editable block renderers and keeps its editing behaviour; a page
// result is a NAVIGATION row whose columns come from the page's own authored
// properties. Before this, the workspace rendered both through one presentation
// switch keyed on `hit.entity`, and a page could therefore only ever be a link
// with the block table's headers over it.
//
// It renders what the backend returned, in the order the backend returned it.
// There is no sorting and no truncation here: ordering and sampling happen in
// SQL over the COMPLETE matched set (Q4), so a frontend re-sort would silently
// replace a complete answer with an answer about the rows that happened to fit.

/** A page hit, narrowed. */
export type QueryPageHit = Extract<QueryHit, { entity: "page" }>;

/** **Stored pages key by physical path and kind** (contract §Identity).
 *
 *  Two pages can share a display name at two paths, and a key that used only
 *  the name would make one of them disappear from the list and the other take
 *  its clicks. A virtual reference-name suggestion names no stored page, so it
 *  keys by the name it IS — that is its whole identity. */
export function pageHitKey(hit: QueryPageHit): string {
  return hit.page.path
    ? `page\0${hit.page.path}\0${hit.page.kind}`
    : `name\0${hit.page.name}`;
}

/** The spans of a page hit's NAME, which is what `page_name` evidence indexes.
 *  A page admitted by its content has none, and marking its name from block
 *  evidence would highlight ranges of a string that evidence never described. */
export function pageNameSpans(hit: QueryPageHit): MatchSpan[] {
  return hit.evidence.filter((item) => item.field === "page_name").flatMap((item) => item.spans);
}

/** The spans of a hit's own field. A page admitted by its CONTENT carries
 *  block-content evidence, which indexes the block text `display_text` already
 *  holds — so the field is read off the evidence rather than assumed from the
 *  entity. */
export function hitMatchSpans(hit: QueryHit): MatchSpan[] {
  const field = hit.entity === "page" ? "page_name" : "visible_content";
  const own = hit.evidence.filter((item) => item.field === field);
  const spans = own.length ? own : hit.evidence;
  return spans.flatMap((item) => item.spans);
}

/** Highlight the matched ranges of a text, merging overlaps. */
export function MarkedText(props: { text: string; spans: MatchSpan[] }): JSX.Element {
  const segments = () => {
    const spans = props.spans
      .map((span) => ({
        start: Math.max(0, Math.min(props.text.length, span.start)),
        end: Math.max(0, Math.min(props.text.length, span.end)),
      }))
      .filter((span) => span.end > span.start)
      .sort((a, b) => a.start - b.start || a.end - b.end);
    const merged: MatchSpan[] = [];
    for (const span of spans) {
      const previous = merged[merged.length - 1];
      if (previous && span.start <= previous.end) previous.end = Math.max(previous.end, span.end);
      else merged.push({ ...span });
    }
    const out: { text: string; marked: boolean }[] = [];
    let cursor = 0;
    for (const span of merged) {
      if (span.start > cursor) out.push({ text: props.text.slice(cursor, span.start), marked: false });
      out.push({ text: props.text.slice(span.start, span.end), marked: true });
      cursor = span.end;
    }
    if (cursor < props.text.length) out.push({ text: props.text.slice(cursor), marked: false });
    return out;
  };
  return (
    <For each={segments()}>{(segment) => segment.marked
      ? <mark>{segment.text}</mark>
      : segment.text}</For>
  );
}

const PAGE_KIND_LABEL: Record<PageKind, string> = { page: "Page", journal: "Journal" };

/** **What one display field means on a PAGE row.**
 *
 *  The three page builtins are the page's own attributes (`queryIr.ts::Attr`'s
 *  page row: name, journal, day, namespace). Everything else is an ordinary
 *  authored property, read from the hydrated row in its authored spelling —
 *  never reconstructed from the display name, which is not an identity. A page
 *  the backend did not hydrate has no properties to show, which is different
 *  from having none: the cell is empty either way, and the row still navigates.
 */
export function pageFieldValue(hit: QueryPageHit, field: string): string {
  const name = field.startsWith("prop:") ? field.slice(5) : field;
  const row: PageRow | undefined = hit.row;
  switch (name) {
    case "name":
      return hit.page.name;
    case "kind":
      return PAGE_KIND_LABEL[hit.page.kind];
    case "day":
    case "journal-day":
    case "journal_day":
      return row?.journal_day != null ? String(row.journal_day)
        : hit.page.date_key != null ? String(hit.page.date_key) : "";
    default: {
      const key = name.trim().toLowerCase();
      const found = (row?.properties ?? []).find(([property]) => property.trim().toLowerCase() === key);
      return found ? found[1] : "";
    }
  }
}

export function pageFieldLabel(field: string): string {
  const name = field.startsWith("prop:") ? field.slice(5) : field;
  switch (name) {
    case "name": return "Name";
    case "kind": return "Kind";
    case "day": case "journal-day": case "journal_day": return "Journal day";
    default: return name;
  }
}

export interface QueryPageResultsProps {
  hits: () => QueryPageHit[];
  view: () => ViewSettings;
  /** Open the page a row names. The event comes with it because a host may
   *  read its modifiers: the same click is "here", "in the sidebar", "in a
   *  background tab" or "in the other pane" depending on them. */
  onOpen: (hit: QueryPageHit, event: MouseEvent) => void;
  /** A stable in-page-find surface id, where the host has one. */
  surfaceId?: (hit: QueryPageHit) => string;
  /** Extra attributes for the row's navigation control. A host with the
   *  application's link gestures supplies its `onMouseDown`/`onAuxClick` here
   *  rather than reimplementing the row: the control is one thing, and two
   *  copies of it drift. */
  linkAttrs?: (hit: QueryPageHit) => JSX.HTMLAttributes<HTMLButtonElement>;
  /** An extra class on the navigation control, where the host already styles
   *  its own page rows. */
  linkClass?: string;
}

/** The one navigation control of a page row. It is a real control INSIDE the
 *  listitem rather than the listitem itself: a button that is also the row has
 *  no row semantics left for a screen reader to announce. */
function PageLink(props: {
  hit: QueryPageHit;
  onOpen: (hit: QueryPageHit, event: MouseEvent) => void;
  surfaceId?: string;
  attrs?: JSX.HTMLAttributes<HTMLButtonElement>;
  extraClass?: string;
}): JSX.Element {
  return (
    <button
      type="button"
      class={props.extraClass ? `query-page-link ${props.extraClass}` : "query-page-link"}
      data-page-path={props.hit.page.path}
      data-page-kind={props.hit.page.kind}
      {...(props.surfaceId ? { "data-inpage-find-surface": props.surfaceId } : {})}
      onClick={(event) => props.onOpen(props.hit, event)}
      {...(props.attrs ?? {})}
    >
      <span class="query-page-name">
        <MarkedText text={props.hit.page.name} spans={pageNameSpans(props.hit)} />
      </span>
      <Show when={props.hit.matched_alias}>{(alias) => (
        <span class="query-page-alias"> (matched alias {alias()})</span>
      )}</Show>
    </button>
  );
}

export function QueryPageResults(props: QueryPageResultsProps): JSX.Element {
  const presentation = () => props.view().view ?? "list";
  const columns = () => props.view().columns ?? [];
  const grouping = createMemo(() => groupingFromViewValue(props.view().group_by));
  /** **Adjacency, not identity** — the same rule the block section uses.
   *
   *  With an explicit sort the backend's order is authoritative, so one group
   *  value may legitimately appear more than once. Re-clustering by value would
   *  reorder rows the backend deliberately placed. */
  const boardGroups = createMemo(() => {
    const field = grouping();
    const out: [string, QueryPageHit[]][] = [];
    for (const hit of props.hits()) {
      const key = field.kind === "field" ? pageFieldValue(hit, field.field) : "";
      const last = out[out.length - 1];
      if (last && last[0] === key) last[1].push(hit);
      else out.push([key, [hit]]);
    }
    return out;
  });
  const surface = (hit: QueryPageHit) => props.surfaceId?.(hit);
  /** One row control, everywhere. The host's gesture attributes and its class
   *  ride along so the four presentations cannot grow four different links. */
  const link = (hit: QueryPageHit) => (
    <PageLink
      hit={hit}
      onOpen={props.onOpen}
      surfaceId={surface(hit)}
      {...(props.linkAttrs ? { attrs: props.linkAttrs(hit) } : {})}
      {...(props.linkClass ? { extraClass: props.linkClass } : {})}
    />
  );
  /** The matched excerpt, shown only when it says something the page's own name
   *  does not. A page admitted by its CONTENT carries the matching block's
   *  text; a page admitted by its NAME carries the name again, and printing it
   *  twice reads as two different facts. */
  const excerpt = (hit: QueryPageHit) => hit.display_text !== hit.page.name;

  return (
    <Switch>
      <Match when={presentation() === "search"}>
        <div class="query-results-search" role="list" aria-label="Page results">
          <For each={props.hits()}>{(hit) => (
            <div role="listitem" data-page-key={pageHitKey(hit)}>
              <span class="switcher-kind">page</span>
              <Show when={excerpt(hit)}>
                <span class="search-result-excerpt">
                  <MarkedText text={hit.display_text} spans={hitMatchSpans(hit)} />
                </span>
              </Show>
              {link(hit)}
            </div>
          )}</For>
        </div>
      </Match>

      <Match when={presentation() === "list"}>
        <ul class="query-results-list" aria-label="Page results">
          <For each={props.hits()}>{(hit) => (
            <li data-page-key={pageHitKey(hit)}>
              {link(hit)}
              <Show when={excerpt(hit)}>
                <span class="query-list-text">
                  <MarkedText text={hit.display_text} spans={hitMatchSpans(hit)} />
                </span>
              </Show>
            </li>
          )}</For>
        </ul>
      </Match>

      <Match when={presentation() === "table"}>
        <div class="query-results-table-wrap">
          <table class="query-results-table">
            <caption class="sr-only">Page results</caption>
            <thead>
              <tr>
                <th scope="col">Page</th>
                <For each={columns()}>{(column) => <th scope="col">{pageFieldLabel(column)}</th>}</For>
              </tr>
            </thead>
            <tbody>
              <For each={props.hits()}>{(hit) => (
                <tr data-page-key={pageHitKey(hit)}>
                  <td>{link(hit)}</td>
                  <For each={columns()}>{(column) => <td>{pageFieldValue(hit, column)}</td>}</For>
                </tr>
              )}</For>
            </tbody>
          </table>
        </div>
      </Match>

      <Match when={presentation() === "board"}>
        <div class="query-results-board" aria-label="Page results grouped">
          <For each={boardGroups()}>{([value, groupHits]) => (
            <section class="query-board-column" aria-label={value || "No value"}>
              <h4>{value || "No value"}<span class="query-board-count">{groupHits.length}</span></h4>
              <div role="list" aria-label="Page results">
                <For each={groupHits}>{(hit) => (
                  <div role="listitem" class="query-board-card" data-page-key={pageHitKey(hit)}>
                    {link(hit)}
                  </div>
                )}</For>
              </div>
            </section>
          )}</For>
        </div>
      </Match>
    </Switch>
  );
}
