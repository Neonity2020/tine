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
use crate::query::ir::{Anchor, Bounds, ExecutionContext, Field, SortDir};
use crate::query::ir::{Query, ViewSettings};
use crate::query::rank::{JournalRankInput, PageRecencyPrograms, QueryRankPrograms};
use crate::query::results::{
    read_page_results, read_results, reset_result_read_census, result_read_census,
    set_before_page_payload_batch_hook, set_before_payload_batch_hook, BackendOrder,
    PageReadInputs, RecencyPage, ResultIdentity, ResultReadError, ResultReadInputs, PAYLOAD_BATCH,
};
use crate::query::sql::sql_gates_tests::{
    scratch, serialize, write_fast_corpus, Corpus, CONTENT_PLAN_SHAPES, IDENTITY_SHAPES,
    PLAN_SHAPES,
};
use crate::query::sql::{descriptor_statement, ContentPlan};
use crate::query::{
    collect_page_rows_over, collect_pred_bounded_over, page_recency_secs_for, ConstructionProfile,
    GraphQueryPages, PreViewGroups, QueryDialect, QueryInput, QueryPageSource,
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

fn page_recency_for(root: &Path) -> PageRecencyPrograms {
    let file_root = root.to_path_buf();
    PageRecencyPrograms::new(
        JournalRankInput::StoredDay,
        |day| page_recency_secs_for(day.parse::<i64>().ok(), Path::new("")),
        move |path| page_recency_secs_for(None, &file_root.join(path)),
    )
}

fn database_page_answer(
    corpus: &Corpus,
    source: &str,
    view: &ViewSettings,
    max_rows: usize,
    max_bytes: usize,
) -> Result<crate::query::results::PageAnswer, ResultReadError> {
    let (anchor, statement) = corpus.lower(source, QueryDialect::Tql, corpus.fts_ready());
    assert_eq!(
        anchor,
        Anchor::Page,
        "the fixture query stays page-anchored"
    );
    let recency = page_recency_for(&corpus.root);
    let mut snapshot = corpus.snapshot();
    let answer = read_page_results(
        &mut snapshot,
        &PageReadInputs {
            statement: &statement,
            order: BackendOrder::Direct,
            view,
            max_rows,
            max_bytes,
            recency: &recency,
        },
    );
    snapshot.finish();
    answer
}

fn oracle_page_answer(
    corpus: &Corpus,
    source: &str,
    view: &ViewSettings,
    max_rows: usize,
    max_bytes: usize,
) -> crate::query::results::PageAnswer {
    let (query, _) = crate::query::parse_query_text(source, QueryDialect::Tql, corpus.today());
    collect_page_rows_over(
        &GraphQueryPages(&corpus.graph),
        &query,
        view,
        corpus.today(),
        Bounds {
            max_rows,
            max_bytes,
        },
    )
}

fn assert_page_answers_equal(
    expected: &crate::query::results::PageAnswer,
    actual: &crate::query::results::PageAnswer,
) {
    assert_eq!(actual.pages, expected.pages);
    assert_eq!(actual.total, expected.total);
    assert_eq!(actual.matched_total, expected.matched_total);
    assert_eq!(actual.exceeded, expected.exceeded);
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

#[test]
fn operation_snapshot_driver_reuses_readers_without_retaining_answers() {
    use crate::query::read_execute::{SnapshotQueryInputs, SnapshotQueryReader};
    let _serial = serialize();
    let root = scratch("publication-snapshot-driver");
    write_fast_corpus(&root);
    let corpus = Corpus::open(root, true);
    let registry = corpus.graph.property_registry();
    let identity = ResultIdentity::Stored;
    let recency = recency_for(&corpus.root);
    let page_recency = page_recency_for(&corpus.root);
    let mut snapshot = corpus.snapshot();
    let cancellation = snapshot.cancellation();
    let reader = SnapshotQueryReader::new(
        &mut snapshot,
        SnapshotQueryInputs {
            registry: &registry,
            identity: &identity,
            order: BackendOrder::Direct,
            recency: &recency,
            page_recency: &page_recency,
            today: corpus.today(),
        },
    )
    .unwrap();
    for (source, dialect) in every_shape() {
        let (query, view) = crate::query::parse_query_text(source, dialect, corpus.today());
        for bounds in [
            Bounds {
                max_rows: 20_000,
                max_bytes: 32 << 20,
            },
            Bounds {
                max_rows: 1,
                max_bytes: 256,
            },
        ] {
            let expected = crate::query::run_query_result_over(
                &GraphQueryPages(&corpus.graph),
                &query,
                &view,
                corpus.today(),
                bounds,
            );
            let actual = reader
                .run(&query, &view, bounds, &ExecutionContext::none())
                .unwrap();
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(expected).unwrap(),
                "{source}"
            );
        }
    }
    let (query, view) =
        crate::query::parse_query_text("(task TODO)", QueryDialect::Og, corpus.today());
    let bounds = Bounds {
        max_rows: 20_000,
        max_bytes: 32 << 20,
    };
    reset_result_read_census();
    let first = reader
        .run(&query, &view, bounds, &ExecutionContext::none())
        .unwrap();
    let first_read_rows = result_read_census().descriptor_rows;
    assert!(first_read_rows > 0);
    let second = reader
        .run(&query, &view, bounds, &ExecutionContext::none())
        .unwrap();
    assert_eq!(
        result_read_census().descriptor_rows,
        first_read_rows * 2,
        "each use runs SQL"
    );
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(second).unwrap()
    );
    cancellation.cancel();
    assert!(matches!(
        reader.run(&query, &view, bounds, &ExecutionContext::none()),
        Err(crate::query::QueryExecutionError::Cancelled)
    ));
    drop(reader);
    snapshot.finish();
}

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

fn write_page_result_corpus(root: &Path, pages: usize) {
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    for at in 0..pages {
        let rank = if at + 1 == pages {
            "a-winner".to_string()
        } else {
            format!("z-{at:03}")
        };
        std::fs::write(
            root.join("pages").join(format!("Page{at:03}.md")),
            format!("rank:: {rank}\ngroup:: {}\n\n- body\n", at % 3),
        )
        .expect("page fixture");
    }
}

