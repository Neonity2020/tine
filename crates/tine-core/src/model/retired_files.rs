//! Recovery for files stranded while an audited conditional publish was in flight.

use super::*;

/// Recover files stranded mid-publish by a crash.
///
/// [`atomic_replace_expected`] briefly leaves `path` non-existent while its
/// content sits under a `.retired` sibling. A crash in that window would
/// otherwise look like a deleted file. Restores the content when the target is
/// missing; otherwise the publish completed (or an external writer recreated
/// the file), so the retired copy goes to recoverable trash rather than being
/// deleted outright.
///
/// Registered directories only - never a whole-graph walk.
pub(crate) fn restore_retired_files(root: &Path, dirs: &[PathBuf]) -> io::Result<usize> {
    restore_retired_files_with(root, dirs, move_file_noreplace)
}

/// [`restore_retired_files`] over an explicit no-replace mover (test seam for
/// storage that refuses the flagged rename, GH #538).
pub(crate) fn restore_retired_files_with(
    root: &Path,
    dirs: &[PathBuf],
    mover: impl Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<usize> {
    let mut recovered = 0usize;
    for dir in dirs {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let retired = entry.path();
            let Some(name) = retired.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(target_name) = retired_target_name(name) else {
                continue;
            };
            let target = dir.join(target_name);
            if target.exists() {
                // The publish completed, or an external writer recreated the
                // file. Either way the retired copy is superseded - keep it
                // recoverable instead of deleting it.
                let trash = typed_trash_dir(root, TrashEntryKind::Conflict);
                fs::create_dir_all(&trash)?;
                restore_vacated_name(&retired, &trash.join(name), &mover).map_err(|error| {
                    named_recovery_error("moving to recovery trash", name, error)
                })?;
                continue;
            }
            // `restore_vacated_name` falls back to a plain rename only when the
            // storage refuses the no-replace flag and `target` is still absent:
            // a stranded `config.edn` otherwise blocks every open (GH #538).
            restore_vacated_name(&retired, &target, &mover)
                .map_err(|error| named_recovery_error("restoring", name, error))?;
            recovered += 1;
        }
    }
    Ok(recovered)
}

/// Name the step and the (graph-relative, `logseq/`) file, so a refusal on the
/// open path is not a bare errno (GH #538 showed only "Invalid argument").
fn named_recovery_error(step: &str, name: &str, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("recovering an interrupted settings write ({step} logseq/{name}) failed: {error}"),
    )
}

/// `.config.edn.1234.7.retired` -> `config.edn`.
pub(super) fn retired_target_name(retired: &str) -> Option<&str> {
    let rest = retired.strip_prefix('.')?;
    let rest = rest.strip_suffix(RETIRED_SUFFIX)?;
    let (rest, _seq) = rest.rsplit_once('.')?;
    let (name, _pid) = rest.rsplit_once('.')?;
    (!name.is_empty()).then_some(name)
}
