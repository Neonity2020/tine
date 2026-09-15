//! The Direct move-recovery journal's directory barrier.
//!
//! Graph-text publication does not come through here: its barriers are strict
//! on every platform, Android included (`model::sync_projection_directory`,
//! `docs/storage-sync-contract.md` §2.10a). The journal's barrier keeps one
//! Android tolerance: app sandboxes and vendor filesystems can deny directory
//! fsync even after permitting every exact file sync, and only that capability
//! refusal is accepted — never a real I/O error, and never on another platform.

use std::io;
use std::path::Path;

use cap_std::ambient_authority;
use cap_std::fs::Dir;

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
