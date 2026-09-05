import {
  For,
  Show,
  createEffect,
  createMemo,
  createResource,
  createSignal,
  createUniqueId,
  onCleanup,
  type Accessor,
  type JSX,
} from "solid-js";
import { backend } from "../backend";
import {
  addChild,
  betweenFilter,
  builderLeafKind,
  builderRoot,
  contentFilter,
  currentAgg,
  currentGroup,
  currentSort,
  filterChildren,
  filterLabel,
  journalFilter,
  namespaceFilter,
  onPageFilter,
  pagePropertyFilter,
  pageRefFilter,
  pageTagsFilter,
  planningFilter,
  priorityFilter,
  propertyFilter,
  removeAt,
  replaceAt,
  searchFilter,
  setOp,
  sortLabel,
  taskFilter,
  unwrapAt,
  withAgg,
  withGroup,
  withSort,
  wrapAt,
  BETWEEN_FIELDS,
  MARKERS,
  MAX_QUERY_BUILDER_DEPTH,
  PRIORITIES,
  SORT_PRESETS,
  type AggState,
  type BetweenField,
  type BuilderLeafKind,
  type SortPreset,
} from "../editor/queryBuilder";
import type { AggFn, Filter, Query, QueryPrintDialect, ViewSettings } from "../editor/queryIr";
import { DATE_PRESETS, previewDate } from "../editor/dateExpr";
import { sharedQueryResult } from "../queryResultCache";
import { dataRev, graphEpoch, graphMeta, queryBuilderAutoOpen, setQueryBuilderAutoOpen } from "../ui";
import { dismissOnOutsidePointer, registerTransientLayer, type TransientLayer } from "../transientLayers";

// Interactive query builder: an OG-style chip bar over the query **IR**.
//
// It used to be a chip bar over a DSL STRING — parse the text, edit the private
// `Clause` tree, print the text back. Both ends of that round trip were a second
// implementation of a language Rust already owns, and they disagreed with it
// (I-12). Now the bar edits `Filter` and `ViewSettings` directly and the only
// text in this component is what `query_print` produced and what the user typed
// into the pane, which `query_parse` reads back.
//
// `stop` keeps clicks inside the bar from bubbling to the block's onClick, which
// would drop the block into raw-text edit mode and replace the builder.
const stop = (e: MouseEvent) => e.stopPropagation();

type QueryFacets = [string, string[]][];
type QueryFacetsAccessor = Accessor<QueryFacets | undefined>;
const locKey = (l: number[]) => l.join(".");

/** The pair an edit session holds (§4.3.1). `query` carries the anchor, the
 *  filter, the diagnostics and the authored source — including the opaque
 *  options map, which only Rust ever splits or appends. */
export interface BuilderSession {
  query: Query;
  view: ViewSettings;
}

const errorMessage = (error: unknown): string =>
  error instanceof Error ? error.message : String(error);

// Every popover in the bar — clause menu, add-filter picker, sort, summarize —
// registers here, so all four answer Escape/Back AND "the user pressed somewhere
// else" the same way. GH #472 is what happens when they do not: two of the four
// had hand-rolled the outside-press effect and two had not, so a clause menu
// stayed open while the user clicked into and edited a different block.
// The trigger is passed as inside-the-popover so its own click can toggle.
function registerVisiblePopover(open: () => boolean, layer: TransientLayer) {
  createEffect(() => {
    if (!open()) return;
    const unregister = registerTransientLayer(layer);
    onCleanup(unregister);
  });
  dismissOnOutsidePointer({
    open,
    inside: () => [layer.root?.(), layer.trigger?.()],
    dismiss: () => layer.dismiss("explicit"),
  });
}

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
// The bar
// ---------------------------------------------------------------------------

