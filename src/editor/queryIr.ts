// **The TypeScript mirror of the Rust query IR** (SPEC §3.1, §7.1).
//
// One query language, defined once, in `crates/tine-core/src/query/ir.rs`. This
// file is the JSON shape of that value as it crosses Tauri — nothing more. It
// declares no semantics, parses no text and prints no text: `query_parse`,
// `query_print` and `query_og_expressible` are the only things that produce or
// interpret an IR, and they all live in Rust (D-14 — the frontend's second
// grammar is what this packet exists to remove).
//
// ## Why it is a hand-written mirror and how it is kept honest
//
// The wire format is internally tagged on `kind`, in `snake_case`, and is fixed
// by §3.1 — a lane may not change it on either side. Because this file is
// hand-written, the danger is not that it disagrees loudly but that it agrees
// SILENTLY: a variant Rust added and TypeScript never learned about is, to
// `JSON.parse`, just an object, and it flows through the app as `never` until
// something reads a field that is not there.
//
// So the mirror is CHECKED, not asserted. `crates/tine-core/tests/fixtures/
// query-ir/*.json` are the golden wire fixtures the Rust side already
// round-trips; `src/editor/queryIr.test.ts` reads the same files from here and
// requires every variant in them to be constructible and exhaustively handled by
// this module's own visitor. A mirror that quietly ignored an unknown variant is
// the exact defect that test exists to catch.
//
// Spans are UTF-16 code-unit offsets into the ORIGINAL source text, converted
// once at the Rust boundary (`Span::from_byte_range`) precisely because the
// consumer is JavaScript.

import type { PageKind, RefGroup } from "../types";
import type { FriendlyPageMatchScope, QueryDisplayDraft } from "./queryDisplayDraft";

// --------------------------------------------------------------------------
// Scalars
// --------------------------------------------------------------------------

/** A source span, in UTF-16 code units into the original source text. */
export interface Span {
  start: number;
  end: number;
}

/** The row type a query selects. Required in the IR and enforced by Tine (Q4). */
export type Anchor = "block" | "page";
export const ANCHORS: readonly Anchor[] = ["block", "page"];

/** The attributes of a block row, of a page row, and the three attributes of the
 *  elements relations yield. Which set is legal is decided by the ROW the leaf
 *  sits on, never by a second enum (I-12). */
export type Attr =
  // block row
  | "content" | "task" | "priority" | "scheduled" | "deadline"
  // page row
  | "name" | "journal" | "day" | "namespace"
  // property element
  | "key" | "value" | "atom_count";
export const ATTRS: readonly Attr[] = [
  "content", "task", "priority", "scheduled", "deadline",
  "name", "journal", "day", "namespace",
  "key", "value", "atom_count",
];

/** One relation of the anchor row. Bare identifiers inside a relation predicate
 *  bind to the ELEMENT, never to the outer row (§3.2). */
export type Rel = "refs" | "tags" | "props" | "children" | "blocks" | "page";
export const RELS: readonly Rel[] = ["refs", "tags", "props", "children", "blocks", "page"];

/** OData §5.1.1.13 quantifiers: `any` is false and `every` true on an empty
 *  collection (Q5). */
export type Quant = "any" | "none" | "every";
export const QUANTS: readonly Quant[] = ["any", "none", "every"];

/** The complete comparison vocabulary (§3.3, M19). */
export type CmpOp =
  | "eq" | "not_eq" | "lt" | "le" | "gt" | "ge"
  | "between" | "in" | "not_in"
  | "like" | "starts_with"
  /** Full-text match on `content` — what today's `(search "…")` head means. */
  | "match"
  /** A Rust regex over `content` — Tine's `(content-regex "…")`. Never OG-expressible. */
  | "regex"
  | "is_set" | "is_not_set" | "is_blank";
export const CMP_OPS: readonly CmpOp[] = [
  "eq", "not_eq", "lt", "le", "gt", "ge",
  "between", "in", "not_in",
  "like", "starts_with", "match", "regex",
  "is_set", "is_not_set", "is_blank",
];

/** Whether this operator takes the `none` operand — and only that one (C1). */
export function isPresenceOp(op: CmpOp): boolean {
  return op === "is_set" || op === "is_not_set" || op === "is_blank";
}

/** Why a query is (partly) not understood. Every kind names an in-scope
 *  scenario: unknown vocabulary, malformed input, or an I-22 refusal. */
export type DiagnosticKind =
  | "unknown_head" | "syntax" | "unknown_ident" | "not_applicable" | "depth" | "size";
