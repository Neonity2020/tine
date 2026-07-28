//! Actor-owned exact external Markdown/Org feed execution.
//!
//! This module is the production core seam between normalized platform watcher
//! observations and the sparse-oplog reconciliation path. It deliberately has
//! crate visibility only. A later runtime actor may call [`ExactExternalFeedOwner::observe`]
//! and [`ExactExternalFeedOwner::drain_one`], but it cannot obtain the raw
//! graph feed lease, watcher queue owner, `LocalActive` authority, promoted
//! runtime capabilities, reconciliation continuation, or SQLite applier.
//!
//! One move-only owner binds all of those values to one exact promoted storage
//! binding. Queue epochs are acknowledged only after admitted reconciliation,
//! exact Graph feed/cache publication, and any initial catch-up publication
//! have all reached terminal `Noop` or `Complete`. Every coarser or incomplete
//! result keeps the epoch owed. Uncertainty rebuilds the complete exact Graph
//! index at the queue fence before it runs complete reconciliation; it never
//! guesses a managed path.

use std::collections::BTreeSet;
use std::cell::{Cell, RefCell};
use std::fmt;

use crate::model::{
    Graph, GraphTextExactFeedFailure, GraphTextExactFeedLease, GraphTextExactFeedPathClass,
};

use super::{
    hot_engine::ProjectionStorageBinding,
    local_active::{
        ExternalImportAdmission, LocalActiveAuthority, PromotedLocalRuntime, RuntimeRevocation,
    },
    reconciliation_baseline::{BaselineTimestamp, ReconciliationBaseline},
    reconciliation_scan::{ReconciliationSchedulerLimits, ReconciliationTrigger},
    reconciliation_session::{
        ReconciliationPendingContinuation, ReconciliationSession,
        ReconciliationSessionDependencies, ReconciliationSessionStep,
        ReconciliationTerminalChangedPaths,
    },
    watcher_queue::{
        WatcherDrainError, WatcherEnqueueError, WatcherEpoch, WatcherObservation,
        WatcherQueueLimits, WatcherQueueOwner, WatcherSettlementError,
    },
    ManagedPath, ProjectionReceiptStore,
};

const EXACT_FEED_MAXIMUM_PATHS: usize = 256;
const EXACT_FEED_MAXIMUM_PATH_BYTES: usize = 64 * 1024;

/// Construction failed before a platform-facing observer existed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExactExternalFeedOpenError {
    detail: String,
}

impl ExactExternalFeedOpenError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for ExactExternalFeedOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for ExactExternalFeedOpenError {}

/// Watcher intake was refused without consuming or acknowledging another
/// queue's work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExactExternalFeedObserveError {
    ForeignBinding,
    Terminal,
    Queue(WatcherEnqueueError),
}

impl fmt::Display for ExactExternalFeedObserveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignBinding => {
                formatter.write_str("exact external feed observation has a foreign binding")
            }
            Self::Terminal => formatter.write_str("exact external feed owner is terminal"),
            Self::Queue(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ExactExternalFeedObserveError {}

/// Permanent stop reason for one owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExactExternalFeedTerminal {
    WorkspaceAuthorityRevoked(RuntimeRevocation),
    RuntimeAuthority(String),
    GraphFeed(String),
    Queue(String),
}

impl fmt::Display for ExactExternalFeedTerminal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceAuthorityRevoked(revocation) => revocation.fmt(formatter),
            Self::RuntimeAuthority(detail) => write!(
                formatter,
                "exact external feed runtime authority is terminal: {detail}"
            ),
            Self::GraphFeed(detail) => {
                write!(formatter, "exact external Graph feed is terminal: {detail}")
            }
            Self::Queue(detail) => {
                write!(formatter, "exact external watcher queue is terminal: {detail}")
            }
        }
    }
}

/// Result of one bounded actor turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExactExternalFeedDrain {
    Idle,
    /// The promoted runtime did not adopt a clean `Safe` handoff. Work remains
    /// queued; only a fresh runtime reopen/takeover may change this result.
    RecoveryBlocked(&'static str),
    /// A durable coordinator continuation or required follow-up full scan is
    /// retained by this owner. The queue epoch remains in flight and unacked.
    Recovering,
    /// The complete scan could not yet reach one stable admitted result. The
    /// same queue epoch remains in flight and unacked.
    RetryFull,
    /// Reconciliation reached a terminal blocked result. The queue epoch was
    /// abandoned back to the queue, not acknowledged.
    Blocked,
    /// A retryable, pre-ack operation failed. The queue epoch remains owed.
    Failed(String),
    AdmittedNoop { epoch: u64 },
    AdmittedComplete { epoch: u64 },
    Terminal(ExactExternalFeedTerminal),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ActiveDrainScope {
    Exact(BTreeSet<ManagedPath>),
    FullScan,
}

struct ActiveDrain {
    epoch: WatcherEpoch,
    scope: ActiveDrainScope,
    continuation: Option<ReconciliationPendingContinuation>,
    rebase_before_step: bool,
}

/// The single, move-only owner of one promoted runtime's exact external feed.
///
/// There is intentionally no `Clone`, no cloneable drain handle, and no
/// accessor for `authority`, `runtime`, `lease`, `queue`, `reconciliation`, or
/// `baseline`. Watcher intake is an actor request on this owner, not a second
/// owner of any of those capabilities.
pub(crate) struct ExactExternalFeedOwner {
    binding: ProjectionStorageBinding,
    authority: LocalActiveAuthority,
    runtime: PromotedLocalRuntime,
    lease: GraphTextExactFeedLease,
    queue: WatcherQueueOwner,
    reconciliation: ReconciliationSession,
    baseline: ReconciliationBaseline,
    active: Option<ActiveDrain>,
    feed_sequence: u64,
    caught_up_published: bool,
    terminal: Option<ExactExternalFeedTerminal>,
}

impl fmt::Debug for ExactExternalFeedOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExactExternalFeedOwner")
            .field("terminal", &self.terminal)
            .field("feed_sequence", &self.feed_sequence)
            .finish_non_exhaustive()
    }
}

impl ExactExternalFeedOwner {
    /// Bind one exact Graph, receipt store, disposable baseline, live
    /// `LocalActive` authority, and promoted runtime into one owner.
    ///
    /// Every fresh owner seeds one uncertainty epoch after building the initial
    /// exact index. Its first admitted drain must therefore rebase and fully
    /// reconcile even when no watcher callback arrived. This closes both the
    /// initial-build/watch-install race and the process-crash loss of an
    /// in-memory queue.
    pub(crate) fn open(
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        authority: LocalActiveAuthority,
        runtime: PromotedLocalRuntime,
        baseline: ReconciliationBaseline,
    ) -> Result<Self, ExactExternalFeedOpenError> {
        let binding = validate_open_binding(graph, receipts, &runtime, &baseline)?;
        let queue = WatcherQueueOwner::new(
            binding,
            WatcherQueueLimits {
                maximum_paths: EXACT_FEED_MAXIMUM_PATHS,
                maximum_path_bytes: EXACT_FEED_MAXIMUM_PATH_BYTES,
                maximum_uncertain_reasons: 8,
            },
        );
        let lease = graph
            .arm_graph_text_exact_feed(0)
            .map_err(|error| ExactExternalFeedOpenError::new(error.to_string()))?;
        graph
            .build_graph_text_exact_feed(&lease)
            .map_err(|error| ExactExternalFeedOpenError::new(error.to_string()))?;

        // This is not optional bookkeeping. A new process cannot reconstruct
        // the predecessor's in-memory watcher epoch, so it always owes one
        // complete scan before any exact epoch may be acknowledged.
        queue
            .handle()
            .enqueue(binding, [WatcherObservation::RescanRequired])
            .map_err(|error| ExactExternalFeedOpenError::new(error.to_string()))?;

        Ok(Self {
            binding,
            authority,
            runtime,
            lease,
            queue,
            reconciliation: ReconciliationSession::new(ReconciliationSchedulerLimits {
                maximum_watcher_paths: EXACT_FEED_MAXIMUM_PATHS,
                maximum_watcher_path_bytes: EXACT_FEED_MAXIMUM_PATH_BYTES,
                maximum_precondition_paths: EXACT_FEED_MAXIMUM_PATHS,
                maximum_precondition_path_bytes: EXACT_FEED_MAXIMUM_PATH_BYTES,
                maximum_full_scan_reasons: 8,
            }),
            baseline,
            active: None,
            feed_sequence: 0,
            caught_up_published: false,
            terminal: None,
        })
    }

