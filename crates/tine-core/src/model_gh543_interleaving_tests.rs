//! GH #543: seeded interleavings of graph operations against the projection.
//!
//! Each seed drives a small graph through a random sequence of saves,
//! creations, deletes, renames, merges, external edits (reported and
//! missed), watcher rescans and restarts, while the launch warm runs on its
//! own thread and a reader keeps asking indexed questions. After the
//! sequence settles, four user outcomes must hold:
//!
//! 1. every wait returned (no reader call, warm or close outlived its bound);
//! 2. readiness is published at the graph's current generation;
//! 3. the index answers what a fresh parse of the disk answers;
//! 4. no consumer ran a whole-graph parse while the projection was alive.
//!
//! Stage 0 of the one-indexing-owner design
//! (`tine-agents/specs/notes/2026-09-22-single-indexing-owner-design.md`).

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// xorshift64*: deterministic and dependency-free.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

const QUERIES: &[&str] = &["(task TODO)", "(task DONE)", "(property status active)"];
const READ_BOUND: Duration = Duration::from_secs(20);
const SETTLE_BOUND: Duration = Duration::from_secs(20);

fn page_text(rng: &mut Rng, names: &[String]) -> String {
    let mut text = String::new();
    for _ in 0..=rng.below(3) {
        let target = &names[rng.below(names.len() as u64) as usize];
        let token = rng.below(1_000);
        text.push_str(&match rng.below(4) {
            0 => format!("- TODO task{token} [[{target}]]\n"),
            1 => format!("- DONE done{token}\n"),
            2 => format!("- plain{token} #{target}\n"),
            _ => format!("- prop{token}\n  status:: active\n"),
        });
    }
    text
}

fn page_path(root: &Path, name: &str) -> PathBuf {
    root.join("pages").join(format!("{name}.md"))
}

/// The app's process-wide permit for whole-graph index passes.
static OWNER_PERMIT: Mutex<()> = Mutex::new(());

/// One graph instance: the projection, its index owner and a reader.
struct Session {
    graph: Arc<Graph>,
    owner: Option<std::thread::JoinHandle<()>>,
    owner_stop: Arc<AtomicBool>,
    settled: Arc<AtomicU64>,
    reader: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    slowest_read_ms: Arc<AtomicU64>,
    read_errors: Arc<Mutex<Vec<String>>>,
}

