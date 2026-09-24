//! GH #594, index liveness (L1-L3): within a bound the index is ready or
//! visibly failed, and no reader waits on it without end.

use super::gh543_r10::{r10_finish, r10_pages, r10_scratch, r10_settle, R10Owner};
use super::*;
use crate::direct_projection::ProjectionProgress;
use crate::query::{IndexFailureClass, QueryExecutionError, QueryUnavailableReason};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SHARING_VIOLATION: &str =
    "The process cannot access the file because it is being used by another process. (os error 32)";

/// Run `read` on its own thread; `None` when it has not returned in `limit`,
/// so a read that never returns fails the test instead of hanging it.
fn within<T: Send + 'static>(
    limit: Duration,
    read: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(read());
    });
    receiver.recv_timeout(limit).ok()
}

/// The field failure: the first launch after a schema change owed a fresh
/// build, which failed every time. The index said "recovering" for the whole
/// session and retried forever, and every reference panel and query failed
/// silently. Now it gives up after its attempts, says why, and a reopen (the
/// user's Retry, or the next launch) builds it.
#[test]
fn gh594_a_build_that_always_fails_ends_failed_and_panels_say_so() {
    let root = r10_scratch("gh594-always-fails");
    r10_pages(&root, 12);
    let graph = Arc::new(Graph::open(&root));
    graph
        .attach_direct_projection(root.join("private/projection.sqlite"))
        .unwrap();
    let projection = graph.direct_projection_test().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let arming = {
        let projection = Arc::clone(&projection);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                projection.fail_next_fresh_publication_test(SHARING_VIOLATION);
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let owner = R10Owner::start(&graph);
    let started = Instant::now();
    let mut progress = projection.progress_at(graph.cache_generation());
    while started.elapsed() < Duration::from_secs(20)
        && !matches!(progress, ProjectionProgress::Failed(_))
    {
        std::thread::sleep(Duration::from_millis(20));
        progress = projection.progress_at(graph.cache_generation());
    }
    let builds = projection.fresh_builds_test();
    let backlinks = {
        let graph = Arc::clone(&graph);
        within(Duration::from_secs(10), move || {
            crate::query::backlinks_bounded_indexed(&*graph, "p1", usize::MAX, usize::MAX)
                .map(|groups| groups.groups.len())
        })
    };
    stop.store(true, Ordering::Release);
    arming.join().unwrap();
    let recorded = crate::direct_projection::index_failures_reported_for_test()
        .into_iter()
        .any(|event| event.class == IndexFailureClass::FileInUse && event.terminal);
    owner.stop();
    crate::direct_projection::release_projection(&*graph);
    drop(graph);

    assert_eq!(
        progress,
        ProjectionProgress::Failed(IndexFailureClass::FileInUse),
        "a build failing every time must leave the index Failed with its class within 20 s"
    );
    assert!(
        builds <= u64::from(crate::direct_projection::INDEX_ATTEMPTS),
        "{builds} fresh builds for a failure no retry gets past"
    );
    assert!(
        recorded,
        "the terminal failure reached the failure observer"
    );
    match backlinks {
        Some(Err(QueryExecutionError::Unavailable(QueryUnavailableReason::IndexFailed(class)))) => {
            assert_eq!(class, IndexFailureClass::FileInUse)
        }
        other => panic!("the Linked References read must report the failed index: {other:?}"),
    }

    // Retry reopens the graph, as the next launch does; with the fault gone
    // the index builds and the panel answers.
    let graph = Arc::new(Graph::open(&root));
    graph
        .attach_direct_projection(root.join("private/projection.sqlite"))
        .unwrap();
    let owner = R10Owner::start(&graph);
    r10_settle(&graph);
    let answered = crate::query::backlinks_bounded_indexed(&*graph, "p1", usize::MAX, usize::MAX)
        .map(|groups| groups.groups.len());
    r10_finish(root, graph, owner);
    assert_eq!(
        answered.ok(),
        Some(1),
        "after the reopen, p0's link to p1 is listed"
    );
}

/// No derived read waits past its patience: with index work announced that
/// never arrives (an owner registered and never run), a page list answered
/// from the parsed pages after the patience instead of waiting for good.
#[test]
fn gh594_a_derived_read_waits_at_most_its_patience() {
    let root = r10_scratch("gh594-patience");
    r10_pages(&root, 12);
    let graph = Arc::new(Graph::open(&root));
    graph
        .attach_direct_projection(root.join("private/projection.sqlite"))
        .unwrap();
    let registration = graph.register_index_owner();
    graph.set_derived_read_patience_test(Duration::from_millis(500));
    let listed = {
        let graph = Arc::clone(&graph);
        within(Duration::from_secs(10), move || graph.list_pages().len())
    };
    drop(registration);
    crate::direct_projection::release_projection(&*graph);
    drop(graph);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        listed,
        Some(12),
        "the page list must answer from the parsed pages once its patience has passed"
    );
}

