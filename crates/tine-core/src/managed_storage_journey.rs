//! The one Tine-managed-storage journey the Android instrumentation runs.
//!
//! It lives here, in the crate both boundaries link, so the host test and the
//! device instrumentation drive the SAME fixture and the SAME call sequence.
//! When they were two hand-maintained copies they diverged, and the divergence
//! was invisible: `ManagedStorageSmokeTest` was green on CI in the same round a
//! physical device flooded the app with
//! `clean external reconciliation failed during Planning: decoded destination
//! logical page name … is already owned by page <uuid>` — because the journey
//! activated, saved, reopened and shared, and never once drove an external
//! change through reconciliation planning (GH: Android, 2026-08-18).
//!
//! ## What this journey does and does not prove
//!
//! It exercises the NATIVE runtime as the app's UID against a real graph tree:
//! activation, an application save, a force-close reopen, clean external
//! reconciliation of changes another writer made under the graph root, the
//! shared-enrollment cut, and clean shutdown.
//!
//! It is deliberately NOT app coverage. There is no WebView, no Tauri command
//! layer, no watcher thread, and no UI: watcher observations are delivered by
//! this code rather than by inotify/`FileObserver`. It is also ONE fixture of a
//! few dozen pages, not corpus scale — a shape gate, not a load gate. A green
//! receipt means the native call sequence works on these shapes at this size,
//! and nothing more.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Instant,
};

use crate::sync_runtime::{
    SyncApplicationPageLoadOutcome, SyncApplicationPageLoadRequest, SyncApplicationPageSaveOutcome,
    SyncApplicationPageSaveRequest, SyncApplicationPageSaveTarget, SyncApplicationPageSelector,
    SyncLocalActivationRequest, SyncLocalActivationStatus, SyncRuntimeHandle,
    SyncRuntimeOpenRequest, SyncRuntimeOpenStatus, SyncRuntimeTick, SyncShutdownOutcome,
    SyncWatcherObservation,
};

/// The page the journey edits through the application save path.
pub const JOURNEY_EDITED_PAGE: &str = "pages/Smoke.md";
/// What that save must leave on disk.
pub const JOURNEY_EDITED_BYTES: &str = "- Android managed storage edited\n";

/// Page names an ordinary human graph contains and a synthetic corpus does not.
///
/// Every shape here has cost this project a real defect or is one edit away
/// from doing so: a non-ASCII letter, the same letter precomposed AND
/// decomposed, an inline `#hashtag` inside a page name, spaces, one title
/// spelled two ways on disk (`#` literal versus the `%23` Tine's own filename
/// encoder writes), and two file names that differ only by case. Synthetic
/// corpora keep missing exactly these, which is why the shapes are pinned in
/// code rather than left to whoever writes the next fixture.
///
/// Content is synthetic. Only the SHAPES come from the field report.
const JOURNEY_NAME_SHAPES: &[(&str, &str)] = &[
    // Non-ASCII (precomposed U+017D), spaces, and an inline hashtag in the name.
    (
        "pages/\u{17d} pilot notes #pilot.md",
        "- precomposed pilot notes\n",
    ),
    // The same name decomposed (Z + U+030C): one portable path identity, two
    // exact spellings — what syncing a graph between a decomposing and a
    // precomposing filesystem produces.
    (
        "pages/Z\u{30c} pilot notes #pilot.md",
        "- decomposed pilot notes\n",
    ),
    // The same title again with the hashtag percent-encoded, which is what
    // `encode_page_name` writes for `#`. Same canonical page name, different
    // portable path.
    (
        "pages/\u{17d} pilot notes %23pilot.md",
        "- encoded pilot notes\n",
    ),
    // Two file names that differ only by case.
    (
        "pages/K\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md",
        "- horse\n",
    ),
    (
        "pages/k\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md",
        "- lowercase horse\n",
    ),
    // An ordinary non-ASCII page with no twin, edited externally later.
    (
        "pages/Denn\u{ed} pozn\u{e1}mky.md",
        "- ordinary non-ASCII page\n",
    ),
    // A nested layout with a space in a directory component.
    (
        "notes/archiv 2026/Star\u{fd} z\u{e1}pis.md",
        "- archived note\n",
    ),
];

