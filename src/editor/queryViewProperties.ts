// **The `tine.*` view properties of a QUERY block: one reader, one writer** (P5A).
//
// A query block's presentation lives in block properties (§7.6), and three
// different surfaces need to agree about it: the save path in `Macro.tsx`, the
// query table in `SheetTable.tsx`, and the static publisher in Rust. This module
// is the TypeScript half of that agreement. It is pure: no store, no DOM, no
// backend. The side-effect owner stays `store.ts::setBlockProperty`.
//
// **Why a TypeScript half exists at all** (D-14). `crates/tine-core/src/query/
// view.rs::resolve_query_columns` is the authority, and `publish.rs` calls it
// rather than copying it — Rust has exactly ONE implementation. Rendering a
// table, however, is a synchronous walk over blocks already in memory and
// cannot take an IPC round-trip per block, which is the same reason
// `queryMacro.ts` transcribes the raw extent reader. The pair is legitimate
// because it is PINNED: `crates/tine-core/tests/fixtures/query-columns/
// resolution.json` is read by `query_columns_resolution.rs` and by
// `queryViewProperties.test.ts`, and if the two readers ever disagree one of
// those tests goes red.
//
// Nothing here is a query parser or printer. The backend remains the only
// producer of query language.
import { propertyKeyNorm } from "../render/block";
import type { AggFn, Field, ViewSettings } from "./queryIr";

/** The six display facts a query block persists (§7.6). A typed sheet SCHEMA is
 *  deliberately not one of them: `tine.fields::` is schema, `tine.columns::` is
 *  which columns the query shows, and conflating the two is what made a filter
 *  save destroy a declared schema. */
export const QUERY_VIEW_PROPERTY_KEYS = [
  "tine.view",
  "tine.sort",
  "tine.group-field",
  "tine.columns",
  "tine.col-aggregates",
  "tine.sample",
] as const;

/** The typed sheet schema property. Read here only to RECOGNISE a pre-split
 *  bare column list; its typed form is never written or interpreted by this
 *  module. */
export const QUERY_SCHEMA_PROPERTY = "tine.fields";
export const QUERY_COLUMNS_PROPERTY = "tine.columns";
/** The query-owned grouping key (P5B): one canonical sheet `FieldId`. */
export const QUERY_GROUP_FIELD_PROPERTY = "tine.group-field";
/** The ambiguous predecessor. Read for compatibility, retired on the first save
 *  that states the grouping, never written. */
export const QUERY_LEGACY_GROUP_PROPERTY = "tine.group-by";

/** The six sheet builtins a column name can spell. Every other string is an
 *  ordinary property name. Mirrors `BUILTIN_FIELDS` in `sheet/config.ts` and
 *  `sheet_field_for_column` in `publish.rs`. */
export const QUERY_COLUMN_BUILTINS: ReadonlySet<string> = new Set([
  "state",
  "priority",
  "scheduled",
  "deadline",
  "tags",
  "page",
]);

export type PropertyPairs = readonly (readonly [string, string])[];

/** What a block's properties say about its visible columns.
 *
 *  `cleared` is not the same as `unset`: a PRESENT `tine.columns` is an explicit
 *  statement, so an empty or invalid one means "no property columns" with no
 *  legacy list and no query-text columns behind it. That is what makes clearing
 *  a column choice final rather than a way to resurrect an older list. */
export type QueryColumnsResolution =
  | { kind: "named"; columns: Field[] }
  | { kind: "cleared" }
  | { kind: "unset" };

/** The column-list grammar, applied to a WHOLE property value: trim, split on
 *  `;`, trim each token, discard empty segments. One token containing `=`, NUL,
 *  CR or LF invalidates the ENTIRE list — a half-read column list is worse
 *  evidence than none, and `=` anywhere means the value is a schema or a mixed
 *  value, never columns.
 *
 *  `null` is "this value is not a column list"; `[]` is "a column list with
 *  nothing in it". No per-name length cap: these are property bytes an outside
 *  editor may have authored. */
