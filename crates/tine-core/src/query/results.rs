//! The ONE database-backed result constructor, for BOTH backends (R3).
//!
//! A ready query's public answer is built here, from ONE owned read snapshot of
//! the projection: no page document is loaded, no source text is read, and no
//! parsed cache is consulted. That is the whole point of the packet — a page
//! with several thousand blocks and one match must cost one descriptor row and
//! one payload row, not a document parse.
//!
//! **What this module is, and is not.**
//!
//! * SELECTION is the compiler's statement ([`crate::query::sql::lower_query`]),
//!   passed in verbatim. This module wraps it once
//!   ([`crate::query::sql::descriptor_statement`]) to add ordering and the
//!   result metadata; it never re-lowers, never re-applies §5.3's result-set
//!   rule (the statement already did), and never edits the predicate.
//! * ORDERING is the order the walk CHARGES its budget in — Direct Files by
//!   `query_page_order.position`, Managed Storage by `pages.path` BINARY — then
//!   `query_block_results.preorder` within a page. DISPLAY order
//!   (`base_order_groups`, `sort-by`, `sample`) is NOT applied here: this
//!   returns the same PRE-VIEW shape the retired document hydration returned,
//!   and [`crate::query::apply_view`] finishes it exactly as before.
//! * The BUDGET is [`ConstructionBudget`] itself, walked with the same four
//!   rules `collect_sql_matched_blocks` uses, in the same order. `total` and
//!   `exceeded` are the budget's, not a second policy.
//! * The PAYLOAD is read only for ADMITTED ids, in batches of
//!   [`PAYLOAD_BATCH`], three bound statements per batch — never one statement
//!   per block, never a tags×properties join, never `SELECT *`.
//!
//! **A damaged read FAILS (D-3).** Every join in the descriptor read is LEFT
//! and every batch is validated for exact coverage, ownership and counts, so a
//! missing metadata row, a stale count or a page id that does not match cannot
//! turn into a shorter answer. The projection is a disposable cache; the
//! recovery for damage is a rebuild, which the caller schedules on
//! [`ResultReadError::Corrupt`]. It is never a silently smaller result set.
//!
//! **No live state, no locks.** `read_results` takes a snapshot and plain
//! inputs. It never calls into `Graph`, never holds a graph or actor lock, and
//! never caches anything. The identity policy and the recency producer are
//! CAPTURED by the caller and passed in, so a mutable live lookup cannot slip
//! into the middle of an answer.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use tine_storage::sqlite::{
    MaterializationError, PhysicalProjectionQuerySnapshot, PhysicalQueryValue,
};

use crate::direct_projection::page_kind_from_sql;
use crate::model::{
    block_dto_estimated_bytes, doc_runtime_id_for_order, shallow_block_facets_dto, BlockDto,
    PageKind, RefGroup, ShallowBlockFacets,
};
use crate::query::sql::{descriptor_statement, page_statement, SqlQuery};
use crate::query::{ConstructionBudget, ConstructionProfile, PreViewGroups, ResultViewGroup};

// The gates. `#[path]` keeps the file beside this one so the shared
// production-source scanner sees a `*_tests.rs` sibling include and blanks it
// from every census, exactly as `sql.rs` does for `sql_gates_tests.rs`.
#[cfg(test)]
#[path = "results_tests.rs"]
mod results_tests;

/// How many admitted ids one payload batch binds. Three statements per batch,
/// so the payload cost of an answer is `3 * ceil(admitted / 128)` statements
/// and nothing else.
pub(crate) const PAYLOAD_BATCH: usize = 128;

/// `owner_type` for a BLOCK, as `PhysicalEntityId::sql_parts` spells it. The
/// same constant `sql.rs` binds; tags and properties are owner-local rows and
/// the page's own facets share these two tables under owner type 0.
const OWNER_BLOCK: i64 = 1;

/// The cross-page base order a backend charges its budget in.
///
/// Not a display order and not a preference: it is the order in which the
/// WALK's page source enumerates pages, because that is what decides which rows
/// survive a truncated budget. Direct Files' inventory order is materialized in
/// `query_page_order`; Managed Storage's is `pages.path` compared as bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackendOrder {
    Direct,
    Managed,
}

/// Where an admitted row's PUBLIC id comes from (WARM-IDENTITY-ORDER-CONTRACT).
///
/// Physical selection ids and public result ids are separate. This policy is
/// captured ONCE per job, beside the snapshot, and is never re-read from live
/// state while an answer is being built.
pub(crate) enum ResultIdentity {
    /// Managed Storage, and Direct rows produced in THIS session: the stored
    /// `query_block_results.result_id` IS the public id.
    Stored,
    /// Direct Files in a FRESH session: a page nobody edited in this session
    /// will be re-parsed on demand into reproducible STRUCTURAL runtime ids, so
    /// its rows resolve through `doc_runtime_id_for_order(path, order_key)` —
    /// no document, no traversal. A page this session DID edit kept its live
    /// ids at an exact revision, so its rows use the stored id.
    ///
    /// The set is captured by the caller together with the snapshot; nothing
    /// here reads live state. `all_session` is the whole-graph shortcut for a
    /// session that owns every page's identity.
    DirectStructural {
        session_pages: Arc<HashSet<[u8; 16]>>,
        all_session: bool,
    },
}

/// WHERE one admitted result physically lives, inside THIS batch.
///
/// The one thing the public answer cannot carry (RET3): `BlockDto::id` is a
/// PUBLIC id and `RefGroup::page` is a DISPLAY name, so two different physical
/// blocks may expose the same pair. Export subtree construction has to read the
/// exact block that was selected, on the exact snapshot it was selected from,
/// which is what this triple names.
///
/// **It is a batch-scoped coordinate, not a handle.** `source` indexes the
/// `&mut [ResultSource]`/`&mut [ExportSubtreeSource]` slice this batch passed
/// in; the two ids are physical row ids of THAT snapshot. It is `Copy`, never
/// crosses IPC or a memo boundary, and is meaningless once the caller releases
/// its snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ResultLocator {
    pub(crate) source: usize,
    pub(crate) page_id: [u8; 16],
    pub(crate) block_id: [u8; 16],
}

/// What one admitted row is stored AS in its group.
///
/// The result collector is one implementation, one set of statements and one
/// admission rule; only the shape of the value it keeps differs. An ordinary
/// reader keeps the `BlockDto` alone, so it retains not one byte of locator;
/// export keeps the DTO beside its [`ResultLocator`]. `carry` is called once
/// per admitted row and its locator argument is dropped unread on the ordinary
/// path, which monomorphisation removes entirely.
pub(crate) trait ResultCarrier {
    type Block;
    fn carry(dto: BlockDto, locator: ResultLocator) -> Self::Block;
}

/// The ordinary reader's carrier: the public DTO and nothing else.
pub(crate) struct PlainResults;

impl ResultCarrier for PlainResults {
    type Block = BlockDto;
    fn carry(dto: BlockDto, _locator: ResultLocator) -> BlockDto {
        dto
    }
}

/// The export reader's carrier: the same DTO plus the physical coordinate the
/// subtree read needs. `ResultViewBlock` is implemented for `(BlockDto, T)`, so
/// the ALREADY ACCEPTED shared view owner sorts, coalesces and samples these
/// entries without a second view implementation.
pub(crate) struct LocatedResults;

impl ResultCarrier for LocatedResults {
    type Block = (BlockDto, ResultLocator);
    fn carry(dto: BlockDto, locator: ResultLocator) -> (BlockDto, ResultLocator) {
        (dto, locator)
    }
}

/// [`PreViewGroups`] with every entry's physical locator retained (RET3).
///
/// The same four fields, the same base order, the same `total`/`exceeded` and
/// the same recency map: only the block type differs, because the locator is
/// attached at admission rather than recovered afterwards.
pub(crate) struct LocatedPreViewGroups {
    pub(crate) groups: Vec<ResultViewGroup<(BlockDto, ResultLocator)>>,
    pub(crate) recency_by_page: std::collections::HashMap<String, i64>,
    pub(crate) total: usize,
    pub(crate) exceeded: bool,
}

/// Everything one result read needs besides the snapshot itself.
pub(crate) struct ResultReadInputs<'a> {
    /// The compiler's statement, unchanged (`lower_query`).
    pub(crate) statement: &'a SqlQuery,
    pub(crate) order: BackendOrder,
    pub(crate) identity: &'a ResultIdentity,
    pub(crate) max_rows: usize,
    pub(crate) max_bytes: usize,
    pub(crate) profile: ConstructionProfile,
    /// The recency axis for `(sort-by modified …)`, by page: each backend's
    /// EXISTING walk producer — Direct Files' `page_recency_secs_for` over the
    /// stored journal day and path, Managed Storage's
    /// `JournalFormat::page_recency_secs` over the page's kind and NAME — given
    /// everything the descriptor row knows about the page. It is a callback
    /// because it is a filesystem `stat` that must not run for a page the
    /// answer did not admit, and because only the caller knows the graph root
    /// the stored relative path hangs off and which producer its walk uses.
    pub(crate) recency: &'a dyn Fn(RecencyPage<'_>) -> i64,
}

