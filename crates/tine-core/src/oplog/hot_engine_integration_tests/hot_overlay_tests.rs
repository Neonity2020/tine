use std::path::PathBuf;
use std::time::Instant;
use std::{collections::BTreeMap, fs};

use cap_std::{ambient_authority, fs::Dir};
use tine_storage::{
    LocalJournalFrame, LocalJournalSegment, LocalJournalSegmentV2, LocalJournalSegmentV2Selection,
};

use super::*;
use crate::fast_commit::forbidden_commit_work;
use crate::oplog::{
    append_managed_local_record, decode_managed_local_record, ApplicationRuntimeRoot,
    ManagedLocalAppendError, ManagedLocalAppendProof, ManagedLocalApplyOutcome,
    ManagedLocalJournal, ManagedLocalJournalPayloadKind, ManagedLocalJournalProtocol,
    ManagedLocalRecordError, MaterializedPage, ProjectionClaim, RebuildSource, SqliteFrontier,
};

const ENDPOINT: u128 = 980_000;
const DEVICE: u128 = 980_001;
const PAGE_BASE: u128 = 981_000;
const HOME_BASE: u128 = 1_000_000;
const BLOCK_BASE: u128 = 1_020_000;
const SECOND_BLOCK: u128 = 1_040_000;
const LOGSEQ_UUID: u128 = 1_050_000;

pub(crate) struct OverlayFixture {
    writer: ObjectStore,
    pub(crate) graph: Graph,
    pub(crate) receipts: ProjectionReceiptStore,
    pub(crate) engine: ShardedHotEngine,
    pub(crate) binding: ProjectionEndpointBinding,
    ids: Ids,
    pub(crate) page_id: PageId,
    pub(crate) home_document_id: DocumentId,
    pub(crate) block_id: crate::oplog::BlockId,
    second_block_id: crate::oplog::BlockId,
    pub(crate) page_path: ManagedPath,
    pub(crate) graph_path: PathBuf,
    _dir: TestDir,
}

impl OverlayFixture {
    pub(crate) fn new(label: &str, extension: &str, pages: usize) -> Self {
        Self::build(label, extension, pages, false, None)
    }

    pub(crate) fn new_at_path(label: &str, relative_path: &str, pages: usize) -> Self {
        let extension = std::path::Path::new(relative_path)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap();
        Self::build(label, extension, pages, false, Some(relative_path))
    }

    fn build(
        label: &str,
        extension: &str,
        pages: usize,
        sparse_identity: bool,
        first_path: Option<&str>,
    ) -> Self {
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
                    path: path(
                        &first_path
                            .filter(|_| index == 0)
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("pages/Overlay-{index:05}.{extension}")),
                    ),
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
        graph.warm_cache();
        let page_path = path(
            &first_path
                .map(str::to_owned)
                .unwrap_or_else(|| format!("pages/Overlay-00000.{extension}")),
        );
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

    pub(crate) fn local_author(&self, seed: u128) -> AuthorBatch {
        AuthorBatch {
            batch_id: BatchId::from_uuid(uuid(seed)),
            author_device_id: self.binding.device_id(),
            author_session_id: SessionId::from_uuid(uuid(seed + 1)),
            crdt_peer_id: CrdtPeerId::from_u64(seed as u64 | 1),
        }
    }

    pub(crate) fn content_edit(&self, generation: usize) -> OperationTransaction {
        tx(vec![SemanticOperation::EditBlockContent {
            block: BlockLocation {
                block_id: self.block_id,
                home_document_id: self.home_document_id,
            },
            content: format!("managed revision {generation}"),
        }])
    }

