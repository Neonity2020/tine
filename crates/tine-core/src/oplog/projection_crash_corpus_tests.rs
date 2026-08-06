// Versioned, durability-cut crash corpus for projection receipts.  This file
// is included by `projection_store`'s unit-test module so the runner can drive
// the real private durability hooks. It deliberately does not re-create
// receipt files with ordinary writes: each named cut is either an existing
// production durability boundary, an actual permission failure, or a process
// boundary.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::model::Graph;
use crate::oplog::{
    execute_manifested_projection_work, plan_affected_import, plan_projection,
    recover_incomplete_projections, ApplicationRuntimeRoot, AuthorBatch, BatchDisposition,
    BatchId, BatchInspection, BatchOrigin, BlobDescription, BlockId, BlockLocation, BlockOwner,
    CrdtPeerId, DeviceId, DocumentId, FrontierV2, ImportPlanStatus, LineageDigest,
    ManagedPath, ManagedTextKind, ManifestProjectionTarget, ManifestedProjectionIntent,
    ObjectKind, ObjectStore, OperationTransaction, PageId, PageState, ProjectionClaim,
    ProjectionEndpointBinding, ProjectionEndpointId, ProjectionError, ProjectionIntent,
    ProjectionPrecondition, ProjectionRecovery, ProjectionWorkStatus, ProjectionWorkTarget,
    RebuildSource, SemanticOperation, SessionId, ShardedHotEngine, SqliteFrontier, WorkspaceId,
};
use crate::oplog::projection::execute_receiver_local_projection_under_handoff;
use crate::oplog::projection_store::ProjectionCleanupRetirementAuthority;
use crate::model::{
    fail_next_projection_directory_sync_for_test, projection_graph_test_counters,
    reset_projection_graph_test_counters, reset_projection_graph_test_hooks,
    ProjectionGraphTestCounters, ProjectionRecoveryCleanup,
};

use super::super::{
    completion_filename, projection_store_test_counters, reset_projection_store_test_counters,
    reset_projection_store_test_hooks, ProjectionStoreTestCounters, ATTEMPT_PUBLICATION_HOOK,
    COMPLETIONS_DIR, COMPLETION_RETAINED_SLOT_HOOK, MUTATION_AUTHORITY_CAPTURED_HOOK,
    MUTATION_AUTHORITY_ACT_HOOK, MUTATION_AUTHORITY_SUFFIX,
};
use super::Fixture;

