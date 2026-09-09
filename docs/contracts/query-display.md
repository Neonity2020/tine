# Contract — query display settings

What a query block's `tine.*` display properties **mean**, and who is allowed to
write them. Kept true by same-commit updates and by the semantic tests named
under each section. Decision history for the presentation-versus-membership
boundary lives in `docs/adr/0030-query-view-unification.md`, which this contract
succeeds for the property grammar without replacing its historical decision.

P5A settled the property grammar and the column resolution. P5B settled the
grouping identity, the inline Display panel, the one summary behind every
grouped face, and the query table's own header/footer/drag surfaces; they are
documented here together rather than in a second file. §11 adds the one surface
with no block to carry properties at all — the virtual query workspace, whose
display draft lives on its route. What remains open is listed in §10.

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
- `src/editor/queryDisplayDraft.ts` — the workspace draft's ONE normalizer
  (§11), shared by `router.ts`'s mutation and by `session.ts`'s two directions,
  and the ONE scoped resolver behind §15 (`queryScopedDisplaySettings`, with its
  route and parsed-query adapters).
- `src/components/QueryResultSections.tsx` — the mixed result's two family
  boundaries, headings, control slots and states (§15.3).
- `src/components/QueryPageResults.tsx` — the Pages family's four presentations
  and its page-field vocabulary (§15.4).
- `src/components/QueryWorkspace.tsx` — `materializeQueryWorkspace`, the
  workspace's one publication path, and the captured-input guard around it
  (§13).
- `src/editor/properties.ts` — `markdownRawWithProperty` / `orgRawWithProperty`,
  the two pure raw-property writers `store.ts::setBlockProperty` already uses
  and the placement rule §13.2 applies to a block that has no store yet.

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

**The same six facts exist once more per result family** (§15). A mixed result
has a Pages half and a Blocks half, and each may state the six independently
under its own prefix — `tine.page-` and `tine.block-` — plus one presence marker
each. The seven scoped keys per namespace are `-view`, `-display`, `-sort`,
`-group-field`, `-columns`, `-col-aggregates`, `-sample`; the eighth key,
`tine.page-match-scope`, is not a display fact at all and is settled in §15.2.

| Property | Value |
| --- | --- |
| `tine.{page,block}-display` | exactly `1` — the PRESENCE marker, nothing else |
| `tine.{page,block}-view` | as `tine.view`, for that family only |
| `tine.{page,block}-sort` etc. | as the singular key of the same name, for that family only |

The marker is what makes the difference between the two ways a family can carry
no settings:

| Marker | Members | Meaning |
| --- | --- | --- |
| missing | any | **no scoped draft**: inherit the singular facts, even if orphan member keys exist |
| `1` | none | **present and empty**: clear the inherited non-view facts |
| `1` | some | **a complete scoped snapshot**: an omitted member does not inherit |

An invalid marker or member is retained byte for byte and reported through
`unreadable_settings`; the namespace is rejected atomically, and a valid
independent presentation still stands. Presence is decided by `Object.hasOwn` on
the frontend and by the marker in Rust — never by truthiness, because `[]`,
`group_by: ""`, `sample: 0` and `{}` are all meaningful values that a truthiness
test reads as absence.

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

The backend owns exact ordinary-field statistics in optional `QueryResult.statistics`.
Rows and statistics come from one current main-SQLite snapshot on both backends.
Statistics describe the complete semantically ordered sample, before payload
admission. The frontend formats these returned facts; visible, revealed or live
editor rows never supply an exact query-wide fold. Children-backed sheets retain
their existing aggregates.

### Ordering and sampling before display limits

Blocks establish the complete base order: binary page display name,
journal-before-page kind, existing construction order for equal page keys, and
document preorder. Explicit stable multisort follows the requested directions,
with ascending full base ordinal as the final tie. Descending does not reverse
equal-key document order. The semantic sample is taken next, then the bridge
admits a bounded payload prefix. Without an explicit sort, sampling selects the
complete base-order prefix; construction-prefix sampling followed by
alphabetization is forbidden.

For construction order `z,y,a`, ascending property sort, sample 1 and row cap 2,
the selected and displayed winner is `a`, and its statistics count is 1.
The old construction-prefix answer `y` is not an allowed result.

