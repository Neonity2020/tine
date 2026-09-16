//! GH #550 regression/measurement fixture: cold-open contention on the first
//! foreground commands.
//!
//! The Android report (0.6.982, build 33175f2) showed a Direct Files open that
//! finished in 384 ms, then `journal_feed_page` (8.4 s), `list_pages` (14.3 s),
//! `get_page` (14.4 s) and `list_templates` (14.4 s) while the background warm
//! parsed the same graph, and the frontend heartbeat slipped 7–9 s. This file
//! reproduces the CONTENDED SHAPE deterministically on a synthetic
//! real-graph-scale fixture (902 journals + 143 pages ≈ 4.5 MB, the scale of
//! the anonymized mirror corpus) and prints a timing table; assertions cover
//! only deterministic invariants, never wall-clock values.
//!
//! Run with `--nocapture` to see the table. For the Android-proxy condition,
//! pin the test to few cores (e.g. `taskset -c 0,1`): the phone's small core
//! count is what turns the redundant whole-graph passes into an 8–14 s stall.
//!
//! See the research report:
//! /aux/koutecky/logseq/tine-agents/opencode/reports/2026-09-16-tine-550-research.md

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tine_core::{Graph, PageKind};

/// Deterministic pseudo-random stream (xorshift64*) so every run builds the
/// byte-identical graph and timings stay comparable across invocations.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const JOURNAL_COUNT: usize = 902;
const PAGE_COUNT: usize = 143;

/// One deterministic journal body (~2–6 KB of ordinary outliner blocks).
fn journal_body(rng: &mut Rng, index: usize) -> String {
    let mut out = String::new();
    let blocks = 6 + rng.below(10) as usize;
    for b in 0..blocks {
        let target = rng.below(PAGE_COUNT as u64);
        let words = 8 + rng.below(30) as usize;
        out.push_str(&format!(
            "- item {index}.{b} with {words} words of ordinary text"
        ));
        for w in 0..words {
            out.push_str(&format!(" word{w}{}", rng.below(997)));
        }
        out.push_str(&format!(" #tag{} [[page_{target}]]\n", rng.below(9)));
        if rng.below(3) == 0 {
            out.push_str(&format!(
                "\t- child note {} with a nested line and a timestamp\n",
                rng.below(99_999)
            ));
        }
        if rng.below(4) == 0 {
            out.push_str(&format!("- TODO follow up on {index}.{b}\n"));
        }
        if rng.below(5) == 0 {
            out.push_str(&format!("- DONE wrapped {b}\n"));
        }
    }
    out
}

/// One deterministic named-page body; a few carry a `template::` property so
/// `templates()` has real hits.
fn page_body(rng: &mut Rng, index: usize, templated: bool) -> String {
    let mut out = String::new();
    if templated {
        out.push_str(&format!("- template:: tpl-{index}\n"));
    }
    out.push_str(&format!("- top of page_{index}\n"));
    let blocks = 10 + rng.below(20) as usize;
    for b in 0..blocks {
        let words = 10 + rng.below(40) as usize;
        out.push_str(&format!("- page {index} block {b}:"));
        for w in 0..words {
            out.push_str(&format!(" token{w}{}", rng.below(9973)));
        }
        out.push_str(&format!(" #ptag{}\n", rng.below(7)));
        if rng.below(3) == 0 {
            out.push_str(&format!(
                "\t- nested detail {} referencing [[page_{}]]\n",
                rng.below(9999),
                rng.below(PAGE_COUNT as u64)
            ));
        }
    }
    out
}

