//! Reserving a unique asset name (compound extensions included) and walking
//! the page files under a directory.

use super::*;

/// Compound asset extensions that a downstream matcher keys on AS A WHOLE (e.g.
/// drawio's editable SVG, whose `.drawio.svg` suffix is what surfaces the
/// "Edit in draw.io" affordance). De-dup must insert its `_N` counter BEFORE the
/// whole suffix — `flow.drawio.svg` must collide to `flow_1.drawio.svg`, NOT
/// `flow.drawio_1.svg` (a naive last-dot split), which would still end in `.svg`
/// but no longer match `\.drawio\.svg$` and silently lose the editor button
/// (GH #38). Longest match wins; case-insensitive.
const COMPOUND_ASSET_EXTS: &[&str] = &[".drawio.svg", ".excalidraw.svg", ".excalidraw.png"];

/// Split an asset filename into (stem, extension) for de-dup counter insertion,
/// preserving known compound extensions (see `COMPOUND_ASSET_EXTS`). Falls back
/// to a last-dot split for ordinary single extensions.
pub(super) fn split_asset_stem_ext(name: &str) -> (String, String) {
    let lower = name.to_ascii_lowercase();
    for ext in COMPOUND_ASSET_EXTS {
        if lower.ends_with(ext) {
            let cut = name.len() - ext.len();
            return (name[..cut].to_string(), name[cut..].to_string());
        }
    }
    match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.to_string(), String::new()),
    }
}

#[cfg(test)]
pub(super) fn reserve_asset(assets: &Path, name: &str) -> io::Result<(String, fs::File)> {
    top_level_asset_name(name)?;
    let create_new = |n: &str| {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(assets.join(n))
    };
    match create_new(name) {
        Ok(f) => return Ok((name.to_string(), f)),
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let (stem, ext) = split_asset_stem_ext(name);
    let mut i = 1;
    loop {
        let candidate = format!("{stem}_{i}{ext}");
        match create_new(&candidate) {
            Ok(f) => return Ok((candidate, f)),
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => i += 1,
        }
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static CACHE_LINEAR_SCAN_STEPS: std::cell::Cell<usize> = std::cell::Cell::new(0);
    pub(super) static GRAPH_TEXT_INVENTORY_ENTRY_VISITS: std::cell::Cell<usize> = std::cell::Cell::new(0);
    pub(super) static GRAPH_TEXT_CONTENT_READS: std::cell::Cell<usize> = std::cell::Cell::new(0);
    pub(super) static GRAPH_TEXT_PARSE_ATTEMPTS: std::cell::Cell<usize> = std::cell::Cell::new(0);
    static EXACT_PAGE_DTO_PARSE_ATTEMPTS: std::cell::Cell<usize> = std::cell::Cell::new(0);
    pub(super) static GRAPH_TEXT_VALIDATION_TARGET_READS: std::cell::Cell<usize> = std::cell::Cell::new(0);
    static JOURNAL_PROJECTION_GUARDED_PARSE_PAIRS: std::cell::Cell<usize> = std::cell::Cell::new(0);
}

#[cfg(test)]
pub(super) fn count_cache_linear_scan(n: usize) {
    CACHE_LINEAR_SCAN_STEPS.with(|steps| steps.set(steps.get() + n));
}

pub(super) fn walk_page_files(dir: &Path, mut visit: impl FnMut(PathBuf)) {
    // Descend into sub-directories (#21). Logseq scans the whole graph root
    // recursively, so a page archived under `pages/client-a/foo.md` is a real
    // page — keyed by its BASENAME (`foo`); the sub-path is discarded, matching
    // OG's `path->file-name` (the file's own `path` stays its load/save identity).
    // One stack-based walk, O(files), no re-scan.
    //
    // `file_type()` does not follow symlinks. Check it for page-looking entries
    // too: otherwise `pages/secret.md -> /outside/secret.md` would be indexed and
    // exposed. Hidden dirs (`.git` &c.) are skipped — never a page store.
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if is_page_file(&path) && file_type.is_file() {
                visit(path);
                continue;
            }
            // Non-page entry: recurse if it's a real (non-symlink, non-hidden)
            // sub-directory. This is the only stat we pay, and never on the hot
            // page-file path above.
            let hidden = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.starts_with('.'))
                .unwrap_or(true);
            if !hidden && file_type.is_dir() {
                stack.push(path);
            }
        }
    }
}
