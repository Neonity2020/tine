//! The pending-page overlay projection (query engine R5b).
//!
//! While the Managed actor holds an undrained local suffix, the accepted
//! projection is not the whole story: some pages have a newer pending state,
//! some are new, some are gone. Until R5 every simple query in that state
//! walked — re-loading every pending page and hydrating one document per
//! accepted candidate — so typing made a query cost the graph.
//!
//! This module owns ONE app-private, disposable SQLite projection file holding
//! ONLY the pending pages, written by a worker thread off the actor from the
//! same per-page lowering the accept path uses, so a pending page's rows are
//! byte-for-byte what the accepted projection would hold once the batch is
//! accepted. The executor (R5a) answers a pending query from two snapshots —
//! the accepted file with the pending pages MASKED, and this file — merged
//! under one budget.
//!
//! **What the file is.** `PhysicalGraphProjectionDatabase` — Direct Files'
//! disposable projection type: WAL, stamp-free, every query table, FTS rows
//! written inline (`search_fts_build.phase` is seeded 1). It is NEVER trusted
//! across process lifetimes: [`PendingOverlay::open`] deletes it and rebuilds
//! it from the authoritative pending state (`latest_projection_frames`), and
//! [`PendingOverlay::close`] deletes it again. It is private state the user can
//! walk away from (I-7), never authority (I-1), and never durable (I-2: a crash
//! costs the rebuild, nothing else).
//!
//! **Revisions.** The actor stamps every update with the next revision and a
//! query captures the latest revision it saw (`required`). The worker flushes
//! the NEWEST state per path and publishes the newest revision it applied.
//! A query needs `flushed ≥ required`; a later pending edit that the flush also
//! carried is exactly a "later ordinary edit" the accepted side tolerates too
//! (plan §2B: consistency is as of snapshot acquisition). Coherence of the
//! accepted/overlay PAIR is the executor's, by open ORDER: overlay first at
//! `(instance, flushed)`, then the accepted file validated at the captured
//! acceptance sequence — a pending path can only leave the set when its batch
//! is accepted, which advances that sequence.
//!
//! A slow flush or announced page awaiting content yields temporary readiness.
//! A failed worker or unreadable file yields a bounded execution failure;
//! waiting cannot repair it, and production queries never traverse instead.
//! Reopening reconstructs the disposable overlay from authoritative state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tine_storage::sqlite::{
    MaterializationError, PhysicalGraphProjectionChange, PhysicalGraphProjectionDatabase,
    PhysicalProjectionQuerySnapshot,
};

use crate::config::ParseConfig;
use crate::oplog::MaterializedPage;

/// A process-wide overlay instance counter: a snapshot validated against
/// `(instance, flushed_revision)` can never mistake a re-created file for the
/// one it was captured against.
pub(crate) fn next_instance() -> u64 {
    static INSTANCES: AtomicU64 = AtomicU64::new(1);
    INSTANCES.fetch_add(1, Ordering::AcqRel)
}

/// The overlay file's name, derived from the accepted projection's path so the
/// two always sit together and a graph has exactly one.
pub(crate) fn overlay_path_for(accepted_projection: &Path) -> PathBuf {
    let mut name = accepted_projection
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".pending-overlay.sqlite");
    accepted_projection.with_file_name(name)
}

/// One actor-stamped change to the pending set, in revision order.
enum OverlayUpdate {
    /// The path entered the pending set (a frame was noted for it); its rows
    /// are not known yet. Masks the accepted page; marks the path incomplete.
    Announce {
        revision: u64,
        path: String,
    },
    /// The pending page's content: rows replace whatever the path had.
    Content {
        revision: u64,
        path: String,
        page: Arc<MaterializedPage>,
    },
    /// The pending state of the path is "no page" (deleted): rows are removed,
    /// the path stays masked, and it is no longer incomplete.
    Tombstone {
        revision: u64,
        path: String,
    },
    /// The path left the pending set (its batch was accepted and drained).
    Remove {
        revision: u64,
        path: String,
    },
    Close,
}

