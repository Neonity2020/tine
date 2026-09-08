use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    FrontierV2, ManagedPath, PageId, ProjectionIntent, ProjectionIntentId, ProjectionTargetKind,
};

pub(crate) type AbsenceDecisionKey = (PageId, ManagedPath);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AbsenceDecision {
    Create,
    DeferredAbsence,
}

/// One completed projection in this activation era. Both the retained
/// receiver receipt store and the coalesced own-endpoint index feed this exact
/// shape; neither half is authoritative by itself.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct AbsenceCompletionAnchor {
    pub(crate) intent_id: ProjectionIntentId,
    pub(crate) page_id: PageId,
    pub(crate) path: ManagedPath,
    pub(crate) target_kind: ProjectionTargetKind,
    pub(crate) frontier: FrontierV2,
}

/// One bounded receiver-summary row. `anchors` is the frontier-maximal
/// antichain for the key; the bit preserves the only historical relation used
/// outside that antichain by restore/recreation handling.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReceiverAbsenceSummaryEntry {
    pub(crate) page_id: PageId,
    pub(crate) path: ManagedPath,
    pub(crate) anchors: Vec<AbsenceCompletionAnchor>,
    pub(crate) restored_generation_requires_deferral: bool,
}

impl AbsenceCompletionAnchor {
    pub(crate) fn from_intent(intent: &ProjectionIntent) -> Result<Self, super::ReceiptError> {
        Ok(Self {
            intent_id: intent.id()?,
            page_id: intent.page_id(),
            path: intent.path().clone(),
            target_kind: intent.target_kind(),
            frontier: intent.frontier().clone(),
        })
    }

    pub(crate) fn key(&self) -> AbsenceDecisionKey {
        (self.page_id, self.path.clone())
    }
}

/// A named repair signal from the point-addressable receiver history.
///
/// A derived point row that cannot be read is damage, never "this page has no
/// history": swallowing it into `Create`/`false` is exactly the silent
/// resurrection this map exists to prevent, so it is propagated to the caller,
/// which routes it to the instrumented rebuild (D-3, I-10).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReceiverHistoryUnavailable(pub(crate) String);

impl std::fmt::Display for ReceiverHistoryUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "receiver absence history: {}", self.0)
    }
}

impl std::error::Error for ReceiverHistoryUnavailable {}

/// Point-addressable durable receiver decision rows.
///
/// One implementation exists — the composed authenticated maps published beside
/// the receiver absence roots — and it answers exactly one (page, exact path)
/// key per call. Nothing here enumerates a page's historical paths, and the
/// returned row is not retained by the map.
pub(crate) trait ReceiverHistoryRead: std::fmt::Debug + Send + Sync {
    /// The durable receiver row for one exact page and exact managed path.
    ///
    /// `Ok(None)` means the authenticated maps prove the key has no durable
    /// receiver history. A row the maps claim but whose bytes are missing or
    /// damaged is an `Err`, never an empty answer.
    fn receiver_row(
        &self,
        page_id: PageId,
        path: &ManagedPath,
    ) -> Result<Option<ReceiverAbsenceSummaryEntry>, ReceiverHistoryUnavailable>;

    /// Retire the derived roots so the next open takes the named, counted
    /// rebuild from retained receipts. Called exactly when a point read proved
    /// damage: the current caller still receives its named refusal, and the
    /// acknowledged state cannot become a permanent reopen failure (I-10).
    fn retire_damaged(&self) {}
}

#[derive(Debug)]
pub(crate) enum AbsenceDecisionError {
    Receipt(super::ReceiptError),
    History(ReceiverHistoryUnavailable),
}

