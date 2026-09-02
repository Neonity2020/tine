use crate::state::{slot_for_context, GraphContext};
use std::path::PathBuf;
use std::sync::OnceLock;
use tauri::Manager;

pub(crate) const NATIVE_FRAME_KEY: &str = "native_window_frame";
static NATIVE_FRAME_ACTIVE: OnceLock<bool> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct KnownGraph {
    pub(crate) path: String,
    pub(crate) name: String,
}

// --- local app settings (outside the graph): currently just the backup keep
// count. A tiny JSON file in the OS app-data dir. ---
pub(crate) fn settings_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("tine-settings.json"))
}

fn app_bool_at(path: &std::path::Path, key: &str, default: bool) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok())
        .and_then(|value| value.get(key).and_then(serde_json::Value::as_bool))
        .unwrap_or(default)
}

/// Freeze the native-frame preference before Tauri constructs any windows.
/// Tao does not support changing Linux decorations on an existing window, so
/// every window created by this process must use the same startup value.
pub(crate) fn init_native_frame_active() -> bool {
    *NATIVE_FRAME_ACTIVE.get_or_init(|| {
        crate::migrate_identifier::current_app_data_dir()
            .map(|dir| app_bool_at(&dir.join("tine-settings.json"), NATIVE_FRAME_KEY, false))
            .unwrap_or(false)
    })
}

pub(crate) fn native_frame_active() -> bool {
    init_native_frame_active()
}
/// Serializes ALL device-settings (tine-settings.json) writers; every `set_*` below
/// goes through `update_settings`, which routes to the shared `tine_core` atomic_update
/// (audit M1): the JSON is read-modify-written under this lock + atomically (temp +
/// fsync + rename), so a crash can't truncate it, a concurrent `set_*` can't clobber
/// another's key, and a transient read error aborts instead of resetting all prefs.
static SETTINGS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Strict, fallible variant used by the signed registry cache. Unlike the
/// legacy generic preference writer, this must never rebuild a malformed
/// settings file from `{}`: doing so could erase unrelated device settings and
/// turn a cache-storage failure into apparent success.
pub(crate) fn update_settings_strict_at(
    path: &std::path::Path,
    mutate: impl Fn(&mut serde_json::Value) -> Result<(), String>,
) -> Result<(), String> {
    tine_core::model::atomic_update(path, &SETTINGS_LOCK, |content| {
        let mut json: serde_json::Value = serde_json::from_str(content)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if !json.is_object() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "device settings root is not an object",
            ));
        }
        mutate(&mut json)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        serde_json::to_string_pretty(&json)
            .map(|mut text| {
                text.push('\n');
                text
            })
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })
    .map_err(|error| error.to_string())
}

/// Merge one or more keys into the device-settings JSON, durably. `mutate` edits the
/// parsed object (an unparseable existing file is treated as `{}`, the prior behavior).
pub(crate) fn update_settings(
    app: &tauri::AppHandle,
    mutate: impl Fn(&mut serde_json::Value),
) -> Result<(), String> {
    let p = settings_path(app).ok_or("no app-data dir")?;
    tine_core::model::atomic_update(&p, &SETTINGS_LOCK, |content| {
        let mut json: serde_json::Value =
            serde_json::from_str(content).unwrap_or_else(|_| serde_json::json!({}));
        mutate(&mut json);
        serde_json::to_string_pretty(&json)
            .map(|mut s| {
                s.push('\n');
                s
            })
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    })
    .map_err(|e| e.to_string())
}

/// Stable installation identity for managed sync. It lives in device-local app
/// settings, never in the provider-shared graph: two installations writing the
/// same device stream would violate the one-writer invariant. Loro peer ids are
/// session-scoped separately; this UUID names the installation directory only.
pub(crate) fn managed_sync_device_id(app: &tauri::AppHandle) -> Result<uuid::Uuid, String> {
    let path = settings_path(app).ok_or("no app-data dir")?;
    managed_sync_device_id_at(&path)
}

