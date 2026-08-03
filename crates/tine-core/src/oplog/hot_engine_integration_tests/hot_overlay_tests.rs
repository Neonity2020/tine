use std::path::PathBuf;
use std::time::Instant;

use cap_std::{ambient_authority, fs::Dir};
use tine_storage::{LocalJournalFrame, LocalJournalSegment};

use super::*;
use crate::fast_commit::forbidden_commit_work;
use crate::oplog::{
    append_managed_local_record, decode_managed_local_record, ApplicationRuntimeRoot,
    ManagedLocalApplyOutcome, ManagedLocalJournalPayloadKind, ManagedLocalRecordError,
    MaterializedPage, ProjectionClaim, RebuildSource, SqliteFrontier,
};

const ENDPOINT: u128 = 980_000;
const DEVICE: u128 = 980_001;
const PAGE_BASE: u128 = 981_000;
const HOME_BASE: u128 = 1_000_000;
const BLOCK_BASE: u128 = 1_020_000;
const SECOND_BLOCK: u128 = 1_040_000;
const LOGSEQ_UUID: u128 = 1_050_000;

struct OverlayFixture {
    writer: ObjectStore,
    graph: Graph,
    receipts: ProjectionReceiptStore,
    engine: ShardedHotEngine,
    binding: ProjectionEndpointBinding,
    ids: Ids,
    page_id: PageId,
    home_document_id: DocumentId,
    block_id: crate::oplog::BlockId,
    second_block_id: crate::oplog::BlockId,
    page_path: ManagedPath,
    graph_path: PathBuf,
    _dir: TestDir,
}

impl OverlayFixture {
    fn new(label: &str, extension: &str, pages: usize) -> Self {
        Self::build(label, extension, pages, false)
    }

