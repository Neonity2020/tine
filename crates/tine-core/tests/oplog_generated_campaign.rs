//! Small deterministic R6 generated campaigns over the production simulator.
//!
//! The generated scenarios deliberately stay in the existing scenario format:
//! their encoded bytes are both the replay trace and the failure input to the
//! exact-identity minimizer.  This module does not model merging itself.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tine_core::oplog::simulator::{
    ByteMutation, CoordinatorAction, CoordinatorFault, CoordinatorHandoffState, CoordinatorOracle,
    CoordinatorReadGate, CoordinatorRunOutcome, CoordinatorSqliteMutation, DeterministicSimulator,
    IngressExpectation, InvariantAssertion, ProviderLocation, ProviderSource, ProviderTree,
    ScheduledAction, ScheduledActionKind, StageExpectation, WireBatch, WireBytes, WireItem,
};
use tine_core::oplog::{
    AuthorBatch, BatchId, BlockId, BlockLocation, CrdtPeerId, DeviceId, DocumentId,
    FrozenCandidateId, LineageDigest, ManagedPath, ManagedTextKind, OperationTransaction, PageId,
    Scenario, ScenarioAction, ScenarioDevice, SemanticOperation, SessionId, ShardedHotEngine,
    WorkspaceId,
};
use uuid::Uuid;

const FAST_SEED: u64 = 70_600;
/// Twenty consecutive seeds exercise every four-way generated variant in all
/// five families while remaining small enough for the normal focused target.
const FAST_COUNT: u64 = 20;
const FIXTURE_CANDIDATE: &str = "1111111111111111111111111111111111111111";
const MAX_BURN_IN_COUNT: u64 = 100_000;

#[derive(Clone, Copy)]
struct Ids {
    workspace: WorkspaceId,
    lineage: LineageDigest,
    catalog: DocumentId,
    page_a: PageId,
    page_b: PageId,
    home_a: DocumentId,
    home_b: DocumentId,
    block_a: BlockId,
    block_b: BlockId,
}

impl Ids {
    fn for_seed(seed: u64) -> Self {
        Self {
            workspace: WorkspaceId::from_uuid(id(seed, 1)),
            lineage: LineageDigest::of(format!("r6-generated-campaign/{seed}").as_bytes()),
            catalog: DocumentId::from_uuid(id(seed, 2)),
            page_a: PageId::from_uuid(id(seed, 10)),
            page_b: PageId::from_uuid(id(seed, 11)),
            home_a: DocumentId::from_uuid(id(seed, 20)),
            home_b: DocumentId::from_uuid(id(seed, 21)),
            block_a: BlockId::from_uuid(id(seed, 30)),
            block_b: BlockId::from_uuid(id(seed, 31)),
        }
    }
}

fn id(seed: u64, tag: u64) -> Uuid {
    Uuid::from_u128((u128::from(seed) << 64) | u128::from(tag))
}

fn device(name: &str, tag: u64) -> ScenarioDevice {
    ScenarioDevice {
        name: name.into(),
        device_id: DeviceId::from_uuid(id(0, 1_000 + tag)),
        crdt_peer_id: CrdtPeerId::from_u64(tag),
    }
}

fn path(value: &str) -> ManagedPath {
    ManagedPath::parse(value).expect("generated paths are valid")
}

fn transaction(operations: Vec<SemanticOperation>) -> OperationTransaction {
    OperationTransaction::new(operations).expect("generated transaction is valid")
}

fn event(event_id: u64, action: ScheduledActionKind) -> ScheduledAction {
    ScheduledAction {
        event_id,
        tick: event_id,
        action,
    }
}

fn push(actions: &mut Vec<ScheduledAction>, next: &mut u64, action: ScheduledActionKind) {
    actions.push(event(*next, action));
    *next += 1;
}

fn coordinator(actions: &mut Vec<ScheduledAction>, next: &mut u64, action: CoordinatorAction) {
    push(
        actions,
        next,
        ScheduledActionKind::Coordinator {
            device: "alpha".into(),
            action,
        },
    );
}

fn authored_object_item(batch_id: BatchId) -> String {
    // This is the production simulator's dynamic AuthorLocal mailbox name for
    // object zero. Every non-empty semantic transaction has that object.
    format!("auth/{batch_id}/object/0")
}

fn legacy_deliver(
    actions: &mut Vec<ScheduledAction>,
    next: &mut u64,
    device: &str,
    batch_id: BatchId,
) {
    push(
        actions,
        next,
        ScheduledActionKind::LegacyDeliver {
            device: device.into(),
            batch_id,
        },
    );
}

