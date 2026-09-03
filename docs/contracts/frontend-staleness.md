# Frontend asynchronous landing

Graph-scoped asynchronous work is owned by `graphBindingRev`, exposed through
`graphBinding()`. Capture a frozen scope before the first `await`, re-check it
after every `await`, and check again immediately before the first graph-scoped
IPC or UI/store commit. A stale background result is dropped; a stale
user-initiated result is dropped with a local toast.

`graphEpoch()` is a render epoch, not graph identity. Typography, journal-title,
and other display changes may bump it without changing the graph. It is compared
only when a result is explicitly repaint-sensitive.

`captureGraphScope`, `isScopeCurrent`, `landAsync`, and `landAsyncOrToast` in
`src/landAsync.ts` provide the standard shape and a discriminated landing result.
Existing specialized exemplars remain `pdfOwnership.ts`, RightSidebar's
`useEnsurePage`, Block's `editorIsCurrent`, and graph's
`journalTemplateOwnerIsCurrent`. The source guard in
`src/frontendStaleness.guard.test.ts` pins the token rule to
`persistence.ts:362` and I-20.

## Harvest W4-P1 — derived-work bounds

Each entry names the item, the production producer that owns the work, the
trigger or key that may re-run it, the numeric bound, and the proof test. All
four are pinned by `src/frontendStaleness.contract.test.ts`; a bound with no
proof test is not a contract. The packet's fifth item is a Rust-side
measurement of the Managed clean-reopen path and is recorded in
`docs/storage-sync-contract.md` instead.

**Item 1 — sort-key derivation.** Producer: the `sortedRows` memo in
`src/components/SheetTable.tsx`. Trigger: a change of sort column/direction or
of the row set. Bound: at most `R` effective sort-key derivations per sort for
`R` rows, whichever branch (title, formula, ordinary property) answers —
derived once per row by decorate–sort–undecorate, never inside the comparator.
The single seam is `__sheetTableTestHooks.onSortKey`, so an equivalent
derivation moved to another helper still counts. Proof:
`src/components/SheetTable.test.tsx::SheetTable sort-key derivation (Harvest W4-P1 item 1)::derives at most one sort key per row for title, property, and formula sorts`.
`rowIndexes` deliberately still walks the full sorted list: off-window keyboard
and block navigation needs the complete map, and that is a correctness
requirement, not a missed cut.

**Item 2 — page-name merge (measured, verified-closed; no production change).**
Producer: the `names` memo in `src/pages.ts`. Trigger: a `dataRev` bump, i.e.
every typing lull. Bound: `0` merge-memo executions across five consecutive
lulls in which the backend answers `{digest: carried, names: null}` — the
`referencedNames` resource resolves to the same array reference and a Solid
resource value is stored in a signal with an identity comparator, so nothing
downstream is notified. The counter sits inside the memo body before its `Set`
is built; a counter downstream would report zero even while the `Set` was
rebuilt. Proof:
`src/pages.inventory.test.ts::GH #229 complete page-name inventory::rebuilds the page-name merge zero times across five unchanged-reply lulls`.

**Item 3 — QueryBuilder facets.** Producer: the facets `createResource` in
`src/components/QueryBuilder.tsx`, routed through `sharedQueryResult` under the
`query-facets` key namespace. Trigger/key: the canonical graph scope
`` `${graphMeta()?.root ?? ""}\0${graphEpoch()}` `` plus `dataRev()`. Bound: `1`
`queryFacets(false)` request per (graph scope, `dataRev`) regardless of how many
builder instances are mounted, with every mounted builder exposing the current
payload through its production Property control. `queryFacets(true)`
(autocomplete) asks a different question and keeps its own call. Proof:
`src/components/QueryBuilder.transient.test.tsx::QueryBuilder facet sharing (Harvest W4-P1 item 3)::issues one shared facets request per (graph scope, dataRev) for five mounted builders`.

**Item 4 — tag-table queries (measured, no cut).** Producer: the tag-table query
resource behind `TagTableToggle`/`TagPageTable` in `src/components/Page.tsx`.
Trigger: a settled-save `dataRev` bump. Bound: `1` `runQuery` per distinct
routed page per invalidation — consumers of the same routed page share one
request, distinct routed pages are distinct questions. Measured at bound on the
checked base, so no production change was made. Proof:
`src/components/Page.test.tsx::tag-page table::issues one tag query per distinct routed page per invalidation, not one per consumer`.
