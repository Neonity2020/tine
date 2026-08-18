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
    fmt, fs,
    io::{self, Write},
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
    (JOURNEY_EXTERNAL_EDIT_PAGE, JOURNEY_EXTERNAL_EDIT_BEFORE),
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
        JOURNEY_EXTERNAL_CREATED_PAGE,
        "- written by another editor\n",
    ),
    // An ordinary offline edit to an existing non-ASCII page.
    (JOURNEY_EXTERNAL_EDIT_PAGE, JOURNEY_EXTERNAL_EDIT_AFTER),
    // An honest backup copy: a second physical file whose decoded page name is
    // already owned. This is the reported refusal, arriving the ordinary way.
    ("archiv/2026/Denn\u{ed} pozn\u{e1}mky.md", "- backup copy\n"),
];

/// The page the outside writer EDITS.
///
/// It is a member of [`JOURNEY_NAME_SHAPES`], so the leg is only an edit when
/// the journey's own fixture is on disk. Driven against a graph that lacks the
/// fixture the same write becomes a CREATE, the backup copy under `archiv/`
/// sorts earlier and wins the decoded name, and the miss surfaces far from its
/// cause as `external edit did not reconcile: Missing`. The journey proves the
/// precondition instead of assuming it.
pub const JOURNEY_EXTERNAL_EDIT_PAGE: &str = "pages/Denn\u{ed} pozn\u{e1}mky.md";
/// What that page holds before the outside writer touches it.
pub const JOURNEY_EXTERNAL_EDIT_BEFORE: &str = "- ordinary non-ASCII page\n";
/// What the outside writer leaves there.
pub const JOURNEY_EXTERNAL_EDIT_AFTER: &str = "- ordinary non-ASCII page\n- edited outside Tine\n";
/// The block the external edit appends, as the editor surfaces it.
pub const JOURNEY_EXTERNAL_EDIT_BLOCK: &str = "edited outside Tine";
/// The page the outside writer CREATES.
pub const JOURNEY_EXTERNAL_CREATED_PAGE: &str = "pages/Extern\u{ed} novinka.md";
/// The block that created page carries, as the editor surfaces it.
pub const JOURNEY_EXTERNAL_CREATED_BLOCK: &str = "written by another editor";

/// Write one graph file and make it durable before the runtime can observe it.
///
/// A plain `fs::write` leaves the bytes in the page cache and the new directory
/// entry unsynced. That is invisible on a host, and it is exactly the ambiguity
/// a device run cannot afford: "the fixture was still landing when activation
/// started" has to be excluded by construction rather than argued about, since
/// activation refuses (`Retryable`) if the graph moves under it.
fn write_journey_file(target: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
        let mut file = fs::File::create(target)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        sync_journey_directory(parent)
    } else {
        let mut file = fs::File::create(target)?;
        file.write_all(bytes)?;
        file.sync_all()
    }
}

#[cfg(unix)]
fn sync_journey_directory(directory: &Path) -> io::Result<()> {
    fs::File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_journey_directory(_directory: &Path) -> io::Result<()> {
    Ok(())
}

/// Render a graph-relative path so two spellings cannot read as one.
///
/// `pages/\u{17d} pilot notes #pilot.md` and `pages/Z\u{30c} pilot notes #pilot.md`
/// are different files that print as the same glyph sequence everywhere. A
/// refusal about one of them is unreadable unless the non-ASCII is escaped.
fn escape_journey_path(path: &str) -> String {
    if path.is_ascii() {
        return path.to_owned();
    }
    let mut rendered = String::with_capacity(path.len());
    for character in path.chars() {
        if character.is_ascii() {
            rendered.push(character);
        } else {
            rendered.push_str(&format!("\\u{{{:x}}}", character as u32));
        }
    }
    rendered
}

/// Prove the fixture actually landed as the fixture, before anything reads it.
///
/// [`JOURNEY_NAME_SHAPES`] deliberately contains two pairs of names that a
/// filesystem may or may not treat as distinct: one pair differing only by
/// Unicode normalization, one only by case. On a filesystem that folds either
/// pair the second write lands on the FIRST file, and the tree the journey then
/// activates is not the tree it wrote — it is one file short, holding another
/// shape's bytes. Nothing downstream can attribute that: it surfaced on Android
/// as `source capture changed before final inventory proof … content:<26 bytes>
/// -> content:<25 bytes>` on a path that printed identically on both sides, and
/// reading it as content normalization was wrong (the 25 bytes are the sibling
/// shape's, verbatim).
///
/// So the fixture proves its own precondition, the way the external-edit leg
/// already proves its own: every shape must read back exactly the bytes written
/// for it, and a shape holding a SIBLING's bytes is reported as the folding it
/// is, naming both spellings.
fn verify_journey_graph_fixture(graph_root: &Path) -> io::Result<()> {
    for (path, bytes) in JOURNEY_NAME_SHAPES {
        let actual = fs::read(graph_root.join(path))?;
        if actual == bytes.as_bytes() {
            continue;
        }
        let folded = JOURNEY_NAME_SHAPES
            .iter()
            .find(|(other, other_bytes)| other != path && actual == other_bytes.as_bytes());
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            match folded {
                Some((other, _)) => format!(
                    "graph filesystem folds two journey page names into one file: {} reads back                      the bytes written for {} ({} bytes, not {}) — the two names differ only by                      Unicode normalization or by case, and this graph tree cannot hold both",
                    escape_journey_path(path),
                    escape_journey_path(other),
                    actual.len(),
                    bytes.len()
                ),
                None => format!(
                    "journey fixture shape {} reads back {} bytes, not the {} written for it",
                    escape_journey_path(path),
                    actual.len(),
                    bytes.len()
                ),
            },
        ));
    }
    Ok(())
}

