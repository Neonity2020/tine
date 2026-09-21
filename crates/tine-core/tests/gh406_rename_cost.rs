//! GH #406 research fixture — "renaming a page takes about 20 seconds".
//!
//! This is a RESEARCH fixture for the rename-cost investigation, not a
//! regression test and not a budget gate. Every test here is `#[ignore]`d: the
//! nextest selection contract skips ignored tests outright, so this file costs
//! the ordinary suite nothing and must be run deliberately:
//!
//!     cargo test --test gh406_rename_cost -- --ignored --nocapture
//!
//! WHY a fixture rather than a reading of the code. v0.6.983 made the Direct
//! Files reference scan ~34.5% faster on a 10,005-file graph, and the reporter
//! retested and said the total was unchanged. So the released improvement did
//! not touch whatever dominates the user-visible wait, and no local harness
//! measured the whole operation: `scripts/e2e-rename.mjs` is a correctness
//! journey on a tiny seeded graph with no timing at all. The open question is
//! therefore not "is rename slow" but "WHICH term dominates", and that is a
//! measurement.
//!
//! What this measures and what it deliberately does NOT.
//!
//! `rename_page_reporting` is the backend transaction only. The frontend adds
//! its own tail that this fixture cannot see — `refreshAfterRename` in
//! src/graph.ts resets the store, bumps the graph epoch and refetches aliases
//! and page identities — so a number here is a LOWER BOUND on the user's wait,
//! never the whole of it. Attributing the reporter's 20 s needs this number
//! plus the frontend tail, measured on the platform they reported (Windows),
//! whose per-file open cost is materially higher than Linux's.
//!
//! The comparison that makes the number interpretable is the raw I/O floor:
//! the time to read every graph file once, which phase 0b of the transaction
//! must pay in some form because it rewrites references across the whole graph.
//! - rename ≈ 1x floor means the cost IS the unavoidable full-graph read, and
//!   only an index that names the referring files can help.
//! - rename >> 1x floor means the extra passes and per-file allocations are
//!   the fat, and the read is not the problem.
//! That ratio, not the absolute millisecond count on this machine, is the
//! finding worth carrying into a fix.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tine_core::Graph;

const TARGET: &str = "Rename Target";
const RENAMED: &str = "Rename Target Renamed";

/// One page in `pages/`. A small minority reference the rename target, which
/// is the realistic shape: a rename rewrites a few files but phase 0b still
/// examines every file in the graph.
fn seed_graph(tag: &str, pages: usize, referrer_every: usize) -> (PathBuf, Graph) {
    let root = std::env::temp_dir().join(format!("tine-gh406-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("pages")).unwrap();
    std::fs::create_dir_all(root.join("journals")).unwrap();

    std::fs::write(
        root.join("pages").join(format!("{TARGET}.md")),
        "- the page being renamed\n- with a second block\n",
    )
    .unwrap();

    for i in 0..pages {
        // Every `referrer_every`-th page links the target; the rest never
        // mention it and exist only to be examined.
        let body = if i % referrer_every == 0 {
            format!("- page {i} mentions [[{TARGET}]] in its first block\n- and some filler\n")
        } else {
            format!("- page {i} first block with ordinary prose and no references\n- filler\n")
        };
        std::fs::write(root.join("pages").join(format!("Page {i:06}.md")), body).unwrap();
    }

    // Open after the raw writes so the whole seeded set joins the inventory the
    // way a real graph does on launch.
    let graph = Graph::open(&root);
    (root, graph)
}

/// The unavoidable floor: read every file in the graph once, from disk.
fn read_every_file_once(root: &Path) -> (Duration, u64) {
    let started = Instant::now();
    let mut bytes = 0_u64;
    for entry in std::fs::read_dir(root.join("pages")).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            bytes += std::fs::read(&path).unwrap().len() as u64;
        }
    }
    (started.elapsed(), bytes)
}