/// What an outside writer does to the graph while Tine holds it, driving the
/// clean external reconciliation leg.
const JOURNEY_EXTERNAL_WRITES: &[(&str, &str)] = &[
    // A brand-new page from another editor or a filesystem sync provider.
    (
        "pages/Extern\u{ed} novinka.md",
        "- written by another editor\n",
    ),
    // An ordinary offline edit to an existing non-ASCII page.
    (
        "pages/Denn\u{ed} pozn\u{e1}mky.md",
        "- ordinary non-ASCII page\n- edited outside Tine\n",
    ),
    // An honest backup copy: a second physical file whose decoded page name is
    // already owned. This is the reported refusal, arriving the ordinary way.
    ("archiv/2026/Denn\u{ed} pozn\u{e1}mky.md", "- backup copy\n"),
];

/// Write the journey's graph tree. Callers own the (empty) `graph_root`.
pub fn write_journey_graph_fixture(graph_root: &Path) -> io::Result<()> {
    fs::create_dir_all(graph_root.join("pages"))?;
    fs::create_dir_all(graph_root.join("journals"))?;
    fs::create_dir_all(graph_root.join("logseq"))?;
    fs::write(graph_root.join("logseq/config.edn"), b"{}\n")?;
    fs::write(
        graph_root.join(JOURNEY_EDITED_PAGE),
        b"- Android managed storage smoke\n",
    )?;
    fs::write(
        graph_root.join("journals/2026_08_18.md"),
        "- journal entry with #pilot and [[\u{17d} pilot notes #pilot]]\n".as_bytes(),
    )?;
    for (path, bytes) in JOURNEY_NAME_SHAPES {
        let target = graph_root.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, bytes.as_bytes())?;
    }
    Ok(())
}

/// Apply the outside writer's changes. Separated so the caller can prove the
/// runtime was already live when they landed.
fn apply_journey_external_writes(graph_root: &Path) -> io::Result<Vec<String>> {
    let mut written = Vec::new();
    for (path, bytes) in JOURNEY_EXTERNAL_WRITES {
        let target = graph_root.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, bytes.as_bytes())?;
        written.push((*path).to_owned());
    }
    Ok(written)
}

fn tick_is_settled(tick: &SyncRuntimeTick) -> bool {
    matches!(
        tick,
        SyncRuntimeTick::Idle
            | SyncRuntimeTick::AdmittedNoop { .. }
            | SyncRuntimeTick::AdmittedComplete { .. }
    )
}

fn tick_is_refusal(tick: &SyncRuntimeTick) -> bool {
    matches!(
        tick,
        SyncRuntimeTick::Failed(_)
            | SyncRuntimeTick::Blocked(_)
            | SyncRuntimeTick::RecoveryBlocked(_)
            | SyncRuntimeTick::Terminal(_)
    )
}

/// Drive reconciliation the way the platform watcher does, and report the exact
/// tick sequence. A refusal is returned as evidence rather than retried away:
/// on the device it repeated forever, one red toast at a time.
fn drain_external_reconciliation(handle: &SyncRuntimeHandle) -> Result<Vec<String>, String> {
    handle
        .observe_watcher(vec![SyncWatcherObservation::RescanRequired])
        .map_err(|error| format!("watcher observation refused: {error}"))?;
    let mut ticks = Vec::new();
    let mut admitted = false;
    for _ in 0..64 {
        let tick = handle
            .tick()
            .map_err(|error| format!("reconciliation tick refused: {error}; ticks={ticks:?}"))?;
        let settled = tick_is_settled(&tick);
        admitted = admitted
            || matches!(
                tick,
                SyncRuntimeTick::AdmittedComplete { .. } | SyncRuntimeTick::AdmittedNoop { .. }
            );
        if tick_is_refusal(&tick) {
            ticks.push(format!("{tick:?}"));
            return Err(format!(
                "external reconciliation refused: {tick:?}; ticks={}",
                ticks.join("|")
            ));
        }
        ticks.push(format!("{tick:?}"));
        let watcher = handle
            .status()
            .map_err(|error| format!("status unavailable during reconciliation: {error}"))?
            .watcher;
        if settled && !watcher.pending && !watcher.drain_in_flight {
            break;
        }
    }
    if !admitted {
        return Err(format!(
            "external reconciliation never admitted an epoch; ticks={}",
            ticks.join("|")
        ));
    }
    Ok(ticks)
}

