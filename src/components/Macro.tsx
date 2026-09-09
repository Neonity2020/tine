import { For, Show, Switch, Match, createEffect, createMemo, createResource, createSignal, useContext, createUniqueId, on, onCleanup, onMount, untrack, type JSX } from "solid-js";
import { backend, QueryPrintRefusedError } from "../backend";
import { focusedRouter, openRouteInOtherPane } from "../panes";
import { openPageTarget, openPageAtBlock, openPageTargetInNewTab, openInNewTab } from "../router";
import { CROSSING_NOTICE, dismissNotice, noticeDismissed, primeNoticeDismissals, openPageInSidebar, openBlockInSidebar, openPageContextMenu, dataRev, graphEpoch, graphMeta, pageIdentityKey } from "../ui";
import { blockProperty, doc, formatForPage, formatForBlock, pageByName, resolveGuidePageDto, setBlockProperty, setRaw, undo, undoTopTag, withUndoUnit } from "../store";
import { resolveBlockBatched } from "../resolveBatch";
import { internalLinkAuxClick, internalLinkDest, internalLinkMouseDown } from "../linkGesture";
import { shouldOpenTextContextMenu } from "../contextMenuPolicy";
import { LiveRefGroup } from "./LiveRefGroup";
import { CrossingNotice } from "./CrossingNotice";
import { QueryBuilder, type BuilderSession } from "./QueryBuilder";
import { SearchResultRow } from "./SearchResultRow";
import { querySummary, type QuerySummary } from "../editor/queryAggregate";
import { quoteEdnString, unquoteEdnString } from "../editor/edn";
import { queryMacroExtents, QUERY_MACRO_NAMES } from "../editor/queryMacro";
import {
  groupingFromViewValue,
  queryDisplayPropertyWrites,
  queryViewPropertyPatch,
  retainedQueryAggregateSegments,
  serializeQuerySort,
  viewAfterViewSwitch,
  type QueryDisplayControl,
} from "../editor/queryViewProperties";
import {
  macroPrintDialect,
  macroTextDialect,
  sourceOptions,
  sourceOriginal,
  sourcePrintDialect,
  type Diagnostic,
  type EmptyExplanation,
  type PageRow,
  type ParsedQuery,
  type Query,
  type QueryReport,
  type Source,
  type ViewSettings,
} from "../editor/queryIr";
import { visibleBody } from "../render/block";
import { facetsOf } from "../render/facets";
import { sheetConfig } from "../sheet/config";
import {
  boardGroupByOptions,
  fieldIdsForRecords,
  fieldLabel,
  isFieldId,
  type FieldId,
  type QueryGroupingControl,
} from "../sheet/fields";
import {
  type FormulaEvalRow,
} from "../sheet/formulaEval";
import { formulasOf } from "../sheet/formulaFields";
import { InlineText } from "../render/inline";
import { SheetTable } from "./SheetTable";
import { SheetBoard } from "./SheetBoard";
import { SheetContainer } from "./SheetContainer";
import type { PageKind, RefGroup } from "../types";
import { sharedQueryResult, sharedQueryScope } from "../queryResultCache";
import { graphBinding } from "../persistence";
import { createReadyQueryResource } from "../createReadyQueryResource";
import { runQueryWhenCurrent } from "../queryReadiness";
import { savedDslToFriendlySearch } from "../editor/searchQuery";
import type { QueryExecution, QueryHit } from "../types";
import { LinkDepthContext, LinkDepthWarning, MAX_DEPTH_OF_LINKS } from "./linkDepth";
import { blockDtoExternalId } from "../blockIdentity";
import { ExternalLink } from "./ExternalLink";
import { editingId } from "../editorController";
import { createQueryRefreshRevision } from "../queryResultGrace";

// Recognize the typed Logseq input without treating an example in a string or
// `;;` comment as live. Only a direct token in the :inputs vector makes query
// execution depend on focused-pane navigation.
function declaresCurrentPageInput(source: string): boolean {
  const boundary = (ch: string | undefined) =>
    ch === undefined || /[\s,\[\](){}]/.test(ch);
  let i = 0;
  while (i < source.length) {
    if (source[i] === '"') {
      i += 1;
      while (i < source.length) {
        if (source[i] === "\\") i += 2;
        else if (source[i] === '"') {
          i += 1;
          break;
        } else i += 1;
      }
      continue;
    }
    if (source[i] === ";") {
      while (i < source.length && source[i] !== "\n") i += 1;
      continue;
    }
    if (
      source.startsWith(":inputs", i) &&
      boundary(source[i - 1]) &&
      boundary(source[i + ":inputs".length])
    ) {
      let cursor = i + ":inputs".length;
      while (cursor < source.length && /[\s,]/.test(source[cursor])) cursor += 1;
      if (source[cursor] !== "[") return false;
      let depth = 1;
      cursor += 1;
      while (cursor < source.length && depth > 0) {
        if (source[cursor] === '"') {
          cursor += 1;
          while (cursor < source.length) {
            if (source[cursor] === "\\") cursor += 2;
            else if (source[cursor] === '"') {
              cursor += 1;
              break;
            } else cursor += 1;
          }
          continue;
        }
        if (source[cursor] === ";") {
          while (cursor < source.length && source[cursor] !== "\n") cursor += 1;
          continue;
        }
        if ("[({".includes(source[cursor])) depth += 1;
        else if ("])}".includes(source[cursor])) depth -= 1;
        else if (
          depth === 1 &&
          source.slice(cursor, cursor + ":current-page".length).toLowerCase() ===
            ":current-page" &&
          boundary(source[cursor - 1]) &&
          boundary(source[cursor + ":current-page".length])
        ) {
          return true;
        }
        cursor += 1;
      }
      return false;
    }
    i += 1;
  }
  return false;
}

type QueryView = "search" | "list" | "table" | "board";
const QUERY_VIEWS: QueryView[] = ["search", "list", "table", "board"];
const QUERY_VIEW_LABEL: Record<QueryView, string> = {
  search: "Search",
  list: "List",
  table: "Table",
  board: "Board",
};

// Collapsed state for query results, keyed by graph + rendered query identity.
// A raw query-string key made unrelated dashboards across pages/graphs collide.
const QCOLLAPSE_KEY = "logseq-claude.queryCollapsed";
function loadCollapsed(key: string): boolean | null {
  try {
    const m = JSON.parse(localStorage.getItem(QCOLLAPSE_KEY) ?? "{}");
    return typeof m[key] === "boolean" ? m[key] : null;
  } catch {
    return null;
  }
}
function saveCollapsed(key: string, v: boolean) {
  try {
    const m = JSON.parse(localStorage.getItem(QCOLLAPSE_KEY) ?? "{}");
    // Keep explicit false: it overrides a source `:collapsed? true` default on
    // remount. Deleting false made an expanded query re-collapse immediately.
    m[key] = v;
    localStorage.setItem(QCOLLAPSE_KEY, JSON.stringify(m));
  } catch {
    // ignore
  }
}

interface Row {
  page: string;
  kind: PageKind;
  path?: string;
  text: string;
  props: Record<string, string>;
}

/** The message a rejected command carried. A `QueryPrintRefusedError` already
 *  carries the printer's OWN located message (I-9), so there is nothing to add
 *  here and nothing to replace it with. */
function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** The advanced ran/ignored note, read off the run's report (M5) instead of a
 *  second command's return shape. `ran`/`ignored` are optional on the wire; an
 *  OG or TQL source reports neither. */
function reportInfo(report: QueryReport): { ran: string[]; ignored: string[]; supported: boolean } {
  return { ran: report.ran ?? [], ignored: report.ignored ?? [], supported: report.supported };
}

/** A bounded excerpt of ENGINE-PRINTED text, for the §7.5 notice.
 *
 *  It is bounded because the text is the author's own query, and a query can be
 *  as long as outside content chooses to make it; a notice is not a place to
 *  render an unbounded string (I-22). It is an excerpt of what the engine
 *  printed, and is labelled as that — it is not, and must not be described as,
 *  "the minimal unsupported subexpression". Nothing in the engine answers that
 *  question today, so nothing here claims it. */
export function boundedFeature(message: string): string | null {
  const single = message.replace(/\s+/g, " ").trim();
  if (!single) return null;
  return single.length > 120 ? `${single.slice(0, 119)}…` : single;
}

/** Remove the block a query is written in from that query's own results.
 *
 *  `{{query "xyz"}}` contains `xyz`, so the block matches its own query and the
 *  backend says so honestly. Rendering that match renders the page the query
 *  lives on, which renders the query, which lists the page again — the
 *  recursion in GH #469. OG removes exactly the host block for the same reason
 *  and states it where it does it (`frontend/components/query/result.cljs` at
 *  `6e7afa8e`: "exclude the current one, otherwise it'll loop forever"). Only
 *  the block itself goes; its children are ordinary results.
 *
 *  Returns the input unchanged when nothing matches, so a query whose results
 *  never contain its host keeps referential equality for downstream memos. */
export function withoutHostBlock(groups: RefGroup[], hostBlockId: string | undefined): RefGroup[] {
  if (!hostBlockId) return groups;
  const hosts = (group: RefGroup) => group.blocks.some((block) => block.id === hostBlockId);
  if (!groups.some(hosts)) return groups;
  return groups
    .map((group) =>
      hosts(group)
        ? { ...group, blocks: group.blocks.filter((block) => block.id !== hostBlockId) }
        : group,
    )
    .filter((group) => group.blocks.length > 0);
}