fn duplicate_authored_object(
    actions: &mut Vec<ScheduledAction>,
    next: &mut u64,
    device: &str,
    batch_id: BatchId,
) {
    push(
        actions,
        next,
        ScheduledActionKind::DeliverItem {
            device: device.into(),
            item_id: authored_object_item(batch_id),
            mutation: ByteMutation::Exact,
            expected: Some(IngressExpectation::Accepted),
        },
    );
}

fn batch_id(seed: u64, tag: u64) -> BatchId {
    BatchId::from_uuid(id(seed, 100 + tag))
}

fn session_id(seed: u64, tag: u64) -> SessionId {
    SessionId::from_uuid(id(seed, 200 + tag))
}

fn workspace(ids: Ids) -> tine_core::oplog::simulator::ScenarioWorkspace {
    tine_core::oplog::simulator::ScenarioWorkspace {
        workspace_id: ids.workspace,
        lineage_digest: ids.lineage,
        catalog_document_id: ids.catalog,
    }
}

fn wire_batch(
    ids: Ids,
    batch_id: BatchId,
    peer: u64,
    transaction: OperationTransaction,
) -> WireBatch {
    let engine = ShardedHotEngine::new(ids.workspace, ids.lineage, ids.catalog);
    let prepared = engine
        .prepare_bootstrap_transaction(
            AuthorBatch {
                batch_id,
                author_device_id: DeviceId::from_uuid(id(0, 2_000 + peer)),
                author_session_id: session_id(0, peer),
                crdt_peer_id: CrdtPeerId::from_u64(peer),
            },
            &transaction,
        )
        .expect("generated wire batch is valid");
    let objects = prepared
        .objects()
        .iter()
        .enumerate()
        .map(|(index, object)| WireItem {
            item_id: format!("wire/{batch_id}/object/{index}"),
            bytes_b64: WireBytes(object.encode().expect("object encoding is canonical")),
        })
        .collect();
    WireBatch {
        name: format!("generated-{batch_id}"),
        batch_id,
        manifest: WireItem {
            item_id: format!("wire/{batch_id}/manifest"),
            bytes_b64: WireBytes(
                prepared
                    .manifest()
                    .encode()
                    .expect("manifest encoding is canonical"),
            ),
        },
        objects,
    }
}

/// Selects a stable production-simulator family without a random dependency
/// or a second protocol model.  `seed / 5` advances each family's variant
/// independently in consecutive five-seed corpus rounds.
fn generate(seed: u64) -> Scenario {
    match seed % 5 {
        0 => generated_transport(seed),
        1 => generated_independent_page_edits(seed),
        2 => generated_same_target_conflicts(seed),
        3 => generated_external_reconciliation(seed),
        4 => generated_sqlite_rebuild(seed),
        _ => unreachable!("seed modulo five is always within the family range"),
    }
}

