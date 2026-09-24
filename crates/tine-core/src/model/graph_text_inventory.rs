//! Graph's graph-text inventory: the bounded walk over configured roots,
//! inventory entries and lookup, decoding a present file, twins, and the
//! shadow-journal and cacheability tests.

use super::*;

/// The listing skip for a FIFO, socket or device named like a page: never
/// graph text, so it owns no page name (`failures_that_could_own`).
pub(super) const NOT_A_REGULAR_FILE_SKIP: &str = ": graph text entry is not a regular file";

impl Graph {
    pub(super) fn text_entries_with_limits_and_budget<'a>(
        &self,
        permit: &GraphTextWritePermit,
        include_sync_conflicts: bool,
        limits: GraphTextInventoryLimits,
        budget: Option<&RetainedContentBudget>,
        roots: Vec<(&'a str, usize)>,
        graph_wide: bool,
    ) -> io::Result<(
        Vec<PageEntry>,
        Option<RetainedContentReservation>,
        std::collections::HashMap<PathBuf, ContentDigest>,
        Vec<String>,
    )> {
        struct PendingDirectory {
            directory: Dir,
            path: PathBuf,
            depth: usize,
        }

        self.graph_text_permit_root(permit)?;
        let mut out = Vec::new();
        let mut out_charge =
            RetainedHeapCharge::new(budget, "graph inventory retained page entries")?;
        let mut pending = Vec::new();
        let mut pending_slots =
            RetainedHeapCharge::new(budget, "graph inventory pending vector capacity")?;
        let mut pending_paths =
            RetainedHeapCharge::new(budget, "graph inventory pending owned paths")?;
        let mut pending_slot_high_water = 0_usize;
        let mut all_entries = 0_usize;
        let mut directory_count = 0_usize;
        let mut path_bytes = 0_u64;
        let mut directory_resources = std::collections::BTreeMap::new();
        let mut directory_resources_charge =
            RetainedHeapCharge::new(budget, "graph inventory directory identity map")?;
        let mut file_resources = std::collections::BTreeMap::new();
        let mut graph_file_identities = std::collections::HashMap::new();
        let mut file_resources_charge =
            RetainedHeapCharge::new(budget, "graph inventory file identity map")?;
        let mut portable_paths = std::collections::BTreeMap::new();
        let mut portable_paths_charge =
            RetainedHeapCharge::new(budget, "graph text portable path identity map")?;
        // GH #332: the graph-wide READ inventory skips an entry it cannot admit
        // instead of failing the whole walk. One FIFO, unreadable folder or
        // oddly named image anywhere under the root used to leave the page
        // cache with zero pages, so every page opened blank. A page file that
        // cannot be admitted is reported here (the caller records it as a page
        // index failure, so the source is not treated as complete). An
        // unreadable folder or a non-page entry is simply not graph text Tine
        // can see; reporting it would keep a graph whose root holds, say,
        // `System Volume Information` on the uncached listing path forever.
        // The configured walk (writes) and the limits stay fail-closed.
        let mut skipped = Vec::new();
        macro_rules! admit_or_skip {
            ($result:expr, $relative:expr, $may_hold_page:expr) => {
                match $result {
                    Ok(value) => value,
                    // Gone since the directory read named it: a deleted
                    // page, not one Tine failed to read (GH #543).
                    Err(error) if graph_wide && error.kind() == io::ErrorKind::NotFound => {
                        continue;
                    }
                    Err(error) if graph_wide => {
                        if $may_hold_page {
                            skipped.push(format!("{}: {error}", $relative));
                        }
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            };
        }
        for (relative, depth) in roots {
            if depth > limits.directory_depth {
                return Err(graph_text_inventory_limit_error("graph directory depth"));
            }
            path_bytes = path_bytes
                .checked_add(usize_to_u64(relative.len())?)
                .ok_or_else(|| graph_text_inventory_limit_error("aggregate path bytes"))?;
            if path_bytes > limits.path_bytes {
                return Err(graph_text_inventory_limit_error("aggregate path bytes"));
            }
            let path = self.root.join(relative);
            let sentinel = path.join(".tine-capability-inventory");
            let target = match self.graph_text_target(permit, &sentinel, false) {
                Ok(target) => target,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            directory_count = directory_count
                .checked_add(1)
                .ok_or_else(|| graph_text_inventory_limit_error("directory count"))?;
            if directory_count > limits.directories {
                return Err(graph_text_inventory_limit_error("directory count"));
            }
            let directory = target.parent().try_clone()?;
            let resource = canonical_projection_directory_resource_id(&directory)?;
            directory_resources_charge.grow(
                checked_add_bytes(
                    conservative_btree_entry_bytes::<ContentDigest, String>()?,
                    owned_string_upper_bound(relative)?,
                )?,
                "graph inventory directory identity map",
            )?;
            if let Some(first) = directory_resources.insert(resource, relative.to_owned()) {
                return Err(graph_text_inventory_alias_error(
                    "directories",
                    &first,
                    relative,
                ));
            }
            if pending.len() == limits.pending_directories {
                return Err(graph_text_inventory_limit_error("pending directories"));
            }
            if pending.len() == pending_slot_high_water {
                pending_slots.grow(
                    conservative_vec_entry_bytes::<PendingDirectory>()?,
                    "graph inventory pending vector capacity",
                )?;
                pending_slot_high_water = pending_slot_high_water
                    .checked_add(1)
                    .ok_or_else(|| graph_text_inventory_limit_error("pending directories"))?;
            }
            pending_paths.grow(
                owned_path_upper_bound(&path)?,
                "graph inventory pending owned paths",
            )?;
            pending.push(PendingDirectory {
                directory,
                path,
                depth,
            });
        }

        while let Some(PendingDirectory {
            directory,
            path,
            depth,
        }) = pending.pop()
        {
            let pending_path_charge = owned_path_upper_bound(&path)?;
            let entries = match directory.entries() {
                Ok(entries) => entries,
                Err(_) if graph_wide && depth > 0 => {
                    pending_paths
                        .shrink(pending_path_charge, "graph inventory pending owned paths")?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            for entry in entries {
                #[cfg(test)]
                if graph_wide {
                    GRAPH_TEXT_INVENTORY_ENTRY_VISITS
                        .with(|visits| visits.set(visits.get().saturating_add(1)));
                }
                all_entries = all_entries
                    .checked_add(1)
                    .ok_or_else(|| graph_text_inventory_limit_error("all directory entries"))?;
                if all_entries > limits.all_entries {
                    return Err(graph_text_inventory_limit_error("all directory entries"));
                }
                let entry = admit_or_skip!(entry, self.rel_path(&path), false);
                let name = entry.file_name();
                let Some(name_text) = name.to_str() else {
                    if graph_wide {
                        // Not nameable as graph text. Report it only when it
                        // looks like a page file.
                        let lossy = name.to_string_lossy();
                        if is_page_file(&path.join(&name)) {
                            skipped.push(format!(
                                "{}/{lossy}: graph text entry name is not UTF-8",
                                self.rel_path(&path)
                            ));
                        }
                        continue;
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "graph text entry name is not UTF-8",
                    ));
                };
                let mut entry_scratch =
                    RetainedHeapCharge::new(budget, "graph inventory entry path scratch")?;
                entry_scratch.grow(
                    checked_mul_bytes(
                        checked_add_bytes(
                            owned_path_upper_bound(&path)?,
                            owned_string_upper_bound(name_text)?,
                        )?,
                        3,
                    )?,
                    "graph inventory entry path scratch",
                )?;
                let child_path = path.join(&name);
                let child_relative = self.rel_path(&child_path);
                path_bytes = path_bytes
                    .checked_add(usize_to_u64(child_relative.len())?)
                    .ok_or_else(|| graph_text_inventory_limit_error("aggregate path bytes"))?;
                if path_bytes > limits.path_bytes {
                    return Err(graph_text_inventory_limit_error("aggregate path bytes"));
                }
                let file_type =
                    admit_or_skip!(entry.file_type(), child_relative, is_page_file(&child_path));
                // The configured walk resolves page identities and rewrites
                // page text; entries that can never be a page are not its
                // business. Hidden entries (an Emacs `.#name.org` lock
                // symlink, `.DS_Store`) and non-page files (images, PDFs, an
                // iCloud file whose download is refused) used to be opened or
                // refused here, which blanked the Journals view and failed
                // today's journal on such graphs (GH #385). A symlink is not a
                // graph-text document on any other path either (the save
                // capture skips it since GH #267, `graph_inventory_entry` never
                // admits one, and every write target is opened no-follow), so
                // no in-scope scenario is defended by refusing it here. Two
                // page files on one inode are still refused below.
                if !graph_wide
                    && (file_type.is_symlink()
                        || (!file_type.is_dir()
                            && (name_text.starts_with('.') || !is_page_file(&child_path))))
                {
                    continue;
                }
                if file_type.is_symlink() {
                    if graph_wide {
                        continue;
                    }
                    return Err(DirectSaveError::into_io(
                        DirectSaveFailureCode::PrecheckSymlink,
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "graph text entry is a symlink or reparse point: {child_relative}"
                            ),
                        ),
                    ));
                }
                if file_type.is_file() {
                    if graph_wide && !self.graph_text_scope.is_eligible(&child_relative) {
                        continue;
                    }
                    #[cfg(test)]
                    {
                        let mut vanish = self.page_build_test.vanish_inside_listing.lock().unwrap();
                        if vanish.as_ref() == Some(&child_path) {
                            std::fs::remove_file(vanish.take().unwrap()).unwrap();
                        }
                    }
                    let file = admit_or_skip!(
                        open_projection_file_nofollow(&directory, name_text),
                        child_relative,
                        true
                    );
                    let resource = admit_or_skip!(
                        canonical_projection_file_resource_id(&file),
                        child_relative,
                        true
                    );
                    if graph_wide {
                        graph_file_identities.insert(child_path.clone(), resource);
                    }
                    file_resources_charge.grow(
                        checked_add_bytes(
                            conservative_btree_entry_bytes::<ContentDigest, String>()?,
                            owned_string_upper_bound(&child_relative)?,
                        )?,
                        "graph inventory file identity map",
                    )?;
                    if let Some(first) = file_resources.insert(resource, child_relative.clone()) {
                        if !graph_wide {
                            return Err(graph_text_inventory_alias_error(
                                "files",
                                &first,
                                &child_relative,
                            ));
                        }
                    }
                    if !is_page_file(&child_path) {
                        continue;
                    }
                    let Some(stem) = child_path.file_stem().and_then(|stem| stem.to_str()) else {
                        continue;
                    };
                    if !include_sync_conflicts && is_sync_conflict(stem) {
                        continue;
                    }
                    let page_candidate_charge = checked_add_bytes(
                        conservative_vec_entry_bytes::<PageEntry>()?,
                        checked_add_bytes(
                            owned_string_upper_bound(stem)?,
                            checked_add_bytes(
                                owned_string_upper_bound(&child_relative)?,
                                owned_path_upper_bound(&child_path)?,
                            )?,
                        )?,
                    )?;
                    out_charge.grow(
                        page_candidate_charge,
                        "graph inventory retained page entries",
                    )?;
                    if graph_wide {
                        let portable = self
                            .graph_text_scope
                            .portable_path_key(&child_relative)
                            .expect("eligible graph path has a portable key");
                        portable_paths_charge.grow(
                            checked_add_bytes(
                                conservative_btree_entry_bytes::<String, String>()?,
                                checked_add_bytes(
                                    owned_string_upper_bound(&portable)?,
                                    owned_string_upper_bound(&child_relative)?,
                                )?,
                            )?,
                            "graph text portable path identity map",
                        )?;
                        // Portable aliases remain discoverable for recovery. Exact
                        // write validation below refuses every member of the
                        // colliding group until the user disambiguates it.
                        portable_paths
                            .entry(portable)
                            .or_insert(child_relative.clone());
                    }
                    let page = if graph_wide {
                        admit_or_skip!(
                            self.graph_inventory_entry(&child_path),
                            child_relative,
                            true
                        )
                    } else {
                        self.graph_text_inventory_entry(&child_path)?
                    };
                    if let Some(page) = page {
                        if out.len() == limits.graph_text_files {
                            return Err(graph_text_inventory_limit_error("graph file count"));
                        }
                        out.push(page);
                    } else {
                        out_charge.shrink(
                            page_candidate_charge,
                            "graph inventory retained page entries",
                        )?;
                    }
                    continue;
                }
                if !file_type.is_dir() {
                    if graph_wide {
                        // A FIFO, socket or device is never graph text.
                        if is_page_file(&child_path) {
                            skipped.push(format!("{child_relative}{NOT_A_REGULAR_FILE_SKIP}"));
                        }
                        continue;
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("graph text entry is not a regular file: {child_relative}"),
                    ));
                }
                if name_text.starts_with('.') {
                    continue;
                }
                if graph_wide && !self.graph_text_scope.should_descend(&child_relative) {
                    continue;
                }
                let child_depth = depth
                    .checked_add(1)
                    .ok_or_else(|| graph_text_inventory_limit_error("graph directory depth"))?;
                if child_depth > limits.directory_depth {
                    return Err(graph_text_inventory_limit_error("graph directory depth"));
                }
                directory_count = directory_count
                    .checked_add(1)
                    .ok_or_else(|| graph_text_inventory_limit_error("directory count"))?;
                if directory_count > limits.directories {
                    return Err(graph_text_inventory_limit_error("directory count"));
                }
                admit_or_skip!(
                    projection_real_directory(&directory, name_text),
                    child_relative,
                    false
                );
                let child = admit_or_skip!(
                    open_projection_dir_nofollow(&directory, name_text),
                    child_relative,
                    false
                );
                let resource = admit_or_skip!(
                    canonical_projection_directory_resource_id(&child),
                    child_relative,
                    false
                );
                directory_resources_charge.grow(
                    checked_add_bytes(
                        conservative_btree_entry_bytes::<ContentDigest, String>()?,
                        owned_string_upper_bound(&child_relative)?,
                    )?,
                    "graph inventory directory identity map",
                )?;
                if let Some(first) = directory_resources.insert(resource, child_relative.clone()) {
                    admit_or_skip!(
                        Err::<(), _>(graph_text_inventory_alias_error(
                            "directories",
                            &first,
                            &child_relative,
                        )),
                        child_relative,
                        false
                    );
                }
                if pending.len() == limits.pending_directories {
                    return Err(graph_text_inventory_limit_error("pending directories"));
                }
                if pending.len() == pending_slot_high_water {
                    pending_slots.grow(
                        conservative_vec_entry_bytes::<PendingDirectory>()?,
                        "graph inventory pending vector capacity",
                    )?;
                    pending_slot_high_water = pending_slot_high_water
                        .checked_add(1)
                        .ok_or_else(|| graph_text_inventory_limit_error("pending directories"))?;
                }
                pending_paths.grow(
                    owned_path_upper_bound(&child_path)?,
                    "graph inventory pending owned paths",
                )?;
                pending.push(PendingDirectory {
                    directory: child,
                    path: child_path,
                    depth: child_depth,
                });
            }
            pending_paths.shrink(pending_path_charge, "graph inventory pending owned paths")?;
        }
        out.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
        Ok((out, out_charge.reservation, graph_file_identities, skipped))
    }

    /// Return the non-overlapping roots that must be walked for a configured-root
    /// inventory.  Nested roots are discovered through their outer root, then
    /// classified by their exact graph-relative path below; equal roots have no
    /// unambiguous owner and fail before any file is parsed or mutated.
    pub(super) fn configured_text_inventory_roots<'c>(
        &self,
        config: &'c Config,
        permit: &GraphTextWritePermit,
    ) -> io::Result<Vec<(&'c str, usize)>> {
        let page_root = configured_root_components(&config.pages_dir).ok_or_else(bad_path)?;
        let journal_root = configured_root_components(&config.journals_dir).ok_or_else(bad_path)?;
        if page_root == journal_root {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "graph page and journal roots must not be equal",
            ));
        }

        let configured = [
            (&config.pages_dir, page_root),
            (&config.journals_dir, journal_root),
        ];
        let mut resources = std::collections::BTreeMap::new();
        for (root, _) in configured.iter() {
            let sentinel = self.root.join(root).join(".tine-capability-inventory");
            let target = match self.graph_text_target(permit, &sentinel, false) {
                Ok(target) => target,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let resource = canonical_projection_directory_resource_id(target.parent())?;
            if let Some(first) = resources.insert(resource, (*root).to_owned()) {
                return Err(graph_text_inventory_alias_error("roots", &first, root));
            }
        }
        Ok(configured
            .iter()
            .filter_map(|(root, components)| {
                let nested = configured.iter().any(|(_, candidate)| {
                    candidate.len() < components.len() && components.starts_with(candidate)
                });
                (!nested).then_some((root.as_str(), components.len()))
            })
            .collect())
    }

    /// Construct a list entry only after assigning the exact path's canonical
    /// longest-root owner. This is also the only ownership rule used by cache
    /// paths through `entry_for_path`.
    pub(super) fn graph_text_inventory_entry(&self, path: &Path) -> io::Result<Option<PageEntry>> {
        if !is_page_file(path) {
            return Ok(None);
        }
        let rel_path = self.rel_path(path);
        let graph_text_path = GraphTextPath::parse(rel_path.clone()).map_err(|_| bad_path())?;
        let mut entry = self
            .graph_text_entry_for_graph_text_path(&graph_text_path)
            .map_err(|_| bad_path())?;
        entry.rel_path = rel_path;
        entry.path = path.to_path_buf();
        Ok(Some(entry))
    }

    pub(super) fn graph_text_entry_for_path(&self, path: &Path) -> io::Result<Option<PageEntry>> {
        self.graph_inventory_entry(path)
    }

    pub(super) fn graph_inventory_entry(&self, path: &Path) -> io::Result<Option<PageEntry>> {
        let rel_path = self.rel_path(path);
        if !self.graph_text_scope.is_eligible(&rel_path) {
            return Ok(None);
        }
        let mut entry = self.graph_entry_for_relative_path(&rel_path)?;
        entry.rel_path = rel_path;
        entry.path = path.to_path_buf();
        Ok(Some(entry))
    }

    pub(super) fn graph_entry_for_relative_path(&self, relative: &str) -> io::Result<PageEntry> {
        let filename = Path::new(relative)
            .file_name()
            .and_then(|filename| filename.to_str())
            .ok_or_else(bad_path)?;
        let (stem, extension) = filename.rsplit_once('.').ok_or_else(bad_path)?;
        if stem.is_empty()
            || !(extension.eq_ignore_ascii_case("md")
                || extension.eq_ignore_ascii_case("markdown")
                || extension.eq_ignore_ascii_case("org"))
        {
            return Err(bad_path());
        }
        let decoded = decode_page_name(stem, self.config().file_name_format);
        let (name, kind, date_key) = match self.journal_format.parse(&decoded) {
            Some(date) => (
                self.journal_format.title(date),
                PageKind::Journal,
                Some(date.ordinal_key()),
            ),
            None => (decoded, PageKind::Page, None),
        };
        Ok(PageEntry {
            name,
            kind,
            date_key,
            rel_path: relative.to_owned(),
            path: self.root.join(relative),
        })
    }

    /// Filename-only semantic evidence for an exact graph-relative path.
    ///
    /// This is intentionally provisional: callers with present bytes must use
    /// `decode_present_graph_text`, which parses content title properties before
    /// assigning kind. Directories never choose Page versus Journal.
    fn provisional_graph_entry_for_graph_text_path(
        &self,
        path: &GraphTextPath,
    ) -> io::Result<PageEntry> {
        if !self.graph_text_scope.is_eligible(path.as_str()) {
            return Err(bad_path());
        }
        self.graph_entry_for_relative_path(path.as_str())
    }

    /// Parse one present external document through the canonical content
    /// semantic boundary. The caller chooses whether import-grade round-trip
    /// evidence is required; both modes use the same parser-owned identity
    /// extraction and exact source-span parse.
    pub(crate) fn parse_external_document(
        &self,
        path: &GraphTextPath,
        bytes: &[u8],
    ) -> io::Result<ParsedExternalDocument> {
        let content = std::str::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("graph text is not UTF-8: {path}"),
            )
        })?;
        let fallback = self.provisional_graph_entry_for_graph_text_path(path)?;
        parse_external_document(self, fallback, content)
    }

    #[cfg(test)]
    pub(super) fn decode_present_graph_text(
        &self,
        path: &GraphTextPath,
        bytes: &[u8],
        permit: GraphTextParseBudgetPermit,
    ) -> io::Result<(PageEntry, Format)> {
        count_graph_text_admission_parser_invocation();
        graph_text_parse_failure_hook()?;
        let parsed = self.parse_external_document(path, bytes)?;
        let semantic = parsed.effective;
        let format = parsed.format;
        let actual_name_allocation = checked_add_bytes(
            usize_to_u64(std::mem::size_of::<String>())?,
            usize_to_u64(semantic.name.capacity())?,
        )?;
        if usize_to_u64(semantic.name.len())? > permit.semantic_name_bytes
            || actual_name_allocation > permit.semantic_name_allocation_bytes
        {
            return Err(graph_text_capture_limit_error(
                "rendered semantic title allocation",
            ));
        }
        Ok((semantic, format))
    }

    /// Parse one present graph-text file through the same semantic path used by
    /// graph-wide admission, while returning only a bounded structural count
    /// for the inactive source-capture per-file limit.  The document is dropped
    /// before this method returns.
    pub(super) fn decode_present_graph_text_with_node_count(
        &self,
        path: &GraphTextPath,
        bytes: &[u8],
        permit: GraphTextParseBudgetPermit,
    ) -> io::Result<(PageEntry, Format, u64)> {
        count_graph_text_admission_parser_invocation();
        graph_text_parse_failure_hook()?;
        let parsed = self.parse_external_document(path, bytes)?;
        let semantic = parsed.effective;
        let format = parsed.format;
        let document = parsed.parsed.document;
        let actual_name_allocation = checked_add_bytes(
            usize_to_u64(std::mem::size_of::<String>())?,
            usize_to_u64(semantic.name.capacity())?,
        )?;
        if usize_to_u64(semantic.name.len())? > permit.semantic_name_bytes
            || actual_name_allocation > permit.semantic_name_allocation_bytes
        {
            return Err(graph_text_capture_limit_error(
                "rendered semantic title allocation",
            ));
        }
        Ok((semantic, format, graph_text_document_node_count(&document)?))
    }

    /// Interpret one already-validated graph-text path exactly as the inventory
    /// callers expect.
    ///
    /// A path under a configured `pages/`/`journals/` root keeps the exact
    /// configured-root interpretation this authority has always produced.
    /// Ordinary graph text outside those roots is decoded from its file name
    /// alone through the same graph-wide decoder `Graph::list_pages` already
    /// uses, because that is what OG does at
    /// 6e7afa8eb040686ff057156ee877193b581dd369:
    /// `deps/graph-parser/src/logseq/graph_parser/extract.cljc`
    /// (`get-page-name`) takes only the last path component, and
    /// `deps/graph-parser/src/logseq/graph_parser/block.cljs`
    /// (`convert-page-if-journal`) decides journal-ness by parsing that title
    /// as a date. The containing directory therefore never
    /// chooses Page versus Journal, and the exact nested spelling is retained.
    pub(crate) fn graph_text_entry_for_graph_text_path(
        &self,
        path: &GraphTextPath,
    ) -> Result<PageEntry, UnsafeGraphTextPath> {
        match self.classify_graph_text_path(path) {
            Ok(GraphTextKind::Page | GraphTextKind::Journal) => {}
            Err(outside) => return self.unconfigured_graph_text_entry(path, outside),
        }
        self.graph_entry_for_relative_path(path.as_str())
            .map_err(|_| UnsafeGraphTextPath(path.as_str().to_owned()))
    }

    /// OG-compatible decode for supported graph text that no configured root
    /// owns. Containers OG itself skips, hidden paths, provider conflict copies
    /// and spellings the guarded writer cannot address keep the original
    /// configured-root rejection instead of gaining new authority here.
    fn unconfigured_graph_text_entry(
        &self,
        path: &GraphTextPath,
        outside: UnsafeGraphTextPath,
    ) -> Result<PageEntry, UnsafeGraphTextPath> {
        if !is_logseq_text_extension(path.extension())
            || !self.graph_text_scope.is_eligible(path.as_str())
        {
            return Err(outside);
        }
        self.graph_entry_for_relative_path(path.as_str())
            .map_err(|_| UnsafeGraphTextPath(path.as_str().to_owned()))
    }

    pub(super) fn graph_text_find_entry(
        &self,
        permit: &GraphTextWritePermit,
        name: &str,
        kind: PageKind,
    ) -> io::Result<Option<PageEntry>> {
        let mut matching = self
            .configured_text_entries(permit, false)?
            .into_iter()
            .filter(|entry| entry.kind == kind && crate::refs::same_page(&entry.name, name));
        let Some(mut winner) = matching.next() else {
            return Ok(None);
        };
        for entry in matching {
            if !is_date_stem_entry(&winner) && is_date_stem_entry(&entry) {
                winner = entry;
            }
        }
        Ok(Some(winner))
    }

    pub(super) fn graph_text_has_twin(
        &self,
        permit: &GraphTextWritePermit,
        name: &str,
        kind: PageKind,
    ) -> io::Result<bool> {
        let (dir, stem) = match kind {
            PageKind::Page => (
                self.pages_path(),
                Some(encode_page_name(name, self.config().file_name_format)),
            ),
            PageKind::Journal => (
                self.journals_path(),
                self.journal_format
                    .parse(name)
                    .map(|date| self.journal_format.file_stem(date)),
            ),
        };
        let Some(stem) = stem else {
            return Ok(false);
        };
        let mut variants = 0;
        for path in configured_text_variant_paths(&dir, &stem) {
            if self.graph_text_exists(permit, &path)? {
                variants += 1;
            }
        }
        Ok(variants > 1)
    }

    /// GH #597: a path built from a page name names the page's file only in
    /// the file's own spelling. On a case- (or normalization-) insensitive
    /// filesystem `pages/Contents.md` also finds `contents.md`; handing that
    /// spelling out gives the editor a path `resolve_rel` refuses. Such a hit
    /// is answered from the inventory, which matches names case-insensitively
    /// and carries each file's spelling on disk.
    fn graph_text_as_spelled(
        &self,
        permit: &GraphTextWritePermit,
        found: PathBuf,
        name: &str,
        kind: PageKind,
    ) -> io::Result<PathBuf> {
        if !path_uses_graph_text_alias(&self.root, &found) {
            return Ok(found);
        }
        self.graph_text_find_entry(permit, name, kind)?
            .map(|entry| entry.path)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the page's file is not spelled as its name on disk",
                )
            })
    }

    pub(super) fn graph_text_path_for(
        &self,
        permit: &GraphTextWritePermit,
        name: &str,
        kind: PageKind,
    ) -> io::Result<PathBuf> {
        let preferred = self.preferred_format();
        match kind {
            PageKind::Journal => Ok(self
                .graph_text_find_entry(permit, name, kind)?
                .map(|entry| entry.path)
                .unwrap_or_else(|| {
                    let stem = self
                        .journal_format
                        .parse(name)
                        .map(|date| self.journal_format.file_stem(date))
                        .unwrap_or_else(|| name.to_owned());
                    self.journals_path()
                        .join(format!("{stem}.{}", preferred.ext()))
                })),
            PageKind::Page => {
                let encoded = encode_page_name(name, self.config().file_name_format);
                let primary = self
                    .pages_path()
                    .join(format!("{encoded}.{}", preferred.ext()));
                if self.graph_text_exists(permit, &primary)? {
                    return self.graph_text_as_spelled(permit, primary, name, kind);
                }
                for alternate in configured_text_variant_paths(&self.pages_path(), &encoded) {
                    if alternate == primary {
                        continue;
                    }
                    if self.graph_text_exists(permit, &alternate)? {
                        return self.graph_text_as_spelled(permit, alternate, name, kind);
                    }
                }
                Ok(primary)
            }
        }
    }

    pub(super) fn is_shadow_journal_under_permit(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        date: crate::date::JournalDate,
    ) -> io::Result<bool> {
        let is_date_stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| crate::date::JournalDate::from_file_stem(stem).is_some());
        if is_date_stem {
            return Ok(false);
        }
        let canonical = self.journal_format.file_stem(date);
        let directory = self.journals_path();
        for path in configured_text_variant_paths(&directory, &canonical) {
            if self.graph_text_exists(permit, &path)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn graph_text_path_is_cacheable(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<bool> {
        if let Some(entry) = self.entry_for_path(path) {
            if entry.kind == PageKind::Journal {
                if let Some(date) = entry.date_key.map(crate::date::JournalDate::from_ordinal) {
                    return Ok(!self.is_shadow_journal_under_permit(permit, path, date)?);
                }
            }
        }
        Ok(true)
    }
}