const FIXTURE_VERSION: u32 = 2;
const WORKER_TEST_NAME: &str =
    "oplog::projection_store::tests::crash_corpus::crash_corpus_subprocess_worker";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CorpusManifest {
    fixture_version: u32,
    pages: Vec<SemanticFixturePage>,
    salvage_rebuild: SalvageRebuildFixture,
    cases: Vec<CorpusCase>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SemanticFixturePage {
    kind: FixtureKind,
    path: String,
    format: String,
    bytes: String,
    sha256: String,
    preamble: Option<String>,
    sparse_ids: Vec<String>,
    deleted: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum FixtureKind {
    Page,
    Journal,
}

impl FixtureKind {
    const fn managed_kind(self) -> ManagedTextKind {
        match self {
            Self::Page => ManagedTextKind::Page,
            Self::Journal => ManagedTextKind::Journal,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SalvageRebuildFixture {
    corrupt_sidecars: Vec<String>,
    expected_rebuilt_pages: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CorpusCase {
    id: String,
    operation: String,
    cut: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
struct Measurements {
    authority_slots: usize,
    authority_bytes: u64,
    attempt_slots: usize,
    projection_write_calls: usize,
    projection_remove_calls: usize,
    projection_recovery_calls: usize,
    forensic_files: usize,
    forensic_bytes: u64,
    catalog_directory_entries: usize,
    completion_lookups: usize,
    accepted_events_validated: usize,
    accepted_events_applied: usize,
    accepted_sequence_page_reads: usize,
}

#[derive(Debug)]
struct CaseReceipt {
    id: String,
    cut: String,
    before: String,
    after: String,
    measured: Measurements,
}

impl CaseReceipt {
    fn render(&self) -> String {
        format!(
            "case={} cut={} before={} after={} measured=authority_slots:{},authority_bytes:{},attempt_slots:{},projection_writes:{},projection_removes:{},projection_recoveries:{},forensic_files:{},forensic_bytes:{},catalog_entries:{},completion_lookups:{},accepted_validated:{},accepted_applied:{},accepted_page_reads:{}",
            self.id,
            self.cut,
            self.before,
            self.after,
            self.measured.authority_slots,
            self.measured.authority_bytes,
            self.measured.attempt_slots,
            self.measured.projection_write_calls,
            self.measured.projection_remove_calls,
            self.measured.projection_recovery_calls,
            self.measured.forensic_files,
            self.measured.forensic_bytes,
            self.measured.catalog_directory_entries,
            self.measured.completion_lookups,
            self.measured.accepted_events_validated,
            self.measured.accepted_events_applied,
            self.measured.accepted_sequence_page_reads,
        )
    }
}

struct CorpusDir(PathBuf);

impl CorpusDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tine-oplog-projection-crash-corpus-{label}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for CorpusDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy_regular_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_regular_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

struct CrashCorpusTestStateGuard;

impl CrashCorpusTestStateGuard {
    fn new() -> Self {
        reset_projection_store_test_hooks();
        reset_projection_graph_test_hooks();
        Self
    }
}

impl Drop for CrashCorpusTestStateGuard {
    fn drop(&mut self) {
        reset_projection_store_test_hooks();
        reset_projection_graph_test_hooks();
    }
}

fn corpus_manifest() -> CorpusManifest {
    const FIXTURE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/oplog-projection-crash-corpus/v2.json"
    ));
    let manifest: CorpusManifest = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(
        manifest.fixture_version, FIXTURE_VERSION,
        "unknown corpus fixture version"
    );
    assert_eq!(
        serde_json::to_string(&manifest).unwrap(),
        FIXTURE.trim_end_matches('\n'),
        "crash corpus fixture must use its canonical compact JSON encoding"
    );
    manifest
}

fn capture_graph_measurements(measured: &mut Measurements) {
    let ProjectionGraphTestCounters {
        write_calls,
        remove_calls,
        recovery_calls,
    } = projection_graph_test_counters();
    measured.projection_write_calls = write_calls;
    measured.projection_remove_calls = remove_calls;
    measured.projection_recovery_calls = recovery_calls;
}

fn fixture_document(page: &SemanticFixturePage) -> crate::doc::Document {
    match page.format.as_str() {
        "markdown" if page.path.ends_with(".md") || page.path.ends_with(".markdown") => {
            crate::doc::parse(&page.bytes)
        }
        "org" if page.path.ends_with(".org") => crate::org::parse_org(&page.bytes),
        other => panic!("fixture format/path mismatch for {}: {other}", page.path),
    }
}

fn assert_fixture_page_hash(page: &SemanticFixturePage) {
    assert_eq!(
        format!("{:x}", Sha256::digest(page.bytes.as_bytes())),
        page.sha256,
        "fixture byte hash drifted for {}",
        page.path
    );
}

fn projection_writer_supports(path: &ManagedPath) -> bool {
    path.as_str().ends_with(".md") || path.as_str().ends_with(".org")
}

fn write_fixture_file(root: &Path, page: &SemanticFixturePage) {
    let destination = root.join(&page.path);
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::write(destination, page.bytes.as_bytes()).unwrap();
}

fn distinct_crlf_base(target: &[u8]) -> Vec<u8> {
    let target = std::str::from_utf8(target).expect("CRLF fixture target must be UTF-8");
    assert!(
        target.contains("\r\n"),
        "CRLF fixture target lost its CRLF line endings"
    );
    let base = if target.contains("retain line endings") {
        target.replacen("retain line endings", "external CRLF baseline", 1)
    } else {
        target.replacen(
            "retain graph-wide CRLF",
            "external graph-wide CRLF baseline",
            1,
        )
    };
    assert_ne!(base, target, "CRLF base must differ from the projected target");
    assert!(
        base.contains("\r\n"),
        "CRLF fixture base lost its CRLF line endings"
    );
    base.into_bytes()
}

fn append_fixture_blocks(
    blocks: &[crate::doc::DocBlock],
    page_id: PageId,
    home_document_id: DocumentId,
    parent: Option<BlockId>,
    next_block: &mut u128,
    operations: &mut Vec<SemanticOperation>,
) {
    for (sibling, block) in blocks.iter().enumerate() {
        let block_id = BlockId::from_uuid(Uuid::from_u128(*next_block));
        *next_block += 1;
        operations.push(SemanticOperation::CreateBlock {
            block: BlockLocation {
                block_id,
                home_document_id,
            },
            page_id,
            parent,
            order: format!("{sibling:08}"),
            content: block.raw.clone(),
        });
        append_fixture_blocks(
            &block.children,
            page_id,
            home_document_id,
            Some(block_id),
            next_block,
            operations,
        );
    }
}

fn parsed_sparse_ids(blocks: &[crate::doc::DocBlock]) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    let mut pending = blocks.iter().collect::<Vec<_>>();
    while let Some(block) = pending.pop() {
        ids.extend(block.properties().into_iter().filter_map(|(key, value)| {
            (crate::doc::property_key_norm(&key) == "id").then_some(value)
        }));
        pending.extend(block.children.iter());
    }
    ids
}

fn assert_live_fixture_pages_semantics(pages: &[SemanticFixturePage]) {
    let admission_root = CorpusDir::new("semantic-fixture-admission");
    let admission_graph_root = admission_root.path().join("graph");
    fs::create_dir_all(admission_graph_root.join("pages")).unwrap();
    fs::create_dir_all(admission_graph_root.join("journals")).unwrap();
    for page in pages.iter().filter(|page| !page.deleted) {
        assert_fixture_page_hash(page);
        write_fixture_file(&admission_graph_root, page);
    }
    let admission_graph = Graph::open(&admission_graph_root);
    fs::create_dir_all(admission_root.path().join("capture")).unwrap();
    let capture = admission_graph
        .capture_inactive_bootstrap_sources(&admission_root.path().join("capture"))
        .unwrap();
    let mut entries = capture.entries_cursor().unwrap();
    let mut captured = Vec::new();
    while let Some(entry) = entries.next().unwrap() {
        captured.push((
            entry.path().as_str().to_owned(),
            entry.kind(),
            entry.description(),
        ));
    }
    captured.sort_by(|left, right| left.0.cmp(&right.0));
    let mut expected_capture = pages
        .iter()
        .filter(|page| !page.deleted)
        .map(|page| {
            (
                page.path.clone(),
                page.kind.managed_kind(),
                BlobDescription::of(page.bytes.as_bytes()),
            )
        })
        .collect::<Vec<_>>();
    expected_capture.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(
        captured, expected_capture,
        "graph-wide fixture admission drifted"
    );

    let root = CorpusDir::new("semantic-fixtures");
    let graph_root = root.path().join("graph");
    fs::create_dir_all(graph_root.join("pages")).unwrap();
    fs::create_dir_all(graph_root.join("journals")).unwrap();
    let graph = Graph::open(&graph_root);
    let workspace = corpus_workspace(81_000);
    let binding = corpus_binding(&graph, 81_010);
    let receipts = crate::oplog::ProjectionReceiptStore::open_for_endpoint(
        &root.path().join("receipts"),
        workspace,
        binding,
    )
    .unwrap();
    let lineage = LineageDigest::of(b"crash-corpus-semantic-fixtures");
    let catalog = DocumentId::from_uuid(corpus_uuid(81_020));
    let author = ShardedHotEngine::new(workspace, lineage, catalog);
    let mut operations = Vec::new();
    let mut live = Vec::new();
    let mut paths = BTreeSet::new();
    let mut next_block = 81_100_u128;

    for (index, page) in pages.iter().enumerate() {
        let path = ManagedPath::parse(&page.path).unwrap();
        assert!(paths.insert(path.clone()), "duplicate fixture path {path}");
        assert_fixture_page_hash(page);
        if page.deleted {
            continue;
        }
        let document = fixture_document(page);
        assert_eq!(
            document.pre_block.as_deref(),
            page.preamble.as_deref(),
            "manifest preamble is not the parser-owned preamble for {path}"
        );
        assert_eq!(
            parsed_sparse_ids(&document.roots),
            page.sparse_ids.iter().cloned().collect(),
            "manifest sparse IDs are not parser-owned block identities for {path}"
        );
        let page_id = PageId::from_uuid(corpus_uuid(81_200 + index as u128));
        let home_document_id = DocumentId::from_uuid(corpus_uuid(81_300 + index as u128));
        operations.push(SemanticOperation::CreatePage {
            page_id,
            home_document_id,
            name: crate::oplog::LogicalPageName::parse(format!(
                "Crash Corpus Page {index}"
            ))
            .unwrap(),
            path: path.clone(),
            kind: page.kind.managed_kind(),
        });
        if document.pre_block.is_some() {
            operations.push(SemanticOperation::SetPagePreamble {
                page_id,
                preamble: document.pre_block.clone(),
            });
        }
        append_fixture_blocks(
            &document.roots,
            page_id,
            home_document_id,
            None,
            &mut next_block,
            &mut operations,
        );
        live.push((index, page_id, path));
    }

    let prepared = author
        .prepare_bootstrap_transaction(
            AuthorBatch {
                batch_id: BatchId::from_uuid(corpus_uuid(81_400)),
                author_device_id: DeviceId::from_uuid(corpus_uuid(81_401)),
                author_session_id: SessionId::from_uuid(corpus_uuid(81_402)),
                crdt_peer_id: CrdtPeerId::from_u64(81_403),
            },
            &OperationTransaction::new(operations).unwrap(),
        )
        .unwrap();
    let archive_path = root.path().join("archive");
    ObjectStore::open(&archive_path, workspace)
        .unwrap()
        .publish_bootstrap_prepared_for_test(&prepared)
        .unwrap();
    let mut engine = ShardedHotEngine::with_enrolled_projection(
        ObjectStore::open(&archive_path, workspace).unwrap(),
        lineage,
        catalog,
        &graph,
        &receipts,
    );
    assert!(matches!(
        engine.stage_archive_batch(prepared.manifest().batch_id()).unwrap().disposition(),
        BatchDisposition::Accepted { .. }
    ));

    for (index, page_id, path) in &live {
        let page = &pages[*index];
        let state = engine.materialize_page_for_projection(*page_id).unwrap();
        assert_eq!(state.page.path, *path);
        assert_eq!(state.page.preamble.as_deref(), page.preamble.as_deref());
        if !projection_writer_supports(path) {
            write_fixture_file(&graph_root, page);
            continue;
        }
        let has_crlf = page.bytes.contains("\r\n");
        let expected_base = has_crlf.then(|| {
            let base = distinct_crlf_base(page.bytes.as_bytes());
            let destination = graph_root.join(path.as_str());
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            // Model an external editor's already-observed base.  The guarded
            // mutation below, from this base to the projection target, must not
            // use an ordinary filesystem write.
            fs::write(destination, &base).unwrap();
            base
        });
        let plan = plan_projection(workspace, &state, expected_base.as_deref()).unwrap();
        assert_eq!(
            plan.target(),
            page.bytes.as_bytes(),
            "real projection parser/serializer drifted for {path}"
        );
        receipts
            .publish_intent(plan.intent(), expected_base.as_deref())
            .unwrap();
        if has_crlf {
            reset_projection_graph_test_counters();
        }
        let reservation = receipts.reserve_attempt(plan.intent()).unwrap();
        let mut authority = receipts
            .begin_mutation(plan.intent(), Some(&reservation))
            .unwrap();
        graph
            .write_page_projection(
                path.as_str(),
                expected_base.as_deref(),
                plan.target(),
                &mut authority,
            )
            .unwrap();
        drop(authority);

        let recovered = recover_incomplete_projections(&graph, &receipts, &engine).unwrap();
        assert_eq!(recovered.len(), 1, "fixture recovery count drifted for {path}");
        assert_eq!(recovered[0].plan.intent(), plan.intent());
        assert_eq!(recovered[0].plan.target(), page.bytes.as_bytes());
        assert_eq!(
            graph.read_raw_managed_text(path).unwrap().unwrap().into_bytes(),
            page.bytes.as_bytes(),
            "recovered fixture bytes drifted for {path}"
        );
        if has_crlf {
            assert_eq!(
                projection_graph_test_counters(),
                ProjectionGraphTestCounters {
                    write_calls: 1,
                    remove_calls: 0,
                    recovery_calls: 1,
                },
                "CRLF target must use the guarded writer once before exact recovery"
            );
        }
    }

    let snapshot = engine.canonical_snapshot().unwrap();
    for (index, page_id, path) in &live {
        let page = &pages[*index];
        assert!(snapshot.pages.iter().any(|(observed_id, state)| {
            *observed_id == *page_id
                && matches!(state, PageState::Live { path: observed_path, kind, .. }
                    if observed_path == path && *kind == page.kind.managed_kind())
        }));
    }
    let requested = live
        .iter()
        .filter(|(_, _, path)| projection_writer_supports(path))
        .map(|(_, _, path)| path.as_str())
        .collect::<Vec<_>>();
    let import = plan_affected_import(&graph, &receipts, &engine, &requested);
    assert_eq!(
        import.status(),
        ImportPlanStatus::Noop,
        "live projection fixture must be import-clean: {import:#?}"
    );
    let inventory = import.inventory().unwrap();
    let matches = import.matches().unwrap();
    assert_eq!(matches.pages().len(), requested.len());
    assert!(matches.rejected_raw_ids().is_empty());
    for (index, _, path) in &live {
        if !projection_writer_supports(path) {
            continue;
        }
        let page = &pages[*index];
        assert_eq!(inventory.present(path.as_str()).unwrap().bytes(), page.bytes.as_bytes());
        if !page.sparse_ids.is_empty() {
            assert!(matches.blocks().iter().any(|matched| matched.path() == path));
        }
    }
}

fn assert_fixture_semantics(manifest: &CorpusManifest) {
    assert_live_fixture_pages_semantics(&manifest.pages);
    let deleted = manifest
        .pages
        .iter()
        .filter(|page| page.deleted)
        .collect::<Vec<_>>();
    assert_eq!(deleted.len(), 1, "fixture must declare one deleted page");
    let page = deleted[0];
    let (dir, graph, receipts, _writer, mut engine, _binding, work) =
        corpus_manifested_deletion_fixture("semantic-deletion", page);
    let target = dir.path().join("graph").join(&page.path);
    assert_eq!(fs::read(&target).unwrap(), page.bytes.as_bytes());
    execute_manifested_projection_work(&graph, &receipts, &mut engine, &work).unwrap();
    assert!(!target.exists());
    assert!(matches!(
        engine.page_state_for_test(work.page_id()).unwrap(),
        Some(PageState::Tombstone { kind, .. }) if kind == page.kind.managed_kind()
    ));
    let import = plan_affected_import(&graph, &receipts, &engine, &[page.path.as_str()]);
    assert_eq!(import.status(), ImportPlanStatus::Noop);
    assert!(import.inventory().unwrap().present(&page.path).is_none());
}

#[test]
fn already_absent_receiver_projection_completes_foreign_deletion() {
    let manifest = corpus_manifest();
    let deleted = manifest
        .pages
        .iter()
        .find(|page| page.deleted)
        .expect("crash corpus deleted page");
    let (dir, source_graph, source_receipts, writer, source_engine, _, work) =
        corpus_manifested_deletion_fixture("receiver-already-absent-deletion", deleted);
    let source = match writer.inspect_batch(work.batch_id()).unwrap() {
        BatchInspection::Ready(batch) => batch
            .objects()
            .iter()
            .find(|object| object.kind() == ObjectKind::ProjectionIntent)
            .map(|object| ManifestedProjectionIntent::decode(object.payload()).unwrap())
            .expect("foreign deletion projection intent"),
        other => panic!("deletion batch is not ready: {other:?}"),
    };
    assert_eq!(source.target(), &ManifestProjectionTarget::Absent);
    assert_eq!(source.path().as_str(), deleted.path);
    drop(source_engine);
    drop(source_receipts);
    drop(source_graph);

    let receiver =
        corpus_provider_receiver(&dir, &source, work.batch_id(), ReceiverInitialTarget::Absent);
    let receiver_state = receiver
        .engine
        .page_state_for_test(source.page_id())
        .unwrap();
    assert!(matches!(
        receiver_state,
        Some(PageState::Tombstone { .. })
    ), "receiver state is {receiver_state:?}");
    assert!(
        receiver
            .graph
            .read_projection_input(source.path())
            .unwrap()
            .is_none(),
        "receiver must begin at the exact already-absent target"
    );

    let receiver_intent = ProjectionIntent::new(
        corpus_workspace(1),
        source.page_id(),
        source.path().clone(),
        source.post_frontier().clone(),
        source.claim_evidence().to_vec(),
        ProjectionPrecondition::Absent,
        BlobDescription::of(&[]),
        Vec::new(),
    )
    .unwrap();
    let handoff = receiver
        .graph
        .mint_handoff_safe(corpus_workspace(1), receiver.binding)
        .unwrap()
        .into_publisher_guard()
        .into_published_latch();
    let executed = execute_receiver_local_projection_under_handoff(
        &receiver.graph,
        &receiver.receipts,
        &receiver.engine,
        &source,
        &handoff,
        true,
    );
    assert!(
        matches!(executed, Ok(Some(true))),
        "an accepted foreign deletion whose receiver target is already absent must complete: \
         {executed:?}"
    );
    assert!(
        receiver
            .receipts
            .load_completion(&receiver_intent)
            .unwrap()
            .is_some(),
        "receiver-local completion must durably bind the accepted tombstone"
    );
    assert!(
        receiver
            .graph
            .read_projection_input(source.path())
            .unwrap()
            .is_none(),
        "already-absent receiver bytes must remain absent"
    );
    let completed = receiver
        .engine
        .projection_work_index()
        .unwrap()
        .completed_receipts_for_path(source.path())
        .unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].target(), ProjectionWorkTarget::Absent);
    assert_ne!(
        completed[0].target(),
        ProjectionWorkTarget::Present(BlobDescription::of(&[])),
        "an absent completion must not collapse into a present empty target"
    );
    let import = plan_affected_import(
        &receiver.graph,
        &receiver.receipts,
        &receiver.engine,
        &[source.path().as_str()],
    );
    assert_eq!(
        import.status(),
        ImportPlanStatus::Noop,
        "Graph/import interpretation must agree with the oplog tombstone"
    );
}

#[test]
fn projected_receiver_bytes_are_removed_by_foreign_deletion() {
    let manifest = corpus_manifest();
    let deleted = deleted_fixture_page(&manifest);
    let (dir, source_graph, source_receipts, writer, source_engine, _, work) =
        corpus_manifested_deletion_fixture("receiver-projected-deletion", deleted);
    let source = manifested_intent_for_work(&writer, &work);
    drop(source_engine);
    drop(source_receipts);
    drop(source_graph);
    let receiver =
        corpus_provider_receiver(&dir, &source, work.batch_id(), ReceiverInitialTarget::Projected);
    let prior = receiver
        .graph
        .read_projection_input(source.path())
        .unwrap()
        .expect("receiver projected prior bytes");
    let handoff = receiver
        .graph
        .mint_handoff_safe(corpus_workspace(1), receiver.binding)
        .unwrap()
        .into_publisher_guard()
        .into_published_latch();
    assert_eq!(
        execute_receiver_local_projection_under_handoff(
            &receiver.graph,
            &receiver.receipts,
            &receiver.engine,
            &source,
            &handoff,
            true,
        )
        .unwrap(),
        Some(true)
    );
    assert!(receiver
        .graph
        .read_projection_input(source.path())
        .unwrap()
        .is_none());
    let completed = receiver
        .engine
        .projection_work_index()
        .unwrap()
        .completed_receipts_for_path(source.path())
        .unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].target(), ProjectionWorkTarget::Absent);
    assert_ne!(BlobDescription::of(&prior), BlobDescription::of(&[]));
}