impl std::fmt::Display for AbsenceDecisionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Receipt(error) => write!(formatter, "{error}"),
            Self::History(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for AbsenceDecisionError {}

impl From<super::ReceiptError> for AbsenceDecisionError {
    fn from(error: super::ReceiptError) -> Self {
        Self::Receipt(error)
    }
}

impl From<ReceiverHistoryUnavailable> for AbsenceDecisionError {
    fn from(error: ReceiverHistoryUnavailable) -> Self {
        Self::History(error)
    }
}

/// Current receiver/own absence-decision state for one activation era.
///
/// Receiver receipts remain the durable truth. What this value keeps resident
/// is bounded by *current* work — unfinished receiver intents, the device-local
/// completion index's already-pruned entries, and rows whose durable
/// write-through failed. Completed receiver history is not resident at all: it
/// is read one page at a time through [`ReceiverHistoryRead`], and the same
/// decision algorithm runs over the historical point row and the live overlay,
/// so there is exactly one producer of an absence answer.
///
/// A map with no history attached (a generic/offline engine with no archive
/// capability) keeps the whole receiver half in `receiver_overlay`; that is the
/// documented full-catalog fallback, and it is the only shape that is still
/// resident-per-history.
#[derive(Clone, Debug, Default)]
pub(crate) struct AbsenceDecisionMap {
    history: Option<std::sync::Arc<dyn ReceiverHistoryRead>>,
    receiver_overlay: BTreeMap<AbsenceDecisionKey, ReceiverAbsenceSummaryEntry>,
    local_completions: BTreeMap<AbsenceDecisionKey, Vec<AbsenceCompletionAnchor>>,
    local_deferrals: BTreeSet<AbsenceDecisionKey>,
    incomplete_receiver:
        BTreeMap<AbsenceDecisionKey, BTreeMap<ProjectionIntentId, ProjectionIntent>>,
}

impl AbsenceDecisionMap {
    /// Bind the durable point-addressable receiver history this map decides
    /// against. Without it every receiver row must be held resident.
    pub(crate) fn attach_history(&mut self, history: std::sync::Arc<dyn ReceiverHistoryRead>) {
        self.history = Some(history);
    }

    /// Route a proven point-read failure to the instrumented rebuild.
    pub(crate) fn retire_damaged_history(&self) {
        if let Some(history) = self.history.as_deref() {
            history.retire_damaged();
        }
    }

    /// Resident current-state rows: the quantity the growth qualification
    /// counts. Completed receiver history is not included because it is not
    /// resident.
    pub(crate) fn resident_row_count(&self) -> usize {
        self.receiver_overlay.len()
            + self.local_completions.len()
            + self.local_deferrals.len()
            + self.incomplete_receiver.len()
    }

    /// Record one durable receiver intent. Returns whether anything in this
    /// map actually changed, so the bounded current-action producer can skip
    /// republishing an identical roots object on a replayed notification.
    pub(crate) fn record_receiver_intent(
        &mut self,
        intent: &ProjectionIntent,
    ) -> Result<bool, AbsenceDecisionError> {
        let key = (intent.page_id(), intent.path().clone());
        let intent_id = intent.id()?;
        if self
            .durable_row(&key)?
            .is_some_and(|entry| entry.anchors.iter().any(|a| a.intent_id == intent_id))
        {
            return Ok(false);
        }
        let changed = self
            .incomplete_receiver
            .entry(key)
            .or_default()
            .insert(intent_id, intent.clone())
            .as_ref()
            != Some(intent);
        Ok(changed)
    }

    /// Seed one durable receiver intent that is known to have no completion —
    /// the roots object's own actionable set. Skips the completion probe the
    /// notification path needs, so a reopen costs no point read per obligation.
    pub(crate) fn record_incomplete_receiver_intent(
        &mut self,
        intent: &ProjectionIntent,
    ) -> Result<(), super::ReceiptError> {
        let key = (intent.page_id(), intent.path().clone());
        self.incomplete_receiver
            .entry(key)
            .or_default()
            .insert(intent.id()?, intent.clone());
        Ok(())
    }

    /// Retire one receiver intent from the resident incomplete set because its
    /// completion became durable somewhere else — the managed path's point row.
    ///
    /// Without this the map keeps one resident entry per *completed* receiver
    /// intent, which is exactly the history term it exists not to hold, and
    /// [`Self::incomplete_receiver_intents`] would keep offering finished work.
    pub(crate) fn retire_receiver_intent(
        &mut self,
        intent: &ProjectionIntent,
    ) -> Result<bool, super::ReceiptError> {
        let key = (intent.page_id(), intent.path().clone());
        let intent_id = intent.id()?;
        let Some(incomplete) = self.incomplete_receiver.get_mut(&key) else {
            return Ok(false);
        };
        let removed = incomplete.remove(&intent_id).is_some();
        if incomplete.is_empty() {
            self.incomplete_receiver.remove(&key);
        }
        Ok(removed)
    }

    /// Record one durable receiver completion into the resident overlay.
    ///
    /// The managed path writes the same completion through to the durable point
    /// row instead; this overlay carries it only while no history store is
    /// attached, or after a durable write-through failed and the row must stay
    /// resident until the next open re-reads it from retained receipts.
    pub(crate) fn record_receiver_completion(
        &mut self,
        intent: &ProjectionIntent,
    ) -> Result<bool, AbsenceDecisionError> {
        let anchor = AbsenceCompletionAnchor::from_intent(intent)?;
        let key = anchor.key();
        let mut changed = false;
        if let Some(incomplete) = self.incomplete_receiver.get_mut(&key) {
            changed |= incomplete.remove(&anchor.intent_id).is_some();
            if incomplete.is_empty() {
                self.incomplete_receiver.remove(&key);
            }
        }
        let (existing, deferral) = self.merged_anchors(&key)?;
        let relation = restored_generation_relation(&existing, &anchor);
        let entry = self.receiver_overlay.entry(key.clone()).or_insert_with(|| {
            ReceiverAbsenceSummaryEntry {
                page_id: key.0,
                path: key.1.clone(),
                anchors: Vec::new(),
                restored_generation_requires_deferral: deferral,
            }
        });
        changed |= merge_anchor(&mut entry.anchors, anchor);
        if relation && !entry.restored_generation_requires_deferral {
            entry.restored_generation_requires_deferral = true;
            changed = true;
        }
        Ok(changed)
    }

    /// Record one own-endpoint completion. The anchors come from the device
    /// local completion index, which prunes to live pages plus retained
    /// intents, so this half is bounded by current work by construction.
    pub(crate) fn record_local_completion(
        &mut self,
        anchor: AbsenceCompletionAnchor,
    ) -> Result<(), AbsenceDecisionError> {
        let key = anchor.key();
        let (existing, _) = self.merged_anchors(&key)?;
        if restored_generation_relation(&existing, &anchor) {
            self.local_deferrals.insert(key.clone());
        }
        merge_anchor(self.local_completions.entry(key).or_default(), anchor);
        Ok(())
    }

    /// Re-derive the own-endpoint half from the device-local completion index
    /// after that index prunes.
    ///
    /// Without this, every own identity completed during one activation stays
    /// resident for the whole activation even after its durable evidence is
    /// pruned. Re-deriving makes the live map converge on exactly what the next
    /// open would seed from the same index, so a long session and a reopen give
    /// the same answers from the same bounded state. The sticky
    /// restored-generation deferrals are re-derived with it, again matching what
    /// the reopen would compute.
    pub(crate) fn reseed_local_completions(
        &mut self,
        anchors: impl IntoIterator<Item = AbsenceCompletionAnchor>,
    ) -> Result<(), AbsenceDecisionError> {
        self.local_completions.clear();
        self.local_deferrals.clear();
        for anchor in anchors {
            self.record_local_completion(anchor)?;
        }
        Ok(())
    }

    /// Seed one receiver row into the resident overlay. Only the no-history
    /// fallback and the qualification harness use this; the managed path keeps
    /// these rows on disk.
    pub(crate) fn record_receiver_summary_entry(&mut self, entry: ReceiverAbsenceSummaryEntry) {
        let key = (entry.page_id, entry.path.clone());
        let existing = self.receiver_overlay.entry(key.clone()).or_insert_with(|| {
            ReceiverAbsenceSummaryEntry {
                page_id: key.0,
                path: key.1,
                anchors: Vec::new(),
                restored_generation_requires_deferral: false,
            }
        });
        existing.restored_generation_requires_deferral |=
            entry.restored_generation_requires_deferral;
        for anchor in entry.anchors {
            merge_anchor(&mut existing.anchors, anchor);
        }
    }

    pub(crate) fn decision(
        &self,
        page_id: PageId,
        path: &ManagedPath,
    ) -> Result<AbsenceDecision, AbsenceDecisionError> {
        let (anchors, _) = self.merged_anchors(&(page_id, path.clone()))?;
        Ok(decide(&anchors))
    }

    pub(crate) fn restored_generation_requires_deferral(
        &self,
        page_id: PageId,
        path: &ManagedPath,
    ) -> Result<bool, AbsenceDecisionError> {
        let (_, deferral) = self.merged_anchors(&(page_id, path.clone()))?;
        Ok(deferral)
    }

    pub(crate) fn incomplete_receiver_intents(
        &self,
        page_id: PageId,
        path: &ManagedPath,
    ) -> Vec<ProjectionIntent> {
        self.incomplete_receiver
            .get(&(page_id, path.clone()))
            .map_or_else(Vec::new, |entries| entries.values().cloned().collect())
    }

    /// Does any receiver evidence — durable completion row or unfinished
    /// intent — exist under this exact page/path key?
    ///
    /// This is the R16-C2 predicate the own-endpoint prune policy asks for one
    /// key at a time. It replaces the lifetime `receiver_history_paths` set.
    pub(crate) fn receiver_history_key_present(
        &self,
        key: &AbsenceDecisionKey,
    ) -> Result<bool, AbsenceDecisionError> {
        Ok(self.receiver_row_anchors(key)?.is_some())
    }

    /// The R16-C2 prune policy's whole question about one exact key, answered
    /// by one point read: `None` when no receiver evidence exists underneath it,
    /// otherwise that key's receiver completion anchors. Asking presence and
    /// domination separately would read the same row twice per local key.
    pub(crate) fn receiver_row_anchors(
        &self,
        key: &AbsenceDecisionKey,
    ) -> Result<Option<Vec<AbsenceCompletionAnchor>>, AbsenceDecisionError> {
        let row = self.durable_row(key)?;
        if row.is_none() && !self.incomplete_receiver.contains_key(key) {
            return Ok(None);
        }
        Ok(Some(row.map_or_else(Vec::new, |entry| entry.anchors)))
    }

    /// Every resident receiver row, used only by the no-history fallback when
    /// it hands its state to a durable rebuild, and by qualification.
    pub(crate) fn resident_receiver_rows(&self) -> Vec<ReceiverAbsenceSummaryEntry> {
        self.receiver_overlay.values().cloned().collect()
    }

    /// The receiver row for one exact key: the resident overlay merged over the
    /// durable point row. One point read, no enumeration, nothing cached.
    fn durable_row(
        &self,
        key: &AbsenceDecisionKey,
    ) -> Result<Option<ReceiverAbsenceSummaryEntry>, AbsenceDecisionError> {
        let mut row = match self.history.as_deref() {
            Some(history) => history.receiver_row(key.0, &key.1)?,
            None => None,
        };
        if let Some(overlay) = self.receiver_overlay.get(key) {
            match row.as_mut() {
                Some(row) => {
                    row.restored_generation_requires_deferral |=
                        overlay.restored_generation_requires_deferral;
                    for anchor in &overlay.anchors {
                        merge_anchor(&mut row.anchors, anchor.clone());
                    }
                }
                None => row = Some(overlay.clone()),
            }
        }
        Ok(row)
    }

    /// The one merge every absence answer goes through: durable historical
    /// point row, resident overlay, and own-endpoint completions folded by the
    /// same `merge_anchor`/`restored_generation_relation` pair the durable row
    /// writer uses.
    fn merged_anchors(
        &self,
        key: &AbsenceDecisionKey,
    ) -> Result<(Vec<AbsenceCompletionAnchor>, bool), AbsenceDecisionError> {
        let mut anchors = Vec::new();
        let mut deferral = self.local_deferrals.contains(key);
        if let Some(entry) = self.durable_row(key)? {
            deferral |= entry.restored_generation_requires_deferral;
            for anchor in entry.anchors {
                merge_anchor(&mut anchors, anchor);
            }
        }
        if let Some(local) = self.local_completions.get(key) {
            for anchor in local {
                merge_anchor(&mut anchors, anchor.clone());
            }
        }
        Ok((anchors, deferral))
    }
}

/// The absence answer for one exact key's frontier-maximal antichain.
///
/// This is the ONE decision algorithm. It is called with the merged historical
/// point row plus the live overlay, and — through the durable row writer — with
/// the same merged set when a row is published, so the persisted row and the
/// live answer can never be two semantic producers.
pub(crate) fn decide(entries: &[AbsenceCompletionAnchor]) -> AbsenceDecision {
    if entries.is_empty() {
        return AbsenceDecision::Create;
    }
    let maximal = entries.iter().filter(|candidate| {
        !entries.iter().any(|other| {
            (other.intent_id != candidate.intent_id || other.target_kind != candidate.target_kind)
                && frontier_strictly_dominates(&other.frontier, &candidate.frontier)
        })
    });
    if maximal
        .into_iter()
        .any(|entry| entry.target_kind == ProjectionTargetKind::Present)
    {
        // A defensive incomparable antichain that mixes target kinds takes
        // the reversible direction. Recreating bytes here would be the
        // resurrection this map exists to prevent.
        AbsenceDecision::DeferredAbsence
    } else {
        AbsenceDecision::Create
    }
}

/// Does adding `anchor` to `entries` establish the restored-generation relation
/// a later recreation must defer on? Shared by the resident overlay and by the
/// durable row writer, so the persisted sticky bit and the live answer agree.
pub(crate) fn restored_generation_relation(
    entries: &[AbsenceCompletionAnchor],
    anchor: &AbsenceCompletionAnchor,
) -> bool {
    entries.iter().any(|other| {
        (anchor.target_kind == ProjectionTargetKind::Present
            && other.target_kind == ProjectionTargetKind::Absent
            && frontier_strictly_dominates(&anchor.frontier, &other.frontier))
            || (other.target_kind == ProjectionTargetKind::Present
                && anchor.target_kind == ProjectionTargetKind::Absent
                && frontier_strictly_dominates(&other.frontier, &anchor.frontier))
    })
}

/// Fold one anchor into a key's frontier-maximal antichain. Returns whether the
/// antichain changed.
pub(crate) fn merge_anchor(
    entries: &mut Vec<AbsenceCompletionAnchor>,
    anchor: AbsenceCompletionAnchor,
) -> bool {
    if entries.iter().any(|entry| entry == &anchor)
        || entries.iter().any(|entry| {
            entry.target_kind == anchor.target_kind
                && entry.intent_id != anchor.intent_id
                && frontier_strictly_dominates(&entry.frontier, &anchor.frontier)
        })
    {
        return false;
    }
    entries.retain(|entry| {
        entry.target_kind != anchor.target_kind
            || entry.intent_id == anchor.intent_id
            || !frontier_strictly_dominates(&anchor.frontier, &entry.frontier)
    });
    entries.push(anchor);
    true
}

/// The projection target changes only when its accepted page state advances,
/// so a strict CRDT-counter superset orders device-local completion frontiers.
/// Equal counters with different dependency heads are left incomparable; the
/// map then chooses the conservative mixed-antichain disposition.
pub(crate) fn frontier_strictly_dominates(later: &FrontierV2, earlier: &FrontierV2) -> bool {
    if later == earlier {
        return false;
    }
    let mut strict = later.documents().len() > earlier.documents().len();
    for earlier_document in earlier.documents() {
        let Ok(index) = later
            .documents()
            .binary_search_by_key(&earlier_document.document_id(), |document| {
                document.document_id()
            })
        else {
            return false;
        };
        let later_document = &later.documents()[index];
        for earlier_counter in earlier_document.peer_counters() {
            let later_counter = later_document
                .peer_counters()
                .binary_search_by_key(&earlier_counter.peer_id(), |counter| counter.peer_id())
                .ok()
                .map(|index| later_document.peer_counters()[index].max_counter())
                .unwrap_or(0);
            if later_counter < earlier_counter.max_counter() {
                return false;
            }
            strict |= later_counter > earlier_counter.max_counter();
        }
        strict |= later_document.peer_counters().len() > earlier_document.peer_counters().len();
    }
    strict
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::oplog::{
        CrdtPeerCounter, CrdtPeerId, DocumentDependencies, DocumentId, ProjectionPrecondition,
    };

    fn frontier(counter: u64) -> FrontierV2 {
        FrontierV2::new(vec![DocumentDependencies::new(
            DocumentId::from_uuid(Uuid::from_u128(0xc3_1000)),
            vec![CrdtPeerCounter::new(CrdtPeerId::from_u64(7), counter)],
            Vec::new(),
        )
        .unwrap()])
        .unwrap()
    }

    fn intent(
        page_id: PageId,
        path: &ManagedPath,
        counter: u64,
        target_kind: ProjectionTargetKind,
    ) -> ProjectionIntent {
        ProjectionIntent::new(
            crate::oplog::WorkspaceId::from_uuid(Uuid::from_u128(0xc3_1001)),
            page_id,
            path.clone(),
            frontier(counter),
            Vec::new(),
            ProjectionPrecondition::Absent,
            target_kind,
            if target_kind == ProjectionTargetKind::Absent {
                crate::oplog::BlobDescription::of(&[])
            } else {
                crate::oplog::BlobDescription::of(format!("target {counter}").as_bytes())
            },
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn frontier_maximal_completion_across_both_halves_decides_absence() {
        let page_id = PageId::from_uuid(Uuid::from_u128(0xc3_1010));
        let path = ManagedPath::parse("receiver-map.md").unwrap();
        let mut map = AbsenceDecisionMap::default();
        let receiver_present = intent(page_id, &path, 1, ProjectionTargetKind::Present);
        map.record_receiver_completion(&receiver_present).unwrap();
        assert_eq!(
            map.decision(page_id, &path).unwrap(),
            AbsenceDecision::DeferredAbsence
        );

        let local_absent = intent(page_id, &path, 2, ProjectionTargetKind::Absent);
        map.record_local_completion(AbsenceCompletionAnchor::from_intent(&local_absent).unwrap())
            .unwrap();
        assert_eq!(
            map.decision(page_id, &path).unwrap(),
            AbsenceDecision::Create
        );

        let later_receiver_present = intent(page_id, &path, 3, ProjectionTargetKind::Present);
        map.record_receiver_completion(&later_receiver_present)
            .unwrap();
        assert_eq!(
            map.decision(page_id, &path).unwrap(),
            AbsenceDecision::DeferredAbsence
        );
    }

    #[test]
    fn no_completion_and_maximal_absent_both_create() {
        let page_id = PageId::from_uuid(Uuid::from_u128(0xc3_1020));
        let path = ManagedPath::parse("receiver-create.md").unwrap();
        let mut map = AbsenceDecisionMap::default();
        assert_eq!(
            map.decision(page_id, &path).unwrap(),
            AbsenceDecision::Create
        );
        let absent = intent(page_id, &path, 1, ProjectionTargetKind::Absent);
        map.record_receiver_completion(&absent).unwrap();
        assert_eq!(
            map.decision(page_id, &path).unwrap(),
            AbsenceDecision::Create
        );
    }

    #[test]
    fn incomparable_mixed_targets_choose_the_reversible_direction() {
        let page_id = PageId::from_uuid(Uuid::from_u128(0xc3_1030));
        let path = ManagedPath::parse("receiver-antichain.md").unwrap();
        let mut map = AbsenceDecisionMap::default();
        let present = intent(page_id, &path, 1, ProjectionTargetKind::Present);
        let absent = ProjectionIntent::new(
            present.workspace_id(),
            page_id,
            path.clone(),
            FrontierV2::new(vec![DocumentDependencies::new(
                DocumentId::from_uuid(Uuid::from_u128(0xc3_1031)),
                vec![CrdtPeerCounter::new(CrdtPeerId::from_u64(9), 1)],
                Vec::new(),
            )
            .unwrap()])
            .unwrap(),
            Vec::new(),
            ProjectionPrecondition::Absent,
            ProjectionTargetKind::Absent,
            crate::oplog::BlobDescription::of(&[]),
            Vec::new(),
        )
        .unwrap();
        map.record_receiver_completion(&present).unwrap();
        map.record_local_completion(AbsenceCompletionAnchor::from_intent(&absent).unwrap())
            .unwrap();
        assert_eq!(
            map.decision(page_id, &path).unwrap(),
            AbsenceDecision::DeferredAbsence
        );
    }

    #[test]
    fn target_kind_collision_across_halves_keeps_both_maximal_answers() {
        let page_id = PageId::from_uuid(Uuid::from_u128(0xc3_1040));
        let path = ManagedPath::parse("receiver-kind-collision.md").unwrap();
        let make = |target_kind| {
            ProjectionIntent::new(
                crate::oplog::WorkspaceId::from_uuid(Uuid::from_u128(0xc3_1001)),
                page_id,
                path.clone(),
                frontier(1),
                Vec::new(),
                ProjectionPrecondition::Absent,
                target_kind,
                crate::oplog::BlobDescription::of(&[]),
                Vec::new(),
            )
            .unwrap()
        };
        let receiver_present = make(ProjectionTargetKind::Present);
        let local_absent = make(ProjectionTargetKind::Absent);
        assert_eq!(receiver_present.id().unwrap(), local_absent.id().unwrap());

        let mut map = AbsenceDecisionMap::default();
        map.record_receiver_completion(&receiver_present).unwrap();
        map.record_local_completion(AbsenceCompletionAnchor::from_intent(&local_absent).unwrap())
            .unwrap();
        assert_eq!(
            map.decision(page_id, &path).unwrap(),
            AbsenceDecision::DeferredAbsence,
            "target kind is not part of the intent id, so a cross-half collision must not overwrite"
        );
    }

    /// A fake point-addressable receiver history: exactly the shape the durable
    /// authenticated page map answers with, one page at a time.
    #[derive(Debug, Default)]
    struct PointRows {
        pages: BTreeMap<PageId, Vec<ReceiverAbsenceSummaryEntry>>,
        reads: std::sync::atomic::AtomicUsize,
    }

    impl PointRows {
        fn insert(&mut self, entry: ReceiverAbsenceSummaryEntry) {
            let rows = self.pages.entry(entry.page_id).or_default();
            match rows.iter_mut().find(|row| row.path == entry.path) {
                Some(row) => {
                    row.restored_generation_requires_deferral |=
                        entry.restored_generation_requires_deferral;
                    for anchor in entry.anchors {
                        merge_anchor(&mut row.anchors, anchor);
                    }
                }
                None => rows.push(entry),
            }
        }
    }

    impl ReceiverHistoryRead for PointRows {
        fn receiver_row(
            &self,
            page_id: PageId,
            path: &ManagedPath,
        ) -> Result<Option<ReceiverAbsenceSummaryEntry>, ReceiverHistoryUnavailable> {
            self.reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self
                .pages
                .get(&page_id)
                .and_then(|rows| rows.iter().find(|row| &row.path == path))
                .cloned())
        }
    }

    #[derive(Debug)]
    struct DamagedRows;

    impl ReceiverHistoryRead for DamagedRows {
        fn receiver_row(
            &self,
            _page_id: PageId,
            _path: &ManagedPath,
        ) -> Result<Option<ReceiverAbsenceSummaryEntry>, ReceiverHistoryUnavailable> {
            Err(ReceiverHistoryUnavailable("row object is missing".into()))
        }
    }

    /// A damaged point row is a named repair, never `Create`/`false`.
    #[test]
    fn a_damaged_point_row_never_becomes_a_create_answer() {
        let page_id = PageId::from_uuid(Uuid::from_u128(0xc3_1050));
        let path = ManagedPath::parse("receiver-damaged.md").unwrap();
        let mut map = AbsenceDecisionMap::default();
        map.attach_history(std::sync::Arc::new(DamagedRows));
        let error = map.decision(page_id, &path).unwrap_err();
        assert!(
            matches!(error, AbsenceDecisionError::History(_)),
            "a derived row that cannot be read must propagate repair, not decide: {error:?}"
        );
        assert!(matches!(
            map.restored_generation_requires_deferral(page_id, &path)
                .unwrap_err(),
            AbsenceDecisionError::History(_)
        ));
        assert!(matches!(
            map.receiver_history_key_present(&(page_id, path))
                .unwrap_err(),
            AbsenceDecisionError::History(_)
        ));
    }

    /// The historical point row and the fully resident map are one producer.
    ///
    /// The `full` map keeps every receiver row resident (the no-history
    /// fallback). The `summarized` map keeps NOTHING resident for completed
    /// receiver history — its rows live only behind a point read — and the two
    /// must answer every decision, deferral and incomplete-intent question
    /// identically, including after further own-endpoint completions.
    #[test]
    fn point_addressable_rows_and_resident_rows_decide_identically() {
        for seed in 1..=96_u64 {
            let mut state = seed;
            let mut next = || {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                state
            };
            let paths = (0..5)
                .map(|index| ManagedPath::parse(format!("generated/{index}.md")).unwrap())
                .collect::<Vec<_>>();
            let pages = (0..5)
                .map(|index| PageId::from_uuid(Uuid::from_u128(0xc6_2000 + index)))
                .collect::<Vec<_>>();
            let mut full = AbsenceDecisionMap::default();
            for _ in 0..48 {
                let value = next();
                let key = (value as usize) % paths.len();
                let target_kind = if value & 1 == 0 {
                    ProjectionTargetKind::Present
                } else {
                    ProjectionTargetKind::Absent
                };
                let candidate = intent(pages[key], &paths[key], (value % 12) + 1, target_kind);
                if value & 4 == 0 {
                    full.record_receiver_intent(&candidate).unwrap();
                } else {
                    full.record_receiver_completion(&candidate).unwrap();
                }
            }

            let incomplete = pages
                .iter()
                .copied()
                .zip(&paths)
                .flat_map(|(page_id, path)| full.incomplete_receiver_intents(page_id, path))
                .collect::<Vec<_>>();

            let mut rows = PointRows::default();
            for entry in full.resident_receiver_rows() {
                rows.insert(entry);
            }
            let mut summarized = AbsenceDecisionMap::default();
            summarized.attach_history(std::sync::Arc::new(rows));
            for intent in &incomplete {
                summarized.record_receiver_intent(intent).unwrap();
            }
            assert_eq!(
                summarized.resident_receiver_rows().len(),
                0,
                "completed receiver history must not be resident behind a point index"
            );

            for _ in 0..24 {
                let value = next();
                let key = (value as usize) % paths.len();
                let target_kind = if value & 2 == 0 {
                    ProjectionTargetKind::Present
                } else {
                    ProjectionTargetKind::Absent
                };
                let anchor = AbsenceCompletionAnchor::from_intent(&intent(
                    pages[key],
                    &paths[key],
                    (value % 16) + 1,
                    target_kind,
                ))
                .unwrap();
                full.record_local_completion(anchor.clone()).unwrap();
                summarized.record_local_completion(anchor).unwrap();
            }

            for (page_id, path) in pages.iter().copied().zip(&paths) {
                assert_eq!(
                    summarized.decision(page_id, path).unwrap(),
                    full.decision(page_id, path).unwrap(),
                    "generated decision mismatch at seed {seed} for {path:?}"
                );
                assert_eq!(
                    summarized
                        .restored_generation_requires_deferral(page_id, path)
                        .unwrap(),
                    full.restored_generation_requires_deferral(page_id, path)
                        .unwrap(),
                    "generated restoration mismatch at seed {seed} for {path:?}"
                );
                assert_eq!(
                    summarized.incomplete_receiver_intents(page_id, path),
                    full.incomplete_receiver_intents(page_id, path),
                    "generated incomplete-intent mismatch at seed {seed} for {path:?}"
                );
                assert_eq!(
                    summarized
                        .receiver_history_key_present(&(page_id, path.clone()))
                        .unwrap(),
                    full.receiver_history_key_present(&(page_id, path.clone()))
                        .unwrap(),
                    "generated receiver-history mismatch at seed {seed} for {path:?}"
                );
            }
        }
    }
}