impl<'a> ResultReadInputs<'a> {
    /// The half of these inputs that a MERGED read shares across its sources.
    fn shared(&self) -> ResultReadShared<'a> {
        ResultReadShared {
            order: self.order,
            identity: self.identity,
            max_rows: self.max_rows,
            max_bytes: self.max_bytes,
            profile: self.profile,
            recency: self.recency,
        }
    }
}

/// Everything a result read needs that is NOT per source.
///
/// [`ResultReadInputs`] minus the statement. A merged read (R5a: the Managed
/// pending route) lowers ONE IR into one statement per source — they differ
/// only in the masked pages and in that source's own FTS readiness — so the
/// statement is per source and the order, identity, bounds, profile and
/// recency axis are shared. There is exactly one budget and one set of groups.
pub(crate) struct ResultReadShared<'a> {
    pub(crate) order: BackendOrder,
    pub(crate) identity: &'a ResultIdentity,
    pub(crate) max_rows: usize,
    pub(crate) max_bytes: usize,
    pub(crate) profile: ConstructionProfile,
    pub(crate) recency: &'a dyn Fn(RecencyPage<'_>) -> i64,
}

/// One source of a merged read: an owned read snapshot and the statement
/// lowered for it.
pub(crate) struct ResultSource<'a> {
    pub(crate) snapshot: &'a mut PhysicalProjectionQuerySnapshot,
    pub(crate) statement: &'a SqlQuery,
}

/// What the descriptor row knows about one admitted page, handed to the
/// caller's recency producer. The two backends' walks read different inputs
/// (Direct the stored day and path; Managed the kind and name), so the read
/// offers all four rather than choosing for them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RecencyPage<'a> {
    pub(crate) journal_day: Option<i64>,
    pub(crate) kind: PageKind,
    pub(crate) name: &'a str,
    pub(crate) path: &'a str,
}

/// Why a result read produced no answer. There is no fourth outcome: a read
/// either answers completely, fails, or was cancelled.
#[derive(Debug)]
pub(crate) enum ResultReadError {
    /// The seam refused the statement or the read.
    Sql(MaterializationError),
    /// The projection contradicts itself. The caller fails the read and
    /// schedules the recovery a disposable cache owes (D-3, §5.9/M9).
    Corrupt(String),
    /// The owner cancelled this job. The snapshot is released by the caller.
    Cancelled,
}

/// Shared full-text readiness fact, read from the query's owned image.
pub(crate) const FTS_READY_PROBE_SQL: &str =
    "SELECT phase FROM search_fts_build WHERE singleton = 1";

pub(crate) fn probe_fts_ready(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
) -> Result<bool, ResultReadError> {
    match snapshot.run_projection_query(FTS_READY_PROBE_SQL, &[]) {
        Ok(rows) => Ok(matches!(
            rows.first().and_then(|row| row.first()),
            Some(PhysicalQueryValue::Integer(1))
        )),
        Err(_) if snapshot.cancellation().is_cancelled() => Err(ResultReadError::Cancelled),
        Err(error) => Err(ResultReadError::Sql(error)),
    }
}

impl std::fmt::Display for ResultReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResultReadError::Sql(error) => write!(f, "projection read failed: {error}"),
            ResultReadError::Corrupt(what) => write!(f, "projection is inconsistent: {what}"),
            ResultReadError::Cancelled => write!(f, "query cancelled"),
        }
    }
}

/// The ONE translation from a projection read failure into the public bounded
/// vocabulary (RET2). Free-form `MaterializationError` payloads and corruption
/// descriptions name columns and paths, so they stay on the directed diagnostic
/// channel and never cross this boundary (I-5).
impl From<ResultReadError> for crate::query::QueryExecutionError {
    fn from(error: ResultReadError) -> Self {
        use crate::query::QueryUnavailableReason as Reason;
        match error {
            ResultReadError::Cancelled => Self::Cancelled,
            ResultReadError::Sql(_) => Self::Unavailable(Reason::ReadFailed),
            ResultReadError::Corrupt(_) => Self::Unavailable(Reason::InvalidSnapshot),
        }
    }
}

/// Construct one query's ordered public result from the projection alone.
///
/// The caller owns capacity, snapshot acquisition and validation, the identity
/// capture, the mapping from [`ResultReadError`] to `FailedRead`/recovery, and
/// dropping the snapshot. The compiled-regex table is installed HERE — see
/// [`install_regexes`] — so there is exactly one place that can leave a stale
/// one behind.
pub(crate) fn read_results(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    inputs: &ResultReadInputs<'_>,
) -> Result<PreViewGroups, ResultReadError> {
    read_results_merged(
        &mut [ResultSource {
            snapshot,
            statement: inputs.statement,
        }],
        &inputs.shared(),
    )
}

