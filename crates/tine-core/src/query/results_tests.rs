//! R3's gates: the database-backed result constructor answers exactly what the
//! walk answers, reads no document to do it, and fails rather than shrinking
//! when the projection contradicts itself.
//!
//! **The oracle is the walk itself.** Every parity gate below compares the
//! COMPLETE `PreViewGroups` — group order, page names and kinds, every
//! `BlockDto` field, `total`, `exceeded` and `recency_by_page` — against
//! [`crate::query::collect_pred_bounded_over`] over the SAME graph, the same
//! day, the same bounds and the same profile. Comparing ids alone would pass
//! with an empty `raw` on every row.
//!
//! **The harness is `sql_gates_tests`'s.** The corpus, the projection built by
//! the PRODUCTION producer, the lowering entry and the shape tables are the
//! ones §5's gates already own; this file adds fixtures, never a second
//! graph/projection fixture (D-14).
//!
//! No corpus content is read into an assertion message, a receipt or any other
//! artifact — difference lines carry a shape source, an index and a field name,
//! and nothing else, so the `#[ignore]`d real-corpus twins are safe to run.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tine_storage::sqlite::{PhysicalProjectionQuerySnapshot, PhysicalQueryValue};

use crate::date::JournalDate;

use crate::model::{block_dto_estimated_bytes, BlockDto, PageKind};
use crate::query::ir::{Bounds, ExecutionContext};
use crate::query::ir::{Query, ViewSettings};
use crate::query::results::{
    read_results, reset_result_read_census, result_read_census, set_before_payload_batch_hook,
    BackendOrder, RecencyPage, ResultIdentity, ResultReadError, ResultReadInputs, PAYLOAD_BATCH,
};
use crate::query::sql::sql_gates_tests::{
    scratch, serialize, write_fast_corpus, Corpus, CONTENT_PLAN_SHAPES, IDENTITY_SHAPES,
    PLAN_SHAPES,
};
use crate::query::sql::{descriptor_statement, ContentPlan};
use crate::query::{
    collect_pred_bounded_over, page_recency_secs_for, ConstructionProfile, GraphQueryPages,
    PreViewGroups, QueryDialect, QueryInput, QueryPageSource,
};

