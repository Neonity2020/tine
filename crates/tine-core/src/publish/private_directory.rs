//! Windows publication scratch directories must not inherit a permissive TEMP
//! ACL. OW is the current object's owner, not an assumption about its parent:
//! https://learn.microsoft.com/en-us/windows/win32/secauthz/sid-strings

use std::{ffi::c_void, io, os::windows::ffi::OsStrExt, path::Path, ptr};
use windows_sys::Win32::{
    Foundation::{LocalFree, ERROR_INSUFFICIENT_BUFFER},
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        GetAce, GetFileSecurityW, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        IsWellKnownSid, WinCreatorOwnerRightsSid, ACCESS_ALLOWED_ACE, ACE_HEADER,
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, SECURITY_ATTRIBUTES,
        SE_DACL_PROTECTED,
    },
    Storage::FileSystem::{CreateDirectoryW, FILE_ALL_ACCESS},
    System::SystemServices::ACCESS_ALLOWED_ACE_TYPE,
};

const OWNER_DACL: &str = "D:P(A;OICI;FA;;;OW)";

struct LocalDescriptor(*mut c_void);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        // SAFETY: this allocation came from ConvertStringSecurityDescriptor...
        // and has not been transferred or freed elsewhere.
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory path contains a null",
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn create_with_dacl(path: &Path, dacl: &str) -> io::Result<()> {
    let path = wide_path(path)?;
    let dacl: Vec<u16> = dacl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = ptr::null_mut();
    // SAFETY: both strings are terminated, output storage is valid, and the
    // descriptor remains alive through CreateDirectoryW's synchronous call.
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            dacl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let descriptor = LocalDescriptor(descriptor);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        if CreateDirectoryW(path.as_ptr(), &attributes) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Check the actual filesystem result, including filesystems which cannot
/// enforce Windows ACLs. This runs while the exclusively created root is empty.
fn validate_owner_dacl(path: &Path) -> io::Result<()> {
    let path = wide_path(path)?;
    let denied = || {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "publication directory is not owner-private",
        )
    };
    let mut required = 0;
    // SAFETY: Win32 supplies a validated self-relative security descriptor.
    // u32 storage provides its required alignment; ACL/ACE pointers borrow that
    // buffer and are only inspected before it drops.
    unsafe {
        if GetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            0,
            &mut required,
        ) != 0
        {
            return Err(denied());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) || required == 0 {
            return Err(error);
        }
        let mut storage = vec![0u32; (required as usize).div_ceil(4)];
        let descriptor = storage.as_mut_ptr().cast();
        if GetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION,
            descriptor,
            required,
            &mut required,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let (mut control, mut revision) = (0, 0);
        if GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) == 0 {
            return Err(io::Error::last_os_error());
        }
        let (mut present, mut defaulted) = (0, 0);
        let mut acl = ptr::null_mut();
        if GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted) == 0 {
            return Err(io::Error::last_os_error());
        }
        if control & SE_DACL_PROTECTED == 0 || present == 0 || acl.is_null() || (*acl).AceCount != 1
        {
            return Err(denied());
        }
        let mut raw = ptr::null_mut();
        if GetAce(acl, 0, &mut raw) == 0 {
            return Err(io::Error::last_os_error());
        }
        let header = &*raw.cast::<ACE_HEADER>();
        if header.AceType as u32 != ACCESS_ALLOWED_ACE_TYPE
            || (header.AceSize as usize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
            || header.AceFlags as u32 != (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE)
        {
            return Err(denied());
        }
        let ace = &*raw.cast::<ACCESS_ALLOWED_ACE>();
        let sid = ptr::addr_of!(ace.SidStart).cast_mut().cast();
        if ace.Mask != FILE_ALL_ACCESS || IsWellKnownSid(sid, WinCreatorOwnerRightsSid) == 0 {
            return Err(denied());
        }
    }
    Ok(())
}

pub(super) fn create(path: &Path) -> io::Result<()> {
    create_with_dacl(path, OWNER_DACL)?;
    if let Err(error) = validate_owner_dacl(path) {
        // Nothing has been written yet; do not return a path whose privacy
        // could not be proved. Never remove a pre-existing directory.
        let _ = std::fs::remove_dir(path);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_child_rejects_parent_permissions_and_preserves_existing_paths() {
        let parent =
            std::env::temp_dir().join(format!("tine-private-acl-{}", uuid::Uuid::new_v4()));
        create_with_dacl(&parent, "D:P(A;OICI;FA;;;WD)").unwrap();
        assert!(validate_owner_dacl(&parent).is_err());
        let child = parent.join("private");
        create(&child).unwrap();
        validate_owner_dacl(&child).unwrap();
        std::fs::write(child.join("witness"), b"private").unwrap();
        assert_eq!(
            create(&child).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(child.join("witness")).unwrap(), b"private");
        std::fs::remove_dir_all(parent).unwrap();
    }
}