// A {{query ...}} block: runs the query and renders matching blocks as a list
// or a sortable table. When `blockId` is given (the block is a standalone query
// block, not an inline-in-text macro) an interactive builder bar is shown and
// edits rewrite the {{query ...}} macro in that block's raw text.
export function QueryMacro(props: {
  body: string;
  /** The macro name this query was AUTHORED under (§7.9). Supplied by the render
   *  dispatch, which recovered it from the raw source alongside the argument.
   *  Absent for callers that build a body string themselves; the name is then
   *  read back off `body`, and failing that defaults to the legacy spelling. */
  macroName?: string;
  blockId?: string;
  title?: string;
  /** Read-only query surfaces can supply page context without a blockId, which
   * would incorrectly enable editing controls. */
  currentPage?: string;
  /** BEGIN_QUERY must never execute a partially understood query or expose its
   * authored payload in an error. The ordinary {{query}} path keeps its existing
   * partial-query diagnostics unless these read-only options are requested. */
  strictAdvanced?: boolean;
  unsupportedLabel?: string;
  // When set, render nothing at all if the query has no results (used for the
  // app-inserted journal agenda, which should disappear once vacated — unlike a
  // user-authored {{query}} block, which keeps showing "No results" so it stays
  // editable).
  hideWhenEmpty?: boolean;
}): JSX.Element {
  const linkDepth = useContext(LinkDepthContext);
  if (linkDepth > MAX_DEPTH_OF_LINKS) return <LinkDepthWarning />;

  // §7.9: strip whichever query macro name this block was authored under, not a
  // hard-coded `query`. A `{{tine-query …}}` body whose name was not stripped
  // would be handed to the parser as `tine-query @block and …`, which is not a
  // query in any grammar.
  const macroName = (): string => {
    if (props.macroName) return props.macroName;
    const authored = QUERY_MACRO_NAMES.find((name) =>
      new RegExp(`^${name}(\\s|$)`, "i").test(props.body.trim()),
    );
    return authored ?? QUERY_MACRO_NAMES[0];
  };
  const arg = () =>
    props.body.trim().replace(new RegExp(`^${macroName()}\\s*`, "i"), "").trim();
  // The host block's `tine.*` properties, which §4.1 gives precedence over the
  // directives lifted from the query text. Merging the two is the engine's job,
  // so they are handed to it rather than reconciled here.
  const blockDirectives = createMemo<[string, string][]>(() => {
    const id = props.blockId;
    const node = id ? doc.byId[id] : undefined;
    if (!id || !node) return [];
    return facetsOf(node.raw, formatForBlock(id)).properties.filter(([key]) =>
      key.startsWith("tine."),
    );
  });
  // **The macro argument is read by the ONE engine (§7.1, X4).** Where the
  // trailing options map begins, and whether a `{{query …}}` holds the OG DSL or
  // advanced datalog, are query-language questions that Rust already answers in
  // `query_parse`. This component used to answer both a second time — with
  // `splitTrailingMap` and an `ADVANCED_RE` regex — and the two answers differed
  // on exactly the inputs that matter (a literal `}` inside a string, a `:find`
  // inside a string). Both twins are gone (I-12, D-14).
  const parseRequest = createMemo(() => ({
    argument: arg(),
    name: macroName(),
    properties: blockDirectives(),
  }));
  // **Readiness, not sorted groups (RET2-Direct).** `query_parse` reads the
  // property registry for its `UnknownIdent` suggestions, and that read is now
  // SQL-only and fallible: it used to swallow every failure into an empty
  // registry, which reported a declared property as unknown. So the parse
  // retries typed `query-not-ready` the way every other query read does, gated
  // on its OWN request still being current.
  //
  // The retry rides INSIDE the existing resource rather than replacing it with
  // `createReadyQueryResource`: that owner also re-keys on the graph epoch and
  // blanks its rows across a rebind, which would re-parse on every repaint and
  // drop the reading the display derivations read through `latest`. Nothing
  // about the result presentation, group ordering or the scroll harness is
  // involved here.
  const [parsedSnapshot] = createResource(parseRequest, async (request) => ({
    request,
    reading: await runQueryWhenCurrent(
      () => backend().parseQuery(request.argument, macroTextDialect(request.name), request.properties),
      () => parseRequest() === request,
    ),
  }));
  // Keep each reading paired with the exact inputs it describes. A displayed
  // reading can intentionally lag a local edit while its replacement loads.
  const parsed = { get latest() { return parsedSnapshot.latest?.reading; } };
  // `latest` rather than `parsed()`: a re-parse after an edit keeps the previous
  // reading visible instead of blanking the query for a frame.
  const source = (): Source | undefined => parsed.latest?.query.source;
  const view = (): ViewSettings => parsed.latest?.view ?? {};
  const [displayCommit, setDisplayCommit] = createSignal<{ raw: string; epoch: number; view: ViewSettings }>();
  const displayView = (): ViewSettings => {
    const committed = displayCommit();
    return committed && committed.epoch === graphEpoch() && committed.raw === doc.byId[props.blockId ?? ""]?.raw
      ? committed.view : view();
  };
  const rememberDisplay = (settings: ViewSettings) => {
    const raw = doc.byId[props.blockId ?? ""]?.raw;
    if (raw !== undefined) setDisplayCommit({ raw, epoch: graphEpoch(), view: settings });
  };
  const form = (): string => {
    const s = source();
    return (s ? sourceOriginal(s) : null) ?? "";
  };
  const opts = (): string => {
    const s = source();
    return s ? sourceOptions(s) : "";
  };
  // `:title` / `:collapsed?` / `:table-view?` are read out of the OPAQUE options
  // map, which the engine carries verbatim and deliberately does not interpret
  // (§4.3, Y2). There is no Rust answer being duplicated here — reading three
  // display keys out of the author's own map is this component's own question.
  const titleOption = (): string | undefined => {
    const m = /:title\s+"((?:[^"\\]|\\.)*)"/.exec(opts());
    return m ? unquoteEdnString(m[1]) : undefined;
  };
  const collapsedOption = () => /:collapsed\?\s+true/.test(opts());
  const tableViewOption = () => /:table-view\?\s+true/.test(opts());
  // GH #301: `<% current page %>` inside a query binds to the FOCUSED pane's
  // route page and re-runs on navigation. Substitution is execution-only —
  // authoring text and every editing/display derivation keep the literal dyvar
  // (the same house rule as template insertion in editor/templateVars.ts).
  const currentPageMarker = createMemo(() => /<%\s*current page\s*%>/i.test(arg()));
  const focusedQueryPage = () => {
    const r = focusedRouter().route();
    return r.kind === "page" ? r.name : undefined;
  };
  // The dyvar is substituted in the TEXT and read by the one engine, exactly as
  // the authored text is: the execution gets its own parse of the substituted
  // argument rather than a frontend rewrite of an already-parsed IR. `null` means
  // "no substitution applies", and then the authoring parse IS the execution's.
  const executionArg = createMemo<string | null>(() => {
    if (!currentPageMarker()) return null;
    const pageName = focusedQueryPage();
    if (!pageName) return null; // no focused page: leave verbatim, like templates
    return arg().replace(/<%\s*current page\s*%>/gi, `[[${pageName}]]`);
  });
  const executionRequest = createMemo(() => {
    const argument = executionArg();
    return argument === null
      ? undefined
      : { argument, name: macroName(), properties: blockDirectives() };
  });
  // The execution-side parse takes the SAME readiness owner the executions
  // below take: `query_parse` reads the property registry, that read is now
  // SQL-only and fallible, and an unretried refusal here would leave a
  // `<% current page %>` macro with no runnable form at all.
  const [executionParsed] = createReadyQueryResource(executionRequest, async (request) => ({
    request,
    reading: await backend().parseQuery(request.argument, macroTextDialect(request.name), request.properties),
  }));
  /** The execution reading, paired with the exact substituted argument it
   *  describes (GH #301). A reading for the PREVIOUS page is not a runnable
   *  form for this one: dispatching it would run a query the presentation has
   *  already moved off, and a readiness retry — which can hold the replacement
   *  for as long as the projection needs — widens exactly that window. The
   *  displayed rows are unaffected: `groupResource` keeps its own `latest`. */
  const executionReading = (): ParsedQuery | undefined => {
    const snapshot = executionParsed.latest;
    return snapshot && snapshot.request.argument === executionArg() ? snapshot.reading : undefined;
  };
  /** The reading the EXECUTION runs. Every authoring and display derivation keeps
   *  the literal dyvar and therefore keeps using `parsed`. */
  const runnable = (): ParsedQuery | undefined =>
    executionArg() === null ? parsed.latest : executionReading();
  const executableForm = createMemo(() => {
    const source = runnable()?.query.source;
    return (source ? sourceOriginal(source) : null) ?? "";
  });
  // The query LANGUAGE decision rides the same substituted form the execution
  // uses (never authoring rewrites): presentation and execution can't disagree
  // about what ran (GH #301).
  const friendlySearch = createMemo(() => savedDslToFriendlySearch(executableForm()));
  const sheet = createMemo(() => {
    if (!props.blockId || !doc.byId[props.blockId]) return null;
    return sheetConfig(facetsOf(doc.byId[props.blockId].raw, formatForBlock(props.blockId)).properties);
  });
  const currentView = (): QueryView => {
    if (!props.blockId) return "list";
    const view = blockProperty(props.blockId, "tine.view");
    return view === "search" || view === "table" || view === "board" ? view : "list";
  };
  const sheetFace = () => currentView() === "table" || currentView() === "board";
  const legacyTable = () => currentView() === "list" && tableViewOption();
  /** The header's view switcher, for the hosts the inline Display panel is not
   *  offered to. It goes through the SAME writer the panel does — the view is
   *  one fact, and two surfaces writing it two ways is how they came apart. */
  const setQueryView = (next: QueryView) => {
    if (!props.blockId) return;
    void applyDisplay(viewAfterViewSwitch(displayView(), next === "list" ? undefined : next));
  };

  // Rewrite just THIS {{query ...}} macro inside the owning block, preserving the
  // front-matter options and surrounding property lines (id::/collapsed::). The
  // extents are found brace/string/page-ref-aware (queryMacroExtents), NOT a lazy
  // regex. A block can hold more than one query, so target the extent whose
  // current body matches OURS (props.body) — editing the 2nd query must not
  // rewrite the 1st. Falls back to the only/first query for the common case.
  const rewriteMacro = (newMacro: string) => {
    if (!props.blockId) return;
    const raw = doc.byId[props.blockId]?.raw ?? "";
    const extents = queryMacroExtents(raw);
    if (!extents.length) return;
    // Target by the extent's own recovered name+argument rather than by a
    // whitespace-normalized slice of the source: that is the same pair the
    // renderer handed this component as `body`, so the match is exact even when
    // two macros differ only inside a string literal.
    const norm = (s: string) => s.replace(/\s+/g, " ").trim();
    const mine = norm(props.body);
    const target =
      extents.find((e) => norm(`${e.name} ${e.argument}`) === mine)
      ?? extents.find((e) => norm(raw.slice(e.start + 2, e.end - 2)) === mine)
      ?? extents[0];
    setRaw(props.blockId, raw.slice(0, target.start) + newMacro + raw.slice(target.end));
  };
  // **The save path (§4.3 Q3, A5; B5).**
  //
  // The frontend no longer prints a query — it hands the engine an IR and a
  // dialect, and writes back the bytes the engine returned (I-12). Three things
  // are decided here, and only here:
  //
  //  1. **The macro name.** `query_og_expressible` first, so P0 never asks the
  //     OG printer a question it is going to refuse: an expressible edit keeps
  //     the block's CURRENT name (a `{{tine-query}}` block is never silently
  //     converted back to `{{query}}`), a non-expressible edit of a `{{query}}`
  //     block is written as `{{tine-query <tql_macro>}}`.
  //  2. **`NotApplicable` is answered, not surfaced.** This is the ONE caller
  //     entitled to see it (`QueryPrintRefusedError.isNotApplicable`): if the OG
  //     printer refuses after all, the answer is to switch dialect, not to show
  //     the user an error about a query Tine can perfectly well store.
  //  3. **Any OTHER refusal is shown and NOT written** (I-4, I-9, W4): a
  //     macro-safety refusal carries the printer's own located message, and the
  //     block's bytes are left exactly as they were.
  //
  // On a crossing (`{{query}}` → `{{tine-query}}`) the view directives the OG
  // text carried have no home in TQL, so they are written to the block's `tine.*`
  // properties in the SAME undo unit (§4.3 Y2) — otherwise the crossed block
  // would re-parse without its sort, sample, grouping or aggregate.
  const applyEdit = async (next: BuilderSession): Promise<boolean> => {
    if (!props.blockId || !doc.byId[props.blockId]) return false;
    const rawAtStart = doc.byId[props.blockId].raw;
    const epochAtStart = graphEpoch();
    const request = parseRequest();
    const baseline = parsedSnapshot.latest;
    if (baseline && (baseline.request.argument !== request.argument || baseline.request.name !== request.name)) {
      setPrintError("The query text changed. Wait for it to refresh, then try this edit again.");
      return false;
    }
    if (baseline && JSON.stringify(baseline.request.properties) !== JSON.stringify(request.properties)) {
      // Rebase only because the reading predates a property edit. The ordinary
      // save still materializes its full effective view, including text-only
      // directives that a reprint would otherwise discard.
      try {
        const fresh = await runQueryWhenCurrent(
          () => backend().parseQuery(request.argument, macroTextDialect(request.name), request.properties),
          () => graphEpoch() === epochAtStart && doc.byId[props.blockId!]?.raw === rawAtStart,
        );
        const rebased = { ...fresh.view };
        for (const key of ["view", "sort", "group_by", "columns", "aggregates", "sample"] as const) {
          const empty = key === "sort" || key === "columns" || key === "aggregates" ? [] : null;
          if (JSON.stringify(baseline.reading.view[key] ?? empty) !== JSON.stringify(next.view[key] ?? empty)) {
            Object.assign(rebased, { [key]: next.view[key] });
          }
        }
        next = { ...next, view: rebased };
      } catch (error) {
        setPrintError(errorText(error));
        return false;
      }
    }
    const current = macroName();
    let expressible = false;
    try {
      expressible = await backend().queryOgExpressible(next.query, next.view);
    } catch (error) {
      setPrintError(errorText(error));
      return false;
    }
    const crossing = !expressible && current.toLowerCase() !== "tine-query";
    let name = expressible ? current : QUERY_MACRO_NAMES[1];
    let dialect = macroPrintDialect(name);
    let argument: string;
    try {
      argument = await backend().printQuery(next.query, next.view, dialect);
    } catch (error) {
      if (error instanceof QueryPrintRefusedError && error.isNotApplicable && dialect === "og") {
        // `og_expressible` said yes and the printer said no. The entitled answer
        // is the other dialect, not a refusal shown to the user.
        name = QUERY_MACRO_NAMES[1];
        dialect = macroPrintDialect(name);
        try {
          argument = await backend().printQuery(next.query, next.view, dialect);
        } catch (second) {
          setPrintError(errorText(second));
          return false;
        }
      } else {
        setPrintError(errorText(error));
        return false;
      }
    }
    if (graphEpoch() !== epochAtStart || doc.byId[props.blockId]?.raw !== rawAtStart) {
      setPrintError("The block changed while saving. Try this edit again.");
      return false;
    }
    setPrintError(null);
    // **What the notice can show comes from the ENGINE, or from nothing.**
    //
    // The crossing itself is `query_og_expressible`'s answer, which is a bare
    // bool: it says THAT the query left OG, never which part. So the notice
    // shows the one piece of evidence that exists — a bounded excerpt of the
    // text the engine just PRINTED and this save is about to write — under its
    // own wording, and the §7.5 sentence stays exactly as general as the
    // engine's answer is. It is not called "the unsupported feature": nothing
    // in the engine answers that question today, and re-deriving it over the
    // TypeScript IR mirror is the second producer D-14 forbids.
    //
    // It costs nothing. `argument` is already in hand; asking the OG printer
    // for a refusal message would be one more IPC per crossing save to
    // manufacture evidence the save path did not need.
    const changed = crossing ? boundedFeature(argument) : null;
    const node = doc.byId[props.blockId];
    let wrote = false;
    const write = () => {
      rewriteMacro(`{{${name} ${argument}}}`);
      writeViewProperties(next.view);
      wrote = true;
    };
    // ONE undo unit for the whole save, under both macro names (§4.3 directive
    // migration). The tag is what the §7.5 notice's [Undo that change] button
    // recognises, so only a CROSSING save carries the crossing tag.
    if (!node) return false;
    withUndoUnit(
      crossing ? `query:cross:${props.blockId}` : `query:save:${props.blockId}`,
      [node.page],
      write,
    );
    if (!wrote) return false;
    // The notice is the user half of the crossing (§7.5): the bytes changed
    // under the user without asking, so say so and offer the way back.
    if (crossing && node) setCrossed(props.blockId, changed);
    rememberDisplay(next.view);
    return true;
  };
  /** **The view lives in the block's `tine.*` properties (§7.6), for BOTH macro
   *  names (§4.3 "Directive migration", Q15).**
   *
   *  TQL text carries no view directives at all, and OG text carries only two of
   *  the six — so before this the frontend wrote the properties on a crossing
   *  only, `og_view` re-emitted `(aggregate …)`/`(group-by …)` back into OG text
   *  (a second home for the same value, which then outlived its removal in the
   *  builder), and `tine.columns::`/`tine.view::` were written by nobody and
   *  lost on every save.
   *
   *  **The baseline is what the block's properties currently SPELL, not which
   *  control the user touched** (P5A). The two readings differ exactly where it
   *  matters: `og_view` re-emits only `(sort-by …)` and `(sample …)`, so a
   *  grouping or an aggregate authored in the query TEXT is dropped by the
   *  reprint of an unrelated filter edit, and only a property write keeps it.
   *  Comparing against the persisted value materializes precisely those facts —
   *  and a crossing to TQL, where the text keeps nothing at all, materializes
   *  the whole effective view for the same reason.
   *
   *  Clearing is not cosmetic: `tine.*` has ABSOLUTE precedence over the DSL
   *  text, so a stale `tine.sort::` line would put a removed sort straight back
   *  on the next parse. And clearing a property the block never had is the
   *  identity on its bytes (`markdownRawWithProperty(raw, k, null)`), so an
   *  ordinary query block gains no property lines it has no view for (I-4).
   *
   *  What it must NOT do is rewrite the block's other metadata. `tine.fields::`
   *  is a TYPED SHEET SCHEMA, not a column list: writing the view's columns into
   *  it destroyed a declared schema on every filter save. Columns now live in
   *  `tine.columns::`, and the only time this writer touches `tine.fields` is to
   *  retire a value PROVEN to be a pre-split bare column list, in the same undo
   *  unit as the save that replaces it. The mapping itself is pure and shared
   *  with the query table (`editor/queryViewProperties.ts`); the side effect
   *  stays here, on the store's one property writer (D-7). */
  const blockPropertyPairs = () => {
    const blockId = props.blockId;
    const node = blockId ? doc.byId[blockId] : undefined;
    return blockId && node ? facetsOf(node.raw, formatForBlock(blockId)).properties : null;
  };
  /** The write set of a DISPLAY edit: only the facts it changed.
   *
   *  A save reprints the query text and has to materialize whatever that
   *  reprint would drop, so it uses the full patch. A display edit reprints
   *  nothing — and `before` is the ENGINE's last reading, which a second edit
   *  made inside one parse round-trip still carries, so restating the untouched
   *  facts from it would undo the first edit. */
  const displayPropertyWrites = (before: ViewSettings, view: ViewSettings) => {
    const properties = blockPropertyPairs();
    return properties ? queryDisplayPropertyWrites({ before, view, properties }) : [];
  };
  /** The property half of a SAVE, which reprints — so it keeps the WIDE
   *  baseline: only the block's own properties can say which facts live solely
   *  in the query text and are about to be dropped (P5A). */
  const writeViewProperties = (view: ViewSettings) => {
    const blockId = props.blockId;
    const properties = blockPropertyPairs();
    if (!blockId || !properties) return;
    for (const [key, value] of queryViewPropertyPatch({ view, properties })) {
      setBlockProperty(blockId, key, value);
    }
  };
  /** **Apply a DISPLAY change** — view, grouping, columns, aggregates, sort or
   *  sample — through the ONE audited write path (D-7).
   *
   *  Grouping, columns and aggregates have no home in the query TEXT at all:
   *  `og_view` re-emits only `(sort-by …)` and `(sample …)`. So stating them is
   *  a property write and nothing else — no printer round-trip, no macro
   *  rewrite, and no way for a refused print to swallow a display change the
   *  user just made.
   *
   *  Sort and sample DO print. `tine.*` outranks the text either way, so the
   *  property write alone would already render correctly — but leaving the text
   *  spelling a sort the block no longer uses is a second, contradictory home
   *  for one fact, and it is the spelling an outside editor sees. Those two go
   *  through the ordinary save, which reprints the macro in the same undo unit.
   *
   *  Either way the write set is the shared patch's, so nothing the block
   *  already spells is rewritten and no key outside the six is touched. The
   *  property-only path narrows it further, to the facts this edit CHANGED
   *  (`queryDisplayPropertyWrites`): `before` is the engine's last reading and
   *  the engine re-reads asynchronously, so a second edit made inside one parse
   *  round-trip still carries the reading that predates the first — and
   *  restating that reading's untouched facts would undo it (I-20). A save has
   *  to keep the wide baseline, because its reprint drops text-only facts. */
  const applyDisplay = async (next: ViewSettings) => {
    const blockId = props.blockId;
    if (!blockId) return;
    const before = displayView();
    const reprints =
      serializeQuerySort(before.sort) !== serializeQuerySort(next.sort) ||
      (before.sample ?? null) !== (next.sample ?? null);
    const reading = parsed.latest;
    if (reprints && reading) {
      await applyEdit({ query: reading.query, view: next });
      return;
    }
    const node = doc.byId[blockId];
    if (!node) return;
    const writes = displayPropertyWrites(before, next);
    // Nothing to say is not an undo entry: re-picking the view a block already
    // has must not leave a step the user has to take back.
    if (writes.length === 0) return;
    // ONE undo unit for the whole display change, so a grouping switch that also
    // retires a legacy key is taken back as one step.
    withUndoUnit(`query:display:${blockId}`, [node.page], () => {
      for (const [key, value] of writes) setBlockProperty(blockId, key, value);
    });
    rememberDisplay(next);
  };
  // Edit the query's display title (:title "…" in the options map). Only offered
  // for a user-authored standalone query (blockId set, no app-supplied title).
  const [editingTitle, setEditingTitle] = createSignal(false);
  const titleText = () => props.title ?? titleOption() ?? "Query";
  const titleEditable = () => !!props.blockId && props.title === undefined;
  // **A title edit is not a filter conversion (§4.3.1).** The new options map is
  // handed back to the printer with `preserveForm`, which re-emits
  // `source.original` verbatim and never re-lowers the IR — so renaming a query
  // the engine only partly understands cannot rewrite the author's filter, and a
  // query that OG could not express is still renameable. The dialect comes off
  // the SOURCE, not the macro name: a `{{query …}}` holding datalog prints as
  // `advanced_macro`.
  const setTitle = async (t: string) => {
    if (!props.blockId) return;
    const reading = parsed.latest;
    if (!reading) return;
    const inner = opts().replace(/^\{|\}$/g, "").trim();
    // Drop any existing :title (escape-aware), keep the other options.
    const rest = inner.replace(/:title\s+"(?:[^"\\]|\\.)*"\s*/, "").trim();
    // Strip chars that would break the {{…}} macro / {…} options map; escape the
    // rest so quotes/backslashes round-trip faithfully through a re-parse.
    const title = t.trim().replace(/[\r\n{}]/g, "");
    const parts = [title ? `:title "${quoteEdnString(title)}"` : "", rest].filter(Boolean);
    const nextOptions = parts.length ? `{${parts.join(" ")}}` : "";
    const nextQuery: Query = {
      ...reading.query,
      source: { ...reading.query.source, og_options: nextOptions } as Source,
    };
    try {
      const argument = await backend().printQuery(
        nextQuery,
        reading.view,
        sourcePrintDialect(reading.query.source),
        true,
      );
      setPrintError(null);
      rewriteMacro(`{{${macroName()} ${argument}}}`);
    } catch (error) {
      // I-4 / T7: a refused print is NEVER swallowed. Nothing is written, and the
      // reason is shown next to the edit that provoked it. A catch-all that
      // turned this into a silent no-op is how an unsaved rename looks saved.
      setPrintError(errorText(error));
    }
  };
  const [printError, setPrintError] = createSignal<string | null>(null);
  /** The undo tag of the crossing save this block's notice is offering to take
   *  back, or null when no notice is showing (§7.5). Holding the TAG rather than
   *  a bare flag is what lets the button know whether Undo would still reverse
   *  THIS change. */
  const [crossedTag, setCrossedTag] = createSignal<string | null>(null);
  /** A bounded excerpt of the ENGINE-PRINTED text the crossing save wrote, or
   *  `null`. Never a claim about which subexpression is unsupported. */
  const [crossedText, setCrossedText] = createSignal<string | null>(null);
  /** **The notice's own UI state, lifted out of the component (N3).**
   *
   *  The notice moves between two hosts — inline under the block while the sheet
   *  is closed, inside the query text pane while it is open — and a component
   *  that is re-parented is a component that is re-created. A tick the user made
   *  on "Don't show this again" must survive that move, and the focus grab must
   *  NOT happen again on every move: it belongs to the moment the bytes changed,
   *  not to opening a sheet. */
  const [noticeDontShow, setNoticeDontShow] = createSignal(false);
  const [noticeFocused, setNoticeFocused] = createSignal(false);
  const setCrossed = (blockId: string, changed: string | null) => {
    // One read per graph, shared by every query block (I-13) — never a lookup
    // per block per render.
    primeNoticeDismissals();
    setCrossedText(changed);
    setNoticeDontShow(false);
    setNoticeFocused(false);
    setCrossedTag(`query:cross:${blockId}`);
  };
  const dismissCrossingNotice = () => {
    if (noticeDontShow()) dismissNotice(CROSSING_NOTICE);
    setCrossedTag(null);
  };
  /** The notice shows only once this device's answer is KNOWN and is "not
   *  dismissed". `undefined` (still reading) renders nothing rather than a
   *  banner that appears and then retracts itself. */
  const showCrossingNotice = () =>
    !!crossedTag() && noticeDismissed(CROSSING_NOTICE) === false;
  /** Undo is offered only while the entry `undo()` would take back IS the
   *  crossing save. Any later edit — including one on another page, in page-only
   *  history mode — makes this false, and the button says to use Ctrl+Z. */
  const crossingIsStillUndoable = () => {
    const tag = crossedTag();
    return !!tag && undoTopTag() === tag;
  };
  /** Whether the builder's sheet is open, which decides WHERE the one notice is
   *  drawn. Nothing else reads it. */
  const [sheetOpen, setSheetOpen] = createSignal(false);
  /** The one notice, built once and placed by whichever host is current. */
  const crossingNotice = () => (
    <CrossingNotice
      canUndo={crossingIsStillUndoable()}
      changed={crossedText() ?? undefined}
      dontShow={noticeDontShow()}
      onDontShowChange={setNoticeDontShow}
      autoFocus={!noticeFocused()}
      onFocused={() => setNoticeFocused(true)}
      onUndo={() => {
        undo();
        dismissCrossingNotice();
      }}
      onKeep={dismissCrossingNotice}
      onDontShowAgain={() => dismissNotice(CROSSING_NOTICE)}
    />
  );
  /** What the builder edits: the AUTHORING reading, never the execution's
   *  dyvar-substituted one — editing a chip must not bake the page you happen to
   *  be looking at into the saved query. */
  const builderSession = (): BuilderSession | undefined => {
    const reading = parsed.latest;
    return reading ? { query: reading.query, view: reading.view } : undefined;
  };

  // Whether this is an advanced (datalog) query is the ENGINE's reading of the
  // text, not a regex over it (§7.1): a `:find` inside a string literal is text.
  const isAdvanced = () => source()?.kind === "advanced";
  const currentPageInput = createMemo(() =>
    isAdvanced() && declaresCurrentPageInput(form())
  );
  // The `⚙ advanced` / `← Simple` pair is gone with the frontend's datalog
  // converters (§9 P0-ts: "datalog conversion deleted"). Both directions were a
  // second query translator living in `queryBuilder.ts` — the one that dropped
  // sort/aggregate/group-by on the way out and could only read back the exact
  // shape it had written. Converting an authored advanced query into a filter is
  // explicitly out of scope (§4.3.1, Q13); an advanced block stays editable as
  // raw text by clicking it, with the ran/ignored note above saying what took.
  const currentPage = () => props.currentPage ?? (props.blockId ? doc.byId[props.blockId]?.page : undefined);
  interface QueryOperationResult {
    statistics?: import("../editor/queryIr").QueryStatistics;
    statisticsView?: ViewSettings;
    groups: RefGroup[];
    advInfo: { ran: string[]; ignored: string[]; supported: boolean } | null;
    // `@page`-anchored rows (K16) are pages rather than degenerate empty groups.
    pageRows: PageRow[] | null;
    // Exact complete count for page rows; null for block/search answers.
    matchedTotal: number | null;
    // Search hits carry evidence used only by the Search presentation.
    searchExecution: QueryExecution | null;
    // An INVALID query returns zero rows plus the engine's own diagnostics.
    diagnostics: Diagnostic[];
  }
  const collapseKey = () => JSON.stringify([
    graphMeta()?.root ?? "",
    props.blockId ?? currentPage() ?? "global",
    arg(),
  ]);
  const storedCollapse = loadCollapsed(collapseKey());
  const [collapsed, setCollapsed] = createSignal(storedCollapse ?? false);
  // `{:collapsed? true}` is an authored DEFAULT, not a reader's choice, so it
  // only applies when this reader has no stored preference for this query. It
  // can only be honoured once the engine has separated the options map from the
  // form, which is why it is seeded when the parse lands rather than at setup.
  if (storedCollapse === undefined || storedCollapse === null) {
    let seeded = false;
    createEffect(() => {
      if (seeded || !parsed.latest) return;
      seeded = true;
      if (collapsedOption()) setCollapsed(true);
    });
  }
  const toggleCollapsed = () => {
    const v = !collapsed();
    setCollapsed(v);
    saveCollapsed(collapseKey(), v);
  };
  // Re-run when the query text changes OR after any save lands (dataRev), so
  // results track edits live — e.g. a task flipped to DONE leaves a (task TODO)
  // query. createResource keeps the previous value during refetch (no flicker).
  // A COLLAPSED query keys off the form only (no dataRev), so it fetches once for
  // its count and doesn't re-run a whole-graph scan on every save while hidden;
  // expanding it (key flips to include dataRev) refreshes it.
  // Nothing runs before the engine has read the text: the form the executor is
  // given is `source.original`, and until the parse lands there is no form —
  // only the raw argument, which still carries the options map. Returning
  // `undefined` keeps `createResource` from fetching at all, rather than running
  // a query nobody authored.
  const queryMembershipIdentity = (): string | undefined => {
    const reading = runnable();
    if (!reading) return undefined;
    // The IR, not the text, is what runs — so it is what identifies the run. Two
    // blocks whose text differs only in whitespace share a request; two blocks
    // whose text is identical but whose `tine.*` properties differ do not.
    const identity = JSON.stringify([reading.query, reading.view]);
    // Only a query that binds `?current-page` needs the focused page in its key:
    // for those the text is identical on both pages and only the execution
    // context differs. A `<% current page %>` query must NOT key off it — the
    // substitution lives in the TEXT, so its reading changes a beat later, and a
    // key that moved first would run the OLD query under the NEW page: one extra
    // execution of a state the user was never in.
    const binding = currentPageInput() ? `\0cp:${executionPage() ?? ""}` : "";
    return `${graphEpoch()}\0${collapsed() ? "collapsed\0" : "expanded\0"}${identity}${binding}`;
  };
  interface DisplayedQueryOperation extends QueryOperationResult {
    identity: string;
  }
  const [displayedOperation, setDisplayedOperation] = createSignal<DisplayedQueryOperation>();
  const refreshRevision = createQueryRefreshRevision({
    revision: dataRev,
    identity: queryMembershipIdentity,
    containsDisplayedBlock: (blockId) => displayedOperation()?.groups.some((group) =>
      group.blocks.some((block) => block.id === blockId)
    ) ?? false,
  });
  const queryRequestKey = (): string | undefined => {
    const identity = queryMembershipIdentity();
    if (!identity) return undefined;
    return collapsed() ? identity : `${identity}\0revision:${refreshRevision()}`;
  };
  /** The page an execution binds `?current-page` to (§4.4). `:inputs
   *  [:current-page]` is a focused-pane binding; an advanced form without it
   *  retains the owner page for `:query-page` compatibility. */
  const executionPage = () => (currentPageInput() ? focusedQueryPage() : currentPage());
  const fetchQueryOperation = async (
    requestKey: string,
    signal: AbortSignal,
  ): Promise<QueryOperationResult> => {
    {
      const scope = sharedQueryScope(graphMeta()?.root, graphEpoch(), graphBinding());
      const searchSource = friendlySearch();
      if (searchSource !== null) {
        const execution = await sharedQueryResult(
          scope,
          `friendly-search\0${requestKey}`,
          () => backend().runGraphSearch(
            searchSource,
            500,
            5_000,
            `inline-query:${props.blockId ?? currentPage() ?? "global"}`,
            false
          ),
          signal,
        );
        if (queryRequestKey() !== requestKey) {
          return { groups: [], advInfo: null, pageRows: null, matchedTotal: null, searchExecution: null, diagnostics: [] };
        }
        // The Search presentation renders these hits directly rather than the
        // RefGroups below, so the host block has to come out here too — the same
        // exclusion `withoutHostBlock` makes, at the other place membership is
        // decided (GH #469). Diagnostics and the explanation are untouched: the
        // hit was really found, it is just not shown to itself.
        const hits = props.blockId
          ? execution.hits.filter((hit) => !(hit.entity === "block" && hit.block.id === props.blockId))
          : execution.hits;
        const visibleExecution = hits.length === execution.hits.length ? execution : { ...execution, hits };
        const grouped = new Map<string, RefGroup>();
        for (const hit of hits) {
          if (hit.entity !== "block") continue;
          const key = `${hit.kind}\0${hit.page}\0${hit.path ?? ""}`;
          const group = grouped.get(key) ?? { page: hit.page, kind: hit.kind, path: hit.path, blocks: [] };
          group.blocks.push(hit.block);
          grouped.set(key, group);
        }
        return {
          groups: [...grouped.values()],
          advInfo: null,
          pageRows: null,
          matchedTotal: null,
          searchExecution: visibleExecution,
          diagnostics: [],
        };
      }
      // **One evaluator (§7.1, B1).** `run_query` re-parsed the OG text and
      // `run_advanced_query` re-parsed the datalog; both are now the same
      // `query_run` over an IR the engine already read, which is also the only
      // reason a `{{tine-query …}}` block can return rows at all — the legacy
      // entry points cannot read TQL. Which grammar the text was is settled by
      // then, and the advanced ran/ignored report rides on the result rather than
      // on a separate command.
      const reading = runnable();
      if (!reading) {
        return { groups: [], advInfo: null, pageRows: null, matchedTotal: null, searchExecution: null, diagnostics: [] };
      }
      const page = executionPage();
      const result = await sharedQueryResult(
        scope,
        `ir\0${page ?? ""}\0${requestKey}`,
        () => backend().queryRun(reading.query, reading.view, page ? { current_page: page } : undefined),
        signal,
      );
      // I-20: the user has edited since this run started; its answer is about a
      // query that is no longer on screen.
      if (queryRequestKey() !== requestKey) {
        return { groups: [], advInfo: null, pageRows: null, matchedTotal: null, searchExecution: null, diagnostics: [] };
      }
      if (result.anchor === "page") {
        return {
          groups: [],
          advInfo: isAdvanced() ? reportInfo(result.report) : null,
          pageRows: result.pages,
          statistics: result.statistics,
          statisticsView: reading.view,
          matchedTotal: result.matched_total ?? result.total,
          searchExecution: null,
          diagnostics: result.diagnostics ?? [],
        };
      }
      return {
        groups: result.groups,
        statistics: isAdvanced() ? undefined : result.statistics,
        statisticsView: reading.view,
        advInfo: isAdvanced() ? reportInfo(result.report) : null,
        pageRows: null,
        matchedTotal: null,
        searchExecution: null,
        diagnostics: result.diagnostics ?? [],
      };
    }
  };
  // A query must not return the block it is written in. `{{query "xyz"}}`
  // contains `xyz`, so the backend answers honestly and the block matches its
  // own query — then the result renders the page it lives on, which renders the
  // query, which lists the page again. OG removes exactly the host block for
  // this reason and says so where it does it (frontend/components/query/result.cljs
  // at 6e7afa8e: "exclude the current one, otherwise it'll loop forever"). Its
  // children are NOT removed; only the block itself. Applied once here, after
  // every fetch path, rather than in each of the three (GH #469).
  const [groupResource, groupsPending] = createReadyQueryResource(
    queryRequestKey,
    async (requestKey, signal) => {
      const identity = queryMembershipIdentity() ?? "";
      const operation = await fetchQueryOperation(requestKey, signal);
      return {
        ...operation,
        identity,
        groups: withoutHostBlock(operation.groups, props.blockId),
      };
    },
  );
  // A membership answer that removes the actively edited result would destroy
  // its mounted textarea and DOM-local selection. Keep the whole previous
  // operation — groups/page rows, search evidence, diagnostics, report, and
  // therefore count/order — coherent while that editor is active. Only the
  // newest completed operation waits here, and it publishes as soon as the
  // ordinary editor lifecycle ends. A changed query/graph/view identity never
  // inherits deferred metadata from this hold.
  let pendingFocusedOperation: DisplayedQueryOperation | undefined;
  createEffect(() => {
    if (groupResource.error) {
      pendingFocusedOperation = undefined;
      return; // Keep the previous coherent rows/statistics pair with error status.
    }
    const candidate = groupResource();
    if (!candidate) {
      pendingFocusedOperation = undefined;
      setDisplayedOperation(undefined);
      return;
    }
    const current = untrack(displayedOperation);
    const focused = editingId();
    const focusedWasDisplayed = !!focused && current?.identity === candidate.identity
      && current.groups.some((group) => group.blocks.some((block) => block.id === focused));
    const focusedStillPresent = !!focused
      && candidate.groups.some((group) => group.blocks.some((block) => block.id === focused));
    if (focusedWasDisplayed && !focusedStillPresent) {
      pendingFocusedOperation = candidate;
      return;
    }
    pendingFocusedOperation = undefined;
    setDisplayedOperation(candidate);
  });
  createEffect(on(editingId, (focused) => {
    if (focused !== null || !pendingFocusedOperation) return;
    const candidate = pendingFocusedOperation;
    pendingFocusedOperation = undefined;
    if (candidate.identity === queryMembershipIdentity()) setDisplayedOperation(candidate);
  }));
  createEffect(on(queryMembershipIdentity, (identity, previous) => {
    if (previous !== undefined && identity !== previous) pendingFocusedOperation = undefined;
  }));
  const groups = () => displayedOperation()?.groups;
  const advInfo = () => displayedOperation()?.advInfo ?? null;
  const pageRows = () => displayedOperation()?.pageRows ?? null;
  const matchedTotal = () => displayedOperation()?.matchedTotal ?? null;
  const searchExecution = () => displayedOperation()?.searchExecution ?? null;
  const diagnostics = () => displayedOperation()?.diagnostics ?? [];
  const emptyResultsMessage = () => groupsPending()?.message
    ?? (groupResource.error ? "Query results unavailable" : groupResource.loading ? "Loading query results…" : "No results");
  const groupsError = () => {
    const error = groupResource.error;
    if (!error) return null;
    const message = error instanceof Error ? error.message : String(error);
    const oversized = message.startsWith("result-too-large:");
    return {
      lead: oversized ? "Query result is too large to display safely:" : "Query couldn't be loaded:",
      message: message.replace(/^result-too-large:\s*/, ""),
    };
  };
  // Presentation never changes membership. Canonical `(search "…")` queries
  // already carry page/block hits and match evidence from QueryPlan. Ordinary
  // DSL queries return RefGroups, so adapt those same blocks into evidence-free
  // search rows instead of making the Search presentation appear empty.
  const searchPresentationHits = createMemo<QueryHit[]>(() => {
    if (friendlySearch() !== null) return searchExecution()?.hits ?? [];
    return (groups() ?? []).flatMap((group) => group.blocks.map((block) => ({
      entity: "block" as const,
      page: group.page,
      kind: group.kind,
      block,
      display_text: visibleBody(block.raw).join(" "),
      evidence: [],
    })));
  });
  const total = () => currentView() === "search"
    ? searchPresentationHits().length
    : (pageRows() ? (matchedTotal() ?? pageRows()!.length) : groups()?.reduce((a, g) => a + g.blocks.length, 0) ?? 0);
  // **Why empty? (Q14, N19; B1).** `query_explain_empty` was decoded and never
  // rendered, so a query that matched nothing said only "No results" — which is
  // the one moment a user most needs to know WHICH conjunct emptied it. Asked
  // only when the run actually came back empty, so an ordinary query costs one
  // command as before.
  const [explainOpen, setExplainOpen] = createSignal(false);
  const explanationRequest = createMemo(
    () => {
      const reading = runnable();
      const key = queryRequestKey();
      if (!explainOpen() || !reading || !key || total() > 0) return undefined;
      return { reading, key };
    },
    undefined,
    { equals: (before, after) => before?.key === after?.key },
  );
  const [explained, explanationPending] = createReadyQueryResource(
    explanationRequest,
    ({ reading }) => {
      const page = executionPage();
      return backend().queryExplainEmpty(
        reading.query,
        reading.view,
        page ? { current_page: page } : undefined,
      );
    },
  );
  /** The engine's answer, or the honest reason there is none: an unbound or
   *  unsupported query has no counts to report, and an empty row list without
   *  its diagnostics would read as "every conjunct matches nothing" instead of
   *  "this query never ran" (I-9). */
  const explainRows = (): EmptyExplanation[] => explained.error ? [] : explained()?.rows ?? [];
  const explainNotice = (): string | null => {
    if (explanationPending()) return explanationPending()!.message;
    if (explained.error) return explained.error instanceof Error ? explained.error.message : String(explained.error);
    const answer = explained();
    if (!answer) return null;
    const blocking = (answer.diagnostics ?? []).filter((d) => !d.disabled);
    if (blocking.length) return blocking.map((d) => d.message).join(" · ");
    if (!answer.report.supported) return "This query has no clauses Tine can run, so nothing was evaluated.";
    if (!answer.rows.length) return "Nothing in this graph matches this query.";
    return null;
  };
  // Sorted results retain the engine's global sequence. Page-level sorting can
  // still return several blocks per group; presentation cannot assume every
  // group is one row or use its changing membership as a component identity.
  const globalSort = createMemo(() => (view().sort ?? []).length > 0);
  const queryGroupKey = (group: RefGroup) =>
    JSON.stringify([group.kind, group.page, group.path ?? ""]);
  const groupedQueryByKey = createMemo(() =>
    new Map((groups() ?? []).map((group) => [queryGroupKey(group), group] as const))
  );
  // Retain group ownership through membership edits. Page-level sorts can
  // return several blocks in a group; block-level sorts can split a page into
  // many groups. Match by surviving physical block identities in linear work,
  // rather than putting the whole membership list into a component key. Keep
  // LiveRefGroup boundaries intact for navigation, disclosure and lazy loading.
  let nextFlatGroupKey = 0;
  const flatQueryByKey = createMemo((previous: Map<string, RefGroup> | undefined) => {
    const previousOwners = new Map<string, Map<string, string>>();
    for (const [key, group] of previous ?? []) {
      const pageKey = queryGroupKey(group);
      let owners = previousOwners.get(pageKey);
      if (!owners) { owners = new Map(); previousOwners.set(pageKey, owners); }
      for (const block of group.blocks) {
        owners.set(block.id, key);
      }
    }
    const next = new Map<string, RefGroup>();
    for (const group of groups() ?? []) {
      const overlaps = new Map<string, number>();
      const owners = previousOwners.get(queryGroupKey(group));
      for (const block of group.blocks) {
        const owner = owners?.get(block.id);
        if (owner !== undefined && !next.has(owner)) overlaps.set(owner, (overlaps.get(owner) ?? 0) + 1);
      }
      let retained: string | undefined;
      let largest = 0;
      for (const [key, count] of overlaps) {
        if (count > largest) { retained = key; largest = count; }
      }
      next.set(retained ?? `query-group-${nextFlatGroupKey++}`, group);
    }
    return next;
  });
  const [sortCol, setSortCol] = createSignal<string>("");
  const [sortDir, setSortDir] = createSignal(1);

  const rows = createMemo<Row[]>(() =>
    (groups() ?? []).flatMap((g) =>
      g.blocks.map((b) => {
        // Properties come off the DTO (computed once in Rust off the lsdoc parse);
        // the row's text is the visible body. No re-derivation here.
        const props: Record<string, string> = {};
        for (const [k, val] of b.properties ?? []) props[k] = val;
        return { page: g.page, kind: g.kind, path: g.path, text: visibleBody(b.raw).join(" "), props };
      })
    )
  );

  const cols = createMemo(() => {
    const keys = new Set<string>();
    for (const r of rows()) for (const k of Object.keys(r.props)) keys.add(k);
    return Array.from(keys);
  });

  // Result summarization (1a): `aggregate` / `group-by` are VIEW settings, lifted
  // out of the query text (or the block's `tine.*` properties) by the engine and
  // returned alongside the IR. The engine returns the full block set and ignores
  // them, so the math is computed HERE from the returned rows. Only the simple
  // DSL carries them (datalog aggregation is OG's :result-transform, which we
  // list as ignored).
  // The block's own formula definitions — the same map `SheetBoard` builds for a
  // query face (which is handed no schema page), so a `formula:` grouping reads
  // identically in the summary and in the board beside it.
  const queryFormulas = createMemo<ReadonlyMap<string, string>>(() => {
    const blockId = props.blockId;
    const owner = blockId ? doc.byId[blockId] : undefined;
    return owner ? formulasOf(facetsOf(owner.raw, formatForBlock(blockId!)).properties) : new Map();
  });
  // The SAME records `SheetTable` and `SheetBoard` flatten out of this result, so
  // the list summary and the board group ONE row set through ONE reader instead
  // of a second property lookup that can disagree with the face beside it.
  const queryRecords = createMemo<FormulaEvalRow[]>(() =>
    (groups() ?? []).flatMap((g) =>
      g.blocks.map((b): FormulaEvalRow => ({ id: b.id, page: g.page, kind: g.kind, dto: b }))
    )
  );
  // **The grouping is the ENGINE's answer, not a second reading.** §4.1's merge
  // already ran `query::view::resolve_query_grouping` over this block's `tine.*`
  // properties and its query text, so `view().group_by` is the canonical field
  // id — and the P5B wire spelling distinguishes the two empties that matter:
  // absent is "nothing said" (a Board may default), `""` is the user's explicit
  // "no grouping" (a Board may NOT).
  const grouping = createMemo(() => groupingFromViewValue(viewAfterViewSwitch(view(), view().view).group_by));
  /** The field this result groups by, or `null` when it is ungrouped — which an
   *  explicit clear and an absent setting both are, for a LIST. */
  const groupingField = createMemo<FieldId | null>(() => {
    const resolved = grouping();
    return resolved.kind === "field" && isFieldId(resolved.field) ? resolved.field : null;
  });
  /** The grouping vocabulary a QUERY board offers: the sheet builtins, the
   *  properties its OWN result rows carry (a children board reads the owner's
   *  children, which a query has none of), the source page, and the block's
   *  formula fields. */
  const queryFormulaNames = createMemo<string[]>(() => [...queryFormulas().keys()]);
  const queryGroupOptions = createMemo<FieldId[]>(() => {
    const formulaFields = [...queryFormulas().keys()].map((name): FieldId => `formula:${name}`);
    return boardGroupByOptions(fieldIdsForRecords(queryRecords(), false), [
      "page",
      ...formulaFields,
    ]);
  });
  /** The ONE writer every inline display change goes through: the Display panel,
   *  the query table's header sorts, its column order and its aggregate footer,
   *  and the board's grouping. */
  const queryDisplayControl = createMemo<QueryDisplayControl>(() => ({
    statistics: displayedOperation()?.statistics,
    statisticsView: displayedOperation()?.statisticsView,
    view: displayView(),
    apply: (next) => void applyDisplay(next),
    // The block's OWN bytes, not the engine's reading: what the query reader
    // does not understand never reaches `ViewSettings` at all, and the save
    // preserves it (§5). Reporting it is what makes that preservation visible.
    retainedAggregates: retainedQueryAggregateSegments(blockPropertyPairs() ?? []),
  }));
  /** The ONE writer a query board's grouping goes through, handed to both its
   *  toolbar dropdown and its context menu.
   *
   *  `cleared` carries the half of the resolution a bare field cannot: an
   *  explicit "no grouping" is one ungrouped column, while a grouping nothing
   *  states is the silence the Board's task-marker default fills (ADR 0030).
   *  Both arrive here as `field: null`, and a board that could not tell them
   *  apart would un-group every existing `tine.view:: board` note. */
  const queryGroupingControl = createMemo<QueryGroupingControl>(() => ({
    field: groupingField(),
    cleared: grouping().kind === "cleared",
    options: queryGroupOptions(),
    set: (field) => void applyDisplay({ ...displayView(), group_by: field ?? "" }),
  }));
  const summary = createMemo<QuerySummary | null>(() => {
    if (isAdvanced()) return null;
    const statistics = displayedOperation()?.statistics;
    const field = statistics?.group_by;
    return querySummary({ statistics, groupLabel: field && isFieldId(field) ? fieldLabel(field) : field });
  });

  const sorted = createMemo(() => {
    const c = sortCol();
    if (!c) return rows();
    const val = (r: Row) => (c === "page" ? r.page : c === "content" ? r.text : r.props[c] ?? "");
    return [...rows()].sort((a, b) => val(a).localeCompare(val(b)) * sortDir());
  });

  const sortBy = (c: string) => {
    if (sortCol() === c) setSortDir(-sortDir());
    else {
      setSortCol(c);
      setSortDir(1);
    }
  };
  // Clicks on query controls must not bubble to the block's onClick (which would
  // start editing the {{query}} block and replace results with raw markdown).
  const stop = (e: MouseEvent) => e.stopPropagation();
  const arrow = (c: string) => (sortCol() === c ? (sortDir() > 0 ? " ▲" : " ▼") : "");

  // Hide the whole block when asked and there's nothing to show (advanced
  // queries still render their "unsupported" notice).
  const hidden = () => props.hideWhenEmpty && !isAdvanced() && total() === 0;
  // The query text pane holds text that does not parse: the rows below are the
  // last reading that RAN, so they are greyed rather than blanked (§4.3.1).
  const [paneStale, setPaneStale] = createSignal(false);
  /** Whether the sentence-and-sheet builder is hosted for this block. The
   *  result count lives beside the sentence when it is, and in the header when
   *  it is not (§7.2). */
  const showBuilder = () => !!props.blockId && !isAdvanced() && !!builderSession();
  /** **Where the inline Display panel is offered** (P5B).
   *
   *  It needs a block to write `tine.*` to and a builder to host it — and it is
   *  NOT offered for a friendly search, whose execution returns search hits
   *  rather than the block result the six display facts describe. Offering a
   *  column order and a grouping for a hit list would be six controls that
   *  save and change nothing on screen.
   *
   *  The same condition removes the header's view switcher: the panel states
   *  the view, and two controls for one enum is how they drift apart. */
  const inlineDisplay = () => showBuilder() && friendlySearch() === null;
  const unsupportedAdvanced = () => isAdvanced() && advInfo() && (
    !advInfo()!.supported || (props.strictAdvanced === true && advInfo()!.ignored.length > 0)
  );

  return (
    <Show when={!hidden()}>
      <div
        class="query-block"
        classList={{ "query-sheet-block": sheetFace(), "query-stale": paneStale() }}
      >
        <Switch>
          <Match when={unsupportedAdvanced()}>
            <div class="query-unsupported" role={props.unsupportedLabel ? "alert" : undefined}>
              <Show
                when={props.unsupportedLabel}
                fallback={<>Advanced (datalog) query: no supported clauses. <code>{`{{${props.body}}}`}</code></>}
              >
                {(label) => <>{label()}: query contains unsupported clauses.</>}
              </Show>
            </div>
          </Match>
          <Match when={true}>
            <Show when={isAdvanced() && advInfo()?.supported}>
              <div class="query-adv-note">
                Partial datalog — ran: {advInfo()!.ran.join(", ") || "—"}
                <Show when={advInfo()!.ignored.length > 0}>
                  {` · ignored: ${advInfo()!.ignored.join(", ")}`}
                </Show>
              </div>
            </Show>
            <div class="query-header">
              <span
                class="query-collapse"
                classList={{ collapsed: collapsed() }}
                title={collapsed() ? "Expand results" : "Collapse results"}
                onClick={(e) => {
                  e.stopPropagation();
                  toggleCollapsed();
                }}
              >
                <svg viewBox="0 0 24 24" class="triangle">
                  <path d="M8 5l8 7-8 7z" />
                </svg>
              </span>
              <Show
                when={editingTitle()}
                fallback={
                  <span
                    class="query-title"
                    classList={{ "query-title-editable": titleEditable() }}
                    title={titleEditable() ? "Click to rename this query" : undefined}
                    onClick={(e) => {
                      if (titleEditable()) {
                        e.stopPropagation();
                        setEditingTitle(true);
                      }
                    }}
                  >
                    {titleText()}
                  </span>
                }
              >
                {(() => {
                  let canceled = false;
                  return (
                    <input
                      class="query-title-input"
                      autofocus
                      value={titleOption() ?? ""}
                      placeholder="Query title"
                      onClick={(e) => e.stopPropagation()}
                      onKeyDown={(e) => {
                        e.stopPropagation();
                        if (e.key === "Enter") {
                          void setTitle(e.currentTarget.value);
                          setEditingTitle(false);
                        } else if (e.key === "Escape") {
                          canceled = true;
                          setEditingTitle(false);
                        }
                      }}
                      onBlur={(e) => {
                        if (!canceled) void setTitle(e.currentTarget.value);
                        setEditingTitle(false);
                      }}
                    />
                  );
                })()}
              </Show>{" "}
              {/* §7.2 moves the count beside the resting SENTENCE, where the
                  builder renders it. It stays here for the queries that have no
                  builder — an authored advanced query, or a block whose reading
                  has not landed yet — so the count never disappears. */}
              <Show when={!showBuilder()}>
                <span class="query-count">{total()}</span>
              </Show>
              <Show when={props.blockId && !inlineDisplay()}>
                <div class="query-view-switcher" role="group" aria-label="Query view" onClick={stop}>
                  <For each={QUERY_VIEWS}>
                    {(view) => (
                      <button
                        type="button"
                        classList={{ active: currentView() === view }}
                        onClick={(e) => {
                          e.stopPropagation();
                          setQueryView(view);
                        }}
                      >
                        {QUERY_VIEW_LABEL[view]}
                      </button>
                    )}
                  </For>
                </div>
              </Show>
            </div>
            {/* The builder edits a FILTER. An authored advanced (datalog) query
                keeps its own editing path — converting one into a filter is out
                of scope (§4.3.1, Q13) — so the sentence and sheet stay hidden
                for it and the ran/ignored note above says which clauses took. */}
            <Show when={showBuilder()}>
              <QueryBuilder
                session={builderSession}
                onChange={applyEdit}
                paneDialect="tql"
                blockId={props.blockId}
                total={<span class="query-count">{total()}</span>}
                onStale={setPaneStale}
                onOpenChange={setSheetOpen}
                notice={showCrossingNotice() && sheetOpen() ? crossingNotice : undefined}
                inlineDisplay={inlineDisplay()}
                display={queryDisplayControl}
                displayFormulas={queryFormulaNames}
              />
            </Show>
            {/* §7.5, N3: **one** notice with one state, in one of two places.
                It sits here while the sheet is shut — where P2 put it, under the
                block whose bytes changed — and inside the query text pane while
                the sheet is open, where the changed text actually is. The two
                `Show`s are mutually exclusive, so it is never drawn twice and
                never silently hidden; its dismissal, its undo gating and its
                checkbox are this component's, not the pane's. */}
            <Show when={showCrossingNotice() && !sheetOpen()}>{crossingNotice()}</Show>
            <Show when={printError()}>
              {(message) => (
                <div class="query-unsupported query-print-refused" role="alert">
                  The query wasn't changed: {message()}
                </div>
              )}
            </Show>
            <Show when={groupsError()}>
              {(message) => (
                <div class="query-unsupported" role="alert">
                  {message().lead} {message().message}
                </div>
              )}
            </Show>
            <Show when={groupsPending() ?? explanationPending()}>
              {(pending) => <span class="query-readiness-status" role="status">{pending().message}</span>}
            </Show>
            {/* The run's OWN diagnostics. A query with an enabled diagnostic is
                invalid and returns zero rows plus these (§3.5) — showing only
                "No results" would report a broken query as an empty graph.
                The lead says what actually happened: the part below was not
                understood, so the query returned nothing. It does NOT say the
                part was "ignored", which would imply the rest still ran and
                these are its results — the exact misreading the unknown-head
                regression (REG-P0-QUERY-UNKNOWN-HEAD-001) exists to prevent. */}
            <Show when={diagnostics().some((d) => !d.disabled)}>
              <div class="query-unsupported query-diagnostics" role="alert">
                <span class="query-diagnostics-lead">
                  Tine didn't understand part of this query, so it returned no results:
                </span>{" "}
                {diagnostics().filter((d) => !d.disabled).map((d) => d.message).join(" · ")}
              </div>
            </Show>
            <Show when={!collapsed()}>
              <Show when={currentView() !== "search" || inlineDisplay()}>
                    {/* Summary panel (1a): the view's aggregates overall, or a
                        per-group breakdown. ALL requested aggregates render, in
                        the order the view lists them — a view carries a LIST, and
                        showing only its first entry dropped every other column
                        its author asked for. Rendered above the current presentation,
                        which stays grouped by page. */}
                    <Show when={summary()?.notice}>
                      <p class="query-summary-note">{summary()?.notice}</p>
                    </Show>
                    <Show when={summary()}>
                      {(s) => (
                        <Show
                          when={s().groups}
                          fallback={
                            <div class="query-summary" onClick={stop}>
                              <For each={s().columns}>
                                {(col, i) => (
                                  <span class="qs-entry">
                                    <span class="qs-label">{col.label}:</span>{" "}
                                    <span class="qs-value">{s().overall[i()]?.text ?? ""}</span>
                                    <Show when={(s().overall[i()]?.skipped ?? 0) > 0}>
                                      <span class="qs-skip">
                                        {" "}
                                        ({s().overall[i()]!.skipped} non-numeric skipped)
                                      </span>
                                    </Show>
                                  </span>
                                )}
                              </For>
                            </div>
                          }
                        >
                          {(rows) => (
                            <>
                              <table class="md-table query-summary-table" onClick={stop}>
                                <thead>
                                  <tr>
                                    <th>{s().groupLabel}</th>
                                    <For each={s().columns}>{(col) => <th>{col.label}</th>}</For>
                                  </tr>
                                </thead>
                                <tbody>
                                  <For each={rows()}>
                                    {(row) => (
                                      <tr>
                                        <td>{row.label}</td>
                                        <For each={row.cells}>
                                          {(cell) => (
                                            <td>
                                              {cell.text}
                                              <Show when={cell.skipped > 0}>
                                                <span class="qs-skip"> ({cell.skipped} skipped)</span>
                                              </Show>
                                            </td>
                                          )}
                                        </For>
                                      </tr>
                                    )}
                                  </For>
                                </tbody>
                              </table>
                              {/* Say the grouping is not a partition rather than
                                  let the counts look like they should add up. */}
                              <Show when={s().multiMembership}>
                                <p class="query-summary-note" onClick={stop}>
                                  A row with several tags appears in every matching group, so these
                                  counts can add up to more than the result.
                                </p>
                              </Show>
                            </>
                          )}
                        </Show>
                      )}
                    </Show>
              </Show>
              <Show
                when={sheetFace()}
                fallback={
                  <>
                    <Show when={currentView() === "search"}>
                      <div class="query-search-results" role="list" aria-label="Search results" onClick={stop}>
                        <Show
                          when={searchPresentationHits().length > 0}
                          fallback={<div class="query-empty">{emptyResultsMessage()}</div>}
                        >
                          <For each={searchPresentationHits()}>
                            {(hit) => (
                              <Show
                                when={hit.entity === "block" ? hit : null}
                                fallback={hit.entity === "page" ? (
                                  <button
                                    type="button"
                                    class="query-search-page"
                                    onMouseDown={internalLinkMouseDown}
                                    onClick={(e) => {
                                      const target = {
                                        name: hit.page.name,
                                        pageKind: hit.page.kind,
                                        ...(hit.page.path ? { path: hit.page.path } : {}),
                                      };
                                      const dest = internalLinkDest(e);
                                      if (dest === "sidebar") openPageInSidebar(target);
                                      else if (dest === "background") openPageTargetInNewTab(target);
                                      else if (dest === "pane") openRouteInOtherPane({ kind: "page", ...target });
                                      else openPageTarget(target);
                                    }}
                                    onAuxClick={(e) => internalLinkAuxClick(e, () => openPageTargetInNewTab({
                                      name: hit.page.name,
                                      pageKind: hit.page.kind,
                                      ...(hit.page.path ? { path: hit.page.path } : {}),
                                    }))}
                                  >
                                    <span class="switcher-kind">{hit.page.kind}</span>
                                    <span>{hit.display_text}</span>
                                  </button>
                                ) : null}
                              >
                                {(blockHit) => (
                                  <button
                                    type="button"
                                    class="query-search-hit switcher-row block-result"
                                    onMouseDown={internalLinkMouseDown}
                                    onClick={(e) => {
                                      const bh = blockHit();
                                      const uuid = blockDtoExternalId(bh.block);
                                      const dest = internalLinkDest(e);
                                      if (dest === "sidebar") {
                                        openBlockInSidebar({ uuid, page: bh.page, pageKind: bh.kind, ...(bh.path ? { path: bh.path } : {}) });
                                      } else if (dest === "background") {
                                        openInNewTab({ kind: "page", name: bh.page, pageKind: bh.kind, block: uuid, ...(bh.path ? { path: bh.path } : {}) });
                                      } else if (dest === "pane") {
                                        openRouteInOtherPane({ kind: "page", name: bh.page, pageKind: bh.kind, block: uuid, ...(bh.path ? { path: bh.path } : {}) });
                                      } else {
                                        openPageAtBlock({
                                          name: bh.page,
                                          pageKind: bh.kind,
                                          block: uuid,
                                          ...(bh.path ? { path: bh.path } : {}),
                                        });
                                      }
                                    }}
                                    onAuxClick={(e) => internalLinkAuxClick(e, () => {
                                      const bh = blockHit();
                                      openInNewTab({ kind: "page", name: bh.page, pageKind: bh.kind, block: blockDtoExternalId(bh.block), ...(bh.path ? { path: bh.path } : {}) });
                                    })}
                                  >
                                    <SearchResultRow
                                      page={blockHit().page}
                                      breadcrumb={blockHit().block.breadcrumb ?? []}
                                      text={blockHit().display_text}
                                      spans={blockHit().evidence.flatMap((evidence) => evidence.spans)}
                                    />
                                  </button>
                                )}
                              </Show>
                            )}
                          </For>
                        </Show>
                      </div>
                    </Show>
                    <Show when={currentView() !== "search"}>
                    {/* `@page`-anchored results are pages, not blocks (K16): they
                        carry their physical owner and need no document
                        load, so they render as page links rather than as empty
                        block groups. */}
                    <Show when={pageRows()}>
                      {(rows) => (
                        <div class="query-page-rows" role="list" aria-label="Matching pages" onClick={stop}>
                          <For each={rows()}>
                            {(row) => (
                              <button
                                type="button"
                                class="query-page-row"
                                role="listitem"
                                onMouseDown={internalLinkMouseDown}
                                onClick={(e) => {
                                  e.stopPropagation();
                                  const target = { name: row.name, pageKind: row.kind, path: row.path };
                                  const dest = internalLinkDest(e);
                                  if (dest === "sidebar") openPageInSidebar(target);
                                  else if (dest === "background") openPageTargetInNewTab(target);
                                  else if (dest === "pane") openRouteInOtherPane({ kind: "page", ...target });
                                  else openPageTarget(target);
                                }}
                                onAuxClick={(e) => internalLinkAuxClick(e, () =>
                                  openPageTargetInNewTab({ name: row.name, pageKind: row.kind, path: row.path })
                                )}
                              >
                                <span class="switcher-kind">{row.kind}</span>
                                <span>{row.name}</span>
                              </button>
                            )}
                          </For>
                        </div>
                      )}
                    </Show>
                    <Show
                      when={groups() && groups()!.length > 0}
                      fallback={
                        <Show when={!pageRows()?.length}>
                          <div class="query-empty">
                            {emptyResultsMessage()}{" "}
                            {/* §7.5: zero results is the one moment a user most
                                needs to know WHICH conjunct emptied the query,
                                and the engine can already say. */}
                            <button
                              type="button"
                              class="query-why-empty"
                              disabled={groupResource.loading || !!groupResource.error}
                              onClick={(e) => { e.stopPropagation(); setExplainOpen(!explainOpen()); }}
                            >
                              {explainOpen() ? "hide" : "why empty?"}
                            </button>
                            <Show when={explainOpen()}>
                              <div class="query-why-empty-panel" onClick={stop}>
                                <Show when={explained.loading}>
                                  <span class="query-why-empty-pending">Checking…</span>
                                </Show>
                                <Show when={explainNotice()}>
                                  {(notice) => <div class="query-why-empty-notice">{notice()}</div>}
                                </Show>
                                <Show when={explainRows().length > 0}>
                                  <table class="md-table query-why-empty-table">
                                    <thead>
                                      <tr>
                                        <th>Condition</th>
                                        <th>Alone</th>
                                        <th>Without it</th>
                                      </tr>
                                    </thead>
                                    <tbody>
                                      <For each={explainRows()}>
                                        {(row) => (
                                          <tr classList={{ "query-why-empty-culprit": row.alone === 0 }}>
                                            <td><code>{row.conjunct}</code></td>
                                            <td>{row.alone}</td>
                                            <td>{row.without ?? "—"}</td>
                                          </tr>
                                        )}
                                      </For>
                                    </tbody>
                                  </table>
                                </Show>
                              </div>
                            </Show>
                          </div>
                        </Show>
                      }
                    >
                      <Show
                        when={legacyTable()}
                        fallback={
                          <Show
                            when={globalSort()}
                            fallback={
                              <For each={[...groupedQueryByKey().keys()]}>
                                {(key) => <QueryGroup group={() => groupedQueryByKey().get(key)} />}
                              </For>
                            }
                          >
                            {/* Sorted: flat global order (each group holds one block). Iterate the
                                groups DIRECTLY and pass the group object — re-`find()`ing the group
                                by page/id for every row was O(groups²) on broad queries (audit #3). */}
                            <For each={[...flatQueryByKey().keys()]}>
                              {(key) => <QueryGroup group={() => flatQueryByKey().get(key)} flat />}
                            </For>
                          </Show>
                        }
                      >
                        <table class="md-table query-table">
                          <thead>
                            <tr onClick={stop}>
                              <th onClick={() => sortBy("content")}>Content{arrow("content")}</th>
                              <th onClick={() => sortBy("page")}>Page{arrow("page")}</th>
                              <For each={cols()}>
                                {(c) => <th onClick={() => sortBy(c)}>{c}{arrow(c)}</th>}
                              </For>
                            </tr>
                          </thead>
                          <tbody>
                            <For each={sorted()}>
                              {(r) => (
                                <tr>
                                  <td>
                                    <InlineText text={r.text} format={formatForPage(r.page)} />
                                  </td>
                                  <td
                                    class="qt-page"
                                    onMouseDown={internalLinkMouseDown}
                                    onClick={(e) => {
                                      e.stopPropagation();
                                      const target = { name: r.page, pageKind: r.kind, ...(r.path ? { path: r.path } : {}) };
                                      const dest = internalLinkDest(e);
                                      if (dest === "sidebar") openPageInSidebar(target);
                                      else if (dest === "background") openPageTargetInNewTab(target);
                                      else if (dest === "pane") openRouteInOtherPane({ kind: "page", ...target });
                                      else openPageTarget(target);
                                    }}
                                    onAuxClick={(e) => {
                                      if (internalLinkAuxClick(e, () =>
                                        openPageTargetInNewTab({ name: r.page, pageKind: r.kind, ...(r.path ? { path: r.path } : {}) })
                                      )) e.stopPropagation();
                                    }}
                                    onContextMenu={(e) => {
                                      if (!shouldOpenTextContextMenu(e.target)) return;
                                      e.preventDefault();
                                      e.stopPropagation();
                                      openPageContextMenu(e.clientX, e.clientY, { name: r.page, pageKind: r.kind, ...(r.path ? { path: r.path } : {}) });
                                    }}
                                  >
                                    {r.page}
                                  </td>
                                  <For each={cols()}>{(c) => <td>{r.props[c] ?? ""}</td>}</For>
                                </tr>
                              )}
                            </For>
                          </tbody>
                        </table>
                      </Show>
                    </Show>
                    </Show>
                  </>
                }
              >
                <Show when={groups() && groups()!.length > 0} fallback={<div class="query-empty">{emptyResultsMessage()}</div>}>
                  <Show when={(sheet()?.view === "table" || sheet()?.view === "board") && props.blockId}>
                    <SheetContainer>
                      <Switch>
                        <Match when={sheet()?.view === "table"}>
                          <SheetTable
                            ownerId={props.blockId!}
                            rowSource="query"
                            groups={groups() ?? []}
                            queryDisplay={queryDisplayControl()}
                          />
                        </Match>
                        <Match when={sheet()?.view === "board"}>
                          <SheetBoard
                            ownerId={props.blockId!}
                            rowSource="query"
                            groups={groups() ?? []}
                            queryGrouping={queryGroupingControl()}
                          />
                        </Match>
                      </Switch>
                    </SheetContainer>
                  </Show>
                </Show>
              </Show>
            </Show>
          </Match>
        </Switch>
      </div>
    </Show>
  );
}

