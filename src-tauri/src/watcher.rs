use crate::settings::{settings_path, update_settings};
use crate::state::{take_in_config_change, AppState, GraphSlot, RefreshLaneWait, RefreshOutcome};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use tauri::{Emitter, Manager, State};
use tine_core::{
    model::GraphTextExactFeedPathClass, model::GraphTextExternalObservationTicket,
    model::GraphTextWatchReach, model::PageKind, Graph,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct GraphChange {
    name: String,
    kind: PageKind,
    created: bool,
    removed: bool,
}

// ---------------------------------------------------------------------------
// Bulk external revisions (Concord P2, GH #337 / spec L6)
// ---------------------------------------------------------------------------
// A VCS checkout, branch switch, or first big sync under a running Tine dumps
// one file event per touched page. Two per-file costs amplify that: the
// incremental reconcile branch pays two full reads + a parse per evented path
// (deliberately no stat shortcut, see `incremental_reconcile`), and the emit
// side sends one `graph-changed` per changed page, which the frontend answers
// with one dataRev bump and up to one `getPage` IPC each. Above the threshold
// both stop scaling per-file: the drained batch escalates to the stat-diff
// full branch (one consistent snapshot; an unchanged file costs one stat), and
// the changed pages are announced as ONE `graph-changed-bulk` event.

/// Boundary between "a burst of ordinary edits" and "an external revision".
///
/// Sizing: one atomic save produces at most 2 paths; a human-scale sync delta
/// (Syncthing propagating a session of edits) is single digits to low tens; a
/// checkout or big sync is typically hundreds. 32 sits between those clusters,
/// inside the 24–64 band the P2 design allows. The cost of escalating a batch
/// of 33 is one stat per unwatched-change file (microseconds each, ~2 ms even
/// on a 1,000-file graph) — cheaper than a single page's double-read — so the
/// exact value only needs to keep ordinary edits per-file, not be optimal.
const BULK_CHANGE_THRESHOLD: usize = 32;

/// Does a drained batch of this many owned event paths escalate to the full
/// stat-diff branch?
fn burst_escalates(owned_paths: usize) -> bool {
    owned_paths > BULK_CHANGE_THRESHOLD
}

/// Does a reconcile cycle that changed this many pages coalesce its frontend
/// notification into one aggregate event? Same boundary as `burst_escalates`,
/// deliberately: below it nothing about today's behavior changes.
fn emit_as_bulk(changed_pages: usize) -> bool {
    changed_pages > BULK_CHANGE_THRESHOLD
}

/// One aggregate frontend notification for a reconcile cycle that changed more
/// than `BULK_CHANGE_THRESHOLD` pages. Carries the full per-page change list so
/// the frontend can reload visible pages, run the dirty-page safety machinery,
/// and summarize the rest — without N events.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct GraphChangedBulk {
    changes: Vec<GraphChange>,
}

/// One coalesced cache-invalidation epoch for ordinary files below the graph's
/// approved assets capability. Paths are assets-relative and never expose the
/// user's absolute graph or external-assets location to the WebView.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct AssetChangedBatch {
    paths: Vec<String>,
}

#[derive(Default)]
struct Pending {
    paths: HashSet<PathBuf>,
    full_paths: HashSet<PathBuf>,
    /// Exact ordinary-file events retained for the separate asset observer.
    /// This queue grants no graph-text admission: it is
    /// later intersected with each binding's approved assets capability and
    /// reduced to frontend cache invalidations only.
    asset_paths: HashSet<PathBuf>,
    /// Ambiguous/directory asset candidates. Only a candidate owned by an
    /// assets capability requests that capability's metadata-only rescan.
    asset_full_paths: HashSet<PathBuf>,
    /// Candidate `logseq/config.edn` paths. Configuration is not graph text --
    /// `incremental_page_paths` discards it a few lines below, which is why an
    /// external config edit was invisible until the next graph open -- so it
    /// needs its own queue rather than a place in `paths`.
    config_paths: HashSet<PathBuf>,
    /// Highest raw-callback frontier admitted into this pending batch for each
    /// Direct graph root. The callback records this while holding the same
    /// mutex used to add its notify event, so a drained batch can never
    /// acknowledge an event that is still waiting to enter the queue.
    legacy_observation_epochs: HashMap<PathBuf, GraphTextExternalObservationTicket>,
    need_full: bool,
    notify_error: bool,
    /// When the FIRST notify callback of the batch currently accumulating
    /// arrived (monotonic). Taken together with the paths at drain time, so a
    /// latency receipt can attribute callback→reconcile time (the coalescing
    /// window plus any scheduling delay). One `Instant` per batch — per-path
    /// stamps would allocate on the hot path for no diagnostic gain.
    first_event_at: Option<Instant>,
}

/// Resolve the filesystem watcher inputs for an existing Direct Files binding.
fn direct_watch_paths(
    slot: &GraphSlot,
) -> Result<(Arc<Graph>, PathBuf), crate::command_error::CommandError> {
    let graph = slot.graph();
    let root = slot.root_key.clone();
    Ok((graph, root))
}

impl Pending {
    fn note_event_arrival(&mut self) {
        if self.first_event_at.is_none() {
            self.first_event_at = Some(Instant::now());
        }
    }

    fn add_event(&mut self, event: notify::Event) {
        self.note_event_arrival();
        for path in event
            .paths
            .iter()
            .filter(|path| path_is_config_file_name(path))
        {
            self.config_paths.insert(path.clone());
        }
        if let Some(paths) = incremental_asset_paths(&event) {
            self.asset_paths.extend(paths);
        } else if !event.paths.is_empty() {
            self.asset_full_paths.extend(event.paths.iter().cloned());
        }
        if event.need_rescan() {
            if event.paths.is_empty() {
                self.need_full = true;
            } else {
                self.full_paths.extend(event.paths);
            }
            return;
        }
        if let Some(paths) = incremental_page_paths(&event) {
            self.paths.extend(paths);
        } else if event.paths.is_empty() {
            self.need_full = true;
        } else {
            // A directory move or genuinely unknown file operation needs a full
            // diff only for the graph that owns its reported path.
            self.full_paths.extend(event.paths);
        }
    }

    fn add_notify_error(&mut self) {
        self.note_event_arrival();
        self.need_full = true;
        self.notify_error = true;
    }

    fn add_legacy_observations(
        &mut self,
        observations: Vec<(PathBuf, GraphTextExternalObservationTicket)>,
    ) {
        for (root, ticket) in observations {
            self.legacy_observation_epochs
                .entry(root)
                .and_modify(|current| {
                    *current = current.later_for_same_instance(ticket).unwrap_or(ticket)
                })
                .or_insert(ticket);
        }
    }

