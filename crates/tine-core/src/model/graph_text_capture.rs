//! Capturing the graph text and building the admission index: the bounded
//! capture walk, its peak and growth charges, and every upper bound the
//! admission budget is checked against.

use super::*;

#[cfg(not(test))]
pub(super) fn graph_text_inventory_limits() -> GraphTextInventoryLimits {
    GRAPH_TEXT_INVENTORY_LIMITS
}

#[cfg(test)]
pub(super) fn graph_text_inventory_limits() -> GraphTextInventoryLimits {
    GRAPH_TEXT_INVENTORY_LIMITS_OVERRIDE.with(|override_limits| {
        override_limits
            .borrow()
            .unwrap_or(GRAPH_TEXT_INVENTORY_LIMITS)
    })
}

pub(super) struct GraphTextCaptureEntry {
    path: GraphTextPath,
    bytes: Option<Vec<u8>>,
    description: BlobDescription,
    file_resource_id: ContentDigest,
    link_count: u64,
}

pub(super) struct GraphTextCapture {
    pub(super) entries: Vec<GraphTextCaptureEntry>,
    directories_by_exact_relative: std::collections::BTreeMap<String, ContentDigest>,
    paths_by_file_resource:
        std::collections::BTreeMap<ContentDigest, std::collections::BTreeSet<String>>,
    file_link_count_by_exact_relative: std::collections::BTreeMap<String, u64>,
    all_entries: u64,
    raw_bytes: u64,
    pub(super) peak_build_charge: u64,
}

pub(super) fn collect_graph_text_capture_inner(
    graph: &Graph,
    _permit: &GraphTextWritePermit,
    retain_bytes: bool,
    limits: GraphTextCaptureLimits,
    simultaneous_capture_bytes: u64,
    require_ambient_binding: bool,
    skip_symlinks: bool,
) -> io::Result<GraphTextCapture> {
    struct PendingDirectory {
        directory: Dir,
        relative: String,
        depth: usize,
    }

    let mut entries = Vec::new();
    let mut raw_bytes = 0_u64;
    let mut all_entries = 0_usize;
    let mut directory_count = 1_usize;
    let mut path_bytes = 0_u64;
    let mut peak_build_charge = graph_text_root_capture_upper_bound()?;
    let mut directories_by_exact_relative = std::collections::BTreeMap::new();
    let mut directory_resources = std::collections::BTreeMap::new();
    let mut paths_by_file_resource =
        std::collections::BTreeMap::<ContentDigest, std::collections::BTreeSet<String>>::new();
    let mut file_link_count_by_exact_relative = std::collections::BTreeMap::new();
    let mut pending = Vec::new();
    ensure_graph_text_peak_limit(
        simultaneous_capture_bytes,
        peak_build_charge,
        limits.peak_build_bytes,
    )?;
    if directory_count > limits.directories {
        return Err(graph_text_capture_limit_error("directory count"));
    }
    if require_ambient_binding {
        graph.ensure_projection_root_binding()?;
    }
    let directory = if require_ambient_binding {
        graph
            .projection_root
            .as_ref()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "graph has no retained no-follow projection capability",
                )
            })?
            .try_clone()?
    } else {
        graph.graph_text_write_binding()?.root.try_clone()?
    };
    let root_resource = canonical_projection_directory_resource_id(&directory)?;
    directories_by_exact_relative.insert(String::new(), root_resource);
    directory_resources.insert(root_resource, String::new());
    if pending.len() == limits.pending_directories {
        return Err(graph_text_capture_limit_error("pending directories"));
    }
    pending.push(PendingDirectory {
        directory,
        relative: String::new(),
        depth: 0,
    });

    while let Some(PendingDirectory {
        directory,
        relative,
        depth,
    }) = pending.pop()
    {
        count_graph_text_admission_builder_enumeration();
        for entry in directory.entries()? {
            all_entries = all_entries
                .checked_add(1)
                .ok_or_else(|| graph_text_capture_limit_error("all directory entries"))?;
            if all_entries > limits.all_entries {
                return Err(graph_text_capture_limit_error("all directory entries"));
            }
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph text entry name is not UTF-8",
                )
            })?;
            let relative_len = relative
                .len()
                .checked_add(usize::from(!relative.is_empty()))
                .and_then(|length| length.checked_add(name.len()))
                .ok_or_else(allocation_overflow)?;
            path_bytes = path_bytes
                .checked_add(
                    usize_to_u64(relative_len)
                        .map_err(|_| graph_text_capture_limit_error("aggregate path bytes"))?,
                )
                .ok_or_else(|| graph_text_capture_limit_error("aggregate path bytes"))?;
            if path_bytes > limits.path_bytes {
                return Err(graph_text_capture_limit_error("aggregate path bytes"));
            }
            grow_graph_text_capture_charge(
                &mut peak_build_charge,
                graph_text_discovered_path_upper_bound(usize_to_u64(relative_len)?)?,
                simultaneous_capture_bytes,
                limits.peak_build_bytes,
            )?;
            let child_relative = if relative.is_empty() {
                name.to_owned()
            } else {
                format!("{relative}/{name}")
            };
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                if skip_symlinks
                    || (!graph.graph_text_scope.should_descend(&child_relative)
                        && !graph.graph_text_scope.is_eligible(&child_relative))
                {
                    // GH #267 / F3. A symlink anywhere in a descended scope used
                    // to abort this capture, and because the save path takes the
                    // capture to answer a filename question, that made the whole
                    // graph permanently unsaveable -- one symlink in `pages/`
                    // and none of the user's OTHER pages could be written.
                    //
                    // Skipping is what the rest of Tine already does: every
                    // traversal here is no-follow, the watcher's snapshot walk
                    // never descends a symlinked directory, and
                    // `graph_inventory_entry` never admits a symlinked file. A
                    // symlink is not a graph-text document on any other path, so
                    // the capture stops being the one place that escalates it to
                    // an error that costs the user their other pages.
                    //
                    // `skip_symlinks` is false for the shadow-import bootstrap,
                    // which still refuses: importing a graph must not silently
                    // leave out a file the user considers part of it.
                    continue;
                }
                return Err(DirectSaveError::into_io(
                    DirectSaveFailureCode::PrecheckSymlink,
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("graph text entry is a symlink or reparse point: {child_relative}"),
                    ),
                ));
            }
            if file_type.is_dir() {
                if !graph.graph_text_scope.should_descend(&child_relative) {
                    continue;
                }
                let child_depth = depth
                    .checked_add(1)
                    .ok_or_else(|| graph_text_capture_limit_error("graph directory depth"))?;
                if child_depth > limits.directory_depth {
                    return Err(graph_text_capture_limit_error("graph directory depth"));
                }
                directory_count = directory_count
                    .checked_add(1)
                    .ok_or_else(|| graph_text_capture_limit_error("directory count"))?;
                if directory_count > limits.directories {
                    return Err(graph_text_capture_limit_error("directory count"));
                }
                projection_real_directory(&directory, name)?;
                let child = open_projection_dir_nofollow(&directory, name)?;
                let resource = canonical_projection_directory_resource_id(&child)?;
                if let Some(first) = directory_resources.insert(resource, child_relative.clone()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "graph directories alias one resource: {first} and {child_relative}"
                        ),
                    ));
                }
                directories_by_exact_relative.insert(child_relative.clone(), resource);
                let rebound = open_projection_dir_nofollow(&directory, name)?;
                if projection_dir_identity(&child)? != projection_dir_identity(&rebound)? {
                    return Err(DirectSaveError::into_io(
                        DirectSaveFailureCode::PrecheckInterrupted,
                        io::Error::new(
                            io::ErrorKind::Interrupted,
                            format!("graph directory changed during capture: {child_relative}"),
                        ),
                    ));
                }
                if pending.len() == limits.pending_directories {
                    return Err(graph_text_capture_limit_error("pending directories"));
                }
                pending.push(PendingDirectory {
                    directory: child,
                    relative: child_relative,
                    depth: child_depth,
                });
                continue;
            }
            if !file_type.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("graph text entry is not a regular file: {child_relative}"),
                ));
            }
            let file = open_projection_file_nofollow(&directory, name)?;
            let file_resource = canonical_projection_file_resource_id(&file)?;
            let link_count = projection_file_link_count(&file)?;
            paths_by_file_resource
                .entry(file_resource)
                .or_default()
                .insert(child_relative.clone());
            file_link_count_by_exact_relative.insert(child_relative.clone(), link_count);
            if !graph.graph_text_scope.is_eligible(&child_relative) {
                continue;
            }
            if entries.len() == limits.graph_text_files {
                return Err(graph_text_capture_limit_error("graph file count"));
            }
            let path = GraphTextPath::parse(child_relative)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
            let remaining_raw = limits
                .raw_bytes
                .checked_sub(raw_bytes)
                .ok_or_else(|| graph_text_capture_limit_error("aggregate raw bytes"))?;
            let live_capture_bytes =
                checked_add_bytes(simultaneous_capture_bytes, peak_build_charge)?;
            let remaining_peak = limits
                .peak_build_bytes
                .checked_sub(live_capture_bytes)
                .ok_or_else(|| graph_text_capture_limit_error("peak build memory"))?;
            let (bytes, description, captured_resource, _, _) =
                read_projection_optional_bound_capture_with_limits(
                    &directory,
                    name,
                    remaining_raw,
                    remaining_peak,
                )?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Interrupted,
                        format!("graph entry disappeared during capture: {path}"),
                    )
                })?;
            if retain_bytes {
                grow_graph_text_capture_charge(
                    &mut peak_build_charge,
                    usize_to_u64(bytes.capacity())?,
                    simultaneous_capture_bytes,
                    limits.peak_build_bytes,
                )?;
            }
            if captured_resource != file_resource {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("graph entry changed after enumeration: {path}"),
                ));
            }
            raw_bytes = raw_bytes
                .checked_add(usize_to_u64(bytes.len())?)
                .ok_or_else(|| graph_text_capture_limit_error("aggregate raw bytes"))?;
            if raw_bytes > limits.raw_bytes {
                return Err(graph_text_capture_limit_error("aggregate raw bytes"));
            }
            entries.push(GraphTextCaptureEntry {
                path,
                description,
                bytes: retain_bytes.then_some(bytes),
                file_resource_id: captured_resource,
                link_count,
            });
        }
    }

    entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    if entries
        .windows(2)
        .any(|window| window[0].path == window[1].path)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "graph-text capture contains duplicate graph paths",
        ));
    }
    Ok(GraphTextCapture {
        entries,
        directories_by_exact_relative,
        paths_by_file_resource,
        file_link_count_by_exact_relative,
        all_entries: all_entries as u64,
        raw_bytes,
        peak_build_charge: {
            #[cfg(test)]
            {
                if retain_bytes {
                    GRAPH_TEXT_FIRST_CAPTURE_CHARGE_OVERRIDE
                        .with(|override_charge| override_charge.take())
                        .unwrap_or(peak_build_charge)
                } else {
                    peak_build_charge
                }
            }
            #[cfg(not(test))]
            {
                peak_build_charge
            }
        },
    })
}