/// The same construction over ONE OR MORE projections of one graph (R5a).
///
/// The Managed pending route answers from two owned snapshots — the pending
/// overlay, and the accepted projection with every pending page masked out of
/// its statement — and the two descriptor streams are merged BEFORE the budget
/// is charged (plan §2F). Merged order is charge order, so `total`, `exceeded`
/// and the cut point are exactly the walk's over the same pending state.
///
/// **The merge key is the descriptor statement's OWN ordering**, read off the
/// row that statement already selects: Managed Storage `(pages.path` under
/// SQLite's BINARY collation, which is `String::cmp` on the UTF-8 bytes,
/// `query_block_results.preorder)`, Direct Files `(query_page_order.position,
/// preorder)`. Nothing is added to the SELECT list — `admit_descriptor` rejects
/// any row whose width is not [`descriptor_column::COLUMNS`].
///
/// **The LAST source is streamed; every earlier source is BUFFERED in full.**
/// A buffered source may not be truncated with a `LIMIT`: `total` counts the
/// matches SEEN, which is why both callers lower with `cutoff: None`. The
/// buffer therefore holds the DECODED row — its merge key, its public identity
/// and its counts, with the page's own fields shared by `Rc` across the page's
/// blocks — and never the raw `PhysicalQueryValue` vector.
///
/// **With exactly one source this IS the single-source read**, statement for
/// statement and batch for batch: nothing is buffered, nothing is compared, and
/// the streamed source is the only one.
///
/// The sources' pages must be DISJOINT — the Managed pending route masks every
/// pending page out of the accepted statement — because [`PageGroups`] is keyed
/// by physical page id. A page id that reaches two sources is
/// [`ResultReadError::Corrupt`], never a page emitted or counted twice.
pub(crate) fn read_results_merged(
    sources: &mut [ResultSource<'_>],
    shared: &ResultReadShared<'_>,
) -> Result<PreViewGroups, ResultReadError> {
    let carried = read_results_carried::<PlainResults>(sources, shared)?;
    Ok(PreViewGroups {
        // A field move per GROUP, never a conversion per block: the ordinary
        // carrier's block vector IS `Vec<BlockDto>` already.
        groups: carried.groups.into_iter().map(RefGroup::from).collect(),
        recency_by_page: carried.recency_by_page,
        total: carried.total,
        exceeded: carried.exceeded,
    })
}

/// The SAME merged construction, with each admitted entry's physical locator
/// retained (RET3).
///
/// Statement for statement and admission rule for admission rule this is
/// [`read_results_merged`]: one shared collector, one shared payload decoder,
/// one shared corruption vocabulary. The locator is attached in
/// [`emit_batch`], where the descriptor's source, physical page id and physical
/// block id are all still in hand — never inferred afterwards from a public id,
/// a display name, raw text or a DTO comparison.
///
/// The locators are valid ONLY for the `sources` slice passed here, for as long
/// as the caller holds those snapshots.
pub(crate) fn read_located_results_merged(
    sources: &mut [ResultSource<'_>],
    shared: &ResultReadShared<'_>,
) -> Result<LocatedPreViewGroups, ResultReadError> {
    let carried = read_results_carried::<LocatedResults>(sources, shared)?;
    Ok(LocatedPreViewGroups {
        groups: carried.groups,
        recency_by_page: carried.recency_by_page,
        total: carried.total,
        exceeded: carried.exceeded,
    })
}

/// The ONE merged result construction, over whichever carrier the caller wants
/// its admitted rows kept in.
fn read_results_carried<C: ResultCarrier>(
    sources: &mut [ResultSource<'_>],
    shared: &ResultReadShared<'_>,
) -> Result<CarriedGroups<C>, ResultReadError> {
    // Per CONNECTION, not per statement: the compiled-regex table is snapshot
    // state, so each source installs its own statement's program (they compile
    // the same leaves, so the two programs agree).
    for source in sources.iter_mut() {
        install_regexes(source.snapshot, source.statement)?;
    }
    let mut pages = PageGroups::<C>::default();
    let mut budget = ConstructionBudget::new(shared.max_rows, shared.max_bytes);
    let admitted = read_descriptors_merged(sources, shared, &mut pages, &mut budget)?;
    // The payload of the rows the budget admitted, per source over its OWN
    // snapshot. A group's blocks all come from one source (the pages are
    // disjoint), so per-source batching keeps each page's `owner_id, ordinal`
    // order exactly as one source produces it.
    for (at, (source, admitted)) in sources.iter_mut().zip(admitted.iter()).enumerate() {
        read_payload(source.snapshot, &mut pages, admitted, at)?;
    }
    Ok(pages.finish(shared, budget))
}

/// [`PreViewGroups`] before the carrier is known — what
/// [`read_results_carried`] answers.
struct CarriedGroups<C: ResultCarrier> {
    groups: Vec<ResultViewGroup<C::Block>>,
    recency_by_page: HashMap<String, i64>,
    total: usize,
    exceeded: bool,
}

/// One `@page` answer, in the SAME shape the retired page walk returned.
///
/// `total` is the number of rows ADMITTED, not the number seen: the walk's page
/// loop stops at `max_rows` and reports `pages.len()`, which is a different
/// rule from the block budget's and is preserved here rather than unified.
#[derive(Debug, Default)]
pub(crate) struct PageAnswer {
    pub(crate) pages: Vec<crate::query::ir::PageRow>,
    pub(crate) total: usize,
    pub(crate) exceeded: bool,
}

/// One `@page` source: an owned read snapshot and the statement lowered for it.
pub(crate) struct PageSource<'a> {
    pub(crate) snapshot: &'a mut PhysicalProjectionQuerySnapshot,
    pub(crate) statement: &'a SqlQuery,
}

/// Construct one `@page` query's ordered public rows from the projection alone.
///
/// No `PageDto`, no `Document`, no parsed cache: the page index — name, kind and
/// journal day — is what an `@page` answer is made of (K16), and the compiler
/// already selected exactly the matching pages. The caller owns capacity, the
/// snapshot and its lifecycle, exactly as it does for [`read_results`].
pub(crate) fn read_page_results(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    statement: &SqlQuery,
    order: BackendOrder,
    max_rows: usize,
) -> Result<PageAnswer, ResultReadError> {
    read_page_results_merged(
        &mut [PageSource {
            snapshot,
            statement,
        }],
        order,
        max_rows,
    )
}

/// The same construction over ONE OR MORE projections of one graph — the
/// Managed pending route's overlay plus its masked accepted projection (R5a).
///
/// The merge rule is the block read's: every source but the LAST is buffered in
/// full, the last is streamed, and a buffered row is admitted as soon as it
/// sorts at or before the streamed row. A page row is two small strings, so the
/// buffer is bounded by the pending overlay's page count. Merged order is
/// admission order, so `total` and `exceeded` are the walk's over the same
/// pending state. Sources must be DISJOINT (the accepted statement masks every
/// pending page); a page id reaching two sources is
/// [`ResultReadError::Corrupt`], never a page emitted twice.
pub(crate) fn read_page_results_merged(
    sources: &mut [PageSource<'_>],
    order: BackendOrder,
    max_rows: usize,
) -> Result<PageAnswer, ResultReadError> {
    for source in sources.iter_mut() {
        install_regexes(source.snapshot, source.statement)?;
    }
    let Some((streamed, earlier)) = sources.split_last_mut() else {
        return Ok(PageAnswer::default());
    };
    let mut seen: HashSet<[u8; 16]> = HashSet::new();
    let mut buffered: Vec<PageRowRead> = Vec::new();
    for source in earlier.iter_mut() {
        buffer_page_rows(source, order, &mut buffered, &mut seen)?;
    }
    if earlier.len() > 1 {
        // Each source arrives in key order already; the stable sort only
        // interleaves them and keeps the sources' given order on a tie.
        buffered.sort_by(|left, right| left.key(order).cmp(&right.key(order)));
    }
    let mut buffered = buffered.into_iter().peekable();

    if streamed.snapshot.cancellation().is_cancelled() {
        return Err(ResultReadError::Cancelled);
    }
    let statement = page_statement(streamed.statement, order).map_err(ResultReadError::Sql)?;
    let mut answer = PageAnswer::default();
    let mut damage: Option<String> = None;
    let visit =
        streamed
            .snapshot
            .visit_projection_query(&statement.sql, &statement.params, |row| {
                #[cfg(test)]
                note(|census| census.page_rows += 1);
                let decoded = match decode_page_row(row, order) {
                    Ok(decoded) => decoded,
                    Err(what) => {
                        damage = Some(what);
                        return Ok(std::ops::ControlFlow::Break(()));
                    }
                };
                if !seen.insert(decoded.page_id) {
                    damage = Some("page in two sources".to_string());
                    return Ok(std::ops::ControlFlow::Break(()));
                }
                let key = decoded.key(order);
                while buffered.peek().is_some_and(|held| held.key(order) <= key)
                    && admit_page(buffered.peek().expect("peeked"), &mut answer, max_rows)
                {
                    buffered.next();
                }
                if answer.exceeded {
                    return Ok(std::ops::ControlFlow::Break(()));
                }
                if !admit_page(&decoded, &mut answer, max_rows) {
                    return Ok(std::ops::ControlFlow::Break(()));
                }
                Ok(std::ops::ControlFlow::Continue(()))
            });
    if let Err(error) = visit {
        return Err(sql_or_cancelled(streamed.snapshot, error));
    }
    if let Some(what) = damage {
        return Err(ResultReadError::Corrupt(what));
    }
    // The stream ended: everything still buffered sorts after its last row.
    if !answer.exceeded {
        for held in buffered {
            if !admit_page(&held, &mut answer, max_rows) {
                break;
            }
        }
    }
    Ok(answer)
}

/// One buffered source's whole page stream, decoded in statement order.
fn buffer_page_rows(
    source: &mut PageSource<'_>,
    order: BackendOrder,
    buffered: &mut Vec<PageRowRead>,
    seen: &mut HashSet<[u8; 16]>,
) -> Result<(), ResultReadError> {
    if source.snapshot.cancellation().is_cancelled() {
        return Err(ResultReadError::Cancelled);
    }
    let statement = page_statement(source.statement, order).map_err(ResultReadError::Sql)?;
    let mut damage: Option<String> = None;
    let visit = source
        .snapshot
        .visit_projection_query(&statement.sql, &statement.params, |row| {
            #[cfg(test)]
            note(|census| census.page_rows += 1);
            match decode_page_row(row, order) {
                Ok(decoded) => {
                    if !seen.insert(decoded.page_id) {
                        damage = Some("page in two sources".to_string());
                        return Ok(std::ops::ControlFlow::Break(()));
                    }
                    buffered.push(decoded);
                    Ok(std::ops::ControlFlow::Continue(()))
                }
                Err(what) => {
                    damage = Some(what);
                    Ok(std::ops::ControlFlow::Break(()))
                }
            }
        });
    if let Err(error) = visit {
        return Err(sql_or_cancelled(source.snapshot, error));
    }
    match damage {
        Some(what) => Err(ResultReadError::Corrupt(what)),
        None => Ok(()),
    }
}

/// The walk's page-loop admission, transcribed: the cap is checked BEFORE the
/// push, so exactly `max_rows` matches fill the answer without setting
/// `exceeded`, and the `max_rows + 1`-th match sets it and stops. `false` means
/// "stop" — either the cap closed or, at `max_rows == 0`, it was closed from
/// the first row. `max_bytes` is deliberately not charged: the page walk never
/// charged it.
fn admit_page(row: &PageRowRead, answer: &mut PageAnswer, max_rows: usize) -> bool {
    if answer.pages.len() >= max_rows {
        answer.exceeded = true;
        return false;
    }
    answer.pages.push(crate::query::ir::PageRow {
        name: row.name.clone(),
        kind: row.kind,
        journal_day: row.journal_day,
    });
    answer.total = answer.pages.len();
    true
}

/// One decoded `@page` row: the public answer's three fields plus the order
/// keys and the identity the merge and the damage checks read.
struct PageRowRead {
    page_id: [u8; 16],
    name: String,
    kind: PageKind,
    journal_day: Option<i64>,
    path: String,
    position: Option<i64>,
}

impl PageRowRead {
    /// The order the page statement itself imposed, as a comparable key — the
    /// same shape [`merge_key`] uses for block rows.
    fn key(&self, order: BackendOrder) -> (Option<i64>, &str) {
        match order {
            BackendOrder::Direct => (self.position, ""),
            BackendOrder::Managed => (None, self.path.as_str()),
        }
    }
}

/// Column offsets of the page row, in the order [`page_statement`] selects them.
mod page_column {
    pub(super) const PAGE_ID: usize = 0;
    pub(super) const NAME: usize = 1;
    pub(super) const TEXT_KIND: usize = 2;
    pub(super) const JOURNAL_DAY: usize = 3;
    pub(super) const PATH: usize = 4;
    pub(super) const POSITION: usize = 5;
    pub(super) const COLUMNS: usize = 6;
}

/// One page row: validate its identity, its kind and its order key. A row that
/// does not decode is damage and fails the read; it is never a page silently
/// missing from the answer (D-3).
fn decode_page_row(row: &[PhysicalQueryValue], order: BackendOrder) -> Result<PageRowRead, String> {
    use page_column as column;
    if row.len() != column::COLUMNS {
        return Err(format!(
            "page row has {} columns, expected {}",
            row.len(),
            column::COLUMNS
        ));
    }
    let page_id = blob16(row, column::PAGE_ID, "page row page_id")?;
    let name = text(row, column::NAME, "pages.name")?;
    let text_kind = integer(row, column::TEXT_KIND, "pages.text_kind")?;
    let Some(kind) = page_kind_from_sql(text_kind) else {
        return Err(format!("pages.text_kind {text_kind} is not a page kind"));
    };
    let journal_day = opt_integer(row, column::JOURNAL_DAY, "pages.journal_day")?;
    let path = text(row, column::PATH, "pages.path")?;
    let position = opt_integer(row, column::POSITION, "query_page_order.position")?;
    // Direct Files' page order IS this column (see `decode_descriptor`).
    if order == BackendOrder::Direct && position.is_none() {
        return Err("query_page_order has no position for a matched page".to_string());
    }
    Ok(PageRowRead {
        page_id,
        name,
        kind,
        journal_day,
        path,
        position,
    })
}

/// §4.3.2's compiled-regex table for THIS statement, installed unconditionally.
///
/// Unconditional because the table is REPLACED rather than added to: a snapshot
/// that answered a regex query and then a plain one must not still be able to
/// resolve the first statement's ids. Installing the empty program is what
/// makes that true (`QueryRegexProgram::predicate` errors on an id it does not
/// name, so a drifted table fails the read instead of matching nothing).
fn install_regexes(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    statement: &SqlQuery,
) -> Result<(), ResultReadError> {
    let predicate = statement.regexes.predicate();
    snapshot
        .set_query_regex_predicate(predicate)
        .map_err(|error| sql_or_cancelled(snapshot, error))
}

/// A snapshot error, classified. Cancellation surfaces through the snapshot as
/// an ordinary `Incomplete`, so the cancellation flag — not the message — is
/// what distinguishes "the owner stopped this job" from "the read failed".
pub(crate) fn sql_or_cancelled(
    snapshot: &PhysicalProjectionQuerySnapshot,
    error: MaterializationError,
) -> ResultReadError {
    if snapshot.cancellation().is_cancelled() {
        ResultReadError::Cancelled
    } else {
        ResultReadError::Sql(error)
    }
}

/// One selected block, as the descriptor read describes it. No payload: the raw
/// text, tags and properties of a row nobody admits are never read.
struct Descriptor {
    block_id: [u8; 16],
    /// The PHYSICAL page this row's block belongs to. Beside `page` (the
    /// group's index), not instead of it: the payload's ownership check and
    /// the export locator both name the physical id, while the group index is
    /// where the DTO is pushed.
    page_id: [u8; 16],
    page: usize,
    /// The public id this row will carry, already resolved through the captured
    /// identity policy.
    result_id: String,
    /// The stored construction estimate, with the identity term adjusted when
    /// the public id is not the stored one.
    estimated_bytes: usize,
    tag_count: usize,
    property_count: usize,
}

/// One result page: the group under construction plus the two fields the
/// recency axis needs, kept out of the group because they are inputs and not
/// part of the answer.
struct PageGroup<C: ResultCarrier> {
    page_id: [u8; 16],
    group: ResultViewGroup<C::Block>,
    journal_day: Option<i64>,
    path: String,
}

/// The groups in BASE order, one per PHYSICAL page.
///
/// Keyed by `page_id`, so two physical pages that happen to share a display
/// name stay two groups here exactly as they are two pages in the walk;
/// `base_order_groups`/`finish_query_groups` merges them for display later, the
/// same way and in the same place as today.
struct PageGroups<C: ResultCarrier> {
    order: Vec<PageGroup<C>>,
    by_page: HashMap<[u8; 16], usize>,
}

// Derived `Default` would demand `C: Default`, which no carrier is: the
// carrier is a type-level switch and never a value.
impl<C: ResultCarrier> Default for PageGroups<C> {
    fn default() -> Self {
        Self {
            order: Vec::new(),
            by_page: HashMap::new(),
        }
    }
}

impl<C: ResultCarrier> PageGroups<C> {
    /// The group for one page, created on first appearance so the group order
    /// is the descriptor order.
    fn slot(
        &mut self,
        page_id: [u8; 16],
        name: &str,
        kind: PageKind,
        journal_day: Option<i64>,
        path: &str,
    ) -> usize {
        if let Some(at) = self.by_page.get(&page_id) {
            return *at;
        }
        let at = self.order.len();
        self.order.push(PageGroup {
            page_id,
            group: ResultViewGroup {
                page: name.to_owned(),
                kind,
                blocks: Vec::new(),
                evidence: Vec::new(),
            },
            journal_day,
            path: path.to_owned(),
        });
        self.by_page.insert(page_id, at);
        at
    }

    /// The pre-view answer: groups that admitted nothing are dropped (the walk
    /// pushes a group only for a non-empty `matched`), and the recency axis is
    /// measured once per surviving page and only when the view needs it.
    fn finish(self, inputs: &ResultReadShared<'_>, budget: ConstructionBudget) -> CarriedGroups<C> {
        let mut groups = Vec::with_capacity(self.order.len());
        let mut recency_by_page = HashMap::new();
        for page in self.order {
            if page.group.blocks.is_empty() {
                continue;
            }
            if inputs.profile.want_recency {
                recency_by_page.insert(
                    page.group.page.clone(),
                    (inputs.recency)(RecencyPage {
                        journal_day: page.journal_day,
                        kind: page.group.kind,
                        name: &page.group.page,
                        path: &page.path,
                    }),
                );
            }
            groups.push(page.group);
        }
        CarriedGroups {
            groups,
            recency_by_page,
            total: budget.total,
            exceeded: budget.exceeded,
        }
    }
}

/// Column offsets of the descriptor row, in the order
/// [`descriptor_statement`] selects them.
mod descriptor_column {
    pub(super) const BLOCK_ID: usize = 0;
    pub(super) const PAGE_ID: usize = 1;
    pub(super) const NAME: usize = 2;
    pub(super) const TEXT_KIND: usize = 3;
    pub(super) const JOURNAL_DAY: usize = 4;
    pub(super) const PATH: usize = 5;
    pub(super) const RESULT_PAGE_ID: usize = 6;
    pub(super) const PREORDER: usize = 7;
    pub(super) const RESULT_ID: usize = 8;
    pub(super) const ESTIMATED_BYTES: usize = 9;
    pub(super) const TAG_COUNT: usize = 10;
    pub(super) const PROPERTY_COUNT: usize = 11;
    pub(super) const ORDER_KEY: usize = 12;
    pub(super) const POSITION: usize = 13;
    pub(super) const COLUMNS: usize = 14;
}

/// Walk the ordered descriptors of every source ONCE, charging
/// [`ConstructionBudget`] exactly as `collect_sql_matched_blocks` does, in the
/// order the walk charges it, and keep only what was admitted.
///
/// The budget rules are transcribed in the walk's order and not re-derived:
/// the unsorted `(sample N)` cap STOPS counting (which is what makes its
/// `total` the truncated count), a closed budget `deny_match`es, and an
/// over-budget row is counted without being emitted. Everything a denied row
/// would have carried is dropped here, so the STREAMED source's peak memory is
/// bounded by `max_rows` rather than by the size of the match set; a BUFFERED
/// source's is bounded by its own match set, which is why only the small
/// pending overlay is ever buffered.
///
/// A closed budget does NOT stop a stream: every remaining row is still visited
/// and still `deny_match`ed, because `total` is the number of matches SEEN. The
/// only early exit is the `(sample N)` cap's `ControlFlow::Break`, and it stops
/// BOTH the streamed source and the remaining buffered rows at once, so the
/// truncation point is exactly the walk's.
fn read_descriptors_merged<C: ResultCarrier>(
    sources: &mut [ResultSource<'_>],
    shared: &ResultReadShared<'_>,
    pages: &mut PageGroups<C>,
    budget: &mut ConstructionBudget,
) -> Result<Vec<Vec<Descriptor>>, ResultReadError> {
    let mut admitted: Vec<Vec<Descriptor>> = (0..sources.len()).map(|_| Vec::new()).collect();
    let Some((streamed, earlier)) = sources.split_last_mut() else {
        return Ok(admitted);
    };
    let streamed_at = earlier.len();

    // Every source but the last, read in full into one key-ordered buffer.
    // With one source this loop does not run and nothing is allocated.
    let mut buffered: Vec<BufferedDescriptor> = Vec::new();
    let mut buffered_pages: HashSet<[u8; 16]> = HashSet::new();
    for (at, source) in earlier.iter_mut().enumerate() {
        buffer_descriptors(source, shared, at, &mut buffered, &mut buffered_pages)?;
    }
    if earlier.len() > 1 {
        // Each source's own stream already arrives in key order, so this only
        // interleaves them; the sort is stable, so equal keys keep the sources'
        // given order (which is the walk's concatenation order).
        buffered.sort_by(|left, right| {
            merge_key(shared.order, &left.page, &left.row).cmp(&merge_key(
                shared.order,
                &right.page,
                &right.row,
            ))
        });
    }
    let mut buffered = buffered.into_iter().peekable();

    if streamed.snapshot.cancellation().is_cancelled() {
        return Err(ResultReadError::Cancelled);
    }
    let statement =
        descriptor_statement(streamed.statement, shared.order).map_err(ResultReadError::Sql)?;
    let mut damage: Option<String> = None;
    let mut capped = false;
    let visit =
        streamed
            .snapshot
            .visit_projection_query(&statement.sql, &statement.params, |row| {
                #[cfg(test)]
                note(|census| census.descriptor_rows += 1);
                let (page, decoded) = match decode_descriptor(row, shared) {
                    Ok(decoded) => decoded,
                    Err(what) => {
                        damage = Some(what);
                        return Ok(std::ops::ControlFlow::Break(()));
                    }
                };
                // The sources are disjoint by masking; a page in two of them would
                // be counted twice and emitted twice, which is the one thing a
                // damaged disposable cache may never do (D-3).
                if buffered_pages.contains(&page.page_id) {
                    damage = Some("page in two sources".to_string());
                    return Ok(std::ops::ControlFlow::Break(()));
                }
                let key = merge_key(shared.order, &page, &decoded);
                // Everything buffered that sorts at or before this row is charged
                // FIRST. `<=` rather than `<` is the walk's own tie-break: its
                // `sources.sort_by` is stable over `[overlay pages…, accepted
                // pages…]`. Disjoint sources cannot actually tie.
                while buffered
                    .peek()
                    .is_some_and(|held| merge_key(shared.order, &held.page, &held.row) <= key)
                {
                    let held = buffered.next().expect("peeked");
                    if admit_decoded(
                        &held.page,
                        held.row,
                        shared,
                        pages,
                        budget,
                        &mut admitted[held.source],
                    )
                    .is_break()
                    {
                        capped = true;
                        return Ok(std::ops::ControlFlow::Break(()));
                    }
                }
                let flow = admit_decoded(
                    &page,
                    decoded,
                    shared,
                    pages,
                    budget,
                    &mut admitted[streamed_at],
                );
                if flow.is_break() {
                    capped = true;
                }
                Ok(flow)
            });
    if let Err(error) = visit {
        return Err(sql_or_cancelled(streamed.snapshot, error));
    }
    if let Some(what) = damage {
        return Err(ResultReadError::Corrupt(what));
    }
    // The stream ended: everything still buffered sorts after its last row.
    if !capped {
        for held in buffered {
            if admit_decoded(
                &held.page,
                held.row,
                shared,
                pages,
                budget,
                &mut admitted[held.source],
            )
            .is_break()
            {
                break;
            }
        }
    }
    Ok(admitted)
}

/// One buffered source's whole descriptor stream, decoded in statement order.
///
/// No budget is charged here: the budget belongs to the MERGED order, which is
/// not known until the streamed source's rows interleave with these.
fn buffer_descriptors(
    source: &mut ResultSource<'_>,
    shared: &ResultReadShared<'_>,
    at: usize,
    buffered: &mut Vec<BufferedDescriptor>,
    buffered_pages: &mut HashSet<[u8; 16]>,
) -> Result<(), ResultReadError> {
    if source.snapshot.cancellation().is_cancelled() {
        return Err(ResultReadError::Cancelled);
    }
    let statement =
        descriptor_statement(source.statement, shared.order).map_err(ResultReadError::Sql)?;
    let mut damage: Option<String> = None;
    let mut current: Option<Rc<DescriptorPage>> = None;
    let visit = source
        .snapshot
        .visit_projection_query(&statement.sql, &statement.params, |row| {
            #[cfg(test)]
            note(|census| census.descriptor_rows += 1);
            match decode_descriptor(row, shared) {
                Ok((page, decoded)) => {
                    // The stream is ordered by page, so ONE `Rc` per page
                    // serves all of its rows: a 9 999-block pending page pays
                    // for its name and path once, not once per block.
                    let page = match current.as_ref().filter(|held| held.page_id == page.page_id) {
                        Some(held) => Rc::clone(held),
                        None => {
                            let held = Rc::new(page);
                            buffered_pages.insert(held.page_id);
                            current = Some(Rc::clone(&held));
                            held
                        }
                    };
                    #[cfg(test)]
                    note_buffered_bytes(&page, &decoded, Rc::strong_count(&page) == 2);
                    buffered.push(BufferedDescriptor {
                        source: at,
                        page,
                        row: decoded,
                    });
                    Ok(std::ops::ControlFlow::Continue(()))
                }
                Err(what) => {
                    damage = Some(what);
                    Ok(std::ops::ControlFlow::Break(()))
                }
            }
        });
    if let Err(error) = visit {
        return Err(sql_or_cancelled(source.snapshot, error));
    }
    match damage {
        Some(what) => Err(ResultReadError::Corrupt(what)),
        None => Ok(()),
    }
}

/// One buffered descriptor: which source produced it, its page (shared with
/// that page's other blocks) and the row itself.
struct BufferedDescriptor {
    source: usize,
    page: Rc<DescriptorPage>,
    row: DecodedDescriptor,
}

/// What the descriptor row says about one PAGE. Decoded once per page in a
/// buffered stream, once per row in the streamed one (where it is dropped
/// immediately after admission).
struct DescriptorPage {
    page_id: [u8; 16],
    name: String,
    kind: PageKind,
    journal_day: Option<i64>,
    path: String,
    /// Direct Files' cross-page base order. `None` is damage on that backend
    /// and unread on Managed Storage.
    position: Option<i64>,
}

/// One descriptor row, decoded and identity-resolved, without the raw
/// `PhysicalQueryValue` vector and without the two columns already consumed:
/// `blocks.order_key` and the stored id, which [`resolve_identity`] folded into
/// `result_id` and `estimated_bytes`.
struct DecodedDescriptor {
    block_id: [u8; 16],
    preorder: i64,
    result_id: String,
    estimated_bytes: usize,
    tag_count: usize,
    property_count: usize,
}

/// The order the descriptor statement itself imposed, as a comparable key.
///
/// It is not a second ordering policy: `descriptor_statement` ends
/// `ORDER BY {base}, q.preorder` with `base` = `o.position` (Direct Files) or
/// `p.path` (Managed Storage), and both columns are on the row already. Paths
/// compare as byte strings, which is what SQLite's BINARY collation does and
/// what the walk's own `sources.sort_by` does.
fn merge_key<'a>(
    order: BackendOrder,
    page: &'a DescriptorPage,
    row: &DecodedDescriptor,
) -> (Option<i64>, &'a str, i64) {
    match order {
        BackendOrder::Direct => (page.position, "", row.preorder),
        BackendOrder::Managed => (None, page.path.as_str(), row.preorder),
    }
}

/// One descriptor row: validate it and resolve its public identity. Nothing
/// here touches the budget or the groups — a buffered row is decoded long
/// before it is charged.
fn decode_descriptor(
    row: &[PhysicalQueryValue],
    inputs: &ResultReadShared<'_>,
) -> Result<(DescriptorPage, DecodedDescriptor), String> {
    use descriptor_column as column;
    if row.len() != column::COLUMNS {
        return Err(format!(
            "descriptor row has {} columns, expected {}",
            row.len(),
            column::COLUMNS
        ));
    }
    let block_id = blob16(row, column::BLOCK_ID, "descriptor block_id")?;
    let page_id = blob16(row, column::PAGE_ID, "descriptor page_id")?;
    // A LEFT JOIN that found nothing is the shape this read exists to catch:
    // the selected block IS in the answer, so its missing metadata is damage
    // and never a dropped row (D-3).
    let name = text(row, column::NAME, "pages.name")?;
    let text_kind = integer(row, column::TEXT_KIND, "pages.text_kind")?;
    let Some(kind) = page_kind_from_sql(text_kind) else {
        return Err(format!("pages.text_kind {text_kind} is not a page kind"));
    };
    let journal_day = opt_integer(row, column::JOURNAL_DAY, "pages.journal_day")?;
    let path = text(row, column::PATH, "pages.path")?;
    let result_page = blob16(row, column::RESULT_PAGE_ID, "query_block_results.page_id")?;
    if result_page != page_id {
        return Err("query_block_results.page_id does not own its block's page".to_string());
    }
    let preorder = integer(row, column::PREORDER, "query_block_results.preorder")?;
    if preorder < 0 {
        return Err("query_block_results.preorder is negative".to_string());
    }
    let stored_id = text(row, column::RESULT_ID, "query_block_results.result_id")?;
    if stored_id.is_empty() {
        return Err("query_block_results.result_id is empty".to_string());
    }
    let stored_estimate = count(
        row,
        column::ESTIMATED_BYTES,
        "query_block_results.estimated_bytes",
    )?;
    let tag_count = count(row, column::TAG_COUNT, "query_block_results.tag_count")?;
    let property_count = count(
        row,
        column::PROPERTY_COUNT,
        "query_block_results.property_count",
    )?;
    let order_key = text(row, column::ORDER_KEY, "blocks.order_key")?;
    let position = opt_integer(row, column::POSITION, "query_page_order.position")?;
    // Direct Files' cross-page order IS this column; a NULL would silently
    // sort a page to one end of the answer, which changes which rows survive a
    // truncated budget.
    if inputs.order == BackendOrder::Direct && position.is_none() {
        return Err("query_page_order has no position for a result page".to_string());
    }
    let (result_id, estimated_bytes) = resolve_identity(
        inputs.identity,
        page_id,
        &path,
        &order_key,
        &stored_id,
        stored_estimate,
    )?;
    Ok((
        DescriptorPage {
            page_id,
            name,
            kind,
            journal_day,
            path,
            position,
        },
        DecodedDescriptor {
            block_id,
            preorder,
            result_id,
            estimated_bytes,
            tag_count,
            property_count,
        },
    ))
}

/// Offer one decoded descriptor to the budget, in MERGED order.
///
/// The four rules, in `collect_sql_matched_blocks`' own order.
fn admit_decoded<C: ResultCarrier>(
    page: &DescriptorPage,
    row: DecodedDescriptor,
    inputs: &ResultReadShared<'_>,
    pages: &mut PageGroups<C>,
    budget: &mut ConstructionBudget,
    admitted: &mut Vec<Descriptor>,
) -> std::ops::ControlFlow<()> {
    if inputs
        .profile
        .sample_admission_cap
        .is_some_and(|cap| budget.rows >= cap)
    {
        // Stop COUNTING here: an unsorted `(sample N)` reports the truncated
        // count as its total, and the walk stops at the first matched block it
        // sees after the cap on every remaining page.
        return std::ops::ControlFlow::Break(());
    }
    if budget.closed() {
        budget.deny_match();
        return std::ops::ControlFlow::Continue(());
    }
    let at = pages.slot(
        page.page_id,
        &page.name,
        page.kind,
        page.journal_day,
        &page.path,
    );
    if !budget.admit_estimated(&page.name, row.estimated_bytes) {
        return std::ops::ControlFlow::Continue(());
    }
    admitted.push(Descriptor {
        block_id: row.block_id,
        page_id: page.page_id,
        page: at,
        result_id: row.result_id,
        estimated_bytes: row.estimated_bytes,
        tag_count: row.tag_count,
        property_count: row.property_count,
    });
    std::ops::ControlFlow::Continue(())
}

/// The public id of one admitted row and the construction estimate that goes
/// with it.
///
/// The stored estimate describes the STORED public identity. When a fresh
/// Direct session resolves a structural id instead, only that one term moves:
/// subtract the stored id's bytes, add the canonical UUID's 36. The arithmetic
/// is checked because a stored estimate smaller than its own identity term is a
/// contradiction, and a saturating subtraction would hide it.
pub(crate) fn resolve_identity(
    identity: &ResultIdentity,
    page_id: [u8; 16],
    path: &str,
    order_key: &str,
    stored_id: &str,
    stored_estimate: usize,
) -> Result<(String, usize), String> {
    let structural = match identity {
        ResultIdentity::Stored => false,
        ResultIdentity::DirectStructural {
            session_pages,
            all_session,
        } => !*all_session && !session_pages.contains(&page_id),
    };
    if !structural {
        return Ok((stored_id.to_owned(), stored_estimate));
    }
    let resolved = doc_runtime_id_for_order(path, order_key)
        .map_err(|error| format!("stored structural order does not resolve an id: {error}"))?
        .to_string();
    let estimate = stored_estimate
        .checked_sub(stored_id.len())
        .and_then(|rest| rest.checked_add(resolved.len()))
        .ok_or_else(|| "stored estimate is smaller than its own identity term".to_string())?;
    Ok((resolved, estimate))
}

/// Which consumer a payload batch belongs to.
///
/// The statements, the batch size and every validation are identical; only the
/// test-only census counter differs, so a gate can report SELECTION payload
/// (the shallow rows a view is built from) separately from EXPORT OUTPUT
/// payload (the admitted descendants of a subtree), which is the whole claim
/// RET3's export core makes about its work.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PayloadChannel {
    Selection,
    ExportOutput,
}

/// What one ADMITTED row tells the shared payload reader about itself.
///
/// Borrowed rather than owned: an ordinary read already holds these six facts
/// on its `Descriptor` and must not pay a second `String` for them.
pub(crate) struct PayloadFacts<'a> {
    pub(crate) block_id: [u8; 16],
    pub(crate) page_id: [u8; 16],
    pub(crate) result_id: &'a str,
    pub(crate) estimated_bytes: usize,
    pub(crate) tag_count: usize,
    pub(crate) property_count: usize,
}

/// **The ONE shallow payload reader**, for ordinary results and for export
/// output alike.
///
/// Three bound statements per batch of [`PAYLOAD_BATCH`] ids, all through the
/// SAME snapshot. Never one statement per block (that is the N+1 R3 removed)
/// and never a tags×properties join (that is a cross product whose row count is
/// the product of two independent facets).
///
/// Every check is a "the projection contradicts itself" check and every one of
/// them abandons the WHOLE read rather than the row: exact coverage, page
/// ownership, the stored tag/property counts, and the stored estimate against
/// the estimate of the DTO actually built. `emit` receives the row's index in
/// `rows` and its finished DTO, in admission order.
pub(crate) fn read_admitted_payload<R>(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    rows: &[R],
    channel: PayloadChannel,
    facts: impl for<'r> Fn(&'r R) -> PayloadFacts<'r>,
    mut emit: impl FnMut(usize, BlockDto),
) -> Result<(), ResultReadError> {
    for (index, batch) in rows.chunks(PAYLOAD_BATCH).enumerate() {
        #[cfg(test)]
        run_before_payload_batch_hook(channel, index);
        #[cfg(not(test))]
        let _ = index;
        // Between batches, not inside one: a cancelled job stops at the next
        // statement boundary and its snapshot is released by the owner.
        if snapshot.cancellation().is_cancelled() {
            return Err(ResultReadError::Cancelled);
        }
        let ids = batch
            .iter()
            .map(|row| PhysicalQueryValue::Blob(facts(row).block_id.to_vec()))
            .collect::<Vec<_>>();
        let block_facets = read_block_facets(snapshot, &ids, channel)?;
        let tags = read_owner_strings(
            snapshot,
            &ids,
            OwnerList::Tags,
            channel,
            "SELECT owner_id, tag FROM tags \
             WHERE owner_type = {owner} AND owner_id IN ({ids}) \
             ORDER BY owner_id, ordinal",
            |row| text(row, 1, "tags.tag"),
        )?;
        let properties = read_owner_strings(
            snapshot,
            &ids,
            OwnerList::Properties,
            channel,
            "SELECT owner_id, name, value FROM properties \
             WHERE owner_type = {owner} AND owner_id IN ({ids}) \
             ORDER BY owner_id, ordinal",
            |row| {
                Ok((
                    text(row, 1, "properties.name")?,
                    text(row, 2, "properties.value")?,
                ))
            },
        )?;
        emit_batch(
            batch,
            index * PAYLOAD_BATCH,
            &facts,
            &mut emit,
            block_facets,
            tags,
            properties,
        )
        .map_err(ResultReadError::Corrupt)?;
    }
    Ok(())
}