/// Write the journey's graph tree. Callers own the (empty) `graph_root`.
pub fn write_journey_graph_fixture(graph_root: &Path) -> io::Result<()> {
    fs::create_dir_all(graph_root.join("pages"))?;
    fs::create_dir_all(graph_root.join("journals"))?;
    fs::create_dir_all(graph_root.join("logseq"))?;
    write_journey_file(&graph_root.join("logseq/config.edn"), b"{}\n")?;
    write_journey_file(
        &graph_root.join(JOURNEY_EDITED_PAGE),
        b"- Android managed storage smoke\n",
    )?;
    write_journey_file(
        &graph_root.join("journals/2026_08_18.md"),
        "- journal entry with #pilot and [[\u{17d} pilot notes #pilot]]\n".as_bytes(),
    )?;
    for (path, bytes) in JOURNEY_NAME_SHAPES {
        write_journey_file(&graph_root.join(path), bytes.as_bytes())?;
    }
    sync_journey_directory(graph_root)?;
    verify_journey_graph_fixture(graph_root)
}

/// Apply the outside writer's changes. Separated so the caller can prove the
/// runtime was already live when they landed.
fn apply_journey_external_writes(graph_root: &Path) -> io::Result<Vec<String>> {
    let mut written = Vec::new();
    for (path, bytes) in JOURNEY_EXTERNAL_WRITES {
        write_journey_file(&graph_root.join(path), bytes.as_bytes())?;
        written.push((*path).to_owned());
    }
    sync_journey_directory(graph_root)?;
    Ok(written)
}

/// The journey drives reconciliation by hand, so it must offer the runtime as
/// many turns as the graph it just changed needs.
///
/// A fixed 64 was below what the graph the device journey actually holds
/// requires: at the host boundary the same 1104-page corpus settled on tick 71,
/// so a correct runtime would have been reported as
/// `external reconciliation never admitted an epoch`. Derive the bound from the
/// graph rather than tuning a number: every turn is a bounded slice, so the
/// count scales with the file set, and the drain still refuses rather than
/// waiting forever.
fn journey_reconciliation_tick_budget(graph_root: &Path) -> usize {
    const RESERVE: usize = 256;
    RESERVE.saturating_add(count_journey_graph_text_files(graph_root))
}

fn count_journey_graph_text_files(root: &Path) -> usize {
    let mut stack = vec![root.to_path_buf()];
    let mut files = 0_usize;
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(entry.path()),
                Ok(kind) if kind.is_file() => {
                    let text = name
                        .rsplit_once('.')
                        .map(|(_, extension)| extension)
                        .is_some_and(|extension| {
                            extension.eq_ignore_ascii_case("md")
                                || extension.eq_ignore_ascii_case("markdown")
                                || extension.eq_ignore_ascii_case("org")
                        });
                    if text {
                        files = files.saturating_add(1);
                    }
                }
                _ => {}
            }
        }
    }
    files
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

/// A bounded account of one reconciliation drain.
///
/// The whole tick sequence used to be carried verbatim, which is unreadable the
/// moment the budget scales with the graph. Keep the opening turns (where a
/// refusal or an unexpected state shows up), the last turn, and the totals.
struct ReconciliationCensus {
    budget: usize,
    ticks: usize,
    admitted: usize,
    opening: Vec<String>,
    last: String,
}

impl ReconciliationCensus {
    /// How many opening turns one census keeps verbatim.
    const OPENING_TURNS: usize = 8;

    fn new(budget: usize) -> Self {
        Self {
            budget,
            ticks: 0,
            admitted: 0,
            opening: Vec::new(),
            last: "none".to_owned(),
        }
    }

