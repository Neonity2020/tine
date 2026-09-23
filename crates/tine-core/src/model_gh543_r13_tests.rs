//! GH #543, indexing audit round 13: each finding's class, pinned. The
//! fixtures follow the auditor's probes (evidence `indexing-audit-r13`).

use super::gh543_r10::{r10_finish, r10_pages, r10_prebuild, r10_scratch, r10_settle, R10Owner};
use super::*;
use std::sync::Arc;
use std::time::Duration;

/// A graph with a stored image from an earlier session, opened, with no
/// owner started yet.
fn r13_warm_open(root: &Path) -> Arc<Graph> {
    let database = root.join("private/projection.sqlite");
    r10_prebuild(root, &database);
    let graph = Arc::new(Graph::open(root));
    graph.attach_direct_projection(database).unwrap();
    graph
}

/// Audit R13-03, "who decides between validate and rebuild?": K1 alone. A
/// failed validation turn on an intact image owes a validation (R11-07); the
/// owner kept a flag of its own that turned it into a whole-graph parse.
#[test]
fn a_failed_launch_validation_turn_on_an_intact_image_is_validated_again() {
    let root = r10_scratch("r13-failed-launch-turn");
    r10_pages(&root, 12);
    let graph = r13_warm_open(&root);
    let projection = graph.direct_projection_test().unwrap();
    projection.inject_next_turn_failure_test();
    let builds = projection.fresh_builds_test();
    let parses = graph.page_build_parses_test();
    let owner = R10Owner::start(&graph);
    let ready = owner.wait_ready(Duration::from_secs(30));
    r10_settle(&graph);
    let rebuilt = projection.fresh_builds_test() - builds;
    let parsed = graph.page_build_parses_test() - parses;
    let state = projection.debug_state_test();
    r10_finish(root, graph, owner);
    assert!(ready, "{state}");
    assert_eq!(
        rebuilt, 0,
        "an intact image whose validation turn failed was rebuilt"
    );
    assert_eq!(
        parsed, 0,
        "a failed validation turn on an intact image parsed the whole graph; the owner \
         decided Fresh where K1 owed a validation (GH #543, audit R13-03)"
    );
}

/// Audit R13-04, "who records a structural change without naming the paths
/// it knows?": nobody. A change made while the launch walk runs; the walk
/// keeps its work when the change names its paths.
/// 0 = an edit, 1 = a journal migration, 2 = a rename, 3 = a merge,
/// 4 = a file rescued into a page.
fn change_during_launch_walk(mode: u8) -> (usize, usize, bool) {
    let root = r10_scratch(&format!("r13-walk-{mode}"));
    r10_pages(&root, 12);
    fs::write(root.join("journals/Jun 18th, 2026.md"), "- journal r13\n").unwrap();
    let graph = r13_warm_open(&root);
    let projection = graph.direct_projection_test().unwrap();
    let pause = graph.pause_next_warm_after_read_test();
    let parses = graph.page_build_parses_test();
    let owner = R10Owner::start(&graph);
    pause.reached.wait();
    match mode {
        0 => {
            fs::write(root.join("pages/p3.md"), "- edited r13 [[p4]]\n").unwrap();
            let _ = graph.sync_file_checked(&root.join("pages/p3.md"));
            pause.release.wait();
        }
        1 => {
            let moved = graph.migrate_journal_filenames_checked().unwrap();
            assert_eq!(moved, 1, "precondition: the journal migrated");
            pause.release.wait();
        }
        4 => {
            graph
                .rename_file_to_page("pages/p5.md", "rescued five")
                .unwrap();
            pause.release.wait();
        }
        _ => {
            // A rename reads its referrers through the readiness wait, so it
            // lands after the walk: release the walk while it waits.
            let changed = Arc::clone(&graph);
            let change = std::thread::spawn(move || {
                if mode == 3 {
                    changed.merge_pages("pages/p5.md", "pages/p6.md").unwrap();
                } else {
                    changed.rename_page("p3", "renamed three").unwrap();
                }
            });
            std::thread::sleep(Duration::from_millis(300));
            pause.release.wait();
            change.join().unwrap();
        }
    }
    let ready = owner.wait_ready(Duration::from_secs(30));
    r10_settle(&graph);
    let passes = graph.owner_passes_test();
    let parsed = graph.page_build_parses_test() - parses;
    let state = projection.debug_state_test();
    r10_finish(root, graph, owner);
    eprintln!("r13 walk mode={mode} passes={passes} parses={parsed} {state}");
    (passes, parsed, ready)
}

#[test]
fn a_change_during_the_launch_walk_keeps_the_walk() {
    for (mode, change) in [
        (0, "an edit"),
        (1, "a journal migration"),
        (2, "a rename"),
        (3, "a merge"),
        (4, "a file rescued into a page"),
    ] {
        let (passes, parsed, ready) = change_during_launch_walk(mode);
        assert!(ready, "{change}");
        assert_eq!(
            parsed, 0,
            "{change} during the launch walk parsed the graph"
        );
        assert_eq!(
            passes, 1,
            "{change} during the launch walk threw the walk away and walked again; a change \
             that knows its paths names them (GH #543, audit R13-04)"
        );
    }
}

