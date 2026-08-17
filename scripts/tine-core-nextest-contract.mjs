#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const LINUX_TINE_CORE_SHARD_COUNT = 4;

// `sync_runtime::tests` still contains the pre-0.7 adversarial actor as a
// differential oracle. Production no longer opens that actor, so its hundreds
// of implementation-shaped scenarios are not a release contract for the clean
// baseline-plus-manifest runtime. Keep the current production journeys exact
// and explicit here. Any new sync-runtime release test must be deliberately
// added; all other tine-core modules remain selected in full.
export const CLEAN_SYNC_RUNTIME_RELEASE_TEST_NAMES = Object.freeze([
  "sync_runtime::tests::activation_rediscovery_does_not_erase_the_physical_failure",
  "sync_runtime::tests::pre_promotion_receipt_retry_preserves_one_diagnostic_tree",
  "sync_runtime::tests::storage_contract_limits_receipt_rebuild_to_pre_enrollment_android_state",
  "sync_runtime::tests::pre_07_activation_oracle_has_no_production_entry_point",
  "sync_runtime::tests::clean_reopen_reads_an_unchanged_page_before_deferred_full_scan_catch_up",
  "sync_runtime::tests::clean_reopen_refuses_disk_divergence_without_hiding_it_behind_sqlite",
  "sync_runtime::tests::clean_reopen_negative_lookup_settles_closed_interval_addition_and_rename",
  "sync_runtime::tests::clean_runtime_factories_adopt_marker_and_cold_reopen_without_legacy_enrollment",
  "sync_runtime::tests::clean_actor_core_retains_one_manifested_save_until_projection_finishes",
  "sync_runtime::tests::clean_full_scan_yields_between_bounded_path_slices",
  "sync_runtime::tests::clean_shutdown_drains_a_full_scan_larger_than_the_generic_retry_limit",
  "sync_runtime::tests::clean_runtime_actor_assembles_without_legacy_authority_and_saves_one_edit",
  "sync_runtime::tests::clean_runtime_handle_serves_sqlite_queries_and_stops_without_legacy_handoff",
  "sync_runtime::tests::clean_runtime_cross_page_move_commits_once_and_cold_reopens",
  "sync_runtime::tests::clean_runtime_serves_regime_neutral_graph_pdf_and_guide_journeys",
  "sync_runtime::tests::retained_clean_reactivation_reports_each_recovery_boundary",
  "sync_runtime::tests::public_cold_open_prefers_clean_marker_without_discovering_legacy_enrollment",
  "sync_runtime::tests::public_fresh_activation_commits_clean_marker_and_skips_legacy_promotion",
  "sync_runtime::tests::public_clean_activation_loads_saves_and_cold_reopens_an_editor_page",
  "sync_runtime::tests::public_clean_activation_loads_and_saves_the_application_page_contract",
  "sync_runtime::tests::public_clean_runtime_reconciles_an_exact_external_edit_and_reopens_it",
  "sync_runtime::tests::public_clean_cold_open_discovers_an_external_edit_while_tine_was_closed",
  "sync_runtime::tests::public_clean_runtime_reconciles_external_create_delete_and_rename_as_one_batch",
  "sync_runtime::tests::shared_provider_clean_late_join_installs_provider_history_without_rewriting_graph",
  "sync_runtime::tests::shared_provider_clean_late_join_refuses_unmatched_local_graph_without_changing_authority",
  "sync_runtime::tests::shared_provider_clean_two_device_unicode_join_and_restart",
]);

// Architectural/contract guards that happen to live in `sync_runtime::tests`
// but are not part of the pre-0.7 oracle. Ten of them read `include_str!` of
// production source or of docs/storage-sync-contract.md; the eleventh
// enumerates every public durable open/activation refusal status and proves it
// carries a scenario. None can fail for legacy-actor reasons, and together they
// are what binds the §3.1 refusal table, the shadow-import publication order,
// and the "no UID coupling" rule to the code. Module membership alone used to
// keep every one of them outside the release gate.
export const SYNC_RUNTIME_CONTRACT_GUARD_TEST_NAMES = Object.freeze([
  "sync_runtime::tests::activation_contract_publishes_shadow_import_only_after_final_source_proof",
  "sync_runtime::tests::public_durable_refusal_scenarios_exactly_match_the_storage_contract",
  "sync_runtime::tests::every_public_durable_open_and_activation_class_has_a_scenario",
  "sync_runtime::tests::every_blocked_reason_literal_is_in_the_refusal_contract_vocabulary",
  "sync_runtime::tests::managed_save_refusal_origins_are_closed_over_the_permitted_slice",
  "sync_runtime::tests::managed_task_query_overlay_stays_at_exact_existing_page_seams",
  "sync_runtime::tests::actor_status_dispatch_is_snapshot_only_after_terminal",
  "sync_runtime::tests::query_rejects_over_limit_before_actor_queue_or_filesystem_work",
  "sync_runtime::tests::managed_local_foreground_source_excludes_legacy_and_derivative_work",
  "sync_runtime::tests::managed_storage_validation_is_not_unix_uid_coupled",
  "sync_runtime::tests::android_receipt_bootstrap_does_not_reenter_capability_preflights",
]);

