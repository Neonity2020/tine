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
use std::sync::Arc;

use tine_storage::sqlite::{
    MaterializationError, PhysicalProjectionQuerySnapshot, PhysicalQueryValue,
};

use crate::direct_projection::page_kind_from_sql;
use crate::model::{
    block_dto_estimated_bytes, doc_runtime_id_for_order, shallow_block_facets_dto, PageKind,
    RefGroup, ShallowBlockFacets,
};
use crate::query::sql::{descriptor_statement, SqlQuery};
use crate::query::{ConstructionBudget, ConstructionProfile, PreViewGroups};

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

/// Everything one result read needs besides the snapshot itself.
pub(crate) struct ResultReadInputs<'a> {
    /// The compiler's statement, unchanged (`lower_query`).
    pub(crate) statement: &'a SqlQuery,
    pub(crate) order: BackendOrder,
    pub(crate) identity: &'a ResultIdentity,
    pub(crate) max_rows: usize,
    pub(crate) max_bytes: usize,
    pub(crate) profile: ConstructionProfile,
    /// The recency axis for `(sort-by modified …)`, by page: the EXISTING
    /// producer ([`crate::query::page_recency_secs_for`]) presented as
    /// `(journal day, page path)`. It is a callback because it is a filesystem
    /// `stat` that must not run for a page the answer did not admit, and
    /// because only the caller knows the graph root the stored relative path
    /// hangs off.
    pub(crate) recency: &'a dyn Fn(Option<i64>, &str) -> i64,
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

