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

#[test]
fn activation_resumes_idempotently_only_for_the_exact_session_and_evidence() {
    let fixture = Fixture::new(
        "idempotent-resume",
        None,
        vec![("pages/resume.md".into(), b"- resume\n".to_vec())],
    );
    let root = fixture.enrollment_root("resume");
    let binding = fixture.enrollment_binding();
    // Two evidence values minted from the same committed VerifiedLocal head:
    // one for the activation, one retained across the simulated restart.
    let evidence = fixture.compose(&root);
    let retained = fixture.compose(&root);
    assert_eq!(retained.enrollment_head(), evidence.enrollment_head());
    let session = SessionId::new();

    let first = activate_verified_local(
        &root,
        evidence,
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    let activated_head = first.enrollment_head();
    let activated_generation = enrollment_generation(&root, &binding);
    drop(first);

    // A fresh process-style reopen with the exact same session and evidence
    // resumes the committed activation record without advancing anything.
    let resumed = activate_verified_local(
        &root,
        retained,
        session,
        &fixture.proofs(),
        &fixture.runtime(),
    )
    .unwrap();
    assert_eq!(resumed.enrollment_head(), activated_head);
    assert_eq!(resumed.session_id(), session);
    assert_eq!(
        resumed.handoff(),
        LocalActiveHandoff::Unsafe {
            session_id: session
        }
    );
    assert_eq!(enrollment_head(&root, &binding), activated_head);
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
