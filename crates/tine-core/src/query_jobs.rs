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
//! Direct Files composes one in its projection. There is no second admission
//! policy to disagree with this one (D-14).
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

pub(crate) type OwnedAdmission = QueryAdmission<Arc<QueryJobOwner>>;
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

    pub(crate) fn capture_epoch(&self) -> QueryJobEpoch {
        QueryJobEpoch(self.state.lock().unwrap().drain_epoch)
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

    /// Close admission before the producer rejects its queued captures. The
    /// caller drops those leases before waiting outside its queue lock.
    pub(crate) fn begin_close(&self) -> QueryDrainFence {
        self.state.lock().unwrap().closed = true;
        self.begin_drain()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(owner: &Arc<QueryJobOwner>) -> OwnedJobSlot {
        match owner.acquire_owned_at_within(owner.capture_epoch(), Duration::ZERO) {
            OwnedAdmission::Slot(slot) => slot,
            _ => panic!("owned admission"),
        }
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
}
