# Contract — query display settings

What a query block's `tine.*` display properties **mean**, and who is allowed to
write them. Kept true by same-commit updates and by the semantic tests named
under each section. Decision history for the presentation-versus-membership
boundary lives in `docs/adr/0030-query-view-unification.md`, which this contract
succeeds for the property grammar without replacing its historical decision.

P5A settled the property grammar and the column resolution. P5B settled the
grouping identity, the inline Display panel, the one summary behind every
grouped face, and the query table's own header/footer/drag surfaces; they are
documented here together rather than in a second file. What remains open is
listed in the last section.

Implementation:

- `crates/tine-core/src/query/view.rs` — the resolvers, and the §4.1 precedence
  merge. The authority.
- `crates/tine-core/src/publish.rs` — the static publisher, which **calls** the
  resolver above rather than carrying a second precedence.
- `src/editor/queryViewProperties.ts` — the TypeScript adapter and the save
  patch, shared by `Macro.tsx` and `SheetTable.tsx`.
- `src/sheet/renameField.ts` — the aggregate-preserving field rename.
- `src/editor/queryAggregate.ts` — `querySummary`, the ONE fold behind every
  grouped or aggregated query face.
- `src/components/QueryDisplay.tsx` — the inline Display panel. It edits
  `ViewSettings` and never a property.
- `src/components/SheetTable.tsx` — the query table's header sort, header
  reorder and footer, all routed through the same writer.
- `src/sheet/fields.ts` — the field-identity helpers those share
  (`queryColumnName`, `querySortFieldName`, `boardGroupByOptions`,
  `groupKeysForBlock`).

## 1. The six display facts, and the one that is not

A query block persists exactly six display facts, all under `tine.`:

| Property | Value |
| --- | --- |
| `tine.view` | `search` / `list` / `table` / `board` |
| `tine.sort` | `<field> <asc\|desc>[; …]`; a segment with no direction sorts ascending |
| `tine.group-field` | one canonical sheet field id, or empty for "no grouping" |
| `tine.columns` | ordered visible column names, `;`-separated |
| `tine.col-aggregates` | `count`, or `<key>=<count\|sum\|avg>`, `;`-separated |
| `tine.sample` | a `u32` |

**`tine.fields` is not one of them.** It is the typed sheet schema
(`name=type`), it belongs to the sheet, and a query save never writes it — with
exactly one narrow exception (§4). Conflating the two is the defect this
contract exists to prevent: writing a column list into `tine.fields` destroyed a
declared schema on every filter save, and declaring a schema destroyed the
column list from the other direction.

`tine.*` is render-hidden by prefix (`src/render/block.ts::isRenderHiddenProp`),
so none of these keys needs a hidden-property registration and none of them
surfaces as prose. `src/sheet/conversions.ts::TINE_SHEET_PROPS` deliberately does
**not** list `tine.columns`: `stripTineSheetProps` serves the markdown-grid
pipe-table conversion, which is children-backed only.

**`tine.group-by` is the ambiguous predecessor of `tine.group-field`.** It is
still read for compatibility and retired on the first save that states the
grouping, and nothing writes it (§3, §4).

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

## 3. The grouping field — the exact resolution

One function answers "which field does this query group by", for the same three
consumers §2 has: `query::view::resolve_query_grouping`. The §4.1 merge itself
calls it, so every `ViewSettings` that reaches the app or the publisher already
carries the canonical answer. **No component re-resolves it** — `Macro.tsx`
reads `view().group_by`, and resolving a second time would prefix an already
canonical `prop:status` again.

**Why a new key.** `tine.group-by` carried one bare token with two meanings:
`state` meant the task marker to the board and an ordinary property named
`state` to the list grouper, and a board's `status` fell through `isFieldId` and
silently became the task marker too. A note had no way to say which it meant, so
switching the view changed what it grouped by. `tine.group-field` takes a
**canonical sheet field id** and has exactly one reading.

**The canonical grammar,** applied after trimming: one of the six builtins
`state`, `priority`, `scheduled`, `deadline`, `tags`, `page`, or `prop:` /
`formula:` with a nonempty suffix. `;` is legal — this is one field, not a list
— but NUL, CR and LF are not, because such a value would not read back off a
property line. Anything else is not a value to guess at; it is an explicit
no-grouping statement.

