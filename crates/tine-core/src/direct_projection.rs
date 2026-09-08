use crate::config::ParseConfig;
use crate::doc::{property_key_norm, DocBlock, Document};
use crate::model::{Format, PageEntry, PageKind, ReferenceKind};
use crate::oplog::query_cursor::drain_after;
use crate::query::PropertyFacetAccumulator;
use crate::query_jobs::{Admission, QueryJobOwner, DEFAULT_QUERY_JOB_CAPACITY};
use fs2::FileExt as _;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use tine_storage::sqlite::{
    PhysicalAliasDeclaration, PhysicalBlock, PhysicalEntityId, PhysicalGraphProjectionChange,
    PhysicalGraphProjectionDatabase, PhysicalGraphProjectionSourceRevision, PhysicalPage,
    PhysicalProjectionQueryReader, PhysicalProjectionQuerySnapshot, PhysicalProperty,
    PhysicalQueryValue, PhysicalReferencePosting, PhysicalReferenceTarget, PhysicalTask,
};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

type PageSnapshot = Arc<Vec<(PageEntry, Arc<Document>)>>;
type PageRevisions = Arc<HashMap<PathBuf, String>>;

// This is the parser-fact extractor identity, not an on-disk schema version.
// Bump it whenever unchanged source bytes must be lowered into new/different
// physical facts. The source-revision delta then rebuilds each page once even
// when tine-storage's disposable SQLite schema itself remains compatible.
const DIRECT_PROJECTION_FACTS_VERSION: u32 = 2;
const REFERENCE_DELTA_WAIT: std::time::Duration = std::time::Duration::from_millis(250);
/// R6: how many streamed warm deltas may wait in the queue before the warm
/// thread parses the next batch. It bounds what a cold or changed open retains
/// beyond the worker's current turn to one batch of documents, instead of the
/// whole parsed graph the full snapshot used to pin (plan §2D).
pub(crate) const WARM_STREAM_HIGH_WATER: usize = 64;

#[cfg(test)]
// Test receipts count only their own graph, including its worker threads.
static PHYSICAL_PAGE_LOWERINGS: Mutex<(Option<PathBuf>, u64)> = Mutex::new((None, 0));
/// R6 test receipt: the most deltas a warm stream ever left queued, so a test
/// can prove the stream never retained more than `WARM_STREAM_HIGH_WATER`.
#[cfg(test)]
static MAX_PENDING_DELTAS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
static BEFORE_APPLY_PENDING: Mutex<Option<Box<dyn FnOnce() + Send>>> = Mutex::new(None);

#[cfg(test)]
fn run_before_apply_pending_hook() {
    if let Some(hook) = BEFORE_APPLY_PENDING.lock().unwrap().take() {
        hook();
    }
}

/// The registry's snapshot-scoped page identity on the Direct Files projection
/// side. The Managed side uses `page:<uuid>` and the cold walk the page's
/// relative path; all three are opaque to `build_registry`, which only ever
/// looks a row's page up in the map that came with it.
fn direct_registry_page_key(page_id: [u8; 16]) -> String {
    format!("page:{}", hex16(page_id))
}

#[cfg(test)]
thread_local! {
    static REGISTRY_READ_ATTEMPTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn take_registry_read_attempts() -> u64 {
    REGISTRY_READ_ATTEMPTS.with(|count| count.replace(0))
}

fn hex16(id: [u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in id {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// One queued page change. **The graph config travels INSIDE the work item**
/// (§5.8 M21, F11): every arm that lowers a page carries the exact
/// [`ParseConfig`] it must be lowered under, so the worker cannot reach a state
/// where queued work exists and the config that describes it does not.
///
/// The config used to sit beside the queue, and the worker read it as
/// `parse_config.clone().unwrap_or_else(|| Arc::new(ParseConfig::default()))`.
/// That fallback was unreachable — the stop check runs first and every enqueue
/// path set the config in the same critical section that inserted the work —
/// but if it had ever fired it would have lowered queued pages under the
/// DEFAULT config and stamped the result as current: silently wrong rows,
/// which is exactly what the stamp exists to prevent, reached from inside. A
/// `debug_assert` would have hidden the release-mode behaviour behind a passing
/// debug run, so the absence is removed by SHAPE — it can no longer be spelled.
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
        /// `None` while a warm stream is open (R6): the rows carry no order
        /// position until the stream's closing order turn reconciles the
        /// whole `query_page_order` table, because a position written mid-stream
        /// could collide with a retained page's previous-session position.
        query_page_order: Option<u64>,
        identity: DeltaIdentity,
    },
    Delete {
        entry: PageEntry,
    },
}

/// R6 session identity rule (WARM-IDENTITY-ORDER-CONTRACT.md item 3): where a
/// replacement's runtime ids came from decides whether the page joins or
/// leaves `ProjectionShared::session_pages`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeltaIdentity {
    /// The document is this process's live one (a save, or a parsed-cache
    /// snapshot that may carry preserved ids): the stored `result_id`s ARE the
    /// public ids, so the page is added.
    Live,
    /// A fresh parse (the warm stream): the stored ids are structural, equal to
    /// what `doc_runtime_id_for_order` derives, so the page is removed — an
    /// external incompatible revision invalidates any live mapping it had.
    Structural,
}

impl PageDelta {
    fn entry(&self) -> &PageEntry {
        match self {
            PageDelta::Replace { entry, .. } | PageDelta::Delete { entry } => entry,
        }
    }
}

/// A queued whole-graph snapshot and the config it must be lowered under. The
/// config is stamped into every page's `projection_source_revision`, so a
/// config edit re-lowers every page on the next snapshot instead of leaving
/// rows that answer a question the config no longer asks (J7, D-1: rebuild,
/// never migrate).
struct PendingFull {
    pages: PageSnapshot,
    revisions: PageRevisions,
    parse_config: Arc<ParseConfig>,
}

/// R6 warm validation: the walk inventory with each page's exact content
/// revision, and nothing parsed. The worker compares it with
/// `direct_source_revisions`; an unchanged graph publishes readiness from this
/// alone, a changed one names the pages the warm thread must parse.
struct PendingWarm {
    generation: u64,
    sources: Vec<(PageEntry, String)>,
    parse_config: Arc<ParseConfig>,
}

/// What the worker's warm-validation turn decided (R6), read by the warm
/// thread through `wait_warm_outcome`.
#[derive(Clone, Debug)]
pub(crate) enum WarmOutcome {
    /// Every walk page's rows are current: readiness publishes without a parse.
    Clean,
    /// These pages' rows are missing or stale; the warm thread streams them.
    Replacements(Vec<PageEntry>),
    /// A full parsed snapshot arrived first and owns readiness.
    Superseded,
    /// The validation turn failed; the parser fallback owns readiness.
    Failed,
}

/// One page of the R6 warm stream, as the warm thread hands it over.
pub(crate) enum WarmStreamItem {
    Replace {
        entry: PageEntry,
        document: Arc<Document>,
        revision: String,
        identity: DeltaIdentity,
    },
    Delete {
        entry: PageEntry,
    },
}

#[derive(Default)]
struct PendingProjection {
    full: Option<PendingFull>,
    rebuild: bool,
    deltas: BTreeMap<String, (u64, PageDelta)>,
    latest_generation: u64,
    stop: bool,
    page_order: BTreeMap<String, u64>,
    next_page_order: u64,
    /// R6 warm validation queued for the worker.
    warm: Option<PendingWarm>,
    /// R6: the worker's verdict on the last warm validation.
    warm_outcome: Option<WarmOutcome>,
    /// R6: a warm stream is open at this generation. Readiness never publishes
    /// while it is `Some`, and deltas recorded meanwhile carry no order
    /// position (see `PageDelta::Replace::query_page_order`).
    warm_stream: Option<u64>,
    /// R6: the stream's closing turn — reconcile `query_page_order` over the
    /// queue's own inventory and then publish readiness.
    order: Option<u64>,
    /// R6: a full snapshot was queued after the warm; the stream must stop
    /// enqueueing (its deltas would drop the snapshot's order rows).
    warm_superseded: bool,
    /// R6: an abandoned stream left stale rows behind; only a full snapshot may
    /// publish readiness again (the worker turns this into
    /// `requires_full_rebuild`).
    needs_full: bool,
}

impl PendingProjection {
    fn record_delta(&mut self, generation: u64, mut delta: PageDelta) {
        let key = delta.entry().rel_path.clone();
        match &mut delta {
            PageDelta::Replace {
                query_page_order, ..
            } => {
                let position = if let Some(position) = self.page_order.get(&key) {
                    *position
                } else {
                    let position = self.next_page_order;
                    self.next_page_order += 1;
                    self.page_order.insert(key.clone(), position);
                    position
                };
                *query_page_order = self.warm_stream.is_none().then_some(position);
            }
            PageDelta::Delete { .. } => {
                self.page_order.remove(&key);
            }
        }
        self.deltas.insert(key, (generation, delta));
        self.latest_generation = self.latest_generation.max(generation);
    }

    /// Seed the queue's page order from a complete inventory (a full snapshot
    /// or a warm walk), replacing whatever a cache-less session appended.
    fn seed_page_order<'a>(&mut self, inventory: impl ExactSizeIterator<Item = &'a str>) {
        let mut inventory = inventory.collect::<Vec<_>>();
        if self.rebuild {
            // Repair preserves the session's retained/append order. Stable
            // sorting leaves newly discovered paths in their inventory order,
            // after existing pages. Re-number both owners together below.
            inventory.sort_by_key(|path| self.page_order.get(*path).copied().unwrap_or(u64::MAX));
        }
        self.next_page_order = inventory.len() as u64;
        self.page_order = inventory
            .into_iter()
            .enumerate()
            .map(|(position, rel_path)| (rel_path.to_owned(), position as u64))
            .collect();
    }

    /// The queue's own page inventory in position order: the R6 order turn's
    /// authority. After a warm seed the map tracks every applied replacement
    /// and deletion, so it names exactly the pages the projection holds.
    fn ordered_inventory(&self) -> Vec<[u8; 16]> {
        let mut ordered = self
            .page_order
            .iter()
            .map(|(rel_path, position)| (*position, page_id(rel_path)))
            .collect::<Vec<_>>();
        ordered.sort_unstable_by_key(|(position, _)| *position);
        ordered.into_iter().map(|(_, id)| id).collect()
    }

    fn has_work(&self) -> bool {
        self.full.is_some()
            || !self.deltas.is_empty()
            || self.warm.is_some()
            || self.order.is_some()
            || self.needs_full
    }
}

struct ProjectionShared {
    path: PathBuf,
    pending: Mutex<PendingProjection>,
    changed: Condvar,
    ready: AtomicBool,
    ready_generation: AtomicU64,
    reader: Mutex<Option<PhysicalGraphProjectionDatabase>>,
    /// The D-15 statement seam, opened lazily beside the typed reader above.
    ///
    /// Named `statement_seam` rather than `…_reader` on purpose: the field above
    /// holds the WRITE-CAPABLE `PhysicalGraphProjectionDatabase` under the name
    /// `reader`, and the tine-storage boundary census attributes a method call
    /// to the receiver NAME by substring. A `query_reader` here would file every
    /// read-only statement under the writable handle in that inventory, which is
    /// exactly the distinction D-15 rests on.
    ///
    /// It is a SECOND read-only connection because that is what the seam is: a
    /// separate, read-only handle over the disposable projection, with no way to
    /// reach a writable one (D-15's enforcement is the handle, not a validator).
    /// It answers §5.9's dispatched query and nothing else.
    statement_seam: Mutex<Option<PhysicalProjectionQueryReader>>,
    /// R3: the ONE admission/cancellation owner for database-owned query jobs
    /// (plan §2B). Capacity is taken before a snapshot is opened; the worker
    /// drains every job before it replaces or resets the file, and `Drop`
    /// drains before the worker is stopped.
    query_jobs: QueryJobOwner,
    /// R3 identity policy (WARM-IDENTITY-ORDER-CONTRACT.md §"Chosen strategy"
    /// 2–3): the pages whose rows THIS process lowered. Their stored
    /// `query_block_results.result_id` is the live runtime id the parsed
    /// document carried when the row was written. Every other page's rows
    /// survived from an earlier session, and a fresh parse of an unchanged
    /// page assigns STRUCTURAL runtime ids, so their public id is derived from
    /// `(path, order_key)` through `model::doc_runtime_id_for_order` instead.
    /// Copy-on-write: the worker swaps a new `Arc` after each successful
    /// apply, and a job clones the `Arc` at snapshot acquisition — never a
    /// live lookup during output.
    session_pages: Mutex<Arc<HashSet<[u8; 16]>>>,
    /// The generation at which §5.10's FTS-building signal was last observed
    /// READY. Readiness is monotonic within one projection file — the index
    /// owner finishes the build and never un-finishes it, and a rebuild
    /// publishes a new generation — so a `true` may be remembered and a `false`
    /// never is. That keeps the signal one probe per generation instead of one
    /// per query (I-15) without ever stranding a query on a stale `false`.
    fts_ready_at: AtomicU64,
    fts_ever_ready: AtomicBool,
    worker_available: AtomicBool,
    worker_failed: AtomicBool,
    worker_busy: AtomicBool,
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
    /// the projection at least once (a full snapshot, or a warm validation's
    /// `Clean` or closing order turn). Until then a live delta keeps the file
    /// converging but must not publish readiness: rows of pages this session
    /// has never compared to disk could be stale from an earlier session.
    /// In-scope scenario: an external edit between two sessions, followed by
    /// a save of some other page before the warm runs.
    validated: AtomicBool,
    #[cfg(test)]
    indexed_reads: AtomicU64,
    /// §5.9's dispatched statements: how many times the lowering ANSWERED a
    /// user query through the seam. Separate from `indexed_reads`, which counts
    /// every seam read including the FTS-readiness probe, so a route guard can
    /// say "exactly one statement per query" and mean it.
    #[cfg(test)]
    statement_reads: AtomicU64,
    /// §5.9's failed-read injection: one read through the seam fails, exactly as
    /// a torn or truncated projection file, a disk error or a resource limit
    /// makes it fail. It exists because the obligation a failed read carries —
    /// note the fallback AND schedule the full-snapshot recovery — is invisible
    /// on a healthy projection, and an obligation nothing can observe is one a
    /// future arm silently drops (M9).
    #[cfg(test)]
    inject_read_failure: AtomicBool,
    #[cfg(test)]
    fallback_reads: AtomicU64,
    #[cfg(test)]
    referenced_name_reads: AtomicU64,
    #[cfg(test)]
    fuzzy_candidate_reads: AtomicU64,
}

impl ProjectionShared {
    /// R3 identity policy bookkeeping, run by the worker after every
    /// successful apply: the pages just lowered carry this process's live ids;
    /// the pages just deleted carry nothing.
    fn record_session_pages(&self, applied: &AppliedPages) {
        if applied.lowered.is_empty()
            && applied.deleted.is_empty()
            && applied.relowered_structurally.is_empty()
        {
            return;
        }
        let mut current = self.session_pages.lock().unwrap();
        let mut next: HashSet<[u8; 16]> = (**current).clone();
        next.extend(applied.lowered.iter().copied());
        for page in applied
            .deleted
            .iter()
            .chain(applied.relowered_structurally.iter())
        {
            next.remove(page);
        }
        *current = Arc::new(next);
    }
}

/// One admitted, snapshot-owning Direct query job (R3). Everything the result
/// read needs is captured here, at acquisition, under the ready-generation
/// validation: the pinned read transaction, the compiled-regex program already
/// installed on its connection, and the identity policy input. Dropping the
/// job releases the transaction and the capacity slot.
pub(crate) struct DirectQueryJob<'a> {
    // Field drop order is a lifecycle boundary: release the SQLite transaction
    // before the admission slot can wake a projection replacement drain.
    pub(crate) snapshot: PhysicalProjectionQuerySnapshot,
    /// Held for its `Drop`: releasing the slot is the job's only exit.
    _slot: crate::query_jobs::JobSlot<'a>,
    /// The pages whose rows this process lowered (see
    /// `ProjectionShared::session_pages`), as of the snapshot.
    pub(crate) session_pages: Arc<HashSet<[u8; 16]>>,
}

impl DirectQueryJob<'_> {
    /// Registry input and selection share this owned transaction. This scans
    /// metadata, not result payload; inference remains build_registry's job.
    pub(crate) fn read_registry(
        &mut self,
        config: &ParseConfig,
    ) -> Result<crate::query::registry::Registry, crate::query::QueryExecutionError> {
        #[cfg(test)]
        REGISTRY_READ_ATTEMPTS.with(|count| count.set(count.get() + 1));
        use crate::query::registry::{OwnerRow, OwnerType, PageMeta};
        use crate::query::results::{blob16, integer, text};
        use crate::query::{QueryExecutionError as Error, QueryUnavailableReason as Reason};
        use std::ops::ControlFlow;
        use tine_storage::sqlite::MaterializationError;

        let mut pages = HashMap::new();
        let mut rows = Vec::new();
        let read = (|| -> Result<(), MaterializationError> {
            self.snapshot.visit_projection_query(
                "SELECT page_id, path, name, text_kind FROM pages",
                &[],
                |row| {
                    let decode = || -> Result<_, String> {
                        let id = blob16(row, 0, "pages.page_id")?;
                        let path = text(row, 1, "pages.path")?;
                        let name = text(row, 2, "pages.name")?;
                        if !matches!(integer(row, 3, "pages.text_kind")?, 0 | 1) {
                            return Err("invalid registry page kind".into());
                        }
                        Ok((
                            direct_registry_page_key(id),
                            PageMeta {
                                format: Format::from_path(Path::new(&path)).into(),
                                name,
                            },
                        ))
                    };
                    let (id, meta) = decode().map_err(MaterializationError::Corrupt)?;
                    if pages.insert(id, meta).is_some() {
                        return Err(MaterializationError::Corrupt(
                            "duplicate registry page".into(),
                        ));
                    }
                    Ok(ControlFlow::Continue(()))
                },
            )?;
            self.snapshot.visit_projection_query(
                "SELECT o.owner_type, o.owner_id, o.page_id, o.name, o.normalized_name, \
                 o.value, o.ordinal, b.page_id FROM properties o \
                 LEFT JOIN blocks b ON o.owner_type = 1 AND b.block_id = o.owner_id \
                 ORDER BY o.owner_type, o.owner_id, o.name, o.ordinal",
                &[],
                |row| {
                    let decode = || -> Result<OwnerRow, String> {
                        let owner_id = blob16(row, 1, "properties.owner_id")?;
                        let page_id = blob16(row, 2, "properties.page_id")?;
                        let (owner_type, prefix) = match integer(row, 0, "properties.owner_type")? {
                            0 if owner_id == page_id => (OwnerType::Page, "p"),
                            1 if blob16(row, 7, "blocks.page_id")? == page_id => {
                                (OwnerType::Block, "b")
                            }
                            _ => return Err("invalid registry property ownership".into()),
                        };
                        let page_id = direct_registry_page_key(page_id);
                        if !pages.contains_key(&page_id) {
                            return Err("registry property names an absent page".into());
                        }
                        Ok(OwnerRow {
                            owner_type,
                            owner_id: format!("{prefix}:{}", hex16(owner_id)),
                            page_id,
                            source_name: text(row, 3, "properties.name")?,
                            normalized_name: text(row, 4, "properties.normalized_name")?,
                            value: text(row, 5, "properties.value")?,
                            ordinal: u32::try_from(integer(row, 6, "properties.ordinal")?)
                                .map_err(|_| "invalid registry property ordinal".to_owned())?,
                        })
                    };
                    rows.push(decode().map_err(MaterializationError::Corrupt)?);
                    Ok(ControlFlow::Continue(()))
                },
            )?;
            Ok(())
        })();
        if let Err(error) = read {
            return Err(if self.snapshot.cancellation().is_cancelled() {
                Error::Cancelled
            } else if matches!(error, MaterializationError::Corrupt(_)) {
                Error::Unavailable(Reason::InvalidSnapshot)
            } else {
                Error::Unavailable(Reason::ReadFailed)
            });
        }
        let registry = crate::query::registry::build_registry(
            rows.into_iter(),
            &|page| pages.get(page).cloned(),
            config,
        )
        .map_err(|_| Error::Unavailable(Reason::InvalidSnapshot))?;
        if self.snapshot.cancellation().is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(registry)
    }
}

#[cfg(test)]
impl DirectQueryJob<'_> {
    /// True once a drain (rebuild, reset, close) has cancelled this job. The
    /// production read checks the snapshot's own sticky flag between batches;
    /// this is the slot's view, for the drain tests.
    pub(crate) fn is_cancelled(&self) -> bool {
        self._slot.is_cancelled()
    }
}

/// What one attempt to open a query job produced (R3; the §5.9 states plus
/// the two the job owner adds).
pub(crate) enum QueryJobOpen<'a> {
    Job(DirectQueryJob<'a>),
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
}

/// What one attempt to answer through the D-15 statement seam produced
/// (SPEC §5.9). See [`DirectProjection::run_statement`].
pub(crate) enum StatementRead {
    Rows(Vec<Vec<PhysicalQueryValue>>),
    NotReady,
    Failed,
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
    pub(crate) fn start(path: PathBuf) -> std::io::Result<Self> {
        let shared = Arc::new(ProjectionShared {
            path,
            pending: Mutex::new(PendingProjection::default()),
            changed: Condvar::new(),
            ready: AtomicBool::new(false),
            ready_generation: AtomicU64::new(0),
            reader: Mutex::new(None),
            statement_seam: Mutex::new(None),
            query_jobs: QueryJobOwner::new(DEFAULT_QUERY_JOB_CAPACITY),
            session_pages: Mutex::new(Arc::new(HashSet::new())),
            fts_ready_at: AtomicU64::new(0),
            fts_ever_ready: AtomicBool::new(false),
            worker_available: AtomicBool::new(true),
            worker_failed: AtomicBool::new(false),
            worker_busy: AtomicBool::new(false),
            worker_finished: AtomicBool::new(false),
            worker_resources: Mutex::new(Some(Vec::new())),
            validated: AtomicBool::new(false),
            #[cfg(test)]
            indexed_reads: AtomicU64::new(0),
            #[cfg(test)]
            statement_reads: AtomicU64::new(0),
            #[cfg(test)]
            inject_read_failure: AtomicBool::new(false),
            #[cfg(test)]
            fallback_reads: AtomicU64::new(0),
            #[cfg(test)]
            referenced_name_reads: AtomicU64::new(0),
            #[cfg(test)]
            fuzzy_candidate_reads: AtomicU64::new(0),
        });
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("tine-direct-projection".into())
            .spawn(move || projection_worker(worker))?;
        Ok(Self { shared })
    }

    /// Keep repair requested until a complete source inventory or parser snapshot arrives.
    pub(crate) fn request_rebuild(&self) {
        let mut pending = self.shared.pending.lock().unwrap();
        pending.rebuild = true;
        self.shared.ready.store(false, Ordering::Release);
    }

    pub(crate) fn enqueue_full(
        &self,
        generation: u64,
        pages: PageSnapshot,
        revisions: PageRevisions,
        parse_config: Arc<ParseConfig>,
    ) {
        self.shared.ready.store(false, Ordering::Release);
        self.shared.worker_failed.store(false, Ordering::Release);
        let mut pending = self.shared.pending.lock().unwrap();
        pending.seed_page_order(pages.iter().map(|(entry, _)| entry.rel_path.as_str()));
        pending.full = Some(PendingFull {
            pages,
            revisions,
            parse_config,
        });
        pending.deltas.clear();
        pending.latest_generation = generation;
        // R6: a complete parsed snapshot owns readiness from here. A warm
        // validation or stream still in flight must not lower beside it — its
        // deltas carry no order positions and would erase the snapshot's.
        if pending.warm.is_some() || pending.warm_stream.is_some() || pending.order.is_some() {
            pending.warm = None;
            pending.warm_stream = None;
            pending.order = None;
            pending.warm_superseded = true;
            pending.warm_outcome = Some(WarmOutcome::Superseded);
        }
        pending.needs_full = false;
        self.shared.changed.notify_all();
    }