/// The ordinary reader's use of [`read_admitted_payload`]: build each admitted
/// DTO into its own page group, carrying the locator the carrier wants.
fn read_payload<C: ResultCarrier>(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    pages: &mut PageGroups<C>,
    admitted: &[Descriptor],
    source: usize,
) -> Result<(), ResultReadError> {
    read_admitted_payload(
        snapshot,
        admitted,
        PayloadChannel::Selection,
        |descriptor: &Descriptor| PayloadFacts {
            block_id: descriptor.block_id,
            page_id: descriptor.page_id,
            result_id: &descriptor.result_id,
            estimated_bytes: descriptor.estimated_bytes,
            tag_count: descriptor.tag_count,
            property_count: descriptor.property_count,
        },
        |at, dto| {
            let descriptor = &admitted[at];
            // Attached HERE, while the source, the physical page id and the
            // physical block id are all still in hand. Nothing downstream
            // recovers identity by comparing exposed ids or rendered text.
            let locator = ResultLocator {
                source,
                page_id: descriptor.page_id,
                block_id: descriptor.block_id,
            };
            pages.order[descriptor.page]
                .group
                .blocks
                .push(C::carry(dto, locator));
        },
    )
}

/// Which owner-keyed facet list a payload statement reads. Only the census
/// distinguishes them; the read itself is one shape.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OwnerList {
    Tags,
    Properties,
}

