use std::collections::HashMap;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};
use tauri::ipc::{CommandArg, CommandItem, InvokeBody, InvokeError};
use tauri::{Manager, Runtime, State, WebviewWindow};
use tine_core::model::Graph;

pub(crate) type WindowKey = String;
pub(crate) const SPARSE_V2_UNSUPPORTED: &str =
    "This action is unavailable while Tine-managed storage is active.";
static NEXT_BINDING: AtomicU64 = AtomicU64::new(1);

/// The bounded application-page envelope the current graph binding can accept.
/// This is an advisory frontend wire record only: the actor remains the final
/// authority for every managed application save.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct ApplicationPageAdmission {
    pub(crate) binding_generation: u64,
    #[serde(flatten)]
    pub(crate) authority: ApplicationPageAdmissionAuthority,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(tag = "authority", rename_all = "snake_case")]
pub(crate) enum ApplicationPageAdmissionAuthority {
    Direct,
    ManagedWritable {
        application_save_page_blocks: usize,
        application_page_request_text_bytes: usize,
        application_page_max_depth: usize,
    },
    ManagedUnavailable,
}

impl ApplicationPageAdmission {
    pub(crate) fn direct(binding_generation: u64) -> Self {
        Self {
            binding_generation,
            authority: ApplicationPageAdmissionAuthority::Direct,
        }
    }

    pub(crate) fn managed_unavailable(binding_generation: u64) -> Self {
        Self {
            binding_generation,
            authority: ApplicationPageAdmissionAuthority::ManagedUnavailable,
        }
    }
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

/// Native authority for a startup recovery action.  The frontend may preserve
/// an injected or locally cached path for display, but a destructive cold
/// recovery is authorized only after the native remembered-graph lookup has
/// associated this exact canonical root with the current window attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StartupRecoveryTarget {
    attempt: u64,
    canonical_root: Option<PathBuf>,
}

/// The single **write** authority retained for one graph/window binding.
///
/// Sparse v2 has its own bounded actor commands and is never routed through
/// legacy Tauri graph surfaces. Keeping the variants mutually exclusive
/// prevents a binding from retaining both a legacy `Graph` and a sparse actor.
pub(crate) enum GraphAuthority {
    Legacy(Arc<Graph>),
    SparseV2(crate::sync_runtime::SparseV2Binding),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GraphAuthorityKind {
    Legacy,
    SparseV2,
}

fn require_legacy_authority(kind: GraphAuthorityKind) -> Result<(), String> {
    match kind {
        GraphAuthorityKind::Legacy => Ok(()),
        GraphAuthorityKind::SparseV2 => Err(SPARSE_V2_UNSUPPORTED.into()),
    }
}

impl GraphAuthority {
    fn kind(&self) -> GraphAuthorityKind {
        match self {
            Self::Legacy(_) => GraphAuthorityKind::Legacy,
            Self::SparseV2(_) => GraphAuthorityKind::SparseV2,
        }
    }

    fn legacy_graph_cloned(&self) -> Result<Arc<Graph>, String> {
        require_legacy_authority(self.kind())?;
        match self {
            Self::Legacy(graph) => Ok(Arc::clone(graph)),
            Self::SparseV2(_) => unreachable!("sparse authority was rejected above"),
        }
    }

    fn is_sparse_v2(&self) -> bool {
        self.kind() == GraphAuthorityKind::SparseV2
    }
}

#[derive(Default)]
struct LegacyLeaseState {
    retiring: bool,
    active: usize,
}

#[derive(Default)]
struct LegacyLeaseTracker {
    state: Mutex<LegacyLeaseState>,
    drained: Condvar,
}

/// A tracked use of the legacy graph authority.
///
/// Promotion first prevents new leases and removes the graph slot, then waits
/// for every instance of this type to drop. Watcher/background/async clones all
/// use the same tracker, including clones retained across a same-root refresh.
pub(crate) struct LegacyGraphLease {
    graph: Arc<Graph>,
    tracker: Arc<LegacyLeaseTracker>,
}

impl Deref for LegacyGraphLease {
    type Target = Graph;

