#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::object_store::StoreError;
use super::uuid_claim_index::SemanticIndexRoot;
use super::{
    BatchCausalDot, BatchId, ContentDigest, DocumentCausalDigest, DocumentDependencies, DocumentId,
    FrontierV2, LogicalPageName, PageDelta, PageId, PageNameKeyDigest, PageState,
    PAGE_NAME_KEY_VERSION,
};

pub const EXACT_LOGICAL_PAGE_NAME_BLOB_SCHEMA_VERSION: u32 = 1;
pub const EXACT_LOGICAL_PAGE_NAME_REF_SCHEMA_VERSION: u32 = 2;
pub const PAGE_NAME_OWNERSHIP_STORE_SCHEMA_VERSION: u32 = 2;
pub const PAGE_NAME_OWNERSHIP_RECORD_SCHEMA_VERSION: u32 = 2;
pub const PAGE_NAME_OWNERSHIP_ROOT_SCHEMA_VERSION: u32 = 2;
pub const PAGE_NAME_CATALOG_FRONTIER_SCHEMA_VERSION: u32 = 1;
pub const PAGE_NAME_CONFLICT_EVIDENCE_SCHEMA_VERSION: u32 = 1;
pub const MAX_PAGE_NAME_POINT_BATCH: usize = 100_000;
pub const MAX_PAGE_NAME_CONFLICT_PARTICIPANTS: usize = 100_000;
pub const MAX_PAGE_NAME_CONFLICT_EVIDENCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_EPHEMERAL_PAGE_NAME_RECORDS: usize = 4_096;

const MAX_EXACT_NAME_BLOB_BYTES: u64 = 4 * 1024 * 1024 + 1024;
const MAX_INLINE_EXACT_NAME_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct AuthoritativeCatalogPageNameObservationsV1 {
    entries: BTreeMap<PageId, Option<PageState>>,
}

impl AuthoritativeCatalogPageNameObservationsV1 {
    pub(crate) fn entries(&self) -> &BTreeMap<PageId, Option<PageState>> {
        &self.entries
    }
}

pub(crate) fn extract_authoritative_catalog_page_names(
    catalog_document_id: DocumentId,
    document: &loro::LoroDoc,
    requested_page_ids: &[PageId],
) -> Result<AuthoritativeCatalogPageNameObservationsV1, StoreError> {
    if requested_page_ids.len() > MAX_PAGE_NAME_POINT_BATCH {
        return Err(StoreError::PageNamePointBatchTooLarge {
            actual: requested_page_ids.len(),
            limit: MAX_PAGE_NAME_POINT_BATCH,
        });
    }
    if requested_page_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(StoreError::NonCanonicalPageNamePointKeys);
    }
    let validated = super::hot_engine::validate_catalog_document(catalog_document_id, document)
        .map_err(|_| StoreError::MalformedPageNameIndex)?;
    let entries = requested_page_ids
        .iter()
        .map(|page_id| {
            super::hot_engine::read_validated_catalog_page(validated, *page_id)
                .map(|state| (*page_id, state))
                .map_err(|_| StoreError::MalformedPageNameIndex)
        })
        .collect::<Result<_, _>>()?;
    Ok(AuthoritativeCatalogPageNameObservationsV1 { entries })
}

/// Select exact affected-page observations from an already validated semantic
/// effect. Local typed authoring derives this effect from the engine's current
/// catalog and its prospective document; re-decoding the complete graph-sized
/// catalog to recover the same bounded before/after rows is redundant work.
pub(crate) fn extract_semantic_page_name_observations(
    page_deltas: &[PageDelta],
    prospective: bool,
) -> Result<AuthoritativeCatalogPageNameObservationsV1, StoreError> {
    if page_deltas.len() > MAX_PAGE_NAME_POINT_BATCH {
        return Err(StoreError::PageNamePointBatchTooLarge {
            actual: page_deltas.len(),
            limit: MAX_PAGE_NAME_POINT_BATCH,
        });
    }
    if page_deltas
        .windows(2)
        .any(|pair| pair[0].page_id >= pair[1].page_id)
    {
        return Err(StoreError::NonCanonicalPageNamePointKeys);
    }
    let entries = page_deltas
        .iter()
        .map(|delta| {
            (
                delta.page_id,
                if prospective {
                    delta.after.clone()
                } else {
                    delta.before.clone()
                },
            )
        })
        .collect();
    Ok(AuthoritativeCatalogPageNameObservationsV1 { entries })
}

