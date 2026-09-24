//! GH #543 audit round 15: the auditor's probes, kept as regressions. Each
//! failed at `c79a2546` (evidence `gh550-batch/indexing-audit-r15/`); the
//! record of unreadable pages now outlives the parsed cache and changes only
//! by path (`model/unreadable_pages.rs`).

use super::gh543_r10::{r10_finish, r10_pages, r10_prebuild, r10_scratch, r10_settle, R10Owner};
use super::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn r15_warm_open_with_panicking_owner(tag: &str) -> (PathBuf, Arc<Graph>, R10Owner) {
    let root = r10_scratch(tag);
    r10_pages(&root, 12);
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);
    fs::write(
        root.join("pages/p3.md"),
        format!("title:: fresh\n\n- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n"),
    )
    .unwrap();
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_ready(Duration::from_secs(20)));
    assert!(owner.wait_settled(Duration::from_secs(20)));
    r10_settle(&graph);
    (root, graph, owner)
}

fn r15_create(graph: &Graph, name: &str) -> Result<(), String> {
    graph
        .save_page(&markdown_page_dto(name, name, "- created\n").unwrap(), None)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// R15-02: a rename, merge, rescue or journal migration discarded the parsed
/// cache and with it `page_index_failures`, the only record of which pages the
/// launch survey could not read. Name-only creation then no longer knew an
/// unreadable page may own the name (DIRECT-REF-CREATE-UNREADABLE-OWNER).
/// mode 0: rename; 1: control (no rename); 2: an edit, then a rename;
/// 3: a journal migration.
fn creation_after_rename(mode: u8) -> (Result<(), String>, Result<(), String>, Vec<String>, bool) {
    let (root, graph, owner) =
        r15_warm_open_with_panicking_owner(&format!("r15-create-rename-{mode}"));
    let before = r15_create(&graph, "fresh");
    if mode == 2 {
        let edited = root.join("pages/p0.md");
        fs::write(&edited, "- external edit\n").unwrap();
        graph.sync_file_checked(&edited).unwrap();
        r10_settle(&graph);
        let _ = graph.page_aliases();
    }
    eprintln!(
        "R15-01 mode={mode} memo={:?}",
        graph.current_page_inventory_snapshot().map(|e| e.len())
    );
    if mode == 3 {
        fs::write(
            root.join("journals").join("Jun 18th, 2026.md"),
            "- TODO recovered\n",
        )
        .unwrap();
        let moved = graph.migrate_journal_filenames_checked().unwrap();
        r10_settle(&graph);
        eprintln!(
            "R15-01 mode=3 migrated={moved} failures right after migration={:?}",
            graph.page_index_failures()
        );
    }
    if mode == 0 || mode == 2 {
        let parses = graph.page_build_parses_test();
        let consumer = graph.consumer_page_parses_test();
        let cached = graph.has_parsed_cache_test();
        graph.rename_page("p5", "renamed").unwrap();
        eprintln!(
            "R15-01 mode={mode} failures right after rename={:?}",
            graph.page_index_failures()
        );
        let during = (
            graph.page_build_parses_test() - parses,
            graph.consumer_page_parses_test() - consumer,
            graph.has_parsed_cache_test(),
        );
        r10_settle(&graph);
        eprintln!("R15-01 mode={mode} cached_before={cached} rename parses(all,consumer,cache_after)={during:?} after_settle=({},{})", graph.page_build_parses_test() - parses, graph.consumer_page_parses_test() - consumer);
    }
    let failures = graph.page_index_failures();
    let after = r15_create(&graph, "fresh");
    let duplicate = root.join("pages/fresh.md").exists();
    r10_finish(root, graph, owner);
    eprintln!("R15-01 mode={mode} before={before:?} failures={failures:?} after={after:?} created_duplicate={duplicate}");
    (before, after, failures, duplicate)
}

#[test]
fn gh543_r15_rename_keeps_the_unreadable_owner_refusal() {
    let (before, after, failures, duplicate) = creation_after_rename(0);
    assert!(
        before.is_err(),
        "precondition: refused before the rename: {before:?}"
    );
    assert!(
        after.is_err() && !duplicate,
        "after a rename, creation beside an unreadable owner was allowed: failures={failures:?} after={after:?}"
    );
}

#[test]
fn gh543_r15_control_no_rename_keeps_refusing() {
    let (before, after, _, duplicate) = creation_after_rename(1);
    assert!(before.is_err() && after.is_err() && !duplicate);
}

#[test]
fn gh543_r15_rename_after_an_edit_keeps_the_unreadable_owner_refusal() {
    let (before, after, failures, duplicate) = creation_after_rename(2);
    assert!(
        before.is_err(),
        "precondition: refused before the rename: {before:?}"
    );
    assert!(
        after.is_err() && !duplicate,
        "after an edit and a rename, creation beside an unreadable owner was allowed: failures={failures:?} after={after:?}"
    );
}

#[test]
fn gh543_r15_journal_migration_keeps_the_unreadable_owner_refusal() {
    let (before, after, failures, duplicate) = creation_after_rename(3);
    assert!(
        before.is_err(),
        "precondition: refused before the migration: {before:?}"
    );
    assert!(
        after.is_err() && !duplicate,
        "after a journal migration, creation beside an unreadable owner was allowed: failures={failures:?} after={after:?}"
    );
}

/// R15-11 control: a Fresh pass over a cold-built parsed cache that recorded
/// one unreadable page parses nothing again.
#[test]
fn gh543_r15_fresh_pass_over_a_cache_with_one_failure_does_not_reparse_the_graph() {
    let root = r10_scratch("r15-fresh-over-failed-cache");
    r10_pages(&root, 12);
    fs::write(
        root.join("pages/p3.md"),
        format!("- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n"),
    )
    .unwrap();
    let graph = Arc::new(Graph::open(&root));
    graph
        .attach_direct_projection(root.join("private/projection.sqlite"))
        .unwrap();
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(20)));
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    let projection = graph.direct_projection_test().unwrap();
    // An ordinary edit moves the generation, as any session does.
    fs::write(root.join("pages/p1.md"), "- edited r15 [[p2]]\n").unwrap();
    graph.sync_file_checked(&root.join("pages/p1.md")).unwrap();
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    let cached = graph.has_parsed_cache_test();
    let failures = graph.page_index_failures();
    let parses = graph.page_build_parses_test();
    let builds = projection.fresh_builds_test();
    let passes = graph.owner_passes_test();
    projection.inject_next_statement_failure();
    let _ = graph.search("p1", 20);
    let started = Instant::now();
    while projection.fresh_builds_test() == builds && started.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    let rebuilt = projection.fresh_builds_test() - builds;
    let parsed = graph.page_build_parses_test() - parses;
    let state = projection.debug_state_test();
    eprintln!(
        "R15-02 owner_passes={} failures_after={:?} cached_after={}",
        graph.owner_passes_test() - passes,
        graph.page_index_failures(),
        graph.has_parsed_cache_test()
    );
    r10_finish(root, graph, owner);
    eprintln!("R15-02 cached={cached} failures={failures:?} rebuilt={rebuilt} parsed={parsed} state={state}");
    assert!(cached && !failures.is_empty(), "precondition");
    assert_eq!(rebuilt, 1, "precondition: damage owed one fresh image");
    assert!(
        parsed <= 1,
        "the Fresh pass re-parsed {parsed} pages although the installed cache held every \
         readable one; only the unreadable page needed a retry"
    );
}