export function QueryBuilder(props: {
  /** The persisted reading of the query. `undefined` while the engine has not
   *  answered yet — the bar renders nothing rather than an empty query it would
   *  then be able to save over the author's text. */
  session: () => BuilderSession | undefined;
  /** Persist an edit. Chip edits call this immediately (each one is a complete,
   *  valid IR); the pane calls it only when the user saves a parse. */
  onChange: (next: BuilderSession) => void;
  /** The text pane's language: `tql` for a query block, `og` for the workspace,
   *  which materializes OG text. */
  paneDialect?: Extract<QueryPrintDialect, "og" | "tql">;
  paneAlwaysOpen?: boolean;
  /** The pane's text no longer parses, so the rows on screen are the LAST
   *  reading that ran. The host greys them; the bar cannot, because the rows are
   *  not its children. */
  onStale?: (stale: boolean) => void;
  blockId?: string;
  parentTransientId?: string;
}): JSX.Element {
  // The pane's last-good parse, not yet saved. `null` = the bar shows the
  // persisted reading. This is what makes "the rows follow the text you typed"
  // and "nothing reaches disk until you save it" both true (§4.3.1).
  const [paneQuery, setPaneQuery] = createSignal<Query | null>(null);
  const [stale, setStale] = createSignal(false);
  createEffect(() => {
    props.session();
    setPaneQuery(null);
  });

  const session = createMemo<BuilderSession | undefined>(() => {
    const persisted = props.session();
    if (!persisted) return undefined;
    const pane = paneQuery();
    return pane ? { query: pane, view: persisted.view } : persisted;
  });
  // The bar always edits an `and`/`or` root, so "+ add filter" has somewhere to
  // add. A single-child `and` prints back as the bare child (`og_form`).
  const root = createMemo(() => builderRoot(session()?.query.filter ?? { kind: "and", items: [] }));
  const view = () => session()?.view ?? {};

  // N builders on one page asked the SAME whole-graph facets question N times
  // per (graphEpoch, dataRev). The scope is per-builder by decision (P0), so the
  // fix is not a shared scope but a shared REQUEST: `sharedQueryResult` collapses
  // identical in-flight/resolved work under its own key namespace, exactly as the
  // page-tag query does. `queryFacets(true)` (autocomplete) asks a different
  // question and deliberately keeps its own path. Harvest W4-P1 item 3.
  const [facets] = createResource(
    () => `${graphEpoch()}\0${dataRev()}`,
    (requestKey) =>
      sharedQueryResult(
        `${graphMeta()?.root ?? ""}\0${graphEpoch()}`,
        `query-facets\0${requestKey}`,
        () => backend().queryFacets(),
      ),
  );
  // Which popover is open, by op/clause loc + purpose. Only one at a time.
  const [openMenu, setOpenMenu] = createSignal<string | null>(null);
  // Open the root add-picker immediately when this block was just created via
  // "/Query (visual builder)" — consume the one-shot flag so only this block does.
  const autoOpen = !!props.blockId && queryBuilderAutoOpen() === props.blockId;
  if (autoOpen) setQueryBuilderAutoOpen(null);
  const [adding, setAdding] = createSignal<string | null>(autoOpen ? "add:" : null);

  /** A chip edit: a new filter over the CURRENT reading, saved immediately. */
  const apply = (next: Filter) => {
    const current = session();
    if (!current) return;
    props.onChange({ query: { ...current.query, filter: next }, view: current.view });
    setOpenMenu(null);
    setAdding(null);
  };
  const applyView = (next: ViewSettings) => {
    const current = session();
    if (!current) return;
    props.onChange({ query: current.query, view: next });
  };
  /** §4.3.1 carry-forward: a pane parse replaces only the filter, the anchor and
   *  the diagnostics. The session's view and its OPAQUE options survive — an
   *  absent map in pane text never means "delete the title". */
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

  return (
    <Show when={session()}>
      <div class="qb-bar" classList={{ "qb-bar-stale": stale() }} onClick={stop}>
        <Node clause={root()} loc={[]} isRoot tree={root} apply={apply} facets={facets}
          openMenu={openMenu} setOpenMenu={setOpenMenu} adding={adding} setAdding={setAdding}
          parentTransientId={props.parentTransientId} />
        <SortControl view={view} apply={applyView} parentTransientId={props.parentTransientId} />
        <SummarizeControl view={view} apply={applyView} facets={facets} parentTransientId={props.parentTransientId} />
        <QueryTextPane
          session={props.session}
          dialect={props.paneDialect ?? "tql"}
          onParsed={(parsed) => setPaneQuery(carryForward(parsed))}
          onCommit={(parsed) => {
            const current = props.session();
            if (!current) return;
            props.onChange({ query: carryForward(parsed), view: current.view });
          }}
          onStale={(value) => { setStale(value); props.onStale?.(value); }}
          alwaysOpen={props.paneAlwaysOpen}
        />
      </div>
    </Show>
  );
}

