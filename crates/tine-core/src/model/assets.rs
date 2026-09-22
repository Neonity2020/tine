//! Graph's asset surface: the assets directory, orphan detection, bounded
//! reads and streaming, saving and importing, and the asset trash.

use super::*;

impl Graph {
    pub fn assets_path(&self) -> PathBuf {
        self.assets_root.clone()
    }

    /// Top-level `assets/` files that NO block references — orphans the user may
    /// want to trash. Tine never auto-deletes assets (a deleted block keeps its
    /// media as a safety net), so this is the discovery half of "find unused
    /// media". Conservative: scans every block's `raw` + page `pre_block` for any
    /// `assets/<name>` mention; skips subdirectories (PDF area-image stores) and
    /// `.edn`/dotfiles (sidecars, not media) so nothing in use is ever flagged.
    /// A graph whose pages cannot be read is an error: with no references,
    /// every asset would be listed as an orphan.
    pub fn orphan_assets(&self) -> io::Result<Vec<AssetInfo>> {
        let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
        self.try_with_pages(|pages| {
            for (_e, doc) in pages {
                if let Some(pre) = &doc.pre_block {
                    collect_asset_refs(pre, &mut referenced);
                }
                for b in &doc.roots {
                    collect_block_asset_refs(b, &mut referenced);
                }
            }
        })?;
        Ok(self.orphan_assets_with_references(&referenced))
    }

