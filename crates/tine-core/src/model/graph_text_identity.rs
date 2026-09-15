//! Graph's guarded graph-text identity index: building and invalidating it,
//! applying identity-path updates, external-observation tickets, and the
//! write transaction that holds it.

use super::*;

impl Graph {
    /// Return the complete retained identity generation used by ordinary
    /// guarded writes, rebuilding it once after explicit watcher uncertainty.
    /// The caller retains the resource-wide identity-mutation authority across
    /// this call and the filesystem transition it authorizes.
    pub(super) fn guarded_graph_text_identity_index(
        &self,
    ) -> io::Result<Arc<CompleteGraphTextAdmissionIndex>> {
        let binding = self.graph_text_write_binding()?;
        let resource_epoch = binding.gate.identity_mutation_epoch_under_authority()?;
        {
            let state = self.guarded_graph_text_identity.read().unwrap();
            if !state.invalidated && state.observed_resource_epoch == Some(resource_epoch) {
                if let Some(index) = state.index.as_ref() {
                    return Ok(Arc::clone(index));
                }
            }
        }

        let (prior, decode_semantics) = {
            let state = self.guarded_graph_text_identity.read().unwrap();
            (
                state.index.clone(),
                state.invalidated || state.observed_resource_epoch != Some(resource_epoch),
            )
        };
        let capture_started = std::time::Instant::now();
        let (capture, combined_capture_bytes) = self
            .capture_retained_graph_text_identity_with_limits(GRAPH_TEXT_CAPTURE_LIMITS)
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("guarded graph-text identity capture failed: {error}"),
                )
            })?;
        let capture_elapsed = capture_started.elapsed();
        let captured_entries = capture.entries.len();
        let replacement_peak = prior.as_ref().map_or(combined_capture_bytes, |index| {
            combined_capture_bytes.saturating_add(index.permanent_bytes)
        });
        let index_started = std::time::Instant::now();
        let mut replacement = build_graph_text_admission_index(
            self,
            &capture,
            GRAPH_TEXT_CAPTURE_LIMITS,
            replacement_peak,
            decode_semantics,
            prior.as_deref(),
        )
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("guarded graph-text identity construction failed: {error}"),
            )
        })?;
        let index_elapsed = index_started.elapsed();

        if binding.gate.identity_mutation_epoch_under_authority()? != resource_epoch {
            return Err(graph_text_admission_unavailable(
                "resource epoch changed during guarded graph-text identity rebuild",
            ));
        }
        let mut state = self.guarded_graph_text_identity.write().unwrap();
        let next_generation = state.generation.checked_add(1).ok_or_else(|| {
            graph_text_admission_unavailable("guarded graph-text identity generation overflow")
        })?;
        replacement.generation = next_generation;
        let replacement = Arc::new(replacement);
        state.generation = next_generation;
        state.index = Some(Arc::clone(&replacement));
        state.observed_resource_epoch = Some(resource_epoch);
        state.invalidated = false;
        state.invalidation_cause = None;
        state.complete_builds = state.complete_builds.saturating_add(1);
        state.last_build = Some(GuardedGraphTextIdentityBuild {
            capture: capture_elapsed,
            index: index_elapsed,
            decode_semantics,
            captured_entries,
            captured_bytes: combined_capture_bytes,
        });
        Ok(replacement)
    }

    pub(super) fn invalidate_guarded_graph_text_identity(&self, cause: impl Into<String>) {
        // This helper is used both inside larger reentrant write transactions
        // and by broad public invalidation. Taking the authority here makes the
        // shared epoch transition unconditional in both cases.
        let identity = self.lock_graph_text_identity_mutation().ok();
        if let Some(identity) = identity.as_ref() {
            identity.gate.advance_identity_mutation_epoch();
        }
        let mut state = self.guarded_graph_text_identity.write().unwrap();
        state.invalidated = true;
        state.invalidation_cause = Some(bounded_graph_text_admission_cause(cause.into()));
    }

    fn apply_guarded_graph_text_identity_path(
        &self,
        current: &mut CompleteGraphTextAdmissionIndex,
        relative: String,
        decode_semantics: bool,
    ) -> io::Result<()> {
        let event_scratch = graph_text_event_scratch_upper_bound(&relative)?;
        ensure_graph_text_peak_limit(current.permanent_bytes, event_scratch, current.peak_limit)?;
        let mut charges = GraphTextExactFeedBatchActualCharges::default();
        let final_state = self.prepare_graph_text_admission_final_state(
            current,
            relative.clone(),
            event_scratch,
            &mut charges,
            false,
            decode_semantics,
        )?;
        let retained_growth = match &final_state {
            PreparedGraphTextAdmissionFinalState::Present(prepared) => prepared.retained_growth,
            PreparedGraphTextAdmissionFinalState::Absent(prepared) => prepared.retained_growth,
        };
        charges.retain_prepared_growth(current, event_scratch, retained_growth)?;
        self.revalidate_prepared_graph_text_admission_batch(
            current,
            std::slice::from_ref(&final_state),
            event_scratch,
            retained_growth,
            false,
        )?;

        let structural_peak = graph_text_admission_delta_structural_peak(current)?;
        let payload_peak = match &final_state {
            PreparedGraphTextAdmissionFinalState::Present(prepared) => {
                graph_text_admission_delta_payload_peak(current, &relative, Some(prepared))?
            }
            PreparedGraphTextAdmissionFinalState::Absent(_) => {
                graph_text_admission_delta_payload_peak(current, &relative, None)?
            }
        };
        let peak = checked_add_bytes(current.permanent_bytes, retained_growth)
            .and_then(|bytes| checked_add_bytes(bytes, structural_peak))
            .and_then(|bytes| checked_add_bytes(bytes, payload_peak))
            .and_then(|bytes| checked_add_bytes(bytes, event_scratch))?;
        if peak > current.peak_limit {
            return Err(graph_text_capture_limit_error("peak build memory"));
        }

        match final_state {
            PreparedGraphTextAdmissionFinalState::Present(prepared) => {
                if let Err(delta_error) =
                    self.apply_prepared_graph_text_file_upsert(current, prepared)
                {
                    let detail = validate_graph_text_admission_index(current)
                        .err()
                        .map_or_else(
                            || "complete index remains internally valid".to_owned(),
                            |error| error.to_string(),
                        );
                    return Err(io::Error::new(
                        delta_error.kind(),
                        format!("{delta_error} at {relative}; {detail}"),
                    ));
                }
            }
            PreparedGraphTextAdmissionFinalState::Absent(prepared) => {
                self.apply_prepared_graph_text_file_remove(current, prepared)?;
            }
        }
        validate_graph_text_admission_delta(current, &relative)?;
        Ok(())
    }

    /// Publish exact final-state changes made by Tine while the same
    /// resource-wide identity authority still covers the filesystem mutation.
    pub(super) fn update_guarded_graph_text_identity_paths<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
        decode_semantics: bool,
    ) -> io::Result<()> {
        let identity = self.lock_graph_text_identity_mutation()?;
        let relatives = paths
            .into_iter()
            .map(|path| self.rel_path(path))
            .collect::<Vec<_>>();
        let prior_resource_epoch = identity.gate.identity_mutation_epoch_under_authority()?;
        // Advance before attempting publication. If the filesystem transition
        // was already committed and any later step fails, every sibling's older
        // epoch is still immediately unusable once this authority is released.
        let resource_epoch = identity.gate.advance_identity_mutation_epoch();
        let mut state = self.guarded_graph_text_identity.write().unwrap();
        if state.invalidated
            || state.index.is_none()
            || state.observed_resource_epoch != Some(prior_resource_epoch)
        {
            state.invalidated = true;
            state.invalidation_cause = Some(bounded_graph_text_admission_cause(
                "exact identity transition could not update a current retained index".to_owned(),
            ));
            return Ok(());
        }
        #[cfg(test)]
        if FAIL_NEXT_GUARDED_GRAPH_TEXT_IDENTITY_UPDATE.with(|fail| fail.replace(false)) {
            state.invalidated = true;
            state.invalidation_cause =
                Some("injected guarded graph-text identity publication failure".to_owned());
            return Err(io::Error::other(
                "injected guarded graph-text identity publication failure",
            ));
        }
        let generation = state.generation.checked_add(1).ok_or_else(|| {
            graph_text_admission_unavailable("guarded graph-text identity generation overflow")
        })?;
        let next = Arc::make_mut(
            state
                .index
                .as_mut()
                .expect("checked retained guarded identity"),
        );
        let update_result = (|| {
            for relative in relatives {
                if self.classify_graph_text_exact_feed_path(&relative)?
                    != GraphTextExactFeedPathClass::RetainedFile
                {
                    continue;
                }
                self.apply_guarded_graph_text_identity_path(next, relative, decode_semantics)?;
            }
            Ok::<(), io::Error>(())
        })();
        if let Err(error) = update_result {
            state.invalidated = true;
            state.invalidation_cause = Some(bounded_graph_text_admission_cause(format!(
                "exact identity update failed: {error}"
            )));
            return Err(error);
        }
        next.generation = generation;
        state.generation = generation;
        state.observed_resource_epoch = Some(resource_epoch);
        state.invalidated = false;
        state.invalidation_cause = None;
        {
            state.exact_updates = state.exact_updates.saturating_add(1);
        }
        Ok(())
    }

    pub(super) fn finish_tine_owned_graph_text_identity_paths<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
    ) -> io::Result<()> {
        let paths = paths.into_iter().map(Path::to_path_buf).collect::<Vec<_>>();
        for path in &paths {
            self.revoke_conflict_authority(path);
        }
        let _ = self
            .update_guarded_graph_text_identity_paths(paths.iter().map(PathBuf::as_path), false);
        // The filesystem transition is already durable. Returning an error
        // here would report failure for a committed edit and strand cache state
        // behind disk; invalidation is the fail-closed recovery boundary.
        Ok(())
    }

    /// Legacy watcher intake boundary for the retained guarded-save index.
    ///
    /// The Tauri watcher calls this directly from the platform callback, before
    /// its 200 ms coalescing delay. Exact file paths update the retained final
    /// state under the same resource-wide authority as Tine writes. Overflow,
    /// notify errors, directory/configuration events, poll cycles, and any
    /// ambiguous path invalidate the generation. Missing-target creation stays
    /// blocked from this callback until the debounced reconciler acknowledges
    /// the observed epoch; an existing exact-owner save uses its retained
    /// path-local and single-link proofs instead.
    pub fn observe_graph_text_external_paths<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a Path>,
        uncertain: bool,
    ) -> io::Result<()> {
        let _identity = self.lock_graph_text_identity_mutation()?;
        let paths = paths.into_iter().map(Path::to_path_buf).collect::<Vec<_>>();
        if uncertain {
            self.revoke_all_conflict_authority();
            self.invalidate_guarded_graph_text_identity("external watcher generation is uncertain");
            return Ok(());
        }
        for path in &paths {
            self.revoke_conflict_authority(path);
        }
        let _ =
            self.update_guarded_graph_text_identity_paths(paths.iter().map(PathBuf::as_path), true);
        Ok(())
    }

    /// Record a relevant raw watcher callback without touching graph bytes.
    /// Returns the epoch the callback published.
    pub fn note_graph_text_external_observation(&self) -> GraphTextExternalObservationTicket {
        // A native callback may arrive after an atomic temporary-name event but
        // before the publishing writer has completed its final reread and
        // ownership receipt.  Raising the graph-wide frontier before taking the
        // writer's identity authority makes that writer reject its own create
        // (the Windows failure reported again in GH #374).  Linearize the epoch
        // itself with graph-text publication: a callback which overlaps a Tine
        // write waits, then the ordinary debounced reconciliation decides
        // whether anything external actually changed.
        //
        // Failing to acquire a binding is already a fail-closed state for every
        // writer.  Keep publishing the epoch in that case so the watcher cannot
        // accidentally make the situation less conservative.
        let _identity = self.lock_graph_text_identity_mutation().ok();
        let epoch = self
            .external_observation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            .saturating_add(1);
        GraphTextExternalObservationTicket {
            instance: self.external_observation_instance,
            epoch,
        }
    }

    /// Snapshot the raw-event frontier represented by one drained watcher batch.
    pub fn graph_text_external_observation_ticket(&self) -> GraphTextExternalObservationTicket {
        GraphTextExternalObservationTicket {
            instance: self.external_observation_instance,
            epoch: self
                .external_observation_epoch
                .load(std::sync::atomic::Ordering::Acquire),
        }
    }

    /// Admit exactly the drained frontier after successful reconciliation. A
    /// newer callback cannot be accidentally acknowledged by an older batch,
    /// and a same-root replacement cannot consume its predecessor's ticket.
    pub fn acknowledge_graph_text_external_observations(
        &self,
        ticket: GraphTextExternalObservationTicket,
    ) -> bool {
        if ticket.instance != self.external_observation_instance {
            return false;
        }
        self.external_reconciled_epoch
            .fetch_max(ticket.epoch, std::sync::atomic::Ordering::AcqRel);
        true
    }

    pub fn owns_graph_text_external_observation_ticket(
        &self,
        ticket: GraphTextExternalObservationTicket,
    ) -> bool {
        ticket.instance == self.external_observation_instance
    }

    pub(super) fn graph_text_external_observation_pending(&self) -> bool {
        self.external_observation_epoch
            .load(std::sync::atomic::Ordering::Acquire)
            != self
                .external_reconciled_epoch
                .load(std::sync::atomic::Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn guarded_graph_text_identity_stats(&self) -> (usize, usize, bool, u64) {
        let report = self.guarded_graph_text_identity_report();
        (
            report.complete_builds,
            report.exact_updates,
            report.invalidated,
            report.generation,
        )
    }

    /// Always available, including in a release build. The whole point is that a
    /// user reporting a slow save can be answered from the binary they are
    /// running, without a debug build or an environment variable. Carries
    /// durations and counts only -- no paths, no content.
    pub fn guarded_graph_text_identity_report(&self) -> GuardedGraphTextIdentityReport {
        let state = self.guarded_graph_text_identity.read().unwrap();
        GuardedGraphTextIdentityReport {
            complete_builds: state.complete_builds,
            exact_updates: state.exact_updates,
            invalidated: state.invalidated,
            generation: state.generation,
            last_build: state.last_build,
        }
    }

    /// Run one bounded, multi-document Tine operation without allowing a raw
    /// native watcher callback to split it between two graph-text
    /// publications. Individual writes still take their ordinary
    /// path locks and perform all exact validation; this only keeps their shared
    /// resource authority contiguous (the Guide copy is the first caller).
    pub(crate) fn with_graph_text_write_transaction<T>(
        &self,
        operation: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        let _write = self.admit_graph_text_writer()?;
        let _identity = self.lock_graph_text_identity_mutation()?;
        operation()
    }

    #[cfg(test)]
    pub(super) fn guarded_graph_text_identity_epochs(&self) -> (Option<u64>, u64) {
        let observed = self
            .guarded_graph_text_identity
            .read()
            .unwrap()
            .observed_resource_epoch;
        let resource = self
            .graph_text_write_binding()
            .expect("test graph has graph writer binding")
            .gate
            .identity_mutation_epoch();
        (observed, resource)
    }
}
