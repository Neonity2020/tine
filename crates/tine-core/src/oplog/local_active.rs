//! The local managed-storage runtime boundary.
//!
//! Clean activation commits a baseline-plus-manifest authority and opens the
//! enrolled engine and disposable SQLite projection. Subsequent process opens
//! reconstruct the same authority from the current enrollment and sealed
//! accepted history; there is no detached or inactive bootstrap runtime.
//!
//! `LocalActiveAuthority` is the only value that admits managed mutation,
//! projection, import, or coordinator execution. It has no public constructor,
//! serialized form, `Clone`, or production mint outside the verified activation
//! and reopen boundaries in this module.
//!
//! `PromotedLocalRuntime` owns the archive-rooted workspace lease together with
//! the SQLite projection it authorizes. Lease identity is re-proved at
//! authority-changing coordinator boundaries and failures latch terminal runtime
//! revocation. Safe handoff remains gated on complete device-local drains.
//!
//! Retained resume state is only an accelerator: refusal or damage falls back to
//! authenticated history replay and cannot become an alternate authority.

use crate::model::Graph;
#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
#[cfg(test)]
use std::sync::Mutex;

use super::enrollment::VerifiedLocalCompositionError;
use super::hot_engine::{EngineError, LocalAuthorGeneration, ShardedHotEngine};
use super::object_store::StoreError;
use super::sqlite::{
    LeasedWorkspaceProjection, ProjectionError, SqliteFrontier, TailOverlay, WorkspaceLeaseIdentity,
};
use super::{DeviceId, ProjectionEndpointBinding, ProjectionIntentId, SessionId, WorkspaceId};

/// A private seal. Sibling modules can name the sealed types but can never
/// construct one, because this module is the only place `Seal` is reachable.
mod seal {
    #[derive(Debug)]
    pub(super) struct Seal;
}

#[cfg(test)]
thread_local! {
    /// Device-local SQLite reads issued by this module's promoted-runtime
    /// boundaries. This module is the only place the promoted admission path
    /// could reach SQLite from, so counting the call sites here is an exact
    /// account of "SQLite statements in mutation admission".
    static PROMOTED_SQLITE_FRONTIER_READS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    /// Archive control-directory identity and canonical resource-claim reads
    /// issued by this module. Both re-stat or reread the archive through an
    /// already-open immutable capability.
    static PROMOTED_ARCHIVE_IDENTITY_READS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
static WORKSPACE_RESUME_LIFECYCLE_CUTS: Mutex<
    BTreeMap<WorkspaceId, (ResumeLifecycleCut, Box<dyn FnOnce() + Send>)>,
> = Mutex::new(BTreeMap::new());

/// Exact causal accounting for one promoted-runtime boundary or admission.
///
/// Counters, never timing.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PromotedRuntimeInstrumentation {
    pub(crate) enrollment: super::enrollment::EnrollmentInstrumentation,
    pub(crate) sqlite_frontier_reads: usize,
    pub(crate) archive_identity_reads: usize,
    /// Archive-rooted workspace runtime lease acquisitions. A retained runtime
    /// takes exactly one for its whole writable life.
    pub(crate) workspace_lease_acquisitions: usize,
    /// While-held workspace-lease identity revalidations: one held-handle stat
    /// plus one no-follow resolution of the exact lease pathname each. It is a
    /// boundary fact, so an unchanged-head admission must perform none.
    pub(crate) workspace_lease_identity_revalidations: usize,
}

#[cfg(test)]
impl PromotedRuntimeInstrumentation {
    pub(crate) fn capture() -> Self {
        Self {
            enrollment: super::enrollment::EnrollmentInstrumentation::capture(),
            sqlite_frontier_reads: PROMOTED_SQLITE_FRONTIER_READS.with(std::cell::Cell::get),
            archive_identity_reads: PROMOTED_ARCHIVE_IDENTITY_READS.with(std::cell::Cell::get),
            workspace_lease_acquisitions: super::sqlite::workspace_runtime_lease_acquisitions(),
            workspace_lease_identity_revalidations:
                super::sqlite::workspace_lease_identity_revalidations(),
        }
    }

    /// The work performed since `self` was captured.
    pub(crate) fn since(self) -> Self {
        let now = Self::capture();
        Self {
            enrollment: self.enrollment.since(),
            sqlite_frontier_reads: now.sqlite_frontier_reads - self.sqlite_frontier_reads,
            archive_identity_reads: now.archive_identity_reads - self.archive_identity_reads,
            workspace_lease_acquisitions: now.workspace_lease_acquisitions
                - self.workspace_lease_acquisitions,
            workspace_lease_identity_revalidations: now.workspace_lease_identity_revalidations
                - self.workspace_lease_identity_revalidations,
        }
    }
}

#[cfg(test)]
fn count(counter: &'static std::thread::LocalKey<std::cell::Cell<usize>>) {
    counter.with(|value| value.set(value.get().saturating_add(1)));
}

#[derive(Debug)]
pub(crate) enum LocalActivationError {
    Enrollment(VerifiedLocalCompositionError),
    /// A live runtime component does not authenticate the committed binding.
    RuntimeBinding(String),
}

impl fmt::Display for LocalActivationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enrollment(error) => error.fmt(formatter),
            Self::RuntimeBinding(detail) => {
                write!(formatter, "local-active runtime binding failed: {detail}")
            }
        }
    }
}

impl std::error::Error for LocalActivationError {}

impl From<VerifiedLocalCompositionError> for LocalActivationError {
    fn from(error: VerifiedLocalCompositionError) -> Self {
        Self::Enrollment(error)
    }
}

/// The value every new-architecture local mutation, projection, import, and
/// coordinator execution path requires.
///
/// The only production constructor is [`PromotedRuntimeAdmission::admission`],
/// which is itself derived from both a live [`LocalActiveAuthority`] and the
/// exact [`PromotedLocalRuntime`] token. Future Tauri wiring therefore cannot
/// reach a writable runtime without first minting an authority *and* opening
/// the promoted runtime that authority's durable promotion state authorizes.
pub(crate) struct LocalRuntimeAdmission<'a> {
    provenance: AdmissionProvenance<'a>,
}