    fn deref(&self) -> &Self::Target {
        &self.graph
    }
}

impl Drop for LegacyGraphLease {
    fn drop(&mut self) {
        let mut state = self.tracker.state.lock().unwrap();
        state.active = state
            .active
            .checked_sub(1)
            .expect("legacy graph lease count underflow");
        if state.active == 0 {
            self.tracker.drained.notify_all();
        }
    }
}

pub(crate) struct GraphSlot {
    authority: GraphAuthority,
    legacy_leases: Option<Arc<LegacyLeaseTracker>>,
    /// The configuration snapshot `load_graph` hands the frontend. Replaceable
    /// because a settings change under managed storage has to update it: the
    /// legacy path publishes a whole replacement slot after a reopen, and
    /// without this a managed graph would report the settings it activated with
    /// the next time the same window loads the same root.
    graph_meta: RwLock<tine_core::model::GraphMeta>,
    /// The graph root this slot answers for, as the meta first reported it.
    /// Fixed for the life of the slot, so refreshing the meta can never move it.
    graph_root: PathBuf,
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
            authority: GraphAuthority::Legacy(Arc::new(graph)),
            legacy_leases: Some(Arc::new(LegacyLeaseTracker::default())),
            graph_root: PathBuf::from(&graph_meta.root),
            graph_meta: RwLock::new(graph_meta),
            root_key,
            binding_generation: NEXT_BINDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            warm_done: AtomicBool::new(false),
            warm_generation: AtomicU64::new(0),
            background_cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Transfer one active or visibly unavailable sparse-v2 binding into the
    /// graph slot. The slot contains no legacy `Graph`.
    pub(crate) fn from_sparse_v2(
        binding: crate::sync_runtime::SparseV2Binding,
        root_key: PathBuf,
        graph_meta: tine_core::model::GraphMeta,
    ) -> Self {
        Self {
            authority: GraphAuthority::SparseV2(binding),
            legacy_leases: None,
            graph_root: PathBuf::from(&graph_meta.root),
            graph_meta: RwLock::new(graph_meta),
            root_key,
            binding_generation: NEXT_BINDING.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            warm_done: AtomicBool::new(false),
            warm_generation: AtomicU64::new(0),
            background_cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The legacy-only authority gate used by all existing Tauri graph paths.
    pub(crate) fn legacy_graph(&self) -> Result<LegacyGraphLease, String> {
        let graph = self.authority.legacy_graph_cloned()?;
        let tracker = self
            .legacy_leases
            .as_ref()
            .expect("legacy authority always has a lease tracker");
        let mut state = tracker.state.lock().unwrap();
        if state.retiring {
            return Err("legacy graph authority is retiring".into());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or("legacy graph lease count exhausted")?;
        drop(state);
        Ok(LegacyGraphLease {
            graph,
            tracker: Arc::clone(tracker),
        })
    }

    /// Clone the legacy writer only after the authority gate has admitted it.
    pub(crate) fn legacy_graph_cloned(&self) -> Result<LegacyGraphLease, String> {
        self.legacy_graph()
    }

    pub(crate) fn is_sparse_v2(&self) -> bool {
        self.authority.is_sparse_v2()
    }

    /// Report the selected save route for this exact graph binding.  The
    /// frontend must not infer this from a managed-status label: a joinable
    /// archive still has a legacy Direct-Files writer, while a sparse slot with
    /// no retained application handle has no writer at all.
    pub(crate) fn application_page_admission(&self) -> ApplicationPageAdmission {
        match &self.authority {
            GraphAuthority::Legacy(_) => ApplicationPageAdmission::direct(self.binding_generation),
            GraphAuthority::SparseV2(binding) if binding.has_active_application_handle() => {
                ApplicationPageAdmission {
                    binding_generation: self.binding_generation,
                    authority: ApplicationPageAdmissionAuthority::ManagedWritable {
                        application_save_page_blocks:
                            tine_core::sync_runtime::MAX_SYNC_EDITOR_BLOCKS,
                        application_page_request_text_bytes:
                            tine_core::sync_runtime::MAX_SYNC_EDITOR_REQUEST_BYTES,
                        application_page_max_depth: tine_core::sync_runtime::MAX_SYNC_EDITOR_DEPTH,
                    },
                }
            }
            GraphAuthority::SparseV2(_) => {
                ApplicationPageAdmission::managed_unavailable(self.binding_generation)
            }
        }
    }

    /// Persist a change to `logseq/config.edn` under whichever authority this
    /// slot holds.
    ///
    /// Graph configuration is **outside the oplog's document domain**: the
    /// managed scanner classifies `logseq/config.edn` as configuration and the
    /// reconciliation baseline drops it as "not managed content", so nothing
    /// imports or projects it. Settings therefore use the short-lived
    /// filesystem capability, enforced in `tine-core` by
    /// `Graph::ensure_config_write_target`, without graph-text authority.
    pub(crate) fn with_config_graph<T>(
        &self,
        f: impl FnOnce(&Graph) -> Result<T, String>,
    ) -> Result<T, String> {
        self.with_filesystem_graph(f)
    }

    /// Move something into (or clear) the recoverable trash under whichever
    /// authority this slot holds.
    ///
    /// `logseq/.tine-trash` is outside the oplog's document domain in exactly
    /// the way `assets/` is, so the asset-side trash writes that a read-only
    /// view refused only incidentally come back here. Trashing a page, journal
    /// or conflict copy is a graph-text deletion and is still refused inside
    /// `tine-core`, at `Graph::admit_managed_text_writer`.
    pub(crate) fn with_trash_graph<T>(
        &self,
        f: impl FnOnce(&Graph) -> Result<T, String>,
    ) -> Result<T, String> {
        self.with_filesystem_graph(f)
    }

    /// Run one point-addressed filesystem/config/asset operation. Managed mode
    /// opens a short-lived root capability and never retains a parsed-page
    /// cache; graph-semantic commands must use the application actor instead.
    pub(crate) fn with_filesystem_graph<T>(
        &self,
        f: impl FnOnce(&Graph) -> Result<T, String>,
    ) -> Result<T, String> {
        match &self.authority {
            GraphAuthority::Legacy(_) => {
                let lease = self.legacy_graph()?;
                f(&lease)
            }
            GraphAuthority::SparseV2(_) => {
                let graph = Graph::open_derived_read_only(&self.graph_root);
                f(&graph)
            }
        }
    }

    pub(crate) fn refresh_filesystem_meta(&self) {
        let graph = Graph::open_derived_read_only(&self.graph_root);
        *self.graph_meta.write().unwrap() = graph.meta();
    }

    pub(crate) fn graph_meta(&self) -> tine_core::model::GraphMeta {
        self.graph_meta.read().unwrap().clone()
    }

    /// Borrow the actor only through the graph slot that owns it.
    pub(crate) fn sparse_runtime(&self) -> Option<&tine_core::sync_runtime::SyncRuntimeHandle> {
        match &self.authority {
            GraphAuthority::Legacy(_) => None,
            GraphAuthority::SparseV2(binding) => binding.handle(),
        }
    }

    pub(crate) fn sparse_binding(&self) -> Option<&crate::sync_runtime::SparseV2Binding> {
        match &self.authority {
            GraphAuthority::Legacy(_) => None,
            GraphAuthority::SparseV2(binding) => Some(binding),
        }
    }

    /// Prevent new legacy work before the registry binding is removed.
    pub(crate) fn begin_legacy_retirement(&self) -> Result<(), String> {
        require_legacy_authority(self.authority.kind())?;
        let tracker = self
            .legacy_leases
            .as_ref()
            .expect("legacy authority always has a lease tracker");
        tracker.state.lock().unwrap().retiring = true;
        self.background_cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Bounded proof that no watcher, background task, or command can still use
    /// this legacy `Graph`.
    pub(crate) fn wait_for_legacy_drain(&self, timeout: Duration) -> Result<(), String> {
        let tracker = self
            .legacy_leases
            .as_ref()
            .ok_or_else(|| SPARSE_V2_UNSUPPORTED.to_string())?;
        let deadline = Instant::now() + timeout;
        let mut state = tracker.state.lock().unwrap();
        while state.active != 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "legacy graph authority did not drain {} retained lease(s)",
                    state.active
                ));
            }
            let (next, wait) = tracker.drained.wait_timeout(state, remaining).unwrap();
            state = next;
            if wait.timed_out() && state.active != 0 {
                return Err(format!(
                    "legacy graph authority did not drain {} retained lease(s)",
                    state.active
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn cancel_legacy_retirement(&self) -> Result<(), String> {
        require_legacy_authority(self.authority.kind())?;
        let tracker = self
            .legacy_leases
            .as_ref()
            .expect("legacy authority always has a lease tracker");
        tracker.state.lock().unwrap().retiring = false;
        self.background_cancelled
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Re-open the graph object for the same window/root without revoking the
    /// frontend's lease. A binding generation identifies a window -> graph-root
    /// assignment, not the particular in-memory `Graph` instance. Minting a new
    /// generation here made every later command from that window stale after a
    /// config refresh, including autosaves.
    fn refreshed(graph: Graph, old: &GraphSlot) -> Result<Self, String> {
        // Refresh is a legacy whole-graph reopen. Refuse before replacing the
        // slot so a sparse actor remains the sole authority for its binding.
        let old_graph = old.legacy_graph()?;
        drop(old_graph);
        let graph_meta = graph.meta();
        Ok(Self {
            authority: GraphAuthority::Legacy(Arc::new(graph)),
            legacy_leases: old.legacy_leases.clone(),
            graph_root: PathBuf::from(&graph_meta.root),
            graph_meta: RwLock::new(graph_meta),
            root_key: old.root_key.clone(),
            binding_generation: old.binding_generation,
            warm_done: AtomicBool::new(old.warm_done.load(std::sync::atomic::Ordering::Acquire)),
            warm_generation: AtomicU64::new(
                old.warm_generation
                    .load(std::sync::atomic::Ordering::Acquire),
            ),
            background_cancelled: Arc::clone(&old.background_cancelled),
        })
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

    pub(crate) fn bind(&mut self, window: WindowKey, slot: Arc<GraphSlot>) -> Result<(), String> {
        for (root, owner) in &self.by_root {
            if owner != &window
                && (root.starts_with(&slot.root_key) || slot.root_key.starts_with(root))
            {
                return Err(format!(
                    "graph {} overlaps graph {} already owned by window {owner}",
                    slot.root_key.display(),
                    root.display()
                ));
            }
        }
        if let Some(old) = self.by_window.insert(window.clone(), slot.clone()) {
            // A same-root refresh replaces only the in-memory Graph object and
            // preserves the frontend binding lease. Let its already-running
            // warm/backup finish; a real graph switch revokes the old source.
            if old.binding_generation != slot.binding_generation || old.root_key != slot.root_key {
                old.background_cancelled
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            self.by_root.remove(&old.root_key);
        }
        self.by_root.insert(slot.root_key.clone(), window);
        Ok(())
    }

    pub(crate) fn remove(&mut self, window: &str) -> Option<Arc<GraphSlot>> {
        let slot = self.by_window.remove(window)?;
        slot.background_cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        // This only revokes Tauri background work. Sparse actor shutdown and
        // any resulting safety status belong to a later integration boundary;
        // dropping the last handle must not be interpreted as a Safe shutdown.
        self.by_root.remove(&slot.root_key);
        Some(slot)
    }
}

pub(crate) struct AppState {
    pub(crate) graphs: RwLock<GraphRegistry>,
    // Serializes open/switch/window-create decisions. Existing commands never
    // take this lock, so a slow graph open cannot stall another graph's editor.
    pub(crate) graph_load: Mutex<()>,
    pub(crate) watch_ctl: Mutex<Option<Sender<()>>>,
    pub(crate) last_focused: Mutex<Option<WindowKey>>,
    pub(crate) capture_graph: Mutex<Option<CaptureGraphBinding>>,
    pub(crate) startup_recovery: Mutex<HashMap<WindowKey, StartupRecoveryTarget>>,
    /// Stateless sparse runtime composition. It retains no runtime handle;
    /// active authority lives only in the corresponding graph slot.
    pub(crate) sync_runtime: crate::sync_runtime::SyncRuntimeFacade,
    #[cfg(desktop)]
    pub(crate) next_window: AtomicU64,
}

impl AppState {
    pub(crate) fn begin_startup_recovery_attempt(&self, window: &str, attempt: u64) {
        self.startup_recovery.lock().unwrap().insert(
            window.to_string(),
            StartupRecoveryTarget {
                attempt,
                canonical_root: None,
            },
        );
    }

    /// A late worker cannot overwrite a newer attempt's authority.  An
    /// unavailable/missing remembered graph remains deliberately unauthorised.
    pub(crate) fn authorize_startup_recovery_target(
        &self,
        window: &str,
        attempt: u64,
        canonical_root: Option<PathBuf>,
    ) {
        if let Some(target) = self.startup_recovery.lock().unwrap().get_mut(window) {
            if target.attempt == attempt {
                target.canonical_root = canonical_root;
            }
        }
    }

    pub(crate) fn authorized_startup_recovery_target(
        &self,
        window: &str,
        attempt: u64,
    ) -> Result<PathBuf, String> {
        let target = self.startup_recovery.lock().unwrap();
        let Some(target) = target.get(window) else {
            return Err(
                "This recovery action is no longer current. Retry graph lookup before returning to Direct files."
                    .into(),
            );
        };
        if target.attempt != attempt {
            return Err(
                "This recovery action is no longer current. Retry graph lookup before returning to Direct files."
                    .into(),
            );
        }
        target.canonical_root.clone().ok_or_else(|| {
            "Tine has not verified a remembered graph for this recovery action. Retry graph lookup before returning to Direct files."
                .into()
        })
    }

    pub(crate) fn startup_recovery_attempt_is_current(&self, window: &str, attempt: u64) -> bool {
        self.startup_recovery
            .lock()
            .unwrap()
            .get(window)
            .is_some_and(|target| target.attempt == attempt)
    }

    /// Starting a normal graph open supersedes any startup recovery action for
    /// that window, so a delayed button click cannot mutate a graph after the
    /// user chose another recovery route.
    pub(crate) fn clear_startup_recovery_target(&self, window: &str) {
        self.startup_recovery.lock().unwrap().remove(window);
    }

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
    pub(crate) fn bind_capture_graph(&self, target: WindowKey, binding_generation: u64) {
        *self.capture_graph.lock().unwrap() = Some(CaptureGraphBinding {
            target,
            binding_generation,
        });
    }

    pub(crate) fn capture_graph_binding(&self) -> Option<CaptureGraphBinding> {
        self.capture_graph.lock().unwrap().clone()
    }

    pub(crate) fn clear_capture_graph(&self) {
        *self.capture_graph.lock().unwrap() = None;
    }
}

pub(crate) struct GraphContext<'a, R: Runtime = tauri::Wry> {
    pub(crate) state: State<'a, AppState>,
    pub(crate) window: WebviewWindow<R>,
    pub(crate) binding_generation: Option<u64>,
}

pub(crate) fn owned_graph_context(
    state: GraphContext<'_>,
) -> Result<(tauri::AppHandle, String, u64), String> {
    let app = state.window.app_handle().clone();
    let label = state.window.label().to_string();
    let binding_generation = state.binding_generation.ok_or("missing-graph-binding")?;
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

pub(crate) fn canonical_graph_root(path: &str) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(path)
        .map_err(|e| format!("couldn't resolve graph path {path}: {e}"))?;
    if !root.is_dir() {
        return Err(format!("graph path is not a folder: {}", root.display()));
    }
    Ok(root)
}

pub(crate) fn slot_for_window(state: &AppState, window: &str) -> Result<Arc<GraphSlot>, String> {
    state
        .graphs
        .read()
        .unwrap()
        .slot(window)
        .ok_or_else(|| format!("no graph loaded for window {window}"))
}

pub(crate) fn slot_for_context(ctx: &GraphContext<'_>) -> Result<Arc<GraphSlot>, String> {
    slot_for_bound_window(&ctx.state, ctx.window.label(), ctx.binding_generation)
}

/// Resolve a normal graph-window command. Quick Capture intentionally has no
/// graph slot, so this path cannot be used to grant it any GraphContext command
/// (including save, delete, trash, or other mutations).
pub(crate) fn slot_for_bound_window(
    state: &AppState,
    window: &str,
    binding_generation: Option<u64>,
) -> Result<Arc<GraphSlot>, String> {
    let slot = slot_for_window(state, window)?;
    let generation = binding_generation.ok_or("missing-graph-binding")?;
    if generation != slot.binding_generation {
        return Err("stale-graph-binding".into());
    }
    Ok(slot)
}

/// Resolve the only graph capability granted to the capture WebView: a bounded
/// page/tag quick-switch query. This is deliberately not a GraphContext route;
/// capture retains no generic read or write access to the selected graph.
pub(crate) fn capture_quick_switch_slot(
    state: &AppState,
    caller: &str,
    binding_generation: Option<u64>,
) -> Result<Arc<GraphSlot>, String> {
    if caller != "capture" {
        return Err("capture quick switch is only available to quick capture".into());
    }
    let capture = state
        .capture_graph_binding()
        .ok_or("no graph bound for quick capture")?;
    let generation = binding_generation.ok_or("missing-graph-binding")?;
    if generation != capture.binding_generation {
        return Err("stale-graph-binding".into());
    }
    let slot = slot_for_window(state, &capture.target)?;
    if slot.binding_generation != capture.binding_generation {
        return Err("stale-graph-binding".into());
    }
    Ok(slot)
}

/// Run one non-graph-semantic filesystem/config/asset operation. Managed mode
/// uses a short-lived root capability and never opens the retained parsed view.
pub(crate) fn with_filesystem_graph<T>(
    ctx: &GraphContext<'_>,
    f: impl FnOnce(&Graph) -> Result<T, String>,
) -> Result<T, String> {
    slot_for_context(ctx)?.with_filesystem_graph(f)
}

/// Run a `logseq/config.edn` write under either authority. See
/// [`GraphSlot::with_config_graph`] for why a managed binding may answer it.
pub(crate) fn with_config_graph<T>(
    ctx: &GraphContext<'_>,
    f: impl FnOnce(&Graph) -> Result<T, String>,
) -> Result<T, String> {
    slot_for_context(ctx)?.with_config_graph(f)
}

/// Run a recoverable-trash write under either authority. See
/// [`GraphSlot::with_trash_graph`] for what it does and does not cover.
pub(crate) fn with_trash_graph<T>(
    ctx: &GraphContext<'_>,
    f: impl FnOnce(&Graph) -> Result<T, String>,
) -> Result<T, String> {
    slot_for_context(ctx)?.with_trash_graph(f)
}

pub(crate) fn refresh_graph(ctx: &GraphContext<'_>) -> Result<(), String> {
    let label = ctx.window.label().to_string();
    // Refresh may migrate graph files before publishing its replacement slot.
    // Serialize the whole operation with graph loads and sparse-v2 promotion.
    let _transition = ctx.state.graph_load.lock().unwrap();
    let old = slot_for_window(&ctx.state, &label)?;
    if old.is_sparse_v2() {
        // A managed binding has no legacy graph to reopen, and the reopen below
        // would install one as a second writer. What the callers actually need
        // after a settings change is for the next read to see the new
        // configuration, so discard the read-only view and wake the watcher.
        //
        // Deliberately NOT done here: `migrate_journal_filenames_checked`. That
        // renames journal files, which is a graph-text mutation the oplog owns;
        // it stays refused until managed renames exist.
        old.refresh_filesystem_meta();
        poke_watcher(&ctx.state);
        return Ok(());
    }
    old.legacy_graph()?;
    let approved =
        crate::settings::approved_external_assets(ctx.window.app_handle(), &old.root_key);
    let graph = Graph::open_checked_with_assets(&old.root_key, approved.as_deref())
        .map_err(|e| e.to_string())?;
    graph
        .migrate_journal_filenames_checked()
        .map_err(|error| format!("journal filename migration failed: {error}"))?;
    let replacement = Arc::new(GraphSlot::refreshed(graph, &old)?);
    ctx.state.graphs.write().unwrap().bind(label, replacement)?;
    poke_watcher(&ctx.state);
    Ok(())
}

pub(crate) fn poke_watcher(state: &AppState) {
    if let Some(tx) = state.watch_ctl.lock().unwrap().as_ref() {
        let _ = tx.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(root: &Path) -> Arc<GraphSlot> {
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::create_dir_all(root.join("journals")).unwrap();
        Arc::new(GraphSlot::new(Graph::open(root), root.to_path_buf()))
    }

    fn managed_slot(root: &Path) -> Arc<GraphSlot> {
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::create_dir_all(root.join("journals")).unwrap();
        let meta = Graph::open(root).meta();
        Arc::new(GraphSlot::from_sparse_v2(
            crate::sync_runtime::SparseV2Binding::without_actor_for_test(),
            root.to_path_buf(),
            meta,
        ))
    }

    /// Every Settings toggle and the orphaned-asset cleanup were dead under
    /// managed storage: both wrote outside the oplog's document domain but were
    /// routed through the write authority that answers "may this caller touch
    /// graph text". They must work; graph text must stay refused.
    #[test]
    fn a_managed_binding_persists_settings_and_trash_but_not_graph_text() {
        let root = std::env::temp_dir().join(format!(
            "tine-managed-write-routing-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let slot = managed_slot(&root);
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/orphan.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        std::fs::write(root.join("journals/2026_08_07.md"), "- a journal day\n").unwrap();
        let conflict = "Alpha.sync-conflict-20260810-120000-DEVICE.md";
        std::fs::write(root.join("pages").join(conflict), "- peer copy\n").unwrap();

        assert!(slot.is_sparse_v2(), "fixture must be a managed binding");

        // The old route, which is what these commands used: still refused.
        assert_eq!(
            slot.legacy_graph()
                .err()
                .expect("a managed binding must never yield legacy write authority"),
            SPARSE_V2_UNSUPPORTED
        );

        // Settings.
        slot.with_config_graph(|graph| {
            graph
                .set_favorites(&["Alpha".to_owned()])
                .map_err(|error| error.to_string())
        })
        .expect("a managed binding must be able to persist a setting");
        assert!(std::fs::read_to_string(root.join("logseq/config.edn"))
            .expect("the setting must have reached config.edn")
            .contains(":favorites [\"Alpha\"]"),);

        // Orphaned-asset cleanup.
        slot.with_trash_graph(|graph| graph.trash_asset("orphan.png").map_err(|e| e.to_string()))
            .expect("a managed binding must be able to trash an orphaned asset");
        assert!(!root.join("assets/orphan.png").exists());
        slot.with_trash_graph(|graph| {
            graph
                .trash_sync_conflict(&format!("pages/{conflict}"))
                .map_err(|error| error.to_string())
        })
        .expect("a managed binding must be able to trash excluded conflict evidence");
        assert!(!root.join("pages").join(conflict).exists());

        // Graph text: still the oplog's, through every one of these routes.
        for (route, attempt) in [
            (
                "config",
                slot.with_config_graph(|graph| {
                    graph
                        .trash_journal_file("2026_08_07.md")
                        .map_err(|e| e.to_string())
                }),
            ),
            (
                "trash",
                slot.with_trash_graph(|graph| {
                    graph
                        .trash_journal_file("2026_08_07.md")
                        .map_err(|e| e.to_string())
                }),
            ),
        ] {
            assert!(
                attempt.is_err(),
                "the {route} capability must not delete graph text"
            );
        }
        assert!(
            root.join("journals/2026_08_07.md").exists(),
            "the refused journal deletion must not have touched the file"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Settings refresh updates the stored metadata from a short-lived config
    /// capability; it must not rebuild the obsolete retained parsed view.
    #[test]
    fn a_managed_settings_change_refreshes_meta_without_the_parsed_view() {
        let root = std::env::temp_dir().join(format!(
            "tine-managed-config-reopen-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let slot = managed_slot(&root);

        let before = slot.graph_meta().start_of_week;

        slot.with_config_graph(|graph| graph.set_start_of_week(3).map_err(|e| e.to_string()))
            .expect("a managed binding must be able to persist a setting");
        assert_ne!(before, 3, "fixture must actually change the value");
        assert_eq!(
            slot.graph_meta().start_of_week,
            before,
            "and so does the snapshot load_graph hands back"
        );

        slot.refresh_filesystem_meta();
        assert_eq!(
            slot.graph_meta().start_of_week,
            3,
            "a window reloading this root must not be told the old setting"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn managed_slots_have_no_retained_parsed_graph_fallback() {
        let source = include_str!("state.rs");
        let production = source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("production state source");
        for forbidden in [
            "derived_read_graph",
            "ReadGraphLease",
            "with_read_graph",
            "read_graph_cloned",
            "invalidate_derived_read_graph",
            "reopen_derived_read_graph",
            "ManagedBroadCacheTouches",
        ] {
            assert!(
                !production.contains(forbidden),
                "the removed managed broad-cache capability `{forbidden}` must not return"
            );
        }
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
        for required in [
            "state.window.app_handle().clone()",
            "state.window.label().to_string()",
            "state.binding_generation",
            "drop(state)",
        ] {
            assert!(
                helper.contains(required),
                "owned command context must retain `{required}` before await"
            );
        }
    }

    #[test]
    fn normal_slots_are_legacy_only_and_authority_types_are_send_sync() {
        let base =
            std::env::temp_dir().join(format!("tine-slot-authority-{}", uuid::Uuid::new_v4()));
        let slot = graph(&base);

        assert!(matches!(&slot.authority, GraphAuthority::Legacy(_)));
        assert!(!slot.is_sparse_v2());
        assert!(slot.legacy_graph().is_ok());

        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GraphAuthority>();
        assert_send_sync::<GraphSlot>();
        let _sparse_runtime: for<'a> fn(
            &'a GraphSlot,
        )
            -> Option<&'a tine_core::sync_runtime::SyncRuntimeHandle> = GraphSlot::sparse_runtime;

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn sparse_type_level_legacy_gate_is_stable_and_refresh_is_legacy_only() {
        assert_eq!(
            require_legacy_authority(GraphAuthorityKind::SparseV2),
            Err(SPARSE_V2_UNSUPPORTED.into())
        );
        assert_eq!(require_legacy_authority(GraphAuthorityKind::Legacy), Ok(()));

        // `GraphSlot::refreshed` starts with the same legacy-only gate before
        // it can construct a replacement, so a sparse binding cannot be
        // replaced/revoked by config refresh.
        let source = include_str!("state.rs");
        assert!(source.contains("old.legacy_graph()?"));
        assert!(source.contains("wait_for_legacy_drain"));
        let public_graph_field = ["pub(crate) graph", ": Arc<Graph>"].concat();
        assert!(!source.contains(&public_graph_field));
        assert!(!include_str!("commands.rs").contains("from_sparse_v2"));
        assert!(include_str!("graph.rs").contains("from_sparse_v2"));
    }

    #[test]
    fn explicit_graph_activation_updates_capture_routing_idempotently() {
        let state = AppState {
            graphs: RwLock::new(GraphRegistry::default()),
            graph_load: Mutex::new(()),
            watch_ctl: Mutex::new(None),
            last_focused: Mutex::new(Some("graph-1".into())),
            capture_graph: Mutex::new(None),
            startup_recovery: Mutex::new(HashMap::new()),
            sync_runtime: crate::sync_runtime::SyncRuntimeFacade::default(),
            #[cfg(desktop)]
            next_window: AtomicU64::new(2),
        };

        assert!(state.note_focused("main"));
        assert_eq!(state.last_focused.lock().unwrap().as_deref(), Some("main"));
        assert!(!state.note_focused("main"));
    }

    #[test]
    fn capture_binding_retains_the_selected_graph_lease() {
        let state = AppState {
            graphs: RwLock::new(GraphRegistry::default()),
            graph_load: Mutex::new(()),
            watch_ctl: Mutex::new(None),
            last_focused: Mutex::new(Some("main".into())),
            capture_graph: Mutex::new(None),
            startup_recovery: Mutex::new(HashMap::new()),
            sync_runtime: crate::sync_runtime::SyncRuntimeFacade::default(),
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

    #[test]
    fn same_root_refresh_preserves_frontend_binding_lease() {
        let base = std::env::temp_dir().join(format!("tine-slot-refresh-{}", std::process::id()));
        let old = graph(&base);
        old.warm_done
            .store(true, std::sync::atomic::Ordering::Release);
        old.warm_generation
            .store(7, std::sync::atomic::Ordering::Release);

        let replacement = GraphSlot::refreshed(Graph::open(&base), &old).unwrap();

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
    fn queued_owned_command_generation_is_rejected_after_graph_replacement() {
        let base = std::env::temp_dir().join(format!("tine-slot-stale-{}", uuid::Uuid::new_v4()));
        let old_root = base.join("old");
        let new_root = base.join("new");
        let state = AppState {
            graphs: RwLock::new(GraphRegistry::default()),
            graph_load: Mutex::new(()),
            watch_ctl: Mutex::new(None),
            last_focused: Mutex::new(Some("main".into())),
            capture_graph: Mutex::new(None),
            startup_recovery: Mutex::new(HashMap::new()),
            sync_runtime: crate::sync_runtime::SyncRuntimeFacade::default(),
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
                .unwrap(),
            "stale-graph-binding"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn legacy_retirement_refuses_new_work_and_proves_every_retained_lease_drained() {
        let base = std::env::temp_dir().join(format!("tine-slot-drain-{}", uuid::Uuid::new_v4()));
        let slot = graph(&base);
        let retained = slot.legacy_graph_cloned().unwrap();

        slot.begin_legacy_retirement().unwrap();
        assert_eq!(
            slot.legacy_graph().err().as_deref(),
            Some("legacy graph authority is retiring")
        );
        assert!(slot
            .wait_for_legacy_drain(Duration::from_millis(1))
            .is_err());

        drop(retained);
        slot.wait_for_legacy_drain(Duration::from_secs(1)).unwrap();
        slot.cancel_legacy_retirement().unwrap();
        assert!(slot.legacy_graph().is_ok());
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