interface NodeCtx {
  loc: number[];
  isRoot?: boolean;
  clause: Filter;
  tree: () => Filter;
  apply: (next: Filter) => void;
  facets: QueryFacetsAccessor;
  openMenu: () => string | null;
  setOpenMenu: (k: string | null) => void;
  adding: () => string | null;
  setAdding: (k: string | null) => void;
  parentTransientId?: string;
}

function Node(props: NodeCtx): JSX.Element {
  if (props.loc.length >= MAX_QUERY_BUILDER_DEPTH) {
    return <span class="qb-depth-limit">Query nesting truncated at {MAX_QUERY_BUILDER_DEPTH} levels</span>;
  }
  // `off` renders as a greyed wrapper around its subtree: the row is present and
  // round-trips, it just does not run (§3.5, Q12). The UI that toggles it is P6's.
  const kind = () => props.clause.kind;
  const children = () => filterChildren(props.clause) ?? [];

  return (
    <Show when={kind() === "and" || kind() === "or" || kind() === "not" || kind() === "off"} fallback={<Chip {...props} />}>
      <Show when={kind() === "and" || kind() === "or"} fallback={
        <span class="qb-op-not" classList={{ "qb-off": kind() === "off" }}>
          <span class="qb-bracket">{kind() === "off" ? "OFF(" : "NOT("}</span>
          <For each={children()}>
            {(child, i) => (
              <Node {...props} clause={child} loc={[...props.loc, i()]} isRoot={false} />
            )}
          </For>
          <ChipMenu {...props} />
          <span class="qb-bracket">)</span>
        </span>
      }>
        <OpGroup {...props} />
      </Show>
    </Show>
  );
}

// An and/or node: optional bracket + a clickable operator pill that flips
// and<->or, its children, and a trailing "+".
function OpGroup(props: NodeCtx): JSX.Element {
  const children = () => filterChildren(props.clause) ?? [];
  const op = () => (props.clause.kind === "or" ? "or" : "and");
  const showBracket = () => !props.isRoot;
  const showOpPill = () => children().length > 1 || !props.isRoot;
  const flip = () => props.apply(setOp(props.tree(), props.loc, op() === "and" ? "or" : "and"));

  return (
    <span class="qb-group" classList={{ "qb-root": props.isRoot }}>
      <Show when={showBracket()}>
        <span class="qb-bracket">(</span>
      </Show>
      <Show when={showOpPill()}>
        <button
          class="qb-op"
          title="Toggle AND / OR"
          onClick={(e) => {
            stop(e);
            flip();
          }}
        >
          {op().toUpperCase()}
        </button>
      </Show>
      <For each={children()}>
        {(child, i) => (
          <Node {...props} clause={child} loc={[...props.loc, i()]} isRoot={false} />
        )}
      </For>
      <AddButton {...props} prominent={props.isRoot && children().length === 0} />
      <Show when={!props.isRoot}>
        <ChipMenu {...props} />
        <span class="qb-bracket">)</span>
      </Show>
    </span>
  );
}

// A leaf chip. Click opens an action menu (delete / wrap).
function Chip(props: NodeCtx): JSX.Element {
  const key = () => `chip:${locKey(props.loc)}`;
  let triggerEl: HTMLButtonElement | undefined;
  return (
    <span class="qb-chip-wrap">
      <button
        ref={triggerEl}
        class="qb-chip"
        classList={{ "qb-chip-raw": props.clause.kind === "raw" }}
        title="Click: delete / wrap in AND·OR·NOT (to exclude or nest)"
        onClick={(e) => {
          stop(e);
          props.setOpenMenu(props.openMenu() === key() ? null : key());
        }}
      >
        {filterLabel(props.clause)}
      </button>
      <ChipMenu {...props} trigger={() => triggerEl ?? null} />
    </span>
  );
}

