use std::path::{Path, PathBuf};

use tine_storage::sqlite::PhysicalProjectionQuerySnapshot;

use super::{set_after_construction_hook, ExportExecutionInputs, PreparedExportBatch};
use crate::query::export_query_subtrees;
use crate::query::results::{BackendOrder, RecencyPage, ResultIdentity, ResultReadError};
use crate::query::sql::sql_gates_tests::{scratch, serialize, Corpus};
use crate::query::{QueryDialect, QueryExportBatch, QueryExportSpec};

#[cfg(test)]
fn print_snapshot(
    corpus: &Corpus,
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    source: &str,
) -> Result<String, crate::publish::PrintPreparationError> {
    let registry = corpus.graph.property_registry();
    let identity = ResultIdentity::Stored;
    let recency = |_page: RecencyPage<'_>| 0;
    let page_recency = crate::query::rank::PageRecencyPrograms::new(
        crate::query::rank::JournalRankInput::StoredDay,
        |_| 0,
        |_| 0,
    );
    let reader = crate::query::read_execute::SnapshotQueryReader::new(
        snapshot,
        crate::query::read_execute::SnapshotQueryInputs {
            registry: &registry,
            identity: &identity,
            order: BackendOrder::Direct,
            recency: &recency,
            page_recency: &page_recency,
            today: corpus.today(),
        },
    )?;
    crate::publish::page_print_html_document(
        &corpus.graph,
        "Print",
        &crate::doc::parse(source),
        crate::publish::PrintOpts::default(),
        &reader,
    )
}

#[test]
fn print_queries_share_one_snapshot_across_nested_and_repeated_macros() {
    let _serial = serialize();
    let root = scratch("q2-print-snapshot");
    write_corpus(&root);
    std::fs::write(
        root.join("pages/Nested.md"),
        "- TODO nested owner\n\t- {{tine-query prop('cost') = 2}}\n",
    )
    .unwrap();
    let corpus = Corpus::open(root, true);
    let path = copy_projection(&corpus, "print-snapshot");
    rusqlite::Connection::open(&path)
        .unwrap()
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
    let writer_path = path.clone();
    set_after_construction_hook(Box::new(move || {
        let writer = rusqlite::Connection::open(writer_path).unwrap();
        assert!(writer.execute("UPDATE block_text SET content = replace(content, 'alpha', 'omega') WHERE content LIKE '%alpha%'", []).unwrap() > 0);
    }));
    let html = print_snapshot(&corpus, &mut snapshot,
        "- {{query (and (task TODO) (page Alpha))}}\n- {{query (and (task TODO) (page Alpha))}}\n- {{query (page Nested)}}\n")
        .unwrap();
    assert_eq!(html.matches("alpha grandchild").count(), 2);
    assert!(!html.contains("omega grandchild"));
    assert!(
        html.contains("beta child"),
        "nested property queries share the captured registry"
    );
    snapshot.finish();
    let mut later = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
    assert!(print_snapshot(
        &corpus,
        &mut later,
        "- {{query (and (task TODO) (page Alpha))}}\n"
    )
    .unwrap()
    .contains("omega grandchild"));
}

#[test]
fn print_missing_payload_or_cancellation_returns_no_html() {
    let _serial = serialize();
    let root = scratch("q2-print-cancel");
    write_corpus(&root);
    let corpus = Corpus::open(root, true);
    let mut snapshot = corpus.snapshot();
    snapshot.cancellation().cancel();
    let result = print_snapshot(&corpus, &mut snapshot, "- {{query (task TODO)}}\n");
    assert!(result.is_err(), "cancelled Print must return no HTML");
    let mut snapshot = corpus.snapshot();
    let cancellation = snapshot.cancellation();
    set_after_construction_hook(Box::new(move || cancellation.cancel()));
    assert!(matches!(
        print_snapshot(&corpus, &mut snapshot, "- {{query (task TODO)}}\n"),
        Err(crate::publish::PrintPreparationError::Query(
            crate::query::QueryExecutionError::Cancelled
        ))
    ));
    let path = copy_projection(&corpus, "print-missing");
    {
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.pragma_update(None, "foreign_keys", false).unwrap();
        assert_eq!(
            writer
                .execute("DELETE FROM block_text WHERE content = 'alpha child'", [])
                .unwrap(),
            1
        );
    }
    let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
    assert!(matches!(
        print_snapshot(&corpus, &mut snapshot, "- {{query (task TODO)}}\n"),
        Err(crate::publish::PrintPreparationError::Query(
            crate::query::QueryExecutionError::Unavailable(_)
        ))
    ));
}

