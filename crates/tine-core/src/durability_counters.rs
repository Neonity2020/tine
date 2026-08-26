//! Permanent primitive counters for durability barriers.
//!
//! A *durability barrier* is a syscall that forces data or metadata to stable
//! storage: `fsync` of a regular file, `fsync` of a directory, or `syncfs` of a
//! filesystem. Each one costs a real device round trip (~0.3 ms on the audit
//! box's ext4; plausibly hundreds of ms on a slow or network filesystem), so
//! the *number* of barriers an operation performs — not the time any one phase
//! reports — is the cost that scales with the user's hardware.
//!
//! The 2026-08-26 managed-storage cost-model audit found that one accepted
//! single-block managed save executed ~66 barriers against a blind budget of 3,
//! because durability was decided artifact by artifact and nobody ever saw the
//! per-operation sum. Phase timers cannot see multiplicity; counters can. These
//! counters therefore exist to make that sum a **testable budget** rather than
//! an invisible property — see
//! `sync_runtime::tests::managed_save_and_move_stay_within_their_barrier_budget`.
//!
//! ## What is counted, and what is not
//!
//! Every barrier `tine-core` itself initiates is counted, including the two
//! barriers that `tine_storage::publish_immutable_exact*` performs on the
//! caller's behalf: its documented contract is one file `fsync` plus one
//! directory `fsync` per published artifact, and
//! [`note_immutable_publication`] records exactly that pair.
//!
//! Barriers executed *inside* `tine-storage` on its own account — the local
//! journal's append `fsync`s, the SQLite VFS, and SQLite file-set publication —
//! are **not** counted, because they are not reachable from this crate without
//! a `tine-storage` API change. The audit measured that undercount at three
//! barriers per ordinary managed save (two local-journal appends and one SQLite
//! file-set checkpoint) and about four for a cross-page move. Budgets stated in
//! this crate are therefore *core-initiated* barriers, and the tests say so.
//!
//! ## Attribution
//!
//! The process-wide totals ([`snapshot`]) are always maintained; overhead is one
//! relaxed atomic increment on a code path that is about to block on a device
//! round trip, so the counters are always compiled in and no feature flag can
//! drift out of step with production.
//!
//! Tests need per-operation numbers, and `cargo test` runs many graphs
//! concurrently in one process, so process-wide totals cannot be differenced
//! safely. [`BarrierSession`] solves that: a session is a thread-local
//! attribution channel that the managed actor thread **inherits from whichever
//! thread spawned it**, so a test that opens a session before creating its
//! runtime sees that runtime's barriers and no other test's.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// The maximum number of core-initiated durability barriers one accepted
/// single-block managed save may perform.
///
/// This is a *ceiling on current behaviour*, not the destination: the
/// cost-model audit's target is 3, and `docs/storage-sync-contract.md`
/// §2.10a-i names exactly which two remaining mechanisms hold the number
/// here. Its job is to make any further growth fail a test instead of
/// disappearing into a phase timer.
///
/// The value is asserted against the contract document by
/// `durability_counters::tests::the_contract_states_the_barrier_budget`, so
/// the two cannot drift apart.
pub const MANAGED_SAVE_BARRIER_BUDGET: u64 = 28;

/// The same ceiling for one accepted cross-page (for example cross-day) move,
/// which projects two pages and therefore pays the receipt-store cost twice.
pub const MANAGED_MOVE_BARRIER_BUDGET: u64 = 77;

/// The primitive kinds counted here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum Barrier {
    /// `fsync`/`FlushFileBuffers` of a regular file.
    File = 0,
    /// `fsync` of a directory: a durable name insertion or removal.
    Directory = 1,
    /// `syncfs` of a whole filesystem.
    Filesystem = 2,
}

const BARRIER_KINDS: usize = 3;

const BARRIER_NAMES: [&str; BARRIER_KINDS] = ["file_fsync", "dir_fsync", "syncfs"];

static TOTALS: [AtomicU64; BARRIER_KINDS] = [const { AtomicU64::new(0) }; BARRIER_KINDS];

