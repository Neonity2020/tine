use crate::config::ParseConfig;
use crate::doc::{property_key_norm, DocBlock, Document};
use crate::query::registry_cache::{CommittedRegistryCache, RegistryCapture};
use crate::query::registry_sql::{self, PageRegistryMetadata};
use crate::query::results::PageLiveIds;
use crate::query::PropertyFacetAccumulator;
use crate::query_cursor::drain_after;
use crate::query_jobs::{
    OwnedAdmission, QueryJobOwner, DEFAULT_QUERY_JOB_CAPACITY, QUERY_JOB_WAIT,
};
use crate::vocab::{Format, PageEntry, PageKind, ReferenceKind};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use fs2::FileExt as _;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use tine_storage::sqlite::{
    PhysicalAliasDeclaration, PhysicalBlock, PhysicalEntityCoordinate, PhysicalEntityId,
    PhysicalGraphProjectionChange, PhysicalGraphProjectionDatabase,
    PhysicalGraphProjectionSourceRevision, PhysicalName, PhysicalPage,
    PhysicalProjectionQuerySnapshot, PhysicalProperty, PhysicalQueryValue,
    PhysicalReferencePosting, PhysicalReferenceTarget, PhysicalTask,
};
use uuid::Uuid;

#[path = "direct_projection_lease.rs"]
mod lease;
#[path = "direct_projection_owner.rs"]
mod owner;
#[cfg(test)]
pub(crate) use owner::index_failures_reported_for_test;
use owner::ReportDamage;
#[cfg(test)]
pub(crate) use owner::INDEX_ATTEMPTS;
use owner::{backing_off, image_is_current, note_unsettled};
pub use owner::{set_index_failure_observer, IndexFailureEvent};
pub(crate) use owner::{IndexFailure, IndexNeed, IndexOwnerRegistration, OwnerStep};

type PageSnapshot = Arc<Vec<(PageEntry, Arc<Document>)>>;
type PageRevisions = Arc<HashMap<PathBuf, String>>;

struct CommittedRegistryOwner {
    cache: CommittedRegistryCache,
    config: Arc<ParseConfig>,
}

type SharedCommittedRegistry = Arc<Mutex<Option<CommittedRegistryOwner>>>;

// This is the parser-fact extractor identity, not an on-disk schema version.
// Bump it whenever unchanged source bytes must be lowered into new/different
// physical facts. The source-revision delta then rebuilds each page once even
// when tine-storage's disposable SQLite schema itself remains compatible.
// v3: `pages.journal_day` is the page's own `date_key`, so a journal named by
// `title::` has its day (GH #543, audit R13-06).
// v4: `blocks.result_id` is the block's structural id, never a session's live
// id, so a page stored by an earlier session resolves after a reopen (R3; GH
// #594).
// v5: the search fold keeps marks that make another letter (kana voicing,
// virama, Indic vowel signs, Cyrillic й) and unstrokes ł ø đ ħ ŧ, so stored
// search tokens and folded names change (decided by Martin 2026-09-24).
const DIRECT_PROJECTION_FACTS_VERSION: u32 = 5;
const REFERENCE_DELTA_WAIT: std::time::Duration = std::time::Duration::from_millis(250);
#[cfg(test)]
// Test receipts count only their own graph, including its worker threads.
static PHYSICAL_PAGE_LOWERINGS: Mutex<(Option<PathBuf>, u64)> = Mutex::new((None, 0));

#[cfg(test)]
type BeforeApplyHook = (PathBuf, Box<dyn FnOnce() + Send>);
#[cfg(test)]
static BEFORE_APPLY_PENDING: Mutex<Option<BeforeApplyHook>> = Mutex::new(None);

/// Count page lowerings under `root` from now on (model-level tests).
#[cfg(test)]
pub(crate) fn count_page_lowerings_test(root: &Path) {
    *PHYSICAL_PAGE_LOWERINGS.lock().unwrap() = (Some(root.to_path_buf()), 0);
}

#[cfg(test)]
pub(crate) fn page_lowerings_test() -> u64 {
    PHYSICAL_PAGE_LOWERINGS.lock().unwrap().1
}

/// Run `hook` once, just before the next turn applies, on the worker whose
/// projection database path starts with `scope` (the database itself, or the
/// graph root when the database lives under it). Scoped to one graph: tests
/// run in parallel, and a hook another graph's worker took would hold that
/// test's turn instead.
#[cfg(test)]
pub(crate) fn before_next_apply_test(scope: &Path, hook: Box<dyn FnOnce() + Send>) {
    *BEFORE_APPLY_PENDING.lock().unwrap() = Some((scope.to_path_buf(), hook));
}

#[cfg(test)]
fn run_before_apply_deltas_hook(database: &Path) {
    let hook = {
        let mut pending = BEFORE_APPLY_PENDING.lock().unwrap();
        if pending
            .as_ref()
            .is_some_and(|(scope, _)| database.starts_with(scope))
        {
            pending.take()
        } else {
            None
        }
    };
    if let Some((_, hook)) = hook {
        hook();
    }
}

#[cfg(test)]
thread_local! {
    static REGISTRY_READ_ATTEMPTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn take_registry_read_attempts() -> u64 {
    REGISTRY_READ_ATTEMPTS.with(|count| count.replace(0))
}

/// One page change, as a mark carries it. **The graph config travels INSIDE the
/// work item** (§5.8 M21, F11): every arm that lowers a page carries the exact
/// [`ParseConfig`] it must be lowered under, so the worker cannot reach a state
/// where queued work exists and the config that describes it does not.
///
/// `Delete` deliberately carries no config: it lowers nothing and stamps no
/// source revision, so a config on that arm would be a value with no reader.
#[derive(Clone)]
enum PageDelta {
    Replace {
        entry: PageEntry,
        document: Arc<Document>,
        revision: String,
        parse_config: Arc<ParseConfig>,
    },
    Delete {
        entry: PageEntry,
    },
}

/// One page the caller has added, rewritten or removed, as the model layer
/// describes it. A mutation that changes the page SET — a rename, a merge, a
/// file rescue — hands its whole change over as one of these lists; see
/// [`DirectProjection::enqueue_page_set`].
pub(crate) enum PageSetChange {
    Replace {
        entry: PageEntry,
        document: Arc<Document>,
        revision: String,
    },
    Delete {
        entry: PageEntry,
    },
}

impl PageSetChange {
    pub(crate) fn entry(&self) -> &PageEntry {
        match self {
            PageSetChange::Replace { entry, .. } | PageSetChange::Delete { entry } => entry,
        }
    }
}

impl PageDelta {
    fn entry(&self) -> &PageEntry {
        match self {
            PageDelta::Replace { entry, .. } | PageDelta::Delete { entry } => entry,
        }
    }

    /// Whether this change leaves the page at `revision` under `digest`.
    fn carries(&self, revision: &str, digest: &tine_storage::ContentDigest) -> bool {
        match self {
            PageDelta::Replace {
                revision: sent,
                parse_config,
                ..
            } => sent == revision && parse_config.digest() == *digest,
            PageDelta::Delete { .. } => false,
        }
    }
}

/// A queued whole-graph snapshot for a fresh build, the generation it was
/// captured at, and the config it must be lowered under. The config is stamped
/// into every page's `projection_source_revision`, so a config edit re-lowers
/// every page (J7, D-1: rebuild, never migrate).
struct PendingFull {
    pages: PageSnapshot,
    revisions: PageRevisions,
    parse_config: Arc<ParseConfig>,
    /// Sources that exist but could not be read, listed or parsed for this
    /// snapshot, as graph-relative paths: a page, or a directory whose pages
    /// all count as unread (`""` is the whole graph). A fresh build over a
    /// healthy image carries their stored rows (see `carried`).
    retained: Vec<String>,
}

/// Above this share of the pages, a stale image found by the launch survey is
/// rebuilt from a complete parsed snapshot instead of repaired page by page:
/// it parses the same pages, writes a fresh file rather than rewriting most of
/// the old one in place, stops between batches and shows progress (GH #543,
/// audit R10-02). Read only by [`repair_is_proportionate`].
const REPAIR_MAX_SHARE_DIVISOR: usize = 4;

/// Whether an image that differs from its source by `changed` of `total`
/// pages is repaired in place rather than built fresh.
pub(crate) fn repair_is_proportionate(changed: usize, total: usize) -> bool {
    changed * REPAIR_MAX_SHARE_DIVISOR <= total
}

enum QueryCaptureRequirement {
    CurrentSnapshot(Currency),
    #[cfg(test)]
    StrictGeneration(u64),
}

struct PendingQueryCapture {
    requirement: QueryCaptureRequirement,
    registry_sensitivity: RegistrySensitivity,
    slot: crate::query_jobs::OwnedJobSlot,
    reply: std::sync::mpsc::SyncSender<QueryJobOpen>,
}

/// Whether a query can observe property type inference. Registry-sensitive
/// jobs freeze the cache input beside their SQL snapshot; all other jobs carry
/// no registry state at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegistrySensitivity {
    Insensitive,
    Required,
}

fn reject_query_captures(captures: Vec<PendingQueryCapture>) {
    for capture in captures {
        // Release admission before replying, so the receiver can immediately
        // retry without being held behind its own rejected capture.
        drop(capture.slot);
        let _ = capture.reply.send(QueryJobOpen::Cancelled);
    }
}

/// A page change on its way to the image, stamped with the cache generation
/// at which its truth was observed: the generation its producer moved to, or
/// the one the launch survey loaded before it read the page.
type Mark = (u64, PageDelta);

/// The index update protocol's whole state (GH #543, the reconciler; design:
/// `specs/notes/2026-09-23-index-reconciler-design.md`).
///
/// **One invariant: every page change reaches the image as a [`Mark`], the
/// queue keeps only the newest mark per page, and no mark older than what the
/// image already holds for that page is applied.** Nothing else crosses time:
/// no verdict computed at one moment is applied later on the strength of that
/// moment. A save racing the launch survey, a deletion confirmed before a
/// re-creation, a failed turn returning its work -- each is one mark against
/// another, and the newer wins wherever they meet (see [`Self::record_mark`]).
#[derive(Default)]
struct PendingProjection {
    // Each capture owns one slot from the shared two-job cap.
    captures: Vec<PendingQueryCapture>,
    /// A fresh build's snapshot, queued.
    full: Option<PendingFull>,
    /// A fresh image is owed: there is none, it is damaged (K1), or the launch
    /// survey found too much of it stale. Only a fresh build clears it. While
    /// it holds, marks wait: the snapshot that clears it contains them.
    rebuild: bool,
    /// Per page, the newest change not yet taken by the worker.
    marks: BTreeMap<String, Mark>,
    /// The marks the running worker turn took and has not committed.
    in_flight: BTreeMap<String, Mark>,
    /// Per page, the generation of the newest mark committed this session.
    applied: HashMap<String, u64>,
    /// The survey validated the image under this configuration: the next turn
    /// reads the committed property registry from it. Queries capture from
    /// that registry, so readiness waits for it.
    registry_owed: Option<Arc<ParseConfig>>,
    /// Every page the image holds is at least this new: the generation of the
    /// last fresh snapshot accepted.
    floor: u64,
    latest_generation: u64,
    stop: bool,
    /// The worker has opened (or found no) stored image. Until then nobody,
    /// the worker included, knows what the image needs.
    set_up: bool,
    /// The stored image was written under the current facts version and parse
    /// configuration, so it may be served before the launch check validates it
    /// ([`owner::serving_stored`]). Decided once, when the worker sets up.
    stored_servable: bool,
    /// The background integrity check ([`integrity`]) holds a read connection
    /// on the image. A drain waits for it to let go.
    integrity_running: bool,
    /// The background WAL checkpoint ([`checkpoint`]) holds a connection on
    /// the image. A drain waits for it to let go.
    checkpoint_running: bool,
    /// The worker is running a fresh build. Queries do not capture from an
    /// image being replaced.
    building: bool,
    /// The page revisions (by graph-relative path) and parse configuration
    /// of the newest full snapshot queued or being built: what the fresh
    /// image will hold, so "does the index have these bytes" is answered
    /// from it while the old image is going away.
    snapshot: Option<(Arc<HashMap<String, String>>, tine_storage::ContentDigest)>,
    /// Index owners registered for this graph: the app's owner loop, or an
    /// inline warm while it runs. While one is, whole-graph index work is
    /// that owner's to start and nobody else's (GH #543).
    owners: usize,
    /// Whole-graph passes and worker turns that ended without making the
    /// index ready, since it last was. Reset where readiness is published.
    unsettled_passes: u32,
    /// No owner pass, and no retry of marks a failed turn returned, starts
    /// before this instant; see [`note_unsettled`].
    retry_after: Option<std::time::Instant>,
    /// The worker is waiting for another writer to release the database's
    /// writer lease; see [`lease::take_writer_lease`].
    lease_wait: bool,
    /// The index stopped trying for this session after [`owner::INDEX_ATTEMPTS`]
    /// consecutive failures, and why (GH #594, liveness L1). Cleared only by
    /// the user's retry.
    failed: Option<crate::query::IndexFailureClass>,
    /// Why the last unsettled pass or turn did not make the index ready.
    last_failure: Option<crate::query::IndexFailureClass>,
}

impl PendingProjection {
    /// The generation of the newest truth about `rel` the image holds or is
    /// being given by the running turn.
    fn held(&self, rel: &str) -> u64 {
        let applied = self.applied.get(rel).copied().unwrap_or(0);
        let in_flight = self
            .in_flight
            .get(rel)
            .map_or(0, |(generation, _)| *generation);
        self.floor.max(applied).max(in_flight)
    }

    /// The one entry of every page change into the queue. A mark older than
    /// what the image holds (or is being given) for its page, or than the mark
    /// already queued for it, is dropped: it describes a state the page has
    /// already left. Returns whether the mark was kept.
    fn record_mark(&mut self, generation: u64, delta: PageDelta) -> bool {
        self.latest_generation = self.latest_generation.max(generation);
        let key = delta.entry().rel_path.clone();
        if generation < self.held(&key)
            || self
                .marks
                .get(&key)
                .is_some_and(|(queued, _)| *queued > generation)
        {
            projection_diag(|| format!("mark dropped: generation={generation} is older"));
            return false;
        }
        self.marks.insert(key, (generation, delta));
        true
    }

    /// The change queued or in flight for `rel`, newest first.
    fn queued(&self, rel: &str) -> Option<&PageDelta> {
        self.marks
            .get(rel)
            .or_else(|| self.in_flight.get(rel))
            .map(|(_, delta)| delta)
    }

    /// Work that stands between the image and readiness.
    fn has_work(&self) -> bool {
        self.full.is_some()
            || !self.marks.is_empty()
            || !self.in_flight.is_empty()
            || self.registry_owed.is_some()
    }

    /// Whether the worker has a turn to take: a fresh snapshot, or marks for
    /// an image it keeps (none while a rebuild is owed, and none inside the
    /// backoff after a failed turn returned them).
    fn worker_can_take(&self) -> bool {
        // A failed index takes nothing until the user retries (GH #594 L1):
        // what is queued meanwhile waits for that.
        self.failed.is_none()
            && (self.full.is_some()
                || ((!self.marks.is_empty() || self.registry_owed.is_some())
                    && !self.rebuild
                    && !backing_off(self)))
    }
}