export function queryColumnTokens(value: string): Field[] | null {
  const out: Field[] = [];
  for (const raw of value.trim().split(";")) {
    const token = raw.trim();
    if (!token) continue;
    if (/[=\0\r\n]/.test(token)) return null;
    out.push(token);
  }
  return out;
}

/** Whether a `tine.fields` value is a PROVEN pre-split bare column list rather
 *  than a typed schema. Only such a value may be read as legacy columns, and
 *  only such a value may be retired when the new key takes over — a typed or
 *  mixed schema is never touched. */
export function isLegacyBareColumnList(value: string | null | undefined): boolean {
  if (value == null) return false;
  const tokens = queryColumnTokens(value);
  return tokens !== null && tokens.length > 0;
}

function firstProperty(props: PropertyPairs, key: string): string | undefined {
  const wanted = propertyKeyNorm(key);
  for (const [rawKey, value] of props) {
    if (propertyKeyNorm(rawKey) === wanted) return value;
  }
  return undefined;
}

/** SPEC §7.6 + P5A precedence for a query block's visible columns. The exact
 *  mirror of `query::view::resolve_query_columns`; the shared fixture set pins
 *  the pair.
 *
 *   1. `tine.columns` PRESENT → its own answer, and nothing behind it.
 *   2. `tine.columns` ABSENT → `tine.fields` read as a LEGACY column list, but
 *      only when every nonempty token passes the same grammar and at least one
 *      exists. This is compatibility for authored notes, not a private-state
 *      migration (D-1 is not engaged).
 *   3. Neither → `unset`, and the query text's own columns stand.
 *
 *  Reading never writes: opening or rendering a block does not migrate it. */
export function resolveQueryColumns(props: PropertyPairs): QueryColumnsResolution {
  const present = firstProperty(props, QUERY_COLUMNS_PROPERTY);
  if (present !== undefined) {
    const tokens = queryColumnTokens(present);
    return tokens && tokens.length > 0 ? { kind: "named", columns: tokens } : { kind: "cleared" };
  }
  const legacy = firstProperty(props, QUERY_SCHEMA_PROPERTY);
  if (legacy !== undefined) {
    const tokens = queryColumnTokens(legacy);
    if (tokens && tokens.length > 0) return { kind: "named", columns: tokens };
  }
  return { kind: "unset" };
}

/** The columns a query face SHOWS, or `null` for "no selection — keep the
 *  default column set". `cleared` and `unset` differ in what they suppress
 *  behind them, not in what the renderer draws. */
export function selectedQueryColumns(props: PropertyPairs): Field[] | null {
  const resolved = resolveQueryColumns(props);
  return resolved.kind === "named" ? resolved.columns : null;
}

// --------------------------------------------------------------------------
// Grouping (P5B)
// --------------------------------------------------------------------------

/** What a block's properties say about its grouping field.
 *
 *  `cleared` and `unset` are NOT the same, and the difference is a product one:
 *  `unset` is the only state in which a Board may apply its `state` default
 *  (ADR 0030). `cleared` is the user having said "no grouping" out loud, and a
 *  view switch may not undo that. */
export type QueryGroupingResolution =
  | { kind: "field"; field: string }
  | { kind: "cleared" }
  | { kind: "unset" };

/** The parsed (query-text) half of a grouping resolution: the `(group-by …)`
 *  directive the engine lifted, and the view the text asked for. */
export interface ParsedGroupingContext {
  group_by?: string | null;
  view?: string | null;
}

const GROUP_BUILTINS: ReadonlySet<string> = QUERY_COLUMN_BUILTINS;

/** A token that could survive a property line at all. `;` is legal here — a
 *  grouping value is one field, not a list — but a NUL or a line break is not. */
