# 0030. Query view unification

- **Status:** Accepted
- **Date:** 2026-07-07

## Context

Tine supports two ways to enrich a query block: the query DSL and visual builder
choose which blocks belong to the result set, while `tine.view:: table` and
`tine.view:: board` choose a sheet face for those results. Before this decision,
those surfaces were mutually exclusive in the UI: once a query block had a sheet
face, the query header and builder disappeared, so the user could edit the view
but not the membership.

OG Logseq also has `:table-view? true` in the query options map. That table is a
read-only presentation toggle inside the query DSL. Tine's sheet table is editable,
round-trips as a block property, and shares the same table/board infrastructure
as child-sourced sheets.

## Decision

We will keep the query header and visual builder visible on query blocks that have
sheet faces. The query block owns membership through the query DSL and builder;
`tine.view::` owns presentation. The List/Table/Board switcher writes ordinary
block properties: Table writes `tine.view:: table`, Board writes
`tine.view:: board` and defaults `tine.group-by:: state` only when the block has
no grouping, and List removes `tine.view::`.

We deliberately supersede OG's interactive query-table toggle in Tine's header
with the sheet table switcher. Existing `:table-view? true` queries keep rendering
the legacy read-only table when they have no `tine.view`, but the header control
no longer writes session state or edits the query options map.

## Consequences

Query membership and query presentation can now be edited side by side on the
same block, including query boards and query tables. View choice becomes durable,
syncable plain text, and it round-trips through OG as harmless `tine.*`
properties.

This is a deliberate OG-parity divergence in the control surface: OG's query
table option is preserved for rendering old graphs, but Tine's active Table view
means the editable sheet table. The switcher must stay property-driven, and any
future query face must respect the same membership-versus-presentation boundary.

## Successor

The decision above stands as recorded. What it did not settle is the **grammar**
of the presentation properties themselves — in particular that `tine.fields::`
was carrying two unrelated things on a query block (a typed sheet schema, and the
list of columns to show), so each of the two writers destroyed the other's value.
Visible columns now have their own key, `tine.columns::`.

The same defect appeared a second time in grouping. This ADR's Board default
writes `tine.group-by:: state`, and that key carried ONE bare token with TWO
meanings: `state` was the task marker to the board and an ordinary property
named `state` to the list grouper, so a view switch could change what a note
grouped by. Grouping now has its own query-owned key, `tine.group-field::`,
whose value is a canonical sheet field id with exactly one reading. The Board
default recorded above is unchanged in intent and still applies only when the
block has no grouping — but "no grouping" is now a resolved state rather than a
missing property, so an explicit "no grouping" (a present empty value) is no
longer refilled by the next view switch. `tine.group-by::` is still read for
compatibility, at the view the note is currently persisted with, and is retired
on the first save that states the grouping.

The living grammar, precedence and ownership rules — which key means what, who
may write it, and what a save is allowed to touch — are maintained in
`docs/contracts/query-display.md`. Read that for current behaviour; read this ADR
for why presentation is property-driven and separate from membership.
