//! The crate-root module layering rule.
//!
//! This is intentionally a source guard: Rust permits every crate module to
//! name every other crate module, while Tine's architecture does not.

#[path = "support/production_source.rs"]
mod production_source;

use production_source::{compiled_source, line_of, module_files, relative_path, repo_root};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};

const LAYERS: &[(&str, &[&str])] = &[
    (
        "L0 leaves",
        &[
            "date",
            "logbook",
            "durability_counters",
            "edn",
            "text_merge",
            "render",
            "html_sanitize",
            "backend_error",
            "directory_identity",
            "filesystem_durability",
            "property_line",
            "search_query",
            "projection_budget",
            "indexing_progress",
        ],
    ),
    (
        "L1 document",
        &[
            "doc",
            "outline",
            "org",
            "refs",
            "reference_evidence",
            "graph_text_path",
            "journal_feed",
            "config",
            "graph_text_scope",
            "vocab",
        ],
    ),
    (
        "L2 engine",
        &[
            "query",
            "query_plan",
            "query_jobs",
            "query_cursor",
            "direct_projection",
        ],
    ),
    (
        "L3 app",
        &[
            "model",
            "publish",
            "onboarding",
            "concord_ledger",
            "concord_queue",
            "direct_move_recovery",
            "pdf",
            "sync_diff",
        ],
    ),
];

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct UpwardReference {
    file: String,
    line: usize,
    from: String,
    to: String,
    item: String,
}

fn layer_by_module() -> BTreeMap<&'static str, usize> {
    LAYERS
        .iter()
        .enumerate()
        .flat_map(|(layer, (_, modules))| modules.iter().map(move |module| (*module, layer)))
        .collect()
}

/// Replace comments and literals with spaces while retaining newlines and byte
/// offsets. Paths in docs, diagnostics and test fixtures are not dependencies.
fn mask_comments_and_literals(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut masked = bytes.to_vec();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let start = cursor;
        let end = if bytes[cursor..].starts_with(b"//") {
            bytes[cursor..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| cursor + offset)
        } else if bytes[cursor..].starts_with(b"/*") {
            let mut depth = 1_usize;
            cursor += 2;
            while cursor < bytes.len() && depth > 0 {
                if bytes[cursor..].starts_with(b"/*") {
                    depth += 1;
                    cursor += 2;
                } else if bytes[cursor..].starts_with(b"*/") {
                    depth -= 1;
                    cursor += 2;
                } else {
                    cursor += 1;
                }
            }
            cursor
        } else if let Some((hashes, prefix_len)) = raw_string_start(&bytes[cursor..]) {
            cursor += prefix_len;
            loop {
                let Some(quote) = bytes[cursor..].iter().position(|byte| *byte == b'"') else {
                    cursor = bytes.len();
                    break;
                };
                cursor += quote + 1;
                if bytes.get(cursor..cursor + hashes) == Some(&vec![b'#'; hashes][..]) {
                    cursor += hashes;
                    break;
                }
            }
            cursor
        } else if bytes[cursor] == b'"'
            || (bytes[cursor] == b'b' && bytes.get(cursor + 1) == Some(&b'"'))
            || (bytes[cursor] == b'c' && bytes.get(cursor + 1) == Some(&b'"'))
        {
            if bytes[cursor] != b'"' {
                cursor += 1;
            }
            cursor += 1;
            while cursor < bytes.len() {
                match bytes[cursor] {
                    b'\\' => cursor = (cursor + 2).min(bytes.len()),
                    b'"' => {
                        cursor += 1;
                        break;
                    }
                    _ => cursor += 1,
                }
            }
            cursor
        } else if bytes[cursor] == b'\'' && char_literal_end(&bytes[cursor..]).is_some() {
            cursor + char_literal_end(&bytes[cursor..]).unwrap()
        } else {
            cursor += 1;
            continue;
        };
        for byte in &mut masked[start..end] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
        cursor = end;
    }
    String::from_utf8(masked).unwrap()
}

fn raw_string_start(bytes: &[u8]) -> Option<(usize, usize)> {
    let prefix = if bytes.starts_with(b"br") || bytes.starts_with(b"cr") {
        2
    } else if bytes.starts_with(b"r") {
        1
    } else {
        return None;
    };
    let hashes = bytes[prefix..]
        .iter()
        .take_while(|byte| **byte == b'#')
        .count();
    (bytes.get(prefix + hashes) == Some(&b'"')).then_some((hashes, prefix + hashes + 1))
}

fn char_literal_end(bytes: &[u8]) -> Option<usize> {
    let mut cursor = 1;
    if bytes.get(cursor) == Some(&b'\\') {
        cursor += 2;
        if bytes.get(1) == Some(&b'\\') && bytes.get(2) == Some(&b'u') {
            while bytes.get(cursor).is_some_and(|byte| *byte != b'}') {
                cursor += 1;
            }
            cursor += 1;
        }
    } else {
        let width = std::str::from_utf8(&bytes[cursor..])
            .ok()?
            .chars()
            .next()?
            .len_utf8();
        cursor += width;
    }
    (bytes.get(cursor) == Some(&b'\'')).then_some(cursor + 1)
}

