use std::fs;
use std::path::{Path, PathBuf};

use crate::oplog::import::{
    commit_clean_activation, open_clean_activation, plan_clean_affected_import,
    prepare_clean_activation,
};
use crate::oplog::lazy_genesis::LAZY_GENESIS_BASELINE_DIRECTORY;
use crate::oplog::local_active::CleanLocalRuntime;
use crate::oplog::operational_coordinator::{CleanLocalMutationState, OperationalCoordinator};
use crate::oplog::projection::render_requested_page_document;
use crate::oplog::sqlite::{LeasedWorkspaceProjection, ProjectionClaim, WorkspaceRuntimeLease};
use crate::oplog::{
    classify_conflict_copy, inventory_affected, inventory_initial_shadow, AuthorBatch, BatchOrigin,
    BlobDescription, BlockId, BlockLocation, BlockMatchBasis, CrdtPeerId, DeviceId, DocumentId,
    ImportBlockReason, ImportPlan, ImportPlanStatus, LineageDigest, LogseqIdentityOrigin,
    LogseqUuid, ManagedPath, ManagedTextKind, MaterializationStats, MaterializedBlock,
    MaterializedPage, ObjectStore, OperationTransaction, PageId, PageMatchBasis, PreparedBatch,
    ProjectionEndpointBinding, ProjectionEndpointId, ProjectionReceiptStore, RawObservation,
    ReferenceCatalogPolicyV1, RejectedRawIdReason, SemanticOperation, SessionId, ShardedHotEngine,
    ValidatedBatch, WorkspaceId,
};
use crate::Graph;
use std::ops::Deref;
use uuid::Uuid;

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tine-oplog-import-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn workspace() -> WorkspaceId {
    WorkspaceId::from_uuid(uuid(1))
}

fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

#[derive(Clone)]
struct BlockSpec {
    content: String,
    parent: Option<usize>,
    order: String,
    logseq_uuid: Option<LogseqUuid>,
}

impl BlockSpec {
    fn root(content: impl Into<String>, order: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            parent: None,
            order: order.into(),
            logseq_uuid: None,
        }
    }

    fn child(content: impl Into<String>, parent: usize, order: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            parent: Some(parent),
            order: order.into(),
            logseq_uuid: None,
        }
    }

    fn with_uuid(mut self, logseq_uuid: LogseqUuid) -> Self {
        self.content = format!("{}\nid:: {logseq_uuid}", self.content);
        self.logseq_uuid = Some(logseq_uuid);
        self
    }
}

struct PageSpec {
    path: String,
    blocks: Vec<BlockSpec>,
    name: Option<String>,
    preamble: Option<String>,
}

struct PageAuthority {
    path: String,
    page_id: PageId,
    home_document_id: DocumentId,
    block_ids: Vec<BlockId>,
    projected: Vec<u8>,
}

struct AuthorityFixture {
    _dir: TestDir,
    graph_root: PathBuf,
    graph: Graph,
    receipts: ProjectionReceiptStore,
    engine: CleanAuthorityRuntime,
    pages: Vec<PageAuthority>,
}

struct CleanAuthorityRuntime(CleanLocalRuntime);

impl Deref for CleanAuthorityRuntime {
    type Target = ShardedHotEngine;

    fn deref(&self) -> &Self::Target {
        self.0.engine()
    }
}

impl AuthorityFixture {
    fn new(label: &str, pages: Vec<PageSpec>) -> Self {
        let dir = TestDir::new(label);
        let graph_root = dir.path().join("graph");
        fs::create_dir_all(graph_root.join("pages")).unwrap();
        fs::create_dir_all(graph_root.join("journals")).unwrap();
        let lineage = LineageDigest::of(b"oplog-import-authority");
        let catalog = DocumentId::from_uuid(uuid(200));
        let archive_path = dir.path().join("archive");
        fs::create_dir(&archive_path).unwrap();
        for (page_index, page) in pages.iter().enumerate() {
            let seed = 1_000 + page_index as u128 * 1_000;
            let page_id = PageId::from_uuid(uuid(seed));
            let home_document_id = DocumentId::from_uuid(uuid(seed + 1));
            let kind = match page.path.split_once('/') {
                Some(("pages", rest)) if !rest.is_empty() => ManagedTextKind::Page,
                Some(("journals", rest)) if !rest.is_empty() => ManagedTextKind::Journal,
                _ => panic!("import fixture path is outside the guarded default layout"),
            };
            let block_ids = (0..page.blocks.len())
                .map(|index| BlockId::from_uuid(uuid(seed + 10 + index as u128)))
                .collect::<Vec<_>>();
            let materialized = MaterializedPage {
                page_id,
                home_document_id,
                name: crate::oplog::LogicalPageName::parse(
                    page.name
                        .clone()
                        .unwrap_or_else(|| format!("Imported Page {page_index}")),
                )
                .unwrap(),
                path: ManagedPath::parse(page.path.clone()).unwrap(),
                kind,
                preamble: page.preamble.clone(),
                blocks: page
                    .blocks
                    .iter()
                    .enumerate()
                    .map(|(index, block)| MaterializedBlock {
                        block_id: block_ids[index],
                        home_document_id,
                        parent: block.parent.map(|parent| block_ids[parent]),
                        order: block.order.clone(),
                        logseq_uuid: block.logseq_uuid,
                        logseq_identity_origin: block
                            .logseq_uuid
                            .map(|_| LogseqIdentityOrigin::ExternalImported),
                        content: block.content.clone(),
                    })
                    .collect(),
                stats: MaterializationStats::default(),
            };
            let bytes = render_requested_page_document(&materialized, None).unwrap();
            write(&graph_root, &page.path, &bytes);
        }
        let graph = Graph::open(&graph_root);
        let capture_root = dir.path().join("capture");
        fs::create_dir(&capture_root).unwrap();
        let capture = graph
            .capture_inactive_bootstrap_sources(&capture_root)
            .unwrap();
        let database_path = dir.path().join("projection.sqlite");
        let preparation = prepare_clean_activation(
            &graph,
            capture,
            workspace(),
            lineage,
            catalog,
            &dir.path().join("preparation"),
            &database_path,
            &ReferenceCatalogPolicyV1::default(),
        )
        .unwrap();
        let mut authority = Vec::new();
        {
            let baseline = preparation.candidates().baseline();
            for page in &pages {
                let managed_path = ManagedPath::parse(page.path.clone()).unwrap();
                let materialized = baseline
                    .page_ids()
                    .filter_map(|page_id| baseline.page(page_id).unwrap())
                    .find(|candidate| candidate.path == managed_path)
                    .expect("clean baseline contains every authority fixture page");
                authority.push((
                    page.path.clone(),
                    materialized.page_id,
                    materialized.home_document_id,
                    materialized
                        .blocks
                        .iter()
                        .map(|block| block.block_id)
                        .collect::<Vec<_>>(),
                ));
            }
        }
        let committed = commit_clean_activation(
            &graph,
            preparation,
            &archive_path.join(LAZY_GENESIS_BASELINE_DIRECTORY),
            &dir.path().join("enrollment"),
        )
        .unwrap();
        let (_, _, baseline_frontier, _) = committed.into_parts();
        let reopened = open_clean_activation(
            &dir.path().join("enrollment"),
            &archive_path.join(LAZY_GENESIS_BASELINE_DIRECTORY),
            &database_path,
            catalog,
            ReferenceCatalogPolicyV1::default(),
        )
        .unwrap()
        .expect("clean import authority fixture reopens");
        let (mut clean_engine, projection, _) = reopened.into_parts();
        let operations_path = archive_path.join("operations");
        clean_engine
            .attach_clean_archive_store(ObjectStore::open(&operations_path, workspace()).unwrap())
            .unwrap();
        let writer = ObjectStore::open(&operations_path, workspace()).unwrap();
        let lease = WorkspaceRuntimeLease::acquire(&writer, workspace()).unwrap();
        let leased = LeasedWorkspaceProjection::adopt_clean_genesis(
            lease,
            &database_path,
            ProjectionClaim::current(workspace(), lineage),
            &baseline_frontier,
            &writer,
            &clean_engine,
            projection,
        )
        .map_err(|(_, error)| error)
        .unwrap();
        let endpoint = ProjectionEndpointBinding::enroll_graph(
            &graph,
            ProjectionEndpointId::from_uuid(uuid(100)),
            DeviceId::from_uuid(uuid(101)),
        )
        .unwrap();
        let receipts = ProjectionReceiptStore::open_for_endpoint(
            &dir.path().join("receipts"),
            workspace(),
            endpoint,
        )
        .unwrap();
        clean_engine
            .attach_clean_projection_endpoint(&graph, &receipts)
            .unwrap();
        let runtime = CleanLocalRuntime::from_open_parts(
            SessionId::from_uuid(uuid(302)),
            endpoint,
            clean_engine,
            leased,
        )
        .unwrap();

        let mut page_authorities = Vec::new();
        for (path, page_id, home_document_id, block_ids) in authority {
            let exact = fs::read(graph_root.join(&path)).unwrap();
            page_authorities.push(PageAuthority {
                path,
                page_id,
                home_document_id,
                block_ids,
                projected: exact,
            });
        }
        Self {
            _dir: dir,
            graph_root,
            graph,
            receipts,
            engine: CleanAuthorityRuntime(runtime),
            pages: page_authorities,
        }
    }