impl Session {
    fn open(root: &Path, database: &Path) -> Self {
        let graph = Arc::new(Graph::open(root));
        graph
            .attach_direct_projection(database.to_path_buf())
            .unwrap();
        // As the app does: the owner is registered before anything else can
        // reach the graph, and runs until the session closes.
        let registration = graph.register_index_owner();
        let owner_stop = Arc::new(AtomicBool::new(false));
        let settled = Arc::new(AtomicU64::new(0));
        let owner = {
            let (graph, owner_stop, settled) = (
                Arc::clone(&graph),
                Arc::clone(&owner_stop),
                Arc::clone(&settled),
            );
            std::thread::spawn(move || {
                graph.run_index_owner(
                    registration,
                    &OWNER_PERMIT,
                    || owner_stop.load(Ordering::Acquire),
                    || {
                        settled.fetch_add(1, Ordering::AcqRel);
                    },
                );
            })
        };
        let stop = Arc::new(AtomicBool::new(false));
        let slowest_read_ms = Arc::new(AtomicU64::new(0));
        let read_errors = Arc::new(Mutex::new(Vec::new()));
        let reader = {
            let (graph, stop, slowest, errors) = (
                Arc::clone(&graph),
                Arc::clone(&stop),
                Arc::clone(&slowest_read_ms),
                Arc::clone(&read_errors),
            );
            std::thread::spawn(move || {
                let mut turn = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    match turn % 3 {
                        0 => match graph.run_query_bounded(
                            QUERIES[turn % QUERIES.len()],
                            100,
                            1 << 20,
                        ) {
                            Ok(_) | Err(crate::query::QueryExecutionError::NotReady(_)) => {}
                            Err(other) => errors.lock().unwrap().push(format!("query: {other}")),
                        },
                        1 => {
                            let _ = graph.list_pages();
                        }
                        _ => {
                            let _ = graph.referenced_page_names();
                        }
                    }
                    slowest.fetch_max(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                    turn += 1;
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };
        Self {
            graph,
            owner: Some(owner),
            owner_stop,
            settled,
            reader: Some(reader),
            stop,
            slowest_read_ms,
            read_errors,
        }
    }

    /// Stop the reader and wait for the owner's launch completion, each
    /// within its bound. The owner keeps running. A thread that outlives its
    /// bound is a finding; it is leaked, not joined.
    fn quiesce(&mut self, findings: &mut Vec<String>) -> bool {
        self.stop.store(true, Ordering::Relaxed);
        let mut ok = join_within("reader", self.reader.take(), findings);
        let started = Instant::now();
        while self.settled.load(Ordering::Acquire) == 0 && started.elapsed() < SETTLE_BOUND {
            std::thread::sleep(Duration::from_millis(10));
        }
        match self.settled.load(Ordering::Acquire) {
            1 => {}
            0 => {
                findings.push(format!(
                    "the owner never signalled completion (> {SETTLE_BOUND:?})"
                ));
                ok = false;
            }
            n => findings.push(format!("the owner signalled completion {n} times")),
        }
        let slowest = Duration::from_millis(self.slowest_read_ms.load(Ordering::Relaxed));
        if slowest > READ_BOUND {
            findings.push(format!("a read waited {slowest:?}"));
        }
        findings.extend(self.read_errors.lock().unwrap().drain(..));
        ok
    }

    fn close(mut self, findings: &mut Vec<String>) {
        self.owner_stop.store(true, Ordering::Release);
        join_within("owner", self.owner.take(), findings);
        if let Some(projection) = self.graph.direct_projection_test() {
            if !projection.close_and_wait_for_worker(SETTLE_BOUND) {
                findings.push("the worker never released its lease".to_owned());
            }
        }
    }
}

/// Wait for a thread within `SETTLE_BOUND`. A thread that outlives it is a
/// finding; it is leaked, not joined.
fn join_within(
    what: &str,
    handle: Option<std::thread::JoinHandle<()>>,
    findings: &mut Vec<String>,
) -> bool {
    let Some(handle) = handle else {
        return true;
    };
    let started = Instant::now();
    while !handle.is_finished() && started.elapsed() < SETTLE_BOUND {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !handle.is_finished() {
        findings.push(format!("the {what} never returned (> {SETTLE_BOUND:?})"));
        return false;
    }
    if handle.join().is_err() {
        findings.push(format!("the {what} thread panicked"));
    }
    true
}

/// The settled checks: readiness at the current generation, and the index
/// answering what a fresh parse of the disk answers.
/// A session's whole-graph work is bounded: no consumer parsed the graph
/// beside a live index (acting reads excepted), and the owner ran a bounded
/// number of passes -- a pass that settles nothing and repeats at once spun
/// millions of times a second (audit R7-02).
fn check_session_work(
    graph: &Graph,
    acting_parses: usize,
    steps: usize,
    findings: &mut Vec<String>,
) {
    let parses = graph.consumer_page_parses_test() - acting_parses;
    if parses > 0 {
        findings.push(format!(
            "{parses} consumer parse(s) while the projection was alive"
        ));
    }
    let passes = graph.owner_passes_test();
    if passes > 8 + 2 * steps {
        findings.push(format!("the owner ran {passes} passes in {steps} steps"));
    }
}

fn check_settled(root: &Path, graph: &Graph, findings: &mut Vec<String>) {
    let started = Instant::now();
    while !graph.direct_projection_ready_test() {
        if started.elapsed() > SETTLE_BOUND {
            findings.push(format!(
                "readiness never reached cache_generation={} ({})",
                graph.cache_generation(),
                graph
                    .direct_projection_test()
                    .map(|projection| projection.debug_state_test())
                    .unwrap_or_else(|| "no projection".to_owned())
            ));
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let oracle = Graph::open(root);
    for query in QUERIES {
        let expected = crate::query::run_query_bounded(&oracle, query, 1_000, 1 << 24);
        match graph.run_query_bounded(query, 1_000, 1 << 24) {
            Ok(indexed) => {
                let answer = |groups: &[crate::model::RefGroup]| {
                    let mut raws = groups
                        .iter()
                        .flat_map(|group| {
                            group
                                .blocks
                                .iter()
                                .map(move |block| format!("{}|{}", group.page, block.raw))
                        })
                        .collect::<Vec<_>>();
                    raws.sort();
                    raws
                };
                if answer(&indexed.groups) != answer(&expected.groups) {
                    findings.push(format!(
                        "{query}: index {:?} != disk {:?}",
                        answer(&indexed.groups),
                        answer(&expected.groups)
                    ));
                }
            }
            Err(error) => findings.push(format!("{query}: ready index refused: {error}")),
        }
    }
    let names = |graph: &Graph| {
        let mut names = graph
            .list_pages()
            .into_iter()
            .map(|entry| entry.name.to_lowercase())
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    if names(graph) != names(&oracle) {
        findings.push(format!(
            "page list {:?} != disk {:?}",
            names(graph),
            names(&oracle)
        ));
    }
}

/// What the watcher does with one drained batch: note the observation, take
/// its ticket, reconcile the paths (after an uncertain event, first tell the
/// graph its text identity is unknown), and acknowledge the ticket once the
/// batch reconciled. An unacknowledged batch makes creation refuse, as it
/// should.
fn watcher_reconcile(graph: &Graph, uncertain: bool, paths: Vec<PathBuf>) -> Result<(), String> {
    graph.note_graph_text_external_observation();
    let ticket = graph.graph_text_external_observation_ticket();
    if uncertain {
        graph
            .observe_graph_text_external_paths(std::iter::empty::<&Path>(), true)
            .map_err(|e| e.to_string())?;
    }
    for path in paths {
        let synced = if path.exists() {
            graph.sync_file_checked(&path).map(|_| ())
        } else {
            graph.sync_deleted_file(&path).map(|_| ())
        };
        synced.map_err(|e| e.to_string())?;
    }
    graph.acknowledge_graph_text_external_observations(ticket);
    Ok(())
}

/// Run one seed; returns its findings (empty = the seed converged).
fn run_seed(seed: u64, steps: usize) -> Vec<String> {
    if std::env::var_os("TINE_INTERLEAVING_TRACE").is_some() {
        crate::backend_error::set_runtime_debug_diagnostics(true);
    }
    let mut rng = Rng::new(seed);
    let root = scratch(&format!("gh543-interleave-{seed}"));
    let database = root.join("private/projection.sqlite");
    let mut names = (0..5).map(|index| format!("p{index}")).collect::<Vec<_>>();
    for name in names.clone() {
        let text = page_text(&mut rng, &names);
        fs::write(page_path(&root, &name), text).unwrap();
    }
    let mut next_name = names.len();
    let mut findings = Vec::new();
    // Every session reopens a stored image, so any whole-graph parse the
    // sequence sees is a consumer's, not the first open's fresh build.
    {
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        graph
            .wait_for_direct_projection_for_test(SETTLE_BOUND)
            .unwrap();
        crate::direct_projection::release_projection(&graph);
    }
    let mut session = Session::open(&root, &database);
    // Files an external writer changed without the watcher reporting them.
    let mut missed = Vec::<PathBuf>::new();
    // Parses made by whole-graph acting reads, which parse by design.
    let mut acting_parses = 0usize;
    let mut session_steps = 0usize;

    for step in 0..steps {
        std::thread::sleep(Duration::from_millis(rng.below(6)));
        let graph = Arc::clone(&session.graph);
        let step_done = Arc::new(AtomicBool::new(false));
        if std::env::var_os("TINE_INTERLEAVING_TRACE").is_some() {
            let (graph, step_done) = (Arc::clone(&graph), Arc::clone(&step_done));
            std::thread::spawn(move || {
                let started = Instant::now();
                while !step_done.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(500));
                    if started.elapsed() > Duration::from_secs(5) {
                        eprintln!(
                            "  stuck {:?}: gen={} parses={} ready={} {}",
                            started.elapsed(),
                            graph.cache_generation(),
                            graph.consumer_page_parses_test(),
                            graph.direct_projection_ready_test(),
                            graph
                                .direct_projection_test()
                                .map(|projection| projection.debug_state_test())
                                .unwrap_or_default()
                        );
                    }
                }
            });
        }
        let pick =
            |rng: &mut Rng, names: &[String]| names[rng.below(names.len() as u64) as usize].clone();
        let op = rng.below(100);
        if std::env::var_os("TINE_INTERLEAVING_TRACE").is_some() {
            eprintln!("seed {seed} step {step}: op {op} names {names:?}");
        }
        let outcome: Result<(), String> = match op {
            // Save an existing page through the editor path.
            0..=29 if !names.is_empty() => {
                let name = pick(&mut rng, &names);
                let text = page_text(&mut rng, &names);
                match graph.load_named(&name, PageKind::Page) {
                    Ok(Some(mut page)) => {
                        page.blocks = markdown_page_dto(&name, &name, &text).unwrap().blocks;
                        let base = page.rev.clone();
                        graph
                            .save_page(&page, base.as_deref())
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    }
                    other => Err(format!("load {name}: {other:?}")),
                }
            }
            // Create a page.
            30..=41 => {
                let name = format!("p{next_name}");
                next_name += 1;
                let text = page_text(&mut rng, &names);
                let created = graph
                    .save_page(&markdown_page_dto(&name, &name, &text).unwrap(), None)
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                if created.is_ok() {
                    names.push(name);
                }
                created
            }
            42..=49 if names.len() > 1 => {
                let name = pick(&mut rng, &names);
                let deleted = graph
                    .delete_page(&name, PageKind::Page)
                    .map_err(|e| e.to_string());
                if deleted.is_ok() {
                    names.retain(|existing| existing != &name);
                }
                deleted
            }
            50..=57 if !names.is_empty() => {
                let old = pick(&mut rng, &names);
                let new = format!("p{next_name}");
                next_name += 1;
                let renamed = graph.rename_page(&old, &new).map_err(|e| e.to_string());
                if renamed.is_ok() {
                    names.retain(|existing| existing != &old);
                    names.push(new);
                }
                renamed
            }
            58..=61 if names.len() > 1 => {
                let source = pick(&mut rng, &names);
                let target = pick(&mut rng, &names);
                if source == target {
                    Ok(())
                } else {
                    let merged = graph
                        .merge_pages(&format!("pages/{source}.md"), &format!("pages/{target}.md"))
                        .map_err(|e| e.to_string());
                    if merged.is_ok() {
                        names.retain(|existing| existing != &source);
                    }
                    merged
                }
            }
            // An external edit the watcher reports.
            62..=73 if !names.is_empty() => {
                let name = pick(&mut rng, &names);
                let path = page_path(&root, &name);
                fs::write(&path, page_text(&mut rng, &names)).unwrap();
                watcher_reconcile(&graph, false, vec![path])
            }
            // An external edit or delete the watcher missed.
            74..=79 if names.len() > 1 => {
                let name = pick(&mut rng, &names);
                let path = page_path(&root, &name);
                if rng.below(4) == 0 {
                    let _ = fs::remove_file(&path);
                    names.retain(|existing| existing != &name);
                } else {
                    fs::write(&path, page_text(&mut rng, &names)).unwrap();
                }
                missed.push(path);
                Ok(())
            }
            // The watcher's rescan after an uncertain event: an uncertain
            // observation, then the reconcile of every changed path.
            80..=85 => watcher_reconcile(&graph, true, missed.drain(..).collect()),
            // Delete a page that has no file: one that exists only through
            // references (audit R7-01).
            86..=88 => {
                let ghost = format!("g{}", rng.below(3));
                graph
                    .delete_page(&ghost, PageKind::Page)
                    .map_err(|e| e.to_string())
            }
            // A whole-graph acting read (publish, orphan assets, the Guide
            // copy): it parses the graph by design and installs the parsed
            // cache beside the index (audit R7-02).
            89..=92 => {
                let before = graph.consumer_page_parses_test();
                let read = graph
                    .try_with_pages(|pages| pages.len())
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                acting_parses += graph.consumer_page_parses_test() - before;
                read
            }
            // Restart: quiesce, release the lease, reopen on the same database.
            93..=99 => {
                check_session_work(&graph, acting_parses, session_steps, &mut findings);
                acting_parses = 0;
                session_steps = 0;
                drop(graph);
                if !session.quiesce(&mut findings) {
                    break;
                }
                session.close(&mut findings);
                // A missed change is still missed after a restart; the
                // launch check is what must find it.
                missed.clear();
                session = Session::open(&root, &database);
                Ok(())
            }
            _ => Ok(()),
        };
        // An operation may be refused (a stale base, a name clash); what
        // matters is that the index converges on whatever the disk holds.
        step_done.store(true, Ordering::Relaxed);
        session_steps += 1;
        if std::env::var_os("TINE_INTERLEAVING_TRACE").is_some() {
            eprintln!(
                "  -> {outcome:?} parses={}",
                session.graph.consumer_page_parses_test()
            );
        }
    }

    // Settle: report the missed changes as the watcher's rescan would.
    let graph = Arc::clone(&session.graph);
    if !missed.is_empty() {
        let _ = watcher_reconcile(&graph, true, missed.drain(..).collect());
    }
    if session.quiesce(&mut findings) {
        check_settled(&root, &graph, &mut findings);
        check_session_work(&graph, acting_parses, session_steps, &mut findings);
        drop(graph);
        session.close(&mut findings);
        let _ = fs::remove_dir_all(&root);
    }
    findings
}

fn run_seeds(seeds: impl IntoIterator<Item = u64>, steps: usize) {
    let failures = seeds
        .into_iter()
        .filter_map(|seed| {
            let findings = run_seed(seed, steps);
            (!findings.is_empty()).then(|| format!("seed {seed}:\n  {}", findings.join("\n  ")))
        })
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "GH #543: {} interleaving(s) did not converge:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn gh543_seeded_interleavings_converge() {
    let seeds = match std::env::var("TINE_INTERLEAVING_SEED") {
        Ok(seed) => vec![seed.parse().unwrap()],
        Err(_) => (1..=12).collect(),
    };
    // A long-run seed replays with `TINE_INTERLEAVING_STEPS=60`.
    let steps = std::env::var("TINE_INTERLEAVING_STEPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(30);
    run_seeds(seeds, steps);
}

/// The long local run: `TINE_INTERLEAVING_SEEDS=500 cargo test -p tine-core
/// gh543_long_interleaving_run -- --ignored`.
#[test]
#[ignore = "long local run"]
fn gh543_long_interleaving_run() {
    let count = std::env::var("TINE_INTERLEAVING_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200u64);
    run_seeds(1_000..1_000 + count, 60);
}

fn save_existing(graph: &Graph, name: &str, text: &str) {
    let mut page = graph
        .load_named(name, PageKind::Page)
        .unwrap()
        .expect("the page exists");
    page.blocks = markdown_page_dto(name, name, text).unwrap().blocks;
    let base = page.rev.clone();
    graph.save_page(&page, base.as_deref()).unwrap();
}

/// GH #543 (stage-0 harness, seed 6): a reopened image keeps the positions
/// it was written with, so a page deleted last session leaves a gap. The
/// launch check seeded the queue's order from the walk instead, the next new
/// page took a stored page's position (`UNIQUE constraint failed:
/// pages.position`), and every later turn failed: the index stayed down for
/// the whole session.
#[test]
fn gh543_a_page_created_after_reopening_a_graph_with_a_deleted_page_is_indexed() {
    let root = scratch("gh543-reopen-position-gap");
    for index in 0..5 {
        fs::write(
            page_path(&root, &format!("p{index}")),
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
        graph.delete_page("p3", PageKind::Page).unwrap();
        assert!(graph.direct_projection_test().unwrap().wait_drained_test());
        crate::direct_projection::release_projection(&graph);
    }
    let graph = Graph::open(&root);
    graph.attach_direct_projection(database).unwrap();
    graph.warm_cache();
    graph
        .wait_for_direct_projection_for_test(Duration::from_secs(10))
        .unwrap();
    let projection = graph.direct_projection_test().unwrap();
    graph.rename_page("p1", "p9").unwrap();
    assert!(
        projection.wait_drained_test(),
        "the index could not take the rename: {}",
        projection.debug_state_test()
    );
    graph
        .save_page(
            &markdown_page_dto("p7", "p7", "- TODO seven\n").unwrap(),
            None,
        )
        .unwrap();
    assert!(
        projection.wait_drained_test(),
        "{}",
        projection.debug_state_test()
    );
    let mut findings = Vec::new();
    check_settled(&root, &graph, &mut findings);
    assert!(findings.is_empty(), "{findings:#?}");
    assert_eq!(
        graph.consumer_page_parses_test(),
        0,
        "the index was healed by parsing the whole graph"
    );
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(root);
}

/// GH #543 (stage-0 harness, seed 6): the launch warm announced "a warm is
/// coming" for its whole thread, its settle included. When a
/// turn failed after the warm had validated, nothing was queued, so the
/// settle waited for the warm -- itself -- forever, the window never heard
/// `warm-cache-done`, and every page list waited with it.
#[test]
fn gh543_a_turn_that_fails_after_the_launch_check_does_not_hang_the_warm() {
    let root = scratch("gh543-warm-self-wait");
    for index in 0..4 {
        fs::write(
            page_path(&root, &format!("p{index}")),
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
        crate::direct_projection::release_projection(&graph);
    }
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    // The app announces the launch warm before its thread starts.
    let announcement = graph.register_index_owner();
    let pause = graph.pause_next_warm_before_settle_test();
    let warm = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.warm_cache_owned(announcement, || false))
    };
    pause.reached.wait();
    let projection = graph.direct_projection_test().unwrap();
    projection.inject_next_turn_failure_test();
    save_existing(&graph, "p0", "- TODO edited\n");
    assert!(
        !projection.wait_drained_test(),
        "the injected turn failure did not happen"
    );
    pause.release.wait();
    let started = Instant::now();
    while !warm.is_finished() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        warm.is_finished(),
        "the warm's settle waited for the warm itself: {}",
        projection.debug_state_test()
    );
    warm.join().unwrap();
    let lister = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.list_pages().len())
    };
    let started = Instant::now();
    while !lister.is_finished() && started.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        lister.is_finished(),
        "a page list waited for a warm that had ended"
    );
    assert_eq!(lister.join().unwrap(), 4);
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(root);
}

/// GH #543 (R6-04): a cold open with no index on disk walked every page to
/// learn that the index had to be built from scratch, threw those reads
/// away, and then read every page again to build it -- three reads per page
/// in all, counting the build's own re-check. The index already knows it has
/// no image, so the open builds straight away. A reopen over a clean image
/// reads each page once, to validate it, and parses nothing.
#[test]
fn gh543_a_cold_open_reads_each_page_at_most_twice() {
    const PAGES: usize = 12;
    let root = scratch("gh543-cold-open-reads");
    for index in 0..PAGES {
        fs::write(
            page_path(&root, &format!("p{index}")),
            format!("- TODO t{index}\n"),
        )
        .unwrap();
    }
    let database = root.join("private/projection.sqlite");
    let open = |expected_reads: usize, what: &str| {
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        GRAPH_TEXT_CONTENT_READS.with(|reads| reads.set(0));
        graph.warm_cache();
        let reads = GRAPH_TEXT_CONTENT_READS.with(Cell::get);
        graph
            .wait_for_direct_projection_for_test(Duration::from_secs(10))
            .unwrap();
        assert!(
            reads <= expected_reads,
            "{what} read {reads} page files for {PAGES} pages (at most {expected_reads})"
        );
        crate::direct_projection::release_projection(&graph);
    };
    open(2 * PAGES, "a cold open");
    open(PAGES, "a reopen over a clean index");
    let _ = fs::remove_dir_all(root);
}

/// GH #543 (design v4 §0.1): one page that cannot be read left the index
/// unready for the whole session once any other page had changed while the
/// app was closed. The launch validation cannot vouch for an image with an
/// unread page and a changed one, so it asks for a full parse; that parse is
/// missing the unreadable page, and the healthy image refused it rather than
/// delete the page. Nothing else ever produced a complete snapshot, so every
/// query and search stayed unavailable. The index now brings the pages it
/// read current and keeps the unreadable page's stored rows.
#[test]
fn gh543_an_unreadable_page_does_not_keep_the_index_down() {
    let root = scratch("gh543-unreadable-keeps-rows");
    fs::write(page_path(&root, "p0"), "- kept marmot\n").unwrap();
    fs::write(page_path(&root, "p1"), "- old wombat\n").unwrap();
    fs::write(page_path(&root, "p2"), "- steady ibis\n").unwrap();
    let database = root.join("private/projection.sqlite");
    {
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        graph
            .wait_for_direct_projection_for_test(Duration::from_secs(10))
            .unwrap();
        crate::direct_projection::release_projection(&graph);
    }
    // While the app was closed: one page became unreadable, another changed.
    fs::write(page_path(&root, "p0"), [0xff, 0xfe]).unwrap();
    fs::write(page_path(&root, "p1"), "- new quokka\n").unwrap();
    let graph = Graph::open(&root);
    graph.attach_direct_projection(database).unwrap();
    graph.warm_cache();
    let ready = graph.wait_for_direct_projection_for_test(Duration::from_secs(10));
    let projection = graph.direct_projection_test().unwrap();
    assert!(
        ready.is_ok(),
        "one unreadable page kept the index unready: {}",
        projection.debug_state_test()
    );
    let hits = |term: &str| graph.search(term, 20).unwrap().len();
    assert_eq!(
        hits("marmot"),
        1,
        "the unreadable page lost its stored rows"
    );
    assert_eq!(
        hits("quokka"),
        1,
        "the changed page was not brought current"
    );
    assert_eq!(hits("wombat"), 0, "the changed page kept its old rows");
    assert_eq!(hits("ibis"), 1);
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(root);
}

/// An index owner run as the app runs it, on its own thread, until stopped.
struct OwnerRun {
    graph: Arc<Graph>,
    stop: Arc<AtomicBool>,
    settled: Arc<AtomicU64>,
    /// The page files the owner's thread read, returned when it ends.
    handle: Option<std::thread::JoinHandle<usize>>,
}

impl OwnerRun {
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

    /// Wait for the launch completion; `false` if it did not come in time.
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

    /// Stop the owner; the page files its thread read.
    fn stop(mut self) -> usize {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().unwrap()
    }
}

/// Build and release a stored index for `root`, as a previous session did.
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

fn write_pages(root: &Path, count: usize) {
    for index in 0..count {
        fs::write(
            page_path(root, &format!("p{index}")),
            format!("- TODO t{index}\n"),
        )
        .unwrap();
    }
}

/// GH #543 (design v4 stage 2): the index owner signals launch completion
/// once, when the index is ready, and keeps owning index work after it. A
/// query whose read then fails reports the need and returns; it used to
/// repair on the query thread, reading and parsing every page itself while
/// the user waited for the answer.
#[test]
fn gh543_after_launch_a_failed_read_is_repaired_by_the_owner_not_the_query() {
    const PAGES: usize = 6;
    let root = scratch("gh543-owner-failed-read");
    write_pages(&root, PAGES);
    let database = root.join("private/projection.sqlite");
    // A reopen: the index is validated, and no parsed cache exists that a
    // repair could reuse without reading the pages.
    prebuild_index(&root, &database);
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = OwnerRun::start(&graph);
    assert!(
        owner.wait_settled(Duration::from_secs(10)),
        "no completion signal"
    );
    assert!(
        graph.direct_projection_ready_test(),
        "completion was signalled before the index was ready"
    );
    let projection = graph.direct_projection_test().unwrap();
    projection.inject_next_statement_failure();
    GRAPH_TEXT_CONTENT_READS.with(|reads| reads.set(0));
    let _ = graph.run_query_bounded("(task TODO)", 100, 1 << 20);
    let query_reads = GRAPH_TEXT_CONTENT_READS.with(Cell::get);
    assert_eq!(
        query_reads, 0,
        "the query thread read {query_reads} page files to repair the index itself"
    );
    assert!(
        owner.wait_ready(Duration::from_secs(10)),
        "the owner did not repair the index: {}",
        projection.debug_state_test()
    );
    let answer = graph
        .run_query_bounded("(task TODO)", 100, 1 << 20)
        .unwrap();
    assert_eq!(answer.groups.len(), PAGES);
    assert_eq!(owner.settled.load(Ordering::Acquire), 1);
    assert_eq!(graph.consumer_page_parses_test(), 0);
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(root);
}

/// GH #543: a page deleted between a whole-graph listing and its read is a
/// page that is gone, not one Tine failed to read -- and so is one deleted
/// between the listing's directory read and its open of the file. Recording it as a read
/// failure left the index looking incomplete, so the next page creation
/// parsed the whole graph on the save thread to prove its name was free.
/// Both passes that list the graph -- the launch check of a stored index and
/// the parse of a graph with no index -- must agree.
#[test]
fn gh543_a_page_deleted_during_a_pass_is_gone_not_unreadable() {
    const PAGES: usize = 6;
    for (stored_index, inside_listing) in
        [(true, false), (false, false), (true, true), (false, true)]
    {
        let root = scratch("gh543-vanished-page");
        write_pages(&root, PAGES);
        let database = root.join("private/projection.sqlite");
        if stored_index {
            prebuild_index(&root, &database);
        }
        let graph = Arc::new(Graph::open(&root));
        graph.attach_direct_projection(database).unwrap();
        if inside_listing {
            graph.vanish_inside_next_listing_test(page_path(&root, "p2"));
        } else {
            graph.vanish_after_next_listing_test(page_path(&root, "p2"));
        }
        let owner = OwnerRun::start(&graph);
        assert!(
            owner.wait_settled(Duration::from_secs(10)),
            "no completion signal (stored index: {stored_index}, inside listing: {inside_listing})"
        );
        assert!(owner.wait_ready(Duration::from_secs(10)));
        let failures = graph.page_index_failures.read().unwrap().clone();
        assert!(
            failures.is_empty(),
            "a deleted page was recorded as unreadable (stored index: {stored_index}, inside listing: {inside_listing}): {failures:?}"
        );
        let parses_before = graph.consumer_page_parses_test();
        graph
            .save_page(
                &markdown_page_dto("fresh", "fresh", "- new\n").unwrap(),
                None,
            )
            .unwrap();
        assert_eq!(
            graph.consumer_page_parses_test() - parses_before,
            0,
            "creating a page parsed the graph (stored index: {stored_index}, inside listing: {inside_listing})"
        );
        let names = graph
            .list_pages()
            .into_iter()
            .map(|entry| entry.name.to_lowercase())
            .collect::<Vec<_>>();
        assert!(!names.contains(&"p2".to_owned()), "{names:?}");
        assert!(names.contains(&"fresh".to_owned()), "{names:?}");
        owner.stop();
        crate::direct_projection::release_projection(&graph);
        let _ = fs::remove_dir_all(root);
    }
}

/// GH #543 (design v4 stage 3): readers with no retry of their own -- a
/// block reference's page, the real page names a query resolves against,
/// and the print/publish reference walk -- wait while the owner's launch
/// pass is coming, instead of parsing the graph that pass is reading. Each
/// read answers from the index once the pass lands.
#[test]
fn gh543_readers_without_a_retry_wait_for_the_launch_pass() {
    const PAGES: usize = 6;
    for reader in ["block page hint", "real page names", "reference walk"] {
        let root = scratch("gh543-ng4-readers");
        write_pages(&root, PAGES);
        fs::write(
            page_path(&root, "p1"),
            "- TODO t1\n  id:: 6512a0f4-0000-4000-8000-000000000001\n- see [[p3]]\n",
        )
        .unwrap();
        let database = root.join("private/projection.sqlite");
        prebuild_index(&root, &database);
        let graph = Arc::new(Graph::open(&root));
        graph.attach_direct_projection(database).unwrap();
        let pause = graph.pause_next_warm_after_read_test();
        let owner = OwnerRun::start(&graph);
        pause.reached.wait();
        let read = {
            let graph = Arc::clone(&graph);
            std::thread::spawn(move || match reader {
                "block page hint" => graph
                    .block_page_hint("6512a0f4-0000-4000-8000-000000000001")
                    .unwrap_or_default(),
                "real page names" => {
                    let names = crate::query::real_page_names(&*graph);
                    names
                        .get("p3")
                        .map(|(_, name)| name.clone())
                        .unwrap_or_default()
                }
                _ => {
                    let found = graph.reference_candidate_pages(
                        &["p3".to_owned()],
                        "elsewhere",
                        ReferenceKind::Plain,
                    );
                    found
                        .pages
                        .iter()
                        .map(|(entry, _)| entry.name.clone())
                        .collect::<Vec<_>>()
                        .join(",")
                }
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        pause.release.wait();
        let answer = read.join().unwrap();
        assert_eq!(
            graph.consumer_page_parses_test(),
            0,
            "{reader}: parsed the graph beside the launch pass"
        );
        assert!(
            answer.contains("p1") || answer == "p3",
            "{reader}: answered {answer:?}"
        );
        assert!(owner.wait_settled(Duration::from_secs(10)));
        owner.stop();
        crate::direct_projection::release_projection(&graph);
        let _ = fs::remove_dir_all(root);
    }
}

/// GH #543 (design v4 stage 2): a rename landing inside the launch check
/// costs at most one more whole-graph pass, and the owner settles.
#[test]
fn gh543_a_rename_during_the_launch_check_costs_one_more_pass() {
    const PAGES: usize = 8;
    let root = scratch("gh543-owner-rename");
    write_pages(&root, PAGES);
    let database = root.join("private/projection.sqlite");
    prebuild_index(&root, &database);
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let pause = graph.pause_next_warm_after_read_test();
    let owner = OwnerRun::start(&graph);
    pause.reached.wait();
    // The walk holds the graph-text permit, so the rename lands as soon as
    // the walk lets go of it: between its read and its drift check.
    let rename = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.rename_page("p3", "renamed").unwrap())
    };
    std::thread::sleep(Duration::from_millis(50));
    pause.release.wait();
    rename.join().unwrap();
    assert!(
        owner.wait_settled(Duration::from_secs(10)),
        "no completion signal"
    );
    assert!(owner.wait_ready(Duration::from_secs(10)));
    let passes = graph.owner_passes_test();
    assert!(
        passes <= 2,
        "{passes} whole-graph passes for one raced check"
    );
    assert_eq!(graph.consumer_page_parses_test(), 0);
    let names = graph
        .list_pages()
        .into_iter()
        .map(|entry| entry.name.to_lowercase())
        .collect::<Vec<_>>();
    assert!(names.contains(&"renamed".to_owned()) && !names.contains(&"p3".to_owned()));
    let reads = owner.stop();
    assert!(
        reads <= 3 * PAGES + 2,
        "the owner read {reads} page files for {PAGES} pages"
    );
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(root);
}

/// GH #543 (design v4 stage 2): a page that panics the parser does not keep
/// the owner passing over the whole graph. The index keeps the page's
/// stored rows, becomes ready, and no further pass follows.
#[test]
fn gh543_a_panicking_page_costs_a_bounded_number_of_passes() {
    const PAGES: usize = 6;
    let root = scratch("gh543-owner-panic");
    write_pages(&root, PAGES);
    let database = root.join("private/projection.sqlite");
    prebuild_index(&root, &database);
    // While the app was closed: one page came to panic the parser, another
    // changed.
    fs::write(
        page_path(&root, "p0"),
        format!("- {TEST_PAGE_PARSE_PANIC_SENTINEL}\n"),
    )
    .unwrap();
    fs::write(page_path(&root, "p1"), "- TODO changed\n").unwrap();
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = OwnerRun::start(&graph);
    assert!(
        owner.wait_settled(Duration::from_secs(10)),
        "no completion signal"
    );
    let projection = graph.direct_projection_test().unwrap();
    assert!(
        owner.wait_ready(Duration::from_secs(10)),
        "the panicking page kept the index unready: {}",
        projection.debug_state_test()
    );
    std::thread::sleep(Duration::from_millis(1_500));
    let passes = graph.owner_passes_test();
    assert!(
        passes <= 2,
        "{passes} whole-graph passes over one panicking page"
    );
    assert_eq!(graph.search("changed", 20).unwrap().len(), 1);
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(root);
}

/// GH #543 (design v4 stage 2): a graph that cannot be listed backs the
/// owner off instead of passing over it again and again, and launch
/// completion is still signalled so the app's waiting reads go ahead.
#[cfg(unix)]
#[test]
fn gh543_an_unlistable_graph_backs_the_owner_off() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("gh543-owner-unlistable");
    write_pages(&root, 4);
    // The index lives outside the graph, so only the graph is unreadable.
    let database = root.with_extension("projection.sqlite");
    let _ = fs::remove_file(&database);
    prebuild_index(&root, &database);
    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&root).is_ok() {
        // Running with permissions that ignore the mode (root): nothing to test.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(root);
        return;
    }
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database.clone()).unwrap();
    let owner = OwnerRun::start(&graph);
    let settled = owner.wait_settled(Duration::from_secs(10));
    std::thread::sleep(Duration::from_secs(4));
    let passes = graph.owner_passes_test();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    owner.stop();
    assert!(
        settled,
        "no completion signal while the graph was unlistable"
    );
    // Two at once, then after 1 s and 2 s more: at most five in these ~4 s.
    assert!(passes >= 2, "the owner never tried: {passes} passes");
    assert!(
        passes <= 5,
        "{passes} whole-graph passes in 4 s over an unlistable graph"
    );
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(database);
}

/// GH #543 (design v4 stage 4, R6-03; audit R8-04): a second writer for the
/// same index -- switching back to a graph whose previous writer is still
/// finishing -- waits for the lease instead of giving the session's index
/// up. An in-process holder is on its way out, so the index counts as coming:
/// the owner does not settle and nothing parses the graph meanwhile; once the
/// first writer lets go the index comes back.
#[test]
fn gh543_a_writer_waiting_for_an_in_process_lease_takes_it_when_released() {
    let root = scratch("gh543-lease-in-process");
    write_pages(&root, 4);
    let database = root.join("private/projection.sqlite");
    let first = Graph::open(&root);
    first.attach_direct_projection(database.clone()).unwrap();
    first.warm_cache();
    first
        .wait_for_direct_projection_for_test(Duration::from_secs(10))
        .unwrap();
    let second = Arc::new(Graph::open(&root));
    second.attach_direct_projection(database).unwrap();
    let owner = OwnerRun::start(&second);
    assert!(
        !owner.wait_settled(Duration::from_millis(1500)),
        "the owner gave up an index that an in-process writer is about to release"
    );
    assert!(first.detach_direct_projection(Duration::from_secs(5)));
    assert!(
        owner.wait_ready(Duration::from_secs(5)),
        "the index did not come back once the first writer released the lease"
    );
    assert!(
        owner.wait_settled(Duration::from_secs(5)),
        "the owner did not settle after taking the released lease"
    );
    let names = crate::query::real_page_names(&*second);
    assert!(names.contains_key("p1"), "a read answered {names:?}");
    owner.stop();
    crate::direct_projection::release_projection(&second);
    let _ = fs::remove_dir_all(root);
}

/// GH #543 (design v4 stage 4): a lease held outside this process -- another
/// Tine instance on the same graph -- is retried on a backoff, not given up.
#[test]
fn gh543_a_lease_held_by_another_process_is_retried() {
    use fs2::FileExt as _;
    let root = scratch("gh543-lease-external");
    write_pages(&root, 4);
    let database = root.join("private/projection.sqlite");
    prebuild_index(&root, &database);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(database.with_extension("sqlite.writer.lock"))
        .unwrap();
    lock.try_lock_exclusive().unwrap();
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = OwnerRun::start(&graph);
    assert!(
        owner.wait_settled(Duration::from_secs(10)),
        "the owner waited for an index another process holds"
    );
    fs2::FileExt::unlock(&lock).unwrap();
    drop(lock);
    assert!(
        owner.wait_ready(Duration::from_secs(10)),
        "the index never came back after the other process let the lease go"
    );
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(root);
}

/// GH #543 (audit R7-01): deleting a page that has no file -- one that exists
/// only through references -- changes nothing the index stores. It marked the
/// index stale, and the owner walked every page file to re-validate it; during
/// the launch check the running walk was abandoned and read the graph again.
#[test]
fn gh543_deleting_a_page_with_no_file_does_not_walk_the_graph() {
    const PAGES: usize = 12;
    let root = scratch("gh543-fileless-delete");
    write_pages(&root, PAGES);
    let database = root.join("private/projection.sqlite");
    prebuild_index(&root, &database);

    // After launch.
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database.clone()).unwrap();
    let owner = OwnerRun::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(10)));
    assert!(owner.wait_ready(Duration::from_secs(10)));
    let passes_before = graph.owner_passes_test();
    graph.delete_page("Ghost", PageKind::Page).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(owner.wait_ready(Duration::from_secs(10)));
    let passes = graph.owner_passes_test() - passes_before;
    let reads = owner.stop();
    crate::direct_projection::release_projection(&graph);
    assert_eq!(passes, 0, "the delete started a whole-graph pass");
    assert_eq!(reads, PAGES, "the owner read {reads} page files");

    // During the launch check.
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let pause = graph.pause_next_warm_after_read_test();
    let owner = OwnerRun::start(&graph);
    pause.reached.wait();
    let deleted = {
        let graph = Arc::clone(&graph);
        std::thread::spawn(move || graph.delete_page("Ghost", PageKind::Page))
    };
    std::thread::sleep(Duration::from_millis(50));
    pause.release.wait();
    deleted.join().unwrap().unwrap();
    assert!(owner.wait_settled(Duration::from_secs(10)));
    assert!(owner.wait_ready(Duration::from_secs(10)));
    let passes = graph.owner_passes_test();
    let reads = owner.stop();
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(passes, 1, "the launch check ran {passes} passes");
    assert_eq!(reads, PAGES, "the launch check read {reads} page files");
}

