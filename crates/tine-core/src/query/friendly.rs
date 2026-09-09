//! Friendly graph search directly over one projection snapshot.
//!
//! SQLite owns candidate selection and ordering. Rust retains only the
//! `limit + 1` descriptors for each section, hydrates admitted block ids with
//! the shared strict payload reader, and derives breadcrumbs from projected
//! ancestor text. No document, source file, parsed cache, or backend mode is
//! consulted here.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tine_storage::sqlite::{PhysicalProjectionQuerySnapshot, PhysicalQueryValue};

use crate::direct_projection::page_kind_from_sql;
use crate::model::{BlockDto, PageEntry, PageKind};
use crate::query::rank::QueryRankPrograms;
use crate::query::results::{
    blob16, count, integer, read_admitted_payload, resolve_identity, sql_or_cancelled, text,
    PayloadChannel, PayloadFacts, ResultIdentity, ResultReadError, PAYLOAD_BATCH,
};
use crate::query_plan::{
    admitted_block_evidence, admitted_page_evidence, rank_block_text, rank_page_text, QueryBranch,
    QueryExecution, QueryExplanation, QueryHasMore, QueryHit, QueryPlan, QueryTarget,
};

/// Everything the shared Friendly reader needs besides its caller-owned
/// snapshot. `explain` preserves the existing public route's cheap opt-out;
/// every other fact is immutable operation input captured beside the snapshot.
pub(crate) struct FriendlyReadInputs<'a> {
    pub(crate) plan: &'a QueryPlan,
    pub(crate) graph_root: &'a Path,
    pub(crate) identity: &'a ResultIdentity,
    pub(crate) explain: bool,
    pub(crate) lane: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