/// One admitted block's non-list facets: the block row, its required text, and
/// the two optional facet rows beside them.
struct BlockFacets {
    page_id: [u8; 16],
    collapsed: bool,
    heading_level: Option<u8>,
    raw: String,
    marker: Option<String>,
    priority: Option<String>,
    scheduled: Option<String>,
    deadline: Option<String>,
}

/// Payload statement 1. `block_text` is INNER-joined because its row is
/// required — a block with no text row is damage, and the coverage check below
/// is what reports it.
///
/// The column sources are the producers', verified on both sides: raw text is
/// `block_text.content`; `collapsed`/`heading_level` are `blocks`; the marker
/// is `tasks.marker` (which both producers write ASCII-uppercased, matching
/// `DocBlock::marker()`'s uppercase-only vocabulary); and priority, scheduled
/// and deadline are `block_planning`'s exact strings, which exist WITHOUT a
/// marker and WITHOUT a parseable day (§3.2 M2, R0 §"Result fields").
fn read_block_facets(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    ids: &[PhysicalQueryValue],
    channel: PayloadChannel,
) -> Result<HashMap<[u8; 16], BlockFacets>, ResultReadError> {
    let sql = format!(
        "SELECT b.block_id, b.page_id, b.collapsed, b.heading_level, t.content, \
         k.marker, pl.priority, pl.scheduled, pl.deadline \
         FROM blocks b \
         JOIN block_text t ON t.block_id = b.block_id \
         LEFT JOIN tasks k ON k.block_id = b.block_id \
         LEFT JOIN block_planning pl ON pl.block_id = b.block_id \
         WHERE b.block_id IN ({})",
        placeholders(ids.len())
    );
    #[cfg(test)]
    note(|census| *payload_statements(census, channel) += 1);
    #[cfg(not(test))]
    let _ = channel;
    let rows = snapshot
        .run_projection_query(&sql, ids)
        .map_err(|error| sql_or_cancelled(snapshot, error))?;
    let mut facets = HashMap::with_capacity(rows.len());
    for row in &rows {
        #[cfg(test)]
        note(|census| *payload_block_rows(census, channel) += 1);
        let decoded = decode_block_facets(row)?;
        if facets.insert(decoded.0, decoded.1).is_some() {
            return Err(ResultReadError::Corrupt(
                "one admitted block has two payload rows".to_string(),
            ));
        }
    }
    Ok(facets)
}

