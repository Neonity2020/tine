# Managed read surface

This contract pins the production `application_*` family. A live row is either
`necessary` because it owns a Managed-only input/index boundary, or an `adapter`
whose body delegates to the named shared owner. New twins are prohibited by
I-12 and D-4. The syntactic guard uses
`projection_producer_census::production_rust()` and verifies membership and
adapter call edges.

## Live family

| symbol | file | class | canonical owner | justification/evidence |
| --- | --- | --- | --- | --- |
| application_page_property_dto | `crates/tine-core/src/query.rs` | necessary | — | Projects one Managed page-property DTO input. |
| application_page_property_pairs | `crates/tine-core/src/query.rs` | necessary | — | Normalizes Managed wire property pairs. |
| application_page_reference_matches | `crates/tine-core/src/query.rs` | necessary | — | Matches references over Managed page DTO input. |
| application_page_templates | `crates/tine-core/src/query.rs` | adapter | visit_template_blocks | Walks Managed DTO shape and delegates each result to the canonical template leaf. |
| application_query_doc_block | `crates/tine-core/src/query.rs` | necessary | — | Rehydrates a complete Managed DTO subtree. |
| application_sparse_query_doc_block | `crates/tine-core/src/query.rs` | necessary | — | Rehydrates a sparse materialized-query row through `DocBlock::new`. |
| application_advanced_query_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed advanced-query transaction boundary. |
| application_all_query_pages_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed all-pages query index boundary. |
| application_backlink_filter_context_ready | `crates/tine-core/src/sync_runtime.rs` | adapter | backlink_filter_entry | Hydrates cached roots and delegates entries to the shared DocBlock producer. |
| application_backlinks_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed backlink candidate/index boundary. |
| application_block_candidates_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Merges SQLite and pending UUID claimant sets. |
| application_block_children_by_identity | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Locates children in a Managed editor DTO tree. |
| application_block_reference_counts_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Merges Managed reference-count index and overlay. |
| application_block_referrers_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Merges Managed referrer index and hydrated pages. |
| application_crumb_line | `crates/tine-core/src/sync_runtime.rs` | adapter | crumb_line | Supplies format to the shared crumb renderer. |
| application_editor_blocks | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Sole Managed save-request block DTO constructor. |
| application_editor_blocks_existing | `crates/tine-core/src/sync_runtime.rs` | adapter | application_editor_blocks | Existing-save exposed-key policy delegates to the shared builder. |
| application_editor_blocks_new | `crates/tine-core/src/sync_runtime.rs` | adapter | application_editor_blocks | New-save generated-key policy delegates to the shared builder. |
| application_equivalent_page_names_ready | `crates/tine-core/src/sync_runtime.rs` | adapter | equivalent_page_names | Supplies Managed name-index candidates to the shared equivalence rule. |
| application_export_query_subtrees_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed export hydration and subtree budget boundary. |
| application_from_clean_foreground_commit | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Converts a Managed foreground commit result. |
| application_fuzzy_candidate_paths_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed fuzzy-name index boundary. |
| application_graph_search_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed search index and pending-overlay merge. |
| application_hydration_cache_budget_for_available | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed hydration-cache resource boundary. |
| application_hydration_retained_bytes | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed hydration-cache accounting boundary. |
| application_inventory_of_kind_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed kind-filtered inventory index. |
| application_inventory_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed materialized inventory plus overlay. |
| application_journal_feed | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed journal-feed state owner. |
| application_journal_feed_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed journal index plus pending overlay. |
| application_journal_naming | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed graph-config journal naming input. |
| application_load_outcome | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Converts exact Managed load state and revision. |
| application_materialized_read_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Opens the exact Managed SQLite frontier. |
| application_move_accepted | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed move durable-acceptance boundary. |
| application_move_batch_id | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed move batch identity boundary. |
| application_move_committed_outcome | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Converts a committed Managed move result. |
| application_move_request_digest | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Digests the Managed move request representation. |
| application_navigation | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Handle-side Managed navigation request boundary. |
| application_navigation | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Actor-side Managed navigation dispatch boundary. |
| application_navigation_aliases_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed alias index plus overlay. |
| application_navigation_overlay_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Decodes the committed-undrained Managed suffix. |
| application_navigation_pages_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed page inventory index plus overlay. |
| application_navigation_reference_names_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed reference-name index plus overlay. |
| application_orphan_assets_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed asset-reference index boundary. |
| application_page_block_reference_counts | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Counts references in a hydrated Managed page. |
| application_page_block_referrers | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Builds Managed referrer breadcrumbs from DTO trees. |
| application_page_identity_map | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Maps Managed editor identities for stable saves. |
| application_page_inventory | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Builds inventory from one hydrated Managed page. |
| application_page_inventory | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Builds inventory from one Managed projected page. |
| application_page_namespace_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed page namespace index boundary. |
| application_page_rename_sources_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed rename-source index boundary. |
| application_page_request_too_large | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed editor admission boundary. |
| application_pages_at_name_key_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed exact-name index boundary. |
| application_parser_indices_for_block_ids | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Maps Managed block IDs to parser positions. |
| application_preview_block_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed bounded-subtree preview boundary. |
| application_projection_roots | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Owns the cached complete DocBlock view for a Managed page. |
| application_property_facets_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed property-facet index plus overlay. |
| application_query_page_recency | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Computes recency from Managed path and graph config. |
| application_query_plan_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed query-plan/index preparation boundary. |
| application_request | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Handle-side Managed application request boundary. |
| application_resolve_blocks_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Materializes resolved Managed UUID groups. |
| application_simple_query_pages_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed simple-query candidate index boundary. |
| application_simple_query_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed simple-query execution boundary. |
| application_sparse_task_query_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed task-index sparse hydration boundary. |
| application_subtree_nodes | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Counts nodes in a Managed DTO subtree for admission. |
| application_templates_ready | `crates/tine-core/src/sync_runtime.rs` | adapter | application_page_templates | Supplies hydrated Managed pages to the canonical template walk. |
| application_unit_page_home_hints | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed unit-transaction page-location hints. |
| application_unlinked_candidate_strategy | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Chooses the Managed unlinked-reference index strategy. |
| application_unlinked_references_ready | `crates/tine-core/src/sync_runtime.rs` | necessary | — | Managed unlinked-reference candidate/index boundary. |
| application_blocks_have_content | `src-tauri/src/commands.rs` | necessary | — | Command adapter consumes application DTO trees without mode branching. |
| application_property_line | `src-tauri/src/commands.rs` | necessary | — | Command adapter parses application DTO property lines. |
| application_page_admission | `src-tauri/src/state.rs` | necessary | — | App-state admission boundary for Managed page payloads. |

