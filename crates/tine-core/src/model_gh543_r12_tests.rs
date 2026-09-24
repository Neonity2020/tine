//! GH #543, indexing audit round 12: each finding's class, pinned. The
//! fixtures follow the auditor's probes (evidence `indexing-audit-r12`).

use super::gh543_r10::{r10_finish, r10_pages, r10_prebuild, r10_scratch, r10_settle, R10Owner};
use super::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn r12_open(root: &Path) -> (Arc<Graph>, R10Owner) {
    let graph = Arc::new(Graph::open(root));
    graph
        .attach_direct_projection(root.join("private/projection.sqlite"))
        .unwrap();
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(20)));
    (graph, owner)
}

fn r12_ready_graph(tag: &str, pages: usize) -> (PathBuf, Arc<Graph>, R10Owner) {
    let root = r10_scratch(tag);
    r10_pages(&root, pages);
    let (graph, owner) = r12_open(&root);
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    (root, graph, owner)
}

/// Make pages p50..p59 contradict the schema's invariants without breaking
/// the schema itself: each of their blocks' page row is gone (its id moved),
/// so reads find contradictory rows while quick_check passes. The update
/// turns below touch only p0..p41, which lower cleanly beside the damage.
fn contradict(root: &Path) {
    let connection = rusqlite::Connection::open(root.join("private/projection.sqlite")).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    connection
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    assert!(
        connection
            .execute(
                "UPDATE pages SET page_id = page_id + 1000000 WHERE path GLOB '*p5[0-9].md'",
                [],
            )
            .unwrap()
            > 0
    );
}

/// R12-01: the decider asked "is a fresh build already replacing the
/// image?" through the progress counter, which also counts any update turn
/// longer than one batch. A contradiction met during such a turn spent its
/// once-per-projection token on a rebuild nobody ran.
#[test]
fn a_contradiction_during_a_bulk_update_turn_is_rebuilt() {
    contradiction_during_update_turn(41, true);
}

/// Control: the same read during a one-batch update turn.
#[test]
fn a_contradiction_during_a_small_update_turn_is_rebuilt() {
    contradiction_during_update_turn(21, false);
}

fn contradiction_during_update_turn(updates: usize, counted: bool) {
    let (root, graph, owner) = r12_ready_graph("r12-k1-bulk", 60);
    let projection = graph.direct_projection_test().unwrap();
    contradict(&root);
    let before = projection.fresh_builds_test();

    // A query meets the contradiction and pauses before its repair.
    let pause = graph.pause_next_failed_read_repair_test();
    let reader = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.run_query_bounded("(task TODO)", 20_000, 32 << 20))
    };
    pause.reached.wait();

    // Hold turn 1 (one update) so the rest queue into turn 2.
    let (release_first, first_released) = std::sync::mpsc::channel::<()>();
    let (first_reached_tx, first_reached) = std::sync::mpsc::channel::<()>();
    crate::direct_projection::before_next_apply_test(
        &root,
        Box::new(move || {
            let _ = first_reached_tx.send(());
            let _ = first_released.recv_timeout(Duration::from_secs(30));
        }),
    );
    fs::write(root.join("pages/p0.md"), "- TODO bulk0 [[p1]]\n").unwrap();
    graph.sync_file_checked(&root.join("pages/p0.md")).unwrap();
    first_reached.recv_timeout(Duration::from_secs(10)).unwrap();
    for index in 1..updates {
        let path = root.join("pages").join(format!("p{index}.md"));
        fs::write(&path, format!("- TODO bulk{index} [[p{}]]\n", index + 1)).unwrap();
        graph.sync_file_checked(&path).unwrap();
    }
    // Turn 1's batch fires the first hook; it arms a second that holds
    // turn 2 after its first batch.
    let (in_bulk_tx, in_bulk) = std::sync::mpsc::channel::<()>();
    let (release_bulk, bulk_released) = std::sync::mpsc::channel::<()>();
    let rearm = Arc::clone(&projection);
    let bulk_released = Arc::new(std::sync::Mutex::new(bulk_released));
    projection.after_next_lowering_batch_test(Box::new(move || {
        let bulk_released = Arc::clone(&bulk_released);
        rearm.after_next_lowering_batch_test(Box::new(move || {
            let _ = in_bulk_tx.send(());
            let _ = bulk_released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30));
        }));
    }));
    release_first.send(()).unwrap();
    in_bulk.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(
        projection.build_progress().is_some(),
        counted,
        "precondition: whether the update turn counts progress"
    );

    // The paused read asks for its repair mid-turn.
    pause.release.wait();
    std::thread::sleep(Duration::from_millis(500));
    release_bulk.send(()).unwrap();
    let _ = reader.join().unwrap();
    let _ = projection.wait_drained_test();
    r10_settle(&graph);

    let started = Instant::now();
    let mut last = None;
    while started.elapsed() < Duration::from_secs(8) {
        let answer = graph.run_query_bounded("(task TODO)", 20_000, 32 << 20);
        last = Some(
            answer
                .as_ref()
                .map(|a| a.groups.len())
                .map_err(|e| format!("{e:?}")),
        );
        if answer.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let state = projection.debug_state_test();
    let rebuilt = projection.fresh_builds_test() - before;
    r10_finish(root, graph, owner);
    assert_eq!(
        rebuilt, 1,
        "the contradictory image must be replaced exactly once (last answer {last:?}; {state})"
    );
}