fn q4_run(corpus: &Corpus, view: &ViewSettings, cap: usize) -> serde_json::Value {
    let (query, _) = crate::query::parse_query_text("@block", QueryDialect::Tql, corpus.today());
    wire(
        &crate::query::run_query_result_ir(
            &corpus.graph,
            &query,
            view,
            Bounds {
                max_rows: cap,
                max_bytes: usize::MAX,
            },
            &ExecutionContext::none(),
        )
        .expect("ready query"),
    )
}

#[test]
fn q4_blocks_sort_all_before_row_cap_and_sample() {
    let _serial = serialize();
    let root = scratch("q4-sort-before-cap");
    std::fs::create_dir_all(root.join("pages")).unwrap();
    std::fs::write(root.join("pages/Order.md"),
        "- z\n  rank:: z\n  cost:: 30\n- y\n  rank:: y\n  cost:: 20\n- a\n  rank:: a\n  cost:: 10\n").unwrap();
    let corpus = Corpus::open(root, true);
    let view = ViewSettings {
        sort: vec![(Field::new("rank"), SortDir::Asc)],
        sample: Some(1),
        ..ViewSettings::default()
    };
    let answer = q4_run(&corpus, &view, 2);
    assert_eq!(answer["groups"][0]["blocks"][0]["properties"][0][1], "a");
    assert_eq!(answer["total"], 1);
    assert_eq!(answer["matched_total"], 3);
    assert_eq!(answer["exceeded"], false);
}

#[test]
fn q4_unsorted_sample_uses_complete_base_order() {
    let _serial = serialize();
    let root = scratch("q4-base-before-sample");
    std::fs::create_dir_all(root.join("pages")).unwrap();
    std::fs::write(root.join("pages/00.md"), "title:: Zulu\n\n- z\n").unwrap();
    std::fs::write(root.join("pages/01.md"), "title:: Alpha\n\n- a\n").unwrap();
    let corpus = Corpus::open(root, true);
    let view = ViewSettings {
        sample: Some(1),
        ..ViewSettings::default()
    };
    let (_, statement) = corpus.lower_block_anchored("@block", QueryDialect::Tql);
    let mut snapshot = corpus.snapshot();
    super::read_ordered_results(
        &mut snapshot,
        &ResultReadInputs {
            statement: &statement,
            order: BackendOrder::Direct,
            identity: &ResultIdentity::Stored,
            max_rows: 2,
            max_bytes: usize::MAX,
            profile: ConstructionProfile::default(),
            recency: &recency_for(&corpus.root),
        },
        &view,
        &page_recency_for(&corpus.root),
    )
    .expect("ordered descriptors and payload");
    snapshot.finish();
    let answer = q4_run(&corpus, &view, 2);
    assert_eq!(answer["groups"][0]["page"], "Alpha");
    assert_eq!(answer["total"], 1);
    assert_eq!(answer["exceeded"], false);
}

fn q4_statistics_fixture(label: &str, body: &str) -> Corpus {
    let root = scratch(label);
    std::fs::create_dir_all(root.join("pages")).unwrap();
    std::fs::write(root.join("pages/Stats.md"), body).unwrap();
    Corpus::open(root, true)
}

fn q4_statistics_view() -> ViewSettings {
    use crate::query::ir::AggFn;
    ViewSettings {
        aggregates: vec![
            (Field::new(""), AggFn::Count),
            (Field::new("cost"), AggFn::Sum),
        ],
        group_by: Some(Field::new("prop:group")),
        ..ViewSettings::default()
    }
}

#[test]
fn q4_statistics_cap1_of3_includes_tail_numeric_group() {
    let _serial = serialize();
    let corpus = q4_statistics_fixture("q4-cap-tail", "- one\n  cost:: 1\n  group:: first\n- two\n  cost:: 2\n  group:: second\n- three\n  cost:: 30\n  group:: tail\n");
    let answer = q4_run(&corpus, &q4_statistics_view(), 1);
    assert_eq!(answer["groups"][0]["blocks"].as_array().unwrap().len(), 1);
    assert_eq!(answer["statistics"]["count"], 3);
    assert_eq!(answer["statistics"]["overall"][1]["value"], 33.0);
    assert_eq!(answer["statistics"]["groups"][2]["key"], "tail");
}

#[test]
fn q4_tail_only_tag_group_does_not_multiply_overall() {
    let _serial = serialize();
    let corpus = q4_statistics_fixture(
        "q4-tag-tail",
        "- one #first\n  cost:: 1\n- two #second #tail\n  cost:: 2\n",
    );
    let mut view = q4_statistics_view();
    view.group_by = Some(Field::new("tags"));
    let answer = q4_run(&corpus, &view, 1);
    assert_eq!(answer["statistics"]["overall"][1]["value"], 3.0);
    assert_eq!(answer["statistics"]["groups"].as_array().unwrap().len(), 3);
    assert_eq!(answer["statistics"]["count"], 2);
}

#[test]
fn q4_repeated_ordered_count_sum_avg_sum() {
    use crate::query::ir::AggFn;
    let _serial = serialize();
    let corpus = q4_statistics_fixture(
        "q4-repeat",
        "- one\n  cost:: 2\n  cost:: 100\n- two\n  cost:: 3 hrs\n- three\n",
    );
    let view = ViewSettings {
        aggregates: [AggFn::Count, AggFn::Sum, AggFn::Avg, AggFn::Sum]
            .into_iter()
            .map(|op| (Field::new("cost"), op))
            .collect(),
        ..ViewSettings::default()
    };
    let answer = q4_run(&corpus, &view, 1);
    assert_eq!(
        answer["statistics"]["overall"],
        serde_json::json!([
            {"kind":"number","value":3.0,"skipped":0},
            {"kind":"number","value":5.0,"skipped":1},
            {"kind":"number","value":2.5,"skipped":1},
            {"kind":"number","value":5.0,"skipped":1}
        ])
    );
}

#[test]
fn q4_sorted_sample2_of4_precedes_bridge_caps() {
    let _serial = serialize();
    let corpus = q4_statistics_fixture(
        "q4-sample-two",
        "- z\n  cost:: 9\n- y\n  cost:: 8\n- b\n  cost:: 2\n- a\n  cost:: 1\n",
    );
    let mut view = q4_statistics_view();
    view.sort = vec![(Field::new("cost"), SortDir::Asc)];
    view.sample = Some(2);
    let answer = q4_run(&corpus, &view, 1);
    assert_eq!(answer["statistics"]["count"], 2);
    assert_eq!(answer["statistics"]["overall"][1]["value"], 3.0);
    assert_eq!(answer["total"], 2);
    assert_eq!(answer["matched_total"], 4);
}

