use crate::settings::{settings_path, update_settings};
use crate::state::{
    refresh_graph_for_label, slot_for_window, AppState, GraphSlot, RefreshLaneWait, RefreshOutcome,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use tauri::{Emitter, Manager, State};
use tine_core::{
    model::GraphTextExactFeedPathClass, model::GraphTextExternalObservationTicket, model::PageKind,
    Graph,
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
/// So bound the wait whenever a desired root is unwatched. (Direct Files
/// data-safety audit 2026-08-09, finding 16, in its reachable form: the blindness
/// is not permanent and there IS a poll fallback, but only when EVERY root fails.)
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

/// Re-read `logseq/config.edn` for the graphs an event named, and refresh any
/// whose configuration actually moved.
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
        let Ok(slot) = slot_for_window(&state, label) else {
            continue;
        };
        // A Direct graph carries a digest of the exact bytes it was opened
        // with, so this costs nothing after Tine's own settings write: that
        // command already refreshed the slot, and the reopened graph matches
        // disk. Skipping here is what keeps a settings toggle from paying for
        // a second whole-graph reopen -- which discards every cache the graph
        // has built.
        let disk = tine_core::model::config_file_description(root);
        let unchanged = {
            let lease = slot.graph();
            // Either the graph was opened with these exact bytes, or it
            // published them itself. The second case is what keeps a star
            // toggled in the sidebar from reading as an outside change.
            lease.open_config_description() == disk || lease.recent_config_write() == disk
        };
        if unchanged {
            continue;
        }
        let before = slot.graph_meta();
        drop(slot);
        match refresh_graph_for_label(&state, app, label, RefreshLaneWait::TryOnce) {
            Ok(RefreshOutcome::Deferred) => {
                recheck.insert(label.clone());
                deferred = true;
            }
            Ok(RefreshOutcome::Refreshed) => {
                let Ok(slot) = slot_for_window(&state, label) else {
                    continue;
                };
                let after = slot.graph_meta();
                // A rewrite that changed no setting we surface -- Logseq
                // touching an unrelated key, Syncthing redelivering identical
                // bytes with a new mtime -- announces nothing.
                if after != before {
                    let _ = app.emit_to(label, "graph-config-changed", after);
                }
            }
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

/// Which pending *unclassified* paths this graph owns — directory moves and
/// event kinds the watcher cannot resolve. These force a full diff, so the
/// question is only whether anything eligible could live at or under the path;
/// an excluded tree (`assets/`, a dot-directory) answers no and costs nothing.
fn full_scan_owner_for_graph(paths: &HashSet<PathBuf>, graph: &Graph) -> HashSet<PathBuf> {
    paths
        .iter()
        .filter(|path| graph.graph_text_watch_could_contain(path))
        .cloned()
        .collect()
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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn unset_watch_mode_prefers_native_events_on_every_platform() {
        assert_eq!(default_watch_mode(), "inotify");
    }
    use tine_core::model::{BlockDto, Format, PageDto};
    #[test]
    fn atomic_page_save_temp_events_stay_incremental() {
        use notify::event::{EventKind, ModifyKind, RenameMode};

        let page = PathBuf::from("/graphs/a/pages/one.md");
        // `create_projection_temp` uses this exact suffix. Keeping the producer's
        // spelling here matters: the old test used a retired `.tmp` form and let
        // every real atomic rename fall through to a full-graph scan.
        let temp = PathBuf::from("/graphs/a/pages/.one.md.123.7.projection.tmp");
        let mut pending = Pending::default();
        pending.add_event(notify::Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            paths: vec![temp, page.clone()],
            attrs: Default::default(),
        });

        assert!(
            !pending.need_full,
            "Tine's own temp rename must not request a full scan"
        );
        assert_eq!(pending.paths, HashSet::from([page]));
    }

    /// GH #366. Windows reports ordinary file writes as `Create(Any)` or
    /// `Modify(Any)`. The raw admission classifier already knew these were exact
    /// page paths, but the queued classifier sent the same event through a full
    /// graph diff. On a large graph that kept creation's external-change barrier
    /// raised past every frontend retry.
    #[test]
    fn windows_exact_page_events_stay_incremental_through_the_pending_queue() {
        use notify::event::{CreateKind, EventKind, ModifyKind};

        let graph_dir = TempGraph::new("windows-pending-exact-page");
        graph_dir.write("pages/TINE版本更新提示词.md", "- 中文内容\n");
        let page = graph_dir.path("pages/TINE版本更新提示词.md");

        for kind in [
            EventKind::Create(CreateKind::Any),
            EventKind::Modify(ModifyKind::Any),
        ] {
            let mut pending = Pending::default();
            pending.add_event(event(kind, vec![page.clone()]));
            assert_eq!(pending.paths, HashSet::from([page.clone()]), "{kind:?}");
            assert!(pending.full_paths.is_empty(), "{kind:?}");
            assert!(!pending.need_full, "{kind:?}");
        }
    }

    #[test]
    fn windows_unicode_event_reconciles_and_releases_new_page_creation() {
        use notify::event::{CreateKind, EventKind};

        let graph_dir = TempGraph::new("windows-unicode-frontier-release");
        graph_dir.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&graph_dir.root);
        warm_direct_graph(&graph);
        let mut snap = collect_graph_text_files(&graph).files;

        graph_dir.write("pages/TINE版本更新提示词.md", "- 中文内容\n");
        let path = graph_dir.path("pages/TINE版本更新提示词.md");
        let create = event(EventKind::Create(CreateKind::Any), vec![path.clone()]);
        assert!(observe_graph_text_event(
            &graph,
            &graph_dir.root,
            Some(&create),
        ));
        let frontier = graph.graph_text_external_observation_ticket();
        assert_new_page_waits_for_reconciliation(&graph, "Blocked During Windows Event");

        let mut pending = Pending::default();
        pending.add_event(create);
        let (changes, _conflicts, used_full, errors) =
            reconcile_pending(&graph, &mut snap, &pending.paths, false, false);
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!used_full, "one exact Windows page event must stay O(page)");
        assert_eq!(changes.len(), 1);
        assert!(graph.acknowledge_graph_text_external_observations(frontier));

        graph
            .save_page(&new_page("Creation After Windows Event"), None)
            .unwrap();
        assert!(graph_dir
            .path("pages/Creation After Windows Event.md")
            .exists());
    }

    /// GH #374 negative follow-up to #366. Windows reports the atomic
    /// publication of Tine's own new journal as an exact graph-text event. That
    /// echo must not raise the external-change admission frontier and strand the
    /// next new page before the debounced reconciler sees the journal bytes.
    #[test]
    fn windows_tine_owned_create_echoes_do_not_block_following_pages_or_journals() {
        use notify::event::{CreateKind, EventKind, ModifyKind, RenameMode};

        let cases = [
            (
                "journal-page",
                new_journal("Aug 25th, 2026"),
                new_page("20260825100915"),
            ),
            ("page-page-unicode", new_page("第一页"), new_page("第二页")),
            (
                "page-journal",
                new_page("Before Journal"),
                new_journal("Aug 24th, 2026"),
            ),
        ];
        for (case, first, second) in cases {
            let graph_dir = TempGraph::new(&format!("windows-owned-{case}"));
            graph_dir.write("pages/Anchor.md", "- anchor\n");
            let graph = Graph::open(&graph_dir.root);
            warm_direct_graph(&graph);

            graph.save_page(&first, None).unwrap();
            let first_path = graph
                .find_entry(&first.name, first.kind)
                .expect("created first entry")
                .path;
            let before = graph.graph_text_external_observation_ticket();
            // ReadDirectoryChangesW can surface several exact shapes for the
            // one atomic publication before (and occasionally just after) the
            // debounce pass. Every one must validate the same exact receipt.
            for kind in [
                EventKind::Create(CreateKind::Any),
                EventKind::Modify(ModifyKind::Any),
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            ] {
                let paths = if matches!(&kind, EventKind::Modify(ModifyKind::Name(_))) {
                    let filename = first_path.file_name().unwrap().to_string_lossy();
                    vec![
                        first_path.with_file_name(format!(".{filename}.123.7.projection.tmp")),
                        first_path.clone(),
                    ]
                } else {
                    vec![first_path.clone()]
                };
                assert!(observe_graph_text_event(
                    &graph,
                    &graph_dir.root,
                    Some(&event(kind, paths)),
                ));
                assert_eq!(
                    graph.graph_text_external_observation_ticket(),
                    before,
                    "{case}: Tine's exact publication echo must not become an external frontier"
                );
            }
            graph
                .sync_file_checked(&first_path)
                .expect("debounced self-write reconciliation");
            assert!(observe_graph_text_event(
                &graph,
                &graph_dir.root,
                Some(&event(EventKind::Modify(ModifyKind::Any), vec![first_path],)),
            ));
            assert_eq!(
                graph.graph_text_external_observation_ticket(),
                before,
                "{case}: a delayed duplicate matching the admitted cache state remains a no-op"
            );

            graph
                .save_page(&second, None)
                .unwrap_or_else(|error| panic!("{case}: following creation failed: {error}"));
        }
    }

    #[test]
    fn windows_external_replacement_of_tine_publication_keeps_creation_blocked() {
        use notify::event::{EventKind, ModifyKind};

        for same_bytes in [false, true] {
            let graph_dir = TempGraph::new(if same_bytes {
                "windows-external-same-bytes-new-identity"
            } else {
                "windows-external-different-bytes"
            });
            graph_dir.write("pages/Anchor.md", "- anchor\n");
            let graph = Graph::open(&graph_dir.root);
            warm_direct_graph(&graph);
            let first = new_page("First Publication");
            graph.save_page(&first, None).unwrap();
            let first_path = graph
                .find_entry(&first.name, first.kind)
                .expect("created page entry")
                .path;
            if same_bytes {
                let bytes = std::fs::read(&first_path).unwrap();
                let replacement = graph_dir.path("external-winner.tmp");
                std::fs::write(&replacement, bytes).unwrap();
                std::fs::remove_file(&first_path).unwrap();
                std::fs::rename(replacement, &first_path).unwrap();
            } else {
                std::fs::write(&first_path, "- external winner\n").unwrap();
            }

            assert!(observe_graph_text_event(
                &graph,
                &graph_dir.root,
                Some(&event(EventKind::Modify(ModifyKind::Any), vec![first_path],)),
            ));
            assert_new_page_waits_for_reconciliation(
                &graph,
                if same_bytes {
                    "Blocked By New Physical Owner"
                } else {
                    "Blocked By Different External Bytes"
                },
            );
        }
    }

    #[test]
    fn unknown_path_event_requests_full_scan_only_for_its_owner() {
        use notify::event::{CreateKind, EventKind};

        let unknown = PathBuf::from("/graphs/a/pages/new-directory");
        let mut pending = Pending::default();
        pending.add_event(notify::Event {
            kind: EventKind::Create(CreateKind::Folder),
            paths: vec![unknown.clone()],
            attrs: Default::default(),
        });

        assert!(!pending.need_full);
        assert_eq!(pending.full_paths, HashSet::from([unknown]));
        assert!(pending.paths.is_empty());
    }

    #[test]
    fn pending_stamps_the_first_event_of_a_batch_once() {
        use notify::event::{CreateKind, EventKind};

        let mut pending = Pending::default();
        assert!(pending.first_event_at.is_none());
        pending.add_event(notify::Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![PathBuf::from("/graphs/a/pages/one.md")],
            attrs: Default::default(),
        });
        let first = pending
            .first_event_at
            .expect("first event stamps the batch");
        pending.add_event(notify::Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![PathBuf::from("/graphs/a/pages/two.md")],
            attrs: Default::default(),
        });
        assert_eq!(
            pending.first_event_at,
            Some(first),
            "later events in the same batch must not move the batch stamp"
        );
        pending.add_notify_error();
        assert_eq!(pending.first_event_at, Some(first));
    }

    #[test]
    fn latency_receipt_measures_the_three_stages() {
        let first = Instant::now();
        let reconcile_started = first + Duration::from_millis(200);
        let emitted_at = reconcile_started + Duration::from_millis(35);

        let receipt = latency_receipt(
            "graph-a",
            true,
            3,
            4,
            false,
            1,
            Some(first),
            reconcile_started,
            emitted_at,
        );

        assert_eq!(receipt.graph, "graph-a");
        assert_eq!(receipt.mode, "inotify");
        assert_eq!(receipt.pages, 3);
        assert_eq!(receipt.event_paths, 4);
        assert!(!receipt.full_diff);
        assert_eq!(receipt.errors, 1);
        assert_eq!(receipt.event_to_reconcile_ms, Some(200));
        assert_eq!(receipt.reconcile_ms, 35);
        assert_eq!(receipt.event_to_emit_ms, Some(235));
    }

    #[test]
    fn latency_receipt_without_a_callback_stamp_reports_only_reconcile_time() {
        let reconcile_started = Instant::now();
        let emitted_at = reconcile_started + Duration::from_millis(12);

        let receipt = latency_receipt(
            "graph-a",
            false,
            2,
            0,
            true,
            0,
            None,
            reconcile_started,
            emitted_at,
        );

        assert_eq!(receipt.mode, "poll");
        assert!(receipt.full_diff);
        assert_eq!(receipt.event_to_reconcile_ms, None);
        assert_eq!(receipt.event_to_emit_ms, None);
        assert_eq!(receipt.reconcile_ms, 12);
    }

    #[test]
    fn latency_receipt_ring_keeps_the_newest_sixty_four() {
        let now = Instant::now();
        let mut ring = VecDeque::new();
        for seq in 1..=(LATENCY_RECEIPT_CAP as u64 + 6) {
            let mut receipt = latency_receipt("graph-a", true, 1, 1, false, 0, None, now, now);
            receipt.seq = seq;
            push_latency_receipt(&mut ring, receipt);
        }
        assert_eq!(ring.len(), LATENCY_RECEIPT_CAP);
        assert_eq!(ring.front().map(|receipt| receipt.seq), Some(7));
        assert_eq!(
            ring.back().map(|receipt| receipt.seq),
            Some(LATENCY_RECEIPT_CAP as u64 + 6)
        );
    }

    /// The receipt is a diagnostic wire format a reporter pastes into an issue;
    /// its field names are part of that contract.
    #[test]
    fn latency_receipt_wire_shape_is_stable() {
        let now = Instant::now();
        let receipt = latency_receipt("graph-a", true, 1, 1, false, 0, Some(now), now, now);
        let wire = serde_json::to_value(&receipt).unwrap();
        for key in [
            "seq",
            "at_unix_ms",
            "graph",
            "mode",
            "pages",
            "event_paths",
            "full_diff",
            "errors",
            "event_to_reconcile_ms",
            "reconcile_ms",
            "event_to_emit_ms",
        ] {
            assert!(wire.get(key).is_some(), "missing receipt field {key}");
        }
    }

    #[test]
    fn explicit_non_graph_text_file_events_do_not_schedule_graph_scans() {
        use notify::event::{CreateKind, EventKind, RemoveKind};

        for (kind, path, asset_paths) in [
            (
                EventKind::Create(CreateKind::File),
                PathBuf::from("/graphs/a/assets/image.png"),
                1,
            ),
            (
                EventKind::Remove(RemoveKind::File),
                PathBuf::from("/graphs/a/logseq/config.edn"),
                1,
            ),
        ] {
            let mut pending = Pending::default();
            pending.add_event(notify::Event {
                kind,
                paths: vec![path],
                attrs: Default::default(),
            });
            assert!(!pending.need_full);
            assert!(pending.full_paths.is_empty());
            assert!(pending.paths.is_empty());
            assert_eq!(
                pending.asset_paths.len(),
                asset_paths,
                "ordinary exact files must remain available to the separate asset observer"
            );
        }
    }

    #[test]
    fn asset_observation_names_only_paths_inside_the_asset_capability() {
        let root = Path::new("/graphs/a/assets");
        assert_eq!(
            asset_relative_event_path(root, Path::new("/graphs/a/assets/diagrams/flow.svg")),
            Some("diagrams/flow.svg".into())
        );
        assert_eq!(
            asset_relative_event_path(root, Path::new("/graphs/a/pages/flow.svg")),
            None
        );
        assert_eq!(asset_relative_event_path(root, root), None);
    }

    #[cfg(unix)]
    #[test]
    fn external_asset_alias_events_refresh_replacement_and_deletion() {
        let graph = TempGraph::new("asset-watch-lexical-alias");
        let canonical = graph.path("external");
        let lexical = graph.path("assets");
        std::fs::create_dir_all(&canonical).unwrap();
        std::os::unix::fs::symlink(&canonical, &lexical).unwrap();
        let image = canonical.join("pixel.png");
        std::fs::write(&image, b"same bytes").unwrap();
        let mut state = AssetWatchState::new(canonical.clone());
        let empty = HashSet::new();
        // notify's recursive graph watch follows this symlink and may assign
        // its lexical path to the same descriptor as the canonical watch.
        let mut exact = HashSet::from([lexical.join("pixel.png")]);
        normalize_asset_event_aliases(
            std::iter::once((graph.path("").as_path(), &state)),
            &mut exact,
            &mut HashSet::new(),
        );
        std::fs::write(canonical.join("replacement"), b"same bytes").unwrap();
        std::fs::rename(canonical.join("replacement"), &image).unwrap();
        assert_eq!(
            reconcile_asset_observation("alias", &mut state, &exact, &empty, false, false),
            vec!["pixel.png"],
        );
        std::fs::remove_file(&image).unwrap();
        let mut exact = HashSet::from([lexical.join("pixel.png")]);
        normalize_asset_event_aliases(
            std::iter::once((graph.path("").as_path(), &state)),
            &mut exact,
            &mut HashSet::new(),
        );
        assert_eq!(
            reconcile_asset_observation("alias", &mut state, &exact, &empty, false, false),
            vec!["pixel.png"],
        );
    }

    #[test]
    fn asset_alias_routing_refreshes_all_shared_root_windows_and_keeps_scope() {
        let graph = TempGraph::new("asset-alias-shared");
        let first_root = graph.path("first");
        let second_root = graph.path("second");
        let inactive_root = graph.path("inactive");
        let canonical = graph.path("external");
        std::fs::create_dir_all(&canonical).unwrap();
        let image = canonical.join("pixel.png");
        std::fs::write(&image, b"old").unwrap();
        let mut first = AssetWatchState::new(canonical.clone());
        let mut second = AssetWatchState::new(canonical.clone());
        let inactive = AssetWatchState::default();
        let lexical_event = second_root.join("assets/pixel.png");
        let mut exact = HashSet::from([
            lexical_event.clone(),
            first_root.join("assets/../outside.png"),
            inactive_root.join("assets/unapproved.png"),
            graph.path("unbound/assets/unknown.png"),
        ]);
        normalize_asset_event_aliases(
            [
                (first_root.as_path(), &first),
                (second_root.as_path(), &second),
                (inactive_root.as_path(), &inactive),
            ]
            .into_iter(),
            &mut exact,
            &mut HashSet::new(),
        );
        assert_eq!(exact.len(), 5, "only the approved pixel alias adds a path");
        assert!(
            exact.contains(&lexical_event),
            "preserve the original event"
        );
        assert!(exact.contains(&image));
        std::fs::write(&image, b"replacement image").unwrap();
        for (label, state) in [("first", &mut first), ("second", &mut second)] {
            assert_eq!(
                reconcile_asset_observation(label, state, &exact, &HashSet::new(), false, false),
                vec!["pixel.png"]
            );
        }
    }

    #[test]
    fn asset_alias_directory_deletion_routes_without_following_missing_paths() {
        let graph = TempGraph::new("asset-alias-directory");
        let root = graph.path("graph");
        let canonical = graph.path("external");
        std::fs::create_dir_all(canonical.join("nested")).unwrap();
        std::fs::write(canonical.join("nested/image.png"), b"image").unwrap();
        let mut state = AssetWatchState::new(canonical.clone());
        std::fs::remove_dir_all(canonical.join("nested")).unwrap();
        let mut full = HashSet::from([root.join("assets/nested")]);
        normalize_asset_event_aliases(
            std::iter::once((root.as_path(), &state)),
            &mut HashSet::new(),
            &mut full,
        );
        assert!(full.contains(&canonical.join("nested")));
        assert_eq!(
            reconcile_asset_observation(
                "directory",
                &mut state,
                &HashSet::new(),
                &full,
                false,
                false
            ),
            vec!["nested/image.png"]
        );
        let mut parent = HashSet::from([root.clone()]);
        normalize_asset_event_aliases(
            std::iter::once((root.as_path(), &state)),
            &mut HashSet::new(),
            &mut parent,
        );
        assert!(
            parent.contains(&canonical),
            "uncertain root events include the approved asset root"
        );
    }

    #[test]
    fn asset_observation_refreshes_peers_but_suppresses_the_originating_self_write() {
        let graph = TempGraph::new("asset-self-write");
        let assets = graph.path("assets");
        std::fs::create_dir_all(&assets).unwrap();
        let mut origin = AssetWatchState::new(assets.clone());
        let mut peer = AssetWatchState::new(assets.clone());
        let empty = HashSet::new();
        assert!(
            reconcile_asset_observation("main", &mut origin, &empty, &empty, true, false,)
                .is_empty()
        );
        assert!(
            reconcile_asset_observation("second", &mut peer, &empty, &empty, true, false,)
                .is_empty()
        );

        let image = assets.join("nested/image.png");
        std::fs::create_dir_all(image.parent().unwrap()).unwrap();
        std::fs::write(&image, b"new image bytes").unwrap();
        note_asset_self_write("main", &image);
        let exact = HashSet::from([image]);

        assert_eq!(
            reconcile_asset_observation("second", &mut peer, &exact, &empty, false, false),
            vec!["nested/image.png"]
        );
        assert!(
            reconcile_asset_observation("main", &mut origin, &exact, &empty, false, false,)
                .is_empty()
        );
    }

    #[test]
    fn asset_read_before_watcher_binding_remains_the_refresh_baseline() {
        let graph = TempGraph::new("asset-read-before-watch");
        let assets = graph.path("assets");
        std::fs::create_dir_all(&assets).unwrap();
        let image = assets.join("startup.png");
        std::fs::write(&image, b"bytes rendered before watcher binding").unwrap();
        note_asset_read("main", &image);

        // An external synchronizer replaces the file before AssetWatchState is
        // constructed. A fresh directory snapshot alone would baseline these
        // new bytes and lose the invalidation for the already-rendered image.
        let replacement = assets.join("startup.replacement");
        std::fs::write(&replacement, b"replacement bytes").unwrap();
        std::fs::rename(&replacement, &image).unwrap();
        let mut state = AssetWatchState::new(assets);

        assert_eq!(
            reconcile_asset_observation(
                "main",
                &mut state,
                &HashSet::new(),
                &HashSet::new(),
                false,
                false,
            ),
            vec!["startup.png"]
        );
    }

    #[test]
    fn asset_read_handoff_compares_only_the_assets_that_were_read() {
        let graph = TempGraph::new("asset-read-exact-handoff");
        let assets = graph.path("assets");
        std::fs::create_dir_all(&assets).unwrap();
        let rendered = assets.join("rendered.png");
        let unrelated = assets.join("unrelated.png");
        std::fs::write(&rendered, b"rendered old").unwrap();
        std::fs::write(&unrelated, b"unrelated old").unwrap();
        let mut state = AssetWatchState::new(assets);
        note_asset_read("exact-handoff", &rendered);

        std::fs::write(&rendered, b"rendered replacement").unwrap();
        std::fs::write(&unrelated, b"unrelated replacement").unwrap();

        assert_eq!(
            reconcile_asset_observation(
                "exact-handoff",
                &mut state,
                &HashSet::new(),
                &HashSet::new(),
                false,
                false,
            ),
            vec!["rendered.png"],
            "a WebView read seed must not trigger a whole asset-tree scan"
        );
    }

    #[cfg(unix)]
    #[test]
    fn self_write_marker_canonicalizes_an_approved_external_assets_link() {
        use std::os::unix::fs::symlink;

        let graph = TempGraph::new("asset-self-write-external");
        let external = TempGraph::new("asset-self-write-external-target");
        let external_assets = external.path("approved-assets");
        std::fs::create_dir_all(&external_assets).unwrap();
        symlink(&external_assets, graph.path("assets")).unwrap();
        let mut state = AssetWatchState::new(external_assets.clone());
        let image = external_assets.join("linked.png");
        std::fs::write(&image, b"linked image").unwrap();
        note_asset_self_write("main", &graph.path("assets/linked.png"));

        assert!(reconcile_asset_observation(
            "main",
            &mut state,
            &HashSet::from([image]),
            &HashSet::new(),
            false,
            false,
        )
        .is_empty());
    }

    #[test]
    fn uncertain_asset_rescan_is_metadata_only_and_does_not_invent_deletions() {
        let source = crate::test_support::rust_module_production_source("watcher.rs");
        let body = source
            .split_once("fn collect_asset_files(")
            .unwrap()
            .1
            .split_once("fn asset_relative_event_path(")
            .unwrap()
            .0;
        assert!(!body.contains("std::fs::read("));
        assert!(!body.contains("Graph::"));
        assert!(!body.contains("observe_watcher"));

        let graph = TempGraph::new("asset-full-diff");
        let assets = graph.path("assets");
        std::fs::create_dir_all(&assets).unwrap();
        graph.write("assets/one.png", "one");
        let mut state = AssetWatchState::new(assets);
        let empty = HashSet::new();
        assert!(
            reconcile_asset_observation("main", &mut state, &empty, &empty, true, false,)
                .is_empty()
        );
        graph.write("assets/one.png", "changed-length");
        graph.write("assets/two.svg", "two");
        assert_eq!(
            reconcile_asset_observation("main", &mut state, &empty, &empty, true, false),
            vec!["one.png", "two.svg"]
        );
    }

    #[test]
    fn markdown_and_case_variant_text_events_stay_incremental() {
        use notify::event::{CreateKind, EventKind};

        let paths = vec![
            PathBuf::from("/graphs/a/archive/one.markdown"),
            PathBuf::from("/graphs/a/archive/two.MD"),
            PathBuf::from("/graphs/a/archive/three.ORG"),
        ];
        let mut pending = Pending::default();
        pending.add_event(notify::Event {
            kind: EventKind::Create(CreateKind::File),
            paths: paths.clone(),
            attrs: Default::default(),
        });
        assert_eq!(pending.paths, paths.into_iter().collect());
        assert!(pending.full_paths.is_empty());
        assert!(!pending.need_full);
    }

    /// Concord P5: what the watcher admits from `.git/**` and its equivalents,
    /// stated explicitly and tested. A repository parked in the graph tree is
    /// the loudest event source a Direct Files user has; none of its churn can
    /// describe graph text, so none of it may cost anything.
    #[test]
    fn vcs_and_tool_churn_never_wakes_the_watcher() {
        use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind};

        let roots = HashSet::from([PathBuf::from("/graphs/a")]);
        let noise: &[(EventKind, &str)] = &[
            (
                EventKind::Create(CreateKind::File),
                "/graphs/a/.git/index.lock",
            ),
            (
                EventKind::Remove(RemoveKind::File),
                "/graphs/a/.git/index.lock",
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                "/graphs/a/.git/objects/ab/cdef0123456789",
            ),
            (
                EventKind::Create(CreateKind::Any),
                "/graphs/a/.git/refs/heads/main",
            ),
            (
                EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                "/graphs/a/.hg/store/data/page.md.i",
            ),
            (
                EventKind::Create(CreateKind::File),
                "/graphs/a/.jj/repo/op_store/x",
            ),
            (EventKind::Create(CreateKind::File), "/graphs/a/.svn/wc.db"),
            (
                EventKind::Create(CreateKind::File),
                "/graphs/a/.stversions/pages/Note~20260818.md",
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                "/graphs/a/.stfolder/marker",
            ),
            (
                EventKind::Create(CreateKind::File),
                "/graphs/a/node_modules/pkg/readme.md",
            ),
        ];
        for (kind, path) in noise {
            let event = notify::Event {
                kind: *kind,
                paths: vec![PathBuf::from(path)],
                attrs: Default::default(),
            };
            assert!(
                watch_event_is_tool_noise(&event, &roots),
                "{path} must not wake the watcher"
            );
        }

        // Everything else still gets through, including the cases a name list
        // is most likely to over-reach on.
        let admitted: &[(EventKind, Vec<&str>)] = &[
            // An ordinary page, and configuration.
            (
                EventKind::Create(CreateKind::File),
                vec!["/graphs/a/pages/Note.md"],
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                vec!["/graphs/a/logseq/config.edn"],
            ),
            // A rename OUT of .git reports both sides: one ordinary path is
            // enough to keep the whole event.
            (
                EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Both)),
                vec!["/graphs/a/.git/tmp_obj_x", "/graphs/a/pages/Note.md"],
            ),
            // A path under no watched root is not ours to judge.
            (
                EventKind::Create(CreateKind::File),
                vec!["/elsewhere/.git/index"],
            ),
        ];
        for (kind, paths) in admitted {
            let event = notify::Event {
                kind: *kind,
                paths: paths.iter().map(PathBuf::from).collect(),
                attrs: Default::default(),
            };
            assert!(
                !watch_event_is_tool_noise(&event, &roots),
                "{paths:?} must still be seen"
            );
        }

        // A kernel queue overflow says nothing about which paths were lost, so
        // its rescan demand survives even when its paths look like noise.
        let mut overflow = notify::Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![PathBuf::from("/graphs/a/.git/objects")],
            attrs: Default::default(),
        };
        overflow = overflow.set_flag(notify::event::Flag::Rescan);
        assert!(!watch_event_is_tool_noise(&overflow, &roots));

        // With no watched root there is nothing to strip a prefix against.
        let event = notify::Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![PathBuf::from("/graphs/a/.git/index.lock")],
            attrs: Default::default(),
        };
        assert!(!watch_event_is_tool_noise(&event, &HashSet::new()));
    }

    /// The list is only safe because every name on it is provably outside graph
    /// text on its own authority — the core's `GraphTextScope`, which discovery
    /// and the full-diff walk also use. If a name ever became page-bearing,
    /// dropping its events would hide pages; this fails first.
    #[test]
    fn vcs_and_tool_noise_dirs_can_never_hold_graph_text() {
        let scope = tine_core::graph_text_scope::GraphTextScope::new(&[], false);
        for name in VCS_AND_TOOL_NOISE_DIRS {
            assert!(
                !scope.should_descend(name),
                "{name} must be outside graph text"
            );
            assert!(
                !scope.is_eligible(&format!("{name}/Page.md")),
                "{name}/Page.md must never be a page"
            );
            assert!(
                !scope.is_eligible(&format!("pages/{name}/Page.md")),
                "pages/{name}/Page.md must never be a page"
            );
        }
        // ...and the one Tine-owned hidden tree that is deliberately NOT here.
        assert!(!VCS_AND_TOOL_NOISE_DIRS.contains(&".tine-sync"));
    }

    /// A graph that itself lives inside a repository's directory must not have
    /// every one of its own events dropped.
    #[test]
    fn a_graph_under_a_noise_directory_is_judged_relative_to_its_root() {
        use notify::event::{CreateKind, EventKind};

        let roots = HashSet::from([PathBuf::from("/repo/.git/notes")]);
        let event = notify::Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![PathBuf::from("/repo/.git/notes/pages/Note.md")],
            attrs: Default::default(),
        };
        assert!(!watch_event_is_tool_noise(&event, &roots));
    }

    #[test]
    fn hidden_sync_events_do_not_schedule_direct_files_reconciliation() {
        use notify::event::{CreateKind, EventKind};

        let chunk =
            PathBuf::from("/graphs/a/.tine-sync/v1/devices/device/sessions/session/0001.chunk");
        let mut pending = Pending::default();
        pending.add_event(notify::Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![chunk.clone()],
            attrs: Default::default(),
        });

        assert!(!pending.need_full);
        assert!(pending.full_paths.is_empty());
        assert!(pending.paths.is_empty());
    }

    #[test]
    fn notify_failures_remain_distinct_from_rescan_obligations() {
        let mut pending = Pending::default();
        pending.add_notify_error();
        assert!(pending.need_full);
        assert!(pending.notify_error);
    }

    #[test]
    fn failed_reconciliation_retries_without_another_filesystem_event() {
        let start = Instant::now();
        let mut retry = RetrySchedule::default();
        retry.failed(start);
        assert!(!retry.take_due(start));
        assert!(retry.take_due(start + RETRY_BACKOFF[0]));

        retry.failed(start + RETRY_BACKOFF[0]);
        assert_eq!(
            retry.remaining(start + RETRY_BACKOFF[0]),
            Some(RETRY_BACKOFF[1])
        );
        retry.succeeded();
        assert_eq!(retry.remaining(start), None);
    }

    #[test]
    fn reconciliation_backoff_is_capped_but_keeps_scheduling() {
        let start = Instant::now();
        let mut retry = RetrySchedule::default();
        for offset in 0..20 {
            retry.failed(start + Duration::from_secs(offset));
        }
        assert_eq!(
            retry.remaining(start + Duration::from_secs(19)),
            Some(*RETRY_BACKOFF.last().unwrap())
        );
    }

    // Direct Files data-safety audit 2026-08-09, finding 16, in its reachable
    // form. The blindness the audit predicted is not permanent and there IS a
    // fallback — but only when EVERY root fails, which empties `watched` and
    // takes the bounded poll branch. The reachable case is mixed: with two
    // graphs open (inotify watch limits are per-user, so a second large graph is
    // exactly how one root fails while the other is fine), the inotify branch
    // blocked forever on an event from the HEALTHY root, and the failing graph
    // stayed invisible until that other graph happened to change.
    //
    // Two gaps, stated rather than papered over. These cover the wait POLICY,
    // not a real kernel `watch()` failure — exhausting fs.inotify.max_user_watches
    // needs privileges this box does not have — and not the wiring that feeds
    // `desired.difference(&watched)` into it. They are therefore a specification
    // of the rule, not fail-before evidence: `inotify_cycle_wait` did not exist
    // before this change, so there is no earlier build they could have failed
    // against. Do not read them as a regression guard for the loop itself.
    #[test]
    fn an_unwatched_root_bounds_the_wait_so_its_retry_actually_runs() {
        // Everything watched: block until the kernel says something, as before.
        assert_eq!(inotify_cycle_wait(None, false), None);
        // A root we want and do not have: never block indefinitely.
        assert_eq!(inotify_cycle_wait(None, true), Some(UNWATCHED_ROOT_RETRY));
    }

    #[test]
    fn an_unwatched_root_never_delays_a_sooner_scheduled_retry() {
        let sooner = Duration::from_millis(250);
        assert_eq!(inotify_cycle_wait(Some(sooner), true), Some(sooner));
        assert_eq!(inotify_cycle_wait(Some(sooner), false), Some(sooner));
        let later = UNWATCHED_ROOT_RETRY * 4;
        assert_eq!(
            inotify_cycle_wait(Some(later), true),
            Some(UNWATCHED_ROOT_RETRY)
        );
        assert_eq!(inotify_cycle_wait(Some(later), false), Some(later));
    }

    #[test]
    fn pending_paths_are_dispatched_only_to_the_owning_graph() {
        let a = TempGraph::new("owner-a");
        let b = TempGraph::new("owner-b");
        a.write("pages/one.md", "- one\n");
        b.write("journals/2026_07_10.md", "- two\n");
        let graph_a = Graph::open(&a.root);
        let graph_b = Graph::open(&b.root);
        let paths = HashSet::from([a.path("pages/one.md"), b.path("journals/2026_07_10.md")]);
        assert_eq!(pending_for_graph(&paths, &graph_a).len(), 1);
        assert_eq!(pending_for_graph(&paths, &graph_b).len(), 1);
        assert!(pending_for_graph(&paths, &graph_a)
            .iter()
            .all(|path| path.starts_with(&a.root)));
    }

    struct TempGraph {
        root: PathBuf,
    }

    impl TempGraph {
        fn new(name: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "tine-watch-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("journals")).unwrap();
            std::fs::create_dir_all(root.join("pages")).unwrap();
            Self { root }
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.path(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }

        fn remove(&self, rel: &str) {
            std::fs::remove_file(self.path(rel)).unwrap();
        }

        fn rename(&self, from: &str, to: &str) {
            let to = self.path(to);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::rename(self.path(from), to).unwrap();
        }
    }

    impl Drop for TempGraph {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn direct_watch_paths_keep_the_bound_graph_root() {
        let temp = TempGraph::new("direct-authority");
        let slot = GraphSlot::new(Graph::open(&temp.root), temp.root.clone());

        let (graph, root) = direct_watch_paths(&slot).unwrap();
        assert_eq!(graph.root, temp.root);
        assert_eq!(root, temp.root);
    }

    fn event(kind: notify::event::EventKind, paths: Vec<PathBuf>) -> notify::Event {
        notify::Event {
            kind,
            paths,
            attrs: Default::default(),
        }
    }

    /// Configuration is deliberately not graph text, so `incremental_page_paths`
    /// throws it away — which is exactly why an outside edit to `config.edn` was
    /// invisible until the next graph open. It has to be queued separately, for
    /// every shape a writer can produce: an in-place write, and the temp+rename
    /// that Tine, Logseq and Syncthing all actually use.
    #[test]
    fn a_config_edn_write_is_queued_even_though_it_is_not_graph_text() {
        use notify::event::{CreateKind, DataChange, EventKind, ModifyKind, RenameMode};
        let config = PathBuf::from("/graph/logseq/config.edn");

        for kind in [
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            EventKind::Create(CreateKind::File),
        ] {
            let mut pending = Pending::default();
            pending.add_event(event(kind, vec![config.clone()]));
            assert!(
                pending.paths.is_empty(),
                "{kind:?}: configuration is not graph text and must not enter the page queue"
            );
            assert!(
                pending.config_paths.contains(&config),
                "{kind:?}: but it must reach the configuration queue"
            );
        }
    }

    #[test]
    fn an_ordinary_page_write_queues_no_configuration_work() {
        use notify::event::{DataChange, EventKind, ModifyKind};
        let mut pending = Pending::default();
        pending.add_event(event(
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            vec![PathBuf::from("/graph/pages/Alpha.md")],
        ));
        assert!(pending.config_paths.is_empty());
        // And an unrelated EDN file is not configuration either. The filename
        // gate is cheap and rough; `is_config_file_path` is the decision.
        let mut pending = Pending::default();
        pending.add_event(event(
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            vec![PathBuf::from("/graph/logseq/pages-metadata.edn")],
        ));
        assert!(pending.config_paths.is_empty());
    }

    fn new_page(name: &str) -> PageDto {
        PageDto {
            activation: None,
            name: name.to_owned(),
            kind: PageKind::Page,
            title: name.to_owned(),
            pre_block: None,
            blocks: vec![BlockDto {
                id: format!("watcher-{}", name.replace(' ', "-")),
                raw: "local".to_owned(),
                ..BlockDto::default()
            }],
            rev: None,
            format: Format::Md,
            read_only: false,
            path: String::new(),
            guide: false,
        }
    }

    fn new_journal(name: &str) -> PageDto {
        let mut page = new_page(name);
        page.kind = PageKind::Journal;
        page
    }

    /// Warm the parsed page cache and exercise one ordinary Direct save. This
    /// deliberately does not build the core's optional complete identity index.
    fn warm_direct_graph(graph: &Graph) {
        warm_cache(graph);
        let mut anchor = graph.load_by_path("pages/Anchor.md").unwrap().unwrap();
        anchor.blocks[0].raw = "warm guarded identity".to_owned();
        graph
            .save_page(&anchor, anchor.rev.as_deref())
            .expect("warm guarded identity save");
    }

    #[test]
    fn drained_frontier_routes_to_a_same_root_replacement_instance() {
        let graph_dir = TempGraph::new("watcher-drained-frontier-refresh");
        graph_dir.write("pages/Anchor.md", "- anchor\n");

        let old_slot = GraphSlot::new(Graph::open(&graph_dir.root), graph_dir.root.clone());
        let old_graph = old_slot.graph();
        warm_direct_graph(&old_graph);
        let old_ticket = old_graph.note_graph_text_external_observation();
        let mut graphs = HashMap::from([(
            "main".to_owned(),
            WatchedGraph {
                assets: AssetWatchState::new(old_graph.assets_path()),
                graph: old_graph,
                root: graph_dir.root.clone(),
                snap: HashMap::new(),
                baseline: true,
                last_reconcile_error: Some("retired retry".to_owned()),
                retry: RetrySchedule::default(),
                pending_observation_epoch: Some(old_ticket),
            },
        )]);

        let replacement_slot = Arc::new(GraphSlot::new(
            Graph::open(&graph_dir.root),
            graph_dir.root.clone(),
        ));
        let replacement = replacement_slot.graph();
        warm_direct_graph(&replacement);
        graph_dir.write(
            "pages/Replacement Event.md",
            "- external replacement event\n",
        );
        let external = graph_dir.path("pages/Replacement Event.md");
        let replacement_ticket = replacement.note_graph_text_external_observation();
        let drained = HashMap::from([(graph_dir.root.clone(), replacement_ticket)]);

        route_drained_direct_frontiers(
            &mut graphs,
            vec![("main".to_owned(), replacement_slot)],
            &drained,
            |slot| Some(slot.graph().assets_path()),
        );

        let routed = graphs.get_mut("main").unwrap();
        assert!(routed
            .graph
            .owns_graph_text_external_observation_ticket(replacement_ticket));
        assert!(!routed.baseline);
        assert!(routed.last_reconcile_error.is_none());
        assert!(routed.pending_observation_epoch.is_none());

        routed.pending_observation_epoch = Some(replacement_ticket);
        routed.graph.sync_file_checked(&external).unwrap();
        let reconciled = routed.pending_observation_epoch.take().unwrap();
        assert!(routed
            .graph
            .acknowledge_graph_text_external_observations(reconciled));
        routed
            .graph
            .save_page(&new_page("Creation After Refresh"), None)
            .unwrap();
        assert!(graph_dir.path("pages/Creation After Refresh.md").exists());
    }

    /// A quiet poll must publish an exact empty observation rather than an
    /// uncertain one. Whether that preserves a live guarded index belongs to
    /// `tine-core` and is tested beside the private index builder there.
    #[test]
    fn quiet_poll_cycles_publish_an_exact_empty_observation() {
        let tg = TempGraph::new("poll-warm");
        tg.write("pages/Anchor.md", "- anchor\n");
        tg.write("Root note.md", "- root\n");
        let graph = Graph::open(&tg.root);
        let snap = collect_graph_text_files(&graph).files;
        for _ in 0..3 {
            let current = collect_graph_text_files(&graph);
            let (changed, uncertain) = poll_observation(&snap, &current);
            assert!(changed.is_empty());
            assert!(!uncertain);
        }
    }

    /// The other half of the same policy: a poll cycle that did observe an
    /// external change must publish its exact path. Applying that path to a
    /// live guarded index is tested inside `tine-core`.
    #[test]
    fn a_poll_cycle_reports_what_it_actually_observed() {
        let tg = TempGraph::new("poll-observe");
        tg.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&tg.root);
        let snap = collect_graph_text_files(&graph).files;
        tg.write("Root note.md", "title:: Root note\n\n- external\n");
        let current = collect_graph_text_files(&graph);
        let (changed, uncertain) = poll_observation(&snap, &current);
        assert!(!uncertain);
        assert_eq!(changed, vec![tg.path("Root note.md")]);
    }

    /// The fallback half. If the rescan could not read part of the graph, what
    /// it found is NOT a complete account of what changed, and the index must be
    /// invalidated rather than exactly updated -- otherwise a save would trust
    /// an index that is missing whatever lives behind the unreadable directory.
    #[test]
    fn an_incomplete_poll_scan_publishes_uncertainty() {
        let tg = TempGraph::new("poll-incomplete");
        tg.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&tg.root);
        let snap = collect_graph_text_files(&graph).files;
        let mut incomplete = collect_graph_text_files(&graph);
        incomplete.complete = false;
        let (changed, uncertain) = poll_observation(&snap, &incomplete);
        assert!(changed.is_empty());
        assert!(uncertain);
    }

    /// The same thing end to end, where the walk itself decides. Root ignores
    /// directory permissions, so this can only be demonstrated when the test
    /// user is not root; it self-skips rather than passing vacuously.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_makes_the_scan_report_itself_incomplete() {
        use std::os::unix::fs::PermissionsExt;

        let tg = TempGraph::new("poll-unreadable");
        tg.write("Archive/Filed.md", "- filed\n");
        tg.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&tg.root);
        assert!(collect_graph_text_files(&graph).complete);

        let blocked = tg.path("Archive");
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable_anyway = std::fs::read_dir(&blocked).is_ok();
        let observed = collect_graph_text_files(&graph);
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();

        if readable_anyway {
            return; // running as root; permissions prove nothing here
        }
        assert!(
            !observed.complete,
            "a directory the scan could not read must make the scan report itself incomplete"
        );
    }

    fn assert_new_page_refused(graph: &Graph, name: &str) {
        assert_eq!(
            graph.save_page(&new_page(name), None).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists,
            "{name} must remain owned by the observed external graph text"
        );
    }

    fn assert_new_page_waits_for_reconciliation(graph: &Graph, name: &str) {
        assert_eq!(
            graph.save_page(&new_page(name), None).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "{name} creation must not race the watcher debounce window"
        );
    }

    fn reconcile_external_path(graph: &Graph, path: &Path) {
        let epoch = graph.graph_text_external_observation_ticket();
        graph
            .sync_file_checked(path)
            .expect("debounced exact-path reconciliation");
        graph.acknowledge_graph_text_external_observations(epoch);
    }

    #[test]
    fn graph_root_text_create_delete_rename_and_semantics_reach_guarded_identity() {
        use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};

        for extension in ["md", "org"] {
            let graph_dir = TempGraph::new(&format!("root-text-{extension}"));
            graph_dir.write("pages/Anchor.md", "- anchor\n");
            let graph = Graph::open(&graph_dir.root);
            warm_direct_graph(&graph);

            let created_rel = format!("nonstandard/deep/Physical Name.{extension}");
            graph_dir.write(
                &created_rel,
                &format!("title:: Created {extension}\n\n- external\n"),
            );
            assert!(observe_graph_text_event(
                &graph,
                &graph_dir.root,
                Some(&event(
                    EventKind::Create(CreateKind::File),
                    vec![graph_dir.path(&created_rel)],
                )),
            ));
            assert_new_page_waits_for_reconciliation(&graph, &format!("Created {extension}"));
            reconcile_external_path(&graph, &graph_dir.path(&created_rel));
            assert_new_page_refused(&graph, &format!("Created {extension}"));

            let deleted_rel = format!("nonstandard/deep/Delete {extension}.{extension}");
            graph_dir.write(&deleted_rel, "- external\n");
            observe_graph_text_event(
                &graph,
                &graph_dir.root,
                Some(&event(
                    EventKind::Create(CreateKind::File),
                    vec![graph_dir.path(&deleted_rel)],
                )),
            );
            assert_new_page_waits_for_reconciliation(&graph, &format!("Delete {extension}"));
            reconcile_external_path(&graph, &graph_dir.path(&deleted_rel));
            assert_new_page_refused(&graph, &format!("Delete {extension}"));
            graph_dir.remove(&deleted_rel);
            let delete_event = event(
                EventKind::Remove(RemoveKind::File),
                vec![graph_dir.path(&deleted_rel)],
            );
            let deletion = graph_text_observation(&graph, &graph_dir.root, Some(&delete_event));
            assert!(!deletion.uncertain);
            assert_eq!(deletion.exact_paths, vec![graph_dir.path(&deleted_rel)]);
            observe_graph_text_event(&graph, &graph_dir.root, Some(&delete_event));
            assert_new_page_waits_for_reconciliation(&graph, &format!("Delete {extension}"));
            let delete_epoch = graph.graph_text_external_observation_ticket();
            graph
                .sync_deleted_file(&graph_dir.path(&deleted_rel))
                .expect("debounced deletion reconciliation");
            graph.acknowledge_graph_text_external_observations(delete_epoch);

            let old_rel = format!("nonstandard/deep/Old {extension}.{extension}");
            let new_rel = format!("nonstandard/deep/New {extension}.{extension}");
            graph_dir.write(&old_rel, "- external\n");
            observe_graph_text_event(
                &graph,
                &graph_dir.root,
                Some(&event(
                    EventKind::Create(CreateKind::File),
                    vec![graph_dir.path(&old_rel)],
                )),
            );
            assert_new_page_waits_for_reconciliation(&graph, &format!("Old {extension}"));
            reconcile_external_path(&graph, &graph_dir.path(&old_rel));
            assert_new_page_refused(&graph, &format!("Old {extension}"));
            graph_dir.rename(&old_rel, &new_rel);
            let rename_event = event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                vec![graph_dir.path(&old_rel), graph_dir.path(&new_rel)],
            );
            let rename = graph_text_observation(&graph, &graph_dir.root, Some(&rename_event));
            assert!(!rename.uncertain);
            assert_eq!(
                rename.exact_paths,
                vec![graph_dir.path(&old_rel), graph_dir.path(&new_rel)]
            );
            observe_graph_text_event(&graph, &graph_dir.root, Some(&rename_event));
            assert_new_page_waits_for_reconciliation(&graph, &format!("New {extension}"));
            let rename_epoch = graph.graph_text_external_observation_ticket();
            graph
                .sync_deleted_file(&graph_dir.path(&old_rel))
                .expect("debounced rename source reconciliation");
            graph
                .sync_file_checked(&graph_dir.path(&new_rel))
                .expect("debounced rename destination reconciliation");
            graph.acknowledge_graph_text_external_observations(rename_epoch);
            assert_new_page_refused(&graph, &format!("New {extension}"));
        }
    }

    /// A callback arriving after batch A was drained belongs to batch B. Batch
    /// A must acknowledge only its captured frontier, never the graph's newer
    /// global epoch, or creation could race B before B is reconciled.
    #[test]
    fn drained_batch_cannot_acknowledge_a_callback_in_the_next_batch() {
        use notify::event::{CreateKind, EventKind};

        let graph_dir = TempGraph::new("watcher-drain-frontier-race");
        graph_dir.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&graph_dir.root);
        warm_direct_graph(&graph);
        let mut pending = Pending::default();

        graph_dir.write("pages/External A.md", "- external A\n");
        let path_a = graph_dir.path("pages/External A.md");
        assert!(observe_graph_text_event(
            &graph,
            &graph_dir.root,
            Some(&event(
                EventKind::Create(CreateKind::File),
                vec![path_a.clone()],
            )),
        ));
        pending.add_legacy_observations(vec![(
            graph_dir.root.clone(),
            graph.graph_text_external_observation_ticket(),
        )]);
        let batch_a = pending.take_legacy_observation_epochs();

        graph_dir.write("pages/External B.md", "- external B\n");
        let path_b = graph_dir.path("pages/External B.md");
        assert!(observe_graph_text_event(
            &graph,
            &graph_dir.root,
            Some(&event(
                EventKind::Create(CreateKind::File),
                vec![path_b.clone()],
            )),
        ));
        pending.add_legacy_observations(vec![(
            graph_dir.root.clone(),
            graph.graph_text_external_observation_ticket(),
        )]);

        graph.sync_file_checked(&path_a).unwrap();
        graph.acknowledge_graph_text_external_observations(batch_a[&graph_dir.root]);
        assert_new_page_waits_for_reconciliation(&graph, "Still Blocked By B");

        let batch_b = pending.take_legacy_observation_epochs();
        graph.sync_file_checked(&path_b).unwrap();
        graph.acknowledge_graph_text_external_observations(batch_b[&graph_dir.root]);
        graph.save_page(&new_page("Now Reconciled"), None).unwrap();
    }

    #[test]
    fn legacy_uncertain_graph_root_events_block_creation_until_reconciliation() {
        use notify::event::{CreateKind, EventKind, ModifyKind, RenameMode};
        use notify::event::{EventAttributes, Flag};

        for case in [
            "config",
            "root-create",
            "directory-rename",
            "rescan",
            "notify-error",
        ] {
            let graph_dir = TempGraph::new(&format!("uncertain-{case}"));
            graph_dir.write("pages/Anchor.md", "- anchor\n");
            let graph = Graph::open(&graph_dir.root);
            warm_direct_graph(&graph);
            graph_dir.write(
                "nonstandard/deep/Physical.md",
                &format!("title:: Epoch {case}\n\n- external\n"),
            );

            let event = match case {
                "config" => {
                    graph_dir.write("logseq/config.edn", "{}\n");
                    Some(event(
                        EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
                        vec![graph_dir.path("logseq/config.edn")],
                    ))
                }
                "root-create" => Some(event(
                    EventKind::Create(CreateKind::Folder),
                    vec![graph_dir.root.clone()],
                )),
                "directory-rename" => {
                    std::fs::create_dir_all(graph_dir.path("nonstandard/from.md")).unwrap();
                    graph_dir.rename("nonstandard/from.md", "nonstandard/to.md");
                    Some(event(
                        EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                        vec![
                            graph_dir.path("nonstandard/from.md"),
                            graph_dir.path("nonstandard/to.md"),
                        ],
                    ))
                }
                "rescan" => {
                    let mut attrs = EventAttributes::new();
                    attrs.set_flag(Flag::Rescan);
                    Some(notify::Event {
                        kind: EventKind::Other,
                        paths: Vec::new(),
                        attrs,
                    })
                }
                "notify-error" => None,
                _ => unreachable!(),
            };
            let observation = graph_text_observation(&graph, &graph_dir.root, event.as_ref());
            assert!(observation.relevant, "{case}");
            assert!(observation.uncertain, "{case}");
            assert!(observe_graph_text_event(
                &graph,
                &graph_dir.root,
                event.as_ref(),
            ));
            assert_new_page_waits_for_reconciliation(&graph, &format!("Epoch {case}"));
        }
    }

    /// Windows ReadDirectoryChangesW reports no sub-kind, so notify emits
    /// `Create(Any)` / `Modify(Any)` / `Remove(Any)` for everything except a
    /// rename. Those used to fall to the catch-all and mark the observation
    /// uncertain, which invalidates the whole guarded identity index -- meaning
    /// every external file event on Windows made the next save rebuild the
    /// entire graph. With an active sync client that is every save.
    ///
    /// Create/Modify are resolvable (the path still exists) and must now take
    /// the exact-path arm. `Remove(Any)` must stay uncertain: the entry is gone,
    /// so we cannot prove it was a file rather than a directory of pages.
    #[test]
    fn ambiguous_windows_event_kinds_take_the_exact_path_arm_except_removal() {
        use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind};

        let graph_dir = TempGraph::new("windows-any-kinds");
        graph_dir.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&graph_dir.root);
        warm_direct_graph(&graph);

        graph_dir.write("pages/External.md", "- external\n");
        let path = graph_dir.path("pages/External.md");

        for kind in [
            EventKind::Create(CreateKind::Any),
            EventKind::Modify(ModifyKind::Any),
        ] {
            let observation = graph_text_observation(
                &graph,
                &graph_dir.root,
                Some(&event(kind, vec![path.clone()])),
            );
            assert!(observation.relevant, "{kind:?}");
            assert!(
                !observation.uncertain,
                "{kind:?} carries an exact path and must not poison the whole index"
            );
            assert_eq!(observation.exact_paths, vec![path.clone()], "{kind:?}");
        }

        // A directory event still cannot be treated as a page file, even when
        // the sub-kind is missing -- the arm discriminates against the live
        // filesystem, not against the event kind.
        std::fs::create_dir_all(graph_dir.path("pages/sub")).unwrap();
        let directory = graph_text_observation(
            &graph,
            &graph_dir.root,
            Some(&event(
                EventKind::Create(CreateKind::Any),
                vec![graph_dir.path("pages/sub")],
            )),
        );
        assert!(
            directory.uncertain,
            "an ambiguous event on a directory is not provably a page file"
        );

        // Removal stays conservative: the path is gone, so its kind is unknowable.
        std::fs::remove_file(&path).unwrap();
        let removed = graph_text_observation(
            &graph,
            &graph_dir.root,
            Some(&event(EventKind::Remove(RemoveKind::Any), vec![path])),
        );
        assert!(
            removed.uncertain,
            "Remove(Any) must stay uncertain -- a removed directory of pages must not be \
             mistaken for a non-page path"
        );
    }

    #[test]
    fn graph_root_observation_routes_only_to_the_owning_graph() {
        use notify::event::{CreateKind, EventKind};

        let graph_a_dir = TempGraph::new("root-owner-a");
        let graph_b_dir = TempGraph::new("root-owner-b");
        graph_a_dir.write("pages/Anchor.md", "- anchor A\n");
        graph_b_dir.write("pages/Anchor.md", "- anchor B\n");
        let graph_a = Graph::open(&graph_a_dir.root);
        let graph_b = Graph::open(&graph_b_dir.root);

        graph_b_dir.write(
            "nonstandard/deep/Stale.md",
            "title:: Must Stay Stale\n\n- external without a B callback\n",
        );
        graph_a_dir.write("nonstandard/deep/A.md", "- observed A\n");
        let event_a = event(
            EventKind::Create(CreateKind::File),
            vec![graph_a_dir.path("nonstandard/deep/A.md")],
        );
        assert!(observe_graph_text_event(
            &graph_a,
            &graph_a_dir.root,
            Some(&event_a),
        ));
        let graph_b_observation =
            graph_text_observation(&graph_b, &graph_b_dir.root, Some(&event_a));
        assert!(!graph_b_observation.relevant);
        assert!(!graph_b_observation.uncertain);
        assert!(graph_b_observation.exact_paths.is_empty());
        assert!(!observe_graph_text_event(
            &graph_b,
            &graph_b_dir.root,
            Some(&event_a),
        ));
    }

    #[test]
    fn excluded_private_text_and_exact_non_text_events_are_not_published() {
        use notify::event::{CreateKind, EventKind};

        let graph_dir = TempGraph::new("excluded-private");
        graph_dir.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&graph_dir.root);
        for (relative, claimed) in [
            (".tine-sync/private/Sync.md", "Excluded Sync"),
            ("assets/private/Asset.org", "Excluded Asset"),
            ("logseq/bak/recovery.md", "Excluded Recovery"),
            (".hidden/private.md", "Excluded Hidden"),
        ] {
            graph_dir.write(relative, &format!("title:: {claimed}\n\n- private\n"));
            let event = event(
                EventKind::Create(CreateKind::File),
                vec![graph_dir.path(relative)],
            );
            let observation = graph_text_observation(&graph, &graph_dir.root, Some(&event));
            assert!(observation.relevant, "{relative}");
            assert!(!observation.uncertain, "{relative}");
            assert!(observation.exact_paths.is_empty(), "{relative}");
            assert!(observe_graph_text_event(
                &graph,
                &graph_dir.root,
                Some(&event)
            ));
        }

        graph_dir.write(
            "nonstandard/deep/Stale.md",
            "title:: Exact Non Text Is Harmless\n\n- unobserved\n",
        );
        graph_dir.write("nonstandard/deep/image.png", "not graph text\n");
        let non_text = event(
            EventKind::Create(CreateKind::File),
            vec![graph_dir.path("nonstandard/deep/image.png")],
        );
        let observation = graph_text_observation(&graph, &graph_dir.root, Some(&non_text));
        assert!(!observation.uncertain);
        assert!(observation.exact_paths.is_empty());
        assert!(observe_graph_text_event(
            &graph,
            &graph_dir.root,
            Some(&non_text)
        ));

        for relative in [".tine-sync", "assets", "logseq/bak", ".hidden"] {
            let private_directory = event(
                EventKind::Create(CreateKind::Folder),
                vec![graph_dir.path(relative)],
            );
            let observation =
                graph_text_observation(&graph, &graph_dir.root, Some(&private_directory));
            assert!(!observation.uncertain, "{relative}");
            assert!(observation.exact_paths.is_empty(), "{relative}");
        }
    }

    fn warm_cache(graph: &Graph) {
        let _ = graph.search("__watcher_warm_cache__", 1);
    }

    fn sorted_changes(mut changes: Vec<GraphChange>) -> Vec<GraphChange> {
        fn kind_key(kind: PageKind) -> &'static str {
            match kind {
                PageKind::Journal => "journal",
                PageKind::Page => "page",
            }
        }
        changes.sort_by(|a, b| {
            (a.removed, kind_key(a.kind), a.name.as_str()).cmp(&(
                b.removed,
                kind_key(b.kind),
                b.name.as_str(),
            ))
        });
        changes
    }

    fn rel_paths(tg: &TempGraph, rels: &[&str]) -> HashSet<PathBuf> {
        rels.iter().map(|rel| tg.path(rel)).collect()
    }

    fn assert_incremental_matches_full(
        name: &str,
        setup: impl FnOnce(&TempGraph),
        mutate: impl FnOnce(&TempGraph) -> HashSet<PathBuf>,
    ) {
        let tg = TempGraph::new(name);
        setup(&tg);

        let inc_graph = Graph::open(&tg.root);
        let full_graph = Graph::open(&tg.root);
        warm_cache(&inc_graph);
        warm_cache(&full_graph);

        let mut inc_snap = collect_graph_text_files(&inc_graph).files;
        let mut full_snap = inc_snap.clone();

        let paths = mutate(&tg);

        let (inc_changes, inc_conflicts_dirty, inc_errors) =
            incremental_reconcile(&inc_graph, &mut inc_snap, &paths);
        let fresh = collect_graph_text_files(&inc_graph).files;
        let (full_changes, full_conflicts_dirty, full_errors) =
            full_diff_reconcile(&full_graph, &mut full_snap, fresh.clone());

        assert_eq!(inc_snap, fresh, "incremental snap must match full scan");
        assert_eq!(full_snap, fresh, "full snap must match fresh scan");
        assert_eq!(inc_conflicts_dirty, full_conflicts_dirty);
        assert!(inc_errors.is_empty());
        assert!(full_errors.is_empty());
        assert_eq!(
            sorted_changes(inc_changes),
            sorted_changes(full_changes),
            "incremental changes must match full-diff changes"
        );
    }

    fn snapshot_relative_names(tg: &TempGraph, graph: &Graph) -> Vec<String> {
        let mut names: Vec<String> = collect_graph_text_files(graph)
            .files
            .keys()
            .map(|path| {
                path.strip_prefix(&tg.root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn snapshot_covers_exactly_what_discovery_covers() {
        // #21: page files in sub-folders must be in the snapshot, so an edit
        // there is reconciled rather than invisible until a graph reopen.
        // GH #268: so must a page at the graph ROOT or in a custom folder --
        // `graph_text_inventory` walks graph-wide through `GraphTextScope`, and
        // a snapshot narrower than discovery makes those pages permanently
        // unreconcilable. Excluded trees stay excluded, by the same authority.
        let tg = TempGraph::new("snapshot-scope");
        tg.write("top.md", "- t\n");
        tg.write("pages/Page.md", "- p\n");
        tg.write("journals/2026_08_06.md", "- j\n");
        tg.write("Archive/mid.org", "* m\n");
        tg.write("Archive/Deep/Deeper/deep.md", "- d\n");
        tg.write("Archive/notes.txt", "ignored\n");
        tg.write(".hidden/skip.md", "- s\n");
        tg.write("assets/embedded.md", "- a\n");
        tg.write("logseq/bak/old.md", "- b\n");

        assert_eq!(
            snapshot_relative_names(&tg, &Graph::open(&tg.root)),
            vec![
                "Archive/Deep/Deeper/deep.md",
                "Archive/mid.org",
                "journals/2026_08_06.md",
                "pages/Page.md",
                "top.md",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_does_not_follow_page_symlinks() {
        use std::os::unix::fs::symlink;

        let tg = TempGraph::new("snapshot-symlink");
        let outside =
            std::env::temp_dir().join(format!("tine-watch-outside-{}.md", std::process::id()));
        let _ = std::fs::remove_file(&outside);
        std::fs::write(&outside, "- outside\n").unwrap();
        symlink(&outside, tg.path("pages/secret.md")).unwrap();

        assert!(snapshot_relative_names(&tg, &Graph::open(&tg.root)).is_empty());

        std::fs::remove_file(&outside).ok();
    }

    #[test]
    fn graph_wide_external_paths_are_routed_like_discovery_routes_them() {
        // GH #268, the event-routing half. The watch is installed recursively on
        // the graph ROOT, so these events all arrive; the reconcile lane used to
        // filter them against `journals/` + `pages/` and silently drop the rest.
        let tg = TempGraph::new("event-scope");
        tg.write("top.md", "- t\n");
        tg.write("Archive/mid.md", "- m\n");
        tg.write("pages/Page.md", "- p\n");
        tg.write("assets/embedded.md", "- a\n");
        tg.write(".hidden/skip.md", "- s\n");
        let graph = Graph::open(&tg.root);

        use notify::event::{DataChange, EventKind, ModifyKind};
        let mut pending = Pending::default();
        for relative in [
            "top.md",
            "Archive/mid.md",
            "pages/Page.md",
            "assets/embedded.md",
            ".hidden/skip.md",
        ] {
            pending.add_event(event(
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                vec![tg.path(relative)],
            ));
        }

        let mut routed: Vec<String> = pending_for_graph(&pending.paths, &graph)
            .iter()
            .map(|path| {
                path.strip_prefix(&tg.root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        routed.sort();
        assert_eq!(routed, vec!["Archive/mid.md", "pages/Page.md", "top.md"]);
    }

    #[test]
    fn unclassified_paths_force_a_full_scan_only_where_pages_could_live() {
        // A directory move reports a path whose nature we cannot know, so it
        // forces a full diff. Excluded trees must not: dropping an image into
        // `assets/` cannot change the text inventory, and rescanning the graph
        // for it is the amplification this program exists to remove.
        let tg = TempGraph::new("full-scan-owner");
        let graph = Graph::open(&tg.root);
        let owned = full_scan_owner_for_graph(
            &HashSet::from([
                tg.path("pages/Moved"),
                tg.path("Archive"),
                tg.path("assets"),
                tg.path("assets/pictures"),
                tg.path(".git/objects"),
                PathBuf::from("/somewhere/else/pages"),
            ]),
            &graph,
        );
        let mut owned: Vec<String> = owned
            .iter()
            .map(|path| {
                path.strip_prefix(&tg.root)
                    .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| path.to_string_lossy().into_owned())
            })
            .collect();
        owned.sort();
        assert_eq!(owned, vec!["Archive", "pages/Moved"]);
    }

    #[test]
    fn incremental_create_top_level_file_matches_full_diff() {
        assert_incremental_matches_full(
            "create-top",
            |tg| tg.write("pages/Seed.md", "- seed\n"),
            |tg| {
                tg.write("pages/New.md", "- new\n");
                rel_paths(tg, &["pages/New.md"])
            },
        );
    }

    #[test]
    fn incremental_create_is_identified_as_inventory_change() {
        let tg = TempGraph::new("create-inventory");
        tg.write("pages/Seed.md", "- seed\n");
        let graph = Graph::open(&tg.root);
        warm_cache(&graph);
        let mut snap = collect_graph_text_files(&graph).files;
        let path = tg.path("pages/New.md");
        tg.write("pages/New.md", "- new\n");

        let (changes, conflicts_dirty, errors) =
            incremental_reconcile(&graph, &mut snap, &HashSet::from([path]));

        assert!(!conflicts_dirty);
        assert!(errors.is_empty());
        assert_eq!(changes.len(), 1);
        assert!(changes[0].created);
        assert!(!changes[0].removed);
    }

    #[test]
    fn incremental_create_nested_file_matches_full_diff() {
        assert_incremental_matches_full(
            "create-nested",
            |tg| tg.write("pages/Seed.md", "- seed\n"),
            |tg| {
                tg.write("pages/sub/New.md", "- nested\n");
                rel_paths(tg, &["pages/sub/New.md"])
            },
        );
    }

    #[test]
    fn incremental_modify_len_change_matches_full_diff() {
        assert_incremental_matches_full(
            "modify-len",
            |tg| tg.write("pages/Edit.md", "- one\n"),
            |tg| {
                std::thread::sleep(Duration::from_millis(20));
                tg.write("pages/Edit.md", "- one\n- two\n");
                rel_paths(tg, &["pages/Edit.md"])
            },
        );
    }

    #[test]
    fn incremental_modify_same_len_mtime_change_matches_full_diff() {
        assert_incremental_matches_full(
            "modify-same-len",
            |tg| tg.write("pages/Edit.md", "- alpha\n"),
            |tg| {
                std::thread::sleep(Duration::from_millis(20));
                tg.write("pages/Edit.md", "- beta!\n");
                rel_paths(tg, &["pages/Edit.md"])
            },
        );
    }

    #[test]
    fn explicit_event_reconciles_even_when_snapshot_metadata_is_equal() {
        let tg = TempGraph::new("explicit-same-metadata");
        tg.write("pages/Edit.md", "- alpha\n");
        let graph = Graph::open(&tg.root);
        warm_cache(&graph);
        let path = tg.path("pages/Edit.md");
        tg.write("pages/Edit.md", "- bravo\n"); // equal byte length
        let stamp = file_snapshot(&path).unwrap();
        // Simulate a sync tool preserving every snapshot field: the explicit
        // notify path must still reach Graph::sync_file's content comparison.
        let mut snap = HashMap::from([(path.clone(), stamp)]);
        let (changes, conflicts_dirty, errors) =
            incremental_reconcile(&graph, &mut snap, &HashSet::from([path]));
        assert!(!conflicts_dirty);
        assert!(errors.is_empty());
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "Edit");
        assert_eq!(changes[0].kind, PageKind::Page);
        assert!(!changes[0].created);
        assert!(!changes[0].removed);
    }

    #[test]
    fn incremental_remove_top_level_file_matches_full_diff() {
        assert_incremental_matches_full(
            "remove-top",
            |tg| {
                tg.write("pages/Keep.md", "- keep\n");
                tg.write("pages/Delete.md", "- delete\n");
            },
            |tg| {
                tg.remove("pages/Delete.md");
                rel_paths(tg, &["pages/Delete.md"])
            },
        );
    }

    #[test]
    fn incremental_remove_nested_file_matches_full_diff() {
        assert_incremental_matches_full(
            "remove-nested",
            |tg| {
                tg.write("pages/Keep.md", "- keep\n");
                tg.write("pages/sub/Delete.md", "- delete\n");
            },
            |tg| {
                tg.remove("pages/sub/Delete.md");
                rel_paths(tg, &["pages/sub/Delete.md"])
            },
        );
    }

    #[test]
    fn incremental_rename_within_pages_matches_full_diff() {
        assert_incremental_matches_full(
            "rename-within-pages",
            |tg| tg.write("pages/Old.md", "- renamed\n"),
            |tg| {
                tg.rename("pages/Old.md", "pages/New.md");
                rel_paths(tg, &["pages/Old.md", "pages/New.md"])
            },
        );
    }

    #[test]
    fn incremental_rename_across_tree_matches_full_diff() {
        assert_incremental_matches_full(
            "rename-across-tree",
            |tg| tg.write("pages/JournalMove.md", "- moved\n"),
            |tg| {
                tg.rename("pages/JournalMove.md", "journals/2026_07_10.md");
                rel_paths(tg, &["pages/JournalMove.md", "journals/2026_07_10.md"])
            },
        );
    }

    #[test]
    fn incremental_burst_union_matches_full_diff() {
        assert_incremental_matches_full(
            "burst-union",
            |tg| {
                tg.write("pages/Edit.md", "- edit before\n");
                tg.write("pages/Delete.md", "- delete\n");
                tg.write("pages/Keep.md", "- keep\n");
            },
            |tg| {
                std::thread::sleep(Duration::from_millis(20));
                tg.write("pages/Edit.md", "- edit after\n");
                tg.remove("pages/Delete.md");
                tg.write("pages/Create.md", "- create\n");
                tg.write("pages/sub/Nested.md", "- nested\n");
                rel_paths(
                    tg,
                    &[
                        "pages/Edit.md",
                        "pages/Delete.md",
                        "pages/Create.md",
                        "pages/sub/Nested.md",
                    ],
                )
            },
        );
    }

    #[test]
    fn bulk_threshold_boundary_is_exclusive_on_both_sides() {
        assert!(!burst_escalates(0));
        assert!(!burst_escalates(BULK_CHANGE_THRESHOLD));
        assert!(burst_escalates(BULK_CHANGE_THRESHOLD + 1));
        assert!(!emit_as_bulk(BULK_CHANGE_THRESHOLD));
        assert!(emit_as_bulk(BULK_CHANGE_THRESHOLD + 1));
    }

    /// `graph-changed-bulk` is a frontend wire contract: the aggregate carries
    /// the same per-page change shape the `graph-changed` event carries.
    #[test]
    fn graph_changed_bulk_wire_shape_is_stable() {
        let wire = serde_json::to_value(GraphChangedBulk {
            changes: vec![GraphChange {
                name: "Page".to_owned(),
                kind: PageKind::Page,
                created: true,
                removed: false,
            }],
        })
        .unwrap();
        let changes = wire.get("changes").and_then(|value| value.as_array());
        let first = changes.and_then(|list| list.first()).expect("one change");
        for key in ["name", "kind", "created", "removed"] {
            assert!(first.get(key).is_some(), "missing bulk change field {key}");
        }
    }

    #[test]
    fn a_drained_batch_above_the_threshold_escalates_to_the_full_branch() {
        // Concord P2 (GH #337 / spec L6): a VCS checkout or first big sync dumps
        // N file events; processing them per-file costs two reads + a parse per
        // path with deliberately no stat shortcut. Above the threshold the batch
        // must take the stat-diff full branch instead.
        let tg = TempGraph::new("burst-escalation");
        tg.write("pages/Seed.md", "- seed\n");
        let graph = Graph::open(&tg.root);
        warm_cache(&graph);
        let mut snap = collect_graph_text_files(&graph).files;

        let mut paths = HashSet::new();
        for index in 0..(BULK_CHANGE_THRESHOLD + 1) {
            let rel = format!("pages/Bulk {index}.md");
            tg.write(&rel, &format!("- bulk {index}\n"));
            paths.insert(tg.path(&rel));
        }

        let (changes, _, used_full, errors) =
            reconcile_pending(&graph, &mut snap, &paths, false, false);
        assert!(
            used_full,
            "a batch of {} paths (> {BULK_CHANGE_THRESHOLD}) must escalate to the full stat-diff branch",
            paths.len()
        );
        assert!(errors.is_empty());
        assert_eq!(changes.len(), BULK_CHANGE_THRESHOLD + 1);
    }

    #[test]
    fn a_drained_batch_at_the_threshold_stays_incremental() {
        // The complement: ordinary bursts (a save, a small sync delta) keep the
        // per-file branch, whose explicit-event semantics deliberately bypass
        // the stat shortcut (see `explicit_event_reconciles_even_when_snapshot_
        // metadata_is_equal`).
        let tg = TempGraph::new("burst-no-escalation");
        tg.write("pages/Seed.md", "- seed\n");
        let graph = Graph::open(&tg.root);
        warm_cache(&graph);
        let mut snap = collect_graph_text_files(&graph).files;

        let mut paths = HashSet::new();
        for index in 0..BULK_CHANGE_THRESHOLD {
            let rel = format!("pages/Bulk {index}.md");
            tg.write(&rel, &format!("- bulk {index}\n"));
            paths.insert(tg.path(&rel));
        }

        let (changes, _, used_full, errors) =
            reconcile_pending(&graph, &mut snap, &paths, false, false);
        assert!(
            !used_full,
            "a batch of exactly {BULK_CHANGE_THRESHOLD} paths must keep the incremental branch"
        );
        assert!(errors.is_empty());
        assert_eq!(changes.len(), BULK_CHANGE_THRESHOLD);
    }

    /// The correctness invariant behind the escalation: whichever branch a burst
    /// takes, the result is the same. Same family as
    /// `incremental_burst_union_matches_full_diff`, sized across the threshold.
    #[test]
    fn incremental_burst_above_threshold_union_matches_full_diff() {
        assert_incremental_matches_full(
            "burst-union-above-threshold",
            |tg| {
                for index in 0..BULK_CHANGE_THRESHOLD {
                    tg.write(&format!("pages/Edit {index}.md"), "- before\n");
                }
                tg.write("pages/Delete.md", "- delete\n");
                tg.write("pages/Keep.md", "- keep\n");
            },
            |tg| {
                std::thread::sleep(Duration::from_millis(20));
                let mut rels: Vec<String> = Vec::new();
                for index in 0..BULK_CHANGE_THRESHOLD {
                    let rel = format!("pages/Edit {index}.md");
                    tg.write(&rel, "- after\n");
                    rels.push(rel);
                }
                tg.remove("pages/Delete.md");
                rels.push("pages/Delete.md".to_owned());
                for index in 0..4 {
                    let rel = format!("pages/sub/Created {index}.md");
                    tg.write(&rel, "- created\n");
                    rels.push(rel);
                }
                rels.iter().map(|rel| tg.path(rel)).collect()
            },
        );
    }

    /// Bulk-change measurement + generous regression gate (Concord P2).
    ///
    /// Ignored: the fixture is a few hundred generated files — deliberately NOT
    /// part of the fast unit corpus. Run explicitly:
    ///   cargo nextest run -p tine --run-ignored ignored-only -E 'test(bulk_reconcile)'
    ///
    /// Measures a checkout-shaped change (many files replaced at once under a
    /// running watcher) through both reconcile branches, prints the numbers, and
    /// asserts only an order-of-magnitude ceiling — never a tight timing bound.
    #[test]
    #[ignore = "bulk fixture (hundreds of generated files); run explicitly"]
    fn bulk_reconcile_bench_and_gate() {
        const TOTAL: usize = 800;
        const CHANGED: usize = 400;

        let tg = TempGraph::new("bulk-bench");
        for index in 0..TOTAL {
            tg.write(
                &format!("pages/Bulk {index}.md"),
                &format!("- bulk page {index}\n- second line {index}\n"),
            );
        }
        let inc_graph = Graph::open(&tg.root);
        let full_graph = Graph::open(&tg.root);
        warm_cache(&inc_graph);
        warm_cache(&full_graph);
        let mut inc_snap = collect_graph_text_files(&inc_graph).files;
        let mut full_snap = inc_snap.clone();

        // The external revision: a checkout replaces CHANGED files' contents.
        std::thread::sleep(Duration::from_millis(20));
        let mut paths = HashSet::new();
        for index in 0..CHANGED {
            let rel = format!("pages/Bulk {index}.md");
            tg.write(&rel, &format!("- bulk page {index} switched\n"));
            paths.insert(tg.path(&rel));
        }

        let incremental_started = Instant::now();
        let (inc_changes, _, inc_errors) = incremental_reconcile(&inc_graph, &mut inc_snap, &paths);
        let incremental_elapsed = incremental_started.elapsed();

        let full_started = Instant::now();
        let snapshot = collect_graph_text_files(&full_graph);
        let (full_changes, _, full_errors) =
            full_diff_reconcile(&full_graph, &mut full_snap, snapshot.files);
        let full_elapsed = full_started.elapsed();

        assert!(inc_errors.is_empty());
        assert!(full_errors.is_empty());
        assert_eq!(inc_changes.len(), CHANGED);
        assert_eq!(full_changes.len(), CHANGED);
        println!(
            "bulk-reconcile bench: {CHANGED} changed of {TOTAL} files — \
             incremental branch {}ms, full stat-diff branch {}ms",
            incremental_elapsed.as_millis(),
            full_elapsed.as_millis(),
        );

        // Generous gate: the escalated (full) branch reconciling a 400-file
        // change over an 800-file graph measured 95 ms on the 2026-08 dev box
        // (incremental branch: 94 ms — the branches cost the same for genuinely
        // changed files; escalation buys one consistent snapshot and one
        // aggregate emit, not reconcile speed). 10 s ≈ 100× measured: it catches
        // an order-of-magnitude regression (e.g. an accidental whole-graph
        // reparse per changed file) without ever flaking under load.
        let ceiling = Duration::from_secs(10);
        assert!(
            full_elapsed < ceiling,
            "escalated bulk reconcile took {}ms (ceiling {}ms)",
            full_elapsed.as_millis(),
            ceiling.as_millis(),
        );
    }

    #[test]
    fn reconcile_pending_need_full_uses_full_scan_branch() {
        let tg = TempGraph::new("need-full");
        tg.write("pages/Seed.md", "- seed\n");

        let inc_graph = Graph::open(&tg.root);
        let full_graph = Graph::open(&tg.root);
        warm_cache(&inc_graph);
        warm_cache(&full_graph);

        let mut inc_snap = collect_graph_text_files(&inc_graph).files;
        let mut full_snap = inc_snap.clone();

        tg.write(
            "pages/sub/CreatedByDirEvent.md",
            "- created through dir op\n",
        );
        let incomplete_paths = rel_paths(&tg, &["pages/Seed.md"]);
        let (inc_changes, inc_conflicts_dirty, used_full, inc_errors) =
            reconcile_pending(&inc_graph, &mut inc_snap, &incomplete_paths, true, false);
        let fresh = collect_graph_text_files(&inc_graph).files;
        let (full_changes, full_conflicts_dirty, full_errors) =
            full_diff_reconcile(&full_graph, &mut full_snap, fresh.clone());

        assert!(used_full, "need_full must bypass incremental reconcile");
        assert!(inc_errors.is_empty());
        assert!(full_errors.is_empty());
        assert_eq!(inc_snap, fresh);
        assert_eq!(full_snap, fresh);
        assert_eq!(inc_conflicts_dirty, full_conflicts_dirty);
        assert_eq!(sorted_changes(inc_changes), sorted_changes(full_changes));
    }

    /// DUP-5: every temp shape a Tine writer can rename INTO the live graph
    /// must be recognized here, or the rename event that publishes the real
    /// page is dropped. The restore shape was invisible until 2026-08-25.
    #[test]
    fn recognizes_every_tine_temp_shape_that_lands_in_the_live_graph() {
        for recognized in [
            ".Foo.md.1234.7.tmp",
            ".Foo.md.1234.7.new.tmp",
            ".Foo.md.1234.7.projection.tmp",
            ".tine-restore-1234-7.tmp",
        ] {
            assert!(
                is_tine_atomic_page_temp_path(Path::new(recognized)),
                "{recognized} must be recognized as a Tine atomic temp"
            );
        }
        for foreign in [
            ".tine-restore-x.tmp",
            ".tine-restore-12.tmp",
            "tine-restore-1234-7.tmp",
            ".Foo.md.restore.tmp",
            "Foo.md",
        ] {
            assert!(
                !is_tine_atomic_page_temp_path(Path::new(foreign)),
                "{foreign} must NOT read as a Tine atomic temp"
            );
        }
    }
}