static MID_TURN_REFS: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

/// R15-07: the index keeps an unreadable page's stored rows (the retained
/// policy); the parsed cache a read fell back to while the index was mid-turn
/// does not hold that page, so the same read answered differently depending
/// on which one answered.
#[test]
fn gh543_r15_cache_fallback_answers_like_the_index_for_an_unreadable_page() {
    let root = r10_scratch("r15-cache-vs-index-unreadable");
    r10_pages(&root, 12);
    fs::write(
        root.join("pages/p3.md"),
        "- zebraquartz stored [[zebrapage]]\n",
    )
    .unwrap();
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);
    fs::write(
        root.join("pages/p3.md"),
        format!("- zebraquartz now {TEST_PAGE_PARSE_PANIC_SENTINEL}\n"),
    )
    .unwrap();
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(20)));
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    let refs = |g: &Graph| {
        g.referenced_page_names_versioned(None)
            .names
            .unwrap_or_default()
            .iter()
            .any(|name| name.to_lowercase().contains("zebrapage"))
    };
    let refs_from_index = refs(&graph);
    let from_index = graph.search("zebraquartz", 20).unwrap().len();
    // An acting read installs the parsed cache (export, asset listing).
    let _ = graph.try_with_pages(|pages| pages.len()).unwrap();
    assert!(graph.has_parsed_cache_test());
    // Hold the worker's next turn so the index is mid-turn.
    let (reached_tx, reached_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    crate::direct_projection::before_next_apply_test(
        &root,
        Box::new(move || {
            let _ = reached_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(20));
        }),
    );
    fs::write(root.join("pages/p1.md"), "- edited r15c [[p2]]\n").unwrap();
    graph.sync_file_checked(&root.join("pages/p1.md")).unwrap();
    let held = reached_rx.recv_timeout(Duration::from_secs(5)).is_ok();
    eprintln!("R15-03 worker held mid-turn: {held}");
    let (tx, rx) = std::sync::mpsc::channel();
    let g = Arc::clone(&graph);
    std::thread::spawn(move || {
        let names = refs(&g);
        eprintln!("R15-03 mid-turn referenced names hold zebrapage: {names}");
        *MID_TURN_REFS.lock().unwrap() = Some(names);
        let _ = tx.send(g.search("zebraquartz", 20).map(|r| r.len()));
    });
    let from_cache = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(answer) => answer.unwrap(),
        Err(_) => {
            eprintln!("R15-03 mid-turn search waited for the turn (no cache fallback)");
            let _ = release_tx.send(());
            rx.recv_timeout(Duration::from_secs(20)).unwrap().unwrap()
        }
    };
    let _ = release_tx.send(());
    r10_settle(&graph);
    let after = graph.search("zebraquartz", 20).unwrap().len();
    r10_finish(root, graph, owner);
    eprintln!("R15-03 refs_from_index={refs_from_index}");
    assert_eq!(
        Some(refs_from_index),
        *MID_TURN_REFS.lock().unwrap(),
        "referenced page names answered by the parsed cache mid-turn differ from the index's \
         answer: the index keeps the unreadable page's stored rows, the cache does not"
    );
    eprintln!("R15-03 index={from_index} cache_fallback={from_cache} after={after}");
    assert_eq!(
        from_index, from_cache,
        "a search answered by the parsed cache differs from the same search answered by the index"
    );
}