/// GH #543 (audit R7-02): readiness is never published over an image the
/// decider still owes a pass. A page update's turn published readiness while
/// the image was marked stale; the owner's validation then found the image
/// "ready", did nothing, and ran again at once -- millions of passes a
/// second for as long as the graph stayed open.
#[test]
fn gh543_a_stale_image_is_not_made_ready_by_a_page_update() {
    const PAGES: usize = 12;
    let root = scratch("gh543-stale-not-ready");
    write_pages(&root, PAGES);
    let database = root.join("private/projection.sqlite");
    prebuild_index(&root, &database);
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = OwnerRun::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(10)));
    assert!(owner.wait_ready(Duration::from_secs(10)));
    // Another graph's pass holds the process-wide permit.
    let held = OWNER_PERMIT.lock().unwrap();
    graph.direct_projection_mark_stale_test();
    // A whole-graph read installs the parsed cache; a save queues a delta.
    graph.try_with_pages(|pages| pages.len()).unwrap();
    save_existing(&graph, "p1", "- edited while stale");
    std::thread::sleep(Duration::from_millis(300));
    let ready_while_stale = graph.direct_projection_ready_test();
    let passes_before = graph.owner_passes_test();
    drop(held);
    assert!(owner.wait_ready(Duration::from_secs(10)));
    std::thread::sleep(Duration::from_millis(300));
    let passes = graph.owner_passes_test() - passes_before;
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(&root);
    assert!(!ready_while_stale, "a page update made a stale image ready");
    assert!(passes <= 2, "the owner ran {passes} passes: it spins");
}