/// Run the whole journey and return its receipt. `Ok` receipts start with
/// `"ok "`; every other string is the refusal, carrying what localises it.
pub fn run_managed_storage_journey(
    graph_root: PathBuf,
    open_request: SyncRuntimeOpenRequest,
    activation_request: SyncLocalActivationRequest,
) -> String {
    let journey_started = Instant::now();
    let mut last_progress = "activation-not-started".to_string();
    let mut progress_receipt = Vec::new();
    let activation = SyncRuntimeHandle::activate_or_resume_local_with_detailed_progress(
        activation_request,
        |progress| {
            last_progress = format!("{progress:?}");
            progress_receipt.push(format!(
                "{}@{}ms",
                progress.diagnostic_name(),
                journey_started.elapsed().as_millis()
            ));
        },
    );
    let activation_ms = journey_started.elapsed().as_millis();
    if activation.status != SyncLocalActivationStatus::Active {
        return format!(
            "activation failed after {last_progress}: {:?}",
            activation.status
        );
    }
    let Some(handle) = activation.handle else {
        return "activation returned Active without a handle".into();
    };
    let first_page_started = Instant::now();
    let (mut page, revision) = match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: JOURNEY_EDITED_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, revision }) => (page, revision),
        outcome => return format!("post-activation page load failed: {outcome:?}"),
    };
    let first_page_ms = first_page_started.elapsed().as_millis();
    let Some(first) = page.blocks.first_mut() else {
        return "post-activation page has no editable block".into();
    };
    first.raw = JOURNEY_EDITED_BYTES.trim_start_matches("- ").trim().into();
    let save_outcome = handle.save_application_page(SyncApplicationPageSaveRequest {
        target: SyncApplicationPageSaveTarget::Existing {
            path: page.path.clone(),
            revision,
        },
        page,
    });
    // A clean-runtime save can succeed only after settling a RETAINED
    // publication, and settling it consumes the failure that caused it. Read
    // the report either way: a converged retry still costs a retry on every
    // write, and a bare "ok" receipt would hide the underlying cause.
    let retained = match handle.last_retained_publication() {
        Ok(Some(report)) => format!(
            "retained_publication=batch:{} phase:{:?} settled:{} turns:{} detail:{}",
            report.batch_id, report.phase, report.settled, report.settle_turns, report.detail
        ),
        Ok(None) => "retained_publication=none".to_owned(),
        Err(error) => format!("retained_publication=unavailable:{error}"),
    };
    match save_outcome {
        Ok(SyncApplicationPageSaveOutcome::Saved { .. }) => {}
        // The instrumentation boundary can only report the returned value, so
        // carry everything that distinguishes one refusal from another: the
        // Display form (which names the stage and reason code), the debug
        // detail, and the runtime's own status.
        outcome => {
            let detail = match &outcome {
                Err(error) => format!(
                    "display={error}; debug_detail={:?}",
                    error.debug_detail().unwrap_or("none")
                ),
                Ok(_) => "not a refusal".to_owned(),
            };
            return format!(
                "post-activation page save failed: {outcome:?}; {detail}; {retained}; status={}; progress={}",
                describe_status(&handle),
                progress_receipt.join("|")
            );
        }
    }
    // Match a force-closed app: do not ask the actor for a clean drain.  Drop
    // the last sender, let the actor stop, and prove the exact durable edit can
    // be recovered by a fresh runtime open before testing sharing.
    drop(handle);

    let reopen_started = Instant::now();
    let crashed_reopen = SyncRuntimeHandle::open(open_request.clone());
    let crash_reopen_ms = reopen_started.elapsed().as_millis();
    if crashed_reopen.status != SyncRuntimeOpenStatus::Active {
        return format!("crash-style reopen failed: {:?}", crashed_reopen.status);
    }
    let Some(handle) = crashed_reopen.handle else {
        return "crash-style reopen returned Active without a handle".into();
    };
    match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: JOURNEY_EDITED_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. })
            if page.blocks.first().map(|block| block.raw.as_str())
                == Some(JOURNEY_EDITED_BYTES.trim_start_matches("- ").trim()) => {}
        outcome => return format!("crash-style reopened page mismatch: {outcome:?}"),
    }

    // The leg this journey was missing: another writer changes the graph tree
    // under a live managed runtime, and reconciliation must plan and apply it.
    let reconciliation_started = Instant::now();
    let external = match apply_journey_external_writes(&graph_root) {
        Ok(written) => written,
        Err(error) => return format!("external write failed: {error}"),
    };
    let reconciliation_ticks = match drain_external_reconciliation(&handle) {
        Ok(ticks) => ticks,
        Err(detail) => {
            return format!(
                "{detail}; external={}; status={}; progress={}",
                external.join(","),
                describe_status(&handle),
                progress_receipt.join("|")
            )
        }
    };
    let reconciliation_ms = reconciliation_started.elapsed().as_millis();
    // The externally created page is a page now, and the externally edited one
    // carries the outside edit.
    match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: "pages/Extern\u{ed} novinka.md".into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. })
            if page.blocks.first().map(|block| block.raw.as_str())
                == Some("written by another editor") => {}
        outcome => {
            return format!(
                "externally created page did not reconcile: {outcome:?}; ticks={}",
                reconciliation_ticks.join("|")
            )
        }
    }
    match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: "pages/Denn\u{ed} pozn\u{e1}mky.md".into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. })
            if page.blocks.len() == 2 && page.blocks[1].raw.as_str() == "edited outside Tine" => {}
        outcome => {
            return format!(
                "external edit did not reconcile: {outcome:?}; ticks={}",
                reconciliation_ticks.join("|")
            )
        }
    }

    let shared = match handle.prepare_shared() {
        Ok(descriptor) => format!("shared_descriptor={}", descriptor.descriptor_digest),
        Err(error) => return format!("prepare shared failed: {error}"),
    };
    // A successful enrollment cut retires the actor, so this shutdown reads
    // the runtime's own final state rather than reaching a live thread.
    // `ActorUnavailable` carries no payload at all, so never report a shutdown
    // refusal bare: `status=` distinguishes "the runtime stopped Safe and this
    // is merely a report of it" from "the snapshot itself is unreachable, so
    // the actor died some way the retirement contract does not describe".
    match handle.clean_shutdown() {
        Ok(SyncShutdownOutcome::Safe(_)) => {}
        outcome => {
            return format!(
                "clean shutdown failed: {outcome:?}; {shared}; status={}; {retained}; progress={}",
                describe_status(&handle),
                progress_receipt.join("|")
            )
        }
    }
    drop(handle);

    let reopened = SyncRuntimeHandle::open(open_request);
    if reopened.status != SyncRuntimeOpenStatus::Active {
        return format!(
            "reopen after sharing failed: {:?}; {shared}",
            reopened.status
        );
    }
    let Some(handle) = reopened.handle else {
        return "reopen returned Active without a handle".into();
    };
    match handle.clean_shutdown() {
        Ok(SyncShutdownOutcome::Safe(_)) => format!(
            "ok activation_ms={activation_ms} first_page_ms={first_page_ms} crash_reopen_ms={crash_reopen_ms} reconciliation_ms={reconciliation_ms} total_ms={} {retained} {shared} reconciliation_ticks={} progress={}",
            journey_started.elapsed().as_millis(),
            reconciliation_ticks.len(),
            progress_receipt.join("|")
        ),
        outcome => format!(
            "reopened clean shutdown failed: {outcome:?}; {shared}; status={}",
            describe_status(&handle)
        ),
    }
}

