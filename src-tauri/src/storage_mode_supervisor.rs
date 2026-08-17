use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;
use tauri::Emitter;

pub(crate) const STORAGE_TRANSITION_EVENT: &str = "storage-transition";

pub(crate) type StorageOperationId = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StableStorageMode {
    Direct,
    Managed,
    Unbound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StorageTransitionKind {
    Lookup,
    OpenDirect,
    OpenManaged,
    ActivateManaged,
    JoinManaged,
    ReturnGracefully,
    ReturnEmergency,
}

impl StorageTransitionKind {
    fn stable_mode(self) -> Option<StableStorageMode> {
        match self {
            Self::Lookup => None,
            Self::OpenDirect | Self::ReturnGracefully | Self::ReturnEmergency => {
                Some(StableStorageMode::Direct)
            }
            Self::OpenManaged | Self::ActivateManaged | Self::JoinManaged => {
                Some(StableStorageMode::Managed)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StorageTransitionPhase {
    Requested,
    WaitingForTransition,
    LookingUpSelection,
    ValidatingTarget,
    OpeningDirect,
    OpeningManaged,
    ActivatingManaged,
    JoiningManaged,
    DrainingManaged,
    ConfirmingProjection,
    QuarantiningManagedSelection,
    PublishingDirect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StorageTransitionOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Superseded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StorageTransitionOperation {
    pub(crate) operation_id: StorageOperationId,
    pub(crate) window: String,
    pub(crate) canonical_root: Option<PathBuf>,
    pub(crate) kind: StorageTransitionKind,
    pub(crate) phase: StorageTransitionPhase,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageTransitionEvent {
    pub(crate) operation_id: StorageOperationId,
    pub(crate) window: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) canonical_root: Option<PathBuf>,
    pub(crate) kind: StorageTransitionKind,
    pub(crate) phase: StorageTransitionPhase,
    pub(crate) elapsed_ms: u64,
    pub(crate) terminal: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<StorageTransitionOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome_code: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BegunStorageTransition {
    pub(crate) operation: StorageTransitionOperation,
    pub(crate) superseded: Option<StorageTransitionEvent>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StorageSupervisorError {
    StaleOperation,
    IllegalPhase {
        kind: StorageTransitionKind,
        from: StorageTransitionPhase,
        to: StorageTransitionPhase,
    },
    EmergencyReturnOwnsTransition,
    RootBusy,
    OperationIdExhausted,
    AlreadyTerminal,
    MismatchedStableMode,
}

#[derive(Clone, Debug)]
struct ActiveStorageTransition {
    operation: StorageTransitionOperation,
    started_ms: u64,
}

#[derive(Debug)]
pub(crate) struct StorageSupervisorModel {
    next_operation_id: StorageOperationId,
    active_by_window: HashMap<String, ActiveStorageTransition>,
    active_root_owner: HashMap<PathBuf, String>,
    terminal_operations: HashSet<StorageOperationId>,
    stable_modes: HashMap<PathBuf, StableStorageMode>,
    selected_root_by_window: HashMap<String, PathBuf>,
}

impl Default for StorageSupervisorModel {
    fn default() -> Self {
        Self {
            next_operation_id: 1,
            active_by_window: HashMap::new(),
            active_root_owner: HashMap::new(),
            terminal_operations: HashSet::new(),
            stable_modes: HashMap::new(),
            selected_root_by_window: HashMap::new(),
        }
    }
}

impl StorageSupervisorModel {
    pub(crate) fn begin(
        &mut self,
        window: impl Into<String>,
        canonical_root: Option<PathBuf>,
        kind: StorageTransitionKind,
        now_ms: u64,
    ) -> Result<BegunStorageTransition, StorageSupervisorError> {
        let window = window.into();
        if let Some(canonical_root) = canonical_root.as_ref() {
            if let Some(owner) = self.active_root_owner.get(canonical_root) {
                if owner != &window {
                    return Err(StorageSupervisorError::RootBusy);
                }
            }
        }
        if self.active_by_window.get(&window).is_some_and(|active| {
            active.operation.kind == StorageTransitionKind::ReturnEmergency
                && kind != StorageTransitionKind::ReturnEmergency
        }) {
            return Err(StorageSupervisorError::EmergencyReturnOwnsTransition);
        }

        let operation_id = self.next_operation_id;
        let next_operation_id = self
            .next_operation_id
            .checked_add(1)
            .ok_or(StorageSupervisorError::OperationIdExhausted)?;
        let superseded = self.retire_current(&window, now_ms, StorageTransitionOutcome::Superseded);
        self.next_operation_id = next_operation_id;
        let operation = StorageTransitionOperation {
            operation_id,
            window: window.clone(),
            canonical_root: canonical_root.clone(),
            kind,
            phase: StorageTransitionPhase::Requested,
        };
        if let Some(canonical_root) = canonical_root {
            self.active_root_owner
                .insert(canonical_root, window.clone());
        }
        self.active_by_window.insert(
            window,
            ActiveStorageTransition {
                operation: operation.clone(),
                started_ms: now_ms,
            },
        );
        Ok(BegunStorageTransition {
            operation,
            superseded,
        })
    }

    pub(crate) fn bind_root(
        &mut self,
        operation_id: StorageOperationId,
        canonical_root: PathBuf,
    ) -> Result<(), StorageSupervisorError> {
        if self.terminal_operations.contains(&operation_id) {
            return Err(StorageSupervisorError::AlreadyTerminal);
        }
        let (window, previous_root) = {
            let active = self.active_mut(operation_id)?;
            (
                active.operation.window.clone(),
                active.operation.canonical_root.clone(),
            )
        };
        if previous_root.as_ref() == Some(&canonical_root) {
            return Ok(());
        }
        if previous_root.is_some() {
            return Err(StorageSupervisorError::MismatchedStableMode);
        }
        if self
            .active_root_owner
            .get(&canonical_root)
            .is_some_and(|owner| owner != &window)
        {
            return Err(StorageSupervisorError::RootBusy);
        }
        self.active_root_owner
            .insert(canonical_root.clone(), window);
        self.active_mut(operation_id)?.operation.canonical_root = Some(canonical_root);
        Ok(())
    }

    pub(crate) fn advance(
        &mut self,
        operation_id: StorageOperationId,
        phase: StorageTransitionPhase,
        now_ms: u64,
    ) -> Result<StorageTransitionEvent, StorageSupervisorError> {
        if self.terminal_operations.contains(&operation_id) {
            return Err(StorageSupervisorError::AlreadyTerminal);
        }
        let active = self.active_mut(operation_id)?;
        if !legal_phase_transition(active.operation.kind, active.operation.phase, phase) {
            return Err(StorageSupervisorError::IllegalPhase {
                kind: active.operation.kind,
                from: active.operation.phase,
                to: phase,
            });
        }
        active.operation.phase = phase;
        Ok(event(active, now_ms, false, None, None))
    }

    pub(crate) fn finish(
        &mut self,
        operation_id: StorageOperationId,
        outcome: StorageTransitionOutcome,
        stable_mode: Option<StableStorageMode>,
        outcome_code: Option<String>,
        now_ms: u64,
    ) -> Result<StorageTransitionEvent, StorageSupervisorError> {
        if self.terminal_operations.contains(&operation_id) {
            return Err(StorageSupervisorError::AlreadyTerminal);
        }
        let window = self
            .active_by_window
            .iter()
            .find_map(|(window, active)| {
                (active.operation.operation_id == operation_id).then(|| window.clone())
            })
            .ok_or(StorageSupervisorError::StaleOperation)?;
        let active = self.active_by_window.get(&window).unwrap();
        if outcome == StorageTransitionOutcome::Succeeded {
            let expected = active.operation.kind.stable_mode();
            if stable_mode != expected && expected.is_some() {
                return Err(StorageSupervisorError::MismatchedStableMode);
            }
            if stable_mode.is_some() && active.operation.canonical_root.is_none() {
                return Err(StorageSupervisorError::MismatchedStableMode);
            }
        }
        let active = self.active_by_window.remove(&window).unwrap();
        if let Some(root) = active.operation.canonical_root.as_ref() {
            self.active_root_owner.remove(root);
        }

        if outcome == StorageTransitionOutcome::Succeeded {
            if let Some(mode) = stable_mode {
                let root = active.operation.canonical_root.clone().unwrap();
                self.stable_modes.insert(root, mode);
            }
        }
        self.terminal_operations.insert(operation_id);
        Ok(event(&active, now_ms, true, Some(outcome), outcome_code))
    }

    pub(crate) fn current_for_window(&self, window: &str) -> Option<&StorageTransitionOperation> {
        self.active_by_window
            .get(window)
            .map(|active| &active.operation)
    }

    pub(crate) fn select_root(&mut self, window: impl Into<String>, root: PathBuf) {
        self.selected_root_by_window.insert(window.into(), root);
    }

    pub(crate) fn selected_root(&self, window: &str) -> Option<&Path> {
        self.active_by_window
            .get(window)
            .and_then(|active| active.operation.canonical_root.as_deref())
            .or_else(|| {
                self.selected_root_by_window
                    .get(window)
                    .map(PathBuf::as_path)
            })
    }

    pub(crate) fn stable_mode(&self, canonical_root: &Path) -> StableStorageMode {
        self.stable_modes
            .get(canonical_root)
            .copied()
            .unwrap_or(StableStorageMode::Unbound)
    }

    fn active_mut(
        &mut self,
        operation_id: StorageOperationId,
    ) -> Result<&mut ActiveStorageTransition, StorageSupervisorError> {
        self.active_by_window
            .values_mut()
            .find(|active| active.operation.operation_id == operation_id)
            .ok_or(StorageSupervisorError::StaleOperation)
    }

    fn retire_current(
        &mut self,
        window: &str,
        now_ms: u64,
        outcome: StorageTransitionOutcome,
    ) -> Option<StorageTransitionEvent> {
        let active = self.active_by_window.remove(window)?;
        if let Some(root) = active.operation.canonical_root.as_ref() {
            self.active_root_owner.remove(root);
        }
        self.terminal_operations
            .insert(active.operation.operation_id);
        Some(event(&active, now_ms, true, Some(outcome), None))
    }
}

fn event(
    active: &ActiveStorageTransition,
    now_ms: u64,
    terminal: bool,
    outcome: Option<StorageTransitionOutcome>,
    outcome_code: Option<String>,
) -> StorageTransitionEvent {
    StorageTransitionEvent {
        operation_id: active.operation.operation_id,
        window: active.operation.window.clone(),
        canonical_root: active.operation.canonical_root.clone(),
        kind: active.operation.kind,
        phase: active.operation.phase,
        elapsed_ms: now_ms.saturating_sub(active.started_ms),
        terminal,
        outcome,
        outcome_code,
    }
}

fn legal_phase_transition(
    kind: StorageTransitionKind,
    from: StorageTransitionPhase,
    to: StorageTransitionPhase,
) -> bool {
    use StorageTransitionKind as K;
    use StorageTransitionPhase as P;
    matches!(
        (kind, from, to),
        (_, P::Requested, P::WaitingForTransition)
            | (
                K::Lookup,
                P::Requested | P::WaitingForTransition,
                P::LookingUpSelection
            )
            | (
                K::OpenDirect,
                P::Requested | P::WaitingForTransition,
                P::ValidatingTarget
            )
            | (K::OpenDirect, P::ValidatingTarget, P::OpeningDirect)
            | (
                K::OpenManaged,
                P::Requested | P::WaitingForTransition,
                P::ValidatingTarget
            )
            | (K::OpenManaged, P::ValidatingTarget, P::OpeningManaged)
            | (
                K::ActivateManaged,
                P::Requested | P::WaitingForTransition,
                P::ValidatingTarget
            )
            | (
                K::ActivateManaged,
                P::ValidatingTarget,
                P::ActivatingManaged
            )
            | (
                K::JoinManaged,
                P::Requested | P::WaitingForTransition,
                P::ValidatingTarget
            )
            | (K::JoinManaged, P::ValidatingTarget, P::JoiningManaged)
            | (
                K::ReturnGracefully,
                P::Requested | P::WaitingForTransition,
                P::ValidatingTarget
            )
            | (K::ReturnGracefully, P::ValidatingTarget, P::DrainingManaged)
            | (
                K::ReturnGracefully,
                P::DrainingManaged,
                P::ConfirmingProjection
            )
            | (
                K::ReturnGracefully,
                P::ConfirmingProjection,
                P::PublishingDirect
            )
            | (
                K::ReturnEmergency,
                P::Requested | P::WaitingForTransition,
                P::ValidatingTarget
            )
            | (
                K::ReturnEmergency,
                P::ValidatingTarget,
                P::QuarantiningManagedSelection
            )
            | (
                K::ReturnEmergency,
                P::QuarantiningManagedSelection,
                P::PublishingDirect
            )
    )
}

/// The only native owner of workspace storage-transition identity and lanes.
///
/// The tested transition model replaces the app-global lock with root-local
/// lanes. Long work shares a lane only with operations on the same canonical
/// graph root; operation IDs still decide whether final publication is current.
#[derive(Debug)]
pub(crate) struct StorageModeSupervisor {
    root_transitions: Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>,
    /// The short linearization lane shared only by operation start and final
    /// graph-registry publication. Long recovery never holds it. This lets us
    /// release the model mutex before touching the graph registry without
    /// allowing an emergency operation to slip between the currentness check
    /// and publication.
    publication: Mutex<()>,
    model: Mutex<StorageSupervisorModel>,
    clock_origin: Instant,
}

pub(crate) struct StorageTransitionGuard<'a> {
    supervisor: &'a StorageModeSupervisor,
    app: &'a tauri::AppHandle,
    operation_id: StorageOperationId,
    terminal: bool,
}

/// The post-publication half of a serving transition.  It deliberately has no
/// publication method: once a prepared successor has been linearized, a
/// second registry/selector publication is unrepresentable through this API.
pub(crate) struct PublishedStorageTransitionGuard<'a> {
    supervisor: &'a StorageModeSupervisor,
    app: &'a tauri::AppHandle,
    operation_id: StorageOperationId,
    terminal: bool,
}

/// Recovery-to-Direct has a different protocol: it may temporarily withdraw
/// a stopped managed actor while archiving it, then publish Direct Files.  Keep
/// that explicitly separate so its multi-step escape machinery cannot be used
/// by ActivateManaged or JoinManaged.
pub(crate) struct StorageRecoveryTransitionGuard<'a> {
    supervisor: &'a StorageModeSupervisor,
    app: &'a tauri::AppHandle,
    operation_id: StorageOperationId,
    terminal: bool,
}

impl<'a> StorageTransitionGuard<'a> {
    pub(crate) fn advance(&self, phase: StorageTransitionPhase) -> Result<(), String> {
        self.supervisor
            .advance_transition(self.app, self.operation_id, phase)
    }

    pub(crate) fn is_current(&self) -> bool {
        self.supervisor.operation_is_current(self.operation_id)
    }

    pub(crate) fn publish<T>(
        mut self,
        publish: impl FnOnce() -> Result<T, String>,
    ) -> Result<(PublishedStorageTransitionGuard<'a>, T), String> {
        let value = self
            .supervisor
            .commit_if_current(self.operation_id, publish)?;
        self.terminal = true;
        Ok((
            PublishedStorageTransitionGuard {
                supervisor: self.supervisor,
                app: self.app,
                operation_id: self.operation_id,
                terminal: false,
            },
            value,
        ))
    }

    pub(crate) fn succeed(mut self, mode: StableStorageMode) -> Result<(), String> {
        self.supervisor.finish_transition(
            self.app,
            self.operation_id,
            StorageTransitionOutcome::Succeeded,
            Some(mode),
            None,
        )?;
        self.terminal = true;
        Ok(())
    }

    pub(crate) fn fail(mut self, code: impl Into<String>) {
        let _ = self.supervisor.finish_transition(
            self.app,
            self.operation_id,
            StorageTransitionOutcome::Failed,
            None,
            Some(code.into()),
        );
        self.terminal = true;
    }
}

impl PublishedStorageTransitionGuard<'_> {
    pub(crate) fn succeed(mut self, mode: StableStorageMode) -> Result<(), String> {
        self.supervisor.finish_transition(
            self.app,
            self.operation_id,
            StorageTransitionOutcome::Succeeded,
            Some(mode),
            None,
        )?;
        self.terminal = true;
        Ok(())
    }

    pub(crate) fn fail(mut self, code: impl Into<String>) {
        let _ = self.supervisor.finish_transition(
            self.app,
            self.operation_id,
            StorageTransitionOutcome::Failed,
            None,
            Some(code.into()),
        );
        self.terminal = true;
    }
}

impl Drop for PublishedStorageTransitionGuard<'_> {
    fn drop(&mut self) {
        if !self.terminal {
            let _ = self.supervisor.finish_transition(
                self.app,
                self.operation_id,
                StorageTransitionOutcome::Failed,
                None,
                Some("published_operation_abandoned".into()),
            );
        }
    }
}

impl StorageRecoveryTransitionGuard<'_> {
    pub(crate) fn advance(&self, phase: StorageTransitionPhase) -> Result<(), String> {
        self.supervisor
            .advance_transition(self.app, self.operation_id, phase)
    }

    pub(crate) fn is_current(&self) -> bool {
        self.supervisor.operation_is_current(self.operation_id)
    }

    pub(crate) fn commit_recovery_step<T>(
        &self,
        publish: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        self.supervisor
            .commit_if_current(self.operation_id, publish)
    }

    pub(crate) fn succeed(mut self, mode: StableStorageMode) -> Result<(), String> {
        self.supervisor.finish_transition(
            self.app,
            self.operation_id,
            StorageTransitionOutcome::Succeeded,
            Some(mode),
            None,
        )?;
        self.terminal = true;
        Ok(())
    }

    pub(crate) fn fail(mut self, code: impl Into<String>) {
        let _ = self.supervisor.finish_transition(
            self.app,
            self.operation_id,
            StorageTransitionOutcome::Failed,
            None,
            Some(code.into()),
        );
        self.terminal = true;
    }
}

impl Drop for StorageRecoveryTransitionGuard<'_> {
    fn drop(&mut self) {
        if !self.terminal {
            let _ = self.supervisor.finish_transition(
                self.app,
                self.operation_id,
                StorageTransitionOutcome::Failed,
                None,
                Some("recovery_operation_abandoned".into()),
            );
        }
    }
}

impl Drop for StorageTransitionGuard<'_> {
    fn drop(&mut self) {
        if self.terminal {
            return;
        }
        let _ = self.supervisor.finish_transition(
            self.app,
            self.operation_id,
            StorageTransitionOutcome::Failed,
            None,
            Some("operation_abandoned".into()),
        );
    }
}