fn managed_sync_device_id_at(path: &std::path::Path) -> Result<uuid::Uuid, String> {
    let chosen = std::sync::Mutex::new(None);
    update_settings_strict_at(path, |json| {
        let id = match json.get("managed_sync_device_id") {
            None => uuid::Uuid::new_v4(),
            Some(value) => value
                .as_str()
                .ok_or_else(|| "managed_sync_device_id must be a UUID string".to_string())
                .and_then(|value| {
                    uuid::Uuid::parse_str(value)
                        .map_err(|_| "managed_sync_device_id is not a valid UUID".to_string())
                })?,
        };
        json["managed_sync_device_id"] = serde_json::Value::String(id.to_string());
        *chosen.lock().unwrap() = Some(id);
        Ok(())
    })?;
    chosen
        .into_inner()
        .map_err(|_| "managed sync device-id lock poisoned".to_string())?
        .ok_or_else(|| "managed sync device id was not written".to_string())
}

fn graph_display_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn parse_known_graphs(json: &serde_json::Value) -> Vec<KnownGraph> {
    json.get("known_graphs")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn remember_graph_json(json: &mut serde_json::Value, path: &str) {
    let mut graphs = parse_known_graphs(json);
    graphs.retain(|graph| graph.path != path);
    graphs.insert(
        0,
        KnownGraph {
            path: path.to_string(),
            name: graph_display_name(path),
        },
    );
    json["known_graphs"] = serde_json::to_value(graphs).unwrap_or_default();
    json["last_graph_path"] = serde_json::Value::String(path.to_string());
}

fn forget_graph_json(json: &mut serde_json::Value, path: &str) {
    let mut graphs = parse_known_graphs(json);
    graphs.retain(|graph| graph.path != path);
    json["known_graphs"] = serde_json::to_value(graphs).unwrap_or_default();
}

fn external_assets_approvals(
    json: &serde_json::Value,
) -> serde_json::Map<String, serde_json::Value> {
    json.get("external_assets_approvals")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// The trust grant is deliberately device-local and keyed by BOTH canonical
/// graph root and canonical external target. It never enters graph content, and
/// a retargeted symlink/junction therefore cannot inherit the old grant.
pub(crate) fn approved_external_assets(
    app: &tauri::AppHandle,
    graph_root: &std::path::Path,
) -> Option<PathBuf> {
    let key = graph_root.display().to_string();
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|json| {
            external_assets_approvals(&json)
                .get(&key)
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
        })
}

pub(crate) fn remember_external_assets_approval(
    app: &tauri::AppHandle,
    graph_root: &std::path::Path,
    assets_root: &std::path::Path,
) -> Result<(), String> {
    let graph = graph_root.display().to_string();
    let assets = assets_root.display().to_string();
    update_settings(app, |json| {
        remember_external_assets_approval_json(json, &graph, &assets)
    })
}

fn remember_external_assets_approval_json(json: &mut serde_json::Value, graph: &str, assets: &str) {
    let mut approvals = external_assets_approvals(json);
    approvals.insert(
        graph.to_string(),
        serde_json::Value::String(assets.to_string()),
    );
    json["external_assets_approvals"] = serde_json::Value::Object(approvals);
}

pub(crate) fn remember_graph(app: &tauri::AppHandle, path: &str) -> Result<(), String> {
    update_settings(app, |json| remember_graph_json(json, path))
}

#[tauri::command]
pub(crate) fn list_known_graphs(app: tauri::AppHandle) -> Vec<KnownGraph> {
    settings_path(&app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .map(|json| parse_known_graphs(&json))
        .unwrap_or_default()
}

#[tauri::command]
pub(crate) fn forget_known_graph(path: String, app: tauri::AppHandle) -> Result<(), String> {
    update_settings(&app, |json| forget_graph_json(json, &path))
}

/// Show a known graph's root folder in the OS file manager, for the graph
/// switcher's per-row context menu. Unlike `open_page_file` this names a graph
/// that is NOT bound to any window, so it cannot go through a graph slot.
///
/// The path is required to be one the app already remembers. The caller only
/// ever has a row it was handed by `list_known_graphs`, so this costs nothing
/// in practice; it keeps an arbitrary caller-chosen path out of the reveal argv.
#[tauri::command]
pub(crate) fn reveal_known_graph(path: String, app: tauri::AppHandle) -> Result<(), String> {
    if !list_known_graphs(app)
        .iter()
        .any(|known| known.path == path)
    {
        return Err("that graph is not in the known-graph list".into());
    }
    #[cfg(desktop)]
    {
        crate::platform::reveal_page_source(std::path::Path::new(&path))
    }
    #[cfg(not(desktop))]
    {
        Err("showing a graph folder is available on desktop only".into())
    }
}

pub(crate) fn last_graph_path(app: &tauri::AppHandle) -> Option<String> {
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|json| {
            json.get("last_graph_path")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
}

/// Quick-capture Enter behaviour (app-level, in tine-settings.json): true → a
/// plain Enter files the capture; false (default) → Enter makes a new block and
/// Cmd/Ctrl+Enter files.
fn capture_enter_files(app: &tauri::AppHandle) -> bool {
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("capture_enter_files").and_then(|x| x.as_bool()))
        .unwrap_or(false)
}

#[tauri::command]
pub(crate) fn get_capture_enter_files(app: tauri::AppHandle) -> bool {
    capture_enter_files(&app)
}

#[tauri::command]
pub(crate) fn set_capture_enter_files(value: bool, app: tauri::AppHandle) -> Result<(), String> {
    update_settings(&app, |json| {
        json["capture_enter_files"] = serde_json::Value::Bool(value);
    })
}

/// `[[`/`#` autocomplete default action (app-level, in tine-settings.json):
/// true → Enter links the first match; false (default, OG) → Enter creates a new
/// page/tag unless an exact match exists. A workflow preference, device-local.
fn link_first_match(app: &tauri::AppHandle) -> bool {
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("link_first_match").and_then(|x| x.as_bool()))
        .unwrap_or(false)
}

#[tauri::command]
pub(crate) fn get_link_first_match(app: tauri::AppHandle) -> bool {
    link_first_match(&app)
}

#[tauri::command]
pub(crate) fn set_link_first_match(value: bool, app: tauri::AppHandle) -> Result<(), String> {
    update_settings(&app, |json| {
        json["link_first_match"] = serde_json::Value::Bool(value);
    })
}

/// Smooth-scrolling preference (app-level, in tine-settings.json). Experimental,
/// default false. Read at startup by the frontend to (re-)install Lenis. Device-
/// local because it's a feel preference, not graph data.
fn smooth_scroll(app: &tauri::AppHandle) -> bool {
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("smooth_scroll").and_then(|x| x.as_bool()))
        .unwrap_or(false)
}