export const DIAGNOSTIC_KINDS: readonly DiagnosticKind[] = [
  "unknown_head", "syntax", "unknown_ident", "not_applicable", "depth", "size",
];

// --------------------------------------------------------------------------
// Values, leaves, the boolean tree
// --------------------------------------------------------------------------

/** A comparison operand. `date` carries the UNRESOLVED literal (`-7d`, `today`,
 *  `2026-09-04`): resolution happens at evaluation time from the execution's own
 *  `today`, so a cached IR never pins a day. */
export type Value =
  | { kind: "text"; text: string }
  | { kind: "number"; number: number }
  | { kind: "date"; literal: string }
  | { kind: "bool"; bool: boolean }
  | { kind: "list"; items: Value[] }
  /** The operand of `is_set` / `is_not_set` / `is_blank` and of nothing else. */
  | { kind: "none" };

/** A self-contained test on the current row. */
export type Leaf =
  /** A comparison on one of the current row's own attributes. */
  | { kind: "attr"; attr: Attr; op: CmpOp; value: Value }
  /** A quantifier over ONE relation of the current row. */
  | { kind: "rel"; rel: Rel; quant: Quant; pred: Filter };

/** The boolean tree. Identity elements are explicit: after normalization `and([])`
 *  is `true` and `or([])` is `false`. */
export type Filter =
  | { kind: "and"; items: Filter[] }
  | { kind: "or"; items: Filter[] }
  | { kind: "not"; inner: Filter }
  | { kind: "leaf"; leaf: Leaf }
  /** Q12: present, round-trips, structurally omitted at evaluation (§3.5). */
  | { kind: "off"; inner: Filter }
  /** An unparsed or unknown span. **Always paired with a diagnostic**, and
   *  lossless by contract (§4.3.2, R4): `text` is the exact payload the author
   *  wrote and `diagnostic_kind` the diagnostic that rejected it. Both survive
   *  every save, reopen and neighbouring edit, as the `raw_hex(…)` capsule.
   *  The wire name is `diagnostic_kind` because `kind` is already the tag. */
  | { kind: "raw"; text: string; diagnostic_kind: DiagnosticKind; span?: Span }
  | { kind: "true" }
  | { kind: "false" };

export interface Diagnostic {
  span?: Span;
  message: string;
  suggestions?: string[];
  /** Set for a diagnostic inside an `off` subtree: the row renders greyed with
   *  its message but does NOT invalidate the query (§3.5). */
  disabled?: boolean;
  kind: DiagnosticKind;
}

/** Where the query's text came from, and the bytes to preserve.
 *
 *  `original` is the exact form slice WITHOUT the trailing options map;
 *  `og_options` is that map INCLUDING its braces, verbatim and opaque — EDN
 *  comments and unknown keys and all. The map has exactly ONE owner, the Rust
 *  parser: nothing outside `query_parse` splits a query argument, and every macro
 *  printer re-appends it once (§3.1, X4, W2). */
export type Source =
  | { kind: "og"; original: string; og_options?: string }
  | { kind: "tql"; original: string; og_options?: string }
  /** Datalog. `original` is the COMPLETE authored advanced form, including
   *  `:query` / `:inputs` when present (§4.4) — a whole `{:query …}` map is the
   *  FORM, and only a map that FOLLOWS it is options. */
  | { kind: "advanced"; original: string; og_options?: string }
  /** Built in the UI: no authored text to preserve. */
  | { kind: "builder" };

/** The opaque trailing options map, or `""`. The ONE reader, so the map cannot be
 *  re-derived per dialect (I-12). Mirrors `Source::og_options`. */
export function sourceOptions(source: Source): string {
  return source.kind === "builder" ? "" : source.og_options ?? "";
}

/** The exact authored form slice, without the trailing options map, or null for a
 *  builder-authored query. Mirrors `Source::original`. */
export function sourceOriginal(source: Source): string | null {
  return source.kind === "builder" ? null : source.original;
}

// --------------------------------------------------------------------------
// View settings, bounds, the query
// --------------------------------------------------------------------------

export type SortDir = "asc" | "desc";
/** `search` is the existing fourth view (`Macro.tsx`), and it stays. */
export type ViewKind = "search" | "list" | "table" | "board";
export const VIEW_KINDS: readonly ViewKind[] = ["search", "list", "table", "board"];
export type AggFn = "count" | "sum" | "avg";

/** A sort/group/column/aggregate target: a property key or an OG-sortable field
 *  name, kept as the user wrote it. Serialized transparently as a bare string. */
export type Field = string;