#[test]
fn print_selection_budget_exceeded_refuses_complete_document() {
    let _serial = serialize();
    let root = scratch("q2-print-selection-limit");
    std::fs::create_dir_all(root.join("pages")).unwrap();
    let content = (0..20_000)
        .map(|n| format!("- TODO row {n}\n"))
        .collect::<String>()
        + "- DONE boundary plus one\n";
    std::fs::write(root.join("pages/Tasks.md"), content).unwrap();
    let corpus = Corpus::open(root, true);
    let mut snapshot = corpus.snapshot();
    let boundary = print_snapshot(&corpus, &mut snapshot, "- {{query (task TODO)}}\n").unwrap();
    assert!(boundary.contains("row 19999"));
    crate::query::export_results::reset_export_subtree_census();
    let result = print_snapshot(
        &corpus,
        &mut snapshot,
        "- earlier body\n- {{query (or (task TODO) (task DONE))}}\n",
    );
    assert!(
        result.is_err(),
        "selection overflow must discard the complete document"
    );
    assert_eq!(result.unwrap_err().to_string(), "Couldn't prepare this page for PDF: a query exceeds the Print limit. Narrow the query and try again.");
    assert_eq!(
        crate::query::export_results::export_subtree_census(),
        Default::default(),
        "denied selection performs no subtree hydration"
    );
}

#[test]
fn print_does_not_inherit_copy_output_caps() {
    let _serial = serialize();
    let root = scratch("q2-print-output-caps");
    std::fs::create_dir_all(root.join("pages")).unwrap();
    let mut content = String::from("- TODO large root\n");
    for n in 0..2_001 {
        content.push_str(&format!("\t- descendant {n} {}\n", "x".repeat(4_200)));
    }
    for n in 0..51 {
        content.push_str(&format!("- DONE root {n}\n"));
    }
    std::fs::write(root.join("pages/Tasks.md"), content).unwrap();
    let corpus = Corpus::open(root, true);
    let mut snapshot = corpus.snapshot();
    let source = "- {{query (task DONE)}}\n".repeat(65) + "- {{query (task TODO)}}\n";
    let html = print_snapshot(&corpus, &mut snapshot, &source).unwrap();
    assert_eq!(html.matches("DONE</span>").count(), 65 * 51);
    assert!(html.contains("descendant 2000"));
    assert!(html.len() > 8 * 1024 * 1024);
}

#[test]
fn print_preserves_source_nesting_expansion_and_asset_budgets() {
    let _serial = serialize();
    let root = scratch("q2-print-source-budget");
    write_corpus(&root);
    let corpus = Corpus::open(root, true);
    let mut snapshot = corpus.snapshot();
    let source = format!(
        "- {{{{query {}}}}}\n",
        "x".repeat(crate::query::QUERY_SOURCE_MAX_BYTES + 1)
    );
    assert!(
        print_snapshot(&corpus, &mut snapshot, &source).is_err(),
        "source overflow rejects Print"
    );
    let at_source = format!(
        "- {{{{query {}(task TODO)}}}}\n",
        " ".repeat(crate::query::QUERY_SOURCE_MAX_BYTES - "(task TODO)".len())
    );
    assert!(print_snapshot(&corpus, &mut snapshot, &at_source)
        .unwrap()
        .contains("alpha child"));
    let nested = |depth: usize| {
        format!(
            "- {{{{query {}(task TODO){}}}}}\n",
            "(and ".repeat(depth - 1),
            ")".repeat(depth - 1)
        )
    };
    assert!(print_snapshot(&corpus, &mut snapshot, &nested(64))
        .unwrap()
        .contains("alpha child"));
    assert!(print_snapshot(&corpus, &mut snapshot, &nested(65)).is_err());
}

