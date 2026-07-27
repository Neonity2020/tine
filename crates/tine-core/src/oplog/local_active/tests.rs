use std::collections::BTreeMap;
use std::fs;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::*;
use crate::model::Graph;
use crate::oplog::enrollment::{
    compose_verified_local, enrollment_application_root_for_test, CommitCut, EnrollmentOpen,
    EnrollmentReader, PreparationId,
};
use crate::oplog::hot_engine::{
    ProjectionEndpointBinding, ProjectionStorageBinding, MAX_EPHEMERAL_BLOCK_CLAIMS,
};
use crate::oplog::import::{
    force_next_bootstrap_part_operation_limit, prepare_inactive_bootstrap_import,
    publish_install_verify_inactive_bootstrap, reopen_inactive_bootstrap_accepted_authority,
    InactiveBootstrapAcceptedAuthority, InactiveBootstrapPreparedPublication,
    InactiveBootstrapVerifiedPublication,
};
use crate::oplog::migration_backup::{
    verify_migration_source_backup, MigrationBackupRoot, VerifiedSourceBackup,
};
use crate::oplog::reconciliation_baseline::{
    BaselineTimestamp, ReconciliationBaseline, ReconciliationBaselineBinding,
    TrustedPrivateApplicationRuntimeRoot,
};
use crate::oplog::reconciliation_scan::{ReconciliationSchedulerLimits, ReconciliationTrigger};
use crate::oplog::reconciliation_session::{
    ReconciliationSession, ReconciliationSessionDependencies, ReconciliationSessionStep,
};
use crate::oplog::shadow_projection::{
    verify_inactive_bootstrap_shadow_projection, VerifiedShadowProjection,
};
use crate::oplog::sqlite::{
    ApplicationRuntimeRoot, RebuildSource, SqliteFrontier, TailOverlay,
    VerifiedBootstrapSqliteProjection,
};
use crate::oplog::{
    AuthorBatch, BatchDisposition, BatchId, BatchOrigin, BlockId, BlockLocation,
    CanonicalArchiveResourceId, CrdtPeerId, DeviceId, DocumentId, LineageDigest, LogicalPageName,
    ManagedPath, ManagedTextKind, ObjectStore, OperationTransaction, PageId, ProjectionClaim,
    ProjectionEndpointId, ProjectionReceiptStore, ReferenceCatalogPolicyV1, SemanticOperation,
    ShardedHotEngine, WorkspaceId,
};

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tine-local-active-{label}-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One complete inactive enrollment: real capture, publication, backup, SQLite
/// bootstrap, shadow projection, and receipt namespace over one real graph.
struct Fixture {
    root: TestRoot,
    graph_root: PathBuf,
    graph: Graph,
    receipts: ProjectionReceiptStore,
    archive_root: PathBuf,
    workspace: WorkspaceId,
    lineage: LineageDigest,
    catalog_document_id: DocumentId,
    prepared: InactiveBootstrapPreparedPublication,
    verified: InactiveBootstrapVerifiedPublication,
    authority: InactiveBootstrapAcceptedAuthority,
    roots: MigrationBackupRoot,
    backup: VerifiedSourceBackup,
    /// Released before a promoted runtime opens: the SQLite applier lease is
    /// one per workspace, and the promoted projection takes it over.
    sqlite: Option<OpenProjection>,
    sqlite_proof: VerifiedBootstrapSqliteProjection,
    archive_resource_id: CanonicalArchiveResourceId,
    shadow: VerifiedShadowProjection,
    preparation: PreparationId,
    original_graph: BTreeMap<String, Vec<u8>>,
}

impl Fixture {
    fn new(label: &str, config: Option<&[u8]>, files: Vec<(String, Vec<u8>)>) -> Self {
        let root = TestRoot::new(label);
        let graph_root = root.path().join("graph");
        fs::create_dir(&graph_root).unwrap();
        if let Some(config) = config {
            fs::create_dir(graph_root.join("logseq")).unwrap();
            fs::write(graph_root.join("logseq/config.edn"), config).unwrap();
        }
        for (path, bytes) in &files {
            let destination = graph_root.join(path);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::write(destination, bytes).unwrap();
        }
        let original_graph = snapshot_files(&graph_root);
        let graph = Graph::open(&graph_root);

        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(0x9100));
        let lineage = LineageDigest::of(b"local-active-activation-test");
        let catalog_document_id = DocumentId::from_uuid(Uuid::from_u128(0x9101));

        // A real receipt namespace supplies the enrolled endpoint and receipt
        // store identity, so the enrollment binding is never synthetic.
        let receipt_root = root.path().join("receipts");
        fs::create_dir(&receipt_root).unwrap();
        let endpoint = ProjectionEndpointBinding::enroll_graph(
            &graph,
            ProjectionEndpointId::from_uuid(Uuid::from_u128(0x9102)),
            DeviceId::from_uuid(Uuid::from_u128(0x9103)),
        )
        .unwrap();
        let receipts =
            ProjectionReceiptStore::open_for_endpoint(&receipt_root, workspace, endpoint).unwrap();

        let capture_root = root.path().join("capture");
        let preparation_root = root.path().join("preparation");
        fs::create_dir(&capture_root).unwrap();
        fs::create_dir(&preparation_root).unwrap();
        let capture = graph
            .capture_inactive_bootstrap_sources(&capture_root)
            .unwrap();
        // The bootstrap is authored for exactly this archive: its accepted cold
        // records bind reference-catalog roots that live in this archive's
        // durable authenticated store.
        let archive_root = root.path().join("archive");
        let prepared = prepare_inactive_bootstrap_import(
            &graph,
            capture,
            workspace,
            lineage,
            catalog_document_id,
            ReferenceCatalogPolicyV1::default(),
            &ObjectStore::open(&archive_root, workspace)
                .unwrap()
                .bootstrap_authoring_capability()
                .unwrap(),
            &preparation_root,
        )
        .unwrap();
        let storage_binding = ProjectionStorageBinding {
            endpoint,
            receipt_store_id: receipts.store_id(),
        };
        let verified = publish_install_verify_inactive_bootstrap(
            &prepared,
            ObjectStore::open(&archive_root, workspace).unwrap(),
            storage_binding,
        )
        .unwrap();
        let authority = reopen_inactive_bootstrap_accepted_authority(
            &verified,
            ObjectStore::open(&archive_root, workspace).unwrap(),
        )
        .unwrap();

        let device_root = root.path().join("device-local");
        fs::create_dir(&device_root).unwrap();
        let roots = MigrationBackupRoot::open(&device_root, &graph_root).unwrap();
        let backup = verify_migration_source_backup(&roots, &prepared, &verified).unwrap();
        let runtime = ApplicationRuntimeRoot::open_for_test(&root.path().join("runtime")).unwrap();
        let (sqlite, sqlite_proof) = SqliteFrontier::open_or_rebuild_inactive_bootstrap(
            &root.path().join("bootstrap.sqlite"),
            &runtime,
            &authority,
        )
        .unwrap();
        let archive_resource_id = authority
            .store()
            .provision_enrolled_archive_resource_id()
            .unwrap();
        let shadow = verify_inactive_bootstrap_shadow_projection(
            &graph,
            &roots,
            &prepared,
            &verified,
            &backup,
            &authority,
            &sqlite,
            &sqlite_proof,
        )
        .unwrap();

        Self {
            root,
            graph_root,
            graph,
            receipts,
            archive_root,
            workspace,
            lineage,
            catalog_document_id,
            prepared,
            verified,
            authority,
            roots,
            backup,
            sqlite: Some(sqlite),
            sqlite_proof,
            archive_resource_id,
            shadow,
            preparation: PreparationId::new(),
            original_graph,
        }
    }

    fn sqlite(&self) -> &OpenProjection {
        self.sqlite
            .as_ref()
            .expect("retained inactive bootstrap projection")
    }

    /// Drop the retained inactive bootstrap SQLite projection, releasing the
    /// one-per-workspace applier lease a promoted projection needs.
    fn release_bootstrap_projection(&mut self) {
        self.sqlite = None;
    }

    fn proofs(&self) -> VerifiedLocalProofSet<'_> {
        VerifiedLocalProofSet {
            graph: &self.graph,
            roots: &self.roots,
            prepared: &self.prepared,
            verified_publication: &self.verified,
            source_backup: &self.backup,
            accepted_authority: &self.authority,
            sqlite: self.sqlite(),
            sqlite_projection: &self.sqlite_proof,
            shadow_projection: &self.shadow,
        }
    }

    fn enrollment_binding(&self) -> EnrollmentBindingV1 {
        let accepted = self.authority.binding();
        let storage = accepted.storage_binding();
        EnrollmentBindingV1::new(
            accepted.workspace_id(),
            accepted.lineage_digest(),
            self.verified.catalog_document_id(),
            storage.endpoint.endpoint_id(),
            storage.endpoint.device_id(),
            accepted.graph_resource(),
            storage.receipt_store_id,
            self.archive_resource_id,
            self.graph.graph_text_scope_binding().unwrap(),
        )
        .unwrap()
    }

    fn enrollment_root(&self, label: &str) -> EnrollmentApplicationRoot {
        enrollment_application_root_for_test(
            &self
                .root
                .path()
                .join(format!("enrollment-{}-{label}", Uuid::new_v4())),
        )
        .unwrap()
    }

    fn runtime(&self) -> LocalActiveRuntime<'_> {
        LocalActiveRuntime {
            engine: self.authority.accepted_engine(),
            projection: self.sqlite(),
        }
    }

    fn archive(&self) -> ObjectStore {
        ObjectStore::open(&self.archive_root, self.workspace).unwrap()
    }

    /// A live ordinary runtime engine enrolled to the exact endpoint, receipt
    /// store, workspace, lineage, and catalog document this enrollment binds.
    ///
    /// It is opened over a separate ordinary archive root on purpose: an
    /// inactive-bootstrap archive is explicitly fenced from ordinary runtime
    /// opening ("inactive bootstrap history cannot be opened as ordinary
    /// runtime"), and promoting it is a later packet. The gate under test here
    /// is the runtime identity/enrollment binding, which this engine reproduces
    /// exactly.
    fn runtime_engine(&self, label: &str) -> ShardedHotEngine {
        let archive_root = self.root.path().join(format!("runtime-archive-{label}"));
        ShardedHotEngine::with_enrolled_projection(
            ObjectStore::open(&archive_root, self.workspace).unwrap(),
            self.lineage,
            self.catalog_document_id,
            &self.graph,
            &self.receipts,
        )
    }

    /// A device-local SQLite projection bound to one exact live runtime engine.
    fn runtime_projection(
        &self,
        engine: &ShardedHotEngine,
        archive: &ObjectStore,
        label: &str,
    ) -> SqliteFrontier {
        let runtime =
            ApplicationRuntimeRoot::open_for_test(&self.root.path().join(format!("rt-{label}")))
                .unwrap();
        SqliteFrontier::open_or_rebuild(
            &self.root.path().join(format!("rt-{label}.sqlite")),
            &runtime,
            ProjectionClaim::current(self.workspace, self.lineage),
            RebuildSource::new(engine, archive).unwrap(),
        )
        .unwrap()
        .database
    }

    /// A fresh device-local reconciliation baseline bound to this exact
    /// enrolled workspace, endpoint, graph resource, and graph-text scope.
    fn reconciliation_baseline(&self, label: &str) -> ReconciliationBaseline {
        let runtime = ApplicationRuntimeRoot::open_for_test(
            &self.root.path().join(format!("baseline-rt-{label}")),
        )
        .unwrap();
        let binding = ReconciliationBaselineBinding::new(
            self.workspace,
            self.authority
                .binding()
                .storage_binding()
                .endpoint
                .endpoint_id(),
            self.graph.canonical_resource_id().unwrap(),
            self.graph.graph_text_scope_binding().unwrap(),
        )
        .unwrap();
        ReconciliationBaseline::create_fresh(
            &TrustedPrivateApplicationRuntimeRoot::from_application_runtime_root(&runtime),
            binding,
        )
        .unwrap()
    }

    fn compose(&self, root: &EnrollmentApplicationRoot) -> VerifiedLocalEvidence {
        compose_verified_local(
            root,
            self.enrollment_binding(),
            self.preparation,
            &self.proofs(),
        )
        .unwrap()
    }

    fn assert_graph_unchanged(&self) {
        assert_eq!(snapshot_files(&self.graph_root), self.original_graph);
    }
}

fn snapshot_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut output = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let mut entries = fs::read_dir(&directory)
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                stack.push(path);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                output.insert(relative, fs::read(path).unwrap());
            }
        }
    }
    output
}

/// Byte identity of a directory, reported compactly so a failure prints
/// digests instead of whole databases.
fn snapshot_file_digests(root: &Path) -> BTreeMap<String, ContentDigest> {
    snapshot_files(root)
        .into_iter()
        .map(|(path, bytes)| (path, ContentDigest::of(&bytes)))
        .collect()
}

