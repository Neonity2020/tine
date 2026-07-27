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
//! Two runtime dependencies remain outside this packet and are deliberately not
//! faked:
//!
//! * The graph-text watcher event queue is owned by the Tauri watcher, so
//!   [`SAFE_HANDOFF_MISSING_DEPENDENCY`] blocks the production `Safe`
//!   transition after every core-checkable drain has been proved.
//! * An inactive-bootstrap archive is still fenced from ordinary runtime
//!   opening (`"inactive bootstrap history cannot be opened as ordinary
//!   runtime"`), so the enrolled runtime engine that [`LocalMutationPermit`]
//!   admits is not yet the engine the bootstrap published. Admission therefore
//!   authenticates the enrolled workspace/lineage/catalog/endpoint/device/
//!   receipt-store identity and refuses any engine behind the activated
//!   accepted frontier, rather than pinning one engine instance.

use std::fmt;

use crate::model::Graph;

use super::enrollment::{
    activate_verified_local_record, reopen_local_active_from_durable_state,
    reopen_local_active_record, transition_local_active_handoff, CommittedLocalActive,
    EnrollmentApplicationRoot, EnrollmentBindingV1, LocalActiveHandoff, LocalActiveSync,
    VerifiedLocalCompositionError, VerifiedLocalEvidence, VerifiedLocalProofSet,
};
use super::hot_engine::ShardedHotEngine;
use super::sqlite::{OpenProjection, SqliteFrontier, TailOverlay};
use super::{ContentDigest, ProjectionEndpointBinding, SessionId};

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

    /// Authenticate the live graph and engine against the committed enrollment
    /// binding. Nothing here reads SQLite rows or projection bytes.
    fn authenticate_runtime(
        &self,
        graph: &Graph,
        engine: &ShardedHotEngine,
    ) -> Result<(), LocalActivationError> {
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

    /// Derive the runtime admission that every new-architecture execution path
    /// requires.
    pub(crate) fn admission(&self) -> LocalRuntimeAdmission<'_> {
        LocalRuntimeAdmission {
            provenance: AdmissionProvenance::LocalActive(self),
        }
    }
}

/// The value every new-architecture local mutation, projection, import, and
/// coordinator execution path requires.
///
/// The only production constructor is [`LocalMutationPermit::admission`], so
/// future Tauri wiring cannot reach a writable runtime without first minting a
/// [`LocalActiveAuthority`].
pub(crate) struct LocalRuntimeAdmission<'a> {
    provenance: AdmissionProvenance<'a>,
}

enum AdmissionProvenance<'a> {
    LocalActive(&'a LocalMutationPermit<'a>),
    UnenrolledPreActivation,
}

impl LocalRuntimeAdmission<'_> {
    /// The pre-activation escape hatch retained only for the crate-private
    /// deterministic scenario corpus and the coordinator/session/import
    /// regressions that predate enrollment. Those fixtures build engines
    /// directly instead of through a bootstrap publication, so no genuine
    /// authority is constructible for them yet.
    ///
    /// It stays `pub(crate)`, so app startup and Tauri cannot reach it: outside
    /// this crate the only way to obtain an admission remains a live
    /// [`LocalActiveAuthority`] permit.
    pub(crate) const fn unenrolled_pre_activation() -> Self {
        Self {
            provenance: AdmissionProvenance::UnenrolledPreActivation,
        }
    }

    /// Revalidate the live runtime against the enrolled binding immediately
    /// before work is admitted.
    pub(crate) fn authorize(
        &self,
        graph: &Graph,
        engine: &ShardedHotEngine,
    ) -> Result<(), LocalActivationError> {
        match &self.provenance {
            AdmissionProvenance::LocalActive(permit) => {
                permit.authority.authenticate_runtime(graph, engine)
            }
            AdmissionProvenance::UnenrolledPreActivation => Ok(()),
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

#[cfg(test)]
mod tests;
