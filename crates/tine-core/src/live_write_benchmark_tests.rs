//! Manual pre-P5 live managed-write benchmark.
//!
//! This module is test-only and is registered from `sync_runtime_tests.rs`.

use super::*;
use crate::oplog::hot_engine::{
    arm_live_write_cold_target, live_write_benchmark_counters, LiveWriteBenchmarkCounters,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SOURCE_PATH: &str = "pages/Tine Live Write 200 Source.md";
const DESTINATION_PATH: &str = "pages/Tine Live Write 200 Destination.md";
const SUBTREE_BLOCKS: usize = 200;

#[derive(Clone, Debug)]
struct SubtreeTarget {
    path: String,
    block_path: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
struct CorpusCensus {
    files: usize,
    markdown_org: usize,
    logical_bytes: u64,
    relative_file_hash_stream_sha256: String,
}

#[derive(Debug)]
struct ActivatedCensusAndTarget {
    inventory_pages: usize,
    parsed_pages: usize,
    parsed_blocks: usize,
    edit_path: String,
    edit_block_path: Vec<usize>,
    edit_identity: String,
    authoritative_home_document_id: String,
}

#[derive(Clone, Copy, Debug, serde::Serialize)]
struct PhaseObservation {
    elapsed_ns: u128,
    rss_before_kib: usize,
    rss_after_kib: usize,
    sampled_peak_rss_kib: usize,
    counters: LiveWriteBenchmarkCounters,
}

struct PhaseMeter {
    started: Instant,
    counters_before: LiveWriteBenchmarkCounters,
    rss_before_kib: usize,
    peak: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    sampler: Option<std::thread::JoinHandle<()>>,
}

impl PhaseMeter {
    fn start() -> Self {
        let rss_before_kib = process_rss_kib();
        let peak = Arc::new(AtomicUsize::new(rss_before_kib));
        let stop = Arc::new(AtomicBool::new(false));
        let sampler_peak = Arc::clone(&peak);
        let sampler_stop = Arc::clone(&stop);
        let sampler = std::thread::Builder::new()
            .name("live-write-rss-sampler".into())
            .spawn(move || {
                while !sampler_stop.load(Ordering::Relaxed) {
                    sampler_peak.fetch_max(process_rss_kib(), Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(1));
                }
                sampler_peak.fetch_max(process_rss_kib(), Ordering::Relaxed);
            })
            .unwrap();
        Self {
            started: Instant::now(),
            counters_before: live_write_benchmark_counters(),
            rss_before_kib,
            peak,
            stop,
            sampler: Some(sampler),
        }
    }

    fn finish(mut self) -> PhaseObservation {
        let elapsed_ns = self.started.elapsed().as_nanos();
        let counters = live_write_benchmark_counters().since(self.counters_before);
        let rss_after_kib = process_rss_kib();
        self.peak.fetch_max(rss_after_kib, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
        self.sampler.take().unwrap().join().unwrap();
        PhaseObservation {
            elapsed_ns,
            rss_before_kib: self.rss_before_kib,
            rss_after_kib,
            sampled_peak_rss_kib: self.peak.load(Ordering::Relaxed),
            counters,
        }
    }
}

fn measure<T>(operation: impl FnOnce() -> T) -> (T, PhaseObservation) {
    let meter = PhaseMeter::start();
    let result = operation();
    (result, meter.finish())
}

fn process_rss_kib() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|value| value.parse().ok())
        })
        .expect("/proc/self/status reports VmRSS")
}

fn emit(
    arm: &str,
    trial: usize,
    scope: &str,
    phase: &str,
    sample: Option<usize>,
    observation: PhaseObservation,
    detail: serde_json::Value,
) {
    println!(
        "LIVE_WRITE_JSON {}",
        serde_json::json!({
            "arm": arm,
            "trial": trial,
            "scope": scope,
            "phase": phase,
            "sample": sample,
            "elapsed_ns": observation.elapsed_ns,
            "rss_before_kib": observation.rss_before_kib,
            "rss_after_kib": observation.rss_after_kib,
            "sampled_peak_rss_kib": observation.sampled_peak_rss_kib,
            "counters": observation.counters,
            "detail": detail,
        })
    );
}

