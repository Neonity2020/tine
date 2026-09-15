//! The projection's filesystem layer: nofollow opens of the root, directories
//! and files, and bounded, budgeted reads of regular files.

use super::*;

pub(super) fn configured_root_components(root: &str) -> Option<Vec<&str>> {
    if root.is_empty() || root.starts_with('/') || root.contains('\\') || root.contains('\0') {
        return None;
    }
    let components = root.split('/').collect::<Vec<_>>();
    components
        .iter()
        .all(|component| projection_component_is_portable(component))
        .then_some(components)
}

pub(super) fn projection_component_is_portable(component: &str) -> bool {
    graph_text_component_is_portable(component)
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    windows
))]
pub(super) fn require_projection_platform() -> io::Result<()> {
    Ok(())
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    windows
)))]
pub(super) fn require_projection_platform() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "exact projection durability is unsupported on this platform",
    ))
}

pub(super) fn open_projection_root_nofollow(root: &Path) -> io::Result<Dir> {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "graph root is not UTF-8"))?;
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent)?;
    let parent = Dir::open_ambient_dir(parent, ambient_authority())?;
    open_projection_dir_nofollow(&parent, name)
}

pub(super) fn projection_real_directory(dir: &Dir, name: &str) -> io::Result<()> {
    let metadata = dir.symlink_metadata(name)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "projection path contains a symlink, reparse point, or special parent",
        ));
    }
    #[cfg(windows)]
    {
        use cap_fs_ext::OsMetadataExt as _;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "projection parent is a reparse point",
            ));
        }
    }
    Ok(())
}

pub(super) fn projection_optional_regular_metadata(dir: &Dir, name: &str) -> io::Result<()> {
    let metadata = match dir.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "projection target is a symlink, reparse point, or special file",
        ));
    }
    #[cfg(windows)]
    {
        use cap_fs_ext::OsMetadataExt as _;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "projection target is a reparse point",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn open_projection_dir_nofollow(dir: &Dir, name: &str) -> io::Result<Dir> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};

    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid directory name"))?;
    let fd = unsafe {
        libc::openat(
            dir.as_fd().as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(projection_platform_error(
            "openat(O_DIRECTORY|O_NOFOLLOW) of a projection parent",
            &format!("{name:?}"),
            io::Error::last_os_error(),
        ));
    }
    Ok(Dir::from_std_file(unsafe { fs::File::from_raw_fd(fd) }))
}

#[cfg(windows)]
pub(super) fn open_projection_dir_nofollow(dir: &Dir, name: &str) -> io::Result<Dir> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _, OpenOptionsMaybeDirExt as _};
    use std::os::windows::fs::MetadataExt;

    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .follow(FollowSymlinks::No)
        .maybe_dir(true);
    let file = dir.open_with(name, &options)?.into_std();
    let metadata = file.metadata()?;
    if !metadata.is_dir()
        || metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(DirectSaveError::into_io(
            DirectSaveFailureCode::PrecheckNofollow,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "projection parent is not a real no-follow directory",
            ),
        ));
    }
    Ok(Dir::from_std_file(file))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_projection_dir_nofollow(_dir: &Dir, _name: &str) -> io::Result<Dir> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no-follow projection directories are unsupported on this platform",
    ))
}

#[cfg(unix)]
pub(super) fn open_projection_file_nofollow(dir: &Dir, name: &str) -> io::Result<fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd};

    #[cfg(test)]
    PROJECTION_EXACT_OPEN_COUNT.with(|count| count.set(count.get() + 1));
    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid page filename"))?;
    let fd = unsafe {
        libc::openat(
            dir.as_fd().as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(projection_platform_error(
            "openat(O_NOFOLLOW) of a projection file",
            &format!("{name:?}"),
            io::Error::last_os_error(),
        ));
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "projection target is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
pub(super) fn open_projection_file_nofollow(dir: &Dir, name: &str) -> io::Result<fs::File> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
    use std::os::windows::fs::MetadataExt;

    #[cfg(test)]
    PROJECTION_EXACT_OPEN_COUNT.with(|count| count.set(count.get() + 1));
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = dir.open_with(name, &options)?.into_std();
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "projection target is not a regular no-follow file",
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_projection_file_nofollow(_dir: &Dir, _name: &str) -> io::Result<fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no-follow projection reads are unsupported on this platform",
    ))
}

