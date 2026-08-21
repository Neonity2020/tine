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
//! [`super::sqlite::WorkspaceRuntimeLease`] for its entire writable life, and
//! owns it inseparably from the device-local database it authorized (see
//! [`super::sqlite::LeasedWorkspaceProjection`]), so the database handle and the
//! workspace authority cannot drift apart.
//!
//! The lease is taken before any archive, engine, SQLite, or enrollment state
//! may *become authority* — which is a weaker and more accurate claim than
//! "before any of it is read". A restarted process holds no capability at all,
//! so [`reopen_promoted_local_runtime`] necessarily reads the enrollment chain,
//! takes the device-local enrollment lease, and reads the durable promoted-runtime
//! state before it can even know which archive and workspace to lease. None of
//! those pre-lease reads can authorize anything: the promotion state is
//! published immutably-once and is compared byte-for-byte against the durable
//! file again under the lease by `authorize_promoted_lineage`, the archive
//! identity is re-derived by `authenticate_archive_identity`, and the bootstrap
//! anchor is reread from the chain by [`require_unchanged_bootstrap_anchor`]
//! after the swap. Everything this runtime *writes* happens under the lease.
//!
//! The lock is never released across a bootstrap -> promoted database handoff.
//! That is a property of the construction, not of a test assembly:
//! [`InactiveBootstrapRuntimeSession`] is the crate's inactive-bootstrap open,
//! and its `promote` consumes the session, so there is no way to reach phase two
//! of promotion except on the exact lease the bootstrap database was opened
//! under. What is *not* yet true is that a running Tine binary executes it:
//! nothing in this module is reachable from application startup, a watcher, or
//! Tauri (see the module comments in `oplog/mod.rs`). Tests drive the same
//! construction the activation wiring will call; they do not stand in for it.
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
//! two racing newcomers cannot both win. It never mints `Safe`. Automatic
//! external import remains fenced until the actor-owned exact feed completes
//! and revalidates one full recovery catch-up, or until a later clean `Safe`
//! handoff is adopted.
//!
//! ## Workspace lease identity
//!
//! Holding the lock is not by itself proof of ownership, because the lock file
//! lives inside the replicated archive. A provider or user action that replaces
//! `<archive>/.tine-runtime/sqlite-workspaces/<workspace>/sqlite-applier.lock`
//! — a receive-only revert, a folder reset/re-add, a delete-then-restore, a
//! `.stversions` restore, `rm -rf .tine-runtime` — unlinks the locked file and
//! puts a new one at the same name. The old holder keeps a lock nobody can
//! reach by name; a newcomer opening the name locks the new file and succeeds.
//!
//! [`super::sqlite::WorkspaceRuntimeLease`] therefore binds itself to one
//! platform-native stable file identity at acquisition and revalidates it
//! while held. This is the exact, and deliberately smallest, set of places that
//! revalidation happens; every one of them is a boundary this runtime already
//! re-derives archive, enrollment, or head authority at:
//!
//! 1. **Acquisition.** `WorkspaceRuntimeLease::acquire` compares the locked
//!    handle against a fresh no-follow resolution of the exact pathname, with a
//!    small bounded number of explicit retries, then fails closed as
//!    `LeaseContended`.
//! 2. **Every database opened under the lease.** `SqliteApplierSlot::authorize`,
//!    reached from `DatabaseApplierLease::acquire` — so the inactive bootstrap
//!    open, its retry [`InactiveBootstrapRuntimeSession::reopen_under`], and the
//!    promoted open inside [`mint_promoted_runtime`] all revalidate.
//! 3. **Every use of the lease as a proof.**
//!    `WorkspaceRuntimeProof::authorize_archive`, which is what
//!    [`RetainedWorkspaceLease::into_lease`] consumes at the bootstrap ->
//!    promoted handover and what
//!    `RetainedEnrollmentSession::authenticate_unsafe_predecessor` consumes
//!    before the crash-takeover compare-and-swap.
//! 4. **Every archive-identity reread.** [`PromotedLocalRuntime::prove_binding`]
//!    revalidates exactly when it rereads the archive's canonical resource claim
//!    and control-directory identity — that is, at every
//!    [`BindingProofDepth::Boundary`] proof (the promoted-open/recovery final
//!    proof) and at every admission whose enrollment binding generation moved.
//! 5. **Every SQLite advance.** [`PromotedRuntimeSession::drain_projection`].
//! 6. **The promoted `Safe` handoff**, before the drain proof is believed.
//!
//! 7. **Every handout of the mutable SQLite/tail parts.**
//!    [`PromotedRuntimeSession::parts`], which is the shape both the
//!    operational coordinator and the reconciliation session take.
//! 8. **Every authority-changing coordinator boundary**: immutable publication,
//!    every bounded accepted-history archive-stage slice, tail admission, the
//!    SQLite drain, and each manifested Markdown projection step, through
//!    [`LocalRuntimeAdmission::reprove_workspace_authority`].
//!
//! It deliberately does **not** run in the per-mutation admission fast path.
//! Lease identity is a stable session fact of exactly the same class as the
//! archive control-directory identity, and it is carried under the identical
//! rule: [`ArchiveAuthentication::Carried`] carries it, any observed
//! enrollment-head change escalates to [`ArchiveAuthentication::Reread`] and
//! re-derives it. So an unchanged-head admission issues no filesystem call for
//! it, which `bounded_admission`'s zero-cost table asserts at 1, 1,000, and
//! 10,000 admissions.
//!
//! ## Revocation is terminal, and deliberately not self-healing
//!
//! The first failed revalidation at *any* of those boundaries latches
//! [`RuntimeRevocation`] on the runtime, one way and forever. Every later
//! admission, mutation window, projection drain, mutable-part handout, window
//! authorization, coordinator phase proof, and `Safe` handoff then refuses
//! from the latch, without reusing a proof this runtime carried from before the
//! replacement — which is exactly the hole a carried
//! [`ArchiveAuthentication::Carried`] admission would otherwise leave open
//! after a failed boundary proof.
//!
//! It is deliberately not recoverable in place. A replaced lease pathname means
//! another process may legitimately own the archive now, so "recovering" would
//! mean two appliers. Recovery is a fresh reopen or crash takeover, which
//! contends for the lease honestly through
//! [`reopen_promoted_local_runtime`]/[`take_over_promoted_local_runtime`].
//!
//! The honest residual: one batch authored inside an already-open mutation
//! window can still be *published* to the archive after a replacement, because
//! immutable content-addressed publication is not lease-gated at all (honest
//! peers on other devices publish into the same archive by design). The
//! coordinator's own publication boundary reproves first, so the *coordinator*
//! cannot publish after a replacement; a hand-rolled caller holding a live
//! window still can. What the lease gates — opening a device-local database,
//! advancing SQLite, admitting to the tail, writing manifested Markdown,
//! swapping a crash-takeover handoff record, and publishing `Safe` — all fail
//! closed first.
//!
//! ## Lock order, and why it is safe to invert it without blocking
//!
//! The global order is enrollment lease, then archive-rooted workspace lease.
//! [`reopen_promoted_local_runtime`] and [`take_over_promoted_local_runtime`]
//! take it in that order. The bootstrap path deliberately **inverts** it:
//! [`InactiveBootstrapRuntimeSession::open`] takes the workspace lease first and
//! [`InactiveBootstrapRuntimeSession::promote`] takes the enrollment lease
//! afterwards, because the workspace lease has to be continuously held across
//! the database handoff.
//!
//! That inversion cannot deadlock for exactly one reason: **both acquisitions
//! are non-blocking.** `fs2::try_lock_exclusive` under `WorkspaceRuntimeLease`
//! and under `EnrollmentLease` both return immediately, and contention becomes
//! an immediate refusal rather than a wait. The identity retry added above is
//! bounded and explicit for the same reason, and it releases the lock it took
//! before retrying.
//!
//! This is a standing contract for the activation, watcher, and Tauri phases:
//! **no future code may block, spin, or retry unboundedly while holding either
//! lease and waiting for the other.** If a retry is ever genuinely needed, bound
//! it explicitly and release the already-held lease first.
//!
//! The bootstrap stays an immutable historical anchor. Ordinary local batches
//! extend it by carrying the identical bootstrap aggregate binding forward, so
//! the promoted history is one homogeneous bootstrap-anchored lineage that the
//! durable promotion state explicitly authorizes — never a silently
//! reinterpreted inactive root and never mixed record bindings. Ancestry is
//! proved by the store-minted [`super::object_store::AuthenticatedEngineHistoryTransition`],
//! never by raw generation or acceptance sequence.
//!
//! # The retained-resume accelerator
//!
//! A promoted open may reuse the run-local engine state a previous session left
//! behind instead of replaying the whole authenticated history. The entire
//! lifecycle lives inside this module's sealed boundaries, and every step is
//! dominated by the one archive-rooted workspace lease this runtime holds:
//!
//! 1. **Open.** [`mint_promoted_runtime`] takes the lease, authenticates the
//!    archive, then — before anything is adopted — reproves the lease, asks the
//!    durable history store for a retention plan, and reads at most one
//!    published candidate through a duplicate of the *retained* archive
//!    capability. The enrollment evidence that read is admitted against is
//!    derived here from the retained authenticated session
//!    ([`resume_enrollment_admission`]), never supplied by a caller.
//! 2. **Selection.** Only a valid `ResumeAdoptionCandidate::Available` is
//!    offered to the engine. Never published, proof denied by residue or
//!    surplus, torn, stale, conflicted, binding-refused, still-leased — every
//!    other shape opens exactly as a fresh full replay would, without failing
//!    startup and without changing one candidate byte. An `Ephemeral` retention
//!    plan means the archive cannot currently prove a retained run collectable,
//!    so this open takes a disposable run and adds nothing to the population.
//! 3. **Unsafe publication.**
//!    [`PromotedLocalRuntime::publish_quiescent_resume_point`] and the narrower
//!    post-open/pre-first-mutation operation mint exact Unsafe-bound evidence.
//!    They run the same device-local drain proof as the Safe transaction and
//!    reprove the lease immediately before the snapshot.
//! 4. **Safe transaction.** [`PromotedLocalRuntime::quiesce_and_mark_safe`]
//!    holds graph and watcher barriers through drain proof, clear-before-Safe,
//!    durable Safe commit/readback, and report-only Safe-bound publication.
//! 5. **Reclamation.** Only the sealed `PublishedResumePoint` a successful
//!    publication mints authorizes deletion, and the pass reproves the lease
//!    once more first. This is the only boundary in this module that can delete
//!    archive bytes. Packed Patricia scanning is enabled only for a caller's
//!    explicit quiescent publication or after durable Safe; the automatic
//!    post-open publication never scans those directories.
//!
//! The whole thing is an accelerator, so it has no `Err` reaching a caller's
//! control flow: [`ResumePublicationStatus`] and [`RuntimeResumeOpenStatus`] are
//! typed, diagnosable status. A full replay is always available and always
//! correct, and a publication or maintenance failure must never block an
//! otherwise valid `Unsafe -> Safe` handoff.
//!
//! Neither status vends a scan, a reachability proof, a record, or any deletion
//! surface, and neither is read by ordinary admission: the accelerator is
//! absent from successful keystroke, authoring, and acceptance paths. The one
//! exceptional boundary is a deferred catalog authentication refusal, which
//! refuses that mutation, fully replays immutable history into a fresh run,
//! and publishes its replacement so the damaged run cannot be re-adopted.
//!
//! A proven pathname/file-identity replacement latches a terminal
//! [`RuntimeRevocation`]; inability to perform an identity check refuses only
//! that operation and remains retryable. Typed refusals distinguish the two,
//! and [`PromotedLocalRuntime::workspace_authority_revocation`] gives a future
//! startup/UI facade the terminal boundary and cause needed to reopen/take over
//! automatically. Core deliberately does not implement that facade.
//!
//! Every new-architecture mutation, projection, import, coordinator, and
//! reconciliation path requires a [`LocalRuntimeAdmission`] whose only
//! production source is [`PromotedRuntimeAdmission::admission`], which is itself
//! derived from *both* a live [`LocalActiveAuthority`] and the exact
//! [`PromotedLocalRuntime`].

