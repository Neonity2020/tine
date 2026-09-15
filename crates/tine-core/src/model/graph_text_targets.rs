//! Reading graph text and validating a write target: optional reads, target
//! validation, portable-alias and collision checks, Direct creation evidence
//! and proof, effective identity, and watcher identity failures.

use super::*;

impl Graph {
    fn graph_text_read_optional(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<Option<Vec<u8>>> {
        let target = match self.graph_text_target(permit, path, false) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        read_projection_optional(target.parent(), &target.filename)
    }

    pub(super) fn graph_text_read_optional_text(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<Option<String>> {
        self.graph_text_read_optional(permit, path)?
            .map(|bytes| {
                String::from_utf8(bytes).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "graph text file is not valid UTF-8",
                    )
                })
            })
            .transpose()
    }

    pub(super) fn graph_text_read_optional_text_with_identity(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<Option<(String, ContentDigest)>> {
        let target = match self.graph_text_target(permit, path, false) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let (file, bytes) =
            match open_and_read_projection_regular(target.parent(), &target.filename) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
        #[cfg(test)]
        GRAPH_TEXT_CONTENT_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
        let identity = canonical_projection_file_resource_id(&file)?;
        let text = String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "graph text file is not valid UTF-8",
            )
        })?;
        Ok(Some((text, identity)))
    }

    /// One-open coherent snapshot for a conflict authority decision. Unlike an
    /// ordinary read, this also performs the hard-refusal admission checks that
    /// must never mint override authority (portable alias and multiple links).
    pub(super) fn graph_text_read_optional_editor_conflict_snapshot(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<Option<(String, ContentDigest)>> {
        let graph_text_path = GraphTextPath::parse(self.rel_path(path)).map_err(|error| {
            DirectSaveError::into_io(
                DirectSaveFailureCode::PrecheckNotPortable,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("guarded graph-text target is not portable: {error}"),
                ),
            )
        })?;
        let target = match self.graph_text_target(permit, path, false) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let (file, bytes) =
            match open_and_read_projection_regular(target.parent(), &target.filename) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
        validate_graph_text_single_link(&file, graph_text_path.as_str())?;
        #[cfg(test)]
        GRAPH_TEXT_CONTENT_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
        let identity = canonical_projection_file_resource_id(&file)?;
        let text = String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "graph text file is not valid UTF-8",
            )
        })?;
        Ok(Some((text, identity)))
    }

    pub(super) fn graph_text_optional_file_identity(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<Option<ContentDigest>> {
        let target = match self.graph_text_target(permit, path, false) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        match open_projection_file_nofollow(target.parent(), &target.filename) {
            Ok(file) => canonical_projection_file_resource_id(&file).map(Some),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Resolve one exact existing document through its retained graph-relative
    /// path. Logical duplicates are deliberately readable: exact-path recovery
    /// must not depend on unrelated page-name uniqueness.
    pub(super) fn load_validated_graph_text_target(
        &self,
        permit: &GraphTextWritePermit,
        target: &Path,
    ) -> io::Result<Option<ExactGraphLoadedPage>> {
        let Some(entry) = self.graph_inventory_entry(target)? else {
            return Ok(None);
        };
        let Some((content, file_identity)) =
            self.graph_text_read_optional_text_with_identity(permit, target)?
        else {
            return Ok(None);
        };
        #[cfg(test)]
        GRAPH_TEXT_VALIDATION_TARGET_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
        if usize_to_u64(content.len())? > graph_text_inventory_limits().retained_content_bytes {
            return Err(graph_text_inventory_limit_error("aggregate text bytes"));
        }
        let (entry, document, revision) = parse_exact_page(self, &entry, &content)?;
        Ok(Some(ExactGraphLoadedPage {
            entry,
            document,
            content,
            revision,
            file_identity,
        }))
    }

    /// Prove the exact-file properties needed before the final mutation
    /// boundary. Portable-path proof is deliberately separate: initial
    /// validation must not enumerate a large retained parent twice per save.
    fn validate_existing_graph_text_target_local(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        expected_identity: ContentDigest,
    ) -> io::Result<()> {
        let target = self.graph_text_target(permit, path, false)?;
        let graph_text_path = GraphTextPath::parse(self.rel_path(path)).map_err(|error| {
            DirectSaveError::into_io(
                DirectSaveFailureCode::PrecheckNotPortable,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("guarded graph-text target is not portable: {error}"),
                ),
            )
        })?;
        self.validate_existing_graph_text_target_exact(
            &target,
            &graph_text_path,
            Some(expected_identity),
        )?;
        Ok(())
    }

    pub(super) fn validate_existing_graph_text_target_exact(
        &self,
        target: &GraphTextTarget,
        graph_text_path: &GraphTextPath,
        expected_identity: Option<ContentDigest>,
    ) -> io::Result<ContentDigest> {
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let file = open_projection_file_nofollow(target.parent(), &target.filename)?;
        let identity = canonical_projection_file_resource_id(&file)?;
        if expected_identity.is_some_and(|expected| expected != identity) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "graph text target changed at the local identity validation boundary",
            ));
        }
        validate_graph_text_single_link(&file, graph_text_path.as_str())?;
        Ok(identity)
    }

    /// Starting from the retained graph root, traverse only directory spellings
    /// whose single-component portable identity matches the requested path.
    /// This discovers case/NFC aliases in any ancestor without admitting an
    /// unrelated subtree or reading graph-text bytes.
    pub(super) fn validate_graph_text_portable_aliases_path_local(
        &self,
        permit: &GraphTextWritePermit,
        graph_text_path: &GraphTextPath,
        strict_creation: bool,
    ) -> io::Result<()> {
        #[cfg(test)]
        GRAPH_TEXT_PORTABLE_TRAVERSALS.with(|count| count.set(count.get().saturating_add(1)));

        struct PortablePrefix {
            directory: Dir,
            relative: String,
        }

        let limits = graph_text_inventory_limits();
        let components = graph_text_path.as_str().split('/').collect::<Vec<_>>();
        if components.len().saturating_sub(1) > limits.directory_depth {
            return Err(graph_text_inventory_limit_error("graph directory depth"));
        }
        let mut prefixes = vec![PortablePrefix {
            directory: self.graph_text_permit_root(permit)?.try_clone()?,
            relative: String::new(),
        }];
        let mut all_entries = 0_usize;
        let mut directory_count = 1_usize;
        let mut path_bytes = 0_u64;

        for (component_index, requested_component) in components.iter().enumerate() {
            // Hoisted out of the entry loop: for a non-ASCII component the fast
            // path never fires, and refolding the same component once per
            // directory entry is slower than the code this replaced.
            let requested_probe = PortablePathKey::graph_text_component_probe(requested_component);
            let is_filename = component_index + 1 == components.len();
            let requested_relative = components[..=component_index].join("/");
            let mut next = Vec::new();

            for prefix in prefixes {
                for entry in prefix.directory.entries()? {
                    all_entries = all_entries
                        .checked_add(1)
                        .ok_or_else(|| graph_text_inventory_limit_error("all directory entries"))?;
                    if all_entries > limits.all_entries {
                        return Err(graph_text_inventory_limit_error("all directory entries"));
                    }
                    let entry = entry?;
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else {
                        // GraphTextPath is UTF-8 by contract, so this entry cannot
                        // share the requested portable component identity.
                        continue;
                    };
                    let relative_len = prefix
                        .relative
                        .len()
                        .checked_add(usize::from(!prefix.relative.is_empty()))
                        .and_then(|length| length.checked_add(name.len()))
                        .ok_or_else(|| graph_text_inventory_limit_error("aggregate path bytes"))?;
                    path_bytes = path_bytes
                        .checked_add(usize_to_u64(relative_len)?)
                        .ok_or_else(|| graph_text_inventory_limit_error("aggregate path bytes"))?;
                    if path_bytes > limits.path_bytes {
                        return Err(graph_text_inventory_limit_error("aggregate path bytes"));
                    }
                    if !PortablePathKey::graph_text_component_matches(
                        name,
                        requested_component,
                        requested_probe.as_ref(),
                    ) {
                        continue;
                    }
                    let relative = if prefix.relative.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{}/{name}", prefix.relative)
                    };
                    let file_type = entry.file_type()?;
                    if file_type.is_symlink() {
                        if strict_creation {
                            return Err(DirectSaveError::into_io(
                                DirectSaveFailureCode::PrecheckNofollow,
                                io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    "projection path has no retained no-follow directory or file",
                                ),
                            ));
                        }
                        continue;
                    }

                    if is_filename {
                        if relative == graph_text_path.as_str()
                            || !file_type.is_file()
                            || !self.graph_text_scope.is_eligible(&relative)
                        {
                            continue;
                        }
                        projection_optional_regular_metadata(&prefix.directory, name)?;
                        match open_projection_file_nofollow(&prefix.directory, name) {
                            Ok(_) => {
                                return Err(DirectSaveError::into_io(
                                    DirectSaveFailureCode::PrecheckPortableCollision,
                                    io::Error::new(
                                        io::ErrorKind::AlreadyExists,
                                        format!(
                                            "graph text paths share one portable case/NFC identity: {relative} and {}",
                                            graph_text_path.as_str()
                                        ),
                                    ),
                                ));
                            }
                            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                            Err(error) => return Err(error),
                        }
                    }

                    if !file_type.is_dir() || !self.graph_text_scope.should_descend(&relative) {
                        continue;
                    }
                    directory_count = directory_count
                        .checked_add(1)
                        .ok_or_else(|| graph_text_inventory_limit_error("directory count"))?;
                    if directory_count > limits.directories {
                        return Err(graph_text_inventory_limit_error("directory count"));
                    }
                    if strict_creation && relative != requested_relative {
                        projection_real_directory(&prefix.directory, name)?;
                        let _alias = open_projection_dir_nofollow(&prefix.directory, name)?;
                        return Err(DirectSaveError::into_io(
                            DirectSaveFailureCode::PrecheckPortableCollision,
                            io::Error::new(
                                io::ErrorKind::AlreadyExists,
                                format!(
                                    "graph text paths share one portable case/NFC identity: {relative} and {requested_relative}"
                                ),
                            ),
                        ));
                    }
                    if next.len() == limits.pending_directories {
                        return Err(graph_text_inventory_limit_error("pending directories"));
                    }
                    projection_real_directory(&prefix.directory, name)?;
                    next.push(PortablePrefix {
                        directory: open_projection_dir_nofollow(&prefix.directory, name)?,
                        relative,
                    });
                }
            }
            if is_filename {
                return Ok(());
            }
            prefixes = next;
        }
        Ok(())
    }

    /// Apply strict current graph-scope collision policy to editor/name-only
    /// mutation. Portable aliases remain readable, but an editor mutation
    /// cannot choose one without authenticated exact logical authority.
    pub(super) fn validate_current_graph_text_collision_strict(
        &self,
        _permit: &GraphTextWritePermit,
        target: &Path,
        target_identity: Option<ContentDigest>,
    ) -> io::Result<Arc<CompleteGraphTextAdmissionIndex>> {
        let target_relative = self.rel_path(target);
        let target_path = GraphTextPath::parse(target_relative.clone()).map_err(|error| {
            DirectSaveError::into_io(
                DirectSaveFailureCode::PrecheckNotPortable,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("guarded graph-text target is not portable: {error}"),
                ),
            )
        })?;
        let index = self.guarded_graph_text_identity_index()?;
        if let Some(sibling) = index
            .paths_by_portable_key
            .get(&target_path.portable_key())
            .and_then(|members| members.iter().find(|member| *member != &target_path))
        {
            return Err(DirectSaveError::into_io(
                DirectSaveFailureCode::PrecheckPortableCollision,
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "graph text paths share one portable case/NFC identity: {} and {target_relative}",
                        sibling.as_str()
                    ),
                ),
            ));
        }
        if let Some(identity) = target_identity {
            if let Some(sibling) = index
                .paths_by_file_resource
                .get(&identity)
                .and_then(|members| {
                    members
                        .iter()
                        .find(|member| member.as_str() != target_relative)
                })
            {
                return Err(DirectSaveError::into_io(
                    DirectSaveFailureCode::PrecheckResourceAlias,
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "graph text files alias one physical resource: {} and {target_relative}",
                            sibling
                        ),
                    ),
                ));
            }
        }
        Ok(index)
    }

    /// Read the parsed ownership evidence once. A clean missing page cache is
    /// repairable; failure-bearing cold evidence and partial or incoherent warm
    /// publication remain hard refusals rather than authority to rebuild around
    /// an unexplained gap.
    pub(super) fn direct_creation_evidence(&self) -> io::Result<DirectCreationEvidence> {
        if self.graph_text_external_observation_pending() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "external graph-text changes are awaiting watcher reconciliation",
            ));
        }
        let cache = self.cache.read().unwrap();
        let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        let Some(_pages) = cache.as_ref() else {
            let published_failures = !self.page_index_failures.read().unwrap().is_empty();
            let retained_failures = self
                .effective_identity_index
                .read()
                .unwrap()
                .as_ref()
                .is_some_and(|index| !index.failures.is_empty());
            if published_failures || retained_failures {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "failure-bearing cold identity evidence cannot authorize name-only creation",
                ));
            }
            return Ok(DirectCreationEvidence::Cold);
        };
        let identity_index = self
            .effective_identity_index
            .read()
            .unwrap()
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "graph has unknown effective identities for name-only creation",
                )
            })?;
        if identity_index.generation() != generation {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "parsed identity evidence is not one coherent generation",
            ));
        }
        if !identity_index.failures.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "effective page identity is incomplete for name-only creation: {} unreadable or unparseable graph document(s)",
                    identity_index.failures.len()
                ),
            ));
        }
        Ok(DirectCreationEvidence::Warm {
            generation,
            identity_index,
        })
    }

    /// Bind creation to one coherent warm semantic-ownership generation. Cold
    /// evidence may own or join exactly one cache-build flight; the second read
    /// must be warm, and there is never a repair retry. Publication itself is
    /// target-local and no-replace; ordinary creation never hashes the graph.
    pub(super) fn direct_creation_proof(
        &self,
        permit: &GraphTextWritePermit,
        target: &Path,
        kind: PageKind,
        name: &str,
    ) -> io::Result<(DirectCreationProof, bool)> {
        let evidence = match self.direct_creation_evidence()? {
            DirectCreationEvidence::Warm {
                generation,
                identity_index,
            } => DirectCreationEvidence::Warm {
                generation,
                identity_index,
            },
            DirectCreationEvidence::Cold => {
                let outcome = self.repair_page_cache_once(permit);
                if !outcome.installed() {
                    return Err(outcome.creation_error());
                }
                self.direct_creation_evidence()?
            }
        };
        let DirectCreationEvidence::Warm {
            generation,
            identity_index,
        } = evidence
        else {
            return Err(PageBuildOutcome::Failed.creation_error());
        };

        let target = GraphTextPath::parse(self.rel_path(target)).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("guarded graph-text target is not portable: {error}"),
            )
        })?;
        if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) != generation {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "effective page identity evidence changed during creation validation",
            ));
        }
        let requested_identity_elsewhere = identity_index
            .owners
            .get(&page_cache_key(kind, name))
            .is_some_and(|owners| !owners.is_empty());
        Ok((
            DirectCreationProof { target, generation },
            requested_identity_elsewhere,
        ))
    }

    fn current_effective_identity_index(&self) -> io::Result<Arc<EffectiveIdentityIndex>> {
        loop {
            let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
            if let Some(index) = self.effective_identity_index.read().unwrap().as_ref() {
                if index.generation() == generation {
                    return Ok(Arc::clone(index));
                }
            }
            let pages = self
                .cache
                .read()
                .unwrap()
                .as_ref()
                .map(Arc::clone)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "effective page identities are not warm for name-only creation",
                    )
                })?;
            let failures = self.page_index_failures.read().unwrap().clone();
            let built = Arc::new(build_effective_identity_index(
                generation,
                pages.as_slice(),
                failures,
            ));
            if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) != generation {
                continue;
            }
            *self.effective_identity_index.write().unwrap() = Some(Arc::clone(&built));
            return Ok(built);
        }
    }

    pub(super) fn validate_name_only_effective_identity(
        &self,
        current_entries: &[PageEntry],
        kind: PageKind,
        name: &str,
    ) -> io::Result<bool> {
        let index = if self.cache.read().unwrap().is_none() {
            let generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
            let failures = self.page_index_failures.read().unwrap().clone();
            let retained = self
                .effective_identity_index
                .read()
                .unwrap()
                .as_ref()
                .map(Arc::clone);
            if let Some(index) = retained {
                if index.generation() == generation {
                    index
                } else if !current_entries.is_empty() || !failures.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "cold graph has stale effective identities; name-only creation requires warm evidence",
                    ));
                } else {
                    let index = Arc::new(EffectiveIdentityIndex {
                        generation: std::sync::atomic::AtomicU64::new(generation),
                        owners: std::collections::HashMap::new(),
                        physical_paths: std::collections::HashSet::new(),
                        failures: Vec::new(),
                    });
                    *self.effective_identity_index.write().unwrap() = Some(Arc::clone(&index));
                    index
                }
            } else if !current_entries.is_empty() || !failures.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "cold graph has unknown effective identities; name-only creation requires warm evidence",
                ));
            } else {
                let index = Arc::new(EffectiveIdentityIndex {
                    generation: std::sync::atomic::AtomicU64::new(generation),
                    owners: std::collections::HashMap::new(),
                    physical_paths: std::collections::HashSet::new(),
                    failures: Vec::new(),
                });
                *self.effective_identity_index.write().unwrap() = Some(Arc::clone(&index));
                index
            }
        } else {
            self.current_effective_identity_index()?
        };
        if !index.failures.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "effective page identity is incomplete for name-only creation: {} unreadable or unparseable graph document(s)",
                    index.failures.len()
                ),
            ));
        }
        if self.cache_gen.load(std::sync::atomic::Ordering::Acquire) != index.generation() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "effective page identity evidence changed during name-only creation",
            ));
        }
        let current_paths = current_entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<std::collections::HashSet<_>>();
        if current_paths != index.physical_paths {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "effective page identity evidence is stale or incomplete for name-only creation",
            ));
        }
        Ok(index
            .owners
            .get(&page_cache_key(kind, name))
            .is_some_and(|owners| !owners.is_empty()))
    }

    pub(super) fn advance_effective_identity_after_upsert(
        &self,
        generation: u64,
        entry: &PageEntry,
        failures: Vec<String>,
    ) {
        let mut guard = self.effective_identity_index.write().unwrap();
        let Some(current) = guard.as_ref() else {
            return;
        };
        if current.generation().checked_add(1) != Some(generation) {
            *guard = None;
            return;
        }
        let mut next = (**current).clone();
        next.generation
            .store(generation, std::sync::atomic::Ordering::Release);
        next.physical_paths.insert(entry.path.clone());
        for owners in next.owners.values_mut() {
            owners.retain(|owner| owner.path != entry.path);
        }
        next.owners.retain(|_, owners| !owners.is_empty());
        next.owners
            .entry(page_cache_key(entry.kind, &entry.name))
            .or_default()
            .push(entry.clone());
        next.failures = failures;
        *guard = Some(Arc::new(next));
    }

    pub(super) fn record_watcher_identity_failure(&self, path: &Path) {
        let failure = self.rel_path(path);
        let cache = self.cache.write().unwrap();
        let mut failures_guard = self.page_index_failures.write().unwrap();
        let mut failures = failures_guard.clone();
        if !failures.iter().any(|candidate| candidate == &failure) {
            failures.push(failure);
            failures.sort();
            failures.dedup();
        }
        let generation = self
            .cache_gen
            .fetch_add(1, std::sync::atomic::Ordering::Release)
            + 1;
        let next = match cache.as_ref() {
            Some(pages) => Some(Arc::new(build_effective_identity_index(
                generation,
                pages,
                failures.clone(),
            ))),
            None => {
                let retained = self.effective_identity_index.read().unwrap().clone();
                Some(Arc::new(retained.map_or_else(
                    || EffectiveIdentityIndex {
                        generation: std::sync::atomic::AtomicU64::new(generation),
                        owners: std::collections::HashMap::new(),
                        physical_paths: std::iter::once(path.to_path_buf()).collect(),
                        failures: failures.clone(),
                    },
                    |current| {
                        let mut next = (*current).clone();
                        next.generation
                            .store(generation, std::sync::atomic::Ordering::Release);
                        next.physical_paths.insert(path.to_path_buf());
                        next.failures = failures.clone();
                        next
                    },
                )))
            }
        };
        *self.effective_identity_index.write().unwrap() = next;
        *failures_guard = failures;
        drop(failures_guard);
        drop(cache);
        *self.page_list_cache.write().unwrap() = None;
        *self.find_entry_cache.write().unwrap() = None;
        *self.derived_cache.write().unwrap() = None;
    }

    pub(super) fn clear_watcher_identity_failure_after_reconciliation(&self, entry: &PageEntry) {
        let cache = self.cache.write().unwrap();
        let mut failures_guard = self.page_index_failures.write().unwrap();
        if !failures_guard
            .iter()
            .any(|failure| failure == &entry.rel_path)
        {
            return;
        }
        let mut failures = failures_guard.clone();
        failures.retain(|failure| failure != &entry.rel_path);
        let generation = self
            .cache_gen
            .fetch_add(1, std::sync::atomic::Ordering::Release)
            + 1;
        let next = match cache.as_ref() {
            Some(pages) => Arc::new(build_effective_identity_index(
                generation,
                pages,
                failures.clone(),
            )),
            None => {
                let retained = self.effective_identity_index.read().unwrap().clone();
                let mut next =
                    retained
                        .as_deref()
                        .cloned()
                        .unwrap_or_else(|| EffectiveIdentityIndex {
                            generation: std::sync::atomic::AtomicU64::new(generation),
                            owners: std::collections::HashMap::new(),
                            physical_paths: std::collections::HashSet::new(),
                            failures: Vec::new(),
                        });
                next.generation
                    .store(generation, std::sync::atomic::Ordering::Release);
                next.physical_paths.insert(entry.path.clone());
                for owners in next.owners.values_mut() {
                    owners.retain(|owner| owner.path != entry.path);
                }
                next.owners.retain(|_, owners| !owners.is_empty());
                next.owners
                    .entry(page_cache_key(entry.kind, &entry.name))
                    .or_default()
                    .push(entry.clone());
                next.failures = failures.clone();
                Arc::new(next)
            }
        };
        *self.effective_identity_index.write().unwrap() = Some(next);
        *failures_guard = failures;
        drop(failures_guard);
        drop(cache);
        *self.derived_cache.write().unwrap() = None;
    }

    /// Validate mutation authority independently from discovery/read authority.
    /// Existing semantic duplicates may be edited only through their captured
    /// exact physical owner. Portable path aliases and same-file aliases remain
    /// readable but every member is non-writable.
    pub(super) fn validate_graph_text_target(
        &self,
        permit: &GraphTextWritePermit,
        target: &Path,
        requested_identity: Option<(PageKind, &str)>,
    ) -> io::Result<ExactGraphValidation> {
        let loaded_target = self.load_validated_graph_text_target(permit, target)?;
        if let Some(loaded) = loaded_target.as_ref() {
            self.validate_existing_graph_text_target_local(permit, target, loaded.file_identity)?;
            return Ok(ExactGraphValidation {
                target: loaded_target,
                requested_identity_elsewhere: false,
                creation_proof: None,
            });
        }
        if let Some((kind, name)) = requested_identity {
            let (creation_proof, requested_identity_elsewhere) =
                self.direct_creation_proof(permit, target, kind, name)?;
            return Ok(ExactGraphValidation {
                target: None,
                requested_identity_elsewhere,
                creation_proof: Some(creation_proof),
            });
        }
        let index = self.validate_current_graph_text_collision_strict(permit, target, None)?;
        let requested_identity_elsewhere = match (loaded_target.as_ref(), requested_identity) {
            (None, Some((kind, name))) => {
                let retained_collision = index
                    .paths_by_semantic_key
                    .get(&(
                        match kind {
                            PageKind::Page => 0,
                            PageKind::Journal => 1,
                        },
                        crate::refs::page_key(name),
                    ))
                    .is_some_and(|members| !members.is_empty());
                let entries = index
                    .files_by_exact_path
                    .iter()
                    .map(|(_, record)| record.semantic.clone())
                    .collect::<Vec<_>>();
                retained_collision
                    || self.validate_name_only_effective_identity(&entries, kind, name)?
            }
            _ => false,
        };
        Ok(ExactGraphValidation {
            target: loaded_target,
            requested_identity_elsewhere,
            creation_proof: None,
        })
    }

    pub(super) fn graph_text_read_to_string(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<String> {
        self.graph_text_read_optional_text(permit, path)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    pub(super) fn graph_text_content_rev_matches(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        expected: &str,
    ) -> io::Result<bool> {
        if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid graph content revision",
            ));
        }
        let target = self.graph_text_target(permit, path, false)?;
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let mut file = open_projection_file_nofollow(target.parent(), &target.filename)?;
        let mut hash = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(read as u64)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "read-byte overflow"))?;
            if total > MAX_PROJECTION_EVIDENCE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph revision evidence exceeds the reload bound",
                ));
            }
            hash.update(&buffer[..read]);
        }
        Ok(format!("{:x}", hash.finalize()) == expected)
    }

    pub(super) fn graph_text_file_equals_bytes(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        expected: &[u8],
    ) -> io::Result<bool> {
        let target = self.graph_text_target(permit, path, false)?;
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let mut file = open_projection_file_nofollow(target.parent(), &target.filename)?;
        if file.metadata()?.len() != expected.len() as u64 {
            return Ok(false);
        }
        let mut offset = 0usize;
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                return Ok(offset == expected.len());
            }
            let end = offset
                .checked_add(read)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "read-byte overflow"))?;
            if expected.get(offset..end) != Some(&buffer[..read]) {
                return Ok(false);
            }
            offset = end;
        }
    }

    pub(super) fn graph_text_read_to_string_with_budget(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        budget: &RetainedContentBudget,
        resource: &'static str,
    ) -> io::Result<BudgetedString> {
        let target = self.graph_text_target(permit, path, false)?;
        projection_optional_regular_metadata(target.parent(), &target.filename)?;
        let (_file, bytes, mut reservation) = open_and_read_projection_regular_with_budget(
            target.parent(),
            &target.filename,
            MAX_PROJECTION_EVIDENCE_BYTES,
            budget,
            resource,
        )?;
        let value = String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "graph text file is not valid UTF-8",
            )
        })?;
        reservation.resize(usize_to_u64(value.capacity())?, resource)?;
        Ok(BudgetedString { value, reservation })
    }

    pub(super) fn graph_text_exists(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<bool> {
        let target = match self.graph_text_target(permit, path, false) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match target.parent().symlink_metadata(&target.filename) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
            Ok(_) => {
                projection_optional_regular_metadata(target.parent(), &target.filename)?;
                Ok(true)
            }
        }
    }
}
