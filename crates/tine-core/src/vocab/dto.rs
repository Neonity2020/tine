//! DTOs shared by the document and query layers.

use super::*;

/// Lightweight entry for the page list / sidebar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageEntry {
    pub name: String,
    pub kind: PageKind,
    /// Sort key `yyyymmdd` for journals; `None` for ordinary pages.
    pub date_key: Option<i64>,
    /// Graph-root-relative path exposed to the frontend so duplicate basenames
    /// can be opened by file, not by ambiguous `(kind,name)`.
    #[serde(rename = "path", default)]
    pub rel_path: String,
    #[serde(skip)]
    pub path: PathBuf,
}

/// A block as sent to / received from the frontend.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockDto {
    pub id: String,
    pub raw: String,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub children: Vec<BlockDto>,
    /// Ancestor first-lines (page-relative path) for search/reference results;
    /// empty for normal page loads. Lets the UI show a "parent › child" trail.
    #[serde(default)]
    pub breadcrumb: Vec<String>,
    /// Synthetic, read-only result row representing references from the source
    /// page's property pre-block rather than an editable outline block.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub page_property: bool,
    // --- M1: block-header facets, computed ONCE off the lsdoc projection (the one
    // grammar source) and shipped so the frontend never re-derives them with its
    // own scanner. Derived (not authoritative — `raw` round-trips); the frontend
    // recomputes locally only for the block it is actively editing. Omitted from the
    // wire when empty to keep the payload small (most blocks have no marker/dates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_level: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub properties: Vec<(String, String)>,
}

/// A group of blocks from one source page — used for both Linked References
/// (backlinks) and `{{query}}` results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefGroup {
    pub page: String,
    pub kind: PageKind,
    pub blocks: Vec<BlockDto>,
    /// Result-only source evidence keyed by block id. Empty for ordinary query
    /// groups and older callers; never crosses the block write boundary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<ReferenceBlockEvidence>,
}

/// One backlink root whose visible subtree can be searched and whose OG-style
/// co-reference facets came from the cached lsdoc projection. This is fetched
/// only when the Linked References filter opens; ordinary backlink DTOs remain
/// shallow so their lazy-loading and bridge cost do not change.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct BacklinkFilterTarget {
    pub page: String,
    pub kind: PageKind,
    pub block_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacklinkFilterEntry {
    pub page: String,
    pub kind: PageKind,
    pub block_id: String,
    pub text: String,
    pub facets: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BacklinkFilterContext {
    pub entries: Vec<BacklinkFilterEntry>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// A deliberately bounded block-reference hover preview. Ordinary query,
/// reference, and batched-resolution results carry shallow block identities;
/// callers that genuinely need a subtree must ask for one explicitly and give
/// it node and byte budgets so an outline cannot be multiplied across the IPC
/// bridge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockPreview {
    pub group: RefGroup,
    /// Number of nodes omitted after either construction budget was reached.
    pub truncated: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Explicit,
    Plain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceSpan {
    /// UTF-16 code-unit offsets into the matching `BlockDto.raw`.
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceOccurrence {
    pub matched_name: String,
    pub canonical: String,
    pub kind: ReferenceKind,
    pub span: ReferenceSpan,
    pub rule: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceBlockEvidence {
    pub block_id: String,
    pub occurrences: Vec<ReferenceOccurrence>,
    /// Total parser-owned matches before the bounded evidence cap.
    #[serde(default)]
    pub total: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceDiagnosticTrace {
    pub page: String,
    pub kind: PageKind,
    pub block_id: String,
    pub occurrences: Vec<ReferenceOccurrence>,
    pub included_linked: bool,
    pub included_unlinked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclusion_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceDiagnostics {
    pub engine_version: String,
    pub target: String,
    pub traces: Vec<ReferenceDiagnosticTrace>,
}

/// A named template (a block with `template:: <name>`) and the blocks to insert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateDto {
    pub name: String,
    pub blocks: Vec<BlockDto>,
    /// Page the template's defining block lives on (so the UI can jump to edit it).
    pub page: String,
    /// Kind of that page (journal/page), for navigation.
    pub kind: PageKind,
}

/// A full page as sent to / received from the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageDto {
    pub name: String,
    pub kind: PageKind,
    pub title: String,
    /// Raw page-property pre-block (if any).
    pub pre_block: Option<String>,
    pub blocks: Vec<BlockDto>,
    /// Hash of the on-disk file content when this page was loaded — the editor's
    /// baseline. Sent back on save so we conflict against the version the editor
    /// actually loaded (not the mutable cache, which the watcher can advance).
    /// `None` for a page with no file yet.
    #[serde(default)]
    pub rev: Option<String>,
    /// On-disk format of this page (markdown vs org), so the editor renders org
    /// inline syntax and shows the right bullet. New pages default to markdown.
    #[serde(default)]
    pub format: Format,
    /// True for an org page Tine can't round-trip byte-for-byte: the editor shows
    /// it but disables editing, so Tine never rewrites (and risks corrupting) it.
    #[serde(default)]
    pub read_only: bool,
    /// Graph-root-relative path of the file this page was loaded from
    /// (`journals/2026_06_26.org`), forward-slashed. Echoed back on save so a page
    /// pinned to a SPECIFIC file — a duplicate-day stray that shares a `(kind,name)`
    /// with the canonical file — saves to its own file instead of being re-resolved
    /// by name to the canonical one (#21). Empty for a brand-new page with no file
    /// yet; then save resolves the path by name, exactly as before.
    #[serde(default)]
    pub path: String,
    /// Which live editor instance is issuing this save, if any.
    ///
    /// The wire half of the editor-activation boundary. It is carried on the DTO
    /// rather than on the frontend's `FeedPage` deliberately: a token stored in a
    /// page value is copied by every clone, snapshot and history round-trip, and
    /// the copy would then claim an identity it does not have. The frontend keeps
    /// activations in a registry keyed by page identity and stamps this field when
    /// it builds the DTO.
    ///
    /// `None` is an editor-less writer (external import,
    /// sync-id migration, PDF-highlight write) or a pre-increment-3 caller. Legal
    /// on the ordinary path, where the base-revision guard is the authority;
    /// refused on the override path. (GH #254 increment 3.)
    #[serde(default)]
    pub activation: Option<u64>,
    /// True for bundled in-app Guide pages. Guide pages are ephemeral/read-only
    /// virtual pages and must never be persisted into the user's graph by the
    /// normal save/writeback path.
    #[serde(default)]
    pub guide: bool,
}
