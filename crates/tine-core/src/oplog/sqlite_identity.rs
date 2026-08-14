//! Application-owned causal identity records stored in the disposable SQLite
//! projection.
//!
//! These values intentionally contain the exact semantic value inline.  They
//! are complete point records: unlike the retired Patricia representation,
//! decoding one never requires opening a content-addressed side blob.  The
//! origin distinguishes immutable activation facts from ordinary accepted
//! operations without fabricating a bootstrap batch or causal dot.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    BatchCausalDot, BatchId, LogicalPageName, ManagedPath, PageDelta, PageId, PageNameKeyDigest,
    PageState, PortablePathKeyDigest,
};

const PAGE_NAME_IDENTITY_RECORD_SCHEMA_VERSION: u32 = 1;
const PORTABLE_PATH_IDENTITY_RECORD_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum IdentityOriginV1 {
    Baseline,
    Accepted {
        batch_id: BatchId,
        causal_dot: BatchCausalDot,
    },
}

impl IdentityOriginV1 {
    pub(crate) const fn accepted(batch_id: BatchId, causal_dot: BatchCausalDot) -> Self {
        Self::Accepted {
            batch_id,
            causal_dot,
        }
    }

    pub(crate) const fn batch_id(self) -> Option<BatchId> {
        match self {
            Self::Baseline => None,
            Self::Accepted { batch_id, .. } => Some(batch_id),
        }
    }

