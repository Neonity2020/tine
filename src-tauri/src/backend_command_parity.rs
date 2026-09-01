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
//! the sources — `managed_command_surface.rs` is the house pattern — so the
//! guard cannot drift from the code it guards.
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
    let source_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&source_dir)
        .expect("src-tauri/src must be readable")
        .map(|entry| entry.expect("readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    entries.sort();
    assert!(
        entries.len() > 10,
        "the src-tauri/src scan found {} sources -- the scanner broke, not the code",
        entries.len()
    );

    let mut names = BTreeSet::new();
    for path in entries {
        let source = std::fs::read_to_string(&path).expect("readable source");
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
