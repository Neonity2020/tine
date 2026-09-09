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
//! * Main-result ORDERING and semantic sampling precede admission in SQL.
//!   Statistics fold that complete sample in the same snapshot; only admitted
//!   descriptors enter payload hydration. Located export and probe consumers
//!   retain their existing construction-order entry point and bounds.
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
use std::sync::Arc;

use tine_storage::sqlite::{
    MaterializationError, PhysicalProjectionQuerySnapshot, PhysicalQueryValue,
};

use crate::direct_projection::page_kind_from_sql;
use crate::model::{
    block_dto_estimated_bytes, doc_runtime_id_for_order, shallow_block_facets_dto, BlockDto,
    PageKind, RefGroup, ShallowBlockFacets,
};
use crate::query::rank::PageRecencyPrograms;
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

/// The backend construction order, also the final tie within equal display
/// page keys. Direct uses `query_page_order`; Managed uses binary `pages.path`.
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
/// which is what this pair names.
///
/// **It is an operation-scoped coordinate, not a handle.** Both ids are
/// physical rows of the caller's one snapshot. It is `Copy`, never crosses IPC
/// or a memo boundary, and is meaningless once the caller releases that
/// snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ResultLocator {
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
    StatisticsResourceLimit,
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
            ResultReadError::StatisticsResourceLimit => {
                f.write_str(crate::query::QueryUnavailableReason::StatisticsResourceLimit.message())
            }
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
            ResultReadError::StatisticsResourceLimit => {
                Self::Unavailable(Reason::StatisticsResourceLimit)
            }
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
    plain_groups(read_results_carried::<PlainResults>(
        snapshot, inputs, None,
    )?)
}

pub(crate) fn read_ordered_results(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    inputs: &ResultReadInputs<'_>,
    view: &crate::query::ir::ViewSettings,
    recency: &PageRecencyPrograms,
) -> Result<PreViewGroups, ResultReadError> {
    plain_groups(read_results_carried::<PlainResults>(
        snapshot,
        inputs,
        Some((view, recency)),
    )?)
}

fn plain_groups(carried: CarriedGroups<PlainResults>) -> Result<PreViewGroups, ResultReadError> {
    Ok(PreViewGroups {
        // A field move per GROUP, never a conversion per block: the ordinary
        // carrier's block vector IS `Vec<BlockDto>` already.
        groups: carried.groups.into_iter().map(RefGroup::from).collect(),
        recency_by_page: carried.recency_by_page,
        total: carried.total,
        exceeded: carried.exceeded,
        ordered: carried.ordered,
        matched_total: carried.matched_total,
        statistics: carried.statistics,
    })
}

/// The same single-snapshot construction, with each admitted entry's physical
/// locator retained for export subtree hydration (RET3).
///
/// Statement for statement and admission rule for admission rule this is
/// [`read_results`]: one collector, one payload decoder and one corruption
/// vocabulary. The locator is attached while the physical page and block ids
/// are still in hand and is valid only for this operation's snapshot.
pub(crate) fn read_located_results(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    inputs: &ResultReadInputs<'_>,
) -> Result<LocatedPreViewGroups, ResultReadError> {
    let carried = read_results_carried::<LocatedResults>(snapshot, inputs, None)?;
    Ok(LocatedPreViewGroups {
        groups: carried.groups,
        recency_by_page: carried.recency_by_page,
        total: carried.total,
        exceeded: carried.exceeded,
    })
}

/// The one result construction, over whichever carrier the caller wants its
/// admitted rows kept in.
fn read_results_carried<C: ResultCarrier>(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    inputs: &ResultReadInputs<'_>,
    ordered: Option<(&crate::query::ir::ViewSettings, &PageRecencyPrograms)>,
) -> Result<CarriedGroups<C>, ResultReadError> {
    install_regexes(snapshot, inputs.statement)?;
    let mut pages = PageGroups::<C>::default();
    pages.adjacent = ordered.is_some();
    pages.coalesce_names = ordered.is_some_and(|(view, _)| !view.sort.is_empty());
    let mut budget = ConstructionBudget::new(inputs.max_rows, inputs.max_bytes);
    let mut statistics = match ordered {
        Some((view, _)) => super::statistics::StatisticsFold::new(view, inputs.max_bytes)?,
        None => None,
    };
    let mut matched_total = ordered.map(|_| 0);
    let ordered_inputs = ResultReadInputs {
        profile: ConstructionProfile::default(),
        ..*inputs
    };
    let inputs = if ordered.is_some() {
        &ordered_inputs
    } else {
        inputs
    };
    let admitted = read_descriptors(
        snapshot,
        inputs,
        &mut pages,
        &mut budget,
        ordered,
        &mut statistics,
        &mut matched_total,
    )?;
    read_payload(snapshot, &mut pages, &admitted)?;
    if snapshot.cancellation().is_cancelled() {
        return Err(ResultReadError::Cancelled);
    }
    let mut answer = pages.finish(inputs, budget);
    answer.ordered = ordered.is_some();
    answer.matched_total = matched_total;
    answer.statistics = statistics.map(super::statistics::StatisticsFold::finish);
    Ok(answer)
}

