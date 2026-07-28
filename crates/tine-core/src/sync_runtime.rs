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
use crate::oplog::projection_store::ProjectionReceiptStore;
use crate::oplog::reconciliation_baseline::{
    BaselineTimestamp, ReconciliationBaseline, ReconciliationBaselineBinding,
    TrustedPrivateApplicationRuntimeRoot,
};
use crate::oplog::sqlite::ApplicationRuntimeRoot;
use crate::oplog::watcher_queue::WatcherObservation;
use crate::oplog::{ManagedPath, SessionId};

const ACTOR_CHANNEL_CAPACITY: usize = 64;
const ACTOR_STACK_BYTES: usize = 16 * 1024 * 1024;
const MAX_WATCHER_OBSERVATIONS: usize = 256;
const MAX_WATCHER_PATH_BYTES: usize = 64 * 1024;
const MAX_CLEAN_DRAIN_TURNS: usize = 4096;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncShutdownOutcome {
    Safe(SyncRuntimeStatusSnapshot),
    Terminal(SyncRuntimeStatusSnapshot),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncRuntimeRequestError {
    InvalidRequest(String),
    RequestTooLarge {
        observations: usize,
        path_bytes: usize,
    },
    ActorRefused(String),
    ActorUnavailable,
}

impl fmt::Display for SyncRuntimeRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(detail) => write!(formatter, "invalid sync request: {detail}"),
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
        let path_bytes = observations
            .iter()
            .map(SyncWatcherObservation::retained_path_bytes)
            .sum::<usize>();
        if observations.len() > MAX_WATCHER_OBSERVATIONS || path_bytes > MAX_WATCHER_PATH_BYTES {
            return Err(SyncRuntimeRequestError::RequestTooLarge {
                observations: observations.len(),
                path_bytes,
            });
        }
        let _operation = self.inner.operation.lock().unwrap();
        let (reply_sender, reply_receiver) = mpsc::channel();
        self.send(ActorRequest::Observe {
            observations,
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
    Observe {
        observations: Vec<SyncWatcherObservation>,
        reply: mpsc::Sender<Result<(), SyncRuntimeRequestError>>,
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
            ActorRequest::Observe {
                observations,
                reply,
            } => {
                let result = actor.observe(observations);
                let _ = reply.send(result);
                false
            }
            ActorRequest::Tick { reply } => {
                let result = actor.tick();
                let _ = reply.send(result);
                false
            }
            ActorRequest::Status { reply } => {
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
struct RuntimeActor {
    graph: Graph,
    receipts: ProjectionReceiptStore,
    authority: Option<LocalActiveAuthority>,
    runtime: Option<PromotedLocalRuntime>,
    feed: Option<ExactExternalFeedState>,
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

    fn tick(&mut self) -> SyncRuntimeTick {
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

    fn clean_shutdown(&mut self) -> Result<SyncShutdownOutcome, SyncRuntimeRequestError> {
        if self.terminal.is_some() {
            return Ok(SyncShutdownOutcome::Terminal(self.snapshot()));
        }

        for _ in 0..MAX_CLEAN_DRAIN_TURNS {
            let tick = self.tick();
            match tick {
                SyncRuntimeTick::Idle if !self.last_watcher.pending => break,
                SyncRuntimeTick::AdmittedNoop { .. }
                | SyncRuntimeTick::AdmittedComplete { .. }
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
    use std::fs;
    use std::path::Path;
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
    fn watcher_requests_are_bounded_before_channel_intake() {
        let fixture = RuntimeHostFixture::safe("sync-runtime-watcher-request-bounds");
        let handle = active_handle(SyncRuntimeHandle::open(fixture.request()));
        drive_initial_feed(&handle);
        let before = handle.status().unwrap();
        let observations = (0..=MAX_WATCHER_OBSERVATIONS)
            .map(|_| SyncWatcherObservation::UnknownPath)
            .collect::<Vec<_>>();
        assert!(matches!(
            handle.observe_watcher(observations),
            Err(SyncRuntimeRequestError::RequestTooLarge {
                observations,
                path_bytes: 0,
            }) if observations == MAX_WATCHER_OBSERVATIONS + 1
        ));
        assert_eq!(
            handle.status().unwrap().watcher,
            before.watcher,
            "a rejected oversized request must never reach actor intake"
        );
        assert!(matches!(
            handle.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
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
    fn unsafe_takeover_cannot_run_with_old_owner_and_uses_recovery_gate_after_crash() {
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
        assert!(matches!(
            handle.tick().unwrap(),
            SyncRuntimeTick::RecoveryBlocked(_)
        ));
        drop(handle);
        assert!(matches!(
            fixture.handoff(),
            EnrollmentDiscoveryHandoff::Unsafe { .. }
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
        assert_eq!(
            ACTOR_THREADS_FINISHED.load(std::sync::atomic::Ordering::SeqCst),
            finished_before + 1
        );

        assert!(matches!(
            handle.clean_shutdown().unwrap(),
            SyncShutdownOutcome::Safe(_)
        ));
    }

    #[test]
    fn unchanged_safe_reopen_does_not_schedule_graph_wide_reconciliation() {
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
            !status.watcher.pending_requires_full_scan,
            "an unchanged authenticated Safe reopen must resume from its durable frontier \
             without scheduling routine graph-wide reconciliation: {:?}",
            status.watcher
        );
        assert_eq!(fixture.manifest_count(), manifests_before_reopen);
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
        assert!(matches!(
            reopened.tick().unwrap(),
            SyncRuntimeTick::RecoveryBlocked(_)
        ));
        drop(reopened);
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
        drop(reopened);
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
}
