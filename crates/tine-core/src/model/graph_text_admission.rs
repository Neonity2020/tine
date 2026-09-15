//! Admitting graph-text events: classifying a path, capturing retained
//! identity, preparing and applying file upserts and removes under the batch
//! charges, and the event-parent and snapshot-binding policies.

use super::*;

impl Graph {
    /// Capture exact current page bytes through the same retained graph
    /// capability used by the guarded writer.
    pub(crate) fn read_projection_input(
        &self,
        path: &GraphTextPath,
    ) -> io::Result<Option<Vec<u8>>> {
        require_projection_platform()?;
        let target = self.projection_page_target(path.as_str())?;
        let lock = self.page_lock(&target.absolute_path);
        let _guard = lock.lock().unwrap();
        let Some(parent) = self.projection_parent_optional(&target)? else {
            return Ok(None);
        };
        self.ensure_projection_parent_binding(&parent, &target)?;
        self.ensure_projection_target_shape(&parent, &target)?;
        read_projection_optional(parent.final_dir(), &target.filename)
    }

    /// Classify one exact graph-text path against this graph's configured text
    /// roots. The longest component-boundary match wins; equal roots are
    /// ambiguous and therefore rejected instead of guessed.
    pub(crate) fn classify_graph_text_path(
        &self,
        path: &GraphTextPath,
    ) -> Result<GraphTextKind, UnsafeGraphTextPath> {
        let path_components = path.as_str().split('/').collect::<Vec<_>>();
        let page_root = configured_root_components(&self.config.pages_dir);
        let journal_root = configured_root_components(&self.config.journals_dir);
        let Some(page_root) = page_root else {
            return Err(UnsafeGraphTextPath(path.as_str().to_owned()));
        };
        let Some(journal_root) = journal_root else {
            return Err(UnsafeGraphTextPath(path.as_str().to_owned()));
        };
        if page_root == journal_root {
            return Err(UnsafeGraphTextPath(path.as_str().to_owned()));
        }

        let page_matches =
            path_components.len() > page_root.len() && path_components.starts_with(&page_root);
        let journal_matches = path_components.len() > journal_root.len()
            && path_components.starts_with(&journal_root);
        match (page_matches, journal_matches) {
            (true, false) => Ok(GraphTextKind::Page),
            (false, true) => Ok(GraphTextKind::Journal),
            (true, true) if page_root.len() > journal_root.len() => Ok(GraphTextKind::Page),
            (true, true) if journal_root.len() > page_root.len() => Ok(GraphTextKind::Journal),
            _ => Err(UnsafeGraphTextPath(path.as_str().to_owned())),
        }
    }

    /// Capture the resource retained by this Graph even when its ambient path
    /// has subsequently been moved or reserved by a replacement graph. The
    /// graph-text write gate and retained directory capability are the authority;
    /// checking the ambient path here would both reject supported moves and
    /// accidentally inspect the replacement resource.
    pub(super) fn capture_retained_graph_text_identity_with_limits(
        &self,
        limits: GraphTextCaptureLimits,
    ) -> io::Result<(GraphTextCapture, u64)> {
        // GH #267 / F3. The two passes must agree, and ANY concurrent filesystem
        // activity anywhere in the graph makes them disagree -- which on a
        // Syncthing, Dropbox or OneDrive folder is not an anomaly, it is the
        // steady state. A single disagreement used to surface as a failed save.
        //
        // Disagreement means "something moved while we looked", not "the graph
        // is broken", so retry it in place a bounded number of times. Only that
        // one outcome is retried; every other error still surfaces at once.
        // The caller holds the identity-mutation authority across all attempts,
        // so this cannot interleave with one of our own writes.
        const CAPTURE_ATTEMPTS: usize = 4;
        let mut last_disagreement = None;
        for _ in 0..CAPTURE_ATTEMPTS {
            match self.attempt_retained_graph_text_identity_capture(limits) {
                Ok(captured) => return Ok(captured),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    last_disagreement = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_disagreement.unwrap_or_else(|| {
            DirectSaveError::into_io(
                DirectSaveFailureCode::PrecheckInterrupted,
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    "graph inventory changed during retained identity capture",
                ),
            )
        }))
    }

