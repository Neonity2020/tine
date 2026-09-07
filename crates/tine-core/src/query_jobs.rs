//! The ONE query-job owner (query-engine campaign R3, plan §2B).
//!
//! A database-owned query holds an owned read snapshot of the projection — a
//! pinned SQLite read transaction on its own connection — for as long as it
//! selects descriptors and reads admitted payload. Two things bound that:
//!
//! * **Capacity is acquired BEFORE a transaction is opened.** A job that is
//!   waiting for a slot holds its request intent and nothing else; it does not
//!   pin WAL pages while it queues. The default is two active jobs per graph,
//!   which is the plan's number and enough for a page render that fans out a
//!   handful of `{{query}}` blocks while an editor save keeps landing.
//! * **Every job is cancellable and the owner can drain them.** Before the
//!   projection worker replaces or resets the disposable file, and when the
//!   graph closes, `cancel_all_and_drain` interrupts every active statement
//!   (through the snapshot's SQLite interrupt handle), wakes every waiter with
//!   `Cancelled`, and blocks until no job holds a slot — so no reader retains a
//!   handle to a file that is about to be removed, and a rebuild never waits on
//!   a snapshot nobody will finish. This is the in-scope scenario (D-3, I-8): a
//!   torn projection being rebuilt under a live reader, not a hostile one.
//!
//! The owner is backend-agnostic on purpose: Direct Files composes one in its
//! projection today (R3); the Managed Storage actor composes the same type in
//! R4. There is no second admission policy to disagree with this one (D-14).
//!
//! Lock discipline: the owner's mutex guards only its own bookkeeping and is
//! never held while SQLite runs or while any graph/actor lock is taken. A job
//! runs on its caller's thread (Tauri's `spawn_blocking` for Direct commands);
//! the owner never spawns.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use tine_storage::sqlite::PhysicalProjectionQueryCancellation;

/// Plan §2B's default: two active query jobs per graph.
pub(crate) const DEFAULT_QUERY_JOB_CAPACITY: usize = 2;

/// How long a job waits for a slot before it gives up. A slot that stays busy
/// this long is a statement that should have been interrupted, not a queue
/// worth extending; the caller answers through its recovery path (the walk)
/// and counts the fallback, exactly as it does for a not-ready projection.
pub(crate) const QUERY_JOB_WAIT: Duration = Duration::from_secs(30);

struct JobState {
    /// Slots currently held (admitted jobs, whether or not they have opened
    /// their snapshot yet).
    active: usize,
    /// Monotonic admission counter; also the id space for `handles`.
    next_id: u64,
    /// Every job admitted with `id < cancelled_below` is cancelled. A job that
    /// was admitted before a drain but registers its snapshot after it reads
    /// this on registration and is cancelled on the spot, so a drain can never
    /// be raced by a snapshot opened "just after".
    cancelled_below: u64,
    /// The interrupt handles of jobs that have opened their snapshot.
    handles: Vec<(u64, PhysicalProjectionQueryCancellation)>,
    /// Set once by `close`; every later admission is refused.
    closed: bool,
    /// Bumped by every drain so a waiter admitted across one learns it was
    /// cancelled instead of taking the slot the drain just freed.
    drain_epoch: u64,
    #[cfg(test)]
    waiting_started: Option<std::sync::mpsc::Sender<()>>,
}

pub(crate) struct QueryJobOwner {
    state: Mutex<JobState>,
    changed: Condvar,
    capacity: usize,
}

/// The outcome of asking for a slot.
pub(crate) enum Admission<'a> {
    Slot(JobSlot<'a>),
    /// The owner is closed, or a drain ran while this job waited.
    Cancelled,
    /// No slot freed within [`QUERY_JOB_WAIT`].
    Busy,
}

/// One held capacity slot. Dropping it releases the slot exactly once and
/// unregisters the snapshot's interrupt handle, on every completion, error and
/// cancellation path — there is no other release.
pub(crate) struct JobSlot<'a> {
    owner: &'a QueryJobOwner,
    id: u64,
}

