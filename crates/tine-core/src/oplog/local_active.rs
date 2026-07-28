//! The two `LocalActive` runtime boundaries: first activation and fresh-process
//! reopen.
//!
//! Neither changes anything but device-local enrollment and runtime state. They
//! never write migration or projection bytes into the live graph, never enable
//! migration on ordinary startup, and never wire a watcher.
//!
//! [`LocalActiveAuthority`] is the only value in the new sparse-oplog
//! architecture that admits local mutation, projection, import, or coordinator
//! execution. It has no public constructor, no serialized form, no `Clone`, and
//! no test mint. Exactly two functions can produce one:
//!
//! * [`activate_verified_local`] performs the one-time `VerifiedLocal ->
//!   LocalActive` transition. It requires the retained
//!   [`VerifiedLocalEvidence`], the exact live retained proof set, the retained
//!   runtime components, and a fresh committed-head reopen proving the exact
//!   verification digest, session, binding, and `Unsafe`+`Idle` state.
//! * [`reopen_local_active_authority`] serves a restarted process, which has no
//!   retained evidence and no authority at all. It reconstructs the predecessor
//!   evidence from the durable, validated enrollment chain, revalidates the same
//!   complete proof set and runtime components, and mints an authority only for
//!   the exact committed session (or, from a clean `Safe` handoff, for exactly
//!   one requested new session).
//!
//! Handoff is conservative. Activation always persists `HandoffUnsafe`, so a
//! crash at any cut resumes unsafe. `Safe` may only be persisted after every
//! device-local drain is proved and revalidated; the dependency that this
//! packet cannot prove inside `tine-core` is named exactly rather than assumed.
//!
//! The graph-text watcher event queue is owned by the Tauri watcher, so
//! [`SAFE_HANDOFF_MISSING_DEPENDENCY`] blocks the production `Safe` transition
//! after every core-checkable drain has been proved.
//!
//! # Runtime promotion
//!
//! [`LocalActiveAuthority`] alone still cannot write: an inactive-bootstrap
//! archive is fenced from ordinary runtime opening, so the bootstrap's own
//! engine is read-only. [`PromotedLocalRuntime`] is the boundary that lifts that
//! fence for exactly one durably bound lineage. Promotion is two phase, because
//! the device-local SQLite applier lease is one-per-workspace and the retained
//! inactive bootstrap projection must be released before a promoted one exists:
//!
//! 1. [`seal_local_runtime_promotion`] re-proves the live authority, retained
//!    proof set, inactive accepted authority, bound archive capability and its
//!    persisted canonical resource claim, enrolled endpoint, and bootstrap
//!    SQLite projection, then publishes one immutable exact durable promotion
//!    state. Publication and its readback run on the exact retained archive
//!    capability, never on a re-resolved pathname, and an archive whose
//!    retained capability and enrolled pathname have stopped naming the same
//!    directory is refused before anything is written.
//! 2. [`open_promoted_local_runtime`] (same process) or
//!    [`reopen_promoted_local_runtime`] (restarted process, no retained
//!    evidence at all) opens and completely recovers the writable enrolled
//!    engine, projection-work index, reference/catalog authority, SQLite
//!    projection, and bounded tail, proves the current history is exactly or
//!    insertion-only descended from the exact bootstrap anchor, and only then
//!    mints the token.
//!
//! # Workspace ownership and crash takeover
//!
//! A promoted runtime holds exactly one archive-rooted
//! [`super::sqlite::WorkspaceRuntimeLease`] for its entire writable life. It is
//! taken before the runtime reads or writes any archive, engine, SQLite, or
//! enrollment-handoff state, and it is owned inseparably from the device-local
//! database it authorized (see [`super::sqlite::LeasedWorkspaceProjection`]), so
//! the database handle and the workspace authority cannot drift apart and the
//! lock is never released across a bootstrap -> promoted database handoff.
//!
//! That lease is also the crash proof. An unclean shutdown leaves
//! `HandoffUnsafe { old session }` committed, and
//! [`take_over_promoted_local_runtime`] is the only boundary that may replace it
//! with `HandoffUnsafe { new session }`. It may do so only while it owns the
//! workspace lease — an `EnrollmentLease` is device-local app data, so a process
//! under another XDG, HOME, or Flatpak root would not contend for it at all —
//! and only after it has recovered and authenticated the entire runtime the
//! crashed process left behind. The replacement is then an exact
//! compare-and-swap from the authenticated old head and old unsafe session, so
//! two racing newcomers cannot both win. It never mints `Safe` and never
//! unblocks automatic external import; only a proved clean drain does that.
//!
//! The bootstrap stays an immutable historical anchor. Ordinary local batches
//! extend it by carrying the identical bootstrap aggregate binding forward, so
//! the promoted history is one homogeneous bootstrap-anchored lineage that the
//! durable promotion state explicitly authorizes — never a silently
//! reinterpreted inactive root and never mixed record bindings. Ancestry is
//! proved by the store-minted [`super::object_store::AuthenticatedEngineHistoryTransition`],
//! never by raw generation or acceptance sequence.
//!
//! Every new-architecture mutation, projection, import, coordinator, and
//! reconciliation path requires a [`LocalRuntimeAdmission`] whose only
//! production source is [`PromotedRuntimeAdmission::admission`], which is itself
//! derived from *both* a live [`LocalActiveAuthority`] and the exact
//! [`PromotedLocalRuntime`].

use std::fmt;
use std::path::Path;

use crate::model::Graph;

use super::enrollment::{
    activate_verified_local_record, reopen_local_active_from_durable_state,
    reopen_local_active_record, reopen_promoted_bootstrap_anchor, transition_local_active_handoff,
    CommittedLocalActive, EnrollmentApplicationRoot, EnrollmentBindingV1, LocalActiveHandoff,
    LocalActiveSync, PromotedBootstrapAnchor, RetainedEnrollmentSession, UnsafeHandoffPredecessor,
    VerifiedLocalCompositionError, VerifiedLocalEvidence, VerifiedLocalProofSet,
};
use super::hot_engine::{EngineError, ShardedHotEngine};
use super::import::InactiveBootstrapAcceptedAuthority;
use super::object_store::{
    EngineHistoryAuthority, PromotedLineageModeV1, PromotedRuntimeStateV1, StoreError,
    PROMOTED_RUNTIME_STATE_SCHEMA_VERSION,
};
use super::projection_store::ProjectionReceiptStore;
use super::sqlite::{
    ApplicationRuntimeRoot, LeasedWorkspaceProjection, OpenProjection, ProjectionClaim,
    ProjectionError, RebuildSource, SqliteFrontier, TailOverlay, VerifiedBootstrapSqliteProjection,
    WorkspaceRuntimeLease,
};
use super::{ContentDigest, ObjectStore, ProjectionEndpointBinding, SessionId, WorkspaceId};

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
}

#[cfg(test)]
impl PromotedRuntimeInstrumentation {
    pub(crate) fn capture() -> Self {
        Self {
            enrollment: super::enrollment::EnrollmentInstrumentation::capture(),
            sqlite_frontier_reads: PROMOTED_SQLITE_FRONTIER_READS.with(std::cell::Cell::get),
            archive_identity_reads: PROMOTED_ARCHIVE_IDENTITY_READS.with(std::cell::Cell::get),
            workspace_lease_acquisitions: super::sqlite::workspace_runtime_lease_acquisitions(),
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
        }
    }
}

#[cfg(test)]
fn count(counter: &'static std::thread::LocalKey<std::cell::Cell<usize>>) {
    counter.with(|value| value.set(value.get().saturating_add(1)));
}

/// Read the device-local SQLite accepted frontier, counted.
fn sqlite_frontier_root(
    database: &SqliteFrontier,
) -> Result<super::hot_engine::AcceptedFrontierRoot, ProjectionError> {
    #[cfg(test)]
    count(&PROMOTED_SQLITE_FRONTIER_READS);
    database.frontier_root()
}

/// Authenticate the archive's persisted canonical resource claim and its
/// physical control-directory identity, counted.
///
/// Both operations reread or re-stat the archive through an already-open
/// immutable capability. They are stable session facts: an `ObjectStore` is a
/// retained no-follow directory capability, so the directory it names cannot be
/// swapped underneath it. Every open, handoff, and recovery boundary — and
/// every observed enrollment-head change — re-proves them; an unchanged-head
/// admission does not.
fn authenticate_archive_identity(
    archive: &ObjectStore,
    state: &PromotedRuntimeStateV1,
    claim_detail: &'static str,
    control_detail: &'static str,
) -> Result<(), RuntimePromotionError> {
    #[cfg(test)]
    count(&PROMOTED_ARCHIVE_IDENTITY_READS);
    archive
        .validate_enrolled_archive_resource_id(state.archive_resource_id)
        .map_err(|error| {
            RuntimePromotionError::Activation(LocalActivationError::RuntimeBinding(format!(
                "{claim_detail}: {error}"
            )))
        })?;
    if archive.canonical_archive_identity()?.binding_digest() != state.archive_control_binding {
        return Err(RuntimePromotionError::Anchor(control_detail));
    }
    Ok(())
}

/// Exact live runtime components retained for the whole writable session.
///
/// These are borrowed, never digests. Activation authenticates them against the
/// retained proof set and the committed enrollment binding.
pub(crate) struct LocalActiveRuntime<'a> {
    /// Authoritative engine whose accepted history this activation binds.
    pub(crate) engine: &'a ShardedHotEngine,
    /// Device-local SQLite projection retained for this session.
    pub(crate) projection: &'a OpenProjection,
}

#[derive(Debug)]
pub(crate) enum LocalActivationError {
    Enrollment(VerifiedLocalCompositionError),
    /// A live runtime component does not authenticate the committed binding.
    RuntimeBinding(String),
    /// The runtime state required to admit work is not currently present.
    NotAdmitted(&'static str),
}

impl fmt::Display for LocalActivationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enrollment(error) => error.fmt(formatter),
            Self::RuntimeBinding(detail) => {
                write!(formatter, "local-active runtime binding failed: {detail}")
            }
            Self::NotAdmitted(detail) => {
                write!(formatter, "local-active runtime is not admitted: {detail}")
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

/// Why a durable `Safe` handoff could not be minted.
#[derive(Debug)]
pub(crate) enum SafeHandoffUnavailable {
    Enrollment(VerifiedLocalCompositionError),
    Runtime(String),
    /// A named device-local drain has outstanding work.
    DrainIncomplete {
        drain: &'static str,
        detail: String,
    },
    /// Every core-checkable drain proved, but one required dependency cannot be
    /// observed from `tine-core` in this packet. `Safe` is deliberately not
    /// minted rather than assumed.
    MissingDependency(&'static str),
}

impl fmt::Display for SafeHandoffUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enrollment(error) => error.fmt(formatter),
            Self::Runtime(detail) => write!(formatter, "safe-handoff runtime error: {detail}"),
            Self::DrainIncomplete { drain, detail } => {
                write!(formatter, "{drain} drain is incomplete: {detail}")
            }
            Self::MissingDependency(dependency) => write!(
                formatter,
                "safe handoff is unavailable: {dependency} cannot be proved by tine-core yet"
            ),
        }
    }
}

impl std::error::Error for SafeHandoffUnavailable {}

impl From<VerifiedLocalCompositionError> for SafeHandoffUnavailable {
    fn from(error: VerifiedLocalCompositionError) -> Self {
        Self::Enrollment(error)
    }
}

/// The exact device-local dependency this packet cannot prove without watcher
/// ownership. It is reported verbatim instead of minting a false `Safe`.
pub(crate) const SAFE_HANDOFF_MISSING_DEPENDENCY: &str =
    "graph-text watcher event queue drain (owned by the Tauri watcher, unwired in this packet)";

/// Opaque device-local write authority for one enrolled graph.
///
/// Deliberately not `Clone`, not `Copy`, not `Debug`-transparent, and neither
/// serializable nor deserializable. Every field is private and every accessor
/// returns copied evidence, never the authority itself.
pub(crate) struct LocalActiveAuthority {
    application_root: EnrollmentApplicationRoot,
    evidence: VerifiedLocalEvidence,
    session_id: SessionId,
    enrollment_head: ContentDigest,
    verification_digest: ContentDigest,
    handoff: LocalActiveHandoff,
    endpoint: ProjectionEndpointBinding,
    activation_acceptance_sequence: u64,
    _seal: seal::Seal,
}

impl fmt::Debug for LocalActiveAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalActiveAuthority")
            .field("enrollment_head", &self.enrollment_head)
            .finish_non_exhaustive()
    }
}

impl LocalActiveAuthority {
    pub(crate) const fn enrollment_head(&self) -> ContentDigest {
        self.enrollment_head
    }

    pub(crate) const fn verification_digest(&self) -> ContentDigest {
        self.verification_digest
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn handoff(&self) -> LocalActiveHandoff {
        self.handoff
    }

    pub(crate) const fn binding(&self) -> &EnrollmentBindingV1 {
        self.evidence.binding()
    }

    pub(crate) const fn endpoint(&self) -> ProjectionEndpointBinding {
        self.endpoint
    }

    /// Admit one short-lived local mutation window.
    ///
    /// The live graph and engine must authenticate the committed enrollment
    /// binding, and the committed enrollment record must still be this exact
    /// session's `LocalActive`. A committed `Safe` handoff is durably moved back
    /// to `Unsafe { session }` *before* the permit exists, so no write is ever
    /// accepted while the persisted state claims a clean handoff.
    ///
    /// The exclusive borrow is load bearing: one authority can never hand out
    /// two live permits.
    ///
    /// This is the pre-promotion form, which owns no retained enrollment
    /// session and therefore opens the journal itself. A promoted runtime must
    /// use [`LocalActiveAuthority::reconcile_promoted_handoff`] instead: it
    /// holds the exclusive enrollment lease for its whole lifetime, so the
    /// `Safe -> Unsafe` transition below would contend with its own process.
    pub(crate) fn admit_local_mutation(
        &mut self,
        graph: &Graph,
        engine: &ShardedHotEngine,
    ) -> Result<LocalMutationPermit<'_>, LocalActivationError> {
        self.authenticate_runtime(graph, engine)?;
        let committed = self.reopen_current()?;
        match committed.handoff() {
            LocalActiveHandoff::Unsafe { session_id } if session_id == self.session_id => {
                self.adopt(committed);
            }
            LocalActiveHandoff::Unsafe { .. } => {
                return Err(LocalActivationError::Enrollment(
                    VerifiedLocalCompositionError::CompetingSession,
                ));
            }
            LocalActiveHandoff::Safe => {
                let unsafe_again = transition_local_active_handoff(
                    &self.application_root,
                    self.evidence.binding(),
                    committed.enrollment_head(),
                    self.verification_digest,
                    LocalActiveHandoff::Unsafe {
                        session_id: self.session_id,
                    },
                )?;
                self.adopt(unsafe_again);
            }
        }
        if !matches!(self.handoff, LocalActiveHandoff::Unsafe { .. }) {
            return Err(LocalActivationError::NotAdmitted(
                "durable handoff did not reach Unsafe before admission",
            ));
        }
        Ok(LocalMutationPermit {
            authority: self,
            _seal: seal::Seal,
        })
    }

    /// Settle the durable handoff for one promoted admission, on the retained
    /// enrollment session.
    ///
    /// This is [`Self::admit_local_mutation`]'s enrollment half, rewritten to
    /// borrow the session a [`PromotedLocalRuntime`] already holds. The
    /// semantics are identical and deliberately so:
    ///
    /// * the committed record must still be this exact session's `LocalActive`
    ///   for this exact verification digest and enrollment binding;
    /// * a committed `Safe` handoff is durably moved back to
    ///   `Unsafe { session }` *before* any permit exists, so no promoted write
    ///   is ever accepted while the persisted state claims a clean handoff;
    /// * a competing session fails closed without advancing anything.
    ///
    /// What changed is only the cost and the lock discipline. The session
    /// performs a cheap exact head-digest check and escalates to the complete
    /// authenticated reopen only when the committed head actually changed, and
    /// the `Safe -> Unsafe` journal mutation borrows the retained lease instead
    /// of opening a second writer that would contend with this process.
    fn reconcile_promoted_handoff(
        &mut self,
        session: &mut RetainedEnrollmentSession,
    ) -> Result<(), LocalActivationError> {
        let (handoff, head) = {
            let committed = session.revalidate()?;
            self.require_own_committed_record(committed)?;
            (committed.handoff(), committed.enrollment_head())
        };
        match handoff {
            LocalActiveHandoff::Unsafe { session_id } if session_id == self.session_id => {
                self.enrollment_head = head;
                self.handoff = handoff;
            }
            LocalActiveHandoff::Unsafe { .. } => {
                return Err(LocalActivationError::Enrollment(
                    VerifiedLocalCompositionError::CompetingSession,
                ));
            }
            LocalActiveHandoff::Safe => {
                let (handoff, head) = {
                    let unsafe_again = session.transition_handoff(LocalActiveHandoff::Unsafe {
                        session_id: self.session_id,
                    })?;
                    self.require_own_committed_record(unsafe_again)?;
                    (unsafe_again.handoff(), unsafe_again.enrollment_head())
                };
                self.enrollment_head = head;
                self.handoff = handoff;
            }
        }
        if !matches!(self.handoff, LocalActiveHandoff::Unsafe { .. }) {
            return Err(LocalActivationError::NotAdmitted(
                "durable handoff did not reach Unsafe before admission",
            ));
        }
        Ok(())
    }