impl std::fmt::Display for ResultReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResultReadError::Sql(error) => write!(f, "projection read failed: {error}"),
            ResultReadError::Corrupt(what) => write!(f, "projection is inconsistent: {what}"),
            ResultReadError::Cancelled => write!(f, "query cancelled"),
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
    install_regexes(snapshot, inputs.statement)?;
    let mut pages = PageGroups::default();
    let mut budget = ConstructionBudget::new(inputs.max_rows, inputs.max_bytes);
    let admitted = read_descriptors(snapshot, inputs, &mut pages, &mut budget)?;
    read_payload(snapshot, &mut pages, &admitted)?;
    Ok(pages.finish(inputs, budget))
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
fn sql_or_cancelled(
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
/// recency axis needs, kept out of `RefGroup` because they are inputs and not
/// part of the answer.
struct PageGroup {
    page_id: [u8; 16],
    group: RefGroup,
    journal_day: Option<i64>,
    path: String,
}

/// The groups in BASE order, one per PHYSICAL page.
///
/// Keyed by `page_id`, so two physical pages that happen to share a display
/// name stay two groups here exactly as they are two pages in the walk;
/// `base_order_groups`/`finish_query_groups` merges them for display later, the
/// same way and in the same place as today.
#[derive(Default)]
struct PageGroups {
    order: Vec<PageGroup>,
    by_page: HashMap<[u8; 16], usize>,
}

impl PageGroups {
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
            group: RefGroup {
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
    fn finish(self, inputs: &ResultReadInputs<'_>, budget: ConstructionBudget) -> PreViewGroups {
        let mut groups = Vec::with_capacity(self.order.len());
        let mut recency_by_page = HashMap::new();
        for page in self.order {
            if page.group.blocks.is_empty() {
                continue;
            }
            if inputs.profile.want_recency {
                recency_by_page.insert(
                    page.group.page.clone(),
                    (inputs.recency)(page.journal_day, &page.path),
                );
            }
            groups.push(page.group);
        }
        PreViewGroups {
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

/// Walk the ordered descriptors ONCE, charging [`ConstructionBudget`] exactly
/// as `collect_sql_matched_blocks` does, and keep only what was admitted.
///
/// The budget rules are transcribed in the walk's order and not re-derived:
/// the unsorted `(sample N)` cap STOPS counting (which is what makes its
/// `total` the truncated count), a closed budget `deny_match`es, and an
/// over-budget row is counted without being emitted. Everything a denied row
/// would have carried is dropped here, so peak memory is bounded by
/// `max_rows` rather than by the size of the match set.
fn read_descriptors(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    inputs: &ResultReadInputs<'_>,
    pages: &mut PageGroups,
    budget: &mut ConstructionBudget,
) -> Result<Vec<Descriptor>, ResultReadError> {
    if snapshot.cancellation().is_cancelled() {
        return Err(ResultReadError::Cancelled);
    }
    let statement =
        descriptor_statement(inputs.statement, inputs.order).map_err(ResultReadError::Sql)?;
    let mut admitted: Vec<Descriptor> = Vec::new();
    let mut damage: Option<String> = None;
    let visit = snapshot.visit_projection_query(&statement.sql, &statement.params, |row| {
        #[cfg(test)]
        note(|census| census.descriptor_rows += 1);
        match admit_descriptor(row, inputs, pages, budget, &mut admitted) {
            Ok(flow) => Ok(flow),
            Err(what) => {
                damage = Some(what);
                Ok(std::ops::ControlFlow::Break(()))
            }
        }
    });
    if let Err(error) = visit {
        return Err(sql_or_cancelled(snapshot, error));
    }
    match damage {
        Some(what) => Err(ResultReadError::Corrupt(what)),
        None => Ok(admitted),
    }
}

/// One descriptor row: validate it, resolve its public identity, and offer it
/// to the budget.
fn admit_descriptor(
    row: &[PhysicalQueryValue],
    inputs: &ResultReadInputs<'_>,
    pages: &mut PageGroups,
    budget: &mut ConstructionBudget,
    admitted: &mut Vec<Descriptor>,
) -> Result<std::ops::ControlFlow<()>, String> {
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
    // Direct Files' cross-page order IS this column; a NULL would silently
    // sort a page to one end of the answer, which changes which rows survive a
    // truncated budget.
    if inputs.order == BackendOrder::Direct
        && opt_integer(row, column::POSITION, "query_page_order.position")?.is_none()
    {
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

    // The budget, in the walk's order (`collect_sql_matched_blocks`).
    if inputs
        .profile
        .sample_admission_cap
        .is_some_and(|cap| budget.rows >= cap)
    {
        // Stop COUNTING here: an unsorted `(sample N)` reports the truncated
        // count as its total, and the walk stops at the first matched block it
        // sees after the cap on every remaining page.
        return Ok(std::ops::ControlFlow::Break(()));
    }
    if budget.closed() {
        budget.deny_match();
        return Ok(std::ops::ControlFlow::Continue(()));
    }
    let page = pages.slot(page_id, &name, kind, journal_day, &path);
    if !budget.admit_estimated(&name, estimated_bytes) {
        return Ok(std::ops::ControlFlow::Continue(()));
    }
    admitted.push(Descriptor {
        block_id,
        page,
        result_id,
        estimated_bytes,
        tag_count,
        property_count,
    });
    Ok(std::ops::ControlFlow::Continue(()))
}

/// The public id of one admitted row and the construction estimate that goes
/// with it.
///
/// The stored estimate describes the STORED public identity. When a fresh
/// Direct session resolves a structural id instead, only that one term moves:
/// subtract the stored id's bytes, add the canonical UUID's 36. The arithmetic
/// is checked because a stored estimate smaller than its own identity term is a
/// contradiction, and a saturating subtraction would hide it.
fn resolve_identity(
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

/// Read the payload of the ADMITTED ids and construct their DTOs.
///
/// Three bound statements per batch of [`PAYLOAD_BATCH`] ids, all through the
/// SAME snapshot. Never one statement per block (that is the N+1 this packet
/// exists to remove) and never a tags×properties join (that is a cross product
/// whose row count is the product of two independent facets).
fn read_payload(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    pages: &mut PageGroups,
    admitted: &[Descriptor],
) -> Result<(), ResultReadError> {
    for (index, batch) in admitted.chunks(PAYLOAD_BATCH).enumerate() {
        #[cfg(test)]
        run_before_payload_batch_hook(index);
        #[cfg(not(test))]
        let _ = index;
        // Between batches, not inside one: a cancelled job stops at the next
        // statement boundary and its snapshot is released by the owner.
        if snapshot.cancellation().is_cancelled() {
            return Err(ResultReadError::Cancelled);
        }
        let ids = batch
            .iter()
            .map(|descriptor| PhysicalQueryValue::Blob(descriptor.block_id.to_vec()))
            .collect::<Vec<_>>();
        let facets = read_block_facets(snapshot, &ids)?;
        let tags = read_owner_strings(
            snapshot,
            &ids,
            OwnerList::Tags,
            "SELECT owner_id, tag FROM tags \
             WHERE owner_type = {owner} AND owner_id IN ({ids}) \
             ORDER BY owner_id, ordinal",
            |row| text(row, 1, "tags.tag"),
        )?;
        let properties = read_owner_strings(
            snapshot,
            &ids,
            OwnerList::Properties,
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
        emit_batch(pages, batch, facets, tags, properties).map_err(ResultReadError::Corrupt)?;
    }
    Ok(())
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
    note(|census| census.payload_statements += 1);
    let rows = snapshot
        .run_projection_query(&sql, ids)
        .map_err(|error| sql_or_cancelled(snapshot, error))?;
    let mut facets = HashMap::with_capacity(rows.len());
    for row in &rows {
        #[cfg(test)]
        note(|census| census.payload_block_rows += 1);
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
    shape: &str,
    decode: impl Fn(&[PhysicalQueryValue]) -> Result<T, String>,
) -> Result<HashMap<[u8; 16], Vec<T>>, ResultReadError> {
    let sql = shape
        .replace("{owner}", &OWNER_BLOCK.to_string())
        .replace("{ids}", &placeholders(ids.len()));
    #[cfg(test)]
    note(|census| census.payload_statements += 1);
    #[cfg(not(test))]
    let _ = list;
    let rows = snapshot
        .run_projection_query(&sql, ids)
        .map_err(|error| sql_or_cancelled(snapshot, error))?;
    let mut owners: HashMap<[u8; 16], Vec<T>> = HashMap::new();
    for row in &rows {
        #[cfg(test)]
        note(|census| match list {
            OwnerList::Tags => census.payload_tag_rows += 1,
            OwnerList::Properties => census.payload_property_rows += 1,
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

/// Validate one batch and emit its DTOs into their groups, in admission order.
///
/// Every check here is a "the projection contradicts itself" check, and every
/// one of them abandons the WHOLE result rather than the row: exact coverage
/// (each admitted id has one and only one payload row, and no row belongs to an
/// id nobody admitted), page ownership, the stored tag/property counts, and
/// finally the stored estimate against the estimate of the DTO that was
/// actually built. That last one is what proves the metadata and the payload
/// describe the same block.
fn emit_batch(
    pages: &mut PageGroups,
    batch: &[Descriptor],
    mut facets: HashMap<[u8; 16], BlockFacets>,
    mut tags: HashMap<[u8; 16], Vec<String>>,
    mut properties: HashMap<[u8; 16], Vec<(String, String)>>,
) -> Result<(), String> {
    for descriptor in batch {
        let Some(facet) = facets.remove(&descriptor.block_id) else {
            return Err("an admitted block has no payload row".to_string());
        };
        let page = &mut pages.order[descriptor.page];
        if facet.page_id != page.page_id {
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
            id: descriptor.result_id.clone(),
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
        page.group.blocks.push(dto);
    }
    if !facets.is_empty() {
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
fn placeholders(count: usize) -> String {
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

fn blob16(row: &[PhysicalQueryValue], at: usize, what: &str) -> Result<[u8; 16], String> {
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

fn text(row: &[PhysicalQueryValue], at: usize, what: &str) -> Result<String, String> {
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

fn integer(row: &[PhysicalQueryValue], at: usize, what: &str) -> Result<i64, String> {
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
fn count(row: &[PhysicalQueryValue], at: usize, what: &str) -> Result<usize, String> {
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

/// A barrier point at the top of each payload batch (test-only), so a gate can
/// cancel BETWEEN batches deterministically instead of racing a sleep against
/// the read. The same shape as `direct_projection`'s `BEFORE_APPLY_PENDING`
/// hook, which exists for the same reason.
#[cfg(test)]
thread_local! {
    static BEFORE_PAYLOAD_BATCH: std::cell::RefCell<Option<Box<dyn Fn(usize)>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_before_payload_batch_hook(hook: Option<Box<dyn Fn(usize)>>) {
    BEFORE_PAYLOAD_BATCH.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(test)]
fn run_before_payload_batch_hook(batch: usize) {
    // Cloned out of the slot's borrow first: the hook may cancel, block on a
    // barrier, or otherwise run for a while, and holding a `RefCell` borrow
    // across that would make the hook unable to touch its own slot.
    let hook = BEFORE_PAYLOAD_BATCH.with(|slot| slot.borrow().is_some());
    if hook {
        BEFORE_PAYLOAD_BATCH.with(|slot| {
            let taken = slot.borrow_mut().take();
            if let Some(hook) = taken {
                hook(batch);
                *slot.borrow_mut() = Some(hook);
            }
        });
    }
}
