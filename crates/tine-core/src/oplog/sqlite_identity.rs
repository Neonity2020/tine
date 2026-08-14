//! Application-owned causal identity records stored in the disposable SQLite
//! projection.
//!
//! These values intentionally contain the exact semantic value inline.  They
//! are complete point records: unlike the retired Patricia representation,
//! decoding one never requires opening a content-addressed side blob.  The
//! origin distinguishes immutable activation facts from ordinary accepted
//! operations without fabricating a bootstrap batch or causal dot.

use serde::{Deserialize, Serialize};

use super::{
    BatchCausalDot, BatchId, LogicalPageName, ManagedPath, PageId, PageNameKeyDigest,
    PortablePathKeyDigest,
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

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

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
}
