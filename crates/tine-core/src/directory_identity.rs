//! Filesystem identity of a directory: which physical directory an open `Dir`
//! is, independent of the path it was opened through — and which absolute path
//! names a graph root, on filesystems whose driver can answer that at all.
//!
//! The restore-recovery barrier cache in `src-tauri/src/backup.rs` keys on the
//! handle identity so a directory renamed or replaced under a running restore
//! is not mistaken for the one whose durability barrier was already proven.
//! Extracted from the retired oplog object store (Managed Storage removal,
//! 2026-09-15); nothing about it was oplog-specific.

use cap_std::fs::Dir;
use std::io;
use std::path::{Path, PathBuf};

/// Resolve `path` to the absolute directory it names, preferring the
/// filesystem's own canonical answer.
///
/// **Why a fallback exists** (GH #561). `std::fs::canonicalize` is
/// `GetFinalPathNameByHandle` on Windows, and a user-space volume driver is
/// allowed not to implement it: Cryptomator and VeraCrypt (Dokany/WinFsp)
/// answer `ERROR_UNRECOGNIZED_VOLUME` (os error 1005) for every path on the
/// mounted drive. Tine took that as "this path cannot be resolved" and refused
/// to create or open any graph on an encrypted volume, while creating the
/// folders first — so the failure looked like a half-made graph rather than an
/// unsupported call.
///
/// Canonicalization is a convenience here, not a safety boundary: the graph
/// tree is opened no-follow through `cap_std` afterwards, and that is what
/// refuses symlinks and reparse points. So when the driver cannot answer, an
/// absolute spelling of a path that IS a directory is used instead. Wherever
/// canonicalize works the result is unchanged, including symlink resolution.
///
/// The fallback still proves the path EXISTS; a missing path returns the
/// filesystem's own original error rather than a fabricated one. It does not
/// judge what kind of entry it is — callers that require a directory keep
/// their own check and their own message.
pub fn canonical_existing_path(path: &Path) -> io::Result<PathBuf> {
    let unresolved = match std::fs::canonicalize(path) {
        Ok(resolved) => return Ok(resolved),
        Err(error) => error,
    };
    absolute_existing_path(path).ok_or(unresolved)
}

/// The fallback arm alone: an absolute spelling of `path`, but only once it is
/// known to name something that exists.
fn absolute_existing_path(path: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(path).ok()?;
    std::fs::metadata(&absolute).is_ok().then_some(absolute)
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DirectoryIdentity {
    volume: u64,
    file_id: [u8; 16],
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DirectoryIdentity;

/// Read the identity of `dir` from the handle itself. Every shipped Tine
/// target (Linux, macOS, iOS, Android, Windows) is `unix` or `windows`; the
/// third arm is a genuine non-Tine platform, not a forgotten one.
#[cfg(unix)]
pub fn directory_identity(dir: &Dir) -> io::Result<DirectoryIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = dir.try_clone()?.into_std_file().metadata()?;
    Ok(DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
pub fn directory_identity(dir: &Dir) -> io::Result<DirectoryIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let file = dir.try_clone()?.into_std_file();
    let mut information: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut information as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(DirectoryIdentity {
        volume: information.VolumeSerialNumber,
        file_id: information.FileId.Identifier,
    })
}

#[cfg(not(any(unix, windows)))]
pub fn directory_identity(_dir: &Dir) -> io::Result<DirectoryIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "directory identity is unavailable on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_std::ambient_authority;

    #[test]
    fn the_same_directory_has_one_identity_and_a_sibling_another() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("a")).unwrap();
        std::fs::create_dir(root.path().join("b")).unwrap();
        let a1 = Dir::open_ambient_dir(root.path().join("a"), ambient_authority()).unwrap();
        let a2 = Dir::open_ambient_dir(root.path().join("a"), ambient_authority()).unwrap();
        let b = Dir::open_ambient_dir(root.path().join("b"), ambient_authority()).unwrap();
        assert_eq!(
            directory_identity(&a1).unwrap(),
            directory_identity(&a2).unwrap()
        );
        assert_ne!(
            directory_identity(&a1).unwrap(),
            directory_identity(&b).unwrap()
        );
    }

    #[test]
    fn a_resolvable_path_keeps_the_filesystems_own_canonical_answer() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("graph")).unwrap();
        let target = root.path().join("graph");

        assert_eq!(
            canonical_existing_path(&target).unwrap(),
            std::fs::canonicalize(&target).unwrap()
        );
    }

    #[test]
    fn a_missing_path_keeps_the_filesystems_own_error() {
        let root = tempfile::tempdir().unwrap();

        let error = canonical_existing_path(&root.path().join("absent")).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
    }

    /// The arm that runs when the volume driver cannot answer
    /// `GetFinalPathNameByHandle` (GH #561: Cryptomator/VeraCrypt on Windows).
    /// It is exercised directly because no Linux filesystem refuses the call.
    #[test]
    fn the_fallback_absolutises_what_exists_and_refuses_what_does_not() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("graph")).unwrap();
        let unnormalised = root.path().join("graph").join(".");

        let resolved = absolute_existing_path(&unnormalised).expect("an existing directory");

        assert!(resolved.is_absolute(), "{}", resolved.display());
        assert!(resolved.metadata().unwrap().is_dir());
        assert!(
            !resolved
                .components()
                .any(|component| component.as_os_str() == "."),
            "the fallback normalises the spelling it returns: {}",
            resolved.display()
        );
        assert_eq!(absolute_existing_path(&root.path().join("absent")), None);
    }
}