    fn record(&mut self, tick: &SyncRuntimeTick) {
        self.ticks = self.ticks.saturating_add(1);
        if matches!(
            tick,
            SyncRuntimeTick::AdmittedComplete { .. } | SyncRuntimeTick::AdmittedNoop { .. }
        ) {
            self.admitted = self.admitted.saturating_add(1);
        }
        self.last = format!("{tick:?}");
        if self.opening.len() < Self::OPENING_TURNS {
            self.opening.push(self.last.clone());
        }
    }
}

impl fmt::Display for ReconciliationCensus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "ticks={}/{} admitted={} last={} opening={}",
            self.ticks,
            self.budget,
            self.admitted,
            self.last,
            self.opening.join("|")
        )
    }
}

/// Drive reconciliation the way the platform watcher does, and report the exact
/// tick census. A refusal is returned as evidence rather than retried away: on
/// the device it repeated forever, one red toast at a time.
fn drain_external_reconciliation(
    handle: &SyncRuntimeHandle,
    budget: usize,
) -> Result<ReconciliationCensus, String> {
    handle
        .observe_watcher(vec![SyncWatcherObservation::RescanRequired])
        .map_err(|error| format!("watcher observation refused: {error}"))?;
    let mut census = ReconciliationCensus::new(budget);
    let mut quiet = false;
    for _ in 0..budget {
        let tick = handle
            .tick()
            .map_err(|error| format!("reconciliation tick refused: {error}; {census}"))?;
        let settled = tick_is_settled(&tick);
        census.record(&tick);
        if tick_is_refusal(&tick) {
            return Err(format!(
                "external reconciliation refused: {tick:?}; {census}"
            ));
        }
        let watcher = handle
            .status()
            .map_err(|error| {
                format!("status unavailable during reconciliation: {error}; {census}")
            })?
            .watcher;
        if settled && !watcher.pending && !watcher.drain_in_flight {
            quiet = true;
            break;
        }
    }
    // Both halves are load-bearing. Without `quiet` a drain that merely ran out
    // of turns reads as a clean drain; without `admitted` a drain that settled
    // without ever admitting an epoch does.
    if !quiet {
        return Err(format!(
            "external reconciliation did not settle within its budget; {census}"
        ));
    }
    if census.admitted == 0 {
        return Err(format!(
            "external reconciliation never admitted an epoch; {census}"
        ));
    }
    Ok(census)
}