/** Presentation. NEVER part of the filter (Q15): `sort-by`, `sample`, `aggregate`
 *  and `group-by` are lifted here on parse and re-emitted from here by the
 *  printers. `["", "count"]` is the whole-result count — today's fieldless
 *  `(aggregate count)` (X3). */
export interface ViewSettings {
  view?: ViewKind;
  sort?: [Field, SortDir][];
  group_by?: Field;
  columns?: Field[];
  aggregates?: [Field, AggFn][];
  sample?: number;
}

/** The two construction limits the result bridge already enforces, and nothing
 *  else. */
export interface Bounds {
  max_rows: number;
  max_bytes: number;
}

/** One query: an anchored filter plus the diagnostics and the authored source. */
export interface Query {
  anchor: Anchor;
  filter: Filter;
  diagnostics?: Diagnostic[];
  source: Source;
}

/** Page/block presentation state read from a saved query's scoped `tine.*`
 * properties. Every field is optional so an old `{query, view}` response keeps
 * exactly its old JSON shape. Unreadable authored settings are reported here,
 * independently of predicate diagnostics. */
export interface ScopedDisplaySettings {
  page_presentation?: ViewKind;
  page_display?: QueryDisplayDraft;
  block_presentation?: ViewKind;
  block_display?: QueryDisplayDraft;
  page_match_scope?: FriendlyPageMatchScope;
  unreadable_settings?: string[];
}

/** `query_parse`'s answer. `Query` and `ViewSettings` are SEPARATE values: the
 * filter never contains presentation (§3.1). Scoped display state is flattened
 * beside that unchanged pair. */
export interface ParsedQuery extends ScopedDisplaySettings {
  query: Query;
  view: ViewSettings;
}

/** A query with at least one ENABLED diagnostic is invalid: it returns zero
 *  results plus its diagnostics (§3.5). A diagnostic inside an `off` subtree
 *  carries `disabled: true` and does not invalidate. Mirrors `Query::is_invalid`. */
export function isInvalid(query: Query): boolean {
  return (query.diagnostics ?? []).some((d) => !d.disabled);
}

// --------------------------------------------------------------------------
// Results
// --------------------------------------------------------------------------

/** One `@page` result row. Needs no document load (K16). */
export interface PageRow {
  path: string;
  name: string;
  kind: PageKind;
  journal_day?: number;
  properties: [string, string][];
}

/** The advanced-query report (M5): OG/TQL sources report an empty `ignored` and
 *  `supported: true`. */
export interface QueryReport {
  ran?: string[];
  ignored?: string[];
  supported: boolean;
}

/** The anchor-specific result rows (§7.1, K16), flattened onto the result. */
export type QueryStatisticsMarker = "non_finite" | "division_by_zero" | "empty_group" | "non_numeric";
export type QueryStatisticsCell =
  | { kind: "number"; value: number; skipped: number }
  | { kind: "marker"; reason: QueryStatisticsMarker; skipped: number };
export type QueryStatisticsGroup = { key: string | null; count: number; cells: QueryStatisticsCell[] };
export type QueryStatistics = {
  count: number; aggregates: [Field, AggFn][]; group_by: Field | null;
  overall: QueryStatisticsCell[]; groups: QueryStatisticsGroup[] | null;
  grouping_status: "none" | "exact" | "unsupported_formula";
};

export function isQueryStatistics(value: unknown): value is QueryStatistics {
  if (!value || typeof value !== "object") return false;
  const v = value as Record<string, unknown>;
  const count = (n: unknown) => typeof n === "number" && Number.isSafeInteger(n) && n >= 0;
  const cell = (c: unknown): boolean => {
    if (!c || typeof c !== "object") return false;
    const x = c as Record<string, unknown>;
    if (!count(x.skipped)) return false;
    if (x.kind === "number") return Object.keys(x).length === 3 && typeof x.value === "number" && Number.isFinite(x.value);
    return x.kind === "marker" && Object.keys(x).length === 3 &&
      ["non_finite", "division_by_zero", "empty_group", "non_numeric"].includes(x.reason as string);
  };
  if (!count(v.count) || !Array.isArray(v.aggregates) || !v.aggregates.every((a) =>
    Array.isArray(a) && a.length === 2 && typeof a[0] === "string" && ["count", "sum", "avg"].includes(a[1]))) return false;
  const cells = (a: unknown) => Array.isArray(a) && a.length === (v.aggregates as unknown[]).length && a.every(cell);
  if (!(v.group_by === null || typeof v.group_by === "string") || !cells(v.overall)) return false;
  if (v.grouping_status === "none" || v.grouping_status === "unsupported_formula") return v.groups === null;
  return v.grouping_status === "exact" && Array.isArray(v.groups) && v.groups.every((g) =>
    g && typeof g === "object" && (g.key === null || typeof g.key === "string") && count(g.count) && cells(g.cells));
}

