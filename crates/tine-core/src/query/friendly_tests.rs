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
    // Q3 gave the Blocks section its own authored sort, so the tail of this
    // ORDER BY is built from the plan and the columns are aliased to the
    // materialized CTE. Neither fact is what this pin is about: the DEFAULT
    // ordering is unchanged, and a compiled branch is still served straight
    // from the projection rather than by restoring a walk-specific filter.
    assert!(source.contains("ORDER BY r.missing_text DESC, {order}"));
    assert!(source.contains("r.rank_key, r.path COLLATE BINARY, r.preorder"));
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

// --------------------------------------------------------------------------
// Q3 — Display controls: page membership scope, independent section bounds
// --------------------------------------------------------------------------

use crate::query::ir::{FriendlyPageMatchScope, SortDir, ViewSettings};
use crate::query_plan::FriendlyDisplayOptions;

/// One page per membership ROLE, so a scope answer names a page rather than a
/// count. `zeta` is the needle throughout; `omega` exists only to state the
/// split-term nonmatch.
fn write_membership_corpus(root: &Path) {
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    // Name-only: the NAME carries the needle, no block of it does.
    std::fs::write(
        root.join("pages/Zeta Notes.md"),
        "- nothing relevant here\n",
    )
    .expect("name-only owner");
    // Alias-only: neither the name nor any block carries it; one alias does.
    std::fs::write(
        root.join("pages/Aliased.md"),
        "alias:: zeta secondary\n\n- unrelated body\n",
    )
    .expect("alias-only owner");
    // Content-only, with THREE matching blocks: exactly one wins evidence, and
    // the page still appears exactly once.
    std::fs::write(
        root.join("pages/Body Only.md"),
        "- a much longer sentence that mentions zeta somewhere late\n\
         - zeta\n\
         - zeta again, at medium length\n",
    )
    .expect("content-only owner");
    // Both: the NAME wins in a union, and its evidence is the name's.
    std::fs::write(
        root.join("pages/Zeta And Body.md"),
        "- this body also says zeta\n",
    )
    .expect("name-and-content owner");
    // Split terms across UNRELATED blocks: neither block satisfies the whole
    // predicate, so `zeta omega` must not admit this page by content.
    std::fs::write(
        root.join("pages/Split.md"),
        "- only zeta lives here\n- and only omega lives here\n",
    )
    .expect("split-term nonmatch");
}

fn read_with(corpus: &Corpus, plan: &QueryPlan, identity: &ResultIdentity) -> QueryExecution {
    read(corpus, plan, identity).expect("the Friendly read answers")
}

/// The ordered page identities an execution returned: `(rel_path, kind)` for a
/// stored page, and the display name for a virtual suggestion — never a bare
/// display name for a stored one, which is what duplicate titles destroy.
fn page_identities(execution: &QueryExecution) -> Vec<(String, PageKind)> {
    execution
        .hits
        .iter()
        .filter_map(|hit| match hit {
            QueryHit::Page { page, .. } if !page.rel_path.is_empty() => {
                Some((page.rel_path.clone(), page.kind))
            }
            _ => None,
        })
        .collect()
}

fn block_identities(execution: &QueryExecution) -> Vec<(String, String)> {
    execution
        .hits
        .iter()
        .filter_map(|hit| match hit {
            QueryHit::Block { path, block, .. } => Some((path.clone(), block.id.clone())),
            _ => None,
        })
        .collect()
}

fn scoped(
    source: &str,
    page_limit: usize,
    block_limit: usize,
    scope: FriendlyPageMatchScope,
) -> QueryPlan {
    QueryPlan::friendly_with_display(
        source,
        page_limit,
        block_limit,
        FriendlyDisplayOptions {
            page_match_scope: Some(scope),
            ..FriendlyDisplayOptions::default()
        },
    )
}