impl QueryJobOwner {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(JobState {
                active: 0,
                next_id: 1,
                cancelled_below: 0,
                handles: Vec::new(),
                closed: false,
                drain_epoch: 0,
                #[cfg(test)]
                waiting_started: None,
            }),
            changed: Condvar::new(),
            capacity: capacity.max(1),
        }
    }

    /// Wait for a slot. The caller opens its snapshot only after this returns
    /// `Slot`, and registers the snapshot's cancellation with [`JobSlot::register`].
    pub(crate) fn acquire(&self) -> Admission<'_> {
        self.acquire_within(QUERY_JOB_WAIT)
    }

    pub(crate) fn acquire_within(&self, wait: Duration) -> Admission<'_> {
        let deadline = Instant::now() + wait;
        let mut state = self.state.lock().unwrap();
        let epoch = state.drain_epoch;
        loop {
            if state.closed || state.drain_epoch != epoch {
                return Admission::Cancelled;
            }
            if state.active < self.capacity {
                state.active += 1;
                let id = state.next_id;
                state.next_id += 1;
                return Admission::Slot(JobSlot { owner: self, id });
            }
            let now = Instant::now();
            if now >= deadline {
                return Admission::Busy;
            }
            #[cfg(test)]
            if let Some(started) = state.waiting_started.take() {
                started.send(()).unwrap();
            }
            let (next, _) = self.changed.wait_timeout(state, deadline - now).unwrap();
            state = next;
        }
    }

    /// Cancel every job — active and waiting — and block until no slot is held.
    /// Idempotent; safe to call with no jobs.
    pub(crate) fn cancel_all_and_drain(&self) {
        let mut state = self.state.lock().unwrap();
        state.cancelled_below = state.next_id;
        state.drain_epoch += 1;
        for (_, cancellation) in &state.handles {
            cancellation.cancel();
        }
        self.changed.notify_all();
        while state.active > 0 {
            state = self.changed.wait(state).unwrap();
        }
    }

    /// Refuse every future admission, then drain. Used when the graph closes.
    pub(crate) fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.cancel_all_and_drain();
    }

    #[cfg(test)]
    pub(crate) fn active(&self) -> usize {
        self.state.lock().unwrap().active
    }
}

impl JobSlot<'_> {
    /// Register the opened snapshot's interrupt handle so a drain can reach the
    /// statement it is running. Returns `false` — and cancels the handle — when
    /// a drain happened between admission and this call; the caller must treat
    /// that as `Cancelled` and not run the statement.
    pub(crate) fn register(&self, cancellation: PhysicalProjectionQueryCancellation) -> bool {
        let mut state = self.owner.state.lock().unwrap();
        // Ids below the drain's `next_id` were admitted before it; the first id
        // admitted after it is exactly `cancelled_below`, and must run.
        if self.id < state.cancelled_below {
            cancellation.cancel();
            return false;
        }
        state.handles.push((self.id, cancellation));
        true
    }

    /// Whether a drain has cancelled this job since it was admitted. The
    /// production read observes cancellation through the snapshot's own sticky
    /// flag (the next statement fails `Cancelled`); this is the slot's view,
    /// for the drain tests.
    #[cfg(test)]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.id < self.owner.state.lock().unwrap().cancelled_below
    }
}

impl Drop for JobSlot<'_> {
    fn drop(&mut self) {
        let mut state = self.owner.state.lock().unwrap();
        state.handles.retain(|(id, _)| *id != self.id);
        state.active -= 1;
        self.owner.changed.notify_all();
    }
}