export type QueryResult = {
  statistics?: QueryStatistics;
  diagnostics?: Diagnostic[];
  report: QueryReport;
  total: number;
  /** Exact complete count when the backend proved one for this row kind. */
  matched_total?: number;
  exceeded: boolean;
} & (
  | { anchor: "block"; groups: RefGroup[] }
  | { anchor: "page"; pages: PageRow[] }
);

/** The runtime inputs an execution binds against (§4.4, §7.1, R5).
 *
 *  It exists because an advanced query's answer is a function of WHERE and WHEN
 *  it runs, not only of its text. An absent `current_page` is NOT "unknown,
 *  guess": `?current-page` simply has no binding and the clause that needs it
 *  stays unsupported. The execution DAY is deliberately not a field — the
 *  resolver snapshots it once per execution. */
export interface ExecutionContext {
  current_page?: string;
}

/** One row of `query_explain_empty` (Q14, N19): a top-level conjunct, the anchor
 *  rows matching it ALONE, and the rows matching all the OTHERS without it. */
export interface EmptyExplanation {
  conjunct: string;
  alone: number;
  without?: number;
}

/** `query_explain_empty`'s complete answer.
 *
 *  The rows alone were not enough: when execution-time resolution fails there are
 *  no honest counts to report, and an empty row list without the diagnostics and
 *  the support report would read as "every conjunct matches nothing" instead of
 *  "this query was never bound". */
export interface ExplainEmptyResult {
  rows: EmptyExplanation[];
  diagnostics?: Diagnostic[];
  report: QueryReport;
}

// --------------------------------------------------------------------------
// The property registry (§6.1)
// --------------------------------------------------------------------------

export type ObservedType = "text" | "number" | "date" | "checkbox" | "ref";
export type Cardinality = "one" | "many";

export interface RegistryRow {
  normalized_name: string;
  cardinality: Cardinality;
  observed_type: ObservedType;
  count_blocks: number;
  count_pages: number;
  /** Atom counts per class. */
  histogram?: [ObservedType, number][];
  mismatch_count: number;
  declared?: [ObservedType, Cardinality];
  /** At most eight, by count. */
  top_values?: [string, number][];
}

export interface RegistrySnapshot {
  rows: RegistryRow[];
  generation: number;
}

// --------------------------------------------------------------------------
// The exhaustive visitor — the mirror's own proof that it knows every variant
// --------------------------------------------------------------------------

/** Thrown when a value arrives carrying a `kind` this mirror does not know.
 *
 *  **This is the point of the whole module.** An unknown variant must be LOUD:
 *  silently treating it as an opaque object is how a frontend ends up rendering
 *  a query it cannot represent, and then saving that misreading back over the
 *  author's bytes. */
export class UnknownIrVariantError extends Error {
  constructor(readonly where: string, readonly value: unknown) {
    super(
      `Unknown query IR variant in ${where}: ${JSON.stringify(value)}.\n`
      + `The Rust IR (crates/tine-core/src/query/ir.rs) has a shape this mirror `
      + `(src/editor/queryIr.ts) does not know. Add it to BOTH, and to the golden `
      + `fixtures in crates/tine-core/tests/fixtures/query-ir/ that pin the pair.`,
    );
    this.name = "UnknownIrVariantError";
  }
}

/** Walk every node of a filter tree, depth-first, in wire order.
 *
 *  Exhaustive by construction: an unrecognised `kind` throws
 *  `UnknownIrVariantError` rather than being skipped. Every consumer that needs
 *  to know "what is in this query" goes through here, so there is one answer to
 *  that question rather than one per component (I-12). */
export function forEachFilter(filter: Filter, visit: (node: Filter) => void): void {
  visit(filter);
  switch (filter.kind) {
    case "and":
    case "or":
      for (const item of filter.items) forEachFilter(item, visit);
      return;
    case "not":
    case "off":
      forEachFilter(filter.inner, visit);
      return;
    case "leaf":
      forEachLeafFilter(filter.leaf, visit);
      return;
    case "raw":
    case "true":
    case "false":
      return;
    default:
      throw new UnknownIrVariantError("Filter", filter);
  }
}

function forEachLeafFilter(leaf: Leaf, visit: (node: Filter) => void): void {
  switch (leaf.kind) {
    case "attr":
      assertKnownValue(leaf.value);
      return;
    case "rel":
      forEachFilter(leaf.pred, visit);
      return;
    default:
      throw new UnknownIrVariantError("Leaf", leaf);
  }
}