**The precedence:**

1. `tine.group-field` **present** → its own answer, and nothing behind it.
   - a token in the canonical grammar → that field;
   - **empty or outside the grammar** → *cleared*: no grouping, and no legacy
     key or directive behind it.
2. `tine.group-field` **absent** → a nonempty `tine.group-by`, read as a
   **legacy** token at the view the block is CURRENTLY persisted with.
3. Otherwise the `(group-by …)` directive the parser lifted, read the same way.
4. Otherwise *unset*.

**The legacy token's meaning** depends on that current view, and both readings
are preserved rather than corrected:

- a **sheet face** (table or board): a builtin keeps its own identity; `prop:x`,
  `formula:x` and the app's alternate spelling `formula.x` are sheet fields; and
  **every other bare name is now an ordinary property** — the one deliberate
  correction, because `status` becoming the task marker was never anything the
  author asked for;
- a **list or search face**: `page` is the source page, and every other token is
  an EXACT property key, `state` and a literal `prop:` prefix included. That is
  what `queryAggregate.ts::groupRows` has always done.

The view used is the block's own `tine.view` when it is readable, otherwise the
text's view, otherwise list. It is never the view being switched TO: an existing
list grouped by a property named `state` keeps that property through a switch to
Board, because the save states the canonical identity before the switch can
reinterpret it (§4).

*Cleared* and *unset* both mean "this note names no grouping field", and the
Board is the one face that tells them apart: *unset* is the silence its ADR 0030
default of `state` fills — in the app and in the publisher alike, which is what
a `tine.view:: board` note with no grouping key has always shown — while
*cleared* is the user having said no out loud and draws ONE ungrouped column.
Every other face renders both ungrouped. That is the whole reason a clear is a
present **empty** value rather than a removal: otherwise turning grouping off on
a board would come back on the next view switch. On the wire the three states
are `group_by: "<field>"`, `group_by: ""`, and absent — the IR shape is
unchanged, and `Option<Field>` still carries it. `QueryGroupingControl` carries
the same distinction to the board as `field` plus `cleared`, because a bare
`FieldId | null` cannot.

**Cross-language pinning,** for the reason §2 gives: one corpus,
`crates/tine-core/tests/fixtures/query-grouping/resolution.json`, read by
`crates/tine-core/tests/query_grouping_resolution.rs` and by
`src/editor/queryViewProperties.test.ts`. A second test asserts the corpus still
covers every frozen branch, so it cannot be quietly trimmed to the easy cases.

Tests: the two above, `crates/tine-core/src/query/view.rs`
(`the_merge_returns_the_canonical_grouping_field_id`,
`the_new_group_key_wins_and_an_empty_one_is_an_explicit_clear`),
`src/editor/queryViewProperties.emptyProperty.test.ts`.

## 4. What a query save writes

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
- For grouping, the baseline is the full resolution of §3 — **taken under the
  view this save leaves behind**, with `tine.view` spelled out rather than left
  to a stale reading. A note that still spells its grouping the legacy way and
  gets an unrelated filter edit writes nothing; a save that does change the
  effective grouping states it canonically AND retires a nonempty
  `tine.group-by` in the same patch and the same undo unit. Together those two
  make a view switch safe: if the untouched legacy token would read differently
  at the destination view, the comparison fails and the canonical identity is
  written first.
- An explicit "no grouping" is written as the **empty value**, not as a removal
  (§3). Both property writers keep an empty value and both readers read it back
  as present.
- The query TEXT is deliberately not part of the grouping or aggregate baseline.
  `og_view` never re-emits `(group-by …)` at all, so a directive-only grouping is
  exactly the fact a reprint destroys, and the first save that touches the block
  states it as a property.
- Re-picking the setting a block already has produces an empty patch, so it
  leaves no undo entry to step back through.
- Every key that is not one of the six is left exactly as it was:
  `tine.fields`, `tine.table-widths`, `tine.col-widths`, `tine.header`,
  `tine.filter`, `tine.formula.*`, and anything unrecognized.

**The one exception.** When a save states the columns and `tine.fields` holds a
value **proven** to be a pre-split bare column list, that value is removed in the
same patch and the same undo unit: it has no reader left, and leaving it would
let a cleared selection come back through the legacy branch. A typed or mixed
schema is never a candidate.

