use super::*;
use crate::state::{slot_for_bound_window, GraphRegistry, GraphSlot};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Mutex, RwLock};
use tine_core::model::Graph;

fn state_with_selected_graph() -> (AppState, PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "tine-capture-quick-switch-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let selected = base.join("selected");
    let other = base.join("other");
    for (root, page) in [
        (&selected, "Selected Capture Target"),
        (&other, "Other Target"),
    ] {
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::create_dir_all(root.join("journals")).unwrap();
        std::fs::write(root.join("pages").join(format!("{page}.md")), "- fixture\n").unwrap();
    }
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
    let selected_slot = Arc::new(GraphSlot::new(Graph::open(&selected), selected.clone()));
    let generation = selected_slot.binding_generation;
    state
        .graphs
        .write()
        .unwrap()
        .bind("main".into(), selected_slot)
        .unwrap();
    state
        .graphs
        .write()
        .unwrap()
        .bind(
            "other".into(),
            Arc::new(GraphSlot::new(Graph::open(&other), other)),
        )
        .unwrap();
    state.bind_capture_graph("main".into(), generation);
    (state, base)
}

#[test]
fn returns_candidates_from_the_selected_capture_graph() {
    let (state, base) = state_with_selected_graph();
    let generation = state.capture_graph_binding().unwrap().binding_generation;
    let result =
        capture_quick_switch_for(&state, "capture", Some(generation), "Selected Capture", 8)
            .unwrap();
    assert!(result
        .iter()
        .any(|page| page.name == "Selected Capture Target"));
    assert!(!result.iter().any(|page| page.name == "Other Target"));
    std::fs::remove_dir_all(base).unwrap();
}

/// GH #543, R6-02: capture's candidates come from the current graph. A
/// query that lands on a graph a refresh just retired waits for the
/// replacement instead of parsing the retired graph's page set.
#[test]
fn a_capture_query_on_a_retired_graph_is_answered_by_its_replacement() {
    let (state, base) = state_with_selected_graph();
    let state = Arc::new(state);
    let generation = state.capture_graph_binding().unwrap().binding_generation;
    let old = slot_for_bound_window(&state, "main", Some(generation)).unwrap();
    old.graph().retire();
    let binder = std::thread::spawn({
        let state = Arc::clone(&state);
        move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
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
    let result =
        capture_quick_switch_for(&state, "capture", Some(generation), "Selected Capture", 8)
            .unwrap();
    let elapsed = started.elapsed();
    binder.join().unwrap();
    assert!(result
        .iter()
        .any(|page| page.name == "Selected Capture Target"));
    assert!(
        elapsed >= std::time::Duration::from_millis(200),
        "the retired graph must not answer by parsing itself; the replacement answers"
    );
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn rejects_a_stale_capture_binding_generation() {
    let (state, base) = state_with_selected_graph();
    let generation = state.capture_graph_binding().unwrap().binding_generation;
    assert_eq!(
        capture_quick_switch_for(&state, "capture", Some(generation + 1), "Selected", 8)
            .unwrap_err()
            .to_string(),
        "stale-graph-binding"
    );
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn rejects_non_capture_callers() {
    let (state, base) = state_with_selected_graph();
    let generation = state.capture_graph_binding().unwrap().binding_generation;
    assert_eq!(
        capture_quick_switch_for(&state, "main", Some(generation), "Selected", 8)
            .unwrap_err()
            .to_string(),
        "capture quick switch is only available to quick capture"
    );
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn capture_binding_never_grants_generic_graphcontext_mutation_access() {
    let (state, base) = state_with_selected_graph();
    let generation = state.capture_graph_binding().unwrap().binding_generation;
    // `save_page` and other mutations resolve through GraphContext, which
    // uses this normal window-slot path and therefore has no capture fallback.
    assert_eq!(
        slot_for_bound_window(&state, "capture", Some(generation))
            .err()
            .unwrap()
            .to_string(),
        "no graph loaded for window capture"
    );
    std::fs::remove_dir_all(base).unwrap();
}