function groupTokenSerializable(token: string): boolean {
  return !/[\0\r\n]/.test(token);
}

/** The NEW key's grammar (the mirror of `view.rs::canonical_group_field`):
 *  exactly a sheet builtin, or `prop:`/`formula:` with a nonempty suffix, after
 *  trimming. Anything else — the empty value included — is an explicit
 *  no-grouping statement rather than a value to guess at. */
export function canonicalGroupField(value: string): string | null {
  const token = value.trim();
  if (!token || !groupTokenSerializable(token)) return null;
  if (GROUP_BUILTINS.has(token)) return token;
  if (token.startsWith("prop:")) return token.length > "prop:".length ? token : null;
  if (token.startsWith("formula:")) return token.length > "formula:".length ? token : null;
  return null;
}

/** The LEGACY token's meaning, captured at the view the note is CURRENTLY
 *  persisted with — never at the view the user is switching to. The mirror of
 *  `view.rs::legacy_group_field`; the shared corpus pins the pair.
 *
 *   * a sheet face (board/table) keeps the sheet spellings, and every OTHER
 *     bare name is now an ordinary property — the fix for `status` silently
 *     becoming the task marker;
 *   * a list/search face reads `page` as the source page and every other token
 *     as an EXACT property key, `state` and a literal `prop:` prefix included,
 *     which is what `groupRows` has always done. */
export function legacyGroupField(value: string, sheetFace: boolean): string | null {
  const token = value.trim();
  if (!token || !groupTokenSerializable(token)) return null;
  if (!sheetFace) return token === "page" ? "page" : `prop:${token}`;
  if (GROUP_BUILTINS.has(token)) return token;
  if (token.startsWith("prop:") && token.length > "prop:".length) return token;
  if (token.startsWith("formula:") && token.length > "formula:".length) return token;
  if (token.startsWith("formula.") && token.length > "formula.".length) {
    return `formula:${token.slice("formula.".length)}`;
  }
  return `prop:${token}`;
}

const VIEW_KINDS: ReadonlySet<string> = new Set(["search", "list", "table", "board"]);

/** The view a legacy grouping token must be READ under: the block's own
 *  `tine.view::` when readable, else whatever the query text asked for, else
 *  the default list. */
function effectiveViewKind(props: PropertyPairs, parsed: ParsedGroupingContext): string {
  const persisted = (firstProperty(props, "tine.view") ?? "").trim().toLowerCase();
  if (VIEW_KINDS.has(persisted)) return persisted;
  const fromText = (parsed.view ?? "").trim().toLowerCase();
  return VIEW_KINDS.has(fromText) ? fromText : "list";
}

/** SPEC §7.6 + P5B precedence for the grouping field of a query block. The exact
 *  mirror of `query::view::resolve_query_grouping`, pinned by
 *  `crates/tine-core/tests/fixtures/query-grouping/resolution.json`.
 *
 *   1. `tine.group-field` PRESENT → its own answer, and nothing behind it (an
 *      unreadable or empty value is an explicit `cleared`).
 *   2. otherwise a nonempty legacy `tine.group-by`, read at the CURRENT view.
 *   3. otherwise the `(group-by …)` directive, read the same way.
 *   4. otherwise `unset`.
 *
 *  Reading never writes. */
export function resolveQueryGrouping(
  props: PropertyPairs,
  parsed: ParsedGroupingContext = {},
): QueryGroupingResolution {
  const present = firstProperty(props, QUERY_GROUP_FIELD_PROPERTY);
  if (present !== undefined) {
    const field = canonicalGroupField(present);
    return field ? { kind: "field", field } : { kind: "cleared" };
  }
  const kind = effectiveViewKind(props, parsed);
  const sheetFace = kind === "table" || kind === "board";
  const legacy = firstProperty(props, QUERY_LEGACY_GROUP_PROPERTY);
  if (legacy !== undefined) {
    const field = legacyGroupField(legacy, sheetFace);
    if (field) return { kind: "field", field };
  }
  const fromText = parsed.group_by == null ? null : legacyGroupField(parsed.group_by, sheetFace);
  return fromText ? { kind: "field", field: fromText } : { kind: "unset" };
}