// The exact set of tine-core tests the Linux release gate deliberately does NOT
// run: the pre-0.7 adversarial actor oracle and the scenario family around it.
// This list is the exclusion contract, and it is compared BY NAME against the
// real `cargo nextest list` inventory in both directions. A newly added healthy
// sync-runtime test therefore cannot be silently un-gated (it would be excluded
// without appearing here), and a renamed or deleted oracle test cannot rot on
// the list. Adding a name here is a deliberate un-gating decision: it removes
// that test from every release shard and from the PR gate.
export const PRE_07_SYNC_RUNTIME_EXCLUDED_TEST_NAMES = Object.freeze([
  "sync_runtime::tests::absent_superseded_head_settles_and_reappeared_head_retires_again",
  "sync_runtime::tests::accepted_audit_ingests_absent_local_manifest_before_checkpointing",
  "sync_runtime::tests::accepted_manifest_audit_checkpoint_resumes_after_crash_reopen",
  "sync_runtime::tests::accepted_non_tip_audit_retries_after_one_shot_provider_read_failure",
  "sync_runtime::tests::accepted_non_tip_audit_retries_after_one_shot_repair_publication_failure",
  "sync_runtime::tests::accepted_non_tip_object_loss_fences_checkpoint_until_repaired",
  "sync_runtime::tests::accepted_non_tip_revalidation_repairs_before_following_mutation",
  "sync_runtime::tests::accepted_ordinary_manifest_loss_without_local_archive_blocks",
  "sync_runtime::tests::activation_external_edit_before_promotion_refuses_then_retries_from_current_direct_files",
  "sync_runtime::tests::activation_progress_is_ordered_exact_byte_and_structurally_near_linear",
  "sync_runtime::tests::activation_retires_older_shadow_import_when_direct_files_changed_before_retry",
  "sync_runtime::tests::affine_before_projection_matches_forced_generic_application_save",
  "sync_runtime::tests::application_cross_page_move_cold_external_delete_requires_reopen",
  "sync_runtime::tests::application_cross_page_move_cold_guarded_conflict_transfers_to_exact_feed",
  "sync_runtime::tests::application_cross_page_move_cold_request_mismatch_cannot_adopt_manifest",
  "sync_runtime::tests::application_cross_page_move_cold_resume_preserves_unrelated_external_queue",
  "sync_runtime::tests::application_cross_page_move_conflicts_have_zero_semantic_commit",
  "sync_runtime::tests::application_cross_page_move_enforces_exact_destination_bounds",
  "sync_runtime::tests::application_cross_page_move_fault_cuts_recover_exactly_once",
  "sync_runtime::tests::application_cross_page_move_forced_batch_collision_fails_closed",
  "sync_runtime::tests::application_cross_page_move_immediate_cold_cuts_never_reauthor",
  "sync_runtime::tests::application_cross_page_move_preserves_identity_home_referrers_and_is_idempotent",
  "sync_runtime::tests::application_cross_page_move_resolves_after_rename_and_admission_change",
  "sync_runtime::tests::application_cross_page_move_same_actor_resolution_is_one_observation",
  "sync_runtime::tests::application_cross_page_move_same_actor_resolution_proves_no_commit",
  "sync_runtime::tests::application_cross_page_move_scaled_work_is_one_bounded_transaction",
  "sync_runtime::tests::application_cross_page_move_sidecar_failure_never_reaches_manifest",
  "sync_runtime::tests::application_cross_page_move_still_deferred_actor_remains_active",
  "sync_runtime::tests::application_delete_refuses_a_projection_race_after_exact_removal_base_capture",
  "sync_runtime::tests::application_delete_refuses_before_tombstone_when_typed_trash_publication_fails",
  "sync_runtime::tests::application_delete_refuses_malformed_trash_without_losing_the_source",
  "sync_runtime::tests::application_delete_trashes_exact_bytes_projected_by_managed_save",
  "sync_runtime::tests::application_gateway_does_not_admit_request_behind_prior_retained_publication",
  "sync_runtime::tests::application_gateway_inventory_and_loads_are_parser_owned_with_safe_block_identity",
  "sync_runtime::tests::application_gateway_join_preserves_parser_read_only_org",
  "sync_runtime::tests::application_gateway_saves_remap_new_ids_and_use_page_local_revisions",
  "sync_runtime::tests::application_gateway_settles_current_retained_publication_before_returning_saved",
  "sync_runtime::tests::application_graph_mutations_are_atomic_and_namespace_complete",
  "sync_runtime::tests::application_guide_copy_is_idempotent_and_keeps_markdown_in_org_graph",
  "sync_runtime::tests::application_navigation_observes_the_immediately_committed_frontier",
  "sync_runtime::tests::application_page_preflight_prepares_exact_511_without_writes_then_real_save_prepares_once",
  "sync_runtime::tests::application_page_preflight_rejects_depth_utf8_text_and_local_transaction_limits_without_writes",
  "sync_runtime::tests::application_search_lane_epoch_cancels_only_the_older_same_lane_request",
  "sync_runtime::tests::authenticated_path_owner_blocks_new_editor_page_when_sqlite_hides_it",
  "sync_runtime::tests::authority_revocation_keeps_published_local_continuation_terminal_and_unsafe",
  "sync_runtime::tests::canonical_retired_marker_refuses_while_its_active_anchor_still_exists",
  "sync_runtime::tests::clean_empty_managed_local_journal_keeps_catalog_cold_and_accepts_application_save",
  "sync_runtime::tests::clean_shutdown_refuses_until_retained_local_publication_resolves",
  "sync_runtime::tests::clean_shutdown_waits_for_imprecise_discovery_before_publishing_own_head",
  "sync_runtime::tests::closed_device_walks_only_an_unseen_linear_tail_from_latest_head",
  "sync_runtime::tests::closed_interval_edit_is_discovered_by_safe_reopen_before_next_safe",
  "sync_runtime::tests::cold_shared_descriptor_discovery_uses_the_canonical_supported_regular_file",
  "sync_runtime::tests::complete_namespace_loss_repair_above_head_scan_cap_is_chunked",
  "sync_runtime::tests::concurrent_explicit_and_filename_fallback_titles_converge_in_both_winner_directions",
  "sync_runtime::tests::concurrent_offline_canonical_equivalent_editor_titles_preserve_exact_semantics",
  "sync_runtime::tests::concurrent_watcher_observation_and_local_submission_are_linearly_reconciled",
  "sync_runtime::tests::converged_schema2_idle_ticks_and_ordinary_save_do_not_enumerate_history",
  "sync_runtime::tests::copy_provider_tree_rejects_root_symlink_without_opening_target",
  "sync_runtime::tests::copy_provider_tree_rejects_symlink_without_copying_target",
  "sync_runtime::tests::crash_after_an_accepted_import_can_still_take_over_its_own_projection",
  "sync_runtime::tests::deleted_own_frontier_head_is_republished_from_local_authority",
  "sync_runtime::tests::detailed_activation_progress_reports_preparation_parts_and_summary_observationally",
  "sync_runtime::tests::deterministic_published_failure_blocks_without_losing_published_work",
  "sync_runtime::tests::drained_schema2_history_below_threshold_does_not_compact",
  "sync_runtime::tests::dropping_without_shutdown_leaves_unsafe_and_fresh_open_must_take_over",
  "sync_runtime::tests::duplicate_effective_page_name_does_not_deny_the_whole_graph",
  "sync_runtime::tests::durable_shared_publication_survives_crash_before_provider_tick",
  "sync_runtime::tests::editor_content_saves_retain_accepted_identity_across_filename_and_journal_policy_changes",
  "sync_runtime::tests::editor_final_name_collisions_refuse_existing_and_new_saves_before_publication",
  "sync_runtime::tests::editor_intent_creates_page_and_journal_with_actor_owned_identity_and_order",
  "sync_runtime::tests::editor_intent_edits_inserts_moves_reorders_and_deletes_nested_outline",
  "sync_runtime::tests::editor_parser_authority_matrix_covers_markdown_org_title_and_kind_transitions",
  "sync_runtime::tests::editor_rejects_oversized_deep_cyclic_and_duplicate_keys_before_actor_enqueue",
  "sync_runtime::tests::editor_stale_base_after_external_watcher_edit_has_zero_save_effects",
  "sync_runtime::tests::editor_title_content_and_referrer_changes_share_one_atomic_user_authoritative_save",
  "sync_runtime::tests::editor_trusted_local_save_is_immediately_durable_and_derivatives_drain_once",
  "sync_runtime::tests::effective_title_projection_authority_is_point_bounded_with_a_wide_unrelated_frontier",
  "sync_runtime::tests::exact_deletion_of_an_accepted_manifest_republishes_from_local_archive",
  "sync_runtime::tests::exact_object_progress_rechecks_every_incomplete_manifest_once_per_wave",
  "sync_runtime::tests::exact_observation_uses_sole_queue_and_clean_shutdown_settles_safe_once_and_joins",
  "sync_runtime::tests::exact_stranded_managed_local_artifacts_converge_without_becoming_authority",
  "sync_runtime::tests::existing_safe_opens_one_owner_and_duplicate_or_foreign_binding_gets_no_authority",
  "sync_runtime::tests::explicit_local_activation_ignores_inert_legacy_v1_and_preserves_exact_bytes",
  "sync_runtime::tests::explicit_local_activation_preserves_nested_unicode_graph_bytes_without_retired_backup",
  "sync_runtime::tests::external_changes_outside_configured_roots_reconcile_without_flattening",
  "sync_runtime::tests::external_delete_after_unrelated_accepted_rename_settles_and_reaches_safe",
  "sync_runtime::tests::external_deletes_outside_configured_roots_reconcile_without_flattening",
  "sync_runtime::tests::first_external_change_publishes_an_ordinary_receipt_that_supersedes_after_restart",
  "sync_runtime::tests::first_local_edit_uses_bootstrap_then_ordinary_receipt_supersedes_after_restart",
  "sync_runtime::tests::foreign_incomplete_manifest_does_not_block_own_frontier_or_intent_retirement",
  "sync_runtime::tests::fresh_activation_application_save_preserves_empty_markdown_bullet_layout",
  "sync_runtime::tests::fresh_activation_retains_one_bootstrap_authority_and_zero_ordinary_page_receipts",
  "sync_runtime::tests::fresh_activation_save_preserves_nonleading_atx_headings_across_restart",
  "sync_runtime::tests::fresh_attack_round3_editor_title_change_must_preserve_graph_oplog_sqlite_equivalence",
  "sync_runtime::tests::fresh_managed_local_state_selects_v2_before_its_first_save_and_never_cleans_authority",
  "sync_runtime::tests::frontier_head_conflicts_fall_back_and_preserve_unreconciled_bytes",
  "sync_runtime::tests::frontier_head_crash_cuts_repair_before_safe_handoff",
  "sync_runtime::tests::generated_provider_conflicts_require_exact_canonical_byte_proof",
  "sync_runtime::tests::genuinely_foreign_v2_residue_remains_refused_without_private_reservation",
  "sync_runtime::tests::handle_is_cloneable_send_and_sync_while_actor_is_not_send_or_sync",
  "sync_runtime::tests::headless_legacy_namespace_falls_back_once_then_reopens_from_frontier_head",
  "sync_runtime::tests::immediate_managed_application_reply_matches_fresh_exact_parse_and_join_for_title_and_journal_identity",
  "sync_runtime::tests::imprecise_observation_during_provider_cursor_forces_a_second_scan",
  "sync_runtime::tests::interior_sqlite_counterfeit_cannot_become_durable_editor_truth",
  "sync_runtime::tests::interrupted_forensics_is_resumed_and_completed_rather_than_refused",
  "sync_runtime::tests::legacy_and_absent_discovery_start_no_actor_and_create_nothing",
  "sync_runtime::tests::legacy_rollover_crash_cuts_reopen_to_exactly_one_schema2_authority",
  "sync_runtime::tests::local_mutation_request_budgets_are_inclusive_and_diagnostics_are_capped",
  "sync_runtime::tests::locally_admitted_shared_object_precedes_own_frontier_publication",
  "sync_runtime::tests::malformed_retired_and_unknown_artifacts_are_left_in_place_and_surface_refusal",
  "sync_runtime::tests::managed_append_unknown_after_write_reopens_exactly_once_without_duplicate_append",
  "sync_runtime::tests::managed_append_unknown_before_write_latches_terminal_without_publishing_or_retrying",
  "sync_runtime::tests::managed_application_conflict_resolution_reauthors_retained_outline_at_one_observed_revision",
  "sync_runtime::tests::managed_application_save_benchmark_target_blocks_follow_the_public_save_boundary",
  "sync_runtime::tests::managed_application_save_is_semantically_and_structurally_page_local_across_graph_sizes",
  "sync_runtime::tests::managed_application_save_rebases_nested_unicode_markdown_and_org_with_bounded_page_local_reads",
  "sync_runtime::tests::managed_application_task_toggle_is_direct_and_durable",
  "sync_runtime::tests::managed_crash_reopen_synthetic_history_sweep_with_and_without_resume_point",
  "sync_runtime::tests::managed_graph_search_accounts_for_pending_overlay_metadata_separately",
  "sync_runtime::tests::managed_local_application_saves_are_one_frame_direct_and_drain_after_twelve_hot_revisions",
  "sync_runtime::tests::managed_local_compaction_faults_are_isolated_per_device",
  "sync_runtime::tests::managed_local_unsafe_reopen_restores_committed_overlay_before_feed_and_accepts_next_save",
  "sync_runtime::tests::managed_local_watcher_preserves_divergent_external_bytes_and_blocks_clean_handoff",
  "sync_runtime::tests::managed_new_page_conflict_resolution_uses_the_identifiable_winner_path_and_revision",
  "sync_runtime::tests::managed_path_shapes_survive_activation_crash_and_safe_reopen",
  "sync_runtime::tests::managed_projection_rebuild_resolves_documents_through_lookup_sessions",
  "sync_runtime::tests::managed_reopen_rebuilds_after_malformed_retained_scratch_blob",
  "sync_runtime::tests::managed_safe_reopen_rebuilds_a_corrupt_reconciliation_baseline",
  "sync_runtime::tests::managed_safe_reopen_rebuilds_a_deleted_disposable_projection",
  "sync_runtime::tests::managed_safe_reopen_rebuilds_an_unreadable_disposable_projection",
  "sync_runtime::tests::managed_save_conflict_and_deferred_wire_classifications_are_unchanged",
  "sync_runtime::tests::managed_save_debug_detail_preserves_the_bounded_public_refusal_contract",
  "sync_runtime::tests::managed_save_queue_refusals_distinguish_overflow_from_monotonicity",
  "sync_runtime::tests::managed_save_record_decode_has_an_exact_code",
  "sync_runtime::tests::managed_save_refusal_codes_are_bounded_and_render_through_application_error",
  "sync_runtime::tests::managed_sparse_task_query_fast_forwards_stale_masked_task_page",
  "sync_runtime::tests::managed_sparse_task_query_matches_direct_markdown_and_org_without_page_hydration",
  "sync_runtime::tests::managed_sparse_task_query_maximum_hot_overlay_is_candidate_bounded",
  "sync_runtime::tests::managed_sparse_task_query_one_match_in_twenty_thousand_blocks_is_candidate_bounded",
  "sync_runtime::tests::managed_task_query_overlay_tracks_exact_existing_pages_and_retires_after_drain",
  "sync_runtime::tests::managed_unsafe_reopen_at_the_accepted_frontier_opens_without_rebuilding",
  "sync_runtime::tests::manifest_recovery_conflicts_and_corruption_block_without_false_authority",
  "sync_runtime::tests::manifest_recovery_publication_crash_cuts_resume_before_canonical_visibility",
  "sync_runtime::tests::manifestless_no_op_partial_direct_dependency_blocks",
  "sync_runtime::tests::missing_projection_is_rebuilt_rather_than_refused",
  "sync_runtime::tests::mixed_schema1_rollover_then_schema2_compaction_retires_schema1_generation",
  "sync_runtime::tests::nested_unicode_markdown_external_edit_materializes_reaches_safe_and_reopens",
  "sync_runtime::tests::new_markdown_and_org_pages_are_born_with_parsed_final_identity_at_selected_path",
  "sync_runtime::tests::non_round_tripping_org_application_save_refuses_before_prepared_projection",
  "sync_runtime::tests::normal_startup_chunks_every_foreign_intent_before_head_publication_and_safe",
  "sync_runtime::tests::observed_receiver_external_edit_precedes_remote_delete_in_both_callback_orders",
  "sync_runtime::tests::only_a_committing_tick_reports_an_observable_change",
  "sync_runtime::tests::ordinary_external_edit_settles_its_epoch_and_still_reaches_a_safe_handoff",
  "sync_runtime::tests::out_of_scope_non_portable_graph_text_name_does_not_block_the_graph",
  "sync_runtime::tests::outbound_child_blocks_when_ordinary_parent_is_lost",
  "sync_runtime::tests::oversized_local_request_has_zero_actor_storage_graph_or_watcher_effects",
  "sync_runtime::tests::oversized_provider_callback_retains_scan_and_safe_shutdown_drains_it",
  "sync_runtime::tests::oversized_watcher_refusal_retains_a_full_scan_before_safe_shutdown",
  "sync_runtime::tests::peer_retired_publication_intent_settles_and_later_delivery_reaches_safe",
  "sync_runtime::tests::poll_mode_provider_observation_delivers_without_runtime_restart",
  "sync_runtime::tests::pre_enrollment_archive_residue_refuses_mismatched_identities_but_exact_resume_reaches_active",
  "sync_runtime::tests::prepared_editor_projection_seals_the_pending_local_predecessor_for_two_511_block_crlf_saves",
  "sync_runtime::tests::preserved_provider_conflict_copy_does_not_block_startup_reconciliation",
  "sync_runtime::tests::projected_restart_revision_remains_a_faithful_save_conflict_base",
  "sync_runtime::tests::provider_dependency_index_handles_multiple_parents_active_heads_and_duplicates",
  "sync_runtime::tests::provider_dependency_index_rechecks_authenticated_frontier_ancestors",
  "sync_runtime::tests::provider_object_physical_write_cut_requires_exact_journal_completion_before_manifest_and_head",
  "sync_runtime::tests::provider_projection_completed_by_application_read_still_notifies_live_views",
  "sync_runtime::tests::provider_staging_siblings_are_non_authoritative_for_exact_and_full_ingress",
  "sync_runtime::tests::provider_turns_reuse_retained_store_and_charge_only_exact_ingress",
  "sync_runtime::tests::public_activation_cut_after_archive_claim_before_enrollment_head_resumes_exact_identities",
  "sync_runtime::tests::public_activation_cut_after_shadow_import_publication_resumes_without_graph_rewrites",
  "sync_runtime::tests::public_activation_cut_after_verified_local_publication_resumes_without_graph_rewrites",
  "sync_runtime::tests::public_activation_cut_before_archive_creation_resumes_exact_identities_without_graph_rewrites",
  "sync_runtime::tests::public_local_mutation_journey_creates_edits_renames_and_deletes",
  "sync_runtime::tests::public_name_lookup_matches_og_names_across_nested_supported_extensions",
  "sync_runtime::tests::public_queries_are_bounded_serialized_and_read_the_exact_materialized_frontier",
  "sync_runtime::tests::published_local_failure_is_retained_and_retried_without_republication",
  "sync_runtime::tests::raw_colon_path_is_classified_incompatible_before_activation",
  "sync_runtime::tests::real_graph_copy_gate_rejects_root_symlink_before_canonicalization",
  "sync_runtime::tests::removing_rejected_exact_provider_residue_unblocks_queued_work",
  "sync_runtime::tests::reordered_remote_acceptance_cannot_reuse_stale_recovery_coverage",
  "sync_runtime::tests::repeated_local_page_rename_round_trip_reaches_durable",
  "sync_runtime::tests::restart_audit_finds_non_tip_accepted_manifest_loss_and_repairs_reappearance",
  "sync_runtime::tests::restarted_provider_child_accepts_manifestless_no_op_dependency_after_duplicate_reordering",
  "sync_runtime::tests::retained_recovery_repairs_missing_same_document_ancestor_for_stale_peer",
  "sync_runtime::tests::retained_tuple_removal_refuses_before_mutating_physical_tuple_after_active_name_is_absent",
  "sync_runtime::tests::retire_exact_refusal_leaves_every_active_anchor_and_tuple_byte_unchanged",
  "sync_runtime::tests::retired_generation_cleanup_cuts_reopen_on_greatest_authority_and_converge",
  "sync_runtime::tests::reverse_delivered_provider_chain_has_linear_readiness_work",
  "sync_runtime::tests::revocation_latches_terminal_drops_authority_and_refuses_later_intake",
  "sync_runtime::tests::safe_reopen_repairs_a_completely_lost_provider_namespace",
  "sync_runtime::tests::safe_reopen_repairs_settled_tip_manifest_lost_while_closed",
  "sync_runtime::tests::same_generation_schema1_and_schema2_authorities_are_refused",
  "sync_runtime::tests::schema1_rollover_uses_next_global_generation_and_reopen_selects_schema2_successor",
  "sync_runtime::tests::schema2_compaction_crash_cuts_reopen_on_the_exact_successor_without_duplicate_append",
  "sync_runtime::tests::schema2_drained_history_crossing_threshold_compacts_to_one_higher_successor",
  "sync_runtime::tests::schema2_two_compactions_retain_only_two_complete_generations_across_reopen",
  "sync_runtime::tests::settled_provider_history_over_shutdown_budget_is_not_replayed_on_reopen",
  "sync_runtime::tests::share_prepared_crash_resumes_descriptor_publication",
  "sync_runtime::tests::shared_join_manifest_disappearance_and_reappearance_never_blocks_enrollment",
  "sync_runtime::tests::shared_join_missing_or_tampered_local_authorship_receipt_is_retryable",
  "sync_runtime::tests::shared_join_owns_enrollment_transition_against_prepare_shared",
  "sync_runtime::tests::shared_join_owns_pending_state_against_a_conflicting_join",
  "sync_runtime::tests::shared_join_recovery_without_canonical_manifest_is_retryable",
  "sync_runtime::tests::shared_join_releases_ordinary_operation_ownership_between_turns",
  "sync_runtime::tests::shared_join_stable_independent_local_tail_remains_terminal",
  "sync_runtime::tests::shared_join_transient_manifest_absence_remains_local_active_across_restart",
  "sync_runtime::tests::shared_provider_archive_beyond_entry_and_byte_scan_caps_joins_incrementally",
  "sync_runtime::tests::shared_provider_new_nested_path_converges_after_duplicate_delivery_and_receiver_crash",
  "sync_runtime::tests::shared_receiver_local_crlf_layout_survives_reopen_and_successor_edit",
  "sync_runtime::tests::shared_receiver_preserves_exact_markdown_layout_and_can_author_successor",
  "sync_runtime::tests::sparse_task_query_dfs_keys_match_materializer_order_for_equal_parent_orders",
  "sync_runtime::tests::sqlite_name_counterfeit_cannot_authorize_or_block_editor_name_mutations",
  "sync_runtime::tests::startup_discovers_manifest_stranded_beyond_an_older_valid_frontier_head",
  "sync_runtime::tests::trusted_local_preparation_and_commit_substages_map_without_nested_detail",
  "sync_runtime::tests::two_offline_authors_union_frontier_heads_converge_without_return_first",
  "sync_runtime::tests::two_offline_devices_union_reordered_frontier_heads_without_history_scan",
  "sync_runtime::tests::unanchored_schema2_pairs_are_ignored_but_a_corrupt_highest_selector_fails_closed",
  "sync_runtime::tests::uncovered_legacy_head_backfills_recovery_in_bounded_chunks_before_safe",
  "sync_runtime::tests::uninterrupted_activation_reuses_complete_proof_but_fresh_reopen_revalidates",
  "sync_runtime::tests::unsafe_reopen_repairs_accepted_batch_after_pending_marker_creation_failure",
  "sync_runtime::tests::unsafe_takeover_cannot_run_with_old_owner_and_recovers_before_safe",
  "sync_runtime::tests::watcher_request_count_boundary_is_accepted_and_drained",
  "sync_runtime::tests::watcher_request_path_byte_overflow_is_retained",
]);