    /// The committed record a retained session offers must be this exact
    /// authority's enrollment, not merely a well-formed `LocalActive` one.
    fn require_own_committed_record(
        &self,
        committed: &CommittedLocalActive,
    ) -> Result<(), LocalActivationError> {
        if committed.verification_digest() != self.verification_digest
            || committed.binding() != self.evidence.binding()
        {
            return Err(LocalActivationError::Enrollment(
                VerifiedLocalCompositionError::ProofMismatch(
                    "the retained enrollment session is not this authority's enrollment",
                ),
            ));
        }
        if committed.sync() != LocalActiveSync::Idle {
            return Err(LocalActivationError::Enrollment(
                VerifiedLocalCompositionError::WrongLifecycle(
                    "LocalActive runtime authority requires an Idle sync state",
                ),
            ));
        }
        Ok(())
    }

    /// Quiesce every device-local drain and, when all of them can be proved,
    /// persist `Safe` and return a typed safe-handoff permit.
    ///
    /// This packet cannot observe the watcher event queue, so the transition is
    /// explicitly unavailable with the exact missing dependency instead of
    /// minting a `Safe` state that is not true. Every other invariant is fully
    /// checked and revalidated after the drain.
    ///
    /// Its drain proof reads the journal but never writes it, so it is safe to
    /// call while a promoted runtime holds the retained enrollment lease. When
    /// the watcher dependency is wired and a real `Safe` record is persisted,
    /// that write must borrow the promoted runtime's retained session — the
    /// same reason [`Self::admit_local_mutation`] does not serve the promoted
    /// path.
    pub(crate) fn quiesce_and_mark_safe(
        &mut self,
        graph: &Graph,
        engine: &ShardedHotEngine,
        database: &SqliteFrontier,
        tail: &TailOverlay,
    ) -> Result<SafeHandoffPermit, SafeHandoffUnavailable> {
        self.prove_core_drains(graph, engine, database, tail)?;
        Err(SafeHandoffUnavailable::MissingDependency(
            SAFE_HANDOFF_MISSING_DEPENDENCY,
        ))
    }

    /// The real `Safe` transition, exercised with the complete production drain
    /// proof but without the one dependency `tine-core` cannot observe yet.
    ///
    /// This mints no authority: it can only run against an already committed
    /// `LocalActive` record owned by this exact session.
    #[cfg(test)]
    pub(crate) fn quiesce_and_mark_safe_without_watcher_dependency(
        &mut self,
        graph: &Graph,
        engine: &ShardedHotEngine,
        database: &SqliteFrontier,
        tail: &TailOverlay,
    ) -> Result<SafeHandoffPermit, SafeHandoffUnavailable> {
        let head = self.prove_core_drains(graph, engine, database, tail)?;
        let committed = transition_local_active_handoff(
            &self.application_root,
            self.evidence.binding(),
            head,
            self.verification_digest,
            LocalActiveHandoff::Safe,
        )?;
        if committed.handoff() != LocalActiveHandoff::Safe
            || committed.sync() != LocalActiveSync::Idle
        {
            return Err(SafeHandoffUnavailable::Runtime(
                "committed handoff state is not Safe+Idle after the transition".into(),
            ));
        }
        let permit = SafeHandoffPermit {
            enrollment_head: committed.enrollment_head(),
            verification_digest: committed.verification_digest(),
            session_id: self.session_id,
            _seal: seal::Seal,
        };
        self.adopt(committed);
        Ok(permit)
    }

    /// Prove every core-checkable drain, twice, while graph text admission is
    /// reserved. Returns the committed head the `Safe` transition may swap.
    fn prove_core_drains(
        &mut self,
        graph: &Graph,
        engine: &ShardedHotEngine,
        database: &SqliteFrontier,
        tail: &TailOverlay,
    ) -> Result<ContentDigest, SafeHandoffUnavailable> {
        self.authenticate_runtime(graph, engine)
            .map_err(|error| SafeHandoffUnavailable::Runtime(error.to_string()))?;
        let committed = self.reopen_current()?;
        match committed.handoff() {
            LocalActiveHandoff::Unsafe { session_id } if session_id == self.session_id => {}
            LocalActiveHandoff::Unsafe { .. } => {
                return Err(SafeHandoffUnavailable::Enrollment(
                    VerifiedLocalCompositionError::CompetingSession,
                ));
            }
            LocalActiveHandoff::Safe => {
                return Err(SafeHandoffUnavailable::Runtime(
                    "committed handoff is already Safe for another drain".into(),
                ));
            }
        }
        self.adopt(committed);

        // Reserving the graph handoff proves that graph text admission and every
        // managed writer lease are drained, and holds them drained across the
        // revalidation pass below.
        let reservation = graph
            .mint_handoff_safe(engine.workspace_id(), self.endpoint)
            .map_err(|error| SafeHandoffUnavailable::Runtime(error.to_string()))?;
        reservation
            .verify_binding(graph, engine.workspace_id(), self.endpoint)
            .map_err(|error| SafeHandoffUnavailable::Runtime(error.to_string()))?;

        let outcome = (|| {
            prove_device_local_drains(engine, database, tail)?;
            // Revalidate after the drain: nothing may have re-entered while the
            // reservation was taken.
            prove_device_local_drains(engine, database, tail)
        })();
        reservation.cancel();
        outcome?;

        let revalidated = self.reopen_current()?;
        if revalidated.enrollment_head() != self.enrollment_head
            || revalidated.handoff()
                != (LocalActiveHandoff::Unsafe {
                    session_id: self.session_id,
                })
            || revalidated.sync() != LocalActiveSync::Idle
        {
            return Err(SafeHandoffUnavailable::Runtime(
                "committed enrollment state changed during the drain".into(),
            ));
        }
        Ok(self.enrollment_head)
    }

    fn reopen_current(&self) -> Result<CommittedLocalActive, VerifiedLocalCompositionError> {
        let committed = super::enrollment::reopen_committed_local_active_for_session(
            &self.application_root,
            self.evidence.binding(),
            self.verification_digest,
        )?;
        if committed.sync() != LocalActiveSync::Idle {
            return Err(VerifiedLocalCompositionError::WrongLifecycle(
                "LocalActive runtime authority requires an Idle sync state",
            ));
        }
        Ok(committed)
    }

    fn adopt(&mut self, committed: CommittedLocalActive) {
        self.enrollment_head = committed.enrollment_head();
        self.handoff = committed.handoff();
    }

    /// Authenticate the live graph capability alone against the committed
    /// enrollment binding.
    fn authenticate_graph(&self, graph: &Graph) -> Result<(), LocalActivationError> {
        let binding = self.evidence.binding();
        let graph_resource = graph
            .canonical_resource_id()
            .map_err(|error| LocalActivationError::RuntimeBinding(error.to_string()))?;
        let scope = graph
            .graph_text_scope_binding()
            .map_err(|error| LocalActivationError::RuntimeBinding(error.to_string()))?;
        if graph_resource != binding.graph_resource_id() {
            return Err(LocalActivationError::RuntimeBinding(
                "live graph resource identity is not the enrolled graph".into(),
            ));
        }
        if scope != binding.graph_text_scope_binding() {
            return Err(LocalActivationError::RuntimeBinding(
                "live graph text scope is not the enrolled scope".into(),
            ));
        }
        Ok(())
    }

    /// Authenticate the live graph and engine against the committed enrollment
    /// binding. Nothing here reads SQLite rows or projection bytes.
    fn authenticate_runtime(
        &self,
        graph: &Graph,
        engine: &ShardedHotEngine,
    ) -> Result<(), LocalActivationError> {
        self.authenticate_graph(graph)?;
        let binding = self.evidence.binding();
        if engine.workspace_id() != binding.workspace_id()
            || engine.lineage_digest() != binding.lineage_digest()
            || engine.catalog_document_id() != binding.catalog_document_id()
        {
            return Err(LocalActivationError::RuntimeBinding(
                "live engine workspace lineage does not match the enrolled binding".into(),
            ));
        }
        match engine.projection_endpoint_binding() {
            Some(endpoint)
                if endpoint.endpoint_id() == binding.endpoint_id()
                    && endpoint.device_id() == binding.device_id()
                    && endpoint.graph_resource_id() == binding.graph_resource_id() => {}
            _ => {
                return Err(LocalActivationError::RuntimeBinding(
                    "live engine projection endpoint is not the enrolled endpoint".into(),
                ));
            }
        }
        if engine.projection_receipt_store_id() != Some(binding.receipt_store_id()) {
            return Err(LocalActivationError::RuntimeBinding(
                "live engine receipt store is not the enrolled receipt store".into(),
            ));
        }
        let accepted = engine
            .accepted_frontier_root()
            .map_err(|error| LocalActivationError::RuntimeBinding(error.to_string()))?;
        if accepted.acceptance_sequence() < self.activation_acceptance_sequence {
            return Err(LocalActivationError::RuntimeBinding(
                "live engine accepted frontier is behind the activated frontier".into(),
            ));
        }
        Ok(())
    }
}

fn prove_device_local_drains(
    engine: &ShardedHotEngine,
    database: &SqliteFrontier,
    tail: &TailOverlay,
) -> Result<(), SafeHandoffUnavailable> {
    if engine.has_pending_author_work() {
        return Err(SafeHandoffUnavailable::DrainIncomplete {
            drain: "authoritative engine author",
            detail: "a prepared author transaction still holds unresolved documents".into(),
        });
    }
    let accepted = engine
        .accepted_frontier_root()
        .map_err(|error| SafeHandoffUnavailable::Runtime(error.to_string()))?;
    let applied = database
        .frontier_root()
        .map_err(|error| SafeHandoffUnavailable::Runtime(error.to_string()))?;
    if applied != accepted {
        return Err(SafeHandoffUnavailable::DrainIncomplete {
            drain: "SQLite accepted frontier",
            detail: "SQLite is behind the authoritative accepted frontier".into(),
        });
    }
    let status = tail.status();
    if status.unapplied_batches != 0 || status.backpressured {
        return Err(SafeHandoffUnavailable::DrainIncomplete {
            drain: "operation tail overlay",
            detail: format!(
                "{} unapplied batches remain (backpressured: {})",
                status.unapplied_batches, status.backpressured
            ),
        });
    }
    let index = engine
        .projection_work_index()
        .map_err(|error| SafeHandoffUnavailable::Runtime(error.to_string()))?;
    let page = index
        .ready_page(None, 1)
        .map_err(|error| SafeHandoffUnavailable::Runtime(error.to_string()))?;
    if !page.work().is_empty() {
        return Err(SafeHandoffUnavailable::DrainIncomplete {
            drain: "projection work",
            detail: "ready projection work remains".into(),
        });
    }
    Ok(())
}

/// A short-lived local mutation window derived from a live authority.
///
/// It borrows the authority exclusively, cannot be cloned, and carries the
/// private seal, so no sibling module can assemble one.
pub(crate) struct LocalMutationPermit<'a> {
    authority: &'a LocalActiveAuthority,
    _seal: seal::Seal,
}

impl LocalMutationPermit<'_> {
    pub(crate) const fn session_id(&self) -> SessionId {
        self.authority.session_id
    }

    pub(crate) const fn enrollment_head(&self) -> ContentDigest {
        self.authority.enrollment_head
    }

    pub(crate) const fn endpoint(&self) -> ProjectionEndpointBinding {
        self.authority.endpoint
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

enum AdmissionProvenance<'a> {
    Promoted(&'a PromotedRuntimeAdmission<'a>),
    UnenrolledPreActivation,
}

impl LocalRuntimeAdmission<'_> {
    /// The pre-activation escape hatch retained only for the crate-private
    /// deterministic scenario corpus and the coordinator/session/import
    /// regressions that predate enrollment. Those fixtures build engines
    /// directly instead of through a bootstrap publication, so no genuine
    /// authority is constructible for them yet.
    ///
    /// Two independent fences keep it away from a user's real graph. It stays
    /// `pub(crate)` — as does [`LocalRuntimeAdmission`] itself — so app startup
    /// and Tauri cannot name or construct one, and outside this crate the only
    /// way to obtain an admission remains a live [`LocalActiveAuthority`]
    /// permit. And [`Self::authorize`] refuses outright when the engine offered
    /// is a promoted runtime, so even inside the crate this hatch can never
    /// authorize work over a promoted lineage; that requires the real
    /// authority-plus-runtime admission.
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
    ) -> Result<(), LocalActivationError> {
        match &self.provenance {
            AdmissionProvenance::Promoted(admission) => admission
                .authorize_engine(graph, engine)
                .map_err(|error| match error {
                    RuntimePromotionError::Activation(error) => error,
                    other => LocalActivationError::RuntimeBinding(other.to_string()),
                }),
            AdmissionProvenance::UnenrolledPreActivation => {
                // A promoted engine is a real activated user graph. The
                // pre-activation hatch exists only for fixtures whose engines
                // were never enrolled through a bootstrap publication, so
                // offering it a promoted runtime is always a construction
                // error, never a fallback.
                if engine.promoted_lineage().is_some() {
                    return Err(LocalActivationError::RuntimeBinding(
                        "the unenrolled pre-activation admission cannot authorize a promoted \
                         runtime engine"
                            .into(),
                    ));
                }
                Ok(())
            }
        }
    }
}

/// Proof that a durable `Safe` handoff was committed and freshly reopened.
#[derive(Debug)]
pub(crate) struct SafeHandoffPermit {
    enrollment_head: ContentDigest,
    verification_digest: ContentDigest,
    session_id: SessionId,
    _seal: seal::Seal,
}

impl SafeHandoffPermit {
    pub(crate) const fn enrollment_head(&self) -> ContentDigest {
        self.enrollment_head
    }

    pub(crate) const fn verification_digest(&self) -> ContentDigest {
        self.verification_digest
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }
}

/// The sole `VerifiedLocal -> LocalActive` runtime boundary.
///
/// It consumes the retained [`VerifiedLocalEvidence`], requires the exact live
/// retained proof set and runtime components, freshly reopens the proofs
/// immediately before the transition, persists
/// `LocalActive { handoff: Unsafe { session }, sync: Idle }` through the
/// existing enrollment record/head durability protocol, and only then proves the
/// committed head by a fresh reopen.
///
/// Repeating the call with the same evidence and session resumes idempotently.
/// A competing session, stale evidence, changed proof, changed head, or any
/// other lifecycle fails closed without advancing.
pub(crate) fn activate_verified_local(
    root: &EnrollmentApplicationRoot,
    evidence: VerifiedLocalEvidence,
    session_id: SessionId,
    proofs: &VerifiedLocalProofSet<'_>,
    runtime: &LocalActiveRuntime<'_>,
) -> Result<LocalActiveAuthority, LocalActivationError> {
    activate_with_optional_cut(root, evidence, session_id, proofs, runtime, None)
}

#[cfg(test)]
pub(crate) fn activate_verified_local_at_cut_for_test(
    root: &EnrollmentApplicationRoot,
    evidence: VerifiedLocalEvidence,
    session_id: SessionId,
    proofs: &VerifiedLocalProofSet<'_>,
    runtime: &LocalActiveRuntime<'_>,
    cut: super::enrollment::CommitCut,
) -> Result<LocalActiveAuthority, LocalActivationError> {
    activate_with_optional_cut(root, evidence, session_id, proofs, runtime, Some(cut))
}

fn activate_with_optional_cut(
    root: &EnrollmentApplicationRoot,
    evidence: VerifiedLocalEvidence,
    session_id: SessionId,
    proofs: &VerifiedLocalProofSet<'_>,
    runtime: &LocalActiveRuntime<'_>,
    #[allow(unused_variables)] cut: Option<super::enrollment::CommitCut>,
) -> Result<LocalActiveAuthority, LocalActivationError> {
    let endpoint = authenticate_activation_runtime(&evidence, proofs, runtime)?;
    let activation_acceptance_sequence = runtime
        .engine
        .accepted_frontier_root()
        .map_err(|error| LocalActivationError::RuntimeBinding(error.to_string()))?
        .acceptance_sequence();

    let committed = match cut {
        #[cfg(test)]
        Some(cut) => super::enrollment::activate_verified_local_record_at_cut_for_test(
            root, &evidence, session_id, proofs, cut,
        )?,
        #[cfg(not(test))]
        Some(_) => unreachable!("crash cuts are test-only"),
        None => activate_verified_local_record(root, &evidence, session_id, proofs)?,
    };

    // Final proof: the committed head must be exactly this session's
    // Unsafe+Idle LocalActive record for the exact verification digest.
    let reopened = reopen_local_active_record(root, &evidence, session_id, proofs)?;
    if reopened.enrollment_head() != committed.enrollment_head()
        || reopened.verification_digest() != evidence.verification_digest()
        || reopened.sync() != LocalActiveSync::Idle
        || reopened.handoff() != (LocalActiveHandoff::Unsafe { session_id })
        || reopened.binding() != evidence.binding()
    {
        return Err(LocalActivationError::Enrollment(
            VerifiedLocalCompositionError::StaleEvidence(
                "committed LocalActive head did not survive the activation reopen",
            ),
        ));
    }

    Ok(LocalActiveAuthority {
        application_root: root.clone(),
        verification_digest: reopened.verification_digest(),
        enrollment_head: reopened.enrollment_head(),
        handoff: reopened.handoff(),
        evidence,
        session_id,
        endpoint,
        activation_acceptance_sequence,
        _seal: seal::Seal,
    })
}

