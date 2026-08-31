//! The one Tine-managed-storage journey the Android instrumentation runs.
//!
//! It lives here, in the crate both boundaries link, so the host test and the
//! device instrumentation drive the SAME fixture and the SAME call sequence.
//! When they were two hand-maintained copies they diverged, and the divergence
//! was invisible: `ManagedStorageSmokeTest` was green on CI in the same round a
//! physical device flooded the app with
//! `clean external reconciliation failed during Planning: decoded destination
//! logical page name … is already owned by page <uuid>` — because the journey
//! activated, saved, reopened and shared, and never once drove an external
//! change through reconciliation planning (GH: Android, 2026-08-18).
//!
//! ## What this journey does and does not prove
//!
//! It exercises the NATIVE runtime as the app's UID against a real graph tree:
//! activation, an application save, a force-close reopen, clean external
//! reconciliation of changes another writer made under the graph root, the
//! shared-enrollment cut, a second-device join and reopen, and clean shutdown.
//! The Android shim follows it with Tauri's real graceful Return-to-Direct-
//! Files composition; host coverage for that app layer lives beside the
//! composition rather than being duplicated here.
//!
//! It is deliberately NOT app coverage. There is no WebView, no Tauri command
//! layer, no watcher thread, and no UI: watcher observations are delivered by
//! this code rather than by inotify/`FileObserver`. It is also ONE fixture of a
//! few dozen pages, not corpus scale — a shape gate, not a load gate. A green
//! receipt means the native call sequence works on these shapes at this size,
//! and nothing more.

use std::{
    collections::BTreeMap,
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use crate::graph_name_folding::{graph_name_folding, GraphNameFolding};
use crate::oplog::{DeviceId, ProjectionEndpointId, SessionId};
use crate::sync_runtime::{
    SyncApplicationPageInventoryOutcome, SyncApplicationPageLoadOutcome,
    SyncApplicationPageLoadRequest, SyncApplicationPageSaveOutcome, SyncApplicationPageSaveRequest,
    SyncApplicationPageSaveTarget, SyncApplicationPageSelector, SyncLocalActivationIdentities,
    SyncLocalActivationRequest, SyncLocalActivationStatus, SyncRuntimeHandle,
    SyncRuntimeOpenRequest, SyncRuntimeOpenStatus, SyncRuntimeTick, SyncSharedEnrollmentDescriptor,
    SyncShutdownOutcome, SyncWatcherObservation,
};
use uuid::Uuid;

/// The page the journey edits through the application save path.
pub const JOURNEY_EDITED_PAGE: &str = "pages/Smoke.md";
/// What that save must leave on disk.
pub const JOURNEY_EDITED_BYTES: &str = "- Android managed storage edited\n";

/// Page names an ordinary human graph contains and a synthetic corpus does not.
///
/// Every shape here has cost this project a real defect or is one edit away
/// from doing so: a non-ASCII letter, the same letter precomposed AND
/// decomposed, an inline `#hashtag` inside a page name, spaces, one title
/// spelled two ways on disk (`#` literal versus the `%23` Tine's own filename
/// encoder writes), and two file names that differ only by case. Synthetic
/// corpora keep missing exactly these, which is why the shapes are pinned in
/// code rather than left to whoever writes the next fixture.
///
/// Content is synthetic. Only the SHAPES come from the field report.
const JOURNEY_NAME_SHAPES: &[(&str, &str)] = &[
    // Non-ASCII (precomposed U+017D), spaces, and an inline hashtag in the name.
    (
        "pages/\u{17d} pilot notes #pilot.md",
        "- precomposed pilot notes\n",
    ),
    // The same name decomposed (Z + U+030C): one portable path identity, two
    // exact spellings — what syncing a graph between a decomposing and a
    // precomposing filesystem produces.
    (
        "pages/Z\u{30c} pilot notes #pilot.md",
        "- decomposed pilot notes\n",
    ),
    // The same title again with the hashtag percent-encoded, which is what
    // `encode_page_name` writes for `#`. Same canonical page name, different
    // portable path.
    (
        "pages/\u{17d} pilot notes %23pilot.md",
        "- encoded pilot notes\n",
    ),
    // Two file names that differ only by case.
    (
        "pages/K\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md",
        "- horse\n",
    ),
    (
        "pages/k\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md",
        "- lowercase horse\n",
    ),
    // An ordinary non-ASCII page with no twin, edited externally later.
    (JOURNEY_EXTERNAL_EDIT_PAGE, JOURNEY_EXTERNAL_EDIT_BEFORE),
    // A nested layout with a space in a directory component.
    (
        "notes/archiv 2026/Star\u{fd} z\u{e1}pis.md",
        "- archived note\n",
    ),
];

/// What an outside writer does to the graph while Tine holds it, driving the
/// clean external reconciliation leg.
const JOURNEY_EXTERNAL_WRITES: &[(&str, &str)] = &[
    // A brand-new page from another editor or a filesystem sync provider.
    (
        JOURNEY_EXTERNAL_CREATED_PAGE,
        "- written by another editor\n",
    ),
    // An ordinary offline edit to an existing non-ASCII page.
    (JOURNEY_EXTERNAL_EDIT_PAGE, JOURNEY_EXTERNAL_EDIT_AFTER),
    // An honest backup copy: a second physical file whose decoded page name is
    // already owned. This is the reported refusal, arriving the ordinary way.
    (JOURNEY_EXTERNAL_BACKUP_PAGE, JOURNEY_EXTERNAL_BACKUP_BYTES),
];

/// The page the outside writer EDITS.
///
/// It is a member of [`JOURNEY_NAME_SHAPES`], so the leg is only an edit when
/// the journey's own fixture is on disk. Driven against a graph that lacks the
/// fixture the same write becomes a CREATE, the backup copy under `archiv/`
/// sorts earlier and wins the decoded name, and the miss surfaces far from its
/// cause as `external edit did not reconcile: Missing`. The journey proves the
/// precondition instead of assuming it.
pub const JOURNEY_EXTERNAL_EDIT_PAGE: &str = "pages/Denn\u{ed} pozn\u{e1}mky.md";
/// What that page holds before the outside writer touches it.
pub const JOURNEY_EXTERNAL_EDIT_BEFORE: &str = "- ordinary non-ASCII page\n";
/// What the outside writer leaves there.
pub const JOURNEY_EXTERNAL_EDIT_AFTER: &str = "- ordinary non-ASCII page\n- edited outside Tine\n";
/// The block the external edit appends, as the editor surfaces it.
pub const JOURNEY_EXTERNAL_EDIT_BLOCK: &str = "edited outside Tine";
/// The honest duplicate-name file that sorts before the authoritative owner.
pub const JOURNEY_EXTERNAL_BACKUP_PAGE: &str = "archiv/2026/Denn\u{ed} pozn\u{e1}mky.md";
/// The duplicate file is intentionally semantically different and unowned.
pub const JOURNEY_EXTERNAL_BACKUP_BYTES: &str = "- backup copy\n";
/// The page the outside writer CREATES.
pub const JOURNEY_EXTERNAL_CREATED_PAGE: &str = "pages/Extern\u{ed} novinka.md";
/// The block that created page carries, as the editor surfaces it.
pub const JOURNEY_EXTERNAL_CREATED_BLOCK: &str = "written by another editor";

/// The upper-case member of the fixture's case pair — the one that is written
/// FIRST, so it is the spelling a case-preserving-but-folding filesystem keeps
/// as the directory entry, and the one that owns the decoded page name in plain
/// byte order everywhere else (`K` sorts before `k`).
pub const JOURNEY_CASE_OWNER_PAGE: &str = "pages/K\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md";
/// Its lower-case twin: a second physical file on a filesystem that holds the
/// two apart, and no file at all on one that folds case.
pub const JOURNEY_CASE_TWIN_PAGE: &str = "pages/k\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md";
/// What an outside writer leaves at the twin spelling after activation.
///
/// This is the leg that separates the two filesystem classes at the PRODUCT
/// boundary rather than at the fixture boundary. Off a folding filesystem it is
/// a create whose decoded page name is already owned — the shape that flooded
/// the device — and it must be admitted and left on disk without a page. On a
/// folding filesystem the same write IS the owner's file, so it is an ordinary
/// external edit and the one page's content changes.
pub const JOURNEY_CASE_TWIN_AFTER: &str = "- lowercase horse, edited outside Tine\n";

/// Write one graph file and make it durable before the runtime can observe it.
///
/// A plain `fs::write` leaves the bytes in the page cache and the new directory
/// entry unsynced. That is invisible on a host, and it is exactly the ambiguity
/// a device run cannot afford: "the fixture was still landing when activation
/// started" has to be excluded by construction rather than argued about, since
/// activation refuses (`Retryable`) if the graph moves under it.
fn write_journey_file(target: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
        let mut file = fs::File::create(target)?;
        file.write_all(bytes)?;
        crate::durability_counters::sync_file(&file)?;
        sync_journey_directory(parent)
    } else {
        let mut file = fs::File::create(target)?;
        file.write_all(bytes)?;
        crate::durability_counters::sync_file(&file)
    }
}