    fn one_page(label: &str, path: &str, blocks: Vec<BlockSpec>) -> Self {
        Self::new(
            label,
            vec![PageSpec {
                path: path.into(),
                blocks,
                name: None,
                preamble: None,
            }],
        )
    }

    fn one_titled_page(
        label: &str,
        path: &str,
        title: &str,
        preamble: &str,
        blocks: Vec<BlockSpec>,
    ) -> Self {
        Self::new(
            label,
            vec![PageSpec {
                path: path.into(),
                blocks,
                name: Some(title.into()),
                preamble: Some(preamble.into()),
            }],
        )
    }

    fn plan(&self, paths: &[&str]) -> ImportPlan {
        plan_clean_affected_import(
            &self.graph,
            self.engine.0.engine(),
            self.engine.0.database(),
            paths,
        )
    }

    fn overwrite(&self, path: &str, bytes: &[u8]) {
        write(&self.graph_root, path, bytes);
    }

    fn read(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.graph_root.join(path)).unwrap()
    }

    fn append_local_tail(&mut self, page: usize, block: usize, content: &str, _seed: u128) {
        let page = &self.pages[page];
        let transaction = OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
            block: BlockLocation {
                block_id: page.block_ids[block],
                home_document_id: page.home_document_id,
            },
            content: content.into(),
        }])
        .unwrap();
        self.execute_transaction(&transaction);
    }

    fn execute_transaction(&mut self, transaction: &OperationTransaction) {
        let mut projection_turns =
            crate::oplog::projection_turn_journal::open_scratch_projection_turn_journal_for(
                self.engine.0.engine(),
            );
        let mut session = self.engine.0.admit_clean_mutation(&self.graph).unwrap();
        match OperationalCoordinator::execute_clean_local(
            &mut session,
            &self.graph,
            &self.receipts,
            transaction,
            &mut projection_turns,
        )
        .unwrap()
        {
            CleanLocalMutationState::Complete(_) => {}
            CleanLocalMutationState::DurablePending(pending) => {
                panic!("clean local tail remained pending: {}", pending.failure())
            }
        }
    }

    fn prepare_transaction(
        &self,
        transaction: &OperationTransaction,
        batch: u128,
        peer: u64,
    ) -> PreparedBatch {
        let author = AuthorBatch {
            batch_id: crate::oplog::BatchId::from_uuid(uuid(batch)),
            author_device_id: DeviceId::from_uuid(uuid(101)),
            author_session_id: SessionId::from_uuid(uuid(batch + 10_000)),
            crdt_peer_id: CrdtPeerId::from_u64(peer),
        };
        let draft = self
            .engine
            .draft_author_transaction(author, BatchOrigin::LocalMutation, transaction)
            .unwrap();
        self.engine
            .finalize_author_transaction(
                draft,
                &self.graph,
                &self.receipts,
                self.engine.0.endpoint(),
            )
            .unwrap()
    }

    fn delete_and_project(&mut self, page: usize, _seed: u128) {
        let authority = &self.pages[page];
        let transaction = OperationTransaction::new(vec![SemanticOperation::DeletePage {
            page_id: authority.page_id,
        }])
        .unwrap();
        let mut projection_turns =
            crate::oplog::projection_turn_journal::open_scratch_projection_turn_journal_for(
                self.engine.0.engine(),
            );
        let mut session = self.engine.0.admit_clean_mutation(&self.graph).unwrap();
        match OperationalCoordinator::execute_clean_local(
            &mut session,
            &self.graph,
            &self.receipts,
            &transaction,
            &mut projection_turns,
        )
        .unwrap()
        {
            CleanLocalMutationState::Complete(_) => {}
            CleanLocalMutationState::DurablePending(pending) => {
                panic!("clean deletion remained pending: {}", pending.failure())
            }
        }
    }
}

fn blocked_reasons(plan: &ImportPlan) -> Vec<ImportBlockReason> {
    assert_eq!(plan.status(), ImportPlanStatus::Blocked, "{plan:?}");
    plan.blocks().iter().map(|block| block.reason).collect()
}

#[test]
fn lazy_genesis_full_integrity_accepts_a_sparse_frontier_overlay() {
    let mut fixture = AuthorityFixture::new(
        "lazy-genesis-sparse-integrity",
        vec![
            PageSpec {
                path: "pages/alpha.md".into(),
                blocks: vec![BlockSpec::root("alpha", "a")],
                name: Some("Alpha".into()),
                preamble: None,
            },
            PageSpec {
                path: "pages/beta.md".into(),
                blocks: vec![BlockSpec::root("beta", "a")],
                name: Some("Beta".into()),
                preamble: None,
            },
        ],
    );

    fixture.append_local_tail(0, 0, "alpha changed", 0x1400);

    let root = fixture.engine.0.database().frontier_root().unwrap();
    assert_eq!(root.document_count(), 3, "two pages plus the catalog");
    fixture
        .engine
        .0
        .database()
        .diagnose_full_integrity()
        .unwrap();
}

#[test]
fn ordinary_clean_save_and_move_never_reconstruct_the_accepted_frontier() {
    let mut fixture = AuthorityFixture::new(
        "ordinary-save-move-no-frontier-reconstruction",
        vec![
            PageSpec {
                path: "pages/source.md".into(),
                blocks: vec![BlockSpec::root("move me", "a")],
                name: Some("Source".into()),
                preamble: None,
            },
            PageSpec {
                path: "pages/destination.md".into(),
                blocks: vec![BlockSpec::root("destination", "a")],
                name: Some("Destination".into()),
                preamble: None,
            },
        ],
    );

    crate::oplog::hot_engine::reset_reconstruct_frontier_calls();
    fixture.append_local_tail(0, 0, "save without replay", 0x1401);
    assert_eq!(
        crate::oplog::hot_engine::reconstruct_frontier_calls(),
        0,
        "an ordinary save must use the current accepted state"
    );

    crate::oplog::hot_engine::reset_reconstruct_frontier_calls();
    let source = &fixture.pages[0];
    let destination = &fixture.pages[1];
    let moved = OperationTransaction::new(vec![SemanticOperation::MoveSubtree {
        root: BlockLocation {
            block_id: source.block_ids[0],
            home_document_id: source.home_document_id,
        },
        from_page_id: source.page_id,
        to_page_id: destination.page_id,
        parent: None,
        order: "b".into(),
    }])
    .unwrap();
    fixture.execute_transaction(&moved);
    assert_eq!(
        crate::oplog::hot_engine::reconstruct_frontier_calls(),
        0,
        "an ordinary cross-page move must use the current accepted state"
    );
}

#[test]
fn concurrent_same_page_fallback_never_publishes_the_stale_local_draft_as_current() {
    let mut fixture = AuthorityFixture::new(
        "concurrent-cross-home-author",
        vec![
            PageSpec {
                path: "pages/concurrent.md".into(),
                blocks: vec![BlockSpec::root("local base", "a")],
                name: Some("Concurrent".into()),
                preamble: None,
            },
            PageSpec {
                path: "pages/support.md".into(),
                blocks: vec![BlockSpec::root("remote base", "a")],
                name: Some("Support".into()),
                preamble: None,
            },
        ],
    );
    let local_page_id = fixture.pages[0].page_id;
    let local_home = fixture.pages[0].home_document_id;
    let local_block = fixture.pages[0].block_ids[0];
    let remote_page_id = fixture.pages[1].page_id;
    let remote_home = fixture.pages[1].home_document_id;
    let remote_block = fixture.pages[1].block_ids[0];
    let move_remote_home_into_local_page =
        OperationTransaction::new(vec![SemanticOperation::MoveSubtree {
            root: BlockLocation {
                block_id: remote_block,
                home_document_id: remote_home,
            },
            from_page_id: remote_page_id,
            to_page_id: local_page_id,
            parent: None,
            order: "b".into(),
        }])
        .unwrap();
    fixture.execute_transaction(&move_remote_home_into_local_page);

    let local_transaction = OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: local_block,
            home_document_id: local_home,
        },
        content: "local winner".into(),
    }])
    .unwrap();
    let remote_transaction = OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
        block: BlockLocation {
            block_id: remote_block,
            home_document_id: remote_home,
        },
        content: "remote winner".into(),
    }])
    .unwrap();

    // Both batches are authored against the same accepted base. Finalize the
    // remote author first, then the local author, so the local retained page
    // remains pending while the remote support document is accepted first.
    let remote = fixture.prepare_transaction(&remote_transaction, 0x1451, 0x1451);
    let local = fixture.prepare_transaction(&local_transaction, 0x1450, 0x1450);
    let local_batch = local.manifest().batch_id();

    let mut session = fixture
        .engine
        .0
        .admit_clean_mutation(&fixture.graph)
        .unwrap();
    let (_, engine, _) = session.parts().unwrap();
    assert!(matches!(
        engine
            .stage_ready(ValidatedBatch::new(remote))
            .disposition(),
        crate::oplog::BatchDisposition::Accepted { .. }
    ));
    assert!(matches!(
        engine.stage_ready(ValidatedBatch::new(local)).disposition(),
        crate::oplog::BatchDisposition::Accepted { .. }
    ));
    let root = engine.accepted_frontier_root().unwrap();
    let immutable = engine
        .accepted_root_materializer(&root)
        .unwrap()
        .materialize_page(local_page_id)
        .unwrap()
        .unwrap();
    let selected = match engine.accepted_author_projection_outcome(
        local_batch,
        root.state_digest(),
        local_page_id,
    ) {
        Some(page) => page,
        None => engine
            .accepted_root_materializer(&root)
            .unwrap()
            .materialize_page(local_page_id)
            .unwrap(),
    }
    .unwrap();

    assert_eq!(selected, immutable);
    assert_eq!(selected.blocks[0].content, "local winner");
    assert_eq!(selected.blocks[1].content, "remote winner");
}