    fn take_legacy_observation_epochs(
        &mut self,
    ) -> HashMap<PathBuf, GraphTextExternalObservationTicket> {
        std::mem::take(&mut self.legacy_observation_epochs)
    }
}

// ---------------------------------------------------------------------------
// Watcher latency receipts (GH #337 diagnosis)
// ---------------------------------------------------------------------------
// The reported 5–20 s external-change latency on Windows is unexplained by a
// pipeline whose design floor is ~200 ms (inotify coalescing) / 3 s (poll), and
// nothing measured the pipeline. These receipts are cheap and always on: one
// monotonic stamp when the first notify callback of a batch arrives, one when
// its post-debounce reconcile starts, one when its `graph-changed` events have
// been emitted. Each external-change batch logs one structured line (via
// `debug::diag`, so `--debug` captures it in the log file a reporter can send)
// and lands in a small in-memory ring the `watcher_latency_recent` command
// returns. No extra reads, no per-path allocation.

/// One external-change batch, as the reconcile loop experienced it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct WatcherLatencyReceipt {
    /// Monotonically increasing receipt number (process-wide).
    seq: u64,
    /// Wall-clock time the receipt was recorded (Unix ms) — to correlate with
    /// the reporter's "I saved the file at ...".
    at_unix_ms: u64,
    /// Graph label the batch was reconciled for.
    graph: String,
    /// "inotify" or "poll".
    mode: &'static str,
    /// `graph-changed` events emitted for this batch.
    pages: usize,
    /// Exact event paths this graph owned in the batch (0 for a pure full diff).
    event_paths: usize,
    /// Whether the full stat-diff branch was taken (poll cycle, unclassifiable
    /// event, kernel queue overflow, retry, or a burst-escalated batch above
    /// `BULK_CHANGE_THRESHOLD`).
    full_diff: bool,
    /// Reconcile errors in this batch (each schedules a backoff retry — a
    /// latency source worth seeing in a receipt trail).
    errors: usize,
    /// First notify callback → reconcile start (debounce + scheduling). `None`
    /// when no callback stamp exists for the batch: poll mode, or a cycle
    /// triggered by retry/control rather than an OS event.
    event_to_reconcile_ms: Option<u64>,
    /// Reconcile start → last `graph-changed` emitted (read + parse + emit).
    reconcile_ms: u64,
    /// First notify callback → last emit; the number GH #337 reports as 5–20 s.
    event_to_emit_ms: Option<u64>,
}

/// Report form of a watcher receipt. Graph labels are deliberately omitted:
/// they can contain a user-chosen graph name.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WatcherDiagnosticReceipt {
    mode: &'static str,
    pages: usize,
    event_paths: usize,
    full_diff: bool,
    errors: usize,
    event_to_reconcile_ms: Option<u64>,
    reconcile_ms: u64,
    event_to_emit_ms: Option<u64>,
}

/// Concord L0's reload-on-focus fallback. Some filesystems and sync clients
/// deliver no inotify edge at all (network mounts, a suspended app, a client
/// that writes through a path the kernel doesn't report), so the ONE thing the
/// user can always do — come back to the window — has to be able to ask.
///
/// This asks the watcher for one full stat-diff pass on its next cycle. It does
/// NOT invent a second freshness path: whatever the diff finds is emitted as
/// ordinary `graph-changed` / `graph-changed-bulk` events, so a page being
/// edited is deferred by the P1 replay machinery exactly as for a live event,
/// and a caret is never stolen.
static FULL_RESCAN_REQUESTED: AtomicU64 = AtomicU64::new(0);
static FULL_RESCAN_COMPLETED: AtomicU64 = AtomicU64::new(0);

