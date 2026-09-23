use crate::command_error::CommandError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tauri::ipc::{CommandArg, CommandItem, InvokeBody, InvokeError};
use tauri::{Emitter, Manager, Runtime, State, WebviewWindow};
use tine_core::model::Graph;

pub(crate) type WindowKey = String;
static NEXT_BINDING: AtomicU64 = AtomicU64::new(1);

/// The current graph binding's page-write admission, as the frontend wire
/// record `{ binding_generation }`. Its presence admits page writes; the
/// generation fences a queued mutation against a window that was rebound.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ApplicationPageAdmission {
    pub(crate) binding_generation: u64,
}

/// Read-only graph lease used by the auxiliary Quick Capture WebView. Capture
/// deliberately does not own a graph slot: the registry permits one writable
/// window per graph root, while this surface only needs the selected graph's
/// query/read commands before it hands writes back to the owning window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CaptureGraphBinding {
    pub(crate) target: WindowKey,
    pub(crate) binding_generation: u64,
}

#[derive(Debug)]
pub(crate) enum AssetStreamError {
    InvalidAsset,
}

pub(crate) struct GraphSlot {
    graph: Arc<Graph>,
    /// The configuration snapshot `load_graph` hands the frontend. Replaceable
    /// so a settings change can update it in place: a config refresh publishes
    /// a whole replacement slot after a reopen, and without this a window would
    /// report the settings the graph opened with the next time it loads the
    /// same root.
    graph_meta: RwLock<tine_core::model::GraphMeta>,
    pub(crate) root_key: PathBuf,
    /// Unique lease for this exact window→graph binding. Frontend mutations carry
    /// it so an IPC queued before an in-place graph switch cannot execute against
    /// the replacement graph after the window label is rebound.
    pub(crate) binding_generation: u64,
    pub(crate) warm_done: AtomicBool,
    pub(crate) warm_generation: AtomicU64,
    /// Revoked as soon as this exact window→graph binding is replaced/removed.
    /// Detached warm/backup workers check it before and during graph-sized work.
    pub(crate) background_cancelled: Arc<AtomicBool>,
}