pub(super) fn ensure_graph_text_peak_limit(
    base: u64,
    additional: u64,
    limit: u64,
) -> io::Result<()> {
    if checked_add_bytes(base, additional)? > limit {
        return Err(graph_text_capture_limit_error("peak build memory"));
    }
    Ok(())
}

fn grow_graph_text_capture_charge(
    charge: &mut u64,
    growth: u64,
    simultaneous_capture_bytes: u64,
    peak_limit: u64,
) -> io::Result<()> {
    let next = checked_add_bytes(*charge, growth)?;
    ensure_graph_text_peak_limit(simultaneous_capture_bytes, next, peak_limit)?;
    *charge = next;
    Ok(())
}

fn graph_text_discovered_path_upper_bound(relative_len: u64) -> io::Result<u64> {
    let owned_path = owned_string_len_upper_bound(relative_len)?;
    let mut bytes = checked_add_bytes(
        conservative_vec_entry_bytes::<GraphTextCaptureEntry>()?,
        checked_add_bytes(
            conservative_vec_entry_bytes::<(Dir, String, usize)>()?,
            owned_path,
        )?,
    )?;
    // Charge the maximum simultaneous directory and file bookkeeping for every
    // discovered name. This intentionally over-reserves rows that are used by
    // only one branch so no branch can allocate before admission.
    for row in [
        conservative_btree_entry_bytes::<String, ContentDigest>()?,
        conservative_btree_entry_bytes::<ContentDigest, String>()?,
        conservative_btree_entry_bytes::<ContentDigest, std::collections::BTreeSet<String>>()?,
        conservative_btree_entry_bytes::<String, ()>()?,
        conservative_btree_entry_bytes::<String, u64>()?,
    ] {
        bytes = checked_add_bytes(bytes, row)?;
        bytes = checked_add_bytes(bytes, owned_path)?;
    }
    // Exact path construction plus the transient child-relative string.
    bytes = checked_add_bytes(bytes, checked_mul_bytes(owned_path, 2)?)?;
    checked_add_bytes(bytes, 512)
}