fn decode_block_facets(
    row: &[PhysicalQueryValue],
) -> Result<([u8; 16], BlockFacets), ResultReadError> {
    let decode = || -> Result<([u8; 16], BlockFacets), String> {
        if row.len() != 9 {
            return Err(format!(
                "block payload row has {} columns, expected 9",
                row.len()
            ));
        }
        let block_id = blob16(row, 0, "blocks.block_id")?;
        let collapsed = match integer(row, 2, "blocks.collapsed")? {
            0 => false,
            1 => true,
            other => return Err(format!("blocks.collapsed is {other}, not 0 or 1")),
        };
        let heading_level = match opt_integer(row, 3, "blocks.heading_level")? {
            None => None,
            Some(level @ 1..=6) => Some(level as u8),
            Some(other) => return Err(format!("blocks.heading_level is {other}, not 1..=6")),
        };
        Ok((
            block_id,
            BlockFacets {
                page_id: blob16(row, 1, "blocks.page_id")?,
                collapsed,
                heading_level,
                raw: text(row, 4, "block_text.content")?,
                marker: opt_text(row, 5, "tasks.marker")?,
                priority: opt_text(row, 6, "block_planning.priority")?,
                scheduled: opt_text(row, 7, "block_planning.scheduled")?,
                deadline: opt_text(row, 8, "block_planning.deadline")?,
            },
        ))
    };
    decode().map_err(ResultReadError::Corrupt)
}

