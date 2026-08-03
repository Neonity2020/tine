use std::time::Instant;

use cap_std::{ambient_authority, fs::Dir};
use tine_storage::{LocalJournalFrame, LocalJournalSegment};

use super::*;
use crate::fast_commit::forbidden_commit_work;
use crate::oplog::{
    ApplicationRuntimeRoot, HotOverlayApplyOutcome, HotOverlayError, LogseqIdentityOrigin,
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
    _dir: TestDir,
}

impl OverlayFixture {
    fn new(label: &str, extension: &str, pages: usize) -> Self {
        Self::build(label, extension, pages, false)
    }

    fn new_sparse(label: &str, extension: &str, pages: usize) -> Self {
        Self::build(label, extension, pages, true)
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
        // Keep each setup transaction below the fixed-capacity portable-path test
        // index. The measured hot path is unchanged; this only builds the accepted
        // 10,000-page base in bounded bootstrap batches.
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
            content: format!("overlay revision {generation}"),
        }])
    }

    fn prepare_draft(
        &self,
        seed: u128,
        generation: usize,
    ) -> crate::oplog::PreparedHotOverlayCommit {
        let draft = self
            .engine
            .draft_author_transaction(
                self.local_author(seed),
                BatchOrigin::LocalMutation,
                &self.content_edit(generation),
            )
            .unwrap();
        self.engine
            .prepare_hot_overlay_draft(draft, self.engine.hot_overlay_next_sequence())
            .unwrap()
    }

    fn journal(&self, label: &str) -> LocalJournalSegment<ObjectKind> {
        let root = self._dir.path().join(format!("journal-{label}"));
        std::fs::create_dir_all(&root).unwrap();
        let dir = Dir::open_ambient_dir(root, ambient_authority()).unwrap();
        LocalJournalSegment::open(&dir, "local.segment", uuid(DEVICE))
            .unwrap()
            .0
    }

    fn append_and_apply(
        &mut self,
        journal: &mut LocalJournalSegment<ObjectKind>,
        prepared: &crate::oplog::PreparedHotOverlayCommit,
    ) -> MaterializedPage {
        let append = journal
            .append(prepared.payload_kind(), prepared.journal_payload())
            .unwrap();
        match self
            .engine
            .apply_durable_hot_overlay(&append, prepared)
            .unwrap()
        {
            HotOverlayApplyOutcome::Applied { page, .. } => page,
        }
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

fn assert_page_semantics(left: &MaterializedPage, right: &MaterializedPage) {
    assert_eq!(left.page_id, right.page_id);
    assert_eq!(left.home_document_id, right.home_document_id);
    assert_eq!(left.name, right.name);
    assert_eq!(left.path, right.path);
    assert_eq!(left.kind, right.kind);
    assert_eq!(left.preamble, right.preamble);
    assert_eq!(left.blocks, right.blocks);
}

#[test]
fn committed_hot_overlay_boundary_starts_empty() {
    let engine = Ids::new().engine();
    assert_eq!(engine.hot_overlay_work().commits_applied, 0);
    assert_eq!(engine.hot_overlay_len(), 0);
}

#[test]
fn markdown_and_org_hot_materialization_matches_accepted_sqlite_semantics() {
    for extension in ["md", "org"] {
        let mut hot =
            OverlayFixture::new_sparse(&format!("overlay-semantic-hot-{extension}"), extension, 8);
        let mut journal = hot.journal("semantic");
        let prepared = hot.prepare_draft(1_070_000, 1);
        let response = hot.append_and_apply(&mut journal, &prepared);
        let direct = hot
            .engine
            .materialize_current_page_at_path(&hot.page_path)
            .unwrap()
            .unwrap();
        assert_page_semantics(prepared.post_page(), &response);
        assert_page_semantics(&response, &direct);

        let mut cold =
            OverlayFixture::new_sparse(&format!("overlay-semantic-cold-{extension}"), extension, 8);
        cold.accept_bootstrap_edit(1_071_000, 1);
        let accepted = cold.engine.materialize_page(cold.page_id).unwrap();
        assert_page_semantics(&direct, &accepted);
        let (sqlite_page, sqlite_blocks) = cold.sqlite_page_and_blocks();
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
        assert_eq!(
            direct.blocks[0].logseq_uuid,
            Some(LogseqUuid::from_uuid(uuid(LOGSEQ_UUID)))
        );
        assert_eq!(
            direct.blocks[0].logseq_identity_origin,
            Some(LogseqIdentityOrigin::PolicyGenerated {
                reason: PolicyGeneratedAnchorReason::Export,
            })
        );
    }
}

#[test]
fn consecutive_local_edits_compose_before_any_cold_or_derivative_work() {
    let mut fixture = OverlayFixture::new("overlay-compose", "md", 100);
    let mut journal = fixture.journal("compose");
    let work_before = fixture.engine.hot_overlay_work();
    for generation in 1..=24 {
        let prepared = fixture.prepare_draft(1_080_000 + generation as u128 * 10, generation);
        let forbidden_before = forbidden_commit_work();
        let response = fixture.append_and_apply(&mut journal, &prepared);
        let current = fixture
            .engine
            .materialize_current_page_at_path(&fixture.page_path)
            .unwrap()
            .unwrap();
        assert_page_semantics(&response, &current);
        assert_eq!(current.stats.catalog_documents_loaded, 0);
        assert_eq!(
            current.blocks[0].content,
            format!("overlay revision {generation}")
        );
        let forbidden = forbidden_commit_work().since(forbidden_before);
        assert!(
            forbidden.is_none(),
            "overlay advance/materialization performed forbidden work: {forbidden:?}"
        );
    }
    let work = fixture.engine.hot_overlay_work().since(work_before);
    assert_eq!(work.commits_applied, 24);
    assert_eq!(work.documents_imported, 24);
    assert_eq!(work.page_materializations, 24);
    assert_eq!(fixture.engine.hot_overlay_len(), 24);
}

#[test]
fn recovered_prefix_replays_identically_and_duplicate_replay_is_refused_without_effect() {
    let mut uninterrupted = OverlayFixture::new("overlay-replay-live", "org", 16);
    let mut journal = uninterrupted.journal("replay");
    let mut frames = Vec::new();
    for generation in 1..=12 {
        let prepared = uninterrupted.prepare_draft(1_090_000 + generation as u128 * 10, generation);
        uninterrupted.append_and_apply(&mut journal, &prepared);
        frames.push(LocalJournalFrame::new(
            uuid(DEVICE),
            prepared.sequence(),
            prepared.payload_kind(),
            prepared.journal_payload().to_vec(),
        ));
    }
    let expected = uninterrupted
        .engine
        .materialize_current_page_at_path(&uninterrupted.page_path)
        .unwrap()
        .unwrap();

    let mut recovered = OverlayFixture::new("overlay-replay-fresh", "org", 16);
    for frame in &frames {
        recovered.engine.replay_hot_overlay_frame(frame).unwrap();
    }
    let actual = recovered
        .engine
        .materialize_current_page_at_path(&recovered.page_path)
        .unwrap()
        .unwrap();
    assert_page_semantics(&expected, &actual);
    let before = actual.clone();
    assert!(matches!(
        recovered.engine.replay_hot_overlay_frame(&frames[0]),
        Err(HotOverlayError::OutOfOrder { .. })
    ));
    let after = recovered
        .engine
        .materialize_current_page_at_path(&recovered.page_path)
        .unwrap()
        .unwrap();
    assert_page_semantics(&before, &after);
}

#[test]
fn stale_order_corruption_binding_and_frontier_failures_leave_visible_state_unchanged() {
    let source = OverlayFixture::new("overlay-refusal-source", "md", 8);
    let first = source.prepare_draft(1_100_000, 1);
    let first_frame = LocalJournalFrame::new(
        uuid(DEVICE),
        first.sequence(),
        first.payload_kind(),
        first.journal_payload().to_vec(),
    );

    let mut stale = OverlayFixture::new("overlay-refusal-stale", "md", 8);
    let before = stale.engine.materialize_page(stale.page_id).unwrap();
    stale.accept_bootstrap_edit(1_100_100, 9);
    let accepted_before = stale.engine.materialize_page(stale.page_id).unwrap();
    assert!(matches!(
        stale.engine.replay_hot_overlay_frame(&first_frame),
        Err(HotOverlayError::StaleBase)
    ));
    assert_page_semantics(
        &accepted_before,
        &stale.engine.materialize_page(stale.page_id).unwrap(),
    );
    assert_ne!(before.blocks[0].content, accepted_before.blocks[0].content);

    let mut live = OverlayFixture::new("overlay-refusal-live", "md", 8);
    live.engine.replay_hot_overlay_frame(&first_frame).unwrap();
    let second = live.prepare_draft(1_100_200, 2);
    let second_frame = LocalJournalFrame::new(
        uuid(DEVICE),
        second.sequence(),
        second.payload_kind(),
        second.journal_payload().to_vec(),
    );
    let mut out_of_order = OverlayFixture::new("overlay-refusal-order", "md", 8);
    let unchanged = out_of_order
        .engine
        .materialize_page(out_of_order.page_id)
        .unwrap();
    assert!(matches!(
        out_of_order.engine.replay_hot_overlay_frame(&second_frame),
        Err(HotOverlayError::OutOfOrder { .. })
    ));
    assert_page_semantics(
        &unchanged,
        &out_of_order
            .engine
            .materialize_page(out_of_order.page_id)
            .unwrap(),
    );

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
        out_of_order.engine.replay_hot_overlay_frame(&corrupt),
        Err(HotOverlayError::CorruptPayload(_)) | Err(HotOverlayError::Engine(_))
    ));
    assert_page_semantics(
        &unchanged,
        &out_of_order
            .engine
            .materialize_page(out_of_order.page_id)
            .unwrap(),
    );

    let mut wrong_workspace = ShardedHotEngine::new(
        WorkspaceId::from_uuid(uuid(1_100_300)),
        source.ids.lineage,
        source.ids.catalog,
    );
    assert!(matches!(
        wrong_workspace.replay_hot_overlay_frame(&first_frame),
        Err(HotOverlayError::Engine(
            EngineError::WorkspaceMismatch { .. }
        ))
    ));
    let wrong_device = LocalJournalFrame::new(
        uuid(DEVICE + 1),
        first.sequence(),
        first.payload_kind(),
        first.journal_payload().to_vec(),
    );
    assert!(matches!(
        out_of_order.engine.replay_hot_overlay_frame(&wrong_device),
        Err(HotOverlayError::CorruptPayload(_))
    ));

    let old_frontier = live.engine.accepted_frontier_root().unwrap();
    let live_before = live.engine.materialize_page(live.page_id).unwrap();
    assert!(matches!(
        live.engine.collapse_hot_overlay(&old_frontier),
        Err(HotOverlayError::AcceptedFrontierMismatch)
    ));
    assert_page_semantics(
        &live_before,
        &live.engine.materialize_page(live.page_id).unwrap(),
    );
}

