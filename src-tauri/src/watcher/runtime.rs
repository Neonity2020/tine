use super::*;

#[derive(Default)]
pub(super) struct GraphTextObservation {
    pub(super) exact_paths: Vec<PathBuf>,
    pub(super) uncertain: bool,
    pub(super) relevant: bool,
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

pub(super) fn graph_text_observation(
    graph: &Graph,
    root: &Path,
    event: Option<&notify::Event>,
) -> GraphTextObservation {
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind};

    let Some(event) = event else {
        return GraphTextObservation {
            uncertain: true,
            relevant: true,
            ..GraphTextObservation::default()
        };
    };
    if event.paths.is_empty() {
        return GraphTextObservation {
            uncertain: true,
            relevant: true,
            ..GraphTextObservation::default()
        };
    }

    let owned = event
        .paths
        .iter()
        .filter(|path| path.starts_with(root))
        .collect::<Vec<_>>();
    if owned.is_empty() {
        return GraphTextObservation::default();
    }

    let mut observation = GraphTextObservation {
        relevant: true,
        uncertain: event.need_rescan(),
        ..GraphTextObservation::default()
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
    // A rename pair whose surviving side is a file moved a file: the old name
    // needs no index to answer for it.
    let rename_has_file_witness = matches!(event.kind, EventKind::Modify(ModifyKind::Name(_)))
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
            // Configuration is not graph text; its own queue decides how far a
            // change reaches (GH #543, audit R9-05).
            GraphTextExactFeedPathClass::Configuration => continue,
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
        } else if rename_has_file_witness {
            if is_page_file_path(path) {
                observation.exact_paths.push(path.clone());
            }
        } else {
            // A rename without that witness, or a kind that did not say file or
            // directory (such as `Remove(Any)`): the old name no longer exists
            // to ask. `graph_text_watch_reach` answers from the current identity
            // index, and is the same answer the batch queue uses (GH #543,
            // audit R9-06).
            match graph.graph_text_watch_reach(path) {
                GraphTextWatchReach::Nothing => continue,
                GraphTextWatchReach::File => {
                    if is_page_file_path(path) {
                        observation.exact_paths.push(path.clone());
                    }
                }
                GraphTextWatchReach::Subtree => {
                    if descendants_excluded {
                        continue;
                    }
                    observation.uncertain = true;
                    break;
                }
            }
        }
    }

    if observation.uncertain {
        observation.exact_paths.clear();
    }
    observation
}

/// Linearize a platform callback with guarded graph-text writes before the
/// watcher's debounce/reconciliation delay. External callbacks publish an
/// admission epoch (and invalidate retained identity only when ambiguous) under
/// the same resource-scoped mutation authority that `Graph::save_page` uses.
/// Exact candidates for a Tine self echo take a bounded two-open identity+bytes
/// proof; debounced reconciliation still captures each final path once.
fn observe_graph_text_callback(
    app: &tauri::AppHandle,
    event: Option<&notify::Event>,
) -> Vec<(PathBuf, GraphTextExternalObservationTicket)> {
    let state = app.state::<AppState>();
    let entries = match state.graphs.read() {
        Ok(graphs) => graphs.entries(),
        Err(_) => return Vec::new(),
    };
    let mut observations = Vec::new();
    for (_, slot) in entries {
        let Ok((graph, root)) = direct_watch_paths(&slot) else {
            continue;
        };
        if observe_graph_text_event(&graph, &root, event) {
            observations.push((root, graph.graph_text_external_observation_ticket()));
        }
    }
    observations
}

pub(super) struct WatchedGraph {
    pub(super) graph: Arc<Graph>,
    pub(super) root: PathBuf,
    pub(super) assets: AssetWatchState,
    pub(super) snap: HashMap<PathBuf, FileStamp>,
    pub(super) baseline: bool,
    pub(super) last_reconcile_error: Option<String>,
    pub(super) retry: RetrySchedule,
    /// Frontier already drained from `Pending` but not yet reconciled
    /// successfully. It survives retry cycles and is acknowledged only after
    /// the matching graph batch succeeds.
    pub(super) pending_observation_epoch: Option<GraphTextExternalObservationTicket>,
    /// A cycle skipped this graph because its storage transition lane was
    /// held; the paths it drained are gone, so the next cycle diffs in full.
    pub(super) transition_skipped: bool,
}

