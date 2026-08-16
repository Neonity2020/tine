use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

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
    pub(crate) canonical_root: PathBuf,
    pub(crate) kind: StorageTransitionKind,
    pub(crate) phase: StorageTransitionPhase,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageTransitionEvent {
    pub(crate) operation_id: StorageOperationId,
    pub(crate) window: String,
    pub(crate) canonical_root: PathBuf,
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
}

impl Default for StorageSupervisorModel {
    fn default() -> Self {
        Self {
            next_operation_id: 1,
            active_by_window: HashMap::new(),
            active_root_owner: HashMap::new(),
            terminal_operations: HashSet::new(),
            stable_modes: HashMap::new(),
        }
    }
}

impl StorageSupervisorModel {
    pub(crate) fn begin(
        &mut self,
        window: impl Into<String>,
        canonical_root: PathBuf,
        kind: StorageTransitionKind,
        now_ms: u64,
    ) -> Result<BegunStorageTransition, StorageSupervisorError> {
        let window = window.into();
        if let Some(owner) = self.active_root_owner.get(&canonical_root) {
            if owner != &window {
                return Err(StorageSupervisorError::RootBusy);
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
        self.active_root_owner
            .insert(canonical_root, window.clone());
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
        let active = self.active_by_window.remove(&window).unwrap();
        self.active_root_owner
            .remove(&active.operation.canonical_root);

        if outcome == StorageTransitionOutcome::Succeeded {
            let expected = active.operation.kind.stable_mode();
            if stable_mode != expected && expected.is_some() {
                self.active_root_owner
                    .insert(active.operation.canonical_root.clone(), window.clone());
                self.active_by_window.insert(window, active);
                return Err(StorageSupervisorError::MismatchedStableMode);
            }
            if let Some(mode) = stable_mode {
                self.stable_modes
                    .insert(active.operation.canonical_root.clone(), mode);
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
        self.active_root_owner
            .remove(&active.operation.canonical_root);
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

/// The only native owner of the serialized workspace storage transition.
///
/// S1 introduces the tested transition model and centralizes the existing lock.
/// Subsequent packets route commands through `model`; until then the lock method
/// is explicitly named as a migration bridge so it cannot become the new API.
#[derive(Debug, Default)]
pub(crate) struct StorageModeSupervisor {
    transition: Mutex<()>,
    pub(crate) model: Mutex<StorageSupervisorModel>,
}

impl StorageModeSupervisor {
    pub(crate) fn legacy_transition_guard(&self) -> MutexGuard<'_, ()> {
        self.transition.lock().unwrap()
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
        let begun = model.begin("main", root(), kind, 10).unwrap();
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
            .begin("main", root(), StorageTransitionKind::OpenManaged, 0)
            .unwrap();
        let newer = model
            .begin(
                "main",
                PathBuf::from("/other"),
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
            .begin("main", root(), StorageTransitionKind::OpenManaged, 0)
            .unwrap();
        let emergency = model
            .begin("main", root(), StorageTransitionKind::ReturnEmergency, 1)
            .unwrap();
        assert_eq!(
            emergency.superseded.unwrap().operation_id,
            managed.operation.operation_id
        );
        assert_eq!(
            model.begin("main", root(), StorageTransitionKind::OpenManaged, 2),
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
            .begin("main", root(), StorageTransitionKind::OpenDirect, 0)
            .unwrap();
        assert_eq!(
            model.begin("second", root(), StorageTransitionKind::OpenDirect, 1),
            Err(StorageSupervisorError::RootBusy)
        );
    }

    #[test]
    fn illegal_phase_does_not_mutate_the_operation() {
        let mut model = StorageSupervisorModel::default();
        let begun = model
            .begin("main", root(), StorageTransitionKind::ReturnEmergency, 0)
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
                let managed = model.begin("main", root(), kind, 0).unwrap();
                for phase in phases.iter().take(cut).copied() {
                    model
                        .advance(managed.operation.operation_id, phase, 1)
                        .unwrap();
                }
                let emergency = model.begin("main", root(), K::ReturnEmergency, 2).unwrap();
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
            .begin("main", root(), StorageTransitionKind::Lookup, 0)
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
            .begin("main", root(), StorageTransitionKind::OpenDirect, 2)
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
    }
}
