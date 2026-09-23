//! GH #543: the index owner's lifecycle against the rest of the session --
//! a predecessor holding the lease, a failed turn while it settles, a reused
//! snapshot on a reopened image, an acting read beside a missed external
//! edit, a mover with nothing to send (GH #543).

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static OWNER_PERMIT: Mutex<()> = Mutex::new(());

struct TestOwner {
    graph: Arc<Graph>,
    stop: Arc<AtomicBool>,
    settled: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<usize>>,
}

impl TestOwner {
    fn start(graph: &Arc<Graph>) -> Self {
        let registration = graph.register_index_owner();
        let stop = Arc::new(AtomicBool::new(false));
        let settled = Arc::new(AtomicU64::new(0));
        let handle = {
            let (graph, stop, settled) =
                (Arc::clone(graph), Arc::clone(&stop), Arc::clone(&settled));
            std::thread::spawn(move || {
                GRAPH_TEXT_CONTENT_READS.with(|reads| reads.set(0));
                graph.run_index_owner(
                    registration,
                    &OWNER_PERMIT,
                    || stop.load(Ordering::Acquire),
                    || {
                        settled.fetch_add(1, Ordering::AcqRel);
                    },
                );
                GRAPH_TEXT_CONTENT_READS.with(Cell::get)
            })
        };
        Self {
            graph: Arc::clone(graph),
            stop,
            settled,
            handle: Some(handle),
        }
    }