/// Nonconstructible authority for one promoted runtime/session to draft a
/// local author transaction at its current author-generation root.
///
/// It is intentionally affine and non-serializable. The coordinator mints it
/// immediately before local drafting and never exposes it to a facade.
pub(crate) struct AdmittedLocalAuthorAuthority<'a> {
    workspace_id: WorkspaceId,
    device_id: DeviceId,
    session_id: SessionId,
    generation: LocalAuthorGeneration,
    _admission: std::marker::PhantomData<&'a LocalRuntimeAdmission<'a>>,
    _seal: seal::Seal,
}

impl AdmittedLocalAuthorAuthority<'_> {
    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub(crate) const fn device_id(&self) -> DeviceId {
        self.device_id
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn generation(&self) -> &LocalAuthorGeneration {
        &self.generation
    }
}

enum AdmissionProvenance<'a> {
    Clean(&'a CleanRuntimeAdmission<'a>),
    /// Retained for the crate-private `#[cfg(test)]` fixture corpus only; see
    /// [`LocalRuntimeAdmission::unenrolled_pre_activation`].
    #[cfg(test)]
    UnenrolledPreActivation,
}

impl LocalRuntimeAdmission<'_> {
    /// The pre-activation escape hatch retained only for the crate-private
    /// deterministic scenario corpus and the coordinator/session/import
    /// regressions that predate enrollment. Those fixtures build engines
    /// directly instead of through a bootstrap publication, so no genuine
    /// authority is constructible for them yet.
    ///
    /// It has no production caller and is therefore `#[cfg(test)]`. Its live
    /// callers are `operational_coordinator.rs`'s `mod tests`,
    /// `trusted_local_commit.rs`'s `mod tests`, and
    /// `hot_engine_integration_tests/hot_overlay_tests.rs`.
    ///
    /// Two independent fences keep it away from a user's real graph. It stays
    /// `pub(crate)` — as does [`LocalRuntimeAdmission`] itself — so app startup
    /// and Tauri cannot name or construct one, and outside this crate the only
    /// way to obtain an admission remains a live runtime permit. And
    /// [`Self::authorize`] refuses outright when the engine offered is a
    /// promoted runtime, so even inside the crate this hatch can never
    /// authorize work over a promoted lineage.
    #[cfg(test)]
    pub(crate) const fn unenrolled_pre_activation() -> Self {
        Self {
            provenance: AdmissionProvenance::UnenrolledPreActivation,
        }
    }

    /// Revalidate the live runtime immediately before work is admitted.
    ///
    /// A promoted admission requires the supplied engine to be the exact
    /// promoted engine instance, proved by its process-local runtime authority,
    /// and re-proves the complete promoted binding: archive resource, storage
    /// binding, authenticated bootstrap-anchor to current-history transition,
    /// engine and SQLite current frontier, committed enrollment
    /// verification/session/head, and live graph capability. A same-identity
    /// engine rebuilt from divergent, rolled-back, or merely
    /// higher-sequence history is refused here, before any durable or graph
    /// mutation.
    pub(crate) fn authorize(
        &self,
        graph: &Graph,
        engine: &ShardedHotEngine,
    ) -> Result<(), RuntimePromotionError> {
        match &self.provenance {
            AdmissionProvenance::Clean(admission) => admission.authorize_engine(graph, engine),
            #[cfg(test)]
            AdmissionProvenance::UnenrolledPreActivation => Ok(()),
        }
    }

    /// Mint exact local-author authority from a live promoted admission.
    ///
    /// The pre-activation fixture admission is deliberately refused: raw
    /// author construction is confined to the coordinator's `cfg(test)` helper
    /// and the legacy engine fixture helper, which production promoted runtimes
    /// reject.
    pub(crate) fn mint_local_author_authority<'a>(
        &'a self,
        graph: &Graph,
        engine: &ShardedHotEngine,
        endpoint: ProjectionEndpointBinding,
    ) -> Result<AdmittedLocalAuthorAuthority<'a>, RuntimePromotionError> {
        self.authorize(graph, engine)?;
        let (admitted_endpoint, admitted_workspace, session_id) = match &self.provenance {
            AdmissionProvenance::Clean(admission) => (
                admission.endpoint,
                admission.workspace_id,
                admission.session_id,
            ),
            #[cfg(test)]
            AdmissionProvenance::UnenrolledPreActivation => {
                return Err(RuntimePromotionError::Activation(
                    LocalActivationError::RuntimeBinding(
                        "local author identity requires a live managed runtime session".into(),
                    ),
                ));
            }
        };
        if endpoint != admitted_endpoint
            || endpoint.device_id() != admitted_endpoint.device_id()
            || engine.workspace_id() != admitted_workspace
        {
            return Err(RuntimePromotionError::Activation(
                LocalActivationError::RuntimeBinding(
                    "promoted local author binding differs from the admitted endpoint or workspace"
                        .into(),
                ),
            ));
        }
        Ok(AdmittedLocalAuthorAuthority {
            workspace_id: engine.workspace_id(),
            device_id: endpoint.device_id(),
            session_id,
            generation: engine
                .local_author_generation()
                .map_err(RuntimePromotionError::Engine)?,
            _admission: std::marker::PhantomData,
            _seal: seal::Seal,
        })
    }

    /// Re-derive archive-rooted workspace authority immediately before one
    /// authority-changing boundary.
    ///
    /// This is the coordinator's phase gate. It is deliberately reachable only
    /// through the admission every phase already holds, so a caller cannot
    /// execute a boundary without the capability that proves it — there is no
    /// separate probe value to forget to pass, and Tauri and unrelated crate
    /// modules cannot construct either one.
    ///
    /// A failure latches the promoted runtime's terminal revocation, so it also
    /// prevents every later boundary, later window, and later admission.
    ///
    /// The pre-activation hatch has no workspace lease at all and
    /// [`Self::authorize`] already refuses it a promoted lineage, so it has no
    /// workspace authority to re-derive and reports `Ok`.
    pub(crate) fn reprove_workspace_authority(
        &self,
        boundary: WorkspaceAuthorityBoundary,
    ) -> Result<(), WorkspaceAuthorityRefusal> {
        match &self.provenance {
            AdmissionProvenance::Clean(admission) => admission.reprove(boundary),
            #[cfg(test)]
            AdmissionProvenance::UnenrolledPreActivation => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime authority: verified clean activation -> writable local runtime.
// ---------------------------------------------------------------------------

/// Why a runtime promotion, promoted open, or promoted admission failed.
#[derive(Debug)]
pub(crate) enum RuntimePromotionError {
    Activation(LocalActivationError),
    Enrollment(VerifiedLocalCompositionError),
    Store(StoreError),
    Engine(EngineError),
    Sqlite(ProjectionError),
    /// The durable runtime state, clean baseline anchor, or authenticated history
    /// transition does not authenticate the live runtime.
    Anchor(&'static str),
    /// This runtime no longer owns the archive-rooted workspace lease and has
    /// latched [`RuntimeRevocation`]. Terminal: recovery is a fresh reopen or
    /// crash takeover, never this runtime.
    WorkspaceAuthorityRevoked(WorkspaceAuthorityRefusal),
    /// The current operation could not perform the identity check. Retryable on
    /// this runtime because no replacement was proved and no latch was set.
    WorkspaceAuthorityCheckUnavailable(WorkspaceAuthorityRefusal),
}

impl fmt::Display for RuntimePromotionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Activation(error) => error.fmt(formatter),
            Self::Enrollment(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            Self::Engine(error) => error.fmt(formatter),
            Self::Sqlite(error) => error.fmt(formatter),
            Self::Anchor(detail) => {
                write!(formatter, "promoted runtime anchor failed: {detail}")
            }
            Self::WorkspaceAuthorityRevoked(refusal) => refusal.fmt(formatter),
            Self::WorkspaceAuthorityCheckUnavailable(refusal) => refusal.fmt(formatter),
        }
    }
}

impl std::error::Error for RuntimePromotionError {}

impl From<WorkspaceAuthorityRefusal> for RuntimePromotionError {
    fn from(refusal: WorkspaceAuthorityRefusal) -> Self {
        if refusal.is_terminal() {
            Self::WorkspaceAuthorityRevoked(refusal)
        } else {
            Self::WorkspaceAuthorityCheckUnavailable(refusal)
        }
    }
}

impl From<LocalActivationError> for RuntimePromotionError {
    fn from(error: LocalActivationError) -> Self {
        Self::Activation(error)
    }
}

impl From<VerifiedLocalCompositionError> for RuntimePromotionError {
    fn from(error: VerifiedLocalCompositionError) -> Self {
        Self::Enrollment(error)
    }
}

impl From<StoreError> for RuntimePromotionError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<EngineError> for RuntimePromotionError {
    fn from(error: EngineError) -> Self {
        Self::Engine(error)
    }
}

impl From<ProjectionError> for RuntimePromotionError {
    fn from(error: ProjectionError) -> Self {
        Self::Sqlite(error)
    }
}

/// The boundary at which archive-rooted workspace authority was demanded.
///
/// Every variant is a place this runtime is about to change authority, take
/// authority, or hand out the ability to change it. The variant is carried into
/// every refusal so a diagnosis names the exact phase rather than "the lease
/// moved".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceAuthorityBoundary {
    /// Opening a promoted mutation window (`admit_promoted_mutation`).
    Admission,
    /// Authorizing an already-open window's engine.
    WindowAuthorization,
    /// Handing out the mutable engine/SQLite/tail parts.
    MutableParts,
    /// Coordinator: immutable batch publication into the archive.
    Publication,
    /// Coordinator: one bounded accepted-history/archive staging slice.
    ArchiveStage,
    /// Coordinator: bounded tail admission of accepted batches.
    TailAdmission,
    /// Coordinator: the SQLite drain/advance.
    SqliteDrain,
    /// Coordinator: one manifested Markdown projection step.
    ProjectionDrain,
}