#[test]
fn q4_formula_grouping_reports_unsupported_preserves_visual_groups() {
    let _serial = serialize();
    let corpus = q4_statistics_fixture("q4-formula", "- one\n  formula-cost:: 3 hrs\n");
    // The projection stores raw authored keys. Exercise a literal colon key
    // without asking the Markdown property grammar to create that wire case.
    let writer = rusqlite::Connection::open(corpus.projection_path()).unwrap();
    writer.execute("UPDATE properties SET name='formula:cost', normalized_name='formula:cost' WHERE name='formula-cost'", []).unwrap();
    let mut view = q4_statistics_view();
    view.group_by = Some(Field::new("formula:cost"));
    view.aggregates[1].0 = Field::new("formula:cost");
    let answer = q4_run(&corpus, &view, 1);
    assert_eq!(
        answer["statistics"]["grouping_status"],
        "unsupported_formula"
    );
    assert_eq!(answer["statistics"]["groups"], serde_json::Value::Null);
    assert_eq!(answer["statistics"]["overall"][1]["value"], 3.0);
    assert_eq!(answer["groups"][0]["blocks"].as_array().unwrap().len(), 1);
}

#[test]
fn q4_legacy_numeric_raw_property_semantics() {
    use crate::query::ir::AggFn;
    let _serial = serialize();
    let corpus = q4_statistics_fixture("q4-raw", "- one\n  cost:: 3 hrs\n  Cost:: 999\n- two\n  cost:: 2026-09-09\n- three\n  cost:: 2e2tail\n- four\n  cost:: 1e+\n- five\n  cost:: Infinity\n");
    let view = ViewSettings {
        aggregates: vec![(Field::new("cost"), AggFn::Sum)],
        ..ViewSettings::default()
    };
    let answer = q4_run(&corpus, &view, 1);
    assert_eq!(
        answer["statistics"]["overall"],
        serde_json::json!([
            {"kind":"number", "value":2230.0,"skipped":1}
        ])
    );
}

#[test]
fn q4_ordered_float_rounding_and_overflow_match_js() {
    let _serial = serialize();
    let corpus = q4_statistics_fixture(
        "q4-float",
        "- one\n  cost:: 1e308\n- two\n  cost:: 1e308\n- three\n  cost:: -1e308\n",
    );
    let answer = q4_run(&corpus, &q4_statistics_view(), 1);
    assert_eq!(
        answer["statistics"]["overall"][1],
        serde_json::json!({"kind":"marker", "reason":"non_finite", "skipped":0})
    );
}

#[test]
fn q4_statistics_empty_group_only_and_scoped_pages() {
    let _serial = serialize();
    let corpus = q4_statistics_fixture("q4-empty", "- one\n");
    let mut view = q4_statistics_view();
    view.sample = Some(0);
    let answer = q4_run(&corpus, &view, 1);
    assert_eq!(answer["statistics"]["count"], 0);
    assert_eq!(answer["statistics"]["overall"][0]["value"], 0.0);
    assert_eq!(answer["statistics"]["overall"][1]["reason"], "empty_group");
    assert_eq!(answer["statistics"]["groups"], serde_json::json!([]));
    view.aggregates.clear();
    view.sample = None;
    let grouped = q4_run(&corpus, &view, 1);
    assert_eq!(
        grouped["statistics"]["aggregates"],
        serde_json::json!([["", "count"]])
    );
    view.group_by = Some(Field::new(""));
    view.view = Some(crate::query::ir::ViewKind::Board);
    assert!(q4_run(&corpus, &view, 1).get("statistics").is_none());
    let page_view = ViewSettings {
        sample: Some(1),
        ..q4_statistics_view()
    };
    for (source, count) in [("@page", 1), ("@page and name = 'absent'", 0)] {
        let (query, _) = crate::query::parse_query_text(source, QueryDialect::Tql, corpus.today());
        let pages = crate::query::run_query_result_ir(
            &corpus.graph,
            &query,
            &page_view,
            Bounds::unbounded(),
            &ExecutionContext::none(),
        )
        .unwrap();
        assert_eq!(pages.statistics.unwrap().count, count, "{source}");
    }
    assert_eq!(
        answer["statistics"]["count"], 0,
        "the separate page sample cannot change block statistics"
    );
}

#[test]
fn q4_physical_duplicate_name_page_aggregation() {
    let _serial = serialize();
    let root = scratch("q4-page-identities");
    std::fs::create_dir_all(root.join("pages")).unwrap();
    std::fs::write(
        root.join("pages/a.md"),
        "title:: Same\ncost:: 2\n\n- body\n",
    )
    .unwrap();
    std::fs::write(
        root.join("pages/b.org"),
        "#+TITLE: Same\n:PROPERTIES:\n:cost: 3\n:END:\n\n* body\n",
    )
    .unwrap();
    let corpus = Corpus::open(root, true);
    let (query, _) = crate::query::parse_query_text(
        "@page and journal = false",
        QueryDialect::Tql,
        corpus.today(),
    );
    let answer = wire(
        &crate::query::run_query_result_ir(
            &corpus.graph,
            &query,
            &q4_statistics_view(),
            Bounds {
                max_rows: 1,
                max_bytes: usize::MAX,
            },
            &ExecutionContext::none(),
        )
        .unwrap(),
    );
    assert_eq!(answer["pages"].as_array().unwrap().len(), 1);
    assert_eq!(answer["statistics"]["count"], 2);
    assert_eq!(answer["statistics"]["overall"][1]["value"], 5.0);
}