fn generated_transport(seed: u64) -> Scenario {
    let ids = Ids::for_seed(seed);
    let batch = wire_batch(
        ids,
        batch_id(seed, 1),
        10,
        transaction(vec![SemanticOperation::CreatePage {
            page_id: ids.page_a,
            home_document_id: ids.home_a,
            name: tine_core::oplog::LogicalPageName::parse("Generated transport").unwrap(),
            path: path("pages/generated-transport.md"),
            kind: ManagedTextKind::Page,
        }]),
    );
    let mut actions = Vec::new();
    let mut next = 1;

    // Manifest-first provider visibility deliberately leaves beta incomplete.
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::ProviderCopy {
            source: ProviderSource::Mailbox {
                item_id: batch.manifest.item_id.clone(),
            },
            destination: ProviderLocation {
                device: "beta".into(),
                tree: ProviderTree::Inbox,
                path: "manifests/generated/manifest".into(),
            },
        },
    );
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::ReceiverRescan {
            device: "beta".into(),
        },
    );
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::ProbeBatch {
            device: "beta".into(),
            batch_id: batch.batch_id,
            expected: Some(StageExpectation::Incomplete),
        },
    );
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::SetProviderPartition {
            device: "beta".into(),
            partitioned: true,
        },
    );
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::ReceiverRescan {
            device: "beta".into(),
        },
    );
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::Crash {
            device: "beta".into(),
        },
    );
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::Restart {
            device: "beta".into(),
        },
    );
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::SetProviderPartition {
            device: "beta".into(),
            partitioned: false,
        },
    );

    // Objects appear in reverse order, then the first object is replayed
    // under a distinct provider name.  The production store must remain
    // idempotent when beta rescans both copies.
    for (index, object) in batch.objects.iter().enumerate().rev() {
        push(
            &mut actions,
            &mut next,
            ScheduledActionKind::ProviderCopy {
                source: ProviderSource::Mailbox {
                    item_id: object.item_id.clone(),
                },
                destination: ProviderLocation {
                    device: "beta".into(),
                    tree: ProviderTree::Inbox,
                    path: format!("objects/generated/{index}"),
                },
            },
        );
    }
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::ProviderCopy {
            source: ProviderSource::Mailbox {
                item_id: batch.objects[0].item_id.clone(),
            },
            destination: ProviderLocation {
                device: "beta".into(),
                tree: ProviderTree::Inbox,
                path: "objects/generated/replay-0".into(),
            },
        },
    );
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::ReceiverRescan {
            device: "beta".into(),
        },
    );
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::ProbeBatch {
            device: "beta".into(),
            batch_id: batch.batch_id,
            expected: Some(StageExpectation::Accepted),
        },
    );
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::ReceiverRescan {
            device: "beta".into(),
        },
    );

    // Alpha takes the opposing object-before-manifest delivery order.
    for object in &batch.objects {
        push(
            &mut actions,
            &mut next,
            ScheduledActionKind::DeliverItem {
                device: "alpha".into(),
                item_id: object.item_id.clone(),
                mutation: ByteMutation::Exact,
                expected: Some(IngressExpectation::Accepted),
            },
        );
    }
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::DeliverItem {
            device: "alpha".into(),
            item_id: batch.manifest.item_id.clone(),
            mutation: ByteMutation::Exact,
            expected: Some(IngressExpectation::Accepted),
        },
    );
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::ProbeBatch {
            device: "alpha".into(),
            batch_id: batch.batch_id,
            expected: Some(StageExpectation::Accepted),
        },
    );
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::AssertInvariant {
            assertion: InvariantAssertion::Converged {
                devices: vec!["alpha".into(), "beta".into()],
            },
        },
    );

    Scenario::from_schedule(
        "r6-generated-transport-v1",
        seed,
        workspace(ids),
        vec![device("alpha", 1), device("beta", 2)],
        vec![batch],
        Vec::new(),
        actions,
        Vec::new(),
        Vec::new(),
    )
    .expect("generated transport schedule is valid")
}

fn generated_independent_page_edits(seed: u64) -> Scenario {
    let ids = Ids::for_seed(seed);
    let root = batch_id(seed, 10);
    let alpha_edit = batch_id(seed, 11);
    let beta_edit = batch_id(seed, 12);
    Scenario::new(
        "r6-generated-independent-page-edits-v1",
        seed,
        ids.workspace,
        ids.lineage,
        ids.catalog,
        vec![device("alpha", 1), device("beta", 2)],
        vec![
            ScenarioAction::LocalTransaction {
                device: 0,
                batch_id: root,
                session_id: session_id(seed, 10),
                transaction: transaction(vec![
                    SemanticOperation::CreatePage {
                        page_id: ids.page_a,
                        home_document_id: ids.home_a,
                        name: tine_core::oplog::LogicalPageName::parse("Generated A").unwrap(),
                        path: path("pages/generated-a.md"),
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
                        content: "base-a".into(),
                    },
                    SemanticOperation::CreatePage {
                        page_id: ids.page_b,
                        home_document_id: ids.home_b,
                        name: tine_core::oplog::LogicalPageName::parse("Generated B").unwrap(),
                        path: path("pages/generated-b.md"),
                        kind: ManagedTextKind::Page,
                    },
                    SemanticOperation::CreateBlock {
                        block: BlockLocation {
                            block_id: ids.block_b,
                            home_document_id: ids.home_b,
                        },
                        page_id: ids.page_b,
                        parent: None,
                        order: "a".into(),
                        content: "base-b".into(),
                    },
                ]),
            },
            ScenarioAction::Deliver {
                device: 1,
                batch_id: root,
            },
            ScenarioAction::LocalTransaction {
                device: 0,
                batch_id: alpha_edit,
                session_id: session_id(seed, 11),
                transaction: transaction(vec![SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: ids.block_a,
                        home_document_id: ids.home_a,
                    },
                    content: format!("alpha independent edit {seed}"),
                }]),
            },
            ScenarioAction::LocalTransaction {
                device: 1,
                batch_id: beta_edit,
                session_id: session_id(seed, 12),
                transaction: transaction(vec![SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: ids.block_b,
                        home_document_id: ids.home_b,
                    },
                    content: format!("beta independent edit {seed}"),
                }]),
            },
            ScenarioAction::Deliver {
                device: 0,
                batch_id: beta_edit,
            },
            ScenarioAction::Deliver {
                device: 1,
                batch_id: alpha_edit,
            },
            ScenarioAction::Deliver {
                device: 1,
                batch_id: alpha_edit,
            },
            ScenarioAction::AssertConverged {
                devices: vec![0, 1],
            },
        ],
    )
    .expect("generated independent-edit schedule is valid")
}

