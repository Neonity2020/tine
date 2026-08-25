use fs2::FileExt as _;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::State;

use crate::state::{AppState, ApplicationPageAdmissionAuthority};

// `diag` is the existing opt-in detailed trace (`TINE_DEBUG=1` / `--debug`).
// It may contain a path or an OS error chosen for a directed investigation, so
// it is NEVER copied into the automatic report. The flight recorder is always
// on and accepts only fixed enums, catalogued command names, and numeric/boolean
// measurements. This separation is the privacy boundary behind GH #343.
const FLIGHT_SCHEMA_VERSION: u8 = 1;
const FLIGHT_SEGMENT_MAX_BYTES: u64 = 1024 * 1024;
const EARLY_EVENT_CAP: usize = 64;

static DEBUG_LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();
static DEBUG_START: OnceLock<std::time::Instant> = OnceLock::new();
static FLIGHT: OnceLock<Option<Mutex<FlightRecorder>>> = OnceLock::new();
static EARLY_EVENTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static PREVIOUS_EXIT_UNCLEAN: AtomicBool = AtomicBool::new(false);

fn early_events() -> &'static Mutex<Vec<String>> {
    EARLY_EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}

fn elapsed_ms() -> u64 {
    let elapsed = DEBUG_START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis();
    u64::try_from(elapsed).unwrap_or(u64::MAX)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

pub(crate) fn debug_enabled() -> bool {
    matches!(std::env::var("TINE_DEBUG"), Ok(v) if !v.is_empty() && v != "0")
        || std::env::args().any(|a| a == "--debug")
}

fn debug_log_path() -> PathBuf {
    std::env::var_os("TINE_DEBUG_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("tine-debug.log"))
}

/// Initialize the directed, detailed trace before Tauri exists. It remains
/// opt-in and retains its historical path/format contract.
pub(crate) fn debug_init() {
    DEBUG_START.get_or_init(std::time::Instant::now);
    DEBUG_LOG.get_or_init(|| {
        if !debug_enabled() {
            return None;
        }
        let path = debug_log_path();
        match private_write_options(true, false).open(&path) {
            Ok(f) => {
                eprintln!("[tine] DEBUG logging to {}", path.display());
                Some(Mutex::new(f))
            }
            Err(e) => {
                eprintln!("[tine] could not open debug log {}: {e}", path.display());
                None
            }
        }
    });
}

/// Emit one detailed line to stderr and, when explicitly enabled, the directed
/// debug file. This function does not write to the privacy-safe flight recorder.
pub(crate) fn diag(msg: impl AsRef<str>) {
    let msg = msg.as_ref();
    eprintln!("[tine] {msg}");
    if let Some(Some(lock)) = DEBUG_LOG.get() {
        if let Ok(mut file) = lock.lock() {
            let _ = writeln!(file, "[+{:>7}ms] {msg}", elapsed_ms());
            let _ = file.flush();
        }
    }
}

pub(crate) fn debug_header() {
    if !debug_enabled() {
        return;
    }
    diag(format!(
        "Tine {} starting — {}/{}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    let env_of = |key: &str| std::env::var(key).unwrap_or_else(|_| "<unset>".into());
    for key in [
        "TINE_GRAPH",
        "TINE_GPU",
        "WEBVIEW2_USER_DATA_FOLDER",
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "WEBKIT_DISABLE_DMABUF_RENDERER",
        "WEBKIT_DISABLE_COMPOSITING_MODE",
        "XDG_SESSION_TYPE",
        "WAYLAND_DISPLAY",
        "APPIMAGE",
        "LD_PRELOAD",
        "GDK_BACKEND",
    ] {
        diag(format!("env {key}={}", env_of(key)));
    }
}

pub(crate) fn install_panic_logger() {
    if debug_enabled() && std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record_fixed_event("runtime.panic", Map::new());
        if debug_enabled() {
            diag(format!("PANIC: {info}"));
            diag(format!(
                "backtrace:\n{}",
                std::backtrace::Backtrace::force_capture()
            ));
        }
        default(info);
    }));
}

// ---------------------------------------------------------------------------
// Always-on privacy-safe flight recorder
// ---------------------------------------------------------------------------

fn private_write_options(truncate: bool, append: bool) -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .create(true)
        .write(true)
        .truncate(truncate)
        .append(append);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    options
}

fn private_read(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000);
    }
    options.open(path)
}

