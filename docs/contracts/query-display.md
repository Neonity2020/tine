# Contract — query display settings

What a query block's `tine.*` display properties **mean**, and who is allowed to
write them. Kept true by same-commit updates and by the semantic tests named
under each section. Decision history for the presentation-versus-membership
boundary lives in `docs/adr/0030-query-view-unification.md`, which this contract
succeeds for the property grammar without replacing its historical decision.

This file documents the **implemented foundation** (P5A). The Display UI, the
grouping ruling, the query-table interaction rules and the workspace policy are
not settled here; they land with their own package and extend this file.

Implementation:

- `crates/tine-core/src/query/view.rs` — the resolvers, and the §4.1 precedence
  merge. The authority.
- `crates/tine-core/src/publish.rs` — the static publisher, which **calls** the
  resolver above rather than carrying a second precedence.
- `src/editor/queryViewProperties.ts` — the TypeScript adapter and the save
  patch, shared by `Macro.tsx` and `SheetTable.tsx`.
- `src/sheet/renameField.ts` — the aggregate-preserving field rename.

## 1. The six display facts, and the one that is not

A query block persists exactly six display facts, all under `tine.`:

| Property | Value |
| --- | --- |
| `tine.view` | `search` / `list` / `table` / `board` |
| `tine.sort` | `<field> <asc\|desc>[; …]`; a segment with no direction sorts ascending |
| `tine.group-by` | one bare field name |
| `tine.columns` | ordered visible column names, `;`-separated |
| `tine.col-aggregates` | `count`, or `<key>=<count\|sum\|avg>`, `;`-separated |
| `tine.sample` | a `u32` |

**`tine.fields` is not one of them.** It is the typed sheet schema
(`name=type`), it belongs to the sheet, and a query save never writes it — with
exactly one narrow exception (§3). Conflating the two is the defect this
contract exists to prevent: writing a column list into `tine.fields` destroyed a
declared schema on every filter save, and declaring a schema destroyed the
column list from the other direction.

`tine.*` is render-hidden by prefix (`src/render/block.ts::isRenderHiddenProp`),
so none of these keys needs a hidden-property registration and none of them
surfaces as prose. `src/sheet/conversions.ts::TINE_SHEET_PROPS` deliberately does
**not** list `tine.columns`: `stripTineSheetProps` serves the markdown-grid
pipe-table conversion, which is children-backed only.

Tests: `crates/tine-core/src/query/view.rs` (module tests),
`src/editor/queryViewProperties.test.ts`, `src/components/QueryColumns.test.tsx`.

## 2. Visible columns — the exact resolution

One function answers "which columns does this query show", for all three
consumers: `query::view::resolve_query_columns`. It reads **normalized** property
keys and takes the **first** occurrence of a key, as every other property reader
here does.

**The token grammar,** applied to a whole property value: trim the value, split
on `;`, trim each token, discard empty segments. A token containing `=`, NUL, CR
or LF invalidates the **entire** list, not just itself. There is no per-name
length cap — these are bytes an outside editor may have authored, and refusing a
long but well-formed name would drop a column its author can see in their own
file. Session and UI caps are a different boundary and are not this grammar's
business.

**The precedence:**

1. `tine.columns` **present** → its own answer, and nothing behind it.
   - a readable, non-empty list → those columns, in that order, spelling
     retained;
   - **empty or invalid** → *cleared*: no custom columns, **no** legacy
     fallback and **no** query-text columns. A present value is an explicit
     statement, which is what makes clearing final rather than a way to
     resurrect an older list.
2. `tine.columns` **absent** → `tine.fields` is read as a **legacy** column list,
   but only when every nonempty token passes the same grammar and at least one
   exists. `=` anywhere means the value is a schema or a mixed value, and is
   never columns.
3. Neither → *unset*: whatever the query text itself carried stands.

*Cleared* and *unset* differ only in what they suppress behind them. Both render
the **default** column set.

The legacy branch is compatibility for **authored notes**, not a private-state
migration: D-1 governs Tine's own private formats, and these are the user's own
Markdown/Org bytes. Reading a legacy list never rewrites the note. Opening,
parsing, rendering, publishing and rebuilding write nothing.

**At the renderer,** a column name maps to a field identity: the six builtins
`state`, `priority`, `scheduled`, `deadline`, `tags`, `page` keep their own
identity, and every other string is an ordinary property name and becomes
`prop:<name>`. The mapping lives at the renderer, in
`src/sheet/fields.ts::queryColumnFieldId` and
`publish.rs::sheet_field_for_column`; the property bytes stay the bare name the
author wrote.

Selection is applied **after** the schema and type lookup, so a shown column
keeps the type its `tine.fields` declaration gave it, and a hidden column keeps
its definition — hiding a column is not deleting a field. A selected column that
no row carries is still a column and renders empty cells. The title column and
the action column are outside the selection and stay reachable. A renderer may
deduplicate identical field ids without rewriting the source.

Children-backed sheets are not a query face and ignore `tine.columns` entirely.

**Cross-language pinning.** Rust has one implementation; TypeScript has an
adapter, because rendering a table is a synchronous walk over blocks already in
memory and cannot take an IPC round-trip per block (the same reason
`queryMacro.ts` transcribes the raw extent reader). The pair is pinned by one
corpus, `crates/tine-core/tests/fixtures/query-columns/resolution.json`, read by
`crates/tine-core/tests/query_columns_resolution.rs` and by
`src/editor/queryViewProperties.test.ts`. If the two ever disagree, one of those
tests goes red.

## 3. What a query save writes

`Macro.tsx`'s save path computes its writes through
`queryViewPropertyPatch`, and `store.ts::setBlockProperty` remains the only
side-effect owner.