/// R12-01, member 2: after its last batch and until it publishes, a fresh
/// build still owns the image, though the progress counter has stopped. A
/// rebuild asked for in that window used to queue a second whole-graph build.
#[test]
fn a_rebuild_asked_for_while_a_fresh_build_finishes_is_that_build() {
    let root = r10_scratch("r12-finish-window");
    r10_pages(&root, 12);
    let graph = Arc::new(Graph::open(&root));
    graph
        .attach_direct_projection(root.join("private/projection.sqlite"))
        .unwrap();
    let projection = graph.direct_projection_test().unwrap();
    let hold = projection.hold_fresh_publication_test();
    let owner = R10Owner::start(&graph);
    hold.0.wait();
    assert!(
        projection.build_progress().is_none(),
        "precondition: the window the progress counter no longer covers"
    );
    projection.request_rebuild();
    hold.1.wait();
    assert!(owner.wait_ready(Duration::from_secs(20)));
    let _ = projection.wait_drained_test();
    r10_settle(&graph);
    let builds = projection.fresh_builds_test();
    let state = projection.debug_state_test();
    r10_finish(root, graph, owner);
    assert_eq!(
        builds, 1,
        "a second fresh build ran for the one already finishing: {state}"
    );
}

/// R12-02: a generation move while a validation was owed was dropped, so
/// the index never became ready at the moved generation and page listings
/// parsed the graph.
#[test]
fn a_move_while_a_validation_is_owed_is_not_lost() {
    let root = r10_scratch("r12-advance-owed");
    r10_pages(&root, 12);
    r10_prebuild(&root, &root.join("private/projection.sqlite"));
    let (graph, owner) = r12_open(&root);
    assert!(owner.wait_ready(Duration::from_secs(10)));
    r10_settle(&graph);
    assert!(
        !graph.has_parsed_cache_test(),
        "precondition: a warm open parses nothing"
    );
    let parses_before = graph.consumer_page_parses_test();
    let pause = graph.pause_next_warm_before_enqueue_test();
    graph.direct_projection_owe_validation_test();
    pause.reached.wait();
    let at_walk = graph.cache_gen.load(std::sync::atomic::Ordering::Acquire);
    // The watcher meets p1 unreadable after the walk's drift check.
    graph.record_watcher_identity_failure(&root.join("pages/p1.md"), false);
    assert_ne!(
        graph.cache_gen.load(std::sync::atomic::Ordering::Acquire),
        at_walk,
        "precondition: the generation moved during the validation"
    );
    pause.release.wait();
    let ready = owner.wait_ready(Duration::from_secs(5));
    let state = graph.direct_projection_test().unwrap().debug_state_test();
    let _ = graph.list_pages();
    let parses = graph.consumer_page_parses_test() - parses_before;
    r10_finish(root, graph, owner);
    assert!(
        ready,
        "the index never became ready at the moved generation: {state}"
    );
    assert_eq!(parses, 0, "listing pages parsed the graph");
}

/// R12-05: a derived read that meets a row it cannot decode asks the one
/// decider for a new image instead of swallowing the error into a parse.
#[test]
fn a_corrupt_derived_row_asks_for_a_new_image() {
    let root = r10_scratch("r12-derived-corrupt");
    r10_pages(&root, 12);
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);
    let (graph, owner) = r12_open(&root);
    assert!(owner.wait_ready(Duration::from_secs(10)));
    r10_settle(&graph);
    let projection = graph.direct_projection_test().unwrap();
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
    let listed = graph.list_pages().len();
    let _ = projection.wait_drained_test();
    let rebuilt = projection.fresh_builds_test() - builds;
    // The rebuild parses every page; the listing itself must parse none.
    let parsed = graph.consumer_page_parses_test() - parses;
    let state = projection.debug_state_test();
    r10_finish(root, graph, owner);
    assert_eq!(listed, 12, "the listing is complete");
    assert_eq!(rebuilt, 1, "nobody replaced the damaged image: {state}");
    assert_eq!(
        parsed, 0,
        "the listing parsed the graph instead of waiting for the image"
    );
}