## Retired producers

| symbol | former file | canonical replacement | packet item |
| --- | --- | --- | --- |
| application_backlink_filter_entry | `crates/tine-core/src/query.rs` | backlink_filter_entry | C7a-1 |
| template_dto_from_application | `crates/tine-core/src/query.rs` | template_dto | C7a-3 |

## UUID ownership policy

| boundary | exact outcome/rule | OG commit | OG path |
| --- | --- | --- | --- |
| Direct-ready | Physical projection is a hint; ambiguity falls back and parser-order first claimant owns the UUID. | c67b8b5fa47f8fe1e1954226c9bdfabd46ebb968 | `deps/graph-parser/src/logseq/graph_parser/block.cljs` (`fix-block-id-if-duplicated!`) |
| Direct-fallback | Parser-order first claimant owns the UUID. | c67b8b5fa47f8fe1e1954226c9bdfabd46ebb968 | `deps/graph-parser/src/logseq/graph_parser/block.cljs` (`fix-block-id-if-duplicated!`) |
| Managed-pending | Merge exact overlay and SQLite pages in graph path/tree order; first claimant owns the UUID. | c67b8b5fa47f8fe1e1954226c9bdfabd46ebb968 | `deps/graph-parser/src/logseq/graph_parser/block.cljs` (`fix-block-id-if-duplicated!`) |
| Managed-drained | A unique SQLite hint is accepted; ambiguity is resolved from pages in graph path/tree order, first claimant. | c67b8b5fa47f8fe1e1954226c9bdfabd46ebb968 | `deps/graph-parser/src/logseq/graph_parser/block.cljs` (`fix-block-id-if-duplicated!`) |

OG also declares `:block/uuid` unique identity in
`deps/db/src/logseq/db/schema.cljs`; the parser establishes which claimant keeps
the UUID before that identity enters the database.

## Template extraction policy

| boundary | exact outcome/rule | OG commit | OG path |
| --- | --- | --- | --- |
| Property extraction | Normalize property keys to lowercase and replace slash, spaces, and underscores with hyphens. | c67b8b5fa47f8fe1e1954226c9bdfabd46ebb968 | `deps/graph-parser/src/logseq/graph_parser/block.cljs` and `deps/graph-parser/src/logseq/graph_parser/extract.cljc` |
| Template discovery | A parsed block whose normalized property map contains `template` is a template; ordinary page pre-block text is not a block template. | c67b8b5fa47f8fe1e1954226c9bdfabd46ebb968 | `src/main/frontend/db/model.cljs` (`get-all-templates`) |

## Measured exception

`shallow_application_block` remains at exactly five syntactically pinned
Managed result boundaries. The DTO already contains parser-derived facets;
routing through `dto_block_to_doc_block` plus `block_to_shallow_dto` creates one
temporary `DocBlock` per result and reparses raw text. See
`managed_shallow_application_block_manual_probe` and
`measurement-shallow-application-block.txt`. This is a measured keep under D-4,
not permission for another shallow producer.

## `BlockDto::id` is a runtime handle, not a durable identity

The two modes put different values in this one wire field, and both are
correct:

| mode | `BlockDto::id` | durable `id::` |
| --- | --- | --- |
| Direct | the projection's `block_id` (a runtime identity; `direct_projection.rs` keeps `logseq_uuid` as a separate column) | in `properties`, unchanged |
| Managed | the durable `id::` uuid where the block has one | in `properties`, unchanged |

Nothing user-facing depends on the difference: every reference-producing path
goes through `ensureBlockId` (`src/store.ts`), which reads the existing `id::`
out of the block's raw text — case-insensitively — and never writes a second
one, precisely so a copied `((ref))` cannot dangle.

**The rule for new consumers:** treat `BlockDto::id` as a handle valid for the
current session and mode only. Anything that persists, exports, or compares a
block identity across modes must read the `id` entry of `properties`. The
real-graph gate `managed_c7a_real_graph_copy_manual_gate` normalizes this field
out of its parity oracle for exactly this reason; if a future packet needs the
two modes to agree on it, that is a deliberate identity change, not a bug fix.
