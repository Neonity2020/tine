use crate::state::{slot_for_context, GraphContext};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{hash_map::Entry, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};
use tine_core::model::{Graph, GraphTextSourceDigest};

const MANIFEST_TOOL: &str = "tine-graph-bytes";
const MANIFEST_ALGORITHM: &str = "sha256";

static VERIFICATIONS: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();

fn verifications() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    VERIFICATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphVerificationProgress {
    operation_id: String,
    processed: usize,
    total: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphVerificationError {
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphVerificationManifest {
    schema_version: u32,
    tool: String,
    algorithm: String,
    complete: bool,
    generated_at_unix_ms: u128,
    files: Vec<GraphTextSourceDigest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aggregate_digest: Option<String>,
    errors: Vec<GraphVerificationError>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GraphVerificationReport {
    text: String,
    suggested_file_name: String,
    total_files: usize,
    total_bytes: u64,
    aggregate_digest: Option<String>,
    complete: bool,
}

struct VerificationRegistration {
    operation_id: String,
}

impl Drop for VerificationRegistration {
    fn drop(&mut self) {
        if let Ok(mut jobs) = verifications().lock() {
            jobs.remove(&self.operation_id);
        }
    }
}

fn aggregate_digest(files: &[GraphTextSourceDigest]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"tine-graph-bytes-v1\0");
    for file in files {
        hasher.update((file.path.len() as u64).to_be_bytes());
        hasher.update(file.path.as_bytes());
        hasher.update(file.length.to_be_bytes());
        hasher.update(file.digest.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn build_manifest(
    graph: &Graph,
    app: &AppHandle,
    operation_id: &str,
    cancelled: &AtomicBool,
) -> Result<GraphVerificationManifest, String> {
    let mut errors = Vec::new();
    let mut paths = match graph.graph_text_source_paths() {
        Ok(paths) => paths,
        Err(error) => {
            errors.push(GraphVerificationError {
                path: None,
                detail: format!("source inventory failed: {error}"),
            });
            Vec::new()
        }
    };
    paths.sort();
    let total = paths.len();
    let mut files = Vec::with_capacity(total);
    for (index, path) in paths.iter().enumerate() {
        if cancelled.load(Ordering::Acquire) {
            return Err(tine_core::sync_runtime::tagged_backend_error(
                "operation-cancelled",
                None,
            ));
        }
        match graph.digest_graph_text_source(path, cancelled) {
            Ok(file) => files.push(file),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                return Err(tine_core::sync_runtime::tagged_backend_error(
                    "operation-cancelled",
                    None,
                ));
            }
            Err(error) => errors.push(GraphVerificationError {
                path: Some(path.clone()),
                detail: error.to_string(),
            }),
        }
        let _ = app.emit(
            "graph-verification-progress",
            GraphVerificationProgress {
                operation_id: operation_id.to_owned(),
                processed: index + 1,
                total,
            },
        );
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));

    match graph.graph_text_source_paths() {
        Ok(mut after) => {
            after.sort();
            if after != paths {
                errors.push(GraphVerificationError {
                    path: None,
                    detail: "source inventory changed while verification was running".into(),
                });
            }
        }
        Err(error) => errors.push(GraphVerificationError {
            path: None,
            detail: format!("final source inventory failed: {error}"),
        }),
    }

    let complete = errors.is_empty() && files.len() == paths.len();
    Ok(GraphVerificationManifest {
        schema_version: 1,
        tool: MANIFEST_TOOL.into(),
        algorithm: MANIFEST_ALGORITHM.into(),
        complete,
        generated_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        aggregate_digest: complete.then(|| aggregate_digest(&files)),
        files,
        errors,
    })
}

#[tauri::command]
pub(crate) async fn create_graph_verification(
    state: GraphContext<'_>,
    app: AppHandle,
    operation_id: String,
) -> Result<GraphVerificationReport, String> {
    if operation_id.is_empty() || operation_id.len() > 128 {
        return Err("invalid graph verification operation id".into());
    }
    let slot = slot_for_context(&state).map_err(|error| error.to_string())?;
    drop(state);
    let cancelled = Arc::new(AtomicBool::new(false));
    {
        let mut jobs = verifications()
            .lock()
            .map_err(|_| "graph verification registry is unavailable")?;
        match jobs.entry(operation_id.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(Arc::clone(&cancelled));
            }
            Entry::Occupied(_) => {
                return Err("graph verification operation id is already active".into());
            }
        }
    }
    let registration = VerificationRegistration {
        operation_id: operation_id.clone(),
    };
    tauri::async_runtime::spawn_blocking(move || {
        let _registration = registration;
        let manifest = slot
            .with_filesystem_graph(|graph| {
                build_manifest(graph, &app, &operation_id, &cancelled)
                    .map_err(crate::command_error::CommandError::prose)
            })
            .map_err(|error| error.to_string())?;
        let total_bytes = manifest
            .files
            .iter()
            .try_fold(0_u64, |sum, file| sum.checked_add(file.length))
            .ok_or("graph verification byte count overflow")?;
        let text = serde_json::to_string_pretty(&manifest)
            .map_err(|error| format!("graph verification report could not be encoded: {error}"))?;
        Ok(GraphVerificationReport {
            text,
            suggested_file_name: format!(
                "tine-graph-verification-{}.json",
                manifest.generated_at_unix_ms
            ),
            total_files: manifest.files.len(),
            total_bytes,
            aggregate_digest: manifest.aggregate_digest,
            complete: manifest.complete,
        })
    })
    .await
    .map_err(|error| format!("graph verification task failed: {error}"))?
}

#[tauri::command]
pub(crate) fn cancel_graph_verification(operation_id: String) -> Result<(), String> {
    let jobs = verifications()
        .lock()
        .map_err(|_| "graph verification registry is unavailable")?;
    if let Some(cancelled) = jobs.get(&operation_id) {
        cancelled.store(true, Ordering::Release);
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn save_graph_verification_report(
    app: AppHandle,
    text: String,
) -> Result<bool, String> {
    let manifest: GraphVerificationManifest = serde_json::from_str(&text)
        .map_err(|error| format!("graph verification report is invalid: {error}"))?;
    if manifest.schema_version != 1
        || manifest.tool != MANIFEST_TOOL
        || manifest.algorithm != MANIFEST_ALGORITHM
    {
        return Err("graph verification report has an unsupported format".into());
    }
    #[cfg(desktop)]
    {
        use tauri_plugin_dialog::DialogExt as _;
        let suggested = format!(
            "tine-graph-verification-{}.json",
            manifest.generated_at_unix_ms
        );
        let chosen = tauri::async_runtime::spawn_blocking(move || {
            app.dialog()
                .file()
                .set_file_name(suggested)
                .add_filter("JSON", &["json"])
                .blocking_save_file()
        })
        .await
        .map_err(|error| format!("graph verification save dialog failed: {error}"))?;
        let Some(chosen) = chosen else {
            return Ok(false);
        };
        let path = chosen
            .into_path()
            .map_err(|_| "graph verification destination is not a local file".to_string())?;
        tine_core::model::atomic_write(&path, text.as_bytes())
            .map_err(|error| format!("graph verification report could not be saved: {error}"))?;
        Ok(true)
    }
    #[cfg(not(desktop))]
    {
        let _ = (app, manifest, text);
        Err("Save report is available on desktop; use Copy report on this device.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_is_ordered_and_content_sensitive() {
        let first = vec![GraphTextSourceDigest {
            path: "pages/a.md".into(),
            length: 3,
            digest: "a".repeat(64),
        }];
        let mut changed = first.clone();
        changed[0].digest = "b".repeat(64);
        assert_ne!(aggregate_digest(&first), aggregate_digest(&changed));
        assert_eq!(aggregate_digest(&first), aggregate_digest(&first));
    }
}