/// What the worker has published, read by the executor under the mutex.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OverlayState {
    /// Bumped by every `open`; a snapshot validated against `(instance,
    /// flushed_revision)` is exactly one flush of one file.
    pub(crate) instance: u64,
    /// The newest revision whose effect is in the file.
    pub(crate) flushed_revision: u64,
    /// The pending set as of `flushed_revision`: paths whose accepted rows the
    /// executor masks. A tombstoned path is here with no rows in the file.
    pub(crate) pending_paths: BTreeSet<String>,
    /// Announced paths whose content has not been written yet. Non-empty only
    /// inside an actor turn (announce and content land in the same turn) or
    /// while a commit's response is deferred; the executor waits, then reports
    /// temporary readiness if content is still outstanding.
    pub(crate) incomplete: BTreeSet<String>,
    /// Sticky until the next open: the file is not to be trusted.
    pub(crate) failed: Option<&'static str>,
    pub(crate) closed: bool,
}

/// Why an overlay snapshot could not be taken.
pub(crate) enum OverlayOpen {
    /// The overlay is at `(instance, flushed_revision)` with `pending_paths`.
    Snapshot {
        snapshot: PhysicalProjectionQuerySnapshot,
        state: OverlayState,
    },
    /// The live worker has not flushed `required`, or content is outstanding.
    Pending,
    /// Waiting cannot repair a failed worker or an unreadable projection.
    Failed(&'static str),
    /// The overlay moved between the two validations of the open.
    Stale,
    Closed,
}

pub(crate) struct PendingOverlay {
    path: PathBuf,
    next_revision: AtomicU64,
    sender: Mutex<Option<mpsc::Sender<OverlayUpdate>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    state: Mutex<OverlayState>,
    changed: Condvar,
}

impl std::fmt::Debug for PendingOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingOverlay")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl PendingOverlay {
    /// Create the overlay for the accepted projection at `accepted_projection`:
    /// delete any file from an earlier process, create a fresh one, and start
    /// the worker under `config`. The caller then announces the current pending
    /// set (the actor's `latest_projection_frames`) through the ordinary
    /// updates, so the file is rebuilt from authoritative state, never reused.
    pub(crate) fn open(
        accepted_projection: &Path,
        config: ParseConfig,
        instance: u64,
    ) -> Result<Arc<Self>, String> {
        let path = overlay_path_for(accepted_projection);
        remove_overlay_files(&path);
        let database = PhysicalGraphProjectionDatabase::open_writable(&path)
            .map_err(|error| format!("pending overlay open: {error}"))?;
        database
            .initialize_schema()
            .map_err(|error| format!("pending overlay schema: {error}"))?;
        database
            .validate_schema()
            .map_err(|error| format!("pending overlay schema validation: {error}"))?;
        let (sender, receiver) = mpsc::channel();
        let overlay = Arc::new(Self {
            path,
            next_revision: AtomicU64::new(1),
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(None),
            state: Mutex::new(OverlayState {
                instance,
                ..OverlayState::default()
            }),
            changed: Condvar::new(),
        });
        let worker_overlay = Arc::clone(&overlay);
        let worker = std::thread::Builder::new()
            .name("tine-pending-overlay".into())
            .spawn(move || worker_overlay.run(database, receiver, config))
            .map_err(|error| format!("pending overlay worker: {error}"))?;
        *overlay.worker.lock().unwrap() = Some(worker);
        Ok(overlay)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The revision a capture taken now must wait for: every update pushed so
    /// far is at or below it.
    pub(crate) fn latest_revision(&self) -> u64 {
        self.next_revision.load(Ordering::Acquire) - 1
    }

    fn push(&self, make: impl FnOnce(u64) -> OverlayUpdate) {
        let revision = self.next_revision.fetch_add(1, Ordering::AcqRel);
        let sender = self.sender.lock().unwrap();
        if let Some(sender) = sender.as_ref() {
            if sender.send(make(revision)).is_err() {
                self.mark_failed("pending overlay worker stopped");
            }
        }
    }

    pub(crate) fn announce(&self, path: &str) {
        let path = path.to_owned();
        self.push(|revision| OverlayUpdate::Announce { revision, path });
    }

    pub(crate) fn content(&self, path: &str, page: Arc<MaterializedPage>) {
        let path = path.to_owned();
        self.push(|revision| OverlayUpdate::Content {
            revision,
            path,
            page,
        });
    }

    pub(crate) fn tombstone(&self, path: &str) {
        let path = path.to_owned();
        self.push(|revision| OverlayUpdate::Tombstone { revision, path });
    }

    pub(crate) fn remove(&self, path: &str) {
        let path = path.to_owned();
        self.push(|revision| OverlayUpdate::Remove { revision, path });
    }

    /// Mark the overlay unusable until the next open (a pending page the
    /// actor could not load). Pending captures report a bounded failure.
    pub(crate) fn mark_failed(&self, reason: &'static str) {
        let mut state = self.state.lock().unwrap();
        if state.failed.is_none() {
            state.failed = Some(reason);
        }
        self.changed.notify_all();
    }

    pub(crate) fn state(&self) -> OverlayState {
        self.state.lock().unwrap().clone()
    }

    /// Block until the file carries `required` (or the overlay is failed or
    /// closed), for at most `wait`.
    pub(crate) fn wait_flushed(&self, required: u64, wait: Duration) -> OverlayState {
        let deadline = Instant::now() + wait;
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed || state.failed.is_some() || state.flushed_revision >= required {
                return state.clone();
            }
            let now = Instant::now();
            if now >= deadline {
                return state.clone();
            }
            let (guard, _) = self.changed.wait_timeout(state, deadline - now).unwrap();
            state = guard;
        }
    }