/// GH #406's lock half: a rename or delete while index work is announced
/// and never arrives finishes within the read patience. Both read the page
/// list, and they used to wait for the index while holding the graph-text
/// identity lock, so every other edit queued behind the wait.
#[test]
fn gh594_a_rename_or_delete_while_indexing_waits_at_most_its_patience() {
    let root = r10_scratch("gh594-rename-patience");
    r10_pages(&root, 12);
    let graph = Arc::new(Graph::open(&root));
    graph
        .attach_direct_projection(root.join("private/projection.sqlite"))
        .unwrap();
    let registration = graph.register_index_owner();
    graph.set_derived_read_patience_test(Duration::from_millis(500));
    let renamed = {
        let graph = Arc::clone(&graph);
        within(Duration::from_secs(10), move || {
            graph
                .rename_page("p3", "p3 renamed")
                .map_err(|e| e.to_string())
        })
    };
    let deleted = {
        let graph = Arc::clone(&graph);
        within(Duration::from_secs(10), move || {
            graph
                .delete_page("p4", crate::vocab::PageKind::Page)
                .map_err(|e| e.to_string())
        })
    };
    let listed = graph.list_pages();
    drop(registration);
    crate::direct_projection::release_projection(&*graph);
    drop(graph);
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        matches!(renamed, Some(Ok(_))),
        "a rename must not wait for index work that never arrives: {renamed:?}"
    );
    assert!(
        matches!(deleted, Some(Ok(_))),
        "a delete must not wait for index work that never arrives: {deleted:?}"
    );
    let names: Vec<_> = listed.iter().map(|page| page.name.to_lowercase()).collect();
    assert!(names.contains(&"p3 renamed".to_owned()), "{names:?}");
    assert!(!names.contains(&"p4".to_owned()), "{names:?}");
}

/// A reference panel asked while the index builds is told so at once. Its
/// alias and page-name lookups used to wait for the build first, and the
/// field's first backlinks read never completed.
#[test]
fn gh594_a_reference_panel_asked_while_indexing_is_told_at_once() {
    let root = r10_scratch("gh594-panel-told");
    r10_pages(&root, 12);
    let graph = Arc::new(Graph::open(&root));
    graph
        .attach_direct_projection(root.join("private/projection.sqlite"))
        .unwrap();
    let registration = graph.register_index_owner();
    let started = Instant::now();
    let backlinks = {
        let graph = Arc::clone(&graph);
        within(Duration::from_secs(5), move || {
            crate::query::backlinks_bounded_indexed(&*graph, "p1", usize::MAX, usize::MAX)
                .map(|groups| groups.groups.len())
        })
    };
    let unlinked = {
        let graph = Arc::clone(&graph);
        within(Duration::from_secs(5), move || {
            crate::query::unlinked_refs_bounded_indexed_with_source(
                &*graph,
                "p1",
                usize::MAX,
                usize::MAX,
            )
            .map(|groups| groups.groups.groups.len())
        })
    };
    let elapsed = started.elapsed();
    drop(registration);
    crate::direct_projection::release_projection(&*graph);
    drop(graph);
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        matches!(backlinks, Some(Err(QueryExecutionError::NotReady(_)))),
        "Linked References while indexing: {backlinks:?}"
    );
    assert!(
        matches!(unlinked, Some(Err(QueryExecutionError::NotReady(_)))),
        "Unlinked References while indexing: {unlinked:?}"
    );
    assert!(elapsed < Duration::from_secs(3), "told after {elapsed:?}");
}

/// `docs/contracts/index-readiness.md` states these values; they are the code's.
#[test]
fn index_readiness_contract_matches_the_code() {
    let contract = include_str!("../../../docs/contracts/index-readiness.md");
    assert!(contract.contains(&format!(
        "`INDEX_ATTEMPTS` = {}",
        crate::direct_projection::INDEX_ATTEMPTS
    )));
    assert!(contract.contains(&format!(
        "`DERIVED_READ_PATIENCE` = {} s",
        crate::model::derived_reads::DERIVED_READ_PATIENCE.as_secs()
    )));
    for class in IndexFailureClass::ALL {
        assert!(
            contract.contains(&format!("| `{}` |", class.as_str())),
            "the contract's class table lacks {}",
            class.as_str()
        );
    }
    assert_eq!(
        contract.matches("\n| `").count(),
        IndexFailureClass::ALL.len(),
        "the class table lists exactly the classes"
    );
}
