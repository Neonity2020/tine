//! Graph's PDF surface: area-image write/trash/rollback, the hls__ highlight page,
//! the PDF view-state and highlight sidecars and their commit/rollback, and
//! whether an asset key is still in use by a PDF.

use super::*;

impl Graph {
    /// Write a cropped area-highlight image to OG's file-graph layout:
    /// `assets/<key>/<page>_<id>_<stamp>.png` (`<stamp>` = the `js/Date.now()`
    /// epoch-ms integer also stored in the highlight's `:content {:image …}`).
    /// Returns the assets-relative path.
    ///
    /// **Non-dedup on purpose:** the filename IS the stable link from the `.edn`
    /// entry to the file, so a re-save must overwrite in place rather than rename
    /// on collision (which `reserve_asset` would do, breaking the link).
    pub fn write_pdf_area_image(
        &self,
        pdf_filename: &str,
        page: i64,
        id: &str,
        stamp: i64,
        bytes: &[u8],
    ) -> io::Result<String> {
        let key = crate::pdf::asset_key(pdf_filename);
        let dir = self.assets_path().join(&key);
        self.ensure_asset_write_target(&dir)?;
        fs::create_dir_all(&dir)?;
        let name = format!("{page}_{id}_{stamp}.png");
        // The highlight `id` round-trips through the graph `.edn`, so a synced/hand-edited
        // file can control it — reject any path separator so it can't escape the assets
        // dir and write a `.png` anywhere (audit M3, path traversal).
        top_level_asset_name(&name)?;
        let target = dir.join(&name);
        self.ensure_asset_write_target(&target)?;
        atomic_write(&target, bytes)?;
        Ok(format!("{key}/{name}"))
    }

    fn move_pdf_area_image_to_trash(
        &self,
        source_key: &str,
        page: i64,
        id: &str,
        stamp: i64,
    ) -> io::Result<Option<(PathBuf, PathBuf)>> {
        let name = format!("{page}_{id}_{stamp}.png");
        top_level_asset_name(&name)?;
        let source = self.assets_path().join(source_key).join(&name);
        if !source.exists() {
            return Ok(None);
        }
        self.ensure_asset_write_target(&source)?;
        let trash = typed_trash_dir(&self.root, TrashEntryKind::Asset);
        self.ensure_trash_write_target(&trash)?;
        let trash_name = format!("{}__pdf-area__{}__{name}", trash_stamp(), source_key);
        top_level_asset_name(&trash_name)?;
        let destination = trash.join(trash_name);
        move_to_trash(&source, &destination, &trash)?;
        Ok(Some((source, destination)))
    }

    /// Roll back a crop written before its highlight sidecar transaction failed.
    /// The nested source path is derived from the same PDF tuple as the writer;
    /// callers never receive a general nested-asset deletion capability.
    pub fn rollback_pdf_area_image(
        &self,
        pdf_filename: &str,
        page: i64,
        id: &str,
        stamp: i64,
    ) -> io::Result<()> {
        let key = crate::pdf::asset_key(pdf_filename);
        self.move_pdf_area_image_to_trash(&key, page, id, stamp)?;
        Ok(())
    }