/// Tell the window which pages Tine newly could not read. Such a page is left
/// out of search, queries and references while the rest of the graph works;
/// its file stays untouched until the user fixes or restores it.
pub(crate) fn announce_unreadable_pages(app: &tauri::AppHandle, label: &str, graph: &Graph) {
    let paths = graph.take_unannounced_page_failures();
    if !paths.is_empty() {
        let _ = app.emit_to(label, "graph-unreadable-pages", paths);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
struct GraphRescanComplete {
    sequence: u64,
}

/// Request that one full rescan. Pair with `state::poke_watcher` — this only
/// arms the flag; the poke is what wakes the loop to read it.
pub(crate) fn request_full_rescan() -> u64 {
    FULL_RESCAN_REQUESTED.fetch_add(1, Ordering::SeqCst) + 1
}

/// Snapshot the newest explicit request not yet completed. Several focus and
/// visibility notifications may arrive before one watcher turn; one full pass
/// satisfies all of them and publishes the newest sequence.
fn pending_full_rescan() -> Option<u64> {
    let requested = FULL_RESCAN_REQUESTED.load(Ordering::SeqCst);
    (requested > FULL_RESCAN_COMPLETED.load(Ordering::SeqCst)).then_some(requested)
}

fn complete_full_rescan(app: &tauri::AppHandle, sequence: u64) {
    FULL_RESCAN_COMPLETED.fetch_max(sequence, Ordering::SeqCst);
    // Broadcast rather than target one graph window: a request can race a graph
    // rebind, and every frontend matches the exact sequence it requested.
    let _ = app.emit("graph-rescan-complete", GraphRescanComplete { sequence });
}

const LATENCY_RECEIPT_CAP: usize = 64;

static LATENCY_RECEIPTS: OnceLock<Mutex<VecDeque<WatcherLatencyReceipt>>> = OnceLock::new();
static LATENCY_RECEIPT_SEQ: AtomicU64 = AtomicU64::new(0);

fn latency_receipts() -> &'static Mutex<VecDeque<WatcherLatencyReceipt>> {
    LATENCY_RECEIPTS.get_or_init(|| Mutex::new(VecDeque::with_capacity(LATENCY_RECEIPT_CAP)))
}

/// Pure builder so the duration arithmetic is unit-testable. `seq` and
/// `at_unix_ms` are stamped by `record_latency_receipt`.
#[allow(clippy::too_many_arguments)]
fn latency_receipt(
    graph: &str,
    inotify: bool,
    pages: usize,
    event_paths: usize,
    full_diff: bool,
    errors: usize,
    first_event_at: Option<Instant>,
    reconcile_started: Instant,
    emitted_at: Instant,
) -> WatcherLatencyReceipt {
    let since = |earlier: Instant, later: Instant| {
        later.saturating_duration_since(earlier).as_millis() as u64
    };
    WatcherLatencyReceipt {
        seq: 0,
        at_unix_ms: 0,
        graph: graph.to_string(),
        mode: if inotify { "inotify" } else { "poll" },
        pages,
        event_paths,
        full_diff,
        errors,
        event_to_reconcile_ms: first_event_at.map(|at| since(at, reconcile_started)),
        reconcile_ms: since(reconcile_started, emitted_at),
        event_to_emit_ms: first_event_at.map(|at| since(at, emitted_at)),
    }
}

fn push_latency_receipt(
    ring: &mut VecDeque<WatcherLatencyReceipt>,
    receipt: WatcherLatencyReceipt,
) {
    while ring.len() >= LATENCY_RECEIPT_CAP {
        ring.pop_front();
    }
    ring.push_back(receipt);
}

fn record_latency_receipt(mut receipt: WatcherLatencyReceipt) {
    receipt.seq = LATENCY_RECEIPT_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    receipt.at_unix_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let stage =
        |value: Option<u64>| value.map_or_else(|| "n/a".to_string(), |ms| format!("{ms}ms"));
    crate::debug::diag(format!(
        "watcher-latency seq={} graph={} mode={} pages={} event_paths={} full_diff={} errors={} event->reconcile={} reconcile={}ms event->emit={}",
        receipt.seq,
        receipt.graph,
        receipt.mode,
        receipt.pages,
        receipt.event_paths,
        receipt.full_diff,
        receipt.errors,
        stage(receipt.event_to_reconcile_ms),
        receipt.reconcile_ms,
        stage(receipt.event_to_emit_ms),
    ));
    crate::debug::record_watcher_latency(
        receipt.mode,
        u64::try_from(receipt.pages).unwrap_or(u64::MAX),
        u64::try_from(receipt.event_paths).unwrap_or(u64::MAX),
        receipt.full_diff,
        u64::try_from(receipt.errors).unwrap_or(u64::MAX),
        receipt.event_to_reconcile_ms,
        receipt.reconcile_ms,
        receipt.event_to_emit_ms,
    );
    if let Ok(mut ring) = latency_receipts().lock() {
        push_latency_receipt(&mut ring, receipt);
    }
}

pub(crate) fn diagnostic_latency_snapshot() -> Vec<WatcherDiagnosticReceipt> {
    latency_receipts()
        .lock()
        .map(|ring| {
            ring.iter()
                .map(|receipt| WatcherDiagnosticReceipt {
                    mode: receipt.mode,
                    pages: receipt.pages,
                    event_paths: receipt.event_paths,
                    full_diff: receipt.full_diff,
                    errors: receipt.errors,
                    event_to_reconcile_ms: receipt.event_to_reconcile_ms,
                    reconcile_ms: receipt.reconcile_ms,
                    event_to_emit_ms: receipt.event_to_emit_ms,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Debug command for bug reports: the last 64 external-change latency receipts,
/// oldest first. A reporter runs it from the devtools console and pastes the
/// result; no UI surface beyond that.
#[tauri::command]
pub(crate) fn watcher_latency_recent() -> Vec<WatcherLatencyReceipt> {
    latency_receipts()
        .lock()
        .map(|ring| ring.iter().cloned().collect())
        .unwrap_or_default()
}

const RETRY_BACKOFF: [Duration; 6] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
];

/// How long the inotify branch may block while a graph root it WANTS to watch is
/// still unwatched. Matches the poll branch's ceiling: this is a recovery cadence
/// for a root the kernel refused, not a polling strategy.
const UNWATCHED_ROOT_RETRY: Duration = Duration::from_secs(3);

/// How long to wait for the next cycle in the inotify branch.
///
/// A root whose `watch()` failed is retried on the next cycle — it is never
/// inserted into `watched` — but a cycle only begins when something wakes this
/// thread, and in the inotify branch that means an event from a root that IS
/// watched. With a single graph that is self-correcting: the failure leaves
/// `watched` empty, which takes the bounded poll branch instead. With two graphs
/// open it is not. inotify limits are per-user (`fs.inotify.max_user_watches`),
/// so opening a second large graph is exactly how one root fails while the other
/// is healthy — and then the failing graph stays invisible to external changes
/// until the healthy one happens to change, which on a quiet graph is never.
///
/// So bound the wait whenever a desired root is unwatched. Each such cycle
/// polls the unwatched root (a full diff and a configuration check) and retries
/// its watch; the cycle that watch succeeds diffs it once more, for what
/// changed in between. (Direct Files data-safety audit 2026-08-09, finding 16;
/// GH #543, audit R11-04: the bounded wait only retried the watch, and nothing
/// diffed the unwatched root.)
fn inotify_cycle_wait(retry_wait: Option<Duration>, unwatched_root: bool) -> Option<Duration> {
    match (retry_wait, unwatched_root) {
        (Some(wait), true) => Some(wait.min(UNWATCHED_ROOT_RETRY)),
        (None, true) => Some(UNWATCHED_ROOT_RETRY),
        (wait, false) => wait,
    }
}

#[derive(Default)]
struct RetrySchedule {
    failures: usize,
    due: Option<Instant>,
}

impl RetrySchedule {
    fn failed(&mut self, now: Instant) {
        let index = self.failures.min(RETRY_BACKOFF.len() - 1);
        self.failures = self.failures.saturating_add(1);
        self.due = Some(now + RETRY_BACKOFF[index]);
    }

    fn succeeded(&mut self) {
        self.failures = 0;
        self.due = None;
    }

    fn take_due(&mut self, now: Instant) -> bool {
        if self.due.is_some_and(|due| due <= now) {
            self.due = None;
            true
        } else {
            false
        }
    }

    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.due.map(|due| due.saturating_duration_since(now))
    }
}

fn is_page_file_path(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md")
                || extension.eq_ignore_ascii_case("markdown")
                || extension.eq_ignore_ascii_case("org")
        })
}

fn path_is_existing_dir(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

fn is_tine_atomic_page_temp_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    let Some(mut stem) = name.strip_suffix(".tmp") else {
        return false;
    };
    // Backup-restore publishes page files into the live graph through
    // `.tine-restore-{pid}-{seq}.tmp` temps (`backup.rs::atomic_copy_new_into_live`).
    // Recognize that shape too, or a rename event pairing such a temp with a
    // page path is dropped and the restored page never queued — previously
    // masked only by an unrelated `refresh_graph` call after restore (DUP-5).
    if let Some(rest) = stem.strip_prefix(".tine-restore-") {
        if let Some((pid, seq)) = rest.split_once('-') {
            if !pid.is_empty()
                && !seq.is_empty()
                && pid.chars().all(|value| value.is_ascii_digit())
                && seq.chars().all(|value| value.is_ascii_digit())
            {
                return true;
            }
        }
    }
    if let Some(without_projection) = stem.strip_suffix(".projection") {
        stem = without_projection;
    }
    if let Some(without_new) = stem.strip_suffix(".new") {
        stem = without_new;
    }
    let Some((before_seq, seq)) = stem.rsplit_once('.') else {
        return false;
    };
    let Some((page_name, pid)) = before_seq.rsplit_once('.') else {
        return false;
    };
    seq.chars().all(|value| value.is_ascii_digit())
        && pid.chars().all(|value| value.is_ascii_digit())
        && page_name.starts_with('.')
        && is_page_file_path(Path::new(page_name))
}

/// Directories whose churn a graph's watcher must never be woken by (Concord
/// P5). A repository or sync client parked inside the graph tree generates
/// thousands of events that CANNOT describe graph text — a `git gc`, an index
/// lock taken and dropped per command, a `.stversions` sweep — and every one of
/// them used to cross the channel, take the app-state read lock, lease each
/// graph and run two scope classifications before being discarded.
///
/// The scope is deliberately an explicit NAME LIST rather than "anything the
/// graph-text scope excludes": `.tine-sync` is also excluded from graph text,
/// but a name here must be provably outside graph text on
/// its own — `vcs_and_tool_noise_dirs_can_never_hold_graph_text` asserts
/// exactly that against `GraphTextScope`, so this list can never hide a page.
///
/// Matched against components of the path RELATIVE to a watched graph root, so
/// a graph that itself lives under (say) `/repo/.git/notes` is unaffected.
const VCS_AND_TOOL_NOISE_DIRS: &[&str] = &[
    ".bzr",
    ".git",
    ".hg",
    ".jj",
    ".stfolder",
    ".stversions",
    ".svn",
    // NOT `_darcs`: it carries no leading dot and is not in the core's fixed
    // exclusions, so `_darcs/Page.md` IS eligible graph text. The guard test
    // below caught it on the first run — which is the whole reason this list is
    // asserted against `GraphTextScope` rather than assumed.
    "node_modules",
];

fn path_is_tool_noise(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    relative.components().any(|component| {
        let Some(name) = component.as_os_str().to_str() else {
            return false;
        };
        VCS_AND_TOOL_NOISE_DIRS.contains(&name)
    })
}

/// True when EVERY path this event reports is tool noise under some watched
/// root — the only case it is safe to drop the event outright.
///
/// Never true for a rescan-required event (a kernel queue overflow says nothing
/// about which paths were lost) or a pathless one, and never true for a
/// multi-path event with even one ordinary path: a rename that moves a file OUT
/// of `.git` reports both sides and must still be seen.
fn watch_event_is_tool_noise(event: &notify::Event, roots: &HashSet<PathBuf>) -> bool {
    if event.need_rescan() || event.paths.is_empty() || roots.is_empty() {
        return false;
    }
    event
        .paths
        .iter()
        .all(|path| roots.iter().any(|root| path_is_tool_noise(root, path)))
}

/// A watch event path that *might* be some graph's `logseq/config.edn`.
///
/// Only the filename, deliberately: which graph owns it -- and whether it sits
/// at the one graph-relative location that counts -- is
/// `tine_core::model::is_config_file_path`'s decision, made per root when the
/// batch drains. This is the cheap gate that keeps the pending set small.
fn path_is_config_file_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("config.edn"))
}