#[test]
fn prior_present_receipt_with_already_absent_receiver_completes_foreign_deletion() {
    let manifest = corpus_manifest();
    let deleted = deleted_fixture_page(&manifest);
    let (dir, source_graph, source_receipts, writer, source_engine, _, work) =
        corpus_manifested_deletion_fixture("receiver-prior-present-already-absent", deleted);
    let source = manifested_intent_for_work(&writer, &work);
    drop(source_engine);
    drop(source_receipts);
    drop(source_graph);
    let receiver = corpus_provider_receiver(
        &dir,
        &source,
        work.batch_id(),
        ReceiverInitialTarget::ProjectedThenRemoved,
    );
    assert!(
        receiver
            .graph
            .read_projection_input(source.path())
            .unwrap()
            .is_none(),
        "receiver file must be absent before the foreign tombstone is accepted"
    );
    let before = receiver
        .engine
        .projection_work_index()
        .unwrap()
        .completed_receipts_for_path(source.path())
        .unwrap();
    let [prior] = before.as_slice() else {
        panic!("receiver must retain exactly one prior Present completion");
    };
    assert!(matches!(
        prior.target(),
        ProjectionWorkTarget::Present(_)
    ));
    let (prior_intent, _) = receiver.receipts.load_completed_receipt(prior).unwrap();
    assert_eq!(
        prior_intent.precondition(),
        &ProjectionPrecondition::Absent,
        "the prior Present projection must not imply retained base bytes"
    );
    assert!(receiver.receipts.load_base(&prior_intent).unwrap().is_none());
    assert!(
        receiver
            .receipts
            .incomplete_intents()
            .unwrap()
            .iter()
            .all(|intent| {
                intent.page_id() != source.page_id()
                    || intent.path() != source.path()
                    || intent.frontier() != source.post_frontier()
            }),
        "removing the receiver file must not create a tombstone attempt"
    );

    let handoff = receiver
        .graph
        .mint_handoff_safe(corpus_workspace(1), receiver.binding)
        .unwrap()
        .into_publisher_guard()
        .into_published_latch();
    assert_eq!(
        execute_receiver_local_projection_under_handoff(
            &receiver.graph,
            &receiver.receipts,
            &receiver.engine,
            &source,
            &handoff,
            true,
        )
        .unwrap(),
        Some(true)
    );
    assert!(
        receiver
            .graph
            .read_projection_input(source.path())
            .unwrap()
            .is_none(),
        "accepted tombstone completion must preserve the already-absent target"
    );

    let receipt_root = receiver.receipts.root_path().to_path_buf();
    let CorpusProviderReceiver {
        graph_root,
        graph,
        receipts,
        engine,
        binding,
    } = receiver;
    drop(handoff);
    drop(receipts);
    drop(graph);
    let graph = Graph::open(&graph_root);
    let receipts = crate::oplog::ProjectionReceiptStore::open_for_endpoint(
        &receipt_root,
        corpus_workspace(1),
        binding,
    )
    .unwrap();
    let handoff = graph
        .mint_handoff_safe(corpus_workspace(1), binding)
        .unwrap()
        .into_publisher_guard()
        .into_published_latch();
    assert_eq!(
        execute_receiver_local_projection_under_handoff(
            &graph, &receipts, &engine, &source, &handoff, true,
        )
        .unwrap(),
        Some(false),
        "restart retry must recognize the exact completed tombstone"
    );
    assert!(graph
        .read_projection_input(source.path())
        .unwrap()
        .is_none());
    let completed = engine
        .projection_work_index()
        .unwrap()
        .completed_receipts_for_path(source.path())
        .unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].target(), ProjectionWorkTarget::Absent);
    let (completed_intent, _) = receipts.load_completed_receipt(&completed[0]).unwrap();
    assert_eq!(
        completed_intent.precondition(),
        &ProjectionPrecondition::Absent,
        "live absence may authorize only the already-absent confirmation path"
    );
    assert!(receipts.load_base(&completed_intent).unwrap().is_none());
    let import = plan_affected_import(&graph, &receipts, &engine, &[source.path().as_str()]);
    assert_eq!(import.status(), ImportPlanStatus::Noop);
    assert!(import
        .inventory()
        .unwrap()
        .present(source.path().as_str())
        .is_none());
}

#[test]
fn changed_receiver_bytes_refuse_foreign_deletion_and_preserve_evidence() {
    let manifest = corpus_manifest();
    let deleted = deleted_fixture_page(&manifest);
    let (dir, source_graph, source_receipts, writer, source_engine, _, work) =
        corpus_manifested_deletion_fixture("receiver-changed-deletion", deleted);
    let source = manifested_intent_for_work(&writer, &work);
    drop(source_engine);
    drop(source_receipts);
    drop(source_graph);
    let receiver =
        corpus_provider_receiver(&dir, &source, work.batch_id(), ReceiverInitialTarget::Projected);
    let changed = b"- receiver changed after its completed projection\n";
    fs::write(receiver.graph_root.join(source.path().as_str()), changed).unwrap();
    let before = receiver
        .engine
        .projection_work_index()
        .unwrap()
        .completed_receipts_for_path(source.path())
        .unwrap();
    assert_eq!(before.len(), 1);
    assert!(matches!(
        before[0].target(),
        ProjectionWorkTarget::Present(_)
    ));
    let handoff = receiver
        .graph
        .mint_handoff_safe(corpus_workspace(1), receiver.binding)
        .unwrap()
        .into_publisher_guard()
        .into_published_latch();
    assert!(matches!(
        execute_receiver_local_projection_under_handoff(
            &receiver.graph,
            &receiver.receipts,
            &receiver.engine,
            &source,
            &handoff,
            true,
        ),
        Err(ProjectionError::ReceiverBaseMismatch)
    ));
    assert_eq!(
        receiver
            .graph
            .read_projection_input(source.path())
            .unwrap()
            .unwrap(),
        changed
    );
    assert_eq!(
        receiver
            .engine
            .projection_work_index()
            .unwrap()
            .completed_receipts_for_path(source.path())
            .unwrap(),
        before,
        "refusal must preserve the prior authenticated completion"
    );
    assert!(receiver
        .receipts
        .validated_catalog()
        .unwrap()
        .iter()
        .all(|entry| {
            entry.intent.path() != source.path()
                || entry.intent.frontier() != source.post_frontier()
                || entry.completion.is_none()
        }));
}

#[test]
fn already_absent_foreign_deletion_retry_is_idempotent_through_path_indexing() {
    let manifest = corpus_manifest();
    let deleted = deleted_fixture_page(&manifest);
    let (dir, source_graph, source_receipts, writer, source_engine, _, work) =
        corpus_manifested_deletion_fixture("receiver-absent-retry", deleted);
    let source = manifested_intent_for_work(&writer, &work);
    drop(source_engine);
    drop(source_receipts);
    drop(source_graph);
    let receiver =
        corpus_provider_receiver(&dir, &source, work.batch_id(), ReceiverInitialTarget::Absent);
    let handoff = receiver
        .graph
        .mint_handoff_safe(corpus_workspace(1), receiver.binding)
        .unwrap()
        .into_publisher_guard()
        .into_published_latch();
    assert_eq!(
        execute_receiver_local_projection_under_handoff(
            &receiver.graph,
            &receiver.receipts,
            &receiver.engine,
            &source,
            &handoff,
            false,
        )
        .unwrap(),
        None
    );
    assert_eq!(
        execute_receiver_local_projection_under_handoff(
            &receiver.graph,
            &receiver.receipts,
            &receiver.engine,
            &source,
            &handoff,
            true,
        )
        .unwrap(),
        Some(true)
    );
    assert_eq!(
        execute_receiver_local_projection_under_handoff(
            &receiver.graph,
            &receiver.receipts,
            &receiver.engine,
            &source,
            &handoff,
            true,
        )
        .unwrap(),
        Some(false)
    );
    let completed = receiver
        .engine
        .projection_work_index()
        .unwrap()
        .completed_receipts_for_path(source.path())
        .unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].target(), ProjectionWorkTarget::Absent);
    receiver
        .receipts
        .load_completed_receipt(&completed[0])
        .unwrap();
}

#[test]
fn already_absent_foreign_deletion_recovers_after_attempt_before_confirmation() {
    let manifest = corpus_manifest();
    let deleted = deleted_fixture_page(&manifest);
    let (dir, source_graph, source_receipts, writer, source_engine, _, work) =
        corpus_manifested_deletion_fixture("receiver-absent-attempt-retry", deleted);
    let source = manifested_intent_for_work(&writer, &work);
    drop(source_engine);
    drop(source_receipts);
    drop(source_graph);
    let receiver =
        corpus_provider_receiver(&dir, &source, work.batch_id(), ReceiverInitialTarget::Absent);
    let handoff = receiver
        .graph
        .mint_handoff_safe(corpus_workspace(1), receiver.binding)
        .unwrap()
        .into_publisher_guard()
        .into_published_latch();

    let receipt_root = receiver.receipts.root_path().to_path_buf();
    let crash_receipt_root = receipt_root.clone();
    MUTATION_AUTHORITY_ACT_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            assert!(
                fs::read_dir(&crash_receipt_root).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .ends_with(MUTATION_AUTHORITY_SUFFIX)
                }),
                "mutation authority must be durable before the crash cut"
            );
            panic!("crash before already-absent confirmation");
        }));
    });
    let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        execute_receiver_local_projection_under_handoff(
            &receiver.graph,
            &receiver.receipts,
            &receiver.engine,
            &source,
            &handoff,
            true,
        )
    }));
    assert!(
        interrupted.is_err(),
        "the injected cut must interrupt after the absent attempt begins"
    );
    let incomplete = receiver
        .receipts
        .incomplete_intents()
        .unwrap()
        .into_iter()
        .filter(|intent| {
            intent.page_id() == source.page_id()
                && intent.path() == source.path()
                && intent.frontier() == source.post_frontier()
                && intent.precondition() == &ProjectionPrecondition::Absent
        })
        .collect::<Vec<_>>();
    let [intent] = incomplete.as_slice() else {
        panic!("already-absent receiver deletion must retain one exact intent");
    };
    assert!(
        receiver.receipts.load_base(intent).unwrap().is_none(),
        "an already-absent confirmation attempt must not invent base bytes"
    );
    assert_eq!(
        receiver
            .receipts
            .load_attempt_reservations(intent)
            .unwrap()
            .len(),
        1,
        "the interrupted confirmation must retain its durable attempt"
    );
    assert!(receiver.receipts.load_completion(intent).unwrap().is_none());
    assert!(receiver
        .graph
        .read_projection_input(source.path())
        .unwrap()
        .is_none());

    let CorpusProviderReceiver {
        graph_root,
        graph,
        receipts,
        engine,
        binding,
    } = receiver;
    drop(handoff);
    drop(receipts);
    drop(graph);
    let graph = Graph::open(&graph_root);
    let receipts = crate::oplog::ProjectionReceiptStore::open_for_endpoint(
        &receipt_root,
        corpus_workspace(1),
        binding,
    )
    .unwrap();
    let receiver = CorpusProviderReceiver {
        graph_root,
        graph,
        receipts,
        engine,
        binding,
    };
    let handoff = receiver
        .graph
        .mint_handoff_safe(corpus_workspace(1), receiver.binding)
        .unwrap()
        .into_publisher_guard()
        .into_published_latch();

    assert_eq!(
        execute_receiver_local_projection_under_handoff(
            &receiver.graph,
            &receiver.receipts,
            &receiver.engine,
            &source,
            &handoff,
            true,
        )
        .unwrap(),
        Some(true)
    );
    assert_eq!(
        execute_receiver_local_projection_under_handoff(
            &receiver.graph,
            &receiver.receipts,
            &receiver.engine,
            &source,
            &handoff,
            true,
        )
        .unwrap(),
        Some(false)
    );
    assert!(
        receiver.receipts.load_base(intent).unwrap().is_none(),
        "retry confirmation must remain base-free"
    );
    assert!(receiver
        .graph
        .read_projection_input(source.path())
        .unwrap()
        .is_none());
    let completed = receiver
        .engine
        .projection_work_index()
        .unwrap()
        .completed_receipts_for_path(source.path())
        .unwrap();
    assert_eq!(completed.len(), 1, "path indexing must be idempotent");
    assert_eq!(completed[0].target(), ProjectionWorkTarget::Absent);
    let import = plan_affected_import(
        &receiver.graph,
        &receiver.receipts,
        &receiver.engine,
        &[source.path().as_str()],
    );
    assert_eq!(
        import.status(),
        ImportPlanStatus::Noop,
        "Graph/import and accepted oplog tombstone must remain a semantic Noop"
    );
    assert!(import
        .inventory()
        .unwrap()
        .present(source.path().as_str())
        .is_none());
}

#[test]
fn accepted_recreation_blocks_stale_foreign_tombstone_removal() {
    let manifest = corpus_manifest();
    let deleted = deleted_fixture_page(&manifest);
    let (dir, source_graph, source_receipts, writer, mut source_engine, _, work) =
        corpus_manifested_deletion_fixture("receiver-recreated-deletion", deleted);
    let source = manifested_intent_for_work(&writer, &work);
    let recreate_batch = BatchId::from_uuid(corpus_uuid(77_000));
    let recreated_page_id = PageId::from_uuid(corpus_uuid(77_010));
    let recreated = source_engine
        .prepare_bootstrap_transaction(
            AuthorBatch {
                batch_id: recreate_batch,
                author_device_id: DeviceId::from_uuid(corpus_uuid(77_001)),
                author_session_id: SessionId::from_uuid(corpus_uuid(77_002)),
                crdt_peer_id: CrdtPeerId::from_u64(77_003),
            },
            &OperationTransaction::new(vec![SemanticOperation::CreatePage {
                page_id: recreated_page_id,
                home_document_id: DocumentId::from_uuid(corpus_uuid(77_011)),
                name: crate::oplog::LogicalPageName::parse(
                    "Crash Corpus Projection Recreated",
                )
                .unwrap(),
                path: source.path().clone(),
                kind: ManagedTextKind::Page,
            }])
            .unwrap(),
        )
        .unwrap();
    writer
        .publish_bootstrap_prepared_for_test(&recreated)
        .unwrap();
    assert!(matches!(
        source_engine
            .stage_archive_batch(recreate_batch)
            .unwrap()
            .disposition(),
        BatchDisposition::Accepted { .. }
    ));
    drop(source_engine);
    drop(source_receipts);
    drop(source_graph);

    let mut receiver =
        corpus_provider_receiver(&dir, &source, work.batch_id(), ReceiverInitialTarget::Absent);
    assert!(matches!(
        receiver
            .engine
            .stage_archive_batch(recreate_batch)
            .unwrap()
            .disposition(),
        BatchDisposition::Accepted { .. }
    ));
    crate::oplog::write_projection_exact(
        &receiver.graph,
        &receiver.receipts,
        &receiver.engine,
        recreated_page_id,
        None,
    )
    .unwrap();
    let recreated_bytes = receiver
        .graph
        .read_projection_input(source.path())
        .unwrap()
        .expect("accepted recreation projection");
    let handoff = receiver
        .graph
        .mint_handoff_safe(corpus_workspace(1), receiver.binding)
        .unwrap()
        .into_publisher_guard()
        .into_published_latch();
    assert_eq!(
        execute_receiver_local_projection_under_handoff(
            &receiver.graph,
            &receiver.receipts,
            &receiver.engine,
            &source,
            &handoff,
            true,
        )
        .unwrap(),
        Some(false),
        "the stale deletion must not authorize against a superseding recreation"
    );
    assert_eq!(
        receiver
            .graph
            .read_projection_input(source.path())
            .unwrap()
            .unwrap(),
        recreated_bytes
    );
}