/// Diagnostic and branchless plans need no database. Supersession still wins.
pub(crate) fn friendly_without_read(
    plan: &QueryPlan,
    explain: bool,
    lane: &Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> Option<QueryExecution> {
    let cancelled = lane.as_ref().is_some_and(|lane| lane());
    if !cancelled && plan.diagnostics.is_empty() && !plan.branches.is_empty() {
        return None;
    }
    Some(QueryExecution {
        hits: Vec::new(),
        diagnostics: plan.diagnostics.clone(),
        explanation: if explain {
            plan.explanation()
        } else {
            QueryExplanation {
                branches: Vec::new(),
            }
        },
        has_more: QueryHasMore::default(),
        cancelled,
    })
}

fn check_lane(
    snapshot: &PhysicalProjectionQuerySnapshot,
    lane: &Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> Result<(), ResultReadError> {
    if lane.as_ref().is_some_and(|lane| lane()) {
        snapshot.cancellation().cancel();
    }
    cancelled(snapshot)
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FriendlyReadCensus {
    pub(crate) page_descriptors: usize,
    pub(crate) block_descriptors: usize,
    pub(crate) ancestor_statements: usize,
    pub(crate) ancestor_rows: usize,
}

#[cfg(test)]
thread_local! {
    static FRIENDLY_CENSUS: std::cell::Cell<FriendlyReadCensus> =
        const { std::cell::Cell::new(FriendlyReadCensus {
            page_descriptors: 0,
            block_descriptors: 0,
            ancestor_statements: 0,
            ancestor_rows: 0,
        }) };
    static BEFORE_FRIENDLY_RANK: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
    static BEFORE_ANCESTOR_BATCH: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn reset_friendly_read_census() {
    FRIENDLY_CENSUS.with(|census| census.set(FriendlyReadCensus::default()));
}

#[cfg(test)]
pub(crate) fn friendly_read_census() -> FriendlyReadCensus {
    FRIENDLY_CENSUS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn set_before_friendly_rank_hook(hook: Option<Box<dyn Fn()>>) {
    BEFORE_FRIENDLY_RANK.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(test)]
pub(crate) fn set_before_ancestor_batch_hook(hook: Option<Box<dyn Fn()>>) {
    BEFORE_ANCESTOR_BATCH.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(test)]
fn run_one_shot_hook(
    slot: &'static std::thread::LocalKey<std::cell::RefCell<Option<Box<dyn Fn()>>>>,
) {
    let hook = slot.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
fn note_friendly(update: impl FnOnce(&mut FriendlyReadCensus)) {
    FRIENDLY_CENSUS.with(|census| {
        let mut current = census.get();
        update(&mut current);
        census.set(current);
    });
}

enum BoundBranch {
    Pages {
        branch: QueryBranch,
        owner_rank: u64,
        global_rank: u64,
    },
    Blocks {
        branch: QueryBranch,
        rank: u64,
    },
}

/// Execute the compiled Friendly plan on one immutable main projection image.
pub(crate) fn read_friendly_results(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    inputs: &FriendlyReadInputs<'_>,
) -> Result<QueryExecution, ResultReadError> {
    check_lane(snapshot, &inputs.lane)?;
    let explanation = if inputs.explain {
        inputs.plan.explanation()
    } else {
        QueryExplanation {
            branches: Vec::new(),
        }
    };
    if !inputs.plan.diagnostics.is_empty() {
        return Ok(QueryExecution {
            hits: Vec::new(),
            diagnostics: inputs.plan.diagnostics.clone(),
            explanation,
            has_more: QueryHasMore::default(),
            cancelled: false,
        });
    }
    cancelled(snapshot)?;

    // One operation-owned program table for every branch. Each closure owns a
    // clone of the already compiled plan; regex compilation never occurs per
    // candidate or inside SQLite's callback.
    let plan = Arc::new(inputs.plan.clone());
    let mut programs = QueryRankPrograms::default();
    let mut branches = Vec::with_capacity(plan.branches.len());
    for branch in &plan.branches {
        match branch.target {
            QueryTarget::Pages => {
                let owner_plan = Arc::clone(&plan);
                let owner_branch = branch.clone();
                let owner_rank = programs.bind(move |candidate| {
                    #[cfg(test)]
                    run_one_shot_hook(&BEFORE_FRIENDLY_RANK);
                    Ok(rank_page_text(&owner_plan, &owner_branch, candidate)
                        .map(|rank| rank.owner_order_key().to_vec()))
                });
                let global_plan = Arc::clone(&plan);
                let global_branch = branch.clone();
                let global_rank = programs.bind_pair(move |physical_name, winning_text| {
                    #[cfg(test)]
                    run_one_shot_hook(&BEFORE_FRIENDLY_RANK);
                    Ok(rank_page_text(&global_plan, &global_branch, winning_text)
                        .map(|rank| rank.global_order_key(physical_name).to_vec()))
                });
                branches.push(BoundBranch::Pages {
                    branch: branch.clone(),
                    owner_rank,
                    global_rank,
                });
            }
            QueryTarget::Blocks => {
                let rank_plan = Arc::clone(&plan);
                let rank_branch = branch.clone();
                let rank = programs.bind(move |visible| {
                    #[cfg(test)]
                    run_one_shot_hook(&BEFORE_FRIENDLY_RANK);
                    Ok(rank_block_text(&rank_plan, &rank_branch, visible)
                        .map(|rank| rank.order_key().to_vec()))
                });
                branches.push(BoundBranch::Blocks {
                    branch: branch.clone(),
                    rank,
                });
            }
        }
    }
    let cancellation = snapshot.cancellation();
    let rank = programs.function(cancellation.clone());
    let lane = inputs.lane.clone();
    snapshot
        .set_query_rank_function(move |id, text| {
            if lane.as_ref().is_some_and(|lane| lane()) {
                cancellation.cancel();
            }
            let answer = rank(id, text);
            if lane.as_ref().is_some_and(|lane| lane()) {
                cancellation.cancel();
            }
            answer
        })
        .map_err(|error| sql_or_cancelled(snapshot, error))?;

    let mut hits = Vec::new();
    let mut has_more = QueryHasMore::default();
    // Public Friendly order is categorical even if a hand-built internal plan
    // happens to store its branches in another order.
    for target in [QueryTarget::Pages, QueryTarget::Blocks] {
        for bound in branches.iter().filter(|bound| match (target, bound) {
            (QueryTarget::Pages, BoundBranch::Pages { .. }) => true,
            (QueryTarget::Blocks, BoundBranch::Blocks { .. }) => true,
            _ => false,
        }) {
            check_lane(snapshot, &inputs.lane)?;
            match bound {
                BoundBranch::Pages {
                    branch,
                    owner_rank,
                    global_rank,
                } => {
                    let (mut section, more) = read_pages(
                        snapshot,
                        inputs.graph_root,
                        &plan,
                        branch,
                        *owner_rank,
                        *global_rank,
                        &inputs.lane,
                    )?;
                    hits.append(&mut section);
                    has_more.pages |= more;
                }
                BoundBranch::Blocks { branch, rank } => {
                    let (mut section, more) = read_blocks(
                        snapshot,
                        inputs.identity,
                        &plan,
                        branch,
                        *rank,
                        &inputs.lane,
                    )?;
                    hits.append(&mut section);
                    has_more.blocks |= more;
                }
            }
        }
    }
    check_lane(snapshot, &inputs.lane)?;
    Ok(QueryExecution {
        hits,
        diagnostics: inputs.plan.diagnostics.clone(),
        explanation,
        has_more,
        cancelled: false,
    })
}

fn cancelled(snapshot: &PhysicalProjectionQuerySnapshot) -> Result<(), ResultReadError> {
    if snapshot.cancellation().is_cancelled() {
        Err(ResultReadError::Cancelled)
    } else {
        Ok(())
    }
}

fn limit_clause(limit: usize, params: &mut Vec<PhysicalQueryValue>) -> String {
    let Some(one_past) = limit
        .checked_add(1)
        .and_then(|value| i64::try_from(value).ok())
    else {
        return String::new();
    };
    params.push(PhysicalQueryValue::Integer(one_past));
    format!(" LIMIT ?{}", params.len())
}

/// SQL expression consumed by `QueryRankPrograms::bind_pair`.
fn framed_pair_sql(left: &str, right: &str) -> String {
    format!("CAST(length(CAST({left} AS BLOB)) AS TEXT) || ':' || {left} || {right}")
}

struct PageDescriptor {
    page_id: Option<[u8; 16]>,
    name: String,
    kind: PageKind,
    journal_day: Option<i64>,
    path: String,
    matched_text: String,
    matched_alias: bool,
    rank_key: Vec<u8>,
}

fn read_pages(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    graph_root: &Path,
    plan: &QueryPlan,
    branch: &QueryBranch,
    owner_rank: u64,
    global_rank: u64,
    lane: &Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> Result<(Vec<QueryHit>, bool), ResultReadError> {
    if branch.limit == 0 {
        return Ok((Vec::new(), false));
    }
    let mut params = vec![
        PhysicalQueryValue::Integer(owner_rank as i64),
        PhysicalQueryValue::Integer(global_rank as i64),
    ];
    let limit = limit_clause(branch.limit, &mut params);
    let framed_physical = framed_pair_sql("w.name", "w.matched_text");
    let framed_virtual = framed_pair_sql("v.raw_name", "v.raw_name");
    let sql = format!(
        "WITH page_text_candidates(page_id, name, text_kind, journal_day, path, \
                  matched_text, source_kind, source_ordinal) AS (\
             SELECT p.page_id, p.name, p.text_kind, p.journal_day, p.path, \
                    p.name, 0, -1 FROM pages p \
             UNION ALL \
             SELECT p.page_id, p.name, p.text_kind, p.journal_day, p.path, \
                    a.normalized_alias, 1, MIN(a.ordinal) \
             FROM reference_alias_declarations a \
             JOIN pages p ON p.page_id = a.source_page_id \
             WHERE a.source_entity_type = 0 AND a.source_entity_id = a.source_page_id \
             GROUP BY p.page_id, a.normalized_alias\
         ), ranked AS MATERIALIZED (\
             SELECT c.*, tine_query_rank(?1, c.matched_text) AS owner_key \
             FROM page_text_candidates c\
         ), choices AS (\
             SELECT r.*, ROW_NUMBER() OVER (\
                 PARTITION BY r.page_id \
                 ORDER BY r.owner_key, r.source_kind, r.source_ordinal\
             ) AS owner_choice \
             FROM ranked r WHERE r.owner_key IS NOT NULL\
         ), physical AS MATERIALIZED (\
             SELECT 0 AS candidate_kind, w.page_id, w.name, w.text_kind, \
                    w.journal_day, w.path, w.matched_text, w.source_kind, \
                    tine_query_rank(?2, {framed_physical}) AS global_key, w.path AS tie_key \
             FROM choices w WHERE w.owner_choice = 1\
         ), real_identities(name_key) AS (\
             SELECT name_key FROM pages \
             UNION SELECT normalized_alias FROM reference_alias_declarations\
         ), reference_choices AS (\
             SELECT r.raw_name, r.normalized_name, ROW_NUMBER() OVER (\
                 PARTITION BY r.normalized_name \
                 ORDER BY p.path, r.raw_name, r.normalized_name, r.source_page_id\
             ) AS name_choice \
             FROM reference_postings r \
             JOIN pages p ON p.page_id = r.source_page_id \
             WHERE r.target_type = 0 AND r.reference_kind <= 4 \
               AND NOT EXISTS (SELECT 1 FROM real_identities i \
                               WHERE i.name_key = r.normalized_name)\
         ), virtual AS MATERIALIZED (\
             SELECT 1 AS candidate_kind, NULL AS page_id, v.raw_name AS name, \
                    0 AS text_kind, NULL AS journal_day, '' AS path, \
                    v.raw_name AS matched_text, 0 AS source_kind, \
                    tine_query_rank(?2, {framed_virtual}) AS global_key, \
                    v.normalized_name AS tie_key \
             FROM reference_choices v WHERE v.name_choice = 1\
         ), candidates AS (\
             SELECT * FROM physical WHERE global_key IS NOT NULL \
             UNION ALL SELECT * FROM virtual WHERE global_key IS NOT NULL\
         ) \
         SELECT candidate_kind, page_id, name, text_kind, journal_day, path, \
                matched_text, source_kind, global_key \
         FROM candidates ORDER BY global_key, tie_key COLLATE BINARY{limit}"
    );
    let rows = snapshot
        .run_projection_query(&sql, &params)
        .map_err(|error| sql_or_cancelled(snapshot, error))?;
    #[cfg(test)]
    note_friendly(|census| census.page_descriptors += rows.len());
    let mut descriptors = rows
        .iter()
        .map(|row| decode_page_descriptor(row))
        .collect::<Result<Vec<_>, _>>()
        .map_err(ResultReadError::Corrupt)?;
    let has_more = descriptors.len() > branch.limit;
    descriptors.truncate(branch.limit);
    let mut seen_physical = HashSet::new();
    let mut hits = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        check_lane(snapshot, lane)?;
        if let Some(page_id) = descriptor.page_id {
            if !seen_physical.insert(page_id) {
                return Err(ResultReadError::Corrupt(
                    "one physical page appears twice in Friendly results".into(),
                ));
            }
        }
        let rank = rank_page_text(plan, branch, &descriptor.matched_text).ok_or_else(|| {
            ResultReadError::Corrupt(
                "a selected page text no longer satisfies its rank program".into(),
            )
        })?;
        let expected = rank.global_order_key(&descriptor.name);
        if descriptor.rank_key != expected {
            return Err(ResultReadError::Corrupt(
                "a selected page rank disagrees with its compiled plan".into(),
            ));
        }
        let evidence =
            admitted_page_evidence(plan, branch, &descriptor.matched_text).ok_or_else(|| {
                ResultReadError::Corrupt(
                    "a selected page text no longer satisfies its evidence program".into(),
                )
            })?;
        let physical = descriptor.page_id.is_some();
        let score = rank.global_score(&descriptor.name);
        let page = PageEntry {
            name: descriptor.name,
            kind: descriptor.kind,
            date_key: descriptor.journal_day,
            rel_path: descriptor.path.clone(),
            path: if physical {
                graph_root.join(&descriptor.path)
            } else {
                PathBuf::new()
            },
        };
        hits.push(QueryHit::Page {
            display_text: descriptor.matched_text.clone(),
            matched_alias: descriptor.matched_alias.then_some(descriptor.matched_text),
            page,
            evidence,
            score,
            match_class: rank.match_class(),
        });
    }
    cancelled(snapshot)?;
    Ok((hits, has_more))
}

fn decode_page_descriptor(row: &[PhysicalQueryValue]) -> Result<PageDescriptor, String> {
    if row.len() != 9 {
        return Err(format!(
            "Friendly page descriptor has {} columns, expected 9",
            row.len()
        ));
    }
    let candidate_kind = integer(row, 0, "Friendly page candidate kind")?;
    let page_id = match (candidate_kind, row.get(1)) {
        (0, _) => Some(blob16(row, 1, "Friendly physical page_id")?),
        (1, Some(PhysicalQueryValue::Null)) => None,
        (1, _) => return Err("Friendly virtual page has a physical page_id".into()),
        _ => return Err("Friendly page candidate kind is not 0 or 1".into()),
    };
    let kind_value = integer(row, 3, "pages.text_kind")?;
    let kind = page_kind_from_sql(kind_value)
        .ok_or_else(|| format!("pages.text_kind {kind_value} is not a page kind"))?;
    let journal_day = match row.get(4) {
        Some(PhysicalQueryValue::Null) => None,
        Some(PhysicalQueryValue::Integer(value)) => Some(*value),
        _ => return Err("pages.journal_day is not an integer or null".into()),
    };
    let matched_alias = match integer(row, 7, "Friendly page matched-alias flag")? {
        0 => false,
        1 if page_id.is_some() => true,
        _ => return Err("Friendly page matched-alias flag is invalid".into()),
    };
    let rank_key = match row.get(8) {
        Some(PhysicalQueryValue::Blob(value)) => value.clone(),
        _ => return Err("Friendly page global rank is not a blob".into()),
    };
    let name = text(row, 2, "pages.name")?;
    let path = text(row, 5, "pages.path")?;
    if page_id.is_none() && (kind != PageKind::Page || journal_day.is_some() || !path.is_empty()) {
        return Err("Friendly virtual page carries physical metadata".into());
    }
    Ok(PageDescriptor {
        page_id,
        name,
        kind,
        journal_day,
        path,
        matched_text: text(row, 6, "Friendly matched page text")?,
        matched_alias,
        rank_key,
    })
}

struct BlockDescriptor {
    block_id: [u8; 16],
    page_id: [u8; 16],
    parent_id: Option<[u8; 16]>,
    page: String,
    kind: PageKind,
    path: String,
    visible: String,
    result_id: String,
    estimated_bytes: usize,
    tag_count: usize,
    property_count: usize,
    rank_key: Vec<u8>,
}

fn read_blocks(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    identity: &ResultIdentity,
    plan: &QueryPlan,
    branch: &QueryBranch,
    rank_program: u64,
    lane: &Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> Result<(Vec<QueryHit>, bool), ResultReadError> {
    if branch.limit == 0 {
        return Ok((Vec::new(), false));
    }
    let mut params = vec![PhysicalQueryValue::Integer(rank_program as i64)];
    let scope = plan.page_scope();
    let scope_sql = match scope {
        Some(scope) if scope.path.is_some() => {
            params.push(PhysicalQueryValue::Text(scope.path.clone().unwrap()));
            format!(" WHERE p.path = ?{}", params.len())
        }
        Some(scope) => {
            params.push(PhysicalQueryValue::Integer(match scope.page_kind {
                PageKind::Page => 0,
                PageKind::Journal => 1,
            }));
            let kind = params.len();
            params.push(PhysicalQueryValue::Text(crate::refs::page_key(&scope.name)));
            format!(
                " WHERE p.text_kind = ?{kind} AND p.name_key = ?{}",
                params.len()
            )
        }
        None => String::new(),
    };
    let limit = limit_clause(branch.limit, &mut params);
    let sql = format!(
        "WITH ranked AS MATERIALIZED (\
             SELECT b.block_id, b.page_id, b.parent_block_id, b.order_key, \
                    bt.query_visible, p.name, p.text_kind, p.path, \
                    q.page_id AS result_page_id, q.preorder, q.result_id, q.estimated_bytes, q.tag_count, \
                    q.property_count, \
                    CASE WHEN bt.block_id IS NULL THEN zeroblob(1) \
                         ELSE tine_query_rank(?1, bt.query_visible) END AS rank_key, \
                    CASE WHEN bt.block_id IS NULL THEN 1 ELSE 0 END AS missing_text \
             FROM blocks b \
             LEFT JOIN block_text bt ON bt.block_id = b.block_id \
             LEFT JOIN pages p ON p.page_id = b.page_id \
             LEFT JOIN query_block_results q ON q.block_id = b.block_id{scope_sql}\
         ) \
         SELECT block_id, page_id, parent_block_id, order_key, query_visible, \
                name, text_kind, path, result_page_id, preorder, result_id, estimated_bytes, \
                tag_count, property_count, rank_key \
         FROM ranked WHERE missing_text = 1 OR rank_key IS NOT NULL \
         ORDER BY missing_text DESC, rank_key, path COLLATE BINARY, preorder{limit}"
    );
    let rows = snapshot
        .run_projection_query(&sql, &params)
        .map_err(|error| sql_or_cancelled(snapshot, error))?;
    #[cfg(test)]
    note_friendly(|census| census.block_descriptors += rows.len());
    let mut descriptors = rows
        .iter()
        .map(|row| decode_block_descriptor(row, identity))
        .collect::<Result<Vec<_>, _>>()
        .map_err(ResultReadError::Corrupt)?;
    let has_more = descriptors.len() > branch.limit;
    descriptors.truncate(branch.limit);
    let mut seen = HashSet::new();
    for descriptor in &descriptors {
        check_lane(snapshot, lane)?;
        if !seen.insert(descriptor.block_id) {
            return Err(ResultReadError::Corrupt(
                "one physical block appears twice in Friendly results".into(),
            ));
        }
        let expected = rank_block_text(plan, branch, &descriptor.visible).ok_or_else(|| {
            ResultReadError::Corrupt("a selected block no longer satisfies its rank program".into())
        })?;
        if descriptor.rank_key != expected.order_key() {
            return Err(ResultReadError::Corrupt(
                "a selected block rank disagrees with its compiled plan".into(),
            ));
        }
    }

    let breadcrumbs = read_breadcrumbs(snapshot, &descriptors, lane)?;
    let mut payload: Vec<Option<BlockDto>> = vec![None; descriptors.len()];
    check_lane(snapshot, lane)?;
    let cancellation = snapshot.cancellation();
    read_admitted_payload(
        snapshot,
        &descriptors,
        PayloadChannel::Selection,
        |descriptor| PayloadFacts {
            block_id: descriptor.block_id,
            page_id: descriptor.page_id,
            result_id: &descriptor.result_id,
            estimated_bytes: descriptor.estimated_bytes,
            tag_count: descriptor.tag_count,
            property_count: descriptor.property_count,
        },
        |at, dto| {
            if lane.as_ref().is_some_and(|lane| lane()) {
                cancellation.cancel();
            }
            payload[at] = Some(dto);
        },
    )?;
    check_lane(snapshot, lane)?;
    let mut hits = Vec::with_capacity(descriptors.len());
    for ((descriptor, mut block), breadcrumb) in descriptors
        .into_iter()
        .zip(payload.into_iter())
        .zip(breadcrumbs)
    {
        check_lane(snapshot, lane)?;
        let mut block = block.take().ok_or_else(|| {
            ResultReadError::Corrupt("an admitted Friendly block has no payload".into())
        })?;
        block.breadcrumb = breadcrumb;
        let rank = rank_block_text(plan, branch, &descriptor.visible).ok_or_else(|| {
            ResultReadError::Corrupt(
                "an admitted block no longer satisfies its rank program".into(),
            )
        })?;
        let evidence =
            admitted_block_evidence(plan, branch, &descriptor.visible).ok_or_else(|| {
                ResultReadError::Corrupt(
                    "an admitted block no longer satisfies its evidence program".into(),
                )
            })?;
        hits.push(QueryHit::Block {
            page: descriptor.page,
            kind: descriptor.kind,
            path: descriptor.path,
            block,
            display_text: descriptor.visible,
            evidence,
            score: rank.score(),
            match_class: rank.match_class(),
        });
    }
    cancelled(snapshot)?;
    Ok((hits, has_more))
}

fn decode_block_descriptor(
    row: &[PhysicalQueryValue],
    identity: &ResultIdentity,
) -> Result<BlockDescriptor, String> {
    if row.len() != 15 {
        return Err(format!(
            "Friendly block descriptor has {} columns, expected 15",
            row.len()
        ));
    }
    let block_id = blob16(row, 0, "blocks.block_id")?;
    let page_id = blob16(row, 1, "blocks.page_id")?;
    let parent_id = optional_blob16(row, 2, "blocks.parent_block_id")?;
    let order_key = text(row, 3, "blocks.order_key")?;
    let visible = text(row, 4, "block_text.query_visible")?;
    let page = text(row, 5, "pages.name")?;
    let kind_value = integer(row, 6, "pages.text_kind")?;
    let kind = page_kind_from_sql(kind_value)
        .ok_or_else(|| format!("pages.text_kind {kind_value} is not a page kind"))?;
    let path = text(row, 7, "pages.path")?;
    let result_page = blob16(row, 8, "query_block_results.page_id")?;
    if result_page != page_id {
        return Err("query_block_results.page_id does not own its block's page".into());
    }
    let preorder = integer(row, 9, "query_block_results.preorder")?;
    if preorder < 0 {
        return Err("query_block_results.preorder is negative".into());
    }
    let stored_id = text(row, 10, "query_block_results.result_id")?;
    if stored_id.is_empty() {
        return Err("query_block_results.result_id is empty".into());
    }
    let stored_estimate = count(row, 11, "query_block_results.estimated_bytes")?;
    let (result_id, estimated_bytes) = resolve_identity(
        identity,
        page_id,
        &path,
        &order_key,
        &stored_id,
        stored_estimate,
    )?;
    let rank_key = match row.get(14) {
        Some(PhysicalQueryValue::Blob(value)) => value.clone(),
        _ => return Err("Friendly block rank is not a blob".into()),
    };
    Ok(BlockDescriptor {
        block_id,
        page_id,
        parent_id,
        page,
        kind,
        path,
        visible,
        result_id,
        estimated_bytes,
        tag_count: count(row, 12, "query_block_results.tag_count")?,
        property_count: count(row, 13, "query_block_results.property_count")?,
        rank_key,
    })
}

fn optional_blob16(
    row: &[PhysicalQueryValue],
    at: usize,
    what: &str,
) -> Result<Option<[u8; 16]>, String> {
    match row.get(at) {
        Some(PhysicalQueryValue::Null) => Ok(None),
        Some(PhysicalQueryValue::Blob(_)) => blob16(row, at, what).map(Some),
        _ => Err(format!("{what} is not a blob or null")),
    }
}

#[derive(Clone)]
struct Ancestor {
    page_id: [u8; 16],
    parent_id: Option<[u8; 16]>,
    visible: String,
}

fn read_breadcrumbs(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    descriptors: &[BlockDescriptor],
    lane: &Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> Result<Vec<Vec<String>>, ResultReadError> {
    let mut active = descriptors
        .iter()
        .map(|row| row.parent_id)
        .collect::<Vec<_>>();
    let mut visited = vec![HashSet::new(); descriptors.len()];
    let mut crumbs = vec![Vec::new(); descriptors.len()];
    while active.iter().any(Option::is_some) {
        check_lane(snapshot, lane)?;
        let requested = active.iter().flatten().copied().collect::<HashSet<_>>();
        let mut ancestors = HashMap::with_capacity(requested.len());
        let requested_ids = requested.iter().copied().collect::<Vec<_>>();
        for batch in requested_ids.chunks(PAYLOAD_BATCH) {
            #[cfg(test)]
            run_one_shot_hook(&BEFORE_ANCESTOR_BATCH);
            check_lane(snapshot, lane)?;
            let params = batch
                .iter()
                .map(|id| PhysicalQueryValue::Blob(id.to_vec()))
                .collect::<Vec<_>>();
            let sql = format!(
                "SELECT b.block_id, b.page_id, b.parent_block_id, bt.query_visible \
                 FROM blocks b LEFT JOIN block_text bt ON bt.block_id = b.block_id \
                 WHERE b.block_id IN ({})",
                crate::query::results::placeholders(params.len())
            );
            let rows = snapshot
                .run_projection_query(&sql, &params)
                .map_err(|error| sql_or_cancelled(snapshot, error))?;
            #[cfg(test)]
            note_friendly(|census| {
                census.ancestor_statements += 1;
                census.ancestor_rows += rows.len();
            });
            for row in &rows {
                if row.len() != 4 {
                    return Err(ResultReadError::Corrupt(format!(
                        "Friendly ancestor row has {} columns, expected 4",
                        row.len()
                    )));
                }
                let id =
                    blob16(row, 0, "ancestor blocks.block_id").map_err(ResultReadError::Corrupt)?;
                if !requested.contains(&id) {
                    return Err(ResultReadError::Corrupt(
                        "an ancestor row belongs to no admitted chain".into(),
                    ));
                }
                let ancestor = Ancestor {
                    page_id: blob16(row, 1, "ancestor blocks.page_id")
                        .map_err(ResultReadError::Corrupt)?,
                    parent_id: optional_blob16(row, 2, "ancestor blocks.parent_block_id")
                        .map_err(ResultReadError::Corrupt)?,
                    visible: text(row, 3, "ancestor block_text.query_visible")
                        .map_err(ResultReadError::Corrupt)?,
                };
                if ancestors.insert(id, ancestor).is_some() {
                    return Err(ResultReadError::Corrupt(
                        "one ancestor has two payload rows".into(),
                    ));
                }
            }
        }
        for (at, next) in active.iter_mut().enumerate() {
            let Some(id) = *next else { continue };
            if !visited[at].insert(id) {
                return Err(ResultReadError::Corrupt(
                    "an admitted block has a cyclic ancestor chain".into(),
                ));
            }
            let ancestor = ancestors.get(&id).ok_or_else(|| {
                ResultReadError::Corrupt("an admitted block has a missing ancestor".into())
            })?;
            if ancestor.page_id != descriptors[at].page_id {
                return Err(ResultReadError::Corrupt(
                    "an admitted block ancestor belongs to another page".into(),
                ));
            }
            crumbs[at].push(crate::doc::crumb_line_text(&ancestor.visible));
            *next = ancestor.parent_id;
        }
    }
    for trail in &mut crumbs {
        trail.reverse();
    }
    cancelled(snapshot)?;
    Ok(crumbs)
}

#[cfg(test)]
#[path = "friendly_tests.rs"]
mod friendly_tests;