/** The grouping a resolution asks a renderer for, as a `ViewSettings.group_by`
 *  value: a canonical `FieldId`, the EMPTY field for an explicit clear, or
 *  `undefined` for "nothing said". This is the one wire spelling — `Field` is
 *  still a plain string and no new IR shape was added (P5B). */
export function groupingToViewValue(resolution: QueryGroupingResolution): string | undefined {
  if (resolution.kind === "field") return resolution.field;
  return resolution.kind === "cleared" ? "" : undefined;
}

/** The exact inverse: what a `ViewSettings.group_by` value means. `undefined` is
 *  `unset`, and every present value is read by `canonicalGroupField` — the same
 *  function `resolveQueryGrouping` will apply to the property this produces.
 *
 *  Reading it back the way it will be read is the point. The merge only ever
 *  emits canonical ids, so this is the identity on real engine output; what it
 *  rules out is a caller writing a value that cannot be read back, leaving a
 *  block whose grouping property says one thing and whose renderer does
 *  another. */
export function groupingFromViewValue(value: string | undefined | null): QueryGroupingResolution {
  if (value == null) return { kind: "unset" };
  const field = canonicalGroupField(value);
  return field ? { kind: "field", field } : { kind: "cleared" };
}

/** The view a switch to `next` produces, **including the Board's one default**.
 *
 *  A board with no columns is not a board, so a switch to Board fills an UNSET
 *  grouping with the task marker — and only an unset one. An explicit clear and
 *  a legacy key that already answers are both statements, and a default that
 *  spoke over either is how switching views used to undo a choice.
 *
 *  It lives here, next to the resolution it reads, because two surfaces switch
 *  the view (the header's switcher and the Display panel) and a default that
 *  only one of them applied would make the same click mean two things. */
export function viewAfterViewSwitch(
  view: ViewSettings,
  next: ViewSettings["view"],
): ViewSettings {
  const settings: ViewSettings = { ...view, view: next };
  if (next === "board" && groupingFromViewValue(view.group_by).kind === "unset") {
    settings.group_by = "state";
  }
  return settings;
}

/** How a QUERY-backed sheet face states a display change (P5B).
 *
 *  A query table does not own its own `tine.*` keys — the QUERY does — so the
 *  header, the column order and the aggregate footer all hand their change back
 *  through this ONE callback, which runs the same `queryViewPropertyPatch` a
 *  filter save runs. Without it a query table wrote the sheet's properties and
 *  a query save wrote the query's, and the two disagreed about the same note. */
export interface QueryDisplayControl {
  /** The view the block currently resolves to — the engine's merged answer. */
  view: ViewSettings;
  /** Route a display change through the query's one save path. */
  apply: (next: ViewSettings) => void;
}

// --------------------------------------------------------------------------
// Serializing the six facts
// --------------------------------------------------------------------------

/** `tine.sort:: <field> <asc|desc>[; …]` (§7.6), the form `view.rs::parse_sort`
 *  reads back. */
export function serializeQuerySort(sort: ViewSettings["sort"]): string {
  return (sort ?? []).map(([field, dir]) => `${field} ${dir}`).join("; ");
}

/** `tine.columns:: <name>[;…]`. Token spelling and order are the author's; a
 *  renderer may deduplicate identical field ids without rewriting the source. */
export function serializeQueryColumns(columns: ViewSettings["columns"]): string {
  return (columns ?? []).join(";");
}

/** One `tine.col-aggregates` entry. `["", "count"]` is the whole-result count,
 *  spelled as a BARE `count` segment with no `=` (X3) — which is exactly why a
 *  query's aggregates can never ride the sheet's `Map<key, fn>` serializer:
 *  that shape has no spelling for a keyless entry and collapses repeated keys. */