#[cfg(test)]
use std::collections::BTreeMap;
use std::fmt;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::time::Duration;

use crate::model::Graph;

use super::enrollment::{
    CommittedLocalActive, LocalActiveSync, RetainedEnrollmentSession,
    VerifiedLocalCompositionError, VerifiedLocalEvidence,
};
use super::hot_engine::{EngineError, LocalAuthorGeneration, ShardedHotEngine};
use super::object_store::{EngineHistoryAuthority, PromotedRuntimeStateV1, StoreError};
use super::shadow_projection::BootstrapProjectionAuthority;
use super::sqlite::{
    LeasedWorkspaceProjection, ProjectionError, SqliteFrontier, TailOverlay, WorkspaceLeaseIdentity,
};
use super::{
    ContentDigest, DeviceId, ObjectStore, ProjectionEndpointBinding, SessionId, WorkspaceId,
};

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

/// Test-only phase receipt for the promoted-runtime portion of an existing
/// managed open. It is keyed by workspace because the actor thread emits it
/// and the benchmark caller consumes it after the thread joins.
#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(crate) struct PromotedRuntimeOpenInstrumentation {
    pub(crate) total: Duration,
    pub(crate) bootstrap_anchor: Duration,
    pub(crate) enrollment_session: Duration,
    pub(crate) promotion_state: Duration,
    pub(crate) mint: Duration,
    pub(crate) handoff_and_final_proof: Duration,
    pub(crate) bootstrap_projection: Duration,
    pub(crate) bootstrap_runtime_authority: Duration,
    pub(crate) resume_candidate: Duration,
    pub(crate) reconstructed_bootstrap_resume: bool,
    pub(crate) reconstructed_ephemeral_bootstrap: bool,
    pub(crate) engine_open: Duration,
    pub(crate) sqlite_open: Duration,
    pub(crate) tail_construction: Duration,
    /// Which branch the disposable SQLite projection actually took at open, and
    /// what the rebuild did if it rebuilt. `sqlite_open` alone cannot distinguish
    /// "reopened a valid projection slowly" from "threw it away and rebuilt the
    /// whole graph", and those have opposite fixes.
    pub(crate) projection_recovery: &'static str,
    pub(crate) projection_rebuild_reason: String,
    pub(crate) projection_applied_batches: usize,
    pub(crate) projection_bulk_pages_materialized: usize,
    pub(crate) projection_ancestry_full_scans: usize,
    /// The rebuild's own counters, carried whole rather than field by field.
    /// Which term carries a superlinear rebuild is not known in advance, so
    /// copying three of them forces a source change every time the search moves.
    pub(crate) projection_rebuild_counters: super::sqlite::RebuildInstrumentation,
    pub(crate) engine: super::hot_engine::EnrolledProjectionOpenInstrumentation,
    pub(crate) engine_stages: super::hot_engine::EngineOpenStageBreakdown,
}