/// The sole fresh-process `LocalActive -> LocalActive` reopen boundary.
///
/// A restarted process has no [`VerifiedLocalEvidence`] and no
/// [`LocalActiveAuthority`]: both are process-local, unserializable, and were
/// destroyed with the previous process. This boundary reconstructs everything
/// it needs from durable, validated enrollment state plus the retained proof
/// set and live runtime components, and mints a new authority only after the
/// complete proof revalidation reproduces the exact committed verification
/// digest.
///
/// Semantics:
///
/// * A committed `Unsafe { session }` record reopens only for exactly that
///   session. Any other requested session fails closed as a competing session
///   and never advances the durable head.
/// * A committed `Safe` record is durably moved to `Unsafe { requested
///   session }` through the existing record/head protocol, and is then freshly
///   reopened before an authority exists. Every crash cut therefore retains
///   either the exact `Safe` predecessor or exactly one `Unsafe` successor for
///   the requested session, which resumes idempotently.
/// * Absent, `ShadowImport`, `VerifiedLocal`, `Blocked`, non-`Idle`, malformed,
///   cross-bound, changed-digest, and invalid-chain state all fail closed.
///
/// No live graph bytes are written and no enrollment record is written on the
/// `Unsafe` resume path.
pub(crate) fn reopen_local_active_authority(
    root: &EnrollmentApplicationRoot,
    binding: &EnrollmentBindingV1,
    session_id: SessionId,
    proofs: &VerifiedLocalProofSet<'_>,
    runtime: &LocalActiveRuntime<'_>,
) -> Result<LocalActiveAuthority, LocalActivationError> {
    reopen_with_optional_cut(root, binding, session_id, proofs, runtime, None)
}

#[cfg(test)]
pub(crate) fn reopen_local_active_authority_at_cut_for_test(
    root: &EnrollmentApplicationRoot,
    binding: &EnrollmentBindingV1,
    session_id: SessionId,
    proofs: &VerifiedLocalProofSet<'_>,
    runtime: &LocalActiveRuntime<'_>,
    cut: super::enrollment::CommitCut,
) -> Result<LocalActiveAuthority, LocalActivationError> {
    reopen_with_optional_cut(root, binding, session_id, proofs, runtime, Some(cut))
}

fn reopen_with_optional_cut(
    root: &EnrollmentApplicationRoot,
    binding: &EnrollmentBindingV1,
    session_id: SessionId,
    proofs: &VerifiedLocalProofSet<'_>,
    runtime: &LocalActiveRuntime<'_>,
    #[allow(unused_variables)] cut: Option<super::enrollment::CommitCut>,
) -> Result<LocalActiveAuthority, LocalActivationError> {
    let (evidence, committed) =
        reopen_local_active_from_durable_state(root, binding, proofs)?.into_parts();

    // The retained proofs and the live runtime components must authenticate the
    // reconstructed predecessor evidence before any durable transition.
    let endpoint = authenticate_activation_runtime(&evidence, proofs, runtime)?;

    let reopened = match committed.handoff() {
        LocalActiveHandoff::Unsafe {
            session_id: committed_session,
        } if committed_session == session_id => committed,
        LocalActiveHandoff::Unsafe { .. } => {
            return Err(LocalActivationError::Enrollment(
                VerifiedLocalCompositionError::CompetingSession,
            ));
        }
        LocalActiveHandoff::Safe => {
            let expected_head = committed.enrollment_head();
            match cut {
                #[cfg(test)]
                Some(cut) => {
                    super::enrollment::transition_local_active_handoff_at_cut_for_test(
                        root,
                        binding,
                        expected_head,
                        evidence.verification_digest(),
                        LocalActiveHandoff::Unsafe { session_id },
                        cut,
                    )?;
                }
                #[cfg(not(test))]
                Some(_) => unreachable!("crash cuts are test-only"),
                None => {
                    transition_local_active_handoff(
                        root,
                        binding,
                        expected_head,
                        evidence.verification_digest(),
                        LocalActiveHandoff::Unsafe { session_id },
                    )?;
                }
            }
            // Prove the new durable state exactly as a fresh process would,
            // including the complete proof revalidation, before any authority
            // exists.
            let (fresh_evidence, fresh_committed) =
                reopen_local_active_from_durable_state(root, binding, proofs)?.into_parts();
            if fresh_evidence.enrollment_head() != evidence.enrollment_head()
                || fresh_evidence.verification_digest() != evidence.verification_digest()
                || fresh_evidence.preparation_id() != evidence.preparation_id()
                || fresh_evidence.binding() != evidence.binding()
            {
                return Err(LocalActivationError::Enrollment(
                    VerifiedLocalCompositionError::StaleEvidence(
                        "VerifiedLocal predecessor changed during the handoff transition",
                    ),
                ));
            }
            fresh_committed
        }
    };

    if reopened.verification_digest() != evidence.verification_digest()
        || reopened.sync() != LocalActiveSync::Idle
        || reopened.handoff() != (LocalActiveHandoff::Unsafe { session_id })
        || reopened.binding() != evidence.binding()
    {
        return Err(LocalActivationError::Enrollment(
            VerifiedLocalCompositionError::StaleEvidence(
                "reopened LocalActive head is not this session's Unsafe+Idle record",
            ),
        ));
    }

    let activation_acceptance_sequence = runtime
        .engine
        .accepted_frontier_root()
        .map_err(|error| LocalActivationError::RuntimeBinding(error.to_string()))?
        .acceptance_sequence();

    Ok(LocalActiveAuthority {
        application_root: root.clone(),
        verification_digest: reopened.verification_digest(),
        enrollment_head: reopened.enrollment_head(),
        handoff: reopened.handoff(),
        evidence,
        session_id,
        endpoint,
        activation_acceptance_sequence,
        _seal: seal::Seal,
    })
}

/// Authenticate the retained runtime components against the retained proofs and
/// the committed enrollment binding, and return the enrolled endpoint.
fn authenticate_activation_runtime(
    evidence: &VerifiedLocalEvidence,
    proofs: &VerifiedLocalProofSet<'_>,
    runtime: &LocalActiveRuntime<'_>,
) -> Result<ProjectionEndpointBinding, LocalActivationError> {
    let binding = evidence.binding();
    let accepted = proofs.accepted_authority.binding();
    let endpoint = accepted.storage_binding().endpoint;
    if endpoint.endpoint_id() != binding.endpoint_id()
        || endpoint.device_id() != binding.device_id()
        || endpoint.graph_resource_id() != binding.graph_resource_id()
        || accepted.storage_binding().receipt_store_id != binding.receipt_store_id()
    {
        return Err(LocalActivationError::RuntimeBinding(
            "retained accepted authority endpoint is not the enrolled endpoint".into(),
        ));
    }

    // The retained runtime engine must be the exact accepted-authority engine
    // instance, proved by its process-local engine identity rather than by any
    // reconstructible bytes.
    if !runtime.engine.runtime_authority().matches(
        proofs
            .accepted_authority
            .accepted_engine()
            .runtime_authority(),
    ) {
        return Err(LocalActivationError::RuntimeBinding(
            "retained runtime engine is not the retained accepted-authority engine".into(),
        ));
    }
    if runtime.engine.workspace_id() != binding.workspace_id()
        || runtime.engine.lineage_digest() != binding.lineage_digest()
        || runtime.engine.catalog_document_id() != binding.catalog_document_id()
    {
        return Err(LocalActivationError::RuntimeBinding(
            "retained runtime engine does not authenticate the enrolled binding".into(),
        ));
    }

    let engine_frontier = runtime
        .engine
        .accepted_frontier_root()
        .map_err(|error| LocalActivationError::RuntimeBinding(error.to_string()))?;
    if engine_frontier.state_digest() != evidence.accepted_frontier_state_digest() {
        return Err(LocalActivationError::RuntimeBinding(
            "retained runtime engine frontier is not the verified accepted frontier".into(),
        ));
    }

    // The retained SQLite projection must be the exact device-local database the
    // verified proof authenticated, and it must already be at the accepted
    // frontier. Activation never authorizes from projection rows.
    if runtime.projection.database.path() != proofs.sqlite.database.path() {
        return Err(LocalActivationError::RuntimeBinding(
            "retained runtime projection is not the verified device-local database".into(),
        ));
    }
    let applied = runtime
        .projection
        .database
        .frontier_root()
        .map_err(|error| LocalActivationError::RuntimeBinding(error.to_string()))?;
    if applied != engine_frontier || &applied != proofs.sqlite_projection.frontier_root() {
        return Err(LocalActivationError::RuntimeBinding(
            "retained runtime projection is not at the verified accepted frontier".into(),
        ));
    }
    Ok(endpoint)
}

// ---------------------------------------------------------------------------
// Runtime promotion: inactive bootstrap archive -> writable promoted runtime.
// ---------------------------------------------------------------------------

/// Why a runtime promotion, promoted open, or promoted admission failed.
#[derive(Debug)]
pub(crate) enum RuntimePromotionError {
    Activation(LocalActivationError),
    Enrollment(VerifiedLocalCompositionError),
    Store(StoreError),
    Engine(EngineError),
    Sqlite(ProjectionError),
    /// The durable promotion state, bootstrap anchor, or authenticated history
    /// transition does not authenticate the live runtime.
    Anchor(&'static str),
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
        }
    }
}

impl std::error::Error for RuntimePromotionError {}

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

/// Proof that exactly one promoted-runtime state is durably committed for one
/// enrolled archive.
///
/// It carries no capability: it is the durable state plus the private seal, and
/// exists so the two-phase promotion below cannot be entered from anywhere but
/// [`seal_local_runtime_promotion`]. Two phases are required, not stylistic: the
/// device-local SQLite applier lease is one-per-workspace, so the retained
/// inactive bootstrap projection inside the proof set must be released before a
/// promoted projection can be opened over the same archive.
pub(crate) struct SealedRuntimePromotion {
    state: PromotedRuntimeStateV1,
    _seal: seal::Seal,
}

impl fmt::Debug for SealedRuntimePromotion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedRuntimePromotion")
            .field("anchor", &self.state.anchor_authority())
            .finish_non_exhaustive()
    }
}

/// The device-local resources a promoted runtime is opened over.
pub(crate) struct PromotedRuntimeOpen<'a> {
    /// The live enrolled graph capability.
    pub(crate) graph: &'a Graph,
    /// The enrolled projection receipt namespace.
    pub(crate) receipts: &'a ProjectionReceiptStore,
    /// Root of the enrolled canonical archive.
    pub(crate) archive_root: &'a Path,
    /// Device-local disposable SQLite projection path.
    pub(crate) database_path: &'a Path,
    pub(crate) application_runtime_root: &'a ApplicationRuntimeRoot,
}

/// Phase one of promotion: durably bind the promoted runtime.
///
/// Consumes nothing and writes nothing but one device-local archive metadata
/// file. It requires, and independently re-proves, the exact live
/// [`LocalActiveAuthority`], the exact retained [`VerifiedLocalProofSet`] and
/// inactive accepted authority, the bound archive capability and its persisted
/// canonical resource claim, the enrolled endpoint/receipt store, and the
/// retained bootstrap SQLite projection at the verified accepted frontier.
///
/// The durable state binds the canonical archive resource and projection
/// storage binding, the bootstrap aggregate/publication/import identity, the
/// authenticated bootstrap history generation and radix index root, the
/// accepted frontier state digest and acceptance sequence, the `LocalActive`
/// verification digest and enrollment binding digest, the promoting session,
/// the promotion schema version, and the explicit homogeneous
/// bootstrap-anchored lineage mode.
///
/// It is published as one immutable exact file, so every crash cut reopens as
/// either the unchanged inactive bootstrap or the one exact resumable promoted
/// state. Repeating the call with the same inputs resumes; any divergent
/// competing promotion fails closed and preserves the committed state.
pub(crate) fn seal_local_runtime_promotion(
    authority: &LocalActiveAuthority,
    proofs: &VerifiedLocalProofSet<'_>,
    runtime: &LocalActiveRuntime<'_>,
) -> Result<SealedRuntimePromotion, RuntimePromotionError> {
    // The retained runtime must be the exact retained accepted-authority engine
    // and the exact verified device-local projection at the verified frontier.
    authenticate_activation_runtime(&authority.evidence, proofs, runtime)?;
    // The live graph must still authenticate the committed binding. The
    // retained engine here is the read-only inactive bootstrap engine, whose
    // enrolled endpoint lives on the accepted-authority binding rather than on
    // the engine, so `authenticate_activation_runtime` above owns that half.
    authority.authenticate_graph(proofs.graph)?;

    let binding = authority.evidence.binding();
    let committed = authority.reopen_current()?;
    match committed.handoff() {
        LocalActiveHandoff::Unsafe { session_id } if session_id == authority.session_id => {}
        LocalActiveHandoff::Unsafe { .. } => {
            return Err(RuntimePromotionError::Enrollment(
                VerifiedLocalCompositionError::CompetingSession,
            ));
        }
        LocalActiveHandoff::Safe => {
            return Err(RuntimePromotionError::Enrollment(
                VerifiedLocalCompositionError::WrongLifecycle(
                    "runtime promotion requires this session's Unsafe LocalActive record",
                ),
            ));
        }
    }
    if committed.enrollment_head() != authority.enrollment_head
        || committed.verification_digest() != authority.verification_digest
        || committed.binding() != binding
    {
        return Err(RuntimePromotionError::Enrollment(
            VerifiedLocalCompositionError::StaleEvidence(
                "committed LocalActive record changed before runtime promotion",
            ),
        ));
    }

    // The physical archive only proves its own control identity, so the
    // persisted canonical archive-resource claim is authenticated separately.
    let accepted = proofs.accepted_authority.binding();
    let archive = proofs.accepted_authority.store();
    archive
        .validate_enrolled_archive_resource_id(binding.archive_resource_id())
        .map_err(|error| {
            RuntimePromotionError::Activation(LocalActivationError::RuntimeBinding(format!(
                "persisted archive resource claim does not authenticate the enrolled binding: {error}"
            )))
        })?;
    // Publication below runs on this exact retained capability, so a renamed
    // archive can never hand its promotion state to whatever now sits at the
    // old pathname. Promotion additionally refuses an *ambiguous* archive: if
    // the retained capability and the enrolled pathname have stopped naming the
    // same directory, two directories both answer to "the enrolled archive",
    // and the one-shot durable publication must fail closed before it writes
    // rather than silently pick one of them.
    archive
        .authenticate_unambiguous_archive_pathname()
        .map_err(|error| {
            RuntimePromotionError::Activation(LocalActivationError::RuntimeBinding(format!(
                "enrolled archive is ambiguous, so runtime promotion cannot publish: {error}"
            )))
        })?;

    let state = PromotedRuntimeStateV1 {
        schema_version: PROMOTED_RUNTIME_STATE_SCHEMA_VERSION,
        lineage_mode: PromotedLineageModeV1::BootstrapAnchoredHomogeneous,
        workspace_id: binding.workspace_id(),
        lineage_digest: binding.lineage_digest(),
        catalog_document_id: binding.catalog_document_id(),
        endpoint_id: binding.endpoint_id(),
        device_id: binding.device_id(),
        graph_resource_id: binding.graph_resource_id(),
        receipt_store_id: binding.receipt_store_id(),
        archive_resource_id: binding.archive_resource_id(),
        archive_control_binding: accepted.archive_identity().binding_digest(),
        bootstrap: accepted.bootstrap_binding(),
        bootstrap_import_id: accepted.import_id(),
        anchor_history_generation: accepted.history_generation(),
        anchor_history_index_root: accepted.history_root(),
        anchor_acceptance_sequence: accepted.accepted_frontier().acceptance_sequence(),
        anchor_accepted_frontier_state_digest: accepted.accepted_frontier().state_digest(),
        enrollment_verification_digest: authority.verification_digest,
        enrollment_binding_digest: binding
            .binding_digest()
            .map_err(VerifiedLocalCompositionError::Enrollment)?,
        promotion_session_id: authority.session_id,
    };
    // The composed state must agree with the anchor the committed VerifiedLocal
    // record binds, not merely with the live retained authority.
    if state.anchor_accepted_frontier_state_digest
        != authority.evidence.accepted_frontier_state_digest()
    {
        return Err(RuntimePromotionError::Anchor(
            "retained accepted frontier is not the verified LocalActive frontier",
        ));
    }

    publish_promotion_state(archive, &state)?;

    // Fresh reopen: the committed state must be exactly this one, and the
    // enrollment head must not have moved while it was published. "Fresh" means
    // a fresh durable-history open over the *same* retained archive capability,
    // never a fresh ambient pathname open.
    let reopened = read_promotion_state(archive, &state)?;
    if reopened != state {
        return Err(RuntimePromotionError::Anchor(
            "committed promoted runtime state is not the state this promotion composed",
        ));
    }
    let after = authority.reopen_current()?;
    if after.enrollment_head() != committed.enrollment_head()
        || after.handoff() != committed.handoff()
    {
        return Err(RuntimePromotionError::Enrollment(
            VerifiedLocalCompositionError::StaleEvidence(
                "committed LocalActive record changed during runtime promotion",
            ),
        ));
    }
    Ok(SealedRuntimePromotion {
        state,
        _seal: seal::Seal,
    })
}