**A DISPLAY edit is narrower than a save.** A save reprints the query text and
therefore has to materialize whatever that reprint would drop. A display edit —
the panel, a header sort, a column drag, the footer, the board's grouping —
reprints nothing (sort and sample excepted, which go through the ordinary save),
so it has nothing to materialize, and `queryDisplayPropertyWrites` narrows the
patch to the facts that edit actually CHANGED.

That narrowing is a correctness requirement, not a tidiness one (I-20). Every
display surface renders from the ENGINE's last reading, and the engine re-reads
asynchronously: two clicks inside one parse round-trip both start from the
reading that predates the first, so restating that reading's untouched facts
against the block's now-newer properties writes the first click straight back
out. Clearing the grouping and then switching the view did exactly that.

The one fact a view switch states even when its own value did not change is the
grouping, and only while `tine.group-field` is still absent: the legacy
`tine.group-by` token and the `(group-by …)` directive mean different things at
different views, so the switch has to pin the meaning the block had. Once the
canonical key is on the block it is view-independent, there is nothing left to
pin, and restating it would be the same clobber.

A **save** keeps the wide baseline: its reprint destroys facts that live only
in query text. The frontend pairs each parse result with its input request. If
properties changed since that reading, the save parses the current properties
and applies only the intended view changes over that fresh result, then uses the
ordinary full-view writer. A changed query argument requires a fresh edit rather
than rebasing a filter over different text. Before writing, it checks the block's
raw revision captured at save start; a concurrent change during printing preserves
the newer block and displays a retry message. The rapid clear-then-sample and
pending-printer regressions in `QueryMacro.test.tsx` pin these boundaries.
Controls use the last successfully persisted view while reparsing, keyed to the
exact block bytes and graph epoch. This prevents two rapid list additions from
collapsing into one; another block revision invalidates that derived display
state. Execution continues to use the backend reading, never this control state.

**Column clearing, and what is behind it.** The writer removes `tine.columns`
rather than emptying it, while §2's resolver distinguishes a present-but-empty
value from an absent one. That asymmetry is safe, and both halves are pinned:
the legacy `tine.fields` branch is retired by the same patch under exactly the
condition that branch would have read it, and the query text has no columns to
resurrect — no dialect lifts a column set out of a query and no printer emits
one, which `query_columns_resolution.rs::
no_dialect_lifts_a_column_set_out_of_the_query_text` asserts across all five
inputs rather than leaving it to a comment. Grouping is the opposite case, and
is written as the empty value, because what sits behind it really can come back.

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
`src/editor/queryViewProperties.test.ts`,
`src/components/QueryDisplayConsumers.test.tsx`.

## 5. `tine.col-aggregates` is shared ground

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
- **The key grammar is its own** (`sheet/fields.ts::queryAggregateFieldName`),
  and it is deliberately not the columns grammar. An aggregate key is a LITERAL
  property name: `prop:` and `formula:` are ordinary characters there, and a
  property named `state`, `page` or `tags` is an ordinary thing to count or sum.
  `tine.columns` reserves those six names because a bare token there selects the
  builtin; nothing in this property does, because no builtin has a key here at
  all. What is refused is the grammar's own punctuation — `;`, `=`, CR/LF/NUL —
  plus an empty key (which already means the keyless whole-result count) and a
  padded key, which the reader's `trim` would hand back as another property's
  name. Only ordinary properties have a key: builtins and formulas carry none,
  so those columns show no footer aggregate rather than a segment whose meaning
  depends on who reads it. Every surface that offers or reads a key — the
  panel's `+ property` vocabulary and the query table's footer — asks this one
  function; before it, both asked `queryColumnName`, and a note saying
  `state=count` about an ordinary property named `state` was rendered by nothing
  and editable by no one.
