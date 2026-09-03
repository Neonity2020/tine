//! App-private durable retention for unresolved live-save conflicts.
//!
//! Harvest B3 names this protocol because the retained editor draft exists
//! nowhere in the graph. Its graph-keyed v1 envelope is atomically replaced or
//! retired through the same durability primitive as other audited native state.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tauri::Manager;

const ENVELOPE_VERSION: u64 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ConflictCapsuleEnvelope {
    version: u64,
    capsules: Vec<Value>,
}

fn graph_key(root: &Path) -> String {
    crate::settings::graph_storage_key(root)
}

fn capsule_path(app_data: &Path, root: &Path) -> PathBuf {
    app_data
        .join("conflict-capsules")
        .join(format!("{}.v1.json", graph_key(root)))
}

static CONFLICT_CAPSULE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn capsule_page_name(capsule: &Value) -> Result<&str, crate::command_error::CommandError> {
    let source = capsule
        .get("source")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            crate::command_error::CommandError::prose("conflict capsule requires a source")
        })?;
    if source != "live-save" {
        return Err(crate::command_error::CommandError::prose(
            "conflict capsule source must be live-save",
        ));
    }
    capsule
        .get("page_name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            crate::command_error::CommandError::prose("conflict capsule requires a page_name")
        })
}

fn reclaim_torn_temps(path: &Path) -> Result<(), crate::command_error::CommandError> {
    let Some(parent) = path.parent() else {
        return Err(crate::command_error::CommandError::prose(
            "conflict capsule has no parent directory",
        ));
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(crate::command_error::CommandError::prose(
            "conflict capsule name is not UTF-8",
        ));
    };
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(crate::command_error::CommandError::from(error)),
    };
    let prefix = format!(".{name}.");
    let mut reclaimed = false;
    for entry in entries {
        let entry = entry.map_err(crate::command_error::CommandError::from)?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with(&prefix) && file_name.ends_with(".tmp") {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => reclaimed = true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(crate::command_error::CommandError::from(error)),
            }
        }
    }
    if reclaimed {
        tine_core::model::sync_dir_for_rename(parent)
            .map_err(crate::command_error::CommandError::from)?;
    }
    Ok(())
}

fn decode_envelope(bytes: &[u8]) -> Result<Vec<Value>, crate::command_error::CommandError> {
    let envelope: ConflictCapsuleEnvelope =
        serde_json::from_slice(bytes).map_err(crate::command_error::CommandError::from)?;
    if envelope.version != ENVELOPE_VERSION {
        return Err(crate::command_error::CommandError::prose(format!(
            "unsupported conflict capsule version {}",
            envelope.version
        )));
    }
    let mut names = std::collections::HashSet::new();
    for capsule in &envelope.capsules {
        let name = capsule_page_name(capsule)?;
        if !names.insert(name.to_owned()) {
            return Err(crate::command_error::CommandError::prose(format!(
                "duplicate conflict capsule for {name}"
            )));
        }
    }
    Ok(envelope.capsules)
}

/// Set an unreadable envelope aside under a unique sibling name and report an
/// empty queue. Recovery over refusal (D-2): the envelope is restart-recovery
/// material, never graph authority, so a file this build cannot decode
/// (in-scope: disk error, a sync client delivering another build's envelope)
/// must not make every later capture and retirement fail forever. The bytes
/// are preserved, not deleted.
fn quarantine_unreadable(
    path: &Path,
    _reason: &crate::command_error::CommandError,
) -> Result<(), crate::command_error::CommandError> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(crate::command_error::CommandError::prose(
            "conflict capsule name is not UTF-8",
        ));
    };
    let aside = path.with_file_name(format!(
        "{name}.unreadable-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::rename(path, &aside).map_err(crate::command_error::CommandError::from)?;
    if let Some(parent) = path.parent() {
        tine_core::model::sync_dir_for_rename(parent)
            .map_err(crate::command_error::CommandError::from)?;
    }
    Ok(())
}

fn load_unlocked(path: &Path) -> Result<Vec<Value>, crate::command_error::CommandError> {
    reclaim_torn_temps(path)?;
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(crate::command_error::CommandError::from(error)),
    };
    match decode_envelope(&bytes) {
        Ok(capsules) => Ok(capsules),
        Err(reason) => {
            quarantine_unreadable(path, &reason)?;
            Ok(Vec::new())
        }
    }
}