/// Build the synthetic graph at the scale of the anonymized mirror corpus
/// (1,045 files ≈ 4.5 MB). Returns the root and the newest journal's name.
fn build_fixture(tag: &str) -> (PathBuf, String) {
    let root =
        std::env::temp_dir().join(format!("tine-gh550-fixture-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("journals")).unwrap();
    std::fs::create_dir_all(root.join("pages")).unwrap();

    // 902 consecutive journal days ending 2026-09-10, so every date is in the
    // past and inside the feed window regardless of the run date.
    let mut rng = Rng(0x5505_5055_5505_5055);
    let mut day = chrono_like_day(2026, 9, 10);
    for i in 0..JOURNAL_COUNT {
        let (y, m, d) = civil_from_day(day);
        let body = journal_body(&mut rng, i);
        std::fs::write(
            root.join("journals")
                .join(format!("{y:04}_{m:02}_{d:02}.md")),
            body,
        )
        .unwrap();
        day -= 1;
    }
    for i in 0..PAGE_COUNT {
        let body = page_body(&mut rng, i, i % 17 == 0);
        std::fs::write(root.join("pages").join(format!("page_{i}.md")), body).unwrap();
    }

    // The name the feed surfaces for the newest journal day (Logseq display
    // name format used by the demo graph tests is not needed here: we address
    // it through the entries journals_desc returns).
    (root, String::new())
}

// ---- minimal proleptic-Gregorian day arithmetic (no chrono dependency) ----

fn civil_from_day(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn chrono_like_day(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era as i64 * 146_097 + doe as i64 - 719_468
}

// ---- measurement helpers ----

struct Reading {
    label: &'static str,
    ms: u128,
    detail: &'static str,
}

fn ms(duration: Duration) -> u128 {
    duration.as_millis()
}

fn print_readings(title: &str, readings: &[Reading]) {
    println!("\n=== GH #550 fixture: {title} ===");
    for r in readings {
        println!("  {:<34} {:>8} ms   {}", r.label, r.ms, r.detail);
    }
}

/// The core work of the `journal_feed_page` command on a cold cache:
/// journals-desc enumeration plus parsing the first three feed rows
/// (FEED_PAGE = 3 in src/components/Page.tsx).
fn feed_window_core(graph: &Graph) -> usize {
    let entries = graph.journals_desc();
    let mut loaded = 0;
    for entry in entries.iter().take(3) {
        if graph.load_page(entry).is_ok() {
            loaded += 1;
        }
    }
    loaded
}

#[test]
fn gh550_cold_open_contention_measurements() {
    let (root_a, _) = build_fixture("control");
    let (root_b, _) = build_fixture("contended");

    // -- Control: cold open, no warm running; each surface measured in
    // isolation. This is the desktop-experience baseline; list_pages now owns
    // (or joins) the same page-build flight as templates.
    let graph = Graph::open(&root_a);
    let t = Instant::now();
    let feed_loaded = feed_window_core(&graph);
    let feed_alone = t.elapsed();
    assert_eq!(feed_loaded, 3, "the feed window must load three journals");

    let t = Instant::now();
    let pages = graph.list_pages();
    let list_pages_alone = t.elapsed();
    assert_eq!(
        pages.len(),
        JOURNAL_COUNT + PAGE_COUNT,
        "every synthetic file must be listed"
    );

    let graph2 = Graph::open(&root_a);
    let t = Instant::now();
    let templates = graph2.templates();
    let templates_alone = t.elapsed();
    assert!(!templates.is_empty(), "fixture must contain templates");
    let t = Instant::now();
    let newest = graph2
        .journals_desc()
        .first()
        .cloned()
        .expect("the fixture has journals");
    let named = graph2
        .load_named(&newest.name, PageKind::Journal)
        .expect("the newest journal must load")
        .expect("the newest journal must exist");
    let get_page_alone = t.elapsed();
    assert!(!named.name.is_empty());

    print_readings(
        "control (cold open, no warm, surfaces in isolation)",
        &[
            Reading {
                label: "journal_feed_page core",
                ms: ms(feed_alone),
                detail: "3 feed rows + metadata walk",
            },
            Reading {
                label: "list_pages",
                ms: ms(list_pages_alone),
                detail: "shared page-build flight",
            },
            Reading {
                label: "list_templates",
                ms: ms(templates_alone),
                detail: "with_pages builds the whole-graph cache",
            },
            Reading {
                label: "get_page (newest journal)",
                ms: ms(get_page_alone),
                detail: "find_entry -> list_pages -> parse one page",
            },
        ],
    );

    // -- Contended open: mirrors the app's real cold start. The four surfaces
    // fire concurrently (as the frontend's post-binding fetches do) while the
    // background warm (`warm_cache_async`, 250 ms delayed in
    // src-tauri/src/graph.rs) joins or observes the same generation-scoped
    // build instead of starting another inventory parse.
    let graph = Graph::open(&root_b);
    let g = std::sync::Arc::new(graph);

    let feed_g = std::sync::Arc::clone(&g);
    let feed_handle = std::thread::spawn(move || {
        let t = Instant::now();
        let loaded = feed_window_core(&feed_g);
        (ms(t.elapsed()), loaded)
    });
    let list_g = std::sync::Arc::clone(&g);
    let list_handle = std::thread::spawn(move || {
        let t = Instant::now();
        let pages = list_g.list_pages();
        (ms(t.elapsed()), pages.len())
    });
    let named_g = std::sync::Arc::clone(&g);
    let named_handle = std::thread::spawn(move || {
        let newest = named_g
            .journals_desc()
            .first()
            .cloned()
            .expect("the fixture has journals");
        let t = Instant::now();
        let page = named_g
            .load_named(&newest.name, PageKind::Journal)
            .expect("the newest journal must load")
            .expect("the newest journal must exist");
        (ms(t.elapsed()), page.name)
    });
    let templates_g = std::sync::Arc::clone(&g);
    let templates_handle = std::thread::spawn(move || {
        let t = Instant::now();
        let templates = templates_g.templates();
        (ms(t.elapsed()), templates.len())
    });
    // The 250 ms delayed background warm, exactly as `warm_cache_async`
    // schedules it after graph publication.
    let warm_g = std::sync::Arc::clone(&g);
    let warm_handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        let t = Instant::now();
        warm_g.warm_cache();
        ms(t.elapsed())
    });

    let (feed_ms, feed_n) = feed_handle.join().unwrap();
    let (list_ms, list_n) = list_handle.join().unwrap();
    let (named_ms, _) = named_handle.join().unwrap();
    let (templates_ms, templates_n) = templates_handle.join().unwrap();
    let warm_ms = warm_handle.join().unwrap();

    assert_eq!(feed_n, 3);
    assert_eq!(list_n, JOURNAL_COUNT + PAGE_COUNT);
    assert!(templates_n > 0);

    print_readings(
        "contended cold open (4 foreground surfaces + delayed background warm)",
        &[
            Reading {
                label: "journal_feed_page core",
                ms: feed_ms,
                detail: "SAME work as control row 1",
            },
            Reading {
                label: "list_pages",
                ms: list_ms,
                detail: "shared page-build flight",
            },
            Reading {
                label: "get_page (newest journal)",
                ms: named_ms,
                detail: "find_entry -> shared inventory under contention",
            },
            Reading {
                label: "list_templates",
                ms: templates_ms,
                detail: "page-build flight (owner or warm joiner)",
            },
            Reading {
                label: "background warm_cache",
                ms: warm_ms,
                detail: "paced whole-graph parse",
            },
        ],
    );

    // -- Steady state: after the warm installed the cache, every surface is a
    // cache hit. This is the experience AFTER the first contended open, and the
    // state the fix should reach on the FIRST open.
    let t = Instant::now();
    let feed_reloaded = feed_window_core(&g);
    let feed_warm = t.elapsed();
    assert_eq!(feed_reloaded, 3);
    let t = Instant::now();
    let _ = g.templates();
    let templates_warm = t.elapsed();
    print_readings(
        "steady state (cache installed)",
        &[
            Reading {
                label: "journal_feed_page core",
                ms: ms(feed_warm),
                detail: "cache hits",
            },
            Reading {
                label: "list_templates",
                ms: ms(templates_warm),
                detail: "cache hits",
            },
        ],
    );

    let _ = std::fs::remove_dir_all(&root_a);
    let _ = std::fs::remove_dir_all(&root_b);
}

// Keep the unused-variable lint quiet for the fixture tag plumbing.
#[allow(dead_code)]
fn _unused(_w: &mut dyn Write, _p: &Path) {}