/// Open the durable history control alone, on the exact retained archive
/// capability, and publish the promotion state.
///
/// `archive` is the capability the [`VerifiedLocalProofSet`] already
/// authenticated. It is duplicated from its retained no-follow directory rather
/// than reopened from a pathname, because `seal_history_only` consumes a store
/// value: an ambient reopen here would let a look-alike directory that appeared
/// at the archive's old pathname receive this archive's promotion state.
fn publish_promotion_state(
    archive: &ObjectStore,
    state: &PromotedRuntimeStateV1,
) -> Result<(), RuntimePromotionError> {
    let (_store, history) = open_retained_history_control(archive, state)?;
    history.publish_promoted_runtime_state(state)?;
    Ok(())
}

/// Freshly reopen the durable promotion state through a fresh durable-history
/// open over the same exact retained archive capability.
fn read_promotion_state(
    archive: &ObjectStore,
    expected: &PromotedRuntimeStateV1,
) -> Result<PromotedRuntimeStateV1, RuntimePromotionError> {
    let (_store, history) = open_retained_history_control(archive, expected)?;
    history
        .read_promoted_runtime_state()?
        .ok_or(RuntimePromotionError::Store(
            StoreError::PromotedRuntimeStateAbsent,
        ))
}

/// Seal one promoted-storage-bound durable history control over a duplicate of
/// `archive`'s retained capability.
fn open_retained_history_control(
    archive: &ObjectStore,
    state: &PromotedRuntimeStateV1,
) -> Result<(ObjectStore, super::object_store::DurableEngineHistoryStore), RuntimePromotionError> {
    let store = archive.duplicate_retained_capability()?;
    let open = store
        .seal_history_only(promoted_storage_binding(state))
        .map_err(|(_store, error)| error)?;
    open.into_history()
        .map_err(|(_store, error)| RuntimePromotionError::Store(error))
}

fn promoted_storage_binding(
    state: &PromotedRuntimeStateV1,
) -> super::hot_engine::ProjectionStorageBinding {
    super::hot_engine::ProjectionStorageBinding {
        endpoint: ProjectionEndpointBinding {
            endpoint_id: state.endpoint_id,
            device_id: state.device_id,
            graph_resource_id: state.graph_resource_id,
        },
        receipt_store_id: state.receipt_store_id,
    }
}

/// The opaque promoted local runtime.
///
/// It owns the writable enrolled engine, the device-local SQLite projection,
/// and the bounded tail overlay for one enrolled graph. It is not `Clone`, not
/// `Copy`, neither serializable nor deserializable, has no public constructor,
/// and no test mint. Exactly two functions produce one:
/// [`open_promoted_local_runtime`] after a same-process promotion, and
/// [`reopen_promoted_local_runtime`] for a restarted process holding no
/// retained evidence at all.
pub(crate) struct PromotedLocalRuntime {
    state: PromotedRuntimeStateV1,
    anchor: EngineHistoryAuthority,
    session_id: SessionId,
    verification_digest: ContentDigest,
    endpoint: ProjectionEndpointBinding,
    /// The one enrollment journal capability this runtime owns for its whole
    /// lifetime. It holds the exclusive enrollment lease, so every journal
    /// mutation in the promoted path borrows it rather than opening a second
    /// writer, and per-mutation admission gets its cheap exact head check from
    /// it instead of reopening the journal.
    ///
    /// No caller-supplied head snapshot can become authority: the session is
    /// minted here, from the durable journal, and its committed record is the
    /// only enrollment fact the admission path reads.
    enrollment: RetainedEnrollmentSession,
    /// The session binding generation at which the archive's canonical
    /// resource claim and physical control-directory identity were last
    /// authenticated. Any change forces the full archive proof again.
    archive_authenticated_generation: u64,
    /// How this runtime came to own the enrollment's handoff state. It decides
    /// whether automatic external import may run at all.
    recovery: RuntimeRecoveryState,
    engine: ShardedHotEngine,
    /// The device-local SQLite projection *and* the archive-rooted workspace
    /// runtime lease that authorized opening it, owned as one inseparable
    /// value. The lease is taken once, before this runtime may mutate enrollment
    /// handoff state or admit a graph mutation, and is released only when this
    /// runtime drops.
    projection: LeasedWorkspaceProjection,
    tail: TailOverlay,
    _seal: seal::Seal,
}

/// How one promoted runtime came to own its enrollment's handoff state.
///
/// This is recovery *evidence*, not authority: it is derived from the durable
/// record the runtime authenticated, and its only effect is to keep automatic
/// external import fenced until a genuinely clean `Safe` handoff has been
/// observed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeRecoveryState {
    /// The promoting process opened the runtime it had just activated.
    FirstPromotion,
    /// A restart reopened this exact session's committed `Unsafe` record, which
    /// is what an unclean shutdown always leaves behind.
    ResumedOwnUnsafe,
    /// A restart adopted a committed `Safe` handoff, which is only ever written
    /// after the complete device-local drain proof.
    AdoptedSafeHandoff,
    /// A new process took over another session's committed `Unsafe` record after
    /// proving, by owning the archive-rooted workspace runtime lease, that the
    /// previous process is gone.
    TookOverCrashedUnsafe { previous_session: SessionId },
}

/// Whether this runtime may run automatic external Markdown/Org import.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExternalImportAdmission {
    Allowed,
    Blocked(&'static str),
}

impl RuntimeRecoveryState {
    /// Automatic external import stays blocked until a clean `Safe` handoff has
    /// been observed.
    ///
    /// A crashed predecessor left an unproved drain: its graph text, watcher
    /// obligations, and projection work were never quiesced, so external files
    /// cannot be assumed to be a faithful projection of the accepted frontier.
    /// Recovering from that state must never look like a clean handoff, and it
    /// must never import external Markdown merely because the previous process
    /// died.
    pub(crate) const fn automatic_external_import(self) -> ExternalImportAdmission {
        match self {
            Self::AdoptedSafeHandoff => ExternalImportAdmission::Allowed,
            Self::FirstPromotion => ExternalImportAdmission::Blocked(
                "a freshly promoted runtime has not completed a clean Safe handoff yet",
            ),
            Self::ResumedOwnUnsafe => ExternalImportAdmission::Blocked(
                "the previous run of this session ended without a proved drain (Unsafe handoff)",
            ),
            Self::TookOverCrashedUnsafe { .. } => ExternalImportAdmission::Blocked(
                "this runtime took over a crashed session's Unsafe handoff, whose drain was \
                 never proved",
            ),
        }
    }
}

mod promoted_workspace {
    /// Sealed: the workspace-authority protocol is a closed set of exactly two
    /// shapes, and both live in this file.
    pub(crate) trait Sealed {}
}

/// The archive-rooted workspace authority one promoted open runs under, and
/// what a refused open owes the caller.
///
/// A promoted runtime holds exactly one of these leases for its entire writable
/// life. It is either taken inside the open ([`AcquireWorkspaceLease`] — a fresh
/// or restarted process, which retains nothing) or handed over by a caller that
/// already holds it ([`RetainedWorkspaceLease`] — the bootstrap -> promoted
/// database handoff, where releasing it even for an instant would let another
/// process claim the archive mid-promotion).
///
/// The two differ in their *failure* type, which is why this is a trait rather
/// than an enum. An acquiring open owns the lease it took, so a failure just
/// releases it and the caller keeps ordinary `Result<_, RuntimePromotionError>`
/// ergonomics, `?` included. A retained open does not own the lease, and
/// [`seal_local_runtime_promotion`] has already durably published the promotion
/// state by the time it runs — so a failure that silently released the archive
/// would hand it to another process at precisely the moment this one must keep
/// holding it. Its failure type therefore carries the lease itself.
pub(crate) trait PromotedWorkspaceAuthority: promoted_workspace::Sealed + Sized {
    /// What a refused promotion hands back.
    type Refusal;
    /// What survives the lease handover and decides what a later failure does
    /// with the lease.
    type Custody: PromotedWorkspaceCustody<Refusal = Self::Refusal>;

    /// Refuse before any lease has been taken or handed over.
    fn refuse(self, error: RuntimePromotionError) -> Self::Refusal;

    /// Yield the archive-rooted lease for this exact archive and workspace.
    fn into_lease(
        self,
        archive: &ObjectStore,
        workspace_id: WorkspaceId,
    ) -> Result<(WorkspaceRuntimeLease, Self::Custody), Self::Refusal>;
}

/// Refuse once the lease exists, handing it to whoever owns it.
pub(crate) trait PromotedWorkspaceCustody: promoted_workspace::Sealed {
    type Refusal;

    fn refuse_returning(
        self,
        lease: WorkspaceRuntimeLease,
        error: RuntimePromotionError,
    ) -> Self::Refusal;
}

/// Acquire the archive-rooted workspace runtime lease inside the open.
pub(crate) struct AcquireWorkspaceLease;

/// A lease this open acquired for itself: a failure releases it, exactly as it
/// always has.
pub(crate) struct ReleaseOwnLease;

impl promoted_workspace::Sealed for AcquireWorkspaceLease {}
impl promoted_workspace::Sealed for ReleaseOwnLease {}

impl PromotedWorkspaceAuthority for AcquireWorkspaceLease {
    type Refusal = RuntimePromotionError;
    type Custody = ReleaseOwnLease;

    fn refuse(self, error: RuntimePromotionError) -> Self::Refusal {
        error
    }

    fn into_lease(
        self,
        archive: &ObjectStore,
        workspace_id: WorkspaceId,
    ) -> Result<(WorkspaceRuntimeLease, Self::Custody), Self::Refusal> {
        Ok((
            WorkspaceRuntimeLease::acquire(archive, workspace_id)?,
            ReleaseOwnLease,
        ))
    }
}

impl PromotedWorkspaceCustody for ReleaseOwnLease {
    type Refusal = RuntimePromotionError;

    fn refuse_returning(
        self,
        lease: WorkspaceRuntimeLease,
        error: RuntimePromotionError,
    ) -> Self::Refusal {
        drop(lease);
        error
    }
}

/// Adopt the caller's already-held lease, which must be this exact archive's
/// and this exact workspace's.
pub(crate) struct RetainedWorkspaceLease(WorkspaceRuntimeLease);

/// A lease this open only borrowed ownership of: every failure hands the exact
/// same lease back.
pub(crate) struct ReturnRetainedLease;

impl promoted_workspace::Sealed for RetainedWorkspaceLease {}
impl promoted_workspace::Sealed for ReturnRetainedLease {}

impl RetainedWorkspaceLease {
    pub(crate) const fn new(lease: WorkspaceRuntimeLease) -> Self {
        Self(lease)
    }
}

impl PromotedWorkspaceAuthority for RetainedWorkspaceLease {
    type Refusal = RetainedPromotionRefusal;
    type Custody = ReturnRetainedLease;

    fn refuse(self, error: RuntimePromotionError) -> Self::Refusal {
        RetainedPromotionRefusal {
            lease: self.0,
            error,
        }
    }

    fn into_lease(
        self,
        archive: &ObjectStore,
        workspace_id: WorkspaceId,
    ) -> Result<(WorkspaceRuntimeLease, Self::Custody), Self::Refusal> {
        let authorized = self.0.proof().authorize_archive(archive, workspace_id);
        match authorized {
            Ok(()) => Ok((self.0, ReturnRetainedLease)),
            Err(error) => Err(RetainedPromotionRefusal {
                lease: self.0,
                error: error.into(),
            }),
        }
    }
}

impl PromotedWorkspaceCustody for ReturnRetainedLease {
    type Refusal = RetainedPromotionRefusal;

    fn refuse_returning(
        self,
        lease: WorkspaceRuntimeLease,
        error: RuntimePromotionError,
    ) -> Self::Refusal {
        RetainedPromotionRefusal { lease, error }
    }
}

/// A refused promotion that was running under a caller-retained workspace
/// lease, carrying that exact lease back.
///
/// The type is the guarantee. [`Self::into_parts`] is the *only* way to reach
/// the error, so a caller cannot learn why the promotion failed while silently
/// letting the archive go; there is deliberately no
/// `From<RetainedPromotionRefusal> for RuntimePromotionError`, so `?` cannot
/// perform that conversion either; and `#[must_use]` catches a discarded
/// result. Releasing the archive after a failed retained promotion remains
/// possible — the lease has to be droppable — but it can only happen where
/// someone wrote the drop.
#[must_use = "a refused retained promotion still owns the caller's workspace lease"]
pub(crate) struct RetainedPromotionRefusal {
    lease: WorkspaceRuntimeLease,
    error: RuntimePromotionError,
}

impl RetainedPromotionRefusal {
    /// Take back the exact lease that was lent, together with the reason.
    pub(crate) fn into_parts(self) -> (WorkspaceRuntimeLease, RuntimePromotionError) {
        (self.lease, self.error)
    }
}

impl fmt::Debug for RetainedPromotionRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedPromotionRefusal")
            .field("error", &self.error)
            .field("returned_lease", &"<retained by the caller>")
            .finish()
    }
}

/// How much of the promoted binding one proof re-derives from durable state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BindingProofDepth {
    /// Per-mutation admission. The retained enrollment session performs its
    /// cheap exact head-digest check, the archive identity facts stay the ones
    /// authenticated at the current binding generation, and the device-local
    /// SQLite projection is not queried at all. Any observed head change
    /// escalates to `Boundary` before anything is admitted.
    Admission,
    /// Open, handoff, and recovery boundaries. The enrollment journal is
    /// completely reauthenticated, the archive claim and control identity are
    /// reread, and the device-local SQLite frontier is proved against the
    /// current accepted frontier.
    Boundary,
}

impl fmt::Debug for PromotedLocalRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromotedLocalRuntime")
            .field("anchor", &self.anchor)
            .finish_non_exhaustive()
    }
}

impl PromotedLocalRuntime {
    pub(crate) const fn engine(&self) -> &ShardedHotEngine {
        &self.engine
    }

    pub(crate) const fn database(&self) -> &SqliteFrontier {
        self.projection.database()
    }

    pub(crate) const fn projection(&self) -> &OpenProjection {
        self.projection.projection()
    }

    /// How this runtime came to own the enrollment's handoff state.
    pub(crate) const fn recovery(&self) -> RuntimeRecoveryState {
        self.recovery
    }

    /// Whether automatic external Markdown/Org import may run under this
    /// runtime. It stays blocked until a clean `Safe` handoff is observed.
    pub(crate) const fn automatic_external_import(&self) -> ExternalImportAdmission {
        self.recovery.automatic_external_import()
    }

    pub(crate) const fn tail(&self) -> &TailOverlay {
        &self.tail
    }

