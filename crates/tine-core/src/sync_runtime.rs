//! Inactive single-owner host for an existing sparse-oplog local runtime.
//!
//! The only public capability is [`SyncRuntimeHandle`]. It is cloneable,
//! `Send + Sync`, and forwards bounded typed requests to one dedicated actor
//! thread. The actor constructs and retains the `Graph`, enrollment authority,
//! promoted runtime, exact external feed, receipt store, reconciliation
//! baseline, watcher owner, SQLite applier, and every continuation on that
//! thread. None of those capabilities can cross this module's public boundary.
//!
//! This module does not activate a graph, create an enrollment, repair state,
//! select itself during normal graph loading, or route legacy mutations. Only
//! an explicit [`SyncStorageProfile::ExperimentalLocal`] request whose
//! read-only discovery result is an authenticated existing `LocalActive` may
//! start an actor.

use std::fmt;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::model::Graph;
use crate::oplog::discovery::{
    discover_startup, AmbiguousEvidence, DiscoveryClassification, DiscoveryComponent,
    DiscoveryRequest, LocalActiveAdvisory, NonActiveStage, StartupStorageProfile,
};
use crate::oplog::enrollment::{
    open_existing_enrollment_application_root, EnrollmentDiscoveryHandoff,
};
use crate::oplog::exact_external_feed::{
    ExactExternalFeedDrain, ExactExternalFeedObserveError, ExactExternalFeedState,
};
use crate::oplog::hot_engine::ProjectionEndpointBinding;
use crate::oplog::local_active::{
    reopen_promoted_local_runtime_existing_projection,
    take_over_promoted_local_runtime_recovering_projection, LocalActiveAuthority,
    PromotedLocalRuntime, PromotedRuntimeOpen, RuntimeRecoveryState,
};
#[cfg(test)]
use crate::oplog::operational_coordinator::{fail_repeatedly_at, OperationalFaultPoint};
use crate::oplog::operational_coordinator::{
    LocalMutationBlockReason, LocalMutationCoordinatorState, LocalMutationRecovery,
    LocalPublishedContinuation, OperationalCoordinator, OperationalPhase,
};
use crate::oplog::projection_store::ProjectionReceiptStore;
use crate::oplog::reconciliation_baseline::{
    BaselineTimestamp, ReconciliationBaseline, ReconciliationBaselineBinding,
    TrustedPrivateApplicationRuntimeRoot,
};
use crate::oplog::sqlite::ApplicationRuntimeRoot;
use crate::oplog::watcher_queue::WatcherObservation;
use crate::oplog::{
    BatchId, BlockId, FrontierReferenceHit, LogseqUuid, ManagedPath, ManagedTextKind,
    MaterializedBlockRow, MaterializedEntityId, MaterializedPageRow, MaterializedPropertyRow,
    MaterializedSearchHit, MaterializedTagRow, MaterializedTaskRow, OperationTransaction, PageId,
    ReferenceFactV1, ReferenceSourceLocatorV1, SemanticOperation, SessionId,
    MAX_MATERIALIZATION_QUERY_BYTES, MAX_MATERIALIZATION_QUERY_ROWS,
};

const ACTOR_CHANNEL_CAPACITY: usize = 64;
const ACTOR_STACK_BYTES: usize = 16 * 1024 * 1024;
const MAX_WATCHER_OBSERVATIONS: usize = 256;
const MAX_WATCHER_PATH_BYTES: usize = 64 * 1024;
const MAX_CLEAN_DRAIN_TURNS: usize = 4096;
/// Maximum top-level operations plus nested rename rows in one submission.
pub const MAX_LOCAL_MUTATION_ROWS: usize = 1024;
/// Maximum managed-path references retained by one submission.
pub const MAX_LOCAL_MUTATION_REFERENCED_PATHS: usize = 512;
/// Maximum aggregate UTF-8 bytes in every referenced managed path.
pub const MAX_LOCAL_MUTATION_PATH_BYTES: usize = 256 * 1024;
/// Maximum aggregate UTF-8 bytes in names, content, preambles, and order keys.
pub const MAX_LOCAL_MUTATION_TEXT_BYTES: usize = 1024 * 1024;
/// The public boundary uses the materialization's proven row cap. Every query
/// validates this before it is placed on the actor queue.
pub const MAX_SYNC_RUNTIME_QUERY_ROWS: usize = MAX_MATERIALIZATION_QUERY_ROWS;
/// Bound aggregate request retention independently of any individual SQLite
/// predicate. This also bounds multi-field requests such as property filters.
pub const MAX_SYNC_RUNTIME_QUERY_BYTES: usize = MAX_MATERIALIZATION_QUERY_BYTES;

#[cfg(test)]
static ACTOR_THREADS_STARTED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static ACTOR_THREADS_FINISHED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Storage selection at the inactive facade boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncStorageProfile {
    LegacyDefault,
    ExperimentalLocal,
}