impl WorkspaceAuthorityBoundary {
    pub(crate) const fn describe(self) -> &'static str {
        match self {
            Self::Admission => "promoted mutation admission",
            Self::WindowAuthorization => "promoted window authorization",
            Self::MutableParts => "mutable runtime parts handout",
            Self::Publication => "coordinator immutable publication",
            Self::ArchiveStage => "coordinator accepted-history archive staging",
            Self::TailAdmission => "coordinator tail admission",
            Self::SqliteDrain => "coordinator SQLite drain",
            Self::ProjectionDrain => "coordinator manifested projection",
        }
    }
}

/// The terminal state one promoted runtime latches the first time its retained
/// archive-rooted workspace lease stops naming its own lease file.
///
/// It is one way. It records where the loss was first observed and the exact
/// [`ProjectionError`] that observed it, and nothing clears it: the runtime is
/// dead for every purpose from that instant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeRevocation {
    boundary: WorkspaceAuthorityBoundary,
    cause: ProjectionError,
}

impl fmt::Display for RuntimeRevocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "workspace authority was revoked at the {}: {}",
            self.boundary.describe(),
            self.cause
        )
    }
}

/// One refused workspace-authority reproof.
///
/// It names the boundary that demanded the proof *now*, the exact cause, and,
/// only for a proven identity replacement, the terminal revocation the runtime
/// carries. A transient inability to check has no revocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkspaceAuthorityRefusal {
    demanded_at: WorkspaceAuthorityBoundary,
    revocation: Option<RuntimeRevocation>,
    cause: ProjectionError,
}

impl WorkspaceAuthorityRefusal {
    pub(crate) const fn revocation(&self) -> Option<&RuntimeRevocation> {
        self.revocation.as_ref()
    }

    pub(crate) const fn is_terminal(&self) -> bool {
        self.revocation.is_some()
    }
}

impl fmt::Display for WorkspaceAuthorityRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.revocation {
            Some(revocation) => write!(
                formatter,
                "the {} refused: {}",
                self.demanded_at.describe(),
                revocation
            ),
            None => write!(
                formatter,
                "the {} could not revalidate workspace authority for this operation: {}",
                self.demanded_at.describe(),
                self.cause
            ),
        }
    }
}

impl std::error::Error for WorkspaceAuthorityRefusal {}