#[tauri::command]
pub(crate) fn get_smooth_scroll(app: tauri::AppHandle) -> bool {
    smooth_scroll(&app)
}

#[tauri::command]
pub(crate) fn set_smooth_scroll(value: bool, app: tauri::AppHandle) -> Result<(), String> {
    update_settings(&app, |json| {
        json["smooth_scroll"] = serde_json::Value::Bool(value);
    })
}

/// Generic device-local boolean preference (tine-settings.json). For simple
/// behavior toggles that don't each warrant bespoke read/get/set code — the caller
/// supplies the key and the default. (Used by the copy-behavior options.)
#[tauri::command]
pub(crate) fn get_app_bool(key: String, default: bool, app: tauri::AppHandle) -> bool {
    settings_path(&app)
        .map(|path| app_bool_at(&path, &key, default))
        .unwrap_or(default)
}

#[tauri::command]
pub(crate) fn set_app_bool(key: String, value: bool, app: tauri::AppHandle) -> Result<(), String> {
    update_settings(&app, |json| {
        json[&key] = serde_json::Value::Bool(value);
    })
}

/// Generic device-local STRING preference (tine-settings.json) — the string twin of
/// `get_app_bool`. Used for the asset-filename format template (a personal naming
/// preference, read once at startup and applied in the frontend tokenizer).
#[tauri::command]
pub(crate) fn get_app_string(key: String, default: String, app: tauri::AppHandle) -> String {
    settings_path(&app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get(&key).and_then(|x| x.as_str().map(str::to_string)))
        .unwrap_or(default)
}

#[tauri::command]
pub(crate) fn set_app_string(
    key: String,
    value: String,
    app: tauri::AppHandle,
) -> Result<(), String> {
    update_settings(&app, |json| {
        json[&key] = serde_json::Value::String(value.clone());
    })
}

/// Path to the persisted UI session (open tabs / active tab / zoom). This is
/// app-level window state, not graph content, so it lives next to the settings
/// file in the app-data dir. The backend owns atomic structured persistence and
/// makes it independent of a particular WebView's localStorage namespace.
fn legacy_session_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("tine-session.json"))
}

fn session_id(root: &std::path::Path) -> String {
    // Stable FNV-1a over the canonical path. The readable basename is cosmetic;
    // the hash prevents two same-named graphs in different folders colliding.
    let text = root.to_string_lossy();
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let name = root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("graph")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>();
    format!("{name}-{hash:016x}.json")
}

