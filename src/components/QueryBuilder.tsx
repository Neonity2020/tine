import {
  For,
  Show,
  createEffect,
  createMemo,
  createResource,
  createSignal,
  createUniqueId,
  onCleanup,
  type JSX,
} from "solid-js";
import { Portal } from "solid-js/web";
import { backend } from "../backend";
import {
  builderRoot,
  currentAgg,
  currentGroup,
  currentSort,
  filterLabel,
  removeAt,
  sortLabel,
  withAgg,
  withGroup,
  withSort,
  SORT_PRESETS,
  type AggState,
  type SortPreset,
} from "../editor/queryBuilder";
import type {
  AggFn,
  Anchor,
  Filter,
  ParsedQuery,
  Query,
  QueryPrintDialect,
  RegistryRow,
  ViewSettings,
} from "../editor/queryIr";
import {
  QuerySentence,
  QuerySheet,
  countConditions,
  rawLeaves,
  registerVisiblePopover,
  stop,
  type AnchorPrompt,
  type QueryFacets,
  type QueryFacetsAccessor,
  type RegistryAccess,
} from "./QuerySheet";
import { sharedQueryResult } from "../queryResultCache";
import { dataRev, graphEpoch, graphMeta, queryBuilderAutoOpen, setQueryBuilderAutoOpen } from "../ui";
import { dismissOnOutsidePointer, registerTransientLayer } from "../transientLayers";

// **The visual query builder: a resting SENTENCE that expands into a SHEET**
// (SPEC §7.2–§7.4).
//
// It used to be a chip bar over a DSL STRING — parse the text, edit a private
// `Clause` tree, print the text back. Both ends of that round trip were a second
// implementation of a language Rust already owns (I-12). Then it was a chip bar
// over the IR. It is now two states: at rest one plain-English line with the
// result count and a ⚙, and while editing a sheet of rows over the same IR.
//
// This file is the HOST. It owns the session, the two graph-level reads, the
// anchor-switch preview and the dismissal layer; `QuerySheet.tsx` owns what the
// sheet draws. The sort/summarize controls and the text pane below are P5's and
// P4's respectively and are unchanged — they simply moved into the sheet's
// footer.

export type { RegistryAccess, QueryFacets, QueryFacetsAccessor };

/** The pair an edit session holds (§4.3.1). `query` carries the anchor, the
 *  filter, the diagnostics and the authored source — including the opaque
 *  options map, which only Rust ever splits or appends. */
export interface BuilderSession {
  query: Query;
  view: ViewSettings;
}

const errorMessage = (error: unknown): string =>
  error instanceof Error ? error.message : String(error);