/// R15-01: a launch survey published its failures only if the generation did
/// not move while it ran. An external edit delivered mid-survey dropped the
/// unreadable page's record, and name-only creation then made a second page
/// for its name.
/// mode 0: an external edit mid-survey; mode 1: control (nothing mid-survey).
fn survey_failures_with_a_mid_survey_move(mode: u8) -> (Vec<String>, Result<(), String>, bool) {
    let root = r10_scratch(&format!("r15-survey-move-{mode}"));
    r10_pages(&root, 12);
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);
    fs::write(
        root.join("pages/p3.md"),
        format!("title:: fresh\n\n- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n"),
    )
    .unwrap();
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let pause = graph.pause_next_warm_after_read_test();
    let owner = R10Owner::start(&graph);
    pause.reached.wait();
    if mode == 0 {
        let edited = root.join("pages/p1.md");
        fs::write(&edited, "- external mid-survey [[p2]]\n").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let g = Arc::clone(&graph);
        std::thread::spawn(move || {
            let _ = tx.send(
                g.sync_file_checked(&edited)
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
            );
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(result) => eprintln!("R15-04 mid-survey sync -> {result:?}"),
            Err(_) => {
                eprintln!("R15-04 mid-survey sync blocked behind the survey; drifting instead");
                graph.drift_generation_test();
            }
        }
    }
    pause.release.wait();
    assert!(owner.wait_settled(Duration::from_secs(20)));
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    let failures = graph.page_index_failures();
    let created = r15_create(&graph, "fresh");
    let duplicate = root.join("pages/fresh.md").exists();
    r10_finish(root, graph, owner);
    eprintln!("R15-04 mode={mode} failures={failures:?} create={created:?} duplicate={duplicate}");
    (failures, created, duplicate)
}

