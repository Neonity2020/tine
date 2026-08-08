#![allow(clippy::result_large_err)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::time::Instant;

use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};

use super::authenticated_patricia::{
    CompletedPatriciaConstruction, PatriciaIndexConstruction, PatriciaIndexRoot, PatriciaIndexStore,
};
#[cfg(test)]
use super::authenticated_patricia::{
    PatriciaIndexConstructionStats, PatriciaIndexStats, MAX_PATRICIA_CONSTRUCTION_RESIDENT_BYTES,
};
use super::object_store::{
    publish_immutable_exact, read_optional_regular, DetachedBootstrapImmutablePublisher, StoreError,
};
use super::{
    BlockId, BlockOwner, ContentDigest, DocumentId, LogicalPageName, LogseqUuid, ManagedPath,
    PageId, PageNameKeyDigest, PageNameOwnershipRootV1, SemanticEffect,
};

pub const REFERENCE_CATALOG_SCHEMA_VERSION: u32 = 2;
pub const REFERENCE_CATALOG_ROOT_SCHEMA_VERSION: u32 = 2;
pub const REFERENCE_CATALOG_POLICY_VERSION: u32 = 1;
pub const REFERENCE_CATALOG_EXTRACTOR_VERSION: u32 = 2;
pub const MAX_REFERENCE_CATALOG_DELTA_SOURCES: usize = super::semantic::MAX_SEMANTIC_DELTA_ENTRIES;
pub const MAX_REFERENCE_CATALOG_DELTA_BYTES: usize = super::semantic::MAX_SEMANTIC_EFFECT_BYTES;
pub const MAX_REFERENCE_TARGET_BYTES: usize = super::semantic::MAX_LOGICAL_PAGE_NAME_BYTES;
const MAX_EPHEMERAL_REFERENCE_CATALOG_SOURCES: usize = 4_096;
const MAX_REFERENCE_OBJECT_BYTES: u64 = super::MAX_OBJECT_BYTES as u64;
const TARGET_POSTING_CHUNK_BYTES: usize = 1024 * 1024;
const POSTING_DOMAIN: &[u8] = b"tine/reference-catalog/source-posting/v2";
const ROOT_DOMAIN: &[u8] = b"tine/reference-catalog/root/v2";
const DELTA_MAGIC: &[u8; 8] = b"TINEREF2";
const COVERAGE_VALUE: &[u8] = b"live-source-v2";
const REVERSE_VALUE: &[u8] = b"reference-source-v2";
const PAGE_REFERENCE_PREFIX: u8 = 0;
const PAGE_ALIAS_PREFIX: u8 = 1;
const BLOCK_UUID_PREFIX: u8 = 2;

