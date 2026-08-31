#![allow(clippy::result_large_err)]

use serde::{Deserialize, Serialize};

use super::object_store::StoreError;
use super::{
    BatchCausalDot, BatchId, ContentDigest, ManagedPath, PageId, PortablePathKeyDigest,
    PORTABLE_PATH_KEY_VERSION,
};
use tine_storage::PatriciaIndexRoot;

const PORTABLE_PATH_RECORD_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PortablePathIndexRoot(PatriciaIndexRoot);

impl PortablePathIndexRoot {
    pub fn empty() -> Self {
        Self(PatriciaIndexRoot::empty())
    }

    pub const fn digest(self) -> ContentDigest {
        self.0.digest()
    }

    pub(crate) const fn from_digest(digest: ContentDigest) -> Self {
        Self(PatriciaIndexRoot::from_digest(digest))
    }
}

impl Default for PortablePathIndexRoot {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortablePathOccupied {
    page_id: PageId,
    exact_path: ManagedPath,
    exact_path_digest: ContentDigest,
    acquisition_batch: BatchId,
    causal_dot: BatchCausalDot,
}

impl PortablePathOccupied {
    pub fn new(
        page_id: PageId,
        exact_path: ManagedPath,
        acquisition_batch: BatchId,
        causal_dot: BatchCausalDot,
    ) -> Self {
        let exact_path_digest = exact_path_digest(&exact_path);
        Self {
            page_id,
            exact_path,
            exact_path_digest,
            acquisition_batch,
            causal_dot,
        }
    }

    pub const fn page_id(&self) -> PageId {
        self.page_id
    }

    pub fn exact_path(&self) -> &ManagedPath {
        &self.exact_path
    }

    pub const fn exact_path_digest(&self) -> ContentDigest {
        self.exact_path_digest
    }

    pub const fn acquisition_batch(&self) -> BatchId {
        self.acquisition_batch
    }

    pub const fn causal_dot(&self) -> BatchCausalDot {
        self.causal_dot
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortablePathReleased {
    prior_page_id: PageId,
    prior_exact_path: ManagedPath,
    prior_acquisition_batch: BatchId,
    release_batch: BatchId,
    causal_dot: BatchCausalDot,
}

impl PortablePathReleased {
    pub const fn new(
        prior_page_id: PageId,
        prior_exact_path: ManagedPath,
        prior_acquisition_batch: BatchId,
        release_batch: BatchId,
        causal_dot: BatchCausalDot,
    ) -> Self {
        Self {
            prior_page_id,
            prior_exact_path,
            prior_acquisition_batch,
            release_batch,
            causal_dot,
        }
    }

    pub const fn prior_page_id(&self) -> PageId {
        self.prior_page_id
    }

    pub const fn release_batch(&self) -> BatchId {
        self.release_batch
    }

    pub fn prior_exact_path(&self) -> &ManagedPath {
        &self.prior_exact_path
    }

    pub const fn prior_acquisition_batch(&self) -> BatchId {
        self.prior_acquisition_batch
    }

    pub const fn causal_dot(&self) -> BatchCausalDot {
        self.causal_dot
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortablePathRecord {
    schema_version: u32,
    key_version: u32,
    key_digest: PortablePathKeyDigest,
    occupied: Option<PortablePathOccupied>,
    latest_release: Option<PortablePathReleased>,
}

impl PortablePathRecord {
    pub fn new(
        key_digest: PortablePathKeyDigest,
        occupied: Option<PortablePathOccupied>,
        latest_release: Option<PortablePathReleased>,
    ) -> Result<Self, StoreError> {
        let record = Self {
            schema_version: PORTABLE_PATH_RECORD_SCHEMA_VERSION,
            key_version: PORTABLE_PATH_KEY_VERSION,
            key_digest,
            occupied,
            latest_release,
        };
        record.validate(key_digest)?;
        Ok(record)
    }

    pub const fn key_digest(&self) -> PortablePathKeyDigest {
        self.key_digest
    }

    pub const fn occupied(&self) -> Option<&PortablePathOccupied> {
        self.occupied.as_ref()
    }

    pub const fn latest_release(&self) -> Option<&PortablePathReleased> {
        self.latest_release.as_ref()
    }

    fn validate(&self, expected: PortablePathKeyDigest) -> Result<(), StoreError> {
        if self.schema_version != PORTABLE_PATH_RECORD_SCHEMA_VERSION
            || self.key_version != PORTABLE_PATH_KEY_VERSION
            || self.key_digest != expected
            || self.occupied.as_ref().is_some_and(|occupied| {
                occupied.exact_path.portable_key().digest() != expected
                    || exact_path_digest(&occupied.exact_path) != occupied.exact_path_digest
            })
            || self
                .latest_release
                .as_ref()
                .is_some_and(|release| release.prior_exact_path.portable_key().digest() != expected)
        {
            return Err(StoreError::MalformedLogseqClaimIndex);
        }
        Ok(())
    }
}

fn exact_path_digest(path: &ManagedPath) -> ContentDigest {
    let mut bytes = b"tine/exact-managed-path/v1\0".to_vec();
    bytes.extend_from_slice(&(path.as_str().len() as u64).to_be_bytes());
    bytes.extend_from_slice(path.as_str().as_bytes());
    ContentDigest::of(&bytes)
}

fn encode_record(record: &PortablePathRecord) -> Result<Vec<u8>, StoreError> {
    postcard::to_allocvec(record).map_err(|_| StoreError::MalformedLogseqClaimIndex)
}

fn decode_record(
    expected: PortablePathKeyDigest,
    bytes: &[u8],
) -> Result<PortablePathRecord, StoreError> {
    let record: PortablePathRecord =
        postcard::from_bytes(bytes).map_err(|_| StoreError::MalformedLogseqClaimIndex)?;
    record.validate(expected)?;
    if encode_record(&record)? != bytes {
        return Err(StoreError::MalformedLogseqClaimIndex);
    }
    Ok(record)
}
