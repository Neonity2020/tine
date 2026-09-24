#[path = "support/production_source.rs"]
mod production_source;

use production_source::{
    collect_rs_files, compiled_source, line_of, production_source_files, relative_path, repo_root,
    source_without_test_regions,
};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};

const CURSOR: &str = "crates/tine-core/src/query_cursor.rs";
const DIRECT: &str = "crates/tine-core/src/direct_projection.rs";

fn sources() -> BTreeMap<String, String> {
    let root = repo_root();
    production_source_files()
        .into_iter()
        .map(|path| (relative_path(&root, &path), compiled_source(&path)))
        .collect()
}

fn function_body<'a>(source: &'a str, symbol: &str) -> &'a str {
    let needle = format!("fn {symbol}(");
    let start = source
        .find(&needle)
        .unwrap_or_else(|| panic!("missing production symbol {symbol}"));
    let brace = source[start..].find('{').unwrap() + start;
    let mut depth = 1_usize;
    let mut end = brace + 1;
    for byte in source.as_bytes()[brace + 1..].iter() {
        match byte {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        end += 1;
        if depth == 0 {
            return &source[start..end];
        }
    }
    panic!("unterminated production symbol {symbol}")
}

#[test]
fn hand_written_cursor_drains_are_pinned() {
    let source = sources();
    assert_eq!(
        source[CURSOR].matches("loop {").count(),
        1,
        "I-12: the shared production cursor owner contains the drain loop"
    );

    let direct = &source[DIRECT];
    for symbol in [
        "property_facets",
        "referenced_page_names",
        "page_aliases_with_owners",
        "real_page_names",
        "reference_candidates",
        "block_ref_counts",
        "block_referrer_candidate_paths",
    ] {
        let body = function_body(direct, symbol);
        assert!(
            !body.contains("loop {"),
            "I-12: {DIRECT}::{symbol} retains caller-owned cursor advancement, termination, or adaptive retry; call drain_after"
        );
    }
    // 10 → 12: P0-rust Wave D's `property_owner_rows` (§6.2's Direct Files
    // registry row source) drains the page map and the property rows. Both
    // DELEGATE to `drain_after` — which is what this guard is for — so the pin
    // moves; it would be a violation only if the new consumer owned its own
    // `loop {}`, which the per-symbol assertions above still forbid.
    // 12 → 13: R6's `page_inventory` (the warm-session `list_pages` source)
    // drains the page map through `drain_after` like the twelve before it.
    // 13 → 12: RET2 deleted `sparse_task_query` — the Direct sparse-task
    // candidate route — along with the query walk it handed its candidates to.
    // Its `task_candidate_locators_after` drain went with it, and so did its
    // row in the per-symbol list above. No surviving consumer changed.
    // 12 → 11: Q1 deleted `fuzzy_candidate_paths` — the Direct Friendly
    // candidate route — along with parsed-page ranking. Its
    // `fuzzy_subsequence_candidate_pages_after` drain went with it, and so did
    // its row above. No surviving consumer changed.
    // 11 → 9: K2 made `property_owner_rows` test-only. It fed the editor
    // registry, whose only reader was the query walk; the walk is the test-only
    // oracle now and the product reads the projection's committed registry.
    // 9 → 7: P3b deleted the fuzzy-candidate cursor and moved authored alias
    // rows through the shared projection statement door. The seven surviving
    // storage cursor consumers still delegate to `drain_after`.
    // 7 → 6 (GH #594 R3, 2026-09-24): `reference_candidates` needs each
    // block's structural identity, which `page_referrer_candidates_after`
    // does not carry, so it reads through the shared projection statement
    // door like P3b's alias rows. The six surviving consumers still delegate.
    assert_eq!(
        direct.matches("drain_after(").count(),
        6,
        "I-12: the six owned Direct cursor consumers must each delegate to drain_after"
    );
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CensusRecord {
    family: String,
    file: String,
    enclosing_symbol: String,
    call_expression: String,
    class: String,
    question: String,
}

fn containing_symbol(source: &str, offset: usize) -> String {
    let before = &source[..offset];
    Regex::new(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?fn\s+([A-Za-z0-9_]+)")
        .unwrap()
        .captures_iter(before)
        .last()
        .map(|capture| capture[1].to_string())
        .expect("read-family call must be inside a named function")
}

fn classify(file: &str, symbol: &str, family: &str) -> (&'static str, &'static str) {
    match (file, symbol, family) {
        (DIRECT, "property_facets", "property_facet_rows_after") => {
            ("other-question", "Direct property facets")
        }
        (DIRECT, "real_page_names", "navigation_pages_after_with_header_validation") => {
            ("other-question", "Direct real page ownership")
        }
        // R6: `list_pages` in a warm session (no parsed cache) is served from
        // the ready projection's page inventory instead of a whole-graph parse.
        (DIRECT, "page_inventory", "navigation_pages_after_with_header_validation") => {
            ("other-question", "Direct page inventory for list_pages")
        }
        _ => panic!("unclassified SQL read-family call: {file}::{symbol} {family}"),
    }
}

fn census(source: &BTreeMap<String, String>) -> BTreeSet<CensusRecord> {
    let families = [
        "navigation_pages_after_with_header_validation",
        "task_candidate_pages_after",
        "page_referrer_candidates_after",
        "block_property_candidates_after",
        "property_facet_rows_after",
        "navigation_pages_after",
    ];
    let mut records = BTreeSet::new();
    for (file, text) in source {
        for family in families {
            let pattern = Regex::new(&format!(r"\.\s*{}\s*\(", regex::escape(family))).unwrap();
            for found in pattern.find_iter(text) {
                let symbol = containing_symbol(text, found.start());
                let (class, question) = classify(file, &symbol, family);
                assert!(
                    records.insert(CensusRecord {
                        family: family.into(),
                        file: file.clone(),
                        enclosing_symbol: symbol,
                        call_expression: format!(".{family}("),
                        class: class.into(),
                        question: question.into(),
                    }),
                    "duplicate census record needs a more exact enclosing symbol"
                );
            }
        }
    }
    records
}

fn assert_exact_census(source: &BTreeMap<String, String>, expected: &BTreeSet<CensusRecord>) {
    assert_eq!(census(source), *expected, "I-12: the shared SQL read-family producer/consumer census changed; classify the exact enclosing production symbol and question; exemplar {DIRECT}");
}

fn expected_census() -> BTreeSet<CensusRecord> {
    let mut records = BTreeSet::new();
    let mut add = |family: &str, file: &str, symbol: &str, class: &str, question: &str| {
        assert!(records.insert(CensusRecord {
            family: family.into(),
            file: file.into(),
            enclosing_symbol: symbol.into(),
            call_expression: format!(".{family}("),
            class: class.into(),
            question: question.into(),
        }));
    };
    for (family, file, symbol, question) in [
        (
            "property_facet_rows_after",
            DIRECT,
            "property_facets",
            "Direct property facets",
        ),
        (
            "navigation_pages_after_with_header_validation",
            DIRECT,
            "real_page_names",
            "Direct real page ownership",
        ),
        (
            "navigation_pages_after_with_header_validation",
            DIRECT,
            "page_inventory",
            "Direct page inventory for list_pages",
        ),
    ] {
        add(family, file, symbol, "other-question", question);
    }
    records
}

#[test]
fn simple_query_read_family_census_is_exact() {
    let source = sources();
    let expected = expected_census();
    assert_exact_census(&source, &expected);

    // `page_referrer_candidates_after` has no product consumer since GH #594
    // R3; a call to it anywhere is unclassified and fails the census.
    let representative = [
        ("property_facet_rows_after", "Source::PageProperty"),
        (
            "navigation_pages_after_with_header_validation",
            "Source::Journal",
        ),
    ];
    for (family, source_variant) in representative {
        let mut sixth_file = source.clone();
        sixth_file.insert(
            format!("crates/tine-core/src/rogue_{family}.rs"),
            format!("fn rogue(read: &Read) {{ read.{family}(None, 1); /* {source_variant} */ }}"),
        );
        assert!(std::panic::catch_unwind(|| assert_exact_census(&sixth_file, &expected)).is_err());

        let mut wrong_symbol = source.clone();
        wrong_symbol
            .get_mut("crates/tine-core/src/model.rs")
            .unwrap()
            .push_str(&format!(
                "\nfn rogue_{family}(read: &Read) {{ read.{family}(None, 1); }}\n"
            ));
        assert!(
            std::panic::catch_unwind(|| assert_exact_census(&wrong_symbol, &expected)).is_err()
        );

        let mut swapped = source.clone();
        let owner = expected
            .iter()
            .find(|record| record.family == family)
            .unwrap();
        let text = swapped.get_mut(&owner.file).unwrap();
        let needle = format!(".{family}(");
        let at = text.find(&needle).unwrap();
        text.replace_range(at..at + needle.len(), ".removed_allowed_call(");
        swapped
            .get_mut("crates/tine-core/src/model.rs")
            .unwrap()
            .push_str(&format!(
                "\nfn swapped_{family}(read: &Read) {{ read.{family}(None, 1); }}\n"
            ));
        assert!(std::panic::catch_unwind(|| assert_exact_census(&swapped, &expected)).is_err());
    }
}

/// The query engines as a test build compiles them: the SQL lowering and the
/// TQL front end ship, and the walk (`eval.rs`) is their test-only oracle, so it
/// is held to the same rule. `*_tests.rs` files are not decisions.
fn query_engine_sources() -> BTreeMap<String, String> {
    let root = repo_root();
    let mut files = vec![root.join("crates/tine-core/src/query.rs")];
    collect_rs_files(&root, &root.join("crates/tine-core/src/query"), &mut files);
    files
        .into_iter()
        .filter(|path| !path.to_string_lossy().ends_with("_tests.rs"))
        .map(|path| {
            (
                relative_path(&root, &path),
                source_without_test_regions(&path),
            )
        })
        .collect()
}

/// Every place an operator decision in `sources` answers the operators it
/// does not name with one rest arm: a `match (op, ..)`, or a `match op` whose
/// top-level arms include `_ =>` or a lowercase binding.
fn operator_rest_arms(sources: &BTreeMap<String, String>) -> Vec<String> {
    let tuple = Regex::new(r"match\s*\(\s*\*?op\s*,").unwrap();
    let single = Regex::new(r"match\s+\*?op\s*\{").unwrap();
    let rest_arm = Regex::new(r"^\s*(?:_|[a-z][a-z0-9_]*)\s*(?:if\b[^\n]*)?=>").unwrap();
    let mut found = Vec::new();
    for (file, text) in sources {
        for decision in tuple.find_iter(text) {
            let line = line_of(text, decision.start());
            found.push(format!("{file}:{line}: `match (op, ..)`"));
        }
        for decision in single.find_iter(text) {
            let bytes = text.as_bytes();
            let mut depth = 0_usize;
            let mut cursor = decision.end() - 1;
            while cursor < bytes.len() {
                match bytes[cursor] {
                    b'"' => {
                        cursor += 1;
                        while cursor < bytes.len() && bytes[cursor] != b'"' {
                            cursor += if bytes[cursor] == b'\\' { 2 } else { 1 };
                        }
                    }
                    b'{' | b'(' | b'[' => depth += 1,
                    b'}' | b')' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                // An arm starts after the opening brace, a newline or a comma
                // at the match's own depth.
                if depth == 1 && matches!(bytes[cursor], b'{' | b'\n' | b',') {
                    let rest = &text[cursor + 1..];
                    let arm = &rest[..rest.find('\n').unwrap_or(rest.len())];
                    if rest_arm.is_match(arm) {
                        let line = line_of(text, cursor + 1);
                        found.push(format!("{file}:{line}: `{}`", arm.trim()));
                    }
                }
                cursor += 1;
            }
        }
    }
    found
}

#[test]
fn every_operator_decision_in_the_query_engines_names_every_operator() {
    let sources = query_engine_sources();
    assert!(
        sources.contains_key("crates/tine-core/src/query/sql.rs")
            && sources.contains_key("crates/tine-core/src/query/eval.rs"),
        "the query engine scan lost its two engines"
    );
    let found = operator_rest_arms(&sources);
    assert!(
        found.is_empty(),
        "I-11: an operator decision in the query engines lists every CmpOp it does \
         not answer instead of one rest arm, so a new operator is a compile error at \
         each decision rather than a silent `false` in one engine only (K4 found five \
         such wrong answers). Match on the operator first, then read the value's shape \
         with `Value::as_text` / `as_list` / `as_bool`; a one-operator test is \
         `if op == CmpOp::In`. Imitate `op_applies` in crates/tine-core/src/query/tql.rs.\n{}",
        found.join("\n")
    );

    for rogue in [
        "fn rogue(op: CmpOp) -> bool {\n    match op {\n        CmpOp::Eq => true,\n        _ => false,\n    }\n}\n",
        "fn rogue(op: CmpOp) -> bool {\n    match op {\n        CmpOp::Eq => true,\n        other => other == CmpOp::NotEq,\n    }\n}\n",
        "fn rogue(op: CmpOp) -> bool { match op { CmpOp::Eq => \"{\".is_empty(), _ => false } }\n",
        "fn rogue(op: &CmpOp, value: &Value) -> bool {\n    match (*op, value) {\n        (CmpOp::Eq, Value::Bool { value }) => *value,\n    }\n}\n",
    ] {
        let mut planted = sources.clone();
        planted
            .get_mut("crates/tine-core/src/query/sql.rs")
            .unwrap()
            .push_str(rogue);
        assert_eq!(
            operator_rest_arms(&planted).len(),
            found.len() + 1,
            "the guard missed a planted rest arm:\n{rogue}"
        );
    }
}