fn graph_text_root_capture_upper_bound() -> io::Result<u64> {
    let empty = owned_string_len_upper_bound(0)?;
    let mut bytes = conservative_vec_entry_bytes::<(Dir, String, usize)>()?;
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<String, ContentDigest>()?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<ContentDigest, String>()?,
    )?;
    bytes = checked_add_bytes(bytes, checked_mul_bytes(empty, 3)?)?;
    checked_add_bytes(bytes, 512)
}

pub(super) fn graph_text_semantic_key(entry: &PageEntry) -> (u8, String) {
    let kind = match entry.kind {
        PageKind::Page => 0,
        PageKind::Journal => 1,
    };
    (kind, crate::refs::page_key(&entry.name))
}

pub(super) fn graph_text_captures_match(
    first: &GraphTextCapture,
    second: &GraphTextCapture,
) -> bool {
    first.directories_by_exact_relative == second.directories_by_exact_relative
        && first.paths_by_file_resource == second.paths_by_file_resource
        && first.file_link_count_by_exact_relative == second.file_link_count_by_exact_relative
        && first.all_entries == second.all_entries
        && first.raw_bytes == second.raw_bytes
        && first.entries.len() == second.entries.len()
        && first
            .entries
            .iter()
            .zip(&second.entries)
            .all(|(first, second)| {
                first.path == second.path
                    && first.description == second.description
                    && first.file_resource_id == second.file_resource_id
                    && first.link_count == second.link_count
            })
}

pub(super) fn build_graph_text_admission_index(
    graph: &Graph,
    capture: &GraphTextCapture,
    limits: GraphTextCaptureLimits,
    combined_capture_bytes: u64,
    decode_semantics: bool,
    prior: Option<&CompleteGraphTextAdmissionIndex>,
) -> io::Result<CompleteGraphTextAdmissionIndex> {
    let (scope_binding, graph_resource) = if decode_semantics {
        (
            graph.graph_text_scope_binding()?,
            graph.canonical_resource_id()?,
        )
    } else {
        let binding = graph.graph_text_write_binding()?;
        (
            graph
                .graph_text_scope
                .bind_graph_resource(binding.resource_id),
            binding.resource_id,
        )
    };
    if scope_binding.graph_resource_id() != graph_resource {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "graph-text scope binding does not match the retained graph resource",
        ));
    }
    let permanent_bytes =
        graph_text_initial_permanent_upper_bound(graph, capture, decode_semantics)?;
    if permanent_bytes > limits.permanent_index_bytes {
        return Err(graph_text_capture_limit_error("permanent index memory"));
    }
    let validation_scratch =
        graph_text_index_validation_scratch_upper_bound(graph, capture, decode_semantics)?;
    ensure_graph_text_peak_limit(
        combined_capture_bytes,
        checked_add_bytes(permanent_bytes, validation_scratch)?,
        limits.peak_build_bytes,
    )?;

    // All permanent rows are reserved above before the first map allocation.
    let mut file_resource_by_exact_relative = PersistentMap::default();
    let mut file_is_graph_text_by_exact_relative = PersistentMap::default();
    for (resource, paths) in &capture.paths_by_file_resource {
        for path in paths {
            count_graph_text_admission_index_map_insertion();
            file_resource_by_exact_relative.insert(path.clone(), *resource);
            count_graph_text_admission_index_map_insertion();
            file_is_graph_text_by_exact_relative.insert(path.clone(), false);
        }
    }
    let mut index = CompleteGraphTextAdmissionIndex {
        instance: Arc::clone(&graph.graph_text_admission_instance),
        scope_binding,
        graph_resource,
        generation: 1,
        files_by_exact_path: PersistentMap::default(),
        paths_by_portable_key: PersistentMap::default(),
        paths_by_file_resource: capture
            .paths_by_file_resource
            .iter()
            .map(|(key, value)| (*key, value.clone()))
            .collect(),
        file_resource_by_exact_relative,
        file_link_count_by_exact_relative: capture
            .file_link_count_by_exact_relative
            .iter()
            .map(|(key, value)| (key.clone(), *value))
            .collect(),
        file_is_graph_text_by_exact_relative,
        paths_by_semantic_key: PersistentMap::default(),
        tombstones_by_exact_path: PersistentMap::default(),
        directories_by_exact_relative: capture
            .directories_by_exact_relative
            .iter()
            .map(|(key, value)| (key.clone(), *value))
            .collect(),
        permanent_bytes,
        permanent_limit: limits.permanent_index_bytes,
        peak_limit: limits.peak_build_bytes,
    };
    // Collision groups are assembled mutably and sealed once. Updating a
    // persistent value for every member would repeatedly copy a growing set
    // before the boundary collision check.
    let mut portable_groups = std::collections::BTreeMap::new();
    let mut semantic_groups = std::collections::BTreeMap::new();
    let cached_semantics = if decode_semantics {
        std::collections::HashMap::new()
    } else {
        graph
            .cache
            .read()
            .unwrap()
            .as_ref()
            .map(|pages| {
                pages
                    .iter()
                    .map(|(entry, _)| (entry.rel_path.clone(), entry.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };
    for entry in &capture.entries {
        let bytes = entry
            .bytes
            .as_deref()
            .expect("the first capture pass retains bytes");
        // THE cut (GH #267). A rebuild used to parse EVERY document in the graph
        // whenever the exact-observation chain had broken -- on every save, and
        // on Windows or a network share that was essentially always. But an
        // invalidation says "we lost track", not "everything changed": almost
        // every file still holds byte-for-byte the same content it held when we
        // last parsed it, and `description` (a SHA-256 of the content plus its
        // length) proves which. Reuse those, and parse only what actually moved.
        //
        // `semantic_parsed` is what makes the reuse sound: a record whose
        // semantic came from the page cache or from its filename is a guess, and
        // carrying it forward would let later builds treat it as parsed.
        let reused = decode_semantics
            .then(|| prior?.files_by_exact_path.get(&entry.path))
            .flatten()
            .filter(|record| record.semantic_parsed && record.description == entry.description);
        let (semantic, format) = if let Some(record) = reused {
            (record.semantic.clone(), record.format)
        } else if decode_semantics {
            let content = std::str::from_utf8(bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("graph text is not UTF-8: {}", entry.path),
                )
            })?;
            let permit = graph_text_parse_budget_permit(graph, &entry.path, content)?;
            let (semantic, format, node_count) =
                graph.decode_present_graph_text_with_node_count(&entry.path, bytes, permit)?;
            if node_count > MAX_GRAPH_TEXT_PARSER_NODES {
                return Err(graph_text_capture_limit_error("parser node count"));
            }
            (semantic, format)
        } else {
            (
                cached_semantics
                    .get(entry.path.as_str())
                    .cloned()
                    .map(Ok)
                    .unwrap_or_else(|| graph.graph_text_entry_for_graph_text_path(&entry.path))
                    .map_err(|error| {
                        io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                    })?,
                Format::from_path(Path::new(entry.path.as_str())),
            )
        };
        let record = GraphTextAdmissionRecord {
            description: entry.description,
            file_resource_id: entry.file_resource_id,
            link_count: entry.link_count,
            semantic,
            format,
            semantic_parsed: decode_semantics,
        };
        index
            .file_is_graph_text_by_exact_relative
            .insert(entry.path.as_str().to_owned(), true);
        count_graph_text_admission_index_map_insertion();
        initial_graph_text_collision_group_insert(
            &mut portable_groups,
            entry.path.portable_key(),
            entry.path.clone(),
        );
        count_graph_text_admission_index_map_insertion();
        initial_graph_text_collision_group_insert(
            &mut semantic_groups,
            graph_text_semantic_key(&record.semantic),
            entry.path.clone(),
        );
        count_graph_text_admission_index_map_insertion();
        if index
            .files_by_exact_path
            .insert(entry.path.clone(), record)
            .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph-text capture contains duplicate exact graph-text paths",
            ));
        }
    }
    index.paths_by_portable_key = portable_groups.into_iter().collect();
    index.paths_by_semantic_key = semantic_groups.into_iter().collect();
    validate_graph_text_admission_index(&index)?;
    Ok(index)
}