#[test]
fn q4_statistics_read_census_keeps_rejected_payload_unread() {
    let _serial = serialize();
    let corpus = q4_statistics_fixture(
        "q4-census",
        "- one\n  cost:: 1\n- two\n  cost:: 2\n- three\n  cost:: 3\n",
    );
    reset_result_read_census();
    let answer = q4_run(&corpus, &q4_statistics_view(), 1);
    assert_eq!(answer["statistics"]["count"], 3);
    assert_eq!(result_read_census().payload_block_rows, 1);
    assert_eq!(result_read_census().statistics_rows, 3);
    assert_eq!(result_read_census().statistics_values, 6);
    for order in [BackendOrder::Direct, BackendOrder::Managed] {
        let mut snapshot = corpus.snapshot();
        reset_result_read_census();
        let result = q4_snapshot_order(
            &corpus,
            &mut snapshot,
            Bounds {
                max_rows: 1,
                max_bytes: usize::MAX,
            },
            order,
        )
        .unwrap();
        assert_eq!(wire(&result)["statistics"], answer["statistics"]);
        let census = result_read_census();
        assert_eq!(
            (
                census.statistics_rows,
                census.statistics_values,
                census.payload_block_rows,
                census.payload_statements
            ),
            (3, 6, 1, 3)
        );
        assert_eq!(census.page_payload_statements, 0);
    }
}

#[test]
fn q4_statistics_wire_numbers_and_markers() {
    let value = serde_json::json!({
        "anchor":"block", "groups":[], "total":0, "exceeded":false,
        "report":{"supported":true},
        "statistics":{"count":0,"aggregates":[["cost","sum"]],"group_by":null,
            "overall":[{"kind":"number","value":3.125,"skipped":1}],
            "groups":null,"grouping_status":"none"}
    });
    let decoded: crate::query::ir::QueryResult = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(wire(&decoded)["statistics"], value["statistics"]);
    for bad in [serde_json::Value::Null, serde_json::json!("3.125")] {
        let mut invalid = value.clone();
        invalid["statistics"]["overall"][0]["value"] = bad;
        assert!(serde_json::from_value::<crate::query::ir::QueryResult>(invalid).is_err());
    }
}

fn q4_snapshot_run(
    corpus: &Corpus,
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    bounds: Bounds,
) -> Result<crate::query::ir::QueryResult, crate::query::QueryExecutionError> {
    q4_snapshot_order(corpus, snapshot, bounds, BackendOrder::Direct)
}

fn q4_snapshot_order(
    corpus: &Corpus,
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    bounds: Bounds,
    order: BackendOrder,
) -> Result<crate::query::ir::QueryResult, crate::query::QueryExecutionError> {
    q4_snapshot_query(corpus, snapshot, bounds, order, "@block")
}

fn q4_snapshot_query(
    corpus: &Corpus,
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    bounds: Bounds,
    order: BackendOrder,
    source: &str,
) -> Result<crate::query::ir::QueryResult, crate::query::QueryExecutionError> {
    use crate::query::read_execute::{SnapshotQueryInputs, SnapshotQueryReader};
    let registry = corpus.graph.property_registry();
    let recency = recency_for(&corpus.root);
    let page_recency = page_recency_for(&corpus.root);
    let reader = SnapshotQueryReader::new(
        snapshot,
        SnapshotQueryInputs {
            registry: &registry,
            identity: &ResultIdentity::Stored,
            order,
            recency: &recency,
            page_recency: &page_recency,
            today: corpus.today(),
        },
    )?;
    let (query, _) = crate::query::parse_query_text(source, QueryDialect::Tql, corpus.today());
    reader.run(
        &query,
        &q4_statistics_view(),
        bounds,
        &ExecutionContext::none(),
    )
}

#[test]
fn q4_statistics_byte_cap_preserves_complete_facts() {
    let _serial = serialize();
    let corpus = q4_statistics_fixture(
        "q4-bytes",
        &format!(
            "- {}\n  cost:: 3\n- small\n  cost:: 4\n",
            "large ".repeat(2000)
        ),
    );
    let mut snapshot = corpus.snapshot();
    let answer = wire(
        &q4_snapshot_run(
            &corpus,
            &mut snapshot,
            Bounds {
                max_rows: 10,
                max_bytes: 4096,
            },
        )
        .unwrap(),
    );
    assert_eq!(answer["groups"], serde_json::json!([]));
    assert_eq!(answer["statistics"]["overall"][1]["value"], 7.0);
    snapshot.finish();
    let mut snapshot = corpus.snapshot();
    let complete = q4_snapshot_run(&corpus, &mut snapshot, Bounds::unbounded()).unwrap();
    snapshot.finish();
    let crate::query::ir::QueryRows::Block { groups } = &complete.rows else {
        panic!("block result");
    };
    let exact = budget_cost(&groups[0].page, &groups[0].blocks[0]);
    for (bytes, displayed) in [(exact, 1), (exact - 1, 0)] {
        let mut snapshot = corpus.snapshot();
        let answer = q4_snapshot_run(
            &corpus,
            &mut snapshot,
            Bounds {
                max_rows: 10,
                max_bytes: bytes,
            },
        )
        .unwrap();
        let crate::query::ir::QueryRows::Block { groups } = &answer.rows else {
            panic!("block result");
        };
        assert_eq!(
            groups.iter().map(|group| group.blocks.len()).sum::<usize>(),
            displayed
        );
        assert_eq!(answer.statistics, complete.statistics);
        assert!(answer.exceeded);
    }
    let mut snapshot = corpus.snapshot();
    let failure = q4_snapshot_run(
        &corpus,
        &mut snapshot,
        Bounds {
            max_rows: 10,
            max_bytes: 0,
        },
    )
    .unwrap_err();
    assert!(failure
        .backend_wire_string()
        .contains("statistics_resource_limit"));
}

#[test]
fn q4_statistics_failure_and_cancellation_return_no_partial_answer() {
    let _serial = serialize();
    let corpus = q4_statistics_fixture("q4-errors", "- one\n  cost:: 1\n");
    let mut snapshot = corpus.snapshot();
    let failure = q4_snapshot_run(
        &corpus,
        &mut snapshot,
        Bounds {
            max_rows: 1,
            max_bytes: 0,
        },
    );
    assert!(
        failure.is_err(),
        "requested statistics cannot fit a zero-byte budget"
    );
    snapshot.finish();
    let mut snapshot = corpus.snapshot();
    snapshot.cancellation().cancel();
    assert!(matches!(
        q4_snapshot_run(&corpus, &mut snapshot, Bounds::unbounded()),
        Err(crate::query::QueryExecutionError::Cancelled)
    ));
    snapshot.finish();
    for (label, sql) in [
        ("sql", "DROP TABLE properties"),
        ("corruption", "DELETE FROM query_block_results"),
    ] {
        let path = copy_projection(&corpus, &format!("q4-failure-{label}"));
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
        let error = q4_snapshot_run(&corpus, &mut snapshot, Bounds::unbounded()).unwrap_err();
        assert!(
            matches!(error, crate::query::QueryExecutionError::Unavailable(_)),
            "{label}: {error}"
        );
    }
}

