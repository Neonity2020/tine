// The visual builder's model — **over the IR, not over text** (SPEC §7.1, §7.4).
//
// ## What this file no longer does, and why that is the point
//
// It used to hold a second query language: a tokenizer, a `parseQuery` that read
// the OG DSL into a private `Clause` tree, a `toDsl`/`clauseDsl` that printed it
// back, and a `clauseToAdvanced`/`advancedToClause` pair that converted between
// the DSL and datalog. Four of those five were TWINS of `crates/tine-core/src/
// query/` — the same questions ("what does this text mean", "what text says
// this") answered a second time, in a second language, by a second author. They
// disagreed exactly where it hurt: the frontend's `(task)` meant "no markers",
// the engine's meant "any open task"; the frontend's `sort-by` defaulted one way
// and OG's the other. **I-12: one question, one canonical answer.** After this
// packet exactly one implementation prints a query, and it is in Rust.
//
// What survives here is what was never a twin: the immutable TREE EDITS a chip
// bar performs (add / remove / replace / wrap / unwrap / flip), the VIEW edits
// the sort and summarize controls perform, and the human PHRASE for a leaf. Those
// are the builder's own questions — the engine has no opinion about what happens
// when you click "Wrap in OR".
//
// ## The leaf constructors
//
// §7.4 makes the builder responsible for the SHAPE of the leaf a picker emits
// ("the builder's *starts with* / *contains* / *ends with* rows emit `like`
// patterns"). The shapes below are the ones `crates/tine-core/src/query/og.rs`
// builds for the same user intent, so a chip added here and a query typed there
// are the same IR and print to the same text. Each constructor names the `og.rs`
// function it matches; the round trip (add a chip → print → re-parse → same IR)
// is what pins them.

import { MARKERS as TASK_MARKERS } from "../markers";
import type {
  AggFn,
  Attr,
  CmpOp,
  Field,
  Filter,
  Leaf,
  Rel,
  SortDir,
  Value,
  ViewSettings,
} from "./queryIr";

// ---------------------------------------------------------------------------
// Vocabulary the pickers offer
// ---------------------------------------------------------------------------

/** Which date a `between` row tests against. The unqualified form is OG's
 *  journal-only predicate; `any` is Tine's broader extension and must stay
 *  explicit so loading and saving a query never changes its membership. */
export type BetweenField = "any" | "journal" | "scheduled" | "deadline";
export const BETWEEN_FIELDS: BetweenField[] = ["journal", "scheduled", "deadline", "any"];

/** The full task-marker set (src/markers.ts) as a mutable array for the picker. */
export const MARKERS: string[] = [...TASK_MARKERS];
export const PRIORITIES = ["A", "B", "C"];

/** The builder renders a tree this deep and then says so rather than recursing
 *  without bound. §7.4 retunes this to the presentation cap of 3 in P3; until
 *  then it stays where it has always been, so no currently-rendering query
 *  starts truncating. */
export const MAX_QUERY_BUILDER_DEPTH = 64;

/** The filter shapes the add-picker can build and the edit popover can re-collect.
 *  A NAME for a leaf shape, not a second IR: every one maps onto the `og.rs`
 *  construction of the same name. */
export type BuilderLeafKind =
  | "page"
  | "task"
  | "priority"
  | "property"
  | "scheduled"
  | "deadline"
  | "journal"
  | "between"
  | "onPage"
  | "namespace"
  | "pageProperty"
  | "pageTags"
  | "content"
  | "search";

// ---------------------------------------------------------------------------
// Leaf constructors — the IR shapes `og.rs` builds for the same intent
// ---------------------------------------------------------------------------

const attr = (a: Attr, op: CmpOp, value: Value): Filter => ({
  kind: "leaf",
  leaf: { kind: "attr", attr: a, op, value },
});
const rel = (r: Rel, pred: Filter): Filter => ({
  kind: "leaf",
  leaf: { kind: "rel", rel: r, quant: "any", pred },
});
const textList = (items: string[]): Value => ({
  kind: "list",
  items: items.map((text) => ({ kind: "text", text }) as Value),
});
/** `og.rs::through_page`: a page-row test read through the block's owning page. */
const throughPage = (pred: Filter): Filter => rel("page", pred);

