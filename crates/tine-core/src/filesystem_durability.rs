//! Platform durability helpers shared by managed-storage bootstrap paths.
//!
//! Linux can cheaply flush the filesystem containing a complete private tree.
//! Android exposes the same syscall, but some kernels and ROMs deny that
//! filesystem-wide operation even though ordinary app-private file and
//! directory synchronization works. In that case we synchronize the exact
//! private tree instead of refusing managed-storage activation.

use std::fs;
#[cfg(any(test, not(target_os = "linux")))]
use std::fs::OpenOptions;
use std::io;
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::os::fd::{AsFd as _, AsRawFd as _};
use std::path::Path;

#[cfg(not(target_os = "linux"))]
use cap_std::{ambient_authority, fs::Dir};

pub(crate) fn sync_private_tree(path: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        return sync_filesystem(path);
    }

    #[cfg(target_os = "android")]
    {
        match sync_filesystem(path) {
            Ok(()) => return Ok(()),
            Err(error) if android_filesystem_sync_may_fallback(error.kind()) => {}
            Err(error) => return Err(error),
        }
    }

    #[cfg(not(target_os = "linux"))]
    sync_private_tree_exact(path)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn sync_filesystem(path: &Path) -> io::Result<()> {
    let directory = fs::File::open(path)?;
    // SAFETY: the opened descriptor names the filesystem containing the
    // complete private tree. Android may reject this filesystem-wide operation
    // and then takes the exact-tree path below.
    let result = unsafe { libc::syncfs(directory.as_fd().as_raw_fd()) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(any(test, target_os = "android"))]
const fn android_filesystem_sync_may_fallback(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput
    )
}

#[cfg(not(target_os = "linux"))]
fn sync_private_tree_exact(path: &Path) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "private durability tree contains a symlink",
            ));
        }
        if metadata.is_dir() {
            sync_private_tree_exact(&child)?;
        } else if metadata.is_file() {
            sync_regular_file(&child)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "private durability tree contains a non-file entry",
            ));
        }
    }
    let directory = Dir::open_ambient_dir(path, ambient_authority())?;
    tine_storage::sync_dir_required(&directory)
}

#[cfg(any(test, not(target_os = "linux")))]
pub(crate) fn sync_regular_file(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true);
    // FlushFileBuffers requires a write-capable handle on Windows.
    #[cfg(windows)]
    options.write(true);
    options.open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_falls_back_only_when_the_filesystem_wide_primitive_is_unavailable() {
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Unsupported,
            io::ErrorKind::InvalidInput,
        ] {
            assert!(android_filesystem_sync_may_fallback(kind));
        }
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::Interrupted,
            io::ErrorKind::InvalidData,
            io::ErrorKind::WriteZero,
            io::ErrorKind::Other,
        ] {
            assert!(!android_filesystem_sync_may_fallback(kind));
        }
    }
}