#[test]
fn print_query_sheets_preserve_org_format_and_physical_identity() {
    let _serial = serialize();
    let root = scratch("q2-print-org");
    std::fs::create_dir_all(root.join("pages")).unwrap();
    std::fs::create_dir_all(root.join("journals")).unwrap();
    let id = "622601de-5df1-4eef-9c7d-2cf997ea0002";
    std::fs::write(
        root.join("pages/Beta.ORG"),
        format!("* TODO beta root\n:PROPERTIES:\n:id: {id}\n:cost: 2\n:END:\n** beta child\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("pages/Decoy.md"),
        format!("title:: Beta\n- DONE decoy root\n  id:: {id}\n\t- wrong physical child\n"),
    )
    .unwrap();
    let corpus = Corpus::open(root, true);
    let mut snapshot = corpus.snapshot();
    let html = print_snapshot(
        &corpus,
        &mut snapshot,
        "- {{query (and (task TODO) (page Beta))}}\n  tine.view:: table\n",
    )
    .unwrap();
    assert!(html.contains("beta root") && html.contains("beta child"));
    assert!(html.contains("<table class=\"sheet-table\">"));
    assert!(!html.contains("wrong physical child"));
    assert!(
        !html.contains(":PROPERTIES:"),
        "uppercase Org format must survive sheet conversion"
    );
    let board = print_snapshot(
        &corpus,
        &mut snapshot,
        "- {{query (and (task TODO) (page Beta))}}\n  tine.view:: board\n",
    )
    .unwrap();
    assert!(board.contains("beta root") && board.contains("beta child"));
    assert!(!board.contains("wrong physical child"));
}

#[test]
fn print_preserves_private_scope_and_unsupported_surfaces() {
    let _serial = serialize();
    let root = scratch("q2-print-private");
    write_corpus(&root);
    let corpus = Corpus::open(root, true);
    let mut snapshot = corpus.snapshot();
    let html = print_snapshot(
        &corpus,
        &mut snapshot,
        "- {{query (task TODO)}}\n- {{tine-query @page and [[Alpha]]}}\n",
    )
    .unwrap();
    assert!(
        html.contains("alpha grandchild"),
        "personal Print includes private query output"
    );
    assert!(!html.contains("non-public pages omitted"));
    assert!(!html.contains("Query results are unavailable for this render."));
}

#[test]
fn print_direct_queries_use_current_sqlite_subtrees() {
    let _serial = serialize();
    let root = scratch("q2-print-direct-sqlite");
    write_corpus(&root);
    std::fs::write(root.join("pages/Print.md"), "- {{query (task TODO)}}\n").unwrap();
    let corpus = Corpus::open(root, true);
    std::fs::write(corpus.root.join("pages/Focus A.md"), "- poisoned source\n").unwrap();
    let parses = corpus.graph.page_build_parses_test();
    let html = corpus
        .graph
        .page_print_html("Print", crate::publish::PrintOpts::default())
        .unwrap()
        .unwrap();
    assert!(html.contains("alpha grandchild"));
    assert!(!html.contains("poisoned source"));
    assert_eq!(corpus.graph.page_build_parses_test(), parses);
}

#[derive(Clone, Copy)]
struct Caps {
    queries: usize,
    roots: usize,
    nodes: usize,
    bytes: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            queries: 64,
            roots: 64,
            nodes: 4_096,
            bytes: 1024 * 1024,
        }
    }
}

fn spec(key: &str, query: &str) -> QueryExportSpec {
    QueryExportSpec {
        key: key.to_string(),
        query: query.to_string(),
        advanced: false,
        simple_dialect: None,
        current_page: None,
    }
}

fn tql_spec(key: &str, query: &str) -> QueryExportSpec {
    QueryExportSpec {
        simple_dialect: Some(QueryDialect::Tql),
        ..spec(key, query)
    }
}

fn advanced_spec(key: &str, query: &str, current_page: Option<&str>) -> QueryExportSpec {
    QueryExportSpec {
        key: key.to_string(),
        query: query.to_string(),
        advanced: true,
        simple_dialect: None,
        current_page: current_page.map(str::to_string),
    }
}