/** `og.rs::escape_like`. Transcribed (D-9) rather than shared, because the
 *  builder has to produce the pattern synchronously as the user commits a chip
 *  and there is no text to hand the parser. `%`, `_` and `\` in the user's words
 *  are DATA — an unescaped `50%` row would silently match everything after the
 *  `50`. `og::plain_like_substring` is the exact inverse the OG printer applies,
 *  so the round trip only holds while these two agree; `queryBuilder.test.ts`
 *  pins the escaping. */
export function escapeLike(text: string): string {
  return text.replace(/[%_\\]/g, (ch) => `\\${ch}`);
}

/** `[[x]]` / `#x` — `og.rs` `page-ref` / `Filter::page_ref` (Q2). */
export function pageRefFilter(name: string): Filter {
  return rel("refs", attr("name", "eq", { kind: "text", text: name }));
}

/** `(task …)` — `og.rs` `"task" | "todo"`. An empty pick is OG's open-task set,
 *  which is the shipped reading the corpus depends on. */
export function taskFilter(markers: string[]): Filter {
  const picked = markers.length ? markers : ["TODO", "DOING", "NOW", "LATER"];
  return attr("task", "in", textList(picked));
}

/** `(priority …)` — `og.rs` `"priority"`. */
export function priorityFilter(levels: string[]): Filter {
  return attr("priority", "in", textList(levels.length ? levels : [...PRIORITIES]));
}

/** `(property k)` / `(property k v)` — `og.rs::property_leaf`, the one shape
 *  §3.3 defines. */
export function propertyFilter(key: string, value: string | null): Filter {
  const keyTest = attr("key", "eq", { kind: "text", text: key });
  const pred: Filter =
    value == null || value === ""
      ? keyTest
      : { kind: "and", items: [keyTest, attr("value", "eq", { kind: "text", text: value })] };
  return rel("props", pred);
}

/** `(page-property …)` — the same predicate read through the page. */
export function pagePropertyFilter(key: string, value: string | null): Filter {
  return throughPage(propertyFilter(key, value));
}

/** `(page-tags …)` — `og.rs` `"page-tags" | "tags"`: the page's `tags` property
 *  with a set test on its atoms. */
export function pageTagsFilter(tags: string[]): Filter {
  return throughPage(
    rel("props", {
      kind: "and",
      items: [attr("key", "eq", { kind: "text", text: "tags" }), attr("value", "in", textList(tags))],
    }),
  );
}

/** `(scheduled)` / `(deadline)` — presence, never OG-expressible (§3.3 B4). */
export function planningFilter(which: "scheduled" | "deadline"): Filter {
  return attr(which, "is_set", { kind: "none" });
}

/** `(journal)` — `og.rs` `"journal"`. */
export function journalFilter(): Filter {
  return throughPage(attr("journal", "eq", { kind: "bool", bool: true }));
}

/** `(page x)` — `og.rs` `"page"`. */
export function onPageFilter(name: string): Filter {
  return throughPage(attr("name", "eq", { kind: "text", text: name }));
}

/** `(namespace x)` — recursive membership: the normalized page name starts with
 *  `x/` (§3.2 M20), `og.rs` `"namespace"`. */
export function namespaceFilter(ns: string): Filter {
  return throughPage(attr("name", "starts_with", { kind: "text", text: `${ns}/` }));
}

/** `og.rs::bounded`: two bounds are a `between`, one bound is the one-sided
 *  comparison, none is plain presence. Bounds stay UNRESOLVED in the IR. */
function boundedFilter(which: Attr, start: string, end: string): Filter {
  const low = start.trim();
  const high = end.trim();
  if (low && high) {
    return attr(which, "between", {
      kind: "list",
      items: [
        { kind: "date", literal: low },
        { kind: "date", literal: high },
      ],
    });
  }
  if (low) return attr(which, "ge", { kind: "date", literal: low });
  if (high) return attr(which, "le", { kind: "date", literal: high });
  return attr(which, "is_set", { kind: "none" });
}

