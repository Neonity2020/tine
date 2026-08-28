use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::hot_engine::{
    CleanImportProjectionPredecessor, DeferredAbsenceObservation, ProjectionClaimSource,
};
use super::object_store::{
    ensure_directory_nofollow, open_dir_nofollow, read_optional_regular, require_regular_entry,
};
use super::projection_manifest::{validate_projection_object_set, ManifestProjectionTarget};
use super::{
    BatchId, ContentDigest, FrontierV2, ManagedPath, ObjectStore, PageId, PreparedBatch,
    ProjectionIntentId, ShardedHotEngine, StoreError, WorkspaceId,
};

pub(crate) const SWEEP_COALESCENCE_WINDOW: Duration = Duration::from_secs(60);
pub(crate) const SWEEP_TIER3_GRACE: Duration = Duration::from_secs(5 * 60);

const SWEEP_SCHEMA_VERSION: u32 = 1;
const SWEEP_NAMESPACE: &str = "sweeps";
const MAX_SWEEP_OBJECT_BYTES: u64 = 64 * 1024 * 1024;
const SWEEP_VERSION_DIGITS: usize = 20;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SweepTier {
    Tier1,
    Tier2,
    Tier3,
}

impl SweepTier {
    pub(crate) fn classify(absence_count: usize, pages_at_open: usize) -> Self {
        let ten_percent = pages_at_open.saturating_add(9) / 10;
        let tier3_threshold = usize::min(50, ten_percent.max(1));
        if absence_count >= tier3_threshold {
            Self::Tier3
        } else if absence_count >= 4 {
            Self::Tier2
        } else {
            Self::Tier1
        }
    }

