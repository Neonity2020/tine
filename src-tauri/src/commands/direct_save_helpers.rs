use super::*;

pub(super) fn collect_graph_text(
    g: &tine_core::model::Graph,
    dir: &std::path::Path,
    max_bytes: u64,
    out: &mut Vec<GraphSourceFile>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_graph_text(g, &p, max_bytes, out);
            continue;
        }
        let format = match p.extension().and_then(|x| x.to_str()) {
            Some("md") => "md",
            Some("org") => "org",
            _ => continue,
        };
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.len() > max_bytes {
            continue; // oversized files skipped, like graph-check
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue; // non-UTF-8 / unreadable file skipped
        };
        out.push(GraphSourceFile {
            rel: g.rel_path(&p),
            text,
            format: format.to_string(),
            bytes: meta.len(),
        });
    }
}

/// A Direct-Markdown save is slow enough to be worth a line above this. Chosen
/// so ordinary saves stay silent (0.6.5 saved in single-digit milliseconds) while
/// anything a user would notice as a hitch is on the record.
const SAVE_DIAGNOSTIC_THRESHOLD_MS: u128 = 150;

/// Turn a failed Direct save into a message the frontend can act on.
///
/// The old mapping collapsed EVERY `AlreadyExists` to the literal string
/// "conflict", and the frontend recognised a conflict by testing whether the
/// message contained that substring. So a portable-filename collision, a
/// physical-resource alias, or "another document owns this page identity" all
/// raised the content-conflict prompt — whose two buttons are "Keep mine
/// (overwrite)" and "Use disk version", neither of which can resolve any of
/// them. Choosing either left the page marked conflicted, and a conflicted page
/// silently refuses to save from then on.
///
/// Only a real base-revision conflict gets the "conflict" contract now. Every
/// other failure carries its bounded code as a stable prefix, so the frontend
/// can classify it without sniffing prose and the user gets an error they can
/// read instead of a prompt that cannot help.
pub(crate) fn direct_save_error_message(error: std::io::Error) -> CommandError {
    let error = tine_core::model::DirectSaveError::ensure_io(error);
    let code = tine_core::model::direct_save_failure_code(&error);
    let io_error_kind = format!("{:?}", error.kind());
    if code.starts_with("conflict.") {
        return CommandError::tagged(
            "save-conflict",
            Some(code),
            Some(serde_json::json!({
                "io_error_kind": io_error_kind,
                "epoch": tine_core::model::direct_save_conflict_epoch(&error),
            })),
        );
    }
    CommandError::tagged(
        "direct-save-failure",
        Some(code),
        Some(serde_json::json!({ "io_error_kind": io_error_kind })),
    )
}

/// Report what a slow or failed Direct-Markdown save actually did.
///
/// Always on. Every field is a duration, a count, or a bounded failure code --
/// no page names, no paths, no content -- so the line is safe to paste into a
/// public issue, which is the only way it helps the people reporting #266/#267
/// from Windows machines we cannot reproduce.
///
/// The counters are the load-bearing part. `builds` distinguishes "the save was
/// slow" from "the save rebuilt a whole-graph index to answer a filename
/// question", and those have opposite fixes.
pub(super) fn report_direct_save_diagnostics(
    graph: &tine_core::model::Graph,
    elapsed: std::time::Duration,
    error: Option<&std::io::Error>,
) {
    if error.is_none() && elapsed.as_millis() < SAVE_DIAGNOSTIC_THRESHOLD_MS {
        return;
    }
    let report = graph.guarded_graph_text_identity_report();
    let outcome = match error {
        Some(error) => tine_core::model::direct_save_failure_code(error),
        None => "ok",
    };
    crate::debug::record_direct_save(
        outcome,
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        u64::try_from(report.complete_builds).unwrap_or(u64::MAX),
        u64::try_from(report.exact_updates).unwrap_or(u64::MAX),
        report.invalidated,
        report
            .last_build
            .as_ref()
            .map(|build| u64::try_from(build.captured_entries).unwrap_or(u64::MAX)),
        report
            .last_build
            .as_ref()
            .map(|build| u64::try_from(build.captured_bytes).unwrap_or(u64::MAX)),
    );
    let build = report.last_build.map_or_else(
        || " last_build=none".to_string(),
        |build| {
            format!(
                " last_build_capture_ms={} last_build_index_ms={} last_build_parsed={} last_build_entries={} last_build_bytes={}",
                build.capture.as_millis(),
                build.index.as_millis(),
                build.decode_semantics,
                build.captured_entries,
                build.captured_bytes,
            )
        },
    );
    crate::debug::diag(format!(
        "direct save: outcome={outcome} total_ms={} guarded_index_builds={} guarded_index_exact_updates={} guarded_index_invalidated={}{build}",
        elapsed.as_millis(),
        report.complete_builds,
        report.exact_updates,
        report.invalidated,
    ));
}