/** `(between [field] start end)` — `og.rs::between`. `any` expands to the
 *  faithful three-way `or`, exactly as the parser does. */
export function betweenFilter(field: BetweenField, start: string, end: string): Filter {
  switch (field) {
    case "journal":
      return throughPage(boundedFilter("day", start, end));
    case "scheduled":
      return boundedFilter("scheduled", start, end);
    case "deadline":
      return boundedFilter("deadline", start, end);
    case "any":
      return {
        kind: "or",
        items: [
          throughPage(boundedFilter("day", start, end)),
          boundedFilter("scheduled", start, end),
          boundedFilter("deadline", start, end),
        ],
      };
  }
}

/** A bare quoted string — `og.rs::content_like`: a case-insensitive substring
 *  test on the block's visible content. */
export function contentFilter(text: string): Filter {
  return attr("content", "like", { kind: "text", text: `%${escapeLike(text)}%` });
}

/** `(search "…")` — the friendly-search grammar, `og.rs` `"search"`. */
export function searchFilter(source: string): Filter {
  return attr("content", "match", { kind: "text", text: source });
}

// ---------------------------------------------------------------------------
// Recognizers — which builder shape, if any, an IR node is
// ---------------------------------------------------------------------------

function asAttrLeaf(filter: Filter): (Leaf & { kind: "attr" }) | null {
  return filter.kind === "leaf" && filter.leaf.kind === "attr" ? filter.leaf : null;
}
function asRelLeaf(filter: Filter, which: Rel): (Leaf & { kind: "rel" }) | null {
  return filter.kind === "leaf" && filter.leaf.kind === "rel" && filter.leaf.rel === which
    ? filter.leaf
    : null;
}
function textOf(value: Value): string | null {
  return value.kind === "text" ? value.text : null;
}
function listOf(value: Value): string[] | null {
  if (value.kind !== "list") return null;
  const out: string[] = [];
  for (const item of value.items) {
    const text = textOf(item);
    if (text == null) return null;
    out.push(text);
  }
  return out;
}
function dateOf(value: Value): string | null {
  return value.kind === "date" ? value.literal : null;
}

/** The `props` predicate's key and optional single atom test, mirroring
 *  `Filter::props_key` / `props_atom_test` on the Rust side. */
function propsParts(pred: Filter): { key: string; atom: Filter | null } | null {
  const items = pred.kind === "and" ? pred.items : [pred];
  let key: string | null = null;
  let atom: Filter | null = null;
  for (const item of items) {
    const leaf = asAttrLeaf(item);
    if (leaf && leaf.attr === "key" && leaf.op === "eq") {
      const text = textOf(leaf.value);
      if (text == null || key != null) return null;
      key = text;
      continue;
    }
    if (atom != null) return null;
    atom = item;
  }
  return key == null ? null : { key, atom };
}

/** Which builder shape this filter is, or `null` for anything the pickers cannot
 *  re-collect. `null` is not an error: an unrecognised subtree still RENDERS
 *  (see {@link filterLabel}) — it just has no "Edit…" affordance. */
export function builderLeafKind(filter: Filter): BuilderLeafKind | null {
  const page = asRelLeaf(filter, "page");
  if (page) {
    const inner = builderLeafKind(page.pred);
    if (inner === "property") return "pageProperty";
    if (inner === "pageTags") return "pageTags";
    const leaf = asAttrLeaf(page.pred);
    if (leaf?.attr === "journal") return "journal";
    if (leaf?.attr === "name" && leaf.op === "eq") return "onPage";
    if (leaf?.attr === "name" && leaf.op === "starts_with") return "namespace";
    if (leaf?.attr === "day") return "between";
    return null;
  }
  const refs = asRelLeaf(filter, "refs");
  if (refs) {
    const leaf = asAttrLeaf(refs.pred);
    return leaf?.attr === "name" && leaf.op === "eq" ? "page" : null;
  }
  const props = asRelLeaf(filter, "props");
  if (props) {
    const parts = propsParts(props.pred);
    if (!parts) return null;
    const atom = parts.atom ? asAttrLeaf(parts.atom) : null;
    if (parts.key === "tags" && atom?.attr === "value" && atom.op === "in") return "pageTags";
    if (!parts.atom) return "property";
    return atom?.attr === "value" && atom.op === "eq" ? "property" : null;
  }
  const leaf = asAttrLeaf(filter);
  if (!leaf) return null;
  switch (leaf.attr) {
    case "task":
      return leaf.op === "in" ? "task" : null;
    case "priority":
      return leaf.op === "in" ? "priority" : null;
    case "scheduled":
    case "deadline":
      if (leaf.op === "is_set") return leaf.attr === "scheduled" ? "scheduled" : "deadline";
      return leaf.op === "between" || leaf.op === "ge" || leaf.op === "le" ? "between" : null;
    case "content":
      if (leaf.op === "like") return "content";
      return leaf.op === "match" ? "search" : null;
    default:
      return null;
  }
}