impl GraphSlot {
    pub(crate) fn new(graph: Graph, root_key: PathBuf) -> Self {
        let graph_meta = graph.meta();
        Self {
            graph: Arc::new(graph),
            graph_meta: RwLock::new(graph_meta),
            root_key,
            binding_generation: NEXT_BINDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            warm_done: AtomicBool::new(false),
            warm_generation: AtomicU64::new(0),
            background_cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// This binding's graph. The slot owns exactly one graph for its whole
    /// life, so the handle is infallible; callers that outlive the command
    /// (watchers, warm and backup workers) keep the returned `Arc`.
    pub(crate) fn graph(&self) -> Arc<Graph> {
        Arc::clone(&self.graph)
    }

    /// Report the selected save route for this exact graph binding.
    pub(crate) fn application_page_admission(&self) -> ApplicationPageAdmission {
        ApplicationPageAdmission {
            binding_generation: self.binding_generation,
        }
    }

    /// Resolve one range-streamed asset without borrowing graph-text write
    /// authority. Containment is the one canonical `Graph::stream_asset_path`
    /// implementation.
    pub(crate) fn asset_stream_path(&self, name: &str) -> Result<PathBuf, AssetStreamError> {
        self.graph
            .stream_asset_path(name)
            .map_err(|_| AssetStreamError::InvalidAsset)
    }

    /// Persist a change to `logseq/config.edn`.
    ///
    /// Settings use the short-lived filesystem capability, enforced in
    /// `tine-core` by `Graph::ensure_config_write_target`, without graph-text
    /// authority.
    fn with_config_graph<T>(
        &self,
        f: impl FnOnce(&Graph) -> Result<T, CommandError>,
    ) -> Result<T, CommandError> {
        self.with_filesystem_graph(f)
    }

    /// Move something into (or clear) the recoverable trash.
    ///
    /// Trashing a page, journal or conflict copy is a graph-text deletion and
    /// is admitted inside `tine-core`, at `Graph::admit_graph_text_writer`.
    pub(crate) fn with_trash_graph<T>(
        &self,
        f: impl FnOnce(&Graph) -> Result<T, CommandError>,
    ) -> Result<T, CommandError> {
        self.with_filesystem_graph(f)
    }

    /// Run one point-addressed filesystem/config/asset operation.
    pub(crate) fn with_filesystem_graph<T>(
        &self,
        f: impl FnOnce(&Graph) -> Result<T, CommandError>,
    ) -> Result<T, CommandError> {
        f(self.graph.as_ref())
    }

    pub(crate) fn graph_meta(&self) -> tine_core::model::GraphMeta {
        self.graph_meta.read().unwrap().clone()
    }

    /// Write `config.edn`, then take the change in as far as it reaches. The
    /// one way a command writes configuration: whether the graph must be
    /// reopened is [`tine_core::config::Config::reach`]'s answer, never the
    /// command's. Reopening retires the index worker and starts the launch
    /// check again, which on a first launch threw away the whole index build
    /// for a toggle (GH #543, IT-06 and R2-P1).
    ///
    /// Private to this module: a command reaches it through the free
    /// [`apply_config_write`], which also reopens the graph for a change that
    /// reaches it.
    fn apply_config_write(
        &self,
        write: impl FnOnce(&Graph) -> std::io::Result<()>,
    ) -> Result<(), CommandError> {
        self.with_config_graph(|graph| write(graph).map_err(CommandError::from))?;
        // The core took the change in as far as it reaches (`write_config`).
        *self.graph_meta.write().unwrap() = self.graph.meta();
        Ok(())
    }

    /// Whether `config.edn` holds bytes this graph has not taken in: a change
    /// that reaches the graph, which only a reopen takes in, or an outside
    /// edit nothing has read yet.
    pub(crate) fn config_pending(&self) -> bool {
        self.graph.served_config_description()
            != tine_core::model::config_file_description(&self.root_key)
    }

    /// Take in `config.edn` as far as the change reaches settings, and give
    /// the frontend the new meta. A change that reaches the graph leaves this
    /// slot as it is; only a new `Graph` can take it in.
    pub(crate) fn take_in_config(&self) -> tine_core::config::ConfigReach {
        let reach = self.graph.take_in_config();
        if reach == tine_core::config::ConfigReach::Settings {
            *self.graph_meta.write().unwrap() = self.graph.meta();
        }
        reach
    }

    /// Re-open the graph object for the same window/root without revoking the
    /// frontend's lease. A binding generation identifies a window -> graph-root
    /// assignment, not the particular in-memory `Graph` instance. Minting a new
    /// generation here made every later command from that window stale after a
    /// config refresh, including autosaves.
    pub(crate) fn refreshed(graph: Graph, old: &GraphSlot) -> Self {
        let graph_meta = graph.meta();
        Self {
            graph: Arc::new(graph),
            graph_meta: RwLock::new(graph_meta),
            root_key: old.root_key.clone(),
            binding_generation: old.binding_generation,
            warm_done: AtomicBool::new(old.warm_done.load(std::sync::atomic::Ordering::Acquire)),
            warm_generation: AtomicU64::new(
                old.warm_generation
                    .load(std::sync::atomic::Ordering::Acquire),
            ),
            background_cancelled: Arc::clone(&old.background_cancelled),
        }
    }
}

#[derive(Default)]
pub(crate) struct GraphRegistry {
    by_window: HashMap<WindowKey, Arc<GraphSlot>>,
    by_root: HashMap<PathBuf, WindowKey>,
}

impl GraphRegistry {
    pub(crate) fn slot(&self, window: &str) -> Option<Arc<GraphSlot>> {
        self.by_window.get(window).cloned()
    }

    pub(crate) fn owner(&self, root: &Path) -> Option<WindowKey> {
        self.by_root.get(root).cloned()
    }

    pub(crate) fn entries(&self) -> Vec<(WindowKey, Arc<GraphSlot>)> {
        self.by_window
            .iter()
            .map(|(window, slot)| (window.clone(), slot.clone()))
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.by_window.len()
    }

    pub(crate) fn bind(
        &mut self,
        window: WindowKey,
        slot: Arc<GraphSlot>,
    ) -> Result<(), CommandError> {
        for (root, owner) in &self.by_root {
            if owner != &window
                && (root.starts_with(&slot.root_key) || slot.root_key.starts_with(root))
            {
                let refused = CommandError::prose(format!(
                    "graph {} overlaps graph {} already owned by window {owner}",
                    slot.root_key.display(),
                    root.display()
                ));
                retire_slot(&slot);
                return Err(refused);
            }
        }
        if let Some(old) = self.by_window.insert(window.clone(), slot.clone()) {
            // A same-root refresh replaces only the in-memory Graph object and
            // preserves the frontend binding lease. Let its already-running
            // warm/backup finish; a real graph switch revokes the old source.
            if old.binding_generation != slot.binding_generation || old.root_key != slot.root_key {
                retire_slot(&old);
            }
            self.by_root.remove(&old.root_key);
        }
        self.by_root.insert(slot.root_key.clone(), window);
        Ok(())
    }

    /// Swap a refreshed slot in for `expected`, the slot the refresh reopened,
    /// if the window still holds it; returns whether it did. A window that
    /// moved on meanwhile keeps its graph: whatever replaced `expected`
    /// already retired it, and binding the refresh over it would reopen the
    /// old root in a window that has left it.
    ///
    /// A slot handed to the registry is bound or retired, never dropped: a
    /// refused slot already has its projection attached, and dropping it
    /// unretired left that worker holding the projection's writer lease
    /// (GH #543, audit R10-12). `bind` does the same for a slot it refuses,
    /// and retires the slot it displaces.
    pub(crate) fn swap_refreshed(
        &mut self,
        window: &str,
        expected: &Arc<GraphSlot>,
        slot: Arc<GraphSlot>,
    ) -> bool {
        match self.by_window.get_mut(window) {
            Some(current) if Arc::ptr_eq(current, expected) => {
                // The retired slot's warm parses a graph nothing will read
                // again; moving its generation stops it between pages.
                expected
                    .warm_generation
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                *current = slot;
                true
            }
            _ => {
                retire_slot(&slot);
                false
            }
        }
    }

    pub(crate) fn remove(&mut self, window: &str) -> Option<Arc<GraphSlot>> {
        let slot = self.by_window.remove(window)?;
        retire_slot(&slot);
        self.by_root.remove(&slot.root_key);
        Some(slot)
    }
}

/// Retire a slot a window has left -- a graph switch or a window close: its
/// background work and display reads are revoked, and its index writer is
/// stopped now, not when the last reference to its graph drops. The writer
/// holds the database's lease, which reopening the graph needs; a close that
/// skipped this left a reopen waiting for the lease until every clone of the
/// old graph was gone (GH #543, audit R7-05). The detach runs off the calling
/// thread, which holds the registry lock.
fn retire_slot(slot: &Arc<GraphSlot>) {
    slot.background_cancelled
        .store(true, std::sync::atomic::Ordering::Release);
    slot.graph().retire();
    let displaced = slot.graph();
    std::thread::spawn(move || {
        if !displaced.detach_direct_projection(Duration::from_secs(15)) {
            crate::debug::diag(
                "Direct Files projection worker of a retired graph did not stop within 15 s"
                    .to_string(),
            );
        }
    });
}

#[derive(Default)]
pub(crate) struct CaptureShow {
    generation: u64,
    pending: bool,
    binding: Option<CaptureGraphBinding>,
}

impl CaptureShow {
    fn begin(&mut self) -> u64 {
        self.generation += 1;
        self.pending = true;
        self.binding = None;
        self.generation
    }

    fn complete(&mut self, generation: u64, binding: CaptureGraphBinding) -> bool {
        if self.generation != generation || !self.pending {
            return false;
        }
        self.pending = false;
        self.binding = Some(binding);
        true
    }
}

pub(crate) struct AppState {
    pub(crate) graphs: RwLock<GraphRegistry>,
    /// Sole owner of serialized open/switch/storage-mode transitions and their
    /// typed native operation model.
    pub(crate) storage_supervisor:
        crate::storage_transition_supervisor::StorageTransitionSupervisor,
    pub(crate) watch_ctl: Mutex<Option<Sender<()>>>,
    pub(crate) last_focused: Mutex<Option<WindowKey>>,
    pub(crate) capture_graph: Mutex<CaptureShow>,
    #[cfg(desktop)]
    pub(crate) next_window: AtomicU64,
}

impl AppState {
    /// Record the graph window that commands such as quick capture should use.
    ///
    /// Explicit graph activation must update this state synchronously: some
    /// headless window managers, and occasionally desktop focus hand-offs, do
    /// not deliver a later `WindowEvent::Focused` even when `set_focus` was
    /// requested successfully.
    pub(crate) fn note_focused(&self, label: &str) -> bool {
        let mut last = self.last_focused.lock().unwrap();
        if last.as_deref() == Some(label) {
            false
        } else {
            *last = Some(label.to_string());
            true
        }
    }

    /// Atomically publish the graph snapshot selected for the next Quick
    /// Capture show. The capture WebView must present this exact generation on
    /// every graph-scoped invoke; a later show, graph switch, or close makes
    /// older requests stale rather than letting them read another graph.
    #[cfg(test)]
    pub(crate) fn bind_capture_graph(&self, target: WindowKey, binding_generation: u64) {
        let generation = self.begin_capture_show();
        assert!(self.complete_capture_show(generation, target, binding_generation));
    }

    pub(crate) fn begin_capture_show(&self) -> u64 {
        self.capture_graph.lock().unwrap().begin()
    }

    pub(crate) fn pending_capture_show(&self) -> Option<u64> {
        let show = self.capture_graph.lock().unwrap();
        show.pending.then_some(show.generation)
    }

    pub(crate) fn complete_capture_show(
        &self,
        generation: u64,
        target: WindowKey,
        binding_generation: u64,
    ) -> bool {
        self.capture_graph.lock().unwrap().complete(
            generation,
            CaptureGraphBinding {
                target,
                binding_generation,
            },
        )
    }

    pub(crate) fn capture_show_is_current(&self, generation: u64) -> bool {
        let show = self.capture_graph.lock().unwrap();
        show.generation == generation && show.binding.is_some()
    }

    pub(crate) fn bound_capture_show(&self) -> Option<u64> {
        let show = self.capture_graph.lock().unwrap();
        show.binding.as_ref().map(|_| show.generation)
    }

    pub(crate) fn capture_graph_binding(&self) -> Option<CaptureGraphBinding> {
        self.capture_graph.lock().unwrap().binding.clone()
    }
}

pub(crate) struct GraphContext<'a, R: Runtime = tauri::Wry> {
    pub(crate) state: State<'a, AppState>,
    pub(crate) window: WebviewWindow<R>,
    pub(crate) binding_generation: Option<u64>,
}

pub(crate) fn owned_graph_context(
    state: GraphContext<'_>,
) -> Result<(tauri::AppHandle, String, u64), CommandError> {
    let app = state.window.app_handle().clone();
    let label = state.window.label().to_string();
    let binding_generation = state
        .binding_generation
        .ok_or_else(|| CommandError::prose("missing-graph-binding"))?;
    drop(state);
    Ok((app, label, binding_generation))
}

impl<'r, 'de: 'r, R: Runtime> CommandArg<'de, R> for GraphContext<'r, R> {
    fn from_command(command: CommandItem<'de, R>) -> Result<Self, InvokeError> {
        let binding_generation = match command.message.payload() {
            InvokeBody::Json(value) => value
                .get("bindingGeneration")
                .or_else(|| value.get("binding_generation"))
                .and_then(|v| v.as_u64()),
            InvokeBody::Raw(_) => None,
        };
        let state: State<'r, AppState> = command
            .message
            .state_ref()
            .try_get()
            .ok_or_else(|| InvokeError::from("AppState is not managed"))?;
        let window = WebviewWindow::<R>::from_command(command)?;
        Ok(Self {
            state,
            window,
            binding_generation,
        })
    }
}

pub(crate) fn canonical_graph_root(path: &str) -> Result<PathBuf, CommandError> {
    // Not `fs::canonicalize` directly: an encrypted-volume driver may refuse to
    // answer it at all, which used to make every graph on that drive
    // unopenable (GH #561).
    let root = tine_core::directory_identity::canonical_existing_path(Path::new(path)).map_err(
        |error| CommandError::coded("couldn't resolve graph path", format!("{path}: {error}")),
    )?;
    if !root.is_dir() {
        return Err(CommandError::prose(format!(
            "graph path is not a folder: {}",
            root.display()
        )));
    }
    Ok(root)
}

pub(crate) fn slot_for_window(
    state: &AppState,
    window: &str,
) -> Result<Arc<GraphSlot>, CommandError> {
    state
        .graphs
        .read()
        .unwrap()
        .slot(window)
        .ok_or_else(|| CommandError::prose(format!("no graph loaded for window {window}")))
}

pub(crate) fn slot_for_context(ctx: &GraphContext<'_>) -> Result<Arc<GraphSlot>, CommandError> {
    slot_for_bound_window(&ctx.state, ctx.window.label(), ctx.binding_generation)
}

/// Resolve a normal graph-window command. Quick Capture intentionally has no
/// graph slot, so this path cannot be used to grant it any GraphContext command
/// (including save, delete, trash, or other mutations).
pub(crate) fn slot_for_bound_window(
    state: &AppState,
    window: &str,
    binding_generation: Option<u64>,
) -> Result<Arc<GraphSlot>, CommandError> {
    let slot = slot_for_window(state, window)?;
    let generation =
        binding_generation.ok_or_else(|| CommandError::prose("missing-graph-binding"))?;
    if generation != slot.binding_generation {
        return Err(CommandError::prose("stale-graph-binding"));
    }
    Ok(slot)
}