// One page's query results, rendered as LIVE editable blocks. The result page
// is loaded into the shared working set on demand; each result is the same
// <Block> the main view uses (so editing a result edits the real block and
// saves to its page). Until the page is loaded, a read-only block stands in.
//
// Keyed by page name (outer <For>) and block uuid (inner <For>) so a reactive
// re-query that returns the same membership reuses the existing rows — it never
// re-mounts a block you're editing in a result and yanks the caret out.
function QueryGroup(props: { group: () => RefGroup | undefined; flat?: boolean }): JSX.Element {
  const kind = (): PageKind => props.group()?.kind ?? "page";
  const page = () => props.group()?.page ?? "";
  const target = () => ({ name: page(), pageKind: kind(), ...(props.group()?.path ? { path: props.group()!.path } : {}) });
  return (
    <Show when={props.group()}>
      {(g) => (
        <div class="query-group" classList={{ "query-group-flat": props.flat }}>
          <div
            class={props.flat ? "query-crumb" : "query-page"}
            onMouseDown={internalLinkMouseDown}
            onClick={(e) => {
              e.stopPropagation();
              const dest = internalLinkDest(e);
              if (dest === "sidebar") openPageInSidebar(target());
              else if (dest === "background") openPageTargetInNewTab(target());
              else if (dest === "pane") openRouteInOtherPane({ kind: "page", ...target() });
              else openPageTarget(target());
            }}
            onAuxClick={(e) => {
              if (internalLinkAuxClick(e, () => openPageTargetInNewTab(target()))) e.stopPropagation();
            }}
            onContextMenu={(e) => {
              if (!shouldOpenTextContextMenu(e.target)) return;
              e.preventDefault();
              e.stopPropagation();
              openPageContextMenu(e.clientX, e.clientY, target());
            }}
          >
            {page()}
          </div>
          <LiveRefGroup page={page()} kind={kind()} path={g().path} blocks={g().blocks} surface="query" showBreadcrumb />
        </div>
      )}
    </Show>
  );
}