#[test]
fn q3_friendly_scope_membership_and_evidence() {
    let _serial = serialize();
    let root = scratch("q3-friendly-scope");
    write_membership_corpus(&root);
    let corpus = Corpus::open(root, true);
    let structural = ResultIdentity::DirectStructural {
        session_pages: Arc::new(HashSet::new()),
        all_session: false,
    };

    // ---- Names: exactly the name and alias owners, and NOTHING body-only ----
    let names = read_with(
        &corpus,
        &scoped("zeta", 8, 8, FriendlyPageMatchScope::Names),
        &ResultIdentity::Stored,
    );
    let named = page_identities(&names);
    assert!(
        named.contains(&("pages/Zeta Notes.md".to_string(), PageKind::Page)),
        "the name-only owner is a Names member: {named:?}"
    );
    assert!(
        named.contains(&("pages/Aliased.md".to_string(), PageKind::Page)),
        "the alias-only owner is a Names member: {named:?}"
    );
    assert!(
        !named.contains(&("pages/Body Only.md".to_string(), PageKind::Page)),
        "a body-only page is NOT a Names member: {named:?}"
    );
    // An absent scope is the historic behaviour, byte for byte.
    let historic = read_with(
        &corpus,
        &QueryPlan::friendly("zeta", 8, 8),
        &ResultIdentity::Stored,
    );
    assert_same_execution(&historic, &names);
    // …and the historic answer is still the independent walk's.
    assert_same_execution(
        &QueryPlan::friendly("zeta", 8, 8).execute_with_explain(&corpus.graph, || false, true),
        &historic,
    );

    // The alias owner is returned ONCE, retaining its winning alias.
    let alias = names
        .hits
        .iter()
        .find(
            |hit| matches!(hit, QueryHit::Page { page, .. } if page.rel_path == "pages/Aliased.md"),
        )
        .expect("the alias owner is present");
    let QueryHit::Page {
        matched_alias,
        evidence,
        display_text,
        ..
    } = alias
    else {
        unreachable!()
    };
    assert_eq!(matched_alias.as_deref(), Some("zeta secondary"));
    assert_eq!(display_text, "zeta secondary");
    assert!(
        evidence
            .iter()
            .all(|e| e.field == crate::query_plan::TextField::PageName),
        "a Names winner's evidence is page-name evidence"
    );

    // ---- Content: exactly the pages one of whose OWN blocks satisfies the
    // block predicate — including the name-and-body owner, excluding the
    // name-only and alias-only owners.
    let content = read_with(
        &corpus,
        &scoped("zeta", 8, 8, FriendlyPageMatchScope::Content),
        &ResultIdentity::Stored,
    );
    let bodies = page_identities(&content);
    assert!(bodies.contains(&("pages/Body Only.md".to_string(), PageKind::Page)));
    assert!(bodies.contains(&("pages/Zeta And Body.md".to_string(), PageKind::Page)));
    assert!(bodies.contains(&("pages/Split.md".to_string(), PageKind::Page)));
    assert!(!bodies.contains(&("pages/Zeta Notes.md".to_string(), PageKind::Page)));
    assert!(!bodies.contains(&("pages/Aliased.md".to_string(), PageKind::Page)));
    assert_eq!(
        bodies
            .iter()
            .filter(|(path, _)| path == "pages/Body Only.md")
            .count(),
        1,
        "three matching blocks admit their page exactly ONCE: {bodies:?}"
    );
    // The winning block is the best-matching one, and its evidence indexes the
    // block's own text.
    let body = content
        .hits
        .iter()
        .find(|hit| matches!(hit, QueryHit::Page { page, .. } if page.rel_path == "pages/Body Only.md"))
        .expect("the content owner is present");
    let QueryHit::Page {
        display_text,
        evidence,
        match_class,
        matched_alias,
        ..
    } = body
    else {
        unreachable!()
    };
    assert_eq!(
        display_text, "zeta",
        "the BEST matching block is the winner"
    );
    assert_eq!(
        *match_class,
        crate::query_plan::ObjectiveMatchClass::BodyEvidence
    );
    assert_eq!(
        matched_alias.as_deref(),
        None,
        "a body winner is not an alias"
    );
    assert!(
        evidence
            .iter()
            .all(|e| e.field == crate::query_plan::TextField::VisibleContent),
        "a Content winner's evidence is block-content evidence"
    );

    // ---- split terms across unrelated blocks do not admit a page ----------
    let split = read_with(
        &corpus,
        &scoped("zeta omega", 8, 8, FriendlyPageMatchScope::Content),
        &ResultIdentity::Stored,
    );
    assert!(
        !page_identities(&split).contains(&("pages/Split.md".to_string(), PageKind::Page)),
        "two terms satisfied by two SEPARATE blocks do not admit their page"
    );

    // ---- Both: the union by physical identity, Names winners first --------
    let both = read_with(
        &corpus,
        &scoped("zeta", 8, 8, FriendlyPageMatchScope::Both),
        &ResultIdentity::Stored,
    );
    let union = page_identities(&both);
    for role in [
        "pages/Zeta Notes.md",
        "pages/Aliased.md",
        "pages/Body Only.md",
        "pages/Zeta And Body.md",
    ] {
        assert_eq!(
            union.iter().filter(|(path, _)| path == role).count(),
            1,
            "{role} is in the union exactly once: {union:?}"
        );
    }
    let names_last = union
        .iter()
        .position(|(path, _)| path == "pages/Zeta And Body.md")
        .expect("the name-and-body owner is in the union");
    let content_first = union
        .iter()
        .position(|(path, _)| path == "pages/Body Only.md")
        .expect("the content-only owner is in the union");
    assert!(
        names_last < content_first,
        "with no explicit sort, Names winners precede Content-only winners: {union:?}"
    );
    // A page matching BOTH uses its Names winner: the evidence is the name's.
    let overlap = both
        .hits
        .iter()
        .find(|hit| matches!(hit, QueryHit::Page { page, .. } if page.rel_path == "pages/Zeta And Body.md"))
        .expect("the overlapping owner is present");
    let QueryHit::Page { evidence, .. } = overlap else {
        unreachable!()
    };
    assert!(
        evidence
            .iter()
            .all(|e| e.field == crate::query_plan::TextField::PageName),
        "a page matching both keeps its NAMES evidence"
    );

    // ---- Managed and Direct answer the same ordered identities ------------
    for scope in [
        FriendlyPageMatchScope::Names,
        FriendlyPageMatchScope::Content,
        FriendlyPageMatchScope::Both,
    ] {
        let plan = scoped("zeta", 8, 8, scope);
        let stored = read_with(&corpus, &plan, &ResultIdentity::Stored);
        let direct = read_with(&corpus, &plan, &structural);
        assert_eq!(
            page_identities(&stored),
            page_identities(&direct),
            "{scope:?}: both backends order the same page identities"
        );
        assert_eq!(
            block_identities(&stored).len(),
            block_identities(&direct).len(),
            "{scope:?}: both backends admit the same blocks"
        );
        // Scope changes page membership ONLY: the Blocks section is the
        // ordinary Friendly block predicate on every scope.
        assert_eq!(
            block_identities(&stored),
            block_identities(&read_with(
                &corpus,
                &QueryPlan::friendly("zeta", 8, 8),
                &ResultIdentity::Stored
            )),
            "{scope:?}: the Blocks section is untouched by page membership scope"
        );
    }
}

