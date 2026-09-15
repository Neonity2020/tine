//! Durable file primitives: the no-replace move, directory fsync barriers,
//! atomic replace with retired-file restore, and atomic publish and write.

use super::*;

/// Atomically move one file without ever replacing an existing destination.
/// Platform-native no-replace rename semantics ensure the source name and inode
/// cannot be swapped between a check and an unlink.
pub(crate) fn move_file_noreplace(src: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let src = std::ffi::CString::new(src.as_os_str().as_bytes())?;
        let dest = std::ffi::CString::new(dest.as_os_str().as_bytes())?;
        // Atomic move + create-if-absent. Call the syscall directly: Android's
        // bionic `renameat2` wrapper is only exported from API 30, whereas
        // `syscall` is available from API 1. A wrapper reference here survived
        // the first GH #192 fix in backup.rs and still prevented the complete
        // native library from loading on Android 9. Whichever inode currently
        // owns `src` at the syscall boundary is moved intact, so the safety and
        // errno contracts remain unchanged.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                src.as_ptr(),
                libc::AT_FDCWD,
                dest.as_ptr(),
                libc::RENAME_NOREPLACE as libc::c_uint,
            )
        };
        return (result == 0)
            .then_some(())
            .ok_or_else(io::Error::last_os_error);
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let src = std::ffi::CString::new(src.as_os_str().as_bytes())?;
        let dest = std::ffi::CString::new(dest.as_os_str().as_bytes())?;
        let result = unsafe { libc::renamex_np(src.as_ptr(), dest.as_ptr(), libc::RENAME_EXCL) };
        return (result == 0)
            .then_some(())
            .ok_or_else(io::Error::last_os_error);
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut src: Vec<u16> = src.as_os_str().encode_wide().collect();
        let mut dest: Vec<u16> = dest.as_os_str().encode_wide().collect();
        src.push(0);
        dest.push(0);
        // MoveFileW fails when the destination already exists (unlike Rust's
        // cross-platform `rename` contract, which permits replacement).
        let result = unsafe {
            windows_sys::Win32::Storage::FileSystem::MoveFileW(src.as_ptr(), dest.as_ptr())
        };
        return (result != 0)
            .then_some(())
            .ok_or_else(io::Error::last_os_error);
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    )))]
    {
        let _ = (src, dest);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic no-replace move is unavailable on this platform",
        ))
    }
}

/// Atomically publish a newly-created file without clobbering a destination that
/// appeared after the caller's collision check. The payload is fsynced in a
/// same-directory temp, then atomically renamed into the final name only if absent.
pub(crate) fn atomic_write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_publish(path, bytes, PublishMode::NoReplace)
}

/// Suffix marking a file retired by [`atomic_replace_expected`] mid-publish.
pub(crate) const RETIRED_SUFFIX: &str = ".retired";

/// True for dir-fsync errors that mean "this filesystem does not offer it",
/// as opposed to a real durability failure.
///
/// Directory fsync is genuinely unavailable in several places: Windows has no
/// handle you can open this way, and some network filesystems reject it. Those
/// must stay non-fatal — that is why the call was best-effort to begin with.
/// A real `EIO`/`ENOSPC`, though, means the rename may not survive a crash, and
/// reporting durable success there is a false ack.
pub(crate) fn dir_fsync_is_unsupported(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::Unsupported
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::NotFound
    ) {
        return true;
    }
    // EBADF / EACCES / EISDIR / EINVAL from an fsync on a directory handle: the
    // filesystem (several NFS and FUSE implementations) is telling us the
    // operation does not apply, not that data was lost.
    const UNSUPPORTED_ERRNOS: [i32; 4] = [9, 13, 21, 22];
    error
        .raw_os_error()
        .is_some_and(|errno| UNSUPPORTED_ERRNOS.contains(&errno))
}

