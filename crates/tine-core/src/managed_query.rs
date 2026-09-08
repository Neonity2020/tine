//! R4: the Managed Storage accepted-frontier query route.
//!
//! A simple query against a Managed graph whose actor holds NO pending local
//! suffix is answered the way a ready Direct Files query is (R3): from ONE
//! owned read snapshot of the accepted projection, through
//! `query::results::read_results`, with no page document loaded and no source
//! text read. The difference is WHERE it runs. The Managed projection is owned
//! by the sync actor, so the actor turn is kept short — it validates the turn,
//! consults the memo and, on a miss, CAPTURES the immutable inputs below — and
//! the read itself runs on the calling thread after the handle has released
//! its `operation` mutex. The actor keeps serving saves and navigation while
//! the statement runs; `QueryJobOwner` bounds how many run at once and drains
//! them before the actor removes, replaces, reopens or resets the file.
//!
//! **R5a/R5c: an actor holding a pending local suffix is answered here too.**
//! A captured pending query — by the turn rule, one whose relations all stay
//! inside a page — is answered OFF the actor
//! from TWO owned snapshots: the pending overlay at the flushed state the
//! capture required (or a later one), and the accepted projection validated
//! against the captured stamp with every pending page MASKED out of its
//! statement. The two descriptor streams are merged in the walk's base order
//! under ONE construction budget
//! ([`crate::query::results::read_results_merged`]), so the answer is exactly
//! what the actor walk would have produced over the same pending state — same
//! rows, same order, same `total` and `exceeded`, same public ids — and it
//! still loads no page document and parses nothing. Traversal remains only as
//! the independent test oracle; readiness and cancellation are typed outcomes.
//!
//! R5c lifts the last pending exclusion: a query with a property leaf is
//! captured too, and the registry it is lowered under is the actor's ACCEPTED
//! table patched here, off the actor, over exactly the keys the pending pages
//! can have changed (`crate::managed_registry_patch`).
//!
//! Ownership (R4/R5 dossiers): this file's types, the shared memo, the census
//! and the capture/reply/drain wiring in `sync_runtime.rs` are the manager's
//! (R4b/R5b); the body of [`execute_managed_query`] and its tests are the
//! R4a/R5a lanes'.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tine_storage::sqlite::{
    MaterializationError, PhysicalProjectionQuerySnapshot, PhysicalQueryValue,
};

use crate::config::ParseConfig;
use crate::date::{JournalDate, JournalFormat};
use crate::model::{PageKind, RefGroup};
use crate::oplog::ContentDigest;
use crate::query::ir::{Query, ViewSettings};
use crate::query::registry::Registry;
use crate::query::results::{
    read_page_results, read_page_results_merged, read_results, read_results_merged, BackendOrder,
    RecencyPage, ResultIdentity, ResultReadError, ResultReadInputs, ResultReadShared, ResultSource,
};
use crate::query::sql::{lower_query, LoweringInputs, RESULT_SET_RULE};
use crate::query::{ConstructionProfile, PreViewGroups};
use crate::query_jobs::{Admission, JobSlot, QueryJobOwner};

/// The ONE definition of "the full-text index is ready to be queried": the row
/// the FTS builder stamps when its build completed at this projection's
/// frontier. Both backends probe it with this exact statement
/// (`direct_projection::probe_fts_ready` and the Managed executor); a second
/// spelling would be a second definition.
pub(crate) const FTS_READY_PROBE_SQL: &str =
    "SELECT phase FROM search_fts_build WHERE singleton = 1";

/// The graph-wide half of a Managed answer's cache identity (C6/E6), and the
/// stamp the executor validates its snapshot against.
///
/// `acceptance_sequence` + `frontier_digest` are what
/// `PhysicalProjectionQuerySnapshot::open_managed` checks against the file's
/// `materialization_stamp`; a projection that has moved on answers `Stale` and
/// the handle re-captures. `config_digest` is carried unconditionally: the
/// journal title format decides a page's kind and day, so even a query with
/// no property leaf is config-sensitive, and a config edit never moves the
/// acceptance sequence. `today` is the execution day (§4.4): `(between …)`
/// resolves against it, so an answer from yesterday is not today's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedQueryStamp {
    pub(crate) acceptance_sequence: u64,
    pub(crate) frontier_digest: ContentDigest,
    pub(crate) config_digest: ContentDigest,
    pub(crate) today: i64,
    /// R5b: the pending overlay's latest revision when the actor holds an
    /// undrained local suffix, `None` when the accepted frontier is the whole
    /// story. Every pending save moves it, so a memo entry or a capture is
    /// exact for the pending state it was taken under.
    pub(crate) overlay_revision: Option<u64>,
    /// Rebuilding restarts revision numbering. The instance disambiguates
    /// equal revisions of different files in both result and registry memos.
    pub(crate) overlay_instance: Option<u64>,
}

/// The pending half of a capture (R5b): the overlay the executor opens and
/// the revision it must carry before the executor may read it.
#[derive(Clone)]
pub(crate) struct PendingOverlayCapture {
    pub(crate) required_revision: u64,
    pub(crate) overlay: Arc<crate::managed_overlay::PendingOverlay>,
}

/// The immutable inputs one actor turn captures for an accepted-frontier
/// query. Everything the executor reads is HERE; it touches no actor state,
/// no graph mutex and no live registry after the turn ends.
pub(crate) struct ManagedQueryCapture {
    /// The accepted projection's SQLite file.
    pub(crate) path: PathBuf,
    /// The pending overlay to merge with, when the actor held a pending suffix.
    pub(crate) overlay: Option<PendingOverlayCapture>,
    /// The graph root the projection's relative page paths hang off (recency).
    pub(crate) graph_root: PathBuf,
    pub(crate) stamp: ManagedQueryStamp,
    pub(crate) config: ParseConfig,
    /// The graph's journal title format: the Managed walk's recency producer
    /// is `JournalFormat::page_recency_secs(kind == Journal, name, path)` —
    /// by the page's NAME, `i64::MIN` when it does not parse — and the
    /// executor must produce exactly that (R4 verification D4).
    pub(crate) journal_format: JournalFormat,
    /// The property registry the query is lowered under. Built only when the
    /// query has a `props` leaf; otherwise the empty registry (C6).
    pub(crate) registry: Arc<Registry>,
    /// The registry generation the answer will be memoized under (0 when the
    /// query has no `props` leaf).
    pub(crate) registry_generation: u64,
    pub(crate) props: bool,
    pub(crate) query: Query,
    pub(crate) view: ViewSettings,
    pub(crate) today: JournalDate,
    pub(crate) max_rows: usize,
    pub(crate) max_bytes: usize,
    pub(crate) profile: ConstructionProfile,
    /// `query::simple_query_cache_key` of the resolved IR and bounds — the
    /// memo key, so two spellings of one query share one entry (I-12).
    pub(crate) key: String,
    /// WHICH answer this capture is for (RET1). The immutable inputs above are
    /// the same for all three; only the row shape and, for an explanation, the
    /// probe decomposition differ.
    pub(crate) request: ManagedQueryRequest,
    /// §4.4's support report for the binding this capture was taken under.
    ///
    /// It travels with the capture rather than with the rows because it is a
    /// property of HOW the source was bound: a memoized row set may be shared
    /// by two executions with different reports, so the report may never be
    /// memoized beside it.
    pub(crate) report: crate::query::ir::QueryReport,
}

/// Which answer one captured Managed execution produces (RET1).
///
/// One capture shape, three row shapes. `Blocks` is the accepted-frontier
/// simple-query route R4 shipped and the public `@block` IR command; `Pages` is
/// the public `@page` IR command, whose rows are the page index and never a
/// loaded document (K16); `Counts` is `query_explain_empty`'s probe
/// decomposition, counted over the SAME snapshots as the answer would be.
#[derive(Clone, Debug)]
pub(crate) enum ManagedQueryRequest {
    Blocks,
    Pages,
    /// The probe queries `query::view::explain_empty_plan` decomposed, in the
    /// order it needs them counted.
    Counts(Vec<Query>),
}