// What the release gate runs out of `sync_runtime::tests`: the clean-runtime
// journeys plus the contract guards.
export const SYNC_RUNTIME_RELEASE_TEST_NAMES = Object.freeze([
  ...CLEAN_SYNC_RUNTIME_RELEASE_TEST_NAMES,
  ...SYNC_RUNTIME_CONTRACT_GUARD_TEST_NAMES,
]);

export const LINUX_CORE_RELEASE_FILTERSET = [
  "not test(/sync_runtime::tests::/)",
  ...SYNC_RUNTIME_RELEASE_TEST_NAMES.map((testName) => `test(=${testName})`),
].join(" | ");

// Windows is deliberately not a second complete tine-core behavior matrix.
// Linux carries that full inventory in four isolated shards. This exact list is
// the Windows release contract: every explicitly Windows-named core test, plus
// the bootstrap/durability/lifecycle witnesses that exercised the Windows
// failures fixed for v0.6.90. Keep the names explicit so a rename, removal, or
// newly added Windows test cannot silently shrink the release gate.
export const WINDOWS_CORE_EXACT_TEST_NAMES = Object.freeze([
  "fail_before_projection_crash_windows_recover_without_unauthorized_execution",
  "model::tests::page_name_encoding_is_injective_reversible_and_windows_safe",
  "model::tests::windows_handle_relative_noreplace_renames_the_exact_source",
  "model::tests::windows_handle_relative_noreplace_moves_between_nonstandard_retained_directories_with_unicode",
  "model::tests::windows_handle_relative_noreplace_preserves_occupied_destination",
  "model::tests::windows_first_save_and_ordinary_rename_preserve_exact_projection",
  "model::tests::windows_directory_durability_limit_does_not_block_save_or_rename",
  "model::tests::checked_open_accepts_an_approved_windows_assets_junction",
  "model::tests::projection_windows_held_handle_link_count_tracks_one_and_two_links",
  "model::tests::windows_live_graph_root_move_is_denied_without_rebinding",
  "oplog::sqlite::tests::windows_entry_file_identity_classifies_reparse_lease_as_replaced",
  "windows_no_follow_publication_read_and_directory_flush_succeed",
  "windows_reparse_files_and_directories_are_rejected",
]);

