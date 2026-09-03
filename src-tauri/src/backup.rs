use crate::settings::{settings_path, update_settings};
use crate::state::{slot_for_context, GraphContext, GraphSlot};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::Manager;
use tine_core::{model::Graph, GraphTextScope, GRAPH_TEXT_SCOPE_VERSION};

// Snapshot the graph's Markdown/Org into the OS app-data dir on open, keeping the
// last few. Local-only (outside the graph, so Syncthing never sees it); a safety
// net against a bad write or accidental edit. Best-effort and fully detached so
// it never blocks startup or holds the graph lock during file copies.
const BACKUP_KEEP_DEFAULT: usize = 12;
const ASSET_RESTORE_RECOVERY_DIR: &str = ".tine-restore-recovery";
static BACKUP_WORK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

pub(crate) fn backup_async(app: tauri::AppHandle, slot: Arc<GraphSlot>) -> Result<(), String> {
    let graph = slot.legacy_graph().map_err(|error| error.to_string())?;
    let source = BackupSource::from_graph(&graph);
    drop(graph);
    std::thread::spawn(move || {
        // Defer the launch snapshot ~1s so its whole-graph file copy doesn't
        // contend for disk I/O with first-journal paint and the warm-cache parse
        // at open (felt on slow/NFS disks or a throttled laptop). Safe: the
        // snapshot guards this session's edits, and the user hasn't edited yet in
        // the first second — the on-disk files are still intact — so a crash in
        // that window loses nothing the snapshot would have protected.
        std::thread::sleep(std::time::Duration::from_millis(1000));
        if slot.background_cancelled.load(Ordering::Acquire) {
            return;
        }
        // Bound whole-graph copying process-wide. Revoked bindings check again
        // after obtaining the permit and between directory entries/files.
        let _worker = BACKUP_WORK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        if slot.background_cancelled.load(Ordering::Acquire) {
            return;
        }
        let _ = do_backup_source_cancellable(&app, source, "", &|| {
            slot.background_cancelled.load(Ordering::Acquire)
        }); // launch snapshot is best-effort
    });
    Ok(())
}

pub(crate) fn backup_graph_now(
    app: &tauri::AppHandle,
    graph: &Graph,
    suffix: &str,
) -> (usize, bool) {
    do_backup_source(app, BackupSource::from_graph(graph), suffix)
}

/// Take one snapshot of the current graph now (synchronous). Returns the number
/// of files copied (0 = nothing to back up). Reads the keep count from the local
/// app-settings file and prunes old snapshots afterwards. `suffix` tags special
/// snapshots (e.g. "pre-restore") so they get a distinct, collision-proof
/// directory name and are exempt from the keep-count prune.
/// Returns (files copied, complete) — `complete` is false if ANY graph
/// text/config/asset-sidecar copy failed, so the caller (restore) can refuse to
/// proceed without a full rollback snapshot.
#[derive(Clone)]
struct BackupSource {
    assets: PathBuf,
    cfg: PathBuf,
    root: PathBuf,
    journals_dir: String,
    pages_dir: String,
    graph_text_scope: GraphTextScope,
    graph_text_policy: SnapshotGraphTextPolicy,
}

impl BackupSource {
    fn from_graph(g: &Graph) -> Self {
        Self {
            assets: g.assets_path(),
            cfg: g.root.join("logseq").join("config.edn"),
            root: g.root.clone(),
            journals_dir: g.config.journals_dir.clone(),
            pages_dir: g.config.pages_dir.clone(),
            graph_text_scope: g.graph_text_scope(),
            graph_text_policy: SnapshotGraphTextPolicy {
                version: GRAPH_TEXT_SCOPE_VERSION,
                hidden: g.config.hidden.clone(),
                hidden_parse_failed_closed: g.config.hidden_parse_failed_closed,
            },
        }
    }
}

const LEGACY_SNAPSHOT_SCHEMA: u32 = 2;
const SNAPSHOT_SCHEMA: u32 = 3;
const SNAPSHOT_MANIFEST: &str = "snapshot.json";

#[cfg(test)]
std::thread_local! {
    static PAYLOAD_HASH_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SnapshotFile {
    path: String,
    sha256: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SnapshotManifest {
    schema: u32,
    root: String,
    journals_dir: String,
    pages_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    graph_text_policy: Option<SnapshotGraphTextPolicy>,
    files: Vec<SnapshotFile>,
    complete: bool,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SnapshotGraphTextPolicy {
    version: u32,
    hidden: Vec<String>,
    hidden_parse_failed_closed: bool,
}

impl SnapshotGraphTextPolicy {
    fn scope(&self) -> Result<GraphTextScope, String> {
        if self.version != GRAPH_TEXT_SCOPE_VERSION {
            return Err(format!(
                "backup uses unsupported graph-text policy version {}",
                self.version
            ));
        }
        Ok(GraphTextScope::new(
            &self.hidden,
            self.hidden_parse_failed_closed,
        ))
    }
}

fn root_backup_id(root: &std::path::Path) -> String {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    let label = canonical
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("graph")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("{label}-{}", &digest[..32])
}

fn write_manifest(dir: &std::path::Path, manifest: &SnapshotManifest) -> std::io::Result<()> {
    let path = dir.join(SNAPSHOT_MANIFEST);
    let tmp = dir.join(".snapshot.json.tmp");
    let bytes = serde_json::to_vec_pretty(manifest).map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    use std::io::Write;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp, path)
}

fn read_manifest(dir: &std::path::Path) -> Option<SnapshotManifest> {
    let bytes = std::fs::read(dir.join(SNAPSHOT_MANIFEST)).ok()?;
    let manifest: SnapshotManifest = serde_json::from_slice(&bytes).ok()?;
    let supported = manifest.schema == LEGACY_SNAPSHOT_SCHEMA
        || (manifest.schema == SNAPSHOT_SCHEMA
            && manifest
                .graph_text_policy
                .as_ref()
                .is_some_and(|policy| policy.version == GRAPH_TEXT_SCOPE_VERSION));
    (supported && manifest.complete).then_some(manifest)
}

fn hash_snapshot_file(path: &std::path::Path) -> std::io::Result<String> {
    #[cfg(test)]
    PAYLOAD_HASH_READS.with(|reads| reads.set(reads.get() + 1));
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn snapshot_inventory(dir: &std::path::Path) -> std::io::Result<Vec<SnapshotFile>> {
    let mut files = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), PathBuf::new())];
    while let Some((current, rel)) = stack.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let rel_child = rel.join(entry.file_name());
            if file_type.is_dir() {
                stack.push((entry.path(), rel_child));
            } else if file_type.is_file()
                && rel_child != std::path::Path::new(SNAPSHOT_MANIFEST)
                && rel_child != std::path::Path::new(".snapshot.json.tmp")
            {
                let path = rel_child
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                files.push(SnapshotFile {
                    path,
                    sha256: hash_snapshot_file(&entry.path())?,
                });
            } else if !file_type.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "snapshot contains a non-regular entry",
                ));
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn verify_snapshot(dir: &std::path::Path, manifest: &SnapshotManifest) -> bool {
    snapshot_inventory(dir)
        .map(|files| files == manifest.files)
        .unwrap_or(false)
}

fn do_backup_source(app: &tauri::AppHandle, source: BackupSource, suffix: &str) -> (usize, bool) {
    let _worker = BACKUP_WORK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap();
    do_backup_source_cancellable(app, source, suffix, &|| false)
}

struct PartialBackup {
    path: PathBuf,
    committed: bool,
}

impl Drop for PartialBackup {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn cleanup_partial_backups(base: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(".partial-") {
            let path = entry.path();
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(path);
            } else {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn do_backup_source_cancellable(
    app: &tauri::AppHandle,
    source: BackupSource,
    suffix: &str,
    cancelled: &dyn Fn() -> bool,
) -> (usize, bool) {
    if cancelled() {
        return (0, false);
    }
    let Ok(data_dir) = app.path().app_data_dir() else {
        return (0, false);
    };
    let base = data_dir.join("backups").join(root_backup_id(&source.root));
    let stamp = backup_stamp();
    let name = if suffix.is_empty() {
        stamp
    } else {
        format!("{stamp}-{suffix}")
    };
    // Reserve a UNIQUE destination directory. The stamp is second-granularity, so
    // two snapshots in the same second (e.g. a launch snapshot racing a pre-restore
    // snapshot) would otherwise share one directory — and copy_md_dir, which copies
    // in but never removes files absent from the live graph, would mix both
    // snapshots' files, leaving a later restore with stale notes/sidecars. `create_dir`
    // (non-recursive) fails atomically if the name is taken, so we bump a counter
    // until we win an unused name.
    let _ = std::fs::create_dir_all(&base);
    cleanup_partial_backups(&base);
    let mut final_dest = base.join(&name);
    let mut dest = base.join(format!(".partial-{name}"));
    let mut k = 2;
    loop {
        match std::fs::create_dir(&dest) {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                final_dest = base.join(format!("{name}-{k}"));
                dest = base.join(format!(".partial-{name}-{k}"));
                k += 1;
            }
            Err(_) => return (0, false),
        }
    }
    let mut partial = PartialBackup {
        path: dest.clone(),
        committed: false,
    };
    let (ct, ft) = copy_graph_text_tree_cancellable(
        &source.root,
        &dest.join("graph"),
        &source.graph_text_scope,
        cancelled,
    );
    let (ca, fa) = copy_asset_sidecars_dir_cancellable(
        &source.assets,
        &dest.join(dir_name(&source.assets)),
        cancelled,
    );
    let mut n = ct + ca;
    let mut failed = ft + fa;
    if !cancelled() && source.cfg.exists() {
        let out = dest.join("logseq");
        if std::fs::create_dir_all(&out).is_ok()
            && std::fs::copy(&source.cfg, out.join("config.edn")).is_ok()
        {
            n += 1;
        } else {
            failed += 1;
        }
    }
    let complete = !cancelled() && failed == 0;
    if n == 0 {
        return (0, complete);
    }
    if complete {
        let Ok(files) = snapshot_inventory(&dest) else {
            return (n, false);
        };
        if files.len() != n {
            return (n, false);
        }
        let manifest = SnapshotManifest {
            schema: SNAPSHOT_SCHEMA,
            root: std::fs::canonicalize(&source.root)
                .unwrap_or(source.root.clone())
                .display()
                .to_string(),
            journals_dir: source.journals_dir,
            pages_dir: source.pages_dir,
            graph_text_policy: Some(source.graph_text_policy),
            files,
            complete: true,
        };
        if write_manifest(&dest, &manifest).is_err() || std::fs::rename(&dest, &final_dest).is_err()
        {
            return (n, false);
        }
        partial.committed = true;
    }
    prune_backups(&base, backup_keep(app));
    (n, complete)
}

fn backup_keep(app: &tauri::AppHandle) -> usize {
    settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("backup_keep").and_then(|x| x.as_u64()))
        .map(|n| (n as usize).max(1))
        .unwrap_or(BACKUP_KEEP_DEFAULT)
}

#[derive(serde::Serialize)]
pub(crate) struct BackupInfo {
    stamp: String,
    files: usize,
}

#[tauri::command]
pub(crate) fn get_backup_keep(app: tauri::AppHandle) -> usize {
    backup_keep(&app)
}

#[tauri::command]
pub(crate) fn set_backup_keep(
    keep: usize,
    app: tauri::AppHandle,
    state: GraphContext<'_>,
) -> Result<(), String> {
    let keep = keep.clamp(1, 1000);
    update_settings(&app, |json| {
        json["backup_keep"] = serde_json::json!(keep);
    })?;
    // Apply the new (possibly lower) cap to the current graph's snapshots now.
    let slot = slot_for_context(&state).map_err(|error| error.to_string())?;
    let graph = slot.legacy_graph().map_err(|error| error.to_string())?;
    if let Some(base) = backup_base(&app, &graph) {
        prune_backups(&base, keep);
    }
    Ok(())
}

/// The backup directory for the currently-open graph (`<app-data>/backups/<id>`).
fn backup_base(app: &tauri::AppHandle, graph: &Graph) -> Option<PathBuf> {
    backup_base_for_root(app, &graph.root)
}

fn backup_base_for_root(app: &tauri::AppHandle, root: &std::path::Path) -> Option<PathBuf> {
    let data_dir = app.path().app_data_dir().ok()?;
    Some(data_dir.join("backups").join(root_backup_id(root)))
}

/// The Concord base-ledger directory for a graph root
/// (`<app-data>/concord-ledger/<id>`, same root-id convention as backups —
/// outside the sync tree, invisible to transports). See ADR 0056.
pub(crate) fn concord_ledger_dir(
    app: &tauri::AppHandle,
    root: &std::path::Path,
) -> Option<PathBuf> {
    let data_dir = app.path().app_data_dir().ok()?;
    Some(data_dir.join("concord-ledger").join(root_backup_id(root)))
}

/// The Direct-move recovery store for a graph root
/// (`<app-data>/direct-move-recovery/<id>`, the same root-id convention as
/// backups and the Concord ledger).
///
/// App-private and graph-keyed, deliberately OUTSIDE the graph tree: the graph
/// directory is Logseq-shared surface and a sync transport carries it, so a
/// device's in-flight move record must never travel with it
/// (`docs/contracts/direct-move-recovery.md` §2).
pub(crate) fn direct_move_recovery_dir(
    app: &tauri::AppHandle,
    root: &std::path::Path,
) -> Option<PathBuf> {
    let data_dir = app.path().app_data_dir().ok()?;
    Some(
        data_dir
            .join("direct-move-recovery")
            .join(root_backup_id(root)),
    )
}

#[tauri::command]
pub(crate) async fn list_backups(
    app: tauri::AppHandle,
    state: GraphContext<'_>,
) -> Result<Vec<BackupInfo>, String> {
    let root = slot_for_context(&state)
        .map_err(|error| error.to_string())?
        .legacy_graph()
        .map_err(|error| error.to_string())?
        .root
        .clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(base) = backup_base_for_root(&app, &root) else {
            return Vec::new();
        };
        list_backups_from_base(&base, &root)
    })
    .await
    .map_err(|error| error.to_string())
}