fn same_target_root(ids: Ids) -> OperationTransaction {
    transaction(vec![
        SemanticOperation::CreatePage {
            page_id: ids.page_a,
            home_document_id: ids.home_a,
            name: tine_core::oplog::LogicalPageName::parse("Generated target").unwrap(),
            path: path("pages/generated-target.md"),
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
            content: "generated target base".into(),
        },
        SemanticOperation::CreatePage {
            page_id: ids.page_b,
            home_document_id: ids.home_b,
            name: tine_core::oplog::LogicalPageName::parse("Generated referrer").unwrap(),
            path: path("pages/generated-referrer.md"),
            kind: ManagedTextKind::Page,
        },
        SemanticOperation::CreateBlock {
            block: BlockLocation {
                block_id: ids.block_b,
                home_document_id: ids.home_b,
            },
            page_id: ids.page_b,
            parent: None,
            order: "a".into(),
            content: "referrer [[Generated target]]".into(),
        },
    ])
}

/// Covers the R6 same-target families through ordinary local transactions and
/// raw simulator deliveries.  Gamma and delta deliberately receive the two
/// concurrent batches in opposing orders; both also see one duplicate.
fn generated_same_target_conflicts(seed: u64) -> Scenario {
    let ids = Ids::for_seed(seed);
    let root = batch_id(seed, 20);
    let alpha_change = batch_id(seed, 21);
    let beta_change = batch_id(seed, 22);
    let variant = ((seed / 5) % 4) as u8;
    let target = BlockLocation {
        block_id: ids.block_a,
        home_document_id: ids.home_a,
    };
    let (alpha_transaction, beta_transaction) = match variant {
        // Same-block text/text.
        0 => (
            transaction(vec![SemanticOperation::EditBlockContent {
                block: target,
                content: format!("alpha same-block text {seed}"),
            }]),
            transaction(vec![SemanticOperation::EditBlockContent {
                block: target,
                content: format!("beta same-block text {seed}"),
            }]),
        ),
        // Edit/delete at the original owner location.
        1 => (
            transaction(vec![SemanticOperation::EditBlockContent {
                block: target,
                content: format!("alpha edit before delete {seed}"),
            }]),
            transaction(vec![SemanticOperation::DeleteSubtree {
                root_block_id: ids.block_a,
                page_id: ids.page_a,
            }]),
        ),
        // Move/delete at the original owner location.
        2 => (
            transaction(vec![SemanticOperation::MoveSubtree {
                root: target,
                from_page_id: ids.page_a,
                to_page_id: ids.page_b,
                parent: None,
                order: format!("moved-{seed}"),
            }]),
            transaction(vec![SemanticOperation::DeleteSubtree {
                root_block_id: ids.block_a,
                page_id: ids.page_a,
            }]),
        ),
        // Rename/rewritten referrer concurrent with an ordinary referrer edit.
        3 => {
            let renamed = format!("Generated target {seed}");
            (
                transaction(vec![SemanticOperation::RenamePagesAndRewriteReferrers {
                    page_changes: vec![tine_core::oplog::PageRename {
                        page_id: ids.page_a,
                        new_name: tine_core::oplog::LogicalPageName::parse(&renamed).unwrap(),
                        new_path: path(&format!("pages/generated-target-{seed}.md")),
                    }],
                    block_rewrites: vec![tine_core::oplog::BlockContentRewrite {
                        block: BlockLocation {
                            block_id: ids.block_b,
                            home_document_id: ids.home_b,
                        },
                        new_content: format!("referrer [[{renamed}]]"),
                    }],
                    page_preamble_rewrites: Vec::new(),
                }]),
                transaction(vec![SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id: ids.block_b,
                        home_document_id: ids.home_b,
                    },
                    content: format!("beta referrer edit [[Generated target]] {seed}"),
                }]),
            )
        }
        _ => unreachable!("same-target variant is modulo four"),
    };

    let mut actions = Vec::new();
    let mut next = 1;
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::AuthorLocal {
            device: "alpha".into(),
            batch_id: root,
            session_id: session_id(seed, 20),
            transaction: same_target_root(ids),
        },
    );
    for device in ["beta", "gamma", "delta"] {
        legacy_deliver(&mut actions, &mut next, device, root);
    }
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::AuthorLocal {
            device: "alpha".into(),
            batch_id: alpha_change,
            session_id: session_id(seed, 21),
            transaction: alpha_transaction,
        },
    );
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::AuthorLocal {
            device: "beta".into(),
            batch_id: beta_change,
            session_id: session_id(seed, 22),
            transaction: beta_transaction,
        },
    );

    if variant & 1 == 0 {
        duplicate_authored_object(&mut actions, &mut next, "gamma", alpha_change);
        legacy_deliver(&mut actions, &mut next, "gamma", alpha_change);
        legacy_deliver(&mut actions, &mut next, "gamma", beta_change);
        duplicate_authored_object(&mut actions, &mut next, "delta", beta_change);
        legacy_deliver(&mut actions, &mut next, "delta", beta_change);
        legacy_deliver(&mut actions, &mut next, "delta", alpha_change);
    } else {
        duplicate_authored_object(&mut actions, &mut next, "gamma", beta_change);
        legacy_deliver(&mut actions, &mut next, "gamma", beta_change);
        legacy_deliver(&mut actions, &mut next, "gamma", alpha_change);
        duplicate_authored_object(&mut actions, &mut next, "delta", alpha_change);
        legacy_deliver(&mut actions, &mut next, "delta", alpha_change);
        legacy_deliver(&mut actions, &mut next, "delta", beta_change);
    }
    duplicate_authored_object(&mut actions, &mut next, "alpha", beta_change);
    legacy_deliver(&mut actions, &mut next, "alpha", beta_change);
    duplicate_authored_object(&mut actions, &mut next, "beta", alpha_change);
    legacy_deliver(&mut actions, &mut next, "beta", alpha_change);
    push(
        &mut actions,
        &mut next,
        ScheduledActionKind::AssertInvariant {
            assertion: InvariantAssertion::Converged {
                devices: vec![
                    "alpha".into(),
                    "beta".into(),
                    "gamma".into(),
                    "delta".into(),
                ],
            },
        },
    );

    Scenario::from_schedule(
        "r6-generated-same-target-conflicts-v1",
        seed,
        workspace(ids),
        vec![
            device("alpha", 1),
            device("beta", 2),
            device("gamma", 3),
            device("delta", 4),
        ],
        Vec::new(),
        Vec::new(),
        actions,
        Vec::new(),
        Vec::new(),
    )
    .expect("generated same-target conflict schedule is valid")
}

