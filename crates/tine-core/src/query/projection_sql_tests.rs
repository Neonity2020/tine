//! The statement census: every SQL shape tine-core sends through the
//! projection door is exercised here against a projection built by the
//! current tine-storage pin, and the set of shapes is blessed in
//! `projection_statement_census.txt`. A renamed or dropped column fails this
//! test; a new or changed statement changes the blessed file, so the diff
//! shows exactly which SQL the app now sends.

use super::census;
use crate::model::Graph;
use crate::query::ir::FriendlyPageMatchScope;
use crate::query::{BacklinkFilterTarget, QueryExportSpec};
use crate::query_plan::FriendlyDisplayOptions;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const BLESSED: &str = "projection_statement_census.txt";
const BLESS_ENV: &str = "TINE_BLESS_PROJECTION_STATEMENTS";

fn blessed_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/query")
        .join(BLESSED)
}

fn write_census_corpus(root: &Path) {
    std::fs::create_dir_all(root.join("pages")).unwrap();
    std::fs::create_dir_all(root.join("journals")).unwrap();
    std::fs::write(
        root.join("pages/Alpha Book.md"),
        "type:: book\n\
         tags:: reading, fiction\n\n\
         - TODO alpha parent #reading [[Beta]]\n\
         \x20 id:: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
         \x20 template:: Census\n\
         \x20 SCHEDULED: <2026-09-20 Sun>\n\
         \t- alpha child\n\
         \t  prop:: value\n\
         \t\t- alpha grandchild [[Gamma]]\n\
         \t- alpha sibling after the exported subtree\n\
         - DONE alpha done task\n\
         - census:: one\n\
         - a plain block mentioning beta\n\
         - plain mentions go and ox\n",
    )
    .unwrap();
    std::fs::write(
        root.join("pages/Beta.md"),
        "alias:: bee\n\n- beta block referencing [[Alpha Book]] ((aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee))\n- another #fiction alpha\n",
    )
    .unwrap();
    std::fs::write(root.join("pages/Go.md"), "alias:: ox\n\n- short target\n").unwrap();
    std::fs::write(root.join("pages/Gamma.md"), "- gamma alpha\n").unwrap();
    std::fs::write(root.join("pages/Alpha%2FChild.md"), "- namespaced alpha\n").unwrap();
    std::fs::write(
        root.join("journals/2026_09_18.md"),
        "- journal alpha entry\n- LATER journal task\n",
    )
    .unwrap();
}