#[cfg(unix)]
fn sync_journey_directory(directory: &Path) -> io::Result<()> {
    crate::durability_counters::sync_directory(&fs::File::open(directory)?)
}

#[cfg(not(unix))]
fn sync_journey_directory(_directory: &Path) -> io::Result<()> {
    Ok(())
}

/// Render a graph-relative path so two spellings cannot read as one.
///
/// `pages/\u{17d} pilot notes #pilot.md` and `pages/Z\u{30c} pilot notes #pilot.md`
/// are different files that print as the same glyph sequence everywhere. A
/// refusal about one of them is unreadable unless the non-ASCII is escaped.
fn escape_journey_path(path: &str) -> String {
    if path.is_ascii() {
        return path.to_owned();
    }
    let mut rendered = String::with_capacity(path.len());
    for character in path.chars() {
        if character.is_ascii() {
            rendered.push(character);
        } else {
            rendered.push_str(&format!("\\u{{{:x}}}", character as u32));
        }
    }
    rendered
}

/// The graph tree the journey INTENDS, expressed as the filesystem will hold it.
///
/// [`JOURNEY_NAME_SHAPES`] deliberately contains two pairs of names that a
/// filesystem may or may not treat as distinct: one pair differing only by
/// Unicode normalization, one only by case. Android shared storage folds the
/// case pair (CI 32123012366), so the tree the journey writes there is one file
/// short of the tree it asked for, and that file holds the LAST write's bytes.
///
/// This type is the journey's model of that. Every intended write is `place`d,
/// which folds the path the way the detected filesystem does and answers the
/// name the bytes actually land under. The journey then asserts against the
/// model rather than against the wish, so the same code proves the same product
/// contract on both filesystem classes instead of refusing to run on one of
/// them.
#[derive(Clone, Debug)]
pub struct JourneyGraphTree {
    folding: GraphNameFolding,
    /// Folded path -> (the spelling that is actually the directory entry, bytes).
    files: BTreeMap<String, (String, Vec<u8>)>,
}

impl JourneyGraphTree {
    fn new(folding: GraphNameFolding) -> Self {
        Self {
            folding,
            files: BTreeMap::new(),
        }
    }

    /// Record one intended write and answer where it really lands.
    ///
    /// A case-folding filesystem is case-PRESERVING: the directory entry keeps
    /// the spelling of whoever created it first, and every later write to a
    /// folded spelling replaces that file's bytes under the original name. So
    /// the first `place` for a folded key fixes the name and each later one
    /// replaces the bytes — exactly what the device reported, where the
    /// upper-case name read back the lower-case content.
    fn place(&mut self, path: &str, bytes: &[u8]) -> String {
        let key = self.folding.effective_path(path);
        let entry = self
            .files
            .entry(key)
            .or_insert_with(|| (path.to_owned(), Vec::new()));
        entry.1 = bytes.to_vec();
        entry.0.clone()
    }

    /// The bytes this filesystem holds for a path, under whatever spelling.
    #[must_use]
    pub fn bytes_at(&self, path: &str) -> Option<&[u8]> {
        self.files
            .get(&self.folding.effective_path(path))
            .map(|(_, bytes)| bytes.as_slice())
    }

    /// Does this exact spelling have a file of its own here?
    ///
    /// False for the folded-away twin, and that single difference is what the
    /// journey's folding leg turns into a product assertion.
    #[must_use]
    pub fn has_its_own_file(&self, path: &str) -> bool {
        self.files
            .get(&self.folding.effective_path(path))
            .is_some_and(|(name, _)| name == path)
    }

    /// What this filesystem folds. Carried verbatim into the receipt.
    #[must_use]
    pub const fn folding(&self) -> GraphNameFolding {
        self.folding
    }

    /// The first block of a page here, as the editor surfaces it.
    #[must_use]
    pub fn first_block_at(&self, path: &str) -> Option<String> {
        let text = std::str::from_utf8(self.bytes_at(path)?).ok()?;
        Some(
            text.lines()
                .next()?
                .trim_start_matches("- ")
                .trim()
                .to_owned(),
        )
    }
}

/// The fixture's intended writes, replayed against a filesystem that folds like
/// this one.
///
/// Pure, so the writer and the runner derive the same tree independently and
/// nothing has to be carried across the JNI boundary.
#[must_use]
pub fn journey_graph_tree(folding: GraphNameFolding) -> JourneyGraphTree {
    let mut tree = JourneyGraphTree::new(folding);
    for (path, bytes) in JOURNEY_FIXTURE_PRELUDE {
        tree.place(path, bytes.as_bytes());
    }
    for (path, bytes) in JOURNEY_NAME_SHAPES {
        tree.place(path, bytes.as_bytes());
    }
    tree
}

/// The same replay, continued through the outside writer's changes.
#[must_use]
pub fn journey_graph_tree_after_external_writes(folding: GraphNameFolding) -> JourneyGraphTree {
    let mut tree = journey_graph_tree(folding);
    for (path, bytes) in journey_external_writes() {
        tree.place(path, bytes.as_bytes());
    }
    tree
}