export function serializeQueryAggregate([field, fn]: [Field, AggFn]): string {
  return field ? `${field}=${fn}` : fn;
}

export function serializeQueryAggregates(aggregates: ViewSettings["aggregates"]): string {
  return (aggregates ?? []).map(serializeQueryAggregate).join(";");
}

// --------------------------------------------------------------------------
// Reading the six facts back, to compare against what is PERSISTED
// --------------------------------------------------------------------------

/** `view.rs::parse_sort`, for comparison only. A segment with no direction
 *  sorts ascending. */
function parsePersistedSort(value: string): [Field, "asc" | "desc"][] {
  const out: [Field, "asc" | "desc"][] = [];
  for (const raw of value.split(";")) {
    const segment = raw.trim();
    if (!segment) continue;
    const at = segment.search(/\s+\S*$/);
    const tail = at < 0 ? "" : segment.slice(at).trim();
    if (at >= 0 && (tail === "asc" || tail === "desc")) {
      const name = segment.slice(0, at).trim();
      if (name) out.push([name, tail]);
      continue;
    }
    out.push([segment, "asc"]);
  }
  return out;
}

/** One recognized `tine.col-aggregates` segment, in the grammar the Rust reader
 *  accepts (`view.rs::parse_col_aggregates`): a bare `count`, or `key=fn` with
 *  `fn` one of count/sum/avg. Anything else is UNRECOGNIZED — not invalid, just
 *  not this writer's business, and preserved verbatim. */
function parseQueryAggregateSegment(segment: string): [Field, AggFn] | null {
  const text = segment.trim();
  if (!text) return null;
  const eq = text.indexOf("=");
  if (eq < 0) return text.toLowerCase() === "count" ? ["", "count"] : null;
  const key = text.slice(0, eq).trim();
  const fn = text.slice(eq + 1).trim().toLowerCase();
  if (fn !== "count" && fn !== "sum" && fn !== "avg") return null;
  return [key, fn as AggFn];
}

// --------------------------------------------------------------------------
// The patch
// --------------------------------------------------------------------------

/** One property write the caller must perform. `value === null` removes the
 *  property. */
export type QueryPropertyWrite = readonly [key: string, value: string | null];

export interface QueryViewPatchInput {
  /** The view the block will have after this save — the EFFECTIVE value,
   *  wherever it came from. `group_by` carries the P5B wire spelling: a
   *  canonical `FieldId`, `""` for an explicit clear, absent for "nothing
   *  said". */
  view: ViewSettings;
  /** The block's properties as they stand now, in document order. */
  properties: PropertyPairs;
}

/** **What a query save must write, and nothing more** (§4.3 Y2, §7.6; I-4).
 *
 *  The baseline is the block's currently PERSISTED properties, never "which
 *  control did the user touch". That distinction is the whole point: the OG
 *  printer re-emits only `(sort-by …)` and `(sample …)`, so a grouping or an
 *  aggregate that lived in the query text is dropped by the reprint of an
 *  unrelated FILTER edit — and only a property write keeps it. Comparing
 *  against the persisted value materializes exactly those facts, and rewrites
 *  nothing that already says the same thing.
 *
 *  What it never touches: `tine.fields` (typed schema), `tine.table-widths`,
 *  `tine.col-widths`, `tine.header`, `tine.filter`, `tine.formula.*`, and every
 *  unknown key. A query save is not a licence to rewrite a block's metadata.
 *
 *  Retiring a legacy list is the one exception, and it is narrow: when this save
 *  states the columns, a `tine.fields` value PROVEN to be a pre-split bare
 *  column list is removed in the same patch, so the retired list cannot come
 *  back through the legacy branch. A typed or mixed schema is never a candidate.
 */
