use super::*;
use std::cell::Cell;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn text_pred(mode: TextMatchMode, value: &str) -> TextPredicate {
    TextPredicate {
        clause_id: 1,
        field: TextField::PageName,
        mode,
        value: value.into(),
    }
}

fn fixture() -> (PathBuf, Graph) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("tine-query-plan-{}-{nonce}", std::process::id()));
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("pages").join("Opinion Diffusion.md"),
        "- Parent\n\t- 🧠 foo ready\n- foo draft\n- ready only\n- regex ABC\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Opdf Notes.md"),
        "alias:: Research Hub\n\n- unrelated\n",
    )
    .unwrap();
    fs::write(dir.join("pages").join("Xopdf.md"), "- unrelated\n").unwrap();
    fs::write(
        dir.join("pages").join("References.md"),
        "- [[Virtual Opdf]]\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Étude.org"),
        "#+TITLE: Étude\n* Žluťoučký unicode fallback\n",
    )
    .unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();
    (dir, graph)
}

#[test]
fn pre_ready_fallback_is_limited_to_ctrl_k_and_block_picker_routes() {
    let (dir, graph) = fixture();

    let picker = graph.search("foo ready", 10).unwrap();
    assert!(picker
        .iter()
        .flat_map(|group| &group.blocks)
        .any(|block| { block.raw.contains("foo ready") }));
    assert!(graph
        .search("Žluťoučký", 10)
        .unwrap()
        .iter()
        .flat_map(|group| &group.blocks)
        .any(|block| block.raw.contains("Žluťoučký")));

    let ctrl_k = graph
        .run_graph_search_displayed_for(
            "Research Hub",
            10,
            10,
            None,
            false,
            FriendlyDisplayOptions::default(),
            FriendlyConsumer::CtrlK,
        )
        .unwrap();
    assert!(ctrl_k.hits.iter().any(|hit| matches!(
        hit,
        QueryHit::Page { matched_alias: Some(alias), .. } if alias == "Research Hub"
    )));

    assert!(matches!(
        graph.run_graph_search("foo ready", 10, 10, false),
        Err(crate::query::QueryExecutionError::Unavailable(
            crate::query::QueryUnavailableReason::ProjectionUnavailable
        ))
    ));
    crate::test_support::remove_dir_all(dir);
}

#[test]
fn pre_ready_ctrl_k_keeps_current_page_scope_and_cancellation_atomic() {
    let (dir, graph) = fixture();
    let scoped = graph
        .run_graph_search_displayed_for(
            "ready",
            10,
            10,
            Some(QueryPageScope {
                name: "Opinion Diffusion".into(),
                page_kind: PageKind::Page,
                path: Some("pages/Opinion Diffusion.md".into()),
            }),
            false,
            FriendlyDisplayOptions::default(),
            FriendlyConsumer::CtrlK,
        )
        .unwrap();
    assert!(scoped.hits.iter().any(|hit| matches!(
        hit,
        QueryHit::Block { block, .. } if block.raw.contains("ready")
    )));
    assert!(scoped.hits.iter().all(|hit| match hit {
        QueryHit::Page { .. } => true,
        QueryHit::Block { path, .. } => path == "pages/Opinion Diffusion.md",
    }));

    let plan = QueryPlan::block_search_literal("ready", 10);
    let cancelled = graph.with_pages(|pages| {
        pre_ready_interactive_snapshot(
            &plan,
            pages,
            || pre_ready_page_inventory(pages, &|| false).map(std::sync::Arc::new),
            false,
            &|| true,
        )
    });
    assert!(cancelled.cancelled);
    assert!(cancelled.hits.is_empty());
    crate::test_support::remove_dir_all(dir);
}

#[test]
fn pre_ready_ctrl_k_ranks_all_page_names_and_aliases_before_limiting() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "tine-query-plan-pre-ready-pages-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    for index in 0..1001 {
        fs::write(
            dir.join("pages").join(format!("a-{index:04}.md")),
            format!(
                "title:: Target filler {index:04}\n\
                 alias:: Needle Alias filler {index:04}\n\n\
                 - unrelated\n"
            ),
        )
        .unwrap();
    }
    fs::write(
        dir.join("pages/z-exact.md"),
        "title:: Target\nalias:: Needle Alias\n\n- unrelated\n",
    )
    .unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();

    for (query, expected_alias) in [("Target", None), ("Needle Alias", Some("Needle Alias"))] {
        let plan = friendly_search_plan_for(
            query,
            100,
            0,
            None,
            FriendlyDisplayOptions::default(),
            FriendlyConsumer::CtrlK,
        );
        let answer = graph.with_pages(|pages| {
            pre_ready_interactive_snapshot(
                &plan,
                pages,
                || pre_ready_page_inventory(pages, &|| false).map(std::sync::Arc::new),
                false,
                &|| false,
            )
        });
        assert_eq!(
            answer
                .hits
                .iter()
                .filter(|hit| matches!(hit, QueryHit::Page { .. }))
                .count(),
            100
        );
        assert!(answer.has_more.pages);
        assert!(
            answer.hits.iter().any(|hit| matches!(
                hit,
                QueryHit::Page { page, matched_alias, .. }
                    if page.name == "Target"
                        && matched_alias.as_deref() == expected_alias
            )),
            "the exact {query:?} owner beyond 1,000 earlier matches was capped before ranking"
        );
    }
    crate::test_support::remove_dir_all(dir);
}

/// The block evaluator produces evidence and result DTOs once per WINNER
/// rather than once per retained candidate, so a `limit`-bounded search
/// over a large page set does O(limit) evidence work, not O(retained).
#[test]
fn block_evaluator_evaluates_evidence_once_per_winner() {
    const PAGES: usize = 5;
    const BLOCKS: usize = 20;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "tine-query-plan-modes-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    for page in 0..PAGES {
        let mut content = String::new();
        for block in 0..BLOCKS {
            content.push_str(&format!(
                "- parent {page}-{block} needle here
"
            ));
            content.push_str(&format!(
                "	- child {page}-{block} needle nested
"
            ));
        }
        fs::write(dir.join("pages").join(format!("Mode-{page}.md")), content).unwrap();
    }
    let graph = Graph::open(&dir);
    graph.warm_cache();
    let matching = PAGES * BLOCKS * 2;
    for limit in [1_usize, 3, 7, 50, 1_000] {
        let plan = QueryPlan::block_search("needle", limit);
        let _ = take_block_evidence_evaluations();
        let execution = plan.execute(&graph, || false);
        let evaluations = take_block_evidence_evaluations();
        assert_eq!(
            execution.hits.len(),
            limit.min(matching),
            "block search returns limit.min(matching) hits at limit={limit}"
        );
        assert_eq!(
                evaluations,
                limit.min(matching),
                "evidence and DTOs must be produced once per winner, not once per retained candidate, at limit={limit}"
            );
    }
    let _ = fs::remove_dir_all(&dir);
}

fn block_fingerprint(groups: Vec<crate::model::RefGroup>) -> Vec<(String, String)> {
    groups
        .into_iter()
        .flat_map(|group| {
            group
                .blocks
                .into_iter()
                .map(move |block| (group.page.clone(), block.raw))
        })
        .collect()
}

fn reference_literal_search<G: QueryGraph>(
    graph: &G,
    query: &str,
    limit: usize,
) -> Vec<(String, String)> {
    if limit == 0 || query.is_empty() {
        return Vec::new();
    }
    let fragments = query
        .split_whitespace()
        .map(canonical_fold)
        .filter(|fragment| !fragment.is_empty())
        .collect::<Vec<_>>();
    if fragments.is_empty() {
        return Vec::new();
    }
    graph.with_pages(|pages| {
        let mut out = Vec::new();
        fn visit(
            page: &str,
            blocks: &[DocBlock],
            fragments: &[String],
            remaining: &mut usize,
            out: &mut Vec<(String, String)>,
        ) {
            for block in blocks {
                if *remaining == 0 {
                    return;
                }
                let projection = block.projection();
                if fragments
                    .iter()
                    .all(|fragment| projection.visible_lower.contains(fragment))
                {
                    out.push((page.to_string(), block.raw.clone()));
                    *remaining -= 1;
                }
                visit(page, &block.children, fragments, remaining, out);
            }
        }
        let mut remaining = limit;
        for (entry, document) in pages {
            visit(
                &entry.name,
                &document.roots,
                &fragments,
                &mut remaining,
                &mut out,
            );
            if remaining == 0 {
                break;
            }
        }
        out
    })
}

#[test]
fn friendly_simple_term_is_fuzzy_only_for_page_names() {
    let plan = QueryPlan::friendly("opdf", 8, 50);
    assert_eq!(plan.branches.len(), 2);
    assert!(matches!(
        &plan.branches[0].predicate,
        QueryExpr::Text(TextPredicate {
            field: TextField::PageName,
            mode: TextMatchMode::Fuzzy,
            value,
            ..
        }) if value == "opdf"
    ));
    assert!(matches!(
        &plan.branches[1].predicate,
        QueryExpr::Text(TextPredicate {
            field: TextField::VisibleContent,
            mode: TextMatchMode::Contains,
            ..
        })
    ));
}

