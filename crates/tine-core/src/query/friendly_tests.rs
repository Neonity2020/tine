//! Source-only gates for the Friendly projection reader. The implementation
//! packet intentionally does not execute them; the manager runs them after
//! wiring the public adapters on the combined exact head.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use tine_storage::sqlite::PhysicalProjectionQuerySnapshot;

use super::*;
use crate::model::PageKind;
use crate::query::results::{
    reset_result_read_census, result_read_census, set_before_payload_batch_hook,
};
use crate::query::sql::sql_gates_tests::{scratch, serialize, Corpus};
use crate::query_plan::{QueryPageScope, QueryTarget};

fn write_friendly_corpus(root: &Path) {
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    std::fs::write(
        root.join("pages/Owner.md"),
        "alias:: alpha, foo OR bar, foo first, foo later\n\n\
         - alpha parent\n\
         \t- alpha child\n\
         \t\t- alpha grandchild\n\
         - a Café block and [[Ghost Page]]\n",
    )
    .expect("owner page");
    std::fs::write(
        root.join("pages/Other.md"),
        "- alpha on another page and [[ghost page]]\n",
    )
    .expect("other page");
    std::fs::write(root.join("pages/Café.md"), "- decomposed Cafe\u{301}\n").expect("unicode page");
}

fn read(
    corpus: &Corpus,
    plan: &QueryPlan,
    identity: &ResultIdentity,
) -> Result<QueryExecution, ResultReadError> {
    let mut snapshot = corpus.snapshot();
    let answer = read_friendly_results(
        &mut snapshot,
        &FriendlyReadInputs {
            plan,
            graph_root: &corpus.root,
            identity,
            explain: true,
            lane: None,
        },
    );
    snapshot.finish();
    answer
}

fn assert_same_execution(expected: &QueryExecution, actual: &QueryExecution) {
    assert_eq!(
        serde_json::to_value(actual).expect("database answer serializes"),
        serde_json::to_value(expected).expect("walk answer serializes")
    );
}

#[test]
fn friendly_main_reader_matches_the_independent_walk_for_rank_and_identity_shapes() {
    let _serial = serialize();
    let root = scratch("friendly-main-parity");
    write_friendly_corpus(&root);
    let corpus = Corpus::open(root, true);
    let plans = [
        QueryPlan::friendly("foo OR bar", 8, 8),
        QueryPlan::friendly("foo", 8, 8),
        QueryPlan::friendly("ghost page", 8, 8),
        QueryPlan::friendly("café", 8, 8),
        QueryPlan::friendly("alpha -another", 8, 8),
        QueryPlan::friendly("/alpha (parent|child)/", 8, 8),
        QueryPlan::friendly("alpha OR Café", 8, 8),
        QueryPlan::friendly("-draft", 8, 8),
    ];
    let structural = ResultIdentity::DirectStructural {
        session_pages: Arc::new(HashSet::new()),
        all_session: false,
    };
    for plan in &plans {
        let expected = plan.execute_with_explain(&corpus.graph, || false, true);
        let stored =
            read(&corpus, plan, &ResultIdentity::Stored).expect("the Stored Friendly read answers");
        assert_same_execution(&expected, &stored);
        let structural =
            read(&corpus, plan, &structural).expect("the structural Friendly read answers");
        assert_same_execution(&expected, &structural);
    }
}