/// The graph files that carry no name-shape argument: config, the edited page,
/// and one journal that references a shape by `[[…]]` and by `#hashtag`.
const JOURNEY_FIXTURE_PRELUDE: &[(&str, &str)] = &[
    ("logseq/config.edn", "{}\n"),
    (JOURNEY_EDITED_PAGE, "- Android managed storage smoke\n"),
    (
        "journals/2026_08_18.md",
        "- journal entry with #pilot and [[\u{17d} pilot notes #pilot]]\n",
    ),
];

/// Everything the outside writer does, in order — the three ordinary changes
/// plus the case twin, which is last because it is the one write whose MEANING
/// depends on the filesystem.
fn journey_external_writes() -> impl Iterator<Item = (&'static str, &'static str)> {
    JOURNEY_EXTERNAL_WRITES
        .iter()
        .map(|(path, bytes)| (*path, *bytes))
        .chain(std::iter::once((
            JOURNEY_CASE_TWIN_PAGE,
            JOURNEY_CASE_TWIN_AFTER,
        )))
}

/// Prove the fixture actually landed as the fixture, before anything reads it.
///
/// The check is against the MODEL above, not against the wish list: on a
/// filesystem whose probe says it folds the case pair, one file short IS the
/// correct tree and is not an error.
///
/// What stays an error — and this is the half that must not be weakened — is a
/// fold the probe did NOT predict. That is a filesystem we have mis-modelled,
/// and everything downstream would then be attributed to the wrong cause: it
/// surfaced on Android as `source capture changed before final inventory proof
/// … content:<26 bytes> -> content:<25 bytes>` on a path that printed
/// identically on both sides, and reading it as content normalization was wrong
/// (the 25 bytes are the sibling shape's, verbatim).
fn verify_journey_graph_fixture(graph_root: &Path, tree: &JourneyGraphTree) -> io::Result<()> {
    for (path, expected) in tree.files.values() {
        let actual = fs::read(graph_root.join(path))?;
        if actual == *expected {
            continue;
        }
        let folded = tree
            .files
            .values()
            .find(|(other, other_bytes)| other != path && actual == *other_bytes)
            .map(|(other, _)| other.as_str())
            .or_else(|| {
                JOURNEY_NAME_SHAPES
                    .iter()
                    .find(|(other, other_bytes)| other != path && actual == other_bytes.as_bytes())
                    .map(|(other, _)| *other)
            });
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            match folded {
                Some(other) => format!(
                    "graph filesystem folds two journey page names into one file and the \
                     name-folding probe did not predict it (probe answered {}): {} reads back \
                     the bytes written for {} ({} bytes, not {}) — the two names differ only by \
                     Unicode normalization or by case",
                    tree.folding.diagnostic(),
                    escape_journey_path(path),
                    escape_journey_path(other),
                    actual.len(),
                    expected.len()
                ),
                None => format!(
                    "journey fixture shape {} reads back {} bytes, not the {} written for it",
                    escape_journey_path(path),
                    actual.len(),
                    expected.len()
                ),
            },
        ));
    }
    Ok(())
}

/// Write the journey's graph tree. Callers own the (empty) `graph_root`.
///
/// The filesystem is asked what it folds BEFORE anything is written, so the
/// tree written is one this filesystem can hold and the verification knows
/// which files to expect. The probe answer is memoized per device, so this
/// costs a handful of writes once per filesystem rather than once per graph.
pub fn write_journey_graph_fixture(graph_root: &Path) -> io::Result<()> {
    fs::create_dir_all(graph_root.join("pages"))?;
    fs::create_dir_all(graph_root.join("journals"))?;
    fs::create_dir_all(graph_root.join("logseq"))?;
    let mut tree = JourneyGraphTree::new(graph_name_folding(graph_root));
    for (path, bytes) in JOURNEY_FIXTURE_PRELUDE
        .iter()
        .chain(JOURNEY_NAME_SHAPES.iter())
    {
        // `place` answers the name this filesystem really stores the bytes
        // under. On a folding filesystem that redirects a twin onto its
        // survivor — which is what the filesystem would do to the write anyway
        // — and off one it is always the path itself. Writing through it keeps
        // host and device driving one code path instead of branching on the
        // platform.
        let landed = tree.place(path, bytes.as_bytes());
        write_journey_file(&graph_root.join(landed), bytes.as_bytes())?;
    }
    sync_journey_directory(graph_root)?;
    verify_journey_graph_fixture(graph_root, &tree)
}

/// Apply the outside writer's changes. Separated so the caller can prove the
/// runtime was already live when they landed.
///
/// The last write is the case twin, and it is what makes this journey cover
/// BOTH filesystem classes: off a folding filesystem it is a create for an
/// already-owned page name — the shape that flooded Martin's device — and on
/// one it is an ordinary external edit of the single file the pair shares.
fn apply_journey_external_writes(
    graph_root: &Path,
    tree: &mut JourneyGraphTree,
) -> io::Result<Vec<String>> {
    let mut written = Vec::new();
    for (path, bytes) in journey_external_writes() {
        let landed = tree.place(path, bytes.as_bytes());
        write_journey_file(&graph_root.join(&landed), bytes.as_bytes())?;
        written.push(landed);
    }
    sync_journey_directory(graph_root)?;
    Ok(written)
}

/// The journey drives reconciliation by hand, so it must offer the runtime as
/// many turns as the graph it just changed needs.
///
/// A fixed 64 was below what the graph the device journey actually holds
/// requires: at the host boundary the same 1104-page corpus settled on tick 71,
/// so a correct runtime would have been reported as
/// `external reconciliation never admitted an epoch`. Derive the bound from the
/// graph rather than tuning a number: every turn is a bounded slice, so the
/// count scales with the file set, and the drain still refuses rather than
/// waiting forever.
fn journey_reconciliation_tick_budget(graph_root: &Path) -> usize {
    const RESERVE: usize = 256;
    RESERVE.saturating_add(count_journey_graph_text_files(graph_root))
}

fn count_journey_graph_text_files(root: &Path) -> usize {
    let mut stack = vec![root.to_path_buf()];
    let mut files = 0_usize;
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(entry.path()),
                Ok(kind) if kind.is_file() => {
                    let text = name
                        .rsplit_once('.')
                        .map(|(_, extension)| extension)
                        .is_some_and(|extension| {
                            extension.eq_ignore_ascii_case("md")
                                || extension.eq_ignore_ascii_case("markdown")
                                || extension.eq_ignore_ascii_case("org")
                        });
                    if text {
                        files = files.saturating_add(1);
                    }
                }
                _ => {}
            }
        }
    }
    files
}

fn tick_is_settled(tick: &SyncRuntimeTick) -> bool {
    matches!(
        tick,
        SyncRuntimeTick::Idle
            | SyncRuntimeTick::AdmittedNoop { .. }
            | SyncRuntimeTick::AdmittedComplete { .. }
    )
}

fn tick_is_refusal(tick: &SyncRuntimeTick) -> bool {
    matches!(
        tick,
        SyncRuntimeTick::Failed(_)
            | SyncRuntimeTick::Blocked(_)
            | SyncRuntimeTick::RecoveryBlocked(_)
            | SyncRuntimeTick::Terminal(_)
    )
}

