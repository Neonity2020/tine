//! Staging and renaming inside the projection: temp and staging files, editor
//! staged recovery, the per-platform no-replace rename, and graph-text exact
//! moves.

use super::*;

pub(super) fn create_projection_temp(
    dir: &Dir,
    filename: &str,
    bytes: &[u8],
) -> io::Result<String> {
    create_projection_staging_file(dir, filename, bytes, "projection.tmp")
}

pub(super) fn create_editor_staged_recovery(
    dir: &Dir,
    filename: &str,
    bytes: &[u8],
    turn_short_id: Option<[u8; 4]>,
) -> io::Result<String> {
    let Some(turn_short_id) = turn_short_id else {
        return create_projection_staging_file(dir, filename, bytes, "editor-staged-recovery");
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    static STAGED_SEQ: AtomicU64 = AtomicU64::new(0);
    for _ in 0..128 {
        let name = format!(
            ".{filename}.{}.{}.{}.editor-staged-recovery",
            std::process::id(),
            STAGED_SEQ.fetch_add(1, Ordering::Relaxed),
            short_turn_id(turn_short_id),
        );
        let mut options = CapOpenOptions::new();
        options.write(true).create_new(true);
        match dir.open_with(&name, &options) {
            Ok(mut file) => {
                let result = file.write_all(bytes).and_then(|()| barrier_sync_all(&file));
                drop(file);
                if let Err(error) = result {
                    let _ = dir.remove_file(&name);
                    return Err(error);
                }
                return Ok(name);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve editor staged-recovery file",
    ))
}

pub(super) fn short_turn_id(bytes: [u8; 4]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    )
}

/// Parse only the complete filenames emitted by the editor publication
/// protocol. The two numeric fields are part of the authority: a user file
/// that merely ends in `editor-recovery` is not a cleanup candidate.
pub(super) fn editor_recovery_target_name(name: &str) -> Option<&str> {
    fn parse_legacy(candidate: &str) -> Option<&str> {
        let (candidate, sequence) = candidate.rsplit_once('.')?;
        let (target, process) = candidate.rsplit_once('.')?;
        (!target.is_empty()
            && !sequence.is_empty()
            && sequence.bytes().all(|byte| byte.is_ascii_digit())
            && !process.is_empty()
            && process.bytes().all(|byte| byte.is_ascii_digit())
            && text_extension_from_path(Path::new(target)).is_some())
        .then_some(target)
    }

    let rest = name.strip_prefix('.')?;
    let rest = rest
        .strip_suffix(".editor-staged-recovery")
        .or_else(|| rest.strip_suffix(".editor-recovery"))?;
    if let Some((legacy_shape, turn)) = rest.rsplit_once('.') {
        if turn.len() == 8 && turn.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            if let Some(target) = parse_legacy(legacy_shape) {
                return Some(target);
            }
        }
    }
    parse_legacy(rest)
}

/// Parse the cleanup-only name emitted after a Direct Files replacement has
/// already published and validated its new live file. Unlike
/// [`editor_recovery_target_name`], this grammar never confers restoration or
/// conflict authority; checked open may only remove the exact producer shape.
pub(super) fn editor_retired_target_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix('.')?.strip_suffix(".editor-retired")?;
    let (candidate, sequence) = rest.rsplit_once('.')?;
    let (target, process) = candidate.rsplit_once('.')?;
    (!target.is_empty()
        && !sequence.is_empty()
        && sequence.bytes().all(|byte| byte.is_ascii_digit())
        && !process.is_empty()
        && process.bytes().all(|byte| byte.is_ascii_digit())
        && text_extension_from_path(Path::new(target)).is_some())
    .then_some(target)
}