    pub(crate) const fn surfaced(self) -> bool {
        matches!(self, Self::Tier2 | Self::Tier3)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SweepAcceptedStateReference {
    pub(crate) page_id: PageId,
    pub(crate) frontier: FrontierV2,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SweepMember {
    pub(crate) path: ManagedPath,
    pub(crate) page_id: PageId,
    pub(crate) deletion_batch_id: Option<BatchId>,
    pub(crate) predecessor_accepted_state: SweepAcceptedStateReference,
    /// Best-effort provenance only. Restore renders from accepted predecessor
    /// state; activation-era pages legitimately have no prior intent object.
    pub(crate) prior_present_intent_id: Option<ProjectionIntentId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SweepActionKind {
    Restore,
    Reapply,
    KeepDeletion,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SweepRestoreCursor {
    pub(crate) chunk_ordinal: u64,
    pub(crate) remaining_operation_watermark: u64,
    pub(crate) nondecreasing_retries: u8,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SweepActionState {
    Started,
    Progress {
        authored_batch_ids: Vec<BatchId>,
        restore_cursor: Option<SweepRestoreCursor>,
    },
    Completed,
    Failed {
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SweepActionRecord {
    pub(crate) action_id: Uuid,
    pub(crate) action: SweepActionKind,
    pub(crate) recorded_at_unix_ms: u64,
    pub(crate) state: SweepActionState,
}

#[derive(Clone, Debug)]
pub(crate) struct SweepRestoreAction {
    pub(crate) action_id: Uuid,
    pub(crate) members: Vec<SweepMember>,
    pub(crate) authored_batch_ids: Vec<BatchId>,
    pub(crate) cursor: Option<SweepRestoreCursor>,
    pub(crate) completed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SweepRecord {
    pub(crate) sweep_id: Uuid,
    pub(crate) opened_at_unix_ms: u64,
    pub(crate) last_observation_at_unix_ms: u64,
    pub(crate) closed_at_unix_ms: Option<u64>,
    pub(crate) pages_at_open: u64,
    pub(crate) tier: SweepTier,
    pub(crate) grace_deadline_unix_ms: Option<u64>,
    pub(crate) disposed_at_unix_ms: Option<u64>,
    pub(crate) members: Vec<SweepMember>,
    pub(crate) actions: Vec<SweepActionRecord>,
}

impl SweepRecord {
    fn is_open(&self) -> bool {
        self.closed_at_unix_ms.is_none() && self.disposed_at_unix_ms.is_none()
    }

    fn barrier_active_at(&self, now_unix_ms: u64) -> bool {
        if self.disposed_at_unix_ms.is_some() {
            return false;
        }
        if self.is_open() {
            return true;
        }
        self.tier == SweepTier::Tier3
            && self
                .grace_deadline_unix_ms
                .is_some_and(|deadline| now_unix_ms < deadline)
    }

    fn deadline_unix_ms(&self) -> Option<u64> {
        if self.is_open() {
            Some(
                self.last_observation_at_unix_ms
                    .saturating_add(duration_millis(SWEEP_COALESCENCE_WINDOW)),
            )
        } else if self.tier == SweepTier::Tier3 && self.disposed_at_unix_ms.is_none() {
            self.grace_deadline_unix_ms
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SweepNotification {
    pub(crate) sweep_id: Uuid,
    pub(crate) tier: SweepTier,
    pub(crate) absence_count: usize,
    pub(crate) pages_at_open: usize,
    pub(crate) opened_at_unix_ms: u64,
    pub(crate) closed_at_unix_ms: Option<u64>,
    pub(crate) grace_deadline_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SweepObject {
    schema_version: u32,
    workspace_id: WorkspaceId,
    version: u64,
    previous_digest: Option<ContentDigest>,
    record: SweepRecord,
}

#[derive(Clone)]
struct SweepChain {
    version: u64,
    digest: ContentDigest,
    record: SweepRecord,
}

#[derive(Clone, Debug)]
struct SweepName {
    sweep_id: Uuid,
    version: u64,
    name: String,
}

#[derive(Debug)]
pub(crate) enum SweepError {
    Store(StoreError),
    Io(std::io::Error),
    Invalid(String),
    Encode(String),
}

impl fmt::Display for SweepError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "sweep store: {error}"),
            Self::Io(error) => write!(formatter, "sweep I/O: {error}"),
            Self::Invalid(error) => write!(formatter, "invalid sweep record: {error}"),
            Self::Encode(error) => write!(formatter, "sweep encoding: {error}"),
        }
    }
}

impl std::error::Error for SweepError {}

impl From<StoreError> for SweepError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<std::io::Error> for SweepError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// The pre-commit seam used by external reconciliation. Implementations must
/// return only after every absence member is durably present in its sweep
/// chain; the caller may then cross the batch commit point.
pub(crate) trait SweepRecorder {
    fn record_prepared_absence_batch(
        &mut self,
        engine: &ShardedHotEngine,
        prepared: &PreparedBatch,
        claim_source: &dyn ProjectionClaimSource,
        page_count_at_open: &mut dyn FnMut() -> Result<usize, SweepError>,
    ) -> Result<Option<Uuid>, SweepError>;
}

#[cfg(test)]
pub(crate) struct NoopSweepRecorder;

#[cfg(test)]
impl SweepRecorder for NoopSweepRecorder {
    fn record_prepared_absence_batch(
        &mut self,
        _engine: &ShardedHotEngine,
        _prepared: &PreparedBatch,
        _claim_source: &dyn ProjectionClaimSource,
        _page_count_at_open: &mut dyn FnMut() -> Result<usize, SweepError>,
    ) -> Result<Option<Uuid>, SweepError> {
        Ok(None)
    }
}

pub(crate) struct SweepManager {
    store: ObjectStore,
    directory: Dir,
    workspace_id: WorkspaceId,
    chains: BTreeMap<Uuid, SweepChain>,
    notifications: Vec<SweepNotification>,
    /// Process-local acknowledgement that the one wake for an expired grace
    /// deadline ran. The record's timestamp remains the authority; this set
    /// only prevents a quiet actor from treating the same elapsed deadline as
    /// runnable forever.
    settled_grace_deadlines: BTreeSet<Uuid>,
}

impl SweepManager {
    /// Open after the workspace lease and archive repair. Matching record names
    /// use a positive grammar; the reserved local-index directory and unrelated
    /// residue never become record authority.
    pub(crate) fn open(
        store: &ObjectStore,
        accepted_batch_ids: &BTreeSet<BatchId>,
    ) -> Result<Self, SweepError> {
        let root = store.private_derived_root_capability()?;
        ensure_directory_nofollow(&root, SWEEP_NAMESPACE)?;
        let directory = open_dir_nofollow(&root, SWEEP_NAMESPACE)?;
        let names = enumerate_names(&directory)?;
        let workspace_id = store.workspace_id();
        let chains = reconstruct_chains(&directory, workspace_id, &names)?;
        let mut manager = Self {
            store: store.duplicate_retained_capability()?,
            directory,
            workspace_id,
            chains,
            notifications: Vec::new(),
            settled_grace_deadlines: BTreeSet::new(),
        };
        manager.reconcile_uncommitted_members(accepted_batch_ids)?;
        manager.process_deadlines_at(now_unix_ms()?)?;
        manager.repeat_resumed_notifications();
        Ok(manager)
    }

    pub(crate) fn publication_barrier_active(&self) -> bool {
        let now = now_unix_ms().unwrap_or(u64::MAX);
        self.chains
            .values()
            .any(|chain| chain.record.barrier_active_at(now))
    }

    pub(crate) fn deadline_remaining(&self) -> Option<Duration> {
        let now = now_unix_ms().ok()?;
        self.chains
            .values()
            .filter(|chain| {
                !self
                    .settled_grace_deadlines
                    .contains(&chain.record.sweep_id)
            })
            .filter_map(|chain| chain.record.deadline_unix_ms())
            .filter(|deadline| *deadline > now)
            .min()
            .map(|deadline| Duration::from_millis(deadline - now))
    }

    pub(crate) fn deadline_due(&self) -> bool {
        let Ok(now) = now_unix_ms() else {
            return false;
        };
        self.chains.values().any(|chain| {
            !self
                .settled_grace_deadlines
                .contains(&chain.record.sweep_id)
                && chain
                    .record
                    .deadline_unix_ms()
                    .is_some_and(|deadline| deadline <= now)
        })
    }

    pub(crate) fn process_deadlines(&mut self) -> Result<bool, SweepError> {
        self.process_deadlines_at(now_unix_ms()?)
    }

    pub(crate) fn notifications(&self) -> &[SweepNotification] {
        &self.notifications
    }

    /// Current durable records that have crossed the user-surfacing boundary.
    /// Tier-1 records remain intentionally quiet; disposed records remain
    /// visible so the application can show the disposition the user chose.
    pub(crate) fn surfaced_records(&self) -> impl Iterator<Item = &SweepRecord> {
        self.chains
            .values()
            .map(|chain| &chain.record)
            .filter(|record| record.tier.surfaced())
    }

    pub(crate) fn record(&self, sweep_id: Uuid) -> Option<&SweepRecord> {
        self.chains.get(&sweep_id).map(|chain| &chain.record)
    }

    #[cfg(test)]
    pub(crate) fn force_open_window_close_for_test(&mut self) -> Result<bool, SweepError> {
        let deadline = self
            .chains
            .values()
            .filter(|chain| chain.record.is_open())
            .filter_map(|chain| chain.record.deadline_unix_ms())
            .max()
            .ok_or_else(|| SweepError::Invalid("no open sweep to close".into()))?;
        self.process_deadlines_at(deadline)
    }

    #[cfg(test)]
    pub(crate) fn force_grace_expiry_for_test(&mut self) -> Result<bool, SweepError> {
        let id = self
            .chains
            .iter()
            .filter(|(_, chain)| chain.record.grace_deadline_unix_ms.is_some())
            .map(|(id, _)| *id)
            .next()
            .ok_or_else(|| SweepError::Invalid("no tier-3 grace to expire".into()))?;
        let now = now_unix_ms()?;
        let mut record = self.chains[&id].record.clone();
        record.grace_deadline_unix_ms = Some(now);
        self.append_record(record)?;
        self.process_deadlines_at(now)
    }

    #[cfg(test)]
    pub(crate) fn record_count_for_test(&self) -> usize {
        self.chains.len()
    }

    #[cfg(test)]
    pub(crate) fn records_for_test(&self) -> Vec<SweepRecord> {
        self.chains
            .values()
            .map(|chain| chain.record.clone())
            .collect()
    }

    #[cfg(test)]
    fn open_empty_sweep_for_test(&mut self, pages_at_open: usize) -> Result<Uuid, SweepError> {
        self.open_or_join_sweep(now_unix_ms()?, &mut || Ok(pages_at_open))
    }

    pub(crate) fn begin_reapply(
        &mut self,
        sweep_id: Uuid,
    ) -> Result<(Uuid, Vec<(PageId, ManagedPath)>), SweepError> {
        if let Some(action_id) = self.pending_reapply_action_for(sweep_id) {
            let pages = self.chains[&sweep_id]
                .record
                .members
                .iter()
                .map(|member| (member.page_id, member.path.clone()))
                .collect();
            return Ok((action_id, pages));
        }
        let action_id = Uuid::new_v4();
        let now = now_unix_ms()?;
        let mut record = self
            .chains
            .get(&sweep_id)
            .ok_or_else(|| SweepError::Invalid(format!("unknown sweep {sweep_id}")))?
            .record
            .clone();
        record.actions.push(SweepActionRecord {
            action_id,
            action: SweepActionKind::Reapply,
            recorded_at_unix_ms: now,
            state: SweepActionState::Started,
        });
        let pages = record
            .members
            .iter()
            .map(|member| (member.page_id, member.path.clone()))
            .collect();
        self.append_record(record)?;
        Ok((action_id, pages))
    }

    pub(crate) fn begin_restore(
        &mut self,
        sweep_id: Uuid,
    ) -> Result<SweepRestoreAction, SweepError> {
        let record = self
            .chains
            .get(&sweep_id)
            .ok_or_else(|| SweepError::Invalid(format!("unknown sweep {sweep_id}")))?
            .record
            .clone();
        if let Some(latest) = record
            .actions
            .iter()
            .rev()
            .find(|action| action.action == SweepActionKind::Restore)
        {
            if !matches!(latest.state, SweepActionState::Failed { .. }) {
                return Ok(restore_action_snapshot(&record, latest.action_id));
            }
        }
        let action_id = Uuid::new_v4();
        let now = now_unix_ms()?;
        let mut record = record;
        record.actions.push(SweepActionRecord {
            action_id,
            action: SweepActionKind::Restore,
            recorded_at_unix_ms: now,
            state: SweepActionState::Started,
        });
        self.append_record(record.clone())?;
        Ok(restore_action_snapshot(&record, action_id))
    }

    pub(crate) fn pending_restore_actions(&self) -> Vec<(Uuid, Uuid)> {
        self.chains
            .iter()
            .filter_map(|(sweep_id, chain)| {
                let mut latest = BTreeMap::<Uuid, (&SweepActionKind, &SweepActionState)>::new();
                for action in &chain.record.actions {
                    latest.insert(action.action_id, (&action.action, &action.state));
                }
                latest.into_iter().find_map(|(action_id, (kind, state))| {
                    (*kind == SweepActionKind::Restore
                        && matches!(
                            state,
                            SweepActionState::Started | SweepActionState::Progress { .. }
                        ))
                    .then_some((*sweep_id, action_id))
                })
            })
            .collect()
    }

    pub(crate) fn record_restore_progress(
        &mut self,
        sweep_id: Uuid,
        action_id: Uuid,
        authored_batch_ids: Vec<BatchId>,
        cursor: SweepRestoreCursor,
    ) -> Result<(), SweepError> {
        let now = now_unix_ms()?;
        let mut record = self
            .chains
            .get(&sweep_id)
            .ok_or_else(|| SweepError::Invalid(format!("unknown sweep {sweep_id}")))?
            .record
            .clone();
        record.actions.push(SweepActionRecord {
            action_id,
            action: SweepActionKind::Restore,
            recorded_at_unix_ms: now,
            state: SweepActionState::Progress {
                authored_batch_ids,
                restore_cursor: Some(cursor),
            },
        });
        self.append_record(record)
    }

    pub(crate) fn finish_restore(
        &mut self,
        sweep_id: Uuid,
        action_id: Uuid,
    ) -> Result<(), SweepError> {
        let now = now_unix_ms()?;
        let mut record = self
            .chains
            .get(&sweep_id)
            .ok_or_else(|| SweepError::Invalid(format!("unknown sweep {sweep_id}")))?
            .record
            .clone();
        record.actions.push(SweepActionRecord {
            action_id,
            action: SweepActionKind::Restore,
            recorded_at_unix_ms: now,
            state: SweepActionState::Completed,
        });
        record.disposed_at_unix_ms = Some(now);
        self.append_record(record)
    }

    pub(crate) fn fail_restore(
        &mut self,
        sweep_id: Uuid,
        action_id: Uuid,
        reason: String,
    ) -> Result<(), SweepError> {
        let now = now_unix_ms()?;
        let mut record = self
            .chains
            .get(&sweep_id)
            .ok_or_else(|| SweepError::Invalid(format!("unknown sweep {sweep_id}")))?
            .record
            .clone();
        record.actions.push(SweepActionRecord {
            action_id,
            action: SweepActionKind::Restore,
            recorded_at_unix_ms: now,
            state: SweepActionState::Failed { reason },
        });
        self.append_record(record)
    }

    pub(crate) fn pending_reapply_actions(&self) -> Vec<(Uuid, Uuid)> {
        self.chains
            .keys()
            .filter_map(|sweep_id| {
                self.pending_reapply_action_for(*sweep_id)
                    .map(|action_id| (*sweep_id, action_id))
            })
            .collect()
    }

    fn pending_reapply_action_for(&self, sweep_id: Uuid) -> Option<Uuid> {
        let record = &self.chains.get(&sweep_id)?.record;
        let mut latest = BTreeMap::<Uuid, (&SweepActionKind, &SweepActionState)>::new();
        for action in &record.actions {
            latest.insert(action.action_id, (&action.action, &action.state));
        }
        latest.into_iter().find_map(|(action_id, (kind, state))| {
            (*kind == SweepActionKind::Reapply
                && matches!(
                    state,
                    SweepActionState::Started | SweepActionState::Progress { .. }
                ))
            .then_some(action_id)
        })
    }

    pub(crate) fn finish_reapply(
        &mut self,
        sweep_id: Uuid,
        action_id: Uuid,
        authored_batch_ids: Vec<BatchId>,
    ) -> Result<(), SweepError> {
        let now = now_unix_ms()?;
        let mut record = self
            .chains
            .get(&sweep_id)
            .ok_or_else(|| SweepError::Invalid(format!("unknown sweep {sweep_id}")))?
            .record
            .clone();
        record.actions.push(SweepActionRecord {
            action_id,
            action: SweepActionKind::Reapply,
            recorded_at_unix_ms: now,
            state: SweepActionState::Progress {
                authored_batch_ids,
                restore_cursor: None,
            },
        });
        record.actions.push(SweepActionRecord {
            action_id,
            action: SweepActionKind::Reapply,
            recorded_at_unix_ms: now,
            state: SweepActionState::Completed,
        });
        record.disposed_at_unix_ms = Some(now);
        self.append_record(record)
    }

    pub(crate) fn fail_reapply(
        &mut self,
        sweep_id: Uuid,
        action_id: Uuid,
        reason: String,
    ) -> Result<(), SweepError> {
        let now = now_unix_ms()?;
        let mut record = self
            .chains
            .get(&sweep_id)
            .ok_or_else(|| SweepError::Invalid(format!("unknown sweep {sweep_id}")))?
            .record
            .clone();
        record.actions.push(SweepActionRecord {
            action_id,
            action: SweepActionKind::Reapply,
            recorded_at_unix_ms: now,
            state: SweepActionState::Failed { reason },
        });
        self.append_record(record)
    }

    pub(crate) fn dispose_keep_deletion(&mut self, sweep_id: Uuid) -> Result<(), SweepError> {
        let now = now_unix_ms()?;
        let mut record = self
            .chains
            .get(&sweep_id)
            .ok_or_else(|| SweepError::Invalid(format!("unknown sweep {sweep_id}")))?
            .record
            .clone();
        if record.disposed_at_unix_ms.is_some() {
            return Ok(());
        }
        record.actions.push(SweepActionRecord {
            action_id: Uuid::new_v4(),
            action: SweepActionKind::KeepDeletion,
            recorded_at_unix_ms: now,
            state: SweepActionState::Completed,
        });
        record.disposed_at_unix_ms = Some(now);
        self.append_record(record)
    }

    pub(crate) fn record_deferred_absences(
        &mut self,
        engine: &ShardedHotEngine,
        claim_source: &dyn ProjectionClaimSource,
        observations: Vec<DeferredAbsenceObservation>,
        page_count_at_open: &mut dyn FnMut() -> Result<usize, SweepError>,
    ) -> Result<Option<Uuid>, SweepError> {
        let mut members = Vec::new();
        for observation in observations {
            let predecessor = engine
                .clean_import_projection_predecessor(
                    &observation.path,
                    Some(observation.page_id),
                    claim_source,
                )
                .map_err(|error| SweepError::Invalid(error.to_string()))?;
            let Some(CleanImportProjectionPredecessor::Present {
                intent: prior_intent,
                ..
            }) = predecessor
            else {
                // The replay observation was superseded before the coalescer
                // turn. The current state wins; a later differs scan handles
                // any still-live absence.
                continue;
            };
            members.push(SweepMember {
                path: observation.path,
                page_id: observation.page_id,
                deletion_batch_id: None,
                predecessor_accepted_state: SweepAcceptedStateReference {
                    page_id: observation.page_id,
                    frontier: prior_intent.frontier().clone(),
                },
                prior_present_intent_id: Some(
                    prior_intent
                        .id()
                        .map_err(|error| SweepError::Invalid(error.to_string()))?,
                ),
            });
        }
        self.record_members(members, page_count_at_open)
    }

    fn record_members(
        &mut self,
        mut members: Vec<SweepMember>,
        page_count_at_open: &mut dyn FnMut() -> Result<usize, SweepError>,
    ) -> Result<Option<Uuid>, SweepError> {
        if members.is_empty() {
            return Ok(None);
        }
        members.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.page_id.cmp(&right.page_id))
        });
        let now = now_unix_ms()?;
        let sweep_id = self.open_or_join_sweep(now, page_count_at_open)?;
        let mut record = self.chains[&sweep_id].record.clone();
        let prior_tier = record.tier;
        record.last_observation_at_unix_ms = now;
        for member in members {
            match record
                .members
                .iter_mut()
                .find(|existing| existing.path == member.path && existing.page_id == member.page_id)
            {
                Some(existing) if existing.deletion_batch_id == member.deletion_batch_id => {}
                Some(existing) if existing.deletion_batch_id.is_none() => *existing = member,
                Some(_) => {}
                None => record.members.push(member),
            }
        }
        record.members.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.page_id.cmp(&right.page_id))
        });
        record.tier = SweepTier::classify(record.members.len(), record.pages_at_open as usize);
        self.append_record(record.clone())?;
        if record.tier.surfaced() && record.tier > prior_tier {
            self.notifications.push(notification(&record));
        }
        Ok(Some(sweep_id))
    }

    fn process_deadlines_at(&mut self, now: u64) -> Result<bool, SweepError> {
        let due = self
            .chains
            .iter()
            .filter_map(|(id, chain)| {
                chain
                    .record
                    .is_open()
                    .then_some((*id, chain.record.deadline_unix_ms()))
            })
            .filter_map(|(id, deadline)| deadline.map(|deadline| (id, deadline)))
            .filter(|(_, deadline)| *deadline <= now)
            .collect::<Vec<_>>();
        let mut changed = false;
        for (id, close_at) in due {
            let mut record = self.chains[&id].record.clone();
            record.closed_at_unix_ms = Some(close_at);
            if record.tier == SweepTier::Tier3 {
                record.grace_deadline_unix_ms =
                    Some(close_at.saturating_add(duration_millis(SWEEP_TIER3_GRACE)));
            }
            self.append_record(record)?;
            let closed = &self.chains[&id].record;
            if closed.tier.surfaced() {
                self.notifications.push(notification(closed));
            }
            changed = true;
        }
        let expired_grace = self
            .chains
            .iter()
            .filter(|(id, chain)| {
                !self.settled_grace_deadlines.contains(id)
                    && !chain.record.is_open()
                    && chain.record.tier == SweepTier::Tier3
                    && chain.record.disposed_at_unix_ms.is_none()
                    && chain
                        .record
                        .grace_deadline_unix_ms
                        .is_some_and(|deadline| deadline <= now)
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in expired_grace {
            self.settled_grace_deadlines.insert(id);
            changed = true;
        }
        Ok(changed)
    }

    fn reconcile_uncommitted_members(
        &mut self,
        accepted_batch_ids: &BTreeSet<BatchId>,
    ) -> Result<(), SweepError> {
        let updates = self
            .chains
            .values()
            .filter_map(|chain| {
                let mut record = chain.record.clone();
                let before = record.members.len();
                record.members.retain(|member| {
                    member
                        .deletion_batch_id
                        .is_none_or(|batch_id| accepted_batch_ids.contains(&batch_id))
                });
                (record.members.len() != before).then_some(record)
            })
            .collect::<Vec<_>>();
        for record in updates {
            self.append_record(record)?;
        }
        Ok(())
    }

    fn repeat_resumed_notifications(&mut self) {
        let now = now_unix_ms().unwrap_or(u64::MAX);
        let resumed = self
            .chains
            .values()
            .filter(|chain| chain.record.tier.surfaced() && chain.record.barrier_active_at(now))
            .map(|chain| notification(&chain.record))
            .collect::<Vec<_>>();
        self.notifications.extend(resumed);
    }

    fn open_or_join_sweep(
        &mut self,
        now: u64,
        page_count_at_open: &mut dyn FnMut() -> Result<usize, SweepError>,
    ) -> Result<Uuid, SweepError> {
        self.process_deadlines_at(now)?;
        if let Some((id, _)) = self
            .chains
            .iter()
            .filter(|(_, chain)| chain.record.is_open())
            .max_by_key(|(_, chain)| chain.record.opened_at_unix_ms)
        {
            return Ok(*id);
        }
        // The denominator is read only when a sweep actually opens, and only
        // AFTER elapsed windows have been processed: a stale open sweep closed
        // in this same turn (suspend/resume outrunning the deadline wake) must
        // not leave the successor with a zero page count, which would collapse
        // the tier-3 threshold to one.
        let pages_at_open = page_count_at_open()?;
        let sweep_id = Uuid::new_v4();
        let record = SweepRecord {
            sweep_id,
            opened_at_unix_ms: now,
            last_observation_at_unix_ms: now,
            closed_at_unix_ms: None,
            pages_at_open: pages_at_open as u64,
            tier: SweepTier::Tier1,
            grace_deadline_unix_ms: None,
            disposed_at_unix_ms: None,
            members: Vec::new(),
            actions: Vec::new(),
        };
        self.append_record(record)?;
        Ok(sweep_id)
    }

    fn append_record(&mut self, record: SweepRecord) -> Result<(), SweepError> {
        let previous = self.chains.get(&record.sweep_id);
        let version = previous.map_or(1, |chain| chain.version.saturating_add(1));
        if version == 0 {
            return Err(SweepError::Invalid("sweep version overflow".into()));
        }
        let object = SweepObject {
            schema_version: SWEEP_SCHEMA_VERSION,
            workspace_id: self.workspace_id,
            version,
            previous_digest: previous.map(|chain| chain.digest),
            record: record.clone(),
        };
        let bytes = encode_object(&object)?;
        let digest = ContentDigest::of(&bytes);
        let name = object_name(record.sweep_id, version);
        self.store.publish_coalesced_private_derived(
            &self.directory,
            &[(name.as_str(), bytes.as_slice(), MAX_SWEEP_OBJECT_BYTES)],
            "sweep record object",
        )?;
        self.chains.insert(
            record.sweep_id,
            SweepChain {
                version,
                digest,
                record,
            },
        );
        Ok(())
    }
}

impl SweepRecorder for SweepManager {
    fn record_prepared_absence_batch(
        &mut self,
        engine: &ShardedHotEngine,
        prepared: &PreparedBatch,
        claim_source: &dyn ProjectionClaimSource,
        page_count_at_open: &mut dyn FnMut() -> Result<usize, SweepError>,
    ) -> Result<Option<Uuid>, SweepError> {
        let projection = validate_projection_object_set(prepared.manifest(), prepared.objects())
            .map_err(|error| SweepError::Invalid(error.to_string()))?;
        let mut members = Vec::new();
        for intent in projection
            .intents()
            .iter()
            .filter(|intent| matches!(intent.target(), ManifestProjectionTarget::Absent))
        {
            let predecessor = engine
                .clean_import_projection_predecessor(
                    intent.path(),
                    Some(intent.page_id()),
                    claim_source,
                )
                .map_err(|error| SweepError::Invalid(error.to_string()))?
                .ok_or_else(|| {
                    SweepError::Invalid(format!(
                        "absence member {} has no accepted predecessor",
                        intent.path()
                    ))
                })?;
            let CleanImportProjectionPredecessor::Present {
                intent: prior_intent,
                ..
            } = predecessor
            else {
                return Err(SweepError::Invalid(format!(
                    "absence member {} has a released predecessor",
                    intent.path()
                )));
            };
            members.push(SweepMember {
                path: intent.path().clone(),
                page_id: intent.page_id(),
                deletion_batch_id: Some(prepared.manifest().batch_id()),
                predecessor_accepted_state: SweepAcceptedStateReference {
                    page_id: intent.page_id(),
                    frontier: prior_intent.frontier().clone(),
                },
                prior_present_intent_id: Some(
                    prior_intent
                        .id()
                        .map_err(|error| SweepError::Invalid(error.to_string()))?,
                ),
            });
        }
        self.record_members(members, page_count_at_open)
    }
}

fn restore_action_snapshot(record: &SweepRecord, action_id: Uuid) -> SweepRestoreAction {
    let mut authored_batch_ids = Vec::new();
    let mut cursor = None;
    let mut completed = false;
    for action in record
        .actions
        .iter()
        .filter(|action| action.action_id == action_id)
    {
        match &action.state {
            SweepActionState::Started => {}
            SweepActionState::Progress {
                authored_batch_ids: batches,
                restore_cursor,
            } => {
                authored_batch_ids = batches.clone();
                cursor = restore_cursor.clone();
            }
            SweepActionState::Completed => completed = true,
            SweepActionState::Failed { .. } => {}
        }
    }
    SweepRestoreAction {
        action_id,
        members: record.members.clone(),
        authored_batch_ids,
        cursor,
        completed,
    }
}

fn notification(record: &SweepRecord) -> SweepNotification {
    SweepNotification {
        sweep_id: record.sweep_id,
        tier: record.tier,
        absence_count: record.members.len(),
        pages_at_open: record.pages_at_open as usize,
        opened_at_unix_ms: record.opened_at_unix_ms,
        closed_at_unix_ms: record.closed_at_unix_ms,
        grace_deadline_unix_ms: record.grace_deadline_unix_ms,
    }
}

fn enumerate_names(directory: &Dir) -> Result<BTreeMap<Uuid, Vec<SweepName>>, SweepError> {
    let mut names = BTreeMap::<Uuid, Vec<SweepName>>::new();
    for entry in directory.entries()? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(parsed) = parse_object_name(&name) else {
            continue;
        };
        require_regular_entry(&entry.file_type()?, &name)?;
        names.entry(parsed.sweep_id).or_default().push(parsed);
    }
    for chain in names.values_mut() {
        chain.sort_by_key(|name| name.version);
        chain.dedup_by_key(|name| name.version);
    }
    Ok(names)
}

fn reconstruct_chains(
    directory: &Dir,
    workspace_id: WorkspaceId,
    names: &BTreeMap<Uuid, Vec<SweepName>>,
) -> Result<BTreeMap<Uuid, SweepChain>, SweepError> {
    let mut chains = BTreeMap::new();
    for (sweep_id, versions) in names {
        let mut previous_digest = None;
        let mut current = None;
        for name in versions {
            let Some(bytes) =
                read_optional_regular(directory, &name.name, MAX_SWEEP_OBJECT_BYTES, None)?
            else {
                break;
            };
            let Ok(object) = decode_bound_object(
                &bytes,
                workspace_id,
                *sweep_id,
                name.version,
                previous_digest,
            ) else {
                // A crash-injected/torn tail cannot invalidate the preceding
                // immutable version. Stop at the last complete chain object.
                break;
            };
            let digest = ContentDigest::of(&bytes);
            current = Some(SweepChain {
                version: name.version,
                digest,
                record: object.record,
            });
            previous_digest = Some(digest);
        }
        if let Some(chain) = current {
            chains.insert(*sweep_id, chain);
        }
    }
    Ok(chains)
}

fn object_name(sweep_id: Uuid, version: u64) -> String {
    format!("{sweep_id}.{version:0SWEEP_VERSION_DIGITS$}")
}

fn parse_object_name(name: &str) -> Result<SweepName, SweepError> {
    let (id, digits) = name
        .rsplit_once('.')
        .ok_or_else(|| SweepError::Invalid("sweep name has no version".into()))?;
    if digits.len() != SWEEP_VERSION_DIGITS || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SweepError::Invalid("non-canonical sweep version".into()));
    }
    let sweep_id = Uuid::parse_str(id)
        .map_err(|error| SweepError::Invalid(format!("invalid sweep id: {error}")))?;
    let version = digits
        .parse::<u64>()
        .map_err(|error| SweepError::Invalid(error.to_string()))?;
    if version == 0 || object_name(sweep_id, version) != name {
        return Err(SweepError::Invalid("non-canonical sweep name".into()));
    }
    Ok(SweepName {
        sweep_id,
        version,
        name: name.to_owned(),
    })
}

fn encode_object(object: &SweepObject) -> Result<Vec<u8>, SweepError> {
    postcard::to_allocvec(object).map_err(|error| SweepError::Encode(error.to_string()))
}

fn decode_bound_object(
    bytes: &[u8],
    workspace_id: WorkspaceId,
    sweep_id: Uuid,
    version: u64,
    previous_digest: Option<ContentDigest>,
) -> Result<SweepObject, SweepError> {
    let object: SweepObject =
        postcard::from_bytes(bytes).map_err(|error| SweepError::Invalid(error.to_string()))?;
    if encode_object(&object)? != bytes
        || object.schema_version != SWEEP_SCHEMA_VERSION
        || object.workspace_id != workspace_id
        || object.version != version
        || object.previous_digest != previous_digest
        || object.record.sweep_id != sweep_id
        || !object
            .record
            .members
            .windows(2)
            .all(|pair| (&pair[0].path, pair[0].page_id) < (&pair[1].path, pair[1].page_id))
    {
        return Err(SweepError::Invalid(
            "sweep object binding or canonical encoding mismatch".into(),
        ));
    }
    Ok(object)
}

fn now_unix_ms() -> Result<u64, SweepError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| SweepError::Invalid(error.to_string()))?;
    u64::try_from(elapsed.as_millis())
        .map_err(|_| SweepError::Invalid("system time millisecond overflow".into()))
}