/// Answer a display-only read (a listing, aliases, icons, counts) from the
/// window's current graph. A read that was running on a graph the app has
/// since replaced returns no answer rather than parse that retired graph, and
/// is asked again of its replacement (GH #543). After a real graph switch the
/// binding no longer matches and the request ends as stale, as before.
pub(crate) fn display_read<T>(
    state: &AppState,
    window: &str,
    binding_generation: u64,
    read: impl Fn(&Graph) -> T,
) -> Result<T, CommandError> {
    display_read_from(
        || slot_for_bound_window(state, window, Some(binding_generation)),
        read,
    )
}

/// [`display_read`] for Quick Capture, whose graph is the one it was bound to
/// rather than its own window's.
pub(crate) fn capture_display_read<T>(
    state: &AppState,
    caller: &str,
    binding_generation: Option<u64>,
    read: impl Fn(&Graph) -> T,
) -> Result<T, CommandError> {
    display_read_from(
        || capture_quick_switch_slot(state, caller, binding_generation),
        read,
    )
}

fn display_read_from<T>(
    slot: impl Fn() -> Result<Arc<GraphSlot>, CommandError>,
    read: impl Fn(&Graph) -> T,
) -> Result<T, CommandError> {
    // A refresh retires the old graph before it binds the replacement; the
    // bound limits the wait if that bind never comes.
    for _ in 0..500 {
        let graph = slot()?.graph();
        if let Some(answer) = graph.display_read(|| read(&graph)) {
            return Ok(answer);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(CommandError::graph(
        "the graph was replaced and its replacement was not bound in time",
    ))
}

/// Resolve the only graph capability granted to the capture WebView: a bounded
/// page/tag quick-switch query. This is deliberately not a GraphContext route;
/// capture retains no generic read or write access to the selected graph.
pub(crate) fn capture_quick_switch_slot(
    state: &AppState,
    caller: &str,
    binding_generation: Option<u64>,
) -> Result<Arc<GraphSlot>, CommandError> {
    if caller != "capture" {
        return Err(CommandError::prose(
            "capture quick switch is only available to quick capture",
        ));
    }
    let capture = state
        .capture_graph_binding()
        .ok_or_else(|| CommandError::prose("no graph bound for quick capture"))?;
    let generation =
        binding_generation.ok_or_else(|| CommandError::prose("missing-graph-binding"))?;
    if generation != capture.binding_generation {
        return Err(CommandError::prose("stale-graph-binding"));
    }
    let slot = slot_for_window(state, &capture.target)?;
    if slot.binding_generation != capture.binding_generation {
        return Err(CommandError::prose("stale-graph-binding"));
    }
    Ok(slot)
}

/// Run one non-graph-semantic filesystem/config/asset operation.
pub(crate) fn with_filesystem_graph<T>(
    ctx: &GraphContext<'_>,
    f: impl FnOnce(&Graph) -> Result<T, CommandError>,
) -> Result<T, CommandError> {
    slot_for_context(ctx)?.with_filesystem_graph(f)
}

/// Run a recoverable-trash write under either authority. See
/// [`GraphSlot::with_trash_graph`] for what it does and does not cover.
pub(crate) fn with_trash_graph<T>(
    ctx: &GraphContext<'_>,
    f: impl FnOnce(&Graph) -> Result<T, CommandError>,
) -> Result<T, CommandError> {
    slot_for_context(ctx)?.with_trash_graph(f)
}

/// How a refresh should behave when another operation holds the storage
/// transition lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefreshLaneWait {
    /// A user-initiated settings change: wait for the lane.
    Block,
    /// A watcher cycle: never block. Blocking here would stall the reconcile
    /// loop for *every* graph behind one graph's load or storage promotion, so
    /// a busy lane reports `Deferred` and the next cycle tries again -- the
    /// on-disk configuration is still there, so nothing is lost by waiting.
    TryOnce,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefreshOutcome {
    Refreshed,
    /// Nothing to reopen: the graph already serves what is on disk.
    Current,
    Deferred,
}

/// Write `config.edn` for a settings command and take the change in.
///
/// The one way a command writes configuration. How far the change reaches is
/// [`tine_core::config::Config::reach`]'s answer, never the command's: a
/// settings change is taken in by the bound graph, and one that reaches the
/// graph is taken in by [`take_in_config_change`], the same decider the
/// watcher uses. The command does not leave that to the watcher, which may
/// not be running: a failed OS watcher left a journal-title-format change
/// untaken for the session (GH #543, audit R10-07). The reopen runs off the
/// main thread; the window learns of it from `graph-rebound`. If the watcher
/// sees the write too, whichever takes the lane second finds the graph
/// current, so one change reopens once.
pub(crate) fn apply_config_write(
    ctx: &GraphContext<'_>,
    write: impl FnOnce(&Graph) -> std::io::Result<()>,
) -> Result<(), CommandError> {
    let slot = slot_for_context(ctx)?;
    slot.apply_config_write(write)?;
    if slot.config_pending() {
        spawn_config_decider(ctx.window.app_handle(), ctx.window.label());
    }
    Ok(())
}

/// Run [`take_in_config_change`] for `label` off the calling thread, waiting
/// for the transition lane: a change only a reopen takes in.
fn spawn_config_decider(app: &tauri::AppHandle, label: &str) {
    let app = app.clone();
    let label = label.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        if let Err(error) = take_in_config_change(&state, &app, &label, RefreshLaneWait::Block) {
            let _ = app.emit_to(&label, "graph-watch-error", error.to_string());
        }
    });
}

/// Make a slot just handed to a window serve what `config.edn` holds now.
/// Called wherever a window is given a graph: an open's bind, a reopen's
/// swap, and a same-root load answering `AlreadyCurrent`.
///
/// A graph reads its configuration when it is opened, and a change that
/// arrives before the window holds it is taken in by whichever graph the
/// window held then: a settings command writes through the slot being
/// replaced, and the watcher takes an outside edit into it. The replacement
/// then served the older value, nothing asked again, and the frontend, which
/// writes `:favorites` as a whole list, dropped a favorite from disk at the
/// next toggle (GH #543, audit R11-03). A change to settings is taken in
/// here; one that reaches the graph goes to the one decider.
pub(crate) fn serve_disk_config(app: &tauri::AppHandle, label: &str, slot: &GraphSlot) {
    let before = slot.graph_meta();
    match take_in_config_now(slot) {
        ConfigTakeIn::Current => {
            let after = slot.graph_meta();
            if after != before {
                let _ = app.emit_to(label, "graph-config-changed", after);
            }
        }
        ConfigTakeIn::Reopen => spawn_config_decider(app, label),
    }
}

/// Rewrite a window's graph on disk and reopen it, for a command. Async so
/// that no sync command can call it: a refresh waits for the storage
/// transition lane, walks the graph and waits up to 15 s for the index worker
/// to stop, and a sync command does all of that on the main thread, freezing
/// every window (GH #543, R6-02).
///
/// `rewrite` runs under the transition lane, before the reopen. The watcher's
/// configuration reopen takes the same lane without waiting, so while the
/// command rewrites it defers, and afterwards it finds the configuration
/// already taken in by the reopened graph. One change, one reopen: a rewrite
/// outside the lane let the watcher reopen the graph and the command reopen
/// it again, restarting indexing twice (GH #543, audit R9-15b).
pub(crate) async fn refresh_graph(
    app: tauri::AppHandle,
    label: String,
    rewrite: impl FnOnce() -> Result<(), CommandError> + Send + 'static,
) -> Result<(), CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        refresh_graph_for_label(&state, &app, &label, RefreshLaneWait::Block, |_| {
            rewrite().map(|()| true)
        })
        .map(|_| ())
    })
    .await
    .map_err(CommandError::worker)?
}

/// How far a configuration change on disk reaches, for a bound graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigTakeIn {
    /// The graph serves what is on disk, or took the change in itself.
    Current,
    /// Only a new `Graph` can take the change in.
    Reopen,
}

/// Take in what `config.edn` holds as far as this graph can: the decision
/// behind [`take_in_config_change`], without an app so it is testable.
pub(crate) fn take_in_config_now(slot: &GraphSlot) -> ConfigTakeIn {
    // The graph knows which bytes its configuration was taken from, so
    // Tine's own settings write (already taken in) and a redelivery of
    // identical bytes cost nothing here. Anything it has not taken in --
    // including an outside change folded into Tine's own write, or an
    // outside revert to the bytes it was opened with -- differs.
    if !slot.config_pending() {
        return ConfigTakeIn::Current;
    }
    // A change to settings only is taken in by this graph; reopening it
    // would restart indexing for an edit to, say, favorites (GH #543).
    match slot.take_in_config() {
        tine_core::config::ConfigReach::Unchanged | tine_core::config::ConfigReach::Settings => {
            ConfigTakeIn::Current
        }
        tine_core::config::ConfigReach::Graph => ConfigTakeIn::Reopen,
    }
}