function assertKnownValue(value: Value): void {
  switch (value.kind) {
    case "text":
    case "number":
    case "date":
    case "bool":
    case "none":
      return;
    case "list":
      for (const item of value.items) assertKnownValue(item);
      return;
    default:
      throw new UnknownIrVariantError("Value", value);
  }
}

/** Validate a whole parsed query against this mirror, throwing on the first
 *  variant it does not know. The type-mirror consistency test runs this over
 *  every golden fixture; callers that have just received a `query_parse` answer
 *  can run it too — it is a cheap tree walk, not a parse. */
export function assertMirrorsIr(query: Query): void {
  switch (query.source.kind) {
    case "og":
    case "tql":
    case "advanced":
    case "builder":
      break;
    default:
      throw new UnknownIrVariantError("Source", query.source);
  }
  if (!ANCHORS.includes(query.anchor)) {
    throw new UnknownIrVariantError("Anchor", query.anchor);
  }
  for (const diagnostic of query.diagnostics ?? []) {
    if (!DIAGNOSTIC_KINDS.includes(diagnostic.kind)) {
      throw new UnknownIrVariantError("DiagnosticKind", diagnostic.kind);
    }
  }
  forEachFilter(query.filter, (node) => {
    if (node.kind !== "leaf") return;
    const leaf = node.leaf;
    if (leaf.kind === "attr") {
      if (!ATTRS.includes(leaf.attr)) throw new UnknownIrVariantError("Attr", leaf.attr);
      if (!CMP_OPS.includes(leaf.op)) throw new UnknownIrVariantError("CmpOp", leaf.op);
      return;
    }
    if (!RELS.includes(leaf.rel)) throw new UnknownIrVariantError("Rel", leaf.rel);
    if (!QUANTS.includes(leaf.quant)) throw new UnknownIrVariantError("Quant", leaf.quant);
  });
}

// --------------------------------------------------------------------------
// Command dialects (SPEC §7.1, §4.3)
// --------------------------------------------------------------------------

/** What `query_parse` is being handed.
 *
 *  `macro_query` / `macro_tql` take the COMPLETE raw macro argument and are the
 *  ONLY inputs that split a trailing options map — once, in Rust. `og`, `tql` and
 *  `advanced` are explicit form inputs; `advanced` also serves the existing
 *  `#+BEGIN_QUERY` container extractor. There is no speculative
 *  parse-and-fallback: the caller states the form, or names the macro and lets
 *  the one Rust discriminator decide. */
export type QueryTextDialect = "og" | "tql" | "advanced" | "macro_query" | "macro_tql";

/** The printed form a `query_print` caller wants (§4.3).
 *
 *  `tql` is the text pane's editing layout — filter and anchor only, never
 *  options or view directives. `tql_macro` is the persisted single-line
 *  `{{tine-query …}}` form, `advanced_macro` a `{{query [:find …]}}` printed from
 *  its authored source, and `og` the legacy DSL, which is PARTIAL and therefore
 *  the only dialect that can refuse. */
export type QueryPrintDialect = "og" | "tql" | "tql_macro" | "advanced_macro";

/** The parse dialect for a macro of this name (§7.1): `{{query …}}` carries OG
 *  or advanced text, `{{tine-query …}}` carries TQL. The ONE mapping, so a
 *  caller cannot pick a dialect that contradicts the name it is about to write. */
export function macroTextDialect(macroName: string): QueryTextDialect {
  return macroName.toLowerCase() === "tine-query" ? "macro_tql" : "macro_query";
}

/** The print dialect that re-emits a macro of this name. */
export function macroPrintDialect(macroName: string): QueryPrintDialect {
  return macroName.toLowerCase() === "tine-query" ? "tql_macro" : "og";
}

/** The print dialect that re-emits the query the way it was AUTHORED.
 *
 *  This is a different question from {@link macroPrintDialect}, which answers
 *  "what dialect does this macro NAME print in". A `{{query …}}` macro can hold
 *  either the OG DSL or advanced datalog, and only the parse knows which — so a
 *  source-preserving re-emit (`preserveForm`) has to pick the dialect off the
 *  source variant, or Rust refuses the pair as mismatched. A builder query has
 *  no authored form at all; `og` is returned so the caller takes the ordinary
 *  lowering path, where `queryOgExpressible` decides. */
export function sourcePrintDialect(source: Source): QueryPrintDialect {
  switch (source.kind) {
    case "advanced":
      return "advanced_macro";
    case "tql":
      return "tql_macro";
    case "og":
    case "builder":
      return "og";
  }
}