#[test]
fn q4_statistics_resource_limit_is_visible_not_zero() {
    let _serial = serialize();
    let body = (0..40)
        .map(|i| format!("- row {i}\n  cost:: 1\n  group:: key{i}\n"))
        .collect::<String>();
    let corpus = q4_statistics_fixture("q4-resource", &body);
    let mut snapshot = corpus.snapshot();
    reset_result_read_census();
    let error = q4_snapshot_run(
        &corpus,
        &mut snapshot,
        Bounds {
            max_rows: 1,
            max_bytes: 2048,
        },
    )
    .unwrap_err();
    assert!(
        result_read_census().statistics_rows > 1,
        "the group budget, not initial overhead, must be exhausted"
    );
    assert_eq!(
        result_read_census().payload_block_rows,
        0,
        "a failed fold publishes no partial payload"
    );
    assert_eq!(error.to_string(), "Exact query statistics exceed the available memory limit. Narrow the query or remove grouping or aggregates.");
    assert!(error
        .backend_wire_string()
        .contains("statistics_resource_limit"));
}

#[test]
fn q4_commit_between_rows_and_statistics_uses_one_snapshot() {
    let _serial = serialize();
    let corpus = q4_statistics_fixture(
        "q4-coherent",
        "- TODO one #old\n  cost:: 1\n  group:: old\n- DONE two #new\n  cost:: 9\n  group:: new\n",
    );
    let path = copy_projection(&corpus, "q4-coherent");
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    let hook_path = path.clone();
    set_before_payload_batch_hook(Some(Box::new(move |_| {
        let writer = rusqlite::Connection::open(&hook_path).unwrap();
        writer.execute_batch("BEGIN; UPDATE properties SET value = '9' WHERE name = 'cost'; UPDATE properties SET value = 'alt' WHERE name = 'group' AND value = 'old'; UPDATE tags SET tag = 'alt' WHERE tag = 'old'; UPDATE tasks SET marker = 'TODO' WHERE marker = 'DONE'; COMMIT;").unwrap();
    })));
    let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
    let result = q4_snapshot_query(
        &corpus,
        &mut snapshot,
        Bounds::unbounded(),
        BackendOrder::Direct,
        "@block and task = 'TODO'",
    );
    set_before_payload_batch_hook(None);
    let answer = wire(&result.unwrap());
    assert_eq!(answer["statistics"]["overall"][1]["value"], 1.0);
    assert_eq!(answer["statistics"]["count"], 1);
    assert_eq!(answer["statistics"]["groups"][0]["key"], "old");
    assert_eq!(
        answer["groups"][0]["blocks"][0]["tags"],
        serde_json::json!(["old"])
    );
    assert_eq!(answer["groups"][0]["blocks"][0]["properties"][0][1], "1");
    snapshot.finish();
    let mut next = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
    let answer = wire(
        &q4_snapshot_query(
            &corpus,
            &mut next,
            Bounds::unbounded(),
            BackendOrder::Direct,
            "@block and task = 'TODO'",
        )
        .unwrap(),
    );
    assert_eq!(answer["statistics"]["overall"][1]["value"], 18.0);
    assert_eq!(answer["statistics"]["count"], 2);
    assert_eq!(answer["statistics"]["groups"][0]["key"], "alt");
}

#[test]
fn page_results_sort_the_complete_set_before_limit_and_keep_exact_counts() {
    let _serial = serialize();
    let root = scratch("s3-page-complete-sort");
    write_page_result_corpus(&root, 45);
    let corpus = Corpus::open(root, true);
    let source = "@page and journal = false";
    let view = ViewSettings {
        sort: vec![(Field::new("rank"), SortDir::Asc)],
        sample: Some(1),
        ..ViewSettings::default()
    };

    let expected = oracle_page_answer(&corpus, source, &view, 2, usize::MAX);
    let actual = database_page_answer(&corpus, source, &view, 2, usize::MAX)
        .expect("the shared page reader answers");
    assert_page_answers_equal(&expected, &actual);
    assert_eq!(actual.matched_total, 45);
    assert_eq!(actual.total, 2, "sample does not rewrite admitted total");
    assert_eq!(
        actual.pages.len(),
        1,
        "sample is applied after construction"
    );
    assert_eq!(actual.pages[0].path, "pages/Page044.md");
    assert_eq!(
        actual.pages[0].properties[0],
        ("rank".into(), "a-winner".into())
    );
    assert!(actual.exceeded);

    let statistics_view = ViewSettings {
        aggregates: vec![(Field::new(""), crate::query::ir::AggFn::Count)],
        ..view.clone()
    };
    let with_statistics = database_page_answer(&corpus, source, &statistics_view, 2, usize::MAX)
        .expect("statistics preserve the approved page admission counters");
    assert_eq!(with_statistics.matched_total, 45);
    assert_eq!(with_statistics.total, 2);
    assert_eq!(with_statistics.pages.len(), 1);
    assert!(with_statistics.exceeded);
    assert_eq!(with_statistics.statistics.unwrap().count, 1);

    let mut snapshot = corpus.snapshot();
    let position = snapshot
        .run_projection_query(
            "SELECT o.position FROM query_page_order o JOIN pages p ON p.page_id = o.page_id \
             WHERE p.path = ?1",
            &[PhysicalQueryValue::Text("pages/Page044.md".into())],
        )
        .expect("the stored inventory position is readable");
    snapshot.finish();
    assert!(
        matches!(
            position.first().and_then(|row| row.first()),
            Some(PhysicalQueryValue::Integer(at)) if *at >= 41
        ),
        "the winning row must originate beyond the retired 41-row prefix"
    );
}

