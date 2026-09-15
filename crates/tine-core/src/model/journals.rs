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
        self.with_pages(|pages| {
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
        let write = self.admit_graph_text_writer()?;
        let entries = self.configured_text_entries(&write, false)?;
        let mut n = 0;
        for entry in entries
            .into_iter()
            .filter(|entry| entry.kind == PageKind::Journal)
        {
            let p = entry.path;
            if let Some(target) = self.journal_filename_migration_target(&p) {
                if self.graph_text_exists(&write, &target)? {
                    continue;
                }
                if self.graph_text_move_noreplace(&write, &p, &target).is_ok() {
                    n += 1;
                }
            }
        }
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
        self.graph_text_move_to_trash(&write, &src, &dest, &trash)?;
        Ok(())
    }
}