type SessionCounts = Arc<[AtomicU64; BARRIER_KINDS]>;

thread_local! {
    /// The attribution channel this thread's barriers are additionally charged
    /// to, if any. Empty in production.
    static ATTRIBUTED: RefCell<Option<SessionCounts>> = const { RefCell::new(None) };
}

/// Record one barrier of `kind`.
#[inline]
pub(crate) fn note(kind: Barrier) {
    TOTALS[kind as usize].fetch_add(1, Ordering::Relaxed);
    attribute(kind, 1);
}

#[inline]
fn attribute(kind: Barrier, count: u64) {
    ATTRIBUTED.with(|slot| {
        if let Some(counts) = slot.borrow().as_ref() {
            counts[kind as usize].fetch_add(count, Ordering::Relaxed);
        }
    });
}

/// Record the two barriers that one `tine_storage` immutable publication
/// performs: the temporary file's `fsync`, and the containing directory's
/// `fsync` after the immutable name is installed.
///
/// Counting the storage crate's documented contract at its `tine-core` call
/// site keeps the per-operation sum complete without a `tine-storage` API
/// change. `tine_storage::publish_immutable_exact_impl` is the function whose
/// contract this mirrors; if it ever stops performing exactly one file barrier
/// and one directory barrier, this helper is what must change with it.
#[inline]
pub(crate) fn note_immutable_publication() {
    note(Barrier::File);
    note(Barrier::Directory);
}

/// A point-in-time reading of every counter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BarrierCounts {
    counts: [u64; BARRIER_KINDS],
}

/// Read the process-wide totals.
pub fn snapshot() -> BarrierCounts {
    BarrierCounts {
        counts: std::array::from_fn(|index| TOTALS[index].load(Ordering::Relaxed)),
    }
}

impl BarrierCounts {
    /// Barriers of one kind.
    pub fn get(&self, kind: Barrier) -> u64 {
        self.counts[kind as usize]
    }

    /// Barriers of every kind.
    pub fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// The counts accumulated between `earlier` and `self`.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            counts: std::array::from_fn(|index| {
                self.counts[index].saturating_sub(earlier.counts[index])
            }),
        }
    }

    /// A one-line rendering: `file_fsync=6 dir_fsync=13 syncfs=0 total=19`.
    pub fn report(&self) -> String {
        let mut out = String::new();
        for (index, name) in BARRIER_NAMES.iter().enumerate() {
            out.push_str(&format!("{name}={} ", self.counts[index]));
        }
        out.push_str(&format!("total={}", self.total()));
        out
    }
}

/// A thread-local attribution channel for durability barriers.
///
/// Open one before creating a managed runtime; the actor thread inherits it at
/// spawn, so [`BarrierSession::counts`] reports the barriers that runtime
/// performed — on the actor thread and on the opening thread — without seeing
/// the barriers of any concurrently running test.
///
/// Dropping the handle detaches the *opening* thread. Threads that inherited it
/// keep charging to the same counts until they exit, which is what makes an
/// actor's deferred drain measurable.
#[derive(Clone, Debug)]
pub struct BarrierSession {
    counts: SessionCounts,
}

impl BarrierSession {
    /// Open a session and attach the calling thread to it.
    pub fn begin() -> Self {
        let counts: SessionCounts = Arc::new(std::array::from_fn(|_| AtomicU64::new(0)));
        ATTRIBUTED.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&counts)));
        Self { counts }
    }

    /// Attach the calling thread to this session.
    pub fn attach(&self) {
        ATTRIBUTED.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&self.counts)));
    }

    /// Detach the calling thread from any session.
    pub fn detach_current_thread() {
        ATTRIBUTED.with(|slot| *slot.borrow_mut() = None);
    }

    /// The barriers charged to this session so far.
    pub fn counts(&self) -> BarrierCounts {
        BarrierCounts {
            counts: std::array::from_fn(|index| self.counts[index].load(Ordering::Relaxed)),
        }
    }

    /// Reset this session's counts to zero, so the next measured operation
    /// starts from a clean slate without closing the session.
    pub fn reset(&self) {
        for counter in self.counts.iter() {
            counter.store(0, Ordering::Relaxed);
        }
    }
}

