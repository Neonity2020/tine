//! Parser-owned reference extraction for the disposable SQLite projection.
//!
//! This module deliberately contains no reference index, catalog root, object
//! store, replay state, or durability protocol. Markdown/Org plus the accepted
//! page state are the semantic authority; callers derive exact facts from that
//! state and hand them to the shared SQLite projection.

use std::collections::BTreeSet;
use std::fmt;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

use super::{
    BlockId, BlockOwner, ContentDigest, DocumentId, LogicalPageName, LogseqUuid, ManagedPath,
    PageId, PageNameKeyDigest, SemanticEffect,
};

pub const REFERENCE_CATALOG_SCHEMA_VERSION: u32 = 2;
pub const REFERENCE_CATALOG_POLICY_VERSION: u32 = 1;
pub const REFERENCE_CATALOG_EXTRACTOR_VERSION: u32 = 2;
const MAX_REFERENCE_TARGET_BYTES: usize = super::semantic::MAX_LOGICAL_PAGE_NAME_BYTES;
const POSTING_DOMAIN: &[u8] = b"tine/reference-projection/source-posting/v1";

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

/// Immutable parser/extraction configuration shared by accepted-history
/// interpretation and the SQLite projection.
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
            b"tine/reference-projection/policy/v1",
            &encode_canonical(self)?,
        )
    }

    fn property_key_enabled(&self, key: &str) -> bool {
        let key = crate::doc::property_key_norm(key);
        self.property_pages_enabled && self.property_page_exclusions.binary_search(&key).is_err()
    }

    fn validate(&self) -> Result<(), ReferenceCatalogError> {
        require_version(
            "reference projection policy schema",
            self.schema_version,
            REFERENCE_CATALOG_POLICY_VERSION,
        )?;
        require_version(
            "reference projection policy",
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

pub(crate) fn extract_source_posting(
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
    if !raw
        .as_bytes()
        .iter()
        .any(|byte| matches!(byte, b'[' | b'#' | b'(' | b'{' | b':'))
    {
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

pub(crate) fn extractor_digest() -> ContentDigest {
    let mut bytes = b"tine/reference-projection/extractor/v1\0".to_vec();
    bytes.extend_from_slice(&REFERENCE_CATALOG_EXTRACTOR_VERSION.to_be_bytes());
    bytes.extend_from_slice(crate::reference_evidence::ENGINE_VERSION.as_bytes());
    ContentDigest::of(&bytes)
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
    Encode(String),
    MalformedFact,
    NonCanonical,
    UnknownVersion {
        component: &'static str,
        found: u32,
        expected: u32,
    },
}

impl fmt::Display for ReferenceCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allocation => formatter.write_str("reference extraction allocation failed"),
            Self::Encode(error) => write!(formatter, "reference fact encoding failed: {error}"),
            Self::MalformedFact => formatter.write_str("malformed parser-derived reference fact"),
            Self::NonCanonical => formatter.write_str("non-canonical reference extraction data"),
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
    use super::*;
    use uuid::Uuid;

    fn source(raw: &str) -> ReferenceSourcePageV1 {
        ReferenceSourcePageV1 {
            page_id: PageId::from_uuid(Uuid::from_u128(1)),
            is_org: false,
            preamble: None,
            blocks: vec![ReferenceSourceBlockV1 {
                block_id: BlockId::from_uuid(Uuid::from_u128(2)),
                home_document_id: DocumentId::from_uuid(Uuid::from_u128(3)),
                content: raw.to_owned(),
            }],
        }
    }

    #[test]
    fn parser_projection_extracts_page_and_block_references() {
        let uuid = "00000000-0000-0000-0000-000000000004";
        let posting = extract_source_posting(
            &ReferenceCatalogPolicyV1::default(),
            source(&format!("[[Target]] #tag (({uuid}))")),
        )
        .unwrap();
        assert_eq!(
            posting
                .facts()
                .iter()
                .filter(|fact| matches!(fact, ReferenceFactV1::PageName(_)))
                .count(),
            2
        );
        assert_eq!(
            posting
                .facts()
                .iter()
                .filter(|fact| matches!(fact, ReferenceFactV1::Block(_)))
                .count(),
            1
        );
    }

    #[test]
    fn plain_text_fast_path_skips_parser() {
        reset_reference_evidence_parse_calls();
        let posting =
            extract_source_posting(&ReferenceCatalogPolicyV1::default(), source("plain text"))
                .unwrap();
        assert!(posting.facts().is_empty());
        assert_eq!(reference_evidence_parse_calls(), 0);
    }
}
