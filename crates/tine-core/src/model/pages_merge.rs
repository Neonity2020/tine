//! Graph's page-merge surface: merging one page into another, renaming a file
//! to a page, page paths, and create-if-absent for pages and assets.

use super::*;
use crate::direct_projection::PageSetChange;

impl Graph {
    /// Reconcile a duplicate-day pair: append every block of `src_rel` to the end of
    /// `dst_rel`, then move `src_rel` to the recoverable trash (#21). Both must be
    /// real graph text files of the SAME format (we don't transcode md⇄org), and an
    /// org file that can't be round-tripped is refused so the merge can never
    /// corrupt it (both files are left untouched on any error). `src`'s page
    /// PROPERTIES that `dst` doesn't already define are carried into `dst` (md only;
    /// dst wins on a clash) so an alias/tags/icon isn't silently lost; src free-text
    /// in the pre-block is dropped. The src is trashed ONLY after `dst` is durably
    /// written.
    pub fn merge_pages(&self, src_rel: &str, dst_rel: &str) -> io::Result<()> {
        let write = self.admit_graph_text_writer()?;
        let _identity = self.lock_graph_text_identity_mutation()?;
        let src = self
            .resolve_graph_text_rel(&write, src_rel)?
            .ok_or_else(bad_path)?;
        let dst = self
            .resolve_graph_text_rel(&write, dst_rel)?
            .ok_or_else(bad_path)?;
        if src == dst {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot merge a file into itself",
            ));
        }
        if Format::from_path(&src) != Format::from_path(&dst) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "files are in different formats",
            ));
        }
        let dst_entry = self.entry_for_path(&dst).ok_or_else(bad_path)?;
        // Write `dst` under its page lock so a concurrent editor/PDF write can't
        // race the merge; read both files inside the lock (the dst baseline must be
        // current for write_page's recheck).
        let lock = self.page_lock(&dst);
        let _guard = lock.lock().unwrap();
        let src_content = self.graph_text_read_to_string(&write, &src)?;
        let dst_content = self.graph_text_read_to_string(&write, &dst)?;
        if Format::from_path(&dst) == Format::Org
            && (!crate::org::org_editable(&dst_content) || !crate::org::org_editable(&src_content))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "an org file in this pair does not round-trip; not merging",
            ));
        }
        let src_doc = parse_doc(&src, &src_content);
        let mut merged = parse_doc(&dst, &dst_content);
        // Preserve src's page PROPERTIES that dst doesn't already define
        // (alias::/tags::/icon::/…). Dropping them silently is real data loss — a
        // lost `alias::` breaks every inbound link that used the alias. dst's value
        // wins on a key clash (no duplicate property line); markdown only (org
        // pre-blocks are header/drawer-structured and gated by the round-trip
        // firewall, so we don't risk a non-round-tripping merge there). Free text in
        // src's pre-block is still dropped — rare, and src is trashed-recoverable.
        if Format::from_path(&dst) == Format::Md {
            if let Some(src_pre) = src_doc.pre_block.as_deref() {
                let dst_pre = merged.pre_block.clone().unwrap_or_default();
                let dst_keys: std::collections::HashSet<String> = dst_pre
                    .lines()
                    .filter_map(|l| {
                        doc::parse_property_line(l).map(|(k, _)| k.to_ascii_lowercase())
                    })
                    .collect();
                let extra: Vec<&str> = src_pre
                    .lines()
                    .filter(|l| {
                        doc::parse_property_line(l)
                            .is_some_and(|(k, _)| !dst_keys.contains(&k.to_ascii_lowercase()))
                    })
                    .collect();
                if !extra.is_empty() {
                    let mut pre = dst_pre;
                    if !pre.is_empty() && !pre.ends_with('\n') {
                        pre.push('\n');
                    }
                    pre.push_str(&extra.join("\n"));
                    merged.pre_block = Some(pre);
                }
            }
        }
        merged.roots.extend(src_doc.roots);
        assign_doc_runtime_ids(&mut merged.roots, &dst_entry.rel_path);
        let dto = page_dto_checked(&dst_entry, &merged)?;
        let dst_cacheable = self.graph_text_path_is_cacheable(&write, &dst)?;
        // L5: stage `src` into the trash BEFORE committing the merged `dst`. The old
        // order (write dst, then trash src) duplicated blocks on a retry when
        // trashing failed: dst already held src's blocks while src survived on disk,
        // so a second merge re-appended them. Now we move src out first — a staging
        // failure aborts the merge cleanly before any write — and if the dst write
        // then fails we roll the move back, so neither the merge nor the source is
        // lost. On success src sits in the recoverable trash.
        // Retained for the projection: once the merge commits, this page's file
        // is in the trash and its rows must go with it.
        let src_entry = self.entry_for_path(&src);
        let trash = typed_trash_dir(
            &self.root,
            match src_entry.as_ref().map(|e| e.kind) {
                Some(PageKind::Journal) => TrashEntryKind::Journal,
                _ => TrashEntryKind::Page,
            },
        );
        self.graph_text_create_dir_all(&write, &trash)?;
        let src_name = src.file_name().and_then(|s| s.to_str()).unwrap_or("file");
        let staged = trash.join(format!("{}__{src_name}", trash_stamp()));
        self.graph_text_move_noreplace(&write, &src, &staged)?;
        if self.graph_text_read_to_string(&write, &staged)? != src_content {
            let _ = self.graph_text_move_noreplace(&write, &staged, &src);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "source changed during merge",
            ));
        }
        // Retire the source the moment its FILE leaves the graph, before the
        // destination is rewritten — not after. Published the other way round,
        // the destination's own `cache_upsert` can be drained on its own and the
        // worker announces a complete, ready index that still contains a page
        // whose file is already in the trash (fifth audit A5-N4). This order has
        // no such state: every image published between these two points is one
        // that actually existed on disk — the source gone, the destination not
        // yet merged.
        if let Some(entry) = src_entry.clone() {
            self.cache_remove_path(&entry);
        }
        // The page lock excludes other Tine writers, but not Logseq/Syncthing.
        // Recheck the baseline at commit so an external edit arriving after our
        // read is not silently overwritten.
        if let Err(e) = self.write_page(
            &write,
            &dto,
            &dst,
            Some(&dst_content),
            true,
            None,
            None,
            None,
            dst_cacheable,
        ) {
            let _ = graph_text_write_during_rollback_hook();
            let _ = self.graph_text_move_noreplace(&write, &staged, &src);
            // The rollback puts the source file back, so its retirement has to
            // come back with it — otherwise a failed merge leaves the index
            // missing a page that exists, with nothing queued to notice.
            // Republished from the exact bytes verified on disk above, so this
            // compensation needs no read that could fail in turn.
            //
            // What goes back into the index is what is ON DISK at that path
            // now — not what we believe the restore did, and not the bytes we
            // read before the merge (seventh audit A7-N1). Two things make the
            // belief wrong in opposite directions: `move_noreplace` renames
            // FIRST and then does fallible durability and identity work, so an
            // `Err` can mean "restored, then a later step failed"; and it
            // refuses a path something else now owns, where the occupant is
            // the honest current content of that path and needs indexing just
            // as much. Reading once answers both, and keeps the rule the fifth
            // and sixth audits arrived at: every published image is one that
            // actually existed on disk.
            if let Some(entry) = src_entry {
                match self.graph_text_read_optional_text(&write, &src) {
                    Ok(Some(current)) => {
                        if let Ok((entry, document, revision)) =
                            parse_exact_page(self, &entry, &current)
                        {
                            self.cache_upsert(entry, document, revision);
                        }
                    }
                    // Genuinely gone: the retirement already published is the
                    // truth, and standing is where it belongs.
                    Ok(None) => {}
                    // The state cannot be established. Leaving the retirement
                    // standing publishes "absent", which is wrong only if the
                    // file is there — and this merge is returning an error to
                    // the user either way; a later reconcile owns it.
                    Err(_) => {}
                }
            }
            return Err(e);
        }
        // GH #543: the merged destination publishes itself through `write_page`,
        // but nothing retired the source — so search kept returning BOTH pages,
        // with the index `ready` and claiming to be complete, while the source's
        // file sat in the trash. A successful answer never reaches the repair a
        // refusal would start, so the ghost never went away (fourth audit A4-N1).
        //
        // Deleting the source's ROWS is not enough, and the first fix here did
        // only that (fifth audit A5-N3). Unlike every other page-set mutation in
        // this file, a merge does NOT invalidate the parsed whole-graph cache —
        // it publishes one destination through `cache_upsert` — so the source
        // stayed in that cache. The parsed cache is an authoritative snapshot
        // producer: a query read failure makes the repair path reset the index
        // and republish it at the CURRENT generation, walking the merged-away
        // page straight back into search, past the older-generation guard, which
        // has nothing to object to. `cache_remove_path` is the blessed one-page
        // retirement — parsed cache, revisions, every derived index, the session
        // ids and the projection delete, in one generation. It runs above,
        // before the destination write, for the reason A5-N4 gives there.
        Ok(())
    }

    /// Logseq-compatible title collision: merge the old page into the existing
    /// destination, then rewrite old-name references (and rename old/* namespace
    /// descendants) exactly as an ordinary rename does. The merge is committed
    /// first so an interruption can never delete or overwrite user content; its
    /// source remains recoverable in typed trash. A later reference-rewrite error
    /// is reported rather than hidden, and retrying `rename_page_expected` is safe
    /// because the source file has already left the live graph.
    pub fn merge_pages_after_rename(
        &self,
        src_rel: &str,
        dst_rel: &str,
        old: &str,
        new: &str,
    ) -> io::Result<()> {
        self.merge_pages(src_rel, dst_rel)?;
        self.rename_page_expected(old, new, None)
    }

    /// Turn a stray file into a normal, uniquely-named page by moving it to
    /// `pages/<encoded new_name>.<its ext>` (#21) — the way to rescue a duplicate-day
    /// leftover whose name collides with the canonical day. Refuses if a page for
    /// `new_name` already exists in any supported text extension (never clobbers)
    /// or another graph-text file already owns that logical page identity. Inbound
    /// references are NOT rewritten (a stray rarely has any); the file's own
    /// content is unchanged.
    pub fn rename_file_to_page(&self, src_rel: &str, new_name: &str) -> io::Result<()> {
        let write = self.admit_graph_text_writer()?;
        let _identity = self.lock_graph_text_identity_mutation()?;
        let page_inventory_snapshot = self.current_page_inventory_snapshot();
        let src = self
            .resolve_graph_text_rel(&write, src_rel)?
            .ok_or_else(bad_path)?;
        let name = new_name.trim();
        if name.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty page name",
            ));
        }
        let ext = text_extension_from_path(&src)
            .map(|extension| extension.to_owned())
            .ok_or_else(bad_path)?;
        let dir = self.pages_path();
        let mut existing_identity = self.retained_legacy_page_identity_exists(&write, name)?;
        // Historical page filenames can be non-portable and therefore cannot
        // enter the strict graph-text path admission index. Keep the retained
        // filename walk above as their compatibility authority, then use the
        // exact no-follow reader and canonical parser for portable admitted
        // graph text so an explicit title cannot be rescued over.
        if !existing_identity {
            for entry in self.graph_text_entries(&write)? {
                if entry.path == src {
                    continue;
                }
                if GraphTextPath::parse(entry.rel_path.as_str()).is_err() {
                    continue;
                }
                let Some(incumbent) = self.load_validated_graph_text_target(&write, &entry.path)?
                else {
                    continue;
                };
                if incumbent.entry.kind == PageKind::Page
                    && crate::refs::same_page(&incumbent.entry.name, name)
                {
                    existing_identity = true;
                    break;
                }
            }
        }
        if existing_identity {
            return Err(DirectSaveError::into_io(
                DirectSaveFailureCode::IdentityNameTaken,
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a page with that name already exists",
                ),
            ));
        }
        let enc = encode_page_name(name, self.config().file_name_format);
        for target in configured_text_variant_paths(&dir, &enc) {
            if self.graph_text_exists(&write, &target)? {
                return Err(DirectSaveError::into_io(
                    DirectSaveFailureCode::IdentityNameTaken,
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "a page with that name already exists",
                    ),
                ));
            }
        }
        self.graph_text_create_dir_all(&write, &dir)?;
        let dst = dir.join(format!("{enc}.{ext}"));
        // Retained for the projection before the move takes the path away.
        let src_entry = self.entry_for_path(&src);
        self.graph_text_move_noreplace(&write, &src, &dst)?;
        // Reopen only the committed destination, not the graph: this binds the
        // inventory entry to the exact bytes that now own the new name even if an
        // external editor changed the retained source inode during the move.
        let effective = self
            .graph_text_read_to_string(&write, &dst)
            .ok()
            .and_then(|content| {
                let provisional = self.graph_inventory_entry(&dst).ok().flatten()?;
                parse_exact_page(self, &provisional, &content)
                    .ok()
                    .map(|(entry, _, _)| entry)
            });
        // The old path owns nothing now, and the moved file is as readable as
        // it was: recorded by path before the discard moves the generation.
        self.note_graph_text_state(&src, true);
        self.note_graph_text_state(&dst, effective.is_some());
        let updated_page_inventory = page_inventory_snapshot.map(|mut inventory| {
            inventory.retain(|entry| entry.path != src);
            inventory.extend(effective);
            inventory
        });
        // The parsed snapshot is invalidated because this rescue changes the
        // physical kind/path. Preserve the separately updated list memo when its
        // pre-transaction generation was current.
        *self.find_entry_cache.write().unwrap() = None;
        let coming = self.index_delta_coming();
        self.discard_parsed_cache(
            vec![src.clone(), dst.clone()],
            graph_drift::IndexEffect::Sent(&coming),
        );
        if let Some(inventory) = updated_page_inventory {
            self.publish_page_inventory_snapshot(inventory);
        }
        // GH #543: the rescue moved a page's file and told the index only that
        // something had changed. Nothing was queued, so search went on offering
        // the page under its OLD path and never found the rescued one — and
        // because those answers SUCCEEDED, no repair was ever started (fourth
        // audit A4-N1). Retire the old path and publish the new one together:
        // published separately, the queue can empty between them and readiness
        // is announced over a graph missing the page (A4-N2).
        let mut page_set = Vec::new();
        if let Some(entry) = src_entry {
            page_set.push(PageSetChange::Delete { entry });
        }
        let replacement = self
            .graph_text_read_to_string(&write, &dst)
            .ok()
            .and_then(|content| {
                let provisional = self.graph_inventory_entry(&dst).ok().flatten()?;
                parse_exact_page(self, &provisional, &content).ok()
            });
        match replacement {
            Some((entry, document, revision)) => page_set.push(PageSetChange::Replace {
                entry,
                document: Arc::new(document),
                revision,
            }),
            // Stale beats absent: without the replacement the old rows are the
            // only evidence this page exists, so leave them and let a later warm
            // reconcile. `page_index_failures` already names it.
            None => page_set.clear(),
        }
        self.direct_projection_publish_page_set(
            self.cache_gen.load(std::sync::atomic::Ordering::Acquire),
            page_set,
        );
        Ok(())
    }

    /// Resolve a page name to a file path. Journals match by date title;
    /// pages match by filename stem.
    pub(crate) fn path_for(&self, name: &str, kind: PageKind) -> PathBuf {
        let pref = self.preferred_format();
        match kind {
            PageKind::Journal => self
                .journals_desc()
                .into_iter()
                .find(|e| crate::refs::same_page(&e.name, name))
                .map(|e| e.path)
                .unwrap_or_else(|| {
                    // New journal: name it by its date stem in the graph's filename
                    // format ("2026_06_18.org"), not the display title — a
                    // title-named file can't be parsed back to a date, so
                    // journals_desc would drop it and the day would look empty. The
                    // extension follows the graph's :preferred-format.
                    let stem = self
                        .journal_format
                        .parse(name)
                        .map(|d| self.journal_format.file_stem(d))
                        .unwrap_or_else(|| name.to_string());
                    self.journals_path().join(format!("{stem}.{}", pref.ext()))
                }),
            PageKind::Page => {
                // Resolve to an EXISTING file (any format) so a save updates it in
                // place rather than creating a second file in the other extension;
                // a brand-new page is created in the graph's preferred format.
                // Cheap `exists()` probes (the common hit needs one), no dir scan.
                let enc = encode_page_name(name, self.config().file_name_format);
                let dir = self.pages_path();
                let primary = dir.join(format!("{enc}.{}", pref.ext()));
                if primary.exists() {
                    return primary;
                }
                let alt_ext = if pref == Format::Org { "md" } else { "org" };
                let alt = dir.join(format!("{enc}.{alt_ext}"));
                if alt.exists() {
                    return alt;
                }
                primary
            }
        }
    }

    /// Create a Markdown page file with `content` if that logical page does not
    /// already exist. Used by the explicit guide-copy action:
    /// it is intentionally raw Markdown, not a serialized DTO, so copied guide
    /// pages stay ordinary Logseq template pages byte-for-byte.
    ///
    /// Returns `true` when a file was created and `false` when an existing page
    /// won. Existing content is never overwritten.
    pub fn create_markdown_page_if_absent(&self, name: &str, content: &str) -> io::Result<bool> {
        let write = self.admit_graph_text_writer()?;
        let _identity = self.lock_graph_text_identity_mutation()?;
        if self
            .graph_text_find_entry(&write, name, PageKind::Page)?
            .is_some()
        {
            return Ok(false);
        }
        let path = self.pages_path().join(format!(
            "{}.md",
            encode_page_name(name, self.config().file_name_format)
        ));
        let filename_name = self
            .graph_entry_for_relative_path(&self.rel_path(&path))?
            .name;
        let content = if filename_name != name {
            bind_markdown_title_property(content, name)
        } else {
            content.to_owned()
        };
        let lock = self.page_lock(&path);
        let _guard = lock.lock().unwrap();
        let validation =
            self.validate_graph_text_target(&write, &path, Some((PageKind::Page, name)))?;
        if validation.target.is_some() || validation.requested_identity_elsewhere {
            return Ok(false);
        }
        let creation_proof = validation.creation_proof.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "missing guide target lacks its creation proof",
            )
        })?;
        if self
            .graph_text_find_entry(&write, name, PageKind::Page)?
            .is_some()
            || self.graph_text_exists(&write, &path)?
        {
            return Ok(false);
        }
        match self.graph_text_atomic_create_with_proof(
            &write,
            &path,
            content.as_bytes(),
            creation_proof,
            None,
        ) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(e),
        }
        guide_twin_race_hook(&path)?;
        let alt = self.pages_path().join(format!(
            "{}.org",
            encode_page_name(name, self.config().file_name_format)
        ));
        if self.graph_text_exists(&write, &alt)? {
            // An Org twin appeared during publication. Withdraw only the exact
            // guide inode we just created. Stage the currently named inode first
            // and verify it in recovery, so an external replacement that wins at
            // the syscall boundary is restored or retained rather than unlinked.
            let _ = self.withdraw_file_to_conflict_if_exact(
                &write,
                &path,
                content.as_bytes(),
                "guide-twin-withdrawal",
            )?;
            return Ok(false);
        }
        *self.page_list_cache.write().unwrap() = None;
        *self.find_entry_cache.write().unwrap() = None;
        let entry = self.graph_inventory_entry(&path)?.ok_or_else(bad_path)?;
        let revision = content_rev(&content);
        self.cache_upsert(entry, parse_doc(&path, &content), revision.clone());
        // This path publishes graph text without passing through
        // `commit_editor_write`, which normally records the exact final bytes
        // and physical identity for the native watcher. Without the equivalent
        // receipt here, the first page of a multi-page Guide copy looks like an
        // external creation and raises the graph-wide admission frontier before
        // the second page can be created (GH #391, and the same family as the
        // negative Windows follow-up in GH #374).
        let Some((reread, identity)) =
            self.graph_text_read_optional_text_with_identity(&write, &path)?
        else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "created guide page disappeared before final publication",
            ));
        };
        if reread != content || content_rev(&reread) != revision {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "created guide page changed before final publication",
            ));
        }
        self.loaded_file_identities
            .write()
            .unwrap()
            .insert(path.clone(), (revision.clone(), identity));
        self.remember_exact_graph_text_state(&path, revision, identity);
        Ok(true)
    }

    /// Create one named top-level asset without replacing an existing file. The
    /// approved asset capability is revalidated at the actual write target so a
    /// graph-directory symlink/junction swap cannot redirect this creation.
    pub(crate) fn create_asset_if_absent(&self, name: &str, bytes: &[u8]) -> io::Result<bool> {
        top_level_asset_name(name)?;
        let path = self.assets_path().join(name);
        self.ensure_asset_write_target(&path)?;
        fs::create_dir_all(self.assets_path())?;
        match atomic_write_new(&path, bytes) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    }
}
