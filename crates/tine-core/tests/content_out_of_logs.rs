#[path = "support/production_source.rs"]
mod production_source;

use production_source::{
    compiled_source, line_of, production_source_files, relative_path, repo_root,
};
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PrintSite {
    file: String,
    line: usize,
    macro_name: String,
}

#[derive(Clone, Copy)]
struct AllowedSite {
    file: &'static str,
    lines: &'static [usize],
    macro_name: &'static str,
    class: &'static str,
    why: &'static str,
    gate: &'static str,
}

// Production app output only. Standalone CLI binaries own their terminal
// output and are outside the application diagnostics contract.
const RUST_PRINT_SITE_COUNT: usize = 74;
const ALLOWLIST: &[AllowedSite] = &[
    AllowedSite { file: "crates/tine-core/src/concord_ledger.rs", lines: &[234], macro_name: "eprintln", class: "content-free-error", why: "best-effort ledger failure carries only its I/O error", gate: "always-on reviewed failure" },
    AllowedSite { file: "crates/tine-core/src/direct_projection.rs", lines: &[821, 839, 850, 893], macro_name: "eprintln", class: "content-free-error", why: "projection availability failures carry only typed storage errors", gate: "always-on reviewed failure" },
    AllowedSite { file: "crates/tine-core/src/model.rs", lines: &[14448], macro_name: "eprintln", class: "numeric-shape", why: "isolated search worker panic reports only a worker number", gate: "always-on reviewed failure" },
    AllowedSite { file: "crates/tine-core/src/model.rs", lines: &[19918, 22802], macro_name: "eprintln", class: "content-free-debug", why: "reconcile and isolated-parse failures contain no path, title, content, or raw error", gate: "runtime_debug_diagnostics_enabled" },
    AllowedSite { file: "crates/tine-core/src/model.rs", lines: &[21669, 21858], macro_name: "eprintln", class: "fixed-debug", why: "Guide-page refusal messages are fixed literals", gate: "cfg(debug_assertions)" },
    AllowedSite { file: "crates/tine-core/src/oplog/batch.rs", lines: &[205], macro_name: "eprintln", class: "numeric-trace", why: "batch composition contains public enum kinds and numeric sizes only", gate: "TINE_BATCH_TRACE" },
    AllowedSite { file: "crates/tine-core/src/oplog/checkpoint_generation.rs", lines: &[999, 1046], macro_name: "eprintln", class: "content-free-error", why: "checkpoint writer failures carry only their I/O error", gate: "always-on reviewed failure" },
    AllowedSite { file: "crates/tine-core/src/oplog/hot_engine.rs", lines: &[8811], macro_name: "eprintln", class: "content-free-error", why: "A5c-owned checkpoint skip reports only a typed error", gate: "always-on; forbidden to I5" },
    AllowedSite { file: "crates/tine-core/src/oplog/hot_engine.rs", lines: &[10864, 14409, 15425, 15453, 15632, 18174, 18203, 18237, 18269, 19126, 19152, 19290, 19309, 21695, 24353], macro_name: "eprintln", class: "directed-core-trace", why: "engine diagnostics run only under explicit performance, CRDT, or activation trace flags", gate: "TINE_PHASE_TRACE/TINE_CRDT_TRACE/TINE_ACTIVATION_TRACE" },
    AllowedSite { file: "crates/tine-core/src/oplog/import.rs", lines: &[1712, 1725], macro_name: "eprintln", class: "fixed-debug", why: "clean-genesis recovery reports one of two fixed states", gate: "TINE_DEBUG" },
    AllowedSite { file: "crates/tine-core/src/oplog/local_journal_drain.rs", lines: &[846, 873, 889, 914], macro_name: "eprintln", class: "numeric-trace", why: "managed-local drain timings contain fixed labels and durations", gate: "TINE_PHASE_TRACE" },
    AllowedSite { file: "crates/tine-core/src/oplog/object_store.rs", lines: &[1888], macro_name: "eprintln", class: "enum-trace", why: "immutable publication reports only a fixed artifact class", gate: "TINE_PUBLISH_TRACE" },
    AllowedSite { file: "crates/tine-core/src/oplog/projection.rs", lines: &[2854, 2959, 3149, 3193], macro_name: "eprintln", class: "directed-core-trace", why: "projection diagnostics are available only for an explicitly directed phase trace", gate: "TINE_PHASE_TRACE" },
    AllowedSite { file: "crates/tine-core/src/oplog/semantic.rs", lines: &[935], macro_name: "eprintln", class: "numeric-trace", why: "semantic snapshot diagnostic contains counts and encoded byte sizes", gate: "TINE_SEMANTIC_TRACE" },
    AllowedSite { file: "crates/tine-core/src/oplog/sqlite.rs", lines: &[1893, 4501, 4505, 4512, 4525, 4537, 4545, 4557, 5317], macro_name: "eprintln", class: "directed-core-trace", why: "SQLite construction diagnostics run only under explicit trace flags", gate: "TINE_PHASE_TRACE/TINE_TERMINAL_TRACE" },
    AllowedSite { file: "crates/tine-core/src/publish.rs", lines: &[4400, 4430], macro_name: "eprintln", class: "content-free-debug", why: "publication refusals report only a fixed shape or collision count", gate: "runtime_debug_diagnostics_enabled" },
    AllowedSite { file: "crates/tine-core/src/sync_runtime.rs", lines: &[6308], macro_name: "eprintln", class: "directed-core-trace", why: "watcher trace is explicitly enabled for a directed investigation", gate: "TINE_CLEAN_WATCHER_TRACE" },
    AllowedSite { file: "crates/tine-core/src/sync_runtime.rs", lines: &[7083, 7101], macro_name: "eprintln", class: "numeric-debug", why: "clean-open stage and counter reports contain fixed names and numeric measurements", gate: "runtime_debug_diagnostics_enabled" },
    AllowedSite { file: "crates/tine-core/src/sync_runtime.rs", lines: &[7273, 7277, 7303, 7315], macro_name: "eprintln", class: "content-free-error", why: "disposable checkpoint fallback carries only typed storage errors", gate: "always-on reviewed failure" },
    AllowedSite { file: "crates/tine-core/src/sync_runtime.rs", lines: &[7447, 20907, 21168, 22018, 22058, 22090], macro_name: "eprintln", class: "content-free-debug", why: "receipt, pending-projection, and conflict-resolution reports contain only counts or fixed states", gate: "runtime_debug_diagnostics_enabled" },
    AllowedSite { file: "crates/tine-core/src/sync_runtime.rs", lines: &[20578, 20669, 20735], macro_name: "eprintln", class: "directed-core-trace", why: "foreground mutation detail is available only for explicitly enabled runtime debugging", gate: "runtime_debug_diagnostics_enabled" },
    AllowedSite { file: "crates/tine-core/src/sync_runtime.rs", lines: &[21550], macro_name: "eprintln", class: "numeric-trace", why: "actor tick report contains a fixed branch label, duration, and pending count", gate: "TINE_TICK_TRACE" },
    AllowedSite { file: "src-tauri/src/data_home.rs", lines: &[128], macro_name: "eprintln", class: "fixed-terminal-failure", why: "fatal startup guidance is a fixed literal with no path or raw error", gate: "always-on fatal startup" },
    AllowedSite { file: "src-tauri/src/debug.rs", lines: &[71, 75, 89], macro_name: "eprintln", class: "directed-native-debug", why: "detailed native stderr is available only under the existing debug opt-in", gate: "debug_enabled" },
    AllowedSite { file: "src-tauri/src/debug.rs", lines: &[347], macro_name: "eprintln", class: "content-free-error", why: "flight-recorder setup failure carries only its I/O error", gate: "always-on reviewed failure" },
];

