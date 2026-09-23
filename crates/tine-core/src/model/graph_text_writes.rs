//! Graph's graph-text write primitives: atomic create, write and replace, the
//! no-replace moves, and move-to-trash, each behind its validated target.

use super::*;

impl Graph {
    /// Reconcile readable final owners after a mutation or its compensation
    /// failed. The caller retains its identity mutation permit throughout.
    pub(super) fn reconcile_failed_graph_text_paths<'a>(
        &self,
        permit: &GraphTextWritePermit,
        paths: impl IntoIterator<Item = &'a Path>,
    ) {
        let mut seen = std::collections::HashSet::new();
        for path in paths {
            if !seen.insert(path) {
                continue;
            }
            let Some(entry) = self.entry_for_path(path) else {
                continue;
            };
            self.recent_writes.lock().unwrap().remove(path);
            match self.graph_text_read_optional_text(permit, path) {
                Ok(Some(content)) => match parse_exact_page(self, &entry, &content) {
                    Ok((entry, document, revision)) => self.cache_upsert(entry, document, revision),
                    // Unreadable now: recorded, or the name it may own stops
                    // being refused (audit R15-02).
                    Err(_) => self.record_watcher_identity_failure(path, false),
                },
                Ok(None) => self.cache_remove_path(&entry),
                Err(_) => self.record_watcher_identity_failure(path, false),
            }
        }
    }

    fn validate_direct_creation_proof_before_mutation(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        proof: &DirectCreationProof,
    ) -> io::Result<()> {
        let graph_text_path = GraphTextPath::parse(self.rel_path(path)).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("guarded graph-text target is not portable: {error}"),
            )
        })?;
        if graph_text_path != proof.target {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "creation proof does not bind one absent exact target",
            ));
        }
        if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) != proof.generation {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "effective page identity evidence changed before creation publication",
            ));
        }
        self.validate_graph_text_portable_aliases_path_local(permit, &graph_text_path, true)?;
        match self.graph_text_target(permit, path, false) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(target) => match target.parent().symlink_metadata(&target.filename) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
                Ok(_) => {
                    projection_optional_regular_metadata(target.parent(), &target.filename)?;
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "direct creation target is already present",
                    ));
                }
            },
        }
        if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) != proof.generation {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "effective page identity evidence changed during creation publication validation",
            ));
        }
        Ok(())
    }

    pub(super) fn graph_text_atomic_create_with_proof(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        bytes: &[u8],
        proof: DirectCreationProof,
        editor_episode: Option<&ConflictEditorEpisode>,
    ) -> io::Result<()> {
        let _identity = self.lock_graph_text_identity_mutation()?;
        self.validate_direct_creation_proof_before_mutation(permit, path, &proof)?;
        let target = self.graph_text_target(permit, path, true)?;
        // Parent creation is itself a mutation, so the first validation above
        // precedes it. Re-run only the path-local portable/no-follow boundary
        // after the chain exists; the graph-wide census remains singular.
        self.validate_direct_creation_proof_before_mutation(permit, path, &proof)?;
        let temp = create_projection_temp(target.parent(), &target.filename, bytes)?;
        graph_text_write_before_mutation_hook()?;
        if self.graph_text_external_observation_pending() {
            let _ = target.parent().remove_file(&temp);
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "external graph-text changes arrived before creation publication",
            ));
        }
        if let Err(error) =
            self.validate_graph_text_portable_aliases_path_local(permit, &proof.target, true)
        {
            let _ = target.parent().remove_file(&temp);
            return Err(error);
        }
        if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) != proof.generation {
            let _ = target.parent().remove_file(&temp);
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "effective page identity evidence changed before no-replace publication",
            ));
        }
        if let Err(error) =
            move_graph_text_exact_no_replace(target.parent(), &temp, &target.filename, bytes)
        {
            let _ = target.parent().remove_file(&temp);
            if error.kind() == io::ErrorKind::AlreadyExists && editor_episode.is_some() {
                return Err(self.observe_editor_conflict(
                    permit,
                    path,
                    editor_episode,
                    EditorConflictSite::CreatePublicationCollision,
                ));
            }
            return Err(error);
        }
        self.finish_tine_owned_graph_text_identity_paths(std::iter::once(path))
    }

    pub(super) fn graph_text_atomic_write_with_conflict(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        bytes: &[u8],
        create_new: bool,
        editor_episode: Option<&ConflictEditorEpisode>,
    ) -> io::Result<()> {
        self.graph_text_atomic_write_validated(
            permit,
            path,
            bytes,
            create_new,
            editor_episode,
            GraphTextPublicationValidation::CompleteIndex,
        )
    }

    pub(super) fn graph_text_atomic_write_from_transaction_inventory(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        bytes: &[u8],
        create_new: bool,
    ) -> io::Result<()> {
        self.graph_text_atomic_write_validated(
            permit,
            path,
            bytes,
            create_new,
            None,
            GraphTextPublicationValidation::TransactionInventory,
        )
    }

    fn graph_text_atomic_write_validated(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        bytes: &[u8],
        create_new: bool,
        editor_episode: Option<&ConflictEditorEpisode>,
        validation: GraphTextPublicationValidation,
    ) -> io::Result<()> {
        if !create_new {
            let expected_identity = self
                .graph_text_optional_file_identity(permit, path)?
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            // There is deliberately NO alias check here. Naming the other graph
            // page that holds this inode requires the complete identity index,
            // and an existing save must never build it (GH #267) — that is what
            // keeps a save O(1) rather than O(graph). The raw link count that
            // used to stand in for the check on this path could not tell a
            // graph sibling from git-annex's `.git/annex/objects` link, so it
            // refused every save on an annexed or deduplicated graph (GH #571,
            // GH #555). Martin, 2026-09-21: allow it here. The precise refusal
            // stays on page creation, where the index is already in hand, and
            // the non-index move paths keep the blanket rule for now.
            // `existing_save_local_proofs_cover_hardlinks_and_index_uncertainty`
            // pins both halves of this: the save is allowed, and it builds no
            // complete generation.
            return self.graph_text_atomic_replace_bound(
                permit,
                path,
                bytes,
                expected_identity,
                None,
                editor_episode,
                None,
            );
        }
        let _identity = self.lock_graph_text_identity_mutation()?;
        // Establish the retained baseline before creating the staged inode. The
        // temp name is deliberately outside the graph-text namespace, but its
        // physical identity would otherwise be captured as a second owner when
        // the staged inode is later published at `path`.
        if validation == GraphTextPublicationValidation::CompleteIndex {
            let _ = self.guarded_graph_text_identity_index()?;
        }
        let graph_text_path = GraphTextPath::parse(self.rel_path(path)).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("guarded graph-text target is not portable: {error}"),
            )
        })?;
        if validation == GraphTextPublicationValidation::TransactionInventory {
            self.validate_graph_text_portable_aliases_path_local(
                permit,
                &graph_text_path,
                create_new,
            )?;
        }
        let target = self.graph_text_target(permit, path, true)?;
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let temp = create_projection_temp(target.parent(), &target.filename, bytes)?;
        graph_text_write_before_mutation_hook()?;
        let validation_result = match validation {
            GraphTextPublicationValidation::CompleteIndex => self
                .validate_current_graph_text_collision_strict(
                    permit,
                    path,
                    self.graph_text_optional_file_identity(permit, path)?,
                )
                .map(|_| ()),
            GraphTextPublicationValidation::PathLocal
            | GraphTextPublicationValidation::TransactionInventory => (|| {
                self.validate_graph_text_portable_aliases_path_local(
                    permit,
                    &graph_text_path,
                    create_new,
                )?;
                match target.parent().symlink_metadata(&target.filename) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(error),
                    Ok(_) => Err(io::Error::from(io::ErrorKind::AlreadyExists)),
                }
            })(),
        };
        if let Err(error) = validation_result {
            let _ = target.parent().remove_file(&temp);
            return Err(error);
        }
        let result =
            move_graph_text_exact_no_replace(target.parent(), &temp, &target.filename, bytes);
        if let Err(error) = result {
            let _ = target.parent().remove_file(&temp);
            if error.kind() == io::ErrorKind::AlreadyExists && editor_episode.is_some() {
                return Err(self.observe_editor_conflict(
                    permit,
                    path,
                    editor_episode,
                    EditorConflictSite::CreatePublicationCollision,
                ));
            }
            return Err(error);
        }
        self.finish_tine_owned_graph_text_identity_paths(std::iter::once(path))
    }

    /// Replace an existing editor target without ever issuing an overwrite
    /// rename against its live name. The old name is first retired with
    /// no-replace through the retained parent capability, then its exact file
    /// identity (and, for normal saves, bytes) are validated from the retired
    /// inode. A different file installed after the caller's final check is moved
    /// back with the same no-replace primitive; its bytes and identity survive.
    ///
    /// Publication also uses no-replace, so an external creator in the brief
    /// retired-name interval wins. In that case the displaced inode remains in a
    /// same-directory hidden recovery name and the user's staged bytes remain in
    /// their own hidden staged-recovery name instead of any version being
    /// overwritten.
    pub(super) fn graph_text_atomic_replace_bound(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        bytes: &[u8],
        expected_identity: ContentDigest,
        expected_bytes: Option<&[u8]>,
        editor_episode: Option<&ConflictEditorEpisode>,
        turn_short_id: Option<[u8; 4]>,
    ) -> io::Result<()> {
        let _identity = self.lock_graph_text_identity_mutation()?;
        use std::sync::atomic::{AtomicU64, Ordering};
        static RECOVERY_SEQ: AtomicU64 = AtomicU64::new(0);

        let target = self.graph_text_target(permit, path, false)?;
        let graph_text_path = GraphTextPath::parse(self.rel_path(path)).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("guarded graph-text target is not portable: {error}"),
            )
        })?;
        if let Err(error) =
            self.validate_existing_graph_text_target_exact(&target, Some(expected_identity))
        {
            if editor_episode.is_some()
                && (error.kind() == io::ErrorKind::NotFound
                    || error
                        .to_string()
                        .contains("changed at the local identity validation boundary"))
            {
                return Err(self.observe_editor_conflict(
                    permit,
                    path,
                    editor_episode,
                    EditorConflictSite::ReplacePreRetirement,
                ));
            }
            return Err(error);
        }
        preflight_projection_chain(&target.chain)?;
        let temp =
            create_editor_staged_recovery(target.parent(), &target.filename, bytes, turn_short_id)?;
        let staged_identity = match (|| {
            let staged_file = open_projection_file_nofollow(target.parent(), &temp)?;
            let identity = canonical_projection_file_resource_id(&staged_file)?;
            Ok::<_, io::Error>(identity)
        })() {
            Ok(identity) => identity,
            Err(error) => {
                let _ = target.parent().remove_file(&temp);
                return Err(error);
            }
        };
        let process = std::process::id();
        let sequence = RECOVERY_SEQ.fetch_add(1, Ordering::Relaxed);
        let recovery = match turn_short_id {
            Some(turn) => format!(
                ".{}.{process}.{sequence}.{}.editor-recovery",
                target.filename,
                short_turn_id(turn),
            ),
            None => format!(".{}.{process}.{sequence}.editor-recovery", target.filename,),
        };
        let retired_cleanup = format!(".{}.{process}.{sequence}.editor-retired", target.filename,);
        let mut retired = false;
        let mut published = false;
        let mut conflict_site = None;
        let mut retired_conflict_snapshot = None;
        let mut restore_succeeded = false;
        let result = (|| {
            // Deterministic tests replace the target here: after normal-save's
            // final byte reread and after force-save's final retained-identity
            // validation, but before the first live-name mutation.
            graph_text_write_before_mutation_hook()?;
            self.validate_graph_text_portable_aliases_path_local(permit, &graph_text_path, false)?;
            if let Err(error) =
                self.validate_existing_graph_text_target_exact(&target, Some(expected_identity))
            {
                if editor_episode.is_some()
                    && (error.kind() == io::ErrorKind::NotFound
                        || error
                            .to_string()
                            .contains("changed at the local identity validation boundary"))
                {
                    return Err(self.observe_editor_conflict(
                        permit,
                        path,
                        editor_episode,
                        EditorConflictSite::ReplacePreRetirement,
                    ));
                }
                return Err(error);
            }
            let rename_noreplace = |from: &str, to: &str, expected: &[u8]| {
                move_graph_text_exact_no_replace(target.parent(), from, to, expected)
            };
            let (live_file, live_bytes) =
                open_and_read_projection_regular(target.parent(), &target.filename)?;
            if canonical_projection_file_resource_id(&live_file)? != expected_identity {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "graph text target changed before durable retirement",
                ));
            }
            drop(live_file);
            rename_noreplace(&target.filename, &recovery, &live_bytes)?;
            retired = true;

            let (retired_file, retired_bytes) =
                open_and_read_projection_regular(target.parent(), &recovery)?;
            let retired_identity = canonical_projection_file_resource_id(&retired_file)?;
            drop(retired_file);
            if retired_identity != expected_identity
                || expected_bytes.is_some_and(|expected| retired_bytes != expected)
            {
                conflict_site = Some(EditorConflictSite::ReplaceRetiredMismatch);
                retired_conflict_snapshot = Some((retired_bytes, retired_identity));
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "graph text target changed at the identity-bound publication boundary",
                ));
            }
            graph_text_write_after_retire_hook()?;

            let staged_file = open_projection_file_nofollow(target.parent(), &temp)?;
            if canonical_projection_file_resource_id(&staged_file)? != staged_identity {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "staged editor identity changed before publication",
                ));
            }
            drop(staged_file);

            if let Err(error) = rename_noreplace(&temp, &target.filename, bytes) {
                if error.kind() == io::ErrorKind::AlreadyExists && editor_episode.is_some() {
                    conflict_site = Some(EditorConflictSite::ReplacePublicationCollision);
                }
                return Err(error);
            }
            published = true;
            journal_projection_after_publish_hook()?;
            if let Err(error) =
                self.validate_existing_graph_text_target_exact(&target, Some(staged_identity))
            {
                if editor_episode.is_some()
                    && (error.kind() == io::ErrorKind::NotFound
                        || error
                            .to_string()
                            .contains("changed at the local identity validation boundary"))
                {
                    conflict_site = Some(EditorConflictSite::ReplacePostPublication);
                }
                return Err(error);
            }
            move_graph_text_exact_no_replace(
                target.parent(),
                &recovery,
                &retired_cleanup,
                &retired_bytes,
            )?;
            let _ = target.parent().remove_file(&retired_cleanup);
            retired = false;
            Ok(())
        })();

        let outcome = match result {
            Ok(()) => {
                let _ = target.parent().remove_file(&temp);
                Ok(())
            }
            Err(primary) => {
                if retired && !published {
                    let restore = graph_text_write_before_restore_hook().and_then(|()| {
                        let recovery_file =
                            open_projection_file_nofollow(target.parent(), &recovery)?;
                        if canonical_projection_file_resource_id(&recovery_file)?
                            != expected_identity
                        {
                            return Err(io::Error::new(
                                io::ErrorKind::AlreadyExists,
                                "displaced target identity changed before restore",
                            ));
                        }
                        let recovery_bytes = read_projection_regular(target.parent(), &recovery)?;
                        move_graph_text_exact_no_replace(
                            target.parent(),
                            &recovery,
                            &target.filename,
                            &recovery_bytes,
                        )
                    });
                    match restore {
                        Ok(()) => {
                            retired = false;
                            restore_succeeded = true;
                            debug_assert!(!retired || published);
                            let _ = target.parent().remove_file(&temp);
                            Err(primary)
                        }
                        Err(restore_error) => Err(io::Error::new(
                            primary.kind(),
                            format!(
                                "{primary}; displaced target retained as {recovery}, \
                                     staged editor bytes retained as {temp}, \
                                     but exact-identity restore failed: {restore_error}"
                            ),
                        )),
                    }
                } else {
                    debug_assert!(!retired || published);
                    let _ = target.parent().remove_file(&temp);
                    Err(primary)
                }
            }
        };
        match outcome {
            Ok(()) => self.finish_tine_owned_graph_text_identity_paths(std::iter::once(path)),
            Err(mut error) => {
                if let Some(site) = conflict_site {
                    error = if matches!(site, EditorConflictSite::ReplaceRetiredMismatch)
                        && restore_succeeded
                    {
                        match retired_conflict_snapshot {
                            Some((bytes, resource_identity)) => match String::from_utf8(bytes) {
                                Ok(bytes) => self.conflict_error_from_snapshot(
                                    path,
                                    editor_episode,
                                    site,
                                    ConflictSnapshot::Present {
                                        revision: content_rev(&bytes),
                                        resource_identity,
                                    },
                                    Some(bytes),
                                ),
                                Err(_) => io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "graph text file is not valid UTF-8",
                                ),
                            },
                            None => error,
                        }
                    } else {
                        self.observe_editor_conflict(permit, path, editor_episode, site)
                    };
                }
                self.invalidate_guarded_graph_text_identity(format!(
                    "identity-bound replacement failed after staging: {error}"
                ));
                Err(error)
            }
        }
    }

    pub(super) fn graph_text_move_noreplace(
        &self,
        permit: &GraphTextWritePermit,
        source: &Path,
        destination: &Path,
    ) -> io::Result<()> {
        self.graph_text_move_noreplace_validated(
            permit,
            source,
            destination,
            GraphTextPublicationValidation::PathLocal,
        )
    }

    pub(super) fn graph_text_move_noreplace_from_transaction_inventory(
        &self,
        permit: &GraphTextWritePermit,
        source: &Path,
        destination: &Path,
    ) -> io::Result<()> {
        self.graph_text_move_noreplace_validated(
            permit,
            source,
            destination,
            GraphTextPublicationValidation::TransactionInventory,
        )
    }

    /// Move one exact hidden file produced by the editor publication protocol.
    /// The retained source is validated directly rather than through
    /// `GraphTextPath`: hidden publication artifacts are not ordinary documents,
    /// and recovery must not treat them as such. Note that `GraphTextPath` itself
    /// does NOT reject a leading-dot name — `is_graph_text_path` only requires a
    /// non-empty stem and a graph-text extension — so this validation is the
    /// boundary, not a redundant second check.
    /// `graph_text_path_accepts_leading_dot_name` in `graph_text_path` keeps that
    /// statement honest.
    pub(super) fn graph_text_move_editor_recovery_noreplace(
        &self,
        permit: &GraphTextWritePermit,
        source_path: &Path,
        destination_path: &Path,
    ) -> io::Result<ContentDigest> {
        let _identity = self.lock_graph_text_identity_mutation()?;
        let destination_graph_text = GraphTextPath::parse(self.rel_path(destination_path))
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("editor recovery destination is not portable: {error}"),
                )
            })?;
        self.validate_graph_text_portable_aliases_path_local(
            permit,
            &destination_graph_text,
            true,
        )?;

        let source = self.graph_text_target(permit, source_path, false)?;
        projection_optional_regular_metadata(source.parent(), &source.filename)?;
        let source_file = open_projection_file_nofollow(source.parent(), &source.filename)?;
        let source_identity = canonical_projection_file_resource_id(&source_file)?;
        // No link-count check: a move keeps the inode, so any other name linked
        // to this artifact keeps exactly the bytes it had (GH #571, GH #555).
        drop(source_file);

        let destination = self.graph_text_target(permit, destination_path, true)?;
        match destination.parent().symlink_metadata(&destination.filename) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(error) => return Err(error),
        }
        graph_text_write_before_mutation_hook()?;
        let rebound = open_projection_file_nofollow(source.parent(), &source.filename)?;
        if canonical_projection_file_resource_id(&rebound)? != source_identity {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "editor recovery artifact changed before reconciliation",
            ));
        }
        self.validate_graph_text_portable_aliases_path_local(
            permit,
            &destination_graph_text,
            true,
        )?;
        match destination.parent().symlink_metadata(&destination.filename) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(error) => return Err(error),
        }
        rename_graph_text_noreplace(
            source.parent(),
            &source.filename,
            destination.parent(),
            &destination.filename,
        )?;
        // Quarantine/restore destinations are sole-authority names. Make the
        // destination chain durable before the source removal: a crash between
        // these barriers may leave a duplicate source entry on non-journaling
        // media, but can never lose the retained object (§4.5).
        sync_projection_chain_required(&destination.chain)?;
        sync_projection_chain_required(&source.chain)?;
        self.finish_tine_owned_graph_text_identity_paths(std::iter::once(destination_path))?;
        Ok(source_identity)
    }

    fn graph_text_move_noreplace_validated(
        &self,
        permit: &GraphTextWritePermit,
        source: &Path,
        destination: &Path,
        validation: GraphTextPublicationValidation,
    ) -> io::Result<()> {
        let _identity = self.lock_graph_text_identity_mutation()?;
        if validation == GraphTextPublicationValidation::CompleteIndex {
            let _ = self.guarded_graph_text_identity_index()?;
        }
        let source_path = source.to_path_buf();
        let destination_path = destination.to_path_buf();
        let source_graph_text =
            GraphTextPath::parse(self.rel_path(&source_path)).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("guarded graph-text source is not portable: {error}"),
                )
            })?;
        let destination_graph_text = GraphTextPath::parse(self.rel_path(&destination_path))
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("guarded graph-text destination is not portable: {error}"),
                )
            })?;
        if validation != GraphTextPublicationValidation::CompleteIndex {
            self.validate_graph_text_portable_aliases_path_local(
                permit,
                &source_graph_text,
                false,
            )?;
            self.validate_graph_text_portable_aliases_path_local(
                permit,
                &destination_graph_text,
                true,
            )?;
        }
        let source = self.graph_text_target(permit, &source_path, false)?;
        projection_optional_regular_metadata(source.parent(), &source.filename)?;
        let destination = self.graph_text_target(permit, &destination_path, true)?;
        match destination.parent().symlink_metadata(&destination.filename) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(error) => return Err(error),
        }
        graph_text_write_before_mutation_hook()?;
        if validation != GraphTextPublicationValidation::CompleteIndex {
            self.validate_graph_text_portable_aliases_path_local(
                permit,
                &source_graph_text,
                false,
            )?;
            self.validate_existing_graph_text_target_exact(&source, None)?;
            // No link-count check on the move source. `MS-REF-GRAPH-TEXT-ALIAS`
            // is the divergence temp + rename PUBLICATION causes by giving one
            // name a new inode; a move keeps the inode, so every other name
            // linked to it keeps exactly the bytes it had. The raw count this
            // used to refuse on defended nothing and made every page of an
            // annexed or deduplicated graph unrenamable (GH #571, GH #555).
            self.validate_graph_text_portable_aliases_path_local(
                permit,
                &destination_graph_text,
                true,
            )?;
            match destination.parent().symlink_metadata(&destination.filename) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
                Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            }
        }
        rename_graph_text_noreplace(
            source.parent(),
            &source.filename,
            destination.parent(),
            &destination.filename,
        )?;
        let result = (|| {
            sync_projection_chain_required(&source.chain)?;
            sync_projection_chain_required(&destination.chain)?;
            self.finish_tine_owned_graph_text_identity_paths([
                source_path.as_path(),
                destination_path.as_path(),
            ])
        })();
        // The rename has already committed. Preserve its durability/identity
        // error, but publish the filesystem state even when the caller exits
        // before its ordinary successful-mutation publication.
        if result.is_err() {
            self.reconcile_failed_graph_text_paths(
                permit,
                [source_path.as_path(), destination_path.as_path()],
            );
        }
        result
    }

    pub(super) fn graph_text_move_to_trash(
        &self,
        permit: &GraphTextWritePermit,
        source: &Path,
        destination: &Path,
        trash: &Path,
    ) -> io::Result<()> {
        self.graph_text_create_dir_all(permit, trash)
            .map_err(|error| {
                let display = trash.strip_prefix(&self.root).unwrap_or(trash).display();
                io::Error::new(
                    error.kind(),
                    format!("could not prepare trash path {display}: {error}"),
                )
            })?;
        self.graph_text_move_noreplace(permit, source, destination)
    }
}