fn create_projection_staging_file(
    dir: &Dir,
    filename: &str,
    bytes: &[u8],
    suffix: &str,
) -> io::Result<String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    for _ in 0..128 {
        let name = format!(
            ".{filename}.{}.{}.{suffix}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let mut options = CapOpenOptions::new();
        options.write(true).create_new(true);
        match dir.open_with(&name, &options) {
            Ok(mut file) => {
                let result = file.write_all(bytes).and_then(|()| barrier_sync_all(&file));
                drop(file);
                if let Err(error) = result {
                    let _ = dir.remove_file(&name);
                    return Err(error);
                }
                return Ok(name);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve projection temporary file",
    ))
}

/// Verify directory durability support before the first live-name mutation.
/// Unix targets probe the filesystem and fail before retirement/publication
/// when directory flushing is unavailable. Windows first validates the retained
/// exact directory capability, then records its documented lack of a
/// directory-entry flush primitive as a platform limitation.
///
/// The probe covers exactly the directory the operation will later flush — the
/// chain leaf — because that is the only barrier the operation takes. It is
/// strict on every platform: the graph tree is the sole authority for its bytes.
pub(super) fn preflight_projection_chain(chain: &[Dir]) -> io::Result<()> {
    sync_projection_chain(chain)
}

/// The exact platform primitive named by the projection receipt. It is a
/// per-target constant so the enriched failure detail keeps naming the call the
/// device actually refused.
#[cfg(any(target_os = "linux", target_os = "android"))]
const PROJECTION_NOREPLACE_RENAME_OPERATION: &str =
    "renameat2(RENAME_NOREPLACE) publishing the projection";

#[cfg(any(target_os = "macos", target_os = "ios"))]
const PROJECTION_NOREPLACE_RENAME_OPERATION: &str =
    "renameatx_np(RENAME_EXCL) publishing the projection";

#[cfg(windows)]
const PROJECTION_NOREPLACE_RENAME_OPERATION: &str =
    "FileRenameInformation(ReplaceIfExists=false) publishing the projection";

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "android",
    windows
)))]
const PROJECTION_NOREPLACE_RENAME_OPERATION: &str =
    "atomic no-clobber rename publishing the projection";

