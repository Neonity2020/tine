//! The public Direct query route, from an integration test (RET2).
//!
//! **Why this exists.** RET2 retired the parsed-graph walk from every public
//! Direct query. `Graph::run_query` and friends now answer from the SQLite
//! projection or return a typed `query::QueryExecutionError`, so an integration
//! fixture that opens a graph and asks a question has to do what the app does:
//! attach the projection, let it initialize, and retry only while the error is
//! `NotReady`. A fixture that instead reached for the walk to stay green would
//! be reconnecting the oracle at the test boundary, which is the one thing the
//! no-production-traversal amendment forbids.
//!
//! **Why the retry is the readiness gate.** `direct_projection_ready_test` is
//! `pub(crate)` and compiled only under the crate's own `cfg(test)`, so an
//! integration test (a separate crate, built against the ordinary library)
//! cannot see it. The typed error IS the public readiness signal — it is the
//! same one `src/queryReadiness.ts` loops on — so the fixture loops on exactly
//! that and fails immediately on anything else. No new public API is invented
//! for the tests' benefit.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tine_core::model::{Graph, RefGroup};
use tine_core::query::QueryExecutionError;

/// How long a fixture waits for the projection worker to publish readiness.
/// The same ceiling the in-crate `wait_ready` helper uses.
const READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Attach this graph's disposable projection under `<root>/private/` and warm
/// it. The database is graph-private state, exactly where the app puts it.
pub fn attach_projection(graph: &Graph, root: &Path) {
    // The parsed cache FIRST, then the projection. `warm_cache` prefers the
    // projection when one is attached and then retains no parsed graph at all
    // (R6), so attaching before the first warm would silently turn every
    // fixture here into a warm-reopen session and change what it observes.
    graph.warm_cache();
    graph
        .attach_direct_projection(root.join("private/projection.sqlite"))
        .expect("the disposable projection attaches");
    graph.warm_cache();
}

/// The same, for a fixture whose graph directory is a checked-in sample that a
/// test must not write into. The projection is disposable derived state (D-3),
/// so where its file lives is the caller's choice; nothing about the graph
/// changes.
pub fn attach_scratch_projection(graph: &Graph, tag: &str) {
    let database = std::env::temp_dir()
        .join(format!("tine-ready-query-{}-{tag}", std::process::id()))
        .join("projection.sqlite");
    let _ = std::fs::remove_dir_all(database.parent().expect("the database has a directory"));
    graph.warm_cache();
    graph
        .attach_direct_projection(database)
        .expect("the disposable projection attaches");
    graph.warm_cache();
}

/// Run `attempt` until it answers, retrying ONLY typed readiness.
///
/// `Unavailable` and `Cancelled` fail the fixture immediately: they are the
/// bounded errors this packet introduced, and a fixture that slept through them
/// would hide exactly the regression the typed vocabulary exists to expose.
pub fn when_ready<T>(mut attempt: impl FnMut() -> Result<T, QueryExecutionError>) -> T {
    let started = Instant::now();
    loop {
        match attempt() {
            Ok(answer) => return answer,
            Err(QueryExecutionError::NotReady(reason)) => {
                assert!(
                    started.elapsed() < READY_TIMEOUT,
                    "the query index never became ready ({})",
                    reason.as_str()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(other) => panic!("the public query route refused: {other}"),
        }
    }
}

/// `Graph::search` after a mutation, waiting for the answer to reflect it.
///
/// `when_ready` alone is not enough here. It retries the typed `NotReady`
/// signal, but a read taken just after a save can also come back `Ok` at the
/// generation BEFORE the save, so a fixture that asserts the new content
/// straight away is racing the projection under load rather than observing it.
/// This waits for the expected hit count, and fails with the count it actually
/// saw so a real regression still reads as one.
///
/// Use it only for a transition a mutation is supposed to cause. An assertion
/// that a token must NEVER match is not a transition: waiting for it would just
/// wait for the first sample and prove nothing, so make those reads plain, and
/// order them AFTER a transition this helper has already observed.
pub fn when_search_hits(graph: &Graph, needle: &str, expected: usize) -> usize {
    let started = Instant::now();
    loop {
        let hits = when_ready(|| graph.search(needle, 10)).len();
        if hits == expected {
            return hits;
        }
        assert!(
            started.elapsed() < READY_TIMEOUT,
            "search for {needle:?} never reached {expected} hits (last saw {hits})"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `Graph::run_query` through the readiness gate.
pub fn run_query(graph: &Graph, source: &str) -> Arc<Vec<RefGroup>> {
    when_ready(|| graph.run_query(source))
}