#[test]
fn durable_receipt_device_binding_matches_replay_and_preserves_visible_state() {
    let mut fixture = OverlayFixture::new("overlay-device-receipt", "md", 8);
    let prepared = fixture.prepare_draft(1_105_000, 1);
    let before = fixture
        .engine
        .materialize_current_page_at_path(&fixture.page_path)
        .unwrap()
        .unwrap();

    let root = fixture._dir.path().join("journal-device-b");
    std::fs::create_dir_all(&root).unwrap();
    let dir = Dir::open_ambient_dir(root, ambient_authority()).unwrap();
    let device_b = uuid(DEVICE + 1);
    let mut journal = LocalJournalSegment::open(&dir, "local.segment", device_b)
        .unwrap()
        .0;
    let append = journal
        .append(prepared.payload_kind(), prepared.journal_payload())
        .unwrap();
    assert_eq!(append.device_id, device_b);

    let live_error = fixture
        .engine
        .apply_durable_hot_overlay(&append, &prepared)
        .unwrap_err();
    assert_eq!(
        live_error,
        HotOverlayError::CorruptPayload("manifest binding differs from journal binding".into())
    );
    assert_eq!(fixture.engine.hot_overlay_len(), 0);
    assert_page_semantics(
        &before,
        &fixture
            .engine
            .materialize_current_page_at_path(&fixture.page_path)
            .unwrap()
            .unwrap(),
    );

    let mut recovered = Vec::new();
    assert_eq!(journal.replay(|frame| recovered.push(frame)).unwrap(), 1);
    assert_eq!(recovered[0].device_id(), device_b);
    let replay_error = fixture
        .engine
        .replay_hot_overlay_frame(&recovered[0])
        .unwrap_err();
    assert_eq!(replay_error, live_error);
    assert_eq!(fixture.engine.hot_overlay_len(), 0);
    assert_page_semantics(
        &before,
        &fixture
            .engine
            .materialize_current_page_at_path(&fixture.page_path)
            .unwrap()
            .unwrap(),
    );
}