fn write_corpus(root: &Path) {
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    std::fs::create_dir_all(root.join("journals")).expect("journals");
    std::fs::write(
        root.join("pages/Alpha.md"),
        "- TODO alpha root\n  cost:: 10\n\t- alpha child\n\t\t- alpha grandchild\n\
         - TODO alpha second\n\t- second child\n\
         - DONE excluded alpha\n",
    )
    .expect("Alpha");
    std::fs::write(
        root.join("pages/Beta.org"),
        "* TODO beta root\n:PROPERTIES:\n:cost: 2\n:END:\n** beta child\n\
         * DONE excluded beta\n",
    )
    .expect("Beta");
    std::fs::write(
        root.join("pages/Focus A.md"),
        "- focus owner\n\t- focus child\n",
    )
    .expect("Focus A");
    std::fs::write(
        root.join("pages/Referrer.md"),
        "- TODO points to [[Focus A]]\n\t- reference child\n",
    )
    .expect("Referrer");
    let journal = crate::date::JournalDate::today().add_days(-3).file_stem();
    std::fs::write(
        root.join(format!("journals/{journal}.md")),
        "- TODO recent journal root\n\t- journal child\n",
    )
    .expect("recent journal");
}

fn read_export(
    corpus: &Corpus,
    specs: &[QueryExportSpec],
    caps: Caps,
    order: BackendOrder,
) -> Result<QueryExportBatch, ResultReadError> {
    let prepared = PreparedExportBatch::prepare(specs, caps.queries, corpus.today());
    if let Some(answer) = prepared.all_refused_result(caps.roots) {
        return Ok(answer);
    }
    let registry = corpus.graph.property_registry();
    let recency = |_page: RecencyPage<'_>| 0;
    let identity = ResultIdentity::Stored;
    let mut snapshot = corpus.snapshot();
    let answer = prepared.execute(
        &mut snapshot,
        &ExportExecutionInputs {
            registry: &registry,
            identity: &identity,
            order,
            recency: &recency,
            max_roots: caps.roots,
            max_nodes: caps.nodes,
            max_bytes: caps.bytes,
        },
    );
    snapshot.finish();
    answer
}

fn walk_export(corpus: &Corpus, specs: &[QueryExportSpec], caps: Caps) -> QueryExportBatch {
    export_query_subtrees(
        &corpus.graph,
        specs,
        caps.queries,
        caps.roots,
        caps.nodes,
        caps.bytes,
    )
}

fn assert_same_batch(expected: &QueryExportBatch, actual: &QueryExportBatch) {
    assert_eq!(
        serde_json::to_value(actual).expect("read batch serializes"),
        serde_json::to_value(expected).expect("walk batch serializes")
    );
}

fn advanced_current_page_source() -> &'static str {
    r#"{:query [:find (pull ?b [*])
                :in $ ?current-page
                :where
                [?p :block/name ?current-page]
                [?b :block/refs ?p]]
        :inputs [:current-page]}"#
}

fn advanced_recent_source() -> &'static str {
    r#"[:find (pull ?b [*])
        :in $ ?start ?end
        :where (between ?b ?start ?end)]
       :inputs [:-7d :today]"#
}

fn unsupported_advanced_source() -> &'static str {
    r#"[:find (pull ?b [*])
        :where [?b :block/unknown-attribute "x"]]"#
}

#[test]
fn one_executor_matches_the_walk_for_og_tql_advanced_and_mixed_batches() {
    let _serial = serialize();
    let root = scratch("s3-common-export-mixed");
    write_corpus(&root);
    let corpus = Corpus::open(root, true);
    let specs = vec![
        spec("og", "(task TODO)"),
        tql_spec("tql", "task = 'TODO'"),
        advanced_spec("advanced", advanced_current_page_source(), Some("Focus A")),
        advanced_spec("relative-day", advanced_recent_source(), None),
        tql_spec("property", "prop('cost') is not null"),
        tql_spec("refused", "@block and ("),
    ];
    let prepared = PreparedExportBatch::prepare(&specs, 64, corpus.today());
    assert!(prepared.requires_registry());
    assert!(prepared.all_refused_result(64).is_none());
    let expected = walk_export(&corpus, &specs, Caps::default());
    let actual = read_export(&corpus, &specs, Caps::default(), BackendOrder::Direct)
        .expect("the common executor answers");
    assert_same_batch(&expected, &actual);
}

