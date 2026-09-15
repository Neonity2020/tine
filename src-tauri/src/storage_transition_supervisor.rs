use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;
use tauri::Emitter;

pub(crate) const STORAGE_TRANSITION_EVENT: &str = "storage-transition";

pub(crate) type StorageOperationId = u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StorageTransitionKind {
    Lookup,
    OpenDirect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StorageTransitionPhase {
    Requested,
    LookingUpSelection,
    ValidatingTarget,
    OpeningDirect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StorageTransitionOutcome {
    Succeeded,
    Failed,
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

/// Content-free snapshot used by the user-created diagnostic report. It omits
/// the canonical root and the actual window label by construction.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageTransitionDiagnostic {
    operation_id: StorageOperationId,
    window_kind: &'static str,
    kind: StorageTransitionKind,
    phase: StorageTransitionPhase,
    elapsed_ms: u64,
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
    RootBusy,
    OperationIdExhausted,
    AlreadyTerminal,
    RootRebound,
    OpenWithoutRoot,
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
}

impl Default for StorageSupervisorModel {
    fn default() -> Self {
        Self {
            next_operation_id: 1,
            active_by_window: HashMap::new(),
            active_root_owner: HashMap::new(),
            terminal_operations: HashSet::new(),
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
            return Err(StorageSupervisorError::RootRebound);
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
            // A graph open publishes a binding for exactly one root, so it
            // cannot succeed before `bind_root` has named that root.
            if active.operation.kind == StorageTransitionKind::OpenDirect
                && active.operation.canonical_root.is_none()
            {
                return Err(StorageSupervisorError::OpenWithoutRoot);
            }
        }
        let active = self.active_by_window.remove(&window).unwrap();
        if let Some(root) = active.operation.canonical_root.as_ref() {
            self.active_root_owner.remove(root);
        }
        self.terminal_operations.insert(operation_id);
        Ok(event(&active, now_ms, true, Some(outcome), outcome_code))
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
        (K::Lookup, P::Requested, P::LookingUpSelection)
            | (K::OpenDirect, P::Requested, P::ValidatingTarget)
            | (K::OpenDirect, P::ValidatingTarget, P::OpeningDirect)
    )
}

/// The only native owner of workspace storage-transition identity and lanes.
///
/// The tested transition model replaces the app-global lock with root-local
/// lanes. Long work shares a lane only with operations on the same canonical
/// graph root; operation IDs still decide whether final publication is current.
/// Since the Managed Storage removal (2026-09-15) the only transitions are the
/// selection lookup and the Direct Files open.
#[derive(Debug)]
pub(crate) struct StorageTransitionSupervisor {
    root_transitions: Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>,
    /// The short linearization lane shared only by operation start and final
    /// graph-registry publication. Long work never holds it. This lets us
    /// release the model mutex before touching the graph registry without
    /// allowing a superseding operation to slip between the currentness check
    /// and publication.
    publication: Mutex<()>,
    model: Mutex<StorageSupervisorModel>,
    clock_origin: Instant,
}

impl Default for StorageTransitionSupervisor {
    fn default() -> Self {
        Self {
            root_transitions: Mutex::new(HashMap::new()),
            publication: Mutex::new(()),
            model: Mutex::new(StorageSupervisorModel::default()),
            clock_origin: Instant::now(),
        }
    }
}

impl StorageTransitionSupervisor {
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
    ) -> Result<StorageOperationId, crate::command_error::CommandError> {
        let _publication = self.publication.lock().unwrap();
        let now_ms = self.now_ms();
        let begun = self
            .model
            .lock()
            .unwrap()
            .begin(window, canonical_root, kind, now_ms)
            .map_err(|error| {
                crate::command_error::CommandError::storage_transition(format!(
                    "storage transition refused: {error:?}"
                ))
            })?;
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

    pub(crate) fn bind_transition_root(
        &self,
        operation_id: StorageOperationId,
        canonical_root: PathBuf,
    ) -> Result<(), crate::command_error::CommandError> {
        self.model
            .lock()
            .unwrap()
            .bind_root(operation_id, canonical_root)
            .map_err(|error| {
                crate::command_error::CommandError::storage_transition(format!(
                    "storage transition root refused: {error:?}"
                ))
            })
    }

    pub(crate) fn advance_transition(
        &self,
        app: &tauri::AppHandle,
        operation_id: StorageOperationId,
        phase: StorageTransitionPhase,
    ) -> Result<(), crate::command_error::CommandError> {
        let event = self
            .model
            .lock()
            .unwrap()
            .advance(operation_id, phase, self.now_ms())
            .map_err(|error| {
                crate::command_error::CommandError::storage_transition(format!(
                    "storage transition progress refused: {error:?}"
                ))
            })?;
        self.emit(app, event);
        Ok(())
    }

    pub(crate) fn finish_transition(
        &self,
        app: &tauri::AppHandle,
        operation_id: StorageOperationId,
        outcome: StorageTransitionOutcome,
        outcome_code: Option<String>,
    ) -> Result<(), crate::command_error::CommandError> {
        let event = self
            .model
            .lock()
            .unwrap()
            .finish(operation_id, outcome, outcome_code, self.now_ms())
            .map_err(|error| {
                crate::command_error::CommandError::storage_transition(format!(
                    "storage transition completion refused: {error:?}"
                ))
            })?;
        self.emit(app, event);
        Ok(())
    }

    pub(crate) fn commit_if_current<T>(
        &self,
        operation_id: StorageOperationId,
        publish: impl FnOnce() -> Result<T, crate::command_error::CommandError>,
    ) -> Result<T, crate::command_error::CommandError> {
        let _publication = self.publication.lock().unwrap();
        {
            let model = self.model.lock().unwrap();
            if !model
                .active_by_window
                .values()
                .any(|active| active.operation.operation_id == operation_id)
            {
                return Err(crate::command_error::CommandError::prose(
                    "storage transition was superseded before publication",
                ));
            }
        }
        publish()
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock_origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn emit(&self, app: &tauri::AppHandle, event: StorageTransitionEvent) {
        crate::debug::record_storage_transition(&event);
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

    pub(crate) fn diagnostic_snapshot(&self) -> Vec<StorageTransitionDiagnostic> {
        let now_ms = self.now_ms();
        self.model
            .lock()
            .map(|model| {
                model
                    .active_by_window
                    .iter()
                    .map(|(window, active)| StorageTransitionDiagnostic {
                        operation_id: active.operation.operation_id,
                        window_kind: if window == "main" {
                            "main"
                        } else if window.starts_with("graph-") {
                            "graph"
                        } else {
                            "other"
                        },
                        kind: active.operation.kind,
                        phase: active.operation.phase,
                        elapsed_ms: now_ms.saturating_sub(active.started_ms),
                    })
                    .collect()
            })
            .unwrap_or_default()
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
            .finish(id, StorageTransitionOutcome::Succeeded, None, 100)
            .unwrap();
        assert!(terminal.terminal);
        assert_eq!(terminal.outcome, Some(StorageTransitionOutcome::Succeeded));
        assert_eq!(
            model.finish(id, StorageTransitionOutcome::Succeeded, None, 101),
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
        ] {
            run_path(kind, &phases);
        }
    }

    #[test]
    fn stale_results_cannot_advance_or_finish() {
        let mut model = StorageSupervisorModel::default();
        let old = model
            .begin("main", Some(root()), StorageTransitionKind::OpenDirect, 0)
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
                StorageTransitionPhase::ValidatingTarget,
                2
            ),
            Err(StorageSupervisorError::AlreadyTerminal)
        );
        model
            .finish(
                newer.operation.operation_id,
                StorageTransitionOutcome::Succeeded,
                None,
                3,
            )
            .unwrap();
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
            .begin("main", Some(root()), StorageTransitionKind::Lookup, 0)
            .unwrap();
        assert!(matches!(
            model.advance(
                begun.operation.operation_id,
                StorageTransitionPhase::OpeningDirect,
                1
            ),
            Err(StorageSupervisorError::IllegalPhase { .. })
        ));
        assert_eq!(
            model.active_by_window["main"].operation.phase,
            StorageTransitionPhase::Requested
        );
    }

    #[test]
    fn a_graph_open_succeeds_only_for_the_one_root_it_bound() {
        let mut model = StorageSupervisorModel::default();
        let open = model
            .begin("main", None, StorageTransitionKind::OpenDirect, 0)
            .unwrap();
        let id = open.operation.operation_id;
        assert_eq!(
            model.finish(id, StorageTransitionOutcome::Succeeded, None, 1),
            Err(StorageSupervisorError::OpenWithoutRoot)
        );
        model.bind_root(id, root()).unwrap();
        model.bind_root(id, root()).unwrap();
        assert_eq!(
            model.bind_root(id, PathBuf::from("/other")),
            Err(StorageSupervisorError::RootRebound)
        );
        model
            .finish(id, StorageTransitionOutcome::Succeeded, None, 2)
            .unwrap();
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
                StorageTransitionOutcome::Failed,
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
        assert!(!state.contains("graph_load: Mutex"));
        assert!(!graph.contains(".graph_load.lock()"));
        // Whitespace-insensitive: rustfmt may wrap the field's type.
        let state_tokens: String = state.split_whitespace().collect();
        assert!(state_tokens.contains(
            "storage_supervisor:crate::storage_transition_supervisor::StorageTransitionSupervisor"
        ));
        let global_lock_field = ["transition", "Mutex<()>"].join(": ");
        assert!(!include_str!("storage_transition_supervisor.rs").contains(&global_lock_field));
    }

    #[test]
    fn a_stuck_root_lane_does_not_delay_an_unrelated_graph() {
        use std::sync::mpsc;
        use std::time::Duration;

        let supervisor = Arc::new(StorageTransitionSupervisor::default());
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
        let supervisor = StorageTransitionSupervisor::default();
        let mut model = supervisor.model.lock().unwrap();
        let old = model
            .begin("main", Some(root()), StorageTransitionKind::OpenDirect, 0)
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
        assert!(frontend.contains("operationId"));
        assert!(frontend.contains("receiveTransition"));
        assert!(!state.contains("startup_recovery:"));
        assert!(!graph.contains("startup-progress"));
    }
}