#[test]
fn receiver_foreign_deletion_recovers_after_removal_before_completion() {
    let manifest = corpus_manifest();
    let deleted = deleted_fixture_page(&manifest);
    let (dir, source_graph, source_receipts, writer, source_engine, _, work) =
        corpus_manifested_deletion_fixture("receiver-removal-retry", deleted);
    let source = manifested_intent_for_work(&writer, &work);
    drop(source_engine);
    drop(source_receipts);
    drop(source_graph);
    let receiver =
        corpus_provider_receiver(&dir, &source, work.batch_id(), ReceiverInitialTarget::Projected);
    let handoff = receiver
        .graph
        .mint_handoff_safe(corpus_workspace(1), receiver.binding)
        .unwrap()
        .into_publisher_guard()
        .into_published_latch();
    assert_eq!(
        execute_receiver_local_projection_under_handoff(
            &receiver.graph,
            &receiver.receipts,
            &receiver.engine,
            &source,
            &handoff,
            false,
        )
        .unwrap(),
        None
    );
    let incomplete = receiver
        .receipts
        .incomplete_intents()
        .unwrap()
        .into_iter()
        .filter(|intent| {
            intent.page_id() == source.page_id()
                && intent.path() == source.path()
                && intent.frontier() == source.post_frontier()
                && intent.target() == BlobDescription::of(&[])
        })
        .collect::<Vec<_>>();
    let [intent] = incomplete.as_slice() else {
        panic!("receiver deletion must publish one incomplete exact intent");
    };
    let base = receiver
        .receipts
        .load_base(intent)
        .unwrap()
        .expect("projected receiver deletion base");
    let reservation = receiver.receipts.reserve_attempt(intent).unwrap();
    let mut mutation = receiver
        .receipts
        .begin_mutation(intent, Some(&reservation))
        .unwrap();
    handoff
        .remove_page_projection(
            &receiver.graph,
            intent.path().as_str(),
            base.bytes(),
            &mut mutation,
        )
        .unwrap();
    drop(mutation);
    assert!(receiver
        .graph
        .read_projection_input(source.path())
        .unwrap()
        .is_none());
    assert!(receiver.receipts.load_completion(intent).unwrap().is_none());

    assert_eq!(
        execute_receiver_local_projection_under_handoff(
            &receiver.graph,
            &receiver.receipts,
            &receiver.engine,
            &source,
            &handoff,
            true,
        )
        .unwrap(),
        Some(true)
    );
    let completed = receiver
        .engine
        .projection_work_index()
        .unwrap()
        .completed_receipts_for_path(source.path())
        .unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].target(), ProjectionWorkTarget::Absent);
    receiver
        .receipts
        .load_completed_receipt(&completed[0])
        .unwrap();
}

fn case_spec(id: &str) -> (&'static str, &'static str) {
    match id {
        "attempt_authority_publication" => (
            "ProjectionReceiptStore::begin_mutation",
            "after publish_immutable_exact(attempt reservation)",
        ),
        "pregraph_drop_retry" => (
            "ProjectionMutationAuthority::drop",
            "after begin_mutation before Graph::write_page_projection",
        ),
        "interrupted_recovery_slot_reuse" => (
            "Graph::recover_page_projection",
            "after durable target proof before publish_completion",
        ),
        "completion_retained_slot" => (
            "ProjectionReceiptStore::publish_completion",
            "after publish_immutable_exact(completion) before authority retirement",
        ),
        "deletion_completion_publication" => (
            "execute_manifested_projection_work(delete)",
            "completion namespace chmod 0555 during immutable completion publication",
        ),
        "deletion_catalog_publication" => (
            "execute_manifested_projection_work(delete)",
            "projection work catalog directory chmod 0555 during mark_completed",
        ),
        "forensic_salvage_rebuild" => (
            "SqliteFrontier::open_or_rebuild",
            "after first forensic move before rebuild (subprocess abort hook)",
        ),
        "sigkill_attempt_publication" => (
            "ProjectionReceiptStore::begin_mutation",
            "after publish_immutable_exact(attempt reservation), parent SIGKILL",
        ),
        "graph_wide_foreign_edit_rejection" => (
            "recover_incomplete_projections",
            "after graph-wide projection intent before recovery with foreign bytes present",
        ),
        other => panic!("unknown crash corpus case {other}"),
    }
}

fn capture_scan_measurements(measured: &mut Measurements) {
    let ProjectionStoreTestCounters {
        completion_lookups,
        catalog_directory_entries,
        ..
    } = projection_store_test_counters();
    measured.completion_lookups = completion_lookups;
    measured.catalog_directory_entries = catalog_directory_entries;
}

fn assertion_receipt(
    case: &CorpusCase,
    before: &str,
    after: &str,
    measured: Measurements,
) -> String {
    CaseReceipt {
        id: case.id.clone(),
        cut: case.cut.clone(),
        before: before.to_owned(),
        after: after.to_owned(),
        measured,
    }
    .render()
}

fn run_attempt_authority_publication(case: &CorpusCase) -> CaseReceipt {
    let fixture = Fixture::new("crash-corpus-attempt-publication");
    let mut stable = None;
    for _ in 0..4 {
        ATTEMPT_PUBLICATION_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|| panic!("crash corpus attempt cut")));
        });
        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fixture.store.begin_mutation(&fixture.intent, None)
        }));
        assert!(
            interrupted.is_err(),
            "attempt publication hook did not interrupt"
        );
        let snapshot = fixture.attempt_snapshot(&fixture.intent);
        if let Some(expected) = &stable {
            assert_eq!(&snapshot, expected, "attempt reservation bytes drifted");
        } else {
            stable = Some(snapshot);
        }
    }

    let before = format!(
        "attempts={:?},authority={:?}",
        fixture.attempt_stats(&fixture.intent),
        fixture.authority_stats()
    );
    let reopened = fixture.reopen_store();
    reset_projection_graph_test_counters();
    let reservation = reopened.reserve_attempt(&fixture.intent).unwrap();
    let mut authority = reopened
        .begin_mutation(&fixture.intent, Some(&reservation))
        .unwrap();
    let proof = fixture
        .graph
        .write_page_projection(
            fixture.intent.path().as_str(),
            None,
            &fixture.target,
            &mut authority,
        )
        .unwrap();
    reopened
        .publish_completion(authority, &fixture.intent, &proof)
        .unwrap();
    reset_projection_store_test_counters();
    assert!(reopened.load_completion(&fixture.intent).unwrap().is_some());
    let mut measured = Measurements {
        authority_slots: fixture.authority_stats().0,
        authority_bytes: fixture.authority_stats().1,
        attempt_slots: fixture.attempt_stats(&fixture.intent).0,
        ..Measurements::default()
    };
    capture_graph_measurements(&mut measured);
    capture_scan_measurements(&mut measured);
    let after = format!(
        "completion=true,attempts={:?},authority={:?}",
        fixture.attempt_stats(&fixture.intent),
        fixture.authority_stats()
    );
    assert!(
        measured.attempt_slots == 1
            && measured.authority_slots == 0
            && measured.projection_write_calls == 1
            && measured.projection_remove_calls == 0
            && measured.projection_recovery_calls == 0
            && measured.completion_lookups == 1
            && measured.catalog_directory_entries == 0,
        "{}",
        assertion_receipt(case, &before, &after, measured)
    );
    CaseReceipt {
        id: case.id.clone(),
        cut: case.cut.clone(),
        before,
        after,
        measured,
    }
}

fn run_pregraph_drop_retry(case: &CorpusCase) -> CaseReceipt {
    let fixture = Fixture::new("crash-corpus-pregraph-drop");
    let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
    let stable_attempts = fixture.attempt_snapshot(&fixture.intent);
    let authority_path = fixture.authority_path(&fixture.intent);
    let mut stable_authority = None;
    for iteration in 0..6 {
        let authority = if iteration % 2 == 0 {
            fixture
                .store
                .begin_mutation(&fixture.intent, Some(&reservation))
                .unwrap()
        } else {
            fixture.store.begin_mutation(&fixture.intent, None).unwrap()
        };
        let bytes = fs::read(&authority_path).unwrap();
        if let Some(expected) = &stable_authority {
            assert_eq!(&bytes, expected, "pregraph authority bytes drifted");
        } else {
            stable_authority = Some(bytes);
        }
        drop(authority);
        assert!(
            !authority_path.exists(),
            "pregraph drop retained a new authority slot"
        );
        assert_eq!(fixture.attempt_snapshot(&fixture.intent), stable_attempts);
    }
    let before = format!(
        "attempts={:?},authority={:?}",
        fixture.attempt_stats(&fixture.intent),
        fixture.authority_stats()
    );
    reset_projection_graph_test_counters();
    reset_projection_store_test_counters();
    let mut authority = fixture
        .store
        .begin_mutation(&fixture.intent, Some(&reservation))
        .unwrap();
    let proof = fixture
        .graph
        .write_page_projection(
            fixture.intent.path().as_str(),
            None,
            &fixture.target,
            &mut authority,
        )
        .unwrap();
    fixture
        .store
        .publish_completion(authority, &fixture.intent, &proof)
        .unwrap();
    let measured = Measurements {
        authority_slots: fixture.authority_stats().0,
        authority_bytes: fixture.authority_stats().1,
        attempt_slots: fixture.attempt_stats(&fixture.intent).0,
        ..Measurements::default()
    };
    let mut measured = measured;
    capture_graph_measurements(&mut measured);
    capture_scan_measurements(&mut measured);
    let after = format!(
        "completion=true,attempts={:?},authority={:?}",
        fixture.attempt_stats(&fixture.intent),
        fixture.authority_stats()
    );
    assert!(
        measured.attempt_slots == 1
            && measured.authority_slots == 0
            && measured.projection_write_calls == 1
            && measured.projection_remove_calls == 0
            && measured.projection_recovery_calls == 0
            && measured.completion_lookups == 0
            && measured.catalog_directory_entries == 0,
        "{}",
        assertion_receipt(case, &before, &after, measured)
    );
    CaseReceipt {
        id: case.id.clone(),
        cut: case.cut.clone(),
        before,
        after,
        measured,
    }
}