/// Durable byte identity of a SQLite database directory.
///
/// The `-shm` sidecar is a volatile shared-memory index that ordinary read
/// transactions legitimately update, so durable identity covers the database
/// file and its write-ahead log, where every committed row actually lands.
fn durable_sqlite_digests(directory: &Path) -> BTreeMap<String, ContentDigest> {
    snapshot_file_digests(directory)
        .into_iter()
        .filter(|(name, _)| !name.ends_with("-shm"))
        .collect()
}

/// Nested, non-standard, Unicode, CRLF, BOM, and multi-chunk graph layout.
fn rich_fixture(label: &str) -> Fixture {
    let mut deep = String::from("notes");
    for ordinal in 0..80 {
        deep.push_str(&format!("/層{ordinal:02}"));
    }
    deep.push_str("/Déjà___計画.markdown");
    Fixture::new(
        label,
        Some(
            br#"{:pages-directory "notes"
                :journals-directory "diary"
                :file/name-format :triple-lowbar
                :journal/file-name-format "dd-MM-yyyy"
                :journal/page-title-format "yyyy-MM-dd"}"#,
        ),
        vec![
            (
                "Root.md".into(),
                b"title:: Root logical\r\n\r\n- CRLF\r\n".to_vec(),
            ),
            (
                "notes/a/same.md".into(),
                b"- same bytes, distinct identity\n".to_vec(),
            ),
            (
                "notes/b/same-copy.org".into(),
                b"- same bytes, distinct identity\n".to_vec(),
            ),
            (deep, "\u{feff}- Unicode caf\u{e9}\r\n".as_bytes().to_vec()),
            ("diary/nested/25-07-2026.org".into(), Vec::new()),
        ],
    )
}

fn enrollment_head(
    root: &EnrollmentApplicationRoot,
    binding: &EnrollmentBindingV1,
) -> ContentDigest {
    match EnrollmentReader::open_existing(root, binding).unwrap() {
        EnrollmentOpen::Present(reader) => reader.current().digest(),
        EnrollmentOpen::Absent => panic!("expected an enrollment head"),
    }
}

fn enrollment_generation(root: &EnrollmentApplicationRoot, binding: &EnrollmentBindingV1) -> u64 {
    match EnrollmentReader::open_existing(root, binding).unwrap() {
        EnrollmentOpen::Present(reader) => reader.current().generation(),
        EnrollmentOpen::Absent => panic!("expected an enrollment head"),
    }
}

fn find_file_with_prefix(root: &Path, prefix: &str) -> PathBuf {
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(directory).unwrap().map(Result::unwrap) {
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else if entry.file_name().to_string_lossy().starts_with(prefix) {
                return entry.path();
            }
        }
    }
    panic!("missing file with prefix {prefix}");
}

#[test]
fn activation_of_zero_one_and_multipart_verified_local_is_exact_and_writes_no_graph_bytes() {
    let mut multipart_bytes = Vec::new();
    for ordinal in 0..4096 {
        multipart_bytes.extend_from_slice(format!("- operation {ordinal:04}\n").as_bytes());
    }
    let cases = [
        Fixture::new("zero", None, Vec::new()),
        Fixture::new(
            "one",
            None,
            vec![("pages/one.md".into(), b"- one\n".to_vec())],
        ),
        Fixture::new(
            "multipart-4096",
            None,
            vec![("pages/multipart.md".into(), multipart_bytes)],
        ),
        rich_fixture("rich-nested-unicode"),
    ];
    // Zero, one, and genuinely multipart bootstraps must all activate.
    assert_eq!(cases[0].verified.part_count(), 0);
    assert_eq!(cases[1].verified.part_count(), 1);
    assert!(cases[2].verified.part_count() >= 2);

    for fixture in &cases {
        let root = fixture.enrollment_root("activate");
        let binding = fixture.enrollment_binding();
        let evidence = fixture.compose(&root);
        let verified_head = evidence.enrollment_head();
        let verification_digest = evidence.verification_digest();
        let session = SessionId::new();

        let before = snapshot_files(&fixture.graph_root);
        let authority = activate_verified_local(
            &root,
            evidence,
            session,
            &fixture.proofs(),
            &fixture.runtime(),
        )
        .unwrap();

        assert_eq!(authority.session_id(), session);
        assert_eq!(authority.verification_digest(), verification_digest);
        assert_eq!(
            authority.handoff(),
            LocalActiveHandoff::Unsafe {
                session_id: session
            }
        );
        assert_eq!(authority.binding(), &binding);
        assert_eq!(
            authority.enrollment_head(),
            enrollment_head(&root, &binding)
        );
        assert_ne!(authority.enrollment_head(), verified_head);

        // Activation changes only device-local enrollment/runtime state.
        assert_eq!(snapshot_files(&fixture.graph_root), before);
        fixture.assert_graph_unchanged();
    }
}

/// A genuine process restart: every in-memory `VerifiedLocalEvidence` and
/// `LocalActiveAuthority` is destroyed before the reopen, which therefore has
/// nothing but the durable enrollment chain, the retained proof set, and the
/// live runtime handles to work from.
#[test]
fn restart_reopens_local_active_from_durable_state_without_any_retained_evidence() {
    let mut multipart_bytes = Vec::new();
    for ordinal in 0..4096 {
        multipart_bytes.extend_from_slice(format!("- operation {ordinal:04}\n").as_bytes());
    }
    let cases = [
        Fixture::new("restart-zero", None, Vec::new()),
        Fixture::new(
            "restart-one",
            None,
            vec![("pages/one.md".into(), b"- one\n".to_vec())],
        ),
        Fixture::new(
            "restart-multipart-4096",
            None,
            vec![("pages/multipart.md".into(), multipart_bytes)],
        ),
        rich_fixture("restart-rich-nested-unicode"),
    ];
    assert_eq!(cases[0].verified.part_count(), 0);
    assert_eq!(cases[1].verified.part_count(), 1);
    assert!(cases[2].verified.part_count() >= 2);

    for fixture in &cases {
        let root = fixture.enrollment_root("restart");
        let binding = fixture.enrollment_binding();
        let session = SessionId::new();
        let evidence = fixture.compose(&root);
        let verified_head = evidence.enrollment_head();
        let verification_digest = evidence.verification_digest();

        let authority = activate_verified_local(
            &root,
            evidence,
            session,
            &fixture.proofs(),
            &fixture.runtime(),
        )
        .unwrap();
        let activated_head = authority.enrollment_head();
        let activated_generation = enrollment_generation(&root, &binding);
        // The previous process is gone: `evidence` was consumed by the
        // activation and the authority is dropped here. Nothing below may
        // depend on either.
        drop(authority);

        // The predecessor boundaries genuinely cannot reconstruct this state:
        // the VerifiedLocal reopen refuses a committed LocalActive head, and
        // the LocalActive record reopen requires the evidence this process no
        // longer has.
        assert!(crate::oplog::enrollment::reopen_verified_local(
            &root,
            &binding,
            &fixture.proofs()
        )
        .is_err());

        let reopened = reopen_local_active_authority(
            &root,
            &binding,
            session,
            &fixture.proofs(),
            &fixture.runtime(),
        )
        .unwrap();
        assert_eq!(reopened.session_id(), session);
        assert_eq!(reopened.enrollment_head(), activated_head);
        assert_eq!(reopened.verification_digest(), verification_digest);
        assert_eq!(
            reopened.handoff(),
            LocalActiveHandoff::Unsafe {
                session_id: session
            }
        );
        assert_eq!(reopened.binding(), &binding);
        assert_ne!(activated_head, verified_head);
        // A reopen of a committed Unsafe record persists nothing at all.
        assert_eq!(enrollment_head(&root, &binding), activated_head);
        assert_eq!(enrollment_generation(&root, &binding), activated_generation);
        drop(reopened);

        // Any other requested session fails closed and never advances.
        assert!(
            matches!(
                reopen_local_active_authority(
                    &root,
                    &binding,
                    SessionId::new(),
                    &fixture.proofs(),
                    &fixture.runtime(),
                ),
                Err(LocalActivationError::Enrollment(
                    VerifiedLocalCompositionError::CompetingSession
                ))
            ),
            "a competing restart session must fail closed"
        );
        assert_eq!(enrollment_head(&root, &binding), activated_head);
        assert_eq!(enrollment_generation(&root, &binding), activated_generation);

        // Repeating the exact restart stays idempotent.
        let again = reopen_local_active_authority(
            &root,
            &binding,
            session,
            &fixture.proofs(),
            &fixture.runtime(),
        )
        .unwrap();
        assert_eq!(again.enrollment_head(), activated_head);
        assert_eq!(enrollment_generation(&root, &binding), activated_generation);

        // A LocalActive enrollment can never be recomposed as VerifiedLocal.
        assert!(compose_verified_local(
            &root,
            binding.clone(),
            fixture.preparation,
            &fixture.proofs(),
        )
        .is_err());
        assert_eq!(enrollment_head(&root, &binding), activated_head);
        fixture.assert_graph_unchanged();
    }
}

/// One coherent live runtime: engine, device-local SQLite, and tail overlay
/// over a single engine identity, as the safe-handoff drain requires.
struct LiveRuntime {
    engine: ShardedHotEngine,
    database: SqliteFrontier,
    tail: TailOverlay,
}

impl LiveRuntime {
    fn open(fixture: &Fixture, label: &str) -> Self {
        let engine = fixture.runtime_engine(label);
        let archive = ObjectStore::open(
            &fixture.root.path().join(format!("runtime-archive-{label}")),
            fixture.workspace,
        )
        .unwrap();
        let database = fixture.runtime_projection(&engine, &archive, label);
        let source = RebuildSource::new(&engine, &archive).unwrap();
        let tail = TailOverlay::from_durable(&database, &source).unwrap();
        Self {
            engine,
            database,
            tail,
        }
    }

    fn mark_safe(&self, fixture: &Fixture, authority: &mut LocalActiveAuthority) -> ContentDigest {
        authority
            .quiesce_and_mark_safe_without_watcher_dependency(
                &fixture.graph,
                &self.engine,
                &self.database,
                &self.tail,
            )
            .unwrap()
            .enrollment_head()
    }
}