/// Every shape the parity gates run, from §5's own three tables. `PLAN_SHAPES`
/// and `CONTENT_PLAN_SHAPES` overlap `IDENTITY_SHAPES`; the duplicates are
/// harmless and de-duplicating them would make the gate's coverage depend on
/// which table happened to name a shape first.
fn every_shape() -> Vec<(&'static str, QueryDialect)> {
    let mut shapes: Vec<(&'static str, QueryDialect)> = Vec::new();
    shapes.extend(IDENTITY_SHAPES.iter().copied());
    shapes.extend(PLAN_SHAPES.iter().copied());
    shapes.extend(
        CONTENT_PLAN_SHAPES.iter().map(
            |(source, dialect, _plan): &(&str, QueryDialect, ContentPlan)| (*source, *dialect),
        ),
    );
    shapes
}

/// The bounds/profile combinations §4's acceptance list names: unbounded, zero
/// rows, zero bytes, an unsorted `(sample N)`, and the recency axis. The byte
/// BOUNDARY cases are computed per shape from the unbounded run, because a
/// boundary that closes mid-page has to be a real cumulative cost and not a
/// guess.
fn bound_combinations() -> Vec<(usize, usize, ConstructionProfile)> {
    vec![
        (usize::MAX, usize::MAX, ConstructionProfile::default()),
        (0, usize::MAX, ConstructionProfile::default()),
        (usize::MAX, 0, ConstructionProfile::default()),
        (2, usize::MAX, ConstructionProfile::default()),
        (
            usize::MAX,
            usize::MAX,
            ConstructionProfile {
                sample_admission_cap: Some(3),
                want_recency: false,
            },
        ),
        (
            usize::MAX,
            usize::MAX,
            ConstructionProfile {
                sample_admission_cap: None,
                want_recency: true,
            },
        ),
    ]
}

/// What `ConstructionBudget::admit_estimated` charges for one admitted row:
/// the payload estimate, the page name, and the fixed group overhead. The gate
/// derives byte boundaries from this so a `max_bytes` can be chosen that closes
/// exactly between two rows of ONE page.
fn budget_cost(page: &str, block: &BlockDto) -> usize {
    block_dto_estimated_bytes(block)
        .saturating_add(page.len())
        .saturating_add(256)
}

/// Byte budgets that close the construction at an exact row boundary, and one
/// byte short of each — the second is what closes MID-page when the page holds
/// more than one admitted row.
fn byte_boundaries(reference: &PreViewGroups) -> Vec<usize> {
    let mut cumulative = 0usize;
    let mut budgets = Vec::new();
    for group in &reference.groups {
        for block in &group.blocks {
            cumulative = cumulative.saturating_add(budget_cost(&group.page, block));
            budgets.push(cumulative);
            if cumulative > 0 {
                budgets.push(cumulative - 1);
            }
            if budgets.len() >= 8 {
                return budgets;
            }
        }
    }
    budgets
}

/// The recency producer, bound to one corpus root: the EXISTING axis
/// (`page_recency_secs_for`) reached through R3's `(journal day, page path)`
/// signature. A second spelling here would make the gate agree with itself
/// rather than with the walk.
fn recency_for(root: &Path) -> impl Fn(RecencyPage<'_>) -> i64 + '_ {
    move |page| page_recency_secs_for(page.journal_day, &root.join(page.path))
}

/// The walk's answer for one shape under one set of bounds.
fn walk_answer(
    corpus: &Corpus,
    source: &str,
    dialect: QueryDialect,
    max_rows: usize,
    max_bytes: usize,
    profile: ConstructionProfile,
) -> PreViewGroups {
    let (query, _statement) = corpus.lower_block_anchored(source, dialect);
    collect_pred_bounded_over(
        &GraphQueryPages(&corpus.graph),
        &query,
        corpus.today(),
        max_rows,
        max_bytes,
        profile,
    )
}

/// The database's answer for one shape under one set of bounds, through an
/// owned read snapshot and nothing else.
fn read_answer(
    corpus: &Corpus,
    source: &str,
    dialect: QueryDialect,
    max_rows: usize,
    max_bytes: usize,
    profile: ConstructionProfile,
    identity: &ResultIdentity,
) -> Result<PreViewGroups, ResultReadError> {
    let (_query, statement) = corpus.lower_block_anchored(source, dialect);
    let root = corpus.root.clone();
    let recency = recency_for(&root);
    let mut snapshot = corpus.snapshot();
    let answer = read_results(
        &mut snapshot,
        &ResultReadInputs {
            statement: &statement,
            order: BackendOrder::Direct,
            identity,
            max_rows,
            max_bytes,
            profile,
            recency: &recency,
        },
    );
    snapshot.finish();
    answer
}

/// ONE line per difference between two pre-view results, naming the shape, the
/// position and the FIELD — never a page name, a raw line, a tag or a property
/// value. This is what makes the real-corpus twins runnable.
fn differences(label: &str, walk: &PreViewGroups, read: &PreViewGroups) -> Vec<String> {
    let mut out = Vec::new();
    if walk.total != read.total {
        out.push(format!(
            "{label}: total walk={} read={}",
            walk.total, read.total
        ));
    }
    if walk.exceeded != read.exceeded {
        out.push(format!(
            "{label}: exceeded walk={} read={}",
            walk.exceeded, read.exceeded
        ));
    }
    if walk.groups.len() != read.groups.len() {
        out.push(format!(
            "{label}: groups walk={} read={}",
            walk.groups.len(),
            read.groups.len()
        ));
        return out;
    }
    for (at, (expected, actual)) in walk.groups.iter().zip(&read.groups).enumerate() {
        if expected.page != actual.page {
            out.push(format!("{label}: group {at} page name differs"));
        }
        if expected.kind != actual.kind {
            out.push(format!("{label}: group {at} page kind differs"));
        }
        if !actual.evidence.is_empty() {
            out.push(format!("{label}: group {at} carries evidence"));
        }
        if expected.blocks.len() != actual.blocks.len() {
            out.push(format!(
                "{label}: group {at} blocks walk={} read={}",
                expected.blocks.len(),
                actual.blocks.len()
            ));
            continue;
        }
        for (row, (expected, actual)) in expected.blocks.iter().zip(&actual.blocks).enumerate() {
            for field in block_field_differences(expected, actual) {
                out.push(format!("{label}: group {at} row {row} field {field}"));
            }
        }
    }
    if walk.recency_by_page.len() != read.recency_by_page.len() {
        out.push(format!(
            "{label}: recency pages walk={} read={}",
            walk.recency_by_page.len(),
            read.recency_by_page.len()
        ));
    }
    for (page, expected) in &walk.recency_by_page {
        match read.recency_by_page.get(page) {
            Some(actual) if actual == expected => {}
            Some(_) => out.push(format!("{label}: a recency value differs")),
            None => out.push(format!("{label}: a recency page is missing")),
        }
    }
    out
}

/// The names of the `BlockDto` fields that differ. Every field is compared:
/// a gate that skipped one would let that field drift silently.
fn block_field_differences(expected: &BlockDto, actual: &BlockDto) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if expected.id != actual.id {
        fields.push("id");
    }
    if expected.raw != actual.raw {
        fields.push("raw");
    }
    if expected.collapsed != actual.collapsed {
        fields.push("collapsed");
    }
    if !actual.children.is_empty() {
        fields.push("children");
    }
    if !actual.breadcrumb.is_empty() {
        fields.push("breadcrumb");
    }
    if actual.page_property {
        fields.push("page_property");
    }
    if expected.marker != actual.marker {
        fields.push("marker");
    }
    if expected.priority != actual.priority {
        fields.push("priority");
    }
    if expected.heading_level != actual.heading_level {
        fields.push("heading_level");
    }
    if expected.scheduled != actual.scheduled {
        fields.push("scheduled");
    }
    if expected.deadline != actual.deadline {
        fields.push("deadline");
    }
    if expected.tags != actual.tags {
        fields.push("tags");
    }
    if expected.properties != actual.properties {
        fields.push("properties");
    }
    fields
}

/// The whole parity sweep over one corpus, under one identity policy.
///
/// Returns `(difference lines, admitted rows)` so the real-corpus twin can
/// report a count and prove the gate had something to compare.
fn parity_over(corpus: &Corpus, identity: &ResultIdentity) -> (Vec<String>, usize) {
    let mut differences_out = Vec::new();
    let mut rows = 0usize;
    for (source, dialect) in every_shape() {
        let unbounded = walk_answer(
            corpus,
            source,
            dialect,
            usize::MAX,
            usize::MAX,
            ConstructionProfile::default(),
        );
        rows += unbounded
            .groups
            .iter()
            .map(|g| g.blocks.len())
            .sum::<usize>();
        let mut combinations = bound_combinations();
        combinations.extend(
            byte_boundaries(&unbounded)
                .into_iter()
                .map(|bytes| (usize::MAX, bytes, ConstructionProfile::default())),
        );
        for (max_rows, max_bytes, profile) in combinations {
            let label = format!("{source} rows={max_rows} bytes={max_bytes} profile={profile:?}");
            let walk = walk_answer(corpus, source, dialect, max_rows, max_bytes, profile);
            match read_answer(
                corpus, source, dialect, max_rows, max_bytes, profile, identity,
            ) {
                Ok(read) => differences_out.extend(differences(&label, &walk, &read)),
                Err(error) => {
                    differences_out.push(format!("{label}: the read failed: {error}"));
                }
            }
        }
    }
    (differences_out, rows)
}

// ===== the ordered full-DTO parity gate =====

/// The acceptance bar: the constructor's COMPLETE public result equals the
/// walk's, for every shape §5 names, under every bound §4 names, with the
/// stored identity.
#[test]
fn the_database_result_equals_the_walk_on_every_shape_and_bound() {
    let _serial = serialize();
    let root = scratch("r3-parity");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let (differences, rows) = parity_over(&corpus, &ResultIdentity::Stored);
    assert!(
        rows > 0,
        "the corpus admitted nothing; the gate proves nothing"
    );
    assert!(
        differences.is_empty(),
        "the walk and the database result disagree:\n{}",
        differences.join("\n")
    );
}

/// The same bar with the FRESH-SESSION Direct identity policy and an empty
/// session set — every row resolves structurally.
///
/// It must produce the SAME answer, ids included, because a fresh parse's
/// runtime ids ARE the structural ones: that is the whole warm-reopen claim,
/// and this is where it is decidable without reopening anything.
#[test]
fn a_fresh_direct_session_resolves_the_same_ids_structurally() {
    let _serial = serialize();
    let root = scratch("r3-structural");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let identity = ResultIdentity::DirectStructural {
        session_pages: Arc::new(HashSet::new()),
        all_session: false,
    };
    let (differences, rows) = parity_over(&corpus, &identity);
    assert!(
        rows > 0,
        "the corpus admitted nothing; the gate proves nothing"
    );
    assert!(
        differences.is_empty(),
        "the structural identity policy changes the answer:\n{}",
        differences.join("\n")
    );
}

/// The same gate over the anonymized graph (AGENTS §4 tier 2). Only shape
/// sources, indices, field names and counts are printed.
#[test]
#[ignore = "acceptance gate over a real corpus: set TINE_QUERY_IDENTITY_GRAPH"]
fn the_database_result_equals_the_walk_over_a_real_corpus() {
    let _serial = serialize();
    let Some(root) = std::env::var_os("TINE_QUERY_IDENTITY_GRAPH") else {
        eprintln!("skipped: set TINE_QUERY_IDENTITY_GRAPH to a corpus directory");
        return;
    };
    let corpus = Corpus::open(PathBuf::from(&root), false);
    let (stored, rows) = parity_over(&corpus, &ResultIdentity::Stored);
    let (structural, _) = parity_over(
        &corpus,
        &ResultIdentity::DirectStructural {
            session_pages: Arc::new(HashSet::new()),
            all_session: false,
        },
    );
    eprintln!(
        "r3_result_read_over_a_real_corpus shapes={} admitted_rows={rows} \
         stored_disagreements={} structural_disagreements={}",
        every_shape().len(),
        stored.len(),
        structural.len()
    );
    assert!(
        stored.is_empty() && structural.is_empty(),
        "the walk and the database result disagree on a real graph:\n{}\n{}",
        stored.join("\n"),
        structural.join("\n")
    );
}

// ===== ordering =====

/// Direct Files' cross-page base order IS the order the walk enumerates pages
/// in. The projection materializes it in `query_page_order`; if the two ever
/// drifted, a truncated budget would keep different rows on the two paths.
fn page_order_differences(corpus: &Corpus) -> Vec<String> {
    let mut walk_order: Vec<String> = Vec::new();
    GraphQueryPages(&corpus.graph).for_each_page(&mut |page| {
        walk_order.push(page.name.to_owned());
        std::ops::ControlFlow::Continue(())
    });
    let mut snapshot = corpus.snapshot();
    let rows = snapshot
        .run_projection_query(
            "SELECT p.name FROM query_page_order o JOIN pages p ON p.page_id = o.page_id \
             ORDER BY o.position",
            &[],
        )
        .expect("the page order is readable through the snapshot");
    snapshot.finish();
    let stored: Vec<String> = rows
        .iter()
        .map(|row| match row.first() {
            Some(PhysicalQueryValue::Text(name)) => name.clone(),
            other => panic!("pages.name is text, got {other:?}"),
        })
        .collect();
    let mut out = Vec::new();
    if walk_order.len() != stored.len() {
        out.push(format!(
            "page count walk={} stored={}",
            walk_order.len(),
            stored.len()
        ));
        return out;
    }
    for (at, (expected, actual)) in walk_order.iter().zip(&stored).enumerate() {
        if expected != actual {
            out.push(format!("page order differs at position {at}"));
        }
    }
    out
}

#[test]
fn the_stored_page_order_is_the_walks_page_order() {
    let _serial = serialize();
    let root = scratch("r3-page-order");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let differences = page_order_differences(&corpus);
    assert!(
        differences.is_empty(),
        "query_page_order is not the walk's enumeration order:\n{}",
        differences.join("\n")
    );
}

#[test]
#[ignore = "acceptance gate over a real corpus: set TINE_QUERY_IDENTITY_GRAPH"]
fn the_stored_page_order_is_the_walks_page_order_over_a_real_corpus() {
    let _serial = serialize();
    let Some(root) = std::env::var_os("TINE_QUERY_IDENTITY_GRAPH") else {
        eprintln!("skipped: set TINE_QUERY_IDENTITY_GRAPH to a corpus directory");
        return;
    };
    let corpus = Corpus::open(PathBuf::from(&root), false);
    let differences = page_order_differences(&corpus);
    eprintln!(
        "r3_page_order_over_a_real_corpus disagreements={}",
        differences.len()
    );
    assert!(
        differences.is_empty(),
        "query_page_order is not the walk's enumeration order on a real graph:\n{}",
        differences.join("\n")
    );
}

/// Pages whose paths separate SQLite's BINARY collation from anything
/// case-folding or locale-aware: ASCII case, a precomposed/decomposed pair, and
/// two physical pages that share a DISPLAY name.
fn write_ordering_corpus(root: &Path) {
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    std::fs::create_dir_all(root.join("pages/nested")).expect("nested pages");
    for (file, line) in [
        ("Alpha.md", "- upper alpha marker"),
        ("alpha.md", "- lower alpha marker"),
        ("Zebra.md", "- upper zebra marker"),
        ("_leading.md", "- underscore sorts after uppercase marker"),
        ("Caf\u{e9}.md", "- precomposed cafe marker"),
        ("Cafe\u{301}.md", "- decomposed cafe marker"),
        ("nested/Alpha.md", "- a duplicate display name marker"),
    ] {
        std::fs::write(root.join("pages").join(file), format!("{line}\n"))
            .unwrap_or_else(|error| panic!("{file}: {error}"));
    }
}

/// `BackendOrder::Managed` orders by `pages.path` under SQLite's default
/// BINARY collation, which must be `String::cmp` on the UTF-8 bytes — no
/// collation, no folding, no locale.
#[test]
fn the_managed_order_is_the_binary_path_order() {
    let _serial = serialize();
    let root = scratch("r3-managed-order");
    write_ordering_corpus(&root);
    let corpus = Corpus::open(root, true);
    let mut snapshot = corpus.snapshot();
    let rows = snapshot
        .run_projection_query("SELECT path FROM pages ORDER BY path", &[])
        .expect("the paths are readable through the snapshot");
    snapshot.finish();
    let sqlite_order: Vec<String> = rows
        .iter()
        .map(|row| match row.first() {
            Some(PhysicalQueryValue::Text(path)) => path.clone(),
            other => panic!("pages.path is text, got {other:?}"),
        })
        .collect();
    let mut rust_order = sqlite_order.clone();
    rust_order.sort_by(|left, right| left.cmp(right));
    assert_eq!(
        sqlite_order, rust_order,
        "SQLite BINARY is not Rust's byte order on these paths"
    );
    assert!(
        sqlite_order.len() >= 7,
        "the ordering corpus lost pages: {}",
        sqlite_order.len()
    );

    // The descriptor read under the Managed order visits pages in exactly that
    // byte order. Asserted on the descriptor's own `pages.path` column, which
    // is the ordering key itself — a group-level assertion could not see two
    // physical pages that share a display name.
    let (_query, statement) =
        corpus.lower_block_anchored("content match 'marker'", QueryDialect::Tql);
    let descriptor = descriptor_statement(&statement, BackendOrder::Managed)
        .expect("the managed descriptor statement builds");
    let mut snapshot = corpus.snapshot();
    snapshot
        .set_query_regex_predicate(statement.regexes.predicate())
        .expect("the regex table installs");
    let rows = snapshot
        .run_projection_query(&descriptor.sql, &descriptor.params)
        .expect("the managed descriptor statement runs");
    snapshot.finish();
    let visited: Vec<String> = rows
        .iter()
        .map(|row| match row.get(5) {
            Some(PhysicalQueryValue::Text(path)) => path.clone(),
            other => panic!("the descriptor selects pages.path, got {other:?}"),
        })
        .collect();
    assert_eq!(visited.len(), 7, "every ordering-corpus page has one match");
    let mut sorted = visited.clone();
    sorted.sort_by(|left, right| left.cmp(right));
    assert_eq!(
        visited, sorted,
        "the managed descriptor order is not the binary path order"
    );

    // Two PHYSICAL pages that share a display name stay two groups here;
    // `base_order_groups` merges for display, later and elsewhere.
    let root = corpus.root.clone();
    let recency = recency_for(&root);
    let mut snapshot = corpus.snapshot();
    let answer = read_results(
        &mut snapshot,
        &ResultReadInputs {
            statement: &statement,
            order: BackendOrder::Managed,
            identity: &ResultIdentity::Stored,
            max_rows: usize::MAX,
            max_bytes: usize::MAX,
            profile: ConstructionProfile::default(),
            recency: &recency,
        },
    )
    .expect("the managed-ordered read answers");
    snapshot.finish();
    let names: Vec<String> = answer
        .groups
        .iter()
        .map(|group| group.page.clone())
        .collect();
    assert_eq!(names.len(), 7, "every matching page is its own group");
    assert_eq!(
        names.iter().filter(|name| name.as_str() == "Alpha").count(),
        2,
        "two physical pages sharing a display name stay two groups"
    );
    assert_eq!(
        names.iter().filter(|name| name.as_str() == "alpha").count(),
        1,
        "case-distinct display names are distinct pages"
    );
}

// ===== the huge page =====

/// A page with several thousand blocks and exactly ONE match. Everything R3
/// promises is visible here: the walk's path parses the whole document, the
/// database path reads one descriptor row and one payload row.
fn write_huge_page_corpus(root: &Path, blocks: usize) {
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    let mut page = String::with_capacity(blocks * 32);
    for at in 0..blocks {
        if at == blocks / 2 {
            page.push_str("- the lone haystack needle\n");
        } else {
            page.push_str("- ordinary filler line\n");
        }
    }
    std::fs::write(root.join("pages/huge.md"), page).expect("huge page");
    std::fs::write(root.join("pages/other.md"), "- a second page of filler\n").expect("other page");
}

#[test]
fn a_huge_page_with_one_match_costs_one_descriptor_row_and_one_payload_row() {
    let _serial = serialize();
    let root = scratch("r3-huge");
    write_huge_page_corpus(&root, 3000);
    let corpus = Corpus::open(root, true);
    let source = "(content-regex \"haystack\")";

    // The PRODUCTION path (R3b): the dispatched query answers from the
    // projection alone. The census below records every parsed document the
    // projection-side readers load; a dispatched query contributes none.
    corpus.graph.reset_direct_projection_candidate_probe_test();
    let today = corpus
        .graph
        .run_query_bounded(source, usize::MAX, usize::MAX)
        .expect("the ready projection answers");
    assert_eq!(
        today
            .groups
            .iter()
            .map(|group| group.blocks.len())
            .sum::<usize>(),
        1,
        "the fixture has exactly one match"
    );
    let hydrated = corpus.graph.direct_projection_hydrated_pages_test();
    assert!(
        hydrated.is_empty(),
        "the dispatched path loads no page document: {hydrated:?}"
    );

    // R3: the same answer from the snapshot alone.
    reset_result_read_census();
    let answer = read_answer(
        &corpus,
        source,
        QueryDialect::Og,
        usize::MAX,
        usize::MAX,
        ConstructionProfile::default(),
        &ResultIdentity::Stored,
    )
    .expect("the database result read answers");
    let census = result_read_census();
    assert_eq!(
        answer
            .groups
            .iter()
            .map(|group| group.blocks.len())
            .sum::<usize>(),
        1
    );
    assert_eq!(
        census.descriptor_rows, 1,
        "one selected block, one descriptor"
    );
    assert_eq!(census.payload_statements, 3, "one batch, three statements");
    assert_eq!(
        census.payload_block_rows, 1,
        "payload only for the admitted id"
    );
    assert_eq!(census.payload_tag_rows, 0);
    assert_eq!(census.payload_property_rows, 0);

    // And it is the walk's answer, field for field.
    let walk = walk_answer(
        &corpus,
        source,
        QueryDialect::Og,
        usize::MAX,
        usize::MAX,
        ConstructionProfile::default(),
    );
    let differences = differences("huge", &walk, &answer);
    assert!(
        differences.is_empty(),
        "the huge-page answer differs:\n{}",
        differences.join("\n")
    );
}

// ===== batch arithmetic =====

#[test]
fn the_payload_is_read_in_batches_of_128_and_never_per_block() {
    let _serial = serialize();
    let root = scratch("r3-batches");
    write_huge_page_corpus(&root, 300);
    let corpus = Corpus::open(root, true);
    reset_result_read_census();
    let answer = read_answer(
        &corpus,
        "content match 'filler'",
        QueryDialect::Tql,
        usize::MAX,
        usize::MAX,
        ConstructionProfile::default(),
        &ResultIdentity::Stored,
    )
    .expect("the database result read answers");
    let census = result_read_census();
    let admitted: usize = answer.groups.iter().map(|group| group.blocks.len()).sum();
    assert!(
        admitted > PAYLOAD_BATCH,
        "the fixture must exceed one batch"
    );
    let batches = admitted.div_ceil(PAYLOAD_BATCH);
    assert_eq!(census.payload_statements, batches * 3);
    assert_eq!(census.payload_block_rows, admitted);
    assert_eq!(census.descriptor_rows, admitted);
}

// ===== corruption fails the read =====

/// A standalone, consistent copy of a corpus's projection, made through
/// SQLite's own `VACUUM INTO` so a WAL-resident page cannot be missed.
fn copy_projection(corpus: &Corpus, tag: &str) -> PathBuf {
    let destination = std::env::temp_dir().join(format!(
        "tine-r3-damaged-{tag}-{}.sqlite",
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

/// Damage one copy and read it. `foreign_keys` toggles the WRITER's
/// enforcement so the gate covers both the enforced and the unenforced
/// deletion, exactly as §4's acceptance list asks.
///
/// The damaged row is always one the healthy answer ADMITTED — a deletion the
/// query never looks at would prove nothing about a shorter answer.
#[allow(clippy::too_many_arguments)]
fn read_damaged(
    corpus: &Corpus,
    source: &str,
    dialect: QueryDialect,
    tag: &str,
    foreign_keys: bool,
    damage: &str,
    bind: &[&dyn rusqlite::ToSql],
) -> Result<PreViewGroups, ResultReadError> {
    let path = copy_projection(corpus, tag);
    {
        let writer = rusqlite::Connection::open(&path).expect("the copy opens writable");
        writer
            .pragma_update(None, "foreign_keys", foreign_keys)
            .expect("foreign key enforcement is settable");
        let changed = writer.execute(damage, bind).expect("the damage applies");
        assert!(
            changed > 0,
            "the damage statement changed nothing: {damage}"
        );
    }
    let (_query, statement) = corpus.lower_block_anchored(source, dialect);
    let root = corpus.root.clone();
    let recency = recency_for(&root);
    let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(()))
        .expect("the damaged copy still opens");
    let answer = read_results(
        &mut snapshot,
        &ResultReadInputs {
            statement: &statement,
            order: BackendOrder::Direct,
            identity: &ResultIdentity::Stored,
            max_rows: usize::MAX,
            max_bytes: usize::MAX,
            profile: ConstructionProfile::default(),
            recency: &recency,
        },
    );
    snapshot.finish();
    let _ = std::fs::remove_file(&path);
    answer
}

/// Every REQUIRED row of one admitted result, deleted one at a time, with
/// foreign keys enforced and not: a damaged disposable cache FAILS the read
/// (D-3). None of these may come back as a shorter answer.
#[test]
fn every_missing_required_row_fails_the_read_rather_than_shortening_it() {
    let _serial = serialize();
    let root = scratch("r3-damage");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);

    // The shape is the fast corpus's TAGGED root block, so one query reaches a
    // block that has a `query_block_results` row, a `block_text` row, a `tags`
    // row and a page with a `query_page_order` row.
    let source = "#inline-tag";
    let dialect = QueryDialect::Og;
    let healthy = read_answer(
        &corpus,
        source,
        dialect,
        usize::MAX,
        usize::MAX,
        ConstructionProfile::default(),
        &ResultIdentity::Stored,
    )
    .expect("the undamaged projection answers");
    let admitted: Vec<&BlockDto> = healthy
        .groups
        .iter()
        .flat_map(|group| group.blocks.iter())
        .collect();
    assert_eq!(admitted.len(), 1, "the damage fixture admits one block");
    let block_id = uuid::Uuid::parse_str(&admitted[0].id)
        .expect("an admitted id is a uuid")
        .into_bytes()
        .to_vec();
    assert!(
        !admitted[0].tags.is_empty(),
        "the damaged block must carry a tag"
    );

    let damages: [(&str, &str); 4] = [
        (
            "result",
            "DELETE FROM query_block_results WHERE block_id = ?1",
        ),
        ("text", "DELETE FROM block_text WHERE block_id = ?1"),
        (
            "tag",
            "DELETE FROM tags WHERE owner_type = 1 AND owner_id = ?1 AND ordinal = 0",
        ),
        (
            "order",
            "DELETE FROM query_page_order WHERE page_id = \
             (SELECT page_id FROM blocks WHERE block_id = ?1)",
        ),
    ];
    for (tag, damage) in damages {
        for foreign_keys in [true, false] {
            match read_damaged(
                &corpus,
                source,
                dialect,
                tag,
                foreign_keys,
                damage,
                rusqlite::params![block_id],
            ) {
                Err(ResultReadError::Corrupt(_)) => {}
                Err(other) => panic!("{tag}/fk={foreign_keys}: expected Corrupt, got {other}"),
                Ok(answer) => panic!(
                    "{tag}/fk={foreign_keys}: a damaged projection answered with {} rows",
                    answer.groups.iter().map(|g| g.blocks.len()).sum::<usize>()
                ),
            }
        }
    }
}

/// A page row that vanished while its blocks stayed is the LEFT-join case the
/// compiler's own routing join would have hidden: an INNER join would drop the
/// descriptor and answer with fewer rows.
///
/// Only with enforcement OFF. With `PRAGMA foreign_keys=ON` the same deletion
/// CASCADES the page's blocks, text and result metadata away, which leaves a
/// consistent projection that has simply lost a page — a smaller answer there
/// is correct, not damage, and demanding a failure would be demanding a
/// refusal with no in-scope scenario (D-2).
#[test]
fn a_missing_page_row_fails_the_read() {
    let _serial = serialize();
    let root = scratch("r3-damage-page");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    match read_damaged(
        &corpus,
        "#inline-tag",
        QueryDialect::Og,
        "page",
        false,
        "DELETE FROM pages WHERE name = ?1",
        rusqlite::params!["refs"],
    ) {
        Err(ResultReadError::Corrupt(_)) => {}
        Err(other) => panic!("expected Corrupt, got {other}"),
        Ok(answer) => panic!(
            "a projection missing a page row answered with {} rows",
            answer.groups.iter().map(|g| g.blocks.len()).sum::<usize>()
        ),
    }
}

// ===== identity =====

/// Rewrite one page's stored result ids to a valid but NON-canonical spelling,
/// and adjust the stored estimate by exactly the identity term.
///
/// This is the shape a live edit leaves behind — a preserved public id that is
/// no longer the structural one — expressed at the level R3a owns. It is
/// STRONGER than a live edit for the arithmetic under test: a live edit
/// preserves a 36-byte canonical UUID, so it could not distinguish a correct
/// identity-term adjustment from one that assumed 36 everywhere.
fn preserve_ids_on_one_page(path: &Path, page: &str) -> ([u8; 16], usize) {
    let writer = rusqlite::Connection::open(path).expect("the copy opens writable");
    let page_id: Vec<u8> = writer
        .query_row(
            "SELECT page_id FROM pages WHERE name = ?1",
            rusqlite::params![page],
            |row| row.get(0),
        )
        .expect("the fixture page exists");
    let rows: Vec<(Vec<u8>, String, i64)> = writer
        .prepare("SELECT block_id, result_id, estimated_bytes FROM query_block_results WHERE page_id = ?1")
        .expect("the metadata is readable")
        .query_map(rusqlite::params![page_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .expect("the metadata is readable")
        .collect::<Result<Vec<_>, _>>()
        .expect("the metadata is readable");
    assert!(!rows.is_empty(), "the fixture page has result metadata");
    let count = rows.len();
    for (block_id, result_id, estimated) in rows {
        // A braced UUID is 38 bytes: valid, preserved, and NOT 36.
        let preserved = format!("{{{result_id}}}");
        let adjusted = estimated - result_id.len() as i64 + preserved.len() as i64;
        writer
            .execute(
                "UPDATE query_block_results SET result_id = ?1, estimated_bytes = ?2 \
                 WHERE block_id = ?3",
                rusqlite::params![preserved, adjusted, block_id],
            )
            .expect("the preserved identity writes");
    }
    (
        page_id.as_slice().try_into().expect("a 16-byte page id"),
        count,
    )
}

#[test]
fn session_pages_keep_their_stored_identity_and_the_estimate_adjustment_is_exact() {
    let _serial = serialize();
    let root = scratch("r3-identity");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let path = copy_projection(&corpus, "identity");
    let (page_id, rows) = preserve_ids_on_one_page(&path, "regex");
    assert!(rows > 0);

    let source = "content regexp 'needle'";
    let (_query, statement) = corpus.lower_block_anchored(source, QueryDialect::Tql);
    let graph_root = corpus.root.clone();
    let recency = recency_for(&graph_root);
    let read = |identity: &ResultIdentity| {
        let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(()))
            .expect("the rewritten copy opens");
        let answer = read_results(
            &mut snapshot,
            &ResultReadInputs {
                statement: &statement,
                order: BackendOrder::Direct,
                identity,
                max_rows: usize::MAX,
                max_bytes: usize::MAX,
                profile: ConstructionProfile::default(),
                recency: &recency,
            },
        );
        snapshot.finish();
        answer
    };

    let walk = walk_answer(
        &corpus,
        source,
        QueryDialect::Tql,
        usize::MAX,
        usize::MAX,
        ConstructionProfile::default(),
    );
    let walk_ids: Vec<String> = walk
        .groups
        .iter()
        .flat_map(|group| group.blocks.iter().map(|block| block.id.clone()))
        .collect();
    assert!(!walk_ids.is_empty(), "the identity fixture matches nothing");

    // The page IS in the session set: its rows keep the PRESERVED id, and the
    // stored estimate (which already describes that id) is used unchanged.
    // Every OTHER page still resolves structurally, which is the same decision
    // taken per page and not per read.
    let mut session = HashSet::new();
    session.insert(page_id);
    let preserved = read(&ResultIdentity::DirectStructural {
        session_pages: Arc::new(session),
        all_session: false,
    })
    .expect("the session-owned read answers");
    let expected: Vec<String> = walk
        .groups
        .iter()
        .flat_map(|group| {
            let braced = group.page == "regex";
            group.blocks.iter().map(move |block| {
                if braced {
                    format!("{{{}}}", block.id)
                } else {
                    block.id.clone()
                }
            })
        })
        .collect();
    assert!(
        expected.iter().any(|id| id.starts_with('{')),
        "the identity fixture must reach the rewritten page"
    );
    assert!(
        expected.iter().any(|id| !id.starts_with('{')),
        "the identity fixture must also reach a page outside the session set"
    );
    let preserved_ids: Vec<String> = preserved
        .groups
        .iter()
        .flat_map(|group| group.blocks.iter().map(|block| block.id.clone()))
        .collect();
    assert_eq!(
        preserved_ids, expected,
        "a session-owned page must return its stored identity"
    );

    // `all_session` takes the same decision for every page at once: the stored
    // id everywhere, which is the rewritten one where it was rewritten.
    let all = read(&ResultIdentity::DirectStructural {
        session_pages: Arc::new(HashSet::new()),
        all_session: true,
    })
    .expect("the all-session read answers");
    assert_eq!(
        all.groups
            .iter()
            .flat_map(|group| group.blocks.iter().map(|block| block.id.clone()))
            .collect::<Vec<_>>(),
        expected
    );

    // The page is NOT in the session set: its rows resolve STRUCTURALLY back
    // to the walk's ids, and the identity-term adjustment (38 bytes out, 36
    // in) has to be exact or `emit_batch`'s estimate check fails the read.
    let structural = read(&ResultIdentity::DirectStructural {
        session_pages: Arc::new(HashSet::new()),
        all_session: false,
    })
    .expect("the fresh-session read answers");
    assert_eq!(
        structural
            .groups
            .iter()
            .flat_map(|group| group.blocks.iter().map(|block| block.id.clone()))
            .collect::<Vec<_>>(),
        walk_ids,
        "a page nobody edited must resolve the structural id"
    );

    // And `Stored` is the Managed policy: always the stored id.
    let stored = read(&ResultIdentity::Stored).expect("the stored read answers");
    assert_eq!(
        stored
            .groups
            .iter()
            .flat_map(|group| group.blocks.iter().map(|block| block.id.clone()))
            .collect::<Vec<_>>(),
        expected
    );
    let _ = std::fs::remove_file(&path);
}

/// A stored estimate smaller than its own identity term is a contradiction,
/// and the checked arithmetic must report it rather than saturate into a
/// smaller budget charge.
#[test]
fn an_impossible_stored_estimate_fails_the_read() {
    let _serial = serialize();
    let root = scratch("r3-estimate");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let path = copy_projection(&corpus, "estimate");
    {
        let writer = rusqlite::Connection::open(&path).expect("the copy opens writable");
        writer
            .execute("UPDATE query_block_results SET estimated_bytes = 0", [])
            .expect("the damage applies");
    }
    let (_query, statement) =
        corpus.lower_block_anchored("content regexp 'needle'", QueryDialect::Tql);
    let graph_root = corpus.root.clone();
    let recency = recency_for(&graph_root);
    let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(()))
        .expect("the rewritten copy opens");
    let answer = read_results(
        &mut snapshot,
        &ResultReadInputs {
            statement: &statement,
            order: BackendOrder::Direct,
            identity: &ResultIdentity::DirectStructural {
                session_pages: Arc::new(HashSet::new()),
                all_session: false,
            },
            max_rows: usize::MAX,
            max_bytes: usize::MAX,
            profile: ConstructionProfile::default(),
            recency: &recency,
        },
    );
    snapshot.finish();
    let _ = std::fs::remove_file(&path);
    assert!(
        matches!(answer, Err(ResultReadError::Corrupt(_))),
        "a zero estimate under a 36-byte identity term must fail the read"
    );
}

// ===== cancellation =====

/// Cancelling BETWEEN payload batches stops the read at the next statement
/// boundary.
///
/// The trigger is a handshake with a real second thread — the owner that holds
/// the cancellation in production — and not a sleep: the reading thread stops
/// at the top of batch 1, hands the canceller the go-ahead, and waits for its
/// acknowledgement, so "batch 0 completed and batch 1 never ran" is a fact and
/// not a timing hope. The handshake is a pair of channels rather than a
/// `Barrier` because a barrier would DEADLOCK the gate if the hook never ran,
/// turning a real regression into a hung suite.
#[test]
fn cancelling_between_batches_stops_the_read_and_releases_the_snapshot() {
    let _serial = serialize();
    let root = scratch("r3-cancel");
    write_huge_page_corpus(&root, 400);
    let corpus = Corpus::open(root, true);
    let (_query, statement) =
        corpus.lower_block_anchored("content match 'filler'", QueryDialect::Tql);
    let graph_root = corpus.root.clone();
    let recency = recency_for(&graph_root);
    let mut snapshot = corpus.snapshot();
    let cancellation = snapshot.cancellation();
    let (trigger, wait_for_trigger) = std::sync::mpsc::channel::<()>();
    let (acknowledge, wait_for_ack) = std::sync::mpsc::channel::<()>();
    let owner = std::thread::spawn(move || {
        if wait_for_trigger.recv().is_ok() {
            cancellation.cancel();
            let _ = acknowledge.send(());
        }
    });
    reset_result_read_census();
    set_before_payload_batch_hook(Some(Box::new(move |batch| {
        if batch == 1 {
            trigger.send(()).expect("the owner thread is listening");
            wait_for_ack.recv().expect("the owner cancels");
        }
    })));
    let answer = read_results(
        &mut snapshot,
        &ResultReadInputs {
            statement: &statement,
            order: BackendOrder::Direct,
            identity: &ResultIdentity::Stored,
            max_rows: usize::MAX,
            max_bytes: usize::MAX,
            profile: ConstructionProfile::default(),
            recency: &recency,
        },
    );
    // Dropping the hook drops the trigger, so the owner thread finishes even
    // when the hook never ran.
    set_before_payload_batch_hook(None);
    owner.join().expect("the owner thread finishes");
    let census = result_read_census();
    assert!(
        matches!(answer, Err(ResultReadError::Cancelled)),
        "a cancelled read must report Cancelled, got {answer:?}"
    );
    assert_eq!(
        census.payload_statements, 3,
        "exactly the first batch's statements ran"
    );
    assert_eq!(census.payload_block_rows, PAYLOAD_BATCH);
    // The snapshot answers nothing further; the owner drops it, which releases
    // the read transaction and the WAL frames it pinned.
    assert!(
        snapshot.run_projection_query("SELECT 1", &[]).is_err(),
        "a cancelled snapshot must not serve another statement"
    );
    snapshot.finish();
}

/// A read cancelled before it starts never touches the projection at all.
#[test]
fn a_cancelled_job_reads_nothing() {
    let _serial = serialize();
    let root = scratch("r3-cancel-early");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let (_query, statement) = corpus.lower_block_anchored("(task TODO)", QueryDialect::Og);
    let graph_root = corpus.root.clone();
    let recency = recency_for(&graph_root);
    let mut snapshot = corpus.snapshot();
    snapshot.cancellation().cancel();
    reset_result_read_census();
    let answer = read_results(
        &mut snapshot,
        &ResultReadInputs {
            statement: &statement,
            order: BackendOrder::Direct,
            identity: &ResultIdentity::Stored,
            max_rows: usize::MAX,
            max_bytes: usize::MAX,
            profile: ConstructionProfile::default(),
            recency: &recency,
        },
    );
    snapshot.finish();
    assert!(matches!(answer, Err(ResultReadError::Cancelled)));
    assert_eq!(result_read_census(), Default::default());
}

// ===== the descriptor statement itself =====

#[test]
fn the_descriptor_statement_wraps_every_lowered_shape() {
    let _serial = serialize();
    let root = scratch("r3-descriptor");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let mut snapshot = corpus.snapshot();
    for (source, dialect) in every_shape() {
        let (_query, statement) = corpus.lower_block_anchored(source, dialect);
        let descriptor = descriptor_statement(&statement, BackendOrder::Direct)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert_eq!(
            descriptor.params, statement.params,
            "{source}: the wrapper binds nothing of its own"
        );
        assert!(
            descriptor.sql.starts_with("WITH "),
            "{source}: the descriptor names its selected relation"
        );
        assert!(
            !descriptor.sql.contains("FROM blocks b JOIN pages p")
                && !descriptor.sql.contains("FROM m JOIN pages p"),
            "{source}: the routing INNER JOIN must be replaced by a LEFT JOIN"
        );
        assert!(
            descriptor
                .sql
                .contains("LEFT JOIN pages p ON p.page_id = r.page_id"),
            "{source}: the descriptor read re-joins pages itself, LEFT"
        );
        snapshot
            .set_query_regex_predicate(statement.regexes.predicate())
            .expect("the regex table installs");
        snapshot
            .run_projection_query(&descriptor.sql, &descriptor.params)
            .unwrap_or_else(|error| panic!("{source}: the descriptor statement must run: {error}"));
    }
    snapshot.finish();
}

#[test]
fn a_page_anchored_statement_has_no_block_descriptor() {
    let _serial = serialize();
    let root = scratch("r3-page-anchor");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let (anchor, statement) = corpus.lower(
        "@page and name like 'proj/%'",
        QueryDialect::Tql,
        false,
        &[],
    );
    assert_eq!(anchor, crate::query::ir::Anchor::Page);
    assert!(
        descriptor_statement(&statement, BackendOrder::Direct).is_err(),
        "a page-anchored statement has no block descriptor read"
    );
}

// ===== the estimator reconciliation =====

/// `query.rs::shallow_dto_estimated_bytes` (over a `DocBlock`) and
/// `model.rs::block_dto_estimated_bytes` (over the emitted `BlockDto`) must
/// compute the SAME number for a shallow result row.
///
/// They do, and the reason they can: with no ancestors the first is
/// `tine_storage::query_result_estimated_bytes` over `(uuid, raw, tags,
/// properties)`, and the second is the same four terms plus a `+128` and two
/// empty vectors. Their ONE divergence is the EMPTY id, where the storage
/// producer substitutes 36 — and an emitted shallow DTO never has one, which
/// `read_results` enforces by failing an empty stored `result_id`. So the
/// database path charges the budget with the storage estimate and verifies the
/// DTO estimate; this gate is what keeps that identity true.
#[test]
fn the_two_shallow_estimators_agree_on_every_corpus_block() {
    let _serial = serialize();
    let root = scratch("r3-estimators");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let mut compared = 0usize;
    GraphQueryPages(&corpus.graph).for_each_page(&mut |page| {
        fn visit(blocks: &[crate::doc::DocBlock], compared: &mut usize) {
            for block in blocks {
                let dto = crate::model::block_to_shallow_dto(block);
                assert_eq!(
                    crate::query::shallow_dto_estimated_bytes(block, &[]),
                    block_dto_estimated_bytes(&dto),
                    "the two shallow estimators disagree"
                );
                *compared += 1;
                visit(&block.children, compared);
            }
        }
        visit(page.roots, &mut compared);
        std::ops::ControlFlow::Continue(())
    });
    assert!(compared > 0, "the corpus has no blocks to compare");
}

// ===== a page kind is a page kind =====

#[test]
fn journals_and_pages_keep_their_kind() {
    let _serial = serialize();
    let root = scratch("r3-kinds");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let answer = read_answer(
        &corpus,
        "[[Project]]",
        QueryDialect::Og,
        usize::MAX,
        usize::MAX,
        ConstructionProfile::default(),
        &ResultIdentity::Stored,
    )
    .expect("the read answers");
    assert!(
        answer
            .groups
            .iter()
            .any(|group| group.kind == PageKind::Journal),
        "the fixture's journal must reach the answer as a journal"
    );
    assert!(
        answer
            .groups
            .iter()
            .any(|group| group.kind == PageKind::Page),
        "the fixture's ordinary pages must reach the answer as pages"
    );
}

/// The common constructor consumes one caller-owned projection snapshot. The
/// concurrent Managed adapter may still name the retired merged API in this
/// packet's base, but the common reader itself must not retain source arrays,
/// source indices, or descriptor buffering for them.
#[test]
fn the_common_result_reader_has_one_snapshot_and_no_source_buffer() {
    let source = include_str!("results.rs");
    let code = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for retired in [
        "read_results_merged",
        "read_page_results_merged",
        "read_located_results_merged",
        "ResultReadShared",
        "ResultSource",
        "PageSource",
        "BufferedDescriptor",
        "locator.source",
    ] {
        assert!(
            !code.contains(retired),
            "common reader still names `{retired}`"
        );
    }
    assert!(code.contains("pub(crate) fn read_results("));
    assert!(code.contains("pub(crate) fn read_page_results("));
    assert!(code.contains("pub(crate) fn read_located_results("));
}

// ===== RET1: the PUBLIC IR commands answer from the database =====
//
// The gates above prove the shared result CONSTRUCTOR equals the walk. These
// prove the two shipped commands — SPEC §7.1 `query_run` and
// `query_explain_empty`, reached through `run_query_result_ir` and
// `explain_empty_query` — actually go through it: same complete answer, an
// actual statement read, and NO traversal of the parsed graph.
//
// The walk is still the oracle and is still built here, so every measurement is
// taken BEFORE the oracle runs: constructing `GraphQueryPages` is what a
// reconnection would look like, and the counters cannot tell the two apart
// after the fact.

/// One public execution, measured cold, with everything a walk would leave
/// behind counted beside its answer.
struct PublicRun<T> {
    answer: T,
    /// Query jobs opened on the projection. A dispatched read opens exactly
    /// one; a lowering that folded to `matches_nothing` needs none.
    statement_reads: u64,
    /// `collect_pred_bounded_over` / `collect_page_rows_over` entries — the
    /// walk's own instrumentation. RET1's claim is that this stays 0.
    walks: u64,
    /// Page documents the projection-side readers loaded from the parsed
    /// cache. A database read contributes nothing here (R3).
    hydrated: usize,
}

fn measured<T>(corpus: &Corpus, run: impl FnOnce(&crate::model::Graph) -> T) -> PublicRun<T> {
    // Every request computes its answer; count SQLite work and oracle/page reads separately.
    corpus.graph.reset_direct_projection_candidate_probe_test();
    let before = corpus.graph.direct_projection_statement_reads_test();
    let answer = run(&corpus.graph);
    PublicRun {
        answer,
        statement_reads: corpus
            .graph
            .direct_projection_statement_reads_test()
            .saturating_sub(before),
        walks: crate::query::full_graph_query_evaluations(),
        hydrated: corpus.graph.direct_projection_hydrated_pages_test().len(),
    }
}

fn wire(value: &impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).expect("the IR result serializes")
}

/// The bounds edges §4 names for the public route: unbounded, zero rows, one
/// row (the `@page` loop decides `exceeded` on the row AFTER the cap, so zero
/// and one are different code paths), and zero bytes.
fn public_bounds() -> Vec<Bounds> {
    vec![
        Bounds {
            max_rows: usize::MAX,
            max_bytes: usize::MAX,
        },
        Bounds {
            max_rows: 0,
            max_bytes: usize::MAX,
        },
        Bounds {
            max_rows: 1,
            max_bytes: usize::MAX,
        },
        Bounds {
            max_rows: usize::MAX,
            max_bytes: 0,
        },
    ]
}

/// Compare one public `query_run` against the walk, and account for how it was
/// answered. Returns the difference lines and whether the answer had rows.
fn public_run_differences(
    corpus: &Corpus,
    source: &str,
    dialect: QueryDialect,
    context: &ExecutionContext,
    bounds: Bounds,
) -> (Vec<String>, Option<crate::query::ir::Anchor>) {
    let today = JournalDate::today();
    let (query, view) = crate::query::parse_query_text(source, dialect, today);
    let label = format!(
        "{source} rows={} bytes={}",
        bounds.max_rows, bounds.max_bytes
    );
    let mut differences = Vec::new();

    let run = measured(corpus, |graph| {
        crate::query::run_query_result_ir(graph, &query, &view, bounds, context)
            .expect("the ready projection answers the public IR route")
    });

    // The oracle, built only now: a `GraphQueryPages` constructed before the
    // measurement is exactly the reconnection this gate exists to catch.
    let resolved = crate::query::resolve_for_execution(&query, context, today);
    let walked = crate::query::run_resolved_query_result_over(
        &GraphQueryPages(&corpus.graph),
        &resolved,
        &view,
        bounds,
    );

    let (rows, anchor) = match &walked.rows {
        crate::query::ir::QueryRows::Block { groups } => (
            groups.iter().map(|group| group.blocks.len()).sum::<usize>(),
            crate::query::ir::Anchor::Block,
        ),
        crate::query::ir::QueryRows::Page { pages } => {
            (pages.len(), crate::query::ir::Anchor::Page)
        }
    };
    if wire(&run.answer) != wire(&walked) {
        differences.push(format!(
            "{label}: the public result differs from the walk (walk rows={rows}, \
             public total={}, walk total={})",
            run.answer.total, walked.total
        ));
    }
    if run.walks != 0 {
        differences.push(format!(
            "{label}: the public result walked the graph {} time(s)",
            run.walks
        ));
    }
    if run.hydrated != 0 {
        differences.push(format!(
            "{label}: the public result hydrated {} page document(s)",
            run.hydrated
        ));
    }
    // A non-empty answer cannot come from a `matches_nothing` lowering, so it
    // proves a statement actually ran rather than merely that no walk did.
    if rows > 0 && run.statement_reads != 1 {
        differences.push(format!(
            "{label}: a {rows}-row answer opened {} query job(s), expected 1",
            run.statement_reads
        ));
    }
    (differences, (rows > 0).then_some(anchor))
}

/// The same, for `query_explain_empty`. Every probe of one explanation must be
/// counted from ONE job, so the read budget here is exactly one as well.
fn public_explain_differences(
    corpus: &Corpus,
    source: &str,
    dialect: QueryDialect,
    context: &ExecutionContext,
    bounds: Bounds,
) -> Vec<String> {
    let today = JournalDate::today();
    let (query, view) = crate::query::parse_query_text(source, dialect, today);
    let label = format!("explain {source}");
    let mut differences = Vec::new();

    let explained = measured(corpus, |graph| {
        crate::query::explain_empty_query(graph, &query, &view, bounds, context)
            .expect("the ready projection answers the public explanation")
    });

    let resolved = crate::query::resolve_for_execution(&query, context, today);
    let walked = crate::query::view::explain_empty(
        &GraphQueryPages(&corpus.graph),
        &resolved,
        &view,
        bounds,
    );

    if explained.answer != walked {
        differences.push(format!(
            "{label}: the explanation differs from the walk's\n  public: {:?}\n  walk:   {:?}",
            explained.answer, walked
        ));
    }
    if explained.walks != 0 {
        differences.push(format!(
            "{label}: the explanation walked the graph {} time(s)",
            explained.walks
        ));
    }
    if explained.hydrated != 0 {
        differences.push(format!(
            "{label}: the explanation hydrated {} page document(s)",
            explained.hydrated
        ));
    }
    // Every probe shares one snapshot. An explanation whose conjuncts came from
    // N separately-timed reads describes N graph states.
    if explained.statement_reads > 1 {
        differences.push(format!(
            "{label}: the explanation opened {} query jobs; every probe of one \
             explanation shares one snapshot",
            explained.statement_reads
        ));
    }
    if !walked.rows.is_empty() && explained.statement_reads != 1 {
        differences.push(format!(
            "{label}: a {}-conjunct explanation opened {} query job(s), expected 1",
            walked.rows.len(),
            explained.statement_reads
        ));
    }
    differences
}

/// **RET1's acceptance bar for `query_run` over Direct Files.** Every shape §5
/// names — block and page anchors, refs, tags, tasks, properties, `content
/// match`, regexes, nested `refs`, page relations — under the row and byte
/// edges, answered by the database and equal to the walk row for row.
#[test]
fn the_public_ir_result_equals_the_walk_and_reads_the_database() {
    let _serial = serialize();
    let root = scratch("ret1-public-run");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let context = ExecutionContext::none();
    // The two view directives that change what is CONSTRUCTED rather than how
    // it is displayed: an unsorted `(sample N)` stops admission at N, and a
    // recency sort measures the per-result-page axis. Only OG spells them —
    // TQL has no view clause — and neither may reach a `@page` answer.
    let mut shapes = every_shape()
        .into_iter()
        .map(|(source, dialect)| (source.to_string(), dialect))
        .collect::<Vec<_>>();
    let with_views = shapes
        .iter()
        .filter(|(_, dialect)| *dialect == QueryDialect::Og)
        .flat_map(|(source, dialect)| {
            [
                (format!("{source} (sample 3)"), *dialect),
                (format!("{source} (sort-by modified desc)"), *dialect),
            ]
        })
        .collect::<Vec<_>>();
    shapes.extend(with_views);

    let mut differences = Vec::new();
    let mut answered_blocks = 0usize;
    let mut answered_pages = 0usize;
    for (source, dialect) in &shapes {
        let (source, dialect) = (source.as_str(), *dialect);
        for bounds in public_bounds() {
            let (lines, answered) =
                public_run_differences(&corpus, source, dialect, &context, bounds);
            differences.extend(lines);
            match answered {
                Some(crate::query::ir::Anchor::Block) => answered_blocks += 1,
                Some(crate::query::ir::Anchor::Page) => answered_pages += 1,
                None => {}
            }
        }
    }
    // Both anchors have to be exercised with actual rows: the `@page` read is a
    // different statement, a different row reader and a different budget, and a
    // gate that only ever saw empty page answers would prove nothing about it.
    assert!(
        answered_blocks > 100 && answered_pages > 10,
        "the corpus answered {answered_blocks} block and {answered_pages} page \
         executions with rows; the gate proves nothing about a read it never made"
    );
    assert!(
        differences.is_empty(),
        "the public IR result is not the database's answer:\n{}",
        differences.join("\n")
    );
}

/// The same bar for `query_explain_empty`, over the same corpus and the same
/// shapes: same explanation, one snapshot, no walk.
#[test]
fn the_public_ir_explanation_equals_the_walk_and_reads_the_database() {
    let _serial = serialize();
    let root = scratch("ret1-public-explain");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let context = ExecutionContext::none();
    let bounds = Bounds {
        max_rows: usize::MAX,
        max_bytes: usize::MAX,
    };
    let mut differences = Vec::new();
    for (source, dialect) in every_shape() {
        differences.extend(public_explain_differences(
            &corpus, source, dialect, &context, bounds,
        ));
    }
    assert!(
        differences.is_empty(),
        "the public IR explanation is not the database's answer:\n{}",
        differences.join("\n")
    );
}

/// A corpus for the inputs the shape tables do not carry: an Org page beside
/// the Markdown ones, two PHYSICAL pages that share one display name, a journal
/// day around today, and blocks a `?current-page` / date-input advanced query
/// can select.
fn write_ret1_context_corpus(root: &Path) {
    let today = JournalDate::today();
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    std::fs::create_dir_all(root.join("journals")).expect("journals");

    std::fs::write(
        root.join("pages/one.md"),
        "- TODO one mentions [[target]]\n\
         \t- child under one\n\
         - DONE two\n",
    )
    .expect("one");
    std::fs::write(
        root.join("pages/target.md"),
        "- the target block\n  status:: active\n- another target block\n",
    )
    .expect("target");

    // The Org half. The parser types this page by extension, so every leaf a
    // shape names — marker, priority, ref, nesting — has to answer identically
    // through a different reader.
    std::fs::write(
        root.join("pages/org-notes.org"),
        "* TODO [#B] org task mentions [[target]]\n\
         ** DONE org child\n\
         * LATER org later\n",
    )
    .expect("org notes");

    // Two PHYSICAL pages carrying one display name. `@page` must answer with
    // both rows; a read that de-duplicated on the name would silently lose one.
    std::fs::write(
        root.join("pages/dup-a.md"),
        "title:: Shared Title\n\n- TODO from the first file\n",
    )
    .expect("dup a");
    std::fs::write(
        root.join("pages/dup-b.md"),
        "title:: Shared Title\n\n- TODO from the second file\n",
    )
    .expect("dup b");

    std::fs::write(
        root.join(format!("journals/{}.md", today.file_stem())),
        "- TODO today mentions [[target]]\n",
    )
    .expect("today");
    std::fs::write(
        root.join(format!("journals/{}.md", today.add_days(-3).file_stem())),
        "- TODO three days ago\n",
    )
    .expect("earlier");
    std::fs::write(
        root.join(format!("journals/{}.md", today.add_days(-40).file_stem())),
        "- TODO forty days ago\n",
    )
    .expect("older");
}

/// The advanced sources whose answer is a function of WHERE and WHEN the
/// execution happens (§4.4): `?current-page` bound from the execution context,
/// and a `(between …)` bound from relative date inputs resolved against the
/// execution day. RET1 must resolve both ONCE, at execution, and hand the
/// bound tree to the compiler — not fold them in at parse time.
const RET1_ADVANCED_SOURCES: &[&str] = &[
    r#"{:query [:find (pull ?b [*])
                :in $ ?current-page
                :where
                [?p :block/name ?current-page]
                [?b :block/refs ?p]]
        :inputs [:current-page]}"#,
    r#"[:find (pull ?b [*])
        :in $ ?start ?end
        :where (between ?b ?start ?end)]
       :inputs [:-7d :today]"#,
    // The unsupported half of the same surface: an advanced source whose
    // clause nothing lowers must still report `supported = false` and the same
    // `ignored` list through the database route.
    r#"[:find (pull ?b [*])
        :where [?b :block/unknown-attribute "x"]]"#,
];