    /// One read snapshot of the overlay, pinned at exactly one published state.
    ///
    /// Waits for `required`, reads the state ONCE, then opens with
    /// `open_direct`, whose validator runs before `BEGIN` and again after the
    /// read that pins the snapshot: a flush between the two is `Stale`, so the
    /// returned `state` is exactly what the transaction sees. Readiness is
    /// temporary only while a live worker can still publish the missing data.
    pub(crate) fn open_snapshot(&self, required: u64, wait: Duration) -> OverlayOpen {
        let observed = self.wait_flushed(required, wait);
        if observed.closed {
            return OverlayOpen::Closed;
        }
        if let Some(reason) = observed.failed {
            return OverlayOpen::Failed(reason);
        }
        if self
            .worker
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
        {
            return OverlayOpen::Failed("pending overlay worker stopped");
        }
        if observed.flushed_revision < required || !observed.incomplete.is_empty() {
            return OverlayOpen::Pending;
        }
        let validate = || {
            let state = self.state.lock().unwrap();
            if state.closed {
                return Err(MaterializationError::Incomplete(
                    "pending overlay closed".into(),
                ));
            }
            if state.instance != observed.instance
                || state.flushed_revision != observed.flushed_revision
                || state.failed.is_some()
            {
                return Err(MaterializationError::Incomplete(
                    "pending overlay moved".into(),
                ));
            }
            Ok(())
        };
        match PhysicalProjectionQuerySnapshot::open_direct(&self.path, validate) {
            Ok(snapshot) => OverlayOpen::Snapshot {
                snapshot,
                state: observed,
            },
            Err(MaterializationError::Incomplete(_)) => OverlayOpen::Stale,
            Err(_) => OverlayOpen::Failed("pending overlay snapshot"),
        }
    }

    /// Stop the worker, join it, and delete the file. Idempotent. The caller
    /// drains every off-actor query job first (I-21): a reader still holding a
    /// snapshot of this file would otherwise outlive it.
    pub(crate) fn close(&self) {
        let sender = self.sender.lock().unwrap().take();
        if let Some(sender) = sender {
            let _ = sender.send(OverlayUpdate::Close);
        }
        let worker = self.worker.lock().unwrap().take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
        {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
        }
        self.changed.notify_all();
        remove_overlay_files(&self.path);
    }