interface YoutubePlayer {
  seekTo(seconds: number, allowSeekAhead: boolean): void;
  getCurrentTime(): number;
  destroy?(): void;
}

interface YoutubeApi {
  Player: new (iframeId: string, options: { events?: { onReady?: () => void } }) => YoutubePlayer;
}

type YoutubeWindow = Window & {
  YT?: YoutubeApi;
  onYouTubeIframeAPIReady?: () => void;
};

const youtubePlayers = new Map<string, YoutubePlayer>();
let youtubeApiLoading: Promise<YoutubeApi | null> | null = null;
const YOUTUBE_API_SCRIPT_ID = "tine-youtube-iframe-api";

// The API is intentionally fetched only from a mounted YouTube embed. OG does
// the same mount-time load/register sequence (og-1.0.0 6e7afa8eb,
// extensions/video/youtube.cljs:20-27, :45-53).
function loadYoutubeApi(): Promise<YoutubeApi | null> {
  if (typeof window === "undefined" || typeof document === "undefined") return Promise.resolve(null);
  const ytWindow = window as YoutubeWindow;
  if (ytWindow.YT?.Player) return Promise.resolve(ytWindow.YT);
  if (youtubeApiLoading) return youtubeApiLoading;

  youtubeApiLoading = new Promise((resolve) => {
    let settled = false;
    const settle = (api: YoutubeApi | undefined) => {
      if (settled) return;
      settled = true;
      resolve(api?.Player ? api : null);
    };
    const priorReady = ytWindow.onYouTubeIframeAPIReady;
    ytWindow.onYouTubeIframeAPIReady = () => {
      try {
        priorReady?.();
      } finally {
        settle(ytWindow.YT);
      }
    };
    let script = document.getElementById(YOUTUBE_API_SCRIPT_ID) as HTMLScriptElement | null;
    if (!script) {
      script = document.createElement("script");
      script.id = YOUTUBE_API_SCRIPT_ID;
      script.async = true;
      script.src = "https://www.youtube.com/iframe_api";
      document.head.appendChild(script);
    }
    script.addEventListener("error", () => settle(undefined), { once: true });
  });
  return youtubeApiLoading;
}