fn load_at(path: &Path) -> Result<Vec<Value>, crate::command_error::CommandError> {
    let _guard = CONFLICT_CAPSULE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    load_unlocked(path)
}

fn write_unlocked(
    path: &Path,
    capsules: Vec<Value>,
) -> Result<(), crate::command_error::CommandError> {
    let parent = path.parent().ok_or_else(|| {
        crate::command_error::CommandError::prose("conflict capsule has no parent directory")
    })?;
    std::fs::create_dir_all(parent).map_err(crate::command_error::CommandError::from)?;
    let bytes = serde_json::to_vec(&ConflictCapsuleEnvelope {
        version: ENVELOPE_VERSION,
        capsules,
    })
    .map_err(crate::command_error::CommandError::from)?;
    // Named audited B3 conflict-capsule publication protocol (I-1/I-2):
    // unique create-new temp, complete file barrier, atomic replacement,
    // failed-temp cleanup, and strict directory-barrier error reporting.
    tine_core::model::atomic_write(path, &bytes).map_err(crate::command_error::CommandError::from)
}

fn upsert_at(path: &Path, capsule: Value) -> Result<(), crate::command_error::CommandError> {
    let page_name = capsule_page_name(&capsule)?.to_owned();
    let _guard = CONFLICT_CAPSULE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut capsules = load_unlocked(path)?;
    if let Some(existing) = capsules
        .iter_mut()
        .find(|existing| capsule_page_name(existing).ok() == Some(page_name.as_str()))
    {
        *existing = capsule;
    } else {
        capsules.push(capsule);
    }
    write_unlocked(path, capsules)
}

fn retire_at(path: &Path, page_name: &str) -> Result<(), crate::command_error::CommandError> {
    let _guard = CONFLICT_CAPSULE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut capsules = load_unlocked(path)?;
    let prior = capsules.len();
    capsules.retain(|capsule| capsule_page_name(capsule).ok() != Some(page_name));
    if capsules.len() == prior {
        return Ok(());
    }
    if !capsules.is_empty() {
        return write_unlocked(path, capsules);
    }
    let parent = path.parent().ok_or_else(|| {
        crate::command_error::CommandError::prose("conflict capsule has no parent directory")
    })?;
    match std::fs::remove_file(path) {
        Ok(()) => tine_core::model::sync_dir_for_rename(parent)
            .map_err(crate::command_error::CommandError::from),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(crate::command_error::CommandError::from(error)),
    }
}

fn app_capsule_path(
    app: &tauri::AppHandle,
    root: &str,
) -> Result<PathBuf, crate::command_error::CommandError> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(crate::command_error::CommandError::from)?;
    Ok(capsule_path(&app_data, Path::new(root)))
}

#[tauri::command]
pub(crate) fn load_conflict_capsules(
    root: String,
    app: tauri::AppHandle,
) -> Result<Vec<Value>, crate::command_error::CommandError> {
    load_at(&app_capsule_path(&app, &root)?)
}

#[tauri::command]
pub(crate) fn store_conflict_capsule(
    root: String,
    capsule: Value,
    app: tauri::AppHandle,
) -> Result<(), crate::command_error::CommandError> {
    upsert_at(&app_capsule_path(&app, &root)?, capsule)
}

