import {
  For,
  Match,
  Show,
  Switch,
  createEffect,
  createMemo,
  createSignal,
  createUniqueId,
  onCleanup,
  type JSX,
} from "solid-js";
import { backend } from "../backend";
import type {
  GraphSearchDisplayOptions,
  QueryResult,
  QueryStatistics,
  ViewSettings,
} from "../editor/queryIr";
import { querySummary } from "../editor/queryAggregate";
import { QUERY_MACRO_NAMES } from "../editor/queryMacroName";
import {
  friendlySearchToDsl,
  friendlySearchToSavedDsl,
  parseSearchQuery,
} from "../editor/searchQuery";
import type { PaneRouter, QueryPresentation, QueryRoute } from "../router";
import type {
  AdvancedQueryResult,
  Format,
  PageDto,
  QueryDiagnostic,
  QueryExecution,
  QueryExplainNode,
  QueryHit,
  RefGroup,
  SavePageResult,
} from "../types";
import { QueryBuilder, createQueryRegistryAccess, type BuilderSession } from "./QueryBuilder";
import { QueryDisplay } from "./QueryDisplay";
import { SearchResultRow } from "./SearchResultRow";
import { registerTransientLayer } from "../transientLayers";
import { bumpPageInventoryRev, graphMeta } from "../ui";
import { blockDtoExternalId } from "../blockIdentity";
import { isSaveConflictFailure } from "../persistence";
import { captureGraphScope, isScopeCurrent, type GraphScope } from "../landAsync";
import { createReadyQueryResource } from "../createReadyQueryResource";
import { runQueryWhenReady } from "../queryReadiness";
import { onGraphRebound } from "../modeHooks";
import { markdownRawWithProperty, orgRawWithProperty } from "../editor/properties";
import {
  queryPageMatchScopePropertyPatch,
  queryScopedDisplayPropertyPatch,
  queryViewPropertyPatch,
} from "../editor/queryViewProperties";
import {
  queryResultDisplaySettings,
  type FriendlyPageMatchScope,
  type QueryDisplayDraft,
} from "../editor/queryDisplayDraft";
import { QueryResultSections } from "./QueryResultSections";
import { QueryPageResults, MarkedText, hitMatchSpans, type QueryPageHit } from "./QueryPageResults";