Block `total` counts the semantic sample, including denied rows; `matched_total`
is the complete pre-sample match count. Block `exceeded` means denial within
that sample. Page counters retain their approved meanings: `matched_total` is
pre-sample, `total` is admitted-before-sample, and `exceeded` retains its existing
behavior. Thus 45 page matches with row cap 2 and sample 1 have `total=2`, one
displayed page and `exceeded=true`, while statistics describe one sampled page.
Neither legacy `total` nor visible-row count is a universal statistics count.

The desktop bridge retains its existing over-budget refusal. A core answer with
`exceeded=true` is not silently passed through that refusal; successful returned
statistics still describe the complete sample when frontend reveal limits hide
rows. No second statistics request or increased consumer budget bypasses it.

### Statistics, grouping and formatting

Aggregates preserve their order and duplicates. Keys are literal case-sensitive
authored property names, including `state`, `page`, `prop:cost` and
`formula:cost`. Count always counts rows, including a named-property count.
Sum and average read only the first exact-spelling raw property by authored
ordinal, use JavaScript `parseFloat` numeric-prefix semantics, skip missing and
non-finite inputs, and add contributors sequentially in sample order. Average
divides by contributors. Skipped counts are explicit.

Cells are tagged finite JSON numbers or markers with reasons:
`non_finite`, `division_by_zero`, `empty_group`, `non_numeric`. A number is
unrounded on the wire; markers have no value. Neither null, numeric strings nor
NaN/Infinity labels are numeric cells. The frontend renders markers as
**Unavailable** with their reason. Finite values retain three-decimal JS rounding
and display behavior without overflowing a large finite value while rounding.
A valid empty selection has count zero, empty-group sum/average markers, and an
empty group list when grouping is requested. Absence of statistics is not zero.

The shared effective view resolves an unset Board grouping to task state without
writing a default on open. Explicitly cleared grouping remains ungrouped.
Grouping alone requests count. Groups follow first full ordinal and then the
row's membership order. Missing null, literal `(none)`, and empty string remain
distinct. Tags contribute once overall and once per membership; the summary
retains the multi-membership notice.

Formula query-wide grouping is deferred. It returns exact ordinary-field overall
cells, `groups=null`, and `grouping_status=unsupported_formula`, with:
**Exact statistics by formula are not supported yet. Overall statistics are shown.**
Saved and visual formula grouping remain intact; a literal aggregate property
named `formula:cost` remains supported. Advanced-query summaries remain disabled.

The overall summary and query-table footer consume the same returned statistics.
Footer lookup uses aggregate-list position, and duplicate entries remain in the
complete summary. Page rows retain physical path identity, so equal display names
do not collapse before aggregation. Scoped page and block answers remain separate.

Statistics have a separate retained-key/cell budget at the consumer's existing
byte ceiling. Exceeding it refuses the complete answer with the canonical typed
`statistics_resource_limit` reason, never partial groups or fabricated zero:
**Exact query statistics exceed the available memory limit. Narrow the query or remove grouping or aggregates.**
It is not a readiness retry or a reason to repair the projection. Existing
refresh ownership retains coherent rows and statistics together and rejects
obsolete completions.

Tests: `crates/tine-core/src/query/statistics_tests.rs`,
`crates/tine-core/src/query/results_tests.rs` (the `q4_` regressions),
`crates/tine-core/tests/query_ir_wire.rs` with
`tests/fixtures/query-statistics/semantics.json`, `src/editor/queryAggregate.test.ts`,
`src/editor/queryIr.test.ts`, `src/components/QuerySummary.test.tsx`,
`src/components/QueryWorkspace.test.tsx` and
`src/components/QueryDisplayConsumers.test.tsx`.

## 7. The inline Display panel

Display remains accessible when the filter sheet is closed. The same control is
mounted at the resting query or in the open sheet, never both simultaneously.
Its open state and the sheet's open state share one registry gate: neither open
means no registry read; either open uses the same scope/revision cache. The
closed-sheet access and zero-at-rest regression is in `QueryMacro.test.tsx`.


`QueryDisplay.tsx` edits all six facts in one place, as what they are: two
enums, three ordered lists and a number.

- It is **the only display control the sheet offers**. `+ sort` and
  `+ summarize` are gone: each could state a FRACTION of one fact — one sort
  pair, one aggregate — and each rewrote a longer authored list as a one-element
  one. `QueryBuilder.inlineDisplay` no longer decides *whether* the panel is
  mounted, only *whose writer* it uses: a host that owns the block's `tine.*`
  writer hands its own control in, and every other host gets the builder's
  session writer. A friendly search reaches the panel through its two result
  sections instead (§15.3), which is where the vocabulary is meaningful.