    pub(crate) fn terminal(&self) -> Option<&ExactExternalFeedTerminal> {
        self.terminal.as_ref()
    }

    /// Submit normalized watcher observations through the existing bounded
    /// watcher queue.
    ///
    /// Exact paths are reclassified against the retained Graph scope before
    /// intake. An excluded path is conservatively made uncertain; a
    /// configuration mutation is terminal because its scope can be recovered
    /// only by opening a fresh Graph/runtime owner.
    pub(crate) fn observe(
        &mut self,
        graph: &Graph,
        binding: ProjectionStorageBinding,
        observations: impl IntoIterator<Item = WatcherObservation>,
    ) -> Result<(), ExactExternalFeedObserveError> {
        if self.terminal.is_some() {
            return Err(ExactExternalFeedObserveError::Terminal);
        }
        if binding != self.binding {
            return Err(ExactExternalFeedObserveError::ForeignBinding);
        }
        // Classification stays lazy: the watcher queue stops polling as soon
        // as uncertainty or overflow subsumes the rest of the batch. This
        // adapter therefore cannot materialize an arbitrarily large callback
        // merely to validate it.
        let configuration_mutated = Cell::new(false);
        let classification_error = RefCell::new(None::<String>);
        let normalized = observations.into_iter().map(|observation| {
            match observation {
                WatcherObservation::ManagedPath(path) => {
                    match graph.classify_graph_text_exact_feed_path(path.as_str()) {
                        Ok(GraphTextExactFeedPathClass::RetainedFile) => {
                            WatcherObservation::ManagedPath(path)
                        }
                        Ok(GraphTextExactFeedPathClass::Excluded) => {
                            WatcherObservation::UnknownPath
                        }
                        Ok(GraphTextExactFeedPathClass::Configuration) => {
                            configuration_mutated.set(true);
                            WatcherObservation::UnknownPath
                        }
                        Err(error) => {
                            *classification_error.borrow_mut() = Some(error.to_string());
                            WatcherObservation::UnknownPath
                        }
                    }
                }
                uncertain => uncertain,
            }
        });
        let enqueue = self
            .queue
            .handle()
            .enqueue(self.binding, normalized)
            .map_err(ExactExternalFeedObserveError::Queue);
        if let Some(detail) = classification_error.into_inner() {
            let _ = graph.poison_graph_text_exact_feed(
                &self.lease,
                GraphTextExactFeedFailure::RootMutation,
                &detail,
            );
            self.terminal = Some(ExactExternalFeedTerminal::GraphFeed(detail));
            return Err(ExactExternalFeedObserveError::Terminal);
        }
        if configuration_mutated.get() {
            let detail =
                "graph-text configuration changed; a fresh runtime reopen is required";
            let _ = graph.poison_graph_text_exact_feed(
                &self.lease,
                GraphTextExactFeedFailure::ScopeOrConfigMutation,
                detail,
            );
            self.terminal = Some(ExactExternalFeedTerminal::GraphFeed(detail.to_owned()));
            return Err(ExactExternalFeedObserveError::Terminal);
        }
        enqueue.map(|_| ())
    }

    /// Drive at most one queue epoch toward terminal admission.
    ///
    /// A continuation/follow-up scan may require another actor turn, but there
    /// is never more than one queue drain in flight and no nonterminal result
    /// acknowledges it.
    pub(crate) fn drain_one(
        &mut self,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        observed_at: BaselineTimestamp,
    ) -> ExactExternalFeedDrain {
        if let Some(terminal) = &self.terminal {
            return ExactExternalFeedDrain::Terminal(terminal.clone());
        }
        if let Some(revocation) = self.runtime.workspace_authority_revocation() {
            return self.stop_revoked(graph, revocation);
        }
        match self.runtime.automatic_external_import() {
            ExternalImportAdmission::Allowed => {}
            ExternalImportAdmission::Blocked(reason) => {
                return ExactExternalFeedDrain::RecoveryBlocked(reason);
            }
        }
        if let Err(error) = validate_live_binding(graph, receipts, &self.runtime, self.binding) {
            return self.stop_runtime(graph, error.detail);
        }

        if self.active.is_none() {
            let drain = match self.queue.begin_drain(self.binding) {
                Ok(Some(drain)) => drain,
                Ok(None) => return ExactExternalFeedDrain::Idle,
                Err(error) => return self.handle_drain_error(graph, error),
            };
            let scope = match drain.trigger() {
                ReconciliationTrigger::WatcherPaths(paths) => {
                    ActiveDrainScope::Exact(paths.clone())
                }
                ReconciliationTrigger::WatcherUncertain => ActiveDrainScope::FullScan,
                _ => {
                    let detail =
                        "watcher queue produced a non-watcher reconciliation trigger".to_owned();
                    let _ = self.queue.abandon_drain(drain.epoch());
                    return self.stop_queue(graph, detail);
                }
            };
            self.active = Some(ActiveDrain {
                epoch: drain.epoch(),
                rebase_before_step: matches!(scope, ActiveDrainScope::FullScan),
                scope,
                continuation: None,
            });
        }

        if self
            .active
            .as_ref()
            .is_some_and(|active| active.rebase_before_step)
        {
            let epoch = self
                .active
                .as_ref()
                .expect("active drain disappeared")
                .epoch;
            match graph.rebase_graph_text_exact_feed_at_fence(&self.lease, epoch.sequence()) {
                Ok(()) => {
                    self.feed_sequence = epoch.sequence();
                    self.active
                        .as_mut()
                        .expect("active drain disappeared")
                        .rebase_before_step = false;
                }
                Err(error) => {
                    if let Some(revocation) = self.runtime.workspace_authority_revocation() {
                        return self.stop_revoked(graph, revocation);
                    }
                    if !self.lease.is_terminal() {
                        return ExactExternalFeedDrain::Failed(error.to_string());
                    }
                    return self.stop_graph(
                        graph,
                        GraphTextExactFeedFailure::BackendError,
                        &error.to_string(),
                    );
                }
            }
        }

        let is_new_job = self
            .active
            .as_ref()
            .is_some_and(|active| active.continuation.is_none())
            && !self.reconciliation.status().active;
        if is_new_job {
            let trigger = match &self.active.as_ref().expect("active drain disappeared").scope {
                ActiveDrainScope::Exact(paths) => ReconciliationTrigger::WatcherPaths(paths.clone()),
                ActiveDrainScope::FullScan => ReconciliationTrigger::WatcherUncertain,
            };
            self.reconciliation.trigger(trigger);
        }

        let continuation = self
            .active
            .as_mut()
            .expect("active drain disappeared")
            .continuation
            .take();
        let step = match self.execute_reconciliation(
            graph,
            receipts,
            observed_at,
            continuation,
        ) {
            Ok(step) => step,
            Err(ExecuteReconciliationError::Revoked(revocation)) => {
                return self.stop_revoked(graph, revocation);
            }
            Err(ExecuteReconciliationError::Runtime(detail)) => {
                return self.stop_runtime(graph, detail);
            }
        };
        match step {
            ReconciliationSessionStep::Pending(continuation) => {
                self.active
                    .as_mut()
                    .expect("active drain disappeared")
                    .continuation = Some(continuation);
                ExactExternalFeedDrain::Recovering
            }
            ReconciliationSessionStep::RetryFull => {
                let active = self.active.as_mut().expect("active drain disappeared");
                // A targeted scan which asks for RetryFull has made its exact
                // hint insufficient. Collapse this same queue epoch to one
                // full rebase; never preserve the targeted shape and later
                // publish an incomplete exact batch.
                active.scope = ActiveDrainScope::FullScan;
                active.rebase_before_step = true;
                ExactExternalFeedDrain::RetryFull
            }
            ReconciliationSessionStep::Blocked => {
                self.reconciliation.take_terminal_changed_paths();
                self.abandon_active();
                ExactExternalFeedDrain::Blocked
            }
            ReconciliationSessionStep::Idle => {
                let detail =
                    "active exact-feed drain reached an idle reconciliation session".to_owned();
                self.stop_runtime(graph, detail)
            }
            ReconciliationSessionStep::Noop | ReconciliationSessionStep::Complete => {
                let Some(changed_paths) = self.reconciliation.take_terminal_changed_paths() else {
                    // A stable full scan may require one post-drain confirmation
                    // scan. The intermediate semantic outcome is not terminal
                    // for the queue epoch and therefore cannot be acknowledged.
                    if matches!(
                        self.active.as_ref().map(|active| &active.scope),
                        Some(ActiveDrainScope::FullScan)
                    ) {
                        self.active
                            .as_mut()
                            .expect("active drain disappeared")
                            .rebase_before_step = true;
                    }
                    return ExactExternalFeedDrain::Recovering;
                };
                self.finish_terminal(graph, step, changed_paths)
            }
        }
    }