#[test]
fn page_result_limits_use_raw_estimates_and_never_hide_the_complete_count() {
    let _serial = serialize();
    let root = scratch("s3-page-bounds");
    write_page_result_corpus(&root, 3);
    let corpus = Corpus::open(root, true);
    let source = "@page and journal = false";
    let view = ViewSettings {
        sort: vec![(Field::new("rank"), SortDir::Asc)],
        ..ViewSettings::default()
    };
    let unbounded = oracle_page_answer(&corpus, source, &view, usize::MAX, usize::MAX);
    let first = &unbounded.pages[0];
    let first_cost = tine_storage::sqlite::query_page_result_estimated_bytes(
        &first.name,
        &first.path,
        first
            .properties
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    );
    assert!(first_cost > 0);

    for (rows, bytes) in [
        (0, usize::MAX),
        (usize::MAX, 0),
        (1, usize::MAX),
        (usize::MAX, first_cost),
        (usize::MAX, first_cost - 1),
    ] {
        let expected = oracle_page_answer(&corpus, source, &view, rows, bytes);
        let actual = database_page_answer(&corpus, source, &view, rows, bytes)
            .expect("the bounded shared page reader answers");
        assert_page_answers_equal(&expected, &actual);
        assert_eq!(actual.matched_total, 3);
    }
    reset_result_read_census();
    let empty = database_page_answer(
        &corpus,
        "@page and name = 'No Such Page'",
        &view,
        usize::MAX,
        usize::MAX,
    )
    .expect("an empty page query answers");
    assert_eq!(
        (empty.pages.len(), empty.total, empty.matched_total),
        (0, 0, 0)
    );
    assert!(!empty.exceeded);
    assert_eq!(result_read_census().page_payload_statements, 0);
}

#[test]
fn page_property_sort_matches_the_independent_oracle_for_saved_semantics() {
    let _serial = serialize();
    let root = scratch("s3-page-sort-semantics");
    std::fs::create_dir_all(root.join("pages/nested")).expect("pages");
    for (path, body) in [
        ("NumTen.md", "rank:: 10\ngroup:: b\n\n- body\n"),
        ("NumTwo.md", "rank:: 2\ngroup:: b\n\n- body\n"),
        ("Twin.md", "rank:: İ\ngroup:: a\n\n- body\n"),
        ("nested/Twin.md", "rank:: i\u{307}\ngroup:: a\n\n- body\n"),
        ("Missing.md", "group:: a\n\n- body\n"),
        ("OrdFirst.md", "rank:: zzz\nrank:: a\ngroup:: c\n\n- body\n"),
        ("OrdSecond.md", "rank:: zz\ngroup:: c\n\n- body\n"),
    ] {
        std::fs::write(root.join("pages").join(path), body).expect("page fixture");
    }
    let corpus = Corpus::open(root, true);
    let source = "@page and journal = false";
    for view in [
        ViewSettings {
            sort: vec![(Field::new("rank"), SortDir::Asc)],
            ..ViewSettings::default()
        },
        ViewSettings {
            sort: vec![
                (Field::new("group"), SortDir::Desc),
                (Field::new("rank"), SortDir::Asc),
            ],
            ..ViewSettings::default()
        },
        ViewSettings {
            sort: vec![(Field::new("name"), SortDir::Desc)],
            ..ViewSettings::default()
        },
    ] {
        let expected = oracle_page_answer(&corpus, source, &view, usize::MAX, usize::MAX);
        let actual = database_page_answer(&corpus, source, &view, usize::MAX, usize::MAX)
            .expect("the sorted page read answers");
        assert_page_answers_equal(&expected, &actual);
    }

    let lexical = database_page_answer(
        &corpus,
        "@page and name like 'Num%'",
        &ViewSettings {
            sort: vec![(Field::new("rank"), SortDir::Asc)],
            ..ViewSettings::default()
        },
        usize::MAX,
        usize::MAX,
    )
    .expect("the lexical property sort answers");
    assert_eq!(
        lexical
            .pages
            .iter()
            .map(|page| page.name.as_str())
            .collect::<Vec<_>>(),
        ["NumTen", "NumTwo"],
        "numeric-looking authored properties remain lexical"
    );
    let twins = lexical.matched_total;
    assert_eq!(twins, 2);
    let first_ordinal = database_page_answer(
        &corpus,
        "@page and name like 'Ord%'",
        &ViewSettings {
            sort: vec![(Field::new("rank"), SortDir::Asc)],
            ..ViewSettings::default()
        },
        usize::MAX,
        usize::MAX,
    )
    .expect("the repeated-property sort answers");
    assert_eq!(
        first_ordinal
            .pages
            .iter()
            .map(|page| page.name.as_str())
            .collect::<Vec<_>>(),
        ["OrdSecond", "OrdFirst"],
        "the first authored property ordinal owns the saved sort scalar"
    );
    let all = database_page_answer(
        &corpus,
        source,
        &ViewSettings::default(),
        usize::MAX,
        usize::MAX,
    )
    .expect("the default page read answers");
    assert_eq!(
        all.pages.iter().filter(|page| page.name == "Twin").count(),
        2
    );
    assert!(all.pages.iter().any(|page| page.path == "pages/Twin.md"));
    assert!(all
        .pages
        .iter()
        .any(|page| page.path == "pages/nested/Twin.md"));
}