/// R12-01's shape: the progress counter is presentation. Only the indexing
/// bar reads it; a decision that consults it (as `fresh_build_owns_image`
/// did) answers "is something counting?", not "is a fresh build running?".
/// Exemplar for a decision: `DirectProjectionShared::fresh_build_running`.
#[test]
fn index_progress_is_presentation_only() {
    let mut uses = std::collections::BTreeMap::new();
    for file in crate::projection_producer_census::production_rust() {
        let count = file.code.matches("build_progress").count();
        if count > 0 {
            uses.insert(file.relative.clone(), count);
        }
    }
    let expected = [
        // The field and its initializer.
        ("crates/tine-core/src/direct_projection.rs", 2),
        // The one writer: the lowering loop.
        ("crates/tine-core/src/direct_projection/lowering.rs", 1),
        // The one reader, and the indexing bar that calls it.
        ("crates/tine-core/src/direct_projection/observers.rs", 2),
        ("crates/tine-core/src/model/projection_lifetime.rs", 1),
    ]
    .into_iter()
    .map(|(path, count)| (path.to_string(), count))
    .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        uses, expected,
        "the index progress counter is presentation-only: a decision about the \
         image must read the state that owns the answer (fresh_build_running, \
         pending), never the counter (GH #543, audit R12-01)"
    );
}

/// R12-05's shape: an index reader never discards a SQL error with `.ok()`.
/// `.reported(self)` hands the failure to `failure_owes_new_image`, the one
/// decider. The only `.ok()` left are on values that are not index answers.
#[test]
fn index_readers_report_damage() {
    const EXEMPT: [&str; 6] = [
        "parse_str",
        "PhysicalProjectionQueryReader::open",
        "set_query_rank_function",
        "explain_query_plan",
        "open_read_only",
        "open_writable",
    ];
    let mut offenders = Vec::new();
    let mut seen = 0;
    for file in crate::projection_producer_census::production_rust() {
        if file.relative != "crates/tine-core/src/direct_projection.rs"
            && file.relative != "crates/tine-core/src/direct_projection/derived_reads.rs"
        {
            continue;
        }
        seen += 1;
        for (at, _) in file.code.match_indices(".ok()") {
            let start = file.code[..at]
                .rfind([';', '{', '}'])
                .map_or(0, |boundary| boundary + 1);
            let statement = &file.code[start..at];
            if !EXEMPT.iter().any(|exempt| statement.contains(exempt)) {
                let line = file.code[..at].lines().count();
                offenders.push(format!("{}:{line}", file.relative));
            }
        }
    }
    assert_eq!(seen, 2, "both index reader files were scanned");
    assert!(
        offenders.is_empty(),
        "an index reader discards its SQL error with `.ok()`; use `.reported(self)` \
         so detected damage reaches failure_owes_new_image (GH #543, audit R12-05; \
         exemplar: DirectProjection::page_inventory): {offenders:?}"
    );
}

/// Reconciler round (GH #543): an update turn whose writes collide with
/// contradictory rows (here: orphaned blocks of every page, their page rows
/// moved) failed a constraint. It was taken as an ordinary failed turn on an
/// intact image, so its marks went back and failed again at every retry: the
/// index never recovered and every reader parsed the graph. The violation is
/// the contradiction's evidence and rebuilds the index once.
#[test]
fn an_update_turn_that_meets_contradictory_rows_rebuilds_the_index_once() {
    let (root, graph, owner) = r12_ready_graph("r12-turn-contradiction", 12);
    let projection = graph.direct_projection_test().unwrap();
    let connection = rusqlite::Connection::open(root.join("private/projection.sqlite")).unwrap();
    connection.busy_timeout(Duration::from_secs(5)).unwrap();
    connection
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    connection
        .execute("UPDATE pages SET page_id = page_id + 1000000", [])
        .unwrap();
    drop(connection);
    let before = projection.fresh_builds_test();
    fs::write(root.join("pages/p0.md"), "- TODO edited [[p1]]\n").unwrap();
    graph.sync_file_checked(&root.join("pages/p0.md")).unwrap();
    let started = Instant::now();
    while !(projection.fresh_builds_test() > before && graph.direct_projection_ready_test())
        && started.elapsed() < Duration::from_secs(10)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let state = projection.debug_state_test();
    let rebuilt = projection.fresh_builds_test() - before;
    let answer = graph
        .run_query_bounded("(task TODO)", 20_000, 32 << 20)
        .map(|answer| answer.groups.len());
    r10_finish(root, graph, owner);
    assert_eq!(rebuilt, 1, "the index was not rebuilt once: {state}");
    assert_eq!(
        answer.ok(),
        Some(12),
        "the rebuilt index answers every page"
    );
}
