//! The statement census: every SQL shape tine-core sends through the
//! projection door is exercised here against a projection built by the
//! current tine-storage pin, and the set of shapes is blessed in
//! `projection_statement_census.txt`. A renamed or dropped column fails this
//! test; a new or changed statement changes the blessed file, so the diff
//! shows exactly which SQL the app now sends.

use super::census;
use crate::model::Graph;
use crate::query::ir::FriendlyPageMatchScope;
use crate::query::QueryExportSpec;
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
         \x20 SCHEDULED: <2026-09-20 Sun>\n\
         \t- alpha child\n\
         \t  prop:: value\n\
         \t\t- alpha grandchild [[Gamma]]\n\
         \t- alpha sibling after the exported subtree\n\
         - DONE alpha done task\n\
         - census:: one\n\
         - a plain block mentioning beta\n",
    )
    .unwrap();
    std::fs::write(
        root.join("pages/Beta.md"),
        "alias:: bee\n\n- beta block referencing [[Alpha Book]]\n- another #fiction alpha\n",
    )
    .unwrap();
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

    let recorded: BTreeSet<String> = census::recorded().into_keys().collect();
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