#[tauri::command]
pub(crate) fn retire_conflict_capsule(
    root: String,
    page_name: String,
    app: tauri::AppHandle,
) -> Result<(), crate::command_error::CommandError> {
    retire_at(&app_capsule_path(&app, &root)?, &page_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capsule(page: &str, draft: &str) -> Value {
        serde_json::json!({
            "id": format!("live:{page}"),
            "source": "live-save",
            "page_name": page,
            "page_path": format!("pages/{page}.md"),
            "kind": "page",
            "sides": [],
            "live": {
                "page": { "name": page, "blocks": [{ "raw": draft }] },
                "base_rev": "base",
                "conflict_epoch": 7,
                "draft_version": 1
            }
        })
    }

    #[test]
    fn native_channel_publishes_the_exact_retained_draft() {
        let temp = tempfile::tempdir().unwrap();
        let path = capsule_path(temp.path(), Path::new("/graphs/alpha"));
        upsert_at(&path, capsule("Note", "exact retained bytes")).unwrap();
        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(bytes.contains("exact retained bytes"));
        assert_eq!(load_at(&path).unwrap()[0]["page_name"], "Note");
    }

    #[test]
    fn unreadable_envelope_is_set_aside_and_the_queue_keeps_working() {
        let temp = tempfile::tempdir().unwrap();
        let path = capsule_path(temp.path(), Path::new("/graphs/alpha"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{\"version\":99,\"capsules\":[]}").unwrap();

        assert!(load_at(&path).unwrap().is_empty());
        assert!(
            !path.exists(),
            "the unreadable envelope is moved aside, not read"
        );
        let aside = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|candidate| {
                candidate
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(".unreadable-")
            })
            .expect("the original bytes are preserved beside the envelope");
        assert_eq!(
            std::fs::read(&aside).unwrap(),
            b"{\"version\":99,\"capsules\":[]}"
        );

        upsert_at(&path, capsule("Note", "after quarantine")).unwrap();
        assert_eq!(load_at(&path).unwrap()[0]["page_name"], "Note");
        retire_at(&path, "Note").unwrap();
        assert!(load_at(&path).unwrap().is_empty());
    }

    #[test]
    fn torn_temp_is_ignored_and_reclaimed_without_inventing_a_capsule() {
        let temp = tempfile::tempdir().unwrap();
        let path = capsule_path(temp.path(), Path::new("/graphs/alpha"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let torn = path.parent().unwrap().join(format!(
            ".{}.999.1.tmp",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&torn, b"{\"version\":1,\"capsules\":[").unwrap();
        assert!(load_at(&path).unwrap().is_empty());
        assert!(!torn.exists());
    }

    #[test]
    fn torn_replacement_keeps_the_previous_complete_envelope() {
        let temp = tempfile::tempdir().unwrap();
        let path = capsule_path(temp.path(), Path::new("/graphs/alpha"));
        upsert_at(&path, capsule("Note", "previous")).unwrap();
        let torn = path.parent().unwrap().join(format!(
            ".{}.999.2.tmp",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&torn, b"replacement not complete").unwrap();
        assert_eq!(
            load_at(&path).unwrap()[0]["live"]["page"]["blocks"][0]["raw"],
            "previous"
        );
        assert!(!torn.exists());
    }

    #[test]
    fn replacement_and_retirement_reopen_as_complete_transitions() {
        let temp = tempfile::tempdir().unwrap();
        let path = capsule_path(temp.path(), Path::new("/graphs/alpha"));
        upsert_at(&path, capsule("One", "first")).unwrap();
        upsert_at(&path, capsule("Two", "second")).unwrap();
        upsert_at(&path, capsule("One", "replacement")).unwrap();
        let reopened = load_at(&path).unwrap();
        assert_eq!(reopened.len(), 2);
        assert!(reopened
            .iter()
            .any(|value| value["live"]["page"]["blocks"][0]["raw"] == "replacement"));
        retire_at(&path, "One").unwrap();
        let reopened = load_at(&path).unwrap();
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened[0]["page_name"], "Two");
    }

    #[test]
    fn retiring_the_last_capsule_durably_removes_the_graph_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = capsule_path(temp.path(), Path::new("/graphs/alpha"));
        upsert_at(&path, capsule("Note", "draft")).unwrap();
        retire_at(&path, "Note").unwrap();
        assert!(!path.exists());
        assert!(load_at(&path).unwrap().is_empty());
    }

    #[test]
    fn graph_keys_equal_the_session_key_for_distinct_path_shapes() {
        for root in [
            Path::new("/one/graph"),
            Path::new("/two/graph"),
            Path::new("/one/my graph"),
            Path::new("/"),
        ] {
            assert_eq!(graph_key(root), crate::settings::graph_storage_key(root));
        }
        assert_ne!(
            graph_key(Path::new("/one/graph")),
            graph_key(Path::new("/two/graph"))
        );
    }
}