    /// Retain a genuine no-op read of a second page's document while changing
    /// the first page. This is useful for proving that a narrow page-local
    /// fast path refuses a draft whose captured pre-state spans more than the
    /// edited page's one home document.
    pub(crate) fn content_edit_with_noop_foreign_read(
        &self,
        generation: usize,
    ) -> OperationTransaction {
        tx(vec![
            SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: self.block_id,
                    home_document_id: self.home_document_id,
                },
                content: format!("managed revision {generation}"),
            },
            SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: crate::oplog::BlockId::from_uuid(uuid(BLOCK_BASE + 1)),
                    home_document_id: DocumentId::from_uuid(uuid(HOME_BASE + 1)),
                },
                content: "initial content 1".into(),
            },
        ])
    }

    pub(crate) fn finalize_edit(&self, seed: u128, generation: usize) -> PreparedBatch {
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

    pub(crate) fn accept_and_project(&mut self, prepared: &PreparedBatch) {
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
                prepared.clone(),
                self.engine.managed_local_prefix_state().next_sequence,
            )
            .unwrap()
    }

    pub(crate) fn journal(
        &self,
        label: &str,
    ) -> (PathBuf, LocalJournalSegment<ManagedLocalJournalPayloadKind>) {
        self.journal_for_device(label, uuid(DEVICE))
    }

    pub(crate) fn journal_for_device(
        &self,
        label: &str,
        device: uuid::Uuid,
    ) -> (PathBuf, LocalJournalSegment<ManagedLocalJournalPayloadKind>) {
        let root = self._dir.path().join(format!("journal-{label}"));
        std::fs::create_dir_all(&root).unwrap();
        let dir = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
        let segment = LocalJournalSegment::open(&dir, "local.segment", device)
            .unwrap()
            .0;
        (root, segment)
    }

    fn append_and_apply(
        &mut self,
        journal: &mut LocalJournalSegment<ManagedLocalJournalPayloadKind>,
        prepared: &mut crate::oplog::PreparedManagedLocalRecord,
    ) -> (ManagedLocalAppendProof, MaterializedPage) {
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

    fn database_and_tail(&self, label: &str) -> (SqliteFrontier, crate::oplog::TailOverlay) {
        let app = ApplicationRuntimeRoot::open_for_test(
            &self._dir.path().join(format!("drain-app-runtime-{label}")),
        )
        .unwrap();
        let source = RebuildSource::new(&self.engine, &self.writer).unwrap();
        let opened = SqliteFrontier::open_or_rebuild(
            &self
                ._dir
                .path()
                .join(format!("drain-projection-{label}.sqlite")),
            &app,
            ProjectionClaim::current(self.ids.workspace, self.ids.lineage),
            source,
        )
        .unwrap();
        let source = RebuildSource::new(&self.engine, &self.writer).unwrap();
        let tail = crate::oplog::TailOverlay::from_durable(&opened.database, &source).unwrap();
        (opened.database, tail)
    }
}

#[derive(Default)]
struct DrainPublisher {
    authorship:
        BTreeMap<BatchId, crate::oplog::local_journal_drain::ManagedLocalDerivativeAuthority>,
    provider: BTreeMap<BatchId, crate::oplog::local_journal_drain::ManagedLocalDerivativeAuthority>,
    pending_authorship_once: bool,
    pending_provider_once: bool,
}

impl crate::oplog::local_journal_drain::ManagedLocalDerivativePublisher for DrainPublisher {
    fn ensure_local_authorship(
        &mut self,
        authority: &crate::oplog::local_journal_drain::ManagedLocalDerivativeAuthority,
    ) -> crate::oplog::local_journal_drain::ManagedLocalPublicationState {
        if self.pending_authorship_once {
            self.pending_authorship_once = false;
            return crate::oplog::local_journal_drain::ManagedLocalPublicationState::Pending(
                "synthetic authorship cut".into(),
            );
        }
        match self.authorship.get(&authority.batch_id) {
            Some(existing) if existing == authority => {
                crate::oplog::local_journal_drain::ManagedLocalPublicationState::Complete
            }
            Some(_) => crate::oplog::local_journal_drain::ManagedLocalPublicationState::Conflict(
                "divergent authorship winner".into(),
            ),
            None => {
                self.authorship
                    .insert(authority.batch_id, authority.clone());
                crate::oplog::local_journal_drain::ManagedLocalPublicationState::Complete
            }
        }
    }

    fn ensure_provider_publication(
        &mut self,
        authority: &crate::oplog::local_journal_drain::ManagedLocalDerivativeAuthority,
    ) -> crate::oplog::local_journal_drain::ManagedLocalPublicationState {
        if self.pending_provider_once {
            self.pending_provider_once = false;
            return crate::oplog::local_journal_drain::ManagedLocalPublicationState::Pending(
                "synthetic provider cut".into(),
            );
        }
        match self.provider.get(&authority.batch_id) {
            Some(existing) if existing == authority => {
                crate::oplog::local_journal_drain::ManagedLocalPublicationState::Complete
            }
            Some(_) => crate::oplog::local_journal_drain::ManagedLocalPublicationState::Conflict(
                "divergent provider winner".into(),
            ),
            None => {
                self.provider.insert(authority.batch_id, authority.clone());
                crate::oplog::local_journal_drain::ManagedLocalPublicationState::Complete
            }
        }
    }
}

fn drain_frame_to_completion(
    fixture: &mut OverlayFixture,
    database: &mut SqliteFrontier,
    tail: &mut crate::oplog::TailOverlay,
    frame: &LocalJournalFrame<ManagedLocalJournalPayloadKind>,
    checkpoint: &crate::oplog::local_journal_drain::ManagedLocalDrainCheckpoint,
    publisher: &mut DrainPublisher,
) -> crate::oplog::local_journal_drain::ManagedLocalDrainCompletion {
    let admission = crate::oplog::local_active::LocalRuntimeAdmission::unenrolled_pre_activation();
    let mut continuation = None;
    for _ in 0..64 {
        let outcome =
            crate::oplog::local_journal_drain::resume_managed_local_journal_drain_with_parts(
                &admission,
                &fixture.graph,
                &fixture.receipts,
                &mut fixture.engine,
                database,
                tail,
                frame,
                checkpoint,
                continuation.as_ref(),
                publisher,
            );
        match outcome {
            crate::oplog::local_journal_drain::ManagedLocalDrainOutcome::Complete(completion) => {
                return completion
            }
            crate::oplog::local_journal_drain::ManagedLocalDrainOutcome::Pending(next) => {
                continuation = Some(next);
            }
            other => panic!("managed-local drain did not converge: {other:?}"),
        }
    }
    panic!("managed-local drain exceeded the bounded retry allowance at {continuation:?}")
}