/// A bounded account of one reconciliation drain.
///
/// The whole tick sequence used to be carried verbatim, which is unreadable the
/// moment the budget scales with the graph. Keep the opening turns (where a
/// refusal or an unexpected state shows up), the last turn, and the totals.
struct ReconciliationCensus {
    budget: usize,
    ticks: usize,
    admitted: usize,
    opening: Vec<String>,
    last: String,
}

impl ReconciliationCensus {
    /// How many opening turns one census keeps verbatim.
    const OPENING_TURNS: usize = 8;

    fn new(budget: usize) -> Self {
        Self {
            budget,
            ticks: 0,
            admitted: 0,
            opening: Vec::new(),
            last: "none".to_owned(),
        }
    }

    fn record(&mut self, tick: &SyncRuntimeTick) {
        self.ticks = self.ticks.saturating_add(1);
        if matches!(
            tick,
            SyncRuntimeTick::AdmittedComplete { .. } | SyncRuntimeTick::AdmittedNoop { .. }
        ) {
            self.admitted = self.admitted.saturating_add(1);
        }
        self.last = format!("{tick:?}");
        if self.opening.len() < Self::OPENING_TURNS {
            self.opening.push(self.last.clone());
        }
    }
}

impl fmt::Display for ReconciliationCensus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "ticks={}/{} admitted={} last={} opening={}",
            self.ticks,
            self.budget,
            self.admitted,
            self.last,
            self.opening.join("|")
        )
    }
}

/// Drive reconciliation the way the platform watcher does, and report the exact
/// tick census. A refusal is returned as evidence rather than retried away: on
/// the device it repeated forever, one red toast at a time.
fn drain_external_reconciliation(
    handle: &SyncRuntimeHandle,
    budget: usize,
) -> Result<ReconciliationCensus, String> {
    handle
        .observe_watcher(vec![SyncWatcherObservation::RescanRequired])
        .map_err(|error| format!("watcher observation refused: {error}"))?;
    let mut census = ReconciliationCensus::new(budget);
    let mut quiet = false;
    for _ in 0..budget {
        let tick = handle
            .tick()
            .map_err(|error| format!("reconciliation tick refused: {error}; {census}"))?;
        let settled = tick_is_settled(&tick);
        census.record(&tick);
        if tick_is_refusal(&tick) {
            return Err(format!(
                "external reconciliation refused: {tick:?}; {census}"
            ));
        }
        let watcher = handle
            .status()
            .map_err(|error| {
                format!("status unavailable during reconciliation: {error}; {census}")
            })?
            .watcher;
        if settled && !watcher.pending && !watcher.drain_in_flight {
            quiet = true;
            break;
        }
    }
    // Both halves are load-bearing. Without `quiet` a drain that merely ran out
    // of turns reads as a clean drain; without `admitted` a drain that settled
    // without ever admitting an epoch does.
    if !quiet {
        return Err(format!(
            "external reconciliation did not settle within its budget; {census}"
        ));
    }
    if census.admitted == 0 {
        return Err(format!(
            "external reconciliation never admitted an epoch; {census}"
        ));
    }
    Ok(census)
}

/// How many times the journey re-drives an activation that refused `Retryable`.
///
/// A graph that moves under a live activation is an IN-SCOPE scenario, not a
/// harness artefact: an external editor, a filesystem sync provider, or the
/// user's own second window saving while Tine is still importing. The runtime
/// answers it by refusing `Retryable` and retracting the disposable archive that
/// attempt created, so the next attempt starts from the current Direct Files
/// bytes. A journey that reports the first refusal proves the refusal and
/// nothing about the recovery — which is how this journey spent three device
/// rounds red. A journey that retried SILENTLY would be worse: it would hide a
/// device that refuses on every attempt. So the retries are bounded, and every
/// refusal retried past is carried verbatim in the receipt as
/// `activation_retried=`.
const JOURNEY_ACTIVATION_ATTEMPTS: usize = 3;

/// The second device lives below the disposable journey roots so the Android
/// instrumentation's existing cleanup owns every byte it creates. The graph
/// directory is hidden from the initiator's graph scan but remains on the same
/// Android shared-storage filesystem and under the same app UID.
pub const JOURNEY_PEER_GRAPH_DIRECTORY: &str = ".tine-journey-peer";
pub const JOURNEY_PEER_PRIVATE_DIRECTORY: &str = "journey-peer";

#[derive(Clone, Debug)]
pub struct ManagedStorageJourneyPeer {
    pub graph_root: PathBuf,
    pub private_root: PathBuf,
    pub open_request: SyncRuntimeOpenRequest,
    pub activation_request: SyncLocalActivationRequest,
}

/// Derive the second-device paths and requests used by both host and Android.
///
/// The graph identity comes from the signed descriptor; endpoint, device,
/// preparation, and session identities are device-local and deliberately new.
#[must_use]
pub fn managed_storage_journey_peer(
    graph_root: &Path,
    private_root: &Path,
    descriptor: &SyncSharedEnrollmentDescriptor,
) -> ManagedStorageJourneyPeer {
    let peer_graph_root = graph_root.join(JOURNEY_PEER_GRAPH_DIRECTORY);
    let peer_private_root = private_root.join(JOURNEY_PEER_PRIVATE_DIRECTORY);
    let identities = SyncLocalActivationIdentities {
        workspace_id: descriptor.workspace_id,
        lineage_digest: descriptor.lineage_digest,
        catalog_document_id: descriptor.catalog_document_id,
        endpoint_id: ProjectionEndpointId::new(),
        device_id: DeviceId::new(),
        preparation_id: Uuid::new_v4(),
        session_id: SessionId::new(),
    };
    let open_request = SyncRuntimeOpenRequest {
        profile: crate::sync_runtime::SyncStorageProfile::ExperimentalLocal,
        clean_identities: Some(identities.clone()),
        graph_root: peer_graph_root.clone(),
        archive_root: peer_private_root.join("archive"),
        enrollment_root: peer_private_root.join("enrollment"),
        receipt_root: peer_private_root.join("receipts"),
        database_path: peer_private_root.join("projection/materialization.sqlite"),
        application_runtime_root: peer_private_root.join("runtime"),
        provider_root: peer_graph_root.join(".tine-sync/v2/shared"),
        provider_journal_root: peer_private_root.join("provider/device/journal"),
    };
    let activation_request = SyncLocalActivationRequest {
        graph_root: peer_graph_root.clone(),
        archive_root: open_request.archive_root.clone(),
        enrollment_root: open_request.enrollment_root.clone(),
        receipt_root: open_request.receipt_root.clone(),
        database_path: open_request.database_path.clone(),
        application_runtime_root: open_request.application_runtime_root.clone(),
        capture_root: peer_private_root.join("capture"),
        preparation_root: peer_private_root.join("preparation"),
        provider_root: open_request.provider_root.clone(),
        provider_journal_root: open_request.provider_journal_root.clone(),
        identities,
    };
    ManagedStorageJourneyPeer {
        graph_root: peer_graph_root,
        private_root: peer_private_root,
        open_request,
        activation_request,
    }
}