export const WINDOWS_CORE_LIFECYCLE_WITNESS_NAMES = Object.freeze([
  "oplog::local_active::tests::a_live_promoted_runtime_blocks_every_newcomer_before_any_durable_write",
  "model::tests::bootstrap_source_regular_file_sync_uses_supported_handle_access",
  "oplog::import::tests::bootstrap_preparation_flush_handles_use_platform_durability_contracts",
  "oplog::import::tests::inactive_streaming_bootstrap_preseal_crash_retries_exactly",
  "oplog::import::tests::inactive_streaming_bootstrap_repeated_run_reuses_exact_seal",
  "oplog::local_active::tests::authoritative_snapshot_prunes_lock_namespaces_before_reading_files",
  "oplog::enrollment::tests::a_second_live_session_cannot_write_the_journal_and_dropping_one_releases_it",
  "oplog::sqlite::tests::separate_process_workspace_lease_contends_and_crash_releases",
]);

export const WINDOWS_CORE_CAPTURE_WITNESS_NAMES = Object.freeze([
  "model::tests::inactive_bootstrap_capture_exact_64_mib_sparse_file_is_accepted",
  "model::tests::inactive_bootstrap_capture_external_sort_is_buffer_bounded_without_real_files",
  "model::tests::inactive_bootstrap_capture_ignores_residue_is_idempotent_and_rejects_conflicting_seal",
  "model::tests::inactive_bootstrap_capture_is_deterministic_and_chunks_zero_one_and_many_files",
  "model::tests::inactive_bootstrap_capture_preserves_exact_nested_unicode_org_and_semantic_kinds",
  "model::tests::inactive_bootstrap_capture_rejects_bad_logical_name_frames",
  "model::tests::inactive_bootstrap_capture_seals_one_pass_and_final_proof_rejects_later_mutations",
  "model::tests::inactive_bootstrap_capture_rejects_file_cap_before_streaming",
]);