fn text_paths(root: &Path) -> Vec<String> {
    fn visit(root: &Path, directory: &Path, paths: &mut Vec<String>) {
        let mut entries = std::fs::read_dir(directory)
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type().unwrap();
            assert!(!file_type.is_symlink(), "corpus copy contains a symlink");
            if file_type.is_dir() {
                visit(root, &path, paths);
            } else if matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("md" | "org")
            ) {
                paths.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut paths = Vec::new();
    visit(root, root, &mut paths);
    paths.sort();
    paths
}

/// Hash every regular corpus file as the sorted byte stream emitted by
/// `sha256sum`: `<content sha256><two spaces><relative path><newline>`.
fn corpus_census(root: &Path) -> CorpusCensus {
    fn visit(root: &Path, directory: &Path, files: &mut Vec<(String, Vec<u8>)>) {
        let mut entries = std::fs::read_dir(directory)
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        entries.sort_unstable_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type().unwrap();
            assert!(!file_type.is_symlink(), "corpus contains a symlink");
            if file_type.is_dir() {
                visit(root, &path, files);
            } else if file_type.is_file() {
                files.push((
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    std::fs::read(path).unwrap(),
                ));
            }
        }
    }

    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let markdown_org = files
        .iter()
        .filter(|(path, _)| path.ends_with(".md") || path.ends_with(".org"))
        .count();
    let logical_bytes = files.iter().map(|(_, bytes)| bytes.len() as u64).sum();
    let mut stream = Sha256::new();
    for (path, bytes) in &files {
        let digest = format!("{:x}", Sha256::digest(bytes));
        stream.update(digest.as_bytes());
        stream.update(b"  ");
        stream.update(path.as_bytes());
        stream.update(b"\n");
    }
    CorpusCensus {
        files: files.len(),
        markdown_org,
        logical_bytes,
        relative_file_hash_stream_sha256: format!("{:x}", stream.finalize()),
    }
}

fn subtree_size(block: &BlockDto) -> usize {
    1 + block.children.iter().map(subtree_size).sum::<usize>()
}

fn page_block_count(page: &PageDto) -> usize {
    page.blocks.iter().map(subtree_size).sum()
}

fn find_exact_subtree(blocks: &[BlockDto], path: &mut Vec<usize>) -> Option<Vec<usize>> {
    for (index, block) in blocks.iter().enumerate() {
        path.push(index);
        if subtree_size(block) == SUBTREE_BLOCKS {
            return Some(path.clone());
        }
        if let Some(found) = find_exact_subtree(&block.children, path) {
            return Some(found);
        }
        path.pop();
    }
    None
}

fn block_at<'a>(blocks: &'a [BlockDto], path: &[usize]) -> &'a BlockDto {
    let (first, rest) = path.split_first().expect("block path is non-empty");
    let block = &blocks[*first];
    if rest.is_empty() {
        block
    } else {
        block_at(&block.children, rest)
    }
}

fn block_at_mut<'a>(blocks: &'a mut [BlockDto], path: &[usize]) -> &'a mut BlockDto {
    let (first, rest) = path.split_first().expect("block path is non-empty");
    let block = &mut blocks[*first];
    if rest.is_empty() {
        block
    } else {
        block_at_mut(&mut block.children, rest)
    }
}

fn find_sparse_block_path(blocks: &[BlockDto], path: &mut Vec<usize>) -> Option<Vec<usize>> {
    for (index, block) in blocks.iter().enumerate() {
        path.push(index);
        if block.id.starts_with(SYNC_APPLICATION_INTERNAL_BLOCK_PREFIX) {
            return Some(path.clone());
        }
        if let Some(found) = find_sparse_block_path(&block.children, path) {
            return Some(found);
        }
        path.pop();
    }
    None
}