fn measure(pages: usize) -> (Duration, Duration, u64) {
    measure_with_referrers(pages, 50)
}

fn measure_with_referrers(pages: usize, referrer_every: usize) -> (Duration, Duration, u64) {
    let (root, graph) = seed_graph(&format!("n{pages}r{referrer_every}"), pages, referrer_every);
    // Read the corpus once before timing either number, so neither measurement
    // is the one that happens to pay for a cold page cache.
    let _ = read_every_file_once(&root);
    let (floor, bytes) = read_every_file_once(&root);

    let started = Instant::now();
    graph
        .rename_page_reporting(TARGET, RENAMED, None)
        .expect("the rename succeeds on a clean seeded graph");
    let rename = started.elapsed();

    assert!(
        root.join("pages").join(format!("{RENAMED}.md")).exists(),
        "sanity: the rename must actually have moved the file"
    );
    let _ = std::fs::remove_dir_all(&root);
    (rename, floor, bytes)
}

#[test]
#[ignore = "research measurement; run with --ignored"]
fn gh406_rename_cost_against_the_full_graph_read_floor() {
    println!(
        "{:>8} {:>12} {:>12} {:>8} {:>12}",
        "pages", "rename ms", "read ms", "ratio", "bytes"
    );
    let mut rows = Vec::new();
    for pages in [500_usize, 2_000, 8_000] {
        let (rename, floor, bytes) = measure(pages);
        let ratio = rename.as_secs_f64() / floor.as_secs_f64().max(f64::EPSILON);
        println!(
            "{:>8} {:>12.1} {:>12.1} {:>8.1} {:>12}",
            pages,
            rename.as_secs_f64() * 1000.0,
            floor.as_secs_f64() * 1000.0,
            ratio,
            bytes
        );
        rows.push((pages, rename, ratio));
    }

    // Report the scaling shape rather than asserting a budget: this fixture
    // exists to produce a number a human reads, and a threshold here would be
    // a machine-speed assertion masquerading as a contract.
    let (small_pages, small, _) = rows.first().copied().unwrap();
    let (large_pages, large, _) = rows.last().copied().unwrap();
    let growth = large.as_secs_f64() / small.as_secs_f64().max(f64::EPSILON);
    let page_growth = large_pages as f64 / small_pages as f64;
    println!(
        "scaling: {page_growth:.0}x the pages cost {growth:.1}x the time \
         (linear would be ~{page_growth:.0}x)"
    );
}

/// Hold the graph size fixed and vary only how many files the rename actually
/// EDITS. This is the measurement that tells the two candidate shapes apart,
/// and they call for opposite fixes:
///
/// - Flat across referrer counts => the cost is the full-graph examination in
///   phase 0b, paid once per file no matter how few of them change. Only an
///   index that names the referring files removes it.
/// - Rising with referrer count => the per-edit work dominates, and it is
///   superlinear by construction: `inventory.retain` / `failures.retain` run
///   once per edit across the whole inventory (page_rename.rs:593-596), and
///   every edited file is parsed twice from the same bytes (:597-605, then
///   :637-641). Both are fixable without any new index.
#[test]
#[ignore = "research measurement; run with --ignored"]
fn gh406_rename_cost_against_the_edited_file_count() {
    const PAGES: usize = 8_000;
    println!(
        "{:>10} {:>10} {:>12} {:>12} {:>8}",
        "referrers", "pages", "rename ms", "read ms", "ratio"
    );
    for referrer_every in [PAGES, 400, 50, 10] {
        let referrers = PAGES.div_ceil(referrer_every);
        let (rename, floor, _) = measure_with_referrers(PAGES, referrer_every);
        println!(
            "{:>10} {:>10} {:>12.1} {:>12.1} {:>8.1}",
            referrers,
            PAGES,
            rename.as_secs_f64() * 1000.0,
            floor.as_secs_f64() * 1000.0,
            rename.as_secs_f64() / floor.as_secs_f64().max(f64::EPSILON)
        );
    }
}
