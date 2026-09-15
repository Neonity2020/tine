//! The Direct move-recovery journal's directory barrier.
//!
//! Graph-text publication does not come through here: its barriers are strict
//! on every platform, Android included (`model::sync_projection_directory`,
//! `docs/storage-sync-contract.md` §2.10a). The journal's barrier keeps one
//! Android tolerance: app sandboxes and vendor filesystems can deny directory
//! fsync even after permitting every exact file sync, and only that capability
//! refusal is accepted — never a real I/O error, and never on another platform.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use cap_std::ambient_authority;
use cap_std::fs::Dir;

mod atomic_fs;

pub(crate) use atomic_fs::{
    atomic_replace_expected, atomic_write_new, barrier_sync_all, move_file_noreplace, sync_dir,
    AtomicReplaceOutcome, RETIRED_SUFFIX,
};
#[cfg(test)]
pub(crate) use atomic_fs::{atomic_replace_expected_with_hooks, dir_fsync_is_unsupported};
pub use atomic_fs::{atomic_write, dir_fsync_error_is_unsupported, sync_dir_for_rename};

/// Read–modify–write a small text file (config.edn, device settings) under a lock,
/// committed via [`atomic_write`]. The ONE guarded path every settings writer goes
/// through, so the discipline is uniform rather than re-derived per call site:
///   - a MISSING file is the empty document `{}`, but any OTHER read error
///     (permission, NFS stale handle, transient I/O) ABORTS — otherwise `edit` would
///     rebuild the whole file from `{}` and destroy every other key (audit H2);
///   - the `lock` serializes concurrent writers to the same logical file so a
///     read-modify-write can't clobber a concurrent one (audit M1/M2);
///   - `edit` returns the new full contents, or an `Err` to abort without writing;
///   - the commit is atomic (temp + fsync + rename), so a crash can't truncate it.
pub fn atomic_update(
    path: &Path,
    lock: &std::sync::Mutex<()>,
    edit: impl Fn(&str) -> io::Result<String>,
) -> io::Result<()> {
    atomic_update_with_hooks(path, lock, edit, |_| {}, |_| {})
}

pub(crate) fn atomic_update_with_hooks(
    path: &Path,
    lock: &std::sync::Mutex<()>,
    edit: impl Fn(&str) -> io::Result<String>,
    before_recheck: impl Fn(usize),
    before_publish: impl Fn(usize),
) -> io::Result<()> {
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for attempt in 0..4 {
        let baseline = match fs::read_to_string(path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let next = edit(baseline.as_deref().unwrap_or("{}\n"))?;
        // CONFIG_LOCK serializes Tine writers, but Logseq/Syncthing do not take
        // it. Re-read immediately before publish and retry the key-local edit on
        // their new bytes instead of overwriting an external update with our stale
        // full-file copy.
        before_recheck(attempt);
        let current = match fs::read_to_string(path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if current != baseline {
            continue;
        }
        before_publish(attempt);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match baseline.as_deref() {
            // Creation is already no-clobber: `create_new` + no-replace rename.
            None => match atomic_write_new(path, next.as_bytes()) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            },
            // Update: the recheck above narrows the race but cannot close it -
            // an external writer can still land between it and the rename. Make
            // the publish itself conditional so their bytes cannot be lost.
            Some(current) => {
                match atomic_replace_expected(path, current.as_bytes(), next.as_bytes())? {
                    AtomicReplaceOutcome::Published => return Ok(()),
                    AtomicReplaceOutcome::ExternalChanged => continue,
                }
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "config changed repeatedly during update",
    ))
}

/// Make one directory-entry change of the Direct move-recovery journal durable:
/// a retired record, or an image entry recovery removed. Desktop platforms take
/// the strict barrier; Android accepts only the documented capability refusal.
pub(crate) fn sync_move_recovery_directory(path: &Path) -> io::Result<()> {
    let directory = Dir::open_ambient_dir(path, ambient_authority())?;
    crate::durability_counters::note(crate::durability_counters::Barrier::Directory);
    let result = tine_storage::sync_dir_required(&directory);
    #[cfg(target_os = "android")]
    return tolerate_android_capability_refusal(result);
    #[cfg(not(target_os = "android"))]
    result
}

/// The exact Android arm, reachable from host tests so the branch the device
/// takes is the branch under test.
#[cfg(any(test, target_os = "android"))]
fn tolerate_android_capability_refusal(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error)
            if !matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::Unsupported
                    | io::ErrorKind::InvalidInput
            ) =>
        {
            Err(error)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_tolerates_only_the_three_capability_refusals() {
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Unsupported,
            io::ErrorKind::InvalidInput,
        ] {
            tolerate_android_capability_refusal(Err(io::Error::new(kind, "denied"))).unwrap();
        }
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::Interrupted,
            io::ErrorKind::InvalidData,
            io::ErrorKind::WriteZero,
            io::ErrorKind::StorageFull,
            io::ErrorKind::Other,
        ] {
            let error =
                tolerate_android_capability_refusal(Err(io::Error::new(kind, "real I/O failure")))
                    .unwrap_err();
            assert_eq!(error.kind(), kind);
        }
    }
}
