//! GH #543, indexing audit round 14: the auditor's probes, pinned. Each
//! failed on the pre-reconciler protocol (`5a7e66b7`); the reconciler
//! (per-page generation marks, one survey) is what they now hold.

use super::gh543_r10::{r10_finish, r10_pages, r10_prebuild, r10_scratch, R10Owner};
use super::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn r14_warm_open(root: &Path) -> Arc<Graph> {
    let database = root.join("private/projection.sqlite");
    r10_prebuild(root, &database);
    let graph = Arc::new(Graph::open(root));
    graph.attach_direct_projection(database).unwrap();
    graph
}

/// R14-A: a page whose parse panics, changed while Tine was closed. The old
/// launch walk could not parse it, returned `Retry`, and nothing decided
/// `Fresh`: the owner re-walked under the backoff and the index was never
/// ready. The survey records the failure and the page keeps its rows.
/// mode 0: the page turned panicking between sessions.
/// mode 1: the page panicked already in the session that built the image.
fn panic_page_warm_open(mode: u8) -> (bool, usize, usize, u64, String, String) {
    let root = r10_scratch(&format!("r14-panic-{mode}"));
    r10_pages(&root, 12);
    let bad = root.join("pages/p3.md");
    if mode == 1 {
        fs::write(&bad, format!("- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n")).unwrap();
    }
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);
    if mode == 0 {
        fs::write(&bad, format!("- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n")).unwrap();
    }
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let projection = graph.direct_projection_test().unwrap();
    let builds = projection.fresh_builds_test();
    let owner = R10Owner::start(&graph);
    let ready = owner.wait_ready(Duration::from_secs(12));
    let passes = graph.owner_passes_test();
    let parses = graph.page_build_parses_test();
    let rebuilt = projection.fresh_builds_test() - builds;
    let query = format!("{:?}", graph.run_query("(task TODO)").map(|g| g.len()));
    let state = projection.debug_state_test();
    r10_finish(root, graph, owner);
    eprintln!(
        "R14-A mode={mode} ready={ready} owner_passes={passes} parses={parses} rebuilt={rebuilt} query={query} {state}"
    );
    (ready, passes, parses, rebuilt, query, state)
}

#[test]
fn gh543_r14_panicking_page_changed_between_sessions_becomes_ready() {
    let (ready, passes, _, _, query, state) = panic_page_warm_open(0);
    assert!(
        ready,
        "never ready after 12 s; owner_passes={passes} query={query} {state}"
    );
}

#[test]
fn gh543_r14_panicking_page_from_an_earlier_session_becomes_ready() {
    let (ready, passes, _, _, query, state) = panic_page_warm_open(1);
    assert!(
        ready,
        "never ready after 12 s; owner_passes={passes} query={query} {state}"
    );
}

/// R14 lead L1: a named path the walk never listed (a journal migration
/// target, a rescued page) during a mid-session validation walk; the old
/// walk's verdict dropped it from the index.
/// mode 0: migration; mode 1: rescue. `revalidate`: owe a validation first
/// (as a failed turn on an intact image does); false is the control.
fn reread_new_path_mid_session(mode: u8, revalidate: bool) -> (bool, bool, usize, String) {
    let root = r10_scratch(&format!("r14-reread-{mode}-{revalidate}"));
    r10_pages(&root, 12);
    fs::write(
        root.join("journals/Jun 18th, 2026.md"),
        "- journal rfourteen\n",
    )
    .unwrap();
    fs::write(root.join("pages/p5.md"), "- rescued rfourteen\n").unwrap();
    let graph = r14_warm_open(&root);
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_ready(Duration::from_secs(20)));
    super::gh543_r10::r10_settle(&graph);
    let projection = graph.direct_projection_test().unwrap();
    let passes0 = graph.owner_passes_test();
    let pause = revalidate.then(|| {
        let pause = graph.pause_next_warm_after_read_test();
        graph.direct_projection_owe_validation_test();
        pause.reached.wait();
        pause
    });
    let target = match mode {
        0 => {
            assert_eq!(graph.migrate_journal_filenames_checked().unwrap(), 1);
            "journals/2026_06_18.md".to_owned()
        }
        _ => {
            graph
                .rename_file_to_page("pages/p5.md", "rescued five")
                .unwrap();
            "pages/rescued five.md".to_owned()
        }
    };
    assert!(
        root.join(&target).exists(),
        "precondition: {target} on disk"
    );
    // Let the delta reach the live image before the walk validates.
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(600) {
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(pause) = pause {
        pause.release.wait();
    }
    let ready = owner.wait_ready(Duration::from_secs(20));
    super::gh543_r10::r10_settle(&graph);
    let inventory = graph.direct_projection_page_inventory();
    let indexed = inventory
        .as_ref()
        .is_some_and(|(_, entries)| entries.iter().any(|entry| entry.rel_path == target));
    let found = format!("{:?}", graph.run_query("\"rfourteen\"").map(|g| g.len()));
    let passes = graph.owner_passes_test() - passes0;
    let state = projection.debug_state_test();
    r10_finish(root, graph, owner);
    eprintln!(
        "R14-L1 mode={mode} revalidate={revalidate} ready={ready} indexed={indexed} search={found} passes={passes} {state}"
    );
    (ready, indexed, passes, found)
}

#[test]
fn gh543_r14_mid_session_walk_keeps_a_migrated_journal() {
    let (ready, indexed, _, found) = reread_new_path_mid_session(0, true);
    assert!(ready);
    assert!(
        indexed,
        "the migrated journal is missing from the index; search={found}"
    );
}

