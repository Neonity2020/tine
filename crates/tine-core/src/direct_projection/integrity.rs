//! The index's integrity check, off the launch path (GH #550, #543; design
//! `2026-09-24-launch-serve-stored-design`, D1; approved by Martin 2026-09-24).
//!
//! The image is SQLite in WAL mode at `synchronous=NORMAL`: a killed process
//! (Android's memory and power management, a crash, a force-stop) cannot
//! damage it. What can is power loss or an OS crash on storage that does not
//! honour fsync, and disk errors. So the check is owed only when the OS may
//! have gone down since the image was last checked -- another boot -- or when
//! it has not been checked for [`CHECK_INTERVAL`]. It then runs on its own
//! read-only connection beside everything else: no reader, survey or worker
//! turn waits for it. `PRAGMA quick_check` took 1.2-1.7 s on a 10k-page graph
//! and ran before every launch's first answer.
//!
//! Damage it finds owes a fresh image, like damage a read meets
//! (`failure_owes_new_image`). The record of the last pass lives beside the
//! image in the app's data directory, never in the graph; losing it only
//! costs one more check.

use super::*;

/// How long a passed check is trusted on the same boot.
pub(crate) const CHECK_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// Two boot times this close are the same boot: the boot time is derived
/// from the wall clock minus the time since boot, which drifts a little.
const SAME_BOOT_TOLERANCE_SECS: i64 = 120;

const RECORD_HEADER: &str = "tine-index-integrity v1";

/// The last check that passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PassRecord {
    /// The OS boot it ran in, as seconds since the Unix epoch; `None` when
    /// the platform could not say.
    pub(super) boot: Option<i64>,
    /// When it passed, seconds since the Unix epoch.
    pub(super) checked_at: u64,
}

pub(super) fn record_path(image: &Path) -> PathBuf {
    let mut name = image.file_name().unwrap_or_default().to_os_string();
    name.push(".integrity");
    image.with_file_name(name)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

pub(super) fn read_record(image: &Path) -> Option<PassRecord> {
    let text = std::fs::read_to_string(record_path(image)).ok()?;
    let mut lines = text.lines();
    if lines.next()? != RECORD_HEADER {
        return None;
    }
    let boot = match lines.next()?.strip_prefix("boot ")? {
        "unknown" => None,
        value => Some(value.parse().ok()?),
    };
    let checked_at = lines.next()?.strip_prefix("checked ")?.parse().ok()?;
    Some(PassRecord { boot, checked_at })
}

pub(super) fn write_record(image: &Path, record: PassRecord) {
    let boot = record
        .boot
        .map_or_else(|| "unknown".to_owned(), |boot| boot.to_string());
    let text = format!(
        "{RECORD_HEADER}\nboot {boot}\nchecked {}\n",
        record.checked_at
    );
    // A torn or failed write reads back as no record: one more check.
    let _ = std::fs::write(record_path(image), text);
}

/// Record that the image at `image` is known intact now: a fresh build checks
/// the image it publishes.
pub(super) fn record_pass_now(image: &Path) {
    write_record(
        image,
        PassRecord {
            boot: boot_time_secs(),
            checked_at: now_secs(),
        },
    );
}

/// Whether the image is owed a check: never checked (or the record is
/// unreadable), checked in another boot, or checked too long ago. A clock
/// that went far backwards counts as too long ago.
pub(super) fn check_due(record: Option<PassRecord>, now: u64, boot: Option<i64>) -> bool {
    let Some(record) = record else {
        return true;
    };
    let interval = CHECK_INTERVAL.as_secs();
    if now.saturating_sub(record.checked_at) >= interval
        || record.checked_at.saturating_sub(now) >= interval
    {
        return true;
    }
    match (boot, record.boot) {
        (Some(boot), Some(recorded)) => (boot - recorded).abs() > SAME_BOOT_TOLERANCE_SECS,
        // Either boot unknown: the interval alone decides.
        _ => false,
    }
}

/// When the OS booted, in seconds since the Unix epoch: the wall clock minus
/// the time since boot, sleep included. Every shipped target has an arm
/// (AGENTS.md: a platform list names every target); `None` means the call
/// failed, and then only [`CHECK_INTERVAL`] schedules checks.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn boot_time_secs() -> Option<i64> {
    let mut since_boot = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes one `timespec` through a valid pointer.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut since_boot) } != 0 {
        return None;
    }
    i64::try_from(now_secs())
        .ok()
        .map(|now| now - i64::from(since_boot.tv_sec))
}

#[cfg(windows)]
pub(super) fn boot_time_secs() -> Option<i64> {
    // SAFETY: no arguments; returns milliseconds since boot, sleep included.
    let since_boot = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() };
    i64::try_from(now_secs())
        .ok()
        .and_then(|now| Some(now - i64::try_from(since_boot / 1000).ok()?))
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(super) fn boot_time_secs() -> Option<i64> {
    let mut boot = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut size = std::mem::size_of::<libc::timeval>();
    let mut name = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    // SAFETY: `sysctl` writes at most `size` bytes into `boot`.
    let status = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            2,
            (&mut boot as *mut libc::timeval).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (status == 0 && boot.tv_sec > 0).then(|| i64::from(boot.tv_sec))
}