#[test]
fn exact_accepted_catchup_collapses_overlay_and_later_edits_continue() {
    let mut fixture = OverlayFixture::new("overlay-collapse", "md", 12);
    let draft = fixture
        .engine
        .draft_author_transaction(
            fixture.local_author(1_110_000),
            BatchOrigin::LocalMutation,
            &fixture.content_edit(1),
        )
        .unwrap();
    let prepared = fixture
        .engine
        .finalize_author_transaction(draft, &fixture.graph, &fixture.receipts, fixture.binding)
        .unwrap();
    let overlay = fixture
        .engine
        .prepare_hot_overlay_commit(&prepared, fixture.page_id, 0)
        .unwrap();
    let mut journal = fixture.journal("collapse");
    let before_frontier = fixture.engine.accepted_frontier_root().unwrap();
    fixture.append_and_apply(&mut journal, &overlay);
    assert!(matches!(
        fixture.engine.collapse_hot_overlay(&before_frontier),
        Err(HotOverlayError::AcceptedFrontierMismatch)
    ));

    fixture.writer.publish_prepared(&prepared).unwrap();
    assert!(matches!(
        fixture
            .engine
            .stage_archive_batch(prepared.manifest().batch_id())
            .unwrap()
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    let expected = fixture.engine.materialize_page(fixture.page_id).unwrap();
    let accepted_frontier = fixture.engine.accepted_frontier_root().unwrap();
    assert_eq!(
        fixture
            .engine
            .collapse_hot_overlay(&accepted_frontier)
            .unwrap(),
        1
    );
    assert_eq!(fixture.engine.hot_overlay_len(), 0);
    let collapsed = fixture.engine.materialize_page(fixture.page_id).unwrap();
    assert_page_semantics(&expected, &collapsed);

    let later = fixture.prepare_draft(1_110_100, 2);
    let later_page = fixture.append_and_apply(&mut journal, &later);
    assert_eq!(later_page.blocks[0].content, "overlay revision 2");
    assert_eq!(fixture.engine.hot_overlay_len(), 1);
}

#[test]
#[ignore = "manual release benchmark; prints every raw sample"]
fn committed_hot_overlay_manual_release_benchmark() {
    let mut observations = Vec::new();
    for pages in [100_usize, 10_000] {
        let mut fixture = OverlayFixture::new(&format!("overlay-bench-{pages}"), "md", pages);
        let work_before = fixture.engine.hot_overlay_work();
        let mut samples = Vec::new();
        for generation in 1..=55 {
            let prepared = fixture.prepare_draft(
                1_120_000 + pages as u128 * 100 + generation as u128 * 2,
                generation,
            );
            let frame = LocalJournalFrame::new(
                uuid(DEVICE),
                prepared.sequence(),
                prepared.payload_kind(),
                prepared.journal_payload().to_vec(),
            );
            let started = Instant::now();
            fixture.engine.replay_hot_overlay_frame(&frame).unwrap();
            let page = fixture
                .engine
                .materialize_current_page_at_path(&fixture.page_path)
                .unwrap()
                .unwrap();
            assert_eq!(page.stats.catalog_documents_loaded, 0);
            let elapsed = started.elapsed().as_secs_f64() * 1_000.0;
            if generation > 5 {
                samples.push(elapsed);
            }
        }
        let work = fixture.engine.hot_overlay_work().since(work_before);
        assert_eq!(work.commits_applied, 55);
        assert_eq!(work.documents_imported, 55);
        assert_eq!(work.page_materializations, 55);
        samples.sort_by(f64::total_cmp);
        let p50 = samples[samples.len() / 2];
        println!(
            "hot_overlay_benchmark pages={pages} n={} p50_ms={p50:.6} raw_ms={samples:?}",
            samples.len()
        );
        assert!(p50 <= 5.0, "{pages} pages p50 {p50:.3} ms exceeds 5 ms");
        observations.push((pages, p50, work));
    }
    assert_eq!(observations[0].2, observations[1].2);
    assert!(
        observations[1].1 <= observations[0].1 * 2.0 + 0.05,
        "10,000-page p50 is not bounded against 100 pages: {observations:?}"
    );
}