struct ProjectionShared {
    path: PathBuf,
    pending: Mutex<PendingProjection>,
    changed: Condvar,
    ready: AtomicBool,
    ready_generation: AtomicU64,
    /// Presentation invalidation only; never a requested edit or read target.
    commit_notification: AtomicU64,
    commit_waker: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    reader: Mutex<Option<PhysicalGraphProjectionDatabase>>,
    /// The ready generation at which a failed read last found the stored
    /// image intact, so a statement SQLite refuses is checked once, not on
    /// every retry (audit R10-01).
    image_verified_intact_at: Mutex<Option<u64>>,
    /// Rows contradicting each other have already cost this projection one
    /// fresh build; see [`owner::failure_owes_new_image`].
    contradiction_rebuilt: AtomicBool,
    /// Worker-owned: the running turn builds a fresh image, from the moment
    /// it decides to until its outcome is recorded. The one answer to "does a
    /// fresh build own the image's replacement?"; the progress counter also
    /// counts update and repair turns, and stops before the build publishes
    /// (GH #543, audit R12-01).
    fresh_build_running: AtomicBool,
    /// R3: the ONE admission/cancellation owner for database-owned query jobs
    /// (plan §2B). Capacity is taken before a snapshot is opened; the worker
    /// drains every job before it replaces or resets the file, and `Drop`
    /// drains before the worker is stopped.
    query_jobs: Arc<QueryJobOwner>,
    /// R3 identity (`docs/contracts/direct-query-identities.md`): the index
    /// stores every block's STRUCTURAL id; these are the live ids this
    /// session's documents carried where they differ, per page and stored
    /// source revision, newest first and at most [`LIVE_ID_REVISIONS`] of
    /// them. Recorded before the rows commit, so a snapshot of either the new
    /// image or the one before it finds the revision its rows were written
    /// at. Copy-on-write: a job clones the `Arc` beside its snapshot
    /// ([`capture_result_identity`]) and never reads it live during output.
    session_ids: Mutex<Arc<SessionLiveIds>>,
    committed_registry: SharedCommittedRegistry,
    worker_available: AtomicBool,
    worker_failed: AtomicBool,
    worker_busy: AtomicBool,
    /// True while the worker is EXECUTING a turn that carries a build — a
    /// fresh snapshot build. The
    /// queue empties the moment the worker takes that payload, so testing the
    /// queue alone reported `None` (idle) for the whole SQL transaction, and a
    /// surface that reruns on the completion edge announced a build finished
    /// in its most loaded moment (GH #543, re-audit A2-F1). `worker_busy` on
    /// its own is too broad: an ordinary one-page save turn is not a build.
    /// The writer worker has RETURNED, and every resource it owned — the
    /// SQLite writer connection and the exclusive writer lease — is closed.
    ///
    /// `worker_available` says only that the worker will take no further work;
    /// it is stored before those two locals drop. A caller that must remove the
    /// database's directory needs the stronger fact, so this flag is published
    /// by a guard declared FIRST in `projection_worker` and therefore dropped
    /// LAST. See [`DirectProjection::close_and_wait_for_worker`].
    worker_finished: AtomicBool,
    /// Resources whose destruction must follow the writer connection and
    /// lease. None closes registration once worker teardown starts.
    worker_resources: Mutex<Option<Vec<Arc<dyn Send + Sync>>>>,
    /// R6: this session has validated the complete page inventory against
    /// the projection at least once (a fresh snapshot, or the launch
    /// survey's `validated`). Until then a live delta keeps the file
    /// converging but must not publish readiness: rows of pages this session
    /// has never compared to disk could be stale from an earlier session.
    /// In-scope scenario: an external edit between two sessions, followed by
    /// a save of some other page before the survey runs.
    validated: AtomicBool,
    /// Interrupts the running background integrity check ([`integrity`]).
    integrity_check: Mutex<Option<tine_storage::sqlite::PhysicalProjectionQueryCancellation>>,
    #[cfg(test)]
    after_sql_commit: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    /// Fires after the next batch of any lowering loop, not only a fresh build's.
    after_lowering_batch: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    before_fresh_publication: Mutex<Option<Box<dyn FnOnce() -> Result<(), String> + Send>>>,
    /// Fails every fresh build just before it publishes, while set.
    #[cfg(test)]
    fresh_publication_failure: Mutex<Option<String>>,
    /// From-scratch builds this index has started.
    #[cfg(test)]
    fresh_builds: AtomicU64,
    #[cfg(test)]
    after_fresh_publication: Mutex<Option<Box<dyn FnOnce() -> Result<(), String> + Send>>>,
    #[cfg(test)]
    before_shared_reader_admission: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    after_shared_reader_admission: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    before_shared_reader_drain_lock: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    after_shared_reader_drain_lock: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    serving_writer_cache_budget: AtomicU64,
    #[cfg(test)]
    projection_health_checks: AtomicU64,
    #[cfg(test)]
    capture_thread: Mutex<Option<std::sync::mpsc::Sender<std::thread::ThreadId>>>,
    #[cfg(test)]
    indexed_reads: AtomicU64,
    /// §5.9's dispatched statements: how many times the lowering ANSWERED a
    /// user query through the seam. Separate from `indexed_reads`, which counts
    /// every seam read including the FTS-readiness probe, so a route guard can
    /// say "exactly one statement per query" and mean it.
    #[cfg(test)]
    statement_reads: AtomicU64,
    #[cfg(test)]
    registry_capture_attempts: AtomicU64,
    /// Repairs currently computing the payload they will enqueue. A repair
    /// that has not reached its `enqueue_*` yet has published nothing, so
    /// without this a query racing it reads the queue as idle and stale and
    /// its own repair attempt as one that "did not take" — a terminal
    /// `Unavailable(ReadFailed)` on a projection that is being repaired
    /// perfectly well by the thread beside it.
    repairs_in_flight: AtomicUsize,
    /// Generation moves whose delta is on its way: the mover has moved the
    /// generation and not yet queued the delta that describes it. A reader
    /// landing in that window found the index behind with nothing queued and
    /// parsed the whole graph (GH #543); it now waits, as for a queued edit.
    deltas_coming: AtomicUsize,
    /// Pages written by the running fresh build, for the indexing progress
    /// bar only (GH #543).
    build_progress: crate::indexing_progress::ProgressCounter,
    /// §5.9's failed-read injection: one read through the seam fails, exactly as
    /// a torn or truncated projection file, a disk error or a resource limit
    /// makes it fail. It exists because the obligation a failed read carries —
    /// note the fallback AND schedule the full-snapshot recovery — is invisible
    /// on a healthy projection, and an obligation nothing can observe is one a
    /// future arm silently drops (M9).
    #[cfg(test)]
    inject_read_failure: AtomicBool,
    /// The next failed-read health check finds the image damaged. An injected
    /// read failure stands for a damaged image, which is what it was written
    /// to exercise; a statement refused on an intact image is a real query.
    #[cfg(test)]
    inject_image_damage: AtomicBool,
    /// The next background integrity check finds the image damaged.
    #[cfg(test)]
    inject_integrity_damage: AtomicBool,
    /// Background integrity checks started ([`integrity`]).
    #[cfg(test)]
    integrity_checks_started: AtomicU64,
    /// Hold the next background integrity check before it opens the image.
    #[cfg(test)]
    integrity_check_pause: Mutex<Option<Arc<(std::sync::Barrier, std::sync::Barrier)>>>,
    /// Fail the worker's next this-many turns, as a disk error or a SQLite
    /// fault would.
    #[cfg(test)]
    inject_turn_failure: std::sync::atomic::AtomicU32,
    /// The worker's last turn failed. A failure on an intact image leaves
    /// nothing else behind to observe it by.
    #[cfg(test)]
    last_turn_failed: AtomicBool,
    /// The worker found the writer lease held at least once.
    #[cfg(test)]
    pub(super) lease_contended: AtomicBool,
    #[cfg(test)]
    fallback_reads: AtomicU64,
    #[cfg(test)]
    referenced_name_reads: AtomicU64,
    /// Background checkpoint passes run, retries included.
    #[cfg(test)]
    pub(super) checkpoint_passes: AtomicU64,
}

impl ProjectionShared {
    fn ready_at(&self, generation: u64) -> bool {
        self.ready.load(Ordering::Acquire)
            && self.ready_generation.load(Ordering::Acquire) == generation
    }

    fn cancel_queued_captures(&self, close: bool) -> crate::query_jobs::QueryDrainFence {
        let (fence, captures) = {
            let mut pending = self.pending.lock().unwrap();
            let fence = if close {
                pending.stop = true;
                self.query_jobs.begin_close()
            } else {
                // Reset/config replacement invalidates existing reader jobs.
                // Ordinary page deltas never enter this lifecycle boundary.
                self.query_jobs.begin_drain()
            };
            (fence, std::mem::take(&mut pending.captures))
        };
        reject_query_captures(captures);
        self.changed.notify_all();
        integrity::cancel_and_wait(self);
        if !close {
            // A close replaces nothing: the checkpoint may finish on its own.
            checkpoint::wait(self);
        }
        fence
    }

    /// R3: record the live-id exceptions of pages about to be written, BEFORE
    /// their rows commit. A page that never had an exception and has none now
    /// needs no entry: its public ids are its stored structural ids.
    pub(super) fn record_live_ids(&self, pages: Vec<(String, String, HashMap<String, String>)>) {
        let mut current = self.session_ids.lock().unwrap();
        let mut next: Option<SessionLiveIds> = None;
        for (path, revision, ids) in pages {
            let known = next.as_ref().unwrap_or(&current).contains_key(&path);
            if ids.is_empty() && !known {
                continue;
            }
            let entries = next
                .get_or_insert_with(|| (**current).clone())
                .entry(path)
                .or_default();
            entries.retain(|entry| entry.revision != revision);
            entries.insert(0, Arc::new(PageLiveIds { revision, ids }));
            entries.truncate(LIVE_ID_REVISIONS);
        }
        if let Some(next) = next {
            *current = Arc::new(next);
        }
    }

    /// R3 bookkeeping after an apply commits: a deleted page's rows are gone,
    /// and so are its exceptions.
    fn record_deleted_pages(&self, applied: &AppliedPages) {
        let mut current = self.session_ids.lock().unwrap();
        if !applied
            .deleted
            .iter()
            .any(|page| current.contains_key(page))
        {
            return;
        }
        let mut next = (**current).clone();
        for page in &applied.deleted {
            next.remove(page);
        }
        *current = Arc::new(next);
    }
}

/// How many stored revisions of one page keep their live-id exceptions: the
/// image being written and the one before it, which a snapshot opened just
/// before the commit still reads.
const LIVE_ID_REVISIONS: usize = 2;

/// Per page path, the live-id exceptions of its latest lowerings, newest
/// first (R3; see `ProjectionShared::session_ids`).
pub(crate) type SessionLiveIds = HashMap<String, Vec<Arc<PageLiveIds>>>;

/// The public-identity decoder of one snapshot (R3): the recorded exceptions
/// whose revision is the one this snapshot stores for their page. A snapshot
/// of an image recorded exceptions do not describe decodes structurally, and
/// never names one block by another's live id.
fn capture_result_identity(
    shared: &ProjectionShared,
    snapshot: &mut PhysicalProjectionQuerySnapshot,
) -> Result<crate::query::results::ResultIdentity, tine_storage::sqlite::MaterializationError> {
    let recorded = Arc::clone(&shared.session_ids.lock().unwrap());
    let mut live = HashMap::new();
    for (path, entries) in recorded.iter() {
        let mut stored = None;
        crate::query::projection_sql::visit(
            snapshot,
            "SELECT revision FROM direct_source_revisions WHERE path = ?",
            &[PhysicalQueryValue::Text(path.clone())],
            |row| {
                if let Some(PhysicalQueryValue::Text(revision)) = row.first() {
                    stored = Some(revision.clone());
                }
                Ok(std::ops::ControlFlow::Break(()))
            },
        )?;
        if let Some(entry) = stored
            .and_then(|stored| entries.iter().find(|entry| entry.revision == stored))
            .filter(|entry| !entry.ids.is_empty())
        {
            live.insert(path.clone(), Arc::clone(entry));
        }
    }
    Ok(crate::query::results::ResultIdentity {
        live: Arc::new(live),
        all_session: false,
    })
}

/// How current an index answer must be (launch design D3). Chosen at the
/// command boundary: a read inside `Graph::display_read` is only displayed and
/// may be `LaunchStored`; every other read acts on its answer and is `Current`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Currency {
    /// Ready at the reader's exact generation.
    Current,
    /// `Current`, or the stored image the last session left while the launch
    /// check has not yet validated it ([`owner::serving_stored`]). Wrong at
    /// worst until the check corrects it, which the display then re-asks.
    LaunchStored,
}

/// The generation a Gate R read is asked at, and how current its answer
/// must be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadAt {
    pub(crate) generation: u64,
    pub(crate) currency: Currency,
}

impl ReadAt {
    #[cfg(test)]
    pub(crate) fn current(generation: u64) -> Self {
        Self {
            generation,
            currency: Currency::Current,
        }
    }
}

/// Admission gate shared by admission and producer snapshot capture.
///
/// Live queries may read an older complete committed image while ordinary
/// page edits are queued. They may not read while a replacement image is being
/// built/published. Before the launch check validates the image, they read it
/// only while it is served as stored ([`owner::serving_stored`]): every query
/// is displayed, and the display re-asks when the check lands (design D2).
fn query_capture_admissible(
    shared: &ProjectionShared,
    pending: &PendingProjection,
    currency: Currency,
) -> bool {
    !pending.stop
        && !pending.rebuild
        && !shared.worker_failed.load(Ordering::Acquire)
        && !pending.building
        && (shared.validated.load(Ordering::Acquire)
            || (currency == Currency::LaunchStored && owner::serving_stored(shared, pending)))
}

fn query_capture_available(
    shared: &ProjectionShared,
    requirement: &QueryCaptureRequirement,
) -> bool {
    match requirement {
        QueryCaptureRequirement::CurrentSnapshot(currency) => {
            let pending = shared.pending.lock().unwrap();
            query_capture_admissible(shared, &pending, *currency)
        }
        #[cfg(test)]
        QueryCaptureRequirement::StrictGeneration(generation) => shared.ready_at(*generation),
    }
}

fn capture_query_job(
    shared: &ProjectionShared,
    requirement: QueryCaptureRequirement,
    registry_sensitivity: RegistrySensitivity,
    slot: crate::query_jobs::OwnedJobSlot,
) -> QueryJobOpen {
    #[cfg(test)]
    {
        let observed = shared.capture_thread.lock().unwrap().take();
        if let Some(observed) = observed {
            observed.send(std::thread::current().id()).unwrap();
        }
    }
    if slot.is_cancelled() {
        return QueryJobOpen::Cancelled;
    }
    let validate = || {
        if query_capture_available(shared, &requirement) {
            Ok(())
        } else {
            Err(tine_storage::sqlite::MaterializationError::Incomplete(
                "projection became unavailable during snapshot acquisition".into(),
            ))
        }
    };
    let mut snapshot = match PhysicalProjectionQuerySnapshot::open_direct(&shared.path, validate) {
        Ok(snapshot) => snapshot,
        // The validator is the only `Incomplete` this call can produce and
        // it means the acquisition condition changed, not a corrupt read.
        // Anything else is an unopenable or unreadable file.
        Err(_) if !query_capture_available(shared, &requirement) => return QueryJobOpen::NotReady,
        Err(_) => return QueryJobOpen::Failed,
    };
    if !slot.register(snapshot.cancellation()) {
        return QueryJobOpen::Cancelled;
    }
    let Ok(identity) = capture_result_identity(shared, &mut snapshot) else {
        return QueryJobOpen::Failed;
    };
    let query_revision = match snapshot.query_revision() {
        Ok(revision) => revision,
        Err(_) => return QueryJobOpen::Failed,
    };
    // Config is an input to every query, even when no property registry is
    // needed. Capture it beside this transaction, never from the live Graph.
    let (config, registry) = {
        let owner = shared.committed_registry.lock().unwrap();
        let Some(owner) = owner.as_ref() else {
            return QueryJobOpen::NotReady;
        };
        let registry = match registry_sensitivity {
            RegistrySensitivity::Insensitive => None,
            RegistrySensitivity::Required => {
                #[cfg(test)]
                shared
                    .registry_capture_attempts
                    .fetch_add(1, Ordering::Relaxed);
                let Ok(capture) = owner.cache.capture(query_revision, &owner.config) else {
                    return QueryJobOpen::Failed;
                };
                Some(capture)
            }
        };
        (Arc::clone(&owner.config), registry)
    };
    if slot.is_cancelled() {
        return QueryJobOpen::Cancelled;
    }
    if !query_capture_available(shared, &requirement) {
        return QueryJobOpen::NotReady;
    }
    #[cfg(test)]
    shared.statement_reads.fetch_add(1, Ordering::Relaxed);
    QueryJobOpen::Job(DirectQueryJob {
        _slot: slot,
        snapshot,
        identity,
        config,
        #[cfg(test)]
        query_revision,
        registry,
        registry_owner: Arc::clone(&shared.committed_registry),
    })
}

/// One admitted, snapshot-owning Direct query job (R3). Everything the result
/// read needs is captured here at a complete producer boundary: the pinned read transaction, the compiled-regex program already
/// installed on its connection, and the identity policy input. Dropping the
/// job releases the transaction and the capacity slot.
pub(crate) struct DirectQueryJob {
    // Field drop order is a lifecycle boundary: release the SQLite transaction
    // before the admission slot can wake a projection replacement drain.
    pub(crate) snapshot: PhysicalProjectionQuerySnapshot,
    /// Held for its `Drop`: releasing the slot is the job's only exit.
    _slot: crate::query_jobs::OwnedJobSlot,
    /// The public-identity decoder of this snapshot's rows (R3,
    /// [`capture_result_identity`]).
    pub(crate) identity: crate::query::results::ResultIdentity,
    pub(crate) config: Arc<ParseConfig>,
    /// Actual acquired SQL image, distinct from the admission target.
    #[cfg(test)]
    pub(crate) query_revision: u64,
    registry: Option<RegistryCapture>,
    registry_owner: SharedCommittedRegistry,
}