fn run_interrupted_recovery_slot_reuse(case: &CorpusCase) -> CaseReceipt {
    let fixture = Fixture::new_replacement("crash-corpus-interrupted-recovery");
    let base = b"- base\n";
    let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
    let recovery_path = fixture
        .graph_root
        .join("pages")
        .join(reservation.recovery_filename());
    let mut authority = fixture
        .store
        .begin_mutation(&fixture.intent, Some(&reservation))
        .unwrap();
    fixture
        .graph
        .write_page_projection(
            fixture.intent.path().as_str(),
            Some(base),
            &fixture.target,
            &mut authority,
        )
        .unwrap();
    drop(authority);
    assert_eq!(fs::read(&recovery_path).unwrap(), base);
    assert_eq!(
        fixture.store.pending_projection_cleanup().unwrap().len(),
        1,
        "exact handle-bound cleanup marker was not durable before completion"
    );
    assert!(fixture
        .store
        .load_completion(&fixture.intent)
        .unwrap()
        .is_none());
    let authority_path = fixture.authority_path(&fixture.intent);
    let stable_authority = fs::read(&authority_path).unwrap();
    let stable_stats = fixture.authority_stats();
    let stable_attempts = fixture.attempt_stats(&fixture.intent);
    let before = format!("attempts={stable_attempts:?},authority={stable_stats:?}");

    reset_projection_graph_test_counters();
    reset_projection_store_test_counters();
    for _ in 0..4 {
        let reopened = fixture.reopen_store();
        let mut recovery = reopened.begin_mutation(&fixture.intent, None).unwrap();
        fixture
            .graph
            .recover_page_projection(
                fixture.intent.path().as_str(),
                Some(base),
                &fixture.target,
                &mut recovery,
            )
            .unwrap();
        drop(recovery);
        assert_eq!(
            fs::read(&recovery_path).unwrap(),
            base,
            "interrupted attempt retired recovery evidence before completion"
        );
        assert_eq!(fixture.authority_stats(), stable_stats);
        assert_eq!(fixture.attempt_stats(&fixture.intent), stable_attempts);
        assert_eq!(fs::read(&authority_path).unwrap(), stable_authority);
    }
    let reopened = fixture.reopen_store();
    let mut recovery = reopened.begin_mutation(&fixture.intent, None).unwrap();
    let proof = fixture
        .graph
        .recover_page_projection(
            fixture.intent.path().as_str(),
            Some(base),
            &fixture.target,
            &mut recovery,
        )
        .unwrap();
    reopened
        .publish_completion(recovery, &fixture.intent, &proof)
        .unwrap();
    let (_, record) = reopened
        .pending_projection_cleanup()
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(
        fixture
            .graph
            .retire_completed_projection_recovery(
                fixture.intent.path().as_str(),
                std::slice::from_ref(&record),
                None,
            )
            .unwrap(),
        ProjectionRecoveryCleanup::Quarantined
    );
    assert!(
        !recovery_path.exists(),
        "durably completed attempt retained its displaced source"
    );
    assert_eq!(
        fixture
            .graph
            .retire_completed_projection_recovery(
                fixture.intent.path().as_str(),
                std::slice::from_ref(&record),
                Some(&ProjectionCleanupRetirementAuthority::for_test(&record)),
            )
            .unwrap(),
        ProjectionRecoveryCleanup::Retired
    );
    reopened
        .retire_pending_projection_cleanup(&record)
        .unwrap();
    let measured = Measurements {
        authority_slots: fixture.authority_stats().0,
        authority_bytes: fixture.authority_stats().1,
        attempt_slots: fixture.attempt_stats(&fixture.intent).0,
        ..Measurements::default()
    };
    let mut measured = measured;
    capture_graph_measurements(&mut measured);
    capture_scan_measurements(&mut measured);
    let after = format!(
        "completion=true,attempts={:?},authority={:?}",
        fixture.attempt_stats(&fixture.intent),
        fixture.authority_stats()
    );
    assert!(
        measured.attempt_slots == 1
            && measured.authority_slots == 0
            && measured.projection_write_calls == 0
            && measured.projection_remove_calls == 0
            && measured.projection_recovery_calls == 5
            && measured.completion_lookups == 0
            && measured.catalog_directory_entries == 0,
        "{}",
        assertion_receipt(case, &before, &after, measured)
    );
    CaseReceipt {
        id: case.id.clone(),
        cut: case.cut.clone(),
        before,
        after,
        measured,
    }
}

fn run_completion_retained_slot(case: &CorpusCase) -> CaseReceipt {
    let fixture = Fixture::new_replacement("crash-corpus-completion-retained-slot");
    let base = b"- base\n";
    reset_projection_graph_test_counters();
    let reservation = fixture.store.reserve_attempt(&fixture.intent).unwrap();
    let recovery_path = fixture
        .graph_root
        .join("pages")
        .join(reservation.recovery_filename());
    let mut authority = fixture
        .store
        .begin_mutation(&fixture.intent, Some(&reservation))
        .unwrap();
    let proof = fixture
        .graph
        .write_page_projection(
            fixture.intent.path().as_str(),
            Some(base),
            &fixture.target,
            &mut authority,
        )
        .unwrap();
    COMPLETION_RETAINED_SLOT_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|| panic!("crash corpus retained completion slot")));
    });
    let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fixture
            .store
            .publish_completion(authority, &fixture.intent, &proof)
    }));
    assert!(
        interrupted.is_err(),
        "completion retained-slot hook did not interrupt"
    );
    let completion_path = fixture
        .store
        .root_path()
        .join(COMPLETIONS_DIR)
        .join(completion_filename(fixture.intent.id().unwrap()));
    let authority_path = fixture.authority_path(&fixture.intent);
    assert!(completion_path.exists() && authority_path.exists());
    assert_eq!(
        fs::read(&recovery_path).unwrap(),
        base,
        "completion cut lost recovery evidence before restart reconciliation"
    );
    let before = format!(
        "completion_bytes={},authority={:?}",
        fs::metadata(&completion_path).unwrap().len(),
        fixture.authority_stats()
    );

    let reopened = fixture.reopen_store();
    reset_projection_store_test_counters();
    assert!(reopened.load_completion(&fixture.intent).unwrap().is_some());
    assert!(
        !authority_path.exists(),
        "reopen did not reconcile retained authority slot"
    );
    let (_, record) = reopened
        .pending_projection_cleanup()
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(
        fixture
            .graph
            .retire_completed_projection_recovery(
                fixture.intent.path().as_str(),
                std::slice::from_ref(&record),
                None,
            )
            .unwrap(),
        ProjectionRecoveryCleanup::Quarantined
    );
    assert_eq!(
        fixture
            .graph
            .retire_completed_projection_recovery(
                fixture.intent.path().as_str(),
                std::slice::from_ref(&record),
                Some(&ProjectionCleanupRetirementAuthority::for_test(&record)),
            )
            .unwrap(),
        ProjectionRecoveryCleanup::Retired
    );
    reopened
        .retire_pending_projection_cleanup(&record)
        .unwrap();
    assert!(
        !recovery_path.exists(),
        "restart did not retire completed-attempt recovery evidence"
    );
    assert!(reopened.load_completion(&fixture.intent).unwrap().is_some());
    let catalog = reopened.validated_catalog().unwrap();
    assert_eq!(catalog.len(), 1);
    let mut measured = Measurements {
        authority_slots: fixture.authority_stats().0,
        authority_bytes: fixture.authority_stats().1,
        attempt_slots: fixture.attempt_stats(&fixture.intent).0,
        ..Measurements::default()
    };
    capture_graph_measurements(&mut measured);
    capture_scan_measurements(&mut measured);
    let after = format!(
        "catalog_rows={},authority={:?}",
        catalog.len(),
        fixture.authority_stats()
    );
    assert!(
        measured.authority_slots == 0
            && measured.attempt_slots == 1
            && measured.projection_write_calls == 1
            && measured.projection_remove_calls == 0
            && measured.projection_recovery_calls == 0
            && measured.completion_lookups == 2
            && measured.catalog_directory_entries == 2,
        "{}",
        assertion_receipt(case, &before, &after, measured)
    );
    CaseReceipt {
        id: case.id.clone(),
        cut: case.cut.clone(),
        before,
        after,
        measured,
    }
}

fn corpus_uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn corpus_workspace(value: u128) -> WorkspaceId {
    WorkspaceId::from_uuid(corpus_uuid(value))
}

fn corpus_binding(graph: &Graph, seed: u128) -> ProjectionEndpointBinding {
    ProjectionEndpointBinding::enroll_graph(
        graph,
        ProjectionEndpointId::from_uuid(corpus_uuid(seed)),
        DeviceId::from_uuid(corpus_uuid(seed + 1)),
    )
    .unwrap()
}

fn corpus_authorized_engine(
    dir: &CorpusDir,
    relative_path: &str,
    content: &str,
    enrollment: Option<(&Graph, &crate::oplog::ProjectionReceiptStore)>,
) -> (ShardedHotEngine, PageId) {
    let workspace_id = corpus_workspace(1);
    let lineage = LineageDigest::of(b"crash-corpus-projection-lineage");
    let catalog = DocumentId::from_uuid(corpus_uuid(700));
    let page_id = PageId::from_uuid(corpus_uuid(701));
    let home = DocumentId::from_uuid(corpus_uuid(702));
    let block_id = BlockId::from_uuid(corpus_uuid(703));
    let batch_id = BatchId::from_uuid(corpus_uuid(704));
    let author = ShardedHotEngine::new(workspace_id, lineage, catalog);
    let transaction = OperationTransaction::new(vec![
        SemanticOperation::CreatePage {
            page_id,
            home_document_id: home,
            name: crate::oplog::LogicalPageName::parse("Crash Corpus Projection").unwrap(),
            path: ManagedPath::parse(relative_path).unwrap(),
            kind: ManagedTextKind::Page,
        },
        SemanticOperation::CreateBlock {
            block: BlockLocation {
                block_id,
                home_document_id: home,
            },
            page_id,
            parent: None,
            order: "a".into(),
            content: content.into(),
        },
    ])
    .unwrap();
    let prepared = author
        .prepare_bootstrap_transaction(
            AuthorBatch {
                batch_id,
                author_device_id: DeviceId::from_uuid(corpus_uuid(705)),
                author_session_id: SessionId::from_uuid(corpus_uuid(706)),
                crdt_peer_id: CrdtPeerId::from_u64(707),
            },
            &transaction,
        )
        .unwrap();
    let archive_path = dir.path().join("archive");
    let writer = ObjectStore::open(&archive_path, workspace_id).unwrap();
    writer.publish_bootstrap_prepared_for_test(&prepared).unwrap();
    drop(writer);
    let reader = ObjectStore::open(&archive_path, workspace_id).unwrap();
    let mut engine = match enrollment {
        Some((graph, receipts)) => {
            ShardedHotEngine::with_enrolled_projection(reader, lineage, catalog, graph, receipts)
        }
        None => ShardedHotEngine::with_archive_store(reader, lineage, catalog),
    };
    assert!(matches!(
        engine.stage_archive_batch(batch_id).unwrap().disposition(),
        BatchDisposition::Accepted { .. }
    ));
    (engine, page_id)
}

#[allow(clippy::type_complexity)]
fn corpus_manifested_deletion_fixture(
    label: &str,
    deleted_page: &SemanticFixturePage,
) -> (
    CorpusDir,
    Graph,
    crate::oplog::ProjectionReceiptStore,
    ObjectStore,
    ShardedHotEngine,
    ProjectionEndpointBinding,
    crate::oplog::ProjectionWork,
) {
    assert!(deleted_page.deleted);
    assert_eq!(deleted_page.kind.managed_kind(), ManagedTextKind::Page);
    let document = fixture_document(deleted_page);
    assert!(document.pre_block.is_none());
    assert_eq!(document.roots.len(), 1);
    assert!(document.roots[0].children.is_empty());
    let deleted_content = document.roots[0].raw.clone();
    let dir = CorpusDir::new(label);
    let graph_root = dir.path().join("graph");
    fs::create_dir_all(graph_root.join("pages")).unwrap();
    fs::create_dir_all(graph_root.join("journals")).unwrap();
    let graph = Graph::open(&graph_root);
    let binding = corpus_binding(&graph, 70_000);
    let receipts = crate::oplog::ProjectionReceiptStore::open_for_endpoint(
        &dir.path().join("receipts"),
        corpus_workspace(1),
        binding,
    )
    .unwrap();
    let (archive_seed, page_id) =
        corpus_authorized_engine(&dir, &deleted_page.path, &deleted_content, None);
    drop(archive_seed);

    let archive_path = dir.path().join("archive");
    let writer = ObjectStore::open(&archive_path, corpus_workspace(1)).unwrap();
    let reader = ObjectStore::open(&archive_path, corpus_workspace(1)).unwrap();
    let mut engine = ShardedHotEngine::with_enrolled_projection(
        reader,
        LineageDigest::of(b"crash-corpus-projection-lineage"),
        DocumentId::from_uuid(corpus_uuid(700)),
        &graph,
        &receipts,
    );
    assert!(matches!(
        engine
            .stage_archive_batch(BatchId::from_uuid(corpus_uuid(704)))
            .unwrap()
            .disposition(),
        BatchDisposition::Accepted { .. }
    ));
    let initial_write =
        crate::oplog::write_projection_exact(&graph, &receipts, &engine, page_id, None).unwrap();
    assert_eq!(initial_write.plan.target(), deleted_page.bytes.as_bytes());
    let batch_id = BatchId::from_uuid(corpus_uuid(70_010));
    let draft = engine
        .draft_author_transaction(
            AuthorBatch {
                batch_id,
                author_device_id: binding.device_id(),
                author_session_id: SessionId::from_uuid(corpus_uuid(70_011)),
                crdt_peer_id: CrdtPeerId::from_u64(70_012),
            },
            BatchOrigin::LocalMutation,
            &OperationTransaction::new(vec![SemanticOperation::DeletePage { page_id }]).unwrap(),
        )
        .unwrap();
    let prepared = engine
        .finalize_author_transaction(draft, &graph, &receipts, binding)
        .unwrap();
    writer.publish_prepared(&prepared).unwrap();
    assert!(matches!(
        engine.stage_archive_batch(batch_id).unwrap().disposition(),
        BatchDisposition::Accepted { .. }
    ));
    let work = engine
        .projection_work_index()
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    (dir, graph, receipts, writer, engine, binding, work)
}

#[derive(Clone, Copy)]
enum ReceiverInitialTarget {
    Absent,
    Projected,
    ProjectedThenRemoved,
}

struct CorpusProviderReceiver {
    graph_root: PathBuf,
    graph: Graph,
    receipts: crate::oplog::ProjectionReceiptStore,
    engine: ShardedHotEngine,
    binding: ProjectionEndpointBinding,
}