/// How many times the journey re-drives an activation that refused `Retryable`.
///
/// A graph that moves under a live activation is an IN-SCOPE scenario, not a
/// harness artefact: an external editor, a filesystem sync provider, or the
/// user's own second window saving while Tine is still importing. The runtime
/// answers it by refusing `Retryable` and retracting the disposable archive that
/// attempt created, so the next attempt starts from the current Direct Files
/// bytes. A journey that reports the first refusal proves the refusal and
/// nothing about the recovery — which is how this journey spent three device
/// rounds red. A journey that retried SILENTLY would be worse: it would hide a
/// device that refuses on every attempt. So the retries are bounded, and every
/// refusal retried past is carried verbatim in the receipt as
/// `activation_retried=`.
const JOURNEY_ACTIVATION_ATTEMPTS: usize = 3;

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
    let mut retried: Vec<String> = Vec::new();
    let mut attempts = 1_usize;
    let activation = loop {
        progress_receipt.clear();
        let result = SyncRuntimeHandle::activate_or_resume_local_with_detailed_progress(
            activation_request.clone(),
            |progress| {
                last_progress = format!("{progress:?}");
                progress_receipt.push(format!(
                    "{}@{}ms",
                    progress.diagnostic_name(),
                    journey_started.elapsed().as_millis()
                ));
            },
        );
        let retryable = matches!(result.status, SyncLocalActivationStatus::Retryable { .. });
        if !retryable || attempts >= JOURNEY_ACTIVATION_ATTEMPTS {
            break result;
        }
        retried.push(format!(
            "attempt {attempts} after {last_progress}: {:?}",
            result.status
        ));
        attempts = attempts.saturating_add(1);
    };
    let activation_ms = journey_started.elapsed().as_millis();
    let retried_receipt = if retried.is_empty() {
        "none".to_owned()
    } else {
        retried.join(" || ")
    };
    if activation.status != SyncLocalActivationStatus::Active {
        // The phase name alone does not say WHEN the phases ran, and the two
        // activation refusals this journey has produced were both about timing
        // under a graph that moved. Carry the per-phase millisecond receipt.
        return format!(
            "activation failed after {last_progress}: {:?}; activation_attempts={attempts}; activation_retried={retried_receipt}; activation_ms={activation_ms}; progress={}",
            activation.status,
            progress_receipt.join("|")
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
    //
    // Prove the leg's own precondition first. The edit target must ALREADY be a
    // page owning its decoded name, or the write below is a create racing the
    // `archiv/` backup copy for that name, and the miss surfaces later as an
    // uninterpretable `external edit did not reconcile: Missing`. That is
    // exactly how the resume instrumentation case failed while the runtime was
    // behaving as specified (CI 32108957903).
    match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: JOURNEY_EXTERNAL_EDIT_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. }) if page.blocks.len() == 1 => {}
        outcome => {
            return format!(
                "journey precondition failed: {JOURNEY_EXTERNAL_EDIT_PAGE} must already be an \
                 owned single-block page before the external edit — drive the journey against \
                 `write_journey_graph_fixture`: {outcome:?}"
            )
        }
    }
    let reconciliation_started = Instant::now();
    let external = match apply_journey_external_writes(&graph_root) {
        Ok(written) => written,
        Err(error) => return format!("external write failed: {error}"),
    };
    let reconciliation_budget = journey_reconciliation_tick_budget(&graph_root);
    let reconciliation = match drain_external_reconciliation(&handle, reconciliation_budget) {
        Ok(census) => census,
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
            path: JOURNEY_EXTERNAL_CREATED_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. })
            if page.blocks.first().map(|block| block.raw.as_str())
                == Some(JOURNEY_EXTERNAL_CREATED_BLOCK) => {}
        outcome => {
            return format!(
                "externally created page did not reconcile: {outcome:?}; {reconciliation}"
            )
        }
    }
    match handle.load_application_page(SyncApplicationPageLoadRequest {
        page: SyncApplicationPageSelector::ExactPath {
            path: JOURNEY_EXTERNAL_EDIT_PAGE.into(),
        },
    }) {
        Ok(SyncApplicationPageLoadOutcome::Loaded { page, .. })
            if page.blocks.len() == 2
                && page.blocks[1].raw.as_str() == JOURNEY_EXTERNAL_EDIT_BLOCK => {}
        outcome => {
            return format!("external edit did not reconcile: {outcome:?}; {reconciliation}")
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
            "ok activation_ms={activation_ms} activation_attempts={attempts} activation_retried={retried_receipt} first_page_ms={first_page_ms} crash_reopen_ms={crash_reopen_ms} reconciliation_ms={reconciliation_ms} total_ms={} {retained} {shared} reconciliation[{reconciliation}] progress={}",
            journey_started.elapsed().as_millis(),
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
        // EVERY case, not just the journey case. The resume case kept its own
        // smaller graph and so drove the shared journey against a tree the
        // journey's external leg does not describe; it failed on the shapes
        // that graph could not hold, not on anything the runtime got wrong.
        assert!(
            !instrumentation.contains("privateRoot.absolutePath,\n        false,"),
            "no instrumentation case may hand-maintain its own copy of the journey graph"
        );
    }

    /// The fixture must refuse a graph tree that could not hold it.
    ///
    /// Two of [`JOURNEY_NAME_SHAPES`] differ from a sibling only by Unicode
    /// normalization, two only by case. A filesystem that folds either pair
    /// silently swallows one write: the tree the journey activates is a file
    /// short and holds a sibling's bytes, and every refusal downstream is about
    /// something else. On Android that arrived as `source capture changed
    /// before final inventory proof … content:…@26 -> content:…@25` on a path
    /// that printed identically on both sides — 25 bytes being the SIBLING
    /// shape's content verbatim, not a normalized copy of the 26.
    ///
    /// The folding is simulated here rather than waited for: no host filesystem
    /// this suite runs on folds those names, which is precisely why the journey
    /// has to carry the proof to the platforms that might.
    #[test]
    fn the_fixture_refuses_a_graph_tree_that_folds_two_of_its_shapes() {
        let root = std::env::temp_dir().join(format!(
            "tine-journey-fixture-folding-{}",
            uuid::Uuid::new_v4()
        ));
        super::write_journey_graph_fixture(&root).expect("an ordinary tree holds every shape");

        // Exactly what a folding filesystem leaves behind: the decomposed
        // write landed on the precomposed file.
        std::fs::write(
            root.join("pages/\u{17d} pilot notes #pilot.md"),
            "- decomposed pilot notes\n",
        )
        .unwrap();
        let refusal = super::verify_journey_graph_fixture(&root)
            .expect_err("a folded shape must be refused where it happened")
            .to_string();
        assert!(
            refusal.contains("folds two journey page names into one file"),
            "{refusal}"
        );
        // Both spellings, escaped: they print as one glyph sequence otherwise.
        assert!(
            refusal.contains("pages/\\u{17d} pilot notes #pilot.md"),
            "{refusal}"
        );
        assert!(
            refusal.contains("pages/Z\\u{30c} pilot notes #pilot.md"),
            "{refusal}"
        );
        let _ = std::fs::remove_dir_all(&root);
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