/// The session the calling thread is attached to, if any.
///
/// A thread that is about to spawn a long-lived worker calls this and passes
/// the result into the new thread, which calls [`BarrierSession::attach`]. That
/// is how the managed actor thread inherits its creator's attribution.
pub fn current_session() -> Option<BarrierSession> {
    ATTRIBUTED.with(|slot| {
        slot.borrow().as_ref().map(|counts| BarrierSession {
            counts: Arc::clone(counts),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The living contract states the enforced budget. If a change moves the
    /// constant without moving the document (or the reverse), this fails.
    #[test]
    fn the_contract_states_the_barrier_budget() {
        let contract = include_str!("../../../docs/storage-sync-contract.md");
        assert!(
            contract.contains("Durability barriers and the batch commit point"),
            "the storage contract must carry the durability-barrier section"
        );
        assert!(
            contract.contains(&format!(
                "`MANAGED_SAVE_BARRIER_BUDGET` = **{MANAGED_SAVE_BARRIER_BUDGET}**"
            )),
            "the storage contract must state the enforced save barrier budget \
             ({MANAGED_SAVE_BARRIER_BUDGET})"
        );
        assert!(
            contract.contains(&format!(
                "`MANAGED_MOVE_BARRIER_BUDGET` =\n**{MANAGED_MOVE_BARRIER_BUDGET}**"
            )) || contract.contains(&format!(
                "`MANAGED_MOVE_BARRIER_BUDGET` = **{MANAGED_MOVE_BARRIER_BUDGET}**"
            )),
            "the storage contract must state the enforced move barrier budget \
             ({MANAGED_MOVE_BARRIER_BUDGET})"
        );
    }

    #[test]
    fn no_read_path_reintroduces_a_durability_barrier() {
        let source = include_str!("model.rs");
        for banned in [
            "fn sync_and_read_projection_regular",
            "fn sync_open_and_read_projection_regular",
            "fn sync_and_reread_retained_projection_file",
        ] {
            assert!(
                !source.contains(banned),
                "a read path reintroduced a durability barrier: {banned}. \
                 See the removed-checks table in docs/storage-sync-contract.md."
            );
        }
    }

    #[test]
    fn a_session_counts_only_its_own_threads() {
        let session = BarrierSession::begin();
        note(Barrier::File);
        note(Barrier::Directory);
        note(Barrier::Directory);
        let inherited = current_session().expect("the opening thread is attached");
        let worker = std::thread::spawn(move || {
            inherited.attach();
            note(Barrier::Filesystem);
        });
        worker.join().unwrap();
        let unattached = std::thread::spawn(|| note(Barrier::File));
        unattached.join().unwrap();

        let counts = session.counts();
        assert_eq!(counts.get(Barrier::File), 1);
        assert_eq!(counts.get(Barrier::Directory), 2);
        assert_eq!(counts.get(Barrier::Filesystem), 1);
        assert_eq!(counts.total(), 4);
        BarrierSession::detach_current_thread();
    }

    #[test]
    fn an_immutable_publication_costs_one_file_and_one_directory_barrier() {
        let session = BarrierSession::begin();
        note_immutable_publication();
        let counts = session.counts();
        assert_eq!(counts.get(Barrier::File), 1);
        assert_eq!(counts.get(Barrier::Directory), 1);
        BarrierSession::detach_current_thread();
    }

    #[test]
    fn resetting_a_session_starts_the_next_measurement_from_zero() {
        let session = BarrierSession::begin();
        note(Barrier::File);
        session.reset();
        note(Barrier::Directory);
        assert_eq!(session.counts().get(Barrier::File), 0);
        assert_eq!(session.counts().get(Barrier::Directory), 1);
        BarrierSession::detach_current_thread();
    }
}