fn list_backups_from_base(base: &std::path::Path, root: &std::path::Path) -> Vec<BackupInfo> {
    let current_root = std::fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .display()
        .to_string();
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&base) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let Some(manifest) = read_manifest(&p) else {
                continue;
            };
            if manifest.root != current_root {
                continue;
            }
            let stamp = match p.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let files = manifest.files.len();
            out.push(BackupInfo { stamp, files });
        }
    }
    out.sort_by(|a, b| b.stamp.cmp(&a.stamp)); // newest first
    out
}

/// Restore a snapshot into the live graph. Schema 3 restores graph text at its
/// exact graph-relative path; schema 2 retains the original configured-root
/// behavior. Asset `.edn` sidecars and `config.edn` are restored by both.
/// Takes a fresh safety snapshot of the *current* state first.
/// Destructive — the frontend confirms.
#[tauri::command]
pub(crate) async fn restore_backup(
    stamp: String,
    app: tauri::AppHandle,
    state: GraphContext<'_>,
) -> Result<(), String> {
    // Guard against path traversal — a stamp is only ever `YYYY-MM-DD_HH-MM-SS`.
    if stamp.is_empty()
        || !stamp
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("invalid backup id".into());
    }
    let slot = slot_for_context(&state).map_err(|error| error.to_string())?;
    let graph = slot
        .legacy_graph_cloned()
        .map_err(|error| error.to_string())?;
    let source = BackupSource::from_graph(&graph);
    let restore_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let base = backup_base_for_root(&restore_app, &source.root).ok_or("no app-data dir")?;
        restore_from_backup_source(&stamp, &base, source, |source| {
            do_backup_source(&restore_app, source.clone(), "pre-restore")
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    crate::state::refresh_graph(&state).map_err(|error| error.to_string())
}

fn restore_from_backup_source(
    stamp: &str,
    base: &std::path::Path,
    source: BackupSource,
    snapshot_current: impl FnOnce(&BackupSource) -> (usize, bool),
) -> Result<(), String> {
    let assets = source.assets.clone();
    let cfg_dest = source.cfg.clone();
    let src = base.join(&stamp);
    if !src.is_dir() {
        return Err("backup not found".into());
    }
    let manifest = read_manifest(&src).ok_or("backup is incomplete or unverified")?;
    let current_root = std::fs::canonicalize(&source.root).unwrap_or_else(|_| source.root.clone());
    if manifest.root != current_root.display().to_string() {
        return Err("backup belongs to a different graph".into());
    }
    if !verify_snapshot(&src, &manifest) {
        return Err("backup contents do not match the verified manifest".into());
    }
    let safe_dir = |raw: &str| -> Result<PathBuf, String> {
        let rel = std::path::Path::new(raw);
        if raw.is_empty()
            || raw.contains('\\')
            || rel.is_absolute()
            || rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("backup contains an unsafe graph directory".into());
        }
        Ok(current_root.join(rel))
    };
    let legacy_restore_roots = (manifest.schema == LEGACY_SNAPSHOT_SCHEMA)
        .then(|| {
            Ok::<_, String>((
                safe_dir(&manifest.journals_dir)?,
                safe_dir(&manifest.pages_dir)?,
            ))
        })
        .transpose()?;
    let validate_live_layout = || -> Result<(), String> {
        ensure_target_within_root(&current_root, &cfg_dest)
            .map_err(|e| format!("unsafe live config path: {e}"))?;
        if let Some((journals, pages)) = &legacy_restore_roots {
            for (label, path) in [("journals", journals), ("pages", pages)] {
                ensure_target_within_root(&current_root, path)
                    .map_err(|e| format!("unsafe live {label} path: {e}"))?;
            }
        }
        // Assets have a separate, explicitly-approved capability and therefore
        // validate against their own canonical root. For ordinary graphs this is
        // still `<graph>/assets`; for GH #127 it is the approved external target.
        ensure_target_within_root(&assets, &assets)
            .map_err(|e| format!("unsafe live assets path: {e}"))?;
        Ok(())
    };
    validate_live_layout()?;
    let recovery_id = format!(
        "{}-pre-restore-extras-{}-{}",
        backup_stamp(),
        std::process::id(),
        RESTORE_RECOVERY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let graph_recovery = reserve_restore_recovery(
        &current_root,
        std::path::Path::new("logseq/.tine-trash"),
        &recovery_id,
    )
    .map_err(|e| format!("couldn't create restore recovery area: {e}"))?;
    let asset_recovery = reserve_restore_recovery(
        &assets,
        std::path::Path::new(ASSET_RESTORE_RECOVERY_DIR),
        &recovery_id,
    )
    .map_err(|e| format!("couldn't create asset restore recovery area: {e}"))?;
    // Safety net: snapshot the current (pre-restore) state first, under a distinct
    // name so it can't collide with (or be pruned by) the launch snapshot the
    // post-restore reload will take. Abort if the snapshot fails while the live
    // graph has content — never run a destructive restore without a way back.
    let (_, complete) = snapshot_current(&source);
    // A destructive restore must be fully reversible. A successful empty
    // snapshot is valid; any failed copy or traversal aborts the restore.
    if !complete {
        return Err(
            "couldn't create a complete pre-restore safety snapshot — restore aborted".into(),
        );
    }
    // The safety snapshot can take time. Revalidate after it so a symlink swap
    // cannot redirect the destructive copy/delete phase outside the graph.
    validate_live_layout()?;
    let snapshot_scope = match manifest.schema {
        LEGACY_SNAPSHOT_SCHEMA => source.graph_text_scope.clone(),
        SNAPSHOT_SCHEMA => manifest
            .graph_text_policy
            .as_ref()
            .ok_or("backup does not record its graph-text policy")?
            .scope()?,
        _ => return Err("backup uses an unsupported snapshot schema".into()),
    };
    // Copies happen before extras are moved to recovery, so failure leaves
    // either the original or a recoverable copy.
    match manifest.schema {
        LEGACY_SNAPSHOT_SCHEMA => {
            let (restore_journals, restore_pages) = legacy_restore_roots
                .as_ref()
                .expect("legacy restore roots were validated");
            restore_md_dir(
                &src.join("journals"),
                restore_journals,
                &graph_recovery,
                std::path::Path::new("journals"),
            )
            .map_err(|e| format!("restore journals failed: {e}"))?;
            restore_md_dir(
                &src.join("pages"),
                restore_pages,
                &graph_recovery,
                std::path::Path::new("pages"),
            )
            .map_err(|e| format!("restore pages failed: {e}"))?;
        }
        SNAPSHOT_SCHEMA => restore_graph_text_tree(
            &src.join("graph"),
            &current_root,
            &snapshot_scope,
            &graph_recovery,
        )
        .map_err(|e| format!("restore graph text failed: {e}"))?,
        _ => return Err("backup uses an unsupported snapshot schema".into()),
    }
    restore_asset_sidecars_dir(
        &src.join(dir_name(&assets)),
        &assets,
        &asset_recovery,
        std::path::Path::new(""),
    )
    .map_err(|e| format!("restore asset sidecars failed: {e}"))?;
    let src_cfg = src.join("logseq").join("config.edn");
    if src_cfg.exists() {
        let cfg_relative = live_relative(&graph_recovery, &cfg_dest)
            .map_err(|e| format!("unsafe live config path: {e}"))?;
        open_or_create_real_parent(
            &graph_recovery.root,
            cfg_relative
                .parent()
                .unwrap_or_else(|| std::path::Path::new("")),
            &graph_recovery.directory_barriers,
            RestoreDirectoryRoot::Live,
            false,
        )
        .map_err(|e| format!("couldn't prepare live config directory: {e}"))?;
        if cfg_dest.exists() {
            move_live_to_recovery(
                &graph_recovery,
                &cfg_dest,
                std::path::Path::new("logseq/config.edn"),
            )
            .map_err(|e| format!("recover current config failed: {e}"))?;
        }
        atomic_copy_new_into_live(&graph_recovery, &src_cfg, &cfg_dest)
            .map_err(|e| format!("restore config failed: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
fn collect_legacy_restore_graph_text(
    snapshot: &Path,
    journals_dir: &str,
    pages_dir: &str,
) -> Result<Vec<(String, String)>, String> {
    fn collect(
        source: &Path,
        graph_dir: &str,
        relative: &Path,
        output: &mut Vec<(String, String)>,
    ) -> Result<(), String> {
        let entries = std::fs::read_dir(source)
            .map_err(|error| format!("cannot read verified backup directory: {error}"))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("cannot read verified backup: {error}"))?;
            let path = entry.path();
            let child = relative.join(entry.file_name());
            if is_graph_text(&path) {
                let tail = child
                    .components()
                    .map(|component| match component {
                        std::path::Component::Normal(value) => value
                            .to_str()
                            .map(str::to_string)
                            .ok_or_else(|| "backup contains a non-UTF-8 graph path".to_string()),
                        _ => Err("backup contains an unsafe graph path".to_string()),
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .join("/");
                let content = std::fs::read_to_string(&path)
                    .map_err(|error| format!("cannot read verified backup page: {error}"))?;
                output.push((format!("{graph_dir}/{tail}"), content));
            } else if is_visible_real_dir(&entry)
                .map_err(|error| format!("cannot inspect verified backup: {error}"))?
            {
                collect(&path, graph_dir, &child, output)?;
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    collect(
        &snapshot.join("journals"),
        journals_dir,
        Path::new(""),
        &mut files,
    )?;
    collect(
        &snapshot.join("pages"),
        pages_dir,
        Path::new(""),
        &mut files,
    )?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

#[cfg(test)]
fn collect_scoped_restore_graph_text(
    source: &Path,
    scope: &GraphTextScope,
) -> Result<Vec<(String, String)>, String> {
    let mut files = Vec::new();
    if !source.is_dir() {
        return Ok(files);
    }
    let mut stack = vec![(source.to_path_buf(), PathBuf::new())];
    while let Some((directory, relative)) = stack.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("cannot read verified backup directory: {error}"))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("cannot read verified backup: {error}"))?;
            let file_type = entry
                .file_type()
                .map_err(|error| format!("cannot inspect verified backup: {error}"))?;
            let child = relative.join(entry.file_name());
            let child_text = graph_relative_text(&child)
                .ok_or_else(|| "backup contains an unsafe graph path".to_string())?;
            if file_type.is_file() && scope.is_eligible(&child_text) {
                let content = std::fs::read_to_string(entry.path())
                    .map_err(|error| format!("cannot read verified backup page: {error}"))?;
                files.push((child_text, content));
            } else if file_type.is_dir() && scope.should_descend(&child_text) {
                stack.push((entry.path(), child));
            } else if file_type.is_symlink() {
                return Err("backup contains a symbolic link".into());
            }
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

struct RestoreRecovery {
    root_path: PathBuf,
    root: Dir,
    dir: Dir,
    directory_barriers: RestoreDirectoryBarriers,
    #[cfg(test)]
    path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RestoreDirectoryRoot {
    Live,
    Recovery,
}

#[derive(Default)]
struct RestoreDirectoryBarriers {
    synced: std::sync::Mutex<
        std::collections::HashMap<
            (RestoreDirectoryRoot, std::path::PathBuf),
            tine_core::oplog::object_store::ControlDirectoryIdentity,
        >,
    >,
}

impl RestoreDirectoryBarriers {
    fn observe_or_sync_changed(
        &self,
        root: RestoreDirectoryRoot,
        relative: &std::path::Path,
        identity: tine_core::oplog::object_store::ControlDirectoryIdentity,
        directory: &Dir,
        reprove_first_observation: bool,
    ) -> std::io::Result<()> {
        let mut synced = self
            .synced
            .lock()
            .map_err(|_| std::io::Error::other("restore directory barrier cache is poisoned"))?;
        let key = (root, relative.to_path_buf());
        if synced.get(&key) == Some(&identity) {
            return Ok(());
        }
        if synced.contains_key(&key) || reprove_first_observation {
            sync_restore_directory(directory)?;
        }
        synced.insert(key, identity);
        Ok(())
    }

    fn record_changed(
        &self,
        root: RestoreDirectoryRoot,
        relative: &std::path::Path,
        identity: tine_core::oplog::object_store::ControlDirectoryIdentity,
    ) -> std::io::Result<()> {
        self.synced
            .lock()
            .map_err(|_| std::io::Error::other("restore directory barrier cache is poisoned"))?
            .insert((root, relative.to_path_buf()), identity);
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static RESTORE_DIRECTORY_SYNC_FAILURE: std::cell::Cell<Option<(usize, i32)>> =
        const { std::cell::Cell::new(None) };
    static RESTORE_DIRECTORY_SYNC_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn fail_restore_directory_sync_at(call: usize, errno: i32) {
    assert!(call > 0);
    RESTORE_DIRECTORY_SYNC_FAILURE.with(|failure| failure.set(Some((call, errno))));
}

#[cfg(test)]
fn reset_restore_directory_sync_count() {
    RESTORE_DIRECTORY_SYNC_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
fn restore_directory_sync_count() -> usize {
    RESTORE_DIRECTORY_SYNC_COUNT.with(std::cell::Cell::get)
}

fn sync_restore_directory(dir: &Dir) -> std::io::Result<()> {
    #[cfg(test)]
    RESTORE_DIRECTORY_SYNC_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    #[cfg(test)]
    RESTORE_DIRECTORY_SYNC_FAILURE.with(|failure| {
        if let Some((remaining, errno)) = failure.get() {
            if remaining == 1 {
                failure.set(None);
                return Err(std::io::Error::from_raw_os_error(errno));
            }
            failure.set(Some((remaining - 1, errno)));
        }
        Ok(())
    })?;

    match dir.try_clone()?.into_std_file().sync_all() {
        Ok(()) => Ok(()),
        Err(error) if tine_core::model::dir_fsync_error_is_unsupported(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

/// A cross-directory rename changes two directory entries. Persist the
/// destination first so the retained copy becomes durable before acknowledging
/// retirement of the only live name.
fn sync_restore_rename_parents(destination: &Dir, source: &Dir) -> std::io::Result<()> {
    sync_restore_directory(destination)?;
    sync_restore_directory(source)
}

/// Reserve and bind a unique recovery directory beneath a live graph/assets
/// capability. All later writes and moves are relative to these handles: a
/// pre-existing symlink ancestor is rejected by cap-std, and a pathname swap
/// after reservation cannot redirect recovery outside the approved root.
fn reserve_restore_recovery(
    root_path: &std::path::Path,
    recovery_parent: &std::path::Path,
    recovery_id: &str,
) -> std::io::Result<RestoreRecovery> {
    let root = Dir::open_ambient_dir(root_path, ambient_authority())?;
    let directory_barriers = RestoreDirectoryBarriers::default();
    let parent = open_or_create_real_parent(
        &root,
        recovery_parent,
        &directory_barriers,
        RestoreDirectoryRoot::Live,
        true,
    )?;
    parent.create_dir(recovery_id)?;
    sync_restore_directory(&parent)?;
    let dir = parent.open_dir(recovery_id)?;
    let recovery_relative = recovery_parent.join(recovery_id);
    let recovery_identity = tine_core::oplog::object_store::control_directory_identity(&dir)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    directory_barriers.record_changed(
        RestoreDirectoryRoot::Live,
        &recovery_relative,
        recovery_identity,
    )?;
    Ok(RestoreRecovery {
        root_path: root_path.to_path_buf(),
        root,
        dir,
        directory_barriers,
        #[cfg(test)]
        path: root_path.join(recovery_parent).join(recovery_id),
    })
}

fn open_or_create_real_parent(
    root: &Dir,
    relative: &std::path::Path,
    barriers: &RestoreDirectoryBarriers,
    root_kind: RestoreDirectoryRoot,
    reprove_first_observation: bool,
) -> std::io::Result<Dir> {
    let mut current = root.try_clone()?;
    let mut current_relative = std::path::PathBuf::new();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "restore recovery path is not relative",
            ));
        };
        let created = match current.create_dir(name) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(error),
        };
        let metadata = current.symlink_metadata(name)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "restore recovery path contains a non-directory entry",
            ));
        }
        let child = current.open_dir(name)?;
        let child_identity = tine_core::oplog::object_store::control_directory_identity(&child)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let child_relative = current_relative.join(name);
        if created {
            sync_restore_directory(&current)?;
            barriers.record_changed(root_kind, &child_relative, child_identity)?;
        } else {
            // Re-prove a possible prior-attempt residue once per exact opened
            // child identity. A concurrent replacement at the same path gets a
            // different identity and therefore a fresh parent barrier.
            barriers.observe_or_sync_changed(
                root_kind,
                &child_relative,
                child_identity,
                &current,
                reprove_first_observation,
            )?;
        }
        current = child;
        current_relative = child_relative;
    }
    Ok(current)
}

fn open_real_parent(root: &Dir, relative: &std::path::Path) -> std::io::Result<Dir> {
    let mut current = root.try_clone()?;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "live restore path is not relative",
            ));
        };
        let metadata = current.symlink_metadata(name)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "live restore path contains a non-directory entry",
            ));
        }
        current = current.open_dir(name)?;
    }
    Ok(current)
}