    /// The worker: apply the newest state per path per wake-up in ONE SQLite
    /// transaction, then publish the newest revision applied.
    fn run(
        &self,
        mut database: PhysicalGraphProjectionDatabase,
        receiver: mpsc::Receiver<OverlayUpdate>,
        config: ParseConfig,
    ) {
        // path → the page id whose rows the file holds for it.
        let mut written: BTreeMap<String, [u8; 16]> = BTreeMap::new();
        'outer: loop {
            let Ok(first) = receiver.recv() else {
                break;
            };
            // Coalesce: the newest content per path wins; announce/remove
            // membership is applied in order into the published set.
            let mut newest_revision = 0;
            let mut pending_paths = self.state.lock().unwrap().pending_paths.clone();
            let mut incomplete = self.state.lock().unwrap().incomplete.clone();
            let mut rows: BTreeMap<String, Option<Arc<MaterializedPage>>> = BTreeMap::new();
            let mut batch = vec![first];
            batch.extend(receiver.try_iter());
            for update in batch {
                match update {
                    OverlayUpdate::Close => break 'outer,
                    OverlayUpdate::Announce { revision, path } => {
                        newest_revision = newest_revision.max(revision);
                        pending_paths.insert(path.clone());
                        incomplete.insert(path);
                    }
                    OverlayUpdate::Content {
                        revision,
                        path,
                        page,
                    } => {
                        newest_revision = newest_revision.max(revision);
                        pending_paths.insert(path.clone());
                        incomplete.remove(&path);
                        rows.insert(path, Some(page));
                    }
                    OverlayUpdate::Tombstone { revision, path } => {
                        newest_revision = newest_revision.max(revision);
                        pending_paths.insert(path.clone());
                        incomplete.remove(&path);
                        rows.insert(path, None);
                    }
                    OverlayUpdate::Remove { revision, path } => {
                        newest_revision = newest_revision.max(revision);
                        pending_paths.remove(&path);
                        incomplete.remove(&path);
                        rows.insert(path, None);
                    }
                }
            }
            let applied = apply_rows(&mut database, &config, &mut written, rows);
            let mut state = self.state.lock().unwrap();
            match applied {
                Ok(()) => {
                    state.flushed_revision = state.flushed_revision.max(newest_revision);
                    state.pending_paths = pending_paths;
                    state.incomplete = incomplete;
                }
                Err(reason) => {
                    if state.failed.is_none() {
                        state.failed = Some(reason);
                    }
                }
            }
            drop(state);
            self.changed.notify_all();
        }
        drop(database);
    }
}

/// Lower the newest content per path with the accept path's own producer and
/// write the whole batch in one transaction. A page whose id changed under
/// its path (deleted and re-created while pending) loses its old rows first.
/// The reason names a stage, never a value (I-5).
fn apply_rows(
    database: &mut PhysicalGraphProjectionDatabase,
    config: &ParseConfig,
    written: &mut BTreeMap<String, [u8; 16]>,
    rows: BTreeMap<String, Option<Arc<MaterializedPage>>>,
) -> Result<(), &'static str> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut inputs = Vec::new();
    let mut deletions = Vec::new();
    let mut replaced: Vec<(String, [u8; 16])> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    for (path, page) in rows {
        let previous = written.get(&path).copied();
        match page {
            Some(page) => {
                let page_id = page.page_id.as_uuid().into_bytes();
                if previous.is_some_and(|old| old != page_id) {
                    deletions.push(previous.expect("checked"));
                }
                inputs.push(crate::oplog::sqlite::materialized_page_input(
                    (*page).clone(),
                ));
                replaced.push((path, page_id));
            }
            None => {
                if let Some(old) = previous {
                    deletions.push(old);
                }
                removed.push(path);
            }
        }
    }
    let replacements =
        crate::oplog::sqlite_materialization::lower_pages_with_derived_rows(&inputs, config)
            .map_err(|_| "pending overlay lowering")?;
    database
        .apply(&PhysicalGraphProjectionChange {
            replacements,
            deletions,
            reference_postings: Vec::new(),
        })
        .map_err(|_| "pending overlay write")?;
    for (path, page_id) in replaced {
        written.insert(path, page_id);
    }
    for path in removed {
        written.remove(&path);
    }
    Ok(())
}

