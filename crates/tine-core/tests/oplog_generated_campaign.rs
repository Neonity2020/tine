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
    ByteMutation, DeterministicSimulator, IngressExpectation, InvariantAssertion, ProviderLocation,
    ProviderSource, ProviderTree, ScheduledAction, ScheduledActionKind, StageExpectation,
    WireBatch, WireBytes, WireItem,
};
use tine_core::oplog::{
    AuthorBatch, BatchId, BlockId, BlockLocation, CrdtPeerId, DeviceId, DocumentId,
    FrozenCandidateId, LineageDigest, ManagedPath, ManagedTextKind, OperationTransaction, PageId,
    Scenario, ScenarioAction, ScenarioDevice, SemanticOperation, SessionId, ShardedHotEngine,
    WorkspaceId,
};
use uuid::Uuid;

const FAST_SEED: u64 = 70_600;
const FAST_COUNT: u64 = 16;
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

/// Produces only the two R6 starter families.  The bit keeps a seed's family
/// stable without adding a random dependency or a second protocol model.
fn generate(seed: u64) -> Scenario {
    if seed & 1 == 0 {
        generated_transport(seed)
    } else {
        generated_independent_page_edits(seed)
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

fn report(label: &str, seed: u64, result: &CampaignResult) {
    println!(
        "r6 generated campaign {label}: seed={seed} count={} runtime_ms={} scenarios_per_second={:.2}",
        result.count,
        result.elapsed.as_millis(),
        result.scenarios_per_second(),
    );
}

#[test]
fn generated_campaign_fast_default() {
    let result = run_campaign(FAST_SEED, FAST_COUNT, &fixture_candidate());
    report("fast", FAST_SEED, &result);
}

#[test]
fn generated_campaign_is_byte_identical_per_seed() {
    for seed in [0, 1, FAST_SEED, FAST_SEED + 1, u64::MAX] {
        assert_eq!(
            generate(seed).encode().unwrap(),
            generate(seed).encode().unwrap(),
            "seed {seed} was not byte-identical",
        );
    }
}

#[test]
fn generated_campaign_known_seed_range_replays_green() {
    let result = run_campaign(81_920, 8, &fixture_candidate());
    report("known-range", 81_920, &result);
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
    let result = run_campaign(seed, count, &frozen_candidate);
    report("burn-in", seed, &result);
}
