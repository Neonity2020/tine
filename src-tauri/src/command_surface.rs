//! The set of Tauri command names this app registers, as a checked constant.
//!
//! `diagnostic_ipc_event` records per-command IPC timings into the flight
//! recorder; the command name is a frontend-supplied string, and the recorder
//! only admits names from this closed set so a malformed or hostile caller
//! cannot grow the diagnostic vocabulary. The test below re-derives the list
//! from `tauri::generate_handler![…]` in `lib.rs` so it cannot drift from the
//! commands actually registered (the retired `managed_command_surface.rs`
//! enumerated the same set together with its Managed Storage routing; the
//! routing went with the subsystem on 2026-09-15).

const KNOWN_COMMANDS: &[&str] = &[
    "activate_absent_editor",
    "activate_editor",
    "app_architecture",
    "app_platform",
    "apply_journal_filename_migrations",
    "apply_spellcheck",
    "approve_external_assets",
    "asset_trash_stats",
    "begin_direct_cross_page_move",
    "block_ref_counts",
    "block_referrers",
    "cancel_graph_verification",
    "cancel_recording",
    "capture_frontend_ready",
    "capture_graph_binding",
    "capture_live_save_conflict",
    "capture_photo",
    "capture_quick_switch",
    "capture_target",
    "clear_diagnostics",
    "clipboard_files",
    "close_graph_window",
    "conflict_capsule_diff",
    "conflict_inventory",
    "copy_guide_into_graph",
    "copy_image_to_clipboard",
    "create_graph",
    "create_graph_verification",
    "debug_info",
    "debug_log",
    "default_graph_parent",
    "delete_page",
    "detect_media_editor",
    "diagnostic_frontend_event",
    "diagnostic_ipc_event",
    "diagnostic_report",
    "diagnostic_session_active",
    "duplicate_journal_diff",
    "durable_live_save_conflict_diff",
    "edit_asset_external",
    "empty_asset_trash",
    "existing_page_names",
    "export_query_subtrees",
    "finish_direct_cross_page_move",
    "forget_known_graph",
    "get_app_bool",
    "get_app_string",
    "get_backlink_filter_context",
    "get_backlinks",
    "get_backup_keep",
    "get_capture_enter_files",
    "get_link_first_match",
    "get_page",
    "get_page_by_path",
    "get_smooth_scroll",
    "get_unlinked_refs",
    "get_watch_mode",
    "gpu_env",
    "graph_source_files",
    "guide_pages",
    "import_asset",
    "import_native_capture",
    "indexing_progress",
    "inspect_graph_access",
    "install_plugin",
    "journal_content_days",
    "journal_feed_page",
    "list_backups",
    "list_installed_plugins",
    "list_journal_conflicts",
    "list_journal_filename_migrations",
    "list_known_graphs",
    "list_orphan_assets",
    "list_pages",
    "list_spellcheck_dictionaries",
    "list_templates",
    "live_save_conflict_diff",
    "load_conflict_capsules",
    "load_graph",
    "load_notices",
    "load_plugin_registry_cache",
    "load_session",
    "load_workspaces",
    "merge_pages",
    "open_asset",
    "open_external",
    "open_graph_window",
    "open_page_file",
    "open_pdf",
    "page_aliases",
    "page_icons",
    "page_print_html",
    "pick_graph_folder",
    "prepare_graph_folder",
    "present_conflict_override",
    "preview_block",
    "publish_html",
    "publish_query",
    "publish_query_plan",
    "query_explain_empty",
    "query_facets",
    "query_og_expressible",
    "query_parse",
    "query_print",
    "query_registry",
    "query_run",
    "quick_switch",
    "read_asset",
    "read_custom_css",
    "read_highlights",
    "read_journal_file",
    "read_local_image",
    "read_plugin_entry",
    "read_text_file",
    "referenced_page_names",
    "rename_file_to_page",
    "rename_page",
    "rescan_graph_now",
    "resolve_block",
    "resolve_blocks",
    "resolve_conflict_capsule",
    "resolve_duplicate_journal_day",
    "resolve_durable_live_save_conflict",
    "resolve_live_save_conflict",
    "resolve_sync_conflict",
    "resolve_vcs_marker_conflict",
    "restore_backup",
    "retire_conflict_capsule",
    "retire_editor_activation",
    "retry_index",
    "reveal_known_graph",
    "rollback_pdf_area_image",
    "run_advanced_query",
    "run_graph_search",
    "run_query",
    "save_asset",
    "save_diagnostic_report",
    "save_graph_verification_report",
    "save_notices",
    "save_page",
    "save_pdf_area_image",
    "save_session",
    "save_workspaces",
    "search",
    "set_app_bool",
    "set_app_string",
    "set_backup_keep",
    "set_capture_enter_files",
    "set_default_home",
    "set_default_journal_template",
    "set_doc_mode_enter_for_new_block",
    "set_favorites",
    "set_favorites_page",
    "set_guide_announced",
    "set_journal_title_format",
    "set_link_first_match",
    "set_logical_outdenting",
    "set_plugin_enabled",
    "set_preferred_format",
    "set_preferred_workflow",
    "set_show_brackets",
    "set_smooth_scroll",
    "set_start_of_week",
    "set_system_bar_appearance",
    "set_timetracking_enabled",
    "set_watch_mode",
    "start_recording",
    "startup_graph_path",
    "stop_recording",
    "store_conflict_capsule",
    "store_plugin_registry_cache",
    "stream_asset_path",
    "sync_conflict_diff",
    "take_data_home_fallback_notice",
    "take_identifier_migration_notice",
    "text_block_diff",
    "text_block_diff3",
    "tine_open_devtools",
    "tine_quit",
    "trash_asset",
    "trash_journal_file",
    "trash_sync_conflict",
    "uninstall_plugin",
    "vcs_marker_conflict_diff",
    "verify_plugin_registry",
    "warm_done",
    "watcher_latency_recent",
    "write_highlights",
    "write_pdf_view_state",
];

pub(crate) fn is_known_command(name: &str) -> bool {
    KNOWN_COMMANDS.binary_search(&name).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every name registered in `lib.rs`, module paths and `cfg` attributes
    /// stripped, on every target.
    fn registered_in_lib() -> BTreeSet<String> {
        let source = include_str!("lib.rs");
        let start = source
            .find("tauri::generate_handler![")
            .expect("lib.rs registers its commands through tauri::generate_handler!")
            + "tauri::generate_handler![".len();
        let mut depth = 1usize;
        let mut end = start;
        for (offset, byte) in source[start..].bytes().enumerate() {
            match byte {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        source[start..end]
            .lines()
            .map(|line| line.trim().trim_end_matches(','))
            .filter(|line| !line.is_empty() && !line.starts_with("//") && !line.starts_with("#["))
            .map(|line| line.rsplit("::").next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_known_command_list_is_exactly_what_lib_rs_registers() {
        let listed: BTreeSet<String> = KNOWN_COMMANDS.iter().map(|name| name.to_string()).collect();
        assert_eq!(listed.len(), KNOWN_COMMANDS.len(), "duplicate command name");
        assert_eq!(
            listed,
            registered_in_lib(),
            "command_surface.rs::KNOWN_COMMANDS must list exactly the commands lib.rs registers"
        );
    }

    #[test]
    fn the_list_is_sorted_so_lookup_can_binary_search() {
        assert!(KNOWN_COMMANDS.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(is_known_command("load_graph"));
        assert!(!is_known_command(""));
    }
}
