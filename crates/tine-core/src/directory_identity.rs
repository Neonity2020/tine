//! Filesystem identity of an open directory handle: which physical directory
//! a `Dir` is, independent of the path it was opened through.
//!
//! The restore-recovery barrier cache in `src-tauri/src/backup.rs` keys on it
//! so a directory renamed or replaced under a running restore is not mistaken
//! for the one whose durability barrier was already proven. Extracted from the
//! retired oplog object store (Managed Storage removal, 2026-09-15); nothing
//! about it was oplog-specific.

use cap_std::fs::Dir;
use std::io;

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
}