    pub(crate) const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) const fn endpoint(&self) -> ProjectionEndpointBinding {
        self.endpoint
    }

    /// The authenticated bootstrap anchor every promoted history must descend
    /// from. Reported for instrumentation and tests, never as authority.
    pub(crate) const fn bootstrap_anchor(&self) -> EngineHistoryAuthority {
        self.anchor
    }

    pub(crate) const fn verification_digest(&self) -> ContentDigest {
        self.verification_digest
    }

    /// Admit one short-lived promoted mutation window.
    ///
    /// The window is derived from *both* the live [`LocalActiveAuthority`] and
    /// this exact promoted runtime. Both exclusive borrows are load bearing: no
    /// second window can exist while this one is live, and the complete
    /// promoted binding is revalidated before the window opens.
    pub(crate) fn admit_promoted_mutation<'a>(
        &'a mut self,
        authority: &'a mut LocalActiveAuthority,
        graph: &Graph,
    ) -> Result<PromotedRuntimeSession<'a>, RuntimePromotionError> {
        self.admit_at_depth(authority, graph, BindingProofDepth::Admission)
    }

    fn admit_at_depth<'a>(
        &'a mut self,
        authority: &'a mut LocalActiveAuthority,
        graph: &Graph,
        depth: BindingProofDepth,
    ) -> Result<PromotedRuntimeSession<'a>, RuntimePromotionError> {
        // The live authority must be this promoted runtime's session before any
        // durable state is read or touched at all.
        if authority.session_id != self.session_id
            || authority.verification_digest != self.verification_digest
        {
            return Err(RuntimePromotionError::Anchor(
                "live authority is not this promoted runtime's session",
            ));
        }
        // Live graph capability and enrolled engine binding, before any journal
        // state is settled. A foreign graph or engine is refused here.
        authority.authenticate_runtime(graph, &self.engine)?;
        // Enrollment: the cheap exact head check, escalating to the complete
        // authenticated reopen on any observed change, plus the durable
        // `Safe -> Unsafe { session }` move — all on the retained session, so
        // the exclusive lease is never reacquired and never self-contended.
        authority.reconcile_promoted_handoff(&mut self.enrollment)?;
        // The complete promoted binding, over the settled enrollment record.
        self.prove_binding(graph, authority, depth)?;

        let Self {
            state,
            anchor,
            enrollment,
            engine,
            projection,
            tail,
            ..
        } = self;
        // Every enrollment invariant `admit_local_mutation` proves has just been
        // proved by `reconcile_promoted_handoff` on the retained session, and
        // this permit cannot be constructed anywhere outside this module.
        let permit = LocalMutationPermit {
            authority: &*authority,
            _seal: seal::Seal,
        };
        let admission = PromotedRuntimeAdmission {
            permit,
            state: state.clone(),
            anchor: *anchor,
            engine_authority: engine.runtime_authority().clone(),
            binding_generation: enrollment.binding_generation(),
            enrollment: &*enrollment,
            _seal: seal::Seal,
        };
        Ok(PromotedRuntimeSession {
            admission,
            engine,
            database: projection.database_mut(),
            tail,
        })
    }

    /// The promoted clean-shutdown transition: prove every device-local drain
    /// and durably record `Safe`, on the retained enrollment session.
    ///
    /// This is [`LocalActiveAuthority::quiesce_and_mark_safe`]'s promoted form.
    /// The drain proof is the identical one — no new or weakened condition —
    /// but the journal mutation borrows the session this runtime already holds,
    /// because a second writer would contend with this process's own exclusive
    /// enrollment lease.
    ///
    /// It carries the same missing-dependency caveat as the pre-promotion form:
    /// the graph-text watcher event queue is owned by the Tauri watcher, so the
    /// production entry point still refuses rather than minting a `Safe` state
    /// that is not true. This test form exercises everything else, which is what
    /// makes the `Safe -> Unsafe { new session }` restart path provable.
    #[cfg(test)]
    pub(crate) fn quiesce_and_mark_safe_without_watcher_dependency_for_test(
        &mut self,
        authority: &mut LocalActiveAuthority,
        graph: &Graph,
    ) -> Result<SafeHandoffPermit, SafeHandoffUnavailable> {
        if authority.session_id != self.session_id
            || authority.verification_digest != self.verification_digest
        {
            return Err(SafeHandoffUnavailable::Runtime(
                "live authority is not this promoted runtime's session".into(),
            ));
        }
        authority
            .authenticate_runtime(graph, &self.engine)
            .map_err(|error| SafeHandoffUnavailable::Runtime(error.to_string()))?;
        let (handoff, head) = {
            let committed = self.enrollment.reauthenticate()?;
            (committed.handoff(), committed.enrollment_head())
        };
        match handoff {
            LocalActiveHandoff::Unsafe { session_id } if session_id == self.session_id => {}
            LocalActiveHandoff::Unsafe { .. } => {
                return Err(SafeHandoffUnavailable::Enrollment(
                    VerifiedLocalCompositionError::CompetingSession,
                ));
            }
            LocalActiveHandoff::Safe => {
                return Err(SafeHandoffUnavailable::Runtime(
                    "committed handoff is already Safe for another drain".into(),
                ));
            }
        }
        authority.enrollment_head = head;
        authority.handoff = handoff;

        // Reserving the graph handoff proves that graph text admission and every
        // managed writer lease are drained, and holds them drained across the
        // revalidation pass below.
        let reservation = graph
            .mint_handoff_safe(self.engine.workspace_id(), self.endpoint)
            .map_err(|error| SafeHandoffUnavailable::Runtime(error.to_string()))?;
        reservation
            .verify_binding(graph, self.engine.workspace_id(), self.endpoint)
            .map_err(|error| SafeHandoffUnavailable::Runtime(error.to_string()))?;
        let outcome = (|| {
            prove_device_local_drains(&self.engine, self.projection.database(), &self.tail)?;
            prove_device_local_drains(&self.engine, self.projection.database(), &self.tail)
        })();
        reservation.cancel();
        outcome?;

        let (handoff, head, verification_digest, sync) = {
            let committed = self
                .enrollment
                .transition_handoff(LocalActiveHandoff::Safe)?;
            (
                committed.handoff(),
                committed.enrollment_head(),
                committed.verification_digest(),
                committed.sync(),
            )
        };
        if handoff != LocalActiveHandoff::Safe || sync != LocalActiveSync::Idle {
            return Err(SafeHandoffUnavailable::Runtime(
                "committed handoff state is not Safe+Idle after the transition".into(),
            ));
        }
        authority.enrollment_head = head;
        authority.handoff = handoff;
        Ok(SafeHandoffPermit {
            enrollment_head: head,
            verification_digest,
            session_id: self.session_id,
            _seal: seal::Seal,
        })
    }

    /// The same admission with the bounded fast path selectively disabled.
    ///
    /// This is the parent shape: every admission performs the complete
    /// authenticated enrollment reopen, rereads the archive claim and control
    /// identity, and queries the device-local SQLite frontier. It exists only
    /// so the bounded-admission assertions can be shown to discriminate — an
    /// instrument that could not see the difference would pass here too.
    #[cfg(test)]
    pub(crate) fn admit_promoted_mutation_at_full_depth_for_test<'a>(
        &'a mut self,
        authority: &'a mut LocalActiveAuthority,
        graph: &Graph,
    ) -> Result<PromotedRuntimeSession<'a>, RuntimePromotionError> {
        self.admit_at_depth(authority, graph, BindingProofDepth::Boundary)
    }

    /// Re-prove the complete promoted binding against live durable state.
    ///
    /// [`BindingProofDepth::Boundary`] is the unabridged proof the open,
    /// handoff, and recovery boundaries use: complete enrollment
    /// reauthentication, archive claim and control identity reread, and the
    /// device-local SQLite frontier. [`BindingProofDepth::Admission`] is the
    /// per-mutation form, whose stable session facts are carried rather than
    /// re-derived — and which escalates itself the moment the retained session
    /// reports a different binding generation.
    ///
    /// The one thing an admission never escalates to is the SQLite frontier
    /// query, deliberately and unconditionally: SQLite is disposable derived
    /// state that can never authorize a write, so it belongs at the open,
    /// rebuild, drain, and `Safe`-handoff drain proofs rather than in the
    /// keystroke path.
    fn prove_binding(
        &mut self,
        graph: &Graph,
        authority: &LocalActiveAuthority,
        depth: BindingProofDepth,
    ) -> Result<(), RuntimePromotionError> {
        if authority.session_id != self.session_id
            || authority.verification_digest != self.verification_digest
        {
            return Err(RuntimePromotionError::Anchor(
                "live authority is not this promoted runtime's session",
            ));
        }
        if depth == BindingProofDepth::Boundary {
            self.enrollment.reauthenticate()?;
        }
        let generation = self.enrollment.binding_generation();
        let archive = if depth == BindingProofDepth::Admission
            && generation == self.archive_authenticated_generation
        {
            ArchiveAuthentication::Carried
        } else {
            ArchiveAuthentication::Reread
        };
        let accepted = revalidate_promoted_binding(
            &self.state,
            self.anchor,
            self.engine.runtime_authority(),
            graph,
            &self.engine,
            authority,
            self.enrollment.committed(),
            archive,
        )?;
        if self.endpoint.endpoint_id() != authority.endpoint.endpoint_id()
            || self.endpoint.device_id() != authority.endpoint.device_id()
            || self.endpoint.graph_resource_id() != authority.endpoint.graph_resource_id()
        {
            return Err(RuntimePromotionError::Anchor(
                "promoted endpoint binding is not the authority's enrolled endpoint",
            ));
        }
        if depth == BindingProofDepth::Boundary
            && sqlite_frontier_root(self.projection.database())? != accepted
        {
            return Err(RuntimePromotionError::Anchor(
                "promoted SQLite frontier is not the current accepted frontier",
            ));
        }
        self.archive_authenticated_generation = generation;
        Ok(())
    }
}

/// Whether one binding proof rereads the archive's identity facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArchiveAuthentication {
    /// Reread the persisted canonical resource claim and re-stat the physical
    /// control directory.
    Reread,
    /// Carry the facts authenticated at this session's current binding
    /// generation. Only reachable when the retained enrollment session reports
    /// that generation unchanged, which means the committed enrollment head is
    /// byte-identical to the one those facts were authenticated against.
    Carried,
}

/// Re-prove the complete promoted binding against live durable state, and
/// return the authenticated current accepted frontier.
///
/// Cost at [`ArchiveAuthentication::Reread`] is one archive-resource claim
/// read, one archive control-identity stat, one durable head read, one
/// shared-radix insertion-only walk bounded by the changed paths, and one
/// engine frontier read. At [`ArchiveAuthentication::Carried`] the two archive
/// operations are the ones already authenticated for this binding generation.
/// The committed enrollment record is supplied by the caller's retained
/// session, which is what removes the per-mutation journal reopen. Nothing here
/// scans lifetime history, the enrollment chain, SQLite, or graph text.
#[allow(clippy::too_many_arguments)]
fn revalidate_promoted_binding(
    state: &PromotedRuntimeStateV1,
    anchor: EngineHistoryAuthority,
    engine_authority: &super::hot_engine::EngineAuthority,
    graph: &Graph,
    engine: &ShardedHotEngine,
    authority: &LocalActiveAuthority,
    committed: &CommittedLocalActive,
    archive_authentication: ArchiveAuthentication,
) -> Result<super::hot_engine::AcceptedFrontierRoot, RuntimePromotionError> {
    // The engine offered for work must be the exact promoted engine instance.
    // A same-identity engine rebuilt from divergent, rolled-back, or merely
    // higher-sequence history mints a distinct process-local authority and is
    // refused here, before any durable or graph mutation.
    if !engine_authority.matches(engine.runtime_authority()) {
        return Err(RuntimePromotionError::Activation(
            LocalActivationError::RuntimeBinding(
                "admitted engine is not the exact promoted runtime engine".into(),
            ),
        ));
    }
    let binding = authority.evidence.binding();
    // Enrollment identity: the authority and the durable promotion state must
    // be the same enrollment and verification digest.
    if authority.verification_digest != state.enrollment_verification_digest
        || binding
            .binding_digest()
            .map_err(VerifiedLocalCompositionError::Enrollment)?
            != state.enrollment_binding_digest
    {
        return Err(RuntimePromotionError::Anchor(
            "live authority is not the enrollment this promoted runtime binds",
        ));
    }
    // Live graph capability and enrolled engine binding.
    authority.authenticate_runtime(graph, engine)?;
    // Storage binding.
    match engine.projection_endpoint_binding() {
        Some(endpoint)
            if endpoint.endpoint_id() == state.endpoint_id
                && endpoint.device_id() == state.device_id
                && endpoint.graph_resource_id() == state.graph_resource_id => {}
        _ => {
            return Err(RuntimePromotionError::Anchor(
                "promoted engine endpoint is not the promoted storage binding",
            ));
        }
    }
    if engine.projection_receipt_store_id() != Some(state.receipt_store_id) {
        return Err(RuntimePromotionError::Anchor(
            "promoted engine receipt store is not the promoted storage binding",
        ));
    }
    // Archive resource claim and physical archive control identity. These are
    // stable session facts reread at every boundary and at every observed
    // enrollment-head change; an unchanged-head admission carries them.
    let archive = engine.archive_store().ok_or(RuntimePromotionError::Anchor(
        "promoted engine retained no archive capability",
    ))?;
    if archive_authentication == ArchiveAuthentication::Reread {
        authenticate_archive_identity(
            archive,
            state,
            "promoted archive resource claim no longer authenticates",
            "promoted archive control directory was substituted",
        )?;
    }
    // The live open must still be authorized by exactly this durable state.
    if engine.promoted_lineage() != Some(state) {
        return Err(RuntimePromotionError::Anchor(
            "promoted engine no longer holds this exact promotion authorization",
        ));
    }
    // Authenticated bootstrap anchor -> current history transition. Raw
    // generation and acceptance sequence are never the proof.
    let transition = engine.authenticate_history_descends_from(anchor)?;
    if transition.before() != anchor || transition.after() != engine.durable_history_authority()? {
        return Err(RuntimePromotionError::Anchor(
            "current durable history is not an authenticated descendant of the bootstrap anchor",
        ));
    }
    let accepted = engine
        .accepted_frontier_root()
        .map_err(RuntimePromotionError::Engine)?;
    if accepted.acceptance_sequence() < state.anchor_acceptance_sequence {
        return Err(RuntimePromotionError::Anchor(
            "promoted engine accepted frontier is behind the bootstrap anchor",
        ));
    }
    // Committed enrollment verification digest, session, and head, as the
    // caller's retained session most recently revalidated them.
    if committed.verification_digest() != state.enrollment_verification_digest
        || committed.binding() != binding
        || committed.sync() != LocalActiveSync::Idle
        || committed.session_id() != Some(authority.session_id)
        || committed.enrollment_head() != authority.enrollment_head
    {
        return Err(RuntimePromotionError::Enrollment(
            VerifiedLocalCompositionError::StaleEvidence(
                "committed LocalActive record is not this promoted session's current record",
            ),
        ));
    }
    Ok(accepted)
}

/// A short-lived promoted mutation window derived from both a live authority
/// and the exact promoted runtime token.
///
/// The window splits the promoted runtime's exclusive borrow so a caller can
/// hold the admission *and* mutate the enrolled engine, SQLite frontier, and
/// tail at the same time, which is what every execution path needs.
pub(crate) struct PromotedRuntimeSession<'a> {
    admission: PromotedRuntimeAdmission<'a>,
    engine: &'a mut ShardedHotEngine,
    database: &'a mut SqliteFrontier,
    tail: &'a mut TailOverlay,
}

impl PromotedRuntimeSession<'_> {
    pub(crate) const fn session_id(&self) -> SessionId {
        self.admission.permit.session_id()
    }

    pub(crate) const fn enrollment_head(&self) -> ContentDigest {
        self.admission.permit.enrollment_head()
    }

    /// Derive the runtime admission every new-architecture mutation,
    /// projection, import, coordinator, and reconciliation path requires.
    pub(crate) const fn admission(&self) -> LocalRuntimeAdmission<'_> {
        LocalRuntimeAdmission {
            provenance: AdmissionProvenance::Promoted(&self.admission),
        }
    }

    pub(crate) const fn engine(&mut self) -> &mut ShardedHotEngine {
        self.engine
    }

    pub(crate) const fn database(&mut self) -> &mut SqliteFrontier {
        self.database
    }

    pub(crate) const fn tail(&mut self) -> &mut TailOverlay {
        self.tail
    }

    /// Drain the accepted tail into the device-local SQLite projection.
    ///
    /// This is the ordinary bounded [`TailOverlay`] drain every enrolled
    /// runtime uses. The only promoted-specific part is the rebuild source:
    /// this lineage's leading acceptance sequences are its retained immutable
    /// bootstrap parts, which live in the archive's bootstrap namespace rather
    /// than the ordinary object namespace.
    ///
    /// Draining is not a mutation authority. It applies only already-accepted
    /// authenticated events, in exact acceptance order, and writes nothing to
    /// the graph.
    pub(crate) fn drain_projection(
        &mut self,
        max_batches: usize,
    ) -> Result<usize, RuntimePromotionError> {
        let engine = &*self.engine;
        let store = engine.archive_store().ok_or(RuntimePromotionError::Anchor(
            "promoted engine retained no archive capability",
        ))?;
        let publication = store
            .load_bootstrap_publication(self.admission.state.bootstrap.publication_id())
            .map_err(RuntimePromotionError::Store)?;
        let source = RebuildSource::from_promoted_runtime(engine, store, &publication)?;
        self.tail
            .drain_ready(self.database, &source, max_batches)
            .map_err(|error| match error {
                super::sqlite::TailOverlayError::Projection(error) => {
                    RuntimePromotionError::Sqlite(error)
                }
                other => RuntimePromotionError::Activation(LocalActivationError::RuntimeBinding(
                    other.to_string(),
                )),
            })
    }

    /// The complete admitted runtime, borrowed disjointly.
    pub(crate) const fn parts(
        &mut self,
    ) -> (
        LocalRuntimeAdmission<'_>,
        &mut ShardedHotEngine,
        &mut SqliteFrontier,
        &mut TailOverlay,
    ) {
        (
            LocalRuntimeAdmission {
                provenance: AdmissionProvenance::Promoted(&self.admission),
            },
            self.engine,
            self.database,
            self.tail,
        )
    }
}