fn completed_coordinator_oracle(sequence: u64) -> CoordinatorOracle {
    CoordinatorOracle {
        accepted_sequence: Some(sequence),
        sqlite_sequence: Some(sequence),
        frontiers_match: Some(true),
        pending_projection_work: Some(0),
        tail_unapplied_batches: Some(0),
        tail_retained_bytes: Some(0),
        handoff: Some(CoordinatorHandoffState::Released),
        read_gate: Some(CoordinatorReadGate::Open),
        last_outcome: Some(CoordinatorRunOutcome::Complete),
        ..CoordinatorOracle::default()
    }
}

fn generated_external_reconciliation(seed: u64) -> Scenario {
    let ids = Ids::for_seed(seed);
    let variant = ((seed / 5) % 4) as u8;
    let extension = if variant & 1 == 0 { "md" } else { "org" };
    let line_ending = if variant & 2 == 0 { "\n" } else { "\r\n" };
    let old = format!("content/pages/研究/über/r6-generated-{seed}.{extension}");
    let new = format!("content/pages/研究/über/renamed-{seed}.{extension}");
    let source = if extension == "md" {
        format!("- external markdown {seed}{line_ending}\t- nested{line_ending}")
    } else {
        format!("* external org {seed}{line_ending}** nested{line_ending}")
    };
    let source = WireBytes(source.into_bytes());
    let mut actions = Vec::new();
    let mut next = 1;

    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::Setup {
            managed_path: old.clone(),
            kind: ManagedTextKind::Page,
            config_edn: Some(WireBytes(
                b"{:pages-directory \"content/pages\"\n:journals-directory \"content/journals\"}\n"
                    .to_vec(),
            )),
        },
    );
    match variant {
        // Markdown/LF external write and an explicit no-op durable checkpoint.
        0 => {
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::ExternalWrite {
                    path: old.clone(),
                    bytes_b64: source,
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Execute {
                    paths: vec![old.clone()],
                    fault: None,
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Assert {
                    oracle: completed_coordinator_oracle(2),
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Checkpoint {
                    name: "external-written".into(),
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Execute {
                    paths: vec![old],
                    fault: None,
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::AssertDurableCheckpoint {
                    name: "external-written".into(),
                },
            );
        }
        // Org/LF rename keeps both old and new paths in the production plan.
        1 => {
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::ExternalRename {
                    from_path: old.clone(),
                    to_path: new.clone(),
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Execute {
                    paths: vec![old, new.clone()],
                    fault: None,
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Assert {
                    oracle: completed_coordinator_oracle(2),
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Checkpoint {
                    name: "external-renamed".into(),
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Execute {
                    paths: vec![new],
                    fault: None,
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::AssertDurableCheckpoint {
                    name: "external-renamed".into(),
                },
            );
        }
        // Markdown/CRLF deletion must end with the meaningful empty image.
        2 => {
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::ExternalDelete { path: old.clone() },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Execute {
                    paths: vec![old.clone()],
                    fault: None,
                },
            );
            let mut deleted = completed_coordinator_oracle(2);
            deleted.managed_files = Some(Vec::new());
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Assert { oracle: deleted },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Checkpoint {
                    name: "external-deleted".into(),
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Execute {
                    paths: vec![old],
                    fault: None,
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::AssertDurableCheckpoint {
                    name: "external-deleted".into(),
                },
            );
        }
        // Org/CRLF write interrupted during projection, then crash/reopen and
        // production retry of the persisted continuation.
        3 => {
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::ExternalWrite {
                    path: old.clone(),
                    bytes_b64: source,
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Execute {
                    paths: vec![old],
                    fault: Some(CoordinatorFault::DuringProjection),
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Assert {
                    oracle: CoordinatorOracle {
                        accepted_sequence: Some(2),
                        sqlite_sequence: Some(2),
                        frontiers_match: Some(true),
                        pending_projection_work: Some(1),
                        tail_unapplied_batches: Some(0),
                        tail_retained_bytes: Some(0),
                        handoff: Some(CoordinatorHandoffState::HeldFailedClosed),
                        read_gate: Some(CoordinatorReadGate::Open),
                        durable_boundary: Some(
                            tine_core::oplog::CoordinatorDurableBoundary::DuringProjection,
                        ),
                        last_outcome: Some(CoordinatorRunOutcome::FailedClosed {
                            phase: "ProjectionDrain".into(),
                        }),
                        ..CoordinatorOracle::default()
                    },
                },
            );
            coordinator(&mut actions, &mut next, CoordinatorAction::Crash);
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Assert {
                    oracle: CoordinatorOracle {
                        handoff: Some(CoordinatorHandoffState::EnrollmentPendingUnprotected),
                        durable_boundary: Some(
                            tine_core::oplog::CoordinatorDurableBoundary::DuringProjection,
                        ),
                        ..CoordinatorOracle::default()
                    },
                },
            );
            coordinator(&mut actions, &mut next, CoordinatorAction::Reopen);
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Retry { fault: None },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Assert {
                    oracle: CoordinatorOracle {
                        accepted_sequence: Some(2),
                        sqlite_sequence: Some(2),
                        frontiers_match: Some(true),
                        pending_projection_work: Some(0),
                        tail_unapplied_batches: Some(0),
                        tail_retained_bytes: Some(0),
                        handoff: Some(CoordinatorHandoffState::EnrollmentPendingUnprotected),
                        read_gate: Some(CoordinatorReadGate::Open),
                        last_outcome: Some(CoordinatorRunOutcome::Complete),
                        ..CoordinatorOracle::default()
                    },
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Checkpoint {
                    name: "external-crash-recovered".into(),
                },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::Retry { fault: None },
            );
            coordinator(
                &mut actions,
                &mut next,
                CoordinatorAction::AssertCheckpoint {
                    name: "external-crash-recovered".into(),
                },
            );
        }
        _ => unreachable!("external reconciliation variant is modulo four"),
    }

    Scenario::from_schedule(
        "r6-generated-coordinator-external-v1",
        seed,
        workspace(ids),
        vec![device("alpha", 1)],
        Vec::new(),
        Vec::new(),
        actions,
        Vec::new(),
        Vec::new(),
    )
    .expect("generated external coordinator schedule is valid")
}

fn generated_sqlite_rebuild(seed: u64) -> Scenario {
    let ids = Ids::for_seed(seed);
    let variant = ((seed / 5) % 4) as u8;
    let (mutation, interruption) = match variant {
        0 => (
            CoordinatorSqliteMutation::Delete,
            CoordinatorFault::DuringSqliteApply,
        ),
        1 => (
            CoordinatorSqliteMutation::Truncate { len: 0 },
            CoordinatorFault::AfterSqliteApply,
        ),
        2 => (
            CoordinatorSqliteMutation::Corrupt {
                offset: 0,
                mask: 0xff,
            },
            CoordinatorFault::BeforeProjection,
        ),
        3 => (
            CoordinatorSqliteMutation::StaleFrontier,
            CoordinatorFault::DuringProjection,
        ),
        _ => unreachable!("SQLite variant is modulo four"),
    };
    let path = format!("content/pages/研究/derived/r6-sqlite-{seed}.md");
    let first = WireBytes(format!("- SQLite baseline {seed}\n\t- nested\n").into_bytes());
    let interrupted = WireBytes(format!("- SQLite interrupted {seed}\n\t- nested\n").into_bytes());
    let mut actions = Vec::new();
    let mut next = 1;

    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::Setup {
            managed_path: path.clone(),
            kind: ManagedTextKind::Page,
            config_edn: Some(WireBytes(
                b"{:pages-directory \"content/pages\"\n:journals-directory \"content/journals\"}\n"
                    .to_vec(),
            )),
        },
    );
    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::ExternalWrite {
            path: path.clone(),
            bytes_b64: first,
        },
    );
    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::Execute {
            paths: vec![path.clone()],
            fault: None,
        },
    );
    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::Assert {
            oracle: completed_coordinator_oracle(2),
        },
    );
    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::ExternalWrite {
            path: path.clone(),
            bytes_b64: interrupted,
        },
    );
    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::Execute {
            paths: vec![path.clone()],
            fault: Some(interruption),
        },
    );
    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::Retry { fault: None },
    );
    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::Assert {
            oracle: completed_coordinator_oracle(3),
        },
    );
    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::Checkpoint {
            name: "saved-materialization".into(),
        },
    );
    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::Sqlite { mutation },
    );
    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::Assert {
            oracle: CoordinatorOracle {
                accepted_sequence: Some(3),
                frontiers_match: Some(false),
                read_gate: Some(CoordinatorReadGate::Closed),
                ..CoordinatorOracle::default()
            },
        },
    );
    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::Sqlite {
            mutation: CoordinatorSqliteMutation::Reopen,
        },
    );
    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::AssertAcceptedArchiveCheckpoint {
            name: "saved-materialization".into(),
        },
    );
    coordinator(
        &mut actions,
        &mut next,
        CoordinatorAction::AssertMaterializationCheckpoint {
            name: "saved-materialization".into(),
        },
    );

    Scenario::from_schedule(
        "r6-generated-coordinator-sqlite-rebuild-v1",
        seed,
        workspace(ids),
        vec![device("alpha", 1)],
        Vec::new(),
        Vec::new(),
        actions,
        Vec::new(),
        Vec::new(),
    )
    .expect("generated SQLite rebuild schedule is valid")
}