#[test]
fn gh543_r15_survey_keeps_its_failures_across_a_mid_survey_edit() {
    let (failures, created, duplicate) = survey_failures_with_a_mid_survey_move(0);
    assert!(
        !failures.is_empty() && created.is_err() && !duplicate,
        "an edit delivered during the launch survey dropped the unreadable page's record \
         (failures={failures:?}); creating its name made a second page ({created:?})"
    );
}

#[test]
fn gh543_r15_survey_keeps_its_failures_control() {
    let (failures, created, duplicate) = survey_failures_with_a_mid_survey_move(1);
    assert!(
        !failures.is_empty() && created.is_err() && !duplicate,
        "{failures:?} {created:?}"
    );
}

/// R15-03: deleting the unreadable page -- the remedy the notice suggests --
/// left its failure record, and creating its name stayed refused for the
/// session.
/// mode 0: the watcher's delete (`sync_deleted_file`); mode 1: a change
/// event that finds the file gone (`sync_file_checked`), then the delete the
/// watcher delivers for it, as `watcher.rs` does.
fn create_after_deleting_the_unreadable_page(
    mode: u8,
) -> (Vec<String>, Vec<String>, Result<(), String>) {
    let (root, graph, owner) =
        r15_warm_open_with_panicking_owner(&format!("r15-delete-unreadable-{mode}"));
    let before = graph.page_index_failures();
    let path = root.join("pages/p3.md");
    fs::remove_file(&path).unwrap();
    let synced = if mode == 0 {
        graph.sync_deleted_file(&path).map(|e| e.is_some())
    } else {
        let changed = graph.sync_file_checked(&path).map(|e| e.is_some());
        assert!(
            !graph
                .page_index_failures()
                .iter()
                .any(|f| f == "pages/p3.md"),
            "a change event that found the file gone kept it recorded as unreadable"
        );
        changed.and_then(|_| graph.sync_deleted_file(&path).map(|e| e.is_some()))
    };
    r10_settle(&graph);
    let after = graph.page_index_failures();
    // `p3.md` carried `title:: fresh`; its file stem is `p3`.
    let created = r15_create(&graph, "p3");
    r10_finish(root, graph, owner);
    eprintln!("R15-03 mode={mode} synced={synced:?} before={before:?} after={after:?} create_p3={created:?}");
    (before, after, created)
}

#[test]
fn gh543_r15_deleting_the_unreadable_page_clears_its_refusal_watcher_delete() {
    let (before, after, created) = create_after_deleting_the_unreadable_page(0);
    assert!(!before.is_empty(), "precondition");
    assert!(
        after.is_empty() && created.is_ok(),
        "the unreadable page was deleted, yet its failure stays ({after:?}) and creating its \
         name is refused: {created:?}"
    );
}

#[test]
fn gh543_r15_deleting_the_unreadable_page_clears_its_refusal_sync_file() {
    let (before, after, created) = create_after_deleting_the_unreadable_page(1);
    assert!(!before.is_empty(), "precondition");
    assert!(
        after.is_empty() && created.is_ok(),
        "the unreadable page was deleted, yet its failure stays ({after:?}) and creating its \
         name is refused: {created:?}"
    );
}

