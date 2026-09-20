//! Tine-core's compatibility boundary for the shared search fold and matcher.
//!
//! The dependency-light implementation lives in `tine-search` so native query
//! execution and the browser Wasm bridge cannot drift. Core-specific source
//! guards remain here because they describe native planner and identity rules.

pub(crate) use tine_search::canonical_fold_with_map;
pub use tine_search::{canonical_fold, AndGroup, Matcher, Term, SEARCH_SYNTAX_EXAMPLES};

#[cfg(test)]
mod tests {
    #[test]
    fn native_search_fold_has_one_explicit_owner() {
        let leaf = include_str!("../../tine-search/src/lib.rs");
        let owner = include_str!("../../tine-search/src/fold.rs");
        let core = include_str!("search_query.rs");
        let bridge = include_str!("../../lsdoc-wasm/src/lib.rs");
        let planner = include_str!("query_plan.rs");
        let sql = include_str!("query/sql.rs");
        let eval = include_str!("query/eval.rs");
        let refs = include_str!("refs.rs");
        let projection = include_str!("direct_projection.rs");
        let document = include_str!("doc.rs");
        let quick_switcher = include_str!("../../../src/components/QuickSwitcher.tsx");

        assert!(leaf.contains("mod fold;"));
        assert!(owner.contains("decompose_compatible"));
        assert!(core.contains("pub use tine_search::{canonical_fold"));
        assert!(!core
            .split_once("#[cfg(test)]")
            .unwrap()
            .0
            .contains("decompose_compatible"));
        assert!(bridge.contains("tine_search::canonical_fold"));
        assert!(bridge.contains("tine_search::Matcher::parse(query)"));
        assert!(!planner.contains("unicode_normalization"));
        assert!(!planner.contains("unicode_segmentation"));
        let folded_needle_boundary = planner
            .split_once("fn folded_needle_chars")
            .unwrap()
            .1
            .split_once("fn merge_spans")
            .unwrap()
            .0;
        assert!(!folded_needle_boundary.contains("canonical_fold"));
        let sql_term_boundary = sql
            .split_once("fn match_term")
            .unwrap()
            .1
            .split_once("fn fts_bound")
            .unwrap()
            .0;
        assert!(sql_term_boundary.contains("term.text.clone()"));
        assert!(!sql_term_boundary.contains("canonical_fold(term"));

        let page_candidates = planner
            .split_once("fn execute_page_candidates")
            .unwrap()
            .1
            .split_once("fn walk_blocks")
            .unwrap()
            .0;
        assert!(page_candidates.contains("crate::refs::page_key(&page.name)"));
        assert!(page_candidates.contains("crate::refs::page_key(alias)"));
        assert!(page_candidates.contains("crate::refs::page_key(&name)"));
        assert!(!page_candidates.contains("canonical_fold(&page.name)"));
        assert!(!page_candidates.contains("canonical_fold(alias)"));
        assert!(refs.contains("fn page_identity_pattern(pattern: &str)"));
        assert!(eval.contains("refs::page_identity_pattern(text)"));
        assert!(sql.contains("refs::page_identity_pattern(text)"));
        assert!(quick_switcher.contains("pageIdentityKey(page.name) === queryIdentity"));
        assert!(quick_switcher.contains("pageIdentityKey(page.matchedAlias) === queryIdentity"));
        assert!(!quick_switcher.contains("p.adaptiveClass === \"exact\""));

        let search_rows = projection
            .lines()
            .filter(|line| line.contains("normalized_searchable_text:"))
            .collect::<Vec<_>>();
        assert_eq!(search_rows.len(), 2);
        assert!(search_rows
            .iter()
            .all(|line| line.contains("crate::search_query::canonical_fold(&searchable_text)")));
        assert!(
            document.contains("let visible_lower = crate::search_query::canonical_fold(&visible);")
        );
        // Page identity (`graph_text_path`/`refs`) and property/sort programs
        // deliberately live outside this guard: they own different contracts.
    }
}