#[cfg(test)]
static PROMOTED_RUNTIME_OPEN_INSTRUMENTATION: Mutex<
    BTreeMap<WorkspaceId, PromotedRuntimeOpenInstrumentation>,
> = Mutex::new(BTreeMap::new());

#[cfg(test)]
pub(crate) fn reset_promoted_runtime_open_instrumentation(workspace: WorkspaceId) {
    PROMOTED_RUNTIME_OPEN_INSTRUMENTATION
        .lock()
        .unwrap()
        .remove(&workspace);
}

#[cfg(test)]
pub(crate) fn take_promoted_runtime_open_instrumentation(
    workspace: WorkspaceId,
) -> PromotedRuntimeOpenInstrumentation {
    PROMOTED_RUNTIME_OPEN_INSTRUMENTATION
        .lock()
        .unwrap()
        .remove(&workspace)
        .expect("promoted runtime timing was recorded")
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
    control_detail: &'static str,
) -> Result<(), RuntimePromotionError> {
    #[cfg(test)]
    count(&PROMOTED_ARCHIVE_IDENTITY_READS);
    archive
        .validate_enrolled_archive_resource_id(state.archive_resource_id)
        .map_err(StoreError::Io)
        .map_err(RuntimePromotionError::Store)?;
    if archive.canonical_archive_identity()?.binding_digest() != state.archive_control_binding {
        return Err(RuntimePromotionError::Anchor(control_detail));
    }
    Ok(())
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

/// Opaque device-local write authority for one enrolled graph.
///
/// Deliberately not `Clone`, not `Copy`, not `Debug`-transparent, and neither
/// serializable nor deserializable. Every field is private and every accessor
/// returns copied evidence, never the authority itself.
pub(crate) struct LocalActiveAuthority {
    evidence: VerifiedLocalEvidence,
    session_id: SessionId,
    enrollment_head: ContentDigest,
    verification_digest: ContentDigest,
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
    Promoted(&'a PromotedRuntimeAdmission<'a>),
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
            AdmissionProvenance::Promoted(admission) => admission.authorize_engine(graph, engine),
            AdmissionProvenance::Clean(admission) => admission.authorize_engine(graph, engine),
            #[cfg(test)]
            AdmissionProvenance::UnenrolledPreActivation => {
                // A promoted engine is a real activated user graph. The
                // pre-activation hatch exists only for fixtures whose engines
                // were never enrolled through a bootstrap publication, so
                // offering it a promoted runtime is always a construction
                // error, never a fallback.
                if engine.promoted_lineage().is_some() {
                    return Err(RuntimePromotionError::Activation(
                        LocalActivationError::RuntimeBinding(
                            "the unenrolled pre-activation admission cannot authorize a promoted \
                             runtime engine"
                                .into(),
                        ),
                    ));
                }
                Ok(())
            }
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
            AdmissionProvenance::Promoted(admission) => (
                admission.permit.endpoint(),
                admission.state.workspace_id,
                admission.permit.session_id(),
            ),
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
            AdmissionProvenance::Promoted(admission) => admission.reprove(boundary),
            AdmissionProvenance::Clean(admission) => admission.reprove(boundary),
            #[cfg(test)]
            AdmissionProvenance::UnenrolledPreActivation => Ok(()),
        }
    }
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
    bootstrap_projection: &'a BootstrapProjectionAuthority,
}