const WINDOWS_CORE_SMOKE_TEST_NAMES = Object.freeze([
  ...new Set([
    ...WINDOWS_CORE_EXACT_TEST_NAMES,
    ...WINDOWS_CORE_LIFECYCLE_WITNESS_NAMES,
    ...WINDOWS_CORE_CAPTURE_WITNESS_NAMES,
  ]),
]);

// The same declared names drive nextest and the inventory verifier. Do not
// replace this with a broad package filter: that would bring platform-neutral
// tests (including currently known Windows-incompatible ones) back into the
// release gate without an intentional policy change.
export const WINDOWS_CORE_SMOKE_FILTERSET = WINDOWS_CORE_SMOKE_TEST_NAMES
  .map((testName) => `test(=${testName})`)
  .join(" | ");

function fail(message) {
  throw new Error(`tine-core nextest contract: ${message}`);
}

function testKey(binaryId, testName) {
  return `${binaryId}\u0000${testName}`;
}

export function inventoryFromNextestList(packageName, list) {
  if (!list || typeof list !== "object" || !list["rust-suites"] || typeof list["rust-suites"] !== "object") {
    fail(`${packageName} list did not contain rust-suites`);
  }

  const tests = new Map();
  for (const [binaryId, suite] of Object.entries(list["rust-suites"])) {
    if (suite?.["package-name"] !== packageName) continue;
    if (!suite.testcases || typeof suite.testcases !== "object") {
      fail(`${packageName} binary ${binaryId} did not contain testcases`);
    }
    for (const [testName, testcase] of Object.entries(suite.testcases)) {
      // `cargo nextest run` does not run #[ignore] tests without an explicit
      // --run-ignored argument. Match that exact normal test-run inventory.
      if (testcase?.ignored || testcase?.["filter-match"]?.status !== "matches") continue;
      const key = testKey(binaryId, testName);
      if (tests.has(key)) fail(`${packageName} listed ${binaryId} ${testName} twice`);
      tests.set(key, { binaryId, testName });
    }
  }
  if (tests.size === 0) fail(`${packageName} selected no non-ignored tests`);
  return { packageName, tests };
}

