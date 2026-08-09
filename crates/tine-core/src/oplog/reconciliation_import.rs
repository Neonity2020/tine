//! Inactive bridge from one stable graph reconciliation scan to the existing
//! point-revalidated operational coordinator.
//!
//! Scan fingerprints are discovery evidence only. This module never turns
//! their bytes, digests, kinds, or owner bindings into semantic operations or
//! write authority.

use std::collections::BTreeMap;

use crate::model::Graph;

use super::{
    hot_engine::MAX_TRANSACTION_OPERATIONS,
    import::MAX_IMPORT_PATH_BYTES,
    local_active::LocalRuntimeAdmission,
    operational_coordinator::{
        FailedClosedOperationalCoordinator, OperationalCompletion, OperationalCoordinator,
        OperationalCoordinatorError, OperationalCoordinatorState,
    },
    reconciliation_scan::{
        ExpectedPathSourceFailure, GraphTextCandidateBinding, GraphTextScanCandidate,
        GraphTextScanDiagnosticKind, JoinedAuthenticatedExpectedPathSource, StableGraphTextScan,
    },
    shadow_projection::BootstrapProjectionAuthority,
    ImportPlan, ManagedPath, ManagedTextKind, ProjectionReceiptStore, ShardedHotEngine,
    SqliteFrontier, TailOverlay,
};