impl WatchedGraph {
    pub(super) fn new(graph: Arc<Graph>, root: PathBuf, asset_root: Option<PathBuf>) -> Self {
        Self::with_assets(
            graph,
            root,
            asset_root.map(AssetWatchState::new).unwrap_or_default(),
        )
    }

    fn with_assets(graph: Arc<Graph>, root: PathBuf, assets: AssetWatchState) -> Self {
        Self {
            assets,
            graph,
            root,
            snap: HashMap::new(),
            baseline: false,
            last_reconcile_error: None,
            retry: RetrySchedule::default(),
            pending_observation_epoch: None,
            transition_skipped: false,
        }
    }

    /// The slot at this root handed the watcher `graph` and `asset_root`:
    /// the one place a same-root watch takes them on (GH #543, audit R12-03).
    ///
    /// Which state belongs to what? The graph-text half belongs to the
    /// graph. A reopen (a restore, a config change) that replaced the graph
    /// read the files itself, and its owner indexes them, so the watch
    /// starts from the new graph's own baseline. Diffing the old graph's
    /// stamps against it re-synced every file a restore rewrote, identical
    /// bytes included (audit R11-05). Only a drained frontier the new graph
    /// owns carries over: it is still owed its acknowledgement.
    ///
    /// The asset half belongs to the asset folder, which a reopen does not
    /// replace. Re-snapshotting it took an asset change still queued for
    /// this cycle into the baseline, and the old image stayed on screen
    /// (audit R13-02). It is started afresh only when the folder changes.
    pub(super) fn take_on(&mut self, graph: Arc<Graph>, asset_root: Option<PathBuf>) {
        let assets = if asset_root.as_ref() == self.assets.active_root() {
            std::mem::take(&mut self.assets)
        } else {
            asset_root.map(AssetWatchState::new).unwrap_or_default()
        };
        if Arc::ptr_eq(&self.graph, &graph) {
            self.assets = assets;
            return;
        }
        let owed = self
            .pending_observation_epoch
            .filter(|ticket| graph.owns_graph_text_external_observation_ticket(*ticket));
        let root = std::mem::take(&mut self.root);
        *self = Self::with_assets(graph, root, assets);
        self.pending_observation_epoch = owed;
    }
}

/// Whether this cycle walks a root in full. A root that is polled, or newly
/// watched, is walked because events for it may have been missed; but a
/// first cycle just walked it for its baseline. Walking it again cost every
/// newly watched graph two whole-graph stat walks (GH #543, audit R12-07).
/// Events drained this cycle still diff, so their observation is still
/// acknowledged.
pub(super) fn rewalks_root(initial_cycle: bool, polled: bool, handed_over: bool) -> bool {
    !initial_cycle && (polled || handed_over)
}

fn asset_root_for_slot(app: &tauri::AppHandle, slot: &GraphSlot) -> Option<PathBuf> {
    let root = &slot.root_key;
    match Graph::external_assets_target(root) {
        Ok(Some(live)) => crate::settings::approved_external_assets(app, root)
            .and_then(|approved| std::fs::canonicalize(approved).ok())
            .filter(|approved| approved == &live)
            .map(|_| live),
        Ok(None) => Some(root.join("assets")),
        Err(_) => None,
    }
}

