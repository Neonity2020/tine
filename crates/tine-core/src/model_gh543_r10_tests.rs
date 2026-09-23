//! GH #543, indexing audit round 10: each finding's class, pinned. The
//! fixtures follow the auditor's probes (evidence `indexing-audit-r10`).

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static R10_PERMIT: Mutex<()> = Mutex::new(());

/// An index owner on its own thread, as the app runs one.
struct R10Owner {
    graph: Arc<Graph>,
    stop: Arc<AtomicBool>,
    settled: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl R10Owner {
    fn start(graph: &Arc<Graph>) -> Self {
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

    #[must_use]
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

    fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().unwrap();
    }
}

fn r10_scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tine-gh543-r10-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    fs::create_dir_all(dir.join("journals")).unwrap();
    fs::create_dir_all(dir.join("pages")).unwrap();
    dir
}

fn r10_pages(root: &Path, count: usize) {
    for index in 0..count {
        fs::write(
            root.join("pages").join(format!("p{index}.md")),
            format!("- TODO t{index} [[p{}]]\n", (index + 1) % count),
        )
        .unwrap();
    }
}

/// Wait until the worker has drained and the index is ready again.
fn r10_settle(graph: &Graph) {
    let projection = graph.direct_projection_test().unwrap();
    assert!(projection.wait_drained_test(), "the worker turn failed");
    let started = Instant::now();
    while !graph.direct_projection_ready_test() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A graph with a ready index and its owner running.
fn r10_ready_graph(tag: &str) -> (PathBuf, Arc<Graph>, R10Owner) {
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

fn r10_finish(root: PathBuf, graph: Arc<Graph>, owner: R10Owner) {
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
