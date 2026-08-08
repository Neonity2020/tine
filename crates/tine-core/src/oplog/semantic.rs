use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use super::{BlockId, DocumentId, LogseqUuid, ManagedPath, ManagedTextKind, PageId};

pub const SEMANTIC_EFFECT_SCHEMA_VERSION: u32 = 5;
pub const CATALOG_PAGE_STATE_SCHEMA_VERSION: u32 = 2;
pub const MAX_SEMANTIC_EFFECT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_SEMANTIC_DELTA_ENTRIES: usize = 100_000;
pub const MAX_BLOCK_CONTENT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_PAGE_PREAMBLE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_LOGICAL_PAGE_NAME_BYTES: usize = 4 * 1024 * 1024;
pub const PAGE_NAME_KEY_VERSION: u32 = 1;
const SEMANTIC_MAGIC: &[u8; 8] = b"TINESEM1";

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct LogicalPageName(String);

impl LogicalPageName {
    pub fn parse(value: impl Into<String>) -> Result<Self, LogicalPageNameError> {
        let value = value.into();
        validate_logical_page_name(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn canonical_key(&self) -> String {
        canonical_page_name_key(&self.0)
    }

    pub fn key_digest(&self) -> PageNameKeyDigest {
        PageNameKeyDigest::of_canonical_key(&self.canonical_key())
    }
}

/// Transparent authenticated-index key for page-name key version 1.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PageNameKeyDigest([u8; 32]);

impl PageNameKeyDigest {
    fn of_canonical_key(canonical_key: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"tine/page-name-key/v1\0");
        hasher.update(PAGE_NAME_KEY_VERSION.to_be_bytes());
        hasher.update((canonical_key.len() as u64).to_be_bytes());
        hasher.update(canonical_key.as_bytes());
        Self(hasher.finalize().into())
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for PageNameKeyDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl AsRef<str> for LogicalPageName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for LogicalPageName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for LogicalPageName {
    type Err = LogicalPageNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for LogicalPageName {
    type Error = LogicalPageNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for LogicalPageName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogicalPageNameError {
    ForbiddenControl,
    EmptyCanonicalKey,
    RawTooLarge(usize),
    CanonicalTooLarge(usize),
}

impl fmt::Display for LogicalPageNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForbiddenControl => {
                formatter.write_str("logical page name contains NUL, CR, or LF")
            }
            Self::EmptyCanonicalKey => {
                formatter.write_str("logical page name has an empty canonical key")
            }
            Self::RawTooLarge(bytes) => {
                write!(
                    formatter,
                    "logical page name is too large: {bytes} raw bytes"
                )
            }
            Self::CanonicalTooLarge(bytes) => write!(
                formatter,
                "logical page name canonical key is too large: {bytes} bytes"
            ),
        }
    }
}

impl std::error::Error for LogicalPageNameError {}

fn validate_logical_page_name(value: &str) -> Result<(), LogicalPageNameError> {
    if value.len() > MAX_LOGICAL_PAGE_NAME_BYTES {
        return Err(LogicalPageNameError::RawTooLarge(value.len()));
    }
    if value
        .chars()
        .any(|character| matches!(character, '\0' | '\r' | '\n'))
    {
        return Err(LogicalPageNameError::ForbiddenControl);
    }
    let canonical = canonical_page_name_key(value);
    if canonical.is_empty() {
        return Err(LogicalPageNameError::EmptyCanonicalKey);
    }
    if canonical.len() > MAX_LOGICAL_PAGE_NAME_BYTES {
        return Err(LogicalPageNameError::CanonicalTooLarge(canonical.len()));
    }
    Ok(())
}

fn canonical_page_name_key(value: &str) -> String {
    let lowered: String = value.trim().chars().flat_map(char::to_lowercase).collect();
    let without_leading = lowered.strip_prefix('/').unwrap_or(&lowered);
    without_leading
        .strip_suffix('/')
        .unwrap_or(without_leading)
        .nfc()
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageState {
    Live {
        name: LogicalPageName,
        path: ManagedPath,
        home_document_id: DocumentId,
        kind: ManagedTextKind,
    },
    Tombstone {
        name: LogicalPageName,
        home_document_id: DocumentId,
        kind: ManagedTextKind,
    },
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct PageStateWireRef<'a> {
    catalog_page_state_schema_version: u32,
    state: PageStateDataRef<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum PageStateDataRef<'a> {
    Live {
        name: &'a LogicalPageName,
        path: &'a ManagedPath,
        home_document_id: DocumentId,
        kind: ManagedTextKind,
    },
    Tombstone {
        name: &'a LogicalPageName,
        home_document_id: DocumentId,
        kind: ManagedTextKind,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageStateWire {
    catalog_page_state_schema_version: u32,
    state: PageStateData,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum PageStateData {
    Live {
        name: LogicalPageName,
        path: ManagedPath,
        home_document_id: DocumentId,
        kind: ManagedTextKind,
    },
    Tombstone {
        name: LogicalPageName,
        home_document_id: DocumentId,
        kind: ManagedTextKind,
    },
}

impl Serialize for PageState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let state = match self {
            Self::Live {
                name,
                path,
                home_document_id,
                kind,
            } => PageStateDataRef::Live {
                name,
                path,
                home_document_id: *home_document_id,
                kind: *kind,
            },
            Self::Tombstone {
                name,
                home_document_id,
                kind,
            } => PageStateDataRef::Tombstone {
                name,
                home_document_id: *home_document_id,
                kind: *kind,
            },
        };
        PageStateWireRef {
            catalog_page_state_schema_version: CATALOG_PAGE_STATE_SCHEMA_VERSION,
            state,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PageState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = PageStateWire::deserialize(deserializer)?;
        if wire.catalog_page_state_schema_version != CATALOG_PAGE_STATE_SCHEMA_VERSION {
            return Err(serde::de::Error::custom(format!(
                "unknown catalog page-state schema {}; expected {}",
                wire.catalog_page_state_schema_version, CATALOG_PAGE_STATE_SCHEMA_VERSION
            )));
        }
        Ok(match wire.state {
            PageStateData::Live {
                name,
                path,
                home_document_id,
                kind,
            } => Self::Live {
                name,
                path,
                home_document_id,
                kind,
            },
            PageStateData::Tombstone {
                name,
                home_document_id,
                kind,
            } => Self::Tombstone {
                name,
                home_document_id,
                kind,
            },
        })
    }
}

impl PageState {
    pub const fn name(&self) -> &LogicalPageName {
        match self {
            Self::Live { name, .. } | Self::Tombstone { name, .. } => name,
        }
    }

    pub const fn home_document_id(&self) -> DocumentId {
        match self {
            Self::Live {
                home_document_id, ..
            }
            | Self::Tombstone {
                home_document_id, ..
            } => *home_document_id,
        }
    }

    pub const fn kind(&self) -> ManagedTextKind {
        match self {
            Self::Live { kind, .. } | Self::Tombstone { kind, .. } => *kind,
        }
    }

    pub fn path(&self) -> Option<&ManagedPath> {
        match self {
            Self::Live { path, .. } => Some(path),
            Self::Tombstone { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockOwner {
    Page(PageId),
    Tombstone,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyGeneratedAnchorReason {
    BlockReference,
    BlockEmbed,
    Export,
    CopiedDeepLink,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum LogseqIdentityOrigin {
    ExternalImported,
    PolicyGenerated { reason: PolicyGeneratedAnchorReason },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockState {
    pub block_id: BlockId,
    pub home_document_id: DocumentId,
    pub owner: BlockOwner,
    pub logseq_uuid: Option<LogseqUuid>,
    pub logseq_identity_origin: Option<LogseqIdentityOrigin>,
    pub content: String,
}

impl BlockState {
    pub const fn policy_generated_logseq_uuid(&self) -> Option<LogseqUuid> {
        match (self.logseq_uuid, self.logseq_identity_origin) {
            (Some(uuid), Some(LogseqIdentityOrigin::PolicyGenerated { .. })) => Some(uuid),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PagePreambleState {
    pub page_id: PageId,
    pub home_document_id: DocumentId,
    pub preamble: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipClaim {
    pub home_document_id: DocumentId,
    pub parent: Option<BlockId>,
    pub order: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisibleMembership {
    pub page_id: PageId,
    pub block_id: BlockId,
    pub home_document_id: DocumentId,
    pub parent: Option<BlockId>,
    pub order: String,
}

impl MembershipClaim {
    pub fn new(
        home_document_id: DocumentId,
        parent: Option<BlockId>,
        order: impl Into<String>,
    ) -> Result<Self, SemanticError> {
        let claim = Self {
            home_document_id,
            parent,
            order: order.into(),
        };
        claim.validate()?;
        Ok(claim)
    }

    pub(crate) fn validate(&self) -> Result<(), SemanticError> {
        if self.order.is_empty()
            || self.order.len() > 512
            || self.order.chars().any(char::is_control)
        {
            return Err(SemanticError::InvalidOrderKey);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageDelta {
    pub page_id: PageId,
    pub before: Option<PageState>,
    pub after: Option<PageState>,
}

impl PageDelta {
    fn validate_lifecycle(&self) -> Result<(), SemanticError> {
        if self.before == self.after {
            return Err(SemanticError::UnchangedDelta);
        }
        if let (Some(before), Some(after)) = (&self.before, &self.after) {
            if before.home_document_id() != after.home_document_id() {
                return Err(SemanticError::HomeShardChanged);
            }
        }

        match (&self.before, &self.after) {
            (None, Some(PageState::Live { .. }))
            | (Some(PageState::Live { .. }), Some(PageState::Live { .. })) => Ok(()),
            (
                Some(PageState::Live {
                    name: before_name,
                    kind: before_kind,
                    ..
                }),
                Some(PageState::Tombstone {
                    name: after_name,
                    kind: after_kind,
                    ..
                }),
            ) if before_name == after_name && before_kind == after_kind => Ok(()),
            _ => Err(SemanticError::InvalidPageLifecycle),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PagePreambleDelta {
    pub page_id: PageId,
    pub home_document_id: DocumentId,
    pub before: Option<PagePreambleState>,
    pub after: Option<PagePreambleState>,
}

/// The authenticated explicit-title post-state selected for one page.
///
/// `Absent` is deliberately distinct from a missing optional delta: the
/// selected immutable effect was inspected and proved to carry no explicit
/// title after the selected transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EffectiveExplicitTitleState {
    Present(PagePreambleDelta),
    Absent {
        page_id: PageId,
        home_document_id: DocumentId,
    },
}

impl EffectiveExplicitTitleState {
    pub(crate) fn from_authenticated_transition(
        page_id: PageId,
        home_document_id: DocumentId,
        path: &ManagedPath,
        selected_name: &LogicalPageName,
        selected_kind: ManagedTextKind,
        transition: Option<PagePreambleDelta>,
    ) -> Result<Self, SemanticError> {
        let Some(transition) = transition else {
            return Ok(Self::Absent {
                page_id,
                home_document_id,
            });
        };
        if transition.page_id != page_id
            || transition.home_document_id != home_document_id
            || transition.after.as_ref().is_none_or(|after| {
                after.page_id != page_id || after.home_document_id != home_document_id
            })
        {
            return Err(SemanticError::InvalidEffectiveTitleSplice);
        }
        let after = transition
            .after
            .as_ref()
            .expect("authenticated transition has an after state");
        // Journal preambles retain the configured authored date while the
        // authenticated PageState carries the parser-normalized logical name.
        match explicit_title_line(after.preamble.as_deref(), path.is_org()) {
            Some((_, _, title))
                if selected_kind == ManagedTextKind::Journal || title == selected_name.as_str() =>
            {
                Ok(Self::Present(transition))
            }
            None => Ok(Self::Absent {
                page_id,
                home_document_id,
            }),
            Some(_) => Err(SemanticError::InvalidEffectiveTitleSplice),
        }
    }

    pub(crate) const fn page_id(&self) -> PageId {
        match self {
            Self::Present(delta) => delta.page_id,
            Self::Absent { page_id, .. } => *page_id,
        }
    }

    pub(crate) const fn home_document_id(&self) -> DocumentId {
        match self {
            Self::Present(delta) => delta.home_document_id,
            Self::Absent {
                home_document_id, ..
            } => *home_document_id,
        }
    }

    pub(crate) fn apply_to_preamble(
        &self,
        path: &ManagedPath,
        preamble: &mut Option<String>,
    ) -> Result<(), SemanticError> {
        let selected = match self {
            Self::Present(delta) => delta
                .after
                .as_ref()
                .and_then(|after| after.preamble.as_deref()),
            Self::Absent { .. } => None,
        };
        *preamble = splice_explicit_title(preamble.as_deref(), selected, path.is_org())?;
        Ok(())
    }
}

fn explicit_title_line(preamble: Option<&str>, is_org: bool) -> Option<(usize, usize, String)> {
    let preamble = preamble?;
    let mut offset = 0;
    for chunk in preamble.split_inclusive('\n') {
        let line = chunk.strip_suffix('\n').unwrap_or(chunk);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let value = crate::doc::parse_property_line(line)
            .and_then(|(key, value)| key.eq_ignore_ascii_case("title").then_some(value))
            .or_else(|| {
                is_org.then(|| {
                    let trimmed = line.trim();
                    trimmed
                        .split_once(':')
                        .and_then(|(key, value)| {
                            key.eq_ignore_ascii_case("#+title").then_some(value)
                        })
                        .or_else(|| {
                            trimmed.strip_prefix(':').and_then(|rest| {
                                rest.split_once(':').and_then(|(key, value)| {
                                    key.eq_ignore_ascii_case("title").then_some(value)
                                })
                            })
                        })
                        .map(str::to_owned)
                })?
            });
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            return Some((offset, offset + line.len(), value.trim().to_owned()));
        }
        offset += chunk.len();
    }
    None
}

fn splice_explicit_title(
    current: Option<&str>,
    selected: Option<&str>,
    is_org: bool,
) -> Result<Option<String>, SemanticError> {
    let selected_line = match selected {
        Some(selected) => {
            let (start, end, _) = explicit_title_line(Some(selected), is_org)
                .ok_or(SemanticError::InvalidEffectiveTitleSplice)?;
            Some(&selected[start..end])
        }
        None => None,
    };
    let Some(current) = current else {
        return Ok(selected_line.map(str::to_owned));
    };
    let current_title = explicit_title_line(Some(current), is_org);
    match (current_title, selected_line) {
        (Some((start, end, _)), Some(selected_line)) => {
            let mut result =
                String::with_capacity(current.len() - (end - start) + selected_line.len());
            result.push_str(&current[..start]);
            result.push_str(selected_line);
            result.push_str(&current[end..]);
            Ok(Some(result))
        }
        (Some((mut start, mut end, _)), None) => {
            if current[end..].starts_with('\n') {
                end += 1;
            } else if start > 0 && current[..start].ends_with('\n') {
                start -= 1;
            }
            let mut result = String::with_capacity(current.len() - (end - start));
            result.push_str(&current[..start]);
            result.push_str(&current[end..]);
            Ok((!result.is_empty()).then_some(result))
        }
        (None, Some(selected_line)) => {
            let mut result = String::with_capacity(selected_line.len() + 1 + current.len());
            result.push_str(selected_line);
            if !current.is_empty() {
                result.push('\n');
                result.push_str(current);
            }
            Ok(Some(result))
        }
        (None, None) => Ok(Some(current.to_owned())),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockDelta {
    pub block_id: BlockId,
    pub home_document_id: DocumentId,
    pub before: Option<BlockState>,
    pub after: Option<BlockState>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipDelta {
    pub page_id: PageId,
    pub block_id: BlockId,
    pub before: Option<MembershipClaim>,
    pub after: Option<MembershipClaim>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SemanticEffect {
    semantic_effect_schema_version: u32,
    pages: Vec<PageDelta>,
    page_preambles: Vec<PagePreambleDelta>,
    blocks: Vec<BlockDelta>,
    memberships: Vec<MembershipDelta>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SemanticEffectWire {
    semantic_effect_schema_version: u32,
    pages: Vec<PageDelta>,
    page_preambles: Vec<PagePreambleDelta>,
    blocks: Vec<BlockDelta>,
    memberships: Vec<MembershipDelta>,
}

impl SemanticEffect {
    pub fn new(
        pages: Vec<PageDelta>,
        blocks: Vec<BlockDelta>,
        memberships: Vec<MembershipDelta>,
    ) -> Result<Self, SemanticError> {
        Self::new_with_page_preambles(pages, Vec::new(), blocks, memberships)
    }

    pub fn new_with_page_preambles(
        mut pages: Vec<PageDelta>,
        mut page_preambles: Vec<PagePreambleDelta>,
        mut blocks: Vec<BlockDelta>,
        mut memberships: Vec<MembershipDelta>,
    ) -> Result<Self, SemanticError> {
        pages.sort_unstable_by_key(|delta| delta.page_id);
        page_preambles.sort_unstable_by_key(|delta| (delta.home_document_id, delta.page_id));
        blocks.sort_unstable_by_key(|delta| (delta.home_document_id, delta.block_id));
        memberships.sort_unstable_by_key(|delta| (delta.page_id, delta.block_id));
        let effect = Self {
            semantic_effect_schema_version: SEMANTIC_EFFECT_SCHEMA_VERSION,
            pages,
            page_preambles,
            blocks,
            memberships,
        };
        effect.validate()?;
        Ok(effect)
    }

    pub fn pages(&self) -> &[PageDelta] {
        &self.pages
    }

    pub fn page_preambles(&self) -> &[PagePreambleDelta] {
        &self.page_preambles
    }

    pub fn blocks(&self) -> &[BlockDelta] {
        &self.blocks
    }

    pub fn memberships(&self) -> &[MembershipDelta] {
        &self.memberships
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
            && self.page_preambles.is_empty()
            && self.blocks.is_empty()
            && self.memberships.is_empty()
    }

    /// Build the transient effective view for one authenticated exact-title
    /// selection. Only title-bearing post-state is replaced; every other page,
    /// path, kind, home, block, and membership facet remains authored.
    pub(crate) fn splice_title_post_state(
        &self,
        page_id: PageId,
        selected_name: &LogicalPageName,
        selected_explicit_title: &EffectiveExplicitTitleState,
    ) -> Result<Self, SemanticError> {
        let mut pages = self.pages.clone();
        let page_index = pages
            .binary_search_by_key(&page_id, |delta| delta.page_id)
            .map_err(|_| SemanticError::InvalidEffectiveTitleSplice)?;
        let selected_key = selected_name.key_digest();
        let (home_document_id, path) = match pages[page_index].after.as_ref() {
            Some(PageState::Live {
                home_document_id,
                path,
                ..
            }) => (*home_document_id, path.clone()),
            Some(PageState::Tombstone { .. }) | None => {
                return Err(SemanticError::InvalidEffectiveTitleSplice);
            }
        };
        let after = pages[page_index]
            .after
            .as_mut()
            .ok_or(SemanticError::InvalidEffectiveTitleSplice)?;
        match after {
            PageState::Live { name, .. } if name.key_digest() == selected_key => {
                *name = selected_name.clone();
            }
            PageState::Live { .. } | PageState::Tombstone { .. } => {
                return Err(SemanticError::InvalidEffectiveTitleSplice);
            }
        }
        if pages[page_index].before == pages[page_index].after {
            pages.remove(page_index);
        }

        if selected_explicit_title.page_id() != page_id
            || selected_explicit_title.home_document_id() != home_document_id
        {
            return Err(SemanticError::InvalidEffectiveTitleSplice);
        }
        let mut page_preambles = self.page_preambles.clone();
        let current = page_preambles.binary_search_by_key(&(home_document_id, page_id), |delta| {
            (delta.home_document_id, delta.page_id)
        });
        match (current, selected_explicit_title) {
            (Ok(index), selected) => {
                let after = page_preambles[index]
                    .after
                    .as_mut()
                    .ok_or(SemanticError::InvalidEffectiveTitleSplice)?;
                selected.apply_to_preamble(&path, &mut after.preamble)?;
                if page_preambles[index].before == page_preambles[index].after {
                    page_preambles.remove(index);
                }
            }
            (Err(_), EffectiveExplicitTitleState::Present(selected)) => {
                let mut inserted = selected.clone();
                let before = inserted
                    .before
                    .as_ref()
                    .ok_or(SemanticError::InvalidEffectiveTitleSplice)?;
                let mut after = before.clone();
                selected_explicit_title.apply_to_preamble(&path, &mut after.preamble)?;
                inserted.after = Some(after);
                if inserted.before != inserted.after {
                    page_preambles.push(inserted);
                }
            }
            (Err(_), EffectiveExplicitTitleState::Absent { .. }) => {}
        }
        Self::new_with_page_preambles(
            pages,
            page_preambles,
            self.blocks.clone(),
            self.memberships.clone(),
        )
    }

    pub fn encode(&self) -> Result<Vec<u8>, SemanticError> {
        self.validate()?;
        self.encode_validated()
    }

    fn encode_validated(&self) -> Result<Vec<u8>, SemanticError> {
        let body = postcard::to_allocvec(self)
            .map_err(|error| SemanticError::Encode(error.to_string()))?;
        // A semantic effect is one object per batch and can reach 64 MiB, four
        // times what the tail will admit -- so its size is load-bearing. Each
        // delta carries both `before` and `after`; on an initial import nothing
        // exists beforehand, so a populated `before` means the content is
        // encoded twice for no purpose. Measure rather than assume.
        if std::env::var_os("TINE_SEMANTIC_TRACE").is_some() {
            let before_pages = self.pages.iter().filter(|d| d.before.is_some()).count();
            let before_blocks = self.blocks.iter().filter(|d| d.before.is_some()).count();
            let before_members = self
                .memberships
                .iter()
                .filter(|d| d.before.is_some())
                .count();
            eprintln!(
                "SEMANTIC EFFECT: {}B | pages={} ({} with before) blocks={} ({} with before) \
                 memberships={} ({} with before) preambles={}",
                body.len() + SEMANTIC_MAGIC.len(),
                self.pages.len(),
                before_pages,
                self.blocks.len(),
                before_blocks,
                self.memberships.len(),
                before_members,
                self.page_preambles.len(),
            );
        }
        let mut bytes = Vec::with_capacity(SEMANTIC_MAGIC.len() + body.len());
        bytes.extend_from_slice(SEMANTIC_MAGIC);
        bytes.extend_from_slice(&body);
        if bytes.len() > MAX_SEMANTIC_EFFECT_BYTES {
            return Err(SemanticError::TooLarge(bytes.len()));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, SemanticError> {
        if bytes.len() > MAX_SEMANTIC_EFFECT_BYTES {
            return Err(SemanticError::TooLarge(bytes.len()));
        }
        let Some(body) = bytes.strip_prefix(SEMANTIC_MAGIC) else {
            return Err(SemanticError::Decode(
                "invalid semantic effect magic".into(),
            ));
        };
        let wire: SemanticEffectWire =
            postcard::from_bytes(body).map_err(|error| SemanticError::Decode(error.to_string()))?;
        let effect = Self {
            semantic_effect_schema_version: wire.semantic_effect_schema_version,
            pages: wire.pages,
            page_preambles: wire.page_preambles,
            blocks: wire.blocks,
            memberships: wire.memberships,
        };
        effect.validate()?;
        if effect.encode_validated()?.as_slice() != bytes {
            return Err(SemanticError::NonCanonical);
        }
        Ok(effect)
    }

    fn validate(&self) -> Result<(), SemanticError> {
        if self.semantic_effect_schema_version != SEMANTIC_EFFECT_SCHEMA_VERSION {
            return Err(SemanticError::UnknownVersion(
                self.semantic_effect_schema_version,
            ));
        }
        let entries = self
            .pages
            .len()
            .checked_add(self.page_preambles.len())
            .and_then(|value| value.checked_add(self.blocks.len()))
            .and_then(|value| value.checked_add(self.memberships.len()))
            .ok_or(SemanticError::TooManyEntries(usize::MAX))?;
        if entries > MAX_SEMANTIC_DELTA_ENTRIES {
            return Err(SemanticError::TooManyEntries(entries));
        }
        if !strictly_sorted_by(&self.pages, |delta| delta.page_id)
            || !strictly_sorted_by(&self.page_preambles, |delta| {
                (delta.home_document_id, delta.page_id)
            })
            || !strictly_sorted_by(&self.blocks, |delta| {
                (delta.home_document_id, delta.block_id)
            })
            || !strictly_sorted_by(&self.memberships, |delta| (delta.page_id, delta.block_id))
        {
            return Err(SemanticError::NonCanonical);
        }
        for delta in &self.pages {
            delta.validate_lifecycle()?;
        }
        for delta in &self.page_preambles {
            if delta.before == delta.after {
                return Err(SemanticError::UnchangedDelta);
            }
            if delta.before.is_some() && delta.after.is_none() {
                return Err(SemanticError::PagePreambleStateRemoved);
            }
            for state in [&delta.before, &delta.after].into_iter().flatten() {
                if state.page_id != delta.page_id
                    || state.home_document_id != delta.home_document_id
                {
                    return Err(SemanticError::HomeShardChanged);
                }
                if let Some(preamble) = &state.preamble {
                    if preamble.len() > MAX_PAGE_PREAMBLE_BYTES {
                        return Err(SemanticError::PagePreambleTooLarge(preamble.len()));
                    }
                }
            }
        }
        for delta in &self.blocks {
            if delta.before == delta.after {
                return Err(SemanticError::UnchangedDelta);
            }
            if delta.before.is_some() && delta.after.is_none() {
                return Err(SemanticError::BlockStateRemoved);
            }
            for state in [&delta.before, &delta.after].into_iter().flatten() {
                if state.block_id != delta.block_id
                    || state.home_document_id != delta.home_document_id
                {
                    return Err(SemanticError::HomeShardChanged);
                }
                if state.content.len() > MAX_BLOCK_CONTENT_BYTES {
                    return Err(SemanticError::ContentTooLarge(state.content.len()));
                }
                if state.logseq_uuid.is_some() != state.logseq_identity_origin.is_some() {
                    return Err(SemanticError::InvalidLogseqIdentityState);
                }
            }
        }
        for delta in &self.memberships {
            if delta.before == delta.after {
                return Err(SemanticError::UnchangedDelta);
            }
            for claim in [&delta.before, &delta.after].into_iter().flatten() {
                claim.validate()?;
            }
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for SemanticEffect {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = SemanticEffectWire::deserialize(deserializer)?;
        let effect = Self {
            semantic_effect_schema_version: wire.semantic_effect_schema_version,
            pages: wire.pages,
            page_preambles: wire.page_preambles,
            blocks: wire.blocks,
            memberships: wire.memberships,
        };
        effect.validate().map_err(serde::de::Error::custom)?;
        Ok(effect)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalSnapshot {
    pub pages: Vec<(PageId, PageState)>,
    pub page_preambles: Vec<PagePreambleState>,
    pub blocks: Vec<BlockState>,
    pub memberships: Vec<VisibleMembership>,
    pub path_conflicts: Vec<(ManagedPath, Vec<PageId>)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SemanticError {
    Decode(String),
    Encode(String),
    UnknownVersion(u32),
    TooLarge(usize),
    TooManyEntries(usize),
    ContentTooLarge(usize),
    PagePreambleTooLarge(usize),
    InvalidOrderKey,
    InvalidLogseqIdentityState,
    InvalidEffectiveTitleSplice,
    NonCanonical,
    UnchangedDelta,
    InvalidPageLifecycle,
    BlockStateRemoved,
    PagePreambleStateRemoved,
    HomeShardChanged,
}

impl fmt::Display for SemanticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "semantic effect decode failed: {error}"),
            Self::Encode(error) => write!(f, "semantic effect encode failed: {error}"),
            Self::UnknownVersion(found) => write!(
                f,
                "unknown semantic effect schema {found}; expected {SEMANTIC_EFFECT_SCHEMA_VERSION}"
            ),
            Self::TooLarge(bytes) => write!(f, "semantic effect is too large: {bytes} bytes"),
            Self::TooManyEntries(entries) => {
                write!(f, "semantic effect has too many entries: {entries}")
            }
            Self::ContentTooLarge(bytes) => write!(f, "block content is too large: {bytes} bytes"),
            Self::PagePreambleTooLarge(bytes) => {
                write!(f, "page preamble is too large: {bytes} bytes")
            }
            Self::InvalidOrderKey => f.write_str("invalid membership order key"),
            Self::InvalidLogseqIdentityState => {
                f.write_str("Logseq UUID and identity origin must be present together")
            }
            Self::InvalidEffectiveTitleSplice => {
                f.write_str("authenticated effective title splice is inconsistent")
            }
            Self::NonCanonical => f.write_str("semantic effect is not canonically ordered/encoded"),
            Self::UnchangedDelta => f.write_str("semantic effect contains an unchanged delta"),
            Self::InvalidPageLifecycle => f.write_str(
                "invalid page lifecycle transition: creation must be None -> Live; edits must be Live -> Live; deletion must be Live -> same-kind Tombstone",
            ),
            Self::BlockStateRemoved => {
                f.write_str("authoritative block state cannot be physically removed")
            }
            Self::PagePreambleStateRemoved => {
                f.write_str("authoritative page preamble state cannot be physically removed")
            }
            Self::HomeShardChanged => f.write_str("stable home shard identity changed"),
        }
    }
}

impl std::error::Error for SemanticError {}

fn strictly_sorted_by<T, K: Ord>(values: &[T], key: impl Fn(&T) -> K) -> bool {
    values.windows(2).all(|pair| key(&pair[0]) < key(&pair[1]))
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn page_id(value: u128) -> PageId {
        PageId::from_uuid(Uuid::from_u128(value))
    }

    fn document_id(value: u128) -> DocumentId {
        DocumentId::from_uuid(Uuid::from_u128(value))
    }

    fn live(kind: ManagedTextKind) -> PageState {
        live_at("shared/name.md", document_id(2), kind)
    }

    fn live_at(path: &str, home_document_id: DocumentId, kind: ManagedTextKind) -> PageState {
        PageState::Live {
            name: LogicalPageName::parse("Shared Name").unwrap(),
            path: ManagedPath::parse(path).unwrap(),
            home_document_id,
            kind,
        }
    }

    fn tombstone(home_document_id: DocumentId, kind: ManagedTextKind) -> PageState {
        PageState::Tombstone {
            name: LogicalPageName::parse("Shared Name").unwrap(),
            home_document_id,
            kind,
        }
    }

    fn page_effect(
        before: Option<PageState>,
        after: Option<PageState>,
    ) -> Result<SemanticEffect, SemanticError> {
        SemanticEffect::new(
            vec![PageDelta {
                page_id: page_id(1),
                before,
                after,
            }],
            Vec::new(),
            Vec::new(),
        )
    }

    fn effect(kind: ManagedTextKind) -> SemanticEffect {
        page_effect(None, Some(live(kind))).unwrap()
    }

    #[test]
    fn authenticated_title_splice_changes_only_title_bearing_post_state() {
        let target = page_id(1);
        let unrelated = page_id(2);
        let deleted = page_id(5);
        let home = document_id(2);
        let block_id = BlockId::from_uuid(Uuid::from_u128(3));
        let before_target = PageState::Live {
            name: LogicalPageName::parse("Original").unwrap(),
            path: ManagedPath::parse("pages/physical.md").unwrap(),
            home_document_id: home,
            kind: ManagedTextKind::Page,
        };
        let authored_target = PageState::Live {
            name: LogicalPageName::parse("Cafe\u{301} Plan").unwrap(),
            path: ManagedPath::parse("pages/physical.md").unwrap(),
            home_document_id: home,
            kind: ManagedTextKind::Page,
        };
        let unrelated_after = PageState::Live {
            name: LogicalPageName::parse("Unrelated").unwrap(),
            path: ManagedPath::parse("pages/unrelated.md").unwrap(),
            home_document_id: document_id(4),
            kind: ManagedTextKind::Page,
        };
        let deleted_before = PageState::Live {
            name: LogicalPageName::parse("Deleted Unrelated").unwrap(),
            path: ManagedPath::parse("pages/deleted-unrelated.md").unwrap(),
            home_document_id: document_id(6),
            kind: ManagedTextKind::Page,
        };
        let deleted_after = PageState::Tombstone {
            name: LogicalPageName::parse("Deleted Unrelated").unwrap(),
            home_document_id: document_id(6),
            kind: ManagedTextKind::Page,
        };
        let block_before = BlockState {
            block_id,
            home_document_id: home,
            owner: BlockOwner::Page(target),
            logseq_uuid: None,
            logseq_identity_origin: None,
            content: "before".into(),
        };
        let block_after = BlockState {
            content: "after".into(),
            ..block_before.clone()
        };
        let membership_before = MembershipClaim::new(home, None, "a").unwrap();
        let membership_after = MembershipClaim::new(home, None, "b").unwrap();
        let authored = SemanticEffect::new_with_page_preambles(
            vec![
                PageDelta {
                    page_id: target,
                    before: Some(before_target),
                    after: Some(authored_target),
                },
                PageDelta {
                    page_id: unrelated,
                    before: None,
                    after: Some(unrelated_after),
                },
                PageDelta {
                    page_id: deleted,
                    before: Some(deleted_before),
                    after: Some(deleted_after),
                },
            ],
            vec![PagePreambleDelta {
                page_id: target,
                home_document_id: home,
                before: Some(PagePreambleState {
                    page_id: target,
                    home_document_id: home,
                    preamble: None,
                }),
                after: Some(PagePreambleState {
                    page_id: target,
                    home_document_id: home,
                    preamble: Some("title:: Cafe\u{301} Plan".into()),
                }),
            }],
            vec![BlockDelta {
                block_id,
                home_document_id: home,
                before: Some(block_before),
                after: Some(block_after),
            }],
            vec![MembershipDelta {
                page_id: target,
                block_id,
                before: Some(membership_before),
                after: Some(membership_after),
            }],
        )
        .unwrap();
        let selected_name = LogicalPageName::parse("Café Plan").unwrap();
        let selected_preamble = PagePreambleDelta {
            page_id: target,
            home_document_id: home,
            before: Some(PagePreambleState {
                page_id: target,
                home_document_id: home,
                preamble: None,
            }),
            after: Some(PagePreambleState {
                page_id: target,
                home_document_id: home,
                preamble: Some("title:: Café Plan".into()),
            }),
        };
        let selected_explicit_title = EffectiveExplicitTitleState::from_authenticated_transition(
            target,
            home,
            &ManagedPath::parse("pages/physical.md").unwrap(),
            &selected_name,
            ManagedTextKind::Page,
            Some(selected_preamble.clone()),
        )
        .unwrap();
        let effective = authored
            .splice_title_post_state(target, &selected_name, &selected_explicit_title)
            .unwrap();

        assert_eq!(effective.pages().len(), authored.pages().len());
        assert_eq!(effective.pages()[1], authored.pages()[1]);
        assert_eq!(effective.pages()[2], authored.pages()[2]);
        assert_eq!(effective.blocks(), authored.blocks());
        assert_eq!(effective.memberships(), authored.memberships());
        assert_eq!(
            effective.pages()[0].before,
            authored.pages()[0].before,
            "the authored pre-state remains the immutable dependency assertion"
        );
        assert_eq!(
            effective.pages()[0].after.as_ref().unwrap().name(),
            &selected_name
        );
        assert_eq!(
            effective.page_preambles()[0].after.as_ref(),
            selected_preamble.after.as_ref()
        );
    }

    #[test]
    fn authenticated_title_splice_covers_explicit_fallback_presence_and_removal() {
        let page_id = page_id(0x510);
        let home = document_id(0x511);
        let path = ManagedPath::parse("pages/physical.md").unwrap();
        let original = LogicalPageName::parse("Original").unwrap();
        let authored_name = LogicalPageName::parse("Cafe\u{301} Plan").unwrap();
        let selected_name = LogicalPageName::parse("Café Plan").unwrap();
        let page_delta = || PageDelta {
            page_id,
            before: Some(PageState::Live {
                name: original.clone(),
                path: path.clone(),
                home_document_id: home,
                kind: ManagedTextKind::Page,
            }),
            after: Some(PageState::Live {
                name: authored_name.clone(),
                path: path.clone(),
                home_document_id: home,
                kind: ManagedTextKind::Page,
            }),
        };
        let preamble_state = |preamble: Option<&str>| PagePreambleState {
            page_id,
            home_document_id: home,
            preamble: preamble.map(str::to_owned),
        };

        let authored_explicit = SemanticEffect::new_with_page_preambles(
            vec![page_delta()],
            vec![PagePreambleDelta {
                page_id,
                home_document_id: home,
                before: Some(preamble_state(Some("alias:: keep\nproperty:: before"))),
                after: Some(preamble_state(Some(
                    "alias:: keep\ntitle:: Cafe\u{301} Plan\nproperty:: after",
                ))),
            }],
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let selected_fallback = EffectiveExplicitTitleState::from_authenticated_transition(
            page_id,
            home,
            &path,
            &selected_name,
            ManagedTextKind::Page,
            None,
        )
        .unwrap();
        let fallback_effective = authored_explicit
            .splice_title_post_state(page_id, &selected_name, &selected_fallback)
            .unwrap();
        assert_eq!(
            fallback_effective.pages()[0].after.as_ref().unwrap().name(),
            &selected_name
        );
        assert_eq!(
            fallback_effective.page_preambles()[0]
                .after
                .as_ref()
                .unwrap()
                .preamble
                .as_deref(),
            Some("alias:: keep\nproperty:: after"),
            "selected fallback removes only the losing explicit title"
        );

        let authored_fallback =
            SemanticEffect::new(vec![page_delta()], Vec::new(), Vec::new()).unwrap();
        let selected_transition = PagePreambleDelta {
            page_id,
            home_document_id: home,
            before: Some(preamble_state(Some("alias:: keep"))),
            after: Some(preamble_state(Some("alias:: keep\ntitle:: Café Plan"))),
        };
        let selected_explicit = EffectiveExplicitTitleState::from_authenticated_transition(
            page_id,
            home,
            &path,
            &selected_name,
            ManagedTextKind::Page,
            Some(selected_transition),
        )
        .unwrap();
        let explicit_effective = authored_fallback
            .splice_title_post_state(page_id, &selected_name, &selected_explicit)
            .unwrap();
        let inserted = &explicit_effective.page_preambles()[0];
        assert_eq!(
            inserted.before.as_ref().unwrap().preamble.as_deref(),
            Some("alias:: keep")
        );
        assert_eq!(
            inserted.after.as_ref().unwrap().preamble.as_deref(),
            Some("title:: Café Plan\nalias:: keep"),
            "selected explicit title is added without dropping unrelated preamble"
        );

        let journal_name = LogicalPageName::parse("2026-07-25").unwrap();
        let journal_transition = PagePreambleDelta {
            page_id,
            home_document_id: home,
            before: Some(preamble_state(Some("alias:: keep"))),
            after: Some(preamble_state(Some("alias:: keep\ntitle:: 25-07-2026"))),
        };
        assert_eq!(
            EffectiveExplicitTitleState::from_authenticated_transition(
                page_id,
                home,
                &path,
                &journal_name,
                ManagedTextKind::Page,
                Some(journal_transition.clone()),
            ),
            Err(SemanticError::InvalidEffectiveTitleSplice),
            "ordinary pages still require exact parser-owned title text"
        );
        let selected_journal = EffectiveExplicitTitleState::from_authenticated_transition(
            page_id,
            home,
            &path,
            &journal_name,
            ManagedTextKind::Journal,
            Some(journal_transition),
        )
        .unwrap();
        let mut journal_preamble = Some("title:: losing\nproperty:: keep".to_owned());
        selected_journal
            .apply_to_preamble(&path, &mut journal_preamble)
            .unwrap();
        assert_eq!(
            journal_preamble.as_deref(),
            Some("title:: 25-07-2026\nproperty:: keep"),
            "journal authority preserves the authenticated authored title while the parser owns \
             its normalized logical name"
        );
    }

    #[test]
    fn logical_page_name_key_v1_golden_vectors_preserve_exact_spelling() {
        let vectors = [
            ("  /Foo/  ", "foo"),
            ("  //Foo//  ", "/foo/"),
            ("İ", "i\u{307}"),
            ("Cafe\u{301}", "café"),
            ("CAFÉ", "café"),
            ("Ａ", "ａ"),
            ("ﬀ", "ﬀ"),
        ];

        for (exact, expected_key) in vectors {
            let name = LogicalPageName::parse(exact).unwrap();
            assert_eq!(name.as_str(), exact);
            assert_eq!(name.canonical_key(), expected_key);
            assert_eq!(
                serde_json::to_string(&name).unwrap(),
                serde_json::to_string(exact).unwrap()
            );
        }
        assert_eq!(PAGE_NAME_KEY_VERSION, 1);
    }

    #[test]
    fn page_name_key_digest_v1_golden_vectors() {
        let vectors = [
            (
                "Foo",
                "foo",
                "77a942d3d10ffd4ccb1cc5a779720da867b9a444d98bed64dd02bda4b9b345eb",
            ),
            (
                " /Namespace/Page/ ",
                "namespace/page",
                "3ca340a434639833783bdf29b2f156048e11bfbaaf07380dc75eeb33b6f894c8",
            ),
            (
                "A\u{0308}",
                "\u{00e4}",
                "9e7cfc07c40adc60d24fab6cf66311853fc12f909d5739b0be650267ce4eb806",
            ),
        ];

        for (exact, canonical, expected) in vectors {
            let name = LogicalPageName::parse(exact).unwrap();
            assert_eq!(name.canonical_key(), canonical);
            assert_eq!(name.key_digest().to_string(), expected);
        }
    }

    #[test]
    fn logical_page_name_rejects_controls_empty_and_raw_or_canonical_overbounds() {
        for invalid in ["", " \t ", "/", " / ", "\0name", "name\r", "name\n"] {
            assert!(
                LogicalPageName::parse(invalid).is_err(),
                "{invalid:?} must fail"
            );
            assert!(
                serde_json::from_value::<LogicalPageName>(serde_json::json!(invalid)).is_err(),
                "{invalid:?} serde must fail closed"
            );
        }

        let raw_overbound = "x".repeat(MAX_LOGICAL_PAGE_NAME_BYTES + 1);
        assert_eq!(
            LogicalPageName::parse(raw_overbound).unwrap_err(),
            LogicalPageNameError::RawTooLarge(MAX_LOGICAL_PAGE_NAME_BYTES + 1)
        );
        let lowercase_expands = "İ".repeat(MAX_LOGICAL_PAGE_NAME_BYTES / "İ".len());
        assert_eq!(lowercase_expands.len(), MAX_LOGICAL_PAGE_NAME_BYTES);
        assert!(matches!(
            LogicalPageName::parse(lowercase_expands),
            Err(LogicalPageNameError::CanonicalTooLarge(bytes))
                if bytes > MAX_LOGICAL_PAGE_NAME_BYTES
        ));
    }

    #[test]
    fn catalog_page_state_v2_round_trips_and_prior_schema_fails_closed() {
        let state = live(ManagedTextKind::Page);
        let mut json = serde_json::to_value(&state).unwrap();
        assert_eq!(
            json["catalog_page_state_schema_version"],
            serde_json::json!(CATALOG_PAGE_STATE_SCHEMA_VERSION)
        );
        assert_eq!(
            serde_json::from_value::<PageState>(json.clone()).unwrap(),
            state
        );

        json["catalog_page_state_schema_version"] =
            serde_json::json!(CATALOG_PAGE_STATE_SCHEMA_VERSION - 1);
        assert!(serde_json::from_value::<PageState>(json).is_err());
    }

    #[test]
    fn catalog_page_state_v2_rejects_unknown_nested_live_and_tombstone_fields() {
        for state in [
            live(ManagedTextKind::Page),
            tombstone(document_id(2), ManagedTextKind::Page),
        ] {
            let encoded = postcard::to_allocvec(&state).unwrap();
            assert_eq!(postcard::from_bytes::<PageState>(&encoded).unwrap(), state);

            let mut json = serde_json::to_value(&state).unwrap();
            let variant = if matches!(state, PageState::Live { .. }) {
                "live"
            } else {
                "tombstone"
            };
            json["state"][variant]["future_field"] = serde_json::json!(true);
            assert!(serde_json::from_value::<PageState>(json).is_err());
        }
    }

    #[test]
    fn semantic_effect_rejects_unknown_nested_logseq_identity_origin_fields() {
        let block = BlockId::from_uuid(Uuid::from_u128(3));
        let effect = SemanticEffect::new(
            Vec::new(),
            vec![BlockDelta {
                block_id: block,
                home_document_id: document_id(2),
                before: None,
                after: Some(BlockState {
                    block_id: block,
                    home_document_id: document_id(2),
                    owner: BlockOwner::Page(page_id(1)),
                    logseq_uuid: Some(LogseqUuid::from_uuid(Uuid::from_u128(4))),
                    logseq_identity_origin: Some(LogseqIdentityOrigin::PolicyGenerated {
                        reason: PolicyGeneratedAnchorReason::BlockReference,
                    }),
                    content: "linked block".into(),
                }),
            }],
            Vec::new(),
        )
        .unwrap();

        let mut json = serde_json::to_value(&effect).unwrap();
        json["blocks"][0]["after"]["logseq_identity_origin"]["policy_generated"]["future_field"] =
            serde_json::json!(true);
        assert!(serde_json::from_value::<SemanticEffect>(json).is_err());
    }

    #[test]
    fn page_delta_lifecycle_allows_creation_live_edits_and_same_kind_deletion() {
        let home = document_id(2);
        let allowed = [
            (
                "creation",
                None,
                Some(live_at("shared/name.md", home, ManagedTextKind::Page)),
            ),
            (
                "kind-only edit",
                Some(live_at("shared/name.md", home, ManagedTextKind::Page)),
                Some(live_at("shared/name.md", home, ManagedTextKind::Journal)),
            ),
            (
                "path-only edit",
                Some(live_at("shared/name.md", home, ManagedTextKind::Page)),
                Some(live_at("renamed/name.md", home, ManagedTextKind::Page)),
            ),
            (
                "path-and-kind edit",
                Some(live_at("shared/name.md", home, ManagedTextKind::Page)),
                Some(live_at("journals/name.md", home, ManagedTextKind::Journal)),
            ),
            (
                "same-kind deletion",
                Some(live_at("shared/name.md", home, ManagedTextKind::Journal)),
                Some(tombstone(home, ManagedTextKind::Journal)),
            ),
        ];

        for (name, before, after) in allowed {
            assert!(page_effect(before, after).is_ok(), "{name} must be valid");
        }
    }

    #[test]
    fn page_delta_lifecycle_rejects_invalid_transitions() {
        let home = document_id(2);
        let rejected = [
            (
                "tombstone creation",
                None,
                Some(tombstone(home, ManagedTextKind::Page)),
                SemanticError::InvalidPageLifecycle,
            ),
            (
                "physical removal of a live page",
                Some(live(ManagedTextKind::Page)),
                None,
                SemanticError::InvalidPageLifecycle,
            ),
            (
                "tombstone physical removal",
                Some(tombstone(home, ManagedTextKind::Page)),
                None,
                SemanticError::InvalidPageLifecycle,
            ),
            (
                "tombstone mutation",
                Some(tombstone(home, ManagedTextKind::Page)),
                Some(tombstone(home, ManagedTextKind::Journal)),
                SemanticError::InvalidPageLifecycle,
            ),
            (
                "tombstone resurrection",
                Some(tombstone(home, ManagedTextKind::Page)),
                Some(live(ManagedTextKind::Page)),
                SemanticError::InvalidPageLifecycle,
            ),
            (
                "kind-changing deletion",
                Some(live(ManagedTextKind::Page)),
                Some(tombstone(home, ManagedTextKind::Journal)),
                SemanticError::InvalidPageLifecycle,
            ),
            (
                "name-changing deletion",
                Some(live(ManagedTextKind::Page)),
                Some(PageState::Tombstone {
                    name: LogicalPageName::parse("Different Name").unwrap(),
                    home_document_id: home,
                    kind: ManagedTextKind::Page,
                }),
                SemanticError::InvalidPageLifecycle,
            ),
            (
                "home change",
                Some(live(ManagedTextKind::Page)),
                Some(live_at(
                    "shared/name.md",
                    document_id(3),
                    ManagedTextKind::Page,
                )),
                SemanticError::HomeShardChanged,
            ),
            (
                "unchanged absent state",
                None,
                None,
                SemanticError::UnchangedDelta,
            ),
            (
                "unchanged live state",
                Some(live(ManagedTextKind::Page)),
                Some(live(ManagedTextKind::Page)),
                SemanticError::UnchangedDelta,
            ),
        ];

        for (name, before, after, expected) in rejected {
            assert_eq!(page_effect(before, after).unwrap_err(), expected, "{name}");
        }
    }

    #[test]
    fn page_kind_is_canonical_semantic_state_and_effect_identity() {
        let page = effect(ManagedTextKind::Page);
        let journal = effect(ManagedTextKind::Journal);
        let page_bytes = page.encode().unwrap();
        let journal_bytes = journal.encode().unwrap();

        assert_ne!(page, journal);
        assert_ne!(page_bytes, journal_bytes);
        assert_ne!(
            super::super::SemanticEffectDigest::of(&page_bytes),
            super::super::SemanticEffectDigest::of(&journal_bytes)
        );
        assert_eq!(
            SemanticEffect::decode(&page_bytes).unwrap().pages()[0]
                .after
                .as_ref()
                .unwrap()
                .kind(),
            ManagedTextKind::Page
        );
        assert_eq!(
            SemanticEffect::decode(&journal_bytes).unwrap().pages()[0]
                .after
                .as_ref()
                .unwrap()
                .kind(),
            ManagedTextKind::Journal
        );
    }

    #[test]
    fn exact_page_name_is_canonical_state_effect_and_digest_identity() {
        let first = effect(ManagedTextKind::Page);
        let mut second_state = live(ManagedTextKind::Page);
        let PageState::Live { name, .. } = &mut second_state else {
            unreachable!("fixture is live")
        };
        *name = LogicalPageName::parse("Different Exact Spelling").unwrap();
        let second = page_effect(None, Some(second_state)).unwrap();
        let first_bytes = first.encode().unwrap();
        let second_bytes = second.encode().unwrap();

        assert_ne!(first, second);
        assert_ne!(first_bytes, second_bytes);
        assert_ne!(
            super::super::SemanticEffectDigest::of(&first_bytes),
            super::super::SemanticEffectDigest::of(&second_bytes)
        );
        assert_eq!(SemanticEffect::decode(&second_bytes).unwrap(), second);
    }

    #[test]
    fn tombstone_kind_round_trips_without_path_or_layout_inference() {
        let effect = SemanticEffect::new(
            vec![PageDelta {
                page_id: page_id(1),
                before: Some(live(ManagedTextKind::Journal)),
                after: Some(PageState::Tombstone {
                    name: LogicalPageName::parse("Shared Name").unwrap(),
                    home_document_id: document_id(2),
                    kind: ManagedTextKind::Journal,
                }),
            }],
            Vec::new(),
            Vec::new(),
        )
        .unwrap();

        let decoded = SemanticEffect::decode(&effect.encode().unwrap()).unwrap();
        let delta = &decoded.pages()[0];
        assert_eq!(
            delta.before.as_ref().unwrap().kind(),
            ManagedTextKind::Journal
        );
        assert_eq!(
            delta.after.as_ref().unwrap().kind(),
            ManagedTextKind::Journal
        );
        assert_eq!(delta.after.as_ref().unwrap().path(), None);
    }

    #[test]
    fn prior_semantic_effect_schema_fails_closed() {
        let current = effect(ManagedTextKind::Page);
        let wire = SemanticEffectWire {
            semantic_effect_schema_version: SEMANTIC_EFFECT_SCHEMA_VERSION - 1,
            pages: current.pages.clone(),
            page_preambles: current.page_preambles.clone(),
            blocks: current.blocks.clone(),
            memberships: current.memberships.clone(),
        };
        let mut bytes = SEMANTIC_MAGIC.to_vec();
        bytes.extend(postcard::to_allocvec(&wire).unwrap());

        assert_eq!(
            SemanticEffect::decode(&bytes),
            Err(SemanticError::UnknownVersion(
                SEMANTIC_EFFECT_SCHEMA_VERSION - 1
            ))
        );
    }
}
