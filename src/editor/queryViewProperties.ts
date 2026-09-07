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
  "tine.group-by",
  "tine.columns",
  "tine.col-aggregates",
  "tine.sample",
] as const;

/** The typed sheet schema property. Read here only to RECOGNISE a pre-split
 *  bare column list; its typed form is never written or interpreted by this
 *  module. */
export const QUERY_SCHEMA_PROPERTY = "tine.fields";
export const QUERY_COLUMNS_PROPERTY = "tine.columns";

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
   *  wherever it came from. */
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

  // `tine.group-by` — one bare name.
  const group = view.group_by ?? "";
  if ((current("tine.group-by") ?? "").trim() !== group) push("tine.group-by", group);

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