// Per-clause action popover (delete, wrap in AND/OR/NOT, and for boolean nodes:
// unwrap). Shown for both leaf chips and operator nodes.
function ChipMenu(props: NodeCtx & { trigger?: () => HTMLElement | null }): JSX.Element {
  const isOpKey = () => filterChildren(props.clause) !== null;
  const key = () => `${isOpKey() ? "op" : "chip"}:${locKey(props.loc)}`;
  const open = () => props.openMenu() === key();
  const act = (f: () => Filter) => () => props.apply(f());
  // The root has no enclosing position to delete/wrap from.
  const atRoot = () => props.loc.length === 0;

  const [editing, setEditing] = createSignal(false);
  let menuEl: HTMLDivElement | undefined;
  const layerId = `query-clause-menu-${createUniqueId()}`;
  registerVisiblePopover(open, {
    id: layerId,
    parentId: props.parentTransientId,
    root: () => menuEl ?? null,
    trigger: props.trigger,
    dismiss: () => { props.setOpenMenu(null); return true; },
  });
  // "Edit…" is offered exactly for the shapes a picker can re-collect. A leaf the
  // pickers do not model still renders and still deletes — it just has no value
  // editor, which is honest rather than a form that would rewrite it into
  // something else.
  const editKind = () => (isOpKey() ? null : builderLeafKind(props.clause));
  const canEdit = () => {
    const kind = editKind();
    return kind != null && kind !== "scheduled" && kind !== "deadline" && kind !== "journal";
  };

  return (
    <Show when={open()}>
      <div ref={menuEl} class="qb-menu" onClick={stop}>
        <Show when={editing()} fallback={
          <>
            <Show when={canEdit()}>
              <button class="qb-menu-item" onClick={() => setEditing(true)}>Edit…</button>
            </Show>
            <Show when={!atRoot()}>
              <button class="qb-menu-item" onClick={act(() => removeAt(props.tree(), props.loc))}>Delete</button>
              <button class="qb-menu-item" onClick={act(() => wrapAt(props.tree(), props.loc, "and"))}>Wrap in AND</button>
              <button class="qb-menu-item" onClick={act(() => wrapAt(props.tree(), props.loc, "or"))}>Wrap in OR</button>
              <button class="qb-menu-item" onClick={act(() => wrapAt(props.tree(), props.loc, "not"))}>Wrap in NOT</button>
            </Show>
            <Show when={isOpKey() && !atRoot()}>
              <button class="qb-menu-item" onClick={act(() => unwrapAt(props.tree(), props.loc))}>Unwrap</button>
            </Show>
          </>
        }>
          <div class="qb-picker-title">Edit value</div>
          <ValuePicker facets={props.facets} kind={editKind()!} onCommit={(c) => props.apply(replaceAt(props.tree(), props.loc, c))} />
        </Show>
      </div>
    </Show>
  );
}

// "+" button that opens the add-filter picker, scoped to the node at `loc`. When
// `prominent` (an empty query), render an inviting "➕ Add filter" call-to-action
// instead of a bare "+", so leaving the bullet reveals an obvious next step.
function AddButton(props: NodeCtx & { prominent?: boolean }): JSX.Element {
  const key = () => `add:${locKey(props.loc)}`;
  const open = () => props.adding() === key();
  let triggerEl: HTMLButtonElement | undefined;
  let pickerEl: HTMLDivElement | undefined;
  const layerId = `query-add-picker-${createUniqueId()}`;
  registerVisiblePopover(open, {
    id: layerId,
    parentId: props.parentTransientId,
    root: () => pickerEl ?? null,
    trigger: () => triggerEl ?? null,
    dismiss: () => { props.setAdding(null); return true; },
  });
  return (
    <span class="qb-add-wrap">
      <button
        ref={triggerEl}
        class="qb-add"
        classList={{ "qb-add-prominent": props.prominent }}
        title="Add filter"
        onClick={(e) => {
          stop(e);
          props.setAdding(open() ? null : key());
        }}
      >
        {props.prominent ? "➕ Add filter" : "+"}
      </button>
      <Show when={open()}>
        <AddPicker
          facets={props.facets}
          rootRef={(element) => { pickerEl = element; }}
          onCommit={(c) => props.apply(addChild(props.tree(), props.loc, c))}
          onSetOp={(op) => props.apply(setOp(props.tree(), props.loc, op))}
        />
      </Show>
    </span>
  );
}