struct CampaignResult {
    count: u64,
    elapsed: Duration,
}

impl CampaignResult {
    fn scenarios_per_second(&self) -> f64 {
        self.count as f64 / self.elapsed.as_secs_f64().max(f64::MIN_POSITIVE)
    }
}

fn run_campaign(seed: u64, count: u64, frozen_candidate: &FrozenCandidateId) -> CampaignResult {
    let started = Instant::now();
    for offset in 0..count {
        run_scenario(generate(seed.wrapping_add(offset)), frozen_candidate);
    }
    CampaignResult {
        count,
        elapsed: started.elapsed(),
    }
}

fn run_scenario(scenario: Scenario, frozen_candidate: &FrozenCandidateId) {
    let mut simulator = match DeterministicSimulator::new(scenario.clone()) {
        Ok(simulator) => simulator,
        Err(error) => fail_with_artifacts(&scenario, frozen_candidate, &error.to_string()),
    };
    if let Err(error) = simulator.run() {
        fail_with_artifacts(&scenario, frozen_candidate, &error.to_string());
    }
}

fn artifact_dir() -> PathBuf {
    env::var_os("TINE_R6_CAMPAIGN_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/tine-r6-generated-campaign-artifacts"))
}

fn fixture_candidate() -> FrozenCandidateId {
    FrozenCandidateId::parse(FIXTURE_CANDIDATE).expect("the fixture candidate is valid")
}