    pub(crate) fn orphan_assets_with_references(
        &self,
        referenced: &std::collections::HashSet<String>,
    ) -> Vec<AssetInfo> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(self.assets_path()) else {
            return out;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_file() {
                continue; // skip subdirs (PDF area-image stores, tied to a PDF)
            }
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // Sidecars/hidden files aren't user media; never flag them as orphans.
            if name.starts_with('.') || name.ends_with(".edn") {
                continue;
            }
            if referenced.contains(name) {
                continue;
            }
            let meta = entry.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            out.push(AssetInfo {
                name: name.to_string(),
                size,
                modified,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Move an asset file to `logseq/.tine-trash` (recoverable), never a hard
    /// delete by default. Refuses any name with a path separator (top-level
    /// assets only) so it can't reach outside `assets/`.
    pub fn trash_asset(&self, name: &str) -> io::Result<()> {
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bad asset name",
            ));
        }
        let src = self.assets_path().join(name);
        if !src.is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such asset"));
        }
        let trash = typed_trash_dir(&self.root, TrashEntryKind::Asset);
        self.ensure_asset_write_target(&src)?;
        self.ensure_trash_write_target(&trash)?;
        let dest = trash.join(format!("{}__{name}", trash_stamp()));
        move_to_trash(&src, &dest, &trash)?;
        Ok(())
    }

    /// File count + total bytes currently in the asset trash. Non-asset recovery
    /// entries are counted separately so an asset cleanup cannot silently sweep
    /// pages, journals, or sync-conflict copies.
    pub fn asset_trash_stats(&self) -> TrashStats {
        trash_stats(&trash_root(&self.root))
    }

    /// Permanently delete asset-type entries in the asset trash. Returns the
    /// number of entries removed. Page, journal, conflict, and unknown legacy
    /// entries stay recoverable in `logseq/.tine-trash`.
    pub fn empty_asset_trash(&self) -> io::Result<u64> {
        let trash = trash_root(&self.root);
        self.ensure_trash_write_target(&trash)?;
        let mut removed = 0;
        match fs::read_dir(&trash) {
            Ok(rd) => {
                for entry in rd.flatten() {
                    let Ok(ft) = entry.file_type() else { continue };
                    if ft.is_dir() {
                        if trash_dir_kind(&entry.path()) == Some(TrashEntryKind::Asset) {
                            for asset_entry in fs::read_dir(entry.path())?.flatten() {
                                let path = asset_entry.path();
                                let ok = match asset_entry.file_type() {
                                    Ok(ft) if ft.is_dir() => fs::remove_dir_all(&path).is_ok(),
                                    Ok(_) => fs::remove_file(&path).is_ok(),
                                    Err(_) => false,
                                };
                                if ok {
                                    removed += 1;
                                }
                            }
                        }
                    } else if classify_legacy_trash_entry(&entry.path(), ft)
                        == TrashEntryKind::Asset
                        && fs::remove_file(entry.path()).is_ok()
                    {
                        removed += 1;
                    }
                }
                Ok(removed)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Read raw bytes of an asset (e.g. a PDF) for the viewer.
    pub fn read_asset(&self, name: &str) -> io::Result<Vec<u8>> {
        fs::read(self.asset_file_for_read(name)?)
    }

    /// Resolve an existing regular asset through the canonical asset capability.
    /// A symlink may point elsewhere inside that approved root, but can never
    /// turn a read/open into access outside it.
    pub fn asset_file_for_read(&self, name: &str) -> io::Result<PathBuf> {
        let relative = relative_asset_path(name)?;
        let assets = fs::canonicalize(self.assets_path())?;
        let path = fs::canonicalize(self.assets_path().join(relative))?;
        if !path.starts_with(&assets) || !fs::metadata(&path)?.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid asset"));
        }
        Ok(path)
    }

    /// Resolve an asset path (regular file OR directory inside the approved
    /// assets root) for the OS opener. An empty name is the assets root itself:
    /// OG opens the empty `[...](./assets/)` link in the file manager (GH #367).
    /// Reads/streaming keep `asset_file_for_read`'s regular-file gate; the
    /// containment check is identical, so a symlink cannot escape assets/.
    pub fn asset_path_for_open(&self, name: &str) -> io::Result<PathBuf> {
        let assets = fs::canonicalize(self.assets_path())?;
        if name.is_empty() {
            return Ok(assets);
        }
        let relative = relative_asset_path(name)?;
        let path = fs::canonicalize(self.assets_path().join(relative))?;
        if !path.starts_with(&assets) || !(path.is_file() || path.is_dir()) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid asset"));
        }
        Ok(path)
    }

    /// Canonical, regular-file path for the native asset protocol. This is used
    /// for audio/video so WebView range requests read at most a small chunk
    /// instead of copying a multi-gigabyte file through Rust Vec → IPC → Blob.
    pub fn stream_asset_path(&self, name: &str) -> io::Result<PathBuf> {
        let relative = relative_asset_path(name)?;
        let mut candidate = self.assets_path();
        for component in relative.components() {
            candidate.push(component);
            if fs::symlink_metadata(&candidate)?.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "asset symlinks cannot be streamed",
                ));
            }
        }
        self.asset_file_for_read(name)
    }

    /// Read an asset only if its current on-disk size is within `max_bytes`.
    /// The post-read check closes the metadata/read race if another process grows
    /// the file between those operations.
    pub fn read_asset_limited(&self, name: &str, max_bytes: u64) -> io::Result<Vec<u8>> {
        let path = self.asset_file_for_read(name)?;
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "asset is not a regular file",
            ));
        }
        if metadata.len() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                crate::backend_error::tagged_backend_error("asset-too-large", None),
            ));
        }
        let bytes = fs::read(path)?;
        if bytes.len() as u64 > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                crate::backend_error::tagged_backend_error("asset-too-large", None),
            ));
        }
        Ok(bytes)
    }

    /// Write raw bytes (e.g. a pasted image) into `assets/`, returning the
    /// stored filename (de-duplicated if it already exists).
    pub fn save_asset(&self, name: &str, bytes: &[u8]) -> io::Result<String> {
        let assets = self.assets_path();
        self.ensure_asset_write_target(&assets)?;
        fs::create_dir_all(&assets)?;
        top_level_asset_name(name)?;
        let (stem, ext) = split_asset_stem_ext(name);
        for i in 0usize.. {
            let final_name = if i == 0 {
                name.to_string()
            } else {
                format!("{stem}_{i}{ext}")
            };
            match atomic_write_new(&assets.join(&final_name), bytes) {
                Ok(()) => return Ok(final_name),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    /// Copy a file into `assets/`, returning the stored filename. De-duplicates
    /// against existing assets (never overwrites one already referenced by notes).
    pub fn import_asset(&self, src: &Path, name: Option<&str>) -> io::Result<String> {
        // Desired stored name (a timestamped name from the frontend), else the
        // source basename. `reserve_asset` still dedups same-name collisions.
        let name = match name {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => src
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad source filename"))?
                .to_string(),
        };
        let assets = self.assets_path();
        self.ensure_asset_write_target(&assets)?;
        fs::create_dir_all(&assets)?;
        top_level_asset_name(&name)?;
        let (stem, ext) = split_asset_stem_ext(&name);
        for i in 0usize.. {
            let final_name = if i == 0 {
                name.clone()
            } else {
                format!("{stem}_{i}{ext}")
            };
            match atomic_copy_new(src, &assets.join(&final_name)) {
                Ok(()) => return Ok(final_name),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    /// Stream an already-open native capture into `assets/` without ever
    /// materializing it as a bridge/base64 value. The source handle is the
    /// capability validated by the native caller; collision retries rewind it.
    pub fn import_asset_file(
        &self,
        src: &mut fs::File,
        name: &str,
        max_bytes: u64,
    ) -> io::Result<String> {
        let assets = self.assets_path();
        self.ensure_asset_write_target(&assets)?;
        fs::create_dir_all(&assets)?;
        top_level_asset_name(name)?;
        let (stem, ext) = split_asset_stem_ext(name);
        for i in 0usize.. {
            let final_name = if i == 0 {
                name.to_string()
            } else {
                format!("{stem}_{i}{ext}")
            };
            src.seek(io::SeekFrom::Start(0))?;
            match atomic_copy_file_new(src, &assets.join(&final_name), max_bytes) {
                Ok(()) => return Ok(final_name),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        unreachable!()
    }
}