const MAX_BLOCK_EVIDENCE_ROWS: usize = 32;
const MAX_BLOCK_EVIDENCE_PATH_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReconciliationImportRetryReason {
    GraphBindingMoved,
    ExpectedBindingMoved,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReconciliationImportRetry {
    pub(crate) reason: ReconciliationImportRetryReason,
    pub(crate) detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReconciliationImportBlockReason {
    CandidateCountLimit,
    CandidatePathBytesLimit,
    CandidateSetAmbiguous,
    UnsupportedDiscovery,
    ExpectedAuthorityUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReconciliationImportEvidenceKind {
    CandidateBindingMismatch,
    CandidateOrderOrExactCollision,
    PortablePathCollision,
    UnsupportedExtension,
    /// The path is not ordinary graph text this graph can load: it names a
    /// container OG itself skips, a hidden path, or a spelling the guarded
    /// writer cannot address. Merely living outside the configured
    /// `pages/`/`journals/` roots is not by itself a reason to block.
    UndecodableGraphTextPath,
    ExpectedKindChanged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReconciliationImportEvidence {
    pub(crate) kind: ReconciliationImportEvidenceKind,
    pub(crate) path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReconciliationImportDiscoveryBlock {
    pub(crate) reason: ReconciliationImportBlockReason,
    pub(crate) detail: String,
    pub(crate) evidence: Vec<ReconciliationImportEvidence>,
    pub(crate) omitted_evidence: usize,
}

pub(crate) enum ReconciliationImportBlocked {
    Discovery(ReconciliationImportDiscoveryBlock),
    Coordinator(ImportPlan),
    CoordinatorError(OperationalCoordinatorError),
}

pub(crate) enum ReconciliationImportOutcome {
    Noop,
    Complete(OperationalCompletion),
    Blocked(ReconciliationImportBlocked),
    RetryFull(ReconciliationImportRetry),
    FailedClosed(FailedClosedOperationalCoordinator),
}

#[derive(Clone, Copy)]
struct PreparationLimits {
    candidates: usize,
    path_bytes: u64,
}

impl Default for PreparationLimits {
    fn default() -> Self {
        Self {
            // A scan epoch is never prefix-published. This stricter count keeps
            // one discovery path from already exceeding the transaction's
            // operation ceiling before the importer performs its exact diff.
            candidates: MAX_TRANSACTION_OPERATIONS,
            path_bytes: MAX_IMPORT_PATH_BYTES,
        }
    }
}

#[derive(Debug)]
enum PreparationFailure {
    Blocked(ReconciliationImportDiscoveryBlock),
    RetryFull(ReconciliationImportRetry),
}

trait ReconciliationImportAuthority {
    fn revalidate(&self, binding: &GraphTextCandidateBinding) -> Result<(), PreparationFailure>;

    fn managed_kind(&self, path: &ManagedPath) -> Result<ManagedTextKind, ()>;
}

struct LiveReconciliationImportAuthority<'a, 'b> {
    graph: &'a Graph,
    source: JoinedAuthenticatedExpectedPathSource<'b>,
}

impl ReconciliationImportAuthority for LiveReconciliationImportAuthority<'_, '_> {
    fn revalidate(&self, binding: &GraphTextCandidateBinding) -> Result<(), PreparationFailure> {
        let graph_resource = self.graph.canonical_resource_id().map_err(|error| {
            PreparationFailure::Blocked(discovery_block(
                ReconciliationImportBlockReason::ExpectedAuthorityUnavailable,
                format!("retained graph binding is unavailable: {error}"),
            ))
        })?;
        let scope_binding = self.graph.graph_text_scope_binding().map_err(|error| {
            PreparationFailure::Blocked(discovery_block(
                ReconciliationImportBlockReason::ExpectedAuthorityUnavailable,
                format!("graph-text scope binding is unavailable: {error}"),
            ))
        })?;
        if graph_resource != binding.graph_resource || scope_binding != binding.scope_binding {
            return Err(PreparationFailure::RetryFull(ReconciliationImportRetry {
                reason: ReconciliationImportRetryReason::GraphBindingMoved,
                detail: "stable scan graph resource or graph-text scope binding moved".to_owned(),
            }));
        }

        let maximum_retained_bytes =
            super::reconciliation_scan::GraphTextScanLimits::default().retained_bytes;
        let (expected_binding, source_commitment) = self
            .source
            .current_scan_identity(maximum_retained_bytes)
            .map_err(|failure| match failure {
                ExpectedPathSourceFailure::Unavailable => {
                    PreparationFailure::RetryFull(ReconciliationImportRetry {
                        reason: ReconciliationImportRetryReason::ExpectedBindingMoved,
                        detail: failure.to_string(),
                    })
                }
                _ => PreparationFailure::Blocked(discovery_block(
                    ReconciliationImportBlockReason::ExpectedAuthorityUnavailable,
                    failure.to_string(),
                )),
            })?;
        if expected_binding != binding.expected_binding
            || source_commitment != binding.expected_source_commitment
        {
            return Err(PreparationFailure::RetryFull(ReconciliationImportRetry {
                reason: ReconciliationImportRetryReason::ExpectedBindingMoved,
                detail:
                    "accepted frontier, projection generation, or joined authority binding moved"
                        .to_owned(),
            }));
        }
        Ok(())
    }

    fn managed_kind(&self, path: &ManagedPath) -> Result<ManagedTextKind, ()> {
        self.graph
            .managed_text_kind_for_managed_path(path)
            .map_err(|_| ())
    }
}

fn discovery_block(
    reason: ReconciliationImportBlockReason,
    detail: impl Into<String>,
) -> ReconciliationImportDiscoveryBlock {
    ReconciliationImportDiscoveryBlock {
        reason,
        detail: detail.into(),
        evidence: Vec::new(),
        omitted_evidence: 0,
    }
}

#[derive(Default)]
struct EvidenceBuilder {
    evidence: Vec<ReconciliationImportEvidence>,
    retained_path_bytes: usize,
    total: usize,
}

impl EvidenceBuilder {
    fn push(&mut self, kind: ReconciliationImportEvidenceKind, path: &str) {
        self.total = self.total.saturating_add(1);
        if self.evidence.len() >= MAX_BLOCK_EVIDENCE_ROWS
            || path.len() > MAX_BLOCK_EVIDENCE_PATH_BYTES.saturating_sub(self.retained_path_bytes)
        {
            return;
        }
        self.retained_path_bytes = self.retained_path_bytes.saturating_add(path.len());
        self.evidence.push(ReconciliationImportEvidence {
            kind,
            path: path.to_owned(),
        });
    }

    fn finish(
        self,
        reason: ReconciliationImportBlockReason,
        detail: impl Into<String>,
    ) -> ReconciliationImportDiscoveryBlock {
        ReconciliationImportDiscoveryBlock {
            reason,
            detail: detail.into(),
            omitted_evidence: self.total.saturating_sub(self.evidence.len()),
            evidence: self.evidence,
        }
    }

    fn is_empty(&self) -> bool {
        self.total == 0
    }
}

fn unsupported_extension(path: &ManagedPath) -> Option<ReconciliationImportEvidenceKind> {
    if path.is_markdown() || path.is_org() {
        None
    } else {
        Some(ReconciliationImportEvidenceKind::UnsupportedExtension)
    }
}

fn blocking_diagnostic_evidence(
    kind: GraphTextScanDiagnosticKind,
) -> Option<ReconciliationImportEvidenceKind> {
    match kind {
        // Conflict copies and logical collision losers remain committed
        // diagnostics, but are deliberately excluded from page candidates and
        // carry no import authority.
        GraphTextScanDiagnosticKind::ProviderConflictCopy
        | GraphTextScanDiagnosticKind::SemanticCollisionLoser
        | GraphTextScanDiagnosticKind::PortableCollisionLoser => None,
    }
}

fn validate_candidate<A: ReconciliationImportAuthority>(
    authority: &A,
    candidate: &GraphTextScanCandidate,
    evidence: &mut EvidenceBuilder,
) {
    if let Some(kind) = unsupported_extension(&candidate.path) {
        evidence.push(kind, candidate.path.as_str());
        return;
    }
    let Ok(current_kind) = authority.managed_kind(&candidate.path) else {
        evidence.push(
            ReconciliationImportEvidenceKind::UndecodableGraphTextPath,
            candidate.path.as_str(),
        );
        return;
    };
    if candidate
        .managed_kind
        .is_some_and(|expected_kind| expected_kind != current_kind)
    {
        evidence.push(
            ReconciliationImportEvidenceKind::ExpectedKindChanged,
            candidate.path.as_str(),
        );
    }
}

fn prepare_stable_scan<A: ReconciliationImportAuthority>(
    scan: &StableGraphTextScan,
    authority: &A,
    limits: PreparationLimits,
) -> Result<Vec<ManagedPath>, PreparationFailure> {
    authority.revalidate(&scan.binding)?;

    if scan.candidates.len() > limits.candidates {
        return Err(PreparationFailure::Blocked(discovery_block(
            ReconciliationImportBlockReason::CandidateCountLimit,
            format!(
                "complete stable candidate count {} exceeds the one-batch limit {}",
                scan.candidates.len(),
                limits.candidates
            ),
        )));
    }

    let mut path_bytes = 0_u64;
    for candidate in &scan.candidates {
        path_bytes = path_bytes
            .checked_add(candidate.path.as_str().len() as u64)
            .ok_or_else(|| {
                PreparationFailure::Blocked(discovery_block(
                    ReconciliationImportBlockReason::CandidatePathBytesLimit,
                    "complete stable candidate path bytes overflowed",
                ))
            })?;
        if path_bytes > limits.path_bytes {
            return Err(PreparationFailure::Blocked(discovery_block(
                ReconciliationImportBlockReason::CandidatePathBytesLimit,
                format!(
                    "complete stable candidate path bytes {path_bytes} exceed the one-batch limit {}",
                    limits.path_bytes
                ),
            )));
        }
    }

    let mut evidence = EvidenceBuilder::default();
    let mut previous: Option<&ManagedPath> = None;
    let mut portable_paths = BTreeMap::new();
    for candidate in &scan.candidates {
        if candidate.binding != scan.binding {
            evidence.push(
                ReconciliationImportEvidenceKind::CandidateBindingMismatch,
                candidate.path.as_str(),
            );
        }
        if previous.is_some_and(|previous| previous >= &candidate.path) {
            evidence.push(
                ReconciliationImportEvidenceKind::CandidateOrderOrExactCollision,
                candidate.path.as_str(),
            );
        }
        previous = Some(&candidate.path);
        if let Some(first) =
            portable_paths.insert(candidate.path.portable_key(), candidate.path.as_str())
        {
            evidence.push(
                ReconciliationImportEvidenceKind::PortablePathCollision,
                first,
            );
            evidence.push(
                ReconciliationImportEvidenceKind::PortablePathCollision,
                candidate.path.as_str(),
            );
        }
        validate_candidate(authority, candidate, &mut evidence);
    }
    for diagnostic in &scan.diagnostics {
        if let Some(kind) = blocking_diagnostic_evidence(diagnostic.kind) {
            evidence.push(kind, &diagnostic.path);
        }
    }
    if !evidence.is_empty() {
        return Err(PreparationFailure::Blocked(evidence.finish(
            ReconciliationImportBlockReason::UnsupportedDiscovery,
            "stable scan contains unsupported, conflicting, or ambiguous graph-wide evidence",
        )));
    }

    let paths = scan
        .candidates
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect();

    authority.revalidate(&scan.binding)?;
    Ok(paths)
}

enum CoordinatorDisposition<B, F> {
    Blocked(B),
    Noop,
    Complete(OperationalCompletion),
    FailedClosed(F),
}

trait ReconciliationCoordinatorCall {
    type Blocked;
    type FailedClosed;

    fn execute(
        &mut self,
        paths: &[&str],
    ) -> Result<
        CoordinatorDisposition<Self::Blocked, Self::FailedClosed>,
        OperationalCoordinatorError,
    >;
}

#[derive(Debug)]
enum CoordinatorHandoff<B, F> {
    Blocked(B),
    CoordinatorError(OperationalCoordinatorError),
    Noop,
    Complete(OperationalCompletion),
    FailedClosed(F),
}

fn hand_off_once<C: ReconciliationCoordinatorCall>(
    paths: &[ManagedPath],
    coordinator: &mut C,
) -> CoordinatorHandoff<C::Blocked, C::FailedClosed> {
    let requested_paths = paths.iter().map(ManagedPath::as_str).collect::<Vec<_>>();
    match coordinator.execute(&requested_paths) {
        Ok(CoordinatorDisposition::Blocked(blocked)) => CoordinatorHandoff::Blocked(blocked),
        Ok(CoordinatorDisposition::Noop) => CoordinatorHandoff::Noop,
        Ok(CoordinatorDisposition::Complete(completion)) => {
            CoordinatorHandoff::Complete(completion)
        }
        Ok(CoordinatorDisposition::FailedClosed(failed)) => {
            CoordinatorHandoff::FailedClosed(failed)
        }
        Err(error) => CoordinatorHandoff::CoordinatorError(error),
    }
}

struct LiveCoordinatorCall<'a> {
    admission: &'a LocalRuntimeAdmission<'a>,
    graph: &'a Graph,
    receipts: &'a ProjectionReceiptStore,
    engine: &'a mut ShardedHotEngine,
    database: &'a mut SqliteFrontier,
    tail: &'a mut TailOverlay,
    bootstrap: Option<&'a BootstrapProjectionAuthority>,
}

impl ReconciliationCoordinatorCall for LiveCoordinatorCall<'_> {
    type Blocked = ImportPlan;
    type FailedClosed = FailedClosedOperationalCoordinator;

    fn execute(
        &mut self,
        paths: &[&str],
    ) -> Result<
        CoordinatorDisposition<Self::Blocked, Self::FailedClosed>,
        OperationalCoordinatorError,
    > {
        OperationalCoordinator::execute_with_bootstrap(
            self.admission,
            self.graph,
            self.receipts,
            self.engine,
            self.database,
            self.tail,
            self.bootstrap,
            paths,
        )
        .map(|state| match state {
            OperationalCoordinatorState::Blocked(plan) => CoordinatorDisposition::Blocked(plan),
            OperationalCoordinatorState::Noop => CoordinatorDisposition::Noop,
            OperationalCoordinatorState::Complete(completion) => {
                CoordinatorDisposition::Complete(completion)
            }
            OperationalCoordinatorState::FailedClosed(failed) => {
                CoordinatorDisposition::FailedClosed(failed)
            }
        })
    }
}

/// Consume one complete stable scan and hand its supported exact path set to
/// the existing coordinator exactly once.
///
/// This function has no production caller in this packet. It does not schedule
/// scans, mark a disposable baseline, infer enrollment, or retry a published
/// failed-closed continuation.
pub(crate) fn execute_stable_scan_import(
    scan: StableGraphTextScan,
    admission: &LocalRuntimeAdmission<'_>,
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    engine: &mut ShardedHotEngine,
    database: &mut SqliteFrontier,
    tail: &mut TailOverlay,
    bootstrap: Option<&BootstrapProjectionAuthority>,
) -> ReconciliationImportOutcome {
    let prepared = {
        let projection = match engine.projection_work_index() {
            Ok(projection) => projection,
            Err(error) => {
                return ReconciliationImportOutcome::Blocked(
                    ReconciliationImportBlocked::Discovery(discovery_block(
                        ReconciliationImportBlockReason::ExpectedAuthorityUnavailable,
                        error.to_string(),
                    )),
                );
            }
        };
        let source = bootstrap.map_or_else(
            || JoinedAuthenticatedExpectedPathSource::new(engine, projection),
            |bootstrap| {
                JoinedAuthenticatedExpectedPathSource::with_bootstrap(
                    engine, projection, bootstrap, database,
                )
            },
        );
        let authority = LiveReconciliationImportAuthority { graph, source };
        prepare_stable_scan(&scan, &authority, PreparationLimits::default())
    };
    let paths = match prepared {
        Ok(paths) => paths,
        Err(PreparationFailure::Blocked(blocked)) => {
            return ReconciliationImportOutcome::Blocked(ReconciliationImportBlocked::Discovery(
                blocked,
            ));
        }
        Err(PreparationFailure::RetryFull(retry)) => {
            return ReconciliationImportOutcome::RetryFull(retry);
        }
    };

    let mut coordinator = LiveCoordinatorCall {
        admission,
        graph,
        receipts,
        engine,
        database,
        tail,
        bootstrap,
    };
    match hand_off_once(&paths, &mut coordinator) {
        CoordinatorHandoff::Blocked(plan) => {
            ReconciliationImportOutcome::Blocked(ReconciliationImportBlocked::Coordinator(plan))
        }
        CoordinatorHandoff::CoordinatorError(error) => ReconciliationImportOutcome::Blocked(
            ReconciliationImportBlocked::CoordinatorError(error),
        ),
        CoordinatorHandoff::Noop => ReconciliationImportOutcome::Noop,
        CoordinatorHandoff::Complete(completion) => {
            ReconciliationImportOutcome::Complete(completion)
        }
        CoordinatorHandoff::FailedClosed(failed) => {
            ReconciliationImportOutcome::FailedClosed(failed)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        time::Duration,
    };
    use uuid::Uuid;

    use super::*;
    use crate::graph_text_scope::GraphTextScope;
    use crate::oplog::{
        reconciliation_scan::{
            ExpectedPathBinding, GraphTextCandidateKind, GraphTextScanDiagnostic,
            GraphTextScanInstrumentation,
        },
        BlobDescription, CanonicalGraphResourceId, ContentDigest,
    };

    struct TempGraph {
        root: PathBuf,
    }

    impl TempGraph {
        fn new(config: Option<&str>) -> Self {
            let root =
                std::env::temp_dir().join(format!("tine-reconciliation-import-{}", Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            if let Some(config) = config {
                fs::create_dir_all(root.join("logseq")).unwrap();
                fs::write(root.join("logseq/config.edn"), config).unwrap();
            }
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for TempGraph {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct FixtureAuthority {
        binding: GraphTextCandidateBinding,
        kinds: BTreeMap<ManagedPath, ManagedTextKind>,
        revalidation: Result<(), ReconciliationImportRetryReason>,
    }

    impl ReconciliationImportAuthority for FixtureAuthority {
        fn revalidate(
            &self,
            binding: &GraphTextCandidateBinding,
        ) -> Result<(), PreparationFailure> {
            if let Err(reason) = self.revalidation {
                return Err(PreparationFailure::RetryFull(ReconciliationImportRetry {
                    reason,
                    detail: "fixture binding moved".to_owned(),
                }));
            }
            if binding != &self.binding {
                return Err(PreparationFailure::RetryFull(ReconciliationImportRetry {
                    reason: ReconciliationImportRetryReason::ExpectedBindingMoved,
                    detail: "fixture binding differs".to_owned(),
                }));
            }
            Ok(())
        }

        fn managed_kind(&self, path: &ManagedPath) -> Result<ManagedTextKind, ()> {
            self.kinds.get(path).copied().ok_or(())
        }
    }

    struct SequencedFixtureAuthority {
        binding: GraphTextCandidateBinding,
        kinds: BTreeMap<ManagedPath, ManagedTextKind>,
        moves_before_handoff: bool,
        revalidation_calls: Cell<usize>,
    }

    impl ReconciliationImportAuthority for SequencedFixtureAuthority {
        fn revalidate(
            &self,
            binding: &GraphTextCandidateBinding,
        ) -> Result<(), PreparationFailure> {
            let call = self.revalidation_calls.get();
            self.revalidation_calls.set(call.saturating_add(1));
            if binding != &self.binding || (self.moves_before_handoff && call != 0) {
                return Err(PreparationFailure::RetryFull(ReconciliationImportRetry {
                    reason: ReconciliationImportRetryReason::ExpectedBindingMoved,
                    detail: "fixture binding moved before handoff".to_owned(),
                }));
            }
            Ok(())
        }

        fn managed_kind(&self, path: &ManagedPath) -> Result<ManagedTextKind, ()> {
            self.kinds.get(path).copied().ok_or(())
        }
    }

    struct GraphFixtureAuthority<'a> {
        graph: &'a Graph,
        binding: GraphTextCandidateBinding,
    }

    impl ReconciliationImportAuthority for GraphFixtureAuthority<'_> {
        fn revalidate(
            &self,
            binding: &GraphTextCandidateBinding,
        ) -> Result<(), PreparationFailure> {
            if binding == &self.binding {
                Ok(())
            } else {
                Err(PreparationFailure::RetryFull(ReconciliationImportRetry {
                    reason: ReconciliationImportRetryReason::GraphBindingMoved,
                    detail: "fixture binding differs".to_owned(),
                }))
            }
        }

        fn managed_kind(&self, path: &ManagedPath) -> Result<ManagedTextKind, ()> {
            self.graph
                .managed_text_kind_for_managed_path(path)
                .map_err(|_| ())
        }
    }

    #[derive(Clone, Copy)]
    enum FakeDisposition {
        Noop,
        FailedClosed,
    }

    struct CountingCoordinator {
        calls: usize,
        received: Vec<Vec<String>>,
        disposition: FakeDisposition,
    }

    impl CountingCoordinator {
        fn new(disposition: FakeDisposition) -> Self {
            Self {
                calls: 0,
                received: Vec::new(),
                disposition,
            }
        }
    }

    impl ReconciliationCoordinatorCall for CountingCoordinator {
        type Blocked = ();
        type FailedClosed = &'static str;

        fn execute(
            &mut self,
            paths: &[&str],
        ) -> Result<
            CoordinatorDisposition<Self::Blocked, Self::FailedClosed>,
            OperationalCoordinatorError,
        > {
            self.calls += 1;
            self.received
                .push(paths.iter().map(|path| (*path).to_owned()).collect());
            Ok(match self.disposition {
                FakeDisposition::Noop => CoordinatorDisposition::Noop,
                FakeDisposition::FailedClosed => {
                    CoordinatorDisposition::FailedClosed("durable-batch")
                }
            })
        }
    }

    fn binding(graph: Option<&Graph>) -> GraphTextCandidateBinding {
        let fixture_resource =
            CanonicalGraphResourceId::from_capability_identity(b"fixture", b"graph");
        GraphTextCandidateBinding {
            graph_resource: graph
                .map(|graph| graph.canonical_resource_id().unwrap())
                .unwrap_or(fixture_resource),
            scope_binding: graph
                .map(|graph| graph.graph_text_scope_binding().unwrap())
                .unwrap_or_else(|| {
                    GraphTextScope::new(&[], false).bind_graph_resource(fixture_resource)
                }),
            expected_binding: ExpectedPathBinding {
                accepted_frontier: ContentDigest::of(b"frontier"),
                projection_generation: 7,
            },
            expected_source_commitment: ContentDigest::of(b"source"),
            expected_rows_commitment: ContentDigest::of(b"rows"),
            scan_epoch_digest: ContentDigest::of(b"epoch"),
        }
    }

    fn candidate(
        path: &str,
        change: GraphTextCandidateKind,
        managed_kind: Option<ManagedTextKind>,
        binding: &GraphTextCandidateBinding,
    ) -> GraphTextScanCandidate {
        GraphTextScanCandidate {
            path: ManagedPath::parse(path).unwrap(),
            managed_kind,
            change,
            expected_description: Some(BlobDescription::of(b"expected")),
            expected_owner_binding: Some(ContentDigest::of(b"owner")),
            observed_description: Some(BlobDescription::of(b"observed")),
            observed_file_resource_id: Some(ContentDigest::of(b"file")),
            observed_link_count: Some(1),
            binding: binding.clone(),
        }
    }

    fn scan(
        binding: GraphTextCandidateBinding,
        candidates: Vec<GraphTextScanCandidate>,
        diagnostics: Vec<GraphTextScanDiagnostic>,
    ) -> StableGraphTextScan {
        let baseline_pass = super::super::reconciliation_scan::GraphTextScanPass {
            graph_resource: binding.graph_resource,
            scope_binding: binding.scope_binding.clone(),
            directories_by_exact_relative: BTreeMap::new(),
            files: Vec::new(),
            instrumentation:
                super::super::reconciliation_scan::GraphTextScanPassInstrumentation::default(),
        };
        StableGraphTextScan {
            candidates,
            diagnostics,
            binding,
            instrumentation: GraphTextScanInstrumentation::default(),
            wall_time: Duration::ZERO,
            expected_path_count: 0,
            pass_a_digest: super::super::reconciliation_scan::graph_text_scan_pass_digest(
                &baseline_pass,
            ),
            pass_b_digest: super::super::reconciliation_scan::graph_text_scan_pass_digest(
                &baseline_pass,
            ),
            baseline_pass,
        }
    }

    fn execute_fixture(
        scan: &StableGraphTextScan,
        authority: &impl ReconciliationImportAuthority,
        coordinator: &mut CountingCoordinator,
        limits: PreparationLimits,
    ) -> Result<CoordinatorHandoff<(), &'static str>, PreparationFailure> {
        let paths = prepare_stable_scan(scan, authority, limits)?;
        Ok(hand_off_once(&paths, coordinator))
    }

    #[test]
    fn reconciliation_import_supported_edit_delete_create_one_call_handoff() {
        let binding = binding(None);
        let candidates = vec![
            candidate(
                "pages/create.md",
                GraphTextCandidateKind::Creation,
                None,
                &binding,
            ),
            candidate(
                "pages/delete.md",
                GraphTextCandidateKind::Absence,
                Some(ManagedTextKind::Page),
                &binding,
            ),
            candidate(
                "pages/edit.org",
                GraphTextCandidateKind::Edit,
                Some(ManagedTextKind::Page),
                &binding,
            ),
        ];
        let authority = SequencedFixtureAuthority {
            binding: binding.clone(),
            kinds: candidates
                .iter()
                .map(|candidate| (candidate.path.clone(), ManagedTextKind::Page))
                .collect(),
            moves_before_handoff: false,
            revalidation_calls: Cell::new(0),
        };
        let scan = scan(binding, candidates, Vec::new());
        let mut coordinator = CountingCoordinator::new(FakeDisposition::Noop);

        let outcome = execute_fixture(
            &scan,
            &authority,
            &mut coordinator,
            PreparationLimits::default(),
        )
        .unwrap();

        assert!(matches!(outcome, CoordinatorHandoff::Noop));
        assert_eq!(authority.revalidation_calls.get(), 2);
        assert_eq!(coordinator.calls, 1);
        assert_eq!(
            coordinator.received,
            vec![vec![
                "pages/create.md".to_owned(),
                "pages/delete.md".to_owned(),
                "pages/edit.org".to_owned(),
            ]]
        );
    }

    #[test]
    fn reconciliation_import_accepts_nested_nonstandard_longest_root_path() {
        let temp = TempGraph::new(None);
        fs::create_dir_all(temp.path().join("logseq")).unwrap();
        fs::write(
            temp.path().join("logseq/config.edn"),
            "{:pages-directory \"managed/text\"\n\
             :journals-directory \"managed/text/daily\"}\n",
        )
        .unwrap();
        let graph = Graph::open(temp.path());
        let binding = binding(Some(&graph));
        let candidates = vec![
            candidate(
                "managed/text/daily/2026/07/26.org",
                GraphTextCandidateKind::Creation,
                None,
                &binding,
            ),
            candidate(
                "managed/text/projects/client/plan.md",
                GraphTextCandidateKind::Creation,
                None,
                &binding,
            ),
        ];
        let scan = scan(binding.clone(), candidates, Vec::new());
        let authority = GraphFixtureAuthority {
            graph: &graph,
            binding,
        };
        let mut coordinator = CountingCoordinator::new(FakeDisposition::Noop);

        let outcome = execute_fixture(
            &scan,
            &authority,
            &mut coordinator,
            PreparationLimits::default(),
        )
        .unwrap();

        assert!(matches!(outcome, CoordinatorHandoff::Noop));
        assert_eq!(coordinator.calls, 1);
        assert_eq!(
            coordinator.received[0],
            vec![
                "managed/text/daily/2026/07/26.org",
                "managed/text/projects/client/plan.md",
            ]
        );
    }

    #[test]
    fn reconciliation_import_accepts_supported_paths_outside_configured_roots() {
        let temp = TempGraph::new(Some(
            "{:journal/file-name-format \"dd-MM-yyyy\"\n\
              :journal/page-title-format \"yyyy-MM-dd\"}\n",
        ));
        let graph = Graph::open(temp.path());
        let binding = binding(Some(&graph));
        // OG discovers ordinary graph text by walking the whole graph directory,
        // so an existing nested page outside `pages/`/`journals/` must reach the
        // coordinator with its exact spelling instead of blocking the epoch.
        let candidates = vec![
            candidate(
                "Top Level.md",
                GraphTextCandidateKind::Creation,
                None,
                &binding,
            ),
            // OG parses the file-name title as a date to decide journal-ness,
            // so the retained kind for a date-named non-journal-root file is
            // Journal and an unchanged expected row must not look changed.
            candidate(
                "archive/2024/25-07-2026.org",
                GraphTextCandidateKind::Edit,
                Some(ManagedTextKind::Journal),
                &binding,
            ),
            candidate(
                "archive/2024/client notes/Ünicode Page.md",
                GraphTextCandidateKind::Edit,
                Some(ManagedTextKind::Page),
                &binding,
            ),
        ];
        let scan = scan(binding.clone(), candidates, Vec::new());
        let authority = GraphFixtureAuthority {
            graph: &graph,
            binding,
        };
        let mut coordinator = CountingCoordinator::new(FakeDisposition::Noop);

        let outcome = execute_fixture(
            &scan,
            &authority,
            &mut coordinator,
            PreparationLimits::default(),
        )
        .unwrap();

        assert!(matches!(outcome, CoordinatorHandoff::Noop));
        assert_eq!(coordinator.calls, 1);
        assert_eq!(
            coordinator.received[0],
            vec![
                "Top Level.md",
                "archive/2024/25-07-2026.org",
                "archive/2024/client notes/Ünicode Page.md",
            ]
        );
    }

    #[test]
    fn reconciliation_import_accepts_canonical_extension_spelling_and_case_variants() {
        let temp = TempGraph::new(None);
        let graph = Graph::open(temp.path());
        let binding = binding(Some(&graph));
        let paths = [
            "archive/extensions/lower.md",
            "archive/extensions/upper.MD",
            "archive/extensions/mixed.mD",
            "archive/extensions/lower.markdown",
            "archive/extensions/upper.MARKDOWN",
            "archive/extensions/mixed.MarkDown",
            "archive/extensions/lower.org",
            "archive/extensions/upper.ORG",
            "archive/extensions/mixed.Org",
        ];
        let mut candidates = paths
            .iter()
            .map(|path| candidate(path, GraphTextCandidateKind::Creation, None, &binding))
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| left.path.cmp(&right.path));
        let expected = candidates
            .iter()
            .map(|candidate| candidate.path.as_str().to_owned())
            .collect::<Vec<_>>();
        let scan = scan(binding.clone(), candidates, Vec::new());
        let authority = GraphFixtureAuthority {
            graph: &graph,
            binding,
        };
        let mut coordinator = CountingCoordinator::new(FakeDisposition::Noop);

        let outcome = execute_fixture(
            &scan,
            &authority,
            &mut coordinator,
            PreparationLimits::default(),
        )
        .unwrap();

        assert!(matches!(outcome, CoordinatorHandoff::Noop));
        assert_eq!(coordinator.calls, 1);
        assert_eq!(coordinator.received, vec![expected]);
    }

    #[test]
    fn reconciliation_import_still_blocks_excluded_containers_and_non_text_extensions() {
        let temp = TempGraph::new(None);
        let graph = Graph::open(temp.path());
        let binding = binding(Some(&graph));
        let candidates = vec![
            candidate(
                "assets/note.md",
                GraphTextCandidateKind::Creation,
                None,
                &binding,
            ),
            candidate(
                "logseq/bak/pages/note.md",
                GraphTextCandidateKind::Creation,
                None,
                &binding,
            ),
            candidate(
                "node_modules/pkg/readme.md",
                GraphTextCandidateKind::Creation,
                None,
                &binding,
            ),
        ];
        let scan = scan(binding.clone(), candidates, Vec::new());
        let authority = GraphFixtureAuthority {
            graph: &graph,
            binding,
        };
        let mut coordinator = CountingCoordinator::new(FakeDisposition::Noop);

        let failure = execute_fixture(
            &scan,
            &authority,
            &mut coordinator,
            PreparationLimits::default(),
        )
        .expect_err("excluded graph-text containers must block");

        let PreparationFailure::Blocked(blocked) = failure else {
            panic!("excluded evidence should be blocked");
        };
        assert_eq!(
            blocked.reason,
            ReconciliationImportBlockReason::UnsupportedDiscovery
        );
        assert_eq!(
            blocked
                .evidence
                .iter()
                .map(|evidence| (evidence.kind, evidence.path.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (
                    ReconciliationImportEvidenceKind::UndecodableGraphTextPath,
                    "assets/note.md"
                ),
                (
                    ReconciliationImportEvidenceKind::UndecodableGraphTextPath,
                    "logseq/bak/pages/note.md"
                ),
                (
                    ReconciliationImportEvidenceKind::UndecodableGraphTextPath,
                    "node_modules/pkg/readme.md"
                ),
            ]
        );
        assert_eq!(coordinator.calls, 0);
        assert!(
            ManagedPath::parse("archive/note.txt").is_err(),
            "a genuinely unsupported extension must not become a reconciliation candidate"
        );
    }

    #[test]
    fn reconciliation_import_provider_conflict_diagnostic_is_informational() {
        let binding = binding(None);
        let candidate = candidate(
            "pages/edit.md",
            GraphTextCandidateKind::Edit,
            Some(ManagedTextKind::Page),
            &binding,
        );
        let authority = FixtureAuthority {
            binding: binding.clone(),
            kinds: [(candidate.path.clone(), ManagedTextKind::Page)]
                .into_iter()
                .collect(),
            revalidation: Ok(()),
        };
        let diagnostics = vec![GraphTextScanDiagnostic {
            path: "pages/note.sync-conflict-20260726.md".to_owned(),
            kind: GraphTextScanDiagnosticKind::ProviderConflictCopy,
            authority_path: None,
            file_resource_id: ContentDigest::of(b"conflict"),
            link_count: 1,
        }];
        let scan = scan(binding, vec![candidate], diagnostics);
        let mut coordinator = CountingCoordinator::new(FakeDisposition::Noop);

        let outcome = execute_fixture(
            &scan,
            &authority,
            &mut coordinator,
            PreparationLimits::default(),
        )
        .unwrap();

        assert!(matches!(outcome, CoordinatorHandoff::Noop));
        assert_eq!(coordinator.calls, 1);
        assert_eq!(coordinator.received, vec![vec!["pages/edit.md".to_owned()]]);
    }

    #[test]
    fn reconciliation_import_bounds_are_all_or_nothing() {
        let binding = binding(None);
        let candidates = vec![
            candidate(
                "pages/a.md",
                GraphTextCandidateKind::Creation,
                None,
                &binding,
            ),
            candidate(
                "pages/b.md",
                GraphTextCandidateKind::Creation,
                None,
                &binding,
            ),
        ];
        let authority = FixtureAuthority {
            binding: binding.clone(),
            kinds: candidates
                .iter()
                .map(|candidate| (candidate.path.clone(), ManagedTextKind::Page))
                .collect(),
            revalidation: Ok(()),
        };
        let scan = scan(binding, candidates, Vec::new());
        let mut coordinator = CountingCoordinator::new(FakeDisposition::Noop);

        let failure = execute_fixture(
            &scan,
            &authority,
            &mut coordinator,
            PreparationLimits {
                candidates: 1,
                path_bytes: MAX_IMPORT_PATH_BYTES,
            },
        )
        .expect_err("overflow must block the complete epoch");

        assert!(matches!(
            failure,
            PreparationFailure::Blocked(ReconciliationImportDiscoveryBlock {
                reason: ReconciliationImportBlockReason::CandidateCountLimit,
                ..
            })
        ));
        assert_eq!(coordinator.calls, 0);

        let total_path_bytes = scan
            .candidates
            .iter()
            .map(|candidate| candidate.path.as_str().len() as u64)
            .sum::<u64>();
        let failure = execute_fixture(
            &scan,
            &authority,
            &mut coordinator,
            PreparationLimits {
                candidates: 2,
                path_bytes: total_path_bytes - 1,
            },
        )
        .expect_err("path-byte overflow must block the complete epoch");
        assert!(matches!(
            failure,
            PreparationFailure::Blocked(ReconciliationImportDiscoveryBlock {
                reason: ReconciliationImportBlockReason::CandidatePathBytesLimit,
                ..
            })
        ));
        assert_eq!(coordinator.calls, 0);
    }

    #[test]
    fn reconciliation_import_portable_collision_blocks_without_handoff() {
        let binding = binding(None);
        let candidates = vec![
            candidate(
                "pages/A.md",
                GraphTextCandidateKind::Creation,
                None,
                &binding,
            ),
            candidate(
                "pages/a.md",
                GraphTextCandidateKind::Creation,
                None,
                &binding,
            ),
        ];
        let authority = FixtureAuthority {
            binding: binding.clone(),
            kinds: candidates
                .iter()
                .map(|candidate| (candidate.path.clone(), ManagedTextKind::Page))
                .collect(),
            revalidation: Ok(()),
        };
        let scan = scan(binding, candidates, Vec::new());
        let mut coordinator = CountingCoordinator::new(FakeDisposition::Noop);

        let failure = execute_fixture(
            &scan,
            &authority,
            &mut coordinator,
            PreparationLimits::default(),
        )
        .expect_err("portable collision must block the complete epoch");

        let PreparationFailure::Blocked(blocked) = failure else {
            panic!("portable collision should be blocked");
        };
        assert_eq!(
            blocked.reason,
            ReconciliationImportBlockReason::UnsupportedDiscovery
        );
        assert_eq!(
            blocked
                .evidence
                .iter()
                .filter(|evidence| {
                    evidence.kind == ReconciliationImportEvidenceKind::PortablePathCollision
                })
                .count(),
            2
        );
        assert_eq!(coordinator.calls, 0);
    }

    #[test]
    fn reconciliation_import_stale_binding_requests_fresh_full_scan() {
        let binding = binding(None);
        let candidate = candidate(
            "pages/edit.md",
            GraphTextCandidateKind::Edit,
            Some(ManagedTextKind::Page),
            &binding,
        );
        let authority = FixtureAuthority {
            binding: binding.clone(),
            kinds: [(candidate.path.clone(), ManagedTextKind::Page)]
                .into_iter()
                .collect(),
            revalidation: Err(ReconciliationImportRetryReason::ExpectedBindingMoved),
        };
        let scan = scan(binding, vec![candidate], Vec::new());
        let mut coordinator = CountingCoordinator::new(FakeDisposition::Noop);

        let failure = execute_fixture(
            &scan,
            &authority,
            &mut coordinator,
            PreparationLimits::default(),
        )
        .expect_err("stale binding must discard the scan");

        assert!(matches!(
            failure,
            PreparationFailure::RetryFull(ReconciliationImportRetry {
                reason: ReconciliationImportRetryReason::ExpectedBindingMoved,
                ..
            })
        ));
        assert_eq!(coordinator.calls, 0);
    }

    #[test]
    fn reconciliation_import_binding_move_before_handoff_requests_fresh_full_scan() {
        let binding = binding(None);
        let candidate = candidate(
            "pages/edit.md",
            GraphTextCandidateKind::Edit,
            Some(ManagedTextKind::Page),
            &binding,
        );
        let authority = SequencedFixtureAuthority {
            binding: binding.clone(),
            kinds: [(candidate.path.clone(), ManagedTextKind::Page)]
                .into_iter()
                .collect(),
            moves_before_handoff: true,
            revalidation_calls: Cell::new(0),
        };
        let scan = scan(binding, vec![candidate], Vec::new());
        let mut coordinator = CountingCoordinator::new(FakeDisposition::Noop);

        let failure = execute_fixture(
            &scan,
            &authority,
            &mut coordinator,
            PreparationLimits::default(),
        )
        .expect_err("a binding move before handoff must discard the path set");

        assert!(matches!(
            failure,
            PreparationFailure::RetryFull(ReconciliationImportRetry {
                reason: ReconciliationImportRetryReason::ExpectedBindingMoved,
                ..
            })
        ));
        assert_eq!(authority.revalidation_calls.get(), 2);
        assert_eq!(coordinator.calls, 0);
    }

    #[test]
    fn reconciliation_import_failed_closed_is_returned_without_replanning() {
        let binding = binding(None);
        let candidate = candidate(
            "pages/edit.md",
            GraphTextCandidateKind::Edit,
            Some(ManagedTextKind::Page),
            &binding,
        );
        let authority = FixtureAuthority {
            binding: binding.clone(),
            kinds: [(candidate.path.clone(), ManagedTextKind::Page)]
                .into_iter()
                .collect(),
            revalidation: Ok(()),
        };
        let scan = scan(binding, vec![candidate], Vec::new());
        let mut coordinator = CountingCoordinator::new(FakeDisposition::FailedClosed);

        let outcome = execute_fixture(
            &scan,
            &authority,
            &mut coordinator,
            PreparationLimits::default(),
        )
        .unwrap();

        assert!(matches!(
            outcome,
            CoordinatorHandoff::FailedClosed("durable-batch")
        ));
        assert_eq!(coordinator.calls, 1);
    }
}