    fn wait_settled(&self, bound: Duration) -> bool {
        let started = Instant::now();
        while self.settled.load(Ordering::Acquire) == 0 {
            if started.elapsed() > bound {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    #[must_use = "a readiness wait that timed out must fail the test or be handled (GH #543, R9-15e)"]
    fn wait_ready(&self, bound: Duration) -> bool {
        let started = Instant::now();
        while !self.graph.direct_projection_ready_test() {
            if started.elapsed() > bound {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    fn stop(mut self) -> usize {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().unwrap()
    }
}

fn prebuild_index(root: &Path, database: &Path) {
    let graph = Graph::open(root);
    graph
        .attach_direct_projection(database.to_path_buf())
        .unwrap();
    graph.warm_cache();
    graph
        .wait_for_direct_projection_for_test(Duration::from_secs(10))
        .unwrap();
    crate::direct_projection::release_projection(&graph);
}

fn ring_pages(root: &Path, count: usize) {
    for index in 0..count {
        fs::write(
            root.join("pages").join(format!("p{index}.md")),
            format!("- TODO t{index} [[p{}]]\n", (index + 1) % count),
        )
        .unwrap();
    }
}

fn index_state(graph: &Graph) -> String {
    graph
        .direct_projection_test()
        .map(|projection| projection.debug_state_test())
        .unwrap_or_default()
}

/// Reopening a graph whose previous in-process writer has not yet let
/// the lease go (a quick switch back, a close then reopen) puts the new
/// worker in `LeaseWait`, which is "not coming": a page listing parses the
/// whole graph on the reader's thread, and its snapshot is offered as a full
/// build instead of the owner's cheap validation.
#[test]
fn gh543_a_reader_waits_for_a_retired_predecessor_to_release_the_index() {
    const PAGES: usize = 12;
    let root = scratch("gh543-lifecycle-lease-wait");
    ring_pages(&root, PAGES);
    let database = root.join("private/projection.sqlite");
    prebuild_index(&root, &database);
    // The previous binding of this graph: its writer still holds the lease.
    let old = Graph::open(&root);
    old.attach_direct_projection(database.clone()).unwrap();
    old.warm_cache();
    old.wait_for_direct_projection_for_test(Duration::from_secs(10))
        .unwrap();
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = TestOwner::start(&graph);
    let started = Instant::now();
    while !crate::direct_projection::lease_wait_started_test(&graph)
        && started.elapsed() < Duration::from_secs(5)
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(300));
    let lease_state = index_state(&graph);
    let query = graph.run_query("(task TODO)").map(|groups| groups.len());
    let settled_during_wait = owner.settled.load(Ordering::Acquire);
    // The old writer lets go, as a retired graph's detach does, while a
    // reader lists the pages.
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        crate::direct_projection::release_projection(&old);
    });
    let listed = graph.list_pages().len();
    let parses_during_wait = graph.page_build_parses_test();
    releaser.join().unwrap();
    let ready = owner.wait_ready(Duration::from_secs(10));
    let state = index_state(&graph);
    let passes = graph.owner_passes_test();
    let parses = graph.page_build_parses_test();
    let reads = owner.stop();
    eprintln!(
        "lease wait: lease_state=[{lease_state}] query_during_wait={query:?} listed={listed} parses_during_wait={parses_during_wait} settled_during_wait={settled_during_wait} ready={ready} owner_passes={passes} parses_total={parses} owner_reads={reads} state=[{state}]"
    );
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(
        parses_during_wait, 0,
        "a reader parsed the whole graph while the index writer waited for an in-process predecessor"
    );
    assert_eq!(
        settled_during_wait, 0,
        "the owner settled while its worker was still setting up"
    );
    assert!(
        matches!(query, Err(crate::query::QueryExecutionError::NotReady(_))),
        "a query during the wait did not report indexing: {query:?}"
    );
}

/// A rename whose only published page cannot be parsed claims
/// `IndexEffect::Sent` for its generation move but queues nothing: the index
/// is left behind the generation with nothing owed, the owner sees `Nothing`,
/// and queries/derived reads are left without a producer.
#[test]
fn gh543_a_rename_with_nothing_to_send_leaves_the_index_current() {
    const PAGES: usize = 12;
    let root = scratch("gh543-lifecycle-rename-unparseable");
    ring_pages(&root, PAGES);
    let database = root.join("private/projection.sqlite");
    prebuild_index(&root, &database);
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = TestOwner::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(10)));
    assert!(owner.wait_ready(Duration::from_secs(10)));
    fs::write(
        root.join("pages/lone.md"),
        format!("- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n"),
    )
    .unwrap();
    let renamed = graph.rename_page("lone", "solo");
    std::thread::sleep(Duration::from_millis(300));
    let ready_after = graph.direct_projection_ready_test();
    let state_after = index_state(&graph);
    let passes_before = graph.owner_passes_test();
    let parses_before = graph.page_build_parses_test();
    let query = graph.run_query("(task TODO)").map(|groups| groups.len());
    let listed = graph.list_pages().len();
    let parses_after_list = graph.page_build_parses_test() - parses_before;
    let started = Instant::now();
    let aliases = graph.page_aliases().len();
    let alias_ms = started.elapsed().as_millis();
    let parses_after_aliases = graph.page_build_parses_test() - parses_before;
    let full_queued = index_state(&graph);
    std::thread::sleep(Duration::from_millis(300));
    let passes = graph.owner_passes_test() - passes_before;
    let reads = owner.stop();
    eprintln!(
        "rename unparseable: renamed={renamed:?} ready_after={ready_after} state_after=[{state_after}] query={query:?} listed={listed} parses_after_list={parses_after_list} aliases={aliases} alias_ms={alias_ms} parses_after_aliases={parses_after_aliases} state_after_aliases=[{full_queued}] ready_end={} owner_passes_after={passes} owner_reads={reads}", graph.direct_projection_ready_test()
    );
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(&root);
    assert!(
        ready_after || renamed.is_err(),
        "the rename moved the generation with nothing queued: the index is behind with nothing owed"
    );
}

/// (Seed 1023 of a 300-seed interleaving run.) The owner used to prefetch
/// derived maps at Settle through an ordinary derived read. If the index then fails a turn (any cause) while
/// the prefetch waits, the need becomes `Fresh` -- work that is "coming" only
/// because the owner will run it -- and the prefetch waits for it forever:
/// the owner never gets back to its loop, and every derived read and every
/// operation that lists pages waits with it.
#[test]
fn gh543_a_turn_failure_while_the_owner_settles_does_not_hang_it() {
    const PAGES: usize = 12;
    let root = scratch("gh543-lifecycle-settle-self-wait");
    ring_pages(&root, PAGES);
    let database = root.join("private/projection.sqlite");
    prebuild_index(&root, &database);
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let pause = graph.pause_next_warm_before_settle_test();
    let owner = TestOwner::start(&graph);
    pause.reached.wait();
    let state_at_settle = index_state(&graph);
    // A turn fails while the owner is about to prefetch (a disk error, a busy
    // database, the seed-1023 `UNIQUE constraint failed: pages.position`).
    graph
        .direct_projection_test()
        .unwrap()
        .inject_next_turn_failure_test();
    let mut page = graph.load_named("p0", PageKind::Page).unwrap().unwrap();
    page.blocks = markdown_page_dto("p0", "p0", "- TODO changed\n")
        .unwrap()
        .blocks;
    let base = page.rev.clone();
    graph.save_page(&page, base.as_deref()).unwrap();
    let started = Instant::now();
    while !index_state(&graph).contains("turn_failed=true")
        && started.elapsed() < Duration::from_secs(5)
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    let state_failed = index_state(&graph);
    pause.release.wait();
    let ready = owner.wait_ready(Duration::from_secs(10));
    let settled = owner.settled.load(Ordering::Acquire);
    let passes = graph.owner_passes_test();
    let state_end = index_state(&graph);
    // A reader now: does it return?
    let reader = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.list_pages().len())
    };
    std::thread::sleep(Duration::from_secs(2));
    let reader_returned = reader.is_finished();
    eprintln!(
        "settle self-wait: state_at_settle=[{state_at_settle}] state_failed=[{state_failed}] ready_within_10s={ready} settled={settled} owner_passes={passes} reader_returned_within_2s={reader_returned} state_end=[{state_end}]"
    );
    // Free the stuck threads: a retired graph's derived reads return.
    graph.retire();
    let _ = reader.join();
    let _ = owner.stop();
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(&root);
    assert!(
        ready,
        "a turn failure while the owner settled left the owner waiting for its own Fresh pass"
    );
}

