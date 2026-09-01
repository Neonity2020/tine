use std::path::{Path, PathBuf};

use crate::oplog::{
    AuthorBatch, BatchCausalDot, BatchDisposition, BatchError, BatchId, BatchInspection,
    BatchOrigin, BlockDelta, BlockLocation, BlockOwner, BlockRestore, CausalPeerId,
    ConflictResolutionIntent, ContentDigest, CrdtPeerCounter, CrdtPeerId, DeviceId,
    DocumentCausalDigest, DocumentDependencies, DocumentId, EngineError, FrontierV2,
    ImmutableHomeClaim, ImmutableHomeConflict, ImmutableHomeEvidence, LineageDigest,
    LogseqIdentityMutation, LogseqIdentityOrigin, LogseqIdentityTrigger, LogseqUuid,
    LogseqUuidResolution, ManagedPath, ManagedTextKind, MembershipClaim, MembershipDelta,
    ObjectKind, ObjectStore, OperationBatch, OperationObject, OperationTransaction, PageDelta,
    PageId, PagePreambleDelta, PagePreambleState, PageState, PolicyGeneratedAnchorReason,
    PreparedBatch, ProjectionEndpointBinding, ProjectionEndpointId, ProjectionReceiptStore,
    SemanticEffect, SemanticEffectDigest, SemanticError, SemanticOperation, SessionId,
    ShardedHotEngine, StoreError, ValidatedBatch, WorkspaceId, WorkspaceStatus,
    MANAGED_ENTITY_SET_VERSION, OPERATION_SCHEMA_VERSION, SEMANTIC_EFFECT_SCHEMA_VERSION,
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
}

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn path(value: &str) -> ManagedPath {
    ManagedPath::parse(value).unwrap()
}

