//! Corpus parity exporter for the query-engine campaign (SPEC §8, dossier O8).
//!
//! Runs every `{{query …}}` found in the given graph directories through the
//! in-memory walk and prints one deterministic line per query:
//!
//! ```text
//! <graph>\t<page>\t<query-hash>\t<anchor>\t<total>\t<exceeded>\t<id,id,…>
//! ```
//!
//! Only identifiers are printed — never block or query text — so the output of a
//! private graph can be diffed and pasted into a receipt. The query text itself
//! is reduced to a stable 16-hex-digit FNV-1a hash.
//!
//! Usage:
//! `python3 scripts/query-oracle-dump.py walk --output /tmp/walk.tsv -- <graph-dir>…`
//!
//! A graph directory that does not exist is reported as `# skipped <label>` on
//! TSV output rather than failing, so the same command works with and without
//! the anonymized graph.

use crate as tine_core;
use std::io::Write;
use std::path::PathBuf;

use tine_core::query::run_query_bounded;
use tine_core::Graph;

#[path = "oracle_corpus_tests.rs"]
mod query_corpus;

use query_corpus::{fnv1a, graph_files, macro_args, materialize_single_file};

#[test]
#[ignore = "explicit corpus oracle; use scripts/query-oracle-dump.py"]
fn dump() {
    let (args, mut output) = query_corpus::dump_inputs();
    if args.is_empty() {
        eprintln!("usage: query-walk-dump <graph-dir>…");
        std::process::exit(2);
    }
    for arg in args {
        let root = PathBuf::from(&arg);
        let label = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(arg.as_str())
            .to_string();
        let (root, scratch) = materialize_single_file(&root);
        if !root.is_dir() {
            writeln!(output, "# skipped {label} (absent)").unwrap();
            continue;
        }
        let graph = Graph::open(&root);
        let mut rows: Vec<String> = Vec::new();
        for file in graph_files(&root) {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let page = file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let mut queries = macro_args(&text, "query");
            queries.extend(macro_args(&text, "tine-query"));
            for source in queries {
                let bounded = run_query_bounded(&graph, &source, usize::MAX, usize::MAX);
                let mut ids: Vec<String> = Vec::new();
                for group in &bounded.groups {
                    for block in &group.blocks {
                        ids.push(format!("{}#{}", group.page, block.id));
                    }
                }
                rows.push(format!(
                    "{label}\t{page}\t{:016x}\tblock\t{}\t{}\t{}",
                    fnv1a(source.as_bytes()),
                    bounded.total,
                    bounded.exceeded,
                    ids.join(","),
                ));
            }
        }
        rows.sort();
        if rows.is_empty() {
            writeln!(output, "# no queries in {label}").unwrap();
        }
        for row in rows {
            writeln!(output, "{row}").unwrap();
        }
        if let Some(dir) = scratch {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