// A small "+ sort" / "sort: field ↑" control in the bar (NOT a filter chip).
// The popover leads with one-click presets (the common cases — no typing, no
// syntax to get wrong) and keeps a free-text row for sorting by any other
// property. `SORT_PRESETS` is the single source of truth (see queryBuilder.ts).
//
// **It edits `ViewSettings`, not the filter (§7.6, Q15).** Sort used to live in
// the clause tree as a fake `sortBy` child, which is why wrapping it in an OR
// silently disabled it and why the chip menu had to special-case it. Presentation
// is now a separate value and the printers re-emit it.
function SortControl(props: {
  view: () => ViewSettings;
  apply: (view: ViewSettings) => void;
  parentTransientId?: string;
}): JSX.Element {
  const [open, setOpen] = createSignal(false);
  const cur = () => currentSort(props.view());
  // The free-text escape hatch: sort by an arbitrary property name.
  const [field, setField] = createSignal("");
  const [dir, setDir] = createSignal<"asc" | "desc">("asc");
  let triggerEl: HTMLButtonElement | undefined;
  let pickerEl: HTMLDivElement | undefined;
  const layerId = `query-sort-${createUniqueId()}`;
  registerVisiblePopover(open, {
    id: layerId,
    parentId: props.parentTransientId,
    root: () => pickerEl ?? null,
    trigger: () => triggerEl ?? null,
    dismiss: () => { setOpen(false); return true; },
  });
  const isPreset = (c: { field: string; dir: "asc" | "desc" } | null) =>
    !!c && SORT_PRESETS.some((p) => p.field === c.field && p.dir === c.dir);
  const activePreset = (p: SortPreset) => {
    const c = cur();
    return !!c && c.field === p.field && c.dir === p.dir;
  };
  const openPopover = () => {
    const c = cur();
    // Pre-fill the free-text row only for a non-preset (custom-property) sort;
    // a preset sort is reflected by its highlighted button instead.
    setField(c && !isPreset(c) ? c.field : "");
    setDir(c?.dir ?? "asc");
    setOpen(true);
  };
  const applyPreset = (p: SortPreset) => {
    props.apply(withSort(props.view(), { field: p.field, dir: p.dir }));
    setOpen(false);
  };
  const applyCustom = () => {
    if (!field().trim()) return;
    props.apply(withSort(props.view(), { field: field().trim(), dir: dir() }));
    setOpen(false);
  };
  const clearSort = () => {
    props.apply(withSort(props.view(), null));
    setOpen(false);
  };
  return (
    <span class="qb-add-wrap">
      {/* A stable "+ sort" affordance — it does NOT morph into the current sort
          value. It just gains an `active` highlight and opens the popover. */}
      <button
        ref={triggerEl}
        class="qb-sort"
        classList={{ active: !!cur() }}
        title={cur() ? `Sorted by ${sortLabel(cur()!.field, cur()!.dir)}. Click to change.` : "Sort results"}
        onClick={(e) => { stop(e); open() ? setOpen(false) : openPopover(); }}
      >
        {cur() ? `sort: ${sortLabel(cur()!.field, cur()!.dir)}` : "+ sort"}
      </button>
      <Show when={open()}>
        <div ref={pickerEl} class="qb-picker qb-sort-picker" onClick={stop}>
          <div class="qb-picker-title">Sort by</div>
          {/* One click = applied. No typing for the common cases. */}
          <div class="qb-sort-presets">
            <For each={SORT_PRESETS}>
              {(p) => (
                <button
                  class="qb-sort-preset"
                  classList={{ active: activePreset(p) }}
                  title={p.hint}
                  onClick={() => applyPreset(p)}
                >
                  {p.label}
                </button>
              )}
            </For>
          </div>
          <div class="qb-divider" />
          {/* Escape hatch: sort by any other property (still no required syntax —
              just the bare property name + a direction). */}
          <div class="qb-sort-custom-label">Or by a property</div>
          <input
            class="qb-input"
            placeholder="property name (e.g. rating)"
            value={field()}
            onInput={(e) => setField(e.currentTarget.value)}
            onKeyDown={(e) => { if (e.key === "Enter") applyCustom(); }}
          />
          <div class="qb-conn-row">
            <button class="qb-conn" classList={{ active: dir() === "asc" }} onClick={() => setDir("asc")}>Asc ↑</button>
            <button class="qb-conn" classList={{ active: dir() === "desc" }} onClick={() => setDir("desc")}>Desc ↓</button>
            <button class="qb-conn" classList={{ disabled: !field().trim() }} onClick={applyCustom}>Apply</button>
          </div>
          <Show when={cur()}>
            <button class="qb-sort-clear" onClick={clearSort}>Clear sort</button>
          </Show>
        </div>
      </Show>
    </span>
  );
}