fn incremental_page_paths(event: &notify::Event) -> Option<Vec<PathBuf>> {
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};

    let exact_file_event = matches!(
        event.kind,
        EventKind::Create(CreateKind::File)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Metadata(_))
            | EventKind::Remove(RemoveKind::File)
            // Windows ReadDirectoryChangesW supplies an exact path but no
            // create/modify sub-kind. The entry still exists, so the live
            // metadata check below is the file witness. `Remove(Any)` remains
            // deliberately absent: after removal we cannot distinguish a file
            // from a directory subtree.
            | EventKind::Create(CreateKind::Any)
            | EventKind::Modify(ModifyKind::Any)
    );
    let supported = exact_file_event
        || matches!(
            event.kind,
            EventKind::Modify(ModifyKind::Name(
                RenameMode::From | RenameMode::To | RenameMode::Both
            ))
        );
    if !supported || event.paths.is_empty() {
        return None;
    }
    if event.paths.iter().any(|path| path_is_existing_dir(path)) {
        return None;
    }
    let all_text_or_temp = event
        .paths
        .iter()
        .all(|path| is_page_file_path(path) || is_tine_atomic_page_temp_path(path));
    if !all_text_or_temp {
        // A rename without a file-kind witness may denote a directory subtree.
        if !exact_file_event {
            return None;
        }
        // An exact ordinary non-page file is outside Direct Files graph text.
        // Keep it out of both the incremental and full queues. The raw observer
        // likewise publishes no admission epoch for excluded paths.
        return Some(Vec::new());
    }
    Some(
        event
            .paths
            .iter()
            .filter(|path| is_page_file_path(path) && !path_is_existing_dir(path))
            .cloned()
            .collect(),
    )
}

/// Preserve exact ordinary-file events for the asset observer without deciding
/// here whether a path belongs to an assets capability. The callback has one
/// process-wide queue; binding ownership is deliberately resolved after drain.
fn incremental_asset_paths(event: &notify::Event) -> Option<Vec<PathBuf>> {
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};

    let supported = matches!(
        event.kind,
        EventKind::Create(CreateKind::File)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Metadata(_))
            | EventKind::Remove(RemoveKind::File)
            | EventKind::Create(CreateKind::Any)
            | EventKind::Modify(ModifyKind::Any)
            | EventKind::Modify(ModifyKind::Name(
                RenameMode::From | RenameMode::To | RenameMode::Both
            ))
    );
    if !supported || event.paths.is_empty() || event.need_rescan() {
        return None;
    }
    if event.paths.iter().any(|path| path_is_existing_dir(path)) {
        return None;
    }
    Some(event.paths.clone())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    modified: SystemTime,
    len: u64,
    identity: u128,
    changed: i128,
}

fn metadata_stamp(md: &std::fs::Metadata) -> Option<FileStamp> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Some(FileStamp {
            modified: md.modified().ok()?,
            len: md.len(),
            identity: ((md.dev() as u128) << 64) | md.ino() as u128,
            changed: (md.ctime() as i128) * 1_000_000_000 + md.ctime_nsec() as i128,
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return Some(FileStamp {
            modified: md.modified().ok()?,
            len: md.len(),
            identity: md.creation_time() as u128,
            changed: md.last_write_time() as i128,
        });
    }
    #[cfg(not(any(unix, windows)))]
    Some(FileStamp {
        modified: md.modified().ok()?,
        len: md.len(),
        identity: md
            .created()
            .ok()?
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_nanos(),
        changed: 0,
    })
}