/// The raw platform no-replace rename. It returns the untouched platform error,
/// so [`rename_projection_noreplace`] names the refused call around the exact
/// `errno` rather than an `io::Error::new` that would discard it.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_projection_noreplace_platform(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd};

    let from = CString::new(from)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid temporary name"))?;
    let to = CString::new(to)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid target name"))?;
    let result = unsafe {
        // Use the syscall entry point on Android: bionic's renameat2 wrapper is
        // API-30-only, while the kernel primitive and syscall() are available
        // on the supported Android baseline. Linux uses the identical path.
        libc::syscall(
            libc::SYS_renameat2,
            dir.as_fd().as_raw_fd(),
            from.as_ptr(),
            dir.as_fd().as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE as libc::c_uint,
        )
    };
    (result == 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn rename_projection_noreplace_platform(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd};

    let from = CString::new(from)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid source name"))?;
    let to = CString::new(to)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid target name"))?;
    let result = unsafe {
        libc::renameatx_np(
            dir.as_fd().as_raw_fd(),
            from.as_ptr(),
            dir.as_fd().as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL as libc::c_uint,
        )
    };
    (result == 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(windows)]
fn rename_projection_noreplace_platform(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    rename_projection_between_noreplace(dir, from, dir, to)
}

#[cfg(windows)]
pub(super) fn rename_projection_between_noreplace(
    source_dir: &Dir,
    from: &str,
    destination_dir: &Dir,
    to: &str,
) -> io::Result<()> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
    use cap_std::fs::OpenOptionsExt as _;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::fs::MetadataExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Wdk::Storage::FileSystem::{
        FileRenameInformation, NtSetInformationFile, FILE_RENAME_INFORMATION,
    };
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    fn valid_leaf(name: &str) -> bool {
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
    }

    if !valid_leaf(from) || !valid_leaf(to) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic no-replace rename requires relative leaf names",
        ));
    }

    // Open the source itself with DELETE access through the retained directory
    // capability. FileRenameInformation then renames that exact handle relative
    // to the retained destination directory. A filesystem that cannot provide
    // the primitive rejects this call before the live source name is retired.
    let mut options = CapOpenOptions::new();
    options
        .follow(FollowSymlinks::No)
        .access_mode(DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let source = source_dir.open_with(from, &options)?.into_std();
    let metadata = source.metadata()?;
    if !metadata.is_file()
        || metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic no-replace rename source is not a regular no-follow file",
        ));
    }

    let destination = OsStr::new(to).encode_wide().collect::<Vec<_>>();
    let destination_bytes = destination
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target name too long"))?;
    let information_length = std::mem::size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(destination_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target name too long"))?;
    let information_words = information_length.div_ceil(std::mem::size_of::<usize>());
    let information_length = u32::try_from(information_length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target name too long"))?;
    let destination_bytes = u32::try_from(destination_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target name too long"))?;
    let mut storage = vec![0_usize; information_words];
    let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let root = destination_dir.try_clone()?.into_std_file();
    let mut io_status = IO_STATUS_BLOCK::default();

    // FileRenameInformation with ReplaceIfExists false atomically fails when
    // the destination name is occupied. The usize-backed allocation aligns the
    // locked binding's variable-tail structure, and both the exact source and
    // retained destination-directory handles outlive the call.
    let status = unsafe {
        (*information).Anonymous.ReplaceIfExists = false;
        (*information).RootDirectory = root.as_raw_handle();
        (*information).FileNameLength = destination_bytes;
        std::ptr::copy_nonoverlapping(
            destination.as_ptr(),
            (*information).FileName.as_mut_ptr(),
            destination.len(),
        );
        NtSetInformationFile(
            source.as_raw_handle(),
            &mut io_status,
            information.cast(),
            information_length,
            FileRenameInformation,
        )
    };
    if status >= 0 {
        Ok(())
    } else {
        let error = unsafe { RtlNtStatusToDosError(status) };
        Err(io::Error::from_raw_os_error(error as i32))
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "android",
    windows
)))]
fn rename_projection_noreplace_platform(_dir: &Dir, _from: &str, _to: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-clobber projection publication is unsupported on this platform",
    ))
}

/// The single no-clobber publication of a graph-tree name. Every caller writes
/// an artifact the graph itself is the only authority for, so the atomic
/// primitive is the contract: there is no second copy to rebuild from, and a
/// two-step publication would leave a reserved-but-empty live name behind a
/// crash. A filesystem that cannot provide the primitive fails the write, on
/// every platform (`docs/storage-sync-contract.md` §2.10b).
pub(super) fn rename_projection_noreplace(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    rename_projection_noreplace_platform(dir, from, to).map_err(|error| {
        projection_platform_error(
            PROJECTION_NOREPLACE_RENAME_OPERATION,
            &format!("{from:?} -> {to:?}"),
            error,
        )
    })
}