/// Paired bootstrap/steady-state fixture constructor. The exact source bytes
/// first pass through the graph-wide initial inventory and independent
/// OG-compatible Graph loader, then through affected import planning.
fn assert_new_external_document_pair(
    fixture: &AuthorityFixture,
    path: &str,
    bytes: &[u8],
    expected_name: &str,
    expected_kind: ManagedTextKind,
) {
    fixture.overwrite(path, bytes);
    let oracle = Graph::open(&fixture.graph_root)
        .list_pages()
        .into_iter()
        .find(|entry| entry.rel_path == path)
        .unwrap();
    assert_eq!(oracle.name, expected_name);
    assert_eq!(
        match oracle.kind {
            crate::PageKind::Page => ManagedTextKind::Page,
            crate::PageKind::Journal => ManagedTextKind::Journal,
        },
        expected_kind
    );
    let initial = inventory_initial_shadow(&Graph::open(&fixture.graph_root)).unwrap();
    assert_eq!(initial.present(path).unwrap().bytes(), bytes);

    let plan = fixture.plan(&[path]);
    assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
    assert_eq!(plan.instrumentation().present_document_parses, 1);
    let diagnostic = format!("{plan:#?}");
    assert!(
        diagnostic.contains("CreatePage")
            && diagnostic.contains(expected_name)
            && diagnostic.contains(path)
            && diagnostic.contains(&format!("kind: {expected_kind:?}")),
        "bootstrap Graph oracle and affected import differ for {path}: {diagnostic}"
    );
}

#[test]
fn exact_inventory_preserves_lf_crlf_twins_nested_paths_and_explicit_absence() {
    let dir = TestDir::new("inventory");
    let nested_path = ManagedPath::parse("pages/topic/subtopic/archive/a.md").unwrap();
    let nested_bytes = b"- nested\n";
    fs::create_dir_all(dir.path().join("pages/topic/subtopic/archive")).unwrap();
    fs::create_dir_all(dir.path().join("journals")).unwrap();
    write(dir.path(), "pages/lf.md", b"- same\n");
    write(dir.path(), "pages/lf.org", b"* same\r\n");
    write(dir.path(), nested_path.as_str(), nested_bytes);
    let graph = Graph::open(dir.path());

    let inventory = inventory_affected(
        &graph,
        &[
            "pages/lf.md",
            "pages/lf.org",
            nested_path.as_str(),
            "pages/missing.md",
        ],
    )
    .unwrap();
    assert_eq!(
        inventory.present("pages/lf.md").unwrap().bytes(),
        b"- same\n"
    );
    assert_eq!(
        inventory.present("pages/lf.org").unwrap().bytes(),
        b"* same\r\n"
    );
    assert_ne!(
        inventory.present("pages/lf.md").unwrap().description(),
        inventory.present("pages/lf.org").unwrap().description()
    );
    assert!(matches!(
        inventory.entries().get(&nested_path),
        Some(RawObservation::Present(bytes)) if bytes.bytes() == nested_bytes
    ));
    assert!(matches!(
        inventory
            .entries()
            .get(&ManagedPath::parse("pages/missing.md").unwrap()),
        Some(RawObservation::Absent)
    ));

    let initial = inventory_initial_shadow(&graph).unwrap();
    assert_eq!(initial.entries().len(), 3);
    assert!(matches!(
        initial.entries().get(&nested_path),
        Some(RawObservation::Present(bytes)) if bytes.bytes() == nested_bytes
    ));
}

#[test]
fn two_id_properties_in_one_block_never_anchor_and_each_occurrence_is_reported() {
    let anchor = LogseqUuid::from_uuid(uuid(500));
    let fixture = AuthorityFixture::one_page(
        "two-ids",
        "pages/page.md",
        vec![BlockSpec::root("base", "a").with_uuid(anchor)],
    );
    fixture.overwrite(
        "pages/page.md",
        format!("- edited\n  id:: {anchor}\n  id:: {anchor}\n").as_bytes(),
    );

    let plan = fixture.plan(&["pages/page.md"]);
    let matches = plan
        .matches()
        .expect("conservative reconciliation remains plannable");
    assert!(matches.blocks().is_empty());
    assert_eq!(matches.rejected_raw_ids().len(), 2);
    assert!(matches
        .rejected_raw_ids()
        .iter()
        .all(|id| id.reason() == RejectedRawIdReason::Duplicate));
}

#[test]
fn invalid_raw_id_bytes_are_preserved_reported_and_never_authorize_identity() {
    let fixture = AuthorityFixture::one_page(
        "invalid-id",
        "pages/page.md",
        vec![BlockSpec::root("base", "a")],
    );
    let external = b"- base\n  id:: definitely-not-a-uuid\n";
    fixture.overwrite("pages/page.md", external);

    let plan = fixture.plan(&["pages/page.md"]);
    let matches = plan
        .matches()
        .expect("invalid identity degrades conservatively");
    assert!(matches.blocks().is_empty());
    assert_eq!(matches.rejected_raw_ids().len(), 1);
    assert_eq!(
        matches.rejected_raw_ids()[0].reason(),
        RejectedRawIdReason::InvalidSyntax
    );
    assert_eq!(
        fs::read(fixture.graph_root.join("pages/page.md")).unwrap(),
        external
    );
}

#[test]
fn unique_uuid_anchor_precedes_structure() {
    let anchor = LogseqUuid::from_uuid(uuid(510));
    let fixture = AuthorityFixture::one_page(
        "uuid-anchor",
        "pages/page.md",
        vec![
            BlockSpec::root("first", "a").with_uuid(anchor),
            BlockSpec::root("second", "b"),
        ],
    );
    fixture.overwrite(
        "pages/page.md",
        format!("- second\n- moved\n  id:: {anchor}\n").as_bytes(),
    );
    let plan = fixture.plan(&["pages/page.md"]);
    let anchored = plan
        .matches()
        .unwrap()
        .blocks()
        .iter()
        .find(|matched| matched.basis() == BlockMatchBasis::UniqueLogseqUuid)
        .unwrap();
    assert_eq!(anchored.block_id(), fixture.pages[0].block_ids[0]);
    assert_eq!(anchored.locator().components(), &[1]);
}

#[test]
fn one_idless_in_place_edit_retains_block_id_by_ordered_tree_alignment() {
    let fixture = AuthorityFixture::one_page(
        "idless-edit",
        "pages/page.md",
        vec![BlockSpec::root("before", "a")],
    );
    fixture.overwrite("pages/page.md", b"- after\n");

    let plan = fixture.plan(&["pages/page.md"]);
    let matched = &plan.matches().unwrap().blocks()[0];
    assert_eq!(matched.block_id(), fixture.pages[0].block_ids[0]);
    assert_eq!(
        matched.basis(),
        BlockMatchBasis::ReceiptOrderedTreeAlignment
    );
}

