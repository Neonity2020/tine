//! Graph's external-change intake: re-reading a file the watcher reported, and
//! forgetting a file that was deleted outside Tine.

use super::*;

impl Graph {
    /// Reconcile a (possibly externally-changed) file with the in-memory cache.
    /// Returns the entry only if its parsed content actually differs from the
    /// cache (i.e. a real external change) — Tine's own writes keep the cache in
    /// sync, so they return None. No-op if the cache hasn't been built yet.
    pub fn sync_file(&self, path: &Path) -> Option<PageEntry> {
        match self.sync_file_checked(path) {
            Ok(entry) => entry,
            Err(_error) => {
                #[cfg(debug_assertions)]
                if crate::backend_error::runtime_debug_diagnostics_enabled() {
                    eprintln!("file reconcile deferred after a content-free I/O failure");
                }
                None
            }
        }
    }

    /// Checked watcher entrypoint for an externally changed page file.
    pub fn sync_file_checked(&self, path: &Path) -> io::Result<Option<PageEntry>> {
        let write = self.admit_graph_text_writer()?;
        let _identity = self.lock_graph_text_identity_mutation()?;
        let lock = self.page_lock(path);
        let _guard = lock.lock().unwrap();
        self.revoke_conflict_authority(path);
        // Watch events are untrusted path inputs. Lexically reject non-graph-text
        // names first; the retained capability traversal below then performs the
        // component-wise no-follow containment and file-shape checks.
        // Concord ledger: a conflict copy just appeared for its winner. Pin the
        // winner's CURRENT base under the copy's identity BEFORE the winner's
        // own external admission overwrites it — the pinned text is the closest
        // thing to the true common ancestor the 3-way merge suggestions need
        // (first-wins in the ledger). Checked here because a conflict copy is
        // deliberately NOT eligible graph text (`entry_for_path` is None for
        // it; the scope check below is the watcher's own lexical authority).
        // Purely additive and best-effort; the event then flows through the
        // unchanged reconcile path (which never caches a copy as a page).
        if path_is_sync_conflict(path) && self.graph_text_watch_relevant(path) {
            if let Some(ledger) = self.concord_ledger.get() {
                if let Some(winner_rel) = self.conflict_winner_rel(path) {
                    ledger.pin_conflict_base(&self.rel_path(path), &winner_rel);
                }
            }
        }
        // GH #597: a spelling that reaches the file only because the
        // filesystem ignores case is not that file's page; the file's own
        // spelling is.
        if self.entry_for_path(path).is_none() || path_uses_graph_text_alias(&self.root, path) {
            return Ok(None);
        }
        // This checked watcher entrypoint is itself an exact external
        // observation boundary (tests and non-Tauri callers use it directly).
        // Publish the observed final state before changing cache evidence; an
        // unreadable or ambiguous path poisons the retained generation and the
        // next guarded write rebuilds instead of trusting stale ownership.
        let _ = self.update_guarded_graph_text_identity_paths(std::iter::once(path), true);
        let (content, identity) =
            match self.graph_text_read_optional_text_with_identity(&write, path) {
                Ok(Some(snapshot)) => snapshot,
                Ok(None) => {
                    self.record_watcher_identity_failure(path, true);
                    return Ok(None);
                }
                Err(error) => return self.watcher_failure(path, error),
            };
        let current = match self.graph_text_read_optional_text_with_identity(&write, path) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => {
                self.record_watcher_identity_failure(path, true);
                return Ok(None);
            }
            Err(error) => return self.watcher_failure(path, error),
        };
        if current.1 != identity || current.0 != content {
            self.record_watcher_identity_failure(path, false);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "graph text watcher snapshot changed before reconciliation",
            ));
        }
        self.remember_exact_graph_text_state(path, content_rev(&content), identity);
        self.repin_retained_identity_at_equal_bytes(path, &content, identity);
        // The watcher consumes the self-write marker (one-shot) so the map stays
        // bounded to in-flight writes.
        let reconciled = match self.sync_file_content(Some(&write), path, &content, true) {
            Ok(reconciled) => reconciled,
            Err(error) => return self.watcher_failure(path, error),
        };
        let entry = if let Some(entry) = reconciled.as_ref() {
            entry.clone()
        } else {
            let physical = self.entry_for_path(path).ok_or_else(bad_path)?;
            match parse_exact_page(self, &physical, &content) {
                Ok((effective, _, _)) => effective,
                Err(error) => return self.watcher_failure(path, error),
            }
        };
        self.clear_watcher_identity_failure_after_reconciliation(&entry);
        Ok(reconciled)
    }

    /// Record a page the watcher could not reconcile. Content Tine cannot
    /// accept (a parser rejection, text that is not UTF-8) is the file's
    /// reconciled state: the failure is recorded and the path is done until
    /// the file changes, so one such file neither fails every watcher cycle
    /// nor keeps its observation unacknowledged, which refused every
    /// name-only creation for the session. Any other error may pass, and the
    /// watcher retries it.
    fn watcher_failure(&self, path: &Path, error: io::Error) -> io::Result<Option<PageEntry>> {
        self.record_watcher_identity_failure(path, false);
        if is_page_content_rejection(&error) {
            Ok(None)
        } else {
            Err(error)
        }
    }

    /// Reconcile the cache for `path` given its already-read `content` — so a
    /// caller that has just read the file (e.g. load_page) doesn't read it twice.
    /// `consume_self_write`: whether a match on the self-write marker REMOVES it.
    /// The watcher passes true (bounding); load_page passes false — load_page can
    /// run in the rename→cache_upsert window and must not steal the marker out
    /// from under the watcher, which would turn the watcher's later poll into a
    /// false "changed on disk".
    pub(super) fn sync_file_content(
        &self,
        write: Option<&GraphTextWritePermit>,
        path: &Path,
        content: &str,
        consume_self_write: bool,
    ) -> io::Result<Option<PageEntry>> {
        let Some(entry) = self.entry_for_path(path) else {
            return Ok(None);
        };
        // A sync-tool conflict copy (`*.sync-conflict-*`) is never a real page: keep
        // it out of the `(kind,name)` cache (it would show as a garbage page and its
        // shared `id::` values would churn the id space). It's surfaced separately via
        // `list_sync_conflicts` and loaded on demand by path for the merge UI.
        if path_is_sync_conflict(path) {
            return Ok(None);
        }
        let (entry, newdoc, _) = parse_exact_page(self, &entry, content)?;
        // A shadow journal file (a title-named leftover coexisting with a canonical
        // date-stem file for the same day, #21) must never be reconciled into the
        // `(kind,name)` cache — that slot belongs to the canonical file, and caching
        // the shadow there would make name-resolution serve the wrong file. A
        // shadow's own external edits are picked up by a fresh path-addressed load
        // (`load_by_path`), so there's nothing to reconcile here.
        if entry.kind == PageKind::Journal {
            if let Some(date) = entry.date_key.map(crate::date::JournalDate::from_ordinal) {
                let shadow = match write {
                    Some(write) => self
                        .is_shadow_journal_under_permit(write, path, date)
                        .unwrap_or(true),
                    None => self.is_shadow_journal(path, date),
                };
                if shadow {
                    // GH #543 (sixth audit A6-N2): "not in the `(kind,name)`
                    // cache" had become "not in the INDEX either", the same
                    // conflation the pinned save path carried (A5-N2/A6-N1). An
                    // externally delivered edit to a shadow file — Syncthing,
                    // Dropbox, an external editor — reached search never, with
                    // the index idle, validated and ready, because nothing was
                    // ever queued for it. Cache slots are keyed by PATH, and name
                    // resolution does not read this cache's `by_name` map at
                    // all — `find_entry` builds its own index and prefers the
                    // date-stem file (`lookup.rs`) — so writing the shadow's own
                    // slot publishes its rows without taking the day from the
                    // canonical file.
                    //
                    // An UNCHANGED delivery publishes nothing (seventh audit
                    // A7-N2). Sync tools and watchers redeliver the same bytes
                    // routinely, and Tine's own save of this file echoes back
                    // through here; the ordinary path has the disk_rev and
                    // self-write comparisons below for exactly that, and this
                    // branch returns before reaching them. Each needless
                    // publication bumps the generation, which invalidates every
                    // memoized whole-graph result and can invalidate an
                    // inventory read that was in flight.
                    let disk_rev = content_rev(content);
                    // The parsed cache reflecting the bytes is not the index
                    // having them (audit R8-02).
                    let unchanged = self.page_revision_current(
                        self.cache.read().unwrap().is_none(),
                        path,
                        &disk_rev,
                    );
                    if !unchanged {
                        self.cache_upsert(entry, newdoc, disk_rev);
                    }
                    return Ok(None);
                }
            }
        }
        // Our own write: if the bytes on disk are exactly what Tine last wrote
        // here, this is not an external change — suppress it even if the parse
        // cache hasn't folded in the write yet (the rename→cache_upsert gap the
        // watcher can read into). The cache comparison below alone races that gap.
        let disk_rev = content_rev(content);
        // The self-write marker exists ONLY to stop the WATCHER raising a false
        // "changed on disk" during the rename→cache_upsert window, so only the
        // watcher (consume_self_write) consults it. load_page must NOT short-
        // circuit here: in that window the cache is still pre-write, so returning
        // early would serve a STALE cached doc with the fresh disk rev (a later
        // save could then clobber disk). load_page instead falls through to the
        // disk_revs fast path / parse-reconcile below and serves content matching
        // the exact bytes it just read.
        if consume_self_write {
            let mut recent = self.recent_writes.lock().unwrap();
            if recent.get(path).is_some_and(|r| *r == disk_rev) {
                recent.remove(path);
                return Ok(None);
            }
        }
        // Fast freshness check (B1): if the cache for this page already reflects
        // these exact disk bytes, there's nothing to reconcile — skip the parse +
        // serialize→parse normalization comparison below. Read disk_revs WHILE
        // HOLDING cache.read(): cache_upsert publishes the cache slot and its rev
        // together under cache.write(), so taking the cache read lock here makes
        // the reader mutually exclusive with that writer and guarantees a
        // consistent (cache, rev) pair — without it, the reader could observe a
        // slot already updated to a new doc while its rev hadn't been inserted yet
        // (separate lock), match the stale rev against disk, and serve the wrong
        // doc. Lock order cache → disk_revs matches every writer, so no deadlock;
        // the guard is dropped before the reconcile path below re-locks the cache.
        // A missing/mismatched entry falls through to the exact comparison, so this
        // can only ever save work, never serve stale content.
        //
        // The cache reflecting these bytes is not the index having them: a
        // whole-graph read installs revisions it never sent
        // (`page_revision_current`, audit R8-02). With no parsed cache, the
        // session record cache_upsert writes stands in for it, so a repeated
        // watcher delivery still enqueues nothing.
        {
            let cache_guard = self.cache.read().unwrap();
            if self.page_revision_current(cache_guard.is_none(), path, &disk_rev) {
                return Ok(None);
            }
        }
        {
            let guard = self.cache.read().unwrap();
            // A warm SQL session deliberately has no parsed cache. The
            // ordinary upsert below must still publish its external page delta;
            // only this optional comparison needs a cached document.
            if let Some(cache) = guard.as_ref() {
                if let Some(i) = self.cached_page_index_for_path(cache, path) {
                    let cached = &cache[i].1;
                    // Compare CONTENT, not the in-memory uuids: cached blocks carry
                    // generated uuids (assigned at cache build / upsert), while a
                    // fresh `parse` leaves them empty for non-ref-target blocks, so a
                    // direct `cached == newdoc` would never match and would flag every
                    // one of Tine's own writes as an external change. Normalize the
                    // cached doc through the same serialize→parse round-trip the file
                    // went through (both sides then have empty uuids) and compare.
                    let cached_norm = match Format::from_path(path) {
                        Format::Md => {
                            let opts = doc::SerializeOpts::detect(Some(content));
                            doc::parse(&doc::serialize_with(cached, &opts))
                        }
                        Format::Org => crate::org::parse_org(&crate::org::serialize_org_detect(
                            cached,
                            Some(content),
                        )),
                    };
                    if cached_norm == newdoc && self.index_has_revision(path, &disk_rev) {
                        return Ok(None); // unchanged / our own write
                    }
                }
            }
        }
        self.cache_upsert(entry.clone(), newdoc, disk_rev);
        // Concord ledger: the external change was admitted, so this content is
        // now what Tine last READ from disk — the new last-agreed text.
        self.concord_record_agreed(path, content);
        Ok(Some(entry))
    }

    /// Drop a file deleted on disk from the cache; returns the entry if it was
    /// cached (so the UI can react).
    pub fn forget_file(&self, path: &Path) -> Option<PageEntry> {
        self.retire_exact_graph_text_state(path);
        self.loaded_file_identities.write().unwrap().remove(path);
        self.revoke_conflict_authority(path);
        let entry = self.entry_for_path(path)?;
        let was_cached = {
            let guard = self.cache.read().unwrap();
            guard
                .as_ref()
                .is_some_and(|c| self.cached_page_index_for_path(c, &entry.path).is_some())
        };
        self.cache_remove_path(&entry);
        was_cached.then_some(entry)
    }

    /// Checked watcher path for an externally removed page file.
    pub fn sync_deleted_file(&self, path: &Path) -> io::Result<Option<PageEntry>> {
        let write = self.admit_graph_text_writer()?;
        let _identity = self.lock_graph_text_identity_mutation()?;
        if self.entry_for_path(path).is_none() {
            return Ok(None);
        }
        let lock = self.page_lock(path);
        let guard = lock.lock().unwrap();
        if self.graph_text_exists(&write, path)? {
            return Ok(None);
        }
        let forgotten = self.forget_file(path);
        drop(guard);
        Ok(forgotten)
    }
}