export function queryViewPropertyPatch(input: QueryViewPatchInput): QueryPropertyWrite[] {
  const { view, properties } = input;
  const writes: QueryPropertyWrite[] = [];
  const current = (key: string) => firstProperty(properties, key);
  const push = (key: string, value: string) => writes.push([key, value || null]);

  // `tine.view` — a bare enum word.
  const viewValue = view.view ?? "";
  if ((current("tine.view") ?? "").trim().toLowerCase() !== viewValue) {
    push("tine.view", viewValue);
  }

  // `tine.sort` — compared as PARSED pairs, so re-spacing an identical sort is
  // not a write.
  const sort = view.sort ?? [];
  const persistedSort = parsePersistedSort(current("tine.sort") ?? "");
  if (!sameSort(persistedSort, sort)) push("tine.sort", serializeQuerySort(sort));

  // **`tine.group-field` — the canonical grouping identity** (P5B).
  //
  // The baseline is the block's current RESOLUTION, legacy key and DSL
  // directive included, so an unrelated filter edit on a note that still
  // spells its grouping the old way writes nothing at all. A save that does
  // change the effective grouping states it canonically and, in the SAME
  // patch and the same undo unit, retires the recognized legacy key — a view
  // switch must never be able to reinterpret a token this save has replaced.
  //
  // An explicit clear is written as the EMPTY value, not as a removal: the
  // property has to stay present, because "the user said no grouping" is what
  // stops a Board default reinstating `state` on the next switch. Both
  // property writers keep an empty value and both readers read it back as
  // present (pinned by `queryViewProperties.emptyProperty.test.ts`).
  //
  // The baseline is resolved under the view this save LEAVES BEHIND, not the
  // one the block has now. That is what makes a view switch safe: the effective
  // grouping handed in was captured at the ORIGINAL view, and if the same
  // untouched legacy token would read differently at the destination view the
  // comparison fails and the canonical identity is written before the switch
  // can reinterpret it.
  //
  // The query TEXT is deliberately NOT part of this baseline. `og_view` never
  // re-emits `(group-by …)` at all, so a directive-only grouping is exactly the
  // fact a reprint destroys — the same reason a text-only sort is materialized
  // here — and the first save that touches this block states it as a property.
  const propertiesAfterSave: PropertyPairs = [
    // The destination view, spelled out: `view.view` absent means the default
    // LIST, and leaving the slot empty would let a stale reading decide instead.
    ["tine.view", viewValue || "list"] as const,
    ...properties.filter(([key]) => propertyKeyNorm(key) !== "tine.view"),
  ];
  const persistedGrouping = resolveQueryGrouping(propertiesAfterSave);
  const nextGrouping = groupingFromViewValue(view.group_by);
  if (!sameGrouping(persistedGrouping, nextGrouping)) {
    const value = groupingToViewValue(nextGrouping);
    // `unset` can only be reached by removing the key outright; every other
    // state is a statement and is written.
    writes.push([QUERY_GROUP_FIELD_PROPERTY, value === undefined ? null : value]);
    if ((current(QUERY_LEGACY_GROUP_PROPERTY) ?? "").trim() !== "") {
      writes.push([QUERY_LEGACY_GROUP_PROPERTY, null]);
    }
  }

  // `tine.sample`.
  const sample = view.sample == null ? "" : String(view.sample);
  const persistedSample = (current("tine.sample") ?? "").trim();
  if (persistedSample !== sample) push("tine.sample", sample);

  // `tine.columns` — the baseline is the full RESOLUTION, legacy branch
  // included, because a legacy bare list is what the block's properties
  // currently spell for columns. So an unrelated filter edit on a pre-split
  // note writes nothing, while a real column change states the new list.
  const columns = view.columns ?? [];
  const resolved = resolveQueryColumns(properties);
  const persistedColumns = resolved.kind === "named" ? resolved.columns : [];
  const columnsChanged = !sameStrings(persistedColumns, columns);
  if (columnsChanged) {
    push(QUERY_COLUMNS_PROPERTY, serializeQueryColumns(columns));
    // This save states the columns, so a proven legacy bare list has no reader
    // left and must not survive as a second, stale answer.
    const legacy = current(QUERY_SCHEMA_PROPERTY);
    if (isLegacyBareColumnList(legacy)) writes.push([QUERY_SCHEMA_PROPERTY, null]);
  }

  // `tine.col-aggregates` — segment-preserving, see `mergeQueryAggregateValue`.
  const aggregates = view.aggregates ?? [];
  const rawAggregates = current("tine.col-aggregates") ?? null;
  const merged = mergeQueryAggregateValue(rawAggregates, aggregates);
  if (merged !== undefined) writes.push(["tine.col-aggregates", merged]);

  return writes;
}