/// The non-forgeable evidence one promoted mutation window carries.
///
/// It retains the durable promotion state, the bootstrap anchor, the exact
/// promoted engine identity, and the live authority permit, so authorization
/// re-proves the whole binding without borrowing the runtime it split.
pub(crate) struct PromotedRuntimeAdmission<'a> {
    permit: LocalMutationPermit<'a>,
    state: PromotedRuntimeStateV1,
    anchor: EngineHistoryAuthority,
    engine_authority: super::hot_engine::EngineAuthority,
    /// The retained session's binding generation when this window opened.
    binding_generation: u64,
    /// The exact retained enrollment session this window was admitted through.
    enrollment: &'a RetainedEnrollmentSession,
    _seal: seal::Seal,
}

impl PromotedRuntimeAdmission<'_> {
    fn authorize_engine(
        &self,
        graph: &Graph,
        engine: &ShardedHotEngine,
    ) -> Result<(), RuntimePromotionError> {
        // A window is authorized only at the exact session-local binding
        // generation it was minted at. Any full revalidation or journal
        // mutation moves that generation, so a window that outlived a lifecycle
        // change cannot authorize work.
        if self.binding_generation != self.enrollment.binding_generation() {
            return Err(RuntimePromotionError::Anchor(
                "promoted admission is not the retained session's current binding generation",
            ));
        }
        // The cheap exact head check again, now fail-closed: a window holds no
        // authority to reauthenticate, so an enrollment head that moved while
        // it was live refuses rather than adopting the new state.
        if !self
            .enrollment
            .committed_head_is_unchanged()
            .map_err(|error| {
                RuntimePromotionError::Enrollment(VerifiedLocalCompositionError::Enrollment(error))
            })?
        {
            return Err(RuntimePromotionError::Enrollment(
                VerifiedLocalCompositionError::StaleEvidence(
                    "committed LocalActive head changed while a promoted window was live",
                ),
            ));
        }
        revalidate_promoted_binding(
            &self.state,
            self.anchor,
            &self.engine_authority,
            graph,
            engine,
            self.permit.authority,
            self.enrollment.committed(),
            ArchiveAuthentication::Carried,
        )
        .map(|_| ())
    }
}

/// Phase two of promotion, same process: open the promoted writable runtime.
///
/// `workspace` is how this process proves it owns the archive:
///
/// * [`RetainedWorkspaceLease`] hands over the exact lease the caller already
///   holds from the inactive bootstrap database, so the bootstrap -> promoted
///   database handoff never releases the workspace lock. Its refusal type
///   returns that exact lease, because this call runs *after* the durable
///   promotion state has been published;
/// * [`AcquireWorkspaceLease`] takes the lease here, which requires the
///   retained inactive bootstrap projection to have been released first. The
///   archive-rooted lease enforces that rather than trusting it, and a refusal
///   is the ordinary [`RuntimePromotionError`].
pub(crate) fn open_promoted_local_runtime<W: PromotedWorkspaceAuthority>(
    sealed: SealedRuntimePromotion,
    authority: &LocalActiveAuthority,
    open: &PromotedRuntimeOpen<'_>,
    workspace: W,
) -> Result<PromotedLocalRuntime, W::Refusal> {
    // The enrollment lease comes first, exactly as the documented global lock
    // order requires: enrollment lease, then archive/engine lease, then graph
    // and process-local locks. The promoted runtime retains this session for
    // its whole lifetime, so it is acquired once, here.
    let enrollment = match RetainedEnrollmentSession::open(
        &authority.application_root,
        authority.evidence.binding(),
        authority.verification_digest,
    ) {
        Ok(enrollment) => enrollment,
        Err(error) => return Err(workspace.refuse(error.into())),
    };
    let committed = enrollment.committed();
    if committed.verification_digest() != sealed.state.enrollment_verification_digest
        || committed.session_id() != Some(authority.session_id)
    {
        return Err(workspace.refuse(RuntimePromotionError::Enrollment(
            VerifiedLocalCompositionError::StaleEvidence(
                "committed LocalActive record is not the promoting session's record",
            ),
        )));
    }
    mint_promoted_runtime(
        sealed.state,
        enrollment,
        authority.session_id,
        authority.verification_digest,
        authority.endpoint,
        authority.evidence.binding(),
        open,
        workspace,
        RuntimeRecoveryState::FirstPromotion,
    )
}

/// The sole fresh-process promoted-runtime boundary.
///
/// A restarted process holds no [`VerifiedLocalEvidence`], no
/// [`LocalActiveAuthority`], no [`SealedRuntimePromotion`], and no engine
/// identity: all four are process-local and unserializable. This reconstructs
/// everything from durable state and the retained immutable bootstrap
/// publication:
///
/// * the committed `LocalActive` record and its exact original `VerifiedLocal`
///   bootstrap anchor, self-authenticated from the hash-linked enrollment chain
///   even though the live history head has advanced far past the bootstrap;
/// * the durable promotion state, which must bind that exact anchor, archive
///   resource, storage binding, enrollment binding, and verification digest;
/// * an enrolled `ShardedHotEngine`, its projection-work index, and its
///   reference/catalog authority, through the ordinary authenticated recovery
///   replay;
/// * an authenticated proof that the current history is exactly, or
///   insertion-only descended from, the bootstrap anchor;
/// * the device-local SQLite projection and bounded tail at the current
///   accepted frontier.
///
/// The authority is minted only after that complete recovery, and the promoted
/// token only after the authority. A crash still resumes `Unsafe`; an
/// `Unsafe { other session }`, `Safe`-without-request, `Blocked`, non-`Idle`,
/// or absent record fails closed.
pub(crate) fn reopen_promoted_local_runtime(
    root: &EnrollmentApplicationRoot,
    binding: &EnrollmentBindingV1,
    session_id: SessionId,
    open: &PromotedRuntimeOpen<'_>,
) -> Result<(LocalActiveAuthority, PromotedLocalRuntime), RuntimePromotionError> {
    reopen_promoted_local_runtime_with_adoption(
        root,
        binding,
        session_id,
        open,
        HandoffAdoption::OwnSessionOrSafe,
        TakeoverPublication::Durable,
    )
}

/// The sole archive-lease-proved `LocalActive` crash-takeover boundary.
///
/// A new process may adopt a *different* session's committed
/// `HandoffUnsafe { old }` only by proving the old process is gone, and the only
/// admissible proof is exclusive ownership of the archive-rooted
/// [`WorkspaceRuntimeLease`]. An `EnrollmentLease` alone is not sufficient:
/// it lives in device-local app data, so a second process running under another
/// XDG, HOME, or Flatpak root would not contend for it at all, while the
/// workspace lease is a file inside the archive that every such process
/// contends on.
///
/// The sequence is deliberately: authenticate the predecessor, take the
/// enrollment lease, take the workspace lease, recover and authenticate the
/// *entire* runtime the predecessor left behind — archive resource and control
/// identity, promotion state, bootstrap anchor, engine history transition,
/// accepted frontier, projection-work index, reference/catalog authority, SQLite
/// authority and frontier, bounded tail — and only then compare-and-swap
/// `Unsafe { old }` to `Unsafe { new }` from the exact authenticated head and
/// session. Every failure before that swap leaves the predecessor's record
/// authoritative and writes nothing.
///
/// It never mints `Safe`, and it never imports external Markdown: a crashed
/// predecessor proved no drain, so [`PromotedLocalRuntime::automatic_external_import`]
/// stays blocked until a later clean `Safe` handoff.
pub(crate) fn take_over_promoted_local_runtime(
    root: &EnrollmentApplicationRoot,
    binding: &EnrollmentBindingV1,
    session_id: SessionId,
    open: &PromotedRuntimeOpen<'_>,
) -> Result<(LocalActiveAuthority, PromotedLocalRuntime), RuntimePromotionError> {
    reopen_promoted_local_runtime_with_adoption(
        root,
        binding,
        session_id,
        open,
        HandoffAdoption::CrashTakeover,
        TakeoverPublication::Durable,
    )
}

/// The same takeover with its durable publication interrupted at one exact cut.
#[cfg(test)]
pub(crate) fn take_over_promoted_local_runtime_at_cut_for_test(
    root: &EnrollmentApplicationRoot,
    binding: &EnrollmentBindingV1,
    session_id: SessionId,
    open: &PromotedRuntimeOpen<'_>,
    cut: super::enrollment::CommitCut,
) -> Result<(LocalActiveAuthority, PromotedLocalRuntime), RuntimePromotionError> {
    reopen_promoted_local_runtime_with_adoption(
        root,
        binding,
        session_id,
        open,
        HandoffAdoption::CrashTakeover,
        TakeoverPublication::AtCut(cut),
    )
}

/// Which committed handoff states a fresh-process open may adopt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HandoffAdoption {
    /// This exact session's `Unsafe` record, or a clean `Safe` handoff. Another
    /// session's `Unsafe` record is a competing owner and is refused.
    OwnSessionOrSafe,
    /// Additionally: another session's `Unsafe` record, taken over under the
    /// archive-rooted workspace runtime lease.
    CrashTakeover,
}

/// How the takeover compare-and-swap is published.
enum TakeoverPublication {
    Durable,
    /// Interrupt the durable publication at one exact enrollment cut.
    #[cfg(test)]
    AtCut(super::enrollment::CommitCut),
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

fn takeover_predecessor_observed() {
    #[cfg(test)]
    TAKEOVER_PREDECESSOR_OBSERVED.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook();
        }
    });
}

fn reopen_promoted_local_runtime_with_adoption(
    root: &EnrollmentApplicationRoot,
    binding: &EnrollmentBindingV1,
    session_id: SessionId,
    open: &PromotedRuntimeOpen<'_>,
    adoption: HandoffAdoption,
    publication: TakeoverPublication,
) -> Result<(LocalActiveAuthority, PromotedLocalRuntime), RuntimePromotionError> {
    let anchor = reopen_promoted_bootstrap_anchor(root, binding)?;
    // The committed record decides which of the three durable predecessors this
    // open is recovering from. A competing session is refused here, before any
    // archive, engine, SQLite, or lease work happens, unless this is an explicit
    // crash takeover — which still writes nothing until the whole runtime below
    // has been recovered and authenticated.
    let (predecessor, recovery) = match anchor.committed().handoff() {
        LocalActiveHandoff::Unsafe {
            session_id: committed_session,
        } if committed_session == session_id => (None, RuntimeRecoveryState::ResumedOwnUnsafe),
        LocalActiveHandoff::Unsafe {
            session_id: committed_session,
        } => {
            if adoption != HandoffAdoption::CrashTakeover {
                return Err(RuntimePromotionError::Enrollment(
                    VerifiedLocalCompositionError::CompetingSession,
                ));
            }
            (
                Some(UnsafeHandoffPredecessor {
                    enrollment_head: anchor.committed().enrollment_head(),
                    session_id: committed_session,
                }),
                RuntimeRecoveryState::TookOverCrashedUnsafe {
                    previous_session: committed_session,
                },
            )
        }
        LocalActiveHandoff::Safe => (None, RuntimeRecoveryState::AdoptedSafeHandoff),
    };
    takeover_predecessor_observed();
    // The enrollment lease comes first, before any archive, engine, or SQLite
    // work, exactly as the documented global lock order requires. The promoted
    // runtime retains this session for its whole lifetime, so it is acquired
    // once, here, and every journal mutation below borrows it.
    let enrollment = RetainedEnrollmentSession::open(root, binding, anchor.verification_digest())?;
    let state = read_promotion_state_for_anchor(open.archive_root, binding, &anchor)?;
    // The archive-rooted workspace runtime lease is taken inside this call,
    // before any archive, engine, or SQLite state is touched, and is retained
    // for the whole writable runtime.
    let mut runtime = mint_promoted_runtime(
        state,
        enrollment,
        session_id,
        anchor.verification_digest(),
        promoted_storage_binding_endpoint(binding),
        binding,
        open,
        AcquireWorkspaceLease,
        recovery,
    )?;

    // Only now, with complete recovery proved, may an authority exist. The
    // durable handoff protocol is the existing one: an `Unsafe { session }`
    // record reopens only for that exact session, and a clean `Safe` record is
    // durably moved to `Unsafe { requested session }` and freshly reopened
    // before any authority is minted. The transition runs on the retained
    // session, so it never reacquires the lease this process already holds.
    let (evidence, _committed) = anchor.into_predecessor_evidence();
    match (predecessor, runtime.enrollment.committed().handoff()) {
        // The archive-lease-proved crash takeover. Everything the predecessor's
        // runtime consisted of has been recovered and authenticated above, and
        // this process has held the workspace lease throughout, so the crashed
        // owner cannot still be running.
        (Some(predecessor), _) => {
            // The durable compare-and-swap borrows this runtime's own workspace
            // lease proof. It is not an assertion the caller supplies: it is
            // minted from the lease this runtime has held continuously since
            // before it read any archive state, and the borrow checker refuses
            // to let it outlive that lease.
            let PromotedLocalRuntime {
                enrollment,
                projection,
                ..
            } = &mut runtime;
            let workspace = projection.workspace_proof();
            match publication {
                TakeoverPublication::Durable => {
                    enrollment.take_over_unsafe_handoff(&workspace, predecessor, session_id)?;
                }
                #[cfg(test)]
                TakeoverPublication::AtCut(cut) => {
                    enrollment.take_over_unsafe_handoff_at_cut_for_test(
                        &workspace,
                        predecessor,
                        session_id,
                        cut,
                    )?;
                }
            }
            require_unchanged_bootstrap_anchor(root, binding, &evidence, &runtime)?;
        }
        (
            None,
            LocalActiveHandoff::Unsafe {
                session_id: committed_session,
            },
        ) if committed_session == session_id => {}
        (None, LocalActiveHandoff::Unsafe { .. }) => {
            return Err(RuntimePromotionError::Enrollment(
                VerifiedLocalCompositionError::CompetingSession,
            ));
        }
        (None, LocalActiveHandoff::Safe) => {
            runtime
                .enrollment
                .transition_handoff(LocalActiveHandoff::Unsafe { session_id })?;
            require_unchanged_bootstrap_anchor(root, binding, &evidence, &runtime)?;
        }
    }
    let reopened = runtime.enrollment.committed();
    if reopened.verification_digest() != evidence.verification_digest()
        || reopened.sync() != LocalActiveSync::Idle
        || reopened.handoff() != (LocalActiveHandoff::Unsafe { session_id })
        || reopened.binding() != evidence.binding()
    {
        return Err(RuntimePromotionError::Enrollment(
            VerifiedLocalCompositionError::StaleEvidence(
                "reopened LocalActive head is not this session's Unsafe+Idle record",
            ),
        ));
    }

    let authority = LocalActiveAuthority {
        application_root: root.clone(),
        verification_digest: reopened.verification_digest(),
        enrollment_head: reopened.enrollment_head(),
        handoff: reopened.handoff(),
        evidence,
        session_id,
        endpoint: runtime.endpoint,
        activation_acceptance_sequence: runtime
            .engine
            .accepted_frontier_root()
            .map_err(RuntimePromotionError::Engine)?
            .acceptance_sequence(),
        _seal: seal::Seal,
    };
    // Final proof: the freshly minted authority and the promoted runtime must
    // authenticate each other exactly, with no pre-minted in-memory evidence.
    // This is a recovery boundary, so it is the unabridged proof.
    runtime.prove_binding(open.graph, &authority, BindingProofDepth::Boundary)?;
    Ok((authority, runtime))
}

/// The immutable activation anchor must not have moved across a durable handoff
/// transition. It is reread from the enrollment chain, never from memory.
fn require_unchanged_bootstrap_anchor(
    root: &EnrollmentApplicationRoot,
    binding: &EnrollmentBindingV1,
    evidence: &VerifiedLocalEvidence,
    runtime: &PromotedLocalRuntime,
) -> Result<(), RuntimePromotionError> {
    let fresh = reopen_promoted_bootstrap_anchor(root, binding)?;
    if fresh.verification_digest() != evidence.verification_digest()
        || fresh.history_root() != runtime.anchor.index_root
        || fresh.history_generation() != runtime.anchor.generation
    {
        return Err(RuntimePromotionError::Enrollment(
            VerifiedLocalCompositionError::StaleEvidence(
                "bootstrap anchor changed during the promoted handoff transition",
            ),
        ));
    }
    Ok(())
}

fn promoted_storage_binding_endpoint(binding: &EnrollmentBindingV1) -> ProjectionEndpointBinding {
    ProjectionEndpointBinding {
        endpoint_id: binding.endpoint_id(),
        device_id: binding.device_id(),
        graph_resource_id: binding.graph_resource_id(),
    }
}