// ---------------------------------------------------------------------------
// Human-readable chip labels
// ---------------------------------------------------------------------------

const ATTR_PHRASE: Record<Attr, string> = {
  content: "text",
  task: "task",
  priority: "priority",
  scheduled: "scheduled",
  deadline: "deadline",
  name: "name",
  journal: "journal",
  day: "date",
  namespace: "namespace",
  key: "key",
  value: "value",
  atom_count: "values",
};
const OP_PHRASE: Record<CmpOp, string> = {
  eq: "is",
  not_eq: "is not",
  lt: "<",
  le: "≤",
  gt: ">",
  ge: "≥",
  between: "between",
  in: "is one of",
  not_in: "is none of",
  like: "contains",
  starts_with: "starts with",
  match: "matches",
  regex: "matches regex",
  is_set: "is set",
  is_not_set: "is not set",
  is_blank: "is blank",
};

function valuePhrase(value: Value): string {
  switch (value.kind) {
    case "text":
      return value.text;
    case "number":
      return String(value.number);
    case "date":
      return value.literal;
    case "bool":
      return value.bool ? "yes" : "no";
    case "list":
      return value.items.map(valuePhrase).join(" | ");
    case "none":
      return "";
  }
}

function betweenPhrase(value: Value): string {
  if (value.kind !== "list" || value.items.length !== 2) return valuePhrase(value);
  return `${dateOf(value.items[0]) ?? valuePhrase(value.items[0])} ~ ${dateOf(value.items[1]) ?? valuePhrase(value.items[1])}`;
}

/** The phrase for one filter node. §7.2's per-leaf phrase function in its P0
 *  form: friendly where the shape is one the pickers know, and an honest
 *  generic rendering everywhere else.
 *
 *  **Total by construction (the anti-Jira property, §7.5):** every query the
 *  parser accepts renders in the builder, invalid ones included — so this never
 *  returns "unsupported" and never throws. */
export function filterLabel(filter: Filter): string {
  switch (filter.kind) {
    case "and":
    case "or":
      return filter.kind.toUpperCase();
    case "not":
      return "NOT";
    case "off":
      return `${filterLabel(filter.inner)} (off)`;
    case "raw":
      return filter.text;
    case "true":
      return "everything";
    case "false":
      return "nothing";
    case "leaf":
      return leafLabel(filter, filter.leaf);
  }
}