impl Default for StorageModeSupervisor {
    fn default() -> Self {
        Self {
            root_transitions: Mutex::new(HashMap::new()),
            publication: Mutex::new(()),
            model: Mutex::new(StorageSupervisorModel::default()),
            clock_origin: Instant::now(),
        }
    }
}

impl StorageModeSupervisor {
    pub(crate) fn begin_guard<'a>(
        &'a self,
        app: &'a tauri::AppHandle,
        window: &str,
        canonical_root: PathBuf,
        kind: StorageTransitionKind,
    ) -> Result<StorageTransitionGuard<'a>, String> {
        if !matches!(
            kind,
            StorageTransitionKind::ActivateManaged | StorageTransitionKind::JoinManaged
        ) {
            return Err(
                "serving transition guard is limited to managed activation and join".into(),
            );
        }
        let operation_id = self.begin_transition(app, window, Some(canonical_root), kind)?;
        Ok(StorageTransitionGuard {
            supervisor: self,
            app,
            operation_id,
            terminal: false,
        })
    }

    pub(crate) fn begin_recovery_guard<'a>(
        &'a self,
        app: &'a tauri::AppHandle,
        window: &str,
        canonical_root: PathBuf,
        kind: StorageTransitionKind,
    ) -> Result<StorageRecoveryTransitionGuard<'a>, String> {
        if !matches!(kind, StorageTransitionKind::ReturnGracefully) {
            return Err(
                "recovery transition guard is limited to graceful Direct Files return".into(),
            );
        }
        let operation_id = self.begin_transition(app, window, Some(canonical_root), kind)?;
        Ok(StorageRecoveryTransitionGuard {
            supervisor: self,
            app,
            operation_id,
            terminal: false,
        })
    }

    pub(crate) fn transition_lane(&self, canonical_root: &Path) -> Arc<Mutex<()>> {
        let mut gates = self.root_transitions.lock().unwrap();
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(canonical_root).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(Mutex::new(()));
        gates.insert(canonical_root.to_path_buf(), Arc::downgrade(&gate));
        gate
    }

    pub(crate) fn begin_transition(
        &self,
        app: &tauri::AppHandle,
        window: &str,
        canonical_root: Option<PathBuf>,
        kind: StorageTransitionKind,
    ) -> Result<StorageOperationId, String> {
        let _publication = self.publication.lock().unwrap();
        let now_ms = self.now_ms();
        let begun = self
            .model
            .lock()
            .unwrap()
            .begin(window, canonical_root, kind, now_ms)
            .map_err(|error| format!("storage transition refused: {error:?}"))?;
        if let Some(superseded) = begun.superseded {
            self.emit(app, superseded);
        }
        let event = StorageTransitionEvent {
            operation_id: begun.operation.operation_id,
            window: begun.operation.window.clone(),
            canonical_root: begun.operation.canonical_root.clone(),
            kind: begun.operation.kind,
            phase: begun.operation.phase,
            elapsed_ms: 0,
            terminal: false,
            outcome: None,
            outcome_code: None,
        };
        self.emit(app, event);
        Ok(begun.operation.operation_id)
    }

    pub(crate) fn select_window_root(&self, window: &str, root: PathBuf) {
        self.model.lock().unwrap().select_root(window, root);
    }

    /// Validate and supersede in one model lock. The submitted path is a UI
    /// echo, never authority: emergency return is accepted only for the exact
    /// native-selected root of this window.
    pub(crate) fn begin_emergency_return(
        &self,
        app: &tauri::AppHandle,
        window: &str,
        canonical_root: PathBuf,
    ) -> Result<StorageOperationId, String> {
        let _publication = self.publication.lock().unwrap();
        let now_ms = self.now_ms();
        let begun = {
            let mut model = self.model.lock().unwrap();
            if model.selected_root(window) != Some(canonical_root.as_path()) {
                return Err(
                    "This recovery action no longer matches the native-selected graph. Retry graph lookup before returning to Direct Files."
                        .into(),
                );
            }
            model
                .begin(
                    window,
                    Some(canonical_root),
                    StorageTransitionKind::ReturnEmergency,
                    now_ms,
                )
                .map_err(|error| format!("storage transition refused: {error:?}"))?
        };
        if let Some(superseded) = begun.superseded {
            self.emit(app, superseded);
        }
        self.emit(
            app,
            StorageTransitionEvent {
                operation_id: begun.operation.operation_id,
                window: begun.operation.window.clone(),
                canonical_root: begun.operation.canonical_root.clone(),
                kind: begun.operation.kind,
                phase: begun.operation.phase,
                elapsed_ms: 0,
                terminal: false,
                outcome: None,
                outcome_code: None,
            },
        );
        Ok(begun.operation.operation_id)
    }

    pub(crate) fn bind_transition_root(
        &self,
        operation_id: StorageOperationId,
        canonical_root: PathBuf,
    ) -> Result<(), String> {
        self.model
            .lock()
            .unwrap()
            .bind_root(operation_id, canonical_root)
            .map_err(|error| format!("storage transition root refused: {error:?}"))
    }

    pub(crate) fn advance_transition(
        &self,
        app: &tauri::AppHandle,
        operation_id: StorageOperationId,
        phase: StorageTransitionPhase,
    ) -> Result<(), String> {
        let event = self
            .model
            .lock()
            .unwrap()
            .advance(operation_id, phase, self.now_ms())
            .map_err(|error| format!("storage transition progress refused: {error:?}"))?;
        self.emit(app, event);
        Ok(())
    }

    pub(crate) fn finish_transition(
        &self,
        app: &tauri::AppHandle,
        operation_id: StorageOperationId,
        outcome: StorageTransitionOutcome,
        stable_mode: Option<StableStorageMode>,
        outcome_code: Option<String>,
    ) -> Result<(), String> {
        let event = self
            .model
            .lock()
            .unwrap()
            .finish(
                operation_id,
                outcome,
                stable_mode,
                outcome_code,
                self.now_ms(),
            )
            .map_err(|error| format!("storage transition completion refused: {error:?}"))?;
        self.emit(app, event);
        Ok(())
    }

    pub(crate) fn operation_is_current(&self, operation_id: StorageOperationId) -> bool {
        self.model
            .lock()
            .unwrap()
            .active_by_window
            .values()
            .any(|active| active.operation.operation_id == operation_id)
    }

    pub(crate) fn commit_if_current<T>(
        &self,
        operation_id: StorageOperationId,
        publish: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let _publication = self.publication.lock().unwrap();
        {
            let model = self.model.lock().unwrap();
            if !model
                .active_by_window
                .values()
                .any(|active| active.operation.operation_id == operation_id)
            {
                return Err("storage transition was superseded before publication".into());
            }
        }
        publish()
    }

    /// Short same-mode recovery publication. The caller must perform an exact
    /// registry generation comparison inside `publish`; this API cannot change
    /// a storage selector and is intentionally unavailable through the managed
    /// activation/join guard.
    pub(crate) fn publish_managed_maintenance<T>(
        &self,
        publish: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let _publication = self.publication.lock().unwrap();
        publish()
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock_origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn emit(&self, app: &tauri::AppHandle, event: StorageTransitionEvent) {
        crate::debug::diag(format!(
            "storage transition: id={}; window={}; kind={:?}; phase={:?}; elapsed_ms={}; terminal={}; outcome={:?}",
            event.operation_id,
            event.window,
            event.kind,
            event.phase,
            event.elapsed_ms,
            event.terminal,
            event.outcome,
        ));
        let window = event.window.clone();
        let _ = app.emit_to(&window, STORAGE_TRANSITION_EVENT, event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/graph")
    }

    fn run_path(kind: StorageTransitionKind, phases: &[StorageTransitionPhase]) {
        let mut model = StorageSupervisorModel::default();
        let begun = model.begin("main", Some(root()), kind, 10).unwrap();
        let id = begun.operation.operation_id;
        for (index, phase) in phases.iter().copied().enumerate() {
            let update = model.advance(id, phase, 11 + index as u64).unwrap();
            assert_eq!(update.operation_id, id);
            assert!(!update.terminal);
        }
        let terminal = model
            .finish(
                id,
                StorageTransitionOutcome::Succeeded,
                kind.stable_mode(),
                None,
                100,
            )
            .unwrap();
        assert!(terminal.terminal);
        assert_eq!(terminal.outcome, Some(StorageTransitionOutcome::Succeeded));
        assert_eq!(
            model.stable_mode(&root()),
            kind.stable_mode().unwrap_or(StableStorageMode::Unbound)
        );
        assert_eq!(
            model.finish(
                id,
                StorageTransitionOutcome::Succeeded,
                kind.stable_mode(),
                None,
                101
            ),
            Err(StorageSupervisorError::AlreadyTerminal)
        );
    }

    #[test]
    fn every_transition_kind_has_one_legal_terminal_path() {
        use StorageTransitionKind as K;
        use StorageTransitionPhase as P;
        for (kind, phases) in [
            (K::Lookup, vec![P::LookingUpSelection]),
            (K::OpenDirect, vec![P::ValidatingTarget, P::OpeningDirect]),
            (K::OpenManaged, vec![P::ValidatingTarget, P::OpeningManaged]),
            (
                K::ActivateManaged,
                vec![P::ValidatingTarget, P::ActivatingManaged],
            ),
            (K::JoinManaged, vec![P::ValidatingTarget, P::JoiningManaged]),
            (
                K::ReturnGracefully,
                vec![
                    P::ValidatingTarget,
                    P::DrainingManaged,
                    P::ConfirmingProjection,
                    P::PublishingDirect,
                ],
            ),
            (
                K::ReturnEmergency,
                vec![
                    P::ValidatingTarget,
                    P::QuarantiningManagedSelection,
                    P::PublishingDirect,
                ],
            ),
        ] {
            run_path(kind, &phases);
        }
    }

    #[test]
    fn stale_results_cannot_advance_finish_or_change_mode() {
        let mut model = StorageSupervisorModel::default();
        let old = model
            .begin("main", Some(root()), StorageTransitionKind::OpenManaged, 0)
            .unwrap();
        let newer = model
            .begin(
                "main",
                Some(PathBuf::from("/other")),
                StorageTransitionKind::OpenDirect,
                1,
            )
            .unwrap();
        assert_eq!(
            newer.superseded.unwrap().operation_id,
            old.operation.operation_id
        );
        assert_eq!(
            model.advance(
                old.operation.operation_id,
                StorageTransitionPhase::OpeningManaged,
                2
            ),
            Err(StorageSupervisorError::AlreadyTerminal)
        );
        assert_eq!(model.stable_mode(&root()), StableStorageMode::Unbound);
        model
            .finish(
                newer.operation.operation_id,
                StorageTransitionOutcome::Succeeded,
                Some(StableStorageMode::Direct),
                None,
                3,
            )
            .unwrap();
        assert_eq!(
            model.stable_mode(Path::new("/other")),
            StableStorageMode::Direct
        );
    }

    #[test]
    fn emergency_return_supersedes_managed_work_and_cannot_be_displaced() {
        let mut model = StorageSupervisorModel::default();
        let managed = model
            .begin("main", Some(root()), StorageTransitionKind::OpenManaged, 0)
            .unwrap();
        let emergency = model
            .begin(
                "main",
                Some(root()),
                StorageTransitionKind::ReturnEmergency,
                1,
            )
            .unwrap();
        assert_eq!(
            emergency.superseded.unwrap().operation_id,
            managed.operation.operation_id
        );
        assert_eq!(
            model.begin("main", Some(root()), StorageTransitionKind::OpenManaged, 2),
            Err(StorageSupervisorError::EmergencyReturnOwnsTransition)
        );
        model
            .advance(
                emergency.operation.operation_id,
                StorageTransitionPhase::ValidatingTarget,
                3,
            )
            .unwrap();
        model
            .advance(
                emergency.operation.operation_id,
                StorageTransitionPhase::QuarantiningManagedSelection,
                4,
            )
            .unwrap();
        model
            .advance(
                emergency.operation.operation_id,
                StorageTransitionPhase::PublishingDirect,
                5,
            )
            .unwrap();
        model
            .finish(
                emergency.operation.operation_id,
                StorageTransitionOutcome::Succeeded,
                Some(StableStorageMode::Direct),
                None,
                6,
            )
            .unwrap();
        assert_eq!(model.stable_mode(&root()), StableStorageMode::Direct);
    }

    #[test]
    fn another_window_cannot_compete_for_the_same_root() {
        let mut model = StorageSupervisorModel::default();
        model
            .begin("main", Some(root()), StorageTransitionKind::OpenDirect, 0)
            .unwrap();
        assert_eq!(
            model.begin("second", Some(root()), StorageTransitionKind::OpenDirect, 1),
            Err(StorageSupervisorError::RootBusy)
        );
    }

    #[test]
    fn illegal_phase_does_not_mutate_the_operation() {
        let mut model = StorageSupervisorModel::default();
        let begun = model
            .begin(
                "main",
                Some(root()),
                StorageTransitionKind::ReturnEmergency,
                0,
            )
            .unwrap();
        assert!(matches!(
            model.advance(
                begun.operation.operation_id,
                StorageTransitionPhase::OpeningManaged,
                1
            ),
            Err(StorageSupervisorError::IllegalPhase { .. })
        ));
        assert_eq!(
            model.current_for_window("main").unwrap().phase,
            StorageTransitionPhase::Requested
        );
    }

    #[test]
    fn emergency_return_wins_at_every_managed_safe_checkpoint() {
        use StorageTransitionKind as K;
        use StorageTransitionPhase as P;
        for (kind, phases) in [
            (K::OpenManaged, vec![P::ValidatingTarget, P::OpeningManaged]),
            (
                K::ActivateManaged,
                vec![P::ValidatingTarget, P::ActivatingManaged],
            ),
            (K::JoinManaged, vec![P::ValidatingTarget, P::JoiningManaged]),
        ] {
            for cut in 0..=phases.len() {
                let mut model = StorageSupervisorModel::default();
                let managed = model.begin("main", Some(root()), kind, 0).unwrap();
                for phase in phases.iter().take(cut).copied() {
                    model
                        .advance(managed.operation.operation_id, phase, 1)
                        .unwrap();
                }
                let emergency = model
                    .begin("main", Some(root()), K::ReturnEmergency, 2)
                    .unwrap();
                assert_eq!(
                    emergency.superseded.unwrap().outcome,
                    Some(StorageTransitionOutcome::Superseded)
                );
                assert_eq!(
                    model.finish(
                        managed.operation.operation_id,
                        StorageTransitionOutcome::Succeeded,
                        Some(StableStorageMode::Managed),
                        None,
                        3,
                    ),
                    Err(StorageSupervisorError::AlreadyTerminal)
                );
            }
        }
    }

    #[test]
    fn operation_ids_are_native_monotonic_and_terminal_outcomes_are_unique() {
        let mut model = StorageSupervisorModel::default();
        let first = model
            .begin("main", Some(root()), StorageTransitionKind::Lookup, 0)
            .unwrap();
        let first_terminal = model
            .finish(
                first.operation.operation_id,
                StorageTransitionOutcome::Failed,
                None,
                Some("missing_graph".into()),
                1,
            )
            .unwrap();
        let second = model
            .begin("main", Some(root()), StorageTransitionKind::OpenDirect, 2)
            .unwrap();
        assert!(second.operation.operation_id > first.operation.operation_id);
        assert_eq!(
            first_terminal.outcome_code.as_deref(),
            Some("missing_graph")
        );
        assert_eq!(
            model.finish(
                first.operation.operation_id,
                StorageTransitionOutcome::Cancelled,
                None,
                None,
                3,
            ),
            Err(StorageSupervisorError::AlreadyTerminal)
        );
    }

    #[test]
    fn serialized_transition_lock_is_owned_only_by_the_supervisor() {
        let state = include_str!("state.rs");
        let graph = include_str!("graph.rs");
        let runtime = include_str!("sync_runtime.rs");
        assert!(!state.contains("graph_load: Mutex"));
        assert!(!graph.contains(".graph_load.lock()"));
        assert!(!runtime.contains(".graph_load.lock()"));
        assert!(state
            .contains("storage_supervisor: crate::storage_mode_supervisor::StorageModeSupervisor"));
        let global_lock_field = ["transition", "Mutex<()>"].join(": ");
        assert!(!include_str!("storage_mode_supervisor.rs").contains(&global_lock_field));
    }

    #[test]
    fn a_stuck_root_lane_does_not_delay_an_unrelated_graph() {
        use std::sync::mpsc;
        use std::time::Duration;

        let supervisor = Arc::new(StorageModeSupervisor::default());
        let graph_a = supervisor.transition_lane(Path::new("/graph-a"));
        let held_a = graph_a.lock().unwrap();
        let (sent, received) = mpsc::channel();

        let other = Arc::clone(&supervisor);
        let sent_b = sent.clone();
        let graph_b_worker = std::thread::spawn(move || {
            let graph_b = other.transition_lane(Path::new("/graph-b"));
            let _held_b = graph_b.lock().unwrap();
            sent_b.send("b").unwrap();
        });
        assert_eq!(received.recv_timeout(Duration::from_secs(1)).unwrap(), "b");

        let same = Arc::clone(&supervisor);
        let same_root_worker = std::thread::spawn(move || {
            let graph_a = same.transition_lane(Path::new("/graph-a"));
            let _held_a = graph_a.lock().unwrap();
            sent.send("a").unwrap();
        });
        assert!(received.recv_timeout(Duration::from_millis(30)).is_err());
        drop(held_a);
        assert_eq!(received.recv_timeout(Duration::from_secs(1)).unwrap(), "a");
        graph_b_worker.join().unwrap();
        same_root_worker.join().unwrap();
    }

    #[test]
    fn superseded_operations_cannot_enter_the_publication_closure() {
        let supervisor = StorageModeSupervisor::default();
        let mut model = supervisor.model.lock().unwrap();
        let old = model
            .begin("main", Some(root()), StorageTransitionKind::OpenManaged, 0)
            .unwrap();
        model
            .begin(
                "main",
                Some(PathBuf::from("/other")),
                StorageTransitionKind::OpenDirect,
                1,
            )
            .unwrap();
        drop(model);
        let published = std::sync::atomic::AtomicBool::new(false);
        assert!(supervisor
            .commit_if_current(old.operation.operation_id, || {
                published.store(true, std::sync::atomic::Ordering::Release);
                Ok(())
            })
            .is_err());
        assert!(!published.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn serving_transition_publication_is_move_only_and_has_no_second_publish_api() {
        let source = include_str!("storage_mode_supervisor.rs");
        let prepared_start = source
            .find("impl<'a> StorageTransitionGuard<'a>")
            .expect("prepared transition implementation");
        let published_start = source[prepared_start..]
            .find("impl PublishedStorageTransitionGuard")
            .map(|offset| prepared_start + offset)
            .expect("published transition implementation");
        let prepared = &source[prepared_start..published_start];
        assert!(prepared.contains("pub(crate) fn publish<T>(\n        mut self,"));
        assert_eq!(prepared.matches("commit_if_current").count(), 1);
        assert!(!prepared.contains("pub(crate) fn commit"));

        let published_end = source[published_start..]
            .find("impl Drop for PublishedStorageTransitionGuard")
            .map(|offset| published_start + offset)
            .expect("published transition drop implementation");
        let published = &source[published_start..published_end];
        assert!(!published.contains("fn publish"));
        assert!(!published.contains("commit_if_current"));
    }

    #[test]
    fn native_selected_root_is_the_only_emergency_return_target() {
        let mut model = StorageSupervisorModel::default();
        let first = PathBuf::from("/first");
        let second = PathBuf::from("/second");
        model.select_root("main", first.clone());
        assert_eq!(model.selected_root("main"), Some(first.as_path()));

        let open = model
            .begin(
                "main",
                Some(second.clone()),
                StorageTransitionKind::OpenDirect,
                0,
            )
            .unwrap();
        assert_eq!(model.selected_root("main"), Some(second.as_path()));
        model
            .finish(
                open.operation.operation_id,
                StorageTransitionOutcome::Succeeded,
                Some(StableStorageMode::Direct),
                None,
                1,
            )
            .unwrap();
        model.select_root("main", second.clone());
        assert_eq!(model.selected_root("main"), Some(second.as_path()));
    }

    #[test]
    fn frontend_is_a_typed_transition_renderer_not_a_recovery_authority() {
        let frontend = include_str!("../../src/startupRecovery.ts");
        let app = include_str!("../../src/App.tsx");
        let state = include_str!("state.rs");
        let graph = include_str!("graph.rs");
        for forbidden in [
            "nativeAttempt",
            "onStartupProgress",
            "receiveProgress",
            "STARTUP_LOOKUP_WATCHDOG_MS",
        ] {
            assert!(
                !frontend.contains(forbidden),
                "obsolete frontend authority: {forbidden}"
            );
            assert!(
                !app.contains(forbidden),
                "obsolete app authority: {forbidden}"
            );
        }
        assert!(!frontend.contains("setTimeout("));
        assert!(!frontend.contains("confirmColdReturn"));
        assert!(frontend.contains("operationId"));
        assert!(frontend.contains("receiveTransition"));
        let emergency = frontend
            .find("const returnToDirectFiles")
            .expect("startup emergency action exists");
        let emergency = &frontend[emergency..];
        assert!(
            emergency.find("deps.coldReturn(").unwrap() < emergency.find("completeOpen(").unwrap(),
            "emergency return must reach native abandonment before reopening the graph"
        );
        assert!(!state.contains("startup_recovery:"));
        assert!(!graph.contains("startup-progress"));
    }

    #[test]
    fn living_contract_names_the_two_distinct_direct_return_semantics() {
        let contract = include_str!("../../docs/storage-sync-contract.md");
        for required in [
            "`StorageModeSupervisor`",
            "move-only publication guard",
            "graceful return drains a healthy",
            "emergency return is always available",
            "Settings action is always graceful",
            "inactivity timers are not storage authority",
            "does not grant renderer work actor-lane priority",
        ] {
            assert!(
                contract.contains(required),
                "missing storage contract: {required}"
            );
        }
    }
}
