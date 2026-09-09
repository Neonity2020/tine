use std::path::{Path, PathBuf};

use tine_storage::sqlite::PhysicalProjectionQuerySnapshot;

use super::{set_after_construction_hook, ExportExecutionInputs, PreparedExportBatch};
use crate::query::export_query_subtrees;
use crate::query::results::{BackendOrder, RecencyPage, ResultIdentity, ResultReadError};
use crate::query::sql::sql_gates_tests::{scratch, serialize, Corpus};
use crate::query::{QueryDialect, QueryExportBatch, QueryExportSpec};

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