#[test]
fn direct_page_recency_uses_file_mtime_for_undated_journals_and_ordinary_pages() {
    let _serial = serialize();
    let root = scratch("s3-page-undated-journal-recency");
    std::fs::create_dir_all(root.join("pages")).expect("pages");
    let ordinary = root.join("pages/A-ordinary.md");
    let undated = root.join("pages/Z-undated.md");
    std::fs::write(&ordinary, "- ordinary\n").expect("ordinary page");
    std::fs::write(&undated, "- undated journal\n").expect("undated journal");
    let set_mtime = |path: &Path, seconds| {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("the fixture file opens");
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds)),
        )
        .expect("the fixture mtime is settable");
    };
    set_mtime(&ordinary, 100);
    set_mtime(&undated, 200);
    let corpus = Corpus::open(root, true);

    // Accepted inventory can retain an explicit Journal classification for a
    // path whose filename supplies no day. The Direct physical producer stores
    // that exact `(Journal, NULL journal_day)` shape.
    let inventory = corpus
        .graph
        .projected_inventory_entry(
            &crate::oplog::ManagedPath::parse("pages/Z-undated.md").unwrap(),
            "Z-undated",
            crate::oplog::ManagedTextKind::Journal,
        )
        .expect("the existing inventory decoder accepts an explicit journal kind");
    assert_eq!(inventory.kind, PageKind::Journal);
    assert_eq!(inventory.date_key, None);
    let mut document = crate::doc::parse("- undated journal\n");
    crate::model::assign_doc_runtime_ids(&mut document.roots, &inventory.rel_path);
    let physical = crate::direct_projection::physical_page_for_test(
        &inventory,
        &document,
        &crate::config::ParseConfig::default(),
    )
    .expect("the Direct producer accepts the inventory row");
    assert_eq!(physical.text_kind, 1);
    assert_eq!(physical.journal_day, None);

    let database = copy_projection(&corpus, "undated-journal-recency");
    {
        let writer = rusqlite::Connection::open(&database).expect("the copy opens writable");
        writer
            .execute(
                "UPDATE pages SET text_kind = 1, journal_day = NULL WHERE path = ?1",
                rusqlite::params!["pages/Z-undated.md"],
            )
            .expect("the accepted undated-journal shape is installed");
    }
    let (anchor, statement) = corpus.lower(
        "@page and name like '%'",
        QueryDialect::Tql,
        corpus.fts_ready(),
    );
    assert_eq!(anchor, Anchor::Page);
    let view = ViewSettings {
        sort: vec![(Field::new("modified"), SortDir::Asc)],
        ..ViewSettings::default()
    };
    let recency = page_recency_for(&corpus.root);
    let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&database, || Ok(()))
        .expect("the accepted projection copy opens");
    let answer = read_page_results(
        &mut snapshot,
        &PageReadInputs {
            statement: &statement,
            order: BackendOrder::Direct,
            view: &view,
            max_rows: usize::MAX,
            max_bytes: usize::MAX,
            recency: &recency,
        },
    )
    .expect("the Direct recency statement answers an undated journal");
    snapshot.finish();

    let mut expected = [
        (
            page_recency_secs_for(None, &ordinary),
            "pages/A-ordinary.md",
        ),
        (page_recency_secs_for(None, &undated), "pages/Z-undated.md"),
    ];
    expected.sort_by(|left, right| left.cmp(right));
    assert_eq!(expected[0].0, 100);
    assert_eq!(expected[1].0, 200);
    assert_eq!(
        answer
            .pages
            .iter()
            .map(|page| page.path.as_str())
            .collect::<Vec<_>>(),
        expected.iter().map(|(_, path)| *path).collect::<Vec<_>>()
    );
    let undated = answer
        .pages
        .iter()
        .find(|page| page.path == "pages/Z-undated.md")
        .expect("the undated journal is returned");
    assert_eq!(undated.kind, PageKind::Journal);
    assert_eq!(undated.journal_day, None);

    let journal_format = crate::date::JournalFormat::new(None, None);
    let managed_root = corpus.root.clone();
    let managed_recency = PageRecencyPrograms::new(
        JournalRankInput::DisplayName,
        move |name| journal_format.page_recency_secs(true, name, Path::new("")),
        move |path| page_recency_secs_for(None, &managed_root.join(path)),
    );
    let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&database, || Ok(()))
        .expect("the accepted projection copy opens again");
    let managed = read_page_results(
        &mut snapshot,
        &PageReadInputs {
            statement: &statement,
            order: BackendOrder::Managed,
            view: &view,
            max_rows: usize::MAX,
            max_bytes: usize::MAX,
            recency: &managed_recency,
        },
    )
    .expect("Managed display-name recency keeps its existing answer");
    snapshot.finish();
    assert_eq!(
        managed
            .pages
            .iter()
            .map(|page| page.path.as_str())
            .collect::<Vec<_>>(),
        ["pages/Z-undated.md", "pages/A-ordinary.md"],
        "an unparseable Managed journal stays at MIN rather than using mtime"
    );
    let _ = std::fs::remove_file(database);
}

#[test]
fn page_property_payload_batches_only_admitted_ids() {
    let _serial = serialize();
    let root = scratch("s3-page-payload-batches");
    write_page_result_corpus(&root, PAYLOAD_BATCH + 1);
    let corpus = Corpus::open(root, true);
    let source = "@page and journal = false";
    for (limit, statements) in [(PAYLOAD_BATCH, 1), (PAYLOAD_BATCH + 1, 2)] {
        reset_result_read_census();
        let answer =
            database_page_answer(&corpus, source, &ViewSettings::default(), limit, usize::MAX)
                .expect("the batched page read answers");
        let census = result_read_census();
        assert_eq!(answer.total, limit);
        assert_eq!(answer.matched_total, PAYLOAD_BATCH + 1);
        assert_eq!(census.page_payload_statements, statements);
        assert_eq!(census.page_payload_property_rows, 2 * limit);
    }
}

#[test]
fn page_rank_programs_replace_as_one_redacted_statement_table() {
    let _serial = serialize();
    let root = scratch("s3-page-rank-programs");
    write_page_result_corpus(&root, 1);
    let corpus = Corpus::open(root, true);
    let mut snapshot = corpus.snapshot();

    let secret = "authored secret text".to_string();
    let mut first = QueryRankPrograms::default();
    let first_id = first.bind({
        let secret = secret.clone();
        move |text| Ok(Some(format!("{secret}:{text}").into_bytes()))
    });
    assert_eq!(first_id, 1);
    assert!(!format!("{first:?}").contains(&secret));
    let first_function = first.function(snapshot.cancellation());
    let unknown = first_function(2, &secret).expect_err("an unknown program id must fail");
    assert!(!format!("{unknown:?}").contains(&secret));
    snapshot
        .set_query_rank_function(first_function)
        .expect("the first statement rank table installs");

    let mut second = QueryRankPrograms::default();
    second.bind(|text| Ok(Some(format!("second:{text}").into_bytes())));
    snapshot
        .set_query_rank_function(second.function(snapshot.cancellation()))
        .expect("the next statement replaces the rank table");
    let rows = snapshot
        .run_projection_query(
            "SELECT tine_query_rank(?1, ?2)",
            &[
                PhysicalQueryValue::Integer(1),
                PhysicalQueryValue::Text("value".into()),
            ],
        )
        .expect("the replacement rank table answers");
    snapshot.finish();
    assert_eq!(
        rows,
        vec![vec![PhysicalQueryValue::Blob(b"second:value".to_vec())]]
    );
}