fn print_sites() -> Vec<PrintSite> {
    let root = repo_root();
    let files = production_source_files();
    let print_macro = Regex::new(r"\b(eprintln|println|dbg)!\s*[({\[]").unwrap();
    let mut sites = Vec::new();
    for file in files {
        let source = compiled_source(&file);
        let relative = relative_path(&root, &file);
        for found in print_macro.captures_iter(&source) {
            let whole = found.get(0).unwrap();
            if source[..whole.start()]
                .rsplit_once('\n')
                .map_or(&source[..whole.start()], |(_, line)| line)
                .trim_start()
                .starts_with("//")
            {
                continue;
            }
            sites.push(PrintSite {
                file: relative.clone(),
                line: line_of(&source, whole.start()),
                macro_name: found[1].to_string(),
            });
        }
    }
    sites.sort();
    sites
}

#[test]
fn production_print_sites_equal_the_reviewed_content_free_census() {
    for entry in ALLOWLIST {
        assert!(!entry.class.is_empty(), "every print site needs a class");
        assert!(!entry.why.is_empty(), "every print site needs a reason");
        assert!(
            !entry.gate.is_empty(),
            "every print site needs an explicit gate"
        );
    }
    let actual = print_sites();
    let mut expected = ALLOWLIST
        .iter()
        .flat_map(|entry| {
            entry.lines.iter().map(|line| PrintSite {
                file: entry.file.to_string(),
                line: *line,
                macro_name: entry.macro_name.to_string(),
            })
        })
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(expected.len(), RUST_PRINT_SITE_COUNT);
    assert_eq!(
        actual, expected,
        "I-5: production print-site census changed. \
         If a print site was ADDED or REMOVED: remove user content and use a fixed-shape event \
         (src-tauri — `debug.rs::record_fixed_event` and its typed callers such as \
         `record_storage_transition` are the exemplar) or a content-free flag-gated line (core), \
         then classify the exact site by hand in ALLOWLIST. \
         If the (file, macro) multiset is UNCHANGED and only line numbers moved, this is pure \
         drift from an edit above a print site: run `node scripts/reanchor-print-census.mjs`, \
         which re-anchors line numbers only and refuses to bless an added or removed site."
    );
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else if entry.file_type().unwrap().is_file() {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn corpus_page_names(root: &Path, names: &mut Vec<String>) {
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            corpus_page_names(&entry.path(), names);
            continue;
        }
        if !entry.file_type().unwrap().is_file()
            || !matches!(
                entry.path().extension().and_then(|value| value.to_str()),
                Some("md" | "markdown" | "org")
            )
        {
            continue;
        }
        if let Some(name) = entry.path().file_stem().and_then(|value| value.to_str()) {
            if !name.is_empty() {
                names.push(name.to_owned());
            }
        }
    }
}