fn publish_journal_graph_target(
    fixture: &OverlayFixture,
    frame: &LocalJournalFrame<ManagedLocalJournalPayloadKind>,
) -> Vec<u8> {
    let record = decode_managed_local_record(frame).unwrap();
    let base = record.projection().precondition_base().bytes();
    let target = record.projection().intent().target().bytes().unwrap();
    let base_text = std::str::from_utf8(base).unwrap();
    let target_text = std::str::from_utf8(target).unwrap();
    let outcome = fixture.graph.recover_committed_journal_page_projection(
        frame.payload_digest(),
        record.projection().intent().path().as_str(),
        &crate::model::content_rev(base_text),
        base,
        target,
        &crate::model::content_rev(target_text),
    );
    let crate::model::JournalPageProjectionOutcome::Durable(durable) = outcome else {
        panic!("synthetic foreground journal projection remained pending")
    };
    assert_eq!(durable.target().target(), target);
    target.to_vec()
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
        let mut prepared = local.prepare_record(&batches[0]);
        let forbidden_before = forbidden_commit_work();
        let stats_before = journal.stats();
        let (_, response) = local.append_and_apply(&mut journal, &mut prepared);
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

    assert_eq!(prepared.record(), &decoded);

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
        let mut prepared = uninterrupted.prepare_record(prepared_batch);
        uninterrupted.append_and_apply(&mut journal, &mut prepared);
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
fn append_refuses_wrong_device_before_writing_and_proof_binds_legacy_one_barrier() {
    let (_, batches) = finalized_edit_chain("managed-record-append-source", "md", 8, 1);
    let mut fixture = OverlayFixture::new("managed-record-append-local", "md", 8);
    let mut prepared = fixture.prepare_record(&batches[0]);
    let wrong_root = fixture._dir.path().join("journal-wrong-device");
    std::fs::create_dir_all(&wrong_root).unwrap();
    let wrong_dir = Dir::open_ambient_dir(wrong_root, ambient_authority()).unwrap();
    let mut wrong = LocalJournalSegment::open(&wrong_dir, "local.segment", uuid(DEVICE + 1))
        .unwrap()
        .0;
    assert_eq!(
        append_managed_local_record(&mut wrong, &prepared),
        Err(ManagedLocalAppendError::DefinitelyNotAppended(
            ManagedLocalRecordError::WrongDurabilityProof
        ))
    );
    assert_eq!(wrong.next_sequence(), 0);
    assert_eq!(wrong.stats().frames_appended, 0);

    let (_, mut correct) = fixture.journal("correct-device");
    let append = append_managed_local_record(&mut correct, &prepared).unwrap();
    assert_eq!(append.protocol(), ManagedLocalJournalProtocol::LegacyV1);
    assert_eq!(append.receipt().device_id, uuid(DEVICE));
    assert_eq!(append.receipt().sequence, 0);
    assert_eq!(
        append.receipt().payload_digest,
        ContentDigest::of(prepared.journal_payload())
    );
    assert_eq!(
        append.receipt().data_durability_syncs,
        append.protocol().expected_successful_append_data_syncs()
    );
    let wrong_proof = append.with_data_durability_syncs_for_test(0);
    assert_eq!(
        fixture
            .engine
            .apply_appended_managed_local_record(&wrong_proof, &mut prepared),
        Err(ManagedLocalRecordError::WrongDurabilityProof)
    );
    assert_eq!(
        fixture.engine.managed_local_prefix_state().records_applied,
        0
    );
    fixture
        .engine
        .apply_appended_managed_local_record(&append, &mut prepared)
        .unwrap();
}

#[test]
fn fresh_schema2_append_proof_requires_two_syncs_and_applies_once() {
    let (_, batches) = finalized_edit_chain("managed-record-append-v2-source", "md", 8, 1);
    let mut fixture = OverlayFixture::new("managed-record-append-v2-local", "md", 8);
    let mut prepared = fixture.prepare_record(&batches[0]);
    let root = fixture._dir.path().join("journal-v2");
    std::fs::create_dir_all(&root).unwrap();
    let directory = Dir::open_ambient_dir(&root, ambient_authority()).unwrap();
    let selection =
        LocalJournalSegmentV2Selection::random("managed-local-v2.journal", uuid(DEVICE), 0)
            .unwrap();
    LocalJournalSegmentV2::<ManagedLocalJournalPayloadKind>::prepare(&directory, &selection)
        .unwrap();
    let (segment, recovery) = LocalJournalSegmentV2::open_selected(&directory, &selection).unwrap();
    assert_eq!(recovery.frames_recovered, 0);
    let mut journal = ManagedLocalJournal::from_open_v2(1, segment);

    let append = append_managed_local_record(&mut journal, &prepared).unwrap();
    assert_eq!(append.protocol(), ManagedLocalJournalProtocol::V2);
    assert_eq!(append.receipt().data_durability_syncs, 2);
    assert_eq!(
        append.receipt().data_durability_syncs,
        append.protocol().expected_successful_append_data_syncs()
    );
    fixture
        .engine
        .apply_appended_managed_local_record(&append, &mut prepared)
        .unwrap();
    assert_eq!(
        fixture.engine.managed_local_prefix_state().records_applied,
        1
    );
}

#[test]
fn managed_local_append_proof_uses_the_protocol_exact_sync_count() {
    assert_eq!(
        ManagedLocalJournalProtocol::LegacyV1.expected_successful_append_data_syncs(),
        1
    );
    assert_eq!(
        ManagedLocalJournalProtocol::V2.expected_successful_append_data_syncs(),
        2
    );
    let source = include_str!("../hot_engine.rs")
        .split("#[cfg(test)]\nmod tests")
        .next()
        .unwrap();
    assert!(source.contains("expected_successful_append_data_syncs"));
    assert!(!source.contains("data_durability_syncs != 1 | 2"));
    assert!(source.contains("fn managed_local_append_proof"));
    assert!(!source.contains("ManagedLocalAppendProof::new"));
    assert!(!source.contains("pub(crate) const fn new("));
}

#[test]
fn intervening_engine_mutation_refuses_retained_managed_candidate_before_visibility() {
    let mut fixture = OverlayFixture::new("managed-record-retained-stale", "md", 8);
    let intervening = fixture
        .engine
        .prepare_bootstrap_transaction(fixture.local_author(1_120_000), &fixture.content_edit(9))
        .unwrap();
    let prepared_batch = fixture.finalize_edit(1_120_010, 1);
    let mut prepared = fixture.prepare_record(&prepared_batch);
    let (_, mut journal) = fixture.journal("retained-stale");
    let append = append_managed_local_record(&mut journal, &prepared).unwrap();

    fixture
        .writer
        .publish_bootstrap_prepared_for_test(&intervening)
        .unwrap();
    assert!(matches!(
        fixture
            .engine
            .stage_archive_batch(intervening.manifest().batch_id())
            .unwrap()
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    let visible_before = fixture.engine.materialize_page(fixture.page_id).unwrap();

    assert!(matches!(
        fixture
            .engine
            .apply_appended_managed_local_record(&append, &mut prepared),
        Err(ManagedLocalRecordError::StaleBase)
    ));
    assert_eq!(
        fixture.engine.managed_local_prefix_state().records_applied,
        0
    );
    assert_page_semantics(
        &visible_before,
        &fixture.engine.materialize_page(fixture.page_id).unwrap(),
    );
}

#[test]
fn torn_final_frame_recovers_and_replays_only_the_complete_prefix() {
    let (_, batches) = finalized_edit_chain("managed-record-torn-source", "org", 8, 2);
    let mut live = OverlayFixture::new("managed-record-torn-live", "org", 8);
    let (journal_root, mut journal) = live.journal("torn");
    let mut first = live.prepare_record(&batches[0]);
    live.append_and_apply(&mut journal, &mut first);
    let expected_prefix = live.engine.materialize_page(live.page_id).unwrap();
    let mut second = live.prepare_record(&batches[1]);
    let (second_append, _) = live.append_and_apply(&mut journal, &mut second);
    let committed = journal.committed_bytes();
    drop(journal);

    let segment_path = journal_root.join("local.segment");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&segment_path)
        .unwrap();
    file.set_len(committed - second_append.receipt().frame_bytes / 2)
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
    let mut prepared = fixture.prepare_record(&prepared_batch);
    let (_, mut journal) = fixture.journal("collapse");
    let before_frontier = fixture.engine.accepted_frontier_root().unwrap();
    fixture.append_and_apply(&mut journal, &mut prepared);
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
fn managed_local_drain_converges_markdown_and_org_without_rewriting_exact_graph_target() {
    for (extension, exact_path) in [
        ("md", "journals/team/deep/2026_08_03.md"),
        ("org", "pages/nonstandard/nested/Drain_Note.org"),
    ] {
        let mut accepted = OverlayFixture::new_at_path(
            &format!("managed-drain-accepted-{extension}"),
            exact_path,
            12,
        );
        let batch = accepted.finalize_edit(1_081_000, 1);
        accepted.accept_and_project(&batch);
        let expected = accepted.engine.materialize_page(accepted.page_id).unwrap();
        let mut fixture = OverlayFixture::new_at_path(
            &format!("managed-drain-local-{extension}"),
            exact_path,
            12,
        );
        let (mut database, mut tail) = fixture.database_and_tail("semantic");
        let prepared = fixture.prepare_record(&batch);
        let record = frame(&prepared);
        fixture.engine.replay_managed_local_record(&record).unwrap();
        let decoded = decode_managed_local_record(&record).unwrap();
        let target = decoded
            .projection()
            .intent()
            .target()
            .bytes()
            .unwrap()
            .to_vec();
        let target_path = fixture.graph_path.join(fixture.page_path.as_str());
        fs::write(&target_path, &target).unwrap();
        fixture.graph = Graph::open(&fixture.graph_path);
        #[cfg(unix)]
        let inode_before = {
            use std::os::unix::fs::MetadataExt as _;
            fs::metadata(&target_path).unwrap().ino()
        };
        let checkpoint = crate::oplog::local_journal_drain::ManagedLocalDrainCheckpoint::initial(
            uuid(DEVICE),
            fixture.ids.workspace,
            fixture.ids.lineage,
        );
        let mut publisher = DrainPublisher {
            pending_authorship_once: true,
            pending_provider_once: true,
            ..DrainPublisher::default()
        };
        let completion = drain_frame_to_completion(
            &mut fixture,
            &mut database,
            &mut tail,
            &record,
            &checkpoint,
            &mut publisher,
        );
        assert_eq!(completion.sequence, 0);
        assert_eq!(completion.batch_id, prepared.batch_id());
        assert_eq!(completion.checkpoint.next_sequence(), 1);
        assert_eq!(completion.reclaimable_through_after_checkpoint, 0);
        let checkpoint_bytes = completion.checkpoint.encode().unwrap();
        assert_eq!(
            crate::oplog::local_journal_drain::ManagedLocalDrainCheckpoint::decode(
                &checkpoint_bytes,
                uuid(DEVICE),
                fixture.ids.workspace,
                fixture.ids.lineage,
            )
            .unwrap(),
            completion.checkpoint
        );
        assert_eq!(fs::read(&target_path).unwrap(), target);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            assert_eq!(fs::metadata(&target_path).unwrap().ino(), inode_before);
        }
        let actual = fixture.engine.materialize_page(fixture.page_id).unwrap();
        assert_page_semantics(&expected, &actual);
        let read = database.materialized_read().unwrap();
        assert_eq!(
            read.page(fixture.page_id).unwrap().unwrap().path,
            fixture.page_path
        );
        assert_eq!(
            fixture
                .engine
                .projection_work_index()
                .unwrap()
                .completed_receipts_for_path(&fixture.page_path)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(publisher.authorship.len(), 1);
        assert_eq!(publisher.provider.len(), 1);
        // Lose the returned checkpoint and every live continuation: exact
        // archive/SQLite/receipt/provider winners are adopted and derive the
        // identical checkpoint without touching graph text.
        let repeated = drain_frame_to_completion(
            &mut fixture,
            &mut database,
            &mut tail,
            &record,
            &checkpoint,
            &mut publisher,
        );
        assert_eq!(repeated.checkpoint, completion.checkpoint);
        assert_eq!(publisher.authorship.len(), 1);
        assert_eq!(publisher.provider.len(), 1);
    }
}

#[test]
fn managed_local_drain_restarts_at_every_derivative_boundary_without_duplication() {
    use crate::oplog::local_journal_drain::ManagedLocalDrainFaultPoint as Cut;

    let (_, batches) = finalized_edit_chain("managed-drain-cuts-source", "md", 8, 1);
    let mut fixture = OverlayFixture::new("managed-drain-cuts-local", "md", 8);
    let (mut database, mut tail) = fixture.database_and_tail("cuts");
    let prepared = fixture.prepare_record(&batches[0]);
    let record = frame(&prepared);
    fixture.engine.replay_managed_local_record(&record).unwrap();
    let target = decode_managed_local_record(&record)
        .unwrap()
        .projection()
        .intent()
        .target()
        .bytes()
        .unwrap()
        .to_vec();
    let target_path = fixture.graph_path.join(fixture.page_path.as_str());
    fs::write(&target_path, &target).unwrap();
    fixture.graph = Graph::open(&fixture.graph_path);
    #[cfg(unix)]
    let inode_before = {
        use std::os::unix::fs::MetadataExt as _;
        fs::metadata(&target_path).unwrap().ino()
    };
    let checkpoint = crate::oplog::local_journal_drain::ManagedLocalDrainCheckpoint::initial(
        uuid(DEVICE),
        fixture.ids.workspace,
        fixture.ids.lineage,
    );
    let admission = crate::oplog::local_active::LocalRuntimeAdmission::unenrolled_pre_activation();
    let mut publisher = DrainPublisher::default();
    for cut in [
        Cut::BeforeArchivePublication,
        Cut::AfterArchivePublication,
        Cut::BeforeEngineAcceptance,
        Cut::AfterEngineAcceptance,
        Cut::BeforeTailAdmission,
        Cut::AfterTailAdmission,
        Cut::BeforeSqliteCommit,
        Cut::AfterSqliteCommit,
        Cut::BeforeProjectionAdoption,
        Cut::AfterProjectionAdoption,
        Cut::BeforeAuthorship,
        Cut::AfterAuthorship,
        Cut::BeforeProvider,
        Cut::AfterProvider,
    ] {
        crate::oplog::local_journal_drain::fail_managed_local_drain_once_at(cut);
        let outcome =
            crate::oplog::local_journal_drain::resume_managed_local_journal_drain_with_parts(
                &admission,
                &fixture.graph,
                &fixture.receipts,
                &mut fixture.engine,
                &mut database,
                &mut tail,
                &record,
                &checkpoint,
                None,
                &mut publisher,
            );
        assert!(
            matches!(
                outcome,
                crate::oplog::local_journal_drain::ManagedLocalDrainOutcome::Pending(_)
            ),
            "cut {cut:?} did not retain the exact record: {outcome:?}"
        );
        assert_eq!(checkpoint.next_sequence(), 0);
    }
    let completion = drain_frame_to_completion(
        &mut fixture,
        &mut database,
        &mut tail,
        &record,
        &checkpoint,
        &mut publisher,
    );
    assert_eq!(completion.checkpoint.next_sequence(), 1);
    assert_eq!(publisher.authorship.len(), 1);
    assert_eq!(publisher.provider.len(), 1);
    assert_eq!(fs::read(&target_path).unwrap(), target);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        assert_eq!(fs::metadata(&target_path).unwrap().ino(), inode_before);
    }
}

#[test]
fn managed_local_drain_expands_twelve_records_in_order_and_restarts_from_durable_state() {
    let (accepted, batches) = finalized_edit_chain("managed-drain-chain-accepted", "org", 16, 12);
    let expected = accepted.engine.materialize_page(accepted.page_id).unwrap();
    let mut fixture = OverlayFixture::new("managed-drain-chain-local", "org", 16);
    let (mut database, mut tail) = fixture.database_and_tail("chain");
    let mut checkpoint = crate::oplog::local_journal_drain::ManagedLocalDrainCheckpoint::initial(
        uuid(DEVICE),
        fixture.ids.workspace,
        fixture.ids.lineage,
    );
    let mut publisher = DrainPublisher::default();
    let mut batch_ids = Vec::new();
    for (index, batch) in batches.iter().enumerate() {
        let prepared = fixture.prepare_record(batch);
        let record = frame(&prepared);
        fixture.engine.replay_managed_local_record(&record).unwrap();
        let decoded = decode_managed_local_record(&record).unwrap();
        let target = decoded.projection().intent().target().bytes().unwrap();
        fs::write(fixture.graph_path.join(fixture.page_path.as_str()), target).unwrap();
        fixture.graph = Graph::open(&fixture.graph_path);
        if matches!(index, 2 | 6 | 10) {
            publisher.pending_provider_once = true;
        }
        let completion = drain_frame_to_completion(
            &mut fixture,
            &mut database,
            &mut tail,
            &record,
            &checkpoint,
            &mut publisher,
        );
        // Persist/redecode at representative boundaries and intentionally lose
        // every live continuation before the next sequence.
        let bytes = completion.checkpoint.encode().unwrap();
        checkpoint = crate::oplog::local_journal_drain::ManagedLocalDrainCheckpoint::decode(
            &bytes,
            uuid(DEVICE),
            fixture.ids.workspace,
            fixture.ids.lineage,
        )
        .unwrap();
        batch_ids.push(completion.batch_id);
    }
    assert_eq!(checkpoint.next_sequence(), 12);
    assert_eq!(publisher.authorship.len(), 12);
    assert_eq!(publisher.provider.len(), 12);
    assert_eq!(batch_ids.len(), 12);
    assert_eq!(
        database.frontier_root().unwrap(),
        fixture.engine.accepted_frontier_root().unwrap()
    );
    assert_page_semantics(
        &expected,
        &fixture.engine.materialize_page(fixture.page_id).unwrap(),
    );
}

#[test]
fn managed_local_drain_refuses_gap_duplicate_and_wrong_graph_before_checkpoint_advance() {
    let (_, batches) = finalized_edit_chain("managed-drain-refusal-source", "md", 8, 2);
    let mut fixture = OverlayFixture::new("managed-drain-refusal-local", "md", 8);
    let (mut database, mut tail) = fixture.database_and_tail("refusal");
    let first = fixture.prepare_record(&batches[0]);
    let first_frame = frame(&first);
    fixture
        .engine
        .replay_managed_local_record(&first_frame)
        .unwrap();
    let checkpoint = crate::oplog::local_journal_drain::ManagedLocalDrainCheckpoint::initial(
        uuid(DEVICE),
        fixture.ids.workspace,
        fixture.ids.lineage,
    );
    let admission = crate::oplog::local_active::LocalRuntimeAdmission::unenrolled_pre_activation();
    let mut publisher = DrainPublisher::default();
    let outcome = crate::oplog::local_journal_drain::resume_managed_local_journal_drain_with_parts(
        &admission,
        &fixture.graph,
        &fixture.receipts,
        &mut fixture.engine,
        &mut database,
        &mut tail,
        &first_frame,
        &checkpoint,
        None,
        &mut publisher,
    );
    assert!(matches!(
        outcome,
        crate::oplog::local_journal_drain::ManagedLocalDrainOutcome::Conflict(_)
    ));
    assert_eq!(checkpoint.next_sequence(), 0);
    assert_eq!(
        fixture.writer.inspect_batch(first.batch_id()).unwrap(),
        BatchInspection::Absent
    );

    let target = decode_managed_local_record(&first_frame)
        .unwrap()
        .projection()
        .intent()
        .target()
        .bytes()
        .unwrap()
        .to_vec();
    fs::write(fixture.graph_path.join(fixture.page_path.as_str()), target).unwrap();
    fixture.graph = Graph::open(&fixture.graph_path);
    let completion = drain_frame_to_completion(
        &mut fixture,
        &mut database,
        &mut tail,
        &first_frame,
        &checkpoint,
        &mut publisher,
    );
    let duplicate =
        crate::oplog::local_journal_drain::resume_managed_local_journal_drain_with_parts(
            &admission,
            &fixture.graph,
            &fixture.receipts,
            &mut fixture.engine,
            &mut database,
            &mut tail,
            &first_frame,
            &completion.checkpoint,
            None,
            &mut publisher,
        );
    assert!(matches!(
        duplicate,
        crate::oplog::local_journal_drain::ManagedLocalDrainOutcome::Conflict(_)
    ));

    let second = fixture.prepare_record(&batches[1]);
    let second_frame = frame(&second);
    fixture
        .engine
        .replay_managed_local_record(&second_frame)
        .unwrap();
    let gap_checkpoint = crate::oplog::local_journal_drain::ManagedLocalDrainCheckpoint::initial(
        uuid(DEVICE),
        fixture.ids.workspace,
        fixture.ids.lineage,
    );
    let gap = crate::oplog::local_journal_drain::resume_managed_local_journal_drain_with_parts(
        &admission,
        &fixture.graph,
        &fixture.receipts,
        &mut fixture.engine,
        &mut database,
        &mut tail,
        &second_frame,
        &gap_checkpoint,
        None,
        &mut publisher,
    );
    assert!(matches!(
        gap,
        crate::oplog::local_journal_drain::ManagedLocalDrainOutcome::Blocked(_)
    ));
    assert_eq!(gap_checkpoint.next_sequence(), 0);
}

#[test]
fn managed_local_drain_adopts_exact_winners_and_rejects_divergent_archive_or_provider() {
    let mut source = OverlayFixture::new("managed-drain-divergence-source", "md", 8);
    let batch = source.finalize_edit(1_095_000, 1);
    source.accept_and_project(&batch);

    let mut archive_conflict = OverlayFixture::new("managed-drain-archive-conflict", "md", 8);
    let divergent = archive_conflict.finalize_edit(1_095_000, 99);
    assert_eq!(divergent.manifest().batch_id(), batch.manifest().batch_id());
    assert_ne!(
        divergent.manifest().encode().unwrap(),
        batch.manifest().encode().unwrap()
    );
    archive_conflict
        .writer
        .publish_prepared(&divergent)
        .unwrap();
    let (mut database, mut tail) = archive_conflict.database_and_tail("archive-conflict");
    let prepared = archive_conflict.prepare_record(&batch);
    let record = frame(&prepared);
    archive_conflict
        .engine
        .replay_managed_local_record(&record)
        .unwrap();
    let target = decode_managed_local_record(&record)
        .unwrap()
        .projection()
        .intent()
        .target()
        .bytes()
        .unwrap()
        .to_vec();
    fs::write(
        archive_conflict
            .graph_path
            .join(archive_conflict.page_path.as_str()),
        target,
    )
    .unwrap();
    archive_conflict.graph = Graph::open(&archive_conflict.graph_path);
    let checkpoint = crate::oplog::local_journal_drain::ManagedLocalDrainCheckpoint::initial(
        uuid(DEVICE),
        archive_conflict.ids.workspace,
        archive_conflict.ids.lineage,
    );
    let admission = crate::oplog::local_active::LocalRuntimeAdmission::unenrolled_pre_activation();
    let mut publisher = DrainPublisher::default();
    let outcome = crate::oplog::local_journal_drain::resume_managed_local_journal_drain_with_parts(
        &admission,
        &archive_conflict.graph,
        &archive_conflict.receipts,
        &mut archive_conflict.engine,
        &mut database,
        &mut tail,
        &record,
        &checkpoint,
        None,
        &mut publisher,
    );
    assert!(matches!(
        outcome,
        crate::oplog::local_journal_drain::ManagedLocalDrainOutcome::Conflict(_)
    ));
    assert_eq!(checkpoint.next_sequence(), 0);
    let BatchInspection::Ready(stored_divergent) = archive_conflict
        .writer
        .inspect_batch(divergent.manifest().batch_id())
        .unwrap()
    else {
        panic!("divergent archive winner disappeared")
    };
    assert_eq!(
        stored_divergent.manifest().encode().unwrap(),
        divergent.manifest().encode().unwrap()
    );

    let mut provider_conflict = OverlayFixture::new("managed-drain-provider-conflict", "md", 8);
    let (mut database, mut tail) = provider_conflict.database_and_tail("provider-conflict");
    let prepared = provider_conflict.prepare_record(&batch);
    let record = frame(&prepared);
    provider_conflict
        .engine
        .replay_managed_local_record(&record)
        .unwrap();
    let target = decode_managed_local_record(&record)
        .unwrap()
        .projection()
        .intent()
        .target()
        .bytes()
        .unwrap()
        .to_vec();
    fs::write(
        provider_conflict
            .graph_path
            .join(provider_conflict.page_path.as_str()),
        target,
    )
    .unwrap();
    provider_conflict.graph = Graph::open(&provider_conflict.graph_path);
    let checkpoint = crate::oplog::local_journal_drain::ManagedLocalDrainCheckpoint::initial(
        uuid(DEVICE),
        provider_conflict.ids.workspace,
        provider_conflict.ids.lineage,
    );
    let mut publisher = DrainPublisher {
        pending_provider_once: true,
        ..DrainPublisher::default()
    };
    let pending = crate::oplog::local_journal_drain::resume_managed_local_journal_drain_with_parts(
        &admission,
        &provider_conflict.graph,
        &provider_conflict.receipts,
        &mut provider_conflict.engine,
        &mut database,
        &mut tail,
        &record,
        &checkpoint,
        None,
        &mut publisher,
    );
    assert!(matches!(
        pending,
        crate::oplog::local_journal_drain::ManagedLocalDrainOutcome::Pending(_)
    ));
    let mut divergent_authority = publisher.authorship[&prepared.batch_id()].clone();
    divergent_authority.accepted_frontier_digest = ContentDigest::of(b"divergent provider");
    publisher
        .provider
        .insert(prepared.batch_id(), divergent_authority.clone());
    let conflict = crate::oplog::local_journal_drain::resume_managed_local_journal_drain_with_parts(
        &admission,
        &provider_conflict.graph,
        &provider_conflict.receipts,
        &mut provider_conflict.engine,
        &mut database,
        &mut tail,
        &record,
        &checkpoint,
        None,
        &mut publisher,
    );
    assert!(matches!(
        conflict,
        crate::oplog::local_journal_drain::ManagedLocalDrainOutcome::Conflict(_)
    ));
    assert_eq!(
        publisher.provider[&prepared.batch_id()],
        divergent_authority
    );
    assert_eq!(checkpoint.next_sequence(), 0);
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

#[test]
#[ignore = "manual release benchmark; includes normal archive/SQLite/receipt derivative cost"]
fn managed_local_drain_manual_release_benchmark() {
    let pages = std::env::var("TINE_MANAGED_LOCAL_DRAIN_BENCH_PAGES")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(|part| part.parse::<usize>().unwrap())
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec![100, 10_000]);
    let edits = std::env::var("TINE_MANAGED_LOCAL_DRAIN_BENCH_EDITS")
        .ok()
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(3);
    assert!(edits > 0);

    let mut structural = Vec::new();
    for page_count in pages {
        let (_, batches) = finalized_edit_chain(
            &format!("managed-drain-bench-source-{page_count}"),
            "md",
            page_count,
            edits,
        );
        let mut fixture = OverlayFixture::new(
            &format!("managed-drain-bench-local-{page_count}"),
            "md",
            page_count,
        );
        let (mut database, mut tail) = fixture.database_and_tail("bench");
        let mut checkpoint =
            crate::oplog::local_journal_drain::ManagedLocalDrainCheckpoint::initial(
                uuid(DEVICE),
                fixture.ids.workspace,
                fixture.ids.lineage,
            );
        let mut publisher = DrainPublisher::default();
        let mut samples = Vec::new();
        let mut observed_work = Vec::new();
        for batch in &batches {
            let prepared = fixture.prepare_record(batch);
            let record = frame(&prepared);
            fixture.engine.replay_managed_local_record(&record).unwrap();
            publish_journal_graph_target(&fixture, &record);
            let started = Instant::now();
            let completion = drain_frame_to_completion(
                &mut fixture,
                &mut database,
                &mut tail,
                &record,
                &checkpoint,
                &mut publisher,
            );
            samples.push(started.elapsed().as_secs_f64() * 1_000.0);
            checkpoint = completion.checkpoint;
            observed_work.push(completion.work);
        }
        samples.sort_by(f64::total_cmp);
        let p50 = samples[samples.len() / 2];
        for work in &observed_work {
            assert_eq!(work.records, 1);
            assert_eq!(work.graph_target_point_reads, 1);
            assert_eq!(work.accepted_events, 1);
            assert_eq!(work.projection_work_point_reads, 1);
            assert_eq!(work.authorship_attempts, 1);
            assert_eq!(work.provider_attempts, 1);
        }
        println!(
            "managed-local-drain pages={page_count} p50_ms={p50:.6} samples_ms={samples:?} work={observed_work:?}"
        );
        structural.push(observed_work);
    }
    assert!(structural.windows(2).all(|pair| pair[0] == pair[1]));
}