    /// After the highlight sidecar + hls page pair is durably committed, move
    /// deleted area crops to recoverable asset trash. OG removes this exact crop
    /// with its highlight (`extensions/pdf/core.cljs:155-159` and
    /// `extensions/pdf/assets.cljs:137-147` at OG 6e7afa8eb); Tine keeps the same
    /// lifecycle without introducing OG's hard delete.
    ///
    /// Cleanup is deliberately best-effort and compare-guarded: the paired save
    /// is already committed, so a cleanup failure must not make the frontend
    /// restore stale state. Any sidecar change before or immediately after a move
    /// aborts cleanup, rolling that move back when possible.
    fn trash_deleted_pdf_area_images(
        &self,
        source_key: &str,
        edn_path: &Path,
        committed_edn: &str,
        source_sidecar_guard: Option<(&Path, &str)>,
        deleted: &[crate::pdf::Highlight],
    ) {
        if deleted.is_empty() {
            return;
        }
        for highlight in deleted {
            let Some(stamp) = highlight.image else {
                continue;
            };
            let Ok(Some(current_edn)) = read_optional_text(edn_path) else {
                return;
            };
            if current_edn != committed_edn {
                return;
            }
            if source_sidecar_guard.is_some_and(|(path, baseline)| {
                read_optional_text(path)
                    .map(|raw| raw.as_deref() != Some(baseline))
                    .unwrap_or(true)
            }) {
                return;
            }
            if crate::pdf::parse_highlights(&current_edn)
                .iter()
                .any(|remaining| remaining.image == Some(stamp))
            {
                continue;
            }

            let Ok(Some((source, destination))) =
                self.move_pdf_area_image_to_trash(source_key, highlight.page, &highlight.id, stamp)
            else {
                continue;
            };

            // A non-cooperating writer can change the sidecar between the
            // last-moment read and rename. Put the crop back if that happened.
            let primary_unchanged = read_optional_text(edn_path)
                .map(|raw| raw.as_deref() == Some(committed_edn))
                .unwrap_or(false);
            let source_unchanged = source_sidecar_guard.is_none_or(|(path, baseline)| {
                read_optional_text(path)
                    .map(|raw| raw.as_deref() == Some(baseline))
                    .unwrap_or(false)
            });
            if primary_unchanged && source_unchanged {
                continue;
            }
            let _ = move_file_noreplace(&destination, &source);
            return;
        }
    }

    /// Read highlights for a PDF from `assets/<key>.edn`.
    ///
    /// If the OG-compatible key's file is absent but a file under Tine's old
    /// `legacy_asset_key` exists, read that instead (it is migrated forward to
    /// the new key on the next `write_highlights`). This keeps highlights made
    /// by pre-launch Tine builds from disappearing after the key change.
    pub fn read_highlights(&self, pdf_filename: &str) -> Vec<crate::pdf::Highlight> {
        self.read_pdf_state(pdf_filename).highlights
    }

    fn read_pdf_state(&self, pdf_filename: &str) -> crate::pdf::PdfState {
        let key = crate::pdf::asset_key(pdf_filename);
        let s = self
            .asset_file_for_read(&format!("{key}.edn"))
            .and_then(fs::read_to_string)
            .ok()
            .or_else(|| {
                let legacy = crate::pdf::legacy_asset_key(pdf_filename);
                (legacy != key)
                    .then(|| {
                        self.asset_file_for_read(&format!("{legacy}.edn"))
                            .and_then(fs::read_to_string)
                            .ok()
                    })
                    .flatten()
            });
        s.map(|s| crate::pdf::parse_pdf_state(&s))
            .unwrap_or_default()
    }

    fn existing_hls_page_path(
        &self,
        write: &GraphTextWritePermit,
        key: &str,
    ) -> io::Result<Option<PathBuf>> {
        let name = crate::pdf::hls_page_name(key);
        let md = self.pages_path().join(format!("{name}.md"));
        let org = self.pages_path().join(format!("{name}.org"));
        match (
            self.graph_text_exists(write, &md)?,
            self.graph_text_exists(write, &org)?,
        ) {
            (true, true) => Err(twin_error(&name)),
            (true, false) => Ok(Some(md)),
            (false, true) => Ok(Some(org)),
            (false, false) => Ok(self
                .graph_text_find_entry(write, &name, PageKind::Page)?
                .map(|entry| entry.path)),
        }
    }