/// Read the durable promotion state and require it to bind exactly the durable
/// bootstrap anchor and enrollment the committed records prove.
///
/// A restarted process holds no retained capability at all, so this is the one
/// place where opening the configured archive pathname is unavoidable. The
/// promoted-state authorization boundary
/// ([`super::object_store::DurableEngineHistoryStore::read_promoted_runtime_state`])
/// revalidates the state's exact physical control-directory identity and its
/// canonical archive-resource claim against the freshly opened archive before it
/// returns any state, so a look-alike directory at the enrolled pathname is
/// rejected here rather than adopted.
fn read_promotion_state_for_anchor(
    archive_root: &Path,
    binding: &EnrollmentBindingV1,
    anchor: &PromotedBootstrapAnchor,
) -> Result<PromotedRuntimeStateV1, RuntimePromotionError> {
    let store = ObjectStore::open(archive_root, binding.workspace_id())?;
    let open = store
        .seal_history_only(super::hot_engine::ProjectionStorageBinding {
            endpoint: promoted_storage_binding_endpoint(binding),
            receipt_store_id: binding.receipt_store_id(),
        })
        .map_err(|(_store, error)| error)?;
    let (_store, history) = open.into_history().map_err(|(_store, error)| error)?;
    let state = history
        .read_promoted_runtime_state()?
        .ok_or(RuntimePromotionError::Store(
            StoreError::PromotedRuntimeStateAbsent,
        ))?;
    drop(history);
    if state.enrollment_verification_digest != anchor.verification_digest()
        || state.enrollment_binding_digest
            != binding
                .binding_digest()
                .map_err(VerifiedLocalCompositionError::Enrollment)?
        || state.archive_resource_id != binding.archive_resource_id()
        || state.workspace_id != binding.workspace_id()
        || state.lineage_digest != binding.lineage_digest()
        || state.catalog_document_id != binding.catalog_document_id()
        || state.endpoint_id != binding.endpoint_id()
        || state.device_id != binding.device_id()
        || state.graph_resource_id != binding.graph_resource_id()
        || state.receipt_store_id != binding.receipt_store_id()
    {
        return Err(RuntimePromotionError::Anchor(
            "durable promotion state is not this enrollment's promotion",
        ));
    }
    if state.anchor_history_generation != anchor.history_generation()
        || state.anchor_history_index_root != anchor.history_root()
        || state.anchor_acceptance_sequence != anchor.acceptance_sequence()
        || state.anchor_accepted_frontier_state_digest != anchor.accepted_frontier_state_digest()
        || u64::from(state.bootstrap.part_count()) != anchor.accepted_history_record_count()
        || ContentDigest::from_bytes(*state.bootstrap_import_id.as_bytes())
            != anchor.bootstrap_import_id()
        || state.bootstrap.part_count() != anchor.bootstrap_part_count()
    {
        return Err(RuntimePromotionError::Anchor(
            "durable promotion state does not bind the committed VerifiedLocal bootstrap anchor",
        ));
    }
    Ok(state)
}

/// Prove the promoted lineage's retained immutable publication and durable
/// reference-catalog authority are both present and exactly this lineage's.
///
/// Everything here is derived from the retained cold record and publication,
/// never from process-local evidence, so a restarted process derives it exactly
/// the same way.
fn require_promoted_bootstrap_runtime_authority(
    archive: &ObjectStore,
    state: &PromotedRuntimeStateV1,
) -> Result<(), RuntimePromotionError> {
    // Duplicated from the caller's already-authenticated retained capability,
    // never reopened from `archive.root_path()`: the promoted lineage's runtime
    // authority must be derived from the exact archive that was authenticated.
    let (store, history) = open_retained_history_control(archive, state)?;
    if history.current_bootstrap_binding()? != Some(state.bootstrap) {
        return Err(RuntimePromotionError::Anchor(
            "durable history bootstrap binding is not the promoted lineage",
        ));
    }
    let binding = super::hot_engine::bootstrap_reference_catalog_binding(&history)?;
    drop(history);
    let Some((_policy, root)) = binding else {
        return Ok(());
    };
    // The retained immutable publication must still load and be exactly this
    // lineage's publication before any runtime authority is derived from it.
    let publication = store.load_bootstrap_publication(state.bootstrap.publication_id())?;
    if publication.aggregate().aggregate_digest() != state.bootstrap.aggregate_digest()
        || publication.aggregate().import_id() != state.bootstrap_import_id
    {
        return Err(RuntimePromotionError::Anchor(
            "retained bootstrap publication is not the promoted lineage's publication",
        ));
    }
    super::hot_engine::require_promoted_bootstrap_reference_catalog(&store, &root)
        .map_err(RuntimePromotionError::Engine)
}

/// Open, recover, and authenticate every promoted runtime component, then mint
/// the token. Nothing partial is ever returned.
///
/// The archive-rooted workspace runtime lease is taken (or adopted) *first*,
/// before one byte of archive, engine, SQLite, or enrollment state is read or
/// written by this runtime. That ordering is the whole crash proof: a live
/// predecessor — this process's own previous runtime, another profile's process
/// under a different XDG/HOME/app-data root, or a racing newcomer — still owns
/// the lease, so this call fails here rather than after touching durable state.
#[allow(clippy::too_many_arguments)]
fn mint_promoted_runtime<W: PromotedWorkspaceAuthority>(
    state: PromotedRuntimeStateV1,
    enrollment: RetainedEnrollmentSession,
    session_id: SessionId,
    verification_digest: ContentDigest,
    expected_endpoint: ProjectionEndpointBinding,
    binding: &EnrollmentBindingV1,
    open: &PromotedRuntimeOpen<'_>,
    workspace: W,
    recovery: RuntimeRecoveryState,
) -> Result<PromotedLocalRuntime, W::Refusal> {
    // Everything below stays in one stack frame on purpose: `PromotedLocalRuntime`
    // and the recovered engine are tens of kilobytes, so an extra function
    // boundary would copy them again on a debug-build test stack. The three
    // macros are the whole error-routing protocol — before the lease exists,
    // while it is held, and once it lives inside the opened projection.
    macro_rules! refuse {
        ($error:expr) => {
            return Err(workspace.refuse($error))
        };
    }
    macro_rules! try_refuse {
        ($result:expr) => {
            match $result {
                Ok(value) => value,
                Err(error) => refuse!(RuntimePromotionError::from(error)),
            }
        };
    }

    if state.enrollment_verification_digest != verification_digest
        || enrollment.verification_digest() != verification_digest
    {
        refuse!(RuntimePromotionError::Anchor(
            "promotion state does not bind this LocalActive verification digest",
        ));
    }
    if enrollment.committed().binding() != binding {
        refuse!(RuntimePromotionError::Anchor(
            "the retained enrollment session is not this promotion's enrollment",
        ));
    }
    let archive = try_refuse!(ObjectStore::open(open.archive_root, state.workspace_id));
    // Workspace ownership, before anything else. A retained lease must be this
    // exact archive's and this exact workspace's; a lease for a look-alike
    // archive at another path can never be laundered into authority here.
    let (workspace_lease, custody) = workspace.into_lease(&archive, state.workspace_id)?;

    // From here the lease exists and this function does not own it: every
    // failure hands it to the custody, which releases it for an acquiring open
    // and returns it to the caller for a retained one.
    macro_rules! release {
        ($error:expr) => {
            return Err(custody.refuse_returning(workspace_lease, $error))
        };
    }
    macro_rules! try_release {
        ($result:expr) => {
            match $result {
                Ok(value) => value,
                Err(error) => release!(RuntimePromotionError::from(error)),
            }
        };
    }

    try_release!(authenticate_archive_identity(
        &archive,
        &state,
        "promoted archive resource claim does not authenticate",
        "promoted archive control directory identity changed",
    ));
    // The retained immutable publication and the durable reference-catalog
    // authority the cold record binds must both be present before the enrolled
    // open recovers from them.
    try_release!(require_promoted_bootstrap_runtime_authority(
        &archive, &state
    ));

    // Recovery replays the archive's committed manifests. This is the existing
    // enrolled-recovery cost and reads no graph text.
    let committed_manifests = try_release!(archive.committed_manifests());
    let anchor = state.anchor_authority();
    let (engine, _outcomes) = try_release!(ShardedHotEngine::open_promoted_projection(
        archive,
        state.lineage_digest,
        state.catalog_document_id,
        open.graph,
        open.receipts,
        &state,
        &committed_manifests,
    ));
    if engine.promoted_lineage() != Some(&state) {
        release!(RuntimePromotionError::Anchor(
            "promoted engine did not adopt the exact durable promotion state",
        ));
    }
    if engine.workspace_id() != binding.workspace_id()
        || engine.lineage_digest() != binding.lineage_digest()
        || engine.catalog_document_id() != binding.catalog_document_id()
        || engine.projection_receipt_store_id() != Some(binding.receipt_store_id())
    {
        release!(RuntimePromotionError::Anchor(
            "promoted engine does not authenticate the enrolled binding",
        ));
    }
    let Some(endpoint) = engine.projection_endpoint_binding() else {
        release!(RuntimePromotionError::Anchor(
            "promoted engine has no projection endpoint enrollment",
        ));
    };
    if endpoint.endpoint_id() != expected_endpoint.endpoint_id()
        || endpoint.device_id() != expected_endpoint.device_id()
        || endpoint.graph_resource_id() != expected_endpoint.graph_resource_id()
        || endpoint.endpoint_id() != binding.endpoint_id()
        || endpoint.device_id() != binding.device_id()
        || endpoint.graph_resource_id() != binding.graph_resource_id()
    {
        release!(RuntimePromotionError::Anchor(
            "promoted engine projection endpoint is not the enrolled endpoint",
        ));
    }
    // The recovered history must be exactly, or insertion-only descended from,
    // the exact bootstrap anchor.
    let transition = try_release!(engine.authenticate_history_descends_from(anchor));
    let durable_history = try_release!(engine.durable_history_authority());
    if transition.before() != anchor || transition.after() != durable_history {
        release!(RuntimePromotionError::Anchor(
            "recovered history is not an authenticated descendant of the bootstrap anchor",
        ));
    }
    let accepted = try_release!(engine
        .accepted_frontier_root()
        .map_err(RuntimePromotionError::Engine));
    if accepted.acceptance_sequence() < state.anchor_acceptance_sequence {
        release!(RuntimePromotionError::Anchor(
            "recovered accepted frontier is behind the bootstrap anchor",
        ));
    }
    // The projection-work index and reference/catalog authority must both be
    // live before the SQLite projection is opened at the current frontier.
    try_release!(engine
        .projection_work_index()
        .map_err(RuntimePromotionError::Engine));
    try_release!(engine
        .reference_catalog_root()
        .map_err(RuntimePromotionError::Engine));

    let Some(store) = engine.archive_store() else {
        release!(RuntimePromotionError::Anchor(
            "promoted engine retained no archive capability",
        ));
    };
    let claim = ProjectionClaim::current(state.workspace_id, state.lineage_digest);
    // A promoted lineage's leading accepted sequences are its retained
    // immutable bootstrap parts, which live in the archive's bootstrap
    // namespace rather than the ordinary object namespace. The publication
    // identity comes from the authorized promotion state.
    let publication = try_release!(store
        .load_bootstrap_publication(state.bootstrap.publication_id())
        .map_err(RuntimePromotionError::Store));
    // The database is opened through the retained workspace lease's single
    // applier slot, never through the compatibility entry point that would take
    // a second, temporary workspace lease of its own. A failed open returns the
    // slot *and* the lease, which is what makes it retryable without ever
    // releasing the archive.
    let (projection, ()) = match LeasedWorkspaceProjection::open_under::<(), ProjectionError>(
        workspace_lease,
        |slot| {
            let source = RebuildSource::from_promoted_runtime(&engine, store, &publication)
                .map_err(ProjectionError::from)?;
            SqliteFrontier::open_or_rebuild_with_applier_slot(
                open.database_path,
                open.application_runtime_root,
                claim,
                source,
                slot,
            )
            .map(|opened| (opened, ()))
        },
    ) {
        Ok(opened) => opened,
        Err((lease, error)) => {
            return Err(custody.refuse_returning(lease, RuntimePromotionError::Sqlite(error)))
        }
    };

    // The lease now lives inside `projection`, so a failure has to close the
    // database to get it back — which releases the database-adjacent lock and
    // nothing else.
    macro_rules! close_and_release {
        ($error:expr) => {
            return Err(custody.refuse_returning(projection.close_retaining_lease(), $error))
        };
    }

    // SQLite divergence is caught here, at open, and again at every drain. It
    // is deliberately absent from per-mutation admission: the keystroke path
    // must not issue a SQLite statement.
    let sqlite_root = match sqlite_frontier_root(projection.database()) {
        Ok(root) => root,
        Err(error) => close_and_release!(RuntimePromotionError::from(error)),
    };
    if sqlite_root != accepted {
        close_and_release!(RuntimePromotionError::Anchor(
            "promoted SQLite projection is not at the current accepted frontier",
        ));
    }
    let tail_source = match RebuildSource::from_promoted_runtime(&engine, store, &publication) {
        Ok(source) => source,
        Err(error) => close_and_release!(RuntimePromotionError::from(error)),
    };
    let tail = match TailOverlay::from_durable(projection.database(), &tail_source) {
        Ok(tail) => tail,
        Err(super::sqlite::TailOverlayError::Projection(error)) => {
            close_and_release!(RuntimePromotionError::Sqlite(error))
        }
        Err(other) => close_and_release!(RuntimePromotionError::Activation(
            LocalActivationError::RuntimeBinding(other.to_string()),
        )),
    };
    // The archive identity facts above were authenticated for exactly this
    // session binding generation, so an unchanged-head admission may carry
    // them and any change forces the reread again.
    let archive_authenticated_generation = enrollment.binding_generation();
    Ok(PromotedLocalRuntime {
        state,
        anchor,
        session_id,
        verification_digest,
        endpoint,
        enrollment,
        archive_authenticated_generation,
        recovery,
        engine,
        projection,
        tail,
        _seal: seal::Seal,
    })
}

/// The device-local inactive-bootstrap projection a local activation runs on,
/// owned together with the archive-rooted workspace runtime lease that
/// authorized opening it.
///
/// This is the crate's inactive-bootstrap open. Every other way of opening that
/// database — [`SqliteFrontier::open_or_rebuild_inactive_bootstrap`] — is the
/// compatibility entry point, which takes a *temporary* workspace lease of its
/// own and releases it when the projection drops; the remaining callers of that
/// entry point are SQLite-level tests that never promote. An activation must
/// not use it, because the bootstrap database and the promoted database it
/// becomes have to be authorized by one continuously-held archive lock:
/// releasing between them is exactly the window in which another process, under
/// any XDG/HOME/Flatpak root, could take the archive after the promotion state
/// has already been published.
///
/// The type makes that window inexpressible rather than merely discouraged.
/// [`Self::promote`] consumes the session, and no accessor hands out the lease,
/// so there is no way to reach phase two of promotion except through the exact
/// lease this database was opened under.
///
/// Scope note: `tine-core`'s oplog stack is still not reachable from
/// application startup (see `oplog/mod.rs`), so "production" here means the
/// non-test construction that the activation wiring will call — not a path a
/// running Tine binary executes today.
pub(crate) struct InactiveBootstrapRuntimeSession {
    projection: LeasedWorkspaceProjection,
    sqlite_proof: VerifiedBootstrapSqliteProjection,
}

impl InactiveBootstrapRuntimeSession {
    /// Take the archive-rooted workspace lease and open the inactive bootstrap
    /// database under its single applier slot.
    pub(crate) fn open(
        archive_root: &Path,
        workspace: WorkspaceId,
        database_path: &Path,
        application_runtime_root: &ApplicationRuntimeRoot,
        authority: &InactiveBootstrapAcceptedAuthority,
    ) -> Result<Self, ProjectionError> {
        let archive = ObjectStore::open(archive_root, workspace)?;
        let lease = WorkspaceRuntimeLease::acquire(&archive, workspace)?;
        Self::reopen_under(lease, database_path, application_runtime_root, authority)
            .map_err(|(_released_lease, error)| error)
    }

    /// Reopen the inactive bootstrap database under a lease this process is
    /// already holding.
    ///
    /// This is the retry path after a refused promotion handed the lease back:
    /// recovering from that failure must not go through releasing the archive
    /// either. A failed reopen returns the lease again for the same reason.
    pub(crate) fn reopen_under(
        lease: WorkspaceRuntimeLease,
        database_path: &Path,
        application_runtime_root: &ApplicationRuntimeRoot,
        authority: &InactiveBootstrapAcceptedAuthority,
    ) -> Result<Self, (WorkspaceRuntimeLease, ProjectionError)> {
        LeasedWorkspaceProjection::open_under::<VerifiedBootstrapSqliteProjection, ProjectionError>(
            lease,
            |slot| {
                SqliteFrontier::open_or_rebuild_inactive_bootstrap_with_applier_slot(
                    database_path,
                    application_runtime_root,
                    authority,
                    slot,
                )
            },
        )
        .map(|(projection, sqlite_proof)| Self {
            projection,
            sqlite_proof,
        })
    }

    pub(crate) const fn projection(&self) -> &OpenProjection {
        self.projection.projection()
    }