/// The instrumentation boundary can report only returned values, and a
/// `SyncRuntimeRequestError` carries no state. Name the runtime's own view of
/// itself next to every refusal so one CI round trip localises it.
fn describe_status(handle: &SyncRuntimeHandle) -> String {
    match handle.status() {
        Ok(status) => format!(
            "lifecycle:{:?} recovery:{:?} shared_role:{:?} shared_phase:{:?} provider_pending:{} managed_local_pending:{} detail:{}",
            status.lifecycle,
            status.recovery,
            status.shared_role,
            status.shared_phase,
            status.provider_pending,
            status.managed_local_pending,
            status.detail.as_deref().unwrap_or("none")
        ),
        Err(error) => format!("unavailable:{error}"),
    }
}

#[cfg(test)]
mod tests {
    /// The Android boundary must DELEGATE to this journey, never re-implement
    /// it. Two hand-maintained copies is the state that let `ManagedStorageSmoke`
    /// stay green while a device failed: the shim owned its own fixture and its
    /// own call sequence, so nothing forced the two to describe the same app.
    ///
    /// This is a source-shape guard because the shim is `cfg(target_os =
    /// "android")` and cannot be compiled, let alone run, on this host.
    #[test]
    fn the_android_shim_delegates_to_this_journey() {
        let shim = include_str!("../../../src-tauri/src/android_managed_storage_smoke.rs");
        assert!(
            shim.contains("run_managed_storage_journey")
                && shim.contains("write_journey_graph_fixture"),
            "the Android instrumentation shim must drive the shared journey"
        );
        assert!(
            !shim.contains("load_application_page") && !shim.contains("prepare_shared"),
            "the Android shim must not carry its own copy of the call sequence"
        );
        let instrumentation = include_str!(
            "../../../src-tauri/gen/android/app/src/androidTest/java/page/tine/app/ManagedStorageSmokeTest.kt"
        );
        assert!(
            instrumentation.contains("writeFixture: Boolean"),
            "the instrumentation test must ask the native side for the shared fixture"
        );
        let journey_case = instrumentation
            .split_once("fun activationEditCrashReopenShareSetupCleanShutdownAndReopen")
            .and_then(|(_, tail)| tail.split_once("@Test"))
            .map(|(body, _)| body)
            .expect("the journey instrumentation case must remain identifiable");
        assert!(
            !journey_case.contains(".writeText(\"- Android managed storage smoke"),
            "the journey case must not write its own copy of the journey fixture"
        );
        assert!(
            journey_case.contains("privateRoot.absolutePath,\n        true,"),
            "the journey case must ask for the shared fixture"
        );
    }

