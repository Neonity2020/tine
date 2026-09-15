//! Graph's editor activation and conflict authority: which file a save targets,
//! activating and retiring an editor on a page, minting and consuming conflict
//! authority, and observing editor conflicts.

use super::*;

impl Graph {
    /// Resolve the file a save writes to, and whether it participates in the
    /// `(kind,name)` page cache. A page pinned to a specific file (`page.path` set
    /// — a duplicate-day stray, #21) writes to THAT exact file and stays OUT of the
    /// cache (the `(kind,name)` slot belongs to the canonical file; caching the
    /// stray there would make name-resolution serve it). A normal page resolves its
    /// path by name and caches as before. Errors on an invalid pinned path (escapes
    /// the graph) or a `.md`+`.org` twin (ambiguous identity, M1).
    pub(super) fn save_target(
        &self,
        write: &GraphTextWritePermit,
        page: &PageDto,
    ) -> io::Result<(PathBuf, bool)> {
        if !page.path.is_empty() {
            // The page knows its own file (every loaded page carries its path).
            // Write THERE — that's how a duplicate-day stray saves to its own file
            // instead of being re-resolved by name to the canonical one. It still
            // participates in the `(kind,name)` cache UNLESS it's a shadow (a
            // title-named journal coexisting with a canonical date-stem file): a
            // shadow's cache slot belongs to the canonical, so it stays out.
            let path = self
                .resolve_graph_rel_with_permit(write, &page.path)?
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid page path"))?;
            // An ABSENT editor's first save re-resolves and compares, because the
            // promise it holds can go stale underneath it: the preferred extension
            // wins only while no configured alternate exists, so an external
            // `.org` appearing after activation moves the answer off the `.md`
            // this editor was promised. Landing on the stale pin would create
            // exactly the ambiguous twin that creation admission exists to refuse.
            if let Some(held) = self.prospective_activation_target(page) {
                let resolved = self.graph_text_path_for(write, &page.name, page.kind)?;
                if resolved != held {
                    // Drift. Whether the new target is free or occupied, the
                    // editor's IDENTITY moves with it: the same person is still
                    // typing the same draft. Carrying it across is what lets the
                    // occupied case be answerable — the ordinary baseline check
                    // below then sees a present file against this absent editor's
                    // `base_rev = None` and mints a normal conflict, and the
                    // override that answers it finds its activation live at the
                    // file it actually drifted onto.
                    //
                    // The alternatives were both reproduced and both wrong:
                    // keeping `base_rev = None` strands the draft on
                    // `AlreadyExists` forever, and adopting the existing file's
                    // revision overwrites external bytes the user never saw.
                    let activation = EditorActivation::from_u64(
                        page.activation
                            .expect("prospective_activation_target requires one"),
                    );
                    self.retarget_editor_activation(&held, &resolved, activation);
                }
                let cache = self.graph_text_path_is_cacheable(write, &resolved)?;
                return Ok((resolved, cache));
            }
            let cache = self.graph_text_path_is_cacheable(write, &path)?;
            return Ok((path, cache));
        }
        // M1: refuse to write an ambiguous page (both .md and .org on disk) — we
        // can't tell which file the editor's content belongs to.
        if self.graph_text_has_twin(write, &page.name, page.kind)? {
            return Err(twin_error(&page.name));
        }
        let path = self.graph_text_path_for(write, &page.name, page.kind)?;
        Ok((path, true))
    }

    /// Re-pin a loaded page's retained file identity when the file was replaced
    /// atomically with **byte-identical** content.
    ///
    /// A save proves it is not clobbering someone else's work two ways: the
    /// editor's `base_rev` must still match the bytes on disk, and the file must
    /// still be the same physical file that was read at load. External tools
    /// break the second without touching the first — OneDrive rehydrating a
    /// Files-On-Demand placeholder, a Syncthing pull that lands the same bytes,
    /// a `cp` over a hardlink. The path then holds exactly the bytes the editor
    /// started from, but a different inode.
    ///
    /// Before this, every later save of that page failed with "existing page
    /// identity changed since load", and both exits from the resulting conflict
    /// prompt were dead ends: "Keep mine (overwrite)" re-ran the same check and
    /// failed too, and "Use disk version" discarded the user's edit. The user
    /// could not save, and the only button that worked lost their work.
    ///
    /// This re-pins the identity **only** when the retained revision still
    /// matches the bytes now on disk, so the proof it stands on is unchanged.
    /// A file whose content differs is a real external change and still
    /// conflicts.
    pub(super) fn repin_retained_identity_at_equal_bytes(
        &self,
        path: &Path,
        content: &str,
        identity: ContentDigest,
    ) {
        let revision = content_rev(content);
        let mut retained = self.loaded_file_identities.write().unwrap();
        if let Some((captured_revision, captured_identity)) = retained.get_mut(path) {
            if *captured_revision == revision {
                *captured_identity = identity;
            }
        }
    }