/** The six display facts, and the property keys a change to each may touch. */
const DISPLAY_FACT_KEYS = {
  view: ["tine.view"],
  sort: ["tine.sort"],
  // A grouping write retires the ambiguous legacy key in the same patch.
  grouping: [QUERY_GROUP_FIELD_PROPERTY, QUERY_LEGACY_GROUP_PROPERTY],
  // A column write retires a proven pre-split bare list in the same patch.
  columns: [QUERY_COLUMNS_PROPERTY, QUERY_SCHEMA_PROPERTY],
  aggregates: ["tine.col-aggregates"],
  sample: ["tine.sample"],
} as const;

type DisplayFact = keyof typeof DISPLAY_FACT_KEYS;

/** Whether two views state the same thing about one fact. */
function sameFact(fact: DisplayFact, a: ViewSettings, b: ViewSettings): boolean {
  switch (fact) {
    case "view":
      return (a.view ?? "") === (b.view ?? "");
    case "sort":
      return serializeQuerySort(a.sort) === serializeQuerySort(b.sort);
    case "grouping":
      return sameGrouping(groupingFromViewValue(a.group_by), groupingFromViewValue(b.group_by));
    case "columns":
      return sameStrings(a.columns ?? [], b.columns ?? []);
    case "aggregates":
      return serializeQueryAggregates(a.aggregates) === serializeQueryAggregates(b.aggregates);
    case "sample":
      return (a.sample ?? null) === (b.sample ?? null);
  }
}

/** **A DISPLAY edit writes the facts it changed, and no others** (I-20).
 *
 *  `queryViewPropertyPatch`'s baseline is deliberately the block's persisted
 *  properties, because a SAVE reprints the query text and a fact that lived
 *  only in that text has to be materialized before the reprint drops it. A
 *  display edit reprints nothing, so it has nothing to materialize — and the
 *  same wide baseline becomes a hazard, because the view it is handed is the
 *  ENGINE's last reading and the engine re-reads asynchronously. Two display
 *  edits inside one parse round-trip therefore both start from the reading that
 *  predates the first, and the second one's untouched facts disagree with the
 *  property the first just wrote: the patch dutifully writes the disagreement,
 *  silently undoing it.
 *
 *  So a display edit states only what it changed. The ONE exception is the
 *  grouping when the VIEW changes AND the canonical key is not there yet: the
 *  legacy `tine.group-by` token and the `(group-by …)` directive mean different
 *  things at different views, so a switch has to pin the meaning the block had
 *  even though the effective grouping is the same on both sides — the whole
 *  point of the canonical key. Once that key IS on the block it is
 *  view-independent, there is nothing left to pin, and re-stating it from a
 *  stale reading is the very clobber above.
 *
 *  Leaving the rest alone is also the stricter reading of I-4: a column edit is
 *  not a licence to rewrite how a note spells its grouping.
 *
 *  A SAVE keeps the wide `queryViewPropertyPatch` baseline instead: its reprint
 *  destroys facts that live only in the query text, and only the block's own
 *  properties can say which those are. The narrower rule here is correct exactly
 *  because a display edit reprints nothing. */
