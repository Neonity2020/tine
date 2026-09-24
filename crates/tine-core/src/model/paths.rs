//! Graph's path resolution: relative, configured and lexical paths, projection
//! targets and parents, and page source files; plus the small accessors that
//! sit among them (page_lock, config-write notes, meta, cache_generation).

use super::*;

impl Graph {
    /// Construct a read-only graph projection from one caller-owned document
    /// snapshot. The empty `root` is only a fail-closed fallback: whole-graph
    /// consumers use the preinstalled cache and page list, so they can never
    /// mix these documents with a later revision from the live graph.
    ///
    pub(crate) fn from_page_snapshot(
        root: impl AsRef<Path>,
        mut pages: Vec<(PageEntry, Arc<Document>)>,
    ) -> Graph {
        for (entry, document) in &mut pages {
            assign_doc_runtime_ids(&mut Arc::make_mut(document).roots, &entry.rel_path);
        }
        let graph = Graph::open(root);
        let entries = pages.iter().map(|(entry, _)| entry.clone()).collect();
        let index = build_page_cache_index(&pages);
        *graph.cache.write().unwrap() = Some(Arc::new(pages));
        *graph.cache_index.write().unwrap() = Some(index);
        *graph.page_list_cache.write().unwrap() = Some((0, entries));
        graph
    }

    /// The write lock for a resolved page path (see `page_locks`). Returns an
    /// `Arc` the caller holds (`let _g = lock.lock().unwrap();`) for the critical
    /// section. The `page_locks` map mutex is released before the per-page lock is
    /// taken, so callers never serialize on the map. Opportunistically prunes
    /// entries no caller still holds (strong_count == 1) to bound growth.
    pub(super) fn page_lock(&self, path: &Path) -> std::sync::Arc<std::sync::Mutex<()>> {
        let mut map = self.page_locks.lock().unwrap();
        if map.len() >= 64 {
            map.retain(|_, v| std::sync::Arc::strong_count(v) > 1);
        }
        map.entry(path.to_path_buf())
            .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
            .clone()
    }

    /// The `logseq/config.edn` bytes this instance was opened with, digested.
    /// `None` when there was no readable file.
    ///
    /// Compare against [`config_file_description`] to learn whether an external
    /// write actually changed the configuration this instance is serving. The
    /// watcher does exactly that before paying for a whole-graph reopen, which
    /// drops every cache the graph has built.
    pub fn open_config_description(&self) -> Option<BlobDescription> {
        self.reconciliation_scan_open_config_description
    }

    /// Digest of the `config.edn` bytes the configuration this instance
    /// serves was taken from (`None`: no readable file). Equal to
    /// [`config_file_description`] exactly when disk holds nothing this
    /// instance has not taken in, which is the watcher's reason to do nothing.
    pub fn served_config_description(&self) -> Option<BlobDescription> {
        *self.served_config_description.read().unwrap()
    }