/// The watcher's diff snapshot: every eligible graph-text file under the graph
/// root with its (mtime, len).
///
/// Scope authority is the core's `GraphTextScope` — the same one discovery
/// (`graph_text_inventory`) walks. Before GH #268 this scanned `journals/` and
/// `pages/` only, so a page at the graph root or in a custom folder was
/// discovered at open but never reconciled afterwards: the full-diff snapshot
/// it would have to differ against did not contain it.
/// `complete` is false when any part of the walk could not be read. A partial
/// snapshot would otherwise read as "every page under that directory was
/// deleted", and in poll mode it is also what decides whether the guarded
/// admission index may be updated from exact paths or has to be invalidated.
struct GraphTextSnapshot {
    files: HashMap<PathBuf, FileStamp>,
    complete: bool,
}

struct AssetSnapshot {
    files: HashMap<PathBuf, FileStamp>,
    complete: bool,
}

#[derive(Default)]
struct AssetWatchState {
    root: PathBuf,
    snap: HashMap<PathBuf, FileStamp>,
    baseline: bool,
    active: bool,
}

impl AssetWatchState {
    fn new(root: PathBuf) -> Self {
        // Capture before the OS watch is installed. The first reconcile after
        // installation then closes the binding→watch handoff: a sync client
        // that replaces an already-rendered image in that interval differs
        // from this baseline instead of silently becoming the baseline.
        let snapshot = collect_asset_files(&root);
        Self {
            root,
            snap: snapshot.files,
            baseline: true,
            active: true,
        }
    }

    fn active_root(&self) -> Option<&PathBuf> {
        self.active.then_some(&self.root)
    }
}

/// Tine's atomic asset publishers use hidden numeric temp names. They are
/// implementation artifacts, not logical assets; the final rename is the one
/// observation consumers care about.
fn is_tine_atomic_asset_temp_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(stem) = name.strip_suffix(".tmp") else {
        return false;
    };
    if !stem.starts_with('.') {
        return false;
    }
    let mut parts = stem.rsplit('.');
    let mut tail = parts.next().unwrap_or_default();
    if !tail.chars().all(|value| value.is_ascii_digit()) {
        // Replacement/copy helpers may name the publication purpose between
        // the numeric sequence and `.tmp`.
        tail = parts.next().unwrap_or_default();
    }
    if !tail.chars().all(|value| value.is_ascii_digit()) {
        return false;
    }
    parts
        .next()
        .is_some_and(|pid| !pid.is_empty() && pid.chars().all(|value| value.is_ascii_digit()))
}

fn collect_asset_files(root: &Path) -> AssetSnapshot {
    let mut files = HashMap::new();
    let mut complete = true;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let read_dir = match std::fs::read_dir(&directory) {
            Ok(read_dir) => read_dir,
            Err(error) if directory == root && error.kind() == std::io::ErrorKind::NotFound => {
                continue
            }
            Err(_) => {
                complete = false;
                continue;
            }
        };
        for entry in read_dir {
            let Ok(entry) = entry else {
                complete = false;
                continue;
            };
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                complete = false;
                continue;
            };
            // No-follow, exactly like graph-text scans: an asset-directory
            // symlink cannot widen the already-approved capability or loop.
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() || is_tine_atomic_asset_temp_path(&path) {
                continue;
            }
            match entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata_stamp(&metadata))
            {
                Some(stamp) => {
                    files.insert(path, stamp);
                }
                None => complete = false,
            }
        }
    }
    AssetSnapshot { files, complete }
}

fn asset_relative_event_path(root: &Path, path: &Path) -> Option<String> {
    if is_tine_atomic_asset_temp_path(path) {
        return None;
    }
    let relative = path.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    relative
        .to_str()
        .map(|relative| relative.replace(std::path::MAIN_SEPARATOR, "/"))
}

#[derive(Clone, Copy)]
struct RecentAssetSelfWrite {
    stamp: Option<FileStamp>,
    recorded_at: Instant,
}

const ASSET_SELF_WRITE_TTL: Duration = Duration::from_secs(30);
const ASSET_SELF_WRITE_CAP: usize = 4096;
static RECENT_ASSET_SELF_WRITES: OnceLock<Mutex<HashMap<(String, PathBuf), RecentAssetSelfWrite>>> =
    OnceLock::new();

/// Assets already handed to a WebView can be rendered before the watcher loop
/// has installed that graph's OS watch. Retain the observed file identity long
/// enough for the watcher to use it as the cache's real baseline instead of
/// accidentally baselining a replacement that arrived during startup.
static RECENT_ASSET_READS: OnceLock<Mutex<HashMap<(String, PathBuf), RecentAssetSelfWrite>>> =
    OnceLock::new();

pub(crate) fn note_asset_read(window_label: &str, path: &Path) {
    let reads = RECENT_ASSET_READS.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut reads) = reads.lock() else {
        return;
    };
    let now = Instant::now();
    reads.retain(|_, read| now.duration_since(read.recorded_at) <= ASSET_SELF_WRITE_TTL);
    if reads.len() >= ASSET_SELF_WRITE_CAP {
        if let Some(oldest) = reads
            .iter()
            .min_by_key(|(_, read)| read.recorded_at)
            .map(|(key, _)| key.clone())
        {
            reads.remove(&oldest);
        }
    }
    reads.insert(
        (window_label.to_owned(), path.to_path_buf()),
        RecentAssetSelfWrite {
            stamp: file_snapshot(path),
            recorded_at: now,
        },
    );
    if crate::debug::debug_enabled() {
        crate::debug::diag("asset observer recorded one WebView read");
    }
}