- **What the merge keeps, the editor shows.** `retainedQueryAggregateSegments`
  lists the segments the query reader does not own, through the SAME
  `parseQueryAggregateSegment` the merge uses — a segment is retained exactly
  when the merge copies it through, so the two cannot drift and there is no
  second parser. They reach the panel as `QueryDisplayControl.retainedAggregates`
  (read from the block's own property bytes by the host, since the engine's
  reading never carries them), and Summarize states them read-only: *"Kept from
  the table, not editable here: estimate=median"*. Preservation the author
  cannot see is indistinguishable from loss.

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
`src/editor/queryViewProperties.test.ts` (`the aggregate segment merge`, `the
segments the panel reports as retained`), `src/sheet/fields.test.ts`
(`queryAggregateFieldName`), `src/components/QueryDisplay.test.tsx`,
`src/components/QueryDisplayConsumers.test.tsx`,
`src/components/QueryMacro.test.tsx`.

## 6. One summary for every grouped or aggregated face

The complete summary is rendered above each inline presentation, including Table,
Board, and builder-backed Search. Table footers are additional column controls;
they do not replace or truncate that ordered summary. Board defaults are resolved
with the existing view-default helper for both controls and summary grouping,
without writing properties on open.

Query aggregate keys are literal property names, including names starting with
`prop:` or `formula:`. These prefixes have no special meaning in this grammar.
`QuerySummary.test.tsx` pins distinct values for both prefixed names and the bare name.

`queryAggregate.ts::querySummary` is the only fold. It takes the rows, the
ordered aggregate list, an optional group-key function and a value reader, and
returns one cell per requested aggregate — for the whole result and for each
group. It replaced `foldAggregate` and `groupRows`, whose callers shipped two
defects:

- they folded `aggregates[0]` and dropped the rest, so a note asking for
  `count; cost=sum` rendered one number, whichever the property happened to
  spell first;
- the list face and the board face grouped through different code, and could
  disagree about which group a row was in.

The **writers** are a separate matter. `queryBuilder.ts::currentSort` /
`currentAgg` still read `sort[0]` / `aggregates[0]` and `withSort` / `withAgg`
still write a one-element list; they are the `+ sort` and `+ summarize` pills,
which remain the only display controls on the faces the panel is not offered on
(§7). Those faces are out of scope here and are listed as open below. The
rendering defect is fixed everywhere; the one-entry WRITE survives exactly where
those pills do.

Grouping keys come from `sheet/fields.ts::groupKeysForBlock` over the same DTO
rows the Board renders, and formula columns through `readFormulaRowField`, so
the list summary, the board columns and the table footer cannot drift apart.
`tags` places a row in every tag's group, exactly as the board does; the summary
reports that (`multiMembership`) rather than pretending the group counts
partition the result. A row with no value for the grouping field falls into
`(none)`, which stays distinct from a group whose value is literally `(none)`.

The arithmetic is what shipped, unchanged: `count` counts rows; `sum` and `avg`
`parseFloat` each value and skip what is not numeric, with `avg` dividing by the
numeric contributors rather than by the row count; results round to three
decimals; and the number of skipped rows is carried rather than folded away.
Repeated aggregate entries render repeatedly, in the requested order, because
that is what the property spells.

Tests: `src/editor/queryAggregate.test.ts`, `src/components/QuerySummary.test.tsx`.

## 7. The inline Display panel

Display remains accessible when the filter sheet is closed. The same control is
mounted at the resting query or in the open sheet, never both simultaneously.
Its open state and the sheet's open state share one registry gate: neither open
means no registry read; either open uses the same scope/revision cache. The
closed-sheet access and zero-at-rest regression is in `QueryMacro.test.tsx`.


`QueryDisplay.tsx` edits all six facts in one place, as what they are: two
enums, three ordered lists and a number.

- It is an **opt-in capability**. `QueryBuilder` takes `inlineDisplay`,
  defaulting to false, and every existing caller keeps the `+ sort` /
  `+ summarize` pills it had. `Macro.tsx` turns it on only where a builder is
  shown and the block is not a friendly search — exactly where the full display
  vocabulary is meaningful.
- It **never writes a property**. It computes the next `ViewSettings` and hands
  it to `QueryDisplayControl.apply`, which is the host's one writer (§4). That
  is what keeps the entries it cannot represent — an unrecognized aggregate
  segment, a sort field it has no picker for — intact.
- Sort, Columns and Summarize are ordered lists with move and remove per row, so
  the second entry is editable rather than invisible. Summarize additionally
  states the `tine.col-aggregates` segments it keeps but does not own, read-only
  (§5) — the one place the aggregates are edited is the one place their
  retention has to be legible.