// A "+ summarize" control: no-code aggregation (count / sum / average of a
// property) and grouping (by page or a property). Modeled on SortControl — a
// single pill + popover, dismiss-on-outside-click. Aggregate + group are
// independent (you can group by page AND count per group). The numbers are
// computed in the frontend from the returned block list (Macro.tsx); this edits
// the VIEW SETTINGS the printers re-emit (§7.6).
function SummarizeControl(props: {
  view: () => ViewSettings;
  apply: (view: ViewSettings) => void;
  facets: QueryFacetsAccessor;
  parentTransientId?: string;
}): JSX.Element {
  const [open, setOpen] = createSignal(false);
  // Two-step property choice: null = show the top-level buttons; "sum"/"avg" =
  // pick a property to aggregate; "group" = pick a property to group by.
  const [pick, setPick] = createSignal<"sum" | "avg" | "group" | null>(null);
  const keys = () => (props.facets() ?? []).map(([k]) => k);
  const agg = () => currentAgg(props.view());
  const group = () => currentGroup(props.view());
  const active = () => !!agg() || !!group();
  let triggerEl: HTMLButtonElement | undefined;
  let pickerEl: HTMLDivElement | undefined;
  const layerId = `query-summarize-${createUniqueId()}`;
  registerVisiblePopover(open, {
    id: layerId,
    parentId: props.parentTransientId,
    root: () => pickerEl ?? null,
    trigger: () => triggerEl ?? null,
    dismiss: () => { setOpen(false); return true; },
  });
  const openPopover = () => {
    setPick(null);
    setOpen(true);
  };
  // Each pick applies and closes the popover (like SortControl's presets). To set
  // BOTH an aggregate and a grouping, reopen — the two are independent, so the
  // view keeps whichever the other pick already set.
  const setAgg = (a: AggState | null) => {
    props.apply(withAgg(props.view(), a));
    setPick(null);
    setOpen(false);
  };
  const setGroup = (f: string | null) => {
    props.apply(withGroup(props.view(), f));
    setPick(null);
    setOpen(false);
  };
  const label = () => {
    const parts: string[] = [];
    const a = agg();
    if (a) parts.push(a.agg === "count" ? "count" : `${a.agg} of ${a.field ?? "?"}`);
    const g = group();
    if (g) parts.push(`by ${g}`);
    return parts.join(", ");
  };
  return (
    <span class="qb-add-wrap">
      <button
        ref={triggerEl}
        class="qb-sort"
        classList={{ active: active() }}
        title={active() ? `Summary: ${label()}. Click to change.` : "Summarize results (count / sum / average / group)"}
        onClick={(e) => { stop(e); open() ? setOpen(false) : openPopover(); }}
      >
        {active() ? `∑ ${label()}` : "+ summarize"}
      </button>
      <Show when={open()}>
        <div ref={pickerEl} class="qb-picker" onClick={stop}>
          {/* Step: pick a property for sum / avg / group-by. */}
          <Show when={pick() != null} fallback={
            <>
              <div class="qb-picker-title">Aggregate</div>
              <button class="qb-menu-item" classList={{ active: agg()?.agg === "count" }} onClick={() => setAgg({ agg: "count", field: null })}>Count</button>
              <button class="qb-menu-item" classList={{ active: agg()?.agg === "sum" }} onClick={() => setPick("sum")}>Sum of a property…</button>
              <button class="qb-menu-item" classList={{ active: agg()?.agg === "avg" }} onClick={() => setPick("avg")}>Average of a property…</button>
              <Show when={agg()}>
                <button class="qb-sort-clear" onClick={() => setAgg(null)}>Clear aggregate</button>
              </Show>
              <div class="qb-divider" />
              <div class="qb-picker-title">Group by</div>
              <button class="qb-menu-item" classList={{ active: group() === "page" }} onClick={() => setGroup("page")}>Page</button>
              <button class="qb-menu-item" classList={{ active: !!group() && group() !== "page" }} onClick={() => setPick("group")}>Property…</button>
              <Show when={group()}>
                <button class="qb-sort-clear" onClick={() => setGroup(null)}>Clear grouping</button>
              </Show>
            </>
          }>
            <div class="qb-picker-title">{pick() === "group" ? "Group by property" : `${pick() === "sum" ? "Sum" : "Average"} of property`}</div>
            <For each={keys()}>
              {(k) => (
                <button class="qb-menu-item" onClick={() => (pick() === "group" ? setGroup(k) : setAgg({ agg: pick() as AggFn, field: k }))}>
                  {k}
                </button>
              )}
            </For>
            <PropNameInput onCommit={(k) => (pick() === "group" ? setGroup(k) : setAgg({ agg: pick() as AggFn, field: k }))} />
          </Show>
        </div>
      </Show>
    </span>
  );
}

// A free-text property-name input (Enter commits) for the summarize picker, so a
// property not yet used in the graph (absent from facets) can still be chosen.
function PropNameInput(props: { onCommit: (key: string) => void }): JSX.Element {
  const [v, setV] = createSignal("");
  return (
    <input
      class="qb-input"
      placeholder="or type a property name"
      value={v()}
      onInput={(e) => setV(e.currentTarget.value)}
      onKeyDown={(e) => { if (e.key === "Enter" && v().trim()) props.onCommit(v().trim()); }}
    />
  );
}

// ---------------------------------------------------------------------------
// The text pane (§4.3.1, §7.1)
// ---------------------------------------------------------------------------

/** How long the pane waits after the last keystroke before asking the engine.
 *  §7.1's ~150 ms: long enough that ordinary typing is one parse, short enough
 *  that the rows follow the text rather than trailing it. */
const PANE_DEBOUNCE_MS = 150;

/** The query text pane.
 *
 *  **One implementation, two dialects.** A query block edits TQL and the query
 *  workspace edits the OG DSL, because that is the text each of them persists —
 *  but "debounce, parse, keep the last good reading, drop a stale response" is
 *  the same question in both, so it is answered once (I-12). The dialect is an
 *  input, not a second pane.
 *
 *  The contract it implements, from §4.3.1:
 *
 *  - A failed parse **keeps the draft and the last-good session**, shows the
 *    parser's OWN message, and disables save. It never blanks the pane and never
 *    writes to disk. The bar above renders greyed while this holds, so the rows
 *    on screen are visibly "what still ran", not "what you just typed".
 *  - **I-20:** a monotonically increasing edit revision discards stale parse
 *    responses. A slow answer for text the user has since retyped is DROPPED,
 *    not rendered — the failure mode it prevents is a late success overwriting a
 *    later edit's error, which reads as "my typo was accepted".
 *  - Saving waits for a successful parse of the CURRENT revision, so the pending
 *    state disables save too. There is no spinner that outlives a response:
 *    every settled revision clears it, including a dropped one.
 *  - The pane is not an options editor. */