/// (Seed 1023's first half.) the seed-6 fix gives a reopened image the
/// queue's dense order only when a warm validation comes back clean. A full
/// snapshot queued on a reopened image (an acting read, a derived-read
/// fallback, a lease-wait read) also reseeds the queue densely
/// (`enqueue_full`), but the worker's `reuse_full` branch only applies deltas:
/// the image keeps the gap a deleted page left, and the next new page takes a
/// stored page's position.
#[test]
fn gh543_a_reused_snapshot_on_a_reopened_image_takes_the_queue_order() {
    crate::backend_error::set_runtime_debug_diagnostics(true);
    let root = scratch("gh543-lifecycle-reuse-full-gap");
    for index in 0..5 {
        fs::write(
            root.join("pages").join(format!("p{index}.md")),
            format!("- TODO t{index}\n"),
        )
        .unwrap();
    }
    let database = root.join("private/projection.sqlite");
    {
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        graph
            .wait_for_direct_projection_for_test(Duration::from_secs(10))
            .unwrap();
        graph.delete_page("p1", PageKind::Page).unwrap();
        assert!(graph.direct_projection_test().unwrap().wait_drained_test());
        crate::direct_projection::release_projection(&graph);
    }
    let graph = Graph::open(&root);
    graph.attach_direct_projection(database).unwrap();
    // An acting whole-graph read parses and offers its snapshot; with no warm
    // coming, it is queued as a full turn, which reuses the stored image.
    let pages = graph.try_with_pages(|pages| pages.len()).unwrap();
    let projection = graph.direct_projection_test().unwrap();
    let drained_full = projection.wait_drained_test();
    let state_after_full = projection.debug_state_test();
    graph
        .save_page(
            &markdown_page_dto("p7", "p7", "- TODO seven\n").unwrap(),
            None,
        )
        .unwrap();
    let drained = projection.wait_drained_test();
    let state = projection.debug_state_test();
    eprintln!(
        "reuse-full gap: pages={pages} drained_full={drained_full} state_after_full=[{state_after_full}] drained_after_create={drained} state=[{state}]"
    );
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(&root);
    assert!(
        !state.contains("turn_failed=true"),
        "a page created after a reused full snapshot on a reopened image failed the index turn"
    );
}

/// Seeds 1066/1141: an external edit the watcher missed, then an acting
/// whole-graph read (publish, orphan assets, the Guide copy) that parses the
/// graph and installs the parsed cache, then the watcher's rescan of the
/// changed path: the rescan finds the parsed cache already current and the
/// index keeps the page's old content for good. `acting_read=false` is the
/// control (the same rescan without the acting read converges).
fn missed_edit_then_rescan(acting_read: bool) -> (Vec<String>, Vec<String>, String) {
    let root = scratch(if acting_read {
        "gh543-lifecycle-missed-edit-acting-read"
    } else {
        "gh543-lifecycle-missed-edit-control"
    });
    ring_pages(&root, 3);
    let database = root.join("private/projection.sqlite");
    prebuild_index(&root, &database);
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = TestOwner::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(10)));
    assert!(owner.wait_ready(Duration::from_secs(10)));
    let path = root.join("pages/p0.md");
    // Keep the byte length different so no size/mtime shortcut hides it.
    fs::write(&path, "- plain externally edited, no task here\n").unwrap();
    if acting_read {
        graph.try_with_pages(|pages| pages.len()).unwrap();
    }
    graph.note_graph_text_external_observation();
    let ticket = graph.graph_text_external_observation_ticket();
    graph
        .observe_graph_text_external_paths(std::iter::empty::<&Path>(), true)
        .unwrap();
    let disk_rev = super::content_rev(&fs::read_to_string(&path).unwrap());
    let current_before =
        graph.page_revision_current(graph.cache.read().unwrap().is_none(), &path, &disk_rev);
    let synced = graph.sync_file_checked(&path).unwrap().is_some();
    graph.acknowledge_graph_text_external_observations(ticket);
    let projection = graph.direct_projection_test().unwrap();
    assert!(
        projection.wait_drained_test(),
        "the sync's index turn failed"
    );
    assert!(
        owner.wait_ready(Duration::from_secs(10)),
        "the index was not ready before the query ran (R9-15e)"
    );
    let answer = |groups: &[crate::model::RefGroup]| {
        let mut raws = groups
            .iter()
            .flat_map(|group| {
                group
                    .blocks
                    .iter()
                    .map(move |b| format!("{}|{}", group.page, b.raw))
            })
            .collect::<Vec<_>>();
        raws.sort();
        raws
    };
    let indexed = answer(
        &graph
            .run_query_bounded("(task TODO)", 1_000, 1 << 24)
            .unwrap()
            .groups,
    );
    let oracle = Graph::open(&root);
    let disk =
        answer(&crate::query::run_query_bounded(&oracle, "(task TODO)", 1_000, 1 << 24).groups);
    let state = format!(
        "{} synced={synced} revision_current_before_sync={current_before}",
        index_state(&graph)
    );
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(&root);
    (indexed, disk, state)
}

