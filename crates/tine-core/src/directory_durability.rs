//! Shared directory-entry durability behavior for graph projections and oplog
//! publication.
//!
//! Regular files are flushed by their callers before an atomic name operation.
//! On Windows we additionally open and validate a write-capable, no-follow
//! directory handle before attempting `FlushFileBuffers`. Hosted NTFS returns
//! `ERROR_INVALID_PARAMETER` for that validated directory operation: only that
//! deterministic platform limitation is accepted. The file bytes remain
//! flushed and the name operation remains atomic, but Windows has no portable
//! directory-entry fsync, so this fallback does not promise the name survives a
//! crash. Every other open, validation, or flush error remains fatal.

use cap_std::fs::Dir;
use std::fs;
use std::io;

#[cfg(any(test, windows))]
use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;

#[cfg(windows)]
pub(crate) struct ValidatedDirectorySync(fs::File);

#[cfg(not(windows))]
pub(crate) struct ValidatedDirectorySync<'a>(&'a Dir);

#[cfg(windows)]
impl ValidatedDirectorySync {
    pub(crate) fn open(dir: &Dir) -> io::Result<Self> {
        use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _, OpenOptionsMaybeDirExt as _};
        use cap_std::fs::OpenOptions;
        use std::os::windows::fs::MetadataExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .follow(FollowSymlinks::No)
            .maybe_dir(true);
        let file = dir.open_with(".", &options)?.into_std();
        let metadata = file.metadata()?;
        validate_windows_directory_target(
            metadata.is_dir(),
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0,
        )?;
        Ok(Self(file))
    }

    pub(crate) fn preflight(&self) -> io::Result<()> {
        self.sync()
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;

        // SAFETY: the handle remains owned by `self` for the call. `open`
        // requested GENERIC_WRITE, which FlushFileBuffers requires, together
        // with directory and no-follow semantics, and then proved that the
        // opened handle is a real directory rather than a reparse point.
        let result = if unsafe { FlushFileBuffers(self.0.as_raw_handle()) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
        validated_windows_directory_flush_result(result)
    }
}

#[cfg(unix)]
impl<'a> ValidatedDirectorySync<'a> {
    pub(crate) fn open(dir: &'a Dir) -> io::Result<Self> {
        Ok(Self(dir))
    }

    pub(crate) fn preflight(&self) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _};

        // cap-std may retain an O_PATH capability, which is suitable for openat
        // but cannot itself be fsynced. Open `.` as a real directory descriptor.
        let fd = unsafe {
            libc::openat(
                self.0.as_fd().as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned one newly owned directory descriptor.
        unsafe { fs::File::from_raw_fd(fd) }.sync_all()
    }
}

#[cfg(not(any(unix, windows)))]
impl<'a> ValidatedDirectorySync<'a> {
    pub(crate) fn open(dir: &'a Dir) -> io::Result<Self> {
        Ok(Self(dir))
    }

    pub(crate) fn preflight(&self) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory durability is unsupported on this target",
        ))
    }
}

pub(crate) fn sync_dir_required(dir: &Dir) -> io::Result<()> {
    ValidatedDirectorySync::open(dir)?.sync()
}

#[cfg(any(test, windows))]
fn validate_windows_directory_target(is_dir: bool, is_reparse: bool) -> io::Result<()> {
    if !is_dir || is_reparse {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory durability handle is not a real no-follow directory",
        ));
    }
    Ok(())
}

#[cfg(any(test, windows))]
fn validated_windows_directory_flush_result(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER.cast_signed()) => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_windows_directory_target, validated_windows_directory_flush_result};
    use std::io;
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};

    #[test]
    fn validated_real_directory_invalid_parameter_is_the_narrow_windows_fallback() {
        validate_windows_directory_target(true, false).unwrap();
        validated_windows_directory_flush_result(Err(io::Error::from_raw_os_error(
            ERROR_INVALID_PARAMETER.cast_signed(),
        )))
        .unwrap();
    }

    #[test]
    fn another_windows_directory_flush_error_remains_fatal() {
        let error = validated_windows_directory_flush_result(Err(io::Error::from_raw_os_error(
            ERROR_ACCESS_DENIED.cast_signed(),
        )))
        .unwrap_err();
        assert_eq!(
            error.raw_os_error(),
            Some(ERROR_ACCESS_DENIED.cast_signed())
        );
    }

    #[test]
    fn windows_directory_validation_rejects_reparse_and_non_directory_handles() {
        assert_eq!(
            validate_windows_directory_target(false, false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            validate_windows_directory_target(true, true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