- It takes a **`rowKind`**. Absent means blocks — the single family every caller
  had before mixed results, keeping its trigger name, dialog name and whole
  vocabulary unchanged. `rowKind="page"` names itself **Display pages** /
  **Page display**, offers only the page vocabulary (§15.4), and counts
  applicability on pages rather than on blocks.
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

- session/UI caps on a column selection;
- a saved sort by title, `state`, `tags` or a formula: that needs an engine sort
  vocabulary, not a frontend replacement sorter, and P5B deliberately shows the
  limit (§8) rather than papering over it with a second answer. A Friendly page
  sort by recency is the same limit seen from the other side: the Friendly read
  captures no page-recency programs, so a recency field contributes no order
  term and the page Display picker never offers one;
- query-wide statistical grouping by a FORMULA. Formula columns still group
  visual rows; the exact breakdown is deferred outside S3;
- `formula:` columns in `tine.columns`: they have no name in that grammar, so a
  table showing one cannot save its column order at all (§8). A grammar that
  could name them is a new columns grammar, which P5B is not authorized to add;
- the publisher's step-3 grouping: a `(group-by …)` that lives only in the query
  text is not read by `publish.rs`, which resolves from block properties alone
  (§9);
- a saved grouping on a **children**-backed sheet, which still reads its own
  `tine.group-by` (§9);

Manager review pin: legacy-schema recognition applies only to query tables;
ordinary children tables keep their existing schema interpretation. A schema
write that also rescues query columns captures both the schema page and query
page in the same undo unit, including an enclosing header reorder. Tests:
`QueryColumns.test.tsx` ordinary-children and cross-page-undo cases.

## 11. The query workspace's display draft — the session foundation

A **query workspace** (ADR 0042) is a virtual, graph-scoped, device-local route
that persists its source expression rather than a result snapshot. It has no
block, so it has nowhere to put the `tine.*` properties §1 describes, and until
P5C its display was simply not a thing that could exist: `QueryRoute` carried an
id, a source, a `sourceKind` and a `presentation`, and a session round trip threw
away anything else.

This section settles where a workspace's display choices live and what they may
contain. It settles **no execution and no UI**: nothing here renders a face,
runs a query, or materializes one into a page. Those are the next package's.

### 11.1 One optional member, three meanings

`QueryRoute.display` is an optional `QueryDisplayDraft`, defined as
`Omit<ViewSettings, 'view'>` — a **complete snapshot** of the non-presentation
half of §1's facts, never a patch:

| State | Meaning |
| --- | --- |
| absent | nothing chosen; the workspace inherits what the parsed query already states |
| `{}` | every non-view setting explicitly cleared |
| populated | exactly these settings, and only these |

`presentation` remains the sole authority for the view (§1's `tine.view`
equivalent for a workspace). That is enforced by the TYPE, not by convention: a
draft has no `view` member to disagree with, and an incoming `view` key is
dropped like any other unrecognized one. The distinction between absent and `{}`
is the same distinction §2 draws between an absent `tine.columns` and a
present-but-empty one, for the same reason: "I said nothing" and "I said none"
are different statements, and collapsing them is how a cleared selection
resurrects an old one.

A draft holds **no result data**. It is display facts only, exactly as §1 lists
them.

### 11.2 One normalizer, three callers

`src/editor/queryDisplayDraft.ts::normalizeQueryDisplayDraft` is the only place a
draft is validated. The router's mutation, the session serializer, and the
session restorer all call it, so a draft that survives a save is exactly a draft
the router would have accepted. It is pure: no store, no DOM, no backend, and
**no query language** — a draft is the already-lifted view half, and the backend
stays the only producer of query text (§1, ADR 0042).

`queryDisplaySettings(draft, parsed, presentation)` is the optional companion
that hands a renderer the `ViewSettings` to use. It takes the view from
`presentation` always, inherits the parsed settings when the draft is absent, and
otherwise replaces that half wholesale. It parses nothing.

**What normalization does.** Unrecognized keys are dropped; a member present as
`undefined` is treated as absent; every list and tuple is deep-copied, so a
caller that keeps mutating the array it passed in cannot reach back into a route
or a history entry that has already been recorded.

**What it refuses**, and refusal is all-or-nothing — one bad entry rejects the
whole draft. A partially applied draft would show a display nobody chose, and a
truncated list is indistinguishable from a deliberately short one:

