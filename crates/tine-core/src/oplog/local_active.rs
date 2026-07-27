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
    LocalActiveSync, PromotedBootstrapAnchor, VerifiedLocalCompositionError, VerifiedLocalEvidence,
    VerifiedLocalProofSet,
};
use super::hot_engine::{EngineError, ShardedHotEngine};
use super::object_store::{
    EngineHistoryAuthority, PromotedLineageModeV1, PromotedRuntimeStateV1, StoreError,
    PROMOTED_RUNTIME_STATE_SCHEMA_VERSION,
};
use super::projection_store::ProjectionReceiptStore;
use super::sqlite::{
    ApplicationRuntimeRoot, OpenProjection, ProjectionClaim, ProjectionError, RebuildSource,
    SqliteFrontier, TailOverlay,
};
use super::{ContentDigest, ObjectStore, ProjectionEndpointBinding, SessionId};

/// A private seal. Sibling modules can name the sealed types but can never
/// construct one, because this module is the only place `Seal` is reachable.
mod seal {
    #[derive(Debug)]
    pub(super) struct Seal;
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

    /// Quiesce every device-local drain and, when all of them can be proved,
    /// persist `Safe` and return a typed safe-handoff permit.
    ///
    /// This packet cannot observe the watcher event queue, so the transition is
    /// explicitly unavailable with the exact missing dependency instead of
    /// minting a `Safe` state that is not true. Every other invariant is fully
    /// checked and revalidated after the drain.
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
    engine: ShardedHotEngine,
    projection: OpenProjection,
    tail: TailOverlay,
    _seal: seal::Seal,
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
        &self.projection.database
    }

    pub(crate) const fn projection(&self) -> &OpenProjection {
        &self.projection
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
        self.revalidate(graph, authority)?;
        // `admit_local_mutation` durably moves a committed `Safe` handoff back
        // to `Unsafe { session }` before the permit exists, so no promoted write
        // is ever accepted while the persisted state claims a clean handoff.
        let permit = authority.admit_local_mutation(graph, &self.engine)?;
        let admission = PromotedRuntimeAdmission {
            permit,
            state: self.state.clone(),
            anchor: self.anchor,
            engine_authority: self.engine.runtime_authority().clone(),
            _seal: seal::Seal,
        };
        Ok(PromotedRuntimeSession {
            admission,
            engine: &mut self.engine,
            database: &mut self.projection.database,
            tail: &mut self.tail,
        })
    }

    /// Re-prove the complete promoted binding, including the device-local
    /// SQLite frontier, against live durable state.
    fn revalidate(
        &self,
        graph: &Graph,
        authority: &LocalActiveAuthority,
    ) -> Result<(), RuntimePromotionError> {
        if authority.session_id != self.session_id
            || authority.verification_digest != self.verification_digest
        {
            return Err(RuntimePromotionError::Anchor(
                "live authority is not this promoted runtime's session",
            ));
        }
        let accepted = revalidate_promoted_binding(
            &self.state,
            self.anchor,
            self.engine.runtime_authority(),
            graph,
            &self.engine,
            authority,
        )?;
        if self.projection.database.frontier_root()? != accepted {
            return Err(RuntimePromotionError::Anchor(
                "promoted SQLite frontier is not the current accepted frontier",
            ));
        }
        if self.endpoint.endpoint_id() != authority.endpoint.endpoint_id()
            || self.endpoint.device_id() != authority.endpoint.device_id()
            || self.endpoint.graph_resource_id() != authority.endpoint.graph_resource_id()
        {
            return Err(RuntimePromotionError::Anchor(
                "promoted endpoint binding is not the authority's enrolled endpoint",
            ));
        }
        Ok(())
    }
}

/// Re-prove the complete promoted binding against live durable state, and
/// return the authenticated current accepted frontier.
///
/// Cost is one archive-resource claim read, one archive control-identity stat,
/// one durable head read, one shared-radix insertion-only walk bounded by the
/// changed paths, one engine frontier read, and one bounded enrollment head
/// reopen. Nothing here scans lifetime history, the enrollment chain, or graph
/// text.
fn revalidate_promoted_binding(
    state: &PromotedRuntimeStateV1,
    anchor: EngineHistoryAuthority,
    engine_authority: &super::hot_engine::EngineAuthority,
    graph: &Graph,
    engine: &ShardedHotEngine,
    authority: &LocalActiveAuthority,
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
    // Archive resource claim and physical archive control identity.
    let archive = engine.archive_store().ok_or(RuntimePromotionError::Anchor(
        "promoted engine retained no archive capability",
    ))?;
    archive
        .validate_enrolled_archive_resource_id(state.archive_resource_id)
        .map_err(|error| {
            RuntimePromotionError::Activation(LocalActivationError::RuntimeBinding(format!(
                "promoted archive resource claim no longer authenticates: {error}"
            )))
        })?;
    if archive.canonical_archive_identity()?.binding_digest() != state.archive_control_binding {
        return Err(RuntimePromotionError::Anchor(
            "promoted archive control directory was substituted",
        ));
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
    // Committed enrollment verification digest, session, and head.
    let committed = authority.reopen_current()?;
    if committed.verification_digest() != state.enrollment_verification_digest
        || committed.binding() != binding
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
    _seal: seal::Seal,
}

impl PromotedRuntimeAdmission<'_> {
    fn authorize_engine(
        &self,
        graph: &Graph,
        engine: &ShardedHotEngine,
    ) -> Result<(), RuntimePromotionError> {
        revalidate_promoted_binding(
            &self.state,
            self.anchor,
            &self.engine_authority,
            graph,
            engine,
            self.permit.authority,
        )
        .map(|_| ())
    }
}