impl DirectQueryJob {
    /// Check the existing static publisher's fresh source capture against this
    /// pinned projection image. Ordinary query reads do not call this method.
    /// These fingerprints are disposable metadata, not a second source authority.
    pub(crate) fn publication_sources_match(
        &mut self,
        sources: &[(PageEntry, String)],
        capture_config: &ParseConfig,
    ) -> Result<bool, crate::query::results::ResultReadError> {
        use crate::query::results::{sql_or_cancelled, ResultReadError};
        let config_digest = capture_config.digest();
        let mut expected = sources
            .iter()
            .map(|(entry, revision)| {
                (
                    entry.rel_path.clone(),
                    projection_source_revision(revision, config_digest),
                )
            })
            .collect::<Vec<_>>();
        expected.sort_unstable();
        if expected.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(ResultReadError::Corrupt(
                "duplicate physical page in publication capture".into(),
            ));
        }
        let mut at = 0;
        let mut matches = true;
        let mut malformed = false;
        let read = crate::query::projection_sql::visit(
            &mut self.snapshot,
            "SELECT p.path, s.revision FROM pages p \
             LEFT JOIN direct_source_revisions s ON s.path = p.path \
             ORDER BY p.path",
            &[],
            |row| {
                let [PhysicalQueryValue::Text(path), PhysicalQueryValue::Text(revision)] = row
                else {
                    malformed = true;
                    return Ok(std::ops::ControlFlow::Break(()));
                };
                if !expected
                    .get(at)
                    .is_some_and(|(wanted_path, wanted_revision)| {
                        wanted_path == path && wanted_revision == revision
                    })
                {
                    matches = false;
                    return Ok(std::ops::ControlFlow::Break(()));
                }
                at += 1;
                Ok(std::ops::ControlFlow::Continue(()))
            },
        );
        read.map_err(|error| sql_or_cancelled(&self.snapshot, error))?;
        if self.snapshot.cancellation().is_cancelled() {
            return Err(ResultReadError::Cancelled);
        }
        if malformed {
            return Err(ResultReadError::Corrupt(
                "publication source fingerprint is absent or malformed".into(),
            ));
        }
        Ok(matches && at == expected.len())
    }

    /// Registry input and selection share this owned transaction. This scans
    /// metadata, not result payload; inference remains build_registry's job.
    pub(crate) fn read_registry(
        &mut self,
        config: &ParseConfig,
    ) -> Result<Arc<crate::query::registry::Registry>, crate::query::QueryExecutionError> {
        #[cfg(test)]
        REGISTRY_READ_ATTEMPTS.with(|count| count.set(count.get() + 1));
        let capture =
            self.registry
                .as_ref()
                .ok_or(crate::query::QueryExecutionError::Unavailable(
                    crate::query::QueryUnavailableReason::InvalidSnapshot,
                ))?;
        let built = capture.build(&mut self.snapshot, config)?;
        let mut owner = self.registry_owner.lock().unwrap();
        // A failed/replaced producer cannot seed its successor from this job.
        // The captured result itself remains coherent with the owned read.
        match owner.as_mut() {
            Some(owner) => owner.cache.publish(capture.clone(), built),
            None => Ok(built),
        }
    }
}

#[cfg(test)]
impl DirectQueryJob {
    /// True once a drain (rebuild, reset, close) has cancelled this job. The
    /// production read checks the snapshot's own sticky flag between batches;
    /// this is the slot's view, for the drain tests.
    pub(crate) fn is_cancelled(&self) -> bool {
        self._slot.is_cancelled()
    }
}

/// What one attempt to open a query job produced (R3; the §5.9 states plus
/// the two the job owner adds).
pub(crate) enum QueryJobOpen {
    Job(DirectQueryJob),
    /// Not ready at this generation, or the generation moved while the
    /// snapshot was being pinned. Nothing is wrong with the projection;
    /// `ProjectionProgress` decides whether readiness is on its way.
    NotReady,
    /// No capacity slot freed within the admission wait (R3). Distinct from
    /// `NotReady`: the projection IS ready and other jobs are draining, so the
    /// caller owes a retry and never a repair.
    Busy,
    /// The snapshot could not be opened or the regex program could not be
    /// installed: a failed read, owed recovery.
    Failed,
    /// A drain or close cancelled the job before it ran. No recovery is owed
    /// against a projection that is being replaced on purpose.
    Cancelled,
}

/// Whether a query that found the projection NOT READY can expect readiness to
/// arrive on its own, needs one repair, or must stop retrying (RET2).
///
/// The vocabulary is deliberately the queue's own: this reads the existing
/// `pending` queue plus `worker_available` / `worker_failed` / `worker_busy`
/// and translates them into the three answers the public boundary can act on.
/// It adds no state of its own, because a second opinion about whether the
/// worker is making progress is exactly the twin D-14 forbids.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProjectionProgress {
    /// Ready at this generation by the time the question was asked: the two
    /// reads straddled a save. Retryable.
    Ready,
    /// Queued or in-flight work will publish readiness. Retryable, with the
    /// reason the queue is holding it.
    Working(crate::query::QueryReadinessReason),
    /// Nothing is queued, the worker is idle, and the projection is stale at
    /// this generation. Only a repair can make it ready.
    Stale,
    /// The worker thread is gone — it never started, lost the writer lease, or
    /// returned. No repair this graph can schedule will be picked up, so a
    /// retry loop here would never end.
    Stopped,
    /// The index could not be built or updated and has stopped trying this
    /// session (GH #594, liveness L1). Terminal until the user retries.
    Failed(crate::query::IndexFailureClass),
}

/// Whether the projection can narrow THIS reference target at all.
///
/// A `Plain` (unlinked) target with no alphanumeric character has no usable
/// posting to look up, so the index cannot say which pages might contain it and
/// the exact parser walk is the only answer. This is a property of the TARGET,
/// not of the projection's readiness, which is why it is a free function: the
/// caller that decides between "wait for the index" and "walk every page" must
/// ask the same question the read itself asks, and one definition is the only
/// way those two can stay in agreement.
pub(crate) fn reference_narrowing_supported(names_norm: &[String], kind: ReferenceKind) -> bool {
    kind != ReferenceKind::Plain
        || names_norm
            .iter()
            .all(|name| name.chars().any(char::is_alphanumeric))
}

/// One reference target's candidate set, as the index can name it.
///
/// `blocks` is `Some` only when the index enumerated the referring blocks for
/// EVERY name in the target's equivalence class. A `None` there means "classify
/// every block of every candidate page", which is what the walk did before this
/// existed, so a partial index can never silently drop a row.
pub(crate) struct ReferenceCandidateIndex {
    pub paths: std::collections::BTreeSet<PathBuf>,
    pub blocks: Option<std::collections::HashSet<String>>,
    /// Page entities admitted by an Interactive verified window. `None` keeps
    /// the established Exhaustive/explicit/fallback behavior: every loaded
    /// candidate page may contribute its property preamble. A containing path
    /// admitted only by one of its blocks is deliberately absent from `Some`.
    pub page_owners: Option<std::collections::HashSet<PathBuf>>,
}