export function verifyLinuxShardCoverage(fullInventory, shardInventories) {
  if (fullInventory?.packageName !== "tine-core") fail("Linux full inventory is not tine-core");
  if (!Array.isArray(shardInventories) || shardInventories.length !== LINUX_TINE_CORE_SHARD_COUNT) {
    fail(`expected ${LINUX_TINE_CORE_SHARD_COUNT} Linux shard inventories`);
  }

  const ownerByTest = new Map();
  for (const [index, shard] of shardInventories.entries()) {
    if (shard?.packageName !== "tine-core") fail(`Linux shard ${index + 1} is not tine-core`);
    if (shard.tests.size === 0) fail(`Linux shard ${index + 1} selected no tests`);
    for (const [key, test] of shard.tests) {
      if (!fullInventory.tests.has(key)) {
        fail(`Linux shard ${index + 1} selected non-inventory test ${test.binaryId} ${test.testName}`);
      }
      const priorOwner = ownerByTest.get(key);
      if (priorOwner !== undefined) {
        fail(`Linux shards ${priorOwner} and ${index + 1} both selected ${test.binaryId} ${test.testName}`);
      }
      ownerByTest.set(key, index + 1);
    }
  }

  const missing = [...fullInventory.tests.entries()]
    .filter(([key]) => !ownerByTest.has(key))
    .map(([, test]) => `${test.binaryId} ${test.testName}`);
  if (missing.length > 0) fail(`Linux shards omitted ${missing.length} tests (first: ${missing[0]})`);

  return { testCount: fullInventory.tests.size, shardCounts: shardInventories.map((shard) => shard.tests.size) };
}