#[test]
fn refused_prefix_answers_without_projection_or_registry_and_preserves_macro_cap() {
    let oversized = "x".repeat(crate::query::QUERY_SOURCE_MAX_BYTES + 1);
    let specs = vec![
        tql_spec("invalid", &oversized),
        advanced_spec("unsupported", unsupported_advanced_source(), None),
        spec("not-evaluated", "(task TODO)"),
    ];
    let prepared = PreparedExportBatch::prepare(&specs, 2, crate::date::JournalDate::today());
    assert!(!prepared.requires_registry());
    let answer = prepared
        .all_refused_result(0)
        .expect("the bounded prefix is fully refused");
    assert_eq!(answer.omitted_queries, 1);
    assert_eq!(answer.results.len(), 2);
    assert_eq!(answer.results[0].key, "invalid");
    assert_eq!(answer.results[1].key, "unsupported");
    assert_eq!(answer.results[0].total, 0);
    assert_eq!(answer.results[0].shown, 0);
    assert!(answer.results[0].groups.is_empty());
}

#[test]
fn every_nonzero_budget_boundary_matches_the_existing_global_budget() {
    let _serial = serialize();
    let root = scratch("s3-common-export-budgets");
    write_corpus(&root);
    let corpus = Corpus::open(root, true);
    let specs = vec![
        spec("todo", "(task TODO)"),
        spec("done", "(task DONE)"),
        tql_spec("tql", "task = 'TODO'"),
    ];
    for caps in [
        Caps {
            queries: 2,
            ..Caps::default()
        },
        Caps {
            roots: 2,
            ..Caps::default()
        },
        Caps {
            nodes: 2,
            ..Caps::default()
        },
        Caps {
            bytes: 300,
            ..Caps::default()
        },
    ] {
        let expected = walk_export(&corpus, &specs, caps);
        let actual = read_export(&corpus, &specs, caps, BackendOrder::Direct)
            .expect("the bounded export answers");
        assert_same_batch(&expected, &actual);
    }
}

#[test]
fn all_zero_limits_keep_the_existing_clamps_and_complete_subtree_accounting() {
    let _serial = serialize();
    let root = scratch("s3-common-export-zero-caps");
    write_corpus(&root);
    let corpus = Corpus::open(root, true);
    let specs = vec![spec("first", "(task TODO)"), spec("omitted", "(task DONE)")];
    let caps = Caps {
        queries: 0,
        roots: 0,
        nodes: 0,
        bytes: 0,
    };
    let expected = walk_export(&corpus, &specs, caps);
    let actual = read_export(&corpus, &specs, caps, BackendOrder::Direct)
        .expect("zero limits are clamped by the shared owners");
    assert_same_batch(&expected, &actual);
}

#[test]
fn sort_sample_and_both_backend_orders_use_the_same_executor() {
    let _serial = serialize();
    let root = scratch("s3-common-export-orders");
    write_corpus(&root);
    let corpus = Corpus::open(root, true);
    let specs = vec![spec(
        "view",
        "(and (task TODO) (sort-by page desc) (sample 2))",
    )];
    let expected = walk_export(&corpus, &specs, Caps::default());
    for order in [BackendOrder::Direct, BackendOrder::Managed] {
        let actual = read_export(&corpus, &specs, Caps::default(), order)
            .expect("the selected ordering policy answers");
        assert_same_batch(&expected, &actual);
    }
}

#[test]
fn physical_locator_wins_when_a_property_id_collides_with_the_selected_root() {
    let _serial = serialize();
    let root = scratch("s3-common-export-collision");
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    std::fs::create_dir_all(root.join("journals")).expect("journals");
    let real = crate::model::doc_runtime_id_for_order("pages/Dup.md", "00000001")
        .expect("fixture structural id")
        .to_string();
    std::fs::write(
        root.join("pages/Dup.md"),
        format!(
            "- decoy parent\n  id:: {real}\n\t- decoy child\n\
             - TODO real target\n\t- the real child\n"
        ),
    )
    .expect("Dup");
    let corpus = Corpus::open(root, true);
    let specs = vec![spec("collision", "(task TODO)")];
    let answer = read_export(&corpus, &specs, Caps::default(), BackendOrder::Direct)
        .expect("the physical root answers");
    let root = &answer.results[0].groups[0].blocks[0];
    assert_eq!(root.raw, "TODO real target");
    assert_eq!(root.children.len(), 1);
    assert_eq!(root.children[0].raw, "the real child");
}