fn copy_journey_tree(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_name() == JOURNEY_PEER_GRAPH_DIRECTORY {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "journey graph contains a symlink at {}",
                    entry.path().display()
                ),
            ));
        }
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_journey_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            write_journey_file(&target, &fs::read(entry.path())?)?;
        }
    }
    sync_journey_directory(destination)
}

/// Run the whole journey and return its receipt. `Ok` receipts start with
/// `"ok "`; every other string is the refusal, carrying what localises it.
pub fn run_managed_storage_journey(
    graph_root: PathBuf,
    open_request: SyncRuntimeOpenRequest,
    activation_request: SyncLocalActivationRequest,
) -> String {
    let journey_started = Instant::now();
    // Ask the graph's filesystem what it folds BEFORE activation starts. The
    // probe writes and removes a hidden directory, which a live source capture
    // would report as the graph moving under it, so it can only run here. The
    // answer is memoized per device, so on the device the fixture writer has
    // already paid for it.
    let folding = graph_name_folding(&graph_root);
    let folding_receipt = format!("graph_name_folding={}", folding.diagnostic());
    let mut last_progress = "activation-not-started".to_string();
    let mut progress_receipt = Vec::new();
    let mut retried: Vec<String> = Vec::new();
    let mut attempts = 1_usize;
    let activation = loop {
        progress_receipt.clear();
        let result = SyncRuntimeHandle::activate_or_resume_local_with_detailed_progress(
            activation_request.clone(),
            |progress| {
                last_progress = format!("{progress:?}");
                progress_receipt.push(format!(
                    "{}@{}ms",
                    progress.diagnostic_name(),
                    journey_started.elapsed().as_millis()
                ));
            },
        );
        let retryable = matches!(result.status, SyncLocalActivationStatus::Retryable { .. });
        if !retryable || attempts >= JOURNEY_ACTIVATION_ATTEMPTS {
            break result;
        }
        retried.push(format!(
            "attempt {attempts} after {last_progress}: {:?}",
            result.status
        ));
        attempts = attempts.saturating_add(1);
    };
    let activation_ms = journey_started.elapsed().as_millis();
    let retried_receipt = if retried.is_empty() {
        "none".to_owned()
    } else {
        retried.join(" || ")
    };
    if activation.status != SyncLocalActivationStatus::Active {
        // The phase name alone does not say WHEN the phases ran, and the two
        // activation refusals this journey has produced were both about timing
        // under a graph that moved. Carry the per-phase millisecond receipt.
        return format!(
            "activation failed after {last_progress}: {:?}; activation_attempts={attempts}; activation_retried={retried_receipt}; activation_ms={activation_ms}; {folding_receipt}; progress={}",
            activation.status,
            progress_receipt.join("|")
        );
    }
    let Some(handle) = activation.handle else {
        return "activation returned Active without a handle".into();
    };
    let first_page_started = Instant::now();
    let (mut page, revision) = match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: JOURNEY_EDITED_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, revision }) => (page, revision),
        outcome => return format!("post-activation page load failed: {outcome:?}"),
    };
    let first_page_ms = first_page_started.elapsed().as_millis();
    let Some(first) = page.blocks.first_mut() else {
        return "post-activation page has no editable block".into();
    };
    first.raw = JOURNEY_EDITED_BYTES.trim_start_matches("- ").trim().into();
    let save_outcome = handle.save_application_page(SyncApplicationPageSaveRequest {
        target: SyncApplicationPageSaveTarget::Existing {
            path: page.path.clone(),
            revision,
        },
        page,
    });
    // A clean-runtime save can succeed only after settling a RETAINED
    // publication, and settling it consumes the failure that caused it. Read
    // the report either way: a converged retry still costs a retry on every
    // write, and a bare "ok" receipt would hide the underlying cause.
    let retained = match handle.last_retained_publication() {
        Ok(Some(report)) => format!(
            "retained_publication=batch:{} phase:{:?} settled:{} turns:{} detail:{}",
            report.batch_id, report.phase, report.settled, report.settle_turns, report.detail
        ),
        Ok(None) => "retained_publication=none".to_owned(),
        Err(error) => format!("retained_publication=unavailable:{error}"),
    };
    match save_outcome {
        Ok(SyncApplicationPageSaveOutcome::Saved { .. }) => {}
        // The instrumentation boundary can only report the returned value, so
        // carry everything that distinguishes one refusal from another: the
        // Display form (which names the stage and reason code), the debug
        // detail, and the runtime's own status.
        outcome => {
            let detail = match &outcome {
                Err(error) => format!(
                    "display={error}; debug_detail={:?}",
                    error.debug_detail().unwrap_or("none")
                ),
                Ok(_) => "not a refusal".to_owned(),
            };
            return format!(
                "post-activation page save failed: {outcome:?}; {detail}; {retained}; status={}; progress={}",
                describe_status(&handle),
                progress_receipt.join("|")
            );
        }
    }
    // Match a force-closed app: do not ask the actor for a clean drain.  Drop
    // the last sender, let the actor stop, and prove the exact durable edit can
    // be recovered by a fresh runtime open before testing sharing.
    drop(handle);

    let reopen_started = Instant::now();
    let crashed_reopen = SyncRuntimeHandle::open(open_request.clone());
    let crash_reopen_ms = reopen_started.elapsed().as_millis();
    if crashed_reopen.status != SyncRuntimeOpenStatus::Active {
        return format!("crash-style reopen failed: {:?}", crashed_reopen.status);
    }
    let Some(handle) = crashed_reopen.handle else {
        return "crash-style reopen returned Active without a handle".into();
    };
    match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: JOURNEY_EDITED_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. })
            if page.blocks.first().map(|block| block.raw.as_str())
                == Some(JOURNEY_EDITED_BYTES.trim_start_matches("- ").trim()) => {}
        outcome => return format!("crash-style reopened page mismatch: {outcome:?}"),
    }

    // The leg this journey was missing: another writer changes the graph tree
    // under a live managed runtime, and reconciliation must plan and apply it.
    //
    // Prove the leg's own precondition first. The edit target must ALREADY be a
    // page owning its decoded name, or the write below is a create racing the
    // `archiv/` backup copy for that name, and the miss surfaces later as an
    // uninterpretable `external edit did not reconcile: Missing`. That is
    // exactly how the resume instrumentation case failed while the runtime was
    // behaving as specified (CI 32108957903).
    match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: JOURNEY_EXTERNAL_EDIT_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. }) if page.blocks.len() == 1 => {}
        outcome => {
            let inventory = match handle.application_page_inventory() {
                Ok(SyncApplicationPageInventoryOutcome::Loaded { pages }) => {
                    let nearby = pages
                        .iter()
                        .filter(|page| page.rel_path.contains("Denn") || page.name.contains("Denn"))
                        .map(|page| {
                            format!(
                                "{}=>{} path_bytes={:?}",
                                page.rel_path,
                                page.name,
                                page.rel_path.as_bytes()
                            )
                        })
                        .collect::<Vec<_>>();
                    format!(
                        "count={} requested_bytes={:?} nearby={nearby:?}",
                        pages.len(),
                        JOURNEY_EXTERNAL_EDIT_PAGE.as_bytes()
                    )
                }
                other => format!("unavailable:{other:?}"),
            };
            return format!(
                "journey precondition failed: {JOURNEY_EXTERNAL_EDIT_PAGE} must already be an \
                 owned single-block page before the external edit — drive the journey against \
                 `write_journey_graph_fixture`: {outcome:?}; inventory={inventory}"
            );
        }
    }
    let reconciliation_started = Instant::now();
    let mut tree = journey_graph_tree(folding);
    let external = match apply_journey_external_writes(&graph_root, &mut tree) {
        Ok(written) => written,
        Err(error) => return format!("external write failed: {error}; {folding_receipt}"),
    };
    let reconciliation_budget = journey_reconciliation_tick_budget(&graph_root);
    let reconciliation = match drain_external_reconciliation(&handle, reconciliation_budget) {
        Ok(census) => census,
        Err(detail) => {
            return format!(
                "{detail}; external={}; status={}; progress={}",
                external.join(","),
                describe_status(&handle),
                progress_receipt.join("|")
            )
        }
    };
    let reconciliation_ms = reconciliation_started.elapsed().as_millis();
    // The externally created page is a page now, and the externally edited one
    // carries the outside edit.
    match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: JOURNEY_EXTERNAL_CREATED_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. })
            if page.blocks.first().map(|block| block.raw.as_str())
                == Some(JOURNEY_EXTERNAL_CREATED_BLOCK) => {}
        outcome => {
            return format!(
                "externally created page did not reconcile: {outcome:?}; {reconciliation}"
            )
        }
    }
    match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: JOURNEY_EXTERNAL_EDIT_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. })
            if page.blocks.len() == 2
                && page.blocks[1].raw.as_str() == JOURNEY_EXTERNAL_EDIT_BLOCK => {}
        outcome => {
            return format!("external edit did not reconcile: {outcome:?}; {reconciliation}")
        }
    }

    // The folding leg. One outside write, two correct outcomes, and which one
    // is correct is a property of the filesystem rather than of the runtime:
    //
    // * where the two spellings are two files, this was a CREATE whose decoded
    //   page name is already owned — the shape that flooded Martin's device.
    //   It must be admitted (the drain above already refused any refusal), the
    //   owner must be untouched, and the twin must stay on disk as ordinary
    //   graph text with no page of its own.
    // * where they are one file, this WAS the owner's file, so it is an
    //   ordinary external edit and the one page carries the new bytes.
    //
    // Nothing in this block branches on the platform: both expectations are
    // read out of the model of what the filesystem did.
    let Some(expected_owner_block) = tree.first_block_at(JOURNEY_CASE_OWNER_PAGE) else {
        return format!("journey model lost the case-pair owner page; {folding_receipt}");
    };
    match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: JOURNEY_CASE_OWNER_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. })
            if page.blocks.len() == 1
                && page.blocks[0].raw.as_str() == expected_owner_block => {}
        outcome => {
            return format!(
                "the case pair's owning page did not hold what this filesystem holds (expected one block {expected_owner_block:?}): {outcome:?}; {folding_receipt}; {reconciliation}"
            )
        }
    }
    // Never a SECOND page, on either filesystem. This is the assertion that
    // says Tine did not silently split one page in two, nor merge two into one:
    // there is exactly one page for this name, whatever the storage did with
    // the spellings.
    match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: JOURNEY_CASE_TWIN_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. }) => {
            return format!(
                "the case twin became a second page: path={} blocks={}; {folding_receipt}; {reconciliation}",
                escape_journey_path(page.path.as_str()),
                page.blocks.len()
            )
        }
        Err(error) => {
            return format!(
                "loading the case twin failed instead of reporting it is not a page: {error}; {folding_receipt}"
            )
        }
        Ok(_) => {}
    }
    // Write-shyness on the file Tine deliberately does not own: the outside
    // writer's exact bytes are still there, under whichever spelling this
    // filesystem stored them.
    let twin_expectation = tree.bytes_at(JOURNEY_CASE_TWIN_PAGE).unwrap_or_default();
    let twin_on_disk = if tree.has_its_own_file(JOURNEY_CASE_TWIN_PAGE) {
        JOURNEY_CASE_TWIN_PAGE
    } else {
        JOURNEY_CASE_OWNER_PAGE
    };
    match fs::read(graph_root.join(twin_on_disk)) {
        Ok(bytes) if bytes == twin_expectation => {}
        other => {
            return format!(
            "the outside writer's bytes at {} were not left alone: {other:?}; {folding_receipt}",
            escape_journey_path(twin_on_disk)
        )
        }
    }

    let descriptor = match handle.prepare_shared() {
        Ok(descriptor) => descriptor,
        Err(error) => return format!("prepare shared failed: {error}"),
    };
    let shared = format!("shared_descriptor={}", descriptor.descriptor_digest);
    // A successful enrollment cut retires the actor, so this shutdown reads
    // the runtime's own final state rather than reaching a live thread.
    // `ActorUnavailable` carries no payload at all, so never report a shutdown
    // refusal bare: `status=` distinguishes "the runtime stopped Safe and this
    // is merely a report of it" from "the snapshot itself is unreachable, so
    // the actor died some way the retirement contract does not describe".
    match handle.clean_shutdown() {
        Ok(SyncShutdownOutcome::Safe(_)) => {}
        outcome => {
            return format!(
                "clean shutdown failed: {outcome:?}; {shared}; status={}; {retained}; progress={}",
                describe_status(&handle),
                progress_receipt.join("|")
            )
        }
    }
    drop(handle);

    // Model the filesystem synchronizer between two devices by copying the
    // complete graph tree only after the initiator has durably prepared its
    // share. The peer then activates from the same Markdown/Org semantics with
    // distinct device-local identities, joins the provider-visible descriptor,
    // retires that enrollment epoch, and cold-reopens as a joiner.
    let Some(private_root) = activation_request.application_runtime_root.parent() else {
        return format!(
            "journey could not derive its private root from {}; {shared}",
            activation_request.application_runtime_root.display()
        );
    };
    let peer = managed_storage_journey_peer(&graph_root, private_root, &descriptor);
    let _ = fs::remove_dir_all(&peer.graph_root);
    let _ = fs::remove_dir_all(&peer.private_root);
    if let Err(error) = copy_journey_tree(&graph_root, &peer.graph_root) {
        return format!("second-device graph delivery failed: {error}; {shared}");
    }
    let join_started = Instant::now();
    let peer_activation = SyncRuntimeHandle::activate_or_resume_local(peer.activation_request);
    if peer_activation.status != SyncLocalActivationStatus::Active {
        return format!(
            "second-device activation failed: {:?}; {shared}",
            peer_activation.status
        );
    }
    let Some(peer_joining) = peer_activation.handle else {
        return "second-device activation returned Active without a handle".into();
    };
    if let Err(error) = peer_joining.join_shared(descriptor) {
        return format!("second-device join failed: {error}; {shared}");
    }
    for (path, expected) in [
        (JOURNEY_EXTERNAL_EDIT_PAGE, JOURNEY_EXTERNAL_EDIT_AFTER),
        (JOURNEY_EXTERNAL_BACKUP_PAGE, JOURNEY_EXTERNAL_BACKUP_BYTES),
    ] {
        match fs::read(peer.graph_root.join(path)) {
            Ok(bytes) if bytes == expected.as_bytes() => {}
            outcome => {
                return format!(
                "second-device join changed duplicate-path bytes at {path}: {outcome:?}; {shared}"
            )
            }
        }
    }
    match peer_joining.clean_shutdown() {
        Ok(SyncShutdownOutcome::Safe(_)) => {}
        outcome => {
            return format!(
                "second-device join retirement was not Safe: {outcome:?}; status={}; {shared}",
                describe_status(&peer_joining)
            )
        }
    }
    drop(peer_joining);
    let join_ms = join_started.elapsed().as_millis();

    let peer_reopen_started = Instant::now();
    let peer_reopened = SyncRuntimeHandle::open(peer.open_request);
    let peer_reopen_ms = peer_reopen_started.elapsed().as_millis();
    if peer_reopened.status != SyncRuntimeOpenStatus::Active {
        return format!(
            "second-device reopen failed: {:?}; {shared}",
            peer_reopened.status
        );
    }
    let Some(peer_handle) = peer_reopened.handle else {
        return "second-device reopen returned Active without a handle".into();
    };
    match peer_handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: JOURNEY_EXTERNAL_EDIT_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. })
            if page
                .blocks
                .iter()
                .map(|block| block.raw.as_str())
                .collect::<Vec<_>>()
                == vec!["ordinary non-ASCII page", JOURNEY_EXTERNAL_EDIT_BLOCK] => {}
        outcome => {
            return format!("second-device duplicate-path owner mismatch: {outcome:?}; {shared}")
        }
    }
    match peer_handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: JOURNEY_EDITED_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. })
            if page.blocks.first().map(|block| block.raw.as_str())
                == Some(JOURNEY_EDITED_BYTES.trim_start_matches("- ").trim()) => {}
        outcome => return format!("second-device joined page mismatch: {outcome:?}; {shared}"),
    }
    match peer_handle.clean_shutdown() {
        Ok(SyncShutdownOutcome::Safe(_)) => {}
        outcome => {
            return format!(
                "second-device reopened clean shutdown failed: {outcome:?}; status={}; {shared}",
                describe_status(&peer_handle)
            )
        }
    }
    drop(peer_handle);

    let reopened = SyncRuntimeHandle::open(open_request);
    if reopened.status != SyncRuntimeOpenStatus::Active {
        return format!(
            "reopen after sharing failed: {:?}; {shared}",
            reopened.status
        );
    }
    let Some(handle) = reopened.handle else {
        return "reopen returned Active without a handle".into();
    };
    match handle.clean_shutdown() {
        Ok(SyncShutdownOutcome::Safe(_)) => format!(
            "ok activation_ms={activation_ms} activation_attempts={attempts} activation_retried={retried_receipt} first_page_ms={first_page_ms} crash_reopen_ms={crash_reopen_ms} reconciliation_ms={reconciliation_ms} join_ms={join_ms} peer_reopen_ms={peer_reopen_ms} total_ms={} {folding_receipt} {retained} {shared} second_device_join=ok reconciliation[{reconciliation}] progress={}",
            journey_started.elapsed().as_millis(),
            progress_receipt.join("|")
        ),
        outcome => format!(
            "reopened clean shutdown failed: {outcome:?}; {shared}; status={}",
            describe_status(&handle)
        ),
    }
}