fn page_contains_id(blocks: &[BlockDto], identity: &str) -> bool {
    blocks
        .iter()
        .any(|block| block.id == identity || page_contains_id(&block.children, identity))
}

fn write_benchmark_pages(graph_root: &Path, add_subtree: bool) {
    let pages = graph_root.join("pages");
    std::fs::create_dir_all(&pages).unwrap();
    if add_subtree {
        let mut source = String::from("- live write exact 200 root\n");
        for index in 0..(SUBTREE_BLOCKS - 1) {
            source.push_str(&format!("\t- live write child {index:03}\n"));
        }
        std::fs::write(graph_root.join(SOURCE_PATH), source).unwrap();
    }
    std::fs::write(
        graph_root.join(DESTINATION_PATH),
        "- live write destination sentinel\n",
    )
    .unwrap();
}

fn prepare_metadata(graph_root: &Path) -> (SubtreeTarget, Vec<String>, bool) {
    let paths = text_paths(graph_root);
    let graph = Graph::open(graph_root);
    let mut exact = None;
    let mut any_exact = false;
    for path in &paths {
        let Some(page) = graph.load_by_path(path).unwrap() else {
            continue;
        };
        let found = find_exact_subtree(&page.blocks, &mut Vec::new());
        if found.is_some() {
            any_exact = true;
        }
        if exact.is_none()
            && !page.read_only
            && page_block_count(&page) <= MAX_SYNC_APPLICATION_PAGE_BLOCKS
        {
            exact = found.map(|block_path| SubtreeTarget {
                path: path.clone(),
                block_path,
            });
        }
    }
    drop(graph);
    if any_exact && exact.is_none() {
        panic!("the corpus has an exact-200 subtree, but none is writable through the application boundary");
    }
    let transformed = !any_exact;
    write_benchmark_pages(graph_root, transformed);
    let paths = text_paths(graph_root);
    let target = if let Some(target) = exact {
        target
    } else {
        let graph = Graph::open(graph_root);
        let page = graph
            .load_by_path(SOURCE_PATH)
            .unwrap()
            .expect("deterministic exact-200 page parses");
        assert_eq!(page_block_count(&page), SUBTREE_BLOCKS);
        assert_eq!(subtree_size(&page.blocks[0]), SUBTREE_BLOCKS);
        SubtreeTarget {
            path: SOURCE_PATH.into(),
            block_path: vec![0],
        }
    };
    (target, paths, transformed)
}

fn authoritative_block_home(
    handle: &SyncRuntimeHandle,
    page: &PageDto,
    edit_identity: &str,
) -> String {
    let resolved = handle
        .query(SyncRuntimeQueryRequest::ResolvePage {
            path: page.path.clone(),
            name: page.name.clone(),
            page_kind: page.kind.into(),
        })
        .unwrap();
    let SyncRuntimeQueryReply::Page(Some(page_row)) = resolved else {
        panic!("authoritative exact-path page query did not resolve");
    };
    let loaded = handle
        .query(SyncRuntimeQueryRequest::LoadPage {
            page_id: page_row.page_id,
            block_limit: MAX_SYNC_APPLICATION_PAGE_BLOCKS,
        })
        .unwrap();
    let SyncRuntimeQueryReply::PageWithBlocks(Some(page_with_blocks)) = loaded else {
        panic!("authoritative block-home query did not load the page");
    };
    let internal_id = edit_identity
        .strip_prefix(SYNC_APPLICATION_INTERNAL_BLOCK_PREFIX)
        .expect("chosen edit block has an internal managed identity");
    let wanted = Uuid::parse_str(internal_id).unwrap();
    page_with_blocks
        .blocks
        .into_iter()
        .find(|block| Uuid::parse_str(&block.block_id).ok() == Some(wanted))
        .map(|block| block.home_document_id)
        .expect("authoritative page row contains the selected block")
}

