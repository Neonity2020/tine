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

mod asset_files;
mod asset_refs;
mod asset_reserve;
mod assets;
mod atomic_fs;
mod block_dto;
mod conflicts;
mod derived_cache;
mod direct_query;
mod dto;
mod editor_activation;
mod editor_types;
mod graph_dir;
mod graph_text_admission;
mod graph_text_identity;
mod graph_text_inventory;
mod graph_text_scope;
mod graph_text_sources;
mod graph_text_state;
mod graph_text_targets;
mod graph_text_writes;
mod journals;
mod lookup;
mod open_graph;
mod page_cache;
mod page_cache_index;
mod page_header;
mod page_inventory;
mod page_parse;
mod page_rename;
mod pages_merge;
mod paths;
mod pdf;
mod persistent_map;
mod projection_fs;
pub use atomic_fs::*;
pub(crate) use projection_fs::*;
mod trash;
use asset_files::*;
use asset_refs::*;
use asset_reserve::*;
pub use block_dto::*;
pub use derived_cache::*;
pub use editor_types::*;
use graph_dir::*;
pub use graph_text_state::*;
pub(crate) use page_cache_index::*;
use page_header::*;
pub(crate) use page_parse::*;
use trash::*;
mod write_gate;
pub use dto::*;
use write_gate::*;
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