/// The instrumentation boundary can report only returned values, and a
/// `SyncRuntimeRequestError` carries no state. Name the runtime's own view of
/// itself next to every refusal so one CI round trip localises it.
fn describe_status(handle: &SyncRuntimeHandle) -> String {
    match handle.status() {
        Ok(status) => format!(
            "lifecycle:{:?} recovery:{:?} shared_role:{:?} shared_phase:{:?} provider_pending:{} managed_local_pending:{} detail:{}",
            status.lifecycle,
            status.recovery,
            status.shared_role,
            status.shared_phase,
            status.provider_pending,
            status.managed_local_pending,
            status.detail.as_deref().unwrap_or("none")
        ),
        Err(error) => format!("unavailable:{error}"),
    }
}

#[cfg(test)]
mod tests {
    /// The Android boundary must DELEGATE to this journey, never re-implement
    /// it. Two hand-maintained copies is the state that let `ManagedStorageSmoke`
    /// stay green while a device failed: the shim owned its own fixture and its
    /// own call sequence, so nothing forced the two to describe the same app.
    ///
    /// This is a source-shape guard because the shim is `cfg(target_os =
    /// "android")` and cannot be compiled, let alone run, on this host.
    #[test]
    fn the_android_shim_delegates_to_this_journey() {
        let shim = include_str!("../../../src-tauri/src/android_managed_storage_smoke.rs");
        assert!(
            shim.contains("run_managed_storage_journey")
                && shim.contains("write_journey_graph_fixture")
                && shim.contains("run_android_managed_return_to_direct_files"),
            "the Android instrumentation shim must drive the shared journey"
        );
        assert!(
            !shim.contains("load_application_page") && !shim.contains("prepare_shared"),
            "the Android shim must not carry its own copy of the call sequence"
        );
        let instrumentation = include_str!(
            "../../../src-tauri/gen/android/app/src/androidTest/java/page/tine/app/ManagedStorageSmokeTest.kt"
        );
        assert!(
            instrumentation.contains("writeFixture: Boolean"),
            "the instrumentation test must ask the native side for the shared fixture"
        );
        let journey_case = instrumentation
            .split_once("fun activationEditCrashReopenShare")
            .and_then(|(_, tail)| tail.split_once("@Test"))
            .map(|(body, _)| body)
            .expect("the journey instrumentation case must remain identifiable");
        assert!(
            !journey_case.contains(".writeText(\"- Android managed storage smoke"),
            "the journey case must not write its own copy of the journey fixture"
        );
        assert!(
            journey_case.contains("privateRoot.absolutePath,\n        true,"),
            "the journey case must ask for the shared fixture"
        );
        // EVERY case, not just the journey case. The resume case kept its own
        // smaller graph and so drove the shared journey against a tree the
        // journey's external leg does not describe; it failed on the shapes
        // that graph could not hold, not on anything the runtime got wrong.
        assert!(
            !instrumentation.contains("privateRoot.absolutePath,\n        false,"),
            "no instrumentation case may hand-maintain its own copy of the journey graph"
        );
    }