#[cfg(test)]
thread_local! {
    static PLAIN_REFERENCE_EXACT_CALLBACKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PLAIN_REFERENCE_QUERY_PLANS: std::cell::RefCell<Vec<Vec<String>>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn reset_plain_reference_query_instrumentation() {
    PLAIN_REFERENCE_EXACT_CALLBACKS.with(|count| count.set(0));
    PLAIN_REFERENCE_QUERY_PLANS.with(|plans| plans.borrow_mut().clear());
}

#[cfg(test)]
pub(crate) fn plain_reference_query_instrumentation() -> (usize, Vec<Vec<String>>) {
    let callbacks = PLAIN_REFERENCE_EXACT_CALLBACKS.with(std::cell::Cell::get);
    let plans = PLAIN_REFERENCE_QUERY_PLANS.with(|plans| plans.borrow().clone());
    (callbacks, plans)
}

#[cfg(test)]
fn capture_plain_reference_query_plan(
    path: &Path,
    sql: &str,
    params: &[PhysicalQueryValue],
) -> Option<()> {
    let plan_reader = tine_storage::sqlite::PhysicalProjectionQueryReader::open(path).ok()?;
    plan_reader.set_query_rank_function(|_, _| Ok(None)).ok()?;
    let plan = plan_reader.explain_query_plan(sql, params).ok()?;
    PLAIN_REFERENCE_QUERY_PLANS.with(|plans| plans.borrow_mut().push(plan));
    Some(())
}

/// Direct Files' disposable parser-fact projection.
///
/// The foreground only publishes already-parsed `Arc<Document>` snapshots into
/// a coalescing page map. One worker owns SQLite, so an editor save never waits
/// for schema work, SQL, disk flushes, or a graph-sized rebuild. Read paths may
/// use the database only at the exact current cache generation.
pub(crate) struct DirectProjection {
    shared: Arc<ProjectionShared>,
}

impl DirectProjection {
    /// Subscribe the existing application watcher before observing the latest
    /// published image, so a commit cannot fall between registration and read.
    pub(crate) fn observe_commits(&self, wake: std::sync::mpsc::Sender<()>) -> u64 {
        let mut notifier = self.shared.commit_waker.lock().unwrap();
        *notifier = Some(wake);
        self.shared.commit_notification.load(Ordering::Acquire)
    }

    /// Start the projection of the graph whose index lives at `path`.
    /// `launch_config` is the parse configuration the graph opens with: a
    /// stored image written under it (and under this facts version) is served
    /// to display reads while the launch check runs (design D2). `None`
    /// serves nothing before the check.
    pub(crate) fn start(
        path: PathBuf,
        launch_config: Option<Arc<ParseConfig>>,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(ProjectionShared {
            path,
            pending: Mutex::new(PendingProjection::default()),
            changed: Condvar::new(),
            ready: AtomicBool::new(false),
            ready_generation: AtomicU64::new(0),
            commit_notification: AtomicU64::new(0),
            commit_waker: Mutex::new(None),
            reader: Mutex::new(None),
            image_verified_intact_at: Mutex::new(None),
            contradiction_rebuilt: AtomicBool::new(false),
            fresh_build_running: AtomicBool::new(false),
            query_jobs: Arc::new(QueryJobOwner::new(DEFAULT_QUERY_JOB_CAPACITY)),
            session_ids: Mutex::new(Arc::default()),
            committed_registry: Arc::new(Mutex::new(None)),
            worker_available: AtomicBool::new(true),
            worker_failed: AtomicBool::new(false),
            worker_busy: AtomicBool::new(false),
            worker_finished: AtomicBool::new(false),
            worker_resources: Mutex::new(Some(Vec::new())),
            validated: AtomicBool::new(false),
            integrity_check: Mutex::new(None),
            #[cfg(test)]
            after_sql_commit: Mutex::new(None),
            #[cfg(test)]
            after_lowering_batch: Mutex::new(None),
            #[cfg(test)]
            before_fresh_publication: Mutex::new(None),
            #[cfg(test)]
            fresh_publication_failure: Mutex::new(None),
            #[cfg(test)]
            fresh_builds: AtomicU64::new(0),
            #[cfg(test)]
            after_fresh_publication: Mutex::new(None),
            #[cfg(test)]
            before_shared_reader_admission: Mutex::new(None),
            #[cfg(test)]
            after_shared_reader_admission: Mutex::new(None),
            #[cfg(test)]
            before_shared_reader_drain_lock: Mutex::new(None),
            #[cfg(test)]
            after_shared_reader_drain_lock: Mutex::new(None),
            #[cfg(test)]
            serving_writer_cache_budget: AtomicU64::new(0),
            #[cfg(test)]
            projection_health_checks: AtomicU64::new(0),
            #[cfg(test)]
            capture_thread: Mutex::new(None),
            #[cfg(test)]
            indexed_reads: AtomicU64::new(0),
            #[cfg(test)]
            statement_reads: AtomicU64::new(0),
            #[cfg(test)]
            registry_capture_attempts: AtomicU64::new(0),
            repairs_in_flight: AtomicUsize::new(0),
            deltas_coming: AtomicUsize::new(0),
            build_progress: Default::default(),
            #[cfg(test)]
            inject_read_failure: AtomicBool::new(false),
            #[cfg(test)]
            inject_image_damage: AtomicBool::new(false),
            #[cfg(test)]
            inject_integrity_damage: AtomicBool::new(false),
            #[cfg(test)]
            integrity_checks_started: AtomicU64::new(0),
            #[cfg(test)]
            integrity_check_pause: Mutex::new(None),
            #[cfg(test)]
            inject_turn_failure: std::sync::atomic::AtomicU32::new(0),
            #[cfg(test)]
            last_turn_failed: AtomicBool::new(false),
            #[cfg(test)]
            lease_contended: AtomicBool::new(false),
            #[cfg(test)]
            fallback_reads: AtomicU64::new(0),
            #[cfg(test)]
            referenced_name_reads: AtomicU64::new(0),
            #[cfg(test)]
            checkpoint_passes: AtomicU64::new(0),
        });
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("tine-direct-projection".into())
            .spawn(move || projection_worker(worker, launch_config))?;
        Ok(Self { shared })
    }

    /// Publish that a repair is computing its payload. `progress_at` reports
    /// `Working(Recovering)` for as long as the returned guard lives, so a
    /// concurrent query waits for it instead of declaring the repair failed.
    pub(crate) fn begin_repair(&self) -> RepairInFlight {
        self.shared.repairs_in_flight.fetch_add(1, Ordering::AcqRel);
        RepairInFlight(Arc::clone(&self.shared))
    }

    /// Announce a generation move whose delta the caller queues next; see
    /// `DeltaComing`.
    pub(crate) fn delta_coming(&self) -> DeltaComing {
        self.shared.deltas_coming.fetch_add(1, Ordering::AcqRel);
        DeltaComing(Arc::clone(&self.shared))
    }

    /// True while a failed turn owes a new image (K1: it was building one, or
    /// the image is damaged). A turn that failed on an intact image returns
    /// its marks instead and leaves this clear. The flag clears on the next
    /// successful turn.
    #[cfg(test)]
    pub(crate) fn worker_failed(&self) -> bool {
        self.shared.worker_failed.load(Ordering::Acquire)
    }

    /// True while the writer worker accepts work (it holds the lease and has
    /// not been told to stop).
    #[cfg(test)]
    pub(crate) fn worker_available(&self) -> bool {
        self.shared.worker_available.load(Ordering::Acquire)
    }

    /// Whether this session has compared the image with the graph (the
    /// launch survey finished, or a fresh build published). A queued edit may
    /// lower onto an image the session has not surveyed, but readiness is
    /// never published over one: `index_need` answers `Validate` until this
    /// is set. So a pending edit on an unsurveyed image is not by itself
    /// coming (GH #543, audit R9-14).
    pub(crate) fn validated(&self) -> bool {
        self.shared.validated.load(Ordering::Acquire)
    }

    /// Queue a fresh build from `pages`, captured at `generation`.
    ///
    /// A snapshot older than the queue is refused: the worker may already
    /// have applied a newer mark that the snapshot does not contain, and a
    /// fresh image built from it would silently lose that page change while
    /// readiness said otherwise (third audit A3-N1). The need stays `Fresh`,
    /// and the owner assembles a snapshot at the current generation.
    ///
    /// Accepting it sets the floor: every mark at or before `generation` is
    /// in the snapshot, so the queue drops them now and any that arrive late.
    pub(crate) fn enqueue_full(
        &self,
        generation: u64,
        pages: PageSnapshot,
        revisions: PageRevisions,
        parse_config: Arc<ParseConfig>,
        retained: Vec<String>,
    ) -> bool {
        let mut pending = self.shared.pending.lock().unwrap();
        // A full snapshot is the fresh build's input and nothing else: over an
        // image nobody owes a rebuild it would only re-lower every page and
        // drop readiness meanwhile (GH #543 round 2).
        if !pending.rebuild {
            projection_diag(|| {
                format!("full refused at generation={generation}: no fresh image owed")
            });
            return false;
        }
        if generation < pending.latest_generation {
            projection_diag(|| {
                format!(
                    "full refused: snapshot generation={generation} older than queue {}",
                    pending.latest_generation,
                )
            });
            return false;
        }
        self.shared.ready.store(false, Ordering::Release);
        pending.snapshot = Some((
            Arc::new(
                pages
                    .iter()
                    .filter_map(|(entry, _)| {
                        revisions
                            .get(&entry.path)
                            .map(|revision| (entry.rel_path.clone(), revision.clone()))
                    })
                    .collect(),
            ),
            parse_config.digest(),
        ));
        pending.full = Some(PendingFull {
            pages,
            revisions,
            parse_config,
            retained,
        });
        pending.marks.clear();
        pending.applied.clear();
        pending.registry_owed = None;
        pending.floor = generation;
        pending.latest_generation = generation;
        drop(pending);
        self.shared.changed.notify_all();
        true
    }

    /// R6: the projected page inventory as `(name, path, kind)` rows,
    /// read through `drain_after` from the ready projection. `list_pages`
    /// rebuilds `PageEntry`s from it instead of parsing every file.
    pub(crate) fn page_inventory(&self, at: ReadAt) -> Option<Vec<(String, String, PageKind)>> {
        let reader = self.shared_reader_at(at)?;
        let read = reader.as_ref()?.read();
        let mut rows = Vec::new();
        drain_after(
            |cursor: Option<(i64, String)>, batch| {
                read.navigation_pages_after_with_header_validation(
                    cursor.as_ref().map(|(_, path)| path.as_str()),
                    cursor.as_ref().map(|(id, _)| *id),
                    batch,
                    |_, kind| derived_reads::page_kind(kind).map(|_| ()),
                )
            },
            |row| (row.cursor, row.path.clone()),
            |row| {
                rows.push((row.name, row.path, derived_reads::page_kind(row.text_kind)?));
                Ok(())
            },
            |error, batch| {
                matches!(
                    error,
                    tine_storage::sqlite::MaterializationError::ResourceLimit { .. }
                )
                .then(|| (batch / 2).max(1))
            },
        )
        .reported(self)?;
        self.answers(at).then_some(rows)
    }

    /// Bounded wait until the index answers `at` (R6): the whole-graph derived
    /// reads that would otherwise fall to a full parse in the milliseconds
    /// after a save or a warm turn wait for that bounded worker turn first.
    /// Same ceiling and same non-authority as `wait_for_reference_generation`.
    #[must_use = "a readiness wait that timed out must fail the test or be handled (GH #543, R9-15e)"]
    pub(crate) fn wait_ready_at(&self, at: ReadAt) -> bool {
        self.wait_for_reference_generation(at)
    }

    /// Unbounded wait for readiness at `generation`, for an index owner only
    /// (GH #543): a survey's marks or a fresh build on a large graph can
    /// outlast the bounded wait used by derived-map reads. Returns `false`
    /// when readiness at this generation is no longer coming: a newer
    /// generation, the worker gone or failed, an idle queue that did not
    /// publish, owner work next, or `cancelled`.
    ///
    /// Owner work next: a rebuild, a validation or a failed index's retry is
    /// work only an owner pass does, and the worker takes no queued edit while
    /// a rebuild is owed. The caller is an owner, so waiting would wait on
    /// itself -- forever when nothing cancels it (the CLI's `warm_cache`; a
    /// rebuild the integrity check requested beside a queued edit hung a test
    /// for 600 s).
    #[must_use = "a readiness wait that timed out must fail the test or be handled (GH #543, R9-15e)"]
    pub(crate) fn wait_until_ready_at(
        &self,
        generation: u64,
        cancelled: &impl Fn() -> bool,
    ) -> bool {
        let mut pending = self.shared.pending.lock().unwrap();
        loop {
            if self.ready_at(generation) {
                return true;
            }
            if cancelled()
                || pending.stop
                || pending.lease_wait
                || !self.shared.worker_available.load(Ordering::Acquire)
                || self.shared.worker_failed.load(Ordering::Acquire)
                || pending.latest_generation > generation
                || matches!(
                    owner::index_need(&self.shared, &pending),
                    IndexNeed::Fresh | IndexNeed::Validate | IndexNeed::Failed
                )
            {
                return false;
            }
            if !pending.has_work() && !self.shared.worker_busy.load(Ordering::Acquire) {
                return false;
            }
            pending = self
                .shared
                .changed
                .wait_timeout(pending, std::time::Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }

    pub(crate) fn enqueue_replace(
        &self,
        generation: u64,
        entry: PageEntry,
        document: Arc<Document>,
        revision: String,
        parse_config: Arc<ParseConfig>,
    ) {
        self.enqueue_delta(
            generation,
            PageDelta::Replace {
                entry,
                document,
                revision,
                parse_config,
            },
        );
    }

    pub(crate) fn enqueue_delete(&self, generation: u64, entry: PageEntry) {
        self.enqueue_delta(generation, PageDelta::Delete { entry });
    }

    /// One page-set change published as ONE queue transaction.
    ///
    /// The worker drains whatever is queued the moment it wakes, so a producer
    /// that enqueues its marks one at a time can have the queue empty
    /// underneath it: a rename's `Delete` was drained on its own, the watermark
    /// already read as the new generation, and readiness was published over an
    /// image whose `Replace` had not been enqueued yet — search answered
    /// "complete" over a graph that was missing the page entirely (fourth audit
    /// A4-N2). Taking the lock once makes the whole change one step.
    pub(crate) fn enqueue_page_set(
        &self,
        generation: u64,
        changes: Vec<PageSetChange>,
        parse_config: Arc<ParseConfig>,
    ) {
        // A mover that claimed `IndexEffect::Sent` and then had nothing to
        // send (every replacement failed to parse) still owes the index its
        // generation (GH #543, audit R8-07).
        if changes.is_empty() {
            self.advance_generation(generation);
            return;
        }
        self.shared.ready.store(false, Ordering::Release);
        let mut pending = self.shared.pending.lock().unwrap();
        for change in changes {
            let delta = match change {
                PageSetChange::Replace {
                    entry,
                    document,
                    revision,
                } => PageDelta::Replace {
                    entry,
                    document,
                    revision,
                    parse_config: Arc::clone(&parse_config),
                },
                PageSetChange::Delete { entry } => PageDelta::Delete { entry },
            };
            pending.record_mark(generation, delta);
        }
        publish_if_current(&self.shared, &mut pending);
        drop(pending);
        self.shared.changed.notify_all();
    }

    fn enqueue_delta(&self, generation: u64, delta: PageDelta) {
        self.shared.ready.store(false, Ordering::Release);
        let mut pending = self.shared.pending.lock().unwrap();
        pending.record_mark(generation, delta);
        // A dropped mark leaves nothing to wait for.
        publish_if_current(&self.shared, &mut pending);
        drop(pending);
        self.shared.changed.notify_all();
    }

    /// The launch survey's page changes, observed at `generation` (loaded
    /// before the survey read anything). Each is a mark like any other: a
    /// save the survey raced is newer and wins (GH #543, audit R14-03).
    pub(crate) fn record_survey_marks(
        &self,
        generation: u64,
        changes: Vec<PageSetChange>,
        parse_config: Arc<ParseConfig>,
    ) {
        if changes.is_empty() {
            return;
        }
        let mut pending = self.shared.pending.lock().unwrap();
        let mut kept = false;
        for change in changes {
            let delta = match change {
                PageSetChange::Replace {
                    entry,
                    document,
                    revision,
                } => PageDelta::Replace {
                    entry,
                    document,
                    revision,
                    parse_config: Arc::clone(&parse_config),
                },
                PageSetChange::Delete { entry } => PageDelta::Delete { entry },
            };
            kept |= pending.record_mark(generation, delta);
        }
        if kept {
            self.shared.ready.store(false, Ordering::Release);
        }
        drop(pending);
        self.shared.changed.notify_all();
    }

    /// The survey has compared the image with the graph at `generation`:
    /// every page whose bytes differed is now a mark. Readiness follows as
    /// soon as those are applied.
    pub(crate) fn survey_validated(&self, generation: u64, parse_config: Arc<ParseConfig>) {
        let mut pending = self.shared.pending.lock().unwrap();
        pending.latest_generation = pending.latest_generation.max(generation);
        pending.registry_owed = Some(parse_config);
        self.shared.ready.store(false, Ordering::Release);
        self.shared.validated.store(true, Ordering::Release);
        publish_if_current(&self.shared, &mut pending);
        drop(pending);
        self.shared.changed.notify_all();
    }

    /// The survey found more of the image stale than a page-by-page repair
    /// is worth ([`repair_is_proportionate`]): a fresh image is owed.
    pub(crate) fn survey_owes_fresh_build(&self) {
        let mut pending = self.shared.pending.lock().unwrap();
        pending.rebuild = true;
        self.shared.ready.store(false, Ordering::Release);
        drop(pending);
        self.shared.changed.notify_all();
    }

    /// The graph moved to `generation` without changing anything this index
    /// holds beyond what is already queued: a page became unreadable, or
    /// readable again before its mark, and its rows stay as they are; or a
    /// survey announced the findings it has recorded as marks. An index ready at the previous
    /// generation is ready at this one; one with work queued publishes
    /// readiness at the latest generation when the work drains. Without this
    /// the index stayed not-ready with nothing coming, and every indexed read
    /// fell back to parsing the graph (GH #543, audit R4-03).
    pub(crate) fn advance_generation(&self, generation: u64) {
        let mut pending = self.shared.pending.lock().unwrap();
        pending.latest_generation = pending.latest_generation.max(generation);
        if !self.shared.worker_busy.load(Ordering::Acquire)
            && self.shared.ready.load(Ordering::Acquire)
        {
            publish_if_current(&self.shared, &mut pending);
        }
        drop(pending);
        self.shared.changed.notify_all();
    }

    /// Whether the index holds, or is being given, a page whose
    /// graph-relative path starts with `prefix` (a directory path ending in
    /// `/`); `None` when it has no image to answer from.
    pub(crate) fn holds_pages_under(&self, prefix: &str) -> Option<bool> {
        let (queued, deleted) = {
            let pending = self.shared.pending.lock().unwrap();
            if !pending.set_up || pending.rebuild || pending.full.is_some() {
                return None;
            }
            let mut queued = false;
            let mut deleted = HashSet::new();
            for (path, (_, delta)) in pending.in_flight.iter().chain(pending.marks.iter()) {
                if !path.starts_with(prefix) {
                    continue;
                }
                match delta {
                    PageDelta::Replace { .. } => {
                        queued = true;
                        deleted.remove(path);
                    }
                    PageDelta::Delete { .. } => {
                        deleted.insert(path.clone());
                    }
                }
            }
            (queued, deleted)
        };
        if queued {
            return Some(true);
        }
        self.image_paths_under(prefix, &deleted)
    }

    /// A reference read which races an already-queued one-page fact delta is
    /// much cheaper if it waits for that bounded worker turn than if it scans
    /// every parsed page. The timeout is a latency ceiling, not an authority:
    /// failure, worker loss, a newer generation, or expiry all return `false`
    /// and the caller uses the exact parser fallback.
    #[must_use = "a readiness wait that timed out must fail the test or be handled (GH #543, R9-15e)"]
    ///
    /// A [`Currency::LaunchStored`] read also stops waiting when the stored
    /// image serves again: an edit made during the launch check is applied
    /// in one turn, and the read then sees it (design D2, read-your-writes).
    pub(crate) fn wait_for_reference_generation(&self, at: ReadAt) -> bool {
        let generation = at.generation;
        if self.answers(at) {
            return true;
        }
        let deadline = std::time::Instant::now() + REFERENCE_DELTA_WAIT;
        let mut pending = self.shared.pending.lock().unwrap();
        loop {
            if self.ready_at(generation)
                || (at.currency == Currency::LaunchStored
                    && owner::serving_stored(&self.shared, &pending))
            {
                return true;
            }
            if !self.shared.worker_available.load(Ordering::Acquire)
                || self.shared.worker_failed.load(Ordering::Acquire)
                || self.shared.ready_generation.load(Ordering::Acquire) > generation
                || pending.latest_generation > generation
            {
                return false;
            }
            if !pending.has_work() && !self.shared.worker_busy.load(Ordering::Acquire) {
                return false;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let (next, timeout) = self
                .shared
                .changed
                .wait_timeout(pending, deadline - now)
                .unwrap();
            pending = next;
            if timeout.timed_out() && !self.ready_at(generation) {
                return false;
            }
        }
    }

    pub(crate) fn property_facets(
        &self,
        at: ReadAt,
        autocomplete: bool,
        hidden_properties: &[String],
        max_items: usize,
        max_bytes: usize,
    ) -> Option<(Vec<(String, Vec<String>)>, bool)> {
        let reader = self.shared_reader_at(at)?;
        let read = reader.as_ref()?.read();
        let mut accumulator = if autocomplete {
            PropertyFacetAccumulator::autocomplete(hidden_properties, max_items, max_bytes)
        } else {
            PropertyFacetAccumulator::query_builder(max_items, max_bytes)
        };
        drain_after(
            |cursor, batch| read.property_facet_rows_after(!autocomplete, cursor, batch),
            |row| {
                let owner = match row.owner {
                    PhysicalEntityId::Page(_) => PhysicalEntityCoordinate::Page(row.owner_cursor),
                    PhysicalEntityId::Block(_) => PhysicalEntityCoordinate::Block(row.owner_cursor),
                };
                (owner, row.name_cursor, row.ordinal)
            },
            |row| {
                accumulator.offer(&row.normalized_name, &row.value);
                Ok(())
            },
            |error, batch| {
                matches!(
                    error,
                    tine_storage::sqlite::MaterializationError::ResourceLimit { .. }
                )
                .then(|| (batch / 2).max(1))
            },
        )
        .reported(self)?;
        if !self.answers(at) {
            return None;
        }
        #[cfg(test)]
        self.shared.indexed_reads.fetch_add(1, Ordering::Relaxed);
        Some(accumulator.finish())
    }

    /// The §6.2 registry row source for **Direct Files, projection ready**: the
    /// ready raw property stream plus the same-snapshot `page_id → (format,
    /// name)` map, taken under ONE read of the projection database.
    ///
    /// **CLOSURE §4 rejects deferring this to the document walk.** The wrapper
    /// above (`property_facets`) aggregates owner identity away, so it cannot
    /// serve a registry that reports cardinality and distinct-owner counts; and
    /// answering a registry read by walking every hydrated document is exactly
    /// the graph-wide scan the ready projection exists to avoid. This is an
    /// ADAPTER onto the one `build_registry` aggregator, not a competing
    /// registry producer: it yields the same [`OwnerRow`] stream the cold document
    /// iterator yields, and the
    /// aggregator downstream is byte-for-byte the same function.
    ///
    /// `None` means "not ready, or the read refused" — the caller falls back to
    /// the document iterator, exactly as §5.9's dispatch does for queries.
    #[cfg(test)]
    pub(crate) fn property_owner_rows(
        &self,
        at: ReadAt,
    ) -> Option<(
        Vec<crate::query::registry::OwnerRow>,
        HashMap<String, crate::query::registry::PageMeta>,
    )> {
        use crate::query::registry::{OwnerRow, OwnerType, PageMeta};
        #[cfg(test)]
        REGISTRY_READ_ATTEMPTS.with(|count| count.set(count.get() + 1));

        let reader = self.shared_reader_at(at)?;
        let read = reader.as_ref()?.read();

        // The page map and the rows are read from the SAME `read`, i.e. the same
        // snapshot: a row naming a page the map does not have is a
        // snapshot-consistency defect and fails the build (§6.2), never a
        // silent fallback to Markdown.
        let mut pages: HashMap<String, PageMeta> = HashMap::new();
        let mut page_ids = HashMap::new();
        drain_after(
            |cursor: Option<(i64, String)>, batch| {
                read.navigation_pages_after_with_header_validation(
                    cursor.as_ref().map(|(_, path)| path.as_str()),
                    cursor.as_ref().map(|(id, _)| *id),
                    batch,
                    |_, kind| derived_reads::page_kind(kind).map(|_| ()),
                )
            },
            |row| (row.cursor, row.path.clone()),
            |row| {
                page_ids.insert(row.path.clone(), row.cursor);
                pages.insert(
                    crate::query::registry_sql::page_key(row.cursor),
                    PageMeta {
                        // §6.2 E4: `Format::from_path`, case-insensitive —
                        // never `reference_source_is_org`.
                        format: Format::from_path(Path::new(&row.path)).into(),
                        name: row.name,
                    },
                );
                Ok(())
            },
            |error, batch| {
                matches!(
                    error,
                    tine_storage::sqlite::MaterializationError::ResourceLimit { .. }
                )
                .then(|| (batch / 2).max(1))
            },
        )
        .reported(self)?;

        let mut rows: Vec<OwnerRow> = Vec::new();
        drain_after(
            |cursor, batch| read.property_facet_rows_after(false, cursor, batch),
            |row| {
                let owner = match row.owner {
                    PhysicalEntityId::Page(_) => PhysicalEntityCoordinate::Page(row.owner_cursor),
                    PhysicalEntityId::Block(_) => PhysicalEntityCoordinate::Block(row.owner_cursor),
                };
                (owner, row.name_cursor, row.ordinal)
            },
            |row| {
                let (owner_type, owner_id) = match row.owner {
                    PhysicalEntityId::Page(_) => {
                        (OwnerType::Page, format!("p:{}", row.owner_cursor))
                    }
                    PhysicalEntityId::Block(_) => {
                        (OwnerType::Block, format!("b:{}", row.owner_cursor))
                    }
                };
                let page_id = page_ids.get(&row.page_path).ok_or_else(|| {
                    tine_storage::sqlite::MaterializationError::Corrupt(
                        "registry property names an absent page path".into(),
                    )
                })?;
                rows.push(OwnerRow {
                    owner_type,
                    owner_id,
                    page_id: crate::query::registry_sql::page_key(*page_id),
                    source_name: row.source_name,
                    normalized_name: row.normalized_name,
                    ordinal: row.ordinal,
                    value: row.value,
                });
                Ok(())
            },
            |error, batch| {
                matches!(
                    error,
                    tine_storage::sqlite::MaterializationError::ResourceLimit { .. }
                )
                .then(|| (batch / 2).max(1))
            },
        )
        .reported(self)?;

        // The generation must still hold AFTER both scans, or the two halves
        // could straddle a rebuild — the same re-check `property_facets` makes.
        if !self.answers(at) {
            return None;
        }
        #[cfg(test)]
        self.shared.indexed_reads.fetch_add(1, Ordering::Relaxed);
        Some((rows, pages))
    }

    /// R3: open a database-owned query job at the current cache generation.
    ///
    /// Capacity is acquired on the caller before enqueueing a capture. The
    /// producer opens the snapshot and captures session identity between write
    /// turns, validating `ready_at(generation)` around the transaction. A
    /// registry-sensitive request also freezes its registry input there; an
    /// insensitive request carries none. The producer registers cancellation
    /// before handing the owned job back. Selection and payload construction
    /// then execute on the caller, off the producer.
    /// The statement's compiled-regex program is installed by
    /// `query::results::read_results` on the job's own connection — the ONE
    /// install site — so a job carries no regex state of its own.
    #[cfg(test)]
    pub(crate) fn open_query_job_for(
        &self,
        cache_generation: u64,
        registry_sensitivity: RegistrySensitivity,
    ) -> QueryJobOpen {
        if !self.ready_at(cache_generation) {
            return QueryJobOpen::NotReady;
        }
        self.enqueue_query_capture(
            QueryCaptureRequirement::StrictGeneration(cache_generation),
            registry_sensitivity,
        )
    }

    /// Existing reader lifecycle identity; ordinary saves do not advance it.
    pub(crate) fn query_epoch(&self) -> crate::query_jobs::QueryJobEpoch {
        self.shared.query_jobs.capture_epoch()
    }

    /// Acquire the current complete projection without waiting for a saved edit.
    /// Ordinary queued deltas do not invalidate the committed image.
    /// A [`Currency::LaunchStored`] job may read the stored image during the
    /// launch check (design D2).
    pub(crate) fn open_current_query_job(
        &self,
        registry_sensitivity: RegistrySensitivity,
        currency: Currency,
    ) -> QueryJobOpen {
        let requirement = QueryCaptureRequirement::CurrentSnapshot(currency);
        if !self.shared.worker_available.load(Ordering::Acquire)
            || !query_capture_available(&self.shared, &requirement)
        {
            return QueryJobOpen::NotReady;
        }
        self.enqueue_query_capture(requirement, registry_sensitivity)
    }

    fn enqueue_query_capture(
        &self,
        requirement: QueryCaptureRequirement,
        registry_sensitivity: RegistrySensitivity,
    ) -> QueryJobOpen {
        #[cfg(test)]
        if self
            .shared
            .inject_read_failure
            .swap(false, Ordering::AcqRel)
        {
            return QueryJobOpen::Failed;
        }
        let slot = match self
            .shared
            .query_jobs
            .acquire_owned_at_within(self.shared.query_jobs.capture_epoch(), QUERY_JOB_WAIT)
        {
            OwnedAdmission::Slot(slot) => slot,
            OwnedAdmission::Cancelled => return QueryJobOpen::Cancelled,
            // RET2: capacity, not readiness. The projection is ready and other
            // jobs are draining, so this is the one `NotReady` the public
            // boundary may retry without ever considering a repair.
            OwnedAdmission::Busy => return QueryJobOpen::Busy,
        };
        let (reply, result) = std::sync::mpsc::sync_channel(1);
        {
            let mut pending = self.shared.pending.lock().unwrap();
            if pending.stop || slot.is_cancelled() {
                return QueryJobOpen::Cancelled;
            }
            if !self.shared.worker_available.load(Ordering::Acquire) {
                return QueryJobOpen::Failed;
            }
            let available = match &requirement {
                QueryCaptureRequirement::CurrentSnapshot(currency) => {
                    query_capture_admissible(&self.shared, &pending, *currency)
                }
                #[cfg(test)]
                QueryCaptureRequirement::StrictGeneration(generation) => self.ready_at(*generation),
            };
            if !available {
                return QueryJobOpen::NotReady;
            }
            pending.captures.push(PendingQueryCapture {
                requirement,
                registry_sensitivity,
                slot,
                reply,
            });
        }
        self.shared.changed.notify_all();
        result.recv().unwrap_or(QueryJobOpen::Failed)
    }

    /// Existing low-level fixtures exercise the stronger registry-bearing job.
    #[cfg(test)]
    pub(crate) fn open_query_job(&self, cache_generation: u64) -> QueryJobOpen {
        self.open_query_job_for(cache_generation, RegistrySensitivity::Required)
    }

    #[cfg(test)]
    pub(crate) fn session_ids_test(&self) -> Arc<SessionLiveIds> {
        Arc::clone(&self.shared.session_ids.lock().unwrap())
    }

    #[cfg(test)]
    pub(crate) fn active_query_jobs_test(&self) -> usize {
        self.shared.query_jobs.active()
    }

    pub(crate) fn note_fallback_read(&self) {
        #[cfg(test)]
        self.shared.fallback_reads.fetch_add(1, Ordering::Relaxed);
    }

    /// Admit a pooled projection read under the same mutex publication drains.
    ///
    /// The fast check avoids taking the mutex for a known-stale generation, but
    /// the check under the mutex is authoritative: a caller paused after the
    /// first check must not reopen the destination after replacement withdrew
    /// readiness. The returned guard stays alive for the complete read, making
    /// every SQLite handle visible to the publication drain.
    fn shared_reader_at(
        &self,
        at: ReadAt,
    ) -> Option<std::sync::MutexGuard<'_, Option<PhysicalGraphProjectionDatabase>>> {
        if !self.answers(at) {
            return None;
        }
        #[cfg(test)]
        {
            let hook = self
                .shared
                .before_shared_reader_admission
                .lock()
                .unwrap()
                .take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if !self.answers(at) {
            return None;
        }
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        reader.as_ref()?;
        #[cfg(test)]
        {
            let hook = self
                .shared
                .after_shared_reader_admission
                .lock()
                .unwrap()
                .take();
            if let Some(hook) = hook {
                hook();
            }
        }
        Some(reader)
    }

    pub(crate) fn referenced_page_names(&self, at: ReadAt) -> Option<Vec<String>> {
        let reader = self.shared_reader_at(at)?;
        let read = reader.as_ref()?.read();
        let mut names = std::collections::HashMap::<String, String>::new();
        drain_after(
            |after: Option<(String, String)>, batch| {
                read.navigation_reference_names_after(
                    after
                        .as_ref()
                        .map(|(normalized, raw)| (normalized.as_str(), raw.as_str())),
                    batch,
                )
            },
            |row| (row.normalized_name.clone(), row.raw_name.clone()),
            |row| {
                names
                    .entry(crate::refs::page_key(&row.raw_name))
                    .or_insert(row.raw_name);
                Ok(())
            },
            |_, _| None,
        )
        .reported(self)?;
        if !self.answers(at) {
            return None;
        }
        let mut names = names.into_values().collect::<Vec<_>>();
        names.sort_by_key(|name| crate::refs::page_key(name));
        #[cfg(test)]
        self.shared
            .referenced_name_reads
            .fetch_add(1, Ordering::Relaxed);
        Some(names)
    }

    pub(crate) fn page_aliases_with_owners(
        &self,
        at: ReadAt,
    ) -> Option<Vec<(String, String, String)>> {
        let _reader = self.shared_reader_at(at)?;
        let mut aliases = Vec::new();
        let mut snapshot =
            PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, || Ok(()))
                .reported(self)?;
        crate::query::projection_sql::visit(
            &mut snapshot,
            "SELECT alias.raw, owner.raw, p.path \
             FROM reference_alias_declarations d \
             JOIN pages p ON p.page_id = d.source_page_id \
             JOIN names owner ON owner.name_id = p.name_id \
             JOIN names alias ON alias.name_id = d.alias_name_id \
             WHERE d.source_entity_type = 0 AND d.source_entity_id = d.source_page_id \
             ORDER BY d.source_page_id, d.ordinal, alias.raw",
            &[],
            |row| {
                let values = row
                    .iter()
                    .map(|value| match value {
                        PhysicalQueryValue::Text(text) => Ok(text.clone()),
                        _ => Err(tine_storage::sqlite::MaterializationError::InvalidQuery(
                            "page alias ownership row contains non-text data".into(),
                        )),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if values.len() != 3 {
                    return Err(tine_storage::sqlite::MaterializationError::InvalidQuery(
                        "page alias ownership row has the wrong width".into(),
                    ));
                }
                aliases.push((values[0].clone(), values[1].clone(), values[2].clone()));
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .reported(self)?;
        self.answers(at).then_some(aliases)
    }

    pub(crate) fn real_page_names(&self, at: ReadAt) -> Option<crate::query::RealPageNames> {
        let reader = self.shared_reader_at(at)?;
        let read = reader.as_ref()?.read();
        let mut names = crate::query::RealPageNames::new();
        drain_after(
            |after: Option<(String, i64)>, batch| {
                read.navigation_pages_after_with_header_validation(
                    after.as_ref().map(|(path, _)| path.as_str()),
                    after.as_ref().map(|(_, id)| *id),
                    batch,
                    |_, _| Ok(()),
                )
            },
            |row| (row.path.clone(), row.cursor),
            |row| {
                let path = PathBuf::from(&row.path);
                match names.get_mut(&row.name_key) {
                    Some((winner_path, winner_name)) if path < *winner_path => {
                        *winner_path = path;
                        *winner_name = row.name;
                    }
                    Some(_) => {}
                    None => {
                        names.insert(row.name_key, (path, row.name));
                    }
                }
                Ok(())
            },
            |_, _| None,
        )
        .reported(self)?;
        self.answers(at).then_some(names)
    }

    /// The candidate set for one reference target: the pages that may contain a
    /// match and, when the index can name them, the BLOCKS.
    ///
    /// The block set is not an optimization bolted on afterwards — it is what
    /// `page_referrer_candidates_after` already returns. Its rows are
    /// `(source_page_id, source_entity)` and the caller used to drop the
    /// entity, so the read narrowed to 184 pages and then re-parsed all 3,434
    /// of their blocks to find the 412 that referred to the target (measured on
    /// the anonymized graph; see `sql_gates_tests.rs`'s narrowing receipt).
    /// Carrying the entity through spends nothing extra in SQL and removes
    /// roughly nine of every ten per-block parses.
    ///
    /// The block set is a SUPERSET filter and never the answer: the parser
    /// still decides membership, exactly as before. It is `None` whenever the
    /// index cannot name blocks for this target, and then every block of every
    /// candidate page is classified as it was.
    pub(crate) fn reference_candidates(
        &self,
        at: ReadAt,
        names_norm: &[String],
        self_page: &str,
        kind: ReferenceKind,
        mode: crate::query::candidate::CandidateMode,
        config: &crate::config::Config,
    ) -> Option<ReferenceCandidateIndex> {
        if !reference_narrowing_supported(names_norm, kind) {
            return None;
        }
        let reader = self.shared_reader_at(at)?;
        let mut paths = std::collections::BTreeSet::new();
        let mut blocks = std::collections::HashSet::new();
        let mut page_owners = matches!(
            (kind, mode),
            (
                ReferenceKind::Plain,
                crate::query::candidate::CandidateMode::Interactive { .. }
            )
        )
        .then(std::collections::HashSet::new);
        reader.as_ref()?;
        // The block set names blocks as the candidate pages' documents do, by
        // the identity policy captured beside the image (R3,
        // `ResultIdentity::public_id`). The stored id of a page edited in an
        // earlier session is that session's live id, which no document
        // carries after a reopen; compared directly, the filter dropped every
        // such block and Linked References lost them (GH #594).
        // All title/alias needles observe one committed image. Reopening per
        // spelling could otherwise union candidates from opposite sides of an
        // edit even though each individual query was coherent.
        let mut snapshot =
            PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, || Ok(()))
                .reported(self)?;
        let identity = capture_result_identity(&self.shared, &mut snapshot).reported(self)?;
        let mut insert_block = |path: &str,
                                result_id: Option<&PhysicalQueryValue>,
                                order_key: Option<&PhysicalQueryValue>|
         -> Result<(), tine_storage::sqlite::MaterializationError> {
            match (result_id, order_key) {
                (
                    Some(PhysicalQueryValue::Text(result_id)),
                    Some(PhysicalQueryValue::Text(order_key)),
                ) => {
                    blocks.insert(
                        identity
                            .public_id(path, order_key, result_id)
                            .map_err(tine_storage::sqlite::MaterializationError::Corrupt)?,
                    );
                    Ok(())
                }
                // A page-level posting names no block. The page-property
                // pseudo-block it stands for is built from the page preamble
                // and never classified through the block walk, so the block
                // set stays complete for the walk.
                (Some(PhysicalQueryValue::Null), Some(PhysicalQueryValue::Null)) => Ok(()),
                _ => Err(tine_storage::sqlite::MaterializationError::InvalidQuery(
                    "reference candidate has an invalid identity".into(),
                )),
            }
        };
        for name in names_norm {
            match kind {
                ReferenceKind::Explicit => {
                    // Property-key pseudo pages are not backlinks; duplicate
                    // occurrences collapse to one source entity.
                    crate::query::projection_sql::visit(
                        &mut snapshot,
                        "SELECT DISTINCT p.path, b.result_id, b.order_key \
                         FROM reference_postings r \
                         JOIN names n ON n.name_id = r.target_name_id \
                         JOIN pages p ON p.page_id = r.source_page_id \
                         LEFT JOIN blocks b \
                           ON r.source_entity_type = 1 AND b.block_id = r.source_entity_id \
                         WHERE r.target_type = 0 AND r.reference_kind <= 4 AND n.key = ?1",
                        &[PhysicalQueryValue::Text(name.clone())],
                        |row| {
                            let Some(PhysicalQueryValue::Text(path)) = row.first() else {
                                return Err(
                                    tine_storage::sqlite::MaterializationError::InvalidQuery(
                                        "reference candidate has no page path".into(),
                                    ),
                                );
                            };
                            insert_block(path, row.get(1), row.get(2))?;
                            paths.insert(PathBuf::from(path));
                            Ok(std::ops::ControlFlow::Continue(()))
                        },
                    )
                    .reported(self)?;
                }
                ReferenceKind::Plain => {
                    let folded = crate::search_query::canonical_fold(name);
                    let plan = crate::query::candidate::fragment_bound(&folded);
                    let mut params = plan
                        .as_ref()
                        .map(|(_, expression)| vec![PhysicalQueryValue::Text(expression.clone())])
                        .unwrap_or_default();
                    let snapshot = &mut snapshot;
                    let exclusions = crate::refs::ReferenceSourceExclusions::new(
                        self_page,
                        config.favorites_page.as_deref(),
                    );
                    let mut indexed_filters = Vec::new();
                    let mut scan_filters = Vec::new();
                    for key in exclusions.keys() {
                        params.push(PhysicalQueryValue::Text(key.clone()));
                        indexed_filters.push(format!("owner_key <> ?{}", params.len()));
                        scan_filters.push(format!("n.key <> ?{}", params.len()));
                    }
                    let indexed_filter = indexed_filters.join(" AND ");
                    let scan_filter = scan_filters.join(" AND ");
                    let mut limit_sql = String::new();
                    match mode {
                        crate::query::candidate::CandidateMode::Exhaustive => {}
                        crate::query::candidate::CandidateMode::Interactive { window } => {
                            let needle = vec![name.clone()];
                            let config = config.clone();
                            snapshot
                                .set_query_rank_function(move |entity_type, framed| {
                                    #[cfg(test)]
                                    PLAIN_REFERENCE_EXACT_CALLBACKS
                                        .with(|count| count.set(count.get().saturating_add(1)));
                                    let (raw, path) = crate::query::rank::decode_pair(framed)?;
                                    let is_org = Format::from_path(Path::new(path)) == Format::Org;
                                    let matched = if entity_type == 0 {
                                        crate::query::page_preamble_has_reference(
                                            raw,
                                            is_org,
                                            &needle,
                                            ReferenceKind::Plain,
                                            &config,
                                        )
                                    } else {
                                        let block = DocBlock::preamble(raw, is_org);
                                        let projection = block.projection();
                                        crate::reference_evidence::has_occurrence_kind(
                                            raw,
                                            &projection.reference_source,
                                            &needle,
                                            ReferenceKind::Plain,
                                            &config,
                                        )
                                    };
                                    Ok(matched.then(Vec::new))
                                })
                                .reported(self)?;
                            params.push(PhysicalQueryValue::Integer(
                                i64::try_from(window).unwrap_or(i64::MAX),
                            ));
                            limit_sql = format!(" LIMIT ?{}", params.len());
                        }
                    }
                    let sql = if let Some((table, _)) = plan {
                        let candidate_source = format!(
                            "(SELECT c.rowid AS entity_id, \
                                     CASE WHEN ep.page_id IS NOT NULL THEN 0 ELSE 1 END AS entity_type, \
                                     owner.path, b.result_id, b.order_key, \
                                     CASE WHEN ep.page_id IS NOT NULL \
                                          THEN COALESCE(pt.preamble, '') ELSE bt.content END AS raw, \
                                     owner_name.key AS owner_key \
                              FROM (SELECT rowid FROM {table} \
                                    WHERE {table} MATCH ?1 ORDER BY rowid DESC) c \
                              LEFT JOIN pages ep ON ep.page_id = c.rowid \
                              LEFT JOIN blocks b ON b.block_id = c.rowid \
                              JOIN pages owner ON owner.page_id = COALESCE(ep.page_id, b.page_id) \
                              JOIN names owner_name ON owner_name.name_id = owner.name_id \
                              LEFT JOIN page_text pt ON pt.page_id = ep.page_id \
                              LEFT JOIN block_text bt ON bt.block_id = b.block_id \
                              WHERE ep.page_id IS NOT NULL OR b.block_id IS NOT NULL) candidates",
                            table = table.name(),
                        );
                        let mut conditions = indexed_filter;
                        if matches!(
                            mode,
                            crate::query::candidate::CandidateMode::Interactive { .. }
                        ) {
                            let frame = crate::query::text::framed_pair_sql("raw", "path");
                            if !conditions.is_empty() {
                                conditions.push_str(" AND ");
                            }
                            conditions.push_str(&format!(
                                "tine_query_rank(entity_type, {frame}) IS NOT NULL"
                            ));
                        }
                        let where_sql = (!conditions.is_empty())
                            .then(|| format!(" WHERE {conditions}"))
                            .unwrap_or_default();
                        format!(
                            "SELECT path, result_id, entity_id, entity_type, order_key \
                             FROM {candidate_source}{where_sql} \
                             ORDER BY entity_id DESC{limit_sql}"
                        )
                    } else {
                        let page_frame = crate::query::text::framed_pair_sql(
                            "COALESCE(pt.preamble, '')",
                            "p.path",
                        );
                        let block_frame =
                            crate::query::text::framed_pair_sql("bt.content", "p.path");
                        let mut page_conditions = scan_filter.clone();
                        let mut block_conditions = scan_filter;
                        if matches!(
                            mode,
                            crate::query::candidate::CandidateMode::Interactive { .. }
                        ) {
                            if !page_conditions.is_empty() {
                                page_conditions.push_str(" AND ");
                                block_conditions.push_str(" AND ");
                            }
                            page_conditions
                                .push_str(&format!("tine_query_rank(0, {page_frame}) IS NOT NULL"));
                            block_conditions.push_str(&format!(
                                "tine_query_rank(1, {block_frame}) IS NOT NULL"
                            ));
                        }
                        let page_where = (!page_conditions.is_empty())
                            .then(|| format!(" WHERE {page_conditions}"))
                            .unwrap_or_default();
                        let block_where = (!block_conditions.is_empty())
                            .then(|| format!(" WHERE {block_conditions}"))
                            .unwrap_or_default();
                        format!(
                            "SELECT p.path AS path, NULL AS result_id, \
                                    p.page_id AS entity_id, 0 AS entity_type, \
                                    NULL AS order_key \
                             FROM pages p JOIN names n ON n.name_id = p.name_id \
                             LEFT JOIN page_text pt ON pt.page_id = p.page_id{page_where} \
                             UNION ALL \
                             SELECT p.path, b.result_id, b.block_id, 1, b.order_key \
                             FROM blocks b JOIN block_text bt ON bt.block_id = b.block_id \
                             JOIN pages p ON p.page_id = b.page_id \
                             JOIN names n ON n.name_id = p.name_id{block_where} \
                             ORDER BY entity_id DESC{limit_sql}"
                        )
                    };
                    #[cfg(test)]
                    if matches!(
                        mode,
                        crate::query::candidate::CandidateMode::Interactive { .. }
                    ) {
                        capture_plain_reference_query_plan(&self.shared.path, &sql, &params)?;
                    }
                    crate::query::projection_sql::visit(snapshot, &sql, &params, |row| {
                        let Some(PhysicalQueryValue::Text(path)) = row.first() else {
                            return Err(tine_storage::sqlite::MaterializationError::InvalidQuery(
                                "plain-reference candidate has no page path".into(),
                            ));
                        };
                        insert_block(path, row.get(1), row.get(4))?;
                        let path = PathBuf::from(path);
                        paths.insert(path.clone());
                        match row.get(3) {
                            Some(PhysicalQueryValue::Integer(0)) => {
                                if let Some(page_owners) = page_owners.as_mut() {
                                    page_owners.insert(path);
                                }
                            }
                            Some(PhysicalQueryValue::Integer(1)) => {}
                            _ => {
                                return Err(
                                    tine_storage::sqlite::MaterializationError::InvalidQuery(
                                        "plain-reference candidate has an invalid entity kind"
                                            .into(),
                                    ),
                                );
                            }
                        }
                        Ok(std::ops::ControlFlow::Continue(()))
                    })
                    .reported(self)?;
                }
            }
        }
        self.answers(at).then_some(ReferenceCandidateIndex {
            paths,
            blocks: Some(blocks),
            page_owners,
        })
    }

    /// Outer `None` means projection unavailable/stale and requires parser
    /// fallback. Inner `None` is an exact current-generation miss.
    pub(crate) fn block_page_hint(&self, at: ReadAt, uuid: &str) -> Option<Option<String>> {
        let parsed_uuid = Uuid::parse_str(uuid).ok()?.into_bytes();
        let reader = self.shared_reader_at(at)?;
        let read = reader.as_ref()?.read();
        // A live id this session gave a block is stored as the block's
        // structural id (R3). A hint needs only the page, and the page of the
        // row found is checked against the page that recorded the id.
        let recorded = Arc::clone(&self.shared.session_ids.lock().unwrap());
        let live = recorded.iter().find_map(|(path, entries)| {
            entries.iter().find_map(|entry| {
                entry
                    .ids
                    .iter()
                    .find_map(|(stored, live)| (live == uuid).then_some((path, stored)))
            })
        });
        let stored = live.map_or(uuid, |(_, stored)| stored.as_str());
        let block = match read
            .block(stored)
            .reported(self)?
            .filter(|block| live.is_none_or(|(path, _)| block.page_path == *path))
        {
            Some(block) => crate::query::logseq_uuid_owner([block], false),
            None => crate::query::logseq_uuid_owner(
                read.blocks_by_logseq_uuid(parsed_uuid, 2).reported(self)?,
                false,
            ),
        };
        let page = match block {
            Some(block) => read
                .page_with_header_validation(&block.page_path, |_, _| Ok(()))
                .reported(self)?
                .map(|page| page.name),
            None => None,
        };
        self.answers(at).then_some(page)
    }

    pub(crate) fn block_ref_counts(
        &self,
        at: ReadAt,
    ) -> Option<std::collections::HashMap<String, usize>> {
        let reader = self.shared_reader_at(at)?;
        let read = reader.as_ref()?.read();
        let mut counts = std::collections::HashMap::new();
        drain_after(
            |after, batch| read.block_reference_counts_after(after, batch),
            |row| row.raw_uuid_claim,
            |row| {
                let distinct = usize::try_from(row.distinct_source_blocks).map_err(|_| {
                    tine_storage::sqlite::MaterializationError::Corrupt(
                        "block reference count exceeds usize".into(),
                    )
                })?;
                counts.insert(Uuid::from_bytes(row.raw_uuid_claim).to_string(), distinct);
                Ok(())
            },
            |_, _| None,
        )
        .reported(self)?;
        self.answers(at).then_some(counts)
    }

    pub(crate) fn block_referrer_candidate_paths(
        &self,
        at: ReadAt,
        uuid: &str,
    ) -> Option<std::collections::BTreeSet<PathBuf>> {
        let uuid = Uuid::parse_str(uuid).ok()?.into_bytes();
        let reader = self.shared_reader_at(at)?;
        let read = reader.as_ref()?.read();
        let mut paths = std::collections::BTreeSet::new();
        drain_after(
            |after, batch| read.block_referrer_candidates_after(uuid, after, batch),
            |row| (row.page_cursor, row.block_cursor),
            |row| {
                paths.insert(PathBuf::from(row.source_page_path));
                Ok(())
            },
            |_, _| None,
        )
        .reported(self)?;
        self.answers(at).then_some(paths)
    }

    pub(crate) fn ready_at(&self, generation: u64) -> bool {
        self.shared.ready_at(generation)
    }

    /// Whether the index may answer a read at `at`: ready at its generation,
    /// or, for a [`Currency::LaunchStored`] read, serving the stored image
    /// during the launch check ([`owner::serving_stored`]).
    pub(crate) fn answers(&self, at: ReadAt) -> bool {
        self.ready_at(at.generation)
            || (at.currency == Currency::LaunchStored && self.serving_stored())
    }

    /// Whether the index answers `LaunchStored` reads from the image the last
    /// session left, before the launch check has validated it (design D2).
    pub(crate) fn serving_stored(&self) -> bool {
        let pending = self.shared.pending.lock().unwrap();
        owner::serving_stored(&self.shared, &pending)
    }

    /// RET2's readiness lifecycle: why this generation is not ready, and what
    /// the caller may do about it.
    ///
    /// The order of the tests is the order of authority.
    ///
    /// * `worker_available` is stored `false` exactly where the worker thread
    ///   gives up for good — no parent directory, an unopenable database, or
    ///   a `stop` turn. A writer waiting for the lease reads the same way
    ///   while it waits, but takes its queue once it has the lease. Nothing this
    ///   graph enqueues afterwards is ever taken, so retrying is endless by
    ///   construction and the caller owes a bounded error instead.
    /// * A queued turn is progress even when the LAST turn failed:
    ///   `worker_failed` stays set until the next successful turn, and the
    ///   repair that clears it is exactly the `full`/`rebuild` work below.
    /// * A failed worker with an EMPTY queue is the stale-idle case: the turn
    ///   failed on a damaged image, `rebuild` is owed, and only a fresh build
    ///   clears it. That is a repair, not a wait.
    pub(crate) fn progress_at(&self, generation: u64) -> ProjectionProgress {
        // Readiness is published under this lock, by the turn that empties
        // the queue. Read before the lock, "not ready" and "nothing queued"
        // came from either side of that publication, and a projection that
        // had just become ready read as stale (GH #543).
        let pending = self.shared.pending.lock().unwrap();
        if self.ready_at(generation) {
            return ProjectionProgress::Ready;
        }
        // One state function answers this and "is work coming" (GH #594 L2).
        match owner::index_state(&self.shared, &pending) {
            owner::IndexState::Stopped => ProjectionProgress::Stopped,
            owner::IndexState::Failed(class) => ProjectionProgress::Failed(class),
            owner::IndexState::Working(reason) => ProjectionProgress::Working(reason),
            owner::IndexState::Idle => ProjectionProgress::Stale,
        }
    }

    /// R3: refuse new jobs, interrupt the active ones and wait for their slots
    /// before the worker is told to stop, so no snapshot outlives the
    /// projection that admitted it. Idempotent — `stop` and a closed admission
    /// owner are both terminal, so a caller that closes explicitly and then
    /// drops pays a second no-op drain and nothing else.
    fn close(&self) {
        let fence = self.shared.cancel_queued_captures(true);
        self.shared.query_jobs.wait_for_drain(fence);
    }

    /// Retain a resource until the writer has released its connection and lease.
    /// The publication root also has a foreground owner until its graph drops.
    #[cfg(test)]
    pub(crate) fn retain_worker_resource(&self, resource: Arc<dyn Send + Sync>) {
        if let Some(resources) = self.shared.worker_resources.lock().unwrap().as_mut() {
            resources.push(resource);
        }
    }

    /// [`DirectProjection::close`], then wait until the writer worker has
    /// actually RETURNED — up to `timeout`. `true` when it did.
    ///
    /// The only caller that needs this is one that owns the database's
    /// directory and is about to remove it: on Windows an open handle refuses
    /// the delete, and on every platform a worker still finishing its turn can
    /// recreate the file under a directory that was just removed. Ordinary
    /// graph close does NOT wait — an app teardown must not block on SQLite —
    /// which is why the wait is an explicit call and not part of `Drop`.
    ///
    /// A `stop` turn is taken as soon as the worker reaches the top of its
    /// loop, so the bound is one in-flight apply, never a queue.
    /// [`DirectProjection::close`] without waiting: callable from the worker.
    #[cfg(test)]
    pub(crate) fn close_test(&self) {
        self.close();
    }

    pub(crate) fn close_and_wait_for_worker(&self, timeout: std::time::Duration) -> bool {
        self.close();
        let started = std::time::Instant::now();
        while !self.shared.worker_finished.load(Ordering::Acquire) {
            if started.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        true
    }
}

impl Drop for DirectProjection {
    fn drop(&mut self) {
        self.close();
    }
}

/// Publish the worker's exit AFTER every resource it owns has been released.
///
/// Declared as the FIRST local in [`projection_worker`], so it drops LAST —
/// after the writer connection and the exclusive writer lease. `Drop` is the
/// only correct place for it: the worker has five early returns and one
/// steady-state one, and a flag stored at each of them is a flag the next arm
/// forgets.
struct ProjectionWorkerExit(Arc<ProjectionShared>);

impl Drop for ProjectionWorkerExit {
    fn drop(&mut self) {
        self.0.worker_available.store(false, Ordering::Release);
        let captures = std::mem::take(&mut self.0.pending.lock().unwrap().captures);
        reject_query_captures(captures);
        let resources = self.0.worker_resources.lock().unwrap().take();
        drop(resources);
        self.0.worker_finished.store(true, Ordering::Release);
        self.0.changed.notify_all();
    }
}

const PROJECTION_UPDATE_FAILURE: &str = "is stale; indexed reads are unavailable";

/// Report a Direct Files projection write failure. Each read surface owns its
/// readiness/error policy; this writer cannot claim that a query will traverse.
///
/// The always-on line names the failure family in fixed words and carries
/// nothing else. I-5: the detail at both call sites is free-form prose from the
/// projection WRITE path, and that path names the graph — `apply_deltas`
/// formats `entry.rel_path` straight into its error string, and
/// `MaterializationError`'s payloads are free-form `String`s produced while
/// storing parsed page text. I-9: the family still reaches the always-on
/// record, because a user who is not running under `TINE_DEBUG` otherwise sees
/// only an unavailable index. The prose stays on the directed debug channel.
/// Process-relative clock for [`projection_diag`], started at the first line.
static PROJECTION_DIAG_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Lines emitted this process, so the gating is checkable rather than asserted
/// in a comment (I-11); a test cannot read stderr.
#[cfg(test)]
static PROJECTION_DIAG_LINES: AtomicU64 = AtomicU64::new(0);

/// One directed projection-lifecycle line, on the SAME opt-in channel as every
/// other runtime diagnostic (`TINE_DEBUG=1` / `--debug`, I-12).
///
/// GH #543: the Windows verify probe could say only that a 10,000-page cold
/// open took 139 s with the switcher stuck on "Indexing 0 of 10,001"; the
/// app's debug log stopped at "Direct Files publish" and the next 105 s were
/// unobserved, so no run could say which worker turn was running or why. The
/// message is built behind a `FnOnce` so a process without diagnostics pays
/// one relaxed atomic load and formats nothing.
pub(crate) fn projection_diag(message: impl FnOnce() -> String) {
    if !crate::backend_error::runtime_debug_diagnostics_enabled() {
        return;
    }
    #[cfg(test)]
    PROJECTION_DIAG_LINES.fetch_add(1, Ordering::Relaxed);
    let elapsed = PROJECTION_DIAG_EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis();
    eprintln!("[tine] projection +{elapsed}ms {}", message());
}

#[cfg(test)]
pub(crate) fn projection_diag_lines_test() -> u64 {
    PROJECTION_DIAG_LINES.load(Ordering::Relaxed)
}

fn report_projection_failure(family: &str, detail: &dyn std::fmt::Display) {
    #[cfg(test)]
    REPORTED_PROJECTION_FAILURES.fetch_add(1, Ordering::Relaxed);
    eprintln!("[tine] Direct Files SQLite projection {family}");
    if crate::backend_error::runtime_debug_diagnostics_enabled() {
        eprintln!("[tine] Direct Files SQLite projection {family}; directed detail: {detail}");
    }
}

/// How many times the always-on failure family has been printed this process.
///
/// The counter exists because the ONLY difference between a reported failure and
/// a silent handoff is which `eprintln!` runs, and a test cannot read stderr.
/// It is what makes [`ProjectionRefusal`]'s split checkable rather than merely
/// asserted in a comment.
#[cfg(test)]
static REPORTED_PROJECTION_FAILURES: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(crate) fn reported_projection_failures_test() -> u64 {
    REPORTED_PROJECTION_FAILURES.load(Ordering::Relaxed)
}

/// Why one worker turn produced no serving image: a write failed (the user
/// is told, and K1 decides whether a new image is owed), or the projection is
/// closing (nothing is owed and nothing is reported).
enum ProjectionRefusal {
    Failed(String),
    Stopped,
}

impl ProjectionRefusal {
    /// Whether this refusal is a genuine write failure the user must be told
    /// about on the always-on channel.
    fn is_reportable_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

impl std::fmt::Display for ProjectionRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(error) => f.write_str(error),
            Self::Stopped => f.write_str("projection stopped before staged publication"),
        }
    }
}

/// Lives for one repair attempt; see `DirectProjection::begin_repair`.
pub(crate) struct RepairInFlight(Arc<ProjectionShared>);

/// Lives from a generation move until the mover has queued the delta that
/// describes it; see `DirectProjection::delta_coming`.
pub(crate) struct DeltaComing(Arc<ProjectionShared>);

impl Drop for DeltaComing {
    fn drop(&mut self) {
        self.0.deltas_coming.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_all();
    }
}

impl Drop for RepairInFlight {
    fn drop(&mut self) {
        self.0.repairs_in_flight.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_all();
    }
}

/// The worker could not set up its image and exits: the index is `Failed`
/// with the error's class, as after its last attempt, so readers are told and
/// the user can retry, instead of a silent "unavailable" (GH #594 L1).
fn worker_cannot_start(shared: &ProjectionShared, error: &str) {
    owner::note_failed(
        &mut shared.pending.lock().unwrap(),
        crate::query::IndexFailureClass::of_message(error),
    );
    shared.worker_available.store(false, Ordering::Release);
    shared.changed.notify_all();
}

fn projection_worker(shared: Arc<ProjectionShared>, launch_config: Option<Arc<ParseConfig>>) {
    // FIRST local, so it is the LAST thing dropped: the writer connection and
    // the exclusive lease below are both released before the exit is published.
    let _exit = ProjectionWorkerExit(Arc::clone(&shared));
    let Some(parent) = shared.path.parent() else {
        shared.worker_available.store(false, Ordering::Release);
        shared.changed.notify_all();
        return;
    };
    if let Err(error) = std::fs::create_dir_all(parent) {
        eprintln!("[tine] Direct Files SQLite projection disabled: create directory: {error}");
        worker_cannot_start(&shared, &error.to_string());
        return;
    }
    // Waits, retrying, while another writer holds the lease; `None` only
    // when this projection is closed meanwhile.
    let Some(_lease) = lease::take_writer_lease(&shared) else {
        shared.worker_available.store(false, Ordering::Release);
        shared.changed.notify_all();
        return;
    };
    let publication_directory =
        projection_publication_names(&shared.path).and_then(|(parent, _, destination)| {
            let directory = Dir::open_ambient_dir(&parent, ambient_authority())
                .map_err(|error| error.to_string())?;
            cleanup_projection_stages(&directory, &destination)
        });
    if let Err(error) = publication_directory {
        report_projection_failure(PROJECTION_UPDATE_FAILURE, &error);
        worker_cannot_start(&shared, &error);
        return;
    }
    let mut writer_slot = open_existing_projection_database(&shared);
    // Every stored row was written under this facts version and this parse
    // configuration: the image may answer display reads before the launch
    // check validates it. After a configuration or facts change it answers
    // nothing until the fresh build the check then owes (design D2).
    let stored_config = launch_config.filter(|config| {
        writer_slot.is_some()
            && derived_reads::stored_facts_are(
                &shared.path,
                &projection_source_revision("", config.digest()),
            )
    });
    {
        let mut pending = shared.pending.lock().unwrap();
        // No image: only a fresh build can give it one.
        pending.rebuild |= writer_slot.is_none();
        pending.set_up = true;
        pending.stored_servable = stored_config.is_some();
        // Queries capture beside the committed property registry: read it
        // now, so a stored image answers them before the check (design D2).
        if pending.registry_owed.is_none() {
            pending.registry_owed = stored_config;
        }
    }
    shared.changed.notify_all();
    if writer_slot.is_some() {
        integrity::start_if_due(&shared);
    }
    loop {
        let turn = {
            let mut pending = shared.pending.lock().unwrap();
            while !pending.worker_can_take() && pending.captures.is_empty() && !pending.stop {
                // Marks a failed turn returned wait out the backoff; wake at
                // its end rather than on the next unrelated notification.
                let wait = pending
                    .retry_after
                    .filter(|_| !pending.marks.is_empty() || pending.registry_owed.is_some())
                    .map(|at| at.saturating_duration_since(std::time::Instant::now()));
                pending = match wait {
                    Some(wait) => {
                        shared
                            .changed
                            .wait_timeout(pending, wait.max(std::time::Duration::from_millis(1)))
                            .unwrap()
                            .0
                    }
                    None => shared.changed.wait(pending).unwrap(),
                };
            }
            if pending.stop {
                shared.worker_available.store(false, Ordering::Release);
                shared.changed.notify_all();
                return;
            }
            let captures = std::mem::take(&mut pending.captures);
            drop(pending);
            for capture in captures {
                let job = capture_query_job(
                    &shared,
                    capture.requirement,
                    capture.registry_sensitivity,
                    capture.slot,
                );
                let _ = capture.reply.send(job);
            }
            let mut pending = shared.pending.lock().unwrap();
            if pending.stop {
                return;
            }
            if !pending.worker_can_take() {
                continue;
            }
            shared.worker_busy.store(true, Ordering::Release);
            let full = pending.full.take();
            pending.building = full.is_some();
            let marks = std::mem::take(&mut pending.marks);
            pending.in_flight = marks.clone();
            let registry_owed = if full.is_some() {
                pending.registry_owed = None;
                None
            } else {
                pending.registry_owed.take()
            };
            WorkerTurn {
                full,
                marks,
                registry_owed,
                latest_generation: pending.latest_generation,
            }
        };
        let WorkerTurn {
            full,
            marks,
            registry_owed,
            latest_generation,
        } = turn;
        let fresh_build = full.is_some();
        let turn_started = std::time::Instant::now();
        projection_diag(|| {
            format!(
                "turn begin fresh_build={fresh_build} marks={} generation={latest_generation}",
                marks.len(),
            )
        });
        let registry_config = full
            .as_ref()
            .map(|full| Arc::clone(&full.parse_config))
            .or_else(|| registry_owed.clone())
            .or_else(|| {
                marks
                    .values()
                    .filter_map(|(generation, delta)| match delta {
                        PageDelta::Replace { parse_config, .. } => Some((generation, parse_config)),
                        PageDelta::Delete { .. } => None,
                    })
                    .max_by_key(|(generation, _)| *generation)
                    .map(|(_, config)| Arc::clone(config))
            });
        // A changed parse configuration differs on every page: marks lowered
        // under it replace rows the open query jobs read under the old one.
        let config_changed = registry_config.as_ref().is_some_and(|config| {
            shared
                .committed_registry
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|owner| owner.config.digest() != config.digest())
        });
        shared
            .fresh_build_running
            .store(fresh_build, Ordering::Release);
        let registry_reset = fresh_build || registry_owed.is_some();
        let touched_pages = marks
            .values()
            .map(|(_, delta)| delta.entry().rel_path.clone())
            .collect::<std::collections::BTreeSet<_>>();
        // The launch's registry-only turn (design D2) applies nothing: a test
        // holding "the next apply" means the survey's work after it.
        #[cfg(test)]
        if fresh_build || !marks.is_empty() || shared.validated.load(Ordering::Acquire) {
            run_before_apply_deltas_hook(&shared.path);
        }
        let applied: Result<AppliedTurn, ProjectionRefusal> = (|| {
            #[cfg(test)]
            if shared
                .inject_turn_failure
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Err(ProjectionRefusal::Failed(
                    "injected turn failure".to_owned(),
                ));
            }
            if config_changed && !fresh_build {
                let fence = shared.cancel_queued_captures(false);
                shared.query_jobs.wait_for_drain(fence);
            }
            let registry_before = if !registry_reset
                && !touched_pages.is_empty()
                && shared.committed_registry.lock().unwrap().is_some()
            {
                let mut snapshot =
                    PhysicalProjectionQuerySnapshot::open_direct(&shared.path, || Ok(()))
                        .map_err(|error| ProjectionRefusal::Failed(error.to_string()))?;
                registry_sql::read_page_registry_metadata(&mut snapshot, &touched_pages)
                    .map_err(|error| ProjectionRefusal::Failed(error.to_string()))?
            } else {
                PageRegistryMetadata::new()
            };

            let mut applied = if let Some(full) = full {
                // A healthy image lends the build the rows of the pages the
                // snapshot could not read (`carried`).
                let carry = writer_slot
                    .as_ref()
                    .is_some_and(|database| projection_image_is_healthy(&shared, database));
                let (database, applied) = build_and_publish_fresh_projection(
                    &shared,
                    writer_slot.take(),
                    full,
                    marks,
                    carry,
                )
                .map_err(ProjectionRefusal::from)?;
                writer_slot = Some(database);
                applied
            } else if marks.is_empty() {
                // Only the registry was owed.
                AppliedTurn::default()
            } else {
                let database = writer_slot.as_mut().ok_or_else(|| {
                    ProjectionRefusal::Failed("no stored image to apply marks to".to_owned())
                })?;
                apply_deltas(database, &shared, marks).map_err(ProjectionRefusal::from)?
            };

            let (revision, registry_after) = {
                let mut snapshot =
                    PhysicalProjectionQuerySnapshot::open_direct(&shared.path, || Ok(()))
                        .map_err(|error| ProjectionRefusal::Failed(error.to_string()))?;
                let revision = snapshot
                    .query_revision()
                    .map_err(|error| ProjectionRefusal::Failed(error.to_string()))?;
                let registry_after = if !registry_reset
                    && !touched_pages.is_empty()
                    && shared.committed_registry.lock().unwrap().is_some()
                {
                    registry_sql::read_page_registry_metadata(&mut snapshot, &touched_pages)
                        .map_err(|error| ProjectionRefusal::Failed(error.to_string()))?
                } else {
                    PageRegistryMetadata::new()
                };
                (revision, registry_after)
            };
            applied.registry_pages = registry_after;
            #[cfg(test)]
            {
                let hook = shared.after_sql_commit.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook();
                }
            }
            shared.record_deleted_pages(&applied.pages);
            let changes = registry_sql::registry_changes(&registry_before, &applied.registry_pages);
            let mut registry = shared.committed_registry.lock().unwrap();
            let config = registry_config
                .as_ref()
                .cloned()
                .or_else(|| registry.as_ref().map(|owner| Arc::clone(&owner.config)));
            if let Some(config) = config {
                match registry.as_mut() {
                    Some(owner) if !registry_reset && owner.config.digest() == config.digest() => {
                        owner
                            .cache
                            .committed(
                                revision,
                                changes.normalized_keys,
                                changes.declaration_page_names,
                            )
                            .map_err(|error| ProjectionRefusal::Failed(error.to_string()))?;
                    }
                    Some(owner) => {
                        owner.cache.reset(revision, &config);
                        owner.config = config;
                    }
                    None => {
                        *registry = Some(CommittedRegistryOwner {
                            cache: CommittedRegistryCache::new(revision, &config),
                            config,
                        })
                    }
                }
            }
            Ok(applied)
        })();
        let applied = match applied {
            Ok(applied) => applied,
            Err(error) => {
                shared.ready.store(false, Ordering::Release);
                // K1: a failed turn owes a new image only when it was building
                // one (the old image went with it) or the image is damaged.
                // On an intact image the turn committed nothing (it is one
                // transaction), and the marks it returns below re-lower its
                // pages (audit R11-07). Asked before `pending`:
                // the check reads the image.
                let failure = match &error {
                    ProjectionRefusal::Failed(message) => {
                        Some(owner::IndexFailure::of_turn(message))
                    }
                    ProjectionRefusal::Stopped => None,
                };
                let owes_new_image = failure.is_some_and(|failure| {
                    fresh_build || owner::failure_owes_new_image(&shared, failure)
                });
                // A fresh build that violates a constraint (a Tine defect no
                // rebuild fixes) is not relowered on every backoff: like any
                // failure it has `INDEX_ATTEMPTS`, then the index is `Failed`
                // (audit R15-08; GH #594 L1).
                if matches!(error, ProjectionRefusal::Failed(_)) {
                    // Taken before `pending`: never hold both.
                    shared.committed_registry.lock().unwrap().take();
                    #[cfg(test)]
                    shared.last_turn_failed.store(true, Ordering::Release);
                }
                let mut pending = shared.pending.lock().unwrap();
                pending.building = false;
                if pending.full.is_none() {
                    pending.snapshot = None;
                }
                shared.fresh_build_running.store(false, Ordering::Release);
                if matches!(error, ProjectionRefusal::Stopped) {
                    shared.worker_busy.store(false, Ordering::Release);
                    shared.worker_available.store(false, Ordering::Release);
                    drop(pending);
                    shared.changed.notify_all();
                    return;
                }
                // The turn's marks go back, unless a newer one arrived.
                for (_, (generation, delta)) in std::mem::take(&mut pending.in_flight) {
                    pending.record_mark(generation, delta);
                }
                if pending.registry_owed.is_none() && pending.full.is_none() {
                    pending.registry_owed = registry_owed;
                }
                if owes_new_image {
                    shared.worker_failed.store(true, Ordering::Release);
                }
                if owes_new_image {
                    pending.rebuild = true;
                }
                let class = match &error {
                    ProjectionRefusal::Failed(message) => {
                        crate::query::IndexFailureClass::of_message(message)
                    }
                    ProjectionRefusal::Stopped => crate::query::IndexFailureClass::Other,
                };
                note_unsettled(&mut pending, class);
                shared.worker_busy.store(false, Ordering::Release);
                drop(pending);
                shared.changed.notify_all();
                if error.is_reportable_failure() {
                    report_projection_failure(PROJECTION_UPDATE_FAILURE, &error);
                } else {
                    projection_diag(|| {
                        format!(
                            "turn deferred after {}ms: {error}",
                            turn_started.elapsed().as_millis()
                        )
                    });
                }
                continue;
            }
        };
        shared.worker_failed.store(false, Ordering::Release);
        #[cfg(test)]
        shared.last_turn_failed.store(false, Ordering::Release);
        projection_diag(|| {
            format!(
                "turn applied in {}ms lowered={} deleted={} fresh_build={fresh_build}",
                turn_started.elapsed().as_millis(),
                applied.pages.lowered.len(),
                applied.pages.deleted.len(),
            )
        });
        let mut pending = shared.pending.lock().unwrap();
        if fresh_build {
            // A rebuild asked for while this build ran was asked of the image
            // it has just replaced (audit R12-01; IT-10's second build).
            pending.rebuild = false;
            shared.validated.store(true, Ordering::Release);
        }
        shared.fresh_build_running.store(false, Ordering::Release);
        for (path, (generation, _)) in std::mem::take(&mut pending.in_flight) {
            let held = pending.applied.entry(path).or_insert(0);
            *held = (*held).max(generation);
        }
        pending.building = false;
        if pending.full.is_none() {
            pending.snapshot = None;
        }
        shared.worker_busy.store(false, Ordering::Release);
        publish_if_current(&shared, &mut pending);
        drop(pending);
        shared.changed.notify_all();
        checkpoint::start_if_due(&shared);
        // Source-change events may have preceded this commit. Wake the existing
        // application watcher after every serving-image publication, without
        // retaining a query or requiring any edit to be covered by its read.
        if shared.validated.load(Ordering::Acquire) {
            shared.commit_notification.fetch_add(1, Ordering::Release);
            if let Some(wake) = shared.commit_waker.lock().unwrap().as_ref() {
                let _ = wake.send(());
            }
        }
    }
}

/// Publish readiness at the latest generation when the image answers for it
/// ([`image_is_current`]), and withdraw it otherwise. The one place readiness
/// is claimed: at the end of a worker turn, when the survey validates, when a
/// dropped mark leaves nothing to wait for, and on a generation move with no
/// index effect.
fn publish_if_current(shared: &ProjectionShared, pending: &mut PendingProjection) {
    if shared.worker_busy.load(Ordering::Acquire) {
        return;
    }
    if image_is_current(shared, pending) {
        let ready_generation = pending.latest_generation;
        shared
            .ready_generation
            .store(ready_generation, Ordering::Release);
        shared.ready.store(true, Ordering::Release);
        pending.unsettled_passes = 0;
        pending.retry_after = None;
        projection_diag(|| format!("ready at generation={ready_generation}"));
    } else {
        shared.ready.store(false, Ordering::Release);
    }
}

/// One worker turn's queued work.
struct WorkerTurn {
    full: Option<PendingFull>,
    marks: BTreeMap<String, Mark>,
    registry_owed: Option<Arc<ParseConfig>>,
    latest_generation: u64,
}

fn projection_image_is_healthy(
    _shared: &ProjectionShared,
    database: &PhysicalGraphProjectionDatabase,
) -> bool {
    #[cfg(test)]
    _shared
        .projection_health_checks
        .fetch_add(1, Ordering::Relaxed);
    database.validate_schema().is_ok() && database.quick_check().is_ok()
}

/// The stored image, if there is one this build can read. Only its schema is
/// checked here: its integrity is checked in the background, and only when
/// the OS may have gone down since the last check ([`integrity`], D1).
fn open_existing_projection_database(
    shared: &ProjectionShared,
) -> Option<PhysicalGraphProjectionDatabase> {
    if !shared.path.exists() {
        return None;
    }
    let database = PhysicalGraphProjectionDatabase::open_writable(&shared.path).ok()?;
    (database.validate_schema().is_ok() && configure_live_writer(&database).is_ok())
        .then_some(database)
}

/// The worker's writer applies bounded turns: its statement journals fit in
/// memory, and a cascaded multi-page delete then writes no second copy of the
/// pages it touches (16-30 MB per 32-page batch on 10k pages, GH #543).
/// Its commits never checkpoint the WAL either: [`checkpoint`] copies it on a
/// thread of its own, so a turn publishes as soon as it commits.
fn configure_live_writer(database: &PhysicalGraphProjectionDatabase) -> Result<(), String> {
    database
        .keep_temporary_files_in_memory()
        .and_then(|()| database.disable_automatic_checkpoints())
        .map_err(|error| error.to_string())
}

const PROJECTION_STAGE_MARKER: &str = ".tine-projection-build-";

fn projection_publication_names(path: &Path) -> Result<(PathBuf, String, String), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "projection path has no parent directory".to_owned())?
        .to_path_buf();
    let destination = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "projection filename is not UTF-8".to_owned())?
        .to_owned();
    let source = format!(
        ".{destination}{PROJECTION_STAGE_MARKER}{}",
        Uuid::new_v4().simple()
    );
    Ok((parent, source, destination))
}