function leafLabel(filter: Filter, leaf: Leaf): string {
  const kind = builderLeafKind(filter);
  if (leaf.kind === "rel") {
    const inner = asAttrLeaf(leaf.pred);
    const innerText = inner ? (textOf(inner.value) ?? "") : "";
    switch (kind) {
      case "page":
        return innerText;
      case "onPage":
        return `page: ${innerText}`;
      case "namespace":
        return `namespace: ${innerText.replace(/\/$/, "")}`;
      case "journal":
        return "on journal page";
      default:
        break;
    }
    if (leaf.rel === "page") {
      const inner = filterLabel(leaf.pred);
      return kind === "pageProperty" ? `page ${inner}` : inner;
    }
    if (leaf.rel === "props") {
      const parts = propsParts(leaf.pred);
      if (parts) {
        const atom = parts.atom ? asAttrLeaf(parts.atom) : null;
        if (parts.key === "tags" && atom?.op === "in") {
          return `page tags: ${(listOf(atom.value) ?? []).join(" | ")}`;
        }
        if (!parts.atom) return `${parts.key}: any`;
        if (atom?.op === "eq") return `${parts.key}: ${valuePhrase(atom.value)}`;
      }
    }
    return `${leaf.quant} ${leaf.rel}: ${filterLabel(leaf.pred)}`;
  }
  // An attribute leaf.
  switch (kind) {
    case "task":
      return `task: ${(listOf(leaf.value) ?? []).join(" | ") || "any"}`;
    case "priority":
      return `priority: ${(listOf(leaf.value) ?? []).join(" | ") || "any"}`;
    case "scheduled":
      return "scheduled";
    case "deadline":
      return "deadline";
    case "content":
      return `text: "${plainLikeSubstring(textOf(leaf.value) ?? "") ?? valuePhrase(leaf.value)}"`;
    case "search":
      return `search: ${valuePhrase(leaf.value)}`;
    default:
      break;
  }
  if (leaf.op === "between") {
    const field = leaf.attr === "day" ? "" : `${ATTR_PHRASE[leaf.attr]} `;
    return `${field}between: ${betweenPhrase(leaf.value)}`;
  }
  if (leaf.op === "is_set" || leaf.op === "is_not_set" || leaf.op === "is_blank") {
    return `${ATTR_PHRASE[leaf.attr]} ${OP_PHRASE[leaf.op]}`;
  }
  return `${ATTR_PHRASE[leaf.attr]} ${OP_PHRASE[leaf.op]} ${valuePhrase(leaf.value)}`;
}

/** The inverse of {@link escapeLike} for a pattern that is exactly `%<literal>%`
 *  — `og::plain_like_substring`, so a `contains` chip shows the words the user
 *  typed rather than the pattern the engine runs. */
export function plainLikeSubstring(pattern: string): string | null {
  if (!pattern.startsWith("%") || !pattern.endsWith("%") || pattern.length < 2) return null;
  const inner = pattern.slice(1, -1);
  let out = "";
  for (let i = 0; i < inner.length; i++) {
    const ch = inner[i];
    if (ch === "\\") {
      i += 1;
      if (i >= inner.length) return null;
      out += inner[i];
      continue;
    }
    if (ch === "%" || ch === "_") return null;
    out += ch;
  }
  return out;
}

// ---------------------------------------------------------------------------
// Sort presets — the one-click sort options in the bar
// ---------------------------------------------------------------------------

export interface SortPreset {
  field: string;
  dir: SortDir;
  label: string;
  hint: string;
}
export const SORT_PRESETS: SortPreset[] = [
  { field: "modified", dir: "desc", label: "Newest first", hint: "Most recent first — journal pages by their date, others by when the file was last modified" },
  { field: "modified", dir: "asc", label: "Oldest first", hint: "Oldest first — journal pages by their date, others by file modified time" },
  { field: "priority", dir: "asc", label: "Priority A→C", hint: "Highest priority ([#A]) first; unprioritized last" },
  { field: "page", dir: "asc", label: "Page A→Z", hint: "Alphabetically by the page each result lives on" },
  { field: "deadline", dir: "asc", label: "Deadline", hint: "Soonest DEADLINE first; blocks without a deadline last" },
  { field: "scheduled", dir: "asc", label: "Scheduled", hint: "Soonest SCHEDULED first; blocks without one last" },
];

/** Friendly text for a sort — a matching preset's label, else `field ↑/↓`. */
export function sortLabel(field: string, dir: SortDir): string {
  const preset = SORT_PRESETS.find((p) => p.field === field && p.dir === dir);
  if (preset) return preset.label.toLowerCase();
  return `${field} ${dir === "desc" ? "↓" : "↑"}`;
}

// ---------------------------------------------------------------------------
// View settings edits (§7.6 in its P0 form)
//
// `sort-by`, `aggregate`, `group-by` and `sample` are PRESENTATION, never part of
// the filter (§3.1, Q15). They used to ride in the clause tree as fake filter
// children — which is why wrapping one in an OR silently disabled it. Here they
// are edits on `ViewSettings`, and the printers re-emit them.
// ---------------------------------------------------------------------------