fn crate_root_modules(lib: &str) -> BTreeSet<String> {
    let declaration =
        Regex::new(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;").unwrap();
    declaration
        .captures_iter(lib)
        .map(|found| found[1].to_string())
        .collect()
}

fn root_aliases(lib: &str, modules: &BTreeSet<String>) -> BTreeMap<String, String> {
    let mut aliases = BTreeMap::new();
    let statement =
        Regex::new(r"(?s)pub\s+use\s+([A-Za-z_][A-Za-z0-9_]*)\s*::\s*([^;]+);").unwrap();
    let identifier = Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap();
    for found in statement.captures_iter(lib) {
        let owner = found[1].to_string();
        if !modules.contains(&owner) {
            continue;
        }
        for item in identifier.find_iter(&found[2]) {
            let name = item.as_str();
            if !matches!(name, "as" | "pub" | "crate" | "self" | "super") {
                aliases.insert(name.to_string(), owner.clone());
            }
        }
    }
    aliases
}

fn resolved_target<'a>(
    first: &str,
    modules: &'a BTreeSet<String>,
    aliases: &'a BTreeMap<String, String>,
) -> Option<&'a str> {
    modules
        .get(first)
        .map(String::as_str)
        .or_else(|| aliases.get(first).map(String::as_str))
}

fn record_path(
    found: &mut BTreeSet<UpwardReference>,
    layers: &BTreeMap<&str, usize>,
    modules: &BTreeSet<String>,
    aliases: &BTreeMap<String, String>,
    relative: &str,
    source: &str,
    offset: usize,
    from: &str,
    first: &str,
    item: String,
) {
    let Some(to) = resolved_target(first, modules, aliases) else {
        return;
    };
    if layers[from] < layers[to] {
        found.insert(UpwardReference {
            file: relative.to_string(),
            line: line_of(source, offset),
            from: from.to_string(),
            to: to.to_string(),
            item,
        });
    }
}

fn paths_in_source(
    found: &mut BTreeSet<UpwardReference>,
    layers: &BTreeMap<&str, usize>,
    modules: &BTreeSet<String>,
    aliases: &BTreeMap<String, String>,
    relative: &str,
    source: &str,
    from: &str,
    root_file: bool,
) {
    let path = Regex::new(r"crate\s*::\s*([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    for captured in path.captures_iter(source) {
        let whole = captured.get(0).unwrap();
        let first = &captured[1];
        let tail = source[whole.end()..]
            .chars()
            .take_while(|character| {
                character.is_alphanumeric() || matches!(character, '_' | ':' | ' ' | '\t')
            })
            .collect::<String>();
        record_path(
            found,
            layers,
            modules,
            aliases,
            relative,
            source,
            whole.start(),
            from,
            first,
            format!("{}{}", whole.as_str(), tail).trim().to_string(),
        );
    }

    let group = Regex::new(r"(?s)crate\s*::\s*\{([^}]*)\}").unwrap();
    let first_identifier = Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap();
    for captured in group.captures_iter(source) {
        let body = captured.get(1).unwrap();
        for entry in body.as_str().split(',') {
            let Some(first) = first_identifier.find(entry) else {
                continue;
            };
            record_path(
                found,
                layers,
                modules,
                aliases,
                relative,
                source,
                body.start() + first.start(),
                from,
                first.as_str(),
                format!("crate::{{{}}}", entry.trim()),
            );
        }
    }

    if root_file {
        let parent_path = Regex::new(r"super\s*::\s*([A-Za-z_][A-Za-z0-9_]*)").unwrap();
        for captured in parent_path.captures_iter(source) {
            let whole = captured.get(0).unwrap();
            record_path(
                found,
                layers,
                modules,
                aliases,
                relative,
                source,
                whole.start(),
                from,
                &captured[1],
                whole.as_str().to_string(),
            );
        }
    }
}

#[test]
fn production_modules_only_refer_to_their_own_or_lower_layers() {
    let root = repo_root();
    let source_root = root.join("crates/tine-core/src");
    let lib_path = source_root.join("lib.rs");
    let lib = mask_comments_and_literals(&compiled_source(&lib_path));
    let modules = crate_root_modules(&lib);
    let layers = layer_by_module();
    let unplaced = modules
        .iter()
        .filter(|module| !layers.contains_key(module.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        unplaced.is_empty(),
        "every production crate-root module declared in lib.rs must be placed in the layer table; missing: {}",
        unplaced.join(", ")
    );

    let aliases = root_aliases(&lib, &modules);
    let mut upward = BTreeSet::new();
    for from in &modules {
        let module_root = format!("crates/tine-core/src/{from}.rs");
        let root_file = root.join(&module_root);
        for file in module_files(&root, &module_root) {
            let source = mask_comments_and_literals(&compiled_source(&file));
            if source.is_empty() {
                continue;
            }
            let relative = relative_path(&root, &file);
            paths_in_source(
                &mut upward,
                &layers,
                &modules,
                &aliases,
                &relative,
                &source,
                from,
                file == root_file,
            );
        }
    }

    let report = upward
        .iter()
        .map(|reference| {
            format!(
                "{}:{} {} → {} {}",
                reference.file, reference.line, reference.from, reference.to, reference.item
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        upward.is_empty(),
        "crate-root production modules may refer only to their own layer or a lower layer; found {} upward reference(s):\n{}\n\nI-11: a guard that still passes must still see what it guards. crates/tine-core/src/journal_feed.rs importing from crate::vocab is the L1 exemplar.",
        upward.len(),
        report
    );
}