const PAGE_LIMIT = 40;
const BLOCK_LIMIT = 100;
const ADVANCED_QUERY_RE = /^\s*\[:/;

export interface MaterializeQueryInput {
  title: string;
  sourceKind: QueryRoute["sourceKind"];
  source: string;
  presentation: QueryPresentation;
  /** The singular non-view settings the workspace is showing. Deep-copied at
   *  capture, so a draft the user keeps editing during the await cannot reach
   *  back into the attempt that is already publishing. */
  display?: QueryDisplayDraft;
  /** The two scoped namespaces. ABSENT means "no scoped draft, inherit"; a
   *  present empty object means "clear". Presence is carried by `Object.hasOwn`
   *  the whole way through, never by truthiness. */
  pagePresentation?: QueryPresentation;
  blockPresentation?: QueryPresentation;
  pageDisplay?: QueryDisplayDraft;
  blockDisplay?: QueryDisplayDraft;
  /** Friendly page membership scope, written to its own property. */
  pageMatchScope?: FriendlyPageMatchScope;
  /** Stable workspace identity: also bounds the native validation cancellation lane. */
  routeId: string;
  /** The graph's preferred on-disk format, captured at submit. A property line
   *  belongs in a different place in each format, so the format has to travel
   *  with the input rather than be read at completion time. Absent means `md`,
   *  which keeps every dependency-injected caller on the behavior it had. */
  format?: Format;
}

/** Is the state this attempt captured still the state the user is looking at?
 *
 *  Supplied by the component that owns the workspace; absent for a direct
 *  caller, which is then unguarded exactly as before. */
export type IsCurrentInput = () => boolean;

export interface MaterializeQueryDependencies {
  getPage(name: string, kind: "page"): Promise<PageDto | null>;
  savePage(page: PageDto, baseRev: null, force: false): Promise<SavePageResult>;
  /** Rust-authoritative friendly-search validation; required before every nonblank friendly save.
   *  `options` carries page membership scope and the two already-resolved
   *  per-kind views; omitting it is the request this dependency has always made. */
  runGraphSearch(
    source: string,
    pageLimit: number,
    blockLimit: number,
    lane: string,
    explain: boolean,
    options?: GraphSearchDisplayOptions,
  ): Promise<QueryExecution>;
}

export type MaterializeQueryResult =
  | { ok: true; name: string; page: PageDto; rev: string }
  | {
      ok: false;
      /** `superseded` is a LOCAL refusal: nothing was written, nothing was
       *  undone, and the user is asked to save again. */
      kind: "invalid-name" | "empty-query" | "invalid-query" | "exists" | "conflict" | "error" | "superseded";
      message: string;
    };

/** The one wording for a local pre-save refusal, so every stale lane says the
 *  same true thing: no write happened, and saving again is the whole remedy. */
const SUPERSEDED_MESSAGE =
  "This workspace changed while it was being saved, so nothing was written. Try saving again.";

export interface QueryWorkspaceDependencies extends MaterializeQueryDependencies {
  /** Run an explicit query through the typed IR route.
   *
   *  `views` carries BOTH effective section views because the anchor is the
   *  parse's answer, not the caller's: a page-anchored query is a Pages section
   *  and a block-anchored one is a Blocks section, and only the parse knows
   *  which. Sending one view and hoping is how a page query ends up ordered by
   *  a block field. */
  runQuery(
    source: string,
    views?: { page: ViewSettings; block: ViewSettings },
  ): Promise<RefGroup[] | QueryResult>;
  runAdvancedQuery(source: string): Promise<AdvancedQueryResult>;
}

export interface QueryWorkspaceProps {
  route: QueryRoute;
  router: PaneRouter;
  /** Dependency injection keeps create/save races and rendering testable without IPC. */
  deps?: QueryWorkspaceDependencies;
  focusSource?: boolean;
}

function savedQueryRaw(
  input: Pick<
    MaterializeQueryInput,
    "source" | "sourceKind" | "presentation" | "format" | "display"
    | "pagePresentation" | "blockPresentation" | "pageDisplay" | "blockDisplay" | "pageMatchScope"
  >
): string {
  const source = input.source.trim();
  const dsl = input.sourceKind === "search" ? friendlySearchToSavedDsl(source) : source;
  // §7.9: the macro name comes from the shared list. The workspace always
  // materializes OG DSL text (`friendlySearchToSavedDsl` and the builder both
  // produce it), so it writes the legacy spelling — but it writes it by NAME,
  // not by spelling it inline, so promoting a workspace to TQL later is one
  // change here rather than a grep across the app.
  const query = `{{${QUERY_MACRO_NAMES[0]} ${dsl}}}`;
  // WHAT to write is `queryViewPropertyPatch`'s answer — the same view→property
  // map every other query save runs through (§7.6), never a second serializer.
  // An absent `tine.view` IS the default list view (the patch spells it that
  // way too), so a list workspace still materializes a bare query block.
  //
  // The COMPLETE envelope, through the existing writers (Q3). The old
  // `tine.view`-only filter dropped every other setting the workspace was
  // showing, so a saved query reopened as a different query than the one that
  // was saved — the singular sort, grouping, columns and sample simply were not
  // written down anywhere.
  //
  // Each namespace goes through the writer that owns it. `queryViewPropertyPatch`
  // states the singular compatibility view; `queryScopedDisplayPropertyPatch`
  // states one scoped namespace without touching the other or any authored key
  // it does not own; `queryPageMatchScopePropertyPatch` states membership scope,
  // which belongs to neither namespace.
  const writes: (readonly [string, string | null])[] = [
    ...queryViewPropertyPatch({
      view: {
        ...(input.display ?? {}),
        ...(input.presentation === "list" ? {} : { view: input.presentation }),
      },
      properties: [],
    }),
    ...queryScopedDisplayPropertyPatch({
      namespace: "page",
      ...(input.pagePresentation !== undefined && input.pagePresentation !== "list"
        ? { presentation: input.pagePresentation } : {}),
      ...(Object.hasOwn(input, "pageDisplay") ? { display: input.pageDisplay ?? {} } : {}),
      properties: [],
    }),
    ...queryScopedDisplayPropertyPatch({
      namespace: "block",
      ...(input.blockPresentation !== undefined && input.blockPresentation !== "list"
        ? { presentation: input.blockPresentation } : {}),
      ...(Object.hasOwn(input, "blockDisplay") ? { display: input.blockDisplay ?? {} } : {}),
      properties: [],
    }),
    ...queryPageMatchScopePropertyPatch({
      ...(input.pageMatchScope !== undefined ? { scope: input.pageMatchScope } : {}),
      properties: [],
    }),
  ];
  // WHERE it goes is the format's own rule, and the two pure writers the store
  // already uses are the rule. Writing markdown `key:: value` into an org file
  // produces visible body text that is never read back as a property (GH #25).
  const withProperty = input.format === "org" ? orgRawWithProperty : markdownRawWithProperty;
  return writes.reduce((raw, [key, value]) => withProperty(raw, key, value), query);
}

/**
 * Materialize a virtual workspace as exactly one ordinary query block.
 *
 * The preflight existence check provides a friendly error. The authoritative
 * race guard for CONTENT is the audited no-baseline save (`null`, never force):
 * if another writer creates the page between the two calls, the backend rejects
 * it as a conflict and this workspace remains virtual. A title lookup is only a
 * friendly preflight; the save conflict stays the final authority.
 *
 * `isCurrent` is the separate, LOCAL guard: it answers "is the input this
 * attempt captured still the one the user is looking at?". It is checked before
 * any work, after each await that can outlive an edit (Rust validation, the
 * title lookup) and immediately before the write — so a save the user has
 * already moved on from refuses locally instead of publishing an obsolete
 * draft. Once `savePage` has begun there is no going back: the page may
 * legitimately commit, and this function reports that honestly rather than
 * pretending it was undone.
 */
export async function materializeQueryWorkspace(
  input: MaterializeQueryInput,
  deps: MaterializeQueryDependencies,
  isCurrent: IsCurrentInput = () => true,
  signal: AbortSignal = new AbortController().signal,
): Promise<MaterializeQueryResult> {
  input = { ...input };
  const superseded = (): MaterializeQueryResult =>
    ({ ok: false, kind: "superseded", message: SUPERSEDED_MESSAGE });
  if (!isCurrent()) return superseded();
  const name = input.title.trim();
  if (!name) {
    return { ok: false, kind: "invalid-name", message: "Enter a page title before saving." };
  }
  if (!input.source.trim()) {
    return { ok: false, kind: "empty-query", message: "Enter a search or query before saving." };
  }
  if (input.sourceKind === "search") {
    try {
      const execution = await runQueryWhenReady(
        () => deps.runGraphSearch(input.source.trim(), 0, 0, `query-workspace:${input.routeId}:materialize`, true),
        { signal, isCurrent, onPending: () => {} },
      );
      if (!isCurrent()) return superseded();
      if (execution.cancelled) return { ok: false, kind: "invalid-query", message: "Search validation was superseded. Try saving again." };
      if (execution.diagnostics.length) return { ok: false, kind: "invalid-query", message: execution.diagnostics.map((item) => item.message).join(" · ") };
      if (!execution.explanation.branches.length) return { ok: false, kind: "empty-query", message: "Enter a search with at least one included term before saving." };
    } catch (error) {
      if (!isCurrent()) return superseded();
      const detail = error instanceof Error ? error.message : String(error);
      return { ok: false, kind: "invalid-query", message: detail ? `Could not validate this search: ${detail}` : "Could not validate this search." };
    }
    // Rust answered about the source that was submitted. If the workspace has
    // moved on since, that answer no longer licenses a write.
    if (!isCurrent()) return superseded();
  }

  try {
    const existing = await deps.getPage(name, "page");
    if (!isCurrent()) return superseded();
    if (existing) {
      return {
        ok: false,
        kind: "exists",
        message: `A page named “${name}” already exists. Choose another title.`,
      };
    }

    // The lookup is an await too, and the last one before the graph is written.
    if (!isCurrent()) return superseded();

    const page: PageDto = {
      name,
      kind: "page",
      title: name,
      pre_block: null,
      // The format the block was WRITTEN for travels with the page, so the
      // backend stores it in the same dialect the property placement assumed.
      format: input.format ?? "md",
      blocks: [{
        id: "",
        raw: savedQueryRaw(input),
        collapsed: false,
        children: [],
      }],
    };
    const saved = await deps.savePage(page, null, false);
    const rev = saved.revision;
    bumpPageInventoryRev();
    return { ok: true, name, page, rev };
  } catch (error) {
    if (!isCurrent()) return superseded();
    const detail = error instanceof Error ? error.message : String(error);
    if (isSaveConflictFailure(error)) {
      return {
        ok: false,
        kind: "conflict",
        message: `“${name}” was created or changed before this workspace could be saved. It has not been overwritten.`,
      };
    }
    return {
      ok: false,
      kind: "error",
      message: detail ? `Could not save “${name}”: ${detail}` : `Could not save “${name}”.`,
    };
  }
}

function defaultDependencies(): QueryWorkspaceDependencies {
  const api = backend();
  return {
    getPage: (name, kind) => api.getPage(name, kind),
    savePage: (page, baseRev, force) => api.savePage(page, baseRev, force),
    runGraphSearch: (source, pageLimit, blockLimit, lane, explain, options) =>
      api.runGraphSearch(source, pageLimit, blockLimit, lane, explain, undefined, options),
    runQuery: async (source, views) => {
      const parsed = await api.parseQuery(source, "og");
      // The effective view of the anchor this query DECLARES (§7.6, Q3). With
      // no views supplied the parse's own settings stand, exactly as before.
      const effective = views?.[parsed.query.anchor];
      return api.queryRun(parsed.query, effective ?? parsed.view);
    },
    runAdvancedQuery: (source) => api.runAdvancedQuery(source),
  };
}

function diagnosticsFromAdvanced(result: AdvancedQueryResult): QueryDiagnostic[] {
  const diagnostics = result.ignored.map((clause) => ({
    code: "unsupported_clause",
    message: `This query clause is not supported yet: ${clause}`,
  }));
  if (!result.supported && !diagnostics.length) {
    diagnostics.push({
      code: "unsupported_query",
      message: "This advanced query has no supported clauses yet.",
    });
  }
  return diagnostics;
}

function groupsToExecution(
  groups: RefGroup[],
  explain: boolean,
  diagnostics: QueryDiagnostic[] = []
): QueryExecution {
  const hits: QueryHit[] = groups.flatMap((group) => group.blocks.map((block) => ({
    entity: "block" as const,
    page: group.page,
    kind: group.kind,
    block,
    display_text: block.raw,
    evidence: [],
  })));
  return {
    hits,
    diagnostics,
    cancelled: false,
    explanation: {
      branches: explain ? [{
        description: `Query DSL selected ${hits.length} block${hits.length === 1 ? "" : "s"} on ${groups.length} page${groups.length === 1 ? "" : "s"}.`,
        children: [],
      }] : [],
    },
  };
}

function hitPage(hit: QueryHit): string {
  return hit.entity === "page" ? hit.page.name : hit.page;
}

function hitKind(hit: QueryHit): "Page" | "Block" {
  return hit.entity === "page" ? "Page" : "Block";
}


function ExplainTree(props: { nodes: QueryExplainNode[] }): JSX.Element {
  return (
    <ul class="query-explain-tree">
      <For each={props.nodes}>{(node) => (
        <li>
          <span>{node.description}</span>
          <Show when={node.children.length}>
            <ExplainTree nodes={node.children} />
          </Show>
        </li>
      )}</For>
    </ul>
  );
}

function friendlySummary(source: string): string {
  const parsed = parseSearchQuery(source);
  if (parsed.kind === "empty") return "Type to search page names and block text.";
  if (parsed.kind === "invalid") return `The regular expression is invalid: ${parsed.error}`;
  if (parsed.kind === "regex") return `Matches page names or block text using the case-sensitive regular expression /${parsed.re.source}/.`;
  const describeGroup = (group: typeof parsed.groups[number]) => group.map((term) => {
    const value = term.quoted ? `the exact phrase “${term.text}”` : `“${term.text}”`;
    return term.negated ? `excluding ${value}` : `containing ${value}`;
  }).join(" and ");
  const groups = parsed.groups.map(describeGroup);
  return groups.length === 1
    ? `Matches page names or block text ${groups[0]}.`
    : `Matches page names or block text when it is ${groups.join("; or ")}.`;
}

function filterWords(value: string): string[] {
  return value.trim().split(/\s+/).filter(Boolean);
}

interface FriendlyFields {
  all: string;
  any: string;
  exact: string;
  exclude: string;
  regex: string;
}

function buildFriendlyFilterSource(fields: FriendlyFields): { source: string; error: string | null } {
  const regex = fields.regex.trim();
  const hasOther = [fields.all, fields.any, fields.exact, fields.exclude].some((value) => value.trim());
  if (regex) {
    if (hasOther) {
      return { source: "", error: "A regular expression cannot be combined with the other friendly fields yet." };
    }
    try {
      new RegExp(regex);
      return { source: `/${regex}/`, error: null };
    } catch (error) {
      return { source: "", error: error instanceof Error ? error.message : "Invalid regular expression." };
    }
  }

  if ([fields.all, fields.any, fields.exact, fields.exclude].some((value) => value.includes('"'))) {
    return { source: "", error: "Quotation marks are not supported inside these fields." };
  }
  const common = [
    ...filterWords(fields.all),
    ...(fields.exact.trim() ? [`"${fields.exact.trim()}"`] : []),
    ...filterWords(fields.exclude).map((term) => `-${term}`),
  ];
  const alternatives = filterWords(fields.any);
  if (!common.some((term) => !term.startsWith("-")) && !alternatives.length) {
    return { source: "", error: "Add at least one word or exact phrase to include." };
  }
  const branches = alternatives.length
    ? alternatives.map((term) => [...common, term].join(" "))
    : [common.join(" ")];
  return { source: branches.join(" OR "), error: null };
}

/** Split the friendly grammar into Gmail-like fields only when that is lossless. */
function friendlyFieldsFromSource(source: string): FriendlyFields | null {
  const parsed = parseSearchQuery(source);
  const empty: FriendlyFields = { all: "", any: "", exact: "", exclude: "", regex: "" };
  if (parsed.kind === "empty") return empty;
  if (parsed.kind === "regex") return { ...empty, regex: parsed.re.source };
  if (parsed.kind !== "boolean") return null;

  const termKey = (term: typeof parsed.groups[number][number]) =>
    `${term.negated ? "-" : "+"}\0${term.quoted ? "q" : "w"}\0${term.text}`;
  const commonKeys = new Set(parsed.groups[0].map(termKey));
  for (const group of parsed.groups.slice(1)) {
    const keys = new Set(group.map(termKey));
    for (const key of [...commonKeys]) if (!keys.has(key)) commonKeys.delete(key);
  }
  const common = parsed.groups[0].filter((term) => commonKeys.has(termKey(term)));
  const remainder = parsed.groups.map((group) => group.filter((term) => !commonKeys.has(termKey(term))));
  const alternatives = remainder.every((group) => group.length === 0)
    ? []
    : remainder.every((group) => group.length === 1 && !group[0].negated && !group[0].quoted)
      ? remainder.map((group) => group[0].text)
      : null;
  const exact = common.filter((term) => !term.negated && term.quoted);
  if (alternatives === null || exact.length > 1 || common.some((term) => term.negated && term.quoted)) {
    return null;
  }
  return {
    all: common.filter((term) => !term.negated && !term.quoted).map((term) => term.text).join(" "),
    any: alternatives.join(" "),
    exact: exact[0]?.text ?? "",
    exclude: common.filter((term) => term.negated).map((term) => term.text).join(" "),
    regex: "",
  };
}

function focusableElements(root: HTMLElement): HTMLElement[] {
  return [...root.querySelectorAll<HTMLElement>(
    'button:not([disabled]), input:not([disabled]), textarea:not([disabled]), select:not([disabled]), summary, [href], [tabindex]:not([tabindex="-1"])'
  )].filter((element) => !element.hasAttribute("hidden") && !element.closest("details:not([open])"));
}

function AdvancedModal(props: {
  source: () => string;
  sourceKind: () => QueryRoute["sourceKind"];
  onApply: (source: string, sourceKind: QueryRoute["sourceKind"]) => void;
  onClose: () => void;
  layerId: string;
  trigger: () => HTMLElement | null;
}): JSX.Element {
  const initialFields = props.sourceKind() === "search" ? friendlyFieldsFromSource(props.source()) : null;
  const [all, setAll] = createSignal(initialFields?.all ?? "");
  const [any, setAny] = createSignal(initialFields?.any ?? "");
  const [exact, setExact] = createSignal(initialFields?.exact ?? "");
  const [exclude, setExclude] = createSignal(initialFields?.exclude ?? "");
  const [regex, setRegex] = createSignal(initialFields?.regex ?? "");
  const [rawFriendly, setRawFriendly] = createSignal(props.sourceKind() === "search" ? props.source() : "");
  const [structuredFriendly] = createSignal(initialFields !== null);
  const [friendlyDirty, setFriendlyDirty] = createSignal(false);
  const [draftKind, setDraftKind] = createSignal<QueryRoute["sourceKind"]>(props.sourceKind());
  const [dsl, setDsl] = createSignal(props.sourceKind() === "dsl" ? props.source() : "");
  const [error, setError] = createSignal<string | null>(null);
  let dialog!: HTMLDivElement;
  let firstField: HTMLElement | undefined;

  // **The workspace's draft is OG DSL text, and the engine is what reads and
  // writes it** (§7.1, I-12). The builder edits the IR; this pair is the one
  // boundary between that IR and the text the workspace materializes
  // (`savedQueryRaw` writes `{{query <dsl>}}`). The frontend does not parse or
  // print here — it asks.
  // RET2-Direct: `query_parse` can report typed readiness, so this pair takes
  // the shared readiness owner and its binding/epoch cancellation, exactly as
  // the workspace's execution resource does.
  const [builderSession] = createReadyQueryResource(dsl, async (text): Promise<BuilderSession> => {
    const parsed = await backend().parseQuery(text, "og");
    return { query: parsed.query, view: parsed.view };
  });
  const applyBuilderEdit = async (next: BuilderSession) => {
    try {
      setDsl(await backend().printQuery(next.query, next.view, "og"));
      setError(null);
    } catch (failure) {
      // A workspace materializes an OG `{{query …}}` block, so an edit the OG
      // syntax cannot say has nowhere to go here. The printer's own message says
      // which part (I-9); the draft is left exactly as it was rather than saved
      // as something else.
      setError(failure instanceof Error ? failure.message : String(failure));
    }
  };

  createEffect(() => {
    const unregister = registerTransientLayer({
      id: props.layerId,
      root: () => dialog ?? null,
      trigger: props.trigger,
      dismiss: () => { props.onClose(); return true; },
    });
    onCleanup(unregister);
  });

  queueMicrotask(() => (firstField ?? dialog)?.focus());

  const apply = () => {
    if (draftKind() === "dsl") {
      if (!dsl().trim()) {
        setError("The query DSL cannot be empty.");
        return;
      }
      props.onApply(dsl().trim(), "dsl");
      return;
    }
    const rawValidation = friendlySearchToDsl(rawFriendly());
    const built = !friendlyDirty()
      ? { source: props.source().trim(), error: friendlySearchToDsl(props.source()).error }
      : structuredFriendly()
        ? buildFriendlyFilterSource({ all: all(), any: any(), exact: exact(), exclude: exclude(), regex: regex() })
        : rawValidation.error
          ? { source: "", error: rawValidation.error }
          : { source: rawFriendly().trim(), error: null };
    if (built.error) {
      setError(built.error);
      return;
    }
    props.onApply(built.source, "search");
  };

  const switchToDsl = () => {
    const rawValidation = friendlySearchToDsl(rawFriendly());
    const friendly = !friendlyDirty()
      ? { source: props.source().trim(), error: friendlySearchToDsl(props.source()).error }
      : structuredFriendly()
        ? buildFriendlyFilterSource({ all: all(), any: any(), exact: exact(), exclude: exclude(), regex: regex() })
        : { source: rawFriendly().trim(), error: rawValidation.error };
    if (friendly.error) {
      setError(friendly.error);
      return;
    }
    const converted = friendlySearchToDsl(friendly.source);
    if (converted.error) {
      setError(converted.error);
      return;
    }
    setDsl(converted.dsl);
    setDraftKind("dsl");
    setError(null);
  };

  return (
    <div class="modal-overlay query-advanced-overlay" onMouseDown={(event) => {
      if (event.target === event.currentTarget) props.onClose();
    }}>
      <div
        ref={dialog}
        class="modal query-advanced-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="query-advanced-title"
        tabIndex={-1}
        onKeyDown={(event) => {
          if (event.key === "Tab") {
            const focusable = focusableElements(dialog);
            const first = focusable[0];
            const last = focusable[focusable.length - 1];
            if (!first || !last) return;
            if (event.shiftKey && document.activeElement === first) {
              event.preventDefault();
              last.focus();
            } else if (!event.shiftKey && document.activeElement === last) {
              event.preventDefault();
              first.focus();
            }
          }
        }}
      >
        <header class="query-advanced-header">
          <div>
            <h2 id="query-advanced-title">Filters and advanced query</h2>
            <p>Use the friendly fields, or switch losslessly to the visual query builder.</p>
          </div>
          <button type="button" aria-label="Close filters" onClick={props.onClose}>×</button>
        </header>

        <Show when={draftKind() === "search"} fallback={
          <div class="query-dsl-editor">
            <QueryBuilder
              session={() => builderSession.latest}
              onChange={(next) => void applyBuilderEdit(next)}
              paneDialect="og"
              sheetAlwaysOpen
              parentTransientId={props.layerId}
            />
            <p class="query-advanced-note">Switching back to friendly fields is offered only when it can be lossless.</p>
          </div>
        }>
          <Show when={structuredFriendly()} fallback={
            <div class="query-friendly-raw">
              <p>This search uses a combination that cannot be split into fields without changing it.</p>
              <label>
                Friendly search syntax
                <textarea
                  ref={(element) => { firstField = element; }}
                  rows={4}
                  value={rawFriendly()}
                  onInput={(event) => { setRawFriendly(event.currentTarget.value); setFriendlyDirty(true); setError(null); }}
                  spellcheck={false}
                />
              </label>
              <button type="button" class="query-switch-to-dsl" onClick={switchToDsl}>
                Edit as visual query
              </button>
            </div>
          }>
          <div class="query-friendly-fields">
            <label>
              All of these words
              <input ref={(element) => { firstField = element; }} value={all()} onInput={(event) => { setAll(event.currentTarget.value); setFriendlyDirty(true); }} />
            </label>
            <label>
              Any of these words
              <input value={any()} onInput={(event) => { setAny(event.currentTarget.value); setFriendlyDirty(true); }} />
            </label>
            <label>
              This exact phrase
              <input value={exact()} onInput={(event) => { setExact(event.currentTarget.value); setFriendlyDirty(true); }} />
            </label>
            <label>
              Exclude these words
              <input value={exclude()} onInput={(event) => { setExclude(event.currentTarget.value); setFriendlyDirty(true); }} />
            </label>
            <label>
              Case-sensitive regular expression
              <input value={regex()} onInput={(event) => { setRegex(event.currentTarget.value); setFriendlyDirty(true); }} placeholder="pattern without / /" />
            </label>
            <button type="button" class="query-switch-to-dsl" onClick={switchToDsl}>
              Edit as visual query
            </button>
          </div>
          </Show>
        </Show>

        <Show when={error()}>
          <p class="query-advanced-error" role="alert">{error()}</p>
        </Show>
        <footer class="query-advanced-actions">
          <button type="button" onClick={props.onClose}>Cancel</button>
          <button type="button" class="primary" onClick={apply}>Apply</button>
        </footer>
      </div>
    </div>
  );
}

/** Everything one save attempt publishes, frozen at submit.
 *
 *  A workspace edit replaces the whole route object and every local signal, so
 *  an attempt that read them at completion time would publish (and route to)
 *  whatever the user happened to be looking at by then. */
interface CapturedSave {
  token: number;
  inputRevision: number;
  routeId: string;
  title: string;
  source: string;
  sourceKind: QueryRoute["sourceKind"];
  presentation: QueryPresentation;
  /** The complete Display state this attempt publishes, DEEP-COPIED. The user
   *  can keep editing a draft while the write is in flight; an attempt that
   *  shared the route's arrays would publish whatever the draft became. Serialized
   *  once so the same string is both the copy and the comparison (§"Save,
   *  reopen and operation lifetime"). */
  settings: string;
  format: Format;
  scope: GraphScope | null;
}

/** The captured Display envelope, as one comparable value. Presence is
 *  preserved: a key that is absent from the route is absent here, and a present
 *  empty draft survives as `{}`. */
function captureSettings(route: QueryRoute): string {
  return JSON.stringify({
    presentation: route.presentation,
    ...(Object.hasOwn(route, "display") ? { display: route.display ?? null } : {}),
    ...(route.pagePresentation !== undefined ? { pagePresentation: route.pagePresentation } : {}),
    ...(route.blockPresentation !== undefined ? { blockPresentation: route.blockPresentation } : {}),
    ...(Object.hasOwn(route, "pageDisplay") ? { pageDisplay: route.pageDisplay ?? {} } : {}),
    ...(Object.hasOwn(route, "blockDisplay") ? { blockDisplay: route.blockDisplay ?? {} } : {}),
    ...(route.pageMatchScope !== undefined ? { pageMatchScope: route.pageMatchScope } : {}),
  });
}

/** …and back, as the members `materializeQueryWorkspace` takes. */
function settingsInput(captured: string): Partial<MaterializeQueryInput> {
  const parsed = JSON.parse(captured) as Record<string, unknown>;
  const out: Partial<MaterializeQueryInput> = {};
  if (Object.hasOwn(parsed, "display")) {
    out.display = (parsed.display ?? undefined) as QueryDisplayDraft | undefined;
  }
  if (Object.hasOwn(parsed, "pagePresentation")) out.pagePresentation = parsed.pagePresentation as QueryPresentation;
  if (Object.hasOwn(parsed, "blockPresentation")) out.blockPresentation = parsed.blockPresentation as QueryPresentation;
  if (Object.hasOwn(parsed, "pageDisplay")) out.pageDisplay = parsed.pageDisplay as QueryDisplayDraft;
  if (Object.hasOwn(parsed, "blockDisplay")) out.blockDisplay = parsed.blockDisplay as QueryDisplayDraft;
  if (Object.hasOwn(parsed, "pageMatchScope")) out.pageMatchScope = parsed.pageMatchScope as FriendlyPageMatchScope;
  return out;
}

export function QueryWorkspace(props: QueryWorkspaceProps): JSX.Element {
  const deps = () => props.deps ?? defaultDependencies();
  const [source, setSource] = createSignal(props.route.source);
  const [sourceKind, setSourceKind] = createSignal(props.route.sourceKind);
  const [presentation, setPresentation] = createSignal(props.route.presentation);
  const [explain, setExplain] = createSignal(false);
  const [advancedOpen, setAdvancedOpen] = createSignal(false);
  const [title, setTitle] = createSignal("");
  const [saveError, setSaveError] = createSignal<string | null>(null);
  /** A stale save that COMMITTED. Not an error: the page exists, and saying so
   *  is the only honest thing left once the write has landed. */
  const [saveNotice, setSaveNotice] = createSignal<string | null>(null);
  const [saving, setSaving] = createSignal(false);
  // A save is asynchronous, and the workspace under it is not frozen: the user
  // can retype the search, switch the view, rename it, change tab or switch
  // graph while validation, the title lookup or the write is still in flight.
  // These two are what every completion has to get past before it may touch
  // anything — routing, error text, the notice, and `saving` itself.
  let alive = true;
  let saveToken = 0;
  let saveController: AbortController | undefined;
  // A changed-and-restored value is still a newer edit. Include the route
  // object so its non-presentation Display draft participates in the revision.
  const inputRevision = createMemo((previous: number) => {
    // The whole route, so every scoped draft, presentation and the membership
    // scope participate: a saved query has to reopen as the query that was
    // saved, and a setting that did not bump this would publish the previous one.
    props.route;
    source();
    sourceKind();
    presentation();
    title();
    graphMeta()?.preferred_format;
    return previous + 1;
  }, 0);
  createEffect(() => {
    inputRevision();
    saveController?.abort();
  });
  onCleanup(onGraphRebound(() => saveController?.abort()));
  onCleanup(() => { alive = false; saveController?.abort(); });
  let advancedButton!: HTMLButtonElement;
  const advancedLayerId = `query-advanced-${createUniqueId()}`;
  let sourceInput: HTMLInputElement | undefined;
  // Props replace the whole route object on source/presentation edits. Keep an
  // explicit identity latch so that replacement cannot re-run the focus work.
  let lastFocusRouteId: string | undefined;
  let previouslyFocused = false;

  createEffect(() => {
    props.route.id;
    setSource(props.route.source);
    setSourceKind(props.route.sourceKind);
    setPresentation(props.route.presentation);
  });
  createEffect(() => {
    const routeId = props.route.id;
    const focusSource = !!props.focusSource;
    const shouldFocus = focusSource && (routeId !== lastFocusRouteId || !previouslyFocused);
    lastFocusRouteId = routeId;
    previouslyFocused = focusSource;
    if (!shouldFocus) return;
    queueMicrotask(() => {
      if (!props.router.route) return;
      const active = props.router.route();
      if (props.focusSource && active.kind === "query" && active.id === routeId) sourceInput?.focus();
    });
  });

  /** **The two effective views, resolved once, for everything** (I-12).
   *
   *  Request identity, the execution arguments, the Display controls and the
   *  rendered rows all read THESE — so a section cannot be executed under one
   *  set of settings and rendered under another. */
  /** The route, with the presentation the user has actually picked. The local
   *  signal leads the route by one round-trip — `updatePresentation` sets it and
   *  then tells the router — and reading the route alone would render both
   *  sections under the PREVIOUS presentation until the route came back. */
  const settingsRoute = createMemo(() => ({ ...props.route, presentation: presentation() }));
  const pageView = createMemo(() => queryResultDisplaySettings(settingsRoute(), undefined, "page"));
  const blockView = createMemo(() => queryResultDisplaySettings(settingsRoute(), undefined, "block"));
  const displayOptions = createMemo((): GraphSearchDisplayOptions => ({
    ...(props.route.pageMatchScope !== undefined
      ? { pageMatchScope: props.route.pageMatchScope } : {}),
    pageView: pageView(),
    blockView: blockView(),
  }));

  const [executionResource, executionPending] = createReadyQueryResource(
    () => ({
      id: props.route.id,
      source: source().trim(),
      sourceKind: sourceKind(),
      explain: explain(),
      presentation: presentation(),
      // The complete captured settings ARE part of what this request asks: two
      // workspaces whose text is identical but whose Display differs are two
      // different questions, and re-using one's answer for the other is how a
      // late result lands on the wrong state (I-20).
      display: JSON.stringify(displayOptions()),
    }),
    async (request): Promise<QueryExecution & { statistics?: QueryStatistics }> => {
      if (!request.source) {
        return { hits: [], diagnostics: [], explanation: { branches: [] }, cancelled: false };
      }
      if (request.sourceKind === "search") {
        return deps().runGraphSearch(
          request.source,
          PAGE_LIMIT,
          BLOCK_LIMIT,
          `query-workspace:${request.id}`,
          request.explain,
          JSON.parse(request.display) as GraphSearchDisplayOptions,
        );
      }
      if (ADVANCED_QUERY_RE.test(request.source)) {
        const result = await deps().runAdvancedQuery(request.source);
        return groupsToExecution(result.groups, request.explain, diagnosticsFromAdvanced(result));
      }
      // An explicit query runs through the typed IR route under the view of its
      // OWN declared anchor: a page-anchored query is a Pages section, and
      // handing it the Blocks section's settings would order pages by a block
      // field. The anchor is the parse's, so the route asks for the view the
      // anchor names rather than guessing from the presentation.
      const options = JSON.parse(request.display) as GraphSearchDisplayOptions;
      const result = await deps().runQuery(request.source, {
        page: options.pageView ?? {},
        block: options.blockView ?? {},
      });
      if (Array.isArray(result)) return groupsToExecution(result, request.explain);
      const execution = result.anchor === "block"
        ? groupsToExecution(result.groups, request.explain)
        : {
          // Page rows survive the adapter WHOLE: the hydrated row is what the
          // page table's columns and the page board's grouping read, and the
          // old adapter threw it away and kept the display name.
          hits: result.pages.map((page): QueryHit => ({
            entity: "page",
            page: {
              name: page.name, kind: page.kind, path: page.path,
              date_key: page.journal_day ?? null,
            },
            display_text: page.name,
            evidence: [],
            score: 0,
            row: page,
          })),
          diagnostics: [], explanation: { branches: [] }, cancelled: false,
        };
      return { ...execution, statistics: result.statistics };
    }
  );

  const execution = createMemo<(QueryExecution & { statistics?: QueryStatistics }) | undefined>(
    (previous) => executionResource.error ? previous : executionResource(),
  );
  const statisticsSummary = createMemo(() => querySummary({ statistics: execution()?.statistics }));
  const hits = () => execution()?.hits ?? [];
  /** **The returned operation, PARTITIONED — never refetched per family.**
   *
   *  One read answers both questions, so the two sections are two views of one
   *  operation. Fetching a family on its own would let the halves describe two
   *  different graph states, which is the exact shape I-20 forbids. */
  const pageHits = createMemo(() =>
    hits().filter((hit): hit is QueryPageHit => hit.entity === "page"));
  const blockHits = createMemo(() => hits().filter((hit) => hit.entity === "block"));
  const boardGroups = createMemo(() => {
    // Adjacency, not identity: with an explicit sort one page name can
    // legitimately open more than one group, and re-clustering by name would
    // reorder rows the backend deliberately placed.
    const out: [string, QueryHit[]][] = [];
    for (const hit of blockHits()) {
      const page = hitPage(hit);
      const last = out[out.length - 1];
      if (last && last[0] === page) last[1].push(hit);
      else out.push([page, [hit]]);
    }
    return out;
  });
  /** Each section's Display panel writes only its OWN namespace on the route. */
  const [displayOpen, setDisplayOpen] = createSignal(false);
  const registry = createQueryRegistryAccess(() => displayOpen());
  const sectionControl = (kind: "page" | "block") => (
    <QueryDisplay
      rowKind={kind}
      registry={registry}
      onOpenChange={setDisplayOpen}
      control={{
        view: kind === "page" ? pageView() : blockView(),
        apply: (next: ViewSettings) => {
          const { view, ...draft } = next;
          props.router.updateActiveQuery(kind === "page"
            ? { pagePresentation: (view ?? "list") as QueryPresentation, pageDisplay: draft }
            : { blockPresentation: (view ?? "list") as QueryPresentation, blockDisplay: draft });
        },
        ...(execution()?.statistics ? { statistics: execution()!.statistics } : {}),
        statisticsView: kind === "page" ? pageView() : blockView(),
      }}
    />
  );

  const updateSource = (next: string, kind = sourceKind()) => {
    setSource(next);
    setSourceKind(kind);
    props.router.updateActiveQuery({ source: next, sourceKind: kind });
  };
  const updatePresentation = (next: QueryPresentation) => {
    setPresentation(next);
    props.router.updateActiveQuery({ presentation: next });
  };
  const closeAdvanced = () => {
    setAdvancedOpen(false);
    queueMicrotask(() => advancedButton?.focus());
  };
  const openHit = (hit: QueryHit) => {
    if (hit.entity === "page") {
      props.router.openPageTarget({
        name: hit.page.name,
        pageKind: hit.page.kind,
        ...(hit.page.path ? { path: hit.page.path } : {}),
      });
    } else {
      props.router.openPageAtBlock({
        name: hit.page,
        pageKind: hit.kind,
        block: blockDtoExternalId(hit.block),
        ...(hit.path ? { path: hit.path } : {}),
      });
    }
  };
  const hitSurfaceId = (hit: QueryHit) =>
    `query:${props.route.id}:${hit.entity}:${hit.entity === "page" ? hit.page.name : hit.block.id}`;
  const captureSave = (): CapturedSave => ({
    token: ++saveToken,
    inputRevision: inputRevision(),
    routeId: props.route.id,
    title: title(),
    source: source(),
    sourceKind: sourceKind(),
    presentation: presentation(),
    settings: captureSettings(props.route),
    // The graph's format decides WHERE the view property goes, so it is part of
    // what this attempt publishes, not something to re-read at completion.
    format: graphMeta()?.preferred_format ?? "md",
    scope: captureGraphScope(),
  });
  /** Is this attempt's workspace still the live one? Same component, same graph
   *  binding (I-20 — the binding, never the render epoch), same ACTIVE route.
   *  Only the newest attempt owns the shared `saving` flag. */
  const sameWorkspace = (captured: CapturedSave): boolean => {
    if (!alive || captured.token !== saveToken) return false;
    if (!isScopeCurrent(captured.scope)) return false;
    const active = props.router.route?.();
    if (active && (active.kind !== "query" || active.id !== captured.routeId)) return false;
    return props.route.id === captured.routeId;
  };
  /** …and does it still say what this attempt captured? A route id is not an
   *  input: a source, view, title or graph-format edit under the SAME id is a
   *  different publication, and publishing the captured one would be wrong. */
  const sameInput = (captured: CapturedSave): boolean =>
    sameWorkspace(captured)
    && inputRevision() === captured.inputRevision
    && title() === captured.title
    && source() === captured.source
    && sourceKind() === captured.sourceKind
    && presentation() === captured.presentation
    // A captured draft mutated and RESTORED during the await is still a newer
    // edit as far as `inputRevision` is concerned; comparing the envelope as
    // well is what makes a scope or draft change supersede the attempt too.
    && captureSettings(props.route) === captured.settings
    && (graphMeta()?.preferred_format ?? "md") === captured.format;
  const save = async (event: SubmitEvent) => {
    event.preventDefault();
    if (saving()) return;
    const captured = captureSave();
    saveController?.abort();
    saveController = new AbortController();
    setSaving(true);
    setSaveError(null);
    setSaveNotice(null);
    try {
      const result = await materializeQueryWorkspace({
        title: captured.title,
        sourceKind: captured.sourceKind,
        source: captured.source,
        presentation: captured.presentation,
        ...settingsInput(captured.settings),
        routeId: captured.routeId,
        format: captured.format,
      }, deps(), () => sameInput(captured), saveController.signal);
      // Every branch below is about the LOCAL surface. A workspace that has
      // moved on gets nothing written into it: not a route replacement, not an
      // error, not a notice.
      if (!sameWorkspace(captured)) return;
      if (!result.ok) {
        setSaveError(result.message);
        return;
      }
      if (!sameInput(captured)) {
        // `savePage` had already begun when the input changed, so the page is
        // real and `bumpPageInventoryRev` has already run. It was not undone
        // and must not be deleted — but it is no longer what this workspace
        // shows, so the route stays where the user put it.
        setSaveNotice(`“${result.name}” was saved from the earlier search, so this workspace was left as it is.`);
        return;
      }
      props.router.replaceActiveRoute({ kind: "page", name: result.name, pageKind: "page" });
    } finally {
      // The captured token guards the shared flag too: a superseded attempt may
      // not re-enable a button a newer one is still using.
      if (alive && captured.token === saveToken) setSaving(false);
    }
  };

  const resultButton = (hit: QueryHit, body: JSX.Element) => (
    <button
      type="button"
      class="query-result-row switcher-row"
      data-inpage-find-surface={hitSurfaceId(hit)}
      onClick={() => openHit(hit)}
    >
      {body}
    </button>
  );

  return (
    <section class="query-workspace" data-query-route-id={props.route.id} aria-label="Search and query workspace">
      <header class="query-workspace-header">
        <div class="query-workspace-search-row">
          <label class="query-workspace-source-label">
            <span class="sr-only">{sourceKind() === "search" ? "Search" : "Query DSL"}</span>
            <input
              ref={sourceInput}
              class="query-workspace-source"
              type="search"
              value={source()}
              onInput={(event) => updateSource(event.currentTarget.value)}
              placeholder={sourceKind() === "search" ? "Search pages and blocks" : "Query DSL"}
              aria-describedby="query-workspace-summary"
              spellcheck={false}
            />
          </label>
          <button
            ref={advancedButton}
            type="button"
            class="query-advanced-toggle"
            aria-haspopup="dialog"
            aria-expanded={advancedOpen()}
            onClick={() => setAdvancedOpen(true)}
          >
            Filters / Advanced
          </button>
        </div>
        <p id="query-workspace-summary" class="query-workspace-summary">
          {sourceKind() === "search"
            ? friendlySummary(source())
            : "Runs the saved query expression against blocks. Its presentation is controlled separately."}
        </p>

        <div class="query-workspace-controls">
          <div class="query-presentations" role="group" aria-label="Result presentation">
            <For each={["search", "list", "table", "board"] as QueryPresentation[]}>
              {(view) => (
                <button
                  type="button"
                  classList={{ active: presentation() === view }}
                  aria-pressed={presentation() === view}
                  onClick={() => updatePresentation(view)}
                >
                  {view[0].toUpperCase() + view.slice(1)}
                </button>
              )}
            </For>
          </div>
          <button
            type="button"
            class="query-explain-toggle"
            aria-pressed={explain()}
            onClick={() => setExplain((value) => !value)}
          >
            {explain() ? "Hide explanation" : "Explain query"}
          </button>
        </div>

        <form class="query-workspace-save" onSubmit={save}>
          <label>
            <span class="sr-only">Page title</span>
            <input
              value={title()}
              onInput={(event) => { setTitle(event.currentTarget.value); setSaveError(null); setSaveNotice(null); }}
              placeholder="Name this search to save it as a page"
              aria-invalid={!!saveError()}
            />
          </label>
          <button type="submit" disabled={saving()}>{saving() ? "Saving…" : "Save page"}</button>
        </form>
        <Show when={saveError()}>
          <p class="query-workspace-save-error" role="alert">{saveError()}</p>
        </Show>
        <Show when={saveNotice()}>
          <p class="query-workspace-save-notice" role="status">{saveNotice()}</p>
        </Show>
      </header>

      <section class="query-workspace-status" aria-live="polite" aria-atomic="true">
        <Show when={!source().trim()}>{sourceKind() === "search" ? "Enter a search to begin." : "Enter a query to begin."}</Show>
        <Show when={!!source().trim() && executionResource.loading}>{executionPending()?.message ?? "Searching…"}</Show>
        <Show when={!!source().trim() && !executionResource.loading && executionResource.error}>
          Search failed: {executionResource.error instanceof Error ? executionResource.error.message : String(executionResource.error)}
        </Show>
        <Show when={!!source().trim() && !executionResource.loading && !executionResource.error && execution()?.cancelled}>
          Search superseded by a newer request.
        </Show>
        <Show when={!!source().trim() && !executionResource.loading && !executionResource.error && execution() && !execution()?.cancelled}>
          {hits().length} result{hits().length === 1 ? "" : "s"}
        </Show>
      </section>

      <Show when={statisticsSummary()}>{(summary) => (
        <section class="query-workspace-summary" aria-label="Query statistics">
          <For each={summary().columns}>{(column, at) => (
            <span>{column.label}: {summary().overall[at()]?.text} </span>
          )}</For>
          <Show when={summary().groups}>{(groups) => (
            <table aria-label="Grouped query statistics">
              <thead><tr><th>{summary().groupLabel}</th><For each={summary().columns}>{(column) => <th>{column.label}</th>}</For></tr></thead>
              <tbody><For each={groups()}>{(group) => <tr><th>{group.label}</th><For each={group.cells}>{(cell) => <td>{cell.text}</td>}</For></tr>}</For></tbody>
            </table>
          )}</Show>
          <Show when={summary().multiMembership}><p>A row with multiple tags contributes to each tag group.</p></Show>
          <Show when={summary().notice}><p>{summary().notice}</p></Show>
        </section>
      )}</Show>

      <Show when={(execution()?.diagnostics.length ?? 0) > 0}>
        <ul class="query-workspace-diagnostics" aria-label="Query diagnostics">
          <For each={execution()?.diagnostics ?? []}>{(diagnostic) => (
            <li role="alert" data-code={diagnostic.code}>{diagnostic.message}</li>
          )}</For>
        </ul>
      </Show>

      <Show when={explain() && (execution()?.explanation.branches.length ?? 0) > 0}>
        <section class="query-workspace-explanation" aria-label="Query explanation">
          <h2>How this query works</h2>
          <ExplainTree nodes={execution()?.explanation.branches ?? []} />
        </section>
      </Show>

      <Show when={!executionResource.loading && !executionResource.error && !hits().length && source().trim() && !execution()?.diagnostics.length}>
        <p class="query-workspace-empty">No matching pages or blocks.</p>
      </Show>

      {/* **Two families, mounted Pages before Blocks, each independently
          controlled** (§7.6, Q3). The rows are rendered in the order the
          backend returned them: ordering and sampling happen in SQL over the
          complete matched set, so a re-sort here would silently replace a
          complete answer with an answer about whatever fitted. */}
      <QueryResultSections
        pending={() => !!source().trim() && executionResource.loading}
        pendingMessage={() => executionPending()?.message ?? "Searching…"}
        failure={() => {
          if (!source().trim() || executionResource.loading) return null;
          const error = executionResource.error;
          if (error) {
            return `Search failed: ${error instanceof Error ? error.message : String(error)}`;
          }
          // A superseded read is not an empty answer either: nothing about the
          // graph was learned, so saying "no results" would be a claim this
          // operation never made.
          return execution()?.cancelled ? "Search superseded by a newer request." : null;
        }}
        families={[
          {
            kind: "page",
            control: sectionControl("page"),
            empty: () => pageHits().length === 0,
            hasMore: () => !!execution()?.has_more?.pages,
            countLabel: () => `${pageHits().length} shown`,
            body: () => (
              <QueryPageResults
                hits={pageHits}
                view={pageView}
                onOpen={openHit}
                surfaceId={hitSurfaceId}
              />
            ),
          },
          {
            kind: "block",
            control: sectionControl("block"),
            empty: () => blockHits().length === 0,
            hasMore: () => !!execution()?.has_more?.blocks,
            countLabel: () => `${blockHits().length} shown`,
            body: () => (
              <Switch>
                <Match when={(blockView().view ?? "list") === "search"}>
                  <div class="query-results-search" role="list" aria-label="Block results">
                    <For each={blockHits()}>{(hit) => (
                      <div role="listitem">
                        {resultButton(hit, <SearchResultRow
                          page={hitPage(hit)}
                          breadcrumb={hit.entity === "block" ? hit.block.breadcrumb ?? [] : []}
                          text={hit.display_text}
                          spans={hitMatchSpans(hit)}
                        />)}
                      </div>
                    )}</For>
                  </div>
                </Match>

                <Match when={(blockView().view ?? "list") === "list"}>
                  <ul class="query-results-list" aria-label="Block results">
                    <For each={blockHits()}>{(hit) => (
                      <li>
                        <button type="button" data-inpage-find-surface={hitSurfaceId(hit)} onClick={() => openHit(hit)}>
                          <span class="query-list-context">{hitPage(hit)}</span>
                          <span class="query-list-text"><MarkedText text={hit.display_text} spans={hitMatchSpans(hit)} /></span>
                        </button>
                      </li>
                    )}</For>
                  </ul>
                </Match>

                <Match when={(blockView().view ?? "list") === "table"}>
                  <div class="query-results-table-wrap">
                    <table class="query-results-table">
                      <caption class="sr-only">Block results</caption>
                      <thead><tr><th scope="col">Page</th><th scope="col">Content</th></tr></thead>
                      <tbody>
                        <For each={blockHits()}>{(hit) => (
                          <tr data-inpage-find-surface={hitSurfaceId(hit)}>
                            <td><button type="button" onClick={() => openHit(hit)}>{hitPage(hit)}</button></td>
                            <td><MarkedText text={hit.display_text} spans={hitMatchSpans(hit)} /></td>
                          </tr>
                        )}</For>
                      </tbody>
                    </table>
                  </div>
                </Match>

                <Match when={(blockView().view ?? "list") === "board"}>
                  <div class="query-results-board" aria-label="Block results grouped by page">
                    <For each={boardGroups()}>{([page, groupHits]) => (
                      <section class="query-board-column" aria-label={page}>
                        <h4>{page}<span class="query-board-count">{groupHits.length}</span></h4>
                        <div role="list" aria-label="Block results">
                          <For each={groupHits}>{(hit) => (
                            <div role="listitem" class="query-board-card">
                              <button type="button" data-inpage-find-surface={hitSurfaceId(hit)} onClick={() => openHit(hit)}>
                                <span class="sr-only">{hitKind(hit)}: </span>
                                <MarkedText text={hit.display_text} spans={hitMatchSpans(hit)} />
                              </button>
                            </div>
                          )}</For>
                        </div>
                      </section>
                    )}</For>
                  </div>
                </Match>
              </Switch>
            ),
          },
        ]}
      />

      <Show when={advancedOpen()}>
        <AdvancedModal
          source={source}
          sourceKind={sourceKind}
          onApply={(next, kind) => { updateSource(next, kind); closeAdvanced(); }}
          onClose={closeAdvanced}
          layerId={advancedLayerId}
          trigger={() => advancedButton ?? null}
        />
      </Show>
    </section>
  );
}