/// Payload statements 2 and 3: one owner-keyed, ordinal-ordered list per
/// admitted block, with the ordinal the producer's own `enumerate()` per owner.
///
/// The `ORDER BY owner_id, ordinal` is the whole ordering contract: original
/// spelling in original order, and the list is built by appending in row order
/// rather than by sorting a second time.
fn read_owner_strings<T>(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    ids: &[PhysicalQueryValue],
    list: OwnerList,
    channel: PayloadChannel,
    shape: &str,
    decode: impl Fn(&[PhysicalQueryValue]) -> Result<T, String>,
) -> Result<HashMap<[u8; 16], Vec<T>>, ResultReadError> {
    let sql = shape
        .replace("{owner}", &OWNER_BLOCK.to_string())
        .replace("{ids}", &placeholders(ids.len()));
    #[cfg(test)]
    note(|census| *payload_statements(census, channel) += 1);
    #[cfg(not(test))]
    let _ = (list, channel);
    let rows = snapshot
        .run_projection_query(&sql, ids)
        .map_err(|error| sql_or_cancelled(snapshot, error))?;
    let mut owners: HashMap<[u8; 16], Vec<T>> = HashMap::new();
    for row in &rows {
        #[cfg(test)]
        note(|census| match list {
            OwnerList::Tags => *payload_tag_rows(census, channel) += 1,
            OwnerList::Properties => *payload_property_rows(census, channel) += 1,
        });
        let decoded = (|| {
            let owner = blob16(row, 0, "owner_id")?;
            Ok::<_, String>((owner, decode(row)?))
        })()
        .map_err(ResultReadError::Corrupt)?;
        owners.entry(decoded.0).or_default().push(decoded.1);
    }
    Ok(owners)
}

/// Validate one batch and emit its DTOs, in admission order.
///
/// Every check here is a "the projection contradicts itself" check, and every
/// one of them abandons the WHOLE result rather than the row: exact coverage
/// (each admitted id has one and only one payload row, and no row belongs to an
/// id nobody admitted), page ownership, the stored tag/property counts, and
/// finally the stored estimate against the estimate of the DTO that was
/// actually built. That last one is what proves the metadata and the payload
/// describe the same block.
fn emit_batch<R>(
    batch: &[R],
    first: usize,
    facts: &impl for<'r> Fn(&'r R) -> PayloadFacts<'r>,
    emit: &mut impl FnMut(usize, BlockDto),
    mut block_facets: HashMap<[u8; 16], BlockFacets>,
    mut tags: HashMap<[u8; 16], Vec<String>>,
    mut properties: HashMap<[u8; 16], Vec<(String, String)>>,
) -> Result<(), String> {
    for (at, row) in batch.iter().enumerate() {
        let descriptor = facts(row);
        let Some(facet) = block_facets.remove(&descriptor.block_id) else {
            return Err("an admitted block has no payload row".to_string());
        };
        if facet.page_id != descriptor.page_id {
            return Err("a payload block row names a different page".to_string());
        }
        let tags = tags.remove(&descriptor.block_id).unwrap_or_default();
        let properties = properties.remove(&descriptor.block_id).unwrap_or_default();
        if tags.len() != descriptor.tag_count {
            return Err("stored tag_count disagrees with the tag rows".to_string());
        }
        if properties.len() != descriptor.property_count {
            return Err("stored property_count disagrees with the property rows".to_string());
        }
        let dto = shallow_block_facets_dto(ShallowBlockFacets {
            id: descriptor.result_id.to_owned(),
            raw: facet.raw,
            collapsed: facet.collapsed,
            heading_level: facet.heading_level,
            marker: facet.marker,
            priority: facet.priority,
            scheduled: facet.scheduled,
            deadline: facet.deadline,
            tags,
            properties,
        });
        // The SAME estimator the walk charges the budget with, recomputed on
        // the emitted DTO. Equality is what proves the stored estimate and the
        // payload agree; a difference means the row the budget admitted is not
        // the row it is about to return.
        if block_dto_estimated_bytes(&dto) != descriptor.estimated_bytes {
            return Err("the emitted result does not match its stored estimate".to_string());
        }
        emit(first + at, dto);
    }
    if !block_facets.is_empty() {
        return Err("a payload block row belongs to no admitted block".to_string());
    }
    if !tags.is_empty() {
        return Err("a tag row belongs to no admitted block".to_string());
    }
    if !properties.is_empty() {
        return Err("a property row belongs to no admitted block".to_string());
    }
    Ok(())
}