#[test]
fn copy_inserted_before_exact_anchor_does_not_steal_trailing_identity() {
    let fixture = AuthorityFixture::one_page(
        "ordered-copy",
        "pages/page.md",
        vec![BlockSpec::root("X", "a"), BlockSpec::root("A", "b")],
    );
    fixture.overwrite("pages/page.md", b"- A\n- X\n- A\n");

    let matches = fixture
        .plan(&["pages/page.md"])
        .matches()
        .unwrap()
        .to_owned();
    let by_locator = matches
        .blocks()
        .iter()
        .map(|matched| (matched.locator().components().to_vec(), matched.block_id()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert!(!by_locator.contains_key(&vec![0]));
    assert_eq!(by_locator[&vec![1]], fixture.pages[0].block_ids[0]);
    assert!(!by_locator.contains_key(&vec![2]));
}

#[test]
fn nested_edits_retain_structure_and_unequal_duplicate_gaps_never_guess() {
    let nested = AuthorityFixture::one_page(
        "nested",
        "pages/page.md",
        vec![
            BlockSpec::root("root", "a"),
            BlockSpec::child("child", 0, "a"),
        ],
    );
    nested.overwrite("pages/page.md", b"- root edited\n\t- child edited\n");
    let nested_matches = nested
        .plan(&["pages/page.md"])
        .matches()
        .unwrap()
        .to_owned();
    assert_eq!(nested_matches.blocks().len(), 2);
    assert_eq!(
        nested_matches.blocks()[0].block_id(),
        nested.pages[0].block_ids[0]
    );
    assert_eq!(
        nested_matches.blocks()[1].block_id(),
        nested.pages[0].block_ids[1]
    );

    let duplicates = AuthorityFixture::one_page(
        "duplicate-gap",
        "pages/page.md",
        vec![BlockSpec::root("same", "a"), BlockSpec::root("same", "b")],
    );
    duplicates.overwrite("pages/page.md", b"- same\n");
    assert!(
        duplicates
            .plan(&["pages/page.md"])
            .matches()
            .unwrap()
            .blocks()
            .is_empty(),
        "unequal duplicate gap must conservatively lose continuity"
    );
}

#[test]
fn non_round_tripping_org_reaches_read_only_external_execution() {
    let org = AuthorityFixture::one_page(
        "skipped-org-level-admission",
        "pages/page.org",
        vec![BlockSpec::root("parent", "a")],
    );
    let skipped = b"* parent changed\n*** child\n";
    org.overwrite("pages/page.org", skipped);
    let org_plan = org.plan(&["pages/page.org"]);
    assert_eq!(
        org_plan.status(),
        ImportPlanStatus::Reconcile,
        "{org_plan:?}"
    );
    assert!(
        org_plan.blocks().is_empty(),
        "read-only Org must not expose refusal blocks"
    );
    assert_eq!(
        fs::read(org.graph_root.join("pages/page.org")).unwrap(),
        skipped
    );
}

#[test]
fn structurally_round_tripping_markdown_reaches_external_execution() {
    let markdown = AuthorityFixture::one_page(
        "mixed-indent-admission",
        "pages/page.md",
        vec![BlockSpec::root("parent", "a")],
    );
    let mixed = b"- parent changed\n\t- a\n  - b\n";
    markdown.overwrite("pages/page.md", mixed);

    let markdown_plan = markdown.plan(&["pages/page.md"]);
    assert_eq!(markdown_plan.status(), ImportPlanStatus::Reconcile);
    assert!(
        markdown_plan.blocks().is_empty(),
        "structurally stable Markdown must not expose refusal blocks"
    );
    assert_eq!(
        fs::read(markdown.graph_root.join("pages/page.md")).unwrap(),
        mixed
    );
}

#[test]
fn round_tripping_external_edits_remain_admitted_and_formatting_noop_stays_unpublished() {
    let markdown = AuthorityFixture::one_page(
        "valid-space-indent-admission",
        "pages/page.md",
        vec![BlockSpec::root("parent", "a")],
    );
    let valid_markdown =
        b"- parent edited\r\n  - child edited\r\n    wrapped\r\n    \r\n    final paragraph";
    markdown.overwrite("pages/page.md", valid_markdown);
    assert_eq!(
        markdown.plan(&["pages/page.md"]).status(),
        ImportPlanStatus::Reconcile
    );
    assert_eq!(
        fs::read(markdown.graph_root.join("pages/page.md")).unwrap(),
        valid_markdown
    );

    let org = AuthorityFixture::one_page(
        "valid-org-admission",
        "pages/page.org",
        vec![BlockSpec::root("parent", "a")],
    );
    let valid_org = b"* parent edited\n** child edited\n";
    org.overwrite("pages/page.org", valid_org);
    assert_eq!(
        org.plan(&["pages/page.org"]).status(),
        ImportPlanStatus::Reconcile
    );

    let formatting_only = AuthorityFixture::one_page(
        "formatting-only-admission",
        "pages/Imported Page 0.md",
        vec![BlockSpec::root("parent", "0000000000")],
    );
    let crlf = b"- parent\r\n";
    formatting_only.overwrite("pages/Imported Page 0.md", crlf);
    let formatting_plan = formatting_only.plan(&["pages/Imported Page 0.md"]);
    assert_eq!(formatting_plan.status(), ImportPlanStatus::Noop);
    assert!(formatting_plan.blocks().is_empty());
    assert_eq!(
        fs::read(formatting_only.graph_root.join("pages/Imported Page 0.md")).unwrap(),
        crlf
    );
}

#[test]
fn uppercase_org_external_edits_use_org_admission_and_preserve_nested_structure() {
    let fixture = AuthorityFixture::one_page(
        "uppercase-org-import",
        "pages/Outline.ORG",
        vec![
            BlockSpec::root("parent", "a"),
            BlockSpec::child("child", 0, "a"),
        ],
    );
    let edited = b"* parent edited\n** child edited\n";
    fixture.overwrite("pages/Outline.ORG", edited);

    let plan = fixture.plan(&["pages/Outline.ORG"]);
    assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
    assert!(plan.blocks().is_empty());
    let matches = plan.matches().unwrap().blocks();
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].locator().components(), &[0]);
    assert_eq!(matches[0].block_id(), fixture.pages[0].block_ids[0]);
    assert_eq!(matches[1].locator().components(), &[0, 0]);
    assert_eq!(matches[1].block_id(), fixture.pages[0].block_ids[1]);
    assert_eq!(
        fs::read(fixture.graph_root.join("pages/Outline.ORG")).unwrap(),
        edited
    );

    let formatting_only = AuthorityFixture::one_page(
        "uppercase-org-formatting-only",
        "pages/Imported Page 0.ORG",
        vec![
            BlockSpec::root("parent", "0000000000"),
            BlockSpec::child("child", 0, "0000000000"),
        ],
    );
    let crlf = b"* parent\r\n** child\r\n";
    formatting_only.overwrite("pages/Imported Page 0.ORG", crlf);
    let formatting_plan = formatting_only.plan(&["pages/Imported Page 0.ORG"]);
    assert_eq!(
        formatting_plan.status(),
        ImportPlanStatus::Noop,
        "{formatting_plan:?}"
    );
    assert!(formatting_plan.blocks().is_empty());
    assert_eq!(
        fs::read(formatting_only.graph_root.join("pages/Imported Page 0.ORG")).unwrap(),
        crlf
    );
}

#[test]
fn equal_structures_separated_by_anchor_remain_globally_ambiguous() {
    let anchor = LogseqUuid::from_uuid(uuid(515));
    let fixture = AuthorityFixture::one_page(
        "anchor-separated-duplicates",
        "pages/page.md",
        vec![
            BlockSpec::root("same", "a"),
            BlockSpec::root("anchor", "b").with_uuid(anchor),
            BlockSpec::root("same", "c"),
        ],
    );
    fixture.overwrite(
        "pages/page.md",
        format!("- same\n- anchor\n  id:: {anchor}\n").as_bytes(),
    );
    let matches = fixture
        .plan(&["pages/page.md"])
        .matches()
        .unwrap()
        .to_owned();
    assert_eq!(matches.blocks().len(), 1);
    assert_eq!(
        matches.blocks()[0].basis(),
        BlockMatchBasis::UniqueLogseqUuid
    );

    fixture.overwrite(
        "pages/page.md",
        format!("- same\n- anchor\n  id:: {anchor}\n- same\n").as_bytes(),
    );
    let both_survive = fixture
        .plan(&["pages/page.md"])
        .matches()
        .unwrap()
        .to_owned();
    assert_eq!(both_survive.blocks().len(), 1);
    assert_eq!(
        both_survive.blocks()[0].basis(),
        BlockMatchBasis::UniqueLogseqUuid
    );
}

#[test]
fn nested_reparented_duplicate_class_does_not_gain_identity_from_an_anchor_gap() {
    let anchor = LogseqUuid::from_uuid(uuid(516));
    let fixture = AuthorityFixture::one_page(
        "nested-anchor-ambiguity",
        "pages/page.md",
        vec![
            BlockSpec::root("parent", "a"),
            BlockSpec::child("same child", 0, "a"),
            BlockSpec::root("anchor", "b").with_uuid(anchor),
            BlockSpec::root("parent", "c"),
            BlockSpec::child("same child", 3, "a"),
        ],
    );
    fixture.overwrite(
        "pages/page.md",
        format!("- parent\n\t- same child\n- anchor\n  id:: {anchor}\n").as_bytes(),
    );
    let matches = fixture
        .plan(&["pages/page.md"])
        .matches()
        .unwrap()
        .to_owned();
    assert_eq!(matches.blocks().len(), 1);
    assert_eq!(
        matches.blocks()[0].basis(),
        BlockMatchBasis::UniqueLogseqUuid
    );
}

