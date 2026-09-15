//! The data types Graph hands across its boundary: page entries, BlockDto and
//! PageDto, reference groups, backlink filters and reference diagnostics,
//! templates, asset and trash stats, journal and sync-conflict records, and
//! rename outcomes.

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

/// Digest of one exact user-visible Markdown/Org source file.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphTextSourceDigest {
    pub path: String,
    pub length: u64,
    pub digest: String,
}

/// One parser-owned interpretation of a present external graph-text document.
///
/// Journal conversion is represented only by `effective`: it is applied after
/// selecting the explicit title or filename fallback, matching OG.
pub(crate) struct ParsedExternalDocument {
    pub(crate) format: Format,
    pub(crate) effective: PageEntry,
    pub(crate) parsed: doc::ParsedDocument,
    pub(crate) revision: String,
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

/// Cache-friendly bounded result metadata. The groups stay behind one `Arc` so
/// routine frontend refreshes can reuse the generation-scoped native result
/// without a deep clone while preserving the construction ceiling's outcome.
#[derive(Debug, Clone)]
pub struct BoundedRefGroups {
    pub statistics: Option<crate::query::ir::QueryStatistics>,
    pub matched_total: Option<usize>,
    pub groups: Arc<Vec<RefGroup>>,
    pub total: usize,
    pub exceeded: bool,
}

/// The reference-name inventory as answered to a caller that may already hold
/// it. `names` is `None` exactly when the caller's presented digest still
/// describes the current set — which is the ordinary case, since the frontend
/// re-asks after every typing lull and typing inside a block rarely adds or
/// removes a `[[link]]`. Callers must read `None` as "keep what you have", never
/// as "the graph references nothing".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferencedPageNames {
    pub digest: u64,
    pub names: Option<Vec<String>>,
}

impl ReferencedPageNames {
    pub(super) fn answer(digest: u64, names: &[String], known: Option<u64>) -> Self {
        Self {
            digest,
            names: (known != Some(digest)).then(|| names.to_vec()),
        }
    }
}

/// Order-independent digest of a reference-name set.
///
/// Commutative on purpose: the memo's order comes from a `HashMap`, so a
/// sequence-dependent hash would report a change on every rebuild even when the
/// set is identical — which is exactly the case this exists to make cheap. The
/// length is mixed in separately so a name swapped for one whose hash collides
/// with it does not slip through unless the count matches too.
pub(super) fn referenced_names_digest(names: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut sum: u64 = 0;
    let mut xor: u64 = 0;
    for name in names {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut hasher);
        let hash = hasher.finish();
        sum = sum.wrapping_add(hash);
        xor ^= hash;
    }
    sum.rotate_left(17) ^ xor.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (names.len() as u64)
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

/// An orphaned asset file (no block references it) — surfaced so the user can
/// review + trash unused media. `size` in bytes; `modified` is the file's
/// last-modified time as Unix seconds (≈ when it entered the graph), or `None`
/// if the filesystem doesn't report it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetInfo {
    pub name: String,
    pub size: u64,
    pub modified: Option<u64>,
}

/// Count + total bytes of recoverable asset trash. `count`/`bytes` are asset
/// entries only; the other counters are protected non-asset recovery files that
/// share `logseq/.tine-trash` for backward compatibility.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TrashStats {
    pub count: u64,
    pub bytes: u64,
    pub pages: u64,
    pub journals: u64,
    pub conflicts: u64,
    pub other: u64,
}

/// One file participating in a journal-day conflict: its on-disk filename, a
/// graph-root-relative path (so the UI can navigate straight to THIS file even
/// when it shares a date with the canonical one, #21), a one-line content
/// preview, and whether its name is the canonical date stem (`yyyy_MM_dd`, the
/// one normally kept).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalFile {
    pub name: String,
    pub path: String,
    pub preview: String,
    pub canonical: bool,
}

/// A journal day that resolves to more than one file (e.g. a canonical
/// `2026_06_26.org` plus a title-named `Friday, 26-06-2026.org`, or a `.md`+`.org`
/// twin). These can't be auto-merged, so they're surfaced for the user to
/// reconcile (delete the redundant one / copy content across).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalConflict {
    pub title: String,
    pub files: Vec<JournalFile>,
}

