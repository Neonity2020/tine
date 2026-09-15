//! The walk-source census: every production site that builds a
//! `QueryPageSource`, pinned by `(file, enclosing function, count)`.
//!
//! **Why this guard exists.** Martin, 2026-09-07 (the no-production-traversal
//! amendment): the in-memory walk *"needs to be only present as an oracle and
//! needs to be retired as soon as we are confident the sqlite works"*. RET1
//! routed the two PUBLIC IR commands — `query_run` and `query_explain_empty` —
//! through the SQL compiler, the shared result constructor and the captured
//! read jobs on both backends. Nothing in the type system stops a later packet
//! from quietly reconnecting the oracle: `run_query_result_over` is
//! `pub(crate)`, `GraphQueryPages` is one constructor call away, and a
//! reconnection reads as a one-line "fall back to the walk" that no result
//! assertion notices, because the walk answers correctly. It is only visible as
//! a COUNTER (no statement read) or as a SOURCE CENSUS. This is the census.
//!
//! **What is pinned.** A walk needs a source, and there is exactly one source
//! constructor, `GraphQueryPages` (the whole parsed graph). Pinning its
//! construction sites therefore pins every walk, without pinning the dozens of
//! interior `&dyn QueryPageSource` parameters that only pass one along.
//!
//! Pinned by enclosing FUNCTION and never by line number, exactly as
//! `retirement_candidates.rs` argues: a line-anchored pin reddens on every
//! unrelated packet that edits the file above it. Moving a walk into a new
//! function, or adding a second walk to a function that already has one, is a
//! deliberate act and fails here first.
//!
//! **The pinned set is also the retirement worklist.** Each row below is
//! annotated with the packet that removes it. `RECEIPT-ret1.md` carries the
//! same list with exact line numbers as of RET1's base commit.

#[path = "support/production_source.rs"]
mod production_source;

use production_source::{
    compiled_source, erase_cfg_test_regions, model_module_files, module_source,
    production_source_files, relative_path, repo_root,
};
use regex::Regex;
use std::collections::BTreeMap;
use std::path::Path;

/// The walk-source constructor. `GraphQueryPagesInMode` is deliberately
/// not listed: it wraps a `GraphQueryPages(..)` it must construct, so its site
/// is already counted.
const SOURCE_CONSTRUCTORS: &[&str] = &["GraphQueryPages("];

#[test]
fn public_quick_switch_routes_bypass_query_quick_switch() {
    let root = repo_root();
    let model = model_module_files(&root)
        .iter()
        .map(|path| compiled_source(path))
        .collect::<Vec<_>>()
        .join("\n");
    let body = model
        .split("pub fn quick_switch(")
        .nth(1)
        .unwrap()
        .split("\n    }")
        .next()
        .unwrap();
    assert!(body.contains("query_plan::legacy_page_search_entries("));
    assert!(!body.contains("query::quick_switch("));
    assert!(!body.contains(".execute("));
    let commands = module_source(&root, "src-tauri/src/commands.rs");
    let body = commands
        .split("async fn quick_switch(")
        .nth(1)
        .unwrap()
        .split("\n}")
        .next()
        .unwrap();
    assert!(body.contains(".quick_switch("));
    assert!(!body.contains("query::quick_switch("));
    let query = module_source(&root, "crates/tine-core/src/query.rs");
    let body = query
        .split("pub fn quick_switch(")
        .nth(1)
        .unwrap()
        .split("\n}")
        .next()
        .unwrap();
    assert!(body.contains("query_plan::legacy_page_search_entries("));
    assert!(!body.contains(".execute("));
}

/// Every production walk, by `(file, enclosing function)`. The list is empty
/// and should stay empty.
///
/// * **RET2** removed the public query routes' walk recovery.
/// * **RET3** (S3 campaign Q2) removed the export subtree reader.
/// * **K2** (consolidation, 2026-09-15) removed the last three rows,
///   `query.rs::run_query_bounded`, `run_query_result` and
///   `run_advanced_query_bounded`, by moving the walk into `query/walk.rs`,
///   which `query.rs` declares under `#[cfg(test)]`. None had a production
///   caller left: `publish.rs` and the `{{query}}` render path dispatch to SQL.
///   What remains is the oracle the parity gates need.
///
/// A new row means production evaluates the parsed graph again. Say in the
/// packet notes which read and why the database could not answer it.
const PINNED: &[(&str, &str, usize, &str)] = &[];