/// One promoted runtime's terminal revocation latch.
///
/// It is interior-mutable on purpose. A live mutation window hands the
/// coordinator an immutably borrowed [`LocalRuntimeAdmission`] while the engine,
/// database, and tail are exclusively borrowed elsewhere, so the phase boundary
/// that discovers the loss can only reach the latch through a shared reference.
/// `Cell`-style latching is what lets that discovery still be terminal for the
/// runtime rather than local to one call.
#[derive(Debug, Default)]
struct RuntimeRevocationLatch {
    revoked: std::cell::RefCell<Option<RuntimeRevocation>>,
}

impl RuntimeRevocationLatch {
    fn latched(&self) -> Option<RuntimeRevocation> {
        self.revoked.borrow().clone()
    }

    /// Latch the first observed loss. A later loss never replaces it, so the
    /// recorded boundary stays the one that actually lost the workspace.
    fn revoke(
        &self,
        boundary: WorkspaceAuthorityBoundary,
        cause: ProjectionError,
    ) -> RuntimeRevocation {
        self.revoked
            .borrow_mut()
            .get_or_insert(RuntimeRevocation { boundary, cause })
            .clone()
    }

    /// Refuse `demanded_at` outright if this runtime is already revoked.
    ///
    /// Pure memory: this is what the per-mutation admission path runs, so it
    /// adds no filesystem work at any admission count.
    fn guard(
        &self,
        demanded_at: WorkspaceAuthorityBoundary,
    ) -> Result<(), WorkspaceAuthorityRefusal> {
        match self.latched() {
            Some(revocation) => Err(WorkspaceAuthorityRefusal {
                demanded_at,
                cause: revocation.cause.clone(),
                revocation: Some(revocation),
            }),
            None => Ok(()),
        }
    }

    /// The one authority-changing shape: refuse if already revoked, otherwise
    /// re-derive lease identity. A proven replacement latches terminally;
    /// inability to perform the check refuses this operation without latching.
    fn reprove_with<T>(
        &self,
        demanded_at: WorkspaceAuthorityBoundary,
        revalidate: impl FnOnce() -> Result<T, ProjectionError>,
    ) -> Result<T, WorkspaceAuthorityRefusal> {
        self.guard(demanded_at)?;
        revalidate().map_err(|cause| self.refuse_cause(demanded_at, cause))
    }

    fn refuse_cause(
        &self,
        demanded_at: WorkspaceAuthorityBoundary,
        cause: ProjectionError,
    ) -> WorkspaceAuthorityRefusal {
        if matches!(cause, ProjectionError::LeaseIdentityUnavailable(_)) {
            WorkspaceAuthorityRefusal {
                demanded_at,
                revocation: None,
                cause,
            }
        } else {
            let revocation = self.revoke(demanded_at, cause);
            WorkspaceAuthorityRefusal {
                demanded_at,
                cause: revocation.cause.clone(),
                revocation: Some(revocation),
            }
        }
    }
}

/// One mutation window for the clean baseline-plus-manifest runtime.
///
/// Unlike [`PromotedRuntimeAdmission`], this carries no enrollment/history or
/// bootstrap-Patricia proof. Its authority is exactly the live engine
/// identity, source endpoint, graph resource and the while-held archive-rooted
/// SQLite workspace lease. The accepted manifest and SQLite frontier are
/// checked by [`CleanLocalRuntime::admit_clean_mutation`] before the fields are
/// split.
pub(crate) struct CleanRuntimeAdmission<'a> {
    workspace_id: WorkspaceId,
    session_id: SessionId,
    endpoint: ProjectionEndpointBinding,
    engine_authority: super::hot_engine::EngineAuthority,
    workspace: WorkspaceLeaseIdentity<'a>,
    revocation: &'a RuntimeRevocationLatch,
    _seal: seal::Seal,
}

impl CleanRuntimeAdmission<'_> {
    fn reprove(
        &self,
        boundary: WorkspaceAuthorityBoundary,
    ) -> Result<(), WorkspaceAuthorityRefusal> {
        self.revocation
            .reprove_with(boundary, || self.workspace.revalidate())
    }

    fn authorize_engine(
        &self,
        graph: &Graph,
        engine: &ShardedHotEngine,
    ) -> Result<(), RuntimePromotionError> {
        self.revocation
            .guard(WorkspaceAuthorityBoundary::WindowAuthorization)?;
        self.workspace.revalidate().map_err(|cause| {
            RuntimePromotionError::from(
                self.revocation
                    .refuse_cause(WorkspaceAuthorityBoundary::WindowAuthorization, cause),
            )
        })?;
        let graph_resource_id = graph.canonical_resource_id().map_err(|error| {
            RuntimePromotionError::Activation(LocalActivationError::RuntimeBinding(
                error.to_string(),
            ))
        })?;
        if !self.engine_authority.matches(engine.runtime_authority())
            || engine.workspace_id() != self.workspace_id
            || graph_resource_id != self.endpoint.graph_resource_id()
            || engine.projection_endpoint_binding() != Some(self.endpoint)
        {
            return Err(RuntimePromotionError::Activation(
                LocalActivationError::RuntimeBinding(
                    "clean runtime admission no longer matches its engine, graph, or endpoint"
                        .into(),
                ),
            ));
        }
        Ok(())
    }
}

/// Production runtime shape for the clean managed-storage architecture.
/// Durable authority lives in the immutable lazy-genesis baseline plus
/// committed operation manifests; SQLite and this value are disposable
/// process state. No history/status Patricia or persistent projection queue is
/// opened by this runtime.
pub(crate) struct CleanLocalRuntime {
    session_id: SessionId,
    endpoint: ProjectionEndpointBinding,
    engine: Box<ShardedHotEngine>,
    projection: LeasedWorkspaceProjection,
    revocation: RuntimeRevocationLatch,
}