/// R15-08: a fresh build that fails deterministically with a constraint
/// violation -- a lowering defect no rebuild can fix -- was rebuilt on every
/// backoff; the `contradiction_rebuilt` latch was bypassed for fresh builds.
#[test]
fn gh543_r15_a_deterministically_failing_fresh_build_is_not_repeated() {
    let root = r10_scratch("r15-fresh-build-loop");
    r10_pages(&root, 12);
    let graph = Arc::new(Graph::open(&root));
    graph
        .attach_direct_projection(root.join("private/projection.sqlite"))
        .unwrap();
    let projection = graph.direct_projection_test().unwrap();
    projection.fail_every_fresh_publication_test("UNIQUE constraint failed: blocks.result_id");
    let owner = R10Owner::start(&graph);
    std::thread::sleep(Duration::from_secs(9));
    let builds = projection.fresh_builds_test();
    let passes = graph.owner_passes_test();
    let parses = graph.page_build_parses_test();
    let state = projection.debug_state_test();
    let progress = projection.progress_at(graph.cache_generation());
    r10_finish(root, graph, owner);
    eprintln!("R15-06 fresh_builds_in_9s={builds} owner_passes={passes} page_parses={parses} state={state}");
    // Since GH #594 (L1) this is every failure's rule, not a constraint's own.
    assert!(
        builds <= u64::from(crate::direct_projection::INDEX_ATTEMPTS),
        "a fresh build failing on the same constraint every time was started {builds} times in \
         9 s; after its attempts the index stays failed until the user retries"
    );
    assert_eq!(
        progress,
        crate::direct_projection::ProjectionProgress::Failed(
            crate::query::IndexFailureClass::Constraint
        ),
        "the index says it failed, and why"
    );
}

/// R15-09: one page whose bytes are not UTF-8 fails every rename. Until a
/// rename skips and reports such a referrer (queued), the error names it.
#[test]
fn gh543_r15_a_rename_an_unreadable_page_fails_names_that_page() {
    let (root, graph, owner) = r15_warm_open_with_panicking_owner("r15-rename-non-utf8");
    fs::write(
        root.join("pages/binary.md"),
        [b'-', b' ', 0xff, 0xfe, b'\n'],
    )
    .unwrap();
    let _ = graph.sync_file_checked(&root.join("pages/binary.md"));
    r10_settle(&graph);
    let renamed = graph.rename_page("p5", "renamed-r15");
    let failures = graph.page_index_failures();
    r10_finish(root, graph, owner);
    eprintln!("R15-09 failures={failures:?} rename={renamed:?}");
    let error = renamed
        .expect_err("a rename that cannot read a referrer fails")
        .to_string();
    assert!(
        error.contains("pages/binary.md"),
        "the rename error names no file, so the user cannot find it: {error}"
    );
}

/// R15-06: an unreadable page whose `title::` is a date in another accepted
/// format owns that journal; creating the journal by name was not refused.
#[test]
fn gh543_r15_an_unreadable_date_titled_page_refuses_its_journal() {
    let root = r10_scratch("r15-date-title-owner");
    r10_pages(&root, 12);
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);
    fs::write(
        root.join("pages/p3.md"),
        format!("title:: 2026-09-23\n\n- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n"),
    )
    .unwrap();
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(20)));
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    let failures = graph.page_index_failures();
    let page = graph
        .save_page(
            &markdown_page_dto("Sep 23rd, 2026", "Sep 23rd, 2026", "- created\n").unwrap(),
            None,
        )
        .map(|_| ())
        .map_err(|e| e.to_string());
    let created: Vec<_> = ["journals/2026_09_23.md", "pages/Sep 23rd, 2026.md"]
        .iter()
        .filter(|rel| root.join(rel).exists())
        .map(|rel| rel.to_string())
        .collect();
    r10_finish(root, graph, owner);
    eprintln!("R15-08 failures={failures:?} create={page:?} created={created:?}");
    assert!(page.is_err() && created.is_empty(), "an unreadable page titled with the date did not refuse creating that name: {page:?} {created:?}");
}

/// R15-04: a watcher event for a file that vanished before it was read
/// recorded it as unreadable; nothing cleared it.
#[test]
fn gh543_r15_a_vanished_file_is_not_recorded_as_unreadable() {
    let (root, graph, owner) = r15_warm_open_with_panicking_owner("r15-vanished");
    let path = root.join("pages/gone.md");
    let synced = graph.sync_file_checked(&path).map(|e| e.is_some());
    r10_settle(&graph);
    let failures = graph.page_index_failures();
    let created = r15_create(&graph, "gone");
    let unrelated = r15_create(&graph, "unrelated-r15");
    r10_finish(root, graph, owner);
    eprintln!("R15-09 synced={synced:?} failures={failures:?} create_gone={created:?} create_unrelated={unrelated:?}");
    assert!(
        !failures.contains(&"pages/gone.md".to_owned()) && created.is_ok(),
        "a file that never existed was recorded as unreadable ({failures:?}); creating its \
         name is refused: {created:?}"
    );
}

