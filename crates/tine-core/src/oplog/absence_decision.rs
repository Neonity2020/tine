use std::collections::{BTreeMap, BTreeSet};

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
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AbsenceCompletionAnchor {
    pub(crate) intent_id: ProjectionIntentId,
    pub(crate) page_id: PageId,
    pub(crate) path: ManagedPath,
    pub(crate) target_kind: ProjectionTargetKind,
    pub(crate) frontier: FrontierV2,
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

/// Per-open materialized receiver/own completion view. Receiver records remain
/// the durable truth; this value is disposable and is rebuilt by one validated
/// catalog pass when the managed engine opens.
#[derive(Clone, Debug, Default)]
pub(crate) struct AbsenceDecisionMap {
    completions: BTreeMap<AbsenceDecisionKey, Vec<AbsenceCompletionAnchor>>,
    receiver_history: BTreeSet<AbsenceDecisionKey>,
    receiver_completions: BTreeMap<AbsenceDecisionKey, Vec<AbsenceCompletionAnchor>>,
    incomplete_receiver:
        BTreeMap<AbsenceDecisionKey, BTreeMap<ProjectionIntentId, ProjectionIntent>>,
}

impl AbsenceDecisionMap {
    pub(crate) fn record_receiver_intent(
        &mut self,
        intent: &ProjectionIntent,
    ) -> Result<(), super::ReceiptError> {
        let key = (intent.page_id(), intent.path().clone());
        let intent_id = intent.id()?;
        self.receiver_history.insert(key.clone());
        if self
            .receiver_completions
            .get(&key)
            .is_some_and(|entries| entries.iter().any(|entry| entry.intent_id == intent_id))
        {
            return Ok(());
        }
        self.incomplete_receiver
            .entry(key)
            .or_default()
            .insert(intent_id, intent.clone());
        Ok(())
    }

    pub(crate) fn record_receiver_completion(
        &mut self,
        intent: &ProjectionIntent,
    ) -> Result<(), super::ReceiptError> {
        let anchor = AbsenceCompletionAnchor::from_intent(intent)?;
        let key = anchor.key();
        self.receiver_history.insert(key.clone());
        if let Some(incomplete) = self.incomplete_receiver.get_mut(&key) {
            incomplete.remove(&anchor.intent_id);
            if incomplete.is_empty() {
                self.incomplete_receiver.remove(&key);
            }
        }
        insert_completion(&mut self.completions, key.clone(), anchor.clone());
        insert_completion(&mut self.receiver_completions, key, anchor);
        Ok(())
    }

    pub(crate) fn record_local_completion(&mut self, anchor: AbsenceCompletionAnchor) {
        insert_completion(&mut self.completions, anchor.key(), anchor);
    }

    pub(crate) fn decision(&self, page_id: PageId, path: &ManagedPath) -> AbsenceDecision {
        let key = (page_id, path.clone());
        let Some(entries) = self.completions.get(&key) else {
            return AbsenceDecision::Create;
        };
        let maximal = entries.iter().filter(|candidate| {
            !entries.iter().any(|other| {
                (other.intent_id != candidate.intent_id
                    || other.target_kind != candidate.target_kind)
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

    pub(crate) fn incomplete_receiver_intents(
        &self,
        page_id: PageId,
        path: &ManagedPath,
    ) -> Vec<ProjectionIntent> {
        self.incomplete_receiver
            .get(&(page_id, path.clone()))
            .map_or_else(Vec::new, |entries| entries.values().cloned().collect())
    }

    pub(crate) fn receiver_history_paths(&self) -> BTreeSet<AbsenceDecisionKey> {
        self.receiver_history.clone()
    }

    pub(crate) fn receiver_completion_anchors(&self) -> Vec<AbsenceCompletionAnchor> {
        self.receiver_completions
            .values()
            .flatten()
            .cloned()
            .collect()
    }
}

fn insert_completion(
    completions: &mut BTreeMap<AbsenceDecisionKey, Vec<AbsenceCompletionAnchor>>,
    key: AbsenceDecisionKey,
    anchor: AbsenceCompletionAnchor,
) {
    let entries = completions.entry(key).or_default();
    if !entries.iter().any(|entry| entry == &anchor) {
        entries.push(anchor);
    }
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
            map.decision(page_id, &path),
            AbsenceDecision::DeferredAbsence
        );

        let local_absent = intent(page_id, &path, 2, ProjectionTargetKind::Absent);
        map.record_local_completion(AbsenceCompletionAnchor::from_intent(&local_absent).unwrap());
        assert_eq!(map.decision(page_id, &path), AbsenceDecision::Create);

        let later_receiver_present = intent(page_id, &path, 3, ProjectionTargetKind::Present);
        map.record_receiver_completion(&later_receiver_present)
            .unwrap();
        assert_eq!(
            map.decision(page_id, &path),
            AbsenceDecision::DeferredAbsence
        );
    }

    #[test]
    fn no_completion_and_maximal_absent_both_create() {
        let page_id = PageId::from_uuid(Uuid::from_u128(0xc3_1020));
        let path = ManagedPath::parse("receiver-create.md").unwrap();
        let mut map = AbsenceDecisionMap::default();
        assert_eq!(map.decision(page_id, &path), AbsenceDecision::Create);
        let absent = intent(page_id, &path, 1, ProjectionTargetKind::Absent);
        map.record_receiver_completion(&absent).unwrap();
        assert_eq!(map.decision(page_id, &path), AbsenceDecision::Create);
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
        map.record_local_completion(AbsenceCompletionAnchor::from_intent(&absent).unwrap());
        assert_eq!(
            map.decision(page_id, &path),
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
        map.record_local_completion(AbsenceCompletionAnchor::from_intent(&local_absent).unwrap());
        assert_eq!(
            map.decision(page_id, &path),
            AbsenceDecision::DeferredAbsence,
            "target kind is not part of the intent id, so a cross-half collision must not overwrite"
        );
    }
}