    fn hls_page_path(
        &self,
        write: &GraphTextWritePermit,
        pdf_filename: &str,
        key: &str,
    ) -> io::Result<PathBuf> {
        if let Some(existing) = self.existing_hls_page_path(write, key)? {
            return Ok(existing);
        }
        // A key migration renames the annotation page but must not implicitly
        // convert its syntax because the graph's preference changed meanwhile.
        let legacy_key = crate::pdf::legacy_asset_key(pdf_filename);
        if legacy_key != key && !self.retained_asset_key_in_use_by_pdf(write, &legacy_key)? {
            if let Some(legacy) = self.existing_hls_page_path(write, &legacy_key)? {
                let ext = legacy
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or("md");
                return Ok(legacy.with_file_name(format!(
                    "{}.{}",
                    crate::pdf::hls_page_name(key),
                    ext
                )));
            }
        }
        Ok(self.pages_path().join(format!(
            "{}.{}",
            crate::pdf::hls_page_name(key),
            self.preferred_format().ext()
        )))
    }

    fn pdf_sidecar_for_update(&self, pdf_filename: &str) -> io::Result<PathBuf> {
        let key = crate::pdf::asset_key(pdf_filename);
        let primary = self.assets_path().join(format!("{key}.edn"));
        if primary.exists() {
            return Ok(primary);
        }
        let legacy_key = crate::pdf::legacy_asset_key(pdf_filename);
        if legacy_key != key && !self.asset_key_in_use_by_pdf(&legacy_key) {
            let legacy = self.assets_path().join(format!("{legacy_key}.edn"));
            if legacy.exists() {
                return Ok(legacy);
            }
        }
        Ok(primary)
    }

    /// Open-time OG artifact initialization plus the persisted PDF state. Existing
    /// sidecars/pages are read without being rewritten; only missing artifacts are
    /// created. Old Tine-key artifacts remain in place until the established
    /// edit-time migration path can carry their notes forward safely.
    pub fn open_pdf(&self, pdf_filename: &str, label: &str) -> io::Result<crate::pdf::PdfState> {
        let write = self.admit_graph_text_writer()?;
        // Gate order (storage-sync-contract §3 invariant 9): the graph-global
        // identity gate is taken before any page lock. Inverting it deadlocks
        // every graph-text write in the process, not just this page.
        let _identity = self.lock_graph_text_identity_mutation()?;
        let key = crate::pdf::asset_key(pdf_filename);
        let page_path = self.hls_page_path(&write, pdf_filename, &key)?;
        let page_lock = self.page_lock(&page_path);
        let _guard = page_lock.lock().unwrap();
        let state = self.open_pdf_asset_only(pdf_filename)?;

        // Do not create a new-key page on top of an unmigrated legacy page: the
        // normal highlight write carries its notes forward under one guarded merge.
        let legacy_key = crate::pdf::legacy_asset_key(pdf_filename);
        let legacy_page_exists = legacy_key != key
            && !self.retained_asset_key_in_use_by_pdf(&write, &legacy_key)?
            && self.existing_hls_page_path(&write, &legacy_key)?.is_some();
        let page_baseline = self.graph_text_read_optional_text(&write, &page_path)?;
        if page_baseline.is_none() && !legacy_page_exists {
            let format = Format::from_path(&page_path);
            let page_doc = crate::pdf::hls_page_document_for_format(
                pdf_filename,
                label,
                &state.highlights,
                format,
            );
            let content = serialize_pdf_hls_page(&page_path, &page_doc, None)?;
            let page_rev = self.commit_editor_write(
                &write, &page_path, &content, None, true, None, None, None, None,
            )?;
            let name = crate::pdf::hls_page_name(&key);
            let entry = PageEntry {
                name,
                kind: PageKind::Page,
                date_key: None,
                rel_path: self.rel_path(&page_path),
                path: page_path.clone(),
            };
            self.cache_upsert(entry, page_doc, page_rev.clone());
            self.drop_self_write_marker(&page_path, &page_rev);
        }
        Ok(state)
    }