// OG's get-player selects the last YouTube iframe whose DOM position precedes
// the target (compareDocumentPosition(..., target) has FOLLOWING set), then
// looks up that iframe's registered handle (og-1.0.0 6e7afa8eb,
// extensions/video/youtube.cljs:85-101).
export function youtubePlayerForTarget(target: Node): YoutubePlayer | undefined {
  if (typeof document === "undefined" || typeof Node === "undefined") return undefined;
  const iframe = Array.from(document.getElementsByTagName("iframe"))
    .filter((node) => node.src.includes("youtube.com"))
    .filter((node) => (node.compareDocumentPosition(target) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0)
    .at(-1);
  return iframe ? youtubePlayers.get(iframe.id) : undefined;
}

// The OG generator floors getCurrentTime before it formats the macro; with no
// registered/ready player it produces NOTHING — the command is a no-op, no
// macro is inserted (og-1.0.0 6e7afa8eb, extensions/video/youtube.cljs:113-122).
export function youtubeTimestampMacroFor(target: Node): string | null {
  const seconds = youtubePlayerForTarget(target)?.getCurrentTime();
  if (typeof seconds !== "number" || !Number.isFinite(seconds)) return null;
  return `{{youtube-timestamp ${Math.max(0, Math.floor(seconds))}}}`;
}

// A {{video}} / {{youtube}} / {{vimeo}} / {{bilibili}} macro: embeds YouTube,
// Vimeo or Bilibili as an iframe and direct media files as a <video>. Each of the
// provider-named macros also accepts a bare id (e.g. `{{vimeo 12345}}`), matching
// OG; the generic `{{video URL}}` sniffs the provider from the URL. Falls back to a
// link. (`youtube-timestamp` is a SEPARATE macro — handled before this one.)
export function VideoMacro(props: { body: string }): JSX.Element {
  const iframeId = `youtube-player-${createUniqueId()}`;
  const parsed = () => {
    const m = /^(\w+)\s*([\s\S]*)$/.exec(props.body.trim());
    const name = (m?.[1] ?? "video").toLowerCase();
    const arg = (m?.[2] ?? "").trim().replace(/^\[\[|\]\]$/g, "");
    return { name, arg };
  };
  const url = () => parsed().arg;
  const embed = () => {
    const { name, arg } = parsed();
    // `?enablejsapi=1` matches OG (youtube.cljs:58) and, together with the
    // referrerpolicy below, is what makes the embed play under WebKitGTK — a bare
    // src with no referrer is rejected by YouTube's player as error 153.
    const yt = /(?:youtube\.com\/(?:watch\?v=|embed\/)|youtu\.be\/)([\w-]{11})/.exec(arg);
    if (yt) return `https://www.youtube.com/embed/${yt[1]}?enablejsapi=1`;
    if (name === "youtube" && /^[\w-]{11}$/.test(arg)) return `https://www.youtube.com/embed/${arg}?enablejsapi=1`;
    const vimeo = /vimeo\.com\/(\d+)/.exec(arg);
    if (vimeo) return `https://player.vimeo.com/video/${vimeo[1]}`;
    if (name === "vimeo" && /^\d+$/.test(arg)) return `https://player.vimeo.com/video/${arg}`;
    const bili = /bilibili\.com\/video\/(BV[0-9A-Za-z]+)/i.exec(arg);
    const bvid = bili ? bili[1] : name === "bilibili" && /^BV[0-9A-Za-z]+$/.test(arg) ? arg : null;
    if (bvid) return `https://player.bilibili.com/player.html?bvid=${bvid}&high_quality=1`;
    return null;
  };
  // OG parity (og-1.0.0 6e7afa8eb): the embed iframe's `allow`/`referrerpolicy`.
  // YouTube (youtube.cljs:54-70) sends a `strict-origin-when-cross-origin`
  // referrer so the app origin reaches YouTube — without a referrer the player
  // fails with error 153. Vimeo (block.cljs:1290-1305) gets the same `allow` list
  // minus picture-in-picture/web-share and NO referrerpolicy; bilibili sets
  // neither (the `.embed-iframe` class already removes the border).
  const embedAttrs = (): Record<string, string> => {
    const src = embed() ?? "";
    if (/(?:^|\/\/)(?:www\.)?(?:youtube\.com|youtube-nocookie\.com)\/embed\//.test(src))
      return {
        allow: "accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture; web-share",
        referrerpolicy: "strict-origin-when-cross-origin",
      };
    if (src.includes("player.vimeo.com"))
      return { allow: "accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope" };
    return {};
  };
  const isYoutubeEmbed = () => /(?:^|\/\/)(?:www\.)?(?:youtube\.com|youtube-nocookie\.com)\/embed\//.test(embed() ?? "");
  let player: YoutubePlayer | undefined;
  onMount(() => {
    if (!isYoutubeEmbed()) return;
    let mounted = true;
    void loadYoutubeApi().then((api) => {
      if (!mounted || !api) return;
      try {
        const registered = new api.Player(iframeId, { events: { onReady: () => undefined } });
        if (!mounted) {
          registered.destroy?.();
          return;
        }
        player = registered;
        youtubePlayers.set(iframeId, registered);
      } catch {
        // Offline, blocked, or malformed API responses leave the timestamp label usable.
      }
    });
    onCleanup(() => {
      mounted = false;
      if (youtubePlayers.get(iframeId) === player) youtubePlayers.delete(iframeId);
      player?.destroy?.();
    });
  });
  return (
    <Show
      when={embed()}
      fallback={
        <Show
          when={/\.(mp4|webm|ogg)(\?|$)/i.test(url())}
          fallback={<ExternalLink class="external-link" dest={url()} target="_blank" rel="noreferrer">{url()}</ExternalLink>}
        >
          <video class="embed-video" src={url()} controls />
        </Show>
      }
    >
      <div class="embed-iframe-wrap">
        <iframe id={isYoutubeEmbed() ? iframeId : undefined} class="embed-iframe" src={embed()!} allowfullscreen title="video" {...embedAttrs()} />
      </div>
    </Show>
  );
}

// A {{tweet URL}} / {{twitter URL}} macro (`twitter` is OG's alias for `tweet`) —
// rendered as a link (no third-party script embedding).
export function TweetMacro(props: { body: string }): JSX.Element {
  const url = () => props.body.replace(/^(tweet|twitter)\s*/i, "").trim();
  return (
    <ExternalLink class="external-link tweet-link" dest={url()} target="_blank" rel="noreferrer">
      🐦 {url()}
    </ExternalLink>
  );
}

// `{{youtube-timestamp <seconds>}}` seeks the OG-selected on-page YouTube player.
export function YoutubeTimestamp(props: { body: string }): JSX.Element {
  const secs = () => {
    const raw = props.body.replace(/^youtube-timestamp\s*/i, "").trim();
    const n = parseInt(raw, 10);
    return Number.isFinite(n) ? n : 0;
  };
  const label = () => {
    const s = Math.max(0, secs());
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = s % 60;
    const pad = (x: number) => String(x).padStart(2, "0");
    return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${m}:${pad(sec)}`;
  };
  return (
    <a
      class="youtube-ts"
      title="Seek the preceding YouTube video"
      onClick={(event) => {
        event.preventDefault();
        event.stopPropagation();
        // OG timestamp click stops the event and calls seekTo(seconds, true)
        // on get-player's result (og-1.0.0 6e7afa8eb,
        // extensions/video/youtube.cljs:103-111).
        youtubePlayerForTarget(event.currentTarget)?.seekTo(secs(), true);
      }}
    >
      ⏱ {label()}
    </a>
  );
}

// `{{cloze answer}}` (optionally `{{cloze answer\\cue}}`) — in OG this is hidden
// only inside the SRS flashcard-review loop. Tine has no SRS engine, so we degrade
// to a click-to-reveal: shows the cue (or `[...]`) until clicked, then the answer.
export function ClozeMacro(props: { body: string }): JSX.Element {
  const [revealed, setRevealed] = createSignal(false);
  const parts = () => props.body.replace(/^cloze\s*/i, "").trim().split(/\\\\/);
  const answer = () => (parts()[0] ?? "").trim();
  const cue = () => parts()[1]?.trim();
  return (
    <span
      class="cloze"
      classList={{ revealed: revealed() }}
      title={revealed() ? "Click to hide" : "Click to reveal"}
      onClick={(e) => {
        e.stopPropagation();
        setRevealed((v) => !v);
      }}
    >
      {revealed() ? answer() : (cue() ?? "[...]")}
    </span>
  );
}

// `{{zotero-imported-file ...}}` / `{{zotero-linked-file ...}}` — OG resolves the
// Zotero item-key to a real attachment via its Zotero connector (data dir + item
// metadata + storage config). Tine has no Zotero integration, so resolving would
// yield a dead link; we degrade to a muted, non-navigating label rather than a
// broken link. Flagged as a known parity gap (niche).
export function ZoteroMacro(props: { body: string }): JSX.Element {
  const arg = () => props.body.replace(/^zotero-(imported|linked)-file\s*/i, "").trim();
  return (
    <span class="zotero-ref" title="Zotero integration isn't supported in Tine">
      📎 {arg() || "Zotero attachment"}
    </span>
  );
}

// A {{embed ((uuid))}} or {{embed [[Page]]}} block.
export function EmbedMacro(props: { body: string; blockId?: string }): JSX.Element {
  const linkDepth = useContext(LinkDepthContext);
  if (linkDepth > MAX_DEPTH_OF_LINKS) return <LinkDepthWarning />;

  const target = () => props.body.replace(/^embed\s*/i, "").trim();
  const pageTarget = () => /^\[\[([^\]]+)\]\]$/.exec(target())?.[1];
  const selfPageEmbed = () => {
    const sourcePage = props.blockId ? doc.byId[props.blockId]?.page : undefined;
    const targetPage = pageTarget();
    return !!sourcePage
      && pageByName(sourcePage)?.kind === "page"
      && !!targetPage
      && pageIdentityKey(sourcePage) === pageIdentityKey(targetPage);
  };

  const [data] = createResource(
    () => selfPageEmbed() ? null : `${target()} ${graphEpoch()} ${dataRev()}`,
    async () => {
    const t = target();
    const blockRef = /^\(\(([^)]+)\)\)$/.exec(t);
    if (blockRef) {
      const g = await resolveBlockBatched(blockRef[1]);
      // embedId = the embedded block's own id, so its ref-count badge is hidden
      // inside the embed (OG hide-block-refs-count?); its children keep theirs.
      return g ? { page: g.page, kind: g.kind, blocks: g.blocks, embedId: g.blocks[0]?.id } : null;
    }
    const pageRef = /^\[\[([^\]]+)\]\]$/.exec(t);
    if (pageRef) {
      // Backend miss → the virtual in-app Guide, matched by bare title (the embed
      // carries no source context to remap the name). No-op for real graphs.
      const p = (await backend().getPage(pageRef[1], "page")) ?? resolveGuidePageDto(pageRef[1]);
      return p ? { page: p.name, kind: "page" as PageKind, blocks: p.blocks, embedId: undefined } : null;
    }
    return null;
  });

  return (
    <div class="embed-block">
      <Show when={!selfPageEmbed()}>
        <Show when={data()} fallback={<div class="embed-missing">{`{{${props.body}}}`}</div>}>
          <LiveRefGroup
            page={data()!.page}
            kind={data()!.kind}
            blocks={data()!.blocks}
            embedId={data()!.embedId}
            hostBlockId={props.blockId}
            surface="embed"
          />
        </Show>
      </Show>
    </div>
  );
}