    fn finish_terminal(
        &mut self,
        graph: &Graph,
        step: ReconciliationSessionStep,
        changed_paths: ReconciliationTerminalChangedPaths,
    ) -> ExactExternalFeedDrain {
        let (epoch, scope) = {
            let active = self.active.as_ref().expect("terminal drain disappeared");
            (active.epoch, active.scope.clone())
        };
        match (&scope, changed_paths.complete_scan()) {
            (ActiveDrainScope::FullScan, true) => {
                if !changed_paths.exact_paths().is_empty() || self.feed_sequence != epoch.sequence()
                {
                    return self.stop_runtime(
                        graph,
                        "terminal full reconciliation did not match its exact feed fence",
                    );
                }
            }
            (ActiveDrainScope::Exact(expected), false)
                if expected == changed_paths.exact_paths() =>
            {
                if self.feed_sequence < epoch.sequence() {
                    let Some(first_sequence) = self.feed_sequence.checked_add(1) else {
                        return self.stop_graph(
                            graph,
                            GraphTextExactFeedFailure::SequenceDiscontinuity,
                            "exact feed sequence exhausted",
                        );
                    };
                    let batch = match self.lease.batch(
                        first_sequence,
                        epoch.sequence(),
                        changed_paths
                            .exact_paths()
                            .iter()
                            .map(|path| path.as_str().to_owned()),
                    ) {
                        Ok(batch) => batch,
                        Err(error) => {
                            return self.stop_graph(
                                graph,
                                GraphTextExactFeedFailure::UnsupportedOrAmbiguousEvent,
                                &error.to_string(),
                            );
                        }
                    };
                    if let Err(error) =
                        graph.apply_graph_text_exact_feed_batch(&self.lease, batch)
                    {
                        return self.stop_graph(
                            graph,
                            GraphTextExactFeedFailure::BackendError,
                            &error.to_string(),
                        );
                    }
                    self.feed_sequence = epoch.sequence();
                } else if self.feed_sequence != epoch.sequence() {
                    return self.stop_graph(
                        graph,
                        GraphTextExactFeedFailure::SequenceDiscontinuity,
                        "exact queue epoch moved behind the Graph feed",
                    );
                }
            }
            _ => {
                return self.stop_runtime(
                    graph,
                    "terminal reconciliation changed-path report differs from its queue epoch",
                );
            }
        }

        if !self.caught_up_published {
            if let Err(error) = graph
                .publish_graph_text_exact_feed_caught_up(&self.lease, self.feed_sequence)
            {
                return self.stop_graph(
                    graph,
                    GraphTextExactFeedFailure::BackendError,
                    &error.to_string(),
                );
            }
            self.caught_up_published = true;
        }

        if let Err(error) = exact_feed_after_terminal_before_ack_hook() {
            let detail = error.to_string();
            self.abandon_active();
            return ExactExternalFeedDrain::Failed(detail);
        }

        if let Err(error) = self.queue.acknowledge_drain(epoch) {
            return self.handle_settlement_error(graph, error);
        }
        self.active = None;
        match step {
            ReconciliationSessionStep::Noop => {
                ExactExternalFeedDrain::AdmittedNoop {
                    epoch: epoch.sequence(),
                }
            }
            ReconciliationSessionStep::Complete => {
                ExactExternalFeedDrain::AdmittedComplete {
                    epoch: epoch.sequence(),
                }
            }
            _ => unreachable!("finish_terminal accepts only admitted terminal outcomes"),
        }
    }

    fn abandon_active(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        let _ = self.queue.abandon_drain(active.epoch);
    }

    fn handle_drain_error(
        &mut self,
        graph: &Graph,
        error: WatcherDrainError,
    ) -> ExactExternalFeedDrain {
        match error {
            WatcherDrainError::DrainInFlight(_) => self.stop_queue(
                graph,
                "watcher queue has an unowned drain in flight".to_owned(),
            ),
            WatcherDrainError::ForeignBinding => {
                self.stop_queue(graph, "watcher queue binding changed".to_owned())
            }
            WatcherDrainError::Quiescing => {
                ExactExternalFeedDrain::Failed("watcher queue is quiescing".to_owned())
            }
        }
    }

    fn handle_settlement_error(
        &mut self,
        graph: &Graph,
        error: WatcherSettlementError,
    ) -> ExactExternalFeedDrain {
        self.stop_queue(
            graph,
            format!("terminal queue acknowledgement was refused: {error}"),
        )
    }

    fn stop_revoked(
        &mut self,
        graph: &Graph,
        revocation: RuntimeRevocation,
    ) -> ExactExternalFeedDrain {
        self.abandon_active();
        let detail = revocation.to_string();
        let _ = graph.poison_graph_text_exact_feed(
            &self.lease,
            GraphTextExactFeedFailure::BackendError,
            &detail,
        );
        let terminal = ExactExternalFeedTerminal::WorkspaceAuthorityRevoked(revocation);
        self.terminal = Some(terminal.clone());
        ExactExternalFeedDrain::Terminal(terminal)
    }

    fn stop_runtime(&mut self, graph: &Graph, detail: impl Into<String>) -> ExactExternalFeedDrain {
        self.abandon_active();
        let detail = detail.into();
        let _ = graph.poison_graph_text_exact_feed(
            &self.lease,
            GraphTextExactFeedFailure::BackendError,
            &detail,
        );
        let terminal = ExactExternalFeedTerminal::RuntimeAuthority(detail);
        self.terminal = Some(terminal.clone());
        ExactExternalFeedDrain::Terminal(terminal)
    }

    fn stop_graph(
        &mut self,
        graph: &Graph,
        reason: GraphTextExactFeedFailure,
        detail: &str,
    ) -> ExactExternalFeedDrain {
        self.abandon_active();
        let _ = graph.poison_graph_text_exact_feed(&self.lease, reason, detail);
        let terminal = ExactExternalFeedTerminal::GraphFeed(detail.to_owned());
        self.terminal = Some(terminal.clone());
        ExactExternalFeedDrain::Terminal(terminal)
    }