/// The ONE text -> IR entry the commands use, per input grammar (§7.1, C3).
/// An advanced form reaches it as `QueryInput::Advanced`; handing the same text
/// to the OG parser would produce a refusal, not a datalog query.
fn ret1_parse(source: &str, input: QueryInput, today: JournalDate) -> (Query, ViewSettings) {
    crate::query::parse_query_input(
        source,
        input,
        today,
        crate::query::registry::Registry::none(),
    )
}

fn ret1_context_shapes() -> Vec<(String, QueryInput)> {
    let mut shapes = vec![
        // block anchor, both formats
        ("(task TODO)".to_string(), QueryInput::Og),
        ("(task TODO DOING LATER)".to_string(), QueryInput::Og),
        ("(priority B)".to_string(), QueryInput::Og),
        ("[[target]]".to_string(), QueryInput::Og),
        ("(property status active)".to_string(), QueryInput::Og),
        ("(content-regex \"org\")".to_string(), QueryInput::Og),
        (
            "any(children, content like '%child%')".to_string(),
            QueryInput::Tql,
        ),
        ("(journal)".to_string(), QueryInput::Og),
        // a sorted sample: ranking over the constructed rows
        (
            "(task TODO) (sort-by modified desc) (sample 2)".to_string(),
            QueryInput::Og,
        ),
        ("(task TODO) (sample 2)".to_string(), QueryInput::Og),
        // page anchor, including the duplicate display name and the journal
        // metadata a page row carries
        (
            "@page and name = 'shared title'".to_string(),
            QueryInput::Tql,
        ),
        ("@page and journal = true".to_string(), QueryInput::Tql),
        ("@page and journal = false".to_string(), QueryInput::Tql),
        ("@page and day is not null".to_string(), QueryInput::Tql),
        ("@page and name like 'org-%'".to_string(), QueryInput::Tql),
    ];
    shapes.extend(
        RET1_ADVANCED_SOURCES
            .iter()
            .map(|source| ((*source).to_string(), QueryInput::Advanced)),
    );
    shapes
}