    /// The fixture must refuse a graph tree that could not hold it.
    ///
    /// Two of [`JOURNEY_NAME_SHAPES`] differ from a sibling only by Unicode
    /// normalization, two only by case. A filesystem that folds either pair
    /// silently swallows one write: the tree the journey activates is a file
    /// short and holds a sibling's bytes, and every refusal downstream is about
    /// something else. On Android that arrived as `source capture changed
    /// before final inventory proof … content:…@26 -> content:…@25` on a path
    /// that printed identically on both sides — 25 bytes being the SIBLING
    /// shape's content verbatim, not a normalized copy of the 26.
    ///
    /// The folding is simulated here rather than waited for: no host filesystem
    /// this suite runs on folds those names, which is precisely why the journey
    /// has to carry the proof to the platforms that might.
    #[test]
    fn the_fixture_refuses_a_graph_tree_that_folds_two_of_its_shapes() {
        let root = std::env::temp_dir().join(format!(
            "tine-journey-fixture-folding-{}",
            uuid::Uuid::new_v4()
        ));
        super::write_journey_graph_fixture(&root).expect("an ordinary tree holds every shape");

        // Exactly what a folding filesystem leaves behind: the decomposed
        // write landed on the precomposed file.
        std::fs::write(
            root.join("pages/\u{17d} pilot notes #pilot.md"),
            "- decomposed pilot notes\n",
        )
        .unwrap();
        // The probe answered "folds nothing" on this host, so this fold is one
        // the model did not predict — still a refusal, and now one that says
        // the model and the filesystem disagree.
        let tree = super::journey_graph_tree(crate::graph_name_folding::GraphNameFolding::NONE);
        let refusal = super::verify_journey_graph_fixture(&root, &tree)
            .expect_err("a folded shape must be refused where it happened")
            .to_string();
        assert!(
            refusal.contains("folds two journey page names into one file"),
            "{refusal}"
        );
        assert!(
            refusal.contains("the name-folding probe did not predict it (probe answered none)"),
            "{refusal}"
        );
        // Both spellings, escaped: they print as one glyph sequence otherwise.
        assert!(
            refusal.contains("pages/\\u{17d} pilot notes #pilot.md"),
            "{refusal}"
        );
        assert!(
            refusal.contains("pages/Z\\u{30c} pilot notes #pilot.md"),
            "{refusal}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other half, and the one the device needs: where the probe DID
    /// predict the fold, the fixture must write a tree the filesystem can hold
    /// and accept it — not refuse and take Android's only coverage with it.
    ///
    /// The fold is forced rather than waited for: no host filesystem this suite
    /// runs on folds anything, which is exactly why the journey has to carry
    /// the proof to the platform that does.
    #[test]
    fn the_fixture_writes_and_accepts_a_tree_a_folding_filesystem_can_hold() {
        use crate::graph_name_folding::{
            clear_graph_name_folding_for_tests, force_graph_name_folding_for_tests,
            GraphNameFolding,
        };

        let root = std::env::temp_dir().join(format!(
            "tine-journey-fixture-folds-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let folding = GraphNameFolding {
            ascii_case: true,
            unicode_case: true,
            normalization: false,
        };
        force_graph_name_folding_for_tests(&root, folding);
        super::write_journey_graph_fixture(&root)
            .expect("a folding filesystem must get a tree it can hold, not a refusal");

        // The twin has no file of its own, and the survivor holds the LAST
        // write's bytes — verbatim what Android reported (the upper-case name
        // read back 18 bytes, the lower-case content, not its own 8).
        assert!(!root.join(super::JOURNEY_CASE_TWIN_PAGE).exists());
        assert_eq!(
            std::fs::read_to_string(root.join(super::JOURNEY_CASE_OWNER_PAGE)).unwrap(),
            "- lowercase horse\n"
        );
        // The normalization pair is NOT folded by this filesystem, so both of
        // those files must still be there. A fixture that dropped every twin
        // whenever anything folded would silently stop covering the shapes that
        // reproduced Martin's failure.
        assert_eq!(
            std::fs::read_to_string(root.join("pages/\u{17d} pilot notes #pilot.md")).unwrap(),
            "- precomposed pilot notes\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("pages/Z\u{30c} pilot notes #pilot.md")).unwrap(),
            "- decomposed pilot notes\n"
        );

        clear_graph_name_folding_for_tests(&root);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The model must describe both filesystem classes, and must not describe
    /// them the same way — every assertion in the journey's folding leg is read
    /// out of it.
    #[test]
    fn the_graph_tree_model_separates_the_two_filesystem_classes() {
        use crate::graph_name_folding::GraphNameFolding;

        let apart = super::journey_graph_tree_after_external_writes(GraphNameFolding::NONE);
        assert!(apart.has_its_own_file(super::JOURNEY_CASE_TWIN_PAGE));
        assert_eq!(
            apart
                .first_block_at(super::JOURNEY_CASE_OWNER_PAGE)
                .unwrap(),
            "horse"
        );
        assert_eq!(
            apart.bytes_at(super::JOURNEY_CASE_TWIN_PAGE).unwrap(),
            super::JOURNEY_CASE_TWIN_AFTER.as_bytes()
        );

        let folded = super::journey_graph_tree_after_external_writes(GraphNameFolding {
            ascii_case: true,
            unicode_case: true,
            normalization: false,
        });
        assert!(!folded.has_its_own_file(super::JOURNEY_CASE_TWIN_PAGE));
        assert!(folded.has_its_own_file(super::JOURNEY_CASE_OWNER_PAGE));
        // One file, and it holds the outside writer's bytes: on this storage
        // that write WAS an edit of the owner, not a create beside it.
        assert_eq!(
            folded
                .first_block_at(super::JOURNEY_CASE_OWNER_PAGE)
                .unwrap(),
            "lowercase horse, edited outside Tine"
        );
        assert_eq!(
            folded.bytes_at(super::JOURNEY_CASE_TWIN_PAGE).unwrap(),
            folded.bytes_at(super::JOURNEY_CASE_OWNER_PAGE).unwrap()
        );
        assert_eq!(folded.folding().diagnostic(), "ascii_case+unicode_case");
    }

    /// The shapes are the point. If someone trims this list, the journey stops
    /// being a gate for the class of defect it was extended to catch.
    #[test]
    fn the_journey_fixture_keeps_its_real_graph_name_shapes() {
        let paths = super::JOURNEY_NAME_SHAPES
            .iter()
            .map(|(path, _)| *path)
            .collect::<Vec<_>>();
        assert!(paths.iter().any(|path| path.contains('\u{17d}')));
        assert!(paths.iter().any(|path| path.contains('\u{30c}')));
        assert!(paths.iter().any(|path| path.contains('#')));
        assert!(paths.iter().any(|path| path.contains("%23")));
        assert!(paths.iter().any(|path| path.contains(' ')));
        // One pair differs only by case, one only by normalization.
        assert!(paths.contains(&"pages/K\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md"));
        assert!(paths.contains(&"pages/k\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md"));
        // The folding leg addresses that pair by name, so the two must stay the
        // same two strings the fixture writes.
        assert!(paths.contains(&super::JOURNEY_CASE_OWNER_PAGE));
        assert!(paths.contains(&super::JOURNEY_CASE_TWIN_PAGE));
        assert!(paths.contains(&"pages/\u{17d} pilot notes #pilot.md"));
        assert!(paths.contains(&"pages/Z\u{30c} pilot notes #pilot.md"));
        // The external leg must keep creating a second physical file for an
        // already-owned page name: that is the reported refusal's shape.
        assert!(super::JOURNEY_EXTERNAL_WRITES
            .iter()
            .any(|(path, _)| path.starts_with("archiv/")
                && path.ends_with("Denn\u{ed} pozn\u{e1}mky.md")));
    }
}