/// `?1, ?2, … ?n`. Two shapes at most reach the connection — a full batch and
/// the final remainder — so `prepare_cached` holds both and neither is
/// recompiled per batch.
pub(crate) fn placeholders(count: usize) -> String {
    (1..=count)
        .map(|at| format!("?{at}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ===== row decoding =====
//
// Every accessor names the COLUMN and the type it found and never the value:
// a decode failure message travels into a receipt and a log, and a projection
// row is user content.

pub(crate) fn blob16(
    row: &[PhysicalQueryValue],
    at: usize,
    what: &str,
) -> Result<[u8; 16], String> {
    match row.get(at) {
        Some(PhysicalQueryValue::Blob(bytes)) if bytes.len() == 16 => {
            Ok(bytes.as_slice().try_into().expect("a checked 16-byte id"))
        }
        Some(PhysicalQueryValue::Blob(bytes)) => {
            Err(format!("{what} is {} bytes, expected 16", bytes.len()))
        }
        other => Err(format!("{what} is {}, expected a blob", spell(other))),
    }
}

pub(crate) fn text(row: &[PhysicalQueryValue], at: usize, what: &str) -> Result<String, String> {
    match row.get(at) {
        Some(PhysicalQueryValue::Text(value)) => Ok(value.clone()),
        other => Err(format!("{what} is {}, expected text", spell(other))),
    }
}

fn opt_text(row: &[PhysicalQueryValue], at: usize, what: &str) -> Result<Option<String>, String> {
    match row.get(at) {
        Some(PhysicalQueryValue::Null) => Ok(None),
        Some(PhysicalQueryValue::Text(value)) => Ok(Some(value.clone())),
        other => Err(format!("{what} is {}, expected text or null", spell(other))),
    }
}

pub(crate) fn integer(row: &[PhysicalQueryValue], at: usize, what: &str) -> Result<i64, String> {
    match row.get(at) {
        Some(PhysicalQueryValue::Integer(value)) => Ok(*value),
        other => Err(format!("{what} is {}, expected an integer", spell(other))),
    }
}

fn opt_integer(row: &[PhysicalQueryValue], at: usize, what: &str) -> Result<Option<i64>, String> {
    match row.get(at) {
        Some(PhysicalQueryValue::Null) => Ok(None),
        Some(PhysicalQueryValue::Integer(value)) => Ok(Some(*value)),
        other => Err(format!(
            "{what} is {}, expected an integer or null",
            spell(other)
        )),
    }
}

/// A non-negative count, as `usize`. The DDL constrains all four of these to be
/// `>= 0`, so a negative one is damage rather than a supported value.
pub(crate) fn count(row: &[PhysicalQueryValue], at: usize, what: &str) -> Result<usize, String> {
    let value = integer(row, at, what)?;
    usize::try_from(value).map_err(|_| format!("{what} is negative"))
}

fn spell(value: Option<&PhysicalQueryValue>) -> &'static str {
    match value {
        None => "absent",
        Some(PhysicalQueryValue::Null) => "null",
        Some(PhysicalQueryValue::Integer(_)) => "an integer",
        Some(PhysicalQueryValue::Real(_)) => "a real",
        Some(PhysicalQueryValue::Text(_)) => "text",
        Some(PhysicalQueryValue::Blob(_)) => "a blob",
    }
}

// ===== test-only census =====

/// The batch arithmetic one result read performed (test-only).
///
/// It exists so a gate can assert the shape of the work rather than its
/// wall-clock: "one descriptor row, three payload statements, one block row"
/// is the claim R3 makes about a huge page with a single match, and a counter
/// is the only thing that can hold it honest.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResultReadCensus {
    pub(crate) descriptor_rows: usize,
    pub(crate) payload_statements: usize,
    pub(crate) payload_block_rows: usize,
    pub(crate) payload_tag_rows: usize,
    pub(crate) payload_property_rows: usize,
    /// `@page` rows the page statement returned (RET1). Beside the descriptor
    /// count, never instead of it: a page answer reads no descriptor and no
    /// payload at all, so a gate that asserts "one page row, zero payload
    /// statements" is asserting the whole cost of the answer.
    pub(crate) page_rows: usize,
    /// The SAME four counters for RET3's export OUTPUT payload — the admitted
    /// descendants of a subtree. Separate fields, not a second census, because
    /// the claim "no payload was read for a rejected descendant" is only
    /// legible beside the selection payload the same batch already paid for.
    pub(crate) export_payload_statements: usize,
    pub(crate) export_payload_block_rows: usize,
    pub(crate) export_payload_tag_rows: usize,
    pub(crate) export_payload_property_rows: usize,
}

/// The four census counters, chosen by channel. One accessor per counter, so a
/// new channel cannot silently reuse another's field.
#[cfg(test)]
fn payload_statements(census: &mut ResultReadCensus, channel: PayloadChannel) -> &mut usize {
    match channel {
        PayloadChannel::Selection => &mut census.payload_statements,
        PayloadChannel::ExportOutput => &mut census.export_payload_statements,
    }
}

#[cfg(test)]
fn payload_block_rows(census: &mut ResultReadCensus, channel: PayloadChannel) -> &mut usize {
    match channel {
        PayloadChannel::Selection => &mut census.payload_block_rows,
        PayloadChannel::ExportOutput => &mut census.export_payload_block_rows,
    }
}

#[cfg(test)]
fn payload_tag_rows(census: &mut ResultReadCensus, channel: PayloadChannel) -> &mut usize {
    match channel {
        PayloadChannel::Selection => &mut census.payload_tag_rows,
        PayloadChannel::ExportOutput => &mut census.export_payload_tag_rows,
    }
}

#[cfg(test)]
fn payload_property_rows(census: &mut ResultReadCensus, channel: PayloadChannel) -> &mut usize {
    match channel {
        PayloadChannel::Selection => &mut census.payload_property_rows,
        PayloadChannel::ExportOutput => &mut census.export_payload_property_rows,
    }
}

// Thread-local rather than the process-global atomics beside
// `direct_projection`'s counters: these gates run under the ordinary parallel
// test harness, and a process-global counter would make one gate's arithmetic
// depend on which other test happened to be running.
#[cfg(test)]
thread_local! {
    static CENSUS: std::cell::Cell<ResultReadCensus> =
        const { std::cell::Cell::new(ResultReadCensus {
            descriptor_rows: 0,
            payload_statements: 0,
            payload_block_rows: 0,
            payload_tag_rows: 0,
            payload_property_rows: 0,
            page_rows: 0,
            export_payload_statements: 0,
            export_payload_block_rows: 0,
            export_payload_tag_rows: 0,
            export_payload_property_rows: 0,
        }) };
}

#[cfg(test)]
fn note(update: impl FnOnce(&mut ResultReadCensus)) {
    CENSUS.with(|census| {
        let mut current = census.get();
        update(&mut current);
        census.set(current);
    });
}

#[cfg(test)]
pub(crate) fn reset_result_read_census() {
    CENSUS.with(|census| census.set(ResultReadCensus::default()));
}

#[cfg(test)]
pub(crate) fn result_read_census() -> ResultReadCensus {
    CENSUS.with(std::cell::Cell::get)
}

/// What the merged read's BUFFER actually costs (test-only), so I-13's claim
/// about a big pending page is measured rather than argued: the heap bytes the
/// decoded rows retain, page fields counted once per page.
#[cfg(test)]
thread_local! {
    static BUFFERED_BYTES: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

#[cfg(test)]
fn note_buffered_bytes(page: &DescriptorPage, row: &DecodedDescriptor, first_of_page: bool) {
    BUFFERED_BYTES.with(|counter| {
        let (rows, bytes) = counter.get();
        let mut added = std::mem::size_of::<BufferedDescriptor>() + row.result_id.capacity();
        if first_of_page {
            added +=
                std::mem::size_of::<DescriptorPage>() + page.name.capacity() + page.path.capacity();
        }
        counter.set((rows + 1, bytes + added));
    });
}

#[cfg(test)]
pub(crate) fn reset_buffered_descriptor_bytes() {
    BUFFERED_BYTES.with(|counter| counter.set((0, 0)));
}

/// `(buffered rows, retained bytes)` since the last reset.
#[cfg(test)]
pub(crate) fn buffered_descriptor_bytes() -> (usize, usize) {
    BUFFERED_BYTES.with(std::cell::Cell::get)
}

/// A barrier point at the top of each payload batch (test-only), so a gate can
/// cancel BETWEEN batches deterministically instead of racing a sleep against
/// the read. The same shape as `direct_projection`'s `BEFORE_APPLY_PENDING`
/// hook, which exists for the same reason.
#[cfg(test)]
thread_local! {
    static BEFORE_PAYLOAD_BATCH: std::cell::RefCell<Option<Box<dyn Fn(usize)>>> =
        const { std::cell::RefCell::new(None) };
    static BEFORE_EXPORT_PAYLOAD_BATCH: std::cell::RefCell<Option<Box<dyn Fn(usize)>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_before_payload_batch_hook(hook: Option<Box<dyn Fn(usize)>>) {
    BEFORE_PAYLOAD_BATCH.with(|slot| *slot.borrow_mut() = hook);
}

/// The same barrier for RET3's export OUTPUT payload batches. A separate slot
/// rather than a channel argument, so an export gate cannot accidentally cancel
/// a selection read it did not mean to touch.
#[cfg(test)]
pub(crate) fn set_before_export_payload_batch_hook(hook: Option<Box<dyn Fn(usize)>>) {
    BEFORE_EXPORT_PAYLOAD_BATCH.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(test)]
fn run_before_payload_batch_hook(channel: PayloadChannel, batch: usize) {
    let slot = match channel {
        PayloadChannel::Selection => &BEFORE_PAYLOAD_BATCH,
        PayloadChannel::ExportOutput => &BEFORE_EXPORT_PAYLOAD_BATCH,
    };
    // Taken out of the slot's borrow first: the hook may cancel, block on a
    // barrier, or otherwise run for a while, and holding a `RefCell` borrow
    // across that would make the hook unable to touch its own slot.
    let present = slot.with(|slot| slot.borrow().is_some());
    if present {
        slot.with(|slot| {
            let taken = slot.borrow_mut().take();
            if let Some(hook) = taken {
                hook(batch);
                *slot.borrow_mut() = Some(hook);
            }
        });
    }
}