impl CleanLocalRuntime {
    pub(crate) fn from_open_parts(
        session_id: SessionId,
        endpoint: ProjectionEndpointBinding,
        engine: ShardedHotEngine,
        projection: LeasedWorkspaceProjection,
    ) -> Result<Self, RuntimePromotionError> {
        if engine.projection_endpoint_binding() != Some(endpoint)
            || engine
                .require_index_free_clean_projection_runtime()
                .is_err()
        {
            return Err(RuntimePromotionError::Activation(
                LocalActivationError::RuntimeBinding(
                    "clean runtime parts are not index-free or endpoint-bound".into(),
                ),
            ));
        }
        projection
            .database()
            .ensure_projection_baseline_digest_column()
            .map_err(RuntimePromotionError::Sqlite)?;
        projection
            .revalidate_workspace_lease_identity()
            .map_err(RuntimePromotionError::Sqlite)?;
        let engine_frontier = engine
            .accepted_frontier_root()
            .map_err(RuntimePromotionError::Engine)?;
        if !engine_frontier.same_accepted_authority(projection.database().required_frontier_root())
        {
            return Err(RuntimePromotionError::Activation(
                LocalActivationError::RuntimeBinding(
                    "clean runtime SQLite is not at the accepted manifest frontier".into(),
                ),
            ));
        }
        Ok(Self {
            session_id,
            endpoint,
            engine: Box::new(engine),
            projection,
            revocation: RuntimeRevocationLatch::default(),
        })
    }

    pub(crate) fn engine(&self) -> &ShardedHotEngine {
        &self.engine
    }

    pub(crate) fn flush_local_projection_completions(
        &mut self,
        retained_intents: BTreeSet<ProjectionIntentId>,
    ) -> Result<bool, EngineError> {
        self.engine
            .flush_local_projection_completions(retained_intents)
    }

    pub(crate) const fn database(&self) -> &SqliteFrontier {
        self.projection.database()
    }

    pub(crate) const fn endpoint(&self) -> ProjectionEndpointBinding {
        self.endpoint
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) fn admit_clean_mutation(
        &mut self,
        graph: &Graph,
    ) -> Result<CleanRuntimeSession<'_>, RuntimePromotionError> {
        self.admit_clean_session(graph, true)
    }

    /// Admit only the derived-state completion of an already committed clean
    /// manifest.  The immutable manifest is now the authority, so SQLite is
    /// expected to be behind until the affine continuation advances it.  This
    /// boundary retains every graph, endpoint, engine, index-free and workspace
    /// lease proof from ordinary admission; it relaxes only the equality that
    /// would make catching the derived projection up impossible.
    pub(crate) fn admit_clean_derived_recovery(
        &mut self,
        graph: &Graph,
    ) -> Result<CleanRuntimeSession<'_>, RuntimePromotionError> {
        self.admit_clean_session(graph, false)
    }

    fn admit_clean_session(
        &mut self,
        graph: &Graph,
        require_sqlite_at_manifest_frontier: bool,
    ) -> Result<CleanRuntimeSession<'_>, RuntimePromotionError> {
        self.revocation
            .guard(WorkspaceAuthorityBoundary::Admission)?;
        self.projection
            .revalidate_workspace_lease_identity()
            .map_err(RuntimePromotionError::Sqlite)?;
        let graph_resource_id = graph.canonical_resource_id().map_err(|error| {
            RuntimePromotionError::Activation(LocalActivationError::RuntimeBinding(
                error.to_string(),
            ))
        })?;
        let engine_frontier = self
            .engine
            .accepted_frontier_root()
            .map_err(RuntimePromotionError::Engine)?;
        if graph_resource_id != self.endpoint.graph_resource_id()
            || self.engine.projection_endpoint_binding() != Some(self.endpoint)
            || self
                .engine
                .require_index_free_clean_projection_runtime()
                .is_err()
            || (require_sqlite_at_manifest_frontier
                && !engine_frontier
                    .same_accepted_authority(self.projection.database().required_frontier_root()))
        {
            return Err(RuntimePromotionError::Activation(
                LocalActivationError::RuntimeBinding(
                    "clean runtime graph, endpoint, engine, or SQLite binding changed".into(),
                ),
            ));
        }
        let Self {
            session_id,
            endpoint,
            engine,
            projection,
            revocation,
        } = self;
        let (database, workspace) = projection.database_and_lease_identity();
        let admission = CleanRuntimeAdmission {
            workspace_id: engine.workspace_id(),
            session_id: *session_id,
            endpoint: *endpoint,
            engine_authority: engine.runtime_authority().clone(),
            workspace,
            revocation,
            _seal: seal::Seal,
        };
        Ok(CleanRuntimeSession {
            admission,
            engine,
            database,
        })
    }
}

pub(crate) struct CleanRuntimeSession<'a> {
    admission: CleanRuntimeAdmission<'a>,
    engine: &'a mut ShardedHotEngine,
    database: &'a mut SqliteFrontier,
}

impl CleanRuntimeSession<'_> {
    /// The window's admission without handing out the mutable parts.
    ///
    /// No production path needs this: the coordinator always takes
    /// [`Self::parts`]. Its one caller is the `bounded_admission` cost test
    /// below, which must measure an admission-plus-authorization window
    /// *without* paying the mutable-parts boundary.
    #[cfg(test)]
    pub(crate) const fn admission(&self) -> LocalRuntimeAdmission<'_> {
        LocalRuntimeAdmission {
            provenance: AdmissionProvenance::Clean(&self.admission),
        }
    }

    pub(crate) fn parts(
        &mut self,
    ) -> Result<
        (
            LocalRuntimeAdmission<'_>,
            &mut ShardedHotEngine,
            &mut SqliteFrontier,
        ),
        WorkspaceAuthorityRefusal,
    > {
        self.admission
            .reprove(WorkspaceAuthorityBoundary::MutableParts)?;
        Ok((
            LocalRuntimeAdmission {
                provenance: AdmissionProvenance::Clean(&self.admission),
            },
            self.engine,
            self.database,
        ))
    }
}