/// App-layer face of [`sync_dir`]: fsync `dir` so a rename into it survives a
/// crash. For src-tauri writers (settings registry, backup restore, window
/// identity) that previously discarded this result with `let _ = …` — a false
/// ack under the in-scope crash/power-loss threat (DUP-5).
pub fn sync_dir_for_rename(dir: &Path) -> io::Result<()> {
    sync_dir(dir)
}

/// App-layer face of [`dir_fsync_is_unsupported`], for writers that hold their
/// own directory handle (the cap-std restore path) and must apply the same
/// tolerate-unsupported / report-real policy.
pub fn dir_fsync_error_is_unsupported(error: &io::Error) -> bool {
    dir_fsync_is_unsupported(error)
}

/// `fsync` one regular file and record the durability barrier.
///
/// Every production file barrier in this module goes through here so the
/// per-operation barrier count is a measurable, testable number rather than an
/// invisible sum spread across modules (2026-08-26 cost-model audit, D1).
#[inline]
pub(crate) fn barrier_sync_all(
    file: &impl crate::durability_counters::DurableHandle,
) -> io::Result<()> {
    crate::durability_counters::sync_file(file)
}

/// `fsync` one already-opened directory handle and record the barrier.
#[inline]
fn barrier_sync_dir_handle(handle: &fs::File) -> io::Result<()> {
    crate::durability_counters::sync_directory(handle)
}

/// fsync a directory so a rename into it survives a crash.
///
/// Errors that mean "unsupported here" are swallowed; everything else is
/// propagated, because a caller told the durability succeeded when it did not
/// will happily report a save as committed.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    match fs::File::open(dir).and_then(|handle| barrier_sync_dir_handle(&handle)) {
        Ok(()) => Ok(()),
        Err(error) if dir_fsync_is_unsupported(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Outcome of a conditional publish.
#[derive(Debug)]
pub(crate) enum AtomicReplaceOutcome {
    /// `next` is now the file's content.
    Published,
    /// Someone else wrote the file first; nothing was published and their
    /// bytes stay in place.
    ExternalChanged,
}

/// Publish `next` to `path` ONLY if `path` still holds `expected`.
///
/// A plain temp+rename publish silently clobbers whatever arrived after the
/// caller's last read: Syncthing delivering a peer's `config.edn`, Logseq
/// writing the same file, an external editor saving a sidecar. Those bytes
/// vanish with no conflict copy and no refusal, which is exactly the class
/// ADR 0007 forbids for pages.
///
/// Checking the file and then renaming over it cannot fix that — the check and
/// the rename are two operations and the writer lands between them. So the
/// capture IS the rename: `path` is renamed aside to a unique sibling first,
/// which atomically takes whatever was current, and the comparison happens on
/// a name nobody else is writing.
///
/// The threat model is honest concurrent writers (crash/power loss, sync
/// delivery, external editors, a second instance), not an attacker forging
/// bytes with local write access — see the 2026-08-07 trust decision.
pub(crate) fn atomic_replace_expected(
    path: &Path,
    expected: &[u8],
    next: &[u8],
) -> io::Result<AtomicReplaceOutcome> {
    atomic_replace_expected_with_hooks(path, expected, next, || Ok(()))
}

pub(crate) fn atomic_replace_expected_with_hooks(
    path: &Path,
    expected: &[u8],
    next: &[u8],
    after_retire: impl Fn() -> io::Result<()>,
) -> io::Result<AtomicReplaceOutcome> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let pid = std::process::id();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{fname}.{pid}.{seq}.publish.tmp"));
    let retired = dir.join(format!(".{fname}.{pid}.{seq}{RETIRED_SUFFIX}"));

    let staged = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(next)?;
        barrier_sync_all(&file)?;
        Ok(())
    })();
    if let Err(error) = staged {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }

    // RETIRE: atomically capture whatever `path` currently holds. A no-replace
    // rename would be wrong here - we intend to vacate the slot.
    if let Err(error) = fs::rename(path, &retired) {
        let _ = fs::remove_file(&tmp);
        if error.kind() == io::ErrorKind::NotFound {
            // The file we meant to update is gone: an external delete. Not ours
            // to recreate silently.
            return Ok(AtomicReplaceOutcome::ExternalChanged);
        }
        return Err(error);
    }

    // A crash between here and the publish leaves the content in `retired`;
    // `restore_retired_files` puts it back on next open.
    if let Err(error) = after_retire() {
        let _ = move_file_noreplace(&retired, path);
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }

    let found = match fs::read(&retired) {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = move_file_noreplace(&retired, path);
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
    };
    if found != expected {
        // Someone wrote between the caller's read and now. Put their bytes back
        // and publish nothing. If an even newer external CREATE took the name,
        // `retired` stays for the recovery sweep rather than deleting anyone's
        // data.
        let _ = move_file_noreplace(&retired, path);
        let _ = fs::remove_file(&tmp);
        return Ok(AtomicReplaceOutcome::ExternalChanged);
    }

    // PUBLISH into the slot we vacated. No-replace: an AlreadyExists here means
    // an external CREATE won the window, and its bytes are not ours to replace.
    if let Err(error) = move_file_noreplace(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        if error.kind() == io::ErrorKind::AlreadyExists {
            // Our retired bytes stay on disk for the sweep to triage.
            return Ok(AtomicReplaceOutcome::ExternalChanged);
        }
        let _ = move_file_noreplace(&retired, path);
        return Err(error);
    }

    sync_dir(dir)?;
    let _ = fs::remove_file(&retired);
    Ok(AtomicReplaceOutcome::Published)
}