/// [`PreViewGroups`] before the carrier is known — what
/// [`read_results_carried`] answers.
struct CarriedGroups<C: ResultCarrier> {
    ordered: bool,
    matched_total: Option<usize>,
    statistics: Option<crate::query::ir::QueryStatistics>,
    groups: Vec<ResultViewGroup<C::Block>>,
    recency_by_page: HashMap<String, i64>,
    total: usize,
    exceeded: bool,
}

/// One `@page` answer, in the SAME shape the retired page walk returned.
///
/// `total` is the number of rows admitted before sampling; `matched_total` is
/// the complete count computed by SQLite before its result limit.
#[derive(Debug, Default)]
pub(crate) struct PageAnswer {
    pub(crate) statistics: Option<crate::query::ir::QueryStatistics>,
    pub(crate) pages: Vec<crate::query::ir::PageRow>,
    pub(crate) total: usize,
    pub(crate) matched_total: usize,
    pub(crate) exceeded: bool,
}

pub(crate) struct PageReadInputs<'a> {
    pub(crate) statement: &'a SqlQuery,
    pub(crate) order: BackendOrder,
    pub(crate) view: &'a crate::query::ir::ViewSettings,
    pub(crate) max_rows: usize,
    pub(crate) max_bytes: usize,
    pub(crate) recency: &'a PageRecencyPrograms,
}