/// Select bounded affected-page observations from a catalog map that has
/// already passed complete replacement/catalog validation.
pub(crate) fn extract_validated_catalog_page_names(
    validated_pages: &BTreeMap<PageId, PageState>,
    requested_page_ids: &[PageId],
) -> Result<AuthoritativeCatalogPageNameObservationsV1, StoreError> {
    if requested_page_ids.len() > MAX_PAGE_NAME_POINT_BATCH {
        return Err(StoreError::PageNamePointBatchTooLarge {
            actual: requested_page_ids.len(),
            limit: MAX_PAGE_NAME_POINT_BATCH,
        });
    }
    if requested_page_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(StoreError::NonCanonicalPageNamePointKeys);
    }
    let entries = requested_page_ids
        .iter()
        .map(|page_id| (*page_id, validated_pages.get(page_id).cloned()))
        .collect();
    Ok(AuthoritativeCatalogPageNameObservationsV1 { entries })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageNameCollisionClassV1 {
    DifferentPagesSameCanonicalKey,
    DivergentCanonicalRename,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageNameReleaseFenceV1 {
    release_batch: BatchId,
    release_dot: BatchCausalDot,
}

impl PageNameReleaseFenceV1 {
    pub const fn release_batch(&self) -> BatchId {
        self.release_batch
    }

    pub const fn release_dot(&self) -> BatchCausalDot {
        self.release_dot
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageNameConflictParticipantV1 {
    page_id: PageId,
    exact_name: LogicalPageName,
    canonical_key: PageNameKeyDigest,
    acquisition_batch: BatchId,
    acquisition_dot: BatchCausalDot,
    exact_state_batch: BatchId,
    exact_state_dot: BatchCausalDot,
    release_fence: Option<PageNameReleaseFenceV1>,
    declared_frontier: FrontierV2,
}

impl PageNameConflictParticipantV1 {
    pub const fn page_id(&self) -> PageId {
        self.page_id
    }

    pub const fn exact_name(&self) -> &LogicalPageName {
        &self.exact_name
    }

    pub const fn canonical_key(&self) -> PageNameKeyDigest {
        self.canonical_key
    }

    pub const fn acquisition_batch(&self) -> BatchId {
        self.acquisition_batch
    }

    pub const fn acquisition_dot(&self) -> BatchCausalDot {
        self.acquisition_dot
    }

    pub const fn exact_state_batch(&self) -> BatchId {
        self.exact_state_batch
    }

    pub const fn exact_state_dot(&self) -> BatchCausalDot {
        self.exact_state_dot
    }

    pub const fn release_fence(&self) -> Option<&PageNameReleaseFenceV1> {
        self.release_fence.as_ref()
    }

    pub const fn declared_frontier(&self) -> &FrontierV2 {
        &self.declared_frontier
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageNameConflictEvidenceV1 {
    schema_version: u32,
    key_version: u32,
    collision_class: PageNameCollisionClassV1,
    canonical_keys: Vec<PageNameKeyDigest>,
    participants: Vec<PageNameConflictParticipantV1>,
}

impl PageNameConflictEvidenceV1 {
    fn new(
        collision_class: PageNameCollisionClassV1,
        mut participants: Vec<PageNameConflictParticipantV1>,
    ) -> Result<Self, StoreError> {
        if !(2..=MAX_PAGE_NAME_CONFLICT_PARTICIPANTS).contains(&participants.len()) {
            return Err(StoreError::MalformedPageNameIndex);
        }
        let mut ordered = participants
            .drain(..)
            .map(|participant| Ok((encode_canonical(&participant)?, participant)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        ordered.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        ordered.dedup_by(|left, right| left.0 == right.0);
        let participants = ordered
            .into_iter()
            .map(|(_, participant)| participant)
            .collect::<Vec<_>>();
        let mut canonical_keys = participants
            .iter()
            .map(PageNameConflictParticipantV1::canonical_key)
            .collect::<Vec<_>>();
        canonical_keys.sort_unstable();
        canonical_keys.dedup();
        let evidence = Self {
            schema_version: PAGE_NAME_CONFLICT_EVIDENCE_SCHEMA_VERSION,
            key_version: PAGE_NAME_KEY_VERSION,
            collision_class,
            canonical_keys,
            participants,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    pub const fn collision_class(&self) -> PageNameCollisionClassV1 {
        self.collision_class
    }

    pub fn canonical_keys(&self) -> &[PageNameKeyDigest] {
        &self.canonical_keys
    }

    pub fn participants(&self) -> &[PageNameConflictParticipantV1] {
        &self.participants
    }

    pub fn encode(&self) -> Result<Vec<u8>, StoreError> {
        self.validate()?;
        let bytes = encode_canonical(self)?;
        if bytes.len() > MAX_PAGE_NAME_CONFLICT_EVIDENCE_BYTES {
            return Err(StoreError::MalformedPageNameIndex);
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, StoreError> {
        if bytes.len() > MAX_PAGE_NAME_CONFLICT_EVIDENCE_BYTES {
            return Err(StoreError::MalformedPageNameIndex);
        }
        let evidence: Self = decode_canonical(bytes)?;
        evidence.validate()?;
        Ok(evidence)
    }

    pub fn digest(&self) -> Result<ContentDigest, StoreError> {
        Ok(ContentDigest::of(&self.encode()?))
    }

    fn validate(&self) -> Result<(), StoreError> {
        require_version(
            "page-name conflict evidence",
            self.schema_version,
            PAGE_NAME_CONFLICT_EVIDENCE_SCHEMA_VERSION,
        )?;
        require_version("page-name key", self.key_version, PAGE_NAME_KEY_VERSION)?;
        if !(2..=MAX_PAGE_NAME_CONFLICT_PARTICIPANTS).contains(&self.participants.len())
            || self.canonical_keys.is_empty()
            || self.canonical_keys.len() > self.participants.len()
            || self
                .canonical_keys
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(StoreError::MalformedPageNameIndex);
        }
        let participant_bytes = self
            .participants
            .iter()
            .map(encode_canonical)
            .collect::<Result<Vec<_>, StoreError>>()?;
        if participant_bytes.windows(2).any(|pair| pair[0] >= pair[1])
            || self.participants.iter().any(|participant| {
                participant.exact_name.key_digest() != participant.canonical_key
                    || self
                        .canonical_keys
                        .binary_search(&participant.canonical_key)
                        .is_err()
            })
        {
            return Err(StoreError::MalformedPageNameIndex);
        }
        match self.collision_class {
            PageNameCollisionClassV1::DifferentPagesSameCanonicalKey => {
                if self.canonical_keys.len() != 1
                    || self
                        .participants
                        .iter()
                        .map(PageNameConflictParticipantV1::page_id)
                        .collect::<BTreeSet<_>>()
                        .len()
                        < 2
                {
                    return Err(StoreError::MalformedPageNameIndex);
                }
            }
            PageNameCollisionClassV1::DivergentCanonicalRename => {
                if self.canonical_keys.len() < 2
                    || self
                        .participants
                        .iter()
                        .map(PageNameConflictParticipantV1::page_id)
                        .collect::<BTreeSet<_>>()
                        .len()
                        != 1
                {
                    return Err(StoreError::MalformedPageNameIndex);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum PageNameTransitionError {
    Store(StoreError),
    MalformedBatch(&'static str),
}

impl From<StoreError> for PageNameTransitionError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct EphemeralPageNameOwnershipStateV1 {
    records: BTreeMap<PageNameKeyDigest, PageNameOwnershipRecordV1>,
    exact_names: BTreeMap<(PageNameKeyDigest, ExactLogicalPageNameRefV1), LogicalPageName>,
}

#[derive(Debug)]
struct EphemeralPageNameOwnershipCandidateV1 {
    records: BTreeMap<PageNameKeyDigest, PageNameOwnershipRecordV1>,
    exact_names: BTreeMap<(PageNameKeyDigest, ExactLogicalPageNameRefV1), LogicalPageName>,
}

pub(crate) struct PageNamePublicationCandidateV1 {
    pub(crate) root: PageNameOwnershipRootV1,
    pub(crate) conflicts: Vec<PageNameConflictEvidenceV1>,
    ephemeral: Option<EphemeralPageNameOwnershipCandidateV1>,
}

/// Authenticated exact-name provenance for one occupied canonical key.
///
/// The fields stay private so callers cannot manufacture an exact-title
/// selection without reading it through an ownership root (or the bounded
/// no-store test authority).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AuthenticatedPageNameExactStateV1 {
    canonical_key: PageNameKeyDigest,
    page_id: PageId,
    exact_name: LogicalPageName,
    acquisition_batch: BatchId,
    exact_state_batch: BatchId,
    exact_state_dot: BatchCausalDot,
}

impl PageNamePublicationCandidateV1 {
    pub(crate) fn authenticated_ephemeral_exact_state(
        &self,
        prior: &EphemeralPageNameOwnershipStateV1,
        key: PageNameKeyDigest,
    ) -> Result<Option<AuthenticatedPageNameExactStateV1>, StoreError> {
        let record = self
            .ephemeral
            .as_ref()
            .and_then(|candidate| candidate.records.get(&key))
            .or_else(|| prior.records.get(&key));
        let Some(occupied) = record.and_then(PageNameOwnershipRecordV1::occupied) else {
            return Ok(None);
        };
        let lookup_key = (key, occupied.exact_name.clone());
        let exact_name = self
            .ephemeral
            .as_ref()
            .and_then(|candidate| candidate.exact_names.get(&lookup_key))
            .or_else(|| prior.exact_names.get(&lookup_key))
            .cloned()
            .ok_or(StoreError::MissingExactLogicalPageNameBlob(
                occupied.exact_name.content_digest,
            ))?;
        validate_exact_name_ref(key, &occupied.exact_name, &exact_name)?;
        Ok(Some(AuthenticatedPageNameExactStateV1 {
            canonical_key: key,
            page_id: occupied.page_id,
            exact_name,
            acquisition_batch: occupied.acquisition_batch,
            exact_state_batch: occupied.exact_state_batch,
            exact_state_dot: occupied.exact_state_dot,
        }))
    }
}

impl AuthenticatedPageNameExactStateV1 {
    pub(crate) const fn canonical_key(&self) -> PageNameKeyDigest {
        self.canonical_key
    }

    pub(crate) const fn page_id(&self) -> PageId {
        self.page_id
    }

    pub(crate) const fn exact_name(&self) -> &LogicalPageName {
        &self.exact_name
    }

    pub(crate) const fn exact_state_batch(&self) -> BatchId {
        self.exact_state_batch
    }

    pub(crate) const fn exact_state_dot(&self) -> BatchCausalDot {
        self.exact_state_dot
    }
}

impl EphemeralPageNameOwnershipStateV1 {
    pub(crate) fn resolve_current(&self, key: PageNameKeyDigest) -> Option<PageId> {
        self.records
            .get(&key)
            .and_then(PageNameOwnershipRecordV1::occupied)
            .map(PageNameOwnershipOccupiedV1::page_id)
    }

    pub(crate) fn commit(&mut self, candidate: PageNamePublicationCandidateV1) {
        let Some(candidate) = candidate.ephemeral else {
            return;
        };
        debug_assert!(candidate.records.len() <= MAX_PAGE_NAME_POINT_BATCH);
        for (key, record) in candidate.records {
            if let Some(prior) = self.records.insert(key, record) {
                if let Some(occupied) = prior.occupied {
                    self.exact_names.remove(&(key, occupied.exact_name));
                }
                if let Some(released) = prior.latest_release {
                    self.exact_names.remove(&(key, released.prior_exact_name));
                }
            }
        }
        self.exact_names.extend(candidate.exact_names);
    }

    pub(crate) fn authenticated_exact_state(
        &self,
        key: PageNameKeyDigest,
    ) -> Result<Option<AuthenticatedPageNameExactStateV1>, StoreError> {
        let Some(occupied) = self
            .records
            .get(&key)
            .and_then(PageNameOwnershipRecordV1::occupied)
        else {
            return Ok(None);
        };
        let exact_name = self
            .exact_names
            .get(&(key, occupied.exact_name.clone()))
            .cloned()
            .ok_or(StoreError::MissingExactLogicalPageNameBlob(
                occupied.exact_name.content_digest,
            ))?;
        validate_exact_name_ref(key, &occupied.exact_name, &exact_name)?;
        Ok(Some(AuthenticatedPageNameExactStateV1 {
            canonical_key: key,
            page_id: occupied.page_id,
            exact_name,
            acquisition_batch: occupied.acquisition_batch,
            exact_state_batch: occupied.exact_state_batch,
            exact_state_dot: occupied.exact_state_dot,
        }))
    }

    #[cfg(test)]
    pub(crate) fn record_count(&self) -> usize {
        self.records.len()
    }
}

trait PageNameTransitionAccess {
    fn lookup_many(
        &self,
        keys: &[PageNameKeyDigest],
    ) -> Result<BTreeMap<PageNameKeyDigest, PageNameOwnershipRecordV1>, StoreError>;

    fn read_exact_name(
        &self,
        expected_key: PageNameKeyDigest,
        name_ref: &ExactLogicalPageNameRefV1,
    ) -> Result<LogicalPageName, StoreError>;

    fn put_exact_name(
        &self,
        name: &LogicalPageName,
    ) -> Result<ExactLogicalPageNameRefV1, StoreError>;
}

struct EphemeralPageNameTransitionAccess<'a> {
    state: &'a EphemeralPageNameOwnershipStateV1,
    staged_exact_names: std::cell::RefCell<
        BTreeMap<(PageNameKeyDigest, ExactLogicalPageNameRefV1), LogicalPageName>,
    >,
}

impl PageNameTransitionAccess for EphemeralPageNameTransitionAccess<'_> {
    fn lookup_many(
        &self,
        keys: &[PageNameKeyDigest],
    ) -> Result<BTreeMap<PageNameKeyDigest, PageNameOwnershipRecordV1>, StoreError> {
        if keys.len() > MAX_PAGE_NAME_POINT_BATCH {
            return Err(StoreError::PageNamePointBatchTooLarge {
                actual: keys.len(),
                limit: MAX_PAGE_NAME_POINT_BATCH,
            });
        }
        if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(StoreError::NonCanonicalPageNamePointKeys);
        }
        Ok(keys
            .iter()
            .filter_map(|key| {
                self.state
                    .records
                    .get(key)
                    .cloned()
                    .map(|record| (*key, record))
            })
            .collect())
    }

    fn read_exact_name(
        &self,
        expected_key: PageNameKeyDigest,
        name_ref: &ExactLogicalPageNameRefV1,
    ) -> Result<LogicalPageName, StoreError> {
        let lookup_key = (expected_key, name_ref.clone());
        let name = self
            .staged_exact_names
            .borrow()
            .get(&lookup_key)
            .cloned()
            .or_else(|| self.state.exact_names.get(&lookup_key).cloned())
            .ok_or(StoreError::MissingExactLogicalPageNameBlob(
                name_ref.content_digest,
            ))?;
        validate_exact_name_ref(expected_key, name_ref, &name)?;
        Ok(name)
    }

    fn put_exact_name(
        &self,
        name: &LogicalPageName,
    ) -> Result<ExactLogicalPageNameRefV1, StoreError> {
        let (_, name_ref) = encode_exact_name_blob(name)?;
        self.staged_exact_names
            .borrow_mut()
            .insert((name.key_digest(), name_ref.clone()), name.clone());
        Ok(name_ref)
    }
}

struct PageNameTransitionCoreCandidateV1 {
    changed: BTreeMap<PageNameKeyDigest, PageNameOwnershipRecordV1>,
    conflicts: Vec<PageNameConflictEvidenceV1>,
}

#[allow(clippy::too_many_arguments)]
fn prepare_page_name_transition_core(
    access: &impl PageNameTransitionAccess,
    batch_id: BatchId,
    causal_dot: BatchCausalDot,
    declared_frontier: &FrontierV2,
    exact_before_pages: &BTreeMap<PageId, Option<PageState>>,
    deltas: &[PageDelta],
    current_pages: &BTreeMap<PageId, Option<PageState>>,
    prospective_pages: &BTreeMap<PageId, Option<PageState>>,
    contains: impl Fn(BatchCausalDot, BatchId) -> bool,
    frontier_for_batch: impl Fn(BatchId) -> Option<FrontierV2>,
) -> Result<PageNameTransitionCoreCandidateV1, PageNameTransitionError> {
    if deltas.len() > MAX_PAGE_NAME_POINT_BATCH {
        return Err(StoreError::PageNamePointBatchTooLarge {
            actual: deltas.len(),
            limit: MAX_PAGE_NAME_POINT_BATCH,
        }
        .into());
    }
    let affected = deltas
        .iter()
        .map(|delta| delta.page_id)
        .collect::<BTreeSet<_>>();
    if affected.len() != deltas.len()
        || affected.iter().any(|page_id| {
            !exact_before_pages.contains_key(page_id)
                || !current_pages.contains_key(page_id)
                || !prospective_pages.contains_key(page_id)
        })
    {
        return Err(PageNameTransitionError::MalformedBatch(
            "page-name transition observations are incomplete or non-unique",
        ));
    }
    for delta in deltas {
        if exact_before_pages[&delta.page_id].as_ref() != delta.before.as_ref() {
            return Err(PageNameTransitionError::MalformedBatch(
                "page-name transition disagrees with the authenticated dependency catalog",
            ));
        }
    }

    let mut keys = BTreeSet::new();
    for delta in deltas {
        for state in [
            delta.before.as_ref(),
            delta.after.as_ref(),
            current_pages[&delta.page_id].as_ref(),
            prospective_pages[&delta.page_id].as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            let key = state.name().key_digest();
            if keys.len() == MAX_PAGE_NAME_POINT_BATCH && !keys.contains(&key) {
                return Err(StoreError::PageNamePointBatchTooLarge {
                    actual: MAX_PAGE_NAME_POINT_BATCH + 1,
                    limit: MAX_PAGE_NAME_POINT_BATCH,
                }
                .into());
            }
            keys.insert(key);
        }
    }
    let requested = keys.into_iter().collect::<Vec<_>>();
    let mut records = access.lookup_many(&requested)?;

    let participant_for_occupied = |key: PageNameKeyDigest,
                                    occupied: &PageNameOwnershipOccupiedV1|
     -> Result<PageNameConflictParticipantV1, StoreError> {
        Ok(PageNameConflictParticipantV1 {
            page_id: occupied.page_id,
            exact_name: access.read_exact_name(key, &occupied.exact_name)?,
            canonical_key: key,
            acquisition_batch: occupied.acquisition_batch,
            acquisition_dot: occupied.acquisition_dot,
            exact_state_batch: occupied.exact_state_batch,
            exact_state_dot: occupied.exact_state_dot,
            release_fence: None,
            declared_frontier: frontier_for_batch(occupied.acquisition_batch)
                .ok_or(StoreError::MalformedPageNameIndex)?,
        })
    };
    let participant_for_release = |key: PageNameKeyDigest,
                                   released: &PageNameOwnershipReleasedV1|
     -> Result<PageNameConflictParticipantV1, StoreError> {
        Ok(PageNameConflictParticipantV1 {
            page_id: released.prior_page_id,
            exact_name: access.read_exact_name(key, &released.prior_exact_name)?,
            canonical_key: key,
            acquisition_batch: released.prior_acquisition_batch,
            acquisition_dot: released.prior_acquisition_dot,
            exact_state_batch: released.prior_exact_state_batch,
            exact_state_dot: released.prior_exact_state_dot,
            release_fence: Some(PageNameReleaseFenceV1 {
                release_batch: released.release_batch,
                release_dot: released.release_dot,
            }),
            declared_frontier: frontier_for_batch(released.prior_acquisition_batch)
                .ok_or(StoreError::MalformedPageNameIndex)?,
        })
    };
    let proposed_participant =
        |page_id: PageId, name: LogicalPageName| -> PageNameConflictParticipantV1 {
            PageNameConflictParticipantV1 {
                page_id,
                canonical_key: name.key_digest(),
                exact_name: name,
                acquisition_batch: batch_id,
                acquisition_dot: causal_dot,
                exact_state_batch: batch_id,
                exact_state_dot: causal_dot,
                release_fence: None,
                declared_frontier: declared_frontier.clone(),
            }
        };

    let mut conflicts = Vec::new();
    for delta in deltas {
        let (Some(before_name), Some(proposed_name), Some(current_name)) = (
            delta.before.as_ref().and_then(PageState::live_name),
            delta.after.as_ref().and_then(PageState::live_name),
            current_pages[&delta.page_id]
                .as_ref()
                .and_then(PageState::live_name),
        ) else {
            continue;
        };
        let before_key = before_name.key_digest();
        let proposed_key = proposed_name.key_digest();
        let current_key = current_name.key_digest();
        if proposed_key == before_key || current_key == before_key || current_key == proposed_key {
            continue;
        }
        let existing = records
            .get(&current_key)
            .and_then(PageNameOwnershipRecordV1::occupied)
            .filter(|occupied| occupied.page_id == delta.page_id)
            .ok_or(StoreError::MalformedPageNameIndex)?;
        if !contains(existing.acquisition_dot, existing.acquisition_batch) {
            conflicts.push(PageNameConflictEvidenceV1::new(
                PageNameCollisionClassV1::DivergentCanonicalRename,
                vec![
                    participant_for_occupied(current_key, existing)?,
                    proposed_participant(delta.page_id, proposed_name.clone()),
                ],
            )?);
        }
    }
    if !conflicts.is_empty() {
        conflicts.sort_unstable_by(|left, right| {
            left.encode()
                .expect("constructed page-name evidence remains canonical")
                .cmp(
                    &right
                        .encode()
                        .expect("constructed page-name evidence remains canonical"),
                )
        });
        conflicts.dedup();
        return Ok(PageNameTransitionCoreCandidateV1 {
            changed: BTreeMap::new(),
            conflicts,
        });
    }

    let mut changed = BTreeMap::new();
    for key in &requested {
        let Some(record) = records.get_mut(key) else {
            continue;
        };
        let Some(occupied) = record.occupied().cloned() else {
            continue;
        };
        if !affected.contains(&occupied.page_id) {
            continue;
        }
        let desired = prospective_pages[&occupied.page_id]
            .as_ref()
            .and_then(PageState::live_name);
        if desired.is_some_and(|name| name.key_digest() == *key) {
            continue;
        }
        let latest_release = PageNameOwnershipReleasedV1::new(
            occupied.page_id,
            occupied.exact_name,
            occupied.acquisition_batch,
            occupied.acquisition_dot,
            occupied.exact_state_batch,
            occupied.exact_state_dot,
            batch_id,
            causal_dot,
        );
        let replacement = PageNameOwnershipRecordV1::new(*key, None, Some(latest_release))?;
        *record = replacement.clone();
        changed.insert(*key, replacement);
    }

    let mut acquisitions = deltas
        .iter()
        .filter_map(|delta| {
            prospective_pages[&delta.page_id]
                .as_ref()
                .and_then(PageState::live_name)
                .map(|name| (name.key_digest(), delta.page_id, name.clone(), delta))
        })
        .collect::<Vec<_>>();
    acquisitions.sort_unstable_by(|left, right| {
        (left.0, left.1, left.2.as_str()).cmp(&(right.0, right.1, right.2.as_str()))
    });
    for pair in acquisitions.windows(2) {
        if pair[0].0 == pair[1].0 && pair[0].1 != pair[1].1 {
            return Err(PageNameTransitionError::MalformedBatch(
                "two PageIds acquire one canonical page-name key in the same batch",
            ));
        }
    }

    for (key, page_id, exact_name, delta) in acquisitions {
        if let Some(existing) = records
            .get(&key)
            .and_then(PageNameOwnershipRecordV1::occupied)
        {
            if existing.page_id == page_id {
                let existing_name = access.read_exact_name(key, &existing.exact_name)?;
                let proposed_exact_name = delta
                    .after
                    .as_ref()
                    .and_then(PageState::live_name)
                    .filter(|name| name.key_digest() == key);
                let title_bearing = delta.before.as_ref().and_then(PageState::live_name)
                    != delta.after.as_ref().and_then(PageState::live_name);
                let Some(proposed_exact_name) = proposed_exact_name.filter(|_| title_bearing)
                else {
                    continue;
                };
                if &existing_name == proposed_exact_name {
                    continue;
                }
                // A causally later exact-title event always wins. Concurrent
                // events use the lexicographically greatest immutable
                // (causal dot, batch id), independent of delivery order.
                let proposed_wins = contains(existing.exact_state_dot, existing.exact_state_batch)
                    || (causal_dot, batch_id)
                        > (existing.exact_state_dot, existing.exact_state_batch);
                if !proposed_wins {
                    continue;
                }
                let replacement = PageNameOwnershipRecordV1::new(
                    key,
                    Some(PageNameOwnershipOccupiedV1::new(
                        page_id,
                        access.put_exact_name(proposed_exact_name)?,
                        existing.acquisition_batch,
                        existing.acquisition_dot,
                        batch_id,
                        causal_dot,
                    )),
                    records
                        .get(&key)
                        .and_then(PageNameOwnershipRecordV1::latest_release)
                        .cloned(),
                )?;
                records.insert(key, replacement.clone());
                changed.insert(key, replacement);
                continue;
            }
            if contains(existing.acquisition_dot, existing.acquisition_batch) {
                return Err(PageNameTransitionError::MalformedBatch(
                    "canonical page-name key is occupied at the declared dependency frontier",
                ));
            }
            conflicts.push(PageNameConflictEvidenceV1::new(
                PageNameCollisionClassV1::DifferentPagesSameCanonicalKey,
                vec![
                    participant_for_occupied(key, existing)?,
                    proposed_participant(page_id, exact_name),
                ],
            )?);
            continue;
        }
        if let Some(released) = records
            .get(&key)
            .and_then(PageNameOwnershipRecordV1::latest_release)
            .filter(|release| {
                release.release_batch != batch_id
                    && !contains(release.release_dot, release.release_batch)
            })
        {
            conflicts.push(PageNameConflictEvidenceV1::new(
                PageNameCollisionClassV1::DifferentPagesSameCanonicalKey,
                vec![
                    participant_for_release(key, released)?,
                    proposed_participant(page_id, exact_name),
                ],
            )?);
            continue;
        }
        let latest_release = records
            .get(&key)
            .and_then(PageNameOwnershipRecordV1::latest_release)
            .cloned();
        let replacement = PageNameOwnershipRecordV1::new(
            key,
            Some(PageNameOwnershipOccupiedV1::new(
                page_id,
                access.put_exact_name(&exact_name)?,
                batch_id,
                causal_dot,
                batch_id,
                causal_dot,
            )),
            latest_release,
        )?;
        records.insert(key, replacement.clone());
        changed.insert(key, replacement);
    }

    if !conflicts.is_empty() {
        conflicts.sort_unstable_by(|left, right| {
            left.encode()
                .expect("constructed page-name evidence remains canonical")
                .cmp(
                    &right
                        .encode()
                        .expect("constructed page-name evidence remains canonical"),
                )
        });
        conflicts.dedup();
        return Ok(PageNameTransitionCoreCandidateV1 {
            changed: BTreeMap::new(),
            conflicts,
        });
    }
    Ok(PageNameTransitionCoreCandidateV1 { changed, conflicts })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_ephemeral_page_name_transition(
    state: &EphemeralPageNameOwnershipStateV1,
    batch_id: BatchId,
    causal_dot: BatchCausalDot,
    declared_frontier: &FrontierV2,
    exact_before: &AuthoritativeCatalogPageNameObservationsV1,
    deltas: &[PageDelta],
    current_pages: &BTreeMap<PageId, Option<PageState>>,
    prospective_pages: &BTreeMap<PageId, Option<PageState>>,
    contains: impl Fn(BatchCausalDot, BatchId) -> bool,
    frontier_for_batch: impl Fn(BatchId) -> Option<FrontierV2>,
) -> Result<PageNamePublicationCandidateV1, PageNameTransitionError> {
    let access = EphemeralPageNameTransitionAccess {
        state,
        staged_exact_names: std::cell::RefCell::new(BTreeMap::new()),
    };
    let candidate = prepare_page_name_transition_core(
        &access,
        batch_id,
        causal_dot,
        declared_frontier,
        &exact_before.entries,
        deltas,
        current_pages,
        prospective_pages,
        contains,
        frontier_for_batch,
    )?;
    let additions = candidate
        .changed
        .keys()
        .filter(|key| !state.records.contains_key(key))
        .count();
    if state.records.len().saturating_add(additions) > MAX_EPHEMERAL_PAGE_NAME_RECORDS {
        return Err(PageNameTransitionError::MalformedBatch(
            "no-store page-name test index reached its fixed capacity",
        ));
    }
    let ephemeral = if candidate.conflicts.is_empty() {
        let staged = access.staged_exact_names.into_inner();
        let required_exact_names = candidate
            .changed
            .iter()
            .flat_map(|(key, record)| {
                record
                    .occupied()
                    .map(|occupied| (*key, occupied.exact_name().clone()))
                    .into_iter()
                    .chain(
                        record
                            .latest_release()
                            .map(|released| (*key, released.prior_exact_name().clone())),
                    )
            })
            .collect::<BTreeSet<_>>();
        let exact_names = required_exact_names
            .into_iter()
            .map(|lookup_key| {
                let name = staged
                    .get(&lookup_key)
                    .cloned()
                    .or_else(|| state.exact_names.get(&lookup_key).cloned())
                    .ok_or(StoreError::MissingExactLogicalPageNameBlob(
                        lookup_key.1.content_digest,
                    ))?;
                Ok((lookup_key, name))
            })
            .collect::<Result<BTreeMap<_, _>, StoreError>>()?;
        Some(EphemeralPageNameOwnershipCandidateV1 {
            records: candidate.changed,
            exact_names,
        })
    } else {
        None
    };
    Ok(PageNamePublicationCandidateV1 {
        root: PageNameOwnershipRootV1::empty(),
        conflicts: candidate.conflicts,
        ephemeral,
    })
}

/// Digest of an exact, pre-canonicalization logical page name.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExactLogicalPageNameDigest([u8; 32]);

impl ExactLogicalPageNameDigest {
    pub fn of(name: &LogicalPageName) -> Self {
        let exact = name.as_str().as_bytes();
        let mut hasher = Sha256::new();
        hasher.update(b"tine/exact-logical-page-name/v1\0");
        hasher.update((exact.len() as u64).to_be_bytes());
        hasher.update(exact);
        Self(hasher.finalize().into())
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ExactLogicalPageNameDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExactLogicalPageNameBlobV1 {
    schema_version: u32,
    exact_name: LogicalPageName,
}

impl ExactLogicalPageNameBlobV1 {
    pub const fn exact_name(&self) -> &LogicalPageName {
        &self.exact_name
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExactLogicalPageNameRefV1 {
    schema_version: u32,
    encoded_len: u64,
    content_digest: ContentDigest,
    exact_name_digest: ExactLogicalPageNameDigest,
    #[serde(default)]
    inline_exact_name: Option<LogicalPageName>,
}

impl ExactLogicalPageNameRefV1 {
    pub const fn encoded_len(&self) -> u64 {
        self.encoded_len
    }

    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    pub const fn exact_name_digest(&self) -> ExactLogicalPageNameDigest {
        self.exact_name_digest
    }

    fn validate_version_and_length(&self) -> Result<(), StoreError> {
        require_version(
            "exact logical page-name reference",
            self.schema_version,
            EXACT_LOGICAL_PAGE_NAME_REF_SCHEMA_VERSION,
        )?;
        if self.encoded_len == 0 || self.encoded_len > MAX_EXACT_NAME_BLOB_BYTES {
            return Err(StoreError::MalformedPageNameIndex);
        }
        Ok(())
    }
}

fn encode_exact_name_blob(
    name: &LogicalPageName,
) -> Result<(Vec<u8>, ExactLogicalPageNameRefV1), StoreError> {
    let blob = ExactLogicalPageNameBlobV1 {
        schema_version: EXACT_LOGICAL_PAGE_NAME_BLOB_SCHEMA_VERSION,
        exact_name: name.clone(),
    };
    let bytes = encode_canonical(&blob)?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_EXACT_NAME_BLOB_BYTES {
        return Err(StoreError::MalformedPageNameIndex);
    }
    Ok((
        bytes.clone(),
        ExactLogicalPageNameRefV1 {
            schema_version: EXACT_LOGICAL_PAGE_NAME_REF_SCHEMA_VERSION,
            encoded_len: bytes.len() as u64,
            content_digest: ContentDigest::of(&bytes),
            exact_name_digest: ExactLogicalPageNameDigest::of(name),
            inline_exact_name: (bytes.len() <= MAX_INLINE_EXACT_NAME_BYTES).then(|| name.clone()),
        },
    ))
}

fn validate_exact_name_ref(
    expected_key: PageNameKeyDigest,
    name_ref: &ExactLogicalPageNameRefV1,
    name: &LogicalPageName,
) -> Result<(), StoreError> {
    name_ref.validate_version_and_length()?;
    let (bytes, expected_ref) = encode_exact_name_blob(name)?;
    if bytes.len() as u64 != name_ref.encoded_len
        || expected_ref.content_digest != name_ref.content_digest
        || expected_ref.exact_name_digest != name_ref.exact_name_digest
        || name.key_digest() != expected_key
    {
        return Err(StoreError::MalformedPageNameIndex);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageNameOwnershipOccupiedV1 {
    page_id: PageId,
    exact_name: ExactLogicalPageNameRefV1,
    acquisition_batch: BatchId,
    acquisition_dot: BatchCausalDot,
    exact_state_batch: BatchId,
    exact_state_dot: BatchCausalDot,
}

impl PageNameOwnershipOccupiedV1 {
    pub const fn new(
        page_id: PageId,
        exact_name: ExactLogicalPageNameRefV1,
        acquisition_batch: BatchId,
        acquisition_dot: BatchCausalDot,
        exact_state_batch: BatchId,
        exact_state_dot: BatchCausalDot,
    ) -> Self {
        Self {
            page_id,
            exact_name,
            acquisition_batch,
            acquisition_dot,
            exact_state_batch,
            exact_state_dot,
        }
    }

    pub const fn page_id(&self) -> PageId {
        self.page_id
    }

    pub const fn exact_name(&self) -> &ExactLogicalPageNameRefV1 {
        &self.exact_name
    }

    pub const fn acquisition_batch(&self) -> BatchId {
        self.acquisition_batch
    }

    pub const fn acquisition_dot(&self) -> BatchCausalDot {
        self.acquisition_dot
    }

    pub const fn exact_state_batch(&self) -> BatchId {
        self.exact_state_batch
    }

    pub const fn exact_state_dot(&self) -> BatchCausalDot {
        self.exact_state_dot
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageNameOwnershipReleasedV1 {
    prior_page_id: PageId,
    prior_exact_name: ExactLogicalPageNameRefV1,
    prior_acquisition_batch: BatchId,
    prior_acquisition_dot: BatchCausalDot,
    prior_exact_state_batch: BatchId,
    prior_exact_state_dot: BatchCausalDot,
    release_batch: BatchId,
    release_dot: BatchCausalDot,
}

impl PageNameOwnershipReleasedV1 {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        prior_page_id: PageId,
        prior_exact_name: ExactLogicalPageNameRefV1,
        prior_acquisition_batch: BatchId,
        prior_acquisition_dot: BatchCausalDot,
        prior_exact_state_batch: BatchId,
        prior_exact_state_dot: BatchCausalDot,
        release_batch: BatchId,
        release_dot: BatchCausalDot,
    ) -> Self {
        Self {
            prior_page_id,
            prior_exact_name,
            prior_acquisition_batch,
            prior_acquisition_dot,
            prior_exact_state_batch,
            prior_exact_state_dot,
            release_batch,
            release_dot,
        }
    }

    pub const fn prior_page_id(&self) -> PageId {
        self.prior_page_id
    }

    pub const fn prior_exact_name(&self) -> &ExactLogicalPageNameRefV1 {
        &self.prior_exact_name
    }

    pub const fn prior_acquisition_batch(&self) -> BatchId {
        self.prior_acquisition_batch
    }

    pub const fn prior_acquisition_dot(&self) -> BatchCausalDot {
        self.prior_acquisition_dot
    }

    pub const fn prior_exact_state_batch(&self) -> BatchId {
        self.prior_exact_state_batch
    }

    pub const fn prior_exact_state_dot(&self) -> BatchCausalDot {
        self.prior_exact_state_dot
    }

    pub const fn release_batch(&self) -> BatchId {
        self.release_batch
    }

    pub const fn release_dot(&self) -> BatchCausalDot {
        self.release_dot
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageNameOwnershipRecordV1 {
    schema_version: u32,
    key_version: u32,
    key_digest: PageNameKeyDigest,
    occupied: Option<PageNameOwnershipOccupiedV1>,
    latest_release: Option<PageNameOwnershipReleasedV1>,
}

impl PageNameOwnershipRecordV1 {
    pub fn new(
        key_digest: PageNameKeyDigest,
        occupied: Option<PageNameOwnershipOccupiedV1>,
        latest_release: Option<PageNameOwnershipReleasedV1>,
    ) -> Result<Self, StoreError> {
        let record = Self {
            schema_version: PAGE_NAME_OWNERSHIP_RECORD_SCHEMA_VERSION,
            key_version: PAGE_NAME_KEY_VERSION,
            key_digest,
            occupied,
            latest_release,
        };
        record.validate_shape(key_digest)?;
        Ok(record)
    }

    pub const fn key_digest(&self) -> PageNameKeyDigest {
        self.key_digest
    }

    pub const fn occupied(&self) -> Option<&PageNameOwnershipOccupiedV1> {
        self.occupied.as_ref()
    }

    pub const fn latest_release(&self) -> Option<&PageNameOwnershipReleasedV1> {
        self.latest_release.as_ref()
    }

    fn validate_shape(&self, expected_key: PageNameKeyDigest) -> Result<(), StoreError> {
        require_version(
            "page-name ownership record",
            self.schema_version,
            PAGE_NAME_OWNERSHIP_RECORD_SCHEMA_VERSION,
        )?;
        require_version("page-name key", self.key_version, PAGE_NAME_KEY_VERSION)?;
        if self.key_digest != expected_key
            || (self.occupied.is_none() && self.latest_release.is_none())
        {
            return Err(StoreError::MalformedPageNameIndex);
        }
        if let Some(occupied) = &self.occupied {
            occupied.exact_name.validate_version_and_length()?;
        }
        if let Some(released) = &self.latest_release {
            released.prior_exact_name.validate_version_and_length()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageNameOwnershipRootV1 {
    schema_version: u32,
    key_version: u32,
    semantic_root: SemanticIndexRoot,
    entry_count: u64,
}

impl PageNameOwnershipRootV1 {
    pub fn empty() -> Self {
        Self {
            schema_version: PAGE_NAME_OWNERSHIP_ROOT_SCHEMA_VERSION,
            key_version: PAGE_NAME_KEY_VERSION,
            semantic_root: SemanticIndexRoot::empty(),
            entry_count: 0,
        }
    }

    pub const fn entry_count(&self) -> u64 {
        self.entry_count
    }

    pub const fn semantic_digest(&self) -> ContentDigest {
        self.semantic_root.digest()
    }

    pub fn external_digest(&self) -> Result<ContentDigest, StoreError> {
        self.validate_version_and_shape()?;
        let encoded = encode_canonical(self)?;
        let mut bytes = b"tine/page-name-ownership-root/v1\0".to_vec();
        bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&encoded);
        Ok(ContentDigest::of(&bytes))
    }

    pub fn encode(&self) -> Result<Vec<u8>, StoreError> {
        self.validate_version_and_shape()?;
        encode_canonical(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, StoreError> {
        let root: Self = decode_canonical(bytes)?;
        root.validate_version_and_shape()?;
        Ok(root)
    }

    fn validate_version_and_shape(&self) -> Result<(), StoreError> {
        require_version(
            "page-name ownership root",
            self.schema_version,
            PAGE_NAME_OWNERSHIP_ROOT_SCHEMA_VERSION,
        )?;
        require_version("page-name key", self.key_version, PAGE_NAME_KEY_VERSION)?;
        if (self.entry_count == 0) != (self.semantic_root == SemanticIndexRoot::empty()) {
            return Err(StoreError::MalformedPageNameIndex);
        }
        Ok(())
    }
}

impl Default for PageNameOwnershipRootV1 {
    fn default() -> Self {
        Self::empty()
    }
}

trait LivePageState {
    fn live_name(&self) -> Option<&LogicalPageName>;
}

impl LivePageState for PageState {
    fn live_name(&self) -> Option<&LogicalPageName> {
        match self {
            Self::Live { name, .. } => Some(name),
            Self::Tombstone { .. } => None,
        }
    }
}

fn encode_canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    postcard::to_allocvec(value).map_err(|_| StoreError::MalformedPageNameIndex)
}

fn decode_canonical<T: for<'de> Deserialize<'de> + Serialize>(
    bytes: &[u8],
) -> Result<T, StoreError> {
    let value: T = postcard::from_bytes(bytes).map_err(|_| StoreError::MalformedPageNameIndex)?;
    if encode_canonical(&value)? != bytes {
        return Err(StoreError::MalformedPageNameIndex);
    }
    Ok(value)
}

fn require_version(store: &'static str, found: u32, current: u32) -> Result<(), StoreError> {
    if found < current {
        return Err(StoreError::UpgradeRequired {
            store,
            found,
            current,
        });
    }
    if found > current {
        return Err(StoreError::UnsupportedStoreVersion {
            store,
            version: found,
        });
    }
    Ok(())
}