- Its `+ property` vocabulary offers only keys the slot's own grammar can carry:
  the columns picker hides a property that would read back as a builtin, the
  sort picker hides what `sort_key` cannot order by, and the aggregate picker
  hides only what the segment grammar cannot spell (§5) — a control that looks
  like it saved is worse than an absent one.
- A view switch goes through `viewAfterViewSwitch`, the single place the Board
  default is applied, and only over an *unset* grouping (§3). The view switcher
  in the macro toolbar routes through the same function.
- The Sample field states what it read: empty is no limit, **zero is a real
  limit** and means no rows, and a number past `u32` is refused by the control
  rather than silently by the parse.
- It mounts through the same portalled transient-layer shell as the other query
  popovers (`registerTransientLayer` + `dismissOnOutsidePointer`), with the
  600px bottom-sheet layout, and its field pickers register as visible popovers
  parented to the panel — so dismissing the panel dismisses them with it.
- **It is the one popover on the sheet's rung that is not rendered inside the
  sheet.** The host sheet decides whether a press is "outside" by asking the DOM
  under its own element whether a popover is open; a portalled panel is not
  there, so every press inside it read as a press outside the sheet, the sheet
  closed, the panel went with it, and the control's own click never landed. The
  panel therefore stamps its parent layer's id on its root
  (`data-transient-parent`) and the sheet's check looks for exactly that. A
  `click()` in jsdom never showed this; a real pointer sequence does, which is
  why the native journey exists.
- It is placed below its trigger when it fits and above it when it does not,
  clamped to the viewport on both axes. It does not scroll the page, so anything
  past an edge is unreachable rather than merely off-screen.
- **Its field pickers are portalled out of it, for the same reason and one more.**
  The panel is a capped scroll box, and a scrolling ancestor clips an absolutely
  positioned popover: nested inside it, the vocabulary list laid its rows out at
  real positions, painted nowhere and answered no click — all four field choices
  (Group by, `+ sort`, `+ column`, `+ property`) with a DOM that read as
  perfectly correct. Each picker is now a zero-size fixed anchor placed by the
  same clamp the panel uses, stamped with the panel's layer id so a press inside
  it holds the panel — and the panel's sheet — still. Hit-testability, not
  presence in the DOM, is what `scripts/shot-query-display.mjs` asserts about it.

Tests: `src/components/QueryDisplay.test.tsx`, `src/components/QueryMacro.test.tsx`.

## 8. The query table's own surfaces

A query table's header, footer and header drag route to the SAME writer as the
panel; none of them owns a property.

