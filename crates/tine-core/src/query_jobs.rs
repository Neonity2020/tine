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

use std::collections::BTreeSet;
use std::ops::Deref;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tine_storage::sqlite::PhysicalProjectionQueryCancellation;

/// Plan §2B's default: two active query jobs per graph.
pub(crate) const DEFAULT_QUERY_JOB_CAPACITY: usize = 2;

/// How long a job waits for a slot before it gives up. A slot that stays busy
/// this long produces typed busy readiness; it never selects a different
/// evaluator.
pub(crate) const QUERY_JOB_WAIT: Duration = Duration::from_secs(30);

struct JobState {
    /// Slots currently held (admitted jobs, whether or not they have opened
    /// their snapshot yet).
    active: BTreeSet<u64>,
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
    #[cfg(test)]
    drain_waiting_started: Option<std::sync::mpsc::Sender<()>>,
    #[cfg(test)]
    before_release: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}

pub(crate) struct QueryJobOwner {
    state: Mutex<JobState>,
    changed: Condvar,
    capacity: usize,
}

/// The admission generation captured with immutable query inputs. Ordinary
/// edits do not change it; projection lifecycle drains do.
#[derive(Clone, Copy, Debug)]
pub(crate) struct QueryJobEpoch(u64);

/// A cancellation boundary whose completion can be awaited outside the actor.
/// Includes admitted jobs which have not opened or registered a snapshot yet.
#[derive(Clone, Copy)]
pub(crate) struct QueryDrainFence(u64);

/// The outcome of asking for a slot.
pub(crate) enum QueryAdmission<O: Deref<Target = QueryJobOwner>> {
    Slot(QueryJobLease<O>),
    /// The owner is closed, or a drain ran while this job waited.
    Cancelled,
    /// No slot freed within [`QUERY_JOB_WAIT`].
    Busy,
}

/// One held capacity slot. Dropping it releases the slot exactly once and
/// unregisters the snapshot's interrupt handle, on every completion, error and
/// cancellation path — there is no other release.
pub(crate) struct QueryJobLease<O: Deref<Target = QueryJobOwner>> {
    owner: O,
    id: u64,
}

pub(crate) type Admission<'a> = QueryAdmission<&'a QueryJobOwner>;
pub(crate) type OwnedAdmission = QueryAdmission<Arc<QueryJobOwner>>;
pub(crate) type JobSlot<'a> = QueryJobLease<&'a QueryJobOwner>;
pub(crate) type OwnedJobSlot = QueryJobLease<Arc<QueryJobOwner>>;

impl QueryJobOwner {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(JobState {
                active: BTreeSet::new(),
                next_id: 1,
                cancelled_below: 0,
                handles: Vec::new(),
                closed: false,
                drain_epoch: 0,
                #[cfg(test)]
                waiting_started: None,
                #[cfg(test)]
                drain_waiting_started: None,
                #[cfg(test)]
                before_release: None,
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
        self.acquire_at_within(self.capture_epoch(), wait)
    }

    pub(crate) fn capture_epoch(&self) -> QueryJobEpoch {
        QueryJobEpoch(self.state.lock().unwrap().drain_epoch)
    }

    /// Refuse captures invalidated before their worker began waiting as well
    /// as jobs invalidated while queued. No transaction is opened here.
    pub(crate) fn acquire_at_within(&self, epoch: QueryJobEpoch, wait: Duration) -> Admission<'_> {
        Self::acquire_lease_at(self, epoch, wait)
    }

    /// An owned lease can travel with an admitted producer capture. It shares
    /// the borrowed lease's exact capacity, epoch, registration and Drop path.
    pub(crate) fn acquire_owned_at_within(
        self: &Arc<Self>,
        epoch: QueryJobEpoch,
        wait: Duration,
    ) -> OwnedAdmission {
        Self::acquire_lease_at(Arc::clone(self), epoch, wait)
    }