#[test]
fn global_exact_matching_retains_unambiguous_cross_page_move_but_not_copy() {
    let moved = AuthorityFixture::new(
        "cross-page-exact-move",
        vec![
            PageSpec {
                path: "pages/a.md".into(),
                blocks: vec![BlockSpec::root("moved", "a")],
                name: None,
                preamble: None,
            },
            PageSpec {
                path: "pages/b.md".into(),
                blocks: vec![BlockSpec::root("resident", "a")],
                name: None,
                preamble: None,
            },
        ],
    );
    moved.overwrite("pages/a.md", b"");
    moved.overwrite("pages/b.md", b"- resident\n- moved\n");
    let matches = moved
        .plan(&["pages/a.md", "pages/b.md"])
        .matches()
        .unwrap()
        .to_owned();
    let retained = matches
        .blocks()
        .iter()
        .find(|matched| matched.block_id() == moved.pages[0].block_ids[0])
        .unwrap();
    assert_eq!(retained.path().as_str(), "pages/b.md");
    assert_eq!(retained.locator().components(), &[1]);
    assert_eq!(retained.basis(), BlockMatchBasis::ReceiptStructuralExact);

    let copied = AuthorityFixture::new(
        "cross-page-exact-copy",
        vec![
            PageSpec {
                path: "pages/a.md".into(),
                blocks: vec![BlockSpec::root("moved", "a")],
                name: None,
                preamble: None,
            },
            PageSpec {
                path: "pages/b.md".into(),
                blocks: vec![BlockSpec::root("resident", "a")],
                name: None,
                preamble: None,
            },
        ],
    );
    copied.overwrite("pages/b.md", b"- resident\n- moved\n");
    let copy_matches = copied
        .plan(&["pages/a.md", "pages/b.md"])
        .matches()
        .unwrap()
        .to_owned();
    assert!(!copy_matches
        .blocks()
        .iter()
        .any(|matched| matched.block_id() == copied.pages[0].block_ids[0]));
}

#[test]
fn unrelated_equal_length_replacements_never_retain_block_identity() {
    let fixture = AuthorityFixture::one_page(
        "equal-replacement-gap",
        "pages/page.md",
        vec![BlockSpec::root("A", "a"), BlockSpec::root("B", "b")],
    );
    fixture.overwrite("pages/page.md", b"- X\n- Y\n");
    assert!(fixture
        .plan(&["pages/page.md"])
        .matches()
        .unwrap()
        .blocks()
        .is_empty());
}

#[test]
fn crossed_uuid_anchors_block_all_unanchored_sibling_guessing() {
    let p = LogseqUuid::from_uuid(uuid(521));
    let q = LogseqUuid::from_uuid(uuid(522));
    let fixture = AuthorityFixture::one_page(
        "crossed-anchors",
        "pages/page.md",
        vec![
            BlockSpec::root("A", "a"),
            BlockSpec::root("P", "b").with_uuid(p),
            BlockSpec::root("B", "c"),
            BlockSpec::root("Q", "d").with_uuid(q),
        ],
    );
    fixture.overwrite(
        "pages/page.md",
        format!("- Q\n  id:: {q}\n- X\n- P\n  id:: {p}\n- Y\n").as_bytes(),
    );
    let matches = fixture
        .plan(&["pages/page.md"])
        .matches()
        .unwrap()
        .to_owned();
    assert_eq!(matches.blocks().len(), 2);
    assert!(matches
        .blocks()
        .iter()
        .all(|matched| matched.basis() == BlockMatchBasis::UniqueLogseqUuid));
    let ids = matches
        .blocks()
        .iter()
        .map(|matched| matched.block_id())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(ids.contains(&fixture.pages[0].block_ids[1]));
    assert!(ids.contains(&fixture.pages[0].block_ids[3]));
}

#[test]
fn exact_rename_and_copy_before_delete_preserve_conservative_page_identity() {
    let rename = AuthorityFixture::one_page(
        "rename",
        "pages/old.md",
        vec![BlockSpec::root("retained", "a")],
    );
    let projected = rename.pages[0].projected.clone();
    fs::rename(
        rename.graph_root.join("pages/old.md"),
        rename.graph_root.join("pages/new.md"),
    )
    .unwrap();
    let plan = rename.plan(&["pages/old.md", "pages/new.md"]);
    let page = &plan.matches().unwrap().pages()[0];
    assert_eq!(page.page_id(), rename.pages[0].page_id);
    assert_eq!(page.basis(), PageMatchBasis::ReceiptBackedExactRename);
    assert_eq!(page.path().as_str(), "pages/new.md");

    let copy = AuthorityFixture::one_page(
        "copy-before-delete",
        "pages/old.md",
        vec![BlockSpec::root("retained", "a")],
    );
    write(&copy.graph_root, "pages/copy.md", &projected);
    let matches = copy
        .plan(&["pages/old.md", "pages/copy.md"])
        .matches()
        .unwrap()
        .to_owned();
    assert!(matches
        .pages()
        .iter()
        .any(|page| page.path().as_str() == "pages/old.md"));
    assert!(!matches
        .pages()
        .iter()
        .any(|page| page.path().as_str() == "pages/copy.md"));
}

#[test]
fn unrelated_zero_block_delete_create_does_not_retain_page_identity() {
    let fixture = AuthorityFixture::one_titled_page(
        "unrelated-zero-block-delete-create",
        "pages/old.md",
        "Old",
        "title:: Old\nowner:: A",
        Vec::new(),
    );
    let old_page_id = fixture.pages[0].page_id;
    fs::remove_file(fixture.graph_root.join("pages/old.md")).unwrap();
    fixture.overwrite("pages/new.md", b"title:: Unrelated\nowner:: B\n");

    let plan = fixture.plan(&["pages/old.md", "pages/new.md"]);
    assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
    assert!(
        plan.matches()
            .unwrap()
            .pages()
            .iter()
            .all(|matched| matched.page_id() != old_page_id),
        "unrelated empty block trees are not move evidence: {plan:#?}"
    );
}

#[test]
fn unrelated_common_block_delete_create_does_not_retain_page_identity() {
    let fixture = AuthorityFixture::one_titled_page(
        "unrelated-common-block-delete-create",
        "pages/old.md",
        "Old",
        "title:: Old\nowner:: A",
        vec![BlockSpec::root("generic", "a")],
    );
    let old_page_id = fixture.pages[0].page_id;
    fs::remove_file(fixture.graph_root.join("pages/old.md")).unwrap();
    fixture.overwrite(
        "pages/new.md",
        b"title:: Unrelated\nowner:: B\n\n- generic\n",
    );

    let plan = fixture.plan(&["pages/old.md", "pages/new.md"]);
    assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
    assert!(
        plan.matches()
            .unwrap()
            .pages()
            .iter()
            .all(|matched| matched.page_id() != old_page_id),
        "a common generic block is not move evidence: {plan:#?}"
    );
}

#[test]
fn completed_release_allows_exact_recreation_as_new() {
    let mut fixture = AuthorityFixture::one_page(
        "released-recreation",
        "pages/released.md",
        vec![BlockSpec::root("historical", "a")],
    );
    let old_page_id = fixture.pages[0].page_id;
    fixture.delete_and_project(0, 58_000);
    assert!(!fixture.graph_root.join("pages/released.md").exists());
    fixture.overwrite("pages/released.md", b"- external recreation\n");

    let recreated = fixture.plan(&["pages/released.md"]);
    assert_eq!(recreated.status(), ImportPlanStatus::Reconcile);
    assert!(recreated
        .matches()
        .unwrap()
        .pages()
        .iter()
        .all(|matched| matched.page_id() != old_page_id));
    assert!(recreated.import_id().is_some());
}

