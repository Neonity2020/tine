use std::fs;
use std::path::Path;

#[track_caller]
pub(crate) fn remove_dir_all(path: impl AsRef<Path>) {
    let path = path.as_ref();
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        #[cfg(windows)]
        Err(error)
            if error.raw_os_error()
                == Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32) => {}
        Err(error) => panic!(
            "failed to remove test directory {}: {error}",
            path.display()
        ),
    }
}