// ---------------------------------------------------------------------------
// Add-filter picker: choose a filter type, then collect its value(s).
// ---------------------------------------------------------------------------

const FILTER_TYPES: { kind: BuilderLeafKind; label: string }[] = [
  { kind: "page", label: "Page / tag reference" },
  { kind: "task", label: "Task marker" },
  { kind: "priority", label: "Priority" },
  { kind: "property", label: "Property" },
  { kind: "scheduled", label: "Scheduled" },
  { kind: "deadline", label: "Deadline" },
  { kind: "journal", label: "On journal page" },
  { kind: "between", label: "Between dates" },
  { kind: "content", label: "Full-text search" },
  { kind: "onPage", label: "On page" },
  { kind: "namespace", label: "In namespace" },
  { kind: "pageProperty", label: "Page property" },
  { kind: "pageTags", label: "Page tags" },
];

function AddPicker(props: {
  facets: QueryFacetsAccessor;
  onCommit: (c: Filter) => void;
  onSetOp: (op: "and" | "or") => void;
  rootRef?: (element: HTMLDivElement) => void;
}): JSX.Element {
  const [step, setStep] = createSignal<BuilderLeafKind | "type">("type");
  // When armed, the next filter is added negated (wrapped in NOT).
  const [negate, setNegate] = createSignal(false);

  const pick = (kind: BuilderLeafKind) => {
    if (kind === "scheduled" || kind === "deadline") return commit(planningFilter(kind));
    if (kind === "journal") return commit(journalFilter());
    setStep(kind);
  };
  const commit = (c: Filter) => props.onCommit(negate() ? { kind: "not", inner: c } : c);

  return (
    <div ref={props.rootRef} class="qb-picker" onClick={stop}>
      <Show when={step() === "type"}>
        {/* Connectives first (OG-style): AND/OR set how this group joins;
            NOT arms negation for the filter you pick next. */}
        <div class="qb-conn-row">
          <button class="qb-conn" title="Join this group with AND" onClick={() => props.onSetOp("and")}>AND</button>
          <button class="qb-conn" title="Join this group with OR" onClick={() => props.onSetOp("or")}>OR</button>
          <button class="qb-conn" classList={{ active: negate() }} title="Exclude the next filter (NOT)" onClick={() => setNegate(!negate())}>NOT</button>
        </div>
        <div class="qb-divider" />
        <div class="qb-picker-title">{negate() ? "Exclude filter…" : "Add filter"}</div>
        <For each={FILTER_TYPES}>
          {(t) => (
            <button class="qb-menu-item" onClick={() => pick(t.kind)}>
              {t.label}
            </button>
          )}
        </For>
      </Show>
      <Show when={step() !== "type"}>
        <ValuePicker facets={props.facets} kind={step() as BuilderLeafKind} onCommit={commit} />
      </Show>
    </div>
  );
}