    pub(crate) const fn sqlite_proof(&self) -> &VerifiedBootstrapSqliteProjection {
        &self.sqlite_proof
    }

    /// Phase two of promotion, under this session's own retained lease.
    ///
    /// Closing the bootstrap database releases the database-adjacent applier
    /// lock and nothing else: the archive-rooted workspace lock is a distinct OS
    /// handle owned by the lease, which is moved straight into the promoted
    /// open. A refusal returns that same lease, so a failed promotion leaves the
    /// archive exactly as held as it was before the attempt.
    pub(crate) fn promote(
        self,
        sealed: SealedRuntimePromotion,
        authority: &LocalActiveAuthority,
        open: &PromotedRuntimeOpen<'_>,
    ) -> Result<PromotedLocalRuntime, RetainedPromotionRefusal> {
        let lease = self.projection.close_retaining_lease();
        open_promoted_local_runtime(sealed, authority, open, RetainedWorkspaceLease::new(lease))
    }
}

#[cfg(test)]
mod tests;

/// Bounded promoted admission: exact causal counters over a real promoted
/// runtime.
///
/// This module owns its own compact promotion fixture on purpose. The
/// neighbouring `tests` module's fixture is private to that module, and the
/// claims proved here are about *cost accounting* rather than about the
/// activation and promotion journeys `tests` already covers.
#[cfg(test)]
mod bounded_admission {
    use super::*;
    use crate::oplog::enrollment::{
        compose_verified_local, enrollment_application_root_for_test, EnrollmentOpen,
        EnrollmentReader, EnrollmentWriter, PreparationId,
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
    use crate::oplog::{
        AuthorBatch, BatchDisposition, BatchId, BatchOrigin, BlockId, BlockLocation,
        CanonicalArchiveResourceId, CrdtPeerId, DeviceId, DocumentId, LineageDigest,
        LogicalPageName, ManagedPath, ManagedTextKind, OperationTransaction, PageId,
        ProjectionEndpointId, ProjectionReceiptStore, ReferenceCatalogPolicyV1, SemanticOperation,
        WorkspaceId,
    };
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use uuid::Uuid;

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("tine-bounded-admission-{label}-{}", Uuid::new_v4()));
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

    /// One complete inactive enrollment over one real graph: real capture,
    /// publication, backup, SQLite bootstrap, shadow projection, and receipt
    /// namespace.
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
        sqlite: Option<InactiveBootstrapRuntimeSession>,
        archive_resource_id: CanonicalArchiveResourceId,
        shadow: VerifiedShadowProjection,
        preparation: PreparationId,
        original_graph: BTreeMap<String, Vec<u8>>,
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
            let original_graph = snapshot_files(&graph_root);
            let graph = Graph::open(&graph_root);

            let workspace = WorkspaceId::from_uuid(Uuid::from_u128(0x9100));
            let lineage = LineageDigest::of(b"bounded-admission-test");
            let catalog_document_id = DocumentId::from_uuid(Uuid::from_u128(0x9101));

            let receipt_root = root.path().join("receipts");
            fs::create_dir(&receipt_root).unwrap();
            let endpoint = ProjectionEndpointBinding::enroll_graph(
                &graph,
                ProjectionEndpointId::from_uuid(Uuid::from_u128(0x9102)),
                DeviceId::from_uuid(Uuid::from_u128(0x9103)),
            )
            .unwrap();
            let receipts =
                ProjectionReceiptStore::open_for_endpoint(&receipt_root, workspace, endpoint)
                    .unwrap();

            let capture_root = root.path().join("capture");
            let preparation_root = root.path().join("preparation");
            fs::create_dir(&capture_root).unwrap();
            fs::create_dir(&preparation_root).unwrap();
            let capture = graph
                .capture_inactive_bootstrap_sources(&capture_root)
                .unwrap();
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
            let runtime =
                ApplicationRuntimeRoot::open_for_test(&root.path().join("runtime")).unwrap();
            // The one production inactive-bootstrap open: the workspace lease
            // and the database it authorized are one value, and the promotion
            // below can only reach phase two through it.
            let sqlite = InactiveBootstrapRuntimeSession::open(
                &archive_root,
                workspace,
                &root.path().join("bootstrap.sqlite"),
                &runtime,
                &authority,
            )
            .expect("inactive bootstrap runtime session");
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
                sqlite.projection(),
                sqlite.sqlite_proof(),
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
                archive_resource_id,
                shadow,
                preparation: PreparationId::new(),
                original_graph,
            }
        }

        fn bootstrap(&self) -> &InactiveBootstrapRuntimeSession {
            self.sqlite
                .as_ref()
                .expect("retained inactive bootstrap projection")
        }

        fn sqlite(&self) -> &OpenProjection {
            self.bootstrap().projection()
        }

        /// Take the production inactive-bootstrap session out of the fixture.
        /// Phase two of promotion runs on its own retained lease, so the
        /// workspace lock is never released between the two databases.
        fn take_bootstrap_session(&mut self) -> InactiveBootstrapRuntimeSession {
            self.sqlite
                .take()
                .expect("retained inactive bootstrap projection")
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
                sqlite_projection: self.bootstrap().sqlite_proof(),
                shadow_projection: &self.shadow,
            }
        }

        fn runtime(&self) -> LocalActiveRuntime<'_> {
            LocalActiveRuntime {
                engine: self.authority.accepted_engine(),
                projection: self.sqlite(),
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
            seal_local_runtime_promotion(&authority, &fixture.proofs(), &fixture.runtime())
                .unwrap();
        let bootstrap = fixture.take_bootstrap_session();
        let runtime = bootstrap
            .promote(sealed, &authority, &paths.open(fixture))
            .map_err(|refusal| refusal.into_parts().1)
            .unwrap();
        (authority, runtime)
    }

    /// Author, publish, accept, and drain one ordinary post-bootstrap local
    /// batch through the promoted runtime's admitted mutation window.
    fn append_local_batch(
        fixture: &Fixture,
        authority: &mut LocalActiveAuthority,
        runtime: &mut PromotedLocalRuntime,
        seed: u128,
    ) {
        let endpoint = authority.endpoint();
        let mut session = runtime
            .admit_promoted_mutation(authority, &fixture.graph)
            .unwrap();
        let transaction = OperationTransaction::new(vec![
            SemanticOperation::CreatePage {
                page_id: PageId::from_uuid(Uuid::from_u128(seed)),
                home_document_id: DocumentId::from_uuid(Uuid::from_u128(seed + 1)),
                name: LogicalPageName::parse(&format!("Bounded {seed}")).unwrap(),
                path: ManagedPath::parse(&format!("pages/bounded-{seed}.md")).unwrap(),
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
                content: format!("bounded local batch {seed}"),
            },
        ])
        .unwrap();

        let (admission, engine, _database, _tail) = session.parts();
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
        assert!(matches!(
            outcome.disposition,
            BatchDisposition::Accepted { .. }
        ));
        assert_eq!(session.drain_projection(16).unwrap(), 1);
    }

    /// Open exactly `count` admission windows and report the causal work they
    /// performed. Each window also authorizes once, which is what every real
    /// mutation path does before touching the engine.
    fn measure_admissions(
        fixture: &Fixture,
        authority: &mut LocalActiveAuthority,
        runtime: &mut PromotedLocalRuntime,
        count: usize,
    ) -> PromotedRuntimeInstrumentation {
        let before = PromotedRuntimeInstrumentation::capture();
        for _ in 0..count {
            let session = runtime
                .admit_promoted_mutation(authority, &fixture.graph)
                .unwrap();
            session
                .admission()
                .authorize(&fixture.graph, runtime_engine_of(&session))
                .unwrap();
        }
        before.since()
    }

    fn runtime_engine_of<'a>(session: &'a PromotedRuntimeSession<'_>) -> &'a ShardedHotEngine {
        session.engine
    }

    /// The keystroke path is bounded and journal/graph-length independent.
    ///
    /// At 1, 1,000, and 10,000 post-bootstrap admissions — and after real
    /// post-bootstrap batches have advanced the durable history — an
    /// unchanged-head admission must perform:
    ///
    /// * zero SQLite statements;
    /// * zero archive control-directory re-stats or resource-claim rereads;
    /// * zero enrollment namespace enumerations, lease reacquisitions,
    ///   directory-tree opens, authority-claim rereads, and record-chain reads;
    /// * exactly two bounded reads of the fixed-size enrollment head file, one
    ///   for the admission and one for its authorization.
    ///
    /// Fail-before is executable and in the same test: the identical work with
    /// the fast path selectively disabled must violate every one of those
    /// bounds.
    #[test]
    fn promoted_admissions_are_bounded_at_one_one_thousand_and_ten_thousand() {
        const HEAD_READS_PER_ADMISSION: usize = 2;
        let mut fixture = Fixture::new(
            "bounded",
            vec![("pages/seed.md".into(), b"- seed\n".to_vec())],
        );
        let root = fixture.enrollment_root("bounded");
        let paths = PromotedPaths::new(&fixture, "bounded");
        let (mut authority, mut runtime) = promote(&mut fixture, &root, SessionId::new(), &paths);

        let bounded = |count: usize| PromotedRuntimeInstrumentation {
            enrollment: super::super::enrollment::EnrollmentInstrumentation {
                record_reads: 0,
                head_reads: count * HEAD_READS_PER_ADMISSION,
                namespace_scans: 0,
                directory_opens: 0,
                lease_acquisitions: 0,
                authority_claim_reads: 0,
            },
            sqlite_frontier_reads: 0,
            archive_identity_reads: 0,
            // The archive-rooted workspace runtime lease is retained for the
            // whole promoted runtime, so no admission ever reacquires it.
            workspace_lease_acquisitions: 0,
        };

        for (label, count) in [
            ("one", 1_usize),
            ("thousand", 1_000),
            ("ten-thousand", 10_000),
        ] {
            let measured = measure_admissions(&fixture, &mut authority, &mut runtime, count);
            assert_eq!(
                measured,
                bounded(count),
                "{label} unchanged-head admissions were not bounded"
            );
        }

        // Real post-bootstrap batches advance the durable history; the
        // per-admission bound is unchanged by them.
        for seed in [0xB100_u128, 0xB200, 0xB300] {
            append_local_batch(&fixture, &mut authority, &mut runtime, seed);
        }
        assert_eq!(
            measure_admissions(&fixture, &mut authority, &mut runtime, 1_000),
            bounded(1_000),
            "admission cost must not depend on how many batches the lineage has"
        );

        // Fail-before: the same admission with the fast path disabled.
        let before = PromotedRuntimeInstrumentation::capture();
        {
            let _full = runtime
                .admit_promoted_mutation_at_full_depth_for_test(&mut authority, &fixture.graph)
                .unwrap();
        }
        let full = before.since();
        assert!(
            full.sqlite_frontier_reads > 0
                && full.archive_identity_reads > 0
                && full.enrollment.record_reads > 0
                && full.enrollment.namespace_scans > 0
                && full.enrollment.authority_claim_reads > 0,
            "the disabled-fast-path control must violate the bound: {full:?}"
        );
        assert_eq!(
            full.enrollment.lease_acquisitions, 0,
            "even the unabridged proof runs on the retained lease"
        );
        fixture.assert_graph_unchanged();
    }

    /// A live window holds no authority to adopt a new enrollment state. If the
    /// committed head moves while it is open, it refuses before any graph or
    /// durable write.
    #[test]
    fn a_window_whose_enrollment_head_moved_refuses_before_any_write() {
        let mut fixture = Fixture::new(
            "stale-window",
            vec![("pages/stale.md".into(), b"- stale\n".to_vec())],
        );
        let root = fixture.enrollment_root("stale-window");
        let binding = fixture.enrollment_binding();
        let paths = PromotedPaths::new(&fixture, "stale-window");
        let (mut authority, mut runtime) = promote(&mut fixture, &root, SessionId::new(), &paths);
        append_local_batch(&fixture, &mut authority, &mut runtime, 0xB400);
        let history_before = runtime.engine().durable_history_authority().unwrap();

        let head_path = enrollment_head_path(&root, &binding);
        let committed = fs::read(&head_path).unwrap();
        let session = runtime
            .admit_promoted_mutation(&mut authority, &fixture.graph)
            .unwrap();
        // A raw head substitution to a well-formed but different digest: the
        // retained session holds the exclusive lease, so nothing legal could
        // have written this.
        fs::write(&head_path, format!("{}\n", "0".repeat(64))).unwrap();
        let error = session
            .admission()
            .authorize(&fixture.graph, runtime_engine_of(&session))
            .err()
            .expect("a window whose head moved must never authorize work");
        assert!(
            error
                .to_string()
                .contains("committed LocalActive head changed while a promoted window was live"),
            "unexpected stale-window refusal: {error}"
        );
        drop(session);
        fs::write(&head_path, &committed).unwrap();

        assert_eq!(
            runtime.engine().durable_history_authority().unwrap(),
            history_before,
            "a refused window must advance no durable history"
        );
        // The restored journal admits again, so the refusal was the head move.
        runtime
            .admit_promoted_mutation(&mut authority, &fixture.graph)
            .unwrap();
        fixture.assert_graph_unchanged();
    }

    /// SQLite is absent from admission, so its divergence must still be caught
    /// where it now exclusively lives: the open/recovery boundary and the
    /// drain. Deleting the whole device-local projection is the strongest
    /// available divergence, and a fresh-process reopen must rebuild it to the
    /// exact accepted frontier before any authority exists.
    #[test]
    fn sqlite_divergence_is_still_caught_at_the_open_boundary() {
        let mut fixture = Fixture::new(
            "sqlite-boundary",
            vec![("pages/db.md".into(), b"- db\n".to_vec())],
        );
        let root = fixture.enrollment_root("sqlite-boundary");
        let binding = fixture.enrollment_binding();
        let paths = PromotedPaths::new(&fixture, "sqlite-boundary");
        let session_id = SessionId::new();
        let (mut authority, mut runtime) = promote(&mut fixture, &root, session_id, &paths);
        for seed in [0xB500_u128, 0xB600] {
            append_local_batch(&fixture, &mut authority, &mut runtime, seed);
        }
        let frontier = runtime.engine().accepted_frontier_root().unwrap();
        assert_eq!(runtime.database().frontier_root().unwrap(), frontier);
        drop(runtime);
        drop(authority);

        for suffix in ["", "-wal", "-shm"] {
            let path = PathBuf::from(format!("{}{suffix}", paths.database_path.display()));
            let _ = fs::remove_file(path);
        }
        assert!(!paths.database_path.exists());

        let before = PromotedRuntimeInstrumentation::capture();
        let (_authority, reopened) =
            reopen_promoted_local_runtime(&root, &binding, session_id, &paths.open(&fixture))
                .unwrap();
        let boundary = before.since();
        assert!(
            boundary.sqlite_frontier_reads > 0,
            "the open boundary must prove the SQLite frontier"
        );
        assert!(
            boundary.archive_identity_reads > 0,
            "the open boundary must reread the archive claim and control identity"
        );
        assert_eq!(
            reopened.database().frontier_root().unwrap(),
            frontier,
            "a deleted projection must be rebuilt to the exact accepted frontier"
        );
        assert_eq!(
            reopened.engine().accepted_frontier_root().unwrap(),
            frontier
        );
        fixture.assert_graph_unchanged();
    }

    /// The promoted runtime owns the exclusive journal lease for its whole
    /// lifetime, and releases it exactly on drop.
    #[test]
    fn a_promoted_runtime_owns_the_journal_lease_and_releases_it_on_drop() {
        let mut fixture = Fixture::new(
            "lease",
            vec![("pages/lease.md".into(), b"- lease\n".to_vec())],
        );
        let root = fixture.enrollment_root("lease");
        let binding = fixture.enrollment_binding();
        let paths = PromotedPaths::new(&fixture, "lease");
        let (authority, runtime) = promote(&mut fixture, &root, SessionId::new(), &paths);

        // No second live session can write this journal.
        assert!(matches!(
            EnrollmentWriter::open_existing(&root, &binding),
            Err(crate::oplog::enrollment::EnrollmentError::LeaseContended(_))
        ));
        // Readers never contend, so the anchor stays reopenable while a
        // promoted runtime is live.
        let head = match EnrollmentReader::open_existing(&root, &binding).unwrap() {
            EnrollmentOpen::Present(reader) => reader.current().digest(),
            EnrollmentOpen::Absent => panic!("expected an enrollment head"),
        };
        assert_eq!(head, authority.enrollment_head());

        drop(runtime);
        drop(authority);
        assert!(matches!(
            EnrollmentWriter::open_existing(&root, &binding).unwrap(),
            EnrollmentOpen::Present(_)
        ));
        fixture.assert_graph_unchanged();
    }

    fn enrollment_head_path(
        root: &EnrollmentApplicationRoot,
        binding: &EnrollmentBindingV1,
    ) -> PathBuf {
        root.path()
            .join("sparse-storage")
            .join("v2")
            .join("local")
            .join(binding.graph_resource_id().to_string())
            .join("enrollment")
            .join("head")
    }
}
