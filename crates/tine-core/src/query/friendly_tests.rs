//! Source-only gates for the Friendly projection reader. The implementation
//! packet intentionally does not execute them; the manager runs them after
//! wiring the public adapters on the combined exact head.

use std::path::Path;

use tine_storage::sqlite::PhysicalProjectionQuerySnapshot;

use super::*;
use crate::model::PageKind;
use crate::query::candidate::INTERACTIVE_SCAN_BUDGET;
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
    let structural = ResultIdentity::structural();
    for plan in &plans {
        let expected = plan.execute_with_explain(&corpus.graph, || false, true);
        let stored = read(&corpus, plan, &ResultIdentity::session_owned())
            .expect("the Stored Friendly read answers");
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
        &ResultIdentity::session_owned(),
    )
    .expect("limited read");
    assert!(matches!(one.hits.first(), Some(QueryHit::Page { .. })));
    assert!(matches!(one.hits.get(1), Some(QueryHit::Block { .. })));
    assert!(one.has_more.blocks);

    let blocks = read(
        &corpus,
        &QueryPlan::friendly("alpha", 0, 16),
        &ResultIdentity::session_owned(),
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
        &ResultIdentity::session_owned(),
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
    let actual = read(&corpus, &plan, &ResultIdentity::session_owned()).expect("scoped read");
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
        &ResultIdentity::session_owned(),
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
            identity: &ResultIdentity::session_owned(),
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
            identity: &ResultIdentity::session_owned(),
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
            identity: &ResultIdentity::session_owned(),
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
    for (tag, damage) in [
        (
            "missing",
            "DELETE FROM block_text WHERE block_id = (\
                SELECT b.block_id FROM blocks b JOIN block_text t USING (block_id) \
                WHERE instr(t.content, 'alpha') > 0 LIMIT 1)",
        ),
        (
            "cross-owner",
            "UPDATE blocks SET page_id = (\
                SELECT page_id FROM pages WHERE path = 'pages/Other.md'), preorder = 999999 \
             WHERE block_id = (SELECT b.block_id FROM blocks b \
                JOIN pages p USING (page_id) JOIN block_text t USING (block_id) \
                WHERE p.path = 'pages/Owner.md' \
                  AND instr(t.content, 'alpha') > 0 LIMIT 1)",
        ),
    ] {
        for consumer in [
            crate::query_plan::FriendlyConsumer::NonInteractive,
            crate::query_plan::FriendlyConsumer::CtrlK,
        ] {
            let plan = crate::query_plan::friendly_search_plan_for(
                "alpha",
                0,
                16,
                None,
                crate::query_plan::FriendlyDisplayOptions::default(),
                consumer,
            );
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
                    identity: &ResultIdentity::session_owned(),
                    explain: true,
                    lane: None,
                },
            );
            assert!(
                matches!(answer, Err(ResultReadError::Corrupt(_))),
                "{tag} damage must fail the {consumer:?} read"
            );
            snapshot.finish();
            let _ = std::fs::remove_file(path);
        }
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
    assert!(source.contains("substr(r.rank_key, 1, {}), r.path COLLATE BINARY, r.preorder"));
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
    let structural = ResultIdentity::structural();

    // ---- Names: exactly the name and alias owners, and NOTHING body-only ----
    let names = read_with(
        &corpus,
        &scoped("zeta", 8, 8, FriendlyPageMatchScope::Names),
        &ResultIdentity::session_owned(),
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
        &ResultIdentity::session_owned(),
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
        &ResultIdentity::session_owned(),
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
        &ResultIdentity::session_owned(),
    );
    assert!(
        !page_identities(&split).contains(&("pages/Split.md".to_string(), PageKind::Page)),
        "two terms satisfied by two SEPARATE blocks do not admit their page"
    );

    // ---- Both: the union by physical identity, Names winners first --------
    let both = read_with(
        &corpus,
        &scoped("zeta", 8, 8, FriendlyPageMatchScope::Both),
        &ResultIdentity::session_owned(),
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

    // ---- Stored and structural identities order the same pages -----------
    for scope in [
        FriendlyPageMatchScope::Names,
        FriendlyPageMatchScope::Content,
        FriendlyPageMatchScope::Both,
    ] {
        let plan = scoped("zeta", 8, 8, scope);
        let stored = read_with(&corpus, &plan, &ResultIdentity::session_owned());
        let direct = read_with(&corpus, &plan, &structural);
        assert_eq!(
            page_identities(&stored),
            page_identities(&direct),
            "{scope:?}: both identity policies order the same pages"
        );
        assert_eq!(
            block_identities(&stored).len(),
            block_identities(&direct).len(),
            "{scope:?}: both identity policies admit the same blocks"
        );
        // Scope changes page membership ONLY: the Blocks section is the
        // ordinary Friendly block predicate on every scope.
        assert_eq!(
            block_identities(&stored),
            block_identities(&read_with(
                &corpus,
                &QueryPlan::friendly("zeta", 8, 8),
                &ResultIdentity::session_owned()
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

    let unbounded = read_with(
        &corpus,
        &sampled(None, None),
        &ResultIdentity::session_owned(),
    );
    let all_pages = page_identities(&unbounded);
    let all_blocks = block_identities(&unbounded);
    assert!(
        all_pages.len() > 1 && all_blocks.len() > 3,
        "the fixture over-fills both sections"
    );

    // Page sample 1 / block sample 3: each family keeps its OWN bound, and the
    // rows it keeps are the first rows of the complete order.
    let mixed = read_with(
        &corpus,
        &sampled(Some(1), Some(3)),
        &ResultIdentity::session_owned(),
    );
    assert_eq!(page_identities(&mixed).len(), 1);
    assert_eq!(block_identities(&mixed).len(), 3);
    assert_eq!(page_identities(&mixed), all_pages[..1].to_vec());
    assert_eq!(block_identities(&mixed), all_blocks[..3].to_vec());
    assert!(mixed.has_more.pages && mixed.has_more.blocks);

    // Zero empties ONLY its own family; no capacity transfers to the other.
    let no_pages = read_with(
        &corpus,
        &sampled(Some(0), Some(3)),
        &ResultIdentity::session_owned(),
    );
    assert!(page_identities(&no_pages).is_empty());
    assert_eq!(block_identities(&no_pages), all_blocks[..3].to_vec());
    let no_blocks = read_with(
        &corpus,
        &sampled(Some(1), Some(0)),
        &ResultIdentity::session_owned(),
    );
    assert_eq!(page_identities(&no_blocks).len(), 1);
    assert!(block_identities(&no_blocks).is_empty());

    // An absent sample preserves the consumer's own bound.
    let half = read_with(
        &corpus,
        &sampled(None, Some(1)),
        &ResultIdentity::session_owned(),
    );
    assert_eq!(page_identities(&half), all_pages);
    assert_eq!(block_identities(&half).len(), 1);

    // A sample larger than the consumer's bound cannot raise it.
    let over = read_with(
        &corpus,
        &sampled(Some(10_000), Some(10_000)),
        &ResultIdentity::session_owned(),
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
        &ResultIdentity::session_owned(),
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
    let hydrated = read_with(&corpus, &displayed, &ResultIdentity::session_owned());
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
        &ResultIdentity::session_owned(),
    ));
    let descending = page_identities(&read_with(
        &corpus,
        &by_name(SortDir::Desc, None),
        &ResultIdentity::session_owned(),
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
        &ResultIdentity::session_owned(),
    ));
    assert_eq!(capped, descending[..2].to_vec());
}

fn write_candidate_bound_corpus(root: &Path, filler: usize) {
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    let mut body = (0..filler)
        .map(|at| format!("- filler line {at}\n"))
        .collect::<String>();
    body.push_str("- Alpha ZQXWOOD upper\n");
    body.push_str("- CJK \u{4f60}\u{597d}\u{4e16}\u{754c} block\n");
    body.push_str("- block 4242 spread across words\n");
    body.push_str("- a quoted needle here phrase\n");
    body.push_str("- oo short pair\n");
    std::fs::write(root.join("pages/Bound.md"), body).expect("bound page");
}

fn block_hits(answer: &QueryExecution) -> usize {
    answer
        .hits
        .iter()
        .filter(|hit| matches!(hit, QueryHit::Block { .. }))
        .count()
}

/// The candidate bound, measured where it acts: rows RANKED, not rows returned.
///
/// The outer select already drops non-matches, so a bounded read and an
/// unbounded one return the same rows — the difference is how many the
/// statement had to materialize and rank to find them. Before the bound, TWO
/// CTEs ranked every block in scope — the block results, and the page-by-content
/// membership set — which is why a 10,000-page graph answered one Ctrl+K
/// keystroke in ~4.5 s whether the needle matched 101 blocks (4524 ms) or none
/// (4372 ms). Driven from the index those are 1721 ms and 1677 ms.
#[test]
fn a_selective_needle_ranks_only_its_candidates() {
    let _serial = serialize();
    let root = scratch("friendly-candidate-bound");
    write_candidate_bound_corpus(&root, 200);
    let corpus = Corpus::open(root, true);
    // A path over an EMPTY index would pass this test while proving nothing,
    // so the precondition is asserted, not assumed.
    assert!(
        corpus.trigram_fts_rows() > 0,
        "the substring index must hold block rows"
    );
    reset_friendly_read_census();
    let answer = read(
        &corpus,
        &QueryPlan::friendly("zqxwood", 0, 20),
        &ResultIdentity::session_owned(),
    )
    .expect("bounded read");
    let census = friendly_read_census();
    assert_eq!(block_hits(&answer), 1);
    assert!(
        census.block_rank_evaluations <= 4,
        "one candidate, but the statement ranked {} rows of a 205-block page",
        census.block_rank_evaluations
    );
}

/// The correctness half: the bound NARROWS which rows are asked and never
/// decides the answer, so every block the exact predicate admits must still
/// come back — through a fold difference, a needle too short to index, a
/// multi-word AND, a quoted phrase, a negation, and an unbounded OR arm.
#[test]
fn the_candidate_bound_drops_no_block_the_exact_predicate_admits() {
    let _serial = serialize();
    let root = scratch("friendly-bound-correctness");
    write_candidate_bound_corpus(&root, 8);
    let corpus = Corpus::open(root, true);
    assert!(corpus.trigram_fts_rows() > 0);
    for (query, expected) in [
        // The query's case differs from the block's: both sides fold.
        ("zqxwood", 1),
        // Two characters yield no trigram, so this needle is unbounded — and
        // answering it is not optional.
        ("\u{4f60}\u{597d}", 1),
        // AND of two terms: bounding by ONE of them can drop no match.
        ("block 4242", 1),
        // A quoted phrase still contains its own whitespace-free runs.
        ("\"needle here\"", 1),
        // The negated term supplies no needle; the positive one does.
        ("-filler zqxwood", 1),
        // One unbounded arm leaves the whole OR unbounded.
        ("oo OR zqxwood", 2),
    ] {
        let answer = read(
            &corpus,
            &QueryPlan::friendly(query, 0, 20),
            &ResultIdentity::session_owned(),
        )
        .unwrap_or_else(|error| panic!("{query} read failed: {error}"));
        assert_eq!(block_hits(&answer), expected, "query {query}");
    }
}

#[test]
fn ctrl_k_ranks_only_the_newest_verified_window_but_inline_remains_exhaustive() {
    let _serial = serialize();
    let root = scratch("friendly-verified-window");
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    let mut body = String::from("- needle\n");
    for at in 0..700 {
        body.push_str(&format!("- needle filler {at}\n"));
    }
    std::fs::write(root.join("pages/Window.md"), body).expect("window page");
    let corpus = Corpus::open(root, true);

    let exhaustive = crate::query_plan::friendly_search_plan_for(
        "needle",
        0,
        1,
        None,
        crate::query_plan::FriendlyDisplayOptions::default(),
        crate::query_plan::FriendlyConsumer::NonInteractive,
    );
    let interactive = crate::query_plan::friendly_search_plan_for(
        "needle",
        0,
        1,
        None,
        crate::query_plan::FriendlyDisplayOptions::default(),
        crate::query_plan::FriendlyConsumer::CtrlK,
    );
    let exhaustive =
        read(&corpus, &exhaustive, &ResultIdentity::session_owned()).expect("exhaustive read");
    reset_friendly_read_census();
    let interactive =
        read(&corpus, &interactive, &ResultIdentity::session_owned()).expect("interactive read");
    let interactive_census = friendly_read_census();

    assert!(matches!(
        exhaustive.hits.first(),
        Some(QueryHit::Block { display_text, .. }) if display_text == "needle"
    ));
    assert!(matches!(
        interactive.hits.first(),
        Some(QueryHit::Block { display_text, .. }) if display_text.starts_with("needle filler ")
    ));
    assert!(interactive.has_more.blocks);
    assert_eq!(interactive_census.block_candidate_visits, 300);
    assert_eq!(interactive_census.block_candidate_verifications, 300);
    assert!(
        interactive_census.block_rank_evaluations <= 305,
        "the retained interactive window must rank only its 300 members, got {} rank calls",
        interactive_census.block_rank_evaluations
    );
}

fn interactive_block_ids(corpus: &Corpus, plan: &QueryPlan) -> Vec<i64> {
    let branch = plan
        .branches
        .iter()
        .find(|branch| branch.target == QueryTarget::Blocks)
        .expect("interactive plan has a block branch");
    let CandidateMode::Interactive { window } = plan.candidate_mode() else {
        panic!("test plan must be interactive");
    };
    let mut snapshot = corpus.snapshot();
    let ids = interactive_verified_block_ids(&mut snapshot, plan, branch, window, &None)
        .expect("interactive candidate cursor answers");
    snapshot.finish();
    ids.ids
}

#[test]
fn broad_scan_and_index_cursors_stop_at_w_verified_rows_in_descending_membership_order() {
    let _serial = serialize();
    let root = scratch("friendly-broad-streams");
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    let body = (0..701)
        .map(|at| format!("- needle oo broad {at}\n"))
        .collect::<String>();
    std::fs::write(root.join("pages/Broad.md"), body).expect("broad page");
    let corpus = Corpus::open(root, true);

    for needle in ["needle", "oo"] {
        let plan = crate::query_plan::friendly_search_plan_for(
            needle,
            0,
            1,
            None,
            crate::query_plan::FriendlyDisplayOptions::default(),
            crate::query_plan::FriendlyConsumer::CtrlK,
        );
        reset_friendly_read_census();
        let ids = interactive_block_ids(&corpus, &plan);
        let census = friendly_read_census();
        assert_eq!(ids.len(), 300, "{needle} retains exactly W members");
        assert!(
            ids.windows(2).all(|pair| pair[0] > pair[1]),
            "{needle} membership remains rowid-descending"
        );
        assert_eq!(census.block_candidate_visits, 300, "{needle} visits");
        assert_eq!(
            census.block_candidate_verifications, 300,
            "{needle} verifications"
        );
    }
}

#[derive(Debug)]
struct IndexedCursorWork {
    rows: usize,
    vm_steps: i32,
    fullscan_steps: i32,
    sorts: i32,
    plan: Vec<String>,
}

fn indexed_cursor_work(blocks: usize) -> IndexedCursorWork {
    use rusqlite::{Connection, OpenFlags, StatementStatus};

    let root = scratch(&format!("friendly-indexed-cursor-{blocks}"));
    write_many_blocks(&root, blocks);
    let corpus = Corpus::open(root, true);
    let writer = Connection::open(corpus.projection_path()).expect("projection opens for ANALYZE");
    writer
        .execute_batch("ANALYZE")
        .expect("fixture is analyzed");
    let statistics: i64 = writer
        .query_row("SELECT COUNT(*) FROM sqlite_stat1", [], |row| row.get(0))
        .expect("sqlite_stat1 remains populated");
    assert!(statistics > 0, "the cost fixture needs planner statistics");
    drop(writer);

    let plan = crate::query_plan::friendly_search_plan_for(
        "needle",
        0,
        1,
        None,
        crate::query_plan::FriendlyDisplayOptions::default(),
        crate::query_plan::FriendlyConsumer::CtrlK,
    );
    let branch = plan
        .branches
        .iter()
        .find(|branch| branch.target == QueryTarget::Blocks)
        .expect("interactive plan has a block branch");
    let (sql, params) = interactive_block_cursor_statement(&plan, branch);
    let mut snapshot = corpus.snapshot();
    let plan = snapshot
        .explain_query_plan(&sql, &params)
        .expect("the production statement seam explains the cursor");
    snapshot.finish();
    let connection =
        Connection::open_with_flags(corpus.projection_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("the projection opens read-only");
    let bound = params
        .iter()
        .map(|value| value as &dyn rusqlite::ToSql)
        .collect::<Vec<_>>();
    let mut statement = connection.prepare(&sql).expect("cursor prepares");
    let mut rows = statement.query(bound.as_slice()).expect("cursor executes");
    let mut seen = Vec::new();
    while seen.len() < crate::query::candidate::INTERACTIVE_VERIFIED_WINDOW {
        let Some(row) = rows.next().expect("cursor advances") else {
            break;
        };
        seen.push(row.get::<_, i64>(0).expect("block id"));
    }
    drop(rows);
    let work = IndexedCursorWork {
        rows: seen.len(),
        vm_steps: statement.get_status(StatementStatus::VmStep),
        fullscan_steps: statement.get_status(StatementStatus::FullscanStep),
        sorts: statement.get_status(StatementStatus::Sort),
        plan,
    };
    assert!(
        seen.windows(2).all(|pair| pair[0] > pair[1]),
        "cursor rows remain newest first"
    );
    work
}

#[test]
fn common_indexed_cursor_streams_the_verified_window_without_sorting_the_posting() {
    let _serial = serialize();
    let small = indexed_cursor_work(1_200);
    let large = indexed_cursor_work(9_000);
    eprintln!("small indexed cursor work: {small:#?}");
    eprintln!("large indexed cursor work: {large:#?}");

    for work in [&small, &large] {
        assert_eq!(
            work.rows,
            crate::query::candidate::INTERACTIVE_VERIFIED_WINDOW
        );
        assert!(
            work.plan
                .iter()
                .any(|step| step.contains("search_fts") && step.contains("M1")),
            "the production cursor must use the FTS match plan: {work:#?}"
        );
        assert_eq!(work.sorts, 0, "the common posting must stream: {work:#?}");
        assert!(
            work.plan
                .iter()
                .all(|step| !step.contains("TEMP B-TREE FOR ORDER BY")),
            "the production cursor must not sort the whole posting: {work:#?}"
        );
    }
    assert!(
        large.vm_steps <= small.vm_steps * 2,
        "VM work to reach W must be posting-size independent: small={small:#?}, large={large:#?}"
    );
    assert!(
        large.fullscan_steps <= small.fullscan_steps * 2,
        "row work to reach W must be posting-size independent: small={small:#?}, large={large:#?}"
    );
}

#[test]
fn sparse_false_positive_candidates_stream_to_exhaustion_before_the_old_match() {
    let _serial = serialize();
    let root = scratch("friendly-sparse-stream");
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    let mut body = String::from("- exact abcd survivor\n");
    for at in 0..650 {
        body.push_str(&format!("- false abc gap bcd candidate {at}\n"));
    }
    std::fs::write(root.join("pages/Sparse.md"), body).expect("sparse page");
    let corpus = Corpus::open(root, true);
    let plan = crate::query_plan::friendly_search_plan_for(
        "abcd",
        0,
        1,
        None,
        crate::query_plan::FriendlyDisplayOptions::default(),
        crate::query_plan::FriendlyConsumer::CtrlK,
    );

    reset_friendly_read_census();
    let ids = interactive_block_ids(&corpus, &plan);
    let census = friendly_read_census();
    assert_eq!(ids.len(), 1);
    assert_eq!(census.block_candidate_visits, 651);
    assert_eq!(census.block_candidate_verifications, 651);
}

#[test]
fn interactive_page_scope_is_applied_before_the_verified_window() {
    let _serial = serialize();
    let root = scratch("friendly-scoped-window");
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    std::fs::write(root.join("pages/A-InScope.md"), "- needle old scoped\n").expect("scoped page");
    let outside = (0..650)
        .map(|at| format!("- needle newer outside {at}\n"))
        .collect::<String>();
    std::fs::write(root.join("pages/Z-Outside.md"), outside).expect("outside page");
    let corpus = Corpus::open(root, true);
    let plan = crate::query_plan::friendly_search_plan_for(
        "needle",
        0,
        1,
        Some(QueryPageScope {
            name: "A-InScope".into(),
            page_kind: PageKind::Page,
            path: Some("pages/A-InScope.md".into()),
        }),
        crate::query_plan::FriendlyDisplayOptions::default(),
        crate::query_plan::FriendlyConsumer::CtrlK,
    );

    reset_friendly_read_census();
    let answer =
        read(&corpus, &plan, &ResultIdentity::session_owned()).expect("scoped interactive read");
    let census = friendly_read_census();
    assert!(matches!(
        answer.hits.first(),
        Some(QueryHit::Block { path, display_text, .. })
            if path == "pages/A-InScope.md" && display_text == "needle old scoped"
    ));
    assert_eq!(census.block_candidate_visits, 1);
    assert_eq!(census.block_candidate_verifications, 1);
}

#[test]
fn virtual_name_candidates_are_dictionary_membership_not_posting_occurrences() {
    let source = include_str!("friendly.rs");
    assert!(source.contains("FROM names n WHERE EXISTS"));
    assert!(source.contains("INDEXED BY reference_postings_navigation_names_idx"));
    assert!(!source.contains("JOIN names n ON n.name_id = r.target_name_id"));
}

mod virtual_name_candidate_cost_tests {
    use rusqlite::{Connection, OpenFlags, StatementStatus};

    use super::*;

    fn candidate_statement() -> String {
        format!(
            "WITH real_identities(name_key) AS (\
                 SELECT n.key FROM pages p JOIN names n ON n.name_id = p.name_id \
                 UNION SELECT n.key FROM reference_alias_declarations a \
                 JOIN names n ON n.name_id = a.alias_name_id\
             ), {} \
             SELECT raw_name, normalized_name FROM reference_choices \
             WHERE name_choice = 1 ORDER BY normalized_name",
            VIRTUAL_REFERENCE_CHOICES_CTE
        )
    }

    fn legacy_occurrence_statement() -> &'static str {
        "WITH real_identities(name_key) AS (\
             SELECT n.key FROM pages p JOIN names n ON n.name_id = p.name_id \
             UNION SELECT n.key FROM reference_alias_declarations a \
             JOIN names n ON n.name_id = a.alias_name_id\
         ), reference_choices AS (\
             SELECT n.raw AS raw_name, n.key AS normalized_name, ROW_NUMBER() OVER (\
                 PARTITION BY n.key ORDER BY n.raw, n.key, r.source_page_id\
             ) AS name_choice \
             FROM reference_postings r JOIN names n ON n.name_id = r.target_name_id \
             WHERE r.target_type = 0 AND r.reference_kind <= 4 \
               AND NOT EXISTS (SELECT 1 FROM real_identities i WHERE i.name_key = n.key)\
         ) SELECT raw_name, normalized_name FROM reference_choices \
         WHERE name_choice = 1 ORDER BY normalized_name"
    }

    #[derive(Debug)]
    struct CandidateWork {
        names: i64,
        postings: i64,
        candidates: Vec<(String, String)>,
        vm_steps: i32,
        fullscan_steps: i32,
        sorts: i32,
        plan: Vec<String>,
        legacy_vm_steps: i32,
    }

    fn statement_work(
        connection: &Connection,
        sql: &str,
    ) -> (Vec<(String, String)>, i32, i32, i32) {
        let mut statement = connection
            .prepare(sql)
            .expect("candidate statement prepares");
        let candidates = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("candidate rows execute")
            .collect::<Result<Vec<_>, _>>()
            .expect("candidate rows decode");
        (
            candidates,
            statement.get_status(StatementStatus::VmStep),
            statement.get_status(StatementStatus::FullscanStep),
            statement.get_status(StatementStatus::Sort),
        )
    }

    fn measure_candidate_work(corpus: &Corpus) -> CandidateWork {
        let connection =
            Connection::open_with_flags(corpus.projection_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("the projection opens read-only");
        let names = connection
            .query_row("SELECT COUNT(*) FROM names", [], |row| row.get(0))
            .expect("names count");
        let postings = connection
            .query_row(
                "SELECT COUNT(*) FROM reference_postings \
                 WHERE target_type = 0 AND reference_kind <= 4",
                [],
                |row| row.get(0),
            )
            .expect("eligible postings count");
        let sql = candidate_statement();
        let (candidates, vm_steps, fullscan_steps, sorts) = statement_work(&connection, &sql);
        let (_, legacy_vm_steps, _, _) = statement_work(&connection, legacy_occurrence_statement());
        let mut explain = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("candidate explain prepares");
        let plan = explain
            .query_map([], |row| row.get(3))
            .expect("candidate explain executes")
            .collect::<Result<Vec<String>, _>>()
            .expect("candidate explain decodes");
        CandidateWork {
            names,
            postings,
            candidates,
            vm_steps,
            fullscan_steps,
            sorts,
            plan,
            legacy_vm_steps,
        }
    }

    fn write_cost_corpus(root: &Path, repetitions: usize) {
        std::fs::create_dir_all(root.join("pages")).expect("pages");
        let mut body = String::from("orphanproperty:: value\n\n");
        for _ in 0..repetitions {
            body.push_str("- [[Ghost One]] [[ghost one]] [[Ghost Two]]\n");
        }
        std::fs::write(root.join("pages/Owner.md"), body).expect("owner page");
    }

    #[test]
    fn virtual_name_membership_work_is_independent_of_reference_occurrences() {
        let _serial = serialize();
        let measure = |tag, repetitions| {
            let root = scratch(tag);
            write_cost_corpus(&root, repetitions);
            let corpus = Corpus::open(root, true);
            measure_candidate_work(&corpus)
        };
        let sparse = measure("friendly-virtual-name-cost-sparse", 1);
        let repeated = measure("friendly-virtual-name-cost-repeated", 2_000);
        eprintln!("virtual-name work sparse={sparse:?} repeated={repeated:?}");

        assert_eq!(sparse.names, repeated.names, "the name inventories match");
        assert!(
            repeated.postings > sparse.postings * 1_000,
            "the fixture must materially increase occurrences: {sparse:?} vs {repeated:?}"
        );
        assert_eq!(sparse.candidates, repeated.candidates);
        assert_eq!(sparse.fullscan_steps, repeated.fullscan_steps);
        assert_eq!(sparse.sorts, repeated.sorts);
        assert!(
            repeated.vm_steps <= sparse.vm_steps + 16,
            "indexed membership must stop at the first posting: {sparse:?} vs {repeated:?}"
        );
        assert!(
            repeated.legacy_vm_steps > sparse.legacy_vm_steps * 100,
            "the occurrence-enumerating counterexample must detect the old work: \
             {sparse:?} vs {repeated:?}"
        );
        assert!(
            repeated.plan.iter().any(|step| step
                .contains("SEARCH r USING COVERING INDEX reference_postings_navigation_names_idx")),
            "eligible membership must use the covering target-name index: {:?}",
            repeated.plan
        );
        assert!(
            !repeated
                .plan
                .iter()
                .any(|step| step == "SCAN r" || step.starts_with("SCAN r ")),
            "the candidate relation must not enumerate postings: {:?}",
            repeated.plan
        );
    }

    #[test]
    fn virtual_name_results_keep_spelling_and_identity_suppression() {
        let _serial = serialize();
        let root = scratch("friendly-virtual-name-semantics");
        std::fs::create_dir_all(root.join("pages")).expect("pages");
        std::fs::write(
            root.join("pages/Known.md"),
            "alias:: Known Alias\n\n- stored owner\n",
        )
        .expect("known page");
        std::fs::write(
            root.join("pages/Owner.md"),
            "orphanproperty:: value\n\n\
             - [[ghost name]] [[Ghost Name]] [[GHOST NAME]]\n\
             - [[Ghost Name]] [[Known]] [[Known Alias]]\n",
        )
        .expect("reference owner");
        let corpus = Corpus::open(root, true);

        let work = measure_candidate_work(&corpus);
        assert_eq!(
            work.candidates,
            vec![(
                "GHOST NAME".to_string(),
                crate::refs::page_key("ghost name")
            )],
            "one lexicographically chosen raw spelling survives; repeated references, \
             property names, physical titles and aliases do not add virtual rows"
        );

        let ghosts = read(
            &corpus,
            &QueryPlan::friendly("ghost name", 16, 0),
            &ResultIdentity::session_owned(),
        )
        .expect("virtual name read");
        let virtual_ghosts = ghosts
            .hits
            .iter()
            .filter_map(|hit| match hit {
                QueryHit::Page { page, .. } if page.rel_path.is_empty() => Some(page.name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(virtual_ghosts, ["GHOST NAME"]);

        for query in ["known", "known alias"] {
            let answer = read(
                &corpus,
                &QueryPlan::friendly(query, 16, 0),
                &ResultIdentity::session_owned(),
            )
            .unwrap_or_else(|error| panic!("{query} read failed: {error}"));
            assert!(
                answer.hits.iter().all(|hit| matches!(
                    hit,
                    QueryHit::Page { page, .. } if !page.rel_path.is_empty()
                )),
                "a physical title/alias identity suppresses its virtual suggestion: {query}"
            );
        }
        let property = read(
            &corpus,
            &QueryPlan::friendly("orphanproperty", 16, 0),
            &ResultIdentity::session_owned(),
        )
        .expect("property-name read");
        assert!(
            property.hits.is_empty(),
            "a property name is not a virtual page"
        );
    }
}

#[test]
fn page_rank_composite_key_preserves_candidates_and_ranks_each_once() {
    let _serial = serialize();
    let root = scratch("friendly-page-composite-rank");
    write_friendly_corpus(&root);
    let corpus = Corpus::open(root, true);
    let identity = ResultIdentity::session_owned();

    reset_friendly_read_census();
    let broad_plan = QueryPlan::friendly("a", 16, 0);
    let broad = read(&corpus, &broad_plan, &identity).expect("broad page read");
    assert_same_execution(
        &broad_plan.execute_with_explain(&corpus.graph, || false, true),
        &broad,
    );
    assert_eq!(
        friendly_read_census().page_rank_evaluations,
        8,
        "three titles, four authored aliases and one chosen virtual name are ranked once each"
    );

    let cases = [
        ("foo later", "pages/Owner.md", "foo later", true),
        ("foo", "pages/Owner.md", "foo OR bar", true),
        ("café", "pages/Café.md", "Café", false),
        ("ghost page", "", "Ghost Page", false),
    ];
    for (query, path, display, matched_alias) in cases {
        let plan = QueryPlan::friendly(query, 16, 0);
        let answer = read(&corpus, &plan, &identity)
            .unwrap_or_else(|error| panic!("{query} page read failed: {error}"));
        assert_same_execution(
            &plan.execute_with_explain(&corpus.graph, || false, true),
            &answer,
        );
        assert!(
            answer.hits.iter().any(|hit| matches!(
                hit,
                QueryHit::Page {
                    page,
                    display_text,
                    matched_alias: alias,
                    ..
                } if page.rel_path == path
                    && display_text == display
                    && alias.is_some() == matched_alias
            )),
            "{query} must retain its title/alias/virtual winner and tie behavior: {answer:#?}"
        );
    }
}

/// GH #543 Ctrl-K: a needle under three characters has no trigram index to
/// drive it, so the interactive read scans blocks newest first. A pair only
/// the oldest block contains made that scan visit every block in the graph
/// (~1.5 s per keystroke at 616k blocks). It now stops at
/// `INTERACTIVE_SCAN_BUDGET` rows and says more matches may exist; a recent
/// match still answers.
/// One- and two-character words are ordinary in Chinese, Japanese and
/// Korean. The scan budget for short needles cut such searches off at the
/// newest blocks; they must find every match, however old. Since ADR 0069 the
/// short-word index drives them: a rare word reads its own blocks, not the
/// ~20k-row walk that took 1.5 s per keystroke at 616k blocks.
#[test]
fn a_short_cjk_search_finds_matches_past_the_scan_budget() {
    let _serial = serialize();
    let root = scratch("friendly-scan-budget-cjk");
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    let mut body = String::from("- 東京 oldest\n- 서울 oldest\n- ねこ oldest\n");
    for at in 0..INTERACTIVE_SCAN_BUDGET + 50 {
        body.push_str(&format!("- plain {at}\n"));
    }
    std::fs::write(root.join("pages/Many.md"), body).expect("many page");
    let corpus = Corpus::open(root, true);
    for needle in ["東京", "서울", "ねこ", "京"] {
        let plan = crate::query_plan::friendly_search_plan_for(
            needle,
            0,
            10,
            None,
            crate::query_plan::FriendlyDisplayOptions::default(),
            crate::query_plan::FriendlyConsumer::CtrlK,
        );
        reset_friendly_read_census();
        let found = read(&corpus, &plan, &ResultIdentity::session_owned()).expect("search");
        let visits = friendly_read_census().block_candidate_visits;
        assert!(
            found.hits.iter().any(|hit| matches!(
                hit,
                QueryHit::Block { display_text, .. } if display_text.contains(needle)
            )),
            "{needle} finds its oldest block"
        );
        assert!(
            visits <= 2,
            "{needle}: the short-word index drives the read, but it visited {visits} blocks"
        );
        assert!(
            !found.has_more.blocks,
            "{needle}: a complete scan has no more"
        );
    }
}

#[test]
fn an_unindexed_interactive_scan_stops_at_its_budget_and_says_so() {
    let _serial = serialize();
    let root = scratch("friendly-scan-budget");
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    let mut body = String::from("- zq oldest\n");
    for at in 0..INTERACTIVE_SCAN_BUDGET + 50 {
        body.push_str(&format!("- plain {at}\n"));
    }
    body.push_str("- xj newest\n");
    std::fs::write(root.join("pages/Many.md"), body).expect("many page");
    let corpus = Corpus::open(root, true);
    let plan = |needle: &str| {
        crate::query_plan::friendly_search_plan_for(
            needle,
            0,
            10,
            None,
            crate::query_plan::FriendlyDisplayOptions::default(),
            crate::query_plan::FriendlyConsumer::CtrlK,
        )
    };

    reset_friendly_read_census();
    let old = read(&corpus, &plan("zq"), &ResultIdentity::session_owned()).expect("old pair");
    let census = friendly_read_census();
    assert!(
        census.block_candidate_visits <= INTERACTIVE_SCAN_BUDGET + 1,
        "the scan visited {} rows",
        census.block_candidate_visits
    );
    assert!(old.hits.is_empty(), "the oldest block lies past the budget");
    assert!(old.has_more.blocks, "a stopped scan says more may match");

    let recent = read(&corpus, &plan("xj"), &ResultIdentity::session_owned()).expect("new pair");
    assert!(matches!(
        recent.hits.first(),
        Some(QueryHit::Block { display_text, .. }) if display_text == "xj newest"
    ));
}