impl PromotedRuntimeSession<'_> {
    // There is deliberately no `database()` or `tail()` here, in any form. The
    // SQLite applier handle and the bounded tail are the
    // one-applier-per-workspace write, so the only way to reach them is
    // [`Self::parts`], which proves the archive-rooted lease immediately first.
    // `the_promoted_window_vends_no_infallible_applier_handle` keeps it that
    // way; a test-only escape hatch would be one `#[cfg(test)]` deletion away
    // from being the production hole again.

    /// The complete admitted runtime, borrowed disjointly.
    ///
    /// This is the shape both real consumers take — [`super::operational_coordinator::OperationalCoordinator::execute`]
    /// and [`super::reconciliation_session::ReconciliationSessionDependencies`] —
    /// so it is an authority boundary, not an accessor: handing out
    /// `&mut SqliteFrontier` and `&mut TailOverlay` is handing out the
    /// one-applier-per-workspace write. It therefore re-derives the lease
    /// identity first; proven replacement latches terminal revocation, while
    /// inability to perform the check refuses only this call.
    ///
    /// The returned [`LocalRuntimeAdmission`] carries the same probe forward, so
    /// the coordinator can reprove at each of its own authority-changing
    /// boundaries without a second capability.
    pub(crate) fn parts(
        &mut self,
    ) -> Result<
        (
            LocalRuntimeAdmission<'_>,
            &mut ShardedHotEngine,
            &mut SqliteFrontier,
            &mut TailOverlay,
        ),
        WorkspaceAuthorityRefusal,
    > {
        self.admission
            .reprove(WorkspaceAuthorityBoundary::MutableParts)?;
        Ok((
            LocalRuntimeAdmission {
                provenance: AdmissionProvenance::Promoted(&self.admission),
            },
            self.engine,
            self.database,
            self.tail,
        ))
    }

    pub(crate) fn parts_with_bootstrap(
        &mut self,
    ) -> Result<
        (
            LocalRuntimeAdmission<'_>,
            &mut ShardedHotEngine,
            &mut SqliteFrontier,
            &mut TailOverlay,
            &BootstrapProjectionAuthority,
        ),
        WorkspaceAuthorityRefusal,
    > {
        self.admission
            .reprove(WorkspaceAuthorityBoundary::MutableParts)?;
        Ok((
            LocalRuntimeAdmission {
                provenance: AdmissionProvenance::Promoted(&self.admission),
            },
            self.engine,
            self.database,
            self.tail,
            self.bootstrap_projection,
        ))
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
    /// The while-held identity check for the archive-rooted workspace lease
    /// that authorized this runtime's database, borrowed disjointly from it so
    /// every authority-changing boundary can still re-prove ownership while the
    /// database and tail are exclusively borrowed elsewhere.
    workspace: WorkspaceLeaseIdentity<'a>,
    /// This runtime's terminal revocation latch, shared so a boundary that only
    /// holds the admission can still kill the whole runtime.
    revocation: &'a RuntimeRevocationLatch,
    _seal: seal::Seal,
}

impl PromotedRuntimeAdmission<'_> {
    /// Re-derive archive-rooted workspace authority immediately before one
    /// authority-changing boundary. Proven replacement latches terminal
    /// revocation; a check that cannot be performed remains retryable.
    ///
    /// One held-handle stat plus one no-follow resolution of the exact lease
    /// pathname. It is never on the per-mutation admission path.
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
        // Terminal first: a window whose runtime was revoked while it was live
        // authorizes nothing, and in particular does not re-derive authority
        // from the `Carried` archive facts it was minted with.
        self.revocation
            .guard(WorkspaceAuthorityBoundary::WindowAuthorization)?;
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
        BlockLocation, DocumentId, LineageDigest, OperationTransaction, ProjectionEndpointId,
        ReferenceCatalogPolicyV1, SemanticOperation,
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
            let mut session = self.runtime.admit_clean_mutation(&self.graph).unwrap();
            let state = OperationalCoordinator::execute_clean_local(
                &mut session,
                &self.graph,
                &self.receipts,
                &transaction,
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