/// Construct one `@page` query's ordered public rows from the projection alone.
///
/// No `PageDto`, no `Document`, no parsed cache: the projection's page-result
/// metadata and authored property rows are the complete `@page` payload. The
/// caller owns capacity, the snapshot and its lifecycle, exactly as it does for
/// [`read_results`].
pub(crate) fn read_page_results(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    inputs: &PageReadInputs<'_>,
) -> Result<PageAnswer, ResultReadError> {
    install_regexes(snapshot, inputs.statement)?;
    if snapshot.cancellation().is_cancelled() {
        return Err(ResultReadError::Cancelled);
    }
    let mut statistics = super::statistics::StatisticsFold::new(inputs.view, inputs.max_bytes)?;
    let statement = page_statement(
        inputs.statement,
        inputs.order,
        inputs.view,
        if statistics.is_some() {
            usize::MAX
        } else {
            inputs.max_rows
        },
        inputs.recency,
    )
    .map_err(ResultReadError::Sql)?;
    let cancellation = snapshot.cancellation();
    snapshot
        .set_query_rank_function(statement.ranks.function(cancellation.clone()))
        .map_err(|error| sql_or_cancelled(snapshot, error))?;
    let mut descriptors = Vec::new();
    let mut budget = ConstructionBudget::new(inputs.max_rows, inputs.max_bytes);
    let mut seen = HashSet::new();
    let mut matched_total = None;
    let mut damage: Option<String> = None;
    let mut failure = None;
    let mut ordinal = 0usize;
    let visit =
        snapshot.visit_projection_query(&statement.query.sql, &statement.query.params, |row| {
            if cancellation.is_cancelled() {
                failure = Some(ResultReadError::Cancelled);
                return Ok(std::ops::ControlFlow::Break(()));
            }
            #[cfg(test)]
            note(|census| census.page_rows += 1);
            let descriptor = if statistics.is_some() {
                row.get(..9).unwrap_or(row)
            } else {
                row
            };
            let decoded = match decode_page_row(descriptor, inputs.order) {
                Ok(decoded) => decoded,
                Err(what) => {
                    damage = Some(what);
                    return Ok(std::ops::ControlFlow::Break(()));
                }
            };
            let unique = if statistics.is_some() {
                matches!(count(row, 9, "physical page multiplicity"), Ok(1))
            } else {
                seen.insert(decoded.page_id)
            };
            if !unique {
                damage = Some("one physical page appears twice in a page result".to_string());
                return Ok(std::ops::ControlFlow::Break(()));
            }
            match matched_total {
                None => matched_total = Some(decoded.matched_total),
                Some(total) if total == decoded.matched_total => {}
                Some(_) => {
                    damage = Some("page rows disagree on their complete match count".to_string());
                    return Ok(std::ops::ControlFlow::Break(()));
                }
            }
            if !inputs
                .view
                .sample
                .is_some_and(|sample| ordinal >= sample as usize)
            {
                if let Some(fold) = &mut statistics {
                    if let Err(error) = fold_statistics_row(fold, row, 10) {
                        failure = Some(error);
                        return Ok(std::ops::ControlFlow::Break(()));
                    }
                }
            }
            ordinal += 1;
            if !budget.closed() && budget.admit_page_estimated(decoded.estimated_bytes) {
                descriptors.push(decoded);
            } else if statistics.is_none() {
                return Ok(std::ops::ControlFlow::Break(()));
            }
            if statistics.is_some()
                && budget.closed()
                && inputs
                    .view
                    .sample
                    .is_some_and(|sample| ordinal >= sample as usize)
            {
                return Ok(std::ops::ControlFlow::Break(()));
            }
            Ok(std::ops::ControlFlow::Continue(()))
        });
    if let Err(error) = visit {
        return Err(sql_or_cancelled(snapshot, error));
    }
    if let Some(what) = damage {
        return Err(ResultReadError::Corrupt(what));
    }
    if let Some(error) = failure {
        return Err(error);
    }
    let matched_total = matched_total.unwrap_or(0);
    let mut pages = hydrate_page_rows(snapshot, &descriptors)?;
    let total = pages.len();
    if let Some(sample) = inputs.view.sample {
        pages.truncate(sample as usize);
    }
    if snapshot.cancellation().is_cancelled() {
        return Err(ResultReadError::Cancelled);
    }
    Ok(PageAnswer {
        statistics: statistics.map(super::statistics::StatisticsFold::finish),
        pages,
        total,
        matched_total,
        exceeded: budget.exceeded || matched_total > total,
    })
}

/// One decoded `@page` descriptor, including the physical and census fields
/// validated before its public payload is hydrated.
pub(crate) struct PageResultDescriptor {
    pub(crate) page_id: [u8; 16],
    pub(crate) name: String,
    pub(crate) kind: PageKind,
    pub(crate) journal_day: Option<i64>,
    pub(crate) path: String,
    pub(crate) estimated_bytes: usize,
    pub(crate) property_count: usize,
    pub(crate) matched_total: usize,
}

/// Column offsets of the page row, in the order [`page_statement`] selects them.
mod page_column {
    pub(super) const PAGE_ID: usize = 0;
    pub(super) const NAME: usize = 1;
    pub(super) const TEXT_KIND: usize = 2;
    pub(super) const JOURNAL_DAY: usize = 3;
    pub(super) const PATH: usize = 4;
    pub(super) const POSITION: usize = 5;
    pub(super) const ESTIMATED_BYTES: usize = 6;
    pub(super) const PROPERTY_COUNT: usize = 7;
    pub(super) const MATCHED_TOTAL: usize = 8;
    pub(super) const COLUMNS: usize = 9;
}

/// One page row: validate its identity, its kind and its order key. A row that
/// does not decode is damage and fails the read; it is never a page silently
/// missing from the answer (D-3).
fn decode_page_row(
    row: &[PhysicalQueryValue],
    order: BackendOrder,
) -> Result<PageResultDescriptor, String> {
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
    Ok(PageResultDescriptor {
        page_id,
        name,
        kind,
        journal_day,
        path,
        estimated_bytes: count(
            row,
            column::ESTIMATED_BYTES,
            "query_page_results.estimated_bytes",
        )?,
        property_count: count(
            row,
            column::PROPERTY_COUNT,
            "query_page_results.property_count",
        )?,
        matched_total: count(row, column::MATCHED_TOTAL, "page matched count")?,
    })
}