fn merge_recent_asset_reads(window_label: &str, state: &mut AssetWatchState) -> HashSet<PathBuf> {
    let Some(reads) = RECENT_ASSET_READS.get() else {
        return HashSet::new();
    };
    let Ok(mut reads) = reads.lock() else {
        return HashSet::new();
    };
    let now = Instant::now();
    let keys = reads
        .iter()
        .filter(|((label, path), read)| {
            now.duration_since(read.recorded_at) <= ASSET_SELF_WRITE_TTL
                && label == window_label
                && path.starts_with(&state.root)
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let mut merged = HashSet::new();
    for key in keys {
        if let Some(read) = reads.remove(&key) {
            let path = key.1;
            match read.stamp {
                Some(stamp) => {
                    state.snap.insert(path.clone(), stamp);
                }
                None => {
                    state.snap.remove(&path);
                }
            }
            merged.insert(path);
        }
    }
    reads.retain(|_, read| now.duration_since(read.recorded_at) <= ASSET_SELF_WRITE_TTL);
    if !merged.is_empty() && crate::debug::debug_enabled() {
        crate::debug::diag(format!(
            "asset observer merged {} WebView-read baseline(s)",
            merged.len()
        ));
    }
    merged
}

/// Record the final filesystem state published by one window's own asset
/// command. The watcher suppresses that echo only for the originating WebView;
/// other windows still need the invalidation.
pub(crate) fn note_asset_self_write(window_label: &str, path: &Path) {
    let writes = RECENT_ASSET_SELF_WRITES.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut writes) = writes.lock() else {
        return;
    };
    let now = Instant::now();
    writes.retain(|_, write| now.duration_since(write.recorded_at) <= ASSET_SELF_WRITE_TTL);
    if writes.len() >= ASSET_SELF_WRITE_CAP {
        if let Some(oldest) = writes
            .iter()
            .min_by_key(|(_, write)| write.recorded_at)
            .map(|(key, _)| key.clone())
        {
            writes.remove(&oldest);
        }
    }
    let canonical_path = std::fs::canonicalize(path).ok().or_else(|| {
        let parent = std::fs::canonicalize(path.parent()?).ok()?;
        Some(parent.join(path.file_name()?))
    });
    let path = canonical_path.unwrap_or_else(|| path.to_path_buf());
    writes.insert(
        (window_label.to_owned(), path.clone()),
        RecentAssetSelfWrite {
            stamp: file_snapshot(&path),
            recorded_at: now,
        },
    );
}

fn take_matching_asset_self_write(
    window_label: &str,
    path: &Path,
    stamp: Option<FileStamp>,
) -> bool {
    let Some(writes) = RECENT_ASSET_SELF_WRITES.get() else {
        return false;
    };
    let Ok(mut writes) = writes.lock() else {
        return false;
    };
    let key = (window_label.to_owned(), path.to_path_buf());
    let Some(write) = writes.remove(&key) else {
        return false;
    };
    Instant::now().duration_since(write.recorded_at) <= ASSET_SELF_WRITE_TTL && write.stamp == stamp
}

// notify can label one watched inode through any recursively watched symlink.
// Add the canonical spelling from every live approved binding before routing
// to individual windows. Do not canonicalize event files: deleted entries no
// longer exist, and an event must never grant a new filesystem capability.
fn normalize_asset_event_aliases<'a>(
    bindings: impl Iterator<Item = (&'a Path, &'a AssetWatchState)>,
    exact_paths: &mut HashSet<PathBuf>,
    full_paths: &mut HashSet<PathBuf>,
) {
    if exact_paths.is_empty() && full_paths.is_empty() {
        return;
    }
    let mut exact_additions = HashSet::new();
    let mut full_additions = HashSet::new();
    for (graph_root, assets) in bindings {
        if !assets.active {
            continue;
        }
        let lexical = graph_root.join("assets");
        if lexical == assets.root {
            continue;
        }
        let map = |path: &Path| {
            let relative = path.strip_prefix(&lexical).ok()?;
            relative
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
                .then(|| assets.root.join(relative))
        };
        exact_additions.extend(exact_paths.iter().filter_map(|path| map(path)));
        for path in full_paths.iter() {
            if let Some(mapped) = map(path) {
                full_additions.insert(mapped);
            } else if lexical.starts_with(path) {
                full_additions.insert(assets.root.clone());
            }
        }
    }
    exact_paths.extend(exact_additions);
    full_paths.extend(full_additions);
}

fn asset_full_scan_owned(root: &Path, paths: &HashSet<PathBuf>) -> bool {
    paths
        .iter()
        .any(|path| path.starts_with(root) || root.starts_with(path))
}

fn reconcile_asset_observation(
    window_label: &str,
    state: &mut AssetWatchState,
    exact_paths: &HashSet<PathBuf>,
    full_paths: &HashSet<PathBuf>,
    force_full: bool,
    poll_mode: bool,
) -> Vec<String> {
    if !state.active {
        return Vec::new();
    }
    if !state.baseline {
        let snapshot = collect_asset_files(&state.root);
        state.snap = snapshot.files;
        state.baseline = true;
        return Vec::new();
    }

    // A read seed is an observation boundary, not a filesystem event. Compare
    // it to current metadata even if its intervening kernel event happened
    // before this watcher binding existed.
    let observed_reads = merge_recent_asset_reads(window_label, state);
    let need_full = force_full || poll_mode || asset_full_scan_owned(&state.root, full_paths);
    let mut changed = Vec::<(PathBuf, Option<FileStamp>)>::new();
    if need_full {
        let current = collect_asset_files(&state.root);
        for (path, stamp) in &current.files {
            if state.snap.get(path) != Some(stamp) {
                changed.push((path.clone(), Some(*stamp)));
            }
        }
        if current.complete {
            for path in state.snap.keys() {
                if !current.files.contains_key(path) {
                    changed.push((path.clone(), None));
                }
            }
            state.snap = current.files;
        } else {
            // A partial walk can prove present changes but never deletion.
            state.snap.extend(current.files);
        }
    } else {
        let mut exact = exact_paths.clone();
        exact.extend(observed_reads);
        for path in exact.iter().filter(|path| path.starts_with(&state.root)) {
            if asset_relative_event_path(&state.root, path).is_none() {
                continue;
            }
            let current = file_snapshot(path);
            if state.snap.get(path).copied() == current {
                continue;
            }
            match current {
                Some(stamp) => {
                    state.snap.insert(path.clone(), stamp);
                }
                None => {
                    state.snap.remove(path);
                }
            }
            changed.push((path.clone(), current));
        }
    }

    let mut logical = changed
        .into_iter()
        .filter(|(path, stamp)| !take_matching_asset_self_write(window_label, path, *stamp))
        .filter_map(|(path, _)| asset_relative_event_path(&state.root, &path))
        .collect::<Vec<_>>();
    logical.sort();
    logical.dedup();
    logical
}

fn collect_graph_text_files(graph: &Graph) -> GraphTextSnapshot {
    collect_scoped_text_files(
        &graph.root,
        &|path| graph.graph_text_watch_descend(path),
        &|path| graph.graph_text_watch_relevant(path),
    )
}

