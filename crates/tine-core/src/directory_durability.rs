//! Shared directory-entry durability behavior for graph projections and oplog
//! publication.
//!
//! Regular files are flushed by their callers before an atomic name operation.
//! On Windows we clone and validate the retained directory capability, then use
//! `ReOpenFile` to obtain write access to that exact object without another
//! pathname lookup before attempting `FlushFileBuffers`. Windows may return
//! `ERROR_INVALID_PARAMETER` for that validated directory flush: only that
//! deterministic platform limitation is accepted. The file bytes remain flushed
//! and the name operation remains atomic, but Windows has no portable
//! directory-entry fsync, so this fallback does not promise the name survives a
//! crash. Every clone, reopen, metadata, validation, and unrelated flush error
//! remains fatal.

use cap_std::fs::Dir;
use std::fs;
use std::io;

#[cfg(any(test, windows))]
use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;

#[cfg(windows)]
pub(crate) struct ValidatedDirectorySync {
    // Retain the validated capability handle alongside the write-capable
    // ReOpenFile handle. Both refer to the same filesystem object, and neither
    // post-publication sync performs a pathname lookup.
    _capability: fs::File,
    flush_handle: fs::File,
}

#[cfg(not(windows))]
pub(crate) struct ValidatedDirectorySync<'a>(&'a Dir);

#[cfg(any(test, windows))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowsDirectorySyncStage {
    CapabilityClone,
    Metadata,
    TargetValidation,
    NoFollowReopen,
    Flush,
}

#[cfg(windows)]
impl ValidatedDirectorySync {
    pub(crate) fn open(dir: &Dir) -> io::Result<Self> {
        use std::os::windows::fs::MetadataExt as _;
        use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
        use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            ReOpenFile, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        };

        let capability = dir.try_clone()?.into_std_file();
        let metadata = capability.metadata()?;
        validate_windows_directory_target(
            metadata.is_dir(),
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0,
        )?;

        // ReOpenFile opens the filesystem object named by an existing handle,
        // rather than resolving another path. This both preserves capability
        // identity and makes the reopen intrinsically no-follow. Request the
        // write access required by FlushFileBuffers and keep cap-std's
        // directory sharing policy, which intentionally excludes delete share.
        let reopened = unsafe {
            ReOpenFile(
                capability.as_raw_handle(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        };
        let flush_handle = if reopened == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        } else {
            // SAFETY: ReOpenFile returned one newly owned handle.
            unsafe { fs::File::from_raw_handle(reopened) }
        };
        let metadata = flush_handle.metadata()?;
        validate_windows_directory_target(
            metadata.is_dir(),
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0,
        )?;

        Ok(Self {
            _capability: capability,
            flush_handle,
        })
    }

    pub(crate) fn preflight(&self) -> io::Result<()> {
        self.sync()
    }

    fn flush_result(&self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;

        // SAFETY: the handle remains owned by `self` for the call. `open`
        // obtained it from the retained directory object with GENERIC_WRITE,
        // which FlushFileBuffers requires, and then proved that both handles
        // identify real directories rather than reparse points.
        if unsafe { FlushFileBuffers(self.flush_handle.as_raw_handle()) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        match self.flush_result() {
            Ok(()) => Ok(()),
            Err(error) => {
                validated_windows_directory_stage_error(WindowsDirectorySyncStage::Flush, error)
            }
        }
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
fn validated_windows_directory_stage_error(
    stage: WindowsDirectorySyncStage,
    error: io::Error,
) -> io::Result<()> {
    if stage == WindowsDirectorySyncStage::Flush
        && error.raw_os_error() == Some(ERROR_INVALID_PARAMETER.cast_signed())
    {
        // FlushFileBuffers has no portable directory-entry meaning on this
        // Windows target/filesystem. Every operation that established the
        // exact, real directory handle has already succeeded.
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        validate_windows_directory_target, validated_windows_directory_stage_error,
        WindowsDirectorySyncStage,
    };
    use std::io;
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};

    #[test]
    fn validated_real_directory_invalid_parameter_is_the_narrow_windows_fallback() {
        validate_windows_directory_target(true, false).unwrap();
        validated_windows_directory_stage_error(
            WindowsDirectorySyncStage::Flush,
            io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER.cast_signed()),
        )
        .unwrap();
    }

    #[test]
    fn another_windows_directory_flush_error_remains_fatal() {
        let error = validated_windows_directory_stage_error(
            WindowsDirectorySyncStage::Flush,
            io::Error::from_raw_os_error(ERROR_ACCESS_DENIED.cast_signed()),
        )
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

    #[test]
    fn invalid_parameter_before_windows_directory_flush_remains_fatal() {
        for stage in [
            WindowsDirectorySyncStage::CapabilityClone,
            WindowsDirectorySyncStage::Metadata,
            WindowsDirectorySyncStage::TargetValidation,
            WindowsDirectorySyncStage::NoFollowReopen,
        ] {
            let error = validated_windows_directory_stage_error(
                stage,
                io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER.cast_signed()),
            )
            .unwrap_err();
            assert_eq!(
                error.raw_os_error(),
                Some(ERROR_INVALID_PARAMETER.cast_signed()),
                "{stage:?}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_retained_directory_handle_reopens_validates_and_reaches_flush() {
        use super::ValidatedDirectorySync;
        use cap_std::{ambient_authority, fs::Dir};
        use std::fs;
        use uuid::Uuid;

        let path =
            std::env::temp_dir().join(format!("tine-directory-durability-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        let dir = Dir::open_ambient_dir(&path, ambient_authority()).unwrap();
        let sync = ValidatedDirectorySync::open(&dir).unwrap();
        let raw_flush = sync.flush_result();
        if let Err(error) = &raw_flush {
            assert_eq!(
                error.raw_os_error(),
                Some(ERROR_INVALID_PARAMETER.cast_signed())
            );
        }
        match raw_flush {
            Ok(()) => Ok(()),
            Err(error) => {
                validated_windows_directory_stage_error(WindowsDirectorySyncStage::Flush, error)
            }
        }
        .unwrap();
        drop(sync);
        drop(dir);
        fs::remove_dir(path).unwrap();
    }
}
