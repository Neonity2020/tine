//! Typed rejection payloads for the command boundary, and the process-wide
//! debug-diagnostics flag.
//!
//! Both lived in the retired sync runtime (`sync_runtime.rs`, removed
//! 2026-09-15) although neither was about sync. They are the two things every
//! `tine-core` producer and every `src-tauri` command share regardless of
//! storage mode.

/// The sole native encoder for app-internal typed rejection payloads. The Tauri
/// wire remains `String`, but its load-bearing kinds are JSON data rather than
/// prose that frontend components must parse back into control flow.
pub fn tagged_backend_error(kind: &'static str, reason_code: Option<&str>) -> String {
    encode_tagged_backend_error(kind, reason_code, None)
}

/// The same encoder for a kind whose contract promises bounded typed data
/// alongside the code. Never note content.
pub fn tagged_backend_error_with_detail(kind: &'static str, detail: serde_json::Value) -> String {
    encode_tagged_backend_error(kind, None, Some(detail))
}

/// The same encoder for a closed reason vocabulary plus bounded typed detail.
/// Direct save failures use this to carry the producer code and filesystem
/// error kind without reparsing display text at the command boundary.
pub fn tagged_backend_error_with_reason_and_detail(
    kind: &'static str,
    reason_code: &str,
    detail: serde_json::Value,
) -> String {
    encode_tagged_backend_error(kind, Some(reason_code), Some(detail))
}

fn encode_tagged_backend_error(
    kind: &'static str,
    reason_code: Option<&str>,
    detail: Option<serde_json::Value>,
) -> String {
    let mut payload = serde_json::Map::new();
    payload.insert("kind".into(), serde_json::Value::String(kind.into()));
    if let Some(reason_code) = reason_code {
        payload.insert(
            "reason_code".into(),
            serde_json::Value::String(reason_code.into()),
        );
    }
    if let Some(detail) = detail {
        payload.insert("detail".into(), detail);
    }
    serde_json::Value::Object(payload).to_string()
}

/// Process-wide "are runtime debug diagnostics on?" flag.
///
/// Core cannot call src-tauri, so the flag lives here and the host process
/// pushes its answer down: `debug_init` parses `TINE_DEBUG=1` / `--debug` once
/// at startup and calls [`set_runtime_debug_diagnostics`]. Default `false`, so
/// tests, benches, CLI tools and any other non-Tauri consumer get diagnostics
/// off unless they opt in deliberately.
static RUNTIME_DEBUG_DIAGNOSTICS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set the process-wide debug-diagnostics flag. Plain and idempotent: calling
/// it twice is a second answer to the same question, not an error, and a test
/// must be able to turn the flag back off — which is why this is not an
/// init-once cell that would silently ignore the second call (I-11).
pub fn set_runtime_debug_diagnostics(enabled: bool) {
    RUNTIME_DEBUG_DIAGNOSTICS.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// The ONE producer of "are runtime debug diagnostics on?" (I-12). Every
/// diagnostic in `tine-core` and in `src-tauri` — where `debug_enabled()` is a
/// thin delegate to this function — asks here.
pub fn runtime_debug_diagnostics_enabled() -> bool {
    RUNTIME_DEBUG_DIAGNOSTICS.load(std::sync::atomic::Ordering::Relaxed)
}

type DiagnosticLineSink = Box<dyn Fn(&str) + Send + Sync>;
static DIAGNOSTIC_LINE_SINK: std::sync::OnceLock<DiagnosticLineSink> = std::sync::OnceLock::new();

/// Install the host's copy of every [`diagnostic_line`]: the app writes it to
/// the opt-in `--debug` log. The first install wins.
pub fn set_diagnostic_line_sink(sink: impl Fn(&str) + Send + Sync + 'static) {
    let _ = DIAGNOSTIC_LINE_SINK.set(Box::new(sink));
}

/// The one way `tine-core` production code writes a line for a person to read.
///
/// It prints to stderr and hands the line to the host's sink. stderr alone
/// reached nobody: the Windows release app has no console, and the `--debug`
/// log held only the app's own lines, so every index failure a reporter hit
/// arrived as `other` with its text gone, even from a `--debug` run (GH #594).
/// `production_core_writes_stderr_only_through_diagnostic_line` keeps it the
/// only writer.
pub fn diagnostic_line(line: &str) {
    eprintln!("{line}");
    if let Some(sink) = DIAGNOSTIC_LINE_SINK.get() {
        sink(line);
    }
}

/// `format!` into [`diagnostic_line`].
macro_rules! core_diag {
    ($($arg:tt)*) => {
        $crate::backend_error::diagnostic_line(&::std::format!($($arg)*))
    };
}
pub(crate) use core_diag;

#[cfg(test)]
mod tests {
    use super::*;

    /// GH #594: the host's sink is handed every line, so the app can put it
    /// in the `--debug` log the reporter sends.
    #[test]
    fn a_diagnostic_line_reaches_the_host_sink() {
        static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        set_diagnostic_line_sink(|line| SEEN.lock().unwrap().push(line.to_owned()));
        core_diag!("[tine] probe {}", 594);
        assert!(SEEN
            .lock()
            .unwrap()
            .iter()
            .any(|line| line == "[tine] probe 594"));
    }

    /// GH #594: a line `tine-core` wrote with `eprintln!` reached no Windows
    /// reporter and no `--debug` log. Write it with `core_diag!` (see
    /// `direct_projection.rs`'s `projection_diag`), which also reaches the log.
    /// Command-line tools under `src/bin` and test files may print.
    #[test]
    fn production_core_writes_stderr_only_through_diagnostic_line() {
        fn visit(dir: &std::path::Path, offenders: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if path.is_dir() {
                    if name != "bin" && name != "tests" {
                        visit(&path, offenders);
                    }
                    continue;
                }
                if !name.ends_with(".rs") || name.contains("tests") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).unwrap();
                let production = source.split("#[cfg(test)]\nmod ").next().unwrap();
                let writes = production
                    .lines()
                    .filter(|line| !line.trim_start().starts_with("//"))
                    .filter(|line| line.contains("eprintln!(") || line.contains("eprint!("))
                    .count();
                let allowed = usize::from(name == "backend_error.rs");
                if writes != allowed {
                    offenders.push(format!("{} ({writes})", path.display()));
                }
            }
        }
        let mut offenders = Vec::new();
        visit(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut offenders,
        );
        assert!(
            offenders.is_empty(),
            "write through crate::backend_error::core_diag!, not eprintln! (GH #594): {offenders:?}"
        );
    }

    #[test]
    fn tagged_errors_encode_kind_reason_and_detail_as_json() {
        assert_eq!(
            tagged_backend_error("operation-cancelled", None),
            r#"{"kind":"operation-cancelled"}"#
        );
        assert_eq!(
            tagged_backend_error("query-not-ready", Some("projection-missing")),
            r#"{"kind":"query-not-ready","reason_code":"projection-missing"}"#
        );
        let with_detail = tagged_backend_error_with_reason_and_detail(
            "save-failed",
            "io",
            serde_json::json!({"error_kind": "NotFound"}),
        );
        let parsed: serde_json::Value = serde_json::from_str(&with_detail).unwrap();
        assert_eq!(parsed["kind"], "save-failed");
        assert_eq!(parsed["reason_code"], "io");
        assert_eq!(parsed["detail"]["error_kind"], "NotFound");
    }
}