fn graph_text_initial_permanent_upper_bound(
    graph: &Graph,
    capture: &GraphTextCapture,
    decode_semantics: bool,
) -> io::Result<u64> {
    let title_format = graph_text_journal_title_format_budget(graph)?;
    let mut bytes = checked_add_bytes(
        usize_to_u64(std::mem::size_of::<CompleteGraphTextAdmissionIndex>())?,
        512,
    )?;
    bytes = checked_add_bytes(
        bytes,
        owned_string_len_upper_bound(title_format.input_bytes)?,
    )?;
    for relative in capture.directories_by_exact_relative.keys() {
        bytes = checked_add_bytes(
            bytes,
            graph_text_owned_btree_row_upper_bound::<String, ContentDigest>(usize_to_u64(
                relative.len(),
            )?)?,
        )?;
    }
    for paths in capture.paths_by_file_resource.values() {
        bytes = checked_add_bytes(
            bytes,
            conservative_btree_entry_bytes::<ContentDigest, std::collections::BTreeSet<String>>()?,
        )?;
        for path in paths {
            let path_len = usize_to_u64(path.len())?;
            bytes = checked_add_bytes(
                bytes,
                graph_text_owned_btree_row_upper_bound::<String, ()>(path_len)?,
            )?;
            bytes = checked_add_bytes(
                bytes,
                graph_text_owned_btree_row_upper_bound::<String, ContentDigest>(path_len)?,
            )?;
            bytes = checked_add_bytes(
                bytes,
                graph_text_owned_btree_row_upper_bound::<String, u64>(path_len)?,
            )?;
            bytes = checked_add_bytes(
                bytes,
                graph_text_owned_btree_row_upper_bound::<String, bool>(path_len)?,
            )?;
        }
    }
    for entry in &capture.entries {
        let content_bytes = entry.bytes.as_ref().expect("first capture retains bytes");
        let semantic_name_len = if decode_semantics {
            let content = std::str::from_utf8(content_bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "graph text is not UTF-8")
            })?;
            graph_text_observed_semantic_name_upper_bound(graph, &entry.path, content)?
                .semantic_name_bytes
        } else {
            guarded_graph_text_semantic_name_upper_bound(graph, &entry.path, content_bytes.len())?
        };
        bytes = checked_add_bytes(
            bytes,
            graph_text_file_record_worst_case_upper_bound(
                graph,
                usize_to_u64(entry.path.as_str().len())?,
                semantic_name_len,
            )?,
        )?;
    }
    Ok(bytes)
}

fn graph_text_index_validation_scratch_upper_bound(
    graph: &Graph,
    capture: &GraphTextCapture,
    decode_semantics: bool,
) -> io::Result<u64> {
    let mut largest_path = 0_u64;
    let mut largest_name = 0_u64;
    for entry in &capture.entries {
        let path = usize_to_u64(entry.path.as_str().len())?;
        let bytes = entry.bytes.as_ref().expect("first capture retains bytes");
        largest_path = largest_path.max(path);
        largest_name = largest_name.max(if decode_semantics {
            let content = std::str::from_utf8(bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "graph text is not UTF-8")
            })?;
            graph_text_observed_semantic_name_upper_bound(graph, &entry.path, content)?
                .semantic_name_bytes
        } else {
            guarded_graph_text_semantic_name_upper_bound(graph, &entry.path, bytes.len())?
        });
    }
    let mut bytes = checked_add_bytes(
        checked_add_bytes(
            owned_string_len_upper_bound(checked_mul_bytes(largest_path, 8)?)?,
            owned_string_len_upper_bound(checked_mul_bytes(largest_name, 8)?)?,
        )?,
        1024,
    )?;
    let all_files = capture.file_link_count_by_exact_relative.len();
    let graph_text_files = capture.entries.len();
    for structural in [
        persistent_map_build_path_peak_upper_bound::<GraphTextPath, GraphTextAdmissionRecord>(
            graph_text_files,
        )?,
        persistent_map_build_path_peak_upper_bound::<
            PortablePathKey,
            std::collections::BTreeSet<GraphTextPath>,
        >(graph_text_files)?,
        persistent_map_build_path_peak_upper_bound::<
            ContentDigest,
            std::collections::BTreeSet<String>,
        >(all_files)?,
        persistent_map_build_path_peak_upper_bound::<String, ContentDigest>(all_files)?,
        persistent_map_build_path_peak_upper_bound::<String, u64>(all_files)?,
        persistent_map_build_path_peak_upper_bound::<String, bool>(all_files)?,
        persistent_map_build_path_peak_upper_bound::<
            (u8, String),
            std::collections::BTreeSet<GraphTextPath>,
        >(graph_text_files)?,
    ] {
        bytes = checked_add_bytes(bytes, structural)?;
    }
    Ok(bytes)
}

