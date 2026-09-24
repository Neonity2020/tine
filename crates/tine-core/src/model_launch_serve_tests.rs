//! Launch design D2/D3 (`2026-09-24-launch-serve-stored-design`, approved by
//! Martin 2026-09-24, BW4): a reopen answers display reads from the image the
//! last session left while the launch check runs, and a read that acts on its
//! answer still waits for the check. Each test holds the check after its read
//! of the graph, where every display read used to wait (GH #550, #543).

use super::gh543_r10::{r10_finish, r10_pages, r10_prebuild, r10_scratch, r10_settle, R10Owner};
use super::*;
use std::sync::Arc;
use std::time::Duration;

struct Paused {
    root: PathBuf,
    graph: Arc<Graph>,
    owner: R10Owner,
    pause: Arc<PageBuildTestPause>,
}

impl Paused {
    /// Reopen a built 12-page graph after `while_closed`, and hold the launch
    /// check after it has read the graph.
    fn reopen(tag: &str, while_closed: impl FnOnce(&Path)) -> Self {
        let root = r10_scratch(tag);
        r10_pages(&root, 12);
        let database = root.join("private/projection.sqlite");
        r10_prebuild(&root, &database);
        while_closed(&root);
        let graph = Arc::new(Graph::open(&root));
        graph.attach_direct_projection(database).unwrap();
        let pause = graph.pause_next_warm_after_read_test();
        let owner = R10Owner::start(&graph);
        pause.reached.wait();
        Self {
            root,
            graph,
            owner,
            pause,
        }
    }

    /// Let the check finish and the index become ready.
    fn release(&self) {
        self.pause.release.wait();
        assert!(self.owner.wait_settled(Duration::from_secs(20)));
        assert!(self.owner.wait_ready(Duration::from_secs(20)));
        r10_settle(&self.graph);
    }

    fn finish(self) {
        r10_finish(self.root, self.graph, self.owner);
    }
}

fn ctrl_k(graph: &Graph, source: &str) -> (String, Option<&'static str>) {
    crate::model::search::LAST_FRIENDLY_SERVED_BY.with(|last| last.set(None));
    let answer = graph
        .run_graph_search_displayed_for(
            source,
            12,
            12,
            None,
            false,
            crate::query_plan::FriendlyDisplayOptions::default(),
            crate::query_plan::FriendlyConsumer::CtrlK,
        )
        .expect("Ctrl-K answers");
    (
        format!("{:?}", answer.hits),
        crate::model::search::LAST_FRIENDLY_SERVED_BY.with(|last| last.get()),
    )
}

fn linked_pages(graph: &Graph, target: &str) -> Vec<String> {
    let answer = graph
        .backlinks_bounded_indexed(target, 1000, 1 << 20)
        .expect("linked references answer");
    let mut pages = answer
        .groups
        .iter()
        .map(|group| group.page.clone())
        .collect::<Vec<_>>();
    pages.sort();
    pages
}

/// Test 1: a clean reopen answers Ctrl-K, a query block, Linked References
/// and the page list from the stored image while the check is held, and
/// parses no page. Each waited for the check before.
#[test]
fn a_clean_reopen_answers_display_reads_during_the_launch_check() {
    let launch = Paused::reopen("serve-clean", |_| {});
    let graph = &launch.graph;
    let answered = graph
        .display_read(|| {
            let (hits, served_by) = ctrl_k(graph, "t3");
            let tasks = graph
                .run_query_bounded("(task TODO)", 1000, 1 << 20)
                .map(|answer| answer.groups.len());
            (
                hits,
                served_by,
                tasks,
                linked_pages(graph, "p4"),
                graph.list_pages().len(),
            )
        })
        .expect("the graph is not retired");
    let parses = graph.page_build_parses_test();
    launch.release();
    launch.finish();
    let (hits, served_by, tasks, linked, pages) = answered;
    assert_eq!(
        served_by,
        Some("index"),
        "Ctrl-K was not answered by the index"
    );
    assert!(hits.contains("t3"), "Ctrl-K found nothing: {hits}");
    assert_eq!(tasks.ok(), Some(12), "the query block did not answer");
    assert_eq!(linked, vec!["p3".to_owned()], "Linked References");
    assert_eq!(pages, 12, "the page list");
    assert_eq!(parses, 0, "a stored answer parsed pages");
}

/// A read that acts on its answer still waits for the check: a query outside
/// a display read (an export) is not ready, and a listing that decides a
/// refusal waits until the check lands.
#[test]
fn a_read_that_acts_on_its_answer_waits_for_the_launch_check() {
    let launch = Paused::reopen("serve-acting", |_| {});
    let graph = Arc::clone(&launch.graph);
    let query = graph.run_query_bounded("(task TODO)", 1000, 1 << 20);
    assert!(
        matches!(query, Err(crate::query::QueryExecutionError::NotReady(_))),
        "a query outside a display read was answered before the check: {:?}",
        query.map(|answer| answer.groups.len())
    );
    let listing = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.try_list_pages().map(|pages| pages.len()))
    };
    std::thread::sleep(Duration::from_millis(300));
    let waited = !listing.is_finished();
    launch.release();
    let listed = listing.join().unwrap();
    launch.finish();
    assert!(waited, "an acting listing did not wait for the check");
    assert_eq!(listed.ok(), Some(12));
}

