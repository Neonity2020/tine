//! Graph's conflict surface: sync-service conflict copies, VCS merge-marker
//! conflicts, live-save conflicts (captured and durable), and duplicate journal
//! days: listing, diffing and resolving each.

use super::*;

impl Graph {
    /// Journal days that resolve to more than one file — the migration leaves these
    /// alone (it never clobbers), so they're reported for the user to reconcile.
    /// Each file gets a one-line preview and a `canonical` flag (date-stem name).
    pub fn journal_conflicts(&self) -> Vec<JournalConflict> {
        let dir = self.journals_path();
        let mut by_date: std::collections::BTreeMap<i64, Vec<(String, PathBuf, bool)>> =
            std::collections::BTreeMap::new();
        walk_page_files(&dir, |p| {
            let ext = match text_extension_from_path(&p) {
                Some(extension) => extension.to_owned(),
                None => return,
            };
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                return;
            };
            // A date-stem file is canonical; otherwise try to parse its title.
            let canonical = JournalDate::from_file_stem(stem).is_some();
            let date =
                JournalDate::from_file_stem(stem).or_else(|| self.journal_format.parse(stem));
            if let Some(d) = date {
                by_date.entry(d.ordinal_key()).or_default().push((
                    format!("{stem}.{ext}"),
                    p,
                    canonical,
                ));
            }
        });
        let mut out = Vec::new();
        for (key, files) in by_date {
            if files.len() < 2 {
                continue;
            }
            let date = JournalDate::from_ordinal(key);
            let mut jfiles: Vec<JournalFile> = files
                .into_iter()
                .map(|(name, path, canonical)| {
                    let preview = fs::read_to_string(&path)
                        .ok()
                        .and_then(|c| {
                            c.lines()
                                .map(|l| {
                                    l.trim_start_matches(|ch| {
                                        ch == '*' || ch == '-' || ch == ' ' || ch == '\t'
                                    })
                                    .trim()
                                    .to_string()
                                })
                                .find(|l| !l.is_empty())
                        })
                        .map(|l| l.chars().take(80).collect::<String>())
                        .unwrap_or_default();
                    let rel = self.rel_path(&path);
                    JournalFile {
                        name,
                        path: rel,
                        preview,
                        canonical,
                    }
                })
                .collect();
            // Canonical first (the keeper), then alphabetical.
            jfiles.sort_by(|a, b| {
                b.canonical
                    .cmp(&a.canonical)
                    .then_with(|| a.name.cmp(&b.name))
            });
            out.push(JournalConflict {
                title: self.journal_format.title(date),
                files: jfiles,
            });
        }
        out
    }

    /// Sync-tool conflict copies (`*.sync-conflict-*`, Dropbox `(conflicted copy)`)
    /// sitting in `journals/` or `pages/`. Each carries the winning page it shadows,
    /// that winner's path (if it still exists), a device/timestamp tag, and a
    /// one-line preview — everything the conflicts panel needs to offer a merge.
    /// These files are deliberately excluded from `list_pages`/the cache
    /// (see [`is_sync_conflict`]); this is the ONLY place they're surfaced.
    pub fn list_sync_conflicts(&self) -> Vec<SyncConflict> {
        let mut out = Vec::new();
        for (dir, kind) in [
            (self.journals_path(), PageKind::Journal),
            (self.pages_path(), PageKind::Page),
        ] {
            walk_page_files(&dir, |p| {
                let ext = match text_extension_from_path(&p) {
                    Some(extension) => extension,
                    None => return,
                };
                let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                    return;
                };
                let Some(base_stem) = sync_conflict_base(stem) else {
                    return;
                };
                // The winner it shadows: same dir, same extension, base stem.
                let base_file = p
                    .parent()
                    .unwrap_or(&dir)
                    .join(format!("{base_stem}.{ext}"));
                let base_path = base_file.is_file().then(|| self.rel_path(&base_file));
                let base_name = match kind {
                    PageKind::Journal => self
                        .journal_format
                        .parse(base_stem)
                        .map(|d| self.journal_format.title(d))
                        .unwrap_or_else(|| base_stem.to_string()),
                    PageKind::Page => decode_page_name(base_stem, self.config.file_name_format),
                };
                let tag = stem[base_stem.len()..]
                    .trim_matches(|c: char| c == '.' || c == ' ' || c == '(' || c == ')')
                    .to_string();
                let preview = fs::read_to_string(&p)
                    .ok()
                    .and_then(|c| {
                        c.lines()
                            .map(|l| {
                                l.trim_start_matches(|ch| {
                                    ch == '*' || ch == '-' || ch == ' ' || ch == '\t'
                                })
                                .trim()
                                .to_string()
                            })
                            .find(|l| !l.is_empty())
                    })
                    .map(|l| l.chars().take(80).collect::<String>())
                    .unwrap_or_default();
                out.push(SyncConflict {
                    path: self.rel_path(&p),
                    base_name,
                    base_path,
                    kind,
                    tag,
                    preview,
                });
            });
        }
        out.sort_by(|a, b| {
            a.base_name
                .cmp(&b.base_name)
                .then_with(|| a.path.cmp(&b.path))
        });
        out
    }

    /// Pages whose on-disk content carries unresolved VCS merge-conflict markers
    /// (git/Fossil; see [`crate::doc::vcs_conflict_markers`]). These are REAL
    /// pages — indexed, readable — but quarantined from saves (the serializer
    /// refuses to rewrite them; see `serialize_page_document`). Surfaced beside
    /// [`Graph::list_sync_conflicts`] for the conflicts panel. Sync-tool
    /// conflict copies are excluded here — they have their own listing.
    pub fn list_vcs_marker_conflicts(&self) -> Vec<VcsMarkerConflict> {
        let mut out = Vec::new();
        for (dir, kind) in [
            (self.journals_path(), PageKind::Journal),
            (self.pages_path(), PageKind::Page),
        ] {
            walk_page_files(&dir, |p| {
                let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                    return;
                };
                if is_sync_conflict(stem) {
                    return;
                }
                let Ok(content) = fs::read_to_string(&p) else {
                    return;
                };
                let markers = doc::vcs_conflict_markers(&content);
                if markers.is_empty() {
                    return;
                }
                let name = match kind {
                    PageKind::Journal => self
                        .journal_format
                        .parse(stem)
                        .map(|d| self.journal_format.title(d))
                        .unwrap_or_else(|| stem.to_string()),
                    PageKind::Page => decode_page_name(stem, self.config.file_name_format),
                };
                out.push(VcsMarkerConflict {
                    path: self.rel_path(&p),
                    name,
                    kind,
                    markers: markers.iter().map(|marker| marker.to_string()).collect(),
                });
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// Whether `path` is inside its one authorized marker-resolution write.
    pub(super) fn is_resolving_markers(&self, path: &Path) -> bool {
        self.marker_resolutions
            .lock()
            .map(|set| set.contains(path))
            .unwrap_or(false)
    }

    /// Authorize exactly one marker-resolution write to `path`, for the lifetime
    /// of the returned guard. Held only across the guarded write, under the
    /// page lock.
    fn authorize_marker_resolution(&self, path: &Path) -> MarkerResolutionGuard<'_> {
        if let Ok(mut set) = self.marker_resolutions.lock() {
            set.insert(path.to_path_buf());
        }
        MarkerResolutionGuard {
            graph: self,
            path: path.to_path_buf(),
        }
    }

    /// The Concord conflict queue (L3): ONE derived inventory of everything on
    /// disk that needs the user's judgement, from both artifact sources.
    ///
    /// Derived, never stored — no new metadata goes into the graph (invariant 1)
    /// and no cache is consulted, so the queue survives a restart trivially: the
    /// same on-disk state recomputes the same objects with the same ids. Block
    /// counts are computed here because conflicts are few (a handful at most) and
    /// each costs one parse of two small texts; the two directory walks the
    /// sources already do dominate.
    pub fn conflict_queue(&self) -> Vec<crate::concord_queue::ConflictObject> {
        use crate::concord_queue::{
            decidable_row_count, ConflictObject, ConflictSide, ConflictSource, SideRole,
        };
        let mut out = Vec::new();
        for copy in self.list_sync_conflicts() {
            let Some(winner) = copy.base_path.clone() else {
                // The page it shadowed is gone — it is a stray, not a two-sided
                // conflict; the Settings panel offers to discard it. Nothing to
                // resolve in place, so it stays out of the queue.
                continue;
            };
            let diff = self.sync_conflict_diff(&winner, &copy.path).ok().flatten();
            let mut sides = vec![
                ConflictSide {
                    role: SideRole::Mine,
                    label: "This device".to_string(),
                    path: Some(winner.clone()),
                },
                ConflictSide {
                    role: SideRole::Theirs,
                    label: if copy.tag.is_empty() {
                        "Conflict copy".to_string()
                    } else {
                        copy.tag.clone()
                    },
                    path: Some(copy.path.clone()),
                },
            ];
            if diff.as_ref().is_some_and(|d| d.three_way) {
                sides.push(ConflictSide {
                    role: SideRole::Base,
                    label: "Last agreed version".to_string(),
                    path: None,
                });
            }
            out.push(ConflictObject {
                id: format!("copy:{}", copy.path),
                source: ConflictSource::SyncCopy,
                page_name: copy.base_name.clone(),
                page_path: winner,
                kind: copy.kind,
                sides,
                block_conflicts: diff.as_ref().map(|d| decidable_row_count(&d.rows)),
                markers: Vec::new(),
            });
        }
        for marked in self.list_vcs_marker_conflicts() {
            let parsed = self.vcs_marker_conflict_diff(&marked.path).ok().flatten();
            let label = |pick: fn(&crate::concord_queue::MarkerConflictDiff) -> &str,
                         fallback: &str| {
                parsed
                    .as_ref()
                    .map(pick)
                    .filter(|l| !l.is_empty())
                    .unwrap_or(fallback)
                    .to_string()
            };
            let mut sides = vec![
                ConflictSide {
                    role: SideRole::Mine,
                    label: label(|p| p.mine_label.as_str(), "Local side"),
                    path: None,
                },
                ConflictSide {
                    role: SideRole::Theirs,
                    label: label(|p| p.theirs_label.as_str(), "Merged-in side"),
                    path: None,
                },
            ];
            if parsed.as_ref().is_some_and(|p| p.diff.three_way) {
                sides.push(ConflictSide {
                    role: SideRole::Base,
                    label: "Common ancestor".to_string(),
                    path: None,
                });
            }
            out.push(ConflictObject {
                id: format!("markers:{}", marked.path),
                source: ConflictSource::VcsMarkers,
                page_name: marked.name.clone(),
                page_path: marked.path.clone(),
                kind: marked.kind,
                sides,
                block_conflicts: parsed.as_ref().map(|p| decidable_row_count(&p.diff.rows)),
                markers: marked.markers.clone(),
            });
        }
        for day in self.journal_conflicts() {
            // `journal_conflicts` sorts canonical-first, so files[0] is the
            // keeper whether or not any file carries the canonical date stem.
            let mut files = day.files.iter();
            let Some(keeper) = files.next() else { continue };
            let strays: Vec<_> = files.collect();
            if strays.is_empty() {
                continue;
            }
            let mut sides = vec![ConflictSide {
                role: SideRole::Mine,
                label: keeper.name.clone(),
                path: Some(keeper.path.clone()),
            }];
            for stray in &strays {
                sides.push(ConflictSide {
                    role: SideRole::Theirs,
                    label: stray.name.clone(),
                    path: Some(stray.path.clone()),
                });
            }
            // A day with three or more files is resolved pairwise: the row
            // decisions belong to the keeper against the FIRST stray, and once
            // that stray is folded in the queue re-derives with one file fewer.
            let diff = self
                .duplicate_journal_diff(&keeper.path, &strays[0].path)
                .ok()
                .flatten();
            out.push(ConflictObject {
                id: format!("journal:{}", keeper.path),
                source: ConflictSource::DuplicateJournal,
                page_name: day.title.clone(),
                page_path: keeper.path.clone(),
                kind: PageKind::Journal,
                sides,
                // `None` where the pair cannot be merged at all (cross-format):
                // the file rows still work, but no row-by-row choice is offered.
                block_conflicts: diff.as_ref().map(|d| decidable_row_count(&d.rows)),
                markers: Vec::new(),
            });
        }
        out.sort_by(|a, b| a.page_name.cmp(&b.page_name).then_with(|| a.id.cmp(&b.id)));
        out
    }

    /// Block-level diff of a marker-bearing page's own two (or three) sides —
    /// Concord L5 completion. The marker sections are parsed into COMPLETE page
    /// texts (`concord_queue::parse_vcs_marker_sides`) and run through the very
    /// same `sync_diff` machinery the conflict-copy path uses, so the in-page
    /// resolution UI is one renderer, not two.
    ///
    /// When the markers also carried the merge tool's own `#######` SUGGESTED
    /// CONFLICT RESOLUTION sections, that fourth reconstruction rides along as
    /// the diff's ARTIFACT: rows the disjoint-edit merge declines may offer the
    /// tool's body instead (`MergedSource::Artifact`). It is still only a
    /// proposal — the resolve re-derives it from the same guarded bytes.
    ///
    /// Read-only. Both staleness tokens are the rev of the whole marker file, so
    /// [`Graph::resolve_vcs_marker_conflict`]'s guard rejects decisions made
    /// against a version the VCS has since changed. `Ok(None)` if the path is
    /// invalid, gone, or not conflicted.
    pub fn vcs_marker_conflict_diff(
        &self,
        rel: &str,
    ) -> io::Result<Option<crate::concord_queue::MarkerConflictDiff>> {
        let path = GraphTextPath::parse(rel.to_owned()).map_err(|_| bad_path())?;
        let Some(bytes) = self.read_projection_input(&path)? else {
            return Ok(None);
        };
        let content = String::from_utf8_lossy(&bytes).into_owned();
        let Some(sides) = crate::concord_queue::parse_vcs_marker_sides(&content) else {
            return Ok(None);
        };
        let org = matches!(Format::from_path(&self.root.join(rel)), Format::Org);
        let mut diff = match sides.base.as_deref() {
            Some(base) => {
                let full = self.root.join(rel);
                let base_doc = parse_doc(&full, base);
                let mine_doc = parse_doc(&full, &sides.mine);
                let theirs_doc = parse_doc(&full, &sides.theirs);
                // The merge tool's own proposed resolution, when EVERY region
                // supplied one. It is not a side and never changes a verdict —
                // it can only fill a `BothChanged` row the disjoint-edit merge
                // declined, and the user still has to confirm it.
                let art_doc = sides
                    .suggested
                    .as_deref()
                    .map(|suggested| parse_doc(&full, suggested));
                crate::sync_diff::diff3_docs_with_artifact(
                    &base_doc,
                    &mine_doc,
                    &theirs_doc,
                    art_doc.as_ref(),
                )
            }
            // No ancestor → no `BothChanged` verdict → no proposal of either
            // source; a suggestion region without a base never surfaces.
            None => crate::sync_diff::diff_texts(&sides.mine, &sides.theirs, org),
        };
        // Both revs address the ONE file the decisions will be applied to.
        let rev = content_rev(&content);
        diff.base_rev = rev.clone();
        diff.conflict_rev = rev;
        Ok(Some(crate::concord_queue::MarkerConflictDiff {
            mine_label: sides.mine_label,
            theirs_label: sides.theirs_label,
            regions: sides.regions,
            diff,
        }))
    }

    /// Apply the user's per-row decisions to a marker-bearing page and write the
    /// CLEAN merged result — the one write Concord invariant 3 permits to such a
    /// file, and only as the direct consequence of the resolution the user just
    /// confirmed in the in-page resolver.
    ///
    /// Same guards as [`Graph::resolve_sync_conflict`]: graph-text write admission,
    /// page lock, `base_rev` staleness guard (here against the whole marker
    /// file), org round-trip firewall. The merge itself is
    /// `sync_diff::merge_blocks` over the SAME alignment the diff published, so a
    /// row id means the same block to both. Once this succeeds the file no longer
    /// carries markers, so the save refusal lifts naturally — nothing else has to
    /// be told about it.
    pub fn resolve_vcs_marker_conflict(
        &self,
        rel: &str,
        decisions: &std::collections::HashMap<String, String>,
        base_rev: &str,
        pre_choice: &str,
    ) -> io::Result<()> {
        let write = self.admit_graph_text_writer()?;
        // Gate order (storage-sync-contract §3 invariant 9): the graph-global
        // identity gate is taken before any page lock. Inverting it deadlocks
        // every graph-text write in the process, not just this page.
        let _identity = self.lock_graph_text_identity_mutation()?;
        let path = self
            .resolve_graph_text_rel(&write, rel)?
            .ok_or_else(bad_path)?;
        let entry = self.entry_for_path(&path).ok_or_else(bad_path)?;
        let lock = self.page_lock(&path);
        let _guard = lock.lock().unwrap();
        let content = self.graph_text_read_to_string(&write, &path)?;
        if content_rev(&content) != base_rev {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "file changed on disk",
            ));
        }
        let Some(sides) = crate::concord_queue::parse_vcs_marker_sides(&content) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no VCS merge conflict markers to resolve",
            ));
        };
        // Org round-trip firewall: refuse rather than risk corrupting an .org
        // page whose sides don't survive a parse/serialize round trip.
        if Format::from_path(&path) == Format::Org
            && (!crate::org::org_editable(&sides.mine) || !crate::org::org_editable(&sides.theirs))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "an org side of this merge does not round-trip; not resolving",
            ));
        }
        let mine_doc = parse_doc(&path, &sides.mine);
        let theirs_doc = parse_doc(&path, &sides.theirs);
        // Same ancestor the marker diff used: the reconstructed base side, which
        // `parse_vcs_marker_sides` supplies only when EVERY region carried one.
        // The `base_rev` guard above pins the whole marker file, so mine, theirs
        // and this base are all still the exact bytes the UI decided against.
        let base_doc = sides.base.as_deref().map(|base| parse_doc(&path, base));
        // Likewise the merge tool's proposal: re-derived from the very bytes the
        // `base_rev` guard pinned, never taken from the frontend, so a confirmed
        // artifact row applies exactly the body the diff offered.
        let art_doc = sides
            .suggested
            .as_deref()
            .map(|suggested| parse_doc(&path, suggested));
        let merged_roots = crate::sync_diff::merge_blocks3(
            base_doc.as_ref().map(|doc| doc.roots.as_slice()),
            &mine_doc.roots,
            &theirs_doc.roots,
            art_doc.as_ref().map(|doc| doc.roots.as_slice()),
            decisions,
        )
        .map_err(merge_refused)?;
        let pre_block = match pre_choice {
            "theirs" => theirs_doc.pre_block.clone(),
            "mine" => mine_doc.pre_block.clone(),
            _ if Format::from_path(&path) == Format::Md => union_pre(
                mine_doc.pre_block.as_deref(),
                theirs_doc.pre_block.as_deref(),
            ),
            _ => mine_doc.pre_block.clone(),
        };
        let mut merged = Document {
            pre_block,
            roots: merged_roots,
        };
        assign_doc_runtime_ids(&mut merged.roots, &entry.rel_path);
        let dto = page_dto_checked(&entry, &merged)?;
        let cacheable = self.graph_text_path_is_cacheable(&write, &path)?;
        // Keep the pre-resolution marker bytes recoverable (ADR 0007), the way
        // the sync-copy resolve already trashes its conflict copy. The marker
        // file is rewritten IN PLACE, so a byte-exact copy staged here is the
        // only place the not-chosen side survives once the clean merge lands —
        // which is also what makes defaulting a missing row decision to Mine
        // at apply time recoverable rather than lossy.
        let trash = typed_trash_dir(&self.root, TrashEntryKind::Conflict);
        self.ensure_trash_write_target(&trash)?;
        fs::create_dir_all(&trash)?;
        let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
        let staged = trash.join(format!("{}__markers__{file_name}", trash_stamp()));
        atomic_write_new(&staged, content.as_bytes())?;
        let authorized = self.authorize_marker_resolution(&path);
        let result = self.write_page(
            &write,
            &dto,
            &path,
            Some(&content),
            true,
            None,
            None,
            None,
            cacheable,
        );
        drop(authorized);
        if result.is_err() {
            // The write refused or failed — the file still carries its
            // markers, so the staged recovery copy is redundant; withdraw it.
            let _ = fs::remove_file(&staged);
        }
        result.map(|_| ())
    }

    /// Capture an exact, still-live save-conflict presentation without
    /// consuming its one-shot write authority.
    fn live_save_conflict_parts(
        &self,
        page: &PageDto,
        base_rev: Option<&str>,
        presented: ConflictOverride,
    ) -> io::Result<(PathBuf, Option<String>, Option<String>)> {
        let write = self.admit_graph_text_writer()?;
        let (path, _) = self.save_target(&write, page)?;
        let activation = page.activation.map(EditorActivation::from_u64);
        let episode = ConflictEditorEpisode {
            loaded_revision: base_rev.map(str::to_owned),
            activation,
        };
        let authority = {
            let state = self.conflict_authority.lock().unwrap();
            let authority = state.tokens.get(&path).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "live conflict authority is missing or already consumed",
                )
            })?;
            if authority.observation_epoch != presented.observation_epoch
                || authority.editor_episode != episode
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "live conflict authority no longer describes this editor",
                ));
            }
            authority.clone()
        };
        let base = activation
            .and_then(|activation| self.editor_activation_baseline(&path, activation, base_rev));
        Ok((path, authority.bytes, base))
    }

    /// Block-level three-way presentation for a retained editor draft whose
    /// ordinary Direct Files save was refused. The disk side comes from the
    /// exact unconsumed authority token; the base comes from the matching live
    /// editor activation and is not advanced by watcher/cache admission.
    pub fn live_save_conflict_diff(
        &self,
        page: &PageDto,
        base_rev: Option<&str>,
        presented: ConflictOverride,
    ) -> io::Result<crate::sync_diff::SyncConflictDiff> {
        let (path, theirs_text, base_text) =
            self.live_save_conflict_parts(page, base_rev, presented)?;
        let mine = page_dto_document(page)?;
        let theirs = theirs_text
            .as_deref()
            .map(|text| parse_doc(&path, text))
            .unwrap_or(Document {
                pre_block: None,
                roots: Vec::new(),
            });
        Ok(match base_text {
            Some(base) => crate::sync_diff::diff3_docs(&parse_doc(&path, &base), &mine, &theirs),
            None => crate::sync_diff::diff_docs(&mine, &theirs),
        })
    }

    /// Capture the complete restart-recoverable presentation while the original
    /// one-shot editor authority is still live. This inspects but does not
    /// consume that authority.
    pub fn capture_live_save_conflict(
        &self,
        page: &PageDto,
        base_rev: Option<&str>,
        presented: ConflictOverride,
    ) -> io::Result<LiveSaveConflictCapture> {
        let (path, theirs_text, base_text) =
            self.live_save_conflict_parts(page, base_rev, presented)?;
        let theirs_text = theirs_text.unwrap_or_default();
        let mine = page_dto_document(page)?;
        let theirs = parse_doc(&path, &theirs_text);
        let mut diff = match base_text.as_deref() {
            Some(base) => crate::sync_diff::diff3_docs(&parse_doc(&path, base), &mine, &theirs),
            None => crate::sync_diff::diff_docs(&mine, &theirs),
        };
        diff.base_rev = base_rev.unwrap_or_default().to_owned();
        diff.conflict_rev = content_rev(&theirs_text);
        Ok(LiveSaveConflictCapture {
            disk_rev: diff.conflict_rev.clone(),
            diff,
            base_text,
        })
    }

    /// Recompute a durable live-conflict review against the disk as it exists
    /// now. This is read-only and deliberately needs no process-local editor
    /// activation: the draft and its exact base came from a prior capture.
    pub fn durable_live_save_conflict_diff(
        &self,
        page: &PageDto,
        base_text: Option<&str>,
    ) -> io::Result<crate::sync_diff::SyncConflictDiff> {
        let write = self.admit_graph_text_writer()?;
        let (path, _) = self.save_target(&write, page)?;
        let theirs_text = self
            .graph_text_read_optional_editor_conflict_snapshot(&write, &path)?
            .map(|(text, _)| text);
        let mine = page_dto_document(page)?;
        let theirs = parse_doc(&path, theirs_text.as_deref().unwrap_or_default());
        let mut diff = match base_text {
            Some(base) => crate::sync_diff::diff3_docs(&parse_doc(&path, base), &mine, &theirs),
            None => crate::sync_diff::diff_docs(&mine, &theirs),
        };
        diff.base_rev = page.rev.clone().unwrap_or_default();
        // Absence is distinct from an existing empty file: a later creator
        // must invalidate this review even when it writes zero bytes.
        diff.conflict_rev = theirs_text
            .as_deref()
            .map(content_rev)
            .unwrap_or_else(|| "absent".to_owned());
        Ok(diff)
    }

    /// Resolve an app-private live-conflict capsule. The expected disk revision
    /// is durable authority: it is checked under the same page lock immediately
    /// before the normal Direct Files writer commits. A later external write
    /// therefore refuses and forces a fresh review.
    pub fn resolve_durable_live_save_conflict(
        &self,
        page: &PageDto,
        expected_disk_rev: &str,
        decisions: &std::collections::HashMap<String, String>,
        pre_choice: &str,
    ) -> io::Result<PageDto> {
        let write = self.admit_graph_text_writer()?;
        // Gate order (storage-sync-contract §3 invariant 9): the graph-global
        // identity gate is taken before any page lock. Inverting it deadlocks
        // every graph-text write in the process, not just this page.
        let _identity = self.lock_graph_text_identity_mutation()?;
        let (path, _) = self.save_target(&write, page)?;
        let lock = self.page_lock(&path);
        let _guard = lock.lock().unwrap();
        let (theirs_text, expected_identity) = self
            .graph_text_read_optional_editor_conflict_snapshot(&write, &path)?
            .unzip();
        let current_disk_rev = theirs_text
            .as_deref()
            .map(content_rev)
            .unwrap_or_else(|| "absent".to_owned());
        if current_disk_rev != expected_disk_rev {
            return Err(DirectSaveError::into_io(
                DirectSaveFailureCode::ConflictBaseRev,
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "live conflict changed on disk",
                ),
            ));
        }
        if Format::from_path(&path) == Format::Org
            && theirs_text
                .as_deref()
                .is_some_and(|text| !crate::org::org_editable(text))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the org file does not round-trip; not merging",
            ));
        }
        let mine = page_dto_document(page)?;
        let theirs = parse_doc(&path, theirs_text.as_deref().unwrap_or_default());
        let pre_block = match pre_choice {
            "theirs" => theirs.pre_block.clone(),
            "mine" => mine.pre_block.clone(),
            _ if page.format == Format::Md => {
                union_pre(mine.pre_block.as_deref(), theirs.pre_block.as_deref())
            }
            _ => mine.pre_block.clone(),
        };
        let mut resolved = existing_document_page_dto(
            page,
            Document {
                pre_block,
                roots: crate::sync_diff::merge_blocks(&mine.roots, &theirs.roots, decisions)
                    .map_err(merge_refused)?,
            },
        )?;
        let cacheable = self.graph_text_path_is_cacheable(&write, &path)?;
        let rev = self.write_page(
            &write,
            &resolved,
            &path,
            theirs_text.as_deref(),
            true,
            expected_identity,
            None,
            None,
            cacheable,
        )?;
        resolved.rev = Some(rev);
        Ok(resolved)
    }

    /// Apply block decisions for a live Direct Files save conflict through the
    /// same one-shot authority consumed by `Keep mine`. No second overwrite
    /// protocol is invented: this only builds a resolved DTO, then delegates to
    /// the existing exact guarded force-save path.
    pub fn resolve_live_save_conflict(
        &self,
        page: &PageDto,
        base_rev: Option<&str>,
        presented: ConflictOverride,
        decisions: &std::collections::HashMap<String, String>,
        pre_choice: &str,
    ) -> io::Result<PageDto> {
        let (path, theirs_text, _) = self.live_save_conflict_parts(page, base_rev, presented)?;
        let mine = page_dto_document(page)?;
        let theirs = theirs_text
            .as_deref()
            .map(|text| parse_doc(&path, text))
            .unwrap_or(Document {
                pre_block: None,
                roots: Vec::new(),
            });
        let pre_block = match pre_choice {
            "theirs" => theirs.pre_block.clone(),
            "mine" => mine.pre_block.clone(),
            _ if page.format == Format::Md => {
                union_pre(mine.pre_block.as_deref(), theirs.pre_block.as_deref())
            }
            _ => mine.pre_block.clone(),
        };
        let mut resolved = existing_document_page_dto(
            page,
            Document {
                pre_block,
                roots: crate::sync_diff::merge_blocks(&mine.roots, &theirs.roots, decisions)
                    .map_err(merge_refused)?,
            },
        )?;
        let rev = self.force_save_page_at_revision(&resolved, base_rev, presented)?;
        resolved.rev = Some(rev);
        Ok(resolved)
    }

    /// Structural block-level diff of a conflict copy against its winner (both
    /// graph-root-relative paths). Loads each file directly by path — the conflict
    /// copy is deliberately not in the page cache — and aligns the two block trees
    /// (see [`crate::sync_diff`]). This is a READ; nothing is written. `Ok(None)`
    /// if either path is invalid or the file is gone.
    pub fn sync_conflict_diff(
        &self,
        winner_rel: &str,
        conflict_rel: &str,
    ) -> io::Result<Option<crate::sync_diff::SyncConflictDiff>> {
        let winner = GraphTextPath::parse(winner_rel.to_owned()).map_err(|_| bad_path())?;
        let Some(win_bytes) = self.read_projection_input(&winner)? else {
            return Ok(None);
        };
        let Some(conf_c) = self.read_sync_conflict_copy(conflict_rel)? else {
            return Ok(None);
        };
        let win_c = String::from_utf8(win_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "winner is not UTF-8"))?;
        let win = self.root.join(winner_rel);
        let conf = self.root.join(conflict_rel);
        let mine = parse_doc(&win, &win_c);
        let theirs = parse_doc(&conf, &conf_c);
        // Concord ledger (ADR 0056): with a usable base — the last text Tine
        // agreed on with the disk — upgrade to a 3-way diff whose rows carry
        // per-row suggestions the UI pre-selects (never auto-applies). A base
        // identical to the winner is almost always the admission artifact (the
        // winner's post-sync bytes were admitted and became the ledger entry
        // before this diff ran), and 3-way against it would blanket-suggest
        // "theirs"; skip it and fall back to the plain 2-way diff.
        let base_c = self
            .concord_ledger
            .get()
            .and_then(|ledger| ledger.conflict_base(conflict_rel, winner_rel))
            .filter(|base| base != &win_c);
        let mut diff = match &base_c {
            Some(base_c) => {
                let base = parse_doc(&win, base_c);
                crate::sync_diff::diff3_docs(&base, &mine, &theirs)
            }
            None => crate::sync_diff::diff_docs(&mine, &theirs),
        };
        diff.base_rev = content_rev(&win_c);
        diff.conflict_rev = content_rev(&conf_c);
        // Identity of the pinned base itself. `base_rev`/`conflict_rev` pin
        // mine and theirs, but the pin is MUTABLE state — the resolve echoes
        // this token back so a repin between diff and apply is a refusal, not
        // a silent substitution of the third input to a `"merged"` body.
        diff.merge_base_rev = base_c.as_deref().map(content_rev);
        Ok(Some(diff))
    }

    /// Two-way diff of a duplicate journal day's canonical file against one of
    /// its strays.
    ///
    /// Unlike a sync-conflict copy, the two files here have **no common
    /// ancestor**: they were never one document that diverged, they are two
    /// files that ended up claiming the same day (usually a journal date-format
    /// change, which never clobbers). So there is no ledger pin to look up and
    /// the diff is always 2-way — which the queue already handles, since it
    /// only offers a Base side when the diff reports `three_way`.
    ///
    /// Where the two files hold disjoint content every row is one-sided and
    /// "keep both" reproduces what Settings' Merge does by concatenation; where
    /// they overlap, the row-by-row choice can drop the duplication instead of
    /// doubling it.
    pub fn duplicate_journal_diff(
        &self,
        canonical_rel: &str,
        stray_rel: &str,
    ) -> io::Result<Option<crate::sync_diff::SyncConflictDiff>> {
        let canonical = GraphTextPath::parse(canonical_rel.to_owned()).map_err(|_| bad_path())?;
        let stray = GraphTextPath::parse(stray_rel.to_owned()).map_err(|_| bad_path())?;
        let Some(canonical_bytes) = self.read_projection_input(&canonical)? else {
            return Ok(None);
        };
        let Some(stray_bytes) = self.read_projection_input(&stray)? else {
            return Ok(None);
        };
        let canonical_path = self.root.join(canonical_rel);
        let stray_path = self.root.join(stray_rel);
        // A cross-format pair (.md against .org) cannot be merged: `merge_pages`
        // and `resolve_sync_conflict` both refuse it, and offering a row-by-row
        // choice we cannot apply would be a dead end. Surfaced as a
        // no-decidable-rows object instead, so the file rows still work.
        if Format::from_path(&canonical_path) != Format::from_path(&stray_path) {
            return Ok(None);
        }
        let canonical_content = String::from_utf8(canonical_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "journal file is not UTF-8"))?;
        let stray_content = String::from_utf8(stray_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "journal file is not UTF-8"))?;
        let mine = parse_doc(&canonical_path, &canonical_content);
        let theirs = parse_doc(&stray_path, &stray_content);
        let mut diff = crate::sync_diff::diff_docs(&mine, &theirs);
        diff.base_rev = content_rev(&canonical_content);
        diff.conflict_rev = content_rev(&stray_content);
        diff.merge_base_rev = None;
        Ok(Some(diff))
    }

    /// Fold one stray of a duplicate journal day into that day's canonical file,
    /// applying the user's per-row decisions, and move the stray to recoverable
    /// trash.
    ///
    /// Guarded so this can never be pointed at two unrelated pages: both paths
    /// must belong to the SAME duplicate day as `journal_conflicts` reports it,
    /// and the stray must not be the canonical file. Beyond those guards the
    /// merge is exactly the two-file reconciliation the sync-copy path already
    /// performs (same row decisions, same org round-trip firewall, same
    /// stage-before-commit ordering, same recoverable trash), so it shares that
    /// implementation rather than growing a second one that could drift.
    pub fn resolve_duplicate_journal_day(
        &self,
        canonical_rel: &str,
        stray_rel: &str,
        decisions: &std::collections::HashMap<String, String>,
        base_rev: &str,
        stray_rev: &str,
        pre_choice: &str,
    ) -> io::Result<PageDto> {
        if canonical_rel == stray_rel {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "canonical and stray are the same file",
            ));
        }
        let day = self
            .journal_conflicts()
            .into_iter()
            .find(|day| day.files.iter().any(|file| file.path == canonical_rel))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "not a file of a duplicate journal day",
                )
            })?;
        if !day.files.iter().any(|file| file.path == stray_rel) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the two files are not the same journal day",
            ));
        }
        // `journal_conflicts` sorts canonical-first, so the keeper is files[0]
        // whether or not any file carries the canonical date stem.
        let keeper = day.files.first().ok_or_else(bad_path)?;
        if keeper.path != canonical_rel {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "that file is not the day's canonical file",
            ));
        }
        // No ledger pin exists for a duplicate day, so the base token is None
        // and the shared path resolves 2-way.
        self.resolve_sync_conflict(
            canonical_rel,
            stray_rel,
            decisions,
            base_rev,
            stray_rev,
            None,
            pre_choice,
        )
    }

    /// Read one explicitly recognized provider conflict copy through the same
    /// confined, no-follow point capability used by graph-text reads.
    pub(crate) fn read_sync_conflict_copy(&self, conflict_rel: &str) -> io::Result<Option<String>> {
        let conflict = self
            .resolve_configured_rel_lexical(conflict_rel)
            .ok_or_else(bad_path)?;
        if !path_is_sync_conflict(&conflict) {
            return Ok(None);
        }
        let path = GraphTextPath::parse(conflict_rel.to_owned()).map_err(|_| bad_path())?;
        self.read_projection_input(&path)?
            .map(String::from_utf8)
            .transpose()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "conflict copy is not UTF-8"))
    }

    /// Stage exact conflict bytes in typed recoverable trash. The source is
    /// excluded from graph discovery, so this changes no page.
    pub(crate) fn stage_sync_conflict_trash(
        &self,
        conflict_rel: &str,
        expected: &[u8],
    ) -> io::Result<()> {
        let source = self
            .resolve_configured_rel_lexical(conflict_rel)
            .ok_or_else(bad_path)?;
        if !path_is_sync_conflict(&source) {
            return Err(bad_path());
        }
        self.ensure_within_graph_root(&source)?;
        let path = GraphTextPath::parse(conflict_rel.to_owned()).map_err(|_| bad_path())?;
        if self.read_projection_input(&path)?.as_deref() != Some(expected) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "conflict copy changed before recoverable staging",
            ));
        }
        let trash = typed_trash_dir(&self.root, TrashEntryKind::Conflict);
        self.ensure_trash_write_target(&trash)?;
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("txt");
        let mut identity = Sha256::new();
        identity.update(conflict_rel.as_bytes());
        identity.update([0]);
        identity.update(expected);
        let staged = trash.join(format!(
            "{}__conflict-{:x}.{extension}",
            trash_stamp(),
            identity.finalize()
        ));
        move_to_trash(&source, &staged, &trash)?;
        if fs::read(&staged)?.as_slice() != expected {
            let _ = move_file_noreplace(&staged, &source);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "conflict copy changed during recoverable staging",
            ));
        }
        Ok(())
    }

    /// Resolve a sync-conflict copy: build the merged winner from the user's
    /// per-row `decisions` (row id → `"mine"`/`"theirs"`/`"both"`/`"merged"`,
    /// see [`crate::sync_diff::merge_blocks3`]), write it through the NORMAL
    /// round-tripping save path, and move the conflict copy to the recoverable
    /// trash.
    ///
    /// Data-safety invariants (ADR 0012 one-writer + ADR 0007 never-silently-
    /// overwrite), mirroring [`merge_pages`]:
    /// - Everything runs under the winner's `page_lock`.
    /// - `base_rev` guard: if the winner changed on disk since the UI diffed it,
    ///   returns `AlreadyExists` ("conflict") WITHOUT writing, so the UI re-diffs
    ///   against fresh content instead of merging a stale alignment.
    /// - Org round-trip firewall: if either side is a non-round-trippable `.org`,
    ///   refuses rather than risk corrupting it.
    /// - Stage-before-commit: the conflict copy is moved to trash BEFORE the
    ///   merged winner is written, and the move is rolled back if the write fails
    ///   — so a retry can never duplicate content, and nothing is lost.
    ///
    /// `pre_choice` decides the page-property pre-block: `"mine"`, `"theirs"`, or
    /// `"union"` (default; markdown only — keep the winner's and add any property
    /// the conflict defines that the winner doesn't, so an `alias::`/`tags::` from
    /// the other device isn't dropped; org keeps the winner's, gated by the
    /// firewall).
    pub fn resolve_sync_conflict(
        &self,
        winner_rel: &str,
        conflict_rel: &str,
        decisions: &std::collections::HashMap<String, String>,
        base_rev: &str,
        conflict_rev: &str,
        merge_base_rev: Option<&str>,
        pre_choice: &str,
    ) -> io::Result<PageDto> {
        let write = self.admit_graph_text_writer()?;
        // Gate order (storage-sync-contract §3 invariant 9): the graph-global
        // identity gate is taken before any page lock. Inverting it deadlocks
        // every graph-text write in the process, not just this page.
        let _identity = self.lock_graph_text_identity_mutation()?;
        let win = self
            .resolve_graph_text_rel(&write, winner_rel)?
            .ok_or_else(bad_path)?;
        let conf = self
            .resolve_graph_text_rel(&write, conflict_rel)?
            .ok_or_else(bad_path)?;
        if win == conf {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "winner and conflict are the same file",
            ));
        }
        let win_entry = self.entry_for_path(&win).ok_or_else(bad_path)?;
        // Lock the winner so a concurrent editor/watcher write can't race the merge.
        let lock = self.page_lock(&win);
        let _guard = lock.lock().unwrap();
        let win_content = self.graph_text_read_to_string(&write, &win)?;
        let conf_content = self.graph_text_read_to_string(&write, &conf)?;
        // base_rev guard — the winner must still be what the UI diffed against.
        if content_rev(&win_content) != base_rev {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "winner changed on disk",
            ));
        }
        if content_rev(&conf_content) != conflict_rev {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "conflict copy changed on disk",
            ));
        }
        // Org round-trip firewall (same as merge_pages).
        if Format::from_path(&win) == Format::Org
            && (!crate::org::org_editable(&win_content) || !crate::org::org_editable(&conf_content))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "an org file in this pair does not round-trip; not merging",
            ));
        }
        let mine_doc = parse_doc(&win, &win_content);
        let theirs_doc = parse_doc(&conf, &conf_content);
        // Re-derive the SAME base `sync_conflict_diff` published suggestions
        // against — ledger pin included, and the same "a base identical to the
        // winner is the admission artifact" filter. The `base_rev`/
        // `conflict_rev` guards pin mine and theirs; `merge_base_rev` pins the
        // THIRD input: the ledger pin is mutable state, so the diff stamped
        // the pinned base's own rev and this apply refuses if it no longer
        // matches — a `"merged"` body is only ever computed from the exact
        // three texts the user saw. A `None` token (the UI reviewed a 2-way
        // diff) resolves without a base: mine/theirs/both decisions don't
        // need one and `"merged"` then refuses in `merge_blocks3`.
        let pinned_base = self
            .concord_ledger
            .get()
            .and_then(|ledger| ledger.conflict_base(conflict_rel, winner_rel))
            .filter(|base| base != &win_content);
        let base_doc = match (merge_base_rev, pinned_base) {
            (None, _) => None,
            (Some(rev), Some(base_content)) if content_rev(&base_content) == rev => {
                Some(parse_doc(&win, &base_content))
            }
            (Some(_), _) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "the pinned merge base changed since the diff",
                ));
            }
        };
        let merged_roots = crate::sync_diff::merge_blocks3(
            base_doc.as_ref().map(|doc| doc.roots.as_slice()),
            &mine_doc.roots,
            &theirs_doc.roots,
            // A conflict copy carries no merge tool's suggestion: computed-only.
            None,
            decisions,
        )
        .map_err(merge_refused)?;
        let pre_block = match pre_choice {
            "theirs" => theirs_doc.pre_block.clone(),
            "mine" => mine_doc.pre_block.clone(),
            _ if Format::from_path(&win) == Format::Md => union_pre(
                mine_doc.pre_block.as_deref(),
                theirs_doc.pre_block.as_deref(),
            ),
            _ => mine_doc.pre_block.clone(),
        };
        let mut merged = Document {
            pre_block,
            roots: merged_roots,
        };
        assign_doc_runtime_ids(&mut merged.roots, &win_entry.rel_path);
        let mut dto = page_dto_checked(&win_entry, &merged)?;
        dto.path = win_entry.rel_path.clone();
        let win_cacheable = self.graph_text_path_is_cacheable(&write, &win)?;
        // Stage-before-commit (L5): move the conflict copy out first, then write the
        // merged winner; roll the move back if the write fails.
        let trash = typed_trash_dir(&self.root, TrashEntryKind::Conflict);
        self.graph_text_create_dir_all(&write, &trash)?;
        let conf_name = conf.file_name().and_then(|s| s.to_str()).unwrap_or("file");
        let staged = trash.join(format!("{}__{conf_name}", trash_stamp()));
        self.graph_text_move_noreplace(&write, &conf, &staged)?;
        if self.graph_text_read_to_string(&write, &staged)? != conf_content {
            let _ = self.graph_text_move_noreplace(&write, &staged, &conf);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "conflict copy changed during merge",
            ));
        }
        let rev = match self.write_page(
            &write,
            &dto,
            &win,
            Some(&win_content),
            true,
            None,
            None,
            None,
            win_cacheable,
        ) {
            Ok(rev) => rev,
            Err(error) => {
                let _ = graph_text_write_during_rollback_hook();
                let _ = self.graph_text_move_noreplace(&write, &staged, &conf);
                return Err(error);
            }
        };
        // The conflict copy is resolved and trashed — its pinned base (if any)
        // has served its purpose; let the ledger forget it (best-effort).
        if let Some(ledger) = self.concord_ledger.get() {
            ledger.drop_pin(conflict_rel);
        }
        dto.rev = Some(rev);
        Ok(dto)
    }

    /// Move a sync-conflict copy to the recoverable trash WITHOUT merging (the
    /// "I've reviewed it, the winner is fine, discard the copy" affordance). Guards
    /// that the target actually IS a conflict copy so this can never trash a real
    /// page. Recoverable in `logseq/.tine-trash` (ADR 0007).
    pub fn trash_sync_conflict(&self, conflict_rel: &str) -> io::Result<()> {
        let content = self
            .read_sync_conflict_copy(conflict_rel)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such conflict file"))?;
        self.stage_sync_conflict_trash(conflict_rel, content.as_bytes())
            .inspect(|()| {
                // Discarded without merging — drop the copy's pinned base too.
                if let Some(ledger) = self.concord_ledger.get() {
                    ledger.drop_pin(conflict_rel);
                }
            })
    }
}
