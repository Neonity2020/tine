//! Graph model: opening a graph directory, listing/loading/saving pages, and
//! the DTOs that cross the Tauri IPC boundary.
//!
//! For M0/M1 the canonical state is the on-disk files; Rust loads a page into a
//! [`PageDto`] tree and writes it back from one. The frontend owns the live
//! editing tree (see plan). File-backed runtime UUIDs are deterministic structural
//! locators; persisted `id::` values remain a separate external reference identity.

use crate::config::{Config, FileNameFormat};
use crate::date::{JournalDate, JournalFormat};
use crate::doc::{self, DocBlock, Document, StructuralLayoutIdentity};
use crate::graph_text_path::{
    graph_text_component_is_portable, BlobDescription, CanonicalGraphResourceId, GraphTextKind,
    GraphTextPath, PortablePathKey, UnsafeGraphTextPath,
};
use crate::graph_text_scope::{GraphTextScope, GraphTextScopeBinding};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::RwLock;
use tine_storage::ContentDigest;
use tine_storage::{DurableDirectoryPublication, FilesystemError};
use uuid::Uuid;

mod assets;
mod conflicts;
mod direct_query;
mod editor_activation;
mod graph_text_identity;
mod graph_text_inventory;
mod graph_text_scope;
mod graph_text_sources;
mod journals;
mod lookup;
mod open_graph;
mod page_cache;
mod page_inventory;
mod page_rename;
mod pages_merge;
mod pdf;
mod persistent_map;
mod projection_lifetime;
mod queries;
mod save_path;
mod search;
mod sync_file;
mod write_receipts;
use persistent_map::{PersistentMap, PersistentMapNode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageKind {
    Journal,
    Page,
}

const LOGSEQ_TEXT_EXTENSIONS: [&str; 3] = ["md", "markdown", "org"];

#[cfg(test)]
thread_local! {
    /// §5.3's hydration census: the pages a DISPATCHED query loaded a `Document`
    /// for. The claim it makes observable is I-13/I-15's — "pages loaded equals
    /// result pages" — which production also enforces by refusing a mismatched
    /// hydration, but a counter a test can read is what keeps the claim from
    /// quietly becoming "pages loaded is at most the whole graph".
    static DIRECT_HYDRATED_PAGES: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// On-disk file format of a page. Markdown (`.md`/`.markdown`) is the default; Logseq org
/// graphs use `.org`. A graph may mix the two — format is decided per file by
/// extension, never graph-wide (matching OG, which stores `:block/format` per
/// page). The graph's `:preferred-format` only chooses the extension for NEW
/// files (see [`Graph::preferred_format`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Md,
    Org,
}

impl Format {
    /// Format of a page file by its extension (`.org` → Org, else Md).
    pub fn from_path(p: &Path) -> Format {
        match p.extension().and_then(|e| e.to_str()) {
            Some(extension) if extension.eq_ignore_ascii_case("org") => Format::Org,
            _ => Format::Md,
        }
    }
    /// File extension (no dot) for this format.
    pub fn ext(self) -> &'static str {
        match self {
            Format::Md => "md",
            Format::Org => "org",
        }
    }
}

fn is_logseq_text_extension(extension: &str) -> bool {
    LOGSEQ_TEXT_EXTENSIONS
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
}

fn text_extension_from_path(path: &Path) -> Option<&str> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| is_logseq_text_extension(extension))
}

fn split_logseq_text_filename(filename: &str) -> Option<(&str, &str)> {
    filename
        .rsplit_once('.')
        .filter(|(stem, extension)| !stem.is_empty() && is_logseq_text_extension(extension))
}

fn configured_text_variant_paths(dir: &Path, stem: &str) -> [PathBuf; 3] {
    LOGSEQ_TEXT_EXTENSIONS.map(|extension| dir.join(format!("{stem}.{extension}")))
}

/// Whether `path` is a page file Tine reads (markdown or org).
fn is_page_file(path: &Path) -> bool {
    text_extension_from_path(path).is_some()
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// If `stem` is a sync tool's conflict copy of another file, return the base file
/// stem it shadows. Recognises the GENERATED shapes only (a page whose name
/// merely resembles one stays a real page):
///
/// - Syncthing: `name.sync-conflict-YYYYMMDD-HHMMSS-DEVICEID`
///   (`conflictName` in syncthing `lib/model/folder_sendrecv.go`; the device id
///   is the modifying device's short id — up to 7 base32 chars `[A-Z2-7]`,
///   empty when unknown — and pre-1.1.0 versions omitted `-DEVICEID`).
/// - Seafile: `name (SFConflict [modifier ]YYYY-MM-DD-HH-MM-SS)`
///   (`gen_conflict_path` in seafile `common/vc-common.c`; the modifier is the
///   editing user's id when known).
/// - Dropbox: `name (conflicted copy …)` / `name (<user>'s conflicted copy …)`.
///
/// Deliberately NOT recognized (too ambiguous to distinguish from a real page
/// name, so treating them as conflict copies would deindex real pages):
/// OneDrive's `name-COMPUTERNAME.ext` and Google Drive's `name (1).ext`.
///
/// A conflict copy is NOT a real page — the versioned graph-text policy keeps it
/// out of normal discovery and exact page resolution. The explicit conflict
/// workflow has its own retained-capability path.
pub fn sync_conflict_base(stem: &str) -> Option<&str> {
    const SYNCTHING_TAG: &str = ".sync-conflict-";
    let mut search = 0;
    while let Some(found) = stem[search..].find(SYNCTHING_TAG) {
        let i = search + found;
        if syncthing_conflict_tail(&stem[i + SYNCTHING_TAG.len()..]) {
            return Some(&stem[..i]);
        }
        search = i + SYNCTHING_TAG.len();
    }
    const SEAFILE_TAG: &str = " (SFConflict ";
    if let Some(inner) = stem.strip_suffix(')') {
        if let Some(i) = inner.rfind(SEAFILE_TAG) {
            let args = &inner[i + SEAFILE_TAG.len()..];
            let timestamp = args.rsplit(' ').next().unwrap_or(args);
            if seafile_conflict_timestamp(timestamp) && !args.contains(')') {
                return Some(&stem[..i]);
            }
        }
    }
    // Dropbox: "<base> (conflicted copy …)" or "<base> (<user>'s conflicted copy …)".
    if let Some(i) = stem.find(" (") {
        if stem[i..].contains("conflicted copy") {
            return Some(&stem[..i]);
        }
    }
    None
}

/// Whether the text after `.sync-conflict-` matches Syncthing's generated
/// `YYYYMMDD-HHMMSS[-DEVICEID]` tail exactly to the end of the stem.
fn syncthing_conflict_tail(tail: &str) -> bool {
    let bytes = tail.as_bytes();
    if bytes.len() < 15
        || !bytes[..8].iter().all(u8::is_ascii_digit)
        || bytes[8] != b'-'
        || !bytes[9..15].iter().all(u8::is_ascii_digit)
    {
        return false;
    }
    match &bytes[15..] {
        // Pre-1.1.0 Syncthing: no `-DEVICEID` suffix at all.
        [] => true,
        // The short device id: up to 7 chars of RFC 4648 base32 (`[A-Z2-7]`),
        // empty when the modifying device is unknown (zero ShortID).
        [b'-', device @ ..] => {
            device.len() <= 7
                && device
                    .iter()
                    .all(|&b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
        }
        _ => false,
    }
}

/// Whether `text` is Seafile's `%Y-%m-%d-%H-%M-%S` conflict timestamp
/// (`gen_conflict_path` in seafile `common/vc-common.c`).
fn seafile_conflict_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 19
        && bytes.iter().enumerate().all(|(i, &b)| {
            if matches!(i, 4 | 7 | 10 | 13 | 16) {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
}

/// Whether `stem` names a sync-tool conflict copy (see [`sync_conflict_base`]).
pub fn is_sync_conflict(stem: &str) -> bool {
    sync_conflict_base(stem).is_some()
}

/// Whether `path`'s file stem names a sync-tool conflict copy — the `Path`-level
/// convenience used by the watcher (which works in paths, not stems).
pub fn path_is_sync_conflict(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(is_sync_conflict)
}

/// Error for an ambiguous page that exists in multiple supported text extensions.
/// Deliberately NOT the `AlreadyExists`/"conflict" signal, so the UI surfaces it
/// as a plain error (a toast) instead of a keep-mine/use-disk conflict prompt.
fn twin_error(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Other,
        format!(
            "\"{name}\" exists in multiple .md/.markdown/.org files — remove all but one (e.g. in Logseq) to edit it in Tine"
        ),
    )
}

/// The error for a path-addressed op (#21) whose graph-root-relative path is
/// invalid — outside `journals/`/`pages/`, a traversal, or the wrong extension.
fn bad_path() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid file path")
}

/// A confirmed `"merged"` row decision the resolve could not re-derive from the
/// same base (see [`crate::sync_diff::MergeRefused`]). Refusing the whole
/// resolve is the point: no side is silently substituted for the merged body
/// the user approved, and nothing has been written when this is returned.
fn merge_refused(refusal: crate::sync_diff::MergeRefused) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, refusal.to_string())
}

thread_local! {
    /// Physical identities captured after this process displaced a live page.
    /// They are intentionally process-local: after a crash recovery must
    /// quarantine rather than treating a persisted identity as unlink authority.
    static IN_TURN_RECOVERY_IDENTITIES:
        std::cell::RefCell<std::collections::BTreeMap<Uuid, ContentDigest>> = const {
            std::cell::RefCell::new(std::collections::BTreeMap::new())
        };
}

struct ProjectionTarget {
    absolute_path: PathBuf,
    parent_components: Vec<String>,
    filename: String,
}

#[derive(Debug)]
struct ProjectionSemanticRefusal(String);

impl std::fmt::Display for ProjectionSemanticRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProjectionSemanticRefusal {}

fn projection_semantic_refusal(kind: io::ErrorKind, message: impl Into<String>) -> io::Error {
    io::Error::new(kind, ProjectionSemanticRefusal(message.into()))
}

pub(crate) fn is_projection_semantic_refusal(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.is::<ProjectionSemanticRefusal>())
}

/// Name the filesystem primitive and the graph location behind a raw platform
/// errno on the projection leg.
///
/// The device is the only oracle for Android's shared-storage semantics and one
/// CI round trip costs ~20 minutes, so a receipt that says only
/// `Invalid argument (os error 22)` cannot be acted on. `ErrorKind` is
/// preserved, because callers above classify on it (`NotFound`/`AlreadyExists`
/// are guarded-conflict signals) and the platform durability policy matches on
/// it too. A semantic refusal is returned untouched so its marker type survives.
fn projection_platform_error(operation: &str, location: &str, error: io::Error) -> io::Error {
    if is_projection_semantic_refusal(&error) {
        return error;
    }
    io::Error::new(
        error.kind(),
        format!("{operation} failed at {location}: {error}"),
    )
}

/// One lexical/scope validation result shared by exact points and feed events.
///
/// This deliberately has no twin path. `.markdown` is one exact physical
/// spelling, not an instruction to synthesize an `.md` or `.org` neighbor.
#[derive(Clone, Debug)]
struct GraphTextExactPath {
    graph_text_path: Option<GraphTextPath>,
    parent_components: Vec<String>,
    filename: String,
}

struct ProjectionParent {
    chain: Vec<Dir>,
}

impl ProjectionParent {
    fn final_dir(&self) -> &Dir {
        self.chain
            .last()
            .expect("projection parent chain always contains the graph root")
    }
}

enum ProjectionParentCapture {
    Missing,
    Present(ProjectionParent),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GraphTextPublicationValidation {
    /// Standalone callers have not established graph-wide collision evidence.
    CompleteIndex,
    /// One exact source/destination transition proves portable aliases,
    /// retained parent ownership, the source's single-link identity, and the
    /// destination's absence directly. No document contents are relevant.
    PathLocal,
    /// A surrounding transaction owns graph-text identity authority and has
    /// already completed a bounded no-follow inventory. Publication still
    /// repeats exact target, single-link, portable-path, and no-clobber checks.
    TransactionInventory,
}

/// Parse a page file's bytes into a [`Document`] using the parser for its
/// format (org headlines vs markdown bullets), chosen by the path's extension.
fn parse_doc(path: &Path, content: &str) -> Document {
    match Format::from_path(path) {
        Format::Md => doc::parse(content),
        Format::Org => crate::org::parse_org(content),
    }
}

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
    fn answer(digest: u64, names: &[String], known: Option<u64>) -> Self {
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
fn referenced_names_digest(names: &[String]) -> u64 {
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
struct MarkerResolutionGuard<'a> {
    graph: &'a Graph,
    path: PathBuf,
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
    legacy_key: String,
    edn_path: PathBuf,
    legacy_edn: Option<PathBuf>,
    merged: Vec<crate::pdf::Highlight>,
    primary_baseline: Option<String>,
    legacy_baseline: Option<String>,
    committed: String,
    area_source_key: String,
    deleted_areas: Vec<crate::pdf::Highlight>,
}

impl PdfHighlightSidecarCommit {
    pub(crate) fn merged(&self) -> &[crate::pdf::Highlight] {
        &self.merged
    }
}

/// Per-retained-resource serialization for every page or journal mutation.
///
/// Every writer of one retained resource shares this gate, so graph-text
/// identity transitions are totally ordered across all of them.
struct GraphTextWriteGate {
    /// Resource-wide serialization for graph-text identity validation, the
    /// corresponding filesystem transition, and retained-index publication.
    ///
    /// This is deliberately reentrant by thread: higher-level transactions
    /// (rename/merge/projection recovery) call the same low-level publication
    /// primitives while retaining one authority window. This lock decides the
    /// total order in which admitted writers change graph-text identity.
    identity_mutation: std::sync::Mutex<GraphTextIdentityMutationState>,
    identity_mutation_changed: std::sync::Condvar,
}

#[derive(Default)]
struct GraphTextIdentityMutationState {
    owner: Option<std::thread::ThreadId>,
    depth: usize,
    #[cfg(test)]
    waiters: usize,
    /// Resource-wide version of every graph-text identity transition observed
    /// under this authority. Per-Graph indexes may be reused only at this exact
    /// epoch; their scope and configuration remain instance-local.
    epoch: u64,
}

struct GraphTextIdentityMutationGuard<'a> {
    gate: &'a GraphTextWriteGate,
}

impl GraphTextWriteGate {
    fn new() -> Self {
        Self {
            identity_mutation: std::sync::Mutex::new(GraphTextIdentityMutationState::default()),
            identity_mutation_changed: std::sync::Condvar::new(),
        }
    }

    fn lock_identity_mutation(&self) -> GraphTextIdentityMutationGuard<'_> {
        let caller = std::thread::current().id();
        let mut state = self.identity_mutation.lock().unwrap();
        #[cfg(test)]
        let mut registered_waiter = false;
        while state.owner.as_ref().is_some_and(|owner| owner != &caller) {
            #[cfg(test)]
            if !registered_waiter {
                state.waiters += 1;
                registered_waiter = true;
                self.identity_mutation_changed.notify_all();
            }
            state = self.identity_mutation_changed.wait(state).unwrap();
        }
        #[cfg(test)]
        if registered_waiter {
            state.waiters -= 1;
        }
        if state.owner.is_none() {
            state.owner = Some(caller);
        }
        state.depth = state
            .depth
            .checked_add(1)
            .expect("graph-text identity mutation depth exhausted");
        GraphTextIdentityMutationGuard { gate: self }
    }

    fn identity_mutation_epoch(&self) -> u64 {
        self.identity_mutation.lock().unwrap().epoch
    }

    /// The resource epoch, read under this thread's own mutation authority.
    ///
    /// This is an internal precondition, not a threat-model refusal: a caller
    /// that does not hold the gate would be comparing two reads of a value
    /// another thread is free to advance between them, so the comparison means
    /// nothing. It used to be a `debug_assert`, which does not exist in the
    /// shipped release profile — see `graph_text_writers_take_the_identity_gate_before_any_page_lock`
    /// for the static proof that no production path reaches here without it.
    fn identity_mutation_epoch_under_authority(&self) -> io::Result<u64> {
        let caller = std::thread::current().id();
        let state = self.identity_mutation.lock().unwrap();
        if state.owner.as_ref() != Some(&caller) || state.depth == 0 {
            return Err(graph_text_admission_unavailable(
                "graph-text identity epoch read without this thread's mutation authority",
            ));
        }
        Ok(state.epoch)
    }

    fn advance_identity_mutation_epoch(&self) -> u64 {
        let caller = std::thread::current().id();
        let mut state = self.identity_mutation.lock().unwrap();
        debug_assert_eq!(state.owner.as_ref(), Some(&caller));
        debug_assert_ne!(state.depth, 0);
        state.epoch = state
            .epoch
            .checked_add(1)
            .expect("graph-text identity mutation epoch exhausted");
        state.epoch
    }
}

impl Drop for GraphTextIdentityMutationGuard<'_> {
    fn drop(&mut self) {
        let caller = std::thread::current().id();
        let mut state = self.gate.identity_mutation.lock().unwrap();
        debug_assert_eq!(state.owner.as_ref(), Some(&caller));
        debug_assert_ne!(state.depth, 0);
        state.depth = state.depth.saturating_sub(1);
        if state.depth == 0 {
            state.owner = None;
            self.gate.identity_mutation_changed.notify_all();
        }
    }
}

/// Process-local weak registry of independent writer gates. A live graph keeps
/// its gate alive; dead resources are pruned on the next open.
static GRAPH_TEXT_WRITE_GATE_REGISTRY: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<CanonicalGraphResourceId, std::sync::Weak<GraphTextWriteGate>>,
    >,
> = std::sync::OnceLock::new();

struct GraphTextWriteBinding {
    resource_id: CanonicalGraphResourceId,
    gate: Arc<GraphTextWriteGate>,
    root: Dir,
}

fn graph_text_write_binding_for_resource(
    root: &Path,
    projection_root: Option<&Dir>,
) -> io::Result<GraphTextWriteBinding> {
    graph_text_write_identity_acquisition_hook()?;
    let retained_root = match projection_root {
        Some(projection_root) => projection_root.try_clone()?,
        None => {
            let resolved = fs::canonicalize(root)?;
            Dir::open_ambient_dir(resolved, ambient_authority())?
        }
    };
    let resource_id = canonical_graph_resource_id(&retained_root)?;

    let registry = GRAPH_TEXT_WRITE_GATE_REGISTRY
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut registry = registry.lock().unwrap();
    registry.retain(|_, gate| gate.upgrade().is_some());
    if let Some(gate) = registry.get(&resource_id).and_then(|gate| gate.upgrade()) {
        return Ok(GraphTextWriteBinding {
            resource_id,
            gate,
            root: retained_root,
        });
    }

    let gate = Arc::new(GraphTextWriteGate::new());
    registry.insert(resource_id, Arc::downgrade(&gate));
    Ok(GraphTextWriteBinding {
        resource_id,
        gate,
        root: retained_root,
    })
}

fn graph_text_write_identity_mismatch_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "ambient graph root no longer names the retained graph text resource",
    )
}

/// An admitted graph-text writer: the retained root capability every write
/// resolves its paths under, bound to the resource identity it was admitted
/// against.
struct GraphTextWritePermit {
    root: Dir,
    resource_id: CanonicalGraphResourceId,
}

struct GraphTextTarget {
    chain: Vec<Dir>,
    filename: String,
}

impl GraphTextTarget {
    fn parent(&self) -> &Dir {
        self.chain
            .last()
            .expect("graph text target retains its parent chain")
    }
}

/// The exact live-path state shown to the user by a resolvable save conflict.
/// Bytes are retained beside this value in `ConflictAuthority`; keeping the
/// authority shape small makes it impossible to mistake ordinary load evidence
/// for override authority.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ConflictSnapshot {
    Present {
        revision: String,
        resource_identity: ContentDigest,
    },
    Absent,
}

/// Identity for one LIVE editor instance over a path.
///
/// Opaque to the frontend: it is minted by [`Graph::activate_editor`], carried
/// across the save transport, and compared for exact equality. It is deliberately
/// NOT derived from the page's content, revision or path, because the defect this
/// exists to close is precisely two different editors that agree on all three — a
/// cloned `PageDto` with the same `base_rev` spending the live editor's epoch.
///
/// Values are unique within one `Graph`. The registry lives on the `Graph`, so a
/// token minted against a different graph is simply not found: the graph binding
/// the spec requires is structural rather than a compared field.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EditorActivation(u64);

impl EditorActivation {
    pub fn as_u64(self) -> u64 {
        self.0
    }

    pub fn from_u64(raw: u64) -> Self {
        Self(raw)
    }
}

/// What an activation request means for a path that already has a live editor.
///
/// Path idempotence and same-path content replacement contradict each other
/// without this discriminator, which is why v5 of the spec was unimplementable at
/// the same-path row.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivationIntent {
    /// Plain re-hydration: return the live activation and mint nothing. Does not
    /// burn the incumbent's authority.
    Reuse,
    /// The working instance is genuinely being replaced (`reloadPage`,
    /// `reloadPageIfStillSafe`, PDF-notes refresh). Mints a new activation; the
    /// frontend retires the exact incumbent only after installing the replacement.
    Replace,
}

/// What presenting a conflict observation established. No arm writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictPresentation {
    /// This editor's observation was live and is now spent: the discard proceeds.
    Authorised,
    /// A newer observation exists. There is a live banner to answer, so the user
    /// answers it again rather than being told nothing happened.
    Superseded,
    /// The authority is gone with no successor — typically the raw-watcher path,
    /// which revokes without emitting any page event. The banner must be
    /// re-observed rather than left dead.
    Withdrawn,
}

/// A live editor instance registered against a path.
#[derive(Clone, Debug)]
struct ActivationRecord {
    activation: EditorActivation,
    /// Present editors are live for an existing file. Absent editors are live for
    /// a prospective target that does not exist yet; the target is re-resolved and
    /// compared at first save.
    prospective: bool,
    /// Exact source text this editor instance last loaded or successfully
    /// wrote. Unlike the graph cache or Concord ledger, an external watcher
    /// admission does not advance it; this is the true three-way base for a
    /// live save conflict.
    baseline: Option<String>,
}

#[derive(Default)]
struct EditorActivationState {
    next: u64,
    // Replacement activation is two-phase across an async frontend boundary:
    // B must become live before A can be retired. Keep every exact activation
    // for the path during that interval; the last record is the prospective
    // current instance returned by idempotent Reuse.
    live: std::collections::HashMap<PathBuf, Vec<ActivationRecord>>,
}

/// The outcome of activating an editor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EditorActivationHandle {
    pub activation: EditorActivation,
    /// The exact path this activation is live for. For an absent editor this is
    /// the prospective target resolved at activation time, which first save
    /// re-resolves and compares.
    pub target: String,
    /// True when the target did not exist at activation time. Holding it reserves
    /// nothing on disk, so activation creates no unrequested write.
    pub prospective: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConflictEditorEpisode {
    loaded_revision: Option<String>,
    /// The editor activation that observed the conflict.
    ///
    /// `None` is the editor-less writer (external import,
    /// sync-id migration, PDF-highlight write) and the pre-increment-3 caller.
    /// Those are legal on the ordinary path — the base-revision guard is their
    /// authority — and refused on the override path.
    activation: Option<EditorActivation>,
}

/// Which conflict a "Keep mine" is answering.
///
/// A force request must name the observation the user was actually SHOWN. The
/// path alone is not enough: authority for a NEWER, unseen winner can be minted
/// between the moment a request is issued and the moment it runs, and a request
/// that names only its path will happily consume it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConflictOverride {
    pub observation_epoch: u64,
}

/// Closed producer vocabulary for failures returned by a Direct Files save.
/// The strings are the stable diagnostic/retry contract; user-controlled error
/// prose is display-only and never participates in classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectSaveFailureCode {
    PrecheckSymlink,
    PrecheckInterrupted,
    PrecheckPortableCollision,
    PrecheckResourceAlias,
    PrecheckNotPortable,
    PrecheckNofollow,
    PrecheckLimit,
    IdentityOwnedElsewhere,
    IdentityNameTaken,
    ConflictRetrySaveBaselinePresent,
    ConflictRetrySaveBaselineAbsent,
    ConflictRetryCommitRecheck,
    ConflictRetryReplacePreRetirement,
    ConflictRetryReplaceRetiredMismatch,
    ConflictRetryReplacePublicationCollision,
    ConflictRetryCreatePublicationCollision,
    ConflictRetryFinalRereadAbsent,
    ConflictRetryFinalRereadPresent,
    ConflictRetryReplacePostPublication,
    ConflictAuthoritySuperseded,
    ConflictAuthorityOtherEpisode,
    ConflictAuthoritySpent,
    ConflictSaveBaselinePresent,
    ConflictSaveBaselineAbsent,
    ConflictCommitRecheck,
    ConflictReplacePreRetirement,
    ConflictReplaceRetiredMismatch,
    ConflictReplacePublicationCollision,
    ConflictCreatePublicationCollision,
    ConflictFinalRereadAbsent,
    ConflictFinalRereadPresent,
    ConflictReplacePostPublication,
    ConflictPinnedOwner,
    ConflictBaseRev,
    Unknown,
}

impl DirectSaveFailureCode {
    pub const ALL: [Self; 35] = [
        Self::PrecheckSymlink,
        Self::PrecheckInterrupted,
        Self::PrecheckPortableCollision,
        Self::PrecheckResourceAlias,
        Self::PrecheckNotPortable,
        Self::PrecheckNofollow,
        Self::PrecheckLimit,
        Self::IdentityOwnedElsewhere,
        Self::IdentityNameTaken,
        Self::ConflictRetrySaveBaselinePresent,
        Self::ConflictRetrySaveBaselineAbsent,
        Self::ConflictRetryCommitRecheck,
        Self::ConflictRetryReplacePreRetirement,
        Self::ConflictRetryReplaceRetiredMismatch,
        Self::ConflictRetryReplacePublicationCollision,
        Self::ConflictRetryCreatePublicationCollision,
        Self::ConflictRetryFinalRereadAbsent,
        Self::ConflictRetryFinalRereadPresent,
        Self::ConflictRetryReplacePostPublication,
        Self::ConflictAuthoritySuperseded,
        Self::ConflictAuthorityOtherEpisode,
        Self::ConflictAuthoritySpent,
        Self::ConflictSaveBaselinePresent,
        Self::ConflictSaveBaselineAbsent,
        Self::ConflictCommitRecheck,
        Self::ConflictReplacePreRetirement,
        Self::ConflictReplaceRetiredMismatch,
        Self::ConflictReplacePublicationCollision,
        Self::ConflictCreatePublicationCollision,
        Self::ConflictFinalRereadAbsent,
        Self::ConflictFinalRereadPresent,
        Self::ConflictReplacePostPublication,
        Self::ConflictPinnedOwner,
        Self::ConflictBaseRev,
        Self::Unknown,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PrecheckSymlink => "precheck.symlink",
            Self::PrecheckInterrupted => "precheck.interrupted",
            Self::PrecheckPortableCollision => "precheck.portable_collision",
            Self::PrecheckResourceAlias => "precheck.resource_alias",
            Self::PrecheckNotPortable => "precheck.not_portable",
            Self::PrecheckNofollow => "precheck.nofollow",
            Self::PrecheckLimit => "precheck.limit",
            Self::IdentityOwnedElsewhere => "identity.owned_elsewhere",
            Self::IdentityNameTaken => "identity.name_taken",
            Self::ConflictRetrySaveBaselinePresent => "conflict_retry.save_baseline_present",
            Self::ConflictRetrySaveBaselineAbsent => "conflict_retry.save_baseline_absent",
            Self::ConflictRetryCommitRecheck => "conflict_retry.commit_recheck",
            Self::ConflictRetryReplacePreRetirement => "conflict_retry.replace_pre_retirement",
            Self::ConflictRetryReplaceRetiredMismatch => "conflict_retry.replace_retired_mismatch",
            Self::ConflictRetryReplacePublicationCollision => {
                "conflict_retry.replace_publication_collision"
            }
            Self::ConflictRetryCreatePublicationCollision => {
                "conflict_retry.create_publication_collision"
            }
            Self::ConflictRetryFinalRereadAbsent => "conflict_retry.final_reread_absent",
            Self::ConflictRetryFinalRereadPresent => "conflict_retry.final_reread_present",
            Self::ConflictRetryReplacePostPublication => "conflict_retry.replace_post_publication",
            Self::ConflictAuthoritySuperseded => "conflict_authority.superseded",
            Self::ConflictAuthorityOtherEpisode => "conflict_authority.other_episode",
            Self::ConflictAuthoritySpent => "conflict_authority.spent",
            Self::ConflictSaveBaselinePresent => "conflict.save_baseline_present",
            Self::ConflictSaveBaselineAbsent => "conflict.save_baseline_absent",
            Self::ConflictCommitRecheck => "conflict.commit_recheck",
            Self::ConflictReplacePreRetirement => "conflict.replace_pre_retirement",
            Self::ConflictReplaceRetiredMismatch => "conflict.replace_retired_mismatch",
            Self::ConflictReplacePublicationCollision => "conflict.replace_publication_collision",
            Self::ConflictCreatePublicationCollision => "conflict.create_publication_collision",
            Self::ConflictFinalRereadAbsent => "conflict.final_reread_absent",
            Self::ConflictFinalRereadPresent => "conflict.final_reread_present",
            Self::ConflictReplacePostPublication => "conflict.replace_post_publication",
            Self::ConflictPinnedOwner => "conflict.pinned_owner",
            Self::ConflictBaseRev => "conflict.base_rev",
            Self::Unknown => "unknown",
        }
    }
}

/// Typed inner error retained inside the public `io::Error` save surface.
#[derive(Debug)]
pub struct DirectSaveError {
    code: DirectSaveFailureCode,
    conflict_epoch: Option<u64>,
    source: io::Error,
}

impl DirectSaveError {
    pub fn into_io(code: DirectSaveFailureCode, source: io::Error) -> io::Error {
        Self::into_io_with_conflict_epoch(code, None, source)
    }

    pub fn into_io_with_conflict_epoch(
        code: DirectSaveFailureCode,
        conflict_epoch: Option<u64>,
        source: io::Error,
    ) -> io::Error {
        let kind = source.kind();
        io::Error::new(
            kind,
            Self {
                code,
                conflict_epoch,
                source,
            },
        )
    }

    pub fn ensure_io(source: io::Error) -> io::Error {
        if source
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<Self>())
            .is_some()
        {
            source
        } else {
            Self::into_io(DirectSaveFailureCode::Unknown, source)
        }
    }

    pub const fn code(&self) -> DirectSaveFailureCode {
        self.code
    }

    pub const fn conflict_epoch(&self) -> Option<u64> {
        self.conflict_epoch
    }
}

impl fmt::Display for DirectSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for DirectSaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// App-private recovery material for a live Direct Files conflict. The caller
/// persists this outside the graph so an unresolved draft survives navigation,
/// a clean shutdown, or a process crash. `disk_rev` is the exact revision the
/// review was computed against; resolution rechecks it under the page lock.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveSaveConflictCapture {
    pub diff: crate::sync_diff::SyncConflictDiff,
    pub base_text: Option<String>,
    pub disk_rev: String,
}

#[derive(Clone, Debug)]
struct ConflictAuthority {
    snapshot: ConflictSnapshot,
    bytes: Option<String>,
    editor_episode: ConflictEditorEpisode,
    observation_epoch: u64,
}

#[derive(Default)]
struct ConflictAuthorityState {
    observation_epochs: std::collections::HashMap<PathBuf, u64>,
    tokens: std::collections::HashMap<PathBuf, ConflictAuthority>,
}

#[derive(Clone, Copy)]
enum EditorConflictSite {
    SaveBaselinePresent,
    SaveBaselineAbsent,
    CommitRecheck,
    ReplacePreRetirement,
    ReplaceRetiredMismatch,
    ReplacePublicationCollision,
    CreatePublicationCollision,
    FinalRereadAbsent,
    FinalRereadPresent,
    ReplacePostPublication,
}

impl EditorConflictSite {
    /// Every conflict-minting site. Exhaustive by construction: the length is
    /// pinned, so adding a variant without adding it here fails to compile and
    /// the site-to-code guards cannot silently stop covering it.
    #[cfg(test)]
    const ALL: [Self; 10] = [
        Self::SaveBaselinePresent,
        Self::SaveBaselineAbsent,
        Self::CommitRecheck,
        Self::ReplacePreRetirement,
        Self::ReplaceRetiredMismatch,
        Self::ReplacePublicationCollision,
        Self::CreatePublicationCollision,
        Self::FinalRereadAbsent,
        Self::FinalRereadPresent,
        Self::ReplacePostPublication,
    ];

    fn message(self) -> &'static str {
        match self {
            Self::SaveBaselinePresent => "editor conflict: save baseline present",
            Self::SaveBaselineAbsent => "editor conflict: save baseline absent",
            Self::CommitRecheck => "editor conflict: commit recheck",
            Self::ReplacePreRetirement => "editor conflict: replace pre-retirement",
            Self::ReplaceRetiredMismatch => "editor conflict: retired mismatch",
            Self::ReplacePublicationCollision => "editor conflict: publication collision",
            Self::CreatePublicationCollision => "editor conflict: create publication collision",
            Self::FinalRereadAbsent => "editor conflict: final reread absent",
            Self::FinalRereadPresent => "editor conflict: final reread present",
            Self::ReplacePostPublication => "editor conflict: post-publication validation",
        }
    }

    fn tokenless_message(self) -> &'static str {
        match self {
            Self::SaveBaselinePresent => "tokenless editor conflict: save baseline present",
            Self::SaveBaselineAbsent => "tokenless editor conflict: save baseline absent",
            Self::CommitRecheck => "tokenless editor conflict: commit recheck",
            Self::ReplacePreRetirement => "tokenless editor conflict: replace pre-retirement",
            Self::ReplaceRetiredMismatch => "tokenless editor conflict: retired mismatch",
            Self::ReplacePublicationCollision => "tokenless editor conflict: publication collision",
            Self::CreatePublicationCollision => {
                "tokenless editor conflict: create publication collision"
            }
            Self::FinalRereadAbsent => "tokenless editor conflict: final reread absent",
            Self::FinalRereadPresent => "tokenless editor conflict: final reread present",
            Self::ReplacePostPublication => {
                "tokenless editor conflict: post-publication validation"
            }
        }
    }

    fn conflict_code(self) -> DirectSaveFailureCode {
        match self {
            Self::SaveBaselinePresent => DirectSaveFailureCode::ConflictSaveBaselinePresent,
            Self::SaveBaselineAbsent => DirectSaveFailureCode::ConflictSaveBaselineAbsent,
            Self::CommitRecheck => DirectSaveFailureCode::ConflictCommitRecheck,
            Self::ReplacePreRetirement => DirectSaveFailureCode::ConflictReplacePreRetirement,
            Self::ReplaceRetiredMismatch => DirectSaveFailureCode::ConflictReplaceRetiredMismatch,
            Self::ReplacePublicationCollision => {
                DirectSaveFailureCode::ConflictReplacePublicationCollision
            }
            Self::CreatePublicationCollision => {
                DirectSaveFailureCode::ConflictCreatePublicationCollision
            }
            Self::FinalRereadAbsent => DirectSaveFailureCode::ConflictFinalRereadAbsent,
            Self::FinalRereadPresent => DirectSaveFailureCode::ConflictFinalRereadPresent,
            Self::ReplacePostPublication => DirectSaveFailureCode::ConflictReplacePostPublication,
        }
    }

    fn tokenless_code(self) -> DirectSaveFailureCode {
        match self {
            Self::SaveBaselinePresent => DirectSaveFailureCode::ConflictRetrySaveBaselinePresent,
            Self::SaveBaselineAbsent => DirectSaveFailureCode::ConflictRetrySaveBaselineAbsent,
            Self::CommitRecheck => DirectSaveFailureCode::ConflictRetryCommitRecheck,
            Self::ReplacePreRetirement => DirectSaveFailureCode::ConflictRetryReplacePreRetirement,
            Self::ReplaceRetiredMismatch => {
                DirectSaveFailureCode::ConflictRetryReplaceRetiredMismatch
            }
            Self::ReplacePublicationCollision => {
                DirectSaveFailureCode::ConflictRetryReplacePublicationCollision
            }
            Self::CreatePublicationCollision => {
                DirectSaveFailureCode::ConflictRetryCreatePublicationCollision
            }
            Self::FinalRereadAbsent => DirectSaveFailureCode::ConflictRetryFinalRereadAbsent,
            Self::FinalRereadPresent => DirectSaveFailureCode::ConflictRetryFinalRereadPresent,
            Self::ReplacePostPublication => {
                DirectSaveFailureCode::ConflictRetryReplacePostPublication
            }
        }
    }
}

pub struct Graph {
    pub root: PathBuf,
    /// Retained no-follow identity of the graph root. Projection writes fail
    /// closed when this capability could not be established at graph open.
    projection_root: Option<Dir>,
    /// Graph-relative live names whose editor-publication claimants could not
    /// be reconciled during the checked-open walk. Journal replay must never
    /// interpret one of these absences as an external deletion (I2c).
    interrupted_publication_claimants: RwLock<std::collections::BTreeSet<GraphTextPath>>,
    /// The canonical filesystem capability used for every asset operation. For
    /// ordinary graphs this is `<root>/assets`; when the runtime has explicitly
    /// approved an external assets symlink/junction it is that exact resolved
    /// directory. No other graph path may use this capability.
    assets_root: PathBuf,
    pub config: Config,
    /// Sole versioned eligibility policy for normal graph text discovery and
    /// exact existing-file access. It grants no creation/projection authority.
    graph_text_scope: GraphTextScope,
    /// Exact bytes from which this scan-capable instance derived its scope and
    /// configured text roots. A scan must require a fresh Graph when the case-insensitive
    /// on-disk config path no longer has this description.
    reconciliation_scan_open_config_description: Option<BlobDescription>,
    /// Digest of the configuration bytes THIS instance last published.
    ///
    /// The watcher cannot otherwise tell Tine's own settings write from an
    /// outside one, and would reopen the whole graph — discarding every cache
    /// it has built — every time the user toggles a star.
    recent_config_write: RwLock<Option<BlobDescription>>,
    /// Unforgeable identity of this exact Graph instance. Reopening the same
    /// resource intentionally produces a different token.
    graph_text_admission_instance: Arc<GraphTextAdmissionInstance>,
    /// Complete graph-text identity evidence retained specifically for ordinary
    /// guarded writes. The legacy watcher records exact paths or uncertainty
    /// here before its deferred cache reconciliation.
    guarded_graph_text_identity: RwLock<GuardedGraphTextIdentityState>,
    /// Journal date formats (filename + title) resolved from `config.edn`, used to
    /// recognize journal files in the user's format and render new ones. Built once
    /// at open (config changes need a reopen, as in OG).
    pub journal_format: JournalFormat,
    /// In-memory cache of every parsed page, keyed implicitly by position.
    /// Built once on first whole-graph query and kept in sync by edits, so
    /// search / backlinks / `{{query}}` scan memory instead of re-reading and
    /// re-parsing the entire tree on every keystroke. `None` = not yet built.
    // `Arc<Document>` so a cache snapshot or a save's scoped-invalidation copy is
    // an O(1) refcount bump, not a deep clone of the whole page (see cache_upsert).
    cache: RwLock<Option<Arc<Vec<(PageEntry, Arc<Document>)>>>>,
    /// Compact runtime IDs for exact revisions published during this session.
    /// Page loading and disposable projection recovery share this owner; neither
    /// needs to retain a Document or trust a damaged database to recover IDs.
    session_page_ids: RwLock<std::collections::HashMap<PathBuf, SessionPageIds>>,
    /// One source-inventory repair at a time. Joiners return to readiness
    /// admission without retaining a snapshot or waiting under a graph lock.
    projection_recovery: std::sync::Mutex<()>,
    /// Graph-relative paths of pages skipped by the latest whole-graph cache
    /// build because their parse/projection panicked. Kept retrievable so an
    /// lsdoc ownership gap can never degrade search completeness invisibly.
    page_index_failures: RwLock<Vec<String>>,
    /// Companion indexes for `cache`: the logical `(kind, page_key(name)) -> Vec
    /// slot` index preserves deterministic first-wins lookup, while the exact-path
    /// index keeps cache ownership physical. The Vec stays the source of truth for
    /// whole-graph iteration. `None` means "rebuild from the Vec on next lookup"
    /// and is preferred over risking a stale slot after broad mutations.
    cache_index: RwLock<Option<PageCacheIndex>>,
    /// Generation-bound effective ownership and parse-failure evidence derived
    /// from the warm physical-owner cache. Name-only creation uses this exact
    /// generation plus target-local no-replace validation; raw watcher events
    /// block creation until their debounced reconciliation has advanced it.
    effective_identity_index: RwLock<Option<Arc<EffectiveIdentityIndex>>>,
    /// Bumped on every cache mutation (upsert/remove). The lock-free cache build
    /// captures this before reading disk and rebuilds if a mutation raced it
    /// (which would otherwise install stale content over a concurrent save).
    cache_gen: std::sync::atomic::AtomicU64,
    /// Raw watcher callbacks publish an O(1) admission barrier before their
    /// debounced reconciliation. The app registry admits only one Graph slot per
    /// canonical root, so this frontier is instance-local and cannot be cleared
    /// by a different cache. Name-only creation refuses while the two epochs
    /// differ; existing exact-owner saves keep their path-local validation.
    external_observation_epoch: std::sync::atomic::AtomicU64,
    external_reconciled_epoch: std::sync::atomic::AtomicU64,
    external_observation_instance: u64,
    /// One explicit whole-graph cache-build flight. Owners parse without holding
    /// this mutex; joiners wait on the flight's own notification and therefore
    /// never wait while holding cache or index locks.
    page_build_flight: std::sync::Mutex<Option<Arc<PageBuildFlight>>>,
    #[cfg(test)]
    page_build_test: PageBuildTestState,
    /// Memoized reference results (backlinks and unlinked references), keyed by `(cache_gen, today)` so it self-invalidates on ANY
    /// cache mutation and on a date rollover (relative-date queries depend on
    /// today). Lets a re-render, a second component showing the same query, or
    /// navigating back to a page recompute nothing; never serves a stale result.
    derived_cache: RwLock<Option<DerivedCache>>,
    /// Disposable SQLite facts for Direct Files. Markdown/Org and the parsed
    /// page cache remain authoritative; indexed reads are admitted only when
    /// this worker has published the exact current `cache_gen`.
    direct_projection: std::sync::Mutex<Option<Arc<crate::direct_projection::DirectProjection>>>,
    /// Memoized `list_pages()` (the journals//pages/ directory scan), keyed by
    /// cache_gen — which bumps on every page create/delete/rename (Tine or watcher)
    /// — so quick-switch / [[ ]] autocomplete don't re-read both dirs on every
    /// keystroke. An externally-created page not yet seen by the watcher is at most
    /// one watcher tick (≤3s) stale here.
    page_list_cache: RwLock<Option<(u64, Vec<PageEntry>)>>,
    /// Memoized exact `find_entry(name, kind)` resolution, keyed by `cache_gen`.
    /// Unlike `list_pages()`, this index is built from raw `list_md` output so it
    /// preserves `find_entry`'s duplicate selection: date-stem file first, else
    /// first directory-walk match.
    find_entry_cache: RwLock<Option<(u64, FindEntryIndex)>>,
    /// `path → content_rev` of the bytes Tine last wrote to each page file,
    /// recorded *before* the write lands on disk. The file watcher reads files
    /// outside the cache lock, so during the window between a save's atomic rename
    /// and its `cache_upsert` it can read disk-ahead-of-cache and mistake Tine's
    /// own write for an external change. This lets the watcher recognize the exact
    /// bytes we wrote and suppress that false positive (the parse-cache comparison
    /// alone races that window). See `write_page` / `sync_file_content`.
    recent_writes: std::sync::Mutex<std::collections::HashMap<PathBuf, String>>,
    /// Recent exact Direct Files states which the native watcher may still echo.
    /// Unlike `recent_writes`, the first receipt is minted only after Tine's
    /// final no-follow reread proved both the published bytes and physical file
    /// identity. Successful debounced reconciliation replaces it with the exact
    /// accepted final state, so delayed duplicate callbacks remain no-ops while
    /// an old state can never regain authority after a newer state was admitted.
    /// The raw callback reopens only candidate paths under the same page lock and
    /// may omit the external-change frontier only when both identity and revision
    /// still match.
    recent_graph_text_states:
        std::sync::Mutex<std::collections::HashMap<PathBuf, ExactGraphTextStateReceipt>>,
    /// Concord base ledger (ADR 0056): the per-page last text Tine agreed on
    /// with the disk, updated best-effort after successful saves and external-
    /// change admissions. A disposable cache stored OUTSIDE the sync tree;
    /// unset (most tests) makes every hook a no-op. Never
    /// consulted on the save critical path — only by conflict diffs.
    concord_ledger: std::sync::OnceLock<Arc<crate::concord_ledger::ConcordLedger>>,
    /// The exact page files currently being rewritten as the DIRECT result of a
    /// user's VCS-marker resolution (Concord L5, `resolve_vcs_marker_conflict`).
    /// Concord invariant 3 says Tine never rewrites a marker-bearing file — the
    /// one exception is the resolution the user just confirmed, which REMOVES
    /// the markers. Scoping the exception to an exact path (held only across the
    /// one guarded write, under that page's lock) means a concurrent editor save
    /// to any OTHER marker-bearing page is still refused. See
    /// `serialize_page_document`.
    marker_resolutions: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    /// `path → content_rev` of the on-disk bytes the cached page's
    /// `Document` was parsed from. Invariant: an entry exists IFF the page is in
    /// the cache, and `disk_revs[path] == content_rev(current disk bytes)` ⟹ the
    /// cached doc reflects disk (is fresh). Lets `sync_file_content` skip the
    /// parse→serialize→parse freshness comparison when a file is unchanged — the
    /// common case on every page navigation and most watcher polls. A missing or
    /// mismatched entry always falls through to the correct parse-compare path, so
    /// the worst a desync can cause is redundant work, never a stale serve.
    disk_revs: RwLock<std::collections::HashMap<PathBuf, String>>,
    /// Exact no-follow file identity observed for a successfully parsed load,
    /// bound to its content revision. Existing-file saves require the same
    /// identity and bytes; this is discovery/read evidence, never creation
    /// authority.
    loaded_file_identities: RwLock<std::collections::HashMap<PathBuf, (String, ContentDigest)>>,
    /// One-shot authority minted only by a coherent editor-conflict observation.
    /// This is deliberately separate from `loaded_file_identities`: ordinary
    /// loads are evidence for ordinary saves, never permission to overwrite.
    conflict_authority: std::sync::Mutex<ConflictAuthorityState>,
    /// Live editor activations, keyed by the exact path each is live for.
    ///
    /// Deliberately a registry on the `Graph` rather than a field of any page
    /// value: a token stored inside a page object is copied by every clone,
    /// snapshot and DTO round-trip, and a copy would then claim an identity it
    /// does not have (see the frontend's `clonePages`/history snapshots).
    editor_activations: std::sync::Mutex<EditorActivationState>,
    /// Per-resolved-path write locks. The same page file has TWO in-process
    /// writers — the editor (`save_page`/`write_page`) and the PDF highlight path
    /// (`write_highlights`, for an `hls__` page) — and a rename rewrites many
    /// files at once. Holding the per-path lock across the whole
    /// read→conflict-check→write→`cache_upsert` makes same-page writes serialize,
    /// so they can't clobber each other or leave a stale self-write marker.
    /// Lock order is ALWAYS page_lock → cache → disk_revs; never the reverse.
    page_locks:
        std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Arc<std::sync::Mutex<()>>>>,
    /// Resource-scoped shared admission boundary for all page/journal
    /// writers. Identity acquisition failure is retained as an error so an open
    /// can never fall back to an unshared gate.
    graph_text_write_binding: io::Result<GraphTextWriteBinding>,
    /// Per-UI-lane cancellation epochs for whole-graph text searches. Starting a
    /// newer search makes its superseded prefix stop promptly.
    search_lanes: std::sync::Mutex<
        std::collections::HashMap<String, std::sync::Arc<std::sync::atomic::AtomicU64>>,
    >,
}

/// Outcome of the checked-open interrupted-publication walk. The surviving
/// claimant set is deliberately path-based: it is consulted only to prevent an
/// absent journal target from being misclassified as an external deletion.
#[derive(Debug, Default)]
pub struct RecoverySummary {
    reconciled: usize,
    claimants: std::collections::BTreeSet<GraphTextPath>,
}

impl RecoverySummary {
    fn record_claimant(&mut self, graph: &Graph, target: &Path) -> io::Result<()> {
        let relative = target.strip_prefix(&graph.root).map_err(|_| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "editor recovery claimant target escapes the graph",
            )
        })?;
        let relative = relative.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "editor recovery claimant target is not UTF-8",
            )
        })?;
        let portable = GraphTextPath::parse(relative).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("editor recovery claimant target is not portable: {error}"),
            )
        })?;
        self.claimants.insert(portable);
        Ok(())
    }
}

static NEXT_EXTERNAL_OBSERVATION_INSTANCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

/// Opaque acknowledgement ticket for one exact `Graph` instance's raw watcher
/// frontier. A same-root reopen cannot consume a ticket minted by its retired
/// predecessor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphTextExternalObservationTicket {
    instance: u64,
    epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExactGraphTextStateReceipt {
    revision: String,
    resource_identity: ContentDigest,
}

impl GraphTextExternalObservationTicket {
    pub fn later_for_same_instance(self, other: Self) -> Option<Self> {
        (self.instance == other.instance).then_some(if self.epoch >= other.epoch {
            self
        } else {
            other
        })
    }
}

#[derive(Debug)]
struct GraphTextAdmissionInstance;

#[derive(Default)]
struct GuardedGraphTextIdentityState {
    index: Option<Arc<CompleteGraphTextAdmissionIndex>>,
    invalidated: bool,
    invalidation_cause: Option<String>,
    /// Epoch of the shared resource state represented by `index`. Before the
    /// first lazy build, this records the epoch observed at Graph open so the
    /// warm cache is reusable only if no sibling transition intervened.
    observed_resource_epoch: Option<u64>,
    generation: u64,
    /// Always recorded, NOT `#[cfg(test)]`. A complete rebuild of this index is
    /// the dominant cost of a save on a large graph, and "how many times did it
    /// rebuild?" is the first question any slow-save report raises. A counter
    /// that exists only in the test binary cannot answer that question on the
    /// machine that has the problem -- which is exactly how a recovery
    /// investigation burned a full diagnostic cycle on 2026-08-05/06.
    complete_builds: usize,
    exact_updates: usize,
    /// Cost of the most recent complete rebuild, split into its two phases.
    /// Durations and counts only -- never a path and never file content, so this
    /// is safe to surface from a user's own graph.
    last_build: Option<GuardedGraphTextIdentityBuild>,
}

/// One complete rebuild of the guarded graph-text admission index, measured.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GuardedGraphTextIdentityBuild {
    /// Two-pass whole-graph retained capture.
    pub capture: std::time::Duration,
    /// Admission-index construction, including the per-document parse when
    /// `decode_semantics` is set.
    pub index: std::time::Duration,
    pub decode_semantics: bool,
    pub captured_entries: usize,
    pub captured_bytes: u64,
}

/// Always-on report of what the guarded graph-text identity index has cost.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GuardedGraphTextIdentityReport {
    pub complete_builds: usize,
    pub exact_updates: usize,
    pub invalidated: bool,
    pub generation: u64,
    pub last_build: Option<GuardedGraphTextIdentityBuild>,
}

struct GraphTextParseBudgetPermit {
    semantic_name_bytes: u64,
    semantic_name_allocation_bytes: u64,
}

/// Core classification for one exact graph-relative platform event path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum GraphTextExactFeedPathClass {
    /// The path is wholly within a fixed or configured excluded subtree.
    Excluded,
    /// The exact path may affect retained file/resource evidence.
    RetainedFile,
    /// `logseq/config.edn` changes always require a fresh Graph instance.
    Configuration,
}

#[derive(Clone, Debug)]
struct GraphTextAdmissionRecord {
    description: BlobDescription,
    file_resource_id: ContentDigest,
    link_count: u64,
    semantic: PageEntry,
    format: Format,
    /// Whether `semantic` came from parsing this file's bytes, rather than from
    /// the page cache or from the filename alone.
    ///
    /// A rebuild reuses a prior record's `semantic` when this is set and the
    /// prior `description` (a SHA-256 of the content, plus its length) equals
    /// the freshly captured one — the bytes are identical, so the parse result
    /// is too. Without the flag the reuse would launder a cache-derived guess
    /// into something later builds treat as parsed.
    semantic_parsed: bool,
}

#[derive(Clone)]
struct GraphTextAdmissionTombstone {
    prior_record: Option<Arc<GraphTextAdmissionRecord>>,
    prior_file_resource_id: ContentDigest,
    prior_link_count: u64,
}

#[derive(Clone)]
struct CompleteGraphTextAdmissionIndex {
    instance: Arc<GraphTextAdmissionInstance>,
    scope_binding: GraphTextScopeBinding,
    graph_resource: CanonicalGraphResourceId,
    generation: u64,
    files_by_exact_path: PersistentMap<GraphTextPath, GraphTextAdmissionRecord>,
    paths_by_portable_key:
        PersistentMap<PortablePathKey, std::collections::BTreeSet<GraphTextPath>>,
    paths_by_file_resource: PersistentMap<ContentDigest, std::collections::BTreeSet<String>>,
    file_resource_by_exact_relative: PersistentMap<String, ContentDigest>,
    file_link_count_by_exact_relative: PersistentMap<String, u64>,
    file_is_graph_text_by_exact_relative: PersistentMap<String, bool>,
    paths_by_semantic_key: PersistentMap<(u8, String), std::collections::BTreeSet<GraphTextPath>>,
    tombstones_by_exact_path: PersistentMap<GraphTextPath, GraphTextAdmissionTombstone>,
    directories_by_exact_relative: PersistentMap<String, ContentDigest>,
    permanent_bytes: u64,
    permanent_limit: u64,
    peak_limit: u64,
}

struct PreparedGraphTextAdmissionUpsert {
    relative: String,
    description: BlobDescription,
    file_resource_id: ContentDigest,
    link_count: u64,
    retained_growth: u64,
    eligible: Option<(GraphTextPath, GraphTextAdmissionRecord)>,
}

struct PreparedGraphTextAdmissionRemove {
    relative: String,
    retained_growth: u64,
}

enum PreparedGraphTextAdmissionFinalState {
    Present(PreparedGraphTextAdmissionUpsert),
    Absent(PreparedGraphTextAdmissionRemove),
}

#[derive(Default)]
struct GraphTextExactFeedBatchActualCharges {
    raw_bytes: u64,
    prepared_growth: u64,
}

impl GraphTextExactFeedBatchActualCharges {
    fn remaining_raw(&self) -> io::Result<u64> {
        MAX_GRAPH_TEXT_EXACT_FEED_BATCH_RAW_BYTES
            .checked_sub(self.raw_bytes)
            .ok_or_else(|| graph_text_capture_limit_error("exact feed batch aggregate raw bytes"))
    }

    fn live_preparation_bytes(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        batch_scratch: u64,
    ) -> io::Result<u64> {
        checked_add_bytes(index.permanent_bytes, batch_scratch)
            .and_then(|live| checked_add_bytes(live, self.prepared_growth))
    }

    fn remaining_peak(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        batch_scratch: u64,
    ) -> io::Result<u64> {
        index
            .peak_limit
            .checked_sub(self.live_preparation_bytes(index, batch_scratch)?)
            .ok_or_else(|| graph_text_capture_limit_error("peak build memory"))
    }

    fn ensure_work_peak(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        batch_scratch: u64,
        working_bytes: u64,
    ) -> io::Result<()> {
        ensure_graph_text_peak_limit(
            self.live_preparation_bytes(index, batch_scratch)?,
            working_bytes,
            index.peak_limit,
        )
    }

    fn reserve_raw(
        &mut self,
        index: &CompleteGraphTextAdmissionIndex,
        batch_scratch: u64,
        raw_bytes: u64,
    ) -> io::Result<()> {
        if raw_bytes > self.remaining_raw()? {
            return Err(graph_text_capture_limit_error(
                "exact feed batch aggregate raw bytes",
            ));
        }
        self.raw_bytes = checked_add_bytes(self.raw_bytes, raw_bytes)?;
        // `raw_bytes` is an aggregate admission cap, not live memory: each
        // touched file is read, parsed, and dropped before the next. Only this
        // file's buffer coexists with previously retained prepared records.
        self.ensure_work_peak(index, batch_scratch, raw_bytes)
    }

    fn ensure_permanent_growth(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        growth: u64,
    ) -> io::Result<()> {
        let permanent = checked_add_bytes(index.permanent_bytes, self.prepared_growth)
            .and_then(|bytes| checked_add_bytes(bytes, growth))?;
        if permanent > index.permanent_limit {
            return Err(graph_text_capture_limit_error("permanent index memory"));
        }
        Ok(())
    }

    fn retain_prepared_growth(
        &mut self,
        index: &CompleteGraphTextAdmissionIndex,
        batch_scratch: u64,
        growth: u64,
    ) -> io::Result<()> {
        self.ensure_permanent_growth(index, growth)?;
        let next = checked_add_bytes(self.prepared_growth, growth)?;
        let prior = self.prepared_growth;
        self.prepared_growth = next;
        if let Err(error) = self.ensure_work_peak(index, batch_scratch, 0) {
            self.prepared_growth = prior;
            return Err(error);
        }
        Ok(())
    }
}

struct PageCacheIndex {
    by_name: std::collections::HashMap<(PageKind, String), usize>,
    by_path: std::collections::HashMap<PathBuf, usize>,
}

struct EffectiveIdentityIndex {
    generation: std::sync::atomic::AtomicU64,
    owners: std::collections::HashMap<(PageKind, String), Vec<PageEntry>>,
    physical_paths: std::collections::HashSet<PathBuf>,
    failures: Vec<String>,
}

impl Clone for EffectiveIdentityIndex {
    fn clone(&self) -> Self {
        Self {
            generation: std::sync::atomic::AtomicU64::new(self.generation()),
            owners: self.owners.clone(),
            physical_paths: self.physical_paths.clone(),
            failures: self.failures.clone(),
        }
    }
}

impl EffectiveIdentityIndex {
    fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    fn retag_generation(&self, previous: u64, next: u64) -> bool {
        self.generation
            .compare_exchange(
                previous,
                next,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }
}

/// Single-use evidence for an ordinary Direct Files creation. The census owns
/// only exact names and fingerprints; graph bytes are streamed through one
/// fixed buffer and are never retained here.
struct DirectCreationProof {
    target: GraphTextPath,
    generation: u64,
}

enum DirectCreationEvidence {
    Cold,
    Warm {
        generation: u64,
        identity_index: Arc<EffectiveIdentityIndex>,
    },
}

pub(crate) struct ReferenceCandidatePages {
    pub pages: Vec<(PageEntry, Arc<Document>)>,
    /// The referring blocks, when the index named them. `None` means "classify
    /// every block of every candidate page", which is what every caller did
    /// before this field existed, so the walk is the behaviour a partial or
    /// absent index falls back to rather than a lossy shortcut.
    pub blocks: Option<std::collections::HashSet<[u8; 16]>>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub indexed: bool,
    #[cfg_attr(not(test), allow(dead_code))]
    pub full_page_count: usize,
}

#[derive(Default)]
struct PageCacheBuild {
    pages: Vec<ParsedPage>,
    failures: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageCacheInstallOutcome {
    Installed,
    AlreadyAvailable,
    GenerationDrift,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageBuildOutcome {
    Installed,
    AlreadyAvailable,
    GenerationDrift,
    Cancelled,
    Failed,
}

impl PageBuildOutcome {
    fn installed(self) -> bool {
        matches!(self, Self::Installed | Self::AlreadyAvailable)
    }

    fn creation_error(self) -> io::Error {
        let message = match self {
            Self::GenerationDrift => "Direct creation identity repair crossed a cache generation",
            Self::Cancelled => "Direct creation identity repair joined a cancelled cache build",
            Self::Failed => "Direct creation identity repair joined a failed cache build",
            Self::Installed | Self::AlreadyAvailable => {
                "Direct creation identity repair completed without coherent evidence"
            }
        };
        io::Error::new(io::ErrorKind::Interrupted, message)
    }
}

impl From<PageCacheInstallOutcome> for PageBuildOutcome {
    fn from(outcome: PageCacheInstallOutcome) -> Self {
        match outcome {
            PageCacheInstallOutcome::Installed => Self::Installed,
            PageCacheInstallOutcome::AlreadyAvailable => Self::AlreadyAvailable,
            PageCacheInstallOutcome::GenerationDrift => Self::GenerationDrift,
        }
    }
}

struct PageBuildFlight {
    expected_generation: u64,
    outcome: std::sync::Mutex<Option<PageBuildOutcome>>,
    completed: std::sync::Condvar,
}

impl PageBuildFlight {
    fn new(expected_generation: u64) -> Self {
        Self {
            expected_generation,
            outcome: std::sync::Mutex::new(None),
            completed: std::sync::Condvar::new(),
        }
    }

    fn complete(&self, outcome: PageBuildOutcome) {
        *self.outcome.lock().unwrap() = Some(outcome);
        self.completed.notify_all();
    }

    fn wait(&self) -> PageBuildOutcome {
        let mut outcome = self.outcome.lock().unwrap();
        while outcome.is_none() {
            outcome = self.completed.wait(outcome).unwrap();
        }
        outcome.expect("completed page build flight has an outcome")
    }
}

#[cfg(test)]
#[derive(Default)]
struct PageBuildTestState {
    owner_pause: std::sync::Mutex<Option<Arc<PageBuildTestPause>>>,
    joined: std::sync::Mutex<usize>,
    joined_changed: std::sync::Condvar,
    force_warm_failure: std::sync::atomic::AtomicBool,
    drift_before_install: std::sync::atomic::AtomicBool,
    enumerations: std::sync::atomic::AtomicUsize,
    parses: std::sync::atomic::AtomicUsize,
    installs: std::sync::atomic::AtomicUsize,
    censuses: std::sync::atomic::AtomicUsize,
    /// R6: pages parsed by the warm STREAM (replacements only).
    warm_stream_parses: std::sync::atomic::AtomicUsize,
    /// R6: pages parsed on demand for reference/fuzzy hydration without a
    /// parsed cache.
    on_demand_parses: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
struct PageBuildTestPause {
    reached: std::sync::Barrier,
    release: std::sync::Barrier,
}

#[cfg(test)]
impl PageBuildTestPause {
    fn new() -> Self {
        Self {
            reached: std::sync::Barrier::new(2),
            release: std::sync::Barrier::new(2),
        }
    }
}

type ParsedPage = (PageEntry, Document, String);
type PageParseResult = Result<Option<ParsedPage>, String>;

impl PageCacheBuild {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            pages: Vec::with_capacity(capacity),
            failures: Vec::new(),
        }
    }

    fn append(&mut self, mut other: Self) {
        self.pages.append(&mut other.pages);
        self.failures.append(&mut other.failures);
    }

    fn collect(&mut self, parsed: PageParseResult) -> bool {
        match parsed {
            Ok(Some(page)) => {
                self.pages.push(page);
                true
            }
            Ok(None) => false,
            Err(path) => {
                self.failures.push(path);
                false
            }
        }
    }
}

fn page_cache_key(kind: PageKind, name: &str) -> (PageKind, String) {
    (kind, crate::refs::page_key(name))
}

fn document_block_ref_counts(
    doc: &Document,
) -> io::Result<std::collections::HashMap<String, usize>> {
    let mut counts = std::collections::HashMap::new();
    let mut frames: [Option<std::slice::Iter<'_, DocBlock>>; MAX_BLOCK_DEPTH] =
        std::array::from_fn(|_| None);
    let mut len = usize::from(!doc.roots.is_empty());
    if len != 0 {
        frames[0] = Some(doc.roots.iter());
    }
    while len != 0 {
        let mut frame = frames[len - 1]
            .take()
            .expect("active document reference frame");
        let Some(block) = frame.next() else {
            len -= 1;
            continue;
        };
        frames[len - 1] = Some(frame);
        // projection().block_refs is already de-duplicated per referrer block,
        // matching the badge's OG-compatible counting semantics.
        for id in &block.projection().block_refs {
            let count = counts.entry(id.clone()).or_insert(0_usize);
            *count = count.checked_add(1).ok_or_else(allocation_overflow)?;
        }
        if !block.children.is_empty() {
            if len == MAX_BLOCK_DEPTH {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cached document nesting exceeds 128 levels",
                ));
            }
            frames[len] = Some(block.children.iter());
            len += 1;
        }
    }
    Ok(counts)
}

fn build_page_cache_index(pages: &[(PageEntry, Arc<Document>)]) -> PageCacheIndex {
    let mut by_name = std::collections::HashMap::with_capacity(pages.len());
    let mut by_path = std::collections::HashMap::with_capacity(pages.len());
    for (i, (entry, _)) in pages.iter().enumerate() {
        // Preserve Vec `.find` semantics if duplicates ever slip in: first wins.
        by_name
            .entry(page_cache_key(entry.kind, &entry.name))
            .or_insert(i);
        by_path.insert(entry.path.clone(), i);
    }
    PageCacheIndex { by_name, by_path }
}

fn build_effective_identity_index(
    generation: u64,
    pages: &[(PageEntry, Arc<Document>)],
    failures: Vec<String>,
) -> EffectiveIdentityIndex {
    let mut owners = std::collections::HashMap::with_capacity(pages.len());
    let mut physical_paths = std::collections::HashSet::with_capacity(pages.len());
    for (entry, _) in pages {
        physical_paths.insert(entry.path.clone());
        owners
            .entry(page_cache_key(entry.kind, &entry.name))
            .or_insert_with(Vec::new)
            .push(entry.clone());
    }
    EffectiveIdentityIndex {
        generation: std::sync::atomic::AtomicU64::new(generation),
        owners,
        physical_paths,
        failures,
    }
}

fn is_date_stem_entry(entry: &PageEntry) -> bool {
    entry
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| crate::date::JournalDate::from_file_stem(s).is_some())
}

struct DerivedCache {
    gen: u64,
    today: i64,
    /// The parse configuration under which the reference results were built.
    /// A mismatch drops the whole cache.
    config_digest: tine_storage::ContentDigest,
    // `Arc<Vec<RefGroup>>` so serving a memoized result (every dataRev re-render)
    // is a refcount bump, not a deep clone of every matched block (see derived_memo).
    results: std::collections::HashMap<String, (DerivedEntry, usize)>,
    lru: std::collections::VecDeque<String>,
    bytes: usize,
}

/// One cached backlink, unlinked-reference, or block-referrer value.
#[derive(Clone)]
struct DerivedEntry {
    result: BoundedRefGroups,
}

impl DerivedEntry {
    fn plain(result: BoundedRefGroups) -> DerivedEntry {
        DerivedEntry { result }
    }
}

/// Which of SPEC §5.9's states one dispatched query attempt reached.
///
/// Generic in what a READY statement produced, because §5.9's states are a
/// property of the projection and not of the row shape: `@block` pre-view
/// groups, `@page` rows and an explanation's probe counts all reach the
/// projection the same way and owe the same classification
/// (`Graph::dispatch_direct_query` is the one place that pays them).
///
/// **RET2.** Every arm that is not `Answered` used to hand the query to the
/// parsed-graph walk. There is no walk here any more: the arms say what the
/// projection did, and the dispatcher turns each into either ONE bounded repair
/// or a typed [`crate::query::QueryExecutionError`]. An attempt returns an
/// OWNED answer, so every snapshot handle it opened is already dropped by the
/// time the dispatcher can decide to repair (R3 §2B).
type DirectQueryRequest = Option<(
    Arc<crate::direct_projection::DirectProjection>,
    crate::query_jobs::QueryJobEpoch,
)>;

enum DirectAttempt<T> {
    /// The statement answered and its rows were hydrated.
    Answered(T),
    /// The projection is not ready at this cache generation. Whether that is
    /// worth retrying, worth repairing, or terminal is
    /// [`crate::direct_projection::ProjectionProgress`]'s answer, not this
    /// arm's: the attempt only knows it could not read.
    NotReady,
    /// Capacity admission refused this attempt (R3): other jobs hold every
    /// slot. Work IS progressing, so this is retryable and never repaired.
    Busy,
    /// A read was attempted and did not answer. Repairable exactly once, and
    /// the reason travels so a contradiction (`InvalidSnapshot`) and a refused
    /// statement (`ReadFailed`) stay distinguishable at the public boundary.
    FailedRead(crate::query::QueryUnavailableReason),
    /// Projection repair cannot answer this request: no projection is
    /// attached at all (`ProjectionUnavailable`), or the compiler could not
    /// lower this shape (`UnsupportedRelation`). The second is a
    /// FUTURE-relation arm and not a live route — `query::sql::lower_query` is
    /// total today — but it is the arm a non-total lowering must take, because
    /// the alternative shapes (a fabricated empty answer, or a switch back to
    /// the evaluator) are both forbidden. Statistics resource exhaustion also
    /// takes this arm: an oversized fold does not mean the index is broken.
    Unavailable(crate::query::QueryUnavailableReason),
    /// The projection cancelled the job — a rebuild drained it, or the graph
    /// closed. Nothing is wrong with the projection, so it is NEVER repaired
    /// and never retried: the caller asked for a read that no longer has a
    /// subject (R3 §2B).
    Cancelled,
}

/// The ONE translation from an attempted read into a dispatch state
/// (D-3, §5.9/M9).
///
/// A seam refusal and a projection that contradicts itself are both "the read
/// was attempted and did not answer", so both are repaired — but they stay
/// distinguishable at the public boundary, because "the index could not be
/// read" and "the index returned inconsistent results" are different things to
/// tell a user. `From<ResultReadError>` in `query::results` is the one place
/// the free-form payload is dropped (I-5).
fn direct_attempt_from_read<T>(
    read: Result<T, crate::query::QueryExecutionError>,
) -> DirectAttempt<T> {
    use crate::query::QueryExecutionError as Error;
    match read {
        Ok(answer) => DirectAttempt::Answered(answer),
        Err(Error::Cancelled) => DirectAttempt::Cancelled,
        Err(Error::NotReady(_)) => DirectAttempt::NotReady,
        Err(Error::Unavailable(
            reason @ crate::query::QueryUnavailableReason::StatisticsResourceLimit,
        )) => DirectAttempt::Unavailable(reason),
        Err(Error::Unavailable(reason)) => DirectAttempt::FailedRead(reason),
    }
}

// Query results contain owned DTO subtrees and can be close to graph-sized. A
// graph-lifetime, key-unbounded memo turns ordinary navigation through many
// pages' Linked References into unbounded retained memory. Oversized results are
// returned to their caller but deliberately not retained here.
const DERIVED_CACHE_MAX_ENTRIES: usize = 64;
const DERIVED_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const DERIVED_CACHE_MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;

fn result_cache_key_estimated_bytes(key: &str) -> usize {
    // The HashMap owns one key and the LRU owns another. Account both copies.
    key.len().saturating_mul(2).saturating_add(128)
}

pub fn block_dto_estimated_bytes(block: &BlockDto) -> usize {
    block.id.len()
        + block.raw.len()
        + block.breadcrumb.iter().map(String::len).sum::<usize>()
        + block.tags.iter().map(String::len).sum::<usize>()
        + block
            .properties
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>()
        + block
            .children
            .iter()
            .map(block_dto_estimated_bytes)
            .sum::<usize>()
        + 128
}

/// Conservative owned-memory estimate for a result payload. Tauri commands use
/// this before serialization as a second guard beside the row cap; derived
/// caches use the same accounting so transport and retention budgets cannot
/// drift apart.
pub fn ref_groups_estimated_bytes(groups: &[RefGroup]) -> usize {
    groups
        .iter()
        .map(|group| {
            group.page.len()
                + group
                    .blocks
                    .iter()
                    .map(block_dto_estimated_bytes)
                    .sum::<usize>()
                + group
                    .evidence
                    .iter()
                    .map(|evidence| {
                        evidence.block_id.len()
                            + evidence
                                .occurrences
                                .iter()
                                .map(|occurrence| {
                                    occurrence.matched_name.len()
                                        + occurrence.canonical.len()
                                        + occurrence.rule.len()
                                        + std::mem::size_of::<ReferenceOccurrence>()
                                })
                                .sum::<usize>()
                    })
                    .sum::<usize>()
                + std::mem::size_of::<RefGroup>()
        })
        .sum()
}

fn bounded_ref_groups(computed: crate::query::BoundedGroups) -> BoundedRefGroups {
    BoundedRefGroups {
        matched_total: None,
        statistics: None,
        groups: Arc::new(computed.groups),
        total: computed.total,
        exceeded: computed.exceeded,
    }
}

fn touch_lru(lru: &mut std::collections::VecDeque<String>, key: &str) {
    if let Some(pos) = lru.iter().position(|candidate| candidate == key) {
        lru.remove(pos);
    }
    lru.push_back(key.to_owned());
}

fn prune_result_cache<T>(
    results: &mut std::collections::HashMap<String, (T, usize)>,
    lru: &mut std::collections::VecDeque<String>,
    bytes: &mut usize,
) {
    while results.len() > DERIVED_CACHE_MAX_ENTRIES || *bytes > DERIVED_CACHE_MAX_BYTES {
        let Some(oldest) = lru.pop_front() else { break };
        if let Some((_, removed_bytes)) = results.remove(&oldest) {
            *bytes = bytes.saturating_sub(removed_bytes);
        }
    }
}

struct FindEntryIndex {
    entries: std::collections::HashMap<(PageKind, String), PageEntry>,
    pages_loaded: bool,
    journals_loaded: bool,
}

impl FindEntryIndex {
    fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            pages_loaded: false,
            journals_loaded: false,
        }
    }

    fn has_kind(&self, kind: PageKind) -> bool {
        match kind {
            PageKind::Journal => self.journals_loaded,
            PageKind::Page => self.pages_loaded,
        }
    }

    fn mark_kind_loaded(&mut self, kind: PageKind) {
        match kind {
            PageKind::Journal => self.journals_loaded = true,
            PageKind::Page => self.pages_loaded = true,
        }
    }
}

/// Validate one config-controlled graph directory. Logseq permits nested relative
/// directories, but an absolute path, traversal component, or symlinked existing
/// ancestor outside the graph would turn ordinary save/delete/restore operations
/// into writes against unrelated files.
fn validate_graph_dir(root: &Path, raw: &str, label: &str) -> io::Result<()> {
    if raw.is_empty() || raw.contains('\\') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {label} directory: {raw:?}"),
        ));
    }
    let rel = Path::new(raw);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} directory must be a safe relative path: {raw:?}"),
        ));
    }
    let candidate = root.join(rel);
    if !path_stays_within_root(root, &candidate) || path_uses_graph_text_alias(root, &candidate) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} directory escapes graph root: {raw:?}"),
        ));
    }
    Ok(())
}

/// Containment check for both existing and not-yet-created targets. Canonicalize
/// the deepest existing ancestor so a symlink in the path cannot smuggle a later
/// filename outside the graph. The runtime root is already canonical, while the
/// fallback keeps disposable direct-`Graph::open` fixtures working as before.
fn path_stays_within_root(root: &Path, target: &Path) -> bool {
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut existing = target;
    while fs::symlink_metadata(existing).is_err() {
        let Some(parent) = existing.parent() else {
            return false;
        };
        existing = parent;
    }
    fs::canonicalize(existing)
        .map(|p| p.starts_with(&canonical_root))
        .unwrap_or(false)
}

/// Graph directories Tine reads or writes must retain their own identity, not merely land
/// somewhere under the graph after canonicalization. An in-graph symlink such as
/// `publish -> assets` passes a plain containment check but redirects generated
/// output onto user assets. Compare the deepest existing ancestor with its
/// expected canonical lexical location to reject any such alias.
fn path_uses_graph_text_alias(root: &Path, target: &Path) -> bool {
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut existing = target;
    while fs::symlink_metadata(existing).is_err() {
        let Some(parent) = existing.parent() else {
            return true;
        };
        existing = parent;
    }
    let Ok(relative) = existing.strip_prefix(root) else {
        return true;
    };
    fs::canonicalize(existing)
        .map(|actual| actual != canonical_root.join(relative))
        .unwrap_or(true)
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct GraphTextAdmissionTestCounters {
    builder_enumerations: usize,
    direct_creation_censuses: usize,
    direct_creation_files_hashed: usize,
    point_query_attempts: usize,
    parser_invocations: usize,
    index_map_insertions: usize,
    event_map_key_reads: usize,
    event_map_key_writes: usize,
    event_reverse_members: usize,
    persistent_node_allocations: usize,
    persistent_rotations: usize,
    persistent_payload_members: usize,
}

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_RENAME_SOURCE_REMOVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static WITHDRAW_RACE_REPLACEMENT: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
    static GUIDE_TWIN_RACE_CONTENT: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
    static PROJECTION_LAST_MOMENT_REPLACEMENT: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
    static PROJECTION_PUBLICATION_RACE_REPLACEMENT: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
    static PROJECTION_AFTER_RETIRE_REPLACEMENT: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
    static PROJECTION_STALE_RECOVERY_WRITE: std::cell::RefCell<Option<(fs::File, Vec<u8>)>> = const { std::cell::RefCell::new(None) };
    static PROJECTION_POST_PUBLISH_REPLACEMENT: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
    static PROJECTION_LATE_COLLISION: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static PROJECTION_AFTER_RETIRE_COLLISION: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static PROJECTION_POST_PUBLISH_COLLISION: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static PROJECTION_BEFORE_RESTORE: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static FAIL_NEXT_PROJECTION_DIRECTORY_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PROJECTION_EXACT_OPEN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static GRAPH_TEXT_INVENTORY_READ_RACE: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static GRAPH_TEXT_CAPTURE_REVALIDATION_RACE: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static GRAPH_TEXT_INVENTORY_LIMITS_OVERRIDE: std::cell::RefCell<Option<GraphTextInventoryLimits>> = const { std::cell::RefCell::new(None) };
    static GRAPH_TEXT_BUDGET_LAST_PEAK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static BOUNDED_READ_AFTER_METADATA: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static GRAPH_TEXT_WRITE_IDENTITY_ACQUISITION: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static GRAPH_TEXT_WRITE_AFTER_ADMISSION: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static GRAPH_TEXT_WRITE_AFTER_IDENTITY_CHECK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static GRAPH_TEXT_WRITE_BEFORE_MUTATION: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static GRAPH_TEXT_WRITE_AFTER_RETIRE: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static JOURNAL_PROJECTION_BEFORE_PUBLISH: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static JOURNAL_PROJECTION_AFTER_PUBLISH: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static JOURNAL_PROJECTION_AFTER_TARGET_REREAD: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static JOURNAL_PROJECTION_BEFORE_CACHE_PUBLICATION: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static GRAPH_TEXT_WRITE_BEFORE_RESTORE: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static GRAPH_TEXT_WRITE_DURING_ROLLBACK: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static EDITOR_RETIRED_CLEANUP: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static EDITOR_COMMIT_BEFORE_RECHECK: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static EDITOR_COMMIT_BEFORE_FINAL_REREAD: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static EXACT_GRAPH_TEXT_EVENT_AFTER_CANDIDATE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static CONFLICT_OBSERVATION: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static GRAPH_TEXT_ADMISSION_TEST_COUNTERS: std::cell::Cell<GraphTextAdmissionTestCounters> = const { std::cell::Cell::new(GraphTextAdmissionTestCounters { builder_enumerations: 0, direct_creation_censuses: 0, direct_creation_files_hashed: 0, point_query_attempts: 0, parser_invocations: 0, index_map_insertions: 0, event_map_key_reads: 0, event_map_key_writes: 0, event_reverse_members: 0, persistent_node_allocations: 0, persistent_rotations: 0, persistent_payload_members: 0 }) };
    static GRAPH_TEXT_PARSE_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static GRAPH_TEXT_FIRST_CAPTURE_CHARGE_OVERRIDE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static GRAPH_TEXT_PORTABLE_TRAVERSALS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static GRAPH_TEXT_EVENT_REVALIDATION_RACE: std::cell::RefCell<Option<Box<dyn FnOnce() -> io::Result<()>>>> = std::cell::RefCell::new(None);
    static FAIL_NEXT_GUARDED_GRAPH_TEXT_IDENTITY_UPDATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static DIRECT_CREATION_CENSUS_BUMP_CACHE_GEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn reset_graph_text_admission_test_counters() {
    GRAPH_TEXT_ADMISSION_TEST_COUNTERS
        .with(|counters| counters.set(GraphTextAdmissionTestCounters::default()));
}

#[cfg(test)]
fn graph_text_admission_test_counters() -> GraphTextAdmissionTestCounters {
    GRAPH_TEXT_ADMISSION_TEST_COUNTERS.with(Cell::get)
}

#[cfg(test)]
fn count_graph_text_admission_builder_enumeration() {
    GRAPH_TEXT_ADMISSION_TEST_COUNTERS.with(|counters| {
        let mut value = counters.get();
        value.builder_enumerations += 1;
        counters.set(value);
    });
}

#[cfg(not(test))]
fn count_graph_text_admission_builder_enumeration() {}

#[cfg(test)]
fn count_graph_text_admission_parser_invocation() {
    GRAPH_TEXT_ADMISSION_TEST_COUNTERS.with(|counters| {
        let mut value = counters.get();
        value.parser_invocations += 1;
        counters.set(value);
    });
}

#[cfg(not(test))]
fn count_graph_text_admission_parser_invocation() {}

#[cfg(test)]
fn count_graph_text_admission_index_map_insertion() {
    GRAPH_TEXT_ADMISSION_TEST_COUNTERS.with(|counters| {
        let mut value = counters.get();
        value.index_map_insertions += 1;
        counters.set(value);
    });
}

#[cfg(not(test))]
fn count_graph_text_admission_index_map_insertion() {}

#[cfg(test)]
fn count_graph_text_admission_event_work(reads: usize, writes: usize, members: usize) {
    GRAPH_TEXT_ADMISSION_TEST_COUNTERS.with(|counters| {
        let mut value = counters.get();
        value.event_map_key_reads += reads;
        value.event_map_key_writes += writes;
        value.event_reverse_members += members;
        counters.set(value);
    });
}

#[cfg(not(test))]
fn count_graph_text_admission_event_work(_reads: usize, _writes: usize, _members: usize) {}

#[cfg(test)]
fn count_graph_text_admission_persistent_node_allocation() {
    GRAPH_TEXT_ADMISSION_TEST_COUNTERS.with(|counters| {
        let mut value = counters.get();
        value.persistent_node_allocations += 1;
        counters.set(value);
    });
}

#[cfg(not(test))]
fn count_graph_text_admission_persistent_node_allocation() {}

#[cfg(test)]
fn count_graph_text_admission_persistent_rotation() {
    GRAPH_TEXT_ADMISSION_TEST_COUNTERS.with(|counters| {
        let mut value = counters.get();
        value.persistent_rotations += 1;
        counters.set(value);
    });
}

#[cfg(not(test))]
fn count_graph_text_admission_persistent_rotation() {}

#[cfg(test)]
fn count_graph_text_admission_persistent_payload_members(members: usize) {
    GRAPH_TEXT_ADMISSION_TEST_COUNTERS.with(|counters| {
        let mut value = counters.get();
        value.persistent_payload_members += members;
        counters.set(value);
    });
}

#[cfg(not(test))]
fn count_graph_text_admission_persistent_payload_members(_members: usize) {}

#[cfg(test)]
fn graph_text_event_revalidation_race_hook() -> io::Result<()> {
    GRAPH_TEXT_EVENT_REVALIDATION_RACE.with(|hook| {
        let callback = hook.borrow_mut().take();
        callback.map_or(Ok(()), |callback| callback())
    })
}

#[cfg(not(test))]
fn graph_text_event_revalidation_race_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn graph_text_parse_failure_hook() -> io::Result<()> {
    GRAPH_TEXT_PARSE_FAILURE.with(|failure| {
        if failure.replace(false) {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "injected graph-text parse failure",
            ))
        } else {
            Ok(())
        }
    })
}

#[cfg(not(test))]
fn graph_text_parse_failure_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn rename_source_remove_failpoint() -> io::Result<()> {
    FAIL_NEXT_RENAME_SOURCE_REMOVE.with(|flag| {
        if flag.replace(false) {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected source remove failure",
            ))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
fn withdrawal_race_hook(path: &Path) -> io::Result<()> {
    WITHDRAW_RACE_REPLACEMENT.with(|replacement| {
        if let Some(bytes) = replacement.borrow_mut().take() {
            fs::write(path, bytes)?;
        }
        Ok(())
    })
}

#[cfg(not(test))]
fn withdrawal_race_hook(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn guide_twin_race_hook(path: &Path) -> io::Result<()> {
    GUIDE_TWIN_RACE_CONTENT.with(|content| {
        if let Some(bytes) = content.borrow_mut().take() {
            fs::write(path.with_extension("org"), bytes)?;
        }
        Ok(())
    })
}

#[cfg(not(test))]
fn guide_twin_race_hook(_path: &Path) -> io::Result<()> {
    Ok(())
}

// Keep the test fault at the narrow core Result boundary rather than adding a
// test-only control surface to tine-storage.  Keying it by the deterministic
// ambient return path keeps parallel runtime fixtures independent.

#[cfg(test)]
fn projection_directory_sync_hook(_dir: &Path) -> io::Result<()> {
    FAIL_NEXT_PROJECTION_DIRECTORY_SYNC.with(|fail| {
        if fail.replace(false) {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "injected projection directory sync failure",
            ))
        } else {
            Ok(())
        }
    })
}

#[cfg(not(test))]
fn projection_directory_sync_hook(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn graph_text_inventory_read_hook() -> io::Result<()> {
    GRAPH_TEXT_INVENTORY_READ_RACE.with(|hook| {
        let hook = hook.borrow_mut().take();
        match hook {
            Some(hook) => hook(),
            None => Ok(()),
        }
    })
}

#[cfg(not(test))]
fn graph_text_inventory_read_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn graph_text_capture_revalidation_hook(_root: &Path) -> io::Result<()> {
    GRAPH_TEXT_CAPTURE_REVALIDATION_RACE.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn graph_text_capture_revalidation_hook(_root: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn bounded_read_after_metadata_hook() -> io::Result<()> {
    BOUNDED_READ_AFTER_METADATA.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn bounded_read_after_metadata_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn graph_text_write_identity_acquisition_hook() -> io::Result<()> {
    GRAPH_TEXT_WRITE_IDENTITY_ACQUISITION.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn graph_text_write_identity_acquisition_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn graph_text_write_after_admission_hook() -> io::Result<()> {
    GRAPH_TEXT_WRITE_AFTER_ADMISSION.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn graph_text_write_after_admission_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn graph_text_write_after_identity_check_hook() {
    GRAPH_TEXT_WRITE_AFTER_IDENTITY_CHECK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn graph_text_write_after_identity_check_hook() {}

#[cfg(test)]
fn graph_text_write_before_mutation_hook() -> io::Result<()> {
    GRAPH_TEXT_WRITE_BEFORE_MUTATION.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn graph_text_write_before_mutation_hook() -> io::Result<()> {
    Ok(())
}

/// The editor writer's displacement fault point (journal-universal durability
/// design §4.6, W1 / §4.3 row F3).
///
/// It fires strictly between the displacement rename `T -> .editor-recovery`
/// and the publication rename `staged -> T`, so arming it produces the state
/// F3 names: `T` absent, the `.editor-recovery` claim holding the precondition.
/// The design lists a hook here as packet-1 work; the hook already existed with
/// exactly that placement and semantics, so packet 1 documents and tests it
/// rather than adding a second one at the same cut.
///
/// PRODUCTION ARMS NOTHING: the non-test definition is a constant `Ok(())`.
#[cfg(test)]
fn graph_text_write_after_retire_hook() -> io::Result<()> {
    GRAPH_TEXT_WRITE_AFTER_RETIRE.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn graph_text_write_after_retire_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn journal_projection_after_publish_hook() -> io::Result<()> {
    JOURNAL_PROJECTION_AFTER_PUBLISH.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn journal_projection_after_publish_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn graph_text_write_before_restore_hook() -> io::Result<()> {
    GRAPH_TEXT_WRITE_BEFORE_RESTORE.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn graph_text_write_before_restore_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn graph_text_write_during_rollback_hook() -> io::Result<()> {
    GRAPH_TEXT_WRITE_DURING_ROLLBACK.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn graph_text_write_during_rollback_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn editor_retired_cleanup_hook() -> io::Result<()> {
    EDITOR_RETIRED_CLEANUP.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn editor_retired_cleanup_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn editor_commit_before_recheck_hook() -> io::Result<()> {
    EDITOR_COMMIT_BEFORE_RECHECK.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn editor_commit_before_recheck_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn editor_commit_before_final_reread_hook() -> io::Result<()> {
    EDITOR_COMMIT_BEFORE_FINAL_REREAD.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn editor_commit_before_final_reread_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn exact_graph_text_event_after_candidate_hook() {
    EXACT_GRAPH_TEXT_EVENT_AFTER_CANDIDATE.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn exact_graph_text_event_after_candidate_hook() {}

#[cfg(test)]
fn conflict_observation_hook() -> io::Result<()> {
    CONFLICT_OBSERVATION.with(|hook| match hook.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(not(test))]
fn conflict_observation_hook() -> io::Result<()> {
    Ok(())
}

#[cfg(not(test))]
fn rename_source_remove_failpoint() -> io::Result<()> {
    Ok(())
}

/// OG's default when `:ref/linked-references-collapsed-threshold` is absent.
fn default_linked_references_collapsed_threshold() -> u32 {
    100
}

// `PartialEq` is load-bearing, not a convenience: the config watcher refreshes
// a graph and then compares the meta it produced against the meta the frontend
// already has, so a rewrite that changes no setting emits nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphMeta {
    pub root: String,
    pub journals_dir: String,
    pub pages_dir: String,
    /// "now" (LATER/NOW) or "todo" (TODO/DOING) — drives the task cycle.
    pub preferred_workflow: String,
    pub shortcuts: std::collections::HashMap<String, String>,
    /// First day of week for the date picker (0=Sunday … 6=Saturday).
    pub start_of_week: u32,
    /// Extra property keys to hide from the rendered properties area.
    pub block_hidden_properties: Vec<String>,
    /// Backlink count at which a page opens its Linked References collapsed
    /// (`:ref/linked-references-collapsed-threshold`, OG default 100).
    #[serde(default = "default_linked_references_collapsed_threshold")]
    pub linked_references_collapsed_threshold: u32,
    /// Template name applied to a new, empty journal page (if configured).
    pub default_journal_template: Option<String>,
    /// Graph-portable startup page from `:default-home {:page "..."}`.
    #[serde(default)]
    pub default_home: Option<String>,
    /// Favorited page names (read from config.edn `:favorites`).
    pub favorites: Vec<String>,
    /// The page holding Tine's Favorites arrangement (`:tine/favorites-page`),
    /// when this graph has one. `:favorites` above stays the flat, Logseq-
    /// readable membership list; this page owns groups and order.
    #[serde(default)]
    pub favorites_page: Option<String>,
    /// Effective journal title format (`:journal/page-title-format`, default
    /// `MMM do, yyyy`) — so the frontend formats "today" to match the backend.
    pub journal_page_title_format: String,
    /// Effective journal filename format (`:journal/file-name-format`, default
    /// `yyyy_MM_dd`).
    pub journal_file_name_format: String,
    /// Format new pages/journals are created in (`"md"` or `"org"`), from
    /// `:preferred-format`. The frontend uses it to label the toggle and pick the
    /// new-page extension.
    pub preferred_format: String,
    /// User-defined `:macros {"name" "template"}` — the frontend substitutes
    /// `$1..$N` args into the template and renders the result as markdown.
    pub macros: std::collections::HashMap<String, String>,
    /// `:feature/enable-timetracking?` effective value; default true.
    pub enable_timetracking: bool,
    /// `:ui/show-brackets?` effective value; default true.
    pub show_brackets: bool,
    /// `:shortcut/doc-mode-enter-for-new-block?` effective value; default false.
    pub doc_mode_enter_for_new_block: bool,
    /// `:editor/logical-outdenting?` effective value; default false.
    pub logical_outdenting: bool,
    /// `:logbook/settings :with-second-support?` effective value; default true.
    pub logbook_with_second_support: bool,
    /// `:logbook/settings :enabled-in-timestamped-blocks` effective value.
    pub logbook_enabled_in_timestamped_blocks: bool,
    /// `:logbook/settings :enabled-in-all-blocks` effective value.
    pub logbook_enabled_in_all_blocks: bool,
    /// Tine-owned graph-local flag: whether this graph has already seen the
    /// one-time in-app Guide announcement.
    pub guide_announced: bool,
}

/// The graph-relative location of the configuration file, as `Graph::open`
/// reads it and as the exact-feed classifier names it. One constant, so moving
/// it can never land in one of those and miss the other.
pub const CONFIG_RELATIVE_PATH: &str = "logseq/config.edn";
/// Upper bound on bytes read back from one graph text file or private
/// artifact; a file above this is refused rather than retained.
pub(crate) const MAX_PROJECTION_EVIDENCE_BYTES: u64 = 64 * 1024 * 1024;
/// Upper bound on parser nodes admitted from one externally edited source
/// file, so a pathological file cannot exhaust memory during a scan.
const MAX_GRAPH_TEXT_PARSER_NODES: u64 = 1_000_000;

fn graph_text_capture_error(detail: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail.into())
}

/// Re-exported so a caller outside the crate can name what
/// [`config_file_description`] and [`Graph::open_config_description`] return.
pub use crate::graph_text_path::BlobDescription as ConfigDescription;

/// Digest `logseq/config.edn` as it stands on disk right now, resolving the
/// path exactly as `Graph::open` does.
///
/// `None` means "no readable configuration file", which is precisely what
/// `open` would have parsed as an empty `Config` -- so a `None` here and a
/// `None` from [`Graph::open_config_description`] agree that nothing changed.
pub fn config_file_description(root: &Path) -> Option<BlobDescription> {
    fs::read(reconciliation_scan_config_path_at_open(root))
        .ok()
        .map(|bytes| BlobDescription::of(&bytes))
}

/// Is `path` the configuration file of the graph rooted at `root`?
///
/// Case-insensitive, like the open path and the classifier: a graph delivered
/// by a case-folding filesystem may spell it `Logseq/Config.edn`.
pub fn is_config_file_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let Some(relative) = relative.to_str() else {
        return false;
    };
    relative
        .replace(std::path::MAIN_SEPARATOR, "/")
        .eq_ignore_ascii_case(CONFIG_RELATIVE_PATH)
}

pub(crate) fn reconciliation_scan_config_path_at_open(root: &Path) -> PathBuf {
    let exact = root.join("logseq").join("config.edn");
    let matching = |directory: &Path, expected: &str| -> Option<PathBuf> {
        let mut found = None;
        for entry in fs::read_dir(directory).ok()? {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let name = name.to_str()?;
            if name.eq_ignore_ascii_case(expected) {
                if found.is_some() {
                    return None;
                }
                found = Some(entry.path());
            }
        }
        found
    };
    let Some(logseq) = matching(root, "logseq") else {
        return exact;
    };
    matching(&logseq, "config.edn").unwrap_or(exact)
}

impl Graph {
    fn graph_text_read_optional(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<Option<Vec<u8>>> {
        let target = match self.graph_text_target(permit, path, false) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        read_projection_optional(target.parent(), &target.filename)
    }

    fn graph_text_read_optional_text(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<Option<String>> {
        self.graph_text_read_optional(permit, path)?
            .map(|bytes| {
                String::from_utf8(bytes).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "graph text file is not valid UTF-8",
                    )
                })
            })
            .transpose()
    }

    fn graph_text_read_optional_text_with_identity(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<Option<(String, ContentDigest)>> {
        let target = match self.graph_text_target(permit, path, false) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let (file, bytes) =
            match open_and_read_projection_regular(target.parent(), &target.filename) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
        #[cfg(test)]
        GRAPH_TEXT_CONTENT_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
        let identity = canonical_projection_file_resource_id(&file)?;
        let text = String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "graph text file is not valid UTF-8",
            )
        })?;
        Ok(Some((text, identity)))
    }

    /// One-open coherent snapshot for a conflict authority decision. Unlike an
    /// ordinary read, this also performs the hard-refusal admission checks that
    /// must never mint override authority (portable alias and multiple links).
    fn graph_text_read_optional_editor_conflict_snapshot(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<Option<(String, ContentDigest)>> {
        let graph_text_path = GraphTextPath::parse(self.rel_path(path)).map_err(|error| {
            DirectSaveError::into_io(
                DirectSaveFailureCode::PrecheckNotPortable,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("guarded graph-text target is not portable: {error}"),
                ),
            )
        })?;
        let target = match self.graph_text_target(permit, path, false) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let (file, bytes) =
            match open_and_read_projection_regular(target.parent(), &target.filename) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
        validate_graph_text_single_link(&file, graph_text_path.as_str())?;
        #[cfg(test)]
        GRAPH_TEXT_CONTENT_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
        let identity = canonical_projection_file_resource_id(&file)?;
        let text = String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "graph text file is not valid UTF-8",
            )
        })?;
        Ok(Some((text, identity)))
    }

    fn graph_text_optional_file_identity(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<Option<ContentDigest>> {
        let target = match self.graph_text_target(permit, path, false) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        match open_projection_file_nofollow(target.parent(), &target.filename) {
            Ok(file) => canonical_projection_file_resource_id(&file).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Resolve one exact existing document through its retained graph-relative
    /// path. Logical duplicates are deliberately readable: exact-path recovery
    /// must not depend on unrelated page-name uniqueness.
    fn load_validated_graph_text_target(
        &self,
        permit: &GraphTextWritePermit,
        target: &Path,
    ) -> io::Result<Option<ExactGraphLoadedPage>> {
        let Some(entry) = self.graph_inventory_entry(target)? else {
            return Ok(None);
        };
        let Some((content, file_identity)) =
            self.graph_text_read_optional_text_with_identity(permit, target)?
        else {
            return Ok(None);
        };
        #[cfg(test)]
        GRAPH_TEXT_VALIDATION_TARGET_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
        if usize_to_u64(content.len())? > graph_text_inventory_limits().retained_content_bytes {
            return Err(graph_text_inventory_limit_error("aggregate text bytes"));
        }
        let (entry, document, revision) = parse_exact_page(self, &entry, &content)?;
        Ok(Some(ExactGraphLoadedPage {
            entry,
            document,
            content,
            revision,
            file_identity,
        }))
    }

    /// Prove the exact-file properties needed before the final mutation
    /// boundary. Portable-path proof is deliberately separate: initial
    /// validation must not enumerate a large retained parent twice per save.
    fn validate_existing_graph_text_target_local(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        expected_identity: ContentDigest,
    ) -> io::Result<()> {
        let target = self.graph_text_target(permit, path, false)?;
        let graph_text_path = GraphTextPath::parse(self.rel_path(path)).map_err(|error| {
            DirectSaveError::into_io(
                DirectSaveFailureCode::PrecheckNotPortable,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("guarded graph-text target is not portable: {error}"),
                ),
            )
        })?;
        self.validate_existing_graph_text_target_exact(
            &target,
            &graph_text_path,
            Some(expected_identity),
        )?;
        Ok(())
    }

    fn validate_existing_graph_text_target_exact(
        &self,
        target: &GraphTextTarget,
        graph_text_path: &GraphTextPath,
        expected_identity: Option<ContentDigest>,
    ) -> io::Result<ContentDigest> {
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let file = open_projection_file_nofollow(target.parent(), &target.filename)?;
        let identity = canonical_projection_file_resource_id(&file)?;
        if expected_identity.is_some_and(|expected| expected != identity) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "graph text target changed at the local identity validation boundary",
            ));
        }
        validate_graph_text_single_link(&file, graph_text_path.as_str())?;
        Ok(identity)
    }

    /// Starting from the retained graph root, traverse only directory spellings
    /// whose single-component portable identity matches the requested path.
    /// This discovers case/NFC aliases in any ancestor without admitting an
    /// unrelated subtree or reading graph-text bytes.
    fn validate_graph_text_portable_aliases_path_local(
        &self,
        permit: &GraphTextWritePermit,
        graph_text_path: &GraphTextPath,
        strict_creation: bool,
    ) -> io::Result<()> {
        #[cfg(test)]
        GRAPH_TEXT_PORTABLE_TRAVERSALS.with(|count| count.set(count.get().saturating_add(1)));

        struct PortablePrefix {
            directory: Dir,
            relative: String,
        }

        let limits = graph_text_inventory_limits();
        let components = graph_text_path.as_str().split('/').collect::<Vec<_>>();
        if components.len().saturating_sub(1) > limits.directory_depth {
            return Err(graph_text_inventory_limit_error("graph directory depth"));
        }
        let mut prefixes = vec![PortablePrefix {
            directory: self.graph_text_permit_root(permit)?.try_clone()?,
            relative: String::new(),
        }];
        let mut all_entries = 0_usize;
        let mut directory_count = 1_usize;
        let mut path_bytes = 0_u64;

        for (component_index, requested_component) in components.iter().enumerate() {
            // Hoisted out of the entry loop: for a non-ASCII component the fast
            // path never fires, and refolding the same component once per
            // directory entry is slower than the code this replaced.
            let requested_probe = PortablePathKey::graph_text_component_probe(requested_component);
            let is_filename = component_index + 1 == components.len();
            let requested_relative = components[..=component_index].join("/");
            let mut next = Vec::new();

            for prefix in prefixes {
                for entry in prefix.directory.entries()? {
                    all_entries = all_entries
                        .checked_add(1)
                        .ok_or_else(|| graph_text_inventory_limit_error("all directory entries"))?;
                    if all_entries > limits.all_entries {
                        return Err(graph_text_inventory_limit_error("all directory entries"));
                    }
                    let entry = entry?;
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else {
                        // GraphTextPath is UTF-8 by contract, so this entry cannot
                        // share the requested portable component identity.
                        continue;
                    };
                    let relative_len = prefix
                        .relative
                        .len()
                        .checked_add(usize::from(!prefix.relative.is_empty()))
                        .and_then(|length| length.checked_add(name.len()))
                        .ok_or_else(|| graph_text_inventory_limit_error("aggregate path bytes"))?;
                    path_bytes = path_bytes
                        .checked_add(usize_to_u64(relative_len)?)
                        .ok_or_else(|| graph_text_inventory_limit_error("aggregate path bytes"))?;
                    if path_bytes > limits.path_bytes {
                        return Err(graph_text_inventory_limit_error("aggregate path bytes"));
                    }
                    if !PortablePathKey::graph_text_component_matches(
                        name,
                        requested_component,
                        requested_probe.as_ref(),
                    ) {
                        continue;
                    }
                    let relative = if prefix.relative.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{}/{name}", prefix.relative)
                    };
                    let file_type = entry.file_type()?;
                    if file_type.is_symlink() {
                        if strict_creation {
                            return Err(DirectSaveError::into_io(
                                DirectSaveFailureCode::PrecheckNofollow,
                                io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    "projection path has no retained no-follow directory or file",
                                ),
                            ));
                        }
                        continue;
                    }

                    if is_filename {
                        if relative == graph_text_path.as_str()
                            || !file_type.is_file()
                            || !self.graph_text_scope.is_eligible(&relative)
                        {
                            continue;
                        }
                        projection_optional_regular_metadata(&prefix.directory, name)?;
                        match open_projection_file_nofollow(&prefix.directory, name) {
                            Ok(_) => {
                                return Err(DirectSaveError::into_io(
                                    DirectSaveFailureCode::PrecheckPortableCollision,
                                    io::Error::new(
                                        io::ErrorKind::AlreadyExists,
                                        format!(
                                            "graph text paths share one portable case/NFC identity: {relative} and {}",
                                            graph_text_path.as_str()
                                        ),
                                    ),
                                ));
                            }
                            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                            Err(error) => return Err(error),
                        }
                    }

                    if !file_type.is_dir() || !self.graph_text_scope.should_descend(&relative) {
                        continue;
                    }
                    directory_count = directory_count
                        .checked_add(1)
                        .ok_or_else(|| graph_text_inventory_limit_error("directory count"))?;
                    if directory_count > limits.directories {
                        return Err(graph_text_inventory_limit_error("directory count"));
                    }
                    if strict_creation && relative != requested_relative {
                        projection_real_directory(&prefix.directory, name)?;
                        let _alias = open_projection_dir_nofollow(&prefix.directory, name)?;
                        return Err(DirectSaveError::into_io(
                            DirectSaveFailureCode::PrecheckPortableCollision,
                            io::Error::new(
                                io::ErrorKind::AlreadyExists,
                                format!(
                                    "graph text paths share one portable case/NFC identity: {relative} and {requested_relative}"
                                ),
                            ),
                        ));
                    }
                    if next.len() == limits.pending_directories {
                        return Err(graph_text_inventory_limit_error("pending directories"));
                    }
                    projection_real_directory(&prefix.directory, name)?;
                    next.push(PortablePrefix {
                        directory: open_projection_dir_nofollow(&prefix.directory, name)?,
                        relative,
                    });
                }
            }
            if is_filename {
                return Ok(());
            }
            prefixes = next;
        }
        Ok(())
    }

    /// Apply strict current graph-scope collision policy to editor/name-only
    /// mutation. Portable aliases remain readable, but an editor mutation
    /// cannot choose one without authenticated exact logical authority.
    fn validate_current_graph_text_collision_strict(
        &self,
        _permit: &GraphTextWritePermit,
        target: &Path,
        target_identity: Option<ContentDigest>,
    ) -> io::Result<Arc<CompleteGraphTextAdmissionIndex>> {
        let target_relative = self.rel_path(target);
        let target_path = GraphTextPath::parse(target_relative.clone()).map_err(|error| {
            DirectSaveError::into_io(
                DirectSaveFailureCode::PrecheckNotPortable,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("guarded graph-text target is not portable: {error}"),
                ),
            )
        })?;
        let index = self.guarded_graph_text_identity_index()?;
        if let Some(sibling) = index
            .paths_by_portable_key
            .get(&target_path.portable_key())
            .and_then(|members| members.iter().find(|member| *member != &target_path))
        {
            return Err(DirectSaveError::into_io(
                DirectSaveFailureCode::PrecheckPortableCollision,
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "graph text paths share one portable case/NFC identity: {} and {target_relative}",
                        sibling.as_str()
                    ),
                ),
            ));
        }
        if let Some(identity) = target_identity {
            if let Some(sibling) = index
                .paths_by_file_resource
                .get(&identity)
                .and_then(|members| {
                    members
                        .iter()
                        .find(|member| member.as_str() != target_relative)
                })
            {
                return Err(DirectSaveError::into_io(
                    DirectSaveFailureCode::PrecheckResourceAlias,
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "graph text files alias one physical resource: {} and {target_relative}",
                            sibling
                        ),
                    ),
                ));
            }
        }
        Ok(index)
    }

    /// Read the parsed ownership evidence once. A clean missing page cache is
    /// repairable; failure-bearing cold evidence and partial or incoherent warm
    /// publication remain hard refusals rather than authority to rebuild around
    /// an unexplained gap.
    fn direct_creation_evidence(&self) -> io::Result<DirectCreationEvidence> {
        if self.graph_text_external_observation_pending() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "external graph-text changes are awaiting watcher reconciliation",
            ));
        }
        let cache = self.cache.read().unwrap();
        let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        let Some(_pages) = cache.as_ref() else {
            let published_failures = !self.page_index_failures.read().unwrap().is_empty();
            let retained_failures = self
                .effective_identity_index
                .read()
                .unwrap()
                .as_ref()
                .is_some_and(|index| !index.failures.is_empty());
            if published_failures || retained_failures {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "failure-bearing cold identity evidence cannot authorize name-only creation",
                ));
            }
            return Ok(DirectCreationEvidence::Cold);
        };
        let identity_index = self
            .effective_identity_index
            .read()
            .unwrap()
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "graph has unknown effective identities for name-only creation",
                )
            })?;
        if identity_index.generation() != generation {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "parsed identity evidence is not one coherent generation",
            ));
        }
        if !identity_index.failures.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "effective page identity is incomplete for name-only creation: {} unreadable or unparseable graph document(s)",
                    identity_index.failures.len()
                ),
            ));
        }
        Ok(DirectCreationEvidence::Warm {
            generation,
            identity_index,
        })
    }

    /// Bind creation to one coherent warm semantic-ownership generation. Cold
    /// evidence may own or join exactly one cache-build flight; the second read
    /// must be warm, and there is never a repair retry. Publication itself is
    /// target-local and no-replace; ordinary creation never hashes the graph.
    fn direct_creation_proof(
        &self,
        permit: &GraphTextWritePermit,
        target: &Path,
        kind: PageKind,
        name: &str,
    ) -> io::Result<(DirectCreationProof, bool)> {
        let evidence = match self.direct_creation_evidence()? {
            DirectCreationEvidence::Warm {
                generation,
                identity_index,
            } => DirectCreationEvidence::Warm {
                generation,
                identity_index,
            },
            DirectCreationEvidence::Cold => {
                let outcome = self.repair_page_cache_once(permit);
                if !outcome.installed() {
                    return Err(outcome.creation_error());
                }
                self.direct_creation_evidence()?
            }
        };
        let DirectCreationEvidence::Warm {
            generation,
            identity_index,
        } = evidence
        else {
            return Err(PageBuildOutcome::Failed.creation_error());
        };

        let target = GraphTextPath::parse(self.rel_path(target)).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("guarded graph-text target is not portable: {error}"),
            )
        })?;
        if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) != generation {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "effective page identity evidence changed during creation validation",
            ));
        }
        let requested_identity_elsewhere = identity_index
            .owners
            .get(&page_cache_key(kind, name))
            .is_some_and(|owners| !owners.is_empty());
        Ok((
            DirectCreationProof { target, generation },
            requested_identity_elsewhere,
        ))
    }

    fn current_effective_identity_index(&self) -> io::Result<Arc<EffectiveIdentityIndex>> {
        loop {
            let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
            if let Some(index) = self.effective_identity_index.read().unwrap().as_ref() {
                if index.generation() == generation {
                    return Ok(Arc::clone(index));
                }
            }
            let pages = self
                .cache
                .read()
                .unwrap()
                .as_ref()
                .map(Arc::clone)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "effective page identities are not warm for name-only creation",
                    )
                })?;
            let failures = self.page_index_failures.read().unwrap().clone();
            let built = Arc::new(build_effective_identity_index(
                generation,
                pages.as_slice(),
                failures,
            ));
            if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) != generation {
                continue;
            }
            *self.effective_identity_index.write().unwrap() = Some(Arc::clone(&built));
            return Ok(built);
        }
    }

    fn validate_name_only_effective_identity(
        &self,
        current_entries: &[PageEntry],
        kind: PageKind,
        name: &str,
    ) -> io::Result<bool> {
        let index = if self.cache.read().unwrap().is_none() {
            let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
            let failures = self.page_index_failures.read().unwrap().clone();
            let retained = self
                .effective_identity_index
                .read()
                .unwrap()
                .as_ref()
                .map(Arc::clone);
            if let Some(index) = retained {
                if index.generation() == generation {
                    index
                } else if !current_entries.is_empty() || !failures.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "cold graph has stale effective identities; name-only creation requires warm evidence",
                    ));
                } else {
                    let index = Arc::new(EffectiveIdentityIndex {
                        generation: std::sync::atomic::AtomicU64::new(generation),
                        owners: std::collections::HashMap::new(),
                        physical_paths: std::collections::HashSet::new(),
                        failures: Vec::new(),
                    });
                    *self.effective_identity_index.write().unwrap() = Some(Arc::clone(&index));
                    index
                }
            } else if !current_entries.is_empty() || !failures.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "cold graph has unknown effective identities; name-only creation requires warm evidence",
                ));
            } else {
                let index = Arc::new(EffectiveIdentityIndex {
                    generation: std::sync::atomic::AtomicU64::new(generation),
                    owners: std::collections::HashMap::new(),
                    physical_paths: std::collections::HashSet::new(),
                    failures: Vec::new(),
                });
                *self.effective_identity_index.write().unwrap() = Some(Arc::clone(&index));
                index
            }
        } else {
            self.current_effective_identity_index()?
        };
        if !index.failures.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "effective page identity is incomplete for name-only creation: {} unreadable or unparseable graph document(s)",
                    index.failures.len()
                ),
            ));
        }
        if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) != index.generation() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "effective page identity evidence changed during name-only creation",
            ));
        }
        let current_paths = current_entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<std::collections::HashSet<_>>();
        if current_paths != index.physical_paths {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "effective page identity evidence is stale or incomplete for name-only creation",
            ));
        }
        Ok(index
            .owners
            .get(&page_cache_key(kind, name))
            .is_some_and(|owners| !owners.is_empty()))
    }

    fn advance_effective_identity_after_upsert(
        &self,
        generation: u64,
        entry: &PageEntry,
        failures: Vec<String>,
    ) {
        let mut guard = self.effective_identity_index.write().unwrap();
        let Some(current) = guard.as_ref() else {
            return;
        };
        if current.generation().checked_add(1) != Some(generation) {
            *guard = None;
            return;
        }
        let mut next = (**current).clone();
        next.generation
            .store(generation, std::sync::atomic::Ordering::Release);
        next.physical_paths.insert(entry.path.clone());
        for owners in next.owners.values_mut() {
            owners.retain(|owner| owner.path != entry.path);
        }
        next.owners.retain(|_, owners| !owners.is_empty());
        next.owners
            .entry(page_cache_key(entry.kind, &entry.name))
            .or_default()
            .push(entry.clone());
        next.failures = failures;
        *guard = Some(Arc::new(next));
    }

    fn record_watcher_identity_failure(&self, path: &Path) {
        let failure = self.rel_path(path);
        let cache = self.cache.write().unwrap();
        let mut failures_guard = self.page_index_failures.write().unwrap();
        let mut failures = failures_guard.clone();
        if !failures.iter().any(|candidate| candidate == &failure) {
            failures.push(failure);
            failures.sort();
            failures.dedup();
        }
        let generation = self
            .cache_gen
            .fetch_add(1, std::sync::atomic::Ordering::Release)
            + 1;
        let next = match cache.as_ref() {
            Some(pages) => Some(Arc::new(build_effective_identity_index(
                generation,
                pages,
                failures.clone(),
            ))),
            None => {
                let retained = self.effective_identity_index.read().unwrap().clone();
                Some(Arc::new(retained.map_or_else(
                    || EffectiveIdentityIndex {
                        generation: std::sync::atomic::AtomicU64::new(generation),
                        owners: std::collections::HashMap::new(),
                        physical_paths: std::iter::once(path.to_path_buf()).collect(),
                        failures: failures.clone(),
                    },
                    |current| {
                        let mut next = (*current).clone();
                        next.generation
                            .store(generation, std::sync::atomic::Ordering::Release);
                        next.physical_paths.insert(path.to_path_buf());
                        next.failures = failures.clone();
                        next
                    },
                )))
            }
        };
        *self.effective_identity_index.write().unwrap() = next;
        *failures_guard = failures;
        drop(failures_guard);
        drop(cache);
        *self.page_list_cache.write().unwrap() = None;
        *self.find_entry_cache.write().unwrap() = None;
        *self.derived_cache.write().unwrap() = None;
    }

    fn clear_watcher_identity_failure_after_reconciliation(&self, entry: &PageEntry) {
        let cache = self.cache.write().unwrap();
        let mut failures_guard = self.page_index_failures.write().unwrap();
        if !failures_guard
            .iter()
            .any(|failure| failure == &entry.rel_path)
        {
            return;
        }
        let mut failures = failures_guard.clone();
        failures.retain(|failure| failure != &entry.rel_path);
        let generation = self
            .cache_gen
            .fetch_add(1, std::sync::atomic::Ordering::Release)
            + 1;
        let next = match cache.as_ref() {
            Some(pages) => Arc::new(build_effective_identity_index(
                generation,
                pages,
                failures.clone(),
            )),
            None => {
                let retained = self.effective_identity_index.read().unwrap().clone();
                let mut next =
                    retained
                        .as_deref()
                        .cloned()
                        .unwrap_or_else(|| EffectiveIdentityIndex {
                            generation: std::sync::atomic::AtomicU64::new(generation),
                            owners: std::collections::HashMap::new(),
                            physical_paths: std::collections::HashSet::new(),
                            failures: Vec::new(),
                        });
                next.generation
                    .store(generation, std::sync::atomic::Ordering::Release);
                next.physical_paths.insert(entry.path.clone());
                for owners in next.owners.values_mut() {
                    owners.retain(|owner| owner.path != entry.path);
                }
                next.owners.retain(|_, owners| !owners.is_empty());
                next.owners
                    .entry(page_cache_key(entry.kind, &entry.name))
                    .or_default()
                    .push(entry.clone());
                next.failures = failures.clone();
                Arc::new(next)
            }
        };
        *self.effective_identity_index.write().unwrap() = Some(next);
        *failures_guard = failures;
        drop(failures_guard);
        drop(cache);
        *self.derived_cache.write().unwrap() = None;
    }

    /// Validate mutation authority independently from discovery/read authority.
    /// Existing semantic duplicates may be edited only through their captured
    /// exact physical owner. Portable path aliases and same-file aliases remain
    /// readable but every member is non-writable.
    fn validate_graph_text_target(
        &self,
        permit: &GraphTextWritePermit,
        target: &Path,
        requested_identity: Option<(PageKind, &str)>,
    ) -> io::Result<ExactGraphValidation> {
        let loaded_target = self.load_validated_graph_text_target(permit, target)?;
        if let Some(loaded) = loaded_target.as_ref() {
            self.validate_existing_graph_text_target_local(permit, target, loaded.file_identity)?;
            return Ok(ExactGraphValidation {
                target: loaded_target,
                requested_identity_elsewhere: false,
                creation_proof: None,
            });
        }
        if let Some((kind, name)) = requested_identity {
            let (creation_proof, requested_identity_elsewhere) =
                self.direct_creation_proof(permit, target, kind, name)?;
            return Ok(ExactGraphValidation {
                target: None,
                requested_identity_elsewhere,
                creation_proof: Some(creation_proof),
            });
        }
        let index = self.validate_current_graph_text_collision_strict(permit, target, None)?;
        let requested_identity_elsewhere = match (loaded_target.as_ref(), requested_identity) {
            (None, Some((kind, name))) => {
                let retained_collision = index
                    .paths_by_semantic_key
                    .get(&(
                        match kind {
                            PageKind::Page => 0,
                            PageKind::Journal => 1,
                        },
                        crate::refs::page_key(name),
                    ))
                    .is_some_and(|members| !members.is_empty());
                let entries = index
                    .files_by_exact_path
                    .iter()
                    .map(|(_, record)| record.semantic.clone())
                    .collect::<Vec<_>>();
                retained_collision
                    || self.validate_name_only_effective_identity(&entries, kind, name)?
            }
            _ => false,
        };
        Ok(ExactGraphValidation {
            target: loaded_target,
            requested_identity_elsewhere,
            creation_proof: None,
        })
    }

    fn graph_text_read_to_string(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<String> {
        self.graph_text_read_optional_text(permit, path)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn graph_text_content_rev_matches(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        expected: &str,
    ) -> io::Result<bool> {
        if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid graph content revision",
            ));
        }
        let target = self.graph_text_target(permit, path, false)?;
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let mut file = open_projection_file_nofollow(target.parent(), &target.filename)?;
        let mut hash = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(read as u64)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "read-byte overflow"))?;
            if total > MAX_PROJECTION_EVIDENCE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph revision evidence exceeds the reload bound",
                ));
            }
            hash.update(&buffer[..read]);
        }
        Ok(format!("{:x}", hash.finalize()) == expected)
    }

    fn graph_text_file_equals_bytes(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        expected: &[u8],
    ) -> io::Result<bool> {
        let target = self.graph_text_target(permit, path, false)?;
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let mut file = open_projection_file_nofollow(target.parent(), &target.filename)?;
        if file.metadata()?.len() != expected.len() as u64 {
            return Ok(false);
        }
        let mut offset = 0usize;
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                return Ok(offset == expected.len());
            }
            let end = offset
                .checked_add(read)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "read-byte overflow"))?;
            if expected.get(offset..end) != Some(&buffer[..read]) {
                return Ok(false);
            }
            offset = end;
        }
    }

    fn graph_text_read_to_string_with_budget(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        budget: &RetainedContentBudget,
        resource: &'static str,
    ) -> io::Result<BudgetedString> {
        let target = self.graph_text_target(permit, path, false)?;
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let (_file, bytes, mut reservation) = open_and_read_projection_regular_with_budget(
            target.parent(),
            &target.filename,
            MAX_PROJECTION_EVIDENCE_BYTES,
            budget,
            resource,
        )?;
        let value = String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "graph text file is not valid UTF-8",
            )
        })?;
        reservation.resize(usize_to_u64(value.capacity())?, resource)?;
        Ok(BudgetedString { value, reservation })
    }

    fn graph_text_exists(&self, permit: &GraphTextWritePermit, path: &Path) -> io::Result<bool> {
        let target = match self.graph_text_target(permit, path, false) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match target.parent().symlink_metadata(&target.filename) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
            Ok(_) => {
                projection_optional_regular_metadata(target.parent(), &target.filename)?;
                Ok(true)
            }
        }
    }

    fn validate_direct_creation_proof_before_mutation(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        proof: &DirectCreationProof,
    ) -> io::Result<()> {
        let graph_text_path = GraphTextPath::parse(self.rel_path(path)).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("guarded graph-text target is not portable: {error}"),
            )
        })?;
        if graph_text_path != proof.target {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "creation proof does not bind one absent exact target",
            ));
        }
        if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) != proof.generation {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "effective page identity evidence changed before creation publication",
            ));
        }
        self.validate_graph_text_portable_aliases_path_local(permit, &graph_text_path, true)?;
        match self.graph_text_target(permit, path, false) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(target) => match target.parent().symlink_metadata(&target.filename) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
                Ok(_) => {
                    projection_optional_regular_metadata(target.parent(), &target.filename)?;
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "direct creation target is already present",
                    ));
                }
            },
        }
        if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) != proof.generation {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "effective page identity evidence changed during creation publication validation",
            ));
        }
        Ok(())
    }

    fn graph_text_atomic_create_with_proof(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        bytes: &[u8],
        proof: DirectCreationProof,
        editor_episode: Option<&ConflictEditorEpisode>,
    ) -> io::Result<()> {
        let _identity = self.lock_graph_text_identity_mutation()?;
        self.validate_direct_creation_proof_before_mutation(permit, path, &proof)?;
        let target = self.graph_text_target(permit, path, true)?;
        // Parent creation is itself a mutation, so the first validation above
        // precedes it. Re-run only the path-local portable/no-follow boundary
        // after the chain exists; the graph-wide census remains singular.
        self.validate_direct_creation_proof_before_mutation(permit, path, &proof)?;
        let temp = create_projection_temp(target.parent(), &target.filename, bytes)?;
        graph_text_write_before_mutation_hook()?;
        if self.graph_text_external_observation_pending() {
            let _ = target.parent().remove_file(&temp);
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "external graph-text changes arrived before creation publication",
            ));
        }
        if let Err(error) =
            self.validate_graph_text_portable_aliases_path_local(permit, &proof.target, true)
        {
            let _ = target.parent().remove_file(&temp);
            return Err(error);
        }
        if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) != proof.generation {
            let _ = target.parent().remove_file(&temp);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "effective page identity evidence changed before no-replace publication",
            ));
        }
        if let Err(error) =
            move_graph_text_exact_no_replace(target.parent(), &temp, &target.filename, bytes)
        {
            let _ = target.parent().remove_file(&temp);
            if error.kind() == io::ErrorKind::AlreadyExists && editor_episode.is_some() {
                return Err(self.observe_editor_conflict(
                    permit,
                    path,
                    editor_episode,
                    EditorConflictSite::CreatePublicationCollision,
                ));
            }
            return Err(error);
        }
        self.finish_tine_owned_graph_text_identity_paths(std::iter::once(path))
    }

    fn graph_text_atomic_write_with_conflict(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        bytes: &[u8],
        create_new: bool,
        editor_episode: Option<&ConflictEditorEpisode>,
    ) -> io::Result<()> {
        self.graph_text_atomic_write_validated(
            permit,
            path,
            bytes,
            create_new,
            editor_episode,
            GraphTextPublicationValidation::CompleteIndex,
        )
    }

    fn graph_text_atomic_write_from_transaction_inventory(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        bytes: &[u8],
        create_new: bool,
    ) -> io::Result<()> {
        self.graph_text_atomic_write_validated(
            permit,
            path,
            bytes,
            create_new,
            None,
            GraphTextPublicationValidation::TransactionInventory,
        )
    }

    fn graph_text_atomic_write_validated(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        bytes: &[u8],
        create_new: bool,
        editor_episode: Option<&ConflictEditorEpisode>,
        validation: GraphTextPublicationValidation,
    ) -> io::Result<()> {
        if !create_new {
            if validation == GraphTextPublicationValidation::CompleteIndex {
                let _ = self.guarded_graph_text_identity_index()?;
            }
            let expected_identity = self
                .graph_text_optional_file_identity(permit, path)?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            return self.graph_text_atomic_replace_bound(
                permit,
                path,
                bytes,
                expected_identity,
                None,
                editor_episode,
                None,
            );
        }
        let _identity = self.lock_graph_text_identity_mutation()?;
        // Establish the retained baseline before creating the staged inode. The
        // temp name is deliberately outside the graph-text namespace, but its
        // physical identity would otherwise be captured as a second owner when
        // the staged inode is later published at `path`.
        if validation == GraphTextPublicationValidation::CompleteIndex {
            let _ = self.guarded_graph_text_identity_index()?;
        }
        let graph_text_path = GraphTextPath::parse(self.rel_path(path)).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("guarded graph-text target is not portable: {error}"),
            )
        })?;
        if validation == GraphTextPublicationValidation::TransactionInventory {
            self.validate_graph_text_portable_aliases_path_local(
                permit,
                &graph_text_path,
                create_new,
            )?;
        }
        let target = self.graph_text_target(permit, path, true)?;
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let temp = create_projection_temp(target.parent(), &target.filename, bytes)?;
        graph_text_write_before_mutation_hook()?;
        let validation_result = match validation {
            GraphTextPublicationValidation::CompleteIndex => self
                .validate_current_graph_text_collision_strict(
                    permit,
                    path,
                    self.graph_text_optional_file_identity(permit, path)?,
                )
                .map(|_| ()),
            GraphTextPublicationValidation::PathLocal
            | GraphTextPublicationValidation::TransactionInventory => (|| {
                self.validate_graph_text_portable_aliases_path_local(
                    permit,
                    &graph_text_path,
                    create_new,
                )?;
                match target.parent().symlink_metadata(&target.filename) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(error),
                    Ok(_) => Err(io::Error::from(io::ErrorKind::AlreadyExists)),
                }
            })(),
        };
        if let Err(error) = validation_result {
            let _ = target.parent().remove_file(&temp);
            return Err(error);
        }
        let result =
            move_graph_text_exact_no_replace(target.parent(), &temp, &target.filename, bytes);
        if let Err(error) = result {
            let _ = target.parent().remove_file(&temp);
            if error.kind() == io::ErrorKind::AlreadyExists && editor_episode.is_some() {
                return Err(self.observe_editor_conflict(
                    permit,
                    path,
                    editor_episode,
                    EditorConflictSite::CreatePublicationCollision,
                ));
            }
            return Err(error);
        }
        self.finish_tine_owned_graph_text_identity_paths(std::iter::once(path))
    }

    /// Replace an existing editor target without ever issuing an overwrite
    /// rename against its live name. The old name is first retired with
    /// no-replace through the retained parent capability, then its exact file
    /// identity (and, for normal saves, bytes) are validated from the retired
    /// inode. A different file installed after the caller's final check is moved
    /// back with the same no-replace primitive; its bytes and identity survive.
    ///
    /// Publication also uses no-replace, so an external creator in the brief
    /// retired-name interval wins. In that case the displaced inode remains in a
    /// same-directory hidden recovery name and the user's staged bytes remain in
    /// their own hidden staged-recovery name instead of any version being
    /// overwritten.
    fn graph_text_atomic_replace_bound(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        bytes: &[u8],
        expected_identity: ContentDigest,
        expected_bytes: Option<&[u8]>,
        editor_episode: Option<&ConflictEditorEpisode>,
        turn_short_id: Option<[u8; 4]>,
    ) -> io::Result<()> {
        let _identity = self.lock_graph_text_identity_mutation()?;
        use std::sync::atomic::{AtomicU64, Ordering};
        static RECOVERY_SEQ: AtomicU64 = AtomicU64::new(0);

        let target = self.graph_text_target(permit, path, false)?;
        let graph_text_path = GraphTextPath::parse(self.rel_path(path)).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("guarded graph-text target is not portable: {error}"),
            )
        })?;
        if let Err(error) = self.validate_existing_graph_text_target_exact(
            &target,
            &graph_text_path,
            Some(expected_identity),
        ) {
            if editor_episode.is_some()
                && (error.kind() == io::ErrorKind::NotFound
                    || error
                        .to_string()
                        .contains("changed at the local identity validation boundary"))
            {
                return Err(self.observe_editor_conflict(
                    permit,
                    path,
                    editor_episode,
                    EditorConflictSite::ReplacePreRetirement,
                ));
            }
            return Err(error);
        }
        preflight_projection_chain(&target.chain)?;
        let temp =
            create_editor_staged_recovery(target.parent(), &target.filename, bytes, turn_short_id)?;
        let staged_identity = match (|| {
            let staged_file = open_projection_file_nofollow(target.parent(), &temp)?;
            let identity = canonical_projection_file_resource_id(&staged_file)?;
            validate_graph_text_single_link(&staged_file, graph_text_path.as_str())?;
            Ok::<_, io::Error>(identity)
        })() {
            Ok(identity) => identity,
            Err(error) => {
                let _ = target.parent().remove_file(&temp);
                return Err(error);
            }
        };
        let process = std::process::id();
        let sequence = RECOVERY_SEQ.fetch_add(1, Ordering::Relaxed);
        let recovery = match turn_short_id {
            Some(turn) => format!(
                ".{}.{process}.{sequence}.{}.editor-recovery",
                target.filename,
                short_turn_id(turn),
            ),
            None => format!(".{}.{process}.{sequence}.editor-recovery", target.filename,),
        };
        let retired_cleanup = format!(".{}.{process}.{sequence}.editor-retired", target.filename,);
        let mut retired = false;
        let mut published = false;
        let mut conflict_site = None;
        let mut retired_conflict_snapshot = None;
        let mut restore_succeeded = false;
        let result = (|| {
            // Deterministic tests replace the target here: after normal-save's
            // final byte reread and after force-save's final retained-identity
            // validation, but before the first live-name mutation.
            graph_text_write_before_mutation_hook()?;
            self.validate_graph_text_portable_aliases_path_local(permit, &graph_text_path, false)?;
            if let Err(error) = self.validate_existing_graph_text_target_exact(
                &target,
                &graph_text_path,
                Some(expected_identity),
            ) {
                if editor_episode.is_some()
                    && (error.kind() == io::ErrorKind::NotFound
                        || error
                            .to_string()
                            .contains("changed at the local identity validation boundary"))
                {
                    return Err(self.observe_editor_conflict(
                        permit,
                        path,
                        editor_episode,
                        EditorConflictSite::ReplacePreRetirement,
                    ));
                }
                return Err(error);
            }
            let rename_noreplace = |from: &str, to: &str, expected: &[u8]| {
                move_graph_text_exact_no_replace(target.parent(), from, to, expected)
            };
            let (live_file, live_bytes) =
                open_and_read_projection_regular(target.parent(), &target.filename)?;
            if canonical_projection_file_resource_id(&live_file)? != expected_identity {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "graph text target changed before durable retirement",
                ));
            }
            validate_graph_text_single_link(&live_file, graph_text_path.as_str())?;
            drop(live_file);
            rename_noreplace(&target.filename, &recovery, &live_bytes)?;
            retired = true;

            let (retired_file, retired_bytes) =
                open_and_read_projection_regular(target.parent(), &recovery)?;
            let retired_identity = canonical_projection_file_resource_id(&retired_file)?;
            validate_graph_text_single_link(&retired_file, graph_text_path.as_str())?;
            drop(retired_file);
            if retired_identity != expected_identity
                || expected_bytes.is_some_and(|expected| retired_bytes != expected)
            {
                conflict_site = Some(EditorConflictSite::ReplaceRetiredMismatch);
                retired_conflict_snapshot = Some((retired_bytes, retired_identity));
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "graph text target changed at the identity-bound publication boundary",
                ));
            }
            graph_text_write_after_retire_hook()?;

            let staged_file = open_projection_file_nofollow(target.parent(), &temp)?;
            if canonical_projection_file_resource_id(&staged_file)? != staged_identity {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "staged editor identity changed before publication",
                ));
            }
            validate_graph_text_single_link(&staged_file, graph_text_path.as_str())?;
            drop(staged_file);

            if let Err(error) = rename_noreplace(&temp, &target.filename, bytes) {
                if error.kind() == io::ErrorKind::AlreadyExists && editor_episode.is_some() {
                    conflict_site = Some(EditorConflictSite::ReplacePublicationCollision);
                }
                return Err(error);
            }
            published = true;
            journal_projection_after_publish_hook()?;
            if let Err(error) = self.validate_existing_graph_text_target_exact(
                &target,
                &graph_text_path,
                Some(staged_identity),
            ) {
                if editor_episode.is_some()
                    && (error.kind() == io::ErrorKind::NotFound
                        || error
                            .to_string()
                            .contains("changed at the local identity validation boundary"))
                {
                    conflict_site = Some(EditorConflictSite::ReplacePostPublication);
                }
                return Err(error);
            }
            move_graph_text_exact_no_replace(
                target.parent(),
                &recovery,
                &retired_cleanup,
                &retired_bytes,
            )?;
            let _ = target.parent().remove_file(&retired_cleanup);
            retired = false;
            Ok(())
        })();

        let outcome = match result {
            Ok(()) => {
                let _ = target.parent().remove_file(&temp);
                Ok(())
            }
            Err(primary) => {
                if retired && !published {
                    let restore = graph_text_write_before_restore_hook().and_then(|()| {
                        let recovery_file =
                            open_projection_file_nofollow(target.parent(), &recovery)?;
                        if canonical_projection_file_resource_id(&recovery_file)?
                            != expected_identity
                        {
                            return Err(io::Error::new(
                                io::ErrorKind::AlreadyExists,
                                "displaced target identity changed before restore",
                            ));
                        }
                        validate_graph_text_single_link(&recovery_file, graph_text_path.as_str())?;
                        let recovery_bytes = read_projection_regular(target.parent(), &recovery)?;
                        move_graph_text_exact_no_replace(
                            target.parent(),
                            &recovery,
                            &target.filename,
                            &recovery_bytes,
                        )
                    });
                    match restore {
                        Ok(()) => {
                            retired = false;
                            restore_succeeded = true;
                            debug_assert!(!retired || published);
                            let _ = target.parent().remove_file(&temp);
                            Err(primary)
                        }
                        Err(restore_error) => Err(io::Error::new(
                            primary.kind(),
                            format!(
                                "{primary}; displaced target retained as {recovery}, \
                                     staged editor bytes retained as {temp}, \
                                     but exact-identity restore failed: {restore_error}"
                            ),
                        )),
                    }
                } else {
                    debug_assert!(!retired || published);
                    let _ = target.parent().remove_file(&temp);
                    Err(primary)
                }
            }
        };
        match outcome {
            Ok(()) => self.finish_tine_owned_graph_text_identity_paths(std::iter::once(path)),
            Err(mut error) => {
                if let Some(site) = conflict_site {
                    error = if matches!(site, EditorConflictSite::ReplaceRetiredMismatch)
                        && restore_succeeded
                    {
                        match retired_conflict_snapshot {
                            Some((bytes, resource_identity)) => match String::from_utf8(bytes) {
                                Ok(bytes) => self.conflict_error_from_snapshot(
                                    path,
                                    editor_episode,
                                    site,
                                    ConflictSnapshot::Present {
                                        revision: content_rev(&bytes),
                                        resource_identity,
                                    },
                                    Some(bytes),
                                ),
                                Err(_) => io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "graph text file is not valid UTF-8",
                                ),
                            },
                            None => error,
                        }
                    } else {
                        self.observe_editor_conflict(permit, path, editor_episode, site)
                    };
                }
                self.invalidate_guarded_graph_text_identity(format!(
                    "identity-bound replacement failed after staging: {error}"
                ));
                Err(error)
            }
        }
    }

    fn graph_text_move_noreplace(
        &self,
        permit: &GraphTextWritePermit,
        source: &Path,
        destination: &Path,
    ) -> io::Result<()> {
        self.graph_text_move_noreplace_validated(
            permit,
            source,
            destination,
            GraphTextPublicationValidation::PathLocal,
        )
    }

    fn graph_text_move_noreplace_from_transaction_inventory(
        &self,
        permit: &GraphTextWritePermit,
        source: &Path,
        destination: &Path,
    ) -> io::Result<()> {
        self.graph_text_move_noreplace_validated(
            permit,
            source,
            destination,
            GraphTextPublicationValidation::TransactionInventory,
        )
    }

    /// Move one exact hidden file produced by the editor publication protocol.
    /// The retained source is validated directly rather than through
    /// `GraphTextPath`: hidden publication artifacts are not ordinary documents,
    /// and recovery must not treat them as such. Note that `GraphTextPath` itself
    /// does NOT reject a leading-dot name — `is_graph_text_path` only requires a
    /// non-empty stem and a graph-text extension — so this validation is the
    /// boundary, not a redundant second check.
    /// `graph_text_path_accepts_leading_dot_name` in `graph_text_path` keeps that
    /// statement honest.
    fn graph_text_move_editor_recovery_noreplace(
        &self,
        permit: &GraphTextWritePermit,
        source_path: &Path,
        destination_path: &Path,
    ) -> io::Result<ContentDigest> {
        let _identity = self.lock_graph_text_identity_mutation()?;
        let destination_graph_text = GraphTextPath::parse(self.rel_path(destination_path))
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("editor recovery destination is not portable: {error}"),
                )
            })?;
        self.validate_graph_text_portable_aliases_path_local(
            permit,
            &destination_graph_text,
            true,
        )?;

        let source = self.graph_text_target(permit, source_path, false)?;
        projection_optional_regular_metadata(source.parent(), &source.filename)?;
        let source_file = open_projection_file_nofollow(source.parent(), &source.filename)?;
        let source_identity = canonical_projection_file_resource_id(&source_file)?;
        validate_graph_text_single_link(&source_file, &self.rel_path(source_path))?;
        drop(source_file);

        let destination = self.graph_text_target(permit, destination_path, true)?;
        match destination.parent().symlink_metadata(&destination.filename) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(error) => return Err(error),
        }
        graph_text_write_before_mutation_hook()?;
        let rebound = open_projection_file_nofollow(source.parent(), &source.filename)?;
        if canonical_projection_file_resource_id(&rebound)? != source_identity {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "editor recovery artifact changed before reconciliation",
            ));
        }
        validate_graph_text_single_link(&rebound, &self.rel_path(source_path))?;
        self.validate_graph_text_portable_aliases_path_local(
            permit,
            &destination_graph_text,
            true,
        )?;
        match destination.parent().symlink_metadata(&destination.filename) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(error) => return Err(error),
        }
        rename_graph_text_noreplace(
            source.parent(),
            &source.filename,
            destination.parent(),
            &destination.filename,
        )?;
        // Quarantine/restore destinations are sole-authority names. Make the
        // destination chain durable before the source removal: a crash between
        // these barriers may leave a duplicate source entry on non-journaling
        // media, but can never lose the retained object (§4.5).
        sync_projection_chain_required(&destination.chain)?;
        sync_projection_chain_required(&source.chain)?;
        self.finish_tine_owned_graph_text_identity_paths(std::iter::once(destination_path))?;
        Ok(source_identity)
    }

    fn graph_text_move_noreplace_validated(
        &self,
        permit: &GraphTextWritePermit,
        source: &Path,
        destination: &Path,
        validation: GraphTextPublicationValidation,
    ) -> io::Result<()> {
        let _identity = self.lock_graph_text_identity_mutation()?;
        if validation == GraphTextPublicationValidation::CompleteIndex {
            let _ = self.guarded_graph_text_identity_index()?;
        }
        let source_path = source.to_path_buf();
        let destination_path = destination.to_path_buf();
        let source_graph_text =
            GraphTextPath::parse(self.rel_path(&source_path)).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("guarded graph-text source is not portable: {error}"),
                )
            })?;
        let destination_graph_text = GraphTextPath::parse(self.rel_path(&destination_path))
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("guarded graph-text destination is not portable: {error}"),
                )
            })?;
        if validation != GraphTextPublicationValidation::CompleteIndex {
            self.validate_graph_text_portable_aliases_path_local(
                permit,
                &source_graph_text,
                false,
            )?;
            self.validate_graph_text_portable_aliases_path_local(
                permit,
                &destination_graph_text,
                true,
            )?;
        }
        let source = self.graph_text_target(permit, &source_path, false)?;
        projection_optional_regular_metadata(source.parent(), &source.filename)?;
        let destination = self.graph_text_target(permit, &destination_path, true)?;
        match destination.parent().symlink_metadata(&destination.filename) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(error) => return Err(error),
        }
        graph_text_write_before_mutation_hook()?;
        if validation != GraphTextPublicationValidation::CompleteIndex {
            self.validate_graph_text_portable_aliases_path_local(
                permit,
                &source_graph_text,
                false,
            )?;
            self.validate_existing_graph_text_target_exact(&source, &source_graph_text, None)?;
            self.validate_graph_text_portable_aliases_path_local(
                permit,
                &destination_graph_text,
                true,
            )?;
            match destination.parent().symlink_metadata(&destination.filename) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
                Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            }
        }
        rename_graph_text_noreplace(
            source.parent(),
            &source.filename,
            destination.parent(),
            &destination.filename,
        )?;
        sync_projection_chain_required(&source.chain)?;
        sync_projection_chain_required(&destination.chain)?;
        self.finish_tine_owned_graph_text_identity_paths([
            source_path.as_path(),
            destination_path.as_path(),
        ])
    }

    fn graph_text_move_to_trash(
        &self,
        permit: &GraphTextWritePermit,
        source: &Path,
        destination: &Path,
        trash: &Path,
    ) -> io::Result<()> {
        self.graph_text_create_dir_all(permit, trash)
            .map_err(|error| {
                let display = trash.strip_prefix(&self.root).unwrap_or(trash).display();
                io::Error::new(
                    error.kind(),
                    format!("could not prepare trash path {display}: {error}"),
                )
            })?;
        self.graph_text_move_noreplace(permit, source, destination)
    }

    /// Capture exact current page bytes through the same retained graph
    /// capability used by the guarded writer.
    pub(crate) fn read_projection_input(
        &self,
        path: &GraphTextPath,
    ) -> io::Result<Option<Vec<u8>>> {
        require_projection_platform()?;
        let target = self.projection_page_target(path.as_str())?;
        let lock = self.page_lock(&target.absolute_path);
        let _guard = lock.lock().unwrap();
        let Some(parent) = self.projection_parent_optional(&target)? else {
            return Ok(None);
        };
        self.ensure_projection_parent_binding(&parent, &target)?;
        self.ensure_projection_target_shape(&parent, &target)?;
        read_projection_optional(parent.final_dir(), &target.filename)
    }

    /// Classify one exact graph-text path against this graph's configured text
    /// roots. The longest component-boundary match wins; equal roots are
    /// ambiguous and therefore rejected instead of guessed.
    pub(crate) fn classify_graph_text_path(
        &self,
        path: &GraphTextPath,
    ) -> Result<GraphTextKind, UnsafeGraphTextPath> {
        let path_components = path.as_str().split('/').collect::<Vec<_>>();
        let page_root = configured_root_components(&self.config.pages_dir);
        let journal_root = configured_root_components(&self.config.journals_dir);
        let Some(page_root) = page_root else {
            return Err(UnsafeGraphTextPath(path.as_str().to_owned()));
        };
        let Some(journal_root) = journal_root else {
            return Err(UnsafeGraphTextPath(path.as_str().to_owned()));
        };
        if page_root == journal_root {
            return Err(UnsafeGraphTextPath(path.as_str().to_owned()));
        }

        let page_matches =
            path_components.len() > page_root.len() && path_components.starts_with(&page_root);
        let journal_matches = path_components.len() > journal_root.len()
            && path_components.starts_with(&journal_root);
        match (page_matches, journal_matches) {
            (true, false) => Ok(GraphTextKind::Page),
            (false, true) => Ok(GraphTextKind::Journal),
            (true, true) if page_root.len() > journal_root.len() => Ok(GraphTextKind::Page),
            (true, true) if journal_root.len() > page_root.len() => Ok(GraphTextKind::Journal),
            _ => Err(UnsafeGraphTextPath(path.as_str().to_owned())),
        }
    }

    /// Capture the resource retained by this Graph even when its ambient path
    /// has subsequently been moved or reserved by a replacement graph. The
    /// graph-text write gate and retained directory capability are the authority;
    /// checking the ambient path here would both reject supported moves and
    /// accidentally inspect the replacement resource.
    fn capture_retained_graph_text_identity_with_limits(
        &self,
        limits: GraphTextCaptureLimits,
    ) -> io::Result<(GraphTextCapture, u64)> {
        // GH #267 / F3. The two passes must agree, and ANY concurrent filesystem
        // activity anywhere in the graph makes them disagree -- which on a
        // Syncthing, Dropbox or OneDrive folder is not an anomaly, it is the
        // steady state. A single disagreement used to surface as a failed save.
        //
        // Disagreement means "something moved while we looked", not "the graph
        // is broken", so retry it in place a bounded number of times. Only that
        // one outcome is retried; every other error still surfaces at once.
        // The caller holds the identity-mutation authority across all attempts,
        // so this cannot interleave with one of our own writes.
        const CAPTURE_ATTEMPTS: usize = 4;
        let mut last_disagreement = None;
        for _ in 0..CAPTURE_ATTEMPTS {
            match self.attempt_retained_graph_text_identity_capture(limits) {
                Ok(captured) => return Ok(captured),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    last_disagreement = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_disagreement.unwrap_or_else(|| {
            DirectSaveError::into_io(
                DirectSaveFailureCode::PrecheckInterrupted,
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    "graph inventory changed during retained identity capture",
                ),
            )
        }))
    }

    fn attempt_retained_graph_text_identity_capture(
        &self,
        limits: GraphTextCaptureLimits,
    ) -> io::Result<(GraphTextCapture, u64)> {
        require_projection_platform()?;
        let permit = self.admit_graph_text_writer()?;
        let first = collect_graph_text_capture_inner(self, &permit, true, limits, 0, false, true)?;
        graph_text_capture_revalidation_hook(&self.root)?;
        let second = collect_graph_text_capture_inner(
            self,
            &permit,
            false,
            limits,
            first.peak_build_charge,
            false,
            true,
        )?;
        if !graph_text_captures_match(&first, &second) {
            return Err(DirectSaveError::into_io(
                DirectSaveFailureCode::PrecheckInterrupted,
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    "graph inventory changed during retained identity capture",
                ),
            ));
        }
        let combined_capture_bytes =
            checked_add_bytes(first.peak_build_charge, second.peak_build_charge)?;
        if combined_capture_bytes > limits.peak_build_bytes {
            return Err(graph_text_capture_limit_error("peak build memory"));
        }
        Ok((first, combined_capture_bytes))
    }

    /// Classify one exact feed path without duplicating scope policy.
    pub fn classify_graph_text_exact_feed_path(
        &self,
        relative: &str,
    ) -> io::Result<GraphTextExactFeedPathClass> {
        validate_graph_text_exact_feed_relative(relative)?;
        if relative.eq_ignore_ascii_case(CONFIG_RELATIVE_PATH) {
            return Ok(GraphTextExactFeedPathClass::Configuration);
        }
        let mut parent = String::new();
        let components = relative.split('/').collect::<Vec<_>>();
        for component in &components[..components.len().saturating_sub(1)] {
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(component);
            if !self.graph_text_scope.should_descend(&parent) {
                return Ok(GraphTextExactFeedPathClass::Excluded);
            }
        }
        Ok(GraphTextExactFeedPathClass::RetainedFile)
    }

    fn prepare_graph_text_admission_final_state(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        relative: String,
        batch_scratch: u64,
        actual_charges: &mut GraphTextExactFeedBatchActualCharges,
        require_ambient_binding: bool,
        decode_semantics: bool,
    ) -> io::Result<PreparedGraphTextAdmissionFinalState> {
        let target = self.graph_text_exact_path(&relative, false)?;
        let parent = self.graph_text_event_parent_policy(&target, require_ambient_binding)?;
        validate_graph_text_event_parent(index, &target, &parent)?;
        match parent.final_dir().symlink_metadata(&target.filename) {
            Ok(metadata) if metadata.is_file() => self
                .prepare_graph_text_file_upsert_for_batch(
                    index,
                    relative,
                    batch_scratch,
                    actual_charges,
                    require_ambient_binding,
                    decode_semantics,
                )
                .map(PreparedGraphTextAdmissionFinalState::Present),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::Interrupted,
                graph_text_exact_feed_failure_cause(&format!(
                    "touched path became non-regular: {relative}"
                )),
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => self
                .prepare_graph_text_file_remove(index, relative, require_ambient_binding)
                .map(PreparedGraphTextAdmissionFinalState::Absent),
            Err(error) => Err(error),
        }
    }

    fn revalidate_prepared_graph_text_admission_batch(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        prepared: &[PreparedGraphTextAdmissionFinalState],
        batch_scratch: u64,
        prepared_growth: u64,
        require_ambient_binding: bool,
    ) -> io::Result<()> {
        self.ensure_graph_text_admission_snapshot_binding_policy(index, require_ambient_binding)?;
        let live = checked_add_bytes(index.permanent_bytes, batch_scratch)
            .and_then(|bytes| checked_add_bytes(bytes, prepared_growth))?;
        let remaining_peak = index
            .peak_limit
            .checked_sub(live)
            .ok_or_else(|| graph_text_capture_limit_error("peak build memory"))?;
        let mut raw_bytes = 0_u64;
        for final_state in prepared {
            let (relative, expected) = match final_state {
                PreparedGraphTextAdmissionFinalState::Present(upsert) => {
                    (&upsert.relative, Some(upsert))
                }
                PreparedGraphTextAdmissionFinalState::Absent(remove) => (&remove.relative, None),
            };
            let target = self.graph_text_exact_path(relative, false)?;
            let parent = self.graph_text_event_parent_policy(&target, require_ambient_binding)?;
            validate_graph_text_event_parent(index, &target, &parent)?;
            match expected {
                Some(upsert) => {
                    let remaining_raw = MAX_GRAPH_TEXT_EXACT_FEED_BATCH_RAW_BYTES
                        .checked_sub(raw_bytes)
                        .ok_or_else(|| {
                            graph_text_capture_limit_error("exact feed batch aggregate raw bytes")
                        })?;
                    let expected_len = upsert.description.byte_length();
                    if expected_len > remaining_raw {
                        return Err(graph_text_capture_limit_error(
                            "exact feed batch aggregate raw bytes",
                        ));
                    }
                    let file = open_projection_file_nofollow(parent.final_dir(), &target.filename)?;
                    if canonical_projection_file_resource_id(&file)? != upsert.file_resource_id
                        || projection_file_link_count(&file)? != upsert.link_count
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            format!("exact feed batch resource/link proof changed: {relative}"),
                        ));
                    }
                    let (_, description, resource, _, _) =
                        read_projection_optional_bound_capture_with_limits(
                            parent.final_dir(),
                            &target.filename,
                            expected_len,
                            remaining_peak,
                        )?
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::Interrupted,
                                format!(
                                    "exact feed batch path disappeared during final proof: {relative}"
                                ),
                            )
                        })?;
                    raw_bytes = checked_add_bytes(raw_bytes, expected_len)?;
                    if description != upsert.description || resource != upsert.file_resource_id {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            format!(
                                "exact feed batch bytes/resource changed during final proof: {relative}"
                            ),
                        ));
                    }
                }
                None => match parent.final_dir().symlink_metadata(&target.filename) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            format!(
                                "exact feed batch absent path reappeared during final proof: {relative}"
                            ),
                        ));
                    }
                    Err(error) => return Err(error),
                },
            }
        }
        self.ensure_graph_text_admission_snapshot_binding_policy(index, require_ambient_binding)
    }

    fn prepare_graph_text_file_upsert_for_batch(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        relative: String,
        batch_scratch: u64,
        actual_charges: &mut GraphTextExactFeedBatchActualCharges,
        require_ambient_binding: bool,
        decode_semantics: bool,
    ) -> io::Result<PreparedGraphTextAdmissionUpsert> {
        self.prepare_graph_text_file_upsert_with_batch_charges(
            index,
            relative,
            batch_scratch,
            Some(actual_charges),
            require_ambient_binding,
            decode_semantics,
        )
    }

    fn graph_text_exact_feed_worst_permanent_growth(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        relative: &str,
        present_len: Option<u64>,
    ) -> io::Result<u64> {
        let eligible_path = self
            .graph_text_scope
            .is_eligible(relative)
            .then(|| GraphTextPath::parse(relative.to_owned()))
            .transpose()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        let mut path_growth =
            graph_text_admission_upsert_retained_upper_bound(relative, None, None)?;
        if eligible_path.is_some() {
            let title_format = graph_text_journal_title_format_budget(self)?;
            let accepted_semantic_name_bound = checked_add_bytes(present_len.unwrap_or(0), 64)?
                .max(checked_add_bytes(usize_to_u64(relative.len())?, 64)?)
                .max(title_format.rendered_bytes)
                .min(MAX_GRAPH_TEXT_SEMANTIC_NAME_BYTES);
            path_growth = checked_add_bytes(
                path_growth,
                graph_text_file_record_worst_case_upper_bound(
                    self,
                    usize_to_u64(relative.len())?,
                    accepted_semantic_name_bound,
                )?,
            )?;
        }
        if let Some(path) = eligible_path.as_ref() {
            path_growth = checked_add_bytes(
                path_growth,
                graph_text_admission_tombstone_upper_bound(
                    relative,
                    index.files_by_exact_path.get(path),
                )?,
            )?;
        }
        Ok(path_growth)
    }
    fn prepare_graph_text_file_upsert_with_batch_charges(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        relative: String,
        event_scratch: u64,
        mut actual_charges: Option<&mut GraphTextExactFeedBatchActualCharges>,
        require_ambient_binding: bool,
        decode_semantics: bool,
    ) -> io::Result<PreparedGraphTextAdmissionUpsert> {
        let target = self.graph_text_exact_path(&relative, false)?;
        let parent = self.graph_text_event_parent_policy(&target, require_ambient_binding)?;
        validate_graph_text_event_parent(index, &target, &parent)?;
        let enumerated = open_projection_file_nofollow(parent.final_dir(), &target.filename)?;
        let enumerated_resource = canonical_projection_file_resource_id(&enumerated)?;
        let link_count = projection_file_link_count(&enumerated)?;
        if link_count != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("exact feed upsert has an unsafe link count: {relative}"),
            ));
        }
        let enumerated_len = enumerated.metadata()?.len();
        if let Some(charges) = actual_charges.as_deref_mut() {
            let worst_growth = self.graph_text_exact_feed_worst_permanent_growth(
                index,
                &relative,
                Some(enumerated_len),
            )?;
            charges.ensure_permanent_growth(index, worst_growth)?;
            charges.reserve_raw(index, event_scratch, enumerated_len)?;
        }
        let live_bytes = match actual_charges.as_deref() {
            Some(charges) => charges.live_preparation_bytes(index, event_scratch)?,
            None => checked_add_bytes(index.permanent_bytes, event_scratch)?,
        };
        let remaining_peak = match actual_charges.as_deref() {
            Some(charges) => charges.remaining_peak(index, event_scratch)?,
            None => index
                .peak_limit
                .checked_sub(live_bytes)
                .ok_or_else(|| graph_text_capture_limit_error("peak build memory"))?,
        };
        let content_limit = match actual_charges.as_deref() {
            Some(_) => enumerated_len,
            None => MAX_PROJECTION_EVIDENCE_BYTES,
        };
        let (bytes, description, file_resource_id, _, _) =
            read_projection_optional_bound_capture_with_limits(
                parent.final_dir(),
                &target.filename,
                content_limit,
                remaining_peak,
            )?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("exact feed upsert disappeared: {}", relative),
                )
            })?;
        if actual_charges.is_some() && usize_to_u64(bytes.capacity())? != enumerated_len {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("exact feed upsert length changed after admission: {relative}"),
            ));
        }
        if file_resource_id != enumerated_resource {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("exact feed upsert changed after observation: {relative}"),
            ));
        }
        self.ensure_graph_text_admission_snapshot_binding_policy(index, require_ambient_binding)?;
        let eligible_path =
            if self.graph_text_scope.is_eligible(&relative) {
                Some(GraphTextPath::parse(relative.clone()).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
                })?)
            } else {
                None
            };
        let content_for_bound = if eligible_path.is_some() {
            std::str::from_utf8(&bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("graph text is not UTF-8: {relative}"),
                )
            })?
        } else {
            ""
        };
        let worst_growth = graph_text_admission_upsert_worst_case_upper_bound(
            self,
            &relative,
            eligible_path.as_ref(),
            content_for_bound,
        )?;
        if let Some(charges) = actual_charges.as_deref() {
            charges.ensure_permanent_growth(index, worst_growth)?;
        } else {
            let worst_permanent = index
                .permanent_bytes
                .checked_add(worst_growth)
                .ok_or_else(|| graph_text_capture_limit_error("permanent index memory"))?;
            if worst_permanent > index.permanent_limit {
                return Err(graph_text_capture_limit_error("permanent index memory"));
            }
        }
        let eligible = if let Some(path) = eligible_path {
            let (semantic, format) = if decode_semantics {
                let content = std::str::from_utf8(&bytes).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("graph text is not UTF-8: {path}"),
                    )
                })?;
                let permit = graph_text_parse_budget_permit(self, &path, content)?;
                let (semantic, format, node_count) =
                    self.decode_present_graph_text_with_node_count(&path, &bytes, permit)?;
                if node_count > MAX_GRAPH_TEXT_PARSER_NODES {
                    return Err(graph_text_capture_limit_error("parser node count"));
                }
                (semantic, format)
            } else {
                (
                    self.graph_text_entry_for_graph_text_path(&path)
                        .map_err(|error| {
                            io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                        })?,
                    Format::from_path(Path::new(path.as_str())),
                )
            };
            Some((
                path,
                GraphTextAdmissionRecord {
                    description,
                    file_resource_id,
                    link_count,
                    semantic,
                    format,
                    semantic_parsed: decode_semantics,
                },
            ))
        } else {
            None
        };
        let retained_growth = graph_text_admission_upsert_retained_upper_bound(
            &relative,
            eligible.as_ref().map(|(path, _)| path),
            eligible.as_ref().map(|(_, record)| &record.semantic),
        )?;
        if let Some(charges) = actual_charges.as_deref() {
            charges.ensure_permanent_growth(index, retained_growth)?;
        } else {
            let final_permanent = index
                .permanent_bytes
                .checked_add(retained_growth)
                .ok_or_else(|| graph_text_capture_limit_error("permanent index memory"))?;
            if final_permanent > index.permanent_limit {
                return Err(graph_text_capture_limit_error("permanent index memory"));
            }
        }
        let revalidation_live = checked_add_bytes(
            checked_add_bytes(live_bytes, usize_to_u64(bytes.capacity())?)?,
            retained_growth,
        )?;
        let revalidation_peak = index
            .peak_limit
            .checked_sub(revalidation_live)
            .ok_or_else(|| graph_text_capture_limit_error("peak build memory"))?;
        graph_text_event_revalidation_race_hook()?;
        let rebound_parent =
            self.graph_text_event_parent_policy(&target, require_ambient_binding)?;
        validate_graph_text_event_parent(index, &target, &rebound_parent)?;
        let rebound = open_projection_file_nofollow(rebound_parent.final_dir(), &target.filename)?;
        if canonical_projection_file_resource_id(&rebound)? != file_resource_id
            || projection_file_link_count(&rebound)? != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("exact feed upsert resource or link proof changed: {relative}"),
            ));
        }
        let (rebound_bytes, rebound_description, rebound_resource, _, _) =
            read_projection_optional_bound_capture_with_limits(
                rebound_parent.final_dir(),
                &target.filename,
                description.byte_length(),
                revalidation_peak,
            )?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("exact feed upsert disappeared during revalidation: {relative}"),
                )
            })?;
        let final_file =
            open_projection_file_nofollow(rebound_parent.final_dir(), &target.filename)?;
        if rebound_bytes != bytes
            || rebound_description != description
            || rebound_resource != file_resource_id
            || canonical_projection_file_resource_id(&final_file)? != file_resource_id
            || projection_file_link_count(&final_file)? != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("exact feed upsert changed during two-sided proof: {relative}"),
            ));
        }
        self.ensure_graph_text_admission_snapshot_binding_policy(index, require_ambient_binding)?;
        Ok(PreparedGraphTextAdmissionUpsert {
            relative,
            description,
            file_resource_id,
            link_count,
            retained_growth,
            eligible,
        })
    }

    fn apply_prepared_graph_text_file_upsert(
        &self,
        index: &mut CompleteGraphTextAdmissionIndex,
        prepared: PreparedGraphTextAdmissionUpsert,
    ) -> io::Result<()> {
        let prior_active = index
            .file_resource_by_exact_relative
            .contains_key(prepared.relative.as_str());
        let prior_graph_text = index
            .file_is_graph_text_by_exact_relative
            .get(prepared.relative.as_str())
            .copied()
            == Some(true);
        let prior_tombstone = GraphTextPath::parse(prepared.relative.clone())
            .ok()
            .is_some_and(|path| index.tombstones_by_exact_path.contains_key(&path));
        count_graph_text_admission_event_work(
            8,
            0,
            graph_text_delta_reverse_members(index, &prepared.relative),
        );
        let _ = remove_graph_text_admission_path(index, &prepared.relative);
        let is_graph_text = prepared.eligible.is_some();
        if let Ok(path) = GraphTextPath::parse(prepared.relative.clone()) {
            index.tombstones_by_exact_path.remove(&path);
        }
        index.permanent_bytes = checked_add_bytes(index.permanent_bytes, prepared.retained_growth)?;
        count_graph_text_admission_index_map_insertion();
        persistent_set_insert(
            &mut index.paths_by_file_resource,
            prepared.file_resource_id,
            prepared.relative.clone(),
        );
        count_graph_text_admission_index_map_insertion();
        index
            .file_resource_by_exact_relative
            .insert(prepared.relative.clone(), prepared.file_resource_id);
        count_graph_text_admission_index_map_insertion();
        index
            .file_link_count_by_exact_relative
            .insert(prepared.relative.clone(), prepared.link_count);
        index
            .file_is_graph_text_by_exact_relative
            .insert(prepared.relative.clone(), is_graph_text);
        if let Some((path, record)) = prepared.eligible {
            count_graph_text_admission_index_map_insertion();
            persistent_set_insert(
                &mut index.paths_by_portable_key,
                path.portable_key(),
                path.clone(),
            );
            count_graph_text_admission_index_map_insertion();
            persistent_set_insert(
                &mut index.paths_by_semantic_key,
                graph_text_semantic_key(&record.semantic),
                path.clone(),
            );
            count_graph_text_admission_index_map_insertion();
            index.files_by_exact_path.insert(path, record);
        }
        let writes = usize::from(prior_active) * 4
            + usize::from(prior_graph_text) * 3
            + usize::from(prior_tombstone)
            + 4
            + usize::from(is_graph_text) * 3;
        count_graph_text_admission_event_work(
            8,
            writes,
            graph_text_delta_reverse_members(index, &prepared.relative),
        );
        validate_graph_text_admission_delta(index, &prepared.relative)
    }

    fn prepare_graph_text_file_remove(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        relative: String,
        require_ambient_binding: bool,
    ) -> io::Result<PreparedGraphTextAdmissionRemove> {
        let target = self.graph_text_exact_path(&relative, false)?;
        match self.graph_text_event_parent_policy(&target, require_ambient_binding) {
            Ok(parent) => {
                validate_graph_text_event_parent(index, &target, &parent)?;
                match parent.final_dir().symlink_metadata(&target.filename) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            format!("exact feed removal is still present: {relative}"),
                        ));
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("exact feed removal parent is unavailable: {relative}"),
                ));
            }
            Err(error) => return Err(error),
        }
        self.ensure_graph_text_admission_snapshot_binding_policy(index, require_ambient_binding)?;
        graph_text_event_revalidation_race_hook()?;
        let rebound_parent = self
            .graph_text_event_parent_policy(&target, require_ambient_binding)
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("exact feed removal parent changed: {error}"),
                )
            })?;
        validate_graph_text_event_parent(index, &target, &rebound_parent)?;
        match rebound_parent
            .final_dir()
            .symlink_metadata(&target.filename)
        {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("exact feed removal reappeared: {relative}"),
                ));
            }
            Err(error) => return Err(error),
        }
        self.ensure_graph_text_admission_snapshot_binding_policy(index, require_ambient_binding)?;
        let retained_growth = match target.graph_text_path.as_ref() {
            Some(path)
                if index
                    .file_resource_by_exact_relative
                    .contains_key(relative.as_str()) =>
            {
                graph_text_admission_tombstone_upper_bound(
                    &relative,
                    index.files_by_exact_path.get(path),
                )?
            }
            None => 0,
            Some(_) => 0,
        };
        Ok(PreparedGraphTextAdmissionRemove {
            relative,
            retained_growth,
        })
    }

    fn apply_prepared_graph_text_file_remove(
        &self,
        index: &mut CompleteGraphTextAdmissionIndex,
        prepared: PreparedGraphTextAdmissionRemove,
    ) -> io::Result<()> {
        let relative = prepared.relative;
        let prior_graph_text = index
            .file_is_graph_text_by_exact_relative
            .get(relative.as_str())
            .copied()
            == Some(true);
        count_graph_text_admission_event_work(
            8,
            0,
            graph_text_delta_reverse_members(index, &relative),
        );
        if let Some(tombstone) = remove_graph_text_admission_path(index, &relative) {
            if let Ok(path) = GraphTextPath::parse(relative.to_owned()) {
                index.tombstones_by_exact_path.insert(path, tombstone);
            }
            index.permanent_bytes =
                checked_add_bytes(index.permanent_bytes, prepared.retained_growth)?;
        }
        count_graph_text_admission_event_work(8, 5 + usize::from(prior_graph_text) * 3, 0);
        validate_graph_text_admission_delta(index, &relative)
    }

    #[cfg(test)]
    fn graph_text_event_parent(&self, target: &GraphTextExactPath) -> io::Result<ProjectionParent> {
        self.graph_text_event_parent_policy(target, true)
    }

    fn graph_text_event_parent_policy(
        &self,
        target: &GraphTextExactPath,
        require_ambient_binding: bool,
    ) -> io::Result<ProjectionParent> {
        let root = if require_ambient_binding {
            self.projection_root.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "graph has no retained no-follow projection capability",
                )
            })?
        } else {
            &self.graph_text_write_binding()?.root
        };
        let mut chain = vec![root.try_clone()?];
        for component in &target.parent_components {
            let current = chain.last().expect("graph-text parent contains root");
            projection_real_directory(current, component)?;
            chain.push(open_projection_dir_nofollow(current, component)?);
        }
        Ok(ProjectionParent { chain })
    }

    fn ensure_graph_text_admission_snapshot_binding_policy(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        require_ambient_binding: bool,
    ) -> io::Result<()> {
        if require_ambient_binding {
            self.ensure_projection_root_binding()?;
        }
        let (graph_resource, scope_binding) = if require_ambient_binding {
            (
                self.canonical_resource_id()?,
                self.graph_text_scope_binding()?,
            )
        } else {
            let binding = self.graph_text_write_binding()?;
            (
                binding.resource_id,
                self.graph_text_scope
                    .bind_graph_resource(binding.resource_id),
            )
        };
        if !Arc::ptr_eq(&index.instance, &self.graph_text_admission_instance)
            || graph_resource != index.graph_resource
            || scope_binding != index.scope_binding
            || scope_binding.graph_resource_id() != graph_resource
        {
            return Err(graph_text_admission_unavailable(
                "graph-text event snapshot binding changed",
            ));
        }
        Ok(())
    }

    fn ensure_projection_root_binding(&self) -> io::Result<()> {
        let retained = self.projection_root.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "graph has no retained no-follow projection capability",
            )
        })?;
        let rebound = open_projection_root_nofollow(&self.root)?;
        if projection_dir_identity(retained)? != projection_dir_identity(&rebound)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "graph root changed during graph inventory capture",
            ));
        }
        Ok(())
    }

    /// Construct a read-only graph projection from one caller-owned document
    /// snapshot. The empty `root` is only a fail-closed fallback: whole-graph
    /// consumers use the preinstalled cache and page list, so they can never
    /// mix these documents with a later revision from the live graph.
    ///
    pub(crate) fn from_page_snapshot(
        root: impl AsRef<Path>,
        mut pages: Vec<(PageEntry, Arc<Document>)>,
    ) -> Graph {
        for (entry, document) in &mut pages {
            assign_doc_runtime_ids(&mut Arc::make_mut(document).roots, &entry.rel_path);
        }
        let graph = Graph::open(root);
        let entries = pages.iter().map(|(entry, _)| entry.clone()).collect();
        let index = build_page_cache_index(&pages);
        *graph.cache.write().unwrap() = Some(Arc::new(pages));
        *graph.cache_index.write().unwrap() = Some(index);
        *graph.page_list_cache.write().unwrap() = Some((0, entries));
        graph
    }

    /// The write lock for a resolved page path (see `page_locks`). Returns an
    /// `Arc` the caller holds (`let _g = lock.lock().unwrap();`) for the critical
    /// section. The `page_locks` map mutex is released before the per-page lock is
    /// taken, so callers never serialize on the map. Opportunistically prunes
    /// entries no caller still holds (strong_count == 1) to bound growth.
    fn page_lock(&self, path: &Path) -> std::sync::Arc<std::sync::Mutex<()>> {
        let mut map = self.page_locks.lock().unwrap();
        if map.len() >= 64 {
            map.retain(|_, v| std::sync::Arc::strong_count(v) > 1);
        }
        map.entry(path.to_path_buf())
            .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
            .clone()
    }

    /// The `logseq/config.edn` bytes this instance was opened with, digested.
    /// `None` when there was no readable file.
    ///
    /// Compare against [`config_file_description`] to learn whether an external
    /// write actually changed the configuration this instance is serving. The
    /// watcher does exactly that before paying for a whole-graph reopen, which
    /// drops every cache the graph has built.
    pub fn open_config_description(&self) -> Option<BlobDescription> {
        self.reconciliation_scan_open_config_description
    }

    /// Digest of the configuration bytes this instance last wrote, if any.
    ///
    /// `None` on an instance that has published nothing — including every
    /// short-lived capability, whose refresh is cheap enough not to
    /// need the distinction.
    pub fn recent_config_write(&self) -> Option<BlobDescription> {
        *self.recent_config_write.read().unwrap()
    }

    /// Record what a configuration write just published. Called by the one
    /// funnel every setter goes through (`Graph::write_config`).
    pub(crate) fn note_config_write(&self) {
        *self.recent_config_write.write().unwrap() = config_file_description(&self.root);
    }

    pub fn meta(&self) -> GraphMeta {
        GraphMeta {
            root: self.root.display().to_string(),
            journals_dir: self.config.journals_dir.clone(),
            pages_dir: self.config.pages_dir.clone(),
            preferred_workflow: match self.config.preferred_workflow {
                crate::config::Workflow::Todo => "todo".into(),
                crate::config::Workflow::Now => "now".into(),
            },
            shortcuts: self.config.shortcuts.clone(),
            start_of_week: self.config.start_of_week,
            linked_references_collapsed_threshold: self
                .config
                .linked_references_collapsed_threshold,
            block_hidden_properties: self.config.block_hidden_properties.clone(),
            default_journal_template: self.config.default_journal_template.clone(),
            default_home: self.config.default_home.clone(),
            favorites: self.config.favorites.clone(),
            favorites_page: self.config.favorites_page.clone(),
            journal_page_title_format: self.journal_format.title_format().to_string(),
            journal_file_name_format: self.journal_format.file_format().to_string(),
            preferred_format: self.config.preferred_format.ext().to_string(),
            macros: self.config.macros.clone(),
            enable_timetracking: self.config.enable_timetracking,
            show_brackets: self.config.show_brackets,
            doc_mode_enter_for_new_block: self.config.doc_mode_enter_for_new_block,
            logical_outdenting: self.config.logical_outdenting,
            logbook_with_second_support: self.config.logbook.with_second_support,
            logbook_enabled_in_timestamped_blocks: self
                .config
                .logbook
                .enabled_in_timestamped_blocks,
            logbook_enabled_in_all_blocks: self.config.logbook.enabled_in_all_blocks,
            guide_announced: self.config.guide_announced,
        }
    }

    /// Current cache generation — bumped on every cache-mutating page change,
    /// and the key that memoized backlink/reference results invalidate against.
    /// Exposed for observability and tests (e.g. asserting a no-op save doesn't
    /// needlessly invalidate everything).
    pub fn cache_generation(&self) -> u64 {
        self.cache_gen.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Pages skipped by the latest whole-graph search-cache build because their
    /// parse/projection panicked. Paths are graph-relative and safe to surface.
    pub fn page_index_failures(&self) -> Vec<String> {
        self.page_index_failures.read().unwrap().clone()
    }

    pub fn journals_path(&self) -> PathBuf {
        self.root.join(&self.config.journals_dir)
    }

    pub fn pages_path(&self) -> PathBuf {
        self.root.join(&self.config.pages_dir)
    }

    /// Graph-root-relative, forward-slashed path for an absolute file path inside
    /// the graph (`…/journals/2026_06_26.org` → `journals/2026_06_26.org`). The
    /// stable, machine-portable id Tine hands the frontend so a page can be pinned
    /// to a SPECIFIC file (#21). Falls back to the input lossily if it's somehow
    /// outside the root (shouldn't happen for graph files).
    pub fn rel_path(&self, abs: &Path) -> String {
        slash_path(abs.strip_prefix(&self.root).unwrap_or(abs))
    }

    /// Resolve a graph-root-relative path (as produced by [`rel_path`]) back to an
    /// absolute file path, validating it belongs to the versioned graph-wide text
    /// scope. Retained no-follow traversal and identity validation are performed
    /// before reads or writes; this lexical gate grants no creation or projection
    /// authority.
    pub fn resolve_rel(&self, rel: &str) -> Option<PathBuf> {
        let abs = self.resolve_rel_lexical(rel)?;
        if !path_stays_within_root(&self.root, &abs) || path_uses_graph_text_alias(&self.root, &abs)
        {
            return None;
        }
        Some(abs)
    }

    fn resolve_graph_text_rel(
        &self,
        permit: &GraphTextWritePermit,
        rel: &str,
    ) -> io::Result<Option<PathBuf>> {
        self.graph_text_permit_root(permit)?;
        Ok(self.resolve_configured_rel_lexical(rel))
    }

    fn resolve_graph_rel_with_permit(
        &self,
        permit: &GraphTextWritePermit,
        rel: &str,
    ) -> io::Result<Option<PathBuf>> {
        self.graph_text_permit_root(permit)?;
        Ok(self.resolve_rel_lexical(rel))
    }

    fn resolve_rel_lexical(&self, rel: &str) -> Option<PathBuf> {
        let rel = rel.trim();
        if !self.graph_text_scope.is_eligible(rel) {
            return None;
        }
        Some(self.root.join(rel))
    }

    fn resolve_configured_rel_lexical(&self, rel: &str) -> Option<PathBuf> {
        let rel = rel.trim();
        if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') {
            return None;
        }
        let parts = rel.split('/').collect::<Vec<_>>();
        let configured_root = [&self.config.journals_dir, &self.config.pages_dir]
            .into_iter()
            .filter_map(|configured| {
                let components = configured.split('/').collect::<Vec<_>>();
                (!components.is_empty()
                    && components
                        .iter()
                        .all(|component| projection_component_is_portable(component))
                    && parts.len() > components.len()
                    && parts.starts_with(&components))
                .then_some((configured, components.len()))
            })
            .max_by_key(|(_, len)| *len)?;
        let base = self.root.join(configured_root.0);
        // The remaining segments are the file's path UNDER that dir. Nested
        // sub-directories are allowed (#21) but the can't-escape-the-graph
        // invariant is kept lexically: every segment must be a plain name — no
        // empty segment (`a//b`, a trailing `/`), no `.`/`..` traversal. With no
        // `..` and no absolute/backslash (rejected above), `base.join(tail)`
        // provably stays within `base`; there must be at least one segment (a bare
        // `pages` is a dir, not a file).
        let mut tail = PathBuf::new();
        for &seg in &parts[configured_root.1..] {
            if seg.is_empty() || seg == "." || seg == ".." {
                return None;
            }
            tail.push(seg);
        }
        if tail.as_os_str().is_empty() {
            return None;
        }
        let abs = base.join(tail);
        text_extension_from_path(&abs).map(|_| ())?;
        Some(abs)
    }

    fn projection_page_target(&self, relative_path: &str) -> io::Result<ProjectionTarget> {
        if relative_path != relative_path.trim()
            || relative_path.is_empty()
            || relative_path.starts_with('/')
            || relative_path.contains('\\')
            || relative_path.contains('\0')
        {
            return Err(bad_path());
        }
        let components = relative_path.split('/').collect::<Vec<_>>();
        let configured_root_len = [&self.config.journals_dir, &self.config.pages_dir]
            .into_iter()
            .filter_map(|configured_root| {
                let root_components = configured_root.split('/').collect::<Vec<_>>();
                (components.len() > root_components.len()
                    && root_components
                        .iter()
                        .all(|component| projection_component_is_portable(component))
                    && components.starts_with(&root_components))
                .then_some(root_components.len())
            })
            .max();
        if components
            .iter()
            .any(|component| !projection_component_is_portable(component))
        {
            return Err(bad_path());
        }
        // A configured root keeps its existing acceptance verbatim. Ordinary
        // graph text that no configured root owns is addressable too, because
        // OG reads and rewrites a page wherever it already lives: its recursive
        // `logseq.common.graph/get-files` walk has no root restriction, and
        // `frontend.modules.file.core/save-tree-aux!` writes back to the exact
        // recorded `:file/path` (only a page with no file at all gets a fresh
        // path under a configured directory). The graph-text scope supplies the
        // containers OG itself skips, so nothing here may address `assets/`,
        // `logseq/bak/`, hidden directories or other excluded state.
        let within_configured_root = components.len() >= 2 && configured_root_len.is_some();
        if !within_configured_root && !self.graph_text_scope.is_eligible(relative_path) {
            return Err(bad_path());
        }
        let filename = components
            .last()
            .expect("nonempty split has a last element");
        let _ = split_logseq_text_filename(filename).ok_or_else(bad_path)?;
        let parent_components = components[..components.len() - 1]
            .iter()
            .map(|component| (*component).to_owned())
            .collect::<Vec<_>>();
        let target = ProjectionTarget {
            absolute_path: self.root.join(relative_path),
            parent_components,
            filename: (*filename).to_owned(),
        };
        Ok(target)
    }

    fn graph_text_exact_path(
        &self,
        relative: &str,
        require_eligible: bool,
    ) -> io::Result<GraphTextExactPath> {
        if relative != relative.trim()
            || relative.is_empty()
            || relative.starts_with('/')
            || relative.contains('\\')
            || relative.contains('\0')
        {
            return Err(bad_path());
        }
        let components = relative.split('/').collect::<Vec<_>>();
        if components
            .iter()
            .any(|component| !projection_component_is_portable(component))
        {
            return Err(bad_path());
        }
        let graph_text_path = GraphTextPath::parse(relative.to_owned()).ok();
        if require_eligible
            && (!self.graph_text_scope.is_eligible(relative) || graph_text_path.is_none())
        {
            return Err(bad_path());
        }
        let filename = components.last().copied().ok_or_else(bad_path)?;
        let mut parent_components = Vec::with_capacity(components.len().saturating_sub(1));
        let mut parent_relative = String::new();
        for component in &components[..components.len() - 1] {
            if !parent_relative.is_empty() {
                parent_relative.push('/');
            }
            parent_relative.push_str(component);
            if !self.graph_text_scope.should_descend(&parent_relative) {
                return Err(bad_path());
            }
            parent_components.push((*component).to_owned());
        }
        Ok(GraphTextExactPath {
            graph_text_path,
            parent_components,
            filename: filename.to_owned(),
        })
    }

    fn projection_parent(&self, target: &ProjectionTarget) -> io::Result<ProjectionParent> {
        let root = self.projection_root.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "graph has no retained no-follow projection capability",
            )
        })?;
        let mut chain = vec![root.try_clone()?];
        for component in &target.parent_components {
            let current = chain.last().expect("projection chain contains root");
            projection_real_directory(current, component)?;
            chain.push(open_projection_dir_nofollow(current, component)?);
        }
        Ok(ProjectionParent { chain })
    }

    fn projection_parent_optional(
        &self,
        target: &ProjectionTarget,
    ) -> io::Result<Option<ProjectionParent>> {
        match self.projection_parent_capture(target)? {
            ProjectionParentCapture::Present(parent) => Ok(Some(parent)),
            ProjectionParentCapture::Missing => {
                self.ensure_projection_root_binding()?;
                match self.projection_parent_capture(target)? {
                    ProjectionParentCapture::Missing => {
                        self.ensure_projection_root_binding()?;
                        Ok(None)
                    }
                    ProjectionParentCapture::Present(_) => Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "projection parent appeared during absence capture",
                    )),
                }
            }
        }
    }

    fn projection_parent_capture(
        &self,
        target: &ProjectionTarget,
    ) -> io::Result<ProjectionParentCapture> {
        let root = self.projection_root.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "graph has no retained no-follow projection capability",
            )
        })?;
        let mut chain = vec![root.try_clone()?];
        for component in &target.parent_components {
            let current = chain.last().expect("projection chain contains root");
            match projection_real_directory(current, component) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(ProjectionParentCapture::Missing);
                }
                Err(error) => return Err(error),
            }
            // A component that disappears or changes after successful shape
            // validation is a traversal race, not semantic absence.
            chain.push(open_projection_dir_nofollow(current, component)?);
        }
        Ok(ProjectionParentCapture::Present(ProjectionParent { chain }))
    }

    fn ensure_projection_target_shape(
        &self,
        parent: &ProjectionParent,
        target: &ProjectionTarget,
    ) -> io::Result<()> {
        projection_optional_regular_metadata(parent.final_dir(), &target.filename)?;
        let (target_stem, _) = split_logseq_text_filename(&target.filename).ok_or_else(bad_path)?;
        for extension in LOGSEQ_TEXT_EXTENSIONS {
            let sibling = format!("{target_stem}.{extension}");
            if sibling == target.filename {
                continue;
            }
            // An authenticated exact projection may coexist with independent
            // regular text siblings. Validate their shape without granting
            // them authority over the target path.
            projection_optional_regular_metadata(parent.final_dir(), &sibling)?;
        }
        Ok(())
    }

    fn ensure_projection_parent_binding(
        &self,
        parent: &ProjectionParent,
        target: &ProjectionTarget,
    ) -> io::Result<()> {
        let rebound = self.projection_parent(target)?;
        if projection_dir_identity(rebound.final_dir())?
            != projection_dir_identity(parent.final_dir())?
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "projection parent changed during publication",
            ));
        }
        Ok(())
    }

    /// Resolve the exact on-disk source file for an explicit user file action.
    /// A loaded page's recorded relative path always wins (including nested and
    /// duplicate-name files); a newly saved page without a refreshed path may
    /// fall back to normal name resolution. The final canonical-file check keeps
    /// symlinks from escaping the configured pages/journals directories.
    pub fn page_source_file(
        &self,
        name: &str,
        kind: PageKind,
        recorded_path: Option<&str>,
    ) -> io::Result<PathBuf> {
        let candidate = recorded_path
            .filter(|path| !path.trim().is_empty())
            .map(|path| {
                self.resolve_rel(path)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid page path"))
            })
            .unwrap_or_else(|| Ok(self.path_for(name, kind)))?;
        let canonical = candidate.canonicalize()?;
        if !canonical.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "page source is not a file",
            ));
        }
        let root = self.root.canonicalize()?;
        if !canonical.starts_with(&root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "page source escapes graph text scope",
            ));
        }
        Ok(canonical)
    }

    /// Whether a journal file is a "shadow": a non-date-stem file (e.g. a leftover
    /// title-named `Friday, 26-06-2026.org`) that coexists with a canonical
    /// date-stem file (`2026_06_26.{md,org}`) for the SAME day. The `(kind,name)`
    /// cache slot belongs to the canonical file, so a shadow must never be folded
    /// into it (that would make name-resolution serve the shadow's content). A
    /// shadow is loaded fresh by path on demand instead (#21). Twins (two date-stem
    /// files of the same day in different extensions) are deliberately NOT shadows —
    /// that case keeps its existing `has_twin`/dedup handling.
    fn is_shadow_journal(&self, path: &Path, date: crate::date::JournalDate) -> bool {
        let is_date_stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| crate::date::JournalDate::from_file_stem(s).is_some());
        if is_date_stem {
            return false;
        }
        let canonical = self.journal_format.file_stem(date);
        // Every Logseq text extension, not a hand-written md/org pair. `.markdown`
        // is a first-class page extension (LOGSEQ_TEXT_EXTENSIONS, and OG accepts
        // it case-insensitively). Asking only two meant a title-named
        // leftover coexisting with a canonical `2026_06_26.markdown` was NOT
        // recognised as a shadow, so it was reconciled into the (kind,name) cache
        // and name resolution served the WRONG file for that day — exactly the #21
        // defect this function exists to prevent, reachable only in a `.markdown`
        // graph. (Direct Files data-safety audit, 2026-08-09, finding 15.)
        configured_text_variant_paths(&self.journals_path(), &canonical)
            .iter()
            .any(|candidate| candidate.is_file())
    }

    /// The format (`Md`/`Org`) new pages and journals are created in, from
    /// `config.edn`'s `:preferred-format`. Existing files keep their own format.
    pub fn preferred_format(&self) -> Format {
        self.config.preferred_format
    }
}

/// Canonical Markdown page-header property grammar mirrored from
/// `src/editor/properties.ts`. It is deliberately narrower than OG's historical
/// "first line contains `:: `" serializer heuristic, so ordinary prose/fences
/// can never be promoted accidentally.
fn page_header_property_line(line: &str) -> Option<(&str, &str)> {
    static KEY: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let (key, value) = line.split_once("::")?;
    if key.is_empty() || key.starts_with('#') {
        return None;
    }
    let valid = KEY
        .get_or_init(|| regex::Regex::new(r"^[\p{L}\p{M}\p{N}_./-]+$").unwrap())
        .is_match(key);
    valid.then_some((key, value))
}

fn page_header_properties_only(raw: &str) -> bool {
    if raw.is_empty() || raw.starts_with('\n') || raw.ends_with('\n') {
        return false;
    }
    let mut saw_property = false;
    for line in raw.split('\n') {
        if line.is_empty() {
            if !saw_property {
                return false;
            }
            continue;
        }
        if page_header_property_line(line).is_none() {
            return false;
        }
        saw_property = true;
    }
    saw_property
}

fn first_root_is_promotable_page_header(doc: &Document) -> bool {
    let Some(first) = doc.roots.first() else {
        return false;
    };
    first.children.is_empty()
        && page_header_properties_only(&first.raw)
        && !first.raw.split('\n').any(|line| {
            page_header_property_line(line).is_some_and(|(key, _)| key.eq_ignore_ascii_case("id"))
        })
}

fn promote_first_root_page_header(doc: &mut Document) {
    if !first_root_is_promotable_page_header(doc) {
        return;
    }
    let first = doc.roots.remove(0);
    doc.pre_block = Some(first.raw);
}

/// Return the first property-shaped outline line that has no outline provenance
/// on disk while the proposal also loses page-header property slots.
///
/// The firewall is deliberately structural rather than an exact-string test:
/// a broken DTO must not evade it by editing the moved line's key/value. At the
/// same time, a property-shaped outline block that genuinely existed on disk is
/// allowed to stay, move, or be edited. We therefore treat existing outline
/// property lines as provenance slots: exact multiset matches consume their
/// original slots first, and remaining slots cover ordinary edits. Only an
/// excess proposed outline line is newly unproven. There is no implicit repair;
/// contradictory structure is rejected before bytes or cache can change.
fn newly_reclassified_page_property_line(existing: &str, proposed: &Document) -> Option<String> {
    // The general data-preservation guard is intentionally a little broader
    // than Tine's editable property grammar: Logseq graphs can contain Unicode
    // or plugin-defined keys that Tine does not expose in its settings panel,
    // but they still must never be reclassified into outline content.
    fn page_header_property(line: &str) -> bool {
        let Some((key, _)) = line.split_once("::") else {
            return false;
        };
        let key = key.trim();
        !key.is_empty() && key.chars().all(|ch| !ch.is_whitespace() && ch != ':')
    }

    fn pre_property_lines(raw: Option<&str>) -> Vec<&str> {
        raw.unwrap_or("")
            .split('\n')
            .filter(|line| page_header_property(line))
            .collect()
    }

    fn outline_property_lines<'a>(blocks: &'a [DocBlock], out: &mut Vec<&'a str>) {
        let mut frames: [Option<std::slice::Iter<'a, DocBlock>>; MAX_BLOCK_DEPTH] =
            std::array::from_fn(|_| None);
        let mut len = usize::from(!blocks.is_empty());
        if len != 0 {
            frames[0] = Some(blocks.iter());
        }
        while len != 0 {
            let mut frame = frames[len - 1]
                .take()
                .expect("active property firewall frame");
            let Some(block) = frame.next() else {
                len -= 1;
                continue;
            };
            frames[len - 1] = Some(frame);
            out.extend(
                block
                    .raw
                    .split('\n')
                    .filter(|line| page_header_property(line)),
            );
            if !block.children.is_empty() {
                if len == MAX_BLOCK_DEPTH {
                    debug_assert!(false, "document nesting exceeded graph depth");
                    continue;
                }
                frames[len] = Some(block.children.iter());
                len += 1;
            }
        }
    }

    let existing_doc = doc::parse(existing);
    let existing_pre = pre_property_lines(existing_doc.pre_block.as_deref());
    let proposed_pre = pre_property_lines(proposed.pre_block.as_deref());
    if proposed_pre.len() >= existing_pre.len() {
        return None;
    }

    let mut existing_outline = Vec::new();
    outline_property_lines(&existing_doc.roots, &mut existing_outline);
    let mut proposed_outline = Vec::new();
    outline_property_lines(&proposed.roots, &mut proposed_outline);
    if proposed_outline.len() <= existing_outline.len() {
        return None;
    }

    // Cancel exact matches as a multiset so the diagnostic identifies a truly
    // excess proposal line even in the presence of duplicates. Any remaining
    // existing slots then cover changed/reordered pre-existing outline lines.
    let mut exact_slots: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for line in &existing_outline {
        *exact_slots.entry(*line).or_default() += 1;
    }
    let mut unmatched = Vec::new();
    let mut exact_matches = 0usize;
    for line in proposed_outline {
        match exact_slots.get_mut(line) {
            Some(count) if *count > 0 => {
                *count -= 1;
                exact_matches += 1;
            }
            _ => unmatched.push(line),
        }
    }
    let edited_provenance_slots = existing_outline.len() - exact_matches;
    unmatched
        .get(edited_provenance_slots)
        .map(|line| (*line).to_string())
}

/// Atomically reserve a unique filename in `assets/` for `name`, de-duplicating
/// against existing files by appending `_1`, `_2`, … to the stem. Unlike a plain
/// `exists()` check followed by a write, this CREATES the file exclusively
/// (`create_new`), so a concurrent writer (OG Logseq, or another asset op) that
/// races between the name check and our write can't claim the same name and get
/// silently overwritten — whoever loses the create retries the next candidate.
/// Returns the chosen name and the open (empty) file handle.
/// Reject an asset name that isn't a plain top-level filename — a path separator
/// or a `.`/`..` component — so a frontend-supplied name can't reach outside
/// `assets/` (defense-in-depth; mirrors `trash_asset`). `create_new` already
/// blocks overwriting an existing file, so the realistic pre-guard outcome was a
/// stray file, not corruption — but reject it outright anyway.
fn top_level_asset_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bad asset name",
        ));
    }
    Ok(())
}

/// Accept a portable assets-relative path for reads. Mutation entry points keep
/// using `top_level_asset_name`: supporting existing nested Logseq assets does
/// not grant frontend callers a nested write capability.
fn relative_asset_path(name: &str) -> io::Result<PathBuf> {
    if name.contains('\\')
        || name
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bad asset path",
        ));
    }

    let path = PathBuf::from(name);
    if path
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bad asset path",
        ));
    }
    Ok(path)
}

/// Preserve a file's CRLF line endings on re-write: if `existing` used Windows
/// endings and the freshly-serialized `content` is all-LF, convert it back, so a
/// real edit produces a minimal diff instead of flipping every line (Syncthing
/// churn vs a Windows editor). New files stay LF. Shared by write_page +
/// write_highlights so the two can't drift on it.
fn serialize_pdf_hls_page(
    path: &Path,
    document: &Document,
    existing: Option<&str>,
) -> io::Result<String> {
    // Concord invariant 4 (write-shyness): an `hls__` page is an ordinary Logseq
    // page the user and OG also write. This used to serialize with DEFAULT opts
    // — one trailing newline, tab indent, one blank line after the preamble —
    // so a highlight save re-indented and re-terminated the whole file even
    // where nothing changed. Reproduce the file's own formatting exactly as the
    // editor save path (`serialize_page_document`) does, including the
    // layout-identity retention that keeps untouched blocks byte-stable.
    let identities = doc::layout_identities_of(document);
    match Format::from_path(path) {
        Format::Md => {
            let opts = doc::SerializeOpts::detect_with_layout_identities(existing, &identities);
            Ok(preserve_crlf(
                doc::serialize_with(document, &opts),
                existing,
            ))
        }
        Format::Org => {
            if existing.is_some_and(|raw| !crate::org::org_editable(raw)) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "org highlight page is read-only (does not round-trip)",
                ));
            }
            Ok(crate::org::serialize_org_detect_with_layout_identities(
                document,
                existing,
                &identities,
            ))
        }
    }
}

fn preserve_crlf(content: String, existing: Option<&str>) -> String {
    if existing.is_some_and(|e| e.contains("\r\n")) && !content.contains('\r') {
        content.replace('\n', "\r\n")
    } else {
        content
    }
}

/// Read an optional UTF-8 text file without conflating "missing" with "could not
/// safely read". Mutation paths use this for their baselines: only NotFound may
/// become `None`; every other error must stop the write.
fn read_optional_text(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn validate_highlight_edn(raw: &str) -> io::Result<()> {
    if raw.trim().is_empty() {
        return Ok(());
    }
    if matches!(crate::edn::parse_strict(raw), Some(crate::edn::Edn::Map(_))) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "highlight sidecar is malformed; refusing to replace it",
        ))
    }
}

/// Runtime identities for one exact page revision, independent of its parsed
/// document and SQLite projection. Child counts describe the complete preorder
/// shape so restoration never applies only a prefix of an incompatible tree.
struct SessionPageIds {
    revision: String,
    config: ContentDigest,
    preorder: Vec<(String, usize)>,
}

impl SessionPageIds {
    fn capture(revision: &str, config: ContentDigest, doc: &Document) -> Self {
        let mut pending: Vec<_> = doc.roots.iter().rev().collect();
        let mut preorder = Vec::new();
        while let Some(block) = pending.pop() {
            preorder.push((block.uuid.clone(), block.children.len()));
            pending.extend(block.children.iter().rev());
        }
        Self {
            revision: revision.to_owned(),
            config,
            preorder,
        }
    }

    fn restore(&self, doc: &mut Document) -> bool {
        // Check the entire tree before changing any ID. Exact source bytes and
        // config should imply this shape; a parser discrepancy must not apply a
        // prefix of one tree's identities to another tree.
        let mut pending: Vec<_> = doc.roots.iter().rev().collect();
        let mut count = 0;
        while let Some(block) = pending.pop() {
            if self
                .preorder
                .get(count)
                .is_none_or(|(_, children)| *children != block.children.len())
            {
                return false;
            }
            count += 1;
            pending.extend(block.children.iter().rev());
        }
        if count != self.preorder.len() {
            return false;
        }
        let mut pending: Vec<_> = doc.roots.iter_mut().rev().collect();
        let mut ids = self.preorder.iter();
        while let Some(block) = pending.pop() {
            block
                .uuid
                .clone_from(&ids.next().expect("complete shape checked").0);
            pending.extend(block.children.iter_mut().rev());
        }
        true
    }
}

impl Graph {
    fn restore_session_page_ids(
        &self,
        entry: &PageEntry,
        revision: &str,
        doc: &mut Document,
    ) -> bool {
        let config = self.config.parse_config().digest();
        self.session_page_ids
            .read()
            .unwrap()
            .get(&entry.path)
            .is_some_and(|ids| ids.revision == revision && ids.config == config && ids.restore(doc))
    }

    fn parse_session_page_content(&self, entry: &PageEntry, content: &str) -> (Document, String) {
        let (mut document, revision) = parse_page_content(entry, content);
        self.restore_session_page_ids(entry, &revision, &mut document);
        (document, revision)
    }
}

fn parse_page_content(e: &PageEntry, content: &str) -> (Document, String) {
    #[cfg(test)]
    GRAPH_TEXT_PARSE_ATTEMPTS.with(|attempts| attempts.set(attempts.get().saturating_add(1)));
    let rev = content_rev(&content);
    let mut d = parse_doc(&e.path, &content);
    #[cfg(test)]
    if content.contains(TEST_PAGE_PARSE_PANIC_SENTINEL) {
        panic!("deterministic test sentinel for a page projection panic");
    }
    assign_doc_runtime_ids(&mut d.roots, &e.rel_path);
    (d, rev)
}

fn parsed_page_title(document: &Document, format: Format) -> Option<String> {
    let preamble = document.pre_block.as_deref()?;
    for line in preamble.lines() {
        if let Some((key, value)) = doc::parse_property_line(line) {
            if key.eq_ignore_ascii_case("title") && !value.trim().is_empty() {
                return Some(value.trim().to_owned());
            }
        }
        if format == Format::Org {
            let trimmed = line.trim();
            let directive = trimmed
                .split_once(':')
                .and_then(|(key, value)| key.eq_ignore_ascii_case("#+title").then_some(value));
            let drawer = trimmed.strip_prefix(':').and_then(|rest| {
                rest.split_once(':')
                    .and_then(|(key, value)| key.eq_ignore_ascii_case("title").then_some(value))
            });
            if let Some(value) = directive.or(drawer) {
                if !value.trim().is_empty() {
                    return Some(value.trim().to_owned());
                }
            }
        }
    }
    None
}

fn replace_page_title_property(raw: &str, name: &str) -> Option<String> {
    let mut offset = 0;
    for chunk in raw.split_inclusive('\n') {
        let line = chunk.strip_suffix('\n').unwrap_or(chunk);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim_start();
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            break;
        }
        if doc::parse_property_line(line).is_some_and(|(key, _)| key.eq_ignore_ascii_case("title"))
        {
            let newline = if chunk.ends_with("\r\n") {
                "\r\n"
            } else if chunk.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            let mut output = String::with_capacity(raw.len() + name.len());
            output.push_str(&raw[..offset]);
            output.push_str("title:: ");
            output.push_str(name);
            output.push_str(newline);
            output.push_str(&raw[offset + chunk.len()..]);
            return Some(output);
        }
        offset += chunk.len();
    }
    None
}

fn bind_markdown_title_property(content: &str, name: &str) -> String {
    replace_page_title_property(content, name)
        .unwrap_or_else(|| format!("title:: {name}\n\n{content}"))
}

fn bind_document_title_property(document: &mut Document, name: &str) {
    let current = document.pre_block.take().unwrap_or_default();
    document.pre_block = Some(
        replace_page_title_property(&current, name).unwrap_or_else(|| {
            if current.is_empty() {
                format!("title:: {name}")
            } else {
                format!("title:: {name}\n{current}")
            }
        }),
    );
}

fn effective_page_entry(
    journal_format: &JournalFormat,
    entry: &PageEntry,
    document: &Document,
) -> PageEntry {
    let mut effective = entry.clone();
    if let Some(title) = parsed_page_title(document, Format::from_path(&entry.path)) {
        effective.name = title;
    }
    match journal_format.parse(&effective.name) {
        Some(date) => {
            effective.name = journal_format.title(date);
            effective.kind = PageKind::Journal;
            effective.date_key = Some(date.ordinal_key());
        }
        None => {
            effective.kind = PageKind::Page;
            effective.date_key = None;
        }
    }
    effective
}

fn parse_external_document(
    graph: &Graph,
    fallback: PageEntry,
    content: &str,
) -> io::Result<ParsedExternalDocument> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        #[cfg(test)]
        GRAPH_TEXT_PARSE_ATTEMPTS.with(|attempts| attempts.set(attempts.get().saturating_add(1)));
        let format = Format::from_path(&fallback.path);
        let mut parsed = match format {
            Format::Md => doc::parse_with_source_spans(content),
            Format::Org => crate::org::parse_org_with_source_spans(content),
        };
        #[cfg(test)]
        if content.contains(TEST_PAGE_PARSE_PANIC_SENTINEL) {
            panic!("deterministic test sentinel for a page projection panic");
        }
        assign_doc_runtime_ids(&mut parsed.document.roots, &fallback.rel_path);
        graph.restore_session_page_ids(&fallback, &content_rev(content), &mut parsed.document);
        let effective = effective_page_entry(&graph.journal_format, &fallback, &parsed.document);
        ParsedExternalDocument {
            format,
            effective,
            parsed,
            revision: content_rev(content),
        }
    })) {
        Ok(parsed) => Ok(parsed),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "external document parser rejected present graph text",
        )),
    }
}

pub(crate) fn parse_exact_page(
    graph: &Graph,
    entry: &PageEntry,
    content: &str,
) -> io::Result<(PageEntry, Document, String)> {
    let parsed = parse_external_document(graph, entry.clone(), content)?;
    Ok((parsed.effective, parsed.parsed.document, parsed.revision))
}

/// Count a parsed document without recursive descent, so an externally edited
/// deeply nested source cannot consume the process stack during inactive source
/// capture.  Returning `limit + 1` is sufficient for the caller to reject
/// before retaining another parser-side row.
fn graph_text_document_node_count(document: &Document) -> io::Result<u64> {
    let mut pending = Vec::<&DocBlock>::new();
    pending
        .try_reserve(document.roots.len())
        .map_err(|_| graph_text_capture_error("source parser-node stack allocation failed"))?;
    pending.extend(document.roots.iter().rev());
    let mut count = 0_u64;
    while let Some(block) = pending.pop() {
        count = count
            .checked_add(1)
            .ok_or_else(|| graph_text_capture_error("source parser-node counter overflow"))?;
        if count > MAX_GRAPH_TEXT_PARSER_NODES {
            return Ok(count);
        }
        pending
            .try_reserve(block.children.len())
            .map_err(|_| graph_text_capture_error("source parser-node stack allocation failed"))?;
        pending.extend(block.children.iter().rev());
    }
    Ok(count)
}

fn isolate_page_parse(
    e: PageEntry,
    journal_format: &JournalFormat,
    parse: impl FnOnce(&PageEntry) -> Option<(Document, String)>,
) -> PageParseResult {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parse(&e))) {
        Ok(Some((doc, rev))) => {
            let effective = effective_page_entry(journal_format, &e, &doc);
            Ok(Some((effective, doc, rev)))
        }
        Ok(None) => Ok(None),
        Err(payload) => {
            let _ = payload;
            if crate::backend_error::runtime_debug_diagnostics_enabled() {
                eprintln!("Tine search index skipped one page after a parse/projection panic");
            }
            Err(e.rel_path)
        }
    }
}

fn page_cache_worker_count() -> usize {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8);
    #[cfg(test)]
    let workers = workers.max(2);
    workers
}

#[cfg(test)]
const TEST_PAGE_PARSE_PANIC_SENTINEL: &str = "__TINE_TEST_PAGE_PARSE_PANIC__";

/// Compound asset extensions that a downstream matcher keys on AS A WHOLE (e.g.
/// drawio's editable SVG, whose `.drawio.svg` suffix is what surfaces the
/// "Edit in draw.io" affordance). De-dup must insert its `_N` counter BEFORE the
/// whole suffix — `flow.drawio.svg` must collide to `flow_1.drawio.svg`, NOT
/// `flow.drawio_1.svg` (a naive last-dot split), which would still end in `.svg`
/// but no longer match `\.drawio\.svg$` and silently lose the editor button
/// (GH #38). Longest match wins; case-insensitive.
const COMPOUND_ASSET_EXTS: &[&str] = &[".drawio.svg", ".excalidraw.svg", ".excalidraw.png"];

/// Split an asset filename into (stem, extension) for de-dup counter insertion,
/// preserving known compound extensions (see `COMPOUND_ASSET_EXTS`). Falls back
/// to a last-dot split for ordinary single extensions.
fn split_asset_stem_ext(name: &str) -> (String, String) {
    let lower = name.to_ascii_lowercase();
    for ext in COMPOUND_ASSET_EXTS {
        if lower.ends_with(ext) {
            let cut = name.len() - ext.len();
            return (name[..cut].to_string(), name[cut..].to_string());
        }
    }
    match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.to_string(), String::new()),
    }
}

#[cfg(test)]
fn reserve_asset(assets: &Path, name: &str) -> io::Result<(String, fs::File)> {
    top_level_asset_name(name)?;
    let create_new = |n: &str| {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(assets.join(n))
    };
    match create_new(name) {
        Ok(f) => return Ok((name.to_string(), f)),
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let (stem, ext) = split_asset_stem_ext(name);
    let mut i = 1;
    loop {
        let candidate = format!("{stem}_{i}{ext}");
        match create_new(&candidate) {
            Ok(f) => return Ok((candidate, f)),
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => i += 1,
        }
    }
}

#[cfg(test)]
thread_local! {
    static CACHE_LINEAR_SCAN_STEPS: std::cell::Cell<usize> = std::cell::Cell::new(0);
    static GRAPH_TEXT_INVENTORY_ENTRY_VISITS: std::cell::Cell<usize> = std::cell::Cell::new(0);
    static GRAPH_TEXT_CONTENT_READS: std::cell::Cell<usize> = std::cell::Cell::new(0);
    static GRAPH_TEXT_PARSE_ATTEMPTS: std::cell::Cell<usize> = std::cell::Cell::new(0);
    static EXACT_PAGE_DTO_PARSE_ATTEMPTS: std::cell::Cell<usize> = std::cell::Cell::new(0);
    static GRAPH_TEXT_VALIDATION_TARGET_READS: std::cell::Cell<usize> = std::cell::Cell::new(0);
    static JOURNAL_PROJECTION_GUARDED_PARSE_PAIRS: std::cell::Cell<usize> = std::cell::Cell::new(0);
}

#[cfg(test)]
fn count_cache_linear_scan(n: usize) {
    CACHE_LINEAR_SCAN_STEPS.with(|steps| steps.set(steps.get() + n));
}

fn walk_page_files(dir: &Path, mut visit: impl FnMut(PathBuf)) {
    // Descend into sub-directories (#21). Logseq scans the whole graph root
    // recursively, so a page archived under `pages/client-a/foo.md` is a real
    // page — keyed by its BASENAME (`foo`); the sub-path is discarded, matching
    // OG's `path->file-name` (the file's own `path` stays its load/save identity).
    // One stack-based walk, O(files), no re-scan.
    //
    // `file_type()` does not follow symlinks. Check it for page-looking entries
    // too: otherwise `pages/secret.md -> /outside/secret.md` would be indexed and
    // exposed. Hidden dirs (`.git` &c.) are skipped — never a page store.
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if is_page_file(&path) && file_type.is_file() {
                visit(path);
                continue;
            }
            // Non-page entry: recurse if it's a real (non-symlink, non-hidden)
            // sub-directory. This is the only stat we pay, and never on the hot
            // page-file path above.
            let hidden = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.starts_with('.'))
                .unwrap_or(true);
            if !hidden && file_type.is_dir() {
                stack.push(path);
            }
        }
    }
}

/// Union of two markdown page-property pre-blocks: keep `mine` and append any
/// `key:: value` line `theirs` defines that `mine` doesn't (mine wins on a clash),
/// so a sync-conflict resolve doesn't silently drop the other device's
/// `alias::`/`tags::`/`icon::`. Free text in `theirs`' pre-block is dropped (rare;
/// the conflict copy is trashed-recoverable). Mirrors the property-carry in
/// [`Graph::merge_pages`].
/// Exposed so the Tauri conflict-capsule resolver composes this one
/// pre-block union instead of re-implementing it over `BlockDto` (D-14).
pub fn union_pre(mine: Option<&str>, theirs: Option<&str>) -> Option<String> {
    let mine = mine.unwrap_or("");
    let Some(theirs) = theirs else {
        return (!mine.is_empty()).then(|| mine.to_string());
    };
    let mine_keys: std::collections::HashSet<String> = mine
        .lines()
        .filter_map(|l| doc::parse_property_line(l).map(|(k, _)| k.to_ascii_lowercase()))
        .collect();
    let extra: Vec<&str> = theirs
        .lines()
        .filter(|l| {
            doc::parse_property_line(l)
                .is_some_and(|(k, _)| !mine_keys.contains(&k.to_ascii_lowercase()))
        })
        .collect();
    if extra.is_empty() {
        return (!mine.is_empty()).then(|| mine.to_string());
    }
    let mut pre = mine.to_string();
    if !pre.is_empty() && !pre.ends_with('\n') {
        pre.push('\n');
    }
    pre.push_str(&extra.join("\n"));
    Some(pre)
}

/// True if any block in the subtree has a non-empty line that isn't a `key::`
/// property line — i.e. the page is more than an empty/placeholder bullet.
fn doc_has_content(blocks: &[DocBlock]) -> bool {
    blocks.iter().any(|b| {
        b.raw
            .lines()
            .any(|l| !l.trim().is_empty() && crate::doc::parse_property_line(l).is_none())
            || doc_has_content(&b.children)
    })
}

/// Versioned namespace for file-mode runtime block locators. These UUIDs are
/// store/UI keys only: persisted `id::` remains the external `((id))` identity.
const FILE_BLOCK_RUNTIME_NAMESPACE_V1: Uuid =
    Uuid::from_u128(0x1e0c_5a13_9b42_5da4_a73c_0be5_8f6a_2320);

/// Versioned namespace for the projection key of a live runtime id that is not
/// itself a UUID. A block created in the editor is saved with the frontend's
/// own id (`src/store.ts` `freshId()`: `b<base36 time>-<counter>`), and the
/// in-memory save path deliberately keeps it so the editor can go on addressing
/// the block. Such an id is a store/UI key exactly like a structural one; the
/// projection needs a 16-byte key for it, never a refusal.
const LIVE_RUNTIME_ID_KEY_NAMESPACE_V1: Uuid =
    Uuid::from_u128(0x7c2d_4b9e_31a6_4f08_9d15_6e3a_b0c4_5d71);

/// The deterministic 16-byte projection key of a live, non-UUID runtime id.
pub(crate) fn live_runtime_id_key(runtime_id: &str) -> Uuid {
    deterministic_runtime_uuid(LIVE_RUNTIME_ID_KEY_NAMESPACE_V1, runtime_id.as_bytes())
}

fn normalized_runtime_owner(owner: &str) -> io::Result<String> {
    let owner = owner.replace('\\', "/");
    let mut parts = Vec::new();
    for part in owner.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "runtime identity owner must be graph-relative",
                ))
            }
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime identity owner must not be empty",
        ));
    }
    Ok(parts.join("/"))
}

fn deterministic_runtime_uuid(namespace: Uuid, name: &[u8]) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    hasher.update((name.len() as u64).to_be_bytes());
    hasher.update(name);
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 9562 variant + version 8 (application-defined deterministic UUID).
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn runtime_owner_namespace(domain: &str, owner: &str) -> io::Result<Uuid> {
    let owner = normalized_runtime_owner(owner)?;
    let mut name = Vec::with_capacity(domain.len() + owner.len() + 16);
    name.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    name.extend_from_slice(domain.as_bytes());
    name.extend_from_slice(&(owner.len() as u64).to_be_bytes());
    name.extend_from_slice(owner.as_bytes());
    Ok(deterministic_runtime_uuid(
        FILE_BLOCK_RUNTIME_NAMESPACE_V1,
        &name,
    ))
}

fn structural_runtime_child(parent: Uuid, sibling: u64) -> Uuid {
    deterministic_runtime_uuid(parent, &sibling.to_be_bytes())
}

/// Reproduce a fresh Direct parse's runtime ID from its stored structural path.
/// Public/external `id::` is a separate identity. This is the R3 result
/// constructor seam: resolve admitted output without a startup-wide ID rewrite.
pub(crate) fn doc_runtime_id_for_order(owner_rel_path: &str, order_key: &str) -> io::Result<Uuid> {
    let mut structural = runtime_owner_namespace("file-block-runtime-v1", owner_rel_path)?;
    let mut depth = 0;
    for component in order_key.split('/') {
        depth += 1;
        if depth > MAX_BLOCK_DEPTH
            || component.len() != 8
            || !component
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid projected structural order",
            ));
        }
        let sibling = u32::from_str_radix(component, 16).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid projected structural order",
            )
        })?;
        structural = structural_runtime_child(structural, u64::from(sibling));
    }
    Ok(structural)
}

fn assign_runtime_ids_checked(blocks: &mut [DocBlock], parent: Uuid) -> io::Result<()> {
    struct Frame<'a> {
        blocks: std::slice::IterMut<'a, DocBlock>,
        parent: Uuid,
        sibling: usize,
    }
    let mut frames: [Option<Frame<'_>>; MAX_BLOCK_DEPTH] = std::array::from_fn(|_| None);
    let mut len = usize::from(!blocks.is_empty());
    if len != 0 {
        frames[0] = Some(Frame {
            blocks: blocks.iter_mut(),
            parent,
            sibling: 0,
        });
    }
    while len != 0 {
        let mut frame = frames[len - 1].take().expect("active runtime-id frame");
        let Some(block) = frame.blocks.next() else {
            len -= 1;
            continue;
        };
        let sibling = frame.sibling;
        frame.sibling = frame
            .sibling
            .checked_add(1)
            .ok_or_else(allocation_overflow)?;
        let structural = structural_runtime_child(frame.parent, usize_to_u64(sibling)?);
        frames[len - 1] = Some(frame);
        if block.uuid.is_empty() {
            block.uuid = structural.to_string();
        }
        if !block.children.is_empty() {
            if len == MAX_BLOCK_DEPTH {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph page block nesting exceeds 128 levels",
                ));
            }
            frames[len] = Some(Frame {
                blocks: block.children.iter_mut(),
                parent: structural,
                sibling: 0,
            });
            len += 1;
        }
    }
    Ok(())
}

/// Seed missing runtime keys for a graph-backed document from its normalized,
/// graph-relative physical owner. Existing live keys survive ordinary saves.
pub fn assign_doc_runtime_ids(roots: &mut [DocBlock], owner_rel_path: &str) {
    let owner = runtime_owner_namespace("file-block-runtime-v1", owner_rel_path)
        .expect("validated document runtime owner");
    let _ = assign_runtime_ids_checked(roots, owner);
}

fn assign_virtual_doc_runtime_ids(
    roots: &mut [DocBlock],
    domain: &str,
    owner: &str,
) -> io::Result<()> {
    let owner = runtime_owner_namespace(domain, owner)?;
    assign_runtime_ids_checked(roots, owner)
}

fn block_runtime_id(b: &DocBlock) -> String {
    assert!(
        !b.uuid.is_empty(),
        "DocBlock must have an explicit runtime owner before DTO projection"
    );
    b.uuid.clone()
}

/// Convert a parsed (cached) block to a DTO, carrying its stable uuid as the id.
pub fn block_to_dto(b: &DocBlock) -> io::Result<BlockDto> {
    doc_blocks_to_dto_checked(std::slice::from_ref(b))?
        .pop()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "block projection produced no result",
            )
        })
}

/// Convert one block to the result-row wire shape. Result membership is about
/// block identity, raw text, and facets; descendants belong to the source page
/// and are hydrated once per page by live consumers. Keeping this constructor
/// separate makes it difficult to accidentally reintroduce overlapping subtree
/// amplification in queries, references, search, or batched resolution.
/// The ONE parser-backed `DocBlock` → shallow `BlockDto` facet projection
/// (DUP-6/B9, 2026-08-25 duplication audit): both DTO constructors delegate
/// here, so a new `BlockDto` facet is a one-site decision on this path. `id`
/// validation stays with the callers — their polite-error vs assert difference
/// is deliberate.
fn doc_block_facets_dto(block: &DocBlock, id: String) -> BlockDto {
    shallow_block_facets_dto(ShallowBlockFacets {
        id,
        raw: block.raw.clone(),
        collapsed: block.collapsed(),
        heading_level: block.heading_level(),
        marker: block.marker().map(str::to_string),
        priority: block.priority().map(str::to_string),
        scheduled: block.scheduled().map(str::to_string),
        deadline: block.deadline().map(str::to_string),
        tags: block.tags(),
        properties: block.properties(),
    })
}

/// The facets a shallow result row carries, independent of where they were
/// read from. [`doc_block_facets_dto`] fills them from a parsed `DocBlock`;
/// R3's database result read fills the same ten from `block_text`, `blocks`,
/// `tasks`, `block_planning`, `tags` and `properties`.
pub(crate) struct ShallowBlockFacets {
    pub(crate) id: String,
    pub(crate) raw: String,
    pub(crate) collapsed: bool,
    pub(crate) heading_level: Option<u8>,
    pub(crate) marker: Option<String>,
    pub(crate) priority: Option<String>,
    pub(crate) scheduled: Option<String>,
    pub(crate) deadline: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) properties: Vec<(String, String)>,
}

/// The ONE shallow `BlockDto` FIELD LIST (DUP-6/B9, extended for R3).
///
/// The three fixed fields a result row never carries — `children`,
/// `breadcrumb`, `page_property` — are decided exactly here, so the
/// parser-backed and the database-backed constructors cannot drift into two
/// different answers to the same question (I-12, I-19). A new `BlockDto` facet
/// is still a one-site decision; it is now a one-site decision for BOTH
/// backends.
pub(crate) fn shallow_block_facets_dto(facets: ShallowBlockFacets) -> BlockDto {
    let ShallowBlockFacets {
        id,
        raw,
        collapsed,
        heading_level,
        marker,
        priority,
        scheduled,
        deadline,
        tags,
        properties,
    } = facets;
    BlockDto {
        id,
        raw,
        collapsed,
        children: Vec::new(),
        breadcrumb: Vec::new(),
        page_property: false,
        marker,
        priority,
        heading_level,
        scheduled,
        deadline,
        tags,
        properties,
    }
}

pub fn block_to_shallow_dto(b: &DocBlock) -> BlockDto {
    doc_block_facets_dto(b, block_runtime_id(b))
}

/// The ONE `BlockDto` → `DocBlock` field mapping (2026-08-25 duplication
/// audit, DUP-application-query-twin). Every path that rehydrates a
/// parseable block from its wire DTO goes through this constructor, so a new
/// `DocBlock` field is initialized in exactly one place. The tree walkers
/// around it deliberately differ — [`dto_blocks_to_doc_checked`] is iterative,
/// depth-bounded, and allocation-guarded because it validates untrusted wire
/// page loads, while the query/projection walkers recurse over block trees that
/// are already inside the trusted process — but the per-block field mapping
/// must not diverge. A source guard in this module's tests pins the invariant.
pub(crate) fn dto_block_to_doc_block(block: &BlockDto, is_org: bool) -> DocBlock {
    DocBlock {
        raw: block.raw.clone(),
        children: Vec::new(),
        uuid: block.id.clone(),
        is_org,
        proj: std::sync::OnceLock::new(),
    }
}

/// Bounded tree walker over [`dto_block_to_doc_block`] for untrusted wire
/// page loads: iterative (no recursion), depth-limited, allocation-guarded.
pub(crate) fn dto_blocks_to_doc_checked(
    blocks: &[BlockDto],
    is_org: bool,
) -> io::Result<Vec<DocBlock>> {
    struct Frame<'a> {
        source: &'a [BlockDto],
        next: usize,
        output: Vec<DocBlock>,
    }
    let mut frames: [Option<Frame<'_>>; MAX_BLOCK_DEPTH] = std::array::from_fn(|_| None);
    frames[0] = Some(Frame {
        source: blocks,
        next: 0,
        output: Vec::with_capacity(blocks.len()),
    });
    let mut len = 1_usize;
    loop {
        let frame = frames[len - 1]
            .as_mut()
            .expect("active DTO conversion frame");
        if frame.next == frame.source.len() {
            let completed = frames[len - 1]
                .take()
                .expect("completed DTO conversion frame")
                .output;
            len -= 1;
            if len == 0 {
                return Ok(completed);
            }
            frames[len - 1]
                .as_mut()
                .expect("parent DTO conversion frame")
                .output
                .last_mut()
                .expect("child frame has parent")
                .children = completed;
            continue;
        }
        let block = &frame.source[frame.next];
        frame.next = frame.next.checked_add(1).ok_or_else(allocation_overflow)?;
        frame.output.push(dto_block_to_doc_block(block, is_org));
        if !block.children.is_empty() {
            if len == MAX_BLOCK_DEPTH {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph page block nesting exceeds 128 levels",
                ));
            }
            frames[len] = Some(Frame {
                source: &block.children,
                next: 0,
                output: Vec::with_capacity(block.children.len()),
            });
            len += 1;
        }
    }
}

fn doc_blocks_to_dto_checked(blocks: &[DocBlock]) -> io::Result<Vec<BlockDto>> {
    fn output_with_source_capacity(source_len: usize) -> io::Result<Vec<BlockDto>> {
        let mut output = Vec::new();
        output
            .try_reserve_exact(source_len)
            .map_err(|_| allocation_overflow())?;
        Ok(output)
    }

    struct Frame<'a> {
        source: &'a [DocBlock],
        next: usize,
        output: Vec<BlockDto>,
    }
    let mut frames: [Option<Frame<'_>>; MAX_BLOCK_DEPTH] = std::array::from_fn(|_| None);
    frames[0] = Some(Frame {
        source: blocks,
        next: 0,
        output: output_with_source_capacity(blocks.len())?,
    });
    let mut len = 1_usize;
    loop {
        let frame = frames[len - 1]
            .as_mut()
            .expect("active document conversion frame");
        if frame.next == frame.source.len() {
            let completed = frames[len - 1]
                .take()
                .expect("completed document conversion frame")
                .output;
            len -= 1;
            if len == 0 {
                return Ok(completed);
            }
            frames[len - 1]
                .as_mut()
                .expect("parent document conversion frame")
                .output
                .last_mut()
                .expect("child frame has parent")
                .children = completed;
            continue;
        }
        let block = &frame.source[frame.next];
        frame.next = frame.next.checked_add(1).ok_or_else(allocation_overflow)?;
        if block.uuid.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "block has no assigned runtime identity",
            ));
        }
        frame
            .output
            .push(doc_block_facets_dto(block, block.uuid.clone()));
        if !block.children.is_empty() {
            if len == MAX_BLOCK_DEPTH {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph document nesting exceeds 128 levels",
                ));
            }
            frames[len] = Some(Frame {
                source: &block.children,
                next: 0,
                output: output_with_source_capacity(block.children.len())?,
            });
            len += 1;
        }
    }
}

pub(crate) fn page_dto_checked(entry: &PageEntry, doc: &Document) -> io::Result<PageDto> {
    Ok(PageDto {
        activation: None,
        name: entry.name.clone(),
        kind: entry.kind,
        title: entry.name.clone(),
        pre_block: doc.pre_block.clone(),
        blocks: doc_blocks_to_dto_checked(&doc.roots)?,
        rev: None,
        format: Format::from_path(&entry.path),
        read_only: false,
        path: String::new(),
        guide: false,
    })
}

/// Build a Markdown page DTO from raw Logseq Markdown without touching disk.
/// Used by the bundled in-app Guide so it reuses the same document parser and
/// DTO projection as normal graph pages.
pub fn markdown_page_dto(name: &str, title: &str, markdown: &str) -> io::Result<PageDto> {
    let mut doc = doc::parse(markdown);
    assign_virtual_doc_runtime_ids(&mut doc.roots, "bundled-markdown-v1", name)?;
    let blocks = doc_blocks_to_dto_checked(&doc.roots)?;
    Ok(PageDto {
        activation: None,
        name: name.to_string(),
        kind: PageKind::Page,
        title: title.to_string(),
        pre_block: doc.pre_block.clone(),
        blocks,
        rev: None,
        format: Format::Md,
        read_only: false,
        path: String::new(),
        guide: false,
    })
}

/// The one bounded `PageDto` -> `Document` conversion. Exposed so native
/// callers (the conflict-capsule commands) never re-grow a recursive twin.
pub fn page_dto_document(page: &PageDto) -> io::Result<Document> {
    Ok(Document {
        pre_block: page.pre_block.clone(),
        roots: dto_blocks_to_doc_checked(&page.blocks, page.format == Format::Org)?,
    })
}

pub(crate) fn existing_document_page_dto(
    base: &PageDto,
    mut document: Document,
) -> io::Result<PageDto> {
    assign_virtual_doc_runtime_ids(
        &mut document.roots,
        "graph-document-update-v1",
        if base.path.is_empty() {
            &base.name
        } else {
            &base.path
        },
    )?;
    let mut page = base.clone();
    page.pre_block = document.pre_block;
    page.blocks = doc_blocks_to_dto_checked(&document.roots)?;
    Ok(page)
}

/// Whether a page should load read-only: an org file whose on-disk bytes don't
/// round-trip through Tine's org parser/serializer, so Tine must never rewrite
/// it (lest it corrupt the user's graph). Markdown pages are always editable.
fn read_only_org(path: &Path, content: &str) -> bool {
    Format::from_path(path) == Format::Org && !crate::org::org_editable(content)
}

/// A page's `icon::` property value from its pre-block, handling markdown
/// (`icon:: 🏁`), org property drawers (`:icon: 🏁`) and org `#+ICON:` directives.
/// None if absent or blank.
pub(crate) fn pre_block_icon(pre: &str) -> Option<String> {
    for line in pre.lines() {
        // Markdown `icon:: value` (single shared parser; needs the `::`).
        if let Some((k, v)) = crate::doc::parse_property_line(line) {
            let v = v.trim();
            if k.eq_ignore_ascii_case("icon") && !v.is_empty() {
                return Some(v.to_string());
            }
        }
        let t = line.trim();
        // Org property drawer `:icon: value` or directive `#+ICON: value`.
        for stripped in [t.strip_prefix(':'), t.strip_prefix("#+")]
            .into_iter()
            .flatten()
        {
            if let Some(idx) = stripped.find(':') {
                let (k, v) = (&stripped[..idx], stripped[idx + 1..].trim());
                if k.eq_ignore_ascii_case("icon") && !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Stable SHA-256 content digest used as the exact loaded-byte baseline for an
/// audited existing-file save.
pub fn content_rev(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}

/// Encode one logical page title as a portable on-disk filename stem.
///
/// This is the shared create/rename identity boundary. It retains OG's
/// configured namespace spellings (`%2F` for legacy, `___` for triple-lowbar)
/// and percent syntax while making the mapping injective: a literal percent is
/// escaped before generated escapes are introduced, and every character the
/// matching decoder would otherwise reinterpret is escaped. New paths are safe
/// on Windows as well as POSIX; already-loaded pages remain path-pinned and are
/// never renamed merely because their historical spelling is non-canonical.
pub(crate) fn encode_page_name(name: &str, fmt: FileNameFormat) -> String {
    let trailing_windows_unsafe = name
        .trim_end_matches(|character| character == ' ' || character == '.')
        .len();
    let mut escaped = String::with_capacity(name.len());
    for (offset, character) in name.char_indices() {
        let encode = character == '%'
            || character <= '\u{1f}'
            || character == '\u{7f}'
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*' | '#'
            )
            || (character == '.'
                && (fmt == FileNameFormat::Legacy
                    || offset == 0
                    || offset >= trailing_windows_unsafe))
            || (character == ' ' && offset >= trailing_windows_unsafe);
        if encode {
            let mut bytes = [0_u8; 4];
            for byte in character.encode_utf8(&mut bytes).as_bytes() {
                push_percent_byte(&mut escaped, *byte);
            }
        } else {
            escaped.push(character);
        }
    }

    let mut encoded = match fmt {
        FileNameFormat::Legacy => escaped.replace('/', "%2F"),
        FileNameFormat::TripleLowbar => escaped
            // Disambiguate underscores that would otherwise be ambiguous after
            // `/`→`___` (OG `fs.cljs`), THEN map the namespace separator.
            .replace("___", "%5F%5F%5F")
            .replace("_/", "%5F/")
            .replace("/_", "/%5F")
            .replace('/', "___"),
    };
    escape_windows_device_stem(&mut encoded);
    encoded
}

fn push_percent_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output.push('%');
    output.push(char::from(HEX[usize::from(byte >> 4)]));
    output.push(char::from(HEX[usize::from(byte & 0x0f)]));
}

/// Win32 reserves these device bodies case-insensitively even when another
/// extension follows. Superscript 1/2/3 are documented aliases for COM/LPT.
fn escape_windows_device_stem(stem: &mut String) {
    let body = stem
        .split('.')
        .next()
        .unwrap_or(stem)
        .trim_end_matches(' ')
        .to_uppercase();
    let reserved = matches!(body.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            body.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
    if reserved {
        let first_len = stem.chars().next().map(char::len_utf8).unwrap_or(0);
        let mut safe = String::with_capacity(stem.len() + 2);
        for byte in stem.as_bytes().iter().take(first_len) {
            push_percent_byte(&mut safe, *byte);
        }
        safe.push_str(&stem[first_len..]);
        *stem = safe;
    }
}

/// Inverse of [`encode_page_name`]. Legacy: dot→slash, then percent-decode
/// (`%2F`→`/`), matching Logseq's backward-compatible title parser.
/// Triple-lowbar: `___`→`/` FIRST, then percent-decode — the OG order
/// (`util.cljs:153-160`), so an encoded literal `___` (stored `%5F%5F%5F`)
/// survives instead of being turned into a separator.
pub(crate) fn decode_page_name(stem: &str, fmt: FileNameFormat) -> String {
    match fmt {
        // OG's legacy title parser predates percent-encoded namespace
        // separators: it first maps every dot to `/`, then URI-decodes. Thus a
        // retained pre-2022 `Foo.Bar.md` and a later `Foo%2FBar.md` share the
        // same effective `Foo/Bar` page identity.
        FileNameFormat::Legacy => percent_decode(&stem.replace('.', "/")),
        FileNameFormat::TripleLowbar => percent_decode(&stem.replace("___", "/")),
    }
}

/// Decode `%XX` percent-escapes (UTF-8 aware, like JS `decodeURIComponent`). An
/// invalid or truncated escape is left literal rather than dropped.
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_nibble(b[i + 1]), hex_nibble(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// A unique-ish label (epoch millis + process-local sequence) for trashed files,
/// so deleting two pages with the same name doesn't collide in the trash.
/// Collect every `assets/<name>` reference in `text` into `into`. Captures both
/// markdown (`![](../assets/x.png)`, `[f](../assets/x.pdf)`) and org
/// (`[[file:../assets/x.png]]`) forms — the name runs from after `assets/` to the
/// next markup closer (`)`/`]`/quote/etc.) or line break. Crucially it does NOT
/// stop at a space, so a referenced filename containing spaces is matched in full
/// (mis-truncating it would make `orphan_assets` flag a file that IS in use). The
/// first path segment is added too, so a PDF area-image ref (`assets/<key>/p.png`)
/// marks `<key>` as in use.
fn collect_asset_refs(text: &str, into: &mut std::collections::HashSet<String>) {
    let mut rest = text;
    while let Some(i) = rest.find("assets/") {
        let after = &rest[i + "assets/".len()..];
        let end = after
            .find(|c: char| {
                matches!(
                    c,
                    ')' | ']' | '"' | '\'' | '<' | '>' | '|' | '\n' | '\r' | '\t'
                )
            })
            .unwrap_or(after.len());
        let name = &after[..end];
        if !name.is_empty() {
            insert_asset_ref(into, name);
            if let Some(seg) = name.split('/').next() {
                if seg != name {
                    insert_asset_ref(into, seg);
                }
            }
        }
        rest = &after[end..];
    }
}

/// Record an asset reference under BOTH its raw form AND its percent-decoded form.
/// A link like `../assets/my%20file.png` names the on-disk file `my file.png`, so
/// comparing the raw URL substring against directory entries would miss the real
/// file and let `orphan_assets` offer an IN-USE asset for trashing (DS Codex#7).
/// Keeping the raw form too covers a file literally named with a `%` escape.
fn insert_asset_ref(into: &mut std::collections::HashSet<String>, raw: &str) {
    let decoded = percent_decode(raw);
    if decoded != raw {
        into.insert(decoded);
    }
    into.insert(raw.to_string());
}

fn collect_block_asset_refs(b: &DocBlock, into: &mut std::collections::HashSet<String>) {
    collect_asset_refs(&b.raw, into);
    for c in &b.children {
        collect_block_asset_refs(c, into);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrashEntryKind {
    Asset,
    Page,
    Journal,
    Conflict,
    Other,
}

impl TrashEntryKind {
    fn dir_name(self) -> Option<&'static str> {
        match self {
            TrashEntryKind::Asset => Some("assets"),
            TrashEntryKind::Page => Some("pages"),
            TrashEntryKind::Journal => Some("journals"),
            TrashEntryKind::Conflict => Some("conflicts"),
            TrashEntryKind::Other => None,
        }
    }
}

/// Translate the storage crate's physical boundary into the Graph API's I/O
/// boundary without losing the collision distinction needed by delete retries.
fn graph_text_trash_filesystem_error(error: FilesystemError) -> io::Error {
    match error {
        FilesystemError::Io(error) => error,
        FilesystemError::DurableNameOperationUnavailable(message) => {
            io::Error::new(io::ErrorKind::Unsupported, message)
        }
        FilesystemError::UnsafeEntry(message) => io::Error::new(io::ErrorKind::InvalidData, message),
        FilesystemError::StoredLengthMismatch {
            path,
            expected,
            actual,
        } => io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "graph recovery stored length mismatch for {path}: expected {expected}, got {actual}"
            ),
        ),
        FilesystemError::StoredFileTooLarge { path, length, limit } => io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "graph recovery stored file is too large for {path}: {length} bytes exceeds {limit}"
            ),
        ),
        FilesystemError::ByteCollision => io::Error::new(
            io::ErrorKind::AlreadyExists,
            "graph recovery destination contains different bytes",
        ),
    }
}

fn trash_root(root: &Path) -> PathBuf {
    root.join("logseq").join(".tine-trash")
}

fn typed_trash_dir(root: &Path, kind: TrashEntryKind) -> PathBuf {
    trash_root(root).join(kind.dir_name().unwrap_or("other"))
}

fn trash_dir_kind(path: &Path) -> Option<TrashEntryKind> {
    match path.file_name().and_then(|s| s.to_str()) {
        Some("assets") => Some(TrashEntryKind::Asset),
        Some("pages") => Some(TrashEntryKind::Page),
        Some("journals") => Some(TrashEntryKind::Journal),
        Some("conflicts") => Some(TrashEntryKind::Conflict),
        _ => None,
    }
}

fn add_trash_stat(stats: &mut TrashStats, kind: TrashEntryKind, bytes: u64) {
    match kind {
        TrashEntryKind::Asset => {
            stats.count += 1;
            stats.bytes += bytes;
        }
        TrashEntryKind::Page => stats.pages += 1,
        TrashEntryKind::Journal => stats.journals += 1,
        TrashEntryKind::Conflict => stats.conflicts += 1,
        TrashEntryKind::Other => stats.other += 1,
    }
}

fn trash_stats(trash: &Path) -> TrashStats {
    let mut stats = TrashStats::default();
    let Ok(rd) = fs::read_dir(trash) else {
        return stats;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            if let Some(kind) = trash_dir_kind(&path) {
                add_typed_trash_dir_stats(&path, kind, &mut stats);
            } else {
                stats.other += 1;
            }
            continue;
        }
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        add_trash_stat(&mut stats, classify_legacy_trash_entry(&path, ft), bytes);
    }
    stats
}

fn add_typed_trash_dir_stats(path: &Path, kind: TrashEntryKind, stats: &mut TrashStats) {
    let Ok(rd) = fs::read_dir(path) else { return };
    for entry in rd.flatten() {
        let bytes = entry
            .file_type()
            .ok()
            .filter(|ft| ft.is_file())
            .and_then(|_| entry.metadata().ok())
            .map(|m| m.len())
            .unwrap_or(0);
        add_trash_stat(stats, kind, bytes);
    }
}

fn classify_legacy_trash_entry(path: &Path, ft: fs::FileType) -> TrashEntryKind {
    if !ft.is_file() {
        return TrashEntryKind::Other;
    }
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return TrashEntryKind::Other;
    };
    let original = legacy_trash_original_name(name);
    let original_path = Path::new(original);
    if path_is_sync_conflict(original_path) {
        return TrashEntryKind::Conflict;
    }
    if text_extension_from_path(original_path).is_some() {
        return original_path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|stem| crate::date::JournalDate::from_file_stem(stem).is_some())
            .map(|_| TrashEntryKind::Journal)
            .unwrap_or(TrashEntryKind::Page);
    }
    if legacy_name_is_asset(original) {
        TrashEntryKind::Asset
    } else {
        TrashEntryKind::Other
    }
}

fn legacy_trash_original_name(name: &str) -> &str {
    name.split_once("__")
        .map(|(_, original)| original)
        .unwrap_or(name)
}

fn legacy_name_is_asset(name: &str) -> bool {
    if name.starts_with('.') || name.contains('/') || name.contains('\\') || name.ends_with(".edn")
    {
        return false;
    }
    let Some(ext) = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
    else {
        return false;
    };
    matches!(
        ext.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "avif"
            | "svg"
            | "bmp"
            | "tif"
            | "tiff"
            | "heic"
            | "heif"
            | "pdf"
            | "mp4"
            | "mov"
            | "m4v"
            | "webm"
            | "mkv"
            | "avi"
            | "mp3"
            | "wav"
            | "m4a"
            | "ogg"
            | "flac"
            | "aac"
            | "opus"
            | "txt"
            | "csv"
            | "tsv"
            | "json"
            | "yaml"
            | "yml"
            | "zip"
            | "tar"
            | "gz"
            | "tgz"
            | "7z"
            | "rar"
            | "doc"
            | "docx"
            | "xls"
            | "xlsx"
            | "ppt"
            | "pptx"
            | "odt"
            | "ods"
            | "odp"
            | "rtf"
    )
}

fn trash_stamp() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{ms}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

fn move_to_trash(src: &Path, dest: &Path, trash: &Path) -> io::Result<()> {
    fs::create_dir_all(trash).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("could not create trash directory {}: {e}", trash.display()),
        )
    })?;
    move_file_noreplace(src, dest).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("could not move file to trash {}: {e}", trash.display()),
        )
    })
}

/// Atomically move one file without ever replacing an existing destination.
/// Platform-native no-replace rename semantics ensure the source name and inode
/// cannot be swapped between a check and an unlink.
pub(crate) fn move_file_noreplace(src: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let src = std::ffi::CString::new(src.as_os_str().as_bytes())?;
        let dest = std::ffi::CString::new(dest.as_os_str().as_bytes())?;
        // Atomic move + create-if-absent. Call the syscall directly: Android's
        // bionic `renameat2` wrapper is only exported from API 30, whereas
        // `syscall` is available from API 1. A wrapper reference here survived
        // the first GH #192 fix in backup.rs and still prevented the complete
        // native library from loading on Android 9. Whichever inode currently
        // owns `src` at the syscall boundary is moved intact, so the safety and
        // errno contracts remain unchanged.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                src.as_ptr(),
                libc::AT_FDCWD,
                dest.as_ptr(),
                libc::RENAME_NOREPLACE as libc::c_uint,
            )
        };
        return (result == 0)
            .then_some(())
            .ok_or_else(io::Error::last_os_error);
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let src = std::ffi::CString::new(src.as_os_str().as_bytes())?;
        let dest = std::ffi::CString::new(dest.as_os_str().as_bytes())?;
        let result = unsafe { libc::renamex_np(src.as_ptr(), dest.as_ptr(), libc::RENAME_EXCL) };
        return (result == 0)
            .then_some(())
            .ok_or_else(io::Error::last_os_error);
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut src: Vec<u16> = src.as_os_str().encode_wide().collect();
        let mut dest: Vec<u16> = dest.as_os_str().encode_wide().collect();
        src.push(0);
        dest.push(0);
        // MoveFileW fails when the destination already exists (unlike Rust's
        // cross-platform `rename` contract, which permits replacement).
        let result = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileW(src.as_ptr(), dest.as_ptr())
        };
        return (result != 0)
            .then_some(())
            .ok_or_else(io::Error::last_os_error);
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    )))]
    {
        let _ = (src, dest);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic no-replace move is unavailable on this platform",
        ))
    }
}

/// Atomically publish a newly-created file without clobbering a destination that
/// appeared after the caller's collision check. The payload is fsynced in a
/// same-directory temp, then atomically renamed into the final name only if absent.
pub(crate) fn atomic_write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_publish(path, bytes, PublishMode::NoReplace)
}

/// Suffix marking a file retired by [`atomic_replace_expected`] mid-publish.
pub(crate) const RETIRED_SUFFIX: &str = ".retired";

/// True for dir-fsync errors that mean "this filesystem does not offer it",
/// as opposed to a real durability failure.
///
/// Directory fsync is genuinely unavailable in several places: Windows has no
/// handle you can open this way, and some network filesystems reject it. Those
/// must stay non-fatal — that is why the call was best-effort to begin with.
/// A real `EIO`/`ENOSPC`, though, means the rename may not survive a crash, and
/// reporting durable success there is a false ack.
fn dir_fsync_is_unsupported(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::Unsupported
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::NotFound
    ) {
        return true;
    }
    // EBADF / EACCES / EISDIR / EINVAL from an fsync on a directory handle: the
    // filesystem (several NFS and FUSE implementations) is telling us the
    // operation does not apply, not that data was lost.
    const UNSUPPORTED_ERRNOS: [i32; 4] = [9, 13, 21, 22];
    error
        .raw_os_error()
        .is_some_and(|errno| UNSUPPORTED_ERRNOS.contains(&errno))
}

/// App-layer face of [`sync_dir`]: fsync `dir` so a rename into it survives a
/// crash. For src-tauri writers (settings registry, backup restore, window
/// identity) that previously discarded this result with `let _ = …` — a false
/// ack under the in-scope crash/power-loss threat (DUP-5).
pub fn sync_dir_for_rename(dir: &Path) -> io::Result<()> {
    sync_dir(dir)
}

/// App-layer face of [`dir_fsync_is_unsupported`], for writers that hold their
/// own directory handle (the cap-std restore path) and must apply the same
/// tolerate-unsupported / report-real policy.
pub fn dir_fsync_error_is_unsupported(error: &io::Error) -> bool {
    dir_fsync_is_unsupported(error)
}

/// `fsync` one regular file and record the durability barrier.
///
/// Every production file barrier in this module goes through here so the
/// per-operation barrier count is a measurable, testable number rather than an
/// invisible sum spread across modules (2026-08-26 cost-model audit, D1).
#[inline]
fn barrier_sync_all(file: &impl crate::durability_counters::DurableHandle) -> io::Result<()> {
    crate::durability_counters::sync_file(file)
}

/// `fsync` one already-opened directory handle and record the barrier.
#[inline]
fn barrier_sync_dir_handle(handle: &fs::File) -> io::Result<()> {
    crate::durability_counters::sync_directory(handle)
}

/// fsync a directory so a rename into it survives a crash.
///
/// Errors that mean "unsupported here" are swallowed; everything else is
/// propagated, because a caller told the durability succeeded when it did not
/// will happily report a save as committed.
fn sync_dir(dir: &Path) -> io::Result<()> {
    match fs::File::open(dir).and_then(|handle| barrier_sync_dir_handle(&handle)) {
        Ok(()) => Ok(()),
        Err(error) if dir_fsync_is_unsupported(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Outcome of a conditional publish.
#[derive(Debug)]
pub(crate) enum AtomicReplaceOutcome {
    /// `next` is now the file's content.
    Published,
    /// Someone else wrote the file first; nothing was published and their
    /// bytes stay in place.
    ExternalChanged,
}

/// Publish `next` to `path` ONLY if `path` still holds `expected`.
///
/// A plain temp+rename publish silently clobbers whatever arrived after the
/// caller's last read: Syncthing delivering a peer's `config.edn`, Logseq
/// writing the same file, an external editor saving a sidecar. Those bytes
/// vanish with no conflict copy and no refusal, which is exactly the class
/// ADR 0007 forbids for pages.
///
/// Checking the file and then renaming over it cannot fix that — the check and
/// the rename are two operations and the writer lands between them. So the
/// capture IS the rename: `path` is renamed aside to a unique sibling first,
/// which atomically takes whatever was current, and the comparison happens on
/// a name nobody else is writing.
///
/// The threat model is honest concurrent writers (crash/power loss, sync
/// delivery, external editors, a second instance), not an attacker forging
/// bytes with local write access — see the 2026-08-07 trust decision.
pub(crate) fn atomic_replace_expected(
    path: &Path,
    expected: &[u8],
    next: &[u8],
) -> io::Result<AtomicReplaceOutcome> {
    atomic_replace_expected_with_hooks(path, expected, next, || Ok(()))
}

fn atomic_replace_expected_with_hooks(
    path: &Path,
    expected: &[u8],
    next: &[u8],
    after_retire: impl Fn() -> io::Result<()>,
) -> io::Result<AtomicReplaceOutcome> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let pid = std::process::id();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{fname}.{pid}.{seq}.publish.tmp"));
    let retired = dir.join(format!(".{fname}.{pid}.{seq}{RETIRED_SUFFIX}"));

    let staged = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(next)?;
        barrier_sync_all(&file)?;
        Ok(())
    })();
    if let Err(error) = staged {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }

    // RETIRE: atomically capture whatever `path` currently holds. A no-replace
    // rename would be wrong here - we intend to vacate the slot.
    if let Err(error) = fs::rename(path, &retired) {
        let _ = fs::remove_file(&tmp);
        if error.kind() == io::ErrorKind::NotFound {
            // The file we meant to update is gone: an external delete. Not ours
            // to recreate silently.
            return Ok(AtomicReplaceOutcome::ExternalChanged);
        }
        return Err(error);
    }

    // A crash between here and the publish leaves the content in `retired`;
    // `restore_retired_files` puts it back on next open.
    if let Err(error) = after_retire() {
        let _ = move_file_noreplace(&retired, path);
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }

    let found = match fs::read(&retired) {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = move_file_noreplace(&retired, path);
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
    };
    if found != expected {
        // Someone wrote between the caller's read and now. Put their bytes back
        // and publish nothing. If an even newer external CREATE took the name,
        // `retired` stays for the recovery sweep rather than deleting anyone's
        // data.
        let _ = move_file_noreplace(&retired, path);
        let _ = fs::remove_file(&tmp);
        return Ok(AtomicReplaceOutcome::ExternalChanged);
    }

    // PUBLISH into the slot we vacated. No-replace: an AlreadyExists here means
    // an external CREATE won the window, and its bytes are not ours to replace.
    if let Err(error) = move_file_noreplace(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        if error.kind() == io::ErrorKind::AlreadyExists {
            // Our retired bytes stay on disk for the sweep to triage.
            return Ok(AtomicReplaceOutcome::ExternalChanged);
        }
        let _ = move_file_noreplace(&retired, path);
        return Err(error);
    }

    sync_dir(dir)?;
    let _ = fs::remove_file(&retired);
    Ok(AtomicReplaceOutcome::Published)
}

/// Recover files stranded mid-publish by a crash.
///
/// [`atomic_replace_expected`] briefly leaves `path` non-existent while its
/// content sits under a `.retired` sibling. A crash in that window would
/// otherwise look like a deleted file. Restores the content when the target is
/// missing; otherwise the publish completed (or an external writer recreated
/// the file), so the retired copy goes to recoverable trash rather than being
/// deleted outright.
///
/// Registered directories only - never a whole-graph walk.
pub(crate) fn restore_retired_files(root: &Path, dirs: &[PathBuf]) -> io::Result<usize> {
    let mut recovered = 0usize;
    for dir in dirs {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let retired = entry.path();
            let Some(name) = retired.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(target_name) = retired_target_name(name) else {
                continue;
            };
            let target = dir.join(target_name);
            if target.exists() {
                // The publish completed, or an external writer recreated the
                // file. Either way the retired copy is superseded - keep it
                // recoverable instead of deleting it.
                let trash = typed_trash_dir(root, TrashEntryKind::Conflict);
                fs::create_dir_all(&trash)?;
                move_file_noreplace(&retired, &trash.join(name))?;
                continue;
            }
            move_file_noreplace(&retired, &target)?;
            recovered += 1;
        }
    }
    Ok(recovered)
}

/// `.config.edn.1234.7.retired` -> `config.edn`.
fn retired_target_name(retired: &str) -> Option<&str> {
    let rest = retired.strip_prefix('.')?;
    let rest = rest.strip_suffix(RETIRED_SUFFIX)?;
    let (rest, _seq) = rest.rsplit_once('.')?;
    let (name, _pid) = rest.rsplit_once('.')?;
    (!name.is_empty()).then_some(name)
}

/// Atomic write: write to a temp file in the same directory, then rename. The
/// temp name is unique per write (pid + sequence) so two concurrent writers to
/// the same path (e.g. an autosave and a highlight/rename rewrite) can't truncate
/// each other's temp; the rename is still atomic. The temp is removed if the
/// write fails, so a unique name never leaks an orphan behind.
/// How [`atomic_publish`] lands the temp on its final name.
enum PublishMode {
    /// `fs::rename` — replaces an existing file (the ordinary save shape).
    Replace,
    /// `move_file_noreplace` — create-only; never clobbers a concurrent creator.
    NoReplace,
}

/// THE temp+fsync+rename publish implementation, shared by [`atomic_write`]
/// and [`atomic_write_new`] (DUP-5, 2026-08-25 duplication audit: the family
/// had drifted into copies with different failure policies; the rationale
/// lives here once).
///
/// - The temp name is unique per write (pid + per-process sequence) so two
///   concurrent writers to the same path can't truncate each other's temp.
/// - The temp is hidden (`.`-prefixed) and ends in `.tmp` — the shape the
///   watcher's `is_tine_atomic_page_temp_path` and the wire-side recognizer
///   understand; change it only together with both recognizers.
/// - Directory-fsync errors that mean "unsupported here" are tolerated; a real
///   `EIO`/`ENOSPC` is REPORTED, because a caller told this succeeded will
///   report the save as durably committed (in-scope threat: crash/power loss
///   right after the rename).
fn atomic_publish(path: &Path, bytes: &[u8], mode: PublishMode) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("page");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let infix = match mode {
        PublishMode::Replace => "",
        PublishMode::NoReplace => ".new",
    };
    let tmp = dir.join(format!(".{fname}.{}.{seq}{infix}.tmp", std::process::id()));
    let res = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        barrier_sync_all(&f)?;
        drop(f);
        match mode {
            PublishMode::Replace => fs::rename(&tmp, path),
            PublishMode::NoReplace => move_file_noreplace(&tmp, path),
        }
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp); // never leave a temp behind on failure
        return res;
    }
    sync_dir(dir)
}

/// Atomically replace a small user-selected output and durably publish its
/// directory entry. This does not provide graph mutation conflict semantics;
/// callers remain responsible for choosing an appropriate destination.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_publish(path, bytes, PublishMode::Replace)
}

fn configured_root_components(root: &str) -> Option<Vec<&str>> {
    if root.is_empty() || root.starts_with('/') || root.contains('\\') || root.contains('\0') {
        return None;
    }
    let components = root.split('/').collect::<Vec<_>>();
    components
        .iter()
        .all(|component| projection_component_is_portable(component))
        .then_some(components)
}

fn projection_component_is_portable(component: &str) -> bool {
    graph_text_component_is_portable(component)
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    windows
))]
fn require_projection_platform() -> io::Result<()> {
    Ok(())
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    windows
)))]
fn require_projection_platform() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "exact projection durability is unsupported on this platform",
    ))
}

fn open_projection_root_nofollow(root: &Path) -> io::Result<Dir> {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "graph root is not UTF-8"))?;
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent)?;
    let parent = Dir::open_ambient_dir(parent, ambient_authority())?;
    open_projection_dir_nofollow(&parent, name)
}

fn projection_real_directory(dir: &Dir, name: &str) -> io::Result<()> {
    let metadata = dir.symlink_metadata(name)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "projection path contains a symlink, reparse point, or special parent",
        ));
    }
    #[cfg(windows)]
    {
        use cap_fs_ext::OsMetadataExt as _;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "projection parent is a reparse point",
            ));
        }
    }
    Ok(())
}

fn projection_optional_regular_metadata(dir: &Dir, name: &str) -> io::Result<()> {
    let metadata = match dir.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "projection target is a symlink, reparse point, or special file",
        ));
    }
    #[cfg(windows)]
    {
        use cap_fs_ext::OsMetadataExt as _;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "projection target is a reparse point",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_projection_dir_nofollow(dir: &Dir, name: &str) -> io::Result<Dir> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};

    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid directory name"))?;
    let fd = unsafe {
        libc::openat(
            dir.as_fd().as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(projection_platform_error(
            "openat(O_DIRECTORY|O_NOFOLLOW) of a projection parent",
            &format!("{name:?}"),
            io::Error::last_os_error(),
        ));
    }
    Ok(Dir::from_std_file(unsafe { fs::File::from_raw_fd(fd) }))
}

#[cfg(windows)]
fn open_projection_dir_nofollow(dir: &Dir, name: &str) -> io::Result<Dir> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _, OpenOptionsMaybeDirExt as _};
    use std::os::windows::fs::MetadataExt;

    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    let file = dir.open_with(name, &options)?.into_std();
    let metadata = file.metadata()?;
    if !metadata.is_dir()
        || metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(DirectSaveError::into_io(
            DirectSaveFailureCode::PrecheckNofollow,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "projection parent is not a real no-follow directory",
            ),
        ));
    }
    Ok(Dir::from_std_file(file))
}

#[cfg(not(any(unix, windows)))]
fn open_projection_dir_nofollow(_dir: &Dir, _name: &str) -> io::Result<Dir> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no-follow projection directories are unsupported on this platform",
    ))
}

#[cfg(unix)]
fn open_projection_file_nofollow(dir: &Dir, name: &str) -> io::Result<fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};

    #[cfg(test)]
    PROJECTION_EXACT_OPEN_COUNT.with(|count| count.set(count.get() + 1));
    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid page filename"))?;
    let fd = unsafe {
        libc::openat(
            dir.as_fd().as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(projection_platform_error(
            "openat(O_NOFOLLOW) of a projection file",
            &format!("{name:?}"),
            io::Error::last_os_error(),
        ));
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "projection target is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_projection_file_nofollow(dir: &Dir, name: &str) -> io::Result<fs::File> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
    use std::os::windows::fs::MetadataExt;

    #[cfg(test)]
    PROJECTION_EXACT_OPEN_COUNT.with(|count| count.set(count.get() + 1));
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = dir.open_with(name, &options)?.into_std();
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "projection target is not a regular no-follow file",
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_projection_file_nofollow(_dir: &Dir, _name: &str) -> io::Result<fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no-follow projection reads are unsupported on this platform",
    ))
}

#[cfg(windows)]
fn open_projection_file_nofollow_for_sync(dir: &Dir, name: &str) -> io::Result<fs::File> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
    use cap_std::fs::OpenOptionsExt as _;
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    #[cfg(test)]
    PROJECTION_EXACT_OPEN_COUNT.with(|count| count.set(count.get() + 1));
    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .write(true)
        .follow(FollowSymlinks::No)
        .access_mode(GENERIC_READ | GENERIC_WRITE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    let file = dir.open_with(name, &options)?.into_std();
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "projection sync target is not a regular no-follow file",
        ));
    }
    Ok(file)
}

fn open_and_read_projection_regular(dir: &Dir, name: &str) -> io::Result<(fs::File, Vec<u8>)> {
    open_and_read_projection_regular_with_limit(dir, name, MAX_PROJECTION_EVIDENCE_BYTES)
}

fn open_and_read_projection_regular_with_limit(
    dir: &Dir,
    name: &str,
    limit: u64,
) -> io::Result<(fs::File, Vec<u8>)> {
    let file = open_projection_file_nofollow(dir, name)?;
    read_open_projection_regular_with_limit(file, limit)
}

fn read_open_projection_regular_with_limit(
    mut file: fs::File,
    limit: u64,
) -> io::Result<(fs::File, Vec<u8>)> {
    let len = file.metadata()?.len();
    if len > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence exceeds the reload bound",
        ));
    }
    bounded_read_after_metadata_hook()?;
    let capacity = usize::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence length is not addressable",
        )
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    let read_limit = limit.checked_add(1).ok_or_else(allocation_overflow)?;
    (&mut file).take(read_limit).read_to_end(&mut bytes)?;
    if usize_to_u64(bytes.len())? > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence grew beyond the reload bound",
        ));
    }
    Ok((file, bytes))
}

/// Read a graph-text body while reserving each retained byte before it enters the
/// returned vector. The chunked path closes the metadata/read growth gap: a
/// file that grows after metadata cannot make the preparation allocation exceed
/// the aggregate budget before it is rejected.
fn open_and_read_projection_regular_with_budget(
    dir: &Dir,
    name: &str,
    limit: u64,
    budget: &RetainedContentBudget,
    resource: &'static str,
) -> io::Result<(fs::File, Vec<u8>, RetainedContentReservation)> {
    let mut file = open_projection_file_nofollow(dir, name)?;
    let len = file.metadata()?.len();
    if len > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence exceeds the reload bound",
        ));
    }
    let reservation = budget.reserve(len, resource)?;
    bounded_read_after_metadata_hook()?;
    let capacity = usize::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence length is not addressable",
        )
    })?;
    // Allocate the metadata-sized buffer once. A shrink leaves this capacity
    // charged; growth is detected with a stack byte and rejected without asking
    // Vec to grow outside admission.
    let mut bytes = vec![0_u8; capacity].into_boxed_slice().into_vec();
    assert_eq!(
        bytes.capacity(),
        capacity,
        "boxed bounded read must transfer exact retained capacity"
    );
    let mut filled = 0usize;
    while filled < bytes.len() {
        let read = file.read(&mut bytes[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    bytes.truncate(filled);
    let mut growth_probe = [0_u8; 1];
    if file.read(&mut growth_probe)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence grew after its bounded allocation",
        ));
    }
    Ok((file, bytes, reservation))
}

/// Read one projection-evidence file's bytes under the evidence bound.
fn read_projection_regular(dir: &Dir, name: &str) -> io::Result<Vec<u8>> {
    open_and_read_projection_regular(dir, name).map(|(_, bytes)| bytes)
}

fn read_projection_optional(dir: &Dir, name: &str) -> io::Result<Option<Vec<u8>>> {
    match open_and_read_projection_regular(dir, name) {
        Ok((_file, bytes)) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_projection_optional_bound_capture_with_limits(
    dir: &Dir,
    name: &str,
    content_limit: u64,
    peak_limit: u64,
) -> io::Result<Option<(Vec<u8>, BlobDescription, ContentDigest, u64, u64)>> {
    read_projection_optional_bound_capture_impl(dir, name, Some((content_limit, peak_limit)))
}

// How many graph text documents this thread has physically opened and read
// through the one projection capture primitive.
//
// Deliberately NOT test-only. It is what lets the clean watcher publish the
// document-read cost of its slowest full-scan turn, which is the property the
// bounded full scan exists to hold — and an architectural claim of that kind
// has to be observable in production, not asserted in a comment. A thread-local
// increment is free next to the open + read + SHA-256 it counts.
thread_local! {
    static GRAPH_TEXT_CAPTURE_READS: Cell<usize> = const { Cell::new(0) };
}

fn count_graph_text_capture_read() {
    GRAPH_TEXT_CAPTURE_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
}

#[cfg(test)]
pub(crate) fn graph_text_capture_reads() -> usize {
    GRAPH_TEXT_CAPTURE_READS.with(Cell::get)
}

fn read_projection_optional_bound_capture_impl(
    dir: &Dir,
    name: &str,
    limits: Option<(u64, u64)>,
) -> io::Result<Option<(Vec<u8>, BlobDescription, ContentDigest, u64, u64)>> {
    count_graph_text_capture_read();
    let rebound_limit = limits
        .map(|(content_limit, _)| content_limit)
        .unwrap_or(MAX_PROJECTION_EVIDENCE_BYTES);
    let opened = match limits {
        Some((content_limit, peak_limit)) => {
            open_and_read_projection_regular_exact_bound(dir, name, content_limit, peak_limit)
        }
        None => open_and_read_projection_regular(dir, name),
    };
    let (opened, bytes) = match opened {
        Ok(result) => result,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            graph_text_inventory_read_hook()?;
            return match dir.symlink_metadata(name) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "graph target appeared during absence capture",
                )),
                Err(error) => Err(error),
            };
        }
        Err(error) => return Err(error),
    };
    graph_text_inventory_read_hook()?;

    let mut rebound = open_projection_file_nofollow(dir, name)?;
    if !projection_files_have_same_identity(&opened, &rebound)? {
        return Err(DirectSaveError::into_io(
            DirectSaveFailureCode::PrecheckInterrupted,
            io::Error::new(
                io::ErrorKind::Interrupted,
                "graph target was replaced or changed during capture",
            ),
        ));
    }
    let expected = BlobDescription::of(&bytes);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut rebound_bytes = 0_u64;
    loop {
        let read = rebound.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        rebound_bytes = rebound_bytes
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "read-byte overflow"))?;
        if rebound_bytes > rebound_limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph target grew beyond the capture bound",
            ));
        }
        hasher.update(&buffer[..read]);
    }
    let rebound_description = BlobDescription::from_parts(hasher.finalize().into(), rebound_bytes);
    if rebound_description != expected {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "graph target changed while its retained binding was validated",
        ));
    }
    let file_resource_id = canonical_projection_file_resource_id(&opened)?;
    let peak_capture_buffer_bytes =
        checked_add_bytes(usize_to_u64(bytes.capacity())?, usize_to_u64(buffer.len())?)?;
    let validation_bytes = checked_add_bytes(expected.byte_length(), rebound_bytes)?;
    Ok(Some((
        bytes,
        expected,
        file_resource_id,
        validation_bytes,
        peak_capture_buffer_bytes,
    )))
}

fn open_and_read_projection_regular_exact_bound(
    dir: &Dir,
    name: &str,
    content_limit: u64,
    peak_limit: u64,
) -> io::Result<(fs::File, Vec<u8>)> {
    let mut file = open_projection_file_nofollow(dir, name)?;
    let len = file.metadata()?.len();
    if len > content_limit {
        return Err(graph_text_capture_limit_error("aggregate raw bytes"));
    }
    let allocation_peak = checked_add_bytes(len, 16 * 1024)?;
    if allocation_peak > peak_limit {
        return Err(graph_text_capture_limit_error("peak build memory"));
    }
    bounded_read_after_metadata_hook()?;
    let capacity = usize::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence length is not addressable",
        )
    })?;
    let mut bytes = vec![0_u8; capacity].into_boxed_slice().into_vec();
    assert_eq!(
        bytes.capacity(),
        capacity,
        "boxed bounded read must transfer exact retained capacity"
    );
    let mut filled = 0usize;
    while filled < bytes.len() {
        let read = file.read(&mut bytes[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    bytes.truncate(filled);
    let mut growth_probe = [0_u8; 1];
    if file.read(&mut growth_probe)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence grew after its bounded allocation",
        ));
    }
    Ok((file, bytes))
}

const MAX_GRAPH_TEXT_CAPTURE_FILES: usize = 1_000_000;
const MAX_GRAPH_TEXT_CAPTURE_RAW_BYTES: u64 = 512 * 1024 * 1024;
const MAX_GRAPH_TEXT_CAPTURE_DIRECTORY_DEPTH: usize = 256;
const MAX_GRAPH_TEXT_CAPTURE_ALL_ENTRIES: usize = 2_000_000;
const MAX_GRAPH_TEXT_CAPTURE_DIRECTORIES: usize = 1_000_000;
const MAX_GRAPH_TEXT_CAPTURE_PENDING_DIRECTORIES: usize = 1_000_000;
const MAX_GRAPH_TEXT_CAPTURE_PATH_BYTES: u64 = 512 * 1024 * 1024;
const MAX_GRAPH_TEXT_ADMISSION_INDEX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_GRAPH_TEXT_ADMISSION_BUILD_PEAK_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_GRAPH_TEXT_EXACT_FEED_BATCH_RAW_BYTES: u64 = 64 * 1024 * 1024;
/// Peak content retained while mutable graph-text preparation has both parsed
/// and raw/projection representations alive. This matches the 512 MiB initial
/// shadow raw-byte ceiling, while accounting for those simultaneous copies.
const MAX_GRAPH_TEXT_RETAINED_CONTENT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy)]
struct GraphTextCaptureLimits {
    graph_text_files: usize,
    raw_bytes: u64,
    directory_depth: usize,
    all_entries: usize,
    directories: usize,
    pending_directories: usize,
    path_bytes: u64,
    permanent_index_bytes: u64,
    peak_build_bytes: u64,
}

const GRAPH_TEXT_CAPTURE_LIMITS: GraphTextCaptureLimits = GraphTextCaptureLimits {
    graph_text_files: MAX_GRAPH_TEXT_CAPTURE_FILES,
    raw_bytes: MAX_GRAPH_TEXT_CAPTURE_RAW_BYTES,
    directory_depth: MAX_GRAPH_TEXT_CAPTURE_DIRECTORY_DEPTH,
    all_entries: MAX_GRAPH_TEXT_CAPTURE_ALL_ENTRIES,
    directories: MAX_GRAPH_TEXT_CAPTURE_DIRECTORIES,
    pending_directories: MAX_GRAPH_TEXT_CAPTURE_PENDING_DIRECTORIES,
    path_bytes: MAX_GRAPH_TEXT_CAPTURE_PATH_BYTES,
    permanent_index_bytes: MAX_GRAPH_TEXT_ADMISSION_INDEX_BYTES,
    peak_build_bytes: MAX_GRAPH_TEXT_ADMISSION_BUILD_PEAK_BYTES,
};

/// Bounds for mutable graph-text inventories. These deliberately reuse the
/// capture limits so a mutable inventory cannot be driven beyond the memory
/// and traversal envelope already accepted for the initial capture.
#[derive(Clone, Copy)]
struct GraphTextInventoryLimits {
    graph_text_files: usize,
    directory_depth: usize,
    all_entries: usize,
    directories: usize,
    pending_directories: usize,
    path_bytes: u64,
    retained_content_bytes: u64,
}

const GRAPH_TEXT_INVENTORY_LIMITS: GraphTextInventoryLimits = GraphTextInventoryLimits {
    graph_text_files: MAX_GRAPH_TEXT_CAPTURE_FILES,
    directory_depth: MAX_GRAPH_TEXT_CAPTURE_DIRECTORY_DEPTH,
    all_entries: MAX_GRAPH_TEXT_CAPTURE_ALL_ENTRIES,
    directories: MAX_GRAPH_TEXT_CAPTURE_DIRECTORIES,
    pending_directories: MAX_GRAPH_TEXT_CAPTURE_PENDING_DIRECTORIES,
    path_bytes: MAX_GRAPH_TEXT_CAPTURE_PATH_BYTES,
    retained_content_bytes: MAX_GRAPH_TEXT_RETAINED_CONTENT_BYTES,
};

/// A single preparation budget spans mutable inventory consumers. Every
/// reservation is atomic and RAII-owned: a failed admission leaves the counter
/// unchanged, and every success is released exactly once when its token drops.
///
/// Tokens charge requested/retained capacities, not logical string lengths.
/// Construction sites first reserve a source-derived upper bound for their
/// temporary allocations, then reconcile the token to the capacity-aware size
/// of the retained value.
#[derive(Clone)]
struct RetainedContentBudget {
    state: Rc<RetainedContentBudgetState>,
}

#[derive(Debug)]
struct RetainedContentBudgetState {
    limit: u64,
    retained: Cell<u64>,
    #[cfg(test)]
    peak: Cell<u64>,
}

#[derive(Debug)]
struct RetainedContentReservation {
    state: Rc<RetainedContentBudgetState>,
    bytes: u64,
}

impl RetainedContentBudget {
    fn new(limits: GraphTextInventoryLimits) -> Self {
        #[cfg(test)]
        GRAPH_TEXT_BUDGET_LAST_PEAK.with(|peak| peak.set(0));
        Self {
            state: Rc::new(RetainedContentBudgetState {
                limit: limits.retained_content_bytes,
                retained: Cell::new(0),
                #[cfg(test)]
                peak: Cell::new(0),
            }),
        }
    }

    fn reserve(
        &self,
        bytes: u64,
        _resource: &'static str,
    ) -> io::Result<RetainedContentReservation> {
        let candidate = self
            .state
            .retained
            .get()
            .checked_add(bytes)
            .ok_or_else(|| graph_text_inventory_limit_error("aggregate retained content bytes"))?;
        if candidate > self.state.limit {
            return Err(graph_text_inventory_limit_error(
                "aggregate retained content bytes",
            ));
        }
        self.state.retained.set(candidate);
        #[cfg(test)]
        self.state.peak.set(self.state.peak.get().max(candidate));
        Ok(RetainedContentReservation {
            state: Rc::clone(&self.state),
            bytes,
        })
    }

    #[cfg(test)]
    fn retained(&self) -> u64 {
        self.state.retained.get()
    }
}

#[cfg(test)]
impl Drop for RetainedContentBudget {
    fn drop(&mut self) {
        GRAPH_TEXT_BUDGET_LAST_PEAK.with(|peak| peak.set(self.state.peak.get()));
    }
}

impl RetainedContentReservation {
    fn resize(&mut self, bytes: u64, resource: &'static str) -> io::Result<()> {
        if bytes > self.bytes {
            let increase = bytes - self.bytes;
            let candidate = self
                .state
                .retained
                .get()
                .checked_add(increase)
                .ok_or_else(|| {
                    graph_text_inventory_limit_error("aggregate retained content bytes")
                })?;
            if candidate > self.state.limit {
                return Err(graph_text_inventory_limit_error(
                    "aggregate retained content bytes",
                ));
            }
            self.state.retained.set(candidate);
            #[cfg(test)]
            self.state.peak.set(self.state.peak.get().max(candidate));
        } else {
            let decrease = self.bytes - bytes;
            let retained = self.state.retained.get();
            assert!(
                retained >= decrease,
                "released unreserved graph content for {resource}"
            );
            self.state.retained.set(retained - decrease);
        }
        self.bytes = bytes;
        Ok(())
    }
}

impl Drop for RetainedContentReservation {
    fn drop(&mut self) {
        let retained = self.state.retained.get();
        assert!(
            retained >= self.bytes,
            "double release of graph content reservation"
        );
        self.state.retained.set(
            retained
                .checked_sub(self.bytes)
                .expect("reservation release was range-checked"),
        );
    }
}

struct BudgetedString {
    value: String,
    reservation: RetainedContentReservation,
}

struct BudgetedPageEntries {
    entries: Vec<PageEntry>,
    _reservation: RetainedContentReservation,
}

impl std::ops::Deref for BudgetedPageEntries {
    type Target = [PageEntry];

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

struct RetainedHeapCharge {
    reservation: Option<RetainedContentReservation>,
    bytes: u64,
}

impl RetainedHeapCharge {
    fn new(budget: Option<&RetainedContentBudget>, resource: &'static str) -> io::Result<Self> {
        Ok(Self {
            reservation: budget
                .map(|budget| budget.reserve(0, resource))
                .transpose()?,
            bytes: 0,
        })
    }

    fn grow(&mut self, bytes: u64, resource: &'static str) -> io::Result<()> {
        let next = checked_add_bytes(self.bytes, bytes)?;
        if let Some(reservation) = self.reservation.as_mut() {
            reservation.resize(next, resource)?;
        }
        self.bytes = next;
        Ok(())
    }

    fn shrink(&mut self, bytes: u64, resource: &'static str) -> io::Result<()> {
        let next = self
            .bytes
            .checked_sub(bytes)
            .expect("retained heap charge releases only admitted bytes");
        if let Some(reservation) = self.reservation.as_mut() {
            reservation.resize(next, resource)?;
        }
        self.bytes = next;
        Ok(())
    }
}

impl std::ops::Deref for BudgetedString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl AsRef<str> for BudgetedString {
    fn as_ref(&self) -> &str {
        &self.value
    }
}

fn allocation_overflow() -> io::Error {
    graph_text_inventory_limit_error("aggregate retained content bytes")
}

fn checked_add_bytes(left: u64, right: u64) -> io::Result<u64> {
    left.checked_add(right).ok_or_else(allocation_overflow)
}

fn checked_mul_bytes(left: u64, right: u64) -> io::Result<u64> {
    left.checked_mul(right).ok_or_else(allocation_overflow)
}

fn conservative_vec_entry_bytes<T>() -> io::Result<u64> {
    checked_add_bytes(
        checked_mul_bytes(usize_to_u64(std::mem::size_of::<T>())?, 2)?,
        16,
    )
}

fn conservative_vec_capacity_upper_bound<T>(entries: u64) -> io::Result<u64> {
    checked_mul_bytes(entries, conservative_vec_entry_bytes::<T>()?)
}

fn conservative_hash_entry_bytes<K, V>() -> io::Result<u64> {
    checked_add_bytes(
        checked_mul_bytes(
            checked_add_bytes(
                usize_to_u64(std::mem::size_of::<K>())?,
                usize_to_u64(std::mem::size_of::<V>())?,
            )?,
            4,
        )?,
        64,
    )
}

fn conservative_btree_entry_bytes<K, V>() -> io::Result<u64> {
    checked_add_bytes(
        checked_mul_bytes(
            checked_add_bytes(
                usize_to_u64(std::mem::size_of::<K>())?,
                usize_to_u64(std::mem::size_of::<V>())?,
            )?,
            2,
        )?,
        256,
    )
}

fn owned_string_upper_bound(value: &str) -> io::Result<u64> {
    owned_string_len_upper_bound(u64::try_from(value.len()).map_err(|_| allocation_overflow())?)
}

fn owned_string_len_upper_bound(len: u64) -> io::Result<u64> {
    checked_add_bytes(
        conservative_vec_capacity_upper_bound::<u8>(len)?,
        usize_to_u64(std::mem::size_of::<String>())?,
    )
}

fn owned_path_upper_bound(value: &Path) -> io::Result<u64> {
    owned_string_len_upper_bound(
        u64::try_from(value.as_os_str().len()).map_err(|_| allocation_overflow())?,
    )
}

fn page_entry_clone_upper_bound(entry: &PageEntry) -> io::Result<u64> {
    let mut bytes = usize_to_u64(std::mem::size_of::<PageEntry>())?;
    bytes = checked_add_bytes(bytes, owned_string_upper_bound(&entry.name)?)?;
    bytes = checked_add_bytes(bytes, owned_string_upper_bound(&entry.rel_path)?)?;
    checked_add_bytes(bytes, owned_path_upper_bound(&entry.path)?)
}

fn graph_text_page_entry_retained_upper_bound(entry: &PageEntry) -> io::Result<u64> {
    let mut bytes = usize_to_u64(std::mem::size_of::<PageEntry>())?;
    bytes = checked_add_bytes(
        bytes,
        checked_add_bytes(
            usize_to_u64(std::mem::size_of::<String>())?,
            usize_to_u64(entry.name.capacity())?,
        )?,
    )?;
    bytes = checked_add_bytes(bytes, owned_string_upper_bound(&entry.rel_path)?)?;
    checked_add_bytes(bytes, owned_path_upper_bound(&entry.path)?)
}

fn usize_to_u64(value: usize) -> io::Result<u64> {
    u64::try_from(value).map_err(|_| allocation_overflow())
}

/// Page input is accepted only through depth 128. All operation-time
/// nested walks use this fixed root-to-leaf frame ceiling, so traversal does
/// not consume attacker-controlled call stack or an uncharged all-node stack.
pub(crate) const MAX_BLOCK_DEPTH: usize = 128;

#[derive(Clone, Copy)]
#[cfg(test)]
struct BlockDtoWalkFrame<'a> {
    blocks: &'a [BlockDto],
    next: usize,
    depth: usize,
}

#[cfg(test)]
struct BlockDtoWalk<'a> {
    frames: [BlockDtoWalkFrame<'a>; MAX_BLOCK_DEPTH],
    len: usize,
}

#[cfg(test)]
impl<'a> BlockDtoWalk<'a> {
    fn new(blocks: &'a [BlockDto]) -> Self {
        let empty = BlockDtoWalkFrame {
            blocks: &[],
            next: 0,
            depth: 0,
        };
        let mut frames = [empty; MAX_BLOCK_DEPTH];
        let len = usize::from(!blocks.is_empty());
        if len != 0 {
            frames[0] = BlockDtoWalkFrame {
                blocks,
                next: 0,
                depth: 1,
            };
        }
        Self { frames, len }
    }

    fn next(&mut self) -> io::Result<Option<(&'a BlockDto, usize)>> {
        loop {
            if self.len == 0 {
                return Ok(None);
            }
            let frame = &mut self.frames[self.len - 1];
            if frame.next == frame.blocks.len() {
                self.len -= 1;
                continue;
            }
            let block = &frame.blocks[frame.next];
            frame.next = frame.next.checked_add(1).ok_or_else(allocation_overflow)?;
            let depth = frame.depth;
            if !block.children.is_empty() {
                if self.len == MAX_BLOCK_DEPTH {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "graph page block nesting exceeds 128 levels",
                    ));
                }
                self.frames[self.len] = BlockDtoWalkFrame {
                    blocks: &block.children,
                    next: 0,
                    depth: depth.checked_add(1).ok_or_else(allocation_overflow)?,
                };
                self.len += 1;
            }
            return Ok(Some((block, depth)));
        }
    }
}

fn rename_rewrite_upper_bound(
    content: &str,
    renames: &std::collections::HashMap<String, String>,
    encode_org_file_links: bool,
) -> io::Result<u64> {
    let raw_max_name = usize_to_u64(renames.values().map(String::len).max().unwrap_or(0))?;
    // A reserved-only title can grow to three bytes per input byte in an encoded
    // Org file-link stem. Ordinary refs/tags and the second tags:: pass stay raw.
    let max_name = if encode_org_file_links && content.contains("[[file:") {
        checked_mul_bytes(raw_max_name, 3)?
    } else {
        raw_max_name
    };
    let candidates = checked_add_bytes(
        usize_to_u64(
            content
                .bytes()
                .filter(|byte| matches!(*byte, b'#' | b'[' | b','))
                .count(),
        )?,
        usize_to_u64(if content.contains("::") {
            content
                .lines()
                .filter(|line| {
                    line.as_bytes()
                        .windows(6)
                        .any(|window| window.eq_ignore_ascii_case(b"tags::"))
                })
                .count()
        } else {
            0
        })?,
    )?;
    let replacement_growth = checked_mul_bytes(candidates, checked_add_bytes(max_name, 8)?)?;
    let code_delimiters = usize_to_u64(
        content
            .bytes()
            .filter(|byte| matches!(*byte, b'`' | b'~'))
            .count(),
    )?;
    let code_ranges = checked_mul_bytes(
        code_delimiters,
        checked_mul_bytes(4, usize_to_u64(std::mem::size_of::<usize>())?)?,
    )?;
    let segment_headers = checked_mul_bytes(
        candidates,
        checked_mul_bytes(2, usize_to_u64(std::mem::size_of::<String>())?)?,
    )?;
    let content_len = usize_to_u64(content.len())?;
    let scanner_scratch = checked_mul_bytes(content_len, 3)?;
    checked_add_bytes(
        checked_add_bytes(
            checked_add_bytes(content_len, replacement_growth)?,
            code_ranges,
        )?,
        checked_add_bytes(segment_headers, scanner_scratch)?,
    )
}

/// What entitles a save to write the file its page is pinned to.
enum PinnedSaveAuthority<'a> {
    /// An ordinary editor save. Proves exact path ownership here and NOTHING
    /// about physical identity, because the identity decision belongs after the
    /// byte comparison, not before it.
    ///
    /// Ordinary frontend saves carry the loaded revision in the separate
    /// `base_rev` argument; `PageDto.rev` is NOT part of the working-store DTO
    /// that `pageToDto` builds, so it must never be read as one.
    ///
    /// Refusing a changed inode up front (as an earlier journal projection
    /// did) would pre-empt the base-revision check and turn every
    /// rename-based external write (Syncthing, Dropbox, Logseq OG, VS Code, any
    /// temp+rename tool) into a permanent
    /// `path-pinned page does not match its captured exact owner`. The frontend
    /// classifies that as transient and retries it forever, so the page becomes
    /// silently unsaveable — GH #254.
    ///
    /// The byte comparison below is the stronger proof anyway: `base_rev` is
    /// SHA-256 of the exact bytes the editor loaded, and under the storage threat
    /// model (`specs/notes/2026-08-07-trust-model-and-threat-model-decision.md`)
    /// a byte-forging adversary is out of scope. Equal bytes therefore mean the
    /// same state regardless of which inode carries them.
    OrdinaryEditorSave {
        loaded_revision: Option<&'a str>,
        prospective_editor: bool,
    },
    /// The user was shown the conflict and chose to keep their own edits.
    ///
    /// A stale revision and a stale identity ARE the conflict being resolved —
    /// requiring either to match makes "keep mine" refuse exactly when it is
    /// needed, leaving discard-my-work as the only exit the app offers. The pin
    /// must still resolve to a retained file owner at the validated path, so an
    /// override cannot be redirected onto a file this page never came from.
    UserOverride(&'a ConflictSnapshot),
}

struct ExactGraphLoadedPage {
    entry: PageEntry,
    document: Document,
    content: String,
    revision: String,
    file_identity: ContentDigest,
}

struct ExactGraphValidation {
    target: Option<ExactGraphLoadedPage>,
    requested_identity_elsewhere: bool,
    creation_proof: Option<DirectCreationProof>,
}

#[cfg(not(test))]
fn graph_text_inventory_limits() -> GraphTextInventoryLimits {
    GRAPH_TEXT_INVENTORY_LIMITS
}

#[cfg(test)]
fn graph_text_inventory_limits() -> GraphTextInventoryLimits {
    GRAPH_TEXT_INVENTORY_LIMITS_OVERRIDE.with(|override_limits| {
        override_limits
            .borrow()
            .unwrap_or(GRAPH_TEXT_INVENTORY_LIMITS)
    })
}

struct GraphTextCaptureEntry {
    path: GraphTextPath,
    bytes: Option<Vec<u8>>,
    description: BlobDescription,
    file_resource_id: ContentDigest,
    link_count: u64,
}

struct GraphTextCapture {
    entries: Vec<GraphTextCaptureEntry>,
    directories_by_exact_relative: std::collections::BTreeMap<String, ContentDigest>,
    paths_by_file_resource:
        std::collections::BTreeMap<ContentDigest, std::collections::BTreeSet<String>>,
    file_link_count_by_exact_relative: std::collections::BTreeMap<String, u64>,
    all_entries: u64,
    raw_bytes: u64,
    peak_build_charge: u64,
}

fn collect_graph_text_capture_inner(
    graph: &Graph,
    _permit: &GraphTextWritePermit,
    retain_bytes: bool,
    limits: GraphTextCaptureLimits,
    simultaneous_capture_bytes: u64,
    require_ambient_binding: bool,
    skip_symlinks: bool,
) -> io::Result<GraphTextCapture> {
    struct PendingDirectory {
        directory: Dir,
        relative: String,
        depth: usize,
    }

    let mut entries = Vec::new();
    let mut raw_bytes = 0_u64;
    let mut all_entries = 0_usize;
    let mut directory_count = 1_usize;
    let mut path_bytes = 0_u64;
    let mut peak_build_charge = graph_text_root_capture_upper_bound()?;
    let mut directories_by_exact_relative = std::collections::BTreeMap::new();
    let mut directory_resources = std::collections::BTreeMap::new();
    let mut paths_by_file_resource =
        std::collections::BTreeMap::<ContentDigest, std::collections::BTreeSet<String>>::new();
    let mut file_link_count_by_exact_relative = std::collections::BTreeMap::new();
    let mut pending = Vec::new();
    ensure_graph_text_peak_limit(
        simultaneous_capture_bytes,
        peak_build_charge,
        limits.peak_build_bytes,
    )?;
    if directory_count > limits.directories {
        return Err(graph_text_capture_limit_error("directory count"));
    }
    if require_ambient_binding {
        graph.ensure_projection_root_binding()?;
    }
    let directory = if require_ambient_binding {
        graph
            .projection_root
            .as_ref()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "graph has no retained no-follow projection capability",
                )
            })?
            .try_clone()?
    } else {
        graph.graph_text_write_binding()?.root.try_clone()?
    };
    let root_resource = canonical_projection_directory_resource_id(&directory)?;
    directories_by_exact_relative.insert(String::new(), root_resource);
    directory_resources.insert(root_resource, String::new());
    if pending.len() == limits.pending_directories {
        return Err(graph_text_capture_limit_error("pending directories"));
    }
    pending.push(PendingDirectory {
        directory,
        relative: String::new(),
        depth: 0,
    });

    while let Some(PendingDirectory {
        directory,
        relative,
        depth,
    }) = pending.pop()
    {
        count_graph_text_admission_builder_enumeration();
        for entry in directory.entries()? {
            all_entries = all_entries
                .checked_add(1)
                .ok_or_else(|| graph_text_capture_limit_error("all directory entries"))?;
            if all_entries > limits.all_entries {
                return Err(graph_text_capture_limit_error("all directory entries"));
            }
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph text entry name is not UTF-8",
                )
            })?;
            let relative_len = relative
                .len()
                .checked_add(usize::from(!relative.is_empty()))
                .and_then(|length| length.checked_add(name.len()))
                .ok_or_else(allocation_overflow)?;
            path_bytes = path_bytes
                .checked_add(
                    usize_to_u64(relative_len)
                        .map_err(|_| graph_text_capture_limit_error("aggregate path bytes"))?,
                )
                .ok_or_else(|| graph_text_capture_limit_error("aggregate path bytes"))?;
            if path_bytes > limits.path_bytes {
                return Err(graph_text_capture_limit_error("aggregate path bytes"));
            }
            grow_graph_text_capture_charge(
                &mut peak_build_charge,
                graph_text_discovered_path_upper_bound(usize_to_u64(relative_len)?)?,
                simultaneous_capture_bytes,
                limits.peak_build_bytes,
            )?;
            let child_relative = if relative.is_empty() {
                name.to_owned()
            } else {
                format!("{relative}/{name}")
            };
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                if skip_symlinks
                    || (!graph.graph_text_scope.should_descend(&child_relative)
                        && !graph.graph_text_scope.is_eligible(&child_relative))
                {
                    // GH #267 / F3. A symlink anywhere in a descended scope used
                    // to abort this capture, and because the save path takes the
                    // capture to answer a filename question, that made the whole
                    // graph permanently unsaveable -- one symlink in `pages/`
                    // and none of the user's OTHER pages could be written.
                    //
                    // Skipping is what the rest of Tine already does: every
                    // traversal here is no-follow, the watcher's snapshot walk
                    // never descends a symlinked directory, and
                    // `graph_inventory_entry` never admits a symlinked file. A
                    // symlink is not a graph-text document on any other path, so
                    // the capture stops being the one place that escalates it to
                    // an error that costs the user their other pages.
                    //
                    // `skip_symlinks` is false for the shadow-import bootstrap,
                    // which still refuses: importing a graph must not silently
                    // leave out a file the user considers part of it.
                    continue;
                }
                return Err(DirectSaveError::into_io(
                    DirectSaveFailureCode::PrecheckSymlink,
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("graph text entry is a symlink or reparse point: {child_relative}"),
                    ),
                ));
            }
            if file_type.is_dir() {
                if !graph.graph_text_scope.should_descend(&child_relative) {
                    continue;
                }
                let child_depth = depth
                    .checked_add(1)
                    .ok_or_else(|| graph_text_capture_limit_error("graph directory depth"))?;
                if child_depth > limits.directory_depth {
                    return Err(graph_text_capture_limit_error("graph directory depth"));
                }
                directory_count = directory_count
                    .checked_add(1)
                    .ok_or_else(|| graph_text_capture_limit_error("directory count"))?;
                if directory_count > limits.directories {
                    return Err(graph_text_capture_limit_error("directory count"));
                }
                projection_real_directory(&directory, name)?;
                let child = open_projection_dir_nofollow(&directory, name)?;
                let resource = canonical_projection_directory_resource_id(&child)?;
                if let Some(first) = directory_resources.insert(resource, child_relative.clone()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "graph directories alias one resource: {first} and {child_relative}"
                        ),
                    ));
                }
                directories_by_exact_relative.insert(child_relative.clone(), resource);
                let rebound = open_projection_dir_nofollow(&directory, name)?;
                if projection_dir_identity(&child)? != projection_dir_identity(&rebound)? {
                    return Err(DirectSaveError::into_io(
                        DirectSaveFailureCode::PrecheckInterrupted,
                        io::Error::new(
                            io::ErrorKind::Interrupted,
                            format!("graph directory changed during capture: {child_relative}"),
                        ),
                    ));
                }
                if pending.len() == limits.pending_directories {
                    return Err(graph_text_capture_limit_error("pending directories"));
                }
                pending.push(PendingDirectory {
                    directory: child,
                    relative: child_relative,
                    depth: child_depth,
                });
                continue;
            }
            if !file_type.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("graph text entry is not a regular file: {child_relative}"),
                ));
            }
            let file = open_projection_file_nofollow(&directory, name)?;
            let file_resource = canonical_projection_file_resource_id(&file)?;
            let link_count = projection_file_link_count(&file)?;
            paths_by_file_resource
                .entry(file_resource)
                .or_default()
                .insert(child_relative.clone());
            file_link_count_by_exact_relative.insert(child_relative.clone(), link_count);
            if !graph.graph_text_scope.is_eligible(&child_relative) {
                continue;
            }
            if entries.len() == limits.graph_text_files {
                return Err(graph_text_capture_limit_error("graph file count"));
            }
            let path = GraphTextPath::parse(child_relative)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
            let remaining_raw = limits
                .raw_bytes
                .checked_sub(raw_bytes)
                .ok_or_else(|| graph_text_capture_limit_error("aggregate raw bytes"))?;
            let live_capture_bytes =
                checked_add_bytes(simultaneous_capture_bytes, peak_build_charge)?;
            let remaining_peak = limits
                .peak_build_bytes
                .checked_sub(live_capture_bytes)
                .ok_or_else(|| graph_text_capture_limit_error("peak build memory"))?;
            let (bytes, description, captured_resource, _, _) =
                read_projection_optional_bound_capture_with_limits(
                    &directory,
                    name,
                    remaining_raw,
                    remaining_peak,
                )?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Interrupted,
                        format!("graph entry disappeared during capture: {path}"),
                    )
                })?;
            if retain_bytes {
                grow_graph_text_capture_charge(
                    &mut peak_build_charge,
                    usize_to_u64(bytes.capacity())?,
                    simultaneous_capture_bytes,
                    limits.peak_build_bytes,
                )?;
            }
            if captured_resource != file_resource {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("graph entry changed after enumeration: {path}"),
                ));
            }
            raw_bytes = raw_bytes
                .checked_add(usize_to_u64(bytes.len())?)
                .ok_or_else(|| graph_text_capture_limit_error("aggregate raw bytes"))?;
            if raw_bytes > limits.raw_bytes {
                return Err(graph_text_capture_limit_error("aggregate raw bytes"));
            }
            entries.push(GraphTextCaptureEntry {
                path,
                description,
                bytes: retain_bytes.then_some(bytes),
                file_resource_id: captured_resource,
                link_count,
            });
        }
    }

    entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    if entries
        .windows(2)
        .any(|window| window[0].path == window[1].path)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "graph-text capture contains duplicate graph paths",
        ));
    }
    Ok(GraphTextCapture {
        entries,
        directories_by_exact_relative,
        paths_by_file_resource,
        file_link_count_by_exact_relative,
        all_entries: all_entries as u64,
        raw_bytes,
        peak_build_charge: {
            #[cfg(test)]
            {
                if retain_bytes {
                    GRAPH_TEXT_FIRST_CAPTURE_CHARGE_OVERRIDE
                        .with(|override_charge| override_charge.take())
                        .unwrap_or(peak_build_charge)
                } else {
                    peak_build_charge
                }
            }
            #[cfg(not(test))]
            {
                peak_build_charge
            }
        },
    })
}

fn ensure_graph_text_peak_limit(base: u64, additional: u64, limit: u64) -> io::Result<()> {
    if checked_add_bytes(base, additional)? > limit {
        return Err(graph_text_capture_limit_error("peak build memory"));
    }
    Ok(())
}

fn grow_graph_text_capture_charge(
    charge: &mut u64,
    growth: u64,
    simultaneous_capture_bytes: u64,
    peak_limit: u64,
) -> io::Result<()> {
    let next = checked_add_bytes(*charge, growth)?;
    ensure_graph_text_peak_limit(simultaneous_capture_bytes, next, peak_limit)?;
    *charge = next;
    Ok(())
}

fn graph_text_discovered_path_upper_bound(relative_len: u64) -> io::Result<u64> {
    let owned_path = owned_string_len_upper_bound(relative_len)?;
    let mut bytes = checked_add_bytes(
        conservative_vec_entry_bytes::<GraphTextCaptureEntry>()?,
        checked_add_bytes(
            conservative_vec_entry_bytes::<(Dir, String, usize)>()?,
            owned_path,
        )?,
    )?;
    // Charge the maximum simultaneous directory and file bookkeeping for every
    // discovered name. This intentionally over-reserves rows that are used by
    // only one branch so no branch can allocate before admission.
    for row in [
        conservative_btree_entry_bytes::<String, ContentDigest>()?,
        conservative_btree_entry_bytes::<ContentDigest, String>()?,
        conservative_btree_entry_bytes::<ContentDigest, std::collections::BTreeSet<String>>()?,
        conservative_btree_entry_bytes::<String, ()>()?,
        conservative_btree_entry_bytes::<String, u64>()?,
    ] {
        bytes = checked_add_bytes(bytes, row)?;
        bytes = checked_add_bytes(bytes, owned_path)?;
    }
    // Exact path construction plus the transient child-relative string.
    bytes = checked_add_bytes(bytes, checked_mul_bytes(owned_path, 2)?)?;
    checked_add_bytes(bytes, 512)
}

fn graph_text_root_capture_upper_bound() -> io::Result<u64> {
    let empty = owned_string_len_upper_bound(0)?;
    let mut bytes = conservative_vec_entry_bytes::<(Dir, String, usize)>()?;
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<String, ContentDigest>()?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<ContentDigest, String>()?,
    )?;
    bytes = checked_add_bytes(bytes, checked_mul_bytes(empty, 3)?)?;
    checked_add_bytes(bytes, 512)
}

fn graph_text_semantic_key(entry: &PageEntry) -> (u8, String) {
    let kind = match entry.kind {
        PageKind::Page => 0,
        PageKind::Journal => 1,
    };
    (kind, crate::refs::page_key(&entry.name))
}

fn graph_text_captures_match(first: &GraphTextCapture, second: &GraphTextCapture) -> bool {
    first.directories_by_exact_relative == second.directories_by_exact_relative
        && first.paths_by_file_resource == second.paths_by_file_resource
        && first.file_link_count_by_exact_relative == second.file_link_count_by_exact_relative
        && first.all_entries == second.all_entries
        && first.raw_bytes == second.raw_bytes
        && first.entries.len() == second.entries.len()
        && first
            .entries
            .iter()
            .zip(&second.entries)
            .all(|(first, second)| {
                first.path == second.path
                    && first.description == second.description
                    && first.file_resource_id == second.file_resource_id
                    && first.link_count == second.link_count
            })
}

fn build_graph_text_admission_index(
    graph: &Graph,
    capture: &GraphTextCapture,
    limits: GraphTextCaptureLimits,
    combined_capture_bytes: u64,
    decode_semantics: bool,
    prior: Option<&CompleteGraphTextAdmissionIndex>,
) -> io::Result<CompleteGraphTextAdmissionIndex> {
    let (scope_binding, graph_resource) = if decode_semantics {
        (
            graph.graph_text_scope_binding()?,
            graph.canonical_resource_id()?,
        )
    } else {
        let binding = graph.graph_text_write_binding()?;
        (
            graph
                .graph_text_scope
                .bind_graph_resource(binding.resource_id),
            binding.resource_id,
        )
    };
    if scope_binding.graph_resource_id() != graph_resource {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "graph-text scope binding does not match the retained graph resource",
        ));
    }
    let permanent_bytes =
        graph_text_initial_permanent_upper_bound(graph, capture, decode_semantics)?;
    if permanent_bytes > limits.permanent_index_bytes {
        return Err(graph_text_capture_limit_error("permanent index memory"));
    }
    let validation_scratch =
        graph_text_index_validation_scratch_upper_bound(graph, capture, decode_semantics)?;
    ensure_graph_text_peak_limit(
        combined_capture_bytes,
        checked_add_bytes(permanent_bytes, validation_scratch)?,
        limits.peak_build_bytes,
    )?;

    // All permanent rows are reserved above before the first map allocation.
    let mut file_resource_by_exact_relative = PersistentMap::default();
    let mut file_is_graph_text_by_exact_relative = PersistentMap::default();
    for (resource, paths) in &capture.paths_by_file_resource {
        for path in paths {
            count_graph_text_admission_index_map_insertion();
            file_resource_by_exact_relative.insert(path.clone(), *resource);
            count_graph_text_admission_index_map_insertion();
            file_is_graph_text_by_exact_relative.insert(path.clone(), false);
        }
    }
    let mut index = CompleteGraphTextAdmissionIndex {
        instance: Arc::clone(&graph.graph_text_admission_instance),
        scope_binding,
        graph_resource,
        generation: 1,
        files_by_exact_path: PersistentMap::default(),
        paths_by_portable_key: PersistentMap::default(),
        paths_by_file_resource: capture
            .paths_by_file_resource
            .iter()
            .map(|(key, value)| (*key, value.clone()))
            .collect(),
        file_resource_by_exact_relative,
        file_link_count_by_exact_relative: capture
            .file_link_count_by_exact_relative
            .iter()
            .map(|(key, value)| (key.clone(), *value))
            .collect(),
        file_is_graph_text_by_exact_relative,
        paths_by_semantic_key: PersistentMap::default(),
        tombstones_by_exact_path: PersistentMap::default(),
        directories_by_exact_relative: capture
            .directories_by_exact_relative
            .iter()
            .map(|(key, value)| (key.clone(), *value))
            .collect(),
        permanent_bytes,
        permanent_limit: limits.permanent_index_bytes,
        peak_limit: limits.peak_build_bytes,
    };
    // Collision groups are assembled mutably and sealed once. Updating a
    // persistent value for every member would repeatedly copy a growing set
    // before the boundary collision check.
    let mut portable_groups = std::collections::BTreeMap::new();
    let mut semantic_groups = std::collections::BTreeMap::new();
    let cached_semantics = if decode_semantics {
        std::collections::HashMap::new()
    } else {
        graph
            .cache
            .read()
            .unwrap()
            .as_ref()
            .map(|pages| {
                pages
                    .iter()
                    .map(|(entry, _)| (entry.rel_path.clone(), entry.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };
    for entry in &capture.entries {
        let bytes = entry
            .bytes
            .as_deref()
            .expect("the first capture pass retains bytes");
        // THE cut (GH #267). A rebuild used to parse EVERY document in the graph
        // whenever the exact-observation chain had broken -- on every save, and
        // on Windows or a network share that was essentially always. But an
        // invalidation says "we lost track", not "everything changed": almost
        // every file still holds byte-for-byte the same content it held when we
        // last parsed it, and `description` (a SHA-256 of the content plus its
        // length) proves which. Reuse those, and parse only what actually moved.
        //
        // `semantic_parsed` is what makes the reuse sound: a record whose
        // semantic came from the page cache or from its filename is a guess, and
        // carrying it forward would let later builds treat it as parsed.
        let reused = decode_semantics
            .then(|| prior?.files_by_exact_path.get(&entry.path))
            .flatten()
            .filter(|record| record.semantic_parsed && record.description == entry.description);
        let (semantic, format) = if let Some(record) = reused {
            (record.semantic.clone(), record.format)
        } else if decode_semantics {
            let content = std::str::from_utf8(bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("graph text is not UTF-8: {}", entry.path),
                )
            })?;
            let permit = graph_text_parse_budget_permit(graph, &entry.path, content)?;
            let (semantic, format, node_count) =
                graph.decode_present_graph_text_with_node_count(&entry.path, bytes, permit)?;
            if node_count > MAX_GRAPH_TEXT_PARSER_NODES {
                return Err(graph_text_capture_limit_error("parser node count"));
            }
            (semantic, format)
        } else {
            (
                cached_semantics
                    .get(entry.path.as_str())
                    .cloned()
                    .map(Ok)
                    .unwrap_or_else(|| graph.graph_text_entry_for_graph_text_path(&entry.path))
                    .map_err(|error| {
                        io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                    })?,
                Format::from_path(Path::new(entry.path.as_str())),
            )
        };
        let record = GraphTextAdmissionRecord {
            description: entry.description,
            file_resource_id: entry.file_resource_id,
            link_count: entry.link_count,
            semantic,
            format,
            semantic_parsed: decode_semantics,
        };
        index
            .file_is_graph_text_by_exact_relative
            .insert(entry.path.as_str().to_owned(), true);
        count_graph_text_admission_index_map_insertion();
        initial_graph_text_collision_group_insert(
            &mut portable_groups,
            entry.path.portable_key(),
            entry.path.clone(),
        );
        count_graph_text_admission_index_map_insertion();
        initial_graph_text_collision_group_insert(
            &mut semantic_groups,
            graph_text_semantic_key(&record.semantic),
            entry.path.clone(),
        );
        count_graph_text_admission_index_map_insertion();
        if index
            .files_by_exact_path
            .insert(entry.path.clone(), record)
            .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph-text capture contains duplicate exact graph-text paths",
            ));
        }
    }
    index.paths_by_portable_key = portable_groups.into_iter().collect();
    index.paths_by_semantic_key = semantic_groups.into_iter().collect();
    validate_graph_text_admission_index(&index)?;
    Ok(index)
}

fn graph_text_initial_permanent_upper_bound(
    graph: &Graph,
    capture: &GraphTextCapture,
    decode_semantics: bool,
) -> io::Result<u64> {
    let title_format = graph_text_journal_title_format_budget(graph)?;
    let mut bytes = checked_add_bytes(
        usize_to_u64(std::mem::size_of::<CompleteGraphTextAdmissionIndex>())?,
        512,
    )?;
    bytes = checked_add_bytes(
        bytes,
        owned_string_len_upper_bound(title_format.input_bytes)?,
    )?;
    for relative in capture.directories_by_exact_relative.keys() {
        bytes = checked_add_bytes(
            bytes,
            graph_text_owned_btree_row_upper_bound::<String, ContentDigest>(usize_to_u64(
                relative.len(),
            )?)?,
        )?;
    }
    for paths in capture.paths_by_file_resource.values() {
        bytes = checked_add_bytes(
            bytes,
            conservative_btree_entry_bytes::<ContentDigest, std::collections::BTreeSet<String>>()?,
        )?;
        for path in paths {
            let path_len = usize_to_u64(path.len())?;
            bytes = checked_add_bytes(
                bytes,
                graph_text_owned_btree_row_upper_bound::<String, ()>(path_len)?,
            )?;
            bytes = checked_add_bytes(
                bytes,
                graph_text_owned_btree_row_upper_bound::<String, ContentDigest>(path_len)?,
            )?;
            bytes = checked_add_bytes(
                bytes,
                graph_text_owned_btree_row_upper_bound::<String, u64>(path_len)?,
            )?;
            bytes = checked_add_bytes(
                bytes,
                graph_text_owned_btree_row_upper_bound::<String, bool>(path_len)?,
            )?;
        }
    }
    for entry in &capture.entries {
        let content_bytes = entry.bytes.as_ref().expect("first capture retains bytes");
        let semantic_name_len = if decode_semantics {
            let content = std::str::from_utf8(content_bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "graph text is not UTF-8")
            })?;
            graph_text_observed_semantic_name_upper_bound(graph, &entry.path, content)?
                .semantic_name_bytes
        } else {
            guarded_graph_text_semantic_name_upper_bound(graph, &entry.path, content_bytes.len())?
        };
        bytes = checked_add_bytes(
            bytes,
            graph_text_file_record_worst_case_upper_bound(
                graph,
                usize_to_u64(entry.path.as_str().len())?,
                semantic_name_len,
            )?,
        )?;
    }
    Ok(bytes)
}

fn graph_text_index_validation_scratch_upper_bound(
    graph: &Graph,
    capture: &GraphTextCapture,
    decode_semantics: bool,
) -> io::Result<u64> {
    let mut largest_path = 0_u64;
    let mut largest_name = 0_u64;
    for entry in &capture.entries {
        let path = usize_to_u64(entry.path.as_str().len())?;
        let bytes = entry.bytes.as_ref().expect("first capture retains bytes");
        largest_path = largest_path.max(path);
        largest_name = largest_name.max(if decode_semantics {
            let content = std::str::from_utf8(bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "graph text is not UTF-8")
            })?;
            graph_text_observed_semantic_name_upper_bound(graph, &entry.path, content)?
                .semantic_name_bytes
        } else {
            guarded_graph_text_semantic_name_upper_bound(graph, &entry.path, bytes.len())?
        });
    }
    let mut bytes = checked_add_bytes(
        checked_add_bytes(
            owned_string_len_upper_bound(checked_mul_bytes(largest_path, 8)?)?,
            owned_string_len_upper_bound(checked_mul_bytes(largest_name, 8)?)?,
        )?,
        1024,
    )?;
    let all_files = capture.file_link_count_by_exact_relative.len();
    let graph_text_files = capture.entries.len();
    for structural in [
        persistent_map_build_path_peak_upper_bound::<GraphTextPath, GraphTextAdmissionRecord>(
            graph_text_files,
        )?,
        persistent_map_build_path_peak_upper_bound::<
            PortablePathKey,
            std::collections::BTreeSet<GraphTextPath>,
        >(graph_text_files)?,
        persistent_map_build_path_peak_upper_bound::<
            ContentDigest,
            std::collections::BTreeSet<String>,
        >(all_files)?,
        persistent_map_build_path_peak_upper_bound::<String, ContentDigest>(all_files)?,
        persistent_map_build_path_peak_upper_bound::<String, u64>(all_files)?,
        persistent_map_build_path_peak_upper_bound::<String, bool>(all_files)?,
        persistent_map_build_path_peak_upper_bound::<
            (u8, String),
            std::collections::BTreeSet<GraphTextPath>,
        >(graph_text_files)?,
    ] {
        bytes = checked_add_bytes(bytes, structural)?;
    }
    Ok(bytes)
}

fn persistent_map_build_path_peak_upper_bound<K, V>(entries: usize) -> io::Result<u64> {
    if entries == 0 {
        return Ok(0);
    }
    let binary_digits = u64::from(usize::BITS - entries.leading_zeros());
    let conservative_avl_depth = checked_mul_bytes(binary_digits, 2)?;
    let copied_nodes = checked_add_bytes(checked_mul_bytes(conservative_avl_depth, 5)?, 1)?;
    checked_mul_bytes(
        copied_nodes,
        usize_to_u64(std::mem::size_of::<PersistentMapNode<K, V>>())?,
    )
}

fn graph_text_owned_btree_row_upper_bound<K, V>(owned_len: u64) -> io::Result<u64> {
    checked_add_bytes(
        conservative_btree_entry_bytes::<K, V>()?,
        owned_string_len_upper_bound(owned_len)?,
    )
}

const MAX_GRAPH_TEXT_SEMANTIC_NAME_BYTES: u64 = 256 * 1024;
const MAX_JOURNAL_TITLE_BYTES_PER_FORMAT_BYTE: u64 = 11;

#[derive(Clone, Copy)]
struct GraphTextJournalTitleFormatBudget {
    input_bytes: u64,
    rendered_bytes: u64,
}

#[derive(Clone, Copy)]
struct GraphTextSemanticNameBudget {
    semantic_name_bytes: u64,
}

fn graph_text_journal_title_format_budget(
    graph: &Graph,
) -> io::Result<GraphTextJournalTitleFormatBudget> {
    let input_bytes = usize_to_u64(graph.journal_format.title_format().len())?;
    // `date::Format` consumes at least one ASCII pattern byte per token and no
    // token can render more than an i32 year (11 bytes). Literal UTF-8 bytes are
    // copied one-for-one, so this is a strict bound for every compiled pattern.
    let rendered_bytes = checked_mul_bytes(input_bytes, MAX_JOURNAL_TITLE_BYTES_PER_FORMAT_BYTE)?;
    if input_bytes > MAX_GRAPH_TEXT_SEMANTIC_NAME_BYTES
        || rendered_bytes > MAX_GRAPH_TEXT_SEMANTIC_NAME_BYTES
    {
        return Err(graph_text_capture_limit_error(
            "journal title format expansion",
        ));
    }
    Ok(GraphTextJournalTitleFormatBudget {
        input_bytes,
        rendered_bytes,
    })
}

fn graph_text_observed_semantic_name_upper_bound(
    graph: &Graph,
    path: &GraphTextPath,
    content: &str,
) -> io::Result<GraphTextSemanticNameBudget> {
    let title_format = graph_text_journal_title_format_budget(graph)?;
    let mut observed = checked_add_bytes(usize_to_u64(path.as_str().len())?, 64)?;
    let format = Format::from_path(Path::new(path.as_str()));
    for line in content.lines() {
        let trimmed = line.trim();
        let title = line
            .split_once("::")
            .and_then(|(key, value)| key.trim().eq_ignore_ascii_case("title").then_some(value))
            .or_else(|| {
                (format == Format::Org).then_some(()).and_then(|()| {
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
                })
            });
        if let Some(title) = title {
            observed = observed.max(checked_add_bytes(usize_to_u64(title.trim().len())?, 64)?);
        }
    }
    observed = observed.max(title_format.rendered_bytes);
    if observed > MAX_GRAPH_TEXT_SEMANTIC_NAME_BYTES {
        return Err(graph_text_capture_limit_error("semantic title bytes"));
    }
    Ok(GraphTextSemanticNameBudget {
        semantic_name_bytes: observed,
    })
}

fn guarded_graph_text_semantic_name_upper_bound(
    graph: &Graph,
    path: &GraphTextPath,
    _content_len: usize,
) -> io::Result<u64> {
    let title_format = graph_text_journal_title_format_budget(graph)?;
    let observed =
        checked_add_bytes(usize_to_u64(path.as_str().len())?, 64)?.max(title_format.rendered_bytes);
    if observed > MAX_GRAPH_TEXT_SEMANTIC_NAME_BYTES {
        return Err(graph_text_capture_limit_error("semantic title bytes"));
    }
    Ok(observed)
}

fn graph_text_file_record_worst_case_upper_bound(
    graph: &Graph,
    path_len: u64,
    semantic_name_len: u64,
) -> io::Result<u64> {
    let absolute_len = checked_add_bytes(
        checked_add_bytes(
            usize_to_u64(graph.root.as_os_str().len())?,
            usize::from(path_len != 0) as u64,
        )?,
        path_len,
    )?;
    let portable_key_len = checked_mul_bytes(path_len, 8)?;
    let semantic_key_len = checked_mul_bytes(semantic_name_len, 8)?;
    let mut bytes = conservative_btree_entry_bytes::<GraphTextPath, GraphTextAdmissionRecord>()?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(path_len)?)?;
    bytes = checked_add_bytes(bytes, usize_to_u64(std::mem::size_of::<PageEntry>())?)?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(semantic_name_len)?)?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(path_len)?)?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(absolute_len)?)?;
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<PortablePathKey, std::collections::BTreeSet<GraphTextPath>>(
        )?,
    )?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(portable_key_len)?)?;
    bytes = checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<GraphTextPath, ()>(path_len)?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<(u8, String), std::collections::BTreeSet<GraphTextPath>>(
        )?,
    )?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(semantic_key_len)?)?;
    checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<GraphTextPath, ()>(path_len)?,
    )
}

fn graph_text_parse_budget_permit(
    graph: &Graph,
    path: &GraphTextPath,
    content: &str,
) -> io::Result<GraphTextParseBudgetPermit> {
    let semantic_budget = graph_text_observed_semantic_name_upper_bound(graph, path, content)?;
    // Parser work is one-file-at-a-time and its tree is dropped before the next
    // file. Source-derived `bytes == nodes in every allocation class` estimates
    // used to count parser, DTO, projection, and index representations as if all
    // were retained together. That rejected ordinary large pages (#311). The
    // real envelope is the 64 MiB exact-feed/source cap plus the post-parse
    // 1,000,000-node cap; only the semantic record below survives this call.
    Ok(GraphTextParseBudgetPermit {
        semantic_name_bytes: semantic_budget.semantic_name_bytes,
        semantic_name_allocation_bytes: owned_string_len_upper_bound(
            semantic_budget.semantic_name_bytes,
        )?,
    })
}

fn graph_text_admission_upsert_retained_upper_bound(
    relative: &str,
    path: Option<&GraphTextPath>,
    semantic: Option<&PageEntry>,
) -> io::Result<u64> {
    let relative_len = usize_to_u64(relative.len())?;
    let mut bytes = graph_text_owned_btree_row_upper_bound::<String, ContentDigest>(relative_len)?;
    bytes = checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<String, u64>(relative_len)?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<String, bool>(relative_len)?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<ContentDigest, std::collections::BTreeSet<String>>()?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<String, ()>(relative_len)?,
    )?;
    let (Some(path), Some(semantic)) = (path, semantic) else {
        return Ok(bytes);
    };
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<GraphTextPath, GraphTextAdmissionRecord>()?,
    )?;
    bytes = checked_add_bytes(bytes, owned_string_upper_bound(path.as_str())?)?;
    bytes = checked_add_bytes(bytes, graph_text_page_entry_retained_upper_bound(semantic)?)?;
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<PortablePathKey, std::collections::BTreeSet<GraphTextPath>>(
        )?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        owned_string_len_upper_bound(checked_mul_bytes(relative_len, 8)?)?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<GraphTextPath, ()>(relative_len)?,
    )?;
    let semantic_key = graph_text_semantic_key(semantic);
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<(u8, String), std::collections::BTreeSet<GraphTextPath>>(
        )?,
    )?;
    bytes = checked_add_bytes(bytes, owned_string_upper_bound(&semantic_key.1)?)?;
    bytes = checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<GraphTextPath, ()>(relative_len)?,
    )?;
    Ok(bytes)
}

fn graph_text_admission_tombstone_upper_bound(
    relative: &str,
    record: Option<&GraphTextAdmissionRecord>,
) -> io::Result<u64> {
    let relative_len = usize_to_u64(relative.len())?;
    let mut bytes = conservative_btree_entry_bytes::<GraphTextPath, GraphTextAdmissionTombstone>()?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(relative_len)?)?;
    if let Some(record) = record {
        bytes = checked_add_bytes(bytes, page_entry_clone_upper_bound(&record.semantic)?)?;
    }
    checked_add_bytes(bytes, 512)
}

fn graph_text_admission_delta_structural_peak(
    index: &CompleteGraphTextAdmissionIndex,
) -> io::Result<u64> {
    let mut bytes = 0;
    for charge in [
        index.files_by_exact_path.path_copy_peak_upper_bound()?,
        index.paths_by_portable_key.path_copy_peak_upper_bound()?,
        index.paths_by_file_resource.path_copy_peak_upper_bound()?,
        index
            .file_resource_by_exact_relative
            .path_copy_peak_upper_bound()?,
        index
            .file_link_count_by_exact_relative
            .path_copy_peak_upper_bound()?,
        index
            .file_is_graph_text_by_exact_relative
            .path_copy_peak_upper_bound()?,
        index.paths_by_semantic_key.path_copy_peak_upper_bound()?,
        index
            .tombstones_by_exact_path
            .path_copy_peak_upper_bound()?,
    ] {
        bytes = checked_add_bytes(bytes, charge)?;
    }
    Ok(bytes)
}

fn graph_text_admission_delta_payload_peak(
    index: &CompleteGraphTextAdmissionIndex,
    relative: &str,
    prepared: Option<&PreparedGraphTextAdmissionUpsert>,
) -> io::Result<u64> {
    fn graph_text_members(
        members: Option<&std::collections::BTreeSet<GraphTextPath>>,
    ) -> io::Result<u64> {
        let Some(members) = members else {
            return Ok(0);
        };
        count_graph_text_admission_persistent_payload_members(members.len());
        let mut bytes = conservative_btree_entry_bytes::<GraphTextPath, ()>()?;
        for member in members {
            bytes = checked_add_bytes(
                bytes,
                graph_text_owned_btree_row_upper_bound::<GraphTextPath, ()>(usize_to_u64(
                    member.as_str().len(),
                )?)?,
            )?;
        }
        Ok(bytes)
    }

    fn string_members(members: Option<&std::collections::BTreeSet<String>>) -> io::Result<u64> {
        let Some(members) = members else {
            return Ok(0);
        };
        count_graph_text_admission_persistent_payload_members(members.len());
        let mut bytes = conservative_btree_entry_bytes::<String, ()>()?;
        for member in members {
            bytes = checked_add_bytes(
                bytes,
                graph_text_owned_btree_row_upper_bound::<String, ()>(usize_to_u64(member.len())?)?,
            )?;
        }
        Ok(bytes)
    }

    let mut bytes = 0;
    if let Ok(path) = GraphTextPath::parse(relative.to_owned()) {
        if let Some(record) = index.files_by_exact_path.get(&path) {
            bytes = checked_add_bytes(
                bytes,
                graph_text_members(index.paths_by_portable_key.get(&path.portable_key()))?,
            )?;
            bytes = checked_add_bytes(
                bytes,
                graph_text_members(
                    index
                        .paths_by_semantic_key
                        .get(&graph_text_semantic_key(&record.semantic)),
                )?,
            )?;
        }
    }
    if let Some(resource) = index.file_resource_by_exact_relative.get(relative) {
        bytes = checked_add_bytes(
            bytes,
            string_members(index.paths_by_file_resource.get(resource))?,
        )?;
    }
    if let Some(prepared) = prepared {
        bytes = checked_add_bytes(
            bytes,
            string_members(index.paths_by_file_resource.get(&prepared.file_resource_id))?,
        )?;
        if let Some((path, record)) = &prepared.eligible {
            bytes = checked_add_bytes(
                bytes,
                graph_text_members(index.paths_by_portable_key.get(&path.portable_key()))?,
            )?;
            bytes = checked_add_bytes(
                bytes,
                graph_text_members(
                    index
                        .paths_by_semantic_key
                        .get(&graph_text_semantic_key(&record.semantic)),
                )?,
            )?;
        }
    }
    Ok(bytes)
}

fn graph_text_admission_upsert_worst_case_upper_bound(
    graph: &Graph,
    relative: &str,
    eligible_path: Option<&GraphTextPath>,
    content: &str,
) -> io::Result<u64> {
    let relative_len = usize_to_u64(relative.len())?;
    let mut bytes = graph_text_admission_upsert_retained_upper_bound(relative, None, None)?;
    if eligible_path.is_some() {
        let path = eligible_path.expect("checked eligible path");
        let semantic_name_len =
            graph_text_observed_semantic_name_upper_bound(graph, path, content)?
                .semantic_name_bytes;
        bytes = checked_add_bytes(
            bytes,
            graph_text_file_record_worst_case_upper_bound(graph, relative_len, semantic_name_len)?,
        )?;
    }
    Ok(bytes)
}

fn persistent_set_insert<K, T>(
    map: &mut PersistentMap<K, std::collections::BTreeSet<T>>,
    key: K,
    member: T,
) where
    K: Ord + Clone,
    T: Ord + Clone,
{
    let mut members = map.get(&key).cloned().unwrap_or_default();
    count_graph_text_admission_persistent_payload_members(members.len().saturating_add(1));
    members.insert(member);
    map.insert(key, members);
}

fn persistent_set_remove<K, T>(
    map: &mut PersistentMap<K, std::collections::BTreeSet<T>>,
    key: &K,
    member: &T,
) where
    K: Ord + Clone,
    T: Ord + Clone,
{
    let Some(mut members) = map.get(key).cloned() else {
        return;
    };
    count_graph_text_admission_persistent_payload_members(members.len().saturating_add(1));
    members.remove(member);
    if members.is_empty() {
        map.remove(key);
    } else {
        map.insert(key.clone(), members);
    }
}

fn initial_graph_text_collision_group_insert<K, T>(
    map: &mut std::collections::BTreeMap<K, std::collections::BTreeSet<T>>,
    key: K,
    member: T,
) where
    K: Ord,
    T: Ord,
{
    count_graph_text_admission_persistent_payload_members(1);
    map.entry(key).or_default().insert(member);
}

fn validate_graph_text_admission_index(index: &CompleteGraphTextAdmissionIndex) -> io::Result<()> {
    for (path, record) in &index.files_by_exact_path {
        if !index
            .paths_by_portable_key
            .get(&path.portable_key())
            .is_some_and(|members| members.contains(path))
            || !index
                .paths_by_file_resource
                .get(&record.file_resource_id)
                .is_some_and(|members| members.contains(path.as_str()))
            || index
                .file_resource_by_exact_relative
                .get(path.as_str())
                .copied()
                != Some(record.file_resource_id)
            || index
                .file_link_count_by_exact_relative
                .get(path.as_str())
                .copied()
                != Some(record.link_count)
            || index
                .file_is_graph_text_by_exact_relative
                .get(path.as_str())
                .copied()
                != Some(true)
            || !index
                .paths_by_semantic_key
                .get(&graph_text_semantic_key(&record.semantic))
                .is_some_and(|members| members.contains(path))
            || index.tombstones_by_exact_path.contains_key(path)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph-text admission reverse map is incomplete",
            ));
        }
    }
    for (portable, members) in &index.paths_by_portable_key {
        for path in members {
            let Some(record) = index.files_by_exact_path.get(path) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph-text portable map contains a reverse-only member",
                ));
            };
            if path.portable_key() != *portable
                || index
                    .file_resource_by_exact_relative
                    .get(&path.as_str().to_owned())
                    .copied()
                    != Some(record.file_resource_id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph-text portable member is under the wrong reverse key",
                ));
            }
        }
    }
    for (semantic, members) in &index.paths_by_semantic_key {
        for path in members {
            let Some(record) = index.files_by_exact_path.get(path) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph-text semantic map contains a reverse-only member",
                ));
            };
            if graph_text_semantic_key(&record.semantic) != *semantic {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph-text semantic member is under the wrong reverse key",
                ));
            }
        }
    }
    for (resource, members) in &index.paths_by_file_resource {
        for relative in members {
            if index.file_resource_by_exact_relative.get(relative) != Some(resource) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph-text resource member is under the wrong reverse key",
                ));
            }
        }
    }
    for (relative, resource) in &index.file_resource_by_exact_relative {
        if !index
            .paths_by_file_resource
            .get(resource)
            .is_some_and(|members| members.contains(relative))
            || !index
                .file_link_count_by_exact_relative
                .contains_key(relative)
            || !index
                .file_is_graph_text_by_exact_relative
                .contains_key(relative)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph-text file-resource map contains a reverse-only member",
            ));
        }
    }
    for relative in index.file_link_count_by_exact_relative.keys() {
        if !index.file_resource_by_exact_relative.contains_key(relative)
            || !index
                .file_is_graph_text_by_exact_relative
                .contains_key(relative)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph-text link-count map contains a forward-only member",
            ));
        }
    }
    for (relative, is_graph_text) in &index.file_is_graph_text_by_exact_relative {
        let exact = GraphTextPath::parse(relative.clone())
            .ok()
            .and_then(|path| index.files_by_exact_path.get(&path));
        if !index.file_resource_by_exact_relative.contains_key(relative)
            || !index
                .file_link_count_by_exact_relative
                .contains_key(relative)
            || (*is_graph_text != exact.is_some())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph-text path-kind relationships are inconsistent",
            ));
        }
    }
    for (path, tombstone) in &index.tombstones_by_exact_path {
        if index.files_by_exact_path.contains_key(path)
            || index
                .file_resource_by_exact_relative
                .contains_key(&path.as_str().to_owned())
            || index
                .file_link_count_by_exact_relative
                .contains_key(&path.as_str().to_owned())
            || index
                .file_is_graph_text_by_exact_relative
                .contains_key(path.as_str())
            || tombstone.prior_record.as_ref().is_some_and(|record| {
                record.file_resource_id != tombstone.prior_file_resource_id
                    || record.link_count != tombstone.prior_link_count
                    || record.semantic.rel_path != path.as_str()
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph-text tombstone relationships are inconsistent",
            ));
        }
    }
    Ok(())
}

fn validate_graph_text_admission_delta(
    index: &CompleteGraphTextAdmissionIndex,
    relative: &str,
) -> io::Result<()> {
    let path = match GraphTextPath::parse(relative.to_owned()) {
        Ok(path) => path,
        Err(_) => {
            let resource = index.file_resource_by_exact_relative.get(relative);
            let link_count = index.file_link_count_by_exact_relative.get(relative);
            let is_graph_text = index
                .file_is_graph_text_by_exact_relative
                .get(relative)
                .copied();
            if let Some(resource) = resource {
                if link_count.is_none()
                    || link_count.copied() != Some(1)
                    || is_graph_text != Some(false)
                    || !index
                        .paths_by_file_resource
                        .get(resource)
                        .is_some_and(|members| members.len() == 1 && members.contains(relative))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "exact non-text delta has inconsistent resource evidence",
                    ));
                }
            } else if link_count.is_some() || is_graph_text.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "removed non-text delta retained forward evidence",
                ));
            }
            return Ok(());
        }
    };
    let record = index.files_by_exact_path.get(&path);
    let resource = index
        .file_resource_by_exact_relative
        .get(&relative.to_owned());
    let link_count = index
        .file_link_count_by_exact_relative
        .get(&relative.to_owned());
    let is_graph_text = index
        .file_is_graph_text_by_exact_relative
        .get(relative)
        .copied();
    if let Some(record) = record {
        let portable = index
            .paths_by_portable_key
            .get(&path.portable_key())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing portable reverse group")
            })?;
        let resources = index
            .paths_by_file_resource
            .get(&record.file_resource_id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing resource reverse group")
            })?;
        let semantic = index
            .paths_by_semantic_key
            .get(&graph_text_semantic_key(&record.semantic))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing semantic reverse group")
            })?;
        if resource != Some(&record.file_resource_id)
            || link_count != Some(&record.link_count)
            || record.link_count != 1
            || !portable.contains(&path)
            || resources.len() != 1
            || !resources.contains(relative)
            || !semantic.contains(&path)
            || index.tombstones_by_exact_path.contains_key(&path)
            || is_graph_text != Some(true)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "exact graph-text delta has colliding or inconsistent evidence: \
                     resource_match={} link_match={} links={} portable_member={} \
                     resource_members={} resource_member={} semantic_member={} \
                     tombstoned={} graph_text={is_graph_text:?}",
                    resource == Some(&record.file_resource_id),
                    link_count == Some(&record.link_count),
                    record.link_count,
                    portable.contains(&path),
                    resources.len(),
                    resources.contains(relative),
                    semantic.contains(&path),
                    index.tombstones_by_exact_path.contains_key(&path),
                ),
            ));
        }
    } else if let Some(resource) = resource {
        let resources = index.paths_by_file_resource.get(resource).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing resource reverse group")
        })?;
        if link_count.copied() != Some(1)
            || resources.len() != 1
            || !resources.contains(relative)
            || is_graph_text != Some(false)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exact non-text delta has inconsistent resource evidence",
            ));
        }
    } else if link_count.is_some() || is_graph_text.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "exact delta retained link evidence without a resource",
        ));
    }
    if index.tombstones_by_exact_path.contains_key(&path)
        && (record.is_some()
            || resource.is_some()
            || link_count.is_some()
            || is_graph_text.is_some())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "exact delta is simultaneously active and deleted",
        ));
    }
    Ok(())
}

fn graph_text_delta_reverse_members(
    index: &CompleteGraphTextAdmissionIndex,
    relative: &str,
) -> usize {
    let Ok(path) = GraphTextPath::parse(relative.to_owned()) else {
        return 0;
    };
    let Some(record) = index.files_by_exact_path.get(&path) else {
        return index
            .file_resource_by_exact_relative
            .get(relative)
            .and_then(|resource| index.paths_by_file_resource.get(resource))
            .map_or(0, std::collections::BTreeSet::len);
    };
    index
        .paths_by_portable_key
        .get(&path.portable_key())
        .map_or(0, std::collections::BTreeSet::len)
        + index
            .paths_by_file_resource
            .get(&record.file_resource_id)
            .map_or(0, std::collections::BTreeSet::len)
        + index
            .paths_by_semantic_key
            .get(&graph_text_semantic_key(&record.semantic))
            .map_or(0, std::collections::BTreeSet::len)
}

fn remove_graph_text_admission_path(
    index: &mut CompleteGraphTextAdmissionIndex,
    relative: &str,
) -> Option<GraphTextAdmissionTombstone> {
    let mut prior_record = None;
    if let Ok(path) = GraphTextPath::parse(relative.to_owned()) {
        if let Some(record) = index.files_by_exact_path.remove(&path) {
            let portable = path.portable_key();
            persistent_set_remove(&mut index.paths_by_portable_key, &portable, &path);
            let semantic = graph_text_semantic_key(&record.semantic);
            persistent_set_remove(&mut index.paths_by_semantic_key, &semantic, &path);
            prior_record = Some(record);
        }
    }
    let relative_owned = relative.to_owned();
    let resource = index
        .file_resource_by_exact_relative
        .remove(&relative_owned)?;
    persistent_set_remove(
        &mut index.paths_by_file_resource,
        resource.as_ref(),
        &relative_owned,
    );
    let link_count = index
        .file_link_count_by_exact_relative
        .remove(&relative_owned)
        .map_or(0, |links| *links);
    index
        .file_is_graph_text_by_exact_relative
        .remove(&relative_owned);
    Some(GraphTextAdmissionTombstone {
        prior_record,
        prior_file_resource_id: *resource,
        prior_link_count: link_count,
    })
}

fn graph_text_event_scratch_upper_bound(relative: &str) -> io::Result<u64> {
    let relative_len = usize_to_u64(relative.len())?;
    let component_slots = relative_len.max(1);
    let mut bytes = conservative_vec_capacity_upper_bound::<String>(component_slots)?;
    bytes = checked_add_bytes(
        bytes,
        conservative_vec_capacity_upper_bound::<Dir>(component_slots)?,
    )?;
    // Component strings, filename, parent-relative construction, GraphTextPath,
    // normalized keys, and error-path scratch are never simultaneously larger
    // than these conservative full-relative clones.
    bytes = checked_add_bytes(
        bytes,
        checked_mul_bytes(owned_string_len_upper_bound(relative_len)?, 8)?,
    )?;
    checked_add_bytes(bytes, 1024)
}

#[cfg(unix)]
fn projection_file_link_count(file: &fs::File) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.nlink())
}

#[cfg(windows)]
fn projection_file_link_count(file: &fs::File) -> io::Result<u64> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` retains the exact live handle and `information` is a
    // correctly sized writable result value.
    let result = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(information.nNumberOfLinks))
}

#[cfg(not(any(unix, windows)))]
fn projection_file_link_count(_file: &fs::File) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file link-count proof is unavailable on this platform",
    ))
}

fn validate_graph_text_single_link(file: &fs::File, relative: &str) -> io::Result<()> {
    let link_count = projection_file_link_count(file)?;
    if link_count != 1 {
        return Err(DirectSaveError::into_io(
            DirectSaveFailureCode::PrecheckResourceAlias,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "graph text files alias one physical resource: {relative} has link count {link_count}"
                ),
            ),
        ));
    }
    Ok(())
}

fn validate_graph_text_event_parent(
    index: &CompleteGraphTextAdmissionIndex,
    target: &GraphTextExactPath,
    parent: &ProjectionParent,
) -> io::Result<()> {
    let mut retained_relative = String::new();
    for (depth, directory) in parent.chain.iter().enumerate() {
        if depth > 0 {
            if !retained_relative.is_empty() {
                retained_relative.push('/');
            }
            retained_relative.push_str(&target.parent_components[depth - 1]);
        }
        let expected = index
            .directories_by_exact_relative
            .get(&retained_relative)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!(
                        "exact feed parent was not retained in the snapshot: {retained_relative}"
                    ),
                )
            })?;
        if canonical_projection_directory_resource_id(directory)? != *expected {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("exact feed parent identity changed: {retained_relative}"),
            ));
        }
    }
    Ok(())
}

const MAX_GRAPH_TEXT_ADMISSION_DIAGNOSTIC_CAUSE_BYTES: usize = 4096;

/// Exact-path shape bounds for one graph-relative platform event path.
/// Carried over unchanged from the deleted exact-feed batch that used to
/// namespace them; `classify_graph_text_exact_feed_path` is the live consumer.
const MAX_GRAPH_TEXT_EXACT_RELATIVE_BYTES: usize = 4096;
const MAX_GRAPH_TEXT_EXACT_PATH_COMPONENTS: usize = MAX_GRAPH_TEXT_CAPTURE_DIRECTORY_DEPTH + 1;

fn validate_graph_text_exact_feed_relative(relative: &str) -> io::Result<()> {
    if relative != relative.trim()
        || relative.is_empty()
        || relative.len() > MAX_GRAPH_TEXT_EXACT_RELATIVE_BYTES
        || relative.starts_with('/')
        || relative.contains('\\')
        || relative.contains('\0')
        || relative.split('/').count() > MAX_GRAPH_TEXT_EXACT_PATH_COMPONENTS
        || relative
            .split('/')
            .any(|component| !projection_component_is_portable(component))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid exact graph-relative feed path",
        ));
    }
    Ok(())
}

fn graph_text_exact_feed_failure_cause(cause: &str) -> String {
    let label = "directory mutation";
    let available = MAX_GRAPH_TEXT_ADMISSION_DIAGNOSTIC_CAUSE_BYTES.saturating_sub(label.len() + 2);
    let mut boundary = cause.len().min(available);
    while !cause.is_char_boundary(boundary) {
        boundary -= 1;
    }
    bounded_graph_text_admission_cause(format!("{label}: {}", &cause[..boundary]))
}
fn bounded_graph_text_admission_cause(mut cause: String) -> String {
    if cause.len() <= MAX_GRAPH_TEXT_ADMISSION_DIAGNOSTIC_CAUSE_BYTES {
        return cause;
    }
    let mut boundary = MAX_GRAPH_TEXT_ADMISSION_DIAGNOSTIC_CAUSE_BYTES;
    while !cause.is_char_boundary(boundary) {
        boundary -= 1;
    }
    cause.truncate(boundary);
    cause
}

fn graph_text_admission_unavailable(cause: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("graph-text admission authority unavailable: {cause}"),
    )
}

/// The observation epoch a banner-class conflict was minted at, if this error
/// carries one. The UI stores it with the banner and presents it back on "Keep
/// mine" so the override answers the conflict the user saw, not whatever
/// authority happens to be current when the request runs.
pub fn direct_save_conflict_epoch(error: &io::Error) -> Option<u64> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<DirectSaveError>())
        .and_then(DirectSaveError::conflict_epoch)
}

/// Read the BOUNDED failure code a Direct-Markdown save producer stamped on
/// this error, or `unknown` if it did not stamp one.
///
/// Two reasons the code exists rather than logging the error itself. First, the
/// error messages carry graph-relative paths, and a user's page titles are their
/// private data -- a diagnostic that cannot be pasted into a bug report is not a
/// diagnostic. Second, "the save failed" is useless triage: the failure classes
/// behind it (a symlink somewhere in the walk, ambient filesystem churn between
/// the capture's two passes, a same-bytes external replace that moved the inode,
/// a rejected reparse point) have completely different fixes, and from outside
/// the process they are otherwise indistinguishable.
///
/// This function matches NOTHING. The code is a typed field on `DirectSaveError`
/// set where the failure is constructed, so a page whose own title contains one
/// of the display sentences can no longer be classified as a conflict -- the
/// misclassification that made the banner's "Use disk version" discard an
/// unsaved edit. `direct_save_failure_code_does_not_inherit_conflict_from_page_text`
/// pins that.
///
/// The risk this moved the failure mode TO is a producer stamping the wrong
/// variant, so the guards are on the producers, not here:
/// `direct_save_conflict_sites_produce_their_own_codes` drives every
/// `EditorConflictSite` through the real minting helpers,
/// `direct_save_precheck_helpers_produce_their_own_codes` drives the free
/// helpers, and `every_direct_save_failure_code_has_a_production_producer`
/// scans shipped source for a construction site per variant.
pub fn direct_save_failure_code(error: &io::Error) -> &'static str {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<DirectSaveError>())
        .map(|typed| typed.code().as_str())
        .unwrap_or(DirectSaveFailureCode::Unknown.as_str())
}

fn graph_text_capture_limit_error(resource: &'static str) -> io::Error {
    DirectSaveError::into_io(
        DirectSaveFailureCode::PrecheckLimit,
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("graph-text capture {resource} bound exceeded"),
        ),
    )
}

fn graph_text_inventory_limit_error(resource: &'static str) -> io::Error {
    DirectSaveError::into_io(
        DirectSaveFailureCode::PrecheckLimit,
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("graph text inventory {resource} bound exceeded"),
        ),
    )
}

fn graph_text_inventory_alias_error(
    resource: &'static str,
    first: &str,
    second: &str,
) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("graph {resource} alias one resource: {first} and {second}"),
    )
}

fn create_projection_temp(dir: &Dir, filename: &str, bytes: &[u8]) -> io::Result<String> {
    create_projection_staging_file(dir, filename, bytes, "projection.tmp")
}

fn create_editor_staged_recovery(
    dir: &Dir,
    filename: &str,
    bytes: &[u8],
    turn_short_id: Option<[u8; 4]>,
) -> io::Result<String> {
    let Some(turn_short_id) = turn_short_id else {
        return create_projection_staging_file(dir, filename, bytes, "editor-staged-recovery");
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    static STAGED_SEQ: AtomicU64 = AtomicU64::new(0);
    for _ in 0..128 {
        let name = format!(
            ".{filename}.{}.{}.{}.editor-staged-recovery",
            std::process::id(),
            STAGED_SEQ.fetch_add(1, Ordering::Relaxed),
            short_turn_id(turn_short_id),
        );
        let mut options = CapOpenOptions::new();
        options.write(true).create_new(true);
        match dir.open_with(&name, &options) {
            Ok(mut file) => {
                let result = file.write_all(bytes).and_then(|()| barrier_sync_all(&file));
                drop(file);
                if let Err(error) = result {
                    let _ = dir.remove_file(&name);
                    return Err(error);
                }
                return Ok(name);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve editor staged-recovery file",
    ))
}

fn short_turn_id(bytes: [u8; 4]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    )
}

/// Parse only the complete filenames emitted by the editor publication
/// protocol. The two numeric fields are part of the authority: a user file
/// that merely ends in `editor-recovery` is not a cleanup candidate.
fn editor_recovery_target_name(name: &str) -> Option<&str> {
    fn parse_legacy(candidate: &str) -> Option<&str> {
        let (candidate, sequence) = candidate.rsplit_once('.')?;
        let (target, process) = candidate.rsplit_once('.')?;
        (!target.is_empty()
            && !sequence.is_empty()
            && sequence.bytes().all(|byte| byte.is_ascii_digit())
            && !process.is_empty()
            && process.bytes().all(|byte| byte.is_ascii_digit())
            && text_extension_from_path(Path::new(target)).is_some())
        .then_some(target)
    }

    let rest = name.strip_prefix('.')?;
    let rest = rest
        .strip_suffix(".editor-staged-recovery")
        .or_else(|| rest.strip_suffix(".editor-recovery"))?;
    if let Some((legacy_shape, turn)) = rest.rsplit_once('.') {
        if turn.len() == 8 && turn.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            if let Some(target) = parse_legacy(legacy_shape) {
                return Some(target);
            }
        }
    }
    parse_legacy(rest)
}

/// Parse the cleanup-only name emitted after a Direct Files replacement has
/// already published and validated its new live file. Unlike
/// [`editor_recovery_target_name`], this grammar never confers restoration or
/// conflict authority; checked open may only remove the exact producer shape.
fn editor_retired_target_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix('.')?.strip_suffix(".editor-retired")?;
    let (candidate, sequence) = rest.rsplit_once('.')?;
    let (target, process) = candidate.rsplit_once('.')?;
    (!target.is_empty()
        && !sequence.is_empty()
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
        && !process.is_empty()
        && process.bytes().all(|byte| byte.is_ascii_digit())
        && text_extension_from_path(Path::new(target)).is_some())
    .then_some(target)
}

fn create_projection_staging_file(
    dir: &Dir,
    filename: &str,
    bytes: &[u8],
    suffix: &str,
) -> io::Result<String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    for _ in 0..128 {
        let name = format!(
            ".{filename}.{}.{}.{suffix}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let mut options = CapOpenOptions::new();
        options.write(true).create_new(true);
        match dir.open_with(&name, &options) {
            Ok(mut file) => {
                let result = file.write_all(bytes).and_then(|()| barrier_sync_all(&file));
                drop(file);
                if let Err(error) = result {
                    let _ = dir.remove_file(&name);
                    return Err(error);
                }
                return Ok(name);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve projection temporary file",
    ))
}

/// Verify directory durability support before the first live-name mutation.
/// Unix targets probe the filesystem and fail before retirement/publication
/// when directory flushing is unavailable. Windows first validates the retained
/// exact directory capability, then records its documented lack of a
/// directory-entry flush primitive as a platform limitation.
///
/// The probe covers exactly the directory the operation will later flush — the
/// chain leaf — because that is the only barrier the operation takes. It is
/// strict on every platform: the graph tree is the sole authority for its bytes.
fn preflight_projection_chain(chain: &[Dir]) -> io::Result<()> {
    sync_projection_chain(chain)
}

/// The exact platform primitive named by the projection receipt. It is a
/// per-target constant so the enriched failure detail keeps naming the call the
/// device actually refused.
#[cfg(any(target_os = "linux", target_os = "android"))]
const PROJECTION_NOREPLACE_RENAME_OPERATION: &str =
    "renameat2(RENAME_NOREPLACE) publishing the projection";

#[cfg(any(target_os = "macos", target_os = "ios"))]
const PROJECTION_NOREPLACE_RENAME_OPERATION: &str =
    "renameatx_np(RENAME_EXCL) publishing the projection";

#[cfg(windows)]
const PROJECTION_NOREPLACE_RENAME_OPERATION: &str =
    "FileRenameInformation(ReplaceIfExists=false) publishing the projection";

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "android",
    windows
)))]
const PROJECTION_NOREPLACE_RENAME_OPERATION: &str =
    "atomic no-clobber rename publishing the projection";

/// The raw platform no-replace rename. It returns the untouched platform error,
/// so [`rename_projection_noreplace`] names the refused call around the exact
/// `errno` rather than an `io::Error::new` that would discard it.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_projection_noreplace_platform(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd};

    let from = CString::new(from)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid temporary name"))?;
    let to = CString::new(to)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid target name"))?;
    let result = unsafe {
        // Use the syscall entry point on Android: bionic's renameat2 wrapper is
        // API-30-only, while the kernel primitive and syscall() are available
        // on the supported Android baseline. Linux uses the identical path.
        libc::syscall(
            libc::SYS_renameat2,
            dir.as_fd().as_raw_fd(),
            from.as_ptr(),
            dir.as_fd().as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE as libc::c_uint,
        )
    };
    (result == 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn rename_projection_noreplace_platform(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd};

    let from = CString::new(from)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid source name"))?;
    let to = CString::new(to)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid target name"))?;
    let result = unsafe {
        libc::renameatx_np(
            dir.as_fd().as_raw_fd(),
            from.as_ptr(),
            dir.as_fd().as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL as libc::c_uint,
        )
    };
    (result == 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(windows)]
fn rename_projection_noreplace_platform(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    rename_projection_between_noreplace(dir, from, dir, to)
}

#[cfg(windows)]
fn rename_projection_between_noreplace(
    source_dir: &Dir,
    from: &str,
    destination_dir: &Dir,
    to: &str,
) -> io::Result<()> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
    use cap_std::fs::OpenOptionsExt as _;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::fs::MetadataExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Wdk::Storage::FileSystem::{
        FileRenameInformation, NtSetInformationFile, FILE_RENAME_INFORMATION,
    };
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    fn valid_leaf(name: &str) -> bool {
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
    }

    if !valid_leaf(from) || !valid_leaf(to) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic no-replace rename requires relative leaf names",
        ));
    }

    // Open the source itself with DELETE access through the retained directory
    // capability. FileRenameInformation then renames that exact handle relative
    // to the retained destination directory. A filesystem that cannot provide
    // the primitive rejects this call before the live source name is retired.
    let mut options = CapOpenOptions::new();
    options
        .follow(FollowSymlinks::No)
        .access_mode(DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let source = source_dir.open_with(from, &options)?.into_std();
    let metadata = source.metadata()?;
    if !metadata.is_file()
        || metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic no-replace rename source is not a regular no-follow file",
        ));
    }

    let destination = OsStr::new(to).encode_wide().collect::<Vec<_>>();
    let destination_bytes = destination
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target name too long"))?;
    let information_length = std::mem::size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(destination_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target name too long"))?;
    let information_words = information_length.div_ceil(std::mem::size_of::<usize>());
    let information_length = u32::try_from(information_length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target name too long"))?;
    let destination_bytes = u32::try_from(destination_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target name too long"))?;
    let mut storage = vec![0_usize; information_words];
    let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let root = destination_dir.try_clone()?.into_std_file();
    let mut io_status = IO_STATUS_BLOCK::default();

    // FileRenameInformation with ReplaceIfExists false atomically fails when
    // the destination name is occupied. The usize-backed allocation aligns the
    // locked binding's variable-tail structure, and both the exact source and
    // retained destination-directory handles outlive the call.
    let status = unsafe {
        (*information).Anonymous.ReplaceIfExists = false;
        (*information).RootDirectory = root.as_raw_handle();
        (*information).FileNameLength = destination_bytes;
        std::ptr::copy_nonoverlapping(
            destination.as_ptr(),
            (*information).FileName.as_mut_ptr(),
            destination.len(),
        );
        NtSetInformationFile(
            source.as_raw_handle(),
            &mut io_status,
            information.cast(),
            information_length,
            FileRenameInformation,
        )
    };
    if status >= 0 {
        Ok(())
    } else {
        let error = unsafe { RtlNtStatusToDosError(status) };
        Err(io::Error::from_raw_os_error(error as i32))
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "android",
    windows
)))]
fn rename_projection_noreplace_platform(_dir: &Dir, _from: &str, _to: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-clobber projection publication is unsupported on this platform",
    ))
}

/// The single no-clobber publication of a graph-tree name. Every caller writes
/// an artifact the graph itself is the only authority for, so the atomic
/// primitive is the contract: there is no second copy to rebuild from, and a
/// two-step publication would leave a reserved-but-empty live name behind a
/// crash. A filesystem that cannot provide the primitive fails the write, on
/// every platform (`docs/storage-sync-contract.md` §2.10b).
fn rename_projection_noreplace(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    rename_projection_noreplace_platform(dir, from, to).map_err(|error| {
        projection_platform_error(
            PROJECTION_NOREPLACE_RENAME_OPERATION,
            &format!("{from:?} -> {to:?}"),
            error,
        )
    })
}

/// The Direct Files graph-text name transition: the exact-byte move protocol of
/// `tine_storage::DurableDirectoryPublication::move_exact_no_replace`, carried
/// by the graph tree's own no-clobber rename.
///
/// GH #466. v0.6.981 routed every Direct Files create, live-name retirement,
/// staged publication, recovery restore and recovery set-aside through the
/// storage crate's move, whose Android arm is hard-link-then-unlink — a
/// primitive the FUSE-backed shared storage a Direct Files graph lives in
/// refuses — so every Android save failed with `Permission denied (os error
/// 13)`. That crate's move is written for app-private sole-writer namespaces
/// (the storage-mode selectors, `durable_private_authority_directory`), where
/// hard links exist; the graph tree is never such a namespace. Its name
/// transitions use [`rename_projection_noreplace`], the primitive v0.6.98
/// shipped here on every target (I-16: `renameat2(RENAME_NOREPLACE)` through
/// the raw syscall on Linux and Android, `renameatx_np(RENAME_EXCL)` on Apple,
/// `FileRenameInformation` on Windows), which also names the refused call in
/// its receipt (I-9) instead of surfacing a bare errno.
///
/// Protocol: `from` must hold exactly `expected` (a staged or retired inode an
/// external writer replaced is a collision, never published); the rename never
/// replaces `to`; the parent barrier is required — the graph tree is the sole
/// authority for these bytes; `to` is re-read to prove what became visible.
/// `crate::model::tests::direct_files_graph_text_publication_uses_the_graph_tree_noreplace_rename`
/// pins every Direct Files site to this function.
fn move_graph_text_exact_no_replace(
    dir: &Dir,
    from: &str,
    to: &str,
    expected: &[u8],
) -> io::Result<()> {
    if read_projection_regular(dir, from)? != expected {
        return Err(graph_text_transition_byte_collision("source"));
    }
    rename_projection_noreplace(dir, from, to)?;
    sync_projection_directory(dir, 0, 1)?;
    if read_projection_regular(dir, to)? != expected {
        return Err(graph_text_transition_byte_collision("published"));
    }
    Ok(())
}

fn graph_text_transition_byte_collision(position: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("graph text name transition found different bytes at its {position} name"),
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_graph_text_noreplace(
    source_dir: &Dir,
    source: &str,
    destination_dir: &Dir,
    destination: &str,
) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd};

    let source = CString::new(source)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid source name"))?;
    let destination = CString::new(destination)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid destination name"))?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_dir.as_fd().as_raw_fd(),
            source.as_ptr(),
            destination_dir.as_fd().as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE as libc::c_uint,
        )
    };
    (result == 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn rename_graph_text_noreplace(
    source_dir: &Dir,
    source: &str,
    destination_dir: &Dir,
    destination: &str,
) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd};

    let source = CString::new(source)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid source name"))?;
    let destination = CString::new(destination)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid destination name"))?;
    let result = unsafe {
        libc::renameatx_np(
            source_dir.as_fd().as_raw_fd(),
            source.as_ptr(),
            destination_dir.as_fd().as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL as libc::c_uint,
        )
    };
    (result == 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(windows)]
fn rename_graph_text_noreplace(
    source_dir: &Dir,
    source: &str,
    destination_dir: &Dir,
    destination: &str,
) -> io::Result<()> {
    // Every other platform gives this function a real no-replace primitive
    // (`renameat2(RENAME_NOREPLACE)`, `renameatx_np(RENAME_EXCL)`). Windows used
    // `Dir::rename`, which cap-std implements with replace semantics — so the
    // one guarantee the name promises was the one Windows did not provide, and
    // an external file landing in the check-to-rename window was clobbered.
    //
    // `rename_projection_between_noreplace` is the same operation done properly:
    // it opens the source with DELETE access through the retained directory
    // capability and renames that exact handle with
    // `FileRenameInformation`/`ReplaceIfExists = FALSE`, rejecting filesystems
    // that cannot provide the primitive BEFORE the live source name is retired.
    // It already takes separate source and destination directories, so this is
    // the cross-directory case it was written for.
    rename_projection_between_noreplace(source_dir, source, destination_dir, destination)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "android",
    windows
)))]
fn rename_graph_text_noreplace(
    _source_dir: &Dir,
    _source: &str,
    _destination_dir: &Dir,
    _destination: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic capability-relative no-clobber move is unsupported on this platform",
    ))
}

/// The strict directory barrier for graph-tree artifacts the graph is the SOLE
/// authority for — conflict copies, trash, withdrawn bytes, assets. A barrier
/// the filesystem refuses for those is a real durability failure and stays fatal
/// on every platform, Android included.
///
/// One barrier, on the directory whose entries the operation changed; see
/// [`sync_projection_chain`] for why the ancestors take none.
fn sync_projection_chain_required(chain: &[Dir]) -> io::Result<()> {
    projection_directory_sync_hook(Path::new("."))?;
    sync_projection_chain(chain)
}

/// Make the directory-entry changes of one projection operation durable.
///
/// **Only the leaf is flushed**, because only the leaf's entry list changed:
/// the operation inserted, replaced or removed a name in `chain.last()`. An
/// ancestor is flushed by exactly one mechanism, and it is not this one —
/// [`create_projection_chain_component`] flushes the parent of every directory
/// Tine creates while building the chain, at the moment it creates it. An
/// ancestor that Tine did not create in this operation already had a durable
/// entry in *its* parent before the operation began, and no in-scope failure
/// (crash/power loss, torn write, disk error, sync-service delivery,
/// external-editor race, honest concurrent instance, honest multi-device
/// divergence, malformed imported content) can un-durable an entry that is
/// already on stable storage. Re-flushing it therefore defends nothing.
///
/// See `docs/storage-sync-contract.md` §2.10a-i, which carries the same
/// argument and the refusal scenario for the flushes this removed. Before the
/// 2026-08-26 chain-flush cut this walked the whole chain leaf-to-root, so a
/// two-deep page path paid three barriers per call and about twelve per
/// foreground save.
fn sync_projection_chain(chain: &[Dir]) -> io::Result<()> {
    let depth = chain.len();
    let Some(leaf) = chain.last() else {
        return Ok(());
    };
    sync_projection_directory(leaf, depth.saturating_sub(1), depth)
}

/// Create one missing component of a projection parent chain and make the new
/// directory's NAME durable in the parent that now holds it.
///
/// This is the *only* place a freshly created projection ancestor gets its
/// barrier, and it is what lets [`sync_projection_chain`] flush the
/// leaf alone: after this returns, the created entry is on stable storage, so a
/// crash between here and the operation's own barrier cannot lose the path the
/// operation is about to publish into.
/// `projection_producer_census::g_b_choke_helper_caller_counts_are_pinned`
/// pins this function's callers; do not create a chain component anywhere else.
fn create_projection_chain_component(parent: &Dir, component: &str) -> io::Result<()> {
    parent.create_dir(component)?;
    sync_projection_directory(parent, 0, 1)
}

/// The single place the projection leg calls the platform directory-flush
/// primitive. It names the operation and the chain position on failure — a bare
/// platform errno on a device receipt is not actionable — and is strict on every
/// platform (`docs/storage-sync-contract.md` §2.10a).
fn sync_projection_directory(dir: &Dir, index: usize, depth: usize) -> io::Result<()> {
    crate::durability_counters::note(crate::durability_counters::Barrier::Directory);
    tine_storage::sync_dir_required(dir).map_err(|error| {
        projection_platform_error(
            "fsync of the projection parent directory",
            &format!("chain depth {}/{depth}", index + 1),
            error,
        )
    })
}

#[cfg(unix)]
fn projection_dir_identity(dir: &Dir) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    let metadata = dir.try_clone()?.into_std_file().metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
fn canonical_projection_directory_resource_id(dir: &Dir) -> io::Result<ContentDigest> {
    let (device, inode) = projection_dir_identity(dir)?;
    let mut hasher = Sha256::new();
    hasher.update(b"tine/projection-directory-resource/v1\0unix-dev-inode\0");
    hasher.update(device.to_be_bytes());
    hasher.update(inode.to_be_bytes());
    Ok(ContentDigest::from_bytes(hasher.finalize().into()))
}

#[cfg(unix)]
fn projection_files_have_same_identity(left: &fs::File, right: &fs::File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok((left.dev(), left.ino()) == (right.dev(), right.ino()))
}

#[cfg(unix)]
fn canonical_projection_file_resource_id(file: &fs::File) -> io::Result<ContentDigest> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    let mut identity = [0_u8; 16];
    identity[..8].copy_from_slice(&metadata.dev().to_be_bytes());
    identity[8..].copy_from_slice(&metadata.ino().to_be_bytes());
    let mut hasher = Sha256::new();
    hasher.update(b"tine/projection-file-resource/v1\0unix-dev-inode\0");
    hasher.update(identity);
    Ok(ContentDigest::from_bytes(hasher.finalize().into()))
}

#[cfg(unix)]
pub(crate) fn canonical_graph_resource_id(dir: &Dir) -> io::Result<CanonicalGraphResourceId> {
    let (device, inode) = projection_dir_identity(dir)?;
    let mut identity = [0_u8; 16];
    identity[..8].copy_from_slice(&device.to_be_bytes());
    identity[8..].copy_from_slice(&inode.to_be_bytes());
    Ok(CanonicalGraphResourceId::from_capability_identity(
        b"unix-dev-inode",
        &identity,
    ))
}

#[cfg(windows)]
fn projection_dir_identity(dir: &Dir) -> io::Result<(u64, [u8; 16])> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let file = dir.try_clone()?.into_std_file();
    let mut information = FILE_ID_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut information as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        information.VolumeSerialNumber,
        information.FileId.Identifier,
    ))
}

#[cfg(windows)]
fn canonical_projection_directory_resource_id(dir: &Dir) -> io::Result<ContentDigest> {
    let (volume, file_id) = projection_dir_identity(dir)?;
    let mut hasher = Sha256::new();
    hasher.update(b"tine/projection-directory-resource/v1\0windows-volume-file-id\0");
    hasher.update(volume.to_be_bytes());
    hasher.update(file_id);
    Ok(ContentDigest::from_bytes(hasher.finalize().into()))
}

#[cfg(windows)]
fn projection_files_have_same_identity(left: &fs::File, right: &fs::File) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    fn identity(file: &fs::File) -> io::Result<(u64, [u8; 16])> {
        let mut information = FILE_ID_INFO::default();
        let result = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&mut information as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((
            information.VolumeSerialNumber,
            information.FileId.Identifier,
        ))
    }

    Ok(identity(left)? == identity(right)?)
}

#[cfg(windows)]
fn canonical_projection_file_resource_id(file: &fs::File) -> io::Result<ContentDigest> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let mut information = FILE_ID_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut information as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut hasher = Sha256::new();
    hasher.update(b"tine/projection-file-resource/v1\0windows-volume-file-id\0");
    hasher.update(information.VolumeSerialNumber.to_be_bytes());
    hasher.update(information.FileId.Identifier);
    Ok(ContentDigest::from_bytes(hasher.finalize().into()))
}

#[cfg(windows)]
pub(crate) fn canonical_graph_resource_id(dir: &Dir) -> io::Result<CanonicalGraphResourceId> {
    let (volume, file_id) = projection_dir_identity(dir)?;
    let mut identity = [0_u8; 24];
    identity[..8].copy_from_slice(&volume.to_be_bytes());
    identity[8..].copy_from_slice(&file_id);
    Ok(CanonicalGraphResourceId::from_capability_identity(
        b"windows-volume-file-id",
        &identity,
    ))
}

#[cfg(not(any(unix, windows)))]
fn projection_dir_identity(_dir: &Dir) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "projection directory identity is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn canonical_projection_directory_resource_id(_dir: &Dir) -> io::Result<ContentDigest> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "projection directory identity is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn projection_files_have_same_identity(_left: &fs::File, _right: &fs::File) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "projection file identity is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn canonical_projection_file_resource_id(_file: &fs::File) -> io::Result<ContentDigest> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "projection file identity is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn canonical_graph_resource_id(_dir: &Dir) -> io::Result<CanonicalGraphResourceId> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "canonical graph resource identity is unsupported on this platform",
    ))
}

/// Like [`atomic_write`] but the payload is COPIED from `src` (so a large import —
/// a PDF, a big image — isn't slurped fully into memory): copy into a unique temp
/// in the destination dir, fsync it, then atomically rename into place. The temp
/// is removed on any failure, and the directory entry is fsynced on success. The
/// temp name is hidden (`.`-prefixed) so the orphan-asset scanner never lists it.
pub fn atomic_copy(src: &Path, dst: &Path) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let fname = dst.file_name().and_then(|s| s.to_str()).unwrap_or("asset");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{fname}.{}.{seq}.import.tmp", std::process::id()));
    let res = (|| {
        let mut input = fs::File::open(src)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        std::io::copy(&mut input, &mut output)?;
        barrier_sync_all(&output)?;
        drop(output);
        fs::rename(&tmp, dst)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
        return res;
    }
    sync_dir(dir)
}

/// Copy into a newly-created destination without replacing a path that appeared
/// concurrently. Used by restore after the previous live inode has been moved to
/// recovery: a sync writer that recreates the live name wins and the restore
/// aborts instead of clobbering it.
pub fn atomic_copy_new(src: &Path, dst: &Path) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let fname = dst.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{fname}.{}.{}.restore.tmp",
        std::process::id(),
        seq
    ));
    let res = (|| {
        let mut input = fs::File::open(src)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        std::io::copy(&mut input, &mut output)?;
        barrier_sync_all(&output)?;
        drop(output);
        move_file_noreplace(&tmp, dst)?;
        sync_dir(dir)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// Copy from an already-open source capability into a new destination while
/// enforcing a byte ceiling during the stream. This is the native-capture path:
/// it avoids reopening an attacker-replaceable pathname and avoids whole-value
/// Android/IPC/base64 amplification.
pub fn atomic_copy_file_new(input: &mut fs::File, dst: &Path, max_bytes: u64) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let fname = dst.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{fname}.{}.{}.capture.tmp",
        std::process::id(),
        seq
    ));
    let res = (|| {
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        let mut limited = input.take(max_bytes.saturating_add(1));
        let copied = io::copy(&mut limited, &mut output)?;
        if copied > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("capture exceeds {max_bytes} byte limit"),
            ));
        }
        barrier_sync_all(&output)?;
        drop(output);
        move_file_noreplace(&tmp, dst)?;
        sync_dir(dir)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// Read–modify–write a small text file (config.edn, device settings) under a lock,
/// committed via [`atomic_write`]. The ONE guarded path every settings writer goes
/// through, so the discipline is uniform rather than re-derived per call site:
///   - a MISSING file is the empty document `{}`, but any OTHER read error
///     (permission, NFS stale handle, transient I/O) ABORTS — otherwise `edit` would
///     rebuild the whole file from `{}` and destroy every other key (audit H2);
///   - the `lock` serializes concurrent writers to the same logical file so a
///     read-modify-write can't clobber a concurrent one (audit M1/M2);
///   - `edit` returns the new full contents, or an `Err` to abort without writing;
///   - the commit is atomic (temp + fsync + rename), so a crash can't truncate it.
pub fn atomic_update(
    path: &Path,
    lock: &std::sync::Mutex<()>,
    edit: impl Fn(&str) -> io::Result<String>,
) -> io::Result<()> {
    atomic_update_with_hooks(path, lock, edit, |_| {}, |_| {})
}

/// Read-modify-write one small app-private authority file through the typed
/// durable directory-publication boundary.
///
/// This is deliberately narrower than [`atomic_update`]: callers must own a
/// private single-writer namespace rather than a graph file that Logseq,
/// Syncthing, or an external editor may also change. The typed publication is
/// required because these files select which storage authority Tine serves;
/// on Windows their create/replace acknowledgement therefore uses certified
/// write-through name operations rather than a rename followed by an
/// unavailable directory fsync.
pub fn durable_private_authority_update(
    path: &Path,
    lock: &std::sync::Mutex<()>,
    edit: impl Fn(&str) -> io::Result<String>,
) -> io::Result<()> {
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for _attempt in 0..4 {
        let baseline = match fs::read_to_string(path) {
            Ok(value) => Some(value),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let next = edit(baseline.as_deref().unwrap_or("{}\n"))?;
        let current = match fs::read_to_string(path) {
            Ok(value) => Some(value),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if current != baseline {
            continue;
        }
        let (directory, filename) = durable_private_authority_directory(path)?;
        let publication = DurableDirectoryPublication::open(&directory)
            .map_err(graph_text_trash_filesystem_error)?;
        let published = match baseline.as_deref() {
            None => publication.publish_new_exact_single_writer(&filename, next.as_bytes()),
            Some(expected) => {
                publication.replace_exact(&filename, expected.as_bytes(), next.as_bytes())
            }
        };
        match published {
            Ok(()) => return Ok(()),
            Err(FilesystemError::ByteCollision) => continue,
            Err(error) => return Err(graph_text_trash_filesystem_error(error)),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "private authority changed repeatedly during update",
    ))
}

/// Durably remove one app-private authority name by first moving its exact
/// bytes to a fresh same-directory name outside the selector grammar.
///
/// A crash after the typed retirement can leave only inert recovery residue;
/// it cannot resurrect the active selector name. Ordinary completion removes
/// that residue immediately.
pub fn durable_private_authority_retire(
    path: &Path,
    lock: &std::sync::Mutex<()>,
) -> io::Result<()> {
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let expected = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let (directory, filename) = durable_private_authority_directory(path)?;
    let publication =
        DurableDirectoryPublication::open(&directory).map_err(graph_text_trash_filesystem_error)?;
    let retired = format!(".{filename}.retired-{}", Uuid::new_v4().simple());
    publication
        .retire_exact(&filename, &retired, &expected)
        .map_err(graph_text_trash_filesystem_error)?;
    let _ = directory.remove_file(&retired);
    Ok(())
}

fn durable_private_authority_directory(path: &Path) -> io::Result<(Dir, String)> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "private authority has no parent",
        )
    })?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "private authority has no safe filename",
            )
        })?
        .to_owned();

    let mut missing = Vec::new();
    let mut cursor = parent;
    while !cursor.exists() {
        missing.push(cursor.to_path_buf());
        cursor = cursor.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "private authority parent has no existing ancestor",
            )
        })?;
    }
    fs::create_dir_all(parent)?;
    // File publication below flushes `parent` itself. Flush every newly
    // created directory's parent as well, so Android/Unix initial activation
    // cannot acknowledge a binding whose parent entry is still volatile.
    for directory in &missing {
        if let Some(created_parent) = directory.parent() {
            sync_dir_for_rename(created_parent)?;
        }
    }
    Dir::open_ambient_dir(parent, ambient_authority()).map(|directory| (directory, filename))
}

fn atomic_update_with_hooks(
    path: &Path,
    lock: &std::sync::Mutex<()>,
    edit: impl Fn(&str) -> io::Result<String>,
    before_recheck: impl Fn(usize),
    before_publish: impl Fn(usize),
) -> io::Result<()> {
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for attempt in 0..4 {
        let baseline = match fs::read_to_string(path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let next = edit(baseline.as_deref().unwrap_or("{}\n"))?;
        // CONFIG_LOCK serializes Tine writers, but Logseq/Syncthing do not take
        // it. Re-read immediately before publish and retry the key-local edit on
        // their new bytes instead of overwriting an external update with our stale
        // full-file copy.
        before_recheck(attempt);
        let current = match fs::read_to_string(path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if current != baseline {
            continue;
        }
        before_publish(attempt);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match baseline.as_deref() {
            // Creation is already no-clobber: `create_new` + no-replace rename.
            None => match atomic_write_new(path, next.as_bytes()) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            },
            // Update: the recheck above narrows the race but cannot close it -
            // an external writer can still land between it and the rename. Make
            // the publish itself conditional so their bytes cannot be lost.
            Some(current) => {
                match atomic_replace_expected(path, current.as_bytes(), next.as_bytes())? {
                    AtomicReplaceOutcome::Published => return Ok(()),
                    AtomicReplaceOutcome::ExternalChanged => continue,
                }
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "config changed repeatedly during update",
    ))
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