#[test]
fn q3_mixed_limits_are_independent() {
    let _serial = serialize();
    let root = scratch("q3-friendly-samples");
    write_membership_corpus(&root);
    let corpus = Corpus::open(root, true);

    let sampled = |page: Option<u32>, block: Option<u32>| {
        QueryPlan::friendly_with_display(
            "zeta",
            8,
            8,
            FriendlyDisplayOptions {
                page_match_scope: Some(FriendlyPageMatchScope::Both),
                page_view: page.map(|sample| ViewSettings {
                    sample: Some(sample),
                    ..ViewSettings::default()
                }),
                block_view: block.map(|sample| ViewSettings {
                    sample: Some(sample),
                    ..ViewSettings::default()
                }),
            },
        )
    };

    let unbounded = read_with(&corpus, &sampled(None, None), &ResultIdentity::Stored);
    let all_pages = page_identities(&unbounded);
    let all_blocks = block_identities(&unbounded);
    assert!(
        all_pages.len() > 1 && all_blocks.len() > 3,
        "the fixture over-fills both sections"
    );

    // Page sample 1 / block sample 3: each family keeps its OWN bound, and the
    // rows it keeps are the first rows of the complete order.
    let mixed = read_with(&corpus, &sampled(Some(1), Some(3)), &ResultIdentity::Stored);
    assert_eq!(page_identities(&mixed).len(), 1);
    assert_eq!(block_identities(&mixed).len(), 3);
    assert_eq!(page_identities(&mixed), all_pages[..1].to_vec());
    assert_eq!(block_identities(&mixed), all_blocks[..3].to_vec());
    assert!(mixed.has_more.pages && mixed.has_more.blocks);

    // Zero empties ONLY its own family; no capacity transfers to the other.
    let no_pages = read_with(&corpus, &sampled(Some(0), Some(3)), &ResultIdentity::Stored);
    assert!(page_identities(&no_pages).is_empty());
    assert_eq!(block_identities(&no_pages), all_blocks[..3].to_vec());
    let no_blocks = read_with(&corpus, &sampled(Some(1), Some(0)), &ResultIdentity::Stored);
    assert_eq!(page_identities(&no_blocks).len(), 1);
    assert!(block_identities(&no_blocks).is_empty());

    // An absent sample preserves the consumer's own bound.
    let half = read_with(&corpus, &sampled(None, Some(1)), &ResultIdentity::Stored);
    assert_eq!(page_identities(&half), all_pages);
    assert_eq!(block_identities(&half).len(), 1);

    // A sample larger than the consumer's bound cannot raise it.
    let over = read_with(
        &corpus,
        &sampled(Some(10_000), Some(10_000)),
        &ResultIdentity::Stored,
    );
    assert_eq!(page_identities(&over), all_pages);
    assert_eq!(block_identities(&over), all_blocks);
}