/// A journal file whose name does not round-trip to its date, and the name it
/// would get. Concord invariant 4: Tine PROPOSES these renames, it no longer
/// performs them behind the user's back at graph open — a rename in a tree the
/// user keeps in git is a diff they did not ask for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalFilenameMigration {
    /// Graph-relative path as it is on disk today.
    pub from: String,
    /// Graph-relative path it would be renamed to.
    pub to: String,
}

/// Lifetime of the ONE authorized write to a marker-bearing file (Concord
/// invariant 3's single exemption). Dropping it — including on an early return
/// or a panic — re-arms the refusal for that path, so an exemption can never
/// outlive the resolution that earned it.
pub(super) struct MarkerResolutionGuard<'a> {
    pub(super) graph: &'a Graph,
    pub(super) path: PathBuf,
}

impl Drop for MarkerResolutionGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut set) = self.graph.marker_resolutions.lock() {
            set.remove(&self.path);
        }
    }
}

/// A sync-tool conflict copy left in the graph (Syncthing/Dropbox) — a
/// `*.sync-conflict-*.md` (or Dropbox `(conflicted copy)`) file that shadows a
/// real page. Surfaced so the user can review + reconcile it instead of it
/// rotting as a garbage page. See [`sync_conflict_base`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConflict {
    /// Graph-root-relative path of the conflict copy file.
    pub path: String,
    /// Display name of the page it shadows (decoded page name / journal title).
    pub base_name: String,
    /// Graph-root-relative path of the winning (base) file, if it still exists.
    pub base_path: Option<String>,
    /// Kind of the shadowed page (journal/page).
    pub kind: PageKind,
    /// The device/timestamp suffix from the conflict filename (best-effort label).
    pub tag: String,
    /// One-line content preview of the conflict copy.
    pub preview: String,
}

/// What a rename deliberately left undone.
///
/// A rename cascades reference rewrites through every referring page. Files
/// under VCS-marker quarantine are skipped rather than rewritten (see
/// [`Graph::rename_page_reporting`]), so the caller needs to know which ones
/// still point at the old name — otherwise the skip is invisible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameOutcome {
    /// Paths of quarantined referrers left byte-identical, old refs intact.
    pub skipped_conflicted_referrers: Vec<String>,
}

/// A page whose ON-DISK bytes carry unresolved VCS merge-conflict markers
/// (git/Fossil; see [`crate::doc::vcs_conflict_markers`]). The page stays
/// readable, but saves to it are refused so Tine never mangles the markers —
/// surfaced alongside [`SyncConflict`]s so the conflicts panel can say
/// "N files contain unresolved VCS merge markers".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcsMarkerConflict {
    /// Graph-root-relative path of the marker-bearing file.
    pub path: String,
    /// Display name of the page (decoded page name / journal title).
    pub name: String,
    pub kind: PageKind,
    /// Distinct marker kinds found, in order of first appearance
    /// (e.g. `["<<<<<<<", "=======", ">>>>>>>"]`).
    pub markers: Vec<String>,
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

/// Exact asset-side result of one PDF-highlight merge. The annotation page is
/// a separate authority boundary, committed through the guarded file writer.
/// Keeping the sidecar receipt typed lets the caller compensate a rejected page
/// transaction without re-reading or guessing which bytes it published.
pub(crate) struct PdfHighlightSidecarCommit {
    pub(super) legacy_key: String,
    pub(super) edn_path: PathBuf,
    pub(super) legacy_edn: Option<PathBuf>,
    pub(super) merged: Vec<crate::pdf::Highlight>,
    pub(super) primary_baseline: Option<String>,
    pub(super) legacy_baseline: Option<String>,
    pub(super) committed: String,
    pub(super) area_source_key: String,
    pub(super) deleted_areas: Vec<crate::pdf::Highlight>,
}

impl PdfHighlightSidecarCommit {
    pub(crate) fn merged(&self) -> &[crate::pdf::Highlight] {
        &self.merged
    }
}
