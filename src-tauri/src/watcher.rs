use crate::settings::{settings_path, update_settings};
use crate::state::{AppState, GraphSlot, LegacyGraphLease};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tauri::{Emitter, Manager, State};
use tine_core::sync_runtime::{SyncRuntimeHandle, SyncRuntimeTick, SyncWatcherObservation};
use tine_core::{model::GraphTextExactFeedPathClass, model::PageKind, Graph};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct GraphChange {
    name: String,
    kind: PageKind,
    created: bool,
    removed: bool,
}

/// Every sparse-runtime watcher event is scoped to the graph binding that
/// produced it. A window can be rebound to another graph while a watcher cycle
/// is in flight; the frontend must be able to drop that older cycle instead of
/// showing its status or failure for the new graph.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct SparseV2RuntimeStatusEvent {
    binding_generation: u64,
    runtime: crate::sync_runtime::SparseV2RuntimeStatusDto,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct SparseV2TickEvent {
    binding_generation: u64,
    tick: crate::sync_runtime::SparseV2TickDto,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct SparseV2ErrorEvent {
    binding_generation: u64,
    message: String,
}

#[derive(Default)]
struct Pending {
    paths: HashSet<PathBuf>,
    full_paths: HashSet<PathBuf>,
    need_full: bool,
    notify_error: bool,
}

/// Resolve the filesystem watcher inputs for an existing legacy binding.
/// Sparse-v2 bindings must later use their actor; they never fall back to a
/// second legacy `Graph` or direct file watcher here.
fn legacy_watch_paths(
    slot: &GraphSlot,
) -> Result<(LegacyGraphLease, PathBuf, [PathBuf; 2], PathBuf), String> {
    let graph = slot.legacy_graph_cloned()?;
    let root = slot.root_key.clone();
    let dirs = [graph.journals_path(), graph.pages_path()];
    let sync_dir = graph.managed_sync_store_path();
    Ok((graph, root, dirs, sync_dir))
}

impl Pending {
    fn add_event(&mut self, event: notify::Event) {
        // Managed-sync chunks and receipts use their own reconcile lane. A pull
        // scans the immutable store, so even a backend rescan notification only
        // needs to retain ownership of the concrete sync path.
        let managed_sync_event = !event.paths.is_empty()
            && event.paths.iter().all(|path| {
                path.components()
                    .any(|component| component.as_os_str() == ".tine-sync")
            });
        if managed_sync_event {
            self.paths.extend(event.paths);
            return;
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
        self.need_full = true;
        self.notify_error = true;
    }
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

    fn progressed(&mut self, now: Instant) {
        self.failures = 0;
        self.due = Some(now + Duration::from_millis(10));
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

fn incremental_page_paths(event: &notify::Event) -> Option<Vec<PathBuf>> {
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};

    let explicit_file_event = matches!(
        event.kind,
        EventKind::Create(CreateKind::File)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Metadata(_))
            | EventKind::Remove(RemoveKind::File)
    );
    let supported = explicit_file_event
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
    if !all_text_or_temp && !explicit_file_event {
        // A rename without a file-kind witness may denote a directory subtree.
        return None;
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

fn collect_graph_text_files(graph: &Graph) -> GraphTextSnapshot {
    let mut files: HashMap<PathBuf, FileStamp> = HashMap::new();
    let mut complete = true;
    let mut stack = vec![graph.root.clone()];
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
                if graph.graph_text_watch_descend(&path) {
                    stack.push(path);
                }
                continue;
            }
            if !file_type.is_file() || !graph.graph_text_watch_relevant(&path) {
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
    let changed = changed_since_snapshot(snap, &snapshot.files);
    let _ = graph.observe_graph_text_external_paths(
        changed.iter().map(PathBuf::as_path),
        !snapshot.complete,
    );
}

fn reconcile_pending(
    graph: &Graph,
    snap: &mut HashMap<PathBuf, FileStamp>,
    paths: &HashSet<PathBuf>,
    need_full: bool,
    poll_mode: bool,
) -> (Vec<GraphChange>, bool, bool, Vec<String>) {
    if need_full || paths.is_empty() {
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

#[derive(Default)]
struct LegacyGraphTextObservation {
    exact_paths: Vec<PathBuf>,
    uncertain: bool,
    relevant: bool,
}

fn relative_graph_text_event_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    relative
        .to_str()
        .map(|relative| relative.replace(std::path::MAIN_SEPARATOR, "/"))
}

fn legacy_graph_text_observation(
    graph: &Graph,
    root: &Path,
    event: Option<&notify::Event>,
) -> LegacyGraphTextObservation {
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind};

    let Some(event) = event else {
        return LegacyGraphTextObservation {
            uncertain: true,
            relevant: true,
            ..LegacyGraphTextObservation::default()
        };
    };
    if event.paths.is_empty() {
        return LegacyGraphTextObservation {
            uncertain: true,
            relevant: true,
            ..LegacyGraphTextObservation::default()
        };
    }

    let owned = event
        .paths
        .iter()
        .filter(|path| path.starts_with(root))
        .collect::<Vec<_>>();
    if owned.is_empty() {
        return LegacyGraphTextObservation::default();
    }

    let mut observation = LegacyGraphTextObservation {
        relevant: true,
        uncertain: event.need_rescan(),
        ..LegacyGraphTextObservation::default()
    };
    if observation.uncertain {
        return observation;
    }

    // Windows ReadDirectoryChangesW cannot report a sub-kind: notify maps every
    // non-rename Windows event to `Create(Any)` / `Modify(Any)` / `Remove(Any)`.
    // Those fell to the catch-all below and marked the observation uncertain,
    // which invalidates the ENTIRE guarded graph-text identity index -- so on
    // Windows any external file event made the next save rebuild the whole
    // graph. The event does carry exact paths; only the sub-kind is missing.
    //
    // Create/Modify are recoverable because the path still exists, so the arm
    // below discriminates file from directory against the live filesystem
    // exactly as it does for an explicit kind.
    //
    // `Remove(Any)` is NOT recoverable and deliberately stays uncertain: once
    // the entry is gone we cannot prove it was a file rather than a directory
    // of pages, and treating a removed directory as "not a page file" would
    // silently drop every page under it.
    let ambiguous_but_resolvable = matches!(
        event.kind,
        EventKind::Create(CreateKind::Any) | EventKind::Modify(ModifyKind::Any)
    );
    let explicit_file_event = ambiguous_but_resolvable
        || matches!(
            event.kind,
            EventKind::Create(CreateKind::File)
                | EventKind::Modify(ModifyKind::Data(_))
                | EventKind::Modify(ModifyKind::Metadata(_))
                | EventKind::Remove(RemoveKind::File)
        );
    let rename_event = matches!(event.kind, EventKind::Modify(ModifyKind::Name(_)));
    let rename_has_file_witness = rename_event
        && event
            .paths
            .iter()
            .any(|path| std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file()))
        && !event
            .paths
            .iter()
            .any(|path| std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()));

    for path in owned {
        let Some(relative) = relative_graph_text_event_path(root, path) else {
            observation.uncertain = true;
            break;
        };
        let class = match graph.classify_graph_text_exact_feed_path(&relative) {
            Ok(class) => class,
            Err(_) => {
                observation.uncertain = true;
                break;
            }
        };
        match class {
            GraphTextExactFeedPathClass::Excluded => continue,
            GraphTextExactFeedPathClass::Configuration => {
                observation.uncertain = true;
                break;
            }
            GraphTextExactFeedPathClass::RetainedFile => {}
            _ => {
                observation.uncertain = true;
                break;
            }
        }
        let descendants_excluded = graph
            .classify_graph_text_exact_feed_path(&format!(
                "{relative}/__tine_watcher_descendant__.md"
            ))
            .is_ok_and(|class| class == GraphTextExactFeedPathClass::Excluded);

        if explicit_file_event {
            if path_is_existing_dir(path) {
                if descendants_excluded {
                    continue;
                }
                observation.uncertain = true;
                break;
            }
            if is_page_file_path(path) {
                observation.exact_paths.push(path.clone());
            }
        } else if rename_event {
            if !rename_has_file_witness {
                if descendants_excluded {
                    continue;
                }
                observation.uncertain = true;
                break;
            }
            if is_page_file_path(path) {
                observation.exact_paths.push(path.clone());
            }
        } else {
            if descendants_excluded {
                continue;
            }
            observation.uncertain = true;
            break;
        }
    }

    if observation.uncertain {
        observation.exact_paths.clear();
    }
    observation
}

fn observe_legacy_graph_text_event(
    graph: &Graph,
    root: &Path,
    event: Option<&notify::Event>,
) -> bool {
    let observation = legacy_graph_text_observation(graph, root, event);
    if !observation.relevant {
        return false;
    }
    if observation.uncertain || !observation.exact_paths.is_empty() {
        let _ = graph.observe_graph_text_external_paths(
            observation.exact_paths.iter().map(PathBuf::as_path),
            observation.uncertain,
        );
    }
    true
}

/// Linearize a platform callback with guarded graph-text writes before the
/// watcher's debounce/reconciliation delay. The callback does not mutate the
/// cache; it only advances the core-owned retained identity generation (or
/// marks it uncertain) under the same resource-scoped mutation authority that
/// `Graph::save_page` uses.
fn observe_legacy_graph_text_callback(app: &tauri::AppHandle, event: Option<&notify::Event>) {
    let state = app.state::<AppState>();
    let entries = match state.graphs.read() {
        Ok(graphs) => graphs.entries(),
        Err(_) => return,
    };
    for (_, slot) in entries {
        let Ok((graph, root, _, _)) = legacy_watch_paths(&slot) else {
            continue;
        };
        observe_legacy_graph_text_event(&graph, &root, event);
    }
}

fn sparse_observations(
    root: &Path,
    paths: &HashSet<PathBuf>,
    full_paths: &HashSet<PathBuf>,
    need_full: bool,
    notify_error: bool,
) -> Vec<SyncWatcherObservation> {
    let mut observations = Vec::new();
    let mut unknown = false;
    for path in paths.iter().chain(full_paths.iter()) {
        if !path.starts_with(root) {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        if relative
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == ".tine-sync")
        {
            continue;
        }
        let observation = relative
            .to_str()
            .map(|relative| relative.replace(std::path::MAIN_SEPARATOR, "/"))
            .and_then(|relative| SyncWatcherObservation::managed_path(relative).ok());
        match observation {
            Some(observation) => observations.push(observation),
            None => unknown = true,
        }
    }
    if unknown {
        observations.push(SyncWatcherObservation::UnknownPath);
    }
    if need_full {
        observations.push(SyncWatcherObservation::RescanRequired);
    }
    if notify_error {
        observations.push(SyncWatcherObservation::NotifyError);
    }
    observations
}

fn sparse_provider_observations(
    root: &Path,
    paths: &HashSet<PathBuf>,
    full_paths: &HashSet<PathBuf>,
) -> (Vec<String>, bool) {
    let provider = root.join(".tine-sync/v2/shared");
    let outbox = provider.join("outbox");
    let mut exact = Vec::new();
    let mut imprecise = full_paths.iter().any(|path| path.starts_with(&provider));
    for path in paths.iter().filter(|path| path.starts_with(&provider)) {
        let Ok(relative) = path.strip_prefix(&outbox) else {
            imprecise = true;
            continue;
        };
        let Some(relative) = relative.to_str() else {
            imprecise = true;
            continue;
        };
        let relative = relative.replace(std::path::MAIN_SEPARATOR, "/");
        let namespace = relative.split('/').next().unwrap_or_default();
        if matches!(namespace, ".part" | "removed" | "rename-evidence") {
            // These are transport-owned retry/retirement namespaces. Their
            // exact churn is not provider ingress and must not turn a local
            // commit-last rename into graph-wide reconciliation.
            continue;
        }
        if relative.is_empty()
            || !relative.contains('/')
            || relative.starts_with('/')
            || relative.contains('\\')
            || !matches!(
                namespace,
                "enrollment"
                    | "frontier-heads-v1"
                    | "publication-intents-v1"
                    | "manifest-recovery-links-v1"
                    | "manifest-recovery-blobs-v1"
                    | "manifests"
                    | "objects"
            )
            || relative
                .split('/')
                .any(|component| component.is_empty() || component == "." || component == "..")
        {
            imprecise = true;
        } else {
            exact.push(relative);
        }
    }
    exact.sort();
    exact.dedup();
    (exact, imprecise)
}

/// Watch the graph dirs for external changes (Logseq, Syncthing) and reconcile
/// them into the cache, emitting `graph-changed` so the UI can reload. Two
/// mechanisms, switchable at runtime via the device-local `watch_mode` setting:
///
///   - **"inotify" (default):** a real OS filesystem watcher (the `notify`
///     crate — inotify on Linux). Idle = *zero* periodic wakeups; the thread
///     blocks until the kernel reports a change. Matches OG Logseq (chokidar)
///     and is the right choice on a normal local disk.
///   - **"poll":** a 3-second mtime scan. Robust on filesystems where inotify is
///     unreliable (some NFS / network mounts), at the cost of constant periodic
///     wakeups. Use this only when inotify misses external edits.
///
/// In both modes the reconcile is identical and suppresses Tine's *own* writes
/// via the cache comparison inside `sync_file`. A control channel (poked by
/// `load_graph` on a graph switch and by `set_watch_mode`) lets the thread
/// re-target or switch mechanism at once, without polling for those either.
pub(crate) fn start_watcher(app: tauri::AppHandle) {
    use notify::Watcher;
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let pending = Arc::new(Mutex::new(Pending::default()));
    if let Ok(mut slot) = app.state::<AppState>().watch_ctl.lock() {
        *slot = Some(tx.clone());
    }
    std::thread::spawn(move || {
        struct WatchedGraph {
            legacy_graph: LegacyGraphLease,
            root: PathBuf,
            dirs: [PathBuf; 2],
            sync_dir: PathBuf,
            snap: HashMap<PathBuf, FileStamp>,
            baseline: bool,
            last_sync_error: Option<String>,
            retry: RetrySchedule,
        }

        struct WatchedSparse {
            handle: SyncRuntimeHandle,
            root: PathBuf,
            binding_generation: u64,
            last_error: Option<String>,
            retry: RetrySchedule,
            initial_tick: bool,
        }

        let mut graphs: HashMap<String, WatchedGraph> = HashMap::new();
        let mut sparse_graphs: HashMap<String, WatchedSparse> = HashMap::new();
        let mut watcher: Option<notify::RecommendedWatcher> = None;
        let mut watched: HashSet<PathBuf> = HashSet::new();
        // Last surfaced `watch()` failure per graph root, so a root that keeps
        // failing reports once instead of every cycle.
        let mut watch_failures: HashMap<PathBuf, String> = HashMap::new();
        let mut forced_sparse_tick = false;
        loop {
            let force_sparse_tick = std::mem::take(&mut forced_sparse_tick);
            let inotify = watch_mode(&app) != "poll";
            let entries = app.state::<AppState>().graphs.read().unwrap().entries();
            let live: HashSet<String> = entries.iter().map(|(label, _)| label.clone()).collect();
            graphs.retain(|label, _| live.contains(label));
            sparse_graphs.retain(|label, _| live.contains(label));
            for (label, slot) in entries {
                if let Some(handle) = slot.sparse_runtime().cloned() {
                    graphs.remove(&label);
                    match sparse_graphs.get_mut(&label) {
                        Some(current)
                            if current.root == slot.root_key
                                && current.binding_generation == slot.binding_generation =>
                        {
                            current.handle = handle;
                        }
                        _ => {
                            sparse_graphs.insert(
                                label,
                                WatchedSparse {
                                    handle,
                                    root: slot.root_key.clone(),
                                    binding_generation: slot.binding_generation,
                                    last_error: None,
                                    retry: RetrySchedule::default(),
                                    initial_tick: true,
                                },
                            );
                        }
                    }
                    continue;
                }
                sparse_graphs.remove(&label);
                let Ok((legacy_graph, root, dirs, sync_dir)) = legacy_watch_paths(&slot) else {
                    // Sparse-v2 owns its actor in the slot. This legacy watcher
                    // must not retain or reopen a Graph for it.
                    graphs.remove(&label);
                    continue;
                };
                match graphs.get_mut(&label) {
                    Some(current)
                        if current.root == root
                            && current.dirs == dirs
                            && current.sync_dir == sync_dir =>
                    {
                        current.legacy_graph = legacy_graph;
                    }
                    _ => {
                        graphs.insert(
                            label,
                            WatchedGraph {
                                legacy_graph,
                                root,
                                dirs,
                                sync_dir,
                                snap: HashMap::new(),
                                baseline: false,
                                last_sync_error: None,
                                retry: RetrySchedule::default(),
                            },
                        );
                    }
                }
            }

            let labels_by_root: Vec<(String, PathBuf)> = graphs
                .iter()
                .map(|(label, graph)| (label.clone(), graph.root.clone()))
                .chain(
                    sparse_graphs
                        .iter()
                        .map(|(label, graph)| (label.clone(), graph.root.clone())),
                )
                .collect();
            let desired: HashSet<PathBuf> = labels_by_root
                .iter()
                .map(|(_, root)| root.clone())
                .collect();

            // Bring the OS watcher in line with the current mode + graph roots.
            if inotify {
                if watcher.is_none() {
                    let txc = tx.clone();
                    let pendingc = pending.clone();
                    let appc = app.clone();
                    watcher =
                        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                            match &res {
                                Ok(event) => observe_legacy_graph_text_callback(&appc, Some(event)),
                                Err(_) => observe_legacy_graph_text_callback(&appc, None),
                            }
                            if let Ok(mut p) = pendingc.lock() {
                                match res {
                                    Ok(event) => p.add_event(event),
                                    Err(_) => p.add_notify_error(),
                                }
                            }
                            let _ = txc.send(());
                        })
                        .ok();
                    watched.clear();
                }
                if let Some(w) = watcher.as_mut() {
                    for dir in watched.difference(&desired).cloned().collect::<Vec<_>>() {
                        let _ = w.unwatch(&dir);
                        watched.remove(&dir);
                    }
                    for dir in desired.difference(&watched).cloned().collect::<Vec<_>>() {
                        // Recursive so the guarded-identity boundary includes
                        // eligible graph text outside configured cache roots.
                        match w.watch(&dir, notify::RecursiveMode::Recursive) {
                            Ok(()) => {
                                watch_failures.remove(&dir);
                                watched.insert(dir);
                            }
                            Err(error) => {
                                // A failure here is retried next cycle (the root
                                // is never inserted into `watched`), so this is
                                // not permanent -- but it was completely silent,
                                // and until it succeeds every external change to
                                // that graph is invisible. Surface it once per
                                // distinct message so a retry loop cannot spam.
                                let message = error.to_string();
                                if watch_failures.get(&dir) != Some(&message) {
                                    for (label, root) in labels_by_root.iter() {
                                        if root == &dir {
                                            let _ =
                                                app.emit_to(label, "graph-watch-error", &message);
                                        }
                                    }
                                    watch_failures.insert(dir, message);
                                }
                            }
                        }
                    }
                }
            } else if watcher.is_some() {
                watcher = None; // poll mode → release the OS watcher
                watched.clear();
                watch_failures.clear();
            }
            watch_failures.retain(|dir, _| desired.contains(dir));

            // --- reconcile (identical in both modes) ---
            let (paths, full_paths, event_need_full, notify_error) = if inotify {
                if let Ok(mut p) = pending.lock() {
                    let paths = std::mem::take(&mut p.paths);
                    let full_paths = std::mem::take(&mut p.full_paths);
                    let need_full = p.need_full;
                    let notify_error = p.notify_error;
                    p.need_full = false;
                    p.notify_error = false;
                    (paths, full_paths, need_full, notify_error)
                } else {
                    (HashSet::new(), HashSet::new(), true, true)
                }
            } else {
                (HashSet::new(), HashSet::new(), false, false)
            };
            for (label, graph) in graphs.iter_mut() {
                let initial_cycle = !graph.baseline;
                if initial_cycle {
                    // No baseline yet, so nothing about the graph's text identity
                    // is known. Once per graph, not once per cycle.
                    let _ = graph
                        .legacy_graph
                        .observe_graph_text_external_paths(std::iter::empty::<&Path>(), true);
                    graph.snap = collect_graph_text_files(&graph.legacy_graph).files;
                    graph.baseline = true;
                }
                let retry_due = graph.retry.take_due(Instant::now());
                let owned = pending_for_graph(&paths, &graph.legacy_graph);
                let full_owned = full_scan_owner_for_graph(&full_paths, &graph.legacy_graph);
                let need_full = event_need_full || !inotify || !full_owned.is_empty() || retry_due;
                let sync_dirty = initial_cycle
                    || need_full
                    || paths.iter().any(|path| path.starts_with(&graph.sync_dir));
                let mut sync_conflicts_dirty = false;
                let mut cycle_failed = false;
                let mut attempted = false;
                if sync_dirty && graph.sync_dir.is_dir() {
                    attempted = true;
                    match graph.legacy_graph.pull_managed_sync() {
                        Ok(pull) => {
                            graph.last_sync_error = None;
                            for change in pull.changes {
                                let _ = app.emit_to(
                                    label,
                                    "graph-changed",
                                    GraphChange {
                                        name: change.entry.name,
                                        kind: change.entry.kind,
                                        created: change.created,
                                        removed: change.removed,
                                    },
                                );
                            }
                            sync_conflicts_dirty = pull.conflicts_changed;
                        }
                        Err(error) => {
                            cycle_failed = true;
                            let message = error.to_string();
                            if graph.last_sync_error.as_deref() != Some(&message) {
                                let _ = app.emit_to(label, "managed-sync-error", &message);
                                graph.last_sync_error = Some(message);
                            }
                        }
                    }
                }
                if need_full || !owned.is_empty() {
                    attempted = true;
                    let (changes, conflicts_dirty, _, errors) = reconcile_pending(
                        &graph.legacy_graph,
                        &mut graph.snap,
                        &owned,
                        need_full,
                        !inotify,
                    );
                    for change in changes {
                        let _ = app.emit_to(label, "graph-changed", change);
                    }
                    if !errors.is_empty() {
                        cycle_failed = true;
                        let message = errors.join("; ");
                        if graph.last_sync_error.as_deref() != Some(&message) {
                            // NOT `managed-sync-error`. This loop is the legacy
                            // (Direct Markdown) lane -- a graph with a sparse
                            // runtime is removed from `graphs` and handled in
                            // `sparse_graphs`, so managed graphs never reach
                            // here. Telling a Direct-mode user "Tine-managed
                            // storage stopped, open Storage & sync to retry
                            // setup" during a write incident pointed them at a
                            // feature they are not using. This is a plain
                            // reconcile failure of the folder watch.
                            let _ = app.emit_to(label, "graph-watch-error", &message);
                            graph.last_sync_error = Some(message);
                        }
                    }
                    if conflicts_dirty || sync_conflicts_dirty {
                        let _ = app.emit_to(label, "conflicts-changed", ());
                    }
                } else if sync_conflicts_dirty {
                    let _ = app.emit_to(label, "conflicts-changed", ());
                }
                if cycle_failed {
                    graph.retry.failed(Instant::now());
                } else if attempted {
                    graph.retry.succeeded();
                    graph.last_sync_error = None;
                }
            }
            for (label, graph) in sparse_graphs.iter_mut() {
                let retry_due = graph.retry.take_due(Instant::now());
                let initial_tick = std::mem::take(&mut graph.initial_tick);
                let poll_cycle = !inotify && !retry_due;
                // The actor's startup scan can finish before this thread has
                // replaced the legacy directory watches with the recursive
                // graph-root watch. One scan after watch installation closes
                // that handoff interval; later steady-state events stay exact.
                let observations = sparse_observations(
                    &graph.root,
                    &paths,
                    &full_paths,
                    event_need_full || initial_tick || poll_cycle,
                    notify_error,
                );
                let (provider_paths, provider_imprecise) =
                    sparse_provider_observations(&graph.root, &paths, &full_paths);
                let provider_poll = poll_cycle;
                if observations.is_empty()
                    && provider_paths.is_empty()
                    && !provider_imprecise
                    && !provider_poll
                    && !retry_due
                    && !initial_tick
                    && !force_sparse_tick
                {
                    continue;
                }

                let result = (|| {
                    if !provider_paths.is_empty() || provider_imprecise || provider_poll {
                        graph.handle.observe_provider_paths(
                            provider_paths,
                            provider_imprecise || provider_poll,
                        )?;
                    }
                    if !observations.is_empty() {
                        graph.handle.observe_watcher(observations)?;
                    }
                    graph.handle.tick()
                })();
                match result {
                    Ok(tick) => {
                        // Only an admission that actually committed a batch is
                        // a content change. `AdmittedNoop` is, by its own name,
                        // the step that took no completed batch.
                        let changed = tick.committed_observable_change();
                        match &tick {
                            SyncRuntimeTick::LocalMutation(_)
                            | SyncRuntimeTick::Recovering
                            | SyncRuntimeTick::RetryFull => graph.retry.progressed(Instant::now()),
                            SyncRuntimeTick::RecoveryBlocked(_) | SyncRuntimeTick::Failed(_) => {
                                graph.retry.failed(Instant::now())
                            }
                            _ => graph.retry.succeeded(),
                        }
                        if matches!(
                            tick,
                            SyncRuntimeTick::RecoveryBlocked(_)
                                | SyncRuntimeTick::Blocked(_)
                                | SyncRuntimeTick::Terminal(_)
                                | SyncRuntimeTick::Failed(_)
                        ) {
                            let message = format!("{tick:?}");
                            if graph.last_error.as_deref() != Some(&message) {
                                let _ = app.emit_to(
                                    label,
                                    "sparse-v2-error",
                                    SparseV2ErrorEvent {
                                        binding_generation: graph.binding_generation,
                                        message: message.clone(),
                                    },
                                );
                                graph.last_error = Some(message);
                            }
                        } else {
                            graph.last_error = None;
                        }
                        let _ = app.emit_to(
                            label,
                            "sparse-v2-tick",
                            SparseV2TickEvent {
                                binding_generation: graph.binding_generation,
                                tick: crate::sync_runtime::tick_dto(tick),
                            },
                        );
                        if changed {
                            let _ = app.emit_to(label, "sparse-v2-changed", ());
                        }
                        if let Ok(status) = graph.handle.status() {
                            let _ = app.emit_to(
                                label,
                                "sparse-v2-status",
                                SparseV2RuntimeStatusEvent {
                                    binding_generation: graph.binding_generation,
                                    runtime: crate::sync_runtime::runtime_status(status),
                                },
                            );
                        }
                    }
                    Err(error) => {
                        graph.retry.failed(Instant::now());
                        let message = error.to_string();
                        if graph.last_error.as_deref() != Some(&message) {
                            let _ = app.emit_to(
                                label,
                                "sparse-v2-error",
                                SparseV2ErrorEvent {
                                    binding_generation: graph.binding_generation,
                                    message: message.clone(),
                                },
                            );
                            graph.last_error = Some(message);
                        }
                    }
                }
            }

            // --- wait for the next cycle ---
            if inotify && !watched.is_empty() {
                // Block until the kernel reports a change (or a control poke).
                // Coalesce the several events produced by one atomic save.
                let now = Instant::now();
                let retry_wait = graphs
                    .values()
                    .filter_map(|graph| graph.retry.remaining(now))
                    .chain(
                        sparse_graphs
                            .values()
                            .filter_map(|graph| graph.retry.remaining(now)),
                    )
                    .min();
                let wait_for =
                    inotify_cycle_wait(retry_wait, desired.difference(&watched).next().is_some());
                let woke_for_event = match wait_for {
                    Some(wait) => rx.recv_timeout(wait).is_ok(),
                    None => rx.recv().is_ok(),
                };
                if woke_for_event {
                    // Control pokes do not carry filesystem paths. Retain one
                    // explicit sparse actor turn for the next loop so a local
                    // managed save can drain its queued derivative work even
                    // when inotify correctly suppresses or coalesces its own
                    // projection write.
                    forced_sparse_tick = true;
                    std::thread::sleep(Duration::from_millis(200));
                    while rx.try_recv().is_ok() {}
                }
            } else {
                let now = Instant::now();
                let retry_wait = sparse_graphs
                    .values()
                    .filter_map(|graph| graph.retry.remaining(now))
                    .min()
                    .unwrap_or(Duration::from_secs(3))
                    .min(Duration::from_secs(3));
                let _ = rx.recv_timeout(retry_wait);
                while rx.try_recv().is_ok() {}
            }
        }
    });
}