/// R15-05: a listing skip for a FIFO in pages/ refused every name-only
/// creation, whatever the name: a FIFO is never graph text.
#[test]
fn gh543_r15_a_listing_skip_does_not_refuse_every_creation() {
    let root = r10_scratch("r15-fifo");
    r10_pages(&root, 12);
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);
    let fifo = root.join("pages/pipe.md");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success());
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(20)));
    assert!(owner.wait_ready(Duration::from_secs(20)));
    r10_settle(&graph);
    let failures = graph.page_index_failures();
    let created = r15_create(&graph, "unrelated-r15");
    r10_finish(root, graph, owner);
    eprintln!("R15-10 failures={failures:?} create_unrelated={created:?}");
    assert!(
        created.is_ok(),
        "a FIFO in pages/ refused creating an unrelated page: {created:?}"
    );
}

/// GH #543 (I-13, audit R15-07): every generation-keyed answer the index
/// gives is asked at a generation the readiness wait handed out. An answer
/// asked at a generation read straight from `cache_gen` declines whenever a
/// turn is still applying it, and the caller answers from a parsed cache
/// that lacks what the index holds for an unreadable page — the autocomplete
/// names did. The answer set is derived: every `DirectProjection` method
/// taking the generation first and returning `Option`, plus every helper
/// that is handed a generation and asks one. A call is waited when its
/// function calls `self.indexed_read(` (or is the bounded reference wait).
/// Imitate `Graph::direct_projection_real_page_names` in
/// `model/direct_query.rs`.
#[test]
fn index_answers_are_asked_at_a_waited_generation() {
    use std::path::Path;
    fn sources(dir: &Path, out: &mut Vec<(String, Vec<String>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                sources(&path, out);
            } else if name.ends_with(".rs") && !name.contains("test") {
                let text = fs::read_to_string(&path).unwrap();
                let rel = path.to_string_lossy().into_owned();
                out.push((rel, text.lines().map(str::to_owned).collect()));
            }
        }
    }
    /// The function around line `at`: its start line and name.
    fn enclosing(lines: &[String], at: usize) -> Option<(usize, String)> {
        for start in (0..=at).rev() {
            let line = &lines[start];
            let Some(index) = line.find("fn ") else {
                continue;
            };
            let name: String = line[index + 3..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let rest = &line[index + 3 + name.len()..];
            if !name.is_empty() && (rest.starts_with('(') || rest.starts_with('<')) {
                return Some((start, name));
            }
        }
        None
    }
    fn signature(lines: &[String], start: usize) -> String {
        let joined = lines[start..(start + 10).min(lines.len())].join(" ");
        let head = joined.split('{').next().unwrap_or_default();
        head.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace("( ", "(")
    }
    /// The function's body, up to the next item at its own indentation.
    fn body(lines: &[String], start: usize) -> String {
        let indent = lines[start].len() - lines[start].trim_start().len();
        let end = (start + 1..lines.len())
            .find(|&at| {
                let line = &lines[at];
                line.trim_start().starts_with('}') && line.len() - line.trim_start().len() == indent
            })
            .unwrap_or(lines.len());
        lines[start..end].join(" ")
    }
    // Launch design D3: the answers take the generation inside a `ReadAt`.
    let takes_generation = |sig: &str| {
        (sig.contains("generation: u64") || sig.contains("ReadAt")) && sig.contains("-> Option<")
    };

    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let mut answers = std::collections::BTreeSet::new();
    for (file, lines) in &files {
        if !file.contains("direct_projection") {
            continue;
        }
        for (at, line) in lines.iter().enumerate() {
            if !line.contains("pub(crate) fn ") {
                continue;
            }
            let Some((_, name)) = enclosing(lines, at) else {
                continue;
            };
            let sig = signature(lines, at);
            if takes_generation(&sig)
                && (sig.contains("(&self, generation: u64")
                    || sig.contains("(&self, cache_generation: u64")
                    || sig.contains("(&self, at: ReadAt"))
            {
                answers.insert(name);
            }
        }
    }
    assert!(
        answers.contains("referenced_page_names") && answers.contains("real_page_names"),
        "the census found no index answers: {answers:?}"
    );
    let mut violations = Vec::new();
    loop {
        let known = answers.len();
        violations.clear();
        for (file, lines) in &files {
            if file.contains("direct_projection") {
                continue;
            }
            for (at, line) in lines.iter().enumerate() {
                for name in answers.clone() {
                    if !line.contains(&format!("projection.{name}("))
                        && !line.contains(&format!("self.{name}(projection"))
                    {
                        continue;
                    }
                    let Some((start, caller)) = enclosing(lines, at) else {
                        continue;
                    };
                    if lines[start.saturating_sub(3)..start]
                        .iter()
                        .any(|line| line.contains("#[cfg(test)]"))
                    {
                        continue;
                    }
                    let text = body(lines, start);
                    if text.contains("self.indexed_read(")
                        || text.contains(".wait_for_reference_generation(")
                    {
                        continue;
                    }
                    if takes_generation(&signature(lines, start)) {
                        answers.insert(caller);
                    } else {
                        violations.push(format!("{file}:{} {caller} asks {name}", at + 1));
                    }
                }
            }
        }
        if answers.len() == known {
            break;
        }
    }
    assert!(
        answers.contains("direct_projection_page_inventory_at"),
        "the census lost the generation-handed helpers: {answers:?}"
    );
    assert!(
        violations.is_empty(),
        "an index answer must be asked at a generation Graph::indexed_read waited for \
         (I-13, R15-07); imitate Graph::direct_projection_real_page_names: {violations:?}"
    );
}