    fn build(label: &str, extension: &str, pages: usize, sparse_identity: bool) -> Self {
        assert!(pages > 0);
        let ids = Ids::new();
        let dir = TestDir::new(label);
        let archive_path = dir.path().join("archive");
        let graph_path = dir.path().join("graph");
        std::fs::create_dir_all(&graph_path).unwrap();
        let graph = Graph::open(&graph_path);
        let binding = ProjectionEndpointBinding::enroll_graph(
            &graph,
            ProjectionEndpointId::from_uuid(uuid(ENDPOINT)),
            DeviceId::from_uuid(uuid(DEVICE)),
        )
        .unwrap();
        let receipts = ProjectionReceiptStore::open_for_endpoint(
            &dir.path().join("receipts"),
            ids.workspace,
            binding,
        )
        .unwrap();
        let writer = ObjectStore::open(&archive_path, ids.workspace).unwrap();
        let reader = ObjectStore::open(&archive_path, ids.workspace).unwrap();
        let mut engine = ShardedHotEngine::with_enrolled_projection(
            reader,
            ids.lineage,
            ids.catalog,
            &graph,
            &receipts,
        );

        let page_id = PageId::from_uuid(uuid(PAGE_BASE));
        let home_document_id = DocumentId::from_uuid(uuid(HOME_BASE));
        let block_id = crate::oplog::BlockId::from_uuid(uuid(BLOCK_BASE));
        let second_block_id = crate::oplog::BlockId::from_uuid(uuid(SECOND_BLOCK));
        const SETUP_PAGES_PER_BATCH: usize = 1_000;
        for start in (0..pages).step_by(SETUP_PAGES_PER_BATCH) {
            let end = (start + SETUP_PAGES_PER_BATCH).min(pages);
            let mut operations = Vec::with_capacity((end - start).saturating_mul(2) + 4);
            for index in start..end {
                let page_id = PageId::from_uuid(uuid(PAGE_BASE + index as u128));
                let home_document_id = DocumentId::from_uuid(uuid(HOME_BASE + index as u128));
                let block_id = crate::oplog::BlockId::from_uuid(uuid(BLOCK_BASE + index as u128));
                operations.push(SemanticOperation::CreatePage {
                    page_id,
                    home_document_id,
                    name: crate::oplog::LogicalPageName::parse(format!("Overlay {index:05}"))
                        .unwrap(),
                    path: path(&format!("pages/Overlay-{index:05}.{extension}")),
                    kind: ManagedTextKind::Page,
                });
                operations.push(SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id,
                        home_document_id,
                    },
                    page_id,
                    parent: None,
                    order: "a".into(),
                    content: format!("initial content {index}"),
                });
            }
            if start == 0 {
                operations.extend([
                    SemanticOperation::CreateBlock {
                        block: BlockLocation {
                            block_id: second_block_id,
                            home_document_id,
                        },
                        page_id,
                        parent: Some(block_id),
                        order: "b".into(),
                        content: "nested child".into(),
                    },
                    SemanticOperation::SetPagePreamble {
                        page_id,
                        preamble: Some("title:: Overlay 00000\ntags:: local".into()),
                    },
                ]);
                if sparse_identity {
                    operations.push(SemanticOperation::MutateBlockLogseqIdentity {
                        block: BlockLocation {
                            block_id,
                            home_document_id,
                        },
                        mutation: LogseqIdentityMutation::Generate {
                            logseq_uuid: LogseqUuid::from_uuid(uuid(LOGSEQ_UUID)),
                            trigger: LogseqIdentityTrigger::ExportUserAction,
                        },
                    });
                }
            }
            let prepared = engine
                .prepare_bootstrap_transaction(
                    author(1_060_000 + start as u128, 1_060_000 + start as u64),
                    &tx(operations),
                )
                .unwrap();
            let batch_id = prepared.manifest().batch_id();
            publish_fixture(&writer, &prepared);
            assert!(matches!(
                engine.stage_archive_batch(batch_id).unwrap().disposition,
                BatchDisposition::Accepted { .. }
            ));
        }
        crate::oplog::write_projection_exact(&graph, &receipts, &engine, page_id, None).unwrap();
        let page_path = path(&format!("pages/Overlay-00000.{extension}"));
        Self {
            writer,
            graph,
            receipts,
            engine,
            binding,
            ids,
            page_id,
            home_document_id,
            block_id,
            second_block_id,
            page_path,
            graph_path,
            _dir: dir,
        }
    }

    fn local_author(&self, seed: u128) -> AuthorBatch {
        AuthorBatch {
            batch_id: BatchId::from_uuid(uuid(seed)),
            author_device_id: self.binding.device_id(),
            author_session_id: SessionId::from_uuid(uuid(seed + 1)),
            crdt_peer_id: CrdtPeerId::from_u64(seed as u64 | 1),
        }
    }

    fn content_edit(&self, generation: usize) -> OperationTransaction {
        tx(vec![SemanticOperation::EditBlockContent {
            block: BlockLocation {
                block_id: self.block_id,
                home_document_id: self.home_document_id,
            },
            content: format!("managed revision {generation}"),
        }])
    }

    fn finalize_edit(&self, seed: u128, generation: usize) -> PreparedBatch {
        let draft = self
            .engine
            .draft_author_transaction(
                self.local_author(seed),
                BatchOrigin::LocalMutation,
                &self.content_edit(generation),
            )
            .unwrap();
        self.engine
            .finalize_author_transaction(draft, &self.graph, &self.receipts, self.binding)
            .unwrap()
    }

    fn accept_and_project(&mut self, prepared: &PreparedBatch) {
        let expected_base = std::fs::read(self.graph_path.join(self.page_path.as_str())).unwrap();
        self.writer.publish_prepared(prepared).unwrap();
        assert!(matches!(
            self.engine
                .stage_archive_batch(prepared.manifest().batch_id())
                .unwrap()
                .disposition,
            BatchDisposition::Accepted { .. }
        ));
        crate::oplog::write_projection_exact(
            &self.graph,
            &self.receipts,
            &self.engine,
            self.page_id,
            Some(&expected_base),
        )
        .unwrap();
    }

    fn prepare_record(&self, prepared: &PreparedBatch) -> crate::oplog::PreparedManagedLocalRecord {
        self.engine
            .prepare_managed_local_record(
                prepared,
                self.engine.managed_local_prefix_state().next_sequence,
            )
            .unwrap()
    }

    fn journal(
        &self,
        label: &str,
    ) -> (PathBuf, LocalJournalSegment<ManagedLocalJournalPayloadKind>) {
        let root = self._dir.path().join(format!("journal-{label}"));
        std::fs::create_dir_all(&root).unwrap();
        let dir = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
        let segment = LocalJournalSegment::open(&dir, "local.segment", uuid(DEVICE))
            .unwrap()
            .0;
        (root, segment)
    }

    fn append_and_apply(
        &mut self,
        journal: &mut LocalJournalSegment<ManagedLocalJournalPayloadKind>,
        prepared: &crate::oplog::PreparedManagedLocalRecord,
    ) -> (tine_storage::LocalJournalAppend, MaterializedPage) {
        let append = append_managed_local_record(journal, prepared).unwrap();
        let page = match self
            .engine
            .apply_appended_managed_local_record(&append, prepared)
            .unwrap()
        {
            ManagedLocalApplyOutcome::Applied { page, .. } => page,
        };
        (append, page)
    }

    fn accept_bootstrap_edit(&mut self, seed: u128, generation: usize) -> PreparedBatch {
        let prepared = self
            .engine
            .prepare_bootstrap_transaction(
                author(seed, seed as u64),
                &self.content_edit(generation),
            )
            .unwrap();
        self.writer
            .publish_bootstrap_prepared_for_test(&prepared)
            .unwrap();
        assert!(matches!(
            self.engine
                .stage_archive_batch(prepared.manifest().batch_id())
                .unwrap()
                .disposition,
            BatchDisposition::Accepted { .. }
        ));
        prepared
    }

    fn sqlite_page_and_blocks(
        &self,
    ) -> (
        crate::oplog::MaterializedPageRow,
        Vec<crate::oplog::MaterializedBlockRow>,
    ) {
        let app =
            ApplicationRuntimeRoot::open_for_test(&self._dir.path().join("app-runtime")).unwrap();
        let source = RebuildSource::new(&self.engine, &self.writer).unwrap();
        let opened = SqliteFrontier::open_or_rebuild(
            &self._dir.path().join("projection.sqlite"),
            &app,
            ProjectionClaim::current(self.ids.workspace, self.ids.lineage),
            source,
        )
        .unwrap();
        let read = opened.database.materialized_read().unwrap();
        let page = read.page(self.page_id).unwrap().unwrap();
        let blocks = [self.block_id, self.second_block_id]
            .into_iter()
            .map(|block_id| read.block(block_id).unwrap().unwrap())
            .collect();
        (page, blocks)
    }
}