    fn stop_queue(&mut self, graph: &Graph, detail: String) -> ExactExternalFeedDrain {
        self.abandon_active();
        let _ = graph.poison_graph_text_exact_feed(
            &self.lease,
            GraphTextExactFeedFailure::OverflowOrQueueLoss,
            &detail,
        );
        let terminal = ExactExternalFeedTerminal::Queue(detail);
        self.terminal = Some(terminal.clone());
        ExactExternalFeedDrain::Terminal(terminal)
    }

    fn execute_reconciliation(
        &mut self,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        observed_at: BaselineTimestamp,
        continuation: Option<ReconciliationPendingContinuation>,
    ) -> Result<ReconciliationSessionStep, ExecuteReconciliationError> {
        let Self {
            authority,
            runtime,
            reconciliation,
            baseline,
            ..
        } = self;
        let mut window = match runtime.admit_promoted_mutation(authority, graph) {
            Ok(window) => window,
            Err(error) => {
                return Err(runtime
                    .workspace_authority_revocation()
                    .map(ExecuteReconciliationError::Revoked)
                    .unwrap_or_else(|| ExecuteReconciliationError::Runtime(error.to_string())));
            }
        };
        let (admission, engine, database, tail) = match window.parts() {
            Ok(parts) => parts,
            Err(error) => {
                return Err(ExecuteReconciliationError::Revoked(
                    error.revocation().clone(),
                ));
            }
        };
        let dependencies = ReconciliationSessionDependencies {
            admission: &admission,
            graph,
            receipts,
            engine,
            database,
            tail,
            baseline,
            observed_at,
        };
        match continuation {
            Some(token) => reconciliation.resume(token, dependencies),
            None => reconciliation.step(dependencies),
        }
        .map_err(|error| {
            ExecuteReconciliationError::Runtime(format!(
                "reconciliation session refused its owned action: {error:?}"
            ))
        })
    }
}

enum ExecuteReconciliationError {
    Revoked(RuntimeRevocation),
    Runtime(String),
}

fn validate_open_binding(
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    runtime: &PromotedLocalRuntime,
    baseline: &ReconciliationBaseline,
) -> Result<ProjectionStorageBinding, ExactExternalFeedOpenError> {
    let graph_resource = graph
        .canonical_resource_id()
        .map_err(|error| ExactExternalFeedOpenError::new(error.to_string()))?;
    let scope_binding = graph
        .graph_text_scope_binding()
        .map_err(|error| ExactExternalFeedOpenError::new(error.to_string()))?;
    let endpoint = runtime.endpoint();
    let receipt_store_id = runtime
        .engine()
        .projection_receipt_store_id()
        .ok_or_else(|| ExactExternalFeedOpenError::new("promoted engine has no receipt binding"))?;
    let binding = ProjectionStorageBinding {
        endpoint,
        receipt_store_id,
    };
    if endpoint.graph_resource_id() != graph_resource
        || receipts.workspace_id() != runtime.engine().workspace_id()
        || receipts.endpoint_binding() != Some(endpoint)
        || receipts.store_id() != receipt_store_id
        || runtime.engine().projection_endpoint_binding() != Some(endpoint)
    {
        return Err(ExactExternalFeedOpenError::new(
            "Graph, promoted runtime, engine, and receipt-store binding differ",
        ));
    }
    let baseline_binding = baseline.binding();
    if baseline_binding.workspace() != runtime.engine().workspace_id()
        || baseline_binding.endpoint() != endpoint.endpoint_id()
        || baseline_binding.graph_resource() != graph_resource
        || baseline_binding.scope_binding() != scope_binding
    {
        return Err(ExactExternalFeedOpenError::new(
            "reconciliation baseline has a foreign runtime or Graph binding",
        ));
    }
    Ok(binding)
}

fn validate_live_binding(
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    runtime: &PromotedLocalRuntime,
    expected: ProjectionStorageBinding,
) -> Result<(), ExactExternalFeedOpenError> {
    let observed = validate_runtime_storage_binding(graph, receipts, runtime)?;
    if observed != expected {
        return Err(ExactExternalFeedOpenError::new(
            "exact external feed runtime binding changed",
        ));
    }
    Ok(())
}

fn validate_runtime_storage_binding(
    graph: &Graph,
    receipts: &ProjectionReceiptStore,
    runtime: &PromotedLocalRuntime,
) -> Result<ProjectionStorageBinding, ExactExternalFeedOpenError> {
    let endpoint = runtime.endpoint();
    let receipt_store_id = runtime
        .engine()
        .projection_receipt_store_id()
        .ok_or_else(|| ExactExternalFeedOpenError::new("promoted engine has no receipt binding"))?;
    if graph
        .canonical_resource_id()
        .map_err(|error| ExactExternalFeedOpenError::new(error.to_string()))?
        != endpoint.graph_resource_id()
        || receipts.workspace_id() != runtime.engine().workspace_id()
        || receipts.endpoint_binding() != Some(endpoint)
        || receipts.store_id() != receipt_store_id
        || runtime.engine().projection_endpoint_binding() != Some(endpoint)
    {
        return Err(ExactExternalFeedOpenError::new(
            "live Graph, runtime, engine, or receipt-store binding differs",
        ));
    }
    Ok(ProjectionStorageBinding {
        endpoint,
        receipt_store_id,
    })
}