/// GH #543 (audit R7-02b): a validation that answers "fresh build required"
/// leaves the image it found, and that image does not answer for the pages.
/// The turn published readiness over it anyway, the owner's fresh build was
/// offered to an index that called itself current, and the owner looped on
/// a need that never cleared -- with the rebuild the failed read asked for
/// never happening.
#[test]
fn gh543_a_validation_that_needs_a_fresh_build_is_not_ready() {
    const PAGES: usize = 12;
    let root = scratch("gh543-fresh-not-ready");
    write_pages(&root, PAGES);
    let database = root.join("private/projection.sqlite");
    prebuild_index(&root, &database);
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = OwnerRun::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(10)));
    assert!(owner.wait_ready(Duration::from_secs(10)));
    let passes_before = graph.owner_passes_test();
    // A validation is owed; a read fails on the image while it is queued.
    let pause = graph.pause_next_warm_before_enqueue_test();
    graph.direct_projection_mark_stale_test();
    pause.reached.wait();
    graph.direct_projection_recover_after_failed_read_test();
    pause.release.wait();
    assert!(owner.wait_ready(Duration::from_secs(10)));
    std::thread::sleep(Duration::from_millis(300));
    let passes = graph.owner_passes_test() - passes_before;
    let parses = graph.page_build_parses_test();
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(&root);
    assert!(passes <= 3, "the owner ran {passes} passes: it spins");
    assert!(
        parses >= PAGES,
        "the rebuild the failed read asked for never ran ({parses} parses)"
    );
}

/// GH #543 (audit R7-06): the owner of a retired graph runs no more passes.
/// A refresh retires the old graph before it cancels the old owner, and in
/// between the owner could walk the retired graph for a need that arose.
#[test]
fn gh543_a_retired_graphs_owner_runs_no_more_passes() {
    let root = scratch("gh543-retired-owner");
    write_pages(&root, 6);
    let database = root.join("private/projection.sqlite");
    prebuild_index(&root, &database);
    let graph = Arc::new(Graph::open(&root));
    graph.attach_direct_projection(database).unwrap();
    let owner = OwnerRun::start(&graph);
    assert!(owner.wait_settled(Duration::from_secs(10)));
    assert!(owner.wait_ready(Duration::from_secs(10)));
    let passes_before = graph.owner_passes_test();
    graph.retire();
    graph.direct_projection_mark_stale_test();
    std::thread::sleep(Duration::from_millis(500));
    let passes = graph.owner_passes_test() - passes_before;
    owner.stop();
    crate::direct_projection::release_projection(&graph);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(passes, 0, "the owner walked a retired graph");
}
