//! Graph's write receipts and watch relevance: entry_for_path, which watcher
//! events can concern graph text, self-write markers, the exact graph-text
//! state, and commit_write / commit_editor_write.

use super::*;

impl Graph {
    /// Map an on-disk `.md` path to its page entry (journal or page), or None if
    /// it isn't in the graph's journals/pages dirs.
    pub fn entry_for_path(&self, path: &Path) -> Option<PageEntry> {
        self.graph_text_entry_for_path(path).ok().flatten()
    }

    /// True when an external filesystem event at `path` can change this graph's
    /// text inventory or its conflict list.
    ///
    /// Purely lexical and cheap, and deliberately so: the path need not exist,
    /// which lets the watcher route deletions through the same predicate it
    /// uses for creations.
    ///
    /// This exists to give the reconcile lane the *same* scope authority that
    /// discovery uses. `graph_text_inventory` walks graph-wide through
    /// `GraphTextScope`, but the watcher used to filter events against
    /// `journals/` + `pages/` alone, so an external edit to a page at the graph
    /// root or in a custom folder was watched, delivered, and then silently
    /// discarded before reconciliation (GH #268).
    pub fn graph_text_watch_relevant(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        let relative = slash_path(relative);
        if relative.is_empty() {
            return false;
        }
        if self.graph_text_scope.is_eligible(&relative) {
            return true;
        }
        // A provider conflict copy is deliberately NOT eligible text — it is
        // never cached as a page — but its appearance or removal still has to
        // refresh the conflicts panel, so the watcher must be able to see one
        // wherever an eligible document could live.
        if !path_is_sync_conflict(path) {
            return false;
        }
        let (parent, filename) = match relative.rsplit_once('/') {
            Some((parent, filename)) => (parent, filename),
            None => ("", relative.as_str()),
        };
        self.graph_text_scope.should_descend(parent)
            && filename.rsplit_once('.').is_some_and(|(_, extension)| {
                extension.eq_ignore_ascii_case("md")
                    || extension.eq_ignore_ascii_case("markdown")
                    || extension.eq_ignore_ascii_case("org")
            })
    }

    /// True when the watcher's snapshot walk may descend into the directory at
    /// `path`. Companion to [`Graph::graph_text_watch_relevant`], same authority
    /// (`GraphTextScope`), same reason: the snapshot has to cover exactly what
    /// discovery covers or a graph-wide event has nothing to reconcile against.
    pub fn graph_text_watch_descend(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        self.graph_text_scope.should_descend(&slash_path(relative))
    }