#[test]
fn gh543_an_acting_read_does_not_hide_a_missed_external_edit() {
    let (control_index, control_disk, _) = missed_edit_then_rescan(false);
    let (indexed, disk, state) = missed_edit_then_rescan(true);
    eprintln!(
        "acting-read swallow: control index={control_index:?} disk={control_disk:?}; with acting read index={indexed:?} disk={disk:?} state=[{state}]"
    );
    assert_eq!(control_index, control_disk, "control did not converge");
    assert_eq!(
        indexed, disk,
        "the rescan after an acting read left the index stale"
    );
}

fn settle_index(graph: &Graph) {
    let projection = graph.direct_projection_test().unwrap();
    assert!(projection.wait_drained_test(), "the index turn failed");
    let started = Instant::now();
    while !graph.direct_projection_ready_test() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Rewrite `path` with the bytes it already has, as a sync tool or an editor
/// does, and deliver it: the name of the page reported changed, and whether
/// the delivery moved the generation.
fn redeliver(graph: &Graph, path: &Path, bytes: &[u8]) -> (Option<String>, bool) {
    fs::write(path, bytes).unwrap();
    let before = graph.cache_generation();
    let delivered = graph
        .sync_file_checked(path)
        .unwrap()
        .map(|entry| entry.name);
    (delivered, graph.cache_generation() != before)
}

/// While the launch build is still lowering the parsed snapshot, the index
/// has been SENT every page's bytes though it is not ready. A delivery of
/// bytes the snapshot carries is no change; one of other bytes is, and a
/// delivery of bytes a queued edit has since replaced is one too (GH #543,
/// audit R9-02).
#[test]
fn gh543_bytes_the_launch_build_carries_are_current_before_it_is_ready() {
    let root = scratch("gh543-currency-during-build");
    ring_pages(&root, 12);
    let graph = Arc::new(Graph::open(&root));
    graph
        .attach_direct_projection(root.join("private/projection.sqlite"))
        .unwrap();
    let held = graph
        .direct_projection_test()
        .unwrap()
        .hold_fresh_publication_test();
    let owner = TestOwner::start(&graph);
    held.0.wait();
    assert!(!graph.direct_projection_ready_test());

    let same = root.join("pages/p3.md");
    let same_bytes = fs::read(&same).unwrap();
    let unchanged = redeliver(&graph, &same, &same_bytes);

    let edited = root.join("pages/p4.md");
    let original = fs::read(&edited).unwrap();
    fs::write(&edited, "- edited during the build [[p5]]\n").unwrap();
    let changed = graph
        .sync_file_checked(&edited)
        .unwrap()
        .map(|entry| entry.name);
    let reverted = redeliver(&graph, &edited, &original);

    held.1.wait();
    assert!(
        owner.wait_ready(Duration::from_secs(10)),
        "{}",
        index_state(&graph)
    );
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(
        unchanged,
        (None, false),
        "bytes the build carries were published again"
    );
    assert_eq!(
        changed.as_deref(),
        Some("p4"),
        "an edit during the build was not published"
    );
    assert_eq!(
        reverted,
        (Some("p4".to_owned()), true),
        "bytes a queued edit replaced were taken as current"
    );
}

/// A session served from the index alone (no parsed cache) whose index holds
/// a page's bytes publishes nothing for a delivery of those bytes, though
/// this session never opened the page (GH #543, audit R9-03).
#[test]
fn gh543_bytes_only_the_index_holds_are_current() {
    let root = scratch("gh543-currency-index-only");
    ring_pages(&root, 12);
    let database = root.join("private/projection.sqlite");
    prebuild_index(&root, &database);
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = TestOwner::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(10)));
    assert!(owner.wait_ready(Duration::from_secs(10)));
    settle_index(&graph);
    assert!(!graph.has_parsed_cache_test());
    let path = root.join("pages/p3.md");
    let bytes = fs::read(&path).unwrap();
    let unchanged = redeliver(&graph, &path, &bytes);
    fs::write(&path, "- edited outside Tine [[p4]]\n").unwrap();
    let changed = graph
        .sync_file_checked(&path)
        .unwrap()
        .map(|entry| entry.name);
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(
        unchanged,
        (None, false),
        "bytes the index holds were published again"
    );
    assert_eq!(
        changed.as_deref(),
        Some("p3"),
        "an external edit was not published"
    );
}