/// A job owner shared between a projection and the readers it admits.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use tine_storage::sqlite::{PhysicalProjectionQuerySnapshot, PhysicalQueryValue};

    fn fixture_projection() -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("tine-query-jobs-{}.sqlite", uuid::Uuid::new_v4()));
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE payload (id INTEGER PRIMARY KEY, value TEXT NOT NULL);
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000)
                 INSERT INTO payload (id, value) SELECT i, 'v' || i FROM n;",
            )
            .unwrap();
        path
    }

    #[test]
    fn capacity_is_acquired_before_any_snapshot_and_released_exactly_once() {
        let owner = QueryJobOwner::new(2);
        let a = match owner.acquire() {
            Admission::Slot(slot) => slot,
            _ => panic!("first admission"),
        };
        let b = match owner.acquire() {
            Admission::Slot(slot) => slot,
            _ => panic!("second admission"),
        };
        assert_eq!(owner.active(), 2);
        assert!(matches!(
            owner.acquire_within(Duration::from_millis(20)),
            Admission::Busy
        ));
        drop(a);
        assert_eq!(owner.active(), 1);
        let c = match owner.acquire_within(Duration::from_millis(20)) {
            Admission::Slot(slot) => slot,
            _ => panic!("a freed slot is re-admitted"),
        };
        drop(b);
        drop(c);
        assert_eq!(owner.active(), 0);
    }

    #[test]
    fn drain_cancels_waiters_registered_jobs_and_late_registrations() {
        let owner = QueryJobOwner::new(1);
        let path = fixture_projection();
        let held = match owner.acquire() {
            Admission::Slot(slot) => slot,
            _ => panic!("admission"),
        };
        let snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
        assert!(held.register(snapshot.cancellation()));
        let cancelled = snapshot.cancellation();
        let released = AtomicBool::new(false);
        std::thread::scope(|scope| {
            // A waiter that will be woken by the drain, not by a freed slot.
            let (started, waiting) = mpsc::channel();
            // Signal under the admission lock after capturing the drain epoch.
            // A signal before acquire() only proves the thread was scheduled;
            // the whole drain could finish before it actually starts waiting.
            owner.state.lock().unwrap().waiting_started = Some(started);
            let owner = &owner;
            let waiter = scope.spawn(move || matches!(owner.acquire(), Admission::Cancelled));
            waiting.recv().unwrap();
            // The drain blocks until the held slot is released; release it
            // from another thread once the drain has cancelled the snapshot.
            let cancelled = &cancelled;
            let released = &released;
            let releaser = scope.spawn(move || {
                let started = Instant::now();
                while !cancelled.is_cancelled() {
                    assert!(
                        started.elapsed() < Duration::from_secs(5),
                        "drain never cancelled"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert_eq!(owner.active(), 1, "drain must wait for the held slot");
                released.store(true, Ordering::Release);
                drop(held);
            });
            owner.cancel_all_and_drain();
            assert!(
                released.load(Ordering::Acquire),
                "drain returned before the slot was released"
            );
            assert_eq!(owner.active(), 0);
            assert!(waiter.join().unwrap(), "the waiter must observe Cancelled");
            releaser.join().unwrap();
        });

        // A job admitted before the drain but registering after it is
        // cancelled on registration, so a snapshot opened "just after" cannot
        // outlive the file it was opened on.
        let owner2 = QueryJobOwner::new(2);
        let early = match owner2.acquire() {
            Admission::Slot(slot) => slot,
            _ => panic!("admission"),
        };
        owner2.cancel_all_and_drain_nonblocking_for_test();
        let late_snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
        let cancellation = late_snapshot.cancellation();
        assert!(!early.register(cancellation.clone()));
        assert!(cancellation.is_cancelled());
        assert!(early.is_cancelled());
        drop(early);
        // Admission beginning after a drain belongs to the new generation.
        assert!(matches!(owner2.acquire(), Admission::Slot(_)));
        let _ = std::fs::remove_file(&path);
    }

    impl QueryJobOwner {
        /// Test-only: mark every admitted job cancelled without waiting for the
        /// slots to be released.
        fn cancel_all_and_drain_nonblocking_for_test(&self) {
            let mut state = self.state.lock().unwrap();
            state.cancelled_below = state.next_id;
            state.drain_epoch += 1;
            for (_, cancellation) in &state.handles {
                cancellation.cancel();
            }
        }
    }

    #[test]
    fn a_drained_job_sees_its_running_statement_interrupted() {
        let owner = QueryJobOwner::new(1);
        let path = fixture_projection();
        let slot = match owner.acquire() {
            Admission::Slot(slot) => slot,
            _ => panic!("admission"),
        };
        let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
        assert!(slot.register(snapshot.cancellation()));
        std::thread::scope(|scope| {
            let (tx, rx) = mpsc::channel();
            let owner = &owner;
            let drainer = scope.spawn(move || {
                rx.recv().unwrap();
                owner.cancel_all_and_drain();
            });
            // A cross join large enough that the progress handler fires.
            let result = snapshot.visit_projection_query(
                "SELECT a.id FROM payload a, payload b WHERE a.id = b.id + 1",
                &[],
                |row| {
                    if let Some(PhysicalQueryValue::Integer(2)) = row.first() {
                        let _ = tx.send(());
                    }
                    Ok(std::ops::ControlFlow::Continue(()))
                },
            );
            assert!(result.is_err(), "the interrupted statement fails the read");
            assert!(slot.is_cancelled());
            drop(snapshot);
            drop(slot);
            drainer.join().unwrap();
        });
        assert_eq!(owner.active(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn close_refuses_every_later_admission() {
        let owner = QueryJobOwner::new(2);
        owner.close();
        assert!(matches!(owner.acquire(), Admission::Cancelled));
    }
}
