import {
  For,
  Match,
  Show,
  Switch,
  createEffect,
  createMemo,
  createResource,
  createSignal,
  createUniqueId,
  onCleanup,
  type JSX,
} from "solid-js";
import { backend } from "../backend";
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
  MatchSpan,
  PageDto,
  QueryDiagnostic,
  QueryExecution,
  QueryExplainNode,
  QueryHit,
  RefGroup,
  SavePageResult,
} from "../types";
import { QueryBuilder, type BuilderSession } from "./QueryBuilder";
import { SearchResultRow, buildSearchExcerpt } from "./SearchResultRow";
import { registerTransientLayer } from "../transientLayers";
import { bumpPageInventoryRev, graphMeta } from "../ui";
import { blockDtoExternalId } from "../blockIdentity";
import { isSaveConflictFailure } from "../persistence";
import { captureGraphScope, isScopeCurrent, type GraphScope } from "../landAsync";
import { markdownRawWithProperty, orgRawWithProperty } from "../editor/properties";
import { queryViewPropertyPatch } from "../editor/queryViewProperties";

const PAGE_LIMIT = 40;
const BLOCK_LIMIT = 100;
const ADVANCED_QUERY_RE = /^\s*\[:/;

export interface MaterializeQueryInput {
  title: string;
  sourceKind: QueryRoute["sourceKind"];
  source: string;
  presentation: QueryPresentation;
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
  /** Rust-authoritative friendly-search validation; required before every nonblank friendly save. */
  runGraphSearch(source: string, pageLimit: number, blockLimit: number, lane: string, explain: boolean): Promise<QueryExecution>;
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
  runQuery(source: string): Promise<RefGroup[]>;
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
  input: Pick<MaterializeQueryInput, "source" | "sourceKind" | "presentation" | "format">
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
  // This packet materializes only the one property the workspace has always
  // written; C2B owns complete effective-view materialization, so any other
  // write the map would produce belongs to that packet, not this one.
  const writes = queryViewPropertyPatch({
    view: { view: input.presentation === "list" ? undefined : input.presentation },
    properties: [],
  }).filter(([key]) => key === "tine.view");
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
  isCurrent: IsCurrentInput = () => true
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
      const execution = await deps.runGraphSearch(input.source.trim(), 0, 0, `query-workspace:${input.routeId}:materialize`, true);
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
    runGraphSearch: (source, pageLimit, blockLimit, lane, explain) =>
      api.runGraphSearch(source, pageLimit, blockLimit, lane, explain),
    runQuery: (source) => api.runQuery(source),
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

function hitSpans(hit: QueryHit): MatchSpan[] {
  const field = hit.entity === "page" ? "page_name" : "visible_content";
  return hit.evidence.filter((item) => item.field === field).flatMap((item) => item.spans);
}

function hitPage(hit: QueryHit): string {
  return hit.entity === "page" ? hit.page.name : hit.page;
}

function hitKind(hit: QueryHit): "Page" | "Block" {
  return hit.entity === "page" ? "Page" : "Block";
}

function MarkedText(props: { text: string; spans: MatchSpan[] }): JSX.Element {
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
  const [builderSession] = createResource(dsl, async (text): Promise<BuilderSession> => {
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
  format: Format;
  scope: GraphScope | null;
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
  // A changed-and-restored value is still a newer edit. Include the route
  // object so its non-presentation Display draft participates in the revision.
  const inputRevision = createMemo((previous: number) => {
    props.route;
    source();
    sourceKind();
    presentation();
    title();
    graphMeta()?.preferred_format;
    return previous + 1;
  }, 0);
  onCleanup(() => { alive = false; });
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

  const [execution] = createResource(
    () => ({
      id: props.route.id,
      source: source().trim(),
      sourceKind: sourceKind(),
      explain: explain(),
    }),
    async (request): Promise<QueryExecution> => {
      if (!request.source) {
        return { hits: [], diagnostics: [], explanation: { branches: [] }, cancelled: false };
      }
      if (request.sourceKind === "search") {
        return deps().runGraphSearch(
          request.source,
          PAGE_LIMIT,
          BLOCK_LIMIT,
          `query-workspace:${request.id}`,
          request.explain
        );
      }
      if (ADVANCED_QUERY_RE.test(request.source)) {
        const result = await deps().runAdvancedQuery(request.source);
        return groupsToExecution(result.groups, request.explain, diagnosticsFromAdvanced(result));
      }
      return groupsToExecution(await deps().runQuery(request.source), request.explain);
    }
  );

  const hits = () => execution()?.hits ?? [];
  const boardGroups = createMemo(() => {
    const grouped = new Map<string, QueryHit[]>();
    for (const hit of hits()) {
      const page = hitPage(hit);
      const group = grouped.get(page);
      if (group) group.push(hit);
      else grouped.set(page, [hit]);
    }
    return [...grouped.entries()];
  });

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
    && (graphMeta()?.preferred_format ?? "md") === captured.format;
  const save = async (event: SubmitEvent) => {
    event.preventDefault();
    if (saving()) return;
    const captured = captureSave();
    setSaving(true);
    setSaveError(null);
    setSaveNotice(null);
    try {
      const result = await materializeQueryWorkspace({
        title: captured.title,
        sourceKind: captured.sourceKind,
        source: captured.source,
        presentation: captured.presentation,
        routeId: captured.routeId,
        format: captured.format,
      }, deps(), () => sameInput(captured));
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
        <Show when={!!source().trim() && execution.loading}>Searching…</Show>
        <Show when={!!source().trim() && !execution.loading && execution.error}>
          Search failed: {execution.error instanceof Error ? execution.error.message : String(execution.error)}
        </Show>
        <Show when={!!source().trim() && !execution.loading && !execution.error && execution()?.cancelled}>
          Search superseded by a newer request.
        </Show>
        <Show when={!!source().trim() && !execution.loading && !execution.error && execution() && !execution()?.cancelled}>
          {hits().length} result{hits().length === 1 ? "" : "s"}
        </Show>
      </section>

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

      <Show when={!execution.loading && !execution.error && !hits().length && source().trim() && !execution()?.diagnostics.length}>
        <p class="query-workspace-empty">No matching pages or blocks.</p>
      </Show>

      <Switch>
        <Match when={presentation() === "search"}>
          <div class="query-results-search" role="list" aria-label="Search results">
            <For each={hits()}>{(hit) => (
              <div role="listitem">
                {hit.entity === "block"
                  ? resultButton(hit, <SearchResultRow
                    page={hit.page}
                    breadcrumb={hit.block.breadcrumb ?? []}
                    text={hit.display_text}
                    spans={hitSpans(hit)}
                  />)
                  : resultButton(hit, <>
                    <span class="switcher-kind">page</span>
                    <span class="search-result-body">
                      <span class="search-result-context">Page</span>
                      <span class="search-result-excerpt">
                        <For each={buildSearchExcerpt(hit.display_text, hitSpans(hit))}>{(segment) => segment.marked
                          ? <mark>{segment.text}</mark>
                          : segment.text}</For>
                      </span>
                    </span>
                  </>)}
              </div>
            )}</For>
          </div>
        </Match>

        <Match when={presentation() === "list"}>
          <ul class="query-results-list" aria-label="Query results">
            <For each={hits()}>{(hit) => (
              <li>
                <button type="button" data-inpage-find-surface={hitSurfaceId(hit)} onClick={() => openHit(hit)}>
                  <span class="query-list-context">{hitPage(hit)}</span>
                  <span class="query-list-text"><MarkedText text={hit.display_text} spans={hitSpans(hit)} /></span>
                </button>
              </li>
            )}</For>
          </ul>
        </Match>

        <Match when={presentation() === "table"}>
          <div class="query-results-table-wrap">
            <table class="query-results-table">
              <caption class="sr-only">Query results</caption>
              <thead><tr><th scope="col">Type</th><th scope="col">Page</th><th scope="col">Content</th></tr></thead>
              <tbody>
                <For each={hits()}>{(hit) => (
                  <tr data-inpage-find-surface={hitSurfaceId(hit)}>
                    <td>{hitKind(hit)}</td>
                    <td><button type="button" onClick={() => openHit(hit)}>{hitPage(hit)}</button></td>
                    <td><MarkedText text={hit.display_text} spans={hitSpans(hit)} /></td>
                  </tr>
                )}</For>
              </tbody>
            </table>
          </div>
        </Match>

        <Match when={presentation() === "board"}>
          <div class="query-results-board" aria-label="Query results grouped by page">
            <For each={boardGroups()}>{([page, pageHits]) => (
              <section class="query-board-column">
                <h2>{page}<span class="query-board-count">{pageHits.length}</span></h2>
                <div role="list">
                  <For each={pageHits}>{(hit) => (
                    <button type="button" role="listitem" class="query-board-card" data-inpage-find-surface={hitSurfaceId(hit)} onClick={() => openHit(hit)}>
                      <span class="sr-only">{hitKind(hit)}: </span><MarkedText text={hit.display_text} spans={hitSpans(hit)} />
                    </button>
                  )}</For>
                </div>
              </section>
            )}</For>
          </div>
        </Match>
      </Switch>

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