// Renders the value collector for a given filter kind, and commits the IR leaf
// `og.rs` builds for the same intent. Shared by the add-filter picker and the
// in-place "Edit value" flow.
function ValuePicker(props: { facets: QueryFacetsAccessor; kind: BuilderLeafKind; onCommit: (c: Filter) => void }): JSX.Element {
  return (
    <>
      <Show when={props.kind === "page"}>
        <PageInput placeholder="Page or tag name" onCommit={(name) => props.onCommit(pageRefFilter(name))} />
      </Show>
      <Show when={props.kind === "task"}>
        <MultiPick options={MARKERS} onCommit={(markers) => props.onCommit(taskFilter(markers))} />
      </Show>
      <Show when={props.kind === "priority"}>
        <MultiPick options={PRIORITIES} onCommit={(levels) => props.onCommit(priorityFilter(levels))} />
      </Show>
      <Show when={props.kind === "property"}>
        <PropertyPick facets={props.facets} onCommit={(key, value) => props.onCommit(propertyFilter(key, value))} />
      </Show>
      <Show when={props.kind === "between"}>
        <BetweenPick onCommit={(field, start, end) => props.onCommit(betweenFilter(field, start, end))} />
      </Show>
      <Show when={props.kind === "onPage"}>
        <PageInput placeholder="Page name" onCommit={(name) => props.onCommit(onPageFilter(name))} />
      </Show>
      <Show when={props.kind === "namespace"}>
        <PageInput placeholder="Namespace (parent page)" onCommit={(ns) => props.onCommit(namespaceFilter(ns))} />
      </Show>
      <Show when={props.kind === "pageProperty"}>
        <PropertyPick facets={props.facets} onCommit={(key, value) => props.onCommit(pagePropertyFilter(key, value))} />
      </Show>
      <Show when={props.kind === "content"}>
        <TextInput placeholder="Text to search for" onCommit={(text) => props.onCommit(contentFilter(text))} />
      </Show>
      <Show when={props.kind === "search"}>
        <TextInput placeholder="Search words and operators" onCommit={(source) => props.onCommit(searchFilter(source))} />
      </Show>
      <Show when={props.kind === "pageTags"}>
        <TextInput placeholder="Tag (one)" onCommit={(t) => props.onCommit(pageTagsFilter([t]))} />
      </Show>
    </>
  );
}

// Plain free-text input that commits on Enter.
function TextInput(props: { placeholder: string; onCommit: (text: string) => void }): JSX.Element {
  const [v, setV] = createSignal("");
  return (
    <div class="qb-value">
      <input
        class="qb-input"
        autofocus
        placeholder={props.placeholder}
        value={v()}
        onInput={(e) => setV(e.currentTarget.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && v().trim()) props.onCommit(v().trim());
        }}
      />
    </div>
  );
}

// Page-name input with fuzzy autocomplete from the graph.
function PageInput(props: { placeholder: string; onCommit: (name: string) => void }): JSX.Element {
  const [q, setQ] = createSignal("");
  // Debounce the backend fuzzy-match (quick_switch lists pages from disk) so
  // holding a key doesn't fire an IPC + dir scan per character.
  const [dq, setDq] = createSignal("");
  let dqTimer: ReturnType<typeof setTimeout> | undefined;
  createEffect(() => {
    const s = q();
    clearTimeout(dqTimer);
    dqTimer = setTimeout(() => setDq(s), 120);
  });
  onCleanup(() => clearTimeout(dqTimer));
  const [matches] = createResource(dq, (s) => backend().quickSwitch(s, 8));
  return (
    <div class="qb-value">
      <input
        class="qb-input"
        autofocus
        placeholder={props.placeholder}
        value={q()}
        onInput={(e) => setQ(e.currentTarget.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && q().trim()) props.onCommit(q().trim());
        }}
      />
      <For each={matches() ?? []}>
        {(p) => (
          <button class="qb-menu-item" onClick={() => props.onCommit(p.name)}>
            {p.name}
          </button>
        )}
      </For>
    </div>
  );
}

// Multi-select (task markers, priorities) with checkboxes + an Add button.
function MultiPick(props: { options: string[]; onCommit: (picked: string[]) => void }): JSX.Element {
  const [picked, setPicked] = createSignal<string[]>([]);
  const toggle = (o: string) =>
    setPicked(picked().includes(o) ? picked().filter((x) => x !== o) : [...picked(), o]);
  return (
    <div class="qb-value">
      <For each={props.options}>
        {(o) => (
          <label class="qb-check">
            <input type="checkbox" checked={picked().includes(o)} onChange={() => toggle(o)} /> {o}
          </label>
        )}
      </For>
      <button class="qb-commit" disabled={picked().length === 0} onClick={() => props.onCommit(picked())}>
        Add
      </button>
    </div>
  );
}