pub(super) fn route_drained_direct_frontiers(
    graphs: &mut HashMap<String, WatchedGraph>,
    latest_entries: Vec<(String, Arc<GraphSlot>)>,
    drained: &HashMap<PathBuf, GraphTextExternalObservationTicket>,
    asset_root_for: impl Fn(&GraphSlot) -> Option<PathBuf>,
) {
    for (label, slot) in latest_entries {
        let asset_root = asset_root_for(&slot);
        let Ok((latest_graph, root)) = direct_watch_paths(&slot) else {
            continue;
        };
        let Some(ticket) = drained.get(&root).copied() else {
            continue;
        };
        if !latest_graph.owns_graph_text_external_observation_ticket(ticket) {
            continue;
        }
        match graphs.get_mut(&label) {
            Some(current) if current.root == root => {
                if !current
                    .graph
                    .owns_graph_text_external_observation_ticket(ticket)
                {
                    current.take_on(latest_graph, asset_root);
                }
            }
            _ => {
                graphs.insert(label, WatchedGraph::new(latest_graph, root, asset_root));
            }
        }
    }
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
/// by comparing each file with the revision Tine last wrote or read
/// (`page_revision_current`, inside `sync_file`). A control channel (poked by
/// `load_graph` on a graph switch and by `set_watch_mode`) lets the thread
/// re-target or switch mechanism at once, without polling for those either.
pub(crate) fn start_watcher(app: tauri::AppHandle) {
    use notify::Watcher;
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let pending = Arc::new(Mutex::new(Pending::default()));
    // The roots the OS watcher currently covers, shared with its callback so
    // the callback can drop VCS/tool noise before it costs anything. Written by
    // the loop each cycle, read once per event.
    let watched_roots: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
    if let Ok(mut slot) = app.state::<AppState>().watch_ctl.lock() {
        *slot = Some(tx.clone());
    }
    std::thread::spawn(move || {
        // Last observed disposable image per live window binding. This only
        // coalesces UI invalidations; no query ever waits on these revisions.
        let mut query_images: HashMap<String, (u64, Option<u64>)> = HashMap::new();
        let mut graphs: HashMap<String, WatchedGraph> = HashMap::new();
        // Windows whose configuration still needs re-reading: named by an event
        // this cycle could not act on because the storage transition lane was
        // busy. Carried across cycles so a deferral cannot lose the change.
        let mut config_recheck: HashSet<String> = HashSet::new();
        let mut watcher: Option<notify::RecommendedWatcher> = None;
        let mut watched: HashSet<PathBuf> = HashSet::new();
        // Last surfaced `watch()` failure per graph root, so a root that keeps
        // failing reports once instead of every cycle.
        let mut watch_failures: HashMap<PathBuf, String> = HashMap::new();
        // Last surfaced failure to create the OS watcher, so a retry loop
        // reports once.
        let mut watcher_failure: Option<String> = None;
        loop {
            let wants_inotify = watch_mode(&app) != "poll";
            let entries = app.state::<AppState>().graphs.read().unwrap().entries();
            let live: HashSet<String> = entries.iter().map(|(label, _)| label.clone()).collect();
            query_images.retain(|label, _| live.contains(label));
            graphs.retain(|label, _| live.contains(label));
            for (label, slot) in entries {
                let asset_root = asset_root_for_slot(&app, &slot);
                let Ok((slot_graph, root)) = direct_watch_paths(&slot) else {
                    graphs.remove(&label);
                    continue;
                };
                let image = slot_graph.observe_direct_projection_commits(tx.clone());
                let observed = (slot.binding_generation, image);
                if query_images.insert(label.clone(), observed) != Some(observed) && image.is_some()
                {
                    let _ =
                        app.emit_to(&label, "query-projection-changed", slot.binding_generation);
                }
                match graphs.get_mut(&label) {
                    Some(current) if current.root == root => {
                        if current.pending_observation_epoch.is_some_and(|ticket| {
                            !slot_graph.owns_graph_text_external_observation_ticket(ticket)
                        }) {
                            current.pending_observation_epoch = None;
                        }
                        current.take_on(slot_graph, asset_root);
                    }
                    _ => {
                        graphs.insert(label, WatchedGraph::new(slot_graph, root, asset_root));
                    }
                }
            }

            let labels_by_root: Vec<(String, PathBuf)> = graphs
                .iter()
                .map(|(label, graph)| (label.clone(), graph.root.clone()))
                .collect();
            let watch_labels = labels_by_root
                .iter()
                .cloned()
                .chain(
                    graphs
                        .iter()
                        .filter(|(_, graph)| {
                            graph.assets.active && !graph.assets.root.starts_with(&graph.root)
                        })
                        .map(|(label, graph)| (label.clone(), graph.assets.root.clone())),
                )
                .collect::<Vec<_>>();
            let desired: HashSet<PathBuf> =
                watch_labels.iter().map(|(_, root)| root.clone()).collect();
            if let Ok(mut roots) = watched_roots.lock() {
                if *roots != desired {
                    roots.clone_from(&desired);
                }
            }

            // Bring the OS watcher in line with the current mode + graph roots.
            let mut newly_watched = HashSet::new();
            if wants_inotify {
                if watcher.is_none() {
                    let txc = tx.clone();
                    let pendingc = pending.clone();
                    let appc = app.clone();
                    let rootsc = watched_roots.clone();
                    let created =
                        notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                            // A repository's own churn is not a graph change.
                            // Dropped here, before the app-state lock, the graph
                            // leases, the scope classifications and the wake.
                            if let Ok(event) = &res {
                                if rootsc
                                    .lock()
                                    .is_ok_and(|roots| watch_event_is_tool_noise(event, &roots))
                                {
                                    return;
                                }
                            }
                            if let Ok(mut p) = pendingc.lock() {
                                let observations = match &res {
                                    Ok(event) => observe_graph_text_callback(&appc, Some(event)),
                                    Err(_) => observe_graph_text_callback(&appc, None),
                                };
                                p.add_legacy_observations(observations);
                                match res {
                                    Ok(event) => p.add_event(event),
                                    Err(_) => p.add_notify_error(),
                                }
                            }
                            let _ = txc.send(());
                        });
                    watcher = match created {
                        Ok(created) => {
                            watcher_failure = None;
                            Some(created)
                        }
                        Err(error) => {
                            // No OS watcher (for example at the inotify
                            // instance limit): this cycle polls instead, and
                            // the next one tries again. Swallowing the error
                            // left every external change, and every graph
                            // configuration change, unseen for the session
                            // (GH #543, audit R10-07).
                            let message = format!(
                                "file watching is unavailable ({error}); checking for changes by polling"
                            );
                            if watcher_failure.as_ref() != Some(&message) {
                                for (label, _) in watch_labels.iter() {
                                    let _ = app.emit_to(label, "graph-watch-error", &message);
                                }
                                watcher_failure = Some(message);
                            }
                            None
                        }
                    };
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
                                newly_watched.insert(dir.clone());
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
                                    for (label, root) in watch_labels.iter() {
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
            // Events come from the OS watcher only while there is one;
            // otherwise this cycle is a poll, whatever the setting says.
            let inotify = wants_inotify && watcher.is_some();
            watch_failures.retain(|dir, _| desired.contains(dir));

            // --- reconcile (identical in both modes) ---
            let (
                paths,
                full_paths,
                asset_paths,
                asset_full_paths,
                config_paths,
                drained_observation_epochs,
                event_need_full,
                notify_error,
                first_event_at,
            ) = if inotify {
                if let Ok(mut p) = pending.lock() {
                    let paths = std::mem::take(&mut p.paths);
                    let full_paths = std::mem::take(&mut p.full_paths);
                    let asset_paths = std::mem::take(&mut p.asset_paths);
                    let asset_full_paths = std::mem::take(&mut p.asset_full_paths);
                    let config_paths = std::mem::take(&mut p.config_paths);
                    let observation_epochs = p.take_legacy_observation_epochs();
                    let need_full = p.need_full;
                    let notify_error = p.notify_error;
                    let first_event_at = p.first_event_at.take();
                    p.need_full = false;
                    p.notify_error = false;
                    (
                        paths,
                        full_paths,
                        asset_paths,
                        asset_full_paths,
                        config_paths,
                        observation_epochs,
                        need_full,
                        notify_error,
                        first_event_at,
                    )
                } else {
                    (
                        HashSet::new(),
                        HashSet::new(),
                        HashSet::new(),
                        HashSet::new(),
                        HashSet::new(),
                        HashMap::new(),
                        true,
                        true,
                        None,
                    )
                }
            } else {
                (
                    HashSet::new(),
                    HashSet::new(),
                    HashSet::new(),
                    HashSet::new(),
                    HashSet::new(),
                    HashMap::new(),
                    false,
                    false,
                    None,
                )
            };
            // The callback reads AppState independently from this loop. A
            // same-root refresh can therefore publish a ticket for the new
            // Graph after this cycle took its initial slot snapshot but before
            // it drained Pending. Re-read only when a Direct frontier was
            // drained and route it to the exact instance that minted it. A
            // stale WatchedGraph must never consume the path while silently
            // discarding the replacement's ticket.
            if !drained_observation_epochs.is_empty() {
                let latest_entries = app.state::<AppState>().graphs.read().unwrap().entries();
                route_drained_direct_frontiers(
                    &mut graphs,
                    latest_entries,
                    &drained_observation_epochs,
                    |slot| asset_root_for_slot(&app, slot),
                );
            }
            // A focus-driven rescan demands the same full stat diff a kernel
            // rescan does.
            let explicit_rescan = pending_full_rescan();
            let event_need_full = event_need_full || explicit_rescan.is_some();

            let mut asset_paths = asset_paths;
            let mut asset_full_paths = asset_full_paths;
            normalize_asset_event_aliases(
                graphs
                    .values()
                    .map(|graph| (graph.root.as_path(), &graph.assets)),
                &mut asset_paths,
                &mut asset_full_paths,
            );

            // Assets are ordinary externally synchronized files, not graph
            // text. Observe only their metadata here and emit one
            // assets-relative cache-invalidation batch; this lane never calls
            // Graph reconciliation.
            // A root the OS refused to watch sends no events, so it is polled
            // like poll mode until a watch succeeds, and a root that becomes
            // watched is diffed once against what it held while unwatched.
            // Both only emitted an error before: nothing in or under it,
            // configuration included, was seen until the next rescan (GH
            // #543, audit R11-04).
            let unwatched =
                |root: &Path| !inotify || !watched.iter().any(|dir| root.starts_with(dir));
            let handed_over = |root: &Path| newly_watched.iter().any(|dir| root.starts_with(dir));
            for (label, graph) in graphs.iter() {
                if unwatched(&graph.root) || handed_over(&graph.root) {
                    config_recheck.insert(label.clone());
                }
            }
            for (label, graph) in graphs.iter_mut() {
                let assets_handed_over = handed_over(&graph.assets.root);
                let assets_polled = unwatched(&graph.assets.root);
                let changed = reconcile_asset_observation(
                    label,
                    &mut graph.assets,
                    &asset_paths,
                    &asset_full_paths,
                    event_need_full || notify_error || assets_handed_over,
                    assets_polled,
                );
                if !changed.is_empty() {
                    if crate::debug::debug_enabled() {
                        crate::debug::diag(format!(
                            "asset observer emitting {} invalidation(s)",
                            changed.len()
                        ));
                    }
                    let _ =
                        app.emit_to(label, "asset-changed", AssetChangedBatch { paths: changed });
                }
            }
            let mut transition_deferred = false;
            for (label, graph) in graphs.iter_mut() {
                if let Some(epoch) = drained_observation_epochs.get(&graph.root).copied() {
                    if graph
                        .graph
                        .owns_graph_text_external_observation_ticket(epoch)
                    {
                        graph.pending_observation_epoch =
                            Some(graph.pending_observation_epoch.map_or(epoch, |pending| {
                                pending.later_for_same_instance(epoch).unwrap_or(epoch)
                            }));
                    }
                }
                let initial_cycle = !graph.baseline;
                if initial_cycle {
                    // No baseline yet, so nothing about the graph's text identity
                    // is known. Once per graph, not once per cycle.
                    let _ = graph
                        .graph
                        .observe_graph_text_external_paths(std::iter::empty::<&Path>(), true);
                    graph.snap = collect_graph_text_files(&graph.graph).files;
                    graph.baseline = true;
                }
                let retry_due = graph.retry.take_due(Instant::now());
                let (full_owned, exact_owned) =
                    unclassified_paths_for_graph(&full_paths, &graph.graph);
                let mut owned = pending_for_graph(&paths, &graph.graph);
                owned.extend(exact_owned);
                let polled = unwatched(&graph.root);
                let rewalk = rewalks_root(initial_cycle, polled, handed_over(&graph.root));
                let need_full = event_need_full
                    || rewalk
                    || !full_owned.is_empty()
                    || retry_due
                    || graph.transition_skipped;
                // Nothing is reconciled into a graph while a load or reopen
                // holds its root's transition lane: a restore rewrites the
                // tree under it, and lowering those files into the graph it
                // is about to retire doubled the work and kept the retiring
                // projection worker busy past its detach bound (GH #543,
                // audit R10-13). Never wait for the lane here -- that would
                // stall every graph behind one -- carry a full diff instead.
                let lane = app
                    .state::<AppState>()
                    .storage_supervisor
                    .transition_lane(&graph.root);
                let _transition = if need_full || !owned.is_empty() {
                    match lane.try_lock() {
                        Ok(guard) => Some(guard),
                        Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                            Some(poisoned.into_inner())
                        }
                        Err(std::sync::TryLockError::WouldBlock) => {
                            graph.transition_skipped = true;
                            transition_deferred = true;
                            continue;
                        }
                    }
                } else {
                    None
                };
                graph.transition_skipped = false;
                let mut cycle_failed = false;
                let mut attempted = false;
                if need_full || !owned.is_empty() {
                    attempted = true;
                    let reconcile_started = Instant::now();
                    let (changes, conflicts_dirty, used_full, errors) =
                        reconcile_pending(&graph.graph, &mut graph.snap, &owned, need_full, polled);
                    let pages = changes.len();
                    if emit_as_bulk(pages) {
                        // One epoch, one notification: the frontend answers with
                        // one dataRev bump and reloads only visible pages,
                        // instead of N events → N bumps → up to N getPage IPCs.
                        let _ =
                            app.emit_to(label, "graph-changed-bulk", GraphChangedBulk { changes });
                    } else {
                        for change in changes {
                            let _ = app.emit_to(label, "graph-changed", change);
                        }
                    }
                    // Receipts only for batches that surfaced something: a
                    // change reaching the frontend, or an error scheduling a
                    // backoff retry (itself a latency source). Quiet cycles —
                    // echo-suppressed self-writes, poll scans that found
                    // nothing — would drown the 64-slot ring in no-ops.
                    if pages > 0 || !errors.is_empty() {
                        record_latency_receipt(latency_receipt(
                            label,
                            !polled,
                            pages,
                            owned.len(),
                            // The branch actually taken — a burst-escalated
                            // batch reads as full_diff in the receipt trail.
                            used_full,
                            errors.len(),
                            if initial_cycle { None } else { first_event_at },
                            reconcile_started,
                            Instant::now(),
                        ));
                    }
                    if !errors.is_empty() {
                        cycle_failed = true;
                        let message = errors.join("; ");
                        if graph.last_reconcile_error.as_deref() != Some(&message) {
                            let _ = app.emit_to(label, "graph-watch-error", &message);
                            graph.last_reconcile_error = Some(message);
                        }
                    }
                    if conflicts_dirty {
                        let _ = app.emit_to(label, "conflicts-changed", ());
                    }
                    super::announce_unreadable_pages(&app, label, &graph.graph);
                }
                if cycle_failed {
                    graph.retry.failed(Instant::now());
                } else if attempted {
                    graph.retry.succeeded();
                    graph.last_reconcile_error = None;
                    if let Some(epoch) = graph.pending_observation_epoch.take() {
                        graph
                            .graph
                            .acknowledge_graph_text_external_observations(epoch);
                    }
                }
            }
            // This is the focus-freshness boundary: every graph lane has
            // finished the requested full pass and all ordinary change events
            // were emitted before this completion marker. The frontend still
            // waits for its asynchronous handlers before admitting edits.
            // Configuration, for every graph this cycle could have touched. A
            // kernel rescan or notify error carries no usable paths, and poll
            // mode has none at all, so both re-check every graph -- one file
            // read and one digest each, against a stat scan they already pay.
            if refresh_changed_configs(
                &app,
                &labels_by_root,
                &config_paths,
                event_need_full || notify_error || !inotify,
                &mut config_recheck,
            ) || transition_deferred
            {
                // A deferral (of a configuration change or of a graph's
                // reconcile) means the lane was busy, not that the change went
                // away. Wake again; the 200 ms coalescing sleep below bounds
                // how fast this can retry while a transition holds the lane.
                let _ = tx.send(());
            }

            if let Some(sequence) = explicit_rescan {
                complete_full_rescan(&app, sequence);
            }

            // --- wait for the next cycle ---
            if inotify && !watched.is_empty() {
                // Block until the kernel reports a change (or a control poke).
                // Coalesce the several events produced by one atomic save.
                let now = Instant::now();
                let retry_wait = graphs
                    .values()
                    .filter_map(|graph| graph.retry.remaining(now))
                    .min();
                let wait_for =
                    inotify_cycle_wait(retry_wait, desired.difference(&watched).next().is_some());
                let woke_for_event = match wait_for {
                    Some(wait) => rx.recv_timeout(wait).is_ok(),
                    None => rx.recv().is_ok(),
                };
                if woke_for_event {
                    std::thread::sleep(Duration::from_millis(200));
                    while rx.try_recv().is_ok() {}
                }
            } else {
                let now = Instant::now();
                let retry_wait = graphs
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
/// tine-settings.json): "inotify" (the default) → a real OS watcher, no idle
/// wakeups; "poll" → a 3s mtime scan for filesystems where native events are
/// unavailable or unreliable. Both feed the same full-diff reconciliation.
pub(super) fn watch_mode(app: &tauri::AppHandle) -> String {
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("watch_mode")
                .and_then(|x| x.as_str().map(String::from))
        })
        .filter(|m| m == "poll" || m == "inotify")
        .unwrap_or_else(|| default_watch_mode().to_string())
}

pub(super) fn default_watch_mode() -> &'static str {
    "inotify"
}