/// Explicit paths for one already-enrolled local runtime.
///
/// No path is inspected for `LegacyDefault`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncRuntimeOpenRequest {
    pub profile: SyncStorageProfile,
    pub graph_root: PathBuf,
    pub enrollment_root: PathBuf,
    pub archive_root: PathBuf,
    pub receipt_root: PathBuf,
    pub database_path: PathBuf,
    pub application_runtime_root: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncRuntimeComponent {
    Enrollment,
    Archive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncNonActiveStage {
    ShadowImport,
    VerifiedLocal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncAmbiguousEvidence {
    EnrollmentResidue,
    EnrollmentNamespace,
    EnrollmentGraphBinding,
    ArchiveResidue,
    ArchiveNamespace,
    ArchiveBinding,
    ActiveArchiveMismatch,
}

/// Result of startup discovery and, only for a valid active result, actor open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncRuntimeOpenStatus {
    LegacyDefault,
    Absent,
    ExistingNonActive(SyncNonActiveStage),
    Blocked { reason_code: String },
    UnsupportedOrIncompatible(SyncRuntimeComponent),
    CorruptOrUnreadable(SyncRuntimeComponent),
    AmbiguousOrForeignResidue(SyncAmbiguousEvidence),
    Active,
    OpenRefused { detail: String },
}

/// Startup returns typed status separately from the optional channel handle.
pub struct SyncRuntimeOpenResult {
    pub status: SyncRuntimeOpenStatus,
    pub handle: Option<SyncRuntimeHandle>,
}

impl fmt::Debug for SyncRuntimeOpenResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SyncRuntimeOpenResult")
            .field("status", &self.status)
            .field("has_handle", &self.handle.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncWatcherObservation {
    ManagedPath(ManagedPath),
    UnknownPath,
    NotifyError,
    RescanRequired,
}

impl SyncWatcherObservation {
    pub fn managed_path(path: impl Into<String>) -> Result<Self, SyncRuntimeRequestError> {
        ManagedPath::parse(path)
            .map(Self::ManagedPath)
            .map_err(|error| SyncRuntimeRequestError::InvalidRequest(error.to_string()))
    }

    fn retained_path_bytes(&self) -> usize {
        match self {
            Self::ManagedPath(path) => path.as_str().len(),
            Self::UnknownPath | Self::NotifyError | Self::RescanRequired => 0,
        }
    }

    fn into_core(self) -> WatcherObservation {
        match self {
            Self::ManagedPath(path) => WatcherObservation::ManagedPath(path),
            Self::UnknownPath => WatcherObservation::UnknownPath,
            Self::NotifyError => WatcherObservation::NotifyError,
            Self::RescanRequired => WatcherObservation::RescanRequired,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncRuntimeRecovery {
    FirstPromotion,
    ResumedOwnUnsafe,
    AdoptedSafeHandoff,
    TookOverCrashedUnsafe,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncRuntimeLifecycle {
    Active,
    Terminal,
    StoppedSafe,
    StoppedCrashed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SyncWatcherStatus {
    pub latest_enqueue: u64,
    pub acknowledged: u64,
    pub drain_in_flight: bool,
    pub pending: bool,
    pub pending_requires_full_scan: bool,
    pub deferred: bool,
    pub quiescing: bool,
    pub sequence_exhausted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncRuntimeTick {
    Idle,
    LocalMutation(SyncLocalMutationOutcome),
    RecoveryBlocked(String),
    Recovering,
    RetryFull,
    Blocked(String),
    Failed(String),
    AdmittedNoop { epoch: u64 },
    AdmittedComplete { epoch: u64 },
    Terminal(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncRuntimeStatusSnapshot {
    pub lifecycle: SyncRuntimeLifecycle,
    pub recovery: Option<SyncRuntimeRecovery>,
    pub watcher: SyncWatcherStatus,
    pub last_tick: Option<SyncRuntimeTick>,
    pub detail: Option<String>,
}

/// Serializable exact text kind at the application boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncPageKind {
    Page,
    Journal,
}

impl From<ManagedTextKind> for SyncPageKind {
    fn from(value: ManagedTextKind) -> Self {
        match value {
            ManagedTextKind::Page => Self::Page,
            ManagedTextKind::Journal => Self::Journal,
        }
    }
}

impl From<SyncPageKind> for ManagedTextKind {
    fn from(value: SyncPageKind) -> Self {
        match value {
            SyncPageKind::Page => Self::Page,
            SyncPageKind::Journal => Self::Journal,
        }
    }
}

/// Public identity keeps opaque engine page/block IDs separate from optional
/// Logseq UUIDs. UUID strings are never accepted as a substitute BlockId.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "entity_type", content = "id", rename_all = "snake_case")]
pub enum SyncEntityId {
    Page(String),
    Block(String),
}

impl From<MaterializedEntityId> for SyncEntityId {
    fn from(value: MaterializedEntityId) -> Self {
        match value {
            MaterializedEntityId::Page(id) => Self::Page(id.to_string()),
            MaterializedEntityId::Block(id) => Self::Block(id.to_string()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncPageDto {
    pub page_id: String,
    pub home_document_id: String,
    pub name: String,
    pub path: String,
    pub kind: SyncPageKind,
    pub preamble: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncBlockDto {
    /// Opaque sparse-oplog identity. It is intentionally not a Logseq UUID.
    pub block_id: String,
    pub page_id: String,
    pub home_document_id: String,
    pub parent_block_id: Option<String>,
    pub order: String,
    pub content: String,
    pub heading_level: Option<u8>,
    pub collapsed: bool,
    pub logseq_uuid: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncPageWithBlocksDto {
    pub page: SyncPageDto,
    /// Ordered, parent-linked blocks. `parent_block_id` describes the tree
    /// without forcing duplicate child retention across the Tauri boundary.
    pub blocks: Vec<SyncBlockDto>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncPropertyDto {
    pub owner: SyncEntityId,
    pub page_id: String,
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncTagDto {
    pub owner: SyncEntityId,
    pub page_id: String,
    pub tag: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncTaskDto {
    pub block_id: String,
    pub page_id: String,
    pub marker: String,
    pub priority: Option<String>,
    pub scheduled: Option<String>,
    pub deadline: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncSearchHitDto {
    pub entity: SyncEntityId,
    pub page_id: String,
    pub text: String,
    pub rank: f64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source_type", rename_all = "snake_case")]
pub enum SyncReferenceSourceDto {
    Preamble,
    Block {
        block_id: String,
        home_document_id: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncReferenceHitDto {
    pub source_page_id: String,
    pub source: SyncReferenceSourceDto,
    pub kind: String,
    pub raw_target: String,
    pub byte_start: u32,
    pub byte_end: u32,
    pub resolved_page_id: Option<String>,
    pub resolved_block_id: Option<String>,
}

/// The bounded public query envelope. All page predicates are exact; there is
/// no graph walk, path glob, or filesystem enumeration branch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SyncRuntimeQueryRequest {
    ResolvePage {
        path: String,
        name: String,
        page_kind: SyncPageKind,
    },
    ListPages {
        page_kind: Option<SyncPageKind>,
        limit: usize,
    },
    LoadPage {
        page_id: String,
        block_limit: usize,
    },
    Search {
        query: String,
        limit: usize,
    },
    PropertiesForOwner {
        owner: SyncEntityId,
        limit: usize,
    },
    PropertiesNamed {
        name: String,
        value: Option<String>,
        limit: usize,
    },
    Tags {
        tag: String,
        limit: usize,
    },
    Tasks {
        marker: Option<String>,
        limit: usize,
    },
    ReferencesToPageName {
        name: String,
        limit: usize,
    },
    ReferencesToLogseqUuid {
        logseq_uuid: String,
        limit: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SyncRuntimeQueryReply {
    Page(Option<SyncPageDto>),
    Pages(Vec<SyncPageDto>),
    PageWithBlocks(Option<SyncPageWithBlocksDto>),
    Search(Vec<SyncSearchHitDto>),
    Properties(Vec<SyncPropertyDto>),
    Tags(Vec<SyncTagDto>),
    Tasks(Vec<SyncTaskDto>),
    References(Vec<SyncReferenceHitDto>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncShutdownOutcome {
    Safe(SyncRuntimeStatusSnapshot),
    Terminal(SyncRuntimeStatusSnapshot),
}

/// Bounded phase diagnosis for one local mutation reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncLocalMutationPhase {
    Bindings,
    Planning,
    Draft,
    Capture,
    Finalize,
    TailReservation,
    Publication,
    ArchiveStage,
    TailAdmission,
    SqliteDrain,
    ProjectionDrain,
}

/// Why a new local mutation was not admitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncLocalMutationBlock {
    /// An earlier published or reconciliation-first mutation still owns the
    /// actor's local mutation slot. The submitted transaction was not run.
    PriorMutationUnresolved,
    /// The submitted mutation was refused before immutable publication.
    Prepublication,
    /// Immutable publication happened, but stable evidence prevents progress.
    RetainedPublished,
}

/// Typed result of one bounded local semantic mutation request.
///
/// A retryable, retained, blocked, or revoked reply never vends the private
/// continuation. The actor remains its sole owner and retries it on later
/// ordered turns. `Durable` means immutable history, SQLite, and graph
/// projection completed before the reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncLocalMutationOutcome {
    Durable {
        batch_id: BatchId,
    },
    RetryableRetainedRecovery {
        batch_id: Option<BatchId>,
        phase: SyncLocalMutationPhase,
    },
    Blocked {
        batch_id: Option<BatchId>,
        phase: SyncLocalMutationPhase,
        reason: SyncLocalMutationBlock,
    },
    Revoked {
        batch_id: Option<BatchId>,
        phase: SyncLocalMutationPhase,
    },
}

/// Bounded overflow witness. Every field is capped at its public limit plus
/// one, so even diagnostics for attacker-shaped DTOs remain small.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncLocalMutationRequestSize {
    pub rows: usize,
    pub referenced_paths: usize,
    pub path_bytes: usize,
    pub text_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncLocalMutationRequestError {
    /// The public DTO bypassed or failed `OperationTransaction::new`, or used
    /// the external-reconciliation-only operation.
    InvalidTransaction,
    /// One or more public intake budgets were exceeded before actor queueing.
    RequestTooLarge(SyncLocalMutationRequestSize),
    /// The actor stopped, crashed, or completed clean shutdown.
    ActorUnavailable,
}

impl fmt::Display for SyncLocalMutationRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransaction => {
                formatter.write_str("local mutation transaction is invalid")
            }
            Self::RequestTooLarge(size) => write!(
                formatter,
                "local mutation exceeds bounds: {} rows, {} paths, {} path bytes, {} text bytes",
                size.rows, size.referenced_paths, size.path_bytes, size.text_bytes
            ),
            Self::ActorUnavailable => formatter.write_str("sync actor is unavailable"),
        }
    }
}

impl std::error::Error for SyncLocalMutationRequestError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncRuntimeRequestError {
    InvalidRequest(String),
    QueryTooLarge {
        limit: usize,
        request_bytes: usize,
    },
    RequestTooLarge {
        observations: usize,
        /// Exact while under the byte cap; otherwise a bounded overflow diagnostic.
        path_bytes: usize,
    },
    ActorRefused(String),
    ActorUnavailable,
}

impl fmt::Display for SyncRuntimeRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(detail) => write!(formatter, "invalid sync request: {detail}"),
            Self::QueryTooLarge {
                limit,
                request_bytes,
            } => write!(
                formatter,
                "sync query exceeds bounds: limit {limit}, {request_bytes} request bytes"
            ),
            Self::RequestTooLarge {
                observations,
                path_bytes,
            } => write!(
                formatter,
                "watcher request exceeds bounds: {observations} observations, {path_bytes} path bytes"
            ),
            Self::ActorRefused(detail) => write!(formatter, "sync actor refused request: {detail}"),
            Self::ActorUnavailable => formatter.write_str("sync actor is unavailable"),
        }
    }
}

impl std::error::Error for SyncRuntimeRequestError {}

/// Cloneable public channel capability for one private actor.
#[derive(Clone)]
pub struct SyncRuntimeHandle {
    inner: Arc<HandleInner>,
}

impl fmt::Debug for SyncRuntimeHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SyncRuntimeHandle")
            .field("status", &*self.inner.status.read().unwrap())
            .finish_non_exhaustive()
    }
}

struct HandleInner {
    operation: Mutex<()>,
    sender: Mutex<Option<SyncSender<ActorRequest>>>,
    join: Mutex<Option<JoinHandle<()>>>,
    status: Arc<RwLock<SyncRuntimeStatusSnapshot>>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        self.sender.get_mut().unwrap().take();
        if let Some(join) = self.join.get_mut().unwrap().take() {
            let _ = join.join();
        }
    }
}

impl SyncRuntimeHandle {
    /// Read-only discovery first; actor construction only for authenticated
    /// existing `LocalActive` evidence.
    pub fn open(request: SyncRuntimeOpenRequest) -> SyncRuntimeOpenResult {
        if request.profile == SyncStorageProfile::LegacyDefault {
            return SyncRuntimeOpenResult {
                status: SyncRuntimeOpenStatus::LegacyDefault,
                handle: None,
            };
        }

        let graph = match Graph::open_checked(&request.graph_root) {
            Ok(graph) => graph,
            Err(error) => {
                return refused(format!("cannot retain graph for discovery: {error}"));
            }
        };
        let graph_resource_id = match graph.canonical_resource_id() {
            Ok(resource) => resource,
            Err(error) => return refused(format!("cannot identify graph for discovery: {error}")),
        };
        let classification = discover_startup(&DiscoveryRequest {
            profile: StartupStorageProfile::ExperimentalSparse,
            graph_resource_id,
            runtime_root: &request.enrollment_root,
            archive_root: &request.archive_root,
        });
        drop(graph);
        let advisory = match classification {
            DiscoveryClassification::ExistingLocalActive(advisory) => advisory,
            other => {
                return SyncRuntimeOpenResult {
                    status: map_discovery(other),
                    handle: None,
                };
            }
        };

        let initial = SyncRuntimeStatusSnapshot {
            lifecycle: SyncRuntimeLifecycle::Active,
            recovery: None,
            watcher: SyncWatcherStatus::default(),
            last_tick: None,
            detail: Some("actor startup is authenticating discovered state".into()),
        };
        let status = Arc::new(RwLock::new(initial));
        let (sender, receiver) = mpsc::sync_channel(ACTOR_CHANNEL_CAPACITY);
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let actor_status = Arc::clone(&status);
        let thread_name = format!("tine-sync-{}", &graph_resource_id.to_string()[..12]);
        let join = match thread::Builder::new()
            .name(thread_name)
            .stack_size(ACTOR_STACK_BYTES)
            .spawn(move || {
                #[cfg(test)]
                ACTOR_THREADS_STARTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    actor_thread(request, advisory, receiver, started_sender, &actor_status)
                }));
                if result.is_err() {
                    *actor_status.write().unwrap() = SyncRuntimeStatusSnapshot {
                        lifecycle: SyncRuntimeLifecycle::StoppedCrashed,
                        recovery: None,
                        watcher: SyncWatcherStatus::default(),
                        last_tick: None,
                        detail: Some("sync actor panicked".into()),
                    };
                }
                #[cfg(test)]
                ACTOR_THREADS_FINISHED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }) {
            Ok(join) => join,
            Err(error) => return refused(format!("cannot start sync actor thread: {error}")),
        };

        match started_receiver.recv() {
            Ok(Ok(snapshot)) => {
                *status.write().unwrap() = snapshot;
                SyncRuntimeOpenResult {
                    status: SyncRuntimeOpenStatus::Active,
                    handle: Some(Self {
                        inner: Arc::new(HandleInner {
                            operation: Mutex::new(()),
                            sender: Mutex::new(Some(sender)),
                            join: Mutex::new(Some(join)),
                            status,
                        }),
                    }),
                }
            }
            Ok(Err(detail)) => {
                drop(sender);
                let _ = join.join();
                refused(detail)
            }
            Err(_) => {
                drop(sender);
                let _ = join.join();
                refused("sync actor stopped during startup".into())
            }
        }
    }

    pub fn observe_watcher(
        &self,
        observations: Vec<SyncWatcherObservation>,
    ) -> Result<(), SyncRuntimeRequestError> {
        // This gate is also the observation linearization point relative to
        // `clean_shutdown`. A rejected callback is still an observed external
        // change, so retain one bounded full-scan obligation before reporting
        // the refusal. Otherwise Safe could commit between rejection and a
        // caller retry while denying work the runtime has already seen.
        let _operation = self.inner.operation.lock().unwrap();
        let path_bytes = bounded_watcher_path_bytes(&observations);
        if observations.len() > MAX_WATCHER_OBSERVATIONS || path_bytes > MAX_WATCHER_PATH_BYTES {
            self.retain_rejected_watcher_work()?;
            return Err(SyncRuntimeRequestError::RequestTooLarge {
                observations: observations.len(),
                path_bytes,
            });
        }
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.send(ActorRequest::Observe {
            observations,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .map_err(|_| SyncRuntimeRequestError::ActorUnavailable)?
    }

    /// Submit one bounded semantic transaction to this existing inactive
    /// `LocalActive` runtime.
    ///
    /// Public DTO fields are revalidated and all row/path/text budgets are
    /// enforced while holding the same operation gate as watcher observation,
    /// ticks, status, and clean shutdown. An oversized or invalid transaction
    /// never enters the actor queue.
    pub fn submit_local_mutation(
        &self,
        transaction: OperationTransaction,
    ) -> Result<SyncLocalMutationOutcome, SyncLocalMutationRequestError> {
        let _operation = self.inner.operation.lock().unwrap();
        let transaction = validate_local_mutation_request(transaction)?;
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.send(ActorRequest::SubmitLocalMutation {
            transaction,
            reply: reply_sender,
        })
        .map_err(map_local_actor_error)?;
        reply_receiver
            .recv()
            .map_err(|_| SyncLocalMutationRequestError::ActorUnavailable)
    }

    /// Execute one bounded, read-only query on the private actor.
    ///
    /// The operation gate places this read in the same total public order as
    /// watcher observations, local mutations, ticks, status, and shutdown.
    pub fn query(
        &self,
        request: SyncRuntimeQueryRequest,
    ) -> Result<SyncRuntimeQueryReply, SyncRuntimeRequestError> {
        let _operation = self.inner.operation.lock().unwrap();
        validate_query_request(&request)?;
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.send(ActorRequest::Query {
            request,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .map_err(|_| SyncRuntimeRequestError::ActorUnavailable)?
    }

    /// A public request may be too large to retain verbatim, but that never
    /// makes its observation disposable. The actor's one-owner queue already
    /// gives this marker an epoch, status visibility, and a full graph scan
    /// before the Safe handoff barrier can pass.
    fn retain_rejected_watcher_work(&self) -> Result<(), SyncRuntimeRequestError> {
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.send(ActorRequest::Observe {
            observations: vec![SyncWatcherObservation::RescanRequired],
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .map_err(|_| SyncRuntimeRequestError::ActorUnavailable)?
    }

    pub fn tick(&self) -> Result<SyncRuntimeTick, SyncRuntimeRequestError> {
        let _operation = self.inner.operation.lock().unwrap();
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.send(ActorRequest::Tick {
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .map_err(|_| SyncRuntimeRequestError::ActorUnavailable)
    }

    pub fn status(&self) -> Result<SyncRuntimeStatusSnapshot, SyncRuntimeRequestError> {
        let _operation = self.inner.operation.lock().unwrap();
        let sender_present = self.inner.sender.lock().unwrap().is_some();
        if !sender_present {
            return Ok(self.inner.status.read().unwrap().clone());
        }
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.send(ActorRequest::Status {
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .map_err(|_| SyncRuntimeRequestError::ActorUnavailable)
    }

    /// Drain the exact feed, execute the production Safe transaction once, and
    /// join the actor. A pre-Safe refusal leaves the actor available for an
    /// explicit retry or crash-style drop.
    pub fn clean_shutdown(&self) -> Result<SyncShutdownOutcome, SyncRuntimeRequestError> {
        let _operation = self.inner.operation.lock().unwrap();
        if self.inner.sender.lock().unwrap().is_none() {
            let snapshot = self.inner.status.read().unwrap().clone();
            return Ok(match snapshot.lifecycle {
                SyncRuntimeLifecycle::StoppedSafe => SyncShutdownOutcome::Safe(snapshot),
                _ => SyncShutdownOutcome::Terminal(snapshot),
            });
        }
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.send(ActorRequest::CleanShutdown {
            reply: reply_sender,
        })?;
        let outcome = reply_receiver
            .recv()
            .map_err(|_| SyncRuntimeRequestError::ActorUnavailable)??;
        self.inner.sender.lock().unwrap().take();
        if let Some(join) = self.inner.join.lock().unwrap().take() {
            join.join()
                .map_err(|_| SyncRuntimeRequestError::ActorUnavailable)?;
        }
        Ok(outcome)
    }

    #[cfg(test)]
    fn install_repeated_operational_fault(
        &self,
        point: OperationalFaultPoint,
        failures: u8,
    ) -> Result<(), SyncRuntimeRequestError> {
        let _operation = self.inner.operation.lock().unwrap();
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.send(ActorRequest::InstallRepeatedOperationalFault {
            point,
            failures,
            reply: reply_sender,
        })?;
        reply_receiver
            .recv()
            .map_err(|_| SyncRuntimeRequestError::ActorUnavailable)
    }

    fn send(&self, request: ActorRequest) -> Result<(), SyncRuntimeRequestError> {
        self.inner
            .sender
            .lock()
            .unwrap()
            .as_ref()
            .ok_or(SyncRuntimeRequestError::ActorUnavailable)?
            .send(request)
            .map_err(|_| SyncRuntimeRequestError::ActorUnavailable)
    }
}

/// Bound public request validation as well as actor intake. A batch above the
/// count cap is already known to require a full scan, so only inspect a fixed
/// prefix. The result is exact while it remains within the byte cap; once the
/// cap is crossed it is merely a bounded overflow witness.
fn bounded_watcher_path_bytes(observations: &[SyncWatcherObservation]) -> usize {
    let mut path_bytes = 0_usize;
    for observation in observations.iter().take(MAX_WATCHER_OBSERVATIONS) {
        path_bytes = path_bytes.saturating_add(observation.retained_path_bytes());
        if path_bytes > MAX_WATCHER_PATH_BYTES {
            break;
        }
    }
    path_bytes
}

fn validate_local_mutation_request(
    transaction: OperationTransaction,
) -> Result<OperationTransaction, SyncLocalMutationRequestError> {
    let size = bounded_local_mutation_size(&transaction);
    if size.rows > MAX_LOCAL_MUTATION_ROWS
        || size.referenced_paths > MAX_LOCAL_MUTATION_REFERENCED_PATHS
        || size.path_bytes > MAX_LOCAL_MUTATION_PATH_BYTES
        || size.text_bytes > MAX_LOCAL_MUTATION_TEXT_BYTES
    {
        return Err(SyncLocalMutationRequestError::RequestTooLarge(size));
    }
    if transaction.operations.iter().any(|operation| {
        matches!(
            operation,
            SemanticOperation::ReconcileExternalPageState { .. }
        )
    }) {
        return Err(SyncLocalMutationRequestError::InvalidTransaction);
    }
    OperationTransaction::new(transaction.operations)
        .map_err(|_| SyncLocalMutationRequestError::InvalidTransaction)
}

fn bounded_local_mutation_size(transaction: &OperationTransaction) -> SyncLocalMutationRequestSize {
    let mut size = SyncLocalMutationRequestSize {
        rows: 0,
        referenced_paths: 0,
        path_bytes: 0,
        text_bytes: 0,
    };
    for operation in transaction
        .operations
        .iter()
        .take(MAX_LOCAL_MUTATION_ROWS + 1)
    {
        charge_local_row(&mut size);
        match operation {
            SemanticOperation::CreatePage { name, path, .. }
            | SemanticOperation::ReconcileExternalPageState { name, path, .. } => {
                charge_local_text(&mut size, name.as_str().len());
                charge_local_path(&mut size, path);
            }
            SemanticOperation::EditPagePath { path, .. } => {
                charge_local_path(&mut size, path);
            }
            SemanticOperation::SetPagePreamble { preamble, .. } => {
                charge_local_text(&mut size, preamble.as_ref().map_or(0, String::len));
            }
            SemanticOperation::CreateBlock { order, content, .. } => {
                charge_local_text(&mut size, order.len());
                charge_local_text(&mut size, content.len());
            }
            SemanticOperation::EditBlockContent { content, .. } => {
                charge_local_text(&mut size, content.len());
            }
            SemanticOperation::MoveSubtree { order, .. }
            | SemanticOperation::ReorderBlock { order, .. } => {
                charge_local_text(&mut size, order.len());
            }
            SemanticOperation::RenamePagesAndRewriteReferrers {
                page_changes,
                block_rewrites,
                page_preamble_rewrites,
            } => {
                for change in page_changes {
                    charge_local_row(&mut size);
                    charge_local_text(&mut size, change.new_name.as_str().len());
                    charge_local_path(&mut size, &change.new_path);
                    if size.rows > MAX_LOCAL_MUTATION_ROWS {
                        break;
                    }
                }
                if size.rows <= MAX_LOCAL_MUTATION_ROWS {
                    for rewrite in block_rewrites {
                        charge_local_row(&mut size);
                        charge_local_text(&mut size, rewrite.new_content.len());
                        if size.rows > MAX_LOCAL_MUTATION_ROWS {
                            break;
                        }
                    }
                }
                if size.rows <= MAX_LOCAL_MUTATION_ROWS {
                    for rewrite in page_preamble_rewrites {
                        charge_local_row(&mut size);
                        charge_local_text(
                            &mut size,
                            rewrite.new_preamble.as_ref().map_or(0, String::len),
                        );
                        if size.rows > MAX_LOCAL_MUTATION_ROWS {
                            break;
                        }
                    }
                }
            }
            SemanticOperation::SetPageKind { .. }
            | SemanticOperation::MutateBlockLogseqIdentity { .. }
            | SemanticOperation::DeleteSubtree { .. }
            | SemanticOperation::DeletePage { .. } => {}
        }
        if local_size_exceeded(size) {
            break;
        }
    }
    if transaction.operations.len() > MAX_LOCAL_MUTATION_ROWS {
        size.rows = MAX_LOCAL_MUTATION_ROWS + 1;
    }
    size
}

fn charge_local_row(size: &mut SyncLocalMutationRequestSize) {
    size.rows = bounded_charge(size.rows, 1, MAX_LOCAL_MUTATION_ROWS);
}

fn charge_local_path(size: &mut SyncLocalMutationRequestSize, path: &ManagedPath) {
    size.referenced_paths = bounded_charge(
        size.referenced_paths,
        1,
        MAX_LOCAL_MUTATION_REFERENCED_PATHS,
    );
    size.path_bytes = bounded_charge(
        size.path_bytes,
        path.as_str().len(),
        MAX_LOCAL_MUTATION_PATH_BYTES,
    );
}

fn charge_local_text(size: &mut SyncLocalMutationRequestSize, bytes: usize) {
    size.text_bytes = bounded_charge(size.text_bytes, bytes, MAX_LOCAL_MUTATION_TEXT_BYTES);
}

fn bounded_charge(current: usize, charge: usize, limit: usize) -> usize {
    current.saturating_add(charge).min(limit.saturating_add(1))
}

fn local_size_exceeded(size: SyncLocalMutationRequestSize) -> bool {
    size.rows > MAX_LOCAL_MUTATION_ROWS
        || size.referenced_paths > MAX_LOCAL_MUTATION_REFERENCED_PATHS
        || size.path_bytes > MAX_LOCAL_MUTATION_PATH_BYTES
        || size.text_bytes > MAX_LOCAL_MUTATION_TEXT_BYTES
}

fn map_local_actor_error(_: SyncRuntimeRequestError) -> SyncLocalMutationRequestError {
    SyncLocalMutationRequestError::ActorUnavailable
}

fn check_query_limit(limit: usize, request_bytes: usize) -> Result<(), SyncRuntimeRequestError> {
    if limit == 0 || limit > MAX_SYNC_RUNTIME_QUERY_ROWS {
        return Err(SyncRuntimeRequestError::QueryTooLarge {
            limit,
            request_bytes,
        });
    }
    Ok(())
}

fn validate_query_request(
    request: &SyncRuntimeQueryRequest,
) -> Result<(), SyncRuntimeRequestError> {
    let mut bytes = 0_usize;
    let mut add = |value: &str| {
        bytes = bytes.saturating_add(value.len());
    };
    let limit = match request {
        SyncRuntimeQueryRequest::ResolvePage { path, name, .. } => {
            add(path);
            add(name);
            1
        }
        SyncRuntimeQueryRequest::ListPages { limit, .. }
        | SyncRuntimeQueryRequest::Search { limit, .. }
        | SyncRuntimeQueryRequest::PropertiesForOwner { limit, .. }
        | SyncRuntimeQueryRequest::PropertiesNamed { limit, .. }
        | SyncRuntimeQueryRequest::Tags { limit, .. }
        | SyncRuntimeQueryRequest::Tasks { limit, .. }
        | SyncRuntimeQueryRequest::ReferencesToPageName { limit, .. }
        | SyncRuntimeQueryRequest::ReferencesToLogseqUuid { limit, .. } => {
            match request {
                SyncRuntimeQueryRequest::Search { query, .. } => add(query),
                SyncRuntimeQueryRequest::PropertiesNamed { name, value, .. } => {
                    add(name);
                    if let Some(value) = value {
                        add(value);
                    }
                }
                SyncRuntimeQueryRequest::Tags { tag, .. } => add(tag),
                SyncRuntimeQueryRequest::Tasks { marker, .. } => {
                    if let Some(marker) = marker {
                        add(marker);
                    }
                }
                SyncRuntimeQueryRequest::ReferencesToPageName { name, .. } => add(name),
                SyncRuntimeQueryRequest::ReferencesToLogseqUuid { logseq_uuid, .. } => {
                    add(logseq_uuid)
                }
                SyncRuntimeQueryRequest::ListPages { .. } => {}
                SyncRuntimeQueryRequest::PropertiesForOwner { owner, .. } => match owner {
                    SyncEntityId::Page(id) | SyncEntityId::Block(id) => add(id),
                },
                _ => unreachable!("all query request variants are covered"),
            }
            *limit
        }
        SyncRuntimeQueryRequest::LoadPage {
            page_id,
            block_limit,
        } => {
            add(page_id);
            *block_limit
        }
    };
    if bytes > MAX_SYNC_RUNTIME_QUERY_BYTES {
        return Err(SyncRuntimeRequestError::QueryTooLarge {
            limit,
            request_bytes: bytes,
        });
    }
    check_query_limit(limit, bytes)
}

fn refused(detail: String) -> SyncRuntimeOpenResult {
    SyncRuntimeOpenResult {
        status: SyncRuntimeOpenStatus::OpenRefused { detail },
        handle: None,
    }
}

fn map_discovery(classification: DiscoveryClassification) -> SyncRuntimeOpenStatus {
    match classification {
        DiscoveryClassification::LegacyDefault => SyncRuntimeOpenStatus::LegacyDefault,
        DiscoveryClassification::Absent => SyncRuntimeOpenStatus::Absent,
        DiscoveryClassification::ExistingLocalActive(_) => SyncRuntimeOpenStatus::Active,
        DiscoveryClassification::ExistingNonActive(advisory) => {
            SyncRuntimeOpenStatus::ExistingNonActive(match advisory.stage {
                NonActiveStage::ShadowImport => SyncNonActiveStage::ShadowImport,
                NonActiveStage::VerifiedLocal => SyncNonActiveStage::VerifiedLocal,
            })
        }
        DiscoveryClassification::Blocked(advisory) => SyncRuntimeOpenStatus::Blocked {
            reason_code: advisory.reason_code,
        },
        DiscoveryClassification::UnsupportedOrIncompatible(component) => {
            SyncRuntimeOpenStatus::UnsupportedOrIncompatible(map_component(component))
        }
        DiscoveryClassification::CorruptOrUnreadable(component) => {
            SyncRuntimeOpenStatus::CorruptOrUnreadable(map_component(component))
        }
        DiscoveryClassification::AmbiguousOrForeignResidue(evidence) => {
            SyncRuntimeOpenStatus::AmbiguousOrForeignResidue(match evidence {
                AmbiguousEvidence::EnrollmentResidue => SyncAmbiguousEvidence::EnrollmentResidue,
                AmbiguousEvidence::EnrollmentNamespace => {
                    SyncAmbiguousEvidence::EnrollmentNamespace
                }
                AmbiguousEvidence::EnrollmentGraphBinding => {
                    SyncAmbiguousEvidence::EnrollmentGraphBinding
                }
                AmbiguousEvidence::ArchiveResidue => SyncAmbiguousEvidence::ArchiveResidue,
                AmbiguousEvidence::ArchiveNamespace => SyncAmbiguousEvidence::ArchiveNamespace,
                AmbiguousEvidence::ArchiveBinding => SyncAmbiguousEvidence::ArchiveBinding,
                AmbiguousEvidence::ActiveArchiveMismatch => {
                    SyncAmbiguousEvidence::ActiveArchiveMismatch
                }
            })
        }
    }
}

fn map_component(component: DiscoveryComponent) -> SyncRuntimeComponent {
    match component {
        DiscoveryComponent::Enrollment => SyncRuntimeComponent::Enrollment,
        DiscoveryComponent::Archive => SyncRuntimeComponent::Archive,
    }
}

enum ActorRequest {
    Query {
        request: SyncRuntimeQueryRequest,
        reply: mpsc::Sender<Result<SyncRuntimeQueryReply, SyncRuntimeRequestError>>,
    },
    Observe {
        observations: Vec<SyncWatcherObservation>,
        reply: mpsc::Sender<Result<(), SyncRuntimeRequestError>>,
    },
    SubmitLocalMutation {
        transaction: OperationTransaction,
        reply: mpsc::Sender<SyncLocalMutationOutcome>,
    },
    Tick {
        reply: mpsc::Sender<SyncRuntimeTick>,
    },
    Status {
        reply: mpsc::Sender<SyncRuntimeStatusSnapshot>,
    },
    #[cfg(test)]
    InstallRepeatedOperationalFault {
        point: OperationalFaultPoint,
        failures: u8,
        reply: mpsc::Sender<()>,
    },
    CleanShutdown {
        reply: mpsc::Sender<Result<SyncShutdownOutcome, SyncRuntimeRequestError>>,
    },
}

fn actor_thread(
    request: SyncRuntimeOpenRequest,
    advisory: LocalActiveAdvisory,
    receiver: Receiver<ActorRequest>,
    started: SyncSender<Result<SyncRuntimeStatusSnapshot, String>>,
    shared_status: &RwLock<SyncRuntimeStatusSnapshot>,
) {
    let mut actor = match RuntimeActor::open(request, advisory) {
        Ok(actor) => actor,
        Err(error) => {
            let _ = started.send(Err(error));
            return;
        }
    };
    let snapshot = actor.snapshot();
    *shared_status.write().unwrap() = snapshot.clone();
    if started.send(Ok(snapshot)).is_err() {
        return;
    }

    while let Ok(request) = receiver.recv() {
        let should_stop = match request {
            ActorRequest::Query { request, reply } => {
                let result = actor.query(request);
                let _ = reply.send(result);
                false
            }
            ActorRequest::Observe {
                observations,
                reply,
            } => {
                actor.advance_local_mutation_once();
                let result = actor.observe(observations);
                let _ = reply.send(result);
                false
            }
            ActorRequest::SubmitLocalMutation { transaction, reply } => {
                let result = actor.submit_local_mutation(transaction);
                let _ = reply.send(result);
                false
            }
            ActorRequest::Tick { reply } => {
                let result = actor.tick();
                let _ = reply.send(result);
                false
            }
            ActorRequest::Status { reply } => {
                actor.advance_local_mutation_once();
                let _ = reply.send(actor.snapshot());
                false
            }
            #[cfg(test)]
            ActorRequest::InstallRepeatedOperationalFault {
                point,
                failures,
                reply,
            } => {
                fail_repeatedly_at(point, failures);
                let _ = reply.send(());
                false
            }
            ActorRequest::CleanShutdown { reply } => match actor.clean_shutdown() {
                Ok(outcome) => {
                    let _ = reply.send(Ok(outcome));
                    true
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                    false
                }
            },
        };
        *shared_status.write().unwrap() = actor.snapshot();
        if should_stop {
            break;
        }
    }
}

/// Deliberately `!Send + !Sync`; constructed and destroyed inside the actor
/// thread. The `Rc` marker makes accidental movement into Tauri state a compile
/// error even if every owned authority happens to gain `Send` later.
enum PendingLocalMutation {
    Reconciliation { transaction: OperationTransaction },
    Published(LocalPublishedContinuation),
}

struct RuntimeActor {
    graph: Graph,
    receipts: ProjectionReceiptStore,
    authority: Option<LocalActiveAuthority>,
    runtime: Option<PromotedLocalRuntime>,
    feed: Option<ExactExternalFeedState>,
    local_mutation: Option<PendingLocalMutation>,
    recovery: SyncRuntimeRecovery,
    last_watcher: SyncWatcherStatus,
    last_tick: Option<SyncRuntimeTick>,
    terminal: Option<String>,
    stopped_safe: bool,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl RuntimeActor {
    fn open(
        request: SyncRuntimeOpenRequest,
        advisory: LocalActiveAdvisory,
    ) -> Result<Self, String> {
        let graph = Graph::open_checked(&request.graph_root).map_err(display)?;
        let graph_resource_id = graph.canonical_resource_id().map_err(display)?;
        if graph_resource_id != advisory.binding.graph_resource_id() {
            return Err("actor graph does not match discovery binding".into());
        }

        let fresh = discover_startup(&DiscoveryRequest {
            profile: StartupStorageProfile::ExperimentalSparse,
            graph_resource_id,
            runtime_root: &request.enrollment_root,
            archive_root: &request.archive_root,
        });
        if fresh != DiscoveryClassification::ExistingLocalActive(advisory.clone()) {
            return Err("discovered LocalActive evidence changed before actor open".into());
        }

        let enrollment_root =
            open_existing_enrollment_application_root(&request.enrollment_root).map_err(display)?;
        let endpoint = ProjectionEndpointBinding {
            endpoint_id: advisory.binding.endpoint_id(),
            device_id: advisory.binding.device_id(),
            graph_resource_id: advisory.binding.graph_resource_id(),
        };
        let receipts = ProjectionReceiptStore::open_existing_for_endpoint(
            &request.receipt_root,
            advisory.binding.workspace_id(),
            endpoint,
            advisory.binding.receipt_store_id(),
        )
        .map_err(display)?;
        let application_runtime_root = ApplicationRuntimeRoot::open_existing_for_runtime_host(
            &request.application_runtime_root,
        )
        .map_err(display)?;
        let trusted_runtime = TrustedPrivateApplicationRuntimeRoot::from_application_runtime_root(
            &application_runtime_root,
        );
        let baseline_binding = ReconciliationBaselineBinding::new(
            advisory.binding.workspace_id(),
            advisory.binding.endpoint_id(),
            graph_resource_id,
            graph.graph_text_scope_binding().map_err(display)?,
        )
        .map_err(display)?;
        let baseline = ReconciliationBaseline::open_existing(&trusted_runtime, baseline_binding)
            .map_err(display)?;
        let open = PromotedRuntimeOpen {
            graph: &graph,
            receipts: &receipts,
            archive_root: &request.archive_root,
            database_path: &request.database_path,
            application_runtime_root: &application_runtime_root,
        };
        let session_id = SessionId::new();
        let (authority, runtime) = match advisory.handoff {
            EnrollmentDiscoveryHandoff::Safe => reopen_promoted_local_runtime_existing_projection(
                &enrollment_root,
                &advisory.binding,
                session_id,
                &open,
            ),
            EnrollmentDiscoveryHandoff::Unsafe { .. } => {
                take_over_promoted_local_runtime_recovering_projection(
                    &enrollment_root,
                    &advisory.binding,
                    session_id,
                    &open,
                )
            }
        }
        .map_err(display)?;
        let recovery = map_recovery(runtime.recovery());
        let feed =
            ExactExternalFeedState::open(&graph, &receipts, &runtime, baseline).map_err(display)?;
        let last_watcher = map_watcher(runtime.watcher_status());
        Ok(Self {
            graph,
            receipts,
            authority: Some(authority),
            runtime: Some(runtime),
            feed: Some(feed),
            local_mutation: None,
            recovery,
            last_watcher,
            last_tick: None,
            terminal: None,
            stopped_safe: false,
            _not_send_or_sync: PhantomData,
        })
    }

    fn observe(
        &mut self,
        observations: Vec<SyncWatcherObservation>,
    ) -> Result<(), SyncRuntimeRequestError> {
        if let Some(detail) = &self.terminal {
            return Err(SyncRuntimeRequestError::ActorRefused(detail.clone()));
        }
        let runtime = self
            .runtime
            .as_ref()
            .ok_or(SyncRuntimeRequestError::ActorUnavailable)?;
        let result = self
            .feed
            .as_mut()
            .ok_or(SyncRuntimeRequestError::ActorUnavailable)?
            .observe(
                &self.graph,
                runtime,
                observations
                    .into_iter()
                    .map(SyncWatcherObservation::into_core),
            );
        self.refresh_watcher();
        match result {
            Ok(()) => Ok(()),
            Err(ExactExternalFeedObserveError::Terminal) => {
                let detail = self
                    .feed
                    .as_ref()
                    .and_then(|feed| feed.terminal())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "exact external feed became terminal".into());
                self.latch_terminal(detail.clone());
                Err(SyncRuntimeRequestError::ActorRefused(detail))
            }
            Err(error) => Err(SyncRuntimeRequestError::ActorRefused(error.to_string())),
        }
    }

    fn query(
        &mut self,
        request: SyncRuntimeQueryRequest,
    ) -> Result<SyncRuntimeQueryReply, SyncRuntimeRequestError> {
        if let Some(detail) = &self.terminal {
            return Err(SyncRuntimeRequestError::ActorRefused(detail.clone()));
        }
        let runtime = self
            .runtime
            .as_ref()
            .ok_or(SyncRuntimeRequestError::ActorUnavailable)?;
        let read = runtime
            .database()
            .materialized_read()
            .map_err(|error| SyncRuntimeRequestError::ActorRefused(error.to_string()))?;
        match request {
            SyncRuntimeQueryRequest::ResolvePage {
                path,
                name,
                page_kind,
            } => {
                let path = ManagedPath::parse(path)
                    .map_err(|error| SyncRuntimeRequestError::InvalidRequest(error.to_string()))?;
                let page = read
                    .pages_by_path(&path, 1)
                    .map_err(materialized_query_error)?
                    .into_iter()
                    .find(|page| page.name == name && SyncPageKind::from(page.kind) == page_kind)
                    .map(sync_page);
                Ok(SyncRuntimeQueryReply::Page(page))
            }
            SyncRuntimeQueryRequest::ListPages { page_kind, limit } => {
                let pages = read
                    .pages(page_kind.map(Into::into), limit)
                    .map_err(materialized_query_error)?
                    .into_iter()
                    .map(sync_page)
                    .collect();
                Ok(SyncRuntimeQueryReply::Pages(pages))
            }
            SyncRuntimeQueryRequest::LoadPage {
                page_id,
                block_limit,
            } => {
                let page_id = parse_page_id(&page_id)?;
                let Some(page) = read.page(page_id).map_err(materialized_query_error)? else {
                    return Ok(SyncRuntimeQueryReply::PageWithBlocks(None));
                };
                let blocks = read
                    .blocks_on_page(page_id, block_limit)
                    .map_err(materialized_query_error)?
                    .into_iter()
                    .map(sync_block)
                    .collect();
                Ok(SyncRuntimeQueryReply::PageWithBlocks(Some(
                    SyncPageWithBlocksDto {
                        page: sync_page(page),
                        blocks,
                    },
                )))
            }
            SyncRuntimeQueryRequest::Search { query, limit } => Ok(SyncRuntimeQueryReply::Search(
                read.search(&query, limit)
                    .map_err(materialized_query_error)?
                    .into_iter()
                    .map(sync_search_hit)
                    .collect(),
            )),
            SyncRuntimeQueryRequest::PropertiesForOwner { owner, limit } => {
                let owner = parse_entity_id(owner)?;
                Ok(SyncRuntimeQueryReply::Properties(
                    read.properties(owner, limit)
                        .map_err(materialized_query_error)?
                        .into_iter()
                        .map(sync_property)
                        .collect(),
                ))
            }
            SyncRuntimeQueryRequest::PropertiesNamed { name, value, limit } => {
                Ok(SyncRuntimeQueryReply::Properties(
                    read.properties_named(&name, value.as_deref(), limit)
                        .map_err(materialized_query_error)?
                        .into_iter()
                        .map(sync_property)
                        .collect(),
                ))
            }
            SyncRuntimeQueryRequest::Tags { tag, limit } => Ok(SyncRuntimeQueryReply::Tags(
                read.tags(&tag, limit)
                    .map_err(materialized_query_error)?
                    .into_iter()
                    .map(sync_tag)
                    .collect(),
            )),
            SyncRuntimeQueryRequest::Tasks { marker, limit } => Ok(SyncRuntimeQueryReply::Tasks(
                read.tasks(marker.as_deref(), limit)
                    .map_err(materialized_query_error)?
                    .into_iter()
                    .map(sync_task)
                    .collect(),
            )),
            SyncRuntimeQueryRequest::ReferencesToPageName { name, limit } => {
                let name = crate::oplog::LogicalPageName::parse(name)
                    .map_err(|error| SyncRuntimeRequestError::InvalidRequest(error.to_string()))?;
                let store = runtime.engine().archive_store().ok_or_else(|| {
                    SyncRuntimeRequestError::ActorRefused(
                        "promoted runtime has no retained archive capability".into(),
                    )
                })?;
                let mut query = runtime
                    .database()
                    .frontier_reference_query(runtime.engine(), store)
                    .map_err(|error| SyncRuntimeRequestError::ActorRefused(error.to_string()))?;
                Ok(SyncRuntimeQueryReply::References(
                    query
                        .references_to_page_name(&name, limit)
                        .map_err(|error| SyncRuntimeRequestError::ActorRefused(error.to_string()))?
                        .hits
                        .into_iter()
                        .map(sync_reference_hit)
                        .collect(),
                ))
            }
            SyncRuntimeQueryRequest::ReferencesToLogseqUuid { logseq_uuid, limit } => {
                let uuid = uuid::Uuid::parse_str(&logseq_uuid)
                    .map(LogseqUuid::from_uuid)
                    .map_err(|error| SyncRuntimeRequestError::InvalidRequest(error.to_string()))?;
                let store = runtime.engine().archive_store().ok_or_else(|| {
                    SyncRuntimeRequestError::ActorRefused(
                        "promoted runtime has no retained archive capability".into(),
                    )
                })?;
                let mut query = runtime
                    .database()
                    .frontier_reference_query(runtime.engine(), store)
                    .map_err(|error| SyncRuntimeRequestError::ActorRefused(error.to_string()))?;
                Ok(SyncRuntimeQueryReply::References(
                    query
                        .references_to_logseq_uuid(uuid, limit)
                        .map_err(|error| SyncRuntimeRequestError::ActorRefused(error.to_string()))?
                        .hits
                        .into_iter()
                        .map(sync_reference_hit)
                        .collect(),
                ))
            }
        }
    }

    fn tick(&mut self) -> SyncRuntimeTick {
        if let Some(outcome) = self.advance_local_mutation_once() {
            return SyncRuntimeTick::LocalMutation(outcome);
        }
        self.tick_external_feed()
    }

    fn tick_external_feed(&mut self) -> SyncRuntimeTick {
        if let Some(detail) = &self.terminal {
            return SyncRuntimeTick::Terminal(detail.clone());
        }
        let observed_at = match current_timestamp() {
            Ok(timestamp) => timestamp,
            Err(detail) => {
                let result = SyncRuntimeTick::Failed(detail);
                self.last_tick = Some(result.clone());
                return result;
            }
        };
        let result = {
            let Some(authority) = self.authority.as_mut() else {
                return SyncRuntimeTick::Terminal("runtime authority was dropped".into());
            };
            let Some(runtime) = self.runtime.as_mut() else {
                return SyncRuntimeTick::Terminal("promoted runtime was dropped".into());
            };
            let Some(feed) = self.feed.as_mut() else {
                return SyncRuntimeTick::Terminal("exact external feed was dropped".into());
            };
            map_tick(feed.drain_one(&self.graph, &self.receipts, authority, runtime, observed_at))
        };
        self.refresh_watcher();
        if let SyncRuntimeTick::Terminal(detail) = &result {
            self.latch_terminal(detail.clone());
        }
        self.last_tick = Some(result.clone());
        result
    }

    fn submit_local_mutation(
        &mut self,
        transaction: OperationTransaction,
    ) -> SyncLocalMutationOutcome {
        if self.terminal.is_some() {
            let (batch_id, phase) = self
                .local_mutation
                .as_ref()
                .map(pending_local_identity)
                .unwrap_or((None, SyncLocalMutationPhase::Bindings));
            return SyncLocalMutationOutcome::Revoked { batch_id, phase };
        }
        if self.local_mutation.is_some() {
            let prior =
                self.advance_local_mutation_once()
                    .unwrap_or(SyncLocalMutationOutcome::Blocked {
                        batch_id: None,
                        phase: SyncLocalMutationPhase::Bindings,
                        reason: SyncLocalMutationBlock::PriorMutationUnresolved,
                    });
            if self.local_mutation.is_some() {
                let (batch_id, phase) = local_outcome_identity(prior);
                return SyncLocalMutationOutcome::Blocked {
                    batch_id,
                    phase,
                    reason: SyncLocalMutationBlock::PriorMutationUnresolved,
                };
            }
        }
        self.execute_local_transaction(transaction)
    }

    fn execute_local_transaction(
        &mut self,
        transaction: OperationTransaction,
    ) -> SyncLocalMutationOutcome {
        let state = {
            let Some(authority) = self.authority.as_mut() else {
                return SyncLocalMutationOutcome::Revoked {
                    batch_id: None,
                    phase: SyncLocalMutationPhase::Bindings,
                };
            };
            let Some(runtime) = self.runtime.as_mut() else {
                return SyncLocalMutationOutcome::Revoked {
                    batch_id: None,
                    phase: SyncLocalMutationPhase::Bindings,
                };
            };
            let mut session = match runtime.admit_promoted_mutation(authority, &self.graph) {
                Ok(session) => session,
                Err(_) => {
                    let revoked = runtime.workspace_authority_revocation().is_some();
                    if revoked {
                        self.latch_terminal("local mutation runtime authority was revoked".into());
                    }
                    return if revoked {
                        SyncLocalMutationOutcome::Revoked {
                            batch_id: None,
                            phase: SyncLocalMutationPhase::Bindings,
                        }
                    } else {
                        SyncLocalMutationOutcome::Blocked {
                            batch_id: None,
                            phase: SyncLocalMutationPhase::Bindings,
                            reason: SyncLocalMutationBlock::Prepublication,
                        }
                    };
                }
            };
            OperationalCoordinator::execute_local(
                &mut session,
                &self.graph,
                &self.receipts,
                &transaction,
            )
        };
        self.retain_local_state(state, Some(transaction))
    }

    fn advance_local_mutation_once(&mut self) -> Option<SyncLocalMutationOutcome> {
        let pending = self.local_mutation.take()?;
        if self.terminal.is_some() {
            let (batch_id, phase) = pending_local_identity(&pending);
            self.local_mutation = Some(pending);
            return Some(SyncLocalMutationOutcome::Revoked { batch_id, phase });
        }
        match pending {
            PendingLocalMutation::Reconciliation { transaction } => {
                if self.last_watcher.pending {
                    let tick = self.tick_external_feed();
                    if matches!(tick, SyncRuntimeTick::Terminal(_)) {
                        self.local_mutation =
                            Some(PendingLocalMutation::Reconciliation { transaction });
                        return Some(SyncLocalMutationOutcome::Revoked {
                            batch_id: None,
                            phase: SyncLocalMutationPhase::Capture,
                        });
                    }
                }
                if self.last_watcher.pending {
                    self.local_mutation =
                        Some(PendingLocalMutation::Reconciliation { transaction });
                    Some(SyncLocalMutationOutcome::RetryableRetainedRecovery {
                        batch_id: None,
                        phase: SyncLocalMutationPhase::Capture,
                    })
                } else {
                    Some(self.execute_local_transaction(transaction))
                }
            }
            PendingLocalMutation::Published(continuation) => {
                let state = {
                    let Some(authority) = self.authority.as_mut() else {
                        self.local_mutation = Some(PendingLocalMutation::Published(continuation));
                        return Some(SyncLocalMutationOutcome::Revoked {
                            batch_id: None,
                            phase: SyncLocalMutationPhase::Bindings,
                        });
                    };
                    let Some(runtime) = self.runtime.as_mut() else {
                        self.local_mutation = Some(PendingLocalMutation::Published(continuation));
                        return Some(SyncLocalMutationOutcome::Revoked {
                            batch_id: None,
                            phase: SyncLocalMutationPhase::Bindings,
                        });
                    };
                    let mut session = match runtime.admit_promoted_mutation(authority, &self.graph)
                    {
                        Ok(session) => session,
                        Err(_) => {
                            let batch_id = continuation.batch_id();
                            let phase = map_local_phase(continuation.phase());
                            let revoked = runtime.workspace_authority_revocation().is_some();
                            self.local_mutation =
                                Some(PendingLocalMutation::Published(continuation));
                            if revoked {
                                self.latch_terminal(
                                    "local mutation runtime authority was revoked".into(),
                                );
                            }
                            return Some(if revoked {
                                SyncLocalMutationOutcome::Revoked {
                                    batch_id: Some(batch_id),
                                    phase,
                                }
                            } else {
                                SyncLocalMutationOutcome::RetryableRetainedRecovery {
                                    batch_id: Some(batch_id),
                                    phase,
                                }
                            });
                        }
                    };
                    OperationalCoordinator::retry_local(
                        &mut session,
                        &self.graph,
                        &self.receipts,
                        continuation,
                    )
                };
                Some(self.retain_local_state(state, None))
            }
        }
    }

    fn retain_local_state(
        &mut self,
        state: LocalMutationCoordinatorState,
        transaction: Option<OperationTransaction>,
    ) -> SyncLocalMutationOutcome {
        match state {
            LocalMutationCoordinatorState::Active(completion) => {
                SyncLocalMutationOutcome::Durable {
                    batch_id: completion.batch_id(),
                }
            }
            LocalMutationCoordinatorState::Recovering(
                LocalMutationRecovery::ReconciliationRequired(reconciliation),
            ) => {
                let observations = reconciliation
                    .paths()
                    .iter()
                    .cloned()
                    .map(WatcherObservation::ManagedPath);
                let observed = match (self.feed.as_mut(), self.runtime.as_ref()) {
                    (Some(feed), Some(runtime)) => feed.observe(&self.graph, runtime, observations),
                    _ => Err(ExactExternalFeedObserveError::Terminal),
                };
                self.refresh_watcher();
                if observed.is_err() {
                    self.latch_terminal(
                        "local mutation reconciliation could not enter the exact feed".into(),
                    );
                    return SyncLocalMutationOutcome::Revoked {
                        batch_id: None,
                        phase: SyncLocalMutationPhase::Capture,
                    };
                }
                let Some(transaction) = transaction else {
                    return SyncLocalMutationOutcome::Blocked {
                        batch_id: None,
                        phase: SyncLocalMutationPhase::Capture,
                        reason: SyncLocalMutationBlock::Prepublication,
                    };
                };
                self.local_mutation = Some(PendingLocalMutation::Reconciliation { transaction });
                SyncLocalMutationOutcome::RetryableRetainedRecovery {
                    batch_id: None,
                    phase: SyncLocalMutationPhase::Capture,
                }
            }
            LocalMutationCoordinatorState::Recovering(LocalMutationRecovery::Published(
                continuation,
            )) => {
                let batch_id = continuation.batch_id();
                let phase = map_local_phase(continuation.phase());
                self.local_mutation = Some(PendingLocalMutation::Published(continuation));
                SyncLocalMutationOutcome::RetryableRetainedRecovery {
                    batch_id: Some(batch_id),
                    phase,
                }
            }
            LocalMutationCoordinatorState::Blocked(blocked) => {
                let phase = map_local_phase(blocked.failure().phase());
                let reason = match blocked.reason() {
                    LocalMutationBlockReason::Prepublication => {
                        SyncLocalMutationBlock::Prepublication
                    }
                    LocalMutationBlockReason::Retained(_) => {
                        SyncLocalMutationBlock::RetainedPublished
                    }
                };
                let continuation = blocked.into_continuation();
                let batch_id = continuation
                    .as_ref()
                    .map(LocalPublishedContinuation::batch_id);
                if let Some(continuation) = continuation {
                    self.local_mutation = Some(PendingLocalMutation::Published(continuation));
                }
                SyncLocalMutationOutcome::Blocked {
                    batch_id,
                    phase,
                    reason,
                }
            }
            LocalMutationCoordinatorState::Revoked(revoked) => {
                let phase = map_local_phase(revoked.failure().phase());
                let continuation = revoked.into_continuation();
                let batch_id = continuation
                    .as_ref()
                    .map(LocalPublishedContinuation::batch_id);
                if let Some(continuation) = continuation {
                    self.local_mutation = Some(PendingLocalMutation::Published(continuation));
                }
                self.latch_terminal("local mutation runtime authority was revoked".into());
                SyncLocalMutationOutcome::Revoked { batch_id, phase }
            }
        }
    }

    fn clean_shutdown(&mut self) -> Result<SyncShutdownOutcome, SyncRuntimeRequestError> {
        if self.local_mutation.is_some() {
            if self.terminal.is_some() {
                return Err(SyncRuntimeRequestError::ActorRefused(
                    "clean shutdown refused by a revoked local mutation".into(),
                ));
            }
            let outcome = self.advance_local_mutation_once();
            if self.local_mutation.is_some() {
                let detail = match outcome {
                    Some(SyncLocalMutationOutcome::Blocked { .. }) => {
                        "clean shutdown refused by a retained blocked local mutation"
                    }
                    Some(SyncLocalMutationOutcome::Revoked { .. }) => {
                        "clean shutdown refused by a revoked local mutation"
                    }
                    _ => "clean shutdown awaits retained local mutation recovery",
                };
                return Err(SyncRuntimeRequestError::ActorRefused(detail.into()));
            }
        }
        if self.terminal.is_some() {
            return Ok(SyncShutdownOutcome::Terminal(self.snapshot()));
        }

        for _ in 0..MAX_CLEAN_DRAIN_TURNS {
            let tick = self.tick_external_feed();
            match tick {
                SyncRuntimeTick::Idle if !self.last_watcher.pending => break,
                SyncRuntimeTick::AdmittedNoop { .. }
                | SyncRuntimeTick::AdmittedComplete { .. }
                | SyncRuntimeTick::LocalMutation(_)
                | SyncRuntimeTick::Recovering
                | SyncRuntimeTick::RetryFull
                | SyncRuntimeTick::Failed(_) => continue,
                SyncRuntimeTick::Idle => continue,
                SyncRuntimeTick::RecoveryBlocked(detail) | SyncRuntimeTick::Terminal(detail) => {
                    return Err(SyncRuntimeRequestError::ActorRefused(detail));
                }
                SyncRuntimeTick::Blocked(detail) => {
                    return Err(SyncRuntimeRequestError::ActorRefused(detail));
                }
            }
        }
        if self.last_watcher.pending {
            return Err(SyncRuntimeRequestError::ActorRefused(
                "clean shutdown could not settle the bounded watcher queue".into(),
            ));
        }
        if self.local_mutation.is_some() {
            return Err(SyncRuntimeRequestError::ActorRefused(
                "clean shutdown awaits retained local mutation recovery".into(),
            ));
        }
        let authority = self
            .authority
            .as_mut()
            .ok_or(SyncRuntimeRequestError::ActorUnavailable)?;
        let runtime = self
            .runtime
            .as_mut()
            .ok_or(SyncRuntimeRequestError::ActorUnavailable)?;
        runtime
            .quiesce_and_mark_safe(authority, &self.graph)
            .map_err(|error| SyncRuntimeRequestError::ActorRefused(error.to_string()))?;
        self.refresh_watcher();
        self.stopped_safe = true;
        let snapshot = self.snapshot();
        Ok(SyncShutdownOutcome::Safe(snapshot))
    }

    fn latch_terminal(&mut self, detail: String) {
        self.refresh_watcher();
        self.terminal = Some(detail);
        self.feed.take();
        self.authority.take();
        self.runtime.take();
    }

    fn refresh_watcher(&mut self) {
        if let Some(runtime) = &self.runtime {
            self.last_watcher = map_watcher(runtime.watcher_status());
        }
    }

    fn snapshot(&self) -> SyncRuntimeStatusSnapshot {
        SyncRuntimeStatusSnapshot {
            lifecycle: if self.stopped_safe {
                SyncRuntimeLifecycle::StoppedSafe
            } else if self.terminal.is_some() {
                SyncRuntimeLifecycle::Terminal
            } else {
                SyncRuntimeLifecycle::Active
            },
            recovery: Some(self.recovery),
            watcher: self.last_watcher,
            last_tick: self.last_tick.clone(),
            detail: self.terminal.clone(),
        }
    }
}

fn materialized_query_error(error: impl fmt::Display) -> SyncRuntimeRequestError {
    SyncRuntimeRequestError::ActorRefused(error.to_string())
}

fn parse_page_id(value: &str) -> Result<PageId, SyncRuntimeRequestError> {
    uuid::Uuid::parse_str(value)
        .map(PageId::from_uuid)
        .map_err(|error| SyncRuntimeRequestError::InvalidRequest(error.to_string()))
}

fn parse_block_id(value: &str) -> Result<BlockId, SyncRuntimeRequestError> {
    uuid::Uuid::parse_str(value)
        .map(BlockId::from_uuid)
        .map_err(|error| SyncRuntimeRequestError::InvalidRequest(error.to_string()))
}

fn parse_entity_id(value: SyncEntityId) -> Result<MaterializedEntityId, SyncRuntimeRequestError> {
    match value {
        SyncEntityId::Page(value) => parse_page_id(&value).map(MaterializedEntityId::Page),
        SyncEntityId::Block(value) => parse_block_id(&value).map(MaterializedEntityId::Block),
    }
}

fn sync_page(row: MaterializedPageRow) -> SyncPageDto {
    SyncPageDto {
        page_id: row.page_id.to_string(),
        home_document_id: row.home_document_id.to_string(),
        name: row.name,
        path: row.path.to_string(),
        kind: row.kind.into(),
        preamble: row.preamble,
    }
}

fn sync_block(row: MaterializedBlockRow) -> SyncBlockDto {
    SyncBlockDto {
        block_id: row.block_id.to_string(),
        page_id: row.page_id.to_string(),
        home_document_id: row.home_document_id.to_string(),
        parent_block_id: row.parent.map(|id| id.to_string()),
        order: row.order,
        content: row.content,
        heading_level: row.heading_level,
        collapsed: row.collapsed,
        logseq_uuid: row.logseq_uuid.map(|id| id.to_string()),
    }
}

fn sync_property(row: MaterializedPropertyRow) -> SyncPropertyDto {
    SyncPropertyDto {
        owner: row.owner.into(),
        page_id: row.page_id.to_string(),
        name: row.name,
        value: row.value,
    }
}

fn sync_tag(row: MaterializedTagRow) -> SyncTagDto {
    SyncTagDto {
        owner: row.owner.into(),
        page_id: row.page_id.to_string(),
        tag: row.tag,
    }
}

fn sync_task(row: MaterializedTaskRow) -> SyncTaskDto {
    SyncTaskDto {
        block_id: row.block_id.to_string(),
        page_id: row.page_id.to_string(),
        marker: row.marker,
        priority: row.priority,
        scheduled: row.scheduled,
        deadline: row.deadline,
    }
}

fn sync_search_hit(row: MaterializedSearchHit) -> SyncSearchHitDto {
    SyncSearchHitDto {
        entity: row.entity.into(),
        page_id: row.page_id.to_string(),
        text: row.text,
        rank: row.rank,
    }
}

fn sync_reference_source(source: ReferenceSourceLocatorV1) -> SyncReferenceSourceDto {
    match source {
        ReferenceSourceLocatorV1::Preamble => SyncReferenceSourceDto::Preamble,
        ReferenceSourceLocatorV1::Block {
            block_id,
            home_document_id,
        } => SyncReferenceSourceDto::Block {
            block_id: block_id.to_string(),
            home_document_id: home_document_id.to_string(),
        },
    }
}

fn sync_reference_hit(hit: FrontierReferenceHit) -> SyncReferenceHitDto {
    let (source, kind, raw_target, byte_start, byte_end) = match hit.fact {
        ReferenceFactV1::PageName(fact) => (
            sync_reference_source(fact.source),
            format!("{:?}", fact.kind).to_lowercase(),
            fact.raw_target,
            fact.byte_start,
            fact.byte_end,
        ),
        ReferenceFactV1::Block(fact) => (
            sync_reference_source(fact.source),
            format!("{:?}", fact.kind).to_lowercase(),
            fact.raw_claim,
            fact.byte_start,
            fact.byte_end,
        ),
    };
    SyncReferenceHitDto {
        source_page_id: hit.source_page_id.to_string(),
        source,
        kind,
        raw_target,
        byte_start,
        byte_end,
        resolved_page_id: hit.resolved_page_id.map(|id| id.to_string()),
        resolved_block_id: hit.resolved_block_id.map(|id| id.to_string()),
    }
}

fn current_timestamp() -> Result<BaselineTimestamp, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock precedes Unix epoch: {error}"))?
        .as_millis();
    let millis =
        u64::try_from(millis).map_err(|_| "system timestamp exceeds u64 range".to_owned())?;
    BaselineTimestamp::from_millis(millis).map_err(display)
}

fn display(error: impl fmt::Display) -> String {
    error.to_string()
}

fn map_recovery(recovery: RuntimeRecoveryState) -> SyncRuntimeRecovery {
    match recovery {
        RuntimeRecoveryState::FirstPromotion => SyncRuntimeRecovery::FirstPromotion,
        RuntimeRecoveryState::ResumedOwnUnsafe => SyncRuntimeRecovery::ResumedOwnUnsafe,
        RuntimeRecoveryState::AdoptedSafeHandoff => SyncRuntimeRecovery::AdoptedSafeHandoff,
        RuntimeRecoveryState::TookOverCrashedUnsafe { .. } => {
            SyncRuntimeRecovery::TookOverCrashedUnsafe
        }
    }
}

fn map_local_phase(phase: OperationalPhase) -> SyncLocalMutationPhase {
    match phase {
        OperationalPhase::Bindings => SyncLocalMutationPhase::Bindings,
        OperationalPhase::Planning => SyncLocalMutationPhase::Planning,
        OperationalPhase::Draft => SyncLocalMutationPhase::Draft,
        OperationalPhase::Capture => SyncLocalMutationPhase::Capture,
        OperationalPhase::Finalize => SyncLocalMutationPhase::Finalize,
        OperationalPhase::TailReservation => SyncLocalMutationPhase::TailReservation,
        OperationalPhase::Publication => SyncLocalMutationPhase::Publication,
        OperationalPhase::ArchiveStage => SyncLocalMutationPhase::ArchiveStage,
        OperationalPhase::TailAdmission => SyncLocalMutationPhase::TailAdmission,
        OperationalPhase::SqliteDrain => SyncLocalMutationPhase::SqliteDrain,
        OperationalPhase::ProjectionDrain => SyncLocalMutationPhase::ProjectionDrain,
    }
}

fn pending_local_identity(
    pending: &PendingLocalMutation,
) -> (Option<BatchId>, SyncLocalMutationPhase) {
    match pending {
        PendingLocalMutation::Reconciliation { .. } => (None, SyncLocalMutationPhase::Capture),
        PendingLocalMutation::Published(continuation) => (
            Some(continuation.batch_id()),
            map_local_phase(continuation.phase()),
        ),
    }
}

fn local_outcome_identity(
    outcome: SyncLocalMutationOutcome,
) -> (Option<BatchId>, SyncLocalMutationPhase) {
    match outcome {
        SyncLocalMutationOutcome::Durable { batch_id } => {
            (Some(batch_id), SyncLocalMutationPhase::ProjectionDrain)
        }
        SyncLocalMutationOutcome::RetryableRetainedRecovery { batch_id, phase }
        | SyncLocalMutationOutcome::Blocked {
            batch_id, phase, ..
        }
        | SyncLocalMutationOutcome::Revoked { batch_id, phase } => (batch_id, phase),
    }
}

fn map_watcher(status: crate::oplog::watcher_queue::WatcherQueueStatus) -> SyncWatcherStatus {
    SyncWatcherStatus {
        latest_enqueue: status.latest_enqueue.sequence(),
        acknowledged: status.acknowledged.sequence(),
        drain_in_flight: status.drain_in_flight.is_some(),
        pending: status.pending,
        pending_requires_full_scan: status.pending_requires_full_scan,
        deferred: status.deferred,
        quiescing: status.quiescing,
        sequence_exhausted: status.sequence_exhausted,
    }
}

fn map_tick(drain: ExactExternalFeedDrain) -> SyncRuntimeTick {
    match drain {
        ExactExternalFeedDrain::Idle => SyncRuntimeTick::Idle,
        ExactExternalFeedDrain::ForeignActor => {
            SyncRuntimeTick::Terminal("exact feed refused its actor-owned authority pair".into())
        }
        ExactExternalFeedDrain::RecoveryBlocked(reason) => {
            SyncRuntimeTick::RecoveryBlocked(reason.into())
        }
        ExactExternalFeedDrain::Recovering => SyncRuntimeTick::Recovering,
        ExactExternalFeedDrain::RetryFull => SyncRuntimeTick::RetryFull,
        ExactExternalFeedDrain::Blocked(detail) => SyncRuntimeTick::Blocked(detail),
        ExactExternalFeedDrain::Failed(detail) => SyncRuntimeTick::Failed(detail),
        ExactExternalFeedDrain::AdmittedNoop { epoch } => SyncRuntimeTick::AdmittedNoop { epoch },
        ExactExternalFeedDrain::AdmittedComplete { epoch } => {
            SyncRuntimeTick::AdmittedComplete { epoch }
        }
        ExactExternalFeedDrain::Terminal(terminal) => {
            SyncRuntimeTick::Terminal(terminal.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::enrollment::EnrollmentDiscoveryHandoff;
    use crate::oplog::exact_external_feed::tests::RuntimeHostFixture;
    use crate::oplog::{
        BlockId, BlockLocation, DocumentId, LogicalPageName, ManagedTextKind, PageId, PageRename,
    };
    use std::fs;
    use std::path::Path;
    use std::sync::Barrier;
    use uuid::Uuid;

    fn empty_request(profile: SyncStorageProfile) -> (PathBuf, SyncRuntimeOpenRequest) {
        let root = std::env::temp_dir().join(format!("tine-sync-runtime-empty-{}", Uuid::new_v4()));
        let graph_root = root.join("graph");
        fs::create_dir_all(&graph_root).unwrap();
        (
            root.clone(),
            SyncRuntimeOpenRequest {
                profile,
                graph_root,
                enrollment_root: root.join("enrollment"),
                archive_root: root.join("archive"),
                receipt_root: root.join("receipts"),
                database_path: root.join("projection.sqlite"),
                application_runtime_root: root.join("application-runtime"),
            },
        )
    }

    #[test]
    fn handle_is_cloneable_send_and_sync_while_actor_is_not_send_or_sync() {
        fn assert_send_sync_clone<T: Send + Sync + Clone>() {}
        assert_send_sync_clone::<SyncRuntimeHandle>();
        let _public_submit: fn(
            &SyncRuntimeHandle,
            OperationTransaction,
        )
            -> Result<SyncLocalMutationOutcome, SyncLocalMutationRequestError> =
            SyncRuntimeHandle::submit_local_mutation;

        trait AmbiguousIfSend<Marker> {
            fn assert_not_send() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
        <RuntimeActor as AmbiguousIfSend<_>>::assert_not_send();

        trait AmbiguousIfSync<Marker> {
            fn assert_not_sync() {}
        }
        impl<T: ?Sized> AmbiguousIfSync<()> for T {}
        impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}
        <RuntimeActor as AmbiguousIfSync<_>>::assert_not_sync();
    }

    #[test]
    fn legacy_and_absent_discovery_start_no_actor_and_create_nothing() {
        let before = ACTOR_THREADS_STARTED.load(std::sync::atomic::Ordering::SeqCst);
        let (legacy_root, legacy_request) = empty_request(SyncStorageProfile::LegacyDefault);
        let legacy = SyncRuntimeHandle::open(legacy_request);
        assert_eq!(legacy.status, SyncRuntimeOpenStatus::LegacyDefault);
        assert!(legacy.handle.is_none());
        assert!(!legacy_root.join("enrollment").exists());
        assert!(!legacy_root.join("archive").exists());
        assert!(!legacy_root.join("application-runtime").exists());

        let (absent_root, absent_request) = empty_request(SyncStorageProfile::ExperimentalLocal);
        let absent = SyncRuntimeHandle::open(absent_request);
        assert_eq!(absent.status, SyncRuntimeOpenStatus::Absent);
        assert!(absent.handle.is_none());
        assert!(!absent_root.join("enrollment").exists());
        assert!(!absent_root.join("archive").exists());
        assert!(!absent_root.join("application-runtime").exists());
        assert_eq!(
            ACTOR_THREADS_STARTED.load(std::sync::atomic::Ordering::SeqCst),
            before
        );

        let _ = fs::remove_dir_all(legacy_root);
        let _ = fs::remove_dir_all(absent_root);
    }

    #[test]
    fn oversized_watcher_refusal_retains_a_full_scan_before_safe_shutdown() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-watcher-request-bounds");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);
        let before = handle.status().unwrap();
        let path = "content/nested pages/oversize watcher batch.md";
        let file = fixture.graph_root().join(path);
        fs::write(
            &file,
            b"- external edit hidden behind an oversized callback\n",
        )
        .unwrap();
        let manifests_before = fixture.manifest_count();
        let observations = std::iter::once(SyncWatcherObservation::managed_path(path).unwrap())
            .chain((0..MAX_WATCHER_OBSERVATIONS).map(|_| SyncWatcherObservation::UnknownPath))
            .collect::<Vec<_>>();
        assert!(matches!(
            handle.observe_watcher(observations),
            Err(SyncRuntimeRequestError::RequestTooLarge {
                observations,
                path_bytes,
            }) if observations == MAX_WATCHER_OBSERVATIONS + 1 && path_bytes == path.len()
        ));
        let rejected = handle.status().unwrap();
        assert!(rejected.watcher.pending);
        assert!(rejected.watcher.pending_requires_full_scan);
        assert!(
            rejected.watcher.latest_enqueue > before.watcher.latest_enqueue,
            "the rejection must retain exactly one runtime-owned full-scan obligation"
        );
        let outcome = handle.clean_shutdown().unwrap();
        assert!(matches!(outcome, SyncShutdownOutcome::Safe(_)));
        assert_eq!(
            fixture.manifest_count(),
            manifests_before + 1,
            "Safe may be published only after the refused callback's full scan admits its edit"
        );
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Safe
        ));
    }

    #[test]
    fn watcher_request_count_boundary_is_accepted_and_drained() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-watcher-request-count-boundary");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);

        handle
            .observe_watcher(
                (0..MAX_WATCHER_OBSERVATIONS)
                    .map(|_| SyncWatcherObservation::UnknownPath)
                    .collect(),
            )
            .unwrap();
        let pending = handle.status().unwrap();
        assert!(pending.watcher.pending);
        assert!(pending.watcher.pending_requires_full_scan);
        assert!(matches!(
            handle.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(snapshot) if !snapshot.watcher.pending
        ));
    }

    #[test]
    fn watcher_request_path_byte_overflow_is_retained() {
        let overflow_path = managed_path_with_bytes(MAX_WATCHER_PATH_BYTES + 1);
        let rejected_fixture = RuntimeHostFixture::safe("sync-runtime-watcher-path-overflow");
        let rejected = active_handle(SyncRuntimeHandle::open(rejected_fixture.request()));
        drive_initial_feed(&rejected);
        assert!(matches!(
            rejected.observe_watcher(vec![SyncWatcherObservation::managed_path(overflow_path).unwrap()]),
            Err(SyncRuntimeRequestError::RequestTooLarge {
                observations: 1,
                path_bytes,
            }) if path_bytes == MAX_WATCHER_PATH_BYTES + 1
        ));
        let status = rejected.status().unwrap();
        assert!(status.watcher.pending);
        assert!(status.watcher.pending_requires_full_scan);
        assert!(matches!(
            rejected.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(snapshot) if !snapshot.watcher.pending
        ));
    }

    fn managed_path_with_bytes(path_bytes: usize) -> String {
        const PREFIX: &str = "pages/";
        const SUFFIX: &str = ".md";
        assert!(path_bytes >= PREFIX.len() + SUFFIX.len());
        format!(
            "{PREFIX}{}{SUFFIX}",
            "a".repeat(path_bytes - PREFIX.len() - SUFFIX.len())
        )
    }

    fn active_handle(opened: SyncRuntimeOpenResult) -> SyncRuntimeHandle {
        assert_eq!(opened.status, SyncRuntimeOpenStatus::Active);
        opened.handle.expect("active startup must return a handle")
    }

    fn drive_initial_feed(handle: &SyncRuntimeHandle) {
        for _ in 0..128 {
            match handle.tick().unwrap() {
                SyncRuntimeTick::Idle => {
                    if !handle.status().unwrap().watcher.pending {
                        return;
                    }
                }
                SyncRuntimeTick::AdmittedNoop { .. } | SyncRuntimeTick::AdmittedComplete { .. } => {
                    if !handle.status().unwrap().watcher.pending {
                        return;
                    }
                }
                SyncRuntimeTick::Recovering
                | SyncRuntimeTick::RetryFull
                | SyncRuntimeTick::Failed(_) => {}
                other => panic!("initial exact feed did not settle: {other:?}"),
            }
        }
        panic!("initial exact feed exceeded the bounded test turn budget");
    }

    fn submit_durable(handle: &SyncRuntimeHandle, operations: Vec<SemanticOperation>) -> BatchId {
        let transaction = OperationTransaction::new(operations).unwrap();
        match handle.submit_local_mutation(transaction).unwrap() {
            SyncLocalMutationOutcome::Durable { batch_id } => batch_id,
            SyncLocalMutationOutcome::RetryableRetainedRecovery { .. } => {
                settle_local_mutation(handle)
            }
            other => panic!("local mutation did not complete durably: {other:?}"),
        }
    }

    fn settle_local_mutation(handle: &SyncRuntimeHandle) -> BatchId {
        for _ in 0..128 {
            match handle.tick().unwrap() {
                SyncRuntimeTick::LocalMutation(SyncLocalMutationOutcome::Durable { batch_id }) => {
                    return batch_id
                }
                SyncRuntimeTick::LocalMutation(
                    SyncLocalMutationOutcome::RetryableRetainedRecovery { .. },
                )
                | SyncRuntimeTick::Recovering
                | SyncRuntimeTick::RetryFull
                | SyncRuntimeTick::Failed(_)
                | SyncRuntimeTick::Idle
                | SyncRuntimeTick::AdmittedNoop { .. }
                | SyncRuntimeTick::AdmittedComplete { .. } => {}
                other => panic!("local mutation did not complete durably: {other:?}"),
            }
        }
        panic!("local mutation exceeded the bounded test retry budget");
    }

    fn snapshot_graph_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        fn visit(root: &Path, directory: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
            let mut entries = fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect::<Vec<_>>();
            entries.sort_unstable_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                if path.is_dir() {
                    visit(root, &path, files);
                } else {
                    files.push((
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        fs::read(path).unwrap(),
                    ));
                }
            }
        }

        let mut files = Vec::new();
        visit(root, root, &mut files);
        files
    }

    #[test]
    fn local_mutation_request_budgets_are_inclusive_and_diagnostics_are_capped() {
        for limit in [
            MAX_LOCAL_MUTATION_ROWS,
            MAX_LOCAL_MUTATION_REFERENCED_PATHS,
            MAX_LOCAL_MUTATION_PATH_BYTES,
            MAX_LOCAL_MUTATION_TEXT_BYTES,
        ] {
            assert_eq!(bounded_charge(0, limit, limit), limit);
            assert_eq!(bounded_charge(0, limit.saturating_add(1), limit), limit + 1);
            assert_eq!(bounded_charge(limit, usize::MAX, limit), limit + 1);
        }

        let exact = OperationTransaction {
            operations: vec![SemanticOperation::SetPagePreamble {
                page_id: PageId::from_uuid(Uuid::from_u128(700_000)),
                preamble: Some("x".repeat(MAX_LOCAL_MUTATION_TEXT_BYTES)),
            }],
        };
        assert_eq!(
            bounded_local_mutation_size(&exact).text_bytes,
            MAX_LOCAL_MUTATION_TEXT_BYTES
        );
        assert!(validate_local_mutation_request(exact).is_ok());

        let oversized = OperationTransaction {
            operations: vec![SemanticOperation::SetPagePreamble {
                page_id: PageId::from_uuid(Uuid::from_u128(700_001)),
                preamble: Some("x".repeat(MAX_LOCAL_MUTATION_TEXT_BYTES + 1)),
            }],
        };
        assert_eq!(
            validate_local_mutation_request(oversized),
            Err(SyncLocalMutationRequestError::RequestTooLarge(
                SyncLocalMutationRequestSize {
                    rows: 1,
                    referenced_paths: 0,
                    path_bytes: 0,
                    text_bytes: MAX_LOCAL_MUTATION_TEXT_BYTES + 1,
                }
            ))
        );

        let too_many_rows = OperationTransaction {
            operations: (0..=MAX_LOCAL_MUTATION_ROWS)
                .map(|index| SemanticOperation::SetPageKind {
                    page_id: PageId::from_uuid(Uuid::from_u128(710_000 + index as u128)),
                    kind: ManagedTextKind::Page,
                })
                .collect(),
        };
        assert_eq!(
            bounded_local_mutation_size(&too_many_rows).rows,
            MAX_LOCAL_MUTATION_ROWS + 1
        );

        let too_many_paths = OperationTransaction {
            operations: (0..=MAX_LOCAL_MUTATION_REFERENCED_PATHS)
                .map(|index| SemanticOperation::EditPagePath {
                    page_id: PageId::from_uuid(Uuid::from_u128(720_000 + index as u128)),
                    path: ManagedPath::parse(&format!(
                        "content/nested pages/bounded-path-{index}.md"
                    ))
                    .unwrap(),
                })
                .collect(),
        };
        assert_eq!(
            bounded_local_mutation_size(&too_many_paths).referenced_paths,
            MAX_LOCAL_MUTATION_REFERENCED_PATHS + 1
        );
        assert!(matches!(
            validate_local_mutation_request(too_many_paths),
            Err(SyncLocalMutationRequestError::RequestTooLarge(_))
        ));
    }

    #[test]
    fn public_local_mutation_journey_creates_edits_renames_and_deletes() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-local-public-journey");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);
        let manifests_before = fixture.manifest_count();
        let page_id = PageId::from_uuid(Uuid::from_u128(720_000));
        let home_document_id = DocumentId::from_uuid(Uuid::from_u128(720_001));
        let block_id = BlockId::from_uuid(Uuid::from_u128(720_002));
        let old_path = ManagedPath::parse("content/nested pages/runtime-public-local.md").unwrap();
        let new_path =
            ManagedPath::parse("content/nested pages/runtime-public-renamed.md").unwrap();

        submit_durable(
            &handle,
            vec![
                SemanticOperation::CreatePage {
                    page_id,
                    home_document_id,
                    name: LogicalPageName::parse("Runtime Public Local").unwrap(),
                    path: old_path.clone(),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id,
                        home_document_id,
                    },
                    page_id,
                    parent: None,
                    order: "a".into(),
                    content: "created through public runtime".into(),
                },
            ],
        );
        assert_eq!(
            fs::read(fixture.graph_root().join(old_path.as_str())).unwrap(),
            b"- created through public runtime\n"
        );

        submit_durable(
            &handle,
            vec![SemanticOperation::EditBlockContent {
                block: BlockLocation {
                    block_id,
                    home_document_id,
                },
                content: "edited through public runtime".into(),
            }],
        );
        assert_eq!(
            fs::read(fixture.graph_root().join(old_path.as_str())).unwrap(),
            b"- edited through public runtime\n"
        );

        submit_durable(
            &handle,
            vec![SemanticOperation::RenamePagesAndRewriteReferrers {
                page_changes: vec![PageRename {
                    page_id,
                    new_name: LogicalPageName::parse("Runtime Public Renamed").unwrap(),
                    new_path: new_path.clone(),
                }],
                block_rewrites: Vec::new(),
                page_preamble_rewrites: Vec::new(),
            }],
        );
        assert!(!fixture.graph_root().join(old_path.as_str()).exists());
        assert_eq!(
            fs::read(fixture.graph_root().join(new_path.as_str())).unwrap(),
            b"- edited through public runtime\n"
        );

        submit_durable(
            &handle,
            vec![
                SemanticOperation::DeleteSubtree {
                    root_block_id: block_id,
                    page_id,
                },
                SemanticOperation::DeletePage { page_id },
            ],
        );
        assert!(!fixture.graph_root().join(new_path.as_str()).exists());
        assert_eq!(fixture.manifest_count(), manifests_before + 4);
        assert!(matches!(
            handle.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
    }

    #[test]
    fn published_local_failure_is_retained_and_retried_without_republication() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-local-published-retry");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);
        let page_id = PageId::from_uuid(Uuid::from_u128(730_000));
        let home_document_id = DocumentId::from_uuid(Uuid::from_u128(730_001));
        let block_id = BlockId::from_uuid(Uuid::from_u128(730_002));
        let path = ManagedPath::parse("content/nested pages/runtime-local-retry.md").unwrap();
        submit_durable(
            &handle,
            vec![
                SemanticOperation::CreatePage {
                    page_id,
                    home_document_id,
                    name: LogicalPageName::parse("Runtime Local Retry").unwrap(),
                    path: path.clone(),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id,
                        home_document_id,
                    },
                    page_id,
                    parent: None,
                    order: "a".into(),
                    content: "before retained retry".into(),
                },
            ],
        );
        let manifests_before = fixture.manifest_count();
        handle
            .install_repeated_operational_fault(OperationalFaultPoint::AfterManifest, 1)
            .unwrap();
        let outcome = handle
            .submit_local_mutation(
                OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id,
                        home_document_id,
                    },
                    content: "after retained retry".into(),
                }])
                .unwrap(),
            )
            .unwrap();
        assert!(matches!(
            outcome,
            SyncLocalMutationOutcome::RetryableRetainedRecovery {
                batch_id: Some(_),
                ..
            }
        ));
        assert_eq!(fixture.manifest_count(), manifests_before + 1);

        settle_local_mutation(&handle);
        assert_eq!(fixture.manifest_count(), manifests_before + 1);
        assert_eq!(
            fs::read(fixture.graph_root().join(path.as_str())).unwrap(),
            b"- after retained retry\n"
        );
        assert!(matches!(
            handle.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
    }

    #[test]
    fn concurrent_watcher_observation_and_local_submission_are_linearly_reconciled() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-local-watcher-order");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);
        let page_id = PageId::from_uuid(Uuid::from_u128(740_000));
        let home_document_id = DocumentId::from_uuid(Uuid::from_u128(740_001));
        let block_id = BlockId::from_uuid(Uuid::from_u128(740_002));
        let path =
            ManagedPath::parse("content/nested pages/runtime-local-watcher-order.md").unwrap();
        submit_durable(
            &handle,
            vec![
                SemanticOperation::CreatePage {
                    page_id,
                    home_document_id,
                    name: LogicalPageName::parse("Runtime Local Watcher Order").unwrap(),
                    path: path.clone(),
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id,
                        home_document_id,
                    },
                    page_id,
                    parent: None,
                    order: "a".into(),
                    content: "before concurrent observation".into(),
                },
            ],
        );
        fs::write(
            fixture.graph_root().join(path.as_str()),
            b"- concurrent external bytes\n",
        )
        .unwrap();

        let barrier = Arc::new(Barrier::new(3));
        let watcher_handle = handle.clone();
        let watcher_path = path.clone();
        let watcher_barrier = Arc::clone(&barrier);
        let watcher = thread::spawn(move || {
            watcher_barrier.wait();
            watcher_handle.observe_watcher(vec![SyncWatcherObservation::ManagedPath(watcher_path)])
        });
        let mutation_handle = handle.clone();
        let mutation_barrier = Arc::clone(&barrier);
        let mutation = thread::spawn(move || {
            mutation_barrier.wait();
            mutation_handle.submit_local_mutation(
                OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
                    block: BlockLocation {
                        block_id,
                        home_document_id,
                    },
                    content: "local mutation after ordered reconciliation".into(),
                }])
                .unwrap(),
            )
        });
        barrier.wait();
        watcher.join().unwrap().unwrap();
        assert!(matches!(
            mutation.join().unwrap().unwrap(),
            SyncLocalMutationOutcome::RetryableRetainedRecovery { .. }
        ));

        for _ in 0..128 {
            handle.tick().unwrap();
            if fs::read(fixture.graph_root().join(path.as_str())).unwrap()
                == b"- local mutation after ordered reconciliation\n"
            {
                break;
            }
        }
        assert_eq!(
            fs::read(fixture.graph_root().join(path.as_str())).unwrap(),
            b"- local mutation after ordered reconciliation\n"
        );
        assert!(matches!(
            handle.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
    }

    #[test]
    fn oversized_local_request_has_zero_actor_storage_graph_or_watcher_effects() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-local-request-bounds");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);
        let manifests_before = fixture.manifest_count();
        let sqlite_before = fixture.applied_batch_count();
        let graph_before = snapshot_graph_files(fixture.graph_root());
        let watcher_before = handle.status().unwrap().watcher;

        let error = handle
            .submit_local_mutation(OperationTransaction {
                operations: vec![SemanticOperation::SetPagePreamble {
                    page_id: PageId::from_uuid(Uuid::from_u128(750_000)),
                    preamble: Some("x".repeat(MAX_LOCAL_MUTATION_TEXT_BYTES + 1)),
                }],
            })
            .unwrap_err();
        assert_eq!(
            error,
            SyncLocalMutationRequestError::RequestTooLarge(SyncLocalMutationRequestSize {
                rows: 1,
                referenced_paths: 0,
                path_bytes: 0,
                text_bytes: MAX_LOCAL_MUTATION_TEXT_BYTES + 1,
            })
        );
        assert_eq!(fixture.manifest_count(), manifests_before);
        assert_eq!(fixture.applied_batch_count(), sqlite_before);
        assert_eq!(snapshot_graph_files(fixture.graph_root()), graph_before);
        assert_eq!(handle.status().unwrap().watcher, watcher_before);
        assert!(matches!(
            handle.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
    }

    #[test]
    fn clean_shutdown_refuses_until_retained_local_publication_resolves() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-local-shutdown-barrier");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);
        let page_id = PageId::from_uuid(Uuid::from_u128(760_000));
        let home_document_id = DocumentId::from_uuid(Uuid::from_u128(760_001));
        let block_id = BlockId::from_uuid(Uuid::from_u128(760_002));
        let path = ManagedPath::parse("content/nested pages/runtime-local-shutdown.md").unwrap();
        submit_durable(
            &handle,
            vec![
                SemanticOperation::CreatePage {
                    page_id,
                    home_document_id,
                    name: LogicalPageName::parse("Runtime Local Shutdown").unwrap(),
                    path,
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id,
                        home_document_id,
                    },
                    page_id,
                    parent: None,
                    order: "a".into(),
                    content: "before shutdown retry".into(),
                },
            ],
        );
        handle
            .install_repeated_operational_fault(OperationalFaultPoint::AfterStage, 2)
            .unwrap();
        assert!(matches!(
            handle
                .submit_local_mutation(
                    OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
                        block: BlockLocation {
                            block_id,
                            home_document_id,
                        },
                        content: "after shutdown retry".into(),
                    }])
                    .unwrap(),
                )
                .unwrap(),
            SyncLocalMutationOutcome::RetryableRetainedRecovery { .. }
        ));
        assert!(matches!(
            handle.clean_shutdown(),
            Err(SyncRuntimeRequestError::ActorRefused(_))
        ));
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Unsafe { .. }
        ));

        settle_local_mutation(&handle);
        assert!(matches!(
            handle.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
        assert_eq!(fixture.handoff(), EnrollmentDiscoveryHandoff::Safe);
        assert_eq!(
            handle.submit_local_mutation(OperationTransaction {
                operations: vec![SemanticOperation::DeletePage { page_id }],
            }),
            Err(SyncLocalMutationRequestError::ActorUnavailable)
        );
    }

    #[test]
    fn authority_revocation_keeps_published_local_continuation_terminal_and_unsafe() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-local-revoked-continuation");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);
        let page_id = PageId::from_uuid(Uuid::from_u128(770_000));
        let home_document_id = DocumentId::from_uuid(Uuid::from_u128(770_001));
        let block_id = BlockId::from_uuid(Uuid::from_u128(770_002));
        let path = ManagedPath::parse("content/nested pages/runtime-local-revoked.md").unwrap();
        submit_durable(
            &handle,
            vec![
                SemanticOperation::CreatePage {
                    page_id,
                    home_document_id,
                    name: LogicalPageName::parse("Runtime Local Revoked").unwrap(),
                    path,
                    kind: ManagedTextKind::Page,
                },
                SemanticOperation::CreateBlock {
                    block: BlockLocation {
                        block_id,
                        home_document_id,
                    },
                    page_id,
                    parent: None,
                    order: "a".into(),
                    content: "before revocation".into(),
                },
            ],
        );
        handle
            .install_repeated_operational_fault(OperationalFaultPoint::AfterManifest, 1)
            .unwrap();
        assert!(matches!(
            handle
                .submit_local_mutation(
                    OperationTransaction::new(vec![SemanticOperation::EditBlockContent {
                        block: BlockLocation {
                            block_id,
                            home_document_id,
                        },
                        content: "published before revocation".into(),
                    }])
                    .unwrap(),
                )
                .unwrap(),
            SyncLocalMutationOutcome::RetryableRetainedRecovery {
                batch_id: Some(_),
                ..
            }
        ));

        let lease_path = fixture.lease_path();
        let incoming = lease_path.with_extension("local-revoked.incoming");
        fs::write(&incoming, b"").unwrap();
        fs::rename(&incoming, &lease_path).unwrap();
        let revoked = handle.tick().unwrap();
        assert!(matches!(
            revoked,
            SyncRuntimeTick::LocalMutation(SyncLocalMutationOutcome::Revoked {
                batch_id: Some(_),
                ..
            })
        ));
        assert!(matches!(
            handle
                .submit_local_mutation(
                    OperationTransaction::new(vec![SemanticOperation::DeletePage { page_id }])
                        .unwrap()
                )
                .unwrap(),
            SyncLocalMutationOutcome::Revoked {
                batch_id: Some(_),
                ..
            }
        ));
        assert!(matches!(
            handle.clean_shutdown(),
            Err(SyncRuntimeRequestError::ActorRefused(_))
        ));
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Unsafe { .. }
        ));
    }

    fn admit_external_page(
        handle: &SyncRuntimeHandle,
        fixture: &RuntimeHostFixture,
        path: &str,
        body: &[u8],
    ) {
        let file = fixture.graph_root().join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, body).unwrap();
        handle
            .observe_watcher(vec![SyncWatcherObservation::managed_path(path).unwrap()])
            .unwrap();
        settle_exact_feed(handle)
            .unwrap_or_else(|state| panic!("external page did not settle: {state:?}"));
    }

    #[test]
    fn public_queries_are_bounded_serialized_and_read_the_exact_materialized_frontier() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-public-query");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);
        let path = "content/nested pages/Résumé 日本語.md";
        admit_external_page(
            &handle,
            &fixture,
            path,
            "title:: Résumé 日本語\ntags:: alpha\n\n- TODO Needle #alpha\n  custom:: value\n  - child [[Résumé 日本語]]\n".as_bytes(),
        );

        let pages = handle
            .query(SyncRuntimeQueryRequest::ListPages {
                page_kind: Some(SyncPageKind::Page),
                limit: 16,
            })
            .unwrap();
        let SyncRuntimeQueryReply::Pages(pages) = pages else {
            panic!("page list returned the wrong reply variant");
        };
        let page = pages
            .into_iter()
            .find(|page| page.path == path)
            .expect("nested Unicode page must be materialized from SQLite");

        let resolved = handle
            .query(SyncRuntimeQueryRequest::ResolvePage {
                path: path.into(),
                name: page.name.clone(),
                page_kind: page.kind,
            })
            .unwrap();
        assert_eq!(resolved, SyncRuntimeQueryReply::Page(Some(page.clone())));

        let loaded = handle
            .query(SyncRuntimeQueryRequest::LoadPage {
                page_id: page.page_id.clone(),
                block_limit: 16,
            })
            .unwrap();
        let SyncRuntimeQueryReply::PageWithBlocks(Some(loaded)) = loaded else {
            panic!("page load must return the exact page and its blocks");
        };
        assert_eq!(loaded.page, page);
        assert!(loaded.blocks.len() >= 2);
        assert!(loaded
            .blocks
            .iter()
            .any(|block| block.parent_block_id.is_some()));
        assert!(loaded.blocks.iter().all(|block| !block.block_id.is_empty()));
        assert!(loaded
            .blocks
            .iter()
            .all(|block| block.logseq_uuid.is_none()));

        let search = handle
            .query(SyncRuntimeQueryRequest::Search {
                query: "Needle".into(),
                limit: 8,
            })
            .unwrap();
        assert!(matches!(search, SyncRuntimeQueryReply::Search(hits) if !hits.is_empty()));
        let properties = handle
            .query(SyncRuntimeQueryRequest::PropertiesNamed {
                name: "custom".into(),
                value: Some("value".into()),
                limit: 8,
            })
            .unwrap();
        assert!(matches!(properties, SyncRuntimeQueryReply::Properties(rows) if !rows.is_empty()));
        let tags = handle
            .query(SyncRuntimeQueryRequest::Tags {
                tag: "alpha".into(),
                limit: 8,
            })
            .unwrap();
        assert!(matches!(tags, SyncRuntimeQueryReply::Tags(rows) if !rows.is_empty()));
        let tasks = handle
            .query(SyncRuntimeQueryRequest::Tasks {
                marker: Some("TODO".into()),
                limit: 8,
            })
            .unwrap();
        assert!(matches!(tasks, SyncRuntimeQueryReply::Tasks(rows) if !rows.is_empty()));
        let references = handle
            .query(SyncRuntimeQueryRequest::ReferencesToPageName {
                name: page.name,
                limit: 8,
            })
            .unwrap();
        assert!(matches!(references, SyncRuntimeQueryReply::References(rows) if !rows.is_empty()));

        let encoded = serde_json::to_string(&SyncRuntimeQueryRequest::ListPages {
            page_kind: Some(SyncPageKind::Page),
            limit: 2,
        })
        .unwrap();
        assert!(encoded.contains("list_pages"));
    }

    #[test]
    fn query_rejects_over_limit_before_actor_queue_or_filesystem_work() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-query-bounds");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        let before = handle.status().unwrap();
        assert!(matches!(
            handle.query(SyncRuntimeQueryRequest::ListPages {
                page_kind: None,
                limit: MAX_SYNC_RUNTIME_QUERY_ROWS + 1,
            }),
            Err(SyncRuntimeRequestError::QueryTooLarge { .. })
        ));
        assert_eq!(handle.status().unwrap(), before);

        let source = include_str!("sync_runtime.rs");
        let production = source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("the runtime source has its test boundary");
        assert!(!production.contains("read_dir"));
        assert!(!production.contains("read_to_string"));
    }

    /// Drive the actor until the exact feed settles the epoch it owes, or give
    /// up. `Err` carries the last observed turn for a useful failure capsule.
    fn settle_exact_feed(handle: &SyncRuntimeHandle) -> Result<SyncRuntimeTick, SyncRuntimeTick> {
        let mut last = SyncRuntimeTick::Idle;
        for _ in 0..64 {
            last = handle.tick().unwrap();
            match last {
                SyncRuntimeTick::Idle
                | SyncRuntimeTick::AdmittedNoop { .. }
                | SyncRuntimeTick::AdmittedComplete { .. } => {
                    if !handle.status().unwrap().watcher.pending {
                        return Ok(last);
                    }
                }
                SyncRuntimeTick::Recovering
                | SyncRuntimeTick::RetryFull
                | SyncRuntimeTick::Failed(_) => {}
                terminal => return Err(terminal),
            }
        }
        Err(last)
    }

    /// A deterministic post-publication failure must not turn an ordinary
    /// edit into an absorbing `Recovering` loop. The published batch remains
    /// truth, its exact continuation and failure stay retained, later watcher
    /// work queues behind it, and shutdown returns that refusal immediately.
    #[test]
    fn deterministic_published_failure_blocks_without_losing_published_work() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-external-edit");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);

        let path = "content/nested pages/Alpha.md";
        let file = fixture.graph_root().join(path);
        let observe = || {
            handle
                .observe_watcher(vec![SyncWatcherObservation::managed_path(path).unwrap()])
                .unwrap();
        };

        fs::write(&file, b"- line 0 rev 0\n").unwrap();
        observe();
        let created = settle_exact_feed(&handle)
            .unwrap_or_else(|stuck| panic!("external page creation did not settle: {stuck:?}"));
        assert!(
            matches!(
                created,
                SyncRuntimeTick::AdmittedNoop { .. } | SyncRuntimeTick::AdmittedComplete { .. }
            ),
            "external page creation must be admitted: {created:?}"
        );

        let acknowledged_before_edit = handle.status().unwrap().watcher.acknowledged;
        let manifests_before_edit = fixture.manifest_count();
        handle
            .install_repeated_operational_fault(
                OperationalFaultPoint::AfterStage,
                crate::oplog::reconciliation_session::MAX_PUBLISHED_CONTINUATION_RETRIES + 1,
            )
            .unwrap();
        fs::write(&file, b"- line 0 rev 1\n- line 1 rev 1\n").unwrap();
        observe();
        let blocked = settle_exact_feed(&handle)
            .expect_err("the deterministic continuation failure must block");
        let SyncRuntimeTick::Blocked(detail) = blocked else {
            panic!("the unretriable continuation must become explicitly blocked: {blocked:?}");
        };
        assert!(
            detail.contains("remained failed after 3 retries")
                && detail.contains(
                    "exact retained failure: ArchiveStage: deterministic operational fault"
                ),
            "the stable refusal must preserve the exact retry count, phase, and failure: {detail}"
        );
        assert_eq!(
            fixture.manifest_count(),
            manifests_before_edit + 1,
            "the already-published authoritative batch must remain in the immutable archive"
        );

        let blocked_status = handle.status().unwrap();
        assert!(blocked_status.watcher.drain_in_flight);
        assert_eq!(
            blocked_status.watcher.acknowledged, acknowledged_before_edit,
            "the failed external-edit epoch must not be acknowledged"
        );

        fs::write(
            &file,
            b"- later watcher work stays queued behind the published block\n",
        )
        .unwrap();
        observe();
        let queued_status = handle.status().unwrap();
        assert!(queued_status.watcher.latest_enqueue > blocked_status.watcher.latest_enqueue);
        assert_eq!(
            queued_status.watcher.acknowledged,
            blocked_status.watcher.acknowledged
        );
        assert!(queued_status.watcher.drain_in_flight);
        assert!(queued_status.watcher.pending);
        assert_eq!(
            handle.tick().unwrap(),
            SyncRuntimeTick::Blocked(detail.clone()),
            "a blocked poll must be stable and must not retry or replace its evidence"
        );

        let shutdown = handle.clean_shutdown();
        assert_eq!(
            shutdown,
            Err(SyncRuntimeRequestError::ActorRefused(detail)),
            "clean shutdown must return the specific retained refusal on its first blocked turn"
        );
        assert!(
            matches!(fixture.handoff(), EnrollmentDiscoveryHandoff::Unsafe { .. }),
            "a blocked drain must not falsely publish a clean Safe handoff"
        );
    }

    /// An ordinary external editor must be able to create a page and then
    /// repeatedly change existing blocks while appending new blocks.
    #[test]
    fn ordinary_external_edit_settles_its_epoch_and_still_reaches_a_safe_handoff() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-external-edit-success");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);

        let path = "content/nested pages/Alpha.md";
        let file = fixture.graph_root().join(path);
        let observe = || {
            handle
                .observe_watcher(vec![SyncWatcherObservation::managed_path(path).unwrap()])
                .unwrap();
        };

        fs::write(&file, b"- line 0 rev 0\n").unwrap();
        observe();
        let created = settle_exact_feed(&handle)
            .unwrap_or_else(|stuck| panic!("external page creation did not settle: {stuck:?}"));
        assert!(
            matches!(
                created,
                SyncRuntimeTick::AdmittedNoop { .. } | SyncRuntimeTick::AdmittedComplete { .. }
            ),
            "external page creation must be admitted: {created:?}"
        );

        for revision in 1..=3 {
            let body = (0..=revision)
                .map(|line| format!("- line {line} rev {revision}\n"))
                .collect::<String>();
            fs::write(&file, body.as_bytes()).unwrap();
            observe();
            let edited = settle_exact_feed(&handle).unwrap_or_else(|stuck| {
                panic!(
                    "external save {revision}, which retypes the existing bullets and appends \
                     one more, never settles its watcher epoch; the actor keeps returning \
                     {stuck:?} with status {:?}",
                    handle.status().unwrap()
                )
            });
            assert!(
                matches!(
                    edited,
                    SyncRuntimeTick::AdmittedNoop { .. } | SyncRuntimeTick::AdmittedComplete { .. }
                ),
                "external save {revision} must be admitted: {edited:?}"
            );
        }

        let outcome = handle.clean_shutdown().unwrap_or_else(|error| {
            panic!("clean shutdown after an ordinary external edit was refused: {error}")
        });
        assert!(
            matches!(outcome, SyncShutdownOutcome::Safe(_)),
            "clean shutdown after an ordinary external edit must publish Safe: {outcome:?}"
        );
        assert!(
            matches!(fixture.handoff(), EnrollmentDiscoveryHandoff::Safe),
            "a refused drain must not strand the durable handoff Unsafe"
        );
    }

    /// An external delete is authorized from the deleted page's own completed
    /// projection, not from an unrelated page's most recent global frontier.
    ///
    /// This deliberately drives only the public runtime handle.  The two
    /// pages first acquire independent durable completions, then an external
    /// rename advances the accepted frontier for `advanced`, and an
    /// ordinary external delete of `deleted` must still reconcile and permit a
    /// clean Safe handoff.
    #[test]
    fn external_delete_after_unrelated_accepted_rename_settles_and_reaches_safe() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-delete-frontier");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);

        let advanced = "content/nested pages/advanced.md";
        let advanced_renamed = "content/nested pages/advanced renamed.md";
        let deleted = "content/nested pages/deleted.md";
        fs::write(fixture.graph_root().join(advanced), b"- advance 0\n").unwrap();
        fs::write(fixture.graph_root().join(deleted), b"- delete me\n").unwrap();
        handle
            .observe_watcher(vec![
                SyncWatcherObservation::managed_path(advanced).unwrap(),
                SyncWatcherObservation::managed_path(deleted).unwrap(),
            ])
            .unwrap();
        let created = settle_exact_feed(&handle)
            .unwrap_or_else(|stuck| panic!("initial external pages did not settle: {stuck:?}"));
        assert!(
            matches!(
                created,
                SyncRuntimeTick::AdmittedNoop { .. } | SyncRuntimeTick::AdmittedComplete { .. }
            ),
            "initial external pages must be admitted: {created:?}"
        );

        fs::rename(
            fixture.graph_root().join(advanced),
            fixture.graph_root().join(advanced_renamed),
        )
        .unwrap();
        handle
            .observe_watcher(vec![
                SyncWatcherObservation::managed_path(advanced).unwrap(),
                SyncWatcherObservation::managed_path(advanced_renamed).unwrap(),
            ])
            .unwrap();
        let renamed_tick = settle_exact_feed(&handle).unwrap_or_else(|stuck| {
            panic!("the unrelated accepted rename did not settle: {stuck:?}")
        });
        assert!(
            matches!(
                renamed_tick,
                SyncRuntimeTick::AdmittedNoop { .. } | SyncRuntimeTick::AdmittedComplete { .. }
            ),
            "the unrelated external rename must be admitted: {renamed_tick:?}"
        );

        fs::remove_file(fixture.graph_root().join(deleted)).unwrap();
        handle
            .observe_watcher(vec![SyncWatcherObservation::managed_path(deleted).unwrap()])
            .unwrap();
        let deleted_tick = settle_exact_feed(&handle).unwrap_or_else(|stuck| {
            panic!(
                "an ordinary delete after an unrelated accepted frontier advance must settle, \
                 but the runtime returned {stuck:?} with status {:?}",
                handle.status().unwrap()
            )
        });
        assert!(
            matches!(
                deleted_tick,
                SyncRuntimeTick::AdmittedNoop { .. } | SyncRuntimeTick::AdmittedComplete { .. }
            ),
            "the external delete must be admitted: {deleted_tick:?}"
        );
        assert!(
            matches!(handle.clean_shutdown(), Ok(SyncShutdownOutcome::Safe(_))),
            "a successfully reconciled external delete must allow Safe handoff"
        );
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Safe
        ));
    }

    #[test]
    fn existing_safe_opens_one_owner_and_duplicate_or_foreign_binding_gets_no_authority() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-safe-owner");
        let request = fixture.request();
        let handle = active_handle(SyncRuntimeHandle::open(request.clone()));
        assert_eq!(
            handle.status().unwrap().recovery,
            Some(SyncRuntimeRecovery::AdoptedSafeHandoff)
        );

        let duplicate = SyncRuntimeHandle::open(request.clone());
        assert!(matches!(
            duplicate.status,
            SyncRuntimeOpenStatus::OpenRefused { .. }
        ));
        assert!(duplicate.handle.is_none());

        let foreign_root = fixture.graph_root().join("foreign graph");
        fs::create_dir(&foreign_root).unwrap();
        let mut foreign = request;
        foreign.graph_root = foreign_root;
        let foreign = SyncRuntimeHandle::open(foreign);
        assert!(matches!(
            foreign.status,
            SyncRuntimeOpenStatus::AmbiguousOrForeignResidue(_)
        ));
        assert!(foreign.handle.is_none());

        assert!(matches!(
            handle.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
    }

    #[test]
    fn unsafe_takeover_cannot_run_with_old_owner_and_recovers_before_safe() {
        let mut fixture = RuntimeHostFixture::unsafe_held("sync-runtime-unsafe-owner");
        let request = fixture.request();
        let refused = SyncRuntimeHandle::open(request.clone());
        assert!(matches!(
            refused.status,
            SyncRuntimeOpenStatus::OpenRefused { .. }
        ));
        assert!(refused.handle.is_none());

        fixture.release_held_owner();
        let handle = active_handle(SyncRuntimeHandle::open(request));
        assert_eq!(
            handle.status().unwrap().recovery,
            Some(SyncRuntimeRecovery::TookOverCrashedUnsafe)
        );
        let ticks = drain_until_settled(&handle);
        assert!(
            admitted_an_epoch(&ticks),
            "crash takeover must settle its startup full scan before Safe: {ticks:?}"
        );
        assert!(matches!(
            handle.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Safe
        ));
    }

    #[test]
    fn exact_observation_uses_sole_queue_and_clean_shutdown_settles_safe_once_and_joins() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-clean");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        let clone = handle.clone();
        drive_initial_feed(&handle);

        let path = "content/nested pages/deep/Café note.md";
        fs::write(
            fixture.graph_root().join(path),
            b"- actor observed unicode and spaces\r\n",
        )
        .unwrap();
        let manifests_before = fixture.manifest_count();
        handle
            .observe_watcher(vec![SyncWatcherObservation::managed_path(path).unwrap()])
            .unwrap();
        let pending = handle.status().unwrap();
        assert!(pending.watcher.pending);
        assert!(pending.watcher.latest_enqueue > pending.watcher.acknowledged);

        let finished_before = ACTOR_THREADS_FINISHED.load(std::sync::atomic::Ordering::SeqCst);
        let outcome = clone.clean_shutdown().unwrap();
        let SyncShutdownOutcome::Safe(snapshot) = outcome else {
            panic!("clean shutdown did not publish Safe");
        };
        assert_eq!(snapshot.lifecycle, SyncRuntimeLifecycle::StoppedSafe);
        assert!(!snapshot.watcher.pending);
        assert_eq!(fixture.manifest_count(), manifests_before + 1);
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Safe
        ));
        assert!(
            ACTOR_THREADS_FINISHED.load(std::sync::atomic::Ordering::SeqCst) >= finished_before + 1
        );

        assert!(matches!(
            handle.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
    }

    #[test]
    fn safe_reopen_honestly_reports_its_deferred_full_scan_catch_up() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-unchanged-safe-reopen");
        let request = fixture.request();
        let first = active_handle(SyncRuntimeHandle::open(request.clone()));
        drive_initial_feed(&first);
        assert!(matches!(
            first.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));

        let manifests_before_reopen = fixture.manifest_count();
        let reopened = active_handle(SyncRuntimeHandle::open(request));
        let status = reopened.status().unwrap();
        assert_eq!(
            status.recovery,
            Some(SyncRuntimeRecovery::AdoptedSafeHandoff)
        );
        assert!(
            status.watcher.pending && status.watcher.pending_requires_full_scan,
            "a Safe handoff proves a prior clean Tine stop, not that Logseq or Syncthing \
             made no closed-interval edit: {:?}",
            status.watcher
        );
        assert_eq!(fixture.manifest_count(), manifests_before_reopen);
        drive_initial_feed(&reopened);
        assert!(
            !reopened.status().unwrap().watcher.pending,
            "the first actor drain must settle the exact owed catch-up before Safe"
        );
        assert!(matches!(
            reopened.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
    }

    #[test]
    fn closed_interval_edit_is_discovered_by_safe_reopen_before_next_safe() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-closed-interval-edit");
        let request = fixture.request();
        let first = active_handle(SyncRuntimeHandle::open(request.clone()));
        drive_initial_feed(&first);
        assert!(matches!(
            first.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));

        let path = "content/nested pages/changed while closed.md";
        fs::write(
            fixture.graph_root().join(path),
            b"- external Logseq or Syncthing edit while Tine was closed\n",
        )
        .unwrap();
        let manifests_before_reopen = fixture.manifest_count();

        let reopened = active_handle(SyncRuntimeHandle::open(request));
        assert!(
            reopened.status().unwrap().watcher.pending_requires_full_scan,
            "the closed interval is intentionally uncertain even after an authenticated Safe handoff"
        );
        drive_initial_feed(&reopened);
        assert_eq!(
            fixture.manifest_count(),
            manifests_before_reopen + 1,
            "the deferred full scan must admit the closed-interval external edit before Safe"
        );
        assert!(matches!(
            reopened.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
    }

    #[test]
    fn dropping_without_shutdown_leaves_unsafe_and_fresh_open_must_take_over() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-crash-drop");
        let request = fixture.request();
        let handle = active_handle(SyncRuntimeHandle::open(request.clone()));
        drop(handle);
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Unsafe { .. }
        ));

        let reopened = active_handle(SyncRuntimeHandle::open(request));
        assert_eq!(
            reopened.status().unwrap().recovery,
            Some(SyncRuntimeRecovery::TookOverCrashedUnsafe)
        );
        let ticks = drain_until_settled(&reopened);
        assert!(
            admitted_an_epoch(&ticks),
            "crash takeover must reconcile its forced full scan: {ticks:?}"
        );
        assert!(matches!(
            reopened.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Safe
        ));
    }

    #[test]
    fn unsafe_crash_takeover_reconciles_closed_interval_edit_and_reaches_safe() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-crash-recovery-liveness");
        let request = fixture.request();
        let handle = active_handle(SyncRuntimeHandle::open(request.clone()));
        drive_initial_feed(&handle);
        drop(handle);
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Unsafe { .. }
        ));

        let path = "content/nested pages/changed after crash.md";
        let bytes = b"- external editor changed this while Tine was down\n";
        fs::write(fixture.graph_root().join(path), bytes).unwrap();
        let manifests_before_reopen = fixture.manifest_count();

        let reopened = active_handle(SyncRuntimeHandle::open(request.clone()));
        assert_eq!(
            reopened.status().unwrap().recovery,
            Some(SyncRuntimeRecovery::TookOverCrashedUnsafe)
        );
        let ticks = drain_until_settled(&reopened);
        assert!(
            admitted_an_epoch(&ticks),
            "crash takeover must drive its authenticated full reconciliation before Safe: {ticks:?}"
        );
        assert_eq!(
            fs::read(fixture.graph_root().join(path)).unwrap(),
            bytes,
            "recovery must not overwrite the unimported projection bytes"
        );
        assert_eq!(
            fixture.manifest_count(),
            manifests_before_reopen + 1,
            "the closed-interval external edit must be admitted before Safe"
        );
        assert!(matches!(
            reopened.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Safe
        ));

        let safe_reopen = active_handle(SyncRuntimeHandle::open(request));
        assert_eq!(
            safe_reopen.status().unwrap().recovery,
            Some(SyncRuntimeRecovery::AdoptedSafeHandoff)
        );
        drive_initial_feed(&safe_reopen);
        assert!(matches!(
            safe_reopen.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
    }

    /// A realistic crashed session has advanced the accepted frontier, so its
    /// disposable projection may need rebuilding during authenticated takeover.
    #[test]
    fn crash_after_an_accepted_import_can_still_take_over_its_own_projection() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-crash-after-import");
        let request = fixture.request();
        let page = "content/nested pages/alpha.md";
        let bytes = b"- imported before the crash\n";

        let handle = active_handle(SyncRuntimeHandle::open(request.clone()));
        drive_initial_feed(&handle);
        let manifests_before = fixture.manifest_count();
        fs::write(fixture.graph_root().join(page), bytes).unwrap();
        handle
            .observe_watcher(vec![SyncWatcherObservation::managed_path(page).unwrap()])
            .unwrap();
        drive_initial_feed(&handle);
        assert_eq!(
            fixture.manifest_count(),
            manifests_before + 1,
            "the external edit must be accepted into the oplog before the crash"
        );

        drop(handle);
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Unsafe { .. }
        ));
        assert_eq!(
            fs::read(fixture.graph_root().join(page)).unwrap(),
            bytes,
            "the external file is untouched by the crash"
        );

        let reopened = SyncRuntimeHandle::open(request);
        assert_eq!(
            reopened.status,
            SyncRuntimeOpenStatus::Active,
            "a crash after an accepted import must still reach the crash-takeover \
             path; the oplog is intact and SQLite is a disposable materialization"
        );
        let reopened = reopened.handle.expect("takeover must return a handle");
        assert_eq!(
            reopened.status().unwrap().recovery,
            Some(SyncRuntimeRecovery::TookOverCrashedUnsafe)
        );
        let manifests_after_reopen = fixture.manifest_count();
        drive_initial_feed(&reopened);
        assert_eq!(
            fixture.manifest_count(),
            manifests_after_reopen,
            "the accepted pre-crash import must not be duplicated during recovery catch-up"
        );
        assert!(matches!(
            reopened.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
    }

    #[test]
    fn revocation_latches_terminal_drops_authority_and_refuses_later_intake() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-revocation");
        let request = fixture.request();
        let handle = active_handle(SyncRuntimeHandle::open(request.clone()));
        drive_initial_feed(&handle);

        let lease_path = fixture.lease_path();
        let incoming = lease_path.with_extension("lock.incoming");
        fs::write(&incoming, b"").unwrap();
        fs::rename(&incoming, &lease_path).unwrap();
        handle
            .observe_watcher(vec![SyncWatcherObservation::RescanRequired])
            .unwrap();
        assert!(matches!(
            handle.tick().unwrap(),
            SyncRuntimeTick::Terminal(_)
        ));
        assert_eq!(
            handle.status().unwrap().lifecycle,
            SyncRuntimeLifecycle::Terminal
        );
        assert!(matches!(
            handle.observe_watcher(vec![SyncWatcherObservation::RescanRequired]),
            Err(SyncRuntimeRequestError::ActorRefused(_))
        ));
        assert!(matches!(
            handle.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Terminal(_)
        ));

        let fresh = active_handle(SyncRuntimeHandle::open(request));
        assert_eq!(
            fresh.status().unwrap().recovery,
            Some(SyncRuntimeRecovery::TookOverCrashedUnsafe)
        );
        drop(fresh);
    }

    #[test]
    fn missing_existing_projection_is_refused_without_rebuild() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-existing-only");
        let mut request = fixture.request();
        request.database_path = fixture.graph_root().join("missing.sqlite");
        let opened = SyncRuntimeHandle::open(request.clone());
        assert!(matches!(
            opened.status,
            SyncRuntimeOpenStatus::OpenRefused { .. }
        ));
        assert!(opened.handle.is_none());
        assert!(!request.database_path.exists());
    }

    #[test]
    fn interrupted_forensics_is_refused_without_moving_the_existing_projection() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-forensics-existing-only");
        let request = fixture.request();
        let database_bytes = fs::read(&request.database_path).unwrap();
        let file_name = request.database_path.file_name().unwrap().to_str().unwrap();
        let forensic = request
            .database_path
            .parent()
            .unwrap()
            .join(format!("{file_name}.forensic-interrupted"));
        fs::create_dir(&forensic).unwrap();

        let opened = SyncRuntimeHandle::open(request.clone());
        assert!(matches!(
            opened.status,
            SyncRuntimeOpenStatus::OpenRefused { .. }
        ));
        assert!(opened.handle.is_none());
        assert_eq!(fs::read(&request.database_path).unwrap(), database_bytes);
        assert!(!forensic.join("database").exists());
        assert!(!forensic.join("EVIDENCE_COMPLETE").exists());
    }

    /// Drive the feed until the watcher queue settles, reporting every tick so
    /// an unsettled or blocked drain is visible in the failure message.
    fn drain_until_settled(handle: &SyncRuntimeHandle) -> Vec<SyncRuntimeTick> {
        let mut ticks = Vec::new();
        for _ in 0..32 {
            let tick = handle.tick().unwrap();
            let settled = matches!(
                tick,
                SyncRuntimeTick::Idle
                    | SyncRuntimeTick::AdmittedNoop { .. }
                    | SyncRuntimeTick::AdmittedComplete { .. }
            );
            ticks.push(tick);
            let watcher = handle.status().unwrap().watcher;
            if settled && !watcher.pending && !watcher.drain_in_flight {
                break;
            }
        }
        ticks
    }

    fn admitted_an_epoch(ticks: &[SyncRuntimeTick]) -> bool {
        ticks.iter().any(|tick| {
            matches!(
                tick,
                SyncRuntimeTick::AdmittedComplete { .. } | SyncRuntimeTick::AdmittedNoop { .. }
            )
        })
    }

    /// One preserved sync-provider conflict copy must not halt startup
    /// reconciliation for the whole graph.
    ///
    /// This is the documented handoff: every device stops, the provider settles,
    /// conflict copies are preserved, and Tine reopens. A conflict copy is
    /// explicitly not a page, so it says nothing about any other page. The
    /// user's ordinary external edit to an unrelated page must still be
    /// imported, and the session must still be able to publish `HandoffSafe`.
    #[test]
    fn preserved_provider_conflict_copy_does_not_block_startup_reconciliation() {
        let edited = "content/nested pages/rename old.org";
        let edit = b"* edited in another editor while Tine was closed\n";
        let conflict =
            "content/nested pages/deep/Café note.sync-conflict-20260728-120000-ABCDEF.md";

        let control = RuntimeHostFixture::safe("sync-runtime-conflict-control");
        fs::write(control.graph_root().join(edited), edit).unwrap();
        let control_manifests = control.manifest_count();
        let control_handle = active_handle(SyncRuntimeHandle::open(control.request()));
        let control_ticks = drain_until_settled(&control_handle);
        assert!(
            admitted_an_epoch(&control_ticks),
            "control: startup reconciliation must settle the offline edit: {control_ticks:?}"
        );
        assert_eq!(control.manifest_count(), control_manifests + 1);
        assert!(matches!(
            control_handle.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));

        let fixture = RuntimeHostFixture::safe("sync-runtime-conflict-copy");
        fs::write(fixture.graph_root().join(conflict), b"- peer device copy\n").unwrap();
        fs::write(fixture.graph_root().join(edited), edit).unwrap();
        let manifests_before = fixture.manifest_count();
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));

        let ticks = drain_until_settled(&handle);
        assert!(
            admitted_an_epoch(&ticks),
            "startup reconciliation must still import an unrelated page's ordinary \
             external edit while a preserved provider conflict copy sits in the \
             graph, but the drain produced {ticks:?}"
        );
        assert_eq!(
            fixture.manifest_count(),
            manifests_before + 1,
            "the unrelated external edit produced no durable batch"
        );
        let shutdown = handle.clean_shutdown();
        assert!(
            matches!(shutdown, Ok(SyncShutdownOutcome::Safe(_))),
            "a preserved provider conflict copy must not make clean Safe handoff \
             unreachable, but shutdown returned {shutdown:?}"
        );
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Safe
        ));
        assert_eq!(
            fs::read(fixture.graph_root().join(conflict)).unwrap(),
            b"- peer device copy\n",
            "the conflict copy itself must be preserved byte for byte"
        );
    }

    /// Every graph-relative regular file under `root`, sorted, for proving that
    /// a configured root gained nothing while nonstandard paths were imported.
    fn relative_files(root: &Path) -> Vec<String> {
        fn walk(base: &Path, current: &Path, out: &mut Vec<String>) {
            let Ok(entries) = fs::read_dir(current) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(base, &path, out);
                } else {
                    out.push(
                        path.strip_prefix(base)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    /// An OG graph whose page files live outside the configured `pages/` and
    /// `journals/` roots stays a live two-way Markdown bridge.
    ///
    /// OG discovers graph text by walking the whole graph directory
    /// (`logseq.common.graph/get-files`), takes the page title from the last
    /// path component only (`graph-parser.extract/get-page-name`), decides
    /// journal-ness by parsing that title as a date
    /// (`graph-parser.block/convert-page-if-journal`), and rewrites an existing
    /// page at its exact recorded `:file/path`
    /// (`frontend.modules.file.core/save-tree-aux!`). External create, edit,
    /// rename and delete at such paths must therefore reconcile without
    /// blocking the epoch and without moving anything into a configured root.
    #[test]
    fn external_changes_outside_configured_roots_reconcile_without_flattening() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-nonstandard-layout");
        let graph_root = fixture.graph_root().to_path_buf();
        // Neither directory is under the fixture's configured
        // `content/nested pages` / `diary/日記` roots.
        let nested = graph_root.join("archive/2026/client notes");
        fs::create_dir_all(&nested).unwrap();
        let page = "archive/2026/client notes/Ünicode outside.md";
        let journal = "archive/2026/client notes/2026_07_04.md";
        let renamed = "archive/2026/client notes/Ünicode renamed.md";
        let configured_roots = [
            graph_root.join("content/nested pages"),
            graph_root.join("diary/日記"),
        ];
        let configured_before = configured_roots
            .iter()
            .map(|root| relative_files(root))
            .collect::<Vec<_>>();

        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);

        // External create: an ordinary page plus a date-titled file that OG
        // classifies as a journal even though it is nowhere near `journals/`.
        let created = "- created by another editor\n".as_bytes();
        let day = "- july fourth\n".as_bytes();
        fs::write(graph_root.join(page), created).unwrap();
        fs::write(graph_root.join(journal), day).unwrap();
        let before = fixture.manifest_count();
        handle
            .observe_watcher(vec![
                SyncWatcherObservation::managed_path(page).unwrap(),
                SyncWatcherObservation::managed_path(journal).unwrap(),
            ])
            .unwrap();
        let ticks = drain_until_settled(&handle);
        assert!(
            admitted_an_epoch(&ticks),
            "an external create outside the configured roots must reconcile, \
             but the drain produced {ticks:?}"
        );
        assert!(
            fixture.manifest_count() > before,
            "the external create produced no durable batch"
        );
        assert_eq!(fs::read(graph_root.join(page)).unwrap(), created);
        assert_eq!(fs::read(graph_root.join(journal)).unwrap(), day);

        // External edit of the nested page.
        let edited = "- edited by another editor\n".as_bytes();
        fs::write(graph_root.join(page), edited).unwrap();
        let before = fixture.manifest_count();
        handle
            .observe_watcher(vec![SyncWatcherObservation::managed_path(page).unwrap()])
            .unwrap();
        let ticks = drain_until_settled(&handle);
        assert!(
            admitted_an_epoch(&ticks),
            "an external edit outside the configured roots must reconcile, \
             but the drain produced {ticks:?}"
        );
        assert!(fixture.manifest_count() > before);
        assert_eq!(fs::read(graph_root.join(page)).unwrap(), edited);

        // External rename inside the same nonstandard directory, exactly as OG
        // renames a page (`compute-new-file-path` keeps the parent components).
        fs::rename(graph_root.join(page), graph_root.join(renamed)).unwrap();
        let before = fixture.manifest_count();
        handle
            .observe_watcher(vec![
                SyncWatcherObservation::managed_path(page).unwrap(),
                SyncWatcherObservation::managed_path(renamed).unwrap(),
            ])
            .unwrap();
        let ticks = drain_until_settled(&handle);
        assert!(
            admitted_an_epoch(&ticks),
            "an external rename outside the configured roots must reconcile, \
             but the drain produced {ticks:?}"
        );
        assert!(fixture.manifest_count() > before);
        assert!(!graph_root.join(page).exists());
        assert_eq!(fs::read(graph_root.join(renamed)).unwrap(), edited);

        let shutdown = handle.clean_shutdown();
        assert!(
            matches!(shutdown, Ok(SyncShutdownOutcome::Safe(_))),
            "nonstandard graph text must not make clean Safe handoff unreachable, \
             but shutdown returned {shutdown:?}"
        );
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Safe
        ));

        // Nothing was flattened or copied into a configured root, and both
        // surviving files still sit at their exact nested spelling.
        for (root, expected) in configured_roots.iter().zip(&configured_before) {
            assert_eq!(
                &relative_files(root),
                expected,
                "a configured root changed while only nonstandard paths were edited"
            );
        }
        assert_eq!(
            relative_files(&nested),
            vec!["2026_07_04.md".to_owned(), "Ünicode renamed.md".to_owned()]
        );
    }

    /// The other destructive half of the same bridge: an external delete at a
    /// nonstandard path.
    ///
    /// This runs on its own runtime because an absence import whose affected
    /// frontier was already advanced by another page's rename or deletion
    /// blocks on `ConflictingLocalTail`. That wedge is layout-independent — the
    /// identical sequence inside `content/nested pages` / `diary/日記` blocks
    /// the same way — so it is deliberately not entangled with this contract.
    #[test]
    fn external_deletes_outside_configured_roots_reconcile_without_flattening() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-nonstandard-delete");
        let graph_root = fixture.graph_root().to_path_buf();
        let nested = graph_root.join("archive/2026/client notes");
        fs::create_dir_all(&nested).unwrap();
        let page = "archive/2026/client notes/Ünicode outside.md";
        let journal = "archive/2026/client notes/2026_07_04.md";
        let configured_roots = [
            graph_root.join("content/nested pages"),
            graph_root.join("diary/日記"),
        ];
        let configured_before = configured_roots
            .iter()
            .map(|root| relative_files(root))
            .collect::<Vec<_>>();

        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);

        fs::write(graph_root.join(page), b"- created by another editor\n").unwrap();
        fs::write(graph_root.join(journal), b"- july fourth\n").unwrap();
        handle
            .observe_watcher(vec![
                SyncWatcherObservation::managed_path(page).unwrap(),
                SyncWatcherObservation::managed_path(journal).unwrap(),
            ])
            .unwrap();
        let ticks = drain_until_settled(&handle);
        assert!(admitted_an_epoch(&ticks), "create: {ticks:?}");

        fs::remove_file(graph_root.join(page)).unwrap();
        fs::remove_file(graph_root.join(journal)).unwrap();
        let before = fixture.manifest_count();
        handle
            .observe_watcher(vec![
                SyncWatcherObservation::managed_path(page).unwrap(),
                SyncWatcherObservation::managed_path(journal).unwrap(),
            ])
            .unwrap();
        let ticks = drain_until_settled(&handle);
        assert!(
            admitted_an_epoch(&ticks),
            "an external delete outside the configured roots must reconcile, \
             but the drain produced {ticks:?}"
        );
        assert!(fixture.manifest_count() > before);
        assert!(relative_files(&nested).is_empty());

        let shutdown = handle.clean_shutdown();
        assert!(
            matches!(shutdown, Ok(SyncShutdownOutcome::Safe(_))),
            "an external delete outside the configured roots must not make clean \
             Safe handoff unreachable, but shutdown returned {shutdown:?}"
        );
        for (root, expected) in configured_roots.iter().zip(&configured_before) {
            assert_eq!(
                &relative_files(root),
                expected,
                "a configured root changed while only nonstandard paths were deleted"
            );
        }
    }

    /// One graph file whose POSIX-legal name is outside the portable path
    /// alphabet must not stop the whole graph's reconciliation.
    ///
    /// `Meeting: notes.md` is not graph text by this build's own definition:
    /// `GraphTextScope::is_eligible` rejects it because `:` is outside
    /// `managed_component_is_portable`, `Graph::list_pages` never lists it, and
    /// `inventory_initial_shadow` never captures it. Every other layer treats
    /// such a path as ordinary retained non-text and ignores it, and unsupported
    /// graph text that *is* in scope (`.markdown`, `.MD`, excluded containers)
    /// is reported as named per-path reconciliation-import evidence.
    ///
    /// `collect_reconciliation_scan_pass` instead gates a mandatory
    /// `ManagedPath::parse` on `is_page_file` alone — an extension-only test
    /// that says nothing about scope — and turns its failure into an
    /// unrecoverable whole-scan error before the very next line computes the
    /// eligibility that would have classified the same path `RetainedNonText`.
    /// The user's unrelated offline edit therefore never imports, every tick
    /// repeats the same full graph walk, no surfaced evidence names the file,
    /// and clean `Safe` handoff becomes unreachable.
    ///
    /// The two controls pin the defect exactly: the identical name with a
    /// non-page extension, and the identical `.md` name inside a hidden
    /// container the scan never descends into, both reconcile normally.
    #[test]
    fn out_of_scope_non_portable_graph_text_name_does_not_block_the_graph() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-nonportable-name");
        let graph_root = fixture.graph_root().to_path_buf();
        fs::create_dir_all(graph_root.join("archive/.private")).unwrap();

        // Controls: same non-portable name, but out of the page-file extension
        // set, and inside a hidden container. Neither is graph text either.
        fs::write(graph_root.join("archive/Meeting: notes.txt"), b"plain\n").unwrap();
        fs::write(
            graph_root.join("archive/.private/Meeting: notes.md"),
            b"- hidden\n",
        )
        .unwrap();

        // The file under test: written by another editor or delivered by a
        // filesystem sync provider into an ordinary graph folder.
        let stray = "archive/Meeting: notes.md";
        fs::write(graph_root.join(stray), b"- not a Tine page\n").unwrap();

        // This build itself refuses to call it graph text.
        let probe = crate::Graph::open(&graph_root);
        assert!(
            !probe
                .list_pages()
                .iter()
                .any(|entry| entry.rel_path == stray),
            "the fixture no longer treats {stray} as out-of-scope graph text"
        );
        drop(probe);

        // The user's ordinary offline edit to a real page in a configured root.
        let edited = "content/nested pages/rename old.org";
        let edit = b"* edited in another editor while Tine was closed\n";
        fs::write(graph_root.join(edited), edit).unwrap();

        let before = fixture.manifest_count();
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        let ticks = drain_until_settled(&handle);
        assert!(
            admitted_an_epoch(&ticks),
            "one out-of-scope non-portable file name must not stop startup \
             reconciliation for the whole graph, but the drain produced {ticks:?}"
        );
        assert!(
            fixture.manifest_count() > before,
            "the unrelated offline edit produced no durable batch"
        );
        assert_eq!(fs::read(graph_root.join(edited)).unwrap(), edit);

        let shutdown = handle.clean_shutdown();
        assert!(
            matches!(shutdown, Ok(SyncShutdownOutcome::Safe(_))),
            "an out-of-scope non-portable file name must not make clean Safe \
             handoff unreachable, but shutdown returned {shutdown:?}"
        );
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Safe
        ));

        // Nothing was moved, rewritten, or removed to achieve that.
        assert_eq!(
            fs::read(graph_root.join(stray)).unwrap(),
            b"- not a Tine page\n"
        );
    }
}