#[test]
fn cancelling_page_selection_or_a_later_payload_batch_returns_no_partial_answer() {
    let _serial = serialize();
    let root = scratch("s3-page-cancellation");
    write_page_result_corpus(&root, PAYLOAD_BATCH + 1);
    let corpus = Corpus::open(root, true);
    let (anchor, statement) = corpus.lower(
        "@page and journal = false",
        QueryDialect::Tql,
        corpus.fts_ready(),
    );
    assert_eq!(anchor, Anchor::Page);
    let view = ViewSettings {
        sort: vec![(Field::new("modified"), SortDir::Asc)],
        ..ViewSettings::default()
    };

    let mut selecting = corpus.snapshot();
    let cancel_during_rank = selecting.cancellation();
    let (rank_started, wait_for_rank) = std::sync::mpsc::channel::<()>();
    let (rank_cancelled, wait_for_cancel) = std::sync::mpsc::channel::<()>();
    let wait_for_cancel = std::sync::Mutex::new(wait_for_cancel);
    let owner = std::thread::spawn(move || {
        wait_for_rank.recv().expect("the rank callback starts");
        cancel_during_rank.cancel();
        rank_cancelled
            .send(())
            .expect("the rank callback is waiting");
    });
    let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let recency = PageRecencyPrograms::new(
        JournalRankInput::StoredDay,
        |_| 0,
        move |_| {
            if first.swap(false, std::sync::atomic::Ordering::Relaxed) {
                rank_started.send(()).expect("the owner is listening");
                wait_for_cancel
                    .lock()
                    .unwrap()
                    .recv()
                    .expect("the owner cancels");
            }
            0
        },
    );
    reset_result_read_census();
    let answer = read_page_results(
        &mut selecting,
        &PageReadInputs {
            statement: &statement,
            order: BackendOrder::Direct,
            view: &view,
            max_rows: usize::MAX,
            max_bytes: usize::MAX,
            recency: &recency,
        },
    );
    assert!(matches!(answer, Err(ResultReadError::Cancelled)));
    let census = result_read_census();
    assert!(census.page_recency_lookups > 0);
    assert_eq!(census.page_payload_statements, 0);
    drop(recency);
    owner.join().expect("the cancellation owner finishes");
    selecting.finish();

    let mut between = corpus.snapshot();
    let cancel_between = between.cancellation();
    set_before_page_payload_batch_hook(Some(Box::new(move |batch| {
        if batch == 1 {
            cancel_between.cancel();
        }
    })));
    let recency = page_recency_for(&corpus.root);
    let answer = read_page_results(
        &mut between,
        &PageReadInputs {
            statement: &statement,
            order: BackendOrder::Direct,
            view: &ViewSettings::default(),
            max_rows: usize::MAX,
            max_bytes: usize::MAX,
            recency: &recency,
        },
    );
    set_before_page_payload_batch_hook(None);
    assert!(matches!(answer, Err(ResultReadError::Cancelled)));
    between.finish();
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

fn read_damaged_page(
    corpus: &Corpus,
    tag: &str,
    damage: &str,
    bind: &[&dyn rusqlite::ToSql],
) -> Result<crate::query::results::PageAnswer, ResultReadError> {
    let path = copy_projection(corpus, tag);
    {
        let writer = rusqlite::Connection::open(&path).expect("the copy opens writable");
        writer
            .pragma_update(None, "foreign_keys", false)
            .expect("foreign key enforcement is settable");
        let changed = writer.execute(damage, bind).expect("the damage applies");
        assert!(
            changed > 0,
            "the damage statement changed nothing: {damage}"
        );
    }
    let (anchor, statement) = corpus.lower(
        "@page and journal = false",
        QueryDialect::Tql,
        corpus.fts_ready(),
    );
    assert_eq!(anchor, Anchor::Page);
    let recency = page_recency_for(&corpus.root);
    let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(()))
        .expect("the damaged copy still opens");
    let answer = read_page_results(
        &mut snapshot,
        &PageReadInputs {
            statement: &statement,
            order: BackendOrder::Direct,
            view: &ViewSettings::default(),
            max_rows: usize::MAX,
            max_bytes: usize::MAX,
            recency: &recency,
        },
    );
    snapshot.finish();
    let _ = std::fs::remove_file(&path);
    answer
}

#[test]
fn inconsistent_page_result_metadata_fails_instead_of_shrinking_the_answer() {
    let _serial = serialize();
    let root = scratch("s3-page-damage");
    write_page_result_corpus(&root, 3);
    let corpus = Corpus::open(root, true);
    let page = "pages/Page000.md";
    let damages = [
        (
            "missing-result",
            "DELETE FROM query_page_results WHERE page_id = \
             (SELECT page_id FROM pages WHERE path = ?1)",
        ),
        (
            "missing-order",
            "DELETE FROM query_page_order WHERE page_id = \
             (SELECT page_id FROM pages WHERE path = ?1)",
        ),
        (
            "count",
            "UPDATE query_page_results SET property_count = property_count + 1 WHERE page_id = \
             (SELECT page_id FROM pages WHERE path = ?1)",
        ),
        (
            "ordinal",
            "UPDATE properties SET ordinal = ordinal + 5 WHERE owner_type = 0 AND ordinal = 0 \
             AND owner_id = (SELECT page_id FROM pages WHERE path = ?1)",
        ),
        (
            "owner",
            "UPDATE properties SET page_id = zeroblob(16) WHERE owner_type = 0 AND ordinal = 0 \
             AND owner_id = (SELECT page_id FROM pages WHERE path = ?1)",
        ),
        (
            "estimate",
            "UPDATE query_page_results SET estimated_bytes = estimated_bytes + 1 WHERE page_id = \
             (SELECT page_id FROM pages WHERE path = ?1)",
        ),
    ];
    for (tag, damage) in damages {
        let answer = read_damaged_page(&corpus, tag, damage, rusqlite::params![page]);
        assert!(
            matches!(answer, Err(ResultReadError::Corrupt(_))),
            "{tag}: inconsistent page metadata must report Corrupt, got {answer:?}"
        );
    }
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
    let (anchor, statement) =
        corpus.lower("@page and name like 'proj/%'", QueryDialect::Tql, false);
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