export function verifyLinuxReleaseSelection(coreInventory, releaseInventory) {
  if (coreInventory?.packageName !== "tine-core") fail("Linux core inventory is not tine-core");
  if (releaseInventory?.packageName !== "tine-core") fail("Linux release inventory is not tine-core");

  requireNamesSelected(
    coreInventory,
    releaseInventory,
    SYNC_RUNTIME_RELEASE_TEST_NAMES,
    "Linux sync-runtime release selection"
  );
  for (const [key, test] of releaseInventory.tests) {
    if (!coreInventory.tests.has(key)) {
      fail(`Linux release selection contains non-inventory test ${test.binaryId} ${test.testName}`);
    }
  }

  const selectedSyncRuntime = [...releaseInventory.tests.values()]
    .filter((test) => test.testName.startsWith("sync_runtime::tests::"))
    .map((test) => test.testName);
  requireExactNameSet(
    selectedSyncRuntime,
    SYNC_RUNTIME_RELEASE_TEST_NAMES,
    "Linux sync-runtime release selection"
  );

  const excluded = [...coreInventory.tests.entries()]
    .filter(([key]) => !releaseInventory.tests.has(key))
    .map(([, test]) => test);
  if (excluded.length === 0) fail("Linux release selection did not isolate the legacy runtime oracle");
  const unexpected = excluded.find((test) => !test.testName.startsWith("sync_runtime::tests::"));
  if (unexpected) {
    fail(`Linux release selection excluded non-oracle test ${unexpected.binaryId} ${unexpected.testName}`);
  }

  // Module membership is not oracle-ness. The claim this contract has to make
  // is that every test the release gate drops is a NAMED, deliberately dropped
  // test, so compare the actual excluded names against the exclusion contract
  // in both directions: an unlisted exclusion means a healthy test was silently
  // un-gated, and a listed name with no test behind it means the list has
  // rotted. Names, never counts.
  requireExactNameSet(
    excluded.map((test) => test.testName),
    PRE_07_SYNC_RUNTIME_EXCLUDED_TEST_NAMES,
    "Linux release exclusion contract"
  );

  return {
    coreTestCount: coreInventory.tests.size,
    releaseTestCount: releaseInventory.tests.size,
    legacyOracleTestCount: excluded.length,
    cleanRuntimeTestCount: CLEAN_SYNC_RUNTIME_RELEASE_TEST_NAMES.length,
    contractGuardTestCount: SYNC_RUNTIME_CONTRACT_GUARD_TEST_NAMES.length,
  };
}

function testNames(inventory) {
  return [...inventory.tests.values()].map((test) => test.testName).sort();
}

function requireUniqueName(inventory, name) {
  const matching = [...inventory.tests.values()].filter((test) => test.testName === name);
  if (matching.length !== 1) {
    fail(`${inventory.packageName} must contain exactly one required test ${name}; found ${matching.length}`);
  }
  return matching[0];
}

function requireNamesSelected(fullInventory, selectedInventory, names, label) {
  for (const name of names) {
    const expected = requireUniqueName(fullInventory, name);
    if (!selectedInventory.tests.has(testKey(expected.binaryId, expected.testName))) {
      fail(`${label} omitted required test ${name}`);
    }
  }
}

function requireExactNameSet(actualNames, expectedNames, label) {
  const actual = [...actualNames].sort();
  const expected = [...expectedNames].sort();
  if (actual.length !== expected.length || actual.some((name, index) => name !== expected[index])) {
    const missing = expected.filter((name) => !actual.includes(name));
    const unexpected = actual.filter((name) => !expected.includes(name));
    fail(
      `${label} changed; missing [${missing.join(", ") || "none"}], unexpected [${unexpected.join(", ") || "none"}]`
    );
  }
}