/// The Direct Files graph-text name transition: the exact-byte move protocol of
/// `tine_storage::DurableDirectoryPublication::move_exact_no_replace`, carried
/// by the graph tree's own no-clobber rename.
///
/// GH #466. v0.6.981 routed every Direct Files create, live-name retirement,
/// staged publication, recovery restore and recovery set-aside through the
/// storage crate's move, whose Android arm is hard-link-then-unlink — a
/// primitive the FUSE-backed shared storage a Direct Files graph lives in
/// refuses — so every Android save failed with `Permission denied (os error
/// 13)`. That crate's move is written for app-private sole-writer namespaces
/// (the storage-mode selectors, `durable_private_authority_directory`), where
/// hard links exist; the graph tree is never such a namespace. Its name
/// transitions use [`rename_projection_noreplace`], the primitive v0.6.98
/// shipped here on every target (I-16: `renameat2(RENAME_NOREPLACE)` through
/// the raw syscall on Linux and Android, `renameatx_np(RENAME_EXCL)` on Apple,
/// `FileRenameInformation` on Windows), which also names the refused call in
/// its receipt (I-9) instead of surfacing a bare errno.
///
/// Protocol: `from` must hold exactly `expected` (a staged or retired inode an
/// external writer replaced is a collision, never published); the rename never
/// replaces `to`; the parent barrier is required — the graph tree is the sole
/// authority for these bytes; `to` is re-read to prove what became visible.
/// `crate::model::tests::direct_files_graph_text_publication_uses_the_graph_tree_noreplace_rename`
/// pins every Direct Files site to this function.
pub(super) fn move_graph_text_exact_no_replace(
    dir: &Dir,
    from: &str,
    to: &str,
    expected: &[u8],
) -> io::Result<()> {
    if read_projection_regular(dir, from)? != expected {
        return Err(graph_text_transition_byte_collision("source"));
    }
    rename_projection_noreplace(dir, from, to)?;
    sync_projection_directory(dir, 0, 1)?;
    if read_projection_regular(dir, to)? != expected {
        return Err(graph_text_transition_byte_collision("published"));
    }
    Ok(())
}

fn graph_text_transition_byte_collision(position: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("graph text name transition found different bytes at its {position} name"),
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn rename_graph_text_noreplace(
    source_dir: &Dir,
    source: &str,
    destination_dir: &Dir,
    destination: &str,
) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd};

    let source = CString::new(source)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid source name"))?;
    let destination = CString::new(destination)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid destination name"))?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_dir.as_fd().as_raw_fd(),
            source.as_ptr(),
            destination_dir.as_fd().as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE as libc::c_uint,
        )
    };
    (result == 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(super) fn rename_graph_text_noreplace(
    source_dir: &Dir,
    source: &str,
    destination_dir: &Dir,
    destination: &str,
) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd};

    let source = CString::new(source)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid source name"))?;
    let destination = CString::new(destination)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid destination name"))?;
    let result = unsafe {
        libc::renameatx_np(
            source_dir.as_fd().as_raw_fd(),
            source.as_ptr(),
            destination_dir.as_fd().as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL as libc::c_uint,
        )
    };
    (result == 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(windows)]
pub(super) fn rename_graph_text_noreplace(
    source_dir: &Dir,
    source: &str,
    destination_dir: &Dir,
    destination: &str,
) -> io::Result<()> {
    // Every other platform gives this function a real no-replace primitive
    // (`renameat2(RENAME_NOREPLACE)`, `renameatx_np(RENAME_EXCL)`). Windows used
    // `Dir::rename`, which cap-std implements with replace semantics — so the
    // one guarantee the name promises was the one Windows did not provide, and
    // an external file landing in the check-to-rename window was clobbered.
    //
    // `rename_projection_between_noreplace` is the same operation done properly:
    // it opens the source with DELETE access through the retained directory
    // capability and renames that exact handle with
    // `FileRenameInformation`/`ReplaceIfExists = FALSE`, rejecting filesystems
    // that cannot provide the primitive BEFORE the live source name is retired.
    // It already takes separate source and destination directories, so this is
    // the cross-directory case it was written for.
    rename_projection_between_noreplace(source_dir, source, destination_dir, destination)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "android",
    windows
)))]
pub(super) fn rename_graph_text_noreplace(
    _source_dir: &Dir,
    _source: &str,
    _destination_dir: &Dir,
    _destination: &str,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic capability-relative no-clobber move is unsupported on this platform",
    ))
}

/// The strict directory barrier for graph-tree artifacts the graph is the SOLE
/// authority for — conflict copies, trash, withdrawn bytes, assets. A barrier
/// the filesystem refuses for those is a real durability failure and stays fatal
/// on every platform, Android included.
///
/// One barrier, on the directory whose entries the operation changed; see
/// [`sync_projection_chain`] for why the ancestors take none.
pub(super) fn sync_projection_chain_required(chain: &[Dir]) -> io::Result<()> {
    projection_directory_sync_hook(Path::new("."))?;
    sync_projection_chain(chain)
}

