# New docs progress

## Completed prerequisites

- **Phase A — `124f6a5b`**: completed editorial calibration for `Tine Guide` and `Features/Sheets`; the index now uses the Start/Workflows/Feature reference/showcase lenses and Sheets begins with ordinary bullets.
- **Phase 0 — `0a1dca82`, `80a898fc`**: completed Guide-link hardening; accidental targets are rejected while registered pages and deliberate stubs are checked by focused regression proof.

## Phase B — candidate: Workflows/Structure repeated information

Added one canonical workflow page, registered it in the shared manifest, linked it from the Workflows lens, and added a semantic onboarding proof for its registration, copy behavior, canonical links, executable tracker/query, and observable result. It remains a manager-review candidate, not a declaration that J05 is complete.

### Behavioral claim sources

- `crates/tine-core/src/templates/sheets.md:7` — “rows are child bullets, columns are their properties, cards are child bullets or blocks a query finds.” The workflow begins with ordinary tracker children and then changes only their view.
- `crates/tine-core/src/templates/sheets.md:8` — “Show children as → Grid or Table” and “`/Query`, then run `/Table` or `/Board`.” The table/board path names current visual controls before stored syntax.
- `crates/tine-core/src/templates/sheets.md:58` — “without copying the source blocks.” The query-backed board is explicitly presented as another view of the matching tracker bullets.
- `docs/FEATURES.md:179` — “Search, list, table, and board are presentations of one result membership.” The workflow separates selecting matching blocks from choosing a result presentation.
- `docs/FEATURES.md:180` — “friendly search-text surface” and “interactive visual query builder” compile with raw DSL to “the same query plan.” Friendly search and the visual builder precede the raw query explanation.
- `docs/adr/0030-query-view-unification.md:23` — “The query block owns membership through the query DSL and builder.” The query instructions describe membership separately from view.
- `docs/adr/0030-query-view-unification.md:24` — “`tine.view::` owns presentation.” The stored-syntax explanation identifies the board/table properties as presentation only.
- `docs/adr/0042-one-query-plan-many-frontends.md:52` — “Friendly search remains the primary authoring surface; raw DSL is optional.” The raw query text appears only after the visual path.
- `src/components/QuickSwitcher.tsx:585` — “Open search tab.” The workflow names the shipped transition from the quick switcher into the persistent result workspace before using workspace-only controls.
- `src/components/QueryWorkspace.tsx:733` — “Filters / Advanced”; `src/components/QueryWorkspace.tsx:744` — Search/List/Table/Board presentations. The workflow uses the shipped workspace labels.
- `src/components/QueryWorkspace.tsx:521` — “Edit as visual query”; `src/components/QueryBuilder.tsx:599` — “➕ Add filter”; `src/components/QueryBuilder.tsx:620` — “Property.” The workflow names the current visual narrowing controls.
- `crates/tine-core/src/templates/formulas.md:19` — “a read-only column appears.” The optional formula step links the canonical Sheets and Formulas pages instead of reproducing their walkthroughs.