    /// The configuration as last taken in.
    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.config.read().unwrap())
    }

    /// Change this instance's configuration before anyone else holds it
    /// (exports and the CLI override single settings for one run).
    pub fn config_mut(&mut self) -> &mut Config {
        Arc::make_mut(self.config.get_mut().unwrap())
    }

    /// Re-read `config.edn` and take in a change that reaches only settings.
    /// [`ConfigReach::Graph`] leaves this instance as it is: the caller opens
    /// a new `Graph`, which is the only way that change is taken in.
    pub fn take_in_config(&self) -> crate::config::ConfigReach {
        let bytes = fs::read(reconciliation_scan_config_path_at_open(&self.root)).ok();
        let description = bytes.as_deref().map(BlobDescription::of);
        let new = bytes
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .map(Config::parse)
            .unwrap_or_default();
        let mut current = self.config.write().unwrap();
        let reach = current.reach(&new);
        if reach == crate::config::ConfigReach::Settings {
            *current = Arc::new(new);
        }
        if reach != crate::config::ConfigReach::Graph {
            *self.served_config_description.write().unwrap() = description;
        }
        reach
    }

    pub fn meta(&self) -> GraphMeta {
        let config = self.config();
        GraphMeta {
            root: self.root.display().to_string(),
            journals_dir: config.journals_dir.clone(),
            pages_dir: config.pages_dir.clone(),
            preferred_workflow: match config.preferred_workflow {
                crate::config::Workflow::Todo => "todo".into(),
                crate::config::Workflow::Now => "now".into(),
            },
            shortcuts: config.shortcuts.clone(),
            start_of_week: config.start_of_week,
            linked_references_collapsed_threshold: config.linked_references_collapsed_threshold,
            block_hidden_properties: config.block_hidden_properties.clone(),
            default_journal_template: config.default_journal_template.clone(),
            default_home: config.default_home.clone(),
            favorites: config.favorites.clone(),
            favorites_page: config.favorites_page.clone(),
            journal_page_title_format: self.journal_format.title_format().to_string(),
            journal_file_name_format: self.journal_format.file_format().to_string(),
            preferred_format: config.preferred_format.ext().to_string(),
            macros: config.macros.clone(),
            enable_timetracking: config.enable_timetracking,
            show_brackets: config.show_brackets,
            doc_mode_enter_for_new_block: config.doc_mode_enter_for_new_block,
            logical_outdenting: config.logical_outdenting,
            logbook_with_second_support: config.logbook.with_second_support,
            logbook_enabled_in_timestamped_blocks: config.logbook.enabled_in_timestamped_blocks,
            logbook_enabled_in_all_blocks: config.logbook.enabled_in_all_blocks,
            guide_announced: config.guide_announced,
        }
    }

    /// Current cache generation — bumped on every cache-mutating page change,
    /// and the key that memoized backlink/reference results invalidate against.
    /// Exposed for observability and tests (e.g. asserting a no-op save doesn't
    /// needlessly invalidate everything).
    pub fn cache_generation(&self) -> u64 {
        self.cache_gen.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Graph text Tine cannot read or parse right now, and listing skips
    /// (`path: why`), as last observed per path (`UnreadablePages`). Paths
    /// are graph-relative and safe to surface.
    pub fn page_index_failures(&self) -> Vec<String> {
        self.page_index_failures.read().unwrap().to_vec()
    }

    /// The page failures not yet announced, marking them announced. A failure
    /// that clears and later recurs is announced again. Tine indexes the rest
    /// of the graph around a page it cannot read, so nothing else tells the
    /// user that page is missing from search, queries and references.
    pub fn take_unannounced_page_failures(&self) -> Vec<String> {
        let current = self.page_index_failures();
        let mut announced = self
            .announced_page_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        announced.retain(|failure| current.contains(failure));
        let fresh: Vec<String> = current
            .into_iter()
            .filter(|failure| !announced.contains(failure))
            .collect();
        announced.extend(fresh.iter().cloned());
        fresh
    }

    pub fn journals_path(&self) -> PathBuf {
        self.root.join(&self.config().journals_dir)
    }

    pub fn pages_path(&self) -> PathBuf {
        self.root.join(&self.config().pages_dir)
    }

    /// Graph-root-relative, forward-slashed path for an absolute file path inside
    /// the graph (`…/journals/2026_06_26.org` → `journals/2026_06_26.org`). The
    /// stable, machine-portable id Tine hands the frontend so a page can be pinned
    /// to a SPECIFIC file (#21). Falls back to the input lossily if it's somehow
    /// outside the root (shouldn't happen for graph files).
    pub fn rel_path(&self, abs: &Path) -> String {
        slash_path(abs.strip_prefix(&self.root).unwrap_or(abs))
    }

    /// Resolve a graph-root-relative path (as produced by [`rel_path`]) back to an
    /// absolute file path, validating it belongs to the versioned graph-wide text
    /// scope. Retained no-follow traversal and identity validation are performed
    /// before reads or writes; this lexical gate grants no creation or projection
    /// authority.
    pub fn resolve_rel(&self, rel: &str) -> Option<PathBuf> {
        let abs = self.resolve_rel_lexical(rel)?;
        if !path_stays_within_root(&self.root, &abs) || path_uses_graph_text_alias(&self.root, &abs)
        {
            return None;
        }
        Some(abs)
    }

    /// GH #597: a path saved under another case spelling of its file (a tab,
    /// Recent entry or sidebar item recorded while Tine handed out
    /// `pages/Contents.md` for `contents.md`) resolves to the file's spelling
    /// on disk. Only a spelling that differs from the disk's by case is
    /// followed, and the result passes [`Self::resolve_rel`] itself.
    pub(super) fn resolve_rel_disk_spelling(&self, rel: &str) -> Option<PathBuf> {
        let rel = rel.trim().replace('\\', "/");
        let canonical_root = fs::canonicalize(&self.root).ok()?;
        let actual = fs::canonicalize(self.resolve_rel_lexical(&rel)?).ok()?;
        let disk_rel = actual
            .strip_prefix(&canonical_root)
            .ok()?
            .to_str()?
            .replace('\\', "/");
        if disk_rel == rel || disk_rel.to_lowercase() != rel.to_lowercase() {
            return None;
        }
        self.resolve_rel(&disk_rel)
    }

    pub(super) fn resolve_graph_text_rel(
        &self,
        permit: &GraphTextWritePermit,
        rel: &str,
    ) -> io::Result<Option<PathBuf>> {
        self.graph_text_permit_root(permit)?;
        Ok(self.resolve_configured_rel_lexical(rel))
    }

    pub(super) fn resolve_graph_rel_with_permit(
        &self,
        permit: &GraphTextWritePermit,
        rel: &str,
    ) -> io::Result<Option<PathBuf>> {
        self.graph_text_permit_root(permit)?;
        Ok(self.resolve_rel_lexical(rel))
    }

    pub(super) fn resolve_rel_lexical(&self, rel: &str) -> Option<PathBuf> {
        let rel = rel.trim();
        if !self.graph_text_scope.is_eligible(rel) {
            return None;
        }
        Some(self.root.join(rel))
    }

    pub(super) fn resolve_configured_rel_lexical(&self, rel: &str) -> Option<PathBuf> {
        let rel = rel.trim();
        if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') {
            return None;
        }
        let parts = rel.split('/').collect::<Vec<_>>();
        let config = self.config();
        let configured_root = [&config.journals_dir, &config.pages_dir]
            .into_iter()
            .filter_map(|configured| {
                let components = configured.split('/').collect::<Vec<_>>();
                (!components.is_empty()
                    && components
                        .iter()
                        .all(|component| projection_component_is_portable(component))
                    && parts.len() > components.len()
                    && parts.starts_with(&components))
                .then_some((configured, components.len()))
            })
            .max_by_key(|(_, len)| *len)?;
        let base = self.root.join(configured_root.0);
        // The remaining segments are the file's path UNDER that dir. Nested
        // sub-directories are allowed (#21) but the can't-escape-the-graph
        // invariant is kept lexically: every segment must be a plain name — no
        // empty segment (`a//b`, a trailing `/`), no `.`/`..` traversal. With no
        // `..` and no absolute/backslash (rejected above), `base.join(tail)`
        // provably stays within `base`; there must be at least one segment (a bare
        // `pages` is a dir, not a file).
        let mut tail = PathBuf::new();
        for &seg in &parts[configured_root.1..] {
            if seg.is_empty() || seg == "." || seg == ".." {
                return None;
            }
            tail.push(seg);
        }
        if tail.as_os_str().is_empty() {
            return None;
        }
        let abs = base.join(tail);
        text_extension_from_path(&abs).map(|_| ())?;
        Some(abs)
    }

    pub(super) fn projection_page_target(
        &self,
        relative_path: &str,
    ) -> io::Result<ProjectionTarget> {
        if relative_path != relative_path.trim()
            || relative_path.is_empty()
            || relative_path.starts_with('/')
            || relative_path.contains('\\')
            || relative_path.contains('\0')
        {
            return Err(bad_path());
        }
        let components = relative_path.split('/').collect::<Vec<_>>();
        let configured_root_len = [&self.config().journals_dir, &self.config().pages_dir]
            .into_iter()
            .filter_map(|configured_root| {
                let root_components = configured_root.split('/').collect::<Vec<_>>();
                (components.len() > root_components.len()
                    && root_components
                        .iter()
                        .all(|component| projection_component_is_portable(component))
                    && components.starts_with(&root_components))
                .then_some(root_components.len())
            })
            .max();
        if components
            .iter()
            .any(|component| !projection_component_is_portable(component))
        {
            return Err(bad_path());
        }
        // A configured root keeps its existing acceptance verbatim. Ordinary
        // graph text that no configured root owns is addressable too, because
        // OG reads and rewrites a page wherever it already lives: its recursive
        // `logseq.common.graph/get-files` walk has no root restriction, and
        // `frontend.modules.file.core/save-tree-aux!` writes back to the exact
        // recorded `:file/path` (only a page with no file at all gets a fresh
        // path under a configured directory). The graph-text scope supplies the
        // containers OG itself skips, so nothing here may address `assets/`,
        // `logseq/bak/`, hidden directories or other excluded state.
        let within_configured_root = components.len() >= 2 && configured_root_len.is_some();
        if !within_configured_root && !self.graph_text_scope.is_eligible(relative_path) {
            return Err(bad_path());
        }
        let filename = components
            .last()
            .expect("nonempty split has a last element");
        let _ = split_logseq_text_filename(filename).ok_or_else(bad_path)?;
        let parent_components = components[..components.len() - 1]
            .iter()
            .map(|component| (*component).to_owned())
            .collect::<Vec<_>>();
        let target = ProjectionTarget {
            absolute_path: self.root.join(relative_path),
            parent_components,
            filename: (*filename).to_owned(),
        };
        Ok(target)
    }

    pub(super) fn graph_text_exact_path(
        &self,
        relative: &str,
        require_eligible: bool,
    ) -> io::Result<GraphTextExactPath> {
        if relative != relative.trim()
            || relative.is_empty()
            || relative.starts_with('/')
            || relative.contains('\\')
            || relative.contains('\0')
        {
            return Err(bad_path());
        }
        let components = relative.split('/').collect::<Vec<_>>();
        if components
            .iter()
            .any(|component| !projection_component_is_portable(component))
        {
            return Err(bad_path());
        }
        let graph_text_path = GraphTextPath::parse(relative.to_owned()).ok();
        if require_eligible
            && (!self.graph_text_scope.is_eligible(relative) || graph_text_path.is_none())
        {
            return Err(bad_path());
        }
        let filename = components.last().copied().ok_or_else(bad_path)?;
        let mut parent_components = Vec::with_capacity(components.len().saturating_sub(1));
        let mut parent_relative = String::new();
        for component in &components[..components.len() - 1] {
            if !parent_relative.is_empty() {
                parent_relative.push('/');
            }
            parent_relative.push_str(component);
            if !self.graph_text_scope.should_descend(&parent_relative) {
                return Err(bad_path());
            }
            parent_components.push((*component).to_owned());
        }
        Ok(GraphTextExactPath {
            graph_text_path,
            parent_components,
            filename: filename.to_owned(),
        })
    }

    pub(super) fn projection_parent(
        &self,
        target: &ProjectionTarget,
    ) -> io::Result<ProjectionParent> {
        let root = self.projection_root.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "graph has no retained no-follow projection capability",
            )
        })?;
        let mut chain = vec![root.try_clone()?];
        for component in &target.parent_components {
            let current = chain.last().expect("projection chain contains root");
            projection_real_directory(current, component)?;
            chain.push(open_projection_dir_nofollow(current, component)?);
        }
        Ok(ProjectionParent { chain })
    }

    pub(super) fn projection_parent_optional(
        &self,
        target: &ProjectionTarget,
    ) -> io::Result<Option<ProjectionParent>> {
        match self.projection_parent_capture(target)? {
            ProjectionParentCapture::Present(parent) => Ok(Some(parent)),
            ProjectionParentCapture::Missing => {
                self.ensure_projection_root_binding()?;
                match self.projection_parent_capture(target)? {
                    ProjectionParentCapture::Missing => {
                        self.ensure_projection_root_binding()?;
                        Ok(None)
                    }
                    ProjectionParentCapture::Present(_) => Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "projection parent appeared during absence capture",
                    )),
                }
            }
        }
    }

    fn projection_parent_capture(
        &self,
        target: &ProjectionTarget,
    ) -> io::Result<ProjectionParentCapture> {
        let root = self.projection_root.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "graph has no retained no-follow projection capability",
            )
        })?;
        let mut chain = vec![root.try_clone()?];
        for component in &target.parent_components {
            let current = chain.last().expect("projection chain contains root");
            match projection_real_directory(current, component) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(ProjectionParentCapture::Missing);
                }
                Err(error) => return Err(error),
            }
            // A component that disappears or changes after successful shape
            // validation is a traversal race, not semantic absence.
            chain.push(open_projection_dir_nofollow(current, component)?);
        }
        Ok(ProjectionParentCapture::Present(ProjectionParent { chain }))
    }

    pub(super) fn ensure_projection_target_shape(
        &self,
        parent: &ProjectionParent,
        target: &ProjectionTarget,
    ) -> io::Result<()> {
        projection_optional_regular_metadata(parent.final_dir(), &target.filename)?;
        let (target_stem, _) = split_logseq_text_filename(&target.filename).ok_or_else(bad_path)?;
        for extension in LOGSEQ_TEXT_EXTENSIONS {
            let sibling = format!("{target_stem}.{extension}");
            if sibling == target.filename {
                continue;
            }
            // An authenticated exact projection may coexist with independent
            // regular text siblings. Validate their shape without granting
            // them authority over the target path.
            projection_optional_regular_metadata(parent.final_dir(), &sibling)?;
        }
        Ok(())
    }

    pub(super) fn ensure_projection_parent_binding(
        &self,
        parent: &ProjectionParent,
        target: &ProjectionTarget,
    ) -> io::Result<()> {
        let rebound = self.projection_parent(target)?;
        if projection_dir_identity(rebound.final_dir())?
            != projection_dir_identity(parent.final_dir())?
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "projection parent changed during publication",
            ));
        }
        Ok(())
    }

    /// Resolve the exact on-disk source file for an explicit user file action.
    /// A loaded page's recorded relative path always wins (including nested and
    /// duplicate-name files); a newly saved page without a refreshed path may
    /// fall back to normal name resolution. The final canonical-file check keeps
    /// symlinks from escaping the configured pages/journals directories.
    pub fn page_source_file(
        &self,
        name: &str,
        kind: PageKind,
        recorded_path: Option<&str>,
    ) -> io::Result<PathBuf> {
        let candidate = recorded_path
            .filter(|path| !path.trim().is_empty())
            .map(|path| {
                self.resolve_rel(path)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid page path"))
            })
            .unwrap_or_else(|| Ok(self.path_for(name, kind)))?;
        let canonical = candidate.canonicalize()?;
        if !canonical.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "page source is not a file",
            ));
        }
        let root = self.root.canonicalize()?;
        if !canonical.starts_with(&root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "page source escapes graph text scope",
            ));
        }
        Ok(canonical)
    }

    /// Whether a journal file is a "shadow": a non-date-stem file (e.g. a leftover
    /// title-named `Friday, 26-06-2026.org`) that coexists with a canonical
    /// date-stem file (`2026_06_26.{md,org}`) for the SAME day. The `(kind,name)`
    /// cache slot belongs to the canonical file, so a shadow must never be folded
    /// into it (that would make name-resolution serve the shadow's content). A
    /// shadow is loaded fresh by path on demand instead (#21). Twins (two date-stem
    /// files of the same day in different extensions) are deliberately NOT shadows —
    /// that case keeps its existing `has_twin`/dedup handling.
    pub(super) fn is_shadow_journal(&self, path: &Path, date: crate::date::JournalDate) -> bool {
        let is_date_stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| crate::date::JournalDate::from_file_stem(s).is_some());
        if is_date_stem {
            return false;
        }
        let canonical = self.journal_format.file_stem(date);
        // Every Logseq text extension, not a hand-written md/org pair. `.markdown`
        // is a first-class page extension (LOGSEQ_TEXT_EXTENSIONS, and OG accepts
        // it case-insensitively). Asking only two meant a title-named
        // leftover coexisting with a canonical `2026_06_26.markdown` was NOT
        // recognised as a shadow, so it was reconciled into the (kind,name) cache
        // and name resolution served the WRONG file for that day — exactly the #21
        // defect this function exists to prevent, reachable only in a `.markdown`
        // graph. (Direct Files data-safety audit, 2026-08-09, finding 15.)
        configured_text_variant_paths(&self.journals_path(), &canonical)
            .iter()
            .any(|candidate| candidate.is_file())
    }

    /// The format (`Md`/`Org`) new pages and journals are created in, from
    /// `config.edn`'s `:preferred-format`. Existing files keep their own format.
    pub fn preferred_format(&self) -> Format {
        self.config().preferred_format
    }
}