- **Header sort.** A column the engine can sort by cycles ascending → descending
  → unsorted in the saved `tine.sort`, and the arrow shows the saved order,
  because that is the order the rows actually came back in. `sort_key`
  understands `priority`, `page`, `scheduled`, `deadline` and any property name
  — and nothing else. Title, `state`, `tags` and formula columns therefore keep
  the table's own transient arrangement, and the table says **"Table-only sort:
  &lt;column&gt;"** above the header, so "sorted" and "saved as sorted" are
  visibly different rather than discovered after a reload. A property named like
  a sortable builtin is not offered: the engine would sort by the builtin's
  meaning instead. A new result revision or a newly saved sort drops the local
  arrangement rather than re-applying it to rows nobody sorted — in a table
  that HAS a saved sort to be second to. A query-sourced table with no query
  display control (the tag page's reference table) has the local arrangement as
  its only sort, and keeps it across a refresh of its rows.
- **Header reorder.** Dropping a header writes `tine.columns` — never
  `tine.fields`, which is the typed schema and says nothing about order or
  visibility. `tine.columns` is a COMPLETE selection, not a hint: whatever it
  lists is what the table shows. So when a visible column has no name in the
  grammar — a formula column, or a property named like one of the six builtins
  — the order is **not** saved at all, and the table says which column it could
  not spell. Writing the order of the rest would not leave that column where it
  was; it would hide it. Coercing it into another field identity is never an
  option, and no new columns grammar is authorized here.
- **Footer.** A query column's footer cycles count / sum / average, edits
  `tine.col-aggregates` **in place** — the list is ordered and repeats are
  meaningful, so changing one column must not reshuffle the others — and reads
  its number through `querySummary` (§6), never through the sheet's `aggregate`.
  Only ordinary properties get a footer aggregate: a builtin's bare name and a
  query aggregate key are the same bytes but not the same thing, and a formula
  has no key at all. A property *named* like a builtin is not a builtin and does
  get one — the footer asks the aggregate grammar (§5), not the columns
  grammar, which reserves those names for a reason that does not apply here.
- **Board grouping.** The toolbar dropdown and the context menu both call one
  `QueryGroupingControl.set`. `field: null` carries a `cleared` flag beside it,
  because it is two answers: an EXPLICIT clear is one ungrouped column, while a
  grouping nothing states anywhere is the silence ADR 0030's task-marker default
  fills — which is what a note authored as `tine.view:: board` with no grouping
  key has always shown, in the app and in the publisher alike. Its options are
  the fields the RESULT rows actually carry, plus the source page and the
  block's formulas —
  `boardGroupByOptions` now accepts the caller's row set, and passing nothing
  keeps a children board's list byte for byte. A query board's grouping is the
  query's `tine.group-field`, so neither surface reaches for a property writer
  of its own, which is how the two used to disagree.

Tests: `src/components/QueryDisplayConsumers.test.tsx`,
`src/components/QueryColumns.test.tsx`.

## 9. Publishing

The static publisher renders a query-backed sheet face from the same resolution
and the same precedence, and preserves declared type rendering (checkbox cells,
enum order) for the columns it shows. Ordinary children-backed sheets and the
flat query list are unchanged.

Its board face **calls** `resolve_query_grouping` rather than carrying a second
precedence, so a published board groups by the field the app shows: the same
canonical key, the same view-aware legacy reading, the same explicit clear as one
ungrouped column, and the same ADR 0030 default when nothing states a grouping. A
children board keeps its own `tine.group-by` reading: that key is the sheet's
there, not the query's, and P5B does not touch it.

**One known gap, inherited and not introduced here.** The publisher resolves the
grouping from the block's properties alone (`ViewSettings::default()` for the
parsed half), so step 3 of §3 — a grouping that lives only in the query text's
`(group-by …)` directive — is not read there, and such a board publishes with the
default instead. The static path never had the parsed view in hand and never
read that directive before P5B either; closing it means parsing the query source
inside `publish.rs`, which is outside this packet's write set. The first save
that touches such a block materializes the directive as `tine.group-field` (§4),
after which the two agree.

Tests: `crates/tine-core/src/publish.rs`
(`publish_query_table_shows_the_selected_columns_in_the_selected_order`,
`publish_query_table_reads_a_legacy_bare_field_list_as_columns`,
`publish_query_table_treats_a_present_empty_columns_list_as_no_selection`,
`publish_children_sheet_ignores_a_query_column_selection`).

## 10. Not settled here

Deliberately open, and owned by the next package rather than guessed forward:

- the workspace draft's presentation and its format-aware materialization;
- session/UI caps on a column selection;
- a saved sort by title, `state`, `tags` or a formula: that needs an engine sort
  vocabulary, not a frontend replacement sorter, and P5B deliberately shows the
  limit (§8) rather than papering over it with a second answer;
- `formula:` columns in `tine.columns`: they have no name in that grammar, so a
  table showing one cannot save its column order at all (§8). A grammar that
  could name them is a new columns grammar, which P5B is not authorized to add;
- the publisher's step-3 grouping: a `(group-by …)` that lives only in the query
  text is not read by `publish.rs`, which resolves from block properties alone
  (§9);
- a saved grouping on a **children**-backed sheet, which still reads its own
  `tine.group-by` (§9);
- the `+ sort` and `+ summarize` pills. They are the display controls on the
  faces `inlineDisplay` is not enabled for — the friendly-search face and the
  workspace draft — and they still write a one-element sort and a one-element
  aggregate list through `withSort` / `withAgg`. Reading is already correct
  there (§6); replacing the writers belongs with whoever settles those faces.

Manager review pin: legacy-schema recognition applies only to query tables;
ordinary children tables keep their existing schema interpretation. A schema
write that also rescues query columns captures both the schema page and query
page in the same undo unit, including an enclosing header reorder. Tests:
`QueryColumns.test.tsx` ordinary-children and cross-page-undo cases.