#[cfg(windows)]
fn open_projection_file_nofollow_for_sync(dir: &Dir, name: &str) -> io::Result<fs::File> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
    use cap_std::fs::OpenOptionsExt as _;
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    #[cfg(test)]
    PROJECTION_EXACT_OPEN_COUNT.with(|count| count.set(count.get() + 1));
    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .write(true)
        .follow(FollowSymlinks::No)
        .access_mode(GENERIC_READ | GENERIC_WRITE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    let file = dir.open_with(name, &options)?.into_std();
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "projection sync target is not a regular no-follow file",
        ));
    }
    Ok(file)
}

pub(super) fn open_and_read_projection_regular(
    dir: &Dir,
    name: &str,
) -> io::Result<(fs::File, Vec<u8>)> {
    open_and_read_projection_regular_with_limit(dir, name, MAX_PROJECTION_EVIDENCE_BYTES)
}

pub(super) fn open_and_read_projection_regular_with_limit(
    dir: &Dir,
    name: &str,
    limit: u64,
) -> io::Result<(fs::File, Vec<u8>)> {
    let file = open_projection_file_nofollow(dir, name)?;
    read_open_projection_regular_with_limit(file, limit)
}

fn read_open_projection_regular_with_limit(
    mut file: fs::File,
    limit: u64,
) -> io::Result<(fs::File, Vec<u8>)> {
    let len = file.metadata()?.len();
    if len > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence exceeds the reload bound",
        ));
    }
    bounded_read_after_metadata_hook()?;
    let capacity = usize::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence length is not addressable",
        )
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    let read_limit = limit.checked_add(1).ok_or_else(allocation_overflow)?;
    (&mut file).take(read_limit).read_to_end(&mut bytes)?;
    if usize_to_u64(bytes.len())? > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence grew beyond the reload bound",
        ));
    }
    Ok((file, bytes))
}

/// Read a graph-text body while reserving each retained byte before it enters the
/// returned vector. The chunked path closes the metadata/read growth gap: a
/// file that grows after metadata cannot make the preparation allocation exceed
/// the aggregate budget before it is rejected.
pub(super) fn open_and_read_projection_regular_with_budget(
    dir: &Dir,
    name: &str,
    limit: u64,
    budget: &RetainedContentBudget,
    resource: &'static str,
) -> io::Result<(fs::File, Vec<u8>, RetainedContentReservation)> {
    let mut file = open_projection_file_nofollow(dir, name)?;
    let len = file.metadata()?.len();
    if len > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence exceeds the reload bound",
        ));
    }
    let reservation = budget.reserve(len, resource)?;
    bounded_read_after_metadata_hook()?;
    let capacity = usize::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence length is not addressable",
        )
    })?;
    // Allocate the metadata-sized buffer once. A shrink leaves this capacity
    // charged; growth is detected with a stack byte and rejected without asking
    // Vec to grow outside admission.
    let mut bytes = vec![0_u8; capacity].into_boxed_slice().into_vec();
    assert_eq!(
        bytes.capacity(),
        capacity,
        "boxed bounded read must transfer exact retained capacity"
    );
    let mut filled = 0usize;
    while filled < bytes.len() {
        let read = file.read(&mut bytes[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    bytes.truncate(filled);
    let mut growth_probe = [0_u8; 1];
    if file.read(&mut growth_probe)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence grew after its bounded allocation",
        ));
    }
    Ok((file, bytes, reservation))
}

/// Read one projection-evidence file's bytes under the evidence bound.
pub(super) fn read_projection_regular(dir: &Dir, name: &str) -> io::Result<Vec<u8>> {
    open_and_read_projection_regular(dir, name).map(|(_, bytes)| bytes)
}

