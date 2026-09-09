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
//! **What is pinned.** A walk needs a source, and there are exactly two source
//! constructors — `GraphQueryPages` (Direct Files' whole parsed graph) and
//! `ApplicationQueryPages` (Managed storage's candidate set). Pinning their
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
    compiled_source, erase_cfg_test_regions, production_source_files, relative_path, repo_root,
};
use regex::Regex;
use std::collections::BTreeMap;
use std::path::Path;

/// The two walk-source constructors. `GraphQueryPagesInMode` is deliberately
/// not listed: it wraps a `GraphQueryPages(..)` it must construct, so its site
/// is already counted.
const SOURCE_CONSTRUCTORS: &[&str] = &["GraphQueryPages(", "ApplicationQueryPages {"];

#[test]
fn public_quick_switch_routes_bypass_query_quick_switch() {
    let root = repo_root();
    let model = compiled_source(&root.join("crates/tine-core/src/model.rs"));
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
    let commands = compiled_source(&root.join("src-tauri/src/commands.rs"));
    let body = commands
        .split("async fn quick_switch(")
        .nth(1)
        .unwrap()
        .split("\n}")
        .next()
        .unwrap();
    assert!(body.contains(".quick_switch("));
    assert!(!body.contains("query::quick_switch("));
    let managed = compiled_source(&root.join("crates/tine-core/src/sync_runtime.rs"));
    // The QuickSwitch request pattern is matched TWICE in this file: once by the
    // request-size validator, which only reads `query` and `limit`, and once by
    // the evaluation arm that actually answers it. Taking the first occurrence
    // reads the validator and proves nothing about routing, so select the arm by
    // the reply it builds and require exactly one such arm.
    let arms = managed
        .split("SyncApplicationNavigationRequest::QuickSwitch {")
        .skip(1)
        .map(|arm| {
            arm.split("SyncApplicationNavigationRequest::ResolveBlocks")
                .next()
                .unwrap()
        })
        .filter(|arm| arm.contains("SyncApplicationNavigationReply::QuickSwitch"))
        .collect::<Vec<_>>();
    assert_eq!(
        arms.len(),
        1,
        "I-12: exactly one managed arm may answer QuickSwitch"
    );
    let body = arms[0];
    assert!(body.contains("query_plan::legacy_page_search_entries("));
    assert!(!body.contains("query::quick_switch("));
    assert!(!body.contains(".execute("));
    let query = compiled_source(&root.join("crates/tine-core/src/query.rs"));
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

/// Every production walk, by `(file, enclosing function)`, and the packet that
/// deletes it.
///
/// * **RET2** — the public query routes' readiness / read-error / cancellation
///   recovery. RET1 kept these as an internal repair checkpoint; RET2 removed
///   them and wired one bounded repair plus a typed
///   `query::QueryExecutionError` instead, which is what makes the public
///   commands database-only. **Both halves have landed and the list is empty:**
///   RET2-Managed took the two `sync_runtime.rs` rows, RET2-Direct took the
///   three `model.rs` rows.
/// * **RET3** — the friendly (`{{query}}` / backlinks / derived) ranking route
///   and the export subtree reader, migrated after the public commands.
/// * **oracle** — a walk that exists to be COMPARED against, or a §8.1
///   counterfactual mode. These stay: the amendment retires production
///   traversal, not the oracle the parity gates need.
const PINNED: &[(&str, &str, usize, &str)] = &[
    // ---- RET2: no rows. The public IR route has no walk behind it. ----
    // (`model.rs::direct_ir_explain_empty`, `model.rs::direct_page_rows` and
    // `model.rs::direct_simple_query_pre_view` were here. RET2-Direct replaced
    // `Graph::dispatched_or_walk` with `Graph::dispatch_direct_query`: one
    // bounded repair through the existing streamed recovery, then a retry of
    // the SAME statement, then a typed `query::QueryExecutionError`. Cancelled
    // never repairs; a missing projection is `ProjectionUnavailable`; a stopped
    // worker is bounded rather than an endless retry. `model.rs` builds no
    // walk source at all any more, which is why it names no function below.)
    //
    // (`sync_runtime.rs::ir_walk_ready` and
    // `sync_runtime.rs::application_simple_query_pages_ready` were here too.
    // RET2-Managed deleted `application_ir_query_walk_ready` and replaced
    // `application_ir_query_turn`'s no-stamp / non-local branches with typed
    // `query::QueryExecutionError`s; both functions are now compiled only under
    // test, as the parity oracles.)
    //
    // Direct Files' `{{query}}` friendly ranking route still walks, but it does
    // so through `query.rs::run_query_bounded` below, which is RET3's.
    // ---- RET3: the friendly / advanced / export routes ----
    (
        "crates/tine-core/src/query.rs",
        "run_query_bounded",
        1,
        "RET3: `{{query}}` ranking over Direct Files. Remaining production \
         consumer after RET2-Direct: `publish.rs`'s static OG-macro export \
         (`Graph::run_query_bounded` no longer reaches it — it dispatches).",
    ),
    (
        "crates/tine-core/src/query.rs",
        "run_pred_bounded",
        1,
        "RET3: the pre-view block constructor's Direct Files entry, reached \
         through `run_query_result`",
    ),
    (
        "crates/tine-core/src/query.rs",
        "run_query_result",
        1,
        "RET3: the TEXT result entry (`run_query_result`), still used by \
         `{{query}}` rendering and by the gates' oracle",
    ),
    (
        "crates/tine-core/src/query.rs",
        "run_advanced_query_bounded",
        1,
        "RET3: advanced datalog over Direct Files. Remaining production \
         consumer after RET2-Direct: `publish.rs`'s static export \
         (`Graph::run_advanced_query_bounded_cached` dispatches instead).",
    ),
    (
        "crates/tine-core/src/query.rs",
        "run_application_query_pages_bounded",
        1,
        "RET3: `{{query}}` ranking over Managed storage",
    ),
    // (`query.rs::run_application_advanced_query_pages_bounded` was here, on
    // RET3's list. RET2-Managed-Advanced reached it early: the Managed
    // advanced datalog query is a PUBLIC query command on the same captured
    // route as the two IR commands and SimpleQuery, so its walk was retired
    // with theirs. `sync_runtime.rs::application_advanced_query_ready` — the
    // actor turn that loaded EVERY page of the graph to answer it — is gone,
    // the wire request is intercepted by
    // `SyncRuntimeHandle::application_captured_query`, and the wrapper is now
    // compiled only under test, as the parity oracle
    // `RuntimeActor::application_complete_page_advanced_query` reaches. Direct
    // Files' `run_advanced_query_bounded` is untouched and is still RET3's.)
    (
        "crates/tine-core/src/query.rs",
        "export_query_subtrees",
        1,
        "RET3: the export reader over Direct Files, reached by the \
         `export_query_subtrees` command's Direct arm",
    ),
    (
        "crates/tine-core/src/query.rs",
        "export_application_query_subtrees",
        1,
        "RET3: the export reader over Managed storage",
    ),
    // Counterfactual corpus modes now compile only in ignored unit tests.
    // scripts/query-oracle-dump.py preserves their reproducible TSV command.
];

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
         A walk source is `GraphQueryPages` or `ApplicationQueryPages`, and \
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
         through `run_query_result_ir` / `explain_empty_query` (Direct Files) \
         and `SyncApplicationNavigationRequest::QueryRun` / `QueryExplainEmpty` \
         (Managed storage). Both are database routes. A command that builds its \
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
fn candidate_planning_is_only_an_oracle_and_the_cursor_owner_remains_shared() {
    let root = repo_root();
    let declarations = Regex::new(
        r"(?:enum|trait|fn)\s+(?:SimpleQueryCandidateSource|SimpleQueryCandidatePlan|SimpleQuerySqlRead|simple_query_candidate_plan|lower_simple_query_candidate_plan)\b",
    ).unwrap();
    let offenders = production_source_files()
        .into_iter()
        .filter_map(|path| {
            declarations
                .is_match(&compiled_source(&path))
                .then(|| relative_path(&root, &path))
        })
        .collect::<Vec<_>>();
    assert!(
        offenders.is_empty(),
        "production still includes the candidate planner: {offenders:?}"
    );
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