/// Test 2: a page edited while Tine was closed is served as stored during the
/// check, and the answer reflects the edit once the check lands. (The display
/// re-asks on `warm-cache-done`: `App.tsx` and its render test.)
#[test]
fn an_edit_made_while_closed_is_corrected_when_the_check_lands() {
    let launch = Paused::reopen("serve-closed-edit", |root| {
        fs::write(root.join("pages/p3.md"), "- TODO freshly [[p9]]\n").unwrap();
    });
    let graph = &launch.graph;
    let during = graph
        .display_read(|| (ctrl_k(graph, "freshly").0, linked_pages(graph, "p9")))
        .unwrap();
    launch.release();
    let after = graph
        .display_read(|| (ctrl_k(graph, "freshly").0, linked_pages(graph, "p9")))
        .unwrap();
    launch.finish();
    assert!(
        !during.0.contains("freshly"),
        "the stored image knew the edit"
    );
    assert_eq!(during.1, vec!["p8".to_owned()]);
    assert!(
        after.0.contains("freshly"),
        "the edit was not found after the check"
    );
    assert_eq!(after.1, vec!["p3".to_owned(), "p8".to_owned()]);
}

/// Test 3: read-your-writes. A page edited during the check is visible to the
/// display reads that follow, still without a parse. (Creating a page is a
/// different read: its name evidence acts on its answer and waits, per the
/// creation guard.)
#[test]
fn an_edit_during_the_launch_check_is_read_back() {
    let launch = Paused::reopen("serve-own-edit", |_| {});
    let graph = &launch.graph;
    let entry = graph
        .entry_for_path(&launch.root.join("pages/p5.md"))
        .unwrap();
    let mut page = graph.load_page(&entry).unwrap();
    let base = page.rev.clone().unwrap();
    page.blocks[0].raw = "TODO ownword [[p4]]".into();
    graph.save_page(&page, Some(&base)).unwrap();
    // A query does not wait: while the edit is being applied it is a
    // retryable NotReady, which Ctrl-K retries (as `QuickSwitcher` does).
    let started = std::time::Instant::now();
    let answered = loop {
        let answer = graph
            .display_read(|| {
                crate::model::search::LAST_FRIENDLY_SERVED_BY.with(|last| last.set(None));
                graph
                    .run_graph_search_displayed_for(
                        "ownword",
                        12,
                        12,
                        None,
                        false,
                        crate::query_plan::FriendlyDisplayOptions::default(),
                        crate::query_plan::FriendlyConsumer::CtrlK,
                    )
                    .map(|answer| (format!("{:?}", answer.hits), linked_pages(graph, "p4")))
            })
            .unwrap();
        match answer {
            Ok(answered) => break answered,
            Err(crate::query::QueryExecutionError::NotReady(_))
                if started.elapsed() < Duration::from_secs(5) =>
            {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => panic!("Ctrl-K did not answer after the edit: {error:?}"),
        }
    };
    let parses = graph.page_build_parses_test();
    let still_held = !launch.graph.direct_projection_test().unwrap().validated();
    launch.release();
    launch.finish();
    assert!(still_held, "the answer waited for the launch check");
    assert!(answered.0.contains("ownword"), "Ctrl-K missed the edit");
    assert_eq!(answered.1, vec!["p3".to_owned(), "p5".to_owned()]);
    assert_eq!(parses, 0, "reading back an edit parsed pages");
}

/// Test 4: a parse configuration changed while closed: every stored fact was
/// written under the old one, so the image serves nothing until the fresh
/// build the check owes.
#[test]
fn a_config_changed_while_closed_serves_nothing_stored() {
    let launch = Paused::reopen("serve-config", |root| {
        fs::create_dir_all(root.join("logseq")).unwrap();
        fs::write(
            root.join("logseq/config.edn"),
            "{:property/separated-by-commas #{:foo}}\n",
        )
        .unwrap();
    });
    let graph = &launch.graph;
    let query = graph
        .display_read(|| graph.run_query_bounded("(task TODO)", 1000, 1 << 20))
        .unwrap();
    let serving = graph.direct_projection_test().unwrap().serving_stored();
    launch.release();
    launch.finish();
    assert!(!serving, "a stale-configuration image was served");
    assert!(
        matches!(query, Err(crate::query::QueryExecutionError::NotReady(_))),
        "a query block was answered from a stale-configuration image: {:?}",
        query.map(|answer| answer.groups.len())
    );
}