#[test]
fn affected_scope_avoids_unrelated_entries_and_accepts_supported_graph_text() {
    let custom = TestDir::new("custom-layout");
    fs::create_dir_all(custom.path().join("logseq")).unwrap();
    fs::write(
        custom.path().join("logseq/config.edn"),
        "{:pages-directory \"notes\" :journals-directory \"diary\"}\n",
    )
    .unwrap();
    let affected_page = "notes/projects/affected.md";
    let affected_journal = "diary/2026_07_26.md";
    write(custom.path(), affected_page, b"- affected page\n");
    write(custom.path(), affected_journal, b"- affected journal\n");
    write(custom.path(), "notes/unrelated.md", b"- unrelated\n");
    // OG walks the whole graph directory, so this is ordinary graph text even
    // though neither configured root owns it.
    let outside = "archive/2026/outside.md";
    write(custom.path(), outside, b"- outside\n");
    let graph = Graph::open(custom.path());

    let inventory = inventory_affected(&graph, &[affected_page, affected_journal]).unwrap();
    assert_eq!(
        inventory
            .entries()
            .keys()
            .map(ManagedPath::as_str)
            .collect::<Vec<_>>(),
        vec![affected_journal, affected_page],
        "affected inventory must not enumerate the unrelated configured page"
    );
    for (path, expected_kind) in [
        (affected_page, ManagedTextKind::Page),
        (affected_journal, ManagedTextKind::Journal),
    ] {
        assert!(matches!(
            inventory.entries().get(&ManagedPath::parse(path).unwrap()),
            Some(RawObservation::Present(bytes)) if !bytes.bytes().is_empty()
        ));
        let entry = graph.entry_for_path(&custom.path().join(path)).unwrap();
        assert_eq!(entry.rel_path, path);
        let kind = match entry.kind {
            crate::PageKind::Page => ManagedTextKind::Page,
            crate::PageKind::Journal => ManagedTextKind::Journal,
        };
        assert_eq!(kind, expected_kind, "configured path {path}");
    }

    // A supported path no configured root owns is inventoried at its exact
    // spelling, and its kind comes from the file-name title exactly as OG
    // 6e7afa8eb040686ff057156ee877193b581dd369 decides it in
    // `deps/graph-parser/src/logseq/graph_parser/extract.cljc`
    // (`get-page-name`) and
    // `deps/graph-parser/src/logseq/graph_parser/block.cljs`
    // (`convert-page-if-journal`).
    let nonstandard = inventory_affected(&graph, &[outside]).unwrap();
    assert_eq!(
        nonstandard
            .entries()
            .keys()
            .map(ManagedPath::as_str)
            .collect::<Vec<_>>(),
        vec![outside]
    );
    assert_eq!(
        graph
            .entry_for_path(&custom.path().join(outside))
            .unwrap()
            .kind,
        crate::PageKind::Page
    );
    // Containers OG itself skips remain unreadable evidence.
    assert!(inventory_affected(&graph, &["assets/note.md"]).is_err());

    let initial = inventory_initial_shadow(&graph).unwrap();
    assert_eq!(
        initial
            .entries()
            .keys()
            .map(ManagedPath::as_str)
            .collect::<Vec<_>>(),
        vec![
            outside,
            affected_journal,
            affected_page,
            "notes/unrelated.md"
        ]
    );
}

#[test]
fn managed_path_identity_matches_graphwide_filename_decoder() {
    let dir = TestDir::new("managed-path-filename-identity");
    for path in ["pages/2026_09_01.md", "journals/Foo.md"] {
        write(dir.path(), path, b"- identity probe\n");
    }
    let graph = Graph::open(dir.path());

    for path in ["pages/2026_09_01.md", "journals/Foo.md"] {
        let managed = graph
            .managed_entry_for_managed_path(&ManagedPath::parse(path).unwrap())
            .unwrap();
        let graphwide = graph.entry_for_path(&dir.path().join(path)).unwrap();
        assert_eq!(managed.name, graphwide.name, "logical name for {path}");
        assert_eq!(managed.kind, graphwide.kind, "page kind for {path}");
        assert_eq!(managed.date_key, graphwide.date_key, "date key for {path}");
    }
}

#[test]
fn attack_external_title_change_at_exact_path_updates_logical_page_name() {
    let fixture = AuthorityFixture::one_page(
        "attack-exact-path-title-change",
        "pages/Imported Page 0.md",
        vec![BlockSpec::root("base", "a")],
    );
    let page_id = fixture.pages[0].page_id;
    let path = "pages/Imported Page 0.md";
    fixture.overwrite(path, b"title:: Renamed Page\n\n- base\n");
    let graph_name = Graph::open(&fixture.graph_root)
        .list_pages()
        .into_iter()
        .find(|entry| entry.rel_path == path)
        .unwrap()
        .name;
    assert_eq!(graph_name, "Renamed Page");

    let plan = fixture.plan(&[path]);
    assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
    let diagnostic = format!("{plan:#?}");
    assert!(
        diagnostic.contains("ReconcileExternalPageState")
            && diagnostic.contains(&page_id.to_string())
            && diagnostic.contains("Renamed Page"),
        "accepted identity must follow the parser-owned external title: {diagnostic}"
    );
}

#[test]
fn attack_new_external_page_uses_parser_owned_title_as_logical_name() {
    let fixture = AuthorityFixture::one_page(
        "attack-new-external-title",
        "pages/seed.md",
        vec![BlockSpec::root("seed", "a")],
    );
    let path = "pages/physical-name.md";
    assert_new_external_document_pair(
        &fixture,
        path,
        b"title:: Logical Title\n\n- external\n",
        "Logical Title",
        ManagedTextKind::Page,
    );
}

#[test]
fn new_org_title_spellings_share_graph_and_import_semantics() {
    let fixture = AuthorityFixture::one_page(
        "paired-org-title-spellings",
        "pages/seed.md",
        vec![BlockSpec::root("seed", "a")],
    );
    for (path, bytes, name) in [
        (
            "pages/lower.org",
            b"#+title: Lower Title\n\n* external\n".as_slice(),
            "Lower Title",
        ),
        (
            "pages/upper.org",
            b"#+TITLE: Upper Title\n\n* external\n".as_slice(),
            "Upper Title",
        ),
        (
            "pages/mixed.org",
            b"#+TiTlE: Mixed Title\n\n* external\n".as_slice(),
            "Mixed Title",
        ),
        (
            "pages/drawer.org",
            b":PROPERTIES:\n:TiTlE: Drawer Title\n:END:\n\n* external\n".as_slice(),
            "Drawer Title",
        ),
    ] {
        assert_new_external_document_pair(&fixture, path, bytes, name, ManagedTextKind::Page);
    }
}

#[test]
fn explicit_titles_precede_directory_and_filename_kind_before_journal_conversion() {
    let mut fixture = AuthorityFixture::one_page(
        "paired-title-kind-precedence",
        "pages/seed.md",
        vec![BlockSpec::root("seed", "a")],
    );
    write(
        &fixture.graph_root,
        "logseq/config.edn",
        b"{:journal/file-name-format \"dd-MM-yyyy\"\n\
          :journal/page-title-format \"yyyy-MM-dd\"}\n",
    );
    fixture.graph = Graph::open(&fixture.graph_root);
    for (path, bytes, name, kind) in [
        (
            "pages/nested/not-a-date.md",
            b"title:: 25-07-2026\n\n- date title\n".as_slice(),
            "2026-07-25",
            ManagedTextKind::Journal,
        ),
        (
            "journals/nested/25-07-2026.md",
            b"title:: Ordinary Page\n\n- non-date title\n".as_slice(),
            "Ordinary Page",
            ManagedTextKind::Page,
        ),
        (
            "archive/deep/physical.org",
            b"#+TiTlE: 26-07-2026\n\n* date title\n".as_slice(),
            "2026-07-26",
            ManagedTextKind::Journal,
        ),
    ] {
        assert_new_external_document_pair(&fixture, path, bytes, name, kind);
    }
}

#[test]
fn title_edited_move_with_unique_uuid_retains_page_identity() {
    let anchor = LogseqUuid::from_uuid(uuid(53_001));
    let fixture = AuthorityFixture::one_page(
        "attack-rename-preamble-identity",
        "pages/old.md",
        vec![BlockSpec::root("base", "a").with_uuid(anchor)],
    );
    let page_id = fixture.pages[0].page_id;
    fs::rename(
        fixture.graph_root.join("pages/old.md"),
        fixture.graph_root.join("pages/new.md"),
    )
    .unwrap();
    fixture.overwrite(
        "pages/new.md",
        format!("title:: New\n\n- edited\n  id:: {anchor}\n").as_bytes(),
    );

    let plan = fixture.plan(&["pages/old.md", "pages/new.md"]);
    assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
    assert!(plan.matches().unwrap().pages().iter().any(|matched| {
        matched.page_id() == page_id
            && matched.previous_path().as_str() == "pages/old.md"
            && matched.path().as_str() == "pages/new.md"
            && matched.basis() == PageMatchBasis::ReceiptBackedAnchoredRename
    }));
}