    fn advance_conflict_observation_epoch(state: &mut ConflictAuthorityState, path: &Path) -> u64 {
        let epoch = state
            .observation_epochs
            .entry(path.to_path_buf())
            .or_insert(0);
        *epoch = epoch
            .checked_add(1)
            .expect("per-path conflict observation epoch exhausted");
        state.tokens.remove(path);
        *epoch
    }

    /// Activate an editor over `rel`, minting or reusing an activation.
    ///
    /// This is deliberately NOT a read. `load_page`/`load_by_path` are
    /// mixed-purpose: some results become store editors and others are read-only,
    /// export, transient, or discarded because the page is already loaded. Minting
    /// on every DTO hand-out would mint for non-editors; minting here means an
    /// activation exists exactly when a live editor does.
    ///
    /// `Reuse` on a path that already has a live activation returns the newest
    /// one unchanged and does not burn it — plain re-hydration is idempotent.
    /// `Replace` mints a
    /// new activation while leaving the incumbent live until the frontend
    /// completes its compare-and-retire swap, which is what `reloadPage`,
    /// `reloadPageIfStillSafe` and the PDF-notes refresh genuinely require.
    ///
    /// An **absent** page (no file at `rel` yet) activates against a prospective
    /// target resolved now and returned to the caller. Holding it reserves nothing
    /// on disk, so activation never performs an unrequested write. First save
    /// re-resolves and compares, because the resolver's answer can drift when an
    /// alternate extension appears.
    pub fn activate_editor(
        &self,
        rel: &str,
        intent: ActivationIntent,
        expected_revision: Option<&str>,
    ) -> io::Result<EditorActivationHandle> {
        let abs = self.resolve_rel(rel).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "editor activation target is outside the graph",
            )
        })?;
        // A present-page installer passes the exact revision of the DTO it just
        // read. Compare it at the native activation boundary so bytes changed
        // between read and activation cannot give a stale DTO a live editor
        // identity. `None` is the deliberately snapshot-less save fallback: it
        // identifies the mounted editor only; the ordinary save's base-revision
        // guard still decides whether bytes may land and mints any conflict under
        // this activation. Absent editors use `activate_absent_editor` instead.
        let matched_baseline = if let Some(expected_revision) = expected_revision {
            let permit = self.admit_retained_graph_text_writer()?;
            match self.graph_text_read_optional_text(&permit, &abs)? {
                Some(content) if content_rev(&content) == expected_revision => Some(content),
                Some(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "activation.snapshot_changed: the file changed after the page snapshot was read",
                    ));
                }
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "activation.snapshot_missing: the expected page no longer exists",
                    ));
                }
            }
        } else {
            None
        };
        // A successfully matched expected revision proves presence even if a
        // concurrently cold/stale inventory has not indexed the file yet.
        let prospective = expected_revision.is_none() && self.entry_for_path(&abs).is_none();
        let mut state = self.editor_activations.lock().unwrap();
        if intent == ActivationIntent::Reuse {
            if let Some(record) = state.live.get(&abs).and_then(|records| records.last()) {
                return Ok(EditorActivationHandle {
                    activation: record.activation,
                    target: rel.to_owned(),
                    prospective: record.prospective,
                });
            }
        }
        state.next += 1;
        let activation = EditorActivation(state.next);
        state.live.entry(abs).or_default().push(ActivationRecord {
            activation,
            prospective,
            baseline: matched_baseline,
        });
        Ok(EditorActivationHandle {
            activation,
            target: rel.to_owned(),
            prospective,
        })
    }

    /// Activate an editor for a page that has no file yet.
    ///
    /// The frontend creates real editors that never receive a core DTO — a missing
    /// routed page via `emptyPage`, quick capture, carry-to-today, the feed's
    /// absent "today" journal. Each can take a first edit and be saved, and each
    /// can meet an external-create conflict on that very first save, so "no token"
    /// cannot be allowed to mean "not an editor".
    ///
    /// The prospective target is resolved now and returned, because
    /// "live for that path" is otherwise undefined for a page with no path. It
    /// reserves nothing on disk. First save re-resolves and compares, since the
    /// resolver's answer can drift when an alternate extension appears
    /// underneath it.
    pub fn activate_absent_editor(
        &self,
        name: &str,
        kind: PageKind,
    ) -> io::Result<EditorActivationHandle> {
        let permit = self.admit_graph_text_writer()?;
        let abs = self.graph_text_path_for(&permit, name, kind)?;
        let rel = self.rel_path(&abs);
        let mut state = self.editor_activations.lock().unwrap();
        state.next += 1;
        let activation = EditorActivation(state.next);
        state.live.entry(abs).or_default().push(ActivationRecord {
            activation,
            prospective: true,
            baseline: None,
        });
        Ok(EditorActivationHandle {
            activation,
            target: rel,
            prospective: true,
        })
    }

    pub(super) fn editor_activation_baseline(
        &self,
        path: &Path,
        activation: EditorActivation,
        expected_revision: Option<&str>,
    ) -> Option<String> {
        self.editor_activations
            .lock()
            .unwrap()
            .live
            .get(path)?
            .iter()
            .find(|record| record.activation == activation)?
            .baseline
            .clone()
            .filter(|content| expected_revision.is_none_or(|rev| content_rev(content) == rev))
    }

    pub(super) fn update_editor_activation_baseline(
        &self,
        path: &Path,
        activation: EditorActivation,
        content: &str,
    ) {
        if let Some(record) = self
            .editor_activations
            .lock()
            .unwrap()
            .live
            .get_mut(path)
            .and_then(|records| {
                records
                    .iter_mut()
                    .find(|record| record.activation == activation)
            })
        {
            record.baseline = Some(content.to_owned());
            // The first save and the frontend's activation handoff are separate
            // operations. Keep the issuing absent activation prospective until
            // `finish_saved_editor_activation` returns its exact resolved target;
            // clearing it here made that mandatory handoff disappear.
        }
    }

    /// Present a conflict observation WITHOUT writing anything.
    ///
    /// "Use disk version" is an authority-answering action just like "Keep mine",
    /// and it has to be decided by the same single source of truth. The frontend
    /// cannot decide it: the raw-watcher path revokes an observation with no page
    /// event to react to, so a locally recorded epoch can be dead while every
    /// local value still compares equal. A map maintained by eventual
    /// notifications cannot prove live membership, so it is not asked to.
    ///
    /// This consumes the authority exactly as a force would — so a stale callback
    /// cannot answer a banner twice — but performs no write at all, in every arm.
    /// The three outcomes are what the caller needs to distinguish: the discard may
    /// proceed, a newer observation superseded it, or the authority is simply gone.
    /// (GH #254 increment 3.)
    pub fn present_conflict_override(
        &self,
        rel: &str,
        base_rev: Option<&str>,
        activation: u64,
        observation_epoch: u64,
    ) -> io::Result<ConflictPresentation> {
        let Some(abs) = self.resolve_rel(rel) else {
            return Ok(ConflictPresentation::Withdrawn);
        };
        let activation = EditorActivation::from_u64(activation);
        if !self.editor_activation_is_live(&abs, activation) {
            return Ok(ConflictPresentation::Superseded);
        }
        let episode = ConflictEditorEpisode {
            loaded_revision: base_rev.map(str::to_owned),
            activation: Some(activation),
        };
        match self.consume_conflict_authority(
            &abs,
            &episode,
            ConflictOverride { observation_epoch },
        ) {
            Ok(_) => Ok(ConflictPresentation::Authorised),
            Err(error) => {
                let message = error.to_string();
                // A newer live token exists — there is a banner to answer.
                if message.contains("newer than the conflict this request answers") {
                    Ok(ConflictPresentation::Superseded)
                } else {
                    // Missing, already consumed, or a different episode: whatever
                    // the banner named is gone.
                    Ok(ConflictPresentation::Withdrawn)
                }
            }
        }
    }

    /// Retire `activation` from `rel`, but only if it is still the live one.
    ///
    /// Compare-and-retire, never a bare "retire this path": a fire-and-forget path
    /// retirement can arrive after a newer activation was installed and would then
    /// revoke the wrong editor. Returns whether anything was retired, so a caller
    /// racing a newer activation learns it was already superseded rather than
    /// silently destroying it.
    pub fn retire_editor_activation(&self, rel: &str, activation: EditorActivation) -> bool {
        let Some(abs) = self.resolve_rel(rel) else {
            return false;
        };
        let mut state = self.editor_activations.lock().unwrap();
        let retired = state.live.get_mut(&abs).and_then(|records| {
            let index = records
                .iter()
                .position(|record| record.activation == activation)?;
            records.remove(index);
            Some(records.is_empty())
        });
        match retired {
            Some(empty) => {
                if empty {
                    state.live.remove(&abs);
                }
                true
            }
            _ => false,
        }
    }

    /// Finish the absent-to-present transition after a successful first save and
    /// return the activation at its exact resolved target.
    ///
    /// The save target can move after absent activation (for example when an
    /// alternate extension appears), so the frontend cannot safely infer the
    /// path from the request it sent. Returning the core's live record lets the
    /// exact issuing editor adopt that result without minting on ordinary
    /// re-saves.
    pub fn finish_saved_editor_activation(
        &self,
        activation: EditorActivation,
    ) -> Option<EditorActivationHandle> {
        let (path, prospective) = {
            let mut state = self.editor_activations.lock().unwrap();
            let (path, record) = state.live.iter_mut().find_map(|(path, records)| {
                records
                    .iter_mut()
                    .find(|record| record.activation == activation && record.prospective)
                    .map(|record| (path, record))
            })?;
            record.prospective = false;
            (path.clone(), record.prospective)
        };
        Some(EditorActivationHandle {
            activation,
            target: self.rel_path(&path),
            prospective,
        })
    }

    /// Is `activation` the live editor for `abs` right now?
    pub(super) fn editor_activation_is_live(
        &self,
        abs: &Path,
        activation: EditorActivation,
    ) -> bool {
        let state = self.editor_activations.lock().unwrap();
        state
            .live
            .get(abs)
            .is_some_and(|records| records.iter().any(|record| record.activation == activation))
    }

    /// Is this save the FIRST one from an editor that had no file when it opened?
    ///
    /// An absent editor pins the prospective target it was given, so by the time
    /// it saves it looks pinned like any other page. The two must be told apart:
    /// a pinned path suppresses creation's semantic-owner admission today, and an
    /// absent editor still needs that admission because it is genuinely creating.
    /// The pin and the admission trigger therefore stop being the same signal, and
    /// the activation's own record is what distinguishes them. (GH #254 inc 3.)
    /// Looked up by ACTIVATION, not by path.
    ///
    /// A by-path lookup breaks the moment a prospective editor re-targets: the
    /// registry moves to the new target while the DTO still names the old pin, so
    /// the second save (typically the force answering the drift conflict) would
    /// stop recognising its own editor and land back on the abandoned path. An
    /// activation is unique within the graph, so identity is the reliable key.
    fn prospective_activation_target(&self, page: &PageDto) -> Option<PathBuf> {
        let activation = EditorActivation::from_u64(page.activation?);
        let state = self.editor_activations.lock().unwrap();
        state.live.iter().find_map(|(path, records)| {
            records
                .iter()
                .any(|record| record.activation == activation && record.prospective)
                .then(|| path.clone())
        })
    }

    pub(super) fn save_is_from_prospective_editor(&self, page: &PageDto, _abs: &Path) -> bool {
        self.prospective_activation_target(page).is_some()
    }

    /// Move a live activation onto a new target, keeping its identity.
    ///
    /// Used when an absent editor's prospective target drifts and the new target
    /// does not exist: the editor is the same editor, so its identity — and any
    /// authority bound to it — must survive the re-target rather than forcing the
    /// user through a fresh conflict for a file nobody has seen.
    fn retarget_editor_activation(
        &self,
        from: &Path,
        to: &Path,
        activation: EditorActivation,
    ) -> bool {
        let mut state = self.editor_activations.lock().unwrap();
        let Some((record, empty)) = state.live.get_mut(from).and_then(|records| {
            let index = records
                .iter()
                .position(|record| record.activation == activation)?;
            let record = records.remove(index);
            Some((record, records.is_empty()))
        }) else {
            return false;
        };
        if empty {
            state.live.remove(from);
        }
        state.live.entry(to.to_path_buf()).or_default().push(record);
        true
    }

    pub(super) fn revoke_conflict_authority(&self, path: &Path) {
        let mut state = self.conflict_authority.lock().unwrap();
        Self::advance_conflict_observation_epoch(&mut state, path);
    }

    pub(super) fn revoke_all_conflict_authority(&self) {
        let mut state = self.conflict_authority.lock().unwrap();
        let paths = state
            .observation_epochs
            .keys()
            .chain(state.tokens.keys())
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        for path in paths {
            Self::advance_conflict_observation_epoch(&mut state, &path);
        }
    }

    pub(super) fn mint_conflict_authority(
        &self,
        path: &Path,
        editor_episode: &ConflictEditorEpisode,
        snapshot: ConflictSnapshot,
        bytes: Option<String>,
    ) -> u64 {
        debug_assert_eq!(
            matches!(snapshot, ConflictSnapshot::Present { .. }),
            bytes.is_some()
        );
        let mut state = self.conflict_authority.lock().unwrap();
        let observation_epoch = Self::advance_conflict_observation_epoch(&mut state, path);
        state.tokens.insert(
            path.to_path_buf(),
            ConflictAuthority {
                snapshot,
                bytes,
                editor_episode: editor_episode.clone(),
                observation_epoch,
            },
        );
        observation_epoch
    }

    pub(super) fn consume_conflict_authority(
        &self,
        path: &Path,
        editor_episode: &ConflictEditorEpisode,
        presented: ConflictOverride,
    ) -> io::Result<ConflictAuthority> {
        let mut state = self.conflict_authority.lock().unwrap();
        // Check ownership BEFORE taking. A request that does not name the live
        // observation is not this token's owner, and spending it on their behalf
        // would leave the user unable to resolve the conflict they can actually
        // see — a stray duplicate click would disarm the banner permanently.
        if let Some(live) = state.tokens.get(path) {
            if live.observation_epoch != presented.observation_epoch {
                return Err(DirectSaveError::into_io(
                    DirectSaveFailureCode::ConflictAuthoritySuperseded,
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "conflict override authority is newer than the conflict this request answers",
                    ),
                ));
            }
        }
        let token = state.tokens.remove(path);
        let consumed_epoch = Self::advance_conflict_observation_epoch(&mut state, path) - 1;
        let token = token.ok_or_else(|| {
            DirectSaveError::into_io(
                DirectSaveFailureCode::ConflictAuthoritySpent,
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "conflict override authority is missing or already consumed",
                ),
            )
        })?;
        if token.observation_epoch != consumed_epoch || token.editor_episode != *editor_episode {
            return Err(DirectSaveError::into_io(
                DirectSaveFailureCode::ConflictAuthorityOtherEpisode,
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "conflict override authority belongs to a different editor episode",
                ),
            ));
        }
        // The caller must name the observation it was SHOWN, not merely hold a
        // path. Without this the check above is only self-consistency: it proves
        // the live token is the newest one, never that the user ever saw it.
        //
        // The reachable loss: an external writer publishes B after the banner
        // showed A. Two force requests are in flight under that one banner (a
        // double click; the button is not disabled while pending). The first
        // correctly refuses B and — being a coherent observation — mints fresh
        // authority FOR B. The second request, issued before B existed and
        // serialized behind the first by the per-page save queue, then consumed
        // that new token and overwrote B. The user never saw B and never chose
        // to discard it. (GH #254 increment 2, adversarial implementation
        // verification, finding 1.)
        debug_assert_eq!(token.observation_epoch, presented.observation_epoch);
        Ok(token)
    }

    pub(super) fn conflict_error_from_snapshot(
        &self,
        path: &Path,
        editor_episode: Option<&ConflictEditorEpisode>,
        site: EditorConflictSite,
        snapshot: ConflictSnapshot,
        bytes: Option<String>,
    ) -> io::Error {
        let Some(editor_episode) = editor_episode else {
            return DirectSaveError::into_io(
                DirectSaveFailureCode::ConflictBaseRev,
                io::Error::new(io::ErrorKind::AlreadyExists, "conflict"),
            );
        };
        let observation_epoch = self.mint_conflict_authority(path, editor_episode, snapshot, bytes);
        DirectSaveError::into_io_with_conflict_epoch(
            site.conflict_code(),
            Some(observation_epoch),
            io::Error::new(io::ErrorKind::AlreadyExists, site.message()),
        )
    }

    pub(super) fn tokenless_conflict_error(
        site: EditorConflictSite,
        observation: io::Error,
    ) -> io::Error {
        DirectSaveError::into_io(
            site.tokenless_code(),
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{}: {observation}", site.tokenless_message()),
            ),
        )
    }

    pub(super) fn observation_failure_or_hard_refusal(
        site: EditorConflictSite,
        observation: io::Error,
    ) -> io::Error {
        match observation.kind() {
            io::ErrorKind::WouldBlock
            | io::ErrorKind::Interrupted
            | io::ErrorKind::TimedOut
            | io::ErrorKind::Other => Self::tokenless_conflict_error(site, observation),
            _ => observation,
        }
    }

    pub(super) fn validate_editor_conflict_portable_path(
        &self,
        write: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<()> {
        let graph_text_path = GraphTextPath::parse(self.rel_path(path)).map_err(|error| {
            DirectSaveError::into_io(
                DirectSaveFailureCode::PrecheckNotPortable,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("guarded graph-text target is not portable: {error}"),
                ),
            )
        })?;
        self.validate_graph_text_portable_aliases_path_local(write, &graph_text_path, false)
    }

    pub(super) fn observe_editor_conflict(
        &self,
        write: &GraphTextWritePermit,
        path: &Path,
        editor_episode: Option<&ConflictEditorEpisode>,
        site: EditorConflictSite,
    ) -> io::Error {
        let Some(editor_episode) = editor_episode else {
            return DirectSaveError::into_io(
                DirectSaveFailureCode::ConflictBaseRev,
                io::Error::new(io::ErrorKind::AlreadyExists, "conflict"),
            );
        };
        if let Err(error) = self.validate_editor_conflict_portable_path(write, path) {
            return error;
        }
        if let Err(error) = conflict_observation_hook() {
            return Self::observation_failure_or_hard_refusal(site, error);
        }
        match self.graph_text_read_optional_editor_conflict_snapshot(write, path) {
            Ok(Some((bytes, resource_identity))) => {
                let revision = content_rev(&bytes);
                self.conflict_error_from_snapshot(
                    path,
                    Some(editor_episode),
                    site,
                    ConflictSnapshot::Present {
                        revision,
                        resource_identity,
                    },
                    Some(bytes),
                )
            }
            Ok(None) => self.conflict_error_from_snapshot(
                path,
                Some(editor_episode),
                site,
                ConflictSnapshot::Absent,
                None,
            ),
            Err(error) => Self::observation_failure_or_hard_refusal(site, error),
        }
    }

    pub(super) fn require_pinned_save_owner(
        &self,
        page: &PageDto,
        path: &Path,
        loaded: Option<&ExactGraphLoadedPage>,
        authority: PinnedSaveAuthority<'_>,
    ) -> io::Result<()> {
        if page.path.is_empty() {
            return Ok(());
        }
        let owner_matches = match authority {
            PinnedSaveAuthority::UserOverride(snapshot) => match (snapshot, loaded) {
                (ConflictSnapshot::Present { .. }, Some(loaded)) => loaded.entry.path == path,
                (ConflictSnapshot::Absent, None) => true,
                _ => false,
            },
            PinnedSaveAuthority::OrdinaryEditorSave {
                loaded_revision,
                prospective_editor,
            } => match loaded {
                Some(loaded) => loaded.entry.path == path,
                // A pinned editor that loaded an existing file may observe its
                // deletion and mint Absent authority. A core-minted prospective
                // activation is the separate proof that a never-loaded pinned
                // editor may create its exact target.
                None => prospective_editor || loaded_revision.is_some(),
            },
        };
        if !owner_matches {
            return Err(DirectSaveError::into_io(
                DirectSaveFailureCode::ConflictPinnedOwner,
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "path-pinned page does not match its captured exact owner",
                ),
            ));
        }
        Ok(())
    }
}