fn persistent_map_build_path_peak_upper_bound<K, V>(entries: usize) -> io::Result<u64> {
    if entries == 0 {
        return Ok(0);
    }
    let binary_digits = u64::from(usize::BITS - entries.leading_zeros());
    let conservative_avl_depth = checked_mul_bytes(binary_digits, 2)?;
    let copied_nodes = checked_add_bytes(checked_mul_bytes(conservative_avl_depth, 5)?, 1)?;
    checked_mul_bytes(
        copied_nodes,
        usize_to_u64(std::mem::size_of::<PersistentMapNode<K, V>>())?,
    )
}

fn graph_text_owned_btree_row_upper_bound<K, V>(owned_len: u64) -> io::Result<u64> {
    checked_add_bytes(
        conservative_btree_entry_bytes::<K, V>()?,
        owned_string_len_upper_bound(owned_len)?,
    )
}

pub(super) const MAX_GRAPH_TEXT_SEMANTIC_NAME_BYTES: u64 = 256 * 1024;
const MAX_JOURNAL_TITLE_BYTES_PER_FORMAT_BYTE: u64 = 11;

#[derive(Clone, Copy)]
pub(super) struct GraphTextJournalTitleFormatBudget {
    input_bytes: u64,
    pub(super) rendered_bytes: u64,
}

#[derive(Clone, Copy)]
pub(super) struct GraphTextSemanticNameBudget {
    pub(super) semantic_name_bytes: u64,
}

pub(super) fn graph_text_journal_title_format_budget(
    graph: &Graph,
) -> io::Result<GraphTextJournalTitleFormatBudget> {
    let input_bytes = usize_to_u64(graph.journal_format.title_format().len())?;
    // `date::Format` consumes at least one ASCII pattern byte per token and no
    // token can render more than an i32 year (11 bytes). Literal UTF-8 bytes are
    // copied one-for-one, so this is a strict bound for every compiled pattern.
    let rendered_bytes = checked_mul_bytes(input_bytes, MAX_JOURNAL_TITLE_BYTES_PER_FORMAT_BYTE)?;
    if input_bytes > MAX_GRAPH_TEXT_SEMANTIC_NAME_BYTES
        || rendered_bytes > MAX_GRAPH_TEXT_SEMANTIC_NAME_BYTES
    {
        return Err(graph_text_capture_limit_error(
            "journal title format expansion",
        ));
    }
    Ok(GraphTextJournalTitleFormatBudget {
        input_bytes,
        rendered_bytes,
    })
}

pub(super) fn graph_text_observed_semantic_name_upper_bound(
    graph: &Graph,
    path: &GraphTextPath,
    content: &str,
) -> io::Result<GraphTextSemanticNameBudget> {
    let title_format = graph_text_journal_title_format_budget(graph)?;
    let mut observed = checked_add_bytes(usize_to_u64(path.as_str().len())?, 64)?;
    let format = Format::from_path(Path::new(path.as_str()));
    for line in content.lines() {
        let trimmed = line.trim();
        let title = line
            .split_once("::")
            .and_then(|(key, value)| key.trim().eq_ignore_ascii_case("title").then_some(value))
            .or_else(|| {
                (format == Format::Org).then_some(()).and_then(|()| {
                    trimmed
                        .split_once(':')
                        .and_then(|(key, value)| {
                            key.eq_ignore_ascii_case("#+title").then_some(value)
                        })
                        .or_else(|| {
                            trimmed.strip_prefix(':').and_then(|rest| {
                                rest.split_once(':').and_then(|(key, value)| {
                                    key.eq_ignore_ascii_case("title").then_some(value)
                                })
                            })
                        })
                })
            });
        if let Some(title) = title {
            observed = observed.max(checked_add_bytes(usize_to_u64(title.trim().len())?, 64)?);
        }
    }
    observed = observed.max(title_format.rendered_bytes);
    if observed > MAX_GRAPH_TEXT_SEMANTIC_NAME_BYTES {
        return Err(graph_text_capture_limit_error("semantic title bytes"));
    }
    Ok(GraphTextSemanticNameBudget {
        semantic_name_bytes: observed,
    })
}

fn guarded_graph_text_semantic_name_upper_bound(
    graph: &Graph,
    path: &GraphTextPath,
    _content_len: usize,
) -> io::Result<u64> {
    let title_format = graph_text_journal_title_format_budget(graph)?;
    let observed =
        checked_add_bytes(usize_to_u64(path.as_str().len())?, 64)?.max(title_format.rendered_bytes);
    if observed > MAX_GRAPH_TEXT_SEMANTIC_NAME_BYTES {
        return Err(graph_text_capture_limit_error("semantic title bytes"));
    }
    Ok(observed)
}

pub(super) fn graph_text_file_record_worst_case_upper_bound(
    graph: &Graph,
    path_len: u64,
    semantic_name_len: u64,
) -> io::Result<u64> {
    let absolute_len = checked_add_bytes(
        checked_add_bytes(
            usize_to_u64(graph.root.as_os_str().len())?,
            usize::from(path_len != 0) as u64,
        )?,
        path_len,
    )?;
    let portable_key_len = checked_mul_bytes(path_len, 8)?;
    let semantic_key_len = checked_mul_bytes(semantic_name_len, 8)?;
    let mut bytes = conservative_btree_entry_bytes::<GraphTextPath, GraphTextAdmissionRecord>()?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(path_len)?)?;
    bytes = checked_add_bytes(bytes, usize_to_u64(std::mem::size_of::<PageEntry>())?)?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(semantic_name_len)?)?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(path_len)?)?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(absolute_len)?)?;
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<PortablePathKey, std::collections::BTreeSet<GraphTextPath>>(
        )?,
    )?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(portable_key_len)?)?;
    bytes = checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<GraphTextPath, ()>(path_len)?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<(u8, String), std::collections::BTreeSet<GraphTextPath>>(
        )?,
    )?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(semantic_key_len)?)?;
    checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<GraphTextPath, ()>(path_len)?,
    )
}

