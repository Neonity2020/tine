//! Graph's Direct projection lifetime: attaching and detaching it, the Concord
//! ledger, enqueueing replacements and deletes, marking it stale, and the
//! projection-backed page inventory, on-demand parses and property facets.

use super::*;

impl Graph {
    /// Attach Direct Files' app-private disposable SQLite projection.
    ///
    /// This never reads or writes graph files. If the parsed cache is already
    /// warm, its exact snapshot is queued; otherwise `install_built` supplies it
    /// when the ordinary background warm completes.
    pub fn attach_direct_projection(&self, path: PathBuf) -> io::Result<()> {
        let projection = Arc::new(crate::direct_projection::DirectProjection::start(path)?);
        let mut slot = self.direct_projection.lock().unwrap();
        if slot.is_some() {
            return Ok(());
        }
        *slot = Some(Arc::clone(&projection));
        let cache = self.cache.read().unwrap();
        if let Some(snapshot) = cache.as_ref().map(Arc::clone) {
            let revisions = Arc::new(self.disk_revs.read().unwrap().clone());
            projection.enqueue_full(
                self.cache_gen.load(std::sync::atomic::Ordering::Acquire),
                snapshot,
                revisions,
                Arc::new(self.config.parse_config()),
            );
        }
        Ok(())
    }

    /// Detach the Direct Files projection and wait for its writer to exit.
    ///
    /// A configuration refresh reopens the same root and attaches a projection
    /// at the SAME path. A second `DirectProjection::start` while this worker
    /// still holds the exclusive writer lease races it (observed as a 15 s
    /// "did not converge" under load), so the old worker is retired first.
    /// Returns whether it exited within `timeout`; `false` is reported by the
    /// caller, never treated as fatal — the replacement attach then decides.
    pub fn detach_direct_projection(&self, timeout: std::time::Duration) -> bool {
        let projection = self.direct_projection.lock().unwrap().take();
        match projection {
            Some(projection) => projection.close_and_wait_for_worker(timeout),
            None => true,
        }
    }

    /// Register the application's existing watcher wake channel and observe the
    /// last committed-image notification. This is notification state only; query reads
    /// never compare it with an edit or wait for it to advance.
    pub fn observe_direct_projection_commits(
        &self,
        wake: std::sync::mpsc::Sender<()>,
    ) -> Option<u64> {
        self.direct_projection
            .lock()
            .unwrap()
            .as_ref()
            .map(|projection| projection.observe_commits(wake))
    }