fn required_frozen_candidate() -> FrozenCandidateId {
    let value = env::var("TINE_R6_FROZEN_CANDIDATE")
        .expect("ignored R6 burn-in requires TINE_R6_FROZEN_CANDIDATE");
    FrozenCandidateId::parse(value)
        .expect("TINE_R6_FROZEN_CANDIDATE must be a full lowercase Git SHA or SHA-256 digest")
}

fn fail_with_artifacts(
    scenario: &Scenario,
    frozen_candidate: &FrozenCandidateId,
    failure: &str,
) -> ! {
    let root = artifact_dir();
    let prefix = format!("seed-{:016x}-{}", scenario.seed, scenario.family);
    let write = |suffix: &str, bytes: &[u8]| {
        fs::create_dir_all(&root)
            .and_then(|()| fs::write(root.join(format!("{prefix}{suffix}")), bytes))
    };

    let mut receipts = Vec::new();
    receipts.push(
        write(
            ".scenario.json",
            &scenario.encode().expect("scenario remains canonical"),
        )
        .map(|()| "original scenario")
        .map_err(|error| error.to_string()),
    );
    receipts.push(
        write(
            ".metadata.txt",
            format!(
                "frozen_candidate={}\nfamily={}\nseed={}\nactions={}\nfailure={failure}\n",
                frozen_candidate.as_str(),
                scenario.family,
                scenario.seed,
                scenario.actions.len(),
            )
            .as_bytes(),
        )
        .map(|()| "failure metadata")
        .map_err(|error| error.to_string()),
    );
    match scenario.minimize_failure(frozen_candidate.clone()) {
        Ok(minimized) => {
            receipts.push(
                write(
                    ".minimized.scenario.json",
                    &minimized
                        .scenario
                        .encode()
                        .expect("minimized scenario remains canonical"),
                )
                .map(|()| "minimized scenario")
                .map_err(|error| error.to_string()),
            );
            receipts.push(
                write(
                    ".failure-capsule.json",
                    &minimized
                        .capsule
                        .encode()
                        .expect("failure capsule remains canonical"),
                )
                .map(|()| "failure capsule")
                .map_err(|error| error.to_string()),
            );
        }
        Err(minimization_error) => receipts.push(
            write(
                ".minimization.txt",
                format!("minimizer invoked but did not produce a capsule: {minimization_error}\n")
                    .as_bytes(),
            )
            .map(|()| "minimizer result")
            .map_err(|error| error.to_string()),
        ),
    }
    let write_errors = receipts
        .into_iter()
        .filter_map(Result::err)
        .collect::<Vec<_>>();
    if write_errors.is_empty() {
        panic!(
            "R6 generated campaign failed for {} seed {}; reproducible artifacts: {} ({failure})",
            scenario.family,
            scenario.seed,
            root.display(),
        );
    }
    panic!(
        "R6 generated campaign failed for {} seed {}; artifact write errors: {} ({failure})",
        scenario.family,
        scenario.seed,
        write_errors.join("; "),
    );
}

