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
use crate::oplog::hot_engine::{ProjectionEndpointBinding, ProjectionStorageBinding};
use crate::oplog::import::{
    prepare_inactive_bootstrap_import, publish_install_verify_inactive_bootstrap,
    reopen_inactive_bootstrap_accepted_authority, InactiveBootstrapAcceptedAuthority,
    InactiveBootstrapPreparedPublication, InactiveBootstrapVerifiedPublication,
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
    CanonicalArchiveResourceId, DeviceId, DocumentId, LineageDigest, ObjectStore, ProjectionClaim,
    ProjectionEndpointId, ProjectionReceiptStore, ReferenceCatalogPolicyV1, ShardedHotEngine,
    WorkspaceId,
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
    sqlite: OpenProjection,
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
        let prepared = prepare_inactive_bootstrap_import(
            &graph,
            capture,
            workspace,
            lineage,
            catalog_document_id,
            ReferenceCatalogPolicyV1::default(),
            &preparation_root,
        )
        .unwrap();
        let storage_binding = ProjectionStorageBinding {
            endpoint,
            receipt_store_id: receipts.store_id(),
        };
        let archive_root = root.path().join("archive");
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
            sqlite,
            sqlite_proof,
            archive_resource_id,
            shadow,
            preparation: PreparationId::new(),
            original_graph,
        }
    }

    fn proofs(&self) -> VerifiedLocalProofSet<'_> {
        VerifiedLocalProofSet {
            graph: &self.graph,
            roots: &self.roots,
            prepared: &self.prepared,
            verified_publication: &self.verified,
            source_backup: &self.backup,
            accepted_authority: &self.authority,
            sqlite: &self.sqlite,
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
            projection: &self.sqlite,
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
        projection: &other.sqlite,
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
        projection: &second.sqlite,
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
/// The refused authority here is a genuine `LocalActiveAuthority` permit minted
/// for a second, separately enrolled graph, so this is a real wrong-authority
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
    let foreign = Fixture::new("full-scan-foreign", None, Vec::new());

    // A genuine live admission that belongs to the *other* enrolled graph.
    let foreign_root = foreign.enrollment_root("foreign-admission");
    let mut foreign_authority = activate_verified_local(
        &foreign_root,
        foreign.compose(&foreign_root),
        SessionId::new(),
        &foreign.proofs(),
        &foreign.runtime(),
    )
    .unwrap();
    let foreign_engine = foreign.runtime_engine("foreign-admission");
    let foreign_permit = foreign_authority
        .admit_local_mutation(&foreign.graph, &foreign_engine)
        .unwrap();
    let wrong_admission = foreign_permit.admission();

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
        assert!(permit
            .admission()
            .authorize(&fixture.graph, &engine)
            .is_ok());
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