/// Take a window's configuration change in, reopening its graph when the
/// change reaches the graph, and tell the window what changed. The one
/// decider for both a settings command ([`apply_config_write`]) and the
/// watcher (GH #543, audit R10-07).
///
/// The decision is made again under the transition lane, on the graph bound
/// then, so a second caller for the same change finds it taken in.
///
/// A reopen replaces the `Graph` the window's in-flight results and page
/// inventory belong to, so it is announced as `graph-rebound`; without it a
/// `:hidden` change left All Pages and page links on the old page set (GH
/// #543, audit R9-13). A changed setting is announced as
/// `graph-config-changed`. `refresh_graph_for_label` is private to this module
/// so this and [`refresh_graph`] are the only ways to reopen a graph.
pub(crate) fn take_in_config_change(
    state: &AppState,
    app: &tauri::AppHandle,
    label: &str,
    wait: RefreshLaneWait,
) -> Result<RefreshOutcome, CommandError> {
    let slot = slot_for_window(state, label)?;
    let before = slot.graph_meta();
    // Outside the lane first: the watcher asks on every poll cycle, and a
    // graph that is current must not contend for (or defer on) the lane.
    let outcome = if take_in_config_now(&slot) == ConfigTakeIn::Current {
        RefreshOutcome::Current
    } else {
        drop(slot);
        refresh_graph_for_label(state, app, label, wait, |bound| {
            Ok(take_in_config_now(bound) == ConfigTakeIn::Reopen)
        })?
    };
    if outcome == RefreshOutcome::Refreshed {
        let _ = app.emit_to(label, "graph-rebound", ());
    }
    if outcome != RefreshOutcome::Deferred {
        let after = slot_for_window(state, label)?.graph_meta();
        // A rewrite that changed no setting we surface -- Logseq touching an
        // unrelated key, Syncthing redelivering identical bytes with a new
        // mtime -- announces nothing.
        if after != before {
            let _ = app.emit_to(label, "graph-config-changed", after);
        }
    }
    Ok(outcome)
}

/// Reopen one window's graph without a `GraphContext`: the shared body of
/// [`refresh_graph`] and [`take_in_config_change`]. `decide` runs under the
/// transition lane on the graph bound then, and says whether to reopen it; a
/// command's rewrite happens inside it.
fn refresh_graph_for_label(
    state: &AppState,
    app: &tauri::AppHandle,
    label: &str,
    wait: RefreshLaneWait,
    decide: impl FnOnce(&GraphSlot) -> Result<bool, CommandError>,
) -> Result<RefreshOutcome, CommandError> {
    let label = label.to_string();
    // Refresh may migrate graph files before publishing its replacement slot.
    // Serialize the whole operation with graph loads.
    let root_hint = slot_for_window(state, &label)?.root_key.clone();
    let transition_gate = state.storage_supervisor.transition_lane(&root_hint);
    let _transition = match wait {
        RefreshLaneWait::Block => transition_gate.lock().unwrap(),
        RefreshLaneWait::TryOnce => match transition_gate.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(RefreshOutcome::Deferred),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        },
    };
    let old = slot_for_window(state, &label)?;
    if old.root_key != root_hint {
        return Err(CommandError::prose(
            "graph changed while refresh waited for its transition lane",
        ));
    }
    if !decide(&old)? {
        return Ok(RefreshOutcome::Current);
    }
    let approved = crate::settings::approved_external_assets(app, &old.root_key);
    let services = crate::graph::direct_files_service_paths(app, &old.root_key);
    let prepared = prepare_legacy_refresh(&old, approved.as_deref(), services)?;
    let replacement = Arc::new(prepared.commit(&old));
    // The reopened graph starts with a cold parsed cache, and the projection
    // attached above takes its full payload from the warm — exactly as the
    // open path does (`publish_prepared_direct_files`).
    let warm = crate::graph::begin_warm_cache(&replacement);
    if !state
        .graphs
        .write()
        .unwrap()
        .swap_refreshed(&label, &old, Arc::clone(&replacement))
    {
        // The window closed or moved to another graph meanwhile. What the
        // caller asked for is done on disk; there is nothing left to reopen.
        return Ok(RefreshOutcome::Current);
    }
    serve_disk_config(app, &label, &replacement);
    crate::graph::warm_cache_async(app.clone(), label, replacement, warm)?;
    poke_watcher(state);
    Ok(RefreshOutcome::Refreshed)
}

/// A configuration refresh with its replacement graph opened, not yet swapped
/// in.
///
/// A refresh has two steps so that nothing can fail after the bound graph is
/// retired. [`prepare_legacy_refresh`] does all the fallible work, reopening
/// and validating the root, and leaves the bound graph serving;
/// [`PreparedRefresh::commit`] cannot fail. A retired graph never answers a
/// display read again, so retiring it before a step that can still fail left
/// the window bound to a graph that answers nothing (GH #543, indexing audit
/// R2-03). Both steps are app-handle-free so a refreshed graph's services are
/// testable without a Tauri app.
pub(crate) struct PreparedRefresh {
    graph: Graph,
    services: crate::graph::DirectFilesServicePaths,
}

pub(crate) fn prepare_legacy_refresh(
    old: &GraphSlot,
    approved_assets: Option<&Path>,
    services: crate::graph::DirectFilesServicePaths,
) -> Result<PreparedRefresh, CommandError> {
    // Concord invariant 4: a refresh re-reads configuration, it does not rewrite
    // the tree. Journal filename repairs are proposed and applied explicitly
    // (`apply_journal_filename_migrations`) — a settings change must not rename
    // the user's files as a side effect.
    let graph = Graph::open_checked_with_assets(&old.root_key, approved_assets)?;
    Ok(PreparedRefresh { graph, services })
}

impl PreparedRefresh {
    /// Retire the bound graph, move the projection over, and build the
    /// replacement slot.
    pub(crate) fn commit(self, old: &GraphSlot) -> GraphSlot {
        let old_graph = old.graph();
        // First, so a display read on the old graph stops rather than falling
        // back to a whole-graph parse once its projection is gone.
        old_graph.retire();
        // The replacement attaches a projection at the SAME path; the old
        // worker must have released the writer lease first or the new one
        // races it.
        if !old_graph.detach_direct_projection(Duration::from_secs(15)) {
            crate::debug::diag(
                "Direct Files projection worker did not stop within 15 s before a refresh; \
                 the replacement attach may find its database busy"
                    .to_string(),
            );
        }
        drop(old_graph);
        crate::graph::attach_direct_files_services(&self.graph, self.services);
        GraphSlot::refreshed(self.graph, old)
    }
}

