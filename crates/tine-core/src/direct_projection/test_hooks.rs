//! Test hooks on [`DirectProjection`]: the waits, counters and injected
//! failures fixtures use to observe and steer the worker. Test-only, kept
//! out of `direct_projection.rs` so the production file stays readable.
use super::*;

impl DirectProjection {
    /// Wait until the worker has drained its queue and finished its turn, and
    /// report whether that turn succeeded (`false`: it failed).
    #[cfg(test)]
    #[must_use = "a readiness wait that timed out must fail the test or be handled (GH #543, R9-15e)"]
    pub(crate) fn wait_drained_test(&self) -> bool {
        let started = std::time::Instant::now();
        loop {
            {
                let pending = self.shared.pending.lock().unwrap();
                if !pending.has_work() && !self.shared.worker_busy.load(Ordering::Acquire) {
                    return !self.shared.last_turn_failed.load(Ordering::Acquire);
                }
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(15),
                "projection worker did not drain"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Test diagnostic: the queue and readiness state in one line, for a
    /// convergence failure that would otherwise be a bare timeout.
    #[cfg(test)]
    pub(crate) fn debug_state_test(&self) -> String {
        let pending = self.shared.pending.lock().unwrap();
        format!(
            "ready={} validated={} ready_generation={} latest_generation={} full={} deltas={} warm={} warm_outcome={:?} rebuild={} stop={} page_order={} worker_available={} worker_failed={} worker_busy={} need={:?} revalidate={} turn_failed={} requires_full_rebuild={}",
            self.shared.ready.load(Ordering::Acquire),
            self.shared.validated.load(Ordering::Acquire),
            self.shared.ready_generation.load(Ordering::Acquire),
            pending.latest_generation,
            pending.full.is_some(),
            pending.deltas.len(),
            pending.warm.is_some(),
            pending.warm_outcome.as_ref().map(|(_, outcome)| match outcome {
                WarmOutcome::Clean => "Clean".to_owned(),
                WarmOutcome::FreshBuildRequired => "FreshBuildRequired".to_owned(),
                WarmOutcome::Changed {
                    replacements,
                    deletions,
                } => format!(
                    "Changed(replacements={}, deletions={})",
                    replacements.len(),
                    deletions.len()
                ),
                WarmOutcome::Superseded => "Superseded".to_owned(),
                WarmOutcome::Failed => "Failed".to_owned(),
            }),
            pending.rebuild,
            pending.stop,
            pending.page_order.len(),
            self.shared.worker_available.load(Ordering::Acquire),
            self.shared.worker_failed.load(Ordering::Acquire),
            self.shared.worker_busy.load(Ordering::Acquire),
            index_need(&self.shared, &pending),
            pending.revalidate,
            self.shared.last_turn_failed.load(Ordering::Acquire),
            pending.requires_full_rebuild,
        )
    }

    #[cfg(test)]
    pub(crate) fn indexed_reads(&self) -> u64 {
        self.shared.indexed_reads.load(Ordering::Relaxed)
    }

    /// Close this projection's query-job admission, the way `Drop` does when a
    /// graph is closing. Every later `open_query_job` is `Cancelled`, which is
    /// the ONE §5.9 state a public query must never repair or retry.
    #[cfg(test)]
    pub(crate) fn close_query_jobs_test(&self) {
        let fence = self.shared.query_jobs.begin_close();
        self.shared.query_jobs.wait_for_drain(fence);
    }

    #[cfg(test)]
    pub(crate) fn inject_next_turn_failure_test(&self) {
        self.shared
            .inject_turn_failure
            .store(true, Ordering::Release);
    }

    /// Refuse the next statement on an intact image, as SQLite refuses a
    /// statement past one of its hard limits.
    #[cfg(test)]
    pub(crate) fn inject_next_statement_refusal(&self) {
        self.shared
            .inject_read_failure
            .store(true, Ordering::Release);
    }

    /// The next failed read finds the stored image damaged.
    #[cfg(test)]
    pub(crate) fn inject_image_damage_test(&self) {
        self.shared
            .inject_image_damage
            .store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn inject_next_statement_failure(&self) {
        self.shared
            .inject_read_failure
            .store(true, Ordering::Release);
        self.shared
            .inject_image_damage
            .store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn statement_reads(&self) -> u64 {
        self.shared.statement_reads.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn take_registry_capture_attempts(&self) -> u64 {
        self.shared
            .registry_capture_attempts
            .swap(0, Ordering::AcqRel)
    }

    #[cfg(test)]
    pub(crate) fn fallback_reads(&self) -> u64 {
        self.shared.fallback_reads.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn referenced_name_reads(&self) -> u64 {
        self.shared.referenced_name_reads.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn serving_writer_cache_budget_test(&self) -> u64 {
        self.shared
            .serving_writer_cache_budget
            .load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn reset_projection_health_checks_test(&self) {
        self.shared
            .projection_health_checks
            .store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn projection_health_checks_test(&self) -> u64 {
        self.shared.projection_health_checks.load(Ordering::Relaxed)
    }

    /// Run `hook` on the worker once, after the next lowering batch it writes.
    #[cfg(test)]
    pub(crate) fn after_next_lowering_batch_test(&self, hook: Box<dyn FnOnce() + Send>) {
        *self.shared.after_fresh_build_batch.lock().unwrap() = Some(hook);
    }

    /// From-scratch builds this index has started.
    #[cfg(test)]
    pub(crate) fn fresh_builds_test(&self) -> u64 {
        self.shared.fresh_builds.load(Ordering::SeqCst)
    }

    /// Hold the next fresh build just before it publishes: the first barrier
    /// releases when it arrives, the second lets it go on.
    #[cfg(test)]
    pub(crate) fn hold_fresh_publication_test(
        &self,
    ) -> Arc<(std::sync::Barrier, std::sync::Barrier)> {
        let pair = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
        let held = Arc::clone(&pair);
        *self.shared.before_fresh_publication.lock().unwrap() = Some(Box::new(move || {
            held.0.wait();
            held.1.wait();
            Ok(())
        }));
        pair
    }
}
