//! The B2 real-scale acceptance gate: run the whole crash matrix over a genuine
//! Logseq graph instead of a synthetic fixture.
//!
//! **Why this exists as a permanent, `#[ignore]`d test.** The project's corpus
//! discipline is that synthetic fixtures are generated from *our model of a
//! graph* — the same model that produced the bug — so a packet touching storage
//! or save takes a real graph as an acceptance gate. That gate is worth nothing
//! if it is a one-off script that vanishes with the packet: the next change to
//! the recovery record needs to re-run it, not reinvent it.
//!
//! **It contains no corpus content, and must never grow any.** The graph is
//! named by the `ANON_GRAPH` environment variable, copied to a scratch
//! directory, and never mutated in place. Every assertion message and every
//! `eprintln!` reports counts, indices and booleans only — never a page name,
//! path or byte. Keep it that way: this file is committed and public, the corpus
//! is not.
//!
//! Run it with:
//!
//! ```text
//! ANON_GRAPH=<graph> cargo test -p tine-core --lib direct_move_recovery_corpus -- --ignored --nocapture
//! ```
//!
//! The fast synthetic matrix that runs on every `cargo test` is
//! `direct_move_recovery_tests.rs`; this one is the slow tier that teaches it.

use crate::direct_move_recovery::{
    direct_move_durable_steps, recover_all, retire_if_terminal, DurableStep, RecoveryStore,
};
use crate::model::{Graph, PageDto};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn collect(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if let Ok(bytes) = fs::read(&path) {
            out.insert(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
                bytes,
            );
        }
    }
}