#[test]
fn configured_root_anchored_move_retains_page_identity_and_uses_new_title() {
    let anchor = LogseqUuid::from_uuid(uuid(53_002));
    let mut fixture = AuthorityFixture::one_page(
        "attack-configured-root-move",
        "pages/Imported Page 0.md",
        vec![BlockSpec::root("base", "a").with_uuid(anchor)],
    );
    let page_id = fixture.pages[0].page_id;
    write(
        &fixture.graph_root,
        "logseq/config.edn",
        b"{:pages-directory \"notes\"}\n",
    );
    fs::create_dir_all(fixture.graph_root.join("notes/deep")).unwrap();
    fs::rename(
        fixture.graph_root.join("pages/Imported Page 0.md"),
        fixture.graph_root.join("notes/deep/renamed.md"),
    )
    .unwrap();
    fixture.overwrite(
        "notes/deep/renamed.md",
        format!("title:: Configured Root Rename\n\n- edited\n  id:: {anchor}\n").as_bytes(),
    );
    fixture.graph = Graph::open(&fixture.graph_root);

    let plan = fixture.plan(&["pages/Imported Page 0.md", "notes/deep/renamed.md"]);
    assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
    let diagnostic = format!("{plan:#?}");
    assert!(
        diagnostic.contains("ReconcileExternalPageState")
            && diagnostic.contains(&page_id.to_string())
            && diagnostic.contains("Configured Root Rename")
            && diagnostic.contains("notes/deep/renamed.md"),
        "move must retain PageId while re-evaluating destination title semantics: {diagnostic}"
    );
}

#[test]
fn title_edited_move_without_uuid_is_conservative_delete_create() {
    let fixture = AuthorityFixture::one_page(
        "title-edited-move-without-anchor",
        "pages/old.md",
        vec![BlockSpec::root("base", "a")],
    );
    let old_page_id = fixture.pages[0].page_id;
    fs::remove_file(fixture.graph_root.join("pages/old.md")).unwrap();
    fixture.overwrite("pages/new.md", b"title:: New\n\n- base\n");

    let plan = fixture.plan(&["pages/old.md", "pages/new.md"]);
    assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
    assert!(plan
        .matches()
        .unwrap()
        .pages()
        .iter()
        .all(|matched| matched.page_id() != old_page_id));
}

#[test]
fn cross_format_title_edited_move_without_uuid_is_conservative_delete_create() {
    let fixture = AuthorityFixture::one_page(
        "cross-format-move-without-anchor",
        "pages/old.md",
        vec![BlockSpec::root("base", "a")],
    );
    let old_page_id = fixture.pages[0].page_id;
    fs::remove_file(fixture.graph_root.join("pages/old.md")).unwrap();
    fixture.overwrite("pages/new.org", b"#+TiTlE: New\n\n* base\n");

    let plan = fixture.plan(&["pages/old.md", "pages/new.org"]);
    assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
    assert!(plan
        .matches()
        .unwrap()
        .pages()
        .iter()
        .all(|matched| matched.page_id() != old_page_id));
}

#[test]
fn duplicate_uuid_evidence_blocks_page_move_identity() {
    let anchor = LogseqUuid::from_uuid(uuid(53_003));
    let fixture = AuthorityFixture::one_page(
        "duplicate-anchor-move",
        "pages/old.md",
        vec![BlockSpec::root("base", "a").with_uuid(anchor)],
    );
    fs::remove_file(fixture.graph_root.join("pages/old.md")).unwrap();
    fixture.overwrite(
        "pages/new.md",
        format!("- first\n  id:: {anchor}\n- second\n  id:: {anchor}\n").as_bytes(),
    );

    let plan = fixture.plan(&["pages/old.md", "pages/new.md"]);
    assert!(blocked_reasons(&plan).contains(&ImportBlockReason::AmbiguousDestructiveMatch));
}

#[test]
fn one_source_to_several_uuid_anchored_destinations_blocks() {
    let first = LogseqUuid::from_uuid(uuid(53_004));
    let second = LogseqUuid::from_uuid(uuid(53_005));
    let fixture = AuthorityFixture::one_page(
        "one-source-several-destinations",
        "pages/old.md",
        vec![
            BlockSpec::root("first", "a").with_uuid(first),
            BlockSpec::root("second", "b").with_uuid(second),
        ],
    );
    fs::remove_file(fixture.graph_root.join("pages/old.md")).unwrap();
    fixture.overwrite(
        "pages/new-a.md",
        format!("title:: New A\n\n- edited first\n  id:: {first}\n").as_bytes(),
    );
    fixture.overwrite(
        "pages/new-b.md",
        format!("title:: New B\n\n- edited second\n  id:: {second}\n").as_bytes(),
    );

    let plan = fixture.plan(&["pages/old.md", "pages/new-a.md", "pages/new-b.md"]);
    assert!(blocked_reasons(&plan).contains(&ImportBlockReason::AmbiguousDestructiveMatch));
}

#[test]
fn several_sources_to_one_uuid_anchored_destination_blocks() {
    let first = LogseqUuid::from_uuid(uuid(53_006));
    let second = LogseqUuid::from_uuid(uuid(53_007));
    let fixture = AuthorityFixture::new(
        "several-sources-one-destination",
        vec![
            PageSpec {
                path: "pages/old-a.md".into(),
                blocks: vec![BlockSpec::root("first", "a").with_uuid(first)],
                name: Some("Old A".into()),
                preamble: Some("title:: Old A".into()),
            },
            PageSpec {
                path: "pages/old-b.md".into(),
                blocks: vec![BlockSpec::root("second", "a").with_uuid(second)],
                name: Some("Old B".into()),
                preamble: Some("title:: Old B".into()),
            },
        ],
    );
    fs::remove_file(fixture.graph_root.join("pages/old-a.md")).unwrap();
    fs::remove_file(fixture.graph_root.join("pages/old-b.md")).unwrap();
    fixture.overwrite(
        "pages/new.md",
        format!("- edited first\n  id:: {first}\n- edited second\n  id:: {second}\n").as_bytes(),
    );

    let plan = fixture.plan(&["pages/old-a.md", "pages/old-b.md", "pages/new.md"]);
    assert!(blocked_reasons(&plan).contains(&ImportBlockReason::AmbiguousDestructiveMatch));
}

#[test]
fn partial_affected_move_scope_does_not_retain_page_identity() {
    let anchor = LogseqUuid::from_uuid(uuid(53_008));
    let fixture = AuthorityFixture::one_page(
        "partial-move-scope",
        "pages/old.md",
        vec![BlockSpec::root("base", "a").with_uuid(anchor)],
    );
    fs::remove_file(fixture.graph_root.join("pages/old.md")).unwrap();
    fixture.overwrite(
        "pages/new.md",
        format!("- edited\n  id:: {anchor}\n").as_bytes(),
    );

    let old_only = fixture.plan(&["pages/old.md"]);
    assert!(old_only.matches().unwrap().pages().is_empty());
    let new_only = fixture.plan(&["pages/new.md"]);
    assert!(new_only.matches().unwrap().pages().is_empty());
}

#[test]
fn exact_title_removal_and_format_only_edit_follow_authenticated_base_evidence() {
    let retitled = AuthorityFixture::one_titled_page(
        "explicit-title-change",
        "pages/physical.md",
        "Original Logical",
        "title:: Original Logical",
        vec![BlockSpec::root("base", "a")],
    );
    let retitled_page_id = retitled.pages[0].page_id;
    retitled.overwrite("pages/physical.md", b"title:: Current Logical\n\n- base\n");
    let title_change = retitled.plan(&["pages/physical.md"]);
    assert_eq!(
        title_change.status(),
        ImportPlanStatus::Reconcile,
        "{title_change:?}"
    );
    let diagnostic = format!("{title_change:#?}");
    assert!(
        diagnostic.contains("ReconcileExternalPageState")
            && diagnostic.contains(&retitled_page_id.to_string())
            && diagnostic.contains("Current Logical"),
        "explicit title A -> B must reconcile the same PageId: {diagnostic}"
    );

    let changed = AuthorityFixture::one_titled_page(
        "explicit-title-removal",
        "pages/physical.md",
        "Logical",
        "title:: Logical",
        vec![BlockSpec::root("base", "a")],
    );
    let page_id = changed.pages[0].page_id;
    changed.overwrite("pages/physical.md", b"- base\n");
    let removal = changed.plan(&["pages/physical.md"]);
    assert_eq!(removal.status(), ImportPlanStatus::Reconcile, "{removal:?}");
    let diagnostic = format!("{removal:#?}");
    assert!(
        diagnostic.contains("ReconcileExternalPageState")
            && diagnostic.contains(&page_id.to_string())
            && diagnostic.contains("physical"),
        "title removal must reconcile to the current filename fallback: {diagnostic}"
    );

    let formatting = AuthorityFixture::one_titled_page(
        "explicit-title-formatting",
        "pages/physical.md",
        "Logical",
        "title:: Logical",
        vec![BlockSpec::root("base", "a")],
    );
    formatting.overwrite(
        "pages/physical.md",
        b"title::   Logical  \nsubtitle:: retained\n\n- base\n",
    );
    let plan = formatting.plan(&["pages/physical.md"]);
    assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
    let diagnostic = format!("{plan:#?}");
    assert!(
        !diagnostic.contains("ReconcileExternalPageState"),
        "unchanged semantic title formatting must not rename: {diagnostic}"
    );
    assert!(diagnostic.contains("SetPagePreamble"));
}