/// A file a SIBLING (or its parent module file) declares under `#[cfg(test)]`.
///
/// The shared walker recognises the `*_tests.rs` convention; `query/conformance.rs`
/// is the one test module in the tree that does not follow it, and it is
/// declared as `#[cfg(test)] mod conformance;` from `query.rs` — the PARENT
/// module file, not a sibling in `query/`. Counting its walks as production
/// would put six oracle gates on the retirement worklist.
fn declared_under_cfg_test(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let directory = path.parent().expect("a source file has a directory");
    let declaration = Regex::new(&format!(
        r#"(?m)^#\[cfg\(test\)\]\s*\n(?:#\[path\s*=\s*"[^"]*"\]\s*\n)?(?:pub(?:\([^)]*\))?\s+)?mod\s+{};"#,
        regex::escape(stem)
    ))
    .expect("the module-declaration pattern compiles");
    let mut candidates = std::fs::read_dir(directory)
        .expect("the directory reads")
        .map(|entry| entry.expect("the entry reads").path())
        .filter(|candidate| {
            candidate
                .extension()
                .is_some_and(|extension| extension == "rs")
        })
        .collect::<Vec<_>>();
    candidates.push(directory.with_extension("rs"));
    candidates.push(directory.join("mod.rs"));
    candidates.into_iter().any(|candidate| {
        candidate != path
            && candidate.is_file()
            && declaration.is_match(&std::fs::read_to_string(&candidate).expect("the file reads"))
    })
}

/// `(file, enclosing function) -> walk-source constructions`, over the source a
/// shipped binary compiles.
fn walk_sources() -> BTreeMap<(String, String), usize> {
    let root = repo_root();
    let signature =
        Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+\x22[^\x22]*\x22\s+)?fn\s+(\w+)")
            .expect("the fn-signature pattern compiles");
    let mut found: BTreeMap<(String, String), usize> = BTreeMap::new();
    for path in production_source_files() {
        if declared_under_cfg_test(&path) {
            continue;
        }
        let relative = relative_path(&root, &path);
        let source = compiled_source(&path);
        let mut enclosing = "<module>".to_string();
        for line in source.lines() {
            if let Some(captured) = signature.captures(line) {
                enclosing = captured[1].to_string();
            }
            let constructions = SOURCE_CONSTRUCTORS
                .iter()
                .filter(|token| line.contains(**token))
                .count();
            if constructions > 0 {
                *found
                    .entry((relative.clone(), enclosing.clone()))
                    .or_default() += constructions;
            }
        }
    }
    found
}

#[test]
fn production_walk_sources_are_pinned() {
    let expected = PINNED
        .iter()
        .map(|(file, function, count, _)| (((*file).to_owned(), (*function).to_owned()), *count))
        .collect::<BTreeMap<_, _>>();
    let found = walk_sources();
    let mut differences = Vec::new();
    for (key, count) in &found {
        match expected.get(key) {
            Some(pinned) if pinned == count => {}
            Some(pinned) => differences.push(format!(
                "{}::{} builds {count} walk sources; {pinned} pinned",
                key.0, key.1
            )),
            None => differences.push(format!(
                "{}::{} builds {count} walk source(s) and is NOT pinned",
                key.0, key.1
            )),
        }
    }
    for key in expected.keys() {
        if !found.contains_key(key) {
            differences.push(format!(
                "{}::{} is pinned but builds no walk source any more",
                key.0, key.1
            ));
        }
    }
    assert!(
        differences.is_empty(),
        "the production walk-source census changed:\n{}\n\n\
         A walk source is a `GraphQueryPages`, and \
         building one is how production evaluates the parsed graph instead of \
         the projection. Martin, 2026-09-07: traversal \"needs to be only \
         present as an oracle and needs to be retired as soon as we are \
         confident the sqlite works\". If you ADDED a row, say in the packet \
         notes which production read now walks and why the database could not \
         answer it — do not add one to make a test pass. If you REMOVED a row, \
         delete it here too; that is a retirement, and it is the point.",
        differences.join("\n")
    );
}