/// The stat sweep the poll cycle gates on.
///
/// `descend`/`relevant` are the only scope authority: the graph's own
/// `GraphTextScope` predicates.
fn collect_scoped_text_files(
    root: &Path,
    descend: &dyn Fn(&Path) -> bool,
    relevant: &dyn Fn(&Path) -> bool,
) -> GraphTextSnapshot {
    let mut files: HashMap<PathBuf, FileStamp> = HashMap::new();
    let mut complete = true;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(read_dir) = std::fs::read_dir(&directory) else {
            complete = false;
            continue;
        };
        for entry in read_dir {
            let Ok(entry) = entry else {
                complete = false;
                continue;
            };
            let path = entry.path();
            // `file_type`/`metadata` on a DirEntry do not traverse a symlink, so
            // a symlinked directory is never descended (no cycles, no escaping
            // the watched tree) and a `secret.md` symlink never contributes
            // outside bytes.
            let Ok(file_type) = entry.file_type() else {
                complete = false;
                continue;
            };
            if file_type.is_dir() {
                if descend(&path) {
                    stack.push(path);
                }
                continue;
            }
            if !file_type.is_file() || !relevant(&path) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                complete = false;
                continue;
            };
            match metadata_stamp(&metadata) {
                Some(stamp) => {
                    files.insert(path, stamp);
                }
                None => complete = false,
            }
        }
    }
    GraphTextSnapshot { files, complete }
}

fn file_snapshot(path: &Path) -> Option<FileStamp> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    metadata_stamp(&md)
}

fn full_diff_reconcile(
    graph: &Graph,
    snap: &mut HashMap<PathBuf, FileStamp>,
    mut current: HashMap<PathBuf, FileStamp>,
) -> (Vec<GraphChange>, bool, Vec<String>) {
    let mut changes: Vec<GraphChange> = Vec::new();
    let mut errors = Vec::new();
    let mut failed_paths = Vec::new();
    // A sync-tool conflict copy appearing/vanishing isn't a page change (it's
    // never cached), but the conflicts panel must refresh — track it and emit
    // `conflicts-changed` once.
    let mut conflicts_dirty = false;
    for (p, m) in &current {
        if snap.get(p) != Some(m) {
            let created = !snap.contains_key(p);
            if tine_core::model::path_is_sync_conflict(p) {
                conflicts_dirty = true;
            } else {
                match graph.sync_file_checked(p) {
                    Ok(Some(en)) => changes.push(GraphChange {
                        name: en.name,
                        kind: en.kind,
                        created,
                        removed: false,
                    }),
                    Ok(None) => {}
                    Err(error) => {
                        errors.push(format!("{}: {error}", p.display()));
                        failed_paths.push(p.clone());
                    }
                }
            }
        }
    }
    for p in snap.keys() {
        if !current.contains_key(p) {
            if tine_core::model::path_is_sync_conflict(p) {
                conflicts_dirty = true;
            } else {
                match graph.sync_deleted_file(p) {
                    Ok(Some(en)) => changes.push(GraphChange {
                        name: en.name,
                        kind: en.kind,
                        created: false,
                        removed: true,
                    }),
                    Ok(None) => {}
                    Err(error) => {
                        errors.push(format!("{}: {error}", p.display()));
                        failed_paths.push(p.clone());
                    }
                }
            }
        }
    }
    for path in failed_paths {
        match snap.get(&path).copied() {
            Some(previous) => {
                current.insert(path, previous);
            }
            None => {
                current.remove(&path);
            }
        }
    }
    *snap = current;
    (changes, conflicts_dirty, errors)
}

fn incremental_reconcile(
    graph: &Graph,
    snap: &mut HashMap<PathBuf, FileStamp>,
    paths: &HashSet<PathBuf>,
) -> (Vec<GraphChange>, bool, Vec<String>) {
    let mut changes: Vec<GraphChange> = Vec::new();
    let mut conflicts_dirty = false;
    let mut errors = Vec::new();

    // Reconcile present destinations before absent sources. A provider-delivered
    // external rename then lets the new path claim persisted block IDs before the
    // old page is tombstoned, preserving identity across the two snapshot events.
    let mut ordered: Vec<&PathBuf> = paths.iter().collect();
    ordered.sort_by_key(|path| file_snapshot(path).is_none());
    for p in ordered {
        if let Some(m) = file_snapshot(p) {
            let created = !snap.contains_key(p);
            // This path came from an explicit OS event. Always compare its
            // content even if a sync/copy tool preserved mtime and length;
            // the graph reconciliation already suppresses Tine's own/unchanged bytes.
            if tine_core::model::path_is_sync_conflict(p) {
                conflicts_dirty = true;
            } else {
                match graph.sync_file_checked(p) {
                    Ok(Some(en)) => changes.push(GraphChange {
                        name: en.name,
                        kind: en.kind,
                        created,
                        removed: false,
                    }),
                    Ok(None) => {}
                    Err(error) => {
                        errors.push(format!("{}: {error}", p.display()));
                        continue;
                    }
                }
            }
            snap.insert(p.clone(), m);
        } else if snap.contains_key(p) {
            if tine_core::model::path_is_sync_conflict(p) {
                conflicts_dirty = true;
            } else {
                match graph.sync_deleted_file(p) {
                    Ok(Some(en)) => changes.push(GraphChange {
                        name: en.name,
                        kind: en.kind,
                        created: false,
                        removed: true,
                    }),
                    Ok(None) => {}
                    Err(error) => {
                        errors.push(format!("{}: {error}", p.display()));
                        continue;
                    }
                }
            }
            snap.remove(p);
        }
    }

    (changes, conflicts_dirty, errors)
}

/// Every path whose on-disk stamp differs from the snapshot, in either
/// direction. This is what a completed graph-wide scan learned.
fn changed_since_snapshot(
    snap: &HashMap<PathBuf, FileStamp>,
    current: &HashMap<PathBuf, FileStamp>,
) -> Vec<PathBuf> {
    let mut changed: Vec<PathBuf> = current
        .iter()
        .filter(|(path, stamp)| snap.get(*path) != Some(*stamp))
        .map(|(path, _)| path.clone())
        .collect();
    changed.extend(
        snap.keys()
            .filter(|path| !current.contains_key(*path))
            .cloned(),
    );
    changed
}

/// Tell the guarded admission index what a poll-mode rescan just learned.
///
/// Poll mode has no per-event callback to keep that index current, so it used to
/// invalidate the whole thing at the top of every 3 s cycle. A poll-mode user
/// (NFS, SMB) could therefore never hold a warm index, and paid a full graph
/// rebuild on the next save — forever.
///
/// The rescan now covers exactly the scope the index covers (GH #268), so what
/// it found IS a complete account of what changed and can be applied as exact
/// observations. Publishing them here — after the scan, before reconciling
/// anything — keeps the index no staler than the scan that produced it, which is
/// the same window the inotify callback lane already has. A scan that could not
/// read part of the graph learned nothing complete, so it falls back to
/// invalidating, exactly as before.
fn publish_poll_observation(
    graph: &Graph,
    snap: &HashMap<PathBuf, FileStamp>,
    snapshot: &GraphTextSnapshot,
) {
    let (changed, uncertain) = poll_observation(snap, snapshot);
    let _ =
        graph.observe_graph_text_external_paths(changed.iter().map(PathBuf::as_path), uncertain);
}