fn live_relative(area: &RestoreRecovery, live: &std::path::Path) -> std::io::Result<PathBuf> {
    live.strip_prefix(&area.root_path)
        .map(PathBuf::from)
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "live restore path is outside its bound root",
            )
        })
}

/// Rename relative to two already-bound directory handles and fail if the
/// destination exists. Linux/Android and Apple expose native no-replace
/// rename-at syscalls. Windows uses a capability-bound hard link followed by
/// source removal, so a concurrent sync-service delivery cannot be replaced.
fn rename_noreplace_between(
    from_dir: &Dir,
    from: &std::path::Path,
    to_dir: &Dir,
    to: &std::ffi::OsStr,
) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
        let from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let to = std::ffi::CString::new(to.as_bytes())?;
        // Invoke the renameat2 SYSCALL directly instead of libc's `renameat2`
        // wrapper. The wrapper is a bionic symbol only exported from Android
        // API 30, so linking it leaves libtine_lib.so with an unresolved
        // `renameat2` that fails `dlopen` at launch on Android 9 / API 28
        // (GH #192). The syscall itself has existed since Linux 3.15 (present on
        // Android's kernel), and `syscall` is exported since API 1, so no
        // API-gated symbol remains. Behaviour and errno handling are unchanged.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                from_dir.as_raw_fd(),
                from.as_ptr(),
                to_dir.as_raw_fd(),
                to.as_ptr(),
                libc::RENAME_NOREPLACE as libc::c_uint,
            )
        };
        return (result == 0)
            .then_some(())
            .ok_or_else(std::io::Error::last_os_error);
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
        let from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let to = std::ffi::CString::new(to.as_bytes())?;
        let result = unsafe {
            libc::renameatx_np(
                from_dir.as_raw_fd(),
                from.as_ptr(),
                to_dir.as_raw_fd(),
                to.as_ptr(),
                libc::RENAME_EXCL as libc::c_uint,
            )
        };
        return (result == 0)
            .then_some(())
            .ok_or_else(std::io::Error::last_os_error);
    }
    #[cfg(target_os = "windows")]
    {
        from_dir.hard_link(from, to_dir, std::path::Path::new(to))?;
        from_dir.remove_file(from)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    )))]
    {
        compile_error!("backup restore publication requires an audited Tine platform arm");
    }
}

/// Publish a fully-written same-directory temp without replacing a concurrent
/// creator. Android needs rename rather than hard links because graph storage
/// may live on emulated/external filesystems; Windows uses a capability-bound
/// hard link because its portable rename primitive may replace the target.
fn publish_temp_noreplace(
    parent: &Dir,
    temp: &std::path::Path,
    name: &std::ffi::OsStr,
) -> std::io::Result<()> {
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))]
    {
        rename_noreplace_between(parent, temp, parent, name)
    }
    #[cfg(target_os = "windows")]
    {
        parent.hard_link(temp, parent, std::path::Path::new(name))?;
        parent.remove_file(temp)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    )))]
    {
        compile_error!("backup temp publication requires an audited Tine platform arm");
    }
}

/// Copy a verified snapshot file into a bound live directory without following
/// ambient pathnames or replacing a concurrent creator. A same-directory temp
/// is fsynced and then published with an atomic create-if-absent rename.
fn atomic_copy_new_into_live(
    area: &RestoreRecovery,
    source: &std::path::Path,
    live: &std::path::Path,
) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COPY_SEQ: AtomicU64 = AtomicU64::new(0);

    let relative = live_relative(area, live)?;
    let name = relative.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "live restore destination has no file name",
        )
    })?;
    let parent = open_or_create_real_parent(
        &area.root,
        relative
            .parent()
            .unwrap_or_else(|| std::path::Path::new("")),
        &area.directory_barriers,
        RestoreDirectoryRoot::Live,
        false,
    )?;
    let temp = format!(
        ".tine-restore-{}-{}.tmp",
        std::process::id(),
        COPY_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut output = parent.open_with(&temp, &options)?.into_std();
        let mut input = std::fs::File::open(source)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        drop(output);
        publish_temp_noreplace(&parent, std::path::Path::new(&temp), name)?;
        // The live name is authoritative after the atomic no-replace rename.
        // Same dir-fsync policy as the save path (DUP-5): tolerate
        // "unsupported here", REPORT a real EIO/ENOSPC — a restore whose
        // rename may not survive a crash must not report success.
        sync_restore_directory(&parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = parent.remove_file(&temp);
    }
    result
}