pub(super) fn graph_text_parse_budget_permit(
    graph: &Graph,
    path: &GraphTextPath,
    content: &str,
) -> io::Result<GraphTextParseBudgetPermit> {
    let semantic_budget = graph_text_observed_semantic_name_upper_bound(graph, path, content)?;
    // Parser work is one-file-at-a-time and its tree is dropped before the next
    // file. Source-derived `bytes == nodes in every allocation class` estimates
    // used to count parser, DTO, projection, and index representations as if all
    // were retained together. That rejected ordinary large pages (#311). The
    // real envelope is the 64 MiB exact-feed/source cap plus the post-parse
    // 1,000,000-node cap; only the semantic record below survives this call.
    Ok(GraphTextParseBudgetPermit {
        semantic_name_bytes: semantic_budget.semantic_name_bytes,
        semantic_name_allocation_bytes: owned_string_len_upper_bound(
            semantic_budget.semantic_name_bytes,
        )?,
    })
}

pub(super) fn graph_text_admission_upsert_retained_upper_bound(
    relative: &str,
    path: Option<&GraphTextPath>,
    semantic: Option<&PageEntry>,
) -> io::Result<u64> {
    let relative_len = usize_to_u64(relative.len())?;
    let mut bytes = graph_text_owned_btree_row_upper_bound::<String, ContentDigest>(relative_len)?;
    bytes = checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<String, u64>(relative_len)?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<String, bool>(relative_len)?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<ContentDigest, std::collections::BTreeSet<String>>()?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<String, ()>(relative_len)?,
    )?;
    let (Some(path), Some(semantic)) = (path, semantic) else {
        return Ok(bytes);
    };
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<GraphTextPath, GraphTextAdmissionRecord>()?,
    )?;
    bytes = checked_add_bytes(bytes, owned_string_upper_bound(path.as_str())?)?;
    bytes = checked_add_bytes(bytes, graph_text_page_entry_retained_upper_bound(semantic)?)?;
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<PortablePathKey, std::collections::BTreeSet<GraphTextPath>>(
        )?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        owned_string_len_upper_bound(checked_mul_bytes(relative_len, 8)?)?,
    )?;
    bytes = checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<GraphTextPath, ()>(relative_len)?,
    )?;
    let semantic_key = graph_text_semantic_key(semantic);
    bytes = checked_add_bytes(
        bytes,
        conservative_btree_entry_bytes::<(u8, String), std::collections::BTreeSet<GraphTextPath>>(
        )?,
    )?;
    bytes = checked_add_bytes(bytes, owned_string_upper_bound(&semantic_key.1)?)?;
    bytes = checked_add_bytes(
        bytes,
        graph_text_owned_btree_row_upper_bound::<GraphTextPath, ()>(relative_len)?,
    )?;
    Ok(bytes)
}

pub(super) fn graph_text_admission_tombstone_upper_bound(
    relative: &str,
    record: Option<&GraphTextAdmissionRecord>,
) -> io::Result<u64> {
    let relative_len = usize_to_u64(relative.len())?;
    let mut bytes = conservative_btree_entry_bytes::<GraphTextPath, GraphTextAdmissionTombstone>()?;
    bytes = checked_add_bytes(bytes, owned_string_len_upper_bound(relative_len)?)?;
    if let Some(record) = record {
        bytes = checked_add_bytes(bytes, page_entry_clone_upper_bound(&record.semantic)?)?;
    }
    checked_add_bytes(bytes, 512)
}

pub(super) fn graph_text_admission_delta_structural_peak(
    index: &CompleteGraphTextAdmissionIndex,
) -> io::Result<u64> {
    let mut bytes = 0;
    for charge in [
        index.files_by_exact_path.path_copy_peak_upper_bound()?,
        index.paths_by_portable_key.path_copy_peak_upper_bound()?,
        index.paths_by_file_resource.path_copy_peak_upper_bound()?,
        index
            .file_resource_by_exact_relative
            .path_copy_peak_upper_bound()?,
        index
            .file_link_count_by_exact_relative
            .path_copy_peak_upper_bound()?,
        index
            .file_is_graph_text_by_exact_relative
            .path_copy_peak_upper_bound()?,
        index.paths_by_semantic_key.path_copy_peak_upper_bound()?,
        index
            .tombstones_by_exact_path
            .path_copy_peak_upper_bound()?,
    ] {
        bytes = checked_add_bytes(bytes, charge)?;
    }
    Ok(bytes)
}

pub(super) fn graph_text_admission_delta_payload_peak(
    index: &CompleteGraphTextAdmissionIndex,
    relative: &str,
    prepared: Option<&PreparedGraphTextAdmissionUpsert>,
) -> io::Result<u64> {
    fn graph_text_members(
        members: Option<&std::collections::BTreeSet<GraphTextPath>>,
    ) -> io::Result<u64> {
        let Some(members) = members else {
            return Ok(0);
        };
        count_graph_text_admission_persistent_payload_members(members.len());
        let mut bytes = conservative_btree_entry_bytes::<GraphTextPath, ()>()?;
        for member in members {
            bytes = checked_add_bytes(
                bytes,
                graph_text_owned_btree_row_upper_bound::<GraphTextPath, ()>(usize_to_u64(
                    member.as_str().len(),
                )?)?,
            )?;
        }
        Ok(bytes)
    }

    fn string_members(members: Option<&std::collections::BTreeSet<String>>) -> io::Result<u64> {
        let Some(members) = members else {
            return Ok(0);
        };
        count_graph_text_admission_persistent_payload_members(members.len());
        let mut bytes = conservative_btree_entry_bytes::<String, ()>()?;
        for member in members {
            bytes = checked_add_bytes(
                bytes,
                graph_text_owned_btree_row_upper_bound::<String, ()>(usize_to_u64(member.len())?)?,
            )?;
        }
        Ok(bytes)
    }

    let mut bytes = 0;
    if let Ok(path) = GraphTextPath::parse(relative.to_owned()) {
        if let Some(record) = index.files_by_exact_path.get(&path) {
            bytes = checked_add_bytes(
                bytes,
                graph_text_members(index.paths_by_portable_key.get(&path.portable_key()))?,
            )?;
            bytes = checked_add_bytes(
                bytes,
                graph_text_members(
                    index
                        .paths_by_semantic_key
                        .get(&graph_text_semantic_key(&record.semantic)),
                )?,
            )?;
        }
    }
    if let Some(resource) = index.file_resource_by_exact_relative.get(relative) {
        bytes = checked_add_bytes(
            bytes,
            string_members(index.paths_by_file_resource.get(resource))?,
        )?;
    }
    if let Some(prepared) = prepared {
        bytes = checked_add_bytes(
            bytes,
            string_members(index.paths_by_file_resource.get(&prepared.file_resource_id))?,
        )?;
        if let Some((path, record)) = &prepared.eligible {
            bytes = checked_add_bytes(
                bytes,
                graph_text_members(index.paths_by_portable_key.get(&path.portable_key()))?,
            )?;
            bytes = checked_add_bytes(
                bytes,
                graph_text_members(
                    index
                        .paths_by_semantic_key
                        .get(&graph_text_semantic_key(&record.semantic)),
                )?,
            )?;
        }
    }
    Ok(bytes)
}