/// Reduce a poll scan to the literal observation published to `tine-core`.
/// Keeping this calculation pure lets the watcher crate prove its routing
/// contract without requiring access to the core's private retained index.
fn poll_observation(
    snap: &HashMap<PathBuf, FileStamp>,
    snapshot: &GraphTextSnapshot,
) -> (Vec<PathBuf>, bool) {
    (
        changed_since_snapshot(snap, &snapshot.files),
        !snapshot.complete,
    )
}

fn reconcile_pending(
    graph: &Graph,
    snap: &mut HashMap<PathBuf, FileStamp>,
    paths: &HashSet<PathBuf>,
    need_full: bool,
    poll_mode: bool,
) -> (Vec<GraphChange>, bool, bool, Vec<String>) {
    if need_full || paths.is_empty() || burst_escalates(paths.len()) {
        let snapshot = collect_graph_text_files(graph);
        if poll_mode {
            publish_poll_observation(graph, snap, &snapshot);
        }
        let (changes, conflicts_dirty, errors) = full_diff_reconcile(graph, snap, snapshot.files);
        (changes, conflicts_dirty, true, errors)
    } else {
        let (changes, conflicts_dirty, errors) = incremental_reconcile(graph, snap, paths);
        (changes, conflicts_dirty, false, errors)
    }
}

/// Re-read `logseq/config.edn` for the graphs an event named, and take in any
/// change through [`take_in_config_change`], the decider a settings command
/// uses too.
///
/// A separate pass rather than a branch inside the reconcile loops, because
/// configuration is not graph text: never in `GraphTextScope`, never
/// projected.
///
/// Returns true when a refresh was deferred and wants another cycle.
fn refresh_changed_configs(
    app: &tauri::AppHandle,
    labels_by_root: &[(String, PathBuf)],
    config_paths: &HashSet<PathBuf>,
    check_all: bool,
    recheck: &mut HashSet<String>,
) -> bool {
    let mut deferred = false;
    let state = app.state::<AppState>();
    for (label, root) in labels_by_root {
        let named = check_all
            || recheck.contains(label)
            || config_paths
                .iter()
                .any(|path| tine_core::model::is_config_file_path(root, path));
        if !named {
            continue;
        }
        recheck.remove(label);
        match take_in_config_change(&state, app, label, RefreshLaneWait::TryOnce) {
            Ok(RefreshOutcome::Deferred) => {
                recheck.insert(label.clone());
                deferred = true;
            }
            Ok(RefreshOutcome::Refreshed | RefreshOutcome::Current) => {}
            Err(message) => {
                // Not silent: until this succeeds the window is serving stale
                // configuration, which is exactly the failure this whole pass
                // exists to prevent.
                let _ = app.emit_to(label, "graph-watch-error", &message);
            }
        }
    }
    deferred
}

/// Which pending event paths this graph should reconcile incrementally.
///
/// The predicate is the core's graph-wide text scope, not `journals/` +
/// `pages/`. Discovery went graph-wide in the #246 fix; event routing did not
/// follow, so an external edit to a page at the graph root or in a custom folder
/// was watched and delivered and then dropped here (GH #268).
fn pending_for_graph(paths: &HashSet<PathBuf>, graph: &Graph) -> HashSet<PathBuf> {
    paths
        .iter()
        .filter(|path| graph.graph_text_watch_relevant(path))
        .cloned()
        .collect()
}

/// Split the pending *unclassified* paths this graph owns — renames and event
/// kinds that did not say file or directory — by what they can reach:
/// `(subtrees, files)`. A subtree forces a full diff; a single file is exact
/// and joins the incremental set, so a renamed-away page is reconciled as the
/// one path it is. A path that reaches nothing (an excluded tree, the
/// configuration file, a non-page file) costs nothing. `graph_text_watch_reach`
/// is the same answer the callback observation uses (GH #543, audit R9-05).
fn unclassified_paths_for_graph(
    paths: &HashSet<PathBuf>,
    graph: &Graph,
) -> (HashSet<PathBuf>, HashSet<PathBuf>) {
    let mut subtrees = HashSet::new();
    let mut files = HashSet::new();
    for path in paths {
        match graph.graph_text_watch_reach(path) {
            GraphTextWatchReach::Nothing => {}
            GraphTextWatchReach::File => {
                files.insert(path.clone());
            }
            GraphTextWatchReach::Subtree => {
                subtrees.insert(path.clone());
            }
        }
    }
    (subtrees, files)
}

fn observe_graph_text_event(graph: &Graph, root: &Path, event: Option<&notify::Event>) -> bool {
    let observation = graph_text_observation(graph, root, event);
    if !observation.relevant {
        return false;
    }
    // The raw platform callback is normally only an admission barrier. Reading
    // and semantically parsing arbitrary exact paths here defeated debounce, so
    // external paths still publish one O(1) pending epoch and are read/parsed by
    // the debounced reconciler. The one bounded exception is a candidate echo of
    // a completed Tine publication: core reopens that exact path twice under the
    // writer's identity + page locks and requires both its content revision and
    // physical identity to match Tine's publication receipt (or the identical
    // already-admitted cache state). Windows can emit Create(Any), Modify(Any)
    // and rename for one atomic publication; none of those self echoes should
    // strand the next new page. Any mismatch remains an external observation.
    // Only genuinely ambiguous events invalidate the retained identity index.
    if observation.uncertain {
        graph.note_graph_text_external_observation();
        let _ = graph.observe_graph_text_external_paths(std::iter::empty::<&Path>(), true);
    } else if !observation.exact_paths.is_empty() {
        let all_match_tine = observation
            .exact_paths
            .iter()
            .all(|path| graph.exact_graph_text_event_matches_tine_state(path));
        if !all_match_tine {
            graph.note_graph_text_external_observation();
        }
    }
    true
}

mod runtime;

#[cfg(test)]
use runtime::rewalks_root;
pub(crate) use runtime::start_watcher;
#[cfg(test)]
use runtime::{default_watch_mode, route_drained_direct_frontiers, WatchedGraph};
use runtime::{graph_text_observation, watch_mode};

#[tauri::command]
pub(crate) fn get_watch_mode(app: tauri::AppHandle) -> String {
    watch_mode(&app)
}

#[tauri::command]
pub(crate) fn set_watch_mode(
    mode: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), crate::command_error::CommandError> {
    let mode = if mode == "poll" { "poll" } else { "inotify" };
    update_settings(&app, |json| {
        json["watch_mode"] = serde_json::json!(mode);
    })?;
    // Wake the watcher so it switches mechanism right away.
    if let Some(tx) = state.watch_ctl.lock().unwrap().as_ref() {
        let _ = tx.send(());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