/// What a captured Managed execution answered, by request.
#[derive(Debug)]
pub(crate) enum ManagedQueryAnswer {
    Blocks(PreViewGroups),
    Pages(crate::query::results::PageAnswer),
    Counts(Vec<usize>),
}

impl ManagedQueryAnswer {
    /// The block answer, or `None` when the executor answered another request
    /// than the caller captured — structurally impossible, and therefore
    /// classified by the caller rather than panicked on.
    pub(crate) fn into_blocks(self) -> Option<PreViewGroups> {
        match self {
            ManagedQueryAnswer::Blocks(pre) => Some(pre),
            _ => None,
        }
    }

    pub(crate) fn into_pages(self) -> Option<crate::query::results::PageAnswer> {
        match self {
            ManagedQueryAnswer::Pages(pages) => Some(pages),
            _ => None,
        }
    }

    pub(crate) fn into_counts(self) -> Option<Vec<usize>> {
        match self {
            ManagedQueryAnswer::Counts(counts) => Some(counts),
            _ => None,
        }
    }
}

/// What one execution attempt produced. There is no fifth state: an attempt
/// answers, or the handle re-captures (`Stale`), or the public route reports a
/// typed `query::QueryExecutionError` (RET2 — the walk that used to answer the
/// remaining states is gone). The doc comments on the variants below are the
/// EXECUTOR's view; `sync_runtime::managed_execution_error` owns how each one
/// is classified for a caller.
#[derive(Debug)]
pub(crate) enum ManagedQueryOutcome {
    /// The statement answered from the snapshot; pre-view, un-ordered.
    Answered(ManagedQueryAnswer),
    /// The file's stamp no longer matches the capture: an accepted batch
    /// landed between the turn and the open. Not a failure — re-capture.
    Stale,
    /// No slot freed, or the pending overlay has not flushed within the wait.
    Busy,
    /// The owner cancelled the job (a drain before a file replacement, or
    /// close). Nothing is counted and nothing is recovered.
    Cancelled,
    /// A read was attempted and did not answer: an unopenable file, a seam
    /// refusal or a projection that contradicts itself. The reason names a
    /// column or table CLASS, never a value (I-5). SPEC §5.9 M10: a failed
    /// Managed read surfaces as an error, exactly as a failed materialized
    /// read does today; there is no walk fallback for it. Counted.
    Failed(&'static str),
}

/// Test-visible counters for the accepted route, owned by the handle so they
/// can be read without an actor turn. Always compiled: three relaxed atomics
/// cost nothing and keep the production and test wiring identical.
#[derive(Debug, Default)]
pub(crate) struct ManagedQueryCensus {
    /// Executions that opened a snapshot and ran the descriptor statement.
    pub(crate) statement_reads: AtomicUsize,
    /// The subset of `statement_reads` that were PENDING reads: two snapshots,
    /// the overlay merged with the masked accepted projection (R5a). Noted
    /// BESIDE `statement_reads`, never instead of it, so the existing census
    /// assertions keep meaning "one database read".
    pub(crate) pending_reads: AtomicUsize,
    /// Queries the walk answered because the executor reported `Busy`, or
    /// `Stale` more times than the handle re-captures.
    ///
    /// **RET2 left this counter with no producer, deliberately.** The public
    /// Managed routes no longer have a walk to fall back to: `Busy` and an
    /// exhausted re-capture are now typed `NotReady` answers the frontend
    /// retries. The field stays because it is the WITNESS — every gate that
    /// asserts a census reads it, and a future packet that quietly reconnects
    /// the oracle would have to increment it here first.
    pub(crate) fallback_reads: AtomicUsize,
    /// Executions that reported `Failed`; the caller received an error.
    pub(crate) failed_reads: AtomicUsize,
    /// Re-captures after a `Stale` execution.
    pub(crate) stale_recaptures: AtomicUsize,
    /// R5c: pending property-registry patches actually COMPUTED off the actor
    /// (a patched-registry cache hit costs nothing and is not counted). One
    /// per distinct pending state per property query, never one per read.
    pub(crate) registry_patches: AtomicUsize,
}

/// A copy of [`ManagedQueryCensus`] for assertions.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManagedQueryCensusSnapshot {
    pub(crate) statement_reads: usize,
    pub(crate) pending_reads: usize,
    pub(crate) fallback_reads: usize,
    pub(crate) failed_reads: usize,
    pub(crate) stale_recaptures: usize,
    pub(crate) registry_patches: usize,
}