fn remove_overlay_files(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let _ = std::fs::remove_file(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The storage contract's account of the overlay, pinned sentence by
    /// sentence so a rewrite of either side fails here first.
    #[test]
    fn storage_contract_names_the_pending_overlay() {
        let contract = include_str!("../../../docs/storage-sync-contract.md");
        let section = contract
            .split("**The pending local suffix has one mirror off the actor: the overlay\nprojection.**")
            .nth(1)
            .and_then(|tail| tail.split("## 2. Enrollment").next())
            .expect("the overlay paragraph precedes section 2");
        for sentence in [
            "`<accepted projection>.pending-overlay.sqlite`",
            "deleted and recreated empty on every runtime open, deleted again on close",
            "carries no frontier, no stamp and no authority",
            "The actor never writes it",
            "a single overlay worker thread",
            "the same per-page lowering the accepted apply uses",
            "reports a bounded failure and never an answer",
            "the stamp's `overlay_revision`",
            "A query is a read: it pushes nothing and advances no revision",
            "a pending\npath leaves the set only when its batch is accepted",
        ] {
            assert!(section.contains(sentence), "contract lost: {sentence}");
        }
        assert!(
            contract
                .contains("| `<configured projection>.pending-overlay.sqlite` (+ `-wal`/`-shm`) |"),
            "the §1.2 layout table lost the overlay row"
        );
        assert!(overlay_path_for(Path::new("p.sqlite"))
            .to_string_lossy()
            .ends_with(".pending-overlay.sqlite"));
    }

    #[test]
    fn the_overlay_file_sits_next_to_the_accepted_projection() {
        assert_eq!(
            overlay_path_for(Path::new("/graph/.tine/projection.sqlite")),
            PathBuf::from("/graph/.tine/projection.sqlite.pending-overlay.sqlite")
        );
    }

    #[test]
    fn open_creates_a_fresh_file_and_close_deletes_it() {
        let dir = std::env::temp_dir().join(format!(
            "tine-pending-overlay-{}-{}",
            std::process::id(),
            next_instance()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let accepted = dir.join("projection.sqlite");
        let overlay_path = overlay_path_for(&accepted);
        std::fs::write(&overlay_path, b"garbage from an earlier process").unwrap();
        let overlay = PendingOverlay::open(&accepted, ParseConfig::default(), 7).unwrap();
        assert!(overlay_path.exists());
        assert_ne!(
            std::fs::read(&overlay_path).unwrap(),
            b"garbage from an earlier process"
        );
        let state = overlay.state();
        assert_eq!(state.instance, 7);
        assert_eq!(state.flushed_revision, 0);
        assert_eq!(overlay.latest_revision(), 0);

        // Membership without content: announced paths are pending and
        // incomplete, a tombstone is pending and complete, a removal is gone.
        overlay.announce("pages/a.md");
        overlay.tombstone("pages/b.md");
        let required = overlay.latest_revision();
        assert_eq!(required, 2);
        let state = overlay.wait_flushed(required, Duration::from_secs(5));
        assert_eq!(state.flushed_revision, 2);
        assert_eq!(
            state.pending_paths,
            BTreeSet::from(["pages/a.md".to_owned(), "pages/b.md".to_owned()])
        );
        assert_eq!(state.incomplete, BTreeSet::from(["pages/a.md".to_owned()]));
        assert!(matches!(
            overlay.open_snapshot(required, Duration::from_millis(10)),
            OverlayOpen::Pending
        ));
        overlay.tombstone("pages/a.md");
        overlay.remove("pages/b.md");
        let required = overlay.latest_revision();
        match overlay.open_snapshot(required, Duration::from_secs(5)) {
            OverlayOpen::Snapshot { state, .. } => {
                assert_eq!(state.flushed_revision, 4);
                assert_eq!(
                    state.pending_paths,
                    BTreeSet::from(["pages/a.md".to_owned()])
                );
                assert!(state.incomplete.is_empty());
            }
            _ => panic!("expected a snapshot"),
        }
        // A revision the worker has not flushed is not waited for beyond the
        // budget: the caller receives temporary readiness.
        assert!(matches!(
            overlay.open_snapshot(required + 1, Duration::from_millis(10)),
            OverlayOpen::Pending
        ));
        // Even a complete file cannot promise future progress after its
        // worker exits. This is a real stopped thread, not a forced outcome.
        overlay
            .sender
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(OverlayUpdate::Close)
            .unwrap();
        overlay
            .worker
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join()
            .unwrap();
        assert!(matches!(
            overlay.open_snapshot(required, Duration::from_millis(10)),
            OverlayOpen::Failed("pending overlay worker stopped")
        ));
        // A subsequent update also records the disconnected receiver.
        overlay.announce("pages/stopped.md");
        assert_eq!(
            overlay.state().failed,
            Some("pending overlay worker stopped")
        );
        overlay.state.lock().unwrap().failed = None;
        overlay.mark_failed("test");
        assert!(matches!(
            overlay.open_snapshot(required, Duration::from_millis(10)),
            OverlayOpen::Failed("test")
        ));
        overlay.close();
        assert!(!overlay_path.exists());
        assert!(matches!(
            overlay.open_snapshot(required, Duration::from_millis(10)),
            OverlayOpen::Closed
        ));
        // Idempotent; pushes after close are dropped, not panics.
        overlay.close();
        overlay.announce("pages/c.md");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
