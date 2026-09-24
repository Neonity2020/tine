//! GH #543, indexing audit round 10: each finding's class, pinned. The
//! fixtures follow the auditor's probes (evidence `indexing-audit-r10`).

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static R10_PERMIT: Mutex<()> = Mutex::new(());

/// An index owner on its own thread, as the app runs one.
pub(super) struct R10Owner {
    graph: Arc<Graph>,
    stop: Arc<AtomicBool>,
    settled: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl R10Owner {
    pub(super) fn start(graph: &Arc<Graph>) -> Self {
        let registration = graph.register_index_owner();
        let stop = Arc::new(AtomicBool::new(false));
        let settled = Arc::new(AtomicU64::new(0));
        let handle = {
            let (graph, stop, settled) =
                (Arc::clone(graph), Arc::clone(&stop), Arc::clone(&settled));
            std::thread::spawn(move || {
                graph.run_index_owner(
                    registration,
                    &R10_PERMIT,
                    || stop.load(Ordering::Acquire),
                    || {
                        settled.fetch_add(1, Ordering::AcqRel);
                    },
                );
            })
        };
        Self {
            graph: Arc::clone(graph),
            stop,
            settled,
            handle: Some(handle),
        }
    }

    #[must_use]
    pub(super) fn wait_settled(&self, bound: Duration) -> bool {
        let started = Instant::now();
        while self.settled.load(Ordering::Acquire) == 0 {
            if started.elapsed() > bound {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    #[must_use]
    pub(super) fn wait_ready(&self, bound: Duration) -> bool {
        let started = Instant::now();
        while !self.graph.direct_projection_ready_test() {
            if started.elapsed() > bound {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    pub(super) fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().unwrap();
    }
}

pub(super) fn r10_scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tine-gh543-r10-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    dir
}

pub(super) fn r10_pages(root: &Path, count: usize) {
    for index in 0..count {
        fs::write(
            root.join("pages").join(format!("p{index}.md")),
            format!("- TODO t{index} [[p{}]]\n", (index + 1) % count),
        )
        .unwrap();
    }
}

/// Wait until the worker has drained and the index is ready again.
pub(super) fn r10_settle(graph: &Graph) {
    let projection = graph.direct_projection_test().unwrap();
    assert!(projection.wait_drained_test(), "the worker turn failed");
    let started = Instant::now();
    while !graph.direct_projection_ready_test() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A graph with a ready index and its owner running.
pub(super) fn r10_ready_graph(tag: &str) -> (PathBuf, Arc<Graph>, R10Owner) {
    let root = r10_scratch(tag);
    r10_pages(&root, 12);
    let graph = Arc::new(Graph::open(&root));
    graph
        .attach_direct_projection(root.join("private/projection.sqlite"))
        .unwrap();
    let owner = R10Owner::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(10)));
    assert!(owner.wait_ready(Duration::from_secs(10)));
    r10_settle(&graph);
    (root, graph, owner)
}

pub(super) fn r10_finish(root: PathBuf, graph: Arc<Graph>, owner: R10Owner) {
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(&root);
}

/// R10-01: a flat `(or …)` / `(and …)` far past SQLite's 1000-level expression
/// depth is an admitted query. It answers, and it rebuilds nothing.
#[test]
fn gh543_a_query_wider_than_sqlites_expression_depth_answers() {
    let (root, graph, owner) = r10_ready_graph("wide-query");
    let projection = graph.direct_projection_test().unwrap();
    let refs = |n: usize| {
        (0..n)
            .map(|i| format!("[[p{}]]", i % 12))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let props = |n: usize| {
        (0..n)
            .map(|i| format!("(property k{i} v)"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let before = projection.fresh_builds_test();
    for (name, source, groups) in [
        ("or-3000-refs", format!("(or {})", refs(3000)), 12),
        ("and-3000-refs", format!("(and {})", refs(3000)), 0),
        ("or-1500-props", format!("(or {})", props(1500)), 0),
        ("and-1500-props", format!("(and {})", props(1500)), 0),
        ("not-or-3000", format!("(not (or {}))", refs(3000)), 0),
    ] {
        let answer = graph
            .run_query_bounded(&source, 20_000, 32 * 1024 * 1024)
            .map(|answer| answer.groups.len());
        assert_eq!(answer.ok(), Some(groups), "{name} did not answer");
    }
    assert_eq!(
        projection.fresh_builds_test(),
        before,
        "a wide query rebuilt the index"
    );
    r10_finish(root, graph, owner);
}

/// R10-01's class: a read refused on an intact image is that query's answer,
/// not evidence of damage. It rebuilds nothing and the index stays ready. (A read over a damaged image still rebuilds:
/// `a_failed_statement_read_repairs_and_retries_the_same_statement`.)
#[test]
fn gh543_a_read_refused_on_an_intact_image_does_not_rebuild_it() {
    let (root, graph, owner) = r10_ready_graph("refused-read");
    let projection = graph.direct_projection_test().unwrap();
    let before = projection.fresh_builds_test();
    for _ in 0..3 {
        projection.inject_next_statement_refusal();
        // The dispatcher re-runs the statement once after the repair, so a
        // one-off refusal still answers; a deterministic one answers
        // `Unavailable(ReadFailed)`. Neither is ever "not ready".
        let answer = graph.run_query_bounded("(task TODO)", 20_000, 32 * 1024 * 1024);
        assert!(
            !matches!(answer, Err(crate::query::QueryExecutionError::NotReady(_))),
            "a refused read on an intact image made the index not ready: {answer:?}"
        );
        assert!(
            graph.direct_projection_ready_test(),
            "a refused read withdrew readiness"
        );
    }
    assert_eq!(
        projection.fresh_builds_test(),
        before,
        "a refused read rebuilt the index"
    );
    let answer = graph.run_query_bounded("(task TODO)", 20_000, 32 * 1024 * 1024);
    assert_eq!(answer.map(|answer| answer.groups.len()).ok(), Some(12));
    r10_finish(root, graph, owner);
}

/// Build and release a stored index for `root`, as a previous session would.
pub(super) fn r10_prebuild(root: &Path, database: &Path) {
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

/// Hold the worker's next turn just before it applies: `.0` is reached,
/// `.1` releases it.
fn r10_hold_next_turn(root: &Path) -> Arc<(std::sync::Barrier, std::sync::Barrier)> {
    let pair = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
    let held = Arc::clone(&pair);
    crate::direct_projection::before_next_apply_test(
        root,
        Box::new(move || {
            held.0.wait();
            held.1.wait();
        }),
    );
    pair
}

/// R10-02: a parse configuration changed while Tine was closed differs on
/// every page, so it is built fresh whichever way the full snapshot reaches
/// the worker: after a clean launch walk (the control), or after a walk an
/// unnamed change abandoned. The second once took the full snapshot as a
/// repair and re-lowered every page in one turn nothing could stop.
#[test]
fn gh543_a_config_changed_while_closed_is_built_fresh_on_every_path() {
    let run = |abandon: bool| -> u64 {
        let root = r10_scratch(if abandon {
            "cfg-abandon"
        } else {
            "cfg-control"
        });
        r10_pages(&root, 12);
        let database = root.join("private/projection.sqlite");
        r10_prebuild(&root, &database);
        fs::create_dir_all(root.join("logseq")).unwrap();
        fs::write(
            root.join("logseq/config.edn"),
            "{:property/separated-by-commas #{:foo}}\n",
        )
        .unwrap();
        let graph = Arc::new(Graph::open(&root));
        graph.attach_direct_projection(database).unwrap();
        let pause = graph.pause_next_warm_after_read_test();
        let owner = R10Owner::start(&graph);
        pause.reached.wait();
        if abandon {
            graph.drift_generation_test();
        }
        pause.release.wait();
        assert!(owner.wait_settled(Duration::from_secs(10)));
        assert!(owner.wait_ready(Duration::from_secs(10)));
        r10_settle(&graph);
        let fresh = graph.direct_projection_test().unwrap().fresh_builds_test();
        r10_finish(root, graph, owner);
        fresh
    };
    assert_eq!(
        run(false),
        1,
        "control: a clean walk sends the change to a fresh build"
    );
    assert_eq!(
        run(true),
        1,
        "an abandoned walk repaired every page in one turn"
    );
}

/// R10-02's class: one repair-or-rebuild rule for both repair paths. A few
/// pages edited while closed are still repaired in place after an abandoned
/// walk (R9-01), lowering only those pages.
#[test]
fn gh543_a_few_pages_changed_while_closed_are_repaired_on_every_path() {
    let root = r10_scratch("few-changed-abandon");
    r10_pages(&root, 12);
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);
    fs::write(root.join("pages/p5.md"), "- edited while closed [[p6]]\n").unwrap();
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    crate::direct_projection::count_page_lowerings_test(&root);
    let pause = graph.pause_next_warm_after_read_test();
    let owner = R10Owner::start(&graph);
    pause.reached.wait();
    graph.drift_generation_test();
    pause.release.wait();
    assert!(owner.wait_settled(Duration::from_secs(10)));
    assert!(owner.wait_ready(Duration::from_secs(10)));
    r10_settle(&graph);
    assert_eq!(
        graph.direct_projection_test().unwrap().fresh_builds_test(),
        0
    );
    assert_eq!(crate::direct_projection::page_lowerings_test(), 1);
    r10_finish(root, graph, owner);
}

/// R10-02's shape: the share bound is read in one place.
#[test]
fn the_repair_share_bound_has_one_reader() {
    let uses = crate::projection_producer_census::production_rust()
        .iter()
        .map(|file| file.code.matches("REPAIR_MAX_SHARE_DIVISOR").count())
        .sum::<usize>();
    assert_eq!(
        uses, 2,
        "REPAIR_MAX_SHARE_DIVISOR is declared once and read once, by \
         repair::repair_is_proportionate: the warm and full repair paths must \
         not decide 'repair or rebuild' differently (GH #543, audit R10-02)"
    );
}

/// R10-04: the bar shows while readers wait for graph-sized work. The audit's
/// shape was a full snapshot applied as a repair (a turn with no build
/// progress and no owner pass running); the reconciler takes a full snapshot
/// only as a fresh build, so this holds the fresh build the launch survey
/// owes when most pages changed while Tine was closed.
#[test]
fn gh543_the_bar_shows_while_readers_wait_for_a_full_repair() {
    let root = r10_scratch("bar-full-repair");
    r10_pages(&root, 12);
    let database = root.join("private/projection.sqlite");
    r10_prebuild(&root, &database);
    for index in 0..12 {
        fs::write(
            root.join("pages").join(format!("p{index}.md")),
            format!("- edited while closed {index}\n"),
        )
        .unwrap();
    }
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let hold = r10_hold_next_turn(&root);
    let owner = R10Owner::start(&graph);
    hold.0.wait();
    let projection = graph.direct_projection_test().unwrap();
    assert!(
        projection.fresh_build_running_test(),
        "precondition: the held turn is the fresh build"
    );
    let progress = graph.indexing_progress();
    let waiting = projection.coming() && !graph.direct_projection_ready_test();
    hold.1.wait();
    assert!(owner.wait_settled(Duration::from_secs(10)));
    assert!(owner.wait_ready(Duration::from_secs(10)));
    assert!(
        graph.indexing_progress().is_none(),
        "the bar outlived the work"
    );
    r10_finish(root, graph, owner);
    assert!(waiting, "precondition: readers wait for this turn");
    assert!(
        progress.is_some(),
        "the bar claimed idle while readers waited"
    );
}

/// R10-03: while the owner backs off after failed turns, a page consumer's
/// whole-graph parse starts no index build; only the owner offers a full
/// snapshot. Since GH #594 (L1) the backoff can end in `Failed`; a consumer
/// parse must start no build there either.
#[test]
fn gh543_a_consumer_parse_starts_no_index_build_during_the_backoff() {
    let (root, graph, owner) = r10_ready_graph("consumer-backoff");
    let projection = graph.direct_projection_test().unwrap();
    // The first failed turn is retried at once and the second waits out a
    // backoff, so exactly two failures put the owner in one. (A thread that
    // re-armed one failure every millisecond lost the race to the immediate
    // retry under load, and the owner went Ready without backing off.)
    projection.inject_turn_failures_test(2);
    fs::write(root.join("pages/p1.md"), "- edit that fails to index\n").unwrap();
    let _ = graph.sync_file_checked(&root.join("pages/p1.md"));
    let started = Instant::now();
    while !projection.backing_off() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        projection.backing_off(),
        "precondition: the owner is backing off"
    );
    let before = projection.fresh_builds_test();
    // A rename drops the parsed cache; the next display read parses the
    // whole graph and installs it.
    graph.rename_page("p3", "p3x").unwrap();
    graph.with_pages(|_| ());
    let _ = projection.wait_drained_test();
    assert_eq!(
        projection.fresh_builds_test(),
        before,
        "a consumer's parse started a whole index build inside the backoff"
    );
    r10_finish(root, graph, owner);
}

/// R10-03's shape: the owner's `offer_installed_cache` is the one caller
/// that offers the index a full snapshot.
#[test]
fn only_the_index_owner_offers_a_full_snapshot() {
    let sites: Vec<_> = crate::projection_producer_census::production_rust()
        .iter()
        .flat_map(|file| {
            file.code
                .match_indices("direct_projection_enqueue_full(")
                .map(move |(at, _)| {
                    format!("{}:{}", file.relative, file.code[..at].lines().count())
                })
        })
        .collect();
    assert_eq!(
        sites.len(),
        2,
        "direct_projection_enqueue_full is declared once and called once, by \
         Graph::offer_installed_cache (the index owner, model/projection_lifetime.rs): \
         a consumer that offers a full snapshot is a second producer of \
         whole-index work (GH #543, I-12, audit R10-03): {sites:?}"
    );
}