    /// Test barrier for ordinary producer progression. Production queries read
    /// the current coherent image and never wait for a saved-edit generation.
    #[cfg(test)]
    pub(crate) fn wait_for_direct_projection_for_test(
        &self,
        timeout: std::time::Duration,
    ) -> io::Result<()> {
        use crate::direct_projection::ProjectionProgress;
        let Some(projection) = self
            .direct_projection
            .lock()
            .unwrap()
            .as_ref()
            .map(Arc::clone)
        else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "snapshot query projection is not attached",
            ));
        };
        let started = std::time::Instant::now();
        loop {
            let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
            match projection.progress_at(generation) {
                ProjectionProgress::Ready => return Ok(()),
                ProjectionProgress::Working(_) => {}
                ProjectionProgress::Stale => {
                    return Err(io::Error::other(
                        "test projection stopped before the producer generation",
                    ))
                }
                ProjectionProgress::Stopped => {
                    return Err(io::Error::other(
                        "snapshot query projection worker is unavailable",
                    ))
                }
            }
            if started.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "snapshot query projection did not finish indexing in time",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Attach the Concord base ledger (ADR 0056) rooted at `dir` (an
    /// app-private directory OUTSIDE the graph tree). Idempotent; the first
    /// attach wins. Queues a background prune of unreferenced blobs.
    pub fn attach_concord_ledger(&self, dir: PathBuf) {
        let ledger = Arc::new(crate::concord_ledger::ConcordLedger::new(dir));
        if self.concord_ledger.set(Arc::clone(&ledger)).is_ok() {
            ledger.queue_prune();
        }
    }

    /// The attached ledger, if any (None ⇒ every Concord hook no-ops).
    pub fn concord_ledger(&self) -> Option<&Arc<crate::concord_ledger::ConcordLedger>> {
        self.concord_ledger.get()
    }

    /// Quitting: wait until `deadline` for this graph's queued ledger updates.
    /// True when there was nothing to wait for or the queue drained in time.
    pub fn drain_concord_ledger_for_exit(&self, deadline: std::time::Instant) -> bool {
        self.concord_ledger
            .get()
            .is_none_or(|ledger| ledger.drain_for_exit(deadline))
    }

    /// Best-effort ledger update: `content` is now the exact text Tine and the
    /// disk agree on for `path`. Called after a successful save commit and
    /// after an external-change admission. Foreground cost is one channel send;
    /// non-page paths (config, assets) are filtered out here.
    pub(super) fn concord_record_agreed(&self, path: &Path, content: &str) {
        let Some(ledger) = self.concord_ledger.get() else {
            return;
        };
        if self.entry_for_path(path).is_none() || path_is_sync_conflict(path) {
            return;
        }
        ledger.record(&self.rel_path(path), content);
    }

    /// The winner page a conflict copy shadows (same dir, same extension, base
    /// stem), as a graph-relative path — the identity the ledger pins under.
    pub(super) fn conflict_winner_rel(&self, conflict_path: &Path) -> Option<String> {
        let ext = text_extension_from_path(conflict_path)?;
        let stem = conflict_path.file_stem()?.to_str()?;
        let base_stem = sync_conflict_base(stem)?;
        let winner = conflict_path.parent()?.join(format!("{base_stem}.{ext}"));
        Some(self.rel_path(&winner))
    }

    /// `force` skips the readiness shortcut below. A repair that has latched
    /// `pending.rebuild` MUST force: the worker consumes that flag only
    /// together with a full or warm payload, so a skipped snapshot would leave
    /// the rebuild latched with nothing to ride in on and every later capture
    /// refused for the lifetime of the graph.
    pub(super) fn direct_projection_enqueue_full(
        &self,
        generation: u64,
        pages: Arc<Vec<(PageEntry, Arc<Document>)>>,
        revisions: Arc<std::collections::HashMap<PathBuf, String>>,
        force: bool,
    ) {
        if let Some(projection) = self
            .direct_projection
            .lock()
            .unwrap()
            .as_ref()
            .map(Arc::clone)
        {
            // R6: a projection already READY at this generation was validated
            // from the same bytes this snapshot was parsed from; a redundant
            // snapshot would only open a NotReady window while it re-validates.
            if !force && projection.ready_at(generation) {
                return;
            }
            projection.enqueue_full(
                generation,
                pages,
                revisions,
                Arc::new(self.config.parse_config()),
            );
        }
    }

    pub(super) fn direct_projection_enqueue_replace(
        &self,
        generation: u64,
        entry: PageEntry,
        document: Arc<Document>,
        revision: String,
    ) {
        if let Some(projection) = self
            .direct_projection
            .lock()
            .unwrap()
            .as_ref()
            .map(Arc::clone)
        {
            projection.enqueue_replace(
                generation,
                entry,
                document,
                revision,
                Arc::new(self.config.parse_config()),
            );
        }
    }

    pub(super) fn direct_projection_enqueue_delete(&self, generation: u64, entry: PageEntry) {
        self.session_page_ids.write().unwrap().remove(&entry.path);
        if let Some(projection) = self
            .direct_projection
            .lock()
            .unwrap()
            .as_ref()
            .map(Arc::clone)
        {
            // A delete lowers nothing, so it carries no parse config (F11).
            projection.enqueue_delete(generation, entry);
        }
    }

    pub(super) fn direct_projection_mark_stale(&self) {
        if let Some(projection) = self
            .direct_projection
            .lock()
            .unwrap()
            .as_ref()
            .map(Arc::clone)
        {
            projection.mark_stale();
        }
    }

    /// R6: the page inventory from the ready projection, rebuilt into the
    /// walk's `PageEntry` shape. `pages.name` is the effective (title::-aware)
    /// name because the producer lowers the effective entry; kind comes from
    /// the row and a journal's sort key from its name.
    pub(super) fn direct_projection_page_inventory(
        &self,
        generation: u64,
    ) -> Option<Vec<PageEntry>> {
        let projection = self
            .direct_projection
            .lock()
            .unwrap()
            .as_ref()
            .map(Arc::clone)?;
        if !projection.wait_ready_at(generation) {
            return None;
        }
        let rows = projection.page_inventory(generation)?;
        let mut entries = Vec::with_capacity(rows.len());
        for (name, rel_path, kind) in rows {
            let kind = crate::direct_projection::page_kind_from_sql(kind)?;
            let date_key = match kind {
                PageKind::Journal => Some(self.journal_format.parse(&name)?.ordinal_key()),
                PageKind::Page => None,
            };
            entries.push(PageEntry {
                name,
                kind,
                date_key,
                path: self.root.join(&rel_path),
                rel_path,
            });
        }
        entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
        (self.cache_gen.load(std::sync::atomic::Ordering::Acquire) == generation).then_some(entries)
    }

    /// R6: parse exactly the named pages for reference/fuzzy hydration when no
    /// parsed cache exists. The documents are returned to the caller and
    /// dropped after use — nothing is installed or retained.
    pub(super) fn parse_pages_on_demand(
        &self,
        generation: u64,
        paths: Vec<PathBuf>,
    ) -> Option<Vec<(PageEntry, Arc<Document>)>> {
        let permit = self.admit_retained_graph_text_writer().ok()?;
        let mut pages = Vec::with_capacity(paths.len());
        for relative in paths {
            let absolute = self.root.join(&relative);
            let entry = self.graph_inventory_entry(&absolute).ok()??;
            if entry.rel_path != relative.to_string_lossy() {
                return None;
            }
            let (content, _) = self
                .graph_text_read_optional_text_with_identity(&permit, &entry.path)
                .ok()??;
            #[cfg(test)]
            self.page_build_test
                .on_demand_parses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (effective, document, _) =
                isolate_page_parse(entry, &self.journal_format, |entry| {
                    Some(self.parse_session_page_content(entry, &content))
                })
                .ok()??;
            pages.push((effective, Arc::new(document)));
        }
        #[cfg(test)]
        DIRECT_HYDRATED_PAGES.with(|recorded| {
            recorded.borrow_mut().extend(
                pages
                    .iter()
                    .map(|(entry, _)| PathBuf::from(&entry.rel_path)),
            );
        });
        (self.cache_gen.load(std::sync::atomic::Ordering::Acquire) == generation).then_some(pages)
    }

    pub(super) fn direct_projection_property_facets(
        &self,
        autocomplete: bool,
        max_items: usize,
        max_bytes: usize,
    ) -> Option<(Vec<(String, Vec<String>)>, bool)> {
        let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        let projection = self
            .direct_projection
            .lock()
            .unwrap()
            .as_ref()
            .map(Arc::clone)?;
        let result = projection.property_facets(
            generation,
            autocomplete,
            &self.config.block_hidden_properties,
            max_items,
            max_bytes,
        )?;
        (self.cache_gen.load(std::sync::atomic::Ordering::Acquire) == generation).then_some(result)
    }

    /// The §6.2 registry row source when the Direct Files projection is READY
    /// (CLOSURE §4): the shared raw property stream and its same-snapshot page
    /// map, or `None` when the projection is not ready or the read refused.
    ///
    /// The generation is re-checked after the read for the same reason every
    /// other projection reader re-checks it: a snapshot that straddles a
    /// rebuild is not a snapshot.
    #[cfg(test)]
    pub(super) fn direct_projection_property_owner_rows(
        &self,
    ) -> Option<(
        Vec<crate::query::registry::OwnerRow>,
        std::collections::HashMap<String, crate::query::registry::PageMeta>,
    )> {
        let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        let projection = self
            .direct_projection
            .lock()
            .unwrap()
            .as_ref()
            .map(Arc::clone)?;
        if !projection.wait_ready_at(generation) {
            return None;
        }
        let result = projection.property_owner_rows(generation)?;
        (self.cache_gen.load(std::sync::atomic::Ordering::Acquire) == generation).then_some(result)
    }
}