    /// Initialize/read only OG's asset-side PDF sidecar. The caller owns the HLS
    /// graph page.
    pub(crate) fn open_pdf_asset_only(
        &self,
        pdf_filename: &str,
    ) -> io::Result<crate::pdf::PdfState> {
        fs::create_dir_all(self.assets_path())?;
        let sidecar_path = self.pdf_sidecar_for_update(pdf_filename)?;
        self.ensure_asset_write_target(&sidecar_path)?;
        let mut sidecar = read_optional_text(&sidecar_path)?;
        if let Some(raw) = &sidecar {
            validate_highlight_edn(raw)?;
        } else {
            let skeleton = crate::pdf::write_highlights(&[], "");
            // Recheck immediately before publish so an external creator wins.
            if let Some(external) = read_optional_text(&sidecar_path)? {
                validate_highlight_edn(&external)?;
                sidecar = Some(external);
            } else {
                match atomic_write_new(&sidecar_path, skeleton.as_bytes()) {
                    Ok(()) => sidecar = Some(skeleton),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        let external = read_optional_text(&sidecar_path)?.ok_or(error)?;
                        validate_highlight_edn(&external)?;
                        sidecar = Some(external);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(crate::pdf::parse_pdf_state(
            sidecar.as_deref().unwrap_or(""),
        ))
    }

    /// Persist only OG's last-view page/scale fields. The hls-page lock is shared
    /// with highlight writes so an in-app highlight update cannot race this
    /// read-modify-write; external writers are handled by the same bounded
    /// compare/retry discipline.
    pub fn write_pdf_view_state(
        &self,
        pdf_filename: &str,
        page: i64,
        scale: f64,
    ) -> io::Result<()> {
        let write = self.admit_graph_text_writer()?;
        let key = crate::pdf::asset_key(pdf_filename);
        let page_path = self.hls_page_path(&write, pdf_filename, &key)?;
        let lock = self.page_lock(&page_path);
        let _guard = lock.lock().unwrap();
        self.write_pdf_view_state_sidecar(pdf_filename, page, scale)
    }

    fn write_pdf_view_state_sidecar(
        &self,
        pdf_filename: &str,
        page: i64,
        scale: f64,
    ) -> io::Result<()> {
        fs::create_dir_all(self.assets_path())?;
        let sidecar_path = self.pdf_sidecar_for_update(pdf_filename)?;
        self.ensure_asset_write_target(&sidecar_path)?;
        for _attempt in 0..4 {
            let baseline = read_optional_text(&sidecar_path)?;
            if let Some(raw) = &baseline {
                validate_highlight_edn(raw)?;
            }
            let next =
                crate::pdf::write_pdf_view_state(baseline.as_deref().unwrap_or(""), page, scale)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid PDF view state")
                    })?;
            if read_optional_text(&sidecar_path)? != baseline {
                continue;
            }
            let publish = if baseline.is_none() {
                atomic_write_new(&sidecar_path, next.as_bytes())
            } else {
                atomic_write(&sidecar_path, next.as_bytes())
            };
            match publish {
                Ok(()) => return Ok(()),
                Err(error)
                    if baseline.is_none() && error.kind() == io::ErrorKind::AlreadyExists =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "highlight sidecar changed repeatedly during view-state update",
        ))
    }

    fn asset_key_in_use_by_pdf(&self, candidate_key: &str) -> bool {
        let Ok(entries) = fs::read_dir(self.assets_path()) else {
            return false;
        };
        entries.flatten().any(|entry| {
            let filename = entry.file_name();
            let Some(filename) = filename.to_str() else {
                return false;
            };
            if !filename.ends_with(".pdf") && !filename.ends_with(".PDF") {
                return false;
            }
            crate::pdf::asset_key(filename) == candidate_key
        })
    }

    /// Read the PDF-name collision input that can select an HLS page from
    /// retained A as well. Internal `assets/` is enumerated through the writer's
    /// retained graph capability, so a replacement B cannot suppress or trigger
    /// legacy HLS migration. An explicitly approved external assets root remains
    /// under its separate authority and is not rebound through the graph permit.
    fn retained_asset_key_in_use_by_pdf(
        &self,
        write: &GraphTextWritePermit,
        candidate_key: &str,
    ) -> io::Result<bool> {
        if self.assets_root != self.root.join("assets") {
            return Ok(self.asset_key_in_use_by_pdf(candidate_key));
        }
        let sentinel = self.root.join("assets/.tine-capability-inventory");
        let target = match self.graph_text_target(write, &sentinel, false) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        for entry in target.parent().entries()? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let filename = entry.file_name();
            let Some(filename) = filename.to_str() else {
                continue;
            };
            if !filename.ends_with(".pdf") && !filename.ends_with(".PDF") {
                continue;
            }
            if crate::pdf::asset_key(filename) == candidate_key {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Merge and publish only the asset-side PDF highlight sidecar. The caller
    /// holds its HLS page lock and must either commit the paired HLS page and
    /// call `finish_...`, or reject the page transaction and call `rollback_...`.
    fn commit_highlight_sidecar_asset_only(
        &self,
        pdf_filename: &str,
        highlights: &[crate::pdf::Highlight],
        base_ids: &[String],
        legacy_active: bool,
    ) -> io::Result<PdfHighlightSidecarCommit> {
        let key = crate::pdf::asset_key(pdf_filename);
        let legacy_key = crate::pdf::legacy_asset_key(pdf_filename);
        let legacy_edn = (legacy_active && legacy_key != key)
            .then(|| self.assets_path().join(format!("{legacy_key}.edn")));
        fs::create_dir_all(self.assets_path())?;
        let edn_path = self.assets_path().join(format!("{key}.edn"));
        self.ensure_asset_write_target(&edn_path)?;
        let base: std::collections::HashSet<&str> = base_ids.iter().map(String::as_str).collect();
        for _attempt in 0..4 {
            let primary_baseline = read_optional_text(&edn_path)?;
            let legacy_baseline = if primary_baseline.is_none() {
                match &legacy_edn {
                    Some(path) => read_optional_text(path)?,
                    None => None,
                }
            } else {
                None
            };
            let existing_edn = primary_baseline.as_ref().or(legacy_baseline.as_ref());
            if let Some(raw) = existing_edn {
                validate_highlight_edn(raw)?;
            }
            let disk_highlights = existing_edn
                .map(|raw| crate::pdf::parse_highlights(raw))
                .unwrap_or_default();
            let have: std::collections::HashSet<&str> = highlights
                .iter()
                .map(|highlight| highlight.id.as_str())
                .collect();
            let mut merged = highlights.to_vec();
            for highlight in &disk_highlights {
                if !have.contains(highlight.id.as_str()) && !base.contains(highlight.id.as_str()) {
                    merged.push(highlight.clone());
                }
            }
            let merged_ids: std::collections::HashSet<&str> = merged
                .iter()
                .map(|highlight| highlight.id.as_str())
                .collect();
            let deleted_areas = disk_highlights
                .into_iter()
                .filter(|highlight| {
                    highlight.image.is_some() && !merged_ids.contains(highlight.id.as_str())
                })
                .collect();
            let area_source_key = if primary_baseline.is_none() && legacy_baseline.is_some() {
                legacy_key.clone()
            } else {
                key.clone()
            };
            let committed =
                crate::pdf::write_highlights(&merged, existing_edn.map_or("", String::as_str));
            let primary_now = read_optional_text(&edn_path)?;
            let legacy_now = if primary_now.is_none() && primary_baseline.is_none() {
                match &legacy_edn {
                    Some(path) => read_optional_text(path)?,
                    None => None,
                }
            } else {
                None
            };
            if primary_now != primary_baseline
                || (primary_baseline.is_none() && legacy_now != legacy_baseline)
            {
                continue;
            }
            let publish = if primary_baseline.is_none() {
                atomic_write_new(&edn_path, committed.as_bytes())
            } else {
                atomic_write(&edn_path, committed.as_bytes())
            };
            match publish {
                Ok(()) => {
                    return Ok(PdfHighlightSidecarCommit {
                        legacy_key,
                        edn_path,
                        legacy_edn,
                        merged,
                        primary_baseline,
                        legacy_baseline,
                        committed,
                        area_source_key,
                        deleted_areas,
                    })
                }
                Err(error)
                    if primary_baseline.is_none()
                        && error.kind() == io::ErrorKind::AlreadyExists =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "highlight sidecar changed repeatedly during update",
        ))
    }

    pub(crate) fn rollback_highlight_sidecar_commit(
        &self,
        receipt: &PdfHighlightSidecarCommit,
    ) -> io::Result<()> {
        self.rollback_highlight_sidecar(
            &receipt.edn_path,
            receipt.primary_baseline.as_deref(),
            &receipt.committed,
        )
    }

    pub(crate) fn finish_highlight_sidecar_commit(
        &self,
        receipt: &PdfHighlightSidecarCommit,
    ) -> io::Result<()> {
        let source_sidecar_guard = (receipt.area_source_key == receipt.legacy_key)
            .then(|| receipt.legacy_edn.as_deref())
            .flatten()
            .zip(receipt.legacy_baseline.as_deref());
        self.trash_deleted_pdf_area_images(
            &receipt.area_source_key,
            &receipt.edn_path,
            &receipt.committed,
            source_sidecar_guard,
            &receipt.deleted_areas,
        );
        if let (Some(path), Some(baseline)) = (&receipt.legacy_edn, &receipt.legacy_baseline) {
            if read_optional_text(path)?.as_ref() == Some(baseline) {
                let trash = typed_trash_dir(&self.root, TrashEntryKind::Conflict);
                self.ensure_trash_write_target(&trash)?;
                fs::create_dir_all(&trash)?;
                let name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("legacy.edn");
                let destination = trash.join(format!("{}__legacy__{name}", trash_stamp()));
                if move_file_noreplace(path, &destination).is_ok()
                    && read_optional_text(&destination)?.as_ref() != Some(baseline)
                {
                    let _ = move_file_noreplace(&destination, path);
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "legacy highlight sidecar changed during migration cleanup",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Persist highlights: write `assets/<key>.edn` and the `hls__<key>` page.
    /// `base_ids` are the highlight ids the editor LOADED (its baseline) — used for
    /// a 3-way merge so a highlight the user deleted is honored while one added
    /// externally (e.g. by OG between load and write) is still preserved.
    pub fn write_highlights(
        &self,
        pdf_filename: &str,
        label: &str,
        highlights: &[crate::pdf::Highlight],
        base_ids: &[String],
    ) -> io::Result<()> {
        let write = self.admit_graph_text_writer()?;
        // Gate order (storage-sync-contract §3 invariant 9): the graph-global
        // identity gate is taken before any page lock. Inverting it deadlocks
        // every graph-text write in the process, not just this page.
        let _identity = self.lock_graph_text_identity_mutation()?;
        let key = crate::pdf::asset_key(pdf_filename);
        // Legacy (pre-launch) key. When it differs and only the legacy files
        // exist, we read those as the baseline and migrate them to the new key
        // below — so the key change never strands existing highlights.
        let legacy_key = crate::pdf::legacy_asset_key(pdf_filename);
        let legacy_active =
            legacy_key != key && !self.retained_asset_key_in_use_by_pdf(&write, &legacy_key)?;
        let legacy_page = if legacy_active {
            self.existing_hls_page_path(&write, &legacy_key)?
        } else {
            None
        };
        // Serialize against an editor save of the SAME `hls__` page (see
        // `page_locks`): hold the page lock across the .edn merge AND the page
        // read→merge→write→cache_upsert, so the two writers can't clobber each
        // other or trip a false self-write conflict.
        let page_path = self.hls_page_path(&write, pdf_filename, &key)?;
        let lock = self.page_lock(&page_path);
        let _guard = lock.lock().unwrap();
        // Read every artifact that will participate before committing either one.
        // If the notes page (or its legacy source) is unreadable, abort while the
        // sidecar is still untouched rather than leaving a half-updated pair.
        let page_baseline = self.graph_text_read_optional_text(&write, &page_path)?;
        let legacy_page_baseline = if page_baseline.is_none() {
            match &legacy_page {
                Some(path) => self.graph_text_read_optional_text(&write, path)?,
                None => None,
            }
        } else {
            None
        };
        let existing_raw = page_baseline
            .clone()
            .or_else(|| legacy_page_baseline.clone());
        // VCS merge-conflict quarantine (Concord invariant 3) — the same
        // refusal the ordinary save path enforces in `serialize_page_dto_for_path`.
        // This path used to bypass it: one added highlight rewrote a
        // conflicted `hls__` page, re-indented the markers off column 0, and
        // thereby silently LIFTED the quarantine while the VCS still
        // considered the merge unresolved. Refuse before the sidecar commit so
        // the pair stays untouched.
        if let Some(existing) = existing_raw.as_deref() {
            let markers = doc::vcs_conflict_markers(existing);
            if !markers.is_empty() {
                return Err(projection_semantic_refusal(
                    io::ErrorKind::InvalidData,
                    format!(
                        "highlight page contains unresolved VCS merge conflict markers ({}) — resolve the merge first; Tine never rewrites a conflicted file",
                        markers.join(", ")
                    ),
                ));
            }
        }
        // The sidecar and annotation page are one logical update. Reject a
        // non-round-trippable Org page before publishing the sidecar so a failed
        // page serialization cannot leave the pair half-updated.
        if Format::from_path(&page_path) == Format::Org
            && existing_raw
                .as_deref()
                .is_some_and(|raw| !crate::org::org_editable(raw))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "org highlight page is read-only (does not round-trip)",
            ));
        }
        let sidecar = self.commit_highlight_sidecar_asset_only(
            pdf_filename,
            highlights,
            base_ids,
            legacy_active,
        )?;

        // Upsert into the existing hls page, preserving note children by id.
        // (`page_path` + its lock were taken at the top of this fn.) Prefer the
        // new-key page; fall back to the legacy-key page so its user notes are
        // carried over during migration.
        // The hls page's OWN bytes are its write baseline; the legacy fallback is
        // used only as a migration merge source. The A3-style recheck below
        // compares the page against this baseline.
        let existing = existing_raw
            .as_deref()
            .map(|raw| parse_doc(&page_path, raw));
        let page_doc = crate::pdf::merge_hls_page_for_format(
            existing.as_ref(),
            pdf_filename,
            label,
            sidecar.merged(),
            Format::from_path(&page_path),
        );
        // Preserve the notes page's CRLF (shared with write_page), then go through
        // the shared write commit (self-write marker → A3 recheck vs `page_baseline`
        // → atomic_write). The recheck is mandatory here precisely because this path
        // lacked save_page's guard: a non-cooperating external writer (OG / Syncthing)
        // could have added a note between `page_baseline` and now, and merge_hls_page
        // only carried notes from the bytes we read — so an overwrite would clobber it.
        // On mismatch → conflict; PdfViewer.persist toasts + reverts and a retry merges
        // cleanly (the .edn was already 3-way-merged, so no highlight is lost).
        let page_md = serialize_pdf_hls_page(&page_path, &page_doc, existing_raw.as_deref())?;
        // No-op save (write_page's guard, which this path lacked): a re-save of
        // an unchanged highlight set produces the page's exact current bytes.
        // Committing them anyway rewrote the file, stamped a watcher
        // suppression marker and woke every sync tool watching the tree for
        // nothing — Concord invariant 4. Skip the write; hash the bytes already
        // on disk for the rev the cache records.
        let page_changed = page_baseline.as_deref() != Some(page_md.as_str());
        let page_rev = if !page_changed {
            content_rev(&page_md)
        } else {
            match self.commit_editor_write(
                &write,
                &page_path,
                &page_md,
                page_baseline.as_deref(),
                true,
                None,
                None,
                None,
                None,
            ) {
                Ok(rev) => rev,
                Err(page_error) => {
                    if let Err(rollback_error) = self.rollback_highlight_sidecar_commit(&sidecar) {
                        return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!(
                            "highlight notes page was not saved ({page_error}); the sidecar rollback also failed ({rollback_error})"
                        ),
                    ));
                    }
                    return Err(page_error);
                }
            }
        };
        // The hls page is a real page; reflect it in the search cache.
        let name = crate::pdf::hls_page_name(&key);
        let entry = self
            .graph_text_find_entry(&write, &name, PageKind::Page)?
            .unwrap_or(PageEntry {
                name,
                kind: PageKind::Page,
                date_key: None,
                rel_path: self.rel_path(&page_path),
                path: page_path.clone(),
            });
        self.cache_upsert(entry, page_doc, page_rev.clone());
        // Drop the self-write marker now the write is published + cached (see
        // write_page / drop_self_write_marker). A no-op save took no marker.
        if page_changed {
            self.drop_self_write_marker(&page_path, &page_rev);
        }
        self.finish_highlight_sidecar_commit(&sidecar)?;
        // Migrate-on-write cleanup is compare-and-recover: only retire a legacy
        // artifact if it still equals the exact bytes we merged. A concurrent
        // legacy update stays at its original path. Unchanged files are moved to
        // recoverable trash rather than hard-deleted.
        if let (Some(path), Some(baseline)) = (&legacy_page, &legacy_page_baseline) {
            if self.graph_text_read_optional_text(&write, path)?.as_ref() == Some(baseline) {
                // Create the trash directory only when something is actually
                // going into it. Unconditionally mkdir-ing it made every
                // highlight save materialize `logseq/.tine-trash/conflict/` in a
                // tree that may never need it (invariant 4).
                let trash = typed_trash_dir(&self.root, TrashEntryKind::Conflict);
                self.graph_text_create_dir_all(&write, &trash)?;
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("legacy.md");
                let dest = trash.join(format!("{}__legacy__{name}", trash_stamp()));
                if self.graph_text_move_noreplace(&write, path, &dest).is_ok() {
                    if self.graph_text_read_optional_text(&write, &dest)?.as_ref() != Some(baseline)
                    {
                        let _ = self.graph_text_move_noreplace(&write, &dest, path);
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "legacy highlight page changed during migration cleanup",
                        ));
                    }
                    self.cache_remove(
                        &crate::pdf::hls_page_name(&legacy_key),
                        PageKind::Page,
                        None,
                    );
                }
            }
        }
        Ok(())
    }

    fn rollback_highlight_sidecar(
        &self,
        path: &Path,
        baseline: Option<&str>,
        committed: &str,
    ) -> io::Result<()> {
        if read_optional_text(path)?.as_deref() != Some(committed) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "highlight sidecar changed after its commit",
            ));
        }
        if let Some(previous) = baseline {
            return atomic_write(path, previous.as_bytes());
        }

        let trash = typed_trash_dir(&self.root, TrashEntryKind::Conflict);
        self.ensure_trash_write_target(&trash)?;
        fs::create_dir_all(&trash)?;
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("highlights.edn");
        let destination = trash.join(format!("{}__failed-highlight-pair__{name}", trash_stamp()));
        move_file_noreplace(path, &destination)?;
        if read_optional_text(&destination)?.as_deref() == Some(committed) {
            return Ok(());
        }

        let _ = move_file_noreplace(&destination, path);
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "highlight sidecar changed during rollback",
        ))
    }
}