/// Audit R12-05's class, "who can swallow detected index damage?", with its
/// member in the model layer (R13-05): a row with an unknown kind is damage
/// the reader reports, not a `None` the model drops.
#[test]
fn a_corrupt_kind_in_a_derived_page_asks_for_a_new_image() {
    let root = r10_scratch("r13-kind");
    r10_pages(&root, 12);
    fs::write(
        root.join("pages/p1.md"),
        "- tpl\n  template:: t1\n  - child [[p2]]\n",
    )
    .unwrap();
    let database = root.join("private/projection.sqlite");
    let graph = r13_warm_open(&root);
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    let projection = graph.direct_projection_test().unwrap();
    assert_eq!(
        graph.templates().len(),
        1,
        "precondition: the index finds the template"
    );
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection.busy_timeout(Duration::from_secs(5)).unwrap();
        connection
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        assert_eq!(
            connection
                .execute(
                    "UPDATE pages SET text_kind = 7 WHERE path = 'pages/p1.md'",
                    []
                )
                .unwrap(),
            1
        );
    }
    let builds = projection.fresh_builds_test();
    let parses = graph.consumer_page_parses_test();
    let found = graph.templates().len();
    let _ = projection.wait_drained_test();
    let rebuilt = projection.fresh_builds_test() - builds;
    let parsed = graph.consumer_page_parses_test() - parses;
    r10_finish(root, graph, owner);
    assert_eq!(found, 1);
    assert_eq!(
        rebuilt, 1,
        "nobody replaced the damaged image (GH #543, audit R13-05)"
    );
    assert_eq!(parsed, 0, "the template read parsed the graph instead");
}

/// Audit R13-06, "where does a journal's day come from?": its row. A date
/// `title::` under a title format with no year names the page by a title the
/// format cannot parse back; parsing the day from the name failed the whole
/// inventory, and the page list parsed the graph every session.
#[test]
fn a_journal_titled_by_a_year_less_format_is_listed_from_the_index() {
    let root = r10_scratch("r13-yearless");
    r10_pages(&root, 12);
    fs::create_dir_all(root.join("logseq")).unwrap();
    fs::write(
        root.join("logseq/config.edn"),
        "{:journal/page-title-format \"EEEE, MMM do\"}\n",
    )
    .unwrap();
    fs::write(root.join("pages/x.md"), "title:: 2026-09-23\n\n- body\n").unwrap();
    let graph = r13_warm_open(&root);
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    let parses = graph.consumer_page_parses_test() + graph.page_build_parses_test();
    let indexed = graph.direct_projection_page_inventory();
    let listed = graph
        .list_pages()
        .into_iter()
        .find(|entry| entry.rel_path == "pages/x.md")
        .expect("the page is listed");
    let days = graph.journal_content_days();
    let parsed = graph.consumer_page_parses_test() + graph.page_build_parses_test() - parses;
    // The parse path's answer for the same page: the graph with no index.
    let parsed_entry = Graph::open(&root)
        .list_pages()
        .into_iter()
        .find(|entry| entry.rel_path == "pages/x.md")
        .expect("the parse path lists the page");
    r10_finish(root, graph, owner);
    assert!(
        indexed.is_some(),
        "the index could not list the pages (GH #543, audit R13-06)"
    );
    assert_eq!(parsed, 0, "listing the pages parsed the graph");
    assert_eq!(listed.date_key, Some(20260923));
    assert_eq!(listed.kind, PageKind::Journal);
    assert_eq!(listed.date_key, parsed_entry.date_key);
    assert!(
        days.contains(&listed.date_key.unwrap()),
        "the calendar lost the day of a journal whose title has no year"
    );
}

/// Audit R13-07, "what does a read do while the answer it waits for comes?":
/// it sleeps. The readiness wait returns at once while the image is ready, so
/// a read that looped on `coming()` spun a core for as long as an update was
/// announced.
#[test]
fn a_read_waiting_for_announced_work_sleeps() {
    let root = r10_scratch("r13-spin");
    r10_pages(&root, 12);
    let graph = r13_warm_open(&root);
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    let projection = graph.direct_projection_test().unwrap();
    let announced = projection.delta_coming();
    graph.leave_indexed_reads_unanswered_test(true);
    let attempts = graph.indexed_read_attempts_test();
    let reader = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.journal_content_days())
    };
    std::thread::sleep(Duration::from_millis(1000));
    let spun = graph.indexed_read_attempts_test() - attempts;
    graph.leave_indexed_reads_unanswered_test(false);
    drop(announced);
    reader.join().unwrap();
    r10_finish(root, graph, owner);
    assert!(
        spun <= 40,
        "a read waiting for announced work asked {spun} times in a second (GH #543, audit \
         R13-07)"
    );
}

/// The model layer is handed decoded rows: a kind or a journal day is decoded
/// where the index is read, so damage is reported there (`page_kind`,
/// `DerivedPage::journal_day`). Decoding them again in the model turned
/// damage into a `None` nobody reported (R13-05) and a day into a parse that
/// fails (R13-06).
#[test]
fn the_model_reads_decoded_index_rows() {
    let mut offenders = Vec::new();
    for file in crate::projection_producer_census::production_rust() {
        if !file.relative.starts_with("crates/tine-core/src/model/") {
            continue;
        }
        for token in ["page_kind_from_sql(", "journal_content_names("] {
            if file.code.contains(token) {
                offenders.push(format!("{}: {token}", file.relative));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the model decodes a stored row itself; decode it in the index reader and report \
         damage with `.reported(self)` (GH #543, audits R12-05, R13-05, R13-06; exemplar: \
         DirectProjection::derived_pages): {offenders:?}"
    );
}

/// Audit R13-04's shape: an unnamed structural change exists only in tests.
#[test]
fn only_tests_record_an_unnamed_change() {
    let mut offenders = Vec::new();
    for file in crate::projection_producer_census::production_rust() {
        if file.code.contains("StructuralChange::Unnamed") {
            offenders.push(file.relative.clone());
        }
    }
    assert!(
        offenders.is_empty(),
        "production code records an unnamed structural change, which throws away a \
         whole-graph pass in flight; name the paths (GH #543, audit R13-04; exemplar: \
         Graph::discard_parsed_cache): {offenders:?}"
    );
}