impl ManagedQueryCensus {
    pub(crate) fn note_statement_read(&self) {
        self.statement_reads.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_pending_read(&self) {
        self.pending_reads.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_failed_read(&self) {
        self.failed_reads.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_stale_recapture(&self) {
        self.stale_recaptures.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_registry_patch(&self) {
        self.registry_patches.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> ManagedQueryCensusSnapshot {
        ManagedQueryCensusSnapshot {
            statement_reads: self.statement_reads.load(Ordering::Relaxed),
            pending_reads: self.pending_reads.load(Ordering::Relaxed),
            fallback_reads: self.fallback_reads.load(Ordering::Relaxed),
            failed_reads: self.failed_reads.load(Ordering::Relaxed),
            stale_recaptures: self.stale_recaptures.load(Ordering::Relaxed),
            registry_patches: self.registry_patches.load(Ordering::Relaxed),
        }
    }

    #[cfg(test)]
    pub(crate) fn reset(&self) {
        self.statement_reads.store(0, Ordering::Relaxed);
        self.pending_reads.store(0, Ordering::Relaxed);
        self.fallback_reads.store(0, Ordering::Relaxed);
        self.failed_reads.store(0, Ordering::Relaxed);
        self.stale_recaptures.store(0, Ordering::Relaxed);
        self.registry_patches.store(0, Ordering::Relaxed);
    }
}

/// How many times the handle re-captures after a `Stale` execution before it
/// reports pending readiness. Two accepted batches landing inside one query's capture
/// window is a burst; three is a runtime that is not going to settle for this
/// answer.
pub(crate) const MAX_STALE_RECAPTURES: usize = 2;

/// Execute one captured accepted-frontier query on the CALLING thread.
///
/// The contract (R4 dossier, "The executor's contract"): capacity before any
/// transaction (`owner.acquire_within(wait)`), `open_managed` validated
/// against the stamp, the FTS probe, `lower_query` with no masked pages,
/// `read_results` with `BackendOrder::Managed` + `ResultIdentity::Stored`,
/// snapshot dropped before the slot. Never holds the actor, `operation`, or
/// any graph mutex; never spawns a thread; never memoizes. `wait` is the
/// owner's bounded slot wait — `QUERY_JOB_WAIT` in production, shorter under
/// a test that exercises `Busy`.
///
/// It loads no page document and reads no source text: an answer is one stamp
/// validation, one FTS probe, one descriptor statement and the payload batches
/// `read_results` charges for the rows it admitted. The only filesystem work is
/// the recency `stat` of a non-journal page the answer already admitted.
pub(crate) fn execute_managed_query(
    capture: &ManagedQueryCapture,
    owner: &QueryJobOwner,
    census: &ManagedQueryCensus,
    patched: &PatchedRegistryCache,
    wait: Duration,
) -> ManagedQueryOutcome {
    // Capacity BEFORE any transaction (plan §2B): a job waiting for a slot
    // holds its request intent and pins no WAL page.
    let slot = match owner.acquire_within(wait) {
        Admission::Slot(slot) => slot,
        Admission::Busy => return ManagedQueryOutcome::Busy,
        Admission::Cancelled => return ManagedQueryOutcome::Cancelled,
    };
    // The snapshot lives in the inner call, so its `Drop` ends the read
    // transaction BEFORE `slot`'s `Drop` releases the capacity: a drain that
    // observes a free slot can never still be waiting on this transaction.
    let outcome = execute_on_slot(capture, &slot, census, patched);
    drop(slot);
    outcome
}

/// The read itself, with the slot already held.
fn execute_on_slot(
    capture: &ManagedQueryCapture,
    slot: &JobSlot<'_>,
    census: &ManagedQueryCensus,
    patched: &PatchedRegistryCache,
) -> ManagedQueryOutcome {
    #[cfg(test)]
    run_before_managed_open_hook();
    // R5a: a capture taken while the actor held a pending local suffix is
    // answered from TWO snapshots. It stays BELOW the hook, so every barrier
    // gate still runs before the first snapshot of either file.
    if let Some(pending) = capture.overlay.as_ref() {
        return execute_pending_on_slot(capture, pending, slot, census, patched);
    }
    // The stamp is validated INSIDE the read transaction that will serve every
    // later statement, so an accepted batch cannot land between the check and
    // the rows. A projection that has moved on is `Stale`, not a failure: the
    // handle re-captures against the actor's new stamp.
    let mut snapshot = match PhysicalProjectionQuerySnapshot::open_managed(
        &capture.path,
        capture.stamp.acceptance_sequence,
        capture.stamp.frontier_digest,
    ) {
        Ok(snapshot) => snapshot,
        Err(MaterializationError::Stale { .. }) => return ManagedQueryOutcome::Stale,
        Err(_) => return ManagedQueryOutcome::Failed("managed projection snapshot"),
    };
    // Registered exactly as `DirectProjection::open_query_job` registers: a job
    // admitted before a drain but opening after it is cancelled here, so no
    // reader retains a handle to a file that is about to be replaced (I-21).
    if !slot.register(snapshot.cancellation()) {
        return ManagedQueryOutcome::Cancelled;
    }
    let fts_ready = match probe_fts_ready(&mut snapshot) {
        Ok(ready) => ready,
        Err(outcome) => return outcome,
    };
    // ONE lowering per selection this request needs, all under the capture's
    // single registry and its ONE execution day. `block_anchored_query` is the
    // same rebase Direct lowers through, never a second one; an `@page` request
    // keeps its own anchor, because a page answer IS the page index.
    //
    // Empty `masked_pages` BY CONSTRUCTION: this arm runs only when the actor
    // held no pending local suffix (the pending route is
    // `execute_pending_on_slot`).
    //
    // NOT `max_rows` as a cutoff: `total` is the number of matches SEEN for a
    // block answer, and the row AFTER the cap is what decides `exceeded` for a
    // page answer, so a `LIMIT` in the statement would corrupt both.
    let lower = |query: &Query| {
        let query = anchored_for(query, &capture.request);
        let compiled = crate::query::eval::CompiledLeaves::for_query(&query.evaluable_filter());
        let statement = lower_query(
            &query,
            &LoweringInputs {
                today: capture.today,
                registry: &capture.registry,
                masked_pages: &[],
                cutoff: None,
                compiled: &compiled,
                fts_ready,
                result_set_rule: RESULT_SET_RULE,
            },
        );
        (query.anchor, statement)
    };
    // The recency axis is the Managed WALK's producer
    // (`application_query_page_recency`), over the descriptor row: a journal
    // page by its display NAME's date (`i64::MIN` when the name does not
    // parse), any other page by the file's mtime. Deliberately NOT
    // `page_recency_secs_for(journal_day, …)`: the stored `journal_day` comes
    // from the file stem and the walk reads the display name, and a Managed
    // journal page whose two disagree is legal. Parity is with the walk.
    let recency = |page: RecencyPage<'_>| {
        capture.journal_format.page_recency_secs(
            page.kind == PageKind::Journal,
            page.name,
            &capture.graph_root.join(page.path),
        )
    };
    // The ROW SHAPE is the lowered query's anchor and nothing else, so a
    // page-anchored explanation probe counts page rows exactly as the page
    // answer itself does.
    let read = |snapshot: &mut PhysicalProjectionQuerySnapshot,
                anchor: crate::query::ir::Anchor,
                statement: &crate::query::sql::SqlQuery|
     -> Result<ManagedQueryAnswer, ResultReadError> {
        // A filter that folded to false has its answer already (§3.5, I-15).
        // Lowering can only happen after the snapshot is open, because
        // `fts_ready` is a property of THIS snapshot, so an empty answer pays
        // one transaction here where Direct pays none. That is the price of
        // validating the stamp inside the read, not a regression.
        if statement.matches_nothing {
            return Ok(empty_answer(anchor));
        }
        match anchor {
            crate::query::ir::Anchor::Page => Ok(ManagedQueryAnswer::Pages(read_page_results(
                snapshot,
                statement,
                BackendOrder::Managed,
                capture.max_rows,
            )?)),
            crate::query::ir::Anchor::Block => Ok(ManagedQueryAnswer::Blocks(read_results(
                snapshot,
                &ResultReadInputs {
                    statement,
                    order: BackendOrder::Managed,
                    identity: &ResultIdentity::Stored,
                    max_rows: capture.max_rows,
                    max_bytes: capture.max_bytes,
                    profile: capture.profile,
                    recency: &recency,
                },
            )?)),
        }
    };
    let answer = match &capture.request {
        ManagedQueryRequest::Counts(probes) => {
            let mut counts = Vec::with_capacity(probes.len());
            let mut failure = None;
            for probe in probes {
                let (anchor, statement) = lower(probe);
                match read(&mut snapshot, anchor, &statement) {
                    Ok(answer) => counts.push(answer_total(&answer)),
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            match failure {
                Some(error) => Err(error),
                None => Ok(ManagedQueryAnswer::Counts(counts)),
            }
        }
        _ => {
            let (anchor, statement) = lower(&capture.query);
            read(&mut snapshot, anchor, &statement)
        }
    };
    let outcome = match answer {
        Ok(answer) => {
            census.note_statement_read();
            ManagedQueryOutcome::Answered(answer)
        }
        Err(ResultReadError::Cancelled) => ManagedQueryOutcome::Cancelled,
        // A seam refusal or a projection that contradicts itself: the read was
        // attempted and did not answer (D-3). The reason names a table class,
        // never a value (I-5).
        Err(ResultReadError::Sql(_)) => ManagedQueryOutcome::Failed("managed projection statement"),
        Err(ResultReadError::Corrupt(_)) => {
            ManagedQueryOutcome::Failed("managed projection result rows")
        }
    };
    drop(snapshot);
    outcome
}

/// The tree one Managed selection lowers, and therefore the ROW SHAPE it
/// produces. The REQUEST decides it, never the incoming anchor:
///
/// * `Blocks` always rebases (`block_anchored_query`). A `{{query …}}` source
///   may parse to an `@page` anchor, and the simple-query route has always
///   answered it as block groups — rebasing is that behaviour, and reading the
///   raw anchor here would silently turn those shapes into page rows.
/// * `Pages` is §7.1's `@page` answer and keeps its anchor.
/// * `Counts` follows the PROBE's own anchor, which is exactly what the oracle
///   (`run_query_result_over`) does per probe, so a `@page` explanation counts
///   page rows and a `@block` one counts block matches.
fn anchored_for(query: &Query, request: &ManagedQueryRequest) -> Query {
    match (request, query.anchor) {
        (ManagedQueryRequest::Pages, _)
        | (ManagedQueryRequest::Counts(_), crate::query::ir::Anchor::Page) => query.clone(),
        _ => crate::query::block_anchored_query(query),
    }
}

/// The empty answer of the shape this anchor produces.
fn empty_answer(anchor: crate::query::ir::Anchor) -> ManagedQueryAnswer {
    match anchor {
        crate::query::ir::Anchor::Page => {
            ManagedQueryAnswer::Pages(crate::query::results::PageAnswer::default())
        }
        crate::query::ir::Anchor::Block => ManagedQueryAnswer::Blocks(PreViewGroups::default()),
    }
}

/// The `total` one probe answer contributes to an explanation.
fn answer_total(answer: &ManagedQueryAnswer) -> usize {
    match answer {
        ManagedQueryAnswer::Blocks(pre) => pre.total,
        ManagedQueryAnswer::Pages(pages) => pages.total,
        ManagedQueryAnswer::Counts(_) => 0,
    }
}

/// How long a PENDING execution waits for the overlay worker to carry the
/// revision its capture requires.
///
/// Deliberately NOT the owner's slot wait. `execute_on_slot` runs with the slot
/// already held, so blocking on the overlay's condvar for the full
/// [`crate::query_jobs::QUERY_JOB_WAIT`] would let two pending queries occupy
/// the whole capacity for 30 s and starve every other query on the graph —
/// exactly the queueing the slot exists to prevent (`query_jobs.rs`: "a job
/// that is waiting for a slot holds its request intent and nothing else", plan
/// §2B). A flush is ONE in-process worker turn; a wait that long has already
/// told us to yield capacity and report temporary readiness.
pub(crate) const OVERLAY_FLUSH_WAIT: Duration =
    Duration::from_millis(crate::query_jobs::QUERY_JOB_WAIT.as_millis() as u64 / 10);

/// The PENDING read: the overlay at the flushed state the capture required (or
/// a later one), plus the accepted projection with every pending page masked
/// out of its statement, merged under one budget (R5a).
///
/// The order of the two opens is the coherence proof; see the comment at the
/// accepted open. Nothing here holds the actor, `operation`, the overlay's
/// state mutex beyond `open_snapshot`, or any graph mutex; nothing spawns a
/// thread; nothing writes to either file.
fn execute_pending_on_slot(
    capture: &ManagedQueryCapture,
    pending: &PendingOverlayCapture,
    slot: &JobSlot<'_>,
    census: &ManagedQueryCensus,
    patched: &PatchedRegistryCache,
) -> ManagedQueryOutcome {
    use crate::managed_overlay::OverlayOpen;

    // (1) The OVERLAY first, at exactly one published state. Only unfinished
    // work is retryable readiness. A failed worker or unreadable file cannot
    // become ready by waiting and must surface a bounded execution failure.
    let (mut overlay, state) = match pending
        .overlay
        .open_snapshot(pending.required_revision, OVERLAY_FLUSH_WAIT)
    {
        OverlayOpen::Snapshot { snapshot, state } => (snapshot, state),
        OverlayOpen::Pending => return ManagedQueryOutcome::Busy,
        OverlayOpen::Failed(reason) => return ManagedQueryOutcome::Failed(reason),
        OverlayOpen::Stale => return ManagedQueryOutcome::Stale,
        OverlayOpen::Closed => return ManagedQueryOutcome::Cancelled,
    };
    if !slot.register(overlay.cancellation()) {
        return ManagedQueryOutcome::Cancelled;
    }
    // (2) The ACCEPTED file second, validated against the capture's stamp
    // inside its own read transaction.
    //
    // **The order IS the coherence proof.** A path leaves the pending set only
    // when its batch is ACCEPTED (`retire_latest_projection_frame` is the only
    // caller of `PendingOverlay::remove`, and it runs after the batch applied),
    // and an accepted batch advances the acceptance sequence this open
    // validates. So: the overlay snapshot is pinned at `(instance, flushed)`
    // BEFORE this open; if this open succeeds, no batch was accepted in
    // between, hence no path left the pending set since the capture, hence
    // `state.pending_paths ⊇` the set at capture time. Every EXTRA path is a
    // pending page that appeared after the capture — masked out of the accepted
    // statement below and present in the overlay. No page is read twice and no
    // page is missing; the answer is "as of overlay acquisition" (plan §2B).
    let mut accepted = match PhysicalProjectionQuerySnapshot::open_managed(
        &capture.path,
        capture.stamp.acceptance_sequence,
        capture.stamp.frontier_digest,
    ) {
        Ok(snapshot) => snapshot,
        Err(MaterializationError::Stale { .. }) => return ManagedQueryOutcome::Stale,
        Err(_) => return ManagedQueryOutcome::Failed("managed projection snapshot"),
    };
    if !slot.register(accepted.cancellation()) {
        return ManagedQueryOutcome::Cancelled;
    }
    // (3) The mask: the accepted page id of every pending path.
    let mask = match overlay_mask_ids(&mut accepted, &state) {
        Ok(mask) => mask,
        Err(outcome) => return outcome,
    };
    // (3b) R5c: ONE registry for both sources, so the two files can never be
    // lowered under different effective types. `capture.registry` is the
    // ACCEPTED table the actor caches; for a query with a property leaf it is
    // patched HERE, under these two snapshots and this mask, over exactly the
    // keys the pending pages can have changed. A query with no property leaf
    // reads no effective type at all (C6) and carries the empty registry.
    let registry =
        match patched_registry(capture, &mut accepted, &mut overlay, &mask, census, patched) {
            Ok(registry) => registry,
            Err(outcome) => return outcome,
        };
    // (4) Readiness PER SOURCE. The overlay's `fts_ready` is 1 by schema
    // seeding, but it is probed with the same statement and the same mapping
    // anyway: a file that says otherwise is damaged, not "still building".
    let accepted_fts = match probe_fts_ready(&mut accepted) {
        Ok(ready) => ready,
        Err(outcome) => return outcome,
    };
    let overlay_fts = match probe_fts_ready(&mut overlay) {
        Ok(ready) => ready,
        Err(outcome) => return outcome,
    };
    // (5) ONE IR per selection, ONE compiled-leaf parse each, lowered TWICE —
    // the sources differ only in the masked pages and their own readiness.
    let lower = |query: &Query| {
        let query = anchored_for(query, &capture.request);
        let compiled = crate::query::eval::CompiledLeaves::for_query(&query.evaluable_filter());
        let lower_for = |masked_pages: &[[u8; 16]], fts_ready: bool| {
            lower_query(
                &query,
                &LoweringInputs {
                    today: capture.today,
                    registry: &registry,
                    masked_pages,
                    // NOT `max_rows`: `total` is the number of matches SEEN,
                    // and a buffered source may not be truncated at all.
                    cutoff: None,
                    compiled: &compiled,
                    fts_ready,
                    result_set_rule: RESULT_SET_RULE,
                },
            )
        };
        (
            query.anchor,
            lower_for(&[], overlay_fts),
            lower_for(&mask, accepted_fts),
        )
    };
    let recency = |page: RecencyPage<'_>| {
        capture.journal_format.page_recency_secs(
            page.kind == PageKind::Journal,
            page.name,
            &capture.graph_root.join(page.path),
        )
    };
    // (6) The overlay FIRST: that is the walk's own concatenation order
    // (`sources` is `[overlay pages…, accepted candidate pages…]` before its
    // stable sort by path), and the merged constructor buffers every source but
    // the last, so the small pending file is the buffered one.
    let read = |overlay: &mut PhysicalProjectionQuerySnapshot,
                accepted: &mut PhysicalProjectionQuerySnapshot,
                anchor: crate::query::ir::Anchor,
                overlay_statement: &crate::query::sql::SqlQuery,
                accepted_statement: &crate::query::sql::SqlQuery|
     -> Result<ManagedQueryAnswer, ResultReadError> {
        // A filter that folded to false has its answer already (§3.5, I-15); a
        // source whose statement matches nothing contributes nothing, and if
        // both do the answer is empty without a statement being run.
        if overlay_statement.matches_nothing && accepted_statement.matches_nothing {
            return Ok(empty_answer(anchor));
        }
        match anchor {
            crate::query::ir::Anchor::Page => {
                let mut sources = Vec::with_capacity(2);
                if !overlay_statement.matches_nothing {
                    sources.push(crate::query::results::PageSource {
                        snapshot: overlay,
                        statement: overlay_statement,
                    });
                }
                if !accepted_statement.matches_nothing {
                    sources.push(crate::query::results::PageSource {
                        snapshot: accepted,
                        statement: accepted_statement,
                    });
                }
                Ok(ManagedQueryAnswer::Pages(read_page_results_merged(
                    &mut sources,
                    BackendOrder::Managed,
                    capture.max_rows,
                )?))
            }
            crate::query::ir::Anchor::Block => {
                let mut sources = Vec::with_capacity(2);
                if !overlay_statement.matches_nothing {
                    sources.push(ResultSource {
                        snapshot: overlay,
                        statement: overlay_statement,
                    });
                }
                if !accepted_statement.matches_nothing {
                    sources.push(ResultSource {
                        snapshot: accepted,
                        statement: accepted_statement,
                    });
                }
                Ok(ManagedQueryAnswer::Blocks(read_results_merged(
                    &mut sources,
                    &ResultReadShared {
                        order: BackendOrder::Managed,
                        // The overlay's rows come from the accept path's own
                        // per-page lowering, so its `result_id`s are the real
                        // Managed ones.
                        identity: &ResultIdentity::Stored,
                        max_rows: capture.max_rows,
                        max_bytes: capture.max_bytes,
                        profile: capture.profile,
                        recency: &recency,
                    },
                )?))
            }
        }
    };
    let answer = match &capture.request {
        ManagedQueryRequest::Counts(probes) => {
            let mut counts = Vec::with_capacity(probes.len());
            let mut failure = None;
            for probe in probes {
                let (anchor, overlay_statement, accepted_statement) = lower(probe);
                match read(
                    &mut overlay,
                    &mut accepted,
                    anchor,
                    &overlay_statement,
                    &accepted_statement,
                ) {
                    Ok(answer) => counts.push(answer_total(&answer)),
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            match failure {
                Some(error) => Err(error),
                None => Ok(ManagedQueryAnswer::Counts(counts)),
            }
        }
        _ => {
            let (anchor, overlay_statement, accepted_statement) = lower(&capture.query);
            read(
                &mut overlay,
                &mut accepted,
                anchor,
                &overlay_statement,
                &accepted_statement,
            )
        }
    };
    let outcome = match answer {
        Ok(answer) => {
            census.note_statement_read();
            census.note_pending_read();
            ManagedQueryOutcome::Answered(answer)
        }
        Err(ResultReadError::Cancelled) => ManagedQueryOutcome::Cancelled,
        Err(ResultReadError::Sql(_)) => ManagedQueryOutcome::Failed("managed projection statement"),
        // A row that decodes wrong inside a successfully opened, validated
        // snapshot is the projection contradicting itself — on EITHER file.
        // Never a silently smaller answer (D-3).
        Err(ResultReadError::Corrupt(_)) => {
            ManagedQueryOutcome::Failed("managed projection result rows")
        }
    };
    // (8) Both transactions end before the slot releases its capacity.
    drop(overlay);
    drop(accepted);
    outcome
}

/// The registry BOTH sources of a pending read are lowered under (R5c).
///
/// For a query with no property leaf this is the capture's empty registry and
/// nothing is read (C6). For a query WITH one it is the accepted table the
/// capture carries, patched over the affected keys under these two snapshots —
/// from the one-entry cache when the same pending state already paid for it,
/// which is what makes a burst of property queries between two keystrokes cost
/// one patch.
///
/// A patch refusal is `Failed` (D-3: a damaged read fails, never a silently
/// wrong table); a cancelled read is `Cancelled`, exactly as the probes are.
fn patched_registry(
    capture: &ManagedQueryCapture,
    accepted: &mut PhysicalProjectionQuerySnapshot,
    overlay: &mut PhysicalProjectionQuerySnapshot,
    mask: &[[u8; 16]],
    census: &ManagedQueryCensus,
    cache: &PatchedRegistryCache,
) -> Result<Arc<Registry>, ManagedQueryOutcome> {
    if !capture.props {
        return Ok(Arc::clone(&capture.registry));
    }
    let base_generation = capture.registry.generation();
    if let Some(hit) = cache.get(&capture.stamp, base_generation) {
        return Ok(hit);
    }
    let built = crate::managed_registry_patch::patched_pending_registry(
        accepted,
        overlay,
        mask,
        &capture.registry,
        &capture.config,
    )
    .map_err(|error| match error {
        crate::managed_registry_patch::PatchError::Cancelled => ManagedQueryOutcome::Cancelled,
        crate::managed_registry_patch::PatchError::Damaged => {
            ManagedQueryOutcome::Failed("property_registry patch")
        }
    })?;
    census.note_registry_patch();
    let built = Arc::new(built);
    cache.put(&capture.stamp, base_generation, &built);
    Ok(built)
}

/// The accepted page id of every path the overlay holds pending, so the
/// accepted statement can exclude them.
///
/// Byte-for-byte the walk's own mask read
/// (`application_simple_query_pages_ready`): no row is a page the accepted file
/// has never seen, one row is masked, and more than one is a refusal — the walk
/// refuses the same shape, and answering would either duplicate or drop a page
/// (D-3).
fn overlay_mask_ids(
    accepted: &mut PhysicalProjectionQuerySnapshot,
    state: &crate::managed_overlay::OverlayState,
) -> Result<Vec<[u8; 16]>, ManagedQueryOutcome> {
    let mut mask = Vec::with_capacity(state.pending_paths.len());
    for path in &state.pending_paths {
        let rows = match accepted.run_projection_query(
            "SELECT page_id FROM pages WHERE path = ?1 LIMIT 2",
            &[PhysicalQueryValue::Text(path.clone())],
        ) {
            Ok(rows) => rows,
            Err(_) if accepted.cancellation().is_cancelled() => {
                return Err(ManagedQueryOutcome::Cancelled)
            }
            Err(_) => return Err(ManagedQueryOutcome::Failed("managed projection pages")),
        };
        if rows.len() > 1 {
            return Err(ManagedQueryOutcome::Failed("overlay path ambiguous"));
        }
        let Some(row) = rows.first() else {
            continue;
        };
        match row.first() {
            Some(PhysicalQueryValue::Blob(bytes)) if bytes.len() == 16 => {
                mask.push(bytes.as_slice().try_into().expect("a checked 16-byte id"));
            }
            _ => return Err(ManagedQueryOutcome::Failed("managed projection pages")),
        }
    }
    Ok(mask)
}

/// A barrier between the slot admission and the snapshot open (test-only), so
/// a gate can move the accepted frontier for real in the window the stamp
/// validation exists to catch, instead of racing a sleep against the actor.
/// The same shape as `query::results`' `BEFORE_PAYLOAD_BATCH` hook, which
/// exists for the same reason.
#[cfg(test)]
thread_local! {
    static BEFORE_MANAGED_OPEN: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_before_managed_open_hook(hook: Option<Box<dyn Fn()>>) {
    BEFORE_MANAGED_OPEN.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(test)]
fn run_before_managed_open_hook() {
    // Taken out of the borrow first: the hook drives a whole save-and-accept
    // turn through the handle, which is free to execute another query on this
    // same thread, and holding the `RefCell` borrow across that would panic.
    let taken = BEFORE_MANAGED_OPEN.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = taken {
        hook();
        BEFORE_MANAGED_OPEN.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                *slot = Some(hook);
            }
        });
    }
}

/// §5.10's readiness probe on THIS snapshot: one statement,
/// [`FTS_READY_PROBE_SQL`], ready iff the row's integer is 1.
///
/// Deliberately NOT Direct's "an unreadable probe means not ready". Direct
/// probes a pooled seam it can abandon; this runs on an OWNED snapshot whose
/// transaction the drain is waiting on, so swallowing an error would turn a
/// cancelled probe into a full unbounded scan the drain then has to interrupt.
/// A cancelled probe is `Cancelled`, any other probe error is `Failed`, and
/// only a successful read of a non-`1` value is "still building".
fn probe_fts_ready(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
) -> Result<bool, ManagedQueryOutcome> {
    match snapshot.run_projection_query(FTS_READY_PROBE_SQL, &[]) {
        Ok(rows) => Ok(matches!(
            rows.first().and_then(|row| row.first()),
            Some(PhysicalQueryValue::Integer(1))
        )),
        Err(_) if snapshot.cancellation().is_cancelled() => Err(ManagedQueryOutcome::Cancelled),
        Err(_) => Err(ManagedQueryOutcome::Failed("search_fts_build phase")),
    }
}

/// The ONE patched pending registry this runtime retains (R5c).
///
/// Keyed by `(the capture's stamp, the accepted base's generation)`. The stamp
/// carries `overlay_revision` and `overlay_instance`, so a save or rebuild misses; it carries
/// `acceptance_sequence` and `frontier_digest`, so an accepted batch misses;
/// it carries `config_digest`, so a config edit misses. The base generation
/// additionally separates the pre-first-build empty table from the first
/// published one. One entry, replaced on every miss: that is what makes N
/// different property queries during one pause between keystrokes cost ONE
/// patch, and it can never serve a table built for another pending state.
///
/// It is a cache of a pure function over two owned snapshots, never authority.
#[derive(Default)]
pub(crate) struct PatchedRegistryCache {
    entry: Mutex<Option<(ManagedQueryStamp, u64, Arc<Registry>)>>,
}

impl PatchedRegistryCache {
    fn get(&self, stamp: &ManagedQueryStamp, base_generation: u64) -> Option<Arc<Registry>> {
        let entry = self.entry.lock().unwrap();
        entry.as_ref().and_then(|(cached, generation, registry)| {
            (cached == stamp && *generation == base_generation).then(|| Arc::clone(registry))
        })
    }

    fn put(&self, stamp: &ManagedQueryStamp, base_generation: u64, registry: &Arc<Registry>) {
        *self.entry.lock().unwrap() = Some((stamp.clone(), base_generation, Arc::clone(registry)));
    }

    #[cfg(test)]
    pub(crate) fn clear(&self) {
        *self.entry.lock().unwrap() = None;
    }
}

/// Everything the accepted route shares between the actor and the handle:
/// the job owner (the actor drains it before touching the file), the census,
/// the memo (the actor turn reads it, the handle and the walk fill it), and
/// the one patched pending registry.
#[derive(Default)]
pub(crate) struct ManagedQueryShared {
    pub(crate) jobs: QueryJobOwner,
    pub(crate) census: ManagedQueryCensus,
    pub(crate) memo: Mutex<ApplicationSimpleQueryMemo>,
    pub(crate) patched_registry: PatchedRegistryCache,
    /// Test hook: the outcomes the handle uses INSTEAD of executing the next
    /// captures, in order. Lets a test drive every handle-side transition
    /// (`Stale` re-capture, the third `Stale`, `Busy`, `Cancelled`, `Failed`)
    /// without a projection that produces it. Never consulted in production.
    #[cfg(test)]
    pub(crate) injected_outcomes: Mutex<VecDeque<ManagedQueryOutcome>>,
    /// Test hook: overrides the owner's slot wait so `Busy` costs a test
    /// milliseconds, not `QUERY_JOB_WAIT`.
    #[cfg(test)]
    pub(crate) job_wait: Mutex<Option<Duration>>,
}

impl ManagedQueryShared {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            jobs: QueryJobOwner::new(capacity),
            ..Self::default()
        }
    }

    /// How long one execution waits for a slot.
    pub(crate) fn job_wait(&self) -> Duration {
        #[cfg(test)]
        if let Some(wait) = *self.job_wait.lock().unwrap() {
            return wait;
        }
        crate::query_jobs::QUERY_JOB_WAIT
    }

    /// Run the executor — or, under test, the next injected outcome.
    pub(crate) fn execute(&self, capture: &ManagedQueryCapture) -> ManagedQueryOutcome {
        #[cfg(test)]
        if let Some(outcome) = self.injected_outcomes.lock().unwrap().pop_front() {
            return outcome;
        }
        execute_managed_query(
            capture,
            &self.jobs,
            &self.census,
            &self.patched_registry,
            self.job_wait(),
        )
    }
}

impl Default for QueryJobOwner {
    fn default() -> Self {
        Self::new(crate::query_jobs::DEFAULT_QUERY_JOB_CAPACITY)
    }
}

/// How many distinct simple-query answers one runtime retains at a time.
const APPLICATION_SIMPLE_QUERY_MEMO_ENTRIES: usize = 4;

/// The largest answer the memo will retain, counted in emitted result blocks.
/// A bigger answer is served but never stored, so the retained set stays a
/// small multiple of this bound rather than of the caller's `max_rows`.
const APPLICATION_SIMPLE_QUERY_MEMO_MAX_BLOCKS: usize = 4_096;

/// One memoized PRE-VIEW answer: base-ordered groups plus the recency axis,
/// so `sort-by` and `sample` are applied per request over one shared entry —
/// the Direct twin is `Graph::derived_memo_pre_view`.
#[derive(Clone, Debug)]
pub(crate) struct MemoizedPreView {
    pub(crate) groups: Arc<Vec<RefGroup>>,
    pub(crate) recency_by_page: Arc<HashMap<String, i64>>,
    pub(crate) total: usize,
    pub(crate) exceeded: bool,
}

#[derive(Debug)]
struct ApplicationSimpleQueryMemoEntry {
    key: String,
    /// Whether the query has a `props` leaf, so its answer depends on the
    /// registry's effective types and a generation advance evicts it (C6).
    props: bool,
    result: MemoizedPreView,
}

/// Bounded memo for a Managed simple query's PRE-VIEW answer.
///
/// Keyed by the resolved IR (`query::simple_query_cache_key`) and the
/// [`ManagedQueryStamp`]; every entry goes when the stamp moves — an accepted
/// batch, a config edit, or the execution day — and the `props` entries go
/// when the registry generation advances. Since R5b a pending suffix is part
/// of the stamp (`overlay_revision`) rather than a reason to have none, so a
/// pending answer is memoized too and every pending save moves the stamp and
/// clears it. Filled by the executor after a successful read;
/// NEVER after `Stale`, `Busy`, `Cancelled` or
/// `Failed`, which are not answers.
///
/// It is a cache of a pure function over durable evidence, never authority: a
/// dropped or absent entry costs one recomputation and nothing else.
#[derive(Debug, Default)]
pub(crate) struct ApplicationSimpleQueryMemo {
    stamp: Option<ManagedQueryStamp>,
    registry_generation: u64,
    entries: VecDeque<ApplicationSimpleQueryMemoEntry>,
}

impl ApplicationSimpleQueryMemo {
    pub(crate) fn get(
        &mut self,
        stamp: &ManagedQueryStamp,
        registry_generation: u64,
        key: &str,
    ) -> Option<MemoizedPreView> {
        if self.stamp.as_ref() != Some(stamp) {
            self.stamp = Some(stamp.clone());
            self.entries.clear();
            self.registry_generation = registry_generation;
            return None;
        }
        self.note_registry_generation(registry_generation);
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.result.clone())
    }

    /// Drop every `props` entry when the registry generation advanced. The
    /// non-`props` entries are untouched: their answers cannot depend on an
    /// effective type they never read (C6).
    fn note_registry_generation(&mut self, registry_generation: u64) {
        if self.registry_generation == registry_generation {
            return;
        }
        self.registry_generation = registry_generation;
        self.entries.retain(|entry| !entry.props);
    }

    /// Store a base-ordered pre-view answer. Returns the entry as stored (or
    /// as it would have been stored, when the answer is too large to retain),
    /// so the caller applies the view to the same shared groups either way.
    pub(crate) fn insert(
        &mut self,
        stamp: &ManagedQueryStamp,
        registry_generation: u64,
        key: &str,
        props: bool,
        pre: PreViewGroups,
    ) -> MemoizedPreView {
        let result = MemoizedPreView {
            groups: Arc::new(pre.groups),
            recency_by_page: Arc::new(pre.recency_by_page),
            total: pre.total,
            exceeded: pre.exceeded,
        };
        if self.stamp.as_ref() != Some(stamp) {
            self.stamp = Some(stamp.clone());
            self.entries.clear();
            self.registry_generation = registry_generation;
        } else {
            self.note_registry_generation(registry_generation);
        }
        let blocks = result
            .groups
            .iter()
            .map(|group| group.blocks.len())
            .sum::<usize>();
        if blocks > APPLICATION_SIMPLE_QUERY_MEMO_MAX_BLOCKS {
            return result;
        }
        self.entries.retain(|entry| entry.key != key);
        while self.entries.len() >= APPLICATION_SIMPLE_QUERY_MEMO_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(ApplicationSimpleQueryMemoEntry {
            key: key.to_owned(),
            props,
            result: result.clone(),
        });
        result
    }

    /// The off-actor executor's insert: store the answer only while the memo
    /// still stands at the capture's stamp. An accepted batch can land between
    /// capture and insert, and a late result must not evict the entries the
    /// actor has since filled under the newer stamp (I-20); it is returned
    /// unstored and served once.
    pub(crate) fn insert_if_current(
        &mut self,
        stamp: &ManagedQueryStamp,
        registry_generation: u64,
        key: &str,
        props: bool,
        pre: PreViewGroups,
    ) -> MemoizedPreView {
        if self.stamp.as_ref() != Some(stamp) {
            return MemoizedPreView {
                groups: Arc::new(pre.groups),
                recency_by_page: Arc::new(pre.recency_by_page),
                total: pre.total,
                exceeded: pre.exceeded,
            };
        }
        self.insert(stamp, registry_generation, key, props, pre)
    }

    pub(crate) fn clear(&mut self) {
        self.stamp = None;
        self.entries.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(sequence: u64, config: &str, today: i64) -> ManagedQueryStamp {
        ManagedQueryStamp {
            acceptance_sequence: sequence,
            frontier_digest: ContentDigest::of(b"frontier"),
            config_digest: ContentDigest::of(config.as_bytes()),
            today,
            overlay_revision: None,
            overlay_instance: None,
        }
    }

    fn pre(total: usize) -> PreViewGroups {
        PreViewGroups {
            groups: Vec::new(),
            recency_by_page: HashMap::new(),
            total,
            exceeded: false,
        }
    }

    #[test]
    fn a_recreated_overlay_cannot_reuse_the_patched_registry() {
        let mut first = stamp(1, "config", 0);
        first.overlay_revision = Some(2);
        first.overlay_instance = Some(10);
        let second = ManagedQueryStamp {
            overlay_instance: Some(11),
            ..first.clone()
        };
        let registry = Arc::new(Registry::empty(&ParseConfig::default()));
        let cache = PatchedRegistryCache::default();
        cache.put(&first, 0, &registry);
        assert!(Arc::ptr_eq(&cache.get(&first, 0).unwrap(), &registry));
        assert!(
            cache.get(&second, 0).is_none(),
            "equal revision numbers do not identify the same overlay"
        );
    }

    #[test]
    fn the_managed_memo_is_keyed_by_the_config_digest_and_the_execution_day() {
        let mut memo = ApplicationSimpleQueryMemo::default();
        let before = stamp(7, "one", 100);
        memo.insert(&before, 0, "k", false, pre(3));
        assert_eq!(
            memo.get(&before, 0, "k").map(|r| r.total),
            Some(3),
            "the same evidence under the same rules is the same answer"
        );
        // E6: the journal title format decides a page's kind and day, so a
        // query with no property leaf at all is config-sensitive, and editing
        // `logseq/config.edn` never moves the acceptance sequence.
        assert_eq!(
            memo.get(&stamp(7, "two", 100), 0, "k").map(|r| r.total),
            None,
            "a config change under an unchanged sequence must not serve the old answer"
        );
        // §4.4: `(between -7d today)` is a different question tomorrow.
        memo.insert(&before, 0, "k", false, pre(3));
        assert_eq!(
            memo.get(&stamp(7, "one", 101), 0, "k").map(|r| r.total),
            None
        );
        // And the frontier digest is part of the identity, not only the sequence.
        memo.insert(&before, 0, "k", false, pre(3));
        let other_frontier = ManagedQueryStamp {
            frontier_digest: ContentDigest::of(b"other"),
            ..before.clone()
        };
        assert_eq!(memo.get(&other_frontier, 0, "k").map(|r| r.total), None);
    }

    #[test]
    fn a_registry_generation_advance_evicts_the_managed_memos_props_entries_only() {
        let mut memo = ApplicationSimpleQueryMemo::default();
        let s = stamp(7, "one", 100);
        memo.insert(&s, 4, "props-query", true, pre(1));
        memo.insert(&s, 4, "task-query", false, pre(2));
        // A graph-wide effective-type change. Per-page retention cannot see
        // it: the pages holding the answer did not change, only the key's
        // type did.
        assert_eq!(
            memo.get(&s, 5, "props-query").map(|r| r.total),
            None,
            "the typed query is recomputed under the new generation"
        );
        assert_eq!(
            memo.get(&s, 5, "task-query").map(|r| r.total),
            Some(2),
            "a query that reads no property atom cannot depend on an effective type"
        );
    }

    #[test]
    fn the_memo_is_bounded_by_entries_and_by_answer_size() {
        let mut memo = ApplicationSimpleQueryMemo::default();
        let s = stamp(1, "c", 1);
        for i in 0..APPLICATION_SIMPLE_QUERY_MEMO_ENTRIES + 2 {
            memo.insert(&s, 0, &format!("k{i}"), false, pre(i));
        }
        assert_eq!(memo.len(), APPLICATION_SIMPLE_QUERY_MEMO_ENTRIES);
        assert!(
            memo.get(&s, 0, "k0").is_none(),
            "the oldest entry is evicted"
        );
        assert!(memo.get(&s, 0, "k5").is_some());

        let mut huge = pre(0);
        huge.groups.push(RefGroup {
            page: "big".into(),
            kind: crate::model::PageKind::Page,
            blocks: (0..APPLICATION_SIMPLE_QUERY_MEMO_MAX_BLOCKS + 1)
                .map(|_| crate::model::BlockDto::default())
                .collect(),
            evidence: Vec::new(),
        });
        let served = memo.insert(&s, 0, "huge", false, huge);
        assert_eq!(
            served.groups[0].blocks.len(),
            APPLICATION_SIMPLE_QUERY_MEMO_MAX_BLOCKS + 1
        );
        assert!(memo.get(&s, 0, "huge").is_none(), "served but never stored");
        memo.clear();
        assert_eq!(memo.len(), 0);
    }

    #[test]
    fn a_late_executor_insert_never_evicts_a_newer_memo() {
        let mut memo = ApplicationSimpleQueryMemo::default();
        let old = stamp(7, "c", 1);
        let new = stamp(8, "c", 1);
        // The actor filled the memo under the newer stamp while an off-actor
        // execution captured under the older one was still running.
        memo.insert(&new, 0, "fresh", false, pre(5));
        let served = memo.insert_if_current(&old, 0, "late", false, pre(9));
        assert_eq!(served.total, 9, "the late answer is still served once");
        assert_eq!(
            memo.get(&new, 0, "fresh").map(|r| r.total),
            Some(5),
            "I-20: a late result cannot land on newer state"
        );
        assert!(memo.get(&new, 0, "late").is_none());
        // Under the current stamp it is an ordinary insert.
        memo.insert_if_current(&new, 0, "late", false, pre(9));
        assert_eq!(memo.get(&new, 0, "late").map(|r| r.total), Some(9));
    }

    /// The storage contract's account of this route, pinned sentence by
    /// sentence so a rewrite of either side fails here first.
    #[test]
    fn storage_contract_names_the_off_actor_accepted_route() {
        let contract = include_str!("../../../docs/storage-sync-contract.md");
        let section = contract
            .split("**A Managed simple query over the accepted frontier runs off the actor.**")
            .nth(1)
            .and_then(|tail| tail.split("## 2. Enrollment").next())
            .expect("the accepted-route paragraph precedes section 2");
        for sentence in [
            "one short actor turn",
            "executes on\nthe calling thread after the actor's operation lock is released",
            "the same owner every off-actor read of that file is admitted\nby",
            "only while that stamp is still the actor's current stamp",
            "re-captures at most twice, then reports",
            "cancelled and is not counted as a fallback",
            "a `Failed` read is an error",
            "These routes never traverse the parsed graph",
            "closed in exactly three places",
            "writes a checkpoint sidecar, never a WAL\ncheckpoint",
        ] {
            assert!(section.contains(sentence), "contract lost: {sentence}");
        }
        assert_eq!(MAX_STALE_RECAPTURES, 2, "the contract says twice");
    }

    /// R5a's pin: the two-source pending read's own paragraph. The open ORDER
    /// is the coherence proof and the mask is what keeps the sources disjoint,
    /// so both must stay written down where the storage contract is read.
    #[test]
    fn storage_contract_names_the_two_source_pending_read() {
        let contract = include_str!("../../../docs/storage-sync-contract.md");
        let section = contract
            .split("**A captured pending query is answered off the actor from BOTH databases.**")
            .nth(1)
            .and_then(|tail| tail.split("## 2. Enrollment").next())
            .expect("the pending-read paragraph precedes section 2");
        for sentence in [
            "opens the overlay first, at the flushed revision the capture
required or later",
            "only then opens the accepted file and validates the
capture's stamp inside that read transaction",
            "that open ORDER is the whole
coherence proof",
            "lowers ONE
compiled query twice",
            "every page of the overlay's pending set masked out",
            "the two sources are disjoint by construction",
            "merged in the walk's own
base order",
            "under ONE construction budget",
            "**The answer is the walk's answer**",
            "no page document is
loaded and nothing is parsed",
            "with a live worker opens `Pending` and reports temporary readiness",
            "stopped worker and an unreadable file open `Failed`, never endless readiness",
            "reachable from both sources —
is `Failed` too rather than answered twice",
        ] {
            assert!(section.contains(sentence), "contract lost: {sentence}");
        }
    }

    /// The same pin for the executor's own paragraph: what one accepted-route
    /// read costs, and what each of its outcomes owes.
    #[test]
    fn storage_contract_names_what_the_accepted_read_costs() {
        let contract = include_str!("../../../docs/storage-sync-contract.md");
        let section = contract
            .split("**That read is one snapshot, and it never opens a page.**")
            .nth(1)
            .and_then(|tail| tail.split("## 2. Enrollment").next())
            .expect("the executor paragraph precedes section 2");
        for sentence in [
            "takes\nits job slot BEFORE any transaction",
            "validates the capture's acceptance sequence and frontier\nroot digest INSIDE the read transaction",
            "the full-text\nreadiness probe",
            "one descriptor statement",
            "the payload batches charged for the rows the budget admitted",
            "that page file's modification\ntime",
            "No page document is loaded, no source text is parsed",
            "`pages.path` under SQLite's\nbinary collation",
            "read from its display NAME exactly as the walk reads it",
            "the transaction ends before the slot is released",
            "never\nanswers as if it succeeded",
            "has no row-count cross-check",
        ] {
            assert!(section.contains(sentence), "contract lost: {sentence}");
        }
    }

    /// The executor's outermost shape, with no projection at all: capacity is
    /// taken and released, an unopenable file is `Failed` (never a silent
    /// fallback), and the executor counts NOTHING for it — `failed_reads` is
    /// the handle's to count, because only the handle knows the request ended
    /// in an error rather than a walk.
    #[test]
    fn an_unopenable_projection_fails_the_read_and_releases_its_slot() {
        let owner = QueryJobOwner::new(1);
        let census = ManagedQueryCensus::default();
        let today = JournalDate::today();
        let (query, view) = crate::query::parse_query_source("(task TODO)", today);
        let profile = ConstructionProfile::from_view(&view);
        let config = crate::config::Config::default();
        let capture = ManagedQueryCapture {
            path: PathBuf::from("/nonexistent/projection.sqlite"),
            overlay: None,
            graph_root: PathBuf::from("/nonexistent"),
            stamp: stamp(1, "c", today.ordinal_key()),
            config: config.parse_config(),
            journal_format: crate::date::JournalFormat::new(
                config.journal_file_name_format.as_deref(),
                config.journal_page_title_format.as_deref(),
            ),
            registry: Arc::new(Registry::empty(&config.parse_config())),
            registry_generation: 0,
            props: false,
            key: crate::query::simple_query_cache_key(&query, 10, 100, profile),
            query,
            view,
            today,
            max_rows: 10,
            max_bytes: 100,
            profile,
            request: ManagedQueryRequest::Blocks,
            report: crate::query::ir::QueryReport {
                ran: Vec::new(),
                ignored: Vec::new(),
                supported: true,
            },
        };
        assert!(matches!(
            execute_managed_query(
                &capture,
                &owner,
                &census,
                &PatchedRegistryCache::default(),
                Duration::from_millis(1)
            ),
            ManagedQueryOutcome::Failed("managed projection snapshot")
        ));
        assert_eq!(census.snapshot(), ManagedQueryCensusSnapshot::default());
        assert_eq!(
            owner.active(),
            0,
            "I-21: the slot is released on the error path"
        );

        // Capacity is acquired BEFORE any transaction, so an exhausted owner
        // is `Busy` without the file being touched at all.
        let held = match owner.acquire() {
            crate::query_jobs::Admission::Slot(slot) => slot,
            _ => panic!("the only slot admits"),
        };
        assert!(matches!(
            execute_managed_query(
                &capture,
                &owner,
                &census,
                &PatchedRegistryCache::default(),
                Duration::from_millis(1)
            ),
            ManagedQueryOutcome::Busy
        ));
        drop(held);

        // A closed owner cancels instead of reporting Busy: the walk answers
        // and nothing is counted as a fallback.
        owner.close();
        assert!(matches!(
            execute_managed_query(
                &capture,
                &owner,
                &census,
                &PatchedRegistryCache::default(),
                Duration::from_millis(1)
            ),
            ManagedQueryOutcome::Cancelled
        ));
        assert_eq!(census.snapshot(), ManagedQueryCensusSnapshot::default());
    }
}