export function verifyWindowsCoreSmokeSelection(coreInventory, smokeInventory) {
  if (coreInventory?.packageName !== "tine-core") fail("Windows core inventory is not tine-core");
  if (smokeInventory?.packageName !== "tine-core") fail("Windows core smoke inventory is not tine-core");

  const windowsNamed = [...coreInventory.tests.values()]
    .filter((test) => test.testName.toLowerCase().includes("windows"))
    .map((test) => test.testName);
  requireExactNameSet(windowsNamed, WINDOWS_CORE_EXACT_TEST_NAMES, "Windows-named tine-core test inventory");
  requireNamesSelected(coreInventory, smokeInventory, WINDOWS_CORE_SMOKE_TEST_NAMES, "Windows core smoke selection");
  requireExactNameSet(testNames(smokeInventory), WINDOWS_CORE_SMOKE_TEST_NAMES, "Windows core smoke selection");

  return {
    coreTestCount: coreInventory.tests.size,
    coreSmokeTestCount: smokeInventory.tests.size,
    windowsNamedCount: windowsNamed.length,
    bootstrapWitnessCount: WINDOWS_CORE_CAPTURE_WITNESS_NAMES.length,
  };
}

function nextestList(profile, packageName, { partition, filterset } = {}) {
  const args = ["nextest", "list", "--profile", profile, "--package", packageName, "--message-format", "json"];
  if (partition) args.push("--partition", partition);
  if (filterset) args.push("--filterset", filterset);
  const result = spawnSync("cargo", args, { cwd: process.cwd(), encoding: "utf8" });
  if (result.error) fail(`could not start cargo nextest list: ${result.error.message}`);
  if (result.status !== 0) {
    fail(`cargo nextest list for ${packageName}${partition ? ` (${partition})` : ""} failed:\n${result.stderr}`);
  }
  try {
    return inventoryFromNextestList(packageName, JSON.parse(result.stdout));
  } catch (error) {
    if (error instanceof SyntaxError) fail(`cargo nextest list for ${packageName} did not emit JSON: ${error.message}`);
    throw error;
  }
}

function runWindowsSmoke(packageName, filterset, label) {
  const result = spawnSync(
    "cargo",
    ["nextest", "run", "--profile", "ci-windows", "--package", packageName, "--filterset", filterset],
    { cwd: process.cwd(), stdio: "inherit" }
  );
  if (result.error) fail(`could not start ${label}: ${result.error.message}`);
  if (result.status !== 0) process.exit(result.status ?? 1);
}

function runLinuxSelection({ shard } = {}) {
  const args = [
    "nextest",
    "run",
    "--profile",
    "ci",
    "--package",
    "tine-core",
    "--filterset",
    LINUX_CORE_RELEASE_FILTERSET,
  ];
  if (shard !== undefined) args.push("--partition", `hash:${shard}/${LINUX_TINE_CORE_SHARD_COUNT}`);
  const label = shard === undefined ? "Linux tine-core release selection" : `Linux tine-core shard ${shard}`;
  const result = spawnSync("cargo", args, { cwd: process.cwd(), stdio: "inherit" });
  if (result.error) fail(`could not start ${label}: ${result.error.message}`);
  if (result.status !== 0) process.exit(result.status ?? 1);
}

function option(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

function main() {
  const mode = option("--mode");
  if (mode === "linux") {
    const core = nextestList("ci", "tine-core");
    const full = nextestList("ci", "tine-core", { filterset: LINUX_CORE_RELEASE_FILTERSET });
    const selection = verifyLinuxReleaseSelection(core, full);
    const shards = Array.from({ length: LINUX_TINE_CORE_SHARD_COUNT }, (_, index) =>
      nextestList("ci", "tine-core", {
        partition: `hash:${index + 1}/${LINUX_TINE_CORE_SHARD_COUNT}`,
        filterset: LINUX_CORE_RELEASE_FILTERSET,
      })
    );
    const result = verifyLinuxShardCoverage(full, shards);
    console.log(
      `Linux nextest contract OK: ${result.testCount} release tests exactly once across ${LINUX_TINE_CORE_SHARD_COUNT} hash shards (${result.shardCounts.join(", ")}); ${selection.cleanRuntimeTestCount} clean runtime journeys and ${selection.contractGuardTestCount} sync-runtime contract guards selected, and exactly the ${selection.legacyOracleTestCount} named pre-0.7 oracle tests isolated from the release gate.`
    );
    const runShard = option("--run-shard");
    if (runShard !== undefined) {
      const shard = Number(runShard);
      if (!Number.isInteger(shard) || shard < 1 || shard > LINUX_TINE_CORE_SHARD_COUNT) {
        fail(`--run-shard must be an integer from 1 to ${LINUX_TINE_CORE_SHARD_COUNT}`);
      }
      runLinuxSelection({ shard });
    }
    // The PR gate runs the whole verified selection in one job instead of four
    // sharded ones; both paths execute the identical filterset and `ci` profile.
    if (process.argv.includes("--run-selection")) runLinuxSelection();
    return;
  }
  if (mode === "windows") {
    const core = nextestList("ci-windows", "tine-core");
    const smoke = nextestList("ci-windows", "tine-core", { filterset: WINDOWS_CORE_SMOKE_FILTERSET });
    const result = verifyWindowsCoreSmokeSelection(core, smoke);
    console.log(
      `Windows nextest contract OK: ${result.coreTestCount} compiled tine-core tests, ${result.coreSmokeTestCount} contract-selected cross-layer smokes, ${result.windowsNamedCount} Windows-named core tests, and ${result.bootstrapWitnessCount} bootstrap capture witnesses.`
    );
    if (process.argv.includes("--run-smoke")) {
      runWindowsSmoke("tine-core", WINDOWS_CORE_SMOKE_FILTERSET, "Windows core/storage integration smoke");
    }
    return;
  }
  fail(
    "pass --mode linux (add --run-shard N for one release shard, or --run-selection for the whole verified release selection) or --mode windows (add --run-smoke to execute the verified Windows core/storage integration selection)"
  );
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
