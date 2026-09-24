//! The index's WAL checkpoint, off the worker (GH #543).
//!
//! SQLite's default copies the WAL into the image inside whichever commit
//! grows it past 1,000 pages, on the committing thread, and flushes both
//! files. The worker is that thread, so a large turn published nothing until
//! the copy was on disk: a 261-page rename on 10,000 pages ran nine such
//! checkpoints, ~60 s on a hosted Windows disk, while every search waited for
//! the turn. The worker's connection no longer checkpoints
//! (`configure_live_writer`); after a turn this copies the WAL on a
//! connection and thread of its own, which nothing interactive waits on.

use super::*;

/// A WAL at least this large is copied into the image after a turn. Smaller
/// ones wait for later turns: a one-block edit adds ~125 KB, and a checkpoint
/// per edit would flush the image per edit.
const CHECKPOINT_WAL_BYTES: u64 = 4 * 1024 * 1024;

/// A copied WAL is emptied only when nothing is reading it or writing to it
/// at that moment, and no later checkpoint runs until the next turn. A
/// session that ends first leaves the copied frames behind, and the next open
/// copies them all again: 13-23 s on a hosted Windows disk, during which new
/// read connections stalled (GH #543). A search is usually what holds the
/// WAL, and it finishes within a second or two, so try again for a while.
const EMPTY_RETRIES: u32 = 20;
const EMPTY_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

#[cfg(test)]
pub(super) static CHECKPOINT_WAL_BYTES_TEST: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn checkpoint_wal_bytes() -> u64 {
    #[cfg(test)]
    {
        let bytes = CHECKPOINT_WAL_BYTES_TEST.load(Ordering::Acquire);
        if bytes > 0 {
            return bytes;
        }
    }
    CHECKPOINT_WAL_BYTES
}

fn wal_bytes(image: &Path) -> u64 {
    std::fs::metadata(wal_path(image)).map_or(0, |wal| wal.len())
}

fn wal_path(image: &Path) -> PathBuf {
    let mut name = image.file_name().unwrap_or_default().to_os_string();
    name.push("-wal");
    image.with_file_name(name)
}

/// Start the checkpoint in the background if the WAL has grown past
/// [`CHECKPOINT_WAL_BYTES`]. Called by the worker after a turn commits.
pub(super) fn start_if_due(shared: &Arc<ProjectionShared>) {
    let due = wal_bytes(&shared.path) >= checkpoint_wal_bytes();
    if !due {
        return;
    }
    {
        let mut pending = shared.pending.lock().unwrap();
        if pending.stop || pending.checkpoint_running {
            return;
        }
        pending.checkpoint_running = true;
    }
    let checkpointer = Arc::clone(shared);
    let spawned = std::thread::Builder::new()
        .name("tine-index-checkpoint".into())
        .spawn(move || {
            let mut retries = 0;
            loop {
                let started = std::time::Instant::now();
                let outcome =
                    PhysicalGraphProjectionDatabase::checkpoint_passive_at(&checkpointer.path);
                projection_diag(|| {
                    format!(
                        "background checkpoint in {}ms: {outcome:?} retry={retries}",
                        started.elapsed().as_millis()
                    )
                });
                #[cfg(test)]
                checkpointer
                    .checkpoint_passes
                    .fetch_add(1, Ordering::AcqRel);
                if outcome.is_err()
                    || retries == EMPTY_RETRIES
                    || wal_bytes(&checkpointer.path) == 0
                {
                    break;
                }
                retries += 1;
                if !step_aside(&checkpointer) {
                    return;
                }
            }
            finish(&checkpointer);
        });
    if spawned.is_err() {
        finish(shared);
    }
}

/// Wait [`EMPTY_RETRY_INTERVAL`] as no running checkpoint, so a drain that
/// must replace the image does not wait for the retry. `false` when this
/// checkpoint should not resume: the projection is stopping, a fresh build
/// may be about to replace the file, or another checkpoint has started.
fn step_aside(shared: &ProjectionShared) -> bool {
    let deadline = std::time::Instant::now() + EMPTY_RETRY_INTERVAL;
    let mut pending = shared.pending.lock().unwrap();
    pending.checkpoint_running = false;
    shared.changed.notify_all();
    loop {
        let now = std::time::Instant::now();
        if pending.stop || now >= deadline {
            break;
        }
        pending = shared
            .changed
            .wait_timeout(pending, deadline - now)
            .unwrap()
            .0;
    }
    if pending.stop
        || pending.checkpoint_running
        || shared.fresh_build_running.load(Ordering::Acquire)
    {
        return false;
    }
    pending.checkpoint_running = true;
    true
}

fn finish(shared: &ProjectionShared) {
    shared.pending.lock().unwrap().checkpoint_running = false;
    shared.changed.notify_all();
}

/// Wait until a running checkpoint has closed its connection. A drain calls
/// this before a replacement image is published over the file: on Windows an
/// open connection would refuse that. A checkpoint cannot be interrupted, but
/// it is bounded by the WAL one turn wrote.
pub(super) fn wait(shared: &ProjectionShared) {
    let mut pending = shared.pending.lock().unwrap();
    while pending.checkpoint_running {
        pending = shared.changed.wait(pending).unwrap();
    }
}
