use super::hot_engine::{test_block_home, MAX_HOT_NON_CATALOG_DOCUMENTS};
use std::path::{Path, PathBuf};

use crate::oplog::{
    AuthorBatch, BatchCausalDot, BatchDisposition, BatchError, BatchId, BatchInspection,
    BatchOrigin, BlockDelta, BlockLocation, BlockOwner, BlockRestore, CausalPeerId,
    ConflictResolutionIntent, ContentDigest, CrdtPeerCounter, CrdtPeerId, DeviceId,
    DocumentCausalDigest, DocumentDependencies, DocumentId, DocumentKey, EngineError, FrontierV2,
    ImmutableHomeClaim, ImmutableHomeConflict, ImmutableHomeEvidence, LineageDigest,
    LogseqIdentityMutation, LogseqIdentityOrigin, LogseqIdentityTrigger, LogseqUuid,
    LogseqUuidResolution, ManagedPath, ManagedTextKind, MembershipClaim, MembershipDelta,
    ObjectKind, ObjectStore, OperationBatch, OperationObject, OperationTransaction, PageDelta,
    PageId, PagePreambleDelta, PagePreambleState, PageState, PolicyGeneratedAnchorReason,
    PreparedBatch, ProjectionEndpointBinding, ProjectionEndpointId, ProjectionReceiptStore,
    SemanticEffect, SemanticEffectDigest, SemanticError, SemanticOperation, SessionId,
    ShardedHotEngine, StoreError, ValidatedBatch, WorkspaceId, WorkspaceStatus,
    WriterIncarnationId, MANAGED_ENTITY_SET_VERSION, OPERATION_SCHEMA_VERSION,
    SEMANTIC_EFFECT_SCHEMA_VERSION,
};
use crate::Graph;
use loro::{ExportMode, LoroDoc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("tine-oplog-hot-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy)]
struct Ids {
    workspace: WorkspaceId,
    lineage: LineageDigest,
    catalog: DocumentId,
    page_a: PageId,
    page_b: PageId,
    page_c: PageId,
    home_a: DocumentId,
    home_b: DocumentId,
    home_c: DocumentId,
    block_a: crate::oplog::BlockId,
    block_c: crate::oplog::BlockId,
}

impl Ids {
    fn new() -> Self {
        Self {
            workspace: WorkspaceId::from_uuid(uuid(1)),
            lineage: LineageDigest::of(b"lineage"),
            catalog: DocumentId::from_uuid(uuid(2)),
            page_a: PageId::from_uuid(uuid(10)),
            page_b: PageId::from_uuid(uuid(11)),
            page_c: PageId::from_uuid(uuid(12)),
            home_a: DocumentId::from_uuid(uuid(20)),
            home_b: DocumentId::from_uuid(uuid(21)),
            home_c: DocumentId::from_uuid(uuid(22)),
            block_a: crate::oplog::BlockId::from_uuid(uuid(30)),
            block_c: crate::oplog::BlockId::from_uuid(uuid(31)),
        }
    }

    fn engine(self) -> ShardedHotEngine {
        ShardedHotEngine::new(self.workspace, self.lineage, self.catalog)
    }

    fn block_home_a(self) -> DocumentId {
        test_block_home(self.block_a)
    }

    fn block_home_c(self) -> DocumentId {
        test_block_home(self.block_c)
    }
}

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn path(value: &str) -> ManagedPath {
    ManagedPath::parse(value).unwrap()
}

fn claim_home(block_id: crate::oplog::BlockId, discriminator: DocumentId) -> DocumentId {
    DocumentId::from_uuid(Uuid::from_u128(
        0x4000_0000_0000_0000_0000_0000_0000_0000
            ^ block_id.as_uuid().as_u128().rotate_left(64)
            ^ discriminator.as_uuid().as_u128(),
    ))
}

fn author(batch: u128, peer: u64) -> AuthorBatch {
    AuthorBatch {
        batch_id: BatchId::from_uuid(uuid(batch)),
        author_device_id: DeviceId::from_uuid(uuid(1_000 + peer as u128)),
        author_session_id: SessionId::from_uuid(uuid(2_000 + peer as u128)),
        crdt_peer_id: CrdtPeerId::from_u64(peer),
        causal_peer_id: CausalPeerId::from_key(WriterIncarnationId::fixture_for_device(
            DeviceId::from_uuid(uuid(1_000 + peer as u128)),
        )),
    }
}

fn tx(operations: Vec<SemanticOperation>) -> OperationTransaction {
    OperationTransaction::new(operations).unwrap()
}

fn publish_fixture(store: &ObjectStore, prepared: &PreparedBatch) {
    store.publish_prepared_fixture(prepared).unwrap();
}

fn stage_fixture_manifest(store: &ObjectStore, prepared: &PreparedBatch) {
    let bytes = prepared.manifest().encode().unwrap();
    store.stage_manifest_bytes(&bytes).unwrap();
}

fn ready(store: &ObjectStore, prepared: &PreparedBatch) -> ValidatedBatch {
    publish_fixture(store, prepared);
    match store.inspect_batch(prepared.manifest().batch_id()).unwrap() {
        BatchInspection::Ready(batch) => batch,
        other => panic!("expected Ready, found {other:?}"),
    }
}

fn semantic_effect(prepared: &PreparedBatch) -> SemanticEffect {
    let semantic = prepared
        .objects()
        .iter()
        .find(|object| object.kind() == ObjectKind::SemanticEffect)
        .expect("prepared batch has one semantic effect");
    SemanticEffect::decode(semantic.payload()).unwrap()
}

fn store(dir: &TestDir, ids: Ids) -> ObjectStore {
    ObjectStore::open(&dir.path().join("store"), ids.workspace).unwrap()
}

fn paged_fatal_evidence(engine: &ShardedHotEngine) -> Option<ImmutableHomeEvidence> {
    let mut cursor = None;
    let mut conflicts = Vec::new();
    loop {
        let page = engine.fatal_evidence_page(cursor, 1).unwrap()?;
        assert!(page.conflicts().len() <= 1);
        conflicts.extend_from_slice(page.conflicts());
        cursor = page.next();
        if cursor.is_none() {
            return Some(ImmutableHomeEvidence::new(conflicts));
        }
    }
}

fn genesis(ids: Ids, engine: &ShardedHotEngine) -> PreparedBatch {
    engine
        .prepare_fixture_transaction(
            author(100, 100),
            &tx(vec![
                SemanticOperation::CreatePage {
                    page_id: ids.page_a,
                    home_document_id: ids.home_a,
                    name: crate::oplog::LogicalPageName::parse("A").unwrap(),
                    path: path("pages/A.md"),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreatePage {
                    page_id: ids.page_b,
                    home_document_id: ids.home_b,
                    name: crate::oplog::LogicalPageName::parse("B").unwrap(),
                    path: path("pages/B.md"),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreatePage {
                    page_id: ids.page_c,
                    home_document_id: ids.home_c,
                    name: crate::oplog::LogicalPageName::parse("C").unwrap(),
                    path: path("pages/C.md"),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id: ids.block_a,
                        home_document_id: test_block_home(ids.block_a),
                    },
                    page_id: ids.page_a,
                    parent: None,
                    order: "a".into(),
                    content: "home A content".into(),
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id: ids.block_c,
                        home_document_id: test_block_home(ids.block_c),
                    },
                    page_id: ids.page_c,
                    parent: None,
                    order: "c".into(),
                    content: "unrelated content".into(),
                },
            ]),
        )
        .unwrap()
}

fn pages_only_genesis(ids: Ids, engine: &ShardedHotEngine, batch: u128) -> PreparedBatch {
    engine
        .prepare_fixture_transaction(
            author(batch, batch as u64),
            &tx(vec![
                SemanticOperation::CreatePage {
                    page_id: ids.page_a,
                    home_document_id: ids.home_a,
                    name: crate::oplog::LogicalPageName::parse("A").unwrap(),
                    path: path("pages/A.md"),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreatePage {
                    page_id: ids.page_b,
                    home_document_id: ids.home_b,
                    name: crate::oplog::LogicalPageName::parse("B").unwrap(),
                    path: path("pages/B.md"),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreatePage {
                    page_id: ids.page_c,
                    home_document_id: ids.home_c,
                    name: crate::oplog::LogicalPageName::parse("C").unwrap(),
                    path: path("pages/C.md"),
                    kind: ManagedTextKind::Page,
                },
            ]),
        )
        .unwrap()
}

fn create_blocks(
    engine: &ShardedHotEngine,
    batch: u128,
    blocks: &[(crate::oplog::BlockId, PageId, DocumentId, &str)],
) -> PreparedBatch {
    engine
        .prepare_fixture_transaction(
            author(batch, batch as u64),
            &tx(blocks
                .iter()
                .map(|(block_id, page_id, home_document_id, order)| {
                    SemanticOperation::CreateBlock {
                        block: BlockLocation {
                            block_id: *block_id,
                            home_document_id: claim_home(*block_id, *home_document_id),
                        },
                        page_id: *page_id,
                        parent: None,
                        order: (*order).into(),
                        content: format!("batch {batch} block {block_id}"),
                    }
                })
                .collect()),
        )
        .unwrap()
}

#[test]
fn author_outline_builds_page_membership_once_for_many_independent_deletes() {
    for blocks in [8_usize, 64] {
        let ids = Ids::new();
        let dir = TestDir::new(&format!("author-outline-delete-{blocks}"));
        let archive_path = dir.path().join("archive");
        let writer = ObjectStore::open(&archive_path, ids.workspace).unwrap();
        let mut engine = ShardedHotEngine::with_clean_archive_store_for_test(
            ObjectStore::open(&archive_path, ids.workspace).unwrap(),
            ids.lineage,
            ids.catalog,
        );
        let block_ids = (0..blocks)
            .map(|index| crate::oplog::BlockId::from_uuid(uuid(70_000 + index as u128)))
            .collect::<Vec<_>>();
        let mut baseline_operations = vec![SemanticOperation::CreatePage {
            page_id: ids.page_a,
            home_document_id: ids.home_a,
            name: crate::oplog::LogicalPageName::parse("Outline Deletes").unwrap(),
            path: path("pages/outline-deletes.md"),
            kind: ManagedTextKind::Page,
        }];
        baseline_operations.extend(block_ids.iter().enumerate().map(|(index, block_id)| {
            SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id: *block_id,
                    home_document_id: test_block_home(*block_id),
                },
                page_id: ids.page_a,
                parent: None,
                order: format!("{index:04}"),
                content: format!("independent root {index}"),
            }
        }));
        let baseline = engine
            .prepare_fixture_transaction(
                author(70_100 + blocks as u128, 70_100 + blocks as u64),
                &tx(baseline_operations),
            )
            .unwrap();
        publish_fixture(&writer, &baseline);
        assert!(matches!(
            engine
                .stage_archive_batch(baseline.manifest().batch_id())
                .unwrap()
                .disposition,
            BatchDisposition::Accepted { .. }
        ));

        let before = engine.instrumentation();
        let deletion = engine
            .prepare_fixture_transaction(
                author(70_200 + blocks as u128, 70_200 + blocks as u64),
                &tx(block_ids
                    .iter()
                    .map(|block_id| SemanticOperation::DeleteSubtree {
                        root_block_id: *block_id,
                        page_id: ids.page_a,
                    })
                    .collect()),
            )
            .unwrap();
        let after = engine.instrumentation();
        let builds = after.author_outline_builds - before.author_outline_builds;
        let membership_reads =
            after.author_outline_membership_reads - before.author_outline_membership_reads;
        let traversal_nodes =
            after.author_outline_traversal_nodes - before.author_outline_traversal_nodes;
        eprintln!(
            "author outline blocks={blocks} builds={builds} membership_reads={membership_reads} traversal_nodes={traversal_nodes}"
        );
        assert_eq!(builds, 1);
        assert_eq!(membership_reads, blocks);
        assert_eq!(traversal_nodes, blocks);
        publish_fixture(&writer, &deletion);
        assert!(matches!(
            engine
                .stage_archive_batch(deletion.manifest().batch_id())
                .unwrap()
                .disposition,
            BatchDisposition::Accepted { .. }
        ));
        assert!(engine
            .materialize_page(ids.page_a)
            .unwrap()
            .blocks
            .is_empty());

        drop(engine);
        let mut replay = ShardedHotEngine::with_clean_archive_store_for_test(
            ObjectStore::open(&archive_path, ids.workspace).unwrap(),
            ids.lineage,
            ids.catalog,
        );
        for batch_id in [
            baseline.manifest().batch_id(),
            deletion.manifest().batch_id(),
        ] {
            assert!(matches!(
                replay.stage_archive_batch(batch_id).unwrap().disposition,
                BatchDisposition::Accepted { .. }
            ));
        }
        assert!(replay
            .materialize_page(ids.page_a)
            .unwrap()
            .blocks
            .is_empty());
    }
}

#[test]
fn author_outline_tracks_mixed_operations_and_owner_transfer_in_order() {
    let ids = Ids::new();
    let dir = TestDir::new("author-outline-mixed");
    let archive_path = dir.path().join("archive");
    let writer = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let mut engine = ShardedHotEngine::with_clean_archive_store_for_test(
        ObjectStore::open(&archive_path, ids.workspace).unwrap(),
        ids.lineage,
        ids.catalog,
    );
    let z = crate::oplog::BlockId::from_uuid(uuid(71_001));
    let unchanged = crate::oplog::BlockId::from_uuid(uuid(71_002));
    let root_b = crate::oplog::BlockId::from_uuid(uuid(71_003));
    let transferred = crate::oplog::BlockId::from_uuid(uuid(71_004));
    let created = crate::oplog::BlockId::from_uuid(uuid(71_005));
    let location = |block_id| BlockLocation {
        block_id,
        home_document_id: test_block_home(block_id),
    };
    let baseline = engine
        .prepare_fixture_transaction(
            author(71_100, 71_100),
            &tx(vec![
                SemanticOperation::CreatePage {
                    page_id: ids.page_a,
                    home_document_id: ids.home_a,
                    name: crate::oplog::LogicalPageName::parse("Outline A").unwrap(),
                    path: path("pages/outline-a.md"),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreatePage {
                    page_id: ids.page_b,
                    home_document_id: ids.home_b,
                    name: crate::oplog::LogicalPageName::parse("Outline B").unwrap(),
                    path: path("pages/outline-b.md"),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreateBlock {
                    block: location(z),
                    page_id: ids.page_a,
                    parent: None,
                    order: "z".into(),
                    content: "same-page move anchor".into(),
                },
                SemanticOperation::CreateBlock {
                    block: location(unchanged),
                    page_id: ids.page_a,
                    parent: None,
                    order: "w".into(),
                    content: "unchanged restore".into(),
                },
                SemanticOperation::CreateBlock {
                    block: location(root_b),
                    page_id: ids.page_b,
                    parent: None,
                    order: "r".into(),
                    content: "deleted B root".into(),
                },
                SemanticOperation::CreateBlock {
                    block: location(transferred),
                    page_id: ids.page_b,
                    parent: Some(root_b),
                    order: "y".into(),
                    content: "surviving owner transfer".into(),
                },
            ]),
        )
        .unwrap();
    publish_fixture(&writer, &baseline);
    assert!(matches!(
        engine
            .stage_archive_batch(baseline.manifest().batch_id())
            .unwrap()
            .disposition,
        BatchDisposition::Accepted { .. }
    ));

    let mixed = engine
        .prepare_fixture_transaction(
            author(71_101, 71_101),
            &tx(vec![
                // Same-page movement is a root-only reorder and does not need
                // to prime A's full outline.
                SemanticOperation::MoveSubtree {
                    root: location(z),
                    from_page_id: ids.page_a,
                    to_page_id: ids.page_a,
                    parent: None,
                    order: "z-same-page".into(),
                },
                SemanticOperation::CreateBlock {
                    block: location(created),
                    page_id: ids.page_a,
                    parent: None,
                    order: "n-created".into(),
                    content: "created then moved without changing identity".into(),
                },
                SemanticOperation::ReorderBlock {
                    block_id: created,
                    page_id: ids.page_a,
                    parent: None,
                    order: "n-reordered".into(),
                },
                SemanticOperation::MoveSubtree {
                    root: location(created),
                    from_page_id: ids.page_a,
                    to_page_id: ids.page_b,
                    parent: None,
                    order: "n-away".into(),
                },
                SemanticOperation::MoveSubtree {
                    root: location(created),
                    from_page_id: ids.page_b,
                    to_page_id: ids.page_a,
                    parent: None,
                    order: "n-back".into(),
                },
                // B is loaded. Restore retains its old pair claim, so owner
                // transfer must remove this child from B's derived outline.
                SemanticOperation::RestoreSubtree {
                    page_id: ids.page_a,
                    blocks: vec![BlockRestore {
                        block: location(transferred),
                        claim: MembershipClaim {
                            home_document_id: test_block_home(transferred),
                            parent: None,
                            order: "y-restored".into(),
                        },
                    }],
                },
                // This is a true no-op over otherwise untouched documents.
                SemanticOperation::RestoreSubtree {
                    page_id: ids.page_a,
                    blocks: vec![BlockRestore {
                        block: location(unchanged),
                        claim: MembershipClaim {
                            home_document_id: test_block_home(unchanged),
                            parent: None,
                            order: "w".into(),
                        },
                    }],
                },
                // If the retained inactive B pair still populated the loaded
                // outline, deleting root_b would incorrectly delete transfer.
                SemanticOperation::DeleteSubtree {
                    root_block_id: root_b,
                    page_id: ids.page_b,
                },
                SemanticOperation::DeleteSubtree {
                    root_block_id: created,
                    page_id: ids.page_a,
                },
            ]),
        )
        .unwrap();
    let effect = semantic_effect(&mixed);
    assert!(effect
        .blocks()
        .iter()
        .all(|delta| delta.block_id != unchanged));
    assert!(effect
        .memberships()
        .iter()
        .all(|delta| delta.block_id != unchanged));
    let unchanged_documents = std::collections::BTreeSet::from([
        DocumentKey::Entity(test_block_home(unchanged)),
        DocumentKey::Membership {
            block_document_id: test_block_home(unchanged),
            page_document_id: ids.home_a,
        },
    ]);
    assert!(mixed
        .objects()
        .iter()
        .all(|object| !unchanged_documents.contains(&object.document_id())));
    publish_fixture(&writer, &mixed);
    let mixed_outcome = engine
        .stage_archive_batch(mixed.manifest().batch_id())
        .unwrap();
    assert!(
        matches!(mixed_outcome.disposition, BatchDisposition::Accepted { .. }),
        "mixed transaction outcome: {mixed_outcome:?}"
    );
    let page_a = engine.materialize_page(ids.page_a).unwrap();
    assert_eq!(
        page_a
            .blocks
            .iter()
            .map(|block| (
                block.block_id,
                block.home_document_id,
                block.content.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            (unchanged, test_block_home(unchanged), "unchanged restore"),
            (
                transferred,
                test_block_home(transferred),
                "surviving owner transfer",
            ),
            (z, test_block_home(z), "same-page move anchor"),
        ]
    );
    assert!(engine
        .materialize_page(ids.page_b)
        .unwrap()
        .blocks
        .is_empty());
    assert!(matches!(
        engine
            .recover_block_state(test_block_home(created), created)
            .unwrap()
            .map(|state| state.owner),
        Some(BlockOwner::Tombstone)
    ));

    drop(engine);
    let mut replay = ShardedHotEngine::with_clean_archive_store_for_test(
        ObjectStore::open(&archive_path, ids.workspace).unwrap(),
        ids.lineage,
        ids.catalog,
    );
    for batch_id in [baseline.manifest().batch_id(), mixed.manifest().batch_id()] {
        assert!(matches!(
            replay.stage_archive_batch(batch_id).unwrap().disposition,
            BatchDisposition::Accepted { .. }
        ));
    }
    assert_eq!(replay.materialize_page(ids.page_a).unwrap(), page_a);
    assert!(replay
        .materialize_page(ids.page_b)
        .unwrap()
        .blocks
        .is_empty());
}

#[test]
fn same_page_nested_move_writes_only_the_root_pair_and_preserves_concurrent_child_reorder() {
    let ids = Ids::new();
    let child = crate::oplog::BlockId::from_uuid(uuid(72_001));
    let grandchild = crate::oplog::BlockId::from_uuid(uuid(72_002));
    let location = |block_id| BlockLocation {
        block_id,
        home_document_id: test_block_home(block_id),
    };
    let dir = TestDir::new("same-page-nested-move");
    let archive = store(&dir, ids);
    let mut seed = ids.engine();
    let baseline = seed
        .prepare_fixture_transaction(
            author(72_100, 72_100),
            &tx(vec![
                SemanticOperation::CreatePage {
                    page_id: ids.page_a,
                    home_document_id: ids.home_a,
                    name: crate::oplog::LogicalPageName::parse("Nested Move").unwrap(),
                    path: path("pages/nested-move.md"),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreateBlock {
                    block: location(ids.block_a),
                    page_id: ids.page_a,
                    parent: None,
                    order: "root".into(),
                    content: "root".into(),
                },
                SemanticOperation::CreateBlock {
                    block: location(child),
                    page_id: ids.page_a,
                    parent: Some(ids.block_a),
                    order: "child".into(),
                    content: "child".into(),
                },
                SemanticOperation::CreateBlock {
                    block: location(grandchild),
                    page_id: ids.page_a,
                    parent: Some(child),
                    order: "grandchild".into(),
                    content: "grandchild".into(),
                },
            ]),
        )
        .unwrap();
    let baseline = ready(&archive, &baseline);
    assert!(matches!(
        seed.stage_ready(baseline.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));

    let wrong_home = seed.prepare_fixture_transaction(
        author(72_101, 72_101),
        &tx(vec![SemanticOperation::MoveSubtree {
            root: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(child),
            },
            from_page_id: ids.page_a,
            to_page_id: ids.page_a,
            parent: None,
            order: "wrong-home".into(),
        }]),
    );
    assert!(matches!(
        wrong_home,
        Err(EngineError::HomeShardMismatch(block_id)) if block_id == ids.block_a
    ));

    let same_page_move = seed
        .prepare_fixture_transaction(
            author(72_102, 90_000),
            &tx(vec![SemanticOperation::MoveSubtree {
                root: location(ids.block_a),
                from_page_id: ids.page_a,
                to_page_id: ids.page_a,
                parent: None,
                order: "root-moved".into(),
            }]),
        )
        .unwrap();
    let changed_documents = same_page_move
        .objects()
        .iter()
        .filter(|object| object.kind() == ObjectKind::CrdtUpdate)
        .map(OperationObject::document_id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        changed_documents,
        std::collections::BTreeSet::from([DocumentKey::Membership {
            block_document_id: test_block_home(ids.block_a),
            page_document_id: ids.home_a,
        }])
    );
    let move_effect = semantic_effect(&same_page_move);
    assert!(move_effect.blocks().is_empty());
    assert!(matches!(
        move_effect.memberships(),
        [delta] if delta.block_id == ids.block_a && delta.page_id == ids.page_a
    ));

    let mut reorder_author = ids.engine();
    assert!(matches!(
        reorder_author.stage_ready(baseline.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let child_reorder = reorder_author
        .prepare_fixture_transaction(
            author(72_103, 80_000),
            &tx(vec![SemanticOperation::ReorderBlock {
                block_id: child,
                page_id: ids.page_a,
                parent: Some(ids.block_a),
                order: "child-concurrent".into(),
            }]),
        )
        .unwrap();
    let same_page_move = ready(&archive, &same_page_move);
    let child_reorder = ready(&archive, &child_reorder);
    let apply = |first: ValidatedBatch, second: ValidatedBatch| {
        let engine = apply_pair(ids, &baseline, first, second);
        let page = engine.materialize_page(ids.page_a).unwrap();
        assert_eq!(
            page.blocks
                .iter()
                .find(|block| block.block_id == child)
                .unwrap()
                .order,
            "child-concurrent"
        );
        assert_eq!(
            page.blocks
                .iter()
                .find(|block| block.block_id == grandchild)
                .unwrap()
                .order,
            "grandchild"
        );
        engine.canonical_snapshot().unwrap()
    };
    assert_eq!(
        apply(same_page_move.clone(), child_reorder.clone()),
        apply(child_reorder, same_page_move)
    );
}

#[test]
fn move_out_racing_source_page_deletion_converges_in_both_delivery_orders() {
    let ids = Ids::new();
    let dir = TestDir::new("move-out-source-page-delete");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let (moved, deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(72_200, 72_200),
        tx(vec![SemanticOperation::MoveSubtree {
            root: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(ids.block_a),
            },
            from_page_id: ids.page_a,
            to_page_id: ids.page_b,
            parent: None,
            order: "moved-out".into(),
        }]),
        author(72_201, 72_201),
        tx(vec![SemanticOperation::DeletePage {
            page_id: ids.page_a,
        }]),
    );
    let apply = |first: ValidatedBatch, second: ValidatedBatch| {
        let engine = apply_pair(ids, &baseline, first, second);
        assert!(matches!(
            engine.materialize_page(ids.page_a),
            Err(EngineError::PageDeleted(page_id)) if page_id == ids.page_a
        ));
        let destination = engine.materialize_page(ids.page_b).unwrap();
        assert!(destination.blocks.iter().any(|block| {
            block.block_id == ids.block_a
                && block.home_document_id == test_block_home(ids.block_a)
                && block.content == "home A content"
        }));
        engine.canonical_snapshot().unwrap()
    };
    assert_eq!(apply(moved.clone(), deleted.clone()), apply(deleted, moved));
}

fn seed_engine(ids: Ids, store: &ObjectStore) -> (ShardedHotEngine, ValidatedBatch) {
    let mut engine = ids.engine();
    let prepared = genesis(ids, &engine);
    let batch = ready(store, &prepared);
    assert!(matches!(
        engine.stage_ready(batch.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    (engine, batch)
}

#[test]
fn pre_p1b1_operation_schema_is_rejected_at_the_manifest_fence() {
    let ids = Ids::new();
    let dir = TestDir::new("old-operation-schema-fence");
    let archive = store(&dir, ids);
    let prepared = genesis(ids, &ids.engine());
    let semantic = prepared
        .objects()
        .iter()
        .find(|object| object.kind() == ObjectKind::SemanticEffect)
        .unwrap();
    SemanticEffect::decode(semantic.payload()).expect("control payload uses the current schema");

    let mut manifest: serde_json::Value =
        serde_json::from_slice(&prepared.manifest().encode().unwrap()).unwrap();
    manifest["operation_schema_version"] = serde_json::json!(OPERATION_SCHEMA_VERSION - 1);
    let old_schema_bytes = serde_json::to_vec(&manifest).unwrap();
    assert!(matches!(
        archive.stage_manifest_bytes(&old_schema_bytes),
        Err(StoreError::Batch(BatchError::UnknownVersion {
            field: "operation_schema_version",
            expected: OPERATION_SCHEMA_VERSION,
            found,
        })) if found == OPERATION_SCHEMA_VERSION - 1
    ));
    manifest["operation_schema_version"] = serde_json::json!(OPERATION_SCHEMA_VERSION);
    manifest["managed_entity_set_version"] = serde_json::json!(MANAGED_ENTITY_SET_VERSION - 1);
    let old_entity_set_bytes = serde_json::to_vec(&manifest).unwrap();
    assert!(matches!(
        archive.stage_manifest_bytes(&old_entity_set_bytes),
        Err(StoreError::Batch(BatchError::UnknownVersion {
            field: "managed_entity_set_version",
            expected: MANAGED_ENTITY_SET_VERSION,
            found,
        })) if found == MANAGED_ENTITY_SET_VERSION - 1
    ));
    assert!(matches!(
        archive
            .inspect_batch(prepared.manifest().batch_id())
            .unwrap(),
        BatchInspection::Absent
    ));
}

#[test]
fn page_kind_is_durable_across_create_rename_mutation_delete_and_replay() {
    let ids = Ids::new();
    let dir = TestDir::new("page-kind-lifecycle");
    let archive = store(&dir, ids);
    let mut engine = ids.engine();
    let create_operation = SemanticOperation::CreatePage {
        page_id: ids.page_a,
        home_document_id: ids.home_a,
        name: crate::oplog::LogicalPageName::parse("A").unwrap(),
        path: path("shared/A.md"),
        kind: ManagedTextKind::Journal,
    };
    let page_operation = SemanticOperation::CreatePage {
        page_id: ids.page_a,
        home_document_id: ids.home_a,
        name: crate::oplog::LogicalPageName::parse("A").unwrap(),
        path: path("shared/A.md"),
        kind: ManagedTextKind::Page,
    };
    assert_ne!(
        postcard::to_allocvec(&create_operation).unwrap(),
        postcard::to_allocvec(&page_operation).unwrap()
    );
    let page_prepared = ids
        .engine()
        .prepare_fixture_transaction(author(40_000, 40_000), &tx(vec![page_operation.clone()]))
        .unwrap();

    let create = engine
        .prepare_fixture_transaction(author(40_000, 40_000), &tx(vec![create_operation]))
        .unwrap();
    assert_ne!(
        semantic_effect(&create).encode().unwrap(),
        semantic_effect(&page_prepared).encode().unwrap()
    );
    assert_ne!(
        create.manifest().semantic_effect_digest(),
        page_prepared.manifest().semantic_effect_digest()
    );
    assert_ne!(
        create
            .objects()
            .iter()
            .map(OperationObject::descriptor)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        page_prepared
            .objects()
            .iter()
            .map(OperationObject::descriptor)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    );
    let created_effect = semantic_effect(&create);
    assert_eq!(
        created_effect.pages()[0].after.as_ref().unwrap().kind(),
        ManagedTextKind::Journal
    );
    assert!(matches!(
        engine.stage_ready(ready(&archive, &create)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let page_archive = ObjectStore::open(&dir.path().join("page-store"), ids.workspace).unwrap();
    let mut page_engine = ids.engine();
    assert!(matches!(
        page_engine
            .stage_ready(ready(&page_archive, &page_prepared))
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_ne!(
        engine.canonical_snapshot().unwrap(),
        page_engine.canonical_snapshot().unwrap()
    );

    assert!(matches!(
        engine.prepare_fixture_transaction(
            author(40_001, 40_001),
            &tx(vec![SemanticOperation::SetPageKind {
                page_id: ids.page_a,
                kind: ManagedTextKind::Journal,
            }]),
        ),
        Err(EngineError::InvalidTransaction(_))
    ));

    let rename = engine
        .prepare_fixture_transaction(
            author(40_002, 40_002),
            &tx(vec![SemanticOperation::EditPagePath {
                page_id: ids.page_a,
                path: path("elsewhere/A.md"),
            }]),
        )
        .unwrap();
    let renamed_effect = semantic_effect(&rename);
    assert_eq!(
        renamed_effect.pages()[0].before.as_ref().unwrap().kind(),
        ManagedTextKind::Journal
    );
    assert_eq!(
        renamed_effect.pages()[0].after.as_ref().unwrap().kind(),
        ManagedTextKind::Journal
    );
    assert_eq!(
        renamed_effect.pages()[0].before.as_ref().unwrap().name(),
        renamed_effect.pages()[0].after.as_ref().unwrap().name()
    );
    assert!(matches!(
        engine.stage_ready(ready(&archive, &rename)).disposition,
        BatchDisposition::Accepted { .. }
    ));

    let change_kind = engine
        .prepare_fixture_transaction(
            author(40_003, 40_003),
            &tx(vec![SemanticOperation::SetPageKind {
                page_id: ids.page_a,
                kind: ManagedTextKind::Page,
            }]),
        )
        .unwrap();
    let kind_effect = semantic_effect(&change_kind);
    assert_eq!(kind_effect.pages().len(), 1);
    assert_eq!(
        kind_effect.pages()[0].before.as_ref().unwrap().kind(),
        ManagedTextKind::Journal
    );
    assert_eq!(
        kind_effect.pages()[0].after.as_ref().unwrap().kind(),
        ManagedTextKind::Page
    );
    assert_eq!(
        kind_effect.pages()[0].before.as_ref().unwrap().path(),
        kind_effect.pages()[0].after.as_ref().unwrap().path()
    );
    assert_eq!(
        kind_effect.pages()[0].before.as_ref().unwrap().name(),
        kind_effect.pages()[0].after.as_ref().unwrap().name()
    );
    assert!(matches!(
        engine
            .stage_ready(ready(&archive, &change_kind))
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(
        engine.canonical_snapshot().unwrap().pages[0].1.kind(),
        ManagedTextKind::Page
    );

    let delete = engine
        .prepare_fixture_transaction(
            author(40_004, 40_004),
            &tx(vec![SemanticOperation::DeletePage {
                page_id: ids.page_a,
            }]),
        )
        .unwrap();
    let delete_effect = semantic_effect(&delete);
    assert_eq!(
        delete_effect.pages()[0].before.as_ref().unwrap().kind(),
        ManagedTextKind::Page
    );
    assert_eq!(
        delete_effect.pages()[0].before.as_ref().unwrap().name(),
        delete_effect.pages()[0].after.as_ref().unwrap().name()
    );
    assert_eq!(
        delete_effect.pages()[0].after,
        Some(PageState::Tombstone {
            name: crate::oplog::LogicalPageName::parse("A").unwrap(),
            home_document_id: ids.home_a,
            kind: ManagedTextKind::Page,
        })
    );
    assert!(matches!(
        engine.stage_ready(ready(&archive, &delete)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        engine.prepare_fixture_transaction(
            author(40_005, 40_005),
            &tx(vec![SemanticOperation::SetPageKind {
                page_id: ids.page_a,
                kind: ManagedTextKind::Journal,
            }]),
        ),
        Err(EngineError::PageDeleted(page_id)) if page_id == ids.page_a
    ));
    assert!(matches!(
        engine.prepare_fixture_transaction(
            author(40_008, 40_008),
            &tx(vec![SemanticOperation::CreatePage {
                page_id: ids.page_a,
                home_document_id: ids.home_a,
                name: crate::oplog::LogicalPageName::parse("A").unwrap(),
                path: path("pages/A.md"),
                kind: ManagedTextKind::Page,
            }]),
        ),
        Err(EngineError::PageAlreadyExists(page_id)) if page_id == ids.page_a
    ));
    assert!(matches!(
        engine.prepare_fixture_transaction(
            author(40_006, 40_006),
            &tx(vec![SemanticOperation::SetPageKind {
                page_id: ids.page_b,
                kind: ManagedTextKind::Journal,
            }]),
        ),
        Err(EngineError::PageNotFound(page_id)) if page_id == ids.page_b
    ));

    let mut replay = ids.engine();
    for manifest in archive.committed_manifests().unwrap() {
        assert!(matches!(
            replay
                .stage_from_store(&archive, manifest.batch_id())
                .unwrap()
                .disposition,
            BatchDisposition::Accepted { .. }
        ));
    }
    assert!(matches!(
        replay.prepare_fixture_transaction(
            author(40_007, 40_007),
            &tx(vec![SemanticOperation::SetPageKind {
                page_id: ids.page_a,
                kind: ManagedTextKind::Journal,
            }]),
        ),
        Err(EngineError::PageDeleted(page_id)) if page_id == ids.page_a
    ));
}

#[test]
fn revive_page_authors_catalog_first_and_replays_the_same_page_identity() {
    let ids = Ids::new();
    let dir = TestDir::new("revive-page-replay");
    let archive = store(&dir, ids);
    let (mut engine, baseline) = seed_engine(ids, &archive);
    let predecessor = engine.materialize_page_for_projection(ids.page_a).unwrap();
    let expected = predecessor.page.clone();
    let deletion = engine
        .prepare_fixture_transaction(
            author(40_100, 40_100),
            &tx(vec![SemanticOperation::DeletePage {
                page_id: ids.page_a,
            }]),
        )
        .unwrap();
    let deletion_effect = semantic_effect(&deletion);
    assert!(deletion_effect.blocks().is_empty());
    assert!(deletion_effect.memberships().is_empty());
    assert_eq!(
        deletion
            .objects()
            .iter()
            .filter(|object| object.kind() == ObjectKind::CrdtUpdate)
            .map(OperationObject::document_id)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([DocumentKey::Entity(ids.home_a)])
    );
    let deletion = ready(&archive, &deletion);
    assert!(matches!(
        engine.stage_ready(deletion.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let drift = engine
        .prepare_fixture_transaction(
            author(40_102, 40_102),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "tombstoned shard drift before revival".into(),
            }]),
        )
        .unwrap();
    let drift = ready(&archive, &drift);
    assert!(matches!(
        engine.stage_ready(drift.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        engine.materialize_page(ids.page_a),
        Err(EngineError::PageDeleted(page_id)) if page_id == ids.page_a
    ));

    let operations = engine
        .plan_revive_page_operations(ids.page_a, &predecessor.frontier, None)
        .unwrap();
    assert!(
        operations.len() > 1,
        "the flip-first gate needs content work after the catalog operation"
    );
    assert!(matches!(
        operations.first(),
        Some(SemanticOperation::RevivePage { page_id, .. }) if *page_id == ids.page_a
    ));
    let revived = engine
        .prepare_fixture_transaction(author(40_101, 40_101), &tx(operations))
        .unwrap();
    assert_eq!(
        semantic_effect(&revived).pages()[0].lifecycle,
        crate::oplog::PageDeltaLifecycle::RevivePage
    );
    let revived = ready(&archive, &revived);
    let revived_outcome = engine.stage_ready(revived.clone());
    assert!(
        matches!(
            revived_outcome.disposition,
            BatchDisposition::Accepted { .. }
        ),
        "revive outcome: {revived_outcome:?}"
    );
    assert_eq!(engine.materialize_page(ids.page_a).unwrap(), expected);

    let mut peer = ids.engine();
    for batch in [baseline, deletion, drift, revived] {
        assert!(matches!(
            peer.stage_ready(batch).disposition,
            BatchDisposition::Accepted { .. }
        ));
    }
    assert_eq!(peer.materialize_page(ids.page_a).unwrap(), expected);
}

#[test]
fn revive_page_concurrent_remote_edit_uses_ordinary_crdt_merge() {
    let ids = Ids::new();
    let dir = TestDir::new("revive-page-concurrent-edit");
    let archive = store(&dir, ids);
    let (mut prefix, baseline) = seed_engine(ids, &archive);
    let predecessor = prefix
        .materialize_page_for_projection(ids.page_a)
        .unwrap()
        .frontier;
    let deletion = prefix
        .prepare_fixture_transaction(
            author(40_200, 40_200),
            &tx(vec![SemanticOperation::DeletePage {
                page_id: ids.page_a,
            }]),
        )
        .unwrap();
    let deletion = ready(&archive, &deletion);
    assert!(matches!(
        prefix.stage_ready(deletion.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));

    let revival_ops = prefix
        .plan_revive_page_operations(ids.page_a, &predecessor, None)
        .unwrap();
    let revival = prefix
        .prepare_fixture_transaction(author(40_201, 40_201), &tx(revival_ops))
        .unwrap();
    let revival = ready(&archive, &revival);

    let mut remote_author = ids.engine();
    for batch in [baseline.clone(), deletion.clone()] {
        assert!(matches!(
            remote_author.stage_ready(batch).disposition,
            BatchDisposition::Accepted { .. }
        ));
    }
    let remote_edit = remote_author
        .prepare_fixture_transaction(
            author(40_202, 40_202),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "concurrent remote edit during revival".into(),
            }]),
        )
        .unwrap();
    let remote_edit = ready(&archive, &remote_edit);

    let converge = |first: ValidatedBatch, second: ValidatedBatch| {
        let mut peer = ids.engine();
        for batch in [baseline.clone(), deletion.clone(), first, second] {
            let outcome = peer.stage_ready(batch);
            assert!(
                matches!(outcome.disposition, BatchDisposition::Accepted { .. }),
                "concurrent revive outcome: {outcome:?}"
            );
        }
        peer.canonical_snapshot().unwrap()
    };
    let revival_then_edit = converge(revival.clone(), remote_edit.clone());
    let edit_then_revival = converge(remote_edit, revival);
    assert_eq!(revival_then_edit, edit_then_revival);
    assert!(matches!(
        revival_then_edit
            .pages
            .iter()
            .find(|(page_id, _)| *page_id == ids.page_a)
            .map(|(_, state)| state),
        Some(PageState::Live { .. })
    ));
    assert!(revival_then_edit
        .blocks
        .iter()
        .any(|block| block.block_id == ids.block_a
            && block.content == "concurrent remote edit during revival"));
}

#[test]
fn block_birth_uses_its_causal_page_when_a_concurrent_delete_arrives_first() {
    let ids = Ids::new();
    let block_id = crate::oplog::BlockId::from_uuid(uuid(40_250));
    let block_home = test_block_home(block_id);
    let dir = TestDir::new("birth-concurrent-page-delete");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let (created, deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(40_251, 40_251),
        tx(vec![SemanticOperation::CreateBlock {
            block: BlockLocation {
                block_id,
                home_document_id: block_home,
            },
            page_id: ids.page_a,
            parent: None,
            order: "concurrent-birth".into(),
            content: "created at the live causal base".into(),
        }]),
        author(40_252, 40_252),
        tx(vec![SemanticOperation::DeletePage {
            page_id: ids.page_a,
        }]),
    );

    let apply = |first: ValidatedBatch, second: ValidatedBatch| {
        let mut engine = ids.engine();
        assert!(matches!(
            engine.stage_ready(baseline.clone()).disposition,
            BatchDisposition::Accepted { .. }
        ));
        for batch in [first, second] {
            let outcome = engine.stage_ready(batch);
            assert!(
                matches!(outcome.disposition, BatchDisposition::Accepted { .. }),
                "a legitimate concurrent birth/delete branch was refused: {outcome:?}"
            );
        }
        assert_eq!(
            engine
                .recover_block_state(block_home, block_id)
                .unwrap()
                .unwrap()
                .content,
            "created at the live causal base"
        );
        engine.canonical_snapshot().unwrap()
    };
    assert_eq!(
        apply(created.clone(), deleted.clone()),
        apply(deleted, created)
    );
}

#[test]
fn same_batch_birth_and_deletion_retains_provenance_for_restore_and_replay() {
    let ids = Ids::new();
    let dir = TestDir::new("same-batch-birth-delete-restore");
    let archive = store(&dir, ids);
    let (mut engine, baseline) = seed_engine(ids, &archive);
    let block_id = crate::oplog::BlockId::from_uuid(uuid(40_300));
    let block_home = test_block_home(block_id);
    let retired = engine
        .prepare_fixture_transaction(
            author(40_301, 40_301),
            &tx(vec![
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id,
                        home_document_id: block_home,
                    },
                    page_id: ids.page_a,
                    parent: None,
                    order: "born-retired".into(),
                    content: "birth survives retirement".into(),
                },
                SemanticOperation::DeleteSubtree {
                    root_block_id: block_id,
                    page_id: ids.page_a,
                },
            ]),
        )
        .unwrap();
    let retired_effect = semantic_effect(&retired);
    let retired_block = retired_effect
        .blocks()
        .iter()
        .find(|delta| delta.block_id == block_id)
        .unwrap();
    assert_eq!(
        retired_block
            .birth
            .as_ref()
            .map(|birth| (birth.page_id, birth.page_document_id)),
        Some((ids.page_a, ids.home_a))
    );
    assert!(matches!(
        retired_block.after.as_ref().map(|state| state.owner),
        Some(crate::oplog::BlockOwner::Tombstone)
    ));
    let retired = ready(&archive, &retired);
    let retired_outcome = engine.stage_ready(retired.clone());
    assert!(
        matches!(
            retired_outcome.disposition,
            BatchDisposition::Accepted { .. }
        ),
        "same-batch create/delete was refused: {retired_outcome:?}"
    );

    let restored = engine
        .prepare_fixture_transaction(
            author(40_302, 40_302),
            &tx(vec![SemanticOperation::RestoreSubtree {
                page_id: ids.page_a,
                blocks: vec![BlockRestore {
                    block: BlockLocation {
                        block_id,
                        home_document_id: block_home,
                    },
                    claim: MembershipClaim {
                        home_document_id: block_home,
                        parent: None,
                        order: "born-retired".into(),
                    },
                }],
            }]),
        )
        .unwrap();
    assert!(restored
        .manifest()
        .dependency_frontier()
        .documents()
        .iter()
        .any(|document| document.document_id() == DocumentKey::Entity(block_home)));
    let restored = ready(&archive, &restored);
    let restored_outcome = engine.stage_ready(restored.clone());
    assert!(
        matches!(
            restored_outcome.disposition,
            BatchDisposition::Accepted { .. }
        ),
        "restore of same-batch-retired birth was refused: {restored_outcome:?}"
    );
    assert_eq!(
        engine
            .materialize_page(ids.page_a)
            .unwrap()
            .blocks
            .iter()
            .find(|block| block.block_id == block_id)
            .unwrap()
            .content,
        "birth survives retirement"
    );

    let mut replay = ids.engine();
    for batch in [baseline, retired, restored] {
        let outcome = replay.stage_ready(batch);
        assert!(
            matches!(outcome.disposition, BatchDisposition::Accepted { .. }),
            "birth/retirement replay refused: {outcome:?}"
        );
    }
    assert_eq!(
        replay
            .materialize_page(ids.page_a)
            .unwrap()
            .blocks
            .iter()
            .find(|block| block.block_id == block_id)
            .unwrap()
            .content,
        "birth survives retirement"
    );
}

#[test]
fn same_batch_page_and_block_create_delete_is_valid_and_replayable() {
    let ids = Ids::new();
    let page_id = crate::oplog::PageId::from_uuid(uuid(40_350));
    let page_home = crate::oplog::DocumentId::from_uuid(uuid(40_351));
    let block_id = crate::oplog::BlockId::from_uuid(uuid(40_352));
    let block_home = test_block_home(block_id);
    let dir = TestDir::new("same-batch-page-block-create-delete");
    let archive = store(&dir, ids);
    let mut engine = ids.engine();
    let prepared = engine
        .prepare_fixture_transaction(
            author(40_353, 40_353),
            &tx(vec![
                SemanticOperation::CreatePage {
                    page_id,
                    home_document_id: page_home,
                    name: crate::oplog::LogicalPageName::parse("Ephemeral Birth").unwrap(),
                    path: path("pages/ephemeral-birth.md"),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id,
                        home_document_id: block_home,
                    },
                    page_id,
                    parent: None,
                    order: "a".into(),
                    content: "retained tombstone content".into(),
                },
                SemanticOperation::DeleteSubtree {
                    root_block_id: block_id,
                    page_id,
                },
                SemanticOperation::DeletePage { page_id },
            ]),
        )
        .unwrap();
    let effect = semantic_effect(&prepared);
    assert!(matches!(
        effect.pages(),
        [delta] if delta.page_id == page_id
            && delta.before.is_none()
            && matches!(delta.after, Some(crate::oplog::PageState::Tombstone { .. }))
    ));
    assert!(matches!(
        effect.blocks(),
        [delta] if delta.block_id == block_id
            && delta.birth.as_ref().is_some_and(|birth| birth.page_id == page_id && birth.page_document_id == page_home)
            && matches!(delta.after.as_ref().map(|state| state.owner), Some(crate::oplog::BlockOwner::Tombstone))
    ));
    let accepted = ready(&archive, &prepared);
    let outcome = engine.stage_ready(accepted.clone());
    assert!(
        matches!(outcome.disposition, BatchDisposition::Accepted { .. }),
        "same-batch page/block create/delete was refused: {outcome:?}"
    );
    assert!(matches!(
        engine.materialize_page(page_id),
        Err(EngineError::PageDeleted(found)) if found == page_id
    ));
    assert!(matches!(
        engine
            .recover_block_state(block_home, block_id)
            .unwrap()
            .map(|state| state.owner),
        Some(crate::oplog::BlockOwner::Tombstone)
    ));

    let mut replay = ids.engine();
    let replay_outcome = replay.stage_ready(accepted);
    assert!(matches!(
        replay_outcome.disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        replay.materialize_page(page_id),
        Err(EngineError::PageDeleted(found)) if found == page_id
    ));
}

#[test]
fn move_into_a_deleted_page_is_indexed_and_removed_by_exact_revival() {
    let ids = Ids::new();
    let dir = TestDir::new("deleted-page-owner-change-revive");
    let archive = store(&dir, ids);
    let (mut engine, _) = seed_engine(ids, &archive);
    let predecessor = engine
        .materialize_page_for_projection(ids.page_b)
        .unwrap()
        .frontier;
    let deletion = engine
        .prepare_fixture_transaction(
            author(40_400, 40_400),
            &tx(vec![SemanticOperation::DeletePage {
                page_id: ids.page_b,
            }]),
        )
        .unwrap();
    assert!(matches!(
        engine.stage_ready(ready(&archive, &deletion)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let moved = engine
        .prepare_fixture_transaction(
            author(40_401, 40_401),
            &tx(vec![SemanticOperation::MoveSubtree {
                root: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                from_page_id: ids.page_a,
                to_page_id: ids.page_b,
                parent: None,
                order: "moved-while-deleted".into(),
            }]),
        )
        .unwrap();
    let moved_outcome = engine.stage_ready(ready(&archive, &moved));
    assert!(
        matches!(moved_outcome.disposition, BatchDisposition::Accepted { .. }),
        "move into deleted page was refused: {moved_outcome:?}"
    );
    assert!(matches!(
        engine.materialize_page(ids.page_b),
        Err(EngineError::PageDeleted(found)) if found == ids.page_b
    ));
    assert!(matches!(
        engine
            .recover_block_state(test_block_home(ids.block_a), ids.block_a)
            .unwrap()
            .map(|state| state.owner),
        Some(crate::oplog::BlockOwner::Page(found)) if found == ids.page_b
    ));

    let operations = engine
        .plan_revive_page_operations(ids.page_b, &predecessor, None)
        .unwrap();
    assert!(operations.iter().any(|operation| matches!(
        operation,
        SemanticOperation::DeleteSubtree { root_block_id, page_id }
            if *root_block_id == ids.block_a && *page_id == ids.page_b
    )));
    let revival = engine
        .prepare_fixture_transaction(author(40_402, 40_402), &tx(operations))
        .unwrap();
    let revival_outcome = engine.stage_ready(ready(&archive, &revival));
    assert!(
        matches!(
            revival_outcome.disposition,
            BatchDisposition::Accepted { .. }
        ),
        "exact revival after a deleted-page owner change was refused: {revival_outcome:?}"
    );
    assert!(engine
        .materialize_page(ids.page_b)
        .unwrap()
        .blocks
        .is_empty());
    assert!(matches!(
        engine
            .recover_block_state(test_block_home(ids.block_a), ids.block_a)
            .unwrap()
            .map(|state| state.owner),
        Some(crate::oplog::BlockOwner::Tombstone)
    ));
}

#[test]
fn old_pair_reorder_racing_a_move_converges_without_resurrecting_the_old_pair() {
    let ids = Ids::new();
    let dir = TestDir::new("old-pair-reorder-vs-move");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let (reordered, moved) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(40_450, 40_450),
        tx(vec![SemanticOperation::ReorderBlock {
            block_id: ids.block_a,
            page_id: ids.page_a,
            parent: None,
            order: "old-pair-reorder".into(),
        }]),
        author(40_451, 40_451),
        tx(vec![SemanticOperation::MoveSubtree {
            root: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(ids.block_a),
            },
            from_page_id: ids.page_a,
            to_page_id: ids.page_b,
            parent: None,
            order: "destination-move".into(),
        }]),
    );

    let apply = |first: ValidatedBatch, second: ValidatedBatch| {
        let mut engine = ids.engine();
        assert!(matches!(
            engine.stage_ready(baseline.clone()).disposition,
            BatchDisposition::Accepted { .. }
        ));
        for batch in [first, second] {
            let outcome = engine.stage_ready(batch);
            assert!(
                matches!(outcome.disposition, BatchDisposition::Accepted { .. }),
                "reorder/move branch was refused: {outcome:?}"
            );
        }
        assert!(engine
            .materialize_page(ids.page_a)
            .unwrap()
            .blocks
            .is_empty());
        let destination = engine.materialize_page(ids.page_b).unwrap();
        assert_eq!(destination.blocks.len(), 1);
        assert_eq!(destination.blocks[0].block_id, ids.block_a);
        engine.canonical_snapshot().unwrap()
    };
    assert_eq!(
        apply(reordered.clone(), moved.clone()),
        apply(moved, reordered)
    );
}

#[test]
fn page_kind_mismatch_between_effect_and_catalog_object_is_rejected() {
    let ids = Ids::new();
    let dir = TestDir::new("page-kind-effect-object-mismatch");
    let archive = store(&dir, ids);
    let author_engine = ids.engine();
    let prepared = author_engine
        .prepare_fixture_transaction(
            author(41_000, 41_000),
            &tx(vec![SemanticOperation::CreatePage {
                page_id: ids.page_a,
                home_document_id: ids.home_a,
                name: crate::oplog::LogicalPageName::parse("A").unwrap(),
                path: path("shared/A.md"),
                kind: ManagedTextKind::Page,
            }]),
        )
        .unwrap();
    let declared = semantic_effect(&prepared);
    let mismatched = SemanticEffect::new_with_page_preambles(
        declared
            .pages()
            .iter()
            .map(|delta| PageDelta {
                page_id: delta.page_id,
                before: delta.before.clone(),
                after: delta.after.as_ref().map(|state| match state {
                    PageState::Live {
                        path,
                        home_document_id,
                        ..
                    } => PageState::Live {
                        name: crate::oplog::LogicalPageName::parse("A").unwrap(),
                        path: path.clone(),
                        home_document_id: *home_document_id,
                        kind: ManagedTextKind::Journal,
                    },
                    PageState::Tombstone {
                        home_document_id, ..
                    } => PageState::Tombstone {
                        name: crate::oplog::LogicalPageName::parse("A").unwrap(),
                        home_document_id: *home_document_id,
                        kind: ManagedTextKind::Journal,
                    },
                }),
                lifecycle: delta.lifecycle,
            })
            .collect(),
        declared.page_preambles().to_vec(),
        declared.blocks().to_vec(),
        declared.memberships().to_vec(),
    )
    .unwrap();
    let objects = prepared
        .objects()
        .iter()
        .map(|object| {
            if object.kind() == ObjectKind::SemanticEffect {
                OperationObject::new(
                    ids.workspace,
                    object.document_id(),
                    ObjectKind::SemanticEffect,
                    mismatched.encode().unwrap(),
                )
                .unwrap()
            } else {
                object.clone()
            }
        })
        .collect();
    let tampered = rebuild(
        prepared.manifest(),
        objects,
        prepared.manifest().dependency_frontier().clone(),
    );
    let mut receiver = ids.engine();

    assert!(matches!(
        receiver.stage_ready(ready(&archive, &tampered)).disposition,
        BatchDisposition::Rejected {
            error: EngineError::SemanticEffectMismatch,
        }
    ));
}

#[test]
fn page_kind_changing_deletion_matching_catalog_and_effect_is_rejected() {
    let ids = Ids::new();
    let dir = TestDir::new("page-kind-changing-delete");
    let archive = store(&dir, ids);
    let mut author_engine = ids.engine();
    let create = author_engine
        .prepare_fixture_transaction(
            author(42_000, 42_000),
            &tx(vec![SemanticOperation::CreatePage {
                page_id: ids.page_a,
                home_document_id: ids.home_a,
                name: crate::oplog::LogicalPageName::parse("A").unwrap(),
                path: path("shared/A.md"),
                kind: ManagedTextKind::Page,
            }]),
        )
        .unwrap();
    let create_ready = ready(&archive, &create);
    assert!(matches!(
        author_engine.stage_ready(create_ready.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let delete = author_engine
        .prepare_fixture_transaction(
            author(42_001, 42_001),
            &tx(vec![SemanticOperation::DeletePage {
                page_id: ids.page_a,
            }]),
        )
        .unwrap();

    let create_page_payload: TestCrdtUpdatePayload = postcard::from_bytes(
        create
            .objects()
            .iter()
            .find(|object| {
                object.kind() == ObjectKind::CrdtUpdate
                    && object.document_id() == DocumentKey::Entity(ids.home_a)
            })
            .unwrap()
            .payload(),
    )
    .unwrap();
    let page_document = LoroDoc::new();
    assert!(page_document
        .import(&create_page_payload.raw_update)
        .unwrap()
        .pending
        .is_none());
    let page_before = page_document.oplog_vv();
    page_document.set_peer_id(42_001).unwrap();
    page_document
        .get_map("meta")
        .insert(
            "state",
            serde_json::to_string(&PageState::Tombstone {
                name: crate::oplog::LogicalPageName::parse("A").unwrap(),
                home_document_id: ids.home_a,
                kind: ManagedTextKind::Journal,
            })
            .unwrap(),
        )
        .unwrap();
    page_document.commit();
    let forged_page_update = page_document
        .export(ExportMode::updates(&page_before))
        .unwrap();

    let declared = semantic_effect(&delete);
    let mut pages = declared.pages().to_vec();
    let delta = pages
        .iter_mut()
        .find(|delta| delta.page_id == ids.page_a)
        .unwrap();
    assert_eq!(delta.before.as_ref().unwrap().kind(), ManagedTextKind::Page);
    delta.after = Some(PageState::Tombstone {
        name: crate::oplog::LogicalPageName::parse("A").unwrap(),
        home_document_id: ids.home_a,
        kind: ManagedTextKind::Journal,
    });
    let tampered_effect = unchecked_semantic_effect_bytes(&declared, pages);

    let objects = delete
        .objects()
        .iter()
        .map(|object| match object.kind() {
            ObjectKind::SemanticEffect => OperationObject::new(
                ids.workspace,
                object.document_id(),
                ObjectKind::SemanticEffect,
                tampered_effect.clone(),
            )
            .unwrap(),
            ObjectKind::CrdtUpdate if object.document_id() == DocumentKey::Entity(ids.home_a) => {
                let mut payload: TestCrdtUpdatePayload =
                    postcard::from_bytes(object.payload()).unwrap();
                payload.raw_update = forged_page_update.clone();
                OperationObject::new(
                    ids.workspace,
                    object.document_id(),
                    ObjectKind::CrdtUpdate,
                    postcard::to_allocvec(&payload).unwrap(),
                )
                .unwrap()
            }
            _ => object.clone(),
        })
        .collect();
    let tampered = rebuild(
        delete.manifest(),
        objects,
        delete.manifest().dependency_frontier().clone(),
    );
    let mut receiver = ids.engine();
    assert!(matches!(
        receiver.stage_ready(create_ready).disposition,
        BatchDisposition::Accepted { .. }
    ));

    assert!(matches!(
        receiver.stage_ready(ready(&archive, &tampered)).disposition,
        BatchDisposition::Rejected {
            error: EngineError::Semantic(error),
        } if error == "invalid page lifecycle transition: creation must be None -> Live; edits must be Live -> Live; deletion must be Live -> same-kind Tombstone; revival must be explicitly discriminated RevivePage Tombstone -> same-identity Live"
    ));
}

#[test]
fn page_preamble_is_authoritative_across_replay_move_and_rename() {
    let ids = Ids::new();
    let dir = TestDir::new("page-preamble-replay");
    let archive_path = dir.path().join("archive");
    let writer = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let mut engine = ids.engine();
    let genesis = genesis(ids, &engine);
    let mut batch_ids = vec![genesis.manifest().batch_id()];
    assert!(matches!(
        engine.stage_ready(ready(&writer, &genesis)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(engine.materialize_page(ids.page_a).unwrap().preamble, None);

    let preamble = "title:: Stable\nfree text before the outline".to_string();
    let set = engine
        .prepare_fixture_transaction(
            author(39_001, 39_001),
            &tx(vec![SemanticOperation::SetPagePreamble {
                page_id: ids.page_a,
                preamble: Some(preamble.clone()),
            }]),
        )
        .unwrap();
    let effect = SemanticEffect::decode(
        set.objects()
            .iter()
            .find(|object| object.kind() == ObjectKind::SemanticEffect)
            .unwrap()
            .payload(),
    )
    .unwrap();
    assert_eq!(effect.page_preambles().len(), 1);
    assert_eq!(
        effect.page_preambles()[0].before.as_ref().unwrap().preamble,
        None
    );
    assert_eq!(
        effect.page_preambles()[0]
            .after
            .as_ref()
            .unwrap()
            .preamble
            .as_deref(),
        Some(preamble.as_str())
    );
    batch_ids.push(set.manifest().batch_id());
    assert!(matches!(
        engine.stage_ready(ready(&writer, &set)).disposition,
        BatchDisposition::Accepted { .. }
    ));

    let neighbors = engine
        .prepare_fixture_transaction(
            author(39_002, 39_002),
            &tx(vec![
                SemanticOperation::EditPagePath {
                    page_id: ids.page_a,
                    path: path("journals/2026_07_23.md"),
                },
                SemanticOperation::MoveSubtree {
                    root: BlockLocation {
                        block_id: ids.block_a,
                        home_document_id: test_block_home(ids.block_a),
                    },
                    from_page_id: ids.page_a,
                    to_page_id: ids.page_b,
                    parent: None,
                    order: "moved".into(),
                },
            ]),
        )
        .unwrap();
    batch_ids.push(neighbors.manifest().batch_id());
    assert!(matches!(
        engine.stage_ready(ready(&writer, &neighbors)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let page = engine.materialize_page(ids.page_a).unwrap();
    assert_eq!(page.path, path("journals/2026_07_23.md"));
    assert_eq!(page.preamble.as_deref(), Some(preamble.as_str()));
    assert!(page.blocks.is_empty());
    assert_eq!(engine.materialize_page(ids.page_b).unwrap().blocks.len(), 1);

    let reader = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let mut replay =
        ShardedHotEngine::with_clean_archive_store_for_test(reader, ids.lineage, ids.catalog);
    for batch_id in batch_ids {
        assert!(matches!(
            replay.stage_archive_batch(batch_id).unwrap().disposition,
            BatchDisposition::Accepted { .. }
        ));
    }
    let replayed = replay.materialize_page(ids.page_a).unwrap();
    assert_eq!(replayed.path, path("journals/2026_07_23.md"));
    assert_eq!(replayed.preamble.as_deref(), Some(preamble.as_str()));
}

#[test]
fn concurrent_page_preamble_mutations_converge_and_validate_semantically() {
    let ids = Ids::new();
    let dir = TestDir::new("page-preamble-convergence");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let (left, right) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(39_010, 39_010),
        tx(vec![SemanticOperation::SetPagePreamble {
            page_id: ids.page_a,
            preamble: Some("left:: value".into()),
        }]),
        author(39_011, 39_011),
        tx(vec![SemanticOperation::SetPagePreamble {
            page_id: ids.page_a,
            preamble: Some("right free text".into()),
        }]),
    );
    let ab = apply_pair(ids, &baseline, left.clone(), right.clone());
    let ba = apply_pair(ids, &baseline, right, left);
    assert_eq!(
        ab.canonical_snapshot().unwrap(),
        ba.canonical_snapshot().unwrap()
    );
    assert_eq!(
        ab.materialize_page(ids.page_a).unwrap().preamble,
        ba.materialize_page(ids.page_a).unwrap().preamble
    );

    let wrong_home = SemanticEffect::new_with_page_preambles(
        Vec::new(),
        vec![PagePreambleDelta {
            page_id: ids.page_a,
            home_document_id: ids.home_a,
            before: None,
            after: Some(PagePreambleState {
                page_id: ids.page_a,
                home_document_id: ids.home_b,
                preamble: Some("invalid".into()),
            }),
        }],
        Vec::new(),
        Vec::new(),
    );
    assert!(matches!(wrong_home, Err(SemanticError::HomeShardChanged)));
}

#[test]
fn projection_write_authorization_requires_durable_engine_derived_state() {
    let ids = Ids::new();
    let dir = TestDir::new("projection-authorization");
    let archive_path = dir.path().join("archive");
    let writer = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let prepared = genesis(ids, &ids.engine());
    let batch_id = prepared.manifest().batch_id();
    let validated = ready(&writer, &prepared);

    let mut hand_built_engine = ids.engine();
    assert!(matches!(
        hand_built_engine.stage_ready(validated).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        hand_built_engine.authorize_projection_write(ids.page_a),
        Err(EngineError::ProjectionAuthorizationUnavailable)
    ));

    let reader = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let mut durable =
        ShardedHotEngine::with_clean_archive_store_for_test(reader, ids.lineage, ids.catalog);
    assert!(matches!(
        durable.stage_archive_batch(batch_id).unwrap().disposition,
        BatchDisposition::Accepted { .. }
    ));
    let expected_state = durable.materialize_page_for_projection(ids.page_a).unwrap();
    let head_occurrences = expected_state
        .frontier
        .documents()
        .iter()
        .map(|document| document.direct_dependency_heads().len())
        .sum::<usize>();
    let distinct_heads = expected_state
        .frontier
        .documents()
        .iter()
        .flat_map(|document| document.direct_dependency_heads().iter().copied())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(distinct_heads, std::collections::BTreeSet::from([batch_id]));
    assert!(
        head_occurrences > distinct_heads.len(),
        "the fixture must name one shared batch from multiple document frontiers"
    );

    let before = durable.instrumentation().store;
    let authorization = durable.authorize_projection_write(ids.page_a).unwrap();
    let after = durable.instrumentation().store;
    assert_eq!(authorization.state(), &expected_state);
    assert_eq!(
        after.inspected_manifest_operations - before.inspected_manifest_operations,
        distinct_heads.len(),
        "authorization must inspect each distinct direct batch exactly once; \
         {head_occurrences} frontier occurrences named {distinct_heads:?}"
    );
    assert_eq!(
        after.inspected_object_operations - before.inspected_object_operations,
        prepared.manifest().required_objects().len(),
        "the one distinct batch must still validate every required original object"
    );
    assert_eq!(authorization.state().page.page_id, ids.page_a);
    assert!(!authorization.state().frontier.documents().is_empty());
    assert!(authorization
        .state()
        .frontier
        .documents()
        .iter()
        .flat_map(|document| document.direct_dependency_heads())
        .all(|head| *head == batch_id));

    let missing_object = prepared.manifest().required_objects()[0].content_digest();
    std::fs::remove_file(
        archive_path
            .join(super::sync_layout::ARCHIVE_OBJECTS_DIR)
            .join(format!("{missing_object}.object")),
    )
    .unwrap();
    assert!(matches!(
        durable.authorize_projection_write(ids.page_a),
        Err(EngineError::ProjectionFrontierNotDurable(found)) if found == batch_id
    ));
}

#[test]
fn logseq_uuid_assignment_is_explicit_idempotent_replaceable_and_removable() {
    let ids = Ids::new();
    let dir = TestDir::new("logseq-uuid-lifecycle");
    let archive = store(&dir, ids);
    let (mut engine, _) = seed_engine(ids, &archive);
    let block = BlockLocation {
        block_id: ids.block_a,
        home_document_id: test_block_home(ids.block_a),
    };
    let first = LogseqUuid::from_uuid(uuid(40_001));
    let second = LogseqUuid::from_uuid(uuid(40_002));

    let assign = engine
        .prepare_fixture_transaction(
            author(40_010, 40_010),
            &tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
                block,
                mutation: LogseqIdentityMutation::AssignExternal { logseq_uuid: first },
            }]),
        )
        .unwrap();
    let effect = SemanticEffect::decode(
        assign
            .objects()
            .iter()
            .find(|object| object.kind() == ObjectKind::SemanticEffect)
            .unwrap()
            .payload(),
    )
    .unwrap();
    assert_eq!(effect.blocks().len(), 1);
    assert_eq!(
        effect.blocks()[0].before.as_ref().unwrap().logseq_uuid,
        None
    );
    assert_eq!(
        effect.blocks()[0].after.as_ref().unwrap().logseq_uuid,
        Some(first)
    );
    assert!(matches!(
        engine.stage_ready(ready(&archive, &assign)).disposition,
        BatchDisposition::Accepted { no_op: false }
    ));
    assert_eq!(
        engine.materialize_page(ids.page_a).unwrap().blocks[0].logseq_uuid,
        Some(first)
    );
    assert_eq!(
        engine.materialize_page(ids.page_a).unwrap().blocks[0].logseq_identity_origin,
        Some(LogseqIdentityOrigin::ExternalImported)
    );

    let duplicate_assign = engine.prepare_fixture_transaction(
        author(40_011, 40_011),
        &tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
            block,
            mutation: LogseqIdentityMutation::AssignExternal { logseq_uuid: first },
        }]),
    );
    assert!(
        matches!(duplicate_assign, Err(EngineError::InvalidTransaction(_))),
        "assignment and replacement must remain distinct typed actions"
    );

    let replace = engine
        .prepare_fixture_transaction(
            author(40_012, 40_012),
            &tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
                block,
                mutation: LogseqIdentityMutation::ReplaceExternal {
                    logseq_uuid: second,
                },
            }]),
        )
        .unwrap();
    assert!(matches!(
        engine.stage_ready(ready(&archive, &replace)).disposition,
        BatchDisposition::Accepted { no_op: false }
    ));
    assert_eq!(
        engine.materialize_page(ids.page_a).unwrap().blocks[0].logseq_uuid,
        Some(second)
    );

    let remove = engine
        .prepare_fixture_transaction(
            author(40_013, 40_013),
            &tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
                block,
                mutation: LogseqIdentityMutation::RemoveExternal,
            }]),
        )
        .unwrap();
    assert!(matches!(
        engine.stage_ready(ready(&archive, &remove)).disposition,
        BatchDisposition::Accepted { no_op: false }
    ));
    assert_eq!(
        engine.materialize_page(ids.page_a).unwrap().blocks[0].logseq_uuid,
        None
    );

    let content_only = engine
        .prepare_fixture_transaction(
            author(40_014, 40_014),
            &tx(vec![SemanticOperation::EditBlockContent {
                block,
                content: format!("id:: {first}"),
            }]),
        )
        .unwrap();
    assert!(matches!(
        engine
            .stage_ready(ready(&archive, &content_only))
            .disposition,
        BatchDisposition::Accepted { no_op: false }
    ));
    assert_eq!(
        engine.materialize_page(ids.page_a).unwrap().blocks[0].logseq_uuid,
        None,
        "semantic identity must never be inferred from content"
    );
}

#[test]
fn logseq_uuid_concurrent_assignment_converges_and_survives_move_delete() {
    let ids = Ids::new();
    let dir = TestDir::new("logseq-uuid-convergence");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let block = BlockLocation {
        block_id: ids.block_a,
        home_document_id: test_block_home(ids.block_a),
    };
    let left_uuid = LogseqUuid::from_uuid(uuid(41_001));
    let right_uuid = LogseqUuid::from_uuid(uuid(41_002));
    let (left, right) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(41_010, 41_010),
        tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
            block,
            mutation: LogseqIdentityMutation::AssignExternal {
                logseq_uuid: left_uuid,
            },
        }]),
        author(41_011, 41_011),
        tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
            block,
            mutation: LogseqIdentityMutation::AssignExternal {
                logseq_uuid: right_uuid,
            },
        }]),
    );
    let mut ab = apply_pair(ids, &baseline, left.clone(), right.clone());
    let ba = apply_pair(ids, &baseline, right, left);
    assert_eq!(
        ab.canonical_snapshot().unwrap(),
        ba.canonical_snapshot().unwrap()
    );
    let winner = ab.materialize_page(ids.page_a).unwrap().blocks[0]
        .logseq_uuid
        .expect("one concurrent UUID register wins deterministically");
    assert!(winner == left_uuid || winner == right_uuid);

    let moved = ab
        .prepare_fixture_transaction(
            author(41_012, 41_012),
            &tx(vec![SemanticOperation::MoveSubtree {
                root: block,
                from_page_id: ids.page_a,
                to_page_id: ids.page_b,
                parent: None,
                order: "moved-with-logseq-uuid".into(),
            }]),
        )
        .unwrap();
    assert!(matches!(
        ab.stage_ready(ready(&archive, &moved)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(
        ab.materialize_page(ids.page_b).unwrap().blocks[0].logseq_uuid,
        Some(winner)
    );

    let deleted = ab
        .prepare_fixture_transaction(
            author(41_013, 41_013),
            &tx(vec![SemanticOperation::DeleteSubtree {
                root_block_id: ids.block_a,
                page_id: ids.page_b,
            }]),
        )
        .unwrap();
    assert!(matches!(
        ab.stage_ready(ready(&archive, &deleted)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(
        ab.recover_block_state(ids.block_home_a(), ids.block_a)
            .unwrap()
            .unwrap()
            .logseq_uuid,
        Some(winner)
    );
}

#[test]
fn logseq_uuid_restarts_and_replays_from_the_stable_home_shard() {
    let ids = Ids::new();
    let dir = TestDir::new("logseq-uuid-replay");
    let archive_path = dir.path().join("archive");
    let writer = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let mut author_engine = ids.engine();
    let genesis = genesis(ids, &author_engine);
    let genesis_id = genesis.manifest().batch_id();
    assert!(matches!(
        author_engine
            .stage_ready(ready(&writer, &genesis))
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    let assigned_uuid = LogseqUuid::from_uuid(uuid(42_001));
    let assigned = author_engine
        .prepare_fixture_transaction(
            author(42_010, 42_010),
            &tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                mutation: LogseqIdentityMutation::AssignExternal {
                    logseq_uuid: assigned_uuid,
                },
            }]),
        )
        .unwrap();
    let assigned_id = assigned.manifest().batch_id();
    assert!(matches!(
        author_engine
            .stage_ready(ready(&writer, &assigned))
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    drop(author_engine);

    let reader = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let mut replay =
        ShardedHotEngine::with_clean_archive_store_for_test(reader, ids.lineage, ids.catalog);
    for batch_id in [genesis_id, assigned_id] {
        assert!(matches!(
            replay.stage_archive_batch(batch_id).unwrap().disposition,
            BatchDisposition::Accepted { .. }
        ));
    }
    assert_eq!(
        replay.materialize_page(ids.page_a).unwrap().blocks[0].logseq_uuid,
        Some(assigned_uuid)
    );
    assert_eq!(
        replay
            .recover_block_state(ids.block_home_a(), ids.block_a)
            .unwrap()
            .unwrap()
            .logseq_uuid,
        Some(assigned_uuid)
    );
}

#[test]
fn projection_page_frontier_is_exact_and_same_batch_uuid_reference_is_atomic() {
    let ids = Ids::new();
    let dir = TestDir::new("projection-page-frontier");
    let archive = store(&dir, ids);
    let (mut engine, _) = seed_engine(ids, &archive);
    let assigned_uuid = LogseqUuid::from_uuid(uuid(43_001));
    let anchored = engine
        .prepare_fixture_transaction(
            author(43_010, 43_010),
            &tx(vec![
                SemanticOperation::MutateBlockLogseqIdentity {
                    block: BlockLocation {
                        block_id: ids.block_a,
                        home_document_id: test_block_home(ids.block_a),
                    },
                    mutation: LogseqIdentityMutation::Generate {
                        logseq_uuid: assigned_uuid,
                        trigger: LogseqIdentityTrigger::BlockReference {
                            referrer: BlockLocation {
                                block_id: ids.block_c,
                                home_document_id: test_block_home(ids.block_c),
                            },
                        },
                    },
                },
                SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: ids.block_c,
                        home_document_id: test_block_home(ids.block_c),
                    },
                    content: format!("same-batch reference (({assigned_uuid}))"),
                },
            ]),
        )
        .unwrap();
    let anchored_id = anchored.manifest().batch_id();
    let updated_documents: Vec<_> = anchored
        .manifest()
        .required_objects()
        .iter()
        .filter(|object| object.kind() == ObjectKind::CrdtUpdate)
        .map(|object| object.document_id())
        .collect();
    assert_eq!(
        updated_documents,
        vec![
            DocumentKey::Entity(ids.block_home_a()),
            DocumentKey::Entity(ids.block_home_c())
        ]
    );
    assert!(matches!(
        engine.stage_ready(ready(&archive, &anchored)).disposition,
        BatchDisposition::Accepted { no_op: false }
    ));

    let page_a = engine.materialize_page_for_projection(ids.page_a).unwrap();
    assert_eq!(page_a.page.blocks[0].logseq_uuid, Some(assigned_uuid));
    assert_eq!(
        page_a.page.blocks[0].logseq_identity_origin,
        Some(LogseqIdentityOrigin::PolicyGenerated {
            reason: PolicyGeneratedAnchorReason::BlockReference,
        })
    );
    let page_a_documents: Vec<_> = page_a
        .frontier
        .documents()
        .iter()
        .map(DocumentDependencies::document_id)
        .collect();
    assert_eq!(
        page_a_documents,
        vec![
            DocumentKey::Entity(ids.home_a),
            DocumentKey::Entity(ids.block_home_a()),
            DocumentKey::Membership {
                block_document_id: ids.block_home_a(),
                page_document_id: ids.home_a,
            },
        ]
    );
    assert!(page_a
        .frontier
        .documents()
        .iter()
        .find(|document| document.document_id() == DocumentKey::Entity(ids.block_home_a()))
        .unwrap()
        .direct_dependency_heads()
        .contains(&anchored_id));

    let page_c = engine.materialize_page_for_projection(ids.page_c).unwrap();
    assert_eq!(
        page_c.page.blocks[0].content,
        format!("same-batch reference (({assigned_uuid}))")
    );
    let page_c_documents: Vec<_> = page_c
        .frontier
        .documents()
        .iter()
        .map(DocumentDependencies::document_id)
        .collect();
    assert_eq!(
        page_c_documents,
        vec![
            DocumentKey::Entity(ids.home_c),
            DocumentKey::Entity(ids.block_home_a()),
            DocumentKey::Entity(ids.block_home_c()),
            DocumentKey::Membership {
                block_document_id: ids.block_home_c(),
                page_document_id: ids.home_c,
            },
        ]
    );
    assert!(page_c
        .frontier
        .documents()
        .iter()
        .find(|document| document.document_id() == DocumentKey::Entity(ids.block_home_c()))
        .unwrap()
        .direct_dependency_heads()
        .contains(&anchored_id));
    assert!(!page_a_documents.contains(&DocumentKey::Entity(ids.block_home_c())));
    assert!(page_c_documents.contains(&DocumentKey::Entity(ids.block_home_a())));
}

#[test]
fn policy_generated_identity_requires_typed_same_batch_content_or_user_action() {
    let ids = Ids::new();
    let dir = TestDir::new("typed-logseq-triggers");
    let archive = store(&dir, ids);
    let (mut engine, _) = seed_engine(ids, &archive);
    let target = BlockLocation {
        block_id: ids.block_a,
        home_document_id: test_block_home(ids.block_a),
    };
    let referrer = BlockLocation {
        block_id: ids.block_c,
        home_document_id: test_block_home(ids.block_c),
    };
    let embed_uuid = LogseqUuid::from_uuid(uuid(43_100));

    assert!(matches!(
        engine.prepare_fixture_transaction(
            author(43_101, 43_101),
            &tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
                block: target,
                mutation: LogseqIdentityMutation::Generate {
                    logseq_uuid: embed_uuid,
                    trigger: LogseqIdentityTrigger::BlockEmbed { referrer },
                },
            }]),
        ),
        Err(EngineError::MissingLogseqIdentityTrigger { .. })
    ));
    assert!(matches!(
        engine.prepare_fixture_transaction(
            author(43_102, 43_102),
            &tx(vec![
                SemanticOperation::MutateBlockLogseqIdentity {
                    block: target,
                    mutation: LogseqIdentityMutation::Generate {
                        logseq_uuid: embed_uuid,
                        trigger: LogseqIdentityTrigger::BlockEmbed { referrer },
                    },
                },
                SemanticOperation::EditBlockContent {
                    block: referrer,
                    content: format!("{{{{embed (({}))}}}}", LogseqUuid::from_uuid(uuid(43_999))),
                },
            ]),
        ),
        Err(EngineError::MissingLogseqIdentityTrigger { .. })
    ));
    assert!(matches!(
        engine.prepare_fixture_transaction(
            author(43_102_001, 43_102_001),
            &tx(vec![
                SemanticOperation::EditPagePath {
                    page_id: ids.page_c,
                    path: path("pages/C.org"),
                },
                SemanticOperation::MutateBlockLogseqIdentity {
                    block: target,
                    mutation: LogseqIdentityMutation::Generate {
                        logseq_uuid: embed_uuid,
                        trigger: LogseqIdentityTrigger::BlockReference { referrer },
                    },
                },
                SemanticOperation::EditBlockContent {
                    block: referrer,
                    content: format!("#+BEGIN_SRC text\n(({embed_uuid}))\n#+END_SRC"),
                },
            ]),
        ),
        Err(EngineError::MissingLogseqIdentityTrigger { .. })
    ));
    assert!(matches!(
        engine.prepare_fixture_transaction(
            author(43_102_002, 43_102_002),
            &tx(vec![
                SemanticOperation::MutateBlockLogseqIdentity {
                    block: target,
                    mutation: LogseqIdentityMutation::Generate {
                        logseq_uuid: embed_uuid,
                        trigger: LogseqIdentityTrigger::BlockEmbed { referrer },
                    },
                },
                SemanticOperation::EditBlockContent {
                    block: referrer,
                    content: format!("{{{{embed (({embed_uuid}))}}}}"),
                },
                SemanticOperation::EditBlockContent {
                    block: referrer,
                    content: "the final content removed the trigger".into(),
                },
            ]),
        ),
        Err(EngineError::MissingLogseqIdentityTrigger { .. })
    ));

    let preexisting = format!("{{{{embed (({embed_uuid}))}}}}");
    let seed_trigger = engine
        .prepare_fixture_transaction(
            author(43_102_010, 43_102_010),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: referrer,
                content: preexisting.clone(),
            }]),
        )
        .unwrap();
    assert!(matches!(
        engine
            .stage_ready(ready(&archive, &seed_trigger))
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        engine.prepare_fixture_transaction(
            author(43_102_011, 43_102_011),
            &tx(vec![
                SemanticOperation::MutateBlockLogseqIdentity {
                    block: target,
                    mutation: LogseqIdentityMutation::Generate {
                        logseq_uuid: embed_uuid,
                        trigger: LogseqIdentityTrigger::BlockEmbed { referrer },
                    },
                },
                SemanticOperation::EditBlockContent {
                    block: referrer,
                    content: preexisting,
                },
            ]),
        ),
        Err(EngineError::MissingLogseqIdentityTrigger { .. })
    ));
    let clear_trigger = engine
        .prepare_fixture_transaction(
            author(43_102_012, 43_102_012),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: referrer,
                content: "cleared".into(),
            }]),
        )
        .unwrap();
    assert!(matches!(
        engine
            .stage_ready(ready(&archive, &clear_trigger))
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    let org_reference = format!("(({embed_uuid}))");
    let seed_org_trigger = engine
        .prepare_fixture_transaction(
            author(43_102_013, 43_102_013),
            &tx(vec![
                SemanticOperation::EditPagePath {
                    page_id: ids.page_c,
                    path: path("pages/C.org"),
                },
                SemanticOperation::EditBlockContent {
                    block: referrer,
                    content: org_reference.clone(),
                },
            ]),
        )
        .unwrap();
    assert!(matches!(
        engine
            .stage_ready(ready(&archive, &seed_org_trigger))
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        engine.prepare_fixture_transaction(
            author(43_102_014, 43_102_014),
            &tx(vec![
                SemanticOperation::MutateBlockLogseqIdentity {
                    block: target,
                    mutation: LogseqIdentityMutation::Generate {
                        logseq_uuid: embed_uuid,
                        trigger: LogseqIdentityTrigger::BlockReference { referrer },
                    },
                },
                SemanticOperation::EditBlockContent {
                    block: referrer,
                    content: org_reference,
                },
            ]),
        ),
        Err(EngineError::MissingLogseqIdentityTrigger { .. })
    ));
    let restore_markdown = engine
        .prepare_fixture_transaction(
            author(43_102_015, 43_102_015),
            &tx(vec![
                SemanticOperation::EditPagePath {
                    page_id: ids.page_c,
                    path: path("pages/C.md"),
                },
                SemanticOperation::EditBlockContent {
                    block: referrer,
                    content: "cleared again".into(),
                },
            ]),
        )
        .unwrap();
    assert!(matches!(
        engine
            .stage_ready(ready(&archive, &restore_markdown))
            .disposition,
        BatchDisposition::Accepted { .. }
    ));

    let embed = engine
        .prepare_fixture_transaction(
            author(43_103, 43_103),
            &tx(vec![
                SemanticOperation::MutateBlockLogseqIdentity {
                    block: target,
                    mutation: LogseqIdentityMutation::Generate {
                        logseq_uuid: embed_uuid,
                        trigger: LogseqIdentityTrigger::BlockEmbed { referrer },
                    },
                },
                SemanticOperation::EditBlockContent {
                    block: referrer,
                    content: format!("{{{{embed (({embed_uuid}))}}}}"),
                },
            ]),
        )
        .unwrap();
    assert!(matches!(
        engine.stage_ready(ready(&archive, &embed)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(
        engine.materialize_page(ids.page_a).unwrap().blocks[0].logseq_identity_origin,
        Some(LogseqIdentityOrigin::PolicyGenerated {
            reason: PolicyGeneratedAnchorReason::BlockEmbed,
        })
    );

    let exported_block = crate::oplog::BlockId::from_uuid(uuid(43_104));
    let exported_uuid = LogseqUuid::from_uuid(uuid(43_105));
    let exported = engine
        .prepare_fixture_transaction(
            author(43_106, 43_106),
            &tx(vec![
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id: exported_block,
                        home_document_id: test_block_home(exported_block),
                    },
                    page_id: ids.page_b,
                    parent: None,
                    order: "exported".into(),
                    content: "explicit export target".into(),
                },
                SemanticOperation::MutateBlockLogseqIdentity {
                    block: BlockLocation {
                        block_id: exported_block,
                        home_document_id: test_block_home(exported_block),
                    },
                    mutation: LogseqIdentityMutation::Generate {
                        logseq_uuid: exported_uuid,
                        trigger: LogseqIdentityTrigger::ExportUserAction,
                    },
                },
            ]),
        )
        .unwrap();
    assert!(matches!(
        engine.stage_ready(ready(&archive, &exported)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(
        engine
            .materialize_page(ids.page_b)
            .unwrap()
            .blocks
            .iter()
            .find(|block| block.block_id == exported_block)
            .unwrap()
            .logseq_identity_origin,
        Some(LogseqIdentityOrigin::PolicyGenerated {
            reason: PolicyGeneratedAnchorReason::Export,
        })
    );
}

#[test]
fn sparse_uuid_claim_index_converges_and_invalidates_reference_frontiers() {
    let ids = Ids::new();
    let dir = TestDir::new("sparse-uuid-claims");
    let archive = store(&dir, ids);
    let (mut seed, genesis_ready) = seed_engine(ids, &archive);
    let block_b = crate::oplog::BlockId::from_uuid(uuid(44_001));
    let create_b = seed
        .prepare_fixture_transaction(
            author(44_002, 44_002),
            &tx(vec![SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id: block_b,
                    home_document_id: test_block_home(block_b),
                },
                page_id: ids.page_b,
                parent: None,
                order: "b".into(),
                content: "second claimant".into(),
            }]),
        )
        .unwrap();
    let create_b_ready = ready(&archive, &create_b);
    assert!(matches!(
        seed.stage_ready(create_b_ready.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let duplicate = LogseqUuid::from_uuid(uuid(44_003));
    let (left, right) = concurrent_ready_from(
        ids,
        &archive,
        &[genesis_ready.clone(), create_b_ready.clone()],
        author(44_004, 44_004),
        tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
            block: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(ids.block_a),
            },
            mutation: LogseqIdentityMutation::AssignExternal {
                logseq_uuid: duplicate,
            },
        }]),
        author(44_005, 44_005),
        tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
            block: BlockLocation {
                block_id: block_b,
                home_document_id: test_block_home(block_b),
            },
            mutation: LogseqIdentityMutation::AssignExternal {
                logseq_uuid: duplicate,
            },
        }]),
    );
    let durable_batch_ids = [
        genesis_ready.manifest().batch_id(),
        create_b_ready.manifest().batch_id(),
        left.manifest().batch_id(),
        right.manifest().batch_id(),
    ];
    let mut ab = apply_pair_from(
        ids,
        &[genesis_ready.clone(), create_b_ready.clone()],
        left.clone(),
        right.clone(),
    );
    let ba = apply_pair_from(ids, &[genesis_ready, create_b_ready], right, left);
    assert_eq!(
        ab.canonical_snapshot().unwrap(),
        ba.canonical_snapshot().unwrap()
    );
    assert_eq!(
        ab.resolve_logseq_uuid(duplicate),
        Ok(LogseqUuidResolution::Ambiguous { claim_count: 2 })
    );
    assert_eq!(
        ba.resolve_logseq_uuid(duplicate),
        Ok(LogseqUuidResolution::Ambiguous { claim_count: 2 })
    );
    assert_eq!(
        ab.materialize_page(ids.page_a).unwrap().blocks[0].logseq_uuid,
        Some(duplicate)
    );
    assert_eq!(
        ab.materialize_page(ids.page_b).unwrap().blocks[0].logseq_uuid,
        Some(duplicate)
    );

    let reader = ObjectStore::open(&dir.path().join("store"), ids.workspace).unwrap();
    let mut durable =
        ShardedHotEngine::with_clean_archive_store_for_test(reader, ids.lineage, ids.catalog);
    for batch_id in durable_batch_ids {
        assert!(matches!(
            durable.stage_archive_batch(batch_id).unwrap().disposition,
            BatchDisposition::Accepted { .. }
        ));
    }
    assert!(matches!(
        durable.authorize_projection_write(ids.page_a),
        Err(EngineError::AmbiguousLogseqUuid {
            logseq_uuid,
            claim_count: 2,
        }) if logseq_uuid == duplicate
    ));

    let reference = ab
        .prepare_fixture_transaction(
            author(44_006, 44_006),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_c,
                    home_document_id: test_block_home(ids.block_c),
                },
                content: format!("ambiguous (({duplicate}))"),
            }]),
        )
        .unwrap();
    assert!(matches!(
        ab.stage_ready(ready(&archive, &reference)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        ab.materialize_page_for_projection(ids.page_c),
        Err(EngineError::AmbiguousLogseqUuid {
            logseq_uuid,
            claim_count: 2,
        }) if logseq_uuid == duplicate
    ));

    let remove_b = ab
        .prepare_fixture_transaction(
            author(44_007, 44_007),
            &tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
                block: BlockLocation {
                    block_id: block_b,
                    home_document_id: test_block_home(block_b),
                },
                mutation: LogseqIdentityMutation::RemoveExternal,
            }]),
        )
        .unwrap();
    assert!(matches!(
        ab.stage_ready(ready(&archive, &remove_b)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let unique_frontier = ab.materialize_page_for_projection(ids.page_c).unwrap();
    let unique_documents: Vec<_> = unique_frontier
        .frontier
        .documents()
        .iter()
        .map(DocumentDependencies::document_id)
        .collect();
    assert!(unique_documents.contains(&DocumentKey::Entity(ids.block_home_a())));
    assert!(unique_documents.contains(&DocumentKey::Entity(test_block_home(block_b))));
    assert_eq!(unique_frontier.claim_evidence.len(), 1);
    assert_eq!(unique_frontier.claim_evidence[0].participants().len(), 2);

    let remove_a = ab
        .prepare_fixture_transaction(
            author(44_008, 44_008),
            &tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                mutation: LogseqIdentityMutation::RemoveExternal,
            }]),
        )
        .unwrap();
    assert!(matches!(
        ab.stage_ready(ready(&archive, &remove_a)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(
        ab.resolve_logseq_uuid(duplicate),
        Ok(LogseqUuidResolution::Unclaimed)
    );
    let removed_frontier = ab.materialize_page_for_projection(ids.page_c).unwrap();
    let removed_documents: Vec<_> = removed_frontier
        .frontier
        .documents()
        .iter()
        .map(DocumentDependencies::document_id)
        .collect();
    assert!(removed_documents.contains(&DocumentKey::Entity(ids.block_home_a())));
    assert!(removed_documents.contains(&DocumentKey::Entity(test_block_home(block_b))));
    assert_eq!(removed_frontier.claim_evidence[0].participants().len(), 2);
    assert_ne!(unique_frontier.frontier, removed_frontier.frontier);
}

#[test]
fn deleting_page_invalidates_uuid_claim_but_retains_participant_evidence() {
    let ids = Ids::new();
    let dir = TestDir::new("page-delete-uuid-claim");
    let archive = store(&dir, ids);
    let (mut author_engine, genesis) = seed_engine(ids, &archive);
    let claimed = LogseqUuid::from_uuid(uuid(44_100));
    let assign = author_engine
        .prepare_fixture_transaction(
            author(44_101, 44_101),
            &tx(vec![
                SemanticOperation::MutateBlockLogseqIdentity {
                    block: BlockLocation {
                        block_id: ids.block_a,
                        home_document_id: test_block_home(ids.block_a),
                    },
                    mutation: LogseqIdentityMutation::AssignExternal {
                        logseq_uuid: claimed,
                    },
                },
                SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: ids.block_c,
                        home_document_id: test_block_home(ids.block_c),
                    },
                    content: format!("reference (({claimed}))"),
                },
            ]),
        )
        .unwrap();
    let assign_ready = ready(&archive, &assign);
    assert!(matches!(
        author_engine.stage_ready(assign_ready.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let delete = author_engine
        .prepare_fixture_transaction(
            author(44_102, 44_102),
            &tx(vec![SemanticOperation::DeletePage {
                page_id: ids.page_a,
            }]),
        )
        .unwrap();
    let delete_ready = ready(&archive, &delete);
    assert!(matches!(
        author_engine.stage_ready(delete_ready.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));

    let reader = ObjectStore::open(&dir.path().join("store"), ids.workspace).unwrap();
    let mut replay =
        ShardedHotEngine::with_clean_archive_store_for_test(reader, ids.lineage, ids.catalog);
    for batch_id in [
        genesis.manifest().batch_id(),
        assign_ready.manifest().batch_id(),
        delete_ready.manifest().batch_id(),
    ] {
        let outcome = replay.stage_archive_batch(batch_id).unwrap();
        assert!(
            matches!(outcome.disposition, BatchDisposition::Accepted { .. }),
            "batch {batch_id}: {outcome:?}"
        );
    }
    assert_eq!(
        replay.resolve_logseq_uuid(claimed),
        Ok(LogseqUuidResolution::Unclaimed)
    );
    let deleted_page = replay.materialize_page(ids.page_a);
    assert!(matches!(
        deleted_page,
        Err(EngineError::PageDeleted(page_id)) if page_id == ids.page_a
    ), "fresh replay must retain the page deletion after invalidating its UUID claim; got {deleted_page:?}");
    let reference = replay.materialize_page_for_projection(ids.page_c).unwrap();
    assert_eq!(reference.claim_evidence.len(), 1);
    assert_eq!(
        reference.claim_evidence[0].participants()[0].block_id(),
        ids.block_a
    );
    assert!(reference
        .frontier
        .documents()
        .iter()
        .any(|document| document.document_id() == DocumentKey::Entity(ids.block_home_a())));
    replay.authorize_projection_write(ids.page_c).unwrap();
}

#[test]
fn store_backed_uuid_claim_lookup_stays_point_local_and_hot_memory_bounded() {
    const CLAIMS: usize = 128;

    let ids = Ids::new();
    let dir = TestDir::new("uuid-claim-scaling");
    let archive = store(&dir, ids);
    let (mut author_engine, genesis) = seed_engine(ids, &archive);
    let mut operations = Vec::with_capacity(CLAIMS * 2);
    let mut target = None;
    for index in 0..CLAIMS {
        let block_id = crate::oplog::BlockId::from_uuid(uuid(45_000 + index as u128));
        let logseq_uuid = LogseqUuid::from_uuid(uuid(46_000 + index as u128));
        target = Some((block_id, logseq_uuid));
        operations.push(SemanticOperation::CreateBlock {
            block: BlockLocation {
                block_id,
                home_document_id: test_block_home(block_id),
            },
            page_id: ids.page_a,
            parent: None,
            order: format!("scale-{index:04}"),
            content: format!("scaled block {index}"),
        });
        operations.push(SemanticOperation::MutateBlockLogseqIdentity {
            block: BlockLocation {
                block_id,
                home_document_id: test_block_home(block_id),
            },
            mutation: LogseqIdentityMutation::AssignExternal { logseq_uuid },
        });
    }
    let bulk = author_engine
        .prepare_fixture_transaction(author(46_500, 46_500), &tx(operations))
        .unwrap();
    let bulk = ready(&archive, &bulk);
    assert!(matches!(
        author_engine.stage_ready(bulk.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));

    let reader = ObjectStore::open(&dir.path().join("store"), ids.workspace).unwrap();
    let mut replay =
        ShardedHotEngine::with_clean_archive_store_for_test(reader, ids.lineage, ids.catalog);
    for batch_id in [genesis.manifest().batch_id(), bulk.manifest().batch_id()] {
        assert!(matches!(
            replay.stage_archive_batch(batch_id).unwrap().disposition,
            BatchDisposition::Accepted { .. }
        ));
    }
    assert_eq!(replay.instrumentation().logseq_claim_hot_entries, CLAIMS);
    let (target_block, target_uuid) = target.unwrap();
    assert!(matches!(
        replay.resolve_logseq_uuid(target_uuid),
        Ok(LogseqUuidResolution::Unique(claim))
            if claim.block_id == target_block
                && claim.home_document_id == test_block_home(target_block)
    ));
    let after = replay.instrumentation();
    assert_eq!(after.logseq_claim_hot_entries, CLAIMS);
}

#[test]
fn author_cannot_alias_a_page_home_to_the_catalog() {
    let ids = Ids::new();
    let engine = ids.engine();
    let outcome = engine.prepare_fixture_transaction(
        author(99, 99),
        &tx(vec![SemanticOperation::CreatePage {
            page_id: ids.page_a,
            home_document_id: ids.catalog,
            name: crate::oplog::LogicalPageName::parse("A").unwrap(),
            path: path("pages/A.md"),
            kind: ManagedTextKind::Page,
        }]),
    );

    assert!(matches!(outcome, Err(EngineError::InvalidTransaction(_))));
}

fn rebuild(
    manifest: &OperationBatch,
    objects: Vec<OperationObject>,
    frontier: FrontierV2,
) -> PreparedBatch {
    let semantic = objects
        .iter()
        .find(|object| object.kind() == ObjectKind::SemanticEffect)
        .unwrap();
    let descriptors = objects
        .iter()
        .map(OperationObject::descriptor)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let causal_dependency_heads = frontier
        .documents()
        .iter()
        .flat_map(|dependencies| dependencies.direct_dependency_heads().iter().copied())
        .collect();
    let manifest = OperationBatch::new_with_causality(
        manifest.workspace_id(),
        manifest.lineage_digest(),
        manifest.batch_id(),
        manifest.author_device_id(),
        manifest.author_session_id(),
        BatchOrigin::BootstrapImport,
        BatchCausalDot::new(
            CausalPeerId::from_key(WriterIncarnationId::fixture_for_device(
                manifest.author_device_id(),
            )),
            1,
        )
        .unwrap(),
        causal_dependency_heads,
        frontier,
        SemanticEffectDigest::of(semantic.payload()),
        descriptors,
    )
    .unwrap();
    PreparedBatch::new(manifest, objects).unwrap()
}

fn rebuild_as(
    manifest: &OperationBatch,
    batch_id: BatchId,
    objects: Vec<OperationObject>,
    frontier: FrontierV2,
) -> PreparedBatch {
    let semantic = objects
        .iter()
        .find(|object| object.kind() == ObjectKind::SemanticEffect)
        .unwrap();
    let descriptors = objects
        .iter()
        .map(OperationObject::descriptor)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let causal_dependency_heads = frontier
        .documents()
        .iter()
        .flat_map(|dependencies| dependencies.direct_dependency_heads().iter().copied())
        .collect();
    let manifest = OperationBatch::new_with_causality(
        manifest.workspace_id(),
        manifest.lineage_digest(),
        batch_id,
        manifest.author_device_id(),
        manifest.author_session_id(),
        BatchOrigin::BootstrapImport,
        BatchCausalDot::new(
            CausalPeerId::from_key(WriterIncarnationId::fixture_for_device(
                manifest.author_device_id(),
            )),
            1,
        )
        .unwrap(),
        causal_dependency_heads,
        frontier,
        SemanticEffectDigest::of(semantic.payload()),
        descriptors,
    )
    .unwrap();
    PreparedBatch::new(manifest, objects).unwrap()
}

#[derive(Serialize, Deserialize)]
struct TestCrdtUpdatePayload {
    schema_version: u32,
    batch_id: BatchId,
    document_id: DocumentKey,
    dependency_heads: Vec<BatchId>,
    batch_dependency_heads: Vec<BatchId>,
    causal_state_digest: Option<DocumentCausalDigest>,
    raw_update: Vec<u8>,
}

#[derive(Serialize)]
struct TestSemanticEffectWire {
    semantic_effect_schema_version: u32,
    pages: Vec<PageDelta>,
    page_preambles: Vec<PagePreambleDelta>,
    blocks: Vec<BlockDelta>,
    memberships: Vec<MembershipDelta>,
}

fn unchecked_semantic_effect_bytes(declared: &SemanticEffect, pages: Vec<PageDelta>) -> Vec<u8> {
    let wire = TestSemanticEffectWire {
        semantic_effect_schema_version: SEMANTIC_EFFECT_SCHEMA_VERSION,
        pages,
        page_preambles: declared.page_preambles().to_vec(),
        blocks: declared.blocks().to_vec(),
        memberships: declared.memberships().to_vec(),
    };
    let mut bytes = b"TINESEM1".to_vec();
    bytes.extend(postcard::to_allocvec(&wire).unwrap());
    bytes
}

/// Rebind the private CRDT envelope to a replacement compact frontier while
/// retaining the raw Loro update. This constructs a canonical, internally
/// coherent witness without adding a production mutation API.
fn rebuild_with_compact_witness(prepared: &PreparedBatch, frontier: FrontierV2) -> PreparedBatch {
    let batch_dependency_heads: Vec<_> = frontier
        .documents()
        .iter()
        .flat_map(|dependencies| dependencies.direct_dependency_heads().iter().copied())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let objects = prepared
        .objects()
        .iter()
        .map(|object| {
            if object.kind() != ObjectKind::CrdtUpdate {
                return object.clone();
            }
            let mut payload: TestCrdtUpdatePayload =
                postcard::from_bytes(object.payload()).unwrap();
            let dependencies = frontier
                .documents()
                .iter()
                .find(|dependencies| dependencies.document_id() == object.document_id());
            payload.dependency_heads = dependencies
                .into_iter()
                .flat_map(|dependencies| dependencies.direct_dependency_heads().iter().copied())
                .collect();
            payload.batch_dependency_heads = batch_dependency_heads.clone();
            payload.causal_state_digest =
                dependencies.map(DocumentDependencies::causal_state_digest);
            OperationObject::new(
                object.workspace_id(),
                object.document_id(),
                object.kind(),
                postcard::to_allocvec(&payload).unwrap(),
            )
            .unwrap()
        })
        .collect();
    rebuild(prepared.manifest(), objects, frontier)
}

#[test]
fn moved_away_block_keeps_stable_home_and_page_read_loads_only_referenced_homes() {
    let ids = Ids::new();
    let dir = TestDir::new("stable-home");
    let archive = store(&dir, ids);
    let (mut engine, _) = seed_engine(ids, &archive);

    let moved = engine
        .prepare_fixture_transaction(
            author(101, 101),
            &tx(vec![SemanticOperation::MoveSubtree {
                root: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                from_page_id: ids.page_a,
                to_page_id: ids.page_b,
                parent: None,
                order: "moved".into(),
            }]),
        )
        .unwrap();
    assert!(matches!(
        engine.stage_ready(ready(&archive, &moved)).disposition,
        BatchDisposition::Accepted { .. }
    ));

    assert!(engine
        .materialize_page(ids.page_a)
        .unwrap()
        .blocks
        .is_empty());
    let page = engine.materialize_page(ids.page_b).unwrap();
    assert_eq!(page.blocks.len(), 1);
    assert_eq!(page.blocks[0].home_document_id, ids.block_home_a());
    assert_eq!(page.blocks[0].content, "home A content");
    assert_eq!(page.stats.catalog_documents_loaded, 1);
    assert_eq!(page.stats.membership_documents_loaded, 1);
    assert_eq!(page.stats.distinct_home_documents, vec![ids.block_home_a()]);
    assert!(!page
        .stats
        .distinct_home_documents
        .contains(&ids.block_home_c()));
}

#[test]
fn malformed_unrelated_shard_rejects_without_poisoning_sparse_page_reads() {
    let ids = Ids::new();
    let dir = TestDir::new("unrelated-malformed");
    let archive = store(&dir, ids);
    let (mut engine, _) = seed_engine(ids, &archive);
    let edit = engine
        .prepare_fixture_transaction(
            author(102, 102),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_c,
                    home_document_id: test_block_home(ids.block_c),
                },
                content: "will be malformed".into(),
            }]),
        )
        .unwrap();
    let objects = edit
        .objects()
        .iter()
        .map(|object| {
            if object.kind() == ObjectKind::CrdtUpdate {
                OperationObject::new(
                    ids.workspace,
                    object.document_id(),
                    ObjectKind::CrdtUpdate,
                    b"not-a-loro-update".to_vec(),
                )
                .unwrap()
            } else {
                object.clone()
            }
        })
        .collect();
    let malformed = rebuild(
        edit.manifest(),
        objects,
        edit.manifest().dependency_frontier().clone(),
    );
    let malformed_batch_id = malformed.manifest().batch_id();
    assert!(matches!(
        engine.stage_ready(ready(&archive, &malformed)).disposition,
        BatchDisposition::Rejected { .. }
    ));
    let page = engine.materialize_page(ids.page_a).unwrap();
    assert_eq!(page.blocks[0].content, "home A content");
    assert_eq!(page.stats.distinct_home_documents, vec![ids.block_home_a()]);

    let dependent = engine
        .prepare_fixture_transaction(
            author(108, 108),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "must not publish".into(),
            }]),
        )
        .unwrap();
    let original = &dependent.manifest().dependency_frontier().documents()[0];
    let mut direct_heads = original.direct_dependency_heads().to_vec();
    direct_heads.push(malformed_batch_id);
    direct_heads.sort_unstable();
    direct_heads.dedup();
    let referenced_frontier = FrontierV2::new(vec![DocumentDependencies::new(
        original.document_id(),
        original.peer_counters().to_vec(),
        direct_heads,
    )
    .unwrap()])
    .unwrap();
    let referenced = rebuild_with_compact_witness(&dependent, referenced_frontier);
    assert!(matches!(
        engine.stage_ready(ready(&archive, &referenced)).disposition,
        BatchDisposition::Rejected {
            error: EngineError::RejectedDependency(batch_id),
            ..
        } if batch_id == malformed_batch_id
    ));
}

#[test]
fn correction11_cold_aged_page_reopens_replays_and_authors_without_history_range_scan() {
    const CATALOG_DOCUMENTS: usize = 1;
    const MAX_HOT_DOCUMENTS: usize = MAX_HOT_NON_CATALOG_DOCUMENTS + CATALOG_DOCUMENTS;
    // Each page contributes its page entity, block entity, and membership
    // document. Crossing half the non-catalog limit therefore retires every
    // page entity plus a small margin of block entities, using fewer pages
    // than the non-catalog document limit.
    const PAGES: usize = MAX_HOT_NON_CATALOG_DOCUMENTS / 2 + 8;
    let ids = Ids::new();
    let dir = TestDir::new("cold-aged-page");
    let archive_path = dir.path().join("archive");
    let writer = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let reader = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let mut author_engine = ShardedHotEngine::new(ids.workspace, ids.lineage, ids.catalog);
    let mut engine =
        ShardedHotEngine::with_clean_archive_store_for_test(reader, ids.lineage, ids.catalog);
    let mut operations = Vec::with_capacity(PAGES * 2);
    for index in 0..PAGES {
        let page_id = PageId::from_uuid(uuid(80_000 + index as u128));
        let home_document_id = DocumentId::from_uuid(uuid(81_000 + index as u128));
        let block_id = crate::oplog::BlockId::from_uuid(uuid(82_000 + index as u128));
        operations.push(SemanticOperation::CreatePage {
            page_id,
            home_document_id,
            name: crate::oplog::LogicalPageName::parse(format!("Aged {index:03}")).unwrap(),
            path: path(&format!("pages/Aged {index:03}.md")),
            kind: ManagedTextKind::Page,
        });
        operations.push(SemanticOperation::CreateBlock {
            block: BlockLocation {
                block_id,
                home_document_id: test_block_home(block_id),
            },
            page_id,
            parent: None,
            order: "a".into(),
            content: format!("initial {index}"),
        });
    }
    let genesis = author_engine
        .prepare_fixture_transaction(author(83_000, 83_000), &tx(operations))
        .unwrap();
    let genesis_ready = ready(&writer, &genesis);
    assert!(matches!(
        author_engine.stage_ready(genesis_ready).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        engine
            .stage_archive_batch(genesis.manifest().batch_id())
            .unwrap()
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(engine.instrumentation().document_hot_entries <= MAX_HOT_DOCUMENTS);

    let cold_index = (0..PAGES)
        .min_by_key(|index| {
            test_block_home(crate::oplog::BlockId::from_uuid(uuid(
                82_000 + *index as u128,
            )))
        })
        .unwrap();
    let cold_page = PageId::from_uuid(uuid(80_000 + cold_index as u128));
    let cold_home = DocumentId::from_uuid(uuid(81_000 + cold_index as u128));
    let cold_block = crate::oplog::BlockId::from_uuid(uuid(82_000 + cold_index as u128));
    assert!(!engine.is_document_resident_for_test(DocumentKey::Entity(cold_home)));
    assert!(!engine.is_document_resident_for_test(DocumentKey::Entity(test_block_home(cold_block))));
    let edit = author_engine
        .prepare_fixture_transaction(
            author(83_001, 83_000),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: cold_block,
                    home_document_id: test_block_home(cold_block),
                },
                content: "edited after eviction".into(),
            }]),
        )
        .unwrap();
    let edit_ready = ready(&writer, &edit);
    assert!(matches!(
        author_engine.stage_ready(edit_ready).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let edit_disposition = engine
        .stage_archive_batch(edit.manifest().batch_id())
        .unwrap()
        .disposition;
    assert!(
        matches!(edit_disposition, BatchDisposition::Accepted { .. }),
        "cold edit disposition: {edit_disposition:?}"
    );
    let materialized = engine.materialize_page(cold_page).unwrap();
    assert_eq!(materialized.blocks[0].content, "edited after eviction");
    let instrumentation = engine.instrumentation();
    assert!(instrumentation.document_hot_entries <= MAX_HOT_DOCUMENTS);

    let genesis_id = genesis.manifest().batch_id();
    let edit_id = edit.manifest().batch_id();
    drop(engine);

    let replay_reader = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let mut replay = ShardedHotEngine::with_clean_archive_store_for_test(
        replay_reader,
        ids.lineage,
        ids.catalog,
    );
    for batch_id in [genesis_id, edit_id] {
        assert!(matches!(
            replay.stage_archive_batch(batch_id).unwrap().disposition,
            BatchDisposition::Accepted { .. }
        ));
    }
    assert!(replay.instrumentation().document_hot_entries <= MAX_HOT_DOCUMENTS);
    assert_eq!(
        replay.materialize_page(cold_page).unwrap().blocks[0].content,
        "edited after eviction"
    );

    let authored = replay
        .prepare_fixture_transaction(
            author(83_002, 83_000),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: cold_block,
                    home_document_id: test_block_home(cold_block),
                },
                content: "authored after cold replay".into(),
            }]),
        )
        .unwrap();
    publish_fixture(&writer, &authored);
    assert!(matches!(
        replay
            .stage_archive_batch(authored.manifest().batch_id())
            .unwrap()
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(
        replay.materialize_page(cold_page).unwrap().blocks[0].content,
        "authored after cold replay"
    );
    assert!(replay.instrumentation().document_hot_entries <= MAX_HOT_DOCUMENTS);
}

#[test]
fn external_cold_replay_concurrent_old_base_map_and_text_edits_converge() {
    let ids = Ids::new();
    let dir = TestDir::new("external-cold-concurrent");
    let archive_path = dir.path().join("archive");
    let archive = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let baseline = genesis(ids, &ids.engine());
    let baseline_ready = ready(&archive, &baseline);

    let concurrent_pair = |left_author, left_tx, right_author, right_tx| {
        let mut left = ids.engine();
        let mut right = ids.engine();
        left.stage_ready(baseline_ready.clone());
        right.stage_ready(baseline_ready.clone());
        let left = left
            .prepare_fixture_transaction(left_author, &left_tx)
            .unwrap();
        let right = right
            .prepare_fixture_transaction(right_author, &right_tx)
            .unwrap();
        publish_fixture(&archive, &left);
        publish_fixture(&archive, &right);
        [left.manifest().batch_id(), right.manifest().batch_id()]
    };

    let map_batches = concurrent_pair(
        author(83_100, 83_100),
        tx(vec![SemanticOperation::EditPagePath {
            page_id: ids.page_a,
            path: path("pages/concurrent-left.md"),
        }]),
        author(83_101, 83_101),
        tx(vec![SemanticOperation::EditPagePath {
            page_id: ids.page_a,
            path: path("pages/concurrent-right.md"),
        }]),
    );
    let text_batches = concurrent_pair(
        author(83_102, 83_102),
        tx(vec![SemanticOperation::EditBlockContent {
            block: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(ids.block_a),
            },
            content: "concurrent left text".into(),
        }]),
        author(83_103, 83_103),
        tx(vec![SemanticOperation::EditBlockContent {
            block: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(ids.block_a),
            },
            content: "concurrent right text".into(),
        }]),
    );

    for batches in [map_batches, text_batches] {
        let mut snapshots = Vec::new();
        for order in [batches, [batches[1], batches[0]]] {
            let reader = ObjectStore::open(&archive_path, ids.workspace).unwrap();
            let mut receiver = ShardedHotEngine::with_clean_archive_store_for_test(
                reader,
                ids.lineage,
                ids.catalog,
            );
            assert!(matches!(
                receiver
                    .stage_archive_batch(baseline.manifest().batch_id())
                    .unwrap()
                    .disposition,
                BatchDisposition::Accepted { .. }
            ));
            assert!(matches!(
                receiver.stage_archive_batch(order[0]).unwrap().disposition,
                BatchDisposition::Accepted { .. }
            ));
            assert!(matches!(
                receiver.stage_archive_batch(order[1]).unwrap().disposition,
                BatchDisposition::Accepted { .. }
            ));
            assert_eq!(receiver.status().accepted_batch_ids().unwrap().len(), 3);
            receiver.materialize_page(ids.page_a).unwrap();
            snapshots.push(receiver.canonical_snapshot().unwrap());
        }
        assert_eq!(snapshots[0], snapshots[1]);
    }
}

#[test]
fn late_block_creation_after_long_causal_chain_uses_bounded_semantic_replay() {
    const CHAIN: usize = 48;
    let ids = Ids::new();
    let dir = TestDir::new("late-block-causal-chain");
    let archive_path = dir.path().join("archive");
    let writer = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let reader = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let mut engine =
        ShardedHotEngine::with_clean_archive_store_for_test(reader, ids.lineage, ids.catalog);
    let initial = pages_only_genesis(ids, &engine, 84_000);
    publish_fixture(&writer, &initial);
    engine
        .stage_archive_batch(initial.manifest().batch_id())
        .unwrap();
    for index in 0..CHAIN {
        let page_id = if index % 2 == 0 {
            ids.page_a
        } else {
            ids.page_b
        };
        let edit = engine
            .prepare_fixture_transaction(
                author(84_001 + index as u128, 84_000),
                &tx(vec![SemanticOperation::EditPagePath {
                    page_id,
                    path: path(&format!("pages/chain-{index:03}.md")),
                }]),
            )
            .unwrap();
        publish_fixture(&writer, &edit);
        assert!(matches!(
            engine
                .stage_archive_batch(edit.manifest().batch_id())
                .unwrap()
                .disposition,
            BatchDisposition::Accepted { .. }
        ));
    }
    let before = engine.instrumentation();
    let create = engine
        .prepare_fixture_transaction(
            author(84_100, 84_000),
            &tx(vec![SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                page_id: ids.page_a,
                parent: None,
                order: "late".into(),
                content: "late block".into(),
            }]),
        )
        .unwrap();
    publish_fixture(&writer, &create);
    assert!(matches!(
        engine
            .stage_archive_batch(create.manifest().batch_id())
            .unwrap()
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    let after = engine.instrumentation();
    assert!(after.ancestry_traversals - before.ancestry_traversals <= 3);
    assert_eq!(engine.materialize_page(ids.page_a).unwrap().blocks.len(), 1);
}

#[test]
#[ignore = "sparse archive-open scaling measurement"]
fn sparse_archive_open_cost_is_independent_of_unrelated_batch_count() {
    use std::time::Instant;

    let ids = Ids::new();
    let unrelated = std::env::var("TINE_SPARSE_UNRELATED_BATCHES")
        .ok()
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(250);
    let dir = TestDir::new("sparse-open-measurement");
    let archive_path = dir.path().join("archive");
    let writer = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let baseline = genesis(ids, &ids.engine());
    publish_fixture(&writer, &baseline);
    let reader = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let mut engine =
        ShardedHotEngine::with_clean_archive_store_for_test(reader, ids.lineage, ids.catalog);
    engine
        .stage_archive_batch(baseline.manifest().batch_id())
        .unwrap();

    for index in 0..unrelated {
        let fixture = ids.engine();
        let prepared = fixture
            .prepare_fixture_transaction(
                author(50_000 + index as u128, 50_000 + index as u64),
                &tx(vec![SemanticOperation::CreatePage {
                    page_id: PageId::from_uuid(uuid(60_000 + index as u128)),
                    home_document_id: DocumentId::from_uuid(uuid(70_000 + index as u128)),
                    name: crate::oplog::LogicalPageName::parse(format!("Unrelated {index:08}"))
                        .unwrap(),
                    path: path(&format!("pages/Unrelated {index:08}.md")),
                    kind: ManagedTextKind::Page,
                }]),
            )
            .unwrap();
        publish_fixture(&writer, &prepared);
    }
    let started = Instant::now();
    let page = engine.materialize_page(ids.page_a).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(page.stats.catalog_documents_loaded, 1);
    assert_eq!(page.stats.membership_documents_loaded, 1);
    assert_eq!(page.stats.home_documents_loaded, 1);
    assert_eq!(page.stats.distinct_home_documents, vec![ids.home_a]);
    assert_eq!(page.stats.physical_manifest_reads, 1);
    assert_eq!(page.stats.physical_object_reads, 1);
    eprintln!(
        "sparse_archive_open unrelated_batches={unrelated} target_batches=1 referenced_homes=1 manifest_reads={} object_reads={} elapsed_us={}",
        page.stats.physical_manifest_reads,
        page.stats.physical_object_reads,
        elapsed.as_micros(),
    );
}

#[test]
fn materialization_block_collection_has_one_owner_and_no_owned_arena() {
    let source = include_str!("hot_engine.rs");
    let start = source
        .find("    fn materialize_page_from_state")
        .expect("the shared state materializer remains present");
    let end = source[start..]
        .find("\n    fn projection_frontier_contains_path_acquisition")
        .map(|offset| start + offset)
        .expect("the materialization region remains bounded");
    let region = &source[start..end];

    assert_eq!(
        region.matches("fn materialize_page_blocks").count(),
        1,
        "I-12: one helper owns membership and block collection"
    );
    assert!(
        !region.contains("let mut by_home ="),
        "the one-document-per-block layout must not rebuild the retired shard grouping"
    );
    assert_eq!(
        region.matches("self.materialize_page_blocks(").count(),
        2,
        "I-12: both materializers must delegate to the one collection helper materialize_page_blocks (oplog/hot_engine.rs)"
    );
    assert!(
        region.contains("MaterializationDocument::Owned(home)"),
        "I-9: the owned arm must hold at most one hot-document clone at a time (oplog/hot_engine.rs materialize_page_blocks)"
    );
    assert!(
        region.contains("let home = document(home_id)?;"),
        "I-9: the borrowed arm must resolve documents lazily per home (oplog/hot_engine.rs materialize_page_blocks)"
    );
    assert!(
        !region.contains("BTreeMap<DocumentId, MaterializationDocument"),
        "I-9: a fold must not retain an all-home owned arena on the hot path; imitate materialize_page_blocks in oplog/hot_engine.rs"
    );
}

#[test]
fn incomplete_store_batch_becomes_ready_without_early_visibility() {
    let ids = Ids::new();
    let dir = TestDir::new("incomplete");
    let archive = store(&dir, ids);
    let mut engine = ids.engine();
    let prepared = genesis(ids, &engine);
    stage_fixture_manifest(&archive, &prepared);
    assert!(matches!(
        engine
            .stage_from_store(&archive, prepared.manifest().batch_id())
            .unwrap()
            .disposition,
        BatchDisposition::IncompleteStaged {
            missing_objects,
            ..
        } if missing_objects == prepared.objects().len()
    ));
    assert!(engine.canonical_snapshot().unwrap().pages.is_empty());
    for object in prepared.objects().iter().rev() {
        archive
            .stage_object_bytes(&object.encode().unwrap())
            .unwrap();
    }
    assert!(matches!(
        engine
            .stage_from_store(&archive, prepared.manifest().batch_id())
            .unwrap()
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(engine.canonical_snapshot().unwrap().pages.len(), 3);
}

#[test]
fn workspace_and_lineage_mismatches_reject_without_visible_mutation() {
    let ids = Ids::new();
    let foreign_workspace_ids = Ids {
        workspace: WorkspaceId::from_uuid(uuid(9_001)),
        ..ids
    };
    let workspace_dir = TestDir::new("workspace-mismatch");
    let workspace_store = store(&workspace_dir, foreign_workspace_ids);
    let foreign_workspace_engine = foreign_workspace_ids.engine();
    let foreign_workspace_batch = ready(
        &workspace_store,
        &genesis(foreign_workspace_ids, &foreign_workspace_engine),
    );
    let mut receiver = ids.engine();
    assert!(matches!(
        receiver.stage_ready(foreign_workspace_batch).disposition,
        BatchDisposition::Rejected {
            error: EngineError::WorkspaceMismatch { .. },
            ..
        }
    ));
    assert!(receiver.canonical_snapshot().unwrap().pages.is_empty());

    let foreign_lineage_ids = Ids {
        lineage: LineageDigest::of(b"foreign-lineage"),
        ..ids
    };
    let lineage_dir = TestDir::new("lineage-mismatch");
    let lineage_store = store(&lineage_dir, foreign_lineage_ids);
    let foreign_lineage_engine = foreign_lineage_ids.engine();
    let foreign_lineage_batch = ready(
        &lineage_store,
        &genesis(foreign_lineage_ids, &foreign_lineage_engine),
    );
    assert!(matches!(
        receiver.stage_ready(foreign_lineage_batch).disposition,
        BatchDisposition::Rejected {
            error: EngineError::LineageMismatch { .. },
            ..
        }
    ));
    assert!(receiver.canonical_snapshot().unwrap().pages.is_empty());
}

#[test]
fn conflicting_reuse_of_an_accepted_batch_id_rejects_without_rollback() {
    let ids = Ids::new();
    let first_dir = TestDir::new("batch-id-first");
    let first_store = store(&first_dir, ids);
    let first_author = ids.engine();
    let first = genesis(ids, &first_author);
    let mut receiver = ids.engine();
    assert!(matches!(
        receiver
            .stage_ready(ready(&first_store, &first))
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    let before = receiver.canonical_snapshot().unwrap();

    let collision_dir = TestDir::new("batch-id-collision");
    let collision_store = store(&collision_dir, ids);
    let collision_author = ids.engine();
    let collision = collision_author
        .prepare_fixture_transaction(
            author(100, 100),
            &tx(vec![SemanticOperation::CreatePage {
                page_id: ids.page_a,
                home_document_id: ids.home_a,
                name: crate::oplog::LogicalPageName::parse("Conflicting").unwrap(),
                path: path("pages/Conflicting.md"),
                kind: ManagedTextKind::Page,
            }]),
        )
        .unwrap();
    assert!(matches!(
        receiver
            .stage_ready(ready(&collision_store, &collision))
            .disposition,
        BatchDisposition::Rejected {
            error: EngineError::BatchCollision(_),
            ..
        }
    ));
    assert_eq!(receiver.canonical_snapshot().unwrap(), before);
    assert_eq!(
        receiver.status().accepted_batch_ids().unwrap(),
        vec![author(100, 100).batch_id]
    );
}

#[test]
fn crdt_payload_is_bound_to_batch_and_same_batch_replay_is_a_duplicate_noop() {
    let ids = Ids::new();
    let dir = TestDir::new("payload-batch-binding");
    let archive = store(&dir, ids);
    let engine = ids.engine();
    let prepared = genesis(ids, &engine);
    let foreign_batch_id = BatchId::from_uuid(uuid(9_999));
    let rebound = rebuild_as(
        prepared.manifest(),
        foreign_batch_id,
        prepared.objects().to_vec(),
        prepared.manifest().dependency_frontier().clone(),
    );
    let mut receiver = ids.engine();
    assert!(matches!(
        receiver.stage_ready(ready(&archive, &rebound)).disposition,
        BatchDisposition::Rejected {
            error: EngineError::CrdtPayloadIdentityMismatch { .. },
            ..
        }
    ));

    let ready = ready(&archive, &prepared);
    assert!(matches!(
        receiver.stage_ready(ready.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        receiver.stage_ready(ready).disposition,
        BatchDisposition::DuplicateAccepted { .. }
    ));
}

#[test]
fn concurrent_same_block_id_in_distinct_homes_blocks_canonically_in_every_order() {
    let ids = Ids::new();
    let block_id = crate::oplog::BlockId::from_uuid(uuid(32));
    let block_home_a = claim_home(block_id, ids.home_a);
    let block_home_b = claim_home(block_id, ids.home_b);
    let dir = TestDir::new("concurrent-immutable-home-conflict");
    let archive_path = dir.path().join("archive");
    let archive = ObjectStore::open(&archive_path, ids.workspace).unwrap();

    let fixture = ids.engine();
    let genesis = fixture
        .prepare_fixture_transaction(
            author(100, 100),
            &tx(vec![
                SemanticOperation::CreatePage {
                    page_id: ids.page_a,
                    home_document_id: ids.home_a,
                    name: crate::oplog::LogicalPageName::parse("A").unwrap(),
                    path: path("pages/A.md"),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreatePage {
                    page_id: ids.page_b,
                    home_document_id: ids.home_b,
                    name: crate::oplog::LogicalPageName::parse("B").unwrap(),
                    path: path("pages/B.md"),
                    kind: ManagedTextKind::Page,
                },
            ]),
        )
        .unwrap();
    publish_fixture(&archive, &genesis);
    let genesis_id = genesis.manifest().batch_id();
    let genesis_ready = match archive.inspect_batch(genesis_id).unwrap() {
        BatchInspection::Ready(batch) => batch,
        other => panic!("expected ready genesis, found {other:?}"),
    };

    let mut author_a = ids.engine();
    let mut author_b = ids.engine();
    assert!(matches!(
        author_a.stage_ready(genesis_ready.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        author_b.stage_ready(genesis_ready.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let created_a = author_a
        .prepare_fixture_transaction(
            author(103, 103),
            &tx(vec![SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id,
                    home_document_id: block_home_a,
                },
                page_id: ids.page_a,
                parent: None,
                order: "x-a".into(),
                content: "concurrent content A".into(),
            }]),
        )
        .unwrap();
    let created_b = author_b
        .prepare_fixture_transaction(
            author(104, 104),
            &tx(vec![SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id,
                    home_document_id: block_home_b,
                },
                page_id: ids.page_b,
                parent: None,
                order: "x-b".into(),
                content: "concurrent content B".into(),
            }]),
        )
        .unwrap();
    publish_fixture(&archive, &created_a);
    publish_fixture(&archive, &created_b);
    let batch_a_id = created_a.manifest().batch_id();
    let batch_b_id = created_b.manifest().batch_id();
    let batch_a = match archive.inspect_batch(batch_a_id).unwrap() {
        BatchInspection::Ready(batch) => batch,
        other => panic!("expected ready A, found {other:?}"),
    };
    let batch_b = match archive.inspect_batch(batch_b_id).unwrap() {
        BatchInspection::Ready(batch) => batch,
        other => panic!("expected ready B, found {other:?}"),
    };

    assert!(matches!(
        author_a.stage_ready(batch_a.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        author_b.stage_ready(batch_b.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let dependent_a = author_a
        .prepare_fixture_transaction(
            author(105, 105),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id,
                    home_document_id: block_home_a,
                },
                content: "later content A".into(),
            }]),
        )
        .unwrap();
    let dependent_b = author_b
        .prepare_fixture_transaction(
            author(106, 106),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id,
                    home_document_id: block_home_b,
                },
                content: "later content B".into(),
            }]),
        )
        .unwrap();
    publish_fixture(&archive, &dependent_a);
    publish_fixture(&archive, &dependent_b);
    let dependent_a_id = dependent_a.manifest().batch_id();
    let dependent_b_id = dependent_b.manifest().batch_id();
    let dependent_a = match archive.inspect_batch(dependent_a_id).unwrap() {
        BatchInspection::Ready(batch) => batch,
        other => panic!("expected ready dependent A, found {other:?}"),
    };
    let dependent_b = match archive.inspect_batch(dependent_b_id).unwrap() {
        BatchInspection::Ready(batch) => batch,
        other => panic!("expected ready dependent B, found {other:?}"),
    };

    let expected = ImmutableHomeEvidence::new(vec![ImmutableHomeConflict::new(
        block_id,
        ImmutableHomeClaim::new(batch_a_id, block_home_a),
        ImmutableHomeClaim::new(batch_b_id, block_home_b),
    )]);
    for (mut engine, staged, conflicting) in [
        (author_a, dependent_b.clone(), batch_b.clone()),
        (author_b, dependent_a.clone(), batch_a.clone()),
    ] {
        assert!(matches!(
            engine.stage_ready(staged.clone()).disposition,
            BatchDisposition::IncompleteStaged { .. }
        ));
        assert!(matches!(
            engine.stage_ready(conflicting.clone()).disposition,
            BatchDisposition::Quarantined
        ));
        assert_eq!(engine.fatal_evidence(), Some(&expected));
        let expected_handle = engine.fatal_evidence_handle().unwrap();
        let outcome = engine.stage_ready(staged);
        assert!(
            matches!(outcome.disposition, BatchDisposition::Quarantined),
            "concurrent duplicate claim must quarantine, got {outcome:?}"
        );
        assert!(matches!(
            engine.stage_ready(conflicting).disposition,
            BatchDisposition::Quarantined
        ));
        assert!(matches!(
            engine.prepare_fixture_transaction(
                author(107, 107),
                &tx(vec![SemanticOperation::EditPagePath {
                    page_id: ids.page_a,
                    path: path("pages/blocked.md"),
                }]),
            ),
            Err(EngineError::WorkspaceBlocked(found)) if found == expected_handle
        ));
        assert!(matches!(
            engine.materialize_page(ids.page_a),
            Err(EngineError::WorkspaceBlocked(found)) if found == expected_handle
        ));
        assert!(matches!(
            engine.canonical_snapshot(),
            Err(EngineError::WorkspaceBlocked(found)) if found == expected_handle
        ));
        assert!(matches!(
            engine.recover_block_state(block_home_a, block_id),
            Err(EngineError::WorkspaceBlocked(found)) if found == expected_handle
        ));
        assert_eq!(engine.status().accepted_batch_ids().unwrap().len(), 2);
    }

    for (first, staged, conflicting) in [
        (batch_a_id, dependent_b_id, batch_b_id),
        (batch_b_id, dependent_a_id, batch_a_id),
    ] {
        let replay_store = ObjectStore::open(&archive_path, ids.workspace).unwrap();
        let mut replay = ShardedHotEngine::with_clean_archive_store_for_test(
            replay_store,
            ids.lineage,
            ids.catalog,
        );
        assert!(matches!(
            replay.stage_archive_batch(genesis_id).unwrap().disposition,
            BatchDisposition::Accepted { .. }
        ));
        assert!(matches!(
            replay.stage_archive_batch(first).unwrap().disposition,
            BatchDisposition::Accepted { .. }
        ));
        assert!(matches!(
            replay.stage_archive_batch(staged).unwrap().disposition,
            BatchDisposition::IncompleteStaged { .. }
        ));
        let conflicting_outcome = replay.stage_archive_batch(conflicting).unwrap();
        assert!(
            matches!(
                conflicting_outcome.disposition,
                BatchDisposition::Quarantined
            ),
            "unexpected replay conflict disposition: {:?}",
            conflicting_outcome.disposition
        );
        assert_eq!(paged_fatal_evidence(&replay), Some(expected.clone()));
        let expected_handle = replay.fatal_evidence_handle().unwrap();
        assert!(matches!(
            archive.inspect_batch(batch_a_id).unwrap(),
            BatchInspection::Ready(_)
        ));
        assert!(matches!(
            archive.inspect_batch(batch_b_id).unwrap(),
            BatchInspection::Ready(_)
        ));
        assert!(matches!(
            archive.inspect_batch(staged).unwrap(),
            BatchInspection::Ready(_)
        ));
        assert!(matches!(
            replay.stage_archive_batch(staged).unwrap().disposition,
            BatchDisposition::Quarantined
        ));
        assert!(matches!(
            replay.canonical_snapshot(),
            Err(EngineError::WorkspaceBlocked(found)) if found == expected_handle
        ));
        assert_eq!(replay.status().accepted_batch_ids().unwrap().len(), 2);
    }
}

#[test]
fn crossed_concurrent_identity_collisions_converge_live_and_from_fresh_store() {
    let ids = Ids::new();
    let block_x = crate::oplog::BlockId::from_uuid(uuid(40));
    let block_y = crate::oplog::BlockId::from_uuid(uuid(41));
    let dir = TestDir::new("crossed-identity-collisions");
    let archive_path = dir.path().join("archive");
    let archive = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let genesis = pages_only_genesis(ids, &ids.engine(), 200);
    let genesis_ready = ready(&archive, &genesis);

    let mut author_a = ids.engine();
    let mut author_b = ids.engine();
    author_a.stage_ready(genesis_ready.clone());
    author_b.stage_ready(genesis_ready.clone());
    let prepared_a = create_blocks(
        &author_a,
        201,
        &[
            (block_x, ids.page_a, ids.home_a, "x-a"),
            (block_y, ids.page_b, ids.home_b, "y-b"),
        ],
    );
    let prepared_b = create_blocks(
        &author_b,
        202,
        &[
            (block_y, ids.page_a, ids.home_a, "y-a"),
            (block_x, ids.page_b, ids.home_b, "x-b"),
        ],
    );
    let batch_a = ready(&archive, &prepared_a);
    let batch_b = ready(&archive, &prepared_b);
    let expected = ImmutableHomeEvidence::new(vec![
        ImmutableHomeConflict::new(
            block_x,
            ImmutableHomeClaim::new(
                prepared_a.manifest().batch_id(),
                claim_home(block_x, ids.home_a),
            ),
            ImmutableHomeClaim::new(
                prepared_b.manifest().batch_id(),
                claim_home(block_x, ids.home_b),
            ),
        ),
        ImmutableHomeConflict::new(
            block_y,
            ImmutableHomeClaim::new(
                prepared_b.manifest().batch_id(),
                claim_home(block_y, ids.home_a),
            ),
            ImmutableHomeClaim::new(
                prepared_a.manifest().batch_id(),
                claim_home(block_y, ids.home_b),
            ),
        ),
    ]);

    let mut live_evidence = Vec::new();
    for order in [
        [batch_a.clone(), batch_b.clone()],
        [batch_b.clone(), batch_a.clone()],
    ] {
        let mut receiver = ids.engine();
        receiver.stage_ready(genesis_ready.clone());
        for batch in order {
            receiver.stage_ready(batch);
        }
        live_evidence.push(receiver.fatal_evidence().cloned().unwrap());
    }
    assert_eq!(live_evidence, vec![expected.clone(), expected.clone()]);

    let genesis_id = genesis.manifest().batch_id();
    let batch_a_id = prepared_a.manifest().batch_id();
    let batch_b_id = prepared_b.manifest().batch_id();
    let mut replay_evidence = Vec::new();
    for order in [[batch_a_id, batch_b_id], [batch_b_id, batch_a_id]] {
        let store = ObjectStore::open(&archive_path, ids.workspace).unwrap();
        let mut receiver =
            ShardedHotEngine::with_clean_archive_store_for_test(store, ids.lineage, ids.catalog);
        receiver.stage_archive_batch(genesis_id).unwrap();
        for batch_id in order {
            receiver.stage_archive_batch(batch_id).unwrap();
        }
        assert!(receiver.instrumentation().block_claim_hot_entries <= 2);
        let first = receiver.fatal_evidence_page(None, 1).unwrap().unwrap();
        assert_eq!(first.conflicts().len(), 1);
        let second = receiver
            .fatal_evidence_page(first.next(), 1)
            .unwrap()
            .unwrap();
        assert_eq!(second.conflicts().len(), 1);
        assert_eq!(second.next(), None);
        replay_evidence.push(ImmutableHomeEvidence::new(
            first
                .conflicts()
                .iter()
                .chain(second.conflicts())
                .cloned()
                .collect(),
        ));
        assert_eq!(receiver.instrumentation().conflict_hot_entries, 2);
    }
    assert_eq!(replay_evidence, live_evidence);
}

#[test]
fn concurrent_same_home_duplicate_creation_converges_after_fresh_replay() {
    let ids = Ids::new();
    let block_id = crate::oplog::BlockId::from_uuid(uuid(56));
    let dir = TestDir::new("same-home-duplicate-replay");
    let archive_path = dir.path().join("archive");
    let archive = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let genesis = pages_only_genesis(ids, &ids.engine(), 300);
    publish_fixture(&archive, &genesis);
    let genesis_ready = ready(&archive, &genesis);

    let mut author_a = ids.engine();
    let mut author_b = ids.engine();
    author_a.stage_ready(genesis_ready.clone());
    author_b.stage_ready(genesis_ready);
    let claim_a = create_blocks(&author_a, 301, &[(block_id, ids.page_a, ids.home_a, "a")]);
    let claim_b = author_b
        .prepare_fixture_transaction(
            author(302, 302),
            &tx(vec![SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id,
                    home_document_id: claim_home(block_id, ids.home_a),
                },
                page_id: ids.page_a,
                parent: None,
                order: "b".into(),
                content: "concurrent same-home duplicate".into(),
            }]),
        )
        .unwrap();
    publish_fixture(&archive, &claim_a);
    publish_fixture(&archive, &claim_b);

    let mut snapshots = Vec::new();
    for order in [
        [claim_a.manifest().batch_id(), claim_b.manifest().batch_id()],
        [claim_b.manifest().batch_id(), claim_a.manifest().batch_id()],
    ] {
        let store = ObjectStore::open(&archive_path, ids.workspace).unwrap();
        let mut replay =
            ShardedHotEngine::with_clean_archive_store_for_test(store, ids.lineage, ids.catalog);
        assert!(matches!(
            replay
                .stage_archive_batch(genesis.manifest().batch_id())
                .unwrap()
                .disposition,
            BatchDisposition::Accepted { .. }
        ));
        for batch_id in order {
            assert!(matches!(
                replay.stage_archive_batch(batch_id).unwrap().disposition,
                BatchDisposition::Accepted { .. }
            ));
        }
        assert_eq!(replay.fatal_evidence(), None);
        assert!(replay.instrumentation().block_claim_hot_entries <= 1);
        snapshots.push(replay.canonical_snapshot().unwrap());
    }
    assert_eq!(snapshots[0], snapshots[1]);
}

#[test]
fn three_concurrent_identity_claims_and_later_blocked_ingress_have_one_evidence_set() {
    let ids = Ids::new();
    let block_id = crate::oplog::BlockId::from_uuid(uuid(42));
    let dir = TestDir::new("three-identity-claims");
    let archive = store(&dir, ids);
    let genesis = pages_only_genesis(ids, &ids.engine(), 210);
    let genesis_ready = ready(&archive, &genesis);
    let mut claims = Vec::new();
    for (batch, page_id, home_document_id) in [
        (211, ids.page_a, ids.home_a),
        (212, ids.page_b, ids.home_b),
        (213, ids.page_c, ids.home_c),
    ] {
        let mut claim_author = ids.engine();
        claim_author.stage_ready(genesis_ready.clone());
        let prepared = create_blocks(
            &claim_author,
            batch,
            &[(block_id, page_id, home_document_id, "claim")],
        );
        claims.push(ready(&archive, &prepared));
    }
    let mut malformed_author = ids.engine();
    malformed_author.stage_ready(genesis_ready.clone());
    let malformed_prepared = create_blocks(
        &malformed_author,
        214,
        &[(block_id, ids.page_a, ids.home_a, "invalid")],
    );
    let malformed_objects = malformed_prepared
        .objects()
        .iter()
        .map(|object| {
            if object.kind() == ObjectKind::CrdtUpdate {
                OperationObject::new(
                    ids.workspace,
                    object.document_id(),
                    ObjectKind::CrdtUpdate,
                    b"invalid-crdt-evidence".to_vec(),
                )
                .unwrap()
            } else {
                object.clone()
            }
        })
        .collect();
    let malformed = rebuild(
        malformed_prepared.manifest(),
        malformed_objects,
        malformed_prepared.manifest().dependency_frontier().clone(),
    );
    let malformed = ready(&archive, &malformed);

    let permutations = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let mut evidence = Vec::new();
    for permutation in permutations {
        let mut receiver = ids.engine();
        receiver.stage_ready(genesis_ready.clone());
        receiver.stage_ready(claims[permutation[0]].clone());
        receiver.stage_ready(claims[permutation[1]].clone());
        assert!(receiver.fatal_evidence().is_some());
        receiver.stage_ready(claims[permutation[2]].clone());
        let before_invalid = receiver.fatal_evidence().cloned().unwrap();
        let terminal_before_invalid = receiver
            .status()
            .validated_unpublished_batch_ids()
            .unwrap()
            .to_vec();
        assert!(matches!(
            receiver.stage_ready(malformed.clone()).disposition,
            BatchDisposition::Rejected { .. }
        ));
        assert_eq!(receiver.fatal_evidence(), Some(&before_invalid));
        assert_eq!(
            receiver.status().validated_unpublished_batch_ids().unwrap(),
            terminal_before_invalid
        );
        evidence.push(before_invalid);
    }
    let expected = ImmutableHomeEvidence::new(vec![ImmutableHomeConflict::from_claims(
        block_id,
        [
            ImmutableHomeClaim::new(
                BatchId::from_uuid(uuid(211)),
                claim_home(block_id, ids.home_a),
            ),
            ImmutableHomeClaim::new(
                BatchId::from_uuid(uuid(212)),
                claim_home(block_id, ids.home_b),
            ),
            ImmutableHomeClaim::new(
                BatchId::from_uuid(uuid(213)),
                claim_home(block_id, ids.home_c),
            ),
        ],
    )]);
    assert_eq!(evidence, vec![expected; permutations.len()]);
}

fn permutations_of_four() -> Vec<[usize; 4]> {
    let mut permutations = Vec::new();
    for a in 0..4 {
        for b in 0..4 {
            for c in 0..4 {
                for d in 0..4 {
                    let candidate = [a, b, c, d];
                    if candidate
                        .iter()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        == 4
                    {
                        permutations.push(candidate);
                    }
                }
            }
        }
    }
    permutations
}

#[test]
fn correction6_four_independent_claims_retain_complete_evidence_in_all_orders() {
    let ids = Ids::new();
    let block_x = crate::oplog::BlockId::from_uuid(uuid(46));
    let block_y = crate::oplog::BlockId::from_uuid(uuid(47));
    let dir = TestDir::new("correction6-four-independent-claims");
    let archive_path = dir.path().join("archive");
    let archive = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let genesis = pages_only_genesis(ids, &ids.engine(), 240);
    let genesis_ready = ready(&archive, &genesis);
    let mut batches = Vec::new();
    for (batch, block_id, page_id, home_document_id) in [
        (241, block_x, ids.page_a, ids.home_a),
        (242, block_x, ids.page_b, ids.home_b),
        (243, block_y, ids.page_a, ids.home_a),
        (244, block_y, ids.page_b, ids.home_b),
    ] {
        let mut claim_author = ids.engine();
        claim_author.stage_ready(genesis_ready.clone());
        batches.push(ready(
            &archive,
            &create_blocks(
                &claim_author,
                batch,
                &[(block_id, page_id, home_document_id, "claim")],
            ),
        ));
    }
    let expected = ImmutableHomeEvidence::new(vec![
        ImmutableHomeConflict::new(
            block_x,
            ImmutableHomeClaim::new(
                BatchId::from_uuid(uuid(241)),
                claim_home(block_x, ids.home_a),
            ),
            ImmutableHomeClaim::new(
                BatchId::from_uuid(uuid(242)),
                claim_home(block_x, ids.home_b),
            ),
        ),
        ImmutableHomeConflict::new(
            block_y,
            ImmutableHomeClaim::new(
                BatchId::from_uuid(uuid(243)),
                claim_home(block_y, ids.home_a),
            ),
            ImmutableHomeClaim::new(
                BatchId::from_uuid(uuid(244)),
                claim_home(block_y, ids.home_b),
            ),
        ),
    ]);

    let permutations = permutations_of_four();
    assert_eq!(permutations.len(), 24);
    for permutation in &permutations {
        let mut receiver = ids.engine();
        receiver.stage_ready(genesis_ready.clone());
        for index in permutation {
            receiver.stage_ready(batches[*index].clone());
        }
        assert_eq!(paged_fatal_evidence(&receiver), Some(expected.clone()));
    }

    let genesis_id = genesis.manifest().batch_id();
    let batch_ids: Vec<_> = batches
        .iter()
        .map(|batch| batch.manifest().batch_id())
        .collect();
    for permutation in permutations {
        let store = ObjectStore::open(&archive_path, ids.workspace).unwrap();
        let mut receiver =
            ShardedHotEngine::with_clean_archive_store_for_test(store, ids.lineage, ids.catalog);
        receiver.stage_archive_batch(genesis_id).unwrap();
        for index in permutation {
            receiver.stage_archive_batch(batch_ids[index]).unwrap();
        }
        let handle = receiver.fatal_evidence_handle().unwrap();
        assert_eq!(handle.conflicting_block_count(), 2);
        assert_eq!(handle.claim_count(), 4);
        let instrumentation = receiver.instrumentation();
        assert_eq!(instrumentation.conflict_hot_entries, 2);
        assert_eq!(instrumentation.batch_status_hot_entries, 5);
        assert_eq!(instrumentation.ready_payload_hot_entries, 0);
        assert!(instrumentation.document_hot_entries <= MAX_HOT_NON_CATALOG_DOCUMENTS + 1);
        assert_eq!(paged_fatal_evidence(&receiver), Some(expected.clone()));
    }
}

#[test]
fn correction6_blocked_frontier_validates_child_before_parent_and_finds_new_conflict() {
    let ids = Ids::new();
    let conflict_x = crate::oplog::BlockId::from_uuid(uuid(48));
    let conflict_y = crate::oplog::BlockId::from_uuid(uuid(49));
    let parent_block = crate::oplog::BlockId::from_uuid(uuid(50));
    let dir = TestDir::new("correction6-blocked-frontier-chain");
    let archive = store(&dir, ids);
    let genesis = pages_only_genesis(ids, &ids.engine(), 250);
    let genesis_ready = ready(&archive, &genesis);

    let mut left_author = ids.engine();
    left_author.stage_ready(genesis_ready.clone());
    let x_left = ready(
        &archive,
        &create_blocks(
            &left_author,
            251,
            &[(conflict_x, ids.page_a, ids.home_a, "x-left")],
        ),
    );
    let mut right_author = ids.engine();
    right_author.stage_ready(genesis_ready.clone());
    let x_right = ready(
        &archive,
        &create_blocks(
            &right_author,
            252,
            &[(conflict_x, ids.page_b, ids.home_b, "x-right")],
        ),
    );
    let y_right = ready(
        &archive,
        &create_blocks(
            &right_author,
            253,
            &[(conflict_y, ids.page_b, ids.home_b, "y-right")],
        ),
    );

    let mut chain_author = ids.engine();
    chain_author.stage_ready(genesis_ready.clone());
    let parent = create_blocks(
        &chain_author,
        260,
        &[(parent_block, ids.page_a, ids.home_a, "parent")],
    );
    let parent_ready = ready(&archive, &parent);
    chain_author.stage_ready(parent_ready.clone());
    // The child BatchId deliberately sorts before its parent so blocked
    // draining must reach a fixed point instead of relying on BatchId order.
    let child = chain_author
        .prepare_fixture_transaction(
            author(259, 259),
            &tx(vec![
                SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: parent_block,
                        home_document_id: claim_home(parent_block, ids.home_a),
                    },
                    content: "child depends on parent".into(),
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id: conflict_y,
                        home_document_id: claim_home(conflict_y, ids.home_a),
                    },
                    page_id: ids.page_a,
                    parent: None,
                    order: "child".into(),
                    content: "child conflict".into(),
                },
            ]),
        )
        .unwrap();
    let child_ready = ready(&archive, &child);

    let mut receiver = ids.engine();
    receiver.stage_ready(genesis_ready);
    receiver.stage_ready(x_left);
    assert!(matches!(
        receiver.stage_ready(x_right).disposition,
        BatchDisposition::Quarantined
    ));
    assert!(matches!(
        receiver.stage_ready(child_ready.clone()).disposition,
        BatchDisposition::IncompleteStaged { .. }
    ));
    assert!(matches!(
        receiver.stage_ready(y_right).disposition,
        BatchDisposition::Quarantined
    ));
    assert!(matches!(
        receiver.stage_ready(parent_ready).disposition,
        BatchDisposition::Quarantined
    ));
    let child_outcome = receiver.stage_ready(child_ready).disposition;
    assert!(
        matches!(child_outcome, BatchDisposition::Quarantined),
        "unexpected terminal child outcome: {child_outcome:?}"
    );
    let expected = ImmutableHomeEvidence::new(vec![
        ImmutableHomeConflict::new(
            conflict_x,
            ImmutableHomeClaim::new(
                BatchId::from_uuid(uuid(251)),
                claim_home(conflict_x, ids.home_a),
            ),
            ImmutableHomeClaim::new(
                BatchId::from_uuid(uuid(252)),
                claim_home(conflict_x, ids.home_b),
            ),
        ),
        ImmutableHomeConflict::new(
            conflict_y,
            ImmutableHomeClaim::new(
                BatchId::from_uuid(uuid(259)),
                claim_home(conflict_y, ids.home_a),
            ),
            ImmutableHomeClaim::new(
                BatchId::from_uuid(uuid(253)),
                claim_home(conflict_y, ids.home_b),
            ),
        ),
    ]);
    assert_eq!(receiver.fatal_evidence(), Some(&expected));

    let store = ObjectStore::open(&dir.path().join("store"), ids.workspace).unwrap();
    let mut replay =
        ShardedHotEngine::with_clean_archive_store_for_test(store, ids.lineage, ids.catalog);
    for batch_id in [
        genesis.manifest().batch_id(),
        BatchId::from_uuid(uuid(251)),
        BatchId::from_uuid(uuid(252)),
        child.manifest().batch_id(),
        BatchId::from_uuid(uuid(253)),
        parent.manifest().batch_id(),
    ] {
        replay.stage_archive_batch(batch_id).unwrap();
    }
    assert_eq!(paged_fatal_evidence(&replay), Some(expected.clone()));
    assert_eq!(
        replay.status().validated_unpublished_batch_ids().unwrap(),
        &[
            BatchId::from_uuid(uuid(252)),
            BatchId::from_uuid(uuid(253)),
            BatchId::from_uuid(uuid(259)),
            BatchId::from_uuid(uuid(260)),
        ]
    );
}

#[test]
fn correction6_latching_batch_retains_novel_claim_for_later_conflict() {
    let ids = Ids::new();
    let block_x = crate::oplog::BlockId::from_uuid(uuid(51));
    let block_y = crate::oplog::BlockId::from_uuid(uuid(52));
    let dir = TestDir::new("correction6-latch-batch-novel-claim");
    let archive = store(&dir, ids);
    let genesis = pages_only_genesis(ids, &ids.engine(), 270);
    let genesis_ready = ready(&archive, &genesis);

    let mut left = ids.engine();
    left.stage_ready(genesis_ready.clone());
    let x_left = ready(
        &archive,
        &create_blocks(&left, 271, &[(block_x, ids.page_a, ids.home_a, "x-left")]),
    );
    let mut right = ids.engine();
    right.stage_ready(genesis_ready.clone());
    let latch = ready(
        &archive,
        &create_blocks(
            &right,
            272,
            &[
                (block_x, ids.page_b, ids.home_b, "x-right"),
                (block_y, ids.page_a, ids.home_a, "y-left"),
            ],
        ),
    );
    let y_right = ready(
        &archive,
        &create_blocks(&right, 273, &[(block_y, ids.page_b, ids.home_b, "y-right")]),
    );

    let mut receiver = ids.engine();
    for batch in [genesis_ready, x_left, latch, y_right] {
        receiver.stage_ready(batch);
    }
    assert_eq!(receiver.fatal_evidence().unwrap().conflicts().len(), 2);
    assert_eq!(
        receiver
            .fatal_evidence()
            .unwrap()
            .conflicts()
            .iter()
            .map(ImmutableHomeConflict::block_id)
            .collect::<Vec<_>>(),
        vec![block_x, block_y]
    );
}

#[test]
fn correction6_quarantined_parent_makes_causal_duplicate_child_reject() {
    let ids = Ids::new();
    let conflict = crate::oplog::BlockId::from_uuid(uuid(54));
    let causal_duplicate = crate::oplog::BlockId::from_uuid(uuid(55));
    let dir = TestDir::new("correction6-terminal-causal-duplicate");
    let archive = store(&dir, ids);
    let genesis = pages_only_genesis(ids, &ids.engine(), 280);
    let genesis_ready = ready(&archive, &genesis);

    let mut left = ids.engine();
    left.stage_ready(genesis_ready.clone());
    let left_claim = ready(
        &archive,
        &create_blocks(&left, 281, &[(conflict, ids.page_a, ids.home_a, "left")]),
    );
    let mut right = ids.engine();
    right.stage_ready(genesis_ready.clone());
    let right_claim = ready(
        &archive,
        &create_blocks(&right, 282, &[(conflict, ids.page_b, ids.home_b, "right")]),
    );

    let mut parent_author = ids.engine();
    parent_author.stage_ready(genesis_ready.clone());
    let parent = create_blocks(
        &parent_author,
        290,
        &[(causal_duplicate, ids.page_a, ids.home_a, "parent")],
    );
    let parent_ready = ready(&archive, &parent);
    parent_author.stage_ready(parent_ready.clone());
    let dependency_template = parent_author
        .prepare_fixture_transaction(
            author(291, 291),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: causal_duplicate,
                    home_document_id: claim_home(causal_duplicate, ids.home_a),
                },
                content: "dependency template".into(),
            }]),
        )
        .unwrap();
    let parent_home_dependency = dependency_template
        .manifest()
        .dependency_frontier()
        .documents()
        .iter()
        .find(|entry| {
            entry.document_id() == DocumentKey::Entity(claim_home(causal_duplicate, ids.home_a))
        })
        .unwrap()
        .clone();

    let duplicate = create_blocks(
        &right,
        // Sort before the parent to exercise child-before-parent draining.
        289,
        &[(causal_duplicate, ids.page_b, ids.home_b, "duplicate")],
    );
    let mut child_frontier = duplicate
        .manifest()
        .dependency_frontier()
        .documents()
        .to_vec();
    child_frontier.push(parent_home_dependency);
    let child = rebuild_with_compact_witness(&duplicate, FrontierV2::new(child_frontier).unwrap());
    let child_ready = ready(&archive, &child);

    let mut receiver = ids.engine();
    for batch in [genesis_ready, left_claim, right_claim] {
        receiver.stage_ready(batch);
    }
    assert!(matches!(
        receiver.stage_ready(child_ready.clone()).disposition,
        BatchDisposition::IncompleteStaged { .. }
    ));
    receiver.stage_ready(parent_ready);
    let child_outcome = receiver.stage_ready(child_ready).disposition;
    assert!(
        matches!(
            child_outcome,
            BatchDisposition::Rejected {
                error: EngineError::BlockAlreadyExists(found),
            }
            if found == causal_duplicate
        ),
        "unexpected causal terminal-parent outcome: {child_outcome:?}"
    );
    assert_eq!(receiver.fatal_evidence().unwrap().conflicts().len(), 1);

    let store = ObjectStore::open(&dir.path().join("store"), ids.workspace).unwrap();
    let mut replay =
        ShardedHotEngine::with_clean_archive_store_for_test(store, ids.lineage, ids.catalog);
    for batch_id in [
        genesis.manifest().batch_id(),
        BatchId::from_uuid(uuid(281)),
        BatchId::from_uuid(uuid(282)),
        child.manifest().batch_id(),
        parent.manifest().batch_id(),
    ] {
        replay.stage_archive_batch(batch_id).unwrap();
    }
    let replay_child = replay
        .stage_archive_batch(child.manifest().batch_id())
        .unwrap()
        .disposition;
    assert!(
        matches!(
            replay_child,
            BatchDisposition::Rejected {
                error: EngineError::BlockAlreadyExists(found),
            }
            if found == causal_duplicate
        ),
        "unexpected replay child disposition: {replay_child:?}"
    );
    assert_eq!(paged_fatal_evidence(&replay).unwrap().conflicts().len(), 1);
}

#[test]
fn author_refuses_same_batch_cross_home_duplicate_without_retained_claim() {
    let ids = Ids::new();
    let block_id = crate::oplog::BlockId::from_uuid(uuid(43));
    let dir = TestDir::new("same-batch-identity-duplicate");
    let archive = store(&dir, ids);
    let genesis = pages_only_genesis(ids, &ids.engine(), 220);
    let genesis_ready = ready(&archive, &genesis);
    let mut author_engine = ids.engine();
    author_engine.stage_ready(genesis_ready.clone());
    let malformed = author_engine.prepare_fixture_transaction(
        author(221, 221),
        &tx(vec![
            SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id,
                    home_document_id: test_block_home(block_id),
                },
                page_id: ids.page_a,
                parent: None,
                order: "a".into(),
                content: "a".into(),
            },
            SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id,
                    home_document_id: test_block_home(block_id),
                },
                page_id: ids.page_b,
                parent: None,
                order: "b".into(),
                content: "b".into(),
            },
        ]),
    );
    assert!(matches!(
        malformed,
        Err(EngineError::BlockAlreadyExists(found)) if found == block_id
    ));
    assert_eq!(author_engine.fatal_evidence(), None);
    assert_eq!(
        author_engine.status().accepted_batch_ids().unwrap(),
        vec![genesis.manifest().batch_id()]
    );
}

#[test]
fn mid_drain_acceptance_and_blocked_duplicate_report_truthful_batch_dispositions() {
    let ids = Ids::new();
    let conflict_id = crate::oplog::BlockId::from_uuid(uuid(44));
    let dependency_id = crate::oplog::BlockId::from_uuid(uuid(45));
    let dir = TestDir::new("mid-drain-blocked-status");
    let archive = store(&dir, ids);
    let genesis = pages_only_genesis(ids, &ids.engine(), 230);
    let genesis_ready = ready(&archive, &genesis);

    let mut author_a = ids.engine();
    author_a.stage_ready(genesis_ready.clone());
    let claim_a = create_blocks(
        &author_a,
        231,
        &[(conflict_id, ids.page_a, ids.home_a, "a")],
    );
    let claim_a_ready = ready(&archive, &claim_a);

    let mut author_b = ids.engine();
    author_b.stage_ready(genesis_ready.clone());
    let dependency = create_blocks(
        &author_b,
        232,
        &[(dependency_id, ids.page_b, ids.home_b, "dependency")],
    );
    let dependency_ready = ready(&archive, &dependency);
    author_b.stage_ready(dependency_ready.clone());
    let claim_b = author_b
        .prepare_fixture_transaction(
            author(233, 233),
            &tx(vec![
                SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: dependency_id,
                        home_document_id: claim_home(dependency_id, ids.home_b),
                    },
                    content: "claim depends on dependency".into(),
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id: conflict_id,
                        home_document_id: claim_home(conflict_id, ids.home_b),
                    },
                    page_id: ids.page_b,
                    parent: None,
                    order: "b".into(),
                    content: "conflicting claim".into(),
                },
            ]),
        )
        .unwrap();
    let claim_b_ready = ready(&archive, &claim_b);

    let mut receiver = ids.engine();
    receiver.stage_ready(genesis_ready);
    receiver.stage_ready(claim_a_ready.clone());
    assert!(matches!(
        receiver.stage_ready(claim_b_ready).disposition,
        BatchDisposition::IncompleteStaged { .. }
    ));
    let dependency_outcome = receiver.stage_ready(dependency_ready);
    assert_eq!(
        dependency_outcome.batch_id(),
        dependency.manifest().batch_id()
    );
    assert_eq!(
        dependency_outcome.disposition,
        BatchDisposition::Accepted { no_op: false }
    );
    assert_eq!(
        dependency_outcome
            .newly_accepted()
            .iter()
            .map(|accepted| accepted.batch_id)
            .collect::<Vec<_>>(),
        vec![dependency.manifest().batch_id()]
    );
    assert_eq!(
        dependency_outcome.status().workspace(),
        &WorkspaceStatus::Blocked(receiver.fatal_evidence_handle().unwrap())
    );
    assert_eq!(
        dependency_outcome.status().accepted_batch_ids().unwrap(),
        vec![
            genesis.manifest().batch_id(),
            claim_a.manifest().batch_id(),
            dependency.manifest().batch_id(),
        ]
    );
    let duplicate_outcome = receiver.stage_ready(claim_a_ready);
    assert_eq!(duplicate_outcome.batch_id(), claim_a.manifest().batch_id());
    assert_eq!(
        duplicate_outcome.disposition,
        BatchDisposition::DuplicateAccepted { no_op: false }
    );
    assert_eq!(
        duplicate_outcome.status().workspace(),
        &WorkspaceStatus::Blocked(receiver.fatal_evidence_handle().unwrap())
    );
    assert_eq!(
        receiver.status().accepted_batch_ids().unwrap(),
        vec![
            genesis.manifest().batch_id(),
            claim_a.manifest().batch_id(),
            dependency.manifest().batch_id(),
        ]
    );
}

#[test]
fn subtree_reorder_and_rename_referrer_transaction_preserve_atomic_semantics() {
    let ids = Ids::new();
    let child = crate::oplog::BlockId::from_uuid(uuid(32));
    let dir = TestDir::new("operation-surface");
    let archive = store(&dir, ids);
    let (mut engine, _) = seed_engine(ids, &archive);
    let created = engine
        .prepare_fixture_transaction(
            author(104, 104),
            &tx(vec![
                SemanticOperation::SetPagePreamble {
                    page_id: ids.page_a,
                    preamble: Some("title:: [[A]]".into()),
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id: child,
                        home_document_id: test_block_home(child),
                    },
                    page_id: ids.page_a,
                    parent: Some(ids.block_a),
                    order: "child".into(),
                    content: "ref [[A]]".into(),
                },
            ]),
        )
        .unwrap();
    engine.stage_ready(ready(&archive, &created));
    let moved_and_reordered = engine
        .prepare_fixture_transaction(
            author(105, 105),
            &tx(vec![
                SemanticOperation::MoveSubtree {
                    root: BlockLocation {
                        block_id: ids.block_a,
                        home_document_id: test_block_home(ids.block_a),
                    },
                    from_page_id: ids.page_a,
                    to_page_id: ids.page_b,
                    parent: None,
                    order: "root-moved".into(),
                },
                SemanticOperation::ReorderBlock {
                    block_id: child,
                    page_id: ids.page_b,
                    parent: Some(ids.block_a),
                    order: "child-reordered".into(),
                },
            ]),
        )
        .unwrap();
    engine.stage_ready(ready(&archive, &moved_and_reordered));
    let renamed = engine
        .prepare_fixture_transaction(
            author(106, 106),
            &tx(vec![SemanticOperation::RenamePagesAndRewriteReferrers {
                page_changes: vec![crate::oplog::PageRename {
                    page_id: ids.page_a,
                    new_name: crate::oplog::LogicalPageName::parse("A Renamed").unwrap(),
                    new_path: path("pages/A Renamed.md"),
                }],
                block_rewrites: vec![crate::oplog::BlockContentRewrite {
                    block: BlockLocation {
                        block_id: child,
                        home_document_id: test_block_home(child),
                    },
                    new_content: "ref [[A Renamed]]".into(),
                }],
                page_preamble_rewrites: vec![crate::oplog::PagePreambleRewrite {
                    page_id: ids.page_a,
                    new_preamble: Some("title:: [[A Renamed]]".into()),
                }],
            }]),
        )
        .unwrap();
    let renamed_outcome = engine.stage_ready(ready(&archive, &renamed));
    assert!(
        matches!(
            renamed_outcome.disposition,
            BatchDisposition::Accepted { .. }
        ),
        "rename outcome: {renamed_outcome:?}"
    );
    let page_b = engine.materialize_page(ids.page_b).unwrap();
    assert_eq!(page_b.blocks.len(), 2);
    let child = page_b
        .blocks
        .iter()
        .find(|block| block.block_id == child)
        .unwrap();
    assert_eq!(child.parent, Some(ids.block_a));
    assert_eq!(child.order, "child-reordered");
    assert_eq!(child.content, "ref [[A Renamed]]");
    let page_a = engine.materialize_page(ids.page_a).unwrap();
    assert_eq!(page_a.path, path("pages/A Renamed.md"));
    assert_eq!(page_a.preamble.as_deref(), Some("title:: [[A Renamed]]"));
    let snapshot = engine.canonical_snapshot().unwrap();
    assert_eq!(
        snapshot
            .pages
            .iter()
            .find(|(page_id, _)| *page_id == ids.page_a)
            .unwrap()
            .1
            .name()
            .as_str(),
        "A Renamed"
    );
}

#[test]
fn namespace_rename_updates_sorted_pages_preambles_and_blocks_atomically() {
    let ids = Ids::new();
    let child_block = crate::oplog::BlockId::from_uuid(uuid(32));
    let dir = TestDir::new("namespace-rename");
    let archive = store(&dir, ids);
    let mut engine = ids.engine();
    let create = engine
        .prepare_fixture_transaction(
            author(43_000, 43_000),
            &tx(vec![
                SemanticOperation::CreatePage {
                    page_id: ids.page_a,
                    home_document_id: ids.home_a,
                    name: crate::oplog::LogicalPageName::parse("Area").unwrap(),
                    path: path("pages/area.md"),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreatePage {
                    page_id: ids.page_b,
                    home_document_id: ids.home_b,
                    name: crate::oplog::LogicalPageName::parse("Area/Child").unwrap(),
                    path: path("pages/area___child.md"),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::SetPagePreamble {
                    page_id: ids.page_a,
                    preamble: Some("alias:: [[Area/Child]]".into()),
                },
                SemanticOperation::SetPagePreamble {
                    page_id: ids.page_b,
                    preamble: Some("parent:: [[Area]]".into()),
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id: ids.block_a,
                        home_document_id: test_block_home(ids.block_a),
                    },
                    page_id: ids.page_a,
                    parent: None,
                    order: "a".into(),
                    content: "see [[Area/Child]]".into(),
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id: child_block,
                        home_document_id: test_block_home(child_block),
                    },
                    page_id: ids.page_b,
                    parent: None,
                    order: "a".into(),
                    content: "back to [[Area]]".into(),
                },
            ]),
        )
        .unwrap();
    assert!(matches!(
        engine.stage_ready(ready(&archive, &create)).disposition,
        BatchDisposition::Accepted { .. }
    ));

    let mut block_rewrites = vec![
        crate::oplog::BlockContentRewrite {
            block: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(ids.block_a),
            },
            new_content: "see [[Domain/Child]]".into(),
        },
        crate::oplog::BlockContentRewrite {
            block: BlockLocation {
                block_id: child_block,
                home_document_id: test_block_home(child_block),
            },
            new_content: "back to [[Domain]]".into(),
        },
    ];
    block_rewrites
        .sort_unstable_by_key(|rewrite| (rewrite.block.home_document_id, rewrite.block.block_id));
    let rename = engine
        .prepare_fixture_transaction(
            author(43_001, 43_001),
            &tx(vec![SemanticOperation::RenamePagesAndRewriteReferrers {
                page_changes: vec![
                    crate::oplog::PageRename {
                        page_id: ids.page_a,
                        new_name: crate::oplog::LogicalPageName::parse("Domain").unwrap(),
                        new_path: path("pages/domain.md"),
                    },
                    crate::oplog::PageRename {
                        page_id: ids.page_b,
                        new_name: crate::oplog::LogicalPageName::parse("Domain/Child").unwrap(),
                        new_path: path("pages/domain___child.md"),
                    },
                ],
                block_rewrites,
                page_preamble_rewrites: vec![
                    crate::oplog::PagePreambleRewrite {
                        page_id: ids.page_a,
                        new_preamble: Some("alias:: [[Domain/Child]]".into()),
                    },
                    crate::oplog::PagePreambleRewrite {
                        page_id: ids.page_b,
                        new_preamble: Some("parent:: [[Domain]]".into()),
                    },
                ],
            }]),
        )
        .unwrap();
    let effect = semantic_effect(&rename);
    assert_eq!(effect.pages().len(), 2);
    assert_eq!(effect.page_preambles().len(), 2);
    assert_eq!(effect.blocks().len(), 2);
    assert!(matches!(
        engine.stage_ready(ready(&archive, &rename)).disposition,
        BatchDisposition::Accepted { .. }
    ));

    let snapshot = engine.canonical_snapshot().unwrap();
    assert_eq!(
        snapshot
            .pages
            .iter()
            .map(|(_, state)| (
                state.name().as_str(),
                state.path().unwrap().as_str(),
                state.kind()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("Domain", "pages/domain.md", ManagedTextKind::Page),
            (
                "Domain/Child",
                "pages/domain___child.md",
                ManagedTextKind::Page
            ),
        ]
    );
    let root = engine.materialize_page(ids.page_a).unwrap();
    let child = engine.materialize_page(ids.page_b).unwrap();
    assert_eq!(root.preamble.as_deref(), Some("alias:: [[Domain/Child]]"));
    assert_eq!(child.preamble.as_deref(), Some("parent:: [[Domain]]"));
    assert_eq!(root.blocks[0].content, "see [[Domain/Child]]");
    assert_eq!(child.blocks[0].content, "back to [[Domain]]");
}

#[test]
fn rename_shape_state_and_wire_validation_fail_before_mutation() {
    let ids = Ids::new();
    let page_a = crate::oplog::PageRename {
        page_id: ids.page_a,
        new_name: crate::oplog::LogicalPageName::parse("Renamed A").unwrap(),
        new_path: path("pages/Renamed A.md"),
    };
    let page_b = crate::oplog::PageRename {
        page_id: ids.page_b,
        new_name: crate::oplog::LogicalPageName::parse("Renamed B").unwrap(),
        new_path: path("pages/Renamed B.md"),
    };
    let operation = SemanticOperation::RenamePagesAndRewriteReferrers {
        page_changes: vec![page_a.clone()],
        block_rewrites: Vec::new(),
        page_preamble_rewrites: Vec::new(),
    };
    let transaction = tx(vec![operation.clone()]);
    assert_eq!(
        postcard::from_bytes::<OperationTransaction>(&postcard::to_allocvec(&transaction).unwrap())
            .unwrap(),
        transaction
    );

    for invalid_pages in [
        Vec::new(),
        vec![page_a.clone(), page_a.clone()],
        vec![page_b.clone(), page_a.clone()],
    ] {
        assert!(OperationTransaction::new(vec![
            SemanticOperation::RenamePagesAndRewriteReferrers {
                page_changes: invalid_pages,
                block_rewrites: Vec::new(),
                page_preamble_rewrites: Vec::new(),
            }
        ])
        .is_err());
    }
    let block = BlockLocation {
        block_id: ids.block_a,
        home_document_id: test_block_home(ids.block_a),
    };
    assert!(
        OperationTransaction::new(vec![SemanticOperation::RenamePagesAndRewriteReferrers {
            page_changes: vec![page_a.clone()],
            block_rewrites: vec![
                crate::oplog::BlockContentRewrite {
                    block,
                    new_content: "one".into(),
                },
                crate::oplog::BlockContentRewrite {
                    block,
                    new_content: "two".into(),
                },
            ],
            page_preamble_rewrites: Vec::new(),
        }])
        .is_err()
    );
    assert!(
        OperationTransaction::new(vec![SemanticOperation::RenamePagesAndRewriteReferrers {
            page_changes: vec![page_a.clone()],
            block_rewrites: Vec::new(),
            page_preamble_rewrites: vec![
                crate::oplog::PagePreambleRewrite {
                    page_id: ids.page_a,
                    new_preamble: Some("one".into()),
                },
                crate::oplog::PagePreambleRewrite {
                    page_id: ids.page_a,
                    new_preamble: Some("two".into()),
                },
            ],
        }])
        .is_err()
    );
    assert!(
        OperationTransaction::new(vec![SemanticOperation::RenamePagesAndRewriteReferrers {
            page_changes: vec![page_a.clone()],
            block_rewrites: vec![crate::oplog::BlockContentRewrite {
                block,
                new_content: "x".repeat(4 * 1024 * 1024 + 1),
            }],
            page_preamble_rewrites: Vec::new(),
        }])
        .is_err()
    );

    let mut malformed_name = serde_json::to_value(&operation).unwrap();
    malformed_name["rename_pages_and_rewrite_referrers"]["page_changes"][0]["new_name"] =
        serde_json::json!("\n");
    assert!(serde_json::from_value::<SemanticOperation>(malformed_name).is_err());
    let mut unknown_variant_field = serde_json::to_value(&operation).unwrap();
    unknown_variant_field["rename_pages_and_rewrite_referrers"]["future_field"] =
        serde_json::json!(true);
    assert!(serde_json::from_value::<SemanticOperation>(unknown_variant_field).is_err());
    let mut forbidden_home = serde_json::to_value(&operation).unwrap();
    forbidden_home["rename_pages_and_rewrite_referrers"]["page_changes"][0]["home_document_id"] =
        serde_json::json!(ids.home_a);
    assert!(serde_json::from_value::<SemanticOperation>(forbidden_home).is_err());
    let mut forbidden_kind = serde_json::to_value(&operation).unwrap();
    forbidden_kind["rename_pages_and_rewrite_referrers"]["page_changes"][0]["kind"] =
        serde_json::json!("journal");
    assert!(serde_json::from_value::<SemanticOperation>(forbidden_kind).is_err());

    let dir = TestDir::new("rename-validation");
    let archive = store(&dir, ids);
    let (mut engine, _) = seed_engine(ids, &archive);
    let before = engine.canonical_snapshot().unwrap();
    assert!(matches!(
        engine.prepare_fixture_transaction(
            author(43_010, 43_010),
            &tx(vec![SemanticOperation::RenamePagesAndRewriteReferrers {
                page_changes: vec![crate::oplog::PageRename {
                    page_id: PageId::from_uuid(uuid(999)),
                    new_name: crate::oplog::LogicalPageName::parse("Missing").unwrap(),
                    new_path: path("pages/Missing.md"),
                }],
                block_rewrites: Vec::new(),
                page_preamble_rewrites: Vec::new(),
            }]),
        ),
        Err(EngineError::PageNotFound(_))
    ));
    assert_eq!(engine.canonical_snapshot().unwrap(), before);

    let delete = engine
        .prepare_fixture_transaction(
            author(43_011, 43_011),
            &tx(vec![SemanticOperation::DeletePage {
                page_id: ids.page_a,
            }]),
        )
        .unwrap();
    engine.stage_ready(ready(&archive, &delete));
    let after_delete = engine.canonical_snapshot().unwrap();
    assert!(matches!(
        engine.prepare_fixture_transaction(
            author(43_012, 43_012),
            &tx(vec![SemanticOperation::RenamePagesAndRewriteReferrers {
                page_changes: vec![page_a],
                block_rewrites: Vec::new(),
                page_preamble_rewrites: Vec::new(),
            }]),
        ),
        Err(EngineError::PageDeleted(page_id)) if page_id == ids.page_a
    ));
    assert_eq!(engine.canonical_snapshot().unwrap(), after_delete);
}

#[test]
fn external_page_state_reconciliation_is_origin_gated() {
    let ids = Ids::new();
    let dir = TestDir::new("external-page-state-origin");
    let archive = store(&dir, ids);
    let (engine, _) = seed_engine(ids, &archive);
    let operation = SemanticOperation::ReconcileExternalPageState {
        page_id: ids.page_a,
        name: crate::oplog::LogicalPageName::parse("External Exact").unwrap(),
        path: path("nested/storage/external.md"),
        kind: ManagedTextKind::Journal,
    };
    let transaction = tx(vec![operation]);

    assert!(matches!(
        engine.prepare_fixture_transaction(author(43_020, 43_020), &transaction),
        Err(EngineError::InvalidTransaction(_))
    ));
    assert!(matches!(
        engine.draft_author_transaction(
            author(43_021, 43_021),
            BatchOrigin::LocalMutation,
            &transaction,
        ),
        Err(EngineError::InvalidTransaction(_))
    ));
    assert!(matches!(
        engine.draft_author_transaction(
            author(43_022, 43_022),
            BatchOrigin::ExternalReconciliation {
                import_id: crate::oplog::ImportId::derive(
                    ids.workspace,
                    &[],
                    &[],
                    crate::oplog::DIFF_SCHEMA_VERSION,
                )
                .unwrap(),
            },
            &transaction,
        ),
        Err(EngineError::Batch(reason))
            if reason.contains("requires exactly one external-import observation")
    ));
}

#[test]
fn causal_frontier_and_semantic_effect_tampering_fail_closed_at_ready_boundary() {
    let ids = Ids::new();
    let dir = TestDir::new("tamper");
    let archive = store(&dir, ids);
    let (engine, genesis_ready) = seed_engine(ids, &archive);
    let edit = engine
        .prepare_fixture_transaction(
            author(103, 103),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "edited".into(),
            }]),
        )
        .unwrap();

    let original = &edit.manifest().dependency_frontier().documents()[0];
    let tampered_frontier = FrontierV2::new(vec![DocumentDependencies::new(
        original.document_id(),
        original.peer_counters().to_vec(),
        Vec::new(),
    )
    .unwrap()])
    .unwrap();
    let frontier_tampered = rebuild(edit.manifest(), edit.objects().to_vec(), tampered_frontier);
    let frontier_ready = ready(&archive, &frontier_tampered);
    let mut receiver = ids.engine();
    receiver.stage_ready(genesis_ready.clone());
    assert!(matches!(
        receiver.stage_ready(frontier_ready).disposition,
        BatchDisposition::Rejected { .. }
    ));
    assert_eq!(
        receiver.materialize_page(ids.page_a).unwrap().blocks[0].content,
        "home A content"
    );

    let empty_effect = SemanticEffect::new(Vec::new(), Vec::new(), Vec::new())
        .unwrap()
        .encode()
        .unwrap();
    let objects = edit
        .objects()
        .iter()
        .map(|object| {
            if object.kind() == ObjectKind::SemanticEffect {
                OperationObject::new(
                    ids.workspace,
                    object.document_id(),
                    ObjectKind::SemanticEffect,
                    empty_effect.clone(),
                )
                .unwrap()
            } else {
                object.clone()
            }
        })
        .collect();
    let semantic_tampered = rebuild(
        edit.manifest(),
        objects,
        edit.manifest().dependency_frontier().clone(),
    );
    let semantic_dir = TestDir::new("semantic-tamper-store");
    let semantic_archive = store(&semantic_dir, ids);
    let mut receiver = ids.engine();
    receiver.stage_ready(genesis_ready);
    assert!(matches!(
        receiver
            .stage_ready(ready(&semantic_archive, &semantic_tampered))
            .disposition,
        BatchDisposition::Rejected { .. }
    ));
}

fn concurrent_ready(
    ids: Ids,
    archive: &ObjectStore,
    baseline: &ValidatedBatch,
    left_author: AuthorBatch,
    left_tx: OperationTransaction,
    right_author: AuthorBatch,
    right_tx: OperationTransaction,
) -> (ValidatedBatch, ValidatedBatch) {
    let mut left = ids.engine();
    let mut right = ids.engine();
    left.stage_ready(baseline.clone());
    right.stage_ready(baseline.clone());
    let left = left
        .prepare_fixture_transaction(left_author, &left_tx)
        .unwrap();
    let right = right
        .prepare_fixture_transaction(right_author, &right_tx)
        .unwrap();
    (ready(archive, &left), ready(archive, &right))
}

fn concurrent_ready_from(
    ids: Ids,
    archive: &ObjectStore,
    baselines: &[ValidatedBatch],
    left_author: AuthorBatch,
    left_tx: OperationTransaction,
    right_author: AuthorBatch,
    right_tx: OperationTransaction,
) -> (ValidatedBatch, ValidatedBatch) {
    let mut left = ids.engine();
    let mut right = ids.engine();
    for baseline in baselines {
        left.stage_ready(baseline.clone());
        right.stage_ready(baseline.clone());
    }
    let left = left
        .prepare_fixture_transaction(left_author, &left_tx)
        .unwrap();
    let right = right
        .prepare_fixture_transaction(right_author, &right_tx)
        .unwrap();
    (ready(archive, &left), ready(archive, &right))
}

fn apply_pair(
    ids: Ids,
    baseline: &ValidatedBatch,
    first: ValidatedBatch,
    second: ValidatedBatch,
) -> ShardedHotEngine {
    let mut engine = ids.engine();
    engine.stage_ready(baseline.clone());
    assert!(!matches!(
        engine.stage_ready(first).disposition,
        BatchDisposition::Rejected { .. }
    ));
    assert!(!matches!(
        engine.stage_ready(second).disposition,
        BatchDisposition::Rejected { .. }
    ));
    engine
}

fn apply_pair_from(
    ids: Ids,
    baselines: &[ValidatedBatch],
    first: ValidatedBatch,
    second: ValidatedBatch,
) -> ShardedHotEngine {
    let mut engine = ids.engine();
    for baseline in baselines {
        engine.stage_ready(baseline.clone());
    }
    assert!(!matches!(
        engine.stage_ready(first).disposition,
        BatchDisposition::Rejected { .. }
    ));
    assert!(!matches!(
        engine.stage_ready(second).disposition,
        BatchDisposition::Rejected { .. }
    ));
    engine
}

#[test]
fn concurrent_move_move_and_move_edit_converge_in_both_delivery_orders() {
    let ids = Ids::new();
    let dir = TestDir::new("move-concurrency");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let move_b = tx(vec![SemanticOperation::MoveSubtree {
        root: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        from_page_id: ids.page_a,
        to_page_id: ids.page_b,
        parent: None,
        order: "b".into(),
    }]);
    let move_c = tx(vec![SemanticOperation::MoveSubtree {
        root: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        from_page_id: ids.page_a,
        to_page_id: ids.page_c,
        parent: None,
        order: "c".into(),
    }]);
    let (left, right) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(110, 110),
        move_b.clone(),
        author(111, 111),
        move_c,
    );
    let ab = apply_pair(ids, &baseline, left.clone(), right.clone());
    let ba = apply_pair(ids, &baseline, right, left);
    assert_eq!(
        ab.canonical_snapshot().unwrap(),
        ba.canonical_snapshot().unwrap()
    );
    let visible = [ids.page_b, ids.page_c]
        .into_iter()
        .filter(|page| !ab.materialize_page(*page).unwrap().blocks.is_empty())
        .count();
    assert_eq!(visible, 1, "losing membership claim must be filtered");

    let edit = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "concurrent edit survives move".into(),
    }]);
    let (moved, edited) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(112, 112),
        move_b,
        author(113, 113),
        edit,
    );
    let ab = apply_pair(ids, &baseline, moved.clone(), edited.clone());
    let ba = apply_pair(ids, &baseline, edited, moved);
    assert_eq!(
        ab.canonical_snapshot().unwrap(),
        ba.canonical_snapshot().unwrap()
    );
    assert_eq!(
        ab.materialize_page(ids.page_b).unwrap().blocks[0].content,
        "concurrent edit survives move"
    );
}

fn move_delete_result(move_peer: u64, delete_peer: u64) -> (bool, bool) {
    let ids = Ids::new();
    let dir = TestDir::new("move-delete-direction");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let moved = tx(vec![SemanticOperation::MoveSubtree {
        root: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        from_page_id: ids.page_a,
        to_page_id: ids.page_b,
        parent: None,
        order: "m".into(),
    }]);
    let deleted = tx(vec![SemanticOperation::DeleteSubtree {
        root_block_id: ids.block_a,
        page_id: ids.page_a,
    }]);
    let (moved, deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(10_000 + move_peer as u128, move_peer),
        moved,
        author(20_000 + delete_peer as u128, delete_peer),
        deleted,
    );
    let ab = apply_pair(ids, &baseline, moved.clone(), deleted.clone());
    let ba = apply_pair(ids, &baseline, deleted, moved);
    assert_eq!(
        ab.canonical_snapshot().unwrap(),
        ba.canonical_snapshot().unwrap()
    );
    let page_won = !ab.materialize_page(ids.page_b).unwrap().blocks.is_empty();
    (page_won, !page_won)
}

#[test]
fn concurrent_move_delete_covers_page_and_tombstone_winner_directions() {
    let low_move = move_delete_result(200, 300);
    let high_move = move_delete_result(400, 300);
    assert_ne!(
        low_move, high_move,
        "peer order must exercise both register winners"
    );
    assert!(
        low_move.0 || high_move.0,
        "one direction must keep the moved page owner"
    );
    assert!(
        low_move.1 || high_move.1,
        "one direction must keep the tombstone owner"
    );
}

fn moved_away_move_delete_result(move_peer: u64, delete_peer: u64) -> bool {
    let ids = Ids::new();
    let dir = TestDir::new("moved-away-move-delete");
    let archive = store(&dir, ids);
    let (mut seed, genesis_ready) = seed_engine(ids, &archive);
    let moved_to_b = seed
        .prepare_fixture_transaction(
            author(30_000, 30_000),
            &tx(vec![SemanticOperation::MoveSubtree {
                root: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                from_page_id: ids.page_a,
                to_page_id: ids.page_b,
                parent: None,
                order: "accepted-on-b".into(),
            }]),
        )
        .unwrap();
    let moved_to_b = ready(&archive, &moved_to_b);
    assert!(matches!(
        seed.stage_ready(moved_to_b.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));

    let mut move_author = ids.engine();
    let mut delete_author = ids.engine();
    for engine in [&mut move_author, &mut delete_author] {
        engine.stage_ready(genesis_ready.clone());
        engine.stage_ready(moved_to_b.clone());
    }
    let moved_to_c = move_author
        .prepare_fixture_transaction(
            author(31_000 + move_peer as u128, move_peer),
            &tx(vec![SemanticOperation::MoveSubtree {
                root: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                from_page_id: ids.page_b,
                to_page_id: ids.page_c,
                parent: None,
                order: "raced-on-c".into(),
            }]),
        )
        .unwrap();
    let deleted_from_b = delete_author
        .prepare_fixture_transaction(
            author(32_000 + delete_peer as u128, delete_peer),
            &tx(vec![SemanticOperation::DeleteSubtree {
                root_block_id: ids.block_a,
                page_id: ids.page_b,
            }]),
        )
        .unwrap();
    let moved_to_c = ready(&archive, &moved_to_c);
    let deleted_from_b = ready(&archive, &deleted_from_b);

    let apply = |first: ValidatedBatch, second: ValidatedBatch| {
        let mut engine = ids.engine();
        engine.stage_ready(genesis_ready.clone());
        engine.stage_ready(moved_to_b.clone());
        assert!(!matches!(
            engine.stage_ready(first).disposition,
            BatchDisposition::Rejected { .. }
        ));
        assert!(!matches!(
            engine.stage_ready(second).disposition,
            BatchDisposition::Rejected { .. }
        ));
        engine
    };
    let move_then_delete = apply(moved_to_c.clone(), deleted_from_b.clone());
    let delete_then_move = apply(deleted_from_b, moved_to_c);
    assert_eq!(
        move_then_delete.canonical_snapshot().unwrap(),
        delete_then_move.canonical_snapshot().unwrap()
    );
    assert!(move_then_delete
        .materialize_page(ids.page_a)
        .unwrap()
        .blocks
        .is_empty());
    assert!(move_then_delete
        .materialize_page(ids.page_b)
        .unwrap()
        .blocks
        .is_empty());
    let page_c = move_then_delete.materialize_page(ids.page_c).unwrap();
    let moved_block = page_c
        .blocks
        .iter()
        .find(|block| block.block_id == ids.block_a);
    if let Some(block) = moved_block {
        assert_eq!(block.home_document_id, ids.block_home_a());
        assert_eq!(block.content, "home A content");
    }
    moved_block.is_some()
}

#[test]
fn moved_away_block_races_move_from_b_to_c_with_delete_from_b_both_orders_and_winners() {
    let low_move = moved_away_move_delete_result(500, 600);
    let high_move = moved_away_move_delete_result(700, 600);
    assert_ne!(
        low_move, high_move,
        "peer order must cover both the moved membership and tombstone winners"
    );
}

#[test]
fn delete_edit_retains_recoverable_crdt_content_but_hides_membership() {
    let ids = Ids::new();
    let dir = TestDir::new("delete-edit");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let deleted = tx(vec![SemanticOperation::DeleteSubtree {
        root_block_id: ids.block_a,
        page_id: ids.page_a,
    }]);
    let edited = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "recoverable concurrent content".into(),
    }]);
    let (deleted, edited) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(130, 130),
        deleted,
        author(131, 131),
        edited,
    );
    let engine = apply_pair(ids, &baseline, deleted, edited);
    assert!(engine
        .materialize_page(ids.page_a)
        .unwrap()
        .blocks
        .is_empty());
    assert!(engine
        .canonical_snapshot()
        .unwrap()
        .blocks
        .iter()
        .all(|block| block.block_id != ids.block_a));
    let recovered = engine
        .recover_block_state(ids.block_home_a(), ids.block_a)
        .unwrap()
        .expect("tombstoned home content remains in immutable CRDT history");
    assert_eq!(recovered.owner, BlockOwner::Tombstone);
    assert_eq!(recovered.content, "recoverable concurrent content");
}

#[test]
fn page_rename_delete_and_path_conflicts_are_deterministic() {
    let ids = Ids::new();
    let dir = TestDir::new("page-conflicts");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let renamed = tx(vec![SemanticOperation::EditPagePath {
        page_id: ids.page_a,
        path: path("pages/Renamed.md"),
    }]);
    let deleted = tx(vec![SemanticOperation::DeletePage {
        page_id: ids.page_a,
    }]);
    let (renamed, deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(140, 140),
        renamed,
        author(141, 141),
        deleted,
    );
    let ab = apply_pair(ids, &baseline, renamed.clone(), deleted.clone());
    let ba = apply_pair(ids, &baseline, deleted, renamed);
    assert_eq!(
        ab.canonical_snapshot().unwrap(),
        ba.canonical_snapshot().unwrap()
    );

    let mut author_a = ids.engine();
    let mut author_b = ids.engine();
    author_a.stage_ready(baseline.clone());
    author_b.stage_ready(baseline.clone());
    let conflict_a = author_a
        .prepare_fixture_transaction(
            author(142, 142),
            &tx(vec![SemanticOperation::EditPagePath {
                page_id: ids.page_a,
                path: path("pages/Conflict.md"),
            }]),
        )
        .unwrap();
    let conflict_b = author_b
        .prepare_fixture_transaction(
            author(143, 143),
            &tx(vec![SemanticOperation::EditPagePath {
                page_id: ids.page_b,
                path: path("pages/Conflict.md"),
            }]),
        )
        .unwrap();
    let ab = apply_pair(
        ids,
        &baseline,
        ready(&archive, &conflict_a),
        ready(&archive, &conflict_b),
    );
    let ba = apply_pair(
        ids,
        &baseline,
        ready(&archive, &conflict_b),
        ready(&archive, &conflict_a),
    );
    assert!(matches!(
        ab.status().workspace(),
        WorkspaceStatus::Blocked(_)
    ));
    assert!(matches!(
        ba.status().workspace(),
        WorkspaceStatus::Blocked(_)
    ));
    assert_eq!(ab.fatal_evidence_handle(), ba.fatal_evidence_handle());
    assert_eq!(ab.portable_path_conflicts(), ba.portable_path_conflicts());
    let conflicts = ab.portable_path_conflicts().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].participants().len(), 2);
    assert_eq!(
        conflicts[0]
            .participants()
            .iter()
            .map(|participant| participant.page_id())
            .collect::<Vec<_>>(),
        vec![ids.page_a, ids.page_b]
    );
    assert!(matches!(
        ab.canonical_snapshot(),
        Err(EngineError::WorkspaceBlocked(_))
    ));
    assert!(matches!(
        ab.materialize_page(ids.page_a),
        Err(EngineError::WorkspaceBlocked(_))
    ));
}

#[test]
fn portable_aliases_quarantine_in_both_orders_but_compatibility_only_names_stay_distinct() {
    let aliases = [
        ("pages/Foo.md", "pages/foo.md"),
        ("pages/Café.md", "pages/Cafe\u{301}.md"),
        ("pages/Straße.md", "pages/STRASSE.md"),
        ("pages/Σίσυφος.md", "pages/σίσυφοσ.md"),
        ("pages/Kelvin.md", "pages/kelvin.md"),
    ];
    for (offset, (left_path, right_path)) in aliases.into_iter().enumerate() {
        let ids = Ids::new();
        let dir = TestDir::new(&format!("portable-alias-{offset}"));
        let archive = store(&dir, ids);
        let (_, baseline) = seed_engine(ids, &archive);
        let (left, right) = concurrent_ready(
            ids,
            &archive,
            &baseline,
            author(40_000 + offset as u128 * 2, 40_000 + offset as u64 * 2),
            tx(vec![SemanticOperation::EditPagePath {
                page_id: ids.page_a,
                path: path(left_path),
            }]),
            author(40_001 + offset as u128 * 2, 40_001 + offset as u64 * 2),
            tx(vec![SemanticOperation::EditPagePath {
                page_id: ids.page_b,
                path: path(right_path),
            }]),
        );
        let ab = apply_pair(ids, &baseline, left.clone(), right.clone());
        let ba = apply_pair(ids, &baseline, right, left);
        assert!(matches!(
            ab.status().workspace(),
            WorkspaceStatus::Blocked(_)
        ));
        assert_eq!(ab.fatal_evidence_handle(), ba.fatal_evidence_handle());
        assert_eq!(ab.portable_path_conflicts(), ba.portable_path_conflicts());
        let evidence = ab.portable_path_conflicts().unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(
            evidence[0].key_digest(),
            path(left_path).portable_key().digest()
        );
        assert_eq!(
            evidence[0]
                .participants()
                .iter()
                .map(|participant| participant.exact_path().as_str())
                .collect::<Vec<_>>(),
            vec![left_path, right_path]
        );
    }

    let ids = Ids::new();
    let dir = TestDir::new("portable-compatibility-distinct");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let (left, right) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(40_100, 40_100),
        tx(vec![SemanticOperation::EditPagePath {
            page_id: ids.page_a,
            path: path("pages/①.md"),
        }]),
        author(40_101, 40_101),
        tx(vec![SemanticOperation::EditPagePath {
            page_id: ids.page_b,
            path: path("pages/1.md"),
        }]),
    );
    let engine = apply_pair(ids, &baseline, left, right);
    assert!(matches!(
        engine.status().workspace(),
        WorkspaceStatus::Operational
    ));
    assert!(engine.portable_path_conflicts().unwrap().is_empty());
    let snapshot = engine.canonical_snapshot().unwrap();
    assert!(snapshot.path_conflicts.is_empty());
}

#[test]
fn concurrent_portable_alias_creates_quarantine_with_order_independent_evidence() {
    let ids = Ids::new();
    let dir = TestDir::new("portable-create-create");
    let archive = store(&dir, ids);
    let left = ids
        .engine()
        .prepare_fixture_transaction(
            author(40_150, 40_150),
            &tx(vec![SemanticOperation::CreatePage {
                page_id: ids.page_a,
                home_document_id: ids.home_a,
                name: crate::oplog::LogicalPageName::parse("Foo").unwrap(),
                path: path("pages/Foo.md"),
                kind: ManagedTextKind::Page,
            }]),
        )
        .unwrap();
    let right = ids
        .engine()
        .prepare_fixture_transaction(
            author(40_151, 40_151),
            &tx(vec![SemanticOperation::CreatePage {
                page_id: ids.page_b,
                home_document_id: ids.home_b,
                name: crate::oplog::LogicalPageName::parse("foo").unwrap(),
                path: path("pages/foo.md"),
                kind: ManagedTextKind::Page,
            }]),
        )
        .unwrap();
    let left = ready(&archive, &left);
    let right = ready(&archive, &right);
    let apply = |first: ValidatedBatch, second: ValidatedBatch| {
        let mut engine = ids.engine();
        assert!(matches!(
            engine.stage_ready(first).disposition,
            BatchDisposition::Accepted { .. }
        ));
        assert!(matches!(
            engine.stage_ready(second).disposition,
            BatchDisposition::Quarantined
        ));
        engine
    };
    let ab = apply(left.clone(), right.clone());
    let ba = apply(right, left);
    assert_eq!(ab.fatal_evidence_handle(), ba.fatal_evidence_handle());
    assert_eq!(ab.portable_path_conflicts(), ba.portable_path_conflicts());
    assert_eq!(
        ab.portable_path_conflicts().unwrap()[0]
            .participants()
            .len(),
        2
    );
}

#[test]
fn sequential_duplicates_reject_at_acceptance_and_atomic_swap_and_causal_reuse_succeed() {
    let ids = Ids::new();
    let dir = TestDir::new("portable-sequential-swap-reuse");
    let archive = store(&dir, ids);
    let (mut engine, _) = seed_engine(ids, &archive);

    assert!(matches!(
        engine.prepare_fixture_transaction(
            author(40_200, 40_200),
            &tx(vec![SemanticOperation::EditPagePath {
                page_id: ids.page_b,
                path: path("pages/a.md"),
            }]),
        ),
        Err(EngineError::InvalidTransaction(_))
    ));
    assert_eq!(
        engine.materialize_page(ids.page_b).unwrap().path.as_str(),
        "pages/B.md",
        "the locally refused duplicate must not mutate accepted state"
    );

    let swap = engine
        .prepare_fixture_transaction(
            author(40_201, 40_201),
            &tx(vec![
                SemanticOperation::EditPagePath {
                    page_id: ids.page_a,
                    path: path("pages/B.md"),
                },
                SemanticOperation::EditPagePath {
                    page_id: ids.page_b,
                    path: path("pages/A.md"),
                },
            ]),
        )
        .unwrap();
    assert!(matches!(
        engine.stage_ready(ready(&archive, &swap)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(
        engine.materialize_page(ids.page_a).unwrap().path.as_str(),
        "pages/B.md"
    );
    assert_eq!(
        engine.materialize_page(ids.page_b).unwrap().path.as_str(),
        "pages/A.md"
    );

    let release = engine
        .prepare_fixture_transaction(
            author(40_202, 40_202),
            &tx(vec![SemanticOperation::DeletePage {
                page_id: ids.page_b,
            }]),
        )
        .unwrap();
    assert!(matches!(
        engine.stage_ready(ready(&archive, &release)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let reuse = engine
        .prepare_fixture_transaction(
            author(40_203, 40_203),
            &tx(vec![SemanticOperation::EditPagePath {
                page_id: ids.page_c,
                path: path("pages/A.md"),
            }]),
        )
        .unwrap();
    assert!(matches!(
        engine.stage_ready(ready(&archive, &reuse)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(
        engine.materialize_page(ids.page_c).unwrap().path.as_str(),
        "pages/A.md"
    );
}

#[test]
fn inline_portable_path_root_advances_for_an_affected_only_rename() {
    let ids = Ids::new();
    let dir = TestDir::new("portable-index-auth");
    let archive_path = dir.path().join("archive");
    let writer = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let bootstrap = genesis(ids, &ids.engine());
    publish_fixture(&writer, &bootstrap);

    let reader = ObjectStore::open(&archive_path, ids.workspace).unwrap();
    let mut engine =
        ShardedHotEngine::with_clean_archive_store_for_test(reader, ids.lineage, ids.catalog);
    assert!(matches!(
        engine
            .stage_archive_batch(bootstrap.manifest().batch_id())
            .unwrap()
            .disposition(),
        BatchDisposition::Accepted { .. }
    ));
    let initial = engine.instrumentation();
    let rename = engine
        .prepare_fixture_transaction(
            author(40_300, 40_300),
            &tx(vec![SemanticOperation::EditPagePath {
                page_id: ids.page_a,
                path: path("pages/Only Affected.md"),
            }]),
        )
        .unwrap();
    publish_fixture(&writer, &rename);
    assert!(matches!(
        engine
            .stage_archive_batch(rename.manifest().batch_id())
            .unwrap()
            .disposition(),
        BatchDisposition::Accepted { .. }
    ));
    let after = engine.instrumentation();
    assert!(
        after
            .portable_path_index_reads
            .saturating_sub(initial.portable_path_index_reads)
            <= 32,
        "one rename must use bounded old/new portable-key point reads"
    );
    assert_ne!(
        engine.portable_path_index_root().unwrap(),
        crate::oplog::PortablePathIndexRoot::empty()
    );
}

#[test]
fn received_reuse_that_omits_the_release_frontier_is_rejected_before_visibility() {
    let ids = Ids::new();
    let dir = TestDir::new("portable-stale-reuse");
    let archive = store(&dir, ids);
    let (mut author_engine, baseline) = seed_engine(ids, &archive);
    let release = author_engine
        .prepare_fixture_transaction(
            author(40_400, 40_400),
            &tx(vec![SemanticOperation::EditPagePath {
                page_id: ids.page_a,
                path: path("pages/Released.md"),
            }]),
        )
        .unwrap();
    let release_ready = ready(&archive, &release);
    assert!(matches!(
        author_engine.stage_ready(release_ready.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let safe_reuse = author_engine
        .prepare_fixture_transaction(
            author(40_401, 40_401),
            &tx(vec![SemanticOperation::EditPagePath {
                page_id: ids.page_b,
                path: path("pages/A.md"),
            }]),
        )
        .unwrap();
    let stale = rebuild_with_compact_witness(
        &safe_reuse,
        release.manifest().dependency_frontier().clone(),
    );
    let stale_ready = ready(&archive, &stale);

    let mut receiver = ids.engine();
    assert!(matches!(
        receiver.stage_ready(baseline).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        receiver.stage_ready(release_ready).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let before_root = receiver.portable_path_index_root();
    assert!(matches!(
        receiver.stage_ready(stale_ready).disposition,
        BatchDisposition::Rejected { .. }
    ));
    assert_eq!(receiver.portable_path_index_root(), before_root);
    assert!(matches!(
        receiver.status().workspace(),
        WorkspaceStatus::Operational
    ));
    assert_eq!(
        receiver.materialize_page(ids.page_b).unwrap().path.as_str(),
        "pages/B.md"
    );
}

#[test]
fn causal_batch_waits_then_validates_at_declared_frontier_not_delivery_current() {
    let ids = Ids::new();
    let dir = TestDir::new("causal-wait");
    let archive = store(&dir, ids);
    let (mut author_engine, baseline) = seed_engine(ids, &archive);
    let moved = author_engine
        .prepare_fixture_transaction(
            author(150, 150),
            &tx(vec![SemanticOperation::MoveSubtree {
                root: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                from_page_id: ids.page_a,
                to_page_id: ids.page_b,
                parent: None,
                order: "m".into(),
            }]),
        )
        .unwrap();
    let moved_ready = ready(&archive, &moved);
    author_engine.stage_ready(moved_ready.clone());
    let dependent = author_engine
        .prepare_fixture_transaction(
            author(151, 150),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "dependent edit".into(),
            }]),
        )
        .unwrap();
    let dependent_ready = ready(&archive, &dependent);

    let mut concurrent_author = ids.engine();
    concurrent_author.stage_ready(baseline.clone());
    let concurrent = concurrent_author
        .prepare_fixture_transaction(
            author(152, 152),
            &tx(vec![SemanticOperation::EditPagePath {
                page_id: ids.page_c,
                path: path("pages/Concurrent.md"),
            }]),
        )
        .unwrap();
    let concurrent_ready = ready(&archive, &concurrent);

    let mut receiver = ids.engine();
    receiver.stage_ready(baseline);
    receiver.stage_ready(concurrent_ready);
    assert!(matches!(
        receiver.stage_ready(dependent_ready).disposition,
        BatchDisposition::IncompleteStaged { .. }
    ));
    let outcome = receiver.stage_ready(moved_ready);
    assert!(matches!(
        outcome.disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(
        outcome
            .newly_accepted()
            .iter()
            .map(|accepted| accepted.batch_id)
            .collect::<Vec<_>>(),
        vec![BatchId::from_uuid(uuid(150)), BatchId::from_uuid(uuid(151)),]
    );
    assert_eq!(
        receiver.materialize_page(ids.page_b).unwrap().blocks[0].content,
        "dependent edit"
    );
}

#[test]
fn duplicate_of_still_staged_batch_truthfully_repeats_missing_dependencies() {
    let ids = Ids::new();
    let dir = TestDir::new("duplicate-staged");
    let archive = store(&dir, ids);
    let (mut author_engine, baseline) = seed_engine(ids, &archive);
    let dependency = author_engine
        .prepare_fixture_transaction(
            author(170, 170),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "dependency".into(),
            }]),
        )
        .unwrap();
    let dependency_ready = ready(&archive, &dependency);
    author_engine.stage_ready(dependency_ready);
    let dependent = author_engine
        .prepare_fixture_transaction(
            author(171, 171),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "dependent".into(),
            }]),
        )
        .unwrap();
    let dependent_ready = ready(&archive, &dependent);

    let mut receiver = ids.engine();
    receiver.stage_ready(baseline);
    let expected_missing = vec![BatchId::from_uuid(uuid(170))];
    for _ in 0..2 {
        assert!(matches!(
            receiver.stage_ready(dependent_ready.clone()).disposition,
            BatchDisposition::IncompleteStaged {
                missing_objects: 0,
                ref missing_dependencies,
                ..
            } if *missing_dependencies == expected_missing
        ));
    }
}

#[test]
fn crdt_update_requires_exact_declared_base_but_not_delivery_current() {
    let ids = Ids::new();
    let dir = TestDir::new("exact-causal-base");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);

    let mut advanced = ids.engine();
    advanced.stage_ready(baseline.clone());
    let intermediate = advanced
        .prepare_fixture_transaction(
            author(180, 180),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "intermediate".into(),
            }]),
        )
        .unwrap();
    let intermediate_ready = ready(&archive, &intermediate);
    advanced.stage_ready(intermediate_ready.clone());
    let based_on_advanced = advanced
        .prepare_fixture_transaction(
            author(181, 181),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "advanced update".into(),
            }]),
        )
        .unwrap();

    let mut baseline_author = ids.engine();
    baseline_author.stage_ready(baseline.clone());
    let baseline_template = baseline_author
        .prepare_fixture_transaction(
            author(181, 181),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "baseline template".into(),
            }]),
        )
        .unwrap();
    let under_declared = rebuild_with_compact_witness(
        &based_on_advanced,
        baseline_template.manifest().dependency_frontier().clone(),
    );
    let mut receiver = ids.engine();
    receiver.stage_ready(baseline.clone());
    assert!(matches!(
        receiver
            .stage_ready(ready(&archive, &under_declared))
            .disposition,
        BatchDisposition::Rejected {
            error: EngineError::CrdtUpdateBaseMismatch(_),
            ..
        }
    ));

    let based_on_baseline = baseline_author
        .prepare_fixture_transaction(
            author(182, 182),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "baseline update".into(),
            }]),
        )
        .unwrap();
    let advanced_template = advanced
        .prepare_fixture_transaction(
            author(182, 182),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "advanced template".into(),
            }]),
        )
        .unwrap();
    let over_declared = rebuild_with_compact_witness(
        &based_on_baseline,
        advanced_template.manifest().dependency_frontier().clone(),
    );
    let mut receiver = ids.engine();
    receiver.stage_ready(baseline.clone());
    receiver.stage_ready(intermediate_ready.clone());
    assert!(matches!(
        receiver
            .stage_ready(ready(&archive, &over_declared))
            .disposition,
        BatchDisposition::Rejected {
            error: EngineError::CrdtUpdateBaseMismatch(_),
            ..
        }
    ));

    let concurrent = baseline_author
        .prepare_fixture_transaction(
            author(183, 183),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "delivery-current concurrency".into(),
            }]),
        )
        .unwrap();
    let target = baseline_author
        .prepare_fixture_transaction(
            author(184, 184),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "target wins".into(),
            }]),
        )
        .unwrap();
    let mut receiver = ids.engine();
    receiver.stage_ready(baseline);
    receiver.stage_ready(ready(&archive, &concurrent));
    assert!(matches!(
        receiver.stage_ready(ready(&archive, &target)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(!receiver.materialize_page(ids.page_a).unwrap().blocks[0]
        .content
        .is_empty());
}

#[test]
fn compact_frontier_rejects_nonmaximal_heads_and_inexact_peer_counters() {
    let ids = Ids::new();
    let dir = TestDir::new("compact-frontier-exactness");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);

    let mut author_engine = ids.engine();
    author_engine.stage_ready(baseline.clone());
    let intermediate = author_engine
        .prepare_fixture_transaction(
            author(185, 185),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "intermediate".into(),
            }]),
        )
        .unwrap();
    let intermediate_ready = ready(&archive, &intermediate);
    author_engine.stage_ready(intermediate_ready.clone());
    let descendant = author_engine
        .prepare_fixture_transaction(
            author(186, 186),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "descendant".into(),
            }]),
        )
        .unwrap();
    let exact = &descendant.manifest().dependency_frontier().documents()[0];

    let nonmaximal = DocumentDependencies::new(
        exact.document_id(),
        exact.peer_counters().to_vec(),
        vec![
            baseline.manifest().batch_id(),
            intermediate.manifest().batch_id(),
        ],
    )
    .unwrap();
    let nonmaximal =
        rebuild_with_compact_witness(&descendant, FrontierV2::new(vec![nonmaximal]).unwrap());
    let mut receiver = ids.engine();
    receiver.stage_ready(baseline.clone());
    receiver.stage_ready(intermediate_ready.clone());
    assert!(matches!(
        receiver
            .stage_ready(ready(&archive, &nonmaximal))
            .disposition,
        BatchDisposition::Rejected {
            error: EngineError::NonMaximalDependencyHead {
                redundant,
                descendant,
            },
        } if redundant == baseline.manifest().batch_id()
            && descendant == intermediate.manifest().batch_id()
    ));

    let mut counters = exact.peer_counters().to_vec();
    let first = counters[0];
    counters[0] = CrdtPeerCounter::new(first.peer_id(), first.max_counter() + 1);
    let inexact = DocumentDependencies::new(
        exact.document_id(),
        counters,
        exact.direct_dependency_heads().to_vec(),
    )
    .unwrap();
    let inexact =
        rebuild_with_compact_witness(&descendant, FrontierV2::new(vec![inexact]).unwrap());
    let inexact_dir = TestDir::new("compact-frontier-inexact-counter");
    let inexact_archive = store(&inexact_dir, ids);
    let mut receiver = ids.engine();
    receiver.stage_ready(baseline);
    receiver.stage_ready(intermediate_ready);
    assert!(matches!(
        receiver
            .stage_ready(ready(&inexact_archive, &inexact))
            .disposition,
        BatchDisposition::Rejected {
            error: EngineError::FrontierVectorMismatch(document_id),
        } if document_id == DocumentKey::Entity(ids.block_home_a())
    ));
}

#[test]
fn compact_frontier_rejects_unrelated_maximal_document_head() {
    let ids = Ids::new();
    let dir = TestDir::new("compact-frontier-unrelated-maximal-head");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);

    let mut unrelated_author = ids.engine();
    unrelated_author.stage_ready(baseline.clone());
    let unrelated = unrelated_author
        .prepare_fixture_transaction(
            author(187, 187),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_c,
                    home_document_id: test_block_home(ids.block_c),
                },
                content: "unrelated accepted head".into(),
            }]),
        )
        .unwrap();
    let unrelated_ready = ready(&archive, &unrelated);

    let mut target_author = ids.engine();
    target_author.stage_ready(baseline.clone());
    let target = target_author
        .prepare_fixture_transaction(
            author(188, 188),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                content: "target edit".into(),
            }]),
        )
        .unwrap();
    let exact = &target.manifest().dependency_frontier().documents()[0];
    assert_eq!(exact.document_id(), DocumentKey::Entity(ids.block_home_a()));
    let smuggled = DocumentDependencies::new(
        DocumentKey::Entity(ids.block_home_a()),
        exact.peer_counters().to_vec(),
        vec![unrelated.manifest().batch_id()],
    )
    .unwrap();
    let smuggled = rebuild_with_compact_witness(&target, FrontierV2::new(vec![smuggled]).unwrap());

    let mut receiver = ids.engine();
    receiver.stage_ready(baseline);
    receiver.stage_ready(unrelated_ready);
    assert!(matches!(
        receiver
            .stage_ready(ready(&archive, &smuggled))
            .disposition,
        BatchDisposition::Rejected {
            error: EngineError::InexactDocumentDependencyHeads { document_id },
        } if document_id == DocumentKey::Entity(ids.block_home_a())
    ));
}

#[test]
fn randomized_replica_delivery_orders_converge_and_duplicates_are_noops() {
    let ids = Ids::new();
    let dir = TestDir::new("random-orders");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let operations = [
        tx(vec![SemanticOperation::EditPagePath {
            page_id: ids.page_b,
            path: path("pages/B2.md"),
        }]),
        tx(vec![SemanticOperation::EditBlockContent {
            block: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(ids.block_a),
            },
            content: "randomized concurrent edit".into(),
        }]),
        tx(vec![SemanticOperation::MoveSubtree {
            root: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(ids.block_a),
            },
            from_page_id: ids.page_a,
            to_page_id: ids.page_c,
            parent: None,
            order: "z".into(),
        }]),
    ];
    let mut batches = Vec::new();
    for (index, operation) in operations.into_iter().enumerate() {
        let mut author_engine = ids.engine();
        author_engine.stage_ready(baseline.clone());
        let prepared = author_engine
            .prepare_fixture_transaction(
                author(160 + index as u128, 160 + index as u64),
                &operation,
            )
            .unwrap();
        batches.push(ready(&archive, &prepared));
    }
    let mut expected = None;
    for seed in 1_u64..=64 {
        let mut order = [0_usize, 1, 2];
        let mut state = seed;
        for index in (1..order.len()).rev() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            order.swap(index, state as usize % (index + 1));
        }
        let mut replica = ids.engine();
        replica.stage_ready(baseline.clone());
        for index in order {
            replica.stage_ready(batches[index].clone());
        }
        assert!(matches!(
            replica.stage_ready(batches[0].clone()).disposition,
            BatchDisposition::DuplicateAccepted { .. }
        ));
        let snapshot = replica.canonical_snapshot().unwrap();
        if let Some(expected) = &expected {
            assert_eq!(&snapshot, expected, "seed {seed}");
        } else {
            expected = Some(snapshot);
        }
    }
}

#[test]
fn semantic_encoding_is_canonical_and_bounded() {
    let effect = SemanticEffect::new(Vec::new(), Vec::new(), Vec::new()).unwrap();
    let bytes = effect.encode().unwrap();
    assert_eq!(SemanticEffect::decode(&bytes).unwrap(), effect);
    let mut noncanonical = bytes;
    noncanonical.push(b' ');
    assert!(SemanticEffect::decode(&noncanonical).is_err());
    assert_ne!(ContentDigest::of(b"a"), ContentDigest::of(b"b"));
    let _ = CrdtPeerCounter::new(CrdtPeerId::from_u64(1), 0);
}

/// One archive-backed engine whose catalog holds `pages` live pages, warmed so
/// the next local author draft is an ordinary warm edit.
#[test]
fn restore_subtree_resurrects_a_tombstoned_block_with_the_concurrent_edit_text() {
    let ids = Ids::new();
    let dir = TestDir::new("restore-after-edit-delete");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let edited = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "offline edit racing deletion".into(),
    }]);
    let deleted = tx(vec![SemanticOperation::DeleteSubtree {
        root_block_id: ids.block_a,
        page_id: ids.page_a,
    }]);
    let (edited, deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(50_100, 501),
        edited,
        author(50_200, 502),
        deleted,
    );
    let merged = apply_pair(ids, &baseline, edited.clone(), deleted.clone());
    assert!(
        merged
            .materialize_page(ids.page_a)
            .unwrap()
            .blocks
            .is_empty(),
        "the unresolved merge tombstones the edited block"
    );

    let restore = tx(vec![SemanticOperation::RestoreSubtree {
        page_id: ids.page_a,
        blocks: vec![BlockRestore {
            block: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(ids.block_a),
            },
            claim: MembershipClaim {
                home_document_id: test_block_home(ids.block_a),
                parent: None,
                order: "a".into(),
            },
        }],
    }]);
    let restore_prepared = {
        let mut author_engine = ids.engine();
        author_engine.stage_ready(baseline.clone());
        author_engine.stage_ready(edited.clone());
        author_engine.stage_ready(deleted.clone());
        author_engine
            .prepare_fixture_transaction(author(50_300, 503), &restore)
            .unwrap()
    };
    let restore = ready(&archive, &restore_prepared);

    let ab = {
        let mut engine = apply_pair(ids, &baseline, edited.clone(), deleted.clone());
        assert!(!matches!(
            engine.stage_ready(restore.clone()).disposition,
            BatchDisposition::Rejected { .. }
        ));
        engine
    };
    let ba = {
        let mut engine = apply_pair(ids, &baseline, deleted, edited);
        assert!(!matches!(
            engine.stage_ready(restore).disposition,
            BatchDisposition::Rejected { .. }
        ));
        engine
    };
    assert_eq!(
        ab.canonical_snapshot().unwrap(),
        ba.canonical_snapshot().unwrap()
    );
    let page = ab.materialize_page(ids.page_a).unwrap();
    assert_eq!(page.blocks.len(), 1);
    assert_eq!(page.blocks[0].content, "offline edit racing deletion");
}

#[test]
fn independently_authored_equal_restores_converge_to_one_visible_block() {
    let ids = Ids::new();
    let dir = TestDir::new("restore-double-author");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let deleted = {
        let mut author_engine = ids.engine();
        author_engine.stage_ready(baseline.clone());
        let prepared = author_engine
            .prepare_fixture_transaction(
                author(51_100, 511),
                &tx(vec![SemanticOperation::DeleteSubtree {
                    root_block_id: ids.block_a,
                    page_id: ids.page_a,
                }]),
            )
            .unwrap();
        ready(&archive, &prepared)
    };
    let restore_operations = || {
        tx(vec![SemanticOperation::RestoreSubtree {
            page_id: ids.page_a,
            blocks: vec![BlockRestore {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: test_block_home(ids.block_a),
                },
                claim: MembershipClaim {
                    home_document_id: test_block_home(ids.block_a),
                    parent: None,
                    order: "a".into(),
                },
            }],
        }])
    };
    let (left, right) = concurrent_ready_from(
        ids,
        &archive,
        &[baseline.clone(), deleted.clone()],
        author(51_200, 512),
        restore_operations(),
        author(51_300, 513),
        restore_operations(),
    );
    let ab = apply_pair_from(
        ids,
        &[baseline.clone(), deleted.clone()],
        left.clone(),
        right.clone(),
    );
    let ba = apply_pair_from(ids, &[baseline, deleted], right, left);
    assert_eq!(
        ab.canonical_snapshot().unwrap(),
        ba.canonical_snapshot().unwrap()
    );
    let page = ab.materialize_page(ids.page_a).unwrap();
    assert_eq!(page.blocks.len(), 1);
    assert_eq!(page.blocks[0].content, "home A content");
}

#[test]
fn restore_subtree_reasserts_a_move_over_a_concurrent_delete() {
    let ids = Ids::new();
    let dir = TestDir::new("restore-move-delete");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let moved = tx(vec![SemanticOperation::MoveSubtree {
        root: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        from_page_id: ids.page_a,
        to_page_id: ids.page_b,
        parent: None,
        order: "z".into(),
    }]);
    let deleted = tx(vec![SemanticOperation::DeleteSubtree {
        root_block_id: ids.block_a,
        page_id: ids.page_a,
    }]);
    let (moved, deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(52_100, 521),
        moved,
        author(52_200, 522),
        deleted,
    );
    let restore = tx(vec![SemanticOperation::RestoreSubtree {
        page_id: ids.page_b,
        blocks: vec![BlockRestore {
            block: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(ids.block_a),
            },
            claim: MembershipClaim {
                home_document_id: test_block_home(ids.block_a),
                parent: None,
                order: "z".into(),
            },
        }],
    }]);
    let restore_prepared = {
        let mut author_engine = ids.engine();
        author_engine.stage_ready(baseline.clone());
        author_engine.stage_ready(moved.clone());
        author_engine.stage_ready(deleted.clone());
        author_engine
            .prepare_fixture_transaction(author(52_300, 523), &restore)
            .unwrap()
    };
    let restore = ready(&archive, &restore_prepared);
    let ab = {
        let mut engine = apply_pair(ids, &baseline, moved.clone(), deleted.clone());
        let disposition = engine.stage_ready(restore.clone()).disposition;
        assert!(
            !matches!(disposition, BatchDisposition::Rejected { .. }),
            "restore rejected: {disposition:?}"
        );
        engine
    };
    let ba = {
        let mut engine = apply_pair(ids, &baseline, deleted, moved);
        let disposition = engine.stage_ready(restore).disposition;
        assert!(
            !matches!(disposition, BatchDisposition::Rejected { .. }),
            "restore rejected: {disposition:?}"
        );
        engine
    };
    assert_eq!(
        ab.canonical_snapshot().unwrap(),
        ba.canonical_snapshot().unwrap()
    );
    assert!(ab.materialize_page(ids.page_a).unwrap().blocks.is_empty());
    let page_b = ab.materialize_page(ids.page_b).unwrap();
    assert_eq!(page_b.blocks.len(), 1);
    assert_eq!(page_b.blocks[0].content, "home A content");
}

#[test]
fn conflict_intents_detect_edit_delete_and_move_delete_races() {
    let ids = Ids::new();
    let dir = TestDir::new("intents-delete-races");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let edited = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "offline edit racing deletion".into(),
    }]);
    let deleted = tx(vec![SemanticOperation::DeleteSubtree {
        root_block_id: ids.block_a,
        page_id: ids.page_a,
    }]);
    let (edited, deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(54_100, 541),
        edited,
        author(54_200, 542),
        deleted,
    );
    for (first, second) in [
        (edited.clone(), deleted.clone()),
        (deleted.clone(), edited.clone()),
    ] {
        let engine = apply_pair(ids, &baseline, first, second.clone());
        let intents = engine
            .conflict_resolution_intents(second.manifest().batch_id())
            .unwrap();
        assert_eq!(intents.len(), 1, "one restore per pair: {intents:?}");
        match &intents[0] {
            ConflictResolutionIntent::RestoreEdited {
                page_id,
                block,
                claim,
                pair,
            } => {
                assert_eq!(*page_id, ids.page_a);
                assert_eq!(block.block_id, ids.block_a);
                assert_eq!(claim.parent, None);
                assert_eq!(claim.order, "a");
                assert_eq!(
                    (pair.min_batch, pair.max_batch),
                    (
                        edited
                            .manifest()
                            .batch_id()
                            .min(deleted.manifest().batch_id()),
                        edited
                            .manifest()
                            .batch_id()
                            .max(deleted.manifest().batch_id()),
                    )
                );
            }
            other => panic!("expected RestoreEdited, found {other:?}"),
        }
        // The first of the pair is linear on this device; no intents for it.
        let engine_first_id = if second.manifest().batch_id() == edited.manifest().batch_id() {
            deleted.manifest().batch_id()
        } else {
            edited.manifest().batch_id()
        };
        assert!(engine
            .conflict_resolution_intents(engine_first_id)
            .unwrap()
            .iter()
            .all(|intent| matches!(intent, ConflictResolutionIntent::RestoreEdited { .. })));
    }

    let moved = tx(vec![SemanticOperation::MoveSubtree {
        root: BlockLocation {
            block_id: ids.block_c,
            home_document_id: test_block_home(ids.block_c),
        },
        from_page_id: ids.page_c,
        to_page_id: ids.page_b,
        parent: None,
        order: "z".into(),
    }]);
    let subtree_deleted = tx(vec![SemanticOperation::DeleteSubtree {
        root_block_id: ids.block_c,
        page_id: ids.page_c,
    }]);
    let (moved, subtree_deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(54_300, 543),
        moved,
        author(54_400, 544),
        subtree_deleted,
    );
    let engine = apply_pair(ids, &baseline, moved.clone(), subtree_deleted.clone());
    let intents = engine
        .conflict_resolution_intents(subtree_deleted.manifest().batch_id())
        .unwrap();
    let restore_moved = intents.iter().find_map(|intent| match intent {
        ConflictResolutionIntent::RestoreMoved {
            page_id,
            block,
            claim,
            ..
        } => Some((*page_id, block.block_id, claim.clone())),
        _ => None,
    });
    if engine
        .materialize_page(ids.page_b)
        .unwrap()
        .blocks
        .is_empty()
    {
        // The tombstone won the register race: move-wins needs the restore.
        let (page_id, block_id, claim) =
            restore_moved.expect("tombstone-winning race yields a RestoreMoved intent");
        assert_eq!(page_id, ids.page_b);
        assert_eq!(block_id, ids.block_c);
        assert_eq!(claim.order, "z");
    } else {
        // The move already won: nothing to re-assert.
        assert!(
            restore_moved.is_none(),
            "move won yet a restore was derived"
        );
    }
}

#[test]
fn projection_supersession_distinguishes_a_linear_prefix_from_a_later_concurrent_merge() {
    let ids = Ids::new();
    let dir = TestDir::new("projection-supersession-linearity");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let edited = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "offline edit racing deletion".into(),
    }]);
    let deleted = tx(vec![SemanticOperation::DeleteSubtree {
        root_block_id: ids.block_a,
        page_id: ids.page_a,
    }]);
    let (edited, deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(54_500, 545),
        edited,
        author(54_600, 546),
        deleted,
    );
    let mut engine = ids.engine();
    engine.stage_ready(baseline);
    engine.stage_ready(edited.clone());
    assert!(
        !engine
            .accepted_batch_projection_is_superseded(edited.manifest().batch_id())
            .unwrap(),
        "a purely linear accepted prefix must retain strict recorded-render validation"
    );
    engine.stage_ready(deleted.clone());
    assert!(
        engine
            .accepted_batch_projection_is_superseded(edited.manifest().batch_id())
            .unwrap(),
        "a later concurrent merge must supersede an earlier linear render"
    );
    assert!(
        engine
            .accepted_batch_projection_is_superseded(deleted.manifest().batch_id())
            .unwrap(),
        "the concurrently admitted batch must classify as superseded"
    );
}

#[test]
fn nested_concurrent_deletions_still_derive_keep_both_when_the_merge_lands_on_one_side() {
    // Audit 4, D1: one deletion subsumes the other, so the CRDT merge equals
    // the wider deletion's after-state byte-for-byte. That equality is NOT
    // resolution evidence — the narrower author's text must still surface as
    // a keep-both sibling.
    let ids = Ids::new();
    let dir = TestDir::new("intents-nested-deletions");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let wider = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "home".into(),
    }]);
    let narrower = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "home content".into(),
    }]);
    let (wider, narrower) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(56_100, 561),
        wider,
        author(56_200, 562),
        narrower,
    );
    let engine = apply_pair(ids, &baseline, wider.clone(), narrower.clone());
    assert_eq!(
        engine.materialize_page(ids.page_a).unwrap().blocks[0].content,
        "home",
        "the union of nested deletions lands exactly on the wider deletion"
    );
    let mut intents = engine
        .conflict_resolution_intents(wider.manifest().batch_id())
        .unwrap();
    intents.extend(
        engine
            .conflict_resolution_intents(narrower.manifest().batch_id())
            .unwrap(),
    );
    let keep_both: Vec<_> = intents
        .iter()
        .filter_map(|intent| match intent {
            ConflictResolutionIntent::KeepBothTexts {
                keep_text,
                sibling_text,
                ..
            } => Some((keep_text.clone(), sibling_text.clone())),
            _ => None,
        })
        .collect();
    assert!(
        !keep_both.is_empty(),
        "a merge landing on one authored version must still derive keep-both: {intents:?}"
    );
    let min_is_wider = wider.manifest().batch_id() <= narrower.manifest().batch_id();
    let expected = if min_is_wider {
        ("home".to_owned(), "home content".to_owned())
    } else {
        ("home content".to_owned(), "home".to_owned())
    };
    assert!(
        keep_both.iter().all(|pair| *pair == expected),
        "keep-both texts follow batch-id order: {keep_both:?}"
    );
}

#[test]
fn a_post_race_redelete_settles_an_edit_delete_pair_without_resurrection() {
    // Audit 4, finding 3 companion: the conflict queue is reseeded from
    // accepted non-linear batches at reopen, so a deliberate re-delete that
    // causally descends from both pair members must suppress re-derivation —
    // otherwise reseeding would resurrect content the user re-deleted.
    let ids = Ids::new();
    let dir = TestDir::new("intents-redelete-settles");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let edited = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "edit racing the delete".into(),
    }]);
    let deleted = tx(vec![SemanticOperation::DeleteSubtree {
        root_block_id: ids.block_a,
        page_id: ids.page_a,
    }]);
    let (edited, deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(56_300, 563),
        edited,
        author(56_400, 564),
        deleted,
    );
    let mut engine = apply_pair(ids, &baseline, edited.clone(), deleted.clone());
    assert!(
        !engine
            .conflict_resolution_intents(deleted.manifest().batch_id())
            .unwrap()
            .is_empty(),
        "the unresolved race owes a restore"
    );
    // Restore, then re-delete — both authored on top of the merged history.
    let restore = tx(vec![SemanticOperation::RestoreSubtree {
        page_id: ids.page_a,
        blocks: vec![BlockRestore {
            block: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(ids.block_a),
            },
            claim: MembershipClaim {
                home_document_id: test_block_home(ids.block_a),
                parent: None,
                order: "a".into(),
            },
        }],
    }]);
    let restore = {
        let mut author_engine = ids.engine();
        author_engine.stage_ready(baseline.clone());
        author_engine.stage_ready(edited.clone());
        author_engine.stage_ready(deleted.clone());
        let prepared = author_engine
            .prepare_fixture_transaction(author(56_500, 565), &restore)
            .unwrap();
        ready(&archive, &prepared)
    };
    assert!(!matches!(
        engine.stage_ready(restore.clone()).disposition,
        BatchDisposition::Rejected { .. }
    ));
    let redelete = tx(vec![SemanticOperation::DeleteSubtree {
        root_block_id: ids.block_a,
        page_id: ids.page_a,
    }]);
    let redelete = {
        let mut author_engine = ids.engine();
        author_engine.stage_ready(baseline.clone());
        author_engine.stage_ready(edited.clone());
        author_engine.stage_ready(deleted.clone());
        author_engine.stage_ready(restore.clone());
        let prepared = author_engine
            .prepare_fixture_transaction(author(56_600, 566), &redelete)
            .unwrap();
        ready(&archive, &prepared)
    };
    assert!(!matches!(
        engine.stage_ready(redelete).disposition,
        BatchDisposition::Rejected { .. }
    ));
    assert!(
        engine
            .materialize_page(ids.page_a)
            .unwrap()
            .blocks
            .is_empty(),
        "the re-delete holds"
    );
    for batch in [edited.manifest().batch_id(), deleted.manifest().batch_id()] {
        assert!(
            engine
                .conflict_resolution_intents(batch)
                .unwrap()
                .is_empty(),
            "a settled pair must not derive again after reseeding"
        );
    }
}

#[test]
fn conflict_intents_classify_text_overlap_and_stay_silent_on_disjoint_edits() {
    let ids = Ids::new();
    let dir = TestDir::new("intents-text-races");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    // Overlap: both replace the whole content.
    let first = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "first offline text".into(),
    }]);
    let second = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "second offline text".into(),
    }]);
    let (first, second) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(55_100, 551),
        first,
        author(55_200, 552),
        second,
    );
    let engine = apply_pair(ids, &baseline, first.clone(), second.clone());
    let intents = engine
        .conflict_resolution_intents(second.manifest().batch_id())
        .unwrap();
    assert_eq!(intents.len(), 1, "{intents:?}");
    match &intents[0] {
        ConflictResolutionIntent::KeepBothTexts {
            page_id,
            block,
            keep_text,
            sibling_text,
            merged_text,
            pair,
            ..
        } => {
            assert_eq!(*page_id, ids.page_a);
            assert_eq!(block.block_id, ids.block_a);
            let min_is_first = first.manifest().batch_id() <= second.manifest().batch_id();
            let (expected_keep, expected_sibling) = if min_is_first {
                ("first offline text", "second offline text")
            } else {
                ("second offline text", "first offline text")
            };
            assert_eq!(keep_text, expected_keep);
            assert_eq!(sibling_text, expected_sibling);
            assert_ne!(merged_text, keep_text);
            assert_ne!(merged_text, sibling_text);
            assert!(pair.min_batch < pair.max_batch);
        }
        other => panic!("expected KeepBothTexts, found {other:?}"),
    }

    // Once a keep-both resolution rewrites the block to one authored version,
    // re-deriving for the same pair must stay silent — otherwise every
    // re-check would author duplicate sibling blocks forever.
    let min_is_first = first.manifest().batch_id() <= second.manifest().batch_id();
    let keep = if min_is_first {
        "first offline text"
    } else {
        "second offline text"
    };
    let resolution = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: keep.into(),
    }]);
    let resolution = {
        let mut author_engine = ids.engine();
        author_engine.stage_ready(baseline.clone());
        author_engine.stage_ready(first.clone());
        author_engine.stage_ready(second.clone());
        let prepared = author_engine
            .prepare_fixture_transaction(author(55_500, 555), &resolution)
            .unwrap();
        ready(&archive, &prepared)
    };
    let mut engine = engine;
    assert!(!matches!(
        engine.stage_ready(resolution).disposition,
        BatchDisposition::Rejected { .. }
    ));
    assert_eq!(
        engine.materialize_page(ids.page_a).unwrap().blocks[0].content,
        keep
    );
    assert!(
        engine
            .conflict_resolution_intents(second.manifest().batch_id())
            .unwrap()
            .is_empty(),
        "a resolved keep-both pair must not derive again"
    );

    // Disjoint regions on block C: the CRDT union is faithful, no intent.
    let prefix = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_c,
            home_document_id: test_block_home(ids.block_c),
        },
        content: "UNRELATED content".into(),
    }]);
    let suffix = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_c,
            home_document_id: test_block_home(ids.block_c),
        },
        content: "unrelated CONTENT".into(),
    }]);
    let (prefix, suffix) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(55_300, 553),
        prefix,
        author(55_400, 554),
        suffix,
    );
    let engine = apply_pair(ids, &baseline, prefix, suffix.clone());
    assert_eq!(
        engine.materialize_page(ids.page_c).unwrap().blocks[0].content,
        "UNRELATED CONTENT"
    );
    assert!(engine
        .conflict_resolution_intents(suffix.manifest().batch_id())
        .unwrap()
        .is_empty());
}

fn a3_conflict_history_load_samples(history_size: usize) -> (usize, Vec<usize>) {
    let ids = Ids::new();
    let dir = TestDir::new(&format!("a3-conflict-history-{history_size}"));
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let mut author_engine = ids.engine();
    author_engine.stage_ready(baseline.clone());
    let mut replay_batches = Vec::with_capacity(history_size);

    for index in 0..history_size {
        let transaction = tx(vec![SemanticOperation::EditBlockContent {
            block: BlockLocation {
                block_id: ids.block_c,
                home_document_id: test_block_home(ids.block_c),
            },
            content: format!("unrelated history {index}"),
        }]);
        let unique = 0xa3_0000_u64 + index as u64;
        let prepared = author_engine
            .prepare_fixture_transaction(author(unique as u128, unique), &transaction)
            .unwrap();
        let batch = ready(&archive, &prepared);
        assert!(matches!(
            author_engine.stage_ready(batch.clone()).disposition,
            BatchDisposition::Accepted { .. }
        ));
        replay_batches.push(batch);
    }

    // Rebuild a fresh run-local index while the retained linear history is
    // delivered newest-first. The ready queue drains it only when the missing
    // prefix arrives, exercising replay and out-of-order admission together.
    let mut evaluation_engine = ids.engine();
    evaluation_engine.stage_ready(baseline.clone());
    for batch in replay_batches.into_iter().rev() {
        assert!(!matches!(
            evaluation_engine.stage_ready(batch).disposition,
            BatchDisposition::Rejected { .. }
        ));
    }

    let edited = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "A3 offline edit".into(),
    }]);
    let deleted = tx(vec![SemanticOperation::DeleteSubtree {
        root_block_id: ids.block_a,
        page_id: ids.page_a,
    }]);
    let unique = 0xa3_f000_u64;
    let (edited, deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(unique as u128, unique),
        edited,
        author((unique + 1) as u128, unique + 1),
        deleted,
    );
    assert!(matches!(
        evaluation_engine.stage_ready(edited).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        evaluation_engine.stage_ready(deleted.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    // Model the checkpoint-open boundary before the first evaluation. That
    // once-per-open O(N) rebuild is deliberately visible in the same counter;
    // the following steady-state samples remain pair-bounded.
    evaluation_engine.drop_conflict_history_index_for_test();
    let rebuild_loads = {
        assert!(!evaluation_engine
            .conflict_resolution_intents(deleted.manifest().batch_id())
            .unwrap()
            .is_empty());
        evaluation_engine
            .instrumentation()
            .conflict_resolution_history_loads
    };
    let steady_state = (0..3)
        .map(|_| {
            assert!(!evaluation_engine
                .conflict_resolution_intents(deleted.manifest().batch_id())
                .unwrap()
                .is_empty());
            evaluation_engine
                .instrumentation()
                .conflict_resolution_history_loads
        })
        .collect();
    (rebuild_loads, steady_state)
}

#[test]
#[ignore = "harvest A3 scale gate: 50/400/800 replayed histories, medians of three"]
fn conflict_resolution_history_loads_are_bounded_by_unresolved_pairs() {
    const UNRESOLVED_PAIRS: usize = 1;
    const LOAD_BOUND: usize = 8 * UNRESOLVED_PAIRS + 64;
    let mut medians = Vec::new();
    for history_size in [50_usize, 400, 800] {
        let (rebuild_loads, mut samples) = a3_conflict_history_load_samples(history_size);
        assert!(
            rebuild_loads >= history_size,
            "I-15: the once-per-open conflict-index rebuild must remain visible to the A3 counter; imitate conflict_backlog_reseed_does_not_rebuild_the_conflict_history_index"
        );
        samples.sort_unstable();
        let median = samples[1];
        eprintln!(
            "A3 history={history_size} unresolved={UNRESOLVED_PAIRS} rebuild_loads={rebuild_loads} steady_samples={samples:?} median={median} bound={LOAD_BOUND}"
        );
        medians.push((history_size, median));
    }
    for (history_size, median) in medians {
        assert!(
            median <= LOAD_BOUND,
            "I-14: conflict evaluation loaded {median} accepted batches at history {history_size}; bound {LOAD_BOUND}. Evaluation cost must scale with unresolved pairs, not history; imitate oplog/conflict_history.rs"
        );
    }
}

#[test]
fn conflict_resolution_is_invariant_to_concurrent_batch_delivery_order() {
    let ids = Ids::new();
    let dir = TestDir::new("a3-conflict-permutation");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let edited = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "permuted concurrent edit".into(),
    }]);
    let deleted = tx(vec![SemanticOperation::DeleteSubtree {
        root_block_id: ids.block_a,
        page_id: ids.page_a,
    }]);
    let (edited, deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(0xa3_f300, 0xa3_f300),
        edited,
        author(0xa3_f301, 0xa3_f301),
        deleted,
    );
    let child = {
        let mut author_engine = ids.engine();
        author_engine.stage_ready(baseline.clone());
        author_engine.stage_ready(edited.clone());
        let transaction = tx(vec![SemanticOperation::EditBlockContent {
            block: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(ids.block_a),
            },
            content: "causal child of the permuted edit".into(),
        }]);
        let prepared = author_engine
            .prepare_fixture_transaction(author(0xa3_f302, 0xa3_f302), &transaction)
            .unwrap();
        ready(&archive, &prepared)
    };
    let edited_id = edited.manifest().batch_id();
    let deleted_id = deleted.manifest().batch_id();
    let child_id = child.manifest().batch_id();
    let build = |order: [ValidatedBatch; 3]| {
        let mut engine = ids.engine();
        engine.stage_ready(baseline.clone());
        for batch in order {
            assert!(!matches!(
                engine.stage_ready(batch).disposition,
                BatchDisposition::Rejected { .. }
            ));
        }
        engine
    };
    let forward = build([edited.clone(), child.clone(), deleted.clone()]);
    let reverse = build([deleted, child, edited]);
    let collect = |engine: &ShardedHotEngine| {
        let mut intents = Vec::new();
        for batch_id in [edited_id, deleted_id, child_id] {
            for intent in engine.conflict_resolution_intents(batch_id).unwrap() {
                if !intents.contains(&intent) {
                    intents.push(intent);
                }
            }
        }
        intents.sort_by_key(|intent| format!("{intent:?}"));
        intents
    };

    let forward_intents = collect(&forward);
    let reverse_intents = collect(&reverse);
    assert_eq!(
        forward_intents.len(),
        2,
        "the four-batch permutation fixture must retain both real conflict intents"
    );
    assert_eq!(
        forward_intents, reverse_intents,
        "I-12: four accepted batches delivered in either valid order must derive identical conflict intents; imitate causal_clock_contains_dot"
    );
    let forward_pairs = forward.conflict_history_unresolved_pair_count_for_test();
    let reverse_pairs = reverse.conflict_history_unresolved_pair_count_for_test();
    assert_eq!(
        forward_pairs, 2,
        "the four-batch permutation fixture must retain both unresolved causal pairs"
    );
    assert_eq!(
        forward_pairs, reverse_pairs,
        "I-12: unresolved-pair accounting must be permutation invariant"
    );
}

#[test]
fn conflict_history_index_rebuild_matches_incremental_resolution_results() {
    let ids = Ids::new();
    let dir = TestDir::new("a3-conflict-index-rebuild");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let edited = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "A3 rebuilt conflict".into(),
    }]);
    let deleted = tx(vec![SemanticOperation::DeleteSubtree {
        root_block_id: ids.block_a,
        page_id: ids.page_a,
    }]);
    let (edited, deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(0xa3_f100, 0xa3_f100),
        edited,
        author(0xa3_f101, 0xa3_f101),
        deleted,
    );
    let engine = apply_pair(ids, &baseline, edited, deleted.clone());
    let before = engine
        .conflict_resolution_intents(deleted.manifest().batch_id())
        .unwrap();
    let snapshot = engine.canonical_snapshot().unwrap();

    // A clean checkpoint intentionally carries no copy of this disposable
    // cache. Dropping it models that open boundary; the next evaluation must
    // reconstruct the exact candidates from accepted batches.
    engine.drop_conflict_history_index_for_test();
    let rebuilt = engine
        .conflict_resolution_intents(deleted.manifest().batch_id())
        .unwrap();
    assert_eq!(rebuilt, before);
    assert_eq!(engine.canonical_snapshot().unwrap(), snapshot);
}

/// I-14: the once-per-open conflict-backlog reseed classifies linearity from
/// manifests alone. A checkpoint-restored engine (no disposable index) must
/// answer `accepted_nonlinear_batch_ids` WITHOUT rebuilding the index, or the
/// first tick after every open loads and re-derives the entire accepted
/// history (wave-3 A3 review, required neighbor). The rebuild stays lazy:
/// it happens on the first evaluation that actually needs the index.
#[test]
fn conflict_backlog_reseed_does_not_rebuild_the_conflict_history_index() {
    let ids = Ids::new();
    let dir = TestDir::new("a3-reseed-no-rebuild");
    let archive = store(&dir, ids);
    let (_, baseline) = seed_engine(ids, &archive);
    let edited = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: test_block_home(ids.block_a),
        },
        content: "A3 reseed without rebuild".into(),
    }]);
    let deleted = tx(vec![SemanticOperation::DeleteSubtree {
        root_block_id: ids.block_a,
        page_id: ids.page_a,
    }]);
    let (edited, deleted) = concurrent_ready(
        ids,
        &archive,
        &baseline,
        author(0xa3_f200, 0xa3_f200),
        edited,
        author(0xa3_f201, 0xa3_f201),
        deleted,
    );
    let engine = apply_pair(ids, &baseline, edited, deleted.clone());
    let expected = engine.accepted_nonlinear_batch_ids().unwrap();
    assert_eq!(expected, vec![deleted.manifest().batch_id()]);

    // Model the checkpoint-open boundary: the disposable index is absent.
    engine.drop_conflict_history_index_for_test();
    assert!(!engine.conflict_history_index_is_current_for_test());

    let reseeded = engine.accepted_nonlinear_batch_ids().unwrap();
    assert_eq!(reseeded, expected);
    assert!(
        !engine.conflict_history_index_is_current_for_test(),
        "I-14: the reopen reseed must classify linearity from manifests, not rebuild the \
         conflict-history index over every accepted batch; imitate \
         accepted_batch_is_causally_linear in oplog/hot_engine.rs"
    );

    // The first real evaluation rebuilds it, once.
    let intents = engine
        .conflict_resolution_intents(deleted.manifest().batch_id())
        .unwrap();
    assert!(!intents.is_empty());
    assert!(engine.conflict_history_index_is_current_for_test());
}
// ---------------------------------------------------------------------------
// Harvest A4 — run-local identity indexes have NO fixed capacity (FIXED).
//
// Four run-local identity maps (page names, portable paths, block claims,
// Logseq claims) were introduced with a shared fixed capacity of 4,096 and
// refused when full. The budgets counted lifetime-DISTINCT identities with no
// removal path, so the refusal was permanent across reopen (I-10), and the
// block-claim member refused only at ACCEPTANCE — after the drain had
// published the manifest — turning a reported save into a permanently
// unopenable store. No refusal in the family ever named an in-scope threat
// scenario (I-8), so the fix REMOVED all four caps; the maps grow with
// lifetime-distinct identities, bounded by archive rebaselining (SPEC-A A5
// decision block). See A4-fix-dossier.md and RECEIPT-repro.md.
//
// The tests below guard the FIXED behavior by driving every path past the
// removed capacity value.
// ---------------------------------------------------------------------------

/// The removed caps' shared value. Tests drive past it so any reintroduced
/// fixed capacity at or below this scale fails them.
const A4_REMOVED_CAP: usize = 4_096;

#[test]
fn a4_run_local_identity_indexes_have_no_fixed_capacity() {
    let index_source = include_str!("page_name_index.rs");
    let engine_source = include_str!("hot_engine.rs");
    for (name, source) in [
        ("page_name_index.rs", index_source),
        ("hot_engine.rs", engine_source),
    ] {
        assert!(
            !source.contains("MAX_EPHEMERAL"),
            "{name} reintroduced a run-local identity capacity. Run-local identity \
             indexes must not refuse at a fixed capacity: such a refusal names no \
             in-scope threat scenario (I-8) and is permanent across reopen because \
             replay refills the maps to identical occupancy (I-10). See \
             specs/campaigns/2026-09-invariant-sweep/A4-fix-dossier.md."
        );
        assert!(
            !source.contains("reached its fixed capacity"),
            "{name} reintroduced a fixed-capacity refusal (I-8/I-10; see \
             A4-fix-dossier.md)"
        );
    }
    // There is exactly one page-name transition access implementation, so the
    // guards above cover the ONLY page-name index Tine has.
    assert_eq!(
        index_source
            .matches("impl PageNameTransitionAccess for")
            .count(),
        1,
        "a second page-name transition access would change what these guards cover"
    );
}

fn a4_page_id(index: usize) -> PageId {
    PageId::from_uuid(uuid(0xa4_0000_0000 + index as u128))
}

fn a4_home_id(index: usize) -> DocumentId {
    DocumentId::from_uuid(uuid(0xa4_4000_0000 + index as u128))
}

fn a4_create_pages(
    engine: &ShardedHotEngine,
    batch: u128,
    range: std::ops::Range<usize>,
) -> Result<PreparedBatch, EngineError> {
    engine.prepare_fixture_transaction(
        author(batch, batch as u64),
        &tx(range
            .map(|index| SemanticOperation::CreatePage {
                page_id: a4_page_id(index),
                home_document_id: a4_home_id(index),
                name: crate::oplog::LogicalPageName::parse(&format!("A4 Page {index}")).unwrap(),
                path: path(&format!("pages/a4-{index}.md")),
                kind: ManagedTextKind::Page,
            })
            .collect()),
    )
}

/// Accept `count` distinct page names in chunks and return the number of
/// accepted batches.
fn a4_seed_names(
    engine: &mut ShardedHotEngine,
    archive: &ObjectStore,
    batch_base: u128,
    count: usize,
    chunk: usize,
) -> usize {
    let mut accepted = 0;
    let mut index = 0;
    while index < count {
        let end = (index + chunk).min(count);
        let prepared = a4_create_pages(engine, batch_base + accepted as u128, index..end)
            .unwrap_or_else(|error| panic!("seeding names {index}..{end} refused: {error:?}"));
        let disposition = engine.stage_ready(ready(archive, &prepared)).disposition;
        assert!(
            matches!(disposition, BatchDisposition::Accepted { .. }),
            "seeding names {index}..{end} was not accepted: {disposition:?}"
        );
        accepted += 1;
        index = end;
    }
    accepted
}

/// Past the removed cap, every page-level operation keeps working: create,
/// same-path rename (page-name index only), delete, and rename back. Before
/// the fix, all of these were refused at 4,096 lifetime-distinct names with
/// no removal path (see RECEIPT-repro.md).
#[test]
#[ignore = "harvest A4 guard: seeds 4,096 real page names (~15s debug)"]
fn a4_page_operations_continue_past_the_removed_cap() {
    let ids = Ids::new();
    let dir = TestDir::new("a4-cap-wedge");
    let archive = store(&dir, ids);
    let mut engine = ids.engine();

    let seeded_batches = a4_seed_names(&mut engine, &archive, 0xa4_0000, A4_REMOVED_CAP, 256);
    eprintln!("a4_cap seeded_names={A4_REMOVED_CAP} seeded_batches={seeded_batches}");
    assert!(paged_fatal_evidence(&engine).is_none());

    // The 4,097th lifetime-distinct page must draft AND be accepted; this
    // consumes both a page-name and a portable-path record past the old caps.
    let past_cap = a4_create_pages(&engine, 0xa4_9000, A4_REMOVED_CAP..A4_REMOVED_CAP + 1)
        .expect("the 4,097th distinct page must draft (I-8/I-10, A4-fix-dossier.md)");
    assert!(matches!(
        engine.stage_ready(ready(&archive, &past_cap)).disposition,
        BatchDisposition::Accepted { .. }
    ));

    // Same-path rename to a brand-new NAME touches ONLY the page-name index.
    let isolated = engine
        .prepare_fixture_transaction(
            author(0xa4_9500, 0xa4_9500),
            &tx(vec![SemanticOperation::RenamePagesAndRewriteReferrers {
                page_changes: vec![crate::oplog::PageRename {
                    page_id: a4_page_id(0),
                    new_name: crate::oplog::LogicalPageName::parse("A4 Isolated New Name").unwrap(),
                    new_path: path("pages/a4-0.md"),
                }],
                block_rewrites: Vec::new(),
                page_preamble_rewrites: Vec::new(),
            }]),
        )
        .expect("a same-path rename past the removed page-name cap must draft");
    assert!(matches!(
        engine.stage_ready(ready(&archive, &isolated)).disposition,
        BatchDisposition::Accepted { .. }
    ));

    // Block-level work keeps working too.
    let block_edit = engine
        .prepare_fixture_transaction(
            author(0xa4_9001, 0xa4_9001),
            &tx(vec![SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id: crate::oplog::BlockId::from_uuid(uuid(0xa4_8000)),
                    home_document_id: test_block_home(crate::oplog::BlockId::from_uuid(uuid(
                        0xa4_8000,
                    ))),
                },
                page_id: a4_page_id(0),
                parent: None,
                order: "a".into(),
                content: "past-cap block write".into(),
            }]),
        )
        .expect("block-only work drafts past the removed caps");
    assert!(matches!(
        engine.stage_ready(ready(&archive, &block_edit)).disposition,
        BatchDisposition::Accepted { .. }
    ));

    // Deleting a page works (the removed portable-path check used to charge
    // the batch's whole changed set and refused even deletes).
    let delete = engine
        .prepare_fixture_transaction(
            author(0xa4_9002, 0xa4_9002),
            &tx(vec![SemanticOperation::DeletePage {
                page_id: a4_page_id(1),
            }]),
        )
        .expect("deleting a page past the removed caps must draft");
    assert!(matches!(
        engine.stage_ready(ready(&archive, &delete)).disposition,
        BatchDisposition::Accepted { .. }
    ));

    // Renaming back to an already-held name works.
    let rename_back = engine
        .prepare_fixture_transaction(
            author(0xa4_9003, 0xa4_9003),
            &tx(vec![SemanticOperation::RenamePagesAndRewriteReferrers {
                page_changes: vec![crate::oplog::PageRename {
                    page_id: a4_page_id(0),
                    new_name: crate::oplog::LogicalPageName::parse("A4 Page 0").unwrap(),
                    new_path: path("pages/a4-0.md"),
                }],
                block_rewrites: Vec::new(),
                page_preamble_rewrites: Vec::new(),
            }]),
        )
        .expect("renaming back past the removed caps must draft");
    assert!(matches!(
        engine
            .stage_ready(ready(&archive, &rename_back))
            .disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(paged_fatal_evidence(&engine).is_none());
}

/// Reopen behavior past the removed cap: replaying the accepted tail into a
/// fresh engine succeeds, post-reopen page work succeeds, and a peer-authored
/// new name is ACCEPTED by a receiver whose indexes are past the old cap
/// (before the fix the peer batch was rejected — a sync/portability hazard).
#[test]
#[ignore = "harvest A4 guard: seeds 4,097 real page names twice (~30s debug)"]
fn a4_reopen_replays_past_the_removed_cap_and_accepts_peer_names() {
    let ids = Ids::new();
    let dir = TestDir::new("a4-cap-reopen");
    let archive = store(&dir, ids);
    let mut engine = ids.engine();
    a4_seed_names(&mut engine, &archive, 0xa4_0000, A4_REMOVED_CAP + 1, 256);

    // Reopen: replay every committed manifest into a fresh sequence-zero
    // engine, exactly as `replay_clean_committed_tail` does at open.
    let mut replay = ids.engine();
    let manifests = archive.committed_manifests().unwrap();
    let mut replayed = 0;
    for manifest in &manifests {
        let disposition = replay
            .stage_from_store(&archive, manifest.batch_id())
            .unwrap()
            .disposition;
        assert!(
            matches!(disposition, BatchDisposition::Accepted { .. }),
            "replay of {} was not accepted: {disposition:?}",
            manifest.batch_id()
        );
        replayed += 1;
    }
    eprintln!(
        "a4_reopen replayed_batches={replayed} manifests={}",
        manifests.len()
    );
    assert!(paged_fatal_evidence(&replay).is_none());

    // Post-reopen page-name work succeeds.
    let after_reopen = replay
        .prepare_fixture_transaction(
            author(0xa4_9100, 0xa4_9100),
            &tx(vec![SemanticOperation::RenamePagesAndRewriteReferrers {
                page_changes: vec![crate::oplog::PageRename {
                    page_id: a4_page_id(0),
                    new_name: crate::oplog::LogicalPageName::parse("A4 Post Reopen Name").unwrap(),
                    new_path: path("pages/a4-0.md"),
                }],
                block_rewrites: Vec::new(),
                page_preamble_rewrites: Vec::new(),
            }]),
        )
        .expect("post-reopen rename past the removed cap must draft");
    assert!(matches!(
        replay
            .stage_ready(ready(&archive, &after_reopen))
            .disposition,
        BatchDisposition::Accepted { .. }
    ));

    // A peer legitimately authors a new name; the receiver must accept it.
    let peer = ids.engine();
    let peer_batch = a4_create_pages(&peer, 0xa4_9200, 900_000..900_001)
        .expect("an empty peer engine can acquire a new page name");
    let peer_dir = TestDir::new("a4-cap-peer");
    let peer_archive = ObjectStore::open(&peer_dir.path().join("store"), ids.workspace).unwrap();
    let delivered = replay
        .stage_ready(ready(&peer_archive, &peer_batch))
        .disposition;
    assert!(
        matches!(delivered, BatchDisposition::Accepted { .. }),
        "a receiver past the removed cap must accept a peer-authored name: {delivered:?}"
    );
}

/// The block-claim member of the removed family: before the fix it had no
/// authoring-time pre-check, so its refusal landed at ACCEPTANCE — after the
/// drain had published the manifest — and the store became permanently
/// unopenable (`OpenRefused`) from ordinary editing. Past the removed cap,
/// every batch must be accepted and the index simply grows.
#[test]
#[ignore = "harvest A4 guard: creates 8,192 real blocks (~30s debug)"]
fn a4_block_claims_grow_past_the_removed_cap_through_acceptance() {
    let ids = Ids::new();
    let dir = TestDir::new("a4-block-claims");
    let archive = store(&dir, ids);
    let mut engine = ids.engine();
    let seed = a4_create_pages(&engine, 0xa4_0000, 0..1).unwrap();
    assert!(matches!(
        engine.stage_ready(ready(&archive, &seed)).disposition,
        BatchDisposition::Accepted { .. }
    ));

    const CHUNK: usize = 256;
    let target = A4_REMOVED_CAP * 2;
    let mut made = 0usize;
    let mut batch = 0xa4_b000u128;
    while made < target {
        let prepared = engine
            .prepare_fixture_transaction(
                author(batch, batch as u64),
                &tx((made..made + CHUNK)
                    .map(|index| SemanticOperation::CreateBlock {
                        block: BlockLocation {
                            block_id: crate::oplog::BlockId::from_uuid(uuid(
                                0xa4_c000_0000 + index as u128,
                            )),
                            home_document_id: test_block_home(crate::oplog::BlockId::from_uuid(
                                uuid(0xa4_c000_0000 + index as u128),
                            )),
                        },
                        page_id: a4_page_id(0),
                        parent: None,
                        order: format!("{index:08}").into(),
                        content: format!("a4 block {index}"),
                    })
                    .collect()),
            )
            .expect("block creation past the removed cap must draft");
        let disposition = engine.stage_ready(ready(&archive, &prepared)).disposition;
        assert!(
            matches!(disposition, BatchDisposition::Accepted { .. }),
            "block batch at {made} lifetime blocks was not accepted \
             (I-8/I-10, A4-fix-dossier.md): {disposition:?}"
        );
        made += CHUNK;
        batch += 1;
    }
    assert_eq!(
        engine.instrumentation().block_claim_hot_entries,
        target,
        "the run-local block-claim index simply grows with lifetime blocks"
    );
}

/// What the run-local page-name index costs on the WAITED OPEN PATH
/// (measurement, unchanged by the fix): `replay_clean_committed_tail` reruns
/// every committed manifest through `validate_and_apply` at every open, and
/// each replayed batch refills the identity indexes. Cost is proportional to
/// lifetime accepted history (I-14); the stated bound is archive rebaselining
/// (SPEC-A A5 decision block).
#[test]
#[ignore = "harvest A4 measurement: seeds and replays up to 4,096 page names"]
fn a4_measure_committed_tail_replay_cost_by_lifetime_page_names() {
    for names in [512usize, 1_024, 2_048, A4_REMOVED_CAP] {
        let ids = Ids::new();
        let dir = TestDir::new("a4-replay-cost");
        let archive = store(&dir, ids);
        let mut engine = ids.engine();
        let seed_started = std::time::Instant::now();
        let batches = a4_seed_names(&mut engine, &archive, 0xa4_0000, names, 256);
        let seed_ms = seed_started.elapsed().as_secs_f64() * 1000.0;

        let manifests = archive.committed_manifests().unwrap();
        let mut replay = ids.engine();
        let replay_started = std::time::Instant::now();
        for manifest in &manifests {
            let disposition = replay
                .stage_from_store(&archive, manifest.batch_id())
                .unwrap()
                .disposition;
            assert!(
                matches!(disposition, BatchDisposition::Accepted { .. }),
                "replay of {} was not accepted: {disposition:?}",
                manifest.batch_id()
            );
        }
        let replay_ms = replay_started.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "a4_replay names={names} batches={batches} manifests={} seed_ms={seed_ms:.1} \
             replay_ms={replay_ms:.1} replay_ms_per_name={:.3} block_claim_entries={}",
            manifests.len(),
            replay_ms / names as f64,
            replay.instrumentation().block_claim_hot_entries
        );
    }
}

/// I-12 guard for W4-G1 item 11. The tick's skip cause and the capture's
/// refusal must come from ONE predicate; the wave-4 manager review found them
/// forked (`capture_clean_checkpoint` kept its own copy of the eight
/// eligibility predicates while the scheduler had grown a second answer), so
/// the engine could have refused a capture the tick reported as eligible.
#[test]
fn clean_checkpoint_capture_eligibility_has_one_producer() {
    let source = include_str!("hot_engine.rs");
    assert_eq!(
        source.matches("self.persisted_staged.is_empty()").count(),
        1,
        "I-12: clean-checkpoint capture eligibility must have exactly ONE producer, \
         `clean_checkpoint_capture_skip_reason`. A second copy of these predicates lets \
         the tick's reported cause and the capture's refusal drift apart. \
         Imitate `causal_clock_contains_dot` in `oplog/conflict_history.rs`."
    );
    assert_eq!(
        source
            .matches("clean_checkpoint_capture_skip_reason(durable_sequence)")
            .count(),
        2,
        "I-12: both `schedule_clean_checkpoint` and `capture_clean_checkpoint` must ask \
         the single eligibility predicate rather than re-deriving it"
    );
}

// ---------------------------------------------------------------------------
// W5-census — acceptance-only refusals for drain-published local batches.
//
// The managed-local drain publishes a batch's immutable manifest BEFORE the
// hot engine accepts it (`local_journal_drain.rs`, stage `ArchivePublication`
// then `EngineAcceptance`). Replay treats a manifest-committed clean operation
// that does not validate as accepted as archive corruption
// (`hot_engine.rs::replay_clean_committed_batch_ids`). So any refusal that a
// drain-published LOCAL batch can only meet at acceptance turns a Save the app
// reported successful into a store that refuses to open on every later open —
// the A4 shape (I-10, I-8). A4-fix removed one proven instance (four run-local
// capacity caps); the census in the "Acceptance-only refusals for
// drain-published local batches" subsection of docs/storage-sync-contract.md
// enumerates the rest, and these tests guard the one R row it found.
//
// Exemplars: specs/campaigns/2026-09-invariant-sweep/A4-fix-dossier.md and the
// `a4_*` guards above.
// ---------------------------------------------------------------------------

mod w5_census {
    use crate::model::{BlockDto, Format, PageDto, PageKind};
    use crate::oplog::{
        DeviceId, DocumentId, LineageDigest, ProjectionEndpointId, SessionId, WorkspaceId,
    };
    use crate::sync_runtime::{
        SyncApplicationGraphMutationRequest, SyncApplicationPageSaveRequest,
        SyncApplicationPageSaveTarget, SyncLocalActivationIdentities, SyncLocalActivationRequest,
        SyncLocalActivationStatus, SyncPageKind, SyncRuntimeHandle, SyncRuntimeOpenRequest,
        SyncRuntimeOpenStatus, SyncStorageProfile,
    };
    use std::path::PathBuf;
    use uuid::Uuid;

    /// What every row of the census must hold for. Quoted in each failure so a
    /// future reintroduction reads the rule, not just a diff.
    const RULE: &str = "a Save the app reported successful must never leave a store that refuses \
to open. The managed-local drain publishes the manifest before the engine accepts the batch, so a \
refusal reachable only at acceptance for a drain-published local batch is permanent data loss \
across reopen (I-10) and names no in-scope scenario the draft could not have named first (I-8). \
Refuse at draft time (before the journal append) or accept. See the \"Acceptance-only refusals for \
drain-published local batches\" subsection of docs/storage-sync-contract.md, \
specs/campaigns/2026-09-invariant-sweep/A4-fix-dossier.md, and the `a4_*` guards.";

    struct Fixture {
        root: PathBuf,
        request: SyncLocalActivationRequest,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fixture(label: &str, seed: u128) -> Fixture {
        let root = std::env::temp_dir().join(format!("tine-w5-census-{label}-{}", Uuid::new_v4()));
        let graph_root = root.join("graph");
        std::fs::create_dir_all(graph_root.join("logseq")).unwrap();
        std::fs::write(
            graph_root.join("logseq/config.edn"),
            br#"{:pages-directory "pages"
                :journals-directory "journals"
                :file/name-format :triple-lowbar
                :journal/file-name-format "yyyy_MM_dd"
                :journal/page-title-format "yyyy-MM-dd"}"#,
        )
        .unwrap();
        std::fs::write(graph_root.join("Root.md"), b"- seed\n").unwrap();
        let private = root.join("private");
        let request = SyncLocalActivationRequest {
            archive_root: private.join("archive"),
            graph_root: graph_root.clone(),
            enrollment_root: private.join("enrollment"),
            receipt_root: private.join("receipts"),
            database_path: private.join("projection/bootstrap.sqlite"),
            application_runtime_root: private.join("runtime"),
            capture_root: private.join("capture"),
            preparation_root: private.join("preparation"),
            provider_root: graph_root.join(".tine-sync/v2/shared"),
            provider_journal_root: private.join("provider/device/journal"),
            identities: SyncLocalActivationIdentities {
                workspace_id: WorkspaceId::from_uuid(Uuid::from_u128(seed)),
                lineage_digest: LineageDigest::of(format!("lineage-{seed}").as_bytes()),
                catalog_document_id: DocumentId::from_uuid(Uuid::from_u128(seed + 1)),
                endpoint_id: ProjectionEndpointId::from_uuid(Uuid::from_u128(seed + 2)),
                device_id: DeviceId::from_uuid(Uuid::from_u128(seed + 3)),
                preparation_id: Uuid::from_u128(seed + 4),
                session_id: SessionId::from_uuid(Uuid::from_u128(seed + 5)),
            },
        };
        Fixture { root, request }
    }

    fn activate(fixture: &Fixture) -> SyncRuntimeHandle {
        let mut activated = SyncRuntimeHandle::activate_or_resume_local(fixture.request.clone());
        for _ in 0..64 {
            if activated.status == SyncLocalActivationStatus::Active {
                break;
            }
            activated = SyncRuntimeHandle::activate_or_resume_local(fixture.request.clone());
        }
        assert_eq!(
            activated.status,
            SyncLocalActivationStatus::Active,
            "the W5-census fixture must activate"
        );
        let handle = activated
            .handle
            .expect("an active activation carries a handle");
        for _ in 0..256 {
            if !handle.status().unwrap().watcher.pending {
                break;
            }
            let _ = handle.tick();
        }
        handle
    }

    fn page(name: &str, body: &str) -> PageDto {
        PageDto {
            activation: None,
            name: name.into(),
            kind: PageKind::Page,
            title: name.into(),
            pre_block: None,
            blocks: vec![BlockDto {
                id: format!("temporary-w5-{body}"),
                raw: body.into(),
                ..BlockDto::default()
            }],
            rev: None,
            format: Format::Md,
            read_only: false,
            path: String::new(),
            guide: false,
        }
    }

    /// Was this save reported to the user as durable?
    /// Save one new page and report the exact outcome. A refusal here is a
    /// legal census answer, so the reason has to survive into the assertion
    /// message rather than being folded into a bare `false`.
    fn save_new(handle: &SyncRuntimeHandle, name: &str, body: &str) -> String {
        format!(
            "{:?}",
            handle.save_application_page(SyncApplicationPageSaveRequest {
                target: SyncApplicationPageSaveTarget::New {
                    name: name.to_owned(),
                    page_kind: SyncPageKind::Page,
                },
                page: page(name, body),
            })
        )
    }

    fn saved(outcome: &str) -> bool {
        outcome.starts_with("Ok(Saved")
    }

    /// Run drain turns until the managed-local journal has no pending record.
    fn settle(handle: &SyncRuntimeHandle) -> bool {
        for _ in 0..4096 {
            let status = handle.status().unwrap();
            if status.managed_local_pending == 0 && !status.watcher.pending {
                return true;
            }
            if handle.tick().is_err() {
                return false;
            }
        }
        false
    }

    fn reopen(fixture: &Fixture) -> SyncRuntimeOpenStatus {
        SyncRuntimeHandle::open(SyncRuntimeOpenRequest {
            profile: SyncStorageProfile::ExperimentalLocal,
            clean_identities: Some(fixture.request.identities.clone()),
            graph_root: fixture.request.graph_root.clone(),
            enrollment_root: fixture.request.enrollment_root.clone(),
            archive_root: fixture.request.archive_root.clone(),
            receipt_root: fixture.request.receipt_root.clone(),
            database_path: fixture.request.database_path.clone(),
            application_runtime_root: fixture.request.application_runtime_root.clone(),
            provider_root: fixture.request.provider_root.clone(),
            provider_journal_root: fixture.request.provider_journal_root.clone(),
        })
        .status
    }

    /// Assert the drain settled and the store still reopens.
    fn assert_settles_and_reopens(fixture: &Fixture, handle: SyncRuntimeHandle, journey: &str) {
        let settled = settle(&handle);
        let status = handle.status().unwrap();
        assert!(
            settled && status.managed_local_pending == 0,
            "{journey}: the managed-local drain never settled \
             (pending={}, stage={:?}, detail={:?}). {RULE}",
            status.managed_local_pending,
            status.managed_local_stage,
            status.detail,
        );
        assert!(
            status.detail.is_none(),
            "{journey}: the drain reported a derivative failure: {:?}. {RULE}",
            status.detail,
        );
        drop(handle);
        let reopened = reopen(fixture);
        assert!(
            matches!(reopened, SyncRuntimeOpenStatus::Active),
            "{journey}: reopen is {reopened:?}, not Active. {RULE}",
        );
    }

    /// The one R row of the census: two pages whose exact names differ but
    /// whose CANONICAL page-name keys are equal ("Alpha" and "/Alpha" both
    /// fold to `alpha`) and whose derived paths differ, saved back to back
    /// with no drain turn in between.
    ///
    /// Before the fix the second Save was reported `Saved` — the run-local
    /// page-name index was blind to the first record, which is journal-durable
    /// but not yet accepted — and the drain then met
    /// "canonical page-name key is occupied at the declared dependency
    /// frontier" at `EngineAcceptance`, after publishing the manifest. Every
    /// later open replayed that manifest and refused: `OpenRefused`.
    #[test]
    fn w5_census_a_canonical_page_name_collision_is_settled_before_the_journal_append() {
        let fixture = fixture("page-name-collision", 0x5c0000);
        let handle = activate(&fixture);

        let first = save_new(&handle, "Alpha", "first");
        assert!(saved(&first), "the first page must save: {first}");
        // No drain turn here: the first record is journal-durable and not yet
        // accepted while the second is drafted.
        let second = save_new(&handle, "/Alpha", "second");
        assert_settles_and_reopens(
            &fixture,
            handle,
            "save \"Alpha\", then save \"/Alpha\" before the drain runs",
        );
        let _ = second;
    }

    /// Breadth guard for the rest of the class: one undrained burst of
    /// ordinary local page work — several creates plus a rename — must either
    /// be refused at Save or be accepted, and must never leave a store that
    /// refuses to open.
    #[test]
    fn w5_census_an_undrained_local_burst_leaves_a_store_that_reopens_active() {
        let fixture = fixture("undrained-burst", 0x5c2000);
        let handle = activate(&fixture);

        for index in 0..4 {
            let outcome = save_new(&handle, &format!("Burst {index}"), &format!("body {index}"));
            assert!(saved(&outcome), "burst create {index} must save: {outcome}");
        }
        let renamed =
            handle.mutate_application_graph(SyncApplicationGraphMutationRequest::RenamePage {
                old: "Burst 1".into(),
                new: "Burst One".into(),
                expected_path: None,
            });
        assert!(
            renamed.is_ok(),
            "an undrained rename must not error: {renamed:?}"
        );
        assert_settles_and_reopens(
            &fixture,
            handle,
            "four creates and a rename with no drain turn in between",
        );
    }
}

// ---------------------------------------------------------------------------
// P3 own-endpoint completion boundary.
//
// The receiver half of the absence decision is qualified in
// `oplog::receiver_absence_summary`. This module qualifies the *own* half
// through a real engine open, stage, flush and prune: the R16-C2 prune policy
// asks two questions per local key, and both must be point questions. If the
// policy ever re-enumerates historical receiver paths, or if the decision map
// keeps every identity this activation completed resident, the flat assertions
// below fail.
// ---------------------------------------------------------------------------

mod p3_own_completion {
    use super::{seed_engine, store, uuid, DocumentKey, Ids, TestDir};
    use crate::oplog::absence_decision::AbsenceDecision;
    use crate::oplog::current_action_roots::{
        ProjectionActionCursor, ACTION_NAMESPACE as ABSENCE_NAMESPACE,
    };
    use crate::oplog::receiver_absence_summary::{
        HistoryAccess, ROWS_NAMESPACE, ROW_PREFIX, SUMMARY_NAMESPACE,
    };
    use crate::oplog::{
        BlobDescription, CrdtPeerCounter, CrdtPeerId, DeviceId, DocumentDependencies, DocumentId,
        FrontierV2, ManagedPath, ObjectStore, PageId, ProjectionEndpointBinding,
        ProjectionEndpointId, ProjectionIntent, ProjectionPrecondition, ProjectionReceiptStore,
        ProjectionTargetKind, ShardedHotEngine, WorkspaceId,
    };
    use crate::Graph;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    /// One frontier on a single shared document, so two counters on the same
    /// peer are strictly comparable and a receiver answer can dominate an own
    /// answer on the same key.
    fn frontier(counter: u64) -> FrontierV2 {
        FrontierV2::new(vec![DocumentDependencies::new(
            DocumentKey::Entity(DocumentId::from_uuid(uuid(0x9f_0001))),
            vec![CrdtPeerCounter::new(CrdtPeerId::from_u64(9), counter)],
            Vec::new(),
        )
        .unwrap()])
        .unwrap()
    }

    fn intent(
        workspace: WorkspaceId,
        page_id: PageId,
        managed_path: &str,
        counter: u64,
        kind: ProjectionTargetKind,
    ) -> ProjectionIntent {
        ProjectionIntent::new(
            workspace,
            page_id,
            ManagedPath::parse(managed_path).unwrap(),
            frontier(counter),
            Vec::new(),
            ProjectionPrecondition::Absent,
            kind,
            match kind {
                ProjectionTargetKind::Present => {
                    BlobDescription::of(format!("- {managed_path} {counter}\n").as_bytes())
                }
                ProjectionTargetKind::Absent => BlobDescription::of(&[]),
            },
            Vec::new(),
        )
        .unwrap()
    }

    /// A real clean-runtime-shaped engine: accepted genesis pages, an attached
    /// operation archive, an enrolled projection endpoint, the device-local
    /// own-endpoint completion chain, and the receiver absence decision map.
    struct Opened {
        _dir: TestDir,
        _graph: Graph,
        _receipts: ProjectionReceiptStore,
        engine: ShardedHotEngine,
        ids: Ids,
    }

    fn open(label: &str) -> Opened {
        let dir = TestDir::new(label);
        let ids = Ids::new();
        let archive = store(&dir, ids);
        let (mut engine, _batch) = seed_engine(ids, &archive);
        engine
            .attach_clean_archive_store(
                ObjectStore::open(&dir.path().join("store"), ids.workspace).unwrap(),
            )
            .unwrap();
        std::fs::create_dir_all(dir.path().join("graph")).unwrap();
        let graph = Graph::open(&dir.path().join("graph"));
        let endpoint = ProjectionEndpointBinding::enroll_graph(
            &graph,
            ProjectionEndpointId::from_uuid(uuid(0x9f_0202)),
            DeviceId::from_uuid(uuid(0x9f_0203)),
        )
        .unwrap();
        let receipts = ProjectionReceiptStore::open_for_endpoint(
            &dir.path().join("receipts"),
            ids.workspace,
            endpoint,
        )
        .unwrap();
        engine
            .attach_clean_projection_endpoint(&graph, &receipts)
            .unwrap();
        engine.open_local_completion_index(&archive).unwrap();
        engine.open_absence_decision_map(&receipts).unwrap();
        // Compact on every flush so each flush actually runs the prune policy.
        engine.force_local_completion_compaction_for_test(1);
        Opened {
            _dir: dir,
            _graph: graph,
            _receipts: receipts,
            engine,
            ids,
        }
    }

    /// A reopenable archive + receipt pair, so a damaged derived index can be
    /// observed at one engine open and repaired at the next.
    struct Fixture {
        _dir: TestDir,
        root: PathBuf,
        graph_root: PathBuf,
        graph: Graph,
        receipts: ProjectionReceiptStore,
        ids: Ids,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let dir = TestDir::new(label);
            let ids = Ids::new();
            let root = dir.path().join("store");
            let graph_root = dir.path().join("graph");
            std::fs::create_dir_all(&graph_root).unwrap();
            let graph = Graph::open(&graph_root);
            let archive = ObjectStore::open(&root, ids.workspace).unwrap();
            let endpoint = ProjectionEndpointBinding::enroll_graph(
                &graph,
                ProjectionEndpointId::from_uuid(uuid(0x9f_0302)),
                DeviceId::from_uuid(uuid(0x9f_0303)),
            )
            .unwrap();
            let receipts = ProjectionReceiptStore::open_for_endpoint(
                &dir.path().join("receipts"),
                ids.workspace,
                endpoint,
            )
            .unwrap();
            // Production managed opens install the durable discovery cursor
            // before any receipt can be authored.
            receipts.attach_action_cursor(std::sync::Arc::new(
                ProjectionActionCursor::open(&archive).unwrap(),
            ));
            Self {
                _dir: dir,
                root,
                graph_root,
                graph,
                receipts,
                ids,
            }
        }

        /// A fresh engine over the same archive: this is the reopen.
        fn absence_engine(&self) -> ShardedHotEngine {
            let mut engine = self.ids.engine();
            engine
                .attach_clean_archive_store(
                    ObjectStore::open(&self.root, self.ids.workspace).unwrap(),
                )
                .unwrap();
            engine
        }

        /// Publish one real receiver receipt — intent, attempt, projection,
        /// completion — and notify the engine exactly as production does.
        fn publish_receiver_receipt(
            &self,
            engine: &mut ShardedHotEngine,
            page_id: PageId,
            managed_path: &ManagedPath,
            counter: u64,
            kind: ProjectionTargetKind,
        ) -> ProjectionIntent {
            let target = match kind {
                ProjectionTargetKind::Present => {
                    format!("- {managed_path} {counter}\n").into_bytes()
                }
                ProjectionTargetKind::Absent => Vec::new(),
            };
            let built = intent(
                self.ids.workspace,
                page_id,
                managed_path.as_str(),
                counter,
                kind,
            );
            self.receipts.publish_intent(&built, None).unwrap();
            engine.note_receiver_projection_intent(&built).unwrap();
            let reservation = self.receipts.reserve_attempt(&built).unwrap();
            let mut authority = self
                .receipts
                .begin_mutation(&built, Some(&reservation))
                .unwrap();
            let proof = self
                .graph
                .write_page_projection(
                    built.path().as_str(),
                    None,
                    target.as_slice(),
                    &mut authority,
                )
                .unwrap();
            self.receipts
                .publish_completion(authority, &built, &proof)
                .unwrap();
            engine.note_receiver_projection_completion(&built).unwrap();
            built
        }

        fn rows_dir(&self) -> PathBuf {
            self.root.join(ABSENCE_NAMESPACE).join(ROWS_NAMESPACE)
        }

        fn summary_dir(&self) -> PathBuf {
            self.root.join(ABSENCE_NAMESPACE).join(SUMMARY_NAMESPACE)
        }

        fn row_object_paths(&self) -> Vec<PathBuf> {
            Self::entries_with_prefix(&self.rows_dir(), ROW_PREFIX)
        }

        fn summary_object_paths(&self) -> Vec<PathBuf> {
            Self::entries_with_prefix(&self.summary_dir(), "")
        }

        fn entries_with_prefix(directory: &Path, prefix: &str) -> Vec<PathBuf> {
            let Ok(entries) = std::fs::read_dir(directory) else {
                return Vec::new();
            };
            let mut found = entries
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(prefix))
                })
                .collect::<Vec<_>>();
            found.sort();
            found
        }
    }

    /// What one steady flush costs and holds once `distinct` historical
    /// identities exist behind fixed live pages and fixed retained work.
    ///
    /// `objects_read` is deliberately outside the equality: the durable rows
    /// live in authenticated maps, so one point read walks O(log H) nodes. That
    /// term is the persistent-bytes allowance D-5 grants, not resident state.
    #[derive(Debug, Eq, PartialEq)]
    struct SteadyFlush {
        local_entries: usize,
        resident_absence_rows: usize,
        point_lookups: usize,
    }

    /// The byte/object side of the same steady flush, kept apart from the
    /// resident equality so a logarithmic term can be bounded as logarithmic.
    #[derive(Debug)]
    struct SteadyObjects {
        flush_objects_read: usize,
        total: HistoryAccess,
    }

    fn measure(distinct: usize) -> (SteadyFlush, SteadyObjects) {
        let mut opened = open(&format!("p3-own-completion-{distinct}"));
        let ids = opened.ids;
        let engine = &mut opened.engine;
        let live_a = "pages/A.md";
        let live_b = "pages/B.md";

        // `distinct` receiver-completed historical identities on one live page:
        // H grows, G/T/P/O do not. Each of these becomes a durable point row.
        for index in 0..distinct {
            // Alternating kinds so the reopened answers below have to be
            // exact in both directions: a receiver-confirmed absence permits a
            // later create, a receiver-confirmed presence must keep deferring.
            let receiver = intent(
                ids.workspace,
                ids.page_a,
                &format!("pages/A-old-{index}.md"),
                2 * index as u64 + 2,
                if index % 2 == 0 {
                    ProjectionTargetKind::Absent
                } else {
                    ProjectionTargetKind::Present
                },
            );
            engine.note_receiver_projection_intent(&receiver).unwrap();
            engine
                .note_receiver_projection_completion(&receiver)
                .unwrap();
        }

        // Own-endpoint completions this activation performed. Two families are
        // prunable — one with receiver history underneath it and a dominated
        // frontier, one with no receiver history at all — and two are the live
        // answers the index must keep.
        for index in 0..distinct {
            let dominated = intent(
                ids.workspace,
                ids.page_a,
                &format!("pages/A-old-{index}.md"),
                2 * index as u64 + 1,
                ProjectionTargetKind::Absent,
            );
            assert!(engine
                .stage_local_projection_completion(&dominated)
                .unwrap());
            let orphan = intent(
                ids.workspace,
                ids.page_a,
                &format!("pages/A-stale-{index}.md"),
                index as u64 + 1,
                ProjectionTargetKind::Absent,
            );
            assert!(engine.stage_local_projection_completion(&orphan).unwrap());
        }
        for (page_id, live) in [(ids.page_a, live_a), (ids.page_b, live_b)] {
            let present = intent(
                ids.workspace,
                page_id,
                live,
                1,
                ProjectionTargetKind::Present,
            );
            assert!(engine.stage_local_projection_completion(&present).unwrap());
        }
        assert!(engine
            .flush_local_projection_completions(BTreeSet::new())
            .unwrap());
        assert_eq!(
            engine.local_completion_entry_count_for_test(),
            2,
            "the prune policy must keep exactly the two live answers at \
{distinct} historical identities"
        );

        // The steady flush: fixed live pages, fixed (empty) retained work, one
        // new own completion per live page.
        let before = engine.receiver_history_access_for_test().unwrap();
        for (page_id, live) in [(ids.page_a, live_a), (ids.page_b, live_b)] {
            let present = intent(
                ids.workspace,
                page_id,
                live,
                2,
                ProjectionTargetKind::Present,
            );
            assert!(engine.stage_local_projection_completion(&present).unwrap());
        }
        assert!(engine
            .flush_local_projection_completions(BTreeSet::new())
            .unwrap());
        let after = engine.receiver_history_access_for_test().unwrap();

        // The historical identities are still exactly answerable: pruning own
        // evidence never resurrects a path the receiver released.
        for index in 0..distinct {
            let historical = ManagedPath::parse(&format!("pages/A-old-{index}.md")).unwrap();
            let expected = if index % 2 == 0 {
                AbsenceDecision::Create
            } else {
                AbsenceDecision::DeferredAbsence
            };
            assert_eq!(
                engine
                    .receiver_absence_decision(ids.page_a, &historical)
                    .unwrap(),
                expected,
                "historical path {historical} must stay exactly answerable after \
its own evidence was pruned"
            );
            assert!(!engine
                .restored_generation_requires_absence_deferral(ids.page_a, &historical)
                .unwrap());
        }
        let unknown = ManagedPath::parse("pages/Never projected.md").unwrap();
        assert_eq!(
            engine
                .receiver_absence_decision(ids.page_c, &unknown)
                .unwrap(),
            AbsenceDecision::Create
        );

        (
            SteadyFlush {
                local_entries: engine.local_completion_entry_count_for_test(),
                resident_absence_rows: engine.resident_absence_row_count_for_test(),
                point_lookups: after.point_lookups - before.point_lookups,
            },
            SteadyObjects {
                flush_objects_read: after.objects_read - before.objects_read,
                total: after,
            },
        )
    }

    /// The own-endpoint half of the P3 bound, measured on a real engine.
    ///
    /// Eight and sixty-four distinct completed identities behind the same two
    /// live pages and the same (empty) retained set must cost the same steady
    /// flush and hold the same resident state.
    #[test]
    fn own_completion_flush_and_prune_are_flat_across_distinct_completed_identities() {
        let (small, small_total) = measure(8);
        let (large, large_total) = measure(64);
        eprintln!("own-completion steady flush at 8 identities: {small:?} total {small_total:?}");
        eprintln!("own-completion steady flush at 64 identities: {large:?} total {large_total:?}");
        assert_eq!(
            small, large,
            "the own-endpoint prune policy grew with completed history"
        );
        // Three point reads per surviving local key: one when the completion is
        // staged, one for the prune policy's R16-C2 question, one when the map
        // is re-derived from what survived. A constant per live key, and the
        // equality above already proves it does not move with history.
        assert!(
            small.point_lookups <= 4 * small.local_entries,
            "the prune policy must ask a constant number of point questions per \
live local key, not one per history row: {} reads for {} keys",
            small.point_lookups,
            small.local_entries
        );
        // Eight times the history may cost at most twice the objects per steady
        // flush: a logarithmic walk, not a scan.
        assert!(
            large_total.flush_objects_read < 2 * small_total.flush_objects_read,
            "steady flush object reads grew faster than the map depth: \
{} at 8 identities, {} at 64",
            small_total.flush_objects_read,
            large_total.flush_objects_read
        );
        // Eight times the identities may cost at most eight times the point
        // reads in total. Any step that enumerated the history would make the
        // whole run quadratic in it instead.
        assert!(
            large_total.total.point_lookups <= 8 * small_total.total.point_lookups,
            "total point reads grew faster than the work performed: {} for 8 \
identities, {} for 64",
            small_total.total.point_lookups,
            large_total.total.point_lookups
        );
    }

    /// The damaged-index contract, through a real engine open.
    ///
    /// The unit-level controls in `oplog::receiver_absence_summary` prove the
    /// index's own behaviour. This proves the engine boundary the production
    /// callers actually use: a row the authenticated root names but disk cannot
    /// supply is refused **by name** rather than answered `Create` — answering
    /// `Create` would recreate a file the receiver deleted — the derived roots
    /// are retired, and the next engine open runs the counted repair from
    /// retained receipts and returns the exact old answer.
    #[test]
    fn a_damaged_row_refuses_at_the_engine_then_repairs_at_the_next_engine_open() {
        let fixture = Fixture::new("p3-engine-damage-repair");
        let gone = ManagedPath::parse("pages/Receiver deleted me.md").unwrap();
        let present = ManagedPath::parse("pages/Receiver keeps me.md").unwrap();

        let mut engine = fixture.absence_engine();
        engine.open_absence_decision_map(&fixture.receipts).unwrap();
        let deleted = fixture.publish_receiver_receipt(
            &mut engine,
            fixture.ids.page_a,
            &gone,
            1,
            ProjectionTargetKind::Present,
        );
        let kept = fixture.publish_receiver_receipt(
            &mut engine,
            fixture.ids.page_b,
            &present,
            1,
            ProjectionTargetKind::Present,
        );
        assert_eq!(
            engine
                .receiver_absence_decision(fixture.ids.page_a, &gone)
                .unwrap(),
            AbsenceDecision::DeferredAbsence
        );
        drop(engine);

        // Damage exactly one durable row.
        let rows = fixture.row_object_paths();
        assert_eq!(rows.len(), 2, "one row per completed receiver identity");
        std::fs::remove_file(&rows[0]).unwrap();

        let probe = fixture.absence_engine();
        probe.open_absence_decision_map(&fixture.receipts).unwrap();
        let probe_stats = probe
            .receiver_absence_summary_open_stats_for_test()
            .expect("the managed path records its open attribution");
        assert_eq!(
            probe_stats.full_catalog_passes, 0,
            "a deleted leaf row is not visible at open: it must be found at its own \
point read, not hidden by an unrelated rebuild: {probe_stats:?}"
        );
        let answers = [
            probe.receiver_absence_decision(fixture.ids.page_a, &gone),
            probe.receiver_absence_decision(fixture.ids.page_b, &present),
        ];
        let refusals = answers
            .iter()
            .filter(|answer| answer.is_err())
            .collect::<Vec<_>>();
        assert_eq!(
            refusals.len(),
            1,
            "exactly the damaged identity refuses: {answers:?}"
        );
        let refusal = format!("{:?}", refusals[0]);
        assert!(
            refusal.contains("missing row object"),
            "the refusal must name the damage, not be a bare error: {refusal}"
        );
        for answer in &answers {
            assert!(
                answer
                    .as_ref()
                    .is_ok_and(|decision| *decision == AbsenceDecision::DeferredAbsence)
                    || answer.is_err(),
                "a missing row is never permission to recreate a deleted file: {answers:?}"
            );
        }
        assert!(
            fixture.summary_object_paths().is_empty(),
            "a proven-damaged index must retire its roots so the next open repairs"
        );
        drop(probe);

        // The next engine open repairs from retained receipts and answers both
        // identities exactly. Nothing was resurrected at any point.
        let mut repaired = fixture.absence_engine();
        repaired
            .open_absence_decision_map(&fixture.receipts)
            .unwrap();
        let stats = repaired
            .receiver_absence_summary_open_stats_for_test()
            .expect("the managed path records its open attribution");
        assert_eq!(
            stats.full_catalog_passes, 1,
            "the repair must be one named counted pass, not a silent reopen: {stats:?}"
        );
        assert!(stats.rebuilt);
        assert_eq!(
            repaired
                .receiver_absence_decision(fixture.ids.page_a, &gone)
                .unwrap(),
            AbsenceDecision::DeferredAbsence,
            "the deleted page must still defer after the repair"
        );
        assert_eq!(
            repaired
                .receiver_absence_decision(fixture.ids.page_b, &present)
                .unwrap(),
            AbsenceDecision::DeferredAbsence
        );
        assert_eq!(repaired.resident_absence_row_count_for_test(), 0);
        assert_eq!(deleted.page_id(), fixture.ids.page_a);
        assert_eq!(kept.page_id(), fixture.ids.page_b);

        // And the repaired index is the ordinary bounded path again.
        drop(repaired);
        let mut steady = fixture.absence_engine();
        steady.open_absence_decision_map(&fixture.receipts).unwrap();
        let steady_stats = steady
            .receiver_absence_summary_open_stats_for_test()
            .expect("the managed path records its open attribution");
        assert_eq!(steady_stats.full_catalog_passes, 0, "{steady_stats:?}");
        assert_eq!(
            steady
                .receiver_absence_decision(fixture.ids.page_a, &gone)
                .unwrap(),
            AbsenceDecision::DeferredAbsence
        );
    }

    /// Historical path enumeration must not come back into the prune policy.
    ///
    /// The measured tests above would catch a re-enumeration, but only for the
    /// shapes they build. This names the rule at the one site that has to hold
    /// it: the policy asks its questions per *local* key, through the point
    /// API, and never materializes a receiver history or completion vector.
    #[test]
    fn the_prune_policy_asks_point_questions_and_never_enumerates_history() {
        let source = include_str!("hot_engine.rs");
        let body = source
            .split_once("pub(crate) fn flush_local_projection_completions(")
            .expect("the own-endpoint flush has one producer")
            .1
            .split_once("\n    }\n")
            .expect("its body is brace-terminated")
            .0;
        assert!(
            body.contains("observed_page_paths()"),
            "the policy must be driven by the local index's own keys"
        );
        assert!(
            body.contains("receiver_row_anchors(&key)"),
            "the policy must ask its receiver question one exact key at a time"
        );
        for forbidden in [
            "resident_receiver_rows",
            "receiver_summary_entries",
            "receiver_history_paths",
            "receiver_completion_anchors",
            "validated_catalog",
        ] {
            assert!(
                !body.contains(forbidden),
                "the own-endpoint prune policy must not enumerate receiver \
history through {forbidden}"
            );
        }
    }

    /// Retained own work is what keeps an entry, not history. A retained intent
    /// on a path that is no longer live survives the prune; the same entry
    /// without the retention does not.
    #[test]
    fn retained_own_work_survives_the_prune_and_unretained_history_does_not() {
        let mut opened = open("p3-own-completion-retention");
        let ids = opened.ids;
        let engine = &mut opened.engine;
        let retained = intent(
            ids.workspace,
            ids.page_a,
            "pages/A-retired.md",
            5,
            ProjectionTargetKind::Absent,
        );
        let dropped = intent(
            ids.workspace,
            ids.page_b,
            "pages/B-retired.md",
            6,
            ProjectionTargetKind::Absent,
        );
        assert!(engine.stage_local_projection_completion(&retained).unwrap());
        assert!(engine.stage_local_projection_completion(&dropped).unwrap());
        let keep = BTreeSet::from([retained.id().unwrap()]);
        assert!(engine.flush_local_projection_completions(keep).unwrap());
        assert_eq!(engine.local_completion_entry_count_for_test(), 1);
        assert!(engine
            .local_completed_projection_intent_ids()
            .contains(&retained.id().unwrap()));
        assert!(!engine
            .local_completed_projection_intent_ids()
            .contains(&dropped.id().unwrap()));
    }
}

// Persistent CRDT writer lanes (rebaselining prerequisite P1).
//
// The rebaselining design bounds resident state by `P` = participating peer
// identities. A fresh peer per batch makes `P` grow with edit count and keeps
// that growth inside every shallow snapshot and every later manifest's before
// vector, so sealing can never retire it. These tests hold the engine half of
// the repair: one persistent lane per (device, role), and receiver-side lane
// ownership that is derived from first admitted use, installed atomically with
// the batch that used it, and enforced on every later use.
// ---------------------------------------------------------------------------

/// One author batch that deliberately does NOT tie its device to its peer, so a
/// test can construct the duplicated-writer-identity case that
/// `author()` cannot express. Its causal writer incarnation is the device's
/// stable fixture incarnation, i.e. "this device never lost its record".
fn author_on_lane(batch: u128, device: u128, peer: u64) -> AuthorBatch {
    author_on_incarnation(batch, device, peer, b"stable")
}

/// The same, with an EXPLICIT writer incarnation label.
///
/// This is how a fixture expresses what production expresses by saving a random
/// `WriterIncarnationId`: the enrolled device is unchanged, while a distinct
/// label is a distinct authoring incarnation — exactly what a lost or torn
/// private writer record produces. Two labels are two causal chains.
fn author_on_incarnation(batch: u128, device: u128, peer: u64, incarnation: &[u8]) -> AuthorBatch {
    AuthorBatch {
        batch_id: BatchId::from_uuid(uuid(batch)),
        author_device_id: DeviceId::from_uuid(uuid(device)),
        author_session_id: SessionId::from_uuid(uuid(2_000 + device)),
        crdt_peer_id: CrdtPeerId::from_u64(peer),
        causal_peer_id: CausalPeerId::from_key(WriterIncarnationId::fixture_labelled(
            &[&device.to_be_bytes()[..], incarnation].concat(),
        )),
    }
}

fn block_in(
    engine: &ShardedHotEngine,
    author: AuthorBatch,
    block: crate::oplog::BlockId,
    page_id: PageId,
    home_document_id: DocumentId,
    order: &str,
    content: &str,
) -> PreparedBatch {
    engine
        .prepare_fixture_transaction(
            author,
            &tx(vec![SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id: block,
                    home_document_id: test_block_home(block),
                },
                page_id,
                parent: None,
                order: order.into(),
                content: content.into(),
            }]),
        )
        .unwrap()
}

/// A long local burst on ONE lane leaves exactly one writer entry in the edited
/// document, with strictly increasing counters. This is the property the whole
/// repair exists for: under the retired fresh-peer-per-batch rule the same
/// program leaves one entry per batch, forever, even after a shallow cut.
#[test]
fn a_local_burst_on_one_writer_lane_keeps_one_peer_and_strictly_increasing_counters() {
    let ids = Ids::new();
    let dir = TestDir::new("writer-lane-burst");
    let archive = store(&dir, ids);
    let (mut engine, _) = seed_engine(ids, &archive);
    let lane = 0xA11E_0001_u64;
    let device = 5_001_u128;

    let mut previous_counter = 0_i32;
    for index in 0..64_u128 {
        let prepared = engine
            .prepare_fixture_transaction(
                author_on_lane(70_000 + index, device, lane),
                &tx(vec![SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: ids.block_a,
                        home_document_id: ids.block_home_a(),
                    },
                    content: format!("burst {index}"),
                }]),
            )
            .unwrap();
        let batch = ready(&archive, &prepared);
        assert!(
            matches!(
                engine.stage_ready(batch).disposition,
                BatchDisposition::Accepted { .. }
            ),
            "burst batch {index} is accepted on the persistent lane"
        );
        let document = engine
            .accepted_document_version_vector_for_test(DocumentKey::Entity(ids.block_home_a()))
            .unwrap();
        let counter = document
            .iter()
            .find(|(peer, _)| **peer == lane)
            .map(|(_, counter)| *counter)
            .expect("the lane advanced the edited document");
        assert!(
            counter > previous_counter,
            "lane counters must strictly increase: {previous_counter} -> {counter}"
        );
        previous_counter = counter;
    }

    let peers = engine
        .accepted_document_peer_ids_for_test(DocumentKey::Entity(ids.block_home_a()))
        .unwrap();
    assert_eq!(
        peers.iter().filter(|peer| peer.as_u64() == lane).count(),
        1,
        "one lane entry after 64 batches, not one per batch: {peers:?}"
    );
    // The genesis author plus this one lane. A fresh peer per batch would have
    // left 65 entries here and in every later manifest before-vector.
    assert!(peers.len() <= 2, "unexpected writer growth: {peers:?}");
    assert_eq!(engine.crdt_lane_count(), 2, "genesis author plus one lane");
}

/// Another author's lane is refused, in either delivery order, and the refusal
/// happens before any live CRDT document changes. Exactly one of two forks of
/// the same lane identity survives — the duplicated-writer-identity case.
#[test]
fn a_foreign_or_forked_writer_lane_is_refused_without_changing_any_document() {
    for reversed in [false, true] {
        let ids = Ids::new();
        let dir = TestDir::new(if reversed {
            "writer-lane-foreign-reversed"
        } else {
            "writer-lane-foreign"
        });
        let archive = store(&dir, ids);
        let (mut engine, _) = seed_engine(ids, &archive);
        let lane = 0xA11E_0002_u64;

        // Two devices claim the same CRDT writer lane, in unrelated documents,
        // so neither batch depends on the other's state.
        let owner = block_in(
            &engine,
            author_on_lane(71_001, 5_101, lane),
            crate::oplog::BlockId::from_uuid(uuid(81_001)),
            ids.page_a,
            ids.home_a,
            "z1",
            "owner edit",
        );
        // The impostor is prepared on a peer engine that has not yet seen the
        // owner, exactly as a cloned device identity would produce it.
        let impostor = block_in(
            &seed_engine(ids, &archive).0,
            author_on_lane(71_002, 5_102, lane),
            crate::oplog::BlockId::from_uuid(uuid(81_002)),
            ids.page_c,
            ids.home_c,
            "z2",
            "impostor edit",
        );
        let (first, second) = if reversed {
            (&impostor, &owner)
        } else {
            (&owner, &impostor)
        };
        let first_device = first.manifest().author_device_id();

        let accepted = ready(&archive, first);
        assert!(matches!(
            engine.stage_ready(accepted).disposition,
            BatchDisposition::Accepted { .. }
        ));
        let owned = engine
            .crdt_lane_owner(CrdtPeerId::from_u64(lane))
            .expect("first admitted use binds the lane");
        assert_eq!(owned.device_id, first_device);

        let target_document = if reversed { ids.home_a } else { ids.home_c };
        let before_pages = engine
            .accepted_document_peer_ids_for_test(DocumentKey::Entity(target_document))
            .unwrap();

        let refused = ready(&archive, second);
        let disposition = engine.stage_ready(refused).disposition;
        match disposition {
            BatchDisposition::Rejected {
                error: EngineError::CrdtLaneNotOwned { peer_id, owned, .. },
            } => {
                assert_eq!(peer_id, CrdtPeerId::from_u64(lane));
                assert_eq!(owned.device_id, first_device);
            }
            other => panic!("a foreign writer lane must be refused, found {other:?}"),
        }
        // Whole-batch atomicity: no CRDT fragment of the refused batch became
        // accepted, and the ownership binding did not move.
        assert_eq!(
            engine
                .accepted_document_peer_ids_for_test(DocumentKey::Entity(target_document))
                .unwrap(),
            before_pages
        );
        assert_eq!(
            engine
                .crdt_lane_owner(CrdtPeerId::from_u64(lane))
                .unwrap()
                .device_id,
            first_device
        );
        // The refused original stays in the archive for recovery.
        assert!(matches!(
            archive.inspect_batch(second.manifest().batch_id()).unwrap(),
            BatchInspection::Ready(_)
        ));
    }
}

/// Incoming edits cannot turn a reserved baseline identity into a device lane.
#[test]
fn delivered_batches_cannot_claim_reserved_genesis_writer_peers() {
    for lane in [0x5449_4e45_4745_4e31, 0x5449_4e45_4745_4e32] {
        let ids = Ids::new();
        let dir = TestDir::new("writer-lane-reserved-genesis");
        let archive = store(&dir, ids);
        let (mut engine, _) = seed_engine(ids, &archive);
        let before = engine
            .accepted_document_version_vector_for_test(DocumentKey::Entity(ids.home_a))
            .unwrap();
        let lane_count = engine.crdt_lane_count();
        let peer_count = engine.causal_peer_count();
        let prepared = block_in(
            &engine,
            author_on_lane(71_101, 5_111, lane),
            crate::oplog::BlockId::from_uuid(uuid(81_101)),
            ids.page_a,
            ids.home_a,
            "z1",
            "reserved peer edit",
        );
        let outcome = engine.stage_ready(ready(&archive, &prepared)).disposition;
        assert!(
            matches!(
                outcome,
                BatchDisposition::Rejected {
                    error: EngineError::CrdtLaneUnauthorizedOrigin { .. },
                }
            ),
            "delivered reserved writer peer {lane:x} must be refused: {outcome:?}"
        );
        assert_eq!(
            engine
                .accepted_document_version_vector_for_test(DocumentKey::Entity(ids.home_a))
                .unwrap(),
            before
        );
        assert_eq!(engine.crdt_lane_count(), lane_count);
        assert_eq!(engine.causal_peer_count(), peer_count);
        assert!(matches!(
            archive
                .inspect_batch(prepared.manifest().batch_id())
                .unwrap(),
            BatchInspection::Ready(_)
        ));
    }
}

/// One device's ordinary lane and its external-import lane are separate
/// identities, and neither may be advanced under the other's role.
#[test]
fn one_device_keeps_separate_local_and_external_import_lanes() {
    let ids = Ids::new();
    let dir = TestDir::new("writer-lane-roles");
    let archive = store(&dir, ids);
    let (mut engine, _) = seed_engine(ids, &archive);
    let seed_lane_count = engine.crdt_lane_count();
    let device = 5_201_u128;
    let local_lane = 0xA11E_0003_u64;

    let local = block_in(
        &engine,
        author_on_lane(72_001, device, local_lane),
        crate::oplog::BlockId::from_uuid(uuid(82_001)),
        ids.page_a,
        ids.home_a,
        "r1",
        "local edit",
    );
    let batch = ready(&archive, &local);
    assert!(matches!(
        engine.stage_ready(batch).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let owner = engine
        .crdt_lane_owner(CrdtPeerId::from_u64(local_lane))
        .unwrap();
    assert_eq!(owner.device_id, DeviceId::from_uuid(uuid(device)));
    assert_eq!(owner.role, crate::oplog::writer_lane::WriterRole::Local);

    // The same device cannot reuse its ordinary lane as an import lane: role
    // separation is what keeps external-editor provenance separable, and it is
    // enforced by the same accepted-state binding, at draft time, before any
    // batch material is assembled.
    let reused = engine.prepare_fixture_transaction_with_origin(
        author_on_lane(72_002, device, local_lane),
        BatchOrigin::ExternalReconciliation {
            import_id: crate::oplog::ImportId::from_digest([9; 32]),
        },
        &tx(vec![SemanticOperation::EditBlockContent {
            block: BlockLocation {
                block_id: ids.block_a,
                home_document_id: test_block_home(ids.block_a),
            },
            content: "import edit".into(),
        }]),
    );
    match reused {
        Err(EngineError::CrdtLaneNotOwned { owned, claimed, .. }) => {
            assert_eq!(owned.role, crate::oplog::writer_lane::WriterRole::Local);
            assert_eq!(
                claimed.role,
                crate::oplog::writer_lane::WriterRole::External
            );
            assert_eq!(owned.device_id, claimed.device_id);
        }
        Err(other) => panic!("role separation must be enforced, found {other:?}"),
        Ok(_) => panic!("role separation must be enforced"),
    }

    // The ordinary lane itself keeps working under its own role: refusal was
    // about the role, not about the lane having been used before.
    let again = block_in(
        &engine,
        author_on_lane(72_003, device, local_lane),
        crate::oplog::BlockId::from_uuid(uuid(82_003)),
        ids.page_a,
        ids.home_a,
        "r3",
        "second local edit",
    );
    let batch = ready(&archive, &again);
    assert!(matches!(
        engine.stage_ready(batch).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(
        engine.crdt_lane_count(),
        seed_lane_count + 1,
        "reusing the admitted local lane must not register another lane; genesis retains its own \
         owner"
    );
}

/// Manager `p1-causal-loss-review/regression-test.rs`, incorporated with
/// explicit fixture incarnation selection.
///
/// One enrolled device publishes work, then loses its private writer record and
/// reopens an OLDER graph copy that cannot see that work. The rebuild mints a
/// fresh writer incarnation (here: an explicit distinct fixture label, which is
/// what the durable record's random `WriterIncarnationId` produces in
/// production), so the two independently prepared publications must NOT land on
/// one `BatchCausalDot` — which is precisely the alias the manager's isolated
/// control reproduced. Both branches then deliver in either arrival order, keep
/// their exact original IDs, and bind to the SAME enrolled author device.
#[test]
fn manager_lost_writer_record_and_stale_archive_preserve_both_original_branches() {
    for reversed in [false, true] {
        let ids = Ids::new();
        let dir = TestDir::new(if reversed {
            "manager-lost-writer-causal-chain-reversed"
        } else {
            "manager-lost-writer-causal-chain"
        });
        let archive = store(&dir, ids);
        let (mut original, genesis) = seed_engine(ids, &archive);
        let device = 5_501_u128;
        let old_prefix = original
            .prepare_fixture_transaction(
                author_on_incarnation(75_001, device, 0xA11E_0015, b"before-loss"),
                &tx(vec![SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: ids.block_a,
                        home_document_id: ids.block_home_a(),
                    },
                    content: "acknowledged before private state loss".into(),
                }]),
            )
            .unwrap();
        assert!(matches!(
            original
                .stage_ready(ready(&archive, &old_prefix))
                .disposition,
            BatchDisposition::Accepted { .. }
        ));

        // Reopening an OLDER graph copy with the writer record lost mints a
        // fresh Loro lane AND a fresh causal writer incarnation, while the
        // enrolled device identity is unchanged.
        let mut stale_rebuild = ids.engine();
        assert!(matches!(
            stale_rebuild.stage_ready(genesis.clone()).disposition,
            BatchDisposition::Accepted { .. }
        ));
        let new_work = stale_rebuild
            .prepare_fixture_transaction(
                author_on_incarnation(75_002, device, 0xA11E_0016, b"after-loss"),
                &tx(vec![SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: ids.block_a,
                        home_document_id: ids.block_home_a(),
                    },
                    content: "acknowledged after private state loss".into(),
                }]),
            )
            .unwrap();
        assert_ne!(
            old_prefix.manifest().causal_dot(),
            new_work.manifest().causal_dot(),
            "independent acknowledged publications by fresh lanes must not share one Tine causal \
             identity after private record loss"
        );
        assert_ne!(
            old_prefix.manifest().causal_dot().peer_id(),
            new_work.manifest().causal_dot().peer_id(),
            "the fresh incarnation is a distinct causal peer, not a reused counter"
        );
        assert_eq!(
            old_prefix.manifest().author_device_id(),
            new_work.manifest().author_device_id(),
            "only the lost writer incarnation changes; the enrolled author device does not"
        );

        let mut receiver = ids.engine();
        assert!(matches!(
            receiver.stage_ready(genesis.clone()).disposition,
            BatchDisposition::Accepted { .. }
        ));
        let seed_lane_count = receiver.crdt_lane_count();
        let seed_incarnations = receiver.causal_peer_count();
        let (first, second) = if reversed {
            (&new_work, &old_prefix)
        } else {
            (&old_prefix, &new_work)
        };
        assert!(matches!(
            receiver.stage_ready(ready(&archive, first)).disposition,
            BatchDisposition::Accepted { .. }
        ));
        let result = receiver.stage_ready(ready(&archive, second)).disposition;
        assert!(
            matches!(result, BatchDisposition::Accepted { .. }),
            "fresh CRDT lanes after private record loss must not alias the old Tine causal dot; \
             reversed={reversed}, old={:?}, new={:?}, result={result:?}",
            old_prefix.manifest().causal_dot(),
            new_work.manifest().causal_dot()
        );

        // Both originals are present with their exact IDs; neither branch had
        // to wait for the other, and neither erased the other's obligations.
        let accepted = receiver.status().accepted_batch_ids().unwrap();
        for prepared in [&old_prefix, &new_work] {
            assert!(
                accepted.contains(&prepared.manifest().batch_id()),
                "the original batch id survives delivery: {:?}",
                prepared.manifest().batch_id()
            );
        }
        let peers = receiver
            .accepted_document_peer_ids_for_test(DocumentKey::Entity(ids.block_home_a()))
            .unwrap();
        for lane in [0xA11E_0015_u64, 0xA11E_0016_u64] {
            assert!(
                peers.contains(&CrdtPeerId::from_u64(lane)),
                "both the retired and the fresh lane advanced the document: {peers:?}"
            );
            let owner = receiver
                .crdt_lane_owner(CrdtPeerId::from_u64(lane))
                .unwrap_or_else(|| panic!("lane {lane:x} is bound by its first admitted use"));
            assert_eq!(owner.device_id, DeviceId::from_uuid(uuid(device)));
            assert_eq!(owner.role, crate::oplog::writer_lane::WriterRole::Local);
        }
        // Both incarnations bind to the one enrolled author device, and the
        // rebuild costs exactly one additional writer identity — the
        // bounded-by-incarnations `P` term, not a per-batch term.
        for prepared in [&old_prefix, &new_work] {
            assert_eq!(
                receiver.causal_peer_owner(prepared.manifest().causal_dot().peer_id()),
                Some(DeviceId::from_uuid(uuid(device))),
                "a causal writer incarnation binds to its enrolled author device"
            );
        }
        assert_eq!(receiver.crdt_lane_count(), seed_lane_count + 2);
        assert_eq!(
            receiver.causal_peer_count(),
            seed_incarnations + 2,
            "one real writer incarnation change costs exactly one P entry"
        );
    }
}

/// The other half of the same boundary: WITHIN one incarnation, a conflicting
/// same-dot claim is a fork and is refused before any live fragment, ownership
/// or effect change — while an ordinary duplicate replay of the original bytes
/// still succeeds. Sparse counter containment cannot tell these apart; the
/// exact accepted per-peer tip can.
#[test]
fn a_same_incarnation_same_dot_fork_is_refused_and_the_original_bytes_are_retained() {
    let ids = Ids::new();
    let dir = TestDir::new("same-dot-fork");
    let archive = store(&dir, ids);
    let (mut author, genesis) = seed_engine(ids, &archive);
    let device = 5_601_u128;

    let original = block_in(
        &author,
        author_on_incarnation(76_001, device, 0xA11E_0017, b"one"),
        crate::oplog::BlockId::from_uuid(uuid(86_001)),
        ids.page_a,
        ids.home_a,
        "f1",
        "original bytes",
    );
    assert!(matches!(
        author.stage_ready(ready(&archive, &original)).disposition,
        BatchDisposition::Accepted { .. }
    ));

    // A second engine that never saw the original prepares a DIFFERENT batch on
    // the SAME incarnation, so it lands on the same (peer, counter). This is
    // what a duplicated or forged private writer record produces.
    let mut forker = ids.engine();
    assert!(matches!(
        forker.stage_ready(genesis.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let fork = block_in(
        &forker,
        author_on_incarnation(76_002, device, 0xA11E_0017, b"one"),
        crate::oplog::BlockId::from_uuid(uuid(86_002)),
        ids.page_a,
        ids.home_a,
        "f2",
        "forked bytes",
    );
    assert_eq!(
        fork.manifest().causal_dot(),
        original.manifest().causal_dot(),
        "the fixture really does construct a same-dot conflict"
    );
    assert_ne!(fork.manifest().batch_id(), original.manifest().batch_id());

    let mut receiver = ids.engine();
    assert!(matches!(
        receiver.stage_ready(genesis).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let original_ready = ready(&archive, &original);
    assert!(matches!(
        receiver.stage_ready(original_ready.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let before = receiver.canonical_snapshot().unwrap();
    let accepted_before = receiver.status().accepted_batch_ids().unwrap();

    let refused = receiver.stage_ready(ready(&archive, &fork)).disposition;
    assert!(
        matches!(
            refused,
            BatchDisposition::Rejected {
                error: EngineError::CausalDotFork { .. }
            }
        ),
        "a conflicting same-dot claim is a fork, not a duplicate: {refused:?}"
    );
    // Nothing accepted moved, and no fragment of any accepted document changed.
    assert_eq!(receiver.canonical_snapshot().unwrap(), before);
    assert_eq!(
        receiver.status().accepted_batch_ids().unwrap(),
        accepted_before
    );
    // The rejected original bytes are preserved in the archive, unmodified.
    assert!(matches!(
        archive.inspect_batch(fork.manifest().batch_id()).unwrap(),
        BatchInspection::Ready(_)
    ));
    // And a normal duplicate replay of the real original still succeeds.
    assert!(matches!(
        receiver.stage_ready(original_ready).disposition,
        BatchDisposition::DuplicateAccepted { .. }
    ));
}

/// A foreign device claiming an incarnation this graph already bound is refused
/// through the same accepted-ownership machinery, atomically and before any
/// visible change. The incarnation stays bound to its original author.
#[test]
fn a_foreign_device_cannot_claim_an_already_bound_causal_writer_incarnation() {
    let ids = Ids::new();
    let dir = TestDir::new("foreign-causal-key");
    let archive = store(&dir, ids);
    let (mut author, genesis) = seed_engine(ids, &archive);
    let owner_device = 5_701_u128;
    let thief_device = 5_702_u128;
    let incarnation = b"shared-label";

    let owned = block_in(
        &author,
        author_on_incarnation(77_001, owner_device, 0xA11E_0018, incarnation),
        crate::oplog::BlockId::from_uuid(uuid(87_001)),
        ids.page_a,
        ids.home_a,
        "t1",
        "owner edit",
    );
    assert!(matches!(
        author.stage_ready(ready(&archive, &owned)).disposition,
        BatchDisposition::Accepted { .. }
    ));

    // A different enrolled device authors under the SAME causal key. Its Loro
    // lane is its own, so only the causal identity is stolen.
    let mut thief = ids.engine();
    assert!(matches!(
        thief.stage_ready(genesis.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let mut stolen_author = author_on_incarnation(77_002, thief_device, 0xA11E_0019, b"thief");
    stolen_author.causal_peer_id = owned.manifest().causal_dot().peer_id();
    let stolen = block_in(
        &thief,
        stolen_author,
        crate::oplog::BlockId::from_uuid(uuid(87_002)),
        ids.page_a,
        ids.home_a,
        "t2",
        "stolen causal key",
    );
    assert_eq!(
        stolen.manifest().causal_dot().peer_id(),
        owned.manifest().causal_dot().peer_id()
    );
    assert_ne!(
        stolen.manifest().author_device_id(),
        owned.manifest().author_device_id()
    );

    let mut receiver = ids.engine();
    assert!(matches!(
        receiver.stage_ready(genesis).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        receiver.stage_ready(ready(&archive, &owned)).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let before = receiver.canonical_snapshot().unwrap();
    let refused = receiver.stage_ready(ready(&archive, &stolen)).disposition;
    assert!(
        matches!(
            refused,
            BatchDisposition::Rejected {
                error: EngineError::CausalPeerNotOwned { .. }
            }
        ),
        "another author may not advance a bound writer incarnation: {refused:?}"
    );
    assert_eq!(receiver.canonical_snapshot().unwrap(), before);
    assert_eq!(
        receiver.causal_peer_owner(owned.manifest().causal_dot().peer_id()),
        Some(DeviceId::from_uuid(uuid(owner_device))),
        "the refused claim did not rebind the incarnation"
    );
    assert!(matches!(
        archive.inspect_batch(stolen.manifest().batch_id()).unwrap(),
        BatchInspection::Ready(_)
    ));
}

/// Lane ownership is rebuilt by full accepted replay: a receiver that opened
/// without a checkpoint still knows who owns every lane.
#[test]
fn writer_lane_ownership_is_rebuilt_by_full_accepted_replay() {
    let ids = Ids::new();
    let dir = TestDir::new("writer-lane-restore");
    let archive = store(&dir, ids);
    let (mut engine, genesis_batch) = seed_engine(ids, &archive);
    let lane = 0xA11E_0004_u64;
    let device = 5_301_u128;

    let owner = block_in(
        &engine,
        author_on_lane(73_001, device, lane),
        crate::oplog::BlockId::from_uuid(uuid(83_001)),
        ids.page_a,
        ids.home_a,
        "s1",
        "owner edit",
    );
    let batch = ready(&archive, &owner);
    assert!(matches!(
        engine.stage_ready(batch).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let expected = engine.crdt_lane_owner(CrdtPeerId::from_u64(lane)).unwrap();

    // Full accepted replay rebuilds the same map from original bytes. (The
    // checkpoint-restored half of the same property is proved separately, on
    // the checkpoint state section itself.)
    let mut replayed = ids.engine();
    let genesis_ready = match archive
        .inspect_batch(genesis_batch.manifest().batch_id())
        .unwrap()
    {
        BatchInspection::Ready(batch) => batch,
        other => panic!("genesis remains Ready: {other:?}"),
    };
    assert!(matches!(
        replayed.stage_ready(genesis_ready).disposition,
        BatchDisposition::Accepted { .. }
    ));
    let owner_ready = match archive.inspect_batch(owner.manifest().batch_id()).unwrap() {
        BatchInspection::Ready(batch) => batch,
        other => panic!("owner batch remains Ready: {other:?}"),
    };
    assert!(matches!(
        replayed.stage_ready(owner_ready).disposition,
        BatchDisposition::Accepted { .. }
    ));
    assert_eq!(
        replayed.crdt_lane_owner(CrdtPeerId::from_u64(lane)),
        Some(expected),
        "full accepted replay rebuilds lane ownership"
    );
}