fn prepare_private_dir(path: &Path) -> std::io::Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "diagnostics location is not a private directory",
            ));
        }
    } else {
        std::fs::create_dir_all(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

struct FlightRecorder {
    dir: PathBuf,
    current: Option<File>,
    current_bytes: u64,
    segment_max_bytes: u64,
    _process_lock: File,
}

impl FlightRecorder {
    fn open(dir: PathBuf, segment_max_bytes: u64) -> std::io::Result<(Self, bool)> {
        prepare_private_dir(&dir)?;
        let process_lock = private_write_options(false, false).open(dir.join("process.lock"))?;
        process_lock.try_lock_exclusive()?;

        let marker = dir.join("session-active");
        let previous_unclean = marker.exists();
        let _marker_file = private_write_options(true, false).open(&marker)?;

        let previous_old = dir.join("previous-old.jsonl");
        let previous = dir.join("previous.jsonl");
        let current_old = dir.join("current-old.jsonl");
        let current = dir.join("current.jsonl");
        let _ = std::fs::remove_file(&previous_old);
        if current_old.exists() {
            std::fs::rename(&current_old, &previous_old)?;
        }
        let _ = std::fs::remove_file(&previous);
        if current.exists() {
            std::fs::rename(&current, &previous)?;
        }
        let file = private_write_options(true, false).open(&current)?;
        Ok((
            Self {
                dir,
                current: Some(file),
                current_bytes: 0,
                segment_max_bytes,
                _process_lock: process_lock,
            },
            previous_unclean,
        ))
    }

    fn append(&mut self, line: &str) -> std::io::Result<()> {
        let line_bytes = u64::try_from(line.len().saturating_add(1)).unwrap_or(u64::MAX);
        if self.current_bytes > 0
            && self.current_bytes.saturating_add(line_bytes) > self.segment_max_bytes
        {
            if let Some(mut file) = self.current.take() {
                file.flush()?;
            }
            let old = self.dir.join("current-old.jsonl");
            let current = self.dir.join("current.jsonl");
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&current, &old)?;
            self.current = Some(private_write_options(true, false).open(current)?);
            self.current_bytes = 0;
        }
        let Some(file) = self.current.as_mut() else {
            return Err(std::io::Error::other("flight recorder file is unavailable"));
        };
        writeln!(file, "{line}")?;
        file.flush()?;
        self.current_bytes = self.current_bytes.saturating_add(line_bytes);
        Ok(())
    }

    fn clear(&mut self) -> std::io::Result<()> {
        if let Some(mut file) = self.current.take() {
            file.flush()?;
        }
        for name in [
            "current.jsonl",
            "current-old.jsonl",
            "previous.jsonl",
            "previous-old.jsonl",
        ] {
            let _ = std::fs::remove_file(self.dir.join(name));
        }
        self.current =
            Some(private_write_options(true, false).open(self.dir.join("current.jsonl"))?);
        self.current_bytes = 0;
        Ok(())
    }

    fn mark_clean_shutdown(&mut self) {
        if let Some(file) = self.current.as_mut() {
            let _ = file.flush();
        }
        let _ = std::fs::remove_file(self.dir.join("session-active"));
    }
}

pub(crate) fn flight_init(dir: PathBuf) {
    let recorder = FLIGHT.get_or_init(|| {
        match FlightRecorder::open(dir, FLIGHT_SEGMENT_MAX_BYTES) {
            Ok((mut recorder, previous_unclean)) => {
                PREVIOUS_EXIT_UNCLEAN.store(previous_unclean, Ordering::Release);
                if let Ok(mut early) = early_events().lock() {
                    for line in early.drain(..) {
                        let _ = recorder.append(&line);
                    }
                }
                Some(Mutex::new(recorder))
            }
            Err(error) => {
                // A forwarded second launch legitimately loses the process
                // lock and must not rotate the primary process's evidence.
                eprintln!("[tine] flight recorder unavailable: {error}");
                None
            }
        }
    });
    if recorder.is_some() {
        record_fixed_event("runtime.started", Map::new());
        if PREVIOUS_EXIT_UNCLEAN.load(Ordering::Acquire) {
            record_fixed_event("runtime.previous_exit_unclean", Map::new());
        }
    }
}

pub(crate) fn mark_clean_shutdown() {
    record_fixed_event("runtime.clean_shutdown", Map::new());
    if let Some(Some(recorder)) = FLIGHT.get() {
        if let Ok(mut recorder) = recorder.lock() {
            recorder.mark_clean_shutdown();
        }
    }
}

fn record_fixed_event(event: &'static str, fields: Map<String, Value>) {
    let mut line = Map::new();
    line.insert("schemaVersion".into(), json!(FLIGHT_SCHEMA_VERSION));
    line.insert("elapsedMs".into(), json!(elapsed_ms()));
    line.insert("event".into(), json!(event));
    for (key, value) in fields {
        line.insert(key, value);
    }
    let Ok(encoded) = serde_json::to_string(&line) else {
        return;
    };
    match FLIGHT.get() {
        Some(Some(recorder)) => {
            if let Ok(mut recorder) = recorder.lock() {
                let _ = recorder.append(&encoded);
            }
        }
        Some(None) => {}
        None => {
            if let Ok(mut early) = early_events().lock() {
                if early.len() >= EARLY_EVENT_CAP {
                    early.remove(0);
                }
                early.push(encoded);
            }
        }
    }
}

fn enum_token<T: Serialize>(value: T) -> Value {
    match serde_json::to_value(value) {
        Ok(Value::String(token)) => Value::String(token),
        _ => Value::String("unknown".into()),
    }
}

pub(crate) fn record_storage_transition(
    event: &crate::storage_mode_supervisor::StorageTransitionEvent,
) {
    let mut fields = Map::new();
    fields.insert("operationId".into(), json!(event.operation_id));
    fields.insert(
        "windowKind".into(),
        json!(if event.window == "main" {
            "main"
        } else if event.window.starts_with("graph-") {
            "graph"
        } else {
            "other"
        }),
    );
    fields.insert("kind".into(), enum_token(event.kind));
    fields.insert("phase".into(), enum_token(event.phase));
    fields.insert("elapsedMs".into(), json!(event.elapsed_ms));
    fields.insert("terminal".into(), json!(event.terminal));
    if let Some(outcome) = event.outcome {
        fields.insert("outcome".into(), enum_token(outcome));
    }
    record_fixed_event("storage.transition", fields);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_direct_save(
    outcome: &'static str,
    total_ms: u64,
    complete_builds: u64,
    exact_updates: u64,
    invalidated: bool,
    captured_entries: Option<u64>,
    captured_bytes: Option<u64>,
) {
    let mut fields = Map::new();
    fields.insert("outcome".into(), json!(outcome));
    fields.insert("totalMs".into(), json!(total_ms));
    fields.insert("guardedIndexBuilds".into(), json!(complete_builds));
    fields.insert("guardedIndexExactUpdates".into(), json!(exact_updates));
    fields.insert("guardedIndexInvalidated".into(), json!(invalidated));
    if let Some(entries) = captured_entries {
        fields.insert("lastBuildEntries".into(), json!(entries));
    }
    if let Some(bytes) = captured_bytes {
        fields.insert("lastBuildBytes".into(), json!(bytes));
    }
    record_fixed_event("direct.save", fields);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_watcher_latency(
    mode: &'static str,
    pages: u64,
    event_paths: u64,
    full_diff: bool,
    errors: u64,
    event_to_reconcile_ms: Option<u64>,
    reconcile_ms: u64,
    event_to_emit_ms: Option<u64>,
) {
    let mut fields = Map::new();
    fields.insert("mode".into(), json!(mode));
    fields.insert("pages".into(), json!(pages));
    fields.insert("eventPaths".into(), json!(event_paths));
    fields.insert("fullDiff".into(), json!(full_diff));
    fields.insert("errors".into(), json!(errors));
    fields.insert("eventToReconcileMs".into(), json!(event_to_reconcile_ms));
    fields.insert("reconcileMs".into(), json!(reconcile_ms));
    fields.insert("eventToEmitMs".into(), json!(event_to_emit_ms));
    record_fixed_event("watcher.batch", fields);
}

#[tauri::command]
pub(crate) fn diagnostic_ipc_event(command: String, phase: String, elapsed_ms: u64) {
    if !crate::managed_command_surface::is_known_command(&command)
        || matches!(
            command.as_str(),
            "diagnostic_ipc_event"
                | "diagnostic_frontend_event"
                | "diagnostic_report"
                | "save_diagnostic_report"
                | "clear_diagnostics"
                | "debug_info"
                | "debug_log"
        )
        || !matches!(phase.as_str(), "slow" | "completed" | "failed")
    {
        return;
    }
    let mut fields = Map::new();
    fields.insert("command".into(), json!(command));
    fields.insert("phase".into(), json!(phase));
    fields.insert("elapsedMs".into(), json!(elapsed_ms));
    record_fixed_event("ipc.command", fields);
}

#[tauri::command]
pub(crate) fn diagnostic_frontend_event(
    kind: String,
    line: Option<u64>,
    column: Option<u64>,
    delay_ms: Option<u64>,
) {
    if !matches!(
        kind.as_str(),
        "uncaught_error" | "unhandled_rejection" | "heartbeat_delay"
    ) {
        return;
    }
    let mut fields = Map::new();
    fields.insert("kind".into(), json!(kind));
    fields.insert("line".into(), json!(line));
    fields.insert("column".into(), json!(column));
    fields.insert("delayMs".into(), json!(delay_ms));
    record_fixed_event("frontend.health", fields);
}

#[tauri::command]
pub(crate) fn debug_log(line: String) {
    if debug_enabled() {
        diag(format!("[ui] {line}"));
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DebugInfo {
    enabled: bool,
    path: String,
    recorder_active: bool,
    previous_exit_unclean: bool,
}

#[tauri::command]
pub(crate) fn debug_info() -> DebugInfo {
    DebugInfo {
        enabled: debug_enabled(),
        path: debug_log_path().display().to_string(),
        recorder_active: matches!(FLIGHT.get(), Some(Some(_))),
        previous_exit_unclean: PREVIOUS_EXIT_UNCLEAN.load(Ordering::Acquire),
    }
}

fn read_event_segment(path: &Path) -> Vec<Value> {
    let Ok(file) = private_read(path) else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    if file
        .take(FLIGHT_SEGMENT_MAX_BYTES.saturating_add(4096))
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Vec::new();
    }
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

fn safe_build_commit(value: String) -> Option<String> {
    (value.len() >= 7 && value.len() <= 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(value)
}

fn safe_build_time(value: String) -> Option<String> {
    (value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_digit() || matches!(byte, b'-' | b':' | b'.' | b'T' | b'Z' | b'+')
        }))
    .then_some(value)
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiagnosticReport {
    pub(crate) text: String,
    pub(crate) suggested_file_name: String,
}

fn build_diagnostic_report(
    state: &AppState,
    build_commit: String,
    build_time: String,
) -> DiagnosticReport {
    let mut direct = 0u64;
    let mut managed_writable = 0u64;
    let mut managed_unavailable = 0u64;
    let mut graph_state_unavailable = false;
    match state.graphs.read() {
        Ok(graphs) => {
            for (_, slot) in graphs.entries() {
                match slot.application_page_admission().authority {
                    ApplicationPageAdmissionAuthority::Direct => direct += 1,
                    ApplicationPageAdmissionAuthority::ManagedWritable { .. } => {
                        managed_writable += 1
                    }
                    ApplicationPageAdmissionAuthority::ManagedUnavailable => {
                        managed_unavailable += 1
                    }
                }
            }
        }
        Err(_) => graph_state_unavailable = true,
    }

    let mut sessions = BTreeMap::new();
    if let Some(Some(recorder)) = FLIGHT.get() {
        if let Ok(mut recorder) = recorder.lock() {
            if let Some(file) = recorder.current.as_mut() {
                let _ = file.flush();
            }
            for (name, file) in [
                ("previousOld", "previous-old.jsonl"),
                ("previous", "previous.jsonl"),
                ("currentOld", "current-old.jsonl"),
                ("current", "current.jsonl"),
            ] {
                sessions.insert(name, read_event_segment(&recorder.dir.join(file)));
            }
        }
    }

    let generated_at = unix_ms();
    let report = json!({
        "schemaVersion": FLIGHT_SCHEMA_VERSION,
        "generatedAtUnixMs": generated_at,
        "app": {
            "version": env!("CARGO_PKG_VERSION"),
            "buildCommit": safe_build_commit(build_commit),
            "buildTime": safe_build_time(build_time),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        },
        "privacy": {
            "automaticUpload": false,
            "containsGraphContent": false,
            "containsPaths": false,
            "containsPageTitles": false,
            "containsQueriesOrUrls": false,
            "containsCredentials": false,
            "verboseDebugLogIncluded": false,
        },
        "runtime": {
            "recorderActive": matches!(FLIGHT.get(), Some(Some(_))),
            "previousExitUnclean": PREVIOUS_EXIT_UNCLEAN.load(Ordering::Acquire),
            "verboseDebugEnabled": debug_enabled(),
            "graphStateUnavailable": graph_state_unavailable,
            "graphBindings": direct + managed_writable + managed_unavailable,
            "directBindings": direct,
            "managedWritableBindings": managed_writable,
            "managedUnavailableBindings": managed_unavailable,
        },
        "activeStorageTransitions": state.storage_supervisor.diagnostic_snapshot(),
        "watcherLatency": crate::watcher::diagnostic_latency_snapshot(),
        "sessions": sessions,
    });
    DiagnosticReport {
        text: serde_json::to_string_pretty(&report).unwrap_or_else(|_| {
            "{\"schemaVersion\":1,\"error\":\"report_serialization_failed\"}".into()
        }),
        suggested_file_name: format!("tine-diagnostics-{generated_at}.json"),
    }
}

#[tauri::command]
pub(crate) fn diagnostic_report(
    state: State<'_, AppState>,
    build_commit: String,
    build_time: String,
) -> DiagnosticReport {
    build_diagnostic_report(&state, build_commit, build_time)
}

#[tauri::command]
pub(crate) async fn save_diagnostic_report(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    build_commit: String,
    build_time: String,
) -> Result<bool, String> {
    let report = build_diagnostic_report(&state, build_commit, build_time);
    #[cfg(desktop)]
    {
        use tauri_plugin_dialog::DialogExt as _;
        let suggested = report.suggested_file_name.clone();
        let chosen = tauri::async_runtime::spawn_blocking(move || {
            app.dialog()
                .file()
                .set_file_name(suggested)
                .add_filter("JSON", &["json"])
                .blocking_save_file()
        })
        .await
        .map_err(|error| format!("diagnostic save dialog failed: {error}"))?;
        let Some(chosen) = chosen else {
            return Ok(false);
        };
        let path = chosen
            .into_path()
            .map_err(|_| "diagnostic save destination is not a local file".to_string())?;
        std::fs::write(path, report.text)
            .map_err(|error| format!("diagnostic report could not be saved: {error}"))?;
        Ok(true)
    }
    #[cfg(not(desktop))]
    {
        let _ = (app, report);
        Err("Save report is available on desktop; use Copy report on this device.".into())
    }
}

#[tauri::command]
pub(crate) fn clear_diagnostics() -> Result<(), String> {
    let Some(Some(recorder)) = FLIGHT.get() else {
        return Ok(());
    };
    recorder
        .lock()
        .map_err(|_| "diagnostic recorder is unavailable".to_string())?
        .clear()
        .map_err(|error| format!("diagnostic events could not be cleared: {error}"))?;
    record_fixed_event("diagnostics.cleared", Map::new());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorder_retains_previous_unclean_run_and_bounds_each_segment() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("diagnostics");
        {
            let (mut recorder, previous_unclean) = FlightRecorder::open(dir.clone(), 120).unwrap();
            assert!(!previous_unclean);
            recorder.append(&"a".repeat(80)).unwrap();
            recorder.append(&"b".repeat(80)).unwrap();
            assert!(dir.join("current-old.jsonl").is_file());
            // Deliberately omit mark_clean_shutdown: simulate a killed process.
        }
        let (mut recorder, previous_unclean) = FlightRecorder::open(dir.clone(), 120).unwrap();
        assert!(previous_unclean);
        assert!(dir.join("previous.jsonl").is_file());
        assert!(dir.join("previous-old.jsonl").is_file());
        recorder.mark_clean_shutdown();
        assert!(!dir.join("session-active").exists());
    }

    #[test]
    fn build_metadata_rejects_strings_that_could_smuggle_report_content() {
        assert_eq!(
            safe_build_commit("abcdef1".into()).as_deref(),
            Some("abcdef1")
        );
        assert_eq!(safe_build_commit("page title".into()), None);
        assert_eq!(
            safe_build_time("2026-08-25T10:00:00.000Z".into()).as_deref(),
            Some("2026-08-25T10:00:00.000Z")
        );
        assert_eq!(safe_build_time("/home/person/graph".into()), None);
    }

    #[test]
    fn fixed_event_shape_contains_no_free_form_message_fields() {
        let source = include_str!("debug.rs");
        assert!(!source.contains("fields.insert(\"message\""));
        assert!(!source.contains("fields.insert(\"path\""));
        assert!(!source.contains("fields.insert(\"detail\""));
        assert!(source.contains("verboseDebugLogIncluded\": false"));
    }
}