fn restore(root: &Path, baseline: &BTreeMap<String, Vec<u8>>, rels: &[String]) {
    for rel in rels {
        let path = root.join(rel);
        match baseline.get(rel) {
            Some(bytes) => {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, bytes).unwrap();
            }
            None => {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

fn rels_of(graph_root: &Path, dtos: &[&PageDto]) -> Vec<String> {
    let _ = graph_root;
    dtos.iter()
        .filter(|d| !d.path.is_empty())
        .map(|d| d.path.replace('\\', "/"))
        .collect()
}

#[test]
#[ignore]
fn anonymized_graph_cross_page_moves_converge() {
    let src = PathBuf::from(std::env::var("ANON_GRAPH").expect("ANON_GRAPH"));
    let work = std::env::temp_dir().join(format!("tine-b2-anon-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    let status = std::process::Command::new("cp")
        .arg("-a")
        .arg(&src)
        .arg(&work)
        .status()
        .unwrap();
    assert!(status.success());
    let store_root = work
        .parent()
        .unwrap()
        .join(format!("tine-b2-anon-store-{}", std::process::id()));
    let _ = fs::remove_dir_all(&store_root);
    fs::create_dir_all(&store_root).unwrap();

    let mut baseline = BTreeMap::new();
    collect(&work, &work, &mut baseline);
    eprintln!("corpus files: {}", baseline.len());

    // Deterministic candidate list: every markdown/org file under pages/ and
    // journals/, in sorted order. Names are never printed.
    let candidates: Vec<String> = baseline
        .keys()
        .filter(|k| {
            (k.starts_with("pages/") || k.starts_with("journals/"))
                && (k.ends_with(".md") || k.ends_with(".org"))
        })
        .cloned()
        .collect();
    eprintln!("candidate pages: {}", candidates.len());

    let mut pairs_run = 0usize;
    let mut cuts_run = 0usize;
    let mut skipped_no_record = 0usize;
    let mut skipped_empty = 0usize;
    let mut nonbyte_exact_roundtrip = 0usize;
    let mut org_pairs = 0usize;
    let mut crlf_pairs = 0usize;

    // Stride across the sorted list so pages and journals, md and org, are all
    // sampled, and pair each with the file 37 positions later (coprime stride).
    let stride = 17usize;
    let mut i = 0usize;
    while i + 1 < candidates.len() && pairs_run < 60 {
        let dest_rel = candidates[i].clone();
        let src_rel = candidates[(i + 37) % candidates.len()].clone();
        i += stride;
        if dest_rel == src_rel {
            continue;
        }

        let graph = Graph::open(&work);
        let load = |rel: &str| -> Option<PageDto> { graph.load_by_path(rel).ok().flatten() };
        let (Some(mut destination), Some(mut source)) = (load(&dest_rel), load(&src_rel)) else {
            skipped_empty += 1;
            continue;
        };
        if source.blocks.is_empty() || destination.read_only || source.read_only {
            skipped_empty += 1;
            continue;
        }
        let moved = source.blocks.remove(0);
        destination.blocks.push(moved);

        let participants = rels_of(&work, &[&destination, &source]);
        if participants.len() != 2 {
            skipped_empty += 1;
            continue;
        }
        if participants.iter().any(|p| p.ends_with(".org")) {
            org_pairs += 1;
        }
        if participants.iter().any(|p| {
            baseline
                .get(p)
                .map(|b| b.windows(2).any(|w| w == b"\r\n"))
                .unwrap_or(false)
        }) {
            crlf_pairs += 1;
        }

        // Oracle: the uncrashed move.
        let after = {
            graph
                .save_page(&destination, destination.rev.as_deref())
                .unwrap();
            graph.save_page(&source, source.rev.as_deref()).unwrap();
            let mut snap = BTreeMap::new();
            collect(&work, &work, &mut snap);
            snap
        };
        // Any file the move did not own must be byte-identical.
        let changed: Vec<&String> = baseline
            .keys()
            .filter(|rel| after.get(*rel) != baseline.get(*rel))
            .collect();
        let stray: Vec<&&String> = changed
            .iter()
            .filter(|r| !participants.contains(**r))
            .collect();
        if !stray.is_empty() {
            eprintln!(
                "pair {pairs_run}: changed={} participants={} stray={} \
                 dest_in_changed={} src_in_changed={} p0_is_dest={} p1_is_src={} \
                 stray_is_org={} stray_ext_md={}",
                changed.len(),
                participants.len(),
                stray.len(),
                changed.iter().any(|r| **r == dest_rel),
                changed.iter().any(|r| **r == src_rel),
                participants[0] == dest_rel,
                participants[1] == src_rel,
                stray.iter().any(|r| r.ends_with(".org")),
                stray.iter().all(|r| r.ends_with(".md")),
            );
            panic!("pair {pairs_run}: a bystander file changed");
        }
        restore(&work, &baseline, &participants);
        drop(graph);

        // A page that does not survive its own save byte-for-byte is a
        // pre-existing serializer observation, not a B2 defect; count it.
        {
            let graph = Graph::open(&work);
            if let Some(d) = graph.load_by_path(&dest_rel).ok().flatten() {
                let before = baseline.get(&participants[0]).cloned();
                graph.save_page(&d, d.rev.as_deref()).unwrap();
                if fs::read(work.join(&participants[0])).ok() != before {
                    nonbyte_exact_roundtrip += 1;
                }
            }
            restore(&work, &baseline, &participants);
        }

        let total_steps = 4; // commit, destination, source 0, retire
        for cut in 0..=total_steps {
            restore(&work, &baseline, &participants);
            let _ = fs::remove_dir_all(&store_root);
            fs::create_dir_all(&store_root).unwrap();

            let record = {
                let graph = Graph::open(&work);
                let Some(prepared) = graph
                    .prepare_direct_cross_page_move(&destination, std::slice::from_ref(&source))
                    .expect("record composes")
                else {
                    skipped_no_record += 1;
                    break;
                };
                let store = RecoveryStore::new(&store_root);
                for step in direct_move_durable_steps(&prepared.record)
                    .into_iter()
                    .take(cut)
                {
                    match step {
                        DurableStep::CommitRecord => store
                            .commit_record(&prepared.record, &prepared.images)
                            .unwrap(),
                        DurableStep::WriteDestination => {
                            graph
                                .save_page(&destination, destination.rev.as_deref())
                                .unwrap();
                        }
                        DurableStep::WriteSource(_) => {
                            graph.save_page(&source, source.rev.as_deref()).unwrap();
                        }
                        DurableStep::RetireRecord => {
                            assert!(retire_if_terminal(
                                &store_root,
                                &work,
                                &prepared.record.move_id
                            ));
                        }
                    }
                }
                prepared.record
            };
            let _ = record;

            recover_all(&store_root, &work);

            let mut now = BTreeMap::new();
            collect(&work, &work, &mut now);
            let converged = now == baseline || now == after;
            assert!(
                converged,
                "pair {pairs_run} cut {cut}: graph did not converge (names withheld)"
            );
            cuts_run += 1;
        }
        // Leave the corpus at baseline for the next pair.
        restore(&work, &baseline, &participants);
        pairs_run += 1;
    }

    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_dir_all(&store_root);
    eprintln!(
        "ANON GATE: pairs={pairs_run} cuts={cuts_run} skipped_no_record={skipped_no_record} \
         skipped_empty={skipped_empty} org_pairs={org_pairs} crlf_pairs={crlf_pairs} \
         non_byte_exact_save_roundtrip={nonbyte_exact_roundtrip}"
    );
    assert!(pairs_run >= 20, "not enough pairs exercised");
}

/// The same gate for 1 + 3 moves (the carry shape) at real scale.
#[test]
#[ignore]
fn anonymized_graph_multi_source_moves_converge() {
    let src = PathBuf::from(std::env::var("ANON_GRAPH").expect("ANON_GRAPH"));
    let work = std::env::temp_dir().join(format!("tine-b2-anon3-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    assert!(std::process::Command::new("cp")
        .arg("-a")
        .arg(&src)
        .arg(&work)
        .status()
        .unwrap()
        .success());
    let store_root = work
        .parent()
        .unwrap()
        .join(format!("tine-b2-anon3-store-{}", std::process::id()));
    let _ = fs::remove_dir_all(&store_root);
    fs::create_dir_all(&store_root).unwrap();

    let mut baseline = BTreeMap::new();
    collect(&work, &work, &mut baseline);
    let candidates: Vec<String> = baseline
        .keys()
        .filter(|k| {
            (k.starts_with("pages/") || k.starts_with("journals/"))
                && (k.ends_with(".md") || k.ends_with(".org"))
        })
        .cloned()
        .collect();

    let mut triples_run = 0usize;
    let mut cuts_run = 0usize;
    let mut i = 0usize;
    while i + 3 < candidates.len() && triples_run < 12 {
        let dest_rel = candidates[i].clone();
        let source_rels: Vec<String> = (1..=3)
            .map(|k| candidates[(i + k * 53) % candidates.len()].clone())
            .collect();
        i += 83;
        let mut all = vec![dest_rel.clone()];
        all.extend(source_rels.iter().cloned());
        all.sort();
        all.dedup();
        if all.len() != 4 {
            continue;
        }

        let graph = Graph::open(&work);
        let Some(mut destination) = graph.load_by_path(&dest_rel).ok().flatten() else {
            continue;
        };
        let mut sources = Vec::new();
        let mut ok = !destination.read_only;
        for rel in &source_rels {
            match graph.load_by_path(rel).ok().flatten() {
                Some(mut dto) if !dto.blocks.is_empty() && !dto.read_only => {
                    destination.blocks.push(dto.blocks.remove(0));
                    sources.push(dto);
                }
                _ => ok = false,
            }
        }
        if !ok || sources.len() != 3 {
            drop(graph);
            restore(&work, &baseline, &all);
            continue;
        }
        let mut participants = vec![destination.path.replace('\\', "/")];
        participants.extend(sources.iter().map(|s| s.path.replace('\\', "/")));

        let after = {
            graph
                .save_page(&destination, destination.rev.as_deref())
                .unwrap();
            for s in &sources {
                graph.save_page(s, s.rev.as_deref()).unwrap();
            }
            let mut snap = BTreeMap::new();
            collect(&work, &work, &mut snap);
            snap
        };
        let stray = baseline
            .keys()
            .filter(|rel| after.get(*rel) != baseline.get(*rel))
            .filter(|rel| !participants.contains(rel))
            .count();
        assert_eq!(stray, 0, "triple {triples_run}: a bystander file changed");
        drop(graph);
        restore(&work, &baseline, &participants);

        for cut in 0..=6usize {
            restore(&work, &baseline, &participants);
            let _ = fs::remove_dir_all(&store_root);
            fs::create_dir_all(&store_root).unwrap();
            {
                let graph = Graph::open(&work);
                let prepared = graph
                    .prepare_direct_cross_page_move(&destination, &sources)
                    .expect("record composes")
                    .expect("multi-source move needs a record");
                let store = RecoveryStore::new(&store_root);
                for step in direct_move_durable_steps(&prepared.record)
                    .into_iter()
                    .take(cut)
                {
                    match step {
                        DurableStep::CommitRecord => store
                            .commit_record(&prepared.record, &prepared.images)
                            .unwrap(),
                        DurableStep::WriteDestination => {
                            graph
                                .save_page(&destination, destination.rev.as_deref())
                                .unwrap();
                        }
                        DurableStep::WriteSource(index) => {
                            let s = &sources[index];
                            graph.save_page(s, s.rev.as_deref()).unwrap();
                        }
                        DurableStep::RetireRecord => {
                            assert!(retire_if_terminal(
                                &store_root,
                                &work,
                                &prepared.record.move_id
                            ));
                        }
                    }
                }
            }
            recover_all(&store_root, &work);
            let mut now = BTreeMap::new();
            collect(&work, &work, &mut now);
            assert!(
                now == baseline || now == after,
                "triple {triples_run} cut {cut}: no convergence"
            );
            cuts_run += 1;
        }
        restore(&work, &baseline, &participants);
        triples_run += 1;
    }

    let _ = fs::remove_dir_all(&work);
    let _ = fs::remove_dir_all(&store_root);
    eprintln!("ANON GATE (1+3): triples={triples_run} cuts={cuts_run}");
    assert!(triples_run >= 8);
}