pub(crate) fn poke_watcher(state: &AppState) {
    if let Some(tx) = state.watch_ctl.lock().unwrap().as_ref() {
        let _ = tx.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::rust_module_source;
    use std::time::Instant;

    /// GH #543, audit R9-15b: one change, one reopen. A command's rewrite
    /// runs under the transition lane, before the reopen, so the watcher's
    /// configuration reopen (which only tries the lane) defers during it and
    /// then finds the change taken in. `restore_backup` is the one command
    /// that rewrites and reopens; its rewrite must be that closure, not a
    /// write before the call.
    #[test]
    fn a_command_rewrite_runs_under_the_lane_its_reopen_holds() {
        let state = include_str!("state.rs");
        let entry = &state[state
            .find("\nfn refresh_graph_for_label")
            .expect("refresh entry")..];
        let entry = &entry[..entry.find("\n}\n").expect("refresh entry ends")];
        let lane = entry
            .find("transition_gate.lock()")
            .expect("refresh takes the lane");
        let rewrite = entry
            .find("decide(&old)?")
            .expect("refresh runs the decision");
        let reopen = entry
            .find("prepare_legacy_refresh(")
            .expect("refresh reopens");
        assert!(
            lane < rewrite && rewrite < reopen,
            "the rewrite runs after the lane is held and before the reopen"
        );
        let backup = include_str!("backup.rs");
        let restore = &backup[backup
            .find("pub(crate) async fn restore_backup(")
            .expect("restore command")..];
        let restore = &restore[..restore.find("\n}\n").expect("restore ends")];
        let reopen = restore
            .find("refresh_graph(")
            .expect("restore reopens the graph");
        let rewrite = restore
            .find("restore_from_backup_source(")
            .expect("restore rewrites the graph");
        assert!(
            reopen < rewrite,
            "restore_backup rewrites the graph inside refresh_graph's rewrite, under \
             the lane; a rewrite before the call lets the watcher reopen too"
        );
    }

    /// A reload that fails leaves the bound graph serving. It used to retire
    /// the graph before reopening the root, so a transient failure left the
    /// window bound to a graph that answered no display read again (GH #543,
    /// indexing audit R2-03).
    #[test]
    fn a_failed_refresh_leaves_the_bound_graph_serving() {
        let root =
            std::env::temp_dir().join(format!("gh543-failed-refresh-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/Alpha.md"), "- alpha\n").unwrap();
        let old = Arc::new(GraphSlot::new(
            Graph::open_checked(&root).unwrap(),
            root.clone(),
        ));
        let mut registry = GraphRegistry::default();
        registry.bind("main".into(), Arc::clone(&old)).unwrap();
        // A transient invalid asset path makes the checked reopen fail.
        std::fs::write(root.join("assets"), "temporarily not a directory").unwrap();
        let prepared = prepare_legacy_refresh(
            &old,
            None,
            crate::graph::DirectFilesServicePaths {
                projection: Ok(root.join("private/projection.sqlite")),
                concord_ledger: None,
            },
        );
        assert!(prepared.is_err(), "the fixture's reopen must fail");
        std::fs::remove_file(root.join("assets")).unwrap();
        let current = registry.slot("main").unwrap();
        let answer = current.graph().display_read(|| 42);
        let _ = std::fs::remove_dir_all(root);
        assert!(Arc::ptr_eq(&current, &old));
        assert_eq!(
            answer,
            Some(42),
            "a failed refresh retired the graph still bound to the window"
        );
    }

    /// A refresh finishing after its window moved to another graph must not
    /// bind the reopened root over it.
    #[test]
    fn a_refresh_does_not_swap_over_a_window_that_moved_on() {
        let base =
            std::env::temp_dir().join(format!("gh543-refresh-swap-{}", uuid::Uuid::new_v4()));
        let (a, b) = (base.join("a"), base.join("b"));
        let mut registry = GraphRegistry::default();
        let old = graph(&a);
        registry.bind("main".into(), Arc::clone(&old)).unwrap();
        let switched = graph(&b);
        registry.bind("main".into(), Arc::clone(&switched)).unwrap();
        let refreshed = Arc::new(GraphSlot::refreshed(Graph::open(&a), &old));
        assert!(!registry.swap_refreshed("main", &old, Arc::clone(&refreshed)));
        assert!(Arc::ptr_eq(&registry.slot("main").unwrap(), &switched));
        assert_eq!(registry.owner(&b).as_deref(), Some("main"));
        // GH #543, audit R10-12: the reopened graph nothing binds is retired,
        // as a closed window's is, not dropped with its projection attached.
        assert!(
            refreshed.graph().is_retired(),
            "a reopened graph no window binds was left serving"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// GH #543, audit R10-12: a slot `bind` refuses (its root overlaps one
    /// another window owns) is retired, not dropped with its projection
    /// attached.
    #[test]
    fn a_slot_the_registry_refuses_is_retired() {
        let base =
            std::env::temp_dir().join(format!("gh543-refused-bind-{}", uuid::Uuid::new_v4()));
        let mut registry = GraphRegistry::default();
        let owner = graph(&base);
        registry.bind("main".into(), Arc::clone(&owner)).unwrap();
        let nested = graph(&base.join("nested"));
        assert!(registry.bind("second".into(), Arc::clone(&nested)).is_err());
        assert!(
            nested.graph().is_retired(),
            "a refused slot was left serving"
        );
        assert!(!owner.graph().is_retired());
        let _ = std::fs::remove_dir_all(base);
    }

    /// GH #543, audit R10-07: a settings write that reaches the graph is
    /// taken in by a reopen the write itself asks for, not by whichever
    /// watcher happens to be running. The decision is app-free: the write
    /// leaves the graph owing a reopen, the reopened graph owes nothing, and
    /// a change that reaches only settings is taken in without one.
    #[test]
    fn a_settings_write_decides_its_own_reopen() {
        let root =
            std::env::temp_dir().join(format!("gh543-config-take-in-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/Alpha.md"), "- alpha\n").unwrap();
        let slot = GraphSlot::new(Graph::open_checked(&root).unwrap(), root.clone());
        assert_eq!(take_in_config_now(&slot), ConfigTakeIn::Current);

        slot.apply_config_write(|g| g.set_show_brackets(false))
            .unwrap();
        assert!(!slot.graph_meta().show_brackets);
        assert_eq!(
            take_in_config_now(&slot),
            ConfigTakeIn::Current,
            "a settings-only change reopened the graph"
        );

        slot.apply_config_write(|g| g.set_journal_page_title_format("yyyy-MM-dd"))
            .unwrap();
        assert!(slot.config_pending(), "the write left nothing to take in");
        assert_eq!(take_in_config_now(&slot), ConfigTakeIn::Reopen);
        assert_eq!(
            take_in_config_now(&slot),
            ConfigTakeIn::Reopen,
            "asking again must not hide the pending reopen"
        );

        let prepared = prepare_legacy_refresh(
            &slot,
            None,
            crate::graph::DirectFilesServicePaths {
                projection: Ok(root.join("private/projection.sqlite")),
                concord_ledger: None,
            },
        )
        .unwrap();
        let reopened = prepared.commit(&slot);
        assert_eq!(
            reopened.graph_meta().journal_page_title_format,
            "yyyy-MM-dd"
        );
        assert_eq!(
            take_in_config_now(&reopened),
            ConfigTakeIn::Current,
            "a second caller for the same change would reopen again"
        );
        drop(reopened);
        let _ = std::fs::remove_dir_all(root);
    }

    /// GH #543, audit R10-07: the command front door reopens through the one
    /// config decider, and no command can write configuration past it
    /// (`GraphSlot::apply_config_write` is private to this module). The
    /// watcher asks the same decider.
    #[test]
    fn a_settings_write_takes_its_change_in_through_the_one_decider() {
        let state = include_str!("state.rs");
        let front = &state[state
            .find("\npub(crate) fn apply_config_write(")
            .expect("settings front door")..];
        let front = &front[..front.find("\n}\n").expect("front door ends")];
        let decider = &state[state
            .find("\nfn spawn_config_decider(")
            .expect("the decider spawn")..];
        let decider = &decider[..decider.find("\n}\n").expect("the decider spawn ends")];
        assert!(
            front.contains("config_pending()")
                && front.contains("spawn_config_decider(")
                && decider.contains("take_in_config_change("),
            "a settings write that reaches the graph must reopen it itself"
        );
        assert!(
            state.contains("\n    fn apply_config_write("),
            "GraphSlot::apply_config_write must stay private to state.rs"
        );
        let watcher = crate::test_support::rust_module_production_source("watcher.rs");
        assert!(
            watcher.contains("take_in_config_change(&state, app, label, RefreshLaneWait::TryOnce)")
        );
        assert!(
            !watcher.contains(".take_in_config()"),
            "the watcher decides a config change itself again; ask take_in_config_change"
        );
    }

    /// GH #543, audit R11-03: a settings write that lands on the slot a reopen
    /// is replacing is taken in by that slot, and the replacement, opened
    /// before the write, serves the older value. Asking the replacement once
    /// it is the window's graph (`serve_disk_config`) takes the write in. The
    /// guard below pins that every hand-over asks.
    #[test]
    fn a_settings_write_during_a_reopen_reaches_the_replacement() {
        let root =
            std::env::temp_dir().join(format!("gh543-r11-reopen-write-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::create_dir_all(root.join("logseq")).unwrap();
        std::fs::write(root.join("logseq/config.edn"), "{}\n").unwrap();
        std::fs::write(root.join("pages/Alpha.md"), "- alpha\n").unwrap();
        let services = || crate::graph::DirectFilesServicePaths {
            projection: Ok(root.join("private/projection.sqlite")),
            concord_ledger: None,
        };
        let old = GraphSlot::new(Graph::open_checked(&root).unwrap(), root.clone());
        let prepared = prepare_legacy_refresh(&old, None, services()).unwrap();
        old.apply_config_write(|g| g.set_favorites(&["Alpha".to_string()]))
            .unwrap();
        assert!(
            !old.config_pending(),
            "precondition: the slot being replaced took the write in, so no decider runs"
        );
        let replacement = prepared.commit(&old);
        assert!(
            replacement.graph_meta().favorites.is_empty(),
            "precondition: the replacement was opened before the write"
        );
        assert_eq!(take_in_config_now(&replacement), ConfigTakeIn::Current);
        assert_eq!(replacement.graph_meta().favorites, ["Alpha"]);
        assert!(!replacement.config_pending());
        let _ = replacement
            .graph()
            .detach_direct_projection(Duration::from_secs(15));
        drop(replacement);
        let _ = std::fs::remove_dir_all(root);
    }

    /// GH #543, audit R11-03: "does a graph handed to a window serve what
    /// `config.edn` holds?" has one answer, `serve_disk_config`, asked at
    /// every hand-over: a reopen's swap, an open's bind, and a same-root load
    /// that answers with the slot it already has. Each asks before the meta
    /// it hands the frontend is read. A new way to hand a window a graph asks
    /// too (exemplar: `refresh_graph_for_label`).
    #[test]
    fn every_graph_handover_serves_the_config_on_disk() {
        let body = |source: &str, start: &str| -> String {
            let from = &source[source.find(start).unwrap_or_else(|| panic!("{start}"))..];
            from[..from.find("\n}\n").expect("item ends")].to_owned()
        };
        let state = crate::test_support::rust_module_production_source("state.rs");
        let graph = crate::test_support::rust_module_production_source("graph.rs");
        let refresh = body(&state, "\nfn refresh_graph_for_label(");
        let swap = refresh.find(".swap_refreshed(").expect("the reopen swaps");
        let serve = refresh
            .find("serve_disk_config(")
            .expect("the reopen serves");
        assert!(swap < serve, "the reopen asks before the swap");
        let publish = body(&graph, "\npub(crate) fn publish_prepared_direct_files(");
        let bind = publish
            .find("publish_direct_files_slot(")
            .expect("the open binds");
        let serve = publish.find("serve_disk_config(").expect("the open serves");
        let meta = publish
            .find("slot.graph_meta()")
            .expect("the open reads meta");
        assert!(
            bind < serve && serve < meta,
            "the open serves after its bind, before its meta"
        );
        let load = body(&graph, "\npub(crate) fn load_graph_for_label(");
        let serve = load
            .find("serve_disk_config(")
            .expect("the same-root load serves");
        let current = load
            .find("LoadGraphResult::AlreadyCurrent {")
            .expect("the same-root answer");
        assert!(
            serve < current,
            "the same-root load serves before it answers"
        );
        let calls = [
            "state.rs",
            "graph.rs",
            "commands.rs",
            "backup.rs",
            "watcher.rs",
        ]
        .iter()
        .map(|file| {
            crate::test_support::rust_module_production_source(file)
                .matches("serve_disk_config(")
                .count()
        })
        .sum::<usize>();
        assert_eq!(calls, 4, "one definition and three hand-overs");
    }

    /// GH #543, audit R10-07: a failure to create the OS watcher is surfaced
    /// and the cycle polls instead. It was `.ok()`-ed away, and nothing
    /// external was seen for the session.
    #[test]
    fn a_missing_os_watcher_is_reported_and_polled_around() {
        let runtime = include_str!("watcher/runtime.rs");
        let created = &runtime[runtime
            .find("notify::recommended_watcher(")
            .expect("watcher creation")..];
        let created = &created[..created.find("watched.clear();").expect("creation ends")];
        assert!(
            created.contains("Err(error)") && created.contains("\"graph-watch-error\""),
            "a failed watcher creation must be surfaced"
        );
        assert!(
            runtime.contains("let inotify = wants_inotify && watcher.is_some();"),
            "a cycle without an OS watcher must poll"
        );
    }

    /// Every function that takes a window's graph without [`display_read`].
    /// A read whose answer is only displayed goes through `display_read`, so a
    /// refresh that replaces its graph cuts it short instead of letting it
    /// parse the old graph (GH #543, R2-01). The functions below take the
    /// graph directly because they write, export, act on their answer, or
    /// resolve the graph for another accessor.
    const DIRECT_GRAPH_ACCESS: &[&str] = &[
        // The accessors themselves, graph binding, and app plumbing.
        "capture_display_read",
        "capture_quick_switch_slot",
        "display_read",
        "drain_concord_ledgers_for_exit",
        "load_graph_for_label",
        "print_error",
        "refresh_capture_graph_binding",
        "refresh_graph_for_label",
        "take_in_config_change",
        "respond",
        "run",
        "slot_for_bound_window",
        "slot_for_context",
        "with_config_graph",
        "with_filesystem_graph",
        "with_trash_graph",
        // Writes: pages, assets, settings, conflicts, editor sessions.
        "activate_absent_editor",
        "activate_editor",
        "apply_journal_filename_migrations",
        "apply_config_write",
        "begin_direct_cross_page_move",
        "capture_live_save_conflict",
        "copy_guide_into_bound_graph",
        "create_graph_verification",
        "delete_page",
        "empty_asset_trash",
        "finish_direct_cross_page_move",
        "import_asset",
        "import_native_capture_blocking",
        "merge_pages",
        "open_pdf",
        "present_conflict_override",
        "rename_file_to_page",
        "rename_page",
        "resolve_conflict_capsule",
        "resolve_duplicate_journal_day",
        "resolve_durable_live_save_conflict",
        "resolve_live_save_conflict",
        "resolve_sync_conflict",
        "resolve_vcs_marker_conflict",
        "restore_backup",
        "retire_editor_activation",
        "save_asset",
        "save_notices",
        "save_page",
        "save_pdf_area_image",
        "save_session",
        "save_workspaces",
        "set_backup_keep",
        "trash_journal_file",
        "write_highlights",
        "write_pdf_view_state",
        // Exports, and reads whose answer is acted on: a partial answer would
        // be wrong output or a wrong deletion, never just a stale display.
        "edit_asset_external",
        "export_query_subtrees",
        "list_orphan_assets",
        "open_asset",
        "open_page_file",
        "publish_html",
        "publish_query",
        "publish_query_plan",
        // Conflict reviews tied to this graph instance's save epoch.
        "conflict_capsule_diff",
        "durable_live_save_conflict_diff",
        "live_save_conflict_diff",
        // Reads of named files or of state that never parses the page set.
        "asset_trash_stats",
        "graph_source_files",
        "indexing_progress",
        "list_backups",
        "list_journal_conflicts",
        "list_journal_filename_migrations",
        "load_notices",
        "load_session",
        "load_workspaces",
        "read_asset",
        "read_custom_css",
        "read_highlights",
        "read_journal_file",
        "stream_asset_path",
        "warm_done",
    ];

    /// The complete list of direct graph accessors: a new one fails here until
    /// it is either routed through `display_read` or listed above.
    #[test]
    fn a_read_takes_its_graph_through_display_read_or_is_listed() {
        const ACCESSORS: [&str; 6] = [
            "slot_for_bound_window(",
            "slot_for_window(",
            "slot_for_context(",
            "with_filesystem_graph(",
            "with_config_graph(",
            "capture_quick_switch_slot(",
        ];
        let mut owners = std::collections::BTreeSet::new();
        for (_, source) in crate::test_support::rust_module_sources() {
            let production = crate::test_support::without_cfg_test_items(&source);
            for accessor in ACCESSORS {
                for (index, _) in production.match_indices(accessor) {
                    let before = &production[..index];
                    if before.ends_with("fn ") {
                        continue;
                    }
                    let Some(at) = before.rfind("fn ") else {
                        continue;
                    };
                    let name = &production[at + 3..];
                    owners.insert(name[..name.find(['(', '<']).unwrap()].to_string());
                }
            }
        }
        let owners = owners.into_iter().collect::<Vec<_>>();
        let mut listed = DIRECT_GRAPH_ACCESS.to_vec();
        listed.sort_unstable();
        assert_eq!(
            owners, listed,
            "A function takes a window's graph without display_read. A read whose \
             answer is only displayed must use state::display_read, or a refresh \
             leaves it parsing the replaced graph (GH #543, I-13). A write, export, \
             or read that acts on its answer is listed in DIRECT_GRAPH_ACCESS."
        );
    }

    /// Retirement is permanent, so it happens only where a graph is finally
    /// replaced or unbound: a registry bind or removal, or a refresh commit,
    /// which runs after the last step that can fail (GH #543, R2-03).
    #[test]
    fn graphs_are_retired_only_where_they_are_replaced() {
        let mut sites = Vec::new();
        for (_, source) in crate::test_support::rust_module_sources() {
            let production = crate::test_support::without_cfg_test_items(&source);
            for (index, _) in production.match_indices(".retire()") {
                let owner = production[..index]
                    .rfind("fn ")
                    .map(|at| {
                        let name = &production[at + 3..];
                        name[..name.find(['(', '<']).unwrap()].to_string()
                    })
                    .unwrap();
                sites.push(owner);
            }
        }
        sites.sort();
        assert_eq!(
            sites,
            ["commit", "retire_slot"],
            "Graph::retire is called outside the two places a graph is replaced: \
             retire_slot (a window switching graphs or closing) and a refresh's \
             commit. A retired graph never answers a display read again, so retire \
             only after every fallible step; see PreparedRefresh::commit (I-13)."
        );
    }

    fn graph(root: &Path) -> Arc<GraphSlot> {
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::create_dir_all(root.join("journals")).unwrap();
        Arc::new(GraphSlot::new(Graph::open(root), root.to_path_buf()))
    }

    /// Rows of the OG query `[[Alpha]]`, straight from the graph's index.
    fn alpha_ref_count(graph: &Graph) -> Result<usize, tine_core::query::QueryExecutionError> {
        let registry = tine_core::query::registry::Registry::from_snapshot(
            &tine_core::query::ir::RegistrySnapshot {
                rows: Vec::new(),
                generation: 0,
            },
        );
        let (query, _view) = tine_core::query::parse_query_input(
            "[[Alpha]]",
            tine_core::query::QueryInput::Og,
            tine_core::date::JournalDate::today(),
            &registry,
        );
        let result = tine_core::query::run_query_result_ir(
            graph,
            &query,
            &tine_core::query::ir::ViewSettings::default(),
            tine_core::query::ir::Bounds::unbounded(),
            &tine_core::query::ir::ExecutionContext::none(),
        )?;
        Ok(match result.rows {
            tine_core::query::ir::QueryRows::Page { pages } => pages.len(),
            tine_core::query::ir::QueryRows::Block { groups } => {
                groups.iter().map(|group| group.blocks.len()).sum()
            }
        })
    }
    fn alpha_ref_count_when_ready(
        graph: &Graph,
    ) -> Result<usize, tine_core::query::QueryExecutionError> {
        let started = Instant::now();
        loop {
            match alpha_ref_count(graph) {
                Err(tine_core::query::QueryExecutionError::NotReady(_)) => {
                    assert!(
                        started.elapsed() < Duration::from_secs(30),
                        "the projection never became ready"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                other => return other,
            }
        }
    }

    /// GH #543 (indexing audit IT-06, R2-P1): a change to settings keeps the
    /// Graph, its index and its launch check. Changing one used to refresh the
    /// graph: the projection worker was retired and the graph reopened, and on
    /// a first launch -- when dismissing the Guide toast is most likely, and
    /// when a device-local home page is migrated into config.edn -- the whole
    /// index build restarted from nothing.
    #[test]
    fn a_settings_change_keeps_the_graph_and_its_index() {
        use tine_core::config::ConfigReach;
        let root = std::env::temp_dir().join(format!(
            "tine-presentation-setting-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["pages", "journals", "logseq"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("logseq/config.edn"), "{}\n").unwrap();
        std::fs::write(root.join("pages/Alpha.md"), "- alpha\n").unwrap();
        std::fs::write(root.join("pages/Beta.md"), "- see [[Alpha]]\n").unwrap();
        let graph = Graph::open_checked_with_assets(&root, None).unwrap();
        crate::graph::attach_direct_files_services(
            &graph,
            crate::graph::DirectFilesServicePaths {
                projection: Ok(root.join("private/projection.sqlite")),
                concord_ledger: None,
            },
        );
        graph.warm_cache();
        let slot = GraphSlot::new(graph, root.clone());
        let before = slot.graph();
        let answered = alpha_ref_count_when_ready(&before).expect("queries answer before");
        assert!(answered > 0, "the fixture has a referring block");
        assert!(!slot.graph_meta().show_brackets || !slot.graph_meta().guide_announced);

        slot.apply_config_write(|graph| graph.set_show_brackets(true))
            .unwrap();
        slot.apply_config_write(|graph| graph.set_guide_announced(true))
            .unwrap();
        slot.apply_config_write(|graph| graph.set_default_home_page(Some("Alpha")))
            .unwrap();
        slot.apply_config_write(|graph| graph.set_preferred_format(tine_core::model::Format::Org))
            .unwrap();

        assert!(
            Arc::ptr_eq(&before, &slot.graph()),
            "the graph was replaced"
        );
        let meta = slot.graph_meta();
        assert!(meta.show_brackets && meta.guide_announced);
        assert_eq!(meta.default_home.as_deref(), Some("Alpha"));
        assert_eq!(meta.preferred_format, "org");
        // The core reads what it was given, not what it was opened with.
        assert_eq!(slot.graph().config().default_home.as_deref(), Some("Alpha"));
        // Persisted: the next open reads them.
        let reopened = Graph::open(&root).meta();
        assert!(reopened.show_brackets && reopened.guide_announced);
        assert_eq!(reopened.default_home.as_deref(), Some("Alpha"));

        // An outside edit (another instance, Logseq, Syncthing) to a setting
        // is taken in the same way when the watcher sees it.
        Graph::open(&root).set_show_brackets(false).unwrap();
        assert_eq!(slot.take_in_config(), ConfigReach::Settings);
        assert!(
            Arc::ptr_eq(&before, &slot.graph()),
            "the graph was replaced"
        );
        assert!(!slot.graph_meta().show_brackets);
        // One that reaches the graph is not: the watcher opens a new Graph.
        Graph::open(&root)
            .set_journal_page_title_format("yyyy-MM-dd")
            .unwrap();
        assert_eq!(slot.take_in_config(), ConfigReach::Graph);
        assert_ne!(
            slot.graph().config().journal_page_title_format.as_deref(),
            Some("yyyy-MM-dd")
        );
        // The index was never detached: the same query answers at once, with
        // no second launch check (a refresh would need one before answering).
        assert_eq!(alpha_ref_count(&before).ok(), Some(answered));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// GH draft "Query Engine" (2026-09-11): dismissing the Guide toast made
    /// every query `ProjectionUnavailable` until the next graph open, because
    /// `set_guide_announced` → `refresh_graph` rebuilt the `Graph` without the
    /// Direct Files projection the open path attaches. Same shape for every
    /// settings command that refreshes, restore-from-backup, and an external
    /// `config.edn` rewrite. The refresh body must attach what the open attaches.
    #[test]
    fn a_config_refresh_keeps_queries_answering() {
        let root = std::env::temp_dir().join(format!(
            "tine-refresh-keeps-projection-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["pages", "journals", "logseq"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("logseq/config.edn"), "{}\n").unwrap();
        std::fs::write(root.join("pages/Alpha.md"), "- alpha root\n").unwrap();
        std::fs::write(
            root.join("pages/Beta.md"),
            "- refers to [[Alpha]]\n- and [[Alpha]] again\n",
        )
        .unwrap();
        let projection = root.join("private/projection.sqlite");
        let services = || crate::graph::DirectFilesServicePaths {
            projection: Ok(projection.clone()),
            concord_ledger: None,
        };

        let graph = Graph::open_checked_with_assets(&root, None).unwrap();
        crate::graph::attach_direct_files_services(&graph, services());
        graph.warm_cache();
        let old = Arc::new(GraphSlot::new(graph, root.clone()));
        let before = alpha_ref_count_when_ready(&old.graph()).expect("queries answer before");
        assert!(before > 0, "the fixture has referring blocks");

        // What a refreshing settings command (`set_preferred_format`,
        // `set_journal_title_format`, `set_default_home`) does: a config write,
        // then a refresh.
        old.graph().set_guide_announced(true).unwrap();
        let replacement = prepare_legacy_refresh(&old, None, services())
            .unwrap()
            .commit(&old);
        // The open path warms through `warm_cache_async`; the refresh core hands
        // that to its caller, so warm here exactly as the caller would.
        let reopened = replacement.graph();
        reopened.warm_cache();
        let after = alpha_ref_count_when_ready(&reopened);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            after.ok(),
            Some(before),
            "after refresh the same query must answer"
        );
    }

    #[test]
    fn owned_command_context_drops_borrowed_tauri_state_after_capturing_exact_binding() {
        let source = include_str!("state.rs");
        let start = source
            .find("pub(crate) fn owned_graph_context(")
            .expect("owned graph context helper");
        let tail = &source[start..];
        let end = tail
            .find("\nimpl<'r, 'de: 'r, R: Runtime> CommandArg")
            .expect("owned graph context helper boundary");
        let helper = &tail[..end];
        let compact: String = helper.split_whitespace().collect();
        for required in [
            "state.window.app_handle().clone()",
            "state.window.label().to_string()",
            "state.binding_generation",
            "drop(state)",
        ] {
            assert!(
                compact.contains(required),
                "owned command context must retain `{required}` before await"
            );
        }
    }

    #[test]
    fn graph_slots_are_send_sync() {
        let base =
            std::env::temp_dir().join(format!("tine-slot-authority-{}", uuid::Uuid::new_v4()));
        let _slot = graph(&base);

        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GraphSlot>();

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn explicit_graph_activation_updates_capture_routing_idempotently() {
        let state = AppState {
            graphs: RwLock::new(GraphRegistry::default()),
            storage_supervisor:
                crate::storage_transition_supervisor::StorageTransitionSupervisor::default(),
            watch_ctl: Mutex::new(None),
            last_focused: Mutex::new(Some("graph-1".into())),
            capture_graph: Mutex::new(Default::default()),
            #[cfg(desktop)]
            next_window: AtomicU64::new(2),
        };

        assert!(state.note_focused("main"));
        assert_eq!(state.last_focused.lock().unwrap().as_deref(), Some("main"));
        assert!(!state.note_focused("main"));
    }

    #[test]
    fn pending_capture_show_is_completed_once_and_newer_show_revokes_it() {
        let mut show = CaptureShow::default();
        let first = show.begin();
        assert!(show.binding.is_none());
        let second = show.begin();
        let binding = CaptureGraphBinding {
            target: "main".into(),
            binding_generation: 17,
        };
        assert!(!show.complete(first, binding.clone()));
        assert!(show.pending);
        assert!(show.complete(second, binding.clone()));
        assert_eq!(show.binding, Some(binding));
        // An unrelated graph publication cannot retarget an already shown lease.
        assert!(!show.complete(
            second,
            CaptureGraphBinding {
                target: "other".into(),
                binding_generation: 18,
            }
        ));
        assert_eq!(show.binding.as_ref().unwrap().target, "main");
        assert!(!show.pending);
        assert!(show.begin() > second);
        assert!(show.binding.is_none());
    }

    #[test]
    fn capture_binding_retains_the_selected_graph_lease() {
        let state = AppState {
            graphs: RwLock::new(GraphRegistry::default()),
            storage_supervisor:
                crate::storage_transition_supervisor::StorageTransitionSupervisor::default(),
            watch_ctl: Mutex::new(None),
            last_focused: Mutex::new(Some("main".into())),
            capture_graph: Mutex::new(Default::default()),
            #[cfg(desktop)]
            next_window: AtomicU64::new(2),
        };

        state.bind_capture_graph("main".into(), 17);
        assert_eq!(
            state.capture_graph_binding(),
            Some(CaptureGraphBinding {
                target: "main".into(),
                binding_generation: 17,
            })
        );
        state.bind_capture_graph("graph-1".into(), 18);
        assert_eq!(
            state.capture_graph_binding(),
            Some(CaptureGraphBinding {
                target: "graph-1".into(),
                binding_generation: 18,
            })
        );
    }

    /// GH #543 (R6-03): switching a window to another graph detaches the
    /// displaced graph's index writer, so its lease is released at once and
    /// a switch back can index instead of waiting for the old graph to drop.
    #[test]
    fn a_graph_switch_detaches_the_displaced_graphs_index_writer() {
        let base =
            std::env::temp_dir().join(format!("tine-switch-detach-{}", uuid::Uuid::new_v4()));
        let first = graph(&base.join("a"));
        let second = graph(&base.join("b"));
        first
            .graph()
            .attach_direct_projection(base.join("index/projection.sqlite"))
            .unwrap();
        let mut registry = GraphRegistry::default();
        registry.bind("main".into(), Arc::clone(&first)).unwrap();
        registry.bind("main".into(), Arc::clone(&second)).unwrap();
        let (wake, _woken) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        while first
            .graph()
            .observe_direct_projection_commits(wake.clone())
            .is_some()
        {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the displaced graph kept its index writer"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_dir_all(base);
    }

    /// The window-close sibling of the switch above (audit R7-05): a closed
    /// window's graph lets go of its index writer too, so reopening the graph
    /// does not wait for the lease until every clone of it is dropped.
    #[test]
    fn a_window_close_detaches_its_graphs_index_writer() {
        let base = std::env::temp_dir().join(format!("tine-close-detach-{}", uuid::Uuid::new_v4()));
        let slot = graph(&base.join("a"));
        slot.graph()
            .attach_direct_projection(base.join("index/projection.sqlite"))
            .unwrap();
        let mut registry = GraphRegistry::default();
        registry.bind("main".into(), Arc::clone(&slot)).unwrap();
        // A capture binding or an in-flight command still holds the slot.
        let removed = registry.remove("main").unwrap();
        let (wake, _woken) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        while slot
            .graph()
            .observe_direct_projection_commits(wake.clone())
            .is_some()
        {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the closed window's graph kept its index writer"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(removed);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn same_root_refresh_preserves_frontend_binding_lease() {
        let base = std::env::temp_dir().join(format!("tine-slot-refresh-{}", std::process::id()));
        let old = graph(&base);
        old.warm_done
            .store(true, std::sync::atomic::Ordering::Release);
        old.warm_generation
            .store(7, std::sync::atomic::Ordering::Release);

        let replacement = GraphSlot::refreshed(Graph::open(&base), &old);

        assert_eq!(replacement.binding_generation, old.binding_generation);
        assert_eq!(replacement.root_key, old.root_key);
        assert!(replacement
            .warm_done
            .load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            replacement
                .warm_generation
                .load(std::sync::atomic::Ordering::Acquire),
            7
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn a_display_read_on_a_retired_graph_is_answered_by_its_replacement() {
        let base = std::env::temp_dir().join(format!("tine-retired-read-{}", uuid::Uuid::new_v4()));
        let state = Arc::new(AppState {
            graphs: RwLock::new(GraphRegistry::default()),
            storage_supervisor:
                crate::storage_transition_supervisor::StorageTransitionSupervisor::default(),
            watch_ctl: Mutex::new(None),
            last_focused: Mutex::new(Some("main".into())),
            capture_graph: Mutex::new(Default::default()),
            #[cfg(desktop)]
            next_window: AtomicU64::new(2),
        });
        let old = graph(&base);
        std::fs::write(base.join("pages/Alpha.md"), "- alpha\n").unwrap();
        let generation = old.binding_generation;
        state
            .graphs
            .write()
            .unwrap()
            .bind("main".into(), Arc::clone(&old))
            .unwrap();
        // A refresh retires the old graph and binds its replacement shortly after.
        old.graph().retire();
        let binder = std::thread::spawn({
            let state = Arc::clone(&state);
            let old = Arc::clone(&old);
            move || {
                std::thread::sleep(Duration::from_millis(200));
                let replacement = Graph::open_checked_with_assets(&old.root_key, None).unwrap();
                let slot = Arc::new(GraphSlot::refreshed(replacement, &old));
                state
                    .graphs
                    .write()
                    .unwrap()
                    .bind("main".into(), slot)
                    .unwrap();
            }
        });
        let started = std::time::Instant::now();
        let names = display_read(&state, "main", generation, |graph| {
            graph
                .list_pages()
                .into_iter()
                .map(|entry| entry.name)
                .collect::<Vec<_>>()
        })
        .unwrap();
        let elapsed = started.elapsed();
        binder.join().unwrap();
        assert_eq!(names, vec!["Alpha".to_owned()]);
        assert!(
            elapsed >= Duration::from_millis(200),
            "the retired graph must not answer by parsing itself; the replacement answers"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn queued_owned_command_generation_is_rejected_after_graph_replacement() {
        let base = std::env::temp_dir().join(format!("tine-slot-stale-{}", uuid::Uuid::new_v4()));
        let old_root = base.join("old");
        let new_root = base.join("new");
        let state = AppState {
            graphs: RwLock::new(GraphRegistry::default()),
            storage_supervisor:
                crate::storage_transition_supervisor::StorageTransitionSupervisor::default(),
            watch_ctl: Mutex::new(None),
            last_focused: Mutex::new(Some("main".into())),
            capture_graph: Mutex::new(Default::default()),
            #[cfg(desktop)]
            next_window: AtomicU64::new(2),
        };
        let old = graph(&old_root);
        let captured_generation = old.binding_generation;
        state
            .graphs
            .write()
            .unwrap()
            .bind("main".into(), old)
            .unwrap();
        let replacement = graph(&new_root);
        assert_ne!(replacement.binding_generation, captured_generation);
        state
            .graphs
            .write()
            .unwrap()
            .bind("main".into(), replacement)
            .unwrap();

        assert_eq!(
            slot_for_bound_window(&state, "main", Some(captured_generation))
                .err()
                .unwrap()
                .to_string(),
            "stale-graph-binding"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn registry_keeps_window_and_root_indices_in_sync() {
        let base = std::env::temp_dir().join(format!("tine-registry-{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        let mut registry = GraphRegistry::default();
        let old = graph(&a);
        registry.bind("main".into(), old.clone()).unwrap();
        assert_eq!(registry.owner(&a).as_deref(), Some("main"));
        registry.bind("main".into(), graph(&b)).unwrap();
        assert!(old
            .background_cancelled
            .load(std::sync::atomic::Ordering::Acquire));
        assert!(registry.owner(&a).is_none());
        assert_eq!(registry.owner(&b).as_deref(), Some("main"));
        let current = registry.slot("main").unwrap();
        registry.remove("main");
        assert!(current
            .background_cancelled
            .load(std::sync::atomic::Ordering::Acquire));
        assert!(registry.owner(&b).is_none());
        assert_eq!(registry.len(), 0);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn unpublished_candidate_has_no_registry_or_watcher_ownership() {
        let base = std::env::temp_dir().join(format!(
            "tine-unpublished-candidate-{}",
            uuid::Uuid::new_v4()
        ));
        let serving_root = base.join("serving");
        let candidate_root = base.join("candidate");
        let mut registry = GraphRegistry::default();
        let serving = graph(&serving_root);
        registry.bind("main".into(), Arc::clone(&serving)).unwrap();

        let candidate = graph(&candidate_root);
        assert_eq!(registry.entries().len(), 1);
        assert!(Arc::ptr_eq(&registry.entries()[0].1, &serving));
        assert!(registry.owner(&candidate.root_key).is_none());
        assert_ne!(candidate.binding_generation, serving.binding_generation);

        let watcher = rust_module_source("watcher.rs");
        assert!(watcher.contains("app.state::<AppState>().graphs.read().unwrap().entries()"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn registry_rejects_two_windows_for_one_root() {
        let base = std::env::temp_dir().join(format!("tine-registry-dupe-{}", std::process::id()));
        let mut registry = GraphRegistry::default();
        registry.bind("main".into(), graph(&base)).unwrap();
        assert!(registry.bind("graph-1".into(), graph(&base)).is_err());
        assert!(registry.slot("graph-1").is_none());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn registry_rejects_ancestor_and_descendant_graph_roots() {
        let base =
            std::env::temp_dir().join(format!("tine-registry-nested-{}", std::process::id()));
        let parent = base.join("parent");
        let child = parent.join("pages").join("child");
        let sibling = base.join("sibling");

        let mut registry = GraphRegistry::default();
        registry.bind("main".into(), graph(&parent)).unwrap();
        assert!(registry.bind("child".into(), graph(&child)).is_err());
        assert!(registry.bind("sibling".into(), graph(&sibling)).is_ok());

        let mut reverse = GraphRegistry::default();
        reverse.bind("child".into(), graph(&child)).unwrap();
        assert!(reverse.bind("parent".into(), graph(&parent)).is_err());
        let _ = std::fs::remove_dir_all(base);
    }
}