    fn acquire_lease_at<O: Deref<Target = Self>>(
        owner: O,
        epoch: QueryJobEpoch,
        wait: Duration,
    ) -> QueryAdmission<O> {
        let deadline = Instant::now() + wait;
        let mut state = owner.state.lock().unwrap();
        loop {
            if state.closed || state.drain_epoch != epoch.0 {
                return QueryAdmission::Cancelled;
            }
            if state.active.len() < owner.capacity {
                let id = state.next_id;
                state.next_id += 1;
                state.active.insert(id);
                drop(state);
                return QueryAdmission::Slot(QueryJobLease { owner, id });
            }
            let now = Instant::now();
            if now >= deadline {
                return QueryAdmission::Busy;
            }
            #[cfg(test)]
            if let Some(started) = state.waiting_started.take() {
                started.send(()).unwrap();
            }
            let (next, _) = owner.changed.wait_timeout(state, deadline - now).unwrap();
            state = next;
        }
    }

    /// Cancel the currently admitted and queued work without waiting for it.
    /// Captures made after this boundary belong to a new admission epoch.
    pub(crate) fn begin_drain(&self) -> QueryDrainFence {
        let mut state = self.state.lock().unwrap();
        state.cancelled_below = state.next_id;
        state.drain_epoch += 1;
        for (_, cancellation) in &state.handles {
            cancellation.cancel();
        }
        self.changed.notify_all();
        QueryDrainFence(state.cancelled_below)
    }

    /// Wait only for work covered by this fence. Never cancels newer work,
    /// and newer admissions cannot prolong this wait.
    pub(crate) fn wait_for_drain(&self, fence: QueryDrainFence) {
        let mut state = self.state.lock().unwrap();
        while state.active.range(..fence.0).next().is_some() {
            #[cfg(test)]
            if let Some(started) = state.drain_waiting_started.take() {
                started.send(()).unwrap();
            }
            state = self.changed.wait(state).unwrap();
        }
    }

    /// Cancel current work and retain the existing full-idle replacement
    /// barrier. Callers using the split fence API instead control admission
    /// to the replacement while its old readers drain off the actor.
    pub(crate) fn cancel_all_and_drain(&self) {
        let fence = self.begin_drain();
        self.wait_for_drain(fence);
        let mut state = self.state.lock().unwrap();
        while !state.active.is_empty() {
            state = self.changed.wait(state).unwrap();
        }
    }

    /// Refuse every future admission, then drain. Used when the graph closes.
    pub(crate) fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.cancel_all_and_drain();
    }

    #[cfg(test)]
    pub(crate) fn pause_next_release_for_test(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, observed) = std::sync::mpsc::channel();
        let (resume, proceed) = std::sync::mpsc::channel();
        self.state.lock().unwrap().before_release = Some((reached, proceed));
        (observed, resume)
    }

    #[cfg(test)]
    pub(crate) fn active(&self) -> usize {
        self.state.lock().unwrap().active.len()
    }
}

impl<O: Deref<Target = QueryJobOwner>> QueryJobLease<O> {
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

    /// Whether a drain has cancelled this job since it was admitted.
    /// Snapshot statements also observe a sticky cancellation flag. This final
    /// slot check covers cancellation after the last statement has completed.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.id < self.owner.state.lock().unwrap().cancelled_below
    }
}