    /// The shapes are the point. If someone trims this list, the journey stops
    /// being a gate for the class of defect it was extended to catch.
    #[test]
    fn the_journey_fixture_keeps_its_real_graph_name_shapes() {
        let paths = super::JOURNEY_NAME_SHAPES
            .iter()
            .map(|(path, _)| *path)
            .collect::<Vec<_>>();
        assert!(paths.iter().any(|path| path.contains('\u{17d}')));
        assert!(paths.iter().any(|path| path.contains('\u{30c}')));
        assert!(paths.iter().any(|path| path.contains('#')));
        assert!(paths.iter().any(|path| path.contains("%23")));
        assert!(paths.iter().any(|path| path.contains(' ')));
        // One pair differs only by case, one only by normalization.
        assert!(paths.contains(&"pages/K\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md"));
        assert!(paths.contains(&"pages/k\u{16f}\u{148} b\u{11b}\u{17e}\u{ed}.md"));
        assert!(paths.contains(&"pages/\u{17d} pilot notes #pilot.md"));
        assert!(paths.contains(&"pages/Z\u{30c} pilot notes #pilot.md"));
        // The external leg must keep creating a second physical file for an
        // already-owned page name: that is the reported refusal's shape.
        assert!(super::JOURNEY_EXTERNAL_WRITES
            .iter()
            .any(|(path, _)| path.starts_with("archiv/")
                && path.ends_with("Denn\u{ed} pozn\u{e1}mky.md")));
    }
}