/// GH #543 acceptance probe on a real-shaped graph: the launch, reopen and
/// ordinary edits cost no whole-graph parse outside the index's own passes.
/// It works on a copy, so the source graph is never written, and prints
/// counts and durations only, never content.
///
/// ```text
/// TINE_GH543_PROBE_GRAPH=~/research/logseq-anonymized \
///   cargo test --release -p tine-core gh543_real_graph_probe -- --ignored --nocapture
/// ```
#[test]
#[ignore = "needs a real graph named by TINE_GH543_PROBE_GRAPH"]
fn gh543_real_graph_probe() {
    let Ok(source) = std::env::var("TINE_GH543_PROBE_GRAPH") else {
        panic!("set TINE_GH543_PROBE_GRAPH to a graph directory");
    };
    fn copy(from: &std::path::Path, to: &std::path::Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), &target).unwrap();
            }
        }
    }
    let root = r10_scratch("real-graph-probe");
    fs::remove_dir_all(&root).unwrap();
    copy(std::path::Path::new(&source), &root);
    let database = r10_scratch("real-graph-probe-db").join("projection.sqlite");
    let bound = Duration::from_secs(600);

    struct Phase<'a> {
        graph: &'a Graph,
        started: Instant,
        builds: usize,
        consumer: usize,
        fresh: u64,
    }
    impl<'a> Phase<'a> {
        fn begin(graph: &'a Graph) -> Self {
            Self {
                graph,
                started: Instant::now(),
                builds: graph.page_build_parses_test(),
                consumer: graph.consumer_page_parses_test(),
                fresh: graph
                    .direct_projection_test()
                    .map_or(0, |projection| projection.fresh_builds_test()),
            }
        }
        fn end(self, label: &str) -> (usize, u64) {
            let consumer = self.graph.consumer_page_parses_test() - self.consumer;
            let fresh = self
                .graph
                .direct_projection_test()
                .map_or(0, |projection| projection.fresh_builds_test())
                - self.fresh;
            eprintln!(
                "GH543PROBE {label}: ms={} whole_graph_parses={} consumer_page_parses={consumer} fresh_builds={fresh}",
                self.started.elapsed().as_millis(),
                self.graph.page_build_parses_test() - self.builds,
            );
            (consumer, fresh)
        }
    }
    let launch = |label: &str| {
        let graph = Arc::new(Graph::open(&root));
        let phase_started = Instant::now();
        graph.attach_direct_projection(database.clone()).unwrap();
        let owner = R10Owner::start(&graph);
        assert!(
            owner.wait_settled(bound) && owner.wait_ready(bound),
            "{label} never ready"
        );
        r10_settle(&graph);
        eprintln!(
            "GH543PROBE {label}: ready_ms={} pages={} whole_graph_parses={} consumer_page_parses={} fresh_builds={}",
            phase_started.elapsed().as_millis(),
            graph.list_pages().len(),
            graph.page_build_parses_test(),
            graph.consumer_page_parses_test(),
            graph.direct_projection_test().unwrap().fresh_builds_test(),
        );
        (graph, owner)
    };
    let reads = |graph: &Graph, label: &str| {
        let phase = Phase::begin(graph);
        let _ = graph.list_pages();
        let _ = graph.page_aliases();
        let _ = graph.search("the", 50);
        let _ = graph.journal_content_days();
        let (consumer, _) = phase.end(label);
        assert_eq!(consumer, 0, "{label}: a ready index read parsed pages");
    };

    let (graph, owner) = launch("cold");
    reads(&graph, "cold reads");
    let pages = graph
        .list_pages()
        .into_iter()
        .filter(|entry| entry.kind == PageKind::Page && !entry.name.contains('/'))
        .collect::<Vec<_>>();
    let target = pages[pages.len() / 2].name.clone();

    let phase = Phase::begin(&graph);
    graph
        .rename_page(&target, &format!("{target} probe"))
        .unwrap();
    r10_settle(&graph);
    reads(&graph, "reads after rename");
    let (_, fresh) = phase.end("rename");
    assert_eq!(fresh, 0, "a rename rebuilt the index");

    let saved = pages[pages.len() / 3].name.clone();
    let phase = Phase::begin(&graph);
    let mut page = graph.load_named(&saved, PageKind::Page).unwrap().unwrap();
    page.blocks = markdown_page_dto(&saved, &saved, "- probe edit\n")
        .unwrap()
        .blocks;
    let base = page.rev.clone();
    graph.save_page(&page, base.as_deref()).unwrap();
    r10_settle(&graph);
    reads(&graph, "reads after save");
    let (_, fresh) = phase.end("save");
    assert_eq!(fresh, 0, "a save rebuilt the index");

    let external = pages[pages.len() / 4].path.clone();
    let phase = Phase::begin(&graph);
    let mut text = fs::read_to_string(&external).unwrap_or_default();
    text.push_str("\n- probe external edit\n");
    fs::write(&external, text).unwrap();
    graph.sync_file_checked(&external).unwrap();
    r10_settle(&graph);
    reads(&graph, "reads after external edit");
    let (_, fresh) = phase.end("external edit");
    assert_eq!(fresh, 0, "an external edit rebuilt the index");
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    drop(graph);

    // Reopen: the stored index answers after a survey; nothing rebuilds.
    let (graph, owner) = launch("reopen");
    assert_eq!(
        graph.direct_projection_test().unwrap().fresh_builds_test(),
        0
    );
    reads(&graph, "reopen reads");
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    drop(graph);

    // One page becomes unreadable while Tine is closed.
    let broken = pages[pages.len() / 5].path.clone();
    fs::write(&broken, b"- \xff\xfe broken\n").unwrap();
    let (graph, owner) = launch("reopen with an unreadable page");
    assert_eq!(
        graph.direct_projection_test().unwrap().fresh_builds_test(),
        0
    );
    reads(&graph, "reads with an unreadable page");
    let failures = graph.page_index_failures();
    eprintln!("GH543PROBE unreadable recorded={}", failures.len());
    assert_eq!(failures.len(), 1);
    r10_finish(root, graph, owner);
    let _ = fs::remove_dir_all(database.parent().unwrap());
}