// Property: choose a key (autocompleted from used properties), then a value
// (from that key's known values, "any", or free text).
function PropertyPick(props: { facets: QueryFacetsAccessor; onCommit: (key: string, value: string | null) => void }): JSX.Element {
  const [key, setKey] = createSignal("");
  const [chosen, setChosen] = createSignal<string | null>(null);
  const keys = () => (props.facets() ?? []).map(([k]) => k);
  const valuesFor = (k: string) => (props.facets() ?? []).find(([kk]) => kk === k)?.[1] ?? [];
  const [val, setVal] = createSignal("");

  return (
    <div class="qb-value">
      <Show
        when={chosen() != null}
        fallback={
          <>
            <input
              class="qb-input"
              autofocus
              placeholder="Property key"
              value={key()}
              onInput={(e) => setKey(e.currentTarget.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && key().trim()) setChosen(key().trim());
              }}
            />
            <For each={keys().filter((k) => k.toLowerCase().includes(key().toLowerCase()))}>
              {(k) => (
                <button class="qb-menu-item" onClick={() => { setKey(k); setChosen(k); }}>
                  {k}
                </button>
              )}
            </For>
          </>
        }
      >
        <div class="qb-picker-title">{chosen()}</div>
        <button class="qb-menu-item" onClick={() => props.onCommit(chosen()!, null)}>
          (any value)
        </button>
        <For each={valuesFor(chosen()!)}>
          {(v) => (
            <button class="qb-menu-item" onClick={() => props.onCommit(chosen()!, v)}>
              {v}
            </button>
          )}
        </For>
        <input
          class="qb-input"
          placeholder="Custom value"
          value={val()}
          onInput={(e) => setVal(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") props.onCommit(chosen()!, val().trim() || null);
          }}
        />
      </Show>
    </div>
  );
}

// Date-range picker. A field selector (which date to test), one-click relative
// presets, and two bound inputs that accept keywords (`today`), relative offsets
// (`-30d`), ISO dates, or a journal-page title — each with a live resolved-date
// preview so the free-text accepts more than its placeholder hints.
const FIELD_LABEL: Record<BetweenField, string> = {
  journal: "Journal date",
  scheduled: "Scheduled",
  deadline: "Deadline",
  any: "Any date",
};
function BetweenPick(props: { onCommit: (field: BetweenField, start: string, end: string) => void }): JSX.Element {
  const [field, setField] = createSignal<BetweenField>("journal");
  const [start, setStart] = createSignal("");
  const [end, setEnd] = createSignal("");
  const ready = () => !!start().trim() && !!end().trim();
  const submit = () => {
    if (ready()) props.onCommit(field(), start().trim(), end().trim());
  };
  return (
    <div class="qb-between">
      <div class="qb-between-field">
        <For each={BETWEEN_FIELDS}>
          {(f) => (
            <button class="qb-conn" classList={{ active: field() === f }} onClick={() => setField(f)}>
              {FIELD_LABEL[f]}
            </button>
          )}
        </For>
      </div>
      <div class="qb-between-presets">
        <For each={DATE_PRESETS}>
          {(p) => (
            <button
              class="qb-preset"
              title={`${p.start} → ${p.end}`}
              onClick={() => {
                setStart(p.start);
                setEnd(p.end);
              }}
            >
              {p.label}
            </button>
          )}
        </For>
      </div>
      <DateBoundInput placeholder="Start — today, -30d, 2026-06-01, or a page" value={start()} onInput={setStart} onEnter={submit} autofocus />
      <DateBoundInput placeholder="End — today, +7d, 2026-06-30, or a page" value={end()} onInput={setEnd} onEnter={submit} />
      <button class="qb-commit" disabled={!ready()} onClick={submit}>
        Add
      </button>
    </div>
  );
}

// A single date-bound input with a live resolved-date preview underneath.
function DateBoundInput(props: {
  placeholder: string;
  value: string;
  onInput: (v: string) => void;
  onEnter: () => void;
  autofocus?: boolean;
}): JSX.Element {
  const preview = createMemo(() => previewDate(props.value));
  return (
    <div class="qb-bound">
      <input
        class="qb-input"
        autofocus={props.autofocus}
        placeholder={props.placeholder}
        value={props.value}
        onInput={(e) => props.onInput(e.currentTarget.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") props.onEnter();
        }}
      />
      <span class="qb-bound-preview">{preview() ? `→ ${preview()}` : " "}</span>
    </div>
  );
}