impl<O: Deref<Target = QueryJobOwner>> Drop for QueryJobLease<O> {
    fn drop(&mut self) {
        #[cfg(test)]
        {
            let pause = self.owner.state.lock().unwrap().before_release.take();
            if let Some((reached, proceed)) = pause {
                reached.send(()).unwrap();
                proceed.recv().unwrap();
            }
        }
        let mut state = self.owner.state.lock().unwrap();
        state.handles.retain(|(id, _)| *id != self.id);
        state.active.remove(&self.id);
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

    fn owned(owner: &Arc<QueryJobOwner>) -> OwnedJobSlot {
        match owner.acquire_owned_at_within(owner.capture_epoch(), Duration::ZERO) {
            OwnedAdmission::Slot(slot) => slot,
            _ => panic!("owned admission"),
        }
    }

    #[test]
    fn owned_and_borrowed_leases_share_capacity_and_drain_fences() {
        let owner = Arc::new(QueryJobOwner::new(2));
        let old_owned = owned(&owner);
        let old_borrowed = match owner.acquire() {
            Admission::Slot(slot) => slot,
            _ => panic!("borrowed admission"),
        };
        assert!(matches!(
            owner.acquire_owned_at_within(owner.capture_epoch(), Duration::ZERO),
            OwnedAdmission::Busy
        ));
        let old_epoch = owner.capture_epoch();
        let fence = owner.begin_drain();
        assert!(old_owned.is_cancelled());
        assert!(old_borrowed.is_cancelled());
        assert!(matches!(
            owner.acquire_owned_at_within(old_epoch, Duration::ZERO),
            OwnedAdmission::Cancelled
        ));
        drop(old_borrowed);
        let new_owned = owned(&owner);
        let (waiting, observed) = mpsc::channel();
        owner.state.lock().unwrap().drain_waiting_started = Some(waiting);
        let drain_owner = Arc::clone(&owner);
        let (done, finished) = mpsc::channel();
        let drainer = std::thread::spawn(move || {
            drain_owner.wait_for_drain(fence);
            done.send(()).unwrap();
        });
        observed.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(matches!(
            finished.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        std::thread::spawn(move || drop(old_owned)).join().unwrap();
        finished.recv_timeout(Duration::from_secs(3)).unwrap();
        drainer.join().unwrap();
        assert_eq!(owner.active(), 1);
        assert!(!new_owned.is_cancelled());
        drop(new_owned);
        assert_eq!(owner.active(), 0);
    }

    #[test]
    fn owned_lease_keeps_owner_alive_across_thread_transfer() {
        let owner = Arc::new(QueryJobOwner::new(1));
        let weak = Arc::downgrade(&owner);
        let slot = owned(&owner);
        drop(owner);
        assert!(weak.upgrade().is_some());
        std::thread::spawn(move || drop(slot)).join().unwrap();
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn owned_lease_cancels_late_snapshot_registration_and_releases_once() {
        let owner = Arc::new(QueryJobOwner::new(1));
        let slot = owned(&owner);
        let fence = owner.begin_drain();
        let path = fixture_projection();
        let mut snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
        assert!(!slot.register(snapshot.cancellation()));
        assert!(snapshot.cancellation().is_cancelled());
        assert!(snapshot.run_projection_query("SELECT 1", &[]).is_err());
        drop(snapshot);
        drop(slot);
        owner.wait_for_drain(fence);
        assert_eq!(owner.active(), 0);
        let new = owned(&owner);
        assert!(!new.is_cancelled());
        drop(new);
        owner.close();
        assert!(matches!(
            owner.acquire_owned_at_within(owner.capture_epoch(), Duration::ZERO),
            OwnedAdmission::Cancelled
        ));
        std::fs::remove_file(path).unwrap();
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
        owner2.begin_drain();
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

    #[test]
    fn a_drain_fence_waits_for_unregistered_old_slots_but_not_new_admissions() {
        let owner = QueryJobOwner::new(2);
        let Admission::Slot(old) = owner.acquire() else {
            panic!("old admission")
        };
        let fence = owner.begin_drain();
        assert!(old.is_cancelled());
        let Admission::Slot(new) = owner.acquire() else {
            panic!("new admission")
        };
        let (finished, received) = mpsc::channel();
        let (waiting, started) = mpsc::channel();
        owner.state.lock().unwrap().drain_waiting_started = Some(waiting);
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                owner.wait_for_drain(fence);
                finished.send(()).unwrap();
            });
            let waited_for_old = started.recv_timeout(Duration::from_secs(5)).is_ok();
            let not_finished_early = received.try_recv().is_err();
            drop(old);
            let completes_with_new_held = received.recv_timeout(Duration::from_secs(5)).is_ok();
            let new_is_live = !new.is_cancelled();
            // Release before assertions, including on failure, so the control
            // cannot deadlock the scoped waiter on the new slot.
            drop(new);
            waiter.join().unwrap();
            assert!(
                waited_for_old && not_finished_early,
                "old unregistered slot was not drained"
            );
            assert!(completes_with_new_held, "new work prolonged the old fence");
            assert!(new_is_live, "waiting on the old fence cancelled new work");
        });
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