fn author(batch: u128, peer: u64) -> AuthorBatch {
    AuthorBatch {
        batch_id: BatchId::from_uuid(uuid(batch)),
        author_device_id: DeviceId::from_uuid(uuid(1_000 + peer as u128)),
        author_session_id: SessionId::from_uuid(uuid(2_000 + peer as u128)),
        crdt_peer_id: CrdtPeerId::from_u64(peer),
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
                        home_document_id: ids.home_a,
                    },
                    page_id: ids.page_a,
                    parent: None,
                    order: "a".into(),
                    content: "home A content".into(),
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id: ids.block_c,
                        home_document_id: ids.home_c,
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
                            home_document_id: *home_document_id,
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
                    home_document_id: ids.home_a,
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
    assert!(matches!(
        engine.stage_ready(revived.clone()).disposition,
        BatchDisposition::Accepted { .. }
    ));
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
                    home_document_id: ids.home_a,
                },
                content: "concurrent remote edit during revival".into(),
            }]),
        )
        .unwrap();
    let remote_edit = ready(&archive, &remote_edit);

    let converge = |first: ValidatedBatch, second: ValidatedBatch| {
        let mut peer = ids.engine();
        for batch in [baseline.clone(), deletion.clone(), first, second] {
            assert!(matches!(
                peer.stage_ready(batch).disposition,
                BatchDisposition::Accepted { .. }
            ));
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

    let create_catalog_payload: TestCrdtUpdatePayload = postcard::from_bytes(
        create
            .objects()
            .iter()
            .find(|object| {
                object.kind() == ObjectKind::CrdtUpdate && object.document_id() == ids.catalog
            })
            .unwrap()
            .payload(),
    )
    .unwrap();
    let catalog = LoroDoc::new();
    assert!(catalog
        .import(&create_catalog_payload.raw_update)
        .unwrap()
        .pending
        .is_none());
    let catalog_before = catalog.oplog_vv();
    catalog.set_peer_id(42_001).unwrap();
    catalog
        .get_map("pages")
        .insert(
            &ids.page_a.to_string(),
            serde_json::to_string(&PageState::Tombstone {
                name: crate::oplog::LogicalPageName::parse("A").unwrap(),
                home_document_id: ids.home_a,
                kind: ManagedTextKind::Journal,
            })
            .unwrap(),
        )
        .unwrap();
    catalog.commit();
    let forged_catalog_update = catalog
        .export(ExportMode::updates(&catalog_before))
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
            ObjectKind::CrdtUpdate if object.document_id() == ids.catalog => {
                let mut payload: TestCrdtUpdatePayload =
                    postcard::from_bytes(object.payload()).unwrap();
                payload.raw_update = forged_catalog_update.clone();
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
                        home_document_id: ids.home_a,
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
    let authorization = durable.authorize_projection_write(ids.page_a).unwrap();
    assert_eq!(authorization.state().page.page_id, ids.page_a);
    assert!(!authorization.state().frontier.documents().is_empty());
    assert!(authorization
        .state()
        .frontier
        .documents()
        .iter()
        .flat_map(|document| document.direct_dependency_heads())
        .all(|head| *head == batch_id));
}

#[test]
fn logseq_uuid_assignment_is_explicit_idempotent_replaceable_and_removable() {
    let ids = Ids::new();
    let dir = TestDir::new("logseq-uuid-lifecycle");
    let archive = store(&dir, ids);
    let (mut engine, _) = seed_engine(ids, &archive);
    let block = BlockLocation {
        block_id: ids.block_a,
        home_document_id: ids.home_a,
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
        home_document_id: ids.home_a,
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
        ab.recover_block_state(ids.home_a, ids.block_a)
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
                    home_document_id: ids.home_a,
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
            .recover_block_state(ids.home_a, ids.block_a)
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
                        home_document_id: ids.home_a,
                    },
                    mutation: LogseqIdentityMutation::Generate {
                        logseq_uuid: assigned_uuid,
                        trigger: LogseqIdentityTrigger::BlockReference {
                            referrer: BlockLocation {
                                block_id: ids.block_c,
                                home_document_id: ids.home_c,
                            },
                        },
                    },
                },
                SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: ids.block_c,
                        home_document_id: ids.home_c,
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
    assert_eq!(updated_documents, vec![ids.home_a, ids.home_c]);
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
    assert_eq!(page_a_documents, vec![ids.catalog, ids.home_a]);
    assert!(page_a
        .frontier
        .documents()
        .iter()
        .find(|document| document.document_id() == ids.home_a)
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
    assert_eq!(page_c_documents, vec![ids.catalog, ids.home_a, ids.home_c]);
    assert!(page_c
        .frontier
        .documents()
        .iter()
        .find(|document| document.document_id() == ids.home_c)
        .unwrap()
        .direct_dependency_heads()
        .contains(&anchored_id));
    assert!(!page_a_documents.contains(&ids.home_c));
    assert!(page_c_documents.contains(&ids.home_a));
}

#[test]
fn policy_generated_identity_requires_typed_same_batch_content_or_user_action() {
    let ids = Ids::new();
    let dir = TestDir::new("typed-logseq-triggers");
    let archive = store(&dir, ids);
    let (mut engine, _) = seed_engine(ids, &archive);
    let target = BlockLocation {
        block_id: ids.block_a,
        home_document_id: ids.home_a,
    };
    let referrer = BlockLocation {
        block_id: ids.block_c,
        home_document_id: ids.home_c,
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
                        home_document_id: ids.home_b,
                    },
                    page_id: ids.page_b,
                    parent: None,
                    order: "exported".into(),
                    content: "explicit export target".into(),
                },
                SemanticOperation::MutateBlockLogseqIdentity {
                    block: BlockLocation {
                        block_id: exported_block,
                        home_document_id: ids.home_b,
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
                    home_document_id: ids.home_b,
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
                home_document_id: ids.home_a,
            },
            mutation: LogseqIdentityMutation::AssignExternal {
                logseq_uuid: duplicate,
            },
        }]),
        author(44_005, 44_005),
        tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
            block: BlockLocation {
                block_id: block_b,
                home_document_id: ids.home_b,
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
                    home_document_id: ids.home_c,
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
                    home_document_id: ids.home_b,
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
    assert!(unique_documents.contains(&ids.home_a));
    assert!(unique_documents.contains(&ids.home_b));
    assert_eq!(unique_frontier.claim_evidence.len(), 1);
    assert_eq!(unique_frontier.claim_evidence[0].participants().len(), 2);

    let remove_a = ab
        .prepare_fixture_transaction(
            author(44_008, 44_008),
            &tx(vec![SemanticOperation::MutateBlockLogseqIdentity {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: ids.home_a,
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
    assert!(removed_documents.contains(&ids.home_a));
    assert!(removed_documents.contains(&ids.home_b));
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
                        home_document_id: ids.home_a,
                    },
                    mutation: LogseqIdentityMutation::AssignExternal {
                        logseq_uuid: claimed,
                    },
                },
                SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: ids.block_c,
                        home_document_id: ids.home_c,
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
        .any(|document| document.document_id() == ids.home_a));
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
                home_document_id: ids.home_a,
            },
            page_id: ids.page_a,
            parent: None,
            order: format!("scale-{index:04}"),
            content: format!("scaled block {index}"),
        });
        operations.push(SemanticOperation::MutateBlockLogseqIdentity {
            block: BlockLocation {
                block_id,
                home_document_id: ids.home_a,
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
            if claim.block_id == target_block && claim.home_document_id == ids.home_a
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
        BatchCausalDot::new(CausalPeerId::from_device_id(manifest.author_device_id()), 1).unwrap(),
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
        BatchCausalDot::new(CausalPeerId::from_device_id(manifest.author_device_id()), 1).unwrap(),
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
    document_id: DocumentId,
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
                    home_document_id: ids.home_a,
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
    assert_eq!(page.blocks[0].home_document_id, ids.home_a);
    assert_eq!(page.blocks[0].content, "home A content");
    assert_eq!(page.stats.catalog_documents_loaded, 1);
    assert_eq!(page.stats.membership_documents_loaded, 1);
    assert_eq!(page.stats.distinct_home_documents, vec![ids.home_a]);
    assert!(!page.stats.distinct_home_documents.contains(&ids.home_c));
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
                    home_document_id: ids.home_c,
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
    assert_eq!(page.stats.distinct_home_documents, vec![ids.home_a]);

    let dependent = engine
        .prepare_fixture_transaction(
            author(108, 108),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: ids.block_a,
                    home_document_id: ids.home_a,
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
    const PAGES: usize = 70;
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
                home_document_id,
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
    assert!(engine.instrumentation().document_hot_entries <= 65);

    let cold_page = PageId::from_uuid(uuid(80_000));
    let cold_home = DocumentId::from_uuid(uuid(81_000));
    let cold_block = crate::oplog::BlockId::from_uuid(uuid(82_000));
    let edit = author_engine
        .prepare_fixture_transaction(
            author(83_001, 83_000),
            &tx(vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id: cold_block,
                    home_document_id: cold_home,
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
    assert!(instrumentation.document_hot_entries <= 65);

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
    assert!(replay.instrumentation().document_hot_entries <= 65);
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
                    home_document_id: cold_home,
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
    assert!(replay.instrumentation().document_hot_entries <= 65);
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
                home_document_id: ids.home_a,
            },
            content: "concurrent left text".into(),
        }]),
        author(83_103, 83_103),
        tx(vec![SemanticOperation::EditBlockContent {
            block: BlockLocation {
                block_id: ids.block_a,
                home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_b,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_b,
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
        ImmutableHomeClaim::new(batch_a_id, ids.home_a),
        ImmutableHomeClaim::new(batch_b_id, ids.home_b),
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
            engine.recover_block_state(ids.home_a, block_id),
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
            ImmutableHomeClaim::new(prepared_a.manifest().batch_id(), ids.home_a),
            ImmutableHomeClaim::new(prepared_b.manifest().batch_id(), ids.home_b),
        ),
        ImmutableHomeConflict::new(
            block_y,
            ImmutableHomeClaim::new(prepared_b.manifest().batch_id(), ids.home_a),
            ImmutableHomeClaim::new(prepared_a.manifest().batch_id(), ids.home_b),
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
                    home_document_id: ids.home_a,
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
            ImmutableHomeClaim::new(BatchId::from_uuid(uuid(211)), ids.home_a),
            ImmutableHomeClaim::new(BatchId::from_uuid(uuid(212)), ids.home_b),
            ImmutableHomeClaim::new(BatchId::from_uuid(uuid(213)), ids.home_c),
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
            ImmutableHomeClaim::new(BatchId::from_uuid(uuid(241)), ids.home_a),
            ImmutableHomeClaim::new(BatchId::from_uuid(uuid(242)), ids.home_b),
        ),
        ImmutableHomeConflict::new(
            block_y,
            ImmutableHomeClaim::new(BatchId::from_uuid(uuid(243)), ids.home_a),
            ImmutableHomeClaim::new(BatchId::from_uuid(uuid(244)), ids.home_b),
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
        assert!(instrumentation.document_hot_entries <= 65);
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
    let child = create_blocks(
        &chain_author,
        259,
        &[(conflict_y, ids.page_a, ids.home_a, "child")],
    );
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
            ImmutableHomeClaim::new(BatchId::from_uuid(uuid(251)), ids.home_a),
            ImmutableHomeClaim::new(BatchId::from_uuid(uuid(252)), ids.home_b),
        ),
        ImmutableHomeConflict::new(
            conflict_y,
            ImmutableHomeClaim::new(BatchId::from_uuid(uuid(259)), ids.home_a),
            ImmutableHomeClaim::new(BatchId::from_uuid(uuid(253)), ids.home_b),
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
                    home_document_id: ids.home_a,
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
        .find(|entry| entry.document_id() == ids.home_a)
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
                    home_document_id: ids.home_a,
                },
                page_id: ids.page_a,
                parent: None,
                order: "a".into(),
                content: "a".into(),
            },
            SemanticOperation::CreateBlock {
                block: BlockLocation {
                    block_id,
                    home_document_id: ids.home_b,
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
    let claim_b = create_blocks(
        &author_b,
        233,
        &[(conflict_id, ids.page_b, ids.home_b, "b")],
    );
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
                        home_document_id: ids.home_a,
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
                        home_document_id: ids.home_a,
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
                        home_document_id: ids.home_a,
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
    assert!(matches!(
        engine.stage_ready(ready(&archive, &renamed)).disposition,
        BatchDisposition::Accepted { .. }
    ));
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
                        home_document_id: ids.home_a,
                    },
                    page_id: ids.page_a,
                    parent: None,
                    order: "a".into(),
                    content: "see [[Area/Child]]".into(),
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id: child_block,
                        home_document_id: ids.home_b,
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
                block_rewrites: vec![
                    crate::oplog::BlockContentRewrite {
                        block: BlockLocation {
                            block_id: ids.block_a,
                            home_document_id: ids.home_a,
                        },
                        new_content: "see [[Domain/Child]]".into(),
                    },
                    crate::oplog::BlockContentRewrite {
                        block: BlockLocation {
                            block_id: child_block,
                            home_document_id: ids.home_b,
                        },
                        new_content: "back to [[Domain]]".into(),
                    },
                ],
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
        home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
            home_document_id: ids.home_a,
        },
        from_page_id: ids.page_a,
        to_page_id: ids.page_b,
        parent: None,
        order: "b".into(),
    }]);
    let move_c = tx(vec![SemanticOperation::MoveSubtree {
        root: BlockLocation {
            block_id: ids.block_a,
            home_document_id: ids.home_a,
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
            home_document_id: ids.home_a,
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
            home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
        assert_eq!(block.home_document_id, ids.home_a);
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
            home_document_id: ids.home_a,
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
        .recover_block_state(ids.home_a, ids.block_a)
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
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
        } if document_id == ids.home_a
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
                    home_document_id: ids.home_c,
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
                    home_document_id: ids.home_a,
                },
                content: "target edit".into(),
            }]),
        )
        .unwrap();
    let exact = &target.manifest().dependency_frontier().documents()[0];
    assert_eq!(exact.document_id(), ids.home_a);
    let smuggled = DocumentDependencies::new(
        ids.home_a,
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
        } if document_id == ids.home_a
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
                home_document_id: ids.home_a,
            },
            content: "randomized concurrent edit".into(),
        }]),
        tx(vec![SemanticOperation::MoveSubtree {
            root: BlockLocation {
                block_id: ids.block_a,
                home_document_id: ids.home_a,
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
            home_document_id: ids.home_a,
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
                home_document_id: ids.home_a,
            },
            claim: MembershipClaim {
                home_document_id: ids.home_a,
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
                    home_document_id: ids.home_a,
                },
                claim: MembershipClaim {
                    home_document_id: ids.home_a,
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
            home_document_id: ids.home_a,
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
                home_document_id: ids.home_a,
            },
            claim: MembershipClaim {
                home_document_id: ids.home_a,
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
            home_document_id: ids.home_a,
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
            home_document_id: ids.home_c,
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
            home_document_id: ids.home_a,
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
            home_document_id: ids.home_a,
        },
        content: "home".into(),
    }]);
    let narrower = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: ids.home_a,
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
            home_document_id: ids.home_a,
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
                home_document_id: ids.home_a,
            },
            claim: MembershipClaim {
                home_document_id: ids.home_a,
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
            home_document_id: ids.home_a,
        },
        content: "first offline text".into(),
    }]);
    let second = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_a,
            home_document_id: ids.home_a,
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
            home_document_id: ids.home_a,
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
            home_document_id: ids.home_c,
        },
        content: "UNRELATED content".into(),
    }]);
    let suffix = tx(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: ids.block_c,
            home_document_id: ids.home_c,
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
                    home_document_id: a4_home_id(0),
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
                            home_document_id: a4_home_id(0),
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