/// Hydrate only admitted page properties, in bounded owner batches, and
/// validate the producer's stored count and estimate before exposing a row.
pub(crate) fn hydrate_page_rows(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    descriptors: &[PageResultDescriptor],
) -> Result<Vec<crate::query::ir::PageRow>, ResultReadError> {
    let mut pages = Vec::with_capacity(descriptors.len());
    for (batch_index, batch) in descriptors.chunks(PAYLOAD_BATCH).enumerate() {
        #[cfg(test)]
        run_before_page_payload_batch_hook(batch_index);
        #[cfg(not(test))]
        let _ = batch_index;
        if snapshot.cancellation().is_cancelled() {
            return Err(ResultReadError::Cancelled);
        }
        let ids = batch
            .iter()
            .map(|row| PhysicalQueryValue::Blob(row.page_id.to_vec()))
            .collect::<Vec<_>>();
        let sql = format!(
            "SELECT owner_id, page_id, name, value, ordinal FROM properties \
             WHERE owner_type = 0 AND owner_id IN ({}) \
             ORDER BY owner_id, ordinal, name",
            placeholders(ids.len())
        );
        #[cfg(test)]
        note(|census| census.page_payload_statements += 1);
        let rows = snapshot
            .run_projection_query(&sql, &ids)
            .map_err(|error| sql_or_cancelled(snapshot, error))?;
        let admitted = batch.iter().map(|row| row.page_id).collect::<HashSet<_>>();
        let mut properties: HashMap<[u8; 16], Vec<(usize, String, String)>> = HashMap::new();
        for row in &rows {
            #[cfg(test)]
            note(|census| census.page_payload_property_rows += 1);
            let owner = blob16(row, 0, "properties.owner_id").map_err(ResultReadError::Corrupt)?;
            if !admitted.contains(&owner) {
                return Err(ResultReadError::Corrupt(
                    "a page property belongs to no admitted page".into(),
                ));
            }
            let page_id = blob16(row, 1, "properties.page_id").map_err(ResultReadError::Corrupt)?;
            if page_id != owner {
                return Err(ResultReadError::Corrupt(
                    "an admitted page property names a different page".into(),
                ));
            }
            let ordinal = count(row, 4, "properties.ordinal").map_err(ResultReadError::Corrupt)?;
            properties.entry(owner).or_default().push((
                ordinal,
                text(row, 2, "properties.name").map_err(ResultReadError::Corrupt)?,
                text(row, 3, "properties.value").map_err(ResultReadError::Corrupt)?,
            ));
        }
        for descriptor in batch {
            let rows = properties.remove(&descriptor.page_id).unwrap_or_default();
            if rows.len() != descriptor.property_count {
                return Err(ResultReadError::Corrupt(
                    "stored page property_count disagrees with the property rows".into(),
                ));
            }
            if rows
                .iter()
                .enumerate()
                .any(|(expected, row)| row.0 != expected)
            {
                return Err(ResultReadError::Corrupt(
                    "page property ordinals are not contiguous".into(),
                ));
            }
            let properties = rows
                .into_iter()
                .map(|(_, name, value)| (name, value))
                .collect::<Vec<_>>();
            let estimated = tine_storage::sqlite::query_page_result_estimated_bytes(
                &descriptor.name,
                &descriptor.path,
                properties
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            );
            if estimated != descriptor.estimated_bytes {
                return Err(ResultReadError::Corrupt(
                    "the emitted page result does not match its stored estimate".into(),
                ));
            }
            pages.push(crate::query::ir::PageRow {
                path: descriptor.path.clone(),
                name: descriptor.name.clone(),
                kind: descriptor.kind,
                journal_day: descriptor.journal_day,
                properties,
            });
        }
        if !properties.is_empty() {
            return Err(ResultReadError::Corrupt(
                "a page property belongs to no admitted page".into(),
            ));
        }
    }
    if snapshot.cancellation().is_cancelled() {
        return Err(ResultReadError::Cancelled);
    }
    Ok(pages)
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
    adjacent: bool,
    coalesce_names: bool,
    order: Vec<PageGroup<C>>,
    by_page: HashMap<[u8; 16], usize>,
}