fn session_path(app: &tauri::AppHandle, root: &std::path::Path) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("sessions").join(session_id(root)))
}

fn workspaces_path(app: &tauri::AppHandle, root: &std::path::Path) -> Option<PathBuf> {
    let id = session_id(root);
    let stem = id.strip_suffix(".json").unwrap_or(&id);
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("sessions").join(format!("{stem}-workspaces.json")))
}

fn blank_session_json() -> serde_json::Value {
    serde_json::json!({
        "tabs": [{
            "history": [{ "kind": "journals" }],
            "pos": 0,
            "pinned": false
        }],
        "activeIndex": 0
    })
}

fn migrated_workspaces_json(session: Option<&str>) -> String {
    let blob = session
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .unwrap_or_else(blank_session_json);
    serde_json::to_string(&serde_json::json!({
        "version": 1,
        "activeId": "default",
        "workspaces": [{ "id": "default", "name": "", "blob": blob }]
    }))
    .expect("workspace migration JSON is serializable")
}

fn validate_workspaces_json(data: &str) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(data).map_err(|e| e.to_string())?;
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err("workspace registry version must be 1".into());
    }
    let active = value
        .get("activeId")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or("workspace registry requires an activeId")?;
    let entries = value
        .get("workspaces")
        .and_then(serde_json::Value::as_array)
        .filter(|entries| !entries.is_empty())
        .ok_or("workspace registry requires at least one workspace")?;
    if !entries.iter().any(|entry| {
        entry.get("id").and_then(serde_json::Value::as_str) == Some(active)
            && entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some()
            && entry.get("blob").is_some_and(serde_json::Value::is_object)
    }) {
        return Err("active workspace is missing or invalid".into());
    }
    Ok(())
}

