//! The IPC name is a string on both sides, and nothing checked that the two
//! sides agree.
//!
//! `src/backend.ts` names every command it calls as a bare string literal;
//! `tauri::generate_handler![…]` in `lib.rs` names every command the app
//! actually registers. A typo, a rename that touched one side, or a handler
//! quietly dropped from the list all produce the same thing at runtime: an
//! `invoke` that rejects with "command not found", on whatever page happens to
//! call it. Nothing fails to compile, and no test notices.
//!
//! This module makes the two lists agree at test time. It re-derives both from
//! the sources — `command_surface.rs` is the sibling pattern — so the guard
//! cannot drift from the code it guards.
//!
//! It pins *names*, not argument shapes or return types. Those are separate
//! (and much larger) parity questions.

#[cfg(test)]
use std::collections::BTreeSet;

#[cfg(test)]
const BACKEND_TS: &str = include_str!("../../src/backend.ts");

#[cfg(test)]
const LIB_RS: &str = include_str!("lib.rs");

/// Registered handlers that `src/backend.ts` deliberately never calls.
///
/// Every entry must say where the command is really invoked from, or why
/// nothing invokes it. "It is unused" is not an acceptable entry — delete the
/// handler instead.
#[cfg(test)]
const NOT_CALLED_FROM_BACKEND_TS: &[(&str, &str)] = &[
    (
        "capture_frontend_ready",
        "the quick-capture mini-window is its own entry point and does not \
         construct the Backend abstraction; it invokes Tauri directly \
         (src/capture.tsx, `await invoke(\"capture_frontend_ready\")`)",
    ),
    (
        "watcher_latency_recent",
        "a diagnostics-only probe read straight from the app shell rather than \
         through the Backend interface (src/App.tsx, `invoke(\"watcher_latency_recent\")`)",
    ),
];

/// `this.call` / `this.invoke` sites in `src/backend.ts` whose command name is
/// NOT a string literal, and therefore cannot be checked by the scan.
///
/// Both of today's entries are the generic dispatcher plumbing itself, not
/// commands. A third entry means someone added a call whose name the guard
/// can no longer see — which is exactly the hole this module exists to close,
/// so adding one is a deliberate edit with a justification.
#[cfg(test)]
const DYNAMIC_CALL_SITES: &[&str] = &[
    // The injected Tauri `invoke` is stored on the instance at import time.
    "this.invoke = m.invoke;",
    // `call()` is the single funnel every named command goes through; `cmd`
    // here is the literal that one of the callers above already supplied.
    "result = await this.invoke<T>(cmd, leasedArgs);",
];