#[cfg(test)]
thread_local! {
    /// Fires after a takeover has authenticated the crashed predecessor and
    /// before it takes any lease. A racing newcomer suspended here is exactly
    /// the loser the compare-and-swap must refuse.
    static TAKEOVER_PREDECESSOR_OBSERVED: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_takeover_predecessor_observed_hook_for_test(hook: Box<dyn FnMut()>) {
    TAKEOVER_PREDECESSOR_OBSERVED.with(|slot| *slot.borrow_mut() = Some(hook));
}

#[cfg(test)]
thread_local! {
    /// Fails the next promoted mint *after* its device-local database is open.
    ///
    /// That is the one failure boundary where the retained workspace lease can
    /// only be recovered by closing the database it now lives inside, so it is
    /// the boundary a natural fault is hardest to reach and the one most worth
    /// a receipt.
    static FAIL_AFTER_PROMOTED_DATABASE_OPEN: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    /// Makes the next process-only candidate fail its typed binding check. The
    /// open must discard it and take the unchanged ordinary replay fallback.
    static FAIL_NEXT_SAME_PROCESS_PROMOTION_TOKEN_MATCH: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_promotion_after_the_database_opens_for_test() {
    FAIL_AFTER_PROMOTED_DATABASE_OPEN.with(|flag| flag.set(true));
}

#[cfg(test)]
pub(crate) fn mismatch_next_same_process_promotion_token_for_test() {
    FAIL_NEXT_SAME_PROCESS_PROMOTION_TOKEN_MATCH.with(|flag| flag.set(true));
}

/// The two resume-lifecycle boundaries a deterministic fault has to reach from
/// outside, because neither is a durability cut any existing hook covers: the
/// instant after this open took the workspace lease and before it reads a
/// published candidate, and the instant after a publication committed and
/// before the reclamation pass it authorized.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResumeLifecycleCut {
    BeforeCandidateRead,
    BeforeReclamation,
    AfterWatcherQuiesce,
    BeforeSafeClear,
    AfterSafeClear,
    AfterSafeCommit,
}

#[cfg(test)]
thread_local! {
    static RESUME_LIFECYCLE_CUT: std::cell::RefCell<Option<(ResumeLifecycleCut, Box<dyn FnOnce() + Send>)>> =
        const { std::cell::RefCell::new(None) };
    static SAFE_GRAPH_WRITER_PROBE_ARMED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    static SAFE_GRAPH_WRITER_PROBE_RESULT: std::cell::Cell<Option<std::io::ErrorKind>> =
        const { std::cell::Cell::new(None) };
}

/// Run `action` exactly once, the next time `cut` is reached. Per-thread, armed
/// explicitly, and self-clearing when it fires.
#[cfg(test)]
pub(crate) fn act_once_at_resume_lifecycle_cut_for_test(
    cut: ResumeLifecycleCut,
    action: Box<dyn FnOnce() + Send>,
) {
    RESUME_LIFECYCLE_CUT.with(|slot| *slot.borrow_mut() = Some((cut, action)));
}

#[cfg(test)]
pub(crate) fn act_once_at_resume_lifecycle_cut_for_workspace_for_test(
    workspace: WorkspaceId,
    cut: ResumeLifecycleCut,
    action: Box<dyn FnOnce() + Send>,
) {
    assert!(
        WORKSPACE_RESUME_LIFECYCLE_CUTS
            .lock()
            .unwrap()
            .insert(workspace, (cut, action))
            .is_none(),
        "workspace lifecycle cut was already armed"
    );
}

#[cfg(test)]
fn take_resume_lifecycle_cut_for_test() -> Option<(ResumeLifecycleCut, Box<dyn FnOnce() + Send>)> {
    RESUME_LIFECYCLE_CUT.with(|slot| slot.borrow_mut().take())
}

#[cfg(test)]
fn restore_resume_lifecycle_cut_for_test(
    armed: Option<(ResumeLifecycleCut, Box<dyn FnOnce() + Send>)>,
) {
    RESUME_LIFECYCLE_CUT.with(|slot| *slot.borrow_mut() = armed);
}

#[cfg(test)]
pub(crate) fn clear_resume_lifecycle_cut_for_test() {
    RESUME_LIFECYCLE_CUT.with(|slot| *slot.borrow_mut() = None);
    SAFE_GRAPH_WRITER_PROBE_ARMED.with(|armed| armed.set(false));
    SAFE_GRAPH_WRITER_PROBE_RESULT.with(|result| result.set(None));
}

#[cfg(test)]
pub(crate) fn arm_safe_graph_writer_probe_for_test() {
    SAFE_GRAPH_WRITER_PROBE_ARMED.with(|armed| armed.set(true));
    SAFE_GRAPH_WRITER_PROBE_RESULT.with(|result| result.set(None));
}

#[cfg(test)]
pub(crate) fn take_safe_graph_writer_probe_for_test() -> Option<std::io::ErrorKind> {
    SAFE_GRAPH_WRITER_PROBE_RESULT.with(|result| result.take())
}

#[cfg(test)]
fn probe_graph_writer_while_safe(graph: &Graph) {
    if !SAFE_GRAPH_WRITER_PROBE_ARMED.with(|armed| armed.replace(false)) {
        return;
    }
    let outcome = match graph.probe_managed_text_writer() {
        Ok(()) => None,
        Err(error) => Some(error.kind()),
    };
    SAFE_GRAPH_WRITER_PROBE_RESULT.with(|result| result.set(outcome));
}

#[cfg(test)]
fn resume_lifecycle_cut_reached(cut: ResumeLifecycleCut) {
    let armed = RESUME_LIFECYCLE_CUT.with(|slot| {
        let mut slot = slot.borrow_mut();
        match slot.as_ref() {
            Some((armed, _)) if *armed == cut => slot.take().map(|(_, action)| action),
            _ => None,
        }
    });
    if let Some(action) = armed {
        action();
    }
}

/// Bounded clean admission: exact causal counters over a real clean runtime.
///
/// This is the carried-forward half of the retired promoted `bounded_admission`
/// module (stage 2d wave 2). The four other tests there asserted the promoted
/// promotion/open boundary itself and died with it; this one asserts a *cost*
/// property of the keystroke path, and that property has to hold on whatever
/// runtime production actually uses. Since stage 2a that runtime is
/// [`CleanLocalRuntime`], so the claim is re-proved here against the exact
/// clean composition production performs at `sync_runtime.rs`'s clean
/// activation open.
///
/// This module owns its own compact clean-activation fixture on purpose: the
/// claim proved here is about cost accounting rather than about the activation
/// journey `import.rs` already covers.
#[cfg(test)]
mod bounded_admission {
    use super::*;
    use crate::oplog::import::{
        commit_clean_activation, open_clean_activation, prepare_clean_activation,
    };
    use crate::oplog::lazy_genesis::LAZY_GENESIS_BASELINE_DIRECTORY;
    use crate::oplog::operational_coordinator::{CleanLocalMutationState, OperationalCoordinator};
    use crate::oplog::projection_store::ProjectionReceiptStore;
    use crate::oplog::sqlite::{ProjectionClaim, WorkspaceRuntimeLease};
    use crate::oplog::{
        BlockLocation, DocumentId, LineageDigest, ObjectStore, OperationTransaction,
        ProjectionEndpointId, ReferenceCatalogPolicyV1, SemanticOperation,
    };
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "tine-bounded-clean-admission-{label}-{}",
                Uuid::new_v4()
            ));
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

    /// One complete clean activation over one real graph, composed exactly the
    /// way `sync_runtime`'s clean open composes it: streaming clean-activation
    /// preparation and commit, reopen from the published marker, clean archive
    /// store, archive-rooted workspace lease, lease-bound genesis projection,
    /// clean projection endpoint, and finally `CleanLocalRuntime`.
    struct Fixture {
        /// Retained only so the temporary tree outlives the fixture.
        _root: TestRoot,
        graph_root: PathBuf,
        graph: Graph,
        receipts: ProjectionReceiptStore,
        runtime: CleanLocalRuntime,
        seed_block: BlockLocation,
    }

    impl Fixture {
        fn new(label: &str, files: Vec<(String, Vec<u8>)>) -> Self {
            let root = TestRoot::new(label);
            let graph_root = root.path().join("graph");
            fs::create_dir(&graph_root).unwrap();
            for (path, bytes) in &files {
                let destination = graph_root.join(path);
                fs::create_dir_all(destination.parent().unwrap()).unwrap();
                fs::write(destination, bytes).unwrap();
            }
            let graph = Graph::open(&graph_root);

            let workspace = WorkspaceId::from_uuid(Uuid::from_u128(0x9200));
            let lineage = LineageDigest::of(b"bounded-clean-admission-test");
            let catalog_document_id = DocumentId::from_uuid(Uuid::from_u128(0x9201));
            let database = root.path().join("clean-projection.sqlite");
            let archive = root.path().join("clean-archive");
            let enrollment = root.path().join("clean-enrollment");
            fs::create_dir(&archive).unwrap();

            let capture_root = root.path().join("capture");
            fs::create_dir(&capture_root).unwrap();
            let capture = graph
                .capture_inactive_bootstrap_sources(&capture_root)
                .unwrap();
            let preparation = prepare_clean_activation(
                &graph,
                capture,
                workspace,
                lineage,
                catalog_document_id,
                &root.path().join("clean-preparation"),
                &database,
                &ReferenceCatalogPolicyV1::default(),
            )
            .unwrap();
            let seed_block = {
                let baseline = preparation.candidates().baseline();
                let page_id = baseline
                    .page_ids()
                    .next()
                    .expect("the clean baseline has a page");
                let page = baseline
                    .page(page_id)
                    .unwrap()
                    .expect("the clean baseline page is addressable");
                let block = page.blocks.first().expect("the seed page has one block");
                BlockLocation {
                    block_id: block.block_id,
                    home_document_id: block.home_document_id,
                }
            };
            let committed = commit_clean_activation(
                &graph,
                preparation,
                &archive.join(LAZY_GENESIS_BASELINE_DIRECTORY),
                &enrollment,
            )
            .unwrap();
            let (baseline, physical, baseline_frontier, _marker) = committed.into_parts();
            drop(physical);
            drop(baseline);

            let reopened = open_clean_activation(
                &enrollment,
                &archive.join(LAZY_GENESIS_BASELINE_DIRECTORY),
                &database,
                catalog_document_id,
                ReferenceCatalogPolicyV1::default(),
            )
            .unwrap()
            .expect("the published clean activation marker reopens");
            let (mut engine, projection, _) = reopened.into_parts();

            let operations = archive.join("operations");
            engine
                .attach_clean_archive_store(ObjectStore::open(&operations, workspace).unwrap())
                .unwrap();
            let store = ObjectStore::open(&operations, workspace).unwrap();
            let lease = WorkspaceRuntimeLease::acquire(&store, workspace).unwrap();
            let leased = LeasedWorkspaceProjection::adopt_clean_genesis(
                lease,
                &database,
                ProjectionClaim::current(workspace, lineage),
                &baseline_frontier,
                &store,
                &engine,
                projection,
            )
            .map_err(|(_, error)| error)
            .unwrap();

            let endpoint = ProjectionEndpointBinding::enroll_graph(
                &graph,
                ProjectionEndpointId::from_uuid(Uuid::from_u128(0x9202)),
                DeviceId::from_uuid(Uuid::from_u128(0x9203)),
            )
            .unwrap();
            let receipts = ProjectionReceiptStore::open_for_endpoint(
                &root.path().join("receipts"),
                workspace,
                endpoint,
            )
            .unwrap();
            engine
                .attach_clean_projection_endpoint(&graph, &receipts)
                .unwrap();

            let runtime = CleanLocalRuntime::from_open_parts(
                SessionId::from_uuid(Uuid::from_u128(0x9204)),
                endpoint,
                engine,
                leased,
            )
            .unwrap();

            Self {
                _root: root,
                graph_root,
                graph,
                receipts,
                runtime,
                seed_block,
            }
        }

        fn snapshot_graph(&self) -> BTreeMap<String, Vec<u8>> {
            snapshot_files(&self.graph_root)
        }

        /// Open exactly `count` admission windows and report the causal work
        /// they performed. Each window also authorizes once, which is what
        /// every real mutation path does before touching the engine.
        fn measure_admissions(&mut self, count: usize) -> PromotedRuntimeInstrumentation {
            let before = PromotedRuntimeInstrumentation::capture();
            for _ in 0..count {
                let session = self.runtime.admit_clean_mutation(&self.graph).unwrap();
                session
                    .admission()
                    .authorize(&self.graph, clean_engine_of(&session))
                    .unwrap();
            }
            before.since()
        }

        /// Author, commit and project one ordinary clean local batch through
        /// the production coordinator, advancing the durable manifest history.
        fn append_local_batch(&mut self, block: BlockLocation, content: &str) {
            let transaction =
                OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
                    block,
                    content: content.into(),
                }])
                .unwrap();
            let mut projection_turns =
                crate::oplog::projection_turn_journal::open_scratch_projection_turn_journal_for(
                    self.runtime.engine(),
                );
            let mut session = self.runtime.admit_clean_mutation(&self.graph).unwrap();
            let state = OperationalCoordinator::execute_clean_local(
                &mut session,
                &self.graph,
                &self.receipts,
                &transaction,
                &mut projection_turns,
            )
            .unwrap();
            match state {
                CleanLocalMutationState::Complete(_) => {}
                CleanLocalMutationState::DurablePending(pending) => {
                    panic!("clean local batch did not complete: {}", pending.failure())
                }
            }
        }
    }

    fn clean_engine_of<'a>(session: &'a CleanRuntimeSession<'_>) -> &'a ShardedHotEngine {
        session.engine
    }

    fn snapshot_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut output = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            for entry in fs::read_dir(&directory).unwrap().map(Result::unwrap) {
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

    /// The keystroke path is bounded and journal/graph-length independent.
    ///
    /// At 1, 1,000 and 10,000 admissions — and again after real committed
    /// clean manifests have advanced the durable history — one
    /// admission-plus-authorization window must perform:
    ///
    /// * zero enrollment namespace enumerations, directory-tree opens, lease
    ///   acquisitions, authority-claim rereads, head reads and record-chain
    ///   reads (the clean runtime never opens a sparse enrollment journal);
    /// * zero archive-rooted workspace lease *acquisitions* (the lease is
    ///   retained for the runtime's whole lifetime);
    /// * exactly two workspace-lease identity revalidations, one for the
    ///   admission and one for its authorization.
    ///
    /// And it writes nothing into the user's graph.
    ///
    /// Deliberately NOT asserted: `PromotedRuntimeInstrumentation`'s
    /// `sqlite_frontier_reads` and `archive_identity_reads`. Both are counted
    /// at promoted-runtime boundaries only, and after stage 2d wave 2 no path
    /// a `CleanLocalRuntime` can reach increments either, so a zero there
    /// would be vacuous rather than evidence.
    ///
    /// Fail-before is executable and in the same test: the identical counter
    /// must resolve the three boundaries apart — a window alone is 1, plus its
    /// authorization is 2, plus a mutable-parts handout is 3. An instrument
    /// that could not see the difference would pass the bound above too.
    #[test]
    fn clean_admissions_are_bounded_at_one_one_thousand_and_ten_thousand() {
        const REVALIDATIONS_PER_ADMISSION: usize = 2;
        let mut fixture = Fixture::new(
            "bounded",
            vec![("pages/seed.md".into(), b"- seed\n".to_vec())],
        );
        let bounded = |count: usize| PromotedRuntimeInstrumentation {
            enrollment: super::super::enrollment::EnrollmentInstrumentation {
                record_reads: 0,
                head_reads: 0,
                namespace_scans: 0,
                directory_opens: 0,
                lease_acquisitions: 0,
                authority_claim_reads: 0,
            },
            workspace_lease_acquisitions: 0,
            workspace_lease_identity_revalidations: count * REVALIDATIONS_PER_ADMISSION,
            ..PromotedRuntimeInstrumentation::default()
        };

        let before_admissions = fixture.snapshot_graph();
        for (label, count) in [
            ("one", 1_usize),
            ("thousand", 1_000),
            ("ten-thousand", 10_000),
        ] {
            let measured = fixture.measure_admissions(count);
            assert_eq!(
                asserted(measured),
                bounded(count),
                "{label} clean admissions were not bounded"
            );
        }
        assert_eq!(
            fixture.snapshot_graph(),
            before_admissions,
            "admitting a mutation window must not write the user's graph"
        );

        // Real committed clean manifests advance the durable history; the
        // per-admission bound is unchanged by them.
        let block = fixture.seed_block;
        for index in 0..3 {
            fixture.append_local_batch(block, &format!("bounded clean batch {index}"));
        }
        let after_history = fixture.snapshot_graph();
        assert_eq!(
            asserted(fixture.measure_admissions(1_000)),
            bounded(1_000),
            "admission cost must not depend on how many manifests the lineage has"
        );
        assert_eq!(
            fixture.snapshot_graph(),
            after_history,
            "admitting a mutation window must not write the user's graph"
        );

        // Fail-before: the same counter, resolving each boundary separately.
        let before = PromotedRuntimeInstrumentation::capture();
        drop(
            fixture
                .runtime
                .admit_clean_mutation(&fixture.graph)
                .unwrap(),
        );
        assert_eq!(
            before.since().workspace_lease_identity_revalidations,
            1,
            "an admission on its own must revalidate the lease identity exactly once"
        );

        let before = PromotedRuntimeInstrumentation::capture();
        {
            let session = fixture
                .runtime
                .admit_clean_mutation(&fixture.graph)
                .unwrap();
            session
                .admission()
                .authorize(&fixture.graph, clean_engine_of(&session))
                .unwrap();
        }
        assert_eq!(
            before.since().workspace_lease_identity_revalidations,
            REVALIDATIONS_PER_ADMISSION,
            "authorizing an open window must add exactly one revalidation"
        );

        let before = PromotedRuntimeInstrumentation::capture();
        {
            let mut session = fixture
                .runtime
                .admit_clean_mutation(&fixture.graph)
                .unwrap();
            session
                .admission()
                .authorize(&fixture.graph, clean_engine_of(&session))
                .unwrap();
            session.parts().unwrap();
        }
        assert_eq!(
            before.since().workspace_lease_identity_revalidations,
            REVALIDATIONS_PER_ADMISSION + 1,
            "vending the mutable parts must add exactly one more revalidation"
        );
    }

    /// Blank the two counters this test deliberately does not assert, so the
    /// comparison above cannot silently start depending on them.
    fn asserted(mut measured: PromotedRuntimeInstrumentation) -> PromotedRuntimeInstrumentation {
        measured.sqlite_frontier_reads = 0;
        measured.archive_identity_reads = 0;
        measured
    }
}