static WORKSPACES_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn atomic_write_workspaces(path: &std::path::Path, data: &str) -> Result<(), String> {
    let parent = path.parent().ok_or("workspace registry has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    // Named audited app-private publication protocol (I-1/I-2): the shared
    // primitive uses a unique create-new temp, file barrier, atomic rename,
    // temp cleanup on failure, and strict directory-barrier error policy.
    tine_core::model::atomic_write(path, data.as_bytes()).map_err(|e| e.to_string())
}

fn load_workspaces_at(path: &std::path::Path, session: &std::path::Path) -> Result<String, String> {
    let _guard = WORKSPACES_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match std::fs::read_to_string(path) {
        Ok(data) => {
            validate_workspaces_json(&data)?;
            Ok(data)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let legacy = std::fs::read_to_string(session).ok();
            let data = migrated_workspaces_json(legacy.as_deref());
            atomic_write_workspaces(path, &data)?;
            Ok(data)
        }
        Err(error) => Err(error.to_string()),
    }
}

fn save_workspaces_at(path: &std::path::Path, data: &str) -> Result<(), String> {
    validate_workspaces_json(data)?;
    let _guard = WORKSPACES_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    atomic_write_workspaces(path, data)
}

pub(crate) fn load_workspaces(
    app: tauri::AppHandle,
    state: GraphContext<'_>,
) -> Result<String, String> {
    let slot = slot_for_context(&state)?;
    let session = session_path(&app, &slot.root_key).ok_or("no app-data dir")?;
    let path = workspaces_path(&app, &slot.root_key).ok_or("no app-data dir")?;
    load_workspaces_at(&path, &session)
}

pub(crate) fn save_workspaces(
    data: String,
    app: tauri::AppHandle,
    state: GraphContext<'_>,
) -> Result<(), String> {
    let slot = slot_for_context(&state)?;
    let path = workspaces_path(&app, &slot.root_key).ok_or("no app-data dir")?;
    save_workspaces_at(&path, &data)
}

static SESSION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn migrate_legacy_session_at(
    path: &std::path::Path,
    legacy: Option<&std::path::Path>,
) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    let Some(legacy) = legacy.filter(|legacy| legacy.exists()) else {
        return Ok(());
    };
    let parent = path.parent().ok_or("session file has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    std::fs::rename(legacy, path).map_err(|e| e.to_string())?;
    // Named audited legacy-migration protocol (I-1/I-2, DUP-5): report real
    // directory-barrier failures rather than falsely acknowledging a rename
    // that may disappear after power loss. Unsupported barriers are tolerated
    // by the shared helper.
    tine_core::model::sync_dir_for_rename(parent).map_err(|e| e.to_string())
}

fn load_session_at(
    path: &std::path::Path,
    legacy: Option<&std::path::Path>,
) -> Result<Option<String>, String> {
    let _guard = SESSION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    migrate_legacy_session_at(path, legacy)?;
    Ok(std::fs::read_to_string(path).ok())
}

fn save_session_at(path: &std::path::Path, data: &str) -> Result<(), String> {
    let _guard = SESSION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let parent = path.parent().ok_or("session file has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    // Named audited app-private publication protocol (I-1/I-2). The lock
    // serializes tab-action bursts; atomic_write additionally gives each call
    // a unique create-new temp and the complete file + directory barriers.
    tine_core::model::atomic_write(path, data.as_bytes()).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn load_session(
    app: tauri::AppHandle,
    state: GraphContext<'_>,
) -> Result<Option<String>, String> {
    let slot = slot_for_context(&state)?;
    let path = session_path(&app, &slot.root_key).ok_or("no app-data dir")?;
    let legacy = legacy_session_path(&app);
    load_session_at(&path, legacy.as_deref())
}

#[tauri::command]
pub(crate) fn save_session(
    data: String,
    app: tauri::AppHandle,
    state: GraphContext<'_>,
) -> Result<(), String> {
    let slot = slot_for_context(&state)?;
    let p = session_path(&app, &slot.root_key).ok_or("no app-data dir")?;
    save_session_at(&p, &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_private_durable_publications_stay_on_named_audited_paths() {
        let production = include_str!("settings.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("settings.rs has production source before its tests");
        let bare_whole_file_writes = production
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("std::fs::write("))
            .map(|(index, line)| format!("{}: {}", index + 1, line.trim()))
            .collect::<Vec<_>>();

        assert!(
            bare_whole_file_writes.is_empty(),
            "I-1/I-2 require every app-private durable-state publication in settings.rs to use a named audited path; bare whole-file writes found:\n{}. Use the blessed atomic_write_workspaces / tine_core::model::atomic_write exemplar.",
            bare_whole_file_writes.join("\n")
        );

        let raw_renames = production
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("std::fs::rename("))
            .map(|(index, line)| format!("{}: {}", index + 1, line.trim()))
            .collect::<Vec<_>>();
        let migration_protocol = production
            .split("fn migrate_legacy_session_at(")
            .nth(1)
            .and_then(|tail| tail.split("fn load_session_at(").next())
            .expect("legacy session rename has a named audited protocol");
        assert_eq!(
            raw_renames.len(),
            1,
            "I-1/I-2 permit only the named audited legacy-session rename in settings.rs; raw renames found:\n{}. Use the blessed atomic_write_workspaces / tine_core::model::atomic_write exemplar.",
            raw_renames.join("\n")
        );
        assert!(
            migration_protocol.contains("std::fs::rename(")
                && migration_protocol.contains("tine_core::model::sync_dir_for_rename("),
            "I-1/I-2 require the named legacy-session migration to pair rename with the blessed strict directory barrier"
        );
    }

    #[test]
    fn session_load_ignores_a_stale_atomic_temp() {
        let temp = tempfile::tempdir().unwrap();
        let session = temp.path().join("sessions/graph.json");
        let current = r#"{"tabs":[{"id":"complete"}],"activeIndex":0}"#;
        save_session_at(&session, current).unwrap();

        let stale = session.parent().unwrap().join(".graph.json.999999.0.tmp");
        std::fs::write(&stale, br#"{"tabs":["#).unwrap();

        assert_eq!(
            load_session_at(&session, None).unwrap().as_deref(),
            Some(current)
        );
        assert!(
            stale.exists(),
            "load ignores unique hidden temps from a prior crash"
        );
    }

    #[test]
    fn concurrent_session_save_burst_keeps_a_complete_last_writer_and_no_temps() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};

        const WRITERS: usize = 16;
        let temp = tempfile::tempdir().unwrap();
        let session = Arc::new(temp.path().join("sessions/graph.json"));
        let initial =
            serde_json::json!({"writer": "initial", "padding": "x".repeat(4096)}).to_string();
        save_session_at(&session, &initial).unwrap();

        let payloads = Arc::new(
            (0..WRITERS)
                .map(|writer| {
                    serde_json::json!({"writer": writer, "padding": "x".repeat(4096)}).to_string()
                })
                .collect::<Vec<_>>(),
        );
        let start = Arc::new(Barrier::new(WRITERS + 1));
        let remaining = Arc::new(AtomicUsize::new(WRITERS));
        let observer_path = Arc::clone(&session);
        let observer_remaining = Arc::clone(&remaining);
        let observer = std::thread::spawn(move || {
            let mut observations = 0;
            loop {
                if let Ok(bytes) = std::fs::read_to_string(observer_path.as_ref()) {
                    serde_json::from_str::<serde_json::Value>(&bytes)
                        .expect("atomic replacement never exposes a truncated session");
                    observations += 1;
                }
                if observer_remaining.load(Ordering::Acquire) == 0 {
                    break;
                }
            }
            observations
        });

        let writers = (0..WRITERS)
            .map(|writer| {
                let session = Arc::clone(&session);
                let payloads = Arc::clone(&payloads);
                let start = Arc::clone(&start);
                let remaining = Arc::clone(&remaining);
                std::thread::spawn(move || {
                    start.wait();
                    save_session_at(&session, &payloads[writer]).unwrap();
                    remaining.fetch_sub(1, Ordering::Release);
                })
            })
            .collect::<Vec<_>>();
        start.wait();
        for writer in writers {
            writer.join().unwrap();
        }
        assert!(observer.join().unwrap() > 0);

        let final_bytes = std::fs::read_to_string(session.as_ref()).unwrap();
        serde_json::from_str::<serde_json::Value>(&final_bytes).unwrap();
        assert!(
            payloads.iter().any(|payload| payload == &final_bytes),
            "the final session is one complete last-writer payload"
        );
        let temp_prefix = format!(".{}.", session.file_name().unwrap().to_string_lossy());
        let stray_temps = std::fs::read_dir(session.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(&temp_prefix) && name.ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(
            stray_temps.is_empty(),
            "completed saves reclaim temps: {stray_temps:?}"
        );
    }

    #[test]
    fn legacy_session_migration_moves_complete_bytes_to_the_scoped_name() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("session.json");
        let session = temp.path().join("sessions/graph.json");
        let prior = r#"{"tabs":[{"id":"legacy"}],"activeIndex":0}"#;
        std::fs::write(&legacy, prior).unwrap();

        assert_eq!(
            load_session_at(&session, Some(&legacy)).unwrap().as_deref(),
            Some(prior)
        );
        assert!(!legacy.exists());
        assert_eq!(std::fs::read_to_string(session).unwrap(), prior);
    }

    #[test]
    fn known_graphs_are_deduplicated_mru_and_removable() {
        let mut json = serde_json::json!({});
        remember_graph_json(&mut json, "/graphs/alpha");
        remember_graph_json(&mut json, "/other/beta");
        remember_graph_json(&mut json, "/graphs/alpha");
        assert_eq!(
            parse_known_graphs(&json),
            vec![
                KnownGraph {
                    path: "/graphs/alpha".into(),
                    name: "alpha".into()
                },
                KnownGraph {
                    path: "/other/beta".into(),
                    name: "beta".into()
                },
            ]
        );
        assert_eq!(json["last_graph_path"], "/graphs/alpha");
        forget_graph_json(&mut json, "/graphs/alpha");
        assert_eq!(parse_known_graphs(&json).len(), 1);
        assert_eq!(json["last_graph_path"], "/graphs/alpha");
    }

    #[test]
    fn session_ids_separate_same_named_graphs() {
        assert_ne!(
            session_id(std::path::Path::new("/one/graph")),
            session_id(std::path::Path::new("/two/graph"))
        );
    }

    #[test]
    fn workspace_migration_preserves_the_session_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let session = temp.path().join("graph.json");
        let registry = temp.path().join("graph-workspaces.json");
        let prior = r#"{"tabs":[{"history":[{"kind":"page","name":"Prior","pageKind":"page"}],"pos":0,"pinned":true}],"activeIndex":0}"#;
        std::fs::write(&session, prior).unwrap();

        let first = load_workspaces_at(&registry, &session).unwrap();
        let first_value: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(first_value["activeId"], "default");
        assert_eq!(first_value["workspaces"][0]["name"], "");
        assert_eq!(
            first_value["workspaces"][0]["blob"],
            serde_json::from_str::<serde_json::Value>(prior).unwrap()
        );
        assert_eq!(std::fs::read_to_string(&session).unwrap(), prior);

        let bytes = std::fs::read(&registry).unwrap();
        let modified = std::fs::metadata(&registry).unwrap().modified().unwrap();
        let second = load_workspaces_at(&registry, &session).unwrap();
        assert_eq!(second, first);
        assert_eq!(std::fs::read(&registry).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&registry).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(std::fs::read_to_string(&session).unwrap(), prior);
    }

    #[test]
    fn workspace_registry_full_cycle_keeps_graph_page_bytes_and_mtime_identical() {
        let temp = tempfile::tempdir().unwrap();
        let graph = temp.path().join("tine-test");
        let pages = graph.join("pages");
        std::fs::create_dir_all(&pages).unwrap();
        let page = pages.join("byte-identical.md");
        std::fs::write(&page, "- original graph bytes\n").unwrap();
        let before_bytes = std::fs::read(&page).unwrap();
        let before_modified = std::fs::metadata(&page).unwrap().modified().unwrap();

        let registry = temp
            .path()
            .join("app-data/sessions/tine-test-workspaces.json");
        let states = [
            serde_json::json!({"version":1,"activeId":"one","workspaces":[{"id":"one","name":"One","blob":blank_session_json()}]}),
            serde_json::json!({"version":1,"activeId":"two","workspaces":[{"id":"one","name":"One","blob":blank_session_json()},{"id":"two","name":"Two","blob":blank_session_json()}]}),
            serde_json::json!({"version":1,"activeId":"one","workspaces":[{"id":"one","name":"Renamed","blob":blank_session_json()},{"id":"two","name":"Two","blob":blank_session_json()}]}),
            serde_json::json!({"version":1,"activeId":"one","workspaces":[{"id":"one","name":"Renamed","blob":blank_session_json()}]}),
        ];
        for state in states {
            save_workspaces_at(&registry, &state.to_string()).unwrap();
            assert_eq!(std::fs::read(&page).unwrap(), before_bytes);
            assert_eq!(
                std::fs::metadata(&page).unwrap().modified().unwrap(),
                before_modified
            );
        }
    }

    #[test]
    fn external_asset_approvals_are_device_local_and_target_specific() {
        let mut json = serde_json::json!({ "unrelated": true });
        remember_external_assets_approval_json(&mut json, "/graphs/a", "/media/one");
        remember_external_assets_approval_json(&mut json, "/graphs/b", "/media/two");
        remember_external_assets_approval_json(&mut json, "/graphs/a", "/media/retargeted");

        let approvals = external_assets_approvals(&json);
        assert_eq!(approvals["/graphs/a"], "/media/retargeted");
        assert_eq!(approvals["/graphs/b"], "/media/two");
        assert_eq!(json["unrelated"], true);
    }

    #[test]
    fn app_bool_reader_preserves_defaults_for_missing_or_invalid_settings() {
        let root = std::env::temp_dir().join(format!(
            "tine-settings-bool-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("settings.json");

        assert!(app_bool_at(&path, "frame", true));
        std::fs::write(&path, "not json").unwrap();
        assert!(!app_bool_at(&path, "frame", false));
        std::fs::write(&path, r#"{"frame":true,"other":false}"#).unwrap();
        assert!(app_bool_at(&path, "frame", false));
        assert!(app_bool_at(&path, "missing", true));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn managed_sync_device_id_is_stable_and_preserves_other_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tine-settings.json");
        std::fs::write(&path, r#"{"unrelated":true}"#).unwrap();

        let first = managed_sync_device_id_at(&path).unwrap();
        let second = managed_sync_device_id_at(&path).unwrap();
        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        assert_eq!(second, first);
        assert_eq!(persisted["managed_sync_device_id"], first.to_string());
        assert_eq!(persisted["unrelated"], true);
    }

    #[test]
    fn managed_sync_device_id_refuses_to_replace_malformed_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tine-settings.json");
        let malformed = b"not json\n";
        std::fs::write(&path, malformed).unwrap();

        assert!(managed_sync_device_id_at(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), malformed);
    }

    #[test]
    fn managed_sync_device_id_refuses_to_regenerate_an_invalid_existing_id() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tine-settings.json");
        let invalid = br#"{"managed_sync_device_id":"not-a-uuid","unrelated":true}"#;
        std::fs::write(&path, invalid).unwrap();

        assert!(managed_sync_device_id_at(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), invalid);
    }
}