// Derived `Default` would demand `C: Default`, which no carrier is: the
// carrier is a type-level switch and never a value.
impl<C: ResultCarrier> Default for PageGroups<C> {
    fn default() -> Self {
        Self {
            adjacent: false,
            coalesce_names: false,
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
        if self.coalesce_names
            && self
                .order
                .last()
                .is_some_and(|last| last.group.page == name && last.group.kind == kind)
        {
            return self.order.len() - 1;
        }
        if let Some(at) = self
            .by_page
            .get(&page_id)
            .filter(|at| !self.adjacent || **at + 1 == self.order.len())
        {
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
    fn finish(self, inputs: &ResultReadInputs<'_>, budget: ConstructionBudget) -> CarriedGroups<C> {
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
            ordered: false,
            matched_total: None,
            statistics: None,
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

/// Walk this snapshot's ordered descriptors once, charging
/// [`ConstructionBudget`] exactly as `collect_sql_matched_blocks` does, and
/// keep only what was admitted.
///
/// The budget rules are transcribed in the walk's order and not re-derived:
/// the unsorted `(sample N)` cap STOPS counting (which is what makes its
/// `total` the truncated count), a closed budget `deny_match`es, and an
/// over-budget row is counted without being emitted. Everything a denied row
/// would have carried is dropped here, so descriptor memory is bounded by
/// `max_rows` rather than by the size of the match set.
///
/// A closed budget does NOT stop a stream: every remaining row is still visited
/// and still `deny_match`ed, because `total` is the number of matches SEEN. The
/// only early exit is the `(sample N)` cap's `ControlFlow::Break`, so the
/// truncation point is exactly the walk's.
fn read_descriptors<C: ResultCarrier>(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    inputs: &ResultReadInputs<'_>,
    pages: &mut PageGroups<C>,
    budget: &mut ConstructionBudget,
    ordered: Option<(&crate::query::ir::ViewSettings, &PageRecencyPrograms)>,
    statistics: &mut Option<super::statistics::StatisticsFold>,
    matched_total: &mut Option<usize>,
) -> Result<Vec<Descriptor>, ResultReadError> {
    if snapshot.cancellation().is_cancelled() {
        return Err(ResultReadError::Cancelled);
    }
    let statement = super::sql::descriptor_view_statement(inputs.statement, inputs.order, ordered)
        .map_err(ResultReadError::Sql)?;
    snapshot
        .set_query_rank_function(statement.ranks.function(snapshot.cancellation()))
        .map_err(|error| sql_or_cancelled(snapshot, error))?;
    let statement = statement.query;
    let mut admitted = Vec::new();
    let mut damage: Option<String> = None;
    let mut failure = None;
    let mut ordinal = 0usize;
    let cancellation = snapshot.cancellation();
    let visit = snapshot.visit_projection_query(&statement.sql, &statement.params, |row| {
        if cancellation.is_cancelled() {
            failure = Some(ResultReadError::Cancelled);
            return Ok(std::ops::ControlFlow::Break(()));
        }
        #[cfg(test)]
        note(|census| census.descriptor_rows += 1);
        if let Some((view, _)) = ordered {
            match count(row, 14, "complete block count") {
                Ok(total) => *matched_total = Some(total),
                Err(what) => {
                    damage = Some(what);
                    return Ok(std::ops::ControlFlow::Break(()));
                }
            }
            if view.sample.is_some_and(|sample| ordinal >= sample as usize) {
                return Ok(std::ops::ControlFlow::Break(()));
            }
        }
        ordinal += 1;
        let descriptor = if ordered.is_some() {
            row.get(..14).unwrap_or(row)
        } else {
            row
        };
        let (page, decoded) = match decode_descriptor(descriptor, inputs) {
            Ok(decoded) => decoded,
            Err(what) => {
                damage = Some(what);
                return Ok(std::ops::ControlFlow::Break(()));
            }
        };
        if let Some(fold) = statistics {
            if let Err(error) = fold_statistics_row(fold, row, 15) {
                failure = Some(error);
                return Ok(std::ops::ControlFlow::Break(()));
            }
        }
        Ok(admit_decoded(
            &page,
            decoded,
            inputs,
            pages,
            budget,
            &mut admitted,
        ))
    });
    if let Err(error) = visit {
        return Err(sql_or_cancelled(snapshot, error));
    }
    if let Some(what) = damage {
        return Err(ResultReadError::Corrupt(what));
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(admitted)
}

fn fold_statistics_row(
    fold: &mut super::statistics::StatisticsFold,
    row: &[PhysicalQueryValue],
    offset: usize,
) -> Result<(), ResultReadError> {
    let width = fold.view().aggregates.len();
    let values = (0..width)
        .map(|at| opt_text(row, offset + at, "statistics raw value"))
        .collect::<Result<Vec<_>, _>>()
        .map_err(ResultReadError::Corrupt)?;
    let encoded =
        text(row, offset + width, "statistics memberships").map_err(ResultReadError::Corrupt)?;
    let mut keys: Vec<Option<String>> = serde_json::from_str(&encoded)
        .map_err(|_| ResultReadError::Corrupt("invalid statistics memberships".into()))?;
    if keys.is_empty() {
        keys.push(None);
    }
    #[cfg(test)]
    note(|census| {
        census.statistics_rows += 1;
        census.statistics_values += width;
    });
    fold.add(&values, keys)
}

/// What one descriptor row says about its page, dropped immediately after the
/// row is offered to the budget.
struct DescriptorPage {
    page_id: [u8; 16],
    name: String,
    kind: PageKind,
    journal_day: Option<i64>,
    path: String,
}

/// One descriptor row, decoded and identity-resolved, without the raw
/// `PhysicalQueryValue` vector and without the two columns already consumed:
/// `blocks.order_key` and the stored id, which [`resolve_identity`] folded into
/// `result_id` and `estimated_bytes`.
struct DecodedDescriptor {
    block_id: [u8; 16],
    result_id: String,
    estimated_bytes: usize,
    tag_count: usize,
    property_count: usize,
}

/// One descriptor row: validate it and resolve its public identity. Nothing
/// here touches the budget or the groups.
fn decode_descriptor(
    row: &[PhysicalQueryValue],
    inputs: &ResultReadInputs<'_>,
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
        },
        DecodedDescriptor {
            block_id,
            result_id,
            estimated_bytes,
            tag_count,
            property_count,
        },
    ))
}

/// Offer one decoded descriptor to the budget, in statement order.
///
/// The four rules, in `collect_sql_matched_blocks`' own order.
fn admit_decoded<C: ResultCarrier>(
    page: &DescriptorPage,
    row: DecodedDescriptor,
    inputs: &ResultReadInputs<'_>,
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
            // Attached here, while the physical page and block ids are both in
            // hand. Nothing downstream recovers identity by comparing exposed
            // ids or rendered text.
            let locator = ResultLocator {
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
    pub(crate) statistics_rows: usize,
    pub(crate) statistics_values: usize,
    pub(crate) descriptor_rows: usize,
    pub(crate) payload_statements: usize,
    pub(crate) payload_block_rows: usize,
    pub(crate) payload_tag_rows: usize,
    pub(crate) payload_property_rows: usize,
    /// `@page` descriptors the page statement returned. Page-property payload
    /// reads have their own counters so a gate can prove rejected owners were
    /// never hydrated.
    pub(crate) page_rows: usize,
    pub(crate) page_payload_statements: usize,
    pub(crate) page_payload_property_rows: usize,
    pub(crate) page_recency_lookups: usize,
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
            statistics_rows: 0,
            statistics_values: 0,
            descriptor_rows: 0,
            payload_statements: 0,
            payload_block_rows: 0,
            payload_tag_rows: 0,
            payload_property_rows: 0,
            page_rows: 0,
            page_payload_statements: 0,
            page_payload_property_rows: 0,
            page_recency_lookups: 0,
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
pub(crate) fn note_page_recency_lookup() {
    note(|census| census.page_recency_lookups += 1);
}

#[cfg(test)]
pub(crate) fn reset_result_read_census() {
    CENSUS.with(|census| census.set(ResultReadCensus::default()));
}

#[cfg(test)]
pub(crate) fn result_read_census() -> ResultReadCensus {
    CENSUS.with(std::cell::Cell::get)
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
    static BEFORE_PAGE_PAYLOAD_BATCH: std::cell::RefCell<Option<Box<dyn Fn(usize)>>> =
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
pub(crate) fn set_before_page_payload_batch_hook(hook: Option<Box<dyn Fn(usize)>>) {
    BEFORE_PAGE_PAYLOAD_BATCH.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(test)]
fn run_before_page_payload_batch_hook(batch: usize) {
    let present = BEFORE_PAGE_PAYLOAD_BATCH.with(|slot| slot.borrow().is_some());
    if present {
        BEFORE_PAGE_PAYLOAD_BATCH.with(|slot| {
            let taken = slot.borrow_mut().take();
            if let Some(hook) = taken {
                hook(batch);
                *slot.borrow_mut() = Some(hook);
            }
        });
    }
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