const fn duration_millis(duration: Duration) -> u64 {
    duration.as_secs().saturating_mul(1_000) + duration.subsec_millis() as u64
}

#[cfg(test)]
pub(crate) fn assert_torn_sweep_tail_recovers_for_oracle() {
    let root = std::env::temp_dir().join(format!("tine-sweep-tail-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let workspace_id = WorkspaceId::from_uuid(Uuid::from_u128(0xc4f001));
    let store = ObjectStore::open(&root, workspace_id).unwrap();
    let mut manager = SweepManager::open(&store, &BTreeSet::new()).unwrap();
    let sweep_id = manager.open_empty_sweep_for_test(100).unwrap();
    drop(manager);

    std::fs::write(
        root.join(SWEEP_NAMESPACE).join(object_name(sweep_id, 2)),
        b"torn",
    )
    .unwrap();
    let reopened = SweepManager::open(&store, &BTreeSet::new()).unwrap();
    assert_eq!(reopened.record_count_for_test(), 1);
    assert_eq!(reopened.record(sweep_id).unwrap().pages_at_open, 100);
    drop(reopened);
    drop(store);
    crate::test_support::remove_dir_all(&root);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_precedence_covers_small_and_large_graph_boundaries() {
        assert_eq!(SweepTier::classify(3, 100), SweepTier::Tier1);
        assert_eq!(SweepTier::classify(4, 100), SweepTier::Tier2);
        assert_eq!(SweepTier::classify(9, 100), SweepTier::Tier2);
        assert_eq!(SweepTier::classify(10, 100), SweepTier::Tier3);
        assert_eq!(SweepTier::classify(49, 10_000), SweepTier::Tier2);
        assert_eq!(SweepTier::classify(50, 10_000), SweepTier::Tier3);
        assert_eq!(SweepTier::classify(1, 3), SweepTier::Tier3);
        assert_eq!(SweepTier::classify(3, 30), SweepTier::Tier3);
    }

    #[test]
    fn sweep_name_grammar_is_positive_and_canonical() {
        let id = Uuid::from_u128(7);
        let name = object_name(id, 42);
        assert_eq!(parse_object_name(&name).unwrap().version, 42);
        for rejected in [
            id.to_string(),
            format!("{id}.42"),
            format!("{id}.00000000000000000000"),
            "local-completion-index-v1".to_owned(),
            format!("{id}.00000000000000000042.extra"),
        ] {
            assert!(parse_object_name(&rejected).is_err(), "accepted {rejected}");
        }
    }

    #[test]
    fn window_and_grace_constants_remain_the_contract_values() {
        assert_eq!(SWEEP_COALESCENCE_WINDOW, Duration::from_secs(60));
        assert_eq!(SWEEP_TIER3_GRACE, Duration::from_secs(300));
    }

    #[test]
    fn coalescence_boundary_is_strictly_less_than_sixty_seconds() {
        let last = 1_000_u64;
        let deadline = last + duration_millis(SWEEP_COALESCENCE_WINDOW);
        assert!(last + 59_999 < deadline);
        assert!(last + 60_000 >= deadline);
        assert!(last + 61_000 >= deadline);
    }

    #[test]
    fn record_chain_ignores_a_torn_highest_version_and_retains_the_last_valid_object() {
        assert_torn_sweep_tail_recovers_for_oracle();
    }

    /// Suspend/resume can deliver a fresh absence observation before the
    /// deadline wake closes an elapsed open sweep. The successor sweep opened
    /// in that same turn must carry the REAL page count: a zero denominator
    /// collapses the tier-3 threshold to one and turns a single external
    /// deletion into a spurious mass-deletion hold.
    #[test]
    fn a_fresh_observation_after_an_elapsed_window_opens_with_the_real_page_count() {
        use crate::oplog::{CrdtPeerCounter, CrdtPeerId, DocumentDependencies, DocumentId};

        let root = std::env::temp_dir().join(format!("tine-sweep-reopen-count-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let workspace_id = WorkspaceId::from_uuid(Uuid::from_u128(0xc4f002));
        let store = ObjectStore::open(&root, workspace_id).unwrap();
        let mut manager = SweepManager::open(&store, &BTreeSet::new()).unwrap();
        let stale = manager.open_empty_sweep_for_test(1_000).unwrap();
        let mut aged = manager.chains[&stale].record.clone();
        aged.opened_at_unix_ms = aged.opened_at_unix_ms.saturating_sub(120_000);
        aged.last_observation_at_unix_ms = aged.last_observation_at_unix_ms.saturating_sub(120_000);
        manager.append_record(aged).unwrap();

        let frontier = FrontierV2::new(vec![DocumentDependencies::new(
            DocumentId::from_uuid(Uuid::from_u128(0xc4f003)),
            vec![CrdtPeerCounter::new(CrdtPeerId::from_u64(3), 1)],
            Vec::new(),
        )
        .unwrap()])
        .unwrap();
        let page_id = PageId::from_uuid(Uuid::from_u128(0xc4f004));
        let member = SweepMember {
            path: ManagedPath::parse("reopen-count.md").unwrap(),
            page_id,
            deletion_batch_id: None,
            predecessor_accepted_state: SweepAcceptedStateReference { page_id, frontier },
            prior_present_intent_id: None,
        };
        let joined = manager
            .record_members(vec![member], &mut || Ok(1_000))
            .unwrap()
            .expect("one member joins a sweep");
        assert_ne!(joined, stale, "the elapsed window closes the stale sweep");
        let record = manager.record(joined).unwrap();
        assert_eq!(record.pages_at_open, 1_000);
        assert_eq!(record.tier, SweepTier::Tier1);
        drop(manager);
        drop(store);
        crate::test_support::remove_dir_all(&root);
    }
}
