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

/// Recursively collect every `.md`/`.org` page file under `dir` with its
/// (mtime, len) — the watcher's diff snapshot. Descends sub-directories so a page
/// in a sub-folder (#21) is reconciled like a top-level one; mirrors the core's
/// `list_md` walk: match page files by extension (the metadata read is needed for
/// mtime/len anyway), skip hidden dirs and symlinked dirs (no cycles, no escaping
/// the watched tree). Scoped to the dir passed in (journals/ or pages/).
fn collect_page_files(dir: &std::path::Path, out: &mut HashMap<PathBuf, FileStamp>) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(file_type) = e.file_type() else {
                continue;
            };
            if is_page_file_path(&p) {
                // Never follow a page-looking symlink. Besides cycles, a
                // `secret.md` symlink could otherwise expose outside bytes.
                if file_type.is_file() {
                    let Ok(md) = e.metadata() else { continue };
                    if let Some(stamp) = metadata_stamp(&md) {
                        out.insert(p, stamp);
                    }
                }
                continue;
            }
            let hidden = p
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.starts_with('.'))
                .unwrap_or(true);
            if !hidden && file_type.is_dir() {
                stack.push(p);
            }
        }
    }
}

fn collect_graph_page_files(dirs: &[PathBuf; 2]) -> HashMap<PathBuf, FileStamp> {
    let mut current: HashMap<PathBuf, FileStamp> = HashMap::new();
    for dir in dirs {
        collect_page_files(dir, &mut current);
    }
    current
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

fn reconcile_pending(
    graph: &Graph,
    dirs: &[PathBuf; 2],
    snap: &mut HashMap<PathBuf, FileStamp>,
    paths: &HashSet<PathBuf>,
    need_full: bool,
) -> (Vec<GraphChange>, bool, bool, Vec<String>) {
    if need_full || paths.is_empty() {
        let current = collect_graph_page_files(dirs);
        let (changes, conflicts_dirty, errors) = full_diff_reconcile(graph, snap, current);
        (changes, conflicts_dirty, true, errors)
    } else {
        let (changes, conflicts_dirty, errors) = incremental_reconcile(graph, snap, paths);
        (changes, conflicts_dirty, false, errors)
    }
}

fn pending_for_graph(paths: &HashSet<PathBuf>, dirs: &[PathBuf; 2]) -> HashSet<PathBuf> {
    paths
        .iter()
        .filter(|path| dirs.iter().any(|dir| path.starts_with(dir)))
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

    let explicit_file_event = matches!(
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
            last_error: Option<String>,
            retry: RetrySchedule,
            initial_tick: bool,
        }

        let mut graphs: HashMap<String, WatchedGraph> = HashMap::new();
        let mut sparse_graphs: HashMap<String, WatchedSparse> = HashMap::new();
        let mut watcher: Option<notify::RecommendedWatcher> = None;
        let mut watched: HashSet<PathBuf> = HashSet::new();
        loop {
            let inotify = watch_mode(&app) != "poll";
            let entries = app.state::<AppState>().graphs.read().unwrap().entries();
            let live: HashSet<String> = entries.iter().map(|(label, _)| label.clone()).collect();
            graphs.retain(|label, _| live.contains(label));
            sparse_graphs.retain(|label, _| live.contains(label));
            for (label, slot) in entries {
                if let Some(handle) = slot.sparse_runtime().cloned() {
                    graphs.remove(&label);
                    match sparse_graphs.get_mut(&label) {
                        Some(current) if current.root == slot.root_key => {
                            current.handle = handle;
                        }
                        _ => {
                            sparse_graphs.insert(
                                label,
                                WatchedSparse {
                                    handle,
                                    root: slot.root_key.clone(),
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

            let desired: HashSet<PathBuf> = graphs
                .values()
                .map(|graph| graph.root.clone())
                .chain(sparse_graphs.values().map(|graph| graph.root.clone()))
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
                        if w.watch(&dir, notify::RecursiveMode::Recursive).is_ok() {
                            watched.insert(dir);
                        }
                    }
                }
            } else if watcher.is_some() {
                watcher = None; // poll mode → release the OS watcher
                watched.clear();
            }

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
                if initial_cycle || !inotify {
                    let _ = graph
                        .legacy_graph
                        .observe_graph_text_external_paths(std::iter::empty::<&Path>(), true);
                }
                if initial_cycle {
                    graph.snap = collect_graph_page_files(&graph.dirs);
                    graph.baseline = true;
                }
                let retry_due = graph.retry.take_due(Instant::now());
                let owned = pending_for_graph(&paths, &graph.dirs);
                let full_owned = pending_for_graph(&full_paths, &graph.dirs);
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
                        &graph.dirs,
                        &mut graph.snap,
                        &owned,
                        need_full,
                    );
                    for change in changes {
                        let _ = app.emit_to(label, "graph-changed", change);
                    }
                    if !errors.is_empty() {
                        cycle_failed = true;
                        let message = errors.join("; ");
                        if graph.last_sync_error.as_deref() != Some(&message) {
                            let _ = app.emit_to(label, "managed-sync-error", &message);
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
                        let completed = matches!(
                            tick,
                            SyncRuntimeTick::AdmittedNoop { .. }
                                | SyncRuntimeTick::AdmittedComplete { .. }
                        );
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
                                let _ = app.emit_to(label, "sparse-v2-error", &message);
                                graph.last_error = Some(message);
                            }
                        } else {
                            graph.last_error = None;
                        }
                        let _ = app.emit_to(
                            label,
                            "sparse-v2-tick",
                            crate::sync_runtime::tick_dto(tick),
                        );
                        if completed {
                            let _ = app.emit_to(label, "sparse-v2-changed", ());
                        }
                        if let Ok(status) = graph.handle.status() {
                            let _ = app.emit_to(
                                label,
                                "sparse-v2-status",
                                crate::sync_runtime::runtime_status(status),
                            );
                        }
                    }
                    Err(error) => {
                        graph.retry.failed(Instant::now());
                        let message = error.to_string();
                        if graph.last_error.as_deref() != Some(&message) {
                            let _ = app.emit_to(label, "sparse-v2-error", &message);
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
                let woke_for_event = match retry_wait {
                    Some(wait) => rx.recv_timeout(wait).is_ok(),
                    None => rx.recv().is_ok(),
                };
                if woke_for_event {
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

    #[test]
    fn pending_paths_are_dispatched_only_to_the_owning_graph() {
        let paths = HashSet::from([
            PathBuf::from("/graphs/a/pages/one.md"),
            PathBuf::from("/graphs/b/journals/2026_07_10.md"),
        ]);
        let a = [
            PathBuf::from("/graphs/a/journals"),
            PathBuf::from("/graphs/a/pages"),
        ];
        let b = [
            PathBuf::from("/graphs/b/journals"),
            PathBuf::from("/graphs/b/pages"),
        ];
        assert_eq!(pending_for_graph(&paths, &a).len(), 1);
        assert_eq!(pending_for_graph(&paths, &b).len(), 1);
        assert!(pending_for_graph(&paths, &a)
            .iter()
            .all(|path| path.starts_with("/graphs/a")));
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

        let dirs = graph_dirs(&inc_graph);
        let mut inc_snap = collect_graph_page_files(&dirs);
        let mut full_snap = inc_snap.clone();

        let paths = mutate(&tg);

        let (inc_changes, inc_conflicts_dirty, inc_errors) =
            incremental_reconcile(&inc_graph, &mut inc_snap, &paths);
        let fresh = collect_graph_page_files(&dirs);
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

    #[test]
    fn collect_page_files_descends_subdirectories() {
        // #21: the watcher snapshot must include page files in sub-folders, so an
        // edit/create there is reconciled (not invisible until a graph reopen).
        let dir = std::env::temp_dir().join(format!("tine-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Archive/Deep/Deeper")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        std::fs::write(dir.join("top.md"), "- t\n").unwrap();
        std::fs::write(dir.join("Archive/mid.org"), "* m\n").unwrap();
        std::fs::write(dir.join("Archive/Deep/Deeper/deep.md"), "- d\n").unwrap();
        std::fs::write(dir.join("Archive/notes.txt"), "ignored\n").unwrap();
        std::fs::write(dir.join(".hidden/skip.md"), "- s\n").unwrap();

        let mut out: HashMap<PathBuf, FileStamp> = HashMap::new();
        collect_page_files(&dir, &mut out);
        let mut names: Vec<String> = out
            .keys()
            .map(|p| {
                p.strip_prefix(&dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["Archive/Deep/Deeper/deep.md", "Archive/mid.org", "top.md"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn collect_page_files_does_not_follow_page_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!("tine-watch-link-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("tine-watch-outside-{}.md", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&outside);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&outside, "- outside\n").unwrap();
        symlink(&outside, dir.join("secret.md")).unwrap();

        let mut out = HashMap::new();
        collect_page_files(&dir, &mut out);
        assert!(out.is_empty());

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&outside).ok();
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
        let dirs = graph_dirs(&graph);
        let mut snap = collect_graph_page_files(&dirs);
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

        let dirs = graph_dirs(&inc_graph);
        let mut inc_snap = collect_graph_page_files(&dirs);
        let mut full_snap = inc_snap.clone();

        tg.write(
            "pages/sub/CreatedByDirEvent.md",
            "- created through dir op\n",
        );
        let incomplete_paths = rel_paths(&tg, &["pages/Seed.md"]);
        let (inc_changes, inc_conflicts_dirty, used_full, inc_errors) =
            reconcile_pending(&inc_graph, &dirs, &mut inc_snap, &incomplete_paths, true);
        let fresh = collect_graph_page_files(&dirs);
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