    fn attempt_retained_graph_text_identity_capture(
        &self,
        limits: GraphTextCaptureLimits,
    ) -> io::Result<(GraphTextCapture, u64)> {
        require_projection_platform()?;
        let permit = self.admit_graph_text_writer()?;
        let first = collect_graph_text_capture_inner(self, &permit, true, limits, 0, false, true)?;
        graph_text_capture_revalidation_hook(&self.root)?;
        let second = collect_graph_text_capture_inner(
            self,
            &permit,
            false,
            limits,
            first.peak_build_charge,
            false,
            true,
        )?;
        if !graph_text_captures_match(&first, &second) {
            return Err(DirectSaveError::into_io(
                DirectSaveFailureCode::PrecheckInterrupted,
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    "graph inventory changed during retained identity capture",
                ),
            ));
        }
        let combined_capture_bytes =
            checked_add_bytes(first.peak_build_charge, second.peak_build_charge)?;
        if combined_capture_bytes > limits.peak_build_bytes {
            return Err(graph_text_capture_limit_error("peak build memory"));
        }
        Ok((first, combined_capture_bytes))
    }

    /// Classify one exact feed path without duplicating scope policy.
    pub fn classify_graph_text_exact_feed_path(
        &self,
        relative: &str,
    ) -> io::Result<GraphTextExactFeedPathClass> {
        validate_graph_text_exact_feed_relative(relative)?;
        if relative.eq_ignore_ascii_case(CONFIG_RELATIVE_PATH) {
            return Ok(GraphTextExactFeedPathClass::Configuration);
        }
        let mut parent = String::new();
        let components = relative.split('/').collect::<Vec<_>>();
        for component in &components[..components.len().saturating_sub(1)] {
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(component);
            if !self.graph_text_scope.should_descend(&parent) {
                return Ok(GraphTextExactFeedPathClass::Excluded);
            }
        }
        Ok(GraphTextExactFeedPathClass::RetainedFile)
    }

    pub(super) fn prepare_graph_text_admission_final_state(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        relative: String,
        batch_scratch: u64,
        actual_charges: &mut GraphTextExactFeedBatchActualCharges,
        require_ambient_binding: bool,
        decode_semantics: bool,
    ) -> io::Result<PreparedGraphTextAdmissionFinalState> {
        let target = self.graph_text_exact_path(&relative, false)?;
        let parent = self.graph_text_event_parent_policy(&target, require_ambient_binding)?;
        validate_graph_text_event_parent(index, &target, &parent)?;
        match parent.final_dir().symlink_metadata(&target.filename) {
            Ok(metadata) if metadata.is_file() => self
                .prepare_graph_text_file_upsert_for_batch(
                    index,
                    relative,
                    batch_scratch,
                    actual_charges,
                    require_ambient_binding,
                    decode_semantics,
                )
                .map(PreparedGraphTextAdmissionFinalState::Present),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::Interrupted,
                graph_text_exact_feed_failure_cause(&format!(
                    "touched path became non-regular: {relative}"
                )),
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => self
                .prepare_graph_text_file_remove(index, relative, require_ambient_binding)
                .map(PreparedGraphTextAdmissionFinalState::Absent),
            Err(error) => Err(error),
        }
    }

    pub(super) fn revalidate_prepared_graph_text_admission_batch(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        prepared: &[PreparedGraphTextAdmissionFinalState],
        batch_scratch: u64,
        prepared_growth: u64,
        require_ambient_binding: bool,
    ) -> io::Result<()> {
        self.ensure_graph_text_admission_snapshot_binding_policy(index, require_ambient_binding)?;
        let live = checked_add_bytes(index.permanent_bytes, batch_scratch)
            .and_then(|bytes| checked_add_bytes(bytes, prepared_growth))?;
        let remaining_peak = index
            .peak_limit
            .checked_sub(live)
            .ok_or_else(|| graph_text_capture_limit_error("peak build memory"))?;
        let mut raw_bytes = 0_u64;
        for final_state in prepared {
            let (relative, expected) = match final_state {
                PreparedGraphTextAdmissionFinalState::Present(upsert) => {
                    (&upsert.relative, Some(upsert))
                }
                PreparedGraphTextAdmissionFinalState::Absent(remove) => (&remove.relative, None),
            };
            let target = self.graph_text_exact_path(relative, false)?;
            let parent = self.graph_text_event_parent_policy(&target, require_ambient_binding)?;
            validate_graph_text_event_parent(index, &target, &parent)?;
            match expected {
                Some(upsert) => {
                    let remaining_raw = MAX_GRAPH_TEXT_EXACT_FEED_BATCH_RAW_BYTES
                        .checked_sub(raw_bytes)
                        .ok_or_else(|| {
                            graph_text_capture_limit_error("exact feed batch aggregate raw bytes")
                        })?;
                    let expected_len = upsert.description.byte_length();
                    if expected_len > remaining_raw {
                        return Err(graph_text_capture_limit_error(
                            "exact feed batch aggregate raw bytes",
                        ));
                    }
                    let file = open_projection_file_nofollow(parent.final_dir(), &target.filename)?;
                    if canonical_projection_file_resource_id(&file)? != upsert.file_resource_id
                        || projection_file_link_count(&file)? != upsert.link_count
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            format!("exact feed batch resource/link proof changed: {relative}"),
                        ));
                    }
                    let (_, description, resource, _, _) =
                        read_projection_optional_bound_capture_with_limits(
                            parent.final_dir(),
                            &target.filename,
                            expected_len,
                            remaining_peak,
                        )?
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::Interrupted,
                                format!(
                                    "exact feed batch path disappeared during final proof: {relative}"
                                ),
                            )
                        })?;
                    raw_bytes = checked_add_bytes(raw_bytes, expected_len)?;
                    if description != upsert.description || resource != upsert.file_resource_id {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            format!(
                                "exact feed batch bytes/resource changed during final proof: {relative}"
                            ),
                        ));
                    }
                }
                None => match parent.final_dir().symlink_metadata(&target.filename) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            format!(
                                "exact feed batch absent path reappeared during final proof: {relative}"
                            ),
                        ));
                    }
                    Err(error) => return Err(error),
                },
            }
        }
        self.ensure_graph_text_admission_snapshot_binding_policy(index, require_ambient_binding)
    }

    fn prepare_graph_text_file_upsert_for_batch(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        relative: String,
        batch_scratch: u64,
        actual_charges: &mut GraphTextExactFeedBatchActualCharges,
        require_ambient_binding: bool,
        decode_semantics: bool,
    ) -> io::Result<PreparedGraphTextAdmissionUpsert> {
        self.prepare_graph_text_file_upsert_with_batch_charges(
            index,
            relative,
            batch_scratch,
            Some(actual_charges),
            require_ambient_binding,
            decode_semantics,
        )
    }

    fn graph_text_exact_feed_worst_permanent_growth(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        relative: &str,
        present_len: Option<u64>,
    ) -> io::Result<u64> {
        let eligible_path = self
            .graph_text_scope
            .is_eligible(relative)
            .then(|| GraphTextPath::parse(relative.to_owned()))
            .transpose()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        let mut path_growth =
            graph_text_admission_upsert_retained_upper_bound(relative, None, None)?;
        if eligible_path.is_some() {
            let title_format = graph_text_journal_title_format_budget(self)?;
            let accepted_semantic_name_bound = checked_add_bytes(present_len.unwrap_or(0), 64)?
                .max(checked_add_bytes(usize_to_u64(relative.len())?, 64)?)
                .max(title_format.rendered_bytes)
                .min(MAX_GRAPH_TEXT_SEMANTIC_NAME_BYTES);
            path_growth = checked_add_bytes(
                path_growth,
                graph_text_file_record_worst_case_upper_bound(
                    self,
                    usize_to_u64(relative.len())?,
                    accepted_semantic_name_bound,
                )?,
            )?;
        }
        if let Some(path) = eligible_path.as_ref() {
            path_growth = checked_add_bytes(
                path_growth,
                graph_text_admission_tombstone_upper_bound(
                    relative,
                    index.files_by_exact_path.get(path),
                )?,
            )?;
        }
        Ok(path_growth)
    }
    fn prepare_graph_text_file_upsert_with_batch_charges(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        relative: String,
        event_scratch: u64,
        mut actual_charges: Option<&mut GraphTextExactFeedBatchActualCharges>,
        require_ambient_binding: bool,
        decode_semantics: bool,
    ) -> io::Result<PreparedGraphTextAdmissionUpsert> {
        let target = self.graph_text_exact_path(&relative, false)?;
        let parent = self.graph_text_event_parent_policy(&target, require_ambient_binding)?;
        validate_graph_text_event_parent(index, &target, &parent)?;
        let enumerated = open_projection_file_nofollow(parent.final_dir(), &target.filename)?;
        let enumerated_resource = canonical_projection_file_resource_id(&enumerated)?;
        let link_count = projection_file_link_count(&enumerated)?;
        if link_count != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("exact feed upsert has an unsafe link count: {relative}"),
            ));
        }
        let enumerated_len = enumerated.metadata()?.len();
        if let Some(charges) = actual_charges.as_deref_mut() {
            let worst_growth = self.graph_text_exact_feed_worst_permanent_growth(
                index,
                &relative,
                Some(enumerated_len),
            )?;
            charges.ensure_permanent_growth(index, worst_growth)?;
            charges.reserve_raw(index, event_scratch, enumerated_len)?;
        }
        let live_bytes = match actual_charges.as_deref() {
            Some(charges) => charges.live_preparation_bytes(index, event_scratch)?,
            None => checked_add_bytes(index.permanent_bytes, event_scratch)?,
        };
        let remaining_peak = match actual_charges.as_deref() {
            Some(charges) => charges.remaining_peak(index, event_scratch)?,
            None => index
                .peak_limit
                .checked_sub(live_bytes)
                .ok_or_else(|| graph_text_capture_limit_error("peak build memory"))?,
        };
        let content_limit = match actual_charges.as_deref() {
            Some(_) => enumerated_len,
            None => MAX_PROJECTION_EVIDENCE_BYTES,
        };
        let (bytes, description, file_resource_id, _, _) =
            read_projection_optional_bound_capture_with_limits(
                parent.final_dir(),
                &target.filename,
                content_limit,
                remaining_peak,
            )?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("exact feed upsert disappeared: {}", relative),
                )
            })?;
        if actual_charges.is_some() && usize_to_u64(bytes.capacity())? != enumerated_len {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("exact feed upsert length changed after admission: {relative}"),
            ));
        }
        if file_resource_id != enumerated_resource {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("exact feed upsert changed after observation: {relative}"),
            ));
        }
        self.ensure_graph_text_admission_snapshot_binding_policy(index, require_ambient_binding)?;
        let eligible_path =
            if self.graph_text_scope.is_eligible(&relative) {
                Some(GraphTextPath::parse(relative.clone()).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
                })?)
            } else {
                None
            };
        let content_for_bound = if eligible_path.is_some() {
            std::str::from_utf8(&bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("graph text is not UTF-8: {relative}"),
                )
            })?
        } else {
            ""
        };
        let worst_growth = graph_text_admission_upsert_worst_case_upper_bound(
            self,
            &relative,
            eligible_path.as_ref(),
            content_for_bound,
        )?;
        if let Some(charges) = actual_charges.as_deref() {
            charges.ensure_permanent_growth(index, worst_growth)?;
        } else {
            let worst_permanent = index
                .permanent_bytes
                .checked_add(worst_growth)
                .ok_or_else(|| graph_text_capture_limit_error("permanent index memory"))?;
            if worst_permanent > index.permanent_limit {
                return Err(graph_text_capture_limit_error("permanent index memory"));
            }
        }
        let eligible = if let Some(path) = eligible_path {
            let (semantic, format) = if decode_semantics {
                let content = std::str::from_utf8(&bytes).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("graph text is not UTF-8: {path}"),
                    )
                })?;
                let permit = graph_text_parse_budget_permit(self, &path, content)?;
                let (semantic, format, node_count) =
                    self.decode_present_graph_text_with_node_count(&path, &bytes, permit)?;
                if node_count > MAX_GRAPH_TEXT_PARSER_NODES {
                    return Err(graph_text_capture_limit_error("parser node count"));
                }
                (semantic, format)
            } else {
                (
                    self.graph_text_entry_for_graph_text_path(&path)
                        .map_err(|error| {
                            io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                        })?,
                    Format::from_path(Path::new(path.as_str())),
                )
            };
            Some((
                path,
                GraphTextAdmissionRecord {
                    description,
                    file_resource_id,
                    link_count,
                    semantic,
                    format,
                    semantic_parsed: decode_semantics,
                },
            ))
        } else {
            None
        };
        let retained_growth = graph_text_admission_upsert_retained_upper_bound(
            &relative,
            eligible.as_ref().map(|(path, _)| path),
            eligible.as_ref().map(|(_, record)| &record.semantic),
        )?;
        if let Some(charges) = actual_charges.as_deref() {
            charges.ensure_permanent_growth(index, retained_growth)?;
        } else {
            let final_permanent = index
                .permanent_bytes
                .checked_add(retained_growth)
                .ok_or_else(|| graph_text_capture_limit_error("permanent index memory"))?;
            if final_permanent > index.permanent_limit {
                return Err(graph_text_capture_limit_error("permanent index memory"));
            }
        }
        let revalidation_live = checked_add_bytes(
            checked_add_bytes(live_bytes, usize_to_u64(bytes.capacity())?)?,
            retained_growth,
        )?;
        let revalidation_peak = index
            .peak_limit
            .checked_sub(revalidation_live)
            .ok_or_else(|| graph_text_capture_limit_error("peak build memory"))?;
        graph_text_event_revalidation_race_hook()?;
        let rebound_parent =
            self.graph_text_event_parent_policy(&target, require_ambient_binding)?;
        validate_graph_text_event_parent(index, &target, &rebound_parent)?;
        let rebound = open_projection_file_nofollow(rebound_parent.final_dir(), &target.filename)?;
        if canonical_projection_file_resource_id(&rebound)? != file_resource_id
            || projection_file_link_count(&rebound)? != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("exact feed upsert resource or link proof changed: {relative}"),
            ));
        }
        let (rebound_bytes, rebound_description, rebound_resource, _, _) =
            read_projection_optional_bound_capture_with_limits(
                rebound_parent.final_dir(),
                &target.filename,
                description.byte_length(),
                revalidation_peak,
            )?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("exact feed upsert disappeared during revalidation: {relative}"),
                )
            })?;
        let final_file =
            open_projection_file_nofollow(rebound_parent.final_dir(), &target.filename)?;
        if rebound_bytes != bytes
            || rebound_description != description
            || rebound_resource != file_resource_id
            || canonical_projection_file_resource_id(&final_file)? != file_resource_id
            || projection_file_link_count(&final_file)? != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                format!("exact feed upsert changed during two-sided proof: {relative}"),
            ));
        }
        self.ensure_graph_text_admission_snapshot_binding_policy(index, require_ambient_binding)?;
        Ok(PreparedGraphTextAdmissionUpsert {
            relative,
            description,
            file_resource_id,
            link_count,
            retained_growth,
            eligible,
        })
    }

    pub(super) fn apply_prepared_graph_text_file_upsert(
        &self,
        index: &mut CompleteGraphTextAdmissionIndex,
        prepared: PreparedGraphTextAdmissionUpsert,
    ) -> io::Result<()> {
        let prior_active = index
            .file_resource_by_exact_relative
            .contains_key(prepared.relative.as_str());
        let prior_graph_text = index
            .file_is_graph_text_by_exact_relative
            .get(prepared.relative.as_str())
            .copied()
            == Some(true);
        let prior_tombstone = GraphTextPath::parse(prepared.relative.clone())
            .ok()
            .is_some_and(|path| index.tombstones_by_exact_path.contains_key(&path));
        count_graph_text_admission_event_work(
            8,
            0,
            graph_text_delta_reverse_members(index, &prepared.relative),
        );
        let _ = remove_graph_text_admission_path(index, &prepared.relative);
        let is_graph_text = prepared.eligible.is_some();
        if let Ok(path) = GraphTextPath::parse(prepared.relative.clone()) {
            index.tombstones_by_exact_path.remove(&path);
        }
        index.permanent_bytes = checked_add_bytes(index.permanent_bytes, prepared.retained_growth)?;
        count_graph_text_admission_index_map_insertion();
        persistent_set_insert(
            &mut index.paths_by_file_resource,
            prepared.file_resource_id,
            prepared.relative.clone(),
        );
        count_graph_text_admission_index_map_insertion();
        index
            .file_resource_by_exact_relative
            .insert(prepared.relative.clone(), prepared.file_resource_id);
        count_graph_text_admission_index_map_insertion();
        index
            .file_link_count_by_exact_relative
            .insert(prepared.relative.clone(), prepared.link_count);
        index
            .file_is_graph_text_by_exact_relative
            .insert(prepared.relative.clone(), is_graph_text);
        if let Some((path, record)) = prepared.eligible {
            count_graph_text_admission_index_map_insertion();
            persistent_set_insert(
                &mut index.paths_by_portable_key,
                path.portable_key(),
                path.clone(),
            );
            count_graph_text_admission_index_map_insertion();
            persistent_set_insert(
                &mut index.paths_by_semantic_key,
                graph_text_semantic_key(&record.semantic),
                path.clone(),
            );
            count_graph_text_admission_index_map_insertion();
            index.files_by_exact_path.insert(path, record);
        }
        let writes = usize::from(prior_active) * 4
            + usize::from(prior_graph_text) * 3
            + usize::from(prior_tombstone)
            + 4
            + usize::from(is_graph_text) * 3;
        count_graph_text_admission_event_work(
            8,
            writes,
            graph_text_delta_reverse_members(index, &prepared.relative),
        );
        validate_graph_text_admission_delta(index, &prepared.relative)
    }

    fn prepare_graph_text_file_remove(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        relative: String,
        require_ambient_binding: bool,
    ) -> io::Result<PreparedGraphTextAdmissionRemove> {
        let target = self.graph_text_exact_path(&relative, false)?;
        match self.graph_text_event_parent_policy(&target, require_ambient_binding) {
            Ok(parent) => {
                validate_graph_text_event_parent(index, &target, &parent)?;
                match parent.final_dir().symlink_metadata(&target.filename) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            format!("exact feed removal is still present: {relative}"),
                        ));
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("exact feed removal parent is unavailable: {relative}"),
                ));
            }
            Err(error) => return Err(error),
        }
        self.ensure_graph_text_admission_snapshot_binding_policy(index, require_ambient_binding)?;
        graph_text_event_revalidation_race_hook()?;
        let rebound_parent = self
            .graph_text_event_parent_policy(&target, require_ambient_binding)
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("exact feed removal parent changed: {error}"),
                )
            })?;
        validate_graph_text_event_parent(index, &target, &rebound_parent)?;
        match rebound_parent
            .final_dir()
            .symlink_metadata(&target.filename)
        {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("exact feed removal reappeared: {relative}"),
                ));
            }
            Err(error) => return Err(error),
        }
        self.ensure_graph_text_admission_snapshot_binding_policy(index, require_ambient_binding)?;
        let retained_growth = match target.graph_text_path.as_ref() {
            Some(path)
                if index
                    .file_resource_by_exact_relative
                    .contains_key(relative.as_str()) =>
            {
                graph_text_admission_tombstone_upper_bound(
                    &relative,
                    index.files_by_exact_path.get(path),
                )?
            }
            None => 0,
            Some(_) => 0,
        };
        Ok(PreparedGraphTextAdmissionRemove {
            relative,
            retained_growth,
        })
    }

    pub(super) fn apply_prepared_graph_text_file_remove(
        &self,
        index: &mut CompleteGraphTextAdmissionIndex,
        prepared: PreparedGraphTextAdmissionRemove,
    ) -> io::Result<()> {
        let relative = prepared.relative;
        let prior_graph_text = index
            .file_is_graph_text_by_exact_relative
            .get(relative.as_str())
            .copied()
            == Some(true);
        count_graph_text_admission_event_work(
            8,
            0,
            graph_text_delta_reverse_members(index, &relative),
        );
        if let Some(tombstone) = remove_graph_text_admission_path(index, &relative) {
            if let Ok(path) = GraphTextPath::parse(relative.to_owned()) {
                index.tombstones_by_exact_path.insert(path, tombstone);
            }
            index.permanent_bytes =
                checked_add_bytes(index.permanent_bytes, prepared.retained_growth)?;
        }
        count_graph_text_admission_event_work(8, 5 + usize::from(prior_graph_text) * 3, 0);
        validate_graph_text_admission_delta(index, &relative)
    }

    #[cfg(test)]
    pub(super) fn graph_text_event_parent(
        &self,
        target: &GraphTextExactPath,
    ) -> io::Result<ProjectionParent> {
        self.graph_text_event_parent_policy(target, true)
    }

    fn graph_text_event_parent_policy(
        &self,
        target: &GraphTextExactPath,
        require_ambient_binding: bool,
    ) -> io::Result<ProjectionParent> {
        let root = if require_ambient_binding {
            self.projection_root.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "graph has no retained no-follow projection capability",
                )
            })?
        } else {
            &self.graph_text_write_binding()?.root
        };
        let mut chain = vec![root.try_clone()?];
        for component in &target.parent_components {
            let current = chain.last().expect("graph-text parent contains root");
            projection_real_directory(current, component)?;
            chain.push(open_projection_dir_nofollow(current, component)?);
        }
        Ok(ProjectionParent { chain })
    }

    fn ensure_graph_text_admission_snapshot_binding_policy(
        &self,
        index: &CompleteGraphTextAdmissionIndex,
        require_ambient_binding: bool,
    ) -> io::Result<()> {
        if require_ambient_binding {
            self.ensure_projection_root_binding()?;
        }
        let (graph_resource, scope_binding) = if require_ambient_binding {
            (
                self.canonical_resource_id()?,
                self.graph_text_scope_binding()?,
            )
        } else {
            let binding = self.graph_text_write_binding()?;
            (
                binding.resource_id,
                self.graph_text_scope
                    .bind_graph_resource(binding.resource_id),
            )
        };
        if !Arc::ptr_eq(&index.instance, &self.graph_text_admission_instance)
            || graph_resource != index.graph_resource
            || scope_binding != index.scope_binding
            || scope_binding.graph_resource_id() != graph_resource
        {
            return Err(graph_text_admission_unavailable(
                "graph-text event snapshot binding changed",
            ));
        }
        Ok(())
    }

    pub(super) fn ensure_projection_root_binding(&self) -> io::Result<()> {
        let retained = self.projection_root.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "graph has no retained no-follow projection capability",
            )
        })?;
        let rebound = open_projection_root_nofollow(&self.root)?;
        if projection_dir_identity(retained)? != projection_dir_identity(&rebound)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "graph root changed during graph inventory capture",
            ));
        }
        Ok(())
    }
}