#[cfg(test)]
static REFERENCE_EVIDENCE_PARSE_CALLS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_reference_evidence_parse_calls() {
    REFERENCE_EVIDENCE_PARSE_CALLS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn reference_evidence_parse_calls() -> usize {
    REFERENCE_EVIDENCE_PARSE_CALLS.load(Ordering::Relaxed)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceCatalogPolicyV1 {
    schema_version: u32,
    policy_version: u32,
    property_pages_enabled: bool,
    property_page_exclusions: Vec<String>,
}

impl ReferenceCatalogPolicyV1 {
    pub fn from_config(config: &crate::config::Config) -> Self {
        let mut property_page_exclusions = config
            .property_pages_excludelist
            .iter()
            .map(|key| crate::doc::property_key_norm(key))
            .collect::<Vec<_>>();
        property_page_exclusions.sort_unstable();
        property_page_exclusions.dedup();
        Self {
            schema_version: REFERENCE_CATALOG_POLICY_VERSION,
            policy_version: REFERENCE_CATALOG_POLICY_VERSION,
            property_pages_enabled: config.property_pages_enabled,
            property_page_exclusions,
        }
    }

    pub fn digest(&self) -> Result<ContentDigest, ReferenceCatalogError> {
        self.validate()?;
        domain_digest(
            b"tine/reference-catalog/policy/v1",
            &encode_canonical(self)?,
        )
    }

    fn property_key_enabled(&self, key: &str) -> bool {
        let key = crate::doc::property_key_norm(key);
        self.property_pages_enabled && self.property_page_exclusions.binary_search(&key).is_err()
    }

    fn validate(&self) -> Result<(), ReferenceCatalogError> {
        require_version(
            "reference catalog policy schema",
            self.schema_version,
            REFERENCE_CATALOG_POLICY_VERSION,
        )?;
        require_version(
            "reference catalog policy",
            self.policy_version,
            REFERENCE_CATALOG_POLICY_VERSION,
        )?;
        if self
            .property_page_exclusions
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
            || self.property_page_exclusions.iter().any(|key| {
                key.is_empty()
                    || key.len() > MAX_REFERENCE_TARGET_BYTES
                    || crate::doc::property_key_norm(key) != *key
            })
        {
            return Err(ReferenceCatalogError::NonCanonical);
        }
        Ok(())
    }
}

impl Default for ReferenceCatalogPolicyV1 {
    fn default() -> Self {
        Self::from_config(&crate::config::Config::default())
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageReferenceKindV1 {
    PageLink,
    Tag,
    PageEmbed,
    LinkablePropertyValue,
    AliasDeclaration,
    PropertyKeyPseudoPage,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockReferenceKindV1 {
    Reference,
    Embed,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ReferenceSourceLocatorV1 {
    Preamble,
    Block {
        block_id: BlockId,
        home_document_id: DocumentId,
    },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageNameReferenceFactV1 {
    pub source: ReferenceSourceLocatorV1,
    pub kind: PageReferenceKindV1,
    pub raw_target: String,
    pub normalized_target: String,
    pub target_key: PageNameKeyDigest,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockReferenceFactV1 {
    pub source: ReferenceSourceLocatorV1,
    pub kind: BlockReferenceKindV1,
    pub raw_claim: String,
    pub logseq_uuid: LogseqUuid,
    pub byte_start: u32,
    pub byte_end: u32,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ReferenceFactV1 {
    PageName(PageNameReferenceFactV1),
    Block(BlockReferenceFactV1),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReferenceCandidateTargetV2 {
    PageName(PageNameKeyDigest),
    PageAlias(PageNameKeyDigest),
    BlockUuid(LogseqUuid),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceSourcePostingV2 {
    schema_version: u32,
    source_page_id: PageId,
    facts: Vec<ReferenceFactV1>,
}

impl ReferenceSourcePostingV2 {
    pub fn source_page_id(&self) -> PageId {
        self.source_page_id
    }

    pub fn facts(&self) -> &[ReferenceFactV1] {
        &self.facts
    }

    pub fn digest(&self) -> Result<ContentDigest, ReferenceCatalogError> {
        self.validate()?;
        domain_digest(POSTING_DOMAIN, &encode_canonical(self)?)
    }

    fn new(
        source_page_id: PageId,
        mut facts: Vec<ReferenceFactV1>,
    ) -> Result<Self, ReferenceCatalogError> {
        facts.sort_unstable();
        let posting = Self {
            schema_version: REFERENCE_CATALOG_SCHEMA_VERSION,
            source_page_id,
            facts,
        };
        posting.validate()?;
        Ok(posting)
    }

    fn validate(&self) -> Result<(), ReferenceCatalogError> {
        require_version(
            "reference source posting",
            self.schema_version,
            REFERENCE_CATALOG_SCHEMA_VERSION,
        )?;
        if self.facts.windows(2).any(|pair| pair[0] > pair[1]) {
            return Err(ReferenceCatalogError::NonCanonical);
        }
        for fact in &self.facts {
            match fact {
                ReferenceFactV1::PageName(fact) => {
                    if fact.raw_target.is_empty()
                        || fact.raw_target.len() > MAX_REFERENCE_TARGET_BYTES
                        || fact.normalized_target.is_empty()
                        || fact.normalized_target.len() > MAX_REFERENCE_TARGET_BYTES
                        || fact.byte_start >= fact.byte_end
                    {
                        return Err(ReferenceCatalogError::MalformedFact);
                    }
                    let name = LogicalPageName::parse(fact.raw_target.clone())
                        .map_err(|_| ReferenceCatalogError::MalformedFact)?;
                    if name.key_digest() != fact.target_key
                        || crate::refs::normalize(&fact.raw_target) != fact.normalized_target
                    {
                        return Err(ReferenceCatalogError::MalformedFact);
                    }
                }
                ReferenceFactV1::Block(fact) => {
                    if LogseqUuid::parse(&fact.raw_claim).ok() != Some(fact.logseq_uuid)
                        || fact.byte_start >= fact.byte_end
                    {
                        return Err(ReferenceCatalogError::MalformedFact);
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReferenceSourcePageV1 {
    pub page_id: PageId,
    pub is_org: bool,
    pub preamble: Option<String>,
    pub blocks: Vec<ReferenceSourceBlockV1>,
}

#[derive(Clone, Debug)]
pub(crate) struct ReferenceSourceBlockV1 {
    pub block_id: BlockId,
    pub home_document_id: DocumentId,
    pub content: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceCatalogRootV2 {
    schema_version: u32,
    extractor_version: u32,
    extractor_digest: ContentDigest,
    policy_version: u32,
    policy_digest: ContentDigest,
    source_count: u64,
    source_coverage_root: ContentDigest,
    facts_root: ContentDigest,
    reverse_candidates_root: ContentDigest,
    page_name_authority_root: ContentDigest,
    external_uuid_claim_authority_root: ContentDigest,
}

impl ReferenceCatalogRootV2 {
    pub fn empty(
        policy: &ReferenceCatalogPolicyV1,
        page_names: &PageNameOwnershipRootV1,
        external_uuid_claim_authority_root: ContentDigest,
    ) -> Result<Self, ReferenceCatalogError> {
        Self::new(
            policy,
            0,
            empty_map_digest(),
            empty_map_digest(),
            empty_map_digest(),
            page_names,
            external_uuid_claim_authority_root,
        )
    }

    pub(crate) fn empty_for_authority_digests(
        policy: &ReferenceCatalogPolicyV1,
        page_name_authority_root: ContentDigest,
        external_uuid_claim_authority_root: ContentDigest,
    ) -> Result<Self, ReferenceCatalogError> {
        let root = Self {
            schema_version: REFERENCE_CATALOG_ROOT_SCHEMA_VERSION,
            extractor_version: REFERENCE_CATALOG_EXTRACTOR_VERSION,
            extractor_digest: extractor_digest(),
            policy_version: REFERENCE_CATALOG_POLICY_VERSION,
            policy_digest: policy.digest()?,
            source_count: 0,
            source_coverage_root: empty_map_digest(),
            facts_root: empty_map_digest(),
            reverse_candidates_root: empty_map_digest(),
            page_name_authority_root,
            external_uuid_claim_authority_root,
        };
        root.validate()?;
        Ok(root)
    }

    fn new(
        policy: &ReferenceCatalogPolicyV1,
        source_count: u64,
        source_coverage_root: ContentDigest,
        facts_root: ContentDigest,
        reverse_candidates_root: ContentDigest,
        page_names: &PageNameOwnershipRootV1,
        external_uuid_claim_authority_root: ContentDigest,
    ) -> Result<Self, ReferenceCatalogError> {
        let root = Self {
            schema_version: REFERENCE_CATALOG_ROOT_SCHEMA_VERSION,
            extractor_version: REFERENCE_CATALOG_EXTRACTOR_VERSION,
            extractor_digest: extractor_digest(),
            policy_version: REFERENCE_CATALOG_POLICY_VERSION,
            policy_digest: policy.digest()?,
            source_count,
            source_coverage_root,
            facts_root,
            reverse_candidates_root,
            page_name_authority_root: page_names
                .external_digest()
                .map_err(|error| ReferenceCatalogError::Authority(error.to_string()))?,
            external_uuid_claim_authority_root,
        };
        root.validate()?;
        Ok(root)
    }

    pub fn encode(&self) -> Result<Vec<u8>, ReferenceCatalogError> {
        self.validate()?;
        encode_canonical(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ReferenceCatalogError> {
        if bytes.len() > 4096 {
            return Err(ReferenceCatalogError::TooLarge(bytes.len()));
        }
        let root: Self = decode_canonical(bytes)?;
        root.validate()?;
        Ok(root)
    }

    pub fn external_digest(&self) -> Result<ContentDigest, ReferenceCatalogError> {
        domain_digest(ROOT_DOMAIN, &self.encode()?)
    }

    pub const fn source_count(&self) -> u64 {
        self.source_count
    }

    pub const fn facts_root(&self) -> ContentDigest {
        self.facts_root
    }

    pub const fn source_coverage_root(&self) -> ContentDigest {
        self.source_coverage_root
    }

    pub const fn reverse_candidates_root(&self) -> ContentDigest {
        self.reverse_candidates_root
    }

    /// Digest of the parser/extractor construction bound into this catalog.
    ///
    /// SQLite records this value only as a disposable dependency stamp.  It is
    /// deliberately exposed here so that the projection can prove it used the
    /// same extractor as the authenticated catalog, not so SQLite can choose
    /// an extractor of its own.
    pub const fn extractor_digest(&self) -> ContentDigest {
        self.extractor_digest
    }

    /// Digest of the reference policy construction bound into this catalog.
    pub const fn policy_digest(&self) -> ContentDigest {
        self.policy_digest
    }

    pub const fn page_name_authority_root(&self) -> ContentDigest {
        self.page_name_authority_root
    }

    pub const fn external_uuid_claim_authority_root(&self) -> ContentDigest {
        self.external_uuid_claim_authority_root
    }

    pub fn validate_bound(
        &self,
        policy: &ReferenceCatalogPolicyV1,
        page_names: &PageNameOwnershipRootV1,
        external_uuid_claim_authority_root: ContentDigest,
    ) -> Result<(), ReferenceCatalogError> {
        self.validate()?;
        if self.policy_digest != policy.digest()?
            || self.page_name_authority_root
                != page_names
                    .external_digest()
                    .map_err(|error| ReferenceCatalogError::Authority(error.to_string()))?
            || self.external_uuid_claim_authority_root != external_uuid_claim_authority_root
        {
            return Err(ReferenceCatalogError::AuthorityMismatch);
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ReferenceCatalogError> {
        require_version(
            "reference catalog root",
            self.schema_version,
            REFERENCE_CATALOG_ROOT_SCHEMA_VERSION,
        )?;
        require_version(
            "reference catalog extractor",
            self.extractor_version,
            REFERENCE_CATALOG_EXTRACTOR_VERSION,
        )?;
        require_version(
            "reference catalog policy",
            self.policy_version,
            REFERENCE_CATALOG_POLICY_VERSION,
        )?;
        if self.extractor_digest != extractor_digest()
            || (self.source_count == 0
                && (self.source_coverage_root != empty_map_digest()
                    || self.facts_root != empty_map_digest()
                    || self.reverse_candidates_root != empty_map_digest()))
            || (self.source_count > 0
                && (self.source_coverage_root == empty_map_digest()
                    || self.facts_root == empty_map_digest()))
        {
            return Err(ReferenceCatalogError::MalformedRoot);
        }
        Ok(())
    }

    fn construction_matches(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version
            && self.extractor_version == other.extractor_version
            && self.extractor_digest == other.extractor_digest
            && self.policy_version == other.policy_version
            && self.policy_digest == other.policy_digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferencePostingRefV2 {
    source_page_id: PageId,
    digest: ContentDigest,
    encoded_byte_length: u64,
    fact_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceCatalogReplacementV2 {
    page_id: PageId,
    prior_posting_digest: Option<ContentDigest>,
    post_posting: Option<ReferencePostingRefV2>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceTransitionRefV2 {
    digest: ContentDigest,
    encoded_byte_length: u64,
    replacement_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum ReferenceTransitionBindingV2 {
    Empty,
    Inline(Vec<ReferenceCatalogReplacementV2>),
    Stored(ReferenceTransitionRefV2),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceCatalogDeltaV2 {
    schema_version: u32,
    prior_root: ReferenceCatalogRootV2,
    post_root: ReferenceCatalogRootV2,
    transition: ReferenceTransitionBindingV2,
}

impl ReferenceCatalogDeltaV2 {
    #[cfg(test)]
    pub(crate) fn empty_transition(root: ReferenceCatalogRootV2) -> Self {
        Self {
            schema_version: REFERENCE_CATALOG_SCHEMA_VERSION,
            prior_root: root.clone(),
            post_root: root,
            transition: ReferenceTransitionBindingV2::Empty,
        }
    }

    #[cfg(test)]
    pub(crate) fn replace_prior_root_for_test(&mut self, root: ReferenceCatalogRootV2) {
        self.prior_root = root;
    }

    pub const fn prior_root(&self) -> &ReferenceCatalogRootV2 {
        &self.prior_root
    }

    pub const fn post_root(&self) -> &ReferenceCatalogRootV2 {
        &self.post_root
    }

    pub fn encode(&self) -> Result<Vec<u8>, ReferenceCatalogError> {
        self.validate()?;
        let body = encode_canonical(self)?;
        let mut bytes = Vec::with_capacity(DELTA_MAGIC.len() + body.len());
        bytes.extend_from_slice(DELTA_MAGIC);
        bytes.extend_from_slice(&body);
        if bytes.len() > MAX_REFERENCE_CATALOG_DELTA_BYTES {
            return Err(ReferenceCatalogError::TooLarge(bytes.len()));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ReferenceCatalogError> {
        if bytes.len() > MAX_REFERENCE_CATALOG_DELTA_BYTES {
            return Err(ReferenceCatalogError::TooLarge(bytes.len()));
        }
        let body = bytes
            .strip_prefix(DELTA_MAGIC)
            .ok_or_else(|| ReferenceCatalogError::Decode("invalid catalog delta magic".into()))?;
        let value: Self = decode_canonical(body)?;
        value.validate()?;
        if value.encode()?.as_slice() != bytes {
            return Err(ReferenceCatalogError::NonCanonical);
        }
        Ok(value)
    }

    fn validate(&self) -> Result<(), ReferenceCatalogError> {
        require_version(
            "reference catalog delta",
            self.schema_version,
            REFERENCE_CATALOG_SCHEMA_VERSION,
        )?;
        self.prior_root.validate()?;
        self.post_root.validate()?;
        if !self.prior_root.construction_matches(&self.post_root) {
            return Err(ReferenceCatalogError::AuthorityMismatch);
        }
        match &self.transition {
            ReferenceTransitionBindingV2::Empty => {
                if self.prior_root.source_count != self.post_root.source_count
                    || self.prior_root.source_coverage_root != self.post_root.source_coverage_root
                    || self.prior_root.facts_root != self.post_root.facts_root
                    || self.prior_root.reverse_candidates_root
                        != self.post_root.reverse_candidates_root
                {
                    return Err(ReferenceCatalogError::MalformedTransition);
                }
            }
            ReferenceTransitionBindingV2::Inline(replacements) => {
                validate_replacements(replacements)?;
            }
            ReferenceTransitionBindingV2::Stored(reference) => {
                if reference.encoded_byte_length == 0
                    || reference.encoded_byte_length > MAX_REFERENCE_OBJECT_BYTES
                    || reference.replacement_count == 0
                    || reference.replacement_count > MAX_REFERENCE_CATALOG_DELTA_SOURCES as u64
                {
                    return Err(ReferenceCatalogError::MalformedTransition);
                }
            }
        }
        Ok(())
    }
}

fn validate_replacements(
    replacements: &[ReferenceCatalogReplacementV2],
) -> Result<(), ReferenceCatalogError> {
    if replacements.len() > MAX_REFERENCE_CATALOG_DELTA_SOURCES
        || replacements
            .windows(2)
            .any(|pair| pair[0].page_id >= pair[1].page_id)
    {
        return Err(ReferenceCatalogError::NonCanonical);
    }
    for replacement in replacements {
        if replacement.post_posting.as_ref().is_some_and(|posting| {
            posting.source_page_id != replacement.page_id
                || posting.encoded_byte_length == 0
                || posting.encoded_byte_length > MAX_REFERENCE_OBJECT_BYTES
        }) {
            return Err(ReferenceCatalogError::MalformedTransition);
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PostingChunkRefV2 {
    digest: ContentDigest,
    encoded_byte_length: u64,
    fact_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PostingChunkV2 {
    schema_version: u32,
    facts: Vec<ReferenceFactV1>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PostingManifestV2 {
    schema_version: u32,
    source_page_id: PageId,
    fact_count: u64,
    chunks: Vec<PostingChunkRefV2>,
}

#[derive(Debug)]
pub(crate) struct ReferenceCatalogStore {
    postings: Dir,
    patricia: PatriciaIndexStore,
    detached_publisher: Option<DetachedBootstrapImmutablePublisher>,
}

impl ReferenceCatalogStore {
    pub(crate) fn new(nodes: Dir, postings: Dir) -> Self {
        Self {
            postings,
            patricia: PatriciaIndexStore::new(nodes),
            detached_publisher: None,
        }
    }

    pub(crate) fn for_detached_bootstrap(
        &self,
        publisher: DetachedBootstrapImmutablePublisher,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            postings: self.postings.try_clone()?,
            patricia: self.patricia.for_detached_bootstrap(publisher.clone())?,
            detached_publisher: Some(publisher),
        })
    }

    fn publish_exact(
        &self,
        filename: &str,
        bytes: &[u8],
        kind: &'static str,
    ) -> Result<(), StoreError> {
        if let Some(publisher) = &self.detached_publisher {
            publisher.publish(&self.postings, filename, bytes, kind)
        } else {
            publish_immutable_exact(&self.postings, filename, bytes, kind)
        }
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> PatriciaIndexStats {
        self.patricia.stats()
    }

    #[cfg(test)]
    fn corrupt_packed_node_for_test(
        &self,
        digest: ContentDigest,
    ) -> Result<(), ReferenceCatalogError> {
        self.patricia
            .corrupt_packed_node_for_test(digest)
            .map_err(store_error)
    }

    pub(crate) const fn patricia_index(&self) -> &PatriciaIndexStore {
        &self.patricia
    }

    fn publish_posting(
        &self,
        posting: &ReferenceSourcePostingV2,
    ) -> Result<ReferencePostingRefV2, ReferenceCatalogError> {
        posting.validate()?;
        let mut chunks = Vec::new();
        let mut current = Vec::new();
        let mut estimated = 0usize;
        for fact in &posting.facts {
            let encoded_fact = encode_canonical(fact)?;
            if !current.is_empty()
                && estimated.saturating_add(encoded_fact.len()) > TARGET_POSTING_CHUNK_BYTES
            {
                chunks.push(self.publish_chunk(std::mem::take(&mut current))?);
                estimated = 0;
            }
            estimated = estimated
                .checked_add(encoded_fact.len())
                .ok_or(ReferenceCatalogError::Allocation)?;
            current.push(fact.clone());
        }
        if !current.is_empty() {
            chunks.push(self.publish_chunk(current)?);
        }
        let manifest = PostingManifestV2 {
            schema_version: REFERENCE_CATALOG_SCHEMA_VERSION,
            source_page_id: posting.source_page_id,
            fact_count: posting.facts.len() as u64,
            chunks,
        };
        let bytes = encode_canonical(&manifest)?;
        require_object_size(&bytes)?;
        let digest = ContentDigest::of(&bytes);
        self.publish_exact(
            &posting_filename(digest),
            &bytes,
            "reference posting manifest",
        )
        .map_err(store_error)?;
        Ok(ReferencePostingRefV2 {
            source_page_id: posting.source_page_id,
            digest,
            encoded_byte_length: bytes.len() as u64,
            fact_count: posting.facts.len() as u64,
        })
    }

    fn publish_chunk(
        &self,
        facts: Vec<ReferenceFactV1>,
    ) -> Result<PostingChunkRefV2, ReferenceCatalogError> {
        let chunk = PostingChunkV2 {
            schema_version: REFERENCE_CATALOG_SCHEMA_VERSION,
            facts,
        };
        let bytes = encode_canonical(&chunk)?;
        require_object_size(&bytes)?;
        let digest = ContentDigest::of(&bytes);
        self.publish_exact(&chunk_filename(digest), &bytes, "reference posting chunk")
            .map_err(store_error)?;
        Ok(PostingChunkRefV2 {
            digest,
            encoded_byte_length: bytes.len() as u64,
            fact_count: chunk.facts.len() as u64,
        })
    }

    fn publish_transition(
        &self,
        replacements: &[ReferenceCatalogReplacementV2],
    ) -> Result<ReferenceTransitionRefV2, ReferenceCatalogError> {
        validate_replacements(replacements)?;
        let bytes = encode_canonical(&replacements)?;
        require_object_size(&bytes)?;
        let digest = ContentDigest::of(&bytes);
        self.publish_exact(
            &transition_filename(digest),
            &bytes,
            "reference catalog transition",
        )
        .map_err(store_error)?;
        Ok(ReferenceTransitionRefV2 {
            digest,
            encoded_byte_length: bytes.len() as u64,
            replacement_count: replacements.len() as u64,
        })
    }

    fn read_transition(
        &self,
        reference: &ReferenceTransitionRefV2,
    ) -> Result<Vec<ReferenceCatalogReplacementV2>, ReferenceCatalogError> {
        let bytes = read_content_addressed(
            &self.postings,
            &transition_filename(reference.digest),
            reference.digest,
            reference.encoded_byte_length,
        )?;
        let replacements: Vec<ReferenceCatalogReplacementV2> = decode_canonical(&bytes)?;
        validate_replacements(&replacements)?;
        if replacements.len() as u64 != reference.replacement_count {
            return Err(ReferenceCatalogError::MalformedTransition);
        }
        Ok(replacements)
    }

    fn read_posting(
        &self,
        reference: &ReferencePostingRefV2,
    ) -> Result<ReferenceSourcePostingV2, ReferenceCatalogError> {
        let bytes = read_content_addressed(
            &self.postings,
            &posting_filename(reference.digest),
            reference.digest,
            reference.encoded_byte_length,
        )?;
        let manifest: PostingManifestV2 = decode_canonical(&bytes)?;
        if manifest.schema_version != REFERENCE_CATALOG_SCHEMA_VERSION
            || manifest.source_page_id != reference.source_page_id
            || manifest.fact_count != reference.fact_count
            || (manifest.fact_count == 0 && !manifest.chunks.is_empty())
            || manifest.chunks.iter().any(|chunk| {
                chunk.encoded_byte_length == 0
                    || chunk.encoded_byte_length > MAX_REFERENCE_OBJECT_BYTES
                    || chunk.fact_count == 0
            })
        {
            return Err(ReferenceCatalogError::MalformedPosting);
        }
        let declared_fact_count = manifest.chunks.iter().try_fold(0u64, |total, chunk| {
            total
                .checked_add(chunk.fact_count)
                .ok_or(ReferenceCatalogError::Allocation)
        })?;
        if declared_fact_count != manifest.fact_count {
            return Err(ReferenceCatalogError::MalformedPosting);
        }
        let capacity =
            usize::try_from(manifest.fact_count).map_err(|_| ReferenceCatalogError::Allocation)?;
        let mut facts = Vec::new();
        facts
            .try_reserve(capacity)
            .map_err(|_| ReferenceCatalogError::Allocation)?;
        for chunk_ref in manifest.chunks {
            let bytes = read_content_addressed(
                &self.postings,
                &chunk_filename(chunk_ref.digest),
                chunk_ref.digest,
                chunk_ref.encoded_byte_length,
            )?;
            let chunk: PostingChunkV2 = decode_canonical(&bytes)?;
            if chunk.schema_version != REFERENCE_CATALOG_SCHEMA_VERSION
                || chunk.facts.len() as u64 != chunk_ref.fact_count
            {
                return Err(ReferenceCatalogError::MalformedPosting);
            }
            facts.extend(chunk.facts);
        }
        let posting = ReferenceSourcePostingV2 {
            schema_version: REFERENCE_CATALOG_SCHEMA_VERSION,
            source_page_id: manifest.source_page_id,
            facts,
        };
        posting.validate()?;
        if posting.facts.len() as u64 != manifest.fact_count {
            return Err(ReferenceCatalogError::MalformedPosting);
        }
        Ok(posting)
    }

    fn posting_reference(
        &self,
        source_page_id: PageId,
        digest: ContentDigest,
    ) -> Result<ReferencePostingRefV2, ReferenceCatalogError> {
        let filename = posting_filename(digest);
        let bytes =
            read_optional_regular(&self.postings, &filename, MAX_REFERENCE_OBJECT_BYTES, None)
                .map_err(store_error)?
                .ok_or(ReferenceCatalogError::MissingObject(digest))?;
        if ContentDigest::of(&bytes) != digest {
            return Err(ReferenceCatalogError::ObjectDigestMismatch(digest));
        }
        let manifest: PostingManifestV2 = decode_canonical(&bytes)?;
        Ok(ReferencePostingRefV2 {
            source_page_id,
            digest,
            encoded_byte_length: bytes.len() as u64,
            fact_count: manifest.fact_count,
        })
    }

    fn posting(
        &self,
        root: &ReferenceCatalogRootV2,
        page_id: PageId,
    ) -> Result<Option<ReferenceSourcePostingV2>, ReferenceCatalogError> {
        let value = self
            .patricia
            .lookup(
                PatriciaIndexRoot::from_digest(root.facts_root),
                page_id.as_uuid().as_bytes(),
            )
            .map_err(store_error)?;
        value
            .map(|value| {
                let digest = digest_value(&value)?;
                let reference = self.posting_reference(page_id, digest)?;
                self.read_posting(&reference)
            })
            .transpose()
    }

    fn posting_constructed(
        &self,
        root: &ReferenceCatalogRootV2,
        page_id: PageId,
        construction: &PatriciaIndexConstruction,
    ) -> Result<Option<ReferenceSourcePostingV2>, ReferenceCatalogError> {
        let value = self
            .patricia
            .construction_lookup(
                construction,
                PatriciaIndexRoot::from_digest(root.facts_root),
                page_id.as_uuid().as_bytes(),
            )
            .map_err(store_error)?;
        value
            .map(|value| {
                let digest = digest_value(&value)?;
                let reference = self.posting_reference(page_id, digest)?;
                self.read_posting(&reference)
            })
            .transpose()
    }

    fn reverse_candidates(
        &self,
        root: &ReferenceCatalogRootV2,
        target: ReferenceCandidateTargetV2,
        limit: usize,
    ) -> Result<BTreeSet<PageId>, ReferenceCatalogError> {
        let prefix = reverse_candidate_prefix(target);
        let rows = self
            .patricia
            .lookup_prefix_limited(
                PatriciaIndexRoot::from_digest(root.reverse_candidates_root),
                &prefix,
                limit.saturating_add(1),
            )
            .map_err(store_error)?;
        if rows.len() > limit {
            return Err(ReferenceCatalogError::TooManyCandidates(rows.len()));
        }
        rows.into_iter()
            .map(|(key, value)| {
                if value != REVERSE_VALUE {
                    return Err(ReferenceCatalogError::MalformedRoot);
                }
                let (found_prefix, page_id) = decode_reverse_candidate_key(&key)?;
                if found_prefix != prefix {
                    return Err(ReferenceCatalogError::MalformedRoot);
                }
                Ok(page_id)
            })
            .collect()
    }

    pub(crate) fn validate_delta(
        &self,
        delta: &ReferenceCatalogDeltaV2,
    ) -> Result<(), ReferenceCatalogError> {
        delta.validate()?;
        let replacements = match &delta.transition {
            ReferenceTransitionBindingV2::Empty => Vec::new(),
            ReferenceTransitionBindingV2::Inline(replacements) => replacements.clone(),
            ReferenceTransitionBindingV2::Stored(reference) => self.read_transition(reference)?,
        };
        let mut facts_root = PatriciaIndexRoot::from_digest(delta.prior_root.facts_root);
        let mut coverage_root =
            PatriciaIndexRoot::from_digest(delta.prior_root.source_coverage_root);
        let mut reverse_root =
            PatriciaIndexRoot::from_digest(delta.prior_root.reverse_candidates_root);
        self.patricia
            .validate_root(facts_root)
            .map_err(store_error)?;
        self.patricia
            .validate_root(coverage_root)
            .map_err(store_error)?;
        self.patricia
            .validate_root(reverse_root)
            .map_err(store_error)?;
        let mut source_count = delta.prior_root.source_count;
        let mut fact_updates = BTreeMap::new();
        let mut coverage_updates = BTreeMap::new();
        let mut removals = Vec::new();
        let mut reverse_updates = BTreeMap::new();
        let mut reverse_removals = BTreeSet::new();
        for replacement in replacements {
            let key = replacement.page_id.as_uuid().as_bytes().to_vec();
            let prior_value = self
                .patricia
                .lookup(facts_root, &key)
                .map_err(store_error)?
                .map(|value| digest_value(&value))
                .transpose()?;
            let prior_covered = self
                .patricia
                .lookup(coverage_root, &key)
                .map_err(store_error)?
                .is_some();
            if prior_value != replacement.prior_posting_digest
                || prior_covered != replacement.prior_posting_digest.is_some()
            {
                return Err(ReferenceCatalogError::MalformedTransition);
            }
            let prior_reverse_keys = prior_value
                .map(|digest| {
                    let reference = self.posting_reference(replacement.page_id, digest)?;
                    self.read_posting(&reference)
                        .map(|posting| reverse_candidate_keys(&posting))
                })
                .transpose()?
                .unwrap_or_default();
            let mut post_reverse_keys = BTreeSet::new();
            match replacement.post_posting {
                Some(reference) => {
                    let posting = self.read_posting(&reference)?;
                    if posting.source_page_id != replacement.page_id {
                        return Err(ReferenceCatalogError::MalformedTransition);
                    }
                    post_reverse_keys = reverse_candidate_keys(&posting);
                    fact_updates.insert(key.clone(), reference.digest.as_bytes().to_vec());
                    coverage_updates.insert(key, COVERAGE_VALUE.to_vec());
                    if !prior_covered {
                        source_count = source_count
                            .checked_add(1)
                            .ok_or(ReferenceCatalogError::Allocation)?;
                    }
                }
                None => {
                    removals.push(key);
                    if prior_covered {
                        source_count = source_count
                            .checked_sub(1)
                            .ok_or(ReferenceCatalogError::MalformedTransition)?;
                    }
                }
            }
            reverse_removals.extend(prior_reverse_keys.difference(&post_reverse_keys).cloned());
            reverse_updates.extend(
                post_reverse_keys
                    .into_iter()
                    .map(|key| (key, REVERSE_VALUE.to_vec())),
            );
        }
        facts_root = self
            .patricia
            .insert_many_verify_existing(facts_root, &fact_updates)
            .map_err(store_error)?;
        coverage_root = self
            .patricia
            .insert_many_verify_existing(coverage_root, &coverage_updates)
            .map_err(store_error)?;
        facts_root = self
            .patricia
            .remove_many(facts_root, &removals)
            .map_err(store_error)?;
        coverage_root = self
            .patricia
            .remove_many(coverage_root, &removals)
            .map_err(store_error)?;
        reverse_root = self
            .patricia
            .insert_many_verify_existing(reverse_root, &reverse_updates)
            .map_err(store_error)?;
        reverse_root = self
            .patricia
            .remove_many(
                reverse_root,
                &reverse_removals.into_iter().collect::<Vec<_>>(),
            )
            .map_err(store_error)?;
        if source_count != delta.post_root.source_count
            || facts_root.digest() != delta.post_root.facts_root
            || coverage_root.digest() != delta.post_root.source_coverage_root
            || reverse_root.digest() != delta.post_root.reverse_candidates_root
        {
            return Err(ReferenceCatalogError::MalformedTransition);
        }
        Ok(())
    }

    /// Bounded proof that this durable store already holds the authenticated
    /// root node of every tree a catalog root names.
    ///
    /// Bootstrap installation uses this immediately before it publishes the
    /// cold history records that bind those roots, so no accepted record can
    /// name a catalog root this archive never received. The complete structural
    /// validation stays at the promotion boundary, where the catalog becomes
    /// live runtime authority.
    pub(crate) fn require_catalog_root_nodes(
        &self,
        root: &ReferenceCatalogRootV2,
    ) -> Result<(), ReferenceCatalogError> {
        root.validate()?;
        for digest in [
            root.facts_root,
            root.source_coverage_root,
            root.reverse_candidates_root,
        ] {
            self.patricia
                .validate_root(PatriciaIndexRoot::from_digest(digest))
                .map_err(store_error)?;
        }
        Ok(())
    }

    pub(crate) fn validate_catalog_root(
        &self,
        root: &ReferenceCatalogRootV2,
    ) -> Result<(), ReferenceCatalogError> {
        root.validate()?;
        let facts_root = PatriciaIndexRoot::from_digest(root.facts_root);
        let coverage_root = PatriciaIndexRoot::from_digest(root.source_coverage_root);
        let reverse_root = PatriciaIndexRoot::from_digest(root.reverse_candidates_root);
        let mut fact_count = 0u64;
        let mut expected_reverse = BTreeSet::new();
        let mut validation_error = None;
        self.patricia
            .visit_all(facts_root, |key, value| {
                let result = (|| {
                    let page_id = page_id_key(key)?;
                    let digest = digest_value(value)?;
                    let reference = self.posting_reference(page_id, digest)?;
                    let posting = self.read_posting(&reference)?;
                    expected_reverse.extend(reverse_candidate_keys(&posting));
                    if self
                        .patricia
                        .lookup(coverage_root, key)
                        .map_err(store_error)?
                        .as_deref()
                        != Some(COVERAGE_VALUE)
                    {
                        return Err(ReferenceCatalogError::MalformedRoot);
                    }
                    fact_count = fact_count
                        .checked_add(1)
                        .ok_or(ReferenceCatalogError::Allocation)?;
                    Ok(())
                })();
                if let Err(error) = result {
                    validation_error = Some(error);
                    false
                } else {
                    true
                }
            })
            .map_err(store_error)?;
        if let Some(error) = validation_error {
            return Err(error);
        }
        let mut coverage_count = 0u64;
        let mut validation_error = None;
        self.patricia
            .visit_all(coverage_root, |_key, value| {
                if value != COVERAGE_VALUE {
                    validation_error = Some(ReferenceCatalogError::MalformedRoot);
                    return false;
                }
                let Some(next) = coverage_count.checked_add(1) else {
                    validation_error = Some(ReferenceCatalogError::Allocation);
                    return false;
                };
                coverage_count = next;
                true
            })
            .map_err(store_error)?;
        if let Some(error) = validation_error {
            return Err(error);
        }
        if fact_count != root.source_count || coverage_count != root.source_count {
            return Err(ReferenceCatalogError::MalformedRoot);
        }
        let mut found_reverse = BTreeSet::new();
        let mut malformed_reverse = false;
        self.patricia
            .visit_all(reverse_root, |key, value| {
                if value != REVERSE_VALUE || decode_reverse_candidate_key(key).is_err() {
                    malformed_reverse = true;
                    return false;
                }
                found_reverse.insert(key.to_vec());
                true
            })
            .map_err(store_error)?;
        if malformed_reverse || found_reverse != expected_reverse {
            return Err(ReferenceCatalogError::MalformedRoot);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct ReferenceCatalogCandidateV2 {
    delta: ReferenceCatalogDeltaV2,
    memory: Option<MemoryCatalog>,
    construction: Option<ReferenceCatalogConstructionEvidenceV2>,
}

#[derive(Debug)]
struct ReferenceCatalogConstructionEvidenceV2 {
    construction_id: uuid::Uuid,
    store: Arc<ReferenceCatalogStore>,
    structurally_validated: Cell<bool>,
}

/// Opaque proof that this exact candidate was prepared by the active private
/// construction transaction. Callers can borrow it but cannot manufacture or
/// retarget it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReferenceCatalogPreparedCandidateV2<'a> {
    candidate: &'a ReferenceCatalogCandidateV2,
    evidence: &'a ReferenceCatalogConstructionEvidenceV2,
}

impl ReferenceCatalogCandidateV2 {
    pub(crate) const fn root(&self) -> &ReferenceCatalogRootV2 {
        self.delta.post_root()
    }

    pub(crate) const fn delta(&self) -> &ReferenceCatalogDeltaV2 {
        &self.delta
    }

    pub(crate) fn prepared_candidate(&self) -> Option<ReferenceCatalogPreparedCandidateV2<'_>> {
        self.construction
            .as_ref()
            .map(|evidence| ReferenceCatalogPreparedCandidateV2 {
                candidate: self,
                evidence,
            })
    }
}

#[derive(Clone, Debug, Default)]
struct MemoryCatalog {
    postings: BTreeMap<PageId, ReferenceSourcePostingV2>,
    facts: BTreeMap<PageId, ContentDigest>,
    coverage: BTreeSet<PageId>,
    reverse_candidates: BTreeSet<Vec<u8>>,
}

#[derive(Debug)]
enum ReferenceCatalogBackend {
    Memory(MemoryCatalog),
    Store(Arc<ReferenceCatalogStore>),
    Construction {
        store: Arc<ReferenceCatalogStore>,
        construction_id: uuid::Uuid,
        patricia: RefCell<PatriciaIndexConstruction>,
    },
    RecoveryRequired(Arc<ReferenceCatalogStore>),
}

#[derive(Debug)]
pub(crate) struct ReferenceCatalogStateV2 {
    policy: ReferenceCatalogPolicyV1,
    root: ReferenceCatalogRootV2,
    backend: ReferenceCatalogBackend,
    #[cfg(test)]
    completed_construction_stats: Option<PatriciaIndexConstructionStats>,
    #[cfg(test)]
    prepared_candidate_validations: Cell<usize>,
    #[cfg(test)]
    full_delta_validations: Cell<usize>,
    #[cfg(test)]
    final_catalog_validations: Cell<usize>,
    #[cfg(test)]
    prepare_attribution: Cell<ReferenceCatalogPrepareAttributionStats>,
}

/// Move-only proof that the authenticated root node of every tree named by one
/// catalog root was present under the held archive capability.
///
/// The proof deliberately does not claim that every historical immutable leaf
/// was eagerly traversed. Content-addressed point reads continue to verify each
/// leaf when it is used and fail closed on missing or corrupt bytes. This is the
/// restart analogue of an adopted scratch root: current authority is established
/// before service, while cold immutable leaves remain lazy.
pub(crate) struct AuthenticatedReferenceCatalogRootNodes {
    root: ReferenceCatalogRootV2,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReferenceCatalogConstructionWorkStats {
    pub(crate) peak_resident_bytes: usize,
    pub(crate) buffer_flushes: usize,
    pub(crate) prepared_candidate_validations: usize,
    pub(crate) full_delta_validations: usize,
    pub(crate) final_catalog_validations: usize,
    pub(crate) extraction_nanos: u128,
    pub(crate) posting_transition_publication_nanos: u128,
    pub(crate) facts_coverage_patricia_nanos: u128,
    pub(crate) reverse_patricia_nanos: u128,
    pub(crate) facts_coverage_patricia_reads: usize,
    pub(crate) reverse_patricia_reads: usize,
    pub(crate) prepared_sources: usize,
    pub(crate) fact_updates: usize,
    pub(crate) reverse_updates: usize,
    pub(crate) persistent_node_reads: usize,
    pub(crate) persistent_node_writes: usize,
    pub(crate) logical_node_writes: usize,
    pub(crate) loose_publication_calls: usize,
    pub(crate) pack_publication_calls: usize,
    pub(crate) catalog_publication_calls: usize,
    pub(crate) immutable_publication_calls: usize,
    pub(crate) head_transitions: usize,
    pub(crate) durability_barriers: usize,
    pub(crate) packed_bytes: usize,
    pub(crate) capacity_fallbacks: usize,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ReferenceCatalogPrepareAttributionStats {
    extraction_nanos: u128,
    posting_transition_publication_nanos: u128,
    facts_coverage_patricia_nanos: u128,
    reverse_patricia_nanos: u128,
    facts_coverage_patricia_reads: usize,
    reverse_patricia_reads: usize,
    prepared_sources: usize,
    fact_updates: usize,
    reverse_updates: usize,
}

impl ReferenceCatalogStateV2 {
    pub(crate) fn empty(
        policy: ReferenceCatalogPolicyV1,
        page_names: &PageNameOwnershipRootV1,
        external_uuid_claim_authority_root: ContentDigest,
    ) -> Result<Self, ReferenceCatalogError> {
        let root =
            ReferenceCatalogRootV2::empty(&policy, page_names, external_uuid_claim_authority_root)?;
        Ok(Self {
            policy,
            root,
            backend: ReferenceCatalogBackend::Memory(MemoryCatalog::default()),
            #[cfg(test)]
            completed_construction_stats: None,
            #[cfg(test)]
            prepared_candidate_validations: Cell::new(0),
            #[cfg(test)]
            full_delta_validations: Cell::new(0),
            #[cfg(test)]
            final_catalog_validations: Cell::new(0),
            #[cfg(test)]
            prepare_attribution: Cell::new(ReferenceCatalogPrepareAttributionStats::default()),
        })
    }

    pub(crate) fn attach_store(
        &mut self,
        store: Arc<ReferenceCatalogStore>,
    ) -> Result<(), ReferenceCatalogError> {
        if self.root.source_count != 0 {
            return Err(ReferenceCatalogError::AuthorityMismatch);
        }
        self.backend = ReferenceCatalogBackend::Store(store);
        Ok(())
    }

    pub(crate) fn attach_construction_store(
        &mut self,
        store: Arc<ReferenceCatalogStore>,
    ) -> Result<(), ReferenceCatalogError> {
        if self.root.source_count != 0 {
            return Err(ReferenceCatalogError::AuthorityMismatch);
        }
        self.backend = ReferenceCatalogBackend::Construction {
            store,
            construction_id: uuid::Uuid::new_v4(),
            patricia: RefCell::new(PatriciaIndexConstruction::default()),
        };
        Ok(())
    }

    pub(crate) fn restore_recovery_required(
        policy: ReferenceCatalogPolicyV1,
        root: ReferenceCatalogRootV2,
        page_names: &PageNameOwnershipRootV1,
        external_uuid_claim_authority_root: ContentDigest,
        store: Arc<ReferenceCatalogStore>,
    ) -> Result<Self, ReferenceCatalogError> {
        root.validate_bound(&policy, page_names, external_uuid_claim_authority_root)?;
        Ok(Self {
            policy,
            root,
            backend: ReferenceCatalogBackend::RecoveryRequired(store),
            #[cfg(test)]
            completed_construction_stats: None,
            #[cfg(test)]
            prepared_candidate_validations: Cell::new(0),
            #[cfg(test)]
            full_delta_validations: Cell::new(0),
            #[cfg(test)]
            final_catalog_validations: Cell::new(0),
            #[cfg(test)]
            prepare_attribution: Cell::new(ReferenceCatalogPrepareAttributionStats::default()),
        })
    }

    pub(crate) const fn root(&self) -> &ReferenceCatalogRootV2 {
        &self.root
    }

    pub(crate) const fn policy(&self) -> &ReferenceCatalogPolicyV1 {
        &self.policy
    }

    pub(crate) fn ensure_ready(&self) -> Result<(), ReferenceCatalogError> {
        match &self.backend {
            ReferenceCatalogBackend::RecoveryRequired(_) => {
                Err(ReferenceCatalogError::RecoveryRequired)
            }
            ReferenceCatalogBackend::Memory(_) => Ok(()),
            ReferenceCatalogBackend::Store(_) => Ok(()),
            ReferenceCatalogBackend::Construction { .. } => Ok(()),
        }
    }

    pub(crate) fn store_handle(&self) -> Option<Arc<ReferenceCatalogStore>> {
        match &self.backend {
            ReferenceCatalogBackend::Store(store)
            | ReferenceCatalogBackend::Construction { store, .. }
            | ReferenceCatalogBackend::RecoveryRequired(store) => Some(Arc::clone(store)),
            ReferenceCatalogBackend::Memory(_) => None,
        }
    }

    pub(crate) fn store_ref(&self) -> Option<&ReferenceCatalogStore> {
        match &self.backend {
            ReferenceCatalogBackend::Store(store)
            | ReferenceCatalogBackend::Construction { store, .. }
            | ReferenceCatalogBackend::RecoveryRequired(store) => Some(store),
            ReferenceCatalogBackend::Memory(_) => None,
        }
    }

    pub(crate) fn finish_recovery(&mut self) -> Result<(), ReferenceCatalogError> {
        let store = match &self.backend {
            ReferenceCatalogBackend::RecoveryRequired(store) => Arc::clone(store),
            ReferenceCatalogBackend::Memory(_) => return Ok(()),
            ReferenceCatalogBackend::Store(_) => return Ok(()),
            ReferenceCatalogBackend::Construction { .. } => {
                return Err(ReferenceCatalogError::AuthorityMismatch);
            }
        };
        store.validate_catalog_root(&self.root)?;
        self.backend = ReferenceCatalogBackend::Store(store);
        Ok(())
    }

    /// Authenticate only the compact root nodes needed to defer old immutable
    /// leaves during an adopted-current restart.
    pub(crate) fn authenticate_root_nodes(
        &self,
    ) -> Result<AuthenticatedReferenceCatalogRootNodes, ReferenceCatalogError> {
        let store = match &self.backend {
            ReferenceCatalogBackend::Store(store)
            | ReferenceCatalogBackend::RecoveryRequired(store) => Arc::clone(store),
            ReferenceCatalogBackend::Memory(_) | ReferenceCatalogBackend::Construction { .. } => {
                return Err(ReferenceCatalogError::StoreRequired);
            }
        };
        store.require_catalog_root_nodes(&self.root)?;
        Ok(AuthenticatedReferenceCatalogRootNodes {
            root: self.root.clone(),
        })
    }

    /// Return a recovery-required catalog to readable service from a move-only
    /// proof minted under the same held archive capability for this exact root.
    pub(crate) fn finish_recovery_from_root_nodes(
        &mut self,
        proof: AuthenticatedReferenceCatalogRootNodes,
    ) -> Result<(), ReferenceCatalogError> {
        let store = match &self.backend {
            ReferenceCatalogBackend::RecoveryRequired(store) => Arc::clone(store),
            ReferenceCatalogBackend::Memory(_) | ReferenceCatalogBackend::Store(_) => {
                return Err(ReferenceCatalogError::AuthorityMismatch);
            }
            ReferenceCatalogBackend::Construction { .. } => {
                return Err(ReferenceCatalogError::AuthorityMismatch);
            }
        };
        if self.root != proof.root {
            return Err(ReferenceCatalogError::AuthorityMismatch);
        }
        self.backend = ReferenceCatalogBackend::Store(store);
        Ok(())
    }

    pub(crate) fn posting(
        &self,
        page_id: PageId,
    ) -> Result<Option<ReferenceSourcePostingV2>, ReferenceCatalogError> {
        match &self.backend {
            ReferenceCatalogBackend::Memory(memory) => Ok(memory.postings.get(&page_id).cloned()),
            ReferenceCatalogBackend::Store(store) => store.posting(&self.root, page_id),
            ReferenceCatalogBackend::Construction {
                store, patricia, ..
            } => store.posting_constructed(&self.root, page_id, &patricia.borrow()),
            ReferenceCatalogBackend::RecoveryRequired(_) => {
                Err(ReferenceCatalogError::RecoveryRequired)
            }
        }
    }

    /// Read a source posting at an already-authenticated catalog root.
    ///
    /// Historical roots are available only from the immutable catalog store.
    /// The in-memory catalog is intentionally not a historical cache: using it
    /// for a different root would turn a current snapshot into a forged tail.
    pub(crate) fn posting_at_root(
        &self,
        root: &ReferenceCatalogRootV2,
        page_id: PageId,
    ) -> Result<Option<ReferenceSourcePostingV2>, ReferenceCatalogError> {
        root.validate()?;
        match &self.backend {
            ReferenceCatalogBackend::Store(store) => store.posting(root, page_id),
            ReferenceCatalogBackend::Construction {
                store, patricia, ..
            } => store.posting_constructed(root, page_id, &patricia.borrow()),
            ReferenceCatalogBackend::Memory(memory) if root == &self.root => {
                Ok(memory.postings.get(&page_id).cloned())
            }
            ReferenceCatalogBackend::Memory(_) => Err(ReferenceCatalogError::StoreRequired),
            ReferenceCatalogBackend::RecoveryRequired(_) => {
                Err(ReferenceCatalogError::RecoveryRequired)
            }
        }
    }

    pub(crate) fn reverse_candidates_at_root(
        &self,
        root: &ReferenceCatalogRootV2,
        target: ReferenceCandidateTargetV2,
        limit: usize,
    ) -> Result<BTreeSet<PageId>, ReferenceCatalogError> {
        root.validate()?;
        match &self.backend {
            ReferenceCatalogBackend::Store(store) => store.reverse_candidates(root, target, limit),
            ReferenceCatalogBackend::Construction { .. } => {
                Err(ReferenceCatalogError::StoreRequired)
            }
            ReferenceCatalogBackend::Memory(memory) if root == &self.root => {
                let prefix = reverse_candidate_prefix(target);
                let mut candidates = BTreeSet::new();
                for key in memory.reverse_candidates.range(prefix.clone()..) {
                    if !key.starts_with(&prefix) {
                        break;
                    }
                    let (found_prefix, page_id) = decode_reverse_candidate_key(key)?;
                    if found_prefix != prefix {
                        return Err(ReferenceCatalogError::MalformedRoot);
                    }
                    candidates.insert(page_id);
                    if candidates.len() > limit {
                        return Err(ReferenceCatalogError::TooManyCandidates(candidates.len()));
                    }
                }
                Ok(candidates)
            }
            ReferenceCatalogBackend::Memory(_) => Err(ReferenceCatalogError::StoreRequired),
            ReferenceCatalogBackend::RecoveryRequired(_) => {
                Err(ReferenceCatalogError::RecoveryRequired)
            }
        }
    }

    pub(crate) fn validate_delta(
        &self,
        delta: &ReferenceCatalogDeltaV2,
    ) -> Result<(), ReferenceCatalogError> {
        match &self.backend {
            ReferenceCatalogBackend::Store(store)
            | ReferenceCatalogBackend::RecoveryRequired(store) => {
                #[cfg(test)]
                self.full_delta_validations
                    .set(self.full_delta_validations.get().saturating_add(1));
                store.validate_delta(delta)
            }
            ReferenceCatalogBackend::Construction { .. } => delta.validate(),
            ReferenceCatalogBackend::Memory(_) => delta.validate(),
        }
    }

    #[cfg(test)]
    pub(crate) fn hot_entry_count(&self) -> usize {
        match &self.backend {
            ReferenceCatalogBackend::Memory(memory) => memory.postings.len(),
            ReferenceCatalogBackend::Store(_) | ReferenceCatalogBackend::RecoveryRequired(_) => 0,
            ReferenceCatalogBackend::Construction { .. } => 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn store_stats(&self) -> PatriciaIndexStats {
        match &self.backend {
            ReferenceCatalogBackend::Store(store)
            | ReferenceCatalogBackend::Construction { store, .. }
            | ReferenceCatalogBackend::RecoveryRequired(store) => store.stats(),
            ReferenceCatalogBackend::Memory(_) => PatriciaIndexStats::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn construction_work_stats(&self) -> ReferenceCatalogConstructionWorkStats {
        let patricia = match &self.backend {
            ReferenceCatalogBackend::Construction { patricia, .. } => patricia.borrow().stats(),
            _ => self.completed_construction_stats.unwrap_or_default(),
        };
        let attribution = self.prepare_attribution.get();
        let persistent = self.store_stats();
        ReferenceCatalogConstructionWorkStats {
            peak_resident_bytes: patricia.peak_resident_bytes,
            buffer_flushes: patricia.flushes,
            prepared_candidate_validations: self.prepared_candidate_validations.get(),
            full_delta_validations: self.full_delta_validations.get(),
            final_catalog_validations: self.final_catalog_validations.get(),
            extraction_nanos: attribution.extraction_nanos,
            posting_transition_publication_nanos: attribution.posting_transition_publication_nanos,
            facts_coverage_patricia_nanos: attribution.facts_coverage_patricia_nanos,
            reverse_patricia_nanos: attribution.reverse_patricia_nanos,
            facts_coverage_patricia_reads: attribution.facts_coverage_patricia_reads,
            reverse_patricia_reads: attribution.reverse_patricia_reads,
            prepared_sources: attribution.prepared_sources,
            fact_updates: attribution.fact_updates,
            reverse_updates: attribution.reverse_updates,
            persistent_node_reads: persistent.reads,
            persistent_node_writes: persistent.writes,
            logical_node_writes: patricia.logical_node_writes,
            loose_publication_calls: patricia.loose_publication_calls,
            pack_publication_calls: patricia.pack_publication_calls,
            catalog_publication_calls: patricia.catalog_publication_calls,
            immutable_publication_calls: patricia.immutable_publication_calls,
            head_transitions: patricia.head_transitions,
            durability_barriers: patricia.durability_barriers,
            packed_bytes: patricia.packed_bytes,
            capacity_fallbacks: patricia.capacity_fallbacks,
        }
    }

    pub(crate) fn prepare(
        &self,
        sources: BTreeMap<PageId, Option<ReferenceSourcePageV1>>,
        page_names: &PageNameOwnershipRootV1,
        external_uuid_claim_authority_root: ContentDigest,
    ) -> Result<ReferenceCatalogCandidateV2, ReferenceCatalogError> {
        self.ensure_ready()?;
        if sources.len() > MAX_REFERENCE_CATALOG_DELTA_SOURCES {
            return Err(ReferenceCatalogError::TooManySources(sources.len()));
        }
        match &self.backend {
            ReferenceCatalogBackend::Memory(memory) => self.prepare_memory(
                memory,
                sources,
                page_names,
                external_uuid_claim_authority_root,
            ),
            ReferenceCatalogBackend::Store(store) => self.prepare_store(
                store,
                sources,
                page_names,
                external_uuid_claim_authority_root,
            ),
            ReferenceCatalogBackend::Construction {
                store,
                construction_id,
                patricia,
            } => self.prepare_store_construction(
                store,
                *construction_id,
                &mut patricia.borrow_mut(),
                sources,
                page_names,
                external_uuid_claim_authority_root,
            ),
            ReferenceCatalogBackend::RecoveryRequired(_) => {
                Err(ReferenceCatalogError::RecoveryRequired)
            }
        }
    }

    fn prepare_memory(
        &self,
        prior: &MemoryCatalog,
        sources: BTreeMap<PageId, Option<ReferenceSourcePageV1>>,
        page_names: &PageNameOwnershipRootV1,
        external_uuid_claim_authority_root: ContentDigest,
    ) -> Result<ReferenceCatalogCandidateV2, ReferenceCatalogError> {
        let additions = sources
            .iter()
            .filter(|(page_id, source)| source.is_some() && !prior.postings.contains_key(page_id))
            .count();
        let removals = sources
            .iter()
            .filter(|(page_id, source)| source.is_none() && prior.postings.contains_key(page_id))
            .count();
        let post_source_count = prior
            .postings
            .len()
            .checked_add(additions)
            .and_then(|count| count.checked_sub(removals))
            .ok_or(ReferenceCatalogError::Allocation)?;
        if post_source_count > MAX_EPHEMERAL_REFERENCE_CATALOG_SOURCES {
            return Err(ReferenceCatalogError::TooManySources(post_source_count));
        }
        let mut memory = prior.clone();
        let mut replacements = Vec::with_capacity(sources.len());
        for (page_id, source) in sources {
            let prior_posting_digest = memory.facts.get(&page_id).copied();
            let prior_reverse_keys = memory
                .postings
                .get(&page_id)
                .map(reverse_candidate_keys)
                .unwrap_or_default();
            let posting = source
                .map(|source| extract_source_posting(&self.policy, source))
                .transpose()?;
            let post_reverse_keys = posting
                .as_ref()
                .map(reverse_candidate_keys)
                .unwrap_or_default();
            for key in prior_reverse_keys.difference(&post_reverse_keys) {
                memory.reverse_candidates.remove(key);
            }
            memory.reverse_candidates.extend(post_reverse_keys);
            let post_posting = match posting {
                Some(posting) => {
                    let digest = posting.digest()?;
                    let encoded = encode_canonical(&posting)?;
                    memory.facts.insert(page_id, digest);
                    memory.coverage.insert(page_id);
                    memory.postings.insert(page_id, posting);
                    Some(ReferencePostingRefV2 {
                        source_page_id: page_id,
                        digest,
                        encoded_byte_length: encoded.len() as u64,
                        fact_count: memory.postings[&page_id].facts.len() as u64,
                    })
                }
                None => {
                    memory.facts.remove(&page_id);
                    memory.coverage.remove(&page_id);
                    memory.postings.remove(&page_id);
                    None
                }
            };
            replacements.push(ReferenceCatalogReplacementV2 {
                page_id,
                prior_posting_digest,
                post_posting,
            });
        }
        let post_root = ReferenceCatalogRootV2::new(
            &self.policy,
            memory.coverage.len() as u64,
            memory_coverage_digest(&memory.coverage),
            memory_facts_digest(&memory.facts),
            memory_reverse_digest(&memory.reverse_candidates),
            page_names,
            external_uuid_claim_authority_root,
        )?;
        let transition = if replacements.is_empty() {
            ReferenceTransitionBindingV2::Empty
        } else {
            ReferenceTransitionBindingV2::Inline(replacements)
        };
        let delta = ReferenceCatalogDeltaV2 {
            schema_version: REFERENCE_CATALOG_SCHEMA_VERSION,
            prior_root: self.root.clone(),
            post_root,
            transition,
        };
        delta.encode()?;
        Ok(ReferenceCatalogCandidateV2 {
            delta,
            memory: Some(memory),
            construction: None,
        })
    }

    fn prepare_store(
        &self,
        store: &Arc<ReferenceCatalogStore>,
        sources: BTreeMap<PageId, Option<ReferenceSourcePageV1>>,
        page_names: &PageNameOwnershipRootV1,
        external_uuid_claim_authority_root: ContentDigest,
    ) -> Result<ReferenceCatalogCandidateV2, ReferenceCatalogError> {
        let mut facts_root = PatriciaIndexRoot::from_digest(self.root.facts_root);
        let mut coverage_root = PatriciaIndexRoot::from_digest(self.root.source_coverage_root);
        let mut reverse_root = PatriciaIndexRoot::from_digest(self.root.reverse_candidates_root);
        let mut source_count = self.root.source_count;
        let mut replacements = Vec::with_capacity(sources.len());
        let mut fact_updates = BTreeMap::new();
        let mut coverage_updates = BTreeMap::new();
        let mut removals = Vec::new();
        let mut reverse_updates = BTreeMap::new();
        let mut reverse_removals = BTreeSet::new();
        for (page_id, source) in sources {
            let key = page_id.as_uuid().as_bytes().to_vec();
            let prior_posting_digest = store
                .patricia
                .lookup(facts_root, &key)
                .map_err(store_error)?
                .map(|value| digest_value(&value))
                .transpose()?;
            let prior_reverse_keys = prior_posting_digest
                .map(|digest| {
                    let reference = store.posting_reference(page_id, digest)?;
                    store
                        .read_posting(&reference)
                        .map(|posting| reverse_candidate_keys(&posting))
                })
                .transpose()?
                .unwrap_or_default();
            let posting = source
                .map(|source| extract_source_posting(&self.policy, source))
                .transpose()?;
            let post_reverse_keys = posting
                .as_ref()
                .map(reverse_candidate_keys)
                .unwrap_or_default();
            reverse_removals.extend(prior_reverse_keys.difference(&post_reverse_keys).cloned());
            reverse_updates.extend(
                post_reverse_keys
                    .into_iter()
                    .map(|key| (key, REVERSE_VALUE.to_vec())),
            );
            let post_posting = match posting {
                Some(posting) => {
                    let reference = store.publish_posting(&posting)?;
                    fact_updates.insert(key.clone(), reference.digest.as_bytes().to_vec());
                    coverage_updates.insert(key.clone(), COVERAGE_VALUE.to_vec());
                    if prior_posting_digest.is_none() {
                        source_count = source_count
                            .checked_add(1)
                            .ok_or(ReferenceCatalogError::Allocation)?;
                    }
                    Some(reference)
                }
                None => {
                    removals.push(key);
                    if prior_posting_digest.is_some() {
                        source_count = source_count
                            .checked_sub(1)
                            .ok_or(ReferenceCatalogError::MalformedTransition)?;
                    }
                    None
                }
            };
            replacements.push(ReferenceCatalogReplacementV2 {
                page_id,
                prior_posting_digest,
                post_posting,
            });
        }
        facts_root = store
            .patricia
            .insert_many(facts_root, &fact_updates)
            .map_err(store_error)?;
        coverage_root = store
            .patricia
            .insert_many(coverage_root, &coverage_updates)
            .map_err(store_error)?;
        facts_root = store
            .patricia
            .remove_many(facts_root, &removals)
            .map_err(store_error)?;
        coverage_root = store
            .patricia
            .remove_many(coverage_root, &removals)
            .map_err(store_error)?;
        reverse_root = store
            .patricia
            .insert_many(reverse_root, &reverse_updates)
            .map_err(store_error)?;
        reverse_root = store
            .patricia
            .remove_many(
                reverse_root,
                &reverse_removals.into_iter().collect::<Vec<_>>(),
            )
            .map_err(store_error)?;
        let post_root = ReferenceCatalogRootV2::new(
            &self.policy,
            source_count,
            coverage_root.digest(),
            facts_root.digest(),
            reverse_root.digest(),
            page_names,
            external_uuid_claim_authority_root,
        )?;
        let transition = if replacements.is_empty() {
            ReferenceTransitionBindingV2::Empty
        } else {
            ReferenceTransitionBindingV2::Stored(store.publish_transition(&replacements)?)
        };
        let delta = ReferenceCatalogDeltaV2 {
            schema_version: REFERENCE_CATALOG_SCHEMA_VERSION,
            prior_root: self.root.clone(),
            post_root,
            transition,
        };
        // The engine independently replays this delta against durable catalog
        // authority in `persist_durable_final_status_with_binding`, before the
        // candidate can be committed to hot state. Replaying it here as well
        // only reconstructs the same authenticated trees a second time.
        delta.encode()?;
        Ok(ReferenceCatalogCandidateV2 {
            delta,
            memory: None,
            construction: None,
        })
    }

    fn prepare_store_construction(
        &self,
        store: &Arc<ReferenceCatalogStore>,
        construction_id: uuid::Uuid,
        construction: &mut PatriciaIndexConstruction,
        sources: BTreeMap<PageId, Option<ReferenceSourcePageV1>>,
        page_names: &PageNameOwnershipRootV1,
        external_uuid_claim_authority_root: ContentDigest,
    ) -> Result<ReferenceCatalogCandidateV2, ReferenceCatalogError> {
        #[cfg(test)]
        let mut attribution = ReferenceCatalogPrepareAttributionStats {
            prepared_sources: sources.len(),
            ..ReferenceCatalogPrepareAttributionStats::default()
        };
        let mut facts_root = PatriciaIndexRoot::from_digest(self.root.facts_root);
        let mut coverage_root = PatriciaIndexRoot::from_digest(self.root.source_coverage_root);
        let mut reverse_root = PatriciaIndexRoot::from_digest(self.root.reverse_candidates_root);
        construction.set_live_roots([facts_root, coverage_root, reverse_root]);
        let mut source_count = self.root.source_count;
        let mut replacements = Vec::with_capacity(sources.len());
        let mut fact_updates = BTreeMap::new();
        let mut coverage_updates = BTreeMap::new();
        let mut removals = Vec::new();
        let mut reverse_updates = BTreeMap::new();
        let mut reverse_removals = BTreeSet::new();
        for (page_id, source) in sources {
            let key = page_id.as_uuid().as_bytes().to_vec();
            let prior_posting_digest = store
                .patricia
                .construction_lookup(construction, facts_root, &key)
                .map_err(store_error)?
                .map(|value| digest_value(&value))
                .transpose()?;
            let prior_reverse_keys = prior_posting_digest
                .map(|digest| {
                    let reference = store.posting_reference(page_id, digest)?;
                    store
                        .read_posting(&reference)
                        .map(|posting| reverse_candidate_keys(&posting))
                })
                .transpose()?
                .unwrap_or_default();
            #[cfg(test)]
            let extraction_started = Instant::now();
            let posting = source
                .map(|source| extract_source_posting(&self.policy, source))
                .transpose()?;
            #[cfg(test)]
            {
                attribution.extraction_nanos = attribution
                    .extraction_nanos
                    .saturating_add(extraction_started.elapsed().as_nanos());
            }
            let post_reverse_keys = posting
                .as_ref()
                .map(reverse_candidate_keys)
                .unwrap_or_default();
            reverse_removals.extend(prior_reverse_keys.difference(&post_reverse_keys).cloned());
            reverse_updates.extend(
                post_reverse_keys
                    .into_iter()
                    .map(|key| (key, REVERSE_VALUE.to_vec())),
            );
            let post_posting = match posting {
                Some(posting) => {
                    #[cfg(test)]
                    let publication_started = Instant::now();
                    let reference = store.publish_posting(&posting)?;
                    #[cfg(test)]
                    {
                        attribution.posting_transition_publication_nanos = attribution
                            .posting_transition_publication_nanos
                            .saturating_add(publication_started.elapsed().as_nanos());
                    }
                    fact_updates.insert(key.clone(), reference.digest.as_bytes().to_vec());
                    coverage_updates.insert(key.clone(), COVERAGE_VALUE.to_vec());
                    if prior_posting_digest.is_none() {
                        source_count = source_count
                            .checked_add(1)
                            .ok_or(ReferenceCatalogError::Allocation)?;
                    }
                    Some(reference)
                }
                None => {
                    removals.push(key);
                    if prior_posting_digest.is_some() {
                        source_count = source_count
                            .checked_sub(1)
                            .ok_or(ReferenceCatalogError::MalformedTransition)?;
                    }
                    None
                }
            };
            replacements.push(ReferenceCatalogReplacementV2 {
                page_id,
                prior_posting_digest,
                post_posting,
            });
        }
        #[cfg(test)]
        {
            attribution.fact_updates = fact_updates.len();
            attribution.reverse_updates = reverse_updates.len();
        }
        #[cfg(test)]
        let facts_coverage_started = Instant::now();
        #[cfg(test)]
        let facts_coverage_reads_before = store.stats().reads;
        facts_root = store
            .patricia
            .construction_insert_many_bulk(construction, facts_root, &fact_updates)
            .map_err(store_error)?;
        construction.set_live_roots([facts_root, coverage_root, reverse_root]);
        coverage_root = store
            .patricia
            .construction_insert_many_bulk(construction, coverage_root, &coverage_updates)
            .map_err(store_error)?;
        construction.set_live_roots([facts_root, coverage_root, reverse_root]);
        facts_root = store
            .patricia
            .construction_remove_many(construction, facts_root, &removals)
            .map_err(store_error)?;
        construction.set_live_roots([facts_root, coverage_root, reverse_root]);
        coverage_root = store
            .patricia
            .construction_remove_many(construction, coverage_root, &removals)
            .map_err(store_error)?;
        construction.set_live_roots([facts_root, coverage_root, reverse_root]);
        #[cfg(test)]
        {
            attribution.facts_coverage_patricia_nanos = facts_coverage_started.elapsed().as_nanos();
            attribution.facts_coverage_patricia_reads = store
                .stats()
                .reads
                .saturating_sub(facts_coverage_reads_before);
        }
        #[cfg(test)]
        let reverse_started = Instant::now();
        #[cfg(test)]
        let reverse_reads_before = store.stats().reads;
        reverse_root = store
            .patricia
            .construction_insert_many_bulk(construction, reverse_root, &reverse_updates)
            .map_err(store_error)?;
        construction.set_live_roots([facts_root, coverage_root, reverse_root]);
        reverse_root = store
            .patricia
            .construction_remove_many(
                construction,
                reverse_root,
                &reverse_removals.into_iter().collect::<Vec<_>>(),
            )
            .map_err(store_error)?;
        construction.set_live_roots([facts_root, coverage_root, reverse_root]);
        #[cfg(test)]
        {
            attribution.reverse_patricia_nanos = reverse_started.elapsed().as_nanos();
            attribution.reverse_patricia_reads =
                store.stats().reads.saturating_sub(reverse_reads_before);
        }
        construction.checkpoint([facts_root, coverage_root, reverse_root]);
        let post_root = ReferenceCatalogRootV2::new(
            &self.policy,
            source_count,
            coverage_root.digest(),
            facts_root.digest(),
            reverse_root.digest(),
            page_names,
            external_uuid_claim_authority_root,
        )?;
        let transition = if replacements.is_empty() {
            ReferenceTransitionBindingV2::Empty
        } else {
            #[cfg(test)]
            let publication_started = Instant::now();
            let transition = store.publish_transition(&replacements)?;
            #[cfg(test)]
            {
                attribution.posting_transition_publication_nanos = attribution
                    .posting_transition_publication_nanos
                    .saturating_add(publication_started.elapsed().as_nanos());
            }
            ReferenceTransitionBindingV2::Stored(transition)
        };
        let delta = ReferenceCatalogDeltaV2 {
            schema_version: REFERENCE_CATALOG_SCHEMA_VERSION,
            prior_root: self.root.clone(),
            post_root,
            transition,
        };
        delta.encode()?;
        #[cfg(test)]
        {
            let prior = self.prepare_attribution.get();
            self.prepare_attribution
                .set(ReferenceCatalogPrepareAttributionStats {
                    extraction_nanos: prior
                        .extraction_nanos
                        .saturating_add(attribution.extraction_nanos),
                    posting_transition_publication_nanos: prior
                        .posting_transition_publication_nanos
                        .saturating_add(attribution.posting_transition_publication_nanos),
                    facts_coverage_patricia_nanos: prior
                        .facts_coverage_patricia_nanos
                        .saturating_add(attribution.facts_coverage_patricia_nanos),
                    reverse_patricia_nanos: prior
                        .reverse_patricia_nanos
                        .saturating_add(attribution.reverse_patricia_nanos),
                    facts_coverage_patricia_reads: prior
                        .facts_coverage_patricia_reads
                        .saturating_add(attribution.facts_coverage_patricia_reads),
                    reverse_patricia_reads: prior
                        .reverse_patricia_reads
                        .saturating_add(attribution.reverse_patricia_reads),
                    prepared_sources: prior
                        .prepared_sources
                        .saturating_add(attribution.prepared_sources),
                    fact_updates: prior.fact_updates.saturating_add(attribution.fact_updates),
                    reverse_updates: prior
                        .reverse_updates
                        .saturating_add(attribution.reverse_updates),
                });
        }
        Ok(ReferenceCatalogCandidateV2 {
            delta,
            memory: None,
            construction: Some(ReferenceCatalogConstructionEvidenceV2 {
                construction_id,
                store: Arc::clone(store),
                structurally_validated: Cell::new(false),
            }),
        })
    }

    pub(crate) fn validate_prepared_candidate(
        &self,
        prepared: ReferenceCatalogPreparedCandidateV2<'_>,
        accepted_delta: &ReferenceCatalogDeltaV2,
        accepted_root: &ReferenceCatalogRootV2,
    ) -> Result<(), ReferenceCatalogError> {
        let ReferenceCatalogBackend::Construction {
            store,
            construction_id,
            ..
        } = &self.backend
        else {
            return Err(ReferenceCatalogError::AuthorityMismatch);
        };
        prepared.candidate.delta.validate()?;
        if prepared.evidence.construction_id != *construction_id
            || !Arc::ptr_eq(&prepared.evidence.store, store)
            || prepared.candidate.delta != *accepted_delta
            || prepared.candidate.delta.prior_root != self.root
            || prepared.candidate.delta.post_root != *accepted_root
            || prepared.candidate.root() != accepted_root
        {
            return Err(ReferenceCatalogError::AuthorityMismatch);
        }
        prepared.evidence.structurally_validated.set(true);
        #[cfg(test)]
        self.prepared_candidate_validations
            .set(self.prepared_candidate_validations.get().saturating_add(1));
        Ok(())
    }

    pub(crate) fn finish_construction(
        &mut self,
    ) -> Result<Option<CompletedPatriciaConstruction>, ReferenceCatalogError> {
        let (store, completion) = match &mut self.backend {
            ReferenceCatalogBackend::Construction {
                store, patricia, ..
            } => {
                let completion = store
                    .patricia
                    .finish_construction(patricia.get_mut())
                    .map_err(store_error)?;
                #[cfg(test)]
                {
                    self.completed_construction_stats = Some(patricia.get_mut().stats());
                }
                (Arc::clone(store), completion)
            }
            ReferenceCatalogBackend::Memory(_)
            | ReferenceCatalogBackend::Store(_)
            | ReferenceCatalogBackend::RecoveryRequired(_) => return Ok(None),
        };
        // This is the sole construction-time full-catalog proof. It runs only
        // after every staged node and posting is immutable-published, before
        // the detached candidate can leave its authoring session.
        store.validate_catalog_root(&self.root)?;
        #[cfg(test)]
        self.final_catalog_validations
            .set(self.final_catalog_validations.get().saturating_add(1));
        self.backend = ReferenceCatalogBackend::Store(store);
        Ok(Some(completion))
    }

    pub(crate) fn commit(&mut self, candidate: ReferenceCatalogCandidateV2) {
        assert_eq!(candidate.delta.prior_root, self.root);
        assert!(
            candidate
                .construction
                .as_ref()
                .is_none_or(|evidence| evidence.structurally_validated.get()),
            "construction candidate must be structurally cross-checked before commit"
        );
        if let Some(memory) = candidate.memory {
            self.backend = ReferenceCatalogBackend::Memory(memory);
        }
        self.root = candidate.delta.post_root;
    }
}

pub(crate) fn affected_reference_sources(effect: &SemanticEffect) -> BTreeSet<PageId> {
    let mut pages = BTreeSet::new();
    pages.extend(effect.pages().iter().map(|delta| delta.page_id));
    pages.extend(effect.page_preambles().iter().map(|delta| delta.page_id));
    for delta in effect.blocks() {
        for state in [&delta.before, &delta.after].into_iter().flatten() {
            if let BlockOwner::Page(page_id) = state.owner {
                pages.insert(page_id);
            }
        }
    }
    pages.extend(effect.memberships().iter().map(|delta| delta.page_id));
    pages
}

pub(crate) fn reference_source_is_org(path: &ManagedPath) -> bool {
    path.as_str().ends_with(".org")
}

fn extract_source_posting(
    policy: &ReferenceCatalogPolicyV1,
    source: ReferenceSourcePageV1,
) -> Result<ReferenceSourcePostingV2, ReferenceCatalogError> {
    let mut facts = Vec::new();
    if let Some(preamble) = &source.preamble {
        extract_text_facts(
            policy,
            preamble,
            source.is_org,
            ReferenceSourceLocatorV1::Preamble,
            &mut facts,
        )?;
    }
    for block in &source.blocks {
        extract_text_facts(
            policy,
            &block.content,
            source.is_org,
            ReferenceSourceLocatorV1::Block {
                block_id: block.block_id,
                home_document_id: block.home_document_id,
            },
            &mut facts,
        )?;
    }
    ReferenceSourcePostingV2::new(source.page_id, facts)
}

fn extract_text_facts(
    policy: &ReferenceCatalogPolicyV1,
    raw: &str,
    is_org: bool,
    source: ReferenceSourceLocatorV1,
    facts: &mut Vec<ReferenceFactV1>,
) -> Result<(), ReferenceCatalogError> {
    // `reference_evidence::project` derives facts only from parser nodes whose
    // surface syntax includes one of these bytes: links and nested links `[`,
    // tags `#`, block references `(`, embeds `{`, or Markdown/Org property
    // declarations `:`. With none present, parsing cannot produce evidence.
    // This is deliberately a negative gate: every possible evidence-bearing
    // source still follows the parser-owned V1 extraction path below.
    if !may_contain_reference_evidence(raw) {
        return Ok(());
    }
    #[cfg(test)]
    REFERENCE_EVIDENCE_PARSE_CALLS.fetch_add(1, Ordering::Relaxed);
    let parsed = crate::render::parse_projection(raw, is_org);
    let projection = crate::reference_evidence::project(raw, is_org, &parsed.blocks);
    facts
        .try_reserve(
            projection
                .explicit
                .len()
                .saturating_add(projection.block_references.len()),
        )
        .map_err(|_| ReferenceCatalogError::Allocation)?;
    for reference in projection.explicit {
        let raw_target = reference.name.trim().to_owned();
        if raw_target.is_empty() {
            continue;
        }
        let kind = match (reference.rule, reference.property_key.as_deref()) {
            ("explicit_property_key", _) => {
                if !policy.property_key_enabled(&raw_target) {
                    continue;
                }
                PageReferenceKindV1::PropertyKeyPseudoPage
            }
            (_, Some("alias" | "aliases")) => PageReferenceKindV1::AliasDeclaration,
            ("implicit_linkable_property", _) => PageReferenceKindV1::LinkablePropertyValue,
            ("explicit_tag", _) => PageReferenceKindV1::Tag,
            ("explicit_embed", _) => PageReferenceKindV1::PageEmbed,
            _ => PageReferenceKindV1::PageLink,
        };
        let logical = LogicalPageName::parse(raw_target.clone())
            .map_err(|_| ReferenceCatalogError::MalformedFact)?;
        facts.push(ReferenceFactV1::PageName(PageNameReferenceFactV1 {
            source,
            kind,
            normalized_target: crate::refs::normalize(&raw_target),
            target_key: logical.key_digest(),
            raw_target,
            byte_start: u32::try_from(reference.range.start)
                .map_err(|_| ReferenceCatalogError::MalformedFact)?,
            byte_end: u32::try_from(reference.range.end)
                .map_err(|_| ReferenceCatalogError::MalformedFact)?,
        }));
    }
    for reference in projection.block_references {
        let raw_claim = reference.raw_claim.trim().to_owned();
        let Ok(logseq_uuid) = LogseqUuid::parse(&raw_claim) else {
            continue;
        };
        facts.push(ReferenceFactV1::Block(BlockReferenceFactV1 {
            source,
            kind: match reference.kind {
                crate::reference_evidence::ProjectedBlockRefKind::Reference => {
                    BlockReferenceKindV1::Reference
                }
                crate::reference_evidence::ProjectedBlockRefKind::Embed => {
                    BlockReferenceKindV1::Embed
                }
            },
            raw_claim,
            logseq_uuid,
            byte_start: u32::try_from(reference.range.start)
                .map_err(|_| ReferenceCatalogError::MalformedFact)?,
            byte_end: u32::try_from(reference.range.end)
                .map_err(|_| ReferenceCatalogError::MalformedFact)?,
        }));
    }
    Ok(())
}

fn may_contain_reference_evidence(raw: &str) -> bool {
    raw.as_bytes()
        .iter()
        .any(|byte| matches!(byte, b'[' | b'#' | b'(' | b'{' | b':'))
}

fn reverse_candidate_keys(posting: &ReferenceSourcePostingV2) -> BTreeSet<Vec<u8>> {
    posting
        .facts
        .iter()
        .map(|fact| {
            let target = match fact {
                ReferenceFactV1::PageName(fact)
                    if fact.kind == PageReferenceKindV1::AliasDeclaration =>
                {
                    ReferenceCandidateTargetV2::PageAlias(fact.target_key)
                }
                ReferenceFactV1::PageName(fact) => {
                    ReferenceCandidateTargetV2::PageName(fact.target_key)
                }
                ReferenceFactV1::Block(fact) => {
                    ReferenceCandidateTargetV2::BlockUuid(fact.logseq_uuid)
                }
            };
            let mut key = reverse_candidate_prefix(target);
            key.extend_from_slice(posting.source_page_id.as_uuid().as_bytes());
            key
        })
        .collect()
}

fn reverse_candidate_prefix(target: ReferenceCandidateTargetV2) -> Vec<u8> {
    match target {
        ReferenceCandidateTargetV2::PageName(target_key) => {
            let mut prefix = Vec::with_capacity(33);
            prefix.push(PAGE_REFERENCE_PREFIX);
            prefix.extend_from_slice(target_key.as_bytes());
            prefix
        }
        ReferenceCandidateTargetV2::PageAlias(target_key) => {
            let mut prefix = Vec::with_capacity(33);
            prefix.push(PAGE_ALIAS_PREFIX);
            prefix.extend_from_slice(target_key.as_bytes());
            prefix
        }
        ReferenceCandidateTargetV2::BlockUuid(logseq_uuid) => {
            let mut prefix = Vec::with_capacity(17);
            prefix.push(BLOCK_UUID_PREFIX);
            prefix.extend_from_slice(logseq_uuid.as_uuid().as_bytes());
            prefix
        }
    }
}

fn decode_reverse_candidate_key(key: &[u8]) -> Result<(Vec<u8>, PageId), ReferenceCatalogError> {
    let prefix_len = match key.first() {
        Some(&PAGE_REFERENCE_PREFIX) | Some(&PAGE_ALIAS_PREFIX) if key.len() == 49 => 33,
        Some(&BLOCK_UUID_PREFIX) if key.len() == 33 => 17,
        _ => return Err(ReferenceCatalogError::MalformedRoot),
    };
    let page_bytes: [u8; 16] = key[prefix_len..]
        .try_into()
        .map_err(|_| ReferenceCatalogError::MalformedRoot)?;
    Ok((
        key[..prefix_len].to_vec(),
        PageId::from_uuid(uuid::Uuid::from_bytes(page_bytes)),
    ))
}

fn memory_facts_digest(facts: &BTreeMap<PageId, ContentDigest>) -> ContentDigest {
    if facts.is_empty() {
        return empty_map_digest();
    }
    let mut bytes = b"tine/reference-catalog/test-memory-facts/v2".to_vec();
    for (page_id, digest) in facts {
        bytes.extend_from_slice(page_id.as_uuid().as_bytes());
        bytes.extend_from_slice(digest.as_bytes());
    }
    ContentDigest::of(&bytes)
}

fn memory_reverse_digest(reverse_candidates: &BTreeSet<Vec<u8>>) -> ContentDigest {
    if reverse_candidates.is_empty() {
        return empty_map_digest();
    }
    let mut bytes = b"tine/reference-catalog/test-memory-reverse/v2".to_vec();
    for key in reverse_candidates {
        bytes.extend_from_slice(&(key.len() as u64).to_be_bytes());
        bytes.extend_from_slice(key);
    }
    ContentDigest::of(&bytes)
}

fn memory_coverage_digest(coverage: &BTreeSet<PageId>) -> ContentDigest {
    if coverage.is_empty() {
        return empty_map_digest();
    }
    let mut bytes = b"tine/reference-catalog/test-memory-coverage/v2".to_vec();
    for page_id in coverage {
        bytes.extend_from_slice(page_id.as_uuid().as_bytes());
    }
    ContentDigest::of(&bytes)
}

fn empty_map_digest() -> ContentDigest {
    PatriciaIndexRoot::empty().digest()
}

fn extractor_digest() -> ContentDigest {
    let mut bytes = b"tine/reference-catalog/extractor/v2\0".to_vec();
    bytes.extend_from_slice(&REFERENCE_CATALOG_EXTRACTOR_VERSION.to_be_bytes());
    bytes.extend_from_slice(crate::reference_evidence::ENGINE_VERSION.as_bytes());
    ContentDigest::of(&bytes)
}

fn posting_filename(digest: ContentDigest) -> String {
    format!("{digest}.reference-posting")
}

fn chunk_filename(digest: ContentDigest) -> String {
    format!("{digest}.reference-chunk")
}

fn transition_filename(digest: ContentDigest) -> String {
    format!("{digest}.reference-transition")
}

fn read_content_addressed(
    directory: &Dir,
    filename: &str,
    digest: ContentDigest,
    expected_length: u64,
) -> Result<Vec<u8>, ReferenceCatalogError> {
    if expected_length == 0 || expected_length > MAX_REFERENCE_OBJECT_BYTES {
        return Err(ReferenceCatalogError::MalformedPosting);
    }
    let bytes = read_optional_regular(
        directory,
        filename,
        MAX_REFERENCE_OBJECT_BYTES,
        Some(expected_length),
    )
    .map_err(store_error)?
    .ok_or(ReferenceCatalogError::MissingObject(digest))?;
    if ContentDigest::of(&bytes) != digest {
        return Err(ReferenceCatalogError::ObjectDigestMismatch(digest));
    }
    Ok(bytes)
}

fn digest_value(value: &[u8]) -> Result<ContentDigest, ReferenceCatalogError> {
    let bytes: [u8; 32] = value
        .try_into()
        .map_err(|_| ReferenceCatalogError::MalformedTransition)?;
    Ok(ContentDigest::from_bytes(bytes))
}

fn page_id_key(key: &[u8]) -> Result<PageId, ReferenceCatalogError> {
    let bytes: [u8; 16] = key
        .try_into()
        .map_err(|_| ReferenceCatalogError::MalformedRoot)?;
    Ok(PageId::from_uuid(uuid::Uuid::from_bytes(bytes)))
}

fn require_object_size(bytes: &[u8]) -> Result<(), ReferenceCatalogError> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_REFERENCE_OBJECT_BYTES {
        return Err(ReferenceCatalogError::TooLarge(bytes.len()));
    }
    Ok(())
}

fn store_error(error: StoreError) -> ReferenceCatalogError {
    ReferenceCatalogError::Store(error.to_string())
}

fn domain_digest(domain: &[u8], encoded: &[u8]) -> Result<ContentDigest, ReferenceCatalogError> {
    let mut bytes = Vec::with_capacity(domain.len() + 8 + encoded.len());
    bytes.extend_from_slice(domain);
    bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
    bytes.extend_from_slice(encoded);
    Ok(ContentDigest::of(&bytes))
}

fn encode_canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, ReferenceCatalogError> {
    postcard::to_allocvec(value).map_err(|error| ReferenceCatalogError::Encode(error.to_string()))
}

fn decode_canonical<T: for<'de> Deserialize<'de> + Serialize>(
    bytes: &[u8],
) -> Result<T, ReferenceCatalogError> {
    let value: T = postcard::from_bytes(bytes)
        .map_err(|error| ReferenceCatalogError::Decode(error.to_string()))?;
    if encode_canonical(&value)? != bytes {
        return Err(ReferenceCatalogError::NonCanonical);
    }
    Ok(value)
}

fn require_version(
    component: &'static str,
    found: u32,
    expected: u32,
) -> Result<(), ReferenceCatalogError> {
    if found != expected {
        return Err(ReferenceCatalogError::UnknownVersion {
            component,
            found,
            expected,
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReferenceCatalogError {
    Allocation,
    Authority(String),
    AuthorityMismatch,
    Decode(String),
    Encode(String),
    MalformedFact,
    MalformedPosting,
    MalformedRoot,
    MalformedTransition,
    MissingObject(ContentDigest),
    NonCanonical,
    ObjectDigestMismatch(ContentDigest),
    RecoveryRequired,
    StoreRequired,
    Store(String),
    TooLarge(usize),
    TooManyCandidates(usize),
    TooManySources(usize),
    UnknownVersion {
        component: &'static str,
        found: u32,
        expected: u32,
    },
}

impl fmt::Display for ReferenceCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allocation => formatter.write_str("reference catalog allocation failed"),
            Self::Authority(error) => write!(formatter, "reference authority failed: {error}"),
            Self::AuthorityMismatch => formatter.write_str("reference authority binding mismatch"),
            Self::Decode(error) => write!(formatter, "reference catalog decode failed: {error}"),
            Self::Encode(error) => write!(formatter, "reference catalog encode failed: {error}"),
            Self::MalformedFact => formatter.write_str("malformed reference catalog fact"),
            Self::MalformedPosting => formatter.write_str("malformed reference catalog posting"),
            Self::MalformedRoot => formatter.write_str("malformed reference catalog root"),
            Self::MalformedTransition => {
                formatter.write_str("malformed reference catalog transition")
            }
            Self::MissingObject(digest) => {
                write!(formatter, "reference catalog object {digest} is missing")
            }
            Self::NonCanonical => formatter.write_str("non-canonical reference catalog encoding"),
            Self::ObjectDigestMismatch(digest) => {
                write!(
                    formatter,
                    "reference catalog object {digest} has mismatched bytes"
                )
            }
            Self::RecoveryRequired => {
                formatter.write_str("reference catalog authenticated recovery is required")
            }
            Self::StoreRequired => {
                formatter.write_str("reference catalog object store is required")
            }
            Self::Store(error) => write!(formatter, "reference catalog store failed: {error}"),
            Self::TooLarge(bytes) => {
                write!(
                    formatter,
                    "reference catalog encoding is too large: {bytes} bytes"
                )
            }
            Self::TooManyCandidates(count) => {
                write!(
                    formatter,
                    "reference target has too many candidate sources: {count}"
                )
            }
            Self::TooManySources(count) => {
                write!(formatter, "reference delta has too many sources: {count}")
            }
            Self::UnknownVersion {
                component,
                found,
                expected,
            } => write!(
                formatter,
                "unknown {component} version {found}; expected {expected}"
            ),
        }
    }
}

impl std::error::Error for ReferenceCatalogError {}

#[cfg(test)]
mod tests {
    use super::super::{ManagedTextKind, ObjectStore, WorkspaceId};
    use super::*;
    use uuid::Uuid;

    fn page(value: u128) -> PageId {
        PageId::from_uuid(Uuid::from_u128(value))
    }

    fn block(value: u128) -> BlockId {
        BlockId::from_uuid(Uuid::from_u128(value))
    }

    fn document(value: u128) -> DocumentId {
        DocumentId::from_uuid(Uuid::from_u128(value))
    }

    fn source_with_format(page_id: PageId, raw: &str, is_org: bool) -> ReferenceSourcePageV1 {
        ReferenceSourcePageV1 {
            page_id,
            is_org,
            preamble: None,
            blocks: vec![ReferenceSourceBlockV1 {
                block_id: block(page_id.as_uuid().as_u128() + 100),
                home_document_id: document(page_id.as_uuid().as_u128() + 200),
                content: raw.to_owned(),
            }],
        }
    }

    fn source(page_id: PageId, raw: &str) -> ReferenceSourcePageV1 {
        source_with_format(page_id, raw, false)
    }

    fn store(name: &str) -> (std::path::PathBuf, Arc<ReferenceCatalogStore>) {
        let path =
            std::env::temp_dir().join(format!("tine-reference-catalog-{name}-{}", Uuid::new_v4()));
        let objects =
            ObjectStore::open(&path, WorkspaceId::from_uuid(Uuid::from_u128(0x100))).unwrap();
        let catalog = Arc::new(objects.open_reference_catalog().unwrap());
        (path, catalog)
    }

    fn dense_source(
        page_index: usize,
        blocks_per_page: usize,
        targets_per_block: usize,
    ) -> ReferenceSourcePageV1 {
        let page_id = page(100_000 + page_index as u128 * 100);
        let blocks = (0..blocks_per_page)
            .map(|block_index| ReferenceSourceBlockV1 {
                block_id: block(page_id.as_uuid().as_u128() + block_index as u128 + 1),
                home_document_id: document(page_id.as_uuid().as_u128() + 50),
                content: (0..targets_per_block)
                    .map(|target_index| {
                        format!("[[Target {page_index:06} {block_index:02} {target_index:02}]]")
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            })
            .collect();
        ReferenceSourcePageV1 {
            page_id,
            is_org: false,
            preamble: None,
            blocks,
        }
    }

    fn dense_sources(
        page_range: std::ops::Range<usize>,
        blocks_per_page: usize,
        targets_per_block: usize,
    ) -> BTreeMap<PageId, Option<ReferenceSourcePageV1>> {
        page_range
            .map(|page_index| {
                let source = dense_source(page_index, blocks_per_page, targets_per_block);
                (source.page_id, Some(source))
            })
            .collect()
    }

    #[test]
    fn exact_markdown_org_unicode_and_opaque_evidence_share_one_projection() {
        let uuid = "6a55b643-1234-5678-9abc-def012345678";
        let markdown = "aliases:: 別名, [[東京]]\ncustom_key:: [[値]]";
        let posting = extract_source_posting(
            &ReferenceCatalogPolicyV1::default(),
            source(page(1), markdown),
        )
        .unwrap();
        for fact in posting.facts() {
            let ReferenceFactV1::PageName(fact) = fact else {
                continue;
            };
            assert!(markdown
                .get(fact.byte_start as usize..fact.byte_end as usize)
                .is_some());
        }
        assert!(posting.facts().iter().any(|fact| matches!(
            fact,
            ReferenceFactV1::PageName(fact)
                if fact.raw_target == "別名"
                    && &markdown[fact.byte_start as usize..fact.byte_end as usize] == "別名"
        )));

        let org =
            "task\n:PROPERTIES:\n:aliases: Org Alias, [[Org Two]]\n:custom_key: [[Value]]\n:END:";
        let posting = extract_source_posting(
            &ReferenceCatalogPolicyV1::default(),
            source_with_format(page(2), org, true),
        )
        .unwrap();
        assert!(posting.facts().iter().any(|fact| matches!(
            fact,
            ReferenceFactV1::PageName(fact)
                if fact.raw_target == "Org Alias"
                    && &org[fact.byte_start as usize..fact.byte_end as usize] == "Org Alias"
        )));
        assert!(posting.facts().iter().any(|fact| matches!(
            fact,
            ReferenceFactV1::PageName(fact)
                if fact.raw_target == "custom-key"
                    && &org[fact.byte_start as usize..fact.byte_end as usize] == "custom_key"
        )));
        assert!(posting.facts().iter().any(|fact| matches!(
            fact,
            ReferenceFactV1::PageName(fact)
                if fact.raw_target == "Value"
                    && org.get(fact.byte_start as usize..fact.byte_end as usize).is_some()
        )));

        let excluded = format!("`[[Code]] (({uuid}))`\n```\n[[Fence]] (({uuid}))\n```");
        let posting = extract_source_posting(
            &ReferenceCatalogPolicyV1::default(),
            source(page(3), &excluded),
        )
        .unwrap();
        assert!(posting.facts().is_empty());
    }

    #[test]
    fn repeated_block_reference_and_embed_occurrences_preserve_exact_ranges() {
        let uuid = "6a55b643-1234-5678-9abc-def012345678";
        let raw =
            format!("(({uuid})) then (({uuid})) {{{{embed (({uuid}))}}}} {{{{embed (({uuid}))}}}}");
        let posting =
            extract_source_posting(&ReferenceCatalogPolicyV1::default(), source(page(4), &raw))
                .unwrap();
        let facts = posting
            .facts()
            .iter()
            .filter_map(|fact| match fact {
                ReferenceFactV1::Block(fact) => Some(fact),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(facts.len(), 4);
        assert_eq!(
            facts
                .iter()
                .filter(|fact| fact.kind == BlockReferenceKindV1::Reference)
                .count(),
            2
        );
        assert_eq!(
            facts
                .iter()
                .filter(|fact| fact.kind == BlockReferenceKindV1::Embed)
                .count(),
            2
        );
        assert_eq!(
            facts
                .iter()
                .map(|fact| (fact.byte_start, fact.byte_end))
                .collect::<BTreeSet<_>>()
                .len(),
            4
        );
        for fact in facts {
            let occurrence = raw
                .get(fact.byte_start as usize..fact.byte_end as usize)
                .unwrap();
            match fact.kind {
                BlockReferenceKindV1::Reference => {
                    assert_eq!(occurrence, format!("(({uuid}))"));
                }
                BlockReferenceKindV1::Embed => {
                    assert_eq!(occurrence, format!("{{{{embed (({uuid}))}}}}"));
                }
            }
        }
    }

    #[test]
    fn ephemeral_catalog_capacity_fails_before_state_mutation() {
        let names = PageNameOwnershipRootV1::empty();
        let uuid_root = ContentDigest::of(b"uuid");
        let mut state =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        let sources = (1..=MAX_EPHEMERAL_REFERENCE_CATALOG_SOURCES as u128)
            .map(|value| {
                (
                    page(value),
                    Some(ReferenceSourcePageV1 {
                        page_id: page(value),
                        is_org: false,
                        preamble: None,
                        blocks: Vec::new(),
                    }),
                )
            })
            .collect();
        let candidate = state.prepare(sources, &names, uuid_root).unwrap();
        state.commit(candidate);
        let root = state.root().clone();
        assert_eq!(
            state.hot_entry_count(),
            MAX_EPHEMERAL_REFERENCE_CATALOG_SOURCES
        );

        let result = state.prepare(
            BTreeMap::from([(
                page(MAX_EPHEMERAL_REFERENCE_CATALOG_SOURCES as u128 + 1),
                Some(ReferenceSourcePageV1 {
                    page_id: page(MAX_EPHEMERAL_REFERENCE_CATALOG_SOURCES as u128 + 1),
                    is_org: false,
                    preamble: None,
                    blocks: Vec::new(),
                }),
            )]),
            &names,
            uuid_root,
        );
        assert_eq!(
            result.unwrap_err(),
            ReferenceCatalogError::TooManySources(MAX_EPHEMERAL_REFERENCE_CATALOG_SOURCES + 1)
        );
        assert_eq!(state.root(), &root);
        assert_eq!(
            state.hot_entry_count(),
            MAX_EPHEMERAL_REFERENCE_CATALOG_SOURCES
        );
    }

    #[test]
    fn prepared_construction_candidate_is_bound_to_exact_delta_roots_and_catalog() {
        let names = PageNameOwnershipRootV1::empty();
        let uuid_root = ContentDigest::of(b"prepared-construction-uuid-root");
        let (path, durable) = store("prepared-construction");
        let mut state =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        state
            .attach_construction_store(Arc::clone(&durable))
            .unwrap();
        let candidate = state
            .prepare(
                BTreeMap::from([(page(1), Some(source(page(1), "[[Target]]")))]),
                &names,
                uuid_root,
            )
            .unwrap();
        let prepared = candidate.prepared_candidate().unwrap();
        let delta = candidate.delta().clone();
        let prior_root = delta.prior_root.clone();
        let post_root = candidate.root().clone();

        assert_eq!(
            state.validate_prepared_candidate(prepared, &delta, &prior_root),
            Err(ReferenceCatalogError::AuthorityMismatch),
            "post root cannot be substituted"
        );
        let mut wrong_prior = delta.clone();
        wrong_prior.prior_root = post_root.clone();
        assert_eq!(
            state.validate_prepared_candidate(prepared, &wrong_prior, &post_root),
            Err(ReferenceCatalogError::AuthorityMismatch),
            "prior root cannot be substituted"
        );
        let mut wrong_post = delta.clone();
        wrong_post.post_root = prior_root.clone();
        assert_eq!(
            state.validate_prepared_candidate(prepared, &wrong_post, &post_root),
            Err(ReferenceCatalogError::AuthorityMismatch),
            "delta cannot be substituted"
        );

        let mut substituted_candidate_state =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        substituted_candidate_state
            .attach_construction_store(Arc::clone(&durable))
            .unwrap();
        let substituted_candidate = substituted_candidate_state
            .prepare(
                BTreeMap::from([(page(2), Some(source(page(2), "[[Other]]")))]),
                &names,
                uuid_root,
            )
            .unwrap();
        assert_eq!(
            state.validate_prepared_candidate(
                substituted_candidate.prepared_candidate().unwrap(),
                substituted_candidate.delta(),
                substituted_candidate.root(),
            ),
            Err(ReferenceCatalogError::AuthorityMismatch),
            "prepared candidate cannot be substituted even within the same catalog store"
        );

        let (_other_path, other_durable) = store("prepared-construction-other");
        let mut other =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        other.attach_construction_store(other_durable).unwrap();
        assert_eq!(
            other.validate_prepared_candidate(prepared, &delta, &post_root),
            Err(ReferenceCatalogError::AuthorityMismatch),
            "catalog capability cannot be substituted"
        );

        state
            .validate_prepared_candidate(prepared, &delta, &post_root)
            .unwrap();
        state.commit(candidate);
        state.finish_construction().unwrap();
        assert_eq!(state.root(), &post_root);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn construction_bulk_matches_legacy_multi_part_roots_and_deltas() {
        let names = PageNameOwnershipRootV1::empty();
        let uuid_root = ContentDigest::of(b"construction-bulk-multipart-differential");
        let (legacy_path, legacy_store) = store("construction-bulk-multipart-legacy");
        let (bulk_path, bulk_store) = store("construction-bulk-multipart-bulk");
        let mut legacy =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        legacy.attach_store(legacy_store).unwrap();
        let mut bulk =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        bulk.attach_construction_store(Arc::clone(&bulk_store))
            .unwrap();

        let mut second = dense_sources(48..96, 3, 4);
        second.extend(dense_sources(24..40, 2, 3));
        let mut third = dense_sources(80..112, 2, 5);
        for page_index in [3_usize, 31, 70, 90] {
            third.insert(page(100_000 + page_index as u128 * 100), None);
        }
        let parts = [dense_sources(0..64, 3, 4), second, third];
        let mut historical_roots = Vec::new();
        for sources in parts {
            let legacy_candidate = legacy.prepare(sources.clone(), &names, uuid_root).unwrap();
            let bulk_candidate = bulk.prepare(sources, &names, uuid_root).unwrap();
            assert_eq!(legacy_candidate.root(), bulk_candidate.root());
            assert_eq!(
                legacy_candidate.delta().encode().unwrap(),
                bulk_candidate.delta().encode().unwrap()
            );
            bulk.validate_prepared_candidate(
                bulk_candidate.prepared_candidate().unwrap(),
                bulk_candidate.delta(),
                bulk_candidate.root(),
            )
            .unwrap();
            historical_roots.push(bulk_candidate.root().clone());
            legacy.commit(legacy_candidate);
            bulk.commit(bulk_candidate);
        }
        bulk.finish_construction().unwrap();
        assert_eq!(legacy.root(), bulk.root());
        for root in &historical_roots {
            bulk_store.validate_catalog_root(root).unwrap();
        }

        std::fs::remove_dir_all(legacy_path).unwrap();
        std::fs::remove_dir_all(bulk_path).unwrap();
    }

    #[test]
    #[ignore = "manual construction reference-catalog attribution receipt"]
    fn construction_reverse_dense_scale_receipt() {
        let total_pages = std::env::var("TINE_REFERENCE_CATALOG_SCALE_PAGES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(128);
        let pages_per_part = std::env::var("TINE_REFERENCE_CATALOG_SCALE_PART_PAGES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(64);
        assert!(total_pages > 0 && pages_per_part > 0);
        let names = PageNameOwnershipRootV1::empty();
        let uuid_root = ContentDigest::of(b"construction-reverse-dense-scale-receipt");
        let (path, durable) = store("construction-reverse-dense-scale");
        let mut state =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        state
            .attach_construction_store(Arc::clone(&durable))
            .unwrap();
        let started = Instant::now();
        let mut checkpoints = Vec::new();
        for part_start in (0..total_pages).step_by(pages_per_part) {
            let part_end = (part_start + pages_per_part).min(total_pages);
            let sources = dense_sources(part_start..part_end, 11, 32);
            let candidate = state.prepare(sources, &names, uuid_root).unwrap();
            state
                .validate_prepared_candidate(
                    candidate.prepared_candidate().unwrap(),
                    candidate.delta(),
                    candidate.root(),
                )
                .unwrap();
            checkpoints.push(candidate.root().clone());
            state.commit(candidate);
        }
        let prepare_elapsed = started.elapsed();
        let finish_started = Instant::now();
        state.finish_construction().unwrap();
        let finish_elapsed = finish_started.elapsed();
        for root in &checkpoints {
            durable.validate_catalog_root(root).unwrap();
        }
        let work = state.construction_work_stats();
        let nodes_path = path.join("reference-catalog-v2").join("nodes");
        let mut loose_node_files = 0_usize;
        let mut pack_files = 0_usize;
        let mut catalog_files = 0_usize;
        let mut head_files = 0_usize;
        let mut current_packed_bytes = 0_u64;
        for entry in std::fs::read_dir(&nodes_path).unwrap() {
            let entry = entry.unwrap();
            if !entry.file_type().unwrap().is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".patricia-node") {
                loose_node_files = loose_node_files.saturating_add(1);
            } else if name.ends_with(".patricia-pack-v1") {
                pack_files = pack_files.saturating_add(1);
                current_packed_bytes =
                    current_packed_bytes.saturating_add(entry.metadata().unwrap().len());
            } else if name.ends_with(".patricia-catalog-v1") {
                catalog_files = catalog_files.saturating_add(1);
                current_packed_bytes =
                    current_packed_bytes.saturating_add(entry.metadata().unwrap().len());
            } else if name == "patricia-pack-head-v1" {
                head_files = head_files.saturating_add(1);
                current_packed_bytes =
                    current_packed_bytes.saturating_add(entry.metadata().unwrap().len());
            }
        }
        let patricia_filesystem_entries = loose_node_files
            .saturating_add(pack_files)
            .saturating_add(catalog_files)
            .saturating_add(head_files);
        eprintln!(
            "construction_reverse_dense_scale pages={total_pages} pages_per_part={pages_per_part} parts={} prepare_ms={:.3} finish_ms={:.3} extraction_ms={:.3} posting_transition_publication_ms={:.3} facts_coverage_patricia_ms={:.3} reverse_patricia_ms={:.3} facts_coverage_reads={} reverse_reads={} prepared_sources={} fact_updates={} reverse_updates={} persistent_reads={} persistent_writes={} logical_node_writes={} loose_node_files={loose_node_files} pack_files={pack_files} catalog_files={catalog_files} head_files={head_files} immutable_publication_calls={} head_transitions={} durability_barriers={} packed_bytes={} current_packed_bytes={current_packed_bytes} capacity_fallbacks={} combined_peak_resident_bytes={} buffer_flushes={}",
            checkpoints.len(),
            prepare_elapsed.as_secs_f64() * 1_000.0,
            finish_elapsed.as_secs_f64() * 1_000.0,
            work.extraction_nanos as f64 / 1_000_000.0,
            work.posting_transition_publication_nanos as f64 / 1_000_000.0,
            work.facts_coverage_patricia_nanos as f64 / 1_000_000.0,
            work.reverse_patricia_nanos as f64 / 1_000_000.0,
            work.facts_coverage_patricia_reads,
            work.reverse_patricia_reads,
            work.prepared_sources,
            work.fact_updates,
            work.reverse_updates,
            work.persistent_node_reads,
            work.persistent_node_writes,
            work.logical_node_writes,
            work.immutable_publication_calls,
            work.head_transitions,
            work.durability_barriers,
            work.packed_bytes,
            work.capacity_fallbacks,
            work.peak_resident_bytes,
            work.buffer_flushes,
        );
        assert!(
            patricia_filesystem_entries.saturating_mul(10) <= work.logical_node_writes,
            "packed construction must reduce Patricia filesystem entries by at least 90%: physical={patricia_filesystem_entries}, logical={}",
            work.logical_node_writes,
        );
        assert!(
            work.peak_resident_bytes <= MAX_PATRICIA_CONSTRUCTION_RESIDENT_BYTES,
            "complete conservative construction peak must remain within 64 MiB: peak={} ceiling={MAX_PATRICIA_CONSTRUCTION_RESIDENT_BYTES}",
            work.peak_resident_bytes,
        );
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn store_backed_roots_are_order_independent_delete_exactly_and_keep_hot_state_empty() {
        let (path, catalog) = store("order");
        let names = PageNameOwnershipRootV1::empty();
        let uuid_root = ContentDigest::of(b"uuid");
        let mut forward =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        forward.attach_store(Arc::clone(&catalog)).unwrap();
        let sources = (1..=128)
            .map(|value| {
                (
                    page(value),
                    Some(source(page(value), &format!("[[Target {value}]]"))),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let candidate = forward.prepare(sources.clone(), &names, uuid_root).unwrap();
        forward.commit(candidate);
        assert_eq!(forward.hot_entry_count(), 0);
        assert_eq!(forward.root().source_count(), 128);

        let (other_path, other_catalog) = store("reverse");
        let mut reverse =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        reverse.attach_store(other_catalog).unwrap();
        let reversed = sources.into_iter().rev().collect();
        let candidate = reverse.prepare(reversed, &names, uuid_root).unwrap();
        reverse.commit(candidate);
        assert_eq!(forward.root(), reverse.root());

        let before_stats = forward.store_stats();
        let candidate = forward
            .prepare(BTreeMap::from([(page(64), None)]), &names, uuid_root)
            .unwrap();
        let after_stats = forward.store_stats();
        assert!(after_stats.reads.saturating_sub(before_stats.reads) < 1_000);
        assert!(after_stats.writes.saturating_sub(before_stats.writes) < 300);
        let delta = candidate.delta().clone();
        catalog.validate_delta(&delta).unwrap();
        forward.commit(candidate);
        assert_eq!(forward.root().source_count(), 127);
        assert_eq!(forward.hot_entry_count(), 0);
        assert!(forward.posting(page(64)).unwrap().is_none());
        let _ = std::fs::remove_dir_all(path);
        let _ = std::fs::remove_dir_all(other_path);
    }

    #[test]
    fn large_posting_uses_compact_delta_without_a_fact_count_product_limit() {
        let (path, catalog) = store("compact");
        let names = PageNameOwnershipRootV1::empty();
        let uuid_root = ContentDigest::of(b"uuid");
        let mut state =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        state.attach_store(catalog).unwrap();
        let raw = (0..12_000)
            .map(|index| format!("[[Target {index}]]"))
            .collect::<Vec<_>>()
            .join(" ");
        let candidate = state
            .prepare(
                BTreeMap::from([(page(1), Some(source(page(1), &raw)))]),
                &names,
                uuid_root,
            )
            .unwrap();
        let delta_bytes = candidate.delta().encode().unwrap();
        let posting =
            extract_source_posting(&ReferenceCatalogPolicyV1::default(), source(page(1), &raw))
                .unwrap();
        let posting_bytes = encode_canonical(&posting).unwrap();
        assert!(posting.facts().len() > 10_000);
        assert!(delta_bytes.len() * 100 < posting_bytes.len());
        let mut maximum_source = "[[Boundary Target]]".to_owned();
        maximum_source.push_str(
            &"x".repeat(super::super::semantic::MAX_BLOCK_CONTENT_BYTES - maximum_source.len()),
        );
        assert_eq!(
            maximum_source.len(),
            super::super::semantic::MAX_BLOCK_CONTENT_BYTES
        );
        state
            .prepare(
                BTreeMap::from([(page(2), Some(source(page(2), &maximum_source)))]),
                &names,
                uuid_root,
            )
            .unwrap();
        let mut maximum_preamble = "[[Preamble Boundary]]".to_owned();
        maximum_preamble.push_str(
            &"x".repeat(super::super::semantic::MAX_PAGE_PREAMBLE_BYTES - maximum_preamble.len()),
        );
        state
            .prepare(
                BTreeMap::from([(
                    page(3),
                    Some(ReferenceSourcePageV1 {
                        page_id: page(3),
                        is_org: false,
                        preamble: Some(maximum_preamble),
                        blocks: Vec::new(),
                    }),
                )]),
                &names,
                uuid_root,
            )
            .unwrap();
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn recovery_gate_blocks_queries_and_mutations_then_validates_store() {
        let (path, catalog) = store("recovery");
        let names = PageNameOwnershipRootV1::empty();
        let uuid_root = ContentDigest::of(b"uuid");
        let mut state =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        state.attach_store(Arc::clone(&catalog)).unwrap();
        let candidate = state
            .prepare(
                BTreeMap::from([(page(1), Some(source(page(1), "[[Target]]")))]),
                &names,
                uuid_root,
            )
            .unwrap();
        state.commit(candidate);
        let root = state.root().clone();
        let mut reopened = ReferenceCatalogStateV2::restore_recovery_required(
            ReferenceCatalogPolicyV1::default(),
            root,
            &names,
            uuid_root,
            catalog,
        )
        .unwrap();
        assert!(matches!(
            reopened.posting(page(1)),
            Err(ReferenceCatalogError::RecoveryRequired)
        ));
        assert!(matches!(
            reopened.prepare(BTreeMap::new(), &names, uuid_root),
            Err(ReferenceCatalogError::RecoveryRequired)
        ));
        assert_eq!(reopened.hot_entry_count(), 0);
        reopened.finish_recovery().unwrap();
        assert_eq!(reopened.hot_entry_count(), 0);
        assert_eq!(reopened.posting(page(1)).unwrap().unwrap().facts().len(), 1);
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn authenticated_reverse_candidates_enforce_exact_limit_boundary() {
        let (path, catalog) = store("reverse-candidate-limit");
        let names = PageNameOwnershipRootV1::empty();
        let uuid_root = ContentDigest::of(b"uuid");
        let mut state =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        state.attach_store(catalog).unwrap();
        let candidate = state
            .prepare(
                BTreeMap::from([
                    (page(1), Some(source(page(1), "[[Target]]"))),
                    (page(2), Some(source(page(2), "[[Target]]"))),
                ]),
                &names,
                uuid_root,
            )
            .unwrap();
        state.commit(candidate);
        let target = ReferenceCandidateTargetV2::PageName(
            LogicalPageName::parse("Target").unwrap().key_digest(),
        );
        assert_eq!(
            state
                .reverse_candidates_at_root(state.root(), target, 2)
                .unwrap(),
            BTreeSet::from([page(1), page(2)])
        );
        assert!(matches!(
            state.reverse_candidates_at_root(state.root(), target, 1),
            Err(ReferenceCatalogError::TooManyCandidates(2))
        ));
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn recovery_rejects_missing_and_tampered_reachable_nodes() {
        let names = PageNameOwnershipRootV1::empty();
        let uuid_root = ContentDigest::of(b"uuid");

        let (missing_path, missing_catalog) = store("recovery-missing");
        let mut missing_state =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        missing_state
            .attach_store(Arc::clone(&missing_catalog))
            .unwrap();
        let candidate = missing_state
            .prepare(
                BTreeMap::from([(page(1), Some(source(page(1), "[[Target]]")))]),
                &names,
                uuid_root,
            )
            .unwrap();
        missing_state.commit(candidate);
        let mut missing_root = missing_state.root().clone();
        missing_root.facts_root = ContentDigest::of(b"missing reference catalog facts root");
        let mut missing_reopened = ReferenceCatalogStateV2::restore_recovery_required(
            ReferenceCatalogPolicyV1::default(),
            missing_root,
            &names,
            uuid_root,
            missing_catalog,
        )
        .unwrap();
        assert!(matches!(
            missing_reopened.finish_recovery(),
            Err(ReferenceCatalogError::Store(error)) if error.contains("node") && error.contains("is missing")
        ));
        let _ = std::fs::remove_dir_all(missing_path);

        let (tampered_path, tampered_catalog) = store("recovery-tampered");
        let mut tampered_state =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        tampered_state
            .attach_store(Arc::clone(&tampered_catalog))
            .unwrap();
        let candidate = tampered_state
            .prepare(
                BTreeMap::from([(page(1), Some(source(page(1), "[[Target]]")))]),
                &names,
                uuid_root,
            )
            .unwrap();
        tampered_state.commit(candidate);
        let tampered_root = tampered_state.root().clone();
        tampered_catalog
            .corrupt_packed_node_for_test(tampered_root.facts_root)
            .unwrap();
        let mut tampered_reopened = ReferenceCatalogStateV2::restore_recovery_required(
            ReferenceCatalogPolicyV1::default(),
            tampered_root,
            &names,
            uuid_root,
            tampered_catalog,
        )
        .unwrap();
        assert!(matches!(
            tampered_reopened.finish_recovery(),
            Err(ReferenceCatalogError::Store(error)) if error.contains("bytes do not match path")
        ));
        let _ = std::fs::remove_dir_all(tampered_path);
    }

    #[test]
    fn transition_tamper_matrix_fails_store_verification() {
        let (path, catalog) = store("tamper");
        let names = PageNameOwnershipRootV1::empty();
        let uuid_root = ContentDigest::of(b"uuid");
        let mut state =
            ReferenceCatalogStateV2::empty(ReferenceCatalogPolicyV1::default(), &names, uuid_root)
                .unwrap();
        state.attach_store(Arc::clone(&catalog)).unwrap();
        let candidate = state
            .prepare(
                BTreeMap::from([(page(1), Some(source(page(1), "[[Target]]")))]),
                &names,
                uuid_root,
            )
            .unwrap();
        let mut post_root = candidate.delta().clone();
        post_root.post_root.facts_root = ContentDigest::of(b"tampered");
        assert!(catalog.validate_delta(&post_root).is_err());

        let mut reverse_root = candidate.delta().clone();
        reverse_root.post_root.reverse_candidates_root = ContentDigest::of(b"tampered");
        assert!(catalog.validate_delta(&reverse_root).is_err());

        let mut transition = candidate.delta().clone();
        let ReferenceTransitionBindingV2::Stored(reference) = &mut transition.transition else {
            panic!("store-backed transition");
        };
        reference.digest = ContentDigest::of(b"tampered");
        assert!(catalog.validate_delta(&transition).is_err());
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn affected_sources_cover_nested_path_moves_deletes_and_membership_owners() {
        let old_page = page(10);
        let new_page = page(11);
        let renamed_page = page(12);
        let preamble_page = page(13);
        let home = document(14);
        let moved_block = block(15);
        let before = super::super::PageState::Live {
            name: LogicalPageName::parse("Before").unwrap(),
            path: ManagedPath::parse("journals/nested/Before.org").unwrap(),
            home_document_id: home,
            kind: ManagedTextKind::Journal,
        };
        let after = super::super::PageState::Tombstone {
            name: LogicalPageName::parse("Before").unwrap(),
            home_document_id: home,
            kind: ManagedTextKind::Journal,
        };
        let rename_before = super::super::PageState::Live {
            name: LogicalPageName::parse("Nested Before").unwrap(),
            path: ManagedPath::parse("pages/nested/Nested Before.md").unwrap(),
            home_document_id: home,
            kind: ManagedTextKind::Page,
        };
        let rename_after = super::super::PageState::Live {
            name: LogicalPageName::parse("Nested After").unwrap(),
            path: ManagedPath::parse("pages/another/Nested After.org").unwrap(),
            home_document_id: home,
            kind: ManagedTextKind::Page,
        };
        let block_before = super::super::BlockState {
            block_id: moved_block,
            home_document_id: home,
            owner: BlockOwner::Page(old_page),
            logseq_uuid: None,
            logseq_identity_origin: None,
            content: "same".into(),
        };
        let mut block_after = block_before.clone();
        block_after.owner = BlockOwner::Page(new_page);
        let claim = super::super::MembershipClaim::new(home, None, "a").unwrap();
        let effect = SemanticEffect::new_with_page_preambles(
            vec![
                super::super::PageDelta {
                    page_id: old_page,
                    before: Some(before),
                    after: Some(after),
                },
                super::super::PageDelta {
                    page_id: renamed_page,
                    before: Some(rename_before),
                    after: Some(rename_after),
                },
            ],
            vec![super::super::PagePreambleDelta {
                page_id: preamble_page,
                home_document_id: home,
                before: Some(super::super::PagePreambleState {
                    page_id: preamble_page,
                    home_document_id: home,
                    preamble: None,
                }),
                after: Some(super::super::PagePreambleState {
                    page_id: preamble_page,
                    home_document_id: home,
                    preamble: Some("[[Changed]]".into()),
                }),
            }],
            vec![super::super::BlockDelta {
                block_id: moved_block,
                home_document_id: home,
                before: Some(block_before),
                after: Some(block_after),
            }],
            vec![super::super::MembershipDelta {
                page_id: new_page,
                block_id: moved_block,
                before: None,
                after: Some(claim),
            }],
        )
        .unwrap();
        assert_eq!(
            affected_reference_sources(&effect),
            BTreeSet::from([old_page, new_page, renamed_page, preamble_page])
        );
        assert!(reference_source_is_org(
            &ManagedPath::parse("journals/deep/2026_07_24.org").unwrap()
        ));
        assert!(!reference_source_is_org(
            &ManagedPath::parse("pages/deep/topic.md").unwrap()
        ));
    }
}