pub(super) fn graph_text_admission_upsert_worst_case_upper_bound(
    graph: &Graph,
    relative: &str,
    eligible_path: Option<&GraphTextPath>,
    content: &str,
) -> io::Result<u64> {
    let relative_len = usize_to_u64(relative.len())?;
    let mut bytes = graph_text_admission_upsert_retained_upper_bound(relative, None, None)?;
    if eligible_path.is_some() {
        let path = eligible_path.expect("checked eligible path");
        let semantic_name_len =
            graph_text_observed_semantic_name_upper_bound(graph, path, content)?
                .semantic_name_bytes;
        bytes = checked_add_bytes(
            bytes,
            graph_text_file_record_worst_case_upper_bound(graph, relative_len, semantic_name_len)?,
        )?;
    }
    Ok(bytes)
}

pub(super) fn persistent_set_insert<K, T>(
    map: &mut PersistentMap<K, std::collections::BTreeSet<T>>,
    key: K,
    member: T,
) where
    K: Ord + Clone,
    T: Ord + Clone,
{
    let mut members = map.get(&key).cloned().unwrap_or_default();
    count_graph_text_admission_persistent_payload_members(members.len().saturating_add(1));
    members.insert(member);
    map.insert(key, members);
}

fn persistent_set_remove<K, T>(
    map: &mut PersistentMap<K, std::collections::BTreeSet<T>>,
    key: &K,
    member: &T,
) where
    K: Ord + Clone,
    T: Ord + Clone,
{
    let Some(mut members) = map.get(key).cloned() else {
        return;
    };
    count_graph_text_admission_persistent_payload_members(members.len().saturating_add(1));
    members.remove(member);
    if members.is_empty() {
        map.remove(key);
    } else {
        map.insert(key.clone(), members);
    }
}

fn initial_graph_text_collision_group_insert<K, T>(
    map: &mut std::collections::BTreeMap<K, std::collections::BTreeSet<T>>,
    key: K,
    member: T,
) where
    K: Ord,
    T: Ord,
{
    count_graph_text_admission_persistent_payload_members(1);
    map.entry(key).or_default().insert(member);
}

pub(super) fn validate_graph_text_admission_index(
    index: &CompleteGraphTextAdmissionIndex,
) -> io::Result<()> {
    for (path, record) in &index.files_by_exact_path {
        if !index
            .paths_by_portable_key
            .get(&path.portable_key())
            .is_some_and(|members| members.contains(path))
            || !index
                .paths_by_file_resource
                .get(&record.file_resource_id)
                .is_some_and(|members| members.contains(path.as_str()))
            || index
                .file_resource_by_exact_relative
                .get(path.as_str())
                .copied()
                != Some(record.file_resource_id)
            || index
                .file_link_count_by_exact_relative
                .get(path.as_str())
                .copied()
                != Some(record.link_count)
            || index
                .file_is_graph_text_by_exact_relative
                .get(path.as_str())
                .copied()
                != Some(true)
            || !index
                .paths_by_semantic_key
                .get(&graph_text_semantic_key(&record.semantic))
                .is_some_and(|members| members.contains(path))
            || index.tombstones_by_exact_path.contains_key(path)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph-text admission reverse map is incomplete",
            ));
        }
    }
    for (portable, members) in &index.paths_by_portable_key {
        for path in members {
            let Some(record) = index.files_by_exact_path.get(path) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph-text portable map contains a reverse-only member",
                ));
            };
            if path.portable_key() != *portable
                || index
                    .file_resource_by_exact_relative
                    .get(&path.as_str().to_owned())
                    .copied()
                    != Some(record.file_resource_id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph-text portable member is under the wrong reverse key",
                ));
            }
        }
    }
    for (semantic, members) in &index.paths_by_semantic_key {
        for path in members {
            let Some(record) = index.files_by_exact_path.get(path) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph-text semantic map contains a reverse-only member",
                ));
            };
            if graph_text_semantic_key(&record.semantic) != *semantic {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph-text semantic member is under the wrong reverse key",
                ));
            }
        }
    }
    for (resource, members) in &index.paths_by_file_resource {
        for relative in members {
            if index.file_resource_by_exact_relative.get(relative) != Some(resource) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "graph-text resource member is under the wrong reverse key",
                ));
            }
        }
    }
    for (relative, resource) in &index.file_resource_by_exact_relative {
        if !index
            .paths_by_file_resource
            .get(resource)
            .is_some_and(|members| members.contains(relative))
            || !index
                .file_link_count_by_exact_relative
                .contains_key(relative)
            || !index
                .file_is_graph_text_by_exact_relative
                .contains_key(relative)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph-text file-resource map contains a reverse-only member",
            ));
        }
    }
    for relative in index.file_link_count_by_exact_relative.keys() {
        if !index.file_resource_by_exact_relative.contains_key(relative)
            || !index
                .file_is_graph_text_by_exact_relative
                .contains_key(relative)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph-text link-count map contains a forward-only member",
            ));
        }
    }
    for (relative, is_graph_text) in &index.file_is_graph_text_by_exact_relative {
        let exact = GraphTextPath::parse(relative.clone())
            .ok()
            .and_then(|path| index.files_by_exact_path.get(&path));
        if !index.file_resource_by_exact_relative.contains_key(relative)
            || !index
                .file_link_count_by_exact_relative
                .contains_key(relative)
            || (*is_graph_text != exact.is_some())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph-text path-kind relationships are inconsistent",
            ));
        }
    }
    for (path, tombstone) in &index.tombstones_by_exact_path {
        if index.files_by_exact_path.contains_key(path)
            || index
                .file_resource_by_exact_relative
                .contains_key(&path.as_str().to_owned())
            || index
                .file_link_count_by_exact_relative
                .contains_key(&path.as_str().to_owned())
            || index
                .file_is_graph_text_by_exact_relative
                .contains_key(path.as_str())
            || tombstone.prior_record.as_ref().is_some_and(|record| {
                record.file_resource_id != tombstone.prior_file_resource_id
                    || record.link_count != tombstone.prior_link_count
                    || record.semantic.rel_path != path.as_str()
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "graph-text tombstone relationships are inconsistent",
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_graph_text_admission_delta(
    index: &CompleteGraphTextAdmissionIndex,
    relative: &str,
) -> io::Result<()> {
    let path = match GraphTextPath::parse(relative.to_owned()) {
        Ok(path) => path,
        Err(_) => {
            let resource = index.file_resource_by_exact_relative.get(relative);
            let link_count = index.file_link_count_by_exact_relative.get(relative);
            let is_graph_text = index
                .file_is_graph_text_by_exact_relative
                .get(relative)
                .copied();
            if let Some(resource) = resource {
                if link_count.is_none()
                    || is_graph_text != Some(false)
                    || !index
                        .paths_by_file_resource
                        .get(resource)
                        .is_some_and(|members| members.len() == 1 && members.contains(relative))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "exact non-text delta has inconsistent resource evidence",
                    ));
                }
            } else if link_count.is_some() || is_graph_text.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "removed non-text delta retained forward evidence",
                ));
            }
            return Ok(());
        }
    };
    let record = index.files_by_exact_path.get(&path);
    let resource = index
        .file_resource_by_exact_relative
        .get(&relative.to_owned());
    let link_count = index
        .file_link_count_by_exact_relative
        .get(&relative.to_owned());
    let is_graph_text = index
        .file_is_graph_text_by_exact_relative
        .get(relative)
        .copied();
    if let Some(record) = record {
        let portable = index
            .paths_by_portable_key
            .get(&path.portable_key())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing portable reverse group")
            })?;
        let resources = index
            .paths_by_file_resource
            .get(&record.file_resource_id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing resource reverse group")
            })?;
        let semantic = index
            .paths_by_semantic_key
            .get(&graph_text_semantic_key(&record.semantic))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing semantic reverse group")
            })?;
        // The link count must agree with the record, not equal 1: a complete
        // build admits a link outside graph-text scope, so a delta must too, or
        // every external edit of an annexed graph discards the warm index
        // (GH #571, GH #555). A second graph-text name is `resources.len() != 1`.
        if resource != Some(&record.file_resource_id)
            || link_count != Some(&record.link_count)
            || !portable.contains(&path)
            || resources.len() != 1
            || !resources.contains(relative)
            || !semantic.contains(&path)
            || index.tombstones_by_exact_path.contains_key(&path)
            || is_graph_text != Some(true)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "exact graph-text delta has colliding or inconsistent evidence: \
                     resource_match={} link_match={} links={} portable_member={} \
                     resource_members={} resource_member={} semantic_member={} \
                     tombstoned={} graph_text={is_graph_text:?}",
                    resource == Some(&record.file_resource_id),
                    link_count == Some(&record.link_count),
                    record.link_count,
                    portable.contains(&path),
                    resources.len(),
                    resources.contains(relative),
                    semantic.contains(&path),
                    index.tombstones_by_exact_path.contains_key(&path),
                ),
            ));
        }
    } else if let Some(resource) = resource {
        let resources = index.paths_by_file_resource.get(resource).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing resource reverse group")
        })?;
        if link_count.is_none()
            || resources.len() != 1
            || !resources.contains(relative)
            || is_graph_text != Some(false)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exact non-text delta has inconsistent resource evidence",
            ));
        }
    } else if link_count.is_some() || is_graph_text.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "exact delta retained link evidence without a resource",
        ));
    }
    if index.tombstones_by_exact_path.contains_key(&path)
        && (record.is_some()
            || resource.is_some()
            || link_count.is_some()
            || is_graph_text.is_some())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "exact delta is simultaneously active and deleted",
        ));
    }
    Ok(())
}