fn manifested_intent_for_work(
    archive: &ObjectStore,
    work: &crate::oplog::ProjectionWork,
) -> ManifestedProjectionIntent {
    match archive.inspect_batch(work.batch_id()).unwrap() {
        BatchInspection::Ready(batch) => batch
            .objects()
            .iter()
            .find(|object| {
                object.kind() == ObjectKind::ProjectionIntent
                    && object.document_id() == work.intent().document_id()
            })
            .map(|object| ManifestedProjectionIntent::decode(object.payload()).unwrap())
            .expect("manifested projection work intent"),
        other => panic!("projection work batch is not ready: {other:?}"),
    }
}

/// Build an isolated receiver from the same immutable provider payloads used
/// by the crash corpus. Archive copying stays behind this shared two-device
/// boundary so receiver scenarios differ only in their semantic initial
/// projection target.
fn corpus_provider_receiver(
    dir: &CorpusDir,
    source: &ManifestedProjectionIntent,
    deletion_batch_id: BatchId,
    initial_target: ReceiverInitialTarget,
) -> CorpusProviderReceiver {
    let workspace = corpus_workspace(1);
    let archive_root = dir.path().join(format!(
        "receiver-archive-{}",
        match initial_target {
            ReceiverInitialTarget::Absent => "absent",
            ReceiverInitialTarget::Projected => "projected",
            ReceiverInitialTarget::ProjectedThenRemoved => "projected-then-removed",
        }
    ));
    drop(ObjectStore::open(&archive_root, workspace).unwrap());
    for namespace in ["objects", "batches"] {
        copy_regular_tree(
            &dir.path().join("archive").join(namespace),
            &archive_root.join(namespace),
        );
    }
    let receiver_archive = ObjectStore::open(&archive_root, workspace).unwrap();
    assert!(
        receiver_archive.committed_manifests().unwrap().len() >= 2,
        "provider fixture requires create then delete"
    );
    drop(receiver_archive);

    let graph_root = dir.path().join(format!(
        "receiver-graph-{}",
        match initial_target {
            ReceiverInitialTarget::Absent => "absent",
            ReceiverInitialTarget::Projected => "projected",
            ReceiverInitialTarget::ProjectedThenRemoved => "projected-then-removed",
        }
    ));
    fs::create_dir_all(graph_root.join("pages")).unwrap();
    fs::create_dir_all(graph_root.join("journals")).unwrap();
    fs::create_dir_all(
        graph_root.join(
            Path::new(source.path().as_str())
                .parent()
                .expect("managed projection parent"),
        ),
    )
    .unwrap();
    let graph = Graph::open(&graph_root);
    let binding = corpus_binding(
        &graph,
        match initial_target {
            ReceiverInitialTarget::Absent => 75_000,
            ReceiverInitialTarget::Projected => 76_000,
            ReceiverInitialTarget::ProjectedThenRemoved => 77_000,
        },
    );
    assert_ne!(binding.endpoint_id(), source.source_endpoint_id());
    let receipts = crate::oplog::ProjectionReceiptStore::open_for_endpoint(
        &dir.path().join(format!(
            "receiver-receipts-{}",
            match initial_target {
                ReceiverInitialTarget::Absent => "absent",
                ReceiverInitialTarget::Projected => "projected",
                ReceiverInitialTarget::ProjectedThenRemoved => "projected-then-removed",
            }
        )),
        workspace,
        binding,
    )
    .unwrap();
    let mut engine = ShardedHotEngine::with_enrolled_projection(
        ObjectStore::open(&archive_root, workspace).unwrap(),
        LineageDigest::of(b"crash-corpus-projection-lineage"),
        DocumentId::from_uuid(corpus_uuid(700)),
        &graph,
        &receipts,
    );
    assert!(matches!(
        engine
            .stage_archive_batch(BatchId::from_uuid(corpus_uuid(704)))
            .unwrap()
            .disposition(),
        BatchDisposition::Accepted { .. }
    ));
    if matches!(
        initial_target,
        ReceiverInitialTarget::Projected | ReceiverInitialTarget::ProjectedThenRemoved
    ) {
        crate::oplog::write_projection_exact(
            &graph,
            &receipts,
            &engine,
            source.page_id(),
            None,
        )
        .unwrap();
    }
    if matches!(initial_target, ReceiverInitialTarget::ProjectedThenRemoved) {
        fs::remove_file(graph_root.join(source.path().as_str())).unwrap();
    }
    assert!(matches!(
        engine
            .stage_archive_batch(deletion_batch_id)
            .unwrap()
            .disposition(),
        BatchDisposition::Accepted { .. }
    ));
    CorpusProviderReceiver {
        graph_root,
        graph,
        receipts,
        engine,
        binding,
    }
}

#[cfg(unix)]
fn readonly_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path).unwrap().permissions().mode()
}

#[cfg(unix)]
fn set_readonly(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = readonly_mode(path);
    fs::set_permissions(path, fs::Permissions::from_mode(mode & !0o222)).unwrap();
    assert!(fs::metadata(path).unwrap().permissions().readonly());
    mode
}

#[cfg(unix)]
fn restore_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Does clearing a directory's write bits actually deny writes to THIS user?
///
/// Root ignores directory permissions, so a `set_readonly` cut silently
/// succeeds instead of failing and the case asserts against a boundary that was
/// never established. Probe the real behaviour rather than testing for uid 0:
/// the same answer is what capability-restricted and unusual-filesystem
/// environments need, and a vacuous pass here would quietly stop proving the
/// durability contract.
#[cfg(unix)]
fn directory_permissions_are_enforced() -> bool {
    let probe = CorpusDir::new("permission-probe");
    let directory = probe.path().join("denied");
    fs::create_dir(&directory).unwrap();
    let mode = set_readonly(&directory);
    let denied = fs::write(directory.join("probe"), b"probe").is_err();
    restore_mode(&directory, mode);
    denied
}

fn deleted_fixture_page(manifest: &CorpusManifest) -> &SemanticFixturePage {
    let pages = manifest
        .pages
        .iter()
        .filter(|page| page.deleted)
        .collect::<Vec<_>>();
    assert_eq!(pages.len(), 1, "fixture must declare one deleted page");
    pages[0]
}

fn deletion_completion_published(
    receipts: &crate::oplog::ProjectionReceiptStore,
    work: &crate::oplog::ProjectionWork,
) -> bool {
    let catalog = receipts.validated_catalog().unwrap();
    let rows = catalog
        .iter()
        .filter(|entry| {
            entry.intent.page_id() == work.page_id()
                && entry.intent.path() == work.path()
                && entry.intent.frontier() == work.post_frontier()
                && entry.intent.target() == BlobDescription::of(&[])
        })
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "delete work must own one local receipt intent");
    rows[0].completion.is_some()
}

#[cfg(unix)]
fn run_deletion_completion_publication(case: &CorpusCase) -> CaseReceipt {
    let manifest = corpus_manifest();
    let page = deleted_fixture_page(&manifest);
    let (dir, graph, receipts, _writer, mut engine, _binding, work) =
        corpus_manifested_deletion_fixture("completion-permission", page);
    let target = dir.path().join("graph").join(&page.path);
    let completion_dir = receipts.root_path().join(COMPLETIONS_DIR);
    let mode = set_readonly(&completion_dir);
    let first = execute_manifested_projection_work(&graph, &receipts, &mut engine, &work);
    restore_mode(&completion_dir, mode);
    assert!(
        first.is_err(),
        "completion directory permission cut unexpectedly succeeded"
    );
    assert!(
        !target.exists(),
        "delete target was not durably removed before completion cut"
    );
    assert!(
        !deletion_completion_published(&receipts, &work),
        "completion was published despite the completion-namespace failure"
    );
    assert_eq!(
        engine
            .projection_work_index()
            .unwrap()
            .status(work.work_id())
            .unwrap(),
        Some(ProjectionWorkStatus::Ready),
        "work advanced despite failed completion publication"
    );
    let before = format!(
        "target=absent,completion=false,status={:?}",
        engine
            .projection_work_index()
            .unwrap()
            .status(work.work_id())
            .unwrap()
    );
    reset_projection_store_test_counters();
    reset_projection_graph_test_counters();
    execute_manifested_projection_work(&graph, &receipts, &mut engine, &work).unwrap();
    assert!(!target.exists());
    assert!(deletion_completion_published(&receipts, &work));
    assert_eq!(
        engine
            .projection_work_index()
            .unwrap()
            .status(work.work_id())
            .unwrap(),
        Some(ProjectionWorkStatus::Completed)
    );
    let mut measured = Measurements::default();
    capture_graph_measurements(&mut measured);
    capture_scan_measurements(&mut measured);
    let after = format!(
        "target=absent,completion=true,status={:?}",
        engine
            .projection_work_index()
            .unwrap()
            .status(work.work_id())
            .unwrap()
    );
    assert!(
        measured.projection_write_calls == 0
            && measured.projection_remove_calls == 0
            && measured.projection_recovery_calls == 1
            && measured.completion_lookups == 4
            && measured.catalog_directory_entries == 4,
        "{}",
        assertion_receipt(case, &before, &after, measured)
    );
    CaseReceipt {
        id: case.id.clone(),
        cut: case.cut.clone(),
        before,
        after,
        measured,
    }
}

#[cfg(not(unix))]
fn run_deletion_completion_publication(case: &CorpusCase) -> CaseReceipt {
    panic!("{} requires a real readonly permission boundary", case.id)
}

#[cfg(unix)]
fn run_deletion_catalog_publication(case: &CorpusCase) -> CaseReceipt {
    let manifest = corpus_manifest();
    let page = deleted_fixture_page(&manifest);
    let (dir, graph, receipts, _writer, mut engine, binding, work) =
        corpus_manifested_deletion_fixture("catalog-permission", page);
    let target = dir.path().join("graph").join(&page.path);
    let work_dir = dir
        .path()
        .join("archive/projection-work-index-v1")
        .join(binding.endpoint_id().to_string());
    let mode = set_readonly(&work_dir);
    let first = execute_manifested_projection_work(&graph, &receipts, &mut engine, &work);
    restore_mode(&work_dir, mode);
    assert!(
        first.is_err(),
        "work catalog permission cut unexpectedly succeeded"
    );
    assert!(!target.exists());
    assert!(deletion_completion_published(&receipts, &work));
    assert_eq!(
        engine
            .projection_work_index()
            .unwrap()
            .status(work.work_id())
            .unwrap(),
        Some(ProjectionWorkStatus::Ready)
    );
    let before = "completion=true,target=absent,status=ready".to_owned();
    reset_projection_store_test_counters();
    reset_projection_graph_test_counters();
    execute_manifested_projection_work(&graph, &receipts, &mut engine, &work).unwrap();
    assert!(!target.exists());
    assert!(deletion_completion_published(&receipts, &work));
    assert_eq!(
        engine
            .projection_work_index()
            .unwrap()
            .status(work.work_id())
            .unwrap(),
        Some(ProjectionWorkStatus::Completed)
    );
    let catalog = receipts.validated_catalog().unwrap();
    let mut measured = Measurements::default();
    capture_graph_measurements(&mut measured);
    capture_scan_measurements(&mut measured);
    let after = format!(
        "catalog_rows={},target=absent,status=completed",
        catalog.len()
    );
    assert!(
        measured.projection_write_calls == 0
            && measured.projection_remove_calls == 0
            && measured.projection_recovery_calls == 0
            && measured.completion_lookups == 4
            && measured.catalog_directory_entries == 8,
        "{}",
        assertion_receipt(case, &before, &after, measured)
    );
    CaseReceipt {
        id: case.id.clone(),
        cut: case.cut.clone(),
        before,
        after,
        measured,
    }
}

#[cfg(not(unix))]
fn run_deletion_catalog_publication(case: &CorpusCase) -> CaseReceipt {
    panic!("{} requires a real readonly permission boundary", case.id)
}

#[derive(Clone, Copy)]
struct ForensicIds {
    workspace: WorkspaceId,
    lineage: LineageDigest,
    catalog: DocumentId,
    document: DocumentId,
    page: PageId,
    block: BlockId,
}

impl ForensicIds {
    fn new() -> Self {
        Self {
            workspace: corpus_workspace(91_001),
            lineage: LineageDigest::of(b"crash-corpus-forensic-lineage"),
            catalog: DocumentId::from_uuid(corpus_uuid(91_002)),
            document: DocumentId::from_uuid(corpus_uuid(91_003)),
            page: PageId::from_uuid(corpus_uuid(91_004)),
            block: BlockId::from_uuid(corpus_uuid(91_005)),
        }
    }

    fn claim(self) -> ProjectionClaim {
        ProjectionClaim::current(self.workspace, self.lineage)
    }

    fn engine(self) -> ShardedHotEngine {
        ShardedHotEngine::new(self.workspace, self.lineage, self.catalog)
    }
}