export function currentSort(view: ViewSettings): { field: Field; dir: SortDir } | null {
  const first = view.sort?.[0];
  return first ? { field: first[0], dir: first[1] } : null;
}

export function withSort(view: ViewSettings, sort: { field: Field; dir: SortDir } | null): ViewSettings {
  const next = { ...view };
  if (sort && sort.field.trim()) next.sort = [[sort.field.trim(), sort.dir]];
  else delete next.sort;
  return next;
}

export type AggState = { agg: AggFn; field: Field | null };

export function currentAgg(view: ViewSettings): AggState | null {
  const first = view.aggregates?.[0];
  if (!first) return null;
  const [field, agg] = first;
  return { agg, field: field === "" ? null : field };
}

export function withAgg(view: ViewSettings, agg: AggState | null): ViewSettings {
  const next = { ...view };
  // `["", "count"]` is the whole-result count — today's fieldless
  // `(aggregate count)` (X3). The empty field is the IR's own spelling for it,
  // not a missing value.
  if (agg) next.aggregates = [[agg.agg === "count" ? "" : (agg.field ?? ""), agg.agg]];
  else delete next.aggregates;
  return next;
}

export function currentGroup(view: ViewSettings): Field | null {
  return view.group_by ?? null;
}

export function withGroup(view: ViewSettings, field: Field | null): ViewSettings {
  const next = { ...view };
  if (field && field.trim()) next.group_by = field.trim();
  else delete next.group_by;
  return next;
}

// ---------------------------------------------------------------------------
// Immutable tree edits. `loc` is a path of child indices from the root.
// `[]` denotes the root itself.
//
// The IR spells a boolean node two ways — `and`/`or` carry `items`, `not`/`off`
// carry a single `inner` — so every edit goes through the one children accessor
// below rather than reaching into a field name. A second accessor is how a
// wrap-in-NOT would come to work on `and` and silently no-op on `not`.
// ---------------------------------------------------------------------------

/** The child list of a boolean node, or `null` for a leaf/raw/true/false. */
export function filterChildren(filter: Filter): Filter[] | null {
  switch (filter.kind) {
    case "and":
    case "or":
      return filter.items;
    case "not":
    case "off":
      return [filter.inner];
    default:
      return null;
  }
}

function withChildren(filter: Filter, children: Filter[]): Filter {
  switch (filter.kind) {
    case "and":
      return { kind: "and", items: children };
    case "or":
      return { kind: "or", items: children };
    case "not":
      return children[0] ? { kind: "not", inner: children[0] } : { kind: "and", items: [] };
    case "off":
      return children[0] ? { kind: "off", inner: children[0] } : { kind: "and", items: [] };
    default:
      return filter;
  }
}

/** The root the bar edits: always an `and`/`or` node, so "add a filter here" has
 *  somewhere to add. A `true` filter is the empty query; anything else is
 *  adopted as the single child of an `and`, which the OG printer collapses back
 *  to the bare child (`og_form`'s single-child rule). */
export function builderRoot(filter: Filter): Filter {
  if (filter.kind === "and" || filter.kind === "or") return filter;
  if (filter.kind === "true") return { kind: "and", items: [] };
  return { kind: "and", items: [filter] };
}

function clone(filter: Filter): Filter {
  return structuredClone(filter);
}

/** Resolve `loc` to the node that CONTAINS the addressed child, plus the index
 *  within it. `null` for the root or an invalid path. */
function locate(root: Filter, loc: number[]): { parent: Filter; children: Filter[]; idx: number } | null {
  if (loc.length === 0) return null;
  let node = root;
  for (let i = 0; i < loc.length - 1; i++) {
    const kids = filterChildren(node);
    if (!kids) return null;
    const next = kids[loc[i]];
    if (!next) return null;
    node = next;
  }
  const children = filterChildren(node);
  if (!children) return null;
  return { parent: node, children, idx: loc[loc.length - 1] };
}

