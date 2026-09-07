//! **The one fixture set both visible-column resolvers must agree on** (P5A).
//!
//! `tine.columns::` owns the ordered list of columns a query block SHOWS;
//! `tine.fields::` keeps the typed sheet schema. Three consumers read that
//! precedence — the §4.1 property merge (`query::view`), the static publisher
//! (`publish.rs`), and the app's query table — and two of them are in Rust
//! while the third is in TypeScript. Rust has exactly one implementation
//! (`query::view::resolve_query_columns`, which `publish.rs` calls rather than
//! copying); TypeScript has an adapter at the existing cross-language boundary
//! (`src/editor/queryViewProperties.ts`), because rendering a table is a
//! synchronous walk over blocks already in memory and cannot take an IPC
//! round-trip per block.
//!
//! What makes that pair legitimate rather than a twin is THIS FILE: one fixture
//! set, read by `src/editor/queryViewProperties.test.ts` and by the test below.
//! If the two readers ever disagree about which columns a note selects, one of
//! these two tests goes red — and a published page silently showing different
//! columns from the app is exactly the failure the pinning exists to prevent.

use std::path::PathBuf;

use tine_core::query::view::{resolve_query_columns, QueryColumns};

#[derive(serde::Deserialize)]
struct Case {
    /// Why this case is in the corpus — read it before deleting a row.
    #[allow(dead_code)]
    why: String,
    /// Block properties in document order, keys unnormalized.
    properties: Vec<(String, String)>,
    resolution: Expected,
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Expected {
    Named { columns: Vec<String> },
    Cleared,
    Unset,
}

fn cases() -> Vec<Case> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/query-columns/resolution.json");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_slice(&bytes).expect("query-columns/resolution.json is valid JSON")
}

#[test]
fn the_rust_column_resolver_matches_the_shared_fixtures() {
    for case in cases() {
        let found = resolve_query_columns(&case.properties);
        let actual = match &found {
            QueryColumns::Named(columns) => Expected::Named {
                columns: columns.iter().map(|f| f.as_str().to_string()).collect(),
            },
            QueryColumns::Cleared => Expected::Cleared,
            QueryColumns::Unset => Expected::Unset,
        };
        match (&actual, &case.resolution) {
            (Expected::Named { columns: got }, Expected::Named { columns: want }) => {
                assert_eq!(
                    got, want,
                    "columns for {:?} ({})",
                    case.properties, case.why
                );
            }
            (Expected::Cleared, Expected::Cleared) | (Expected::Unset, Expected::Unset) => {}
            _ => panic!(
                "resolution KIND for {:?} ({}): got {:?}",
                case.properties, case.why, found
            ),
        }
    }
}

/// The corpus is only worth anything if it exercises the branches the packet
/// froze. A future edit that quietly trimmed it to the easy cases would leave
/// the two readers free to drift on exactly the inputs that matter.
#[test]
fn the_corpus_covers_every_frozen_branch() {
    let cases = cases();
    let kind = |c: &Case| match c.resolution {
        Expected::Named { .. } => "named",
        Expected::Cleared => "cleared",
        Expected::Unset => "unset",
    };
    for wanted in ["named", "cleared", "unset"] {
        assert!(
            cases.iter().any(|c| kind(c) == wanted),
            "the corpus lost every `{wanted}` case"
        );
    }
    let value_for = |key: &str, case: &Case| {
        case.properties
            .iter()
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(key))
            .map(|(_, v)| v.clone())
    };
    assert!(
        cases
            .iter()
            .any(|c| value_for("tine.columns", c).is_some_and(|v| v.contains('='))),
        "no case proves that `=` invalidates a whole tine.columns list"
    );
    assert!(
        cases.iter().any(
            |c| value_for("tine.columns", c).is_some() && value_for("tine.fields", c).is_some()
        ),
        "no case proves the precedence between the new key and the legacy one"
    );
    for forbidden in ['\0', '\r', '\n'] {
        assert!(
            cases
                .iter()
                .any(|c| value_for("tine.columns", c).is_some_and(|v| v.contains(forbidden))),
            "no case proves that {forbidden:?} invalidates a tine.columns list"
        );
    }
}