fn activated_census_and_target(
    handle: &SyncRuntimeHandle,
    paths: &[String],
    excluded: &[&str],
) -> ActivatedCensusAndTarget {
    let inventory_pages = match handle.application_page_inventory().unwrap() {
        SyncApplicationPageInventoryOutcome::Loaded { pages } => pages.len(),
        SyncApplicationPageInventoryOutcome::Deferred { state } => {
            panic!("activated inventory unexpectedly deferred: {state:?}")
        }
    };
    let mut parsed_pages = 0;
    let mut parsed_blocks = 0;
    let mut target = None;
    for path in paths {
        let outcome = handle
            .load_application_page(SyncApplicationPageLoadRequest {
                page: SyncApplicationPageSelector::ExactPath { path: path.clone() },
            })
            .unwrap();
        let page = match outcome {
            SyncApplicationPageLoadOutcome::Loaded { page, .. } => page,
            other => panic!("activated exact-path page did not load: {path}: {other:?}"),
        };
        parsed_pages += 1;
        parsed_blocks += page_block_count(&page);
        if target.is_none()
            && path.starts_with("pages/")
            && !excluded.contains(&path.as_str())
            && !page.read_only
        {
            if let Some(block_path) = find_sparse_block_path(&page.blocks, &mut Vec::new()) {
                let identity = block_at(&page.blocks, &block_path).id.clone();
                let authoritative_home_document_id =
                    authoritative_block_home(handle, &page, &identity);
                target = Some((
                    path.clone(),
                    block_path,
                    identity,
                    authoritative_home_document_id,
                ));
            }
        }
    }
    let (edit_path, edit_block_path, edit_identity, authoritative_home_document_id) =
        target.expect("the corpus copy has no writable page with an import-owned block");
    assert_eq!(inventory_pages, parsed_pages);
    ActivatedCensusAndTarget {
        inventory_pages,
        parsed_pages,
        parsed_blocks,
        edit_path,
        edit_block_path,
        edit_identity,
        authoritative_home_document_id,
    }
}

fn save_outcome_name(outcome: &SyncApplicationPageSaveOutcome) -> &'static str {
    match outcome {
        SyncApplicationPageSaveOutcome::Prepared => "prepared",
        SyncApplicationPageSaveOutcome::Saved { .. } => "saved",
        SyncApplicationPageSaveOutcome::Unchanged { .. } => "unchanged",
        SyncApplicationPageSaveOutcome::Conflict { .. } => "conflict",
        SyncApplicationPageSaveOutcome::Deferred { .. } => "deferred",
    }
}

fn move_outcome_name(outcome: &SyncApplicationMoveSubtreesOutcome) -> &'static str {
    match outcome {
        SyncApplicationMoveSubtreesOutcome::Committed { .. } => "committed",
        SyncApplicationMoveSubtreesOutcome::NoCommit { .. } => "no_commit",
        SyncApplicationMoveSubtreesOutcome::Deferred { .. } => "deferred",
    }
}

fn drain_projection(handle: &SyncRuntimeHandle) {
    for _ in 0..4096 {
        if handle.status().unwrap().managed_local_pending == 0 {
            return;
        }
        handle.tick().unwrap();
    }
    panic!("projection did not settle within 4096 actor turns");
}

fn finish_save(
    handle: &SyncRuntimeHandle,
    path: &str,
    outcome: SyncApplicationPageSaveOutcome,
) -> (PageDto, String) {
    drain_projection(handle);
    match outcome {
        SyncApplicationPageSaveOutcome::Saved { page, revision, .. } => (page, revision),
        SyncApplicationPageSaveOutcome::Deferred { .. } => load_application_exact(handle, path),
        other => panic!("application save did not commit: {other:?}"),
    }
}

