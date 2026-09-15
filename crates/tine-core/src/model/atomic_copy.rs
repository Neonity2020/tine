//! Atomic copy and atomic update, and the durable private-authority file
//! operations built on them.

use super::*;

/// Like [`atomic_write`] but the payload is COPIED from `src` (so a large import —
/// a PDF, a big image — isn't slurped fully into memory): copy into a unique temp
/// in the destination dir, fsync it, then atomically rename into place. The temp
/// is removed on any failure, and the directory entry is fsynced on success. The
/// temp name is hidden (`.`-prefixed) so the orphan-asset scanner never lists it.
pub fn atomic_copy(src: &Path, dst: &Path) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let fname = dst.file_name().and_then(|s| s.to_str()).unwrap_or("asset");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{fname}.{}.{seq}.import.tmp", std::process::id()));
    let res = (|| {
        let mut input = fs::File::open(src)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        std::io::copy(&mut input, &mut output)?;
        barrier_sync_all(&output)?;
        drop(output);
        fs::rename(&tmp, dst)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
        return res;
    }
    sync_dir(dir)
}

/// Copy into a newly-created destination without replacing a path that appeared
/// concurrently. Used by restore after the previous live inode has been moved to
/// recovery: a sync writer that recreates the live name wins and the restore
/// aborts instead of clobbering it.
pub fn atomic_copy_new(src: &Path, dst: &Path) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let fname = dst.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{fname}.{}.{}.restore.tmp",
        std::process::id(),
        seq
    ));
    let res = (|| {
        let mut input = fs::File::open(src)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        std::io::copy(&mut input, &mut output)?;
        barrier_sync_all(&output)?;
        drop(output);
        move_file_noreplace(&tmp, dst)?;
        sync_dir(dir)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// Copy from an already-open source capability into a new destination while
/// enforcing a byte ceiling during the stream. This is the native-capture path:
/// it avoids reopening an attacker-replaceable pathname and avoids whole-value
/// Android/IPC/base64 amplification.
pub fn atomic_copy_file_new(input: &mut fs::File, dst: &Path, max_bytes: u64) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = dst.parent().unwrap_or_else(|| Path::new("."));
    let fname = dst.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{fname}.{}.{}.capture.tmp",
        std::process::id(),
        seq
    ));
    let res = (|| {
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        let mut limited = input.take(max_bytes.saturating_add(1));
        let copied = io::copy(&mut limited, &mut output)?;
        if copied > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("capture exceeds {max_bytes} byte limit"),
            ));
        }
        barrier_sync_all(&output)?;
        drop(output);
        move_file_noreplace(&tmp, dst)?;
        sync_dir(dir)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// Read-modify-write one small app-private authority file through the typed
/// durable directory-publication boundary.
///
/// This is deliberately narrower than [`atomic_update`]: callers must own a
/// private single-writer namespace rather than a graph file that Logseq,
/// Syncthing, or an external editor may also change. The typed publication is
/// required because these files select which storage authority Tine serves;
/// on Windows their create/replace acknowledgement therefore uses certified
/// write-through name operations rather than a rename followed by an
/// unavailable directory fsync.
pub fn durable_private_authority_update(
    path: &Path,
    lock: &std::sync::Mutex<()>,
    edit: impl Fn(&str) -> io::Result<String>,
) -> io::Result<()> {
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for _attempt in 0..4 {
        let baseline = match fs::read_to_string(path) {
            Ok(value) => Some(value),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let next = edit(baseline.as_deref().unwrap_or("{}\n"))?;
        let current = match fs::read_to_string(path) {
            Ok(value) => Some(value),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if current != baseline {
            continue;
        }
        let (directory, filename) = durable_private_authority_directory(path)?;
        let publication = DurableDirectoryPublication::open(&directory)
            .map_err(graph_text_trash_filesystem_error)?;
        let published = match baseline.as_deref() {
            None => publication.publish_new_exact_single_writer(&filename, next.as_bytes()),
            Some(expected) => {
                publication.replace_exact(&filename, expected.as_bytes(), next.as_bytes())
            }
        };
        match published {
            Ok(()) => return Ok(()),
            Err(FilesystemError::ByteCollision) => continue,
            Err(error) => return Err(graph_text_trash_filesystem_error(error)),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "private authority changed repeatedly during update",
    ))
}

/// Durably remove one app-private authority name by first moving its exact
/// bytes to a fresh same-directory name outside the selector grammar.
///
/// A crash after the typed retirement can leave only inert recovery residue;
/// it cannot resurrect the active selector name. Ordinary completion removes
/// that residue immediately.
pub fn durable_private_authority_retire(
    path: &Path,
    lock: &std::sync::Mutex<()>,
) -> io::Result<()> {
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let expected = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let (directory, filename) = durable_private_authority_directory(path)?;
    let publication =
        DurableDirectoryPublication::open(&directory).map_err(graph_text_trash_filesystem_error)?;
    let retired = format!(".{filename}.retired-{}", Uuid::new_v4().simple());
    publication
        .retire_exact(&filename, &retired, &expected)
        .map_err(graph_text_trash_filesystem_error)?;
    let _ = directory.remove_file(&retired);
    Ok(())
}

fn durable_private_authority_directory(path: &Path) -> io::Result<(Dir, String)> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "private authority has no parent",
        )
    })?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "private authority has no safe filename",
            )
        })?
        .to_owned();

    let mut missing = Vec::new();
    let mut cursor = parent;
    while !cursor.exists() {
        missing.push(cursor.to_path_buf());
        cursor = cursor.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "private authority parent has no existing ancestor",
            )
        })?;
    }
    fs::create_dir_all(parent)?;
    // File publication below flushes `parent` itself. Flush every newly
    // created directory's parent as well, so Android/Unix initial activation
    // cannot acknowledge a binding whose parent entry is still volatile.
    for directory in &missing {
        if let Some(created_parent) = directory.parent() {
            sync_dir_for_rename(created_parent)?;
        }
    }
    Dir::open_ambient_dir(parent, ambient_authority()).map(|directory| (directory, filename))
}