    pub(crate) const fn causal_dot(self) -> Option<BatchCausalDot> {
        match self {
            Self::Baseline => None,
            Self::Accepted { causal_dot, .. } => Some(causal_dot),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageNameIdentityOccupiedV1 {
    page_id: PageId,
    exact_name: LogicalPageName,
    acquisition: IdentityOriginV1,
    exact_state: IdentityOriginV1,
}

impl PageNameIdentityOccupiedV1 {
    pub(crate) fn new(
        page_id: PageId,
        exact_name: LogicalPageName,
        acquisition: IdentityOriginV1,
        exact_state: IdentityOriginV1,
    ) -> Self {
        Self {
            page_id,
            exact_name,
            acquisition,
            exact_state,
        }
    }

    pub(crate) const fn page_id(&self) -> PageId {
        self.page_id
    }

    pub(crate) const fn exact_name(&self) -> &LogicalPageName {
        &self.exact_name
    }

    pub(crate) const fn acquisition(&self) -> IdentityOriginV1 {
        self.acquisition
    }

    pub(crate) const fn exact_state(&self) -> IdentityOriginV1 {
        self.exact_state
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageNameIdentityReleasedV1 {
    prior_page_id: PageId,
    prior_exact_name: LogicalPageName,
    prior_acquisition: IdentityOriginV1,
    prior_exact_state: IdentityOriginV1,
    release: IdentityOriginV1,
}

impl PageNameIdentityReleasedV1 {
    pub(crate) fn new(
        prior_page_id: PageId,
        prior_exact_name: LogicalPageName,
        prior_acquisition: IdentityOriginV1,
        prior_exact_state: IdentityOriginV1,
        release: IdentityOriginV1,
    ) -> Self {
        Self {
            prior_page_id,
            prior_exact_name,
            prior_acquisition,
            prior_exact_state,
            release,
        }
    }

    pub(crate) const fn prior_page_id(&self) -> PageId {
        self.prior_page_id
    }

    pub(crate) const fn prior_exact_name(&self) -> &LogicalPageName {
        &self.prior_exact_name
    }

    pub(crate) const fn prior_acquisition(&self) -> IdentityOriginV1 {
        self.prior_acquisition
    }

    pub(crate) const fn prior_exact_state(&self) -> IdentityOriginV1 {
        self.prior_exact_state
    }

    pub(crate) const fn release(&self) -> IdentityOriginV1 {
        self.release
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageNameIdentityRecordV1 {
    schema_version: u32,
    key_digest: PageNameKeyDigest,
    occupied: Option<PageNameIdentityOccupiedV1>,
    latest_release: Option<PageNameIdentityReleasedV1>,
}

impl PageNameIdentityRecordV1 {
    pub(crate) fn baseline(page_id: PageId, exact_name: LogicalPageName) -> Result<Self, String> {
        let key_digest = exact_name.key_digest();
        Self::new(
            key_digest,
            Some(PageNameIdentityOccupiedV1::new(
                page_id,
                exact_name,
                IdentityOriginV1::Baseline,
                IdentityOriginV1::Baseline,
            )),
            None,
        )
    }

    pub(crate) fn new(
        key_digest: PageNameKeyDigest,
        occupied: Option<PageNameIdentityOccupiedV1>,
        latest_release: Option<PageNameIdentityReleasedV1>,
    ) -> Result<Self, String> {
        let record = Self {
            schema_version: PAGE_NAME_IDENTITY_RECORD_SCHEMA_VERSION,
            key_digest,
            occupied,
            latest_release,
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) const fn key_digest(&self) -> PageNameKeyDigest {
        self.key_digest
    }

    pub(crate) const fn occupied(&self) -> Option<&PageNameIdentityOccupiedV1> {
        self.occupied.as_ref()
    }

    pub(crate) const fn latest_release(&self) -> Option<&PageNameIdentityReleasedV1> {
        self.latest_release.as_ref()
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        postcard::to_allocvec(self).map_err(|error| error.to_string())
    }

    pub(crate) fn decode(expected_key: PageNameKeyDigest, bytes: &[u8]) -> Result<Self, String> {
        let record: Self = postcard::from_bytes(bytes).map_err(|error| error.to_string())?;
        record.validate()?;
        if record.key_digest != expected_key
            || postcard::to_allocvec(&record).map_err(|error| error.to_string())? != bytes
        {
            return Err("page-name identity record is not canonically bound to its key".into());
        }
        Ok(record)
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema_version != PAGE_NAME_IDENTITY_RECORD_SCHEMA_VERSION
            || (self.occupied.is_none() && self.latest_release.is_none())
            || self
                .occupied
                .as_ref()
                .is_some_and(|occupied| occupied.exact_name.key_digest() != self.key_digest)
            || self.latest_release.as_ref().is_some_and(|released| {
                released.prior_exact_name.key_digest() != self.key_digest
                    || matches!(released.release, IdentityOriginV1::Baseline)
            })
        {
            return Err("malformed page-name identity record".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortablePathIdentityOccupiedV1 {
    page_id: PageId,
    exact_path: ManagedPath,
    acquisition: IdentityOriginV1,
}

impl PortablePathIdentityOccupiedV1 {
    pub(crate) fn new(
        page_id: PageId,
        exact_path: ManagedPath,
        acquisition: IdentityOriginV1,
    ) -> Self {
        Self {
            page_id,
            exact_path,
            acquisition,
        }
    }

    pub(crate) const fn page_id(&self) -> PageId {
        self.page_id
    }

    pub(crate) const fn exact_path(&self) -> &ManagedPath {
        &self.exact_path
    }

    pub(crate) const fn acquisition(&self) -> IdentityOriginV1 {
        self.acquisition
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortablePathIdentityReleasedV1 {
    prior_page_id: PageId,
    prior_exact_path: ManagedPath,
    prior_acquisition: IdentityOriginV1,
    release: IdentityOriginV1,
}

impl PortablePathIdentityReleasedV1 {
    pub(crate) fn new(
        prior_page_id: PageId,
        prior_exact_path: ManagedPath,
        prior_acquisition: IdentityOriginV1,
        release: IdentityOriginV1,
    ) -> Self {
        Self {
            prior_page_id,
            prior_exact_path,
            prior_acquisition,
            release,
        }
    }

    pub(crate) const fn prior_page_id(&self) -> PageId {
        self.prior_page_id
    }

    pub(crate) const fn prior_exact_path(&self) -> &ManagedPath {
        &self.prior_exact_path
    }

    pub(crate) const fn prior_acquisition(&self) -> IdentityOriginV1 {
        self.prior_acquisition
    }

    pub(crate) const fn release(&self) -> IdentityOriginV1 {
        self.release
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PortablePathIdentityRecordV1 {
    schema_version: u32,
    key_digest: PortablePathKeyDigest,
    occupied: Option<PortablePathIdentityOccupiedV1>,
    latest_release: Option<PortablePathIdentityReleasedV1>,
}

impl PortablePathIdentityRecordV1 {
    pub(crate) fn baseline(page_id: PageId, exact_path: ManagedPath) -> Result<Self, String> {
        let key_digest = exact_path.portable_key().digest();
        Self::new(
            key_digest,
            Some(PortablePathIdentityOccupiedV1::new(
                page_id,
                exact_path,
                IdentityOriginV1::Baseline,
            )),
            None,
        )
    }

    pub(crate) fn new(
        key_digest: PortablePathKeyDigest,
        occupied: Option<PortablePathIdentityOccupiedV1>,
        latest_release: Option<PortablePathIdentityReleasedV1>,
    ) -> Result<Self, String> {
        let record = Self {
            schema_version: PORTABLE_PATH_IDENTITY_RECORD_SCHEMA_VERSION,
            key_digest,
            occupied,
            latest_release,
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) const fn key_digest(&self) -> PortablePathKeyDigest {
        self.key_digest
    }

    pub(crate) const fn occupied(&self) -> Option<&PortablePathIdentityOccupiedV1> {
        self.occupied.as_ref()
    }

    pub(crate) const fn latest_release(&self) -> Option<&PortablePathIdentityReleasedV1> {
        self.latest_release.as_ref()
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, String> {
        self.validate()?;
        postcard::to_allocvec(self).map_err(|error| error.to_string())
    }

    pub(crate) fn decode(
        expected_key: PortablePathKeyDigest,
        bytes: &[u8],
    ) -> Result<Self, String> {
        let record: Self = postcard::from_bytes(bytes).map_err(|error| error.to_string())?;
        record.validate()?;
        if record.key_digest != expected_key
            || postcard::to_allocvec(&record).map_err(|error| error.to_string())? != bytes
        {
            return Err("portable-path identity record is not canonically bound to its key".into());
        }
        Ok(record)
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema_version != PORTABLE_PATH_IDENTITY_RECORD_SCHEMA_VERSION
            || (self.occupied.is_none() && self.latest_release.is_none())
            || self.occupied.as_ref().is_some_and(|occupied| {
                occupied.exact_path.portable_key().digest() != self.key_digest
            })
            || self.latest_release.as_ref().is_some_and(|released| {
                released.prior_exact_path.portable_key().digest() != self.key_digest
                    || matches!(released.release, IdentityOriginV1::Baseline)
            })
        {
            return Err("malformed portable-path identity record".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PageNameIdentityCollisionClass {
    DifferentPagesSameCanonicalKey,
    DivergentCanonicalRename,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PageNameIdentityConflict {
    pub(crate) class: PageNameIdentityCollisionClass,
    pub(crate) key: PageNameKeyDigest,
    pub(crate) page_ids: Vec<PageId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PortablePathIdentityConflict {
    pub(crate) key: PortablePathKeyDigest,
    pub(crate) page_ids: Vec<PageId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PageNameIdentityTransition {
    pub(crate) changed: BTreeMap<PageNameKeyDigest, PageNameIdentityRecordV1>,
    pub(crate) conflicts: Vec<PageNameIdentityConflict>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PortablePathIdentityTransition {
    pub(crate) changed: BTreeMap<PortablePathKeyDigest, PortablePathIdentityRecordV1>,
    pub(crate) conflicts: Vec<PortablePathIdentityConflict>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum IdentityTransitionError {
    Malformed(String),
    Occupied(String),
}

fn origin_is_contained(
    origin: IdentityOriginV1,
    candidate_batch: BatchId,
    contains: &impl Fn(BatchCausalDot, BatchId) -> bool,
) -> bool {
    match origin {
        IdentityOriginV1::Baseline => true,
        IdentityOriginV1::Accepted {
            batch_id,
            causal_dot,
        } => batch_id == candidate_batch || contains(causal_dot, batch_id),
    }
}

fn live_page_name(state: &PageState) -> Option<&LogicalPageName> {
    match state {
        PageState::Live { name, .. } => Some(name),
        PageState::Tombstone { .. } => None,
    }
}

/// Compute the complete page-name point-record delta from the projection at
/// frontier F. The function performs no I/O and publishes nothing, so its
/// result may be prepared before the semantic operation becomes durable and
/// applied to SQLite only afterwards at F+1.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_page_name_identity_transition(
    batch_id: BatchId,
    causal_dot: BatchCausalDot,
    exact_before_pages: &BTreeMap<PageId, Option<PageState>>,
    deltas: &[PageDelta],
    current_pages: &BTreeMap<PageId, Option<PageState>>,
    prospective_pages: &BTreeMap<PageId, Option<PageState>>,
    mut records: BTreeMap<PageNameKeyDigest, PageNameIdentityRecordV1>,
    contains: impl Fn(BatchCausalDot, BatchId) -> bool,
) -> Result<PageNameIdentityTransition, IdentityTransitionError> {
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
        || deltas
            .iter()
            .any(|delta| exact_before_pages[&delta.page_id].as_ref() != delta.before.as_ref())
    {
        return Err(IdentityTransitionError::Malformed(
            "page-name transition observations are incomplete or inconsistent".into(),
        ));
    }
    for (key, record) in &records {
        record
            .validate()
            .map_err(IdentityTransitionError::Malformed)?;
        if record.key_digest() != *key {
            return Err(IdentityTransitionError::Malformed(
                "page-name record is stored under another key".into(),
            ));
        }
    }

    let proposed_origin = IdentityOriginV1::accepted(batch_id, causal_dot);
    let mut conflicts = Vec::new();
    for delta in deltas {
        let (Some(before_name), Some(proposed_name), Some(current_name)) = (
            delta.before.as_ref().and_then(live_page_name),
            delta.after.as_ref().and_then(live_page_name),
            current_pages[&delta.page_id]
                .as_ref()
                .and_then(live_page_name),
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
            .and_then(PageNameIdentityRecordV1::occupied)
            .filter(|occupied| occupied.page_id() == delta.page_id)
            .ok_or_else(|| {
                IdentityTransitionError::Malformed(
                    "current exact title has no matching SQLite owner".into(),
                )
            })?;
        if !origin_is_contained(existing.acquisition(), batch_id, &contains) {
            conflicts.push(PageNameIdentityConflict {
                class: PageNameIdentityCollisionClass::DivergentCanonicalRename,
                key: proposed_key,
                page_ids: vec![delta.page_id],
            });
        }
    }
    if !conflicts.is_empty() {
        return Ok(PageNameIdentityTransition {
            changed: BTreeMap::new(),
            conflicts,
        });
    }

    let mut changed = BTreeMap::new();
    let existing_keys = records.keys().copied().collect::<Vec<_>>();
    for key in existing_keys {
        let Some(occupied) = records
            .get(&key)
            .and_then(PageNameIdentityRecordV1::occupied)
            .cloned()
        else {
            continue;
        };
        if !affected.contains(&occupied.page_id()) {
            continue;
        }
        let desired = prospective_pages[&occupied.page_id()]
            .as_ref()
            .and_then(live_page_name);
        if desired.is_some_and(|name| name.key_digest() == key) {
            continue;
        }
        let replacement = PageNameIdentityRecordV1::new(
            key,
            None,
            Some(PageNameIdentityReleasedV1::new(
                occupied.page_id(),
                occupied.exact_name().clone(),
                occupied.acquisition(),
                occupied.exact_state(),
                proposed_origin,
            )),
        )
        .map_err(IdentityTransitionError::Malformed)?;
        records.insert(key, replacement.clone());
        changed.insert(key, replacement);
    }

    let mut acquisitions = deltas
        .iter()
        .filter_map(|delta| {
            prospective_pages[&delta.page_id]
                .as_ref()
                .and_then(live_page_name)
                .map(|name| (name.key_digest(), delta.page_id, name.clone(), delta))
        })
        .collect::<Vec<_>>();
    acquisitions.sort_unstable_by(|left, right| {
        (left.0, left.1, left.2.as_str()).cmp(&(right.0, right.1, right.2.as_str()))
    });
    for pair in acquisitions.windows(2) {
        if pair[0].0 == pair[1].0 && pair[0].1 != pair[1].1 {
            return Err(IdentityTransitionError::Malformed(
                "two pages acquire one canonical page-name key in one operation".into(),
            ));
        }
    }

    for (key, page_id, exact_name, delta) in acquisitions {
        if let Some(existing) = records
            .get(&key)
            .and_then(PageNameIdentityRecordV1::occupied)
            .cloned()
        {
            if existing.page_id() == page_id {
                let title_bearing = delta.before.as_ref().and_then(live_page_name)
                    != delta.after.as_ref().and_then(live_page_name);
                if !title_bearing || existing.exact_name() == &exact_name {
                    continue;
                }
                let proposed_wins =
                    origin_is_contained(existing.exact_state(), batch_id, &contains)
                        || proposed_origin > existing.exact_state();
                if !proposed_wins {
                    continue;
                }
                let replacement = PageNameIdentityRecordV1::new(
                    key,
                    Some(PageNameIdentityOccupiedV1::new(
                        page_id,
                        exact_name,
                        existing.acquisition(),
                        proposed_origin,
                    )),
                    records
                        .get(&key)
                        .and_then(PageNameIdentityRecordV1::latest_release)
                        .cloned(),
                )
                .map_err(IdentityTransitionError::Malformed)?;
                records.insert(key, replacement.clone());
                changed.insert(key, replacement);
                continue;
            }
            if origin_is_contained(existing.acquisition(), batch_id, &contains) {
                return Err(IdentityTransitionError::Occupied(
                    "canonical page-name key is occupied at frontier F".into(),
                ));
            }
            conflicts.push(PageNameIdentityConflict {
                class: PageNameIdentityCollisionClass::DifferentPagesSameCanonicalKey,
                key,
                page_ids: vec![existing.page_id(), page_id],
            });
            continue;
        }
        if let Some(released) = records
            .get(&key)
            .and_then(PageNameIdentityRecordV1::latest_release)
            .filter(|released| !origin_is_contained(released.release(), batch_id, &contains))
        {
            conflicts.push(PageNameIdentityConflict {
                class: PageNameIdentityCollisionClass::DifferentPagesSameCanonicalKey,
                key,
                page_ids: vec![released.prior_page_id(), page_id],
            });
            continue;
        }
        let latest_release = records
            .get(&key)
            .and_then(PageNameIdentityRecordV1::latest_release)
            .cloned();
        let replacement = PageNameIdentityRecordV1::new(
            key,
            Some(PageNameIdentityOccupiedV1::new(
                page_id,
                exact_name,
                proposed_origin,
                proposed_origin,
            )),
            latest_release,
        )
        .map_err(IdentityTransitionError::Malformed)?;
        records.insert(key, replacement.clone());
        changed.insert(key, replacement);
    }

    if !conflicts.is_empty() {
        return Ok(PageNameIdentityTransition {
            changed: BTreeMap::new(),
            conflicts,
        });
    }
    Ok(PageNameIdentityTransition { changed, conflicts })
}

/// Portable-path counterpart of [`prepare_page_name_identity_transition`].
/// Exact paths and their causal fences are complete in each SQLite point row.
pub(crate) fn prepare_portable_path_identity_transition(
    batch_id: BatchId,
    causal_dot: BatchCausalDot,
    deltas: &[PageDelta],
    current_pages: &BTreeMap<PageId, Option<PageState>>,
    prospective_pages: &BTreeMap<PageId, Option<PageState>>,
    mut records: BTreeMap<PortablePathKeyDigest, PortablePathIdentityRecordV1>,
    contains: impl Fn(BatchCausalDot, BatchId) -> bool,
) -> Result<PortablePathIdentityTransition, IdentityTransitionError> {
    let affected = deltas
        .iter()
        .map(|delta| delta.page_id)
        .collect::<BTreeSet<_>>();
    if affected.len() != deltas.len()
        || affected.iter().any(|page_id| {
            !current_pages.contains_key(page_id) || !prospective_pages.contains_key(page_id)
        })
    {
        return Err(IdentityTransitionError::Malformed(
            "portable-path transition observations are incomplete".into(),
        ));
    }
    for (key, record) in &records {
        record
            .validate()
            .map_err(IdentityTransitionError::Malformed)?;
        if record.key_digest() != *key {
            return Err(IdentityTransitionError::Malformed(
                "portable-path record is stored under another key".into(),
            ));
        }
        if let Some(occupied) = record.occupied() {
            let current = current_pages
                .get(&occupied.page_id())
                .and_then(Option::as_ref)
                .and_then(PageState::path);
            if current != Some(occupied.exact_path()) {
                return Err(IdentityTransitionError::Malformed(
                    "portable-path row disagrees with current page state".into(),
                ));
            }
        }
    }

    let proposed_origin = IdentityOriginV1::accepted(batch_id, causal_dot);
    let mut desired = BTreeMap::<PageId, Option<ManagedPath>>::new();
    for delta in deltas {
        desired.insert(
            delta.page_id,
            prospective_pages[&delta.page_id]
                .as_ref()
                .and_then(PageState::path)
                .cloned(),
        );
    }
    let mut changed = BTreeMap::new();
    let existing_keys = records.keys().copied().collect::<Vec<_>>();
    for key in existing_keys {
        let Some(occupied) = records
            .get(&key)
            .and_then(PortablePathIdentityRecordV1::occupied)
            .cloned()
        else {
            continue;
        };
        let release = affected.contains(&occupied.page_id())
            && desired[&occupied.page_id()].as_ref().is_none_or(|path| {
                path.portable_key().digest() != key || path != occupied.exact_path()
            });
        if !release {
            continue;
        }
        let replacement = PortablePathIdentityRecordV1::new(
            key,
            None,
            Some(PortablePathIdentityReleasedV1::new(
                occupied.page_id(),
                occupied.exact_path().clone(),
                occupied.acquisition(),
                proposed_origin,
            )),
        )
        .map_err(IdentityTransitionError::Malformed)?;
        records.insert(key, replacement.clone());
        changed.insert(key, replacement);
    }

    let mut acquisitions = desired
        .iter()
        .filter_map(|(page_id, path)| {
            path.as_ref()
                .map(|path| (path.portable_key().digest(), *page_id, path.clone()))
        })
        .collect::<Vec<_>>();
    acquisitions.sort_unstable();
    let mut conflicts = Vec::new();
    for (key, page_id, path) in acquisitions {
        let existing = records.get(&key);
        if existing
            .and_then(PortablePathIdentityRecordV1::occupied)
            .is_some_and(|occupied| occupied.page_id() == page_id && occupied.exact_path() == &path)
        {
            continue;
        }
        if let Some(occupied) = existing.and_then(PortablePathIdentityRecordV1::occupied) {
            if origin_is_contained(occupied.acquisition(), batch_id, &contains) {
                return Err(IdentityTransitionError::Occupied(
                    "portable path is occupied at frontier F".into(),
                ));
            }
            conflicts.push(PortablePathIdentityConflict {
                key,
                page_ids: vec![occupied.page_id(), page_id],
            });
            continue;
        }
        if let Some(released) = existing
            .and_then(PortablePathIdentityRecordV1::latest_release)
            .filter(|released| !origin_is_contained(released.release(), batch_id, &contains))
        {
            conflicts.push(PortablePathIdentityConflict {
                key,
                page_ids: vec![released.prior_page_id(), page_id],
            });
            continue;
        }
        let latest_release = existing
            .and_then(PortablePathIdentityRecordV1::latest_release)
            .cloned();
        let replacement = PortablePathIdentityRecordV1::new(
            key,
            Some(PortablePathIdentityOccupiedV1::new(
                page_id,
                path,
                proposed_origin,
            )),
            latest_release,
        )
        .map_err(IdentityTransitionError::Malformed)?;
        records.insert(key, replacement.clone());
        changed.insert(key, replacement);
    }
    if !conflicts.is_empty() {
        return Ok(PortablePathIdentityTransition {
            changed: BTreeMap::new(),
            conflicts,
        });
    }
    Ok(PortablePathIdentityTransition { changed, conflicts })
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::oplog::{CausalPeerId, DeviceId, DocumentId, ManagedTextKind};

    fn batch(value: u128) -> BatchId {
        BatchId::from_uuid(Uuid::from_u128(value))
    }

    fn dot(value: u64) -> BatchCausalDot {
        BatchCausalDot::new(
            CausalPeerId::from_device_id(DeviceId::from_uuid(Uuid::from_u128(
                10_000 + u128::from(value),
            ))),
            value,
        )
        .unwrap()
    }

    fn page(value: u128) -> PageId {
        PageId::from_uuid(Uuid::from_u128(value))
    }

    fn live(name: &str, path: &str, home: u128) -> PageState {
        PageState::Live {
            name: LogicalPageName::parse(name).unwrap(),
            path: ManagedPath::parse(path).unwrap(),
            home_document_id: DocumentId::from_uuid(Uuid::from_u128(home)),
            kind: ManagedTextKind::Page,
        }
    }

    #[test]
    fn baseline_records_are_inline_canonical_and_need_no_fabricated_batch() {
        let page_id = PageId::from_uuid(Uuid::from_u128(1));
        let name = LogicalPageName::parse("Baseline Name").unwrap();
        let page_record = PageNameIdentityRecordV1::baseline(page_id, name.clone()).unwrap();
        let page_bytes = page_record.encode().unwrap();
        assert_eq!(
            PageNameIdentityRecordV1::decode(name.key_digest(), &page_bytes).unwrap(),
            page_record
        );
        assert_eq!(
            page_record.occupied().unwrap().acquisition(),
            IdentityOriginV1::Baseline
        );

        let path = ManagedPath::parse("pages/baseline-name.md").unwrap();
        let path_record = PortablePathIdentityRecordV1::baseline(page_id, path.clone()).unwrap();
        let path_bytes = path_record.encode().unwrap();
        assert_eq!(
            PortablePathIdentityRecordV1::decode(path.portable_key().digest(), &path_bytes)
                .unwrap(),
            path_record
        );
        assert_eq!(
            path_record.occupied().unwrap().acquisition(),
            IdentityOriginV1::Baseline
        );
    }

    #[test]
    fn clean_identity_records_are_independent_of_patricia_and_side_blobs() {
        let source = include_str!("sqlite_identity.rs");
        let forbidden = [
            ["content_", "patricia"].concat(),
            ["Patricia", "Index"].concat(),
            ["ExactLogicalPageName", "Ref"].concat(),
            ["PAGE_NAME_EXACT_", "NAMES_DIR"].concat(),
        ];
        for forbidden in forbidden {
            assert!(
                !source.contains(&forbidden),
                "clean SQLite identity record regained dependency on {forbidden}"
            );
        }
        let contract = include_str!("../../../../docs/storage-sync-contract.md");
        assert!(contract.contains("`tine-storage` SQLite schema 20"));
        assert!(contract.contains("explicitly either `Baseline` or an accepted"));
        assert!(contract.contains("then deleted rather than retained as a\nsecond ready route"));
    }

    #[test]
    fn baseline_name_and_path_transition_without_fabricated_causality() {
        let page_id = page(20);
        let before = live("Alpha", "pages/alpha.md", 21);
        let after = live("Beta", "pages/beta.md", 21);
        let delta = PageDelta {
            page_id,
            before: Some(before.clone()),
            after: Some(after.clone()),
        };
        let name_key = before.name().key_digest();
        let path_key = before.path().unwrap().portable_key().digest();
        let exact_before = BTreeMap::from([(page_id, Some(before.clone()))]);
        let current = exact_before.clone();
        let prospective = BTreeMap::from([(page_id, Some(after.clone()))]);

        let names = prepare_page_name_identity_transition(
            batch(30),
            dot(30),
            &exact_before,
            std::slice::from_ref(&delta),
            &current,
            &prospective,
            BTreeMap::from([(
                name_key,
                PageNameIdentityRecordV1::baseline(page_id, before.name().clone()).unwrap(),
            )]),
            |_, _| false,
        )
        .unwrap();
        assert!(names.conflicts.is_empty());
        let released = names.changed[&name_key].latest_release().unwrap();
        assert_eq!(released.prior_acquisition(), IdentityOriginV1::Baseline);
        assert_eq!(released.prior_exact_state(), IdentityOriginV1::Baseline);
        assert_eq!(released.release().batch_id(), Some(batch(30)));
        assert_eq!(
            names.changed[&after.name().key_digest()]
                .occupied()
                .unwrap()
                .acquisition()
                .batch_id(),
            Some(batch(30))
        );

        let paths = prepare_portable_path_identity_transition(
            batch(30),
            dot(30),
            std::slice::from_ref(&delta),
            &current,
            &prospective,
            BTreeMap::from([(
                path_key,
                PortablePathIdentityRecordV1::baseline(page_id, before.path().unwrap().clone())
                    .unwrap(),
            )]),
            |_, _| false,
        )
        .unwrap();
        assert!(paths.conflicts.is_empty());
        assert_eq!(
            paths.changed[&path_key]
                .latest_release()
                .unwrap()
                .prior_acquisition(),
            IdentityOriginV1::Baseline
        );
        assert_eq!(
            paths.changed[&after.path().unwrap().portable_key().digest()]
                .occupied()
                .unwrap()
                .acquisition()
                .batch_id(),
            Some(batch(30))
        );
    }

    #[test]
    fn concurrent_exact_title_selection_is_delivery_order_independent() {
        let page_id = page(40);
        let base = live("alpha", "pages/alpha.md", 41);
        let left = live("Alpha", "pages/alpha.md", 41);
        let right = live("ALPHA", "pages/alpha.md", 41);
        let key = base.name().key_digest();
        let candidate = |current_state: PageState,
                         proposed_state: PageState,
                         existing_batch: BatchId,
                         existing_dot: BatchCausalDot,
                         proposed_batch: BatchId,
                         proposed_dot: BatchCausalDot| {
            prepare_page_name_identity_transition(
                proposed_batch,
                proposed_dot,
                &BTreeMap::from([(page_id, Some(base.clone()))]),
                &[PageDelta {
                    page_id,
                    before: Some(base.clone()),
                    after: Some(proposed_state.clone()),
                }],
                &BTreeMap::from([(page_id, Some(current_state.clone()))]),
                &BTreeMap::from([(page_id, Some(proposed_state))]),
                BTreeMap::from([(
                    key,
                    PageNameIdentityRecordV1::new(
                        key,
                        Some(PageNameIdentityOccupiedV1::new(
                            page_id,
                            current_state.name().clone(),
                            IdentityOriginV1::Baseline,
                            IdentityOriginV1::accepted(existing_batch, existing_dot),
                        )),
                        None,
                    )
                    .unwrap(),
                )]),
                |_, _| false,
            )
            .unwrap()
        };
        let left_batch = batch(42);
        let right_batch = batch(43);
        let left_dot = dot(42);
        let right_dot = dot(43);
        let right_after_left = candidate(
            left.clone(),
            right.clone(),
            left_batch,
            left_dot,
            right_batch,
            right_dot,
        );
        let left_after_right = candidate(
            right.clone(),
            left,
            right_batch,
            right_dot,
            left_batch,
            left_dot,
        );
        assert_eq!(
            right_after_left.changed[&key]
                .occupied()
                .unwrap()
                .exact_name(),
            right.name()
        );
        assert!(left_after_right.changed.is_empty());
    }

    #[test]
    fn concurrent_reuse_is_conflict_but_causal_reuse_is_allowed() {
        let prior_page = page(50);
        let next_page = page(51);
        let name = LogicalPageName::parse("Reused").unwrap();
        let key = name.key_digest();
        let acquisition = IdentityOriginV1::accepted(batch(52), dot(52));
        let release = IdentityOriginV1::accepted(batch(53), dot(53));
        let record = PageNameIdentityRecordV1::new(
            key,
            None,
            Some(PageNameIdentityReleasedV1::new(
                prior_page,
                name.clone(),
                acquisition,
                acquisition,
                release,
            )),
        )
        .unwrap();
        let after = live("Reused", "pages/reused.md", 54);
        let delta = PageDelta {
            page_id: next_page,
            before: None,
            after: Some(after.clone()),
        };
        let maps = (
            BTreeMap::from([(next_page, None)]),
            BTreeMap::from([(next_page, None)]),
            BTreeMap::from([(next_page, Some(after))]),
        );
        let concurrent = prepare_page_name_identity_transition(
            batch(55),
            dot(55),
            &maps.0,
            std::slice::from_ref(&delta),
            &maps.1,
            &maps.2,
            BTreeMap::from([(key, record.clone())]),
            |_, _| false,
        )
        .unwrap();
        assert!(concurrent.changed.is_empty());
        assert_eq!(concurrent.conflicts.len(), 1);

        let causal = prepare_page_name_identity_transition(
            batch(56),
            dot(56),
            &maps.0,
            std::slice::from_ref(&delta),
            &maps.1,
            &maps.2,
            BTreeMap::from([(key, record)]),
            |dot, batch| {
                dot == release.causal_dot().unwrap() && batch == release.batch_id().unwrap()
            },
        )
        .unwrap();
        assert!(causal.conflicts.is_empty());
        assert_eq!(
            causal.changed[&key].occupied().unwrap().page_id(),
            next_page
        );
    }
}