fn ensure_target_within_root(
    root: &std::path::Path,
    target: &std::path::Path,
) -> std::io::Result<()> {
    let canonical_root = std::fs::canonicalize(root)?;
    let mut existing = target;
    while std::fs::symlink_metadata(existing).is_err() {
        existing = existing.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "target has no existing ancestor",
            )
        })?;
    }
    let canonical_existing = std::fs::canonicalize(existing)?;
    let expected = existing
        .strip_prefix(root)
        .map(|rel| canonical_root.join(rel))
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "target is outside graph root",
            )
        })?;
    if canonical_existing == expected {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target escapes graph root",
        ))
    }
}

/// Atomically detach the current live name into the restore recovery tree. A
/// writer with an open handle continues writing the recovered inode; a writer
/// that recreates the live name is left untouched. The recovery roots are kept
/// on the live graph/assets filesystems; if an unexpected nested mount still
/// makes `rename` cross-device, preserve a copy but abort the restore without
/// removing the live file rather than risk a copy-then-delete race.
fn move_live_to_recovery(
    area: &RestoreRecovery,
    live: &std::path::Path,
    recovery_relative: &std::path::Path,
) -> std::io::Result<()> {
    let live_relative = live_relative(area, live)?;
    let live_name = live_relative.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "live restore source has no file name",
        )
    })?;
    let live_parent = open_real_parent(
        &area.root,
        live_relative
            .parent()
            .unwrap_or_else(|| std::path::Path::new("")),
    )?;
    let recovery_name = recovery_relative.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "restore recovery destination has no file name",
        )
    })?;
    let recovery_parent = open_or_create_real_parent(
        &area.dir,
        recovery_relative
            .parent()
            .unwrap_or_else(|| std::path::Path::new("")),
        &area.directory_barriers,
        RestoreDirectoryRoot::Recovery,
        true,
    )?;
    match recovery_parent.symlink_metadata(recovery_name) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "restore recovery destination already exists",
            ))
        }
        Err(error) => return Err(error),
    }
    match rename_noreplace_between(
        &live_parent,
        std::path::Path::new(live_name),
        &recovery_parent,
        recovery_name,
    ) {
        Ok(()) => {
            sync_restore_rename_parents(&recovery_parent, &live_parent)?;
            Ok(())
        }
        Err(rename_err) => {
            // Unexpected nested mounts can still produce EXDEV. Preserve a
            // bounded copy inside the bound recovery directory, but leave the
            // live name untouched and abort instead of copy-then-delete.
            let mut source = live_parent.open(live_name)?.into_std();
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            let mut copy = recovery_parent
                .open_with(recovery_name, &options)?
                .into_std();
            std::io::copy(&mut source, &mut copy)?;
            copy.sync_all()?;
            sync_restore_directory(&recovery_parent)?;
            Err(std::io::Error::new(
                rename_err.kind(),
                format!(
                    "live file copied to recovery but could not be atomically detached: {rename_err}"
                ),
            ))
        }
    }
}

/// Restore graph text files in `dest` from `src`. Each file is copied through
/// the shared atomic helper, so a failure or power-loss mid-copy can never leave
/// a live note truncated/half-written. Copies happen FIRST; only after they all
/// succeed do we move `dest` graph text files not in the backup to a recovery
/// area. A copy error returns early leaving a superset of files. Other files are
/// left untouched.
static RESTORE_RECOVERY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn restore_md_dir(
    src: &std::path::Path,
    dest: &std::path::Path,
    recovery: &RestoreRecovery,
    recovery_prefix: &std::path::Path,
) -> std::io::Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    ensure_target_within_root(&recovery.root_path, dest)?;
    let dest_relative = live_relative(recovery, dest)?;
    open_or_create_real_parent(
        &recovery.root,
        &dest_relative,
        &recovery.directory_barriers,
        RestoreDirectoryRoot::Live,
        false,
    )?;
    let mut restored: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    restore_md_copy(
        src,
        dest,
        std::path::Path::new(""),
        &mut restored,
        recovery,
        recovery_prefix,
    )?;
    delete_unrestored_md(
        dest,
        std::path::Path::new(""),
        &restored,
        recovery,
        recovery_prefix,
    )?;
    Ok(())
}

fn restore_graph_text_tree(
    source: &Path,
    graph_root: &Path,
    scope: &GraphTextScope,
    recovery: &RestoreRecovery,
) -> std::io::Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    ensure_target_within_root(&recovery.root_path, graph_root)?;
    let mut restored = std::collections::HashSet::new();
    restore_scoped_graph_text_copy(
        source,
        graph_root,
        Path::new(""),
        scope,
        &mut restored,
        recovery,
    )?;
    retire_unrestored_graph_text(graph_root, Path::new(""), scope, &restored, recovery)
}

fn restore_scoped_graph_text_copy(
    source: &Path,
    graph_root: &Path,
    relative: &Path,
    scope: &GraphTextScope,
    restored: &mut std::collections::HashSet<PathBuf>,
    recovery: &RestoreRecovery,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(source.join(relative))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let child = relative.join(entry.file_name());
        let child_text = graph_relative_text(&child).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "non-UTF-8 graph path")
        })?;
        if file_type.is_file() && scope.is_eligible(&child_text) {
            let target = graph_root.join(&child);
            ensure_target_within_root(&recovery.root_path, &target)?;
            if target.exists() {
                move_live_to_recovery(recovery, &target, &Path::new("graph").join(&child))?;
            }
            atomic_copy_new_into_live(recovery, &entry.path(), &target)?;
            restored.insert(child);
        } else if file_type.is_dir() && scope.should_descend(&child_text) {
            ensure_target_within_root(&recovery.root_path, &graph_root.join(&child))?;
            restore_scoped_graph_text_copy(source, graph_root, &child, scope, restored, recovery)?;
        } else if file_type.is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "snapshot graph tree contains a symbolic link",
            ));
        }
    }
    Ok(())
}

fn retire_unrestored_graph_text(
    graph_root: &Path,
    relative: &Path,
    scope: &GraphTextScope,
    restored: &std::collections::HashSet<PathBuf>,
    recovery: &RestoreRecovery,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(graph_root.join(relative))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let child = relative.join(entry.file_name());
        let Some(child_text) = graph_relative_text(&child) else {
            continue;
        };
        if file_type.is_file() && scope.is_eligible(&child_text) {
            if !restored.contains(&child) {
                let live = graph_root.join(&child);
                ensure_target_within_root(&recovery.root_path, &live)?;
                move_live_to_recovery(recovery, &live, &Path::new("graph").join(&child))?;
            }
        } else if file_type.is_dir() && scope.should_descend(&child_text) {
            retire_unrestored_graph_text(graph_root, &child, scope, restored, recovery)?;
        }
    }
    Ok(())
}

fn restore_md_copy(
    src: &std::path::Path,
    dest: &std::path::Path,
    rel: &std::path::Path,
    restored: &mut std::collections::HashSet<std::path::PathBuf>,
    recovery: &RestoreRecovery,
    recovery_prefix: &std::path::Path,
) -> std::io::Result<()> {
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let p = e.path();
        let rel_child = rel.join(e.file_name());
        if is_graph_text(&p) {
            let target = dest.join(&rel_child);
            ensure_target_within_root(&recovery.root_path, &target)?;
            if target.exists() {
                move_live_to_recovery(recovery, &target, &recovery_prefix.join(&rel_child))?;
            }
            atomic_copy_new_into_live(recovery, &p, &target)?;
            restored.insert(rel_child);
        } else if is_visible_real_dir(&e)? {
            ensure_target_within_root(&recovery.root_path, &dest.join(&rel_child))?;
            let child_relative = live_relative(recovery, &dest.join(&rel_child))?;
            open_or_create_real_parent(
                &recovery.root,
                &child_relative,
                &recovery.directory_barriers,
                RestoreDirectoryRoot::Live,
                false,
            )?;
            restore_md_copy(&p, dest, &rel_child, restored, recovery, recovery_prefix)?;
        }
    }
    Ok(())
}

fn delete_unrestored_md(
    dest: &std::path::Path,
    rel: &std::path::Path,
    restored: &std::collections::HashSet<std::path::PathBuf>,
    recovery: &RestoreRecovery,
    recovery_prefix: &std::path::Path,
) -> std::io::Result<()> {
    let dir = dest.join(rel);
    if !dir.is_dir() {
        return Ok(());
    }
    for e in std::fs::read_dir(&dir)? {
        let e = e?;
        let p = e.path();
        let rel_child = rel.join(e.file_name());
        if is_graph_text(&p) {
            if !restored.contains(&rel_child) {
                ensure_target_within_root(&recovery.root_path, &p)?;
                move_live_to_recovery(recovery, &p, &recovery_prefix.join(&rel_child))?;
            }
        } else if is_visible_real_dir(&e)? {
            delete_unrestored_md(dest, &rel_child, restored, recovery, recovery_prefix)?;
        }
    }
    Ok(())
}

fn restore_asset_sidecars_dir(
    src: &std::path::Path,
    dest: &std::path::Path,
    recovery: &RestoreRecovery,
    recovery_prefix: &std::path::Path,
) -> std::io::Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    ensure_target_within_root(&recovery.root_path, dest)?;
    let dest_relative = live_relative(recovery, dest)?;
    open_or_create_real_parent(
        &recovery.root,
        &dest_relative,
        &recovery.directory_barriers,
        RestoreDirectoryRoot::Live,
        false,
    )?;
    let mut restored: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    restore_asset_sidecars_copy(
        src,
        dest,
        std::path::Path::new(""),
        &mut restored,
        recovery,
        recovery_prefix,
    )?;
    delete_unrestored_asset_sidecars(
        dest,
        std::path::Path::new(""),
        &restored,
        recovery,
        recovery_prefix,
    )?;
    Ok(())
}

fn restore_asset_sidecars_copy(
    src: &std::path::Path,
    dest: &std::path::Path,
    rel: &std::path::Path,
    restored: &mut std::collections::HashSet<std::path::PathBuf>,
    recovery: &RestoreRecovery,
    recovery_prefix: &std::path::Path,
) -> std::io::Result<()> {
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let ft = e.file_type()?;
        let rel_child = rel.join(e.file_name());
        let p = e.path();
        if ft.is_dir() && !is_asset_restore_recovery_entry(&e) {
            ensure_target_within_root(&recovery.root_path, &dest.join(&rel_child))?;
            let child_relative = live_relative(recovery, &dest.join(&rel_child))?;
            open_or_create_real_parent(
                &recovery.root,
                &child_relative,
                &recovery.directory_barriers,
                RestoreDirectoryRoot::Live,
                false,
            )?;
            restore_asset_sidecars_copy(&p, dest, &rel_child, restored, recovery, recovery_prefix)?;
        } else if ft.is_file() && is_asset_sidecar(&p) {
            let target = dest.join(&rel_child);
            ensure_target_within_root(&recovery.root_path, &target)?;
            if target.exists() {
                move_live_to_recovery(recovery, &target, &recovery_prefix.join(&rel_child))?;
            }
            atomic_copy_new_into_live(recovery, &p, &target)?;
            restored.insert(rel_child);
        }
    }
    Ok(())
}