#[cfg(test)]
thread_local! {
    static EXACT_FEED_AFTER_TERMINAL_BEFORE_ACK_HOOK:
        std::cell::RefCell<Option<Box<dyn FnOnce() -> std::io::Result<()>>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn exact_feed_after_terminal_before_ack_hook() -> std::io::Result<()> {
    EXACT_FEED_AFTER_TERMINAL_BEFORE_ACK_HOOK
        .with(|hook| hook.borrow_mut().take().map_or(Ok(()), |hook| hook()))
}

#[cfg(not(test))]
fn exact_feed_after_terminal_before_ack_hook() -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    use uuid::Uuid;

    use super::*;
    use crate::oplog::enrollment::{
        compose_verified_local, enrollment_application_root_for_test, EnrollmentApplicationRoot,
        EnrollmentBindingV1, PreparationId, VerifiedLocalEvidence, VerifiedLocalProofSet,
    };
    use crate::oplog::import::{
        prepare_inactive_bootstrap_import, publish_install_verify_inactive_bootstrap,
        reopen_inactive_bootstrap_accepted_authority, InactiveBootstrapAcceptedAuthority,
        InactiveBootstrapPreparedPublication, InactiveBootstrapVerifiedPublication,
    };
    use crate::oplog::local_active::{
        activate_verified_local, reopen_promoted_local_runtime, seal_local_runtime_promotion,
        take_over_promoted_local_runtime, InactiveBootstrapRuntimeSession, LocalActiveRuntime,
        PromotedRuntimeOpen, RuntimeRecoveryState,
    };
    use crate::oplog::migration_backup::{
        verify_migration_source_backup, MigrationBackupRoot, VerifiedSourceBackup,
    };
    use crate::oplog::reconciliation_baseline::{
        ReconciliationBaselineBinding, TrustedPrivateApplicationRuntimeRoot,
    };
    use crate::oplog::shadow_projection::{
        verify_inactive_bootstrap_shadow_projection, VerifiedShadowProjection,
    };
    use crate::oplog::{
        ApplicationRuntimeRoot, CanonicalArchiveResourceId, DeviceId, DocumentId, LineageDigest,
        ObjectStore, OpenProjection, ProjectionEndpointBinding, ProjectionEndpointId,
        ProjectionReceiptStoreId, ReferenceCatalogPolicyV1, SessionId, WorkspaceId,
    };

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "tine-exact-external-feed-{label}-{}",
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

    /// One real inactive bootstrap and enrollment. The owner tests intentionally
    /// pay this setup cost: fake authorities cannot prove the production seam
    /// owns the sole promoted SQLite applier and admitted coordinator path.
    struct Fixture {
        root: TestRoot,
        graph_root: PathBuf,
        graph: Graph,
        receipts: ProjectionReceiptStore,
        archive_root: PathBuf,
        workspace: WorkspaceId,
        prepared: InactiveBootstrapPreparedPublication,
        verified: InactiveBootstrapVerifiedPublication,
        accepted: InactiveBootstrapAcceptedAuthority,
        backup_roots: MigrationBackupRoot,
        backup: VerifiedSourceBackup,
        bootstrap: Option<InactiveBootstrapRuntimeSession>,
        archive_resource: CanonicalArchiveResourceId,
        shadow: VerifiedShadowProjection,
        preparation: PreparationId,
    }

    impl Fixture {
        fn new(
            label: &str,
            config: Option<&[u8]>,
            files: impl IntoIterator<Item = (String, Vec<u8>)>,
        ) -> Self {
            let root = TestRoot::new(label);
            let graph_root = root.path().join("graph");
            fs::create_dir(&graph_root).unwrap();
            if let Some(config) = config {
                fs::create_dir(graph_root.join("logseq")).unwrap();
                fs::write(graph_root.join("logseq/config.edn"), config).unwrap();
            }
            for (path, bytes) in files {
                let destination = graph_root.join(path);
                fs::create_dir_all(destination.parent().unwrap()).unwrap();
                fs::write(destination, bytes).unwrap();
            }
            let graph = Graph::open(&graph_root);
            let workspace = WorkspaceId::from_uuid(Uuid::new_v4());
            let lineage = LineageDigest::of(format!("exact-feed-{label}").as_bytes());
            let catalog_document_id = DocumentId::from_uuid(Uuid::new_v4());

            let receipt_root = root.path().join("receipts");
            fs::create_dir(&receipt_root).unwrap();
            let endpoint = ProjectionEndpointBinding::enroll_graph(
                &graph,
                ProjectionEndpointId::from_uuid(Uuid::new_v4()),
                DeviceId::from_uuid(Uuid::new_v4()),
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
            let storage = ProjectionStorageBinding {
                endpoint,
                receipt_store_id: receipts.store_id(),
            };
            let verified = publish_install_verify_inactive_bootstrap(
                &prepared,
                ObjectStore::open(&archive_root, workspace).unwrap(),
                storage,
            )
            .unwrap();
            let accepted = reopen_inactive_bootstrap_accepted_authority(
                &verified,
                ObjectStore::open(&archive_root, workspace).unwrap(),
            )
            .unwrap();

            let device_root = root.path().join("device");
            fs::create_dir(&device_root).unwrap();
            let backup_roots = MigrationBackupRoot::open(&device_root, &graph_root).unwrap();
            let backup =
                verify_migration_source_backup(&backup_roots, &prepared, &verified).unwrap();
            let bootstrap_runtime =
                ApplicationRuntimeRoot::open_for_test(&root.path().join("bootstrap-runtime"))
                    .unwrap();
            let bootstrap = InactiveBootstrapRuntimeSession::open(
                &archive_root,
                workspace,
                &root.path().join("bootstrap.sqlite"),
                &bootstrap_runtime,
                &accepted,
            )
            .unwrap();
            let archive_resource = accepted
                .store()
                .provision_enrolled_archive_resource_id()
                .unwrap();
            let shadow = verify_inactive_bootstrap_shadow_projection(
                &graph,
                &backup_roots,
                &prepared,
                &verified,
                &backup,
                &accepted,
                bootstrap.projection(),
                bootstrap.sqlite_proof(),
            )
            .unwrap();
            Self {
                root,
                graph_root,
                graph,
                receipts,
                archive_root,
                workspace,
                prepared,
                verified,
                accepted,
                backup_roots,
                backup,
                bootstrap: Some(bootstrap),
                archive_resource,
                shadow,
                preparation: PreparationId::new(),
            }
        }

        fn bootstrap(&self) -> &InactiveBootstrapRuntimeSession {
            self.bootstrap.as_ref().unwrap()
        }

        fn sqlite(&self) -> &OpenProjection {
            self.bootstrap().projection()
        }

        fn proofs(&self) -> VerifiedLocalProofSet<'_> {
            VerifiedLocalProofSet {
                graph: &self.graph,
                roots: &self.backup_roots,
                prepared: &self.prepared,
                verified_publication: &self.verified,
                source_backup: &self.backup,
                accepted_authority: &self.accepted,
                sqlite: self.sqlite(),
                sqlite_projection: self.bootstrap().sqlite_proof(),
                shadow_projection: &self.shadow,
            }
        }

        fn inactive_runtime(&self) -> LocalActiveRuntime<'_> {
            LocalActiveRuntime {
                engine: self.accepted.accepted_engine(),
                projection: self.sqlite(),
            }
        }

        fn enrollment_binding(&self) -> EnrollmentBindingV1 {
            let accepted = self.accepted.binding();
            let storage = accepted.storage_binding();
            EnrollmentBindingV1::new(
                accepted.workspace_id(),
                accepted.lineage_digest(),
                self.verified.catalog_document_id(),
                storage.endpoint.endpoint_id(),
                storage.endpoint.device_id(),
                accepted.graph_resource(),
                storage.receipt_store_id,
                self.archive_resource,
                self.graph.graph_text_scope_binding().unwrap(),
            )
            .unwrap()
        }

        fn enrollment_root(&self, label: &str) -> EnrollmentApplicationRoot {
            enrollment_application_root_for_test(
                &self.root.path().join(format!("enrollment-{label}")),
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

        fn take_bootstrap(&mut self) -> InactiveBootstrapRuntimeSession {
            self.bootstrap.take().unwrap()
        }

        fn baseline(
            &self,
            graph: &Graph,
            label: &str,
            existing: bool,
        ) -> ReconciliationBaseline {
            let runtime = ApplicationRuntimeRoot::open_for_test(
                &self.root.path().join(format!("baseline-runtime-{label}")),
            )
            .unwrap();
            let trusted =
                TrustedPrivateApplicationRuntimeRoot::from_application_runtime_root(&runtime);
            let binding = ReconciliationBaselineBinding::new(
                self.workspace,
                self.receipts.endpoint_binding().unwrap().endpoint_id(),
                graph.canonical_resource_id().unwrap(),
                graph.graph_text_scope_binding().unwrap(),
            )
            .unwrap();
            if existing {
                ReconciliationBaseline::open_existing(&trusted, binding).unwrap()
            } else {
                ReconciliationBaseline::create_fresh(&trusted, binding).unwrap()
            }
        }

        fn manifest_count(&self) -> usize {
            ObjectStore::open(&self.archive_root, self.workspace)
                .unwrap()
                .committed_manifests()
                .unwrap()
                .len()
        }
    }

    struct PromotedPaths {
        runtime_root: ApplicationRuntimeRoot,
        database_path: PathBuf,
    }

    impl PromotedPaths {
        fn new(fixture: &Fixture, label: &str) -> Self {
            Self {
                runtime_root: ApplicationRuntimeRoot::open_for_test(
                    &fixture.root.path().join(format!("promoted-runtime-{label}")),
                )
                .unwrap(),
                database_path: fixture.root.path().join(format!("promoted-{label}.sqlite")),
            }
        }

        fn open<'a>(
            &'a self,
            fixture: &'a Fixture,
            graph: &'a Graph,
        ) -> PromotedRuntimeOpen<'a> {
            PromotedRuntimeOpen {
                graph,
                receipts: &fixture.receipts,
                archive_root: &fixture.archive_root,
                database_path: &self.database_path,
                application_runtime_root: &self.runtime_root,
            }
        }
    }

    fn promote(
        fixture: &mut Fixture,
        enrollment_root: &EnrollmentApplicationRoot,
        session: SessionId,
        paths: &PromotedPaths,
    ) -> (LocalActiveAuthority, PromotedLocalRuntime) {
        let authority = activate_verified_local(
            enrollment_root,
            fixture.compose(enrollment_root),
            session,
            &fixture.proofs(),
            &fixture.inactive_runtime(),
        )
        .unwrap();
        let sealed = seal_local_runtime_promotion(
            &authority,
            &fixture.proofs(),
            &fixture.inactive_runtime(),
        )
        .unwrap();
        let bootstrap = fixture.take_bootstrap();
        let runtime = bootstrap
            .promote(sealed, &authority, &paths.open(fixture, &fixture.graph))
            .map_err(|refusal| refusal.into_parts().1)
            .unwrap();
        (authority, runtime)
    }

    fn promoted_safe_reopen(
        fixture: &mut Fixture,
        enrollment_root: &EnrollmentApplicationRoot,
        paths: &PromotedPaths,
    ) -> (LocalActiveAuthority, PromotedLocalRuntime) {
        let first = SessionId::new();
        {
            let (mut authority, mut runtime) =
                promote(fixture, enrollment_root, first, paths);
            runtime
                .quiesce_and_mark_safe_without_watcher_dependency_for_test(
                    &mut authority,
                    &fixture.graph,
                )
                .unwrap();
        }
        let second = SessionId::new();
        let reopened = reopen_promoted_local_runtime(
            enrollment_root,
            &fixture.enrollment_binding(),
            second,
            &paths.open(fixture, &fixture.graph),
        )
        .unwrap();
        assert_eq!(reopened.1.recovery(), RuntimeRecoveryState::AdoptedSafeHandoff);
        assert_eq!(
            reopened.1.automatic_external_import(),
            ExternalImportAdmission::Allowed
        );
        reopened
    }

    fn drive_terminal(
        owner: &mut ExactExternalFeedOwner,
        graph: &Graph,
        receipts: &ProjectionReceiptStore,
        clock: &mut u64,
    ) -> ExactExternalFeedDrain {
        for _ in 0..16 {
            *clock += 1;
            let result = owner.drain_one(
                graph,
                receipts,
                BaselineTimestamp::from_millis(*clock).unwrap(),
            );
            match result {
                ExactExternalFeedDrain::Recovering | ExactExternalFeedDrain::RetryFull => {}
                terminal => return terminal,
            }
        }
        panic!("exact external feed did not reach a bounded terminal actor result");
    }

    fn assert_admitted(result: ExactExternalFeedDrain) {
        assert!(
            matches!(
                result,
                ExactExternalFeedDrain::AdmittedNoop { .. }
                    | ExactExternalFeedDrain::AdmittedComplete { .. }
            ),
            "unexpected exact feed result: {result:?}"
        );
    }

    fn configured_fixture(label: &str) -> Fixture {
        Fixture::new(
            label,
            Some(
                b"{:pages-directory \"content/nested pages\" :journals-directory \"diary/\xE6\x97\xA5\xE8\xA8\x98\"}\n",
            ),
            [
                (
                    "content/nested pages/deep/Caf\u{e9} note.MD".to_owned(),
                    b"- markdown original\r\n\t- nested\r\n".to_vec(),
                ),
                (
                    "diary/\u{65e5}\u{8a18}/journal space.ORG".to_owned(),
                    b"#+title: Journal\r\n* org original\r\n".to_vec(),
                ),
                (
                    "content/nested pages/rename old.org".to_owned(),
                    b"#+title: Rename\r\n* old\r\n".to_vec(),
                ),
            ],
        )
    }

    #[test]
    fn exact_markdown_org_delete_and_both_rename_orders_admit_once_and_ack_terminally() {
        for (label, rename_order) in [
            ("rename-old-first", [0_usize, 1_usize]),
            ("rename-new-first", [1_usize, 0_usize]),
        ] {
            let mut fixture = configured_fixture(label);
            let enrollment = fixture.enrollment_root(label);
            let paths = PromotedPaths::new(&fixture, label);
            let (authority, runtime) =
                promoted_safe_reopen(&mut fixture, &enrollment, &paths);
            let baseline = fixture.baseline(&fixture.graph, label, false);
            let mut owner = ExactExternalFeedOwner::open(
                &fixture.graph,
                &fixture.receipts,
                authority,
                runtime,
                baseline,
            )
            .unwrap();
            let mut clock = 0;
            assert_admitted(drive_terminal(
                &mut owner,
                &fixture.graph,
                &fixture.receipts,
                &mut clock,
            ));

            let markdown = "content/nested pages/deep/Caf\u{e9} note.MD";
            let org = "diary/\u{65e5}\u{8a18}/journal space.ORG";
            let old = "content/nested pages/rename old.org";
            let new = "content/nested pages/deeper/renamed \u{65e5}.ORG";
            fs::write(
                fixture.graph_root.join(markdown),
                b"- markdown exact edit\r\n\t- nested Unicode\r\n",
            )
            .unwrap();
            fs::write(
                fixture.graph_root.join(org),
                b"#+title: Journal\r\n* org exact edit\r\n",
            )
            .unwrap();
            fs::create_dir_all(fixture.graph_root.join("content/nested pages/deeper")).unwrap();
            fs::rename(fixture.graph_root.join(old), fixture.graph_root.join(new)).unwrap();
            let rename = [
                WatcherObservation::ManagedPath(ManagedPath::parse(old).unwrap()),
                WatcherObservation::ManagedPath(ManagedPath::parse(new).unwrap()),
            ];
            let observations = [
                WatcherObservation::ManagedPath(ManagedPath::parse(markdown).unwrap()),
                WatcherObservation::ManagedPath(ManagedPath::parse(org).unwrap()),
                rename[rename_order[0]].clone(),
                rename[rename_order[1]].clone(),
            ];
            let before = fixture.manifest_count();
            owner
                .observe(&fixture.graph, owner.binding, observations)
                .unwrap();
            let result =
                drive_terminal(&mut owner, &fixture.graph, &fixture.receipts, &mut clock);
            assert!(matches!(
                result,
                ExactExternalFeedDrain::AdmittedComplete { .. }
            ));
            assert_eq!(fixture.manifest_count(), before + 1);
            assert_eq!(
                fs::read(fixture.graph_root.join(markdown)).unwrap(),
                b"- markdown exact edit\r\n\t- nested Unicode\r\n"
            );
            assert_eq!(
                fs::read(fixture.graph_root.join(org)).unwrap(),
                b"#+title: Journal\r\n* org exact edit\r\n"
            );
            assert!(!fixture.graph_root.join(old).exists());
            assert_eq!(
                fs::read(fixture.graph_root.join(new)).unwrap(),
                b"#+title: Rename\r\n* old\r\n"
            );

            fs::remove_file(fixture.graph_root.join(org)).unwrap();
            let before_delete = fixture.manifest_count();
            owner
                .observe(
                    &fixture.graph,
                    owner.binding,
                    [WatcherObservation::ManagedPath(
                        ManagedPath::parse(org).unwrap(),
                    )],
                )
                .unwrap();
            assert!(matches!(
                drive_terminal(&mut owner, &fixture.graph, &fixture.receipts, &mut clock),
                ExactExternalFeedDrain::AdmittedComplete { .. }
            ));
            assert_eq!(fixture.manifest_count(), before_delete + 1);
            assert!(!fixture.graph_root.join(org).exists());
            let status = owner.queue.status();
            assert_eq!(status.acknowledged, status.latest_enqueue);
            assert!(!status.pending);
        }
    }

    #[test]
    fn every_uncertainty_and_both_exact_bounds_collapse_to_one_rebased_full_scan_epoch() {
        let mut fixture = configured_fixture("uncertainty");
        let enrollment = fixture.enrollment_root("uncertainty");
        let paths = PromotedPaths::new(&fixture, "uncertainty");
        let (authority, runtime) = promoted_safe_reopen(&mut fixture, &enrollment, &paths);
        let baseline = fixture.baseline(&fixture.graph, "uncertainty", false);
        let mut owner = ExactExternalFeedOwner::open(
            &fixture.graph,
            &fixture.receipts,
            authority,
            runtime,
            baseline,
        )
        .unwrap();
        let mut clock = 0;
        assert_admitted(drive_terminal(
            &mut owner,
            &fixture.graph,
            &fixture.receipts,
            &mut clock,
        ));

        for observation in [
            WatcherObservation::UnknownPath,
            WatcherObservation::NotifyError,
            WatcherObservation::RescanRequired,
        ] {
            owner
                .observe(&fixture.graph, owner.binding, [observation])
                .unwrap();
            let status = owner.queue.status();
            assert!(status.pending_requires_full_scan);
            assert_admitted(drive_terminal(
                &mut owner,
                &fixture.graph,
                &fixture.receipts,
                &mut clock,
            ));
            let settled = owner.queue.status();
            assert_eq!(settled.acknowledged, settled.latest_enqueue);
            assert_eq!(owner.feed_sequence, settled.acknowledged.sequence());
        }

        let count_overflow = (0..=EXACT_FEED_MAXIMUM_PATHS)
            .map(|index| {
                WatcherObservation::ManagedPath(
                    ManagedPath::parse(format!("content/nested pages/count-{index}.md")).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        owner
            .observe(&fixture.graph, owner.binding, count_overflow)
            .unwrap();
        assert!(owner.queue.status().pending_requires_full_scan);
        let count_drain = owner.queue.begin_drain(owner.binding).unwrap().unwrap();
        assert!(count_drain
            .uncertain_reasons()
            .contains(&super::super::watcher_queue::WatcherUncertainReason::PathOverflow));
        owner.queue.abandon_drain(count_drain.epoch()).unwrap();
        assert_admitted(drive_terminal(
            &mut owner,
            &fixture.graph,
            &fixture.receipts,
            &mut clock,
        ));

        let component = "x".repeat(100);
        let byte_overflow = (0..220)
            .map(|index| {
                WatcherObservation::ManagedPath(
                    ManagedPath::parse(format!(
                        "content/nested pages/{component}/{component}/{component}/{index}.md"
                    ))
                    .unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert!(
            byte_overflow
                .iter()
                .map(|observation| match observation {
                    WatcherObservation::ManagedPath(path) => path.as_str().len(),
                    _ => 0,
                })
                .sum::<usize>()
                > EXACT_FEED_MAXIMUM_PATH_BYTES
        );
        owner
            .observe(&fixture.graph, owner.binding, byte_overflow)
            .unwrap();
        assert!(owner.queue.status().pending_requires_full_scan);
        let byte_drain = owner.queue.begin_drain(owner.binding).unwrap().unwrap();
        assert!(byte_drain
            .uncertain_reasons()
            .contains(&super::super::watcher_queue::WatcherUncertainReason::PathOverflow));
        owner.queue.abandon_drain(byte_drain.epoch()).unwrap();
        assert_admitted(drive_terminal(
            &mut owner,
            &fixture.graph,
            &fixture.receipts,
            &mut clock,
        ));
        let settled = owner.queue.status();
        assert_eq!(settled.acknowledged, settled.latest_enqueue);
        assert_eq!(owner.feed_sequence, settled.acknowledged.sequence());
    }

    #[test]
    fn recovery_gate_retains_the_forced_scan_until_a_fresh_safe_reopen() {
        let mut fixture = configured_fixture("recovery-gate");
        let enrollment = fixture.enrollment_root("recovery-gate");
        let paths = PromotedPaths::new(&fixture, "recovery-gate");
        let (authority, runtime) =
            promote(&mut fixture, &enrollment, SessionId::new(), &paths);
        assert_eq!(runtime.recovery(), RuntimeRecoveryState::FirstPromotion);
        let baseline = fixture.baseline(&fixture.graph, "recovery-gate", false);
        let mut owner = ExactExternalFeedOwner::open(
            &fixture.graph,
            &fixture.receipts,
            authority,
            runtime,
            baseline,
        )
        .unwrap();
        let result = owner.drain_one(
            &fixture.graph,
            &fixture.receipts,
            BaselineTimestamp::from_millis(1).unwrap(),
        );
        assert!(matches!(
            result,
            ExactExternalFeedDrain::RecoveryBlocked(_)
        ));
        let status = owner.queue.status();
        assert_eq!(status.latest_enqueue.sequence(), 1);
        assert_eq!(status.acknowledged.sequence(), 0);
        assert!(status.pending_requires_full_scan);
        assert_eq!(owner.feed_sequence, 0);
    }

    #[test]
    fn crash_after_terminal_reconcile_before_ack_replays_without_duplicate_semantic_admission() {
        let mut fixture = configured_fixture("crash-before-ack");
        let enrollment = fixture.enrollment_root("crash-before-ack");
        let binding = fixture.enrollment_binding();
        let paths = PromotedPaths::new(&fixture, "crash-before-ack");
        let (authority, runtime) = promoted_safe_reopen(&mut fixture, &enrollment, &paths);
        let baseline = fixture.baseline(&fixture.graph, "crash-before-ack", false);
        let mut owner = ExactExternalFeedOwner::open(
            &fixture.graph,
            &fixture.receipts,
            authority,
            runtime,
            baseline,
        )
        .unwrap();
        let mut clock = 0;
        assert_admitted(drive_terminal(
            &mut owner,
            &fixture.graph,
            &fixture.receipts,
            &mut clock,
        ));

        let markdown = "content/nested pages/deep/Caf\u{e9} note.MD";
        fs::write(
            fixture.graph_root.join(markdown),
            b"- admitted exactly once\r\n",
        )
        .unwrap();
        owner
            .observe(
                &fixture.graph,
                owner.binding,
                [WatcherObservation::ManagedPath(
                    ManagedPath::parse(markdown).unwrap(),
                )],
            )
            .unwrap();
        let acknowledged_before = owner.queue.status().acknowledged;
        let manifests_before = fixture.manifest_count();
        EXACT_FEED_AFTER_TERMINAL_BEFORE_ACK_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|| {
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "injected crash after terminal reconcile before ack",
                ))
            }));
        });
        let failed =
            drive_terminal(&mut owner, &fixture.graph, &fixture.receipts, &mut clock);
        assert!(matches!(failed, ExactExternalFeedDrain::Failed(_)));
        assert_eq!(owner.queue.status().acknowledged, acknowledged_before);
        assert!(owner.queue.status().pending);
        assert_eq!(fixture.manifest_count(), manifests_before + 1);
        let committed_after_crash = fixture.manifest_count();

        // Dropping the process-local owner loses its in-memory epoch. A genuine
        // crash takeover remains recovery-gated and cannot import in place.
        drop(owner);
        let reopened_graph = Graph::open(&fixture.graph_root);
        let takeover_session = SessionId::new();
        let (mut takeover_authority, mut takeover_runtime) = take_over_promoted_local_runtime(
            &enrollment,
            &binding,
            takeover_session,
            &paths.open(&fixture, &reopened_graph),
        )
        .unwrap();
        assert!(matches!(
            takeover_runtime.automatic_external_import(),
            ExternalImportAdmission::Blocked(_)
        ));

        // The parallel C4 worker is the production owner of the Safe ordering.
        // Its existing test-only proof boundary lets this packet exercise the
        // later safe reopen without weakening the production recovery gate.
        takeover_runtime
            .quiesce_and_mark_safe_without_watcher_dependency_for_test(
                &mut takeover_authority,
                &reopened_graph,
            )
            .unwrap();
        drop(takeover_runtime);
        drop(takeover_authority);
        let (authority, runtime) = reopen_promoted_local_runtime(
            &enrollment,
            &binding,
            SessionId::new(),
            &paths.open(&fixture, &reopened_graph),
        )
        .unwrap();
        assert_eq!(runtime.recovery(), RuntimeRecoveryState::AdoptedSafeHandoff);
        let baseline = fixture.baseline(&reopened_graph, "crash-before-ack", true);
        let mut reopened = ExactExternalFeedOwner::open(
            &reopened_graph,
            &fixture.receipts,
            authority,
            runtime,
            baseline,
        )
        .unwrap();
        assert!(reopened.queue.status().pending_requires_full_scan);
        assert_admitted(drive_terminal(
            &mut reopened,
            &reopened_graph,
            &fixture.receipts,
            &mut clock,
        ));
        assert_eq!(
            fixture.manifest_count(),
            committed_after_crash,
            "the fresh forced scan must reuse deterministic import/receipt identity"
        );
        let status = reopened.queue.status();
        assert_eq!(status.acknowledged, status.latest_enqueue);
    }

    #[test]
    fn foreign_binding_config_mutation_and_workspace_revocation_never_ack_or_continue() {
        let mut fixture = configured_fixture("terminal-refusal");
        let enrollment = fixture.enrollment_root("terminal-refusal");
        let paths = PromotedPaths::new(&fixture, "terminal-refusal");
        let (authority, runtime) = promoted_safe_reopen(&mut fixture, &enrollment, &paths);
        let baseline = fixture.baseline(&fixture.graph, "terminal-refusal", false);
        let mut owner = ExactExternalFeedOwner::open(
            &fixture.graph,
            &fixture.receipts,
            authority,
            runtime,
            baseline,
        )
        .unwrap();
        let mut clock = 0;
        assert_admitted(drive_terminal(
            &mut owner,
            &fixture.graph,
            &fixture.receipts,
            &mut clock,
        ));
        let clean = owner.queue.status();

        let foreign = ProjectionStorageBinding {
            endpoint: owner.binding.endpoint,
            receipt_store_id: ProjectionReceiptStoreId::from_capability_identity(
                b"test",
                b"foreign-exact-feed-receipt-store",
            ),
        };
        assert_eq!(
            owner.observe(
                &fixture.graph,
                foreign,
                [WatcherObservation::UnknownPath]
            ),
            Err(ExactExternalFeedObserveError::ForeignBinding)
        );
        assert_eq!(owner.queue.status(), clean);

        let markdown = "content/nested pages/deep/Caf\u{e9} note.MD";
        fs::write(fixture.graph_root.join(markdown), b"- revoke me\r\n").unwrap();
        owner
            .observe(
                &fixture.graph,
                owner.binding,
                [WatcherObservation::ManagedPath(
                    ManagedPath::parse(markdown).unwrap(),
                )],
            )
            .unwrap();
        let owned_drain = owner.queue.begin_drain(owner.binding).unwrap().unwrap();
        assert!(matches!(
            owner.queue.begin_drain(owner.binding),
            Err(WatcherDrainError::DrainInFlight(epoch)) if epoch == owned_drain.epoch()
        ));
        let foreign_queue = WatcherQueueOwner::new(
            owner.binding,
            WatcherQueueLimits {
                maximum_paths: EXACT_FEED_MAXIMUM_PATHS,
                maximum_path_bytes: EXACT_FEED_MAXIMUM_PATH_BYTES,
                maximum_uncertain_reasons: 8,
            },
        );
        foreign_queue
            .handle()
            .enqueue(owner.binding, [WatcherObservation::UnknownPath])
            .unwrap();
        let foreign_epoch = foreign_queue
            .begin_drain(owner.binding)
            .unwrap()
            .unwrap()
            .epoch();
        assert!(matches!(
            owner.queue.acknowledge_drain(foreign_epoch),
            Err(WatcherSettlementError::StaleOrForeignEpoch { in_flight })
                if in_flight == owned_drain.epoch()
        ));
        owner.queue.abandon_drain(owned_drain.epoch()).unwrap();
        let owed = owner.queue.status();
        let lease_path = fixture
            .archive_root
            .join(".tine-runtime")
            .join("sqlite-workspaces")
            .join(fixture.workspace.to_string())
            .join("sqlite-applier.lock");
        let incoming = lease_path.with_extension("lock.incoming");
        fs::write(&incoming, b"").unwrap();
        fs::rename(&incoming, &lease_path).unwrap();

        let revoked = owner.drain_one(
            &fixture.graph,
            &fixture.receipts,
            BaselineTimestamp::from_millis(clock + 1).unwrap(),
        );
        assert!(matches!(
            revoked,
            ExactExternalFeedDrain::Terminal(
                ExactExternalFeedTerminal::WorkspaceAuthorityRevoked(_)
            )
        ));
        let after = owner.queue.status();
        assert_eq!(after.acknowledged, owed.acknowledged);
        assert!(after.pending);
        assert!(owner.terminal().is_some());
        assert!(matches!(
            owner.drain_one(
                &fixture.graph,
                &fixture.receipts,
                BaselineTimestamp::from_millis(clock + 2).unwrap(),
            ),
            ExactExternalFeedDrain::Terminal(_)
        ));
        assert_eq!(
            owner.observe(
                &fixture.graph,
                owner.binding,
                [WatcherObservation::RescanRequired]
            ),
            Err(ExactExternalFeedObserveError::Terminal)
        );
    }

    #[test]
    fn configuration_observation_is_terminal_and_requires_a_fresh_graph_owner() {
        let mut fixture = configured_fixture("config-mutation");
        let enrollment = fixture.enrollment_root("config-mutation");
        let paths = PromotedPaths::new(&fixture, "config-mutation");
        let (authority, runtime) = promoted_safe_reopen(&mut fixture, &enrollment, &paths);
        let baseline = fixture.baseline(&fixture.graph, "config-mutation", false);
        let mut owner = ExactExternalFeedOwner::open(
            &fixture.graph,
            &fixture.receipts,
            authority,
            runtime,
            baseline,
        )
        .unwrap();
        assert_eq!(
            owner.observe(
                &fixture.graph,
                owner.binding,
                [WatcherObservation::ManagedPath(
                    ManagedPath::parse("logseq/config.edn").unwrap(),
                )],
            ),
            Err(ExactExternalFeedObserveError::Terminal)
        );
        assert!(matches!(
            owner.terminal(),
            Some(ExactExternalFeedTerminal::GraphFeed(_))
        ));
        assert_eq!(owner.queue.status().acknowledged.sequence(), 0);
    }

    // Keep a structural receipt near the runtime tests: the production owner
    // owns every move-only capability directly and exposes no cloneable drain,
    // authority, runtime, queue-owner, or lease accessor. If a future edit adds
    // `#[derive(Clone)]`, this explicit field census is where review starts.
    #[test]
    fn owner_is_one_move_only_capability_bundle() {
        trait AmbiguousIfClone<Marker> {
            fn assert_not_clone() {}
        }
        impl<T: ?Sized> AmbiguousIfClone<()> for T {}
        impl<T: ?Sized + Clone> AmbiguousIfClone<u8> for T {}

        <ExactExternalFeedOwner as AmbiguousIfClone<_>>::assert_not_clone();
        assert!(std::mem::needs_drop::<ExactExternalFeedOwner>());
    }
}