export function queryDisplayPropertyWrites(input: {
  /** The view this edit started from — the engine's reading the surface that
   *  made the change was rendered with. */
  before: ViewSettings;
  view: ViewSettings;
  properties: PropertyPairs;
}): QueryPropertyWrite[] {
  const pinLegacyGrouping =
    !sameFact("view", input.before, input.view)
    && firstProperty(input.properties, QUERY_GROUP_FIELD_PROPERTY) === undefined;
  const allowed = new Set<string>();
  for (const fact of Object.keys(DISPLAY_FACT_KEYS) as DisplayFact[]) {
    const changed =
      !sameFact(fact, input.before, input.view) || (fact === "grouping" && pinLegacyGrouping);
    if (changed) for (const key of DISPLAY_FACT_KEYS[fact]) allowed.add(key);
  }
  return queryViewPropertyPatch({ view: input.view, properties: input.properties }).filter(
    ([key]) => allowed.has(propertyKeyNorm(key)),
  );
}

function sameGrouping(a: QueryGroupingResolution, b: QueryGroupingResolution): boolean {
  if (a.kind !== b.kind) return false;
  return a.kind !== "field" || a.field === (b as { field: string }).field;
}

function sameStrings(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((item, index) => item === b[index]);
}

function sameSort(
  a: readonly [Field, "asc" | "desc"][],
  b: readonly [Field, "asc" | "desc"][],
): boolean {
  return a.length === b.length
    && a.every(([field, dir], index) => b[index][0] === field && b[index][1] === dir);
}

/** **Editing the recognized aggregates while leaving everything else alone.**
 *
 *  `tine.col-aggregates` is shared ground: the query reader understands a bare
 *  `count` and `key=count|sum|avg`, and the sheet's own footer understands a
 *  seventeen-name vocabulary the query knows nothing about. Rewriting the whole
 *  value from the query's list would silently delete a table-only `estimate=
 *  median`, so the merge is positional:
 *
 *   * recognized segments are replaced in place, in new-list order;
 *   * surplus recognized slots are removed;
 *   * remaining new entries are appended;
 *   * unrecognized slots keep their text and their relative order.
 *
 *  Returns `undefined` for "no write needed" — including the case where the
 *  recognized list is unchanged, where the raw value is preserved byte for byte
 *  rather than reformatted. A value that holds only unrecognized settings is
 *  not empty metadata: it is never deleted just because the query has no
 *  aggregates. */
export function mergeQueryAggregateValue(
  raw: string | null,
  next: readonly [Field, AggFn][],
): string | null | undefined {
  const segments = raw == null ? [] : raw.split(";");
  const recognized: number[] = [];
  const parsed: [Field, AggFn][] = [];
  const unrecognized: number[] = [];
  segments.forEach((segment, index) => {
    const entry = parseQueryAggregateSegment(segment);
    if (entry) {
      recognized.push(index);
      parsed.push(entry);
    } else if (segment.trim()) {
      unrecognized.push(index);
    }
  });
  const unchanged = parsed.length === next.length
    && parsed.every(([field, fn], i) => next[i][0] === field && next[i][1] === fn);
  // No aggregate change and nothing to migrate: the raw value survives byte for
  // byte, including its spacing and any table-only segments.
  if (unchanged) return undefined;

  const out: string[] = [];
  let taken = 0;
  for (let index = 0; index < segments.length; index += 1) {
    if (recognized.includes(index)) {
      if (taken < next.length) out.push(serializeQueryAggregate(next[taken]));
      taken += 1;
      continue;
    }
    if (unrecognized.includes(index)) out.push(segments[index]);
  }
  for (let i = taken; i < next.length; i += 1) out.push(serializeQueryAggregate(next[i]));
  const value = out.join(";");
  return value === (raw ?? "") ? undefined : (value || null);
}