/// Wait for a Direct projection to converge, the way `Corpus::open` does.
fn ret1_wait_ready(graph: &crate::model::Graph) {
    let started = std::time::Instant::now();
    while !graph.direct_projection_ready_test() {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(120),
            "the Direct Files projection did not converge"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// **RET1's warm-reopen bar.** A session that reopens a valid projection has no
/// parsed cache at all, and both public IR commands still answer — from the
/// database, with the walk's exact rows, and WITHOUT parsing the graph.
///
/// This is the property the fail-before probe in `direct_projection.rs` states
/// for one query; here it is stated for both anchors, both file formats, the
/// duplicate display name, the journal metadata, a sorted sample, and the two
/// execution-time bindings (`?current-page` and a relative date input).
///
/// The oracle is a SECOND graph opened on the same directory with no projection
/// attached, so the measured session never has to parse anything to be checked.
#[test]
fn the_public_ir_route_answers_a_warm_reopen_without_parsing() {
    let _serial = serialize();
    let root = scratch("ret1-warm-reopen");
    write_ret1_context_corpus(&root);
    let database = scratch("ret1-warm-reopen-db").join("projection.sqlite");

    {
        let graph = crate::model::Graph::open(&root);
        graph
            .attach_direct_projection(database.clone())
            .expect("the projection worker starts");
        graph.warm_cache();
        ret1_wait_ready(&graph);
    }

    let graph = crate::model::Graph::open(&root);
    graph
        .attach_direct_projection(database.clone())
        .expect("the projection worker starts");
    graph.warm_cache();
    ret1_wait_ready(&graph);
    assert!(
        !graph.has_parsed_cache_test(),
        "a valid projection must be adopted from file bytes alone"
    );

    // The independent oracle: a fully parsed session with no projection.
    let oracle = crate::model::Graph::open(&root);
    oracle.warm_cache();

    let today = JournalDate::today();
    let bounds = Bounds {
        max_rows: 100,
        max_bytes: 1_000_000,
    };
    let contexts = [
        ExecutionContext::none(),
        ExecutionContext::on_page("target"),
        ExecutionContext::on_page("one"),
    ];

    // RET2: a `props`-sensitive memo stamps itself with the CURRENT property
    // registry generation, and that generation is now read from SQL instead of
    // by walking parsed documents. On a warm reopen nothing is published yet,
    // so the FIRST such query pays one extra owned read and every later one
    // takes the published fast path. That one-time per-session cost is not the
    // per-execution cost this gate measures, so it is paid here explicitly —
    // through the same public command the frontend uses.
    graph
        .query_registry_snapshot_ready()
        .expect("the reopened projection answers the public registry read");

    let mut differences = Vec::new();
    let mut answered = 0usize;
    for (source, input) in ret1_context_shapes() {
        let (query, view) = ret1_parse(&source, input, today);
        for context in &contexts {
            let label = format!("{source} on {:?}", context.current_page);

            graph.reset_direct_projection_candidate_probe_test();
            let before = graph.direct_projection_statement_reads_test();
            let result = crate::query::run_query_result_ir(&graph, &query, &view, bounds, context)
                .expect("the ready projection answers the public IR route");
            let explained =
                crate::query::explain_empty_query(&graph, &query, &view, bounds, context)
                    .expect("the ready projection answers the public explanation");
            let statement_reads = graph.direct_projection_statement_reads_test() - before;
            let walks = crate::query::full_graph_query_evaluations();
            let hydrated = graph.direct_projection_hydrated_pages_test().len();

            if graph.has_parsed_cache_test() {
                differences.push(format!("{label}: the public route parsed the graph"));
            }
            if walks != 0 {
                differences.push(format!("{label}: the public route walked {walks} time(s)"));
            }
            if hydrated != 0 {
                differences.push(format!(
                    "{label}: the public route hydrated {hydrated} page(s)"
                ));
            }

            let resolved = crate::query::resolve_for_execution(&query, context, today);
            let walked = crate::query::run_resolved_query_result_over(
                &GraphQueryPages(&oracle),
                &resolved,
                &view,
                bounds,
            );
            let walked_explained = crate::query::view::explain_empty(
                &GraphQueryPages(&oracle),
                &resolved,
                &view,
                bounds,
            );
            let rows = match &walked.rows {
                crate::query::ir::QueryRows::Block { groups } => {
                    groups.iter().map(|group| group.blocks.len()).sum::<usize>()
                }
                crate::query::ir::QueryRows::Page { pages } => pages.len(),
            };
            if rows > 0 {
                answered += 1;
                // One job for the result and one for the explanation: a
                // non-empty answer cannot have come from a folded-away lowering.
                if statement_reads != 2 {
                    differences.push(format!(
                        "{label}: a {rows}-row answer plus its explanation opened \
                         {statement_reads} query job(s), expected 2"
                    ));
                }
            }
            if wire(&result) != wire(&walked) {
                differences.push(format!(
                    "{label}: the result differs from the walk's\n  public: {}\n  walk:   {}",
                    wire(&result),
                    wire(&walked)
                ));
            }
            if explained != walked_explained {
                differences.push(format!(
                    "{label}: the explanation differs from the walk's\n  public: {explained:?}\n  \
                     walk:   {walked_explained:?}"
                ));
            }
        }
    }

    // The duplicate display name, against an oracle that is not the walk: the
    // page read must return one row per PHYSICAL page, not one per name.
    let physical_shared = oracle
        .list_pages()
        .into_iter()
        .filter(|entry| entry.name.eq_ignore_ascii_case("shared title"))
        .count();
    let (query, view) = ret1_parse("@page and name = 'shared title'", QueryInput::Tql, today);
    let shared =
        crate::query::run_query_result_ir(&graph, &query, &view, bounds, &ExecutionContext::none())
            .expect("the ready projection answers the public IR route");
    let shared_rows = match &shared.rows {
        crate::query::ir::QueryRows::Page { pages } => pages.len(),
        other => panic!("a `@page` query must answer with page rows, got {other:?}"),
    };
    assert_eq!(
        physical_shared, 2,
        "the fixture must write two physical pages under one display name"
    );
    assert_eq!(
        shared_rows, physical_shared,
        "the page read returned {shared_rows} row(s) for {physical_shared} physical page(s) \
         sharing one display name"
    );

    // The two execution-time bindings, asserted directly rather than only
    // through the parity comparison: a route that ignored the context or the
    // day would agree with a walk that ignored them too.
    let rows_of = |source: &str, input: QueryInput, context: &ExecutionContext| {
        let (query, view) = ret1_parse(source, input, today);
        match crate::query::run_query_result_ir(&graph, &query, &view, bounds, context)
            .expect("the ready projection answers the public IR route")
            .rows
        {
            crate::query::ir::QueryRows::Block { groups } => {
                groups.iter().map(|group| group.blocks.len()).sum::<usize>()
            }
            crate::query::ir::QueryRows::Page { pages } => pages.len(),
        }
    };
    let current_page = RET1_ADVANCED_SOURCES[0];
    assert_eq!(
        rows_of(
            current_page,
            QueryInput::Advanced,
            &ExecutionContext::none()
        ),
        0,
        "an unbound `?current-page` leaves its clause unsupported (§4.4), so it \
         selects nothing"
    );
    assert!(
        rows_of(
            current_page,
            QueryInput::Advanced,
            &ExecutionContext::on_page("target")
        ) > 0,
        "`?current-page` bound to `target` must select the blocks that reference it"
    );
    assert_ne!(
        rows_of(
            current_page,
            QueryInput::Advanced,
            &ExecutionContext::on_page("target")
        ),
        rows_of(
            current_page,
            QueryInput::Advanced,
            &ExecutionContext::on_page("one")
        ),
        "two different current pages must produce two different answers"
    );
    let dated = RET1_ADVANCED_SOURCES[1];
    let within_a_week = rows_of(dated, QueryInput::Advanced, &ExecutionContext::none());
    assert!(
        within_a_week > 0,
        "the `:-7d`/`:today` inputs must resolve against the execution day and \
         select the journal blocks inside that window"
    );
    assert!(
        within_a_week < rows_of("(journal)", QueryInput::Og, &ExecutionContext::none()),
        "the date window must exclude the journal day outside it; a route that \
         resolved no date at all would select every journal block"
    );

    assert!(
        answered > 10,
        "only {answered} execution(s) returned rows; the gate proves little about the read"
    );
    assert!(
        differences.is_empty(),
        "the warm-reopened public IR route is not the database's answer:\n{}",
        differences.join("\n")
    );
    assert!(
        !graph.has_parsed_cache_test(),
        "no public execution above may have parsed the graph"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(database.parent().expect("the database has a directory"));
}

/// **RET1 preserves §3.5 and §4.4's refusal semantics.** An INVALID query and
/// an advanced query nothing lowers both answer ZERO rows with their
/// diagnostics and their support report — not a truncated answer, not a table
/// of zeroes that reads like a result — and neither reaches the parsed graph to
/// find that out.
#[test]
fn the_public_ir_route_refuses_without_walking_and_keeps_its_report() {
    let _serial = serialize();
    let root = scratch("ret1-refusals");
    write_ret1_context_corpus(&root);
    let corpus = Corpus::open(root, true);
    let bounds = Bounds {
        max_rows: 100,
        max_bytes: 1_000_000,
    };
    let context = ExecutionContext::none();
    let today = JournalDate::today();

    // `diagnostics` says whether the refusal is REPORTED as one; an invalid
    // regex is a FALSE LEAF (§3.4/§4.3.2) rather than a refusal, so it matches
    // nothing and says nothing.
    for (label, source, input, supported, diagnostics) in [
        // §3.5: a syntactically valid form whose leaf does not apply. The whole
        // query is invalid, so it returns zero results plus its diagnostics —
        // never a truncated answer.
        (
            "an inapplicable leaf",
            "@page and day is not null",
            QueryInput::Tql,
            true,
            true,
        ),
        // §4.3.2: an invalid regex is a false leaf, not a crash.
        (
            "an invalid regex",
            "(content-regex \"[\")",
            QueryInput::Og,
            true,
            false,
        ),
        // §4.4: an advanced clause nothing lowers is REFUSED, and says so.
        (
            "an unlowerable advanced clause",
            RET1_ADVANCED_SOURCES[2],
            QueryInput::Advanced,
            false,
            true,
        ),
    ] {
        let (query, view) = ret1_parse(source, input, today);
        let run = measured(&corpus, |graph| {
            crate::query::run_query_result_ir(graph, &query, &view, bounds, &context)
                .expect("the ready projection answers the public IR route")
        });
        let explained = measured(&corpus, |graph| {
            crate::query::explain_empty_query(graph, &query, &view, bounds, &context)
                .expect("the ready projection answers the public explanation")
        });

        let rows = match &run.answer.rows {
            crate::query::ir::QueryRows::Block { groups } => {
                groups.iter().map(|group| group.blocks.len()).sum::<usize>()
            }
            crate::query::ir::QueryRows::Page { pages } => pages.len(),
        };
        assert_eq!(rows, 0, "{label}: a refusal has no rows");
        assert_eq!(run.answer.total, 0, "{label}: a refusal counts nothing");
        assert!(
            !run.answer.exceeded,
            "{label}: a refusal is not over budget"
        );
        assert_eq!(
            run.answer.report.supported, supported,
            "{label}: the support report travels with the answer"
        );
        assert_eq!(
            run.walks, 0,
            "{label}: a refusal must not walk the graph to produce no rows"
        );
        assert_eq!(explained.walks, 0, "{label}: nor must its explanation");

        // The diagnostics are the ANSWER for a refusal: without them an empty
        // result reads as "nothing matched" instead of "this was refused".
        assert_eq!(
            !run.answer.diagnostics.is_empty(),
            diagnostics,
            "{label}: diagnostics travel with the answer exactly when there are any"
        );
        assert_eq!(
            explained.answer.report, run.answer.report,
            "{label}: the explanation reports the same binding as the result"
        );
        assert_eq!(
            explained.answer.diagnostics, run.answer.diagnostics,
            "{label}: the explanation carries the result's diagnostics"
        );

        // And the same answer the walk would have given, so the refusal is the
        // engine's shared rule and not a database-route shortcut.
        let resolved = crate::query::resolve_for_execution(&query, &context, today);
        // An execution that was never bound has nothing honest to count; one
        // that WAS bound and simply matches nothing still explains itself.
        assert_eq!(
            explained.answer.rows.is_empty(),
            !resolved.is_executable(),
            "{label}: only an unbound execution has no conjunct rows"
        );
        let walked = crate::query::run_resolved_query_result_over(
            &GraphQueryPages(&corpus.graph),
            &resolved,
            &view,
            bounds,
        );
        assert_eq!(
            wire(&run.answer),
            wire(&walked),
            "{label}: the refusal differs from the walk's"
        );
    }
}

#[test]
fn fts_readiness_uses_owned_image_and_propagates_cancellation() {
    let root = scratch("fts-owned-image");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("projection.sqlite");
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE search_fts_build(singleton INTEGER PRIMARY KEY, phase INTEGER); INSERT INTO search_fts_build VALUES(1, 1);").unwrap();
    let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
    assert!(crate::query::results::probe_fts_ready(&mut snapshot).unwrap());
    writer
        .execute("UPDATE search_fts_build SET phase = 0", [])
        .unwrap();
    assert!(crate::query::results::probe_fts_ready(&mut snapshot).unwrap());
    let mut newer = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
    assert!(!crate::query::results::probe_fts_ready(&mut newer).unwrap());
    snapshot.cancellation().cancel();
    assert!(matches!(
        crate::query::results::probe_fts_ready(&mut snapshot),
        Err(ResultReadError::Cancelled)
    ));
    drop(newer);
    drop(snapshot);
    drop(writer);
    std::fs::remove_dir_all(root).unwrap();
}