/// Atomic write: write to a temp file in the same directory, then rename. The
/// temp name is unique per write (pid + sequence) so two concurrent writers to
/// the same path (e.g. an autosave and a highlight/rename rewrite) can't truncate
/// each other's temp; the rename is still atomic. The temp is removed if the
/// write fails, so a unique name never leaks an orphan behind.
/// How [`atomic_publish`] lands the temp on its final name.
enum PublishMode {
    /// `fs::rename` — replaces an existing file (the ordinary save shape).
    Replace,
    /// `move_file_noreplace` — create-only; never clobbers a concurrent creator.
    NoReplace,
}

/// THE temp+fsync+rename publish implementation, shared by [`atomic_write`]
/// and [`atomic_write_new`] (DUP-5, 2026-08-25 duplication audit: the family
/// had drifted into copies with different failure policies; the rationale
/// lives here once).
///
/// - The temp name is unique per write (pid + per-process sequence) so two
///   concurrent writers to the same path can't truncate each other's temp.
/// - The temp is hidden (`.`-prefixed) and ends in `.tmp` — the shape the
///   watcher's `is_tine_atomic_page_temp_path` and the wire-side recognizer
///   understand; change it only together with both recognizers.
/// - Directory-fsync errors that mean "unsupported here" are tolerated; a real
///   `EIO`/`ENOSPC` is REPORTED, because a caller told this succeeded will
///   report the save as durably committed (in-scope threat: crash/power loss
///   right after the rename).
fn atomic_publish(path: &Path, bytes: &[u8], mode: PublishMode) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("page");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let infix = match mode {
        PublishMode::Replace => "",
        PublishMode::NoReplace => ".new",
    };
    let tmp = dir.join(format!(".{fname}.{}.{seq}{infix}.tmp", std::process::id()));
    let res = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        barrier_sync_all(&f)?;
        drop(f);
        match mode {
            PublishMode::Replace => fs::rename(&tmp, path),
            PublishMode::NoReplace => move_file_noreplace(&tmp, path),
        }
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp); // never leave a temp behind on failure
        return res;
    }
    sync_dir(dir)
}

/// Atomically replace a small user-selected output and durably publish its
/// directory entry. This does not provide graph mutation conflict semantics;
/// callers remain responsible for choosing an appropriate destination.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    atomic_publish(path, bytes, PublishMode::Replace)
}
