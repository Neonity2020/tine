use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::current_action_roots::{CurrentActionRoots, SweepRetentionPin};
use super::hot_engine::{
    CleanImportProjectionPredecessor, DeferredAbsenceObservation, ProjectionClaimSource,
};
use super::object_store::{
    ensure_directory_nofollow, open_dir_nofollow, read_optional_regular, require_regular_entry,
    sync_dir_required,
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
const ROOTS_NAMESPACE: &str = "sweep-action-roots-v1";
const ROOTS_PREFIX: &str = "sweep-action-roots-";
const ROOTS_SUFFIX: &str = ".roots";
const ROOTS_SCHEMA_VERSION: u32 = 1;
const MAX_ROOTS_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

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

/// Accepted-batch membership as a point question.
///
/// The old open built the complete accepted batch-id set purely so that member
/// reconciliation could ask "was this deletion accepted?" a handful of times.
/// That materialized an O(accepted history) collection on every ordinary open
/// to answer a bounded number of point queries, so the seam is now the query
/// itself and the caller decides how to answer it.
pub(crate) trait AcceptedBatchMembership {
    fn is_accepted(&self, batch_id: BatchId) -> Result<bool, SweepError>;
}

impl AcceptedBatchMembership for BTreeSet<BatchId> {
    fn is_accepted(&self, batch_id: BatchId) -> Result<bool, SweepError> {
        Ok(self.contains(&batch_id))
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SweepManagerOpenStats {
    /// Sweep record filenames enumerated. Zero on a healthy open: the active
    /// roots replaced the lifetime directory walk.
    pub(crate) names_observed: usize,
    /// Sweep chain objects decoded, including bounded catch-up probes.
    pub(crate) chain_objects_read: usize,
    /// Chains resident after open. Bounded by unfinished actions and explicit
    /// pending Restore, never by retained sweep history.
    pub(crate) active_chains: usize,
    /// Terminal chains the roots account for without loading them. They stay
    /// point-addressable by sweep id.
    pub(crate) retired_chains: u64,
    /// The roots object was missing or damaged and was rebuilt from retained
    /// sweep records. A named, counted repair, never a refusal (D-3, I-10).
    pub(crate) repaired: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveSweepPin {
    version: u64,
    digest: ContentDigest,
    record: SweepRecord,
}

/// The complete current-action roster for absence sweeps.
///
/// `active` is a complete root, not a delta chain: every sweep that still owes
/// work or awaits an explicit user disposition is present with its exact chain
/// version and digest. Terminal chains are counted and otherwise absent; their
/// records remain on disk and are reachable by exact sweep id, which is what
/// keeps historical Restore available without keeping every old record
/// resident.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SweepActionRootsObject {
    schema_version: u32,
    workspace_id: WorkspaceId,
    generation: u64,
    previous_digest: Option<ContentDigest>,
    active: Vec<ActiveSweepPin>,
    retired_chains: u64,
}

#[derive(Clone, Debug)]
struct RootsName {
    name: String,
    digest: ContentDigest,
}

pub(crate) struct SweepManager {
    store: ObjectStore,
    directory: Dir,
    roots_directory: Dir,
    workspace_id: WorkspaceId,
    chains: BTreeMap<Uuid, SweepChain>,
    roots_generation: u64,
    roots_tail_digest: Option<ContentDigest>,
    roots_names: BTreeMap<u64, RootsName>,
    retired_chains: u64,
    open_stats: SweepManagerOpenStats,
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
        membership: &dyn AcceptedBatchMembership,
    ) -> Result<Self, SweepError> {
        let root = store.private_derived_root_capability()?;
        ensure_directory_nofollow(&root, SWEEP_NAMESPACE)?;
        let directory = open_dir_nofollow(&root, SWEEP_NAMESPACE)?;
        // The active-sweep roots object is derived, but `absence_sweep.rs` is
        // pinned as a durable-authority module by
        // `android_private_directory_durability_is_explicit_at_every_exception`,
        // so its directory takes the strict one-time authority barrier rather
        // than the reconstructible exception.
        ensure_directory_nofollow(&directory, ROOTS_NAMESPACE)?;
        let roots_directory = open_dir_nofollow(&directory, ROOTS_NAMESPACE)?;
        let workspace_id = store.workspace_id();

        let mut stats = SweepManagerOpenStats::default();
        let mut manager = Self {
            store: store.duplicate_retained_capability()?,
            directory,
            roots_directory,
            workspace_id,
            chains: BTreeMap::new(),
            roots_generation: 0,
            roots_tail_digest: None,
            roots_names: BTreeMap::new(),
            retired_chains: 0,
            open_stats: SweepManagerOpenStats::default(),
            notifications: Vec::new(),
            settled_grace_deadlines: BTreeSet::new(),
        };

        let resumed = manager.resume_from_roots(&mut stats).unwrap_or(false);
        if !resumed {
            stats.repaired = true;
            manager.rebuild_roots_from_records(&mut stats)?;
        }
        stats.active_chains = manager.chains.len();
        stats.retired_chains = manager.retired_chains;
        manager.open_stats = stats;

        manager.reconcile_uncommitted_members(membership)?;
        manager.process_deadlines_at(now_unix_ms()?)?;
        manager.repeat_resumed_notifications();
        if manager.open_stats.repaired {
            manager.install_roots()?;
        }
        Ok(manager)
    }

    /// Resume the bounded active roster. Reads one roots object and, for each
    /// active pin, chases forward only as far as a crash could have left that
    /// chain ahead of its roots. No directory enumeration, no terminal chain.
    fn resume_from_roots(&mut self, stats: &mut SweepManagerOpenStats) -> Result<bool, SweepError> {
        let names = enumerate_roots_names(&self.roots_directory)?;
        let Some((&generation, latest)) = names.last_key_value() else {
            return Ok(false);
        };
        let bytes = read_optional_regular(
            &self.roots_directory,
            &latest.name,
            MAX_ROOTS_OBJECT_BYTES,
            None,
        )?
        .ok_or_else(|| SweepError::Invalid("sweep action roots disappeared during open".into()))?;
        if ContentDigest::of(&bytes) != latest.digest {
            return Err(SweepError::Invalid(
                "sweep action roots filename digest mismatch".into(),
            ));
        }
        let object = decode_bound_roots(&bytes, self.workspace_id, generation)?;
        match (
            object.previous_digest,
            names.range(..generation).next_back(),
        ) {
            (None, None) => {}
            (Some(expected), Some((_, previous))) if previous.digest == expected => {}
            _ => {
                return Err(SweepError::Invalid(
                    "sweep action roots chain is torn".into(),
                ))
            }
        }

        for pin in object.active {
            let sweep_id = pin.record.sweep_id;
            let start = SweepChain {
                version: pin.version,
                digest: pin.digest,
                record: pin.record,
            };
            // Chase forward with no cap. Catch-up reads only the versions of
            // the one sweep it was asked about, by exact name, and stops at the
            // first absent or torn version — so a long catch-up is bounded
            // point work, never a reason to reconstruct every chain ever
            // written. Occupancy is not damage (D-5).
            let current = advance_chain(
                &self.directory,
                self.workspace_id,
                sweep_id,
                Some(start),
                u64::MAX,
                stats,
            )?;
            if self.chains.insert(sweep_id, current).is_some() {
                return Err(SweepError::Invalid(
                    "sweep action roots repeat a sweep".into(),
                ));
            }
        }
        self.retired_chains = object.retired_chains;
        self.roots_generation = generation;
        self.roots_tail_digest = Some(ContentDigest::of(&bytes));
        self.roots_names = names;
        Ok(true)
    }

    /// The named repair: one enumeration of retained sweep records, exactly
    /// the work every open used to do unconditionally.
    fn rebuild_roots_from_records(
        &mut self,
        stats: &mut SweepManagerOpenStats,
    ) -> Result<(), SweepError> {
        let names = enumerate_names(&self.directory)?;
        stats.names_observed = names.values().map(Vec::len).sum();
        let chains = reconstruct_chains(&self.directory, self.workspace_id, &names, stats)?;
        let now = now_unix_ms()?;
        let mut retired = 0_u64;
        self.chains = chains
            .into_iter()
            .filter(|(_, chain)| {
                if is_terminal_record(&chain.record, now) {
                    retired = retired.saturating_add(1);
                    false
                } else {
                    true
                }
            })
            .collect();
        self.retired_chains = retired;
        // The rebuilt roster supersedes every prior roots object; start a
        // fresh chain rather than pretending to continue a damaged one.
        clear_roots_chain(&self.roots_directory)?;
        self.roots_names.clear();
        self.roots_generation = 0;
        self.roots_tail_digest = None;
        Ok(())
    }

    pub(crate) fn open_stats(&self) -> &SweepManagerOpenStats {
        &self.open_stats
    }

    /// How many sweep chains this manager holds resident. Diagnostic
    /// attribution only; no caller branches on it.
    pub(crate) fn chain_count(&self) -> usize {
        self.chains.len()
    }

    /// Unfinished sweep work and explicit pending Restore, with the exact
    /// Restore pins a generation capture must retain.
    pub(crate) fn current_action_roots(&self) -> CurrentActionRoots {
        let mut pins = Vec::new();
        for (sweep_id, chain) in &self.chains {
            let pending = latest_pending_action(&chain.record);
            for member in &chain.record.members {
                pins.push(SweepRetentionPin::from_member(*sweep_id, member, pending));
            }
        }
        CurrentActionRoots {
            actionable_intents: Vec::new(),
            sweep_pins: pins,
        }
    }

    /// Point-load one sweep's chain by exact id, active or terminal.
    ///
    /// A completed sweep leaves the active roster but never leaves the store:
    /// its records are read by exact name, which is what keeps historical
    /// Restore available without keeping every old record resident.
    fn load_historical_chain(&self, sweep_id: Uuid) -> Result<Option<SweepChain>, SweepError> {
        let mut stats = SweepManagerOpenStats::default();
        let first = object_name(sweep_id, 1);
        if read_optional_regular(&self.directory, &first, MAX_SWEEP_OBJECT_BYTES, None)?.is_none() {
            return Ok(None);
        }
        Ok(Some(advance_chain(
            &self.directory,
            self.workspace_id,
            sweep_id,
            None,
            u64::MAX,
            &mut stats,
        )?))
    }

    /// Reload one sweep into the active roster so an explicit user action can
    /// run against it. Restore after completion is a first-class case: the
    /// disposition is durable history, and choosing to act on it again makes
    /// the chain current work once more.
    fn activate(&mut self, sweep_id: Uuid) -> Result<(), SweepError> {
        if self.chains.contains_key(&sweep_id) {
            return Ok(());
        }
        let chain = self
            .load_historical_chain(sweep_id)?
            .ok_or_else(|| SweepError::Invalid(format!("unknown sweep {sweep_id}")))?;
        self.chains.insert(sweep_id, chain);
        self.retired_chains = self.retired_chains.saturating_sub(1);
        Ok(())
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

    #[cfg(test)]
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

    #[cfg(test)]
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
        // Acting on a completed sweep is a first-class case: reload its
        // durable chain into the active roster before authoring anything.
        self.activate(sweep_id)?;
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
        // Acting on a completed sweep is a first-class case: reload its
        // durable chain into the active roster before authoring anything.
        self.activate(sweep_id)?;
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
        // Acting on a completed sweep is a first-class case: reload its
        // durable chain into the active roster before authoring anything.
        self.activate(sweep_id)?;
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
        // Acting on a completed sweep is a first-class case: reload its
        // durable chain into the active roster before authoring anything.
        self.activate(sweep_id)?;
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
        // Acting on a completed sweep is a first-class case: reload its
        // durable chain into the active roster before authoring anything.
        self.activate(sweep_id)?;
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
        // Acting on a completed sweep is a first-class case: reload its
        // durable chain into the active roster before authoring anything.
        self.activate(sweep_id)?;
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
        // Acting on a completed sweep is a first-class case: reload its
        // durable chain into the active roster before authoring anything.
        self.activate(sweep_id)?;
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
        // Acting on a completed sweep is a first-class case: reload its
        // durable chain into the active roster before authoring anything.
        self.activate(sweep_id)?;
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

    /// Drop members whose deletion batch never became accepted.
    ///
    /// Only the active roster is reconciled, and membership is asked as a
    /// point question per uncommitted member — normally none at all. A
    /// terminal chain has no uncommitted member left to reconcile, so this no
    /// longer needs the complete accepted batch set to exist.
    fn reconcile_uncommitted_members(
        &mut self,
        membership: &dyn AcceptedBatchMembership,
    ) -> Result<(), SweepError> {
        let mut updates = Vec::new();
        for chain in self.chains.values() {
            let mut retained = Vec::with_capacity(chain.record.members.len());
            let mut dropped = false;
            for member in &chain.record.members {
                let keep = match member.deletion_batch_id {
                    None => true,
                    Some(batch_id) => membership.is_accepted(batch_id)?,
                };
                if keep {
                    retained.push(member.clone());
                } else {
                    dropped = true;
                }
            }
            if dropped {
                let mut record = chain.record.clone();
                record.members = retained;
                updates.push(record);
            }
        }
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
        // The record is durable first, the roots second. A crash between the
        // two leaves the roots one version behind, which the bounded
        // catch-up probe at open resolves; the reverse order would let the
        // roots claim a record that does not exist.
        self.install_roots()
    }

    /// Republish the bounded active roster at the existing sweep commit
    /// boundary.
    fn install_roots(&mut self) -> Result<(), SweepError> {
        let now = now_unix_ms()?;
        let terminal = self
            .chains
            .iter()
            .filter(|(_, chain)| is_terminal_record(&chain.record, now))
            .map(|(sweep_id, _)| *sweep_id)
            .collect::<Vec<_>>();
        // A terminal chain stays on disk and stays point-addressable; it just
        // stops being current actionable state.
        let mut active = self
            .chains
            .iter()
            .filter(|(sweep_id, _)| !terminal.contains(sweep_id))
            .map(|(_, chain)| ActiveSweepPin {
                version: chain.version,
                digest: chain.digest,
                record: chain.record.clone(),
            })
            .collect::<Vec<_>>();
        active.sort_by_key(|pin| pin.record.sweep_id);
        let generation = self
            .roots_generation
            .checked_add(1)
            .ok_or_else(|| SweepError::Invalid("sweep action roots generation overflow".into()))?;
        let object = SweepActionRootsObject {
            schema_version: ROOTS_SCHEMA_VERSION,
            workspace_id: self.workspace_id,
            generation,
            previous_digest: self.roots_tail_digest,
            active,
            retired_chains: self.retired_chains.saturating_add(terminal.len() as u64),
        };
        let bytes = encode_roots(&object)?;
        if bytes.len() as u64 > MAX_ROOTS_OBJECT_BYTES {
            return Err(SweepError::Invalid(
                "sweep action roots exceed their bounded object limit".into(),
            ));
        }
        let digest = ContentDigest::of(&bytes);
        let name = roots_object_name(generation, digest);
        self.store.publish_coalesced_private_derived(
            &self.roots_directory,
            &[(name.as_str(), bytes.as_slice(), MAX_ROOTS_OBJECT_BYTES)],
            "sweep action roots object",
        )?;
        self.roots_generation = generation;
        self.roots_tail_digest = Some(digest);
        self.roots_names
            .insert(generation, RootsName { name, digest });
        self.prune_roots_chain()
    }

    fn prune_roots_chain(&mut self) -> Result<(), SweepError> {
        let obsolete = self
            .roots_names
            .iter()
            .rev()
            .skip(2)
            .map(|(generation, name)| (*generation, name.name.clone()))
            .collect::<Vec<_>>();
        for (generation, name) in &obsolete {
            match self.roots_directory.remove_file(name) {
                Ok(()) | Err(_) => {
                    self.roots_names.remove(generation);
                }
            }
        }
        if !obsolete.is_empty() {
            sync_dir_required(&self.roots_directory)?;
        }
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
    stats: &mut SweepManagerOpenStats,
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
            stats.chain_objects_read = stats.chain_objects_read.saturating_add(1);
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

/// Walk one sweep chain forward by exact name from a known-good point.
///
/// This is the point-addressable read that replaces reconstructing every
/// chain: it touches only the versions of the one sweep it was asked about,
/// and stops at the first version that is absent or torn. `start` of `None`
/// begins at version 1, which is how a terminal chain is loaded on demand.
fn advance_chain(
    directory: &Dir,
    workspace_id: WorkspaceId,
    sweep_id: Uuid,
    start: Option<SweepChain>,
    max_versions: u64,
    stats: &mut SweepManagerOpenStats,
) -> Result<SweepChain, SweepError> {
    let mut current = start;
    let mut steps = 0_u64;
    loop {
        let next_version = current.as_ref().map_or(1, |chain| {
            chain
                .version
                .checked_add(1)
                .expect("validated sweep version")
        });
        if steps >= max_versions {
            break;
        }
        let name = object_name(sweep_id, next_version);
        let Some(bytes) = read_optional_regular(directory, &name, MAX_SWEEP_OBJECT_BYTES, None)?
        else {
            break;
        };
        stats.chain_objects_read = stats.chain_objects_read.saturating_add(1);
        let previous_digest = current.as_ref().map(|chain| chain.digest);
        let Ok(object) = decode_bound_object(
            &bytes,
            workspace_id,
            sweep_id,
            next_version,
            previous_digest,
        ) else {
            // A torn tail cannot invalidate the preceding immutable version.
            break;
        };
        current = Some(SweepChain {
            version: next_version,
            digest: ContentDigest::of(&bytes),
            record: object.record,
        });
        steps = steps.saturating_add(1);
    }
    current.ok_or_else(|| SweepError::Invalid(format!("sweep {sweep_id} has no valid record")))
}

/// Nothing current can still act on this chain.
///
/// It is closed, no barrier is active, no action is unfinished, and either the
/// user already chose a disposition or the record never crossed the
/// user-surfacing boundary at all — a quiet tier-1 record has no disposition
/// to await, so holding it active forever would be exactly the append-forever
/// term this replaces. The record itself is never deleted.
fn is_terminal_record(record: &SweepRecord, now_unix_ms: u64) -> bool {
    !record.is_open()
        && !record.barrier_active_at(now_unix_ms)
        && latest_pending_action(record).is_none()
        && (record.disposed_at_unix_ms.is_some() || !record.tier.surfaced())
}

/// The kind of the one action still owed, if any.
fn latest_pending_action(record: &SweepRecord) -> Option<SweepActionKind> {
    let mut latest = BTreeMap::<Uuid, (SweepActionKind, &SweepActionState)>::new();
    for action in &record.actions {
        latest.insert(action.action_id, (action.action, &action.state));
    }
    latest.into_values().find_map(|(kind, state)| {
        matches!(
            state,
            SweepActionState::Started | SweepActionState::Progress { .. }
        )
        .then_some(kind)
    })
}

fn encode_roots(object: &SweepActionRootsObject) -> Result<Vec<u8>, SweepError> {
    postcard::to_allocvec(object).map_err(|error| SweepError::Encode(error.to_string()))
}

fn decode_bound_roots(
    bytes: &[u8],
    workspace_id: WorkspaceId,
    generation: u64,
) -> Result<SweepActionRootsObject, SweepError> {
    let object: SweepActionRootsObject =
        postcard::from_bytes(bytes).map_err(|error| SweepError::Invalid(error.to_string()))?;
    if encode_roots(&object)? != bytes
        || object.schema_version != ROOTS_SCHEMA_VERSION
        || object.workspace_id != workspace_id
        || object.generation != generation
        || !object
            .active
            .windows(2)
            .all(|pair| pair[0].record.sweep_id < pair[1].record.sweep_id)
        || object.active.iter().any(|pin| pin.version == 0)
    {
        return Err(SweepError::Invalid(
            "sweep action roots binding or canonical encoding mismatch".into(),
        ));
    }
    Ok(object)
}

fn roots_object_name(generation: u64, digest: ContentDigest) -> String {
    format!("{ROOTS_PREFIX}{generation:020}-{digest}{ROOTS_SUFFIX}")
}

fn enumerate_roots_names(directory: &Dir) -> Result<BTreeMap<u64, RootsName>, SweepError> {
    let mut names = BTreeMap::new();
    for entry in directory.entries()? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.starts_with(ROOTS_PREFIX) {
            continue;
        }
        require_regular_entry(&entry.file_type()?, &name)?;
        let (generation, parsed) = parse_roots_name(&name)?;
        if names.insert(generation, parsed).is_some() {
            return Err(SweepError::Invalid(
                "sweep action roots generation twin".into(),
            ));
        }
    }
    Ok(names)
}

fn parse_roots_name(name: &str) -> Result<(u64, RootsName), SweepError> {
    let body = name
        .strip_prefix(ROOTS_PREFIX)
        .and_then(|value| value.strip_suffix(ROOTS_SUFFIX))
        .ok_or_else(|| SweepError::Invalid("invalid sweep action roots name".into()))?;
    let (digits, digest_hex) = body
        .split_once('-')
        .ok_or_else(|| SweepError::Invalid("sweep action roots name lacks a digest".into()))?;
    if digits.len() != 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SweepError::Invalid(
            "non-canonical sweep action roots generation".into(),
        ));
    }
    let digest = super::identity::parse_digest(digest_hex)
        .map(ContentDigest::from_bytes)
        .map_err(|error| SweepError::Invalid(error.to_string()))?;
    let generation = digits
        .parse::<u64>()
        .map_err(|error| SweepError::Invalid(error.to_string()))?;
    if generation == 0 || roots_object_name(generation, digest) != name {
        return Err(SweepError::Invalid(
            "non-canonical sweep action roots name".into(),
        ));
    }
    Ok((
        generation,
        RootsName {
            name: name.to_owned(),
            digest,
        },
    ))
}

fn clear_roots_chain(directory: &Dir) -> Result<(), SweepError> {
    let mut removed = false;
    for entry in directory.entries()?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(ROOTS_PREFIX) {
            continue;
        }
        if directory.remove_file(name).is_ok() {
            removed = true;
        }
    }
    if removed {
        sync_dir_required(directory)?;
    }
    Ok(())
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
    use super::super::identity::DocumentKey;
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
    struct SweepRoot {
        path: std::path::PathBuf,
        store: ObjectStore,
    }

    impl SweepRoot {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("tine-sweep-{label}-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            let workspace_id = WorkspaceId::from_uuid(Uuid::from_u128(0xc4_f100));
            let store = ObjectStore::open(&path, workspace_id).unwrap();
            Self { path, store }
        }

        fn manager(&self) -> SweepManager {
            SweepManager::open(&self.store, &BTreeSet::new()).unwrap()
        }
    }

    impl Drop for SweepRoot {
        fn drop(&mut self) {
            crate::test_support::remove_dir_all(&self.path);
        }
    }

    /// A distinctive predecessor frontier, so an equality assertion below
    /// cannot pass against a default or a recomputed value.
    fn predecessor_frontier(counter: u64) -> FrontierV2 {
        FrontierV2::new(vec![super::super::DocumentDependencies::new(
            DocumentKey::Entity(super::super::DocumentId::from_uuid(Uuid::from_u128(
                0xc4_f201,
            ))),
            vec![super::super::CrdtPeerCounter::new(
                super::super::CrdtPeerId::from_u64(11),
                counter,
            )],
            vec![BatchId::from_uuid(Uuid::from_u128(0xc4_f301))],
        )
        .unwrap()])
        .unwrap()
    }

    fn sweep_member(counter: u64) -> SweepMember {
        SweepMember {
            path: ManagedPath::parse("pages/retired.md").unwrap(),
            page_id: PageId::from_uuid(Uuid::from_u128(0xc4_f401)),
            deletion_batch_id: None,
            predecessor_accepted_state: SweepAcceptedStateReference {
                page_id: PageId::from_uuid(Uuid::from_u128(0xc4_f501)),
                frontier: predecessor_frontier(counter),
            },
            prior_present_intent_id: Some(ProjectionIntentId::from_marker_digest([0x5a; 32])),
        }
    }

    fn seeded_record(sweep_id: Uuid, now: u64, member: SweepMember) -> SweepRecord {
        SweepRecord {
            sweep_id,
            opened_at_unix_ms: now,
            last_observation_at_unix_ms: now,
            closed_at_unix_ms: None,
            pages_at_open: 100,
            tier: SweepTier::Tier2,
            grace_deadline_unix_ms: None,
            disposed_at_unix_ms: None,
            members: vec![member],
            actions: Vec::new(),
        }
    }

    /// A completed sweep leaves current actionable state, and its exact
    /// Restore predecessor survives that departure.
    ///
    /// This exercises the actual post-completion Restore entry point rather
    /// than comparing capsule digests: `begin_restore` reloads the terminal
    /// chain by exact sweep id and must hand back the member verbatim —
    /// predecessor page identity, predecessor `FrontierV2` with its dependency
    /// heads, and the best-effort prior present intent.
    #[test]
    fn a_disposed_sweep_leaves_active_roots_and_restore_still_reads_its_exact_predecessor() {
        let root = SweepRoot::new("restore-after-completion");
        let sweep_id = Uuid::new_v4();
        let member = sweep_member(7);
        let now = now_unix_ms().unwrap();

        let mut manager = root.manager();
        manager
            .append_record(seeded_record(sweep_id, now, member.clone()))
            .unwrap();
        let pins = manager.current_action_roots();
        assert_eq!(pins.sweep_pins.len(), 1, "an open sweep is current work");
        assert_eq!(
            pins.sweep_pins[0].predecessor_frontier,
            member.predecessor_accepted_state.frontier
        );
        let closure = pins.retention_closure();
        assert!(
            closure
                .documents
                .contains(&member.predecessor_accepted_state.frontier.documents()[0].document_id())
                && closure
                    .batches
                    .contains(&BatchId::from_uuid(Uuid::from_u128(0xc4_f301))),
            "the retention closure names the predecessor's document and dependency head"
        );

        // The user chooses a disposition: the record is terminal.
        let mut disposed = manager.record(sweep_id).unwrap().clone();
        disposed.closed_at_unix_ms = Some(now);
        disposed.disposed_at_unix_ms = Some(now);
        manager.append_record(disposed).unwrap();
        drop(manager);

        let mut reopened = root.manager();
        assert_eq!(
            reopened.open_stats().names_observed,
            0,
            "a healthy open enumerates no sweep record filename: {:?}",
            reopened.open_stats()
        );
        assert!(!reopened.open_stats().repaired);
        assert_eq!(
            reopened.chain_count(),
            0,
            "a disposed sweep is no longer current actionable state"
        );
        assert_eq!(reopened.open_stats().retired_chains, 1);
        assert!(
            reopened.current_action_roots().sweep_pins.is_empty(),
            "history alone pins nothing in the current-action roots"
        );

        // Restore after completion: the original record is still authority.
        let action = reopened.begin_restore(sweep_id).unwrap();
        assert_eq!(
            action.members,
            vec![member.clone()],
            "post-completion Restore must read the exact recorded predecessor state"
        );
        assert_eq!(
            action.members[0].predecessor_accepted_state.frontier,
            predecessor_frontier(7)
        );
        assert_eq!(
            action.members[0].prior_present_intent_id,
            member.prior_present_intent_id
        );
        assert_eq!(
            reopened.current_action_roots().sweep_pins.len(),
            1,
            "an explicit pending Restore is current actionable state again"
        );
    }

    /// A Restore interrupted mid-flight resumes from its durable cursor, and
    /// the chain stays in the active roots across the crash.
    #[test]
    fn a_partial_restore_resumes_from_its_durable_cursor_after_a_reopen() {
        let root = SweepRoot::new("partial-restore-resume");
        let sweep_id = Uuid::new_v4();
        let member = sweep_member(9);
        let now = now_unix_ms().unwrap();

        let mut manager = root.manager();
        manager
            .append_record(seeded_record(sweep_id, now, member.clone()))
            .unwrap();
        let started = manager.begin_restore(sweep_id).unwrap();
        let authored = vec![BatchId::from_uuid(Uuid::from_u128(0xc4_f601))];
        let cursor = SweepRestoreCursor {
            chunk_ordinal: 3,
            remaining_operation_watermark: 41,
            nondecreasing_retries: 0,
        };
        manager
            .record_restore_progress(
                sweep_id,
                started.action_id,
                authored.clone(),
                cursor.clone(),
            )
            .unwrap();
        drop(manager);

        let reopened = root.manager();
        assert_eq!(
            reopened.open_stats().names_observed,
            0,
            "resuming an unfinished action reads the roots, not the record directory"
        );
        assert_eq!(reopened.chain_count(), 1);
        assert_eq!(
            reopened.pending_restore_actions(),
            vec![(sweep_id, started.action_id)],
            "the unfinished Restore is still owed after the reopen"
        );
        let pins = reopened.current_action_roots().sweep_pins;
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].pending_action, Some(SweepActionKind::Restore));
        assert_eq!(
            pins[0].predecessor_frontier,
            member.predecessor_accepted_state.frontier
        );

        let mut reopened = reopened;
        let resumed = reopened.begin_restore(sweep_id).unwrap();
        assert_eq!(
            resumed.action_id, started.action_id,
            "no second action opens"
        );
        assert_eq!(resumed.authored_batch_ids, authored);
        assert_eq!(resumed.cursor, Some(cursor));
        assert!(!resumed.completed);
    }

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
            DocumentKey::Entity(DocumentId::from_uuid(Uuid::from_u128(0xc4f003))),
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