#[test]
fn only_explicit_interactive_consumers_receive_the_verified_window() {
    use crate::query::candidate::{CandidateMode, INTERACTIVE_VERIFIED_WINDOW};

    let non_interactive = friendly_search_plan_for(
        "needle",
        8,
        50,
        None,
        FriendlyDisplayOptions::default(),
        FriendlyConsumer::NonInteractive,
    );
    let ctrl_k = friendly_search_plan_for(
        "needle",
        8,
        50,
        None,
        FriendlyDisplayOptions::default(),
        FriendlyConsumer::CtrlK,
    );
    assert_eq!(non_interactive.candidate_mode(), CandidateMode::Exhaustive);
    assert_eq!(
        ctrl_k.candidate_mode(),
        CandidateMode::Interactive {
            window: INTERACTIVE_VERIFIED_WINDOW,
        }
    );
    assert_eq!(
        QueryPlan::block_search_literal("needle words", 50).candidate_mode(),
        CandidateMode::Interactive {
            window: INTERACTIVE_VERIFIED_WINDOW,
        }
    );
    assert_eq!(
        QueryPlan::page_name_fuzzy("needle", 50).candidate_mode(),
        CandidateMode::Exhaustive
    );
}

#[test]
fn folded_empty_text_predicates_never_match_rank_or_emit_evidence() {
    for mode in [TextMatchMode::Contains, TextMatchMode::Phrase] {
        let pred = TextPredicate {
            clause_id: 1,
            field: TextField::VisibleContent,
            mode,
            value: String::new(),
        };
        let expr = QueryExpr::Text(pred.clone());
        let plan = QueryPlan {
            branches: Vec::new(),
            diagnostics: Vec::new(),
            page_scope: None,
            display: FriendlyDisplayOptions::default(),
            page_exact: None,
            regexes: HashMap::new(),
            candidate_mode: crate::query::candidate::CandidateMode::Exhaustive,
            page_name_suggestions: false,
        };
        assert!(!eval_expr_fast(
            &plan,
            &expr,
            TextField::VisibleContent,
            "anything",
            "anything",
        ));
        assert!(match_text(&plan, &pred, "anything").is_none());
        assert!(text_predicate_relevance(&plan, &pred, "anything", "anything").is_none());

        let page_pred = TextPredicate {
            field: TextField::PageName,
            ..pred
        };
        assert!(
            page_base_score(&plan, &QueryExpr::Text(page_pred), "anything", "anything",).is_none()
        );
    }
}

#[test]
fn nonempty_queries_erased_by_a6_never_become_picker_match_all() {
    let mark = "\u{301}";
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "tine-query-plan-empty-fold-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(dir.join("pages/Foo.md"), "- foo body\n").unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();

    for query in [mark.to_string(), format!("{mark} foo")] {
        assert!(QueryPlan::friendly(&query, 8, 8)
            .execute(&graph, || false)
            .hits
            .is_empty());
    }
    for query in [format!("{mark} OR foo"), format!("foo -{mark}")] {
        let hits = QueryPlan::friendly(&query, 8, 8)
            .execute(&graph, || false)
            .hits;
        assert!(
            hits.iter().any(|hit| matches!(hit, QueryHit::Page { .. })),
            "{query}"
        );
        assert!(
            hits.iter().any(|hit| matches!(hit, QueryHit::Block { .. })),
            "{query}"
        );
    }
    assert!(QueryPlan::friendly(&format!("-{mark}"), 8, 8)
        .branches
        .is_empty());

    let raw_empty = QueryPlan::legacy_page_search("", 8).execute(&graph, || false);
    assert!(raw_empty
        .hits
        .iter()
        .any(|hit| matches!(hit, QueryHit::Page { .. })));
    assert!(QueryPlan::legacy_page_search(mark, 8)
        .execute(&graph, || false)
        .hits
        .is_empty());
    assert!(QueryPlan::block_search_literal(mark, 8)
        .execute(&graph, || false)
        .hits
        .is_empty());
    assert!(QueryPlan::block_search_literal("", 8).branches.is_empty());

    crate::test_support::remove_dir_all(dir);
}

#[test]
fn a6_search_equivalence_does_not_collapse_page_candidate_identity() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "tine-query-plan-page-identity-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(dir.join("pages/Café.md"), "- real page\n").unwrap();
    fs::write(
        dir.join("pages/References.md"),
        "- [[Cafe]] [[Cafe\u{301}]] [[Ｃａｆｅ]]\n",
    )
    .unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();

    let mut pages = QueryPlan::friendly("cafe", 20, 0)
        .execute(&graph, || false)
        .hits
        .into_iter()
        .filter_map(|hit| match hit {
            QueryHit::Page {
                page, match_class, ..
            } => Some((page.name, page.rel_path, match_class)),
            QueryHit::Block { .. } => None,
        })
        .collect::<Vec<_>>();
    pages.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(pages.len(), 3, "{pages:#?}");
    assert!(pages
        .iter()
        .any(|(name, path, _)| name == "Café" && !path.is_empty()));
    assert!(pages
        .iter()
        .any(|(name, path, _)| name == "Cafe" && path.is_empty()));
    assert!(pages
        .iter()
        .any(|(name, path, _)| name == "Ｃａｆｅ" && path.is_empty()));
    assert!(!pages
        .iter()
        .any(|(name, path, _)| name == "Cafe\u{301}" && path.is_empty()));
    assert!(pages
        .iter()
        .all(|(_, _, class)| *class == ObjectiveMatchClass::Exact));

    crate::test_support::remove_dir_all(dir);
}

#[test]
fn graph_search_reports_per_category_truncation() {
    let (dir, graph) = fixture();

    let page_truncated = crate::query_plan::QueryPlan::friendly("opdf", 2, 1).execute_with_explain(
        &graph,
        || false,
        false,
    );
    assert!(page_truncated.has_more.pages);
    assert!(!page_truncated.has_more.blocks);

    let block_truncated = crate::query_plan::QueryPlan::friendly("foo", 10, 1)
        .execute_with_explain(&graph, || false, false);
    assert!(!block_truncated.has_more.pages);
    assert!(block_truncated.has_more.blocks);

    let complete = crate::query_plan::QueryPlan::friendly("foo", 10, 10).execute_with_explain(
        &graph,
        || false,
        false,
    );
    assert!(!complete.has_more.pages);
    assert!(!complete.has_more.blocks);

    crate::test_support::remove_dir_all(dir);
}

#[test]
fn explicit_fuzzy_subsequence_has_score_and_utf16_spans() {
    let plan = QueryPlan::page_name_fuzzy("of", 8);
    let pred = &plan.branches[0].predicate;
    let hit = eval_expr(&plan, pred, TextField::PageName, "🧠 Opinion Diffusion").unwrap();
    assert_eq!(hit.evidence[0].score, Some(100));
    assert_eq!(
        hit.evidence[0].spans,
        vec![
            MatchSpan { start: 3, end: 4 },
            MatchSpan { start: 13, end: 14 }
        ]
    );
}

#[test]
fn unicode_substring_and_regex_spans_use_utf16_units() {
    let contains = QueryPlan::page_name_fuzzy("foo", 8);
    let hit = eval_expr(
        &contains,
        &contains.branches[0].predicate,
        TextField::PageName,
        "🧠 foo",
    )
    .unwrap();
    assert_eq!(hit.evidence[0].spans, vec![MatchSpan { start: 3, end: 6 }]);

    let regex_plan = QueryPlan::friendly("/foo/", 8, 8);
    let page = &regex_plan.branches[0].predicate;
    let hit = eval_expr(&regex_plan, page, TextField::PageName, "🧠 foo").unwrap();
    assert_eq!(hit.evidence[0].spans, vec![MatchSpan { start: 3, end: 6 }]);
}

