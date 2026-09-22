//! The projection database's writer lease (GH #543, R6-03). A worker that
//! cannot take it waits and retries; it never gives the session's index up.
//! While it waits, nothing is coming (see `IndexNeed::LeaseWait`), so readers
//! take their ordinary route instead of waiting for it.

use super::*;
use std::sync::{LazyLock, Weak};

/// The writer holding each projection database's lease in this process. The
/// entry is added when the lease is taken and removed when it is released,
/// both under this lock, so a worker that fails to take the lease under the
/// same lock knows exactly whether the holder is in this process (wait for it
/// to let go) or outside it (another Tine instance: retry on a backoff).
static WRITERS: LazyLock<Mutex<HashMap<PathBuf, Weak<ProjectionShared>>>> =
    LazyLock::new(Default::default);
/// Notified whenever an entry leaves [`WRITERS`].
static WRITER_RELEASED: Condvar = Condvar::new();

/// The retry backoff when another process holds the lease:
/// `1 s · 2^(n−1)`, at most 30 minutes.
fn lease_retry_wait(failures: u32) -> std::time::Duration {
    let exponent = failures.saturating_sub(1).min(11);
    std::time::Duration::from_secs(1u64 << exponent).min(std::time::Duration::from_secs(30 * 60))
}

/// The worker's exclusive writer lease, held for the worker's lifetime.
pub(super) struct WriterLease {
    file: Option<std::fs::File>,
    shared: Arc<ProjectionShared>,
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        let mut writers = WRITERS.lock().unwrap();
        let ours = writers
            .get(&self.shared.path)
            .is_some_and(|writer| std::ptr::eq(writer.as_ptr(), Arc::as_ptr(&self.shared)));
        if ours {
            writers.remove(&self.shared.path);
        }
        // Released under the registry lock with the entry, so no waiter can
        // see the entry gone and the lease still held.
        drop(self.file.take());
        drop(writers);
        WRITER_RELEASED.notify_all();
    }
}

fn open_lease(path: &Path) -> std::io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock_exclusive()?;
    Ok(file)
}

fn stopped(shared: &ProjectionShared) -> bool {
    shared.pending.lock().unwrap().stop
}

/// Take the writer lease, waiting for it as long as it takes. `None` only
/// when the projection is closed meanwhile.
///
/// I-8 scenarios: a quick graph switch back to a graph whose previous writer
/// is still finishing its turn (in-process holder); an honest second Tine
/// instance on the same graph, or a transient error opening the lock file
/// (retried on the backoff, never given up).
pub(super) fn take_writer_lease(shared: &Arc<ProjectionShared>) -> Option<WriterLease> {
    let lease_path = shared.path.with_extension("sqlite.writer.lock");
    let mut failures = 0u32;
    loop {
        let mut writers = WRITERS.lock().unwrap();
        let error = match open_lease(&lease_path) {
            Ok(file) => {
                writers.insert(shared.path.clone(), Arc::downgrade(shared));
                drop(writers);
                if failures > 0 {
                    shared.pending.lock().unwrap().lease_wait = false;
                    shared.changed.notify_all();
                    projection_diag(|| format!("writer lease taken after {failures} waits"));
                }
                return Some(WriterLease {
                    file: Some(file),
                    shared: Arc::clone(shared),
                });
            }
            Err(error) => error,
        };
        failures = failures.saturating_add(1);
        if failures == 1 {
            eprintln!(
                "[tine] Direct Files SQLite projection waiting: another graph instance owns it or its lease cannot be opened: {error}"
            );
            shared.pending.lock().unwrap().lease_wait = true;
            shared.changed.notify_all();
        }
        if writers.contains_key(&shared.path) {
            // Retry the moment the holder lets go; poll only to notice our
            // own close.
            while writers.contains_key(&shared.path) {
                if stopped(shared) {
                    return None;
                }
                writers = WRITER_RELEASED
                    .wait_timeout(writers, std::time::Duration::from_millis(100))
                    .unwrap()
                    .0;
            }
            continue;
        }
        drop(writers);
        let wait = lease_retry_wait(failures);
        projection_diag(|| format!("writer lease busy; retrying in {wait:?}"));
        let deadline = std::time::Instant::now() + wait;
        let mut pending = shared.pending.lock().unwrap();
        loop {
            if pending.stop {
                return None;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            pending = shared.changed.wait_timeout(pending, left).unwrap().0;
        }
    }
}