fn delete_unrestored_asset_sidecars(
    dest: &std::path::Path,
    rel: &std::path::Path,
    restored: &std::collections::HashSet<std::path::PathBuf>,
    recovery: &RestoreRecovery,
    recovery_prefix: &std::path::Path,
) -> std::io::Result<()> {
    let dir = dest.join(rel);
    if !dir.is_dir() {
        return Ok(());
    }
    for e in std::fs::read_dir(&dir)? {
        let e = e?;
        let ft = e.file_type()?;
        let rel_child = rel.join(e.file_name());
        let p = e.path();
        if ft.is_dir() && !is_asset_restore_recovery_entry(&e) {
            delete_unrestored_asset_sidecars(
                dest,
                &rel_child,
                restored,
                recovery,
                recovery_prefix,
            )?;
        } else if ft.is_file() && is_asset_sidecar(&p) && !restored.contains(&rel_child) {
            ensure_target_within_root(&recovery.root_path, &p)?;
            move_live_to_recovery(recovery, &p, &recovery_prefix.join(&rel_child))?;
        }
    }
    Ok(())
}

/// Page/journal text files Tine snapshots + restores: Markdown and Org. Asset
/// `.edn` sidecars are handled separately under `assets`; binary asset bytes stay
/// excluded from snapshots by design.
fn is_graph_text(p: &std::path::Path) -> bool {
    p.extension().and_then(|x| x.to_str()).is_some_and(|ext| {
        ext.eq_ignore_ascii_case("md")
            || ext.eq_ignore_ascii_case("markdown")
            || ext.eq_ignore_ascii_case("org")
    })
}

fn graph_relative_text(relative: &Path) -> Option<String> {
    let components = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    (!components.is_empty()).then(|| components.join("/"))
}

fn is_asset_sidecar(p: &std::path::Path) -> bool {
    matches!(p.extension().and_then(|x| x.to_str()), Some("edn"))
}

fn is_asset_restore_recovery_entry(e: &std::fs::DirEntry) -> bool {
    e.file_name() == std::ffi::OsStr::new(ASSET_RESTORE_RECOVERY_DIR)
}

fn is_visible_real_dir(e: &std::fs::DirEntry) -> std::io::Result<bool> {
    let hidden = e
        .file_name()
        .to_str()
        .map(|s| s.starts_with('.'))
        .unwrap_or(true);
    if hidden {
        return Ok(false);
    }
    e.file_type().map(|ft| ft.is_dir())
}

fn dir_name(p: &std::path::Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("dir")
        .to_string()
}
/// Copy every graph text file from `src` to `dest`. Returns (copied, failed) so
/// the caller can tell a complete snapshot from a partial one.
#[cfg(test)]
fn copy_md_dir(src: &std::path::Path, dest: &std::path::Path) -> (usize, usize) {
    copy_md_dir_cancellable(src, dest, &|| false)
}

#[cfg(test)]
fn copy_md_dir_cancellable(
    src: &std::path::Path,
    dest: &std::path::Path,
    cancelled: &dyn Fn() -> bool,
) -> (usize, usize) {
    if cancelled() {
        return (0, 1);
    }
    // Materialize the dest dir up front, even when src has no .md files — so the
    // snapshot records "this dir existed and was empty". Otherwise restore can't
    // tell an empty-at-backup dir from a missing one, and leaves destination .md
    // extras in place (mixing current files into the restored snapshot).
    let _ = std::fs::create_dir_all(dest);
    match std::fs::read_dir(src) {
        Ok(_) => {}
        // A genuinely-absent source dir (e.g. a graph with no pages/) is not a
        // failure — there's nothing to snapshot. But a dir we CAN'T read
        // (permission / I/O) MUST count as failed, so a pre-restore safety
        // snapshot isn't falsely reported complete and a destructive restore can
        // refuse to proceed without a trustworthy rollback.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (0, 0),
        Err(_) => return (0, 1),
    }
    let (mut copied, mut failed) = (0usize, 0usize);
    let mut stack = vec![(src.to_path_buf(), std::path::PathBuf::new())];
    while let Some((dir, rel)) = stack.pop() {
        if cancelled() {
            return (copied, failed + 1);
        }
        let target_dir = dest.join(&rel);
        let _ = std::fs::create_dir_all(&target_dir);
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => {
                failed += 1;
                continue;
            }
        };
        for entry in rd {
            if cancelled() {
                return (copied, failed + 1);
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    failed += 1;
                    continue;
                }
            };
            let p = entry.path();
            let rel_child = rel.join(entry.file_name());
            if is_graph_text(&p) {
                let target = dest.join(&rel_child);
                let copied_ok = target
                    .parent()
                    .map(std::fs::create_dir_all)
                    .unwrap_or(Ok(()))
                    .is_ok()
                    && std::fs::copy(&p, &target).is_ok();
                if copied_ok {
                    copied += 1;
                } else {
                    failed += 1;
                }
            } else {
                match is_visible_real_dir(&entry) {
                    Ok(true) => stack.push((p, rel_child)),
                    Ok(false) => {}
                    Err(_) => failed += 1,
                }
            }
        }
    }
    (copied, failed)
}

fn copy_graph_text_tree_cancellable(
    root: &Path,
    destination: &Path,
    scope: &GraphTextScope,
    cancelled: &dyn Fn() -> bool,
) -> (usize, usize) {
    if cancelled() {
        return (0, 1);
    }
    if let Err(error) = std::fs::read_dir(root) {
        return if error.kind() == std::io::ErrorKind::NotFound {
            (0, 0)
        } else {
            (0, 1)
        };
    }
    if std::fs::create_dir_all(destination).is_err() {
        return (0, 1);
    }

    let (mut copied, mut failed) = (0usize, 0usize);
    let mut stack = vec![(root.to_path_buf(), PathBuf::new())];
    while let Some((directory, relative)) = stack.pop() {
        if cancelled() {
            return (copied, failed + 1);
        }
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                failed += 1;
                continue;
            }
        };
        for entry in entries {
            if cancelled() {
                return (copied, failed + 1);
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    failed += 1;
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => {
                    failed += 1;
                    continue;
                }
            };
            let child = relative.join(entry.file_name());
            let Some(child_text) = graph_relative_text(&child) else {
                continue;
            };
            if file_type.is_file() && scope.is_eligible(&child_text) {
                let target = destination.join(&child);
                let copied_ok = target
                    .parent()
                    .map(std::fs::create_dir_all)
                    .unwrap_or(Ok(()))
                    .is_ok()
                    && std::fs::copy(entry.path(), target).is_ok();
                if copied_ok {
                    copied += 1;
                } else {
                    failed += 1;
                }
            } else if file_type.is_dir() && scope.should_descend(&child_text) {
                stack.push((entry.path(), child));
            }
        }
    }
    (copied, failed)
}

#[cfg(test)]
fn copy_asset_sidecars_dir(src: &std::path::Path, dest: &std::path::Path) -> (usize, usize) {
    copy_asset_sidecars_dir_cancellable(src, dest, &|| false)
}

