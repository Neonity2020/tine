//! Graph's save path: preparing a cross-page move, save_page and
//! force_save_page, write_page, and serializing a page for its path.

use super::*;

impl Graph {
    pub fn prepare_direct_cross_page_move(
        &self,
        destination: &PageDto,
        sources: &[PageDto],
    ) -> io::Result<Option<crate::direct_move_recovery::PreparedDirectMove>> {
        use crate::direct_move_recovery::{
            DirectMoveRecord, ImageRef, MoveParticipant, ParticipantRole, PreparedDirectMove,
            RECORD_SCHEMA,
        };

        let write = self.admit_graph_text_writer()?;
        let mut images: std::collections::BTreeMap<String, Vec<u8>> =
            std::collections::BTreeMap::new();
        let mut participants: Vec<MoveParticipant> = Vec::new();
        let mut seen: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();

        for (role, page) in std::iter::once((ParticipantRole::Destination, destination))
            .chain(sources.iter().map(|page| (ParticipantRole::Source, page)))
        {
            if page.guide || page.read_only {
                return Ok(None); // never persisted; a record would name a file that is never written
            }
            let (path, _cache) = self.save_target(&write, page)?;
            if !seen.insert(path.clone()) {
                // The same physical file on both sides of the move. Degenerate:
                // one ordinary save, already convergent.
                return Ok(None);
            }
            let existing: Option<String> = match fs::read(&path) {
                Ok(bytes) => Some(String::from_utf8(bytes).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "page file is not UTF-8; no move record composed",
                    )
                })?),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            let (_doc, postimage) =
                self.serialize_page_dto_for_path(page, &path, existing.as_deref())?;

            let preimage_ref = match &existing {
                None => ImageRef::Absent,
                Some(content) => {
                    let bytes = content.as_bytes().to_vec();
                    let image = ImageRef::blob_of(&bytes);
                    if let ImageRef::Blob { sha256, .. } = &image {
                        images.entry(sha256.clone()).or_insert(bytes);
                    }
                    image
                }
            };
            let postimage_bytes = postimage.as_bytes().to_vec();
            let postimage_ref = ImageRef::blob_of(&postimage_bytes);
            if let ImageRef::Blob { sha256, .. } = &postimage_ref {
                images.entry(sha256.clone()).or_insert(postimage_bytes);
            }

