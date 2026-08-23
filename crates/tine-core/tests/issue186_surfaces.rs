//! GH #186 (follow-up): the `医保`-style report that Ctrl+K finds more results
//! than the `#`/`[[`/`((` autocomplete surfaces. This is a PROBE harness to
//! establish, on a synthetic CJK-heavy graph, whether valid candidates are
//! genuinely dropped by any surface — or whether each surface simply searches
//! its own designed entity pool/window.

use std::fs;
use tine_core::model::Graph;
use tine_core::query_plan::{QueryHit, QueryPlan};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tine-issue186-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(dir.join("pages")).unwrap();
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("logseq")).unwrap();
    dir
}

#[test]
fn page_and_block_surfaces_share_candidates_within_designed_windows() {
    let dir = scratch("yibao");
    // Pages: exact, prefix matches, middle matches, distractors sharing one char.
    fs::write(dir.join("pages").join("医保.md"), "- the exact page\n").unwrap();
    for i in 0..30 {
        fs::write(
            dir.join("pages").join(format!("医保目录第{i}批.md")),
            "- prefix family\n",
        )
        .unwrap();
    }
    fs::create_dir_all(dir.join("pages").join("数据")).unwrap();
    for i in 0..30 {
        fs::write(
            dir.join("pages")
                .join("数据")
                .join(format!("医保 安全 规范 {i}.md")),
            "- middle family\n",
        )
        .unwrap();
    }
    for i in 0..90 {
        fs::write(
            dir.join("pages").join(format!("医疗保险{i}号.md")),
            "- distractor sharing first char only\n",
        )
        .unwrap();
    }
    // Genuine pages whose name never mentions 医保 but whose CONTENT does —
    // the `((`/Ctrl+K block layer must see them, page layers must ignore them.
    for i in 0..12 {
        fs::write(
            dir.join("pages").join(format!("报销流程{i}.md")),
            "- 医保 报销流程需要哪些材料\n- ordinary block\n",
        )
        .unwrap();
    }
    let graph = Graph::open(&dir);
    graph.warm_cache();

    let qs = graph.quick_switch("医保", 100);
    let qs_names: Vec<&str> = qs.iter().map(|p| p.name.as_str()).collect();
    eprintln!("QUICK_SWITCH count={}", qs_names.len());
    eprintln!("QS first 12: {:?}", &qs_names[..qs_names.len().min(12)]);

    let friendly = graph.run_graph_search("医保", 100, 100, false);
    let page_hits: Vec<&str> = friendly
        .hits
        .iter()
        .filter_map(|h| match h {
            QueryHit::Page { page, .. } => Some(page.name.as_str()),
            _ => None,
        })
        .collect();
    let block_hits = friendly
        .hits
        .iter()
        .filter(|h| matches!(h, QueryHit::Block { .. }))
        .count();
    eprintln!(
        "FRIENDLY pages={} blocks={} has_more={:?}",
        page_hits.len(),
        block_hits,
        friendly.has_more
    );
    eprintln!(
        "FR page first 12: {:?}",
        &page_hits[..page_hits.len().min(12)]
    );

    let literal_blocks = QueryPlan::block_search_literal("医保", 20).execute(&graph, || false);
    eprintln!(
        "LITERAL-BLOCK-PICKER hits={} has_more={:?}",
        literal_blocks.hits.len(),
        literal_blocks.has_more
    );

    // The core contract under test: every 医保-PREFIX page is a top-class match
    // and must be present in BOTH page surfaces with the SAME top order (the
    // shared executor + deterministic ties means one pool).
    for must in ["医保", "医保目录第0批", "医保目录第29批"] {
        assert!(qs_names.contains(&must), "quick_switch must contain {must}");
        assert!(
            page_hits.contains(&must),
            "Ctrl+K page hits must contain {must}"
        );
    }
    assert_eq!(
        &qs_names[..12],
        &page_hits[..12],
        "page surfaces share the same top ordering for the same query"
    );
    // Blocks: both block surfaces find all 12 医保 content blocks (the picker
    // returns every eligible match when it fits its top-20 window).
    assert_eq!(block_hits, 12, "Ctrl+K must find the 12 content blocks");
    assert_eq!(
        literal_blocks.hits.len(),
        12,
        "the (( picker returns every eligible match within its window"
    );
    // Same-graph truncation is SIGNALLED (only place it can be): Ctrl+K's
    // has_more.pages is true here because the eligible page pool (163) exceeds
    // the 100 window — this is the designed windowing, and it is honest there.
    assert!(
        friendly.has_more.pages,
        "Ctrl+K signals page-window truncation on this pool"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn literal_autocomplete_is_stable_across_character_swaps_and_dsl_tokens() {
    // Reproduction of the reporter's character-swap ladder on current code:
    // otherwise identical strings differing by one character must not flip
    // between present/absent, and DSL-token characters stay literal.
    let dir = scratch("swap-ladder");
    for name in [
        "ORb-target-page",
        "医保abc目标",
        "医保abd目标",
        "医保x目标",
        "OR 专题",
    ] {
        fs::write(
            dir.join("pages").join(format!("{name}.md")),
            "- swap ladder fixture pages\n",
        )
        .unwrap();
    }
    // Content blocks for the (( picker over the same token set.
    fs::write(dir.join("pages").join("generic blocks.md"), {
        let mut lines = String::new();
        for l in [
            "ORb target block",
            "医保abc target block",
            "医保abd target block",
        ] {
            lines.push_str(&format!("- {l}\n"));
        }
        lines
    })
    .unwrap();
    let graph = Graph::open(&dir);
    graph.warm_cache();

    for (query, want) in [
        ("O", "ORb-target-page"),
        ("OR", "ORb-target-page"),
        ("ORb", "ORb-target-page"),
        ("医保a", "医保abc目标"),
        ("医保ab", "医保abc目标"),
        ("医保abc", "医保abc目标"),
        ("医保abd", "医保abd目标"),
    ] {
        let entries = graph.quick_switch(query, 100);
        let names: Vec<&str> = entries.iter().map(|p| p.name.as_str()).collect();
        assert!(
            names.contains(&want),
            "quick_switch({query:?}) must contain {want:?}: {names:?}"
        );
    }
    // DSL tokens stay literal in the autocomplete lanes: bare uppercase `OR`
    // must match `ORb-target-page` AND `OR 专题`, never collapse to the
    // empty-matcher fallback pool. And the `((` picker treats it literally too.
    let or_entries = graph.quick_switch("OR", 100);
    let or_names: Vec<&str> = or_entries.iter().map(|p| p.name.as_str()).collect();
    assert!(or_names.contains(&"ORb-target-page") && or_names.contains(&"OR 专题"));
    let or_blocks = QueryPlan::block_search_literal("OR", 20).execute(&graph, || false);
    assert!(
        or_blocks
            .hits
            .iter()
            .any(|h| matches!(h, QueryHit::Block { display_text, .. } if display_text.contains("ORb target"))),
        "(( picker treats 'OR' literally"
    );

    let _ = fs::remove_dir_all(&dir);
}
