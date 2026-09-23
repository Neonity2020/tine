//! Graph's journal surface: the journal feed, journal content days, the
//! journal filename-format migration, and reading or trashing a journal file.

use super::*;

impl Graph {
    /// Journals sorted newest-first.
    pub fn journals_desc(&self) -> Vec<PageEntry> {
        // Prefer the warmed whole-graph cache — its PageEntry list is kept current
        // by cache_upsert/cache_remove. Before warm completes, enumerate only
        // metadata and filenames, then parse the handful of feed rows selected by
        // journal_feed_page. Calling list_pages here used to read and parse every
        // non-journal page on the foreground first-content path, immediately
        // before background warm repeated that graph-sized work.
        let raw: Vec<PageEntry> = match self.cache.read().unwrap().as_ref() {
            Some(pages) => pages
                .iter()
                .filter(|(e, _)| e.kind == PageKind::Journal && e.date_key.is_some())
                .map(|(e, _)| e.clone())
                .collect(),
            None => self
                .admit_retained_graph_text_writer()
                .and_then(|permit| self.graph_text_entries(&permit))
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| entry.kind == PageKind::Journal && entry.date_key.is_some())
                .collect(),
        };
        // A day with more than one file (e.g. a leftover title-named duplicate of
        // a `yyyy_MM_dd` file) must appear ONCE — both files resolve to the same
        // page name, so otherwise the day renders twice. The stray stays visible
        // via journal_conflicts() for reconciliation.
        //
        // This dedup/ordering rule is journal_feed's, not this file's: it used to
        // be a second hand-written copy here, whose canonicality test read only
        // `path` where journal_feed's reads `rel_path` first. Two copies of the
        // rule that decides which file represents a day is how a day silently
        // drops out of a user's history.
        crate::journal_feed::journal_feed_candidates_desc(raw)
    }

    /// Feed membership is narrower than the raw journal inventory: future
    /// journals remain directly reachable graph pages, but are not in Journals.
    pub fn feed_journals_desc_through(&self, cutoff: JournalDate) -> Vec<PageEntry> {
        let cutoff = cutoff.ordinal_key();
        self.journals_desc()
            .into_iter()
            .filter(|entry| entry.date_key.is_some_and(|day| day <= cutoff))
            .collect()
    }

    pub fn feed_journals_desc(&self) -> Vec<PageEntry> {
        self.feed_journals_desc_through(JournalDate::today())
    }

    /// Journal `date_key`s (yyyymmdd) whose page has real content — i.e. at
    /// least one block with a non-empty, non-property line. Drives the calendar
    /// picker's empty/non-empty day marking. Served from the cache.
    pub fn journal_content_days(&self) -> Vec<i64> {
        let fallback = match self.indexed_or_fallback(|| self.indexed_journal_content_days()) {
            Ok(days) => return days,
            Err(fallback) => fallback,
        };
        fallback.with_pages(self, |pages| {
            pages
                .iter()
                .filter(|(e, _)| e.kind == PageKind::Journal)
                .filter_map(|(e, d)| e.date_key.filter(|_| doc_has_content(&d.roots)))
                .collect()
        })
    }

    /// One-time recovery: a journal that was saved under its display title
    /// ("Jun 18th, 2026.md") instead of its date stem ("2026_06_18.md") can't be
    /// parsed back to a date, so it drops out of the feed and the day looks
    /// empty. Rename such files to their stem — but only when the stem file
    /// doesn't already exist (never clobber/merge). Returns how many were fixed.
    pub fn has_journal_filename_migrations(&self) -> bool {
        !self.journal_filename_migrations().is_empty()
    }

    /// The pending renames, for the user to review and authorize. Same
    /// selection `migrate_journal_filenames_checked` acts on: a file whose stem
    /// parses as a journal date but is not the graph's filename format, and
    /// whose target name is free (the migration never clobbers).
    pub fn journal_filename_migrations(&self) -> Vec<JournalFilenameMigration> {
        let dir = self.journals_path();
        let Ok(rd) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out: Vec<JournalFilenameMigration> = rd
            .flatten()
            .filter_map(|entry| {
                let from = entry.path();
                let target = self.journal_filename_migration_target(&from)?;
                (!target.exists()).then(|| JournalFilenameMigration {
                    from: self.rel_path(&from),
                    to: self.rel_path(&target),
                })
            })
            .collect();
        out.sort_by(|a, b| a.from.cmp(&b.from));
        out
    }

    fn journal_filename_migration_target(&self, p: &std::path::Path) -> Option<PathBuf> {
        // Supported text formats — an org graph's title-named journals are `.org`;
        // OG markdown files may also carry the long `.markdown` spelling.
        let ext = text_extension_from_path(p)?;
        let stem = p.file_stem().and_then(|s| s.to_str())?;
        if JournalDate::from_file_stem(stem).is_some() {
            return None; // already a plausible date stem (yyyy_MM_dd / yyyy-MM-dd) — leave it
        }
        // A title-named ("Jun 18th, 2026.md", "Thursday, 25-06-2026.org") or
        // otherwise non-stem journal file: normalize it to the graph's filename
        // format so it round-trips with OG and is recognized in the feed.
        let d = self.journal_format.parse(stem)?;
        let want = self.journal_format.file_stem(d);
        if want == stem {
            return None; // already in the graph's filename format
        }
        let target = self.journals_path().join(format!("{want}.{ext}"));
        Some(target)
    }

    pub fn migrate_journal_filenames(&self) -> usize {
        self.migrate_journal_filenames_checked().unwrap_or(0)
    }

    pub fn migrate_journal_filenames_checked(&self) -> io::Result<usize> {
        // GH #543 (sixth audit A6-N4): every other page-set mutation holds the
        // identity gate across its whole transaction; this one took it only
        // inside each individual move primitive and released it again. So the
        // window between the last move and the publication below was open: a
        // save could land in it, publish its rows, and then be replaced by the
        // bytes this function had already read — stale text carrying the newer
        // generation. Hold the gate for the whole migration, as `merge_pages`,
        // `rename_file_to_page` and the save path do. The gate is reentrant per
        // thread, so the per-move acquisitions underneath still work.
        let _identity = self.lock_graph_text_identity_mutation()?;
        let write = self.admit_graph_text_writer()?;
        let mut moved: Vec<(PathBuf, Option<PageEntry>, PathBuf)> = Vec::new();
        // GH #543 (seventh audit A7-N3): the enumeration and the moves are
        // fallible, and every one of their error exits used to `?` straight
        // past the publication below — leaving files this function had ALREADY
        // moved described in the index at paths that no longer exist, with
        // nothing queued. A committed filesystem move does not un-happen
        // because a later file failed, so the outcome is captured here and the
        // moves made before it are published either way.
        let outcome = (|| -> io::Result<()> {
            let entries = self.configured_text_entries(&write, false)?;
            for entry in entries
                .into_iter()
                .filter(|entry| entry.kind == PageKind::Journal)
            {
                let p = entry.path;
                if let Some(target) = self.journal_filename_migration_target(&p) {
                    if self.graph_text_exists(&write, &target)? {
                        continue;
                    }
                    // Retained for the projection before the move takes the path away.
                    let retired = self.entry_for_path(&p);
                    let attempt = self.graph_text_move_noreplace(&write, &p, &target);
                    // Ask the filesystem what happened, not the Result: the
                    // move renames FIRST and then does fallible durability and
                    // identity work, so an `Err` here can mean "the file moved
                    // and a later step failed" — which used to be reported as
                    // nothing having migrated (A7-N3).
                    let landed = attempt.is_ok()
                        || (self.graph_text_exists(&write, &target).unwrap_or(false)
                            && !self.graph_text_exists(&write, &p).unwrap_or(true));
                    if landed {
                        moved.push((p.clone(), retired, target));
                    }
                    attempt?;
                }
            }
            Ok(())
        })();
        let n = moved.len();
        if n > 0 {
            // GH #543 (fifth audit A5-N1): this migration MOVES files and told
            // nothing. The logical page and its text are unchanged, so search
            // kept answering — from rows keyed to paths that no longer exist,
            // with nothing queued to correct them. The next ordinary save of a
            // migrated page then published its NEW path beside the retired
            // one's surviving row, so one file answered twice.
            let moved = moved
                .into_iter()
                .map(|(source, retired, target)| {
                    let replacement = self
                        .graph_text_read_to_string(&write, &target)
                        .ok()
                        .and_then(|content| {
                            let provisional = self.graph_inventory_entry(&target).ok().flatten()?;
                            parse_exact_page(self, &provisional, &content).ok()
                        });
                    // The old path owns nothing now, and the moved journal is
                    // as readable as it was: recorded by path before the
                    // discard moves the generation (audit R15-02).
                    self.note_graph_text_state(&source, true);
                    self.note_graph_text_state(&target, replacement.is_some());
                    (source, retired, target, replacement)
                })
                .collect::<Vec<_>>();
            let coming = self.index_delta_coming();
            let touched = moved
                .iter()
                .flat_map(|(source, _, target, _)| [source.clone(), target.clone()])
                .collect();
            self.discard_parsed_cache(touched, graph_drift::IndexEffect::Sent(&coming));
            let mut page_set = Vec::new();
            for (_, retired, _, replacement) in moved {
                if let Some(entry) = retired {
                    page_set.push(crate::direct_projection::PageSetChange::Delete { entry });
                }
                // Stale beats absent: without the replacement the retired rows
                // are the only evidence this journal exists, so leave them and
                // let a later warm reconcile (`rename_file_to_page` reasons the
                // same way).
                match replacement {
                    Some((entry, document, revision)) => {
                        page_set.push(crate::direct_projection::PageSetChange::Replace {
                            entry,
                            document: Arc::new(document),
                            revision,
                        })
                    }
                    None => {
                        page_set.pop();
                    }
                }
            }
            self.direct_projection_publish_page_set(
                self.cache_gen.load(std::sync::atomic::Ordering::Acquire),
                page_set,
            );
        }
        // Reported only after the moves that DID happen are published, so a
        // caller seeing the error still sees an index that matches the disk.
        outcome?;
        Ok(n)
    }

    /// Raw contents of ONE journal file (by exact filename) — lets the UI show a
    /// duplicate day's individual files (which can't be navigated to separately,
    /// as pages are keyed by date) so the user can inspect before reconciling.
    pub fn read_journal_file(&self, name: &str) -> io::Result<String> {
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bad journal file name",
            ));
        }
        fs::read_to_string(self.journals_path().join(name))
    }

    /// Move ONE journal file (by its exact filename) to the recoverable trash —
    /// the affordance for reconciling a duplicate day. Refuses a path separator so
    /// it can't reach outside `journals/`.
    pub fn trash_journal_file(&self, name: &str) -> io::Result<()> {
        let write = self.admit_graph_text_writer()?;
        let _identity = self.lock_graph_text_identity_mutation()?;
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bad journal file name",
            ));
        }
        let src = self.journals_path().join(name);
        if !self.graph_text_exists(&write, &src)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no such journal file",
            ));
        }
        let trash = typed_trash_dir(&self.root, TrashEntryKind::Journal);
        let dest = trash.join(format!("{}__{name}", trash_stamp()));
        // Retained for the projection before the move takes the path away.
        let retired = self.entry_for_path(&src);
        self.graph_text_move_to_trash(&write, &src, &dest, &trash)?;
        // GH #543 (fifth audit A5-N1): this published nothing at all — no cache
        // removal, no generation advance, no delta — so search went on offering
        // the trashed file's text from an index that called itself complete.
        // `cache_remove_path` is the blessed one-page retirement: it drops the
        // page from the parsed cache and every index derived from it, advances
        // the generation, and queues the projection's delete.
        if let Some(entry) = retired {
            self.cache_remove_path(&entry);
        }
        Ok(())
    }
}