/// Phase two of promotion, same process: open the promoted writable runtime.
///
/// The caller must have released the retained inactive bootstrap projection
/// first; the one-per-workspace SQLite applier lease enforces that rather than
/// trusting it.
pub(crate) fn open_promoted_local_runtime(
    sealed: SealedRuntimePromotion,
    authority: &LocalActiveAuthority,
    open: &PromotedRuntimeOpen<'_>,
) -> Result<PromotedLocalRuntime, RuntimePromotionError> {
    let committed = authority.reopen_current()?;
    if committed.verification_digest() != sealed.state.enrollment_verification_digest
        || committed.session_id() != Some(authority.session_id)
    {
        return Err(RuntimePromotionError::Enrollment(
            VerifiedLocalCompositionError::StaleEvidence(
                "committed LocalActive record is not the promoting session's record",
            ),
        ));
    }
    mint_promoted_runtime(
        sealed.state,
        authority.session_id,
        authority.verification_digest,
        authority.endpoint,
        authority.evidence.binding(),
        open,
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
    let anchor = reopen_promoted_bootstrap_anchor(root, binding)?;
    // A competing session is refused before any archive, engine, SQLite, or
    // lease work happens, so a losing restart advances and touches nothing.
    if let LocalActiveHandoff::Unsafe {
        session_id: committed_session,
    } = anchor.committed().handoff()
    {
        if committed_session != session_id {
            return Err(RuntimePromotionError::Enrollment(
                VerifiedLocalCompositionError::CompetingSession,
            ));
        }
    }
    let state = read_promotion_state_for_anchor(open.archive_root, binding, &anchor)?;
    let runtime = mint_promoted_runtime(
        state,
        session_id,
        anchor.verification_digest(),
        promoted_storage_binding_endpoint(binding),
        binding,
        open,
    )?;

    // Only now, with complete recovery proved, may an authority exist. The
    // durable handoff protocol is the existing one: an `Unsafe { session }`
    // record reopens only for that exact session, and a clean `Safe` record is
    // durably moved to `Unsafe { requested session }` and freshly reopened
    // before any authority is minted.
    let (evidence, committed) = anchor.into_predecessor_evidence();
    let reopened = match committed.handoff() {
        LocalActiveHandoff::Unsafe {
            session_id: committed_session,
        } if committed_session == session_id => committed,
        LocalActiveHandoff::Unsafe { .. } => {
            return Err(RuntimePromotionError::Enrollment(
                VerifiedLocalCompositionError::CompetingSession,
            ));
        }
        LocalActiveHandoff::Safe => {
            transition_local_active_handoff(
                root,
                binding,
                committed.enrollment_head(),
                evidence.verification_digest(),
                LocalActiveHandoff::Unsafe { session_id },
            )?;
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
            let (_, fresh_committed) = fresh.into_predecessor_evidence();
            fresh_committed
        }
    };
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
    runtime.revalidate(open.graph, &authority)?;
    Ok((authority, runtime))
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
fn mint_promoted_runtime(
    state: PromotedRuntimeStateV1,
    session_id: SessionId,
    verification_digest: ContentDigest,
    expected_endpoint: ProjectionEndpointBinding,
    binding: &EnrollmentBindingV1,
    open: &PromotedRuntimeOpen<'_>,
) -> Result<PromotedLocalRuntime, RuntimePromotionError> {
    if state.enrollment_verification_digest != verification_digest {
        return Err(RuntimePromotionError::Anchor(
            "promotion state does not bind this LocalActive verification digest",
        ));
    }
    let archive = ObjectStore::open(open.archive_root, state.workspace_id)?;
    archive
        .validate_enrolled_archive_resource_id(state.archive_resource_id)
        .map_err(|error| {
            RuntimePromotionError::Activation(LocalActivationError::RuntimeBinding(format!(
                "promoted archive resource claim does not authenticate: {error}"
            )))
        })?;
    if archive.canonical_archive_identity()?.binding_digest() != state.archive_control_binding {
        return Err(RuntimePromotionError::Anchor(
            "promoted archive control directory identity changed",
        ));
    }
    // The retained immutable publication and the durable reference-catalog
    // authority the cold record binds must both be present before the enrolled
    // open recovers from them.
    require_promoted_bootstrap_runtime_authority(&archive, &state)?;

    // Recovery replays the archive's committed manifests. This is the existing
    // enrolled-recovery cost and reads no graph text.
    let committed_manifests = archive.committed_manifests()?;
    let anchor = state.anchor_authority();
    let (engine, _outcomes) = ShardedHotEngine::open_promoted_projection(
        archive,
        state.lineage_digest,
        state.catalog_document_id,
        open.graph,
        open.receipts,
        &state,
        &committed_manifests,
    )?;
    if engine.promoted_lineage() != Some(&state) {
        return Err(RuntimePromotionError::Anchor(
            "promoted engine did not adopt the exact durable promotion state",
        ));
    }
    if engine.workspace_id() != binding.workspace_id()
        || engine.lineage_digest() != binding.lineage_digest()
        || engine.catalog_document_id() != binding.catalog_document_id()
        || engine.projection_receipt_store_id() != Some(binding.receipt_store_id())
    {
        return Err(RuntimePromotionError::Anchor(
            "promoted engine does not authenticate the enrolled binding",
        ));
    }
    let endpoint = engine
        .projection_endpoint_binding()
        .ok_or(RuntimePromotionError::Anchor(
            "promoted engine has no projection endpoint enrollment",
        ))?;
    if endpoint.endpoint_id() != expected_endpoint.endpoint_id()
        || endpoint.device_id() != expected_endpoint.device_id()
        || endpoint.graph_resource_id() != expected_endpoint.graph_resource_id()
        || endpoint.endpoint_id() != binding.endpoint_id()
        || endpoint.device_id() != binding.device_id()
        || endpoint.graph_resource_id() != binding.graph_resource_id()
    {
        return Err(RuntimePromotionError::Anchor(
            "promoted engine projection endpoint is not the enrolled endpoint",
        ));
    }
    // The recovered history must be exactly, or insertion-only descended from,
    // the exact bootstrap anchor.
    let transition = engine.authenticate_history_descends_from(anchor)?;
    if transition.before() != anchor || transition.after() != engine.durable_history_authority()? {
        return Err(RuntimePromotionError::Anchor(
            "recovered history is not an authenticated descendant of the bootstrap anchor",
        ));
    }
    let accepted = engine
        .accepted_frontier_root()
        .map_err(RuntimePromotionError::Engine)?;
    if accepted.acceptance_sequence() < state.anchor_acceptance_sequence {
        return Err(RuntimePromotionError::Anchor(
            "recovered accepted frontier is behind the bootstrap anchor",
        ));
    }
    // The projection-work index and reference/catalog authority must both be
    // live before the SQLite projection is opened at the current frontier.
    engine
        .projection_work_index()
        .map_err(RuntimePromotionError::Engine)?;
    engine
        .reference_catalog_root()
        .map_err(RuntimePromotionError::Engine)?;

    let store = engine.archive_store().ok_or(RuntimePromotionError::Anchor(
        "promoted engine retained no archive capability",
    ))?;
    let claim = ProjectionClaim::current(state.workspace_id, state.lineage_digest);
    // A promoted lineage's leading accepted sequences are its retained
    // immutable bootstrap parts, which live in the archive's bootstrap
    // namespace rather than the ordinary object namespace. The publication
    // identity comes from the authorized promotion state.
    let publication = store
        .load_bootstrap_publication(state.bootstrap.publication_id())
        .map_err(RuntimePromotionError::Store)?;
    let projection = SqliteFrontier::open_or_rebuild(
        open.database_path,
        open.application_runtime_root,
        claim,
        RebuildSource::from_promoted_runtime(&engine, store, &publication)?,
    )?;
    if projection.database.frontier_root()? != accepted {
        return Err(RuntimePromotionError::Anchor(
            "promoted SQLite projection is not at the current accepted frontier",
        ));
    }
    let tail = TailOverlay::from_durable(
        &projection.database,
        &RebuildSource::from_promoted_runtime(&engine, store, &publication)?,
    )
    .map_err(|error| match error {
        super::sqlite::TailOverlayError::Projection(error) => RuntimePromotionError::Sqlite(error),
        other => RuntimePromotionError::Activation(LocalActivationError::RuntimeBinding(
            other.to_string(),
        )),
    })?;
    Ok(PromotedLocalRuntime {
        state,
        anchor,
        session_id,
        verification_digest,
        endpoint,
        engine,
        projection,
        tail,
        _seal: seal::Seal,
    })
}

#[cfg(test)]
mod tests;