fn wait_ready(graph: &Graph) {
    let started = Instant::now();
    while !graph.direct_projection_ready_test() {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "projection did not converge for the statement census"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn exercise_every_surface(graph: &Graph) {
    graph.page_icons(&["Alpha Book".into(), "bee".into()]);
    graph.resolve_blocks(&["aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()]);
    graph.preview_block("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", 8);
    graph.block_referrers_bounded("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", 10, 10000);
    graph.templates();
    graph.journal_content_days();
    // The page list of a graph that never parsed: the index inventory, with
    // each journal's day from its row.
    graph.forget_page_list_test();
    assert!(!graph.list_pages().is_empty());
    // Friendly search: names (default), content and both; block hits with
    // ancestors (breadcrumbs) and the trigram-driven candidate path.
    graph
        .run_graph_search_latest("census", "alpha", 12, 12, true)
        .expect("friendly names");
    for scope in [
        FriendlyPageMatchScope::Content,
        FriendlyPageMatchScope::Both,
    ] {
        graph
            .run_graph_search_latest_displayed(
                "census",
                "alpha",
                12,
                12,
                None,
                false,
                FriendlyDisplayOptions {
                    page_match_scope: Some(scope),
                    ..FriendlyDisplayOptions::default()
                },
            )
            .expect("friendly scoped");
    }
    // Explicit Ctrl-K production routes: one indexed needle, one short scan,
    // and page-by-content under the interactive verified window.
    for source in ["alpha", "al"] {
        graph
            .run_graph_search_displayed_for(
                source,
                12,
                12,
                None,
                false,
                FriendlyDisplayOptions::default(),
                crate::query_plan::FriendlyConsumer::CtrlK,
            )
            .expect("interactive Ctrl-K");
    }
    graph
        .run_graph_search_displayed_for(
            "alpha",
            12,
            12,
            None,
            false,
            FriendlyDisplayOptions {
                page_match_scope: Some(FriendlyPageMatchScope::Content),
                ..FriendlyDisplayOptions::default()
            },
            crate::query_plan::FriendlyConsumer::CtrlK,
        )
        .expect("interactive page-by-content");
    let scoped = graph
        .run_graph_search_displayed_for(
            "alpha",
            12,
            12,
            Some(crate::query_plan::QueryPageScope {
                name: "Alpha Book".into(),
                page_kind: crate::model::PageKind::Page,
                path: Some("pages/Alpha Book.md".into()),
            }),
            false,
            FriendlyDisplayOptions::default(),
            crate::query_plan::FriendlyConsumer::CtrlK,
        )
        .expect("scoped interactive Ctrl-K");
    assert!(
        !scoped.hits.is_empty(),
        "scoped interactive Ctrl-K answered nothing"
    );
    let alias_suggestions = graph.quick_switch("b", 12);
    assert!(
        alias_suggestions
            .iter()
            .any(|entry| entry.name == "bee" && entry.rel_path == "pages/Beta.md"),
        "ready quick-switch must execute the authored-alias suggestion route: {alias_suggestions:?}"
    );
    // Macro queries, one per lowering family the compiler emits, hydrated
    // through the results reader (descriptor + payload batches).
    for query in [
        "(task TODO DONE)",
        "(property prop value)",
        "(page-property type book)",
        "(page-tags reading)",
        "[[Beta]]",
        "(between -30d +30d)",
        "(namespace Alpha)",
        "(and (task LATER) (between -30d +30d))",
        "\"alpha\"",
        "(all-page-tags)",
    ] {
        let groups = graph
            .run_query(query)
            .unwrap_or_else(|error| panic!("query {query}: {error:?}"));
        assert!(
            !groups.is_empty(),
            "census query {query} answered nothing; the fixture no longer exercises it"
        );
    }
    // The public IR route ({{query}} rendering): page rows hydrate page
    // properties; block rows share the block reader.
    for source in ["(all-page-tags)", "(task TODO)"] {
        let (query, view) = crate::query::parse_query_text(
            source,
            crate::query::QueryDialect::Og,
            crate::date::JournalDate::today(),
        );
        let result = crate::query::run_query_result_ir(
            graph,
            &query,
            &view,
            crate::query::ir::Bounds {
                max_rows: 100,
                max_bytes: 1 << 20,
            },
            &crate::query::ir::ExecutionContext::default(),
        )
        .unwrap_or_else(|error| panic!("IR query {source}: {error:?}"));
        assert!(
            result.total > 0,
            "census IR query {source} answered nothing; the fixture no longer exercises it"
        );
    }
    let filter_targets = graph
        .backlinks("Beta")
        .iter()
        .flat_map(|group| {
            group.blocks.iter().map(|block| BacklinkFilterTarget {
                page: group.page.clone(),
                kind: group.kind,
                block_id: block.id.clone(),
            })
        })
        .collect::<Vec<_>>();
    crate::query::backlink_filter_context(graph, "Beta", &filter_targets, "alpha")
        .expect("backlink filter context");
    crate::query::backlink_filter_context(graph, "2026-09-18", &[], "")
        .expect("journal backlink filter context");
    let interactive_indexed = graph
        .unlinked_refs_bounded_indexed("Beta", 100, 1 << 20)
        .expect("windowed unlinked narrowing");
    assert!(
        interactive_indexed.total > 0,
        "interactive indexed unlinked narrowing answered nothing"
    );
    // These equivalent spellings deliberately use distinct memo keys: each
    // public route must execute rather than borrowing another route's result.
    let exhaustive_indexed = graph.unlinked_refs_bounded("bee", 100, 1 << 20);
    assert!(
        exhaustive_indexed.total > 0,
        "exhaustive indexed unlinked narrowing answered nothing"
    );
    let interactive_scan = graph
        .unlinked_refs_bounded_indexed("Go", 100, 1 << 20)
        .expect("windowed short-scan unlinked narrowing");
    assert!(
        interactive_scan.total > 0,
        "interactive short-scan unlinked narrowing answered nothing"
    );
    let exhaustive_scan = graph.unlinked_refs_bounded("ox", 100, 1 << 20);
    assert!(
        exhaustive_scan.total > 0,
        "exhaustive short-scan unlinked narrowing answered nothing"
    );
    // Live export: a top-level root and a nested root (the nested one runs the
    // boundary-parent check).
    let export = graph
        .export_query_subtrees(
            &[
                QueryExportSpec {
                    key: "top".into(),
                    query: "(task TODO)".into(),
                    advanced: false,
                    simple_dialect: None,
                    current_page: None,
                },
                QueryExportSpec {
                    key: "nested".into(),
                    query: "(property prop value)".into(),
                    advanced: false,
                    simple_dialect: None,
                    current_page: None,
                },
            ],
            4,
            16,
            256,
            1 << 20,
        )
        .expect("export");
    for result in &export.results {
        eprintln!(
            "export {}: shown={} total={} omitted_nodes={}",
            result.key, result.shown, result.total, result.omitted_nodes
        );
    }
    // Publication fingerprint.
    crate::publish::publish_graph(graph).expect("publish");
}

#[test]
fn every_projection_statement_shape_is_blessed() {
    let root = tempfile::tempdir().unwrap();
    write_census_corpus(root.path());
    let graph = Graph::open(root.path());
    graph
        .attach_direct_projection(root.path().join("private/projection.sqlite"))
        .unwrap();
    graph.warm_cache();
    wait_ready(&graph);
    exercise_every_surface(&graph);

    // A save after readiness that changes a property value patches the
    // registry from the projection (affected keys) and reads the page's
    // registry metadata on the delta apply.
    let entry = graph
        .list_pages()
        .into_iter()
        .find(|entry| entry.name == "Alpha Book")
        .expect("fixture page");
    let mut page = graph.load_page(&entry).unwrap();
    let baseline = page.rev.clone();
    let census_block = page
        .blocks
        .iter_mut()
        .find(|block| block.raw.contains("census:: one"))
        .expect("fixture census block");
    census_block.raw = "census:: two".into();
    graph.save_page(&page, baseline.as_deref()).unwrap();
    wait_ready(&graph);
    exercise_every_surface(&graph);
    // A fresh build carries the pages a snapshot could not read from the
    // image it replaces (GH #543, decision DK4).
    crate::direct_projection::carried_physical_pages_test(
        &root.path().join("private/projection.sqlite"),
        &std::sync::Arc::new(crate::config::ParseConfig::default()),
    )
    .expect("the census image's pages are carried");
    // The reconciler's launch survey compares the image's stored revisions
    // with the graph; the watcher asks whether the image holds pages under a
    // directory (GH #543).
    let projection = graph.direct_projection_test().unwrap();
    assert!(projection
        .stored_revisions()
        .is_some_and(|stored| !stored.is_empty()));
    assert_eq!(projection.holds_pages_under("pages/"), Some(true));
    // The background integrity check, owed after a reboot (design D1).
    assert!(projection.run_integrity_check_test());
    // A reopen serves the stored image during the launch check only when
    // every stored fact was written under the current configuration (D2).
    assert!(projection.stored_facts_are_test(&graph.config().parse_config()));

    let recorded: BTreeSet<String> = census::recorded().into_keys().collect();
    let assert_shape = |label: &str, required: &[&str], forbidden: &[&str]| {
        assert!(
            recorded.iter().any(|sql| {
                required.iter().all(|part| sql.contains(part))
                    && forbidden.iter().all(|part| !sql.contains(part))
            }),
            "statement census did not execute {label}"
        );
    };
    assert_shape(
        "survey stored revisions SQL",
        &["SELECT path, revision FROM direct_source_revisions"],
        &["WHERE"],
    );
    assert_shape(
        "image pages under a directory SQL",
        &["SELECT path FROM pages WHERE path >= ?"],
        &[],
    );
    let indexed_unlinked = [
        "SELECT path, result_id, entity_id, entity_type",
        "FROM (SELECT rowid FROM search_fts WHERE search_fts MATCH ? ORDER BY rowid DESC) c",
    ];
    assert_shape(
        "interactive indexed unlinked SQL",
        &[
            indexed_unlinked[0],
            indexed_unlinked[1],
            "tine_query_rank(entity_type",
            "LIMIT ?",
        ],
        &[],
    );
    assert_shape(
        "exhaustive indexed unlinked SQL",
        &indexed_unlinked,
        &["tine_query_rank(entity_type", "LIMIT ?"],
    );
    let scan_unlinked = [
        "SELECT p.path AS path, NULL AS result_id",
        "UNION ALL SELECT p.path, b.result_id",
    ];
    assert_shape(
        "interactive short-scan unlinked SQL",
        &[
            scan_unlinked[0],
            scan_unlinked[1],
            "tine_query_rank(0",
            "tine_query_rank(1",
            "LIMIT ?",
        ],
        &[],
    );
    assert_shape(
        "exhaustive short-scan unlinked SQL",
        &scan_unlinked,
        &["tine_query_rank(", "LIMIT ?"],
    );
    assert_shape(
        "authored-alias page-name suggestions SQL",
        &["PARTITION BY r.page_id, r.matched_text"],
        &[],
    );
    assert_shape(
        "scoped interactive block cursor SQL",
        &[
            "SELECT b.block_id, bt.content, p.path",
            "WHERE p.path = ?",
            "ORDER BY c.block_id DESC",
        ],
        &[],
    );
    let path = blessed_path();
    if std::env::var_os(BLESS_ENV).is_some() {
        let mut text = recorded.iter().cloned().collect::<Vec<_>>().join("\n");
        text.push('\n');
        std::fs::write(&path, text).unwrap();
    }
    let blessed: BTreeSet<String> = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    let missing = blessed.difference(&recorded).cloned().collect::<Vec<_>>();
    let extra = recorded.difference(&blessed).cloned().collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "I-11: blessed projection statements this census no longer sends \
         (a surface stopped being exercised, or its SQL changed shape — \
         re-bless with {BLESS_ENV}=1 after checking the diff):\n{}",
        missing.join("\n")
    );
    // nextest runs each test in its own process, so there the recorded set is
    // exactly this census; under plain `cargo test` other tests in the same
    // process may add shapes, and only the subset direction is checked.
    if std::env::var_os("NEXTEST").is_some() {
        assert!(
            extra.is_empty(),
            "I-11: projection statements sent but not blessed in {BLESSED} \
             (re-bless with {BLESS_ENV}=1 after checking the diff):\n{}",
            extra.join("\n")
        );
    }
    assert!(!blessed.is_empty(), "the blessed census is empty");
}