fn finalized_edit_chain(
    label: &str,
    extension: &str,
    pages: usize,
    edits: usize,
) -> (OverlayFixture, Vec<PreparedBatch>) {
    let mut authoring = OverlayFixture::new(label, extension, pages);
    let mut batches = Vec::with_capacity(edits);
    for generation in 1..=edits {
        let prepared = authoring.finalize_edit(
            1_070_000 + pages as u128 * 1_000 + generation as u128 * 10,
            generation,
        );
        authoring.accept_and_project(&prepared);
        batches.push(prepared);
    }
    (authoring, batches)
}

fn assert_page_semantics(left: &MaterializedPage, right: &MaterializedPage) {
    assert_eq!(left.page_id, right.page_id);
    assert_eq!(left.home_document_id, right.home_document_id);
    assert_eq!(left.name, right.name);
    assert_eq!(left.path, right.path);
    assert_eq!(left.kind, right.kind);
    assert_eq!(left.preamble, right.preamble);
    assert_eq!(left.blocks, right.blocks);
}

fn frame(
    prepared: &crate::oplog::PreparedManagedLocalRecord,
) -> LocalJournalFrame<ManagedLocalJournalPayloadKind> {
    LocalJournalFrame::new(
        uuid(DEVICE),
        prepared.sequence(),
        prepared.payload_kind(),
        prepared.journal_payload().to_vec(),
    )
}