fn forensic_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn build_forensic_accepted_engine(
    root: &Path,
) -> (ForensicIds, ObjectStore, ShardedHotEngine, PathBuf) {
    let ids = ForensicIds::new();
    let store = ObjectStore::open(&root.join("objects"), ids.workspace).unwrap();
    let author = ids.engine();
    let transaction = OperationTransaction::new(vec![
        SemanticOperation::CreatePage {
            page_id: ids.page,
            home_document_id: ids.document,
            name: crate::oplog::LogicalPageName::parse("rebuild").unwrap(),
            path: ManagedPath::parse("pages/salvage/rebuild.md").unwrap(),
            kind: ManagedTextKind::Page,
        },
        SemanticOperation::CreateBlock {
            block: BlockLocation {
                block_id: ids.block,
                home_document_id: ids.document,
            },
            page_id: ids.page,
            parent: None,
            order: "a".into(),
            content: "salvage rebuild".into(),
        },
    ])
    .unwrap();
    let batch_id = BatchId::from_uuid(corpus_uuid(91_006));
    let prepared = author
        .prepare_bootstrap_transaction(
            AuthorBatch {
                batch_id,
                author_device_id: DeviceId::from_uuid(corpus_uuid(91_007)),
                author_session_id: SessionId::from_uuid(corpus_uuid(91_008)),
                crdt_peer_id: CrdtPeerId::from_u64(91_009),
            },
            &transaction,
        )
        .unwrap();
    store.publish_bootstrap_prepared_for_test(&prepared).unwrap();
    let mut accepted_engine = ids.engine();
    assert!(matches!(
        accepted_engine
            .stage_from_store(&store, batch_id)
            .unwrap()
            .disposition(),
        BatchDisposition::Accepted { .. }
    ));
    (ids, store, accepted_engine, root.join("frontier.sqlite"))
}

fn write_marker(path: &Path) {
    fs::write(path, b"ready").unwrap();
    fs::File::open(path).unwrap().sync_all().unwrap();
}

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for crash corpus marker {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_worker(mode: &str, root: &Path) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(WORKER_TEST_NAME)
        .arg("--nocapture")
        .env("TINE_PROJECTION_CRASH_CORPUS_WORKER", mode)
        .env("TINE_PROJECTION_CRASH_CORPUS_ROOT", root)
        .env("XDG_DATA_HOME", root.join("xdg"));
    if mode == "forensic-crash" {
        command.env("TINE_SQLITE_FORENSIC_ABORT", "after-move:1");
    }
    command.spawn().unwrap()
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ForensicEvidenceReceipt {
    sidecar: String,
    preserved_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RebuiltPageReceipt {
    page_id: PageId,
    kind: ManagedTextKind,
    path: String,
    content: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ForensicWorkerReceipt {
    preserved_sidecars: Vec<ForensicEvidenceReceipt>,
    evidence_bytes: u64,
    applied_batches: usize,
    rebuilt_pages: Vec<RebuiltPageReceipt>,
    accepted_events_validated: usize,
    accepted_events_applied: usize,
    accepted_sequence_page_reads: usize,
}

fn forensic_sidecar_path(path: &Path, sidecar: &str) -> PathBuf {
    match sidecar {
        "database" => path.to_path_buf(),
        "wal" => forensic_sidecar(path, "-wal"),
        "shm" => forensic_sidecar(path, "-shm"),
        "auth" => forensic_sidecar(path, "-auth"),
        other => panic!("unknown forensic sidecar declaration {other}"),
    }
}

fn forensic_sidecar_bytes(sidecar: &str) -> &'static [u8] {
    match sidecar {
        "database" => b"corrupt SQLite evidence",
        "wal" => b"partial wal",
        "shm" => b"partial shm",
        "auth" => b"partial auth",
        other => panic!("unknown forensic sidecar declaration {other}"),
    }
}

fn forensic_crash_worker(root: &Path) {
    let (ids, store, accepted_engine, path) = build_forensic_accepted_engine(root);
    let runtime = ApplicationRuntimeRoot::open().unwrap();
    let empty = ids.engine();
    drop(
        SqliteFrontier::open_or_rebuild(
            &path,
            &runtime,
            ids.claim(),
            RebuildSource::new(&empty, &store).unwrap(),
        )
        .unwrap(),
    );
    for sidecar in &corpus_manifest().salvage_rebuild.corrupt_sidecars {
        fs::write(forensic_sidecar_path(&path, sidecar), forensic_sidecar_bytes(sidecar)).unwrap();
    }
    write_marker(&root.join("forensic-ready"));
    let _ = SqliteFrontier::open_or_rebuild(
        &path,
        &runtime,
        ids.claim(),
        RebuildSource::new(&accepted_engine, &store).unwrap(),
    );
    panic!("forensic subprocess did not abort at its configured durability cut");
}

fn forensic_repair_worker(root: &Path) {
    let (ids, store, accepted_engine, path) = build_forensic_accepted_engine(root);
    let runtime = ApplicationRuntimeRoot::open().unwrap();
    let reopened = SqliteFrontier::open_or_rebuild(
        &path,
        &runtime,
        ids.claim(),
        RebuildSource::new(&accepted_engine, &store).unwrap(),
    )
    .unwrap();
    let ProjectionRecovery::RebuiltPreservingEvidence { evidence, .. } = &reopened.recovery else {
        panic!("forensic repair did not rebuild preserved evidence");
    };
    let manifest = corpus_manifest();
    let mut preserved_sidecars = Vec::new();
    for sidecar in &manifest.salvage_rebuild.corrupt_sidecars {
        let original_path = forensic_sidecar_path(&path, sidecar);
        let matching = evidence
            .iter()
            .filter(|item| item.original_path == original_path)
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1, "missing preserved {sidecar} evidence");
        let item = matching[0];
        assert_ne!(item.preserved_path, item.original_path);
        assert!(item.preserved_path.exists());
        let preserved_bytes = fs::read(&item.preserved_path).unwrap();
        assert_eq!(preserved_bytes, forensic_sidecar_bytes(sidecar));
        preserved_sidecars.push(ForensicEvidenceReceipt {
            sidecar: sidecar.clone(),
            preserved_bytes,
        });
    }
    assert_eq!(evidence.len(), preserved_sidecars.len());
    let effects = reopened.database.applied_semantic_effects_for_test().unwrap();
    let mut rebuilt_pages = Vec::new();
    for effect in effects {
        for delta in effect.pages() {
            let Some(PageState::Live { path, kind, .. }) = &delta.after else {
                continue;
            };
            let content = effect
                .blocks()
                .iter()
                .find_map(|block| match &block.after {
                    Some(state)
                        if matches!(state.owner, BlockOwner::Page(page_id) if page_id == delta.page_id) =>
                    Some(state.content.clone()),
                    _ => None,
                })
                .expect("rebuilt page must retain its block content");
            rebuilt_pages.push(RebuiltPageReceipt {
                page_id: delta.page_id,
                kind: *kind,
                path: path.as_str().to_owned(),
                content,
            });
        }
    }
    rebuilt_pages.sort_by_key(|page| page.page_id);
    assert_eq!(
        rebuilt_pages,
        vec![RebuiltPageReceipt {
            page_id: ids.page,
            kind: ManagedTextKind::Page,
            path: "pages/salvage/rebuild.md".into(),
            content: "salvage rebuild".into(),
        }]
    );
    let receipt = ForensicWorkerReceipt {
        evidence_bytes: preserved_sidecars
            .iter()
            .map(|item| item.preserved_bytes.len() as u64)
            .sum(),
        preserved_sidecars,
        applied_batches: reopened.database.applied_batch_count().unwrap(),
        rebuilt_pages,
        accepted_events_validated: reopened.rebuild.accepted_events_validated,
        accepted_events_applied: reopened.rebuild.accepted_events_applied,
        accepted_sequence_page_reads: reopened.rebuild.accepted_sequence_page_reads,
    };
    fs::write(
        root.join("forensic-receipt.json"),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
}

#[cfg(unix)]
fn require_signal(status: std::process::ExitStatus, signal: i32, context: &str) {
    use std::os::unix::process::ExitStatusExt as _;
    assert_eq!(status.signal(), Some(signal), "{context}: {status:?}");
}

fn run_forensic_salvage_rebuild(case: &CorpusCase) -> CaseReceipt {
    let root = CorpusDir::new("forensic-salvage");
    let mut crashing = spawn_worker("forensic-crash", root.path());
    wait_for_path(&root.path().join("forensic-ready"));
    let crash_status = crashing.wait().unwrap();
    #[cfg(unix)]
    require_signal(crash_status, libc::SIGABRT, "forensic abort cut");
    #[cfg(not(unix))]
    assert!(
        !crash_status.success(),
        "forensic abort cut unexpectedly succeeded"
    );
    let before = "forensic_move=1,repair=not_started".to_owned();
    let repaired = spawn_worker("forensic-repair", root.path()).wait().unwrap();
    assert!(
        repaired.success(),
        "forensic repair subprocess failed: {repaired:?}"
    );
    let worker: ForensicWorkerReceipt =
        serde_json::from_slice(&fs::read(root.path().join("forensic-receipt.json")).unwrap())
            .unwrap();
    let manifest = corpus_manifest();
    assert_eq!(
        worker
            .preserved_sidecars
            .iter()
            .map(|item| item.sidecar.clone())
            .collect::<Vec<_>>(),
        manifest.salvage_rebuild.corrupt_sidecars
    );
    assert!(worker
        .preserved_sidecars
        .iter()
        .all(|item| item.preserved_bytes == forensic_sidecar_bytes(&item.sidecar)));
    assert_eq!(
        worker.rebuilt_pages.len(),
        manifest.salvage_rebuild.expected_rebuilt_pages
    );
    assert_eq!(
        worker.rebuilt_pages,
        vec![RebuiltPageReceipt {
            page_id: ForensicIds::new().page,
            kind: ManagedTextKind::Page,
            path: "pages/salvage/rebuild.md".into(),
            content: "salvage rebuild".into(),
        }]
    );
    let injected_evidence_bytes = manifest
        .salvage_rebuild
        .corrupt_sidecars
        .iter()
        .map(|sidecar| forensic_sidecar_bytes(sidecar).len() as u64)
        .sum::<u64>();
    let measured = Measurements {
        forensic_files: worker.preserved_sidecars.len(),
        forensic_bytes: worker.evidence_bytes,
        accepted_events_validated: worker.accepted_events_validated,
        accepted_events_applied: worker.accepted_events_applied,
        accepted_sequence_page_reads: worker.accepted_sequence_page_reads,
        ..Measurements::default()
    };
    let after = format!(
        "forensic_files={},rebuilt_pages={},applied_batches={}",
        worker.preserved_sidecars.len(),
        worker.rebuilt_pages.len(),
        worker.applied_batches
    );
    assert!(
        worker.applied_batches == 1
            && measured.forensic_files == manifest.salvage_rebuild.corrupt_sidecars.len()
            && measured.forensic_bytes == injected_evidence_bytes
            && measured.accepted_events_validated == 1
            && measured.accepted_events_applied == 1
            && measured.accepted_sequence_page_reads == 0,
        "{}",
        assertion_receipt(case, &before, &after, measured)
    );
    CaseReceipt {
        id: case.id.clone(),
        cut: case.cut.clone(),
        before,
        after,
        measured,
    }
}

fn corpus_fixture_intent(workspace: WorkspaceId) -> ProjectionIntent {
    let target = b"- target\n";
    ProjectionIntent::new(
        workspace,
        PageId::from_uuid(Uuid::from_u128(2)),
        ManagedPath::parse("pages/authority.md").unwrap(),
        FrontierV2::default(),
        Vec::new(),
        ProjectionPrecondition::Absent,
        BlobDescription::of(target),
        Vec::new(),
    )
    .unwrap()
}

fn kill_attempt_worker(root: &Path) {
    let store = crate::oplog::ProjectionReceiptStore::open(
        &root.join("receipts"),
        WorkspaceId::from_uuid(Uuid::from_u128(1)),
    )
    .unwrap();
    let intent = corpus_fixture_intent(store.workspace_id());
    let marker = root.join("sigkill-ready");
    ATTEMPT_PUBLICATION_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            write_marker(&marker);
            loop {
                thread::sleep(Duration::from_secs(60));
            }
        }));
    });
    let _ = store.begin_mutation(&intent, None);
    panic!("SIGKILL worker escaped the attempt-publication cut");
}