/** Resolve `loc` to the node it addresses (`[]` = root). */
function nodeAt(root: Filter, loc: number[]): Filter | null {
  let node = root;
  for (const i of loc) {
    const kids = filterChildren(node);
    if (!kids) return null;
    const next = kids[i];
    if (!next) return null;
    node = next;
  }
  return node;
}

/** Drop empty `and`/`or` nodes anywhere except the root, so an edit never leaves
 *  a vacuous `and([])` behind that would match everything. */
function prune(filter: Filter): Filter | null {
  const children = filterChildren(filter);
  if (!children) return filter;
  if (filter.kind === "not" || filter.kind === "off") {
    const inner = prune(children[0]);
    return inner ? withChildren(filter, [inner]) : null;
  }
  const kids = children.map(prune).filter((x): x is Filter => x != null);
  if (kids.length === 0) return null;
  return withChildren(filter, kids);
}

function normalize(root: Filter): Filter {
  const children = filterChildren(root);
  if (!children) return root;
  const kids = children.map(prune).filter((x): x is Filter => x != null);
  // A `not`/`off` root has no enclosing position, so it becomes an `and` root
  // holding what survived — the same rule the old `Clause` normalizer used.
  if (root.kind === "not" || root.kind === "off") return { kind: "and", items: kids };
  return withChildren(root, kids);
}

/** Mutating a node the path does not address is a no-op that returns the input
 *  unchanged, so a stale `loc` from a popover that outlived its tree cannot
 *  corrupt the query. */
function edit(root: Filter, apply: (draft: Filter) => boolean): Filter {
  const draft = clone(root);
  return apply(draft) ? normalize(draft) : root;
}

/** Append `filter` to the boolean node addressed by `opLoc` (`[]` = root). */
export function addChild(root: Filter, opLoc: number[], filter: Filter): Filter {
  return edit(root, (draft) => {
    const node = nodeAt(draft, opLoc);
    const children = node ? filterChildren(node) : null;
    if (!node || !children || node.kind === "not" || node.kind === "off") return false;
    children.push(filter);
    return true;
  });
}

export function removeAt(root: Filter, loc: number[]): Filter {
  return edit(root, (draft) => {
    const at = locate(draft, loc);
    if (!at || !at.children[at.idx]) return false;
    at.children.splice(at.idx, 1);
    return true;
  });
}

export function replaceAt(root: Filter, loc: number[], filter: Filter): Filter {
  return edit(root, (draft) => {
    const at = locate(draft, loc);
    if (!at || !at.children[at.idx]) return false;
    at.children[at.idx] = filter;
    return true;
  });
}

/** Wrap the node at `loc` in a new boolean node. */
export function wrapAt(root: Filter, loc: number[], op: "and" | "or" | "not"): Filter {
  return edit(root, (draft) => {
    const at = locate(draft, loc);
    const current = at?.children[at.idx];
    if (!at || !current) return false;
    at.children[at.idx] =
      op === "not" ? { kind: "not", inner: current } : { kind: op, items: [current] };
    return true;
  });
}

/** Replace the boolean node at `loc` with its children spliced into the parent. */
export function unwrapAt(root: Filter, loc: number[]): Filter {
  return edit(root, (draft) => {
    const at = locate(draft, loc);
    const current = at?.children[at.idx];
    const kids = current ? filterChildren(current) : null;
    if (!at || !current || !kids) return false;
    at.children.splice(at.idx, 1, ...kids);
    return true;
  });
}

/** Change `and` ↔ `or` on the node addressed by `loc` (`[]` = root). */
export function setOp(root: Filter, loc: number[], op: "and" | "or"): Filter {
  if (loc.length === 0) {
    if (root.kind !== "and" && root.kind !== "or") return root;
    return normalize({ kind: op, items: structuredClone(filterChildren(root) ?? []) });
  }
  return edit(root, (draft) => {
    const at = locate(draft, loc);
    const current = at?.children[at.idx];
    if (!at || !current || (current.kind !== "and" && current.kind !== "or")) return false;
    at.children[at.idx] = { kind: op, items: filterChildren(current) ?? [] };
    return true;
  });
}