**The baseline is what the block's properties currently SPELL — never "which
control the user touched".** The two readings differ exactly where it matters:
the OG printer re-emits only `(sort-by …)` and `(sample …)`, so a grouping or an
aggregate authored in the query text is dropped by the reprint of an unrelated
**filter** edit, and only a property write keeps it. Comparing against the
persisted value materializes precisely those facts. A crossing to
`{{tine-query}}`, whose text carries no directives at all, materializes the whole
effective view for the same reason and in the same undo unit.

Consequences that are load-bearing:

- A fact the property already spells is **not rewritten** merely to reformat it.
- A cleared setting removes its stale property, and the reprint — through the
  existing backend printer, which is handed the new view — does not put the
  directive back.
- For columns, the baseline is the full resolution of §2, legacy branch included.
  So an unrelated filter edit on a pre-split note writes nothing, while a real
  column change states the new list.
- Every key that is not one of the six is left exactly as it was:
  `tine.fields`, `tine.table-widths`, `tine.col-widths`, `tine.header`,
  `tine.filter`, `tine.formula.*`, and anything unrecognized.

**The one exception.** When a save states the columns and `tine.fields` holds a
value **proven** to be a pre-split bare column list, that value is removed in the
same patch and the same undo unit: it has no reader left, and leaving it would
let a cleared selection come back through the legacy branch. A typed or mixed
schema is never a candidate.

**The mirror case, in the sheet.** `SheetTable`'s `schemaHome` treats a proven
bare column list as *no schema*, exactly as if the property were absent — reading
it as a schema made the home non-null over an empty parse, which marked every
column a stray and disabled header reordering. When the user declares a schema
over such a list on a **query** face, the list is rescued into `tine.columns`
first, in one `withUndoUnit` with the declaration. It is rescued **only** when
`tine.columns` is absent: a present value, including a present empty or invalid
one, is an explicit statement and its presence wins. Page-versus-block schema
ownership is unchanged.

Tests: `src/components/QueryColumns.test.tsx`,
`src/components/QueryMacro.ir.test.tsx` (`B6: directive migration`),
`src/editor/queryViewProperties.test.ts`.

## 4. `tine.col-aggregates` is shared ground

Two readers use this one property, and neither owns it:

- the **query** reader (`view.rs::parse_col_aggregates`) understands a bare
  `count` — the whole-result count, with no `=` — and `<key>=<count|sum|avg>`.
  Its entries are an ordered **list**, and repeated keys are meaningful;
- the **sheet** footer (`sheet/config.ts`, `sheet/aggregate.ts`) understands a
  seventeen-name vocabulary keyed into a `Map`, which has no spelling for a
  keyless entry and collapses repeats.

Therefore:

- A query's aggregates are **never** serialized through the sheet's `Map`
  serializer. `serializeQueryAggregates` is array-based.
- A query save **merges** rather than rewrites: recognized segments are replaced
  in place, in new-list order; surplus recognized slots are removed; remaining
  new entries are appended; unrecognized slots keep their text and their relative
  order. If the recognized list is unchanged, the raw value is preserved byte for
  byte. A value holding only unrecognized settings is not empty metadata and is
  never deleted because the query has no aggregates.
- `avg` is **not** added to `AggregateFn` / `isAggregateFn` / `applyAggregate` /
  the publisher's `SHEET_AGGREGATE_FNS`. The sheet has no implementation for it,
  and a "valid" sheet aggregate with no implementation is worse than an
  unrecognized one.

**Field rename** (`renameField.ts::rewriteAggregateValue`) renames only an exact
`prop:<oldName>` key — bare query keys and `formula:` keys are outside the
sheet's rename ownership. It accepts a bare `count`, recognizes `key=avg` without
claiming the sheet can execute it, retains repeated keys and their order, and
preserves unrecognized segments verbatim. It refuses only where the rename is
genuinely ambiguous: an unparseable segment that names the field being renamed,
and a key differing from `prop:<oldName>` only by case. Its preservation proof is
an **ordered segment comparison** admitting exactly the intended key
substitution — the previous `Map`-size comparison proved nothing about the
segments the rename actually touches. Native query-result rename remains out of
scope: `planSheetFieldRename` still refuses `rowSource !== "children"`.

Tests: `src/sheet/renameField.test.ts`,
`src/editor/queryViewProperties.test.ts` (`the aggregate segment merge`).

## 5. Publishing

The static publisher renders a query-backed sheet face from the same resolution
and the same precedence, and preserves declared type rendering (checkbox cells,
enum order) for the columns it shows. Ordinary children-backed sheets and the
flat query list are unchanged.

Tests: `crates/tine-core/src/publish.rs`
(`publish_query_table_shows_the_selected_columns_in_the_selected_order`,
`publish_query_table_reads_a_legacy_bare_field_list_as_columns`,
`publish_query_table_treats_a_present_empty_columns_list_as_no_selection`,
`publish_children_sheet_ignores_a_query_column_selection`).

## 6. Not settled here

Deliberately open, and owned by the next package rather than guessed forward:

- the `state` collision — one bare `tine.group-by` token means the task marker to
  the board and an ordinary property to the list grouper. No grouping behaviour
  changes in this foundation;
- where a Display control mounts, and what it offers per query source kind;
- query-table header sort routing, and what the `title` and `formula:` headers do;
- `ContextMenu`'s `tine.group-by` writes on query boards;
- the workspace draft's presentation and its format-aware materialization;
- session/UI caps on a column selection.

Manager review pin: legacy-schema recognition applies only to query tables;
ordinary children tables keep their existing schema interpretation. A schema
write that also rescues query columns captures both the schema page and query
page in the same undo unit, including an enclosing header reorder. Tests:
`QueryColumns.test.tsx` ordinary-children and cross-page-undo cases.