pub(super) fn read_projection_optional(dir: &Dir, name: &str) -> io::Result<Option<Vec<u8>>> {
    match open_and_read_projection_regular(dir, name) {
        Ok((_file, bytes)) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn read_projection_optional_bound_capture_with_limits(
    dir: &Dir,
    name: &str,
    content_limit: u64,
    peak_limit: u64,
) -> io::Result<Option<(Vec<u8>, BlobDescription, ContentDigest, u64, u64)>> {
    read_projection_optional_bound_capture_impl(dir, name, Some((content_limit, peak_limit)))
}

// How many graph text documents this thread has physically opened and read
// through the one projection capture primitive.
//
// Deliberately NOT test-only. It is what lets the clean watcher publish the
// document-read cost of its slowest full-scan turn, which is the property the
// bounded full scan exists to hold — and an architectural claim of that kind
// has to be observable in production, not asserted in a comment. A thread-local
// increment is free next to the open + read + SHA-256 it counts.
thread_local! {
    static GRAPH_TEXT_CAPTURE_READS: Cell<usize> = const { Cell::new(0) };
}

fn count_graph_text_capture_read() {
    GRAPH_TEXT_CAPTURE_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
}

#[cfg(test)]
pub(crate) fn graph_text_capture_reads() -> usize {
    GRAPH_TEXT_CAPTURE_READS.with(Cell::get)
}

fn read_projection_optional_bound_capture_impl(
    dir: &Dir,
    name: &str,
    limits: Option<(u64, u64)>,
) -> io::Result<Option<(Vec<u8>, BlobDescription, ContentDigest, u64, u64)>> {
    count_graph_text_capture_read();
    let rebound_limit = limits
        .map(|(content_limit, _)| content_limit)
        .unwrap_or(MAX_PROJECTION_EVIDENCE_BYTES);
    let opened = match limits {
        Some((content_limit, peak_limit)) => {
            open_and_read_projection_regular_exact_bound(dir, name, content_limit, peak_limit)
        }
        None => open_and_read_projection_regular(dir, name),
    };
    let (opened, bytes) = match opened {
        Ok(result) => result,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            graph_text_inventory_read_hook()?;
            return match dir.symlink_metadata(name) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "graph target appeared during absence capture",
                )),
                Err(error) => Err(error),
            };
        }
        Err(error) => return Err(error),
    };
    graph_text_inventory_read_hook()?;

    let mut rebound = open_projection_file_nofollow(dir, name)?;
    if !projection_files_have_same_identity(&opened, &rebound)? {
        return Err(DirectSaveError::into_io(
            DirectSaveFailureCode::PrecheckInterrupted,
            io::Error::new(
                io::ErrorKind::Interrupted,
                "graph target was replaced or changed during capture",
            ),
        ));
    }
    let expected = BlobDescription::of(&bytes);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut rebound_bytes = 0_u64;
    loop {
        let read = rebound.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        rebound_bytes = rebound_bytes
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "read-byte overflow"))?;
        if rebound_bytes > rebound_limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph target grew beyond the capture bound",
            ));
        }
        hasher.update(&buffer[..read]);
    }
    let rebound_description = BlobDescription::from_parts(hasher.finalize().into(), rebound_bytes);
    if rebound_description != expected {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "graph target changed while its retained binding was validated",
        ));
    }
    let file_resource_id = canonical_projection_file_resource_id(&opened)?;
    let peak_capture_buffer_bytes =
        checked_add_bytes(usize_to_u64(bytes.capacity())?, usize_to_u64(buffer.len())?)?;
    let validation_bytes = checked_add_bytes(expected.byte_length(), rebound_bytes)?;
    Ok(Some((
        bytes,
        expected,
        file_resource_id,
        validation_bytes,
        peak_capture_buffer_bytes,
    )))
}

fn open_and_read_projection_regular_exact_bound(
    dir: &Dir,
    name: &str,
    content_limit: u64,
    peak_limit: u64,
) -> io::Result<(fs::File, Vec<u8>)> {
    let mut file = open_projection_file_nofollow(dir, name)?;
    let len = file.metadata()?.len();
    if len > content_limit {
        return Err(graph_text_capture_limit_error("aggregate raw bytes"));
    }
    let allocation_peak = checked_add_bytes(len, 16 * 1024)?;
    if allocation_peak > peak_limit {
        return Err(graph_text_capture_limit_error("peak build memory"));
    }
    bounded_read_after_metadata_hook()?;
    let capacity = usize::try_from(len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence length is not addressable",
        )
    })?;
    let mut bytes = vec![0_u8; capacity].into_boxed_slice().into_vec();
    assert_eq!(
        bytes.capacity(),
        capacity,
        "boxed bounded read must transfer exact retained capacity"
    );
    let mut filled = 0usize;
    while filled < bytes.len() {
        let read = file.read(&mut bytes[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    bytes.truncate(filled);
    let mut growth_probe = [0_u8; 1];
    if file.read(&mut growth_probe)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "projection evidence grew after its bounded allocation",
        ));
    }
    Ok((file, bytes))
}