function QueryTextPane(props: {
  session: () => BuilderSession | undefined;
  dialect: Extract<QueryPrintDialect, "og" | "tql">;
  /** A successful parse of the current revision: the new filter/anchor, ready to
   *  be shown. Carry-forward of the view and the opaque options is the caller's
   *  (`QueryBuilder`'s), because it owns the session. */
  onParsed: (query: Query) => void;
  /** Commit the last-good parse. Enabled only when the current revision parsed. */
  onCommit: (query: Query) => void;
  onStale: (stale: boolean) => void;
  alwaysOpen?: boolean;
}): JSX.Element {
  const [draft, setDraft] = createSignal<string | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [pending, setPending] = createSignal(false);
  const [good, setGood] = createSignal<Query | null>(null);
  // The edit revision. `settled` is the newest revision whose response we
  // accepted; a response for anything older is dropped unrendered (I-20).
  let revision = 0;
  let settled = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(timer));

  // The pane is collapsed until the user asks for it, and a closed pane prints
  // nothing: the text it would show is an IPC round trip per query block on the
  // page, spent on bytes nobody is looking at.
  const [open, setOpen] = createSignal(!!props.alwaysOpen);

  // The session's own text is PRINTED BY RUST. The pane never renders a query it
  // spelled itself — that was the twin this packet removed.
  const [printed] = createResource(
    () => (open() ? props.session() : undefined),
    async (session) => {
      try {
        return { text: await backend().printQuery(session.query, session.view, props.dialect), refusal: null };
      } catch (error) {
        return { text: null, refusal: errorMessage(error) };
      }
    },
  );

  // A session arriving from outside the pane (a chip edit, a save landing, a
  // different block) invalidates every outstanding response and the draft with
  // it (§4.3.1: "switching host block or closing the session invalidates
  // outstanding responses").
  createEffect(() => {
    props.session();
    revision += 1;
    settled = revision;
    clearTimeout(timer);
    setDraft(null);
    setError(null);
    setPending(false);
    setGood(null);
    props.onStale(false);
  });

  const text = () => draft() ?? printed.latest?.text ?? "";
  const refusal = () => (draft() === null ? printed.latest?.refusal ?? null : null);

  const run = async (source: string, mine: number) => {
    try {
      const parsed = await backend().parseQuery(source, props.dialect);
      // I-20: the user has typed since; this answer is about text that no longer
      // exists. Dropping it is the whole point — rendering it would replace a
      // newer reading with an older one.
      if (mine <= settled) return;
      settled = mine;
      setPending(false);
      // A diagnostic inside an `off` subtree carries `disabled` and does not
      // invalidate (§3.5) — a parse with only disabled diagnostics is successful
      // and saveable.
      const blocking = (parsed.query.diagnostics ?? []).filter((d) => !d.disabled);
      if (blocking.length) {
        setError(blocking.map((d) => d.message).join(" · "));
        props.onStale(true);
        return;
      }
      setError(null);
      setGood(parsed.query);
      props.onStale(false);
      props.onParsed(parsed.query);
    } catch (error) {
      if (mine <= settled) return;
      settled = mine;
      setPending(false);
      setError(errorMessage(error));
      props.onStale(true);
    }
  };

  const onInput = (next: string) => {
    setDraft(next);
    revision += 1;
    const mine = revision;
    clearTimeout(timer);
    // The pane is not an options editor (§4.3.1). TQL has no braces and the OG
    // DSL's form never ends in one, so a trailing `}` is an options map that was
    // pasted here — which is a different control, not a parse error. This makes
    // no claim about WHERE the map starts; splitting one is Rust's job and only
    // Rust's.
    if (next.trim().endsWith("}")) {
      settled = mine;
      setPending(false);
      setError("The options map (title, collapsed) is edited with the title and Display controls, not here.");
      props.onStale(true);
      return;
    }
    setPending(true);
    timer = setTimeout(() => void run(next, mine), PANE_DEBOUNCE_MS);
  };

  const savable = () => draft() !== null && !pending() && !error() && good() !== null;

  const body = () => (
    <div class="query-text-pane">
      <textarea
        class="qb-input query-text-pane-input"
        classList={{ "query-text-pane-invalid": !!error() }}
        rows={3}
        spellcheck={false}
        aria-label={props.dialect === "tql" ? "Query text (TQL)" : "Query expression"}
        aria-invalid={error() ? "true" : undefined}
        value={text()}
        disabled={!!refusal()}
        onInput={(event) => onInput(event.currentTarget.value)}
      />
      <div class="query-text-pane-status">
        <Show when={refusal()}>
          {(message) => <span class="query-text-pane-error" role="alert">{message()}</span>}
        </Show>
        <Show when={error()}>
          {/* The parser's OWN message, never a catch-all (I-9). The rows above
              stay on screen and greyed; they are the last reading that ran. */}
          {(message) => <span class="query-text-pane-error" role="alert">{message()}</span>}
        </Show>
        <Show when={pending() && !error()}>
          <span class="query-text-pane-pending">Checking…</span>
        </Show>
        <button
          type="button"
          class="qb-commit query-text-pane-save"
          disabled={!savable()}
          onClick={() => { const q = good(); if (q) props.onCommit(q); }}
        >
          Save query text
        </button>
      </div>
    </div>
  );

  return (
    <Show when={!props.alwaysOpen} fallback={body()}>
      <details
        class="query-text-pane-details"
        onClick={stop}
        onToggle={(event) => setOpen(event.currentTarget.open)}
      >
        <summary>{props.dialect === "tql" ? "Query text" : "Raw query DSL"}</summary>
        <Show when={open()}>{body()}</Show>
      </details>
    </Show>
  );
}

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