/// A restart over a cleanly handed-off `Safe` record may adopt exactly one new
/// session, through the ordinary durable record/head protocol.
#[test]
fn restart_from_a_safe_handoff_adopts_exactly_the_requested_new_session() {
    let fixture = Fixture::new("restart-safe", None, Vec::new());
    let root = fixture.enrollment_root("restart-safe");
    let binding = fixture.enrollment_binding();
    let runtime = LiveRuntime::open(&fixture, "restart-safe");

    let first_session = SessionId::new();
    let mut authority = activate_verified_local(
        &root,
        fixture.compose(&root),
        first_session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    let safe_head = runtime.mark_safe(&fixture, &mut authority);
    let safe_generation = enrollment_generation(&root, &binding);
    assert_eq!(authority.handoff(), LocalActiveHandoff::Safe);
    // The previous process is gone.
    drop(authority);

    let second_session = SessionId::new();
    let mut reopened = reopen_local_active_authority(
        &root,
        &binding,
        second_session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    assert_eq!(
        reopened.handoff(),
        LocalActiveHandoff::Unsafe {
            session_id: second_session
        }
    );
    assert_ne!(reopened.enrollment_head(), safe_head);
    assert_eq!(reopened.enrollment_head(), enrollment_head(&root, &binding));
    assert_eq!(enrollment_generation(&root, &binding), safe_generation + 1);
    let resumed_head = reopened.enrollment_head();

    // The reopened value is a genuine authority: it admits a live mutation
    // window over the exact enrolled runtime.
    {
        let permit = reopened
            .admit_local_mutation(&fixture.graph, &runtime.engine)
            .unwrap();
        assert_eq!(permit.session_id(), second_session);
        assert_eq!(permit.enrollment_head(), resumed_head);
    }
    assert_eq!(enrollment_generation(&root, &binding), safe_generation + 1);
    drop(reopened);

    // The same session restarts idempotently: no second transition.
    let again = reopen_local_active_authority(
        &root,
        &binding,
        second_session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    assert_eq!(again.enrollment_head(), resumed_head);
    assert_eq!(enrollment_generation(&root, &binding), safe_generation + 1);
    drop(again);

    // A third session cannot take over the committed Unsafe record.
    assert!(matches!(
        reopen_local_active_authority(
            &root,
            &binding,
            SessionId::new(),
            &fixture.proofs(),
            &fixture.runtime(),
        ),
        Err(LocalActivationError::Enrollment(
            VerifiedLocalCompositionError::CompetingSession
        ))
    ));
    assert_eq!(enrollment_head(&root, &binding), resumed_head);
    assert_eq!(enrollment_generation(&root, &binding), safe_generation + 1);
    fixture.assert_graph_unchanged();
}

/// The reopen never assumes that the committed `LocalActive` record directly
/// succeeds `VerifiedLocal`: it traverses any valid sequence of `Safe`/`Unsafe`
/// handoff records back to the exact predecessor.
#[test]
fn restart_traverses_a_long_valid_handoff_record_chain() {
    let fixture = Fixture::new("restart-chain", None, Vec::new());
    let root = fixture.enrollment_root("restart-chain");
    let binding = fixture.enrollment_binding();
    let runtime = LiveRuntime::open(&fixture, "restart-chain");

    let mut authority = activate_verified_local(
        &root,
        fixture.compose(&root),
        SessionId::new(),
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    let mut session = SessionId::new();
    for _ in 0..3 {
        runtime.mark_safe(&fixture, &mut authority);
        drop(authority);
        session = SessionId::new();
        authority = reopen_local_active_authority(
            &root,
            &binding,
            session,
            &fixture.proofs(),
            &fixture.runtime(),
        )
        .unwrap();
    }
    let head = authority.enrollment_head();
    drop(authority);

    // ShadowImport, VerifiedLocal, the original activation, and three
    // Safe/Unsafe handoff pairs.
    assert_eq!(enrollment_generation(&root, &binding), 9);

    let reopened = reopen_local_active_authority(
        &root,
        &binding,
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    assert_eq!(reopened.enrollment_head(), head);
    assert_eq!(
        reopened.handoff(),
        LocalActiveHandoff::Unsafe {
            session_id: session
        }
    );
    assert_eq!(enrollment_generation(&root, &binding), 9);
    fixture.assert_graph_unchanged();
}

/// Every durability cut of the `Safe -> Unsafe { new session }` reopen
/// transition leaves exactly one resumable head.
#[test]
fn restart_from_safe_at_every_durability_cut_resumes_one_exact_head() {
    let fixture = Fixture::new("restart-cuts", None, Vec::new());
    let runtime = LiveRuntime::open(&fixture, "restart-cuts");
    let cuts = [
        CommitCut::AfterRecordTempCreate,
        CommitCut::AfterRecordWrite,
        CommitCut::AfterRecordFileSync,
        CommitCut::AfterRecordLink,
        CommitCut::AfterRecordInsert,
        CommitCut::AfterRecordsDirectorySync,
        CommitCut::AfterHeadTempCreate,
        CommitCut::AfterHeadWrite,
        CommitCut::AfterHeadFileSync,
        CommitCut::AfterHeadReplace,
        CommitCut::AfterEnrollmentDirectorySync,
    ];
    for cut in cuts {
        let root = fixture.enrollment_root("restart-cut");
        let binding = fixture.enrollment_binding();
        let mut authority = activate_verified_local(
            &root,
            fixture.compose(&root),
            SessionId::new(),
            &fixture.proofs(),
            &fixture.runtime(),
        )
        .unwrap();
        let verification_digest = authority.verification_digest();
        let safe_head = runtime.mark_safe(&fixture, &mut authority);
        drop(authority);

        let session = SessionId::new();
        let interrupted = super::reopen_local_active_authority_at_cut_for_test(
            &root,
            &binding,
            session,
            &fixture.proofs(),
            &fixture.runtime(),
            cut,
        );
        assert!(interrupted.is_err(), "{cut:?} must not return an authority");

        // Whatever the cut left behind, the durable state is either still the
        // exact Safe predecessor or exactly one Unsafe successor for this
        // requested session. Both resume to one head.
        let head_after_crash = enrollment_head(&root, &binding);
        let committed = crate::oplog::enrollment::reopen_committed_local_active_for_session(
            &root,
            &binding,
            verification_digest,
        )
        .unwrap();
        assert_eq!(committed.sync(), LocalActiveSync::Idle, "{cut:?}");
        match committed.handoff() {
            LocalActiveHandoff::Safe => assert_eq!(head_after_crash, safe_head, "{cut:?}"),
            LocalActiveHandoff::Unsafe { session_id } => {
                assert_eq!(session_id, session, "{cut:?}");
                assert_ne!(head_after_crash, safe_head, "{cut:?}");
            }
        }

        let resumed = reopen_local_active_authority(
            &root,
            &binding,
            session,
            &fixture.proofs(),
            &fixture.runtime(),
        )
        .unwrap();
        assert_eq!(
            resumed.handoff(),
            LocalActiveHandoff::Unsafe {
                session_id: session
            },
            "{cut:?}"
        );
        assert_eq!(resumed.enrollment_head(), enrollment_head(&root, &binding));
        assert_ne!(resumed.enrollment_head(), safe_head, "{cut:?}");
    }
    fixture.assert_graph_unchanged();
}

/// Every wrong durable lifecycle, malformed head, mixed proof set, foreign
/// binding, and cross-bound runtime fails the restart closed.
#[test]
fn restart_reopen_rejects_wrong_state_proofs_bindings_and_runtimes() {
    let fixture = Fixture::new(
        "restart-reject",
        None,
        vec![("pages/reject.md".into(), b"- reject\n".to_vec())],
    );
    let other = Fixture::new(
        "restart-reject-other",
        None,
        vec![("pages/other.md".into(), b"- other\n".to_vec())],
    );
    let binding = fixture.enrollment_binding();

    // Absent enrollment.
    let absent = fixture.enrollment_root("reject-absent");
    assert!(reopen_local_active_authority(
        &absent,
        &binding,
        SessionId::new(),
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .is_err());

    // A committed ShadowImport head, left behind by an interrupted
    // VerifiedLocal composition.
    let shadow_root = fixture.enrollment_root("reject-shadow");
    assert!(
        crate::oplog::enrollment::compose_verified_local_at_cut_for_test(
            &shadow_root,
            binding.clone(),
            fixture.preparation,
            &fixture.proofs(),
            CommitCut::AfterRecordWrite,
        )
        .is_err()
    );
    let shadow_head = enrollment_head(&shadow_root, &binding);
    assert!(reopen_local_active_authority(
        &shadow_root,
        &binding,
        SessionId::new(),
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .is_err());
    assert_eq!(enrollment_head(&shadow_root, &binding), shadow_head);

    // A committed VerifiedLocal head is not LocalActive authority.
    let verified_root = fixture.enrollment_root("reject-verified");
    let verified_head = fixture.compose(&verified_root).enrollment_head();
    assert!(reopen_local_active_authority(
        &verified_root,
        &binding,
        SessionId::new(),
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .is_err());
    assert_eq!(enrollment_head(&verified_root, &binding), verified_head);

    // One genuinely activated enrollment for the remaining cases.
    let root = fixture.enrollment_root("reject-active");
    let session = SessionId::new();
    let authority = activate_verified_local(
        &root,
        fixture.compose(&root),
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    let active_head = authority.enrollment_head();
    let verification_digest = authority.verification_digest();
    let active_generation = enrollment_generation(&root, &binding);
    drop(authority);

    // Every single-proof substitution from a second genuinely enrolled graph.
    for proofs in [
        VerifiedLocalProofSet {
            accepted_authority: &other.authority,
            ..fixture.proofs()
        },
        VerifiedLocalProofSet {
            sqlite_projection: &other.sqlite_proof,
            ..fixture.proofs()
        },
        VerifiedLocalProofSet {
            shadow_projection: &other.shadow,
            ..fixture.proofs()
        },
        VerifiedLocalProofSet {
            source_backup: &other.backup,
            ..fixture.proofs()
        },
        VerifiedLocalProofSet {
            roots: &other.roots,
            ..fixture.proofs()
        },
        VerifiedLocalProofSet {
            graph: &other.graph,
            ..fixture.proofs()
        },
    ] {
        assert!(
            reopen_local_active_authority(&root, &binding, session, &proofs, &fixture.runtime())
                .is_err(),
            "a mixed proof set must never reopen an authority"
        );
        assert_eq!(enrollment_head(&root, &binding), active_head);
    }

    // A cross-bound runtime and a foreign enrollment binding.
    let foreign_runtime = LocalActiveRuntime {
        engine: other.authority.accepted_engine(),
        projection: other.sqlite(),
    };
    assert!(reopen_local_active_authority(
        &root,
        &binding,
        session,
        &fixture.proofs(),
        &foreign_runtime,
    )
    .is_err());
    assert!(reopen_local_active_authority(
        &root,
        &other.enrollment_binding(),
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .is_err());
    assert_eq!(enrollment_head(&root, &binding), active_head);
    assert_eq!(enrollment_generation(&root, &binding), active_generation);

    // A non-Idle (published) sync state never reopens.
    let published_root = fixture.enrollment_root("reject-published");
    let published_session = SessionId::new();
    let published_authority = activate_verified_local(
        &published_root,
        fixture.compose(&published_root),
        published_session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    let published_head = crate::oplog::enrollment::publish_local_active_for_test(
        &published_root,
        &binding,
        published_authority.enrollment_head(),
        published_authority.verification_digest(),
        published_session,
    )
    .unwrap();
    drop(published_authority);
    let published_error = reopen_local_active_authority(
        &published_root,
        &binding,
        published_session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap_err();
    assert!(
        matches!(
            published_error,
            LocalActivationError::Enrollment(VerifiedLocalCompositionError::WrongLifecycle(
                detail
            )) if detail.contains("Idle")
        ),
        "unexpected published-state outcome: {published_error}"
    );
    assert_eq!(enrollment_head(&published_root, &binding), published_head);

    // A blocked enrollment never reopens.
    let blocked_root = fixture.enrollment_root("reject-blocked");
    let blocked_session = SessionId::new();
    let blocked_authority = activate_verified_local(
        &blocked_root,
        fixture.compose(&blocked_root),
        blocked_session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    let blocked_head = crate::oplog::enrollment::block_current_for_test(
        &blocked_root,
        &binding,
        blocked_authority.enrollment_head(),
        "restart.test".into(),
    )
    .unwrap();
    drop(blocked_authority);
    assert!(reopen_local_active_authority(
        &blocked_root,
        &binding,
        blocked_session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .is_err());
    assert_eq!(enrollment_head(&blocked_root, &binding), blocked_head);

    // A truncated committed head is malformed and never reopens.
    let truncated_root = fixture.enrollment_root("reject-truncated");
    let truncated_session = SessionId::new();
    drop(
        activate_verified_local(
            &truncated_root,
            fixture.compose(&truncated_root),
            truncated_session,
            &fixture.proofs(),
            &fixture.runtime(),
        )
        .unwrap(),
    );
    let head_file = find_file_with_prefix(truncated_root.path(), "head");
    assert_eq!(head_file.file_name().unwrap(), "head");
    fs::OpenOptions::new()
        .write(true)
        .open(&head_file)
        .unwrap()
        .set_len(7)
        .unwrap();
    assert!(reopen_local_active_authority(
        &truncated_root,
        &binding,
        truncated_session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .is_err());

    // The genuine restart still works, and the retained verification digest is
    // unchanged throughout.
    let reopened = reopen_local_active_authority(
        &root,
        &binding,
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    assert_eq!(reopened.enrollment_head(), active_head);
    assert_eq!(reopened.verification_digest(), verification_digest);
    fixture.assert_graph_unchanged();
    other.assert_graph_unchanged();
}

#[test]
fn activation_resume_requires_the_exact_session_and_rejects_a_competing_one() {
    let fixture = Fixture::new(
        "competing-session",
        None,
        vec![("pages/session.md".into(), b"- session\n".to_vec())],
    );
    let root = fixture.enrollment_root("session");
    let binding = fixture.enrollment_binding();
    let evidence = fixture.compose(&root);
    let retained = fixture.compose(&root);
    let session = SessionId::new();

    let authority = activate_verified_local(
        &root,
        evidence,
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    let activated_head = authority.enrollment_head();
    let activated_generation = enrollment_generation(&root, &binding);
    drop(authority);

    // The identical retained evidence under a competing session fails closed
    // and never advances the committed head.
    let competing = activate_verified_local(
        &root,
        retained,
        SessionId::new(),
        &fixture.proofs(),
        &fixture.runtime(),
    );
    assert!(
        matches!(
            competing,
            Err(LocalActivationError::Enrollment(
                VerifiedLocalCompositionError::CompetingSession
            ))
        ),
        "a competing activation session must fail closed"
    );
    assert_eq!(enrollment_head(&root, &binding), activated_head);
    assert_eq!(enrollment_generation(&root, &binding), activated_generation);
    fixture.assert_graph_unchanged();
}

#[test]
fn activation_rejects_stale_and_cross_bound_evidence_without_advancing() {
    let first = Fixture::new(
        "cross-first",
        None,
        vec![("pages/first.md".into(), b"- first\n".to_vec())],
    );
    let second = Fixture::new(
        "cross-second",
        None,
        vec![("pages/second.md".into(), b"- second\n".to_vec())],
    );

    let root = first.enrollment_root("cross");
    let binding = first.enrollment_binding();
    let evidence = first.compose(&root);
    let verified_head = evidence.enrollment_head();

    // Every single-proof substitution from a second genuinely enrolled graph
    // fails closed and leaves the committed VerifiedLocal head untouched.
    for proofs in [
        VerifiedLocalProofSet {
            accepted_authority: &second.authority,
            ..first.proofs()
        },
        VerifiedLocalProofSet {
            sqlite_projection: &second.sqlite_proof,
            ..first.proofs()
        },
        VerifiedLocalProofSet {
            shadow_projection: &second.shadow,
            ..first.proofs()
        },
        VerifiedLocalProofSet {
            source_backup: &second.backup,
            ..first.proofs()
        },
        VerifiedLocalProofSet {
            roots: &second.roots,
            ..first.proofs()
        },
        VerifiedLocalProofSet {
            graph: &second.graph,
            ..first.proofs()
        },
    ] {
        let attempt = activate_verified_local(
            &root,
            first.compose(&root),
            SessionId::new(),
            &proofs,
            &first.runtime(),
        );
        assert!(attempt.is_err(), "mixed proof sets must never activate");
        assert_eq!(enrollment_head(&root, &binding), verified_head);
    }

    // A runtime component from the other enrollment is also refused.
    let foreign_runtime = LocalActiveRuntime {
        engine: second.authority.accepted_engine(),
        projection: second.sqlite(),
    };
    assert!(activate_verified_local(
        &root,
        first.compose(&root),
        SessionId::new(),
        &first.proofs(),
        &foreign_runtime,
    )
    .is_err());
    assert_eq!(enrollment_head(&root, &binding), verified_head);

    // The genuine proof set and runtime still activate exactly once.
    let authority = activate_verified_local(
        &root,
        evidence,
        SessionId::new(),
        &first.proofs(),
        &first.runtime(),
    )
    .unwrap();
    assert_ne!(authority.enrollment_head(), verified_head);
    first.assert_graph_unchanged();
    second.assert_graph_unchanged();
}

#[test]
fn activation_at_every_enrollment_durability_cut_resumes_one_exact_head() {
    let fixture = Fixture::new(
        "activation-cuts",
        None,
        vec![("pages/cuts.md".into(), b"- durability\n".to_vec())],
    );
    let cuts = [
        CommitCut::AfterRecordTempCreate,
        CommitCut::AfterRecordWrite,
        CommitCut::AfterRecordFileSync,
        CommitCut::AfterRecordLink,
        CommitCut::AfterRecordInsert,
        CommitCut::AfterRecordsDirectorySync,
        CommitCut::AfterHeadTempCreate,
        CommitCut::AfterHeadWrite,
        CommitCut::AfterHeadFileSync,
        CommitCut::AfterHeadReplace,
        CommitCut::AfterEnrollmentDirectorySync,
    ];
    for cut in cuts {
        let root = fixture.enrollment_root("cut");
        let binding = fixture.enrollment_binding();
        let evidence = fixture.compose(&root);
        let verified_head = evidence.enrollment_head();
        let verification_digest = evidence.verification_digest();
        let session = SessionId::new();

        let interrupted = super::activate_verified_local_at_cut_for_test(
            &root,
            evidence,
            session,
            &fixture.proofs(),
            &fixture.runtime(),
            cut,
        );
        assert!(interrupted.is_err(), "{cut:?} must not return an authority");

        // Whatever the cut left behind, a crash always resumes conservatively
        // to exactly one head: either still VerifiedLocal, or the exact
        // Unsafe+Idle LocalActive record for this session.
        let head_after_crash = enrollment_head(&root, &binding);
        match crate::oplog::enrollment::reopen_verified_local(&root, &binding, &fixture.proofs()) {
            Ok(evidence) => {
                assert_eq!(evidence.enrollment_head(), verified_head, "{cut:?}");
                let resumed = activate_verified_local(
                    &root,
                    evidence,
                    session,
                    &fixture.proofs(),
                    &fixture.runtime(),
                )
                .unwrap();
                assert_eq!(resumed.enrollment_head(), enrollment_head(&root, &binding));
                assert_eq!(
                    resumed.handoff(),
                    LocalActiveHandoff::Unsafe {
                        session_id: session
                    }
                );
            }
            Err(_) => {
                // The head already advanced past VerifiedLocal, so the record
                // must be exactly this session's Unsafe+Idle activation.
                assert_ne!(head_after_crash, verified_head, "{cut:?}");
                let committed =
                    crate::oplog::enrollment::reopen_committed_local_active_for_session(
                        &root,
                        &binding,
                        verification_digest,
                    )
                    .unwrap();
                assert_eq!(
                    committed.handoff(),
                    LocalActiveHandoff::Unsafe {
                        session_id: session
                    },
                    "{cut:?}"
                );
                assert_eq!(committed.sync(), LocalActiveSync::Idle, "{cut:?}");
                assert_eq!(committed.enrollment_head(), head_after_crash);
            }
        }
    }
    fixture.assert_graph_unchanged();
}

#[test]
fn activation_partial_record_and_head_temporaries_fail_closed_or_resume_exactly() {
    let fixture = Fixture::new(
        "activation-partial",
        None,
        vec![("pages/partial.md".into(), b"- partial\n".to_vec())],
    );

    // A truncated record temporary is ambiguous and must never advance.
    let record_root = fixture.enrollment_root("partial-record");
    let binding = fixture.enrollment_binding();
    let record_evidence = fixture.compose(&record_root);
    let verified_head = record_evidence.enrollment_head();
    assert!(super::activate_verified_local_at_cut_for_test(
        &record_root,
        record_evidence,
        SessionId::new(),
        &fixture.proofs(),
        &fixture.runtime(),
        CommitCut::AfterRecordWrite,
    )
    .is_err());
    let temp = find_file_with_prefix(record_root.path(), ".record-tmp-");
    let length = fs::metadata(&temp).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(&temp)
        .unwrap()
        .set_len(length / 2)
        .unwrap();
    let resumed =
        crate::oplog::enrollment::reopen_verified_local(&record_root, &binding, &fixture.proofs())
            .unwrap();
    assert_eq!(resumed.enrollment_head(), verified_head);
    let session = SessionId::new();
    // A stranded partial record never yields a divergent head: activation either
    // fails closed at the exact VerifiedLocal head, or commits exactly one
    // Unsafe+Idle LocalActive record for this session.
    match activate_verified_local(
        &record_root,
        resumed,
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    ) {
        Ok(authority) => {
            assert_eq!(
                authority.enrollment_head(),
                enrollment_head(&record_root, &binding)
            );
            assert_eq!(
                authority.handoff(),
                LocalActiveHandoff::Unsafe {
                    session_id: session
                }
            );
        }
        Err(_) => assert_eq!(enrollment_head(&record_root, &binding), verified_head),
    }

    // A truncated head temporary is discardable, so activation resumes.
    let head_root = fixture.enrollment_root("partial-head");
    let head_evidence = fixture.compose(&head_root);
    let head_session = SessionId::new();
    assert!(super::activate_verified_local_at_cut_for_test(
        &head_root,
        head_evidence,
        head_session,
        &fixture.proofs(),
        &fixture.runtime(),
        CommitCut::AfterHeadWrite,
    )
    .is_err());
    let head_temp = find_file_with_prefix(head_root.path(), ".head-tmp-");
    fs::OpenOptions::new()
        .write(true)
        .open(&head_temp)
        .unwrap()
        .set_len(7)
        .unwrap();
    let evidence =
        crate::oplog::enrollment::reopen_verified_local(&head_root, &binding, &fixture.proofs())
            .unwrap();
    let authority = activate_verified_local(
        &head_root,
        evidence,
        head_session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    assert_eq!(
        authority.enrollment_head(),
        enrollment_head(&head_root, &binding)
    );
    fixture.assert_graph_unchanged();
}

#[test]
fn blocked_and_non_verified_lifecycles_never_activate() {
    let fixture = Fixture::new(
        "blocked-lifecycle",
        None,
        vec![("pages/blocked.md".into(), b"- blocked\n".to_vec())],
    );
    let root = fixture.enrollment_root("blocked");
    let binding = fixture.enrollment_binding();
    let evidence = fixture.compose(&root);

    crate::oplog::enrollment::block_current_for_test(
        &root,
        &binding,
        evidence.enrollment_head(),
        "activation.test".into(),
    )
    .unwrap();
    let blocked_head = enrollment_head(&root, &binding);

    assert!(activate_verified_local(
        &root,
        evidence,
        SessionId::new(),
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .is_err());
    assert_eq!(enrollment_head(&root, &binding), blocked_head);
    fixture.assert_graph_unchanged();
}

/// A full-scan reconciliation dispatch owns the first durable baseline
/// mutation of a step (`begin_epoch` plus the scan row appends). It must
/// therefore authorize the exact live graph and engine *before* that mutation,
/// not only later inside the coordinator.
///
/// The refused authority here is a genuine promoted admission — a real
/// `LocalActiveAuthority` plus the real `PromotedLocalRuntime` minted for a
/// second, separately enrolled graph — so this is a live wrong-authority
/// dispatch rather than a synthetic admission value.
#[test]
fn full_scan_dispatch_with_wrong_authority_never_mutates_the_baseline() {
    let fixture = Fixture::new(
        "full-scan-authority",
        None,
        vec![("pages/scan.md".into(), b"- scan\n".to_vec())],
    );
    // The foreign enrollment is deliberately empty so its own live runtime
    // engine is admissible: the refusal under test must come from the graph and
    // engine identity, not from a stale accepted frontier.
    let mut foreign = Fixture::new("full-scan-foreign", None, Vec::new());

    // A genuine live promoted admission that belongs to the *other* graph.
    let foreign_root = foreign.enrollment_root("foreign-admission");
    let foreign_paths = PromotedPaths::new(&foreign, "foreign-admission");
    let (mut foreign_authority, mut foreign_runtime) = promote(
        &mut foreign,
        &foreign_root,
        SessionId::new(),
        &foreign_paths,
    );
    let foreign_session = foreign_runtime
        .admit_promoted_mutation(&mut foreign_authority, &foreign.graph)
        .unwrap();
    let wrong_admission = foreign_session.admission();

    // One coherent live runtime for the graph actually being reconciled.
    let mut engine = fixture.runtime_engine("scan");
    let archive = ObjectStore::open(
        &fixture.root.path().join("runtime-archive-scan"),
        fixture.workspace,
    )
    .unwrap();
    let mut database = fixture.runtime_projection(&engine, &archive, "scan");
    let source = RebuildSource::new(&engine, &archive).unwrap();
    let mut tail = TailOverlay::from_durable(&database, &source).unwrap();
    let mut baseline = fixture.reconciliation_baseline("scan");
    let baseline_directory = baseline.path().parent().unwrap().to_path_buf();

    // A fresh baseline has no clean head yet, so the head observation is the
    // exact `Option`, not an unwrapped value.
    let head_before = baseline.head().ok();
    let epochs_before = baseline.epoch_rows_for_test();
    let bytes_before = durable_sqlite_digests(&baseline_directory);
    let projection_before = ContentDigest::of(&fs::read(database.path()).unwrap());
    let frontier_before = database.frontier_root().unwrap();
    let accepted_before = engine.accepted_frontier_root().unwrap();

    let mut session = ReconciliationSession::new(ReconciliationSchedulerLimits::default());
    session.trigger(ReconciliationTrigger::Explicit);
    assert_eq!(
        session.step(ReconciliationSessionDependencies {
            admission: &wrong_admission,
            graph: &fixture.graph,
            receipts: &fixture.receipts,
            engine: &mut engine,
            database: &mut database,
            tail: &mut tail,
            baseline: &mut baseline,
            observed_at: BaselineTimestamp::from_millis(1).unwrap(),
        }),
        Ok(ReconciliationSessionStep::Blocked),
        "a full scan that is not admitted must fail closed"
    );

    // Nothing scan-owned may have been written: not the baseline bytes, not a
    // building epoch, not a row, not the head, and not the projection.
    assert_eq!(
        durable_sqlite_digests(&baseline_directory),
        bytes_before,
        "a refused full scan must leave the baseline database byte-identical"
    );
    assert_eq!(baseline.epoch_rows_for_test(), epochs_before);
    assert_eq!(baseline.head().ok(), head_before);
    assert_eq!(
        ContentDigest::of(&fs::read(database.path()).unwrap()),
        projection_before
    );
    assert_eq!(database.frontier_root().unwrap(), frontier_before);
    assert_eq!(engine.accepted_frontier_root().unwrap(), accepted_before);
    fixture.assert_graph_unchanged();
    foreign.assert_graph_unchanged();

    // Control: the identical dispatch under an admitted runtime does reach the
    // baseline, so the assertions above are not vacuous.
    let admitted = LocalRuntimeAdmission::unenrolled_pre_activation();
    let mut control = ReconciliationSession::new(ReconciliationSchedulerLimits::default());
    control.trigger(ReconciliationTrigger::Explicit);
    assert!(control
        .step(ReconciliationSessionDependencies {
            admission: &admitted,
            graph: &fixture.graph,
            receipts: &fixture.receipts,
            engine: &mut engine,
            database: &mut database,
            tail: &mut tail,
            baseline: &mut baseline,
            observed_at: BaselineTimestamp::from_millis(2).unwrap(),
        })
        .is_ok());
    assert_ne!(
        baseline.epoch_rows_for_test(),
        epochs_before,
        "an admitted full scan must reach the baseline"
    );
    fixture.assert_graph_unchanged();
}

/// Compile-time proof that the authority cannot be cloned, serialized, or
/// deserialized. The inherent associated const wins whenever the bound holds.
struct Probe<T>(PhantomData<T>);

trait NegativeProbe {
    const CLONEABLE: bool = false;
    const SERIALIZABLE: bool = false;
    const DESERIALIZABLE: bool = false;
}

impl<T> NegativeProbe for Probe<T> {}

impl<T: Clone> Probe<T> {
    const CLONEABLE: bool = true;
}

struct SerdeProbe<T>(PhantomData<T>);

trait NegativeSerdeProbe {
    const SERIALIZABLE: bool = false;
}

impl<T> NegativeSerdeProbe for SerdeProbe<T> {}

impl<T: serde::Serialize> SerdeProbe<T> {
    const SERIALIZABLE: bool = true;
}

struct DeserializeProbe<T>(PhantomData<T>);

trait NegativeDeserializeProbe {
    const DESERIALIZABLE: bool = false;
}

impl<T> NegativeDeserializeProbe for DeserializeProbe<T> {}

impl<T: serde::de::DeserializeOwned> DeserializeProbe<T> {
    const DESERIALIZABLE: bool = true;
}

#[test]
fn local_active_authority_cannot_be_cloned_serialized_or_deserialized() {
    assert!(!Probe::<LocalActiveAuthority>::CLONEABLE);
    assert!(!SerdeProbe::<LocalActiveAuthority>::SERIALIZABLE);
    assert!(!DeserializeProbe::<LocalActiveAuthority>::DESERIALIZABLE);
    assert!(!Probe::<SafeHandoffPermit>::CLONEABLE);
    assert!(!SerdeProbe::<SafeHandoffPermit>::SERIALIZABLE);
    // The positive control proves the probe actually discriminates.
    assert!(Probe::<ContentDigest>::CLONEABLE);
    assert!(SerdeProbe::<ContentDigest>::SERIALIZABLE);
}

#[test]
fn runtime_mutation_is_denied_without_wrong_or_stale_authority_and_allowed_with_the_exact_one() {
    let fixture = Fixture::new("runtime-gate", None, Vec::new());
    let root = fixture.enrollment_root("gate");
    let binding = fixture.enrollment_binding();
    let session = SessionId::new();
    let mut authority = activate_verified_local(
        &root,
        fixture.compose(&root),
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();

    let engine = fixture.runtime_engine("gate");

    // Allowed: the exact current authority, live graph, and live engine.
    {
        let permit = authority
            .admit_local_mutation(&fixture.graph, &engine)
            .unwrap();
        assert_eq!(permit.session_id(), session);
        assert_eq!(permit.enrollment_head(), enrollment_head(&root, &binding));
    }

    // Denied: a foreign graph and a foreign engine.
    let other = Fixture::new(
        "runtime-gate-foreign",
        None,
        vec![("pages/foreign.md".into(), b"- foreign\n".to_vec())],
    );
    let foreign_engine = other.runtime_engine("foreign");
    assert!(authority
        .admit_local_mutation(&other.graph, &engine)
        .is_err());
    assert!(authority
        .admit_local_mutation(&fixture.graph, &foreign_engine)
        .is_err());

    // Denied: a runtime engine behind the activated accepted frontier. The
    // enrollment identity matches exactly; only the accepted sequence is stale.
    let advanced = Fixture::new(
        "runtime-gate-advanced",
        None,
        vec![("pages/advanced.md".into(), b"- advanced\n".to_vec())],
    );
    let advanced_root = advanced.enrollment_root("advanced");
    let mut advanced_authority = activate_verified_local(
        &advanced_root,
        advanced.compose(&advanced_root),
        SessionId::new(),
        &advanced.proofs(),
        &advanced.runtime(),
    )
    .unwrap();
    let behind = advanced.runtime_engine("behind");
    assert_eq!(
        behind
            .accepted_frontier_root()
            .unwrap()
            .acceptance_sequence(),
        0
    );
    assert!(advanced.verified.part_count() >= 1);
    assert!(
        advanced_authority
            .admit_local_mutation(&advanced.graph, &behind)
            .is_err(),
        "a runtime engine behind the activated frontier must never be admitted"
    );

    // Denied: an unenrolled engine has no endpoint at all.
    let unenrolled = ShardedHotEngine::new(
        WorkspaceId::from_uuid(Uuid::from_u128(0x9900)),
        LineageDigest::of(b"unenrolled"),
        DocumentId::from_uuid(Uuid::from_u128(0x9901)),
    );
    assert!(authority
        .admit_local_mutation(&fixture.graph, &unenrolled)
        .is_err());

    // Denied: the enrollment itself is no longer this session's LocalActive.
    crate::oplog::enrollment::block_current_for_test(
        &root,
        &binding,
        authority.enrollment_head(),
        "gate.test".into(),
    )
    .unwrap();
    let blocked_head = enrollment_head(&root, &binding);
    assert!(authority
        .admit_local_mutation(&fixture.graph, &engine)
        .is_err());
    assert_eq!(enrollment_head(&root, &binding), blocked_head);

    fixture.assert_graph_unchanged();
    other.assert_graph_unchanged();
    advanced.assert_graph_unchanged();
}

#[test]
fn safe_handoff_proves_every_core_drain_and_names_its_missing_dependency() {
    let fixture = Fixture::new("safe-handoff", None, Vec::new());
    let root = fixture.enrollment_root("safe");
    let binding = fixture.enrollment_binding();
    let session = SessionId::new();
    let mut authority = activate_verified_local(
        &root,
        fixture.compose(&root),
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    let unsafe_head = authority.enrollment_head();

    // One coherent live runtime: engine, device-local SQLite, and tail overlay
    // all share a single engine identity.
    let engine = &fixture.runtime_engine("safe");
    let archive = ObjectStore::open(
        &fixture.root.path().join("runtime-archive-safe"),
        fixture.workspace,
    )
    .unwrap();
    let database = &fixture.runtime_projection(engine, &archive, "safe");
    let source = RebuildSource::new(engine, &archive).unwrap();
    let tail = TailOverlay::from_durable(database, &source).unwrap();

    // The production transition proves every core-checkable drain and then
    // refuses to mint Safe, naming the exact missing dependency.
    let unavailable = authority
        .quiesce_and_mark_safe(&fixture.graph, engine, database, &tail)
        .unwrap_err();
    assert!(
        matches!(
            unavailable,
            SafeHandoffUnavailable::MissingDependency(SAFE_HANDOFF_MISSING_DEPENDENCY)
        ),
        "unexpected safe-handoff outcome: {unavailable}"
    );
    assert_eq!(enrollment_head(&root, &binding), unsafe_head);
    assert_eq!(
        authority.handoff(),
        LocalActiveHandoff::Unsafe {
            session_id: session
        }
    );

    // With that one dependency set aside, the same drain proof persists Safe
    // and a fresh committed-head reopen confirms it.
    let permit = authority
        .quiesce_and_mark_safe_without_watcher_dependency(&fixture.graph, engine, database, &tail)
        .unwrap();
    assert_eq!(permit.session_id(), session);
    assert_eq!(permit.enrollment_head(), enrollment_head(&root, &binding));
    assert_ne!(permit.enrollment_head(), unsafe_head);
    assert_eq!(authority.handoff(), LocalActiveHandoff::Safe);
    let safe_head = authority.enrollment_head();

    // Any mutation admission must durably move Safe back to Unsafe first.
    {
        let admitted = authority
            .admit_local_mutation(&fixture.graph, engine)
            .unwrap();
        assert_eq!(admitted.session_id(), session);
    }
    assert_eq!(
        authority.handoff(),
        LocalActiveHandoff::Unsafe {
            session_id: session
        }
    );
    assert_ne!(authority.enrollment_head(), safe_head);
    assert_eq!(
        authority.enrollment_head(),
        enrollment_head(&root, &binding)
    );

    // An incomplete drain never reaches Safe.
    let mut pressured = tail;
    let _reservation = pressured
        .reserve_mutation(crate::oplog::TAIL_MAX_BYTES)
        .unwrap();
    let blocked = authority
        .quiesce_and_mark_safe_without_watcher_dependency(
            &fixture.graph,
            engine,
            database,
            &pressured,
        )
        .unwrap_err();
    assert!(
        matches!(blocked, SafeHandoffUnavailable::DrainIncomplete { .. }),
        "unexpected drain outcome: {blocked}"
    );
    assert_eq!(
        authority.handoff(),
        LocalActiveHandoff::Unsafe {
            session_id: session
        }
    );
    fixture.assert_graph_unchanged();
}

// ---------------------------------------------------------------------------
// P2N8 runtime promotion.
// ---------------------------------------------------------------------------

/// The device-local paths one promoted runtime is opened over.
struct PromotedPaths {
    runtime_root: ApplicationRuntimeRoot,
    database_path: PathBuf,
}

impl PromotedPaths {
    fn new(fixture: &Fixture, label: &str) -> Self {
        Self {
            runtime_root: ApplicationRuntimeRoot::open_for_test(
                &fixture.root.path().join(format!("promoted-rt-{label}")),
            )
            .unwrap(),
            database_path: fixture.root.path().join(format!("promoted-{label}.sqlite")),
        }
    }

    fn open<'a>(&'a self, fixture: &'a Fixture) -> PromotedRuntimeOpen<'a> {
        PromotedRuntimeOpen {
            graph: &fixture.graph,
            receipts: &fixture.receipts,
            archive_root: &fixture.archive_root,
            database_path: &self.database_path,
            application_runtime_root: &self.runtime_root,
        }
    }
}

/// Prove the P2N7 fence is still real for this archive: an ordinary enrolled
/// open of an inactive bootstrap history must fail closed.
fn assert_ordinary_enrolled_open_is_fenced(fixture: &Fixture) {
    let storage = ProjectionStorageBinding {
        endpoint: fixture.authority.binding().storage_binding().endpoint,
        receipt_store_id: fixture.receipts.store_id(),
    };
    let error = fixture
        .archive()
        .seal_enrolled_projection(storage)
        .err()
        .expect("an inactive bootstrap archive must refuse an ordinary enrolled open")
        .1;
    assert!(
        matches!(
            error,
            crate::oplog::StoreError::InactiveBootstrapHistory
                | crate::oplog::StoreError::PromotedRuntimeStateMismatch(_)
        ),
        "unexpected ordinary-open error: {error}"
    );
}

/// Activate P2N7 and then complete the P2N8 promotion.
fn promote(
    fixture: &mut Fixture,
    root: &EnrollmentApplicationRoot,
    session: SessionId,
    paths: &PromotedPaths,
) -> (LocalActiveAuthority, PromotedLocalRuntime) {
    let authority = activate_verified_local(
        root,
        fixture.compose(root),
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    let sealed =
        seal_local_runtime_promotion(&authority, &fixture.proofs(), &fixture.runtime()).unwrap();
    fixture.release_bootstrap_projection();
    let runtime = open_promoted_local_runtime(sealed, &authority, &paths.open(fixture)).unwrap();
    (authority, runtime)
}

/// Author, publish, and accept one ordinary local batch through the promoted
/// runtime's admitted mutation window.
fn append_local_batch(
    fixture: &Fixture,
    authority: &mut LocalActiveAuthority,
    runtime: &mut PromotedLocalRuntime,
    seed: u128,
) {
    append_local_batch_at(fixture, authority, runtime, seed, "pages")
}

/// `page_directory` must be the configured pages directory of `fixture`'s
/// graph, so the authored projection path is a valid managed path there.
fn append_local_batch_at(
    fixture: &Fixture,
    authority: &mut LocalActiveAuthority,
    runtime: &mut PromotedLocalRuntime,
    seed: u128,
    page_directory: &str,
) {
    let endpoint = authority.endpoint();
    let mut session = runtime
        .admit_promoted_mutation(authority, &fixture.graph)
        .unwrap();
    let transaction = OperationTransaction::new(vec![
        SemanticOperation::CreatePage {
            page_id: PageId::from_uuid(Uuid::from_u128(seed)),
            home_document_id: DocumentId::from_uuid(Uuid::from_u128(seed + 1)),
            name: LogicalPageName::parse(&format!("Promoted {seed}")).unwrap(),
            path: ManagedPath::parse(&format!("{page_directory}/promoted-{seed}.md")).unwrap(),
            kind: ManagedTextKind::Page,
        },
        SemanticOperation::CreateBlock {
            block: BlockLocation {
                block_id: BlockId::from_uuid(Uuid::from_u128(seed + 2)),
                home_document_id: DocumentId::from_uuid(Uuid::from_u128(seed + 1)),
            },
            page_id: PageId::from_uuid(Uuid::from_u128(seed)),
            parent: None,
            order: "a".into(),
            content: format!("promoted local batch {seed}"),
        },
    ])
    .unwrap();

    let (admission, engine, _database, _tail) = session.parts();
    // Every mutation path authorizes first; a promoted admission proves the
    // whole binding, not merely a non-regressing acceptance sequence.
    admission.authorize(&fixture.graph, engine).unwrap();

    let draft = engine
        .draft_author_transaction(
            AuthorBatch {
                batch_id: BatchId::from_uuid(Uuid::from_u128(seed + 3)),
                author_device_id: endpoint.device_id(),
                author_session_id: SessionId::from_uuid(Uuid::from_u128(seed + 4)),
                crdt_peer_id: CrdtPeerId::from_u64((seed as u64) | 1),
            },
            BatchOrigin::LocalMutation,
            &transaction,
        )
        .unwrap();
    let prepared = engine
        .finalize_author_transaction(draft, &fixture.graph, &fixture.receipts, endpoint)
        .unwrap();
    ObjectStore::open(&fixture.archive_root, fixture.workspace)
        .unwrap()
        .publish_prepared(&prepared)
        .unwrap();
    let outcome = engine
        .stage_archive_batch(prepared.manifest().batch_id())
        .unwrap();
    assert!(
        matches!(outcome.disposition, BatchDisposition::Accepted { .. }),
        "promoted local batch was not accepted: {:?}",
        outcome.disposition
    );

    // A promoted admission requires the device-local SQLite projection to be at
    // the current accepted frontier, so every mutation drains before the next
    // window opens. This is the ordinary bounded tail drain.
    let drained = session.drain_projection(16).unwrap();
    assert_eq!(drained, 1, "exactly the new accepted batch drains");
}

/// The durable promotion state file inside one archive root.
fn promotion_state_path_in(archive_root: &Path, fixture: &Fixture) -> PathBuf {
    archive_root
        .join("engine-history")
        .join(
            fixture
                .authority
                .binding()
                .storage_binding()
                .endpoint
                .endpoint_id()
                .to_string(),
        )
        .join("promoted-runtime.state")
}

/// The durable promotion state file for one fixture archive.
fn promotion_state_path(fixture: &Fixture) -> PathBuf {
    promotion_state_path_in(&fixture.archive_root, fixture)
}

/// Recursively copy a directory tree, producing fresh directory inodes.
///
/// The copy is byte-identical and structurally identical, but every directory
/// in it is a distinct filesystem resource from its source. That distinction is
/// exactly what a retargeting attack cannot forge and what archive identity is
/// derived from.
fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap().map(Result::unwrap) {
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if fs::symlink_metadata(&from).unwrap().is_dir() {
            copy_tree(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

/// The whole first-promotion boundary over a rich, nested, Unicode, CRLF,
/// multipart bootstrap: fenced before, writable after, byte-identical graph,
/// and an exactly resumable durable state.
#[test]
fn inactive_bootstrap_promotes_to_a_writable_runtime_and_resumes_exactly() {
    let mut fixture = rich_fixture("promote-rich");
    let root = fixture.enrollment_root("promote-rich");
    let binding = fixture.enrollment_binding();
    let paths = PromotedPaths::new(&fixture, "rich");
    let session = SessionId::new();

    // Fail-before: this archive cannot be opened as an ordinary runtime.
    assert_ordinary_enrolled_open_is_fenced(&fixture);
    assert!(!promotion_state_path(&fixture).exists());

    let authority = activate_verified_local(
        &root,
        fixture.compose(&root),
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    let activated_head = enrollment_head(&root, &binding);
    let bootstrap_frontier = fixture
        .authority
        .accepted_engine()
        .accepted_frontier_root()
        .unwrap();
    let bootstrap_generation = fixture.authority.binding().history_generation();
    let bootstrap_root = fixture.authority.binding().history_root();
    assert!(
        fixture.verified.part_count() >= 1,
        "rich fixture is nonempty"
    );

    // Phase one is idempotent: repeating it with the same inputs resumes
    // against the identical committed bytes.
    let _first =
        seal_local_runtime_promotion(&authority, &fixture.proofs(), &fixture.runtime()).unwrap();
    let state_bytes = fs::read(promotion_state_path(&fixture)).unwrap();
    let sealed =
        seal_local_runtime_promotion(&authority, &fixture.proofs(), &fixture.runtime()).unwrap();
    assert_eq!(
        fs::read(promotion_state_path(&fixture)).unwrap(),
        state_bytes,
        "an idempotent resume must not rewrite the committed promotion state"
    );

    fixture.release_bootstrap_projection();
    let runtime = open_promoted_local_runtime(sealed, &authority, &paths.open(&fixture)).unwrap();

    // The promoted runtime is the bootstrap's own lineage at its own frontier.
    assert_eq!(
        runtime.bootstrap_anchor().generation,
        bootstrap_generation,
        "the promoted anchor must be the exact bootstrap history generation"
    );
    assert_eq!(runtime.bootstrap_anchor().index_root, bootstrap_root);
    // Every authenticated field of the frontier must be reproduced exactly.
    // `scratch_root` locates the run-local scratch LSM page holding this
    // frontier's point index; its file offset is reconstructible derived state
    // that legitimately differs between the inactive and promoted runs.
    assert!(
        runtime
            .engine()
            .accepted_frontier_root()
            .unwrap()
            .same_accepted_authority(&bootstrap_frontier),
        "promotion must reproduce the exact accepted bootstrap frontier"
    );
    assert_eq!(
        runtime
            .engine()
            .accepted_frontier_root()
            .unwrap()
            .reference_catalog_root(),
        bootstrap_frontier.reference_catalog_root(),
        "the promoted catalog root is the exact bootstrap catalog root"
    );
    assert!(runtime
        .database()
        .frontier_root()
        .unwrap()
        .same_accepted_authority(&bootstrap_frontier));
    assert_eq!(runtime.session_id(), session);
    assert_eq!(
        runtime.verification_digest(),
        authority.verification_digest()
    );

    // Promotion advanced no enrollment state and wrote no graph byte.
    assert_eq!(enrollment_head(&root, &binding), activated_head);
    fixture.assert_graph_unchanged();
}

/// A zero-file graph promotes exactly like a populated one: an empty bootstrap
/// anchor is a legitimate anchor, not a missing one.
#[test]
fn a_zero_part_bootstrap_promotes_at_the_empty_anchor() {
    let mut fixture = Fixture::new("promote-zero", None, Vec::new());
    let root = fixture.enrollment_root("promote-zero");
    let paths = PromotedPaths::new(&fixture, "zero");
    assert_eq!(fixture.verified.part_count(), 0);

    let (_authority, runtime) = promote(&mut fixture, &root, SessionId::new(), &paths);
    assert_eq!(runtime.bootstrap_anchor().generation, 0);
    assert_eq!(
        runtime
            .engine()
            .accepted_frontier_root()
            .unwrap()
            .acceptance_sequence(),
        0
    );
    fixture.assert_graph_unchanged();
}

/// Every durable cut of the one-time promotion publication reopens as either
/// the unchanged inactive bootstrap or the one exact resumable promoted state.
/// Partial, truncated, and foreign residue fails closed and is preserved.
#[test]
fn promotion_state_residue_fails_closed_and_preserves_evidence() {
    let mut fixture = Fixture::new(
        "promote-residue",
        None,
        vec![("pages/residue.md".into(), b"- residue\n".to_vec())],
    );
    let root = fixture.enrollment_root("promote-residue");
    let paths = PromotedPaths::new(&fixture, "residue");
    let session = SessionId::new();
    let authority = activate_verified_local(
        &root,
        fixture.compose(&root),
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();

    // The pre-publication cut: no state file at all reopens as the unchanged
    // inactive bootstrap, and a promoted open refuses.
    let state_path = promotion_state_path(&fixture);
    assert!(!state_path.exists());
    let binding = fixture.enrollment_binding();
    assert!(
        reopen_promoted_local_runtime(&root, &binding, session, &paths.open(&fixture)).is_err(),
        "an unpromoted archive must never open a promoted runtime"
    );

    let sealed =
        seal_local_runtime_promotion(&authority, &fixture.proofs(), &fixture.runtime()).unwrap();
    let committed = fs::read(&state_path).unwrap();

    // Truncated residue at every prefix fails closed rather than being
    // repaired or partially believed.
    for cut in [0, 1, committed.len() / 2, committed.len() - 1] {
        fs::write(&state_path, &committed[..cut]).unwrap();
        assert!(
            reopen_promoted_local_runtime(&root, &binding, session, &paths.open(&fixture)).is_err(),
            "a truncated promotion state must fail closed at cut {cut}"
        );
        assert!(state_path.exists(), "evidence must be preserved");
    }

    // A byte-flipped, non-canonical, or foreign claim also fails closed.
    let mut corrupt = committed.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xff;
    fs::write(&state_path, &corrupt).unwrap();
    assert!(
        reopen_promoted_local_runtime(&root, &binding, session, &paths.open(&fixture)).is_err()
    );

    // Restoring the exact committed bytes resumes the one promoted state.
    fs::write(&state_path, &committed).unwrap();
    fixture.release_bootstrap_projection();
    let runtime = open_promoted_local_runtime(sealed, &authority, &paths.open(&fixture)).unwrap();
    assert_eq!(runtime.session_id(), session);
    fixture.assert_graph_unchanged();
}

/// A restarted process holds no evidence, no authority, no sealed promotion,
/// and no engine identity. It reconstructs everything from durable state.
#[test]
fn a_fresh_process_reopens_the_promoted_runtime_with_no_retained_evidence() {
    let mut fixture = Fixture::new(
        "promote-restart",
        None,
        vec![("pages/restart.md".into(), b"- restart\n".to_vec())],
    );
    let root = fixture.enrollment_root("promote-restart");
    let binding = fixture.enrollment_binding();
    let paths = PromotedPaths::new(&fixture, "restart");
    let session = SessionId::new();

    let (authority, runtime) = promote(&mut fixture, &root, session, &paths);
    let anchor = runtime.bootstrap_anchor();
    let frontier = runtime.engine().accepted_frontier_root().unwrap();
    // The previous process is gone: every process-local value dies with it.
    drop(runtime);
    drop(authority);

    // A competing session is refused before any archive or lease work.
    assert!(
        reopen_promoted_local_runtime(&root, &binding, SessionId::new(), &paths.open(&fixture))
            .is_err(),
        "a crash resumes Unsafe for exactly the committed session"
    );

    let (reopened_authority, reopened) =
        reopen_promoted_local_runtime(&root, &binding, session, &paths.open(&fixture)).unwrap();
    assert_eq!(reopened.bootstrap_anchor(), anchor);
    assert_eq!(
        reopened.engine().accepted_frontier_root().unwrap(),
        frontier
    );
    assert_eq!(reopened.database().frontier_root().unwrap(), frontier);
    assert_eq!(
        reopened_authority.handoff(),
        LocalActiveHandoff::Unsafe {
            session_id: session
        },
        "a crash remains Unsafe"
    );
    assert_eq!(reopened_authority.session_id(), session);
    fixture.assert_graph_unchanged();
}

/// Ordinary local batches extend the bootstrap without ever making it
/// unverifiable. After a restart the exact bootstrap ancestor is still proved,
/// the current frontier is reopened, and one more mutation is admitted.
#[test]
fn local_batches_extend_the_bootstrap_anchor_and_restart_proves_exact_ancestry() {
    let mut fixture = Fixture::new(
        "promote-append",
        None,
        vec![("pages/seed.md".into(), b"- seed\n".to_vec())],
    );
    let root = fixture.enrollment_root("promote-append");
    let binding = fixture.enrollment_binding();
    let paths = PromotedPaths::new(&fixture, "append");
    let session = SessionId::new();

    let (mut authority, mut runtime) = promote(&mut fixture, &root, session, &paths);
    let anchor = runtime.bootstrap_anchor();
    let anchor_frontier = runtime.engine().accepted_frontier_root().unwrap();

    append_local_batch(&fixture, &mut authority, &mut runtime, 0x9200);
    let after_one = runtime.engine().durable_history_authority().unwrap();
    assert!(
        after_one.generation > anchor.generation,
        "an ordinary local batch must extend the durable history"
    );
    append_local_batch(&fixture, &mut authority, &mut runtime, 0x9300);
    append_local_batch(&fixture, &mut authority, &mut runtime, 0x9400);
    let advanced = runtime.engine().durable_history_authority().unwrap();
    let advanced_frontier = runtime.engine().accepted_frontier_root().unwrap();
    assert_eq!(advanced.generation, anchor.generation + 3);
    assert!(advanced_frontier.acceptance_sequence() > anchor_frontier.acceptance_sequence());

    // The advanced history is still an authenticated descendant of the exact
    // bootstrap anchor, proved from the shared radix structure.
    let transition = runtime
        .engine()
        .authenticate_history_descends_from(anchor)
        .unwrap();
    assert_eq!(transition.before(), anchor);
    assert_eq!(transition.after(), advanced);

    drop(runtime);
    drop(authority);

    // Restart: the anchor is reconstructed from durable state alone, the live
    // history is proved to descend from it, and the current frontier reopens.
    let (mut authority, mut runtime) =
        reopen_promoted_local_runtime(&root, &binding, session, &paths.open(&fixture)).unwrap();
    assert_eq!(
        runtime.bootstrap_anchor(),
        anchor,
        "restart must reconstruct the exact original bootstrap anchor"
    );
    assert_eq!(
        runtime.engine().durable_history_authority().unwrap(),
        advanced
    );
    assert_eq!(
        runtime.engine().accepted_frontier_root().unwrap(),
        advanced_frontier
    );
    assert_eq!(
        runtime.database().frontier_root().unwrap(),
        advanced_frontier
    );

    // One more mutation is admitted after the restart.
    append_local_batch(&fixture, &mut authority, &mut runtime, 0x9500);
    assert_eq!(
        runtime
            .engine()
            .durable_history_authority()
            .unwrap()
            .generation,
        anchor.generation + 4
    );
    assert_eq!(
        enrollment_head(&root, &binding),
        authority.enrollment_head()
    );
    fixture.assert_graph_unchanged();
}

/// The original `VerifiedLocal` bootstrap proof stays reopenable after the
/// promoted history advances, and the retained immutable publication is
/// unchanged.
#[test]
fn the_original_bootstrap_anchor_stays_reopenable_after_the_history_advances() {
    let mut fixture = Fixture::new(
        "promote-anchor",
        None,
        vec![("pages/anchor.md".into(), b"- anchor\n".to_vec())],
    );
    let root = fixture.enrollment_root("promote-anchor");
    let binding = fixture.enrollment_binding();
    let paths = PromotedPaths::new(&fixture, "anchor");
    let session = SessionId::new();

    let before = crate::oplog::enrollment::reopen_promoted_bootstrap_anchor(&root, &binding);
    assert!(
        before.is_err(),
        "no LocalActive record exists before activation"
    );

    let (mut authority, mut runtime) = promote(&mut fixture, &root, session, &paths);
    let anchor = crate::oplog::enrollment::reopen_promoted_bootstrap_anchor(&root, &binding)
        .expect("the committed anchor must be reconstructible immediately after promotion");
    let anchor_generation = anchor.history_generation();
    let anchor_root = anchor.history_root();
    let anchor_digest = anchor.verification_digest();

    for seed in [0x9600_u128, 0x9700, 0x9800] {
        append_local_batch(&fixture, &mut authority, &mut runtime, seed);
    }
    assert!(
        runtime
            .engine()
            .durable_history_authority()
            .unwrap()
            .generation
            > anchor_generation
    );

    let after = crate::oplog::enrollment::reopen_promoted_bootstrap_anchor(&root, &binding)
        .expect("the bootstrap anchor must stay reopenable after the history advances");
    assert_eq!(after.history_generation(), anchor_generation);
    assert_eq!(after.history_root(), anchor_root);
    assert_eq!(after.verification_digest(), anchor_digest);
    assert_eq!(
        after.bootstrap_part_count(),
        fixture.verified.part_count(),
        "the retained immutable publication identity is unchanged"
    );
    fixture.assert_graph_unchanged();
}

/// A promoted admission requires *both* the live authority and the exact
/// promoted runtime. Substituted graphs, engines, and enrollments are refused
/// before any durable or graph mutation.
#[test]
fn a_promoted_admission_rejects_substituted_runtime_components() {
    let mut fixture = Fixture::new(
        "promote-substitute",
        None,
        vec![("pages/subject.md".into(), b"- subject\n".to_vec())],
    );
    let root = fixture.enrollment_root("promote-substitute");
    let paths = PromotedPaths::new(&fixture, "substitute");
    let (mut authority, mut runtime) = promote(&mut fixture, &root, SessionId::new(), &paths);

    let mut foreign = Fixture::new(
        "promote-substitute-foreign",
        None,
        vec![("pages/foreign.md".into(), b"- foreign\n".to_vec())],
    );
    let foreign_root = foreign.enrollment_root("substitute-foreign");
    let foreign_paths = PromotedPaths::new(&foreign, "substitute-foreign");
    let (mut foreign_authority, mut foreign_runtime) = promote(
        &mut foreign,
        &foreign_root,
        SessionId::new(),
        &foreign_paths,
    );

    let advanced_before = runtime.engine().durable_history_authority().unwrap();
    let foreign_before = foreign_runtime
        .engine()
        .durable_history_authority()
        .unwrap();

    // A foreign graph is refused.
    assert!(runtime
        .admit_promoted_mutation(&mut authority, &foreign.graph)
        .is_err());
    // A foreign authority is refused for this runtime.
    assert!(runtime
        .admit_promoted_mutation(&mut foreign_authority, &fixture.graph)
        .is_err());
    // A foreign runtime is refused for this authority.
    assert!(foreign_runtime
        .admit_promoted_mutation(&mut authority, &foreign.graph)
        .is_err());

    // A genuine admission refuses a substituted engine, including one built
    // over the very same enrolled identity.
    {
        let session = runtime
            .admit_promoted_mutation(&mut authority, &fixture.graph)
            .unwrap();
        let admission = session.admission();
        let substitute = fixture.runtime_engine("substitute");
        assert!(
            admission.authorize(&fixture.graph, &substitute).is_err(),
            "a same-identity engine from another history must be refused"
        );
        assert!(admission
            .authorize(&foreign.graph, foreign_runtime.engine())
            .is_err());
    }

    assert_eq!(
        runtime.engine().durable_history_authority().unwrap(),
        advanced_before,
        "a refused admission must advance no durable history"
    );
    assert_eq!(
        foreign_runtime
            .engine()
            .durable_history_authority()
            .unwrap(),
        foreign_before
    );
    fixture.assert_graph_unchanged();
    foreign.assert_graph_unchanged();
}

/// The bootstrap-anchor ancestry proof is bounded. An unchanged history costs
/// zero radix node reads, and a point extension costs the changed paths, never
/// the lifetime record count.
#[test]
fn the_bootstrap_ancestry_proof_is_bounded_by_the_changed_radix_paths() {
    let mut fixture = rich_fixture("promote-bounded");
    let root = fixture.enrollment_root("promote-bounded");
    let paths = PromotedPaths::new(&fixture, "bounded");
    let (mut authority, mut runtime) = promote(&mut fixture, &root, SessionId::new(), &paths);
    let anchor = runtime.bootstrap_anchor();
    let archive = runtime
        .engine()
        .archive_store()
        .expect("the promoted engine retains its archive")
        .instrumentation();

    // Exact, unchanged history: the shared subtree terminates immediately.
    runtime
        .engine()
        .authenticate_history_descends_from(anchor)
        .unwrap();
    let unchanged = runtime.engine().archive_store().unwrap().instrumentation();
    assert_eq!(
        unchanged.history_index_reads, archive.history_index_reads,
        "an exact-history proof must read no radix nodes at all"
    );

    // The rich fixture configures `notes` as its pages directory.
    append_local_batch_at(&fixture, &mut authority, &mut runtime, 0x9900, "notes");
    let before_point = runtime
        .engine()
        .archive_store()
        .unwrap()
        .instrumentation()
        .history_index_reads;
    runtime
        .engine()
        .authenticate_history_descends_from(anchor)
        .unwrap();
    let point = runtime
        .engine()
        .archive_store()
        .unwrap()
        .instrumentation()
        .history_index_reads
        - before_point;
    // One inserted record touches one root-to-leaf path on each side.
    assert!(
        point
            <= 2 * (u64::from(crate::oplog::object_store::ENGINE_HISTORY_RADIX_DEPTH) + 1) as usize,
        "a point extension proof read {point} radix nodes"
    );
    fixture.assert_graph_unchanged();
}

/// The construction regression for the promoted reference catalog.
///
/// A non-empty inactive bootstrap binds a `reference_catalog_root` into every
/// accepted cold record. That root has exactly one construction — the target
/// archive's durable authenticated Patricia store — so every bound root must be
/// fully openable from a *fresh* archive open that holds no process-local
/// engine, candidate, or in-memory catalog, both while the bootstrap is still
/// inactive and after the runtime is promoted and its history has advanced.
///
/// Fail-before: authoring the bootstrap against the run-local ephemeral catalog
/// backend produced flat in-memory digests instead of Patricia roots, so this
/// validation failed with a missing authenticated node and promotion could
/// never open a non-empty bootstrap.
#[test]
fn a_non_empty_bootstrap_catalog_root_opens_from_a_fresh_archive_before_and_after_promotion() {
    // A genuinely multipart bootstrap, so more than one accepted cold record
    // binds a catalog root, over content that produces real reference sources.
    let mut multipart = String::from("title:: Catalog root\n\n");
    for ordinal in 0..4096 {
        // Deliberately syntax-free: the operation count alone forces a second
        // part, while reference evidence stays on the two pages below so the
        // catalog walk here is not an accidental 4096-target benchmark.
        multipart.push_str(&format!("- operation {ordinal:04}\n"));
    }
    let mut fixture = Fixture::new(
        "promote-catalog-root",
        None,
        vec![
            ("pages/multipart.md".into(), multipart.into_bytes()),
            (
                "pages/référence.md".into(),
                "- see [[Catalog root]] and #tag\r\n".as_bytes().to_vec(),
            ),
        ],
    );
    let root = fixture.enrollment_root("promote-catalog-root");
    let paths = PromotedPaths::new(&fixture, "catalog-root");
    assert!(
        fixture.verified.part_count() >= 2,
        "the fixture must bind more than one cold record: {}",
        fixture.verified.part_count()
    );

    // Every root bound by an accepted cold record, validated completely against
    // a freshly opened durable catalog.
    fn assert_every_bound_catalog_root_opens(fixture: &Fixture, label: &str) {
        let catalog = fixture.archive().open_reference_catalog().unwrap();
        let materials = fixture.prepared.engine_materials();
        assert_eq!(materials.len(), fixture.verified.part_count() as usize);
        let mut covered = 0;
        for material in materials {
            let bound = material.reference_catalog_root();
            catalog
                .validate_catalog_root(bound)
                .unwrap_or_else(|error| {
                    panic!("{label}: a bound bootstrap catalog root is not durable: {error}")
                });
            covered = covered.max(bound.source_count());
        }
        assert!(
            covered > 0,
            "{label}: the bootstrap covers reference sources"
        );
    }

    // Before promotion: the inactive bootstrap's own bound roots.
    assert_every_bound_catalog_root_opens(&fixture, "inactive bootstrap");

    let session = SessionId::new();
    let (mut authority, mut runtime) = promote(&mut fixture, &root, session, &paths);

    // After promotion, and after the history advances past the bootstrap.
    for seed in [0xA100_u128, 0xA200] {
        append_local_batch(&fixture, &mut authority, &mut runtime, seed);
    }
    assert!(
        runtime
            .engine()
            .durable_history_authority()
            .unwrap()
            .generation
            > u64::from(fixture.verified.part_count())
    );
    let advanced = runtime.engine().reference_catalog_root().unwrap().clone();
    drop(runtime);
    drop(authority);

    assert_every_bound_catalog_root_opens(&fixture, "promoted and advanced");
    // The live advanced catalog root is durable too, from a fresh archive open.
    fixture
        .archive()
        .open_reference_catalog()
        .unwrap()
        .validate_catalog_root(&advanced)
        .expect("the advanced promoted catalog root opens from a fresh archive");
    fixture.assert_graph_unchanged();
}

/// The promoted runtime token, its sealed promotion, and its mutation window
/// are opaque: no clone, no serde, and no way to reconstruct one from bytes.
#[test]
fn promoted_runtime_values_cannot_be_cloned_serialized_or_deserialized() {
    assert!(!Probe::<PromotedLocalRuntime>::CLONEABLE);
    assert!(!SerdeProbe::<PromotedLocalRuntime>::SERIALIZABLE);
    assert!(!DeserializeProbe::<PromotedLocalRuntime>::DESERIALIZABLE);
    assert!(!Probe::<SealedRuntimePromotion>::CLONEABLE);
    assert!(!SerdeProbe::<SealedRuntimePromotion>::SERIALIZABLE);
    assert!(!DeserializeProbe::<SealedRuntimePromotion>::DESERIALIZABLE);
    assert!(!Probe::<LocalRuntimeAdmission<'static>>::CLONEABLE);
    assert!(!SerdeProbe::<LocalRuntimeAdmission<'static>>::SERIALIZABLE);
    // The positive control proves the probe actually discriminates.
    assert!(Probe::<ContentDigest>::CLONEABLE);
    assert!(SerdeProbe::<ContentDigest>::SERIALIZABLE);
}

/// The pre-activation escape hatch can never authorize a promoted runtime.
///
/// It is `pub(crate)`, so no app or Tauri code can name it at all. This proves
/// the second, structural fence: even from inside the crate it refuses a
/// promoted engine, so a real activated user graph is only ever writable
/// through the authority-plus-runtime admission.
#[test]
fn the_pre_activation_admission_refuses_a_promoted_runtime_engine() {
    let mut fixture = Fixture::new(
        "promote-hatch",
        None,
        vec![("pages/hatch.md".into(), b"- hatch\n".to_vec())],
    );
    let root = fixture.enrollment_root("promote-hatch");
    let paths = PromotedPaths::new(&fixture, "hatch");
    let (mut authority, mut runtime) = promote(&mut fixture, &root, SessionId::new(), &paths);

    let hatch = LocalRuntimeAdmission::unenrolled_pre_activation();
    assert!(
        hatch.authorize(&fixture.graph, runtime.engine()).is_err(),
        "the pre-activation hatch must never authorize a promoted runtime"
    );

    // An unpromoted fixture engine still passes, so the refusal is specific.
    let unpromoted = fixture.runtime_engine("hatch-unpromoted");
    assert!(hatch.authorize(&fixture.graph, &unpromoted).is_ok());

    // The genuine promoted admission still works, and the refused hatch
    // advanced no durable history.
    let before = runtime.engine().durable_history_authority().unwrap();
    append_local_batch(&fixture, &mut authority, &mut runtime, 0xA300);
    assert_eq!(
        runtime
            .engine()
            .durable_history_authority()
            .unwrap()
            .generation,
        before.generation + 1
    );
    fixture.assert_graph_unchanged();
}

/// Promotion publication is bound to the exact retained archive capability,
/// never to whatever currently answers to the archive's pathname.
///
/// The positive control proves the ordinary same-capability seal and its
/// readback still commit one exact immutable state and resume idempotently. The
/// negative case is the retargeting cut: an archive renamed while its retained
/// capability stays open, with a byte-identical recursive copy left at the old
/// pathname. That copy is a perfect forgery of everything content-addressed —
/// identical durable history, identical bootstrap publication, identical
/// canonical archive-resource claim bytes — and differs only in physical
/// directory identity. Publication must not durably land in either directory.
#[test]
fn promotion_publication_binds_the_exact_retained_archive_capability() {
    // --- positive control: ordinary same-capability seal and readback --------
    let control = Fixture::new(
        "promote-capability-control",
        None,
        vec![("pages/control.md".into(), b"- control\n".to_vec())],
    );
    let control_root = control.enrollment_root("promote-capability-control");
    let control_binding = control.enrollment_binding();
    let control_session = SessionId::new();
    let control_authority = activate_verified_local(
        &control_root,
        control.compose(&control_root),
        control_session,
        &control.proofs(),
        &control.runtime(),
    )
    .unwrap();
    let control_state = promotion_state_path(&control);
    assert!(!control_state.exists());

    let control_head = enrollment_head(&control_root, &control_binding);
    seal_local_runtime_promotion(&control_authority, &control.proofs(), &control.runtime())
        .unwrap();
    let committed = fs::read(&control_state).unwrap();
    // The readback inside the seal is a genuine fresh durable-history open over
    // the same retained capability, and repeating phase one resumes against the
    // identical committed bytes rather than rewriting them.
    seal_local_runtime_promotion(&control_authority, &control.proofs(), &control.runtime())
        .unwrap();
    assert_eq!(fs::read(&control_state).unwrap(), committed);
    assert_eq!(
        enrollment_head(&control_root, &control_binding),
        control_head
    );
    control.assert_graph_unchanged();
    drop(control_authority);

    // --- the retargeting cut -------------------------------------------------
    let mut fixture = Fixture::new(
        "promote-retarget",
        None,
        vec![("pages/retarget.md".into(), b"- retarget\n".to_vec())],
    );
    let root = fixture.enrollment_root("promote-retarget");
    let binding = fixture.enrollment_binding();
    let session = SessionId::new();
    let authority = activate_verified_local(
        &root,
        fixture.compose(&root),
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    let activated_head = enrollment_head(&root, &binding);

    // Archive A is renamed while every retained capability in the proof set
    // stays open on it; a byte-identical copy B then takes its old pathname.
    let retained = fixture.archive_root.clone();
    let renamed = fixture.root.path().join("archive-renamed");
    fs::rename(&retained, &renamed).unwrap();
    copy_tree(&renamed, &retained);
    assert_eq!(
        snapshot_file_digests(&renamed),
        snapshot_file_digests(&retained),
        "the stale copy must be byte-identical, so only directory identity differs"
    );
    fixture.archive_root = renamed.clone();

    let retained_before = snapshot_file_digests(&renamed);
    let stale_before = snapshot_file_digests(&retained);
    let error = seal_local_runtime_promotion(&authority, &fixture.proofs(), &fixture.runtime())
        .err()
        .expect("an ambiguous archive must block promotion before publication");
    assert!(
        matches!(
            error,
            RuntimePromotionError::Activation(LocalActivationError::RuntimeBinding(_))
        ),
        "unexpected retargeting error: {error}"
    );

    // Neither archive may gain a promotion-state file, and neither archive's
    // durable history may have moved at all.
    assert!(
        !promotion_state_path_in(&renamed, &fixture).exists(),
        "the retained archive must not have been published into"
    );
    assert!(
        !promotion_state_path_in(&retained, &fixture).exists(),
        "the stale look-alike archive must not have been published into"
    );
    assert_eq!(snapshot_file_digests(&renamed), retained_before);
    assert_eq!(snapshot_file_digests(&retained), stale_before);
    assert_eq!(enrollment_head(&root, &binding), activated_head);
    fixture.assert_graph_unchanged();
}

/// The promoted-state authorization boundary itself refuses a foreign archive.
///
/// A restarted process holds no retained capability, so it must open the
/// configured archive pathname. Centralizing the exact archive binding inside
/// the durable-history control means the refusal happens at the state read —
/// before any promoted engine, projection-work index, SQLite lease, or replay
/// exists — rather than only later, at the promoted-runtime mint. A later
/// caller that reaches promoted state some other way inherits the same refusal.
#[test]
fn a_byte_identical_copy_of_a_promoted_archive_is_refused_at_the_state_boundary() {
    let mut fixture = Fixture::new(
        "promote-copied-archive",
        None,
        vec![("pages/copied.md".into(), b"- copied\n".to_vec())],
    );
    let root = fixture.enrollment_root("promote-copied-archive");
    let binding = fixture.enrollment_binding();
    let paths = PromotedPaths::new(&fixture, "copied");
    let session = SessionId::new();
    let (authority, runtime) = promote(&mut fixture, &root, session, &paths);
    drop(runtime);
    drop(authority);

    // A byte-identical recursive copy of the whole promoted archive, including
    // its committed promotion state. Only directory identity differs.
    let copy = fixture.root.path().join("archive-copy");
    copy_tree(&fixture.archive_root, &copy);
    assert_eq!(
        snapshot_file_digests(&fixture.archive_root),
        snapshot_file_digests(&copy)
    );
    assert!(promotion_state_path_in(&copy, &fixture).exists());

    let copied_paths = PromotedPaths::new(&fixture, "copied-target");
    fixture.archive_root = copy.clone();
    let error =
        reopen_promoted_local_runtime(&root, &binding, session, &copied_paths.open(&fixture))
            .err()
            .expect("a foreign archive must never adopt another archive's promotion state");
    assert!(
        matches!(
            error,
            RuntimePromotionError::Store(crate::oplog::StoreError::PromotedRuntimeStateMismatch(_))
        ),
        "the refusal must come from the promoted-state boundary, not a later mint: {error}"
    );
    // The refusal happened before any promoted runtime existed, so the copy
    // gained no device-local projection at all.
    assert!(!copied_paths.database_path.exists());
    fixture.assert_graph_unchanged();
}

/// Promoted recovery replays its immutable bootstrap parts one at a time.
///
/// Restart resident memory must be one bootstrap part, not the whole graph, so
/// the observed maximum bootstrap-part residency has to be exactly one over a
/// genuinely multi-part publication — on the same-process promoted open and
/// again on a fresh-process reopen that holds no retained evidence at all.
///
/// Fail-before: recovery built a `Vec<PreparedBatch>` containing every part and
/// all of its objects before staging any of them, so the observed maximum was
/// the whole part count.
#[test]
fn promoted_recovery_streams_exactly_one_bootstrap_part_at_a_time() {
    // Two operations per part over a six-operation graph partitions into
    // exactly three parts, deterministically and without a four-thousand-block
    // fixture. Only the partition boundary is forced: every part is authored,
    // published, installed, and replayed through the ordinary path.
    force_next_bootstrap_part_operation_limit(2);
    let mut fixture = Fixture::new(
        "promote-stream-parts",
        None,
        vec![
            (
                "pages/anchor.md".into(),
                b"title:: Streamed anchor\n\n- one\n- two\n- three\n".to_vec(),
            ),
            // Real reference evidence, so the promoted open performs the
            // authenticated recovery replay rather than skipping it.
            (
                "pages/referrer.md".into(),
                "- see [[Streamed anchor]] and #tag\n".as_bytes().to_vec(),
            ),
        ],
    );
    let part_count = fixture.verified.part_count() as usize;
    assert!(
        part_count >= 3,
        "the streaming regression needs a genuinely multi-part bootstrap: {part_count}"
    );

    let root = fixture.enrollment_root("promote-stream-parts");
    let binding = fixture.enrollment_binding();
    let paths = PromotedPaths::new(&fixture, "stream-parts");
    let session = SessionId::new();
    let (authority, runtime) = promote(&mut fixture, &root, session, &paths);

    let same_process = runtime.engine().bootstrap_recovery_instrumentation();
    assert_eq!(
        same_process.bootstrap_part_reads, part_count,
        "recovery must read every bootstrap part exactly once"
    );
    assert!(same_process.bootstrap_object_reads > 0);
    assert_eq!(
        same_process.max_live_bootstrap_parts, 1,
        "at most one bootstrap part payload may be resident at a time"
    );

    // A restarted process reconstructs everything from durable state; the same
    // residency bound must hold on that path too.
    drop(runtime);
    drop(authority);
    let (_reopened_authority, reopened) =
        reopen_promoted_local_runtime(&root, &binding, session, &paths.open(&fixture)).unwrap();
    let fresh_process = reopened.engine().bootstrap_recovery_instrumentation();
    assert_eq!(fresh_process, same_process);
    assert_eq!(fresh_process.max_live_bootstrap_parts, 1);
    fixture.assert_graph_unchanged();
}

/// The scratch-backed detached block-claim index removes the old fixed cap
/// instead of moving it.
///
/// Detached bootstrap authoring used to register block claims in the bounded
/// no-store in-memory map, which refuses the claim past
/// `MAX_EPHEMERAL_BLOCK_CLAIMS`. This is the smallest graph that crosses that
/// exact boundary, carried through the whole real path: preparation,
/// installation, promotion, and a fresh-process reopen.
///
/// Fail-before: preparing this exact fixture failed with "no-store block-claim
/// test index reached its fixed capacity". The bounded map itself still keeps
/// its cap — `no_store_block_claim_capacity_rejects_before_candidate_mutation`
/// covers that — so what changed is that authoring no longer uses it at all.
#[test]
fn a_bootstrap_one_block_past_the_old_claim_cap_promotes_and_reopens() {
    let blocks = MAX_EPHEMERAL_BLOCK_CLAIMS + 1;
    let mut source = String::new();
    for ordinal in 0..blocks {
        source.push_str(&format!("- claim {ordinal:05}\n"));
    }
    let mut fixture = Fixture::new(
        "promote-claim-cap",
        None,
        vec![("pages/claims.md".into(), source.into_bytes())],
    );
    let root = fixture.enrollment_root("promote-claim-cap");
    let binding = fixture.enrollment_binding();
    let paths = PromotedPaths::new(&fixture, "claim-cap");
    let session = SessionId::new();

    let (authority, runtime) = promote(&mut fixture, &root, session, &paths);
    let frontier = runtime.engine().accepted_frontier_root().unwrap();
    // The cap is removed, not raised: the promoted engine holds its claims in
    // the scratch-backed point index, so the bounded map stays empty.
    assert_eq!(
        runtime.engine().instrumentation().block_claim_hot_entries,
        0,
        "a store-backed engine must hold no ephemeral block claims"
    );
    assert!(runtime
        .database()
        .frontier_root()
        .unwrap()
        .same_accepted_authority(&frontier));
    drop(runtime);
    drop(authority);

    let (_reopened_authority, reopened) =
        reopen_promoted_local_runtime(&root, &binding, session, &paths.open(&fixture)).unwrap();
    assert_eq!(
        reopened.engine().instrumentation().block_claim_hot_entries,
        0
    );
    assert_eq!(
        reopened.engine().accepted_frontier_root().unwrap(),
        frontier
    );
    fixture.assert_graph_unchanged();
}
