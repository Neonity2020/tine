//! Tine's trash: typed trash directories, stats, legacy-entry classification,
//! and moving a file into the trash.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TrashEntryKind {
    Asset,
    Page,
    Journal,
    Conflict,
    Other,
}

impl TrashEntryKind {
    fn dir_name(self) -> Option<&'static str> {
        match self {
            TrashEntryKind::Asset => Some("assets"),
            TrashEntryKind::Page => Some("pages"),
            TrashEntryKind::Journal => Some("journals"),
            TrashEntryKind::Conflict => Some("conflicts"),
            TrashEntryKind::Other => None,
        }
    }
}

/// Translate the storage crate's physical boundary into the Graph API's I/O
/// boundary without losing the collision distinction needed by delete retries.
pub(super) fn graph_text_trash_filesystem_error(error: FilesystemError) -> io::Error {
    match error {
        FilesystemError::Io(error) => error,
        FilesystemError::DurableNameOperationUnavailable(message) => {
            io::Error::new(io::ErrorKind::Unsupported, message)
        }
        FilesystemError::UnsafeEntry(message) => io::Error::new(io::ErrorKind::InvalidData, message),
        FilesystemError::StoredLengthMismatch {
            path,
            expected,
            actual,
        } => io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "graph recovery stored length mismatch for {path}: expected {expected}, got {actual}"
            ),
        ),
        FilesystemError::StoredFileTooLarge { path, length, limit } => io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "graph recovery stored file is too large for {path}: {length} bytes exceeds {limit}"
            ),
        ),
        FilesystemError::ByteCollision => io::Error::new(
            io::ErrorKind::AlreadyExists,
            "graph recovery destination contains different bytes",
        ),
    }
}

pub(super) fn trash_root(root: &Path) -> PathBuf {
    root.join("logseq").join(".tine-trash")
}

pub(super) fn typed_trash_dir(root: &Path, kind: TrashEntryKind) -> PathBuf {
    trash_root(root).join(kind.dir_name().unwrap_or("other"))
}

pub(super) fn trash_dir_kind(path: &Path) -> Option<TrashEntryKind> {
    match path.file_name().and_then(|s| s.to_str()) {
        Some("assets") => Some(TrashEntryKind::Asset),
        Some("pages") => Some(TrashEntryKind::Page),
        Some("journals") => Some(TrashEntryKind::Journal),
        Some("conflicts") => Some(TrashEntryKind::Conflict),
        _ => None,
    }
}

fn add_trash_stat(stats: &mut TrashStats, kind: TrashEntryKind, bytes: u64) {
    match kind {
        TrashEntryKind::Asset => {
            stats.count += 1;
            stats.bytes += bytes;
        }
        TrashEntryKind::Page => stats.pages += 1,
        TrashEntryKind::Journal => stats.journals += 1,
        TrashEntryKind::Conflict => stats.conflicts += 1,
        TrashEntryKind::Other => stats.other += 1,
    }
}

pub(super) fn trash_stats(trash: &Path) -> TrashStats {
    let mut stats = TrashStats::default();
    let Ok(rd) = fs::read_dir(trash) else {
        return stats;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            if let Some(kind) = trash_dir_kind(&path) {
                add_typed_trash_dir_stats(&path, kind, &mut stats);
            } else {
                stats.other += 1;
            }
            continue;
        }
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        add_trash_stat(&mut stats, classify_legacy_trash_entry(&path, ft), bytes);
    }
    stats
}

fn add_typed_trash_dir_stats(path: &Path, kind: TrashEntryKind, stats: &mut TrashStats) {
    let Ok(rd) = fs::read_dir(path) else { return };
    for entry in rd.flatten() {
        let bytes = entry
            .file_type()
            .ok()
            .filter(|ft| ft.is_file())
            .and_then(|_| entry.metadata().ok())
            .map(|m| m.len())
            .unwrap_or(0);
        add_trash_stat(stats, kind, bytes);
    }
}

pub(super) fn classify_legacy_trash_entry(path: &Path, ft: fs::FileType) -> TrashEntryKind {
    if !ft.is_file() {
        return TrashEntryKind::Other;
    }
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return TrashEntryKind::Other;
    };
    let original = legacy_trash_original_name(name);
    let original_path = Path::new(original);
    if path_is_sync_conflict(original_path) {
        return TrashEntryKind::Conflict;
    }
    if text_extension_from_path(original_path).is_some() {
        return original_path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|stem| crate::date::JournalDate::from_file_stem(stem).is_some())
            .map(|_| TrashEntryKind::Journal)
            .unwrap_or(TrashEntryKind::Page);
    }
    if legacy_name_is_asset(original) {
        TrashEntryKind::Asset
    } else {
        TrashEntryKind::Other
    }
}

fn legacy_trash_original_name(name: &str) -> &str {
    name.split_once("__")
        .map(|(_, original)| original)
        .unwrap_or(name)
}

fn legacy_name_is_asset(name: &str) -> bool {
    if name.starts_with('.') || name.contains('/') || name.contains('\\') || name.ends_with(".edn")
    {
        return false;
    }
    let Some(ext) = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
    else {
        return false;
    };
    matches!(
        ext.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "avif"
            | "svg"
            | "bmp"
            | "tif"
            | "tiff"
            | "heic"
            | "heif"
            | "pdf"
            | "mp4"
            | "mov"
            | "m4v"
            | "webm"
            | "mkv"
            | "avi"
            | "mp3"
            | "wav"
            | "m4a"
            | "ogg"
            | "flac"
            | "aac"
            | "opus"
            | "txt"
            | "csv"
            | "tsv"
            | "json"
            | "yaml"
            | "yml"
            | "zip"
            | "tar"
            | "gz"
            | "tgz"
            | "7z"
            | "rar"
            | "doc"
            | "docx"
            | "xls"
            | "xlsx"
            | "ppt"
            | "pptx"
            | "odt"
            | "ods"
            | "odp"
            | "rtf"
    )
}

pub(super) fn trash_stamp() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{ms}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

pub(super) fn move_to_trash(src: &Path, dest: &Path, trash: &Path) -> io::Result<()> {
    fs::create_dir_all(trash).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("could not create trash directory {}: {e}", trash.display()),
        )
    })?;
    move_file_noreplace(src, dest).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("could not move file to trash {}: {e}", trash.display()),
        )
    })
}