/// Every command name `src/backend.ts` passes to `this.call` / `this.invoke`
/// as a string literal, plus the source line of every call site whose name is
/// not a literal.
#[cfg(test)]
fn backend_ts_commands(source: &str) -> (BTreeSet<String>, Vec<String>) {
    let bytes: Vec<char> = source.chars().collect();
    let mut names = BTreeSet::new();
    let mut dynamic = Vec::new();

    let mut at = 0usize;
    while at < bytes.len() {
        let Some(start) = find_call_site(&bytes, at) else {
            break;
        };
        at = start + 1;
        let mut i = skip_ws(&bytes, next_after_call_site(&bytes, start));

        // An optional generic argument list: `<T>`, `<PageDto | null>`,
        // `<import("./types").JournalFeedPage>`. Balanced on angle brackets;
        // a `>` that closes a `=>` is not a bracket.
        if bytes.get(i) == Some(&'<') {
            let mut depth = 0i32;
            while i < bytes.len() {
                match bytes[i] {
                    '<' => depth += 1,
                    '>' if i > 0 && bytes[i - 1] != '=' => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
        }
        i = skip_ws(&bytes, i);
        if bytes.get(i) != Some(&'(') {
            dynamic.push(line_containing(source, start));
            continue;
        }
        i = skip_ws(&bytes, i + 1);
        if bytes.get(i) != Some(&'"') {
            dynamic.push(line_containing(source, start));
            continue;
        }
        let mut end = i + 1;
        while end < bytes.len() && bytes[end] != '"' {
            end += 1;
        }
        names.insert(bytes[i + 1..end].iter().collect::<String>());
    }
    (names, dynamic)
}

#[cfg(test)]
fn find_call_site(bytes: &[char], from: usize) -> Option<usize> {
    (from..bytes.len()).find(|index| {
        let rest = &bytes[*index..];
        (starts_with(rest, "this.call") && !is_ident_char(rest.get(9)))
            || (starts_with(rest, "this.invoke") && !is_ident_char(rest.get(11)))
    })
}

#[cfg(test)]
fn next_after_call_site(bytes: &[char], start: usize) -> usize {
    if starts_with(&bytes[start..], "this.invoke") {
        start + "this.invoke".len()
    } else {
        start + "this.call".len()
    }
}

#[cfg(test)]
fn starts_with(haystack: &[char], needle: &str) -> bool {
    needle
        .chars()
        .enumerate()
        .all(|(offset, expected)| haystack.get(offset) == Some(&expected))
}

#[cfg(test)]
fn is_ident_char(c: Option<&char>) -> bool {
    matches!(c, Some(c) if c.is_ascii_alphanumeric() || *c == '_')
}

#[cfg(test)]
fn skip_ws(bytes: &[char], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_whitespace() {
        i += 1;
    }
    i
}

#[cfg(test)]
fn line_containing(source: &str, char_index: usize) -> String {
    let byte_index = source
        .char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(source.len());
    let start = source[..byte_index].rfind('\n').map(|n| n + 1).unwrap_or(0);
    let end = source[byte_index..]
        .find('\n')
        .map(|n| byte_index + n)
        .unwrap_or(source.len());
    source[start..end].trim().to_owned()
}

/// Every `#[tauri::command]` in `src-tauri/src` whose body reaches
/// `state::refresh_graph` — i.e. every command that REOPENS the graph.
///
/// `src/backend.ts` keeps its own hand-written copy of this set
/// (`REBINDING_COMMANDS`) and its comment claimed each entry "was verified to
/// reach `refresh_graph`". Nothing checked it. A new Rust command that reopens
/// the graph and is not added to that set stops the frontend rebinding its
/// graph-scoped state — resolved paths and editor activations that belong to a
/// `Graph` which no longer exists.
///
/// The directory is walked rather than listed, because a list of sources is the
/// same hole one level up: a command added in a NEW file would be invisible.
#[cfg(test)]
fn commands_that_reopen_the_graph() -> BTreeSet<String> {
    let modules = crate::test_support::rust_module_sources();
    assert!(
        modules.len() > 10,
        "the src-tauri/src scan found {} sources -- the scanner broke, not the code",
        modules.len()
    );

    let mut names = BTreeSet::new();
    for (_, source) in modules {
        for (name, body) in tauri_command_bodies(&source) {
            if body.contains("refresh_graph(") {
                names.insert(name);
            }
        }
    }
    names
}

/// `(command name, function body)` for every `#[tauri::command]` in one source.
#[cfg(test)]
fn tauri_command_bodies(source: &str) -> Vec<(String, String)> {
    const ATTRIBUTE: &str = "#[tauri::command]";
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    // Only a line that IS the attribute counts. The same text appears inside doc
    // comments and assertion messages -- including in this very module.
    let mut attributes: Vec<usize> = Vec::new();
    let mut line_start = 0usize;
    for line in source.split_inclusive('\n') {
        if line.trim() == ATTRIBUTE {
            attributes.push(line_start + line.len());
        }
        line_start += line.len();
    }
    for search in attributes {
        // The name: the first `fn <ident>` after the attribute (any further
        // attributes, `pub(crate)`, `async` and so on lie between).
        let Some(fn_offset) = source[search..].find("fn ") else {
            continue;
        };
        let name_start = search + fn_offset + "fn ".len();
        let name_end = source[name_start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map(|end| name_start + end)
            .unwrap_or(source.len());
        let name = source[name_start..name_end].to_owned();

        // The body: the first `{` after the argument list and any return type,
        // then balanced braces. String and comment contents are not skipped;
        // this pins call sites, and a stray brace inside one would only ever
        // end a body early, which the assertion below would surface as a
        // missing command rather than a silent pass.
        let Some(open) = source[name_end..].find('{').map(|at| name_end + at) else {
            continue;
        };
        let mut depth = 0i32;
        let mut end = open;
        for (at, byte) in bytes[open..].iter().enumerate() {
            match byte {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + at;
                        break;
                    }
                }
                _ => {}
            }
        }
        out.push((name, source[open..=end].to_owned()));
    }
    out
}

/// The command names `src/backend.ts` lists in `REBINDING_COMMANDS`.
#[cfg(test)]
fn rebinding_commands(source: &str) -> BTreeSet<String> {
    let start = source
        .find("const REBINDING_COMMANDS = new Set([")
        .expect("src/backend.ts must declare REBINDING_COMMANDS as a literal Set");
    let open = start + source[start..].find('[').expect("checked above");
    let end = open
        + source[open..]
            .find("])")
            .expect("unterminated REBINDING_COMMANDS literal");
    let mut names = BTreeSet::new();
    let mut rest = &source[open..end];
    while let Some(quote) = rest.find('"') {
        let after = &rest[quote + 1..];
        let close = after.find('"').expect("unterminated string literal");
        names.insert(after[..close].to_owned());
        rest = &after[close + 1..];
    }
    names
}

/// Every command registered in `tauri::generate_handler![…]`, with module
/// paths (`android_media::capture_photo`) reduced to the IPC name Tauri
/// actually exposes, and `#[cfg(…)]`-gated entries included regardless of the
/// build target — the frontend calls them all.
#[cfg(test)]
fn registered_handlers(source: &str) -> BTreeSet<String> {
    let start = source
        .find("tauri::generate_handler![")
        .expect("lib.rs must register its commands through tauri::generate_handler!");
    let open = start + source[start..].find('[').expect("checked above");
    let mut depth = 0i32;
    let mut end = open;
    for (offset, c) in source[open..].char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = open + offset;
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(end > open, "unterminated generate_handler! list");

    let mut names = BTreeSet::new();
    for line in source[open + 1..end].lines() {
        let line = line.trim().trim_end_matches(',');
        if line.is_empty() || line.starts_with("//") || line.starts_with("#[") {
            continue;
        }
        names.insert(line.rsplit("::").next().expect("non-empty").to_owned());
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHASE_B_COMMANDS: &[(&str, &str)] = &[
        ("android_folder_picker.rs", "pick_graph_folder"),
        ("android_media.rs", "capture_photo"),
        ("android_media.rs", "start_recording"),
        ("android_media.rs", "stop_recording"),
        ("android_media.rs", "cancel_recording"),
        ("android_system_bars.rs", "set_system_bar_appearance"),
        ("backup.rs", "list_backups"),
        ("backup.rs", "restore_backup"),
        ("backup.rs", "set_backup_keep"),
        ("conflict_capsule.rs", "load_conflict_capsules"),
        ("conflict_capsule.rs", "retire_conflict_capsule"),
        ("conflict_capsule.rs", "store_conflict_capsule"),
        ("debug.rs", "clear_diagnostics"),
        ("debug.rs", "save_diagnostic_report"),
        ("graph.rs", "approve_external_assets"),
        ("graph.rs", "begin_direct_cross_page_move"),
        ("graph.rs", "capture_graph_binding"),
        ("graph.rs", "capture_target"),
        ("graph.rs", "create_graph"),
        ("graph.rs", "default_graph_parent"),
        ("graph.rs", "finish_direct_cross_page_move"),
        ("graph.rs", "indexing_progress"),
        ("graph.rs", "inspect_graph_access"),
        ("graph.rs", "load_graph"),
        ("graph.rs", "open_graph_window"),
        ("graph.rs", "warm_done"),
        ("graph_verification.rs", "cancel_graph_verification"),
        ("graph_verification.rs", "create_graph_verification"),
        ("graph_verification.rs", "save_graph_verification_report"),
        ("ios_folder_picker.rs", "pick_graph_folder"),
        ("ios_folder_picker.rs", "prepare_graph_folder"),
        ("lib.rs", "capture_frontend_ready"),
        ("platform.rs", "clipboard_files"),
        ("platform.rs", "copy_image_to_clipboard"),
        ("platform.rs", "open_external"),
        ("plugins.rs", "install_plugin"),
        ("plugins.rs", "read_plugin_entry"),
        ("plugins.rs", "set_plugin_enabled"),
        ("plugins.rs", "store_plugin_registry_cache"),
        ("plugins.rs", "uninstall_plugin"),
        ("plugins.rs", "verify_plugin_registry"),
        ("settings.rs", "forget_known_graph"),
        ("settings.rs", "load_session"),
        ("settings.rs", "reveal_known_graph"),
        ("settings.rs", "save_session"),
        ("settings.rs", "set_app_bool"),
        ("settings.rs", "set_app_string"),
        ("settings.rs", "set_capture_enter_files"),
        ("settings.rs", "set_link_first_match"),
        ("settings.rs", "set_smooth_scroll"),
        ("watcher.rs", "set_watch_mode"),
    ];

    const INFALLIBLE: &[&str] = &[
        "app_architecture",
        "app_platform",
        "apply_spellcheck",
        "debug_info",
        "debug_log",
        "diagnostic_frontend_event",
        "diagnostic_ipc_event",
        "diagnostic_report",
        "diagnostic_session_active",
        "get_app_bool",
        "get_app_string",
        "get_backup_keep",
        "get_capture_enter_files",
        "get_link_first_match",
        // SPEC §7.1: a total predicate over the IR it is handed. There is no
        // failure mode to report -- the OG DSL either can say this query or
        // cannot, and saying so is the whole point of the command.
        "query_og_expressible",
        "get_smooth_scroll",
        "get_watch_mode",
        "gpu_env",
        "list_installed_plugins",
        "list_known_graphs",
        "list_spellcheck_dictionaries",
        "load_plugin_registry_cache",
        "rescan_graph_now",
        "startup_graph_path",
        "take_data_home_fallback_notice",
        "take_identifier_migration_notice",
        "tine_open_devtools",
        "watcher_latency_recent",
    ];

    fn registered_for_target(source: &str, target: &str) -> Vec<String> {
        let start = source.find("tauri::generate_handler![").unwrap();
        let open = start + source[start..].find('[').unwrap();
        let mut depth = 0i32;
        let mut end = open;
        for (offset, c) in source[open..].char_indices() {
            match c {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        let mut cfg = "all";
        let mut paths = Vec::new();
        for line in source[open + 1..end].lines() {
            let line = line.trim().trim_end_matches(',');
            if line.starts_with("#[cfg(") {
                cfg = line;
                continue;
            }
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            let active = cfg == "all"
                || (cfg == "#[cfg(target_os = \"ios\")]" && target == "ios")
                || (cfg == "#[cfg(not(target_os = \"ios\"))]" && target != "ios");
            if active {
                paths.push(line.to_owned());
            }
            cfg = "all";
        }
        paths
    }

    fn command_signatures(source: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut attributes = Vec::new();
        let mut offset = 0usize;
        for line in source.split_inclusive('\n') {
            if line.trim() == "#[tauri::command]" {
                attributes.push(offset + line.len());
            }
            offset += line.len();
        }
        for after in attributes {
            let Some(fn_relative) = source[after..].find("fn ") else {
                break;
            };
            let name_start = after + fn_relative + 3;
            let name_end = source[name_start..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .map(|end| name_start + end)
                .unwrap();
            let Some(open) = source[name_end..].find('{').map(|at| name_end + at) else {
                break;
            };
            out.push((
                source[name_start..name_end].to_owned(),
                source[name_start..open].to_owned(),
            ));
        }
        out
    }

    fn command_definition(module_path: &str, name: &str) -> (String, String) {
        if module_path == "android_media" {
            let source = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/android_media.rs"),
            )
            .unwrap();
            assert_eq!(source.matches("pub(crate) async fn $name").count(), 2);
            assert!(
                source
                    .matches(") -> Result<MediaCaptureResult, crate::command_error::CommandError>")
                    .count()
                    >= 2
            );
            return (
                "android_media.rs".into(),
                "Result<MediaCaptureResult, crate::command_error::CommandError>".into(),
            );
        }
        let selected = if module_path.is_empty() {
            None
        } else {
            Some(format!("{module_path}.rs"))
        };
        let mut matches = Vec::new();
        for (file, source) in crate::test_support::rust_module_sources() {
            if selected.as_ref().is_some_and(|selected| selected != &file) {
                continue;
            }
            for (found, signature) in command_signatures(&source) {
                if found == name {
                    matches.push((file.clone(), signature));
                }
            }
        }
        matches.sort();
        matches.dedup();
        assert!(
            !matches.is_empty(),
            "registered command {module_path}::{name} must have a visible definition"
        );
        let shape = |signature: &str| {
            (
                signature.contains("Result<"),
                signature.contains("CommandError"),
                signature.contains("String>"),
            )
        };
        let expected = shape(&matches[0].1);
        assert!(
            matches
                .iter()
                .all(|(_, signature)| shape(signature) == expected),
            "cfg-split definitions for {module_path}::{name} disagree on error shape: {matches:?}"
        );
        matches.into_iter().next().unwrap()
    }

    #[test]
    fn phase_a_command_error_manifest_is_exact_for_every_target() {
        let commands = crate::test_support::rust_module_source_at(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands.rs"),
        );
        let state = include_str!("state.rs");
        let command_error = include_str!("command_error.rs");
        assert!(
            !commands.contains(&["map_err(|error| error", ".to_string())"].concat())
                && !commands.contains(&["map_err(|e| e", ".to_string())"].concat())
                && !state.contains(&["map_err(|error| error", ".to_string())"].concat())
                && !state.contains(&["map_err(|e| e", ".to_string())"].concat())
                && !commands.contains(&["CommandError::prose(error", ".to_string())"].concat())
                && !commands.contains("CommandError::prose(format!(\"{error}\"))")
                && !command_error.contains(&["From<", "String> for CommandError"].concat())
                && !command_error.contains(&["From<", "&str> for CommandError"].concat()),
            "I-9: phase-A typed sources must not route through String/Prose; imitate command_error.rs and direct_save_error_message"
        );

        let expected_infallible: BTreeSet<String> =
            INFALLIBLE.iter().map(|name| (*name).into()).collect();
        let mut seen_infallible = BTreeSet::new();
        for target in ["desktop", "android", "ios"] {
            for path in registered_for_target(LIB_RS, target) {
                let name = path.rsplit("::").next().unwrap();
                let module_path = path.rsplit_once("::").map_or("", |(module, _)| module);
                let (file, signature) = command_definition(module_path, name);
                let fallible = signature.contains("Result<");
                let compact: String = signature.chars().filter(|c| !c.is_whitespace()).collect();
                if !fallible {
                    assert!(
                        expected_infallible.contains(name),
                        "{target}: infallible command {name} needs a by-name allowlist entry"
                    );
                    seen_infallible.insert(name.to_owned());
                } else {
                    assert!(
                        compact.ends_with("CommandError>")
                            || compact.ends_with("CommandError,>"),
                        "{target}: every fallible command {file}::{name} must return the one CommandError"
                    );
                }
            }
        }
        assert_eq!(
            seen_infallible, expected_infallible,
            "stale infallible command allowlist row"
        );

        let direct_mapper = commands
            .split("pub(crate) fn direct_save_error_message")
            .nth(1)
            .unwrap()
            .split("/// Report what a slow")
            .next()
            .unwrap();
        let direct_mapper_tokens: String = direct_mapper
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            direct_mapper_tokens.contains("CommandError::tagged(\"save-conflict\"")
                && direct_mapper_tokens.contains("CommandError::tagged(\"direct-save-failure\"")
                && !direct_mapper.contains("CommandError::from"),
            "DirectSaveError must retain its closed code and epoch in Tagged, never Io/Prose"
        );

        let worker_mapper = ".map_err(CommandError::worker)";
        let mut rest = commands.as_str();
        while let Some(at) = rest.find(worker_mapper) {
            assert!(
                rest[..at].trim_end().ends_with(".await"),
                "Worker is reserved for spawn_blocking(...).await JoinError sites"
            );
            rest = &rest[at + worker_mapper.len()..];
        }
    }

    /// Calls into a native platform capability convert with a FAMILY
    /// CONSTRUCTOR, never `CommandError::from`.
    ///
    /// `CommandError` has `From` for `std::io::Error`, `tauri::Error`,
    /// `serde_json::Error` and the typed `Sync*` request errors — and for
    /// nothing else. `run_mobile_plugin` yields `PluginInvokeError` and
    /// `open_url` yields `tauri_plugin_opener::Error`; neither has a `From`
    /// impl, so `CommandError::from` at those sites does not compile.
    ///
    /// Why this needs a guard rather than the compiler: **every one of these
    /// sites is inside a `cfg` body no Linux host compiles.** W4-E2b wrote
    /// `CommandError::from` at nine of them and passed every local gate;
    /// hosted CI then failed Android with 9 errors and Windows with 1, and
    /// the two iOS sites were caught only by reading all five targets by hand
    /// because NO CI job compiles iOS (compare the `rename_noreplace` iOS gap).
    ///
    /// Exemplar to imitate: `android_media::call` —
    /// `.run_mobile_plugin(method, ()).map_err(CommandError::platform)`.
    /// `platform` is the family for OS capabilities; `plugin` means the Tine
    /// plugin system, whose only producer is `plugins.rs`.
    #[test]
    fn native_platform_calls_convert_through_a_family_constructor() {
        let mut offenders = Vec::new();
        let mut checked = 0_usize;
        for (file, source) in crate::test_support::rust_module_sources() {
            if file == "backend_command_parity.rs" {
                continue;
            }
            for call in ["run_mobile_plugin", ".open_url("] {
                let mut offset = 0;
                while let Some(relative) = source[offset..].find(call) {
                    let start = offset + relative;
                    offset = start + call.len();
                    checked += 1;
                    // The mapper is the next `CommandError::` after the call,
                    // within the same statement (up to the terminating `;` or
                    // the end of the expression block).
                    let tail = &source[start..];
                    let stop = tail.find(";\n").unwrap_or(tail.len().min(400));
                    let statement = &tail[..stop];
                    if !statement.contains("CommandError::") {
                        continue;
                    }
                    if statement.contains("CommandError::from") {
                        offenders.push(format!("{file}|{}", enclosing_symbol(&source[..start])));
                    }
                }
            }
        }
        assert!(
            checked >= 6,
            "the native-platform-call scan found only {checked} sites -- the scanner broke, \
             not the code"
        );
        assert!(
            offenders.is_empty(),
            "I-9 / platform-cfg rule: these native platform calls convert with \
             `CommandError::from`, which has no `From` impl for `PluginInvokeError` or \
             `tauri_plugin_opener::Error` and therefore does NOT COMPILE on the target that \
             selects the body -- a target your Linux gates never build. Use the family \
             constructor instead: `.map_err(CommandError::platform)`. Exemplar: \
             `android_media::call`. Offending sites (file|enclosing symbol): {offenders:?}"
        );
    }

    #[test]
    fn phase_b_command_error_manifest_is_exact_for_every_target() {
        let mut string_results = Vec::new();
        let mut phase_b_sources = Vec::new();
        for (file, source) in crate::test_support::rust_module_sources() {
            if result_error_is_string(&source) {
                string_results.push(file.clone());
            }
            phase_b_sources.push((file, source));
        }
        assert!(
            string_results.is_empty(),
            "I-9: src-tauri String error results must be structurally zero; found {string_results:?}"
        );
        let (site_count, site_fingerprint, site_rows) =
            phase_b_mapper_site_fingerprint(&phase_b_sources);
        // Count and placement are pinned separately on purpose: an equal count
        // with a different fingerprint is the count-preserving swap (a Plugin
        // failure mapped as Backup), which is the vacuous implementation this
        // whole packet exists to make unwritable.
        assert_eq!(
            (site_count, site_fingerprint),
            (240, 3_095_691_140_800_970_953),
            "I-9: phase-B mapper sites drifted. Each row is file|enclosing symbol|mapper, \
             sorted, with NO line numbers — so this cannot be pure line drift; a mapper \
             genuinely moved, changed family, appeared or disappeared. Diff these against \
             the last accepted run and re-pin both values only once every row is intended:\n{}",
            site_rows.join("\n")
        );

        let expected_phase_b: BTreeSet<(String, String)> = PHASE_B_COMMANDS
            .iter()
            .map(|(file, name)| ((*file).into(), (*name).into()))
            .collect();
        let mut seen_phase_b = BTreeSet::new();
        for target in ["desktop", "android", "ios"] {
            for path in registered_for_target(LIB_RS, target) {
                let name = path.rsplit("::").next().unwrap();
                let module_path = path.rsplit_once("::").map_or("", |(module, _)| module);
                let (file, signature) = command_definition(module_path, name);
                let compact: String = signature.chars().filter(|c| !c.is_whitespace()).collect();
                if expected_phase_b.contains(&(file.clone(), name.to_owned())) {
                    assert!(
                        compact.ends_with("CommandError>") || compact.ends_with("CommandError,>"),
                        "{target}: phase-B command {file}::{name} must return CommandError"
                    );
                    seen_phase_b.insert((file, name.to_owned()));
                }
            }
        }
        assert_eq!(
            seen_phase_b, expected_phase_b,
            "phase-B command manifest drift"
        );

        let compact_sources: Vec<(String, String)> = phase_b_sources
            .iter()
            .map(|(file, source)| {
                (
                    file.clone(),
                    source.chars().filter(|c| !c.is_whitespace()).collect(),
                )
            })
            .collect();
        for (_, compact) in &compact_sources {
            assert!(!compact.contains(&["map_err(|error|error", ".to_string())"].concat()));
            assert!(!compact.contains(&["map_err(|e|e", ".to_string())"].concat()));
            assert!(!compact.contains(&["CommandError::prose(error", ".to_string())"].concat()));
        }
        let command_error = include_str!("command_error.rs");
        assert!(!command_error.contains(&["From<", "String> for CommandError"].concat()));
        assert!(!command_error.contains(&["From<", "&str> for CommandError"].concat()));

        for (file, source) in &phase_b_sources {
            if matches!(
                file.as_str(),
                "commands.rs" | "state.rs" | "command_error.rs" | "backend_command_parity.rs"
            ) {
                continue;
            }
            assert_eq!(assert_mapper_placement(file, source), None);
            let worker = ".map_err(crate::command_error::CommandError::worker)";
            let mut rest = source.as_str();
            while let Some(at) = rest.find(worker) {
                assert!(
                    rest[..at].trim_end().ends_with(".await"),
                    "{file}: Worker is reserved for spawn_blocking(...).await JoinError sites"
                );
                rest = &rest[at + worker.len()..];
            }
        }

        let plugins = include_str!("plugins.rs").replacen(
            "CommandError::plugin(",
            "CommandError::backup(",
            1,
        );
        let backup =
            include_str!("backup.rs").replacen("CommandError::backup(", "CommandError::plugin(", 1);
        assert!(assert_mapper_placement("plugins.rs", &plugins).is_some());
        assert!(assert_mapper_placement("backup.rs", &backup).is_some());
    }

    fn assert_mapper_placement(file: &str, source: &str) -> Option<String> {
        const PLACEMENT: &[(&str, &[&str])] = &[
            ("plugin", &["plugins.rs"]),
            ("clipboard", &["platform.rs"]),
            (
                "platform",
                &[
                    "platform.rs",
                    "lib.rs",
                    "android_folder_picker.rs",
                    "android_media.rs",
                    "android_system_bars.rs",
                    "ios_folder_picker.rs",
                ],
            ),
            ("graph_verification", &["graph_verification.rs"]),
            ("graph", &["graph.rs", "watcher.rs"]),
            ("storage_transition", &["storage_transition_supervisor.rs"]),
            ("settings", &["settings.rs"]),
            ("diagnostic", &["debug.rs"]),
            ("backup", &["backup.rs"]),
            (
                "json",
                &[
                    "conflict_capsule.rs",
                    "graph_verification.rs",
                    "plugins.rs",
                    "settings.rs",
                ],
            ),
        ];
        for (mapper, allowed) in PLACEMENT {
            let needle = format!("CommandError::{mapper}(");
            if source.contains(&needle) && !allowed.contains(&file) {
                return Some(format!("{file}: mapper {mapper} belongs to {allowed:?}"));
            }
        }
        None
    }

    /// `(file, enclosing symbol, mapper)` for every phase-B mapper use — sorted,
    /// so it is stable under line drift. Deliberately NOT `(file, line)`: a
    /// line-anchored census reddens on any edit above a site, which is how the
    /// print census cost this campaign five re-anchoring rounds in one day.
    ///
    /// The rows come back beside the hash because a bare 64-bit mismatch tells
    /// the next reader nothing about WHAT moved. Print them and diff.
    fn phase_b_mapper_site_fingerprint(sources: &[(String, String)]) -> (usize, u64, Vec<String>) {
        let mut rows = Vec::new();
        for (file, source) in sources {
            if matches!(
                file.as_str(),
                "commands.rs" | "state.rs" | "command_error.rs" | "backend_command_parity.rs"
            ) {
                continue;
            }
            let mut offset = 0;
            while let Some(relative) = source[offset..].find("CommandError::") {
                let start = offset + relative;
                let mapper_start = start + "CommandError::".len();
                let mapper_end = source[mapper_start..]
                    .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                    .map(|end| mapper_start + end)
                    .unwrap_or(source.len());
                let mapper = &source[mapper_start..mapper_end];
                if mapper.chars().next().is_some_and(char::is_lowercase) {
                    let symbol = enclosing_symbol(&source[..start]);
                    rows.push(format!("{file}|{symbol}|{mapper}"));
                }
                offset = mapper_end;
            }
        }
        rows.sort();
        let mut hash = 0xcbf29ce484222325_u64;
        for row in &rows {
            for byte in row.bytes().chain(std::iter::once(b'\n')) {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
        (rows.len(), hash, rows)
    }

    fn enclosing_symbol(prefix: &str) -> &str {
        let Some(start) = prefix.rfind("fn ").map(|at| at + 3) else {
            return "<module>";
        };
        let end = prefix[start..]
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'))
            .map(|end| start + end)
            .unwrap_or(prefix.len());
        &prefix[start..end]
    }

    fn result_error_is_string(source: &str) -> bool {
        let mut rest = source;
        while let Some(start) = rest.find("Result<") {
            rest = &rest[start + "Result<".len()..];
            let mut depth = 1_i32;
            let mut last_comma = None;
            let mut end = None;
            for (index, byte) in rest.bytes().enumerate() {
                match byte {
                    b'<' => depth += 1,
                    b'>' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(index);
                            break;
                        }
                    }
                    b',' if depth == 1 => last_comma = Some(index),
                    _ => {}
                }
            }
            let Some(end) = end else { return false };
            if last_comma.is_some_and(|comma| rest[comma + 1..end].trim() == "String") {
                return true;
            }
            rest = &rest[end + 1..];
        }
        false
    }

    /// Why each remaining sync command may run on the main thread. A sync
    /// `#[tauri::command]` runs there and freezes every window until it returns
    /// (GH #332 measured 10-16 s; GH #543 R6-02 waited out a whole index
    /// pass). Anything that walks the graph, waits on the index, a lane or a
    /// subprocess, or reads a file the call did not size is `async` +
    /// `spawn_blocking` instead (exemplar: `conflict_inventory` in commands.rs).
    const SYNC_COMMANDS: &[(&str, &str)] = {
        const CONFIG: &str =
            "one config.edn write; fire-and-forget callers rely on main-thread order";
        const APP_FILE: &str =
            "one small app-private file; bursty callers rely on main-thread order";
        const ONE_FILE: &str = "stats, reads, moves or opens one named file";
        const PAYLOAD: &str = "writes the bytes this call carried";
        const STATE: &str = "atomics, a registry read or a channel send";
        const PURE: &str = "no filesystem work beyond a bounded app-private read";
        &[
            ("app_architecture", PURE),
            ("app_platform", PURE),
            ("apply_spellcheck", STATE),
            ("approve_external_assets", ONE_FILE),
            ("cancel_graph_verification", STATE),
            ("capture_frontend_ready", STATE),
            ("capture_graph_binding", STATE),
            ("capture_target", STATE),
            ("clear_diagnostics", PURE),
            ("close_graph_window", STATE),
            ("copy_image_to_clipboard", PAYLOAD),
            (
                "create_graph",
                "writes a fixed-size demo scaffold into a new folder",
            ),
            ("debug_info", PURE),
            ("debug_log", PURE),
            ("default_graph_parent", ONE_FILE),
            (
                "detect_media_editor",
                "stats a fixed list of install locations",
            ),
            ("diagnostic_frontend_event", PURE),
            ("diagnostic_ipc_event", PURE),
            ("diagnostic_report", PURE),
            ("diagnostic_session_active", PURE),
            ("edit_asset_external", ONE_FILE),
            ("forget_known_graph", APP_FILE),
            ("get_app_bool", APP_FILE),
            ("get_app_string", APP_FILE),
            ("get_backup_keep", APP_FILE),
            ("get_capture_enter_files", APP_FILE),
            ("get_link_first_match", APP_FILE),
            ("get_smooth_scroll", APP_FILE),
            ("get_watch_mode", APP_FILE),
            ("gpu_env", PURE),
            ("guide_pages", PURE),
            ("indexing_progress", STATE),
            ("inspect_graph_access", ONE_FILE),
            ("install_plugin", PAYLOAD),
            ("list_known_graphs", APP_FILE),
            ("load_conflict_capsules", APP_FILE),
            ("load_notices", APP_FILE),
            ("load_plugin_registry_cache", APP_FILE),
            ("load_session", APP_FILE),
            ("load_workspaces", APP_FILE),
            ("open_asset", ONE_FILE),
            ("open_external", STATE),
            ("read_custom_css", ONE_FILE),
            ("read_highlights", ONE_FILE),
            ("read_journal_file", ONE_FILE),
            ("read_plugin_entry", ONE_FILE),
            ("read_text_file", "one CSV/TSV file, capped at 10 MiB"),
            ("rescan_graph_now", STATE),
            ("retire_conflict_capsule", APP_FILE),
            ("reveal_known_graph", STATE),
            ("rollback_pdf_area_image", ONE_FILE),
            ("save_asset", PAYLOAD),
            ("save_notices", APP_FILE),
            ("save_pdf_area_image", PAYLOAD),
            ("save_session", APP_FILE),
            ("save_workspaces", APP_FILE),
            ("set_app_bool", APP_FILE),
            ("set_app_string", APP_FILE),
            ("set_capture_enter_files", APP_FILE),
            ("set_default_home", CONFIG),
            ("set_default_journal_template", CONFIG),
            ("set_doc_mode_enter_for_new_block", CONFIG),
            ("set_favorites", CONFIG),
            ("set_favorites_page", CONFIG),
            ("set_guide_announced", CONFIG),
            ("set_link_first_match", APP_FILE),
            ("set_logical_outdenting", CONFIG),
            ("set_plugin_enabled", APP_FILE),
            ("set_preferred_format", CONFIG),
            ("set_journal_title_format", CONFIG),
            ("set_preferred_workflow", CONFIG),
            ("set_show_brackets", CONFIG),
            ("set_smooth_scroll", APP_FILE),
            ("set_start_of_week", CONFIG),
            ("set_timetracking_enabled", CONFIG),
            ("set_watch_mode", APP_FILE),
            ("store_conflict_capsule", APP_FILE),
            ("store_plugin_registry_cache", APP_FILE),
            ("stream_asset_path", ONE_FILE),
            ("take_data_home_fallback_notice", STATE),
            ("take_identifier_migration_notice", STATE),
            ("tine_open_devtools", STATE),
            (
                "tine_quit",
                "exits; the exit drain is bounded by EXIT_DRAIN_BUDGET",
            ),
            ("trash_asset", ONE_FILE),
            ("trash_sync_conflict", ONE_FILE),
            ("uninstall_plugin", ONE_FILE),
            ("verify_plugin_registry", PURE),
            ("warm_done", STATE),
            ("watcher_latency_recent", PURE),
        ]
    };

    /// `(name, is async, body)` for every `#[tauri::command]` in `src-tauri/src`.
    fn every_command() -> Vec<(String, bool, String)> {
        let mut out = Vec::new();
        for (_, source) in crate::test_support::rust_module_sources() {
            let source = crate::test_support::without_cfg_test_items(&source);
            for (name, body) in tauri_command_bodies(&source) {
                let head = &source[..source.find(&body).unwrap()];
                let head = &head[head.rfind("#[tauri::command]").unwrap()..];
                let signature = &head[..head.find(&format!("fn {name}")).unwrap()];
                let is_async = signature.split_whitespace().any(|word| word == "async");
                out.push((name, is_async, body));
            }
        }
        assert!(
            out.len() > 150,
            "the command scan found only {} commands -- the scanner broke, not the code",
            out.len()
        );
        out
    }

    #[test]
    fn every_sync_command_says_why_it_may_run_on_the_main_thread() {
        let sync: BTreeSet<String> = every_command()
            .into_iter()
            .filter(|(_, is_async, _)| !is_async)
            .map(|(name, _, _)| name)
            .collect();
        let listed: BTreeSet<String> = SYNC_COMMANDS
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect();
        let unlisted: Vec<&String> = sync.difference(&listed).collect();
        assert!(
            unlisted.is_empty(),
            "these sync #[tauri::command]s run on the main thread and freeze every window \
             until they return. Make each `async` + `spawn_blocking` (exemplar: \
             conflict_inventory in commands.rs), or add it to SYNC_COMMANDS with the reason \
             it is bounded (GH #543, R6-02): {unlisted:?}"
        );
        let stale: Vec<&String> = listed.difference(&sync).collect();
        assert!(
            stale.is_empty(),
            "SYNC_COMMANDS lists commands that are no longer sync commands: {stale:?}"
        );
    }

    /// An async command runs on a runtime worker; its blocking work belongs in
    /// `spawn_blocking` (`open_graph_window` once opened a graph inline).
    #[test]
    fn async_commands_open_graphs_inside_spawn_blocking() {
        for (name, is_async, body) in every_command() {
            let Some(call) = body.find("load_graph_for_label(") else {
                continue;
            };
            assert!(
                is_async && body[..call].contains("spawn_blocking("),
                "{name} opens a graph outside spawn_blocking; exemplar: load_graph in graph.rs"
            );
        }
    }

    /// Everything the frontend asks for must exist. A name that is not
    /// registered fails at runtime with "command not found", on whatever page
    /// happens to call it.
    #[test]
    fn every_command_backend_ts_calls_is_registered() {
        let (called, _) = backend_ts_commands(BACKEND_TS);
        let registered = registered_handlers(LIB_RS);
        assert!(
            !called.is_empty(),
            "the backend.ts scan found no commands at all -- the scanner broke, not the code"
        );
        let missing: Vec<&String> = called.difference(&registered).collect();
        assert!(
            missing.is_empty(),
            "src/backend.ts invokes commands that tauri::generate_handler! does not register \
             (they fail at runtime with `command not found`): {missing:?}"
        );
    }

    /// And the other direction: a registered handler with no caller is either
    /// invoked from somewhere other than `backend.ts` -- which has to be said
    /// out loud -- or it is dead weight nobody noticed.
    #[test]
    fn every_registered_command_has_a_declared_caller() {
        let (called, _) = backend_ts_commands(BACKEND_TS);
        let registered = registered_handlers(LIB_RS);
        let allowed: BTreeSet<String> = NOT_CALLED_FROM_BACKEND_TS
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect();

        let unexplained: Vec<&String> = registered
            .iter()
            .filter(|name| !called.contains(*name) && !allowed.contains(*name))
            .collect();
        assert!(
            unexplained.is_empty(),
            "commands are registered but never called from src/backend.ts; either wire them up \
             or add them to NOT_CALLED_FROM_BACKEND_TS with the file that does call them: \
             {unexplained:?}"
        );

        let stale: Vec<&&str> = NOT_CALLED_FROM_BACKEND_TS
            .iter()
            .map(|(name, _)| name)
            .filter(|name| !registered.contains(**name) || called.contains(**name))
            .collect();
        assert!(
            stale.is_empty(),
            "NOT_CALLED_FROM_BACKEND_TS entries that are no longer true (unregistered, or now \
             called from backend.ts): {stale:?}"
        );
    }

    /// `REBINDING_COMMANDS` is a claim about the backend: "each of these was
    /// verified to reach `refresh_graph`". Verify it, both ways. A command that
    /// reopens the graph but is missing here leaves the frontend holding
    /// graph-scoped state for a `Graph` that no longer exists; a stale entry
    /// makes the frontend throw away live state for no reason.
    #[test]
    fn rebinding_commands_are_exactly_the_commands_that_reopen_the_graph() {
        let declared = rebinding_commands(BACKEND_TS);
        let actual = commands_that_reopen_the_graph();
        assert!(
            !actual.is_empty(),
            "no command was found to call refresh_graph -- the scanner broke, not the code"
        );

        let missing: Vec<&String> = actual.difference(&declared).collect();
        assert!(
            missing.is_empty(),
            "these #[tauri::command]s reopen the graph but are not in \
             REBINDING_COMMANDS in src/backend.ts, so the frontend never rebinds \
             its graph-scoped state after they return: {missing:?}"
        );

        let stale: Vec<&String> = declared.difference(&actual).collect();
        assert!(
            stale.is_empty(),
            "these REBINDING_COMMANDS entries no longer reach refresh_graph; the \
             frontend discards live graph-scoped state for nothing: {stale:?}"
        );
    }

    /// GH #543 (indexing audit IT-06, R2-P1): a settings command does not
    /// decide to reopen the graph; `Config::reach` does, and
    /// `state::apply_config_write` reopens through the one config decider
    /// (`take_in_config_change`, audit R10-07). `refresh_graph` retires the
    /// index worker and restarts the launch check, so a settings command that
    /// calls it by hand restarts indexing for a toggle, and reopens a second
    /// time for a change that reaches the graph. There is no exception:
    /// `set_journal_title_format` claimed one for a journal-file migration no
    /// refresh performs (audit R9-15b).
    #[test]
    fn settings_commands_leave_reopening_to_the_config_reach() {
        let reopening = commands_that_reopen_the_graph();
        let mut settings_that_reopen = BTreeSet::new();
        for (_, source) in crate::test_support::rust_module_sources() {
            for (name, body) in tauri_command_bodies(&source) {
                if body.contains("apply_config_write(") && reopening.contains(&name) {
                    settings_that_reopen.insert(name);
                }
            }
        }
        assert_eq!(
            settings_that_reopen,
            BTreeSet::new(),
            "a settings command calls refresh_graph itself; let apply_config_write \
             take the change in (see set_show_brackets in commands.rs)"
        );
    }

    /// Every registered command must be one the graph-reopening scan could see.
    /// If a rebinding command is ever registered under a name the scan does not
    /// produce, the guard above is checking a set that does not exist.
    #[test]
    fn every_graph_reopening_command_is_actually_registered() {
        let registered = registered_handlers(LIB_RS);
        let reopening = commands_that_reopen_the_graph();
        let unregistered: Vec<&String> = reopening
            .iter()
            .filter(|name| !registered.contains(*name))
            .collect();
        assert!(
            unregistered.is_empty(),
            "commands that call refresh_graph but are not in generate_handler!: {unregistered:?}"
        );
    }

    /// The scan can only see string literals. Pin the call sites where the
    /// command name is computed, so a new one is a deliberate edit rather than
    /// a silent hole in the guard above.
    #[test]
    fn only_the_declared_call_sites_hide_their_command_name() {
        let (_, dynamic) = backend_ts_commands(BACKEND_TS);
        let expected: Vec<String> = DYNAMIC_CALL_SITES.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(
            dynamic, expected,
            "the set of src/backend.ts call sites whose command name is not a literal changed; \
             a name the scan cannot read is a command the parity guard cannot check"
        );
    }
}
