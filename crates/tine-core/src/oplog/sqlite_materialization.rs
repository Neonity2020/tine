//! Complete, disposable graph-wide materialization behind the SQLite frontier.
//!
//! The types in this module are an adapter boundary, not a second authority.
//! An accepted semantic effect does not currently contain parser-derived names,
//! references, properties, tags, task facets, formatting facets, or searchable
//! text.  Callers must therefore provide those values explicitly from an
//! authoritative post-acceptance snapshot.  The SQLite applier validates the
//! input against the accepted semantic effect and applies it in the same SQL
//! transaction that advances the accepted frontier.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::config::ParseConfig;
#[cfg(test)]
use rusqlite::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};
use tine_storage::sqlite as storage;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::{
    AcceptedBatchEvent, BatchCausalDot, BatchId, BlockId, BlockOwner, CausalPeerId, ContentDigest,
    DeviceId, DocumentId, LogicalPageName, LogseqIdentityOrigin, LogseqUuid, ManagedPath,
    ManagedTextKind, PageId, PageState, PolicyGeneratedAnchorReason, ReferenceSourceLocatorV1,
    SemanticEffect,
};

pub const MAX_MATERIALIZATION_QUERY_ROWS: usize = storage::MAX_MATERIALIZATION_QUERY_ROWS;
/// Largest accepted materialization string other than a page preamble.
///
/// This retains the established semantic block-content capacity while keeping
/// individual SQLite/FTS values bounded.
pub const MAX_MATERIALIZATION_FIELD_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_MATERIALIZATION_PREAMBLE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_MATERIALIZATION_FACET_VALUES: usize = 16_384;
pub const MAX_MATERIALIZATION_FACET_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_MATERIALIZATION_CHANGE_PAGES: usize = 65_536;
pub const MAX_MATERIALIZATION_CHANGE_BLOCKS: usize = 262_144;
pub const MAX_MATERIALIZATION_CHANGE_FACET_VALUES: usize = 1_048_576;
pub const MAX_MATERIALIZATION_CHANGE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_MATERIALIZATION_QUERY_BYTES: usize = storage::MAX_MATERIALIZATION_QUERY_BYTES;
pub const MAX_MATERIALIZATION_READ_BYTES: usize = storage::MAX_MATERIALIZATION_READ_BYTES;

const MATERIALIZATION_PAGE_OVERHEAD_BYTES: usize = 96;
const MATERIALIZATION_BLOCK_OVERHEAD_BYTES: usize = 128;
const MATERIALIZATION_REFERENCE_OVERHEAD_BYTES: usize = 48;
const MATERIALIZATION_PROPERTY_OVERHEAD_BYTES: usize = 24;
const MATERIALIZATION_TAG_OVERHEAD_BYTES: usize = 16;
const MATERIALIZATION_STRING_OVERHEAD_BYTES: usize = 16;
const REFERENCE_CATALOG_POSTING_OVERHEAD_BYTES: usize = 96;
const REFERENCE_CATALOG_ALIAS_OVERHEAD_BYTES: usize = 80;
// Parser-derived query facts are disposable rows bound only by the accepted
// frontier stamp. They are never a second authenticated authority.
const MATERIALIZATION_INPUT_SCHEMA_VERSION: u32 = 8;

pub(crate) type ApplyChangeInstrumentation = storage::ApplyChangeInstrumentation;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializedEntityId {
    Page(PageId),
    Block(BlockId),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializedReferenceKind {
    Reference,
    Embed,
    TagReference,
    PropertyReference,
}

impl MaterializedReferenceKind {
    const fn sql_value(self) -> i64 {
        match self {
            Self::Reference => 0,
            Self::Embed => 1,
            Self::TagReference => 2,
            Self::PropertyReference => 3,
        }
    }

    fn from_sql(value: i64) -> Result<Self, MaterializationError> {
        match value {
            0 => Ok(Self::Reference),
            1 => Ok(Self::Embed),
            2 => Ok(Self::TagReference),
            3 => Ok(Self::PropertyReference),
            _ => Err(MaterializationError::Corrupt(format!(
                "unknown reference kind {value}"
            ))),
        }
    }
}

/// SQLite's crate-private representation of Packet 1 reference kinds.
///
/// It deliberately does not make Packet 1 depend on SQLite. The authenticated
/// catalog adapter in the following slice is the only boundary that may turn
/// Packet 1 facts into these values.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReferenceCatalogReferenceKind {
    PageLink,
    Tag,
    PageEmbed,
    LinkablePropertyValue,
    AliasDeclaration,
    PropertyKeyPseudoPage,
    BlockReference,
    BlockEmbed,
}

impl ReferenceCatalogReferenceKind {
    pub(crate) const fn from_page_kind(kind: super::PageReferenceKindV1) -> Self {
        match kind {
            super::PageReferenceKindV1::PageLink => Self::PageLink,
            super::PageReferenceKindV1::Tag => Self::Tag,
            super::PageReferenceKindV1::PageEmbed => Self::PageEmbed,
            super::PageReferenceKindV1::LinkablePropertyValue => Self::LinkablePropertyValue,
            super::PageReferenceKindV1::AliasDeclaration => Self::AliasDeclaration,
            super::PageReferenceKindV1::PropertyKeyPseudoPage => Self::PropertyKeyPseudoPage,
        }
    }

    pub(crate) const fn from_block_kind(kind: super::BlockReferenceKindV1) -> Self {
        match kind {
            super::BlockReferenceKindV1::Reference => Self::BlockReference,
            super::BlockReferenceKindV1::Embed => Self::BlockEmbed,
        }
    }

    pub(crate) const fn sql_value(self) -> i64 {
        match self {
            Self::PageLink => 0,
            Self::Tag => 1,
            Self::PageEmbed => 2,
            Self::LinkablePropertyValue => 3,
            Self::AliasDeclaration => 4,
            Self::PropertyKeyPseudoPage => 5,
            Self::BlockReference => 6,
            Self::BlockEmbed => 7,
        }
    }