#[test]
fn sections_limit_independently_pages_precede_blocks_and_children_are_not_suppressed() {
    let _serial = serialize();
    let root = scratch("friendly-main-limits");
    write_friendly_corpus(&root);
    let corpus = Corpus::open(root, true);

    let one = read(
        &corpus,
        &QueryPlan::friendly("alpha", 1, 1),
        &ResultIdentity::Stored,
    )
    .expect("limited read");
    assert!(matches!(one.hits.first(), Some(QueryHit::Page { .. })));
    assert!(matches!(one.hits.get(1), Some(QueryHit::Block { .. })));
    assert!(one.has_more.blocks);

    let blocks = read(
        &corpus,
        &QueryPlan::friendly("alpha", 0, 16),
        &ResultIdentity::Stored,
    )
    .expect("block-only limited read");
    assert!(!blocks.has_more.pages);
    assert_same_execution(
        &QueryPlan::friendly("alpha", 0, 16).execute_with_explain(&corpus.graph, || false, true),
        &blocks,
    );
    let owner = blocks
        .hits
        .iter()
        .filter_map(|hit| match hit {
            QueryHit::Block {
                path,
                display_text,
                block,
                ..
            } if path == "pages/Owner.md" => {
                Some((display_text.as_str(), block.breadcrumb.as_slice()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        owner.len(),
        3,
        "matching parent, child and grandchild survive"
    );
    // Existing Friendly ranking prefers the shorter equal-class child text;
    // traversal order is only a tie breaker after the complete rank key.
    assert_eq!(owner[0], ("alpha child", &["alpha parent".to_string()][..]));
    assert_eq!(owner[1], ("alpha parent", &[][..]));
    assert_eq!(
        owner[2],
        (
            "alpha grandchild",
            &["alpha parent".to_string(), "alpha child".to_string()][..]
        )
    );

    let zero = read(
        &corpus,
        &QueryPlan::friendly("alpha", 0, 0),
        &ResultIdentity::Stored,
    )
    .expect("zero-limit read");
    assert!(zero.hits.is_empty());
    assert!(!zero.has_more.pages && !zero.has_more.blocks);
}

#[test]
fn supplied_scope_path_is_authoritative_over_the_scope_name() {
    let _serial = serialize();
    let root = scratch("friendly-main-scope");
    write_friendly_corpus(&root);
    let corpus = Corpus::open(root, true);
    let plan = QueryPlan::friendly_for_page(
        "alpha",
        16,
        QueryPageScope {
            name: "a deliberately different display identity".into(),
            page_kind: PageKind::Journal,
            path: Some("pages/Owner.md".into()),
        },
    );
    let expected = plan.execute_with_explain(&corpus.graph, || false, true);
    let actual = read(&corpus, &plan, &ResultIdentity::Stored).expect("scoped read");
    assert_same_execution(&expected, &actual);
    assert!(actual.hits.iter().all(|hit| matches!(
        hit,
        QueryHit::Block { path, .. } if path == "pages/Owner.md"
    )));
}

fn write_many_blocks(root: &Path, count: usize) {
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    let body = (0..count)
        .map(|at| format!("- batch needle {at}\n"))
        .collect::<String>();
    std::fs::write(root.join("pages/Many.md"), body).expect("many blocks");
}

#[test]
fn admitted_payload_uses_the_shared_128_row_batches_and_never_hydrates_the_sentinel() {
    let _serial = serialize();
    let root = scratch("friendly-main-batches");
    write_many_blocks(&root, PAYLOAD_BATCH + 2);
    let corpus = Corpus::open(root, true);
    reset_result_read_census();
    reset_friendly_read_census();
    let answer = read(
        &corpus,
        &QueryPlan::friendly("needle", 0, PAYLOAD_BATCH + 1),
        &ResultIdentity::Stored,
    )
    .expect("batched read");
    let census = result_read_census();
    let friendly = friendly_read_census();
    assert_eq!(
        answer
            .hits
            .iter()
            .filter(|hit| matches!(hit, QueryHit::Block { .. }))
            .count(),
        PAYLOAD_BATCH + 1
    );
    assert!(answer.has_more.blocks);
    assert_eq!(census.payload_statements, 6);
    assert_eq!(census.payload_block_rows, PAYLOAD_BATCH + 1);
    assert_eq!(friendly.block_descriptors, PAYLOAD_BATCH + 2);
    assert_eq!(friendly.ancestor_statements, 0);
}

#[test]
fn cancellation_between_payload_batches_returns_no_partial_execution() {
    let _serial = serialize();
    let root = scratch("friendly-main-cancel");
    write_many_blocks(&root, PAYLOAD_BATCH + 2);
    let corpus = Corpus::open(root, true);
    let plan = QueryPlan::friendly("needle", 0, PAYLOAD_BATCH + 1);
    let mut snapshot = corpus.snapshot();
    let cancellation = snapshot.cancellation();
    set_before_payload_batch_hook(Some(Box::new(move |batch| {
        if batch == 1 {
            cancellation.cancel();
        }
    })));
    let answer = read_friendly_results(
        &mut snapshot,
        &FriendlyReadInputs {
            plan: &plan,
            graph_root: &corpus.root,
            identity: &ResultIdentity::Stored,
            explain: true,
            lane: None,
        },
    );
    set_before_payload_batch_hook(None);
    assert!(matches!(answer, Err(ResultReadError::Cancelled)));
    snapshot.finish();
}

#[test]
fn cancellation_inside_rank_and_before_ancestor_work_returns_no_partial_execution() {
    let _serial = serialize();
    let root = scratch("friendly-main-rank-ancestor-cancel");
    write_friendly_corpus(&root);
    let corpus = Corpus::open(root, true);

    let rank_plan = QueryPlan::friendly("alpha", 8, 8);
    let mut rank_snapshot = corpus.snapshot();
    let rank_cancellation = rank_snapshot.cancellation();
    set_before_friendly_rank_hook(Some(Box::new(move || rank_cancellation.cancel())));
    let ranked = read_friendly_results(
        &mut rank_snapshot,
        &FriendlyReadInputs {
            plan: &rank_plan,
            graph_root: &corpus.root,
            identity: &ResultIdentity::Stored,
            explain: true,
            lane: None,
        },
    );
    set_before_friendly_rank_hook(None);
    assert!(matches!(ranked, Err(ResultReadError::Cancelled)));
    rank_snapshot.finish();

    let ancestor_plan = QueryPlan::friendly("grandchild", 0, 8);
    let mut ancestor_snapshot = corpus.snapshot();
    let ancestor_cancellation = ancestor_snapshot.cancellation();
    set_before_ancestor_batch_hook(Some(Box::new(move || ancestor_cancellation.cancel())));
    let ancestry = read_friendly_results(
        &mut ancestor_snapshot,
        &FriendlyReadInputs {
            plan: &ancestor_plan,
            graph_root: &corpus.root,
            identity: &ResultIdentity::Stored,
            explain: true,
            lane: None,
        },
    );
    set_before_ancestor_batch_hook(None);
    assert!(matches!(ancestry, Err(ResultReadError::Cancelled)));
    ancestor_snapshot.finish();
}

fn copy_projection(corpus: &Corpus, tag: &str) -> std::path::PathBuf {
    let destination = std::env::temp_dir().join(format!(
        "tine-friendly-damaged-{tag}-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    let source = rusqlite::Connection::open_with_flags(
        corpus.projection_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("the projection opens read-only");
    source
        .execute(
            "VACUUM INTO ?1",
            rusqlite::params![destination.to_string_lossy().as_ref()],
        )
        .expect("the projection copies");
    destination
}

#[test]
fn missing_and_cross_owner_result_metadata_fail_the_whole_read() {
    let _serial = serialize();
    let root = scratch("friendly-main-damage");
    write_friendly_corpus(&root);
    let corpus = Corpus::open(root, true);
    let plan = QueryPlan::friendly("alpha", 0, 16);
    for (tag, damage) in [
        (
            "missing",
            "DELETE FROM query_block_results WHERE block_id = (\
                SELECT b.block_id FROM blocks b JOIN block_text t USING (block_id) \
                WHERE instr(t.query_visible, 'alpha') > 0 LIMIT 1)",
        ),
        (
            "cross-owner",
            "UPDATE query_block_results SET page_id = (\
                SELECT page_id FROM pages WHERE path = 'pages/Other.md'), preorder = 999999 \
             WHERE block_id = (SELECT b.block_id FROM blocks b \
                JOIN pages p USING (page_id) JOIN block_text t USING (block_id) \
                WHERE p.path = 'pages/Owner.md' \
                  AND instr(t.query_visible, 'alpha') > 0 LIMIT 1)",
        ),
    ] {
        let path = copy_projection(&corpus, tag);
        let writer = rusqlite::Connection::open(&path).expect("damage copy opens");
        writer
            .pragma_update(None, "foreign_keys", false)
            .expect("foreign keys disabled for damage fixture");
        assert!(writer.execute(damage, []).expect("damage applies") > 0);
        drop(writer);
        let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(()))
            .expect("damaged snapshot opens");
        let answer = read_friendly_results(
            &mut snapshot,
            &FriendlyReadInputs {
                plan: &plan,
                graph_root: &corpus.root,
                identity: &ResultIdentity::Stored,
                explain: true,
                lane: None,
            },
        );
        assert!(matches!(answer, Err(ResultReadError::Corrupt(_))));
        snapshot.finish();
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn compiled_plan_branches_are_served_without_restoring_a_walk_specific_filter() {
    let source = include_str!("friendly.rs");
    assert!(source.contains("ORDER BY missing_text DESC, rank_key, path COLLATE BINARY, preorder"));
    assert!(!source.contains("matched_parent"));
    assert!(!source.contains("ConstructionBudget"));
    assert_eq!(
        QueryPlan::friendly("needle", 2, 3)
            .branches
            .iter()
            .map(|branch| branch.target)
            .collect::<Vec<_>>(),
        [QueryTarget::Pages, QueryTarget::Blocks]
    );
}