fn required_env_u64(name: &str) -> u64 {
    env::var(name)
        .unwrap_or_else(|_| panic!("ignored R6 burn-in requires {name}"))
        .parse()
        .unwrap_or_else(|_| panic!("{name} must be an unsigned decimal integer"))
}

fn report(label: &str, seed: u64, result: &CampaignResult, frozen_candidate: &FrozenCandidateId) {
    println!(
        "r6 generated campaign {label}: frozen_candidate={} seed={seed} count={} runtime_ms={} scenarios_per_second={:.2}",
        frozen_candidate.as_str(),
        result.count,
        result.elapsed.as_millis(),
        result.scenarios_per_second(),
    );
}

#[test]
fn generated_campaign_fast_default() {
    let frozen_candidate = fixture_candidate();
    let result = run_campaign(FAST_SEED, FAST_COUNT, &frozen_candidate);
    report("fast", FAST_SEED, &result, &frozen_candidate);
}

#[test]
fn generated_campaign_is_byte_identical_per_seed() {
    for base in [0, FAST_SEED, 81_920, u64::MAX - (FAST_COUNT - 1)] {
        for offset in 0..FAST_COUNT {
            let seed = base + offset;
            assert_eq!(
                generate(seed).encode().unwrap(),
                generate(seed).encode().unwrap(),
                "seed {seed} was not byte-identical",
            );
        }
    }
}

#[test]
fn generated_campaign_known_seed_range_replays_green() {
    let frozen_candidate = fixture_candidate();
    let result = run_campaign(81_920, FAST_COUNT, &frozen_candidate);
    report("known-range", 81_920, &result, &frozen_candidate);
}

#[test]
#[ignore = "explicitly bounded burn-in; set TINE_R6_CAMPAIGN_SEED and TINE_R6_CAMPAIGN_COUNT"]
fn generated_campaign_burn_in() {
    let seed = required_env_u64("TINE_R6_CAMPAIGN_SEED");
    let count = required_env_u64("TINE_R6_CAMPAIGN_COUNT");
    let frozen_candidate = required_frozen_candidate();
    assert!(
        (1..=MAX_BURN_IN_COUNT).contains(&count),
        "TINE_R6_CAMPAIGN_COUNT must be within 1..={MAX_BURN_IN_COUNT}",
    );
    assert!(
        seed.checked_add(count - 1).is_some(),
        "TINE_R6_CAMPAIGN_SEED plus TINE_R6_CAMPAIGN_COUNT must not wrap",
    );
    let result = run_campaign(seed, count, &frozen_candidate);
    report("burn-in", seed, &result, &frozen_candidate);
}