fn copy_asset_sidecars_dir_cancellable(
    src: &std::path::Path,
    dest: &std::path::Path,
    cancelled: &dyn Fn() -> bool,
) -> (usize, usize) {
    if cancelled() {
        return (0, 1);
    }
    let _ = std::fs::create_dir_all(dest);
    let rd = match std::fs::read_dir(src) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (0, 0),
        Err(_) => return (0, 1),
    };
    let (mut copied, mut failed) = (0usize, 0usize);
    for entry in rd {
        if cancelled() {
            return (copied, failed + 1);
        }
        let Ok(entry) = entry else {
            failed += 1;
            continue;
        };
        let p = entry.path();
        let Ok(ft) = entry.file_type() else {
            failed += 1;
            continue;
        };
        let target = dest.join(entry.file_name());
        if ft.is_dir() && !is_asset_restore_recovery_entry(&entry) {
            let (c, f) = copy_asset_sidecars_dir_cancellable(&p, &target, cancelled);
            copied += c;
            failed += f;
        } else if ft.is_file() && is_asset_sidecar(&p) {
            if std::fs::create_dir_all(dest).is_ok() && std::fs::copy(&p, &target).is_ok() {
                copied += 1;
            } else {
                failed += 1;
            }
        }
    }
    (copied, failed)
}
fn prune_backups(base: &std::path::Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(base) else {
        return;
    };
    // Only the routine launch snapshots are subject to the keep-count. Tagged
    // snapshots (e.g. "...-pre-restore") are deliberate safety points and are
    // never auto-pruned.
    let mut dirs: Vec<std::path::PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && !p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.starts_with(".partial-"))
                    .unwrap_or(true)
                && !p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|s| s.contains("-pre-restore"))
                    .unwrap_or(false)
        })
        .collect();
    dirs.sort(); // timestamp-named → chronological
    if dirs.len() > keep {
        for d in &dirs[..dirs.len() - keep] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
/// UTC `YYYY-MM-DD_HH-MM-SS` from the system clock (Hinnant civil-from-days).
fn backup_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}_{h:02}-{mi:02}-{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tine-tauri-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn backup_root_ids_do_not_conflate_punctuation() {
        let root = scratch("backup-root-id");
        let dash = root.join("a-b");
        let underscore = root.join("a_b");
        std::fs::create_dir_all(&dash).unwrap();
        std::fs::create_dir_all(&underscore).unwrap();
        assert_ne!(root_backup_id(&dash), root_backup_id(&underscore));
        assert_eq!(root_backup_id(&dash), root_backup_id(&dash));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_and_abandoned_partial_backups_are_removed() {
        let root = scratch("partial-backup-cleanup");
        let failed = root.join(".partial-failed");
        std::fs::create_dir_all(&failed).unwrap();
        std::fs::write(failed.join("half.md"), b"partial").unwrap();
        {
            let _guard = PartialBackup {
                path: failed.clone(),
                committed: false,
            };
        }
        assert!(!failed.exists());

        let crashed = root.join(".partial-crashed");
        std::fs::create_dir_all(&crashed).unwrap();
        std::fs::write(crashed.join("half.md"), b"partial").unwrap();
        cleanup_partial_backups(&root);
        assert!(!crashed.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancellable_copy_stops_before_traversing_the_tree() {
        let root = scratch("backup-cancel");
        let src = root.join("src");
        let dest = root.join("dest");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("note.md"), b"secret").unwrap();
        assert_eq!(copy_md_dir_cancellable(&src, &dest, &|| true), (0, 1));
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restore_recovery_roots_live_on_the_filesystems_they_detach_from() {
        let root = scratch("restore-recovery-roots");
        let graph = root.join("mounted-graph");
        let assets = root.join("mounted-assets");
        std::fs::create_dir_all(graph.join("logseq")).unwrap();
        std::fs::create_dir_all(&assets).unwrap();
        let graph_recovery = reserve_restore_recovery(
            &graph,
            std::path::Path::new("logseq/.tine-trash"),
            "restore-1",
        )
        .unwrap();
        let asset_recovery = reserve_restore_recovery(
            &assets,
            std::path::Path::new(ASSET_RESTORE_RECOVERY_DIR),
            "restore-1",
        )
        .unwrap();

        assert!(graph_recovery
            .path
            .starts_with(graph.join("logseq/.tine-trash")));
        assert!(asset_recovery
            .path
            .starts_with(assets.join(".tine-restore-recovery")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restore_recovery_creation_refuses_to_ack_a_failed_directory_barrier() {
        let root = scratch("restore-recovery-create-barrier");
        std::fs::create_dir_all(root.join("logseq/.tine-trash")).unwrap();

        fail_restore_directory_sync_at(3, 5);
        let error = match reserve_restore_recovery(
            &root,
            std::path::Path::new("logseq/.tine-trash"),
            "restore-failure",
        ) {
            Ok(_) => panic!("a real directory barrier failure must reject recovery reservation"),
            Err(error) => error,
        };
        assert_eq!(error.raw_os_error(), Some(5));
        assert!(root.join("logseq/.tine-trash/restore-failure").is_dir());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restore_retirement_preserves_complete_bytes_when_a_directory_barrier_fails() {
        let root = scratch("restore-retire-barrier");
        let live = root.join("pages/note.md");
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, b"authoritative live bytes").unwrap();
        let recovery = reserve_restore_recovery(
            &root,
            std::path::Path::new("logseq/.tine-trash"),
            "restore-retire",
        )
        .unwrap();
        std::fs::create_dir_all(recovery.path.join("graph")).unwrap();

        fail_restore_directory_sync_at(2, 5);
        let error = move_live_to_recovery(&recovery, &live, std::path::Path::new("graph/note.md"))
            .expect_err("retirement must not report success before both directory barriers");
        assert_eq!(error.raw_os_error(), Some(5));
        assert!(!live.exists());
        assert_eq!(
            std::fs::read(recovery.path.join("graph/note.md")).unwrap(),
            b"authoritative live bytes"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restore_publication_never_acknowledges_an_unsynced_live_name() {
        let root = scratch("restore-publish-barrier");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        let source = root.join("snapshot-note.md");
        let live = root.join("pages/note.md");
        std::fs::write(&source, b"complete snapshot bytes").unwrap();
        let recovery = reserve_restore_recovery(
            &root,
            std::path::Path::new("logseq/.tine-trash"),
            "restore-publish",
        )
        .unwrap();

        fail_restore_directory_sync_at(1, 5);
        let error = atomic_copy_new_into_live(&recovery, &source, &live)
            .expect_err("publication must fail closed when its directory barrier fails");
        assert_eq!(error.raw_os_error(), Some(5));
        assert_eq!(std::fs::read(&live).unwrap(), b"complete snapshot bytes");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restore_sibling_publications_do_not_repeat_ancestor_barriers() {
        let root = scratch("restore-sibling-barriers");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        let first_source = root.join("snapshot-first.md");
        let second_source = root.join("snapshot-second.md");
        std::fs::write(&first_source, b"first").unwrap();
        std::fs::write(&second_source, b"second").unwrap();
        let recovery = reserve_restore_recovery(
            &root,
            std::path::Path::new("logseq/.tine-trash"),
            "restore-siblings",
        )
        .unwrap();

        reset_restore_directory_sync_count();
        atomic_copy_new_into_live(&recovery, &first_source, &root.join("pages/first.md")).unwrap();
        atomic_copy_new_into_live(&recovery, &second_source, &root.join("pages/second.md"))
            .unwrap();
        assert_eq!(
            restore_directory_sync_count(),
            2,
            "each sibling needs its changed leaf-parent barrier, but stable ancestors are proved once per restore"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restore_rebarriers_an_honestly_replaced_directory_identity() {
        let root = scratch("restore-replaced-directory-barrier");
        let pages = root.join("pages");
        let displaced = root.join("pages.displaced");
        std::fs::create_dir_all(&pages).unwrap();
        let first_source = root.join("snapshot-first.md");
        let second_source = root.join("snapshot-second.md");
        std::fs::write(&first_source, b"first").unwrap();
        std::fs::write(&second_source, b"second").unwrap();
        let recovery = reserve_restore_recovery(
            &root,
            std::path::Path::new("logseq/.tine-trash"),
            "restore-replacement",
        )
        .unwrap();

        reset_restore_directory_sync_count();
        atomic_copy_new_into_live(&recovery, &first_source, &pages.join("first.md")).unwrap();
        std::fs::rename(&pages, &displaced).unwrap();
        std::fs::create_dir(&pages).unwrap();
        atomic_copy_new_into_live(&recovery, &second_source, &pages.join("second.md")).unwrap();

        assert_eq!(std::fs::read(displaced.join("first.md")).unwrap(), b"first");
        assert_eq!(std::fs::read(pages.join("second.md")).unwrap(), b"second");
        assert_eq!(
            restore_directory_sync_count(),
            3,
            "two leaf publications plus one changed-identity parent barrier are required"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restore_stack_keeps_capability_bound_publication_and_orders_crash_barriers() {
        let source = include_str!("backup.rs");
        let shared_publish = ["tine_core::model::", "atomic_write("].concat();
        assert!(
            !source.contains(&shared_publish),
            "backup restore keeps its separate capability-bound publication stack"
        );

        let reserve = function_source(source, "fn reserve_restore_recovery(");
        assert!(reserve.contains("sync_restore_directory(&parent)?"));

        let create_parent = function_source(source, "fn open_or_create_real_parent(");
        assert!(create_parent.contains("sync_restore_directory(&current)?"));

        let publish = function_source(source, "fn atomic_copy_new_into_live(");
        assert!(publish.contains("sync_restore_directory(&parent)?"));

        let retire = function_source(source, "fn move_live_to_recovery(");
        assert!(retire.contains("sync_restore_rename_parents(&recovery_parent, &live_parent)?"));

        let no_replace = function_source(source, "fn rename_noreplace_between(");
        assert!(no_replace.contains("#[cfg(target_os = \"windows\")]"));
        assert!(no_replace.contains("from_dir.hard_link("));
        assert!(no_replace.contains("target_os = \"windows\""));
        assert!(no_replace.contains("compile_error!"));
        assert!(
            !no_replace.contains("#[cfg(not(any(\n        target_os = \"linux\",\n        target_os = \"android\",\n        target_os = \"macos\",\n        target_os = \"ios\"\n    )))]"),
            "the fallback must not silently mean Windows"
        );
    }

    fn function_source<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source.find(signature).expect("function remains present");
        &source[start
            ..source[start..]
                .find("\n}\n")
                .map(|offset| start + offset + 3)
                .unwrap_or(source.len())]
    }

    #[cfg(unix)]
    #[test]
    fn restore_recovery_symlink_cannot_redirect_or_replace_outside() {
        use std::os::unix::fs::symlink;

        let root = scratch("restore-recovery-symlink");
        let graph = root.join("graph");
        let outside = root.join("outside");
        let live = graph.join("pages/secret.md");
        let outside_target = outside.join("restore-1/pages/secret.md");
        std::fs::create_dir_all(graph.join("logseq")).unwrap();
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::create_dir_all(outside_target.parent().unwrap()).unwrap();
        std::fs::write(&live, b"live graph data").unwrap();
        std::fs::write(&outside_target, b"outside sentinel").unwrap();
        symlink(&outside, graph.join("logseq/.tine-trash")).unwrap();

        let result = reserve_restore_recovery(
            &graph,
            std::path::Path::new("logseq/.tine-trash"),
            "restore-1",
        );

        assert!(
            result.is_err(),
            "a symlinked recovery ancestor must be rejected"
        );
        assert!(
            live.exists(),
            "an unsafe recovery setup must leave the live file untouched"
        );
        assert_eq!(std::fs::read(&outside_target).unwrap(), b"outside sentinel");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn restore_recovery_path_swap_stays_on_the_bound_directory() {
        use std::os::unix::fs::symlink;

        let root = scratch("restore-recovery-swap");
        let graph = root.join("graph");
        let live = graph.join("pages/secret.md");
        let outside = root.join("outside");
        std::fs::create_dir_all(graph.join("logseq")).unwrap();
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::create_dir_all(outside.join("pages")).unwrap();
        std::fs::write(&live, b"live graph data").unwrap();
        std::fs::write(outside.join("pages/secret.md"), b"outside sentinel").unwrap();
        let recovery = reserve_restore_recovery(
            &graph,
            std::path::Path::new("logseq/.tine-trash"),
            "restore-1",
        )
        .unwrap();
        let displaced = recovery.path.with_extension("displaced");
        std::fs::rename(&recovery.path, &displaced).unwrap();
        symlink(&outside, &recovery.path).unwrap();

        move_live_to_recovery(&recovery, &live, std::path::Path::new("pages/secret.md")).unwrap();

        assert!(!live.exists());
        assert_eq!(
            std::fs::read(displaced.join("pages/secret.md")).unwrap(),
            b"live graph data"
        );
        assert_eq!(
            std::fs::read(outside.join("pages/secret.md")).unwrap(),
            b"outside sentinel"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn restore_live_path_swap_cannot_move_or_publish_outside() {
        use std::os::unix::fs::symlink;

        let root = scratch("restore-live-swap");
        let graph = root.join("graph");
        let pages = graph.join("pages");
        let outside = root.join("outside");
        let snapshot = root.join("snapshot.md");
        std::fs::create_dir_all(graph.join("logseq")).unwrap();
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(pages.join("secret.md"), b"live graph data").unwrap();
        std::fs::write(outside.join("secret.md"), b"outside sentinel").unwrap();
        std::fs::write(&snapshot, b"snapshot data").unwrap();
        let recovery = reserve_restore_recovery(
            &graph,
            std::path::Path::new("logseq/.tine-trash"),
            "restore-1",
        )
        .unwrap();
        let displaced = graph.join("pages.displaced");
        std::fs::rename(&pages, &displaced).unwrap();
        symlink(&outside, &pages).unwrap();

        assert!(move_live_to_recovery(
            &recovery,
            &pages.join("secret.md"),
            std::path::Path::new("pages/secret.md"),
        )
        .is_err());
        assert!(atomic_copy_new_into_live(&recovery, &snapshot, &pages.join("new.md")).is_err());

        assert_eq!(
            std::fs::read(displaced.join("secret.md")).unwrap(),
            b"live graph data"
        );
        assert_eq!(
            std::fs::read(outside.join("secret.md")).unwrap(),
            b"outside sentinel"
        );
        assert!(!outside.join("new.md").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restore_recovery_never_replaces_an_existing_entry() {
        let root = scratch("restore-recovery-no-replace");
        let graph = root.join("graph");
        let live = graph.join("pages/secret.md");
        std::fs::create_dir_all(graph.join("logseq")).unwrap();
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, b"live graph data").unwrap();
        let recovery = reserve_restore_recovery(
            &graph,
            std::path::Path::new("logseq/.tine-trash"),
            "restore-1",
        )
        .unwrap();
        recovery.dir.create_dir("pages").unwrap();
        recovery
            .dir
            .write("pages/secret.md", b"recovery sentinel")
            .unwrap();

        assert!(
            move_live_to_recovery(&recovery, &live, std::path::Path::new("pages/secret.md"),)
                .is_err()
        );
        assert_eq!(std::fs::read(&live).unwrap(), b"live graph data");
        assert_eq!(
            std::fs::read(recovery.path.join("pages/secret.md")).unwrap(),
            b"recovery sentinel"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_backup_recovery_rejects_a_concurrent_sync_delivery() {
        let root = scratch("windows-restore-concurrent-delivery");
        let live_path = root.join("live");
        let recovery_path = root.join("recovery");
        std::fs::create_dir_all(&live_path).unwrap();
        std::fs::create_dir_all(&recovery_path).unwrap();
        std::fs::write(live_path.join("page.md"), b"live source").unwrap();
        std::fs::write(recovery_path.join("page.md"), b"sync delivery").unwrap();
        let live = Dir::open_ambient_dir(&live_path, cap_std::ambient_authority()).unwrap();
        let recovery = Dir::open_ambient_dir(&recovery_path, cap_std::ambient_authority()).unwrap();

        assert!(rename_noreplace_between(
            &live,
            std::path::Path::new("page.md"),
            &recovery,
            std::ffi::OsStr::new("page.md"),
        )
        .is_err());
        assert_eq!(
            std::fs::read(live_path.join("page.md")).unwrap(),
            b"live source"
        );
        assert_eq!(
            std::fs::read(recovery_path.join("page.md")).unwrap(),
            b"sync delivery"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn complete_v2_and_v3_manifests_are_readable() {
        let root = scratch("backup-manifest");
        let mut manifest = SnapshotManifest {
            schema: LEGACY_SNAPSHOT_SCHEMA,
            root: root.display().to_string(),
            journals_dir: "diary".into(),
            pages_dir: "archive/pages".into(),
            graph_text_policy: None,
            files: Vec::new(),
            complete: true,
        };
        write_manifest(&root, &manifest).unwrap();
        let read = read_manifest(&root).unwrap();
        assert_eq!(read.pages_dir, "archive/pages");
        assert!(verify_snapshot(&root, &read));
        let v3 = root.join("v3");
        std::fs::create_dir_all(&v3).unwrap();
        manifest.schema = SNAPSHOT_SCHEMA;
        manifest.graph_text_policy = Some(SnapshotGraphTextPolicy {
            version: GRAPH_TEXT_SCOPE_VERSION,
            hidden: Vec::new(),
            hidden_parse_failed_closed: false,
        });
        write_manifest(&v3, &manifest).unwrap();
        assert_eq!(read_manifest(&v3).unwrap().schema, SNAPSHOT_SCHEMA);
        std::fs::write(root.join("journals.md"), "- changed\n").unwrap();
        assert!(!verify_snapshot(&root, &read));
        std::fs::remove_file(root.join("journals.md")).unwrap();
        std::fs::write(
            root.join(SNAPSHOT_MANIFEST),
            r#"{"schema":2,"root":"x","journals_dir":"journals","pages_dir":"pages","files":[],"complete":false}"#,
        )
        .unwrap();
        assert!(read_manifest(&root).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn manifest_listing_never_hashes_snapshot_payloads() {
        let root = scratch("manifest-only-listing");
        let graph = root.join("graph");
        let base = root.join("backups");
        let snapshot = base.join("2026-07-22_12-00-00");
        std::fs::create_dir_all(graph.join("pages")).unwrap();
        std::fs::create_dir_all(snapshot.join("pages")).unwrap();
        std::fs::write(snapshot.join("pages/note.md"), b"tampered payload").unwrap();
        write_manifest(
            &snapshot,
            &SnapshotManifest {
                schema: LEGACY_SNAPSHOT_SCHEMA,
                root: std::fs::canonicalize(&graph).unwrap().display().to_string(),
                journals_dir: "journals".into(),
                pages_dir: "pages".into(),
                graph_text_policy: None,
                files: vec![SnapshotFile {
                    path: "pages/note.md".into(),
                    sha256: "manifest metadata only".into(),
                }],
                complete: true,
            },
        )
        .unwrap();

        PAYLOAD_HASH_READS.with(|reads| reads.set(0));
        let listed = list_backups_from_base(&base, &graph);

        assert_eq!(
            PAYLOAD_HASH_READS.with(|reads| reads.get()),
            0,
            "listing must not read or hash snapshot payloads"
        );
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].files, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restore_verifies_a_selected_snapshot_before_mutating_the_graph() {
        let root = scratch("restore-verification-before-mutation");
        let graph = root.join("graph");
        let base = root.join("backups");
        let stamp = "2026-07-22_12-00-00";
        let snapshot = base.join(stamp);
        let live_page = graph.join("pages/note.md");
        std::fs::create_dir_all(live_page.parent().unwrap()).unwrap();
        std::fs::create_dir_all(snapshot.join("pages")).unwrap();
        std::fs::write(&live_page, b"live graph data").unwrap();
        std::fs::write(snapshot.join("pages/note.md"), b"tampered payload").unwrap();
        write_manifest(
            &snapshot,
            &SnapshotManifest {
                schema: LEGACY_SNAPSHOT_SCHEMA,
                root: std::fs::canonicalize(&graph).unwrap().display().to_string(),
                journals_dir: "journals".into(),
                pages_dir: "pages".into(),
                graph_text_policy: None,
                files: vec![SnapshotFile {
                    path: "pages/note.md".into(),
                    sha256: "does not match the payload".into(),
                }],
                complete: true,
            },
        )
        .unwrap();
        let source = BackupSource {
            assets: graph.join("assets"),
            cfg: graph.join("logseq/config.edn"),
            root: graph.clone(),
            journals_dir: "journals".into(),
            pages_dir: "pages".into(),
            graph_text_scope: GraphTextScope::new(&[], false),
            graph_text_policy: SnapshotGraphTextPolicy {
                version: GRAPH_TEXT_SCOPE_VERSION,
                hidden: Vec::new(),
                hidden_parse_failed_closed: false,
            },
        };

        PAYLOAD_HASH_READS.with(|reads| reads.set(0));
        let result = restore_from_backup_source(stamp, &base, source, |_| {
            std::fs::write(&live_page, b"mutated graph data").unwrap();
            (1, true)
        });

        assert_eq!(
            PAYLOAD_HASH_READS.with(|reads| reads.get()),
            1,
            "restoring must verify the selected snapshot payload"
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(&live_page).unwrap(), b"live graph data");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn copy_asset_sidecars_dir_copies_only_edn_recursively() {
        let root = scratch("copy-sidecars");
        let src = root.join("assets");
        let dst = root.join("backup").join("assets");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("doc.edn"), "{:a 1}\n").unwrap();
        std::fs::write(src.join("nested").join("hl.edn"), "{:b 2}\n").unwrap();
        std::fs::write(src.join("image.png"), b"png").unwrap();
        std::fs::write(src.join("nested").join("image.png"), b"png").unwrap();
        std::fs::create_dir_all(src.join(ASSET_RESTORE_RECOVERY_DIR)).unwrap();
        std::fs::write(
            src.join(ASSET_RESTORE_RECOVERY_DIR).join("old.edn"),
            "{:old true}\n",
        )
        .unwrap();

        assert_eq!(copy_asset_sidecars_dir(&src, &dst), (2, 0));
        assert_eq!(
            std::fs::read_to_string(dst.join("doc.edn")).unwrap(),
            "{:a 1}\n"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("nested").join("hl.edn")).unwrap(),
            "{:b 2}\n"
        );
        assert!(!dst.join("image.png").exists());
        assert!(!dst.join("nested").join("image.png").exists());
        assert!(!dst.join(ASSET_RESTORE_RECOVERY_DIR).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_preflight_collects_nested_text_with_configured_graph_paths() {
        let root = scratch("restore-preflight");
        let snapshot = root.join("snapshot");
        std::fs::create_dir_all(snapshot.join("journals")).unwrap();
        std::fs::create_dir_all(snapshot.join("pages").join("nested")).unwrap();
        std::fs::write(snapshot.join("journals").join("day.org"), "* day\n").unwrap();
        std::fs::write(
            snapshot.join("pages").join("nested").join("Page.md"),
            "- page\n",
        )
        .unwrap();
        std::fs::write(snapshot.join("pages").join("ignored.txt"), "ignored\n").unwrap();

        let files = collect_legacy_restore_graph_text(&snapshot, "diary", "notes").unwrap();
        assert_eq!(
            files,
            vec![
                ("diary/day.org".into(), "* day\n".into()),
                ("notes/nested/Page.md".into(), "- page\n".into()),
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn graph_wide_snapshot_preserves_eligible_paths_and_excludes_internal_trees() {
        let root = scratch("graph-wide-snapshot");
        let graph = root.join("graph");
        let snapshot = root.join("snapshot/graph");
        for directory in [
            "pages",
            "archive/自由",
            "assets",
            "logseq/.tine-trash/pages",
            ".hidden",
            "private",
        ] {
            std::fs::create_dir_all(graph.join(directory)).unwrap();
        }
        for (relative, bytes) in [
            ("Root.md", b"root\n".as_slice()),
            ("pages/Normal.org", b"* normal\n".as_slice()),
            ("archive/自由/Elsewhere.Markdown", b"elsewhere\n".as_slice()),
            ("assets/ignored.md", b"asset\n".as_slice()),
            ("logseq/.tine-trash/pages/ignored.md", b"trash\n".as_slice()),
            (".hidden/ignored.md", b"hidden\n".as_slice()),
            ("private/ignored.md", b"private\n".as_slice()),
        ] {
            std::fs::write(graph.join(relative), bytes).unwrap();
        }

        let scope = GraphTextScope::new(&["private".into()], false);
        let result = copy_graph_text_tree_cancellable(&graph, &snapshot, &scope, &|| false);

        assert_eq!(result, (3, 0));
        assert_eq!(std::fs::read(snapshot.join("Root.md")).unwrap(), b"root\n");
        assert_eq!(
            std::fs::read(snapshot.join("archive/自由/Elsewhere.Markdown")).unwrap(),
            b"elsewhere\n"
        );
        for excluded in [
            "assets/ignored.md",
            "logseq/.tine-trash/pages/ignored.md",
            ".hidden/ignored.md",
            "private/ignored.md",
        ] {
            assert!(!snapshot.join(excluded).exists(), "{excluded}");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn graph_wide_restore_is_exact_for_eligible_text_and_leaves_exclusions_untouched() {
        let root = scratch("graph-wide-restore");
        let graph = root.join("graph");
        let snapshot = root.join("snapshot/graph");
        for directory in [
            "pages",
            "archive/自由",
            "assets",
            "logseq/.tine-trash/pages",
            ".hidden",
            "private",
        ] {
            std::fs::create_dir_all(graph.join(directory)).unwrap();
        }
        std::fs::create_dir_all(snapshot.join("pages")).unwrap();
        std::fs::create_dir_all(snapshot.join("archive/自由")).unwrap();
        std::fs::write(snapshot.join("Root.md"), b"snapshot root\n").unwrap();
        std::fs::write(snapshot.join("pages/Normal.org"), b"* snapshot\n").unwrap();
        std::fs::write(
            snapshot.join("archive/自由/Elsewhere.markdown"),
            b"snapshot elsewhere\n",
        )
        .unwrap();
        std::fs::write(graph.join("Root.md"), b"live root\n").unwrap();
        std::fs::write(graph.join("archive/Stale.md"), b"stale\n").unwrap();
        for (relative, bytes) in [
            ("assets/untouched.md", b"asset\n".as_slice()),
            (
                "logseq/.tine-trash/pages/untouched.md",
                b"trash\n".as_slice(),
            ),
            (".hidden/untouched.md", b"hidden\n".as_slice()),
            ("private/untouched.md", b"private\n".as_slice()),
        ] {
            std::fs::write(graph.join(relative), bytes).unwrap();
        }
        let recovery = reserve_restore_recovery(
            &graph,
            Path::new("logseq/.tine-trash"),
            "restore-graph-wide",
        )
        .unwrap();
        let scope = GraphTextScope::new(&["private".into()], false);

        restore_graph_text_tree(&snapshot, &graph, &scope, &recovery).unwrap();

        assert_eq!(
            std::fs::read(graph.join("Root.md")).unwrap(),
            b"snapshot root\n"
        );
        assert_eq!(
            std::fs::read(graph.join("archive/自由/Elsewhere.markdown")).unwrap(),
            b"snapshot elsewhere\n"
        );
        assert!(!graph.join("archive/Stale.md").exists());
        assert_eq!(
            std::fs::read(recovery.path.join("graph/archive/Stale.md")).unwrap(),
            b"stale\n"
        );
        for (relative, bytes) in [
            ("assets/untouched.md", b"asset\n".as_slice()),
            (
                "logseq/.tine-trash/pages/untouched.md",
                b"trash\n".as_slice(),
            ),
            (".hidden/untouched.md", b"hidden\n".as_slice()),
            ("private/untouched.md", b"private\n".as_slice()),
        ] {
            assert_eq!(
                std::fs::read(graph.join(relative)).unwrap(),
                bytes,
                "{relative}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn managed_restore_collection_keeps_graph_relative_paths() {
        let root = scratch("managed-restore-graph-wide");
        let graph = root.join("graph");
        std::fs::create_dir_all(graph.join("archive/自由")).unwrap();
        std::fs::write(graph.join("Root.md"), "- root\n").unwrap();
        std::fs::write(graph.join("archive/自由/Page.markdown"), "- nested\n").unwrap();
        let scope = GraphTextScope::new(&[], false);

        let files = collect_scoped_restore_graph_text(&graph, &scope).unwrap();

        assert_eq!(
            files,
            vec![
                ("Root.md".into(), "- root\n".into()),
                ("archive/自由/Page.markdown".into(), "- nested\n".into()),
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn schema_v3_restore_dispatches_graph_wide_payload() {
        let root = scratch("schema-v3-restore");
        let graph = root.join("graph");
        let base = root.join("backups");
        let snapshot = base.join("2026-08-11_12-00-00");
        for directory in ["logseq", "assets", "archive"] {
            std::fs::create_dir_all(graph.join(directory)).unwrap();
        }
        std::fs::create_dir_all(snapshot.join("graph/archive/自由")).unwrap();
        std::fs::write(snapshot.join("graph/Root.md"), b"snapshot root\n").unwrap();
        std::fs::write(
            snapshot.join("graph/archive/自由/Page.markdown"),
            b"snapshot nested\n",
        )
        .unwrap();
        std::fs::write(graph.join("Root.md"), b"live root\n").unwrap();
        std::fs::write(graph.join("archive/Stale.org"), b"* stale\n").unwrap();
        let files = snapshot_inventory(&snapshot).unwrap();
        write_manifest(
            &snapshot,
            &SnapshotManifest {
                schema: SNAPSHOT_SCHEMA,
                root: std::fs::canonicalize(&graph).unwrap().display().to_string(),
                // Schema 3 paths are carried by graph/, not these schema-2
                // compatibility fields.
                journals_dir: "../unused-legacy-root".into(),
                pages_dir: "/unused-legacy-root".into(),
                graph_text_policy: Some(SnapshotGraphTextPolicy {
                    version: GRAPH_TEXT_SCOPE_VERSION,
                    hidden: Vec::new(),
                    hidden_parse_failed_closed: false,
                }),
                files,
                complete: true,
            },
        )
        .unwrap();
        let source = BackupSource {
            assets: graph.join("assets"),
            cfg: graph.join("logseq/config.edn"),
            root: graph.clone(),
            journals_dir: "journals".into(),
            pages_dir: "pages".into(),
            graph_text_scope: GraphTextScope::new(&[], false),
            graph_text_policy: SnapshotGraphTextPolicy {
                version: GRAPH_TEXT_SCOPE_VERSION,
                hidden: Vec::new(),
                hidden_parse_failed_closed: false,
            },
        };

        restore_from_backup_source("2026-08-11_12-00-00", &base, source, |_| (1, true)).unwrap();

        assert_eq!(
            std::fs::read(graph.join("Root.md")).unwrap(),
            b"snapshot root\n"
        );
        assert_eq!(
            std::fs::read(graph.join("archive/自由/Page.markdown")).unwrap(),
            b"snapshot nested\n"
        );
        assert!(!graph.join("archive/Stale.org").exists());
        let recovered = graph.join("logseq/.tine-trash");
        assert!(walk_contains(
            &recovered,
            Path::new("graph/archive/Stale.org")
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    fn walk_contains(root: &Path, suffix: &Path) -> bool {
        let mut stack = vec![root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.path().ends_with(suffix) {
                    return true;
                }
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    stack.push(entry.path());
                }
            }
        }
        false
    }

    #[test]
    fn restore_asset_sidecars_dir_restores_sidecars_and_leaves_binary_assets() {
        let root = scratch("restore-sidecars");
        let src = root.join("backup").join("assets");
        let dest = root.join("graph").join("assets");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::create_dir_all(dest.join("nested")).unwrap();
        std::fs::write(src.join("doc.edn"), "new\n").unwrap();
        std::fs::write(src.join("nested").join("hl.edn"), "nested new\n").unwrap();
        std::fs::write(dest.join("doc.edn"), "old\n").unwrap();
        std::fs::write(dest.join("stale.edn"), "stale\n").unwrap();
        std::fs::write(dest.join("image.png"), b"keep").unwrap();
        std::fs::write(dest.join("nested").join("stale.edn"), "stale\n").unwrap();
        std::fs::write(dest.join("nested").join("image.png"), b"keep").unwrap();

        let recovery = reserve_restore_recovery(
            &dest,
            std::path::Path::new(ASSET_RESTORE_RECOVERY_DIR),
            "restore-sidecars",
        )
        .unwrap();
        restore_asset_sidecars_dir(&src, &dest, &recovery, std::path::Path::new("")).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("doc.edn")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("nested").join("hl.edn")).unwrap(),
            "nested new\n"
        );
        assert!(!dest.join("stale.edn").exists());
        assert!(!dest.join("nested").join("stale.edn").exists());
        assert_eq!(
            std::fs::read_to_string(recovery.path.join("stale.edn")).unwrap(),
            "stale\n"
        );
        assert_eq!(std::fs::read(dest.join("image.png")).unwrap(), b"keep");
        assert_eq!(
            std::fs::read(dest.join("nested").join("image.png")).unwrap(),
            b"keep"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn graph_text_backup_and_restore_include_nested_pages() {
        let root = scratch("nested-md");
        let graph = root.join("graph");
        let pages = graph.join("pages");
        let journals = graph.join("journals");
        let backup = root.join("backup");
        std::fs::create_dir_all(pages.join("client-a")).unwrap();
        std::fs::create_dir_all(&journals).unwrap();
        std::fs::write(pages.join("Top.md"), b"top\n").unwrap();
        std::fs::write(pages.join("client-a").join("Deep.md"), b"deep\n").unwrap();
        std::fs::write(journals.join("2026_07_09.md"), b"journal\n").unwrap();

        let (copied_pages, failed_pages) = copy_md_dir(&pages, &backup.join("pages"));
        let (copied_journals, failed_journals) = copy_md_dir(&journals, &backup.join("journals"));
        let copied = copied_pages + copied_journals;
        let failed = failed_pages + failed_journals;
        let complete = failed == 0 && copied == 3;

        assert!(complete);
        assert_eq!(
            std::fs::read(backup.join("pages").join("Top.md")).unwrap(),
            b"top\n"
        );
        assert_eq!(
            std::fs::read(backup.join("pages").join("client-a").join("Deep.md")).unwrap(),
            b"deep\n"
        );
        assert_eq!(
            std::fs::read(backup.join("journals").join("2026_07_09.md")).unwrap(),
            b"journal\n"
        );

        std::fs::write(pages.join("client-a").join("Deep.md"), b"corrupt\n").unwrap();
        std::fs::write(pages.join("client-a").join("Stale.md"), b"stale\n").unwrap();
        std::fs::write(pages.join("client-a").join("notes.txt"), b"keep\n").unwrap();
        let recovery = reserve_restore_recovery(
            &graph,
            std::path::Path::new("logseq/.tine-trash"),
            "restore-pages",
        )
        .unwrap();
        restore_md_dir(
            &backup.join("pages"),
            &pages,
            &recovery,
            std::path::Path::new("pages"),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(pages.join("client-a").join("Deep.md")).unwrap(),
            b"deep\n"
        );
        assert!(!pages.join("client-a").join("Stale.md").exists());
        assert_eq!(
            std::fs::read_to_string(recovery.path.join("pages/client-a/Stale.md")).unwrap(),
            "stale\n"
        );
        assert_eq!(
            std::fs::read(pages.join("client-a").join("notes.txt")).unwrap(),
            b"keep\n"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn complete_restore_crosses_from_app_data_to_a_distinct_live_filesystem() {
        use std::os::unix::fs::MetadataExt;

        // GH #130's actual fault boundary: Android's app-data snapshot and the
        // user-selected graph can have distinct st_dev values.  Use /dev/shm as
        // the live device when the host exposes it; skip only on hosts where it
        // is unavailable or aliases the temp filesystem.
        let app_data = scratch("restore-cross-device-source");
        let live_root = PathBuf::from("/dev/shm").join(format!(
            "tine-restore-cross-device-live-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&live_root);
        if std::fs::create_dir_all(&live_root).is_err()
            || std::fs::metadata(&app_data).unwrap().dev()
                == std::fs::metadata(&live_root).unwrap().dev()
        {
            let _ = std::fs::remove_dir_all(&app_data);
            let _ = std::fs::remove_dir_all(&live_root);
            return;
        }

        let snapshot = app_data.join("snapshot");
        for dir in ["pages", "journals", "assets", "logseq"] {
            std::fs::create_dir_all(snapshot.join(dir)).unwrap();
            std::fs::create_dir_all(live_root.join(dir)).unwrap();
        }
        std::fs::write(snapshot.join("pages/Kept.md"), b"snapshot page\n").unwrap();
        std::fs::write(
            snapshot.join("journals/2026_07_15.md"),
            b"snapshot journal\n",
        )
        .unwrap();
        std::fs::write(snapshot.join("assets/doc.edn"), b"{:snapshot true}\n").unwrap();
        std::fs::write(snapshot.join("logseq/config.edn"), b"{:snapshot true}\n").unwrap();
        std::fs::write(live_root.join("pages/Kept.md"), b"live page\n").unwrap();
        std::fs::write(live_root.join("pages/Stale.md"), b"stale page\n").unwrap();
        std::fs::write(live_root.join("journals/Old.md"), b"old journal\n").unwrap();
        std::fs::write(live_root.join("assets/doc.edn"), b"{:live true}\n").unwrap();
        std::fs::write(live_root.join("assets/stale.edn"), b"{:stale true}\n").unwrap();
        std::fs::write(live_root.join("assets/binary.pdf"), b"keep binary").unwrap();
        std::fs::write(live_root.join("logseq/config.edn"), b"{:live true}\n").unwrap();

        let graph_recovery = reserve_restore_recovery(
            &live_root,
            std::path::Path::new("logseq/.tine-trash"),
            "restore-cross-device",
        )
        .unwrap();
        let asset_recovery = reserve_restore_recovery(
            &live_root.join("assets"),
            std::path::Path::new(ASSET_RESTORE_RECOVERY_DIR),
            "restore-cross-device",
        )
        .unwrap();

        restore_md_dir(
            &snapshot.join("pages"),
            &live_root.join("pages"),
            &graph_recovery,
            std::path::Path::new("pages"),
        )
        .unwrap();
        restore_md_dir(
            &snapshot.join("journals"),
            &live_root.join("journals"),
            &graph_recovery,
            std::path::Path::new("journals"),
        )
        .unwrap();
        restore_asset_sidecars_dir(
            &snapshot.join("assets"),
            &live_root.join("assets"),
            &asset_recovery,
            std::path::Path::new(""),
        )
        .unwrap();
        let live_config = live_root.join("logseq/config.edn");
        move_live_to_recovery(
            &graph_recovery,
            &live_config,
            std::path::Path::new("logseq/config.edn"),
        )
        .unwrap();
        atomic_copy_new_into_live(
            &graph_recovery,
            &snapshot.join("logseq/config.edn"),
            &live_config,
        )
        .unwrap();

        assert_eq!(
            std::fs::read(live_root.join("pages/Kept.md")).unwrap(),
            b"snapshot page\n"
        );
        assert!(!live_root.join("pages/Stale.md").exists());
        assert_eq!(
            std::fs::read(live_root.join("journals/2026_07_15.md")).unwrap(),
            b"snapshot journal\n"
        );
        assert!(!live_root.join("journals/Old.md").exists());
        assert_eq!(
            std::fs::read(live_root.join("assets/doc.edn")).unwrap(),
            b"{:snapshot true}\n"
        );
        assert!(!live_root.join("assets/stale.edn").exists());
        assert_eq!(
            std::fs::read(live_root.join("assets/binary.pdf")).unwrap(),
            b"keep binary"
        );
        assert_eq!(std::fs::read(&live_config).unwrap(), b"{:snapshot true}\n");
        assert_eq!(
            std::fs::read(graph_recovery.path.join("pages/Stale.md")).unwrap(),
            b"stale page\n"
        );
        assert_eq!(
            std::fs::read(graph_recovery.path.join("logseq/config.edn")).unwrap(),
            b"{:live true}\n"
        );
        assert_eq!(
            std::fs::read(asset_recovery.path.join("stale.edn")).unwrap(),
            b"{:stale true}\n"
        );

        let _ = std::fs::remove_dir_all(&app_data);
        let _ = std::fs::remove_dir_all(&live_root);
    }
}