    /// What an event at `path` can change in this graph's text inventory.
    ///
    /// This is the one answer both watcher deciders use — the batch queue
    /// choosing between an exact path and a full diff, and the callback
    /// choosing whether to invalidate the guarded identity index — so that
    /// they cannot disagree about a path (GH #543, audit R9-05/R9-06).
    ///
    /// - Excluded trees (`assets/`, `node_modules/`, dot-directories,
    ///   `logseq/bak/`) reach nothing, which keeps an image drop from
    ///   rescanning the graph.
    /// - `logseq/config.edn` reaches nothing here: configuration is not graph
    ///   text and has its own queue, which decides how far a change reaches.
    /// - A path that exists answers by what it is.
    /// - A path that is gone — the old name of a rename — is a subtree only
    ///   when something that knows the graph's files holds one under it
    ///   ([`Self::gone_path_holds_files_under`]), or, when nothing can say, it
    ///   was not named like a page. Assuming a subtree whenever the old name
    ///   is gone turned an editor's atomic save of any non-page file into a
    ///   full diff of the graph and a rebuilt identity index, and asking only
    ///   the identity index (which only a page create or move builds) did so
    ///   for every external delete (GH #543, audit R10-08).
    pub fn graph_text_watch_reach(&self, path: &Path) -> GraphTextWatchReach {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return GraphTextWatchReach::Nothing;
        };
        let relative = slash_path(relative);
        if relative.is_empty() {
            return GraphTextWatchReach::Subtree;
        }
        if is_config_file_path(&self.root, path) {
            return GraphTextWatchReach::Nothing;
        }
        let file = if self.graph_text_watch_relevant(path) {
            GraphTextWatchReach::File
        } else {
            GraphTextWatchReach::Nothing
        };
        if !self.graph_text_scope.should_descend(&relative) {
            return file;
        }
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => GraphTextWatchReach::Subtree,
            Ok(_) => file,
            Err(_) => match self.gone_path_holds_files_under(&relative) {
                Some(false) => file,
                Some(true) => GraphTextWatchReach::Subtree,
                None if text_extension_from_path(path).is_some() => file,
                None => GraphTextWatchReach::Subtree,
            },
        }
    }

    /// Whether a gone path held graph files strictly under it, asked of
    /// whatever knows the graph's files without reading the disk: the current
    /// guarded identity index, the search index's complete page inventory,
    /// then a parsed cache that read every page. `None` when none of them can
    /// say. Builds nothing.
    fn gone_path_holds_files_under(&self, relative: &str) -> Option<bool> {
        let prefix = format!("{relative}/");
        {
            let state = self.guarded_graph_text_identity.read().unwrap();
            if let Some(index) = state.index.as_ref().filter(|_| !state.invalidated) {
                return Some(
                    index
                        .file_resource_by_exact_relative
                        .keys()
                        .any(|held| held.starts_with(&prefix)),
                );
            }
        }
        if let Some(held) = self
            .direct_projection
            .get()
            .and_then(|projection| projection.holds_pages_under(&prefix))
        {
            return Some(held);
        }
        let cache = self.cache.read().unwrap();
        let pages = cache.as_ref()?;
        if !self.page_index_failures.read().unwrap().is_empty() {
            return None;
        }
        Some(
            pages
                .iter()
                .any(|(entry, _)| entry.rel_path.starts_with(&prefix)),
        )
    }

    /// Record that Tine just wrote content with rev `rev` to `path`, so the file
    /// watcher recognizes the write as ours (see `sync_file_content`). The map is
    /// consumed on first match; this hard cap is a backstop so a write that the
    /// watcher never observes (file deleted before the next poll, watcher idle)
    /// can't leak across a long session. Clearing only reopens the tiny
    /// rename→cache_upsert race for genuinely in-flight writes — harmless.
    pub(super) fn note_self_write(&self, path: &Path, rev: String) {
        let mut recent = self.recent_writes.lock().unwrap();
        if recent.len() >= 1024 {
            recent.clear();
        }
        recent.insert(path.to_path_buf(), rev);
    }

    /// Drop the self-write marker for `path` once a write is fully published (after
    /// the cache_upsert), bounding it to its write window so it can never outlive
    /// this save and later suppress a real external change. Removes it only if it's
    /// still OURS (a concurrent same-path writer may have replaced it).
    pub(super) fn drop_self_write_marker(&self, path: &Path, rev: &str) {
        let mut recent = self.recent_writes.lock().unwrap();
        if recent.get(path).is_some_and(|r| r == rev) {
            recent.remove(path);
        }
    }

    pub(super) fn remember_exact_graph_text_state(
        &self,
        path: &Path,
        revision: String,
        resource_identity: ContentDigest,
    ) {
        let mut recent = self.recent_graph_text_states.lock().unwrap();
        if recent.len() >= 1024 && !recent.contains_key(path) {
            // This is only an optimization receipt. Losing it makes the next
            // callback take the ordinary fail-closed external frontier; it can
            // never authorize a write or weaken a collision check.
            recent.clear();
        }
        recent.insert(
            path.to_path_buf(),
            ExactGraphTextStateReceipt {
                revision,
                resource_identity,
            },
        );
    }

    pub(super) fn retire_exact_graph_text_state(&self, path: &Path) {
        self.recent_graph_text_states.lock().unwrap().remove(path);
    }

    /// Return true only when an exact native-watcher path is still the physical
    /// file and byte state Tine just published, or the identical state already
    /// admitted into the cache. The callback calls this before raising the
    /// graph-wide external-observation frontier.
    ///
    /// The writer permit, graph-text identity authority and per-path lock use
    /// the same order as `save_page`. A callback delivered during atomic
    /// publication therefore waits until the writer has completed its final
    /// no-follow reread and cache publication. Two coherent snapshots then bind
    /// the proof to the exact path, content revision and file identity. A sync
    /// service or second Tine which replaced the path, even with identical
    /// bytes, has a different identity and takes the ordinary external lane.
    pub fn exact_graph_text_event_matches_tine_state(&self, path: &Path) -> bool {
        if !self.recent_writes.lock().unwrap().contains_key(path)
            && !self
                .recent_graph_text_states
                .lock()
                .unwrap()
                .contains_key(path)
        {
            return false;
        }
        exact_graph_text_event_after_candidate_hook();
        let Ok(write) = self.admit_graph_text_writer() else {
            return false;
        };
        let Ok(_identity) = self.lock_graph_text_identity_mutation() else {
            return false;
        };
        let lock = self.page_lock(path);
        let Ok(_guard) = lock.lock() else {
            return false;
        };
        let first = match self.graph_text_read_optional_text_with_identity(&write, path) {
            Ok(Some(snapshot)) => snapshot,
            _ => return false,
        };
        let second = match self.graph_text_read_optional_text_with_identity(&write, path) {
            Ok(Some(snapshot)) => snapshot,
            _ => return false,
        };
        if first != second {
            return false;
        }
        let revision = content_rev(&second.0);
        let publication_matches = self
            .recent_graph_text_states
            .lock()
            .unwrap()
            .get(path)
            .is_some_and(|receipt| {
                receipt.revision == revision && receipt.resource_identity == second.1
            });
        let accepted_matches = self
            .disk_revs
            .read()
            .unwrap()
            .get(path)
            .is_some_and(|accepted| accepted == &revision)
            && self
                .loaded_file_identities
                .read()
                .unwrap()
                .get(path)
                .is_some_and(|(accepted_revision, accepted_identity)| {
                    accepted_revision == &revision && *accepted_identity == second.1
                });
        publication_matches || accepted_matches
    }

    /// Remove a transaction-owned live file without ever unlinking a race winner.
    /// The currently named inode is first moved atomically into recoverable
    /// conflict trash. Exact expected bytes stay there as the withdrawn copy; a
    /// different inode is restored if the live name is free, or retained in
    /// recovery if another writer has already recreated the name.
    pub(super) fn withdraw_file_to_conflict_if_exact(
        &self,
        write: &GraphTextWritePermit,
        path: &Path,
        expected: &[u8],
        reason: &str,
    ) -> io::Result<bool> {
        withdrawal_race_hook(path)?;
        if !self.graph_text_exists(write, path)? {
            return Ok(false);
        }
        let trash = typed_trash_dir(&self.root, TrashEntryKind::Conflict);
        self.graph_text_create_dir_all(write, &trash)?;
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("file");
        let staged = trash.join(format!("{}__{reason}__{name}", trash_stamp()));
        match self.graph_text_move_noreplace(write, path, &staged) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        }
        let staged_matches = match self.graph_text_file_equals_bytes(write, &staged, expected) {
            Ok(matches) => matches,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let _ = self.graph_text_move_noreplace(write, &staged, path);
                return Err(io::Error::from(io::ErrorKind::NotFound));
            }
            Err(error) => return Err(error),
        };
        if staged_matches {
            return Ok(true);
        }
        match self.graph_text_move_noreplace(write, &staged, path) {
            Ok(()) => Ok(false),
            // A new live winner appeared after staging. Keeping the displaced
            // inode in conflict trash preserves both versions.
            Err(_) if self.graph_text_exists(write, path).unwrap_or(false) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The shared page-write commit protocol, written ONCE so editor saves and
    /// highlight saves cannot drift on marker lifecycle
    /// or cross the mutation boundary through a second writer:
    ///   record self-write marker → optional parent preparation / last-moment
    ///   baseline recheck → the selected guarded publication strategy.
    /// We hold the page lock, so no other Tine writer raced us; the recheck guards
    /// a non-cooperating external writer (OG/Syncthing) that touched the file since
    /// our baseline read — on mismatch we abort WITHOUT writing and drop our marker
    /// so the watcher still sees the external change. Returns the new content rev.
    /// The post-publish marker drop is `drop_self_write_marker` (it must run AFTER
    /// the caller's cache_upsert, so it stays the caller's responsibility).
    fn commit_write<T>(
        &self,
        write: &GraphTextWritePermit,
        path: &Path,
        content: &str,
        baseline: Option<&str>,
        recheck: bool,
        create_parent: bool,
        editor_episode: Option<&ConflictEditorEpisode>,
        publish: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<(String, T)> {
        let rev = content_rev(content);
        self.note_self_write(path, rev.clone());
        let result = (|| {
            if create_parent {
                if let Some(parent) = path.parent() {
                    self.graph_text_create_dir_all(write, parent)?;
                }
            }
            if recheck {
                editor_commit_before_recheck_hook()?;
                // Only NotFound means "no baseline file". Permission errors, invalid
                // UTF-8, and transient I/O failures must abort; collapsing them to
                // None would authorize an overwrite of unreadable on-disk data.
                let now = match self.graph_text_read_optional_editor_conflict_snapshot(write, path)
                {
                    Ok(now) => now,
                    Err(error) if editor_episode.is_some() => {
                        return Err(Self::observation_failure_or_hard_refusal(
                            EditorConflictSite::CommitRecheck,
                            error,
                        ));
                    }
                    Err(error) => return Err(error),
                };
                let still_matches = match (now.as_ref().map(|(text, _)| text.as_str()), baseline) {
                    (Some(n), Some(e)) => n == e,
                    (None, None) => true,
                    _ => false,
                };
                if !still_matches {
                    if let Err(error) = self.validate_editor_conflict_portable_path(write, path) {
                        return Err(error);
                    }
                    let error = match now {
                        Some((bytes, resource_identity)) => self.conflict_error_from_snapshot(
                            path,
                            editor_episode,
                            EditorConflictSite::CommitRecheck,
                            ConflictSnapshot::Present {
                                revision: content_rev(&bytes),
                                resource_identity,
                            },
                            Some(bytes),
                        ),
                        None => self.conflict_error_from_snapshot(
                            path,
                            editor_episode,
                            EditorConflictSite::CommitRecheck,
                            ConflictSnapshot::Absent,
                            None,
                        ),
                    };
                    return Err(error);
                }
            }
            publish()
        })();
        match result {
            Ok(published) => {
                if let Some(activation) = editor_episode.and_then(|episode| episode.activation) {
                    self.update_editor_activation_baseline(path, activation, content);
                }
                // Concord ledger (ADR 0056): these exact bytes are now on disk,
                // so they are the last text Tine and the disk agree on. Off the
                // save critical path — one channel send, work happens on the
                // ledger's worker thread; failures are logged, never surfaced.
                self.concord_record_agreed(path, content);
                Ok((rev, published))
            }
            Err(error) => {
                self.drop_self_write_marker(path, &rev);
                Err(error)
            }
        }
    }

    pub(super) fn commit_editor_write(
        &self,
        write: &GraphTextWritePermit,
        path: &Path,
        content: &str,
        baseline: Option<&str>,
        recheck: bool,
        expected_identity: Option<ContentDigest>,
        editor_episode: Option<&ConflictEditorEpisode>,
        creation_proof: Option<DirectCreationProof>,
        turn_short_id: Option<[u8; 4]>,
    ) -> io::Result<String> {
        // The Direct existing-file replacement already performs the late
        // baseline proof at the stronger boundary: it atomically retires the
        // exact expected inode, reads that detached inode, and restores it on a
        // byte mismatch before reporting the conflict. Reading the live name in
        // `commit_write` immediately beforehand duplicated a full-file read
        // without closing an additional race. Keep that earlier recheck for
        // creates and unpinned auxiliary writes.
        let commit_recheck = recheck && expected_identity.is_none();
        let create_parent = creation_proof.is_none();
        let result = (|| {
            let (rev, ()) = self.commit_write(
                write,
                path,
                content,
                baseline,
                commit_recheck,
                create_parent,
                editor_episode,
                || match (expected_identity, creation_proof) {
                    (Some(identity), _) => self.graph_text_atomic_replace_bound(
                        write,
                        path,
                        content.as_bytes(),
                        identity,
                        recheck.then_some(baseline).flatten().map(str::as_bytes),
                        editor_episode,
                        turn_short_id,
                    ),
                    (None, Some(creation_proof)) if baseline.is_none() => self
                        .graph_text_atomic_create_with_proof(
                            write,
                            path,
                            content.as_bytes(),
                            creation_proof,
                            editor_episode,
                        ),
                    (None, _) => self.graph_text_atomic_write_with_conflict(
                        write,
                        path,
                        content.as_bytes(),
                        baseline.is_none(),
                        editor_episode,
                    ),
                },
            )?;
            editor_commit_before_final_reread_hook()?;
            let reread = match self.graph_text_read_optional_editor_conflict_snapshot(write, path) {
                Ok(reread) => reread,
                Err(error) if editor_episode.is_some() => {
                    return Err(Self::observation_failure_or_hard_refusal(
                        EditorConflictSite::FinalRereadPresent,
                        error,
                    ));
                }
                Err(error) => return Err(error),
            };
            let Some((reread, identity)) = reread else {
                self.validate_editor_conflict_portable_path(write, path)?;
                return Err(self.conflict_error_from_snapshot(
                    path,
                    editor_episode,
                    EditorConflictSite::FinalRereadAbsent,
                    ConflictSnapshot::Absent,
                    None,
                ));
            };
            if reread != content || content_rev(&reread) != rev {
                if editor_episode.is_some() {
                    self.validate_editor_conflict_portable_path(write, path)?;
                    return Err(self.conflict_error_from_snapshot(
                        path,
                        editor_episode,
                        EditorConflictSite::FinalRereadPresent,
                        ConflictSnapshot::Present {
                            revision: content_rev(&reread),
                            resource_identity: identity,
                        },
                        Some(reread),
                    ));
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph text final reread does not match published bytes",
                ));
            }
            self.loaded_file_identities
                .write()
                .unwrap()
                .insert(path.to_path_buf(), (rev.clone(), identity));
            self.remember_exact_graph_text_state(path, rev.clone(), identity);
            Ok(rev)
        })();
        if result.is_err() {
            self.reconcile_failed_graph_text_paths(write, std::iter::once(path));
        }
        result
    }
}