/// One page edited while Tine was closed, met by a launch validation that is
/// abandoned mid-walk: the escalated pass offers a complete snapshot that
/// differs from the stored image by that page, and the index lowers that
/// page, not the whole graph (GH #543, audit R9-01).
#[test]
fn gh543_a_snapshot_over_a_healthy_image_repairs_only_what_differs() {
    let root = scratch("gh543-full-repairs-one-page");
    ring_pages(&root, 12);
    let database = root.join("private/projection.sqlite");
    prebuild_index(&root, &database);
    fs::write(root.join("pages/p5.md"), "- edited while closed [[p6]]\n").unwrap();
    fs::remove_file(root.join("pages/p7.md")).unwrap();
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let pause = graph.pause_next_warm_after_read_test();
    let owner = TestOwner::start(&graph);
    pause.reached.wait();
    graph.drift_generation_test();
    pause.release.wait();
    assert!(owner.wait_settled(Duration::from_secs(10)));
    assert!(
        owner.wait_ready(Duration::from_secs(10)),
        "{}",
        index_state(&graph)
    );
    settle_index(&graph);
    let builds = graph.direct_projection_test().unwrap().fresh_builds_test();
    let edited = graph
        .run_query("\"edited while closed\"")
        .map(|groups| groups.len());
    let deleted = graph.run_query("\"t7\"").map(|groups| groups.len());
    let state = index_state(&graph);
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(
        builds, 0,
        "a one-page difference rebuilt the whole index: {state}"
    );
    assert_eq!(edited.ok(), Some(1), "the edited page is not searchable");
    assert_eq!(
        deleted.ok(),
        Some(0),
        "the deleted page is still searchable"
    );
}

/// A page that recovers from unreadable travels as its own update, with or
/// without an index owner, and never rebuilds the index from scratch
/// (GH #543, audit R9-04).
#[test]
fn gh543_a_recovered_page_is_one_update() {
    for with_owner in [true, false] {
        let root = scratch("gh543-recovered-page-update");
        ring_pages(&root, 12);
        let bad = root.join("pages/bad.md");
        fs::write(&bad, format!("- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n")).unwrap();
        let graph = Arc::new(Graph::open(&root));
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        let owner = with_owner.then(|| TestOwner::start(&graph));
        match &owner {
            Some(owner) => {
                assert!(owner.wait_settled(Duration::from_secs(10)));
                assert!(owner.wait_ready(Duration::from_secs(10)));
            }
            None => graph.warm_cache(),
        }
        settle_index(&graph);
        let projection = graph.direct_projection_test().unwrap();
        let builds_before = projection.fresh_builds_test();
        fs::write(&bad, "- recovered [[p1]]\n").unwrap();
        let delivered = graph
            .sync_file_checked(&bad)
            .unwrap()
            .map(|entry| entry.name);
        settle_index(&graph);
        let builds = projection.fresh_builds_test() - builds_before;
        let found = graph.run_query("\"recovered\"").map(|groups| groups.len());
        let state = index_state(&graph);
        if let Some(owner) = owner {
            owner.stop();
        }
        crate::direct_projection::release_projection(&graph);
        let _ = fs::remove_dir_all(&root);
        assert_eq!(delivered.as_deref(), Some("bad"), "with_owner={with_owner}");
        assert_eq!(
            builds, 0,
            "with_owner={with_owner}: a recovered page rebuilt the index: {state}"
        );
        assert_eq!(
            found.ok(),
            Some(1),
            "with_owner={with_owner}: not searchable: {state}"
        );
    }
}