#[test]
fn managed_local_boundary_starts_with_an_empty_prefix() {
    let engine = Ids::new().engine();
    assert_eq!(engine.managed_local_work().commits_applied, 0);
    assert_eq!(
        engine.managed_local_prefix_state(),
        crate::oplog::ManagedLocalPrefixState {
            next_sequence: 0,
            records_applied: 0,
            commitment: ContentDigest::of(b"tine/managed-local-prefix/empty/v1\0"),
        }
    );
}

#[test]
fn markdown_and_org_record_application_matches_the_exact_accepted_engine_and_sqlite_semantics() {
    for extension in ["md", "org"] {
        let (accepted, batches) = finalized_edit_chain(
            &format!("managed-record-accepted-{extension}"),
            extension,
            8,
            1,
        );
        let expected = accepted.engine.materialize_page(accepted.page_id).unwrap();
        let mut local =
            OverlayFixture::new(&format!("managed-record-local-{extension}"), extension, 8);
        let (_, mut journal) = local.journal("semantic");
        let prepared = local.prepare_record(&batches[0]);
        let forbidden_before = forbidden_commit_work();
        let stats_before = journal.stats();
        let (_, response) = local.append_and_apply(&mut journal, &prepared);
        let direct = local
            .engine
            .materialize_current_page_at_path(&local.page_path)
            .unwrap()
            .unwrap();
        let forbidden = forbidden_commit_work().since(forbidden_before);
        assert!(
            forbidden.is_none(),
            "managed-local append/apply performed forbidden work: {forbidden:?}"
        );
        let stats = journal.stats();
        assert_eq!(stats.frames_appended - stats_before.frames_appended, 1);
        assert_eq!(
            stats.data_durability_syncs - stats_before.data_durability_syncs,
            1
        );
        assert_page_semantics(prepared.post_page(), &response);
        assert_page_semantics(&response, &direct);
        assert_page_semantics(&direct, &expected);

        let (sqlite_page, sqlite_blocks) = accepted.sqlite_page_and_blocks();
        assert_eq!(sqlite_page.page_id, direct.page_id);
        assert_eq!(sqlite_page.home_document_id, direct.home_document_id);
        assert_eq!(sqlite_page.name, direct.name.as_str());
        assert_eq!(sqlite_page.path, direct.path);
        assert_eq!(sqlite_page.kind, direct.kind);
        assert_eq!(sqlite_page.preamble, direct.preamble);
        for (row, block) in sqlite_blocks.iter().zip(&direct.blocks) {
            assert_eq!(row.block_id, block.block_id);
            assert_eq!(row.home_document_id, block.home_document_id);
            assert_eq!(row.parent, block.parent);
            assert_eq!(row.order, block.order);
            assert_eq!(row.content, block.content);
            assert_eq!(row.logseq_uuid, block.logseq_uuid);
            assert_eq!(row.logseq_identity_origin, block.logseq_identity_origin);
        }
        assert_eq!(direct.blocks[0].logseq_uuid, None);
        assert_eq!(direct.blocks[0].logseq_identity_origin, None);
    }
}