            participants.push(MoveParticipant {
                role,
                relative_path: self.rel_path(&path),
                page_name: page.name.clone(),
                page_kind: match page.kind {
                    PageKind::Journal => "journal".to_string(),
                    PageKind::Page => "page".to_string(),
                },
                base_revision: existing.as_deref().map(content_rev),
                preimage: preimage_ref,
                postimage: postimage_ref,
            });
        }

        if participants.len() < 2 {
            return Ok(None);
        }

        Ok(Some(PreparedDirectMove {
            record: DirectMoveRecord {
                schema: RECORD_SCHEMA,
                move_id: crate::direct_move_recovery::new_move_id(),
                graph_root: self.root.display().to_string(),
                created_unix_ms: crate::direct_move_recovery::unix_millis_now(),
                participants,
            },
            images,
        }))
    }

    /// Save a page, refusing to clobber an external change. If the file on disk
    /// no longer matches what Tine last knew (another app or a Syncthing pull
    /// wrote it), returns an `AlreadyExists` "conflict" error WITHOUT writing,
    /// so the caller can surface it and keep the in-memory edits.
    pub fn save_page(&self, page: &PageDto, base_rev: Option<&str>) -> io::Result<String> {
        if page.guide {
            #[cfg(debug_assertions)]
            eprintln!("attempted to persist an ephemeral bundled Guide page");
            return Ok("guide-ephemeral".into());
        }
        let write = self.admit_graph_text_writer()?;
        let _identity = self.lock_graph_text_identity_mutation()?;
        let (path, cache) = self.save_target(&write, page)?;
        // Serialize against any other writer of THIS page (a PDF highlight write
        // of the same `hls__` page, or another save) for the whole
        // read→conflict-check→write→cache_upsert, so neither can clobber the other
        // or steal its self-write marker (see `page_locks`).
        let lock = self.page_lock(&path);
        let _guard = lock.lock().unwrap();
        let editor_episode = ConflictEditorEpisode {
            loaded_revision: base_rev.map(str::to_owned),
            activation: page.activation.map(EditorActivation::from_u64),
        };
        // Single read of the current file (the conflict baseline AND the
        // formatting source AND, with the written content, the returned rev) —
        // avoids re-reading the file 2-3× per save, which is felt on NFS.
        // Creation admission is NOT the same question as "is this page pinned".
        // An absent editor pins the prospective target it was handed, and would
        // otherwise skip the semantic-owner check precisely when it is creating.
        // For an already-existing target the semantic lookup is inert, so this is
        // safe on both writers. (GH #254 increment 3.)
        let requested_identity = (page.path.is_empty()
            || self.save_is_from_prospective_editor(page, &path))
        .then_some((page.kind, page.name.as_str()));
        let validation = self.validate_graph_text_target(&write, &path, requested_identity)?;
        let prospective_editor = self.save_is_from_prospective_editor(page, &path);
        self.require_pinned_save_owner(
            page,
            &path,
            validation.target.as_ref(),
            PinnedSaveAuthority::OrdinaryEditorSave {
                loaded_revision: base_rev,
                prospective_editor,
            },
        )?;
        if validation.requested_identity_elsewhere {
            return Err(DirectSaveError::into_io(
                DirectSaveFailureCode::IdentityOwnedElsewhere,
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another graph document owns this effective page identity",
                ),
            ));
        }
        let creation_proof = validation.creation_proof;
        let existing: Option<(String, ContentDigest)> = match validation.target {
            Some(ExactGraphLoadedPage {
                content: disk_s,
                file_identity: current_identity,
                ..
            }) => {
                // The file must still match the exact bytes the editor loaded
                // (`base_rev`); if it changed underneath us (external edit /
                // Syncthing pull), refuse to clobber. `base_rev == None` means the
                // editor believed the page was new, so any existing file is an
                // external creation → conflict.
                if !base_rev.is_some_and(|rev| content_rev(&disk_s) == rev) {
                    return Err(self.conflict_error_from_snapshot(
                        &path,
                        Some(&editor_episode),
                        EditorConflictSite::SaveBaselinePresent,
                        ConflictSnapshot::Present {
                            revision: content_rev(&disk_s),
                            resource_identity: current_identity,
                        },
                        Some(disk_s),
                    ));
                }
                // Reaching here PROVES the file holds exactly the bytes the
                // editor loaded. A different inode carrying those same bytes is
                // an atomic republication of the state we already have — every
                // tool that publishes by `rename()` does this — so it is not a
                // conflict, and refusing it made the page permanently unsaveable
                // (GH #254; the frontend retries the old refusal forever).
                //
                // Re-pin to the CURRENT snapshot so the write binds to the inode
                // that actually exists, and so the identity-bound retire/publish
                // check below has the right expectation. `expected_identity`
                // already flows from `current_identity`; this only keeps the
                // retained map honest for the next save.
                //
                self.loaded_file_identities
                    .write()
                    .unwrap()
                    .insert(path.clone(), (content_rev(&disk_s), current_identity));
                Some((disk_s, current_identity))
            }
            None => {
                // The file is gone. If the editor had a baseline (page existed at
                // load), it was deleted externally — DON'T silently resurrect it.
                if base_rev.is_some() {
                    return Err(self.conflict_error_from_snapshot(
                        &path,
                        Some(&editor_episode),
                        EditorConflictSite::SaveBaselineAbsent,
                        ConflictSnapshot::Absent,
                        None,
                    ));
                }
                None
            }
        };
        // recheck = true: re-verify the file hasn't changed on disk in the instant
        // before the write, to narrow the inherent race against a NON-cooperating
        // external writer (OG/Syncthing) that doesn't take our page lock.
        // M2: write to the SAME path we locked + read the baseline from — never
        // re-resolve `path_for` under the lock (an `exists()`-probe could otherwise
        // pick a different extension if a twin appears mid-save).
        let existing_content = existing.as_ref().map(|(content, _)| content.as_str());
        let expected_identity = existing.as_ref().map(|(_, identity)| *identity);
        let result = self.write_page(
            &write,
            page,
            &path,
            existing_content,
            true,
            expected_identity,
            Some(&editor_episode),
            creation_proof,
            cache,
        );
        if result.is_ok() {
            self.revoke_conflict_authority(&path);
        }
        result
    }

    /// Apply the one-shot conflict authority associated with the revision carried
    /// by a directly loaded `PageDto`.
    /// The conflict currently outstanding for this page's save target, if any.
    ///
    /// The UI reads this when it raises a banner and echoes it back on "Keep
    /// mine", so the override names the observation the user saw. Only Tine's
    /// own save attempts mint, and they are serialized per page, so the value a
    /// caller reads immediately after its own save failed is that save's own
    /// observation.
    pub fn outstanding_conflict_override(
        &self,
        page: &PageDto,
    ) -> io::Result<Option<ConflictOverride>> {
        let write = self.admit_graph_text_writer()?;
        let (path, _cache) = self.save_target(&write, page)?;
        let state = self.conflict_authority.lock().unwrap();
        Ok(state.tokens.get(&path).map(|token| ConflictOverride {
            observation_epoch: token.observation_epoch,
        }))
    }

    /// Compatibility symbol for callers that have not migrated to explicit
    /// conflict authority. It deliberately fails closed: choosing whatever
    /// observation happens to be current could spend authority for a winner the
    /// caller was never shown.
    pub fn force_save_page(&self, page: &PageDto) -> io::Result<String> {
        if page.guide {
            return Ok("guide-ephemeral".into());
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            DirectSaveError {
                code: DirectSaveFailureCode::ConflictAuthoritySpent,
                conflict_epoch: None,
                source: io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "conflict_authority.explicit_required: force_save_page requires an explicitly captured observation; use force_save_page_at_revision",
                ),
            },
        ))
    }

    /// Save with the exact conflict snapshot shown to this editor episode as the
    /// ordinary-save baseline. `base_rev` comes from the frontend's editor state;
    /// its save DTO deliberately omits `PageDto.rev`. `authority` names WHICH
    /// conflict observation the user answered.
    pub fn force_save_page_at_revision(
        &self,
        page: &PageDto,
        base_rev: Option<&str>,
        authority: ConflictOverride,
    ) -> io::Result<String> {
        if page.guide {
            #[cfg(debug_assertions)]
            eprintln!("attempted to force-persist an ephemeral bundled Guide page");
            return Ok("guide-ephemeral".into());
        }
        let write = self.admit_graph_text_writer()?;
        let _identity = self.lock_graph_text_identity_mutation()?;
        let (path, cache) = self.save_target(&write, page)?;
        let lock = self.page_lock(&path);
        let _guard = lock.lock().unwrap();
        let editor_episode = ConflictEditorEpisode {
            loaded_revision: base_rev.map(str::to_owned),
            activation: page.activation.map(EditorActivation::from_u64),
        };
        // The override path demands a LIVE editor activation. Rule 1 of the
        // contract: an override may only be spent by the exact editor activation
        // that was shown the conflict. A request with no activation, or one that
        // is no longer live for this path, cannot be that editor — it is a stale
        // callback, a cloned DTO, or an editor-less writer that has no business
        // forcing. Checked BEFORE the atomic consume so a refusal does not burn
        // the token the real editor still needs. (GH #254 increment 3.)
        match editor_episode.activation {
            Some(activation) if self.editor_activation_is_live(&path, activation) => {}
            Some(_) => {
                return Err(DirectSaveError::into_io(
                    DirectSaveFailureCode::ConflictAuthoritySuperseded,
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "conflict_authority.superseded: the editor activation answering this conflict is no longer live",
                    ),
                ));
            }
            None => {
                return Err(DirectSaveError::into_io(
                    DirectSaveFailureCode::ConflictAuthoritySpent,
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "conflict_authority.spent: a conflict override requires a live editor activation",
                    ),
                ));
            }
        }
        // Atomic take happens before every fallible validation below. A failed or
        // replayed attempt therefore cannot reuse the authority it started with.
        let authority = self.consume_conflict_authority(&path, &editor_episode, authority)?;
        // "Keep mine" resolves a content conflict, but it must not turn an I/O or
        // decoding failure into permission to overwrite unknown bytes.
        // Creation admission is NOT the same question as "is this page pinned".
        // An absent editor pins the prospective target it was handed, and would
        // otherwise skip the semantic-owner check precisely when it is creating.
        // For an already-existing target the semantic lookup is inert, so this is
        // safe on both writers. (GH #254 increment 3.)
        let requested_identity = (page.path.is_empty()
            || self.save_is_from_prospective_editor(page, &path))
        .then_some((page.kind, page.name.as_str()));
        let validation = self.validate_graph_text_target(&write, &path, requested_identity)?;
        if validation.requested_identity_elsewhere {
            return Err(DirectSaveError::into_io(
                DirectSaveFailureCode::IdentityOwnedElsewhere,
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another graph document owns this effective page identity",
                ),
            ));
        }
        let validation_snapshot = validation.target.as_ref().map(|loaded| {
            (
                loaded.content.as_str(),
                loaded.revision.as_str(),
                loaded.file_identity,
            )
        });
        // The BYTES decide. A changed resource identity does not veto.
        //
        // A syncer that republishes the same content by temp+rename leaves the
        // shown bytes on a NEW inode. Increment 1 already ruled that case "an
        // atomic republication of the state we already have" and re-pins rather
        // than conflicting. Force is the ordinary save with a substituted
        // baseline, so it must not be STRICTER than the path it is defined in
        // terms of — refusing here would mint a fresh, visually identical banner
        // for the user to click again, in exactly the Syncthing scenario GH #254
        // exists for, and a busy syncer can repeat it. Martin's 2026-08-09
        // ruling is that state decides, and the state the user was shown is
        // still what is on disk.
        //
        // Safety is unchanged: `rebound_identity` becomes the write's
        // `expected_identity`, so the identity-bound retire/publish check still
        // fails closed against a DIFFERENT-byte winner. A same-byte
        // republication is the only thing this admits.
        let mut rebound_identity = None;
        let snapshot_still_matches = match (&authority.snapshot, validation_snapshot) {
            (
                ConflictSnapshot::Present { revision, .. },
                Some((content, current_revision, current_identity)),
            ) => {
                let same_state =
                    authority.bytes.as_deref() == Some(content) && revision == current_revision;
                if same_state {
                    rebound_identity = Some(current_identity);
                }
                same_state
            }
            (ConflictSnapshot::Absent, None) => true,
            _ => false,
        };
        if !snapshot_still_matches {
            let (snapshot, bytes, site) = match validation.target {
                Some(loaded) => (
                    ConflictSnapshot::Present {
                        revision: loaded.revision,
                        resource_identity: loaded.file_identity,
                    },
                    Some(loaded.content),
                    EditorConflictSite::SaveBaselinePresent,
                ),
                None => (
                    ConflictSnapshot::Absent,
                    None,
                    EditorConflictSite::SaveBaselineAbsent,
                ),
            };
            return Err(self.conflict_error_from_snapshot(
                &path,
                Some(&editor_episode),
                site,
                snapshot,
                bytes,
            ));
        }
        self.require_pinned_save_owner(
            page,
            &path,
            validation.target.as_ref(),
            PinnedSaveAuthority::UserOverride(&authority.snapshot),
        )?;
        let existing: Option<(String, ContentDigest)> = match authority.snapshot {
            ConflictSnapshot::Present {
                revision,
                resource_identity,
            } => {
                let bytes = authority.bytes.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "present conflict authority has no retained baseline bytes",
                    )
                })?;
                if content_rev(&bytes) != revision {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "present conflict authority revision does not match retained bytes",
                    ));
                }
                // Bind to the identity that is actually on disk now. These differ
                // only for a same-byte republication, which the check above
                // deliberately admits; binding to the stale one would make the
                // publication boundary refuse the write we just authorized.
                Some((bytes, rebound_identity.unwrap_or(resource_identity)))
            }
            ConflictSnapshot::Absent => None,
        };
        let creation_proof = validation.creation_proof;
        // Force is the ordinary editor-save protocol with a substituted baseline.
        // Both the late byte recheck and exact-identity publication remain enabled.
        let existing_content = existing.as_ref().map(|(content, _)| content.as_str());
        let expected_identity = existing.as_ref().map(|(_, identity)| *identity);
        let result = self.write_page(
            &write,
            page,
            &path,
            existing_content,
            true,
            expected_identity,
            Some(&editor_episode),
            creation_proof,
            cache,
        );
        if result.is_ok() {
            self.revoke_conflict_authority(&path);
        }
        result
    }

    /// Write a page to `path` (already resolved + locked by the caller), reproducing
    /// `existing`'s formatting, and return the new on-disk content rev (computed from
    /// what was written — no extra read).
    pub(super) fn write_page(
        &self,
        write: &GraphTextWritePermit,
        page: &PageDto,
        path: &Path,
        existing: Option<&str>,
        recheck: bool,
        expected_identity: Option<ContentDigest>,
        editor_episode: Option<&ConflictEditorEpisode>,
        creation_proof: Option<DirectCreationProof>,
        cache: bool,
    ) -> io::Result<String> {
        let (doc, content) = self.serialize_page_dto_for_path(page, path, existing)?;
        // No-op save: identical bytes already on disk (e.g. focus/blur with no real
        // edit, or a forced flush of an unchanged page). Skip the write, the
        // watcher record, AND — crucially — the cache update below.
        let changed = existing != Some(content.as_str());
        // The shared commit protocol (marker → A3 recheck vs `existing` →
        // atomic_write). A force save substitutes its one-shot conflict snapshot
        // for `existing` and still rechecks it. On a no-op, just hash the unchanged
        // bytes for the returned/cached rev — no write, no marker.
        let rev = if changed {
            self.commit_editor_write(
                write,
                &path,
                &content,
                existing,
                recheck,
                expected_identity,
                editor_episode,
                creation_proof,
                None,
            )?
        } else {
            content_rev(&content)
        };
        // Touch the cache only when the bytes changed, or the page isn't in an
        // already-built cache yet (fold a cold page in). A no-op save of an
        // already-cached page MUST NOT call cache_upsert: it bumps `cache_gen`,
        // which keys every memoized backlink/reference result — so an unchanged
        // re-save would force a whole-graph rescan on every open dashboard.
        // A path-pinned save (`cache == false`, a duplicate-day stray, #21) still
        // owns its OWN path slot — what it must not do is take the day away from
        // the canonical file when someone opens it BY NAME. Those are different
        // questions and different code: cache slots are keyed by path, while
        // name resolution does not read this cache's `by_name` map at all.
        // `find_entry` builds its own index over `list_pages` and explicitly
        // prefers the date-stem file (`lookup.rs`), which is what makes opening
        // the day deterministic. `a_page_cache_by_name_map_is_not_a_lookup`
        // pins that, because it is the whole reason this save is safe.
        let need_cache_update = cache
            && (changed || {
                let guard = self.cache.read().unwrap();
                guard
                    .as_ref()
                    .is_some_and(|pages| self.cached_page_index_for_path(pages, &path).is_none())
            });
        // GH #543 (fifth audit A5-N2): the `(kind,name)` exclusion above had
        // quietly become an exclusion from the INDEX too. The projection's rows
        // are keyed by PATH, not by logical name, so a pinned file has its own
        // rows and they are what search answers from — but `cache_upsert` is
        // where the delta is published, so skipping it left this file indexed at
        // its pre-save text forever, with the index idle, validated and ready.
        // Nothing was queued and a successful answer never reaches the repair a
        // refusal would start, so it never corrected itself.
        //
        // (Sixth audit A6-N1: publishing the delta alone was still not enough.
        // The parsed cache is an authoritative PRODUCER — a repair snapshots it
        // and republishes it wholesale at the current generation — so a pinned
        // save that left its own slot holding pre-save text had its rows undone
        // by the next repair, and, publishing at an unchanged generation, could
        // not outrank them either. A file this save rewrote goes through the
        // same front door as every other rewritten file.)
        let need_cache_update = need_cache_update || (!cache && changed);
        if need_cache_update {
            // For a brand-new journal, derive its date_key from the name so it's
            // recognized as a dated journal by `journals_desc` (which reads this
            // cache) — otherwise today's freshly-created page would be missing.
            let base_entry = self.entry_for_path(&path).unwrap_or_else(|| {
                let date_key = if page.kind == PageKind::Journal {
                    crate::date::JournalDate::from_title(&page.name).map(|d| d.ordinal_key())
                } else {
                    None
                };
                PageEntry {
                    name: page.name.clone(),
                    kind: page.kind,
                    date_key,
                    rel_path: self.rel_path(&path),
                    path: path.to_path_buf(),
                }
            });
            // H4: for org, the on-disk bytes are authoritative. If the user typed a
            // structural marker (a column-0 `* ` line, or an unbalanced #+BEGIN_)
            // into a block body, `content` re-parses to a DIFFERENT tree than the
            // frontend `doc` — cache what's actually on disk so the next load shows
            // the real structure instead of a cache that silently disagrees. Common
            // case: structures match → keep `doc` (block uuids stay stable). Markdown
            // continuation lines are indented, so they can't re-read differently.
            let cache_doc = if Format::from_path(&path) == Format::Org {
                let reparsed = crate::org::parse_org(&content);
                if reparsed == doc {
                    doc
                } else {
                    reparsed
                }
            } else {
                doc
            };
            let entry = effective_page_entry(&self.journal_format, &base_entry, &cache_doc);
            self.cache_upsert(entry, cache_doc, rev.clone());
        }
        // Drop the self-write marker now the write is published + cached (it only
        // had to cover the atomic-write → cache_upsert window; disk_revs now
        // suppresses the watcher). See drop_self_write_marker.
        if changed {
            self.drop_self_write_marker(&path, &rev);
        }
        // The new baseline rev = hash of exactly what's now on disk (the content we
        // serialized, or the identical existing bytes on a no-op) — no re-read.
        Ok(rev)
    }

    fn serialize_page_dto_for_path(
        &self,
        page: &PageDto,
        path: &Path,
        existing: Option<&str>,
    ) -> io::Result<(Document, String)> {
        let dto_is_org = matches!(Format::from_path(path), Format::Org);
        let mut doc = Document {
            pre_block: page.pre_block.clone(),
            roots: dto_blocks_to_doc_checked(&page.blocks, dto_is_org)?,
        };
        if existing.is_none() && page.kind == PageKind::Page {
            let filename_name = self
                .graph_entry_for_relative_path(&self.rel_path(path))?
                .name;
            if filename_name != page.name {
                bind_document_title_property(&mut doc, &page.name);
            }
        }
        // Claim source-layout retention for every block the editor still holds
        // by identity. Passing `&[]` here (as this did) left the retention
        // machinery inert on the ordinary Direct save path, so editing ONE block
        // rewrote untouched bytes elsewhere on the page — measured at 96 of 983
        // files on a real-shaped graph. Identities are matched against the
        // existing source by structural position, so a block the user actually
        // moved simply fails to match and keeps today's behaviour.
        let identities = doc::layout_identities_of(&doc);
        self.serialize_page_document(doc, path, existing, &identities)
    }

    /// The one page serialization and corruption-firewall boundary used by
    /// ordinary editor saves.
    fn serialize_page_document(
        &self,
        mut doc: Document,
        path: &Path,
        existing: Option<&str>,
        layout_identities: &[StructuralLayoutIdentity],
    ) -> io::Result<(Document, String)> {
        // (A new journal's `path` was named by `path_for` using the graph's
        // `:journal/file-name-format` — so custom-format graphs create the correct
        // file for the day instead of a misplaced default-named duplicate.)
        let dto_is_org = matches!(Format::from_path(path), Format::Org);
        // VCS merge-conflict quarantine (Concord invariant 3). In-scope threat:
        // an external VCS merge (git/Fossil) left column-0 conflict markers in
        // this file. Re-serializing would re-indent the markers as continuation
        // lines (or drop them), which destroys the VCS's own conflict
        // detection and can silently lose one side of the merge. The page
        // stays readable; every write to it — normal and force — is refused
        // until the user resolves the merge, in Tine's own in-page conflict
        // resolver (`resolve_vcs_marker_conflict`, the ONE exemption below) or
        // outside Tine. Mirrors the GH #163 refusal-instead-of-rewrite pattern.
        if let Some(existing) = existing {
            let markers = doc::vcs_conflict_markers(existing);
            if !markers.is_empty() && !self.is_resolving_markers(path) {
                return Err(projection_semantic_refusal(
                    io::ErrorKind::InvalidData,
                    format!(
                        "file contains unresolved VCS merge conflict markers ({}) — resolve the merge with your version-control tool or an external editor first; Tine never rewrites a conflicted file",
                        markers.join(", ")
                    ),
                ));
            }
        }
        // Data-preservation firewall for page-header properties (GH #163).
        // A frontend/store bug once reclassified a suffix of the page pre-block
        // as the first outline block (`A::` stayed in the header while `B::` and
        // `C::` were serialized as `- B::` / indented continuation text).  The
        // string helper used by the gear panel was correct, so helper tests could
        // not protect the actual DTO -> disk boundary.  No Tine editing command
        // intentionally moves an existing page-header property line into the
        // outline; promotion keeps property lines in the pre-block.  Refuse both
        // normal and force writes that do so, leaving the original bytes intact.
        if let Some(existing) = existing {
            // A nonempty disk preamble is authoritative. If a contradictory DTO
            // drops it while presenting a first-root header candidate, refusing
            // the save is safer than either overwriting the preamble or silently
            // keeping the candidate as a bullet. This also protects force-save.
            let existing_doc = doc::parse(existing);
            if existing_doc
                .pre_block
                .as_deref()
                .is_some_and(|pre| !pre.is_empty())
                && doc.pre_block.as_deref().unwrap_or("").is_empty()
                && first_root_is_promotable_page_header(&doc)
            {
                return Err(projection_semantic_refusal(
                    io::ErrorKind::InvalidData,
                    "refusing to drop an existing page preamble while authoring page-header properties",
                ));
            }
            if let Some(line) = newly_reclassified_page_property_line(existing, &doc) {
                return Err(projection_semantic_refusal(
                    io::ErrorKind::InvalidData,
                    format!("refusing to move page-header property into outline content: {line}"),
                ));
            }
        }
        // Match OG's pre-block serialization decision at one native boundary:
        // a genuinely headerless Markdown page may author a qualifying first
        // root through the ordinary editor, but the persisted/cache shape is an
        // unbulleted page header. Existing nonempty preambles were rejected above.
        if !dto_is_org
            && doc.pre_block.as_deref().unwrap_or("").is_empty()
            && existing
                .map(doc::parse)
                .and_then(|parsed| parsed.pre_block)
                .as_deref()
                .unwrap_or("")
                .is_empty()
        {
            promote_first_root_page_header(&mut doc);
        }
        let content = match Format::from_path(path) {
            Format::Md => {
                // Reproduce the existing file's formatting (trailing newline,
                // post-property blank line, indent) so an unchanged save is
                // byte-identical and edits produce a minimal diff — critical to
                // avoid Syncthing churn against Logseq.
                let opts =
                    doc::SerializeOpts::detect_with_layout_identities(existing, layout_identities);
                let mut content = doc::serialize_with(&doc, &opts);
                // A5: if the ONLY difference from disk is whitespace trivia the
                // serializer doesn't round-trip byte-exactly (post-property
                // blank-line count, empty-bullet spelling `- ` vs `-`, indented
                // blank continuation lines), keep the existing bytes verbatim.
                // `doc::parse` collapses exactly this trivia (and ignores uuids),
                // so equal parses ⟹ the user changed nothing of substance → don't
                // rewrite (avoids needless Syncthing churn). Adopting the disk
                // bytes makes `changed` below false and the returned rev the
                // on-disk rev — so every downstream path stays correct.
                if let Some(e) = existing {
                    if e != content && doc::parse(e) == doc::parse(&content) {
                        content = e.to_string();
                    }
                }
                // CRLF preservation (shared with write_highlights). No-op saves
                // already kept the existing bytes verbatim (A5 above), so this can't
                // double-convert.
                preserve_crlf(content, existing)
            }
            Format::Org => {
                // Corruption firewall: never write a .org file Tine cannot
                // reproduce byte-for-byte. Such a page is served read-only (the
                // editor blocks edits), but defend the write path too — a stale
                // editor or a direct save must not rewrite it. The org serializer
                // is itself byte-exact (no trivia dance / CRLF rewrite needed):
                // the block bodies carry their verbatim text, including any `\r`.
                if let Some(e) = existing {
                    if !crate::org::org_editable(e) {
                        return Err(projection_semantic_refusal(
                            io::ErrorKind::PermissionDenied,
                            "org file is read-only (does not round-trip)",
                        ));
                    }
                }
                crate::org::serialize_org_detect_with_layout_identities(
                    &doc,
                    existing,
                    layout_identities,
                )
            }
        };
        Ok((doc, content))
    }
}
