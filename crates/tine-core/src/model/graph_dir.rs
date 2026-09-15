//! Validating a graph directory and keeping a path inside its root.

use super::*;

/// Validate one config-controlled graph directory. Logseq permits nested relative
/// directories, but an absolute path, traversal component, or symlinked existing
/// ancestor outside the graph would turn ordinary save/delete/restore operations
/// into writes against unrelated files.
pub(super) fn validate_graph_dir(root: &Path, raw: &str, label: &str) -> io::Result<()> {
    if raw.is_empty() || raw.contains('\\') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {label} directory: {raw:?}"),
        ));
    }
    let rel = Path::new(raw);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} directory must be a safe relative path: {raw:?}"),
        ));
    }
    let candidate = root.join(rel);
    if !path_stays_within_root(root, &candidate) || path_uses_graph_text_alias(root, &candidate) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} directory escapes graph root: {raw:?}"),
        ));
    }
    Ok(())
}

/// Containment check for both existing and not-yet-created targets. Canonicalize
/// the deepest existing ancestor so a symlink in the path cannot smuggle a later
/// filename outside the graph. The runtime root is already canonical, while the
/// fallback keeps disposable direct-`Graph::open` fixtures working as before.
pub(super) fn path_stays_within_root(root: &Path, target: &Path) -> bool {
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut existing = target;
    while fs::symlink_metadata(existing).is_err() {
        let Some(parent) = existing.parent() else {
            return false;
        };
        existing = parent;
    }
    fs::canonicalize(existing)
        .map(|p| p.starts_with(&canonical_root))
        .unwrap_or(false)
}

/// Graph directories Tine reads or writes must retain their own identity, not merely land
/// somewhere under the graph after canonicalization. An in-graph symlink such as
/// `publish -> assets` passes a plain containment check but redirects generated
/// output onto user assets. Compare the deepest existing ancestor with its
/// expected canonical lexical location to reject any such alias.
pub(super) fn path_uses_graph_text_alias(root: &Path, target: &Path) -> bool {
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut existing = target;
    while fs::symlink_metadata(existing).is_err() {
        let Some(parent) = existing.parent() else {
            return true;
        };
        existing = parent;
    }
    let Ok(relative) = existing.strip_prefix(root) else {
        return true;
    };
    fs::canonicalize(existing)
        .map(|actual| actual != canonical_root.join(relative))
        .unwrap_or(true)
}
