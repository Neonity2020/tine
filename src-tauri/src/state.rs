use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};
use tauri::ipc::{CommandArg, CommandItem, InvokeError};
use tauri::{Runtime, State, WebviewWindow};
use tine_core::model::Graph;

pub(crate) type WindowKey = String;

pub(crate) struct GraphSlot {
    pub(crate) graph: Arc<Graph>,
    pub(crate) root_key: PathBuf,
    pub(crate) warm_done: AtomicBool,
    pub(crate) warm_generation: AtomicU64,
}

impl GraphSlot {
    pub(crate) fn new(graph: Graph, root_key: PathBuf) -> Self {
        Self {
            graph: Arc::new(graph),
            root_key,
            warm_done: AtomicBool::new(false),
            warm_generation: AtomicU64::new(0),
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

    pub(crate) fn bind(&mut self, window: WindowKey, slot: Arc<GraphSlot>) -> Result<(), String> {
        if let Some(owner) = self.by_root.get(&slot.root_key) {
            if owner != &window {
                return Err(format!(
                    "graph {} is already owned by window {owner}",
                    slot.root_key.display()
                ));
            }
        }
        if let Some(old) = self.by_window.insert(window.clone(), slot.clone()) {
            self.by_root.remove(&old.root_key);
        }
        self.by_root.insert(slot.root_key.clone(), window);
        Ok(())
    }

    pub(crate) fn remove(&mut self, window: &str) -> Option<Arc<GraphSlot>> {
        let slot = self.by_window.remove(window)?;
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
    #[cfg(desktop)]
    pub(crate) next_window: AtomicU64,
}

pub(crate) struct GraphContext<'a, R: Runtime = tauri::Wry> {
    pub(crate) state: State<'a, AppState>,
    pub(crate) window: WebviewWindow<R>,
}

impl<'r, 'de: 'r, R: Runtime> CommandArg<'de, R> for GraphContext<'r, R> {
    fn from_command(command: CommandItem<'de, R>) -> Result<Self, InvokeError> {
        let state: State<'r, AppState> = command
            .message
            .state_ref()
            .try_get()
            .ok_or_else(|| InvokeError::from("AppState is not managed"))?;
        let window = WebviewWindow::<R>::from_command(command)?;
        Ok(Self { state, window })
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

pub(crate) fn with_graph<T>(
    ctx: &GraphContext<'_>,
    f: impl FnOnce(&Graph) -> Result<T, String>,
) -> Result<T, String> {
    let slot = slot_for_window(&ctx.state, ctx.window.label())?;
    f(&slot.graph)
}

pub(crate) fn refresh_graph(ctx: &GraphContext<'_>) -> Result<(), String> {
    let label = ctx.window.label().to_string();
    let old = slot_for_window(&ctx.state, &label)?;
    let graph = Graph::open(&old.root_key);
    graph.migrate_journal_filenames();
    let replacement = Arc::new(GraphSlot::new(graph, old.root_key.clone()));
    replacement.warm_done.store(
        old.warm_done.load(std::sync::atomic::Ordering::Acquire),
        std::sync::atomic::Ordering::Release,
    );
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

    #[test]
    fn registry_keeps_window_and_root_indices_in_sync() {
        let base = std::env::temp_dir().join(format!("tine-registry-{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        let mut registry = GraphRegistry::default();
        registry.bind("main".into(), graph(&a)).unwrap();
        assert_eq!(registry.owner(&a).as_deref(), Some("main"));
        registry.bind("main".into(), graph(&b)).unwrap();
        assert!(registry.owner(&a).is_none());
        assert_eq!(registry.owner(&b).as_deref(), Some("main"));
        registry.remove("main");
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
}