#[cfg(unix)]
fn run_sigkill_attempt_publication(case: &CorpusCase) -> CaseReceipt {
    let fixture = Fixture::new("crash-corpus-sigkill-attempt");
    let mut child = spawn_worker("sigkill-attempt", &fixture.root);
    wait_for_path(&fixture.root.join("sigkill-ready"));
    let kill_result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
    assert_eq!(
        kill_result, 0,
        "parent could not SIGKILL crash-corpus child"
    );
    let status = child.wait().unwrap();
    require_signal(status, libc::SIGKILL, "attempt publication true-kill cut");
    let before = format!(
        "attempts={:?},authority={:?}",
        fixture.attempt_stats(&fixture.intent),
        fixture.authority_stats()
    );
    let reopened = fixture.reopen_store();
    reset_projection_graph_test_counters();
    reset_projection_store_test_counters();
    let reservation = reopened.reserve_attempt(&fixture.intent).unwrap();
    let mut authority = reopened
        .begin_mutation(&fixture.intent, Some(&reservation))
        .unwrap();
    let proof = fixture
        .graph
        .write_page_projection(
            fixture.intent.path().as_str(),
            None,
            &fixture.target,
            &mut authority,
        )
        .unwrap();
    reopened
        .publish_completion(authority, &fixture.intent, &proof)
        .unwrap();
    assert!(reopened.load_completion(&fixture.intent).unwrap().is_some());
    let mut measured = Measurements {
        authority_slots: fixture.authority_stats().0,
        authority_bytes: fixture.authority_stats().1,
        attempt_slots: fixture.attempt_stats(&fixture.intent).0,
        ..Measurements::default()
    };
    capture_graph_measurements(&mut measured);
    capture_scan_measurements(&mut measured);
    let after = format!(
        "completion=true,attempts={:?},authority={:?}",
        fixture.attempt_stats(&fixture.intent),
        fixture.authority_stats()
    );
    assert!(
        measured.attempt_slots == 1
            && measured.authority_slots == 0
            && measured.projection_write_calls == 1
            && measured.projection_remove_calls == 0
            && measured.projection_recovery_calls == 0
            && measured.completion_lookups == 1
            && measured.catalog_directory_entries == 0,
        "{}",
        assertion_receipt(case, &before, &after, measured)
    );
    CaseReceipt {
        id: case.id.clone(),
        cut: case.cut.clone(),
        before,
        after,
        measured,
    }
}

#[cfg(not(unix))]
fn run_sigkill_attempt_publication(case: &CorpusCase) -> CaseReceipt {
    panic!("{} requires Unix SIGKILL support", case.id)
}

fn run_graph_wide_foreign_edit_rejection(case: &CorpusCase) -> CaseReceipt {
    const PATH: &str = "archive/客户/Πλάνο Καφέ.md";
    const EXTERNAL: &[u8] = b"- foreign external edit\n";
    const TARGET_HASH: &str = "616bcb42e0737ee9ea0c0a3ac930ae0f25f08d0599a9aa88b6c6df5fbc32f9b5";
    const EXTERNAL_HASH: &str =
        "02b3eb6c70a8b6a26ac2e094f912dff84d81bf95883e21fe5a9f1e0ad9188bfe";

    assert_eq!(format!("{:x}", Sha256::digest(EXTERNAL)), EXTERNAL_HASH);
    let dir = CorpusDir::new("graph-wide-foreign-edit");
    let graph_root = dir.path().join("graph");
    fs::create_dir_all(graph_root.join("pages")).unwrap();
    fs::create_dir_all(graph_root.join("journals")).unwrap();
    let graph = Graph::open(&graph_root);
    let binding = corpus_binding(&graph, 73_000);
    let receipts = crate::oplog::ProjectionReceiptStore::open_for_endpoint(
        &dir.path().join("receipts"),
        corpus_workspace(1),
        binding,
    )
    .unwrap();
    let (engine, page_id) = corpus_authorized_engine(
        &dir,
        PATH,
        "should not land",
        Some((&graph, &receipts)),
    );
    let authorization = engine.authorize_projection_write(page_id).unwrap();
    let plan = plan_projection(corpus_workspace(1), authorization.state(), None).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(plan.target())),
        TARGET_HASH,
        "graph-wide target hash drifted"
    );
    receipts.publish_intent(plan.intent(), None).unwrap();
    let target = graph_root.join(PATH);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, EXTERNAL).unwrap();

    reset_projection_store_test_counters();
    reset_projection_graph_test_counters();
    let recovered = recover_incomplete_projections(&graph, &receipts, &engine);
    assert!(
        recovered.is_err(),
        "foreign graph-wide edit must fail closed"
    );
    assert_eq!(fs::read(&target).unwrap(), EXTERNAL);
    assert!(receipts.load_completion(plan.intent()).unwrap().is_none());

    let mut measured = Measurements::default();
    capture_graph_measurements(&mut measured);
    capture_scan_measurements(&mut measured);
    let before = format!("path={PATH},foreign_sha256={EXTERNAL_HASH}");
    let after = "completion=false,foreign-bytes=preserved".to_owned();
    assert!(
        measured.projection_write_calls <= 1
            && measured.projection_remove_calls == 0
            && measured.projection_recovery_calls == 1,
        "{}",
        assertion_receipt(case, &before, &after, measured)
    );
    CaseReceipt {
        id: case.id.clone(),
        cut: case.cut.clone(),
        before,
        after,
        measured,
    }
}

#[test]
fn crash_corpus_subprocess_worker() {
    let _state = CrashCorpusTestStateGuard::new();
    let Ok(mode) = std::env::var("TINE_PROJECTION_CRASH_CORPUS_WORKER") else {
        return;
    };
    let root = PathBuf::from(
        std::env::var_os("TINE_PROJECTION_CRASH_CORPUS_ROOT").expect("crash corpus worker root"),
    );
    match mode.as_str() {
        "forensic-crash" => forensic_crash_worker(&root),
        "forensic-repair" => forensic_repair_worker(&root),
        "sigkill-attempt" => kill_attempt_worker(&root),
        other => panic!("unknown crash corpus worker mode {other}"),
    }
}

const UNIX_ONLY_CASE_IDS: &[&str] = &[
    "deletion_completion_publication",
    "deletion_catalog_publication",
    "sigkill_attempt_publication",
];

fn is_unix_only_case(id: &str) -> bool {
    UNIX_ONLY_CASE_IDS.contains(&id)
}

/// The Unix-only cases whose cut is established by removing a directory's write
/// bits. They are undemonstrable wherever those bits are not enforced; the
/// SIGKILL case is not, so it keeps running everywhere Unix does.
const PERMISSION_CUT_CASE_IDS: &[&str] = &[
    "deletion_completion_publication",
    "deletion_catalog_publication",
];

fn is_permission_cut_case(id: &str) -> bool {
    PERMISSION_CUT_CASE_IDS.contains(&id)
}

fn run_portable_case(case: &CorpusCase) -> CaseReceipt {
    match case.id.as_str() {
        "attempt_authority_publication" => run_attempt_authority_publication(case),
        "pregraph_drop_retry" => run_pregraph_drop_retry(case),
        "interrupted_recovery_slot_reuse" => run_interrupted_recovery_slot_reuse(case),
        "completion_retained_slot" => run_completion_retained_slot(case),
        "forensic_salvage_rebuild" => run_forensic_salvage_rebuild(case),
        "graph_wide_foreign_edit_rejection" => run_graph_wide_foreign_edit_rejection(case),
        other => panic!("unknown portable crash-corpus case {other}"),
    }
}

#[cfg(unix)]
fn run_unix_only_case(case: &CorpusCase) -> CaseReceipt {
    match case.id.as_str() {
        "deletion_completion_publication" => run_deletion_completion_publication(case),
        "deletion_catalog_publication" => run_deletion_catalog_publication(case),
        "sigkill_attempt_publication" => run_sigkill_attempt_publication(case),
        other => panic!("unknown Unix-only crash-corpus case {other}"),
    }
}

fn minimal_semantic_fixture_pages() -> Vec<SemanticFixturePage> {
    vec![SemanticFixturePage {
        kind: FixtureKind::Page,
        path: "pages/guard.md".to_owned(),
        format: "markdown".to_owned(),
        bytes: "- guard\n".to_owned(),
        sha256: "d1d5678890fd91bc04b349d9d45fff8420e43620131f2b0646266a68c1422022"
            .to_owned(),
        preamble: None,
        sparse_ids: Vec::new(),
        deleted: false,
    }]
}

#[test]
fn crash_corpus_state_guard_scopes_stale_and_intentional_prelude_hooks() {
    let stale_receipt_hook_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    MUTATION_AUTHORITY_CAPTURED_HOOK.with(|hook| {
        let fired = stale_receipt_hook_fired.clone();
        *hook.borrow_mut() = Some(Box::new(move || {
            fired.store(true, std::sync::atomic::Ordering::SeqCst);
        }));
    });
    fail_next_projection_directory_sync_for_test();

    let _state = CrashCorpusTestStateGuard::new();
    let stale_prelude = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_live_fixture_pages_semantics(&minimal_semantic_fixture_pages());
    }));
    assert!(
        stale_prelude.is_ok(),
        "the guard must clear stale receipt and projection controls before the semantic prelude"
    );
    assert!(
        !stale_receipt_hook_fired.load(std::sync::atomic::Ordering::SeqCst),
        "the semantic prelude fired an abandoned receipt hook"
    );

    let intentional_receipt_hook_fired =
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    MUTATION_AUTHORITY_CAPTURED_HOOK.with(|hook| {
        let fired = intentional_receipt_hook_fired.clone();
        *hook.borrow_mut() = Some(Box::new(move || {
            fired.store(true, std::sync::atomic::Ordering::SeqCst);
        }));
    });
    fail_next_projection_directory_sync_for_test();
    let intentional_prelude = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_live_fixture_pages_semantics(&minimal_semantic_fixture_pages());
    }));
    assert!(
        intentional_prelude.is_err(),
        "a directory-sync failpoint installed after guard construction must remain active"
    );
    assert!(
        intentional_receipt_hook_fired.load(std::sync::atomic::Ordering::SeqCst),
        "a receipt hook installed after guard construction must remain active"
    );
}

#[test]
fn crash_corpus_state_guard_clears_hooks_during_panic_unwind() {
    let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _state = CrashCorpusTestStateGuard::new();
        MUTATION_AUTHORITY_CAPTURED_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|| {
                panic!("receipt hook escaped corpus-state guard drop")
            }));
        });
        fail_next_projection_directory_sync_for_test();
        panic!("simulate an ordinary crash-corpus panic");
    }));
    assert!(interrupted.is_err());

    let later_prelude = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_live_fixture_pages_semantics(&minimal_semantic_fixture_pages());
    }));
    assert!(
        later_prelude.is_ok(),
        "guard drop must clear corpus hooks before a reused worker enters the next prelude"
    );
}

#[test]
fn crash_corpus_v2_executes_every_declared_cut() {
    let _state = CrashCorpusTestStateGuard::new();
    let manifest = corpus_manifest();
    assert_fixture_semantics(&manifest);
    let mut seen = BTreeSet::new();
    let mut receipts = Vec::new();
    let mut unexecuted_unix: Vec<&str> = Vec::new();
    #[cfg(unix)]
    let permissions_enforced = directory_permissions_are_enforced();

    let manifest_unix = manifest
        .cases
        .iter()
        .filter(|case| is_unix_only_case(&case.id))
        .map(|case| case.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(manifest_unix, UNIX_ONLY_CASE_IDS);

    for case in &manifest.cases {
        assert!(
            seen.insert(case.id.clone()),
            "duplicate crash corpus case {}",
            case.id
        );
        let (operation, cut) = case_spec(&case.id);
        assert_eq!(case.operation, operation, "operation drift for {}", case.id);
        assert_eq!(case.cut, cut, "cut drift for {}", case.id);
        if is_unix_only_case(&case.id) {
            #[cfg(unix)]
            {
                if is_permission_cut_case(&case.id) && !permissions_enforced {
                    eprintln!(
                        "case={} execution=unexecuted reason=this user is not denied by directory write bits, so the cut cannot be established",
                        case.id
                    );
                    unexecuted_unix.push(case.id.as_str());
                    continue;
                }
                let receipt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_unix_only_case(case)
                }))
                .unwrap_or_else(|_| {
                    panic!(
                        "{}",
                        assertion_receipt(
                            case,
                            "runner-start",
                            "interrupted",
                            Measurements::default()
                        )
                    )
                });
                eprintln!("{}", receipt.render());
                receipts.push(receipt);
            }
            #[cfg(not(unix))]
            {
                let receipt = format!(
                    "case={} execution=unexecuted reason=requires Unix chmod or SIGKILL boundary",
                    case.id
                );
                eprintln!("{receipt}");
                unexecuted_unix.push(case.id.as_str());
            }
            continue;
        }
        let receipt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_portable_case(case)
        }))
        .unwrap_or_else(|_| {
            panic!(
                "{}",
                assertion_receipt(case, "runner-start", "interrupted", Measurements::default())
            )
        });
        eprintln!("{}", receipt.render());
        receipts.push(receipt);
    }
    #[cfg(unix)]
    {
        // A case may only go unexecuted here for the one reason the runner can
        // prove: a permission cut this user is not subject to. Anything else
        // silently shrinking the corpus is the failure mode to guard against.
        assert!(
            unexecuted_unix
                .iter()
                .all(|id| is_permission_cut_case(id)),
            "only permission cuts may go unexecuted on Unix, got {unexecuted_unix:?}"
        );
        assert!(
            unexecuted_unix.is_empty() || !permissions_enforced,
            "a permission cut was skipped even though directory write bits are enforced"
        );
    }
    #[cfg(not(unix))]
    assert_eq!(unexecuted_unix, UNIX_ONLY_CASE_IDS);
    assert_eq!(receipts.len() + unexecuted_unix.len(), manifest.cases.len());
}