/// Make the directory-entry changes of one projection operation durable.
///
/// **Only the leaf is flushed**, because only the leaf's entry list changed:
/// the operation inserted, replaced or removed a name in `chain.last()`. An
/// ancestor is flushed by exactly one mechanism, and it is not this one —
/// [`create_projection_chain_component`] flushes the parent of every directory
/// Tine creates while building the chain, at the moment it creates it. An
/// ancestor that Tine did not create in this operation already had a durable
/// entry in *its* parent before the operation began, and no in-scope failure
/// (crash/power loss, torn write, disk error, sync-service delivery,
/// external-editor race, honest concurrent instance, honest multi-device
/// divergence, malformed imported content) can un-durable an entry that is
/// already on stable storage. Re-flushing it therefore defends nothing.
///
/// See `docs/storage-sync-contract.md` §2.10a-i, which carries the same
/// argument and the refusal scenario for the flushes this removed. Before the
/// 2026-08-26 chain-flush cut this walked the whole chain leaf-to-root, so a
/// two-deep page path paid three barriers per call and about twelve per
/// foreground save.
fn sync_projection_chain(chain: &[Dir]) -> io::Result<()> {
    let depth = chain.len();
    let Some(leaf) = chain.last() else {
        return Ok(());
    };
    sync_projection_directory(leaf, depth.saturating_sub(1), depth)
}

/// Create one missing component of a projection parent chain and make the new
/// directory's NAME durable in the parent that now holds it.
///
/// This is the *only* place a freshly created projection ancestor gets its
/// barrier, and it is what lets [`sync_projection_chain`] flush the
/// leaf alone: after this returns, the created entry is on stable storage, so a
/// crash between here and the operation's own barrier cannot lose the path the
/// operation is about to publish into.
/// `projection_producer_census::g_b_choke_helper_caller_counts_are_pinned`
/// pins this function's callers; do not create a chain component anywhere else.
pub(super) fn create_projection_chain_component(parent: &Dir, component: &str) -> io::Result<()> {
    parent.create_dir(component)?;
    sync_projection_directory(parent, 0, 1)
}

/// The single place the projection leg calls the platform directory-flush
/// primitive. It names the operation and the chain position on failure — a bare
/// platform errno on a device receipt is not actionable — and is strict on every
/// platform (`docs/storage-sync-contract.md` §2.10a).
fn sync_projection_directory(dir: &Dir, index: usize, depth: usize) -> io::Result<()> {
    crate::durability_counters::note(crate::durability_counters::Barrier::Directory);
    tine_storage::sync_dir_required(dir).map_err(|error| {
        projection_platform_error(
            "fsync of the projection parent directory",
            &format!("chain depth {}/{depth}", index + 1),
            error,
        )
    })
}

#[cfg(unix)]
pub(super) fn projection_dir_identity(dir: &Dir) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    let metadata = dir.try_clone()?.into_std_file().metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
pub(super) fn canonical_projection_directory_resource_id(dir: &Dir) -> io::Result<ContentDigest> {
    let (device, inode) = projection_dir_identity(dir)?;
    let mut hasher = Sha256::new();
    hasher.update(b"tine/projection-directory-resource/v1\0unix-dev-inode\0");
    hasher.update(device.to_be_bytes());
    hasher.update(inode.to_be_bytes());
    Ok(ContentDigest::from_bytes(hasher.finalize().into()))
}

#[cfg(unix)]
pub(super) fn projection_files_have_same_identity(
    left: &fs::File,
    right: &fs::File,
) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok((left.dev(), left.ino()) == (right.dev(), right.ino()))
}