#[test]
fn canonical_unicode_matches_pages_aliases_blocks_and_original_utf16_spans() {
    let multiword = QueryPlan::friendly("Canonical page", 8, 8);
    let multiword_page = best_page_match(
        &multiword,
        &multiword.branches[0].predicate,
        "Canonical page",
        &[],
    )
    .unwrap();
    assert_eq!(multiword_page.1, ObjectiveMatchClass::Exact);

    let syntax = QueryPlan::friendly("foo -draft", 8, 8);
    assert!(best_page_match(&syntax, &syntax.branches[0].predicate, "foo -draft", &[],).is_none());
    assert!(best_page_match(&syntax, &syntax.branches[0].predicate, "foo ready", &[],).is_some());

    let page_plan = QueryPlan::page_name_fuzzy("Café", 8);
    let page_pred = &page_plan.branches[0].predicate;
    let page = best_page_match(&page_plan, page_pred, "Cafe\u{301}", &[]).unwrap();
    assert_eq!(page.1, ObjectiveMatchClass::Exact);
    let page_evidence =
        eval_expr(&page_plan, page_pred, TextField::PageName, "Cafe\u{301}").unwrap();
    assert_eq!(
        page_evidence.evidence[0].spans,
        vec![MatchSpan { start: 0, end: 5 }]
    );

    let alias_plan = QueryPlan::page_name_fuzzy("Résumé", 8);
    let alias = best_page_match(
        &alias_plan,
        &alias_plan.branches[0].predicate,
        "Canonical page",
        &["Re\u{301}sume\u{301}".into()],
    )
    .unwrap();
    assert_eq!(alias.1, ObjectiveMatchClass::Exact);
    assert_eq!(alias.3.as_deref(), Some("Re\u{301}sume\u{301}"));

    let block_plan = QueryPlan::block_search("Résumé", 8);
    let block_pred = &block_plan.branches[0].predicate;
    let original = "🧠 Re\u{301}sume\u{301}";
    let folded = canonical_fold(original);
    assert!(eval_expr_fast(
        &block_plan,
        block_pred,
        TextField::VisibleContent,
        original,
        &folded,
    ));
    let evidence = eval_expr(&block_plan, block_pred, TextField::VisibleContent, original).unwrap();
    assert_eq!(
        evidence.evidence[0].spans,
        vec![MatchSpan { start: 3, end: 11 }]
    );

    let hangul = QueryPlan::block_search("\u{ac00}", 8);
    let hit = eval_expr(
        &hangul,
        &hangul.branches[0].predicate,
        TextField::VisibleContent,
        "\u{1100}\u{1161}",
    )
    .unwrap();
    assert_eq!(hit.evidence[0].spans, vec![MatchSpan { start: 0, end: 2 }]);

    let reordered = QueryPlan::block_search("è\u{315}", 8);
    let hit = eval_expr(
        &reordered,
        &reordered.branches[0].predicate,
        TextField::VisibleContent,
        "e\u{315}\u{300}",
    )
    .unwrap();
    assert_eq!(hit.evidence[0].spans, vec![MatchSpan { start: 0, end: 3 }]);

    let accent_fold = QueryPlan::block_search("cafe", 8);
    let hit = eval_expr(
        &accent_fold,
        &accent_fold.branches[0].predicate,
        TextField::VisibleContent,
        "café",
    )
    .unwrap();
    assert_eq!(hit.evidence[0].spans, vec![MatchSpan { start: 0, end: 4 }]);

    let expansion = QueryPlan::block_search("i\u{307}", 8);
    let hit = eval_expr(
        &expansion,
        &expansion.branches[0].predicate,
        TextField::VisibleContent,
        "\u{130}",
    )
    .unwrap();
    assert_eq!(hit.evidence[0].spans, vec![MatchSpan { start: 0, end: 1 }]);
}

#[test]
fn a6_evidence_maps_compatibility_and_removed_marks_to_raw_utf16() {
    let cases = [
        ("f", "😀aﬁx tail", MatchSpan { start: 3, end: 4 }),
        ("tine", "😀Ｔｉｎｅ tail", MatchSpan { start: 2, end: 6 }),
        ("ガイド", "ｶﾞｲﾄﾞ", MatchSpan { start: 0, end: 5 }),
        (
            "\"prilis zlutoucky kun\"",
            "Příliš žluťoučký kůň",
            MatchSpan { start: 0, end: 20 },
        ),
        ("가", "ㄱ\u{301}ㅏ", MatchSpan { start: 0, end: 3 }),
        (
            "\u{1715}\u{302e}",
            "😀a\u{302e}\u{034f}\u{1715}z",
            MatchSpan { start: 3, end: 6 },
        ),
        ("\"ος σ\"", "ΟΣ Σ", MatchSpan { start: 0, end: 4 }),
    ];

    for (needle, original, expected) in cases {
        let plan = QueryPlan::block_search(needle, 8);
        let hit = eval_expr(
            &plan,
            &plan.branches[0].predicate,
            TextField::VisibleContent,
            original,
        )
        .unwrap_or_else(|| panic!("{needle:?} must match {original:?}"));
        assert_eq!(hit.evidence[0].spans, vec![expected], "needle={needle:?}");
    }
}

#[test]
fn mapped_evidence_consumes_the_already_folded_needle_exactly_once() {
    let plan = QueryPlan::block_search("𝐀", 8);
    let QueryExpr::Text(pred) = &plan.branches[0].predicate else {
        panic!("one literal term must remain one predicate");
    };
    assert_eq!(pred.value, "A");
    let hit = eval_expr(
        &plan,
        &plan.branches[0].predicate,
        TextField::VisibleContent,
        "𝐀",
    )
    .unwrap();
    assert_eq!(hit.evidence[0].spans, vec![MatchSpan { start: 0, end: 2 }]);
    assert!(eval_expr(
        &QueryPlan::block_search("a", 8),
        &QueryPlan::block_search("a", 8).branches[0].predicate,
        TextField::VisibleContent,
        "𝐀",
    )
    .is_none());
}

#[test]
fn canonical_unicode_executes_through_real_page_alias_and_block_projections() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "tine-query-plan-unicode-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("pages").join("Cafe\u{301}.md"),
        "alias:: Re\u{301}sume\u{301}\n\n- Re\u{301}sume\u{301}\n",
    )
    .unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();

    let page = QueryPlan::page_name_fuzzy("Café", 8).execute(&graph, || false);
    assert!(matches!(
        page.hits.first(),
        Some(QueryHit::Page {
            page,
            match_class: ObjectiveMatchClass::Exact,
            matched_alias: None,
            evidence,
            ..
        }) if page.name == "Cafe\u{301}"
            && evidence[0].spans == vec![MatchSpan { start: 0, end: 5 }]
    ));

    let alias = QueryPlan::page_name_fuzzy("Résumé", 8).execute(&graph, || false);
    assert!(
        matches!(
            alias.hits.first(),
            Some(QueryHit::Page {
                page,
                match_class: ObjectiveMatchClass::Exact,
                matched_alias: Some(name),
                ..
            }) if page.name == "Cafe\u{301}" && name == "Re\u{301}sume\u{301}"
        ),
        "{:#?}",
        alias.hits
    );

    let blocks = QueryPlan::block_search("Résumé", 8).execute(&graph, || false);
    assert!(matches!(
        blocks.hits.first(),
        Some(QueryHit::Block { display_text, evidence, .. })
            if display_text == "Re\u{301}sume\u{301}"
                && evidence[0].spans == vec![MatchSpan { start: 0, end: 8 }]
    ));
    crate::test_support::remove_dir_all(dir);
}