- each of `sort`, `columns`, `aggregates`: at most 64 entries;
- each field: 1–512 UTF-16 units;
- the whole normalized draft: at most 65 536 UTF-16 units of JSON. The bound is
  asked of the recognized display data only — the route's id, source and
  presentation keep their own existing bounds;
- `sort` directions are exactly `asc`/`desc`; aggregate functions are exactly
  `count`/`sum`/`avg`;
- `sample` is a safe integer in `0 ..= 4294967295` (the bridge's `u32`); null,
  non-finite, fractional and overflowing values are refused;
- a field in a LIST member must be nonempty, free of `;` `=` CR LF NUL, and
  free of outer padding — the §5 grammar's own punctuation, plus the trim its
  reader performs, so a padded name would come back naming a different field.

Two exceptions, both from elsewhere in this contract:

- `["", "count"]` is the fieldless whole-result count (§5, X3). An empty field
  with `sum` or `avg` names nothing to add up and is refused;
- `group_by: ""` is the explicit clear (§3), the one non-canonical grouping
  value accepted.

**Grouping uses §3's canonical helper, not the legacy reader.**
`canonicalGroupField` is what reads a draft's `group_by`, so `prop:state` is the
ordinary property and a bare `state` is the task marker — a bare `status` is
refused rather than guessed into `prop:status` the way `legacyGroupField` must
guess for pre-P5B bytes. Because a grouping value is ONE field and not a list,
`;` and `=` are ordinary bytes inside a canonical `prop:`/`formula:` name there;
the list rule above must not leak across. The B helper still refuses CR/LF/NUL,
and the 512-unit bound still applies.

### 11.3 What each seam does with a bad draft

The two seams fail in deliberately different directions, because the cost of
being wrong differs:

- **A router update** (`updateActiveQuery`) carrying an unreadable draft retains
  the previous route **entirely** — the source and presentation in the same
  patch included. A half-applied edit would be a silent partial success. The
  display panel that will own this seam prevalidates with the same function, so
  it can report the refusal visibly rather than discovering it here.
- **A session restore or serialization** drops **only** the draft and keeps the
  otherwise valid route. Losing a tab, or a pane's whole layout, over a display
  choice would be the disproportionate refusal D-3 rules out: a draft is
  disposable, the workspace is not. The workspace reopens inheriting what its
  query text says.

A patch that does not mention `display` keeps whatever the route had, `{}`
included; `display: undefined` is the explicit way to clear the draft back to
inheriting.

Tests: `src/editor/queryDisplayDraft.test.ts` for the normalizer and the
combining helper; `src/router.test.ts` (`query workspace display draft (P5C)`)
for the mutation seam; `src/session.test.ts` for the round trip, the absent-
versus-`{}` distinction, the malformed-draft drop, and the copy that keeps a
persisted snapshot from aliasing a live route.

## 12. Ordered sort execution (P5C shared prerequisite)

The shared Rust view adapter applies every requested sort clause in order. Each
clause uses its existing field semantics and ascending/descending direction; only
when all keys tie does original row order decide. Sampling follows the completed
sort. A nonempty sort list disables the unsorted sample admission shortcut, and
a recency field in any position requests the same recency construction metadata
and construction profile as a primary recency sort. Every result passes through
this adapter; the frontend does not sort a replacement result set.

This corrects the former first-clause-only QueryOpts adapter. Sort keys are
computed once per admitted block per requested clause; existing missing-property
visible-text fallback can parse each admitted block for a missing key. No page
document is loaded. Work scales with admitted rows and requested clauses.

Tests: query.rs ordered_view_sort_uses_secondary_direction_before_sample_and_keeps_ties
and ordered_view_sort_secondary_recency_requires_construction_axis; existing
query tests retain single-sort/sample behavior.

### 12.1 Result lifetime

The backend does not retain complete query answers. Each valid query executes
against the current projection, constructs operation-local pre-view groups,
and applies the requested view before returning the final DTO. Repeating the
same query therefore performs another projection read and returns the same
ordered groups, totals and exceeded state when the projection is unchanged.
Display rerenders may keep their mounted frontend state, but that state is not
a backend answer cache.

The observed property registry, pending-registry patch, parsed document,
projection and reference caches remain separate derived data. They do not
authorize reuse of a complete query result. The SQL result route does not load
page documents merely because it executes again.

Tests: `clean_runtime_repeated_simple_queries_execute_again_and_follow_edits`,
`ret1_the_public_ir_route_reads_statements_and_hydrates_no_page`,
`ret2_the_public_advanced_route_reads_statements_and_hydrates_no_page`, and
`ret2_repeated_advanced_queries_execute_again_and_follow_edits_and_declarations`.

## 13. Materializing a workspace: revision safety and format (P5C2A)

A workspace has no block, so making one is a **publication**: it creates a page
that did not exist. `materializeQueryWorkspace` is that one path, and this
section settles what it is allowed to publish and when.

It publishes the **complete** display envelope (Q3). The `tine.view`-only filter
it used to apply dropped every other setting the workspace was showing, so a
saved query reopened as a different query than the one that was saved. Each part
now goes through the writer that owns it — `queryViewPropertyPatch` for the
singular facts, `queryScopedDisplayPropertyPatch` once per namespace, and
`queryPageMatchScopePropertyPatch` for membership scope — and each writer states
only what it owns. A workspace with no scoped drafts still materializes a bare
query block, because absence is a value: writing a marker for a namespace that
has none would CLEAR settings the reopened query should inherit.

### 13.1 A save publishes the input it captured, or nothing

Publication is asynchronous — Rust friendly-search validation, a title lookup,
then the write — and the workspace underneath it is not frozen. The user can
retype the search, switch the view, rename the page, change tab or switch graph
in the middle. **Route id is not an input**: two attempts under the same route
id can be publishing different searches, different views or different titles,
and the second is not the first.

So a save captures everything it is publishing at submit — source, `sourceKind`,
presentation, title, graph format, route id and the graph scope — and the
component supplies an `isCurrent` callback that answers whether that capture is
still what the user is looking at. `materializeQueryWorkspace` checks it before
any work, after the Rust validation, and immediately before `savePage`. A
capture that has gone stale returns `{ ok: false, kind: "superseded" }` — a
**local** refusal with retry text — and `savePage` is never called.

Two guards, deliberately distinct:

- **`sameWorkspace`** — same live component (not unmounted), same graph
  (`captureGraphScope`/`isScopeCurrent`: the graph BINDING, per I-20, never the
  render epoch), and the ACTIVE route is still this workspace's route. It is
  what decides whether a completion may touch the local surface at all.
- **`sameInput`** — `sameWorkspace`, and the source, kind, title, graph format
  and the whole captured settings envelope all still equal the capture. The
  envelope is captured as ONE serialized value (`captureSettings`), which is
  both the deep copy and the comparison: the user can keep editing a draft while
  the write is in flight, and an attempt that shared the route's arrays would
  publish whatever the draft became. Presence survives the round trip — a key
  absent from the route is absent from the capture, and a present empty draft
  survives as `{}`. A monotonic input revision also rejects edits that restore an
  earlier value; equal text after a newer edit is not the original submission.

`isCurrent` is optional. A direct caller that passes none is unguarded, exactly
as before this section existed.

### 13.2 What is authoritative, and what only committed

The audited no-baseline save `savePage(page, null, false)` remains the
authority on whether the page may be created. The title lookup in front of it
is a friendly preflight and nothing more; a collision reaching the save is still
a `conflict` refusal, and the workspace stays virtual. Nothing here ever forces
a save.

The captured-input guard is a **local** guard on top of that, and it stops at
`savePage`. Once the write has begun the page may legitimately land despite a
later edit or navigation, and then:

- the page exists. It is never described as undone and never deleted;
- `bumpPageInventoryRev()` has run, so every inventory reader sees it;
- but the route is **not** replaced, and no error is written into a workspace
  that has moved on. When the workspace is still the live one, the commit is
  acknowledged in its own non-error notice; when the user has changed tab or
  graph, the completion is silent there;
- and the publication is never retried under the changed input. Saving again is
  the user's call.

The captured token guards the shared `saving` flag on the same principle: a
superseded attempt may not re-enable a control a newer one is still using.

### 13.3 The property goes where the graph's format reads it

**What** to write is `queryViewPropertyPatch` (§4) — the same view→property map
every other query save runs through, never a second serializer. An absent
`tine.view` IS the default list view, which is how §4 spells it and how the
inline panel switches to it, so a list workspace still materializes a bare
`{{query …}}` block in both formats.

**Where** it goes is the graph's preferred format, captured at submit and set
explicitly on the published `PageDto.format`. Markdown gets a `tine.view:: …`
line in the canonical head region; org gets a `:PROPERTIES:` drawer. A markdown
property line written into an org file renders as visible body text and is never
read back as a property (the GH #25 class), so this is correctness, not
formatting. Both writers are `store.ts::setBlockProperty`'s own — moved
unchanged into `src/editor/properties.ts` so a block being built before it
belongs to any store uses the same placement rule. An absent format means `md`.

A format change mid-flight is a **stale capture**, not a cross-format publish:
the block raw and the page format must agree, so the attempt refuses and the
user saves again.

Tests: `src/components/QueryWorkspace.test.tsx` — the materializer's three
guard points, the org drawer and markdown line forms, and the component
fixtures that hold one real await open while the source, view, title, graph
format, graph root or active route changes underneath it.

## 14. Live result editing and refresh retention

Query result blocks use the ordinary editor, save path and Undo stack. A coherent
older SQL membership answer must not replace a newer live block edit with its
older payload: the mounted result continues to use the ordinary source object.
Crossing
the open/completed task boundary updates live block content immediately.
`QUERY_COMPLETION_GRACE_MS` is 2,000 milliseconds: a query displaying the
interacted block defers membership reads during this presentation grace, retaining
only the latest demanded data revision. Repeated completion interactions extend
the deadline. Query, graph, view, binding or expanded/collapsed identity changes
cancel the hold; unmount cancels its timer. Later database-change notifications
remain ordinary refresh demands, including when an earlier read saw an older
coherent projection.

If a completed read would remove an actively edited result, retain the entire
previous same-identity displayed operation until the ordinary editor ends. Groups,
page rows, Friendly/Search hits and evidence, diagnostics, report metadata, count
and order publish together. Only the latest pending operation is retained; an
identity change discards incompatible pending output. Surviving groups and blocks
continue to reconcile by stable keys, preserving mounted editors and revealed
depth. This UI state does not retain backend answers between operations or require
a saved-edit freshness token.

Tests: `src/queryResultGrace.test.tsx` and
`src/components/QueryMacro.test.tsx`; native scroll and mounted-row geometry:
`scripts/e2e-query-result-scroll.mjs`.

Direct projection commits wake the existing application watcher after SQL,
identity and registry publication. The watcher coalesces an opaque notification
counter per graph binding and emits `query-projection-changed`; the frontend
filters retired bindings and increments ordinary dataRev without reloading
editors. This covers an early coherent read followed by a later commit, including
a database replacement whose physical SQLite revision happens to repeat.
The counter is never a query target, result key or saved-edit acknowledgment.
Managed uses its existing completed local/provider drain notifications.


Static whole-site queries use the same current-main SQL reader for OG, advanced,
TQL, BEGIN_QUERY and query-backed sheets. Every authored occurrence executes on
that publication's snapshot; no backend answer cache or publication query index
is built. Page queries render links through exact captured public page paths,
with private matches omitted after selection and counted as omissions. The
whole-site command preserves its fresh captured-document/privacy boundary;
ordinary live-query reads still accept a coherent older main image. Print-query
routing and the remaining Friendly public adapters are not covered by this
publication migration.

Tests: `publish::tests`,
`managed_static_publication_pairs_ordinary_drain_with_current_main_queries`, and
`publication_sources_are_compared_inside_the_owned_main_snapshot`.

Print executes ordinary OG/TQL macros and query tables/boards through the shared
current-main SQLite selection and subtree executor on both backends. One document
owns one snapshot, registry, execution day, identity policy and cancellation
lifetime, including nested and repeated occurrences. Direct reads the requested
page body from its file; Managed loads the exact actor-owned page body and reads
queries off actor through the existing Managed read admission. Those body and
query authorities do not imply one source revision or a just-saved-edit guarantee.

Each occurrence independently permits 20,000 shallow rows and 32 MiB of shallow
construction, with a 64 KiB source limit and the existing 64-level OG/EDN nesting
guard. Expansion depth remains four. Print has no query-count, subtree-node,
subtree-byte or whole-HTML cap. One asset budget covers the body and query output:
12 MiB per image and 32 MiB cumulative raw asset bytes, with inert omission markers.
Copy's bounded subtree policy and omission counts are separate and unchanged.

Required query rows and descendants are complete or the entire preparation fails.
Selection overflow is rejected before subtree hydration as `query-unavailable`,
reason `print_query_budget_exceeded`, with the message “Couldn't prepare this page
for PDF: a query exceeds the Print limit. Narrow the query and try again.” Source
and nesting refusals also give bounded, specific messages. Typed readiness is
retried under the Print operation owner; terminal errors, cancellation, a newer
Print request, or a graph rebound cannot insert an incomplete iframe or print it.

Personal Print includes private matches. Page-anchored results, positional query
grids and BEGIN_QUERY remain explicitly unsupported; cross-page links retain
their existing degradation. Query subtree construction uses physical SQLite
locators and stored payload, never parsed-graph hydration or public-ID rediscovery.
Org sheet conversion uses the physical path, including uppercase `.ORG` files.

Proof: `print_query_reader_regression_renders_matches_on_both_backends`, the
`print_*` executor/route tests, `src/print.query.test.ts`, and the native page-menu
journey in `scripts/e2e-print-security.mjs` for each backend.

## 15. Mixed results: two families, one operation (Q3)

A **mixed result** is the Friendly search's union and nothing else: it answers
"which pages match" and "which blocks match" at the same time. An explicit query
is not mixed — it renders the one section its declared anchor names.

Before this, both halves arrived in one flat list under one presentation. That
made "show the pages as a table and the blocks as a list" unsayable, it made a
page's own Display settings unreachable, and outside the Search face the page
half was dropped entirely.

### 15.1 The effective settings of one family

One resolver answers "what is this family showing", for every caller:
`queryDisplayDraft.ts::queryScopedDisplaySettings`. The workspace adapts its
route through `queryResultDisplaySettings`; an inline query adapts the flattened
`ParsedQuery` fields through `queryParsedDisplaySettings`. **There is no second
resolver in a component.**

- A scoped **presentation** independently overrides the singular presentation.
- A scoped **draft** independently overrides the ENTIRE singular non-view
  snapshot. It is complete or it is absent (§1's marker table); a missing member
  of a present draft never merges back from the singular state.
- A first scoped edit therefore **clones the effective non-view snapshot**
  (`queryScopedDraftFrom`), applies the change on top, and persists the whole
  thing. An edit that stated only the fact it changed would silently clear
  everything the section was already showing.
- **Use inherited settings** removes the draft; **Clear settings** writes `{}`.
  They are distinct actions because "show what the query says" and "show nothing
  extra" are different requests, and a section that could only be replaced and
  never given back would be a state with no way out (I-10). Presentation
  inheritance is independently controllable.
- Switching a presentation changes neither the predicate nor the membership
  scope. Unsupported authored settings stay visible as retained settings and are
  never silently rewritten (§5).

### 15.2 Friendly page membership — `tine.page-match-scope`

**Page matches** takes exactly three wire values, and nothing else:

| Value | Meaning |
| --- | --- |
| `names` | the existing page-name/alias matcher — exact, prefix, substring, fuzzy, Unicode/case, boolean and regex. "Names" does **not** mean exact-equality-only |
| `content` | a stored page qualifies when an individual contained block satisfies the Friendly block predicate; that page's best matching block supplies its rank and evidence |
| `both` | the union of qualifying physical pages by identity |

An absent or unreadable value resolves to `names` at execution — the historical
default. Absence stays distinct from `names` in the reader; only the execution
layer applies the fallback.

- Under `both` and with no explicit sort, Names winners precede Content-only
  winners, a page matching both keeps its Names winner, and physical-path
  tie-breaking is preserved.
- The owner-local exact-name/alias override still runs before global ranking; the
  physical owner is returned once, keeping its winning alias and evidence.
- `content` never concatenates blocks and never matches separate terms across
  unrelated blocks. One block satisfies the predicate, or the page does not
  qualify by content.
- This is **not** a renamed-page history search. An old name participates only
  through existing current name/alias/reference metadata.
- Scope changes affect page membership only. The Blocks section still evaluates
  the ordinary Friendly block predicate.
- Virtual reference-name navigation suggestions survive in applicable name
  search, are distinguishable from stored page rows, never hydrate fake
  properties, and never consume the stored-page limit.
- The physical `QueryPageScope` is a different member and keeps its meaning:
  "search within one routed page". Membership scope never overloads it.

### 15.3 The DOM contract

| Contract | Required DOM/behaviour |
| --- | --- |
| Family boundary | `<section data-query-result-kind="page\|block" aria-labelledby="…">` with per-MOUNT ids; headings **Pages** and **Blocks**, Pages first |
| Controls | buttons named **Display pages** / **Display blocks**, each controlling only its own section; dialogs named **Page display** / **Block display** |
| Search/List | native list semantics or `role="list"` named **Page results** / **Block results**; each row is a listitem CONTAINING its navigation/edit control, never a button that is also the row |
| Table | native `<table>` with a matching `<caption>`, `scope="col"` headers and body rows |
| Board | named grouping sections containing lists; page cards navigate, block cards keep ordinary editing |
| Identity | stored pages key by physical path and kind, virtual suggestions by name, blocks by physical owner plus block identity — never by display name alone and never by array index |
| Pending/error | the family container exposes `aria-busy`; pending text is `role="status"` and a failure is `role="alert"`. A read failure is not an empty-state message |
| Empty | a family with no rows says so and **keeps its controls** (I-10) |
| Truncation | `has_more.pages` / `has_more.blocks` beside the corresponding section; the label describes shown rows and never infers completeness from rendered length |

The two sections are two views of ONE returned operation, partitioned in the
frontend. Neither half is refetched on its own: two independent reads could
describe two different graph states, which is exactly what I-20 forbids.
Grouping may arrange admitted rows but must preserve backend order within
groups — with an explicit sort one group value legitimately opens twice, so
grouping is by **adjacency**, never by re-clustering on the value.

### 15.4 The page vocabulary

A page is not a block, and its Display panel says so.

- Page builtins are `name`, `kind` and `day` only. Task state, priority,
  planning dates and formulas are **not** offered: none of them is a page
  attribute, and none of the writers can spell one for a page row.
- Everything else is an ordinary authored property, offered only where the
  existing field grammar preserves that identity (§5's gates apply unchanged to
  both row kinds, because both write the same property spellings).
- Applicability is counted on the row the vocabulary is FOR: a property observed
  on no page of this graph is not a page field. The block vocabulary keeps its
  established combined count.
- A page Board with no applicable grouping is **one ungrouped column**. It never
  inherits the block Board's task-state default, which would show the author a
  grouping their pages cannot have.
- A page row's columns come from the page's own hydrated `PageRow`, in its
  authored spelling. A page the backend did not hydrate has no properties to
  show; the cell is empty and the row still navigates.

### 15.5 Limits and ordering

Workspace defaults remain 40 pages / 100 blocks; inline Friendly defaults remain
500 pages / 5,000 blocks. Each family's own sample reduces only its own
requested rows: `sample: 0` empties that family alone, an absent sample keeps
that consumer's bound, and **no unused capacity transfers between families**.

The combined bridge ceiling is unchanged at 20,000 rows / 32 MiB estimated
bytes, enforced across search hits; the added page-row payload counts toward it.
Scoped settings cannot raise that ceiling or any per-consumer budget.

Complete matches are ordered **before** sampling and output admission (Martin,
2026-09-09). Q3 carries both views and preserves the returned order; the
order-before-cap correction itself is Q4's, consumed rather than duplicated. A
frontend re-sort or truncation would replace a complete answer with an answer
about whichever rows happened to fit, so neither family's renderer does either.

Tests: `src/components/QueryResultSections.test.tsx`,
`src/components/QueryPageResults.test.tsx`,
`src/components/QueryDisplay.test.tsx` (`q3_page_vocabulary_excludes_block_fields`),
`src/components/QueryBuilder.test.tsx` (`q3_shared_display_retires_legacy_controls`),
`src/components/QueryMacro.test.tsx` (`q3_macro_consumes_flattened_scopes`,
`q3_absent_scope_inherits_and_present_empty_clears`,
`q3_scoped_edit_preserves_sibling_and_source`,
`q3_mixed_operation_rejects_stale_completion`),
`src/components/QueryWorkspace.test.tsx`
(`q3_workspace_executes_page_anchor_and_both_displays`,
`q3_scoped_save_reopen_md_org`, `q3_materialize_scoped_capture_is_revision_safe`),
`crates/tine-core/src/query/friendly_tests.rs`
(`q3_friendly_scope_membership_and_evidence`, `q3_mixed_limits_are_independent`,
`q3_friendly_page_display_hydrates_stored_rows_only`,
`q3_friendly_sections_sort_the_complete_set_before_their_bound`),
`crates/tine-core/src/onboarding.rs`
(`q3_guide_scoped_display_example_roundtrips`); native journeys in
`scripts/e2e-query-workspace.mjs` and `scripts/e2e-query-display.mjs`.