fn copy_projection(corpus: &Corpus, tag: &str) -> PathBuf {
    let destination = std::env::temp_dir().join(format!(
        "tine-s3-common-export-{tag}-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    let source = rusqlite::Connection::open_with_flags(
        corpus.projection_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("projection opens read-only");
    source
        .execute(
            "VACUUM INTO ?1",
            rusqlite::params![destination.to_string_lossy().as_ref()],
        )
        .expect("projection copies");
    destination
}

#[test]
fn missing_required_descendant_payload_fails_the_whole_batch() {
    let _serial = serialize();
    let root = scratch("s3-common-export-damage");
    write_corpus(&root);
    let corpus = Corpus::open(root, true);
    let path = copy_projection(&corpus, "damage");
    {
        let writer = rusqlite::Connection::open(&path).expect("copy opens writable");
        writer
            .pragma_update(None, "foreign_keys", false)
            .expect("foreign keys can be disabled on disposable test copy");
        let changed = writer
            .execute(
                "DELETE FROM block_text WHERE block_id = (
                     SELECT b.block_id FROM blocks b
                     JOIN block_text t ON t.block_id = b.block_id
                     WHERE t.content = 'alpha child'
                 )",
                [],
            )
            .expect("payload is removed");
        assert_eq!(changed, 1);
    }
    let specs = vec![spec("damaged", "(and (task TODO) (page Alpha))")];
    let prepared = PreparedExportBatch::prepare(&specs, 64, corpus.today());
    let registry = corpus.graph.property_registry();
    let identity = ResultIdentity::Stored;
    let recency = |_page: RecencyPage<'_>| 0;
    let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(()))
        .expect("damaged projection still opens");
    let answer = prepared.execute(
        &mut snapshot,
        &ExportExecutionInputs {
            registry: &registry,
            identity: &identity,
            order: BackendOrder::Direct,
            recency: &recency,
            max_roots: 64,
            max_nodes: 4_096,
            max_bytes: 1024 * 1024,
        },
    );
    snapshot.finish();
    let _ = std::fs::remove_file(path);
    assert!(matches!(answer, Err(ResultReadError::Corrupt(_))));
}

#[test]
fn cancellation_after_complete_construction_returns_no_partial_batch() {
    let _serial = serialize();
    let root = scratch("s3-common-export-cancel");
    write_corpus(&root);
    let corpus = Corpus::open(root, true);
    let specs = vec![spec("cancel", "(task TODO)")];
    let prepared = PreparedExportBatch::prepare(&specs, 64, corpus.today());
    let registry = corpus.graph.property_registry();
    let identity = ResultIdentity::Stored;
    let recency = |_page: RecencyPage<'_>| 0;
    let mut snapshot = corpus.snapshot();
    let cancellation = snapshot.cancellation();
    set_after_construction_hook(Box::new(move || cancellation.cancel()));
    let answer = prepared.execute(
        &mut snapshot,
        &ExportExecutionInputs {
            registry: &registry,
            identity: &identity,
            order: BackendOrder::Direct,
            recency: &recency,
            max_roots: 64,
            max_nodes: 4_096,
            max_bytes: 1024 * 1024,
        },
    );
    snapshot.finish();
    assert!(matches!(answer, Err(ResultReadError::Cancelled)));
}

#[test]
fn executor_source_owns_no_snapshot_open_graph_walk_or_second_export_algorithm() {
    let source = include_str!("export_execute.rs");
    assert!(!source.contains("open_direct"));
    assert!(!source.contains("open_managed"));
    assert!(!source.contains("Graph::"));
    assert!(!source.contains("Document"));
    assert_eq!(source.matches("select_located_export_queries(").count(), 1);
    assert_eq!(source.matches("hydrate_located_export_queries(").count(), 1);
    assert_eq!(source.matches("read_located_results(").count(), 1);
}