/** Deepest-and-rightmost first, so removing several leaves in one pass never
 *  invalidates a `loc` that has not been used yet. */
function compareLocsDescending(a: number[], b: number[]): number {
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const left = a[i] ?? -1;
    const right = b[i] ?? -1;
    if (left !== right) return right - left;
  }
  return 0;
}

/** The four "Try:" suggestions of the empty state (§7.3).
 *
 *  **SPEC §7.3 is the authority, not the design pass:** the four are the most
 *  frequent PROPERTY keys the registry holds. The design pass's marker / page /
 *  date mix has no frequency source in the engine — the registry holds property
 *  rows only, and `referencedPageNames` carries no counts — and a TypeScript
 *  walk of the graph to invent one is exactly the whole-graph-work-per-render
 *  shape I-13 forbids and D-14 says not to build a twin of. Fewer than four
 *  rows shows what exists; an empty registry shows no line at all. */
export function suggestedKeys(rows: RegistryRow[] | undefined): string[] {
  if (!rows?.length) return [];
  return [...rows]
    .sort((a, b) => {
      const byCount =
        b.count_blocks + b.count_pages - (a.count_blocks + a.count_pages);
      return byCount !== 0 ? byCount : a.normalized_name.localeCompare(b.normalized_name);
    })
    .slice(0, 4)
    .map((row) => row.normalized_name);
}