#[test]
#[ignore = "child process for the real-corpus stderr probe"]
fn real_corpus_open_save_publish_child() {
    let source = PathBuf::from(std::env::var_os("TINE_REAL_GRAPH").expect("TINE_REAL_GRAPH"));
    let scratch = std::env::temp_dir().join(format!(
        "tine-i5-content-out-of-logs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    copy_tree(&source, &scratch);
    fs::create_dir_all(scratch.join("pages")).unwrap();
    fs::write(
        scratch.join("pages/I5 Diagnostics Probe.md"),
        "public:: true\n\n- fixed-shape diagnostics probe\n",
    )
    .unwrap();

    let graph = tine_core::model::Graph::open_checked(&scratch).unwrap();
    graph.warm_cache();
    let mut page = graph
        .load_by_path("pages/I5 Diagnostics Probe.md")
        .unwrap()
        .unwrap();
    page.blocks[0].raw.push_str(" updated");
    let baseline = page.rev.clone();
    graph.save_page(&page, baseline.as_deref()).unwrap();
    let (_, published) = graph.publish_html().unwrap();
    assert!(published > 0, "real-corpus publish produced no public page");
    fs::remove_dir_all(scratch).unwrap();
}

#[test]
#[ignore = "manual real-corpus gate: set TINE_REAL_GRAPH"]
fn real_corpus_open_save_publish_emits_no_page_name_with_debug_disabled() {
    let source = PathBuf::from(std::env::var_os("TINE_REAL_GRAPH").expect("TINE_REAL_GRAPH"));
    let mut names = Vec::new();
    corpus_page_names(&source, &mut names);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "real_corpus_open_save_publish_child",
            "--nocapture",
        ])
        .env("TINE_REAL_GRAPH", &source)
        .env_remove("TINE_DEBUG")
        .output()
        .unwrap();
    assert!(output.status.success(), "real-corpus child failed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let matches = names
        .iter()
        .filter(|name| stderr.contains(name.as_str()))
        .count();
    println!(
        "I5_REAL_CORPUS files={} stderr_bytes={} page_name_matches={matches}",
        names.len(),
        stderr.len()
    );
    assert_eq!(
        matches, 0,
        "captured stderr contained {matches} corpus page-name matches"
    );
}
