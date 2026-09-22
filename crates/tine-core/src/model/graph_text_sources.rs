//! Graph's configured graph-text sources: listing the configured text entries
//! under limits and budgets, the source digest, and the legacy identity check.

use super::*;

impl Graph {
    /// Recursively enumerate the configured page/journal trees through the exact
    /// retained root carried by `permit`. Returned paths are lexical names under
    /// `self.root`, but every directory decision comes from a no-follow retained
    /// directory handle, so replacing the ambient graph pathname cannot change
    /// the selected mutation set.
    pub(super) fn configured_text_entries(
        &self,
        permit: &GraphTextWritePermit,
        include_sync_conflicts: bool,
    ) -> io::Result<Vec<PageEntry>> {
        Ok(self
            .configured_text_entries_with_limits_and_budget(
                permit,
                include_sync_conflicts,
                graph_text_inventory_limits(),
                None,
            )?
            .0)
    }

    pub(super) fn configured_text_entries_with_budget(
        &self,
        permit: &GraphTextWritePermit,
        include_sync_conflicts: bool,
        budget: &RetainedContentBudget,
    ) -> io::Result<BudgetedPageEntries> {
        let (entries, reservation) = self.configured_text_entries_with_limits_and_budget(
            permit,
            include_sync_conflicts,
            graph_text_inventory_limits(),
            Some(budget),
        )?;
        Ok(BudgetedPageEntries {
            entries,
            _reservation: reservation
                .expect("budgeted graph inventory returns its retained charge"),
        })
    }

    fn configured_text_entries_with_limits_and_budget(
        &self,
        permit: &GraphTextWritePermit,
        include_sync_conflicts: bool,
        limits: GraphTextInventoryLimits,
        budget: Option<&RetainedContentBudget>,
    ) -> io::Result<(Vec<PageEntry>, Option<RetainedContentReservation>)> {
        let config = self.config();
        let roots = self.configured_text_inventory_roots(&config, permit)?;
        let (entries, reservation, _, _) = self.text_entries_with_limits_and_budget(
            permit,
            include_sync_conflicts,
            limits,
            budget,
            roots,
            false,
        )?;
        Ok((entries, reservation))
    }

    pub(super) fn graph_text_entries(
        &self,
        permit: &GraphTextWritePermit,
    ) -> io::Result<Vec<PageEntry>> {
        Ok(self.graph_text_inventory(permit)?.0)
    }

    /// [`Self::graph_text_entries`] plus the entries the read walk had to skip
    /// (GH #332), which a page build records as page index failures.
    pub(super) fn graph_text_entries_and_skipped(
        &self,
        permit: &GraphTextWritePermit,
    ) -> io::Result<(Vec<PageEntry>, Vec<String>)> {
        let (entries, _, skipped) = self.graph_text_inventory(permit)?;
        Ok((entries, skipped))
    }

    /// Enumerate every user-visible Markdown/Org source file using the same
    /// scope and nested-layout rules as Tine's ordinary graph inventory.
    pub fn graph_text_source_paths(&self) -> io::Result<Vec<String>> {
        let permit = self.admit_retained_graph_text_writer()?;
        self.graph_text_entries(&permit)
            .map(|entries| entries.into_iter().map(|entry| entry.rel_path).collect())
    }