export function QueryBuilder(props: {
  /** The persisted reading of the query. `undefined` while the engine has not
   *  answered yet — the builder renders nothing rather than an empty query it
   *  would then be able to save over the author's text. */
  session: () => BuilderSession | undefined;
  /** Persist an edit. Row edits call this immediately (each one is a complete,
   *  valid IR); the pane calls it only when the user saves a parse. */
  onChange: (next: BuilderSession) => void;
  /** The text pane's language: `tql` for a query block, `og` for the workspace,
   *  which materializes OG text. */
  paneDialect?: Extract<QueryPrintDialect, "og" | "tql">;
  paneAlwaysOpen?: boolean;
  /** The workspace's permanently expanded sheet (§7.2). Inline, not portalled,
   *  and not a dismissable layer of its own. */
  sheetAlwaysOpen?: boolean;
  /** The live result count, rendered beside the sentence (§7.2). */
  total?: JSX.Element;
  /** The pane's text no longer parses, so the rows on screen are the LAST
   *  reading that ran. The host greys them; the builder cannot, because the
   *  results are not its children. */
  onStale?: (stale: boolean) => void;
  blockId?: string;
  parentTransientId?: string;
}): JSX.Element {
  // The pane's last-good parse, not yet saved. `null` = the builder shows the
  // persisted reading. This is what makes "the rows follow the text you typed"
  // and "nothing reaches disk until you save it" both true (§4.3.1).
  const [paneQuery, setPaneQuery] = createSignal<Query | null>(null);
  const [stale, setStale] = createSignal(false);
  const [open, setOpen] = createSignal(false);
  const [openMenu, setOpenMenu] = createSignal<string | null>(null);
  const [anchorPrompt, setAnchorPrompt] = createSignal<AnchorPrompt | null>(null);
  const [previewError, setPreviewError] = createSignal<string | null>(null);

  // **I-20: an async answer lands only on the state it was computed for.**
  // The anchor preview is a print-then-parse round trip, so a slow answer for
  // an anchor the user has since changed must be DROPPED, not rendered — the
  // failure it prevents is a late "2 conditions don't apply" prompt about a
  // switch that is no longer pending. A session identity check alone is not
  // enough: two anchor clicks race under one unchanged host session, which is
  // why this is a monotonic revision with a settled watermark, exactly as the
  // text pane does it.
  let anchorRevision = 0;
  let anchorSettled = 0;
  const invalidateAnchorPreview = () => {
    anchorRevision += 1;
    anchorSettled = anchorRevision;
    setAnchorPrompt(null);
  };
  onCleanup(invalidateAnchorPreview);

  createEffect(() => {
    props.session();
    setPaneQuery(null);
    invalidateAnchorPreview();
    setPreviewError(null);
  });

  const session = createMemo<BuilderSession | undefined>(() => {
    const persisted = props.session();
    if (!persisted) return undefined;
    const pane = paneQuery();
    return pane ? { query: pane, view: persisted.view } : persisted;
  });
  // The sheet always edits an `and`/`or` root, so "+ add condition" has
  // somewhere to add. A single-child `and` prints back as the bare child.
  const root = createMemo(() => builderRoot(session()?.query.filter ?? { kind: "and", items: [] }));
  const view = () => session()?.view ?? {};

  const sheetOpen = () => !!props.sheetAlwaysOpen || open();

  // N builders on one page asked the SAME whole-graph facets question N times
  // per (graphEpoch, dataRev). The scope is per-builder by decision (P0), so the
  // fix is not a shared scope but a shared REQUEST: `sharedQueryResult` collapses
  // identical in-flight/resolved work under its own key namespace, exactly as the
  // page-tag query does. Harvest W4-P1 item 3.
  //
  // **And it is LAZY (I-13).** The key is `undefined` while no sheet is open, so
  // a page of resting sentences issues zero graph-level calls; the first sheet
  // that opens issues exactly one, which the sharing keeps shared with every
  // other builder on the page. (P4 replaces the facets read itself with the
  // registry, §6.4; this packet only stops it happening at rest.)
  const [facets] = createResource(
    () => (sheetOpen() ? `${graphEpoch()}\0${dataRev()}` : undefined),
    (requestKey) =>
      sharedQueryResult(
        `${graphMeta()?.root ?? ""}\0${graphEpoch()}`,
        `query-facets\0${requestKey}`,
        () => backend().queryFacets(),
      ),
  );
  // **The ONE registry read (§6.4, K20, I-13).** `query_registry` is a
  // graph-level table. It is fetched when a sheet opens and again after a
  // declaration is written — never on a keystroke, never per row. There is no
  // generation signal from the projection to TypeScript, so there is
  // deliberately no third trigger.
  const [registryRequests, setRegistryRequests] = createSignal(0);
  const [registrySnapshot] = createResource(
    () => (registryRequests() > 0 ? `${graphEpoch()}\0${registryRequests()}` : undefined),
    () => backend().queryRegistry(),
  );
  const registry: RegistryAccess = {
    rows: () => registrySnapshot.latest?.rows,
    request: () => setRegistryRequests((n) => n + 1),
  };
  const suggestions = createMemo(() => suggestedKeys(registry.rows()));

  // Open the sheet with the field chooser focused when this block was just
  // created via `/query` — consume the one-shot flag so only this block does.
  const autoOpen = !!props.blockId && queryBuilderAutoOpen() === props.blockId;
  if (autoOpen) {
    setQueryBuilderAutoOpen(null);
    setOpen(true);
  }

  /** A row edit: a new filter over the CURRENT reading, saved immediately. */
  const apply = (next: Filter) => {
    const current = session();
    if (!current) return;
    invalidateAnchorPreview();
    props.onChange({ query: { ...current.query, filter: next }, view: current.view });
    setOpenMenu(null);
  };
  const applyView = (next: ViewSettings) => {
    const current = session();
    if (!current) return;
    props.onChange({ query: current.query, view: next });
  };
  /** §4.3.1 carry-forward: a parse replaces only the filter, the anchor and the
   *  diagnostics. The session's view and its OPAQUE options survive — an absent
   *  map in pane text never means "delete the title". */
  const carryForward = (parsed: Query): Query => {
    const current = props.session();
    const options = current && current.query.source.kind !== "builder"
      ? current.query.source.og_options ?? ""
      : "";
    return {
      anchor: parsed.anchor,
      filter: parsed.filter,
      diagnostics: parsed.diagnostics,
      source: parsed.source.kind === "builder"
        ? parsed.source
        : { ...parsed.source, og_options: options },
    };
  };
  const commitQuery = (query: Query) => {
    const current = session();
    if (!current) return;
    props.onChange({ query, view: current.view });
  };

  /**
   * **Switching the anchor re-validates through the ENGINE (§7.4, §3.5, D-14).**
   *
   * The frontend has no "does this leaf apply to this row" oracle and must not
   * grow one — that is the twin this campaign removed. So the preview is the
   * engine answering: print the query under the new anchor, parse it back, and
   * read the `not_applicable` diagnostics the lowering raised. The leaves that
   * do not apply come back RETAINED (`Raw(NotApplicable)`, P3's Rust half), so
   * "keep anyway" keeps the author's condition rather than a memory of it.
   */
  const switchAnchor = async (anchor: Anchor) => {
    const current = session();
    if (!current || current.query.anchor === anchor) return;
    anchorRevision += 1;
    const mine = anchorRevision;
    setAnchorPrompt(null);
    setPreviewError(null);
    const next: Query = { ...current.query, anchor };
    let parsed: ParsedQuery;
    try {
      const text = await backend().printQuery(next, current.view, "tql");
      parsed = await backend().parseQuery(text, "tql");
    } catch (error) {
      if (mine <= anchorSettled) return;
      anchorSettled = mine;
      setPreviewError(errorMessage(error));
      return;
    }
    if (mine <= anchorSettled) return;
    anchorSettled = mine;
    const carried = carryForward(parsed.query);
    const notApplicable = (carried.diagnostics ?? []).filter((d) => d.kind === "not_applicable");
    const live = notApplicable.filter((d) => d.disabled !== true);
    if (live.length === 0) {
      // Nothing objected: commit the switch itself, not the round trip, so an
      // untouched query keeps the tree it already had.
      if (notApplicable.length === 0) return commitQuery(next);
      // Only DISABLED objections: the grey Off rows were already off, and a
      // prompt about conditions that are not running would be noise.
      return commitQuery(carried);
    }
    const rootFilter = builderRoot(carried.filter);
    const sites = rawLeaves(rootFilter).filter(
      (site) => site.leaf.diagnostic_kind === "not_applicable" && !site.disabled,
    );
    setAnchorPrompt({
      anchor,
      count: live.length,
      total: countConditions(rootFilter),
      names: sites.map((site) => filterLabel(site.leaf)),
      onRemove: () => {
        let filter = rootFilter;
        for (const loc of sites.map((site) => site.loc).sort(compareLocsDescending)) {
          filter = loc.length === 0 ? { kind: "and", items: [] } : removeAt(filter, loc);
        }
        setAnchorPrompt(null);
        commitQuery({ ...carried, filter });
      },
      // The leaves stay as red rows with their message; the query is invalid
      // and returns nothing until they are edited, removed or disabled (§3.5).
      onKeep: () => {
        setAnchorPrompt(null);
        commitQuery(carried);
      },
      onCancel: () => setAnchorPrompt(null),
    });
  };

  // -- the sheet's layer and its position ------------------------------------

  // **A ROOT transient layer whose id is unique per MOUNT, not per block.**
  // `transientLayers` keys by id and a later registration REPLACES an earlier
  // one, and the same block can be mounted twice (main pane + split pane or
  // sidebar) — so a block-derived id would silently unregister the first
  // sheet, and Escape would close the wrong one. The block id is a readable
  // prefix, nothing more.
  const sheetLayerId = `query-sheet:${props.blockId ?? "workspace"}:${createUniqueId()}`;
  let sheetEl: HTMLDivElement | undefined;
  let sentenceEl: HTMLSpanElement | undefined;

  const dismissable = () => open() && !props.sheetAlwaysOpen;
  createEffect(() => {
    if (!dismissable()) return;
    const unregister = registerTransientLayer({
      id: sheetLayerId,
      root: () => sheetEl ?? null,
      trigger: () => sentenceEl ?? null,
      dismiss: () => {
        setOpen(false);
        return true;
      },
    });
    onCleanup(unregister);
  });
  dismissOnOutsidePointer({
    open: dismissable,
    // A press while one of the sheet's own popovers is open belongs to that
    // popover's rung: closing the sheet under it would collapse two levels of
    // the ladder on one press. Treating the whole document as "inside" for that
    // press is what holds the sheet still while the popover's own layer takes
    // it; the NEXT press, with nothing open, closes the sheet.
    //
    // `openMenu()` covers the rows, the anchor and the add chooser. The footer's
    // sort and summarize popovers are P5's and keep their own open state, so the
    // question is asked of the DOM instead of duplicating their signals: a panel
    // rendered inside the sheet right now IS an open popover.
    inside: () =>
      openMenu() !== null || sheetEl?.querySelector(".qs-menu, .qb-picker, .qb-menu")
        ? [document.body]
        : [sheetEl ?? null, sentenceEl ?? null],
    dismiss: () => setOpen(false),
  });

  // The sheet is portalled and positioned from the sentence's rect on wide
  // screens. **Why a portal:** `.query-block` carries `transform: translateZ(0)`
  // plus `position: relative; z-index: 1` (the GH #64 / WebKitGTK flicker fix —
  // read the comment in app.css), which makes it the containing block for any
  // `fixed` descendant, so an in-block sheet could never be viewport-fixed and
  // the narrow bottom sheet would be trapped inside the block.
  const [rect, setRect] = createSignal<{ top: number; left: number; width: number } | null>(null);
  const measure = () => {
    const element = sentenceEl;
    if (!element) return;
    const box = element.getBoundingClientRect();
    setRect({ top: box.bottom, left: box.left, width: Math.max(box.width, 320) });
  };
  createEffect(() => {
    if (!open() || props.sheetAlwaysOpen) return;
    measure();
    if (typeof window === "undefined") return;
    window.addEventListener("scroll", measure, true);
    window.addEventListener("resize", measure);
    onCleanup(() => {
      window.removeEventListener("scroll", measure, true);
      window.removeEventListener("resize", measure);
    });
  });

  const footer = () => (
    <>
      <SortControl view={view} apply={applyView} parentTransientId={sheetLayerId} />
      <SummarizeControl
        view={view}
        apply={applyView}
        facets={facets}
        parentTransientId={sheetLayerId}
      />
      <QueryTextPane
        session={props.session}
        dialect={props.paneDialect ?? "tql"}
        onParsed={(parsed) => setPaneQuery(carryForward(parsed))}
        onCommit={(parsed) => {
          const current = props.session();
          if (!current) return;
          props.onChange({ query: carryForward(parsed), view: current.view });
        }}
        onStale={(value) => {
          setStale(value);
          props.onStale?.(value);
        }}
        alwaysOpen={props.paneAlwaysOpen}
      />
    </>
  );

  const sheet = (extraRef?: (element: HTMLDivElement) => void) => (
    <QuerySheet
      anchor={() => session()?.query.anchor ?? "block"}
      onAnchor={(anchor) => void switchAnchor(anchor)}
      anchorPrompt={anchorPrompt}
      root={root}
      query={() => session()?.query}
      apply={apply}
      facets={facets}
      registry={registry}
      suggestions={suggestions}
      openMenu={openMenu}
      setOpenMenu={setOpenMenu}
      // In the workspace the sheet is not a layer of its own: its menus parent
      // to the Advanced modal exactly as the chip popovers did, so that
      // dialog's local Tab trap keeps containing them.
      layerId={props.sheetAlwaysOpen ? props.parentTransientId : sheetLayerId}
      autoOpenChooser={autoOpen}
      footer={footer()}
      stale={stale()}
      sheetRef={(element) => {
        sheetEl = element;
        (window as any).__sheetEl = element;
        (window as any).__insideProbe = () => [sheetEl, sentenceEl];
        extraRef?.(element);
      }}
    />
  );

  return (
    <Show when={session()}>
      {(current) => (
        <div class="qs-builder">
          <Show when={!props.sheetAlwaysOpen}>
            <QuerySentence
              query={current().query}
              total={props.total}
              open={open()}
              onOpen={() => setOpen(!open())}
              sentenceRef={(element) => {
                sentenceEl = element;
              }}
            />
          </Show>
          <Show when={previewError()}>
            {(message) => (
              <div class="qs-preview-error" role="alert">
                The anchor wasn't changed: {message()}
              </div>
            )}
          </Show>
          <Show when={props.sheetAlwaysOpen}>{sheet()}</Show>
          <Show when={open() && !props.sheetAlwaysOpen}>
            <Portal>
              <div
                class="qs-overlay"
                onClick={(e) => {
                  stop(e);
                  setOpen(false);
                }}
              />
              <div
                class="qs-sheet-anchor"
                style={
                  rect()
                    ? {
                        top: `${rect()!.top}px`,
                        left: `${rect()!.left}px`,
                        width: `${rect()!.width}px`,
                      }
                    : undefined
                }
              >
                {sheet()}
              </div>
            </Portal>
          </Show>
        </div>
      )}
    </Show>
  );
}