    /// R6 warm validation: hand the worker the walk inventory with exact
    /// content revisions and nothing parsed. Refused (`false`) when a newer
    /// mutation or queued work already outranks this generation — the caller
    /// then leaves readiness to the parser fallback, exactly as
    /// `install_built` does on generation drift.
    pub(crate) fn enqueue_warm(
        &self,
        generation: u64,
        sources: Vec<(PageEntry, String)>,
        parse_config: Arc<ParseConfig>,
    ) -> bool {
        if !self.shared.worker_available.load(Ordering::Acquire) {
            return false;
        }
        let mut pending = self.shared.pending.lock().unwrap();
        if pending.has_work()
            || pending.warm_stream.is_some()
            || pending.latest_generation > generation
            || (self.shared.worker_failed.load(Ordering::Acquire) && !pending.rebuild)
        {
            return false;
        }
        self.shared.ready.store(false, Ordering::Release);
        pending.seed_page_order(sources.iter().map(|(entry, _)| entry.rel_path.as_str()));
        // Optimistically open the stream now so every delta recorded from here
        // until the outcome carries no order position; the worker closes it
        // again in the same turn when the outcome is `Clean`.
        pending.warm_stream = Some(generation);
        pending.warm_outcome = None;
        pending.warm_superseded = false;
        pending.warm = Some(PendingWarm {
            generation,
            sources,
            parse_config,
        });
        pending.latest_generation = generation;
        self.shared.changed.notify_all();
        true
    }

    /// Block until the worker has decided the queued warm validation.
    pub(crate) fn wait_warm_outcome(&self) -> WarmOutcome {
        let mut pending = self.shared.pending.lock().unwrap();
        loop {
            if let Some(outcome) = pending.warm_outcome.take() {
                return outcome;
            }
            if !self.shared.worker_available.load(Ordering::Acquire) || pending.stop {
                return WarmOutcome::Failed;
            }
            pending = self.shared.changed.wait(pending).unwrap();
        }
    }

    /// R6 stream back-pressure: wait until fewer than `WARM_STREAM_HIGH_WATER`
    /// deltas are queued, so the warm thread never parses further ahead than
    /// one batch beyond the worker's current turn. `false` when the stream is
    /// no longer this thread's to feed (superseded, failed, or drifted).
    pub(crate) fn warm_stream_admit(&self, generation: u64, batch_len: usize) -> bool {
        let mut pending = self.shared.pending.lock().unwrap();
        loop {
            if pending.warm_superseded
                || pending.warm_stream != Some(generation)
                || pending.latest_generation > generation
                || pending.stop
                || !self.shared.worker_available.load(Ordering::Acquire)
                || self.shared.worker_failed.load(Ordering::Acquire)
            {
                return false;
            }
            if pending.deltas.len() + batch_len <= WARM_STREAM_HIGH_WATER {
                return true;
            }
            pending = self.shared.changed.wait(pending).unwrap();
        }
    }

    /// Queue one parsed batch of the warm stream. The session identity owner
    /// marks exact-revision restored IDs as Live, fresh IDs as Structural. A page that failed to
    /// parse is deleted from the projection. `false` means the batch was
    /// refused: a newer mutation outranks this generation, a full snapshot
    /// superseded the stream, or the worker failed — the caller abandons.
    pub(crate) fn enqueue_warm_stream(
        &self,
        generation: u64,
        batch: Vec<WarmStreamItem>,
        parse_config: Arc<ParseConfig>,
    ) -> bool {
        let mut pending = self.shared.pending.lock().unwrap();
        if pending.warm_superseded
            || pending.warm_stream != Some(generation)
            || pending.latest_generation > generation
            || self.shared.worker_failed.load(Ordering::Acquire)
        {
            return false;
        }
        for item in batch {
            let delta = match item {
                WarmStreamItem::Replace {
                    entry,
                    document,
                    revision,
                    identity,
                } => PageDelta::Replace {
                    entry,
                    document,
                    revision,
                    parse_config: Arc::clone(&parse_config),
                    query_page_order: None,
                    identity,
                },
                WarmStreamItem::Delete { entry } => PageDelta::Delete { entry },
            };
            pending.record_delta(generation, delta);
        }
        #[cfg(test)]
        MAX_PENDING_DELTAS.fetch_max(pending.deltas.len() as u64, Ordering::Relaxed);
        self.shared.changed.notify_all();
        true
    }

    /// Close the warm stream (R6): the worker reconciles `query_page_order`
    /// over the queue's inventory and then publishes readiness. `false` when
    /// the stream is no longer this thread's; a superseding snapshot owns
    /// readiness in that case and nothing is owed.
    pub(crate) fn finish_warm_stream(&self, generation: u64) -> bool {
        let mut pending = self.shared.pending.lock().unwrap();
        if pending.warm_superseded {
            return true;
        }
        if pending.warm_stream != Some(generation)
            || pending.latest_generation > generation
            || self.shared.worker_failed.load(Ordering::Acquire)
        {
            return false;
        }
        pending.order = Some(generation);
        self.shared.changed.notify_all();
        true
    }

    /// Abandon an open warm stream (R6: cancellation, drift, or a refused
    /// batch). Rows already validated or streamed are consistent, but the
    /// replacements not yet streamed are stale; only a full snapshot may
    /// publish readiness again. In-scope scenario: a save racing the warm.
    /// Returns whether a full snapshot superseded the stream — in which case
    /// that snapshot owns readiness and the caller has nothing to fall back to.
    pub(crate) fn abandon_warm_stream(&self, generation: u64) -> bool {
        let mut pending = self.shared.pending.lock().unwrap();
        if pending.warm_superseded {
            return true;
        }
        if pending.warm_stream != Some(generation) {
            return false;
        }
        pending.warm_stream = None;
        pending.order = None;
        pending.needs_full = true;
        self.shared.ready.store(false, Ordering::Release);
        self.shared.changed.notify_all();
        false
    }