fn finish_move(
    handle: &SyncRuntimeHandle,
    request: &SyncApplicationMoveSubtreesRequest,
    mut outcome: SyncApplicationMoveSubtreesOutcome,
) -> SyncApplicationMoveSubtreesOutcome {
    for _ in 0..MAX_EDITOR_SETTLE_TURNS {
        match outcome {
            committed @ SyncApplicationMoveSubtreesOutcome::Committed { .. } => {
                drain_projection(handle);
                return committed;
            }
            SyncApplicationMoveSubtreesOutcome::Deferred { .. } => {
                handle.tick().unwrap();
                outcome = handle.move_application_subtrees(request.clone()).unwrap();
            }
            other => panic!("application move did not commit: {other:?}"),
        }
    }
    panic!("application move did not settle within the bounded editor budget");
}

#[test]
#[ignore = "manual release-only pre-P5 managed live-write benchmark"]
fn managed_live_write_pre_p5_release_benchmark() {
    assert!(
        !cfg!(debug_assertions),
        "live-write headline numbers require --release"
    );
    let arm = std::env::var("TINE_LIVE_WRITE_ARM").expect("TINE_LIVE_WRITE_ARM");
    let trial = std::env::var("TINE_LIVE_WRITE_TRIAL")
        .expect("TINE_LIVE_WRITE_TRIAL")
        .parse::<usize>()
        .unwrap();
    let corpus =
        PathBuf::from(std::env::var("TINE_LIVE_WRITE_CORPUS").expect("TINE_LIVE_WRITE_CORPUS"));
    let run_root =
        PathBuf::from(std::env::var("TINE_LIVE_WRITE_RUN_ROOT").expect("TINE_LIVE_WRITE_RUN_ROOT"));
    let repository_root =
        std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
    assert!(run_root.starts_with(repository_root));
    assert!(!run_root.exists(), "trial root must be fresh");
    std::fs::create_dir_all(&run_root).unwrap();

    let (source_census, source_census_observation) = measure(|| corpus_census(&corpus));
    assert_eq!(source_census.files, 1046);
    assert_eq!(source_census.markdown_org, 1045);
    assert_eq!(source_census.logical_bytes, 1_286_352);
    assert_eq!(
        source_census.relative_file_hash_stream_sha256,
        "782c550b5557c072c2595a2ffdf646792ceb2e79eb398657d9366142f458afad"
    );
    emit(
        &arm,
        trial,
        "setup",
        "source_corpus_census",
        None,
        source_census_observation,
        serde_json::to_value(&source_census).unwrap(),
    );

    let (_, copy_observation) = measure(|| copy_provider_tree(&corpus, &run_root.join("graph")));
    emit(
        &arm,
        trial,
        "setup",
        "corpus_copy",
        None,
        copy_observation,
        serde_json::json!({}),
    );

    let graph_root = run_root.join("graph");
    let (copied_census, copied_census_observation) = measure(|| corpus_census(&graph_root));
    assert_eq!(copied_census, source_census);
    emit(
        &arm,
        trial,
        "setup",
        "copied_corpus_census",
        None,
        copied_census_observation,
        serde_json::to_value(&copied_census).unwrap(),
    );
    let ((subtree_target, paths, transformed), metadata_observation) =
        measure(|| prepare_metadata(&graph_root));
    emit(
        &arm,
        trial,
        "setup",
        "metadata_and_transform",
        None,
        metadata_observation,
        serde_json::json!({
            "transformed": transformed,
            "subtree_path": subtree_target.path,
            "subtree_size": SUBTREE_BLOCKS,
            "source_files_after": paths.len(),
        }),
    );
    let (fixture_census, fixture_census_observation) = measure(|| corpus_census(&graph_root));
    assert_eq!(fixture_census.files, source_census.files + 2);
    assert_eq!(fixture_census.markdown_org, source_census.markdown_org + 2);
    emit(
        &arm,
        trial,
        "setup",
        "fixture_corpus_census",
        None,
        fixture_census_observation,
        serde_json::to_value(&fixture_census).unwrap(),
    );

    let seed = 0x1_17e_0000_u128 + trial as u128 * 0x100;
    let fixture = ActivationFixture::reopen_at(run_root.clone(), seed);
    let (activated, activation_observation) =
        measure(|| SyncRuntimeHandle::activate_or_resume_local(fixture.request.clone()));
    emit(
        &arm,
        trial,
        "setup",
        "activation",
        None,
        activation_observation,
        serde_json::json!({}),
    );
    assert_eq!(activated.status, SyncLocalActivationStatus::Active);
    let handle = activated.handle.expect("corpus activates");

    let (_, initial_feed_observation) =
        measure(|| drive_initial_feed_with_turn_budget(&handle, 4096));
    emit(
        &arm,
        trial,
        "setup",
        "activation_feed",
        None,
        initial_feed_observation,
        serde_json::json!({}),
    );

    let (activated_census, target_observation) = measure(|| {
        activated_census_and_target(
            &handle,
            &paths,
            &[subtree_target.path.as_str(), DESTINATION_PATH],
        )
    });
    emit(
        &arm,
        trial,
        "setup",
        "activated_page_block_census_and_target",
        None,
        target_observation,
        serde_json::json!({
            "inventory_pages": activated_census.inventory_pages,
            "parsed_pages": activated_census.parsed_pages,
            "parsed_blocks": activated_census.parsed_blocks,
            "edit_path": activated_census.edit_path,
            "block_depth": activated_census.edit_block_path.len(),
            "edit_identity": activated_census.edit_identity,
            "authoritative_home_document_id": activated_census.authoritative_home_document_id,
            "home_source": "activated_runtime_exact_page_and_block_query",
        }),
    );
    let edit_path = activated_census.edit_path;
    let edit_block_path = activated_census.edit_block_path;
    let edit_identity = activated_census.edit_identity;
    let cold_home_document_id = DocumentId::from_uuid(
        Uuid::parse_str(&activated_census.authoritative_home_document_id).unwrap(),
    );

    let (shutdown, shutdown_observation) = measure(|| handle.clean_shutdown().unwrap());
    emit(
        &arm,
        trial,
        "setup",
        "post_activation_shutdown",
        None,
        shutdown_observation,
        serde_json::json!({}),
    );
    assert!(matches!(shutdown, SyncShutdownOutcome::Safe(_)));
    drop(handle);

    let (opened, reopen_observation) =
        measure(|| SyncRuntimeHandle::open(reopen_request(&fixture.request)));
    emit(
        &arm,
        trial,
        "setup",
        "cold_reopen",
        None,
        reopen_observation,
        serde_json::json!({}),
    );
    assert_eq!(opened.status, SyncRuntimeOpenStatus::Active);
    let handle = opened.handle.expect("activated corpus cold-reopens");
    arm_live_write_cold_target(cold_home_document_id);

    let cold_meter = PhaseMeter::start();
    let (mut edit_page, edit_revision) = load_application_exact(&handle, &edit_path);
    assert_eq!(
        block_at(&edit_page.blocks, &edit_block_path).id,
        edit_identity
    );
    block_at_mut(&mut edit_page.blocks, &edit_block_path)
        .raw
        .push_str("\nlive-write cold edit trial");
    let save_counters_before = live_write_benchmark_counters();
    let save_rss_before = process_rss_kib();
    let save_started = Instant::now();
    let cold_outcome = handle
        .save_application_page(SyncApplicationPageSaveRequest {
            target: SyncApplicationPageSaveTarget::Existing {
                path: edit_page.path.clone(),
                revision: edit_revision,
            },
            page: edit_page,
        })
        .unwrap();
    let save_elapsed_ns = save_started.elapsed().as_nanos();
    let save_counters = live_write_benchmark_counters().since(save_counters_before);
    let save_rss_after = process_rss_kib();
    let cold_outcome_label = save_outcome_name(&cold_outcome);
    let cold_observation = cold_meter.finish();
    emit(
        &arm,
        trial,
        "foreground",
        "cold_load_edit_save",
        None,
        cold_observation,
        serde_json::json!({"outcome": cold_outcome_label}),
    );
    emit(
        &arm,
        trial,
        "foreground_component",
        "cold_save_call",
        None,
        PhaseObservation {
            elapsed_ns: save_elapsed_ns,
            rss_before_kib: save_rss_before,
            rss_after_kib: save_rss_after,
            sampled_peak_rss_kib: save_rss_before.max(save_rss_after),
            counters: save_counters,
        },
        serde_json::json!({"outcome": cold_outcome_label, "peak_scope": "endpoint_only"}),
    );
    assert!(
        cold_observation.counters.cold_target_misses > 0,
        "the chosen block document was already resident on the fresh runtime: {:?}",
        cold_observation.counters
    );

    let ((mut edit_page, mut edit_revision), cold_settlement) =
        measure(|| finish_save(&handle, &edit_path, cold_outcome));
    emit(
        &arm,
        trial,
        "settlement",
        "cold_edit",
        None,
        cold_settlement,
        serde_json::json!({}),
    );
    assert!(block_at(&edit_page.blocks, &edit_block_path)
        .raw
        .contains("live-write cold edit trial"));

    for sample in 1..=10 {
        block_at_mut(&mut edit_page.blocks, &edit_block_path)
            .raw
            .push_str(&format!("\nlive-write repeat {sample}"));
        let request = SyncApplicationPageSaveRequest {
            target: SyncApplicationPageSaveTarget::Existing {
                path: edit_page.path.clone(),
                revision: edit_revision,
            },
            page: edit_page,
        };
        let (outcome, foreground) = measure(|| handle.save_application_page(request).unwrap());
        let outcome_label = save_outcome_name(&outcome);
        emit(
            &arm,
            trial,
            "foreground",
            "repeated_edit",
            Some(sample),
            foreground,
            serde_json::json!({"outcome": outcome_label}),
        );
        let (settled, settlement) = measure(|| finish_save(&handle, &edit_path, outcome));
        emit(
            &arm,
            trial,
            "settlement",
            "repeated_edit",
            Some(sample),
            settlement,
            serde_json::json!({}),
        );
        edit_page = settled.0;
        edit_revision = settled.1;
        assert!(block_at(&edit_page.blocks, &edit_block_path)
            .raw
            .contains(&format!("live-write repeat {sample}")));
    }

    let (((source_page, source_revision), (destination_page, destination_revision)), move_setup) =
        measure(|| {
            (
                load_application_exact(&handle, &subtree_target.path),
                load_application_exact(&handle, DESTINATION_PATH),
            )
        });
    let moved_root = block_at(&source_page.blocks, &subtree_target.block_path);
    assert_eq!(subtree_size(moved_root), SUBTREE_BLOCKS);
    let moved_identity = moved_root.id.clone();
    emit(
        &arm,
        trial,
        "setup",
        "move_targets",
        None,
        move_setup,
        serde_json::json!({
            "source_path": subtree_target.path,
            "destination_path": DESTINATION_PATH,
            "subtree_size": subtree_size(moved_root),
            "source_total_blocks": page_block_count(&source_page),
            "destination_total_blocks": page_block_count(&destination_page),
        }),
    );
    let move_request = SyncApplicationMoveSubtreesRequest {
        episode_id: Uuid::new_v4().to_string(),
        source_path: source_page.path.clone(),
        source_revision,
        destination_path: destination_page.path.clone(),
        destination_revision,
        roots: vec![SyncApplicationMoveRoot {
            identity: moved_identity.clone(),
            raw_rewrite: None,
        }],
        placement: SyncApplicationMovePlacement::Root { position: 0 },
        admission: application_move_admission(),
    };
    let (move_outcome, move_foreground) = measure(|| {
        handle
            .move_application_subtrees(move_request.clone())
            .unwrap()
    });
    let move_foreground_label = move_outcome_name(&move_outcome);
    emit(
        &arm,
        trial,
        "foreground",
        "move_200_subtree",
        None,
        move_foreground,
        serde_json::json!({"outcome": move_foreground_label}),
    );
    let (move_outcome, move_settlement) =
        measure(|| finish_move(&handle, &move_request, move_outcome));
    emit(
        &arm,
        trial,
        "settlement",
        "move_200_subtree",
        None,
        move_settlement,
        serde_json::json!({}),
    );
    let SyncApplicationMoveSubtreesOutcome::Committed {
        source,
        destination,
        ..
    } = move_outcome
    else {
        unreachable!()
    };
    assert!(!page_contains_id(&source.page.blocks, &moved_identity));
    let destination_root = destination
        .page
        .blocks
        .iter()
        .find(|block| block.id == moved_identity)
        .expect("moved subtree is a destination root");
    assert_eq!(subtree_size(destination_root), SUBTREE_BLOCKS);
    let (
        ((reloaded_source, _), (reloaded_destination, reloaded_destination_revision)),
        delete_setup,
    ) = measure(|| {
        (
            load_application_exact(&handle, &subtree_target.path),
            load_application_exact(&handle, DESTINATION_PATH),
        )
    });
    emit(
        &arm,
        trial,
        "setup",
        "delete_target_reload",
        None,
        delete_setup,
        serde_json::json!({
            "source_path": subtree_target.path,
            "destination_path": DESTINATION_PATH,
        }),
    );
    assert!(!page_contains_id(&reloaded_source.blocks, &moved_identity));
    assert!(page_contains_id(
        &reloaded_destination.blocks,
        &moved_identity
    ));

    let before_delete_blocks = page_block_count(&reloaded_destination);
    let mut delete_page = reloaded_destination;
    let root_index = delete_page
        .blocks
        .iter()
        .position(|block| block.id == moved_identity)
        .expect("delete target remains a root");
    let removed = delete_page.blocks.remove(root_index);
    assert_eq!(subtree_size(&removed), SUBTREE_BLOCKS);
    let delete_request = SyncApplicationPageSaveRequest {
        target: SyncApplicationPageSaveTarget::Existing {
            path: delete_page.path.clone(),
            revision: reloaded_destination_revision,
        },
        page: delete_page,
    };
    let (delete_outcome, delete_foreground) =
        measure(|| handle.save_application_page(delete_request).unwrap());
    let delete_outcome_label = save_outcome_name(&delete_outcome);
    emit(
        &arm,
        trial,
        "foreground",
        "delete_200_subtree",
        None,
        delete_foreground,
        serde_json::json!({"outcome": delete_outcome_label}),
    );
    let ((deleted_page, _), delete_settlement) =
        measure(|| finish_save(&handle, DESTINATION_PATH, delete_outcome));
    emit(
        &arm,
        trial,
        "settlement",
        "delete_200_subtree",
        None,
        delete_settlement,
        serde_json::json!({}),
    );
    assert!(!page_contains_id(&deleted_page.blocks, &moved_identity));
    assert_eq!(
        page_block_count(&deleted_page) + SUBTREE_BLOCKS,
        before_delete_blocks
    );
    let (deleted_reloaded, _) = load_application_exact(&handle, DESTINATION_PATH);
    assert!(!page_contains_id(&deleted_reloaded.blocks, &moved_identity));

    let (shutdown, final_shutdown) = measure(|| handle.clean_shutdown().unwrap());
    emit(
        &arm,
        trial,
        "setup",
        "final_shutdown",
        None,
        final_shutdown,
        serde_json::json!({}),
    );
    assert!(matches!(shutdown, SyncShutdownOutcome::Safe(_)));
    std::mem::forget(fixture);
}