fn cleanup_projection_stages(directory: &Dir, destination: &str) -> Result<(), String> {
    let prefix = format!(".{destination}{PROJECTION_STAGE_MARKER}");
    for entry in directory.entries().map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let base = name
            .strip_suffix("-wal")
            .or_else(|| name.strip_suffix("-shm"))
            .or_else(|| name.strip_suffix("-journal"))
            .unwrap_or(name);
        let Some(id) = base.strip_prefix(&prefix) else {
            continue;
        };
        if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let metadata = directory
            .symlink_metadata(name)
            .map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "projection staging entry is not a regular file: {name}"
            ));
        }
        directory
            .remove_file(name)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn cleanup_projection_stage_artifacts(directory: &Dir, stage_name: &str) -> Result<(), String> {
    for name in [
        stage_name.to_owned(),
        format!("{stage_name}-wal"),
        format!("{stage_name}-shm"),
        format!("{stage_name}-journal"),
    ] {
        let metadata = match directory.symlink_metadata(&name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "projection staging artifact is not a regular file: {name}"
            ));
        }
        directory
            .remove_file(&name)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn remove_projection_sidecar(directory: &Dir, name: &str) -> Result<(), String> {
    let metadata = match directory.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("projection sidecar is not a regular file: {name}"));
    }
    directory
        .remove_file(name)
        .map_err(|error| error.to_string())
}