/// Not a Tine platform: no boot time.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    windows,
    target_os = "macos",
    target_os = "ios"
)))]
pub(super) fn boot_time_secs() -> Option<i64> {
    None
}

/// Start the check in the background if the image is owed one. Called by the
/// worker once it has opened a stored image.
pub(super) fn start_if_due(shared: &Arc<ProjectionShared>) {
    let boot = boot_time_secs();
    if !check_due(read_record(&shared.path), now_secs(), boot) {
        return;
    }
    {
        let mut pending = shared.pending.lock().unwrap();
        if pending.stop || pending.integrity_running {
            return;
        }
        pending.integrity_running = true;
    }
    #[cfg(test)]
    shared
        .integrity_checks_started
        .fetch_add(1, Ordering::Relaxed);
    let checker = Arc::clone(shared);
    let spawned = std::thread::Builder::new()
        .name("tine-index-check".into())
        .spawn(move || {
            let outcome = run_check(&checker);
            finish(&checker, outcome, boot);
        });
    if spawned.is_err() {
        shared.pending.lock().unwrap().integrity_running = false;
        shared.changed.notify_all();
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CheckOutcome {
    Intact,
    Damaged,
    /// Cancelled by a drain or close, or the check itself could not run:
    /// nothing is known, and the next launch checks again.
    Unknown,
}

fn run_check(shared: &ProjectionShared) -> CheckOutcome {
    #[cfg(test)]
    {
        let pause = shared.integrity_check_pause.lock().unwrap().take();
        if let Some(pause) = pause {
            pause.0.wait();
            pause.1.wait();
        }
    }
    let Ok(mut snapshot) = PhysicalProjectionQuerySnapshot::open_direct(&shared.path, || Ok(()))
    else {
        return CheckOutcome::Unknown;
    };
    {
        let pending = shared.pending.lock().unwrap();
        if pending.stop {
            return CheckOutcome::Unknown;
        }
        *shared.integrity_check.lock().unwrap() = Some(snapshot.cancellation());
    }
    let mut verdict = None;
    let result =
        crate::query::projection_sql::visit(&mut snapshot, "PRAGMA quick_check(1)", &[], |row| {
            if let Some(PhysicalQueryValue::Text(text)) = row.first() {
                verdict = Some(text == "ok");
            }
            Ok(std::ops::ControlFlow::Break(()))
        });
    let cancelled = snapshot.cancellation().is_cancelled();
    shared.integrity_check.lock().unwrap().take();
    drop(snapshot);
    #[cfg(test)]
    if shared.inject_integrity_damage.swap(false, Ordering::AcqRel) {
        return CheckOutcome::Damaged;
    }
    match (result, verdict) {
        _ if cancelled => CheckOutcome::Unknown,
        (Ok(()), Some(true)) => CheckOutcome::Intact,
        (Ok(()), Some(false)) => CheckOutcome::Damaged,
        (Err(error), _)
            if crate::query::IndexFailureClass::of_message(&error.to_string())
                == crate::query::IndexFailureClass::Corrupt =>
        {
            CheckOutcome::Damaged
        }
        _ => CheckOutcome::Unknown,
    }
}

fn finish(shared: &ProjectionShared, outcome: CheckOutcome, boot: Option<i64>) {
    projection_diag(|| format!("background integrity check: {outcome:?}"));
    match outcome {
        CheckOutcome::Intact => write_record(
            &shared.path,
            PassRecord {
                boot,
                checked_at: now_secs(),
            },
        ),
        CheckOutcome::Damaged => {
            eprintln!("[tine] the search index is damaged; rebuilding it");
            owner::report_index_failure(IndexFailureEvent {
                class: crate::query::IndexFailureClass::Corrupt,
                attempt: 0,
                terminal: false,
            });
            owner::request_rebuild(shared);
        }
        CheckOutcome::Unknown => {}
    }
    shared.pending.lock().unwrap().integrity_running = false;
    shared.changed.notify_all();
}

/// Interrupt a running check and wait until it has let go of the image. Every
/// drain calls this: a replacement image is published over the file (on
/// Windows an open reader would refuse that), and a close must leave no
/// connection behind.
pub(super) fn cancel_and_wait(shared: &ProjectionShared) {
    if let Some(cancellation) = shared.integrity_check.lock().unwrap().as_ref() {
        cancellation.cancel();
    }
    let mut pending = shared.pending.lock().unwrap();
    while pending.integrity_running {
        // A check between its open and its registration sees `stop` or
        // registers and is cancelled on the next wake.
        if let Some(cancellation) = shared.integrity_check.lock().unwrap().as_ref() {
            cancellation.cancel();
        }
        pending = shared
            .changed
            .wait_timeout(pending, std::time::Duration::from_millis(20))
            .unwrap()
            .0;
    }
}

/// Run the check now on the calling thread; whether the image is intact.
#[cfg(test)]
pub(super) fn run_check_now(shared: &ProjectionShared) -> bool {
    run_check(shared) == CheckOutcome::Intact
}