    /// R6: the projected page inventory as `(name, path, text_kind)` rows,
    /// read through `drain_after` from the ready projection. `list_pages`
    /// rebuilds `PageEntry`s from it instead of parsing every file.
    pub(crate) fn page_inventory(
        &self,
        cache_generation: u64,
    ) -> Option<Vec<(String, String, i64)>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut rows = Vec::new();
        drain_after(
            |cursor: Option<([u8; 16], String)>, batch| {
                read.navigation_pages_after_with_header_validation(
                    cursor.as_ref().map(|(_, path)| path.as_str()),
                    cursor.as_ref().map(|(id, _)| id),
                    batch,
                    |_, kind| match kind {
                        0 | 1 => Ok(()),
                        _ => Err(tine_storage::sqlite::MaterializationError::Corrupt(
                            format!("unknown Direct Files text kind {kind}"),
                        )),
                    },
                )
            },
            |row| (row.page_id, row.path.clone()),
            |row| {
                rows.push((row.name, row.path, row.text_kind));
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
        .ok()?;
        self.ready_at(cache_generation).then_some(rows)
    }

    /// Bounded wait for readiness at `generation` (R6): the whole-graph derived
    /// reads that would otherwise fall to a full parse in the milliseconds
    /// after a save or a warm turn wait for that bounded worker turn first.
    /// Same ceiling and same non-authority as `wait_for_reference_generation`.
    pub(crate) fn wait_ready_at(&self, generation: u64) -> bool {
        self.wait_for_reference_generation(generation)
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
                query_page_order: None, // Filled under the queue lock, before coalescing.
                identity: DeltaIdentity::Live,
            },
        );
    }

    pub(crate) fn enqueue_delete(&self, generation: u64, entry: PageEntry) {
        self.enqueue_delta(generation, PageDelta::Delete { entry });
    }

    fn enqueue_delta(&self, generation: u64, delta: PageDelta) {
        self.shared.ready.store(false, Ordering::Release);
        let mut pending = self.shared.pending.lock().unwrap();
        pending.record_delta(generation, delta);
        self.shared.changed.notify_one();
    }

    pub(crate) fn mark_stale(&self) {
        self.shared.ready.store(false, Ordering::Release);
    }

    /// A reference read which races an already-queued one-page fact delta is
    /// much cheaper if it waits for that bounded worker turn than if it scans
    /// every parsed page. The timeout is a latency ceiling, not an authority:
    /// failure, worker loss, a newer generation, or expiry all return `false`
    /// and the caller uses the exact parser fallback.
    pub(crate) fn wait_for_reference_generation(&self, generation: u64) -> bool {
        if self.ready_at(generation) {
            return true;
        }
        let deadline = std::time::Instant::now() + REFERENCE_DELTA_WAIT;
        let mut pending = self.shared.pending.lock().unwrap();
        loop {
            if self.ready_at(generation) {
                return true;
            }
            if !self.shared.worker_available.load(Ordering::Acquire)
                || self.shared.worker_failed.load(Ordering::Acquire)
                || self.shared.ready_generation.load(Ordering::Acquire) > generation
                || pending.latest_generation > generation
            {
                return false;
            }
            if !pending.has_work()
                && pending.warm_stream.is_none()
                && !self.shared.worker_busy.load(Ordering::Acquire)
            {
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
        cache_generation: u64,
        autocomplete: bool,
        hidden_properties: &[String],
        max_items: usize,
        max_bytes: usize,
    ) -> Option<(Vec<(String, Vec<String>)>, bool)> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut accumulator = if autocomplete {
            PropertyFacetAccumulator::autocomplete(hidden_properties, max_items, max_bytes)
        } else {
            PropertyFacetAccumulator::query_builder(max_items, max_bytes)
        };
        drain_after(
            |cursor, batch| read.property_facet_rows_after(!autocomplete, cursor, batch),
            |row| (row.owner, row.source_name.clone(), row.ordinal),
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
        .ok()?;
        if !self.ready_at(cache_generation) {
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
    /// registry producer: it yields the same [`OwnerRow`] stream the Managed
    /// materialized read and the cold document iterator yield, and the
    /// aggregator downstream is byte-for-byte the same function.
    ///
    /// `None` means "not ready, or the read refused" — the caller falls back to
    /// the document iterator, exactly as §5.9's dispatch does for queries.
    pub(crate) fn property_owner_rows(
        &self,
        cache_generation: u64,
    ) -> Option<(
        Vec<crate::query::registry::OwnerRow>,
        HashMap<String, crate::query::registry::PageMeta>,
    )> {
        use crate::query::registry::{OwnerRow, OwnerType, PageMeta};
        #[cfg(test)]
        REGISTRY_READ_ATTEMPTS.with(|count| count.set(count.get() + 1));

        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();

        // The page map and the rows are read from the SAME `read`, i.e. the same
        // snapshot: a row naming a page the map does not have is a
        // snapshot-consistency defect and fails the build (§6.2), never a
        // silent fallback to Markdown.
        let mut pages: HashMap<String, PageMeta> = HashMap::new();
        drain_after(
            |cursor: Option<([u8; 16], String)>, batch| {
                read.navigation_pages_after_with_header_validation(
                    cursor.as_ref().map(|(_, path)| path.as_str()),
                    cursor.as_ref().map(|(id, _)| id),
                    batch,
                    |_, kind| match kind {
                        0 | 1 => Ok(()),
                        _ => Err(tine_storage::sqlite::MaterializationError::Corrupt(
                            format!("unknown Direct Files text kind {kind}"),
                        )),
                    },
                )
            },
            |row| (row.page_id, row.path.clone()),
            |row| {
                pages.insert(
                    direct_registry_page_key(row.page_id),
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
        .ok()?;

        let mut rows: Vec<OwnerRow> = Vec::new();
        drain_after(
            |cursor, batch| read.property_facet_rows_after(false, cursor, batch),
            |row| (row.owner, row.source_name.clone(), row.ordinal),
            |row| {
                let (owner_type, owner_id) = match row.owner {
                    PhysicalEntityId::Page(id) => (OwnerType::Page, format!("p:{}", hex16(id))),
                    PhysicalEntityId::Block(id) => (OwnerType::Block, format!("b:{}", hex16(id))),
                };
                rows.push(OwnerRow {
                    owner_type,
                    owner_id,
                    page_id: direct_registry_page_key(row.page_id),
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
        .ok()?;

        // The generation must still hold AFTER both scans, or the two halves
        // could straddle a rebuild — the same re-check `property_facets` makes.
        if !self.ready_at(cache_generation) {
            return None;
        }
        #[cfg(test)]
        self.shared.indexed_reads.fetch_add(1, Ordering::Relaxed);
        Some((rows, pages))
    }

    /// R3: open a database-owned query job at the current cache generation.
    ///
    /// Order matters and is the plan's (§2B): capacity FIRST, so a waiting job
    /// pins no WAL pages; then the owned snapshot, validated by
    /// `ready_at(generation)` before and after SQLite establishes the read
    /// transaction (`open_direct`); then the job registers its interrupt
    /// handle with the owner, which is what lets a rebuild reach a statement
    /// mid-flight; finally the identity input is captured and the generation
    /// re-checked, so the captured set describes the rows the snapshot sees.
    /// The statement's compiled-regex program is installed by
    /// `query::results::read_results` on the job's own connection — the ONE
    /// install site — so a job carries no regex state of its own.
    pub(crate) fn open_query_job(&self, cache_generation: u64) -> QueryJobOpen<'_> {
        if !self.ready_at(cache_generation) {
            return QueryJobOpen::NotReady;
        }
        #[cfg(test)]
        if self
            .shared
            .inject_read_failure
            .swap(false, Ordering::AcqRel)
        {
            return QueryJobOpen::Failed;
        }
        let slot = match self.shared.query_jobs.acquire() {
            Admission::Slot(slot) => slot,
            Admission::Cancelled => return QueryJobOpen::Cancelled,
            // RET2: capacity, not readiness. The projection is ready and other
            // jobs are draining, so this is the one `NotReady` the public
            // boundary may retry without ever considering a repair.
            Admission::Busy => return QueryJobOpen::Busy,
        };
        let validate = || {
            if self.ready_at(cache_generation) {
                Ok(())
            } else {
                Err(tine_storage::sqlite::MaterializationError::Incomplete(
                    "projection generation moved during snapshot acquisition".into(),
                ))
            }
        };
        let snapshot =
            match PhysicalProjectionQuerySnapshot::open_direct(&self.shared.path, validate) {
                Ok(snapshot) => snapshot,
                // The validator is the only `Incomplete` this call can produce and
                // it means the generation moved: not a defect. Anything else is
                // an unopenable or unreadable file.
                Err(_) if !self.ready_at(cache_generation) => return QueryJobOpen::NotReady,
                Err(_) => return QueryJobOpen::Failed,
            };
        if !slot.register(snapshot.cancellation()) {
            return QueryJobOpen::Cancelled;
        }
        let session_pages = Arc::clone(&self.shared.session_pages.lock().unwrap());
        if !self.ready_at(cache_generation) {
            return QueryJobOpen::NotReady;
        }
        #[cfg(test)]
        self.shared.statement_reads.fetch_add(1, Ordering::Relaxed);
        QueryJobOpen::Job(DirectQueryJob {
            _slot: slot,
            snapshot,
            session_pages,
        })
    }

    #[cfg(test)]
    pub(crate) fn session_pages_test(&self) -> Arc<HashSet<[u8; 16]>> {
        Arc::clone(&self.shared.session_pages.lock().unwrap())
    }

    #[cfg(test)]
    pub(crate) fn active_query_jobs_test(&self) -> usize {
        self.shared.query_jobs.active()
    }

    /// One read through the D-15 seam. [`DirectProjection::run_statement`] is
    /// this plus §5.9's dispatched-statement census and §4.3.2's regex
    /// registration; [`DirectProjection::fts_ready`] is this without either,
    /// because a readiness probe is not an answer and binds no pattern.
    ///
    /// R3: dispatched statements no longer run here — they run on a job's own
    /// owned snapshot (`open_query_job`), which is also where the statement's
    /// compiled-regex program is installed. The pooled seam serves the
    /// readiness probe and the reference readers only.
    fn seam_read(
        &self,
        cache_generation: u64,
        sql: &str,
        parameters: &[PhysicalQueryValue],
    ) -> StatementRead {
        if !self.ready_at(cache_generation) {
            return StatementRead::NotReady;
        }
        // Named `seam`, not `reader`, for the reason the field is (see
        // `ProjectionShared::statement_seam`).
        let mut seam = self.shared.statement_seam.lock().unwrap();
        if seam.is_none() {
            *seam = PhysicalProjectionQueryReader::open(&self.shared.path).ok();
        }
        let Some(seam) = seam.as_ref() else {
            return StatementRead::Failed;
        };
        let Ok(rows) = seam.run_projection_query(sql, parameters) else {
            return StatementRead::Failed;
        };
        // A snapshot that straddles a rebuild is not a snapshot — the same
        // re-check every other reader here makes. The generation moving is not a
        // projection defect, so it is `NotReady` and not `Failed`.
        if !self.ready_at(cache_generation) {
            return StatementRead::NotReady;
        }
        #[cfg(test)]
        self.shared.indexed_reads.fetch_add(1, Ordering::Relaxed);
        StatementRead::Rows(rows)
    }

    /// The EXISTING FTS-building signal (§5.10), read on the SAME materialized
    /// read and generation as the query it accelerates and SEPARATELY from
    /// projection readiness. `false` means the transient building phase, where
    /// the compiler omits candidate bounds and evaluates the same exact
    /// predicates on the ready block columns.
    ///
    /// A read that cannot answer reports `false`, which costs a bound and never
    /// an answer.
    pub(crate) fn fts_ready(&self, cache_generation: u64) -> bool {
        if self.shared.fts_ever_ready.load(Ordering::Acquire)
            && self.shared.fts_ready_at.load(Ordering::Acquire) == cache_generation
        {
            return true;
        }
        let ready = self.probe_fts_ready(cache_generation);
        if ready {
            self.shared
                .fts_ready_at
                .store(cache_generation, Ordering::Release);
            self.shared.fts_ever_ready.store(true, Ordering::Release);
        }
        ready
    }

    fn probe_fts_ready(&self, cache_generation: u64) -> bool {
        matches!(
            self.seam_read(
                cache_generation,
                crate::managed_query::FTS_READY_PROBE_SQL,
                &[],
            ),
            StatementRead::Rows(rows)
                if matches!(
                    rows.first().and_then(|row| row.first()),
                    Some(PhysicalQueryValue::Integer(1))
                )
        )
    }

    pub(crate) fn note_fallback_read(&self) {
        #[cfg(test)]
        self.shared.fallback_reads.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn referenced_page_names(&self, cache_generation: u64) -> Option<Vec<String>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut names = std::collections::HashMap::<String, String>::new();
        drain_after(
            |after: Option<(String, String, String, [u8; 16])>, batch| {
                read.navigation_reference_names_after(
                    after.as_ref().map(|(path, raw, normalized, id)| {
                        (path.as_str(), raw.as_str(), normalized.as_str(), id)
                    }),
                    batch,
                )
            },
            |row| {
                (
                    row.owner_path.clone(),
                    row.raw_name.clone(),
                    row.normalized_name.clone(),
                    row.source_page_id,
                )
            },
            |row| {
                names
                    .entry(crate::refs::page_key(&row.raw_name))
                    .or_insert(row.raw_name);
                Ok(())
            },
            |_, _| None,
        )
        .ok()?;
        if !self.ready_at(cache_generation) {
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

    pub(crate) fn fuzzy_candidate_paths(
        &self,
        cache_generation: u64,
        normalized_needle: &str,
    ) -> Option<std::collections::HashSet<String>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut paths = std::collections::HashSet::new();
        drain_after(
            |after, batch| {
                read.fuzzy_subsequence_candidate_pages_after(normalized_needle, after, batch)
            },
            |row| row.page_id,
            |row| {
                paths.insert(row.path);
                Ok(())
            },
            |_, _| None,
        )
        .ok()?;
        let current = self.ready_at(cache_generation).then_some(paths);
        #[cfg(test)]
        if current.is_some() {
            self.shared
                .fuzzy_candidate_reads
                .fetch_add(1, Ordering::Relaxed);
        }
        current
    }

    pub(crate) fn page_aliases_with_owners(
        &self,
        cache_generation: u64,
    ) -> Option<Vec<(String, String, String)>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut aliases = Vec::new();
        drain_after(
            |after: Option<(String, String, [u8; 16])>, batch| {
                read.navigation_aliases_after(
                    after
                        .as_ref()
                        .map(|(path, alias, id)| (path.as_str(), alias.as_str(), id)),
                    batch,
                )
            },
            |row| {
                (
                    row.owner_path.clone(),
                    row.normalized_alias.clone(),
                    row.source_page_id,
                )
            },
            |row| {
                aliases.push((row.normalized_alias, row.owner_name, row.owner_path));
                Ok(())
            },
            |_, _| None,
        )
        .ok()?;
        self.ready_at(cache_generation).then_some(aliases)
    }

    pub(crate) fn real_page_names(
        &self,
        cache_generation: u64,
    ) -> Option<crate::query::RealPageNames> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut names = crate::query::RealPageNames::new();
        drain_after(
            |after: Option<(String, [u8; 16])>, batch| {
                read.navigation_pages_after_with_header_validation(
                    after.as_ref().map(|(path, _)| path.as_str()),
                    after.as_ref().map(|(_, id)| id),
                    batch,
                    |_, _| Ok(()),
                )
            },
            |row| (row.path.clone(), row.page_id),
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
        .ok()?;
        self.ready_at(cache_generation).then_some(names)
    }

    pub(crate) fn reference_candidate_paths(
        &self,
        cache_generation: u64,
        names_norm: &[String],
        kind: ReferenceKind,
    ) -> Option<std::collections::BTreeSet<PathBuf>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        if kind == ReferenceKind::Plain
            && names_norm
                .iter()
                .any(|name| !name.chars().any(char::is_alphanumeric))
        {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut page_ids = std::collections::BTreeSet::new();
        for name in names_norm {
            match kind {
                ReferenceKind::Explicit => {
                    drain_after(
                        |after, batch| read.page_referrer_candidates_after(name, after, batch),
                        |row| (row.source_page_id, row.source),
                        |row| {
                            page_ids.insert(row.source_page_id);
                            Ok(())
                        },
                        |_, _| None,
                    )
                    .ok()?;
                }
                ReferenceKind::Plain => {
                    drain_after(
                        |after, batch| read.plain_text_candidate_pages_after(name, after, batch),
                        |row| row.page_id,
                        |row| {
                            page_ids.insert(row.page_id);
                            Ok(())
                        },
                        |_, _| None,
                    )
                    .ok()?;
                }
            }
        }
        let mut paths = std::collections::BTreeSet::new();
        for page_id in page_ids {
            let page = read
                .page_with_header_validation(page_id, |_, _| Ok(()))
                .ok()??;
            paths.insert(PathBuf::from(page.path));
        }
        self.ready_at(cache_generation).then_some(paths)
    }

    /// Outer `None` means projection unavailable/stale and requires parser
    /// fallback. Inner `None` is an exact current-generation miss.
    pub(crate) fn block_page_hint(
        &self,
        cache_generation: u64,
        uuid: &str,
    ) -> Option<Option<String>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let uuid = Uuid::parse_str(uuid).ok()?.into_bytes();
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let block = match read.block(uuid).ok()? {
            Some(block) => crate::query::logseq_uuid_owner([block], false),
            None => {
                crate::query::logseq_uuid_owner(read.blocks_by_logseq_uuid(uuid, 2).ok()?, false)
            }
        };
        let page = match block {
            Some(block) => read
                .page_with_header_validation(block.page_id, |_, _| Ok(()))
                .ok()?
                .map(|page| page.name),
            None => None,
        };
        self.ready_at(cache_generation).then_some(page)
    }

    pub(crate) fn block_ref_counts(
        &self,
        cache_generation: u64,
    ) -> Option<std::collections::HashMap<String, usize>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
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
        .ok()?;
        self.ready_at(cache_generation).then_some(counts)
    }

    pub(crate) fn block_referrer_candidate_paths(
        &self,
        cache_generation: u64,
        uuid: &str,
    ) -> Option<std::collections::BTreeSet<PathBuf>> {
        if !self.ready_at(cache_generation) {
            return None;
        }
        let uuid = Uuid::parse_str(uuid).ok()?.into_bytes();
        let mut reader = self.shared.reader.lock().unwrap();
        if reader.is_none() {
            *reader = PhysicalGraphProjectionDatabase::open_read_only(&self.shared.path).ok();
        }
        let read = reader.as_ref()?.read();
        let mut page_ids = std::collections::BTreeSet::new();
        drain_after(
            |after, batch| read.block_referrer_candidates_after(uuid, after, batch),
            |row| (row.source_page_id, row.source_block_id),
            |row| {
                page_ids.insert(row.source_page_id);
                Ok(())
            },
            |_, _| None,
        )
        .ok()?;
        let mut paths = std::collections::BTreeSet::new();
        for page_id in page_ids {
            let page = read
                .page_with_header_validation(page_id, |_, _| Ok(()))
                .ok()??;
            paths.insert(PathBuf::from(page.path));
        }
        self.ready_at(cache_generation).then_some(paths)
    }

    pub(crate) fn ready_at(&self, generation: u64) -> bool {
        self.shared.ready.load(Ordering::Acquire)
            && self.shared.ready_generation.load(Ordering::Acquire) == generation
    }

    /// RET2's readiness lifecycle: why this generation is not ready, and what
    /// the caller may do about it.
    ///
    /// The order of the tests is the order of authority.
    ///
    /// * `worker_available` is stored `false` exactly where the worker thread
    ///   gives up for good — no parent directory, an unopenable database, a
    ///   writer lease another instance owns, or a `stop` turn. Nothing this
    ///   graph enqueues afterwards is ever taken, so retrying is endless by
    ///   construction and the caller owes a bounded error instead.
    /// * A queued turn is progress even when the LAST turn failed:
    ///   `worker_failed` stays set until the next successful turn, and the
    ///   repair that clears it is exactly the `full`/`rebuild` work below.
    /// * A failed worker with an EMPTY queue is the stale-idle case: the turn
    ///   failed, `requires_full_rebuild` latched inside the worker, and until
    ///   a complete source inventory arrives every further delta turn refuses.
    ///   That is a repair, not a wait.
    pub(crate) fn progress_at(&self, generation: u64) -> ProjectionProgress {
        use crate::query::QueryReadinessReason as Reason;
        if self.ready_at(generation) {
            return ProjectionProgress::Ready;
        }
        let pending = self.shared.pending.lock().unwrap();
        if pending.stop || !self.shared.worker_available.load(Ordering::Acquire) {
            return ProjectionProgress::Stopped;
        }
        if pending.rebuild || pending.needs_full || pending.full.is_some() {
            return ProjectionProgress::Working(Reason::Recovering);
        }
        if pending.warm.is_some() || pending.warm_stream.is_some() || pending.order.is_some() {
            return ProjectionProgress::Working(Reason::Indexing);
        }
        if !pending.deltas.is_empty() {
            return ProjectionProgress::Working(Reason::PendingEdits);
        }
        if self.shared.worker_failed.load(Ordering::Acquire) {
            // The queue is empty and the last turn failed: nothing is coming.
            return ProjectionProgress::Stale;
        }
        if self.shared.worker_busy.load(Ordering::Acquire) {
            return ProjectionProgress::Working(Reason::Busy);
        }
        ProjectionProgress::Stale
    }

    /// Test diagnostic: the queue and readiness state in one line, for a
    /// convergence failure that would otherwise be a bare timeout.
    #[cfg(test)]
    pub(crate) fn debug_state_test(&self) -> String {
        let pending = self.shared.pending.lock().unwrap();
        format!(
            "ready={} validated={} ready_generation={} latest_generation={} full={} deltas={} warm={} warm_outcome={:?} warm_stream={:?} order={:?} superseded={} needs_full={} rebuild={} stop={} page_order={} worker_available={} worker_failed={} worker_busy={}",
            self.shared.ready.load(Ordering::Acquire),
            self.shared.validated.load(Ordering::Acquire),
            self.shared.ready_generation.load(Ordering::Acquire),
            pending.latest_generation,
            pending.full.is_some(),
            pending.deltas.len(),
            pending.warm.is_some(),
            pending.warm_outcome.as_ref().map(|outcome| match outcome {
                WarmOutcome::Clean => "Clean".to_owned(),
                WarmOutcome::Replacements(pages) => format!("Replacements({})", pages.len()),
                WarmOutcome::Superseded => "Superseded".to_owned(),
                WarmOutcome::Failed => "Failed".to_owned(),
            }),
            pending.warm_stream,
            pending.order,
            pending.warm_superseded,
            pending.needs_full,
            pending.rebuild,
            pending.stop,
            pending.page_order.len(),
            self.shared.worker_available.load(Ordering::Acquire),
            self.shared.worker_failed.load(Ordering::Acquire),
            self.shared.worker_busy.load(Ordering::Acquire),
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
        self.shared.query_jobs.close();
    }

    #[cfg(test)]
    pub(crate) fn inject_next_statement_failure(&self) {
        self.shared
            .inject_read_failure
            .store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn statement_reads(&self) -> u64 {
        self.shared.statement_reads.load(Ordering::Relaxed)
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
    pub(crate) fn fuzzy_candidate_reads(&self) -> u64 {
        self.shared.fuzzy_candidate_reads.load(Ordering::Relaxed)
    }

    /// R3: refuse new jobs, interrupt the active ones and wait for their slots
    /// before the worker is told to stop, so no snapshot outlives the
    /// projection that admitted it. Idempotent — `stop` and a closed admission
    /// owner are both terminal, so a caller that closes explicitly and then
    /// drops pays a second no-op drain and nothing else.
    fn close(&self) {
        self.shared.query_jobs.close();
        {
            let mut pending = self.shared.pending.lock().unwrap();
            pending.stop = true;
        }
        self.shared.changed.notify_all();
    }

    /// Retain a resource until the writer has released its connection and lease.
    /// The publication root also has a foreground owner until its graph drops.
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
/// projection WRITE path, and that path names the graph — `apply_pending`
/// formats `entry.rel_path` straight into its error string, and
/// `MaterializationError`'s payloads are free-form `String`s produced while
/// storing parsed page text. I-9: the family still reaches the always-on
/// record, because a user who is not running under `TINE_DEBUG` otherwise sees
/// only an unavailable index. The prose stays on the directed debug channel.
fn report_projection_failure(family: &str, detail: &dyn std::fmt::Display) {
    eprintln!("[tine] Direct Files SQLite projection {family}");
    if crate::sync_runtime::runtime_debug_diagnostics_enabled() {
        eprintln!("[tine] Direct Files SQLite projection {family}; directed detail: {detail}");
    }
}

fn projection_worker(shared: Arc<ProjectionShared>) {
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
        shared.worker_available.store(false, Ordering::Release);
        shared.changed.notify_all();
        return;
    }
    let lease_path = shared.path.with_extension("sqlite.writer.lock");
    let lease = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lease_path)
        .and_then(|file| {
            file.try_lock_exclusive()?;
            Ok(file)
        }) {
        Ok(lease) => lease,
        Err(error) => {
            eprintln!(
                "[tine] Direct Files SQLite projection unavailable; another graph instance owns it or its lease cannot be opened: {error}"
            );
            shared.worker_available.store(false, Ordering::Release);
            shared.changed.notify_all();
            return;
        }
    };
    let mut writer_slot = match open_projection_database(&shared.path) {
        Ok(database) => Some(database),
        Err(error) => {
            report_projection_failure("disabled: its database could not be opened", &error);
            shared.worker_available.store(false, Ordering::Release);
            shared.changed.notify_all();
            return;
        }
    };
    // The lock file is app-private disposable state. Retain its exclusive lock
    // for the complete writer lifetime so another Graph instance cannot replace
    // this database's facts behind a locally-ready generation watermark.
    let _lease = lease;
    let mut requires_full_rebuild = false;
    loop {
        let turn = {
            let mut pending = shared.pending.lock().unwrap();
            while !pending.has_work() && !pending.stop {
                pending = shared.changed.wait(pending).unwrap();
            }
            if pending.stop {
                shared.worker_available.store(false, Ordering::Release);
                shared.changed.notify_all();
                return;
            }
            shared.worker_busy.store(true, Ordering::Release);
            if std::mem::take(&mut pending.needs_full) {
                requires_full_rebuild = true;
            }
            let rebuild = (pending.full.is_some() || pending.warm.is_some())
                && std::mem::take(&mut pending.rebuild);
            // R6: a full snapshot queued beside a warm validation owns
            // readiness; the warm is dropped as superseded.
            let warm = if pending.full.is_some() {
                if pending.warm.take().is_some() {
                    pending.warm_stream = None;
                    pending.warm_superseded = true;
                    pending.warm_outcome = Some(WarmOutcome::Superseded);
                }
                None
            } else {
                pending.warm.take()
            };
            let order = pending.order.take();
            let deltas = std::mem::take(&mut pending.deltas);
            let unordered = deltas.values().any(|(_, delta)| {
                matches!(
                    delta,
                    PageDelta::Replace {
                        query_page_order: None,
                        ..
                    }
                )
            });
            let inventory = (order.is_some() || warm.is_some() || unordered)
                .then(|| pending.ordered_inventory());
            WorkerTurn {
                full: pending.full.take(),
                warm,
                deltas,
                order,
                inventory,
                stream_open: pending.warm_stream.is_some(),
                latest_generation: pending.latest_generation,
                rebuild,
            }
        };
        let WorkerTurn {
            full,
            warm,
            deltas,
            order,
            inventory,
            stream_open,
            latest_generation,
            rebuild,
        } = turn;
        let had_full = full.is_some();
        let had_warm = warm.is_some();
        let stream_closed = order.is_some();
        #[cfg(test)]
        run_before_apply_pending_hook();
        let applied: Result<AppliedTurn, String> =
            if requires_full_rebuild && !had_full && !had_warm {
                Err("a prior projection failure requires a complete source inventory".into())
            } else {
                (|| {
                    if rebuild || requires_full_rebuild || writer_slot.is_none() {
                        // R3: interrupt and drain every query job first, so no
                        // owned snapshot retains a handle to the file about to be
                        // reset or removed, and the rebuild never waits on a read
                        // nobody will finish. In-scope scenario: a torn projection
                        // rebuilt under a live reader (D-3).
                        shared.query_jobs.cancel_all_and_drain();
                        // Drop every connection before the disposable file can be
                        // replaced; a reader must not retain an old file handle.
                        let mut reader = shared.reader.lock().unwrap();
                        let mut seam = shared.statement_seam.lock().unwrap();
                        reader.take();
                        seam.take();
                        shared.fts_ever_ready.store(false, Ordering::Release);
                        writer_slot.take();
                        let mut database = open_projection_database(&shared.path)
                            .map_err(|error| error.to_string())?;
                        // Even repaired DDL leaves unchanged source stamps behind.
                        // Reset them so the complete inventory relowers every source page.
                        database.reset().map_err(|error| error.to_string())?;
                        writer_slot = Some(database);
                    }
                    let mut applied =
                        apply_pending(writer_slot.as_mut().unwrap(), full, warm.as_ref(), deltas)?;
                    // R6: the stream's closing turn (or a `Clean` warm turn, or a
                    // turn that lowered mid-stream deltas without positions)
                    // reconciles the order table over the queue's inventory. The
                    // queue's map tracks every applied replacement and deletion
                    // since its seed, so it names exactly the projected pages.
                    let warm_clean = matches!(applied.warm_outcome, Some(WarmOutcome::Clean));
                    applied.stream_open = if had_warm {
                        matches!(applied.warm_outcome, Some(WarmOutcome::Replacements(_)))
                    } else {
                        stream_open && !stream_closed
                    };
                    if !applied.stream_open
                        && (stream_closed || warm_clean || applied.unordered_replacements)
                    {
                        let inventory = inventory.ok_or_else(|| {
                            "the order turn ran without its queue inventory".to_owned()
                        })?;
                        writer_slot
                            .as_mut()
                            .unwrap()
                            .apply_with_source_revisions_aliases_and_page_order(
                                &PhysicalGraphProjectionChange {
                                    replacements: Vec::new(),
                                    deletions: Vec::new(),
                                    reference_postings: Vec::new(),
                                },
                                &[],
                                &[],
                                &inventory,
                            )
                            .map_err(|error| error.to_string())?;
                    }
                    Ok(applied)
                })()
            };
        let applied = match applied {
            Ok(applied) => applied,
            Err(error) => {
                requires_full_rebuild = true;
                shared.ready.store(false, Ordering::Release);
                shared.worker_failed.store(true, Ordering::Release);
                {
                    let mut pending = shared.pending.lock().unwrap();
                    if had_warm {
                        pending.warm_outcome = Some(WarmOutcome::Failed);
                    }
                    if had_warm || stream_closed {
                        pending.warm_stream = None;
                        pending.order = None;
                    }
                }
                shared.worker_busy.store(false, Ordering::Release);
                shared.changed.notify_all();
                report_projection_failure(PROJECTION_UPDATE_FAILURE, &error);
                continue;
            }
        };
        shared.record_session_pages(&applied.pages);
        if had_full || had_warm {
            requires_full_rebuild = false;
        }
        if had_full || stream_closed || matches!(applied.warm_outcome, Some(WarmOutcome::Clean)) {
            shared.validated.store(true, Ordering::Release);
        }
        shared.worker_failed.store(false, Ordering::Release);
        let mut pending = shared.pending.lock().unwrap();
        shared.worker_busy.store(false, Ordering::Release);
        if had_warm {
            // A `Replacements` outcome keeps the stream open at its generation
            // and readiness waits for the closing order turn; any other
            // outcome closes the stream this warm opened.
            if !applied.stream_open {
                pending.warm_stream = None;
            }
            if pending.warm_outcome.is_none() && !pending.warm_superseded {
                pending.warm_outcome = applied.warm_outcome.clone();
            }
        }
        if stream_closed {
            pending.warm_stream = None;
        }
        if !pending.rebuild
            && !pending.has_work()
            && pending.warm_stream.is_none()
            && pending.latest_generation == latest_generation
            && shared.validated.load(Ordering::Acquire)
        {
            shared
                .ready_generation
                .store(latest_generation, Ordering::Release);
            shared.ready.store(true, Ordering::Release);
        }
        drop(pending);
        shared.changed.notify_all();
    }
}

/// One worker turn's queued work (R6 widened it beyond full + deltas).
struct WorkerTurn {
    full: Option<PendingFull>,
    warm: Option<PendingWarm>,
    deltas: BTreeMap<String, (u64, PageDelta)>,
    order: Option<u64>,
    /// The queue's inventory captured with the deltas, so the order turn
    /// reconciles exactly the pages this turn leaves projected.
    inventory: Option<Vec<[u8; 16]>>,
    /// Whether a warm stream was open when the turn was taken.
    stream_open: bool,
    latest_generation: u64,
    rebuild: bool,
}

fn open_projection_database(
    path: &Path,
) -> Result<PhysicalGraphProjectionDatabase, tine_storage::sqlite::MaterializationError> {
    let database = PhysicalGraphProjectionDatabase::open_writable(path)?;
    if database.validate_schema().is_ok() && database.quick_check().is_ok() {
        return Ok(database);
    }
    if database.initialize_schema().is_ok()
        && database.validate_schema().is_ok()
        && database.quick_check().is_ok()
    {
        return Ok(database);
    }
    drop(database);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let database = PhysicalGraphProjectionDatabase::open_writable(path)?;
    database.initialize_schema()?;
    database.validate_schema()?;
    Ok(database)
}

/// Which pages one worker turn actually WROTE (R3 identity policy): the pages
/// whose rows now carry this process's live runtime ids, and the pages whose
/// rows are gone. A full snapshot on a warm reopen reuses every unchanged
/// page's rows, so "a full snapshot was applied" is not "every page was
/// lowered" — only the source delta's replacements were.
#[derive(Default)]
struct AppliedPages {
    lowered: Vec<[u8; 16]>,
    deleted: Vec<[u8; 16]>,
    /// R6: pages relowered from a fresh parse; they leave the session set.
    relowered_structurally: Vec<[u8; 16]>,
}

#[derive(Default)]
struct AppliedTurn {
    pages: AppliedPages,
    /// R6: the warm validation's verdict, when this turn ran one.
    warm_outcome: Option<WarmOutcome>,
    /// R6: this turn lowered replacements that carried no order position
    /// (queued while a stream was open), so the order table must be
    /// reconciled once the stream is closed.
    unordered_replacements: bool,
    stream_open: bool,
}

/// R6 warm validation inside one worker turn: compare the walk inventory's
/// exact revisions with `direct_source_revisions`, delete what the walk no
/// longer has, and name what must be relowered. Nothing here parses.
fn validate_warm(
    database: &mut PhysicalGraphProjectionDatabase,
    warm: &PendingWarm,
    applied: &mut AppliedPages,
) -> Result<WarmOutcome, String> {
    let config_digest = warm.parse_config.digest();
    let sources = warm
        .sources
        .iter()
        .map(|(entry, revision)| PhysicalGraphProjectionSourceRevision {
            page_id: page_id(&entry.rel_path),
            revision: projection_source_revision(revision, config_digest),
        })
        .collect::<Vec<_>>();
    let source_delta = database
        .source_delta(&sources)
        .map_err(|error| error.to_string())?;
    if !source_delta.deletions.is_empty() {
        applied
            .deleted
            .extend(source_delta.deletions.iter().copied());
        database
            .apply_with_source_revisions_and_aliases(
                &PhysicalGraphProjectionChange {
                    replacements: Vec::new(),
                    deletions: source_delta.deletions,
                    reference_postings: Vec::new(),
                },
                &[],
                &[],
            )
            .map_err(|error| error.to_string())?;
    }
    if source_delta.replacements.is_empty() {
        return Ok(WarmOutcome::Clean);
    }
    let needed = source_delta
        .replacements
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    Ok(WarmOutcome::Replacements(
        warm.sources
            .iter()
            .filter(|(entry, _)| needed.contains(&page_id(&entry.rel_path)))
            .map(|(entry, _)| entry.clone())
            .collect(),
    ))
}

fn apply_pending(
    database: &mut PhysicalGraphProjectionDatabase,
    full: Option<PendingFull>,
    warm: Option<&PendingWarm>,
    deltas: BTreeMap<String, (u64, PageDelta)>,
) -> Result<AppliedTurn, String> {
    let mut turn = AppliedTurn::default();
    let applied = &mut turn.pages;
    if let Some(PendingFull {
        pages,
        revisions,
        parse_config,
    }) = full
    {
        let parse_config = parse_config.as_ref();
        let config_digest = parse_config.digest();
        let sources = pages
            .iter()
            .map(|(entry, _)| {
                Ok(PhysicalGraphProjectionSourceRevision {
                    page_id: page_id(&entry.rel_path),
                    revision: projection_source_revision(
                        revisions.get(&entry.path).ok_or_else(|| {
                            format!(
                                "parsed page has no exact source revision: {}",
                                entry.rel_path
                            )
                        })?,
                        config_digest,
                    ),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let source_delta = database
            .source_delta(&sources)
            .map_err(|error| error.to_string())?;
        let replacements_needed = source_delta
            .replacements
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let inventory = sources
            .iter()
            .map(|source| source.page_id)
            .collect::<Vec<_>>();
        let lowered = pages
            .iter()
            .enumerate()
            .filter(|(_, (entry, _))| replacements_needed.contains(&page_id(&entry.rel_path)))
            .map(|(position, (entry, document))| {
                let (mut page, postings, aliases) = physical_page(entry, document, parse_config)?;
                page.query_page_order = Some(position as u64);
                Ok::<_, String>((page, postings, aliases))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut replacements = Vec::with_capacity(lowered.len());
        let mut reference_postings = Vec::new();
        let mut aliases = Vec::new();
        for (page, mut postings, mut page_aliases) in lowered {
            replacements.push(page);
            reference_postings.append(&mut postings);
            aliases.append(&mut page_aliases);
        }
        let replacement_sources = sources
            .into_iter()
            .filter(|source| replacements_needed.contains(&source.page_id))
            .collect::<Vec<_>>();
        applied.lowered.extend(replacements_needed.iter().copied());
        applied
            .deleted
            .extend(source_delta.deletions.iter().copied());
        database
            .apply_with_source_revisions_aliases_and_page_order(
                &PhysicalGraphProjectionChange {
                    replacements,
                    deletions: source_delta.deletions,
                    reference_postings,
                },
                &replacement_sources,
                &aliases,
                &inventory,
            )
            .map_err(|error| error.to_string())?;
    }
    if let Some(warm) = warm {
        turn.warm_outcome = Some(validate_warm(database, warm, applied)?);
    }
    if !deltas.is_empty() {
        let mut replacements = Vec::new();
        let mut reference_postings = Vec::new();
        let mut aliases = Vec::new();
        let mut replacement_sources = Vec::new();
        let mut deletions = Vec::new();
        for (_, (_, delta)) in deltas {
            match delta {
                // Each replacement lowers under the config it was queued with,
                // never under a later page's or a default (F11).
                PageDelta::Replace {
                    entry,
                    document,
                    revision,
                    parse_config,
                    query_page_order,
                    identity,
                } => {
                    replacement_sources.push(PhysicalGraphProjectionSourceRevision {
                        page_id: page_id(&entry.rel_path),
                        revision: projection_source_revision(&revision, parse_config.digest()),
                    });
                    let (mut page, mut postings, mut page_aliases) =
                        physical_page(&entry, &document, &parse_config)?;
                    page.query_page_order = query_page_order;
                    if query_page_order.is_none() {
                        turn.unordered_replacements = true;
                    }
                    match identity {
                        DeltaIdentity::Live => applied.lowered.push(page.page_id),
                        DeltaIdentity::Structural => {
                            applied.relowered_structurally.push(page.page_id)
                        }
                    }
                    replacements.push(page);
                    reference_postings.append(&mut postings);
                    aliases.append(&mut page_aliases);
                }
                PageDelta::Delete { entry } => {
                    let id = page_id(&entry.rel_path);
                    applied.deleted.push(id);
                    deletions.push(id);
                }
            }
        }
        database
            .apply_with_source_revisions_and_aliases(
                &PhysicalGraphProjectionChange {
                    replacements,
                    deletions,
                    reference_postings,
                },
                &replacement_sources,
                &aliases,
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(turn)
}

/// The revision Direct Files compares to decide whether a page's rows are still
/// current. Folding the parse-config digest in is what makes a config edit a
/// full re-lowering (§5.8 J7): reconciliation compares only source revisions,
/// so without it an unchanged file would keep rows built under the old config.
fn projection_source_revision(
    content_revision: &str,
    parse_config_digest: tine_storage::ContentDigest,
) -> String {
    let digest = parse_config_digest
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("direct-facts-v{DIRECT_PROJECTION_FACTS_VERSION}:{digest}:{content_revision}")
}

/// The Direct Files producer, reachable from the cross-backend parity guard.
///
/// Named as a seam rather than widened: the guard has to compare the rows this
/// exact function emits against the Managed Storage producer's and the walk's,
/// and a reimplementation in the test would prove only that the test agrees
/// with itself (§5.8 G1, I-19).
#[cfg(test)]
pub(crate) fn physical_page_for_test(
    entry: &PageEntry,
    document: &Document,
    parse_config: &ParseConfig,
) -> Result<PhysicalPage, String> {
    physical_page(entry, document, parse_config).map(|(page, _, _)| page)
}

fn physical_page(
    entry: &PageEntry,
    document: &Document,
    parse_config: &ParseConfig,
) -> Result<
    (
        PhysicalPage,
        Vec<PhysicalReferencePosting>,
        Vec<PhysicalAliasDeclaration>,
    ),
    String,
> {
    #[cfg(test)]
    {
        let mut receipt = PHYSICAL_PAGE_LOWERINGS.lock().unwrap();
        if receipt
            .0
            .as_ref()
            .is_some_and(|root| entry.path.starts_with(root))
        {
            receipt.1 += 1;
        }
    }
    let id = page_id(&entry.rel_path);
    let format = Format::from_path(Path::new(&entry.rel_path));
    let is_org = format == Format::Org;
    // `Format::from_path` and never `reference_source_is_org`: the latter is a
    // case-sensitive `ends_with(".org")` and would type an `Outline.ORG` page
    // Markdown here while Direct Files types it Org (§5.8 E4).
    let atom_format = crate::query::atom::AtomFormat::from(format);
    let (preamble_search, properties, tags) = document
        .pre_block
        .as_deref()
        .map(|raw| facets(raw, is_org))
        .unwrap_or_default();
    let searchable_text = if preamble_search.is_empty() {
        entry.name.clone()
    } else {
        format!("{} {preamble_search}", entry.name)
    };
    let mut blocks = Vec::new();
    let mut reference_postings = Vec::new();
    let aliases = crate::query::document_aliases(document)
        .into_iter()
        .enumerate()
        .map(|(ordinal, alias)| {
            Ok(PhysicalAliasDeclaration {
                source_page_id: id,
                source_entity: PhysicalEntityId::Page(id),
                source_locator: b"page-alias".to_vec(),
                ordinal: u32::try_from(ordinal)
                    .map_err(|_| "one page exceeds u32::MAX aliases".to_string())?,
                raw_alias: alias.clone(),
                normalized_alias: alias,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if let Some(preamble) = document.pre_block.as_deref() {
        append_reference_postings(
            &mut reference_postings,
            id,
            PhysicalEntityId::Page(id),
            b"preamble",
            std::iter::empty(),
            crate::doc::property_reference_page_names(preamble).into_iter(),
        )?;
    }
    let mut block_refs_norm: Vec<Vec<String>> = Vec::new();
    lower_blocks(
        &document.roots,
        id,
        None,
        &mut Vec::new(),
        &mut blocks,
        &mut reference_postings,
        &mut block_refs_norm,
        parse_config,
        atom_format,
    )?;
    // The two derived tables come from the ONE tine-core computation (§5.8):
    // this side only hands it the block's own `refs_norm` and its parent.
    let flat = blocks
        .iter()
        .zip(block_refs_norm.iter())
        .map(|(block, refs)| crate::query::path_refs::PathRefBlock {
            id: block.block_id,
            parent: block.parent,
            refs: refs.as_slice(),
        })
        .collect::<Vec<_>>();
    let mut path_refs = crate::query::derived::path_ref_rows(&entry.name, &flat);
    for block in &mut blocks {
        block.path_refs = path_refs.remove(&block.block_id).unwrap_or_default();
    }
    let journal_days = crate::query::derived::JournalDays::new(parse_config);
    let page_property_atoms = crate::query::derived::property_atom_rows(
        &properties
            .iter()
            .map(|property| (property.name.clone(), property.value.clone()))
            .collect::<Vec<_>>(),
        atom_format,
        parse_config,
    );
    Ok((
        PhysicalPage {
            page_id: id,
            query_page_order: None,
            home_document_id: id,
            name: entry.name.clone(),
            name_key: crate::refs::page_key(&entry.name),
            path: entry.rel_path.clone(),
            text_kind: page_kind_to_sql(entry.kind),
            journal_day: journal_days.day(&entry.rel_path, entry.kind == PageKind::Journal),
            preamble: document.pre_block.clone(),
            normalized_searchable_text: searchable_text.to_lowercase().nfc().collect(),
            searchable_text,
            references: Vec::new(),
            properties,
            tags: crate::query::derived::tag_rows(&tags),
            property_atoms: page_property_atoms,
            blocks,
        },
        reference_postings,
        aliases,
    ))
}

#[allow(clippy::too_many_arguments)]
fn lower_blocks(
    source: &[DocBlock],
    page_id: [u8; 16],
    parent: Option<[u8; 16]>,
    structural_path: &mut Vec<u32>,
    out: &mut Vec<PhysicalBlock>,
    reference_postings: &mut Vec<PhysicalReferencePosting>,
    refs_norm: &mut Vec<Vec<String>>,
    parse_config: &ParseConfig,
    atom_format: crate::query::atom::AtomFormat,
) -> Result<(), String> {
    for (position, block) in source.iter().enumerate() {
        let position = u32::try_from(position)
            .map_err(|_| "page has more than u32::MAX sibling blocks".to_string())?;
        structural_path.push(position);
        let block_id = Uuid::parse_str(&block.uuid)
            .map_err(|_| {
                format!(
                    "block has no assigned runtime UUID in projection: {}",
                    block.uuid
                )
            })?
            .into_bytes();
        let projection = block.projection();
        let order = structural_path
            .iter()
            .map(|part| format!("{part:08x}"))
            .collect::<Vec<_>>()
            .join("/");
        append_reference_postings(
            reference_postings,
            page_id,
            PhysicalEntityId::Block(block_id),
            order.as_bytes(),
            projection.refs_page.iter().cloned(),
            crate::doc::property_reference_page_names(&block.raw).into_iter(),
        )?;
        for raw_claim in &projection.block_refs {
            let Ok(raw_claim) = Uuid::parse_str(raw_claim) else {
                continue;
            };
            reference_postings.push(PhysicalReferencePosting {
                source_page_id: page_id,
                source_entity: PhysicalEntityId::Block(block_id),
                source_locator: order.as_bytes().to_vec(),
                ordinal: u32::try_from(reference_postings.len())
                    .map_err(|_| "one page exceeds u32::MAX reference postings".to_string())?,
                kind: 6,
                target: PhysicalReferenceTarget::ExternalUuid {
                    raw_claim: raw_claim.into_bytes(),
                    resolved_block_id: None,
                },
            });
        }
        let searchable_text = projection
            .visible
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        // The query columns are the EXACT visible text and its fold, never the
        // whitespace-collapsed `searchable_text` beside them (§5.10).
        let (query_visible, query_visible_folded) = crate::query::derived::query_visible_columns(
            &projection.visible,
            Some(&projection.visible_lower),
        );
        let properties = projection
            .properties
            .iter()
            .map(|(name, value)| PhysicalProperty {
                name: name.clone(),
                normalized_name: property_key_norm(name),
                value: value.clone(),
            })
            .collect();
        let property_atoms = crate::query::derived::property_atom_rows(
            &projection.properties,
            atom_format,
            parse_config,
        );
        refs_norm.push(projection.refs_norm.clone());
        let logseq_uuid = block
            .property("id")
            .and_then(|value| Uuid::parse_str(value.trim()).ok())
            .map(Uuid::into_bytes);
        out.push(PhysicalBlock {
            block_id,
            query_result_id: block.uuid.clone(),
            own_refs: projection.refs_norm.clone(),
            home_document_id: page_id,
            parent,
            order,
            content: block.raw.clone(),
            normalized_searchable_text: searchable_text.to_lowercase().nfc().collect(),
            searchable_text,
            query_visible,
            query_visible_folded,
            heading_level: projection.heading_level,
            collapsed: block.collapsed(),
            logseq_uuid,
            logseq_identity_origin: logseq_uuid.map(|_| 0),
            references: Vec::new(),
            properties,
            tags: crate::query::derived::tag_rows(&projection.tags),
            task: projection.marker.as_ref().map(|marker| PhysicalTask {
                marker: marker.to_ascii_uppercase(),
                priority: projection.priority.clone(),
                scheduled: projection.scheduled.clone(),
                deadline: projection.deadline.clone(),
            }),
            // Written from the three projection fields alone, so a markerless
            // block gets a row exactly as a marked one does (§3.2 M2).
            planning: crate::query::derived::planning_row(
                projection.priority.as_deref(),
                projection.scheduled.as_deref(),
                projection.deadline.as_deref(),
            ),
            // Filled once per page, after the whole flat block list exists.
            path_refs: Vec::new(),
            property_atoms,
        });
        lower_blocks(
            &block.children,
            page_id,
            Some(block_id),
            structural_path,
            out,
            reference_postings,
            refs_norm,
            parse_config,
            atom_format,
        )?;
        structural_path.pop();
    }
    Ok(())
}

fn append_reference_postings(
    out: &mut Vec<PhysicalReferencePosting>,
    page_id: [u8; 16],
    source: PhysicalEntityId,
    source_locator: &[u8],
    inline_names: impl IntoIterator<Item = String>,
    property_names: impl IntoIterator<Item = String>,
) -> Result<(), String> {
    let mut ordinal = 0_u32;
    for (kind, names) in [
        (0_i64, inline_names.into_iter().collect::<Vec<_>>()),
        (3_i64, property_names.into_iter().collect::<Vec<_>>()),
    ] {
        for raw_name in names {
            out.push(PhysicalReferencePosting {
                source_page_id: page_id,
                source_entity: source,
                source_locator: source_locator.to_vec(),
                ordinal,
                kind,
                target: PhysicalReferenceTarget::PageName {
                    normalized_name: crate::refs::page_key(&raw_name),
                    raw_name,
                    resolved_page_id: None,
                },
            });
            ordinal = ordinal
                .checked_add(1)
                .ok_or_else(|| "one reference source exceeds u32::MAX postings".to_string())?;
        }
    }
    Ok(())
}

fn facets(raw: &str, is_org: bool) -> (String, Vec<PhysicalProperty>, Vec<String>) {
    let mut block = DocBlock::new(raw);
    block.is_org = is_org;
    let searchable = block
        .visible_text()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let properties = block
        .projection()
        .properties
        .iter()
        .map(|(name, value)| PhysicalProperty {
            name: name.clone(),
            normalized_name: property_key_norm(name),
            value: value.clone(),
        })
        .collect();
    (searchable, properties, block.projection().tags.clone())
}

pub(crate) fn page_id(relative_path: &str) -> [u8; 16] {
    let mut digest = Sha256::new();
    digest.update(b"tine-direct-page-v1\0");
    digest.update(relative_path.as_bytes());
    let bytes = digest.finalize();
    let mut id = [0; 16];
    id.copy_from_slice(&bytes[..16]);
    id
}

fn page_kind_to_sql(kind: PageKind) -> i64 {
    match kind {
        PageKind::Page => 0,
        PageKind::Journal => 1,
    }
}

/// `pages.text_kind` back to the parser's `PageKind`. A value outside the two
/// the producer writes is projection damage, not a third kind, so every reader
/// treats `None` as a failed read (D-3).
pub(crate) fn page_kind_from_sql(kind: i64) -> Option<PageKind> {
    match kind {
        0 => Some(PageKind::Page),
        1 => Some(PageKind::Journal),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Graph;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    static PROJECTION_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tine-direct-projection-{tag}-{}", Uuid::new_v4()))
    }

    fn reset_lowerings(root: &Path) {
        *PHYSICAL_PAGE_LOWERINGS.lock().unwrap() = (Some(root.to_path_buf()), 0);
    }

    fn lowerings() -> u64 {
        PHYSICAL_PAGE_LOWERINGS.lock().unwrap().1
    }

    fn signature(groups: &[crate::model::RefGroup]) -> Vec<(String, Vec<(String, String)>)> {
        groups
            .iter()
            .map(|group| {
                (
                    group.page.clone(),
                    group
                        .blocks
                        .iter()
                        .map(|block| (block.id.clone(), block.raw.clone()))
                        .collect(),
                )
            })
            .collect()
    }

    /// Run `attempt` until it answers, retrying ONLY typed readiness.
    ///
    /// RET2's public Direct route answers from SQL or returns a typed error;
    /// `NotReady` is the one error a caller may retry, and this is the same
    /// signal `src/queryReadiness.ts` loops on. `Unavailable` and `Cancelled`
    /// fail the fixture immediately.
    fn when_ready<T>(
        mut attempt: impl FnMut() -> Result<T, crate::query::QueryExecutionError>,
    ) -> T {
        let started = Instant::now();
        loop {
            match attempt() {
                Ok(answer) => return answer,
                Err(crate::query::QueryExecutionError::NotReady(reason)) => {
                    assert!(
                        started.elapsed() < Duration::from_secs(15),
                        "the query index never became ready ({})",
                        reason.as_str()
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(other) => panic!("the public query route refused: {other}"),
            }
        }
    }

    fn wait_ready(graph: &Graph) {
        let started = Instant::now();
        while !graph.direct_projection_ready_test() {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "Direct Files projection did not converge: cache_generation={} {}",
                graph.cache_generation(),
                graph
                    .direct_projection_test()
                    .map(|projection| projection.debug_state_test())
                    .unwrap_or_else(|| "no projection".to_owned())
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn query_job_closes_snapshot_before_releasing_capacity() {
        let path = std::env::temp_dir().join(format!("tine-query-drop-{}.sqlite", Uuid::new_v4()));
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.busy_timeout(Duration::ZERO).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE payload(value TEXT); INSERT INTO payload VALUES ('before');").unwrap();
        let snapshot = PhysicalProjectionQuerySnapshot::open_direct(&path, || Ok(())).unwrap();
        let owner = crate::query_jobs::QueryJobOwner::new(1);
        let slot = match owner.acquire() {
            crate::query_jobs::Admission::Slot(slot) => slot,
            _ => panic!("query admission"),
        };
        assert!(slot.register(snapshot.cancellation()));
        let job = DirectQueryJob {
            snapshot,
            _slot: slot,
            session_pages: Arc::new(HashSet::new()),
        };
        writer
            .execute("UPDATE payload SET value='after'", [])
            .unwrap();
        let checkpoint = || {
            writer
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
        };
        assert_eq!(checkpoint(), 1, "fixture must retain a real WAL snapshot");
        let (releasing, resume) = owner.pause_next_release_for_test();
        let busy_at_release = std::thread::scope(|scope| {
            let dropper = scope.spawn(move || drop(job));
            releasing.recv_timeout(Duration::from_secs(3)).unwrap();
            let busy = checkpoint();
            // Resume before asserting: the old declaration order must fail,
            // not leave the scope waiting forever for its paused dropper.
            resume.send(()).unwrap();
            dropper.join().unwrap();
            busy
        });
        assert_eq!(owner.active(), 0);
        drop(writer);
        let _ = std::fs::remove_file(path);
        assert_eq!(
            busy_at_release, 0,
            "slot release must follow SQLite transaction release"
        );
    }

    #[test]
    fn direct_projection_matches_parser_tasks_and_tracks_replace_delete() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("task-parity");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::create_dir_all(root.join("journals")).unwrap();
        std::fs::write(
            root.join("pages/tasks.md"),
            "- TODO [#A] parent\n\t- TODO child\n- TODO other\n  SCHEDULED: <2026-08-13 Thu>\n",
        )
        .unwrap();
        std::fs::write(root.join("pages/org.org"), "* TODO [#B] org task\n").unwrap();

        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);

        for query in [
            "(task TODO)",
            "(and (task TODO) (priority A))",
            "(and (task TODO) (scheduled))",
            "(and (task TODO) (sort-by priority desc))",
        ] {
            let oracle = crate::query::run_query_bounded(&graph, query, 100, 1_000_000);
            let indexed = graph
                .run_query_bounded(query, 100, 1_000_000)
                .expect("the ready projection answers the public bounded route");
            assert_eq!(
                signature(&indexed.groups),
                signature(&oracle.groups),
                "{query}"
            );
            assert_eq!(
                (indexed.total, indexed.exceeded),
                (oracle.total, oracle.exceeded)
            );
        }
        // R3: every user query above was answered by the dispatched statement.
        // Three distinct pre-view shapes: `sort-by` is a view directive, so the
        // fourth query shares the first one's pre-view memo entry.
        assert_eq!(graph.direct_projection_fallback_reads_test(), 0);
        assert!(graph.direct_projection_statement_reads_test() >= 3);
        let statement_reads = graph.direct_projection_statement_reads_test();
        let repeated = graph
            .run_query_bounded("(task TODO)", 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        assert_eq!(
            signature(&repeated.groups),
            signature(
                &crate::query::run_query_bounded(&graph, "(task TODO)", 100, 1_000_000).groups
            )
        );
        assert_eq!(
            graph.direct_projection_statement_reads_test(),
            statement_reads,
            "the generation-keyed presentation memo must avoid repeated SQL/parser work"
        );

        let entry = graph
            .list_pages()
            .into_iter()
            .find(|entry| entry.name == "tasks")
            .unwrap();
        let mut page = graph.load_page(&entry).unwrap();
        let baseline = page.rev.clone();
        page.blocks[0].raw = "DONE [#A] parent".into();
        graph.save_page(&page, baseline.as_deref()).unwrap();
        wait_ready(&graph);
        for query in ["(task TODO)", "(task DONE)"] {
            let oracle = crate::query::run_query_bounded(&graph, query, 100, 1_000_000);
            let indexed = graph
                .run_query_bounded(query, 100, 1_000_000)
                .expect("the ready projection answers the public bounded route");
            assert_eq!(
                signature(&indexed.groups),
                signature(&oracle.groups),
                "{query}"
            );
        }

        graph.delete_page("org", PageKind::Page).unwrap();
        wait_ready(&graph);
        let oracle = crate::query::run_query_bounded(&graph, "(task TODO)", 100, 1_000_000);
        let indexed = graph
            .run_query_bounded("(task TODO)", 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        assert_eq!(signature(&indexed.groups), signature(&oracle.groups));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn b4_page_ref_and_property_facets_record_indexed_reads() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("b4-indexed-reads");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/source.md"),
            "category:: work\ntags:: work\n\n- TODO points to [[Target]]\n  status:: active\n",
        )
        .unwrap();
        std::fs::write(root.join("pages/target.md"), "- target\n").unwrap();
        std::fs::write(root.join("pages/Project___Child.md"), "- namespace child\n").unwrap();
        std::fs::create_dir_all(root.join("journals")).unwrap();
        std::fs::write(root.join("journals/2026_09_03.md"), "- journal block\n").unwrap();

        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);

        let indexed_before = graph.direct_projection_indexed_reads_test();
        let statements_before = graph.direct_projection_statement_reads_test();
        for query in [
            "(page-ref Target)",
            "(and (page-ref Target) \"points\")",
            "(and \"points\" (page-ref Target))",
        ] {
            let oracle = crate::query::run_query_bounded(&graph, query, 100, 1_000_000);
            let indexed = graph
                .run_query_bounded(query, 100, 1_000_000)
                .expect("the ready projection answers the public bounded route");
            assert_eq!(signature(&indexed.groups), signature(&oracle.groups));
            assert_eq!(
                (indexed.total, indexed.exceeded),
                (oracle.total, oracle.exceeded)
            );
        }
        assert_eq!(
            graph.property_facets(),
            crate::query::property_facets(&graph)
        );
        assert_eq!(
            graph.autocomplete_property_facets_bounded(100, 1_000_000),
            crate::query::autocomplete_property_facets_bounded(&graph, 100, 1_000_000)
        );
        // R3: a user query is answered by the dispatched statement from its own
        // read snapshot — it no longer passes through the indexed page reads.
        assert!(
            graph.direct_projection_statement_reads_test() >= statements_before + 3,
            "PageRef queries must be answered by the dispatched statement"
        );
        assert!(
            graph.direct_projection_indexed_reads_test() >= indexed_before + 2,
            "both property-facet entry points must use the generation-bound SQLite read"
        );

        // **SPEC §5.9's ready shape.** When the projection is ready and the
        // statement lowers, the STATEMENT answers: exactly one dispatched read,
        // no walk, no fallback — and the same answer the walk gives, including
        // `total` and `exceeded`. This replaces the candidate-plan route, which
        // selected a page SUPERSET and then walked it; the statement selects the
        // answer. There is no cost test in front of this and no fourth route:
        // `(journal)` and `"points"` below are deliberately in the list because
        // one is unselective and the other is an unbounded content predicate,
        // and §5.9 routes both to the statement anyway.
        for query in [
            "(and (task TODO) (page source))",
            "(property status active)",
            "(page-property category work)",
            "(page source)",
            "(namespace Project)",
            "(journal)",
            "(and (property status active) (page source))",
            "(or (page source) (page Target))",
            "\"points\"",
        ] {
            let oracle = crate::query::run_query_bounded(&graph, query, 100, 1_000_000);
            let statements_before = graph.direct_projection_statement_reads_test();
            let fallback_before = graph.direct_projection_fallback_reads_test();
            graph.reset_direct_projection_candidate_probe_test();
            let actual = graph
                .run_query_bounded(query, 100, 1_000_000)
                .expect("the ready projection answers the public bounded route");
            assert_eq!(
                signature(&actual.groups),
                signature(&oracle.groups),
                "{query}"
            );
            assert_eq!(
                (actual.total, actual.exceeded),
                (oracle.total, oracle.exceeded),
                "{query}"
            );
            assert_eq!(
                graph.direct_projection_statement_reads_test(),
                statements_before + 1,
                "{query}: exactly one dispatched statement must answer"
            );
            assert_eq!(
                crate::query::full_graph_query_evaluations(),
                0,
                "{query}: production invocation entered the forbidden full-graph evaluator"
            );
            assert_eq!(
                graph.direct_projection_fallback_reads_test(),
                fallback_before,
                "{query}: ready dispatch fell back"
            );
        }

        let statements_before = graph.direct_projection_statement_reads_test();
        let fallback_before = graph.direct_projection_fallback_reads_test();
        graph.reset_direct_projection_candidate_probe_test();
        let empty = graph
            .run_query_bounded("(", 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        assert!(empty.groups.is_empty());
        assert_eq!(
            crate::query::full_graph_query_evaluations(),
            0,
            "a refused source must not enter the graph evaluator"
        );
        assert_eq!(
            graph.direct_projection_statement_reads_test(),
            statements_before,
            "a refused source must not run a statement"
        );
        assert_eq!(
            graph.direct_projection_fallback_reads_test(),
            fallback_before,
            "a refused source must not record fallback access"
        );

        // Staleness splits the two families RET2 deliberately treats
        // differently. The property-facet read is NOT a public query and keeps
        // its parser fallback; the public query route has no fallback left and
        // owes readiness instead.
        let fallback_query = "(and (page-ref Target) (not (page Missing)))";
        let oracle = crate::query::run_query_bounded(&graph, fallback_query, 100, 1_000_000);
        assert!(
            oracle.total > 0,
            "the stale-state query must have a real answer"
        );

        let fallback_before = graph.direct_projection_fallback_reads_test();
        graph.direct_projection_mark_stale_test();
        assert_eq!(
            graph.property_facets(),
            crate::query::property_facets(&graph)
        );
        assert!(
            graph.direct_projection_fallback_reads_test() >= fallback_before + 1,
            "a stale facet read must still record a parser fallback"
        );

        let fallback_before = graph.direct_projection_fallback_reads_test();
        let walks_before = crate::query::full_graph_query_evaluations();
        // The worker may already have caught up, so accept either verdict --
        // but ONLY the two the route is allowed to give.
        match graph.run_query_bounded(fallback_query, 100, 1_000_000) {
            Ok(_) => assert!(graph.direct_projection_ready_test()),
            Err(crate::query::QueryExecutionError::NotReady(_)) => {}
            other => panic!("a stale projection owes readiness, got {other:?}"),
        }
        let fallback = when_ready(|| graph.run_query_bounded(fallback_query, 100, 1_000_000));
        assert_eq!(signature(&fallback.groups), signature(&oracle.groups));
        assert_eq!(
            crate::query::full_graph_query_evaluations(),
            walks_before,
            "a stale public query must not traverse the graph"
        );
        assert_eq!(
            graph.direct_projection_fallback_reads_test(),
            fallback_before,
            "the public query route has no fallback left to record"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_damaged_query_table_is_rebuilt_without_a_source_edit() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("damaged-query-table");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/source.md"), "- TODO links [[Target]]\n").unwrap();
        let path = root.join("private/projection.sqlite");
        let graph = Graph::open(&root);
        graph.attach_direct_projection(path.clone()).unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        // Persistently unavailable projection data, unlike a one-shot seam
        // error on a healthy file. Recovery must actually rebuild the cache.
        let damaged = rusqlite::Connection::open(&path).unwrap();
        damaged.execute("DROP TABLE block_path_refs", []).unwrap();
        drop(damaged);
        let query = "(page-ref Target)";
        let oracle = crate::query::run_query_bounded(&graph, query, 100, 1_000_000);
        // RET2: a damaged table is a failed read, so the route repairs once and
        // retries the SAME statement. The repair is asynchronous, so the public
        // answer may be `NotReady(Recovering)` until the rebuild lands — which
        // is the signal the frontend retries on, and never a walked answer.
        let answer = when_ready(|| graph.run_query_bounded(query, 100, 1_000_000));
        assert_eq!(signature(&answer.groups), signature(&oracle.groups));
        wait_ready(&graph);
        let statements_before = graph.direct_projection_statement_reads_test();
        // A different memo key must reach the repaired SQL table.
        let next = "(and (page-ref Target) (task TODO))";
        let oracle = crate::query::run_query_bounded(&graph, next, 100, 1_000_000);
        let answer = graph
            .run_query_bounded(next, 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        assert_eq!(signature(&answer.groups), signature(&oracle.groups));
        assert_eq!(
            graph.direct_projection_statement_reads_test(),
            statements_before + 1
        );
        drop(graph);
        let _ = std::fs::remove_dir_all(root);
    }

    /// R3 (§2B, D-3): a rebuild under a LIVE query job. The worker must
    /// interrupt and drain every owned snapshot before it resets the
    /// disposable file, and a job admitted before the drain can neither run
    /// its statement nor outlive it. In-scope scenario: a torn projection
    /// rebuilt while a query is reading it. Also pins the admission answers a
    /// dispatch maps: a stale generation is `NotReady` and an unopenable file
    /// is `Failed`, and neither consumes a slot.
    #[test]
    fn a_rebuild_drains_a_live_query_job_before_touching_the_file() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("rebuild-drains-job");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/source.md"), "- TODO links [[Target]]\n").unwrap();
        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        let projection = graph.direct_projection_test().unwrap();
        let generation = graph.cache_generation();

        assert!(matches!(
            projection.open_query_job(generation + 1),
            QueryJobOpen::NotReady
        ));
        assert_eq!(projection.active_query_jobs_test(), 0);
        projection.inject_next_statement_failure();
        assert!(matches!(
            projection.open_query_job(generation),
            QueryJobOpen::Failed
        ));
        assert_eq!(projection.active_query_jobs_test(), 0);

        let QueryJobOpen::Job(mut job) = projection.open_query_job(generation) else {
            panic!("a ready projection admits a job at its generation");
        };
        assert_eq!(projection.active_query_jobs_test(), 1);
        assert!(!job.is_cancelled());
        // R6: the cold open streamed a fresh parse (structural ids), so the
        // page holds no live-id claim; only a live save adds one.
        assert!(
            !job.session_pages.contains(&page_id("pages/source.md")),
            "a streamed structural lowering claims no live ids"
        );
        let mut rows = 0usize;
        job.snapshot
            .visit_projection_query("SELECT block_id FROM blocks", &[], |_| {
                rows += 1;
                Ok(std::ops::ControlFlow::Continue(()))
            })
            .unwrap();
        assert_eq!(rows, 1, "the owned snapshot reads the projection");

        // Streaming repair waits for reset before producing replacement pages.
        // Hold the reader on this thread while a separate caller requests
        // recovery, as in production (a failed query releases its own job
        // before requesting repair). Otherwise this fixture waits on itself.
        std::thread::scope(|scope| {
            let repair = scope.spawn(|| graph.direct_projection_recover_after_failed_read_test());
            let started = Instant::now();
            while !job.is_cancelled() {
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "the rebuild must cancel the live job"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            // The worker waits for the drain: the slot is still held, the
            // projection is not ready, and nothing has been able to reset the file
            // under the pinned read transaction.
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(projection.active_query_jobs_test(), 1);
            assert!(!graph.direct_projection_ready_test());
            let interrupted = job.snapshot.visit_projection_query(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 200000) \
             SELECT count(*) FROM c",
                &[],
                |_| Ok(std::ops::ControlFlow::Continue(())),
            );
            assert!(
                interrupted.is_err(),
                "a cancelled snapshot cannot run a statement"
            );
            assert!(matches!(
                projection.open_query_job(generation),
                QueryJobOpen::NotReady
            ));

            drop(job);
            repair.join().unwrap();
        });
        wait_ready(&graph);
        assert_eq!(projection.active_query_jobs_test(), 0);
        let generation = graph.cache_generation();
        assert!(matches!(
            projection.open_query_job(generation),
            QueryJobOpen::Job(_)
        ));
        drop(projection);
        drop(graph);
        let _ = std::fs::remove_dir_all(root);
    }

    /// R3 identity policy: the projection names exactly the pages THIS process
    /// lowered — a full snapshot's replacements, each live delta's page, minus
    /// deletions — and a warm reopen that reuses the file (R1) starts from the
    /// empty set, because those rows' stored ids came from an earlier session
    /// and must be answered structurally.
    #[test]
    fn lowering_measurement_excludes_other_graphs() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("measurement-owner");
        let other = scratch("measurement-other");
        reset_lowerings(&root);
        for graph_root in [&root, &other] {
            std::fs::create_dir_all(graph_root.join("pages")).unwrap();
            std::fs::write(graph_root.join("pages/one.md"), "- TODO one\n").unwrap();
            let graph = Graph::open(graph_root);
            graph
                .attach_direct_projection(graph_root.join("projection.sqlite"))
                .unwrap();
            graph.warm_cache();
            wait_ready(&graph);
        }
        assert_eq!(lowerings(), 1, "only the measured graph contributes");
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(other);
    }

    #[test]
    fn session_pages_name_exactly_the_pages_this_process_lowered() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("session-pages");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/one.md"), "- TODO one\n").unwrap();
        std::fs::write(root.join("pages/two.md"), "- DONE two\n").unwrap();
        let database = scratch("session-pages-db").join("projection.sqlite");
        let ids = |graph: &Graph, names: &[&str]| -> HashSet<[u8; 16]> {
            graph
                .list_pages()
                .into_iter()
                .filter(|entry| names.contains(&entry.name.as_str()))
                .map(|entry| page_id(&entry.rel_path))
                .collect()
        };

        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
            let projection = graph.direct_projection_test().unwrap();
            // R6: a cold open streams fresh parses (structural ids), so no
            // page holds a live-id claim yet.
            assert!(projection.session_pages_test().is_empty());

            let two = ids(&graph, &["two"]);
            graph.delete_page("two", PageKind::Page).unwrap();
            wait_ready(&graph);
            let after_delete = projection.session_pages_test();
            assert!(after_delete.is_empty());
            assert!(after_delete.is_disjoint(&two));

            let entry = graph
                .list_pages()
                .into_iter()
                .find(|entry| entry.name == "one")
                .unwrap();
            let mut page = graph.load_page(&entry).unwrap();
            let baseline = page.rev.clone();
            page.blocks[0].raw = "DONE one".into();
            graph.save_page(&page, baseline.as_deref()).unwrap();
            wait_ready(&graph);
            assert_eq!(*projection.session_pages_test(), ids(&graph, &["one"]));
        }
        std::thread::sleep(Duration::from_millis(20));

        std::fs::write(root.join("pages/two.md"), "- DONE two again\n").unwrap();
        reset_lowerings(&root);
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
            assert_eq!(lowerings(), 1, "only the externally written page relowers");
            let projection = graph.direct_projection_test().unwrap();
            // R6: the relowering is a warm-stream parse — structural ids, so
            // the page holds no live-id claim either (the derived id and the
            // stored id coincide). Only a live save adds a page.
            assert!(
                projection.session_pages_test().is_empty(),
                "a reused row keeps an earlier session's id and a structural relowering claims none"
            );
        }
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    /// **RET2's cancellation shape.** A cancelled snapshot means the caller's
    /// own work was withdrawn — the graph is closing, or a drain superseded the
    /// request. It is the ONE §5.9 state that must NOT be repaired and must NOT
    /// be retried: repairing would schedule a rebuild nobody asked for, and
    /// retrying would race the very drain that cancelled the first attempt.
    ///
    /// The fixture matches on the typed error rather than on a bare `is_err`,
    /// because `Cancelled` and `Unavailable` differ in exactly the way the
    /// frontend acts on: one is silent, the other is shown.
    #[test]
    fn a_cancelled_query_job_refuses_without_repairing_or_walking() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("cancelled-query-job");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/tasks.md"),
            "- TODO ship it\n  status:: active\n",
        )
        .unwrap();

        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);

        // The projection answers a DIFFERENT query first, so the route is known
        // to be healthy going in. It has to be a different one: a memoized
        // answer is returned without ever opening a query job, so reusing this
        // source below would test the memo and not the cancellation.
        let warm =
            when_ready(|| graph.run_query_bounded("(property status active)", 100, 1_000_000));
        assert!(warm.total > 0, "the warm-up query must match something");

        // Non-vacuity for the cancelled query itself, from the independent
        // oracle: the refusal below is not a correct empty answer.
        let oracle = crate::query::run_query_bounded(&graph, "(task TODO)", 100, 1_000_000);
        assert!(
            oracle.total > 0,
            "the cancelled query must have a real answer"
        );

        let projection = graph.direct_projection_test().expect("a projection");
        let generation_before = graph.cache_generation();
        let walks_before = crate::query::full_graph_query_evaluations();
        projection.close_query_jobs_test();

        let refused = graph.run_query_bounded("(task TODO)", 100, 1_000_000);
        assert!(
            matches!(refused, Err(crate::query::QueryExecutionError::Cancelled)),
            "a cancelled job is reported as cancelled, not repaired away: {refused:?}"
        );
        assert_eq!(
            crate::query::full_graph_query_evaluations(),
            walks_before,
            "a cancelled query must not traverse the graph"
        );
        // A repair would have marked the projection stale and enqueued a full
        // rebuild. Readiness at the same generation is what proves neither
        // happened.
        assert_eq!(graph.cache_generation(), generation_before);
        assert!(
            graph.direct_projection_ready_test(),
            "cancellation must not schedule a rebuild of a healthy projection"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    /// **RET2's readiness shape.** A projection that is genuinely behind the
    /// parsed cache answers `NotReady`, carrying the reason the queue is in —
    /// never a walked answer and never a false empty one. This is the signal
    /// `src/queryReadiness.ts` retries on, so getting the CLASS wrong (a
    /// terminal `Unavailable` for a transient lag) is a user-visible defect.
    #[test]
    fn a_projection_behind_the_parsed_cache_reports_readiness_not_an_answer() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("behind-parsed-cache");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/tasks.md"), "- TODO ship it\n").unwrap();

        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);

        // Non-vacuity: there IS a matching TODO, and the ready projection finds
        // it, so an unready refusal below is not a correct empty answer.
        let answered = when_ready(|| graph.run_query_bounded("(task TODO)", 100, 1_000_000));
        assert!(
            answered.total > 0,
            "the fixture must have something to match"
        );

        // Move the parsed cache ahead of the projection WITHOUT waiting: the
        // save bumps the cache generation and the worker has not caught up.
        let walks_before = crate::query::full_graph_query_evaluations();
        let mut page = graph.load_named("tasks", PageKind::Page).unwrap().unwrap();
        page.blocks[0].raw = "TODO ship it soon".into();
        graph.save_page(&page, page.rev.as_deref()).unwrap();

        // The race is real, so accept either verdict — but ONLY the two the
        // route is allowed to give: the projection had already caught up, or it
        // says so in words the frontend retries on.
        match graph.run_query_bounded("(task TODO)", 100, 1_000_000) {
            Ok(answer) => assert!(
                graph.direct_projection_ready_test(),
                "an answer may only come from a caught-up projection: {}",
                answer.total
            ),
            Err(crate::query::QueryExecutionError::NotReady(_)) => {}
            other => panic!("a projection behind the cache owes readiness, got {other:?}"),
        }
        assert_eq!(
            crate::query::full_graph_query_evaluations(),
            walks_before,
            "an unready projection must not traverse the graph"
        );

        // And readiness is transient by construction: the same query answers
        // once the worker converges, with the edited row.
        let after = when_ready(|| graph.run_query_bounded("(task TODO)", 100, 1_000_000));
        assert_eq!(after.total, 1);
        assert_eq!(after.groups[0].blocks[0].raw, "TODO ship it soon");

        let _ = std::fs::remove_dir_all(root);
    }

    /// **SPEC §5.9's failed-read shape, RET2.** A read that was ATTEMPTED and
    /// did not answer owes a repair and a RETRY OF THE SAME STATEMENT — not a
    /// walk. The walk is gone from the public route, so the obligation the old
    /// shape discharged by falling back is now discharged by
    /// `direct_projection_recover_after_failed_read` followed by a second SQL
    /// attempt inside the same public call.
    ///
    /// **In-scope scenario** (AGENTS §5): a torn or truncated projection file
    /// after a crash or power loss, a disk error, or a projection whose page set
    /// has drifted from the parsed cache. The projection is disposable derived
    /// state (D-3), so the answer is still recovery and not refusal — the user
    /// gets the RIGHT rows, from SQL, without a user edit and without the graph
    /// ever being traversed.
    #[test]
    fn a_failed_statement_read_repairs_and_retries_the_same_statement() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("failed-read-recovers");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/source.md"),
            "- TODO points to [[Target]]\n  status:: active\n",
        )
        .unwrap();
        std::fs::write(root.join("pages/target.md"), "- target\n").unwrap();

        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);

        let query = "(page-ref Target)";
        let oracle = crate::query::run_query_bounded(&graph, query, 100, 1_000_000);
        assert!(oracle.total > 0, "the fixture must have something to match");
        // The counter that used to prove the failed read: the query route has
        // no fallback left to count, so it must NOT move here. It still counts
        // the property-facet fallback, which RET2 did not touch.
        let fallbacks_before = graph.direct_projection_fallback_reads_test();
        graph.reset_direct_projection_candidate_probe_test();
        graph.direct_projection_inject_read_failure_test();

        let walks_before = crate::query::full_graph_query_evaluations();
        let statements_before_failure = graph.direct_projection_statement_reads_test();

        let answered = when_ready(|| graph.run_query_bounded(query, 100, 1_000_000));
        assert_eq!(
            signature(&answered.groups),
            signature(&oracle.groups),
            "a failed read must be answered by a repaired SQL retry, not refused"
        );
        assert_eq!(
            crate::query::full_graph_query_evaluations(),
            walks_before,
            "the public route never walks the graph, not even to survive a failed read"
        );
        assert_eq!(
            graph.direct_projection_fallback_reads_test(),
            fallbacks_before,
            "the query route has no fallback left to take"
        );
        assert!(
            graph.direct_projection_statement_reads_test() > statements_before_failure,
            "the retry must go through the statement seam again"
        );

        // The recovery obligation: `mark_stale` alone would only clear `ready`
        // and strand the projection. The full-snapshot enqueue is scheduled from
        // the already-parsed cache, so it needs no reparse, no disk read, and no
        // user action — `ready` comes back on its own.
        wait_ready(&graph);
        // A DIFFERENT query, because the first one's answer is now in the
        // derived cache under its IR key and would be served without touching
        // the statement seam at all.
        let after = "(property status active)";
        let after_oracle = crate::query::run_query_bounded(&graph, after, 100, 1_000_000);
        let statements_before = graph.direct_projection_statement_reads_test();
        let fallbacks_before = graph.direct_projection_fallback_reads_test();
        let recovered = when_ready(|| graph.run_query_bounded(after, 100, 1_000_000));
        assert_eq!(
            signature(&recovered.groups),
            signature(&after_oracle.groups)
        );
        assert_eq!(
            graph.direct_projection_statement_reads_test(),
            statements_before + 1,
            "the recovered projection must answer through the statement again"
        );
        assert_eq!(
            graph.direct_projection_fallback_reads_test(),
            fallbacks_before,
            "the recovered projection must not fall back"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    /// **SPEC §5.3's base order and hydration, together.**
    ///
    /// The selection relation is unordered; its descriptor wrapper orders by
    /// persisted page position and block preorder. `signature` compares the
    /// ordered page list and each page's ordered block list, so a set-equivalent
    /// result with changed traversal order fails this gate.
    ///
    /// Two visible-order paths are covered because they are different paths and
    /// a feed-only repro misses real bugs: a routed NAMED page (nested blocks,
    /// document order within the page) and the JOURNAL feed (kind rank, journal
    /// before page at the same display name).
    ///
    /// Result payload comes from SQLite for admitted rows. No query result page
    /// is loaded as a `Document` (I-13, I-15).
    #[test]
    fn the_dispatched_result_reproduces_the_walks_order_and_loads_only_result_pages() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("dispatch-order");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::create_dir_all(root.join("journals")).unwrap();
        // A routed named page with NESTED matches, so within-page document order
        // is observable: `tree/filter-top-level-blocks` keeps the outer match and
        // the grandchild, and the ordered comparison sees which comes first.
        std::fs::write(
            root.join("pages/Alpha.md"),
            "- TODO alpha one\n\t- plain middle\n\t\t- TODO alpha three\n- TODO alpha four\n",
        )
        .unwrap();
        std::fs::write(root.join("pages/Beta.md"), "- TODO beta one\n").unwrap();
        // Never matches: it must not be hydrated.
        std::fs::write(root.join("pages/Gamma.md"), "- ordinary prose\n").unwrap();
        std::fs::write(
            root.join("journals/2026_06_28.md"),
            "- TODO journal one\n- TODO journal two\n",
        )
        .unwrap();
        std::fs::write(
            root.join("journals/2026_06_29.md"),
            "- TODO journal three\n",
        )
        .unwrap();

        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);

        let feed = "(task TODO)";
        let oracle = crate::query::run_query_bounded(&graph, feed, 100, 1_000_000);
        graph.reset_direct_projection_candidate_probe_test();
        let dispatched = graph
            .run_query_bounded(feed, 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        assert_eq!(
            signature(&dispatched.groups),
            signature(&oracle.groups),
            "the journal feed must match the walk INCLUDING order"
        );
        // The fixture has to be able to fail: more than one page, and a page with
        // more than one block, or the ordered comparison proves nothing.
        assert!(
            dispatched.groups.len() >= 4,
            "fixture must span several pages: {:?}",
            dispatched
                .groups
                .iter()
                .map(|g| &g.page)
                .collect::<Vec<_>>()
        );
        assert!(
            dispatched.groups.iter().any(|g| g.blocks.len() > 1),
            "fixture must have a page with several ordered matches"
        );
        assert_eq!(
            crate::query::full_graph_query_evaluations(),
            0,
            "the feed must not enter the whole-graph evaluator"
        );
        // R3 (I-13, I-15): the answer is constructed from the projection alone.
        // NO parsed document is loaded for a dispatched query — not the result
        // pages, and not `Gamma`, which matches nothing.
        let hydrated = graph.direct_projection_hydrated_pages_test();
        assert!(
            hydrated.is_empty(),
            "a dispatched query loads no page document: {hydrated:?}"
        );

        // The routed named-page path: the same query scoped to one page, whose
        // within-page order is document order and not any projection column.
        let routed = "(and (task TODO) (page Alpha))";
        let routed_oracle = crate::query::run_query_bounded(&graph, routed, 100, 1_000_000);
        graph.reset_direct_projection_candidate_probe_test();
        let routed_dispatched = graph
            .run_query_bounded(routed, 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        assert_eq!(
            signature(&routed_dispatched.groups),
            signature(&routed_oracle.groups),
            "a routed named page must match the walk INCLUDING order"
        );
        assert_eq!(
            routed_dispatched.groups.len(),
            1,
            "the routed query names exactly one page"
        );
        assert_eq!(
            routed_dispatched.groups[0].blocks.len(),
            3,
            "Alpha contributes the outer match, its grandchild and its sibling, \
             in document order"
        );
        assert!(
            graph.direct_projection_hydrated_pages_test().is_empty(),
            "a routed one-page result loads no page document either"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    /// **SPEC §5.9: an unselective shape takes the statement too — there is no
    /// fourth route.**
    ///
    /// This fixture exists because of the route it USED to prove. The candidate
    /// plan materialized a page SUPERSET and then walked it, which on `(journal)`
    /// meant materializing most of the graph and running 7–10× slower than the
    /// walk; a candidate-count hatch abandoned the projection on exactly that
    /// shape. §5.9 removes the reason for the hatch rather than the hatch's
    /// symptom: the statement selects the ANSWER, so an unselective shape costs
    /// what its answer costs and there is nothing to abandon.
    ///
    /// The obligation the hatch protected is kept as an assertion, not as a
    /// route: on the unselective shape the dispatched path must load exactly the
    /// RESULT's pages and must not enter the whole-graph evaluator. The hatch
    /// itself is still alive for Managed Storage's candidate route and goes with
    /// it (P1-e).
    #[test]
    fn an_unselective_shape_answers_through_the_statement_without_a_candidate_superset() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("b4-candidate-cutoff");
        std::fs::create_dir_all(root.join("journals")).unwrap();
        // 50 real journal dates, comfortably past the 32-page small-graph
        // floor. Two months, because a date that does not exist (2026-09-31)
        // is not a journal and would not become a candidate.
        for (month, days) in [(9, 30), (10, 20)] {
            for day in 1..=days {
                std::fs::write(
                    root.join(format!("journals/2026_{month:02}_{day:02}.md")),
                    "- journal block\n",
                )
                .unwrap();
            }
        }
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/source.md"),
            "- TODO points to [[Target]]\n  status:: active\n",
        )
        .unwrap();
        std::fs::write(root.join("pages/target.md"), "- target\n").unwrap();

        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);

        // The shape the retired hatch existed for: `(journal)` matches most of
        // the graph. The fixture is pinned through the ANSWER's own page count
        // so a fixture that stopped being unselective fails here rather than
        // silently testing nothing — the candidate lowering that used to pin it
        // is gone along with the hatch and the walk it fell back to.
        let unselective = "(journal)";

        // Run the oracle BEFORE resetting the probes, so the oracle's own walk
        // is not counted as the production invocation's route evidence.
        let oracle = crate::query::run_query_bounded(&graph, unselective, 500, 4_000_000);
        assert!(
            oracle.groups.len() > 32,
            "fixture must exceed the old cutoff; got {} pages",
            oracle.groups.len()
        );
        let statements_before = graph.direct_projection_statement_reads_test();
        let fallback_before = graph.direct_projection_fallback_reads_test();
        graph.reset_direct_projection_candidate_probe_test();
        let dispatched = graph
            .run_query_bounded(unselective, 500, 4_000_000)
            .expect("the ready projection answers the public bounded route");

        assert_eq!(
            signature(&dispatched.groups),
            signature(&oracle.groups),
            "the statement must answer an unselective shape identically"
        );
        assert_eq!(
            (dispatched.total, dispatched.exceeded),
            (oracle.total, oracle.exceeded),
            "the statement must reproduce the walk's bound outcome"
        );
        assert_eq!(
            graph.direct_projection_statement_reads_test(),
            statements_before + 1,
            "an unselective shape is still answered by exactly one statement"
        );
        assert_eq!(
            graph.direct_projection_fallback_reads_test(),
            fallback_before,
            "a ready projection must not fall back on an unselective shape"
        );
        assert_eq!(
            crate::query::full_graph_query_evaluations(),
            0,
            "an unselective shape must not enter the whole-graph evaluator"
        );
        // **The I-13/I-15 obligation the hatch used to buy with a route.** The
        // dispatched path loads exactly the pages the RESULT names — here every
        // journal, because every journal matches — and never a superset. The
        // number that mattered was "pages materialized that the answer does not
        // contain", and it is zero by construction now.
        assert_eq!(
            dispatched.groups.len(),
            oracle.groups.len(),
            "the dispatched result must name the walk's pages"
        );

        // Same graph, same readiness: a selective shape is the same one route.
        let selective = "(page-ref Target)";
        let selective_oracle = crate::query::run_query_bounded(&graph, selective, 500, 4_000_000);
        let statements_before = graph.direct_projection_statement_reads_test();
        let fallback_before = graph.direct_projection_fallback_reads_test();
        graph.reset_direct_projection_candidate_probe_test();
        let routed = graph
            .run_query_bounded(selective, 500, 4_000_000)
            .expect("the ready projection answers the public bounded route");
        assert_eq!(
            signature(&routed.groups),
            signature(&selective_oracle.groups)
        );
        assert_eq!(
            graph.direct_projection_statement_reads_test(),
            statements_before + 1,
            "a selective shape is answered by exactly one statement"
        );
        assert_eq!(
            graph.direct_projection_fallback_reads_test(),
            fallback_before,
            "a selective shape must not fall back"
        );
        assert_eq!(
            crate::query::full_graph_query_evaluations(),
            0,
            "a selective shape must not enter the full-graph evaluator"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "manual B4 corpus gate; set TINE_B4_QUERY_CORPUS"]
    fn b4_corpus_page_ref_and_facets_match_oracle_with_route_evidence() {
        fn copy_tree(source: &Path, target: &Path) {
            std::fs::create_dir_all(target).unwrap();
            for entry in std::fs::read_dir(source).unwrap() {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                let destination = target.join(entry.file_name());
                if kind.is_dir() {
                    copy_tree(&entry.path(), &destination);
                } else if kind.is_file() {
                    std::fs::copy(entry.path(), destination).unwrap();
                }
            }
        }

        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let source = PathBuf::from(
            std::env::var("TINE_B4_QUERY_CORPUS").expect("TINE_B4_QUERY_CORPUS is required"),
        );
        let root = scratch("b4-corpus");
        if source.is_dir() {
            copy_tree(&source, &root);
        } else {
            std::fs::create_dir_all(root.join("pages")).unwrap();
            std::fs::copy(&source, root.join("pages/corpus-fixture.md")).unwrap();
        }
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/B4 Indexed Source.md"),
            "b4-page-facet:: yes\ntags:: b4-tag\n\n- TODO synthetic [[B4 Indexed Target]]\n  b4-facet:: yes\n",
        )
        .unwrap();
        std::fs::write(
            root.join("pages/B4___Namespace.md"),
            "- synthetic namespace\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("journals")).unwrap();
        std::fs::write(root.join("journals/2026_09_03.md"), "- synthetic journal\n").unwrap();
        std::fs::write(
            root.join("pages/B4 Indexed Target.md"),
            "- synthetic target\n",
        )
        .unwrap();

        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join(".b4-private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);

        // RET2 deleted the candidate-count escape hatch and the parser walk it
        // handed the query back to, so there are no longer two sides to assert.
        // What the real corpus still proves, and no synthetic fixture does at
        // scale, is that EVERY shape below — including `(journal)`, whose
        // candidate set is the size of the journal directory — is answered by
        // one statement, equals the parser oracle row for row, and never
        // reaches the full-graph evaluator.
        let graph_page_count = graph.with_pages(|pages| pages.len());
        let mut answered = 0usize;
        for query in [
            "(page-ref \"B4 Indexed Target\")",
            "(and (task TODO) (page \"B4 Indexed Source\"))",
            "(property b4-facet yes)",
            "(page-property b4-page-facet yes)",
            "(page \"B4 Indexed Source\")",
            "(namespace B4)",
            "(journal)",
            "(and (property b4-facet yes) (page \"B4 Indexed Source\"))",
            "(or (page \"B4 Indexed Source\") (page \"B4 Indexed Target\"))",
        ] {
            let oracle = crate::query::run_query_bounded(&graph, query, 20_000, 32 * 1024 * 1024);
            let statements_before = graph.direct_projection_statement_reads_test();
            let fallback_before = graph.direct_projection_fallback_reads_test();
            graph.reset_direct_projection_candidate_probe_test();
            let indexed = when_ready(|| graph.run_query_bounded(query, 20_000, 32 * 1024 * 1024));
            assert_eq!(
                signature(&indexed.groups),
                signature(&oracle.groups),
                "{query}: the answer must equal the parser oracle"
            );
            assert_eq!(
                (indexed.total, indexed.exceeded),
                (oracle.total, oracle.exceeded)
            );
            answered += 1;
            assert_eq!(
                graph.direct_projection_statement_reads_test(),
                statements_before + 1,
                "{query}: exactly one statement must answer"
            );
            assert_eq!(
                graph.direct_projection_fallback_reads_test(),
                fallback_before,
                "{query}: the query route has no fallback left to take"
            );
            assert_eq!(
                crate::query::full_graph_query_evaluations(),
                0,
                "{query}: the public route must not enter the full-graph evaluator"
            );
            assert!(
                graph.direct_projection_hydrated_pages_test().len() <= indexed.groups.len(),
                "{query}: a dispatched query hydrates only the pages its result names"
            );
        }
        // Non-vacuity: a corpus that answered nothing proves nothing.
        assert_eq!(
            answered, 9,
            "every shape must have been answered (pages={graph_page_count})"
        );
        assert!(
            graph.property_facets() == crate::query::property_facets(&graph),
            "corpus query-builder facets differ from the parser oracle"
        );
        assert!(
            graph.autocomplete_property_facets_bounded(20_000, 32 * 1024 * 1024)
                == crate::query::autocomplete_property_facets_bounded(
                    &graph,
                    20_000,
                    32 * 1024 * 1024,
                ),
            "corpus autocomplete facets differ from the parser oracle"
        );
        // A stale projection owes readiness, not a walked answer; the worker
        // catches up on its own and the same statement then answers.
        let fallback_before = graph.direct_projection_fallback_reads_test();
        let walks_before = crate::query::full_graph_query_evaluations();
        graph.direct_projection_mark_stale_test();
        let stale_query = "(and (page-ref \"B4 Indexed Target\") \"synthetic\")";
        let oracle = crate::query::run_query_bounded(&graph, stale_query, 20_000, 32 * 1024 * 1024);
        let recovered =
            when_ready(|| graph.run_query_bounded(stale_query, 20_000, 32 * 1024 * 1024));
        assert!(
            signature(&recovered.groups) == signature(&oracle.groups),
            "corpus stale recovery differs from the parser oracle"
        );
        assert_eq!(
            crate::query::full_graph_query_evaluations(),
            walks_before,
            "a stale public query must not traverse the graph"
        );
        assert_eq!(
            graph.direct_projection_fallback_reads_test(),
            fallback_before,
            "the query route has no fallback left to record"
        );

        let pages = graph.with_pages(|pages| pages.len());
        println!(
            "b4_corpus_gate pages={pages} indexed_reads={} fallback_reads={}",
            graph.direct_projection_indexed_reads_test(),
            graph.direct_projection_fallback_reads_test()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn direct_projection_matches_fuzzy_search_and_virtual_reference_names() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("search-reference-parity");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/one.md"),
            "tags:: Page Tag, [[Property Page]]\nalias:: Alias Page\nquoted:: untouched\n\n- Characteristically useful [[Inline Page]]\n  aliases:: #Block Alias\n- c% literal\n",
        )
        .unwrap();
        std::fs::write(root.join("pages/two.md"), "- unrelated content\n").unwrap();
        let graph = Graph::open(&root);
        graph.warm_cache();
        let oracle = crate::query::search(&graph, "cly", 20);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        wait_ready(&graph);

        let candidate_pages = graph
            .direct_projection_fuzzy_candidate_pages("cly")
            .unwrap();
        assert_eq!(candidate_pages.len(), 1);
        assert_eq!(candidate_pages[0].0.rel_path, "pages/one.md");
        assert_eq!(signature(&graph.search("cly", 20)), signature(&oracle));
        assert!(graph.direct_projection_fuzzy_candidate_reads_test() > 0);
        let names = graph
            .referenced_page_names()
            .into_iter()
            .map(|name| crate::refs::page_key(&name))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            names,
            [
                "page tag",
                "property page",
                "alias page",
                "inline page",
                "block",
                "block alias",
            ]
            .into_iter()
            .map(str::to_string)
            .collect()
        );
        assert!(graph.direct_projection_referenced_name_reads_test() > 0);

        let fuzzy_reads = graph.direct_projection_fuzzy_candidate_reads_test();
        let name_reads = graph.direct_projection_referenced_name_reads_test();
        graph.direct_projection_mark_stale_test();
        assert_eq!(signature(&graph.search("cly", 20)), signature(&oracle));
        assert_eq!(
            graph
                .referenced_page_names()
                .into_iter()
                .map(|name| crate::refs::page_key(&name))
                .collect::<std::collections::BTreeSet<_>>(),
            names
        );
        assert_eq!(
            graph.direct_projection_fuzzy_candidate_reads_test(),
            fuzzy_reads,
            "a stale generation must use the parser fallback"
        );
        assert_eq!(
            graph.direct_projection_referenced_name_reads_test(),
            name_reads,
            "a stale generation must not read reference names from SQLite"
        );

        let entry = graph
            .list_pages()
            .into_iter()
            .find(|entry| entry.name == "one")
            .unwrap();
        let mut page = graph.load_page(&entry).unwrap();
        let baseline = page.rev.clone();
        page.blocks[0].raw = "Nothing matching [[Replacement Page]]".into();
        graph.save_page(&page, baseline.as_deref()).unwrap();
        wait_ready(&graph);
        assert!(graph.search("cly", 20).is_empty());
        let names = graph
            .referenced_page_names()
            .into_iter()
            .map(|name| crate::refs::page_key(&name))
            .collect::<std::collections::BTreeSet<_>>();
        assert!(names.contains("replacement page"));
        assert!(!names.contains("inline page"));

        std::fs::write(
            root.join("pages/one.md"),
            "tags:: External Tag\n\n- Externally changed fuzzy [[External Page]]\n",
        )
        .unwrap();
        graph.sync_file_checked(&root.join("pages/one.md")).unwrap();
        wait_ready(&graph);
        assert!(!graph.search("ecf", 20).is_empty());
        let names = graph
            .referenced_page_names()
            .into_iter()
            .map(|name| crate::refs::page_key(&name))
            .collect::<std::collections::BTreeSet<_>>();
        assert!(names.contains("external tag"));
        assert!(names.contains("external page"));
        assert!(!names.contains("replacement page"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn direct_projection_matches_parser_reference_family_and_stale_fallback() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("reference-family-parity");
        let target_id = "11111111-2222-4333-8444-555555555555";
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/target.md"),
            format!("alias:: Alias Target\n\n- target\n  id:: {target_id}\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("pages/referrer.md"),
            format!(
                "- [[Alias Target]] and plain Alias Target and (({target_id})) (({target_id}))\n- another (({target_id}))\n"
            ),
        )
        .unwrap();
        std::fs::write(root.join("pages/unrelated.md"), "- unrelated\n").unwrap();

        let graph = Graph::open(&root);
        graph.warm_cache();
        let parser_aliases = crate::query::page_aliases_with_owners(&graph);
        let parser_backlinks = crate::query::backlinks(&graph, "target");
        let parser_unlinked = crate::query::unlinked_refs(&graph, "target");
        let parser_referrers = crate::query::block_referrers(&graph, target_id);
        let parser_resolved = crate::query::resolve_block(&graph, target_id);
        let parser_counts = graph.block_ref_counts().unwrap();

        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        wait_ready(&graph);

        assert_eq!(graph.page_aliases_with_owners(), parser_aliases);
        let explicit_candidates = graph.reference_candidate_pages(
            &[
                crate::refs::page_key("target"),
                crate::refs::page_key("Alias Target"),
            ],
            ReferenceKind::Explicit,
        );
        assert!(explicit_candidates.indexed);
        assert!(explicit_candidates.pages.len() < explicit_candidates.full_page_count);
        assert_eq!(
            signature(&crate::query::backlinks(&graph, "target")),
            signature(&parser_backlinks)
        );
        assert_eq!(
            signature(&crate::query::unlinked_refs(&graph, "target")),
            signature(&parser_unlinked)
        );
        assert_eq!(
            signature(&crate::query::block_referrers(&graph, target_id)),
            signature(&parser_referrers)
        );
        assert_eq!(
            crate::query::resolve_block(&graph, target_id)
                .as_ref()
                .map(|group| signature(std::slice::from_ref(group))),
            parser_resolved
                .as_ref()
                .map(|group| signature(std::slice::from_ref(group)))
        );
        assert_eq!(
            graph.block_ref_counts().unwrap().as_ref(),
            parser_counts.as_ref()
        );
        assert_eq!(graph.block_ref_counts().unwrap().get(target_id), Some(&2));

        let custom_path = root.join("pages/custom.md");
        std::fs::write(&custom_path, "- custom identity\n  id:: not-a-uuid\n").unwrap();
        assert!(graph.sync_file(&custom_path).is_some());
        wait_ready(&graph);
        assert_eq!(
            crate::query::resolve_block(&graph, "not-a-uuid")
                .and_then(|group| group.blocks.into_iter().next())
                .map(|block| block.raw),
            Some("custom identity\nid:: not-a-uuid".to_string())
        );

        graph.direct_projection_mark_stale_test();
        assert_eq!(graph.page_aliases_with_owners(), parser_aliases);
        assert_eq!(
            signature(&crate::query::backlinks(&graph, "target")),
            signature(&parser_backlinks)
        );
        assert_eq!(
            signature(&crate::query::block_referrers(&graph, target_id)),
            signature(&parser_referrers)
        );
        assert_eq!(
            graph.block_ref_counts().unwrap().as_ref(),
            parser_counts.as_ref()
        );

        let target_path = root.join("pages/target.md");
        std::fs::write(
            &target_path,
            format!("alias:: Changed Alias\n\n- target\n  id:: {target_id}\n"),
        )
        .unwrap();
        assert!(graph.sync_file(&target_path).is_some());
        wait_ready(&graph);
        let changed_aliases = graph.page_aliases_with_owners();
        assert!(changed_aliases
            .iter()
            .any(|(alias, owner, _)| alias == "changed alias" && owner == "target"));
        assert!(!changed_aliases
            .iter()
            .any(|(alias, _, _)| alias == "alias target"));

        graph.delete_page("target", PageKind::Page).unwrap();
        wait_ready(&graph);
        assert!(!graph
            .page_aliases_with_owners()
            .iter()
            .any(|(alias, _, _)| alias == "changed alias"));
        let _ = std::fs::remove_dir_all(root);
    }

    /// GH #400. An ordinary edit has already published its parsed page and
    /// queued the exact one-page SQLite delta. A reference read which overlaps
    /// that short worker turn must not immediately turn into a whole-graph
    /// parser scan. Waiting for this already-running bounded delta preserves the
    /// same semantics and avoids the reported multi-second fallback.
    #[test]
    fn a_timed_out_close_retains_resources_until_the_writer_really_exits() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("worker-resource-lifetime");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/source.md"), "- before\n").unwrap();
        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/query.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        let projection = graph.direct_projection_test().unwrap();
        let resource = Arc::new(());
        let weak = Arc::downgrade(&resource);
        projection.retain_worker_resource(resource);
        let (paused_tx, paused_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        *BEFORE_APPLY_PENDING.lock().unwrap() = Some(Box::new(move || {
            paused_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
        }));
        let entry = graph
            .list_pages()
            .into_iter()
            .find(|entry| entry.name == "source")
            .unwrap();
        let mut page = graph.load_page(&entry).unwrap();
        let revision = page.rev.clone();
        page.blocks[0].raw = "after".into();
        graph.save_page(&page, revision.as_deref()).unwrap();
        paused_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let closed = projection.close_and_wait_for_worker(Duration::ZERO);
        let retained = weak.upgrade().is_some();
        // Release the real worker even if an assertion below fails.
        resume_tx.send(()).unwrap();
        assert!(!closed, "the paused writer cannot have finished");
        assert!(
            retained,
            "a wait timeout must not destroy the writer's resources"
        );
        assert!(projection.close_and_wait_for_worker(Duration::from_secs(5)));
        assert!(
            weak.upgrade().is_none(),
            "the exited writer must release resources"
        );
        // Registration after exit must not retain a resource forever.
        let late = Arc::new(());
        let late_weak = Arc::downgrade(&late);
        projection.retain_worker_resource(late);
        assert!(late_weak.upgrade().is_none());
        drop(graph);
        drop(projection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reference_lookup_waits_for_an_inflight_one_page_projection_delta() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("reference-delta-handoff");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/target.md"), "- target\n").unwrap();
        std::fs::write(root.join("pages/source.md"), "- unrelated\n").unwrap();

        let graph = Arc::new(Graph::open(&root));
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);

        let (worker_paused_tx, worker_paused_rx) = mpsc::channel();
        let (release_worker_tx, release_worker_rx) = mpsc::channel();
        *BEFORE_APPLY_PENDING.lock().unwrap() = Some(Box::new(move || {
            worker_paused_tx.send(()).unwrap();
            release_worker_rx.recv().unwrap();
        }));

        let entry = graph
            .list_pages()
            .into_iter()
            .find(|entry| entry.name == "source")
            .unwrap();
        let mut page = graph.load_page(&entry).unwrap();
        let baseline = page.rev.clone();
        page.blocks[0].raw = "plain target mention".into();
        graph.save_page(&page, baseline.as_deref()).unwrap();
        worker_paused_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the one-page projection delta reached the worker");

        let reader = Arc::clone(&graph);
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let candidates = reader.reference_candidate_pages(
                &[crate::refs::page_key("target")],
                ReferenceKind::Plain,
            );
            result_tx.send(candidates.indexed).unwrap();
        });

        match result_rx.recv_timeout(Duration::from_millis(100)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            result => {
                let _ = release_worker_tx.send(());
                panic!(
                    "reference lookup escaped to parser fallback before its queued delta completed: {result:?}"
                );
            }
        }
        release_worker_tx.send(()).unwrap();
        assert_eq!(
            result_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            true,
            "the converged lookup must use current indexed candidates"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reference_wait_is_zero_cost_when_no_projection_work_exists() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("reference-no-work-wait");
        let projection = DirectProjection::start(root.join("projection.sqlite")).unwrap();
        let started = Instant::now();
        assert!(!projection.wait_for_reference_generation(1));
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "an unavailable projection must fall back immediately"
        );
        drop(projection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn direct_projection_preserves_external_uuid_ambiguity_for_parser_resolution() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("external-uuid-ambiguity");
        let target_id = "11111111-2222-4333-8444-555555555555";
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/alpha.md"),
            format!("- alpha claimant\n  id:: {target_id}\n"),
        )
        .unwrap();
        std::fs::write(
            root.join("pages/beta.md"),
            format!("- beta claimant\n  id:: {target_id}\n"),
        )
        .unwrap();

        let graph = Graph::open(&root);
        graph.warm_cache();
        let parser_resolution = crate::query::resolve_block(&graph, target_id)
            .map(|group| signature(std::slice::from_ref(&group)));
        let projection_path = root.join("private/projection.sqlite");
        graph
            .attach_direct_projection(projection_path.clone())
            .unwrap();
        wait_ready(&graph);

        let database = PhysicalGraphProjectionDatabase::open_read_only(&projection_path).unwrap();
        let claim = Uuid::parse_str(target_id).unwrap().into_bytes();
        assert_eq!(
            database
                .read()
                .blocks_by_logseq_uuid(claim, 2)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            crate::query::resolve_block(&graph, target_id)
                .map(|group| signature(std::slice::from_ref(&group))),
            parser_resolution,
            "SQLite must not choose one external UUID owner from an ambiguous graph"
        );
        drop(database);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reference_family_has_no_second_in_memory_semantic_index() {
        let model = include_str!("model.rs");
        for removed in [
            "alias_cache",
            "reference_candidate_index",
            "block_ref_count_cache",
            "block_index: RwLock",
        ] {
            assert!(
                !model.contains(removed),
                "Direct Files reference family reintroduced {removed} beside SQLite"
            );
        }
    }

    #[test]
    fn direct_projection_fuzzy_candidates_preserve_parser_corpus_semantics() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("search-corpus-parity");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/search.md"),
            "- Characteristically useful\n  - descendant Needle\n- Café and cafe\u{301}\n- 100% under_score back\\slash\n- MixedCASE\n- x a y b z\n",
        )
        .unwrap();
        std::fs::write(
            root.join("pages/other.md"),
            "- Another characteristically useful result\n",
        )
        .unwrap();
        let cases = [
            ("", 20),
            ("   ", 20),
            ("cly", 20),
            ("needle", 20),
            ("CAFÉ", 20),
            ("cafe\u{301}", 20),
            ("%", 20),
            ("_", 20),
            ("\\", 20),
            ("mixedcase", 20),
            ("xyz", 20),
            ("cly", 1),
        ];
        let oracle_graph = Graph::open(&root);
        oracle_graph.warm_cache();
        let oracle = cases
            .iter()
            .map(|(query, limit)| signature(&crate::query::search(&oracle_graph, query, *limit)))
            .collect::<Vec<_>>();
        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        assert!(
            graph.warm_cache_cancellable(|| false),
            "corpus cache failed to warm: {:?}",
            graph.page_index_failures()
        );
        wait_ready(&graph);
        for ((query, limit), expected) in cases.into_iter().zip(oracle) {
            assert_eq!(
                signature(&graph.search(query, limit)),
                expected,
                "{query:?}"
            );
        }
        let cancellation_checks = std::cell::Cell::new(0);
        assert!(crate::query::search_cancellable(&graph, "cly", 20, || {
            cancellation_checks.set(cancellation_checks.get() + 1);
            cancellation_checks.get() > 1
        })
        .is_empty());

        graph.rename_page("search", "renamed search").unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        assert_eq!(
            signature(&graph.search("needle", 20)),
            signature(&crate::query::search(&graph, "needle", 20))
        );
        graph.delete_page("renamed search", PageKind::Page).unwrap();
        wait_ready(&graph);
        assert!(graph.search("needle", 20).is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    /// **RET2's missing-projection shape.** A projection that could not be
    /// created at all is not a reason to walk the graph: the public query route
    /// refuses with a typed `Unavailable(ProjectionUnavailable)`, which is the
    /// bounded signal the frontend surfaces instead of a spinner that never
    /// ends. The primitives that are NOT part of the query route — search and
    /// the reference-name inventory — keep their own semantics unchanged, which
    /// is what makes this a query-route claim and not a graph-wide one.
    #[test]
    fn unavailable_projection_refuses_the_public_query_and_keeps_other_semantics() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("fallback");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(
            root.join("pages/tasks.md"),
            "- TODO Characteristically readable [[Inline Only]]\n  alias:: #Alias Only\n",
        )
        .unwrap();
        let blocked_parent = root.join("not-a-directory");
        std::fs::write(&blocked_parent, b"ordinary file").unwrap();

        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(blocked_parent.join("projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        std::thread::sleep(Duration::from_millis(30));
        // Non-vacuity: the graph DOES hold a matching TODO, so a refusal here
        // cannot be confused with a correct empty answer.
        let oracle = crate::query::run_query_bounded(&graph, "(task TODO)", 100, 1_000_000);
        assert!(oracle.total > 0, "the fixture must have something to match");

        let walks_before = crate::query::full_graph_query_evaluations();
        let refused = graph.run_query_bounded("(task TODO)", 100, 1_000_000);
        assert!(
            matches!(
                refused,
                Err(crate::query::QueryExecutionError::Unavailable(
                    crate::query::QueryUnavailableReason::ProjectionUnavailable
                ))
            ),
            "a projection that could not be created refuses, it does not walk: {refused:?}"
        );
        assert_eq!(
            crate::query::full_graph_query_evaluations(),
            walks_before,
            "the refusal must not traverse the graph"
        );
        assert_eq!(graph.direct_projection_indexed_reads_test(), 0);
        assert_eq!(
            signature(&graph.search("cly", 20)),
            signature(&crate::query::search(&graph, "cly", 20))
        );
        let names = graph
            .referenced_page_names()
            .into_iter()
            .map(|name| crate::refs::page_key(&name))
            .collect::<std::collections::BTreeSet<_>>();
        assert!(names.contains("inline only"));
        assert!(names.contains("alias only"));
        assert_eq!(graph.direct_projection_fuzzy_candidate_reads_test(), 0);
        assert_eq!(graph.direct_projection_referenced_name_reads_test(), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_graph_instance_cannot_replace_ready_projection_facts() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("single-writer");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/tasks.md"), "- TODO one\n").unwrap();
        let database = scratch("single-writer-db").join("projection.sqlite");

        let owner = Graph::open(&root);
        owner.attach_direct_projection(database.clone()).unwrap();
        owner.warm_cache();
        wait_ready(&owner);

        let fallback = Graph::open(&root);
        fallback.attach_direct_projection(database.clone()).unwrap();
        fallback.warm_cache();
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !fallback.direct_projection_ready_test(),
            "a second graph instance must not publish into the first instance's ready database"
        );
        // Non-vacuity: the graph matches, so the second instance's refusal is
        // not a correct empty answer wearing a hat.
        let oracle = crate::query::run_query_bounded(&fallback, "(task TODO)", 100, 1_000_000);
        assert!(oracle.total > 0, "the fixture must have something to match");
        let walks_before = crate::query::full_graph_query_evaluations();
        let refused = fallback.run_query_bounded("(task TODO)", 100, 1_000_000);
        assert!(
            refused.is_err(),
            "an instance that cannot publish into the owner's database refuses \
             rather than walking: {refused:?}"
        );
        assert_eq!(
            crate::query::full_graph_query_evaluations(),
            walks_before,
            "the refusal must not traverse the graph"
        );
        assert_eq!(fallback.direct_projection_indexed_reads_test(), 0);

        let owner_oracle = crate::query::run_query_bounded(&owner, "(task TODO)", 100, 1_000_000);
        let owner_actual = when_ready(|| owner.run_query_bounded("(task TODO)", 100, 1_000_000));
        assert_eq!(
            signature(&owner_actual.groups),
            signature(&owner_oracle.groups)
        );
        assert!(owner.direct_projection_indexed_reads_test() > 0);

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    #[test]
    fn coalesced_edits_keep_first_insertion_page_order_and_readds_append() {
        let entry = |name: &str| PageEntry {
            name: name.into(),
            kind: PageKind::Page,
            date_key: None,
            rel_path: format!("pages/{name}.md"),
            path: PathBuf::from(format!("pages/{name}.md")),
        };
        let replacement = |name: &str| PageDelta::Replace {
            entry: entry(name),
            document: Arc::new(crate::doc::parse("- text")),
            revision: "exact-revision".into(),
            parse_config: Arc::new(ParseConfig::default()),
            query_page_order: None,
            identity: DeltaIdentity::Live,
        };
        let position = |pending: &PendingProjection, name: &str| match &pending.deltas
            [&format!("pages/{name}.md")]
            .1
        {
            PageDelta::Replace {
                query_page_order, ..
            } => query_page_order.expect("a delta outside a warm stream carries its position"),
            _ => panic!("replacement expected"),
        };
        let mut pending = PendingProjection::default();
        pending.record_delta(1, replacement("z-first"));
        pending.record_delta(2, replacement("a-second"));
        pending.record_delta(3, replacement("z-first"));
        assert_eq!(position(&pending, "z-first"), 0);
        assert_eq!(position(&pending, "a-second"), 1);
        assert_eq!(pending.deltas.len(), 2, "first page edit is coalesced");
        pending.record_delta(
            4,
            PageDelta::Delete {
                entry: entry("z-first"),
            },
        );
        pending.record_delta(5, replacement("z-first"));
        assert_eq!(position(&pending, "a-second"), 1);
        assert_eq!(position(&pending, "z-first"), 2);
    }

    #[test]
    fn clean_reopen_reuses_sqlite_and_external_edit_relowers_only_one_page() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("reopen-revisions");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/one.md"), "- TODO one\n").unwrap();
        std::fs::write(root.join("pages/two.md"), "- DONE two\n").unwrap();
        let database = scratch("reopen-revisions-db").join("projection.sqlite");

        reset_lowerings(&root);
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
            assert_eq!(lowerings(), 2);
        }
        std::thread::sleep(Duration::from_millis(20));

        reset_lowerings(&root);
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
            assert_eq!(lowerings(), 0, "unchanged pages must stay inside SQLite");
        }
        std::thread::sleep(Duration::from_millis(20));

        std::fs::write(root.join("pages/one.md"), "- TODO one changed\n").unwrap();
        reset_lowerings(&root);
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
            assert_eq!(
                lowerings(),
                1,
                "one changed page must produce one SQL delta"
            );
            assert_eq!(
                signature(
                    &graph
                        .run_query_bounded("(task TODO)", 100, 1_000_000)
                        .expect("the ready projection answers the public bounded route")
                        .groups
                ),
                signature(
                    &crate::query::run_query_bounded(&graph, "(task TODO)", 100, 1_000_000).groups
                )
            );
        }

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    #[test]
    fn extractor_version_participates_in_disposable_source_revision() {
        let source = "sha256:unchanged-source";
        let digest = ParseConfig::default().digest();
        let projected = projection_source_revision(source, digest);
        let hex = digest
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            projected,
            format!("direct-facts-v2:{hex}:sha256:unchanged-source")
        );
        assert_ne!(projected, source);
    }

    /// Guard 4, Direct Files half (§5.8 J7). Reconciliation compares only
    /// source revisions, so a config edit that changes no file byte must still
    /// change the revision it compares -- otherwise every unchanged page keeps
    /// rows derived under the old config forever.
    #[test]
    fn a_parse_config_change_moves_every_source_revision() {
        let source = "sha256:unchanged-source";
        let mut edited = ParseConfig::default();
        edited.separated_by_commas.push("authors".to_owned());
        assert_ne!(ParseConfig::default().digest(), edited.digest());
        assert_ne!(
            projection_source_revision(source, ParseConfig::default().digest()),
            projection_source_revision(source, edited.digest()),
        );
    }

    /// **F11.** The parse config travels inside each queued work item, so two
    /// replacements coalesced into one worker turn are each lowered and stamped
    /// under the config they were queued with -- never under whichever config
    /// the last enqueue happened to leave beside the queue, and never under a
    /// default that absence could stand in for.
    ///
    /// The stamp is what reconciliation compares, so a page carrying another
    /// page's config digest is a page whose rows answer a question the config
    /// no longer asks and which no later reopen will notice (J7, D-1).
    #[test]
    fn each_queued_page_lowers_under_the_config_it_was_queued_with() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("per-item-parse-config");
        std::fs::create_dir_all(&root).unwrap();
        let mut database = open_projection_database(&root.join("projection.sqlite")).unwrap();

        let default_config = Arc::new(ParseConfig::default());
        let edited_config = Arc::new({
            let mut edited = ParseConfig::default();
            edited.separated_by_commas.push("authors".to_owned());
            edited
        });
        assert_ne!(default_config.digest(), edited_config.digest());

        let queued = |rel_path: &str, parse_config: &Arc<ParseConfig>| {
            (
                rel_path.to_owned(),
                (
                    1_u64,
                    PageDelta::Replace {
                        entry: PageEntry {
                            name: rel_path.trim_end_matches(".md").to_owned(),
                            kind: PageKind::Page,
                            date_key: None,
                            rel_path: rel_path.to_owned(),
                            path: root.join(rel_path),
                        },
                        document: Arc::new({
                            let mut document = crate::doc::parse("- authors:: ada, grace\n");
                            crate::model::assign_doc_runtime_ids(&mut document.roots, rel_path);
                            document
                        }),
                        revision: format!("sha256:{rel_path}"),
                        parse_config: Arc::clone(parse_config),
                        query_page_order: Some(u64::from(rel_path == "beta.md")),
                        identity: DeltaIdentity::Live,
                    },
                ),
            )
        };
        let deltas = BTreeMap::from([
            queued("alpha.md", &default_config),
            queued("beta.md", &edited_config),
        ]);
        apply_pending(&mut database, None, None, deltas).unwrap();

        let stamped = |alpha: &Arc<ParseConfig>, beta: &Arc<ParseConfig>| {
            database
                .source_delta(&[
                    PhysicalGraphProjectionSourceRevision {
                        page_id: page_id("alpha.md"),
                        revision: projection_source_revision("sha256:alpha.md", alpha.digest()),
                    },
                    PhysicalGraphProjectionSourceRevision {
                        page_id: page_id("beta.md"),
                        revision: projection_source_revision("sha256:beta.md", beta.digest()),
                    },
                ])
                .unwrap()
                .replacements
        };
        assert!(
            stamped(&default_config, &edited_config).is_empty(),
            "each page must carry the digest of the config it was queued with"
        );
        // Not vacuous: the two stamps really are distinct, so the assertion
        // above could have failed.
        assert_eq!(
            stamped(&edited_config, &default_config).len(),
            2,
            "swapping the two configs must make both pages stale"
        );
        drop(database);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn storage_contract_names_the_generation_bound_cutover() {
        fn contains_words(haystack: &str, needle: &str) -> bool {
            let normalize = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
            let needle = normalize(needle);
            normalize(haystack).contains(needle.as_str())
        }

        let contract = include_str!("../../../docs/storage-sync-contract.md");
        assert!(contains_words(
            contract,
            "direct-files-projections/<canonical-graph-path-digest>.sqlite"
        ));
        // RET2 correction: the sparse task-query family is DELETED, so the
        // contract must say so rather than describe it as a live read family.
        // Pinned as a retirement, because "the document still mentions it" is
        // how dead code survives a sweep.
        assert!(contains_words(
            contract,
            "There is no separate sparse task-query read family."
        ));
        assert!(contains_words(
            contract,
            "the RET2 correction deleted the\ngate"
        ));
        assert!(contains_words(contract, "shared\nproperty-facet rows"));
        assert!(contains_words(
            contract,
            "PageRef simple-query candidate plan"
        ));
        assert!(contains_words(
            contract,
            "PageRef simple-query candidate plan in the independent test oracle."
        ));
        assert!(contains_words(
            contract,
            "Both production\nquery backends now select results through SQL."
        ));
        assert!(contains_words(contract, "literal fuzzy-search candidate"));
        assert!(contains_words(contract, "referenced-page\ninventory"));
        assert!(contains_words(
            contract,
            "retains no separate semantic memo"
        ));
        assert!(contains_words(
            contract,
            "exact current graph cache generation"
        ));
        assert!(contains_words(contract, "Direct fact-extractor version"));
        assert!(contains_words(
            contract,
            "app-private graph-fact projection contains no managed state"
        ));
        assert!(contains_words(contract, "clean\nreopen lowers none"));
        assert!(contains_words(
            contract,
            "memo of already-shaped frontend result DTOs remains Tine-native"
        ));
        assert!(contains_words(contract, "grants no\n   authority"));
        // R3: the owned-snapshot job contract this file implements.
        assert!(contains_words(
            contract,
            "capacity is acquired before\nthe snapshot"
        ));
        assert!(contains_words(
            contract,
            "the worker drains every job before a rebuild touches the file"
        ));
        assert!(contains_words(
            contract,
            "Cancellation is\na typed dispatch answer, not a failed read"
        ));
        assert!(contains_words(
            contract,
            "Result identity follows who lowered the row"
        ));
        // R6: warm validation and the session-identity ownership rule.
        assert!(contains_words(
            contract,
            "Warm validation from bytes, never from a parsed graph"
        ));
        assert!(contains_words(
            contract,
            "a delta alone never publishes an\ninventory"
        ));
        assert!(contains_words(
            contract,
            "dropping the parsed cache clears the set"
        ));

        // The routing rule is asserted inside its own section, not anywhere in
        // the document: a whole-document `contains` passes with the sentence
        // parked under an unrelated heading, which is exactly how a contract
        // stops describing the subsystem it claims to describe.
        let heading = "### 1.3 Direct Files disposable graph projection";
        let start = contract.find(heading).expect("Direct projection section");
        let body = &contract[start + heading.len()..];
        let section = body
            .find("\n## ")
            .map_or(body, |end| &body[..end])
            .to_owned();
        // RET2's Direct Files route, and the lifecycle facts a reader has to be
        // able to check without reading the code: which reads a query performs,
        // how every non-answer is classified, what one repair owes, and what a
        // cached result is keyed by.
        for sentence in [
            // One SQL route. The no-walk clause is pinned because weakening it
            // into an availability fallback would restore the retired engine.
            "The Direct public-query route has no production tree-walk fallback.",
            "semantically refused source returns its existing empty or unsupported-report\nanswer before any job or snapshot",
            "`@block`, `@page` and Explain reads enter `dispatch_direct_query`",
            "a refresh enters the same dispatcher\nand query-job owner",
            "There is no cost test and no selectivity hatch in\nfront of this route.",
            "None of these\nbranches evaluates the parsed graph or fabricates an empty success.",
            // Snapshot, metadata and ordered result reads (I-13, I-15).
            "Capacity is acquired before SQLite opens\nthe owned snapshot.",
            "when the table is not\nalready current, the job reads it from its own snapshot before lowering",
            "Ready query selection\nand result construction load NO `Document`, read NO source text and consult no\nparsed graph.",
            "Recovery source-inventory work is counted separately.",
            "Its descriptor wrapper does: Direct\nblock answers carry `query_page_order.position` and\n`query_block_results.preorder` and end with `ORDER BY` on those columns",
            "Missing Direct order metadata\nfails the read",
            "remembered once per generation,\nnever once per query",
            // Typed non-answers and the one-repair obligation.
            "Capacity pressure is\n`NotReady(Busy)`.",
            "cancellation is `Cancelled` and schedules no\nrepair",
            "gets at most one\nbounded repair and one SQL retry",
            "a torn or truncated projection file after a crash or power\nloss, a disk error, a resource limit, or a projection whose page set has drifted\nfrom the current graph generation",
            "otherwise recovery validates a\ncomplete source inventory from bytes and streams bounded per-page replacements",
            "Clearing readiness\nalone would strand the projection until another edit.",
            "Cancellation is excluded\nfrom repair",
            // The cache key.
            "memoized PRE-VIEW",
            "under the resolved normalized query IR, the\ngraph cache generation, the execution day, the construction bounds and profile",
            "The parse-config digest is unconditional",
            "A warm parsed cache\nallows a safe ordinary content edit to retain unaffected entries",
            "A cold session with no parsed\ncache, a page-set or alias/identity change, an unreadable key, or another\ngraph-wide uncertainty drops the applicable memo",
            "Query execution and memo hits themselves\ndo not require a resident parsed graph.",
            // Navigation and Friendly remain separately scoped migration work.
            "Friendly graph\nsearch likewise still ranks and produces evidence from parser-projected blocks",
            "they do not authorize a fallback from the\nsimple, advanced, page, registry, or Explain public-query dispatch",
            // The candidate planner and its selectivity cutoff are oracle-only.
            "Managed production queries no longer construct a candidate-page plan or apply\nits selectivity cutoff.",
            "semantic empty refusal before any snapshot or registry acquisition",
            "Candidate types and lowering remain test-only\nfor the independent oracle.",
        ] {
            assert!(
                contains_words(&section, sentence),
                "§1.3 must state the required Direct Files query semantics: {sentence}"
            );
        }
    }

    #[test]
    #[ignore = "manual storage packet receipt; set TINE_DIRECT_PROJECTION_CORPUS"]
    fn real_corpus_projection_converges_and_matches_task_query() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = PathBuf::from(
            std::env::var("TINE_DIRECT_PROJECTION_CORPUS")
                .expect("TINE_DIRECT_PROJECTION_CORPUS is required"),
        );
        let database = scratch("real-corpus").join("projection.sqlite");
        let oracle_graph = Graph::open(&root);
        oracle_graph.warm_cache();
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        let started = Instant::now();
        graph.warm_cache();
        let warm = started.elapsed();
        wait_ready(&graph);
        let converged = started.elapsed();
        let oracle_started = Instant::now();
        let oracle =
            crate::query::run_query_bounded(&oracle_graph, "(task TODO)", 20_000, 32 << 20);
        let oracle_elapsed = oracle_started.elapsed();
        let query_started = Instant::now();
        let indexed = graph
            .run_query_bounded("(task TODO)", 20_000, 32 << 20)
            .expect("the ready projection answers the public bounded route");
        let indexed_elapsed = query_started.elapsed();
        assert_eq!(signature(&indexed.groups), signature(&oracle.groups));
        let indexed_reads = graph.direct_projection_indexed_reads_test();
        let memo_started = Instant::now();
        let repeated = graph
            .run_query_bounded("(task TODO)", 20_000, 32 << 20)
            .expect("the ready projection answers the public bounded route");
        let memo_elapsed = memo_started.elapsed();
        assert_eq!(signature(&repeated.groups), signature(&oracle.groups));
        assert_eq!(graph.direct_projection_indexed_reads_test(), indexed_reads);
        let mut fuzzy_indexed = Duration::ZERO;
        let mut fuzzy_oracle = Duration::ZERO;
        for value in ["a", "todo", "http", "2026", "%", "_", "é"] {
            let indexed_started = Instant::now();
            let indexed_search = graph.search(value, 5_000);
            fuzzy_indexed += indexed_started.elapsed();
            let oracle_started = Instant::now();
            let oracle_search = crate::query::search(&oracle_graph, value, 5_000);
            fuzzy_oracle += oracle_started.elapsed();
            assert_eq!(
                signature(&indexed_search),
                signature(&oracle_search),
                "real-corpus fuzzy search diverged for a bounded probe"
            );
        }
        eprintln!(
            "direct projection fuzzy receipt: indexed_total_ms={} oracle_total_ms={}",
            fuzzy_indexed.as_millis(),
            fuzzy_oracle.as_millis(),
        );
        let normalize_names = |mut names: Vec<String>| {
            names.sort_by_key(|name| crate::refs::page_key(name));
            names
        };
        assert_eq!(
            normalize_names(graph.referenced_page_names()),
            normalize_names(oracle_graph.referenced_page_names()),
            "real-corpus referenced-page inventory diverged"
        );
        assert!(graph.direct_projection_fuzzy_candidate_reads_test() > 0);
        assert!(graph.direct_projection_referenced_name_reads_test() > 0);
        let task_candidates = PhysicalGraphProjectionDatabase::open_read_only(&database)
            .unwrap()
            .read()
            .task_candidate_blocks_after("TODO", None, 10_000)
            .unwrap()
            .len();
        eprintln!(
            "direct projection receipt: warm_ms={} projection_total_ms={} oracle_query_us={} indexed_query_us={} repeated_query_us={} pages={} task_candidates={}",
            warm.as_millis(),
            converged.as_millis(),
            oracle_elapsed.as_micros(),
            indexed_elapsed.as_micros(),
            memo_elapsed.as_micros(),
            graph.list_pages().len(),
            task_candidates,
        );
    }

    // ----- R6: parsed independence (warm reuse, streaming cold init, session
    // identity). Each test below was RED at 5684c8cb (necessity receipt in
    // tine-agents/evidence/qe/r6/).

    fn r6_graph(tag: &str) -> PathBuf {
        let root = scratch(tag);
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::create_dir_all(root.join("journals")).unwrap();
        std::fs::write(root.join("pages/one.md"), "- TODO one [[target]]\n").unwrap();
        std::fs::write(root.join("pages/two.md"), "- DONE two\n").unwrap();
        std::fs::write(
            root.join("pages/target.md"),
            "- target\n  status:: active\n",
        )
        .unwrap();
        std::fs::write(
            root.join("pages/titled.md"),
            "title:: Titled Page\n\n- TODO titled\n",
        )
        .unwrap();
        std::fs::write(root.join("journals/2026_09_06.md"), "- TODO today\n").unwrap();
        root
    }

    fn entry_signature(entries: &[PageEntry]) -> Vec<(String, String, Option<i64>, String)> {
        let mut signature = entries
            .iter()
            .map(|entry| {
                (
                    entry.rel_path.clone(),
                    entry.name.clone(),
                    format!("{:?}", entry.kind),
                    entry.date_key,
                )
            })
            .map(|(rel_path, name, kind, date_key)| (rel_path, name, date_key, kind))
            .collect::<Vec<_>>();
        signature.sort();
        signature
    }

    /// **R6 §1.** An unchanged reopen validates the projection from file
    /// bytes alone: nothing is parsed, nothing is lowered, no parsed cache
    /// exists, and every startup consumer — the dispatched query, aliases,
    /// block-ref counts, the property registry, `list_pages` — answers from
    /// SQL with the cache still absent.
    #[test]
    fn public_ir_query_on_warm_reopen_uses_sql_without_parsed_cache() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = r6_graph("public-ir-warm");
        let database = scratch("public-ir-warm-db").join("projection.sqlite");
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
        }
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database).unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        assert!(!graph.has_parsed_cache_test());
        let (query, view) = crate::query::parse_query_text(
            "(task TODO)",
            crate::query::QueryDialect::Og,
            crate::date::JournalDate::today(),
        );
        let before = graph.direct_projection_statement_reads_test();
        let result = crate::query::run_query_result_ir(
            &graph,
            &query,
            &view,
            crate::query::ir::Bounds {
                max_rows: 100,
                max_bytes: 1_000_000,
            },
            &crate::query::ir::ExecutionContext::default(),
        )
        .expect("the ready projection answers the public IR route");
        assert!(
            result.total > 0,
            "fixture must exercise actual result construction"
        );
        assert_eq!(
            graph.direct_projection_statement_reads_test(),
            before + 1,
            "public query_run must execute SQLite, not the oracle"
        );
        assert!(
            !graph.has_parsed_cache_test(),
            "query_run must not hydrate the graph"
        );
    }

    #[test]
    fn warm_reopen_parses_nothing_and_answers_from_sql() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = r6_graph("warm-reopen");
        let database = scratch("warm-reopen-db").join("projection.sqlite");
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
        }
        std::thread::sleep(Duration::from_millis(20));

        reset_lowerings(&root);
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        assert!(graph.warm_cache_cancellable(|| false));
        wait_ready(&graph);
        assert_eq!(lowerings(), 0, "unchanged pages stay inside SQLite");
        assert_eq!(
            graph.page_build_parses_test(),
            0,
            "a warm reopen parses nothing"
        );
        assert_eq!(graph.warm_stream_parses_test(), 0);
        assert!(
            !graph.has_parsed_cache_test(),
            "readiness must not require the whole-graph parsed cache"
        );

        let oracle = Graph::open(&root);
        let statements = graph.direct_projection_statement_reads_test();
        let fallbacks = graph.direct_projection_fallback_reads_test();
        let indexed = graph
            .run_query_bounded("(task TODO)", 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        assert_eq!(
            signature(&indexed.groups),
            signature(
                &crate::query::run_query_bounded(&oracle, "(task TODO)", 100, 1_000_000).groups
            )
        );
        assert_eq!(
            graph.direct_projection_statement_reads_test(),
            statements + 1
        );
        assert_eq!(graph.direct_projection_fallback_reads_test(), fallbacks);

        assert_eq!(
            graph.page_aliases_with_owners(),
            crate::query::page_aliases_with_owners(&oracle)
        );
        assert_eq!(
            *graph.block_ref_counts().unwrap(),
            *oracle.block_ref_counts().unwrap()
        );
        let _ = graph.property_registry();
        assert_eq!(
            entry_signature(&graph.list_pages()),
            entry_signature(&oracle.list_pages())
        );
        assert_eq!(graph.page_build_parses_test(), 0);
        assert!(
            !graph.has_parsed_cache_test(),
            "no startup consumer may force the whole-graph parse"
        );

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    #[test]
    fn query_registry_snapshot_preserves_old_reads_without_publishing_over_new_edits() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("registry-snapshot");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        let source = root.join("pages/Source.md");
        std::fs::write(&source, "score:: 1\n- TODO task\n  score:: 2\n").unwrap();
        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        let projection = graph.direct_projection_test().unwrap();
        let old_generation = graph.cache_generation();
        let QueryJobOpen::Job(mut old_job) = projection.open_query_job(old_generation) else {
            panic!("initial snapshot must be ready");
        };
        let config = graph.config.parse_config();
        let old = old_job.read_registry(&config).unwrap();
        assert!(!old.rows().is_empty());
        let legacy = graph.property_registry();
        assert!(
            old.rows_equal(&legacy),
            "the shared inference producer must agree"
        );

        std::fs::write(&source, "score:: word\n- TODO task\n  score:: another\n").unwrap();
        graph.invalidate_cache();
        assert!(graph.warm_cache_cancellable(|| false));
        wait_ready(&graph);
        let new_generation = graph.cache_generation();
        assert_ne!(old_generation, new_generation);
        let QueryJobOpen::Job(mut new_job) = projection.open_query_job(new_generation) else {
            panic!("updated snapshot must be ready");
        };
        let new = graph
            .query_property_registry_at(new_generation, &config, &mut new_job)
            .unwrap();
        assert!(!old.rows_equal(&new), "the property type changed");
        let retained = graph
            .query_property_registry_at(old_generation, &config, &mut old_job)
            .unwrap();
        assert!(
            old.rows_equal(&retained),
            "later edits preserve the acquired read"
        );
        assert!(
            new.rows_equal(&graph.property_registry()),
            "old readers cannot overwrite the new registry"
        );
        assert!(
            !graph.has_parsed_cache_test(),
            "registry reads retain no Documents"
        );
    }

    #[test]
    fn query_registry_snapshot_rejects_orphaned_property_owners() {
        use crate::query::{QueryExecutionError, QueryUnavailableReason};
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("registry-orphan");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/Source.md"), "- TODO task\n  score:: 2\n").unwrap();
        let graph = Graph::open(&root);
        let database = root.join("private/projection.sqlite");
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        let damage = rusqlite::Connection::open(&database).unwrap();
        damage.execute("PRAGMA foreign_keys = OFF", []).unwrap();
        damage.execute("DELETE FROM blocks", []).unwrap();
        drop(damage);
        let projection = graph.direct_projection_test().unwrap();
        // The old row adapter accepted this impossible owner: pin the failure
        // scenario independently of the new visitor's implementation.
        assert!(!projection
            .property_owner_rows(graph.cache_generation())
            .unwrap()
            .0
            .is_empty());
        let QueryJobOpen::Job(mut job) = projection.open_query_job(graph.cache_generation()) else {
            panic!("the schema still opens before corrupt ownership is inspected");
        };
        assert!(matches!(
            job.read_registry(&graph.config.parse_config()),
            Err(QueryExecutionError::Unavailable(
                QueryUnavailableReason::InvalidSnapshot
            ))
        ));
        assert!(!graph.has_parsed_cache_test());
    }

    #[test]
    fn task_query_does_not_refresh_property_registry() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("task-no-registry");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        std::fs::write(root.join("pages/Source.md"), "- TODO task\n  score:: 2\n").unwrap();
        let graph = Graph::open(&root);
        graph
            .attach_direct_projection(root.join("private/projection.sqlite"))
            .unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        take_registry_read_attempts();
        let result = graph
            .run_query_bounded("(task TODO)", 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        assert_eq!(result.total, 1);
        assert_eq!(
            take_registry_read_attempts(),
            0,
            "a task query must not scan property metadata to check its memo"
        );
        assert!(!graph.has_parsed_cache_test());
    }

    /// **R6 §1, cold.** A fresh projection streams its build: every page is
    /// lowered, no parsed cache is retained, and never more than
    /// `WARM_STREAM_HIGH_WATER` documents wait in the queue.
    #[test]
    fn cold_open_streams_without_retaining_the_graph() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = scratch("cold-stream");
        std::fs::create_dir_all(root.join("pages")).unwrap();
        let pages = 3 * WARM_STREAM_HIGH_WATER + 7;
        for i in 0..pages {
            std::fs::write(
                root.join(format!("pages/p{i:03}.md")),
                format!("- TODO task {i}\n- DONE done {i}\n"),
            )
            .unwrap();
        }
        let database = scratch("cold-stream-db").join("projection.sqlite");
        reset_lowerings(&root);
        MAX_PENDING_DELTAS.store(0, Ordering::Relaxed);
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        assert!(graph.warm_cache_cancellable(|| false));
        wait_ready(&graph);
        assert_eq!(lowerings(), pages as u64);
        assert_eq!(graph.warm_stream_parses_test(), pages);
        assert!(
            !graph.has_parsed_cache_test(),
            "a cold open must stream, not pin the graph"
        );
        assert!(
            MAX_PENDING_DELTAS.load(Ordering::Relaxed) <= WARM_STREAM_HIGH_WATER as u64,
            "the stream ran ahead of the worker: {} queued deltas",
            MAX_PENDING_DELTAS.load(Ordering::Relaxed)
        );
        let oracle = Graph::open(&root);
        assert_eq!(
            signature(
                &graph
                    .run_query_bounded("(task TODO)", 1_000, 8_000_000)
                    .expect("the ready projection answers the public bounded route")
                    .groups
            ),
            signature(
                &crate::query::run_query_bounded(&oracle, "(task TODO)", 1_000, 8_000_000).groups
            )
        );
        assert!(!graph.has_parsed_cache_test());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    /// **R6 §1, one external edit between sessions.** Exactly that page is
    /// parsed and relowered; the parsed cache is never built.
    #[test]
    fn an_external_edit_between_sessions_relowers_one_page_without_a_cache() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = r6_graph("external-edit-stream");
        let database = scratch("external-edit-stream-db").join("projection.sqlite");
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
        }
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(root.join("pages/two.md"), "- TODO two changed\n").unwrap();

        reset_lowerings(&root);
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        assert!(graph.warm_cache_cancellable(|| false));
        wait_ready(&graph);
        assert_eq!(lowerings(), 1);
        assert_eq!(graph.warm_stream_parses_test(), 1);
        assert_eq!(graph.page_build_parses_test(), 0);
        assert!(!graph.has_parsed_cache_test());
        let oracle = Graph::open(&root);
        let indexed = graph
            .run_query_bounded("(task TODO)", 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        assert_eq!(
            signature(&indexed.groups),
            signature(
                &crate::query::run_query_bounded(&oracle, "(task TODO)", 100, 1_000_000).groups
            )
        );
        assert!(indexed.groups.iter().any(|group| group
            .blocks
            .iter()
            .any(|block| block.raw.contains("two changed"))));
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    /// **R6 §1, damaged file.** A projection whose schema is damaged is
    /// recreated and its rebuild streams like a cold open — no parsed cache.
    #[test]
    fn a_damaged_projection_streams_its_rebuild() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = r6_graph("damaged-stream");
        let database = scratch("damaged-stream-db").join("projection.sqlite");
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
        }
        std::thread::sleep(Duration::from_millis(20));
        // Schema damage (a dropped fact table) is the in-scope shape: the open
        // route recreates the file, so the warm meets an empty projection.
        let damaged = rusqlite::Connection::open(&database).unwrap();
        damaged.execute("DROP TABLE block_path_refs", []).unwrap();
        drop(damaged);

        reset_lowerings(&root);
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        assert!(graph.warm_cache_cancellable(|| false));
        wait_ready(&graph);
        assert_eq!(
            lowerings(),
            5,
            "every page is relowered into the recreated file"
        );
        assert_eq!(graph.warm_stream_parses_test(), 5);
        assert!(!graph.has_parsed_cache_test());
        let oracle = Graph::open(&root);
        assert_eq!(
            signature(
                &graph
                    .run_query_bounded("(task TODO)", 100, 1_000_000)
                    .expect("the ready projection answers the public bounded route")
                    .groups
            ),
            signature(
                &crate::query::run_query_bounded(&oracle, "(task TODO)", 100, 1_000_000).groups
            )
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    #[test]
    fn edited_page_reload_and_sql_keep_the_same_session_ids() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = r6_graph("edited-session-reload");
        let database = scratch("edited-session-reload-db").join("projection.sqlite");
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database).unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        let entry = graph
            .list_pages()
            .into_iter()
            .find(|entry| entry.name == "one")
            .unwrap();
        let mut page = graph.load_page(&entry).unwrap();
        let baseline = page.rev.clone();
        let kept = page.blocks[0].clone();
        let mut inserted = kept.clone();
        inserted.id = Uuid::new_v4().to_string();
        inserted.raw = "TODO inserted first".into();
        page.blocks.insert(0, inserted);
        graph.save_page(&page, baseline.as_deref()).unwrap();
        wait_ready(&graph);
        assert!(!graph.has_parsed_cache_test());
        let live = graph
            .run_query_bounded("(task TODO)", 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        let query_id = &live
            .groups
            .iter()
            .flat_map(|group| &group.blocks)
            .find(|block| block.raw == kept.raw)
            .unwrap()
            .id;
        assert_eq!(query_id, &kept.id);
        let reloaded = graph.load_by_path(&entry.rel_path).unwrap().unwrap();
        let reloaded_id = &reloaded
            .blocks
            .iter()
            .find(|block| block.raw == kept.raw)
            .unwrap()
            .id;
        assert_eq!(
            reloaded_id, query_id,
            "reloading an unchanged edited page must retain the IDs SQLite exposes"
        );

        // An incompatible external revision must use that revision's parser
        // identities, even if it happens to have the same tree shape.
        let changed = std::fs::read_to_string(&entry.path)
            .unwrap()
            .replace("inserted first", "external first");
        std::fs::write(&entry.path, changed).unwrap();
        graph.sync_file_checked(&entry.path).unwrap();
        // sync_file's parsed-cache adapter is a no-op without that cache;
        // streamed reconciliation owns the absent-cache path.
        graph.invalidate_cache();
        graph.warm_cache();
        wait_ready(&graph);
        let external = graph.load_by_path(&entry.rel_path).unwrap().unwrap();
        let external_id = &external
            .blocks
            .iter()
            .find(|block| block.raw == kept.raw)
            .unwrap()
            .id;
        assert_ne!(
            external_id, &kept.id,
            "incompatible source does not reuse the old map"
        );
        let sql = graph
            .run_query_bounded("(task TODO)", 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        assert_eq!(
            &sql.groups
                .iter()
                .flat_map(|group| &group.blocks)
                .find(|block| block.raw == kept.raw)
                .unwrap()
                .id,
            external_id
        );
    }

    #[test]
    fn failed_projection_recovery_does_not_build_a_parsed_graph() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = r6_graph("streamed-damage-recovery");
        let database = scratch("streamed-damage-recovery-db").join("projection.sqlite");
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        assert!(!graph.has_parsed_cache_test());
        assert!(graph
            .create_markdown_page_if_absent("aaa-added", "- TODO appended\n")
            .unwrap());
        wait_ready(&graph);
        let query = "(and (task TODO) (not (journal)))";
        // Admission happens before output sorting. A new page appends to this
        // session even when its filename sorts before the existing pages.
        let expected = graph
            .run_query_bounded(query, 1, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        assert_ne!(expected.groups[0].page, "aaa-added");
        // Page creation may have warmed another feature's cache. Evict it
        // before repair so this gate measures recovery's own source ownership.
        graph.invalidate_cache();
        assert!(!graph.has_parsed_cache_test());
        let writer = rusqlite::Connection::open(&database).unwrap();
        writer.execute_batch("DROP TABLE block_text").unwrap();
        drop(writer);
        graph.direct_projection_recover_after_failed_read();
        wait_ready(&graph);
        assert!(
            !graph.has_parsed_cache_test(),
            "projection repair must stream source pages without retaining the parsed graph"
        );
        graph.clear_query_memos_test();
        let before = graph.direct_projection_statement_reads_test();
        let actual = graph
            .run_query_bounded(query, 1, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        assert_eq!(graph.direct_projection_statement_reads_test(), before + 1);
        assert_eq!(signature(&actual.groups), signature(&expected.groups));
    }

    #[test]
    fn failed_projection_writer_can_recover_from_source_inventory() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = r6_graph("failed-writer-stream-recovery");
        let database = scratch("failed-writer-stream-recovery-db").join("projection.sqlite");
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        let entry = graph
            .list_pages()
            .into_iter()
            .find(|entry| entry.name == "one")
            .unwrap();
        let mut page = graph.load_page(&entry).unwrap();
        let baseline = page.rev.clone();
        page.blocks[0].raw = "TODO repaired from acknowledged edit".into();
        let kept_id = page.blocks[0].id.clone();
        let writer = rusqlite::Connection::open(&database).unwrap();
        writer.execute_batch("DROP TABLE block_text").unwrap();
        drop(writer);
        graph.save_page(&page, baseline.as_deref()).unwrap();
        let projection = graph.direct_projection_test().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !projection.shared.worker_failed.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "damaged projection must fail its edit turn"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        graph.direct_projection_recover_after_failed_read();
        wait_ready(&graph);
        assert!(!graph.has_parsed_cache_test());
        let result = graph
            .run_query_bounded("(task TODO)", 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        let block = result
            .groups
            .iter()
            .flat_map(|group| &group.blocks)
            .find(|block| block.id == kept_id)
            .unwrap();
        assert_eq!(block.raw, page.blocks[0].raw);
        assert!(!projection.shared.worker_failed.load(Ordering::Acquire));
    }

    /// Cache eviction does not change an unchanged page's session identities.
    /// The compact session owner survives without retaining parsed documents.
    #[test]
    fn session_identity_survives_parsed_page_eviction() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = r6_graph("identity-eviction");
        let database = scratch("identity-eviction-db").join("projection.sqlite");
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        let projection = graph.direct_projection_test().unwrap();

        let entry = graph
            .list_pages()
            .into_iter()
            .find(|entry| entry.name == "one")
            .unwrap();
        let one = page_id(&entry.rel_path);
        let mut page = graph.load_page(&entry).unwrap();
        let baseline = page.rev.clone();
        let kept = page.blocks[0].clone();
        let mut inserted = kept.clone();
        inserted.id = Uuid::new_v4().to_string();
        inserted.raw = "TODO inserted first".into();
        page.blocks.insert(0, inserted);
        graph.save_page(&page, baseline.as_deref()).unwrap();
        wait_ready(&graph);
        assert!(projection.session_pages_test().contains(&one));
        let live = graph
            .run_query_bounded("(task TODO)", 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        let live_id = live
            .groups
            .iter()
            .flat_map(|group| group.blocks.iter())
            .find(|block| block.raw == "TODO one [[target]]")
            .map(|block| block.id.clone())
            .expect("the moved block answers");
        assert_eq!(
            live_id, kept.id,
            "a live save answers with the preserved id"
        );

        // Losing this disposable source stamp forces a streamed replacement
        // from unchanged bytes, exercising recovery's identity provenance.
        let writer = rusqlite::Connection::open(&database).unwrap();
        assert_eq!(
            writer
                .execute(
                    "DELETE FROM direct_source_revisions WHERE page_id = ?1",
                    rusqlite::params![one.as_slice()]
                )
                .unwrap(),
            1
        );
        drop(writer);
        let parses = graph.warm_stream_parses_test();
        graph.invalidate_cache();
        assert!(
            projection.session_pages_test().contains(&one),
            "dropping the parsed cache must retain compatible live IDs"
        );
        graph.warm_cache();
        wait_ready(&graph);
        assert_eq!(graph.warm_stream_parses_test(), parses + 1);
        assert!(!graph.has_parsed_cache_test());
        let statements = graph.direct_projection_statement_reads_test();
        let after = graph
            .run_query_bounded("(task TODO)", 100, 1_000_000)
            .expect("the ready projection answers the public bounded route");
        assert_eq!(
            graph.direct_projection_statement_reads_test(),
            statements + 1
        );
        assert_eq!(
            signature(&after.groups),
            signature(&live.groups),
            "cache eviction preserves the same session's complete result"
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    /// **R6 §3, unit.** A structural relowering removes the page from the
    /// session set exactly as a deletion does.
    #[test]
    fn a_structural_relower_drops_the_session_identity() {
        let shared = ProjectionShared {
            path: PathBuf::from("unused"),
            pending: Mutex::new(PendingProjection::default()),
            changed: Condvar::new(),
            ready: AtomicBool::new(false),
            ready_generation: AtomicU64::new(0),
            reader: Mutex::new(None),
            statement_seam: Mutex::new(None),
            query_jobs: QueryJobOwner::new(DEFAULT_QUERY_JOB_CAPACITY),
            session_pages: Mutex::new(Arc::new(HashSet::new())),
            fts_ready_at: AtomicU64::new(0),
            fts_ever_ready: AtomicBool::new(false),
            worker_available: AtomicBool::new(true),
            worker_failed: AtomicBool::new(false),
            worker_busy: AtomicBool::new(false),
            worker_finished: AtomicBool::new(false),
            worker_resources: Mutex::new(Some(Vec::new())),
            validated: AtomicBool::new(false),
            indexed_reads: AtomicU64::new(0),
            statement_reads: AtomicU64::new(0),
            inject_read_failure: AtomicBool::new(false),
            fallback_reads: AtomicU64::new(0),
            referenced_name_reads: AtomicU64::new(0),
            fuzzy_candidate_reads: AtomicU64::new(0),
        };
        let a = page_id("pages/a.md");
        let b = page_id("pages/b.md");
        shared.record_session_pages(&AppliedPages {
            lowered: vec![a, b],
            ..AppliedPages::default()
        });
        assert_eq!(
            **shared.session_pages.lock().unwrap(),
            HashSet::from([a, b])
        );
        shared.record_session_pages(&AppliedPages {
            relowered_structurally: vec![a],
            ..AppliedPages::default()
        });
        assert_eq!(**shared.session_pages.lock().unwrap(), HashSet::from([b]));
    }

    /// **R6 §2.** Backlinks on a warm, cache-less graph hydrate exactly the
    /// projection's candidates from disk and build no whole-graph cache.
    #[test]
    fn reference_hydration_without_a_cache_parses_only_candidates() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = r6_graph("hydration");
        let database = scratch("hydration-db").join("projection.sqlite");
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
        }
        std::thread::sleep(Duration::from_millis(20));
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        assert!(graph.warm_cache_cancellable(|| false));
        wait_ready(&graph);
        graph.reset_direct_projection_candidate_probe_test();
        let backlinks = crate::query::backlinks(&graph, "target");
        let oracle = Graph::open(&root);
        assert_eq!(
            signature(&backlinks),
            signature(&crate::query::backlinks(&oracle, "target"))
        );
        assert_eq!(
            graph.direct_projection_hydrated_pages_test(),
            vec![PathBuf::from("pages/one.md")]
        );
        assert_eq!(graph.on_demand_parses_test(), 1);
        assert_eq!(graph.page_build_parses_test(), 0);
        assert!(!graph.has_parsed_cache_test());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    /// **R6 §2.** `list_pages` is served from the projection inventory when
    /// it is ready: no parse, no cache, and the same effective entries as the
    /// cold whole-graph listing — a `title::` page and a journal's sort key
    /// included.
    #[test]
    fn list_pages_is_served_from_the_projection_when_ready() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = r6_graph("list-pages");
        let database = scratch("list-pages-db").join("projection.sqlite");
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
        }
        std::thread::sleep(Duration::from_millis(20));
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        assert!(graph.warm_cache_cancellable(|| false));
        wait_ready(&graph);
        let listed = graph.list_pages();
        assert_eq!(graph.page_build_parses_test(), 0);
        assert!(!graph.has_parsed_cache_test());
        let oracle = Graph::open(&root);
        assert_eq!(
            entry_signature(&listed),
            entry_signature(&oracle.list_pages())
        );
        assert!(listed
            .iter()
            .any(|entry| entry.name == "Titled Page" && entry.rel_path == "pages/titled.md"));
        assert!(listed.iter().any(|entry| {
            entry.kind == PageKind::Journal
                && entry.date_key == Some(20260906)
                && entry.rel_path == "journals/2026_09_06.md"
        }));
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    #[test]
    #[ignore = "manual storage packet receipt; set TINE_DIRECT_PROJECTION_CORPUS"]
    fn real_corpus_clean_reopen_reuses_projected_pages() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = PathBuf::from(
            std::env::var("TINE_DIRECT_PROJECTION_CORPUS")
                .expect("TINE_DIRECT_PROJECTION_CORPUS is required"),
        );
        let database = scratch("real-corpus-reopen").join("projection.sqlite");
        {
            let graph = Graph::open(&root);
            graph.attach_direct_projection(database.clone()).unwrap();
            graph.warm_cache();
            wait_ready(&graph);
        }
        std::thread::sleep(Duration::from_millis(20));

        reset_lowerings(&root);
        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        let started = Instant::now();
        graph.warm_cache();
        let warm = started.elapsed();
        wait_ready(&graph);
        let converged = started.elapsed();
        // R6: the warm validated from bytes alone — no page parsed, no cache.
        let parses = graph.page_build_parses_test() + graph.warm_stream_parses_test();
        assert_eq!(parses, 0, "clean reopen must not parse any page");
        assert!(!graph.has_parsed_cache_test());
        let query_started = Instant::now();
        let indexed = graph
            .run_query_bounded("(task TODO)", 20_000, 32 << 20)
            .expect("the ready projection answers the public bounded route");
        let indexed_elapsed = query_started.elapsed();
        let oracle = crate::query::run_query_bounded(&graph, "(task TODO)", 20_000, 32 << 20);
        assert_eq!(signature(&indexed.groups), signature(&oracle.groups));
        assert_eq!(
            lowerings(),
            0,
            "clean reopen must not lower unchanged pages"
        );
        eprintln!(
            "direct projection clean-reopen receipt: warm_ms={} warm_validate_ms={} projection_total_ms={} projection_tail_ms={} indexed_query_us={} pages_lowered={} pages_parsed={}",
            warm.as_millis(),
            warm.as_millis(),
            converged.as_millis(),
            converged.saturating_sub(warm).as_millis(),
            indexed_elapsed.as_micros(),
            lowerings(),
            parses,
        );
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    #[test]
    #[ignore = "manual storage packet receipt; set TINE_DIRECT_PROJECTION_CORPUS"]
    fn real_corpus_reference_family_matches_parser_oracle() {
        let _serial = PROJECTION_TEST_LOCK.lock().unwrap();
        let root = PathBuf::from(
            std::env::var("TINE_DIRECT_PROJECTION_CORPUS")
                .expect("TINE_DIRECT_PROJECTION_CORPUS is required"),
        );
        let database = scratch("real-corpus-reference-family").join("projection.sqlite");
        let oracle = Graph::open(&root);
        oracle.warm_cache();
        let aliases = crate::query::page_aliases_with_owners(&oracle);
        let alias_target = aliases.first().map(|(alias, _, _)| alias.clone());
        let oracle_backlinks = alias_target
            .as_deref()
            .map(|target| crate::query::backlinks(&oracle, target));
        let oracle_unlinked_started = Instant::now();
        let oracle_unlinked = alias_target
            .as_deref()
            .map(|target| crate::query::unlinked_refs(&oracle, target));
        let oracle_unlinked_elapsed = oracle_unlinked_started.elapsed();
        let oracle_count_started = Instant::now();
        let oracle_counts = oracle.block_ref_counts().unwrap();
        let oracle_count_elapsed = oracle_count_started.elapsed();
        let block_claim = oracle.with_pages(|pages| {
            pages.iter().find_map(|(_, document)| {
                let mut claim = None;
                fn visit(blocks: &[DocBlock], claim: &mut Option<String>) {
                    for block in blocks {
                        if claim.is_none() {
                            *claim = block.projection().block_refs.first().cloned();
                        }
                        visit(&block.children, claim);
                    }
                }
                visit(&document.roots, &mut claim);
                claim
            })
        });
        let oracle_referrers = block_claim
            .as_deref()
            .map(|claim| crate::query::block_referrers(&oracle, claim));
        let oracle_resolved = block_claim
            .as_deref()
            .and_then(|claim| crate::query::resolve_block(&oracle, claim));

        let graph = Graph::open(&root);
        graph.attach_direct_projection(database.clone()).unwrap();
        graph.warm_cache();
        wait_ready(&graph);
        assert_eq!(graph.page_aliases_with_owners(), aliases);
        let projected_count_started = Instant::now();
        let projected_counts = graph.block_ref_counts().unwrap();
        let projected_count_elapsed = projected_count_started.elapsed();
        assert_eq!(projected_counts.as_ref(), oracle_counts.as_ref());
        eprintln!(
            "real-corpus-reference counts={} parser_count_us={} sqlite_count_us={}",
            projected_counts.len(),
            oracle_count_elapsed.as_micros(),
            projected_count_elapsed.as_micros(),
        );
        if let Some(target) = alias_target.as_deref() {
            let indexed_unlinked_started = Instant::now();
            let indexed_unlinked = crate::query::unlinked_refs(&graph, target);
            let indexed_unlinked_elapsed = indexed_unlinked_started.elapsed();
            assert_eq!(
                signature(&crate::query::backlinks(&graph, target)),
                signature(oracle_backlinks.as_deref().unwrap())
            );
            assert_eq!(
                signature(&indexed_unlinked),
                signature(oracle_unlinked.as_deref().unwrap())
            );
            let candidates = graph.reference_candidate_pages(
                &[crate::refs::page_key(target)],
                ReferenceKind::Explicit,
            );
            assert!(candidates.indexed);
            eprintln!(
                "real-corpus-reference explicit_candidates={} full_pages={} parser_unlinked_us={} indexed_unlinked_us={}",
                candidates.pages.len(),
                candidates.full_page_count,
                oracle_unlinked_elapsed.as_micros(),
                indexed_unlinked_elapsed.as_micros(),
            );
        }
        if let Some(claim) = block_claim.as_deref() {
            assert_eq!(
                signature(&crate::query::block_referrers(&graph, claim)),
                signature(oracle_referrers.as_deref().unwrap())
            );
            assert_eq!(
                crate::query::resolve_block(&graph, claim)
                    .as_ref()
                    .map(|group| signature(std::slice::from_ref(group))),
                oracle_resolved
                    .as_ref()
                    .map(|group| signature(std::slice::from_ref(group)))
            );
        }
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
    }

    /// Child half of the two `retired_class_c_*` probes. Emits BOTH retired
    /// class-(c) reports, each with its own planted marker, through the exact
    /// production reporter and the exact error types the call sites hand it.
    #[test]
    #[ignore = "child process for the retired class-(c) stderr probe"]
    fn w4_i5b_projection_failure_marker_child() {
        if std::env::var("TINE_I5B_SET_FLAG").as_deref() == Ok("1") {
            crate::sync_runtime::set_runtime_debug_diagnostics(true);
        }
        // Exactly what `open_projection_database` returns: a free-form
        // `MaterializationError` payload.
        report_projection_failure(
            "disabled: its database could not be opened",
            &tine_storage::sqlite::MaterializationError::Sqlite(
                "planted-open-marker-Zq7Page".to_owned(),
            ),
        );
        // Exactly what `apply_pending` returns: a `String` naming the
        // graph-relative page it was projecting.
        report_projection_failure(
            PROJECTION_UPDATE_FAILURE,
            &"parsed page has no exact source revision: pages/planted-apply-marker-Zq7Page.md"
                .to_owned(),
        );
    }

    fn projection_failure_child_stderr(set_flag: &str) -> String {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "direct_projection::tests::w4_i5b_projection_failure_marker_child",
                "--nocapture",
            ])
            .env_remove("TINE_DEBUG")
            .env("TINE_I5B_SET_FLAG", set_flag)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "projection-failure child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// I-5, retired class-(c) row `direct_projection.rs` "projection database
    /// could not be opened": the always-on line carried a free-form
    /// `MaterializationError` payload.
    #[test]
    fn retired_class_c_projection_database_open_emits_no_planted_marker() {
        let marker = "planted-open-marker-Zq7Page";
        assert!(
            !projection_failure_child_stderr("0").contains(marker),
            "I-5: the always-on projection-open failure still carried its error prose. \
             The always-on line names the failure family only; the detail belongs behind \
             `runtime_debug_diagnostics_enabled()` (I-9 keeps the family, not the prose)."
        );
        assert!(
            projection_failure_child_stderr("1").contains(marker),
            "the directed debug channel must still carry the detail, or this probe proves \
             nothing about where the prose went"
        );
    }

    /// I-5, retired class-(c) row `direct_projection.rs` "projection is stale;
    /// using parser fallback": `apply_pending` formats the graph-relative page
    /// path into the error this line used to print always-on.
    #[test]
    fn retired_class_c_projection_apply_failure_emits_no_planted_marker() {
        let marker = "planted-apply-marker-Zq7Page";
        let ordinary = projection_failure_child_stderr("0");
        assert!(ordinary.contains(PROJECTION_UPDATE_FAILURE));
        assert!(!ordinary.contains("using parser fallback"));
        assert!(
            source_of_this_file().contains("parsed page has no exact source revision: {}"),
            "non-vacuity: this probe exists because `apply_pending` names the page it was \
             projecting in its error string. If that error no longer does, re-derive the row's \
             class before relaxing the probe."
        );
        assert!(
            !ordinary.contains(marker),
            "I-5: the always-on parser-fallback line still carried the graph-relative page \
             path from `apply_pending`. The always-on line names the failure family only."
        );
        assert!(
            projection_failure_child_stderr("1").contains(marker),
            "the directed debug channel must still carry the detail, or this probe proves \
             nothing about where the prose went"
        );
    }

    fn source_of_this_file() -> String {
        std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/direct_projection.rs"),
        )
        .unwrap()
    }
}