#[test]
fn q3_friendly_page_display_hydrates_stored_rows_only() {
    let _serial = serialize();
    let root = scratch("q3-friendly-hydration");
    write_membership_corpus(&root);
    // A reference nobody stores: a virtual navigation suggestion, which names
    // no page and therefore has no properties to hydrate.
    std::fs::write(
        root.join("pages/Zeta Notes.md"),
        "type:: note\nowner:: zeta team\n\n- see [[Zeta Ghost]]\n",
    )
    .expect("owner with properties and a ghost reference");
    let corpus = Corpus::open(root, true);

    // No page view: nothing hydrates, exactly as before this packet.
    let plain = read_with(
        &corpus,
        &QueryPlan::friendly("zeta", 8, 8),
        &ResultIdentity::Stored,
    );
    assert!(plain.hits.iter().all(|hit| matches!(
        hit,
        QueryHit::Page { row: None, .. } | QueryHit::Block { .. }
    )));

    let displayed = QueryPlan::friendly_with_display(
        "zeta",
        8,
        8,
        FriendlyDisplayOptions {
            page_view: Some(ViewSettings::default()),
            ..FriendlyDisplayOptions::default()
        },
    );
    let hydrated = read_with(&corpus, &displayed, &ResultIdentity::Stored);
    let mut stored_rows = 0;
    let mut virtual_rows = 0;
    for hit in &hydrated.hits {
        let QueryHit::Page { page, row, .. } = hit else {
            continue;
        };
        if page.rel_path.is_empty() {
            virtual_rows += 1;
            assert!(row.is_none(), "a virtual suggestion hydrates no properties");
            continue;
        }
        stored_rows += 1;
        let row = row
            .as_ref()
            .expect("a stored page on the Display path carries its row");
        assert_eq!(row.path, page.rel_path);
        assert_eq!(row.name, page.name);
        if row.path == "pages/Zeta Notes.md" {
            assert_eq!(
                row.properties,
                vec![
                    ("type".to_string(), "note".to_string()),
                    ("owner".to_string(), "zeta team".to_string()),
                ],
                "authored page properties arrive in source order"
            );
        }
    }
    assert!(stored_rows > 0, "the fixture returns stored pages");
    assert!(virtual_rows > 0, "the fixture returns a virtual suggestion");
}

#[test]
fn q3_friendly_sections_sort_the_complete_set_before_their_bound() {
    let _serial = serialize();
    let root = scratch("q3-friendly-order");
    write_membership_corpus(&root);
    let corpus = Corpus::open(root, true);

    let by_name = |direction: SortDir, sample: Option<u32>| {
        QueryPlan::friendly_with_display(
            "zeta",
            8,
            8,
            FriendlyDisplayOptions {
                page_match_scope: Some(FriendlyPageMatchScope::Both),
                page_view: Some(ViewSettings {
                    sort: vec![(crate::query::ir::Field::new("name"), direction)],
                    sample,
                    ..ViewSettings::default()
                }),
                ..FriendlyDisplayOptions::default()
            },
        )
    };

    let ascending = page_identities(&read_with(
        &corpus,
        &by_name(SortDir::Asc, None),
        &ResultIdentity::Stored,
    ));
    let descending = page_identities(&read_with(
        &corpus,
        &by_name(SortDir::Desc, None),
        &ResultIdentity::Stored,
    ));
    assert!(ascending.len() > 2, "the fixture has enough pages to order");
    assert_ne!(ascending, descending, "the authored direction is honoured");
    assert_eq!(
        descending,
        ascending.iter().rev().cloned().collect::<Vec<_>>(),
        "one order, reversed — not two different selections"
    );
    // An explicit sort orders the COMPLETE union, so the bound takes the first
    // rows of THAT order rather than of the relevance order (Q4's decision).
    let capped = page_identities(&read_with(
        &corpus,
        &by_name(SortDir::Desc, Some(2)),
        &ResultIdentity::Stored,
    ));
    assert_eq!(capped, descending[..2].to_vec());
}