    /// Hash the actual bytes of one path returned by
    /// [`Graph::graph_text_source_paths`]. The file is opened without following
    /// links, streamed through SHA-256, then rechecked through its path. Any
    /// observed replacement or metadata change is an incomplete verification,
    /// never a digest result.
    pub fn digest_graph_text_source(
        &self,
        relative: &str,
        cancelled: &AtomicBool,
    ) -> io::Result<GraphTextSourceDigest> {
        if cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                crate::backend_error::tagged_backend_error("operation-cancelled", None),
            ));
        }
        let text_path = GraphTextPath::parse(relative.to_owned()).map_err(|_| bad_path())?;
        let absolute = self.root.join(text_path.as_str());
        let Some(entry) = self.graph_inventory_entry(&absolute)? else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is not an admitted graph-text source",
            ));
        };
        if entry.rel_path != text_path.as_str() {
            return Err(bad_path());
        }

        let permit = self.admit_retained_graph_text_writer()?;
        let target = self.graph_text_target(&permit, &absolute, false)?;
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let mut file = open_projection_file_nofollow(target.parent(), &target.filename)?;
        let before = file.metadata()?;
        let before_modified = before.modified()?;
        let before_identity = canonical_projection_file_resource_id(&file)?;
        let mut hasher = Sha256::new();
        let mut length = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    crate::backend_error::tagged_backend_error("operation-cancelled", None),
                ));
            }
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            length = length
                .checked_add(read as u64)
                .ok_or_else(allocation_overflow)?;
        }
        let after = file.metadata()?;
        let rebound = open_projection_file_nofollow(target.parent(), &target.filename)?;
        let rebound_metadata = rebound.metadata()?;
        let stable = length == before.len()
            && after.len() == before.len()
            && after.modified()? == before_modified
            && rebound_metadata.len() == before.len()
            && rebound_metadata.modified()? == before_modified
            && canonical_projection_file_resource_id(&rebound)? == before_identity;
        if !stable {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "graph source changed while it was being verified",
            ));
        }
        Ok(GraphTextSourceDigest {
            path: text_path.to_string(),
            length,
            digest: format!("{:x}", hasher.finalize()),
        })
    }

    /// Check historical page filenames through the retained graph capability.
    ///
    /// Unlike graph-text inventory, this deliberately admits legacy filenames
    /// which cannot be parsed as `GraphTextPath`s. It is filename-only evidence:
    /// parser-derived effective-title validation remains with the admitted
    /// graph-text inventory below the caller.
    pub(super) fn retained_legacy_page_identity_exists(
        &self,
        permit: &GraphTextWritePermit,
        page_name: &str,
    ) -> io::Result<bool> {
        struct PendingDirectory {
            directory: Dir,
            relative: String,
            depth: usize,
        }

        let limits = graph_text_inventory_limits();
        let root_depth = configured_root_components(&self.config().pages_dir)
            .ok_or_else(bad_path)?
            .len();
        if root_depth > limits.directory_depth {
            return Err(graph_text_inventory_limit_error("graph directory depth"));
        }
        let root = self.pages_path();
        // `graph_text_target` opens every configured root component from the
        // retained root capability and rejects a root-level pages symlink
        // before this scan can inspect an ambient pathname.
        let target = match self.graph_text_target(
            permit,
            &root.join(".tine-capability-legacy-page-scan"),
            false,
        ) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut all_entries = 0_usize;
        let mut page_files = 0_usize;
        let mut directory_count = 1_usize;
        if directory_count > limits.directories {
            return Err(graph_text_inventory_limit_error("directory count"));
        }
        let mut path_bytes = usize_to_u64(self.config().pages_dir.len())?;
        if path_bytes > limits.path_bytes {
            return Err(graph_text_inventory_limit_error("aggregate path bytes"));
        }
        let mut pending = Vec::new();
        if pending.len() >= limits.pending_directories {
            return Err(graph_text_inventory_limit_error("pending directories"));
        }
        pending.push(PendingDirectory {
            directory: target.parent().try_clone()?,
            relative: self.config().pages_dir.clone(),
            depth: root_depth,
        });

        while let Some(PendingDirectory {
            directory,
            relative,
            depth,
        }) = pending.pop()
        {
            for entry in directory.entries()? {
                all_entries = all_entries
                    .checked_add(1)
                    .ok_or_else(|| graph_text_inventory_limit_error("all directory entries"))?;
                if all_entries > limits.all_entries {
                    return Err(graph_text_inventory_limit_error("all directory entries"));
                }
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "legacy page scan entry name is not UTF-8",
                    )
                })?;
                let child_relative_len = relative
                    .len()
                    .checked_add(usize::from(!relative.is_empty()))
                    .and_then(|length| length.checked_add(name.len()))
                    .ok_or_else(|| graph_text_inventory_limit_error("aggregate path bytes"))?;
                path_bytes = path_bytes
                    .checked_add(usize_to_u64(child_relative_len)?)
                    .ok_or_else(|| graph_text_inventory_limit_error("aggregate path bytes"))?;
                if path_bytes > limits.path_bytes {
                    return Err(graph_text_inventory_limit_error("aggregate path bytes"));
                }
                let file_type = entry.file_type()?;
                if file_type.is_symlink() {
                    // Legacy behavior ignores linked files and directories; do
                    // not follow them while deciding a rescue collision.
                    continue;
                }
                if file_type.is_file() {
                    if !is_page_file(Path::new(name)) {
                        continue;
                    }
                    page_files = page_files
                        .checked_add(1)
                        .ok_or_else(|| graph_text_inventory_limit_error("graph file count"))?;
                    if page_files > limits.graph_text_files {
                        return Err(graph_text_inventory_limit_error("graph file count"));
                    }
                    let Some(stem) = Path::new(name).file_stem().and_then(|stem| stem.to_str())
                    else {
                        continue;
                    };
                    if crate::refs::same_page(
                        &decode_page_name(stem, self.config().file_name_format),
                        page_name,
                    ) {
                        return Ok(true);
                    }
                    continue;
                }
                if !file_type.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("legacy page scan entry is not a regular file: {relative}/{name}"),
                    ));
                }
                if name.starts_with('.') {
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
                // Both checks are required: metadata prevents a changed entry
                // from being treated as a directory and the platform no-follow
                // open closes the check/open race.
                projection_real_directory(&directory, name)?;
                let child = open_projection_dir_nofollow(&directory, name)?;
                if pending.len() >= limits.pending_directories {
                    return Err(graph_text_inventory_limit_error("pending directories"));
                }
                pending.push(PendingDirectory {
                    directory: child,
                    relative: format!("{relative}/{name}"),
                    depth: child_depth,
                });
            }
        }
        Ok(false)
    }

    fn graph_text_inventory(
        &self,
        permit: &GraphTextWritePermit,
    ) -> io::Result<(
        Vec<PageEntry>,
        std::collections::HashMap<PathBuf, ContentDigest>,
        Vec<String>,
    )> {
        let (entries, _, identities, skipped) = self.text_entries_with_limits_and_budget(
            permit,
            false,
            graph_text_inventory_limits(),
            None,
            vec![("", 0)],
            true,
        )?;
        Ok((entries, identities, skipped))
    }
}