#[test]
fn the_public_query_commands_never_build_a_walk_source() {
    let root = repo_root();
    let mut offenders = Vec::new();
    for path in production_source_files() {
        if !path.starts_with(root.join("src-tauri/src")) {
            continue;
        }
        let relative = relative_path(&root, &path);
        for (number, line) in compiled_source(&path).lines().enumerate() {
            if SOURCE_CONSTRUCTORS
                .iter()
                .any(|token| line.contains(*token))
            {
                offenders.push(format!("{relative}:{}", number + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the Tauri command layer builds a walk source at:\n{}\n\n\
         SPEC §7.1's `query_run` and `query_explain_empty` reach the engine \
         through `run_query_result_ir` / `explain_empty_query`, a database \
         route. A command that builds its \
         own page source has reconnected the oracle at the wire, where no \
         result assertion can see it.",
        offenders.join("\n")
    );
}

/// The erasure the census depends on, checked directly: a walk inside a
/// `#[cfg(test)]` region is not a production walk, and a census that counted
/// one would put every parity gate on the retirement worklist.
#[test]
fn a_walk_inside_a_cfg_test_region_is_not_counted() {
    let erased = erase_cfg_test_regions(
        "fn production() { let _ = GraphQueryPages(graph); }\n\
         #[cfg(test)]\n\
         mod gates { fn oracle() { let _ = GraphQueryPages(graph); } }\n"
            .to_string(),
    );
    assert_eq!(
        erased.matches("GraphQueryPages(").count(),
        1,
        "the shared walker must blank `#[cfg(test)]` regions in place"
    );
}

#[test]
fn the_cursor_owner_remains_shared() {
    let root = repo_root();
    let direct = compiled_source(&root.join("crates/tine-core/src/direct_projection.rs"));
    assert!(direct.contains("query_cursor::drain_after"));
}

#[test]
fn an_oracle_module_is_test_only_without_a_test_filename_suffix() {
    let directory =
        std::env::temp_dir().join(format!("tine-oracle-module-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let oracle = directory.join("oracle.rs");
    let parent = directory.join("mod.rs");
    std::fs::write(&oracle, "pub fn selected() {}\n").unwrap();
    std::fs::write(&parent, "#[cfg(test)]\npub(crate) mod oracle;\n").unwrap();
    assert!(compiled_source(&oracle).is_empty());
    std::fs::write(&parent, "pub(crate) mod oracle;\n").unwrap();
    assert!(compiled_source(&oracle).contains("fn selected()"));
    std::fs::remove_dir_all(directory).unwrap();
}

/// The reference panels reach the engine through the INDEXED policy.
///
/// `Graph::backlinks_bounded` and `Graph::unlinked_refs_bounded` answer a
/// projection that is only mid-turn by parsing every page in the graph. That is
/// the right answer for print, publish and diagnostics, which have no readiness
/// retry to wait on. It is the wrong answer for a panel, which does: during the
/// cold-open window it parses the whole graph, once per panel, to produce rows
/// the index serves a moment later.
///
/// The two policies differ by a suffix, so nothing but this guard stops the next
/// panel from picking whichever name autocompletes first.
#[test]
fn the_reference_panel_commands_use_the_indexed_policy() {
    let root = repo_root();
    let mut offenders = Vec::new();
    for path in production_source_files() {
        if !path.starts_with(root.join("src-tauri/src")) {
            continue;
        }
        let relative = relative_path(&root, &path);
        for (number, line) in compiled_source(&path).lines().enumerate() {
            for walking in ["backlinks_bounded(", "unlinked_refs_bounded("] {
                if line.contains(walking) {
                    offenders.push(format!("{relative}:{}: {}", number + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a Tauri command uses the walking reference policy at:\n{}\n\n\
         Use `backlinks_bounded_indexed` / `unlinked_refs_bounded_indexed` \
         instead. They return `Result<_, QueryExecutionError>`; `?` at the \
         command boundary turns a `NotReady` into the same tagged wire error a \
         query block already retries through `runQueryWhenCurrent`. \
         `src-tauri/src/commands.rs`'s `get_backlinks` is the exemplar.",
        offenders.join("\n")
    );
}