    const fn accepts_target(self, target: &MaterializedReferenceTarget) -> bool {
        matches!(
            (self, target),
            (
                Self::PageLink
                    | Self::Tag
                    | Self::PageEmbed
                    | Self::LinkablePropertyValue
                    | Self::AliasDeclaration
                    | Self::PropertyKeyPseudoPage,
                MaterializedReferenceTarget::PageName { .. }
            ) | (
                Self::BlockReference | Self::BlockEmbed,
                MaterializedReferenceTarget::ExternalUuid { .. }
            )
        )
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum MaterializedReferenceTarget {
    PageName {
        raw_name: String,
        normalized_name: String,
        resolved_page_id: Option<PageId>,
    },
    ExternalUuid {
        raw_claim: LogseqUuid,
        resolved_block_id: Option<BlockId>,
    },
}

impl MaterializedReferenceTarget {
    fn validate(
        &self,
        input_budget: &mut MaterializationInputBudget,
    ) -> Result<(), MaterializationError> {
        match self {
            Self::PageName {
                raw_name,
                normalized_name,
                ..
            } => {
                validate_page_name_pair("reference target", raw_name, normalized_name)?;
                input_budget.add_field(
                    "reference raw name bytes",
                    raw_name,
                    MAX_MATERIALIZATION_FIELD_BYTES,
                )?;
                input_budget.add_field(
                    "reference normalized name bytes",
                    normalized_name,
                    MAX_MATERIALIZATION_FIELD_BYTES,
                )?;
            }
            Self::ExternalUuid { .. } => input_budget.add_bytes(16)?,
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MaterializedReferencePosting {
    pub(crate) source_page_id: PageId,
    pub(crate) source_entity: MaterializedEntityId,
    pub(crate) source_locator: ReferenceSourceLocatorV1,
    pub(crate) ordinal: u32,
    pub(crate) kind: ReferenceCatalogReferenceKind,
    pub(crate) target: MaterializedReferenceTarget,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MaterializedAliasDeclaration {
    pub(crate) source_page_id: PageId,
    pub(crate) source_entity: MaterializedEntityId,
    pub(crate) source_locator: ReferenceSourceLocatorV1,
    pub(crate) ordinal: u32,
    pub(crate) raw_alias: String,
    pub(crate) normalized_alias: String,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MaterializedPortablePathClaim {
    pub(crate) page_id: PageId,
    pub(crate) portable_path_key: ContentDigest,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MaterializedIdentityRecord {
    pub(crate) key_digest: ContentDigest,
    pub(crate) record: Vec<u8>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedReference {
    pub target: MaterializedEntityId,
    pub kind: MaterializedReferenceKind,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedProperty {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedTask {
    pub marker: String,
    pub priority: Option<String>,
    pub scheduled: Option<String>,
    pub deadline: Option<String>,
}

/// `[#A]` / `SCHEDULED:` / `DEADLINE:`, carried INDEPENDENTLY of
/// [`MaterializedTask`] (SPEC §3.2 M2).
///
/// It is a separate field and not three more `MaterializedTask` fields because
/// `task` is filled only under a marker, while the walk evaluates all three on
/// markerless blocks too -- so folding them into `task` is exactly the shape
/// that made `walk == SQL` unreachable for these attributes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedPlanning {
    pub priority: Option<String>,
    pub scheduled: Option<String>,
    pub deadline: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedBlockInput {
    pub block_id: BlockId,
    pub home_document_id: DocumentId,
    pub parent: Option<BlockId>,
    pub order: String,
    pub content: String,
    pub searchable_text: String,
    /// The block's exact visible text, from which the shared producer derives
    /// both `blocks.query_visible` and `blocks.query_visible_folded`.
    ///
    /// `searchable_text` cannot serve: it is whitespace-collapsed for today's
    /// search consumers, and a content predicate has to be able to tell `a  b`
    /// from `a b` (§5.10).
    pub query_visible: String,
    pub heading_level: Option<u8>,
    pub collapsed: bool,
    pub logseq_uuid: Option<LogseqUuid>,
    pub logseq_identity_origin: Option<LogseqIdentityOrigin>,
    pub references: Vec<MaterializedReference>,
    pub properties: Vec<MaterializedProperty>,
    pub tags: Vec<String>,
    pub task: Option<MaterializedTask>,
    pub planning: Option<MaterializedPlanning>,
    /// The block's OWN normalized page references — `BlockProjection.refs_norm`,
    /// the walk's exact source (§5.8 G1) — from which the shared closure
    /// function derives `block_path_refs`.
    ///
    /// It is a separate field because `references` holds only a resolved entity
    /// id and a kind and cannot carry names, and because the reference postings
    /// encode different things per backend, so no kind filter over them could
    /// be parity-safe (E2).
    pub path_ref_names: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedPageInput {
    pub page_id: PageId,
    pub home_document_id: DocumentId,
    pub name: String,
    pub name_key: String,
    pub path: ManagedPath,
    pub kind: ManagedTextKind,
    pub preamble: Option<String>,
    pub searchable_text: String,
    pub references: Vec<MaterializedReference>,
    pub properties: Vec<MaterializedProperty>,
    pub tags: Vec<String>,
    pub blocks: Vec<MaterializedBlockInput>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializationChange {
    schema_version: u32,
    batch_id: BatchId,
    replacements: Vec<MaterializedPageInput>,
    deletions: Vec<PageId>,
    derived_reference_postings: Vec<MaterializedReferencePosting>,
    derived_aliases: Vec<MaterializedAliasDeclaration>,
    portable_path_claims: Vec<MaterializedPortablePathClaim>,
    page_name_identity_records: Vec<MaterializedIdentityRecord>,
    portable_path_identity_records: Vec<MaterializedIdentityRecord>,
}

/// Per-page validation authority for one accepted event.
///
/// Global causal linearity is too coarse an authority switch: one unrelated
/// concurrently accepted batch must not relax validation for pages it never
/// touched. A page is CONTESTED only when its home document's authored
/// dependency view differs from the receiver's accepted view — i.e. a
/// concurrent batch actually touched that document. Contested pages are not
/// exempted from validation; they are validated against `merged_*`, the
/// engine's own deterministic rendering at this event's accepted root,
/// instead of the authored effect the merge superseded.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct EffectValidationContext {
    pub(crate) contested_pages: BTreeSet<PageId>,
    pub(crate) merged_replacements: BTreeMap<PageId, MaterializedPageInput>,
    pub(crate) merged_deletions: BTreeSet<PageId>,
}

impl EffectValidationContext {
    /// Every page validated byte-exactly against the authored effect: the
    /// correct context for any batch with no concurrent history, and the
    /// fail-closed default everywhere the engine is unavailable (a contested
    /// page without a merged expectation is refused, never waved through).
    pub(crate) fn linear() -> Self {
        Self::default()
    }
}

impl MaterializationChange {
    pub fn new(
        batch_id: BatchId,
        mut replacements: Vec<MaterializedPageInput>,
        mut deletions: Vec<PageId>,
    ) -> Result<Self, MaterializationError> {
        let mut input_budget = MaterializationInputBudget::default();
        input_budget.add_pages(replacements.len())?;
        input_budget.add_pages(deletions.len())?;
        for page in &mut replacements {
            canonicalize_page_blocks(page);
        }
        replacements.sort_unstable_by_key(|page| page.page_id);
        deletions.sort_unstable();
        let change = Self {
            schema_version: MATERIALIZATION_INPUT_SCHEMA_VERSION,
            batch_id,
            replacements,
            deletions,
            derived_reference_postings: Vec::new(),
            derived_aliases: Vec::new(),
            portable_path_claims: Vec::new(),
            page_name_identity_records: Vec::new(),
            portable_path_identity_records: Vec::new(),
        };
        change.validate_shape()?;
        Ok(change)
    }

    pub const fn batch_id(&self) -> BatchId {
        self.batch_id
    }

    pub fn replacements(&self) -> &[MaterializedPageInput] {
        &self.replacements
    }

    pub fn deletions(&self) -> &[PageId] {
        &self.deletions
    }

    pub(crate) fn with_derived_graph_facts(
        mut self,
        mut reference_postings: Vec<MaterializedReferencePosting>,
        mut aliases: Vec<MaterializedAliasDeclaration>,
        mut portable_path_claims: Vec<MaterializedPortablePathClaim>,
    ) -> Result<Self, MaterializationError> {
        reference_postings.sort_unstable();
        aliases.sort_unstable();
        portable_path_claims.sort_unstable();
        self.derived_reference_postings = reference_postings;
        self.derived_aliases = aliases;
        self.portable_path_claims = portable_path_claims;
        self.validate_shape()?;
        Ok(self)
    }

    pub(crate) fn with_identity_projection_records(
        mut self,
        mut page_names: Vec<MaterializedIdentityRecord>,
        mut portable_paths: Vec<MaterializedIdentityRecord>,
    ) -> Result<Self, MaterializationError> {
        page_names.sort_unstable();
        portable_paths.sort_unstable();
        self.page_name_identity_records = page_names;
        self.portable_path_identity_records = portable_paths;
        self.validate_shape()?;
        Ok(self)
    }

    pub fn digest(&self) -> Result<ContentDigest, MaterializationError> {
        self.validate_shape()?;
        let encoded = postcard::to_allocvec(self)
            .map_err(|error| MaterializationError::InvalidInput(error.to_string()))?;
        if encoded.len() > MAX_MATERIALIZATION_CHANGE_BYTES {
            return Err(resource_limit(
                "materialization change bytes",
                encoded.len(),
                MAX_MATERIALIZATION_CHANGE_BYTES,
            ));
        }
        Ok(ContentDigest::of(&encoded))
    }

    /// How strictly a materialization input may be compared against an
    /// accepted event's effective semantic effect.
    ///
    /// `LinearExact` is the ordinary case: the batch's causal past is the
    /// entire accepted prefix, so its effective effect IS the merged
    /// post-state and every per-delta value must match the replacement
    /// exactly. `ConcurrentSuperseded` covers a batch accepted concurrently
    /// with other history (GH #351): the merge decides the post-state, the
    /// authored per-delta values are stale by construction, and only
    /// structural completeness — every affected page supplied exactly once —
    /// remains checkable.
    pub(crate) fn validate_for_event(
        &self,
        event: &AcceptedBatchEvent,
    ) -> Result<ContentDigest, MaterializationError> {
        if self.batch_id != event.batch_id() {
            return Err(MaterializationError::BatchMismatch {
                expected: event.batch_id(),
                found: self.batch_id,
            });
        }
        let effect = SemanticEffect::decode(event.semantic_effect())
            .map_err(|error| MaterializationError::InvalidInput(error.to_string()))?;
        self.validate_against_effect(&effect, event.effect_validation_context())?;
        self.digest()
    }

    #[cfg(test)]
    pub(crate) fn validate_against_stored(
        &self,
        batch_id: BatchId,
        semantic_effect: &[u8],
    ) -> Result<ContentDigest, MaterializationError> {
        self.validate_against_stored_with_context(
            batch_id,
            semantic_effect,
            &EffectValidationContext::linear(),
        )
    }

    #[cfg(test)]
    pub(crate) fn validate_against_stored_with_context(
        &self,
        batch_id: BatchId,
        semantic_effect: &[u8],
        context: &EffectValidationContext,
    ) -> Result<ContentDigest, MaterializationError> {
        if self.batch_id != batch_id {
            return Err(MaterializationError::BatchMismatch {
                expected: batch_id,
                found: self.batch_id,
            });
        }
        let effect = SemanticEffect::decode(semantic_effect)
            .map_err(|error| MaterializationError::InvalidInput(error.to_string()))?;
        self.validate_against_effect(&effect, context)?;
        self.digest()
    }

    fn validate_shape(&self) -> Result<(), MaterializationError> {
        if self.schema_version != MATERIALIZATION_INPUT_SCHEMA_VERSION {
            return Err(MaterializationError::InvalidInput(format!(
                "unknown materialization input schema {}",
                self.schema_version
            )));
        }
        if !strictly_sorted_unique_by(&self.replacements, |page| page.page_id)
            || !strictly_sorted_unique_by(&self.deletions, |page_id| *page_id)
        {
            return Err(MaterializationError::InvalidInput(
                "page replacements/deletions are not canonical".into(),
            ));
        }
        let mut input_budget = MaterializationInputBudget::default();
        input_budget.add_pages(self.replacements.len())?;
        input_budget.add_pages(self.deletions.len())?;
        input_budget.add_bytes(self.deletions.len().checked_mul(32).ok_or_else(|| {
            resource_limit(
                "materialization change bytes",
                usize::MAX,
                MAX_MATERIALIZATION_CHANGE_BYTES,
            )
        })?)?;
        let replacement_ids = self
            .replacements
            .iter()
            .map(|page| page.page_id)
            .collect::<BTreeSet<_>>();
        if self
            .deletions
            .iter()
            .any(|page_id| replacement_ids.contains(page_id))
        {
            return Err(MaterializationError::InvalidInput(
                "one page is both replaced and deleted".into(),
            ));
        }
        let mut block_ids = BTreeSet::new();
        for page in &self.replacements {
            validate_page(page, &mut input_budget)?;
            for block in &page.blocks {
                if !block_ids.insert(block.block_id) {
                    return Err(MaterializationError::InvalidInput(format!(
                        "block {} occurs in multiple replacement pages",
                        block.block_id
                    )));
                }
            }
        }
        if !strictly_sorted_unique_by(&self.derived_reference_postings, |posting| {
            (
                posting.source_page_id,
                posting.source_entity.clone(),
                posting.source_locator,
                posting.ordinal,
            )
        }) || !strictly_sorted_unique_by(&self.derived_aliases, |alias| {
            (
                alias.source_page_id,
                alias.source_entity.clone(),
                alias.source_locator,
                alias.ordinal,
            )
        }) || !strictly_sorted_unique_by(&self.portable_path_claims, |claim| claim.page_id)
            || !strictly_sorted_unique_by(&self.page_name_identity_records, |record| {
                record.key_digest
            })
            || !strictly_sorted_unique_by(&self.portable_path_identity_records, |record| {
                record.key_digest
            })
        {
            return Err(MaterializationError::InvalidInput(
                "derived graph facts are not canonical".into(),
            ));
        }
        let replacement_ids = self
            .replacements
            .iter()
            .map(|page| page.page_id)
            .collect::<BTreeSet<_>>();
        if self
            .derived_reference_postings
            .iter()
            .any(|posting| !replacement_ids.contains(&posting.source_page_id))
            || self
                .derived_aliases
                .iter()
                .any(|alias| !replacement_ids.contains(&alias.source_page_id))
            || (!self.portable_path_claims.is_empty()
                && self
                    .portable_path_claims
                    .iter()
                    .map(|claim| claim.page_id)
                    .collect::<BTreeSet<_>>()
                    != replacement_ids)
        {
            return Err(MaterializationError::InvalidInput(
                "derived graph facts do not exactly belong to replacement pages".into(),
            ));
        }
        for posting in &self.derived_reference_postings {
            validate_reference_posting(posting, &mut input_budget)?;
        }
        for alias in &self.derived_aliases {
            validate_alias_declaration(alias, &mut input_budget)?;
        }
        input_budget.add_facet_values(self.portable_path_claims.len())?;
        input_budget.add_bytes(self.portable_path_claims.len().checked_mul(48).ok_or_else(
            || {
                resource_limit(
                    "materialization change bytes",
                    usize::MAX,
                    MAX_MATERIALIZATION_CHANGE_BYTES,
                )
            },
        )?)?;
        for record in self
            .page_name_identity_records
            .iter()
            .chain(&self.portable_path_identity_records)
        {
            if record.record.is_empty() || record.record.len() > MAX_MATERIALIZATION_FIELD_BYTES {
                return Err(resource_limit(
                    "causal identity record bytes",
                    record.record.len(),
                    MAX_MATERIALIZATION_FIELD_BYTES,
                ));
            }
            input_budget.add_bytes(record.record.len().saturating_add(48))?;
        }
        Ok(())
    }

    fn validate_against_effect(
        &self,
        effect: &SemanticEffect,
        context: &EffectValidationContext,
    ) -> Result<(), MaterializationError> {
        self.validate_shape()?;
        let exact = |page_id: PageId| !context.contested_pages.contains(&page_id);
        let replacements = self
            .replacements
            .iter()
            .map(|page| (page.page_id, page))
            .collect::<BTreeMap<_, _>>();
        // `validate_shape` above rejects duplicate IDs, so this canonical
        // per-page index preserves the prior membership/block lookup semantics.
        let replacement_blocks = self
            .replacements
            .iter()
            .map(|page| {
                (
                    page.page_id,
                    page.blocks
                        .iter()
                        .map(|block| (block.block_id, block))
                        .collect::<BTreeMap<_, _>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let deletions = self.deletions.iter().copied().collect::<BTreeSet<_>>();
        let mut affected = BTreeSet::new();
        let mut required_deletions = BTreeSet::new();

        for delta in effect.pages() {
            affected.insert(delta.page_id);
            match delta.after.as_ref() {
                Some(PageState::Live {
                    name,
                    path,
                    home_document_id,
                    kind,
                }) => {
                    if !exact(delta.page_id) {
                        // A concurrent sibling deleted or reshaped this page;
                        // the merge is the authority, and the contested pass
                        // below holds the supplied replacement to the merged
                        // rendering instead.
                        continue;
                    }
                    let page = replacements.get(&delta.page_id).ok_or_else(|| {
                        MaterializationError::Incomplete(format!(
                            "live page {} has no complete replacement",
                            delta.page_id
                        ))
                    })?;
                    let expected_name_key = name.canonical_key();
                    if page.name.as_str() != name.as_str()
                        || page.name_key.as_str() != expected_name_key.as_str()
                        || &page.path != path
                        || page.home_document_id != *home_document_id
                        || page.kind != *kind
                    {
                        return Err(MaterializationError::Contradiction(format!(
                            "page {} replacement differs from accepted name/key/path/kind/home",
                            delta.page_id
                        )));
                    }
                }
                Some(PageState::Tombstone { .. }) => {
                    required_deletions.insert(delta.page_id);
                }
                None => {
                    return Err(MaterializationError::Incomplete(format!(
                        "accepted page {} has no post-state",
                        delta.page_id
                    )));
                }
            }
        }
        for delta in effect.page_preambles() {
            affected.insert(delta.page_id);
            if !exact(delta.page_id) {
                continue;
            }
            let page = replacements.get(&delta.page_id).ok_or_else(|| {
                MaterializationError::Incomplete(format!(
                    "preamble change for page {} has no replacement",
                    delta.page_id
                ))
            })?;
            let after = delta.after.as_ref().ok_or_else(|| {
                MaterializationError::Incomplete(format!(
                    "preamble change for page {} has no post-state",
                    delta.page_id
                ))
            })?;
            if page.home_document_id != after.home_document_id || page.preamble != after.preamble {
                return Err(MaterializationError::Contradiction(format!(
                    "page {} replacement differs from accepted preamble",
                    delta.page_id
                )));
            }
        }
        for delta in effect.memberships() {
            affected.insert(delta.page_id);
            if !exact(delta.page_id) || required_deletions.contains(&delta.page_id) {
                continue;
            }
            let blocks = replacement_blocks.get(&delta.page_id).ok_or_else(|| {
                MaterializationError::Incomplete(format!(
                    "membership change for page {} has no replacement",
                    delta.page_id
                ))
            })?;
            match delta.after.as_ref() {
                Some(after) => {
                    let block = blocks.get(&delta.block_id).ok_or_else(|| {
                        MaterializationError::Contradiction(format!(
                            "accepted member {} is absent from page {}",
                            delta.block_id, delta.page_id
                        ))
                    })?;
                    if block.home_document_id != after.home_document_id
                        || block.parent != after.parent
                        || block.order != after.order
                    {
                        return Err(MaterializationError::Contradiction(format!(
                            "member {} differs from accepted parent/order/home",
                            delta.block_id
                        )));
                    }
                }
                None if blocks.contains_key(&delta.block_id) => {
                    return Err(MaterializationError::Contradiction(format!(
                        "removed member {} remains on page {}",
                        delta.block_id, delta.page_id
                    )));
                }
                None => {}
            }
        }
        for delta in effect.blocks() {
            let owner = delta
                .after
                .as_ref()
                .and_then(block_owner_page)
                .or_else(|| delta.before.as_ref().and_then(block_owner_page));
            let Some(page_id) = owner else {
                continue;
            };
            affected.insert(page_id);
            if !exact(page_id) || required_deletions.contains(&page_id) {
                continue;
            }
            let blocks = replacement_blocks.get(&page_id).ok_or_else(|| {
                MaterializationError::Incomplete(format!(
                    "block change for page {page_id} has no replacement"
                ))
            })?;
            match delta.after.as_ref() {
                Some(after) if matches!(after.owner, BlockOwner::Page(owner) if owner == page_id) =>
                {
                    let block = blocks.get(&delta.block_id).ok_or_else(|| {
                        MaterializationError::Contradiction(format!(
                            "accepted live block {} is absent from page {page_id}",
                            delta.block_id
                        ))
                    })?;
                    if block.home_document_id != after.home_document_id
                        || block.content != after.content
                        || block.logseq_uuid != after.logseq_uuid
                        || block.logseq_identity_origin != after.logseq_identity_origin
                    {
                        return Err(MaterializationError::Contradiction(format!(
                            "block {} replacement differs from accepted state",
                            delta.block_id
                        )));
                    }
                }
                Some(_) | None if blocks.contains_key(&delta.block_id) => {
                    return Err(MaterializationError::Contradiction(format!(
                        "non-live block {} remains on page {page_id}",
                        delta.block_id
                    )));
                }
                Some(_) | None => {}
            }
        }

        // Contested pages are validated against the engine's merged rendering
        // at this event's accepted root — recompute-and-compare, never skip.
        // A contested page with no merged expectation means the event was
        // built without engine authority; refusing is the only safe answer.
        for page_id in &context.contested_pages {
            if !affected.contains(page_id) {
                return Err(MaterializationError::Contradiction(format!(
                    "contested page {page_id} is not an affected page of the effect"
                )));
            }
            if let Some(expected) = context.merged_replacements.get(page_id) {
                let supplied_page = replacements.get(page_id).ok_or_else(|| {
                    MaterializationError::Incomplete(format!(
                        "contested live page {page_id} has no complete replacement"
                    ))
                })?;
                if **supplied_page != *expected {
                    return Err(MaterializationError::Contradiction(format!(
                        "page {page_id} replacement differs from the merged accepted rendering"
                    )));
                }
            } else if context.merged_deletions.contains(page_id) {
                if replacements.contains_key(page_id) {
                    return Err(MaterializationError::Contradiction(format!(
                        "merge-deleted page {page_id} is supplied as a replacement"
                    )));
                }
            } else {
                return Err(MaterializationError::Incomplete(format!(
                    "contested page {page_id} has no merged expectation"
                )));
            }
        }

        let supplied = replacements
            .keys()
            .copied()
            .chain(deletions.iter().copied())
            .collect::<BTreeSet<_>>();
        if supplied != affected {
            return Err(MaterializationError::Incomplete(format!(
                "supplied pages {supplied:?} differ from accepted affected pages {affected:?}"
            )));
        }
        // A contested page's authored deletion polarity is superseded by the
        // merge: the expected deletion set keeps the authored tombstones of
        // every uncontested page and takes the merged answer for the rest.
        let expected_deletions = required_deletions
            .iter()
            .copied()
            .filter(|page_id| exact(*page_id))
            .chain(context.merged_deletions.iter().copied())
            .collect::<BTreeSet<_>>();
        if deletions != expected_deletions {
            return Err(MaterializationError::Contradiction(format!(
                "supplied deletions {deletions:?} differ from accepted deletions {expected_deletions:?}"
            )));
        }
        Ok(())
    }
}

pub(crate) fn canonicalize_page_blocks(page: &mut MaterializedPageInput) {
    page.blocks.sort_unstable_by(|left, right| {
        (&left.order, left.block_id).cmp(&(&right.order, right.block_id))
    });
}

#[derive(Default)]
struct MaterializationInputBudget {
    bytes: usize,
    pages: usize,
    blocks: usize,
    facet_values: usize,
}

impl MaterializationInputBudget {
    fn add_bytes(&mut self, bytes: usize) -> Result<(), MaterializationError> {
        self.bytes = checked_budget_add(
            "materialization change bytes",
            self.bytes,
            bytes,
            MAX_MATERIALIZATION_CHANGE_BYTES,
        )?;
        Ok(())
    }

    fn add_pages(&mut self, pages: usize) -> Result<(), MaterializationError> {
        self.pages = checked_budget_add(
            "materialization change pages",
            self.pages,
            pages,
            MAX_MATERIALIZATION_CHANGE_PAGES,
        )?;
        Ok(())
    }

    fn add_blocks(&mut self, blocks: usize) -> Result<(), MaterializationError> {
        self.blocks = checked_budget_add(
            "materialization change blocks",
            self.blocks,
            blocks,
            MAX_MATERIALIZATION_CHANGE_BLOCKS,
        )?;
        Ok(())
    }

    fn add_facet_values(&mut self, values: usize) -> Result<(), MaterializationError> {
        self.facet_values = checked_budget_add(
            "materialization change facet values",
            self.facet_values,
            values,
            MAX_MATERIALIZATION_CHANGE_FACET_VALUES,
        )?;
        Ok(())
    }

    fn add_field(
        &mut self,
        resource: &'static str,
        value: &str,
        maximum: usize,
    ) -> Result<(), MaterializationError> {
        if value.len() > maximum {
            return Err(resource_limit(resource, value.len(), maximum));
        }
        self.add_bytes(value.len())?;
        self.add_bytes(MATERIALIZATION_STRING_OVERHEAD_BYTES)
    }
}

fn checked_budget_add(
    resource: &'static str,
    current: usize,
    added: usize,
    maximum: usize,
) -> Result<usize, MaterializationError> {
    let found = current
        .checked_add(added)
        .ok_or_else(|| resource_limit(resource, usize::MAX, maximum))?;
    if found > maximum {
        return Err(resource_limit(resource, found, maximum));
    }
    Ok(found)
}

fn resource_limit(resource: &'static str, found: usize, maximum: usize) -> MaterializationError {
    MaterializationError::ResourceLimit {
        resource,
        found,
        maximum,
    }
}

fn canonical_reference_source_locator_bytes(
    locator: ReferenceSourceLocatorV1,
) -> Result<Vec<u8>, MaterializationError> {
    let bytes = postcard::to_allocvec(&locator)
        .map_err(|error| MaterializationError::InvalidInput(error.to_string()))?;
    validate_reference_source_locator_bytes(&bytes)?;
    Ok(bytes)
}

fn validate_reference_source_locator_bytes(bytes: &[u8]) -> Result<(), MaterializationError> {
    if bytes.is_empty() || bytes.len() > MAX_MATERIALIZATION_FIELD_BYTES {
        return Err(MaterializationError::InvalidInput(
            "reference source locator bytes are out of bounds".into(),
        ));
    }
    let locator: ReferenceSourceLocatorV1 = postcard::from_bytes(bytes).map_err(|_| {
        MaterializationError::InvalidInput("reference source locator bytes are malformed".into())
    })?;
    let canonical = postcard::to_allocvec(&locator)
        .map_err(|error| MaterializationError::InvalidInput(error.to_string()))?;
    if canonical != bytes {
        return Err(MaterializationError::InvalidInput(
            "reference source locator bytes are not canonical".into(),
        ));
    }
    Ok(())
}

fn validate_page_name_pair(
    description: &str,
    raw_name: &str,
    normalized_name: &str,
) -> Result<(), MaterializationError> {
    if raw_name.is_empty() || normalized_name.is_empty() {
        return Err(MaterializationError::InvalidInput(format!(
            "{description} has an empty raw/normalized name"
        )));
    }
    if raw_name.len() > MAX_MATERIALIZATION_FIELD_BYTES
        || normalized_name.len() > MAX_MATERIALIZATION_FIELD_BYTES
    {
        return Err(MaterializationError::InvalidInput(format!(
            "{description} name exceeds the materialization field limit"
        )));
    }
    if crate::refs::page_key(raw_name) != normalized_name {
        return Err(MaterializationError::InvalidInput(format!(
            "{description} normalized name does not match refs::page_key"
        )));
    }
    Ok(())
}

fn validate_reference_posting(
    posting: &MaterializedReferencePosting,
    input_budget: &mut MaterializationInputBudget,
) -> Result<(), MaterializationError> {
    if !posting.kind.accepts_target(&posting.target) {
        return Err(MaterializationError::InvalidInput(
            "reference kind and target type are incompatible".into(),
        ));
    }
    let locator = canonical_reference_source_locator_bytes(posting.source_locator)?;
    input_budget.add_facet_values(1)?;
    input_budget.add_bytes(REFERENCE_CATALOG_POSTING_OVERHEAD_BYTES)?;
    input_budget.add_bytes(locator.len())?;
    posting.target.validate(input_budget)
}

fn validate_alias_declaration(
    alias: &MaterializedAliasDeclaration,
    input_budget: &mut MaterializationInputBudget,
) -> Result<(), MaterializationError> {
    let locator = canonical_reference_source_locator_bytes(alias.source_locator)?;
    validate_page_name_pair(
        "reference alias declaration",
        &alias.raw_alias,
        &alias.normalized_alias,
    )?;
    input_budget.add_facet_values(1)?;
    input_budget.add_bytes(REFERENCE_CATALOG_ALIAS_OVERHEAD_BYTES)?;
    input_budget.add_bytes(locator.len())?;
    input_budget.add_field(
        "reference alias raw bytes",
        &alias.raw_alias,
        MAX_MATERIALIZATION_FIELD_BYTES,
    )?;
    input_budget.add_field(
        "reference alias normalized bytes",
        &alias.normalized_alias,
        MAX_MATERIALIZATION_FIELD_BYTES,
    )
}

pub(crate) fn block_owner_page(state: &super::BlockState) -> Option<PageId> {
    match state.owner {
        BlockOwner::Page(page_id) => Some(page_id),
        BlockOwner::Tombstone => None,
    }
}

fn validate_page(
    page: &MaterializedPageInput,
    input_budget: &mut MaterializationInputBudget,
) -> Result<(), MaterializationError> {
    input_budget.add_bytes(MATERIALIZATION_PAGE_OVERHEAD_BYTES)?;
    input_budget.add_field(
        "page name bytes",
        &page.name,
        MAX_MATERIALIZATION_FIELD_BYTES,
    )?;
    input_budget.add_field(
        "page name key bytes",
        &page.name_key,
        MAX_MATERIALIZATION_FIELD_BYTES,
    )?;
    input_budget.add_field(
        "page path bytes",
        page.path.as_str(),
        MAX_MATERIALIZATION_FIELD_BYTES,
    )?;
    if let Some(preamble) = &page.preamble {
        input_budget.add_field(
            "page preamble bytes",
            preamble,
            MAX_MATERIALIZATION_PREAMBLE_BYTES,
        )?;
    }
    input_budget.add_field(
        "page searchable text bytes",
        &page.searchable_text,
        MAX_MATERIALIZATION_FIELD_BYTES,
    )?;
    if page.name.is_empty() || page.name_key.is_empty() {
        return Err(MaterializationError::InvalidInput(format!(
            "page {} has an empty name/name key",
            page.page_id
        )));
    }
    validate_references(&page.references, input_budget)?;
    validate_properties(&page.properties, input_budget)?;
    validate_tags(&page.tags, input_budget)?;
    input_budget.add_blocks(page.blocks.len())?;
    let block_ids = page
        .blocks
        .iter()
        .map(|block| block.block_id)
        .collect::<BTreeSet<_>>();
    if block_ids.len() != page.blocks.len() {
        return Err(MaterializationError::InvalidInput(format!(
            "page {} contains duplicate block identities",
            page.page_id
        )));
    }
    if !page
        .blocks
        .windows(2)
        .all(|pair| (&pair[0].order, pair[0].block_id) < (&pair[1].order, pair[1].block_id))
    {
        return Err(MaterializationError::InvalidInput(format!(
            "page {} blocks are not in canonical order",
            page.page_id
        )));
    }
    for block in &page.blocks {
        input_budget.add_bytes(MATERIALIZATION_BLOCK_OVERHEAD_BYTES)?;
        input_budget.add_field(
            "block order bytes",
            &block.order,
            MAX_MATERIALIZATION_FIELD_BYTES,
        )?;
        input_budget.add_field(
            "block content bytes",
            &block.content,
            MAX_MATERIALIZATION_FIELD_BYTES,
        )?;
        input_budget.add_field(
            "block searchable text bytes",
            &block.searchable_text,
            MAX_MATERIALIZATION_FIELD_BYTES,
        )?;
        input_budget.add_field(
            "block query visible bytes",
            &block.query_visible,
            MAX_MATERIALIZATION_FIELD_BYTES,
        )?;
        for name in &block.path_ref_names {
            input_budget.add_field(
                "path ref names bytes",
                name,
                MAX_MATERIALIZATION_FIELD_BYTES,
            )?;
        }
        if block.order.is_empty() {
            return Err(MaterializationError::InvalidInput(format!(
                "block {} has an empty order key",
                block.block_id
            )));
        }
        if block
            .heading_level
            .is_some_and(|level| !(1..=6).contains(&level))
        {
            return Err(MaterializationError::InvalidInput(format!(
                "block {} has an invalid heading level",
                block.block_id
            )));
        }
        if block.logseq_uuid.is_some() != block.logseq_identity_origin.is_some() {
            return Err(MaterializationError::InvalidInput(format!(
                "block {} has incomplete Logseq identity metadata",
                block.block_id
            )));
        }
        if block
            .parent
            .is_some_and(|parent| !block_ids.contains(&parent))
        {
            return Err(MaterializationError::InvalidInput(format!(
                "block {} has a parent outside page {}",
                block.block_id, page.page_id
            )));
        }
        validate_references(&block.references, input_budget)?;
        validate_properties(&block.properties, input_budget)?;
        validate_tags(&block.tags, input_budget)?;
        if let Some(task) = &block.task {
            input_budget.add_field(
                "task marker bytes",
                &task.marker,
                MAX_MATERIALIZATION_FIELD_BYTES,
            )?;
            for (resource, value) in [
                ("task priority bytes", task.priority.as_deref()),
                ("task scheduled bytes", task.scheduled.as_deref()),
                ("task deadline bytes", task.deadline.as_deref()),
            ] {
                if let Some(value) = value {
                    input_budget.add_field(resource, value, MAX_MATERIALIZATION_FIELD_BYTES)?;
                }
            }
            if task.marker.is_empty() {
                return Err(MaterializationError::InvalidInput(format!(
                    "block {} has an empty task marker",
                    block.block_id
                )));
            }
        }
        if let Some(planning) = &block.planning {
            for value in [
                planning.priority.as_deref(),
                planning.scheduled.as_deref(),
                planning.deadline.as_deref(),
            ] {
                if let Some(value) = value {
                    input_budget.add_field(
                        "planning priority/scheduled/deadline bytes",
                        value,
                        MAX_MATERIALIZATION_FIELD_BYTES,
                    )?;
                }
            }
            if planning.priority.is_none()
                && planning.scheduled.is_none()
                && planning.deadline.is_none()
            {
                return Err(MaterializationError::InvalidInput(format!(
                    "block {} carries an empty planning facet",
                    block.block_id
                )));
            }
        }
    }
    Ok(())
}

fn validate_references(
    references: &[MaterializedReference],
    input_budget: &mut MaterializationInputBudget,
) -> Result<(), MaterializationError> {
    let bytes = references
        .len()
        .checked_mul(MATERIALIZATION_REFERENCE_OVERHEAD_BYTES)
        .ok_or_else(|| {
            resource_limit(
                "reference facet bytes",
                usize::MAX,
                MAX_MATERIALIZATION_FACET_BYTES,
            )
        })?;
    validate_facet("reference", references.len(), bytes)?;
    input_budget.add_facet_values(references.len())?;
    input_budget.add_bytes(bytes)
}

fn validate_properties(
    properties: &[MaterializedProperty],
    input_budget: &mut MaterializationInputBudget,
) -> Result<(), MaterializationError> {
    let bytes = properties.iter().try_fold(0_usize, |total, property| {
        total
            .checked_add(property.name.len())
            .and_then(|total| total.checked_add(property.value.len()))
            .and_then(|total| total.checked_add(MATERIALIZATION_PROPERTY_OVERHEAD_BYTES))
    });
    let bytes = bytes.ok_or_else(|| {
        resource_limit(
            "property facet bytes",
            usize::MAX,
            MAX_MATERIALIZATION_FACET_BYTES,
        )
    })?;
    validate_facet("property", properties.len(), bytes)?;
    if properties.iter().any(|property| property.name.is_empty()) {
        return Err(MaterializationError::InvalidInput(
            "property names must be non-empty".into(),
        ));
    }
    input_budget.add_facet_values(properties.len())?;
    for property in properties {
        input_budget.add_bytes(MATERIALIZATION_PROPERTY_OVERHEAD_BYTES)?;
        input_budget.add_field(
            "property name bytes",
            &property.name,
            MAX_MATERIALIZATION_FIELD_BYTES,
        )?;
        input_budget.add_field(
            "property value bytes",
            &property.value,
            MAX_MATERIALIZATION_FIELD_BYTES,
        )?;
    }
    Ok(())
}

fn validate_tags(
    tags: &[String],
    input_budget: &mut MaterializationInputBudget,
) -> Result<(), MaterializationError> {
    let bytes = tags.iter().try_fold(0_usize, |total, tag| {
        total
            .checked_add(tag.len())
            .and_then(|total| total.checked_add(MATERIALIZATION_TAG_OVERHEAD_BYTES))
    });
    let bytes = bytes.ok_or_else(|| {
        resource_limit(
            "tag facet bytes",
            usize::MAX,
            MAX_MATERIALIZATION_FACET_BYTES,
        )
    })?;
    validate_facet("tag", tags.len(), bytes)?;
    if tags.iter().any(String::is_empty) {
        return Err(MaterializationError::InvalidInput(
            "tags must be non-empty".into(),
        ));
    }
    input_budget.add_facet_values(tags.len())?;
    for tag in tags {
        input_budget.add_bytes(MATERIALIZATION_TAG_OVERHEAD_BYTES)?;
        input_budget.add_field("tag bytes", tag, MAX_MATERIALIZATION_FIELD_BYTES)?;
    }
    Ok(())
}

fn validate_facet(
    facet: &'static str,
    values: usize,
    bytes: usize,
) -> Result<(), MaterializationError> {
    if values > MAX_MATERIALIZATION_FACET_VALUES {
        return Err(resource_limit(
            match facet {
                "reference" => "reference facet values",
                "property" => "property facet values",
                "tag" => "tag facet values",
                _ => "materialization facet values",
            },
            values,
            MAX_MATERIALIZATION_FACET_VALUES,
        ));
    }
    if bytes > MAX_MATERIALIZATION_FACET_BYTES {
        return Err(resource_limit(
            match facet {
                "reference" => "reference facet bytes",
                "property" => "property facet bytes",
                "tag" => "tag facet bytes",
                _ => "materialization facet bytes",
            },
            bytes,
            MAX_MATERIALIZATION_FACET_BYTES,
        ));
    }
    Ok(())
}

fn strictly_sorted_unique_by<T, K: Ord>(values: &[T], key: impl Fn(&T) -> K) -> bool {
    values.windows(2).all(|pair| key(&pair[0]) < key(&pair[1]))
}

#[cfg(test)]
pub(crate) fn initialize_schema(
    connection: &Connection,
    empty_frontier_digest: ContentDigest,
) -> Result<(), MaterializationError> {
    // Unit fixtures at this level exercise lowering, not the config-change
    // rebuild route, so they stamp the default parse config. The routes that
    // must notice a config change compare the stamp themselves (§5.8 H5).
    storage::initialize_materialization_schema_for_test(
        connection,
        empty_frontier_digest,
        ParseConfig::default().digest(),
    )
    .map_err(Into::into)
}

#[cfg(test)]
pub(crate) fn apply_change(
    transaction: &Transaction<'_>,
    change: &MaterializationChange,
    semantic_effect: &[u8],
    sequence: u64,
    input_digest: ContentDigest,
    post_frontier_digest: ContentDigest,
) -> Result<ApplyChangeInstrumentation, MaterializationError> {
    let physical = lower_validated_change(
        change,
        semantic_effect,
        None,
        &EffectValidationContext::linear(),
        &ParseConfig::default(),
    )?;
    storage::apply_materialization_change_for_test(
        transaction,
        &physical,
        sequence,
        input_digest,
        post_frontier_digest,
    )
    .map_err(Into::into)
}

pub(crate) fn lower_validated_change(
    change: &MaterializationChange,
    semantic_effect: &[u8],
    causal_dot: Option<BatchCausalDot>,
    context: &EffectValidationContext,
    parse_config: &ParseConfig,
) -> Result<storage::PhysicalMaterializationChange, MaterializationError> {
    change.validate_shape()?;
    let effect = SemanticEffect::decode(semantic_effect)
        .map_err(|error| MaterializationError::InvalidInput(error.to_string()))?;
    change.validate_against_effect(&effect, context)?;

    let pages_with_live_metadata_delta = effect
        .pages()
        .iter()
        .filter(|delta| matches!(delta.after.as_ref(), Some(PageState::Live { .. })))
        .map(|delta| delta.page_id.as_uuid().into_bytes())
        .collect();
    let block_home_claims = effect
        .blocks()
        .iter()
        .filter(|delta| delta.before.is_none() && delta.after.is_some())
        .map(|delta| storage::PhysicalBlockHomeClaim {
            block_id: delta.block_id.as_uuid().into_bytes(),
            home_document_id: delta.home_document_id.as_uuid().into_bytes(),
            batch_id: Some(change.batch_id.as_uuid().into_bytes()),
            causal_peer_id: causal_dot
                .map(|dot| dot.peer_id().as_device_id().as_uuid().into_bytes()),
            causal_counter: causal_dot.map(BatchCausalDot::counter),
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let logseq_uuid_introductions = effect
        .blocks()
        .iter()
        .filter_map(|delta| {
            let before = delta.before.as_ref().and_then(|state| state.logseq_uuid);
            let after = delta.after.as_ref()?.logseq_uuid?;
            (before != Some(after)).then_some(storage::PhysicalLogseqUuidIntroduction {
                logseq_uuid: after.as_uuid().into_bytes(),
                block_id: delta.block_id.as_uuid().into_bytes(),
                home_document_id: delta.home_document_id.as_uuid().into_bytes(),
                batch_id: Some(change.batch_id.as_uuid().into_bytes()),
                causal_peer_id: causal_dot
                    .map(|dot| dot.peer_id().as_device_id().as_uuid().into_bytes()),
                causal_counter: causal_dot.map(BatchCausalDot::counter),
            })
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let replacements = lower_pages_with_derived_rows(&change.replacements, parse_config)?;
    Ok(storage::PhysicalMaterializationChange {
        batch_id: change.batch_id.as_uuid().into_bytes(),
        replacements,
        deletions: change
            .deletions
            .iter()
            .map(|page_id| page_id.as_uuid().into_bytes())
            .collect(),
        pages_with_live_metadata_delta,
        derived_reference_postings: lower_reference_postings(&change.derived_reference_postings)?,
        derived_aliases: lower_alias_declarations(&change.derived_aliases)?,
        portable_path_claims: change
            .portable_path_claims
            .iter()
            .map(|claim| storage::PhysicalPagePortablePathClaim {
                page_id: claim.page_id.as_uuid().into_bytes(),
                portable_path_key: claim.portable_path_key,
            })
            .collect(),
        block_home_claims,
        page_name_identity_records: change
            .page_name_identity_records
            .iter()
            .map(|record| storage::PhysicalIdentityRecord {
                key_digest: record.key_digest,
                record: record.record.clone(),
            })
            .collect(),
        portable_path_identity_records: change
            .portable_path_identity_records
            .iter()
            .map(|record| storage::PhysicalIdentityRecord {
                key_digest: record.key_digest,
                record: record.record.clone(),
            })
            .collect(),
        logseq_uuid_introductions,
    })
}

/// Lower every replacement page AND its derived rows through the ONE tine-core
/// computation (SPEC §5.8 J2).
///
/// Managed Storage has two lowering entry points — per-event
/// [`lower_validated_change`] and the terminal builder's
/// [`lower_terminal_chunk`] — and they must not each grow their own copy of the
/// closure and the atomizer, so both come through here.
pub(crate) fn lower_pages_with_derived_rows(
    pages: &[MaterializedPageInput],
    parse_config: &ParseConfig,
) -> Result<Vec<storage::PhysicalPage>, MaterializationError> {
    // Built once for the whole batch: `JournalFormat::new` compiles five
    // patterns, which is per-graph work rather than per-page work.
    let journal_days = crate::query::derived::JournalDays::new(parse_config);
    pages
        .iter()
        .map(|page| lower_page(page, parse_config, &journal_days))
        .collect()
}

fn lower_page(
    page: &MaterializedPageInput,
    parse_config: &ParseConfig,
    journal_days: &crate::query::derived::JournalDays,
) -> Result<storage::PhysicalPage, MaterializationError> {
    let normalized_searchable_text = normalized_searchable_text(&page.searchable_text)?;
    // The page's format comes from its own path, case-insensitively
    // (`Format::from_path`), never from `reference_source_is_org`, whose
    // `ends_with(".org")` would type an `Outline.ORG` page Markdown here while
    // Direct Files types it Org (§5.8 E4).
    let format = crate::query::atom::AtomFormat::from(crate::model::Format::from_path(
        std::path::Path::new(page.path.as_str()),
    ));
    let page_property_atoms = crate::query::derived::property_atom_rows(
        &page
            .properties
            .iter()
            .map(|property| (property.name.clone(), property.value.clone()))
            .collect::<Vec<_>>(),
        format,
        parse_config,
    );
    let flat = page
        .blocks
        .iter()
        .map(|block| crate::query::path_refs::PathRefBlock {
            id: block.block_id,
            parent: block.parent,
            refs: block.path_ref_names.as_slice(),
        })
        .collect::<Vec<_>>();
    let mut path_refs = crate::query::derived::path_ref_rows(&page.name, &flat);
    Ok(storage::PhysicalPage {
        page_id: page.page_id.as_uuid().into_bytes(),
        home_document_id: page.home_document_id.as_uuid().into_bytes(),
        name: page.name.clone(),
        name_key: page.name_key.clone(),
        path: page.path.as_str().to_owned(),
        text_kind: text_kind_to_sql(page.kind),
        journal_day: journal_days.day(page.path.as_str(), page.kind == ManagedTextKind::Journal),
        preamble: page.preamble.clone(),
        searchable_text: page.searchable_text.clone(),
        normalized_searchable_text,
        references: page.references.iter().map(lower_reference).collect(),
        properties: page.properties.iter().map(lower_property).collect(),
        tags: crate::query::derived::tag_rows(&page.tags),
        property_atoms: page_property_atoms,
        blocks: page
            .blocks
            .iter()
            .map(|block| {
                lower_block(
                    block,
                    path_refs.remove(&block.block_id).unwrap_or_default(),
                    format,
                    parse_config,
                )
            })
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn lower_block(
    block: &MaterializedBlockInput,
    path_refs: Vec<String>,
    format: crate::query::atom::AtomFormat,
    parse_config: &ParseConfig,
) -> Result<storage::PhysicalBlock, MaterializationError> {
    let normalized_searchable_text = normalized_searchable_text(&block.searchable_text)?;
    // Managed Storage carries only the visible text across its input capture,
    // so the fold is derived here -- by the same `canonical_fold` Direct Files
    // already applied when it filled `BlockProjection::visible_lower`.
    let (query_visible, query_visible_folded) =
        crate::query::derived::query_visible_columns(&block.query_visible, None);
    let property_atoms = crate::query::derived::property_atom_rows(
        &block
            .properties
            .iter()
            .map(|property| (property.name.clone(), property.value.clone()))
            .collect::<Vec<_>>(),
        format,
        parse_config,
    );
    Ok(storage::PhysicalBlock {
        block_id: block.block_id.as_uuid().into_bytes(),
        home_document_id: block.home_document_id.as_uuid().into_bytes(),
        parent: block.parent.map(|id| id.as_uuid().into_bytes()),
        order: block.order.clone(),
        content: block.content.clone(),
        searchable_text: block.searchable_text.clone(),
        normalized_searchable_text,
        query_visible,
        query_visible_folded,
        heading_level: block.heading_level,
        collapsed: block.collapsed,
        logseq_uuid: block.logseq_uuid.map(|id| id.as_uuid().into_bytes()),
        logseq_identity_origin: block.logseq_identity_origin.map(identity_origin_to_sql),
        references: block.references.iter().map(lower_reference).collect(),
        properties: block.properties.iter().map(lower_property).collect(),
        tags: crate::query::derived::tag_rows(&block.tags),
        task: block.task.as_ref().map(|task| storage::PhysicalTask {
            marker: task.marker.clone(),
            priority: task.priority.clone(),
            scheduled: task.scheduled.clone(),
            deadline: task.deadline.clone(),
        }),
        planning: block.planning.as_ref().and_then(|planning| {
            crate::query::derived::planning_row(
                planning.priority.as_deref(),
                planning.scheduled.as_deref(),
                planning.deadline.as_deref(),
            )
        }),
        path_refs,
        property_atoms,
    })
}

fn normalized_searchable_text(value: &str) -> Result<String, MaterializationError> {
    let normalized = value.to_lowercase().nfc().collect::<String>();
    if normalized.len() > MAX_MATERIALIZATION_FIELD_BYTES {
        return Err(MaterializationError::ResourceLimit {
            resource: "normalized searchable text bytes",
            found: normalized.len(),
            maximum: MAX_MATERIALIZATION_FIELD_BYTES,
        });
    }
    Ok(normalized)
}

fn lower_reference(reference: &MaterializedReference) -> storage::PhysicalReference {
    storage::PhysicalReference {
        target: lower_entity(reference.target),
        kind: reference.kind.sql_value(),
    }
}

fn lower_property(property: &MaterializedProperty) -> storage::PhysicalProperty {
    storage::PhysicalProperty {
        name: property.name.clone(),
        normalized_name: crate::doc::property_key_norm(&property.name),
        value: property.value.clone(),
    }
}

fn lower_entity(entity: MaterializedEntityId) -> storage::PhysicalEntityId {
    match entity {
        MaterializedEntityId::Page(id) => {
            storage::PhysicalEntityId::Page(id.as_uuid().into_bytes())
        }
        MaterializedEntityId::Block(id) => {
            storage::PhysicalEntityId::Block(id.as_uuid().into_bytes())
        }
    }
}

fn lower_reference_postings(
    postings: &[MaterializedReferencePosting],
) -> Result<Vec<storage::PhysicalReferencePosting>, MaterializationError> {
    postings
        .iter()
        .map(|posting| {
            Ok(storage::PhysicalReferencePosting {
                source_page_id: posting.source_page_id.as_uuid().into_bytes(),
                source_entity: lower_entity(posting.source_entity),
                source_locator: canonical_reference_source_locator_bytes(posting.source_locator)?,
                ordinal: posting.ordinal,
                kind: posting.kind.sql_value(),
                target: match &posting.target {
                    MaterializedReferenceTarget::PageName {
                        raw_name,
                        normalized_name,
                        resolved_page_id,
                    } => storage::PhysicalReferenceTarget::PageName {
                        raw_name: raw_name.clone(),
                        normalized_name: normalized_name.clone(),
                        resolved_page_id: resolved_page_id.map(|id| id.as_uuid().into_bytes()),
                    },
                    MaterializedReferenceTarget::ExternalUuid {
                        raw_claim,
                        resolved_block_id,
                    } => storage::PhysicalReferenceTarget::ExternalUuid {
                        raw_claim: raw_claim.as_uuid().into_bytes(),
                        resolved_block_id: resolved_block_id.map(|id| id.as_uuid().into_bytes()),
                    },
                },
            })
        })
        .collect()
}

fn lower_alias_declarations(
    aliases: &[MaterializedAliasDeclaration],
) -> Result<Vec<storage::PhysicalAliasDeclaration>, MaterializationError> {
    aliases
        .iter()
        .map(|alias| {
            Ok(storage::PhysicalAliasDeclaration {
                source_page_id: alias.source_page_id.as_uuid().into_bytes(),
                source_entity: lower_entity(alias.source_entity),
                source_locator: canonical_reference_source_locator_bytes(alias.source_locator)?,
                ordinal: alias.ordinal,
                raw_alias: alias.raw_alias.clone(),
                normalized_alias: alias.normalized_alias.clone(),
            })
        })
        .collect()
}

/// One bounded chunk of terminal bootstrap rows, before lowering.
///
/// Terminal construction never replays an intermediate page or reference
/// replacement. Parser-derived reference rows are disposable projection facts
/// covered by the final accepted-frontier stamp, not catalog coverage rows.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TerminalMaterializationChunk {
    pub(crate) pages: Vec<MaterializedPageInput>,
    pub(crate) postings: Vec<MaterializedReferencePosting>,
    pub(crate) aliases: Vec<MaterializedAliasDeclaration>,
}

/// Validate and lower one bounded terminal chunk with the same field rules the
/// per-event lowering applies. The caller separately proves the chunk's pages
/// are exactly the authenticated terminal current-path catalog rows, so there
/// is no per-event semantic effect to validate against here.
pub(crate) fn lower_terminal_chunk(
    mut chunk: TerminalMaterializationChunk,
    parse_config: &ParseConfig,
) -> Result<storage::PhysicalTerminalMaterializationChunk, MaterializationError> {
    for page in &mut chunk.pages {
        canonicalize_page_blocks(page);
    }
    chunk.pages.sort_unstable_by_key(|page| page.page_id);
    chunk.postings.sort_unstable();
    chunk.aliases.sort_unstable();
    let mut input_budget = MaterializationInputBudget::default();
    input_budget.add_pages(chunk.pages.len())?;
    if !strictly_sorted_unique_by(&chunk.pages, |page| page.page_id) {
        return Err(MaterializationError::InvalidInput(
            "terminal pages are not canonical".into(),
        ));
    }
    let mut block_ids = BTreeSet::new();
    for page in &chunk.pages {
        validate_page(page, &mut input_budget)?;
        for block in &page.blocks {
            if !block_ids.insert(block.block_id) {
                return Err(MaterializationError::InvalidInput(format!(
                    "block {} occurs in multiple terminal pages",
                    block.block_id
                )));
            }
        }
    }
    if !strictly_sorted_unique_by(&chunk.postings, |posting| {
        (
            posting.source_page_id,
            posting.source_entity,
            posting.source_locator,
            posting.ordinal,
        )
    }) {
        return Err(MaterializationError::InvalidInput(
            "terminal reference postings are not canonical".into(),
        ));
    }
    if !strictly_sorted_unique_by(&chunk.aliases, |alias| {
        (
            alias.source_page_id,
            alias.source_entity,
            alias.source_locator,
            alias.ordinal,
        )
    }) {
        return Err(MaterializationError::InvalidInput(
            "terminal reference alias declarations are not canonical".into(),
        ));
    }
    let page_ids = chunk
        .pages
        .iter()
        .map(|page| page.page_id)
        .collect::<BTreeSet<_>>();
    for posting in &chunk.postings {
        if !page_ids.contains(&posting.source_page_id) {
            return Err(MaterializationError::InvalidInput(
                "terminal reference posting has no source page".into(),
            ));
        }
        validate_reference_posting(posting, &mut input_budget)?;
    }
    for alias in &chunk.aliases {
        if !page_ids.contains(&alias.source_page_id) {
            return Err(MaterializationError::InvalidInput(
                "terminal reference alias has no source page".into(),
            ));
        }
        validate_alias_declaration(alias, &mut input_budget)?;
    }
    let mut page_name_identity_records = chunk
        .pages
        .iter()
        .map(|page| {
            let name = LogicalPageName::parse(&page.name)
                .map_err(|error| MaterializationError::InvalidInput(error.to_string()))?;
            let record =
                super::sqlite_identity::PageNameIdentityRecordV1::baseline(page.page_id, name)
                    .map_err(MaterializationError::InvalidInput)?;
            Ok(storage::PhysicalIdentityRecord {
                key_digest: ContentDigest::from_bytes(*record.key_digest().as_bytes()),
                record: record
                    .encode()
                    .map_err(MaterializationError::InvalidInput)?,
            })
        })
        .collect::<Result<Vec<_>, MaterializationError>>()?;
    page_name_identity_records.sort_unstable_by_key(|record| record.key_digest);
    if !strictly_sorted_unique_by(&page_name_identity_records, |record| record.key_digest) {
        return Err(MaterializationError::InvalidInput(
            "terminal baseline repeats a canonical page-name key".into(),
        ));
    }
    let mut portable_path_identity_records = chunk
        .pages
        .iter()
        .map(|page| {
            let record = super::sqlite_identity::PortablePathIdentityRecordV1::baseline(
                page.page_id,
                page.path.clone(),
            )
            .map_err(MaterializationError::InvalidInput)?;
            Ok(storage::PhysicalIdentityRecord {
                key_digest: ContentDigest::from_bytes(*record.key_digest().as_bytes()),
                record: record
                    .encode()
                    .map_err(MaterializationError::InvalidInput)?,
            })
        })
        .collect::<Result<Vec<_>, MaterializationError>>()?;
    portable_path_identity_records.sort_unstable_by_key(|record| record.key_digest);
    if !strictly_sorted_unique_by(&portable_path_identity_records, |record| record.key_digest) {
        return Err(MaterializationError::InvalidInput(
            "terminal baseline repeats a portable path key".into(),
        ));
    }
    Ok(storage::PhysicalTerminalMaterializationChunk {
        pages: lower_pages_with_derived_rows(&chunk.pages, parse_config)?,
        postings: lower_reference_postings(&chunk.postings)?,
        aliases: lower_alias_declarations(&chunk.aliases)?,
        block_home_claims: chunk
            .pages
            .iter()
            .flat_map(|page| page.blocks.iter())
            .map(|block| storage::PhysicalBlockHomeClaim {
                block_id: block.block_id.as_uuid().into_bytes(),
                home_document_id: block.home_document_id.as_uuid().into_bytes(),
                batch_id: None,
                causal_peer_id: None,
                causal_counter: None,
            })
            .collect(),
        page_name_identity_records,
        portable_path_identity_records,
        logseq_uuid_introductions: chunk
            .pages
            .iter()
            .flat_map(|page| page.blocks.iter())
            .filter_map(|block| {
                block
                    .logseq_uuid
                    .map(|logseq_uuid| storage::PhysicalLogseqUuidIntroduction {
                        logseq_uuid: logseq_uuid.as_uuid().into_bytes(),
                        block_id: block.block_id.as_uuid().into_bytes(),
                        home_document_id: block.home_document_id.as_uuid().into_bytes(),
                        batch_id: None,
                        causal_peer_id: None,
                        causal_counter: None,
                    })
            })
            .collect(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedPageRow {
    pub page_id: PageId,
    pub home_document_id: DocumentId,
    pub name: String,
    pub name_key: String,
    pub path: ManagedPath,
    pub kind: ManagedTextKind,
    pub preamble: Option<String>,
    pub searchable_text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedPageInventoryRow {
    pub page_id: PageId,
    pub name: String,
    pub path: ManagedPath,
    pub kind: ManagedTextKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedNavigationPageRow {
    pub page_id: PageId,
    pub name: String,
    pub name_key: String,
    pub path: ManagedPath,
    pub kind: ManagedTextKind,
    pub preamble: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedNavigationAliasRow {
    pub source_page_id: PageId,
    pub owner_name: String,
    pub owner_path: ManagedPath,
    pub normalized_alias: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedNavigationReferenceNameRow {
    pub source_page_id: PageId,
    pub owner_path: ManagedPath,
    pub raw_name: String,
    pub normalized_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedBlockRow {
    pub block_id: BlockId,
    pub page_id: PageId,
    pub home_document_id: DocumentId,
    pub parent: Option<BlockId>,
    pub order: String,
    pub content: String,
    pub searchable_text: String,
    pub heading_level: Option<u8>,
    pub collapsed: bool,
    pub logseq_uuid: Option<LogseqUuid>,
    pub logseq_identity_origin: Option<LogseqIdentityOrigin>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializedBlockHomeClaimRow {
    pub block_id: BlockId,
    pub home_document_id: DocumentId,
    /// Absent only for a claim inherent in the immutable activation baseline.
    pub batch_id: Option<BatchId>,
    pub causal_dot: Option<BatchCausalDot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedIdentityRecordRow {
    pub key_digest: ContentDigest,
    pub record: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializedLogseqUuidIntroductionRow {
    pub logseq_uuid: LogseqUuid,
    pub block_id: BlockId,
    pub home_document_id: DocumentId,
    /// Absent only for an introduction inherent in the immutable activation
    /// baseline.
    pub batch_id: Option<BatchId>,
    pub causal_dot: Option<BatchCausalDot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedReferrerRow {
    pub source: MaterializedEntityId,
    pub source_page_id: PageId,
    pub kind: MaterializedReferenceKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedBlockReferenceCountRow {
    pub raw_uuid_claim: LogseqUuid,
    pub distinct_source_blocks: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedBlockReferrerCandidateRow {
    pub source_page_id: PageId,
    pub source_block_id: BlockId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedPageReferrerCandidateRow {
    pub source_page_id: PageId,
    pub source: MaterializedEntityId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedPlainTextCandidatePageRow {
    pub page_id: PageId,
}

/// One page that could contain a literal needle as an ordered subsequence.
/// A candidate only; the parser-owned matcher still decides and ranks blocks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedFuzzyCandidatePageRow {
    pub page_id: PageId,
    pub path: ManagedPath,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedBlockPropertyCandidateRow {
    pub page_id: PageId,
    pub block_id: BlockId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedPropertyFacetRow {
    pub owner: MaterializedEntityId,
    pub page_id: PageId,
    pub source_name: String,
    pub normalized_name: String,
    pub value: String,
    pub ordinal: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedTaskCandidatePageRow {
    pub page_id: PageId,
}

/// One physical task-index candidate, converted at Tine's managed-storage
/// boundary. The raw block text deliberately remains parser-owned input; task
/// facets are not accepted from SQLite as query semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedTaskCandidateBlockRow {
    pub block_id: BlockId,
    pub page_id: PageId,
    pub parent: Option<BlockId>,
    pub order: String,
    pub content: String,
    pub logseq_uuid: Option<LogseqUuid>,
    pub page_name: String,
    pub page_path: ManagedPath,
    pub page_kind: ManagedTextKind,
}

/// The deliberately text-free structural record used for a bounded ancestor
/// walk by a caller that already owns the candidate's parser input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedBlockStructureRow {
    pub block_id: BlockId,
    pub page_id: PageId,
    pub parent: Option<BlockId>,
    pub order: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedPropertyRow {
    pub owner: MaterializedEntityId,
    pub page_id: PageId,
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedTagRow {
    pub owner: MaterializedEntityId,
    pub page_id: PageId,
    pub tag: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedTaskRow {
    pub block_id: BlockId,
    pub page_id: PageId,
    pub marker: String,
    pub priority: Option<String>,
    pub scheduled: Option<String>,
    pub deadline: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MaterializedSearchHit {
    pub entity: MaterializedEntityId,
    pub page_id: PageId,
    pub text: String,
    pub rank: f64,
}

pub struct SqliteMaterializedRead<'a> {
    inner: storage::SqliteMaterializedRead<'a>,
}

impl<'a> SqliteMaterializedRead<'a> {
    pub(crate) fn from_storage(inner: storage::SqliteMaterializedRead<'a>) -> Self {
        Self { inner }
    }

    #[cfg(test)]
    pub(crate) fn new(
        connection: &'a Connection,
        acceptance_sequence: u64,
        frontier_digest: ContentDigest,
    ) -> Result<Self, MaterializationError> {
        Ok(Self::from_storage(
            storage::SqliteMaterializedRead::from_connection_for_test(
                connection,
                acceptance_sequence,
                frontier_digest,
            )?,
        ))
    }

    pub const fn acceptance_sequence(&self) -> u64 {
        self.inner.acceptance_sequence()
    }

    pub fn page(
        &self,
        page_id: PageId,
    ) -> Result<Option<MaterializedPageRow>, MaterializationError> {
        self.inner
            .page_with_header_validation(
                page_id.as_uuid().into_bytes(),
                validate_storage_page_header,
            )?
            .map(page_row_from_storage)
            .transpose()
    }

    pub fn block(
        &self,
        block_id: BlockId,
    ) -> Result<Option<MaterializedBlockRow>, MaterializationError> {
        self.inner
            .block(block_id.as_uuid().into_bytes())?
            .map(block_row_from_storage)
            .transpose()
    }

    pub fn block_home_claims(
        &self,
        block_id: BlockId,
        limit: usize,
    ) -> Result<Vec<MaterializedBlockHomeClaimRow>, MaterializationError> {
        convert_rows(
            self.inner
                .block_home_claims(block_id.as_uuid().into_bytes(), limit)?,
            block_home_claim_row_from_storage,
        )
    }

    pub fn page_name_identity_record(
        &self,
        key_digest: ContentDigest,
    ) -> Result<Option<MaterializedIdentityRecordRow>, MaterializationError> {
        self.inner
            .page_name_identity_record(key_digest)?
            .map(identity_record_row_from_storage)
            .transpose()
    }

    pub(crate) fn causal_page_name_identity_record(
        &self,
        key: super::PageNameKeyDigest,
    ) -> Result<Option<super::sqlite_identity::PageNameIdentityRecordV1>, MaterializationError>
    {
        let digest = ContentDigest::from_bytes(*key.as_bytes());
        self.page_name_identity_record(digest)?
            .map(|row| {
                super::sqlite_identity::PageNameIdentityRecordV1::decode(key, &row.record)
                    .map_err(MaterializationError::Corrupt)
            })
            .transpose()
    }

    pub fn portable_path_identity_record(
        &self,
        key_digest: ContentDigest,
    ) -> Result<Option<MaterializedIdentityRecordRow>, MaterializationError> {
        self.inner
            .portable_path_identity_record(key_digest)?
            .map(identity_record_row_from_storage)
            .transpose()
    }

    pub(crate) fn causal_portable_path_identity_record(
        &self,
        key: super::PortablePathKeyDigest,
    ) -> Result<Option<super::sqlite_identity::PortablePathIdentityRecordV1>, MaterializationError>
    {
        let digest = ContentDigest::from_bytes(*key.as_bytes());
        self.portable_path_identity_record(digest)?
            .map(|row| {
                super::sqlite_identity::PortablePathIdentityRecordV1::decode(key, &row.record)
                    .map_err(MaterializationError::Corrupt)
            })
            .transpose()
    }

    pub fn logseq_uuid_introductions(
        &self,
        logseq_uuid: LogseqUuid,
        limit: usize,
    ) -> Result<Vec<MaterializedLogseqUuidIntroductionRow>, MaterializationError> {
        convert_rows(
            self.inner
                .logseq_uuid_introductions(logseq_uuid.as_uuid().into_bytes(), limit)?,
            logseq_uuid_introduction_row_from_storage,
        )
    }

    pub fn blocks_by_logseq_uuid(
        &self,
        logseq_uuid: LogseqUuid,
        limit: usize,
    ) -> Result<Vec<MaterializedBlockRow>, MaterializationError> {
        convert_rows(
            self.inner
                .blocks_by_logseq_uuid(logseq_uuid.as_uuid().into_bytes(), limit)?,
            block_row_from_storage,
        )
    }

    pub fn pages_by_name(
        &self,
        name: &str,
        limit: usize,
    ) -> Result<Vec<MaterializedPageRow>, MaterializationError> {
        convert_rows(
            self.inner.pages_by_name_with_header_validation(
                name,
                limit,
                validate_storage_page_header,
            )?,
            page_row_from_storage,
        )
    }

    pub fn pages_by_name_key(
        &self,
        name_key: &str,
        limit: usize,
    ) -> Result<Vec<MaterializedPageRow>, MaterializationError> {
        convert_rows(
            self.inner.pages_by_name_key_with_header_validation(
                name_key,
                limit,
                validate_storage_page_header,
            )?,
            page_row_from_storage,
        )
    }

    pub fn pages_by_name_key_and_kind(
        &self,
        name_key: &str,
        kind: ManagedTextKind,
        limit: usize,
    ) -> Result<Vec<MaterializedPageRow>, MaterializationError> {
        convert_rows(
            self.inner
                .pages_by_name_key_and_kind_with_header_validation(
                    name_key,
                    text_kind_to_sql(kind),
                    limit,
                    validate_storage_page_header,
                )?,
            page_row_from_storage,
        )
    }

    pub fn pages_by_path(
        &self,
        path: &ManagedPath,
        limit: usize,
    ) -> Result<Vec<MaterializedPageRow>, MaterializationError> {
        convert_rows(
            self.inner.pages_by_path_with_header_validation(
                &path.as_str().to_owned(),
                limit,
                validate_storage_page_header,
            )?,
            page_row_from_storage,
        )
    }

    pub fn pages(
        &self,
        kind: Option<ManagedTextKind>,
        limit: usize,
    ) -> Result<Vec<MaterializedPageRow>, MaterializationError> {
        convert_rows(
            self.inner.pages_with_header_validation(
                kind.map(text_kind_to_sql),
                limit,
                validate_storage_page_header,
            )?,
            page_row_from_storage,
        )
    }

    pub fn page_inventory_after(
        &self,
        after: Option<(&ManagedPath, PageId)>,
        kind: Option<ManagedTextKind>,
        limit: usize,
    ) -> Result<Vec<MaterializedPageInventoryRow>, MaterializationError> {
        let after_page_id = after.map(|(_, page_id)| page_id.as_uuid().into_bytes());
        convert_rows(
            self.inner.page_inventory_after_with_header_validation(
                after.map(|(path, _)| path.as_str()),
                after_page_id.as_ref(),
                kind.map(text_kind_to_sql),
                limit,
                validate_storage_page_header,
            )?,
            page_inventory_row_from_storage,
        )
    }

    pub fn navigation_pages_after(
        &self,
        after: Option<(&ManagedPath, PageId)>,
        limit: usize,
    ) -> Result<Vec<MaterializedNavigationPageRow>, MaterializationError> {
        let after_page_id = after.map(|(_, page_id)| page_id.as_uuid().into_bytes());
        convert_rows(
            self.inner.navigation_pages_after_with_header_validation(
                after.map(|(path, _)| path.as_str()),
                after_page_id.as_ref(),
                limit,
                validate_storage_page_header,
            )?,
            navigation_page_row_from_storage,
        )
    }

    pub fn navigation_pages_by_name_key(
        &self,
        name_key: &str,
        limit: usize,
    ) -> Result<Vec<MaterializedNavigationPageRow>, MaterializationError> {
        convert_rows(
            self.inner
                .navigation_pages_by_name_key_with_header_validation(
                    name_key,
                    limit,
                    validate_storage_page_header,
                )?,
            navigation_page_row_from_storage,
        )
    }

    pub fn navigation_pages_by_name_key_namespace_after(
        &self,
        parent_name_key: &str,
        after: Option<(&str, PageId)>,
        limit: usize,
    ) -> Result<Vec<MaterializedNavigationPageRow>, MaterializationError> {
        let after_page_id = after.map(|(_, page_id)| page_id.as_uuid().into_bytes());
        convert_rows(
            self.inner
                .navigation_pages_by_name_key_namespace_after_with_header_validation(
                    parent_name_key,
                    after.map(|(name_key, _)| name_key),
                    after_page_id.as_ref(),
                    limit,
                    validate_storage_page_header,
                )?,
            navigation_page_row_from_storage,
        )
    }

    pub fn navigation_aliases_after(
        &self,
        after: Option<(&ManagedPath, &str, PageId)>,
        limit: usize,
    ) -> Result<Vec<MaterializedNavigationAliasRow>, MaterializationError> {
        let after_page_id = after.map(|(_, _, page_id)| page_id.as_uuid().into_bytes());
        convert_rows(
            self.inner.navigation_aliases_after(
                after
                    .zip(after_page_id.as_ref())
                    .map(|((path, alias, _), page_id)| (path.as_str(), alias, page_id)),
                limit,
            )?,
            navigation_alias_row_from_storage,
        )
    }

    pub fn navigation_reference_names_after(
        &self,
        after: Option<(&ManagedPath, &str, &str, PageId)>,
        limit: usize,
    ) -> Result<Vec<MaterializedNavigationReferenceNameRow>, MaterializationError> {
        let after_page_id = after.map(|(_, _, _, page_id)| page_id.as_uuid().into_bytes());
        convert_rows(
            self.inner.navigation_reference_names_after(
                after
                    .zip(after_page_id.as_ref())
                    .map(|((path, raw, normalized, _), page_id)| {
                        (path.as_str(), raw, normalized, page_id)
                    }),
                limit,
            )?,
            navigation_reference_name_row_from_storage,
        )
    }

    pub fn blocks_on_page(
        &self,
        page_id: PageId,
        limit: usize,
    ) -> Result<Vec<MaterializedBlockRow>, MaterializationError> {
        convert_rows(
            self.inner
                .blocks_on_page(page_id.as_uuid().into_bytes(), limit)?,
            block_row_from_storage,
        )
    }

    pub fn referrers_to(
        &self,
        target: MaterializedEntityId,
        limit: usize,
    ) -> Result<Vec<MaterializedReferrerRow>, MaterializationError> {
        convert_rows(
            self.inner.referrers_to(lower_entity(target), limit)?,
            referrer_row_from_storage,
        )
    }

    pub fn block_reference_counts_after(
        &self,
        after: Option<LogseqUuid>,
        limit: usize,
    ) -> Result<Vec<MaterializedBlockReferenceCountRow>, MaterializationError> {
        self.inner
            .block_reference_counts_after(after.map(|uuid| uuid.as_uuid().into_bytes()), limit)?
            .into_iter()
            .map(block_reference_count_row_from_storage)
            .collect()
    }

    pub fn block_reference_counts_for_source_page_after(
        &self,
        source_page_id: PageId,
        after: Option<LogseqUuid>,
        limit: usize,
    ) -> Result<Vec<MaterializedBlockReferenceCountRow>, MaterializationError> {
        self.inner
            .block_reference_counts_for_source_page_after(
                source_page_id.as_uuid().into_bytes(),
                after.map(|uuid| uuid.as_uuid().into_bytes()),
                limit,
            )?
            .into_iter()
            .map(block_reference_count_row_from_storage)
            .collect()
    }

    pub fn block_referrer_candidates_after(
        &self,
        raw_uuid_claim: LogseqUuid,
        after: Option<(PageId, BlockId)>,
        limit: usize,
    ) -> Result<Vec<MaterializedBlockReferrerCandidateRow>, MaterializationError> {
        self.inner
            .block_referrer_candidates_after(
                raw_uuid_claim.as_uuid().into_bytes(),
                after.map(|(page, block)| {
                    (page.as_uuid().into_bytes(), block.as_uuid().into_bytes())
                }),
                limit,
            )?
            .into_iter()
            .map(block_referrer_candidate_row_from_storage)
            .collect()
    }

    pub fn page_referrer_candidates_after(
        &self,
        normalized_name: &str,
        after: Option<(PageId, MaterializedEntityId)>,
        limit: usize,
    ) -> Result<Vec<MaterializedPageReferrerCandidateRow>, MaterializationError> {
        self.inner
            .page_referrer_candidates_after(
                normalized_name,
                after.map(|(page, source)| (page.as_uuid().into_bytes(), lower_entity(source))),
                limit,
            )?
            .into_iter()
            .map(page_referrer_candidate_row_from_storage)
            .collect()
    }

    pub fn plain_text_candidate_pages_after(
        &self,
        normalized_phrase: &str,
        after: Option<PageId>,
        limit: usize,
    ) -> Result<Vec<MaterializedPlainTextCandidatePageRow>, MaterializationError> {
        self.inner
            .plain_text_candidate_pages_after(
                normalized_phrase,
                after.map(|page| page.as_uuid().into_bytes()),
                limit,
            )?
            .into_iter()
            .map(plain_text_candidate_page_row_from_storage)
            .collect()
    }

    /// Page-level candidates for the legacy ordered-subsequence matcher --
    /// the managed twin of the read Direct Files narrows through in
    /// `direct_projection::fuzzy_candidate_paths`, over the same
    /// `search_substring_fts` rows and the same
    /// `to_lowercase().nfc()` normalization both writers apply.
    pub fn fuzzy_subsequence_candidate_pages_after(
        &self,
        normalized_needle: &str,
        after: Option<PageId>,
        limit: usize,
    ) -> Result<Vec<MaterializedFuzzyCandidatePageRow>, MaterializationError> {
        self.inner
            .fuzzy_subsequence_candidate_pages_after(
                normalized_needle,
                after.map(|page| page.as_uuid().into_bytes()),
                limit,
            )?
            .into_iter()
            .map(fuzzy_candidate_page_row_from_storage)
            .collect()
    }

    pub fn block_property_candidates_after(
        &self,
        normalized_name: &str,
        after: Option<(PageId, BlockId)>,
        limit: usize,
    ) -> Result<Vec<MaterializedBlockPropertyCandidateRow>, MaterializationError> {
        self.inner
            .block_property_candidates_after(
                normalized_name,
                after.map(|(page, block)| {
                    (page.as_uuid().into_bytes(), block.as_uuid().into_bytes())
                }),
                limit,
            )?
            .into_iter()
            .map(block_property_candidate_row_from_storage)
            .collect()
    }

    pub fn property_facet_rows_after(
        &self,
        block_owners_only: bool,
        after: Option<(MaterializedEntityId, String, u32)>,
        limit: usize,
    ) -> Result<Vec<MaterializedPropertyFacetRow>, MaterializationError> {
        self.inner
            .property_facet_rows_after(
                block_owners_only,
                after.map(|(owner, name, ordinal)| (lower_entity(owner), name, ordinal)),
                limit,
            )?
            .into_iter()
            .map(property_facet_row_from_storage)
            .collect()
    }

    pub fn properties(
        &self,
        owner: MaterializedEntityId,
        limit: usize,
    ) -> Result<Vec<MaterializedPropertyRow>, MaterializationError> {
        convert_rows(
            self.inner.properties(lower_entity(owner), limit)?,
            property_row_from_storage,
        )
    }

    pub fn properties_named(
        &self,
        name: &str,
        value: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MaterializedPropertyRow>, MaterializationError> {
        convert_rows(
            self.inner.properties_named(name, value, limit)?,
            property_row_from_storage,
        )
    }

    pub fn tags(
        &self,
        tag: &str,
        limit: usize,
    ) -> Result<Vec<MaterializedTagRow>, MaterializationError> {
        convert_rows(self.inner.tags(tag, limit)?, tag_row_from_storage)
    }

    pub fn tasks(
        &self,
        marker: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MaterializedTaskRow>, MaterializationError> {
        convert_rows(self.inner.tasks(marker, limit)?, task_row_from_storage)
    }

    pub fn task_candidate_pages_after(
        &self,
        marker: &str,
        after: Option<PageId>,
        limit: usize,
    ) -> Result<Vec<MaterializedTaskCandidatePageRow>, MaterializationError> {
        self.inner
            .task_candidate_pages_after(
                marker,
                after.map(|page| page.as_uuid().into_bytes()),
                limit,
            )?
            .into_iter()
            .map(|row| {
                Ok(MaterializedTaskCandidatePageRow {
                    page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
                })
            })
            .collect()
    }

    pub fn task_candidate_blocks_after(
        &self,
        marker: &str,
        after: Option<(PageId, BlockId)>,
        limit: usize,
    ) -> Result<Vec<MaterializedTaskCandidateBlockRow>, MaterializationError> {
        convert_rows(
            self.inner
                .task_candidate_blocks_after_with_header_validation(
                    marker,
                    after.map(|(page, block)| {
                        (page.as_uuid().into_bytes(), block.as_uuid().into_bytes())
                    }),
                    limit,
                    validate_storage_page_header,
                )?,
            task_candidate_block_row_from_storage,
        )
    }

    pub fn block_structure(
        &self,
        block_id: BlockId,
    ) -> Result<Option<MaterializedBlockStructureRow>, MaterializationError> {
        self.inner
            .block_structure(block_id.as_uuid().into_bytes())?
            .map(block_structure_row_from_storage)
            .transpose()
    }

    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MaterializedSearchHit>, MaterializationError> {
        convert_rows(self.inner.search(query, limit)?, search_hit_from_storage)
    }
}

fn convert_rows<T, U>(
    rows: Vec<T>,
    convert: impl Fn(T) -> Result<U, MaterializationError>,
) -> Result<Vec<U>, MaterializationError> {
    rows.into_iter().map(convert).collect()
}

fn validate_storage_page_header(
    path: &str,
    kind: i64,
) -> Result<(), storage::MaterializationError> {
    ManagedPath::parse(path).map_err(|error| {
        storage::MaterializationError::Corrupt(format!("malformed managed path row: {error}"))
    })?;
    text_kind_from_sql(kind)
        .map(|_| ())
        .map_err(|error| storage::MaterializationError::Corrupt(error.to_string()))
}

fn page_row_from_storage(
    row: storage::PhysicalPageRow,
) -> Result<MaterializedPageRow, MaterializationError> {
    Ok(MaterializedPageRow {
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
        home_document_id: DocumentId::from_uuid(Uuid::from_bytes(row.home_document_id)),
        name: row.name,
        name_key: row.name_key,
        path: ManagedPath::parse(row.path).map_err(typed_sql_decode_error)?,
        kind: text_kind_from_sql(row.text_kind).map_err(typed_sql_decode_error)?,
        preamble: row.preamble,
        searchable_text: row.searchable_text,
    })
}

fn page_inventory_row_from_storage(
    row: storage::PhysicalPageInventoryRow,
) -> Result<MaterializedPageInventoryRow, MaterializationError> {
    Ok(MaterializedPageInventoryRow {
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
        name: row.name,
        path: ManagedPath::parse(row.path).map_err(typed_sql_decode_error)?,
        kind: text_kind_from_sql(row.text_kind).map_err(typed_sql_decode_error)?,
    })
}

fn navigation_page_row_from_storage(
    row: storage::PhysicalNavigationPageRow,
) -> Result<MaterializedNavigationPageRow, MaterializationError> {
    Ok(MaterializedNavigationPageRow {
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
        name: row.name,
        name_key: row.name_key,
        path: ManagedPath::parse(row.path).map_err(typed_sql_decode_error)?,
        kind: text_kind_from_sql(row.text_kind).map_err(typed_sql_decode_error)?,
        preamble: row.preamble,
    })
}

fn navigation_alias_row_from_storage(
    row: storage::PhysicalNavigationAliasRow,
) -> Result<MaterializedNavigationAliasRow, MaterializationError> {
    Ok(MaterializedNavigationAliasRow {
        source_page_id: PageId::from_uuid(Uuid::from_bytes(row.source_page_id)),
        owner_name: row.owner_name,
        owner_path: ManagedPath::parse(row.owner_path).map_err(typed_sql_decode_error)?,
        normalized_alias: row.normalized_alias,
    })
}

fn navigation_reference_name_row_from_storage(
    row: storage::PhysicalNavigationReferenceNameRow,
) -> Result<MaterializedNavigationReferenceNameRow, MaterializationError> {
    Ok(MaterializedNavigationReferenceNameRow {
        source_page_id: PageId::from_uuid(Uuid::from_bytes(row.source_page_id)),
        owner_path: ManagedPath::parse(row.owner_path).map_err(typed_sql_decode_error)?,
        raw_name: row.raw_name,
        normalized_name: row.normalized_name,
    })
}

fn block_row_from_storage(
    row: storage::PhysicalBlockRow,
) -> Result<MaterializedBlockRow, MaterializationError> {
    Ok(MaterializedBlockRow {
        block_id: BlockId::from_uuid(Uuid::from_bytes(row.block_id)),
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
        home_document_id: DocumentId::from_uuid(Uuid::from_bytes(row.home_document_id)),
        parent: row
            .parent
            .map(|id| BlockId::from_uuid(Uuid::from_bytes(id))),
        order: row.order,
        content: row.content,
        searchable_text: row.searchable_text,
        heading_level: row.heading_level,
        collapsed: row.collapsed,
        logseq_uuid: row
            .logseq_uuid
            .map(|id| LogseqUuid::from_uuid(Uuid::from_bytes(id))),
        logseq_identity_origin: row
            .logseq_identity_origin
            .map(identity_origin_from_sql)
            .transpose()
            .map_err(typed_sql_decode_error)?,
    })
}

fn block_home_claim_row_from_storage(
    row: storage::PhysicalBlockHomeClaimRow,
) -> Result<MaterializedBlockHomeClaimRow, MaterializationError> {
    let causal_dot = match (row.causal_peer_id, row.causal_counter) {
        (None, None) => None,
        (Some(peer), Some(counter)) => Some(
            BatchCausalDot::new(
                CausalPeerId::from_device_id(DeviceId::from_uuid(Uuid::from_bytes(peer))),
                counter,
            )
            .map_err(|error| MaterializationError::Corrupt(error.to_string()))?,
        ),
        _ => {
            return Err(MaterializationError::Corrupt(
                "block-home claim has incomplete causal provenance".into(),
            ))
        }
    };
    if row.batch_id.is_none() && causal_dot.is_some() {
        return Err(MaterializationError::Corrupt(
            "baseline block-home claim has accepted-batch causality".into(),
        ));
    }
    Ok(MaterializedBlockHomeClaimRow {
        block_id: BlockId::from_uuid(Uuid::from_bytes(row.block_id)),
        home_document_id: DocumentId::from_uuid(Uuid::from_bytes(row.home_document_id)),
        batch_id: row
            .batch_id
            .map(|id| BatchId::from_uuid(Uuid::from_bytes(id))),
        causal_dot,
    })
}

fn identity_record_row_from_storage(
    row: storage::PhysicalIdentityRecordRow,
) -> Result<MaterializedIdentityRecordRow, MaterializationError> {
    Ok(MaterializedIdentityRecordRow {
        key_digest: row.key_digest,
        record: row.record,
    })
}

fn logseq_uuid_introduction_row_from_storage(
    row: storage::PhysicalLogseqUuidIntroductionRow,
) -> Result<MaterializedLogseqUuidIntroductionRow, MaterializationError> {
    let causal_dot = match (row.causal_peer_id, row.causal_counter) {
        (None, None) => None,
        (Some(peer), Some(counter)) => Some(
            BatchCausalDot::new(
                CausalPeerId::from_device_id(DeviceId::from_uuid(Uuid::from_bytes(peer))),
                counter,
            )
            .map_err(|error| MaterializationError::Corrupt(error.to_string()))?,
        ),
        _ => {
            return Err(MaterializationError::Corrupt(
                "external UUID introduction has incomplete causal provenance".into(),
            ))
        }
    };
    if row.batch_id.is_none() && causal_dot.is_some() {
        return Err(MaterializationError::Corrupt(
            "baseline external UUID introduction has accepted-batch causality".into(),
        ));
    }
    Ok(MaterializedLogseqUuidIntroductionRow {
        logseq_uuid: LogseqUuid::from_uuid(Uuid::from_bytes(row.logseq_uuid)),
        block_id: BlockId::from_uuid(Uuid::from_bytes(row.block_id)),
        home_document_id: DocumentId::from_uuid(Uuid::from_bytes(row.home_document_id)),
        batch_id: row
            .batch_id
            .map(|id| BatchId::from_uuid(Uuid::from_bytes(id))),
        causal_dot,
    })
}

fn entity_from_storage(entity: storage::PhysicalEntityId) -> MaterializedEntityId {
    match entity {
        storage::PhysicalEntityId::Page(id) => {
            MaterializedEntityId::Page(PageId::from_uuid(Uuid::from_bytes(id)))
        }
        storage::PhysicalEntityId::Block(id) => {
            MaterializedEntityId::Block(BlockId::from_uuid(Uuid::from_bytes(id)))
        }
    }
}

fn referrer_row_from_storage(
    row: storage::PhysicalReferrerRow,
) -> Result<MaterializedReferrerRow, MaterializationError> {
    Ok(MaterializedReferrerRow {
        source: entity_from_storage(row.source),
        source_page_id: PageId::from_uuid(Uuid::from_bytes(row.source_page_id)),
        kind: MaterializedReferenceKind::from_sql(row.kind)?,
    })
}

fn block_reference_count_row_from_storage(
    row: storage::PhysicalBlockReferenceCountRow,
) -> Result<MaterializedBlockReferenceCountRow, MaterializationError> {
    Ok(MaterializedBlockReferenceCountRow {
        raw_uuid_claim: LogseqUuid::from_uuid(Uuid::from_bytes(row.raw_uuid_claim)),
        distinct_source_blocks: row.distinct_source_blocks,
    })
}

fn block_referrer_candidate_row_from_storage(
    row: storage::PhysicalBlockReferrerCandidateRow,
) -> Result<MaterializedBlockReferrerCandidateRow, MaterializationError> {
    Ok(MaterializedBlockReferrerCandidateRow {
        source_page_id: PageId::from_uuid(Uuid::from_bytes(row.source_page_id)),
        source_block_id: BlockId::from_uuid(Uuid::from_bytes(row.source_block_id)),
    })
}

fn page_referrer_candidate_row_from_storage(
    row: storage::PhysicalPageReferrerCandidateRow,
) -> Result<MaterializedPageReferrerCandidateRow, MaterializationError> {
    Ok(MaterializedPageReferrerCandidateRow {
        source_page_id: PageId::from_uuid(Uuid::from_bytes(row.source_page_id)),
        source: entity_from_storage(row.source),
    })
}

fn plain_text_candidate_page_row_from_storage(
    row: storage::PhysicalPlainTextCandidatePageRow,
) -> Result<MaterializedPlainTextCandidatePageRow, MaterializationError> {
    Ok(MaterializedPlainTextCandidatePageRow {
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
    })
}

fn fuzzy_candidate_page_row_from_storage(
    row: storage::PhysicalFuzzyCandidatePageRow,
) -> Result<MaterializedFuzzyCandidatePageRow, MaterializationError> {
    Ok(MaterializedFuzzyCandidatePageRow {
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
        path: ManagedPath::parse(row.path).map_err(typed_sql_decode_error)?,
    })
}

fn block_property_candidate_row_from_storage(
    row: storage::PhysicalBlockPropertyCandidateRow,
) -> Result<MaterializedBlockPropertyCandidateRow, MaterializationError> {
    Ok(MaterializedBlockPropertyCandidateRow {
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
        block_id: BlockId::from_uuid(Uuid::from_bytes(row.block_id)),
    })
}

fn property_facet_row_from_storage(
    row: storage::PhysicalPropertyFacetRow,
) -> Result<MaterializedPropertyFacetRow, MaterializationError> {
    Ok(MaterializedPropertyFacetRow {
        owner: entity_from_storage(row.owner),
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
        source_name: row.source_name,
        normalized_name: row.normalized_name,
        value: row.value,
        ordinal: row.ordinal,
    })
}

fn property_row_from_storage(
    row: storage::PhysicalPropertyRow,
) -> Result<MaterializedPropertyRow, MaterializationError> {
    Ok(MaterializedPropertyRow {
        owner: entity_from_storage(row.owner),
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
        name: row.name,
        value: row.value,
    })
}

fn tag_row_from_storage(
    row: storage::PhysicalTagRow,
) -> Result<MaterializedTagRow, MaterializationError> {
    Ok(MaterializedTagRow {
        owner: entity_from_storage(row.owner),
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
        tag: row.tag,
    })
}

fn task_row_from_storage(
    row: storage::PhysicalTaskRow,
) -> Result<MaterializedTaskRow, MaterializationError> {
    Ok(MaterializedTaskRow {
        block_id: BlockId::from_uuid(Uuid::from_bytes(row.block_id)),
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
        marker: row.marker,
        priority: row.priority,
        scheduled: row.scheduled,
        deadline: row.deadline,
    })
}

fn task_candidate_block_row_from_storage(
    row: storage::PhysicalTaskCandidateBlockRow,
) -> Result<MaterializedTaskCandidateBlockRow, MaterializationError> {
    Ok(MaterializedTaskCandidateBlockRow {
        block_id: BlockId::from_uuid(Uuid::from_bytes(row.block_id)),
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
        parent: row
            .parent
            .map(|id| BlockId::from_uuid(Uuid::from_bytes(id))),
        order: row.order,
        content: row.content,
        logseq_uuid: row
            .logseq_uuid
            .map(|id| LogseqUuid::from_uuid(Uuid::from_bytes(id))),
        page_name: row.page_name,
        page_path: ManagedPath::parse(row.page_path).map_err(typed_sql_decode_error)?,
        page_kind: text_kind_from_sql(row.page_text_kind).map_err(typed_sql_decode_error)?,
    })
}

fn block_structure_row_from_storage(
    row: storage::PhysicalBlockStructureRow,
) -> Result<MaterializedBlockStructureRow, MaterializationError> {
    Ok(MaterializedBlockStructureRow {
        block_id: BlockId::from_uuid(Uuid::from_bytes(row.block_id)),
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
        parent: row
            .parent
            .map(|id| BlockId::from_uuid(Uuid::from_bytes(id))),
        order: row.order,
    })
}

fn search_hit_from_storage(
    row: storage::PhysicalSearchHit,
) -> Result<MaterializedSearchHit, MaterializationError> {
    Ok(MaterializedSearchHit {
        entity: entity_from_storage(row.entity),
        page_id: PageId::from_uuid(Uuid::from_bytes(row.page_id)),
        text: row.text,
        rank: row.rank,
    })
}

fn typed_sql_decode_error(
    error: impl std::error::Error + Send + Sync + 'static,
) -> MaterializationError {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(error))
        .into()
}

fn text_kind_to_sql(kind: ManagedTextKind) -> i64 {
    match kind {
        ManagedTextKind::Page => 0,
        ManagedTextKind::Journal => 1,
    }
}

fn text_kind_from_sql(value: i64) -> Result<ManagedTextKind, MaterializationError> {
    match value {
        0 => Ok(ManagedTextKind::Page),
        1 => Ok(ManagedTextKind::Journal),
        _ => Err(MaterializationError::Corrupt(format!(
            "unknown managed text kind {value}"
        ))),
    }
}

fn identity_origin_to_sql(origin: LogseqIdentityOrigin) -> i64 {
    match origin {
        LogseqIdentityOrigin::ExternalImported => 0,
        LogseqIdentityOrigin::PolicyGenerated {
            reason: PolicyGeneratedAnchorReason::BlockReference,
        } => 1,
        LogseqIdentityOrigin::PolicyGenerated {
            reason: PolicyGeneratedAnchorReason::BlockEmbed,
        } => 2,
        LogseqIdentityOrigin::PolicyGenerated {
            reason: PolicyGeneratedAnchorReason::Export,
        } => 3,
        LogseqIdentityOrigin::PolicyGenerated {
            reason: PolicyGeneratedAnchorReason::CopiedDeepLink,
        } => 4,
    }
}

fn identity_origin_from_sql(value: i64) -> Result<LogseqIdentityOrigin, MaterializationError> {
    match value {
        0 => Ok(LogseqIdentityOrigin::ExternalImported),
        1 => Ok(LogseqIdentityOrigin::PolicyGenerated {
            reason: PolicyGeneratedAnchorReason::BlockReference,
        }),
        2 => Ok(LogseqIdentityOrigin::PolicyGenerated {
            reason: PolicyGeneratedAnchorReason::BlockEmbed,
        }),
        3 => Ok(LogseqIdentityOrigin::PolicyGenerated {
            reason: PolicyGeneratedAnchorReason::Export,
        }),
        4 => Ok(LogseqIdentityOrigin::PolicyGenerated {
            reason: PolicyGeneratedAnchorReason::CopiedDeepLink,
        }),
        _ => Err(MaterializationError::Corrupt(format!(
            "unknown Logseq identity origin {value}"
        ))),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MaterializationError {
    Sqlite(String),
    Schema(String),
    Corrupt(String),
    ResourceLimit {
        resource: &'static str,
        found: usize,
        maximum: usize,
    },
    InvalidInput(String),
    Incomplete(String),
    Contradiction(String),
    BatchMismatch {
        expected: BatchId,
        found: BatchId,
    },
    Stale {
        materialized: u64,
        frontier: u64,
    },
    SearchIndexBuilding {
        horizon_sequence: u64,
    },
    DuplicateCollision(BatchId),
    InvalidQuery(String),
}

impl fmt::Display for MaterializationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "SQLite materialization error: {error}"),
            Self::Schema(error) => write!(f, "materialization schema mismatch: {error}"),
            Self::Corrupt(error) => write!(f, "corrupt materialization: {error}"),
            Self::ResourceLimit {
                resource,
                found,
                maximum,
            } => write!(
                f,
                "materialization {resource} {found} exceeds limit {maximum}"
            ),
            Self::InvalidInput(error) => write!(f, "invalid materialization input: {error}"),
            Self::Incomplete(error) => write!(f, "incomplete materialization input: {error}"),
            Self::Contradiction(error) => {
                write!(f, "materialization contradicts accepted semantics: {error}")
            }
            Self::BatchMismatch { expected, found } => {
                write!(
                    f,
                    "materialization batch {found} != accepted batch {expected}"
                )
            }
            Self::Stale {
                materialized,
                frontier,
            } => write!(
                f,
                "materialization frontier {materialized} is stale against accepted frontier {frontier}"
            ),
            Self::DuplicateCollision(batch_id) => {
                write!(
                    f,
                    "materialization for batch {batch_id} has different canonical bytes"
                )
            }
            Self::SearchIndexBuilding { horizon_sequence } => {
                write!(f, "search index building from projection frontier {horizon_sequence}")
            }
            Self::InvalidQuery(error) => write!(f, "invalid materialization query: {error}"),
        }
    }
}

impl std::error::Error for MaterializationError {}

impl From<rusqlite::Error> for MaterializationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error.to_string())
    }
}

impl From<storage::MaterializationError> for MaterializationError {
    fn from(error: storage::MaterializationError) -> Self {
        if let Some(horizon_sequence) = error.search_index_building_horizon() {
            return Self::SearchIndexBuilding { horizon_sequence };
        }
        match error {
            storage::MaterializationError::Sqlite(error) => Self::Sqlite(error),
            storage::MaterializationError::Schema(error) => Self::Schema(error),
            storage::MaterializationError::Corrupt(error) => Self::Corrupt(error),
            storage::MaterializationError::ResourceLimit {
                resource,
                found,
                maximum,
            } => Self::ResourceLimit {
                resource,
                found,
                maximum,
            },
            storage::MaterializationError::InvalidInput(error) => Self::InvalidInput(error),
            storage::MaterializationError::Incomplete(error) => Self::Incomplete(error),
            storage::MaterializationError::Contradiction(error) => Self::Contradiction(error),
            storage::MaterializationError::Stale {
                materialized,
                frontier,
            } => Self::Stale {
                materialized,
                frontier,
            },
            storage::MaterializationError::InvalidQuery(error) => Self::InvalidQuery(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_id(value: u128) -> PageId {
        PageId::from_uuid(Uuid::from_u128(value))
    }

    fn document_id(value: u128) -> DocumentId {
        DocumentId::from_uuid(Uuid::from_u128(value))
    }

    fn block_id(value: u128) -> BlockId {
        BlockId::from_uuid(Uuid::from_u128(value))
    }

    fn batch_id(value: u128) -> BatchId {
        BatchId::from_uuid(Uuid::from_u128(value))
    }

    #[test]
    fn task_candidate_and_structure_rows_keep_typed_authority_boundaries() {
        let page = page_id(1);
        let parent = block_id(2);
        let block = block_id(3);
        let uuid = LogseqUuid::from_uuid(Uuid::from_u128(4));
        let candidate = storage::PhysicalTaskCandidateBlockRow {
            block_id: block.as_uuid().into_bytes(),
            page_id: page.as_uuid().into_bytes(),
            parent: Some(parent.as_uuid().into_bytes()),
            order: "a".into(),
            content: "TODO parser-owned semantics".into(),
            logseq_uuid: Some(uuid.as_uuid().into_bytes()),
            page_name: "Task page".into(),
            page_path: "nested/task.md".into(),
            page_text_kind: 0,
        };
        assert_eq!(
            task_candidate_block_row_from_storage(candidate).unwrap(),
            MaterializedTaskCandidateBlockRow {
                block_id: block,
                page_id: page,
                parent: Some(parent),
                order: "a".into(),
                content: "TODO parser-owned semantics".into(),
                logseq_uuid: Some(uuid),
                page_name: "Task page".into(),
                page_path: ManagedPath::parse("nested/task.md").unwrap(),
                page_kind: ManagedTextKind::Page,
            }
        );
        assert_eq!(
            block_structure_row_from_storage(storage::PhysicalBlockStructureRow {
                block_id: block.as_uuid().into_bytes(),
                page_id: page.as_uuid().into_bytes(),
                parent: Some(parent.as_uuid().into_bytes()),
                order: "a".into(),
            })
            .unwrap(),
            MaterializedBlockStructureRow {
                block_id: block,
                page_id: page,
                parent: Some(parent),
                order: "a".into(),
            }
        );

        let invalid_header = storage::PhysicalTaskCandidateBlockRow {
            block_id: block.as_uuid().into_bytes(),
            page_id: page.as_uuid().into_bytes(),
            parent: None,
            order: "a".into(),
            content: String::new(),
            logseq_uuid: None,
            page_name: "Task page".into(),
            page_path: "/not-a-managed-path.md".into(),
            page_text_kind: 99,
        };
        assert!(task_candidate_block_row_from_storage(invalid_header).is_err());
    }

    #[test]
    fn derived_reference_input_preserves_raw_spellings_and_structural_locators() {
        let source_page = page_id(1);
        let source_block = block_id(2);
        let locator = ReferenceSourceLocatorV1::Block {
            block_id: source_block,
            home_document_id: document_id(3),
        };
        let locator_bytes = canonical_reference_source_locator_bytes(locator).unwrap();
        assert!(ManagedPath::parse("nested/physical/layout/source.md").is_ok());
        assert!(!locator_bytes
            .windows(b"nested/physical/layout/source.md".len())
            .any(|window| window == b"nested/physical/layout/source.md"));

        let raw_name = " /Über/ ".to_owned();
        let normalized_name = crate::refs::page_key(&raw_name);
        let raw_alias = " /Alias/ ".to_owned();
        let normalized_alias = crate::refs::page_key(&raw_alias);
        let input = MaterializationChange::new(
            batch_id(4),
            vec![page_input(source_page, "source".into())],
            Vec::new(),
        )
        .unwrap()
        .with_derived_graph_facts(
            vec![MaterializedReferencePosting {
                source_page_id: source_page,
                source_entity: MaterializedEntityId::Page(source_page),
                source_locator: ReferenceSourceLocatorV1::Preamble,
                ordinal: 0,
                kind: ReferenceCatalogReferenceKind::PropertyKeyPseudoPage,
                target: MaterializedReferenceTarget::PageName {
                    raw_name: raw_name.clone(),
                    normalized_name: normalized_name.clone(),
                    resolved_page_id: None,
                },
            }],
            vec![MaterializedAliasDeclaration {
                source_page_id: source_page,
                source_entity: MaterializedEntityId::Page(source_page),
                source_locator: ReferenceSourceLocatorV1::Preamble,
                ordinal: 1,
                raw_alias: raw_alias.clone(),
                normalized_alias: normalized_alias.clone(),
            }],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            input.derived_reference_postings[0].target,
            MaterializedReferenceTarget::PageName {
                raw_name,
                normalized_name: crate::refs::page_key(" /Über/ "),
                resolved_page_id: None,
            }
        );
        assert_eq!(input.derived_aliases[0].raw_alias, raw_alias);
    }

    #[test]
    fn derived_reference_input_rejects_malformed_names_locators_and_aggregate_limits() {
        assert!(matches!(
            validate_reference_source_locator_bytes(b"not-a-postcard-locator"),
            Err(MaterializationError::InvalidInput(_))
        ));
        let malformed = MaterializedReferencePosting {
            source_page_id: page_id(1),
            source_entity: MaterializedEntityId::Page(page_id(1)),
            source_locator: ReferenceSourceLocatorV1::Preamble,
            ordinal: 0,
            kind: ReferenceCatalogReferenceKind::PageLink,
            target: MaterializedReferenceTarget::PageName {
                raw_name: "Correct spelling".into(),
                normalized_name: "wrong key".into(),
                resolved_page_id: None,
            },
        };
        assert!(matches!(
            MaterializationChange::new(
                batch_id(2),
                vec![page_input(page_id(1), "source".into())],
                Vec::new(),
            )
            .unwrap()
            .with_derived_graph_facts(vec![malformed], Vec::new(), Vec::new(),),
            Err(MaterializationError::InvalidInput(_))
        ));

        let mut budget = MaterializationInputBudget::default();
        assert!(matches!(
            budget.add_facet_values(MAX_MATERIALIZATION_CHANGE_FACET_VALUES + 1),
            Err(MaterializationError::ResourceLimit {
                resource: "materialization change facet values",
                ..
            })
        ));
        let mut budget = MaterializationInputBudget::default();
        assert!(matches!(
            budget.add_bytes(MAX_MATERIALIZATION_CHANGE_BYTES + 1),
            Err(MaterializationError::ResourceLimit {
                resource: "materialization change bytes",
                ..
            })
        ));
    }

    #[test]
    fn derived_reference_input_rejects_cross_kind_target_pairs() {
        for posting in [
            MaterializedReferencePosting {
                source_page_id: page_id(1),
                source_entity: MaterializedEntityId::Page(page_id(1)),
                source_locator: ReferenceSourceLocatorV1::Preamble,
                ordinal: 0,
                kind: ReferenceCatalogReferenceKind::PageLink,
                target: MaterializedReferenceTarget::ExternalUuid {
                    raw_claim: LogseqUuid::from_uuid(Uuid::from_u128(1)),
                    resolved_block_id: None,
                },
            },
            MaterializedReferencePosting {
                source_page_id: page_id(1),
                source_entity: MaterializedEntityId::Page(page_id(1)),
                source_locator: ReferenceSourceLocatorV1::Preamble,
                ordinal: 1,
                kind: ReferenceCatalogReferenceKind::BlockReference,
                target: MaterializedReferenceTarget::PageName {
                    raw_name: "page name".into(),
                    normalized_name: "page name".into(),
                    resolved_page_id: None,
                },
            },
        ] {
            assert!(matches!(
                MaterializationChange::new(
                    batch_id(2),
                    vec![page_input(page_id(1), "source".into())],
                    Vec::new(),
                )
                .unwrap()
                .with_derived_graph_facts(vec![posting], Vec::new(), Vec::new(),),
                Err(MaterializationError::InvalidInput(_))
            ));
        }
    }

    fn page_input(page: PageId, searchable_text: String) -> MaterializedPageInput {
        MaterializedPageInput {
            page_id: page,
            home_document_id: document_id(10_000),
            name: "shared".into(),
            name_key: "shared".into(),
            path: ManagedPath::parse(format!("test/{page}.md")).unwrap(),
            kind: ManagedTextKind::Page,
            preamble: None,
            searchable_text,
            references: Vec::new(),
            properties: Vec::new(),
            tags: Vec::new(),
            blocks: Vec::new(),
        }
    }

    fn block_input(
        block_id: BlockId,
        home_document_id: DocumentId,
        parent: Option<BlockId>,
        order: &str,
    ) -> MaterializedBlockInput {
        MaterializedBlockInput {
            block_id,
            home_document_id,
            parent,
            order: order.into(),
            content: block_id.to_string(),
            searchable_text: block_id.to_string(),
            query_visible: block_id.to_string(),
            heading_level: None,
            collapsed: false,
            logseq_uuid: None,
            logseq_identity_origin: None,
            references: Vec::new(),
            properties: Vec::new(),
            tags: Vec::new(),
            task: None,
            planning: None,
            path_ref_names: Vec::new(),
        }
    }

    #[test]
    fn materialization_boundaries_canonicalize_nested_outline_traversal_rows() {
        let page_id = page_id(310_000);
        let home_document_id = document_id(310_001);
        let root = block_id(310_030);
        let child = block_id(310_020);
        let sibling = block_id(310_010);
        let mut page = page_input(page_id, "nested traversal".into());
        page.home_document_id = home_document_id;
        page.blocks = vec![
            block_input(root, home_document_id, None, "0000000000"),
            block_input(child, home_document_id, Some(root), "0000000000"),
            block_input(sibling, home_document_id, None, "0000000001"),
        ];

        let change = MaterializationChange::new(batch_id(310_002), vec![page.clone()], Vec::new())
            .expect("nested traversal order is a valid materialization input");
        assert_eq!(
            change.replacements()[0]
                .blocks
                .iter()
                .map(|block| block.block_id)
                .collect::<Vec<_>>(),
            vec![child, root, sibling]
        );
        assert_eq!(change.replacements()[0].blocks[0].parent, Some(root));

        let physical = lower_terminal_chunk(
            TerminalMaterializationChunk {
                pages: vec![page],
                postings: Vec::new(),
                aliases: Vec::new(),
            },
            &ParseConfig::default(),
        )
        .expect("terminal bootstrap accepts the same nested traversal order");
        assert_eq!(
            physical.pages[0]
                .blocks
                .iter()
                .map(|block| block.block_id)
                .collect::<Vec<_>>(),
            vec![
                child.as_uuid().into_bytes(),
                root.as_uuid().into_bytes(),
                sibling.as_uuid().into_bytes(),
            ]
        );
        assert_eq!(
            physical.pages[0].blocks[0].parent,
            Some(root.as_uuid().into_bytes())
        );
    }

    #[test]
    fn terminal_lowering_seeds_true_baseline_identity_records() {
        let page_id = page_id(311_000);
        let block_id = block_id(311_001);
        let home_document_id = document_id(311_002);
        let logseq_uuid = LogseqUuid::from_uuid(Uuid::from_u128(311_003));
        let mut page = page_input(page_id, "baseline identity".into());
        page.name = "Baseline Identity".into();
        page.name_key = crate::refs::page_key(&page.name);
        page.path = ManagedPath::parse("pages/baseline-identity.md").unwrap();
        page.home_document_id = home_document_id;
        page.blocks.push(MaterializedBlockInput {
            block_id,
            home_document_id,
            parent: None,
            order: "a".into(),
            content: "baseline block".into(),
            searchable_text: "baseline block".into(),
            query_visible: "baseline block".into(),
            heading_level: None,
            collapsed: false,
            logseq_uuid: Some(logseq_uuid),
            logseq_identity_origin: Some(LogseqIdentityOrigin::ExternalImported),
            references: Vec::new(),
            properties: Vec::new(),
            tags: Vec::new(),
            task: None,
            planning: None,
            path_ref_names: Vec::new(),
        });

        let physical = lower_terminal_chunk(
            TerminalMaterializationChunk {
                pages: vec![page.clone()],
                postings: Vec::new(),
                aliases: Vec::new(),
            },
            &ParseConfig::default(),
        )
        .unwrap();

        let name_key = LogicalPageName::parse(&page.name).unwrap().key_digest();
        let name_record = crate::oplog::sqlite_identity::PageNameIdentityRecordV1::decode(
            name_key,
            &physical.page_name_identity_records[0].record,
        )
        .unwrap();
        let name_occupied = name_record.occupied().unwrap();
        assert_eq!(name_occupied.page_id(), page_id);
        assert_eq!(
            name_occupied.acquisition(),
            crate::oplog::sqlite_identity::IdentityOriginV1::Baseline
        );
        assert_eq!(
            name_occupied.exact_state(),
            crate::oplog::sqlite_identity::IdentityOriginV1::Baseline
        );

        let path_key = page.path.portable_key().digest();
        let path_record = crate::oplog::sqlite_identity::PortablePathIdentityRecordV1::decode(
            path_key,
            &physical.portable_path_identity_records[0].record,
        )
        .unwrap();
        let path_occupied = path_record.occupied().unwrap();
        assert_eq!(path_occupied.page_id(), page_id);
        assert_eq!(
            path_occupied.acquisition(),
            crate::oplog::sqlite_identity::IdentityOriginV1::Baseline
        );

        assert_eq!(physical.block_home_claims.len(), 1);
        assert!(physical.block_home_claims[0].batch_id.is_none());
        assert!(physical.block_home_claims[0].causal_peer_id.is_none());
        assert!(physical.block_home_claims[0].causal_counter.is_none());
        assert_eq!(physical.logseq_uuid_introductions.len(), 1);
        assert!(physical.logseq_uuid_introductions[0].batch_id.is_none());
        assert!(physical.logseq_uuid_introductions[0]
            .causal_peer_id
            .is_none());
        assert!(physical.logseq_uuid_introductions[0]
            .causal_counter
            .is_none());
    }

    fn semantic_effect_for_replacements(pages: &[MaterializedPageInput]) -> Vec<u8> {
        SemanticEffect::new(
            pages
                .iter()
                .map(|page| super::super::PageDelta {
                    page_id: page.page_id,
                    before: None,
                    after: Some(PageState::Live {
                        name: super::super::LogicalPageName::parse(&page.name).unwrap(),
                        path: page.path.clone(),
                        home_document_id: page.home_document_id,
                        kind: page.kind,
                    }),
                    lifecycle: super::super::PageDeltaLifecycle::Ordinary,
                })
                .collect(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap()
        .encode()
        .unwrap()
    }

    #[test]
    fn membership_validation_retains_presence_and_absence_semantics_across_many_blocks() {
        let page_id = page_id(410_000);
        let home_document_id = document_id(410_001);
        let target_block_id = BlockId::from_uuid(Uuid::from_u128(410_020));
        let mut page = page_input(page_id, "membership index".into());
        page.blocks = (0..32)
            .map(|index| {
                let block_id = if index == 23 {
                    target_block_id
                } else {
                    BlockId::from_uuid(Uuid::from_u128(410_100 + index))
                };
                MaterializedBlockInput {
                    block_id,
                    home_document_id,
                    parent: None,
                    order: format!("{index:02}"),
                    content: format!("block {index}"),
                    searchable_text: format!("block {index}"),
                    query_visible: format!("block {index}"),
                    heading_level: None,
                    collapsed: false,
                    logseq_uuid: None,
                    logseq_identity_origin: None,
                    references: Vec::new(),
                    properties: Vec::new(),
                    tags: Vec::new(),
                    task: None,
                    planning: None,
                    path_ref_names: Vec::new(),
                }
            })
            .collect();
        let semantic_effect = SemanticEffect::new(
            Vec::new(),
            Vec::new(),
            vec![super::super::MembershipDelta {
                page_id,
                block_id: target_block_id,
                before: None,
                after: Some(
                    super::super::MembershipClaim::new(home_document_id, None, "23").unwrap(),
                ),
            }],
        )
        .unwrap()
        .encode()
        .unwrap();

        let accepted =
            MaterializationChange::new(batch_id(410_002), vec![page.clone()], Vec::new()).unwrap();
        assert!(accepted
            .validate_against_stored(batch_id(410_002), &semantic_effect)
            .is_ok());

        page.blocks
            .retain(|block| block.block_id != target_block_id);
        let missing =
            MaterializationChange::new(batch_id(410_002), vec![page], Vec::new()).unwrap();
        assert!(matches!(
            missing.validate_against_stored(batch_id(410_002), &semantic_effect),
            Err(MaterializationError::Contradiction(message))
                if message.contains("accepted member") && message.contains("absent")
        ));
    }

    fn resource_limit(error: Result<MaterializationChange, MaterializationError>, resource: &str) {
        assert!(matches!(
            error,
            Err(MaterializationError::ResourceLimit {
                resource: found,
                ..
            }) if found == resource
        ));
    }

    #[test]
    fn materialization_input_limits_reject_before_digest_or_sqlite_write() {
        let page = page_id(1);
        let mut oversized_field = page_input(page, String::new());
        oversized_field.name = "x".repeat(MAX_MATERIALIZATION_FIELD_BYTES + 1);
        resource_limit(
            MaterializationChange::new(batch_id(1), vec![oversized_field], Vec::new()),
            "page name bytes",
        );

        let reference = MaterializedReference {
            target: MaterializedEntityId::Page(page_id(2)),
            kind: MaterializedReferenceKind::Reference,
        };
        let mut oversized_facet_count = page_input(page_id(3), String::new());
        oversized_facet_count.references = vec![reference; MAX_MATERIALIZATION_FACET_VALUES + 1];
        resource_limit(
            MaterializationChange::new(batch_id(2), vec![oversized_facet_count], Vec::new()),
            "reference facet values",
        );

        let oversized_property = MaterializedProperty {
            name: "n".into(),
            value: "x".repeat(MAX_MATERIALIZATION_FIELD_BYTES),
        };
        let mut oversized_facet_bytes = page_input(page_id(4), String::new());
        oversized_facet_bytes.properties = vec![
            oversized_property;
            MAX_MATERIALIZATION_FACET_BYTES
                / MAX_MATERIALIZATION_FIELD_BYTES
        ];
        resource_limit(
            MaterializationChange::new(batch_id(3), vec![oversized_facet_bytes], Vec::new()),
            "property facet bytes",
        );

        let too_many_deletions = (0..=MAX_MATERIALIZATION_CHANGE_PAGES)
            .map(|index| page_id(100_000 + index as u128))
            .collect();
        resource_limit(
            MaterializationChange::new(batch_id(4), Vec::new(), too_many_deletions),
            "materialization change pages",
        );

        let oversized_change = (0..=MAX_MATERIALIZATION_CHANGE_BYTES
            / MAX_MATERIALIZATION_FIELD_BYTES)
            .map(|index| {
                page_input(
                    page_id(200_000 + index as u128),
                    "x".repeat(MAX_MATERIALIZATION_FIELD_BYTES),
                )
            })
            .collect();
        resource_limit(
            MaterializationChange::new(batch_id(5), oversized_change, Vec::new()),
            "materialization change bytes",
        );

        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection, ContentDigest::of(b"empty")).unwrap();
        let too_large = "x".repeat(MAX_MATERIALIZATION_FIELD_BYTES + 1);
        assert!(connection
            .execute(
                "INSERT INTO pages (
                     page_id, home_document_id, name, name_key, path, text_kind,
                     preamble, searchable_text
                 ) VALUES (?1, ?2, ?3, 'key', 'test/schema.md', 0, NULL, '')",
                params![
                    page_id(300_000).as_uuid().as_bytes().as_slice(),
                    document_id(300_001).as_uuid().as_bytes().as_slice(),
                    too_large,
                ],
            )
            .is_err());
    }

    #[test]
    fn materialization_input_schema_refuses_prior_and_future_before_sqlite_write() {
        assert_eq!(MATERIALIZATION_INPUT_SCHEMA_VERSION, 8);
        let current = MaterializationChange::new(
            batch_id(500_000),
            vec![page_input(page_id(500_001), "current".into())],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(current.schema_version, MATERIALIZATION_INPUT_SCHEMA_VERSION);

        for schema_version in [
            MATERIALIZATION_INPUT_SCHEMA_VERSION - 1,
            MATERIALIZATION_INPUT_SCHEMA_VERSION + 1,
        ] {
            let mut rejected = current.clone();
            rejected.schema_version = schema_version;
            let encoded = postcard::to_allocvec(&rejected).unwrap();
            let rejected: MaterializationChange = postcard::from_bytes(&encoded).unwrap();
            assert!(matches!(
                rejected.digest(),
                Err(MaterializationError::InvalidInput(message))
                    if message == format!("unknown materialization input schema {schema_version}")
            ));

            let mut connection = Connection::open_in_memory().unwrap();
            let empty_frontier = ContentDigest::of(b"empty");
            initialize_schema(&connection, empty_frontier).unwrap();
            let transaction = connection.transaction().unwrap();
            assert!(matches!(
                apply_change(
                    &transaction,
                    &rejected,
                    b"",
                    1,
                    ContentDigest::of(b"input"),
                    ContentDigest::of(b"next"),
                ),
                Err(MaterializationError::InvalidInput(message))
                    if message == format!("unknown materialization input schema {schema_version}")
            ));
            transaction.commit().unwrap();
            let page_count: i64 = connection
                .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
                .unwrap();
            let batch_count: i64 = connection
                .query_row("SELECT COUNT(*) FROM materialization_batches", [], |row| {
                    row.get(0)
                })
                .unwrap();
            let stamp_sequence: i64 = connection
                .query_row(
                    "SELECT acceptance_sequence FROM materialization_stamp WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!((page_count, batch_count, stamp_sequence), (0, 0, 0));
        }
    }

    #[test]
    fn non_page_effect_replacement_without_prior_metadata_fails_closed() {
        let page_id = page_id(600_000);
        let mut page = page_input(page_id, "preamble searchable".into());
        page.preamble = Some("updated preamble".into());
        let change = MaterializationChange::new(batch_id(600_001), vec![page], Vec::new()).unwrap();
        let semantic_effect = SemanticEffect::new_with_page_preambles(
            Vec::new(),
            vec![super::super::PagePreambleDelta {
                page_id,
                home_document_id: document_id(10_000),
                before: Some(super::super::PagePreambleState {
                    page_id,
                    home_document_id: document_id(10_000),
                    preamble: None,
                }),
                after: Some(super::super::PagePreambleState {
                    page_id,
                    home_document_id: document_id(10_000),
                    preamble: Some("updated preamble".into()),
                }),
            }],
            Vec::new(),
            Vec::new(),
        )
        .unwrap()
        .encode()
        .unwrap();
        let mut connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection, ContentDigest::of(b"empty")).unwrap();
        let transaction = connection.transaction().unwrap();
        assert!(matches!(
            apply_change(
                &transaction,
                &change,
                &semantic_effect,
                1,
                change.digest().unwrap(),
                ContentDigest::of(b"next"),
            ),
            Err(MaterializationError::Incomplete(message))
                if message.contains("lacks prior validated metadata")
        ));
        transaction.commit().unwrap();
        let state: (i64, i64, i64) = connection
            .query_row(
                "SELECT
                     (SELECT COUNT(*) FROM pages),
                     (SELECT COUNT(*) FROM materialization_batches),
                     (SELECT acceptance_sequence FROM materialization_stamp WHERE singleton = 1)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, (0, 0, 0));
    }

    #[test]
    fn materialized_reads_reject_oversized_queries_and_aggregate_output() {
        let mut connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection, ContentDigest::of(b"empty")).unwrap();
        let searchable_text = format!("needle {}", "x".repeat(1024 * 1024 - "needle ".len()));
        let pages_per_change = 33;
        let mut final_frontier = ContentDigest::of(b"empty");
        for group in 0..2 {
            let replacements = (0..pages_per_change)
                .map(|index| {
                    page_input(
                        page_id(400_000 + (group * pages_per_change + index) as u128),
                        searchable_text.clone(),
                    )
                })
                .collect();
            let change = MaterializationChange::new(
                batch_id(400_000 + group as u128),
                replacements,
                Vec::new(),
            )
            .unwrap();
            let digest = change.digest().unwrap();
            final_frontier = ContentDigest::of(&[group as u8 + 1]);
            let semantic_effect = semantic_effect_for_replacements(change.replacements());
            let transaction = connection.transaction().unwrap();
            apply_change(
                &transaction,
                &change,
                &semantic_effect,
                group as u64 + 1,
                digest,
                final_frontier,
            )
            .unwrap();
            transaction.commit().unwrap();
        }
        let read = SqliteMaterializedRead::new(&connection, 2, final_frontier).unwrap();
        let oversized_query = "q".repeat(MAX_MATERIALIZATION_QUERY_BYTES + 1);
        assert!(matches!(
            read.search(&oversized_query, 1),
            Err(MaterializationError::ResourceLimit {
                resource: "materialization query bytes",
                ..
            })
        ));
        assert!(matches!(
            read.search("needle", pages_per_change * 2),
            Err(MaterializationError::ResourceLimit {
                resource: "materialization read output bytes",
                ..
            })
        ));
    }

    #[test]
    fn malformed_page_path_precedes_aggregate_read_budget_exhaustion() {
        let mut connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection, ContentDigest::of(b"empty")).unwrap();
        let searchable_text = format!("needle {}", "x".repeat(1024 * 1024 - "needle ".len()));
        let pages_per_change = 33;
        let mut final_frontier = ContentDigest::of(b"empty");
        for group in 0..2 {
            let replacements = (0..pages_per_change)
                .map(|index| {
                    page_input(
                        page_id(700_000 + (group * pages_per_change + index) as u128),
                        searchable_text.clone(),
                    )
                })
                .collect();
            let change = MaterializationChange::new(
                batch_id(700_000 + group as u128),
                replacements,
                Vec::new(),
            )
            .unwrap();
            let digest = change.digest().unwrap();
            final_frontier = ContentDigest::of(&[group as u8 + 1]);
            let semantic_effect = semantic_effect_for_replacements(change.replacements());
            let transaction = connection.transaction().unwrap();
            apply_change(
                &transaction,
                &change,
                &semantic_effect,
                group as u64 + 1,
                digest,
                final_frontier,
            )
            .unwrap();
            transaction.commit().unwrap();
        }

        connection
            .execute(
                "UPDATE pages SET path = ?1 WHERE page_id = ?2",
                params![
                    "../corrupt.md",
                    page_id(700_000).as_uuid().as_bytes().as_slice(),
                ],
            )
            .unwrap();

        let read = SqliteMaterializedRead::new(&connection, 2, final_frontier).unwrap();
        assert!(matches!(
            read.pages(None, pages_per_change * 2),
            Err(MaterializationError::Corrupt(message))
                if message.contains("malformed managed path row")
        ));
    }

    // -----------------------------------------------------------------------
    // §5.8 acceptance: the two derived tables, produced once and agreed on by
    // both backends and the tree walk.
    // -----------------------------------------------------------------------

    /// One page carrying every property form the closure and the atomizer have
    /// to survive together: a property-key-only block, an `alias::`, a `tags::`
    /// value, an inline `#tag` and a `[[link]]`.
    const PARITY_FIXTURE: &str = concat!(
        "alias:: Second Name\n",
        "tags:: Release, Docs\n",
        "\n",
        "- status:: open\n",
        "- tags:: Release, Docs\n",
        "- outer [[Alpha]]\n",
        "\t- inner #beta\n",
        "\t\t- leaf\n",
        // The three shapes `tasks` cannot represent, plus the one it can, so a
        // guard over them compares a populated table against a populated table.
        "- ship it\n",
        "  SCHEDULED: <2026-06-28 Sun>\n",
        "  DEADLINE: <2026-07-01 Wed>\n",
        "- [#A] ship it\n",
        "- TODO marked\n",
        "  SCHEDULED: <2026-06-29 Mon>\n",
        "  DEADLINE: <2026-07-02 Thu>\n",
    );

    const PARITY_PATH: &str = "pages/parity-page.md";
    const PARITY_NAME: &str = "Parity Page";

    fn parity_document() -> crate::doc::Document {
        parse_page(PARITY_PATH, PARITY_FIXTURE)
    }

    /// One page's text, parsed exactly as both backends parse it: the format
    /// comes from the path (`Format::from_path`, E4), and the blocks carry the
    /// runtime identities the producers key on.
    fn parse_page(rel_path: &str, text: &str) -> crate::doc::Document {
        let mut document = match crate::model::Format::from_path(std::path::Path::new(rel_path)) {
            crate::model::Format::Md => crate::doc::parse(text),
            crate::model::Format::Org => crate::org::parse_org(text),
        };
        crate::model::assign_doc_runtime_ids(&mut document.roots, rel_path);
        document
    }

    /// Direct Files' answer: content -> `block_path_refs` names.
    fn direct_files_path_refs() -> Vec<(String, Vec<String>)> {
        direct_files_path_refs_for(PARITY_NAME, PARITY_PATH, &parity_document())
    }

    fn direct_files_path_refs_for(
        name: &str,
        rel_path: &str,
        document: &crate::doc::Document,
    ) -> Vec<(String, Vec<String>)> {
        let entry = crate::model::PageEntry {
            name: name.to_owned(),
            kind: crate::model::PageKind::Page,
            date_key: None,
            rel_path: rel_path.to_owned(),
            path: std::path::PathBuf::from(rel_path),
        };
        let page = crate::direct_projection::physical_page_for_test(
            &entry,
            document,
            &ParseConfig::default(),
        )
        .expect("the page lowers through Direct Files");
        page.blocks
            .into_iter()
            .map(|block| (block.content, block.path_refs))
            .collect()
    }

    /// Managed Storage's answer, entered through the same capture the accepted
    /// event path uses so the guard compares producers, not transcriptions.
    fn managed_storage_path_refs() -> Vec<(String, Vec<String>)> {
        managed_storage_path_refs_for(&parity_page_input())
    }

    fn managed_storage_path_refs_for(page: &MaterializedPageInput) -> Vec<(String, Vec<String>)> {
        let lowered =
            lower_pages_with_derived_rows(std::slice::from_ref(page), &ParseConfig::default())
                .expect("the page lowers through Managed Storage");
        lowered
            .into_iter()
            .next()
            .expect("one lowered page")
            .blocks
            .into_iter()
            .map(|block| (block.content, block.path_refs))
            .collect()
    }

    /// The fixture as Managed Storage receives it: one page whose blocks carry
    /// the facets `document_facets_from_parsed_block` captures, including the
    /// `refs_norm` the closure runs over.
    fn parity_page_input() -> MaterializedPageInput {
        page_input_for(PARITY_NAME, PARITY_PATH, &parity_document())
    }

    fn page_input_for(
        name: &str,
        rel_path: &str,
        document: &crate::doc::Document,
    ) -> MaterializedPageInput {
        let mut flat: Vec<MaterializedBlockInput> = Vec::new();
        fn walk(
            blocks: &[crate::doc::DocBlock],
            parent: Option<BlockId>,
            out: &mut Vec<MaterializedBlockInput>,
        ) {
            for block in blocks {
                let id = BlockId::from_uuid(
                    Uuid::parse_str(&block.uuid).expect("assigned runtime block identity"),
                );
                let facets = super::super::sqlite::document_facets_from_parsed_block(block);
                out.push(MaterializedBlockInput {
                    block_id: id,
                    home_document_id: document_id(1),
                    parent,
                    order: format!("{:08x}", out.len()),
                    content: block.raw.clone(),
                    searchable_text: facets.searchable_text,
                    query_visible: facets.query_visible,
                    heading_level: facets.heading_level,
                    collapsed: facets.collapsed,
                    logseq_uuid: None,
                    logseq_identity_origin: None,
                    references: Vec::new(),
                    properties: facets.properties,
                    tags: facets.tags,
                    task: facets.task,
                    planning: facets.planning,
                    path_ref_names: facets.path_ref_names,
                });
                walk(&block.children, Some(id), out);
            }
        }
        walk(&document.roots, None, &mut flat);
        MaterializedPageInput {
            page_id: page_id(1),
            home_document_id: document_id(1),
            name: name.to_owned(),
            name_key: super::super::LogicalPageName::parse(name)
                .expect("the page name is a logical page name")
                .canonical_key()
                .as_str()
                .to_owned(),
            path: ManagedPath::parse(rel_path).unwrap(),
            kind: ManagedTextKind::Page,
            preamble: document.pre_block.clone(),
            searchable_text: name.to_owned(),
            references: Vec::new(),
            properties: Vec::new(),
            tags: Vec::new(),
            blocks: flat,
        }
    }

    /// Guard 2 (§5.8 G1, I-19). One graph, two backends, one behaviour: the
    /// rows Direct Files writes, the rows Managed Storage writes and the
    /// closure the tree walk materializes are the same list for the same page.
    /// The fixture pins whatever `refs_norm` holds for each form; it does not
    /// legislate it.
    #[test]
    fn direct_files_managed_storage_and_the_walk_agree_on_block_path_refs() {
        let direct = direct_files_path_refs();
        let managed = managed_storage_path_refs();
        let walked =
            crate::query::walk_closure_names_for_test(PARITY_NAME, &parity_document().roots);

        // Stated first: three empty lists would agree and prove nothing.
        assert_eq!(direct.len(), 8, "the fixture page has eight blocks");
        assert!(
            direct.iter().any(|(_, refs)| refs.len() > 1),
            "at least one block must inherit a ref from an ancestor"
        );

        assert_eq!(direct, managed, "Direct Files and Managed Storage disagree");
        assert_eq!(direct, walked, "the producers and the walk disagree");

        // The shared truth, recorded. This is what `refs_norm` holds for each
        // form today, not a rule this guard imposes: `[[Alpha]]` and `#beta`
        // are the only two forms that produce a ref, every block carries its
        // own page, and descendants inherit their ancestors' refs. Note that a
        // block-level `tags::` value contributes NO ref here -- all three
        // producers agree on that, so it is a recorded property of the parser,
        // not a backend disagreement.
        assert_eq!(
            direct,
            vec![
                ("status:: open".to_owned(), vec!["parity page".to_owned()]),
                (
                    "tags:: Release, Docs".to_owned(),
                    vec!["parity page".to_owned()]
                ),
                (
                    "outer [[Alpha]]".to_owned(),
                    vec!["alpha".to_owned(), "parity page".to_owned()]
                ),
                (
                    "inner #beta".to_owned(),
                    vec![
                        "alpha".to_owned(),
                        "beta".to_owned(),
                        "parity page".to_owned()
                    ]
                ),
                (
                    "leaf".to_owned(),
                    vec![
                        "alpha".to_owned(),
                        "beta".to_owned(),
                        "parity page".to_owned()
                    ]
                ),
                // The three planning blocks reference nothing, so each carries
                // only its own page -- recorded, not required.
                (
                    "ship it\nSCHEDULED: <2026-06-28 Sun>\nDEADLINE: <2026-07-01 Wed>".to_owned(),
                    vec!["parity page".to_owned()]
                ),
                ("[#A] ship it".to_owned(), vec!["parity page".to_owned()]),
                (
                    "TODO marked\nSCHEDULED: <2026-06-29 Mon>\nDEADLINE: <2026-07-02 Thu>"
                        .to_owned(),
                    vec!["parity page".to_owned()]
                ),
            ]
        );
    }

    /// One page's planning rows as a producer emits them.
    type PlanningRows = Vec<(String, Option<PlanningRow>)>;
    type PlanningRow = (
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<i64>,
    );

    fn planning_rows(page: &storage::PhysicalPage) -> PlanningRows {
        page.blocks
            .iter()
            .map(|block| {
                (
                    block.content.clone(),
                    block.planning.as_ref().map(|planning| {
                        (
                            planning.priority.clone(),
                            planning.scheduled.clone(),
                            planning.scheduled_day,
                            planning.deadline.clone(),
                            planning.deadline_day,
                        )
                    }),
                )
            })
            .collect()
    }

    fn direct_files_page(
        name: &str,
        rel_path: &str,
        kind: crate::model::PageKind,
    ) -> storage::PhysicalPage {
        let entry = crate::model::PageEntry {
            name: name.to_owned(),
            kind,
            date_key: None,
            rel_path: rel_path.to_owned(),
            path: std::path::PathBuf::from(rel_path),
        };
        crate::direct_projection::physical_page_for_test(
            &entry,
            &parse_page(rel_path, PARITY_FIXTURE),
            &ParseConfig::default(),
        )
        .expect("the page lowers through Direct Files")
    }

    fn managed_page(page: &MaterializedPageInput, config: &ParseConfig) -> storage::PhysicalPage {
        lower_pages_with_derived_rows(std::slice::from_ref(page), config)
            .expect("the page lowers through Managed Storage")
            .into_iter()
            .next()
            .expect("one lowered page")
    }

    /// Guard 2b (§5.8, §3.2 M2, I-19). `block_planning` is the same list on
    /// both backends and under the tree walk -- and it is populated for the
    /// three blocks `tasks` cannot hold at all.
    ///
    /// The walk's answer is `BlockProjection`'s three fields, which are
    /// independent of `marker` by construction; the fixture pins what they hold
    /// for each form rather than legislating it.
    #[test]
    fn direct_files_managed_storage_and_the_walk_agree_on_block_planning() {
        let direct = planning_rows(&direct_files_page(
            PARITY_NAME,
            PARITY_PATH,
            crate::model::PageKind::Page,
        ));
        let managed = planning_rows(&managed_page(&parity_page_input(), &ParseConfig::default()));

        let mut walked: PlanningRows = Vec::new();
        fn walk(blocks: &[crate::doc::DocBlock], out: &mut PlanningRows) {
            for block in blocks {
                let projection = block.projection();
                let row = (projection.priority.is_some()
                    || projection.scheduled.is_some()
                    || projection.deadline.is_some())
                .then(|| {
                    (
                        projection.priority.clone(),
                        projection.scheduled.clone(),
                        projection
                            .scheduled
                            .as_deref()
                            .and_then(crate::query::eval::planning_day),
                        projection.deadline.clone(),
                        projection
                            .deadline
                            .as_deref()
                            .and_then(crate::query::eval::planning_day),
                    )
                });
                out.push((block.raw.clone(), row));
                walk(&block.children, out);
            }
        }
        walk(&parity_document().roots, &mut walked);

        // Stated first: three all-`None` lists would agree and prove nothing.
        let with_rows = direct.iter().filter(|(_, row)| row.is_some()).count();
        assert_eq!(with_rows, 3, "the fixture has three planning blocks");

        assert_eq!(direct, managed, "Direct Files and Managed Storage disagree");
        assert_eq!(direct, walked, "the producers and the walk disagree");

        // The shared truth, recorded. Two of the three rows belong to blocks
        // with NO marker, so `tasks` holds neither -- which is the whole reason
        // this table exists.
        let markerless = direct
            .iter()
            .filter(|(content, row)| row.is_some() && !content.contains("TODO"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            markerless,
            vec![
                (
                    "ship it\nSCHEDULED: <2026-06-28 Sun>\nDEADLINE: <2026-07-01 Wed>".to_owned(),
                    Some((
                        None,
                        Some("2026-06-28 Sun".to_owned()),
                        Some(20_260_628),
                        Some("2026-07-01 Wed".to_owned()),
                        Some(20_260_701),
                    )),
                ),
                (
                    "[#A] ship it".to_owned(),
                    Some((Some("A".to_owned()), None, None, None, None)),
                ),
            ]
        );
        let marked = direct
            .iter()
            .filter(|(content, _)| content.starts_with("TODO"))
            .collect::<Vec<_>>();
        assert_eq!(marked.len(), 1, "the fixture has one marked task");
        assert!(
            marked[0].1.is_some(),
            "a marked task gets a planning row too, not only a tasks row"
        );
    }

    /// Guard 2c (§5.8, §5.10, I-19). The two query-text columns are the block's
    /// EXACT visible text and its canonical fold on both backends -- not the
    /// whitespace-collapsed `searchable_text` beside them.
    #[test]
    fn direct_files_and_managed_storage_agree_on_the_query_visible_columns() {
        let direct = direct_files_page(PARITY_NAME, PARITY_PATH, crate::model::PageKind::Page);
        let managed = managed_page(&parity_page_input(), &ParseConfig::default());
        let columns = |page: &storage::PhysicalPage| {
            page.blocks
                .iter()
                .map(|block| {
                    (
                        block.query_visible.clone(),
                        block.query_visible_folded.clone(),
                        block.searchable_text.clone(),
                        // §5.10: this is the column the substring FTS indexes,
                        // so the candidate bound the SQL compiler emits is only
                        // backend-independent if the two producers write it the
                        // same way. It is `canonical_fold(searchable_text)` on
                        // both, i.e. the same fold over COLLAPSED text.
                        block.normalized_searchable_text.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let direct_columns = columns(&direct);
        assert_eq!(
            direct_columns,
            columns(&managed),
            "the two producers disagree on the query-visible columns"
        );

        // The columns are the projection's own visible text, folded once.
        let mut expected: Vec<(String, String)> = Vec::new();
        fn walk(blocks: &[crate::doc::DocBlock], out: &mut Vec<(String, String)>) {
            for block in blocks {
                let visible = block.projection().visible.clone();
                let folded = crate::search_query::canonical_fold(&visible);
                out.push((visible, folded));
                walk(&block.children, out);
            }
        }
        walk(&parity_document().roots, &mut expected);
        assert_eq!(
            direct_columns
                .iter()
                .map(|(visible, folded, _, _)| (visible.clone(), folded.clone()))
                .collect::<Vec<_>>(),
            expected,
            "the columns are exactly `visible` and `canonical_fold(visible)`"
        );
        // And the FTS source column is the same fold over the COLLAPSED text,
        // on both backends — which is what makes §5.10's candidate needle a
        // whitespace-free run rather than the whole phrase.
        for (_, _, searchable, normalized) in &direct_columns {
            assert_eq!(
                normalized,
                &crate::search_query::canonical_fold(searchable),
                "`normalized_searchable_text` is `canonical_fold(searchable_text)`"
            );
        }

        // And they are NOT `searchable_text`: the multi-line planning block
        // keeps its newlines here and loses them there. Stated as a difference
        // the fixture actually contains, so the two columns cannot quietly
        // become the same value.
        assert!(
            direct_columns
                .iter()
                .any(|(visible, _, searchable, _)| visible != searchable),
            "the fixture must contain a block whose visible text is not its collapsed text"
        );
    }

    /// Guard 2d (§3.2 K18). `tags.tag_key` is the page key of the tag, on both
    /// backends -- so `tag('X')` and `[[X]]` cannot disagree about one word.
    #[test]
    fn direct_files_and_managed_storage_agree_on_tag_keys() {
        let mut input = parity_page_input();
        input.tags = vec!["Release".to_owned(), "Docs".to_owned()];
        let managed = managed_page(&input, &ParseConfig::default());
        let mut direct = direct_files_page(PARITY_NAME, PARITY_PATH, crate::model::PageKind::Page);
        direct.tags = crate::query::derived::tag_rows(&["Release".to_owned(), "Docs".to_owned()]);

        let keys = |page: &storage::PhysicalPage| {
            page.tags
                .iter()
                .map(|tag| (tag.tag.clone(), tag.tag_key.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&direct), keys(&managed));
        assert_eq!(
            keys(&direct),
            vec![
                ("Release".to_owned(), "release".to_owned()),
                ("Docs".to_owned(), "docs".to_owned()),
            ],
            "the key is `refs::page_key(tag)`, the page-identity key"
        );
        // Block-level inline tags carry the same key rule.
        let inline = direct
            .blocks
            .iter()
            .flat_map(|block| block.tags.iter())
            .map(|tag| (tag.tag.clone(), tag.tag_key.clone()))
            .collect::<Vec<_>>();
        assert!(
            inline
                .iter()
                .any(|(tag, key)| tag == "beta" && key == "beta"),
            "the inline #beta tag carries its key too: {inline:?}"
        );
    }

    /// Guard 2e (§5.8, §3.2). `pages.journal_day` is the journal page's
    /// `yyyymmdd` on both backends, under the graph's OWN
    /// `:journal/file-name-format` -- and it is NULL for an ordinary page.
    ///
    /// The custom format is the point: a day derived under the default format
    /// would be silently wrong for exactly the graphs that configured one, and
    /// the format reaches the producer only because it is a `ParseConfig` field
    /// (§5.8 C3).
    #[test]
    fn both_backends_read_the_journal_day_under_the_graphs_own_file_name_format() {
        let config = crate::config::Config::parse(
            "{:journal/file-name-format \"dd_MM_yyyy\" :journal/page-title-format \"dd_MM_yyyy\"}",
        )
        .parse_config();
        let rel_path = "journals/28_06_2026.md";
        let name = "28_06_2026";

        let direct_journal = {
            let entry = crate::model::PageEntry {
                name: name.to_owned(),
                kind: crate::model::PageKind::Journal,
                date_key: None,
                rel_path: rel_path.to_owned(),
                path: std::path::PathBuf::from(rel_path),
            };
            crate::direct_projection::physical_page_for_test(
                &entry,
                &parse_page(rel_path, PARITY_FIXTURE),
                &config,
            )
            .expect("the journal lowers through Direct Files")
            .journal_day
        };

        let mut managed_input =
            page_input_for(name, rel_path, &parse_page(rel_path, PARITY_FIXTURE));
        managed_input.kind = ManagedTextKind::Journal;
        managed_input.path = ManagedPath::parse(rel_path.to_owned()).unwrap();
        let managed_journal = managed_page(&managed_input, &config).journal_day;

        assert_eq!(direct_journal, Some(20_260_628));
        assert_eq!(direct_journal, managed_journal, "the backends disagree");

        // The same file under the DEFAULT format is not a journal day at all,
        // so the config really is what decided the answer.
        let entry = crate::model::PageEntry {
            name: name.to_owned(),
            kind: crate::model::PageKind::Journal,
            date_key: None,
            rel_path: rel_path.to_owned(),
            path: std::path::PathBuf::from(rel_path),
        };
        assert_eq!(
            crate::direct_projection::physical_page_for_test(
                &entry,
                &parse_page(rel_path, PARITY_FIXTURE),
                &ParseConfig::default(),
            )
            .unwrap()
            .journal_day,
            None,
            "the default format must not parse this stem, or the guard is vacuous"
        );

        // An ordinary page never carries a day, whatever its name looks like.
        assert_eq!(
            direct_files_page(PARITY_NAME, PARITY_PATH, crate::model::PageKind::Page).journal_day,
            None
        );
        assert_eq!(
            managed_page(&parity_page_input(), &config).journal_day,
            None
        );
    }

    /// Guard 5 (AGENTS §4 tier 2). The three-way agreement above, re-run over
    /// every page of a real graph.
    ///
    /// Opt-in because this repository ships no corpus of that scale or shape:
    /// `TINE_DERIVED_PARITY_GRAPH=~/research/logseq-anonymized`. Only aggregate
    /// counts are printed; no corpus content is ever emitted, and on a
    /// disagreement the failure names the page's INDEX, not its text.
    ///
    /// A disagreement here is a corpus defect in the synthetic fixture above:
    /// the fix is to extract the minimal shape into the permanent fast corpus,
    /// not to weaken this gate.
    #[test]
    #[ignore = "acceptance gate over a real corpus: set TINE_DERIVED_PARITY_GRAPH"]
    fn derived_rows_agree_across_backends_over_a_real_corpus() {
        let Some(root) = std::env::var_os("TINE_DERIVED_PARITY_GRAPH") else {
            eprintln!("skipped: set TINE_DERIVED_PARITY_GRAPH to a corpus directory");
            return;
        };
        let graph = crate::model::Graph::open(std::path::PathBuf::from(&root));
        let mut pages = 0usize;
        let mut blocks = 0usize;
        let mut rows = 0usize;
        let mut atoms = 0usize;
        let mut planning = 0usize;
        let mut journal_days = 0usize;
        let mut disagreements = 0usize;
        let config = ParseConfig::default();
        let days = crate::query::derived::JournalDays::new(&config);
        for (index, entry) in graph.list_pages().into_iter().enumerate() {
            let Ok(text) = std::fs::read_to_string(&entry.path) else {
                continue;
            };
            let document = parse_page(&entry.rel_path, &text);
            let Ok(managed_path) = ManagedPath::parse(entry.rel_path.clone()) else {
                continue;
            };
            let mut input = page_input_for(&entry.name, &entry.rel_path, &document);
            input.path = managed_path;
            input.kind = if entry.kind == crate::model::PageKind::Journal {
                ManagedTextKind::Journal
            } else {
                ManagedTextKind::Page
            };
            let direct = direct_files_path_refs_for(&entry.name, &entry.rel_path, &document);
            let managed = managed_storage_path_refs_for(&input);
            let walked = crate::query::walk_closure_names_for_test(&entry.name, &document.roots);
            if direct != managed || direct != walked {
                disagreements += 1;
                eprintln!(
                    "derived-row parity disagreement at corpus page index {index} \
                     (direct={} managed={} walk={} block rows)",
                    direct.len(),
                    managed.len(),
                    walked.len()
                );
            }
            // The four §5.8 objects this packet adds, on the same corpus page.
            let direct_page = {
                let page_entry = crate::model::PageEntry {
                    name: entry.name.clone(),
                    kind: entry.kind,
                    date_key: None,
                    rel_path: entry.rel_path.clone(),
                    path: entry.path.clone(),
                };
                crate::direct_projection::physical_page_for_test(&page_entry, &document, &config)
                    .expect("the corpus page lowers through Direct Files")
            };
            let managed_page = managed_page(&input, &config);
            if planning_rows(&direct_page) != planning_rows(&managed_page)
                || direct_page.journal_day != managed_page.journal_day
                || direct_page
                    .blocks
                    .iter()
                    .map(|block| (&block.query_visible, &block.query_visible_folded))
                    .ne(managed_page
                        .blocks
                        .iter()
                        .map(|block| (&block.query_visible, &block.query_visible_folded)))
                || direct_page
                    .blocks
                    .iter()
                    .flat_map(|block| block.tags.iter().map(|tag| &tag.tag_key))
                    .ne(managed_page
                        .blocks
                        .iter()
                        .flat_map(|block| block.tags.iter().map(|tag| &tag.tag_key)))
            {
                disagreements += 1;
                eprintln!("§5.8 projection-object disagreement at corpus page index {index}");
            }
            // The journal day the producer derives must be the day the page
            // entry already carries: two rules for one question is the defect.
            let derived_day = days.day(
                &entry.rel_path,
                entry.kind == crate::model::PageKind::Journal,
            );
            if derived_day != entry.date_key {
                disagreements += 1;
                eprintln!("journal-day disagreement at corpus page index {index}");
            }
            journal_days += usize::from(derived_day.is_some());
            planning += direct_page
                .blocks
                .iter()
                .filter(|block| block.planning.is_some())
                .count();
            pages += 1;
            blocks += direct.len();
            rows += direct.iter().map(|(_, refs)| refs.len()).sum::<usize>();
            atoms += lower_pages_with_derived_rows(
                std::slice::from_ref(&input),
                &ParseConfig::default(),
            )
            .expect("the corpus page lowers")
            .into_iter()
            .map(|page| {
                page.property_atoms.len()
                    + page
                        .blocks
                        .iter()
                        .map(|block| block.property_atoms.len())
                        .sum::<usize>()
            })
            .sum::<usize>();
        }
        eprintln!(
            "derived_rows_agree_across_backends_over_a_real_corpus pages={pages} blocks={blocks} \
             block_path_refs_rows={rows} property_atoms_rows={atoms} \
             block_planning_rows={planning} journal_day_pages={journal_days} \
             disagreements={disagreements}"
        );
        assert!(pages > 0, "the corpus directory holds no readable pages");
        assert_eq!(disagreements, 0, "the backends disagree on a real graph");
    }

    /// I-15 cost report. Two new tables and six new indexes are new expensive
    /// primitives; this measures what they cost on a real graph rather than
    /// asserting they are cheap.
    ///
    /// "Before" is not a guess: both stores are built from the SAME lowered
    /// pages through the SAME insert path, and the before-store simply carries
    /// no `block_path_refs` or `property_atoms` rows -- which is exactly the
    /// pre-P1-a row content, index maintenance included.
    ///
    /// Opt-in: `TINE_DERIVED_COST_GRAPH=~/research/logseq-anonymized`. Prints
    /// aggregates only; no corpus content is emitted.
    #[test]
    #[ignore = "I-15 cost report over a real corpus: set TINE_DERIVED_COST_GRAPH"]
    fn derived_row_build_cost_over_a_real_corpus() {
        use std::time::Instant;

        let Some(root) = std::env::var_os("TINE_DERIVED_COST_GRAPH") else {
            eprintln!("skipped: set TINE_DERIVED_COST_GRAPH to a corpus directory");
            return;
        };
        let config = ParseConfig::default();
        let graph = crate::model::Graph::open(std::path::PathBuf::from(&root));
        let mut inputs = Vec::new();
        let mut skipped = 0usize;
        for entry in graph.list_pages() {
            let Ok(text) = std::fs::read_to_string(&entry.path) else {
                skipped += 1;
                continue;
            };
            if super::super::LogicalPageName::parse(&entry.name).is_err()
                || ManagedPath::parse(entry.rel_path.clone()).is_err()
            {
                skipped += 1;
                continue;
            }
            let document = parse_page(&entry.rel_path, &text);
            let mut input = page_input_for(&entry.name, &entry.rel_path, &document);
            input.page_id = page_id(inputs.len() as u128 + 1);
            inputs.push(input);
        }
        assert!(
            !inputs.is_empty(),
            "the corpus directory holds no usable pages"
        );

        // The producers' own CPU, measured on the same inputs the builds use.
        let producer_start = Instant::now();
        let mut produced_rows = 0usize;
        for input in &inputs {
            let flat = input
                .blocks
                .iter()
                .map(|block| crate::query::path_refs::PathRefBlock {
                    id: block.block_id,
                    parent: block.parent,
                    refs: block.path_ref_names.as_slice(),
                })
                .collect::<Vec<_>>();
            produced_rows += crate::query::derived::path_ref_rows(&input.name, &flat)
                .values()
                .map(Vec::len)
                .sum::<usize>();
            for block in &input.blocks {
                produced_rows += crate::query::derived::property_atom_rows(
                    &block
                        .properties
                        .iter()
                        .map(|property| (property.name.clone(), property.value.clone()))
                        .collect::<Vec<_>>(),
                    crate::query::atom::AtomFormat::Markdown,
                    &config,
                )
                .len();
                // This packet's own producers, on the same inputs.
                produced_rows += usize::from(
                    crate::query::derived::planning_row(
                        block.planning.as_ref().and_then(|p| p.priority.as_deref()),
                        block.planning.as_ref().and_then(|p| p.scheduled.as_deref()),
                        block.planning.as_ref().and_then(|p| p.deadline.as_deref()),
                    )
                    .is_some(),
                );
                produced_rows += crate::query::derived::tag_rows(&block.tags).len();
                produced_rows += usize::from(
                    !crate::query::derived::query_visible_columns(&block.query_visible, None)
                        .1
                        .is_empty(),
                );
            }
        }
        let producer_elapsed = producer_start.elapsed();

        const GROUP: usize = 32;
        // "Before" is the pre-P1-a/P1-a2 row content: the same lowered pages
        // through the same insert path, carrying none of the rows the two
        // packets added and paying none of their index maintenance.
        let strip = |page: &mut storage::PhysicalPage| {
            page.property_atoms.clear();
            page.journal_day = None;
            for block in &mut page.blocks {
                block.path_refs.clear();
                block.property_atoms.clear();
                block.planning = None;
                block.query_visible = String::new();
                block.query_visible_folded = String::new();
            }
        };

        // ---- genesis ----
        let mut genesis_after = Vec::new();
        for group in inputs.chunks(GROUP) {
            genesis_after.push(
                lower_terminal_chunk(
                    TerminalMaterializationChunk {
                        pages: group.to_vec(),
                        postings: Vec::new(),
                        aliases: Vec::new(),
                    },
                    &config,
                )
                .expect("the corpus lowers as a terminal chunk"),
            );
        }
        let mut genesis_before = genesis_after.clone();
        for chunk in &mut genesis_before {
            for page in &mut chunk.pages {
                strip(page);
            }
        }
        let seed = |chunks: &[storage::PhysicalTerminalMaterializationChunk]| {
            let mut connection = Connection::open_in_memory().unwrap();
            initialize_schema(&connection, ContentDigest::of(b"empty")).unwrap();
            let start = Instant::now();
            for chunk in chunks {
                let transaction = connection.transaction().unwrap();
                storage::seed_terminal_chunk_for_test(&transaction, chunk).unwrap();
                transaction.commit().unwrap();
            }
            start.elapsed()
        };
        let genesis_before_elapsed = seed(&genesis_before);
        let genesis_after_elapsed = seed(&genesis_after);

        // ---- delta ----
        let mut delta_after = Vec::new();
        for (index, group) in inputs.chunks(GROUP).enumerate() {
            let change =
                MaterializationChange::new(batch_id(index as u128 + 1), group.to_vec(), Vec::new())
                    .unwrap();
            let effect = semantic_effect_for_replacements(change.replacements());
            let physical = lower_validated_change(
                &change,
                &effect,
                None,
                &EffectValidationContext::linear(),
                &config,
            )
            .expect("the corpus lowers as a validated change");
            delta_after.push((physical, change.digest().unwrap()));
        }
        let mut delta_before = delta_after.clone();
        for (change, _) in &mut delta_before {
            for page in &mut change.replacements {
                strip(page);
            }
        }
        let apply = |changes: &[(storage::PhysicalMaterializationChange, ContentDigest)]| {
            let mut connection = Connection::open_in_memory().unwrap();
            initialize_schema(&connection, ContentDigest::of(b"empty")).unwrap();
            let start = Instant::now();
            for (index, (change, digest)) in changes.iter().enumerate() {
                let sequence = index as u64 + 1;
                let transaction = connection.transaction().unwrap();
                storage::apply_materialization_change_for_test(
                    &transaction,
                    change,
                    sequence,
                    *digest,
                    ContentDigest::of(&sequence.to_be_bytes()),
                )
                .unwrap();
                transaction.commit().unwrap();
            }
            start.elapsed()
        };
        let delta_before_elapsed = apply(&delta_before);
        let delta_after_elapsed = apply(&delta_after);

        eprintln!(
            "derived_row_build_cost_over_a_real_corpus pages={} skipped={skipped} \
             derived_rows={produced_rows} producer_cpu_ms={:.1} \
             genesis_before_ms={:.1} genesis_after_ms={:.1} \
             delta_before_ms={:.1} delta_after_ms={:.1}",
            inputs.len(),
            producer_elapsed.as_secs_f64() * 1000.0,
            genesis_before_elapsed.as_secs_f64() * 1000.0,
            genesis_after_elapsed.as_secs_f64() * 1000.0,
            delta_before_elapsed.as_secs_f64() * 1000.0,
            delta_after_elapsed.as_secs_f64() * 1000.0,
        );
    }

    /// Contract-first (AGENTS §5). The storage contract's derived-row rules and
    /// the code that implements them move together, and the load-bearing names
    /// in the prose are the ones the code actually uses: the tables exist under
    /// those names, and every config key the contract names really does move
    /// the digest the stamp records.
    #[test]
    fn the_storage_contract_names_the_derived_tables_and_the_parse_config_stamp() {
        let contract = include_str!("../../../../docs/storage-sync-contract.md");
        for phrase in [
            "`block_path_refs`",
            "`property_atoms`",
            "`materialization_stamp.parse_config_hash`",
            "de-duplicated by `atom_key`",
            "renumbered `0..n`",
            "`projection_source_revision`",
            "never migrated in place, and no\nforensic evidence is preserved",
            // F11: the config travels inside the work item, so the sentence
            // above is structurally true and not merely currently true.
            "travels **inside** each\nqueued Direct Files work item",
            // P1-a2: each of the four §5.8 objects states the question it
            // answers and why the neighbouring column cannot answer it.
            "`block_planning`",
            "never conditioned on the\ntask marker",
            "`blocks.query_visible`",
            "`blocks.query_visible_folded`",
            "`tags.tag_key` is `refs::page_key(tag)`",
            "`pages.journal_day`",
            // D-15: the seam's justification is the projection's disposability
            // and the enforcement is the engine's. If either sentence goes, the
            // reason the boundary may be widened here — and only here — is gone.
            "Raw SQL crosses that boundary; **authority does not.**",
            "The restriction is the **engine's**, not a validator's.",
            "This seam adds no refusal.",
        ] {
            assert!(
                contract.contains(phrase),
                "the storage contract no longer says: {phrase}"
            );
        }

        let connection = Connection::open_in_memory().unwrap();
        initialize_schema(&connection, ContentDigest::of(b"empty")).unwrap();
        for table in ["block_path_refs", "property_atoms", "block_planning"] {
            let found: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                found, 1,
                "the contract names a table the schema lacks: {table}"
            );
        }
        connection
            .query_row(
                "SELECT parse_config_hash FROM materialization_stamp WHERE singleton = 1",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .expect("the stamp carries the column the contract names");
        // Every column the prose above names, read back off the real schema:
        // a sentence about a column that does not exist is not a contract.
        for (table, column) in [
            ("blocks", "query_visible"),
            ("blocks", "query_visible_folded"),
            ("tags", "tag_key"),
            ("pages", "journal_day"),
            ("block_planning", "scheduled_day"),
            ("block_planning", "deadline_day"),
        ] {
            let found: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
                    params![table, column],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                found, 1,
                "the contract names a missing column: {table}.{column}"
            );
        }

        // Six config facts, six digest movements. A key named in the prose but
        // absent from `ParseConfig` would leave the rebuild rule describing
        // something that cannot happen.
        let baseline = ParseConfig::default().digest();
        for edn in [
            "{:property/separated-by-commas #{:authors}}",
            "{:ignored-page-references-keywords #{:url}}",
            "{:block-hidden-properties #{:internal}}",
            "{:journal/page-title-format \"yyyy-MM-dd\"}",
            "{:journal/file-name-format \"yyyy_MM_dd\"}",
            "{:file/name-format :triple-lowbar}",
        ] {
            assert_ne!(
                crate::config::Config::parse(edn).parse_config().digest(),
                baseline,
                "this config edit does not move the parse-config digest: {edn}"
            );
        }
    }

    /// Guard 1 (§5.8, I-19). A genesis-built store and a delta-built store of
    /// the same page hold byte-identical `block_path_refs` and
    /// `property_atoms`. The two routes reach SQLite through different lowering
    /// entry points -- `lower_terminal_chunk` and `lower_validated_change` --
    /// so agreeing here is what makes "one graph, two build paths, one
    /// behaviour" a fact rather than an intention.
    #[test]
    fn genesis_and_delta_stores_hold_byte_identical_derived_rows() {
        // `pages` and `blocks` are here for their new query columns:
        // `journal_day`, `query_visible` and `query_visible_folded` are written
        // by the two lowering entry points exactly as the derived tables are,
        // so they belong to the same proof.
        const DERIVED: [&str; 6] = [
            "pages",
            "blocks",
            "tags",
            "block_planning",
            "block_path_refs",
            "property_atoms",
        ];
        let empty_frontier = ContentDigest::of(b"empty");
        let page = parity_page_input();

        let mut delta = Connection::open_in_memory().unwrap();
        initialize_schema(&delta, empty_frontier).unwrap();
        let change =
            MaterializationChange::new(batch_id(1), vec![page.clone()], Vec::new()).unwrap();
        let input_digest = change.digest().unwrap();
        let semantic_effect = semantic_effect_for_replacements(change.replacements());
        let transaction = delta.transaction().unwrap();
        apply_change(
            &transaction,
            &change,
            &semantic_effect,
            1,
            input_digest,
            ContentDigest::of(b"after"),
        )
        .unwrap();
        transaction.commit().unwrap();

        let mut genesis = Connection::open_in_memory().unwrap();
        initialize_schema(&genesis, empty_frontier).unwrap();
        let chunk = lower_terminal_chunk(
            TerminalMaterializationChunk {
                pages: vec![page],
                postings: Vec::new(),
                aliases: Vec::new(),
            },
            &ParseConfig::default(),
        )
        .unwrap();
        let transaction = genesis.transaction().unwrap();
        storage::seed_terminal_chunk_for_test(&transaction, &chunk).unwrap();
        transaction.commit().unwrap();

        let empty = Connection::open_in_memory().unwrap();
        initialize_schema(&empty, empty_frontier).unwrap();

        let derived_digests = |connection: &Connection| {
            storage::materialization_row_digests_by_table_for_test(connection)
                .unwrap()
                .into_iter()
                .filter(|(table, _)| DERIVED.contains(table))
                .collect::<Vec<_>>()
        };
        let genesis_digests = derived_digests(&genesis);
        let delta_digests = derived_digests(&delta);
        let empty_digests = derived_digests(&empty);

        // Two empty tables have equal digests, so state that the fixture
        // actually populated both before comparing them.
        assert_eq!(
            genesis_digests.len(),
            DERIVED.len(),
            "every derived table is digested"
        );
        assert_ne!(
            genesis_digests, empty_digests,
            "the fixture must populate the derived tables it compares"
        );
        for ((table, genesis), (_, empty)) in genesis_digests.iter().zip(empty_digests.iter()) {
            assert_ne!(genesis, empty, "{table} is empty in the genesis store");
        }

        assert_eq!(
            genesis_digests, delta_digests,
            "the genesis and delta stores disagree on the derived rows"
        );
    }

    /// Guard 3 (§5.8 flattening). Per source row in source-ordinal order,
    /// concatenated, de-duplicated by `atom_key` (first wins), renumbered
    /// `0..n` -- asserted on the rows the producer actually emits.
    #[test]
    fn property_atom_rows_flatten_concatenate_dedupe_and_renumber() {
        let properties = vec![
            ("k".to_owned(), "a".to_owned()),
            ("k".to_owned(), "a".to_owned()),
            ("K".to_owned(), "b".to_owned()),
            ("k".to_owned(), "a, c".to_owned()),
        ];
        let rows = crate::query::derived::property_atom_rows(
            &properties,
            crate::query::atom::AtomFormat::Markdown,
            &ParseConfig::default(),
        );
        let atoms = rows
            .iter()
            .filter(|row| row.normalized_name == "k")
            .map(|row| (row.ordinal, row.atom.clone(), row.atom_key.clone()))
            .collect::<Vec<_>>();
        // Four source rows in source-ordinal order yield `a`, `a`, `b`, then
        // `a` and `c`. Concatenated that is [a, a, b, a, c]; de-duplicated by
        // `atom_key` with first-wins it is [a, b, c]; renumbered it is 0..2.
        // The atomizer's own comma handling is what turns the fourth row into
        // two atoms -- the fixture pins that, it does not legislate it.
        assert_eq!(
            atoms,
            vec![
                (0, "a".to_owned(), "a".to_owned()),
                (1, "b".to_owned(), "b".to_owned()),
                (2, "c".to_owned(), "c".to_owned()),
            ],
            "atoms must concatenate in source order, drop repeats and renumber from zero"
        );
        // Renumbering is `0..n` over the surviving atoms, not the source rows:
        // a gap would leave the primary key's ordinal meaning "which source row"
        // instead of "which atom".
        assert!(
            rows.iter()
                .filter(|row| row.normalized_name == "k")
                .enumerate()
                .all(|(index, row)| u32::try_from(index) == Ok(row.ordinal)),
            "surviving atoms are renumbered contiguously"
        );
    }

    /// The de-duplication key is `atom_key`, not the atom text: two source rows
    /// that differ only in case are one atom, and the FIRST spelling is the one
    /// that survives.
    #[test]
    fn property_atom_dedupe_is_by_atom_key_and_keeps_the_first_spelling() {
        let properties = vec![
            ("k".to_owned(), "Alpha".to_owned()),
            ("k".to_owned(), "alpha".to_owned()),
        ];
        let rows = crate::query::derived::property_atom_rows(
            &properties,
            crate::query::atom::AtomFormat::Markdown,
            &ParseConfig::default(),
        );
        let atoms = rows
            .iter()
            .filter(|row| row.normalized_name == "k")
            .map(|row| (row.ordinal, row.atom.clone(), row.atom_key.clone()))
            .collect::<Vec<_>>();
        assert_eq!(atoms, vec![(0, "Alpha".to_owned(), "alpha".to_owned())]);
    }
}