/// How the file-watcher detects external changes (device-local, in
/// tine-settings.json): "inotify" (default on desktop) → a real OS watcher, no
/// idle wakeups; "poll" (default on Android) → a 3s mtime scan for filesystems
/// where inotify is flaky (some NFS, Android shared storage). See `start_watcher`.
fn watch_mode(app: &tauri::AppHandle) -> String {
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("watch_mode")
                .and_then(|x| x.as_str().map(String::from))
        })
        .filter(|m| m == "poll" || m == "inotify")
        .unwrap_or_else(|| {
            if cfg!(target_os = "android") {
                "poll".to_string()
            } else {
                "inotify".to_string()
            }
        })
}

#[tauri::command]
pub(crate) fn get_watch_mode(app: tauri::AppHandle) -> String {
    watch_mode(&app)
}

#[tauri::command]
pub(crate) fn set_watch_mode(
    mode: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
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
    use tine_core::model::{BlockDto, Format, PageDto};

    #[test]
    fn atomic_page_save_temp_events_stay_incremental() {
        use notify::event::{EventKind, ModifyKind, RenameMode};

        let page = PathBuf::from("/graphs/a/pages/one.md");
        let temp = PathBuf::from("/graphs/a/pages/.one.md.123.7.tmp");
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
    fn explicit_unmanaged_file_events_do_not_schedule_graph_scans() {
        use notify::event::{CreateKind, EventKind, RemoveKind};

        for (kind, path) in [
            (
                EventKind::Create(CreateKind::File),
                PathBuf::from("/graphs/a/assets/image.png"),
            ),
            (
                EventKind::Remove(RemoveKind::File),
                PathBuf::from("/graphs/a/logseq/config.edn"),
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
        }
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

    #[test]
    fn managed_sync_store_events_use_the_dedicated_incremental_lane() {
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
        assert_eq!(pending.paths, HashSet::from([chunk]));
    }

    #[test]
    fn sparse_watcher_routes_nested_unicode_nonstandard_text_and_fault_observations() {
        let root = PathBuf::from("/graphs/研究");
        let nested = root.join("archive/層/計画.markdown");
        let org = root.join("nonstandard/deep/日記.org");
        let unknown = root.join("config.edn");
        let outside = PathBuf::from("/graphs/other/pages/ignored.md");
        let paths = HashSet::from([nested, org, outside]);
        let full_paths = HashSet::from([unknown]);

        let observations = sparse_observations(&root, &paths, &full_paths, true, true);
        assert!(observations
            .contains(&SyncWatcherObservation::managed_path("archive/層/計画.markdown").unwrap()));
        assert!(observations
            .contains(&SyncWatcherObservation::managed_path("nonstandard/deep/日記.org").unwrap()));
        assert!(observations.contains(&SyncWatcherObservation::UnknownPath));
        assert!(observations.contains(&SyncWatcherObservation::RescanRequired));
        assert!(observations.contains(&SyncWatcherObservation::NotifyError));
        assert_eq!(
            observations
                .iter()
                .filter(|observation| matches!(observation, SyncWatcherObservation::ManagedPath(_)))
                .count(),
            2
        );
    }

    #[test]
    fn notify_failures_remain_distinct_from_rescan_obligations() {
        let mut pending = Pending::default();
        pending.add_notify_error();
        assert!(pending.need_full);
        assert!(pending.notify_error);

        let observations = sparse_observations(
            Path::new("/graph"),
            &HashSet::new(),
            &HashSet::new(),
            pending.need_full,
            pending.notify_error,
        );
        assert_eq!(
            observations,
            vec![
                SyncWatcherObservation::RescanRequired,
                SyncWatcherObservation::NotifyError
            ]
        );
    }

    #[test]
    fn sparse_watcher_does_not_reimport_its_private_archive_writes() {
        let root = PathBuf::from("/graph");
        let paths = HashSet::from([root.join(".tine-sync/v2/objects/immutable")]);
        assert!(sparse_observations(&root, &paths, &HashSet::new(), false, false).is_empty());
    }

    #[test]
    fn sparse_provider_poll_and_exact_events_stay_out_of_graph_reconciliation() {
        let root = PathBuf::from("/graphs/研究");
        let manifest = root.join(
            ".tine-sync/v2/shared/outbox/manifests/12345678-1234-1234-1234-123456789abc.manifest",
        );
        let paths = HashSet::from([manifest]);
        assert!(
            sparse_observations(&root, &paths, &HashSet::new(), false, false).is_empty(),
            "provider polling must not trigger graph-wide reconciliation"
        );
        let (provider_paths, imprecise) =
            sparse_provider_observations(&root, &paths, &HashSet::new());
        assert_eq!(
            provider_paths,
            vec!["manifests/12345678-1234-1234-1234-123456789abc.manifest".to_owned()]
        );
        assert!(!imprecise);

        let recovery = HashSet::from([
            root.join(
                ".tine-sync/v2/shared/outbox/manifest-recovery-links-v1/12345678-1234-1234-1234-123456789abc.link",
            ),
            root.join(
                ".tine-sync/v2/shared/outbox/manifest-recovery-blobs-v1/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.manifest",
            ),
        ]);
        let (provider_paths, imprecise) =
            sparse_provider_observations(&root, &recovery, &HashSet::new());
        assert_eq!(
            provider_paths,
            vec![
                "manifest-recovery-blobs-v1/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.manifest"
                    .to_owned(),
                "manifest-recovery-links-v1/12345678-1234-1234-1234-123456789abc.link"
                    .to_owned(),
            ]
        );
        assert!(!imprecise);

        let head = root.join(
            ".tine-sync/v2/shared/outbox/frontier-heads-v1/12345678-1234-1234-1234-123456789abc-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.head",
        );
        let (provider_paths, imprecise) =
            sparse_provider_observations(&root, &HashSet::from([head]), &HashSet::new());
        assert_eq!(
            provider_paths,
            vec![
                "frontier-heads-v1/12345678-1234-1234-1234-123456789abc-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.head"
                    .to_owned()
            ]
        );
        assert!(!imprecise);

        let internal = HashSet::from([
            root.join(".tine-sync/v2/shared/outbox/.part/local-write"),
            root.join(".tine-sync/v2/shared/outbox/removed/retired"),
        ]);
        let (provider_paths, imprecise) =
            sparse_provider_observations(&root, &internal, &HashSet::new());
        assert!(provider_paths.is_empty());
        assert!(!imprecise);

        // Poll mode has no filesystem paths; its caller independently sets
        // provider_imprecise=true while requesting a graph rescan only on the
        // ordinary three-second graph poll.
        let (provider_paths, imprecise) =
            sparse_provider_observations(&root, &HashSet::new(), &HashSet::new());
        assert!(provider_paths.is_empty());
        assert!(!imprecise);
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

    #[test]
    fn recovering_progress_retries_promptly_without_failure_backoff() {
        let start = Instant::now();
        let mut retry = RetrySchedule::default();
        retry.failed(start);
        retry.progressed(start);
        assert_eq!(retry.failures, 0);
        assert!(!retry.take_due(start));
        assert!(retry.take_due(start + Duration::from_millis(10)));
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
    fn legacy_watch_paths_keep_the_bound_graph_root_and_existing_reconcile_paths() {
        let temp = TempGraph::new("legacy-authority");
        let slot = GraphSlot::new(Graph::open(&temp.root), temp.root.clone());

        let (graph, root, dirs, sync_dir) = legacy_watch_paths(&slot).unwrap();
        assert_eq!(graph.root, temp.root);
        assert_eq!(root, temp.root);
        assert_eq!(dirs, graph_dirs(&graph));
        assert_eq!(sync_dir, graph.managed_sync_store_path());
    }

    fn event(kind: notify::event::EventKind, paths: Vec<PathBuf>) -> notify::Event {
        notify::Event {
            kind,
            paths,
            attrs: Default::default(),
        }
    }

    fn new_page(name: &str) -> PageDto {
        PageDto {
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

    fn warm_guarded_identity(graph: &Graph) {
        warm_cache(graph);
        let mut anchor = graph.load_by_path("pages/Anchor.md").unwrap().unwrap();
        anchor.blocks[0].raw = "warm guarded identity".to_owned();
        graph
            .save_page(&anchor, anchor.rev.as_deref())
            .expect("warm guarded identity save");
    }

    /// F2, poll half. A poll-mode cycle used to invalidate the whole guarded
    /// admission index unconditionally, so a user on NFS/SMB could never hold a
    /// warm index: every save rebuilt it from the entire graph, every time.
    #[test]
    fn quiet_poll_cycles_keep_the_admission_index_warm() {
        let tg = TempGraph::new("poll-warm");
        tg.write("pages/Anchor.md", "- anchor\n");
        tg.write("Root note.md", "- root\n");
        let graph = Graph::open(&tg.root);
        warm_guarded_identity(&graph);

        let before = graph.guarded_graph_text_identity_report();
        assert!(
            !before.invalidated && before.complete_builds >= 1,
            "the save should have left a live index: {before:?}"
        );

        let mut snap = collect_graph_text_files(&graph).files;
        for _ in 0..3 {
            reconcile_pending(&graph, &mut snap, &HashSet::new(), true, true);
        }

        let after = graph.guarded_graph_text_identity_report();
        assert!(
            !after.invalidated,
            "a poll cycle that found nothing must not invalidate the index: {after:?}"
        );
        assert_eq!(
            after.complete_builds, before.complete_builds,
            "a poll cycle must not force the next save to rebuild the whole graph"
        );
    }

    /// The other half of the same policy: a poll cycle that DID observe an
    /// external change must publish it, or the index would still claim a page
    /// exists after it was deleted.
    #[test]
    fn a_poll_cycle_publishes_what_it_actually_observed() {
        let tg = TempGraph::new("poll-observe");
        tg.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&tg.root);
        warm_guarded_identity(&graph);
        let before = graph.guarded_graph_text_identity_report();

        // An external creation the poll cycle is about to find.
        tg.write("Root note.md", "title:: Root note\n\n- external\n");
        let mut snap = collect_graph_text_files(&graph).files;
        snap.remove(&tg.path("Root note.md"));
        reconcile_pending(&graph, &mut snap, &HashSet::new(), true, true);

        let after = graph.guarded_graph_text_identity_report();
        assert!(
            !after.invalidated,
            "an exactly-observed change must not fall back to invalidation: {after:?}"
        );
        assert!(
            after.exact_updates > before.exact_updates,
            "the observed path must reach the index as an exact update: {after:?}"
        );
        // And the index now owns that name, so a colliding create is refused.
        assert_new_page_refused(&graph, "Root note");
    }

    /// The fallback half. If the rescan could not read part of the graph, what
    /// it found is NOT a complete account of what changed, and the index must be
    /// invalidated rather than exactly updated -- otherwise a save would trust
    /// an index that is missing whatever lives behind the unreadable directory.
    #[test]
    fn an_incomplete_poll_scan_invalidates_instead_of_publishing() {
        let tg = TempGraph::new("poll-incomplete");
        tg.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&tg.root);
        warm_guarded_identity(&graph);
        assert!(!graph.guarded_graph_text_identity_report().invalidated);

        let mut complete = collect_graph_text_files(&graph);
        complete.complete = false;
        publish_poll_observation(&graph, &complete.files.clone(), &complete);

        assert!(
            graph.guarded_graph_text_identity_report().invalidated,
            "a scan that could not read the whole graph must not leave the index trusted"
        );
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

    #[test]
    fn legacy_graph_root_text_create_delete_rename_and_semantics_reach_guarded_identity() {
        use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};

        for extension in ["md", "org"] {
            let graph_dir = TempGraph::new(&format!("root-text-{extension}"));
            graph_dir.write("pages/Anchor.md", "- anchor\n");
            let graph = Graph::open(&graph_dir.root);
            warm_guarded_identity(&graph);

            let created_rel = format!("nonstandard/deep/Physical Name.{extension}");
            graph_dir.write(
                &created_rel,
                &format!("title:: Created {extension}\n\n- external\n"),
            );
            assert!(observe_legacy_graph_text_event(
                &graph,
                &graph_dir.root,
                Some(&event(
                    EventKind::Create(CreateKind::File),
                    vec![graph_dir.path(&created_rel)],
                )),
            ));
            assert_new_page_refused(&graph, &format!("Created {extension}"));

            let deleted_rel = format!("nonstandard/deep/Delete {extension}.{extension}");
            graph_dir.write(&deleted_rel, "- external\n");
            observe_legacy_graph_text_event(
                &graph,
                &graph_dir.root,
                Some(&event(
                    EventKind::Create(CreateKind::File),
                    vec![graph_dir.path(&deleted_rel)],
                )),
            );
            assert_new_page_refused(&graph, &format!("Delete {extension}"));
            graph_dir.remove(&deleted_rel);
            let delete_event = event(
                EventKind::Remove(RemoveKind::File),
                vec![graph_dir.path(&deleted_rel)],
            );
            let deletion =
                legacy_graph_text_observation(&graph, &graph_dir.root, Some(&delete_event));
            assert!(!deletion.uncertain);
            assert_eq!(deletion.exact_paths, vec![graph_dir.path(&deleted_rel)]);
            observe_legacy_graph_text_event(&graph, &graph_dir.root, Some(&delete_event));

            let old_rel = format!("nonstandard/deep/Old {extension}.{extension}");
            let new_rel = format!("nonstandard/deep/New {extension}.{extension}");
            graph_dir.write(&old_rel, "- external\n");
            observe_legacy_graph_text_event(
                &graph,
                &graph_dir.root,
                Some(&event(
                    EventKind::Create(CreateKind::File),
                    vec![graph_dir.path(&old_rel)],
                )),
            );
            assert_new_page_refused(&graph, &format!("Old {extension}"));
            graph_dir.rename(&old_rel, &new_rel);
            let rename_event = event(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                vec![graph_dir.path(&old_rel), graph_dir.path(&new_rel)],
            );
            let rename =
                legacy_graph_text_observation(&graph, &graph_dir.root, Some(&rename_event));
            assert!(!rename.uncertain);
            assert_eq!(
                rename.exact_paths,
                vec![graph_dir.path(&old_rel), graph_dir.path(&new_rel)]
            );
            observe_legacy_graph_text_event(&graph, &graph_dir.root, Some(&rename_event));
            assert_new_page_refused(&graph, &format!("New {extension}"));
        }
    }

    #[test]
    fn legacy_uncertain_graph_root_events_advance_the_shared_resource_epoch() {
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
            let observer = Graph::open(&graph_dir.root);
            let guarded = Graph::open(&graph_dir.root);
            warm_guarded_identity(&guarded);
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
            let observation =
                legacy_graph_text_observation(&observer, &graph_dir.root, event.as_ref());
            assert!(observation.relevant, "{case}");
            assert!(observation.uncertain, "{case}");
            assert!(observe_legacy_graph_text_event(
                &observer,
                &graph_dir.root,
                event.as_ref(),
            ));
            assert_new_page_refused(&guarded, &format!("Epoch {case}"));
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
        warm_guarded_identity(&graph);

        graph_dir.write("pages/External.md", "- external\n");
        let path = graph_dir.path("pages/External.md");

        for kind in [
            EventKind::Create(CreateKind::Any),
            EventKind::Modify(ModifyKind::Any),
        ] {
            let observation = legacy_graph_text_observation(
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
        let directory = legacy_graph_text_observation(
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
        let removed = legacy_graph_text_observation(
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
    fn legacy_graph_root_observation_excludes_other_open_graphs() {
        use notify::event::{CreateKind, EventKind};

        let graph_a_dir = TempGraph::new("root-owner-a");
        let graph_b_dir = TempGraph::new("root-owner-b");
        graph_a_dir.write("pages/Anchor.md", "- anchor A\n");
        graph_b_dir.write("pages/Anchor.md", "- anchor B\n");
        let graph_a = Graph::open(&graph_a_dir.root);
        let graph_b = Graph::open(&graph_b_dir.root);
        warm_guarded_identity(&graph_b);

        graph_b_dir.write(
            "nonstandard/deep/Stale.md",
            "title:: Must Stay Stale\n\n- external without a B callback\n",
        );
        graph_a_dir.write("nonstandard/deep/A.md", "- observed A\n");
        let event_a = event(
            EventKind::Create(CreateKind::File),
            vec![graph_a_dir.path("nonstandard/deep/A.md")],
        );
        assert!(observe_legacy_graph_text_event(
            &graph_a,
            &graph_a_dir.root,
            Some(&event_a),
        ));
        assert!(!observe_legacy_graph_text_event(
            &graph_b,
            &graph_b_dir.root,
            Some(&event_a),
        ));
        graph_b
            .save_page(&new_page("Must Stay Stale"), None)
            .expect("an event owned by graph A must not invalidate graph B");
    }

    #[test]
    fn excluded_private_text_and_exact_non_text_events_are_harmless() {
        use notify::event::{CreateKind, EventKind};

        let graph_dir = TempGraph::new("excluded-private");
        graph_dir.write("pages/Anchor.md", "- anchor\n");
        let graph = Graph::open(&graph_dir.root);
        warm_guarded_identity(&graph);

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
            let observation = legacy_graph_text_observation(&graph, &graph_dir.root, Some(&event));
            assert!(observation.relevant, "{relative}");
            assert!(!observation.uncertain, "{relative}");
            assert!(observation.exact_paths.is_empty(), "{relative}");
            observe_legacy_graph_text_event(&graph, &graph_dir.root, Some(&event));
            graph
                .save_page(&new_page(claimed), None)
                .expect("excluded text must not become a retained graph-text owner");
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
        let observation = legacy_graph_text_observation(&graph, &graph_dir.root, Some(&non_text));
        assert!(!observation.uncertain);
        assert!(observation.exact_paths.is_empty());
        observe_legacy_graph_text_event(&graph, &graph_dir.root, Some(&non_text));
        graph
            .save_page(&new_page("Exact Non Text Is Harmless"), None)
            .expect("an exact non-text event must not invalidate retained identity");

        for relative in [".tine-sync", "assets", "logseq/bak", ".hidden"] {
            let private_directory = event(
                EventKind::Create(CreateKind::Folder),
                vec![graph_dir.path(relative)],
            );
            let observation =
                legacy_graph_text_observation(&graph, &graph_dir.root, Some(&private_directory));
            assert!(!observation.uncertain, "{relative}");
            assert!(observation.exact_paths.is_empty(), "{relative}");
        }
    }

    fn graph_dirs(graph: &Graph) -> [PathBuf; 2] {
        [graph.journals_path(), graph.pages_path()]
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
}