#[test]
fn record_reconstructs_exact_prepared_batch_and_projection_material() {
    let (_, batches) = finalized_edit_chain("managed-record-reconstruct-source", "md", 8, 1);
    let fixture = OverlayFixture::new("managed-record-reconstruct-local", "md", 8);
    let prepared = fixture.prepare_record(&batches[0]);
    let decoded = decode_managed_local_record(&frame(&prepared)).unwrap();

    assert_eq!(
        decoded.prepared_batch().manifest().encode().unwrap(),
        batches[0].manifest().encode().unwrap()
    );
    let encoded = |batch: &PreparedBatch| {
        batch
            .objects()
            .iter()
            .map(|object| object.encode().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(encoded(decoded.prepared_batch()), encoded(&batches[0]));

    let original = crate::oplog::projection_manifest::validate_projection_object_set(
        batches[0].manifest(),
        batches[0].objects(),
    )
    .unwrap();
    assert_eq!(original.intents(), &[decoded.projection().intent().clone()]);
    let base_reference = decoded.projection().intent().precondition().base().unwrap();
    assert_eq!(
        original.bases().get(&base_reference.document_id()),
        Some(decoded.projection().precondition_base())
    );
    assert_eq!(decoded.projection().intent().page_id(), fixture.page_id);
    assert_eq!(decoded.projection().intent().path(), &fixture.page_path);
    assert_eq!(
        decoded.projection().precondition_base().source_path(),
        &fixture.page_path
    );
    assert_eq!(
        decoded.projection().precondition_base().bytes(),
        std::fs::read(fixture.graph_path.join(fixture.page_path.as_str())).unwrap()
    );
    assert!(decoded.projection().intent().target().bytes().is_some());
}

#[test]
fn consecutive_records_replay_into_a_fresh_engine_like_uninterrupted_execution() {
    let (accepted, batches) = finalized_edit_chain("managed-record-chain-source", "org", 16, 12);
    let expected = accepted.engine.materialize_page(accepted.page_id).unwrap();
    let mut uninterrupted = OverlayFixture::new("managed-record-chain-live", "org", 16);
    let (_, mut journal) = uninterrupted.journal("chain");
    let mut frames = Vec::new();
    for prepared_batch in &batches {
        let prepared = uninterrupted.prepare_record(prepared_batch);
        uninterrupted.append_and_apply(&mut journal, &prepared);
        frames.push(frame(&prepared));
    }
    let live = uninterrupted
        .engine
        .materialize_current_page_at_path(&uninterrupted.page_path)
        .unwrap()
        .unwrap();

    let mut recovered = OverlayFixture::new("managed-record-chain-recovered", "org", 16);
    // Startup establishes the accepted touched-page base before replaying its
    // journal prefix. The measured replay itself may not consult the archive.
    recovered
        .engine
        .materialize_page(recovered.page_id)
        .unwrap();
    let forbidden_before = forbidden_commit_work();
    for record in &frames {
        recovered
            .engine
            .replay_managed_local_record(record)
            .unwrap();
    }
    let forbidden = forbidden_commit_work().since(forbidden_before);
    assert!(
        forbidden.is_none(),
        "managed-local replay performed forbidden work: {forbidden:?}"
    );
    let replayed = recovered
        .engine
        .materialize_current_page_at_path(&recovered.page_path)
        .unwrap()
        .unwrap();
    assert_page_semantics(&expected, &live);
    assert_page_semantics(&live, &replayed);
    assert_eq!(
        recovered
            .engine
            .managed_local_prefix_state()
            .records_applied,
        12
    );
    assert_eq!(recovered.engine.managed_local_work().documents_imported, 12);
}

#[test]
fn corrupt_binding_order_and_stale_base_are_refused_before_visible_change() {
    let (_, batches) = finalized_edit_chain("managed-record-refusal-source", "md", 8, 2);
    let source = OverlayFixture::new("managed-record-refusal-encode", "md", 8);
    let first = source.prepare_record(&batches[0]);
    let first_frame = frame(&first);

    let mut stale = OverlayFixture::new("managed-record-refusal-stale", "md", 8);
    stale.accept_bootstrap_edit(1_100_100, 9);
    let stale_before = stale.engine.materialize_page(stale.page_id).unwrap();
    assert!(matches!(
        stale.engine.replay_managed_local_record(&first_frame),
        Err(ManagedLocalRecordError::StaleBase)
    ));
    assert_page_semantics(
        &stale_before,
        &stale.engine.materialize_page(stale.page_id).unwrap(),
    );

    let mut clean = OverlayFixture::new("managed-record-refusal-clean", "md", 8);
    let unchanged = clean.engine.materialize_page(clean.page_id).unwrap();
    let mut corrupt_bytes = first.journal_payload().to_vec();
    let corrupt_index = corrupt_bytes.len() / 2;
    corrupt_bytes[corrupt_index] ^= 0x80;
    let corrupt = LocalJournalFrame::new(
        uuid(DEVICE),
        first.sequence(),
        first.payload_kind(),
        corrupt_bytes,
    );
    assert!(matches!(
        clean.engine.replay_managed_local_record(&corrupt),
        Err(ManagedLocalRecordError::CorruptPayload(_)) | Err(ManagedLocalRecordError::Engine(_))
    ));
    assert_page_semantics(
        &unchanged,
        &clean.engine.materialize_page(clean.page_id).unwrap(),
    );

    let wrong_device = LocalJournalFrame::new(
        uuid(DEVICE + 1),
        first.sequence(),
        first.payload_kind(),
        first.journal_payload().to_vec(),
    );
    assert!(matches!(
        clean.engine.replay_managed_local_record(&wrong_device),
        Err(ManagedLocalRecordError::CorruptPayload(_))
    ));
    let wrong_sequence = LocalJournalFrame::new(
        uuid(DEVICE),
        1,
        first.payload_kind(),
        first.journal_payload().to_vec(),
    );
    assert!(matches!(
        clean.engine.replay_managed_local_record(&wrong_sequence),
        Err(ManagedLocalRecordError::CorruptPayload(_))
    ));

    let mut live = OverlayFixture::new("managed-record-refusal-live", "md", 8);
    live.engine
        .replay_managed_local_record(&first_frame)
        .unwrap();
    let second = live.prepare_record(&batches[1]);
    let second_frame = frame(&second);
    let mut gap = OverlayFixture::new("managed-record-refusal-gap", "md", 8);
    let gap_before = gap.engine.materialize_page(gap.page_id).unwrap();
    assert!(matches!(
        gap.engine.replay_managed_local_record(&second_frame),
        Err(ManagedLocalRecordError::OutOfOrder {
            expected: 0,
            found: 1
        })
    ));
    assert_page_semantics(
        &gap_before,
        &gap.engine.materialize_page(gap.page_id).unwrap(),
    );
    let live_before_duplicate = live.engine.materialize_page(live.page_id).unwrap();
    assert!(matches!(
        live.engine.replay_managed_local_record(&first_frame),
        Err(ManagedLocalRecordError::OutOfOrder {
            expected: 1,
            found: 0
        })
    ));
    assert_page_semantics(
        &live_before_duplicate,
        &live.engine.materialize_page(live.page_id).unwrap(),
    );

    let mut wrong_workspace = ShardedHotEngine::new(
        WorkspaceId::from_uuid(uuid(1_100_300)),
        source.ids.lineage,
        source.ids.catalog,
    );
    assert!(matches!(
        wrong_workspace.replay_managed_local_record(&first_frame),
        Err(ManagedLocalRecordError::Engine(
            EngineError::WorkspaceMismatch { .. }
        ))
    ));
    let mut wrong_lineage = ShardedHotEngine::new(
        source.ids.workspace,
        LineageDigest::of(b"wrong managed-local lineage"),
        source.ids.catalog,
    );
    assert!(matches!(
        wrong_lineage.replay_managed_local_record(&first_frame),
        Err(ManagedLocalRecordError::Engine(
            EngineError::LineageMismatch { .. }
        ))
    ));
}

#[test]
fn append_refuses_wrong_device_before_writing_and_receipt_binds_one_barrier() {
    let (_, batches) = finalized_edit_chain("managed-record-append-source", "md", 8, 1);
    let mut fixture = OverlayFixture::new("managed-record-append-local", "md", 8);
    let prepared = fixture.prepare_record(&batches[0]);
    let wrong_root = fixture._dir.path().join("journal-wrong-device");
    std::fs::create_dir_all(&wrong_root).unwrap();
    let wrong_dir = Dir::open_ambient_dir(wrong_root, ambient_authority()).unwrap();
    let mut wrong = LocalJournalSegment::open(&wrong_dir, "local.segment", uuid(DEVICE + 1))
        .unwrap()
        .0;
    assert_eq!(
        append_managed_local_record(&mut wrong, &prepared),
        Err(ManagedLocalRecordError::WrongDurabilityProof)
    );
    assert_eq!(wrong.next_sequence(), 0);
    assert_eq!(wrong.stats().frames_appended, 0);

    let (_, mut correct) = fixture.journal("correct-device");
    let append = append_managed_local_record(&mut correct, &prepared).unwrap();
    assert_eq!(append.device_id, uuid(DEVICE));
    assert_eq!(append.sequence, 0);
    assert_eq!(
        append.payload_digest,
        ContentDigest::of(prepared.journal_payload())
    );
    assert_eq!(append.data_durability_syncs, 1);
    fixture
        .engine
        .apply_appended_managed_local_record(&append, &prepared)
        .unwrap();
}

#[test]
fn torn_final_frame_recovers_and_replays_only_the_complete_prefix() {
    let (_, batches) = finalized_edit_chain("managed-record-torn-source", "org", 8, 2);
    let mut live = OverlayFixture::new("managed-record-torn-live", "org", 8);
    let (journal_root, mut journal) = live.journal("torn");
    let first = live.prepare_record(&batches[0]);
    live.append_and_apply(&mut journal, &first);
    let expected_prefix = live.engine.materialize_page(live.page_id).unwrap();
    let second = live.prepare_record(&batches[1]);
    let (second_append, _) = live.append_and_apply(&mut journal, &second);
    let committed = journal.committed_bytes();
    drop(journal);

    let segment_path = journal_root.join("local.segment");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&segment_path)
        .unwrap();
    file.set_len(committed - second_append.frame_bytes / 2)
        .unwrap();
    file.sync_data().unwrap();
    drop(file);

    let dir = Dir::open_ambient_dir(&journal_root, ambient_authority()).unwrap();
    let (recovered_segment, recovery) =
        LocalJournalSegment::<ManagedLocalJournalPayloadKind>::open(
            &dir,
            "local.segment",
            uuid(DEVICE),
        )
        .unwrap();
    assert_eq!(recovery.frames_recovered, 1);
    assert!(recovery.discarded_tail_bytes > 0);
    assert_eq!(recovered_segment.next_sequence(), 1);
    let mut frames = Vec::new();
    recovered_segment
        .replay(|record| frames.push(record))
        .unwrap();
    assert_eq!(frames.len(), 1);

    let mut fresh = OverlayFixture::new("managed-record-torn-fresh", "org", 8);
    fresh
        .engine
        .replay_managed_local_record(&frames[0])
        .unwrap();
    assert_page_semantics(
        &expected_prefix,
        &fresh.engine.materialize_page(fresh.page_id).unwrap(),
    );
}

#[test]
fn accepted_catchup_collapses_prefix_without_semantic_change_and_sequence_stays_monotonic() {
    let mut fixture = OverlayFixture::new("managed-record-collapse", "md", 12);
    let prepared_batch = fixture.finalize_edit(1_110_000, 1);
    let prepared = fixture.prepare_record(&prepared_batch);
    let (_, mut journal) = fixture.journal("collapse");
    let before_frontier = fixture.engine.accepted_frontier_root().unwrap();
    fixture.append_and_apply(&mut journal, &prepared);
    assert!(matches!(
        fixture
            .engine
            .collapse_managed_local_prefix(&before_frontier),
        Err(ManagedLocalRecordError::AcceptedFrontierMismatch)
    ));

    fixture.writer.publish_prepared(&prepared_batch).unwrap();
    assert!(matches!(
        fixture
            .engine
            .stage_archive_batch(prepared_batch.manifest().batch_id())
            .unwrap()
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    let expected = fixture.engine.materialize_page(fixture.page_id).unwrap();
    let accepted_frontier = fixture.engine.accepted_frontier_root().unwrap();
    assert_eq!(
        fixture
            .engine
            .collapse_managed_local_prefix(&accepted_frontier)
            .unwrap(),
        1
    );
    let prefix = fixture.engine.managed_local_prefix_state();
    assert_eq!(prefix.records_applied, 0);
    assert_eq!(prefix.next_sequence, 1);
    assert_page_semantics(
        &expected,
        &fixture.engine.materialize_page(fixture.page_id).unwrap(),
    );
}

#[test]
#[ignore = "manual release benchmark; synthetic pages and bounded record work"]
fn managed_local_record_manual_release_benchmark() {
    let pages = std::env::var("TINE_MANAGED_LOCAL_RECORD_BENCH_PAGES")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(|part| part.parse::<usize>().unwrap())
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec![100, 10_000]);
    let edits = std::env::var("TINE_MANAGED_LOCAL_RECORD_BENCH_EDITS")
        .ok()
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(8);
    let warmups = std::env::var("TINE_MANAGED_LOCAL_RECORD_BENCH_WARMUPS")
        .ok()
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(2);
    assert!(edits > warmups);

    let mut observations = Vec::new();
    for page_count in pages {
        let (_, batches) = finalized_edit_chain(
            &format!("managed-record-bench-source-{page_count}"),
            "md",
            page_count,
            edits,
        );
        let mut fixture = OverlayFixture::new(
            &format!("managed-record-bench-replay-{page_count}"),
            "md",
            page_count,
        );
        fixture.engine.materialize_page(fixture.page_id).unwrap();
        let work_before = fixture.engine.managed_local_work();
        let mut samples = Vec::new();
        for (index, batch) in batches.iter().enumerate() {
            let forbidden_before = forbidden_commit_work();
            let started = Instant::now();
            let prepared = fixture.prepare_record(batch);
            fixture
                .engine
                .replay_managed_local_record(&frame(&prepared))
                .unwrap();
            let page = fixture
                .engine
                .materialize_current_page_at_path(&fixture.page_path)
                .unwrap()
                .unwrap();
            let elapsed = started.elapsed().as_secs_f64() * 1_000.0;
            assert!(forbidden_commit_work().since(forbidden_before).is_none());
            assert_eq!(page.stats.catalog_documents_loaded, 0);
            if index >= warmups {
                samples.push(elapsed);
            }
        }
        let work = fixture.engine.managed_local_work().since(work_before);
        assert_eq!(work.commits_applied, edits);
        assert_eq!(work.documents_imported, edits);
        assert_eq!(work.page_materializations, edits);
        samples.sort_by(f64::total_cmp);
        let p50 = samples[samples.len() / 2];
        println!(
            "managed_local_record_benchmark pages={page_count} n={} p50_ms={p50:.6} raw_ms={samples:?}",
            samples.len()
        );
        observations.push((page_count, p50, work));
    }
    assert!(observations.len() >= 2);
    assert_eq!(observations[0].2, observations[1].2);
    assert!(
        observations[1].1 <= observations[0].1 * 2.0 + 1.0,
        "10,000-page p50 is not bounded against 100 pages: {observations:?}"
    );
}