#[test]
fn gh543_r14_mid_session_walk_keeps_a_rescued_page() {
    let (ready, indexed, _, found) = reread_new_path_mid_session(1, true);
    assert!(ready);
    assert!(
        indexed,
        "the rescued page is missing from the index; search={found}"
    );
}

#[test]
fn gh543_r14_control_reread_without_a_walk() {
    for mode in [0, 1] {
        let (ready, indexed, _, found) = reread_new_path_mid_session(mode, false);
        assert!(ready && indexed, "control mode={mode} search={found}");
    }
}

/// R14-03 (seed 1085): an editor save retires the live file, then publishes
/// the new one. A validation walk in that window finds the page gone and
/// its verdict named it a deletion; the save published before the walk's
/// next drift check. The page's update was lowered, and the carried repair
/// then deleted the page's fresh rows. One round per call; returns whether
/// many rounds lost the page. Under the reconciler the save's mark is newer
/// than the survey's, and an absence is confirmed by a read under the
/// identity gate, so the save always wins.
fn save_window_walk_round(round: usize) -> (bool, String) {
    let root = r10_scratch(&format!("r14-savewin-{round}"));
    r10_pages(&root, 12);
    let graph = r14_warm_open(&root);
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_ready(Duration::from_secs(20)));
    super::gh543_r10::r10_settle(&graph);
    let retired = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let saver = {
        let graph = Arc::clone(&graph);
        let retired = Arc::clone(&retired);
        let release = Arc::clone(&release);
        std::thread::spawn(move || {
            GRAPH_TEXT_WRITE_AFTER_RETIRE.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    retired.wait();
                    release.wait();
                    Ok(())
                }));
            });
            let mut page = graph.load_named("p5", PageKind::Page).unwrap().unwrap();
            page.blocks = markdown_page_dto("p5", "p5", "- TODO saved rfourteen [[p6]]\n")
                .unwrap()
                .blocks;
            let base = page.rev.clone();
            graph
                .save_page(&page, base.as_deref())
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    };
    retired.wait();
    assert!(
        !root.join("pages/p5.md").exists(),
        "precondition: the live name is retired"
    );
    let enqueue = graph.pause_next_warm_before_enqueue_test();
    graph.direct_projection_owe_validation_test();
    enqueue.reached.wait();
    // Offer the validation and publish the save at once, as seed 1085 did.
    enqueue.release.wait();
    release.wait();
    let saved = saver.join().unwrap();
    assert!(owner.wait_ready(Duration::from_secs(20)));
    super::gh543_r10::r10_settle(&graph);
    let on_disk = root.join("pages/p5.md").exists();
    let indexed = graph
        .direct_projection_page_inventory()
        .as_ref()
        .is_some_and(|(_, entries)| entries.iter().any(|entry| entry.rel_path == "pages/p5.md"));
    let found = format!("{:?}", graph.run_query("\"rfourteen\"").map(|g| g.len()));
    r10_finish(root, graph, owner);
    eprintln!(
        "R14-03 round={round} saved={saved:?} on_disk={on_disk} indexed={indexed} search={found}"
    );
    (on_disk && !indexed, found)
}

#[test]
fn gh543_r14_a_page_saved_during_a_validation_walk_stays_indexed() {
    let rounds = 10;
    let lost = (0..rounds)
        .filter(|round| save_window_walk_round(*round).0)
        .count();
    eprintln!("R14-03 lost the saved page in {lost}/{rounds} rounds");
    assert_eq!(
        lost, 0,
        "a page saved during a validation walk was deleted from the index"
    );
}

/// The survey that finds a page it cannot parse publishes that failure
/// before readiness. A page creation waiting for the launch survey asks,
/// once woken, which identities are unknown: it read no failure, and either
/// trusted the stored identity of a page whose content it never parsed, or
/// found the failure a moment later and parsed the whole graph only to
/// refuse. Name-only creation refuses on an unknown identity, without the
/// parse.
#[test]
fn gh543_a_creation_waiting_for_the_survey_refuses_without_parsing() {
    let root = r10_scratch("r14-create-during-survey");
    r10_pages(&root, 12);
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);
    fs::write(
        root.join("pages/p3.md"),
        format!("- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n"),
    )
    .unwrap();
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let survey = graph.pause_next_warm_after_read_test();
    let owner = R10Owner::start(&graph);
    survey.reached.wait();
    let asked = graph.pause_next_derived_read_test();
    let parses = graph.consumer_page_parses_test();
    let creator = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || {
            graph
                .save_page(
                    &markdown_page_dto("fresh", "fresh", "- fresh page\n").unwrap(),
                    None,
                )
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    };
    asked.reached.wait();
    asked.release.wait();
    // Let the creation reach its wait for the survey.
    std::thread::sleep(Duration::from_millis(100));
    survey.release.wait();
    let created = creator.join().unwrap();
    assert!(owner.wait_ready(Duration::from_secs(20)));
    let parsed = graph.consumer_page_parses_test() - parses;
    let state = graph.direct_projection_test().unwrap().debug_state_test();
    r10_finish(root, graph, owner);
    assert!(
        created
            .as_ref()
            .is_err_and(|error| error.contains("unreadable or unparseable")
                || error.contains("failure-bearing")),
        "a creation beside an unparseable page was not refused: {created:?} {state}"
    );
    assert_eq!(
        parsed, 0,
        "the creation parsed the graph to refuse: {created:?} {state}"
    );
}