#[test]
fn page_alias_search_is_scoped_to_its_physical_owner() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "tine-query-plan-alias-owner-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(dir.join("pages").join("sub")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    fs::write(
        dir.join("pages").join("Foo.md"),
        "alias:: bar\n\n- declaring page\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("sub").join("Foo.md"),
        "- same-named sibling\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Unique.md"),
        "alias:: quux\n\n- unique alias owner\n",
    )
    .unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();

    let alias_hits = crate::query_plan::QueryPlan::friendly("bar", 10, 0)
        .execute_with_explain(&graph, || false, false)
        .hits
        .into_iter()
        .filter_map(|hit| match hit {
            QueryHit::Page {
                page,
                match_class,
                matched_alias,
                ..
            } if !page.rel_path.is_empty() => Some((page.rel_path, match_class, matched_alias)),
            QueryHit::Page { .. } => None,
            QueryHit::Block { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        alias_hits,
        vec![(
            "pages/Foo.md".to_string(),
            ObjectiveMatchClass::Exact,
            Some("bar".to_string()),
        )],
        "a duplicate-named sibling must not inherit another file's alias"
    );

    let unique_hits = crate::query_plan::QueryPlan::friendly("quux", 10, 0)
        .execute_with_explain(&graph, || false, false)
        .hits
        .into_iter()
        .filter_map(|hit| match hit {
            QueryHit::Page {
                page,
                match_class,
                matched_alias,
                ..
            } if !page.rel_path.is_empty() => Some((page.rel_path, match_class, matched_alias)),
            QueryHit::Page { .. } => None,
            QueryHit::Block { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        unique_hits,
        vec![(
            "pages/Unique.md".to_string(),
            ObjectiveMatchClass::Exact,
            Some("quux".to_string()),
        )],
        "a unique page name must retain ordinary alias matching"
    );

    crate::test_support::remove_dir_all(dir);
}

#[test]
fn alias_reference_is_never_a_phantom_alias_page() {
    // GH #353: an alias text that is also referenced anywhere in the graph
    // (`[[Book]]`) must not surface as its own selectable page — only the
    // owner page may appear, carrying the matched alias as context. Covers
    // canonical (case-folded) matching and multiple aliases.
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "tine-query-plan-alias-ghost-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    // One owner page carrying TWO aliases (`alias:: Book, Reading`), the
    // alias texts referenced elsewhere in the graph, one real page whose
    // name merely overlaps an alias, and one unrelated referenced page.
    fs::write(
        dir.join("pages").join("Research Hub.md"),
        "alias:: Book, Reading\n\n- actual reading notes\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Real Reading.md"),
        "- a genuinely real page whose name contains an alias\n",
    )
    .unwrap();
    fs::write(
        dir.join("pages").join("Notes.md"),
        "- see [[Book]], [[Reading]] and [[Book Shelf]] for the list\n",
    )
    .unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();

    for query in ["book", "BOOK", "Book", "reading"] {
        let page_hits: Vec<(String, String, Option<String>)> =
            crate::query_plan::QueryPlan::friendly(query, 100, 0)
                .execute_with_explain(&graph, || false, false)
                .hits
                .into_iter()
                .filter_map(|hit| match hit {
                    QueryHit::Page {
                        page,
                        matched_alias,
                        ..
                    } => Some((page.name, page.rel_path, matched_alias)),
                    QueryHit::Block { .. } => None,
                })
                .collect();
        // The exact invariant: an alias's folded text must never be a
        // path-less (referenced/virtual) page candidate. Unrelated
        // referenced pages (e.g. "Book Shelf") MAY appear — they are real.
        assert!(
            page_hits.iter().all(|(name, rel_path, _)| {
                let folded = canonical_fold(name);
                !rel_path.is_empty() || (folded != "book" && folded != "reading")
            }),
            "query {query:?} must not offer a path-less phantom alias page: {page_hits:?}"
        );
        assert!(
            page_hits
                .iter()
                .any(|(name, _, matched_alias)| name == "Research Hub" && matched_alias.is_some()),
            "query {query:?} must carry the matched alias on the owner hit: {page_hits:?}"
        );
        // An ordinary page whose name merely contains the alias text is
        // not affected by the alias-owner dedup ("Real Reading" still
        // shows up for "reading"), and only the owner carries the alias.
        if query == "reading" {
            assert!(
                page_hits.iter().any(|(name, _, matched_alias)| {
                    name == "Real Reading" && matched_alias.is_none()
                }),
                "an ordinary partial name match stays a hit: {page_hits:?}"
            );
        }
        // A genuinely unrelated referenced page is NOT affected.
        let shelf: Vec<String> = crate::query_plan::QueryPlan::friendly("Book Shelf", 100, 0)
            .execute_with_explain(&graph, || false, false)
            .hits
            .into_iter()
            .filter_map(|hit| match hit {
                QueryHit::Page { page, .. } => Some(page.name),
                QueryHit::Block { .. } => None,
            })
            .collect();
        assert!(
            shelf.contains(&"Book Shelf".to_string()),
            "a referenced page with no alias owner still surfaces: {shelf:?}"
        );
        // Ctrl-K inserts the authored alias spelling, while the entry's path
        // retains the owning page identity. That is distinct from a path-less
        // virtual page which merely happens to have the alias's name.
        let switch_entries = graph.quick_switch(query, 100);
        assert!(
            switch_entries.iter().all(|entry| {
                let folded = canonical_fold(&entry.name);
                !entry.rel_path.is_empty() || (folded != "book" && folded != "reading")
            }),
            "quick_switch must not offer an alias-named phantom: {switch_entries:?}"
        );
        assert!(
            switch_entries.iter().any(|entry| {
                matches!(canonical_fold(&entry.name).as_str(), "book" | "reading")
                    && entry.rel_path == "pages/Research Hub.md"
            }),
            "quick_switch must retain alias ownership: {switch_entries:?}"
        );
    }

    crate::test_support::remove_dir_all(dir);
}

#[test]
fn boolean_evidence_uses_positive_terms_and_first_matching_or_branch() {
    let plan = QueryPlan::friendly("foo -draft OR ready", 8, 8);
    let blocks = &plan.branches[1].predicate;
    let first = eval_expr(&plan, blocks, TextField::VisibleContent, "foo ship").unwrap();
    assert_eq!(first.evidence.len(), 1);
    assert_eq!(first.evidence[0].mode, TextMatchMode::Contains);
    assert!(eval_expr(&plan, blocks, TextField::VisibleContent, "foo draft").is_none());
    let second = eval_expr(&plan, blocks, TextField::VisibleContent, "ready draft").unwrap();
    assert_eq!(second.evidence.len(), 1);
    assert_ne!(first.evidence[0].clause_id, second.evidence[0].clause_id);
}

#[test]
fn invalid_regex_is_a_rust_diagnostic_and_matches_nothing() {
    let plan = QueryPlan::friendly("/(unclosed/", 8, 8);
    assert!(plan.branches.is_empty());
    assert_eq!(plan.diagnostics.len(), 1);
    assert_eq!(plan.diagnostics[0].code, "invalid_regex");
    assert_eq!(
        plan.diagnostics[0].span,
        Some(MatchSpan { start: 0, end: 11 })
    );
}

#[test]
fn zero_limit_friendly_plans_still_classify_saveable_sources() {
    let rust_only_invalid = QueryPlan::friendly("/(a)\\1/", 0, 0);
    assert!(rust_only_invalid.branches.is_empty());
    assert_eq!(
        rust_only_invalid
            .diagnostics
            .first()
            .map(|item| item.code.as_str()),
        Some("invalid_regex")
    );
    let valid = QueryPlan::friendly("alpha", 0, 0);
    assert!(!valid.branches.is_empty());
    assert!(valid.branches.iter().all(|branch| branch.limit == 0));
    let excluded = QueryPlan::friendly("-draft", 0, 0);
    assert!(excluded.diagnostics.is_empty() && excluded.branches.is_empty());
}

#[test]
fn pure_negation_and_empty_friendly_search_have_no_branches() {
    assert!(QueryPlan::friendly("-draft", 8, 8).branches.is_empty());
    assert!(QueryPlan::friendly("", 8, 8).branches.is_empty());
    assert_eq!(QueryPlan::legacy_page_search("-draft", 8).branches.len(), 1);
}

#[test]
fn matching_modes_are_explicit() {
    let pred = text_pred(TextMatchMode::Fuzzy, "abc");
    assert_eq!(pred.mode, TextMatchMode::Fuzzy);
    let plan = QueryPlan::friendly("\"exact phrase\"", 8, 8);
    assert!(matches!(
        &plan.branches[1].predicate,
        QueryExpr::Text(TextPredicate {
            mode: TextMatchMode::Phrase,
            ..
        })
    ));
}

#[test]
fn combined_execution_returns_typed_ranked_hits_and_exact_evidence_text() {
    let (dir, graph) = fixture();
    let pages = QueryPlan::page_name_fuzzy("opdf", 10).execute(&graph, || false);
    let names = pages
        .hits
        .iter()
        .filter_map(|hit| match hit {
            QueryHit::Page { page, .. } => Some(page.name.as_str()),
            QueryHit::Block { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(names.first().copied(), Some("Opdf Notes"));
    assert!(
        names.iter().position(|name| *name == "Xopdf")
            < names.iter().position(|name| *name == "Opinion Diffusion")
    );
    let file_hit = pages.hits.iter().find_map(|hit| match hit {
        QueryHit::Page { page, .. } if page.name == "Opdf Notes" => Some(page),
        _ => None,
    });
    assert_eq!(file_hit.unwrap().rel_path, "pages/Opdf Notes.md");
    let virtual_hit = pages.hits.iter().find_map(|hit| match hit {
        QueryHit::Page { page, .. } if page.name == "Virtual Opdf" => Some(page),
        _ => None,
    });
    assert_eq!(virtual_hit.unwrap().rel_path, "");

    let execution = crate::query_plan::QueryPlan::friendly("foo -draft OR ready", 10, 10)
        .execute_with_explain(&graph, || false, true);
    let blocks = execution
        .hits
        .iter()
        .filter_map(|hit| match hit {
            QueryHit::Block {
                block,
                display_text,
                evidence,
                ..
            } => Some((block, display_text, evidence)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(blocks.len(), 2);
    let nested = blocks
        .iter()
        .find(|(_, text, _)| text.as_str() == "🧠 foo ready")
        .unwrap();
    assert_eq!(nested.0.breadcrumb, vec!["Parent"]);
    assert_eq!(nested.2.len(), 1, "successful NOT has no positive evidence");
    assert_eq!(nested.2[0].spans, vec![MatchSpan { start: 3, end: 6 }]);
    assert!(!execution.explanation.branches.is_empty());
    let explanation = serde_json::to_string(&execution.explanation).unwrap();
    assert!(explanation.contains("Block text"));
    assert!(explanation.contains("contains “foo”"));
    assert!(!explanation.contains("PageName"));
    assert!(!explanation.contains("VisibleContent"));

    let no_explain = crate::query_plan::QueryPlan::friendly("foo", 10, 10).execute_with_explain(
        &graph,
        || false,
        false,
    );
    assert!(no_explain.explanation.branches.is_empty());
    crate::test_support::remove_dir_all(dir);
}

#[test]
fn current_page_scope_is_block_only_and_path_authoritative() {
    let (dir, graph) = fixture();
    fs::create_dir_all(dir.join("pages").join("duplicate")).unwrap();
    fs::write(
        dir.join("pages")
            .join("duplicate")
            .join("Opinion Diffusion.md"),
        "- duplicate foo\n",
    )
    .unwrap();
    graph.warm_cache();

    let execution = QueryPlan::friendly_for_page(
        "foo",
        50,
        QueryPageScope {
            name: "Opinion Diffusion".into(),
            page_kind: PageKind::Page,
            path: Some("pages/Opinion Diffusion.md".into()),
        },
    )
    .execute(&graph, || false);
    assert!(!execution.hits.is_empty());
    assert!(execution.hits.iter().all(|hit| matches!(
        hit,
        QueryHit::Block { page, path, block, .. }
            if page == "Opinion Diffusion"
                && path == "pages/Opinion Diffusion.md"
                && block.raw != "duplicate foo"
    )));
    crate::test_support::remove_dir_all(dir);
}

#[test]
fn page_hits_expose_objective_classes_and_alias_evidence() {
    let (dir, graph) = fixture();
    let exact = QueryPlan::page_name_fuzzy("Opdf Notes", 10).execute(&graph, || false);
    assert!(matches!(
        exact.hits.first(),
        Some(QueryHit::Page {
            match_class: ObjectiveMatchClass::Exact,
            matched_alias: None,
            ..
        })
    ));

    let alias = QueryPlan::page_name_fuzzy("Research Hub", 10).execute(&graph, || false);
    let hit = alias
        .hits
        .iter()
        .find(|hit| matches!(hit, QueryHit::Page { page, .. } if page.name == "Opdf Notes"));
    assert!(matches!(
        hit,
        Some(QueryHit::Page {
            display_text,
            match_class: ObjectiveMatchClass::Exact,
            matched_alias: Some(matched_alias),
            ..
        }) if display_text == "Research Hub" && matched_alias == "Research Hub"
    ));
    crate::test_support::remove_dir_all(dir);
}

#[test]
fn regex_evidence_is_authoritative_and_bounded_to_projected_text() {
    let (dir, graph) = fixture();
    let execution = crate::query_plan::QueryPlan::friendly("/[A-Z]{3}/", 10, 10)
        .execute_with_explain(&graph, || false, true);
    let (text, evidence) = execution
        .hits
        .iter()
        .find_map(|hit| match hit {
            QueryHit::Block {
                display_text,
                evidence,
                ..
            } if display_text == "regex ABC" => Some((display_text, evidence)),
            _ => None,
        })
        .unwrap();
    assert_eq!(text, "regex ABC");
    assert_eq!(evidence[0].spans, vec![MatchSpan { start: 6, end: 9 }]);
    crate::test_support::remove_dir_all(dir);
}

#[test]
fn literal_block_search_adapter_requires_whitespace_fragments_and_preserves_ranked_topk() {
    let (dir, graph) = fixture();
    for query in [
        "",
        "foo",
        "foo ready",
        "foo OR ready",
        "foo -draft",
        "-draft",
        "\"foo ready\"",
        "/[A-Z]{3}/",
        "/(unclosed/",
    ] {
        let full = block_fingerprint(crate::query::search_cancellable(
            &graph,
            query,
            usize::MAX,
            || false,
        ));
        let mut full_membership = full.clone();
        full_membership.sort();
        let mut reference = reference_literal_search(&graph, query, usize::MAX);
        reference.sort();
        assert_eq!(full_membership, reference, "query={query:?}");
        for limit in [0, 1, 2, 20] {
            assert_eq!(
                block_fingerprint(crate::query::search_cancellable(
                    &graph,
                    query,
                    limit,
                    || false
                )),
                full.iter().take(limit).cloned().collect::<Vec<_>>(),
                "query={query:?} limit={limit}"
            );
        }
    }
    crate::test_support::remove_dir_all(dir);
}

#[test]
fn query_hit_json_contract_uses_tagged_entities_and_utf16_evidence() {
    let hit = QueryHit::Page {
        page: PageEntry {
            name: "🧠 Foo".into(),
            kind: PageKind::Page,
            date_key: None,
            rel_path: "pages/Foo.md".into(),
            path: std::path::PathBuf::new(),
        },
        display_text: "🧠 Foo".into(),
        evidence: vec![MatchEvidence {
            clause_id: 7,
            field: TextField::PageName,
            mode: TextMatchMode::Fuzzy,
            spans: vec![MatchSpan { start: 3, end: 6 }],
            score: None,
        }],
        score: 1_000,
        match_class: ObjectiveMatchClass::Prefix,
        matched_alias: None,
        // Additive and absent: the wire shape of a hit that carries no
        // hydrated row is EXACTLY the shape it had before this field.
        row: None,
    };

    assert_eq!(
        serde_json::to_value(hit).unwrap(),
        serde_json::json!({
            "entity": "page",
            "page": {
                "name": "🧠 Foo",
                "kind": "page",
                "date_key": null,
                "path": "pages/Foo.md"
            },
            "display_text": "🧠 Foo",
            "evidence": [{
                "clause_id": 7,
                "field": "page_name",
                "mode": "fuzzy",
                "spans": [{"start": 3, "end": 6}]
            }],
            "score": 1_000,
            "match_class": "prefix"
        })
    );
}

#[test]
fn cancellation_discards_all_partial_combined_results() {
    let (dir, graph) = fixture();
    let checks = Cell::new(0usize);
    let execution = QueryPlan::friendly("unrelated", 10, 10).execute(&graph, || {
        checks.set(checks.get() + 1);
        checks.get() > 3
    });
    assert!(execution.cancelled);
    assert!(execution.hits.is_empty());
    crate::test_support::remove_dir_all(dir);
}

// -----------------------------------------------------------------
// Page text rank bridge: owner-local name/alias choice stays distinct
// from the global page ordering consumed by SQLite.
// -----------------------------------------------------------------

fn page_branch(plan: &QueryPlan) -> Option<&QueryBranch> {
    plan.branches
        .iter()
        .find(|branch| branch.target == QueryTarget::Pages)
}

fn bridge_page_choice<'a>(
    plan: &QueryPlan,
    branch: &QueryBranch,
    name: &'a str,
    aliases: &'a [&'a str],
) -> Option<(PageTextRank, &'a str, Option<&'a str>)> {
    let mut best = rank_page_text(plan, branch, name).map(|rank| (rank, name, None));
    for &alias in aliases {
        let Some(rank) = rank_page_text(plan, branch, alias) else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|(current, _, _)| rank.is_better_owner_choice_than(current))
        {
            best = Some((rank, alias, Some(alias)));
        }
    }
    best
}

// Independent pre-extraction owner-selection oracle from a6f49357. This
// deliberately does not call PageTextRank or its owner comparator.
fn legacy_page_choice(
    plan: &QueryPlan,
    expr: &QueryExpr,
    page_name: &str,
    aliases: &[String],
) -> Option<(i32, ObjectiveMatchClass, String, Option<String>)> {
    let page_match = page_base_score(plan, expr, page_name, &canonical_fold(page_name));
    let mut best = page_match.map(|(score, class)| (score, class, page_name.to_string(), None));
    for alias in aliases {
        let Some((score, class)) = page_base_score(plan, expr, alias, &canonical_fold(alias))
        else {
            continue;
        };
        let replace = best.as_ref().is_none_or(|(best_score, best_class, _, _)| {
            class.rank() > best_class.rank() || (class == *best_class && score > *best_score)
        });
        if replace {
            best = Some((score, class, alias.clone(), Some(alias.clone())));
        }
    }
    // Upgrade only an outcome that already satisfied the parsed expression.
    // This repairs the objective class for ordinary multi-word titles without
    // bypassing NOT/OR/regex membership semantics for syntax-looking names.
    if let Some(exact) = plan.page_exact.as_deref() {
        if page_match.is_some() && canonical_fold(page_name) == exact {
            return Some((
                1500,
                ObjectiveMatchClass::Exact,
                page_name.to_string(),
                None,
            ));
        }
        if let Some(alias) = aliases.iter().find(|alias| {
            canonical_fold(alias) == exact
                && page_base_score(plan, expr, alias, &canonical_fold(alias)).is_some()
        }) {
            return Some((
                1500,
                ObjectiveMatchClass::Exact,
                alias.clone(),
                Some(alias.clone()),
            ));
        }
    }
    best
}

#[test]
fn page_rank_bridge_matches_best_page_match_across_compiled_shapes() {
    let cases = [
        (
            QueryPlan::page_name_fuzzy("opdf", 8),
            "Opdf Notes",
            vec!["Research Hub"],
        ),
        (
            QueryPlan::friendly("foo ready", 8, 8),
            "foo ready notes",
            vec!["unrelated"],
        ),
        (
            QueryPlan::friendly("zzz OR ready", 8, 8),
            "ready page",
            vec!["zzz alias"],
        ),
        (
            QueryPlan::friendly("foo -draft", 8, 8),
            "foo ready",
            vec!["foo draft"],
        ),
        (
            QueryPlan::friendly("/A[BC]+/", 8, 8),
            "regex ABC",
            vec!["regex ACC"],
        ),
        (
            QueryPlan::friendly("foo -draft", 8, 8),
            "foo -draft",
            vec![],
        ),
    ];
    for (plan, name, aliases) in &cases {
        let branch = page_branch(plan).expect("the case must plan a page branch");
        let owned_aliases = aliases
            .iter()
            .map(|alias| (*alias).to_string())
            .collect::<Vec<_>>();
        let expected = legacy_page_choice(plan, &branch.predicate, name, &owned_aliases);
        let actual = bridge_page_choice(plan, branch, name, aliases);
        assert_eq!(actual.is_some(), expected.is_some(), "name={name:?}");
        if let (Some((rank, text, alias)), Some((score, class, expected_text, expected_alias))) =
            (actual, expected)
        {
            assert_eq!((rank.base_score(), rank.match_class()), (score, class));
            assert_eq!(text, expected_text);
            assert_eq!(alias.map(str::to_string), expected_alias);
        }
    }

    assert!(QueryPlan::friendly("", 8, 8).branches.is_empty());
    assert!(QueryPlan::friendly("/(unclosed/", 8, 8).branches.is_empty());
}

#[test]
fn page_rank_bridge_owner_choice_keeps_exact_override_and_stable_equal_ties_separate() {
    let override_plan = QueryPlan::friendly("foo OR bar", 8, 8);
    let override_branch = page_branch(&override_plan).unwrap();
    let name = rank_page_text(&override_plan, override_branch, "foo").unwrap();
    let alias = rank_page_text(&override_plan, override_branch, "foo OR bar").unwrap();
    assert_eq!(
        (name.base_score(), name.match_class()),
        (1500, ObjectiveMatchClass::Exact)
    );
    assert_eq!(
        (alias.base_score(), alias.match_class()),
        (1500, ObjectiveMatchClass::Exact)
    );
    assert!(!name.is_exact_override());
    assert!(alias.is_exact_override());
    assert!(alias.is_better_owner_choice_than(&name));
    assert_eq!(
        alias.global_order_key("Owner"),
        name.global_order_key("Owner")
    );
    let selected =
        bridge_page_choice(&override_plan, override_branch, "foo", &["foo OR bar"]).unwrap();
    assert_eq!((selected.1, selected.2), ("foo OR bar", Some("foo OR bar")));

    let equal_plan = QueryPlan::page_name_fuzzy("foo", 8);
    let equal_branch = page_branch(&equal_plan).unwrap();
    let name_wins =
        bridge_page_choice(&equal_plan, equal_branch, "foo name", &["foo alias"]).unwrap();
    assert_eq!((name_wins.1, name_wins.2), ("foo name", None));
    let first_alias_wins = bridge_page_choice(
        &equal_plan,
        equal_branch,
        "unrelated",
        &["foo first", "foo later"],
    )
    .unwrap();
    assert_eq!(
        (first_alias_wins.1, first_alias_wins.2),
        ("foo first", Some("foo first"))
    );
}

#[test]
fn page_owner_rank_blob_matches_the_existing_strict_comparator() {
    let classes = [
        ObjectiveMatchClass::Exact,
        ObjectiveMatchClass::Prefix,
        ObjectiveMatchClass::Substring,
        ObjectiveMatchClass::Fuzzy,
        ObjectiveMatchClass::BodyEvidence,
    ];
    let ranks = [false, true].into_iter().flat_map(|exact_override| {
        classes.into_iter().flat_map(move |match_class| {
            [i32::MIN, -1, 0, 1, i32::MAX]
                .into_iter()
                .map(move |base_score| PageTextRank {
                    base_score,
                    match_class,
                    exact_override,
                })
        })
    });
    let ranks = ranks.collect::<Vec<_>>();
    for left in &ranks {
        for right in &ranks {
            assert_eq!(
                left.owner_order_key() < right.owner_order_key(),
                left.is_better_owner_choice_than(right)
            );
            assert_eq!(
                left.owner_order_key() == right.owner_order_key(),
                !left.is_better_owner_choice_than(right)
                    && !right.is_better_owner_choice_than(left)
            );
        }
    }
}

#[test]
fn page_rank_bridge_blob_matches_scored_page_class_then_signed_score_order() {
    let classes = [
        ObjectiveMatchClass::Exact,
        ObjectiveMatchClass::Prefix,
        ObjectiveMatchClass::Substring,
        ObjectiveMatchClass::Fuzzy,
        ObjectiveMatchClass::BodyEvidence,
    ];
    let scores = [i32::MIN, -1, 0, 1, i32::MAX];
    let ranks = classes
        .into_iter()
        .flat_map(|match_class| {
            scores.into_iter().map(move |base_score| PageTextRank {
                base_score,
                match_class,
                exact_override: false,
            })
        })
        .collect::<Vec<_>>();
    for left in &ranks {
        for right in &ranks {
            let left_page = ScoredPage {
                score: left.base_score,
                match_class: left.match_class,
                matched_text: String::new(),
                matched_alias: None,
                tie_key: String::new(),
                candidate: PageCandidate::Referenced(PageEntry {
                    name: String::new(),
                    kind: PageKind::Page,
                    date_key: None,
                    rel_path: String::new(),
                    path: PathBuf::new(),
                }),
            };
            let right_page = ScoredPage {
                score: right.base_score,
                match_class: right.match_class,
                matched_text: String::new(),
                matched_alias: None,
                tie_key: String::new(),
                candidate: PageCandidate::Referenced(PageEntry {
                    name: String::new(),
                    kind: PageKind::Page,
                    date_key: None,
                    rel_path: String::new(),
                    path: PathBuf::new(),
                }),
            };
            assert_eq!(
                left.global_order_key("").cmp(&right.global_order_key("")),
                right
                    .match_class
                    .rank()
                    .cmp(&left.match_class.rank())
                    .then_with(|| right.base_score.cmp(&left.base_score))
            );
            assert_eq!(
                left.global_order_key("") < right.global_order_key(""),
                left_page.is_better_than(&right_page)
            );
        }
    }
}

#[test]
fn page_rank_bridge_rank_only_builds_no_evidence_and_admitted_evidence_is_utf16_exact() {
    let plan = QueryPlan::page_name_fuzzy("caf\u{e9}", 8);
    let branch = page_branch(&plan).unwrap();
    let text = "\u{1D11E} CAFE\u{301}";
    TEXT_EVIDENCE_EVALUATIONS.with(|count| count.set(0));
    let rank = rank_page_text(&plan, branch, text).expect("canonical fold must match");
    assert_eq!(rank.match_class(), ObjectiveMatchClass::Substring);
    TEXT_EVIDENCE_EVALUATIONS
        .with(|count| assert_eq!(count.get(), 0, "page rank selection must produce no spans"));
    let evidence = admitted_page_evidence(&plan, branch, text).unwrap();
    TEXT_EVIDENCE_EVALUATIONS
        .with(|count| assert!(count.get() > 0, "the actual span producer must be observed"));
    assert_eq!(evidence[0].spans, vec![MatchSpan { start: 3, end: 8 }]);
    assert_eq!(evidence[0].field, TextField::PageName);

    let block = plan
        .branches
        .iter()
        .find(|candidate| candidate.target == QueryTarget::Blocks);
    assert!(block.is_none(), "page-only plan has no block branch");
    let mixed = QueryPlan::friendly("caf\u{e9}", 8, 8);
    let block = mixed
        .branches
        .iter()
        .find(|candidate| candidate.target == QueryTarget::Blocks)
        .unwrap();
    assert!(rank_page_text(&mixed, block, text).is_none());
    assert!(admitted_page_evidence(&mixed, block, text).is_none());
}

#[test]
fn page_rank_bridge_handles_zero_limits_actual_alias_hits_and_virtual_names() {
    let zero = QueryPlan::friendly("opdf", 0, 0);
    let zero_branch = page_branch(&zero).unwrap();
    assert!(rank_page_text(&zero, zero_branch, "Opdf Notes").is_some());

    let (dir, graph) = fixture();
    let plan = QueryPlan::friendly("Research Hub", 10, 0);
    let branch = page_branch(&plan).unwrap();
    let alias_rank = rank_page_text(&plan, branch, "research hub").unwrap();
    let alias_hit = plan
        .execute(&graph, || false)
        .hits
        .into_iter()
        .find_map(|hit| match hit {
            QueryHit::Page {
                page,
                display_text,
                score,
                match_class,
                matched_alias,
                ..
            } if page.name == "Opdf Notes" => {
                Some((page, display_text, score, match_class, matched_alias))
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(alias_hit.1, "Research Hub");
    assert_eq!(alias_hit.2, alias_rank.global_score(&alias_hit.0.name));
    assert_eq!(alias_hit.3, alias_rank.match_class());
    assert_eq!(alias_hit.4.as_deref(), Some("Research Hub"));

    let virtual_plan = QueryPlan::friendly("Virtual Opdf", 10, 0);
    let virtual_branch = page_branch(&virtual_plan).unwrap();
    let virtual_rank = rank_page_text(&virtual_plan, virtual_branch, "Virtual Opdf").unwrap();
    let virtual_hit = virtual_plan
        .execute(&graph, || false)
        .hits
        .into_iter()
        .find_map(|hit| match hit {
            QueryHit::Page {
                page,
                score,
                match_class,
                ..
            } if page.name == "Virtual Opdf" => Some((page, score, match_class)),
            _ => None,
        })
        .unwrap();
    assert!(virtual_hit.0.rel_path.is_empty());
    assert_eq!(
        virtual_hit.1,
        virtual_rank.global_score(&virtual_hit.0.name)
    );
    assert_eq!(virtual_hit.2, virtual_rank.match_class());
    crate::test_support::remove_dir_all(dir);
}

// -----------------------------------------------------------------
// Block text rank bridge: the text-only seam a SQLite adapter binds
// its ORDER BY to. Ranking authority stays here; the adapter only
// sorts the bytes and supplies the equal-rank tie breakers.
// -----------------------------------------------------------------

fn relevance(
    match_class: ObjectiveMatchClass,
    word_boundary: bool,
    first_offset: usize,
    text_len: usize,
    occurrences: usize,
) -> BlockRelevance {
    BlockRelevance {
        match_class,
        word_boundary,
        first_offset,
        text_len,
        occurrences,
        positive: true,
    }
}

fn block_branch(plan: &QueryPlan) -> Option<&QueryBranch> {
    plan.branches
        .iter()
        .find(|branch| branch.target == QueryTarget::Blocks)
}

/// Read the five components back out of the key, proving the encoding is
/// lossless rather than a hash of the tuple.
fn decode_rank_key(key: &[u8; BLOCK_RANK_KEY_LEN]) -> (i32, bool, usize, usize, usize) {
    let class_rank = ((!u32::from_be_bytes(key[0..4].try_into().unwrap())) ^ (1u32 << 31)) as i32;
    let number = |at: usize| {
        usize::try_from(u64::from_be_bytes(key[at..at + 8].try_into().unwrap())).unwrap()
    };
    (class_rank, key[4] == 0, number(5), number(13), number(21))
}

/// Fail-before gate. `score()` saturates offsets, lengths and occurrence
/// counts, so two objectively UNEQUAL tuples collide on it; ordering by the
/// display score would make their order arbitrary. The BLOB key must still
/// separate them, in the direction `cmp_quality` chose.
#[test]
fn rank_blob_separates_large_tuples_whose_display_score_collides() {
    let collisions = [
        // Both offsets are past the 50_000 penalty clamp.
        (
            relevance(ObjectiveMatchClass::Exact, true, 60_000, 0, 1),
            relevance(ObjectiveMatchClass::Exact, true, 70_000, 0, 1),
        ),
        // Both lengths are past the 40_000 clamp.
        (
            relevance(ObjectiveMatchClass::Substring, false, 7, 40_001, 3),
            relevance(ObjectiveMatchClass::Substring, false, 7, 900_000, 3),
        ),
        // Both occurrence counts are past the 9_999 clamp, and the existing
        // rule that FEWER occurrences win is preserved.
        (
            relevance(ObjectiveMatchClass::Prefix, true, 0, 10, 10_001),
            relevance(ObjectiveMatchClass::Prefix, true, 0, 10, 25_000),
        ),
        // The extreme: every component of the worse tuple is `usize::MAX`
        // and every one of them still collapses into the same clamp.
        (
            relevance(ObjectiveMatchClass::Fuzzy, true, 50_001, 40_001, 10_000),
            relevance(
                ObjectiveMatchClass::Fuzzy,
                true,
                usize::MAX,
                usize::MAX,
                usize::MAX,
            ),
        ),
    ];
    for (better, worse) in collisions {
        assert_eq!(
            better.score(),
            worse.score(),
            "the display score must actually collide for this gate to mean anything"
        );
        assert_eq!(better.cmp_quality(&worse), Ordering::Greater);
        assert!(
            better.order_key() < worse.order_key(),
            "ascending BLOB order must still put the better tuple first"
        );
    }

    // Sorting by the key alone, with no access to the tuple, recovers the
    // order `cmp_quality` intended.
    for (better, worse) in collisions {
        let mut keys = [worse.order_key(), better.order_key()];
        keys.sort();
        assert_eq!(keys, [better.order_key(), worse.order_key()]);
    }
}

/// The BLOB order is EXACTLY `cmp_quality` reversed at every component
/// boundary -- both saturation cliffs and `usize::MAX` -- in both
/// directions and on equality.
#[test]
fn rank_blob_order_is_cmp_quality_reversed_at_every_component_boundary() {
    let classes = [
        ObjectiveMatchClass::Exact,
        ObjectiveMatchClass::Prefix,
        ObjectiveMatchClass::Substring,
        ObjectiveMatchClass::Fuzzy,
        ObjectiveMatchClass::BodyEvidence,
    ];
    let offsets = [0usize, 1, 50_000, 50_001, usize::MAX];
    let lengths = [0usize, 1, 40_000, 40_001, usize::MAX];
    let counts = [0usize, 1, 2, 9_999, 10_000, usize::MAX];
    let mut tuples = Vec::new();
    for class in classes {
        for word_boundary in [false, true] {
            for &first_offset in &offsets {
                for &text_len in &lengths {
                    for &occurrences in &counts {
                        tuples.push(relevance(
                            class,
                            word_boundary,
                            first_offset,
                            text_len,
                            occurrences,
                        ));
                    }
                }
            }
        }
    }
    assert_eq!(tuples.len(), 5 * 2 * 5 * 5 * 6);
    let keys = tuples
        .iter()
        .map(BlockRelevance::order_key)
        .collect::<Vec<_>>();
    for (left_at, left) in tuples.iter().enumerate() {
        for (right_at, right) in tuples.iter().enumerate() {
            assert_eq!(
                keys[left_at].cmp(&keys[right_at]),
                right.cmp_quality(left),
                "ascending key order must be cmp_quality reversed for {left:?} vs {right:?}"
            );
        }
    }
}

/// The key carries every component losslessly, and carries nothing else:
/// `positive` is a membership detail, not a `cmp_quality` component.
#[test]
fn rank_blob_is_lossless_and_ignores_the_non_ordering_positive_flag() {
    let sample = relevance(ObjectiveMatchClass::Substring, true, 12_345, usize::MAX, 7);
    let key = sample.order_key();
    assert_eq!(key.len(), 29);
    assert_eq!(
        decode_rank_key(&key),
        (
            ObjectiveMatchClass::Substring.rank(),
            true,
            12_345,
            usize::MAX,
            7
        )
    );

    let unbounded = relevance(ObjectiveMatchClass::Substring, false, 12_345, usize::MAX, 7);
    assert_eq!(unbounded.order_key()[4], 1);
    assert!(key < unbounded.order_key(), "a boundary match sorts first");

    let mut negated = sample;
    negated.positive = false;
    assert_eq!(negated.order_key(), key);
    assert_eq!(negated.cmp_quality(&sample), Ordering::Equal);
}

/// The point of a BLOB key is that the DATABASE does the sort. This binds
/// real bridge keys and lets SQLite's own `ORDER BY ... ASC` produce the
/// order, then checks it against the independent `cmp_quality`.
#[test]
fn sqlite_order_by_bound_rank_blobs_reproduces_the_ranked_order() {
    let plan = QueryPlan::friendly("ready", 8, 8);
    let branch = block_branch(&plan).expect("a bare term plans a block branch");
    let texts = [
        "ready",
        "ready only",
        "not ready yet",
        "alreadyx",
        "ready ready ready",
        "a rather long line that only mentions ready quite late in its text",
    ];

    let connection = rusqlite::Connection::open_in_memory().unwrap();
    connection
        .execute(
            "CREATE TABLE ranked (label TEXT NOT NULL, rank_key BLOB NOT NULL)",
            [],
        )
        .unwrap();
    for text in texts {
        let rank =
            rank_block_text(&plan, branch, text).unwrap_or_else(|| panic!("{text:?} must match"));
        connection
            .execute(
                "INSERT INTO ranked (label, rank_key) VALUES (?1, ?2)",
                rusqlite::params![text, rank.order_key().to_vec()],
            )
            .unwrap();
    }
    let mut statement = connection
        .prepare("SELECT label FROM ranked ORDER BY rank_key ASC")
        .unwrap();
    let ordered = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let reference = |text: &str| {
        block_relevance(&plan, &branch.predicate, text, &canonical_fold(text)).unwrap()
    };
    let mut expected = texts.to_vec();
    expected.sort_by(|left, right| reference(right).cmp_quality(&reference(left)));
    assert_eq!(ordered, expected);
    // Pin the ends so an accidentally uniform key cannot pass vacuously.
    assert_eq!(ordered.first().map(String::as_str), Some("ready"));
    assert_eq!(ordered.last().map(String::as_str), Some("alreadyx"));
}

/// Every compiled predicate shape, driven through the bridge on text alone
/// and through the existing evaluator on the same text.
#[test]
fn rank_bridge_reproduces_block_relevance_for_every_compiled_shape() {
    let texts = [
        "ready",
        "🧠 foo ready",
        "foo draft",
        "ready only",
        "regex ABC",
        "abc ready foo abc",
        "unrelated",
        "",
    ];
    let plans = [
        QueryPlan::friendly("ready", 8, 8),         // literal contains
        QueryPlan::friendly("\"foo ready\"", 8, 8), // phrase
        QueryPlan::friendly("/A[BC]+/", 8, 8),      // compiled regex
        QueryPlan::friendly("foo ready", 8, 8),     // AND
        QueryPlan::friendly("zzz OR ready", 8, 8),  // OR
        QueryPlan::friendly("foo -draft", 8, 8),    // AND with NOT
        QueryPlan::block_search("ready", 8),        // block-only plan
        QueryPlan::block_search_literal("rdy", 8),  // fuzzy subsequence
    ];
    for plan in &plans {
        let branch = block_branch(plan).expect("every plan here has a block branch");
        for text in texts {
            let expected = block_relevance(plan, &branch.predicate, text, &canonical_fold(text));
            let actual = rank_block_text(plan, branch, text);
            assert_eq!(
                actual.is_some(),
                expected.is_some(),
                "membership must not change for {text:?}"
            );
            if let (Some(actual), Some(expected)) = (actual, expected) {
                assert_eq!(actual.order_key(), expected.order_key());
                assert_eq!(actual.score(), expected.score());
                assert_eq!(actual.match_class(), expected.match_class);
            }
        }
    }
}

/// The stronger check: the bridge, holding nothing but visible text, agrees
/// with what real graph-backed execution ranked those very blocks at --
/// including that its own `canonical_fold` reproduces the projection's
/// cached `visible_lower`.
#[test]
fn rank_bridge_agrees_with_executed_block_hits_over_real_projected_text() {
    let (dir, graph) = fixture();
    for query in [
        "ready",
        "foo ready",
        "zzz OR ready",
        "foo -draft",
        "/A[BC]+/",
        "\"foo ready\"",
    ] {
        let plan = QueryPlan::friendly(query, 10, 10);
        let branch = block_branch(&plan).expect("every query here plans a block branch");
        let hits = plan
            .execute(&graph, || false)
            .hits
            .into_iter()
            .filter_map(|hit| match hit {
                QueryHit::Block {
                    display_text,
                    score,
                    match_class,
                    ..
                } => Some((display_text, score, match_class)),
                QueryHit::Page { .. } => None,
            })
            .collect::<Vec<_>>();
        assert!(!hits.is_empty(), "{query} matched no block");
        let mut previous: Option<[u8; BLOCK_RANK_KEY_LEN]> = None;
        for (display_text, score, match_class) in hits {
            let rank = rank_block_text(&plan, branch, &display_text)
                .unwrap_or_else(|| panic!("{query}: bridge rejected executed hit"));
            assert_eq!(rank.score(), score, "{query}: {display_text:?}");
            assert_eq!(rank.match_class(), match_class, "{query}: {display_text:?}");
            let key = rank.order_key();
            if let Some(previous) = previous {
                assert!(
                    previous <= key,
                    "{query}: executed order must be non-decreasing in the BLOB key"
                );
            }
            previous = Some(key);
        }
    }
    crate::test_support::remove_dir_all(dir);
}

/// A6 fold + UTF-16 offsets, computed from visible text alone. The
/// leading musical symbol is two UTF-16 units but one scalar, so a
/// char-counting encoder would report offset 2 rather than 3.
#[test]
fn rank_bridge_folds_unicode_and_counts_utf16_units() {
    // Decomposed "CAFE" + combining acute; the parsed needle is precomposed.
    let text = "\u{1D11E} CAFE\u{301} note";
    let plan = QueryPlan::friendly("caf\u{e9}", 8, 8);
    let branch = block_branch(&plan).expect("a bare term plans a block branch");
    let rank =
        rank_block_text(&plan, branch, text).expect("A6 must admit the decomposed block text");
    assert_eq!(rank.match_class(), ObjectiveMatchClass::Substring);
    assert_eq!(
        decode_rank_key(&rank.order_key()),
        (ObjectiveMatchClass::Substring.rank(), true, 3, 13, 1)
    );
    assert_eq!(
        text.chars().count(),
        12,
        "UTF-16 length is not the char count"
    );

    // The precomposed spelling folds to the same needle position and class;
    // only `text_len` differs, because UTF-16 length is measured on the
    // ORIGINAL visible text, exactly as the existing evaluator measures it.
    let precomposed = "\u{1D11E} CAF\u{c9} note";
    let composed_rank = rank_block_text(&plan, branch, precomposed)
        .expect("the precomposed spelling matches the same needle");
    assert_eq!(
        decode_rank_key(&composed_rank.order_key()),
        (ObjectiveMatchClass::Substring.rank(), true, 3, 12, 1)
    );
    assert!(
        composed_rank.order_key() < rank.order_key(),
        "the shorter original text is the better tuple"
    );
    let unaccented_rank = rank_block_text(&plan, branch, "\u{1D11E} cafe note")
        .expect("A6 removes Mn accents from both spellings");
    assert_eq!(
        decode_rank_key(&unaccented_rank.order_key()),
        (ObjectiveMatchClass::Substring.rank(), true, 3, 12, 1)
    );
    assert_eq!(unaccented_rank.order_key(), composed_rank.order_key());
}

/// Selection is rank-only: it constructs no match evidence. The optional
/// accessor reproduces the existing evaluator's spans exactly, including
/// its BEST-branch OR choice, which is not the membership evaluator's
/// first-branch choice.
#[test]
fn rank_only_selection_builds_no_evidence_while_the_accessor_reproduces_it() {
    let plan = QueryPlan::friendly("zzz OR ready", 8, 8);
    let branch = block_branch(&plan).expect("an OR query plans a block branch");
    let text = "ready and zzz";

    let _ = take_block_evidence_evaluations();
    TEXT_EVIDENCE_EVALUATIONS.with(|count| count.set(0));
    let rank = rank_block_text(&plan, branch, text).expect("both OR arms match");
    TEXT_EVIDENCE_EVALUATIONS.with(|count| {
        assert_eq!(count.get(), 0, "selection must not construct text evidence");
    });
    assert_eq!(
        take_block_evidence_evaluations(),
        0,
        "rank-only selection must not run the evidence evaluator"
    );
    assert_eq!(rank.match_class(), ObjectiveMatchClass::Prefix);

    let evidence =
        admitted_block_evidence(&plan, branch, text).expect("the admitted row has a reason");
    assert_eq!(take_block_evidence_evaluations(), 1);
    TEXT_EVIDENCE_EVALUATIONS.with(|count| {
        assert!(
            count.get() > 0,
            "the evidence counter must observe the matcher"
        );
    });
    let reference =
        eval_ranked_block_expr(&plan, &branch.predicate, text, &canonical_fold(text)).unwrap();
    assert_eq!(evidence, reference.evidence);
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].spans, vec![MatchSpan { start: 0, end: 5 }]);
    assert_eq!(evidence[0].mode, TextMatchMode::Contains);

    // Best branch, not first branch.
    let first_branch = eval_expr(&plan, &branch.predicate, TextField::VisibleContent, text)
        .expect("membership evaluator also admits");
    assert_ne!(evidence[0].clause_id, first_branch.evidence[0].clause_id);
}

/// Boolean membership is preserved verbatim, including a satisfied
/// negation admitting on the neutral tuple with no evidence at all. Page
/// branches are deliberately out of scope: page-name/alias ranking is not
/// covered by these five block components.
#[test]
fn rank_bridge_preserves_neutral_negation_and_stays_out_of_page_ranking() {
    let plan = QueryPlan::friendly("draft", 8, 8);
    let source = block_branch(&plan).expect("a bare term plans a block branch");
    let negated = QueryBranch {
        target: QueryTarget::Blocks,
        predicate: QueryExpr::Not(Box::new(source.predicate.clone())),
        limit: source.limit,
    };
    let rank = rank_block_text(&plan, &negated, "ship it").expect("a satisfied NOT admits");
    assert_eq!(
        decode_rank_key(&rank.order_key()),
        (ObjectiveMatchClass::Exact.rank(), true, 0, 0, 0)
    );
    assert_eq!(rank.match_class(), ObjectiveMatchClass::Exact);
    assert_eq!(
        admitted_block_evidence(&plan, &negated, "ship it"),
        Some(Vec::new()),
        "a successful negation contributes no positive evidence"
    );
    assert!(rank_block_text(&plan, &negated, "draft one").is_none());

    let pages = plan
        .branches
        .iter()
        .find(|branch| branch.target == QueryTarget::Pages)
        .expect("friendly plans a page branch");
    assert!(rank_block_text(&plan, pages, "draft").is_none());
    assert!(admitted_block_evidence(&plan, pages, "draft").is_none());
}

/// There is no second regex grammar and no literal fallback behind the
/// bridge: a clause the plan never compiled matches nothing, and an
/// unparseable regex never reaches a branch at all.
#[test]
fn rank_bridge_has_no_fallback_for_an_uncompiled_regex_clause() {
    assert!(QueryPlan::friendly("/(unclosed/", 8, 8).branches.is_empty());
    let plan = QueryPlan::friendly("/A[BC]+/", 8, 8);
    let branch = block_branch(&plan).expect("a valid regex plans a block branch");
    assert!(rank_block_text(&plan, branch, "regex ABC").is_some());

    let pred = match &branch.predicate {
        QueryExpr::Text(pred) => pred.clone(),
        other => panic!("a single regex term compiles to one text clause, got {other:?}"),
    };
    let uncompiled = QueryBranch {
        target: QueryTarget::Blocks,
        predicate: QueryExpr::Text(TextPredicate {
            clause_id: pred.clause_id.wrapping_add(1_000),
            ..pred
        }),
        limit: branch.limit,
    };
    assert!(rank_block_text(&plan, &uncompiled, "regex ABC").is_none());
    assert!(admitted_block_evidence(&plan, &uncompiled, "regex ABC").is_none());
}