/// One canonical page name has one owner; a second physical file for it is a
/// deduplicated source, not a reason to refuse the transaction.
///
/// Activation already makes exactly this selection over the same graph
/// (`bootstrap_authoritative_source_paths`), so refusing here denied every
/// affected path for as long as both files existed — permanently, since no user
/// action inside Tine could clear it.
#[test]
fn explicit_title_collisions_import_the_first_exact_path_and_withhold_the_rest() {
    let fixture = AuthorityFixture::one_page(
        "explicit-title-collision",
        "pages/Owned.md",
        vec![BlockSpec::root("owned", "a")],
    );
    fixture.overwrite("pages/first.md", b"title:: Shared Explicit\n\n- first\n");
    fixture.overwrite("pages/second.md", b"title:: Shared Explicit\n\n- second\n");
    let affected = fixture.plan(&["pages/first.md", "pages/second.md"]);
    assert!(affected.blocks().is_empty(), "{affected:?}");
    assert_eq!(
        affected.status(),
        ImportPlanStatus::Reconcile,
        "{affected:?}"
    );
    let diagnostic = format!("{affected:#?}");
    assert_eq!(
        diagnostic.matches("CreatePage").count(),
        1,
        "exactly one exact path may carry the shared name: {diagnostic}"
    );
    assert!(
        diagnostic.contains("path: ManagedPath(\n                        \"pages/first.md\",")
            || diagnostic.contains("\"pages/first.md\""),
        "the first exact path is the one that carries it: {diagnostic}"
    );
    assert_eq!(
        fixture.read("pages/second.md").as_slice(),
        b"title:: Shared Explicit\n\n- second\n".as_slice(),
        "the withheld source keeps its exact bytes"
    );
}

#[test]
fn parser_owned_deep_input_is_refused_and_inventory_counts_physical_work() {
    let deep = AuthorityFixture::one_page(
        "deep-budget",
        "pages/deep.md",
        vec![BlockSpec::root("base", "a")],
    );
    let external = (0..=crate::oplog::MAX_IMPORT_DEPTH)
        .map(|depth| format!("{}- depth {depth}\n", "  ".repeat(depth)))
        .collect::<String>();
    deep.overwrite("pages/deep.md", external.as_bytes());
    let blocked = deep.plan(&["pages/deep.md"]);
    assert!(blocked_reasons(&blocked).contains(&ImportBlockReason::ResourceLimit));

    let fixture = AuthorityFixture::new(
        "inventory-peak",
        vec![
            PageSpec {
                path: "pages/a.md".into(),
                blocks: vec![BlockSpec::root("short", "a")],
                name: None,
                preamble: None,
            },
            PageSpec {
                path: "pages/b.md".into(),
                blocks: vec![BlockSpec::root("a somewhat longer block", "a")],
                name: None,
                preamble: None,
            },
        ],
    );
    let plan = fixture.plan(&["pages/a.md", "pages/b.md"]);
    assert_eq!(plan.status(), ImportPlanStatus::Noop);
    let total = fixture
        .pages
        .iter()
        .map(|page| page.projected.len() as u64)
        .sum::<u64>();
    let max = fixture
        .pages
        .iter()
        .map(|page| page.projected.len() as u64)
        .max()
        .unwrap();
    let work = plan.instrumentation();
    assert_eq!(work.bytes_read, total * 4);
    assert!(work.bytes_hashed >= total * 4);
    assert_eq!(work.peak_owned_raw_bytes, total + max + 16 * 1024);
}

#[test]
fn conflict_copy_classification_is_read_only_and_non_authoritative_for_deletion() {
    let dir = TestDir::new("conflict");
    fs::create_dir_all(dir.path().join("pages")).unwrap();
    fs::create_dir_all(dir.path().join("journals")).unwrap();
    let path = "pages/a.sync-conflict-20260724-120000-AAAAAAA.md";
    write(dir.path(), path, b"- generated\n");
    let graph = Graph::open(dir.path());
    let inventory = inventory_affected(&graph, &[path]).unwrap();
    let class = classify_conflict_copy(
        ManagedPath::parse(path).unwrap(),
        inventory.present(path).unwrap(),
        BlobDescription::of(b"- generated\n"),
        Some(BlobDescription::of(b"- external\n")),
    )
    .unwrap();
    assert_eq!(format!("{class:?}"), "GeneratedExact");
    assert!(dir.path().join(path).exists());
}

#[test]
fn large_noop_and_many_error_scopes_obey_complete_numeric_work_ceilings() {
    const PAGE_COUNT: usize = 128;
    let pages = (0..PAGE_COUNT)
        .map(|index| PageSpec {
            path: format!("pages/p{index:04}.md"),
            blocks: vec![BlockSpec::root(format!("block {index}"), "a")],
            name: None,
            preamble: None,
        })
        .collect();
    let fixture = AuthorityFixture::new("large-noop", pages);
    let owned_paths = fixture
        .pages
        .iter()
        .map(|page| page.path.clone())
        .collect::<Vec<_>>();
    let paths = owned_paths.iter().map(String::as_str).collect::<Vec<_>>();
    let noop = fixture.plan(&paths);
    assert_eq!(noop.status(), ImportPlanStatus::Noop);
    let work = noop.instrumentation();
    eprintln!(
        "large-noop instrumentation: {work:?}, total={}",
        work.recorded_work_units()
    );
    assert!(
        work.recorded_work_units() <= PAGE_COUNT * 4096,
        "complete work ceiling exceeded: {work:?}"
    );
    assert!(work.bytes_read <= PAGE_COUNT as u64 * 64);
    assert!(work.catalog_bytes_hashed <= PAGE_COUNT as u64 * 4096);
    assert!(work.locator_components_materialized <= PAGE_COUNT * 2);
    assert!(work.structural_key_comparisons <= PAGE_COUNT * 16);

    let error_names = (0..PAGE_COUNT)
        .map(|index| format!("pages/error-{index:04}.md"))
        .collect::<Vec<_>>();
    for path in &error_names {
        write(&fixture.graph_root, path, &[0xff, b'\n']);
    }
    let error_paths = error_names.iter().map(String::as_str).collect::<Vec<_>>();
    let errors = fixture.plan(&error_paths);
    assert_eq!(errors.status(), ImportPlanStatus::Blocked);
    assert_eq!(errors.blocks().len(), 1);
    assert_eq!(errors.inventory().unwrap().entries().len(), PAGE_COUNT);
    eprintln!(
        "many-error instrumentation: {:?}, total={}",
        errors.instrumentation(),
        errors.instrumentation().recorded_work_units()
    );
    assert!(
        errors.instrumentation().recorded_work_units() <= PAGE_COUNT * 4096,
        "many-error work ceiling exceeded: {:?}",
        errors.instrumentation()
    );
}

#[test]
fn large_disjoint_delete_create_sets_use_indexed_anchored_page_work() {
    const PAGE_COUNT: usize = 64;
    let fixture = AuthorityFixture::new(
        "large-disjoint-delete-create",
        (0..PAGE_COUNT)
            .map(|index| PageSpec {
                path: format!("pages/old-{index:04}.md"),
                blocks: vec![BlockSpec::root(format!("old block {index}"), "a")],
                name: Some(format!("Old {index}")),
                preamble: Some(format!("title:: Old {index}")),
            })
            .collect(),
    );
    let mut affected = Vec::with_capacity(PAGE_COUNT * 2);
    for index in 0..PAGE_COUNT {
        let old = format!("pages/old-{index:04}.md");
        let new = format!("pages/new-{index:04}.md");
        fs::remove_file(fixture.graph_root.join(&old)).unwrap();
        fixture.overwrite(
            &new,
            format!("title:: New {index}\n\n- unrelated new block {index}\n").as_bytes(),
        );
        affected.push(old);
        affected.push(new);
    }
    let paths = affected.iter().map(String::as_str).collect::<Vec<_>>();

    let plan = fixture.plan(&paths);
    assert_eq!(plan.status(), ImportPlanStatus::Reconcile, "{plan:?}");
    assert!(plan.matches().unwrap().pages().is_empty());
    let work = plan.instrumentation();
    assert_eq!(work.anchored_page_owner_inserts, PAGE_COUNT);
    assert_eq!(work.anchored_page_owner_lookups, 0);
    assert_eq!(work.anchored_page_edge_inserts, 0);
    assert!(
        work.recorded_work_units() <= PAGE_COUNT * 2 * 8192,
        "indexed disjoint-set work ceiling exceeded: {work:?}"
    );
}