#[cfg(unix)]
pub(super) fn canonical_projection_file_resource_id(file: &fs::File) -> io::Result<ContentDigest> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    let mut identity = [0_u8; 16];
    identity[..8].copy_from_slice(&metadata.dev().to_be_bytes());
    identity[8..].copy_from_slice(&metadata.ino().to_be_bytes());
    let mut hasher = Sha256::new();
    hasher.update(b"tine/projection-file-resource/v1\0unix-dev-inode\0");
    hasher.update(identity);
    Ok(ContentDigest::from_bytes(hasher.finalize().into()))
}

#[cfg(unix)]
pub(crate) fn canonical_graph_resource_id(dir: &Dir) -> io::Result<CanonicalGraphResourceId> {
    let (device, inode) = projection_dir_identity(dir)?;
    let mut identity = [0_u8; 16];
    identity[..8].copy_from_slice(&device.to_be_bytes());
    identity[8..].copy_from_slice(&inode.to_be_bytes());
    Ok(CanonicalGraphResourceId::from_capability_identity(
        b"unix-dev-inode",
        &identity,
    ))
}

#[cfg(windows)]
pub(super) fn projection_dir_identity(dir: &Dir) -> io::Result<(u64, [u8; 16])> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let file = dir.try_clone()?.into_std_file();
    let mut information = FILE_ID_INFO::default();
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
    Ok((
        information.VolumeSerialNumber,
        information.FileId.Identifier,
    ))
}

#[cfg(windows)]
pub(super) fn canonical_projection_directory_resource_id(dir: &Dir) -> io::Result<ContentDigest> {
    let (volume, file_id) = projection_dir_identity(dir)?;
    let mut hasher = Sha256::new();
    hasher.update(b"tine/projection-directory-resource/v1\0windows-volume-file-id\0");
    hasher.update(volume.to_be_bytes());
    hasher.update(file_id);
    Ok(ContentDigest::from_bytes(hasher.finalize().into()))
}

#[cfg(windows)]
pub(super) fn projection_files_have_same_identity(
    left: &fs::File,
    right: &fs::File,
) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    fn identity(file: &fs::File) -> io::Result<(u64, [u8; 16])> {
        let mut information = FILE_ID_INFO::default();
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
        Ok((
            information.VolumeSerialNumber,
            information.FileId.Identifier,
        ))
    }

    Ok(identity(left)? == identity(right)?)
}

#[cfg(windows)]
pub(super) fn canonical_projection_file_resource_id(file: &fs::File) -> io::Result<ContentDigest> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let mut information = FILE_ID_INFO::default();
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
    let mut hasher = Sha256::new();
    hasher.update(b"tine/projection-file-resource/v1\0windows-volume-file-id\0");
    hasher.update(information.VolumeSerialNumber.to_be_bytes());
    hasher.update(information.FileId.Identifier);
    Ok(ContentDigest::from_bytes(hasher.finalize().into()))
}

#[cfg(windows)]
pub(crate) fn canonical_graph_resource_id(dir: &Dir) -> io::Result<CanonicalGraphResourceId> {
    let (volume, file_id) = projection_dir_identity(dir)?;
    let mut identity = [0_u8; 24];
    identity[..8].copy_from_slice(&volume.to_be_bytes());
    identity[8..].copy_from_slice(&file_id);
    Ok(CanonicalGraphResourceId::from_capability_identity(
        b"windows-volume-file-id",
        &identity,
    ))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn projection_dir_identity(_dir: &Dir) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "projection directory identity is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn canonical_projection_directory_resource_id(_dir: &Dir) -> io::Result<ContentDigest> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "projection directory identity is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn projection_files_have_same_identity(
    _left: &fs::File,
    _right: &fs::File,
) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "projection file identity is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn canonical_projection_file_resource_id(_file: &fs::File) -> io::Result<ContentDigest> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "projection file identity is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn canonical_graph_resource_id(_dir: &Dir) -> io::Result<CanonicalGraphResourceId> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "canonical graph resource identity is unsupported on this platform",
    ))
}