use lowering::{delta_inputs, lower_in_batches, LoweringError, LoweringInput};

impl From<LoweringError> for ProjectionRefusal {
    fn from(error: LoweringError) -> Self {
        match error {
            LoweringError::Stopped => ProjectionRefusal::Stopped,
            LoweringError::Failed(error) => ProjectionRefusal::Failed(error),
        }
    }
}

fn fresh_build_stopped(shared: &ProjectionShared) -> bool {
    shared.pending.lock().unwrap().stop
}

/// Build `full` into a staged file and publish it over the image. The pages
/// `full` could not read are lowered from what `old_writer`'s image stored for
/// them when `carry` says that image is healthy (see `carried`), so an
/// incomplete snapshot is built fresh like any other.
fn build_and_publish_fresh_projection(
    shared: &ProjectionShared,
    old_writer: Option<PhysicalGraphProjectionDatabase>,
    full: PendingFull,
    deltas: BTreeMap<String, Mark>,
    carry: bool,
) -> Result<(PhysicalGraphProjectionDatabase, AppliedTurn), LoweringError> {
    #[cfg(test)]
    shared.fresh_builds.fetch_add(1, Ordering::SeqCst);

    let (parent, stage_name, destination_name) =
        projection_publication_names(&shared.path).map_err(LoweringError::Failed)?;
    let directory = Dir::open_ambient_dir(&parent, ambient_authority())
        .map_err(|error| LoweringError::Failed(error.to_string()))?;
    cleanup_projection_stages(&directory, &destination_name).map_err(LoweringError::Failed)?;
    let stage_path = parent.join(&stage_name);

    let build = (|| {
        if fresh_build_stopped(shared) {
            return Err(LoweringError::Stopped);
        }
        let PendingFull {
            pages,
            revisions,
            parse_config,
            retained,
        } = full;
        let config_digest = parse_config.digest();
        let mut inputs = pages
            .iter()
            .map(|(entry, document)| {
                let revision = revisions.get(&entry.path).ok_or_else(|| {
                    LoweringError::Failed(format!(
                        "parsed page has no exact source revision: {}",
                        entry.rel_path
                    ))
                })?;
                Ok(LoweringInput {
                    entry: entry.clone(),
                    document: Arc::clone(document),
                    revision: projection_source_revision(revision, config_digest),
                    parse_config: Arc::clone(&parse_config),
                })
            })
            .collect::<Result<Vec<_>, LoweringError>>()?;
        // Storage takes the complete page order of the image it finishes. It
        // is path order: a function of the page, never of history (design
        // §6), so nothing here tracks positions across turns.
        let mut inventory = pages
            .iter()
            .map(|(entry, _)| entry.rel_path.clone())
            .collect::<BTreeSet<_>>();
        projection_diag(|| {
            format!(
                "fresh build: {} page(s), {} unread source(s), carry={carry}",
                pages.len(),
                retained.len()
            )
        });
        if carry && !retained.is_empty() {
            let superseded = pages
                .iter()
                .map(|(entry, _)| entry.rel_path.as_str())
                .chain(deltas.keys().map(String::as_str))
                .collect::<HashSet<_>>();
            // A read that fails keeps the build going without them rather
            // than refusing to rebuild: the pages come back when their files
            // can be read again, and a build that cannot run keeps nothing.
            match carried::stored_unread_pages(&shared.path, &retained, &superseded, &parse_config)
            {
                Ok(carried) => {
                    projection_diag(|| {
                        format!("fresh build: carrying {} unread page(s)", carried.len())
                    });
                    inventory.extend(carried.iter().map(|page| page.entry.rel_path.clone()));
                    inputs.extend(carried);
                }
                Err(error) => report_projection_failure("could not carry unread pages", &error),
            }
        }
        drop(pages);
        let stage_publication = tine_storage::DurableDirectoryPublication::open(&directory)
            .map_err(|error| LoweringError::Failed(error.to_string()))?;
        let mut database =
            PhysicalGraphProjectionDatabase::create_fresh_build(&stage_path, stage_publication)
                .map_err(|error| LoweringError::Failed(error.to_string()))?;
        let mut applied = AppliedTurn::default();
        let mut text_bytes = 0u64;
        applied.pages.lowered =
            lower_in_batches(shared, inputs, Vec::new(), |change, revisions, aliases| {
                text_bytes = text_bytes.saturating_add(projected_text_bytes(&change.replacements));
                database
                    .set_page_cache_budget(crate::projection_budget::build_page_cache_budget(
                        text_bytes,
                        crate::projection_budget::physical_memory_bytes(),
                    ))
                    .map_err(|error| error.to_string())?;
                database
                    .append_with_source_revisions_and_aliases(&change, &revisions, &aliases)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })?;

        // The updates taken with the snapshot land in `finish`, as one tail.
        let (tail, tail_deletions) = delta_inputs(deltas);
        inventory.extend(tail.iter().map(|page| page.entry.rel_path.clone()));
        for deleted in &tail_deletions {
            inventory.remove(deleted);
        }
        let inventory = inventory.into_iter().collect::<Vec<_>>();
        let mut tail_change = PhysicalGraphProjectionChange {
            replacements: Vec::new(),
            deletions: Vec::new(),
            reference_postings: Vec::new(),
        };
        let (mut tail_revisions, mut tail_aliases) = (Vec::new(), Vec::new());
        applied.pages.deleted.extend(tail_deletions.iter().cloned());
        let tail_lowered = lower_in_batches(
            shared,
            tail,
            tail_deletions,
            |change, revisions, aliases| {
                tail_change.replacements.extend(change.replacements);
                tail_change.deletions.extend(change.deletions);
                tail_change
                    .reference_postings
                    .extend(change.reference_postings);
                tail_revisions.extend(revisions);
                tail_aliases.extend(aliases);
                Ok(())
            },
        )?;
        applied.pages.lowered.extend(tail_lowered);
        if fresh_build_stopped(shared) {
            return Err(LoweringError::Stopped);
        }
        let finalized = database
            .finish(&tail_change, &tail_revisions, &tail_aliases, &inventory)
            .map_err(|error| LoweringError::Failed(error.to_string()))?;
        Ok((finalized, applied, text_bytes))
    })();

    let (finalized, applied, text_bytes) = match build {
        Ok(built) => built,
        Err(error) => {
            let _ = cleanup_projection_stage_artifacts(&directory, &stage_name);
            return Err(error);
        }
    };

    let publication = (|| {
        if fresh_build_stopped(shared) {
            return Err(LoweringError::Stopped);
        }
        let fence = shared.cancel_queued_captures(false);
        shared.query_jobs.wait_for_drain(fence);
        #[cfg(test)]
        {
            let hook = shared
                .before_shared_reader_drain_lock
                .lock()
                .unwrap()
                .take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let mut reader = shared.reader.lock().unwrap();
        #[cfg(test)]
        {
            let hook = shared.after_shared_reader_drain_lock.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        reader.take();
        drop(reader);
        if let Some(database) = old_writer {
            database
                .checkpoint_truncate()
                .map_err(|error| LoweringError::Failed(error.to_string()))?;
            drop(database);
        }
        remove_projection_sidecar(&directory, &format!("{destination_name}-wal"))
            .map_err(LoweringError::Failed)?;
        remove_projection_sidecar(&directory, &format!("{destination_name}-shm"))
            .map_err(LoweringError::Failed)?;

        #[cfg(test)]
        {
            let hook = shared.before_fresh_publication.lock().unwrap().take();
            if let Some(hook) = hook {
                hook().map_err(LoweringError::Failed)?;
            }
            let failure = shared.fresh_publication_failure.lock().unwrap().clone();
            if let Some(message) = failure {
                return Err(LoweringError::Failed(message));
            }
        }
        // This is the cancellation boundary. Once the storage primitive below
        // starts, its atomic name operation owns the outcome; a stop arriving
        // after this check may leave the complete new image installed.
        if fresh_build_stopped(shared) {
            return Err(LoweringError::Stopped);
        }

        finalized
            .publish_replace_single_writer(&destination_name)
            .map_err(|error| LoweringError::Failed(error.to_string()))?;

        #[cfg(test)]
        {
            let hook = shared.after_fresh_publication.lock().unwrap().take();
            if let Some(hook) = hook {
                hook().map_err(LoweringError::Failed)?;
            }
        }
        if fresh_build_stopped(shared) {
            return Err(LoweringError::Stopped);
        }

        let database = PhysicalGraphProjectionDatabase::open_writable(&shared.path)
            .map_err(|error| LoweringError::Failed(error.to_string()))?;
        database
            .validate_schema()
            .map_err(|error| LoweringError::Failed(error.to_string()))?;
        database
            .quick_check()
            .map_err(|error| LoweringError::Failed(error.to_string()))?;
        integrity::record_pass_now(&shared.path);
        database
            .checkpoint_truncate()
            .map_err(|error| LoweringError::Failed(error.to_string()))?;
        configure_live_writer(&database).map_err(LoweringError::Failed)?;
        database
            .shrink_page_cache_budget(crate::projection_budget::resting_page_cache_budget(
                text_bytes,
            ))
            .map_err(|error| LoweringError::Failed(error.to_string()))?;
        #[cfg(test)]
        shared.serving_writer_cache_budget.store(
            database
                .page_cache_budget()
                .map_err(|error| LoweringError::Failed(error.to_string()))?,
            Ordering::Release,
        );
        Ok(database)
    })();
    match publication {
        Ok(database) => {
            cleanup_projection_stage_artifacts(&directory, &stage_name)
                .map_err(LoweringError::Failed)?;
            Ok((database, applied))
        }
        Err(error) => {
            let _ = cleanup_projection_stage_artifacts(&directory, &stage_name);
            Err(error)
        }
    }
}

/// Which pages one worker turn actually WROTE (R3 identity policy): the pages
/// whose rows now carry this process's live runtime ids, and the pages whose
/// rows are gone.
#[derive(Default)]
struct AppliedPages {
    lowered: Vec<String>,
    deleted: Vec<String>,
}

#[derive(Default)]
struct AppliedTurn {
    pages: AppliedPages,
    registry_pages: PageRegistryMetadata,
}

fn apply_deltas(
    database: &mut PhysicalGraphProjectionDatabase,
    shared: &ProjectionShared,
    deltas: BTreeMap<String, Mark>,
) -> Result<AppliedTurn, LoweringError> {
    let (pages, deletions) = delta_inputs(deltas);
    let mut applied = AppliedTurn::default();
    applied.pages.deleted = deletions.clone();
    // One transaction for the whole turn: an index page every batch touches
    // is then written once, not once per batch (GH #543: a 261-page rename
    // wrote 620 MB as nine transactions). A stopped or failed turn commits
    // nothing, and its marks re-lower every page.
    let mut turn = database
        .begin_turn()
        .map_err(|error| LoweringError::Failed(error.to_string()))?;
    applied.pages.lowered =
        lower_in_batches(shared, pages, deletions, |change, revisions, aliases| {
            turn.apply_with_source_revisions_and_aliases(&change, &revisions, &aliases)
                .map(|_| ())
                .map_err(|error| error.to_string())
        })?;
    turn.commit()
        .map_err(|error| LoweringError::Failed(error.to_string()))?;
    Ok(applied)
}

/// Whether this graph's worker has found the writer lease held.
#[cfg(test)]
pub(crate) fn lease_wait_started_test<G: crate::query::graph::QueryGraph>(graph: &G) -> bool {
    graph
        .direct_projection_test()
        .is_some_and(|projection| projection.shared.lease_contended.load(Ordering::Acquire))
}

#[cfg(test)]
/// Release a projection so the SAME database can be reattached.
///
/// `Drop` deliberately does NOT wait for the worker (see
/// [`DirectProjection::close_and_wait_for_worker`]: an app teardown must not
/// block on SQLite), and the worker releases its exclusive writer lease only
/// just before it publishes its exit. So a fixture that drops one graph and
/// immediately reattaches the same path can find the lease still held. The
/// second instance then never becomes ready -- by design, proven by
/// `concurrent_graph_instance_cannot_replace_ready_projection_facts` -- and
/// `wait_ready` spins its whole 15s before panicking "did not converge" with
/// `cache_generation=0`, naming the reopen rather than the handoff.
///
/// That is not hypothetical: it is what took down the Linux release
/// selection on 2026-09-09, on a loaded hosted runner, in two tests that
/// pass locally in 0.05s. Every fixture that reopens a projection database
/// calls this first.
pub(crate) fn release_projection<G: crate::query::graph::QueryGraph>(graph: &G) {
    let Some(projection) = graph.direct_projection_test() else {
        return;
    };
    assert!(
        projection.close_and_wait_for_worker(std::time::Duration::from_secs(15)),
        "the projection worker did not release its writer lease, so reattaching \
         the same database would race it"
    );
}

#[cfg(test)]
/// Retry projection recovery until it converges.
///
/// `direct_projection_recover_after_failed_read` is ONE attempt and is allowed
/// to accomplish nothing: a read whose repair did not take answers
/// `Unavailable` and the next read asks again. If the turn carrying a
/// rebuild's payload fails, the payload is gone and the attempt achieved
/// nothing, so the projection stays failed until something enqueues work again.
///
/// This helper calls the recovery entry point DIRECTLY, which the running app
/// does not do: the app issues ordinary queries and the dispatcher decides. So
/// this cannot be the proof that public recovery converges, and it once hid the
/// fact that it did not — a latched `rebuild` made every query retryable rather
/// than repairing, and nothing called recovery again (GH #543). That property
/// is proved separately through the public query route by
/// `a_failed_statement_read_repairs_and_retries_the_same_statement`. Keep it
/// that way: do not "fix" a convergence failure by reaching for this helper.
///
/// A fixture that calls recovery once and then waits has assumed a convergence
/// guarantee the contract does not make. It fails about one run in twenty on a
/// loaded machine and passes every time on an idle one, which is how
/// `current_snapshot_write_failure_recovers_from_authoritative_source` took
/// down the Linux release selection on 2026-09-10 after passing all night.
pub(crate) fn recover_until_ready<G: crate::query::graph::QueryGraph>(graph: &G) {
    let started = std::time::Instant::now();
    let budget = std::time::Duration::from_secs(30);
    loop {
        graph.direct_projection_recover_after_failed_read();
        let attempt = std::time::Instant::now();
        while !graph.direct_projection_ready_test() {
            if attempt.elapsed() >= std::time::Duration::from_millis(500)
                || started.elapsed() >= budget
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if graph.direct_projection_ready_test() {
            return;
        }
        assert!(
            started.elapsed() < budget,
            "Direct Files projection did not converge across repeated recovery attempts: \
             cache_generation={} {}",
            graph.cache_generation(),
            graph
                .direct_projection_test()
                .map(|projection| projection.debug_state_test())
                .unwrap_or_else(|| "no projection".to_owned())
        );
    }
}

mod carried;
mod checkpoint;
pub(crate) mod derived_reads;
mod integrity;
#[cfg(test)]
pub(crate) use integrity::CHECK_INTERVAL as INDEX_INTEGRITY_CHECK_INTERVAL;
mod lowering;
#[cfg(test)]
mod test_hooks;
pub(crate) use lowering::*;
#[cfg(test)]
#[path = "direct_projection_tests.rs"]
mod tests;

mod observers;