pub(super) fn graph_text_delta_reverse_members(
    index: &CompleteGraphTextAdmissionIndex,
    relative: &str,
) -> usize {
    let Ok(path) = GraphTextPath::parse(relative.to_owned()) else {
        return 0;
    };
    let Some(record) = index.files_by_exact_path.get(&path) else {
        return index
            .file_resource_by_exact_relative
            .get(relative)
            .and_then(|resource| index.paths_by_file_resource.get(resource))
            .map_or(0, std::collections::BTreeSet::len);
    };
    index
        .paths_by_portable_key
        .get(&path.portable_key())
        .map_or(0, std::collections::BTreeSet::len)
        + index
            .paths_by_file_resource
            .get(&record.file_resource_id)
            .map_or(0, std::collections::BTreeSet::len)
        + index
            .paths_by_semantic_key
            .get(&graph_text_semantic_key(&record.semantic))
            .map_or(0, std::collections::BTreeSet::len)
}

pub(super) fn remove_graph_text_admission_path(
    index: &mut CompleteGraphTextAdmissionIndex,
    relative: &str,
) -> Option<GraphTextAdmissionTombstone> {
    let mut prior_record = None;
    if let Ok(path) = GraphTextPath::parse(relative.to_owned()) {
        if let Some(record) = index.files_by_exact_path.remove(&path) {
            let portable = path.portable_key();
            persistent_set_remove(&mut index.paths_by_portable_key, &portable, &path);
            let semantic = graph_text_semantic_key(&record.semantic);
            persistent_set_remove(&mut index.paths_by_semantic_key, &semantic, &path);
            prior_record = Some(record);
        }
    }
    let relative_owned = relative.to_owned();
    let resource = index
        .file_resource_by_exact_relative
        .remove(&relative_owned)?;
    persistent_set_remove(
        &mut index.paths_by_file_resource,
        resource.as_ref(),
        &relative_owned,
    );
    let link_count = index
        .file_link_count_by_exact_relative
        .remove(&relative_owned)
        .map_or(0, |links| *links);
    index
        .file_is_graph_text_by_exact_relative
        .remove(&relative_owned);
    Some(GraphTextAdmissionTombstone {
        prior_record,
        prior_file_resource_id: *resource,
        prior_link_count: link_count,
    })
}

pub(super) fn graph_text_event_scratch_upper_bound(relative: &str) -> io::Result<u64> {
    let relative_len = usize_to_u64(relative.len())?;
    let component_slots = relative_len.max(1);
    let mut bytes = conservative_vec_capacity_upper_bound::<String>(component_slots)?;
    bytes = checked_add_bytes(
        bytes,
        conservative_vec_capacity_upper_bound::<Dir>(component_slots)?,
    )?;
    // Component strings, filename, parent-relative construction, GraphTextPath,
    // normalized keys, and error-path scratch are never simultaneously larger
    // than these conservative full-relative clones.
    bytes = checked_add_bytes(
        bytes,
        checked_mul_bytes(owned_string_len_upper_bound(relative_len)?, 8)?,
    )?;
    checked_add_bytes(bytes, 1024)
}
