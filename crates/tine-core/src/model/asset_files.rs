//! Asset path helpers and the PDF-highlight sidecar text: asset names and
//! relative paths, serializing and validating the .edn highlights, CRLF
//! preservation and optional text reads.

use super::*;

/// Atomically reserve a unique filename in `assets/` for `name`, de-duplicating
/// against existing files by appending `_1`, `_2`, … to the stem. Unlike a plain
/// `exists()` check followed by a write, this CREATES the file exclusively
/// (`create_new`), so a concurrent writer (OG Logseq, or another asset op) that
/// races between the name check and our write can't claim the same name and get
/// silently overwritten — whoever loses the create retries the next candidate.
/// Returns the chosen name and the open (empty) file handle.
/// Reject an asset name that isn't a plain top-level filename — a path separator
/// or a `.`/`..` component — so a frontend-supplied name can't reach outside
/// `assets/` (defense-in-depth; mirrors `trash_asset`). `create_new` already
/// blocks overwriting an existing file, so the realistic pre-guard outcome was a
/// stray file, not corruption — but reject it outright anyway.
pub(super) fn top_level_asset_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bad asset name",
        ));
    }
    Ok(())
}

/// Accept a portable assets-relative path for reads. Mutation entry points keep
/// using `top_level_asset_name`: supporting existing nested Logseq assets does
/// not grant frontend callers a nested write capability.
pub(super) fn relative_asset_path(name: &str) -> io::Result<PathBuf> {
    if name.contains('\\')
        || name
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bad asset path",
        ));
    }

    let path = PathBuf::from(name);
    if path
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bad asset path",
        ));
    }
    Ok(path)
}

/// Preserve a file's CRLF line endings on re-write: if `existing` used Windows
/// endings and the freshly-serialized `content` is all-LF, convert it back, so a
/// real edit produces a minimal diff instead of flipping every line (Syncthing
/// churn vs a Windows editor). New files stay LF. Shared by write_page +
/// write_highlights so the two can't drift on it.
pub(super) fn serialize_pdf_hls_page(
    path: &Path,
    document: &Document,
    existing: Option<&str>,
) -> io::Result<String> {
    // Concord invariant 4 (write-shyness): an `hls__` page is an ordinary Logseq
    // page the user and OG also write. This used to serialize with DEFAULT opts
    // — one trailing newline, tab indent, one blank line after the preamble —
    // so a highlight save re-indented and re-terminated the whole file even
    // where nothing changed. Reproduce the file's own formatting exactly as the
    // editor save path (`serialize_page_document`) does, including the
    // layout-identity retention that keeps untouched blocks byte-stable.
    let identities = doc::layout_identities_of(document);
    match Format::from_path(path) {
        Format::Md => {
            let opts = doc::SerializeOpts::detect_with_layout_identities(existing, &identities);
            Ok(preserve_crlf(
                doc::serialize_with(document, &opts),
                existing,
            ))
        }
        Format::Org => {
            if existing.is_some_and(|raw| !crate::org::org_editable(raw)) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "org highlight page is read-only (does not round-trip)",
                ));
            }
            Ok(crate::org::serialize_org_detect_with_layout_identities(
                document,
                existing,
                &identities,
            ))
        }
    }
}

pub(super) fn preserve_crlf(content: String, existing: Option<&str>) -> String {
    if existing.is_some_and(|e| e.contains("\r\n")) && !content.contains('\r') {
        content.replace('\n', "\r\n")
    } else {
        content
    }
}

/// Read an optional UTF-8 text file without conflating "missing" with "could not
/// safely read". Mutation paths use this for their baselines: only NotFound may
/// become `None`; every other error must stop the write.
pub(super) fn read_optional_text(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub(super) fn validate_highlight_edn(raw: &str) -> io::Result<()> {
    if raw.trim().is_empty() {
        return Ok(());
    }
    if matches!(crate::edn::parse_strict(raw), Some(crate::edn::Edn::Map(_))) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "highlight sidecar is malformed; refusing to replace it",
        ))
    }
}
