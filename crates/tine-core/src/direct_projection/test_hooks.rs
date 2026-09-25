//! Test hooks on [`DirectProjection`]: the waits, counters and injected
//! failures fixtures use to observe and steer the worker. Test-only, kept
//! out of `direct_projection.rs` so the production file stays readable.
use super::owner::index_need;
use super::*;

impl DirectProjection {
    /// Wait until the worker has drained its queue and finished its turn, and
    /// report whether that turn succeeded (`false`: it failed). An index that
    /// has given up for the session (GH #594, L1) has nothing more coming, so
    /// it counts as drained, and failed.
    #[cfg(test)]
    #[must_use = "a readiness wait that timed out must fail the test or be handled (GH #543, R9-15e)"]
    pub(crate) fn wait_drained_test(&self) -> bool {
        let started = std::time::Instant::now();
        loop {
            {
                let pending = self.shared.pending.lock().unwrap();
                let idle = !self.shared.worker_busy.load(Ordering::Acquire);
                if idle && pending.failed.is_some() {
                    return false;
                }
                if !pending.has_work() && idle {
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
            "ready={} validated={} ready_generation={} latest_generation={} floor={} full={} marks={} registry_owed={} in_flight={} applied={} rebuild={} building={} stop={} worker_available={} worker_failed={} worker_busy={} need={:?} backing_off={} turn_failed={}",
            self.shared.ready.load(Ordering::Acquire),
            self.shared.validated.load(Ordering::Acquire),
            self.shared.ready_generation.load(Ordering::Acquire),
            pending.latest_generation,
            pending.floor,
            pending.full.is_some(),
            pending.marks.len(),
            pending.registry_owed.is_some(),
            pending.in_flight.len(),
            pending.applied.len(),
            pending.rebuild,
            pending.building,
            pending.stop,
            self.shared.worker_available.load(Ordering::Acquire),
            self.shared.worker_failed.load(Ordering::Acquire),
            self.shared.worker_busy.load(Ordering::Acquire),
            index_need(&self.shared, &pending),
            backing_off(&pending),
            self.shared.last_turn_failed.load(Ordering::Acquire),
        )
    }

    /// Owe the image a survey again, as if many files had changed behind the
    /// graph with no event naming them. Nothing in the app does this: every
    /// change reaches the graph by path (audit R11-08).
    #[cfg(test)]
    /// The image is validated but not ready: what a queued mark leaves.
    pub(crate) fn unready_test(&self) {
        let _pending = self.shared.pending.lock().unwrap();
        self.shared.ready.store(false, Ordering::Release);
    }

    pub(crate) fn owe_validation_test(&self) {
        let _pending = self.shared.pending.lock().unwrap();
        self.shared.validated.store(false, Ordering::Release);
        self.shared.ready.store(false, Ordering::Release);
        drop(_pending);
        self.shared.changed.notify_all();
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
            .fetch_max(1, Ordering::AcqRel);
    }

    /// Fail the worker's next `turns` turns as well as any already owed: a
    /// fault that outlasts the retries (GH #594 L6).
    pub(crate) fn inject_turn_failures_test(&self, turns: u32) {
        self.shared
            .inject_turn_failure
            .fetch_add(turns, Ordering::AcqRel);
    }

    /// Whether an injected turn failure is still waiting for a turn.
    pub(crate) fn turn_failure_injection_pending_test(&self) -> bool {
        self.shared.inject_turn_failure.load(Ordering::Acquire) > 0
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
        *self.shared.after_lowering_batch.lock().unwrap() = Some(hook);
    }

    /// Whether the worker's current turn is a fresh build.
    #[cfg(test)]
    pub(crate) fn fresh_build_running_test(&self) -> bool {
        self.shared.fresh_build_running.load(Ordering::Acquire)
    }

    /// From-scratch builds this index has started.
    #[cfg(test)]
    pub(crate) fn fresh_builds_test(&self) -> u64 {
        self.shared.fresh_builds.load(Ordering::SeqCst)
    }

    /// Fail every fresh build just before it publishes, with `message`: a
    /// deterministic lowering failure (audit R15-08). Persistent rather than
    /// re-armed, so no build can slip through between two arms under load.
    #[cfg(test)]
    pub(crate) fn fail_every_fresh_publication_test(&self, message: &str) {
        *self.shared.fresh_publication_failure.lock().unwrap() = Some(message.to_owned());
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

impl DirectProjection {
    /// Background integrity checks started so far.
    pub(crate) fn integrity_checks_started_test(&self) -> u64 {
        self.shared.integrity_checks_started.load(Ordering::Relaxed)
    }

    /// Hold the next background integrity check before it opens the image:
    /// `.0` is reached, `.1` releases it.
    pub(crate) fn pause_next_integrity_check_test(
        &self,
    ) -> Arc<(std::sync::Barrier, std::sync::Barrier)> {
        let pause = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
        *self.shared.integrity_check_pause.lock().unwrap() = Some(Arc::clone(&pause));
        pause
    }

    /// The next background integrity check finds the image damaged.
    pub(crate) fn inject_integrity_damage_test(&self) {
        self.shared
            .inject_integrity_damage
            .store(true, Ordering::Release);
    }

    /// Whether a background integrity check is running.
    pub(crate) fn integrity_running_test(&self) -> bool {
        self.shared.pending.lock().unwrap().integrity_running
    }

    /// Run the integrity check on this thread; whether the image is intact.
    pub(crate) fn run_integrity_check_test(&self) -> bool {
        super::integrity::run_check_now(&self.shared)
    }

    /// The reopen's question: was every stored fact written under `config`?
    pub(crate) fn stored_facts_are_test(&self, config: &ParseConfig) -> bool {
        super::derived_reads::stored_facts_are(
            &self.shared.path,
            &projection_source_revision("", config.digest()),
        )
    }
}
