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
use crate::query::candidate::{
    expression_plan, interactive_scan_budget, CandidateMode, CandidatePlan,
};
use crate::query::ir::FriendlyPageMatchScope;
use crate::query::rank::QueryRankPrograms;
use crate::query::results::{
    count, hydrate_page_rows, integer, read_admitted_payload, resolve_identity, sql_or_cancelled,
    text, PageResultDescriptor, PayloadChannel, PayloadFacts, ResultIdentity, ResultReadError,
    PAYLOAD_BATCH,
};
use crate::query::sql::{block_sort_expression, page_sort_expression, SortBinder};
use crate::query::text::{framed_byte_len_and_text_sql, framed_pair_sql};
use crate::query_plan::{
    admitted_block_evidence, admitted_page_evidence, rank_block_text_folded, rank_page_text,
    ObjectiveMatchClass, QueryBranch, QueryExecution, QueryExplanation, QueryHasMore, QueryHit,
    QueryPlan, QueryTarget, PAGE_OWNER_RANK_KEY_LEN, PAGE_RANK_KEY_LEN,
};
use crate::vocab::{BlockDto, PageEntry, PageKind};

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
    /// Rows yielded by the streamed interactive candidate statement.
    pub(crate) block_candidate_visits: usize,
    /// Candidate rows passed to the exact Rust verifier.
    pub(crate) block_candidate_verifications: usize,
    /// Rows the block statement actually ranked (see the rank program).
    pub(crate) block_rank_evaluations: usize,
    /// Physical title/alias and virtual-name candidates passed to the page
    /// rank program. Each candidate produces both owner and global keys.
    pub(crate) page_rank_evaluations: usize,
    pub(crate) ancestor_statements: usize,
    pub(crate) ancestor_rows: usize,
}

#[cfg(test)]
thread_local! {
    static FRIENDLY_CENSUS: std::cell::Cell<FriendlyReadCensus> =
        const { std::cell::Cell::new(FriendlyReadCensus {
            page_descriptors: 0,
            block_descriptors: 0,
            block_candidate_visits: 0,
            block_candidate_verifications: 0,
            block_rank_evaluations: 0,
            page_rank_evaluations: 0,
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
    Pages { branch: QueryBranch, rank: u64 },
    Blocks { branch: QueryBranch, rank: u64 },
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
                let rank_plan = Arc::clone(&plan);
                let rank_branch = branch.clone();
                let rank = programs.bind_byte_len_and_text(move |physical_name_len, candidate| {
                    #[cfg(test)]
                    run_one_shot_hook(&BEFORE_FRIENDLY_RANK);
                    #[cfg(test)]
                    note_friendly(|census| census.page_rank_evaluations += 1);
                    Ok(
                        rank_page_text(&rank_plan, &rank_branch, candidate).map(|rank| {
                            let mut key = rank.owner_order_key().to_vec();
                            key.extend_from_slice(
                                &rank.global_order_key_for_name_len(physical_name_len),
                            );
                            key
                        }),
                    )
                });
                branches.push(BoundBranch::Pages {
                    branch: branch.clone(),
                    rank,
                });
            }
            QueryTarget::Blocks => {
                let rank_plan = Arc::clone(&plan);
                let rank_branch = branch.clone();
                let rank = programs.bind_pair(move |raw, path| {
                    #[cfg(test)]
                    run_one_shot_hook(&BEFORE_FRIENDLY_RANK);
                    // One per ROW the statement ranked, which is the size of the
                    // scan the candidate bound exists to cut. Counting returned
                    // rows cannot see it: the outer select already drops
                    // non-matches, so an unbounded read and a bounded one return
                    // the same rows and rank wildly different numbers of them.
                    #[cfg(test)]
                    note_friendly(|census| census.block_rank_evaluations += 1);
                    let projection =
                        crate::query::text::visible_projection_from_raw_path(raw, path);
                    Ok(rank_block_text_folded(
                        &rank_plan,
                        &rank_branch,
                        &projection.visible,
                        &projection.visible_lower,
                    )
                    .map(|rank| {
                        let mut key = rank.order_key().to_vec();
                        key.extend_from_slice(projection.visible.as_bytes());
                        key
                    }))
                });
                branches.push(BoundBranch::Blocks {
                    branch: branch.clone(),
                    rank,
                });
            }
        }
    }
    // The sort vocabulary's case-folding program has to exist in THIS table
    // before the rank function is installed: a program bound afterwards is not
    // callable from the statement that needs it. Bound only when a section
    // actually states a sort, so a plain search binds nothing extra.
    let page_sort_program = plan
        .page_view()
        .filter(|view| !view.sort.is_empty())
        .map(|_| FriendlySortPrograms {
            lowercase: programs.bind_unicode_lowercase(),
            visible: programs.bind_pair(|raw, path| {
                let visible = crate::query::text::visible_from_raw_path(raw, path);
                Ok(Some(
                    visible
                        .split('\n')
                        .next()
                        .unwrap_or_default()
                        .to_lowercase()
                        .into_bytes(),
                ))
            }),
        });
    let block_sort_program = plan
        .block_view()
        .filter(|view| !view.sort.is_empty())
        .map(|_| FriendlySortPrograms {
            lowercase: programs.bind_unicode_lowercase(),
            visible: programs.bind_pair(|raw, path| {
                let visible = crate::query::text::visible_from_raw_path(raw, path);
                Ok(Some(
                    visible
                        .split('\n')
                        .next()
                        .unwrap_or_default()
                        .to_lowercase()
                        .into_bytes(),
                ))
            }),
        });
    // Page membership by CONTENT asks the Blocks section's own predicate of a
    // page's blocks, so it reuses that branch's already-bound rank program
    // rather than compiling a second copy of the same predicate.
    let content = branches.iter().find_map(|bound| match bound {
        BoundBranch::Blocks { branch, rank } => Some((branch.clone(), *rank)),
        BoundBranch::Pages { .. } => None,
    });
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
                BoundBranch::Pages { branch, rank } => {
                    let (mut section, more) = read_pages(
                        snapshot,
                        inputs.graph_root,
                        &plan,
                        branch,
                        *rank,
                        content.as_ref().map(|(branch, rank)| (branch, *rank)),
                        page_sort_program,
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
                        block_sort_program,
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

/// **The Friendly reader's half of the shared sort vocabulary** (I-12).
///
/// `query/sql.rs` owns what a sort field MEANS; this owns only how this
/// statement binds the parameters that meaning needs. The case-folding program
/// is bound into the operation's ONE program table before the rank function is
/// installed — a program registered afterwards would not be callable from the
/// statement that needs it — so the id arrives already bound and this binder
/// only places it in a parameter slot.
///
/// [`SortBinder::has_recency`] is `false` and stays false: a Friendly read
/// captures no `PageRecencyPrograms`, and inventing a second recency producer
/// to fill the gap is the twin D-14 forbids. A recency field therefore
/// contributes no order term. Nothing offers one: the Display picker's sort
/// vocabulary (`sheet/fields.ts::querySortFieldName`) does not include recency
/// for either family.
struct FriendlySortBinder<'a> {
    programs: FriendlySortPrograms,
    params: &'a mut Vec<PhysicalQueryValue>,
    lowercase: Option<String>,
    keys: HashMap<String, String>,
}

#[derive(Clone, Copy)]
struct FriendlySortPrograms {
    lowercase: u64,
    visible: u64,
}

impl<'a> FriendlySortBinder<'a> {
    fn new(programs: FriendlySortPrograms, params: &'a mut Vec<PhysicalQueryValue>) -> Self {
        Self {
            programs,
            params,
            lowercase: None,
            keys: HashMap::new(),
        }
    }
}

impl SortBinder for FriendlySortBinder<'_> {
    fn lowercase(&mut self) -> String {
        if let Some(bound) = &self.lowercase {
            return bound.clone();
        }
        self.params
            .push(PhysicalQueryValue::Integer(self.programs.lowercase as i64));
        let bound = format!("?{}", self.params.len());
        self.lowercase = Some(bound.clone());
        bound
    }

    fn property_key(&mut self, key: String) -> String {
        if let Some(bound) = self.keys.get(&key) {
            return bound.clone();
        }
        self.params.push(PhysicalQueryValue::Text(key.clone()));
        let bound = format!("?{}", self.params.len());
        self.keys.insert(key, bound.clone());
        bound
    }

    fn visible_lowercase(&mut self, block_alias: &str, _page_alias: &str) -> String {
        self.params
            .push(PhysicalQueryValue::Integer(self.programs.visible as i64));
        let program = self.params.len();
        let framed = framed_pair_sql(
            &format!("{block_alias}.raw_content"),
            &format!("{block_alias}.path"),
        );
        format!("tine_query_rank(?{program}, {framed})")
    }

    fn has_recency(&self) -> bool {
        false
    }
}

/// The authored ORDER BY terms of one section, or an empty list when the
/// section states no sort this reader can carry — and then the section keeps
/// its established relevance order.
fn sort_terms(
    view: Option<&crate::query::ir::ViewSettings>,
    program: Option<FriendlySortPrograms>,
    params: &mut Vec<PhysicalQueryValue>,
    mut expression: impl FnMut(&str, &mut dyn SortBinder) -> Option<String>,
) -> Vec<String> {
    let (Some(view), Some(program)) = (view, program) else {
        return Vec::new();
    };
    let mut binder = FriendlySortBinder::new(program, params);
    let mut terms = Vec::new();
    for (field, direction) in &view.sort {
        let Some(sql) = expression(field.as_str(), &mut binder) else {
            continue;
        };
        terms.push(format!(
            "{sql} {}",
            match direction {
                crate::query::ir::SortDir::Asc => "ASC",
                crate::query::ir::SortDir::Desc => "DESC",
            }
        ));
    }
    terms
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

/// One admitted page candidate, before its evidence and payload are built.
struct PageDescriptor {
    /// Which membership source admitted it: names/aliases, or contained block
    /// text. The two are ranked in different key spaces and verified against
    /// different branches, so the row says which it is rather than leaving the
    /// reader to infer it from a rank blob.
    from_content: bool,
    page_id: Option<i64>,
    name: String,
    kind: PageKind,
    journal_day: Option<i64>,
    path: String,
    matched_text: String,
    /// Present only for content-derived page candidates, from the same
    /// operation-local projection as `matched_text`.
    matched_text_lower: Option<String>,
    matched_alias: bool,
    rank_key: Vec<u8>,
    /// The producer's stored payload facts, selected only on the
    /// Display-enabled path, which is the only one that hydrates.
    payload: Option<(usize, usize)>,
}

const VIRTUAL_REFERENCE_CHOICES_CTE: &str = "reference_choices AS (\
         SELECT n.raw AS raw_name, n.key AS normalized_name, ROW_NUMBER() OVER (\
             PARTITION BY n.key ORDER BY n.raw, n.key, n.name_id\
         ) AS name_choice \
         FROM names n WHERE EXISTS (\
             SELECT 1 FROM reference_postings r \
             INDEXED BY reference_postings_navigation_names_idx \
             WHERE r.target_name_id = n.name_id \
               AND r.target_type = 0 AND r.reference_kind <= 4\
         ) AND NOT EXISTS (SELECT 1 FROM real_identities i \
                           WHERE i.name_key = n.key)\
     )";

// `names` is UNIQUE(key, raw), so `name_id` can only break a tie between rows
// already identical for the user-visible spelling and identity. It replaces
// the old occurrence-row `source_page_id` tie without changing which spelling
// wins, while the indexed EXISTS stops after the first eligible occurrence.

#[allow(clippy::too_many_arguments)]
fn read_pages(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    graph_root: &Path,
    plan: &QueryPlan,
    branch: &QueryBranch,
    rank: u64,
    content: Option<(&QueryBranch, u64)>,
    sort_program: Option<FriendlySortPrograms>,
    lane: &Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> Result<(Vec<QueryHit>, bool), ResultReadError> {
    if branch.limit == 0 {
        return Ok((Vec::new(), false));
    }
    let scope = plan.page_match_scope();
    let want_names = matches!(
        scope,
        FriendlyPageMatchScope::Names | FriendlyPageMatchScope::Both
    );
    // Content membership is the BLOCK predicate asked of a page's own blocks.
    // A plan with no block branch states no such predicate, so a Content-only
    // search over it admits nothing rather than falling back to names.
    let content = matches!(
        scope,
        FriendlyPageMatchScope::Content | FriendlyPageMatchScope::Both
    )
    .then_some(content)
    .flatten();
    if !want_names && content.is_none() {
        return Ok((Vec::new(), false));
    }
    let hydrate = plan.page_view().is_some();
    let mut params = vec![PhysicalQueryValue::Integer(rank as i64)];
    let framed_physical = framed_byte_len_and_text_sql("c.name", "c.matched_text");
    let framed_virtual = framed_byte_len_and_text_sql("v.raw_name", "v.raw_name");
    // Names and aliases: unchanged selection, ranking, owner-local override and
    // virtual reference-name suggestions. `match_source` is a constant 0 here,
    // so a Names search orders exactly as it did before this packet.
    //
    // A page that exists only by reference can be spelled several ways
    // (`[[Ghost Page]]` here, `[[ghost page]]` there). Which spelling is shown
    // is a CROSS-PATH CONTRACT, not a local choice: `reference_choices` below
    // and `DirectProjection::referenced_page_names` answer the same question
    // for the same user-visible list, and `friendly_main_reader_matches_the_
    // independent_walk_for_rank_and_identity_shapes` asserts they agree. The
    // rule is **the lexicographically smallest raw spelling wins**, which both
    // can compute from the name alone. This ordering used to lead with
    // `p.path` — the spelling from the alphabetically first owner page — and
    // the navigation reader agreed only by accident, because it ordered by the
    // opaque `source_page_id` blob and that happened to rank the same row
    // first on the fixture. When the navigation reader moved onto
    // `reference_postings_navigation_names_idx` (tine-storage v0.24.0), which
    // carries no path and no page id, that coincidence broke. Keep both sides
    // keyed on `raw_name` or the two lists will disagree again.
    let owner_partition = if plan.page_name_suggestions() {
        // Autocomplete keeps every distinct authored spelling for an owner so
        // an alias is selectable even when the canonical title also matches.
        "r.page_id, r.matched_text"
    } else {
        // Friendly search remains one coherent row per physical owner.
        "r.page_id"
    };
    let physical_tie_key = if plan.page_name_suggestions() {
        "w.path || char(0) || w.matched_text"
    } else {
        "w.path"
    };
    let names_ctes = if want_names {
        let reference_choices = VIRTUAL_REFERENCE_CHOICES_CTE;
        format!(
            "page_text_candidates(page_id, name, text_kind, journal_day, path, \
                  matched_text, source_kind, source_ordinal) AS (\
             SELECT p.page_id, pn.raw, p.text_kind, p.journal_day, p.path, \
                    pn.raw, 0, -1 FROM pages p JOIN names pn ON pn.name_id = p.name_id \
             UNION ALL \
             SELECT p.page_id, pn.raw, p.text_kind, p.journal_day, p.path, \
                    an.raw, 1, MIN(a.ordinal) \
             FROM reference_alias_declarations a \
             JOIN pages p ON p.page_id = a.source_page_id \
             JOIN names pn ON pn.name_id = p.name_id \
             JOIN names an ON an.name_id = a.alias_name_id \
             WHERE a.source_entity_type = 0 AND a.source_entity_id = a.source_page_id \
             GROUP BY p.page_id, a.alias_name_id, an.raw\
         ), ranked AS MATERIALIZED (\
             SELECT c.*, tine_query_rank(?1, {framed_physical}) AS rank_key \
             FROM page_text_candidates c\
         ), choices AS (\
             SELECT r.*, ROW_NUMBER() OVER (\
                 PARTITION BY {owner_partition} \
                 ORDER BY substr(r.rank_key, 1, {owner_rank_len}), \
                          r.source_kind, r.source_ordinal\
             ) AS owner_choice \
             FROM ranked r WHERE r.rank_key IS NOT NULL\
         ), physical AS MATERIALIZED (\
             SELECT 0 AS match_source, 0 AS candidate_kind, w.page_id, w.name, w.text_kind, \
                    w.journal_day, w.path, w.matched_text, w.source_kind, \
                    substr(w.rank_key, {global_rank_at}, {global_rank_len}) AS global_key, \
                    {physical_tie_key} AS tie_key \
             FROM choices w WHERE w.owner_choice = 1\
         ), real_identities(name_key) AS (\
             SELECT n.key FROM pages p JOIN names n ON n.name_id = p.name_id \
             UNION SELECT n.key FROM reference_alias_declarations a \
             JOIN names n ON n.name_id = a.alias_name_id\
         ), {reference_choices}, virtual AS MATERIALIZED (\
             SELECT 0 AS match_source, 1 AS candidate_kind, NULL AS page_id, v.raw_name AS name, \
                    0 AS text_kind, NULL AS journal_day, '' AS path, \
                    v.raw_name AS matched_text, 0 AS source_kind, \
                    substr(tine_query_rank(?1, {framed_virtual}), \
                           {global_rank_at}, {global_rank_len}) AS global_key, \
                    v.normalized_name AS tie_key \
             FROM reference_choices v WHERE v.name_choice = 1\
         )",
            owner_rank_len = PAGE_OWNER_RANK_KEY_LEN,
            global_rank_at = PAGE_OWNER_RANK_KEY_LEN + 1,
            global_rank_len = PAGE_RANK_KEY_LEN,
        )
    } else {
        String::new()
    };
    // Content membership: one page qualifies through ONE of its own blocks, and
    // that block is chosen by the same block rank program the Blocks section
    // ranks with. Terms are never matched across unrelated blocks and blocks are
    // never concatenated — the window picks a single winning row per page.
    // The page-by-content scan is the SECOND full read of every block one
    // Ctrl-K keystroke performs, and it long outlived the first being bounded:
    // with only the Blocks statement driven from the index, a zero-hit needle
    // on a 10,000-page graph still cost ~1.6 s, all of it here. It ranks with
    // the Blocks branch's own program, so the same needle admits the same
    // blocks and the same candidate set is sound for it. Unlike the Blocks
    // statement this join is INNER, so it never carried a textless arm and
    // driving it changes no result at all.
    let interactive_content_ids = match (content, plan.candidate_mode()) {
        (Some((content_branch, _)), CandidateMode::Interactive { window }) => Some(
            interactive_verified_block_ids(snapshot, plan, content_branch, window, lane)?,
        ),
        _ => None,
    };
    let content_budget_spent = interactive_content_ids
        .as_ref()
        .is_some_and(|verified| verified.budget_spent);
    let content_block_source = match content {
        Some((content_branch, _)) => match interactive_content_ids.as_ref() {
            Some(verified) => verified_id_block_source(&mut params, &verified.ids),
            None => indexed_block_source(&mut params, &content_branch.predicate).sql,
        },
        None => "blocks b".to_string(),
    };
    let content_ctes = content.map(|(_, rank)| {
        params.push(PhysicalQueryValue::Integer(rank as i64));
        let program = params.len();
        // Page-by-content reuses the Blocks branch's already-bound program, so
        // it must frame its pair exactly as that statement does.
        let framed_block_text = crate::query::text::framed_pair_sql("bt.content", "p.path");
        let verified_window = String::new();
        let ranked_hint = " MATERIALIZED";
        // In Both, a page that also matched by name keeps its NAMES winner:
        // the union is by physical identity, and the name evidence is the
        // stronger statement about why the page is in the answer.
        let dedupe = if want_names {
            " AND NOT EXISTS (SELECT 1 FROM physical x WHERE x.page_id = k.page_id)"
        } else {
            ""
        };
        format!(
            "content_ranked AS{ranked_hint} (\
                 SELECT b.block_id, b.page_id, bt.content AS matched_text, p.path AS matched_path, \
                        tine_query_rank(?{program}, {framed_block_text}) AS content_key \
                 FROM {content_block_source} JOIN block_text bt ON bt.block_id = b.block_id \
                 JOIN pages p ON p.page_id = b.page_id\
             ), content_verified AS MATERIALIZED (\
                 SELECT * FROM content_ranked WHERE content_key IS NOT NULL{verified_window}\
             ), content_choices AS (\
                 SELECT k.*, ROW_NUMBER() OVER (\
                     PARTITION BY k.page_id ORDER BY k.content_key\
                 ) AS content_choice \
                 FROM content_verified k WHERE 1{dedupe}\
             ), content AS MATERIALIZED (\
                 SELECT 1 AS match_source, 0 AS candidate_kind, p.page_id, pn.raw AS name, p.text_kind, \
                        p.journal_day, p.path, k.matched_text, 0 AS source_kind, \
                        substr(k.content_key, 1, {rank_len}) AS global_key, k.content_key AS tie_key \
                 FROM content_choices k JOIN pages p ON p.page_id = k.page_id \
                 JOIN names pn ON pn.name_id = p.name_id \
                 WHERE k.content_choice = 1\
             )",
            rank_len = crate::query_plan::BLOCK_RANK_KEY_LEN,
        )
    });
    let mut ctes = Vec::new();
    if want_names {
        ctes.push(names_ctes);
    }
    if let Some(content_ctes) = content_ctes {
        ctes.push(content_ctes);
    }
    let mut arms = Vec::new();
    if want_names {
        arms.push("SELECT * FROM physical WHERE global_key IS NOT NULL".to_string());
        arms.push("SELECT * FROM virtual WHERE global_key IS NOT NULL".to_string());
    }
    if content.is_some() {
        arms.push("SELECT * FROM content WHERE global_key IS NOT NULL".to_string());
    }
    ctes.push(format!("candidates AS ({})", arms.join(" UNION ALL ")));
    // An authored page sort orders the COMPLETE union before any bound applies
    // (Q4's settled ordering). With no authored sort the established Friendly
    // order stands, and `match_source` is what puts Names winners before
    // Content-only ones.
    let authored = sort_terms(
        plan.page_view(),
        sort_program,
        &mut params,
        |field, binder| page_sort_expression(field, "c", binder),
    );
    let order = if authored.is_empty() {
        "c.match_source, c.global_key, c.tie_key COLLATE BINARY".to_string()
    } else {
        format!(
            "{}, c.path COLLATE BINARY, c.name COLLATE BINARY",
            authored.join(", ")
        )
    };
    let (payload_columns, payload_join) = if hydrate {
        (
            ", stored.estimated_bytes, stored.property_count",
            " LEFT JOIN pages stored ON stored.page_id = c.page_id",
        )
    } else {
        ("", "")
    };
    let limit = limit_clause(branch.limit, &mut params);
    let sql = format!(
        "WITH {} \
         SELECT c.match_source, c.candidate_kind, c.page_id, c.name, c.text_kind, c.journal_day, \
                c.path, c.matched_text, c.source_kind, c.global_key{payload_columns} \
         FROM candidates c{payload_join} ORDER BY {order}{limit}",
        ctes.join(", ")
    );
    let rows = crate::query::projection_sql::run(snapshot, &sql, &params)
        .map_err(|error| sql_or_cancelled(snapshot, error))?;
    #[cfg(test)]
    note_friendly(|census| census.page_descriptors += rows.len());
    let mut descriptors = rows
        .iter()
        .map(|row| decode_page_descriptor(row, hydrate))
        .collect::<Result<Vec<_>, _>>()
        .map_err(ResultReadError::Corrupt)?;
    let has_more = descriptors.len() > branch.limit || content_budget_spent;
    descriptors.truncate(branch.limit);
    // Hydrate only ADMITTED stored pages, through the shared page hydrator, and
    // only once the descriptors are final. A virtual suggestion names no stored
    // page, so it is not in this batch and gains no fabricated properties.
    let mut hydrated: HashMap<i64, crate::query::ir::PageRow> = HashMap::new();
    if hydrate {
        let admitted = descriptors
            .iter()
            .filter_map(|descriptor| {
                let page_id = descriptor.page_id?;
                let (estimated_bytes, property_count) = descriptor.payload?;
                Some(PageResultDescriptor {
                    page_id,
                    name: descriptor.name.clone(),
                    kind: descriptor.kind,
                    journal_day: descriptor.journal_day,
                    path: descriptor.path.clone(),
                    estimated_bytes,
                    property_count,
                    matched_total: 0,
                })
            })
            .collect::<Vec<_>>();
        check_lane(snapshot, lane)?;
        for (descriptor, row) in admitted.iter().zip(hydrate_page_rows(snapshot, &admitted)?) {
            hydrated.insert(descriptor.page_id, row);
        }
    }
    let mut seen_physical = HashSet::new();
    let mut hits = Vec::with_capacity(descriptors.len());
    for mut descriptor in descriptors {
        check_lane(snapshot, lane)?;
        if let Some(page_id) = descriptor.page_id.filter(|_| !plan.page_name_suggestions()) {
            if !seen_physical.insert(page_id) {
                return Err(ResultReadError::Corrupt(
                    "one physical page appears twice in Friendly results".into(),
                ));
            }
        }
        let (evidence, score, match_class) = if descriptor.from_content {
            let (block_branch, _) = content.ok_or_else(|| {
                ResultReadError::Corrupt(
                    "a content page candidate arrived without a block branch".into(),
                )
            })?;
            let matched_text_lower = descriptor.matched_text_lower.as_deref().ok_or_else(|| {
                ResultReadError::Corrupt("a selected content page has no folded block text".into())
            })?;
            let rank = rank_block_text_folded(
                plan,
                block_branch,
                &descriptor.matched_text,
                matched_text_lower,
            )
            .ok_or_else(|| {
                ResultReadError::Corrupt(
                    "a selected page's block no longer satisfies its rank program".into(),
                )
            })?;
            if descriptor.rank_key != rank.order_key() {
                return Err(ResultReadError::Corrupt(
                    "a selected content page rank disagrees with its compiled plan".into(),
                ));
            }
            let evidence = admitted_block_evidence(plan, block_branch, &descriptor.matched_text)
                .ok_or_else(|| {
                    ResultReadError::Corrupt(
                        "a selected page's block no longer satisfies its evidence program".into(),
                    )
                })?;
            // A page admitted by what a block of it SAYS is body evidence, which
            // is the class this vocabulary already has for exactly that. Its
            // score is the winning block's, so two content winners order by how
            // well their own best block matched.
            (evidence, rank.score(), ObjectiveMatchClass::BodyEvidence)
        } else {
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
            let evidence = admitted_page_evidence(plan, branch, &descriptor.matched_text)
                .ok_or_else(|| {
                    ResultReadError::Corrupt(
                        "a selected page text no longer satisfies its evidence program".into(),
                    )
                })?;
            (
                evidence,
                rank.global_score(&descriptor.name),
                rank.match_class(),
            )
        };
        let physical = descriptor.page_id.is_some();
        let row = descriptor
            .page_id
            .and_then(|page_id| hydrated.remove(&page_id));
        let page = PageEntry {
            name: std::mem::take(&mut descriptor.name),
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
            match_class,
            row,
        });
    }
    cancelled(snapshot)?;
    Ok((hits, has_more))
}

fn decode_page_descriptor(
    row: &[PhysicalQueryValue],
    hydrate: bool,
) -> Result<PageDescriptor, String> {
    let expected = if hydrate { 12 } else { 10 };
    if row.len() != expected {
        return Err(format!(
            "Friendly page descriptor has {} columns, expected {expected}",
            row.len()
        ));
    }
    let from_content = match integer(row, 0, "Friendly page match source")? {
        0 => false,
        1 => true,
        _ => return Err("Friendly page match source is not 0 or 1".into()),
    };
    let candidate_kind = integer(row, 1, "Friendly page candidate kind")?;
    let page_id = match (candidate_kind, row.get(2)) {
        (0, _) => Some(integer(row, 2, "Friendly physical page_id")?),
        (1, Some(PhysicalQueryValue::Null)) => None,
        (1, _) => return Err("Friendly virtual page has a physical page_id".into()),
        _ => return Err("Friendly page candidate kind is not 0 or 1".into()),
    };
    if from_content && page_id.is_none() {
        return Err("a virtual page cannot match by content".into());
    }
    let kind_value = integer(row, 4, "pages.text_kind")?;
    let kind = page_kind_from_sql(kind_value)
        .ok_or_else(|| format!("pages.text_kind {kind_value} is not a page kind"))?;
    let journal_day = match row.get(5) {
        Some(PhysicalQueryValue::Null) => None,
        Some(PhysicalQueryValue::Integer(value)) => Some(*value),
        _ => return Err("pages.journal_day is not an integer or null".into()),
    };
    let matched_alias = match integer(row, 8, "Friendly page matched-alias flag")? {
        0 => false,
        1 if page_id.is_some() && !from_content => true,
        _ => return Err("Friendly page matched-alias flag is invalid".into()),
    };
    let rank_key = match row.get(9) {
        Some(PhysicalQueryValue::Blob(value)) => value.clone(),
        _ => return Err("Friendly page global rank is not a blob".into()),
    };
    let name = text(row, 3, "pages.name")?;
    let path = text(row, 6, "pages.path")?;
    if page_id.is_none() && (kind != PageKind::Page || journal_day.is_some() || !path.is_empty()) {
        return Err("Friendly virtual page carries physical metadata".into());
    }
    // A stored page with no producer row has nothing to hydrate against; the
    // hydrator validates the stored count and estimate, so an absent pair skips
    // the row rather than inventing one.
    let payload = if hydrate && page_id.is_some() {
        match (row.get(10), row.get(11)) {
            (Some(PhysicalQueryValue::Null), _) | (_, Some(PhysicalQueryValue::Null)) => None,
            _ => Some((
                count(row, 10, "pages.estimated_bytes")?,
                count(row, 11, "pages.property_count")?,
            )),
        }
    } else {
        None
    };
    let matched = text(row, 7, "Friendly matched page text")?;
    let (matched_text, matched_text_lower) = if from_content {
        let projection = crate::query::text::visible_projection_from_raw_path(&matched, &path);
        (projection.visible, Some(projection.visible_lower))
    } else {
        (matched, None)
    };
    Ok(PageDescriptor {
        from_content,
        page_id,
        name,
        kind,
        journal_day,
        path,
        matched_text,
        matched_text_lower,
        matched_alias,
        rank_key,
        payload,
    })
}

struct BlockDescriptor {
    block_id: i64,
    page_id: i64,
    parent_id: Option<i64>,
    page: String,
    kind: PageKind,
    path: String,
    visible: String,
    visible_lower: String,
    result_id: String,
    estimated_bytes: usize,
    tag_count: usize,
    property_count: usize,
    rank_key: Vec<u8>,
}

/// `blocks b`, or the trigram index driving it, for one block predicate.
///
/// ONE producer for both block reads a Friendly search performs: the Blocks
/// section's own statement, and the page-by-content membership CTE in
/// [`read_pages`], which ranks with the SAME program and therefore admits
/// exactly the same blocks. Before this existed only the first was bounded,
/// and one Ctrl-K keystroke still scanned every block in the graph — through
/// the other one.
///
/// The difference between driving and filtering is the whole point: as a
/// `WHERE` clause the index only skips the rank call, so SQLite still walks
/// every row; as the driving table it reads the candidates and nothing else.
struct BlockCandidateSource {
    sql: String,
    // Name the driving coordinate: ordering by the equal joined PK makes
    // SQLite sort the complete FTS posting before the Rust cursor can stop.
    recency: &'static str,
}

fn indexed_block_source(
    params: &mut Vec<PhysicalQueryValue>,
    predicate: &crate::query_plan::QueryExpr,
) -> BlockCandidateSource {
    match expression_plan(predicate) {
        CandidatePlan::Scan => BlockCandidateSource {
            sql: "blocks b".to_string(),
            recency: "b.block_id",
        },
        CandidatePlan::Index {
            table,
            match_expression,
        } => {
            params.push(PhysicalQueryValue::Text(match_expression));
            BlockCandidateSource {
                sql: format!(
                    "(SELECT rowid AS block_id FROM {table} \
                       WHERE {table} MATCH ?{} ORDER BY rowid DESC) c \
                     JOIN blocks b ON b.block_id = c.block_id",
                    params.len(),
                    table = table.name(),
                ),
                recency: "c.block_id",
            }
        }
    }
}

fn verified_id_block_source(params: &mut Vec<PhysicalQueryValue>, ids: &[i64]) -> String {
    if ids.is_empty() {
        return "(SELECT block_id FROM blocks WHERE 0) c \
                JOIN blocks b ON b.block_id = c.block_id"
            .to_string();
    }
    let mut placeholders = Vec::with_capacity(ids.len());
    for id in ids {
        params.push(PhysicalQueryValue::Integer(*id));
        placeholders.push(format!("?{}", params.len()));
    }
    format!(
        "(SELECT block_id FROM blocks WHERE block_id IN ({})) c \
         JOIN blocks b ON b.block_id = c.block_id",
        placeholders.join(", ")
    )
}

/// Bind the one established current-page membership rule for any block
/// candidate statement. The interactive cursor must apply it before counting
/// verified matches; applying it only to the later ranked winners lets newer
/// out-of-scope rows consume the whole window.
fn block_scope_conditions(params: &mut Vec<PhysicalQueryValue>, plan: &QueryPlan) -> Vec<String> {
    let mut conditions = Vec::new();
    match plan.page_scope() {
        Some(scope) if scope.path.is_some() => {
            params.push(PhysicalQueryValue::Text(scope.path.clone().unwrap()));
            conditions.push(format!("p.path = ?{}", params.len()));
        }
        Some(scope) => {
            params.push(PhysicalQueryValue::Integer(match scope.page_kind {
                PageKind::Page => 0,
                PageKind::Journal => 1,
            }));
            let kind = params.len();
            params.push(PhysicalQueryValue::Text(crate::refs::page_key(&scope.name)));
            conditions.push(format!(
                "p.text_kind = ?{kind} AND p_name.key = ?{}",
                params.len()
            ));
        }
        None => {}
    }
    conditions
}

fn interactive_block_cursor_statement(
    plan: &QueryPlan,
    branch: &QueryBranch,
) -> (String, Vec<PhysicalQueryValue>) {
    let mut params = Vec::new();
    let BlockCandidateSource {
        sql: source,
        recency,
    } = indexed_block_source(&mut params, &branch.predicate);
    let conditions = block_scope_conditions(&mut params, plan);
    let scope_sql = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    let sql = format!(
        "SELECT b.block_id, bt.content, p.path \
         FROM {source} \
         LEFT JOIN block_text bt ON bt.block_id = b.block_id \
         LEFT JOIN pages p ON p.page_id = b.page_id \
         LEFT JOIN names p_name ON p_name.name_id = p.name_id{scope_sql} \
         ORDER BY {recency} DESC"
    );
    (sql, params)
}

/// The verified candidates of one interactive read.
struct VerifiedIds {
    ids: Vec<i64>,
    /// An unindexed scan stopped at its [`interactive_scan_budget`] before the
    /// window filled: older blocks were not visited, so more may match.
    budget_spent: bool,
}

/// Consume rowid-descending candidates until `window` exact matches survive.
/// This is deliberately a Rust cursor: putting the exact callback in a
/// materialized SQL CTE evaluates the complete candidate set before LIMIT and
/// turns a verified-match window back into graph-sized work. A scan no index
/// drives also stops at its [`interactive_scan_budget`].
fn interactive_verified_block_ids(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    plan: &QueryPlan,
    branch: &QueryBranch,
    window: usize,
    lane: &Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> Result<VerifiedIds, ResultReadError> {
    let (sql, params) = interactive_block_cursor_statement(plan, branch);
    let budget = interactive_scan_budget(&branch.predicate);
    let mut ids = Vec::with_capacity(window);
    let mut visits = 0usize;
    let mut budget_spent = false;
    let mut damage = None;
    let cancellation = snapshot.cancellation();
    crate::query::projection_sql::visit(snapshot, &sql, &params, |row| {
        #[cfg(test)]
        note_friendly(|census| census.block_candidate_visits += 1);
        if budget.is_some_and(|budget| visits == budget) {
            budget_spent = true;
            return Ok(std::ops::ControlFlow::Break(()));
        }
        visits += 1;
        if lane.as_ref().is_some_and(|cancelled| cancelled()) {
            cancellation.cancel();
        }
        if cancellation.is_cancelled() {
            return Err(tine_storage::sqlite::MaterializationError::Incomplete(
                "Friendly candidate read cancelled".into(),
            ));
        }
        let block_id = match row.first() {
            Some(PhysicalQueryValue::Integer(id)) => *id,
            _ => {
                damage = Some("interactive candidate has no block coordinate".into());
                return Ok(std::ops::ControlFlow::Break(()));
            }
        };
        let raw = match row.get(1) {
            Some(PhysicalQueryValue::Text(raw)) => raw,
            _ => {
                damage = Some(format!(
                    "interactive candidate block {block_id} has no raw text"
                ));
                return Ok(std::ops::ControlFlow::Break(()));
            }
        };
        let path = match row.get(2) {
            Some(PhysicalQueryValue::Text(path)) => path,
            _ => {
                damage = Some(format!(
                    "interactive candidate block {block_id} has no page path"
                ));
                return Ok(std::ops::ControlFlow::Break(()));
            }
        };
        #[cfg(test)]
        note_friendly(|census| census.block_candidate_verifications += 1);
        let projection = crate::query::text::visible_projection_from_raw_path(raw, path);
        if rank_block_text_folded(plan, branch, &projection.visible, &projection.visible_lower)
            .is_some()
        {
            ids.push(block_id);
            if ids.len() == window {
                return Ok(std::ops::ControlFlow::Break(()));
            }
        }
        Ok(std::ops::ControlFlow::Continue(()))
    })
    .map_err(|error| sql_or_cancelled(snapshot, error))?;
    if let Some(message) = damage {
        return Err(ResultReadError::Corrupt(message));
    }
    crate::direct_projection::projection_diag(|| {
        format!(
            "interactive block candidates: visits={visits} verified={} indexed={} budget_spent={budget_spent}",
            ids.len(),
            budget.is_none()
        )
    });
    Ok(VerifiedIds { ids, budget_spent })
}

fn read_blocks(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    identity: &ResultIdentity,
    plan: &QueryPlan,
    branch: &QueryBranch,
    rank_program: u64,
    sort_program: Option<FriendlySortPrograms>,
    lane: &Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) -> Result<(Vec<QueryHit>, bool), ResultReadError> {
    if branch.limit == 0 {
        return Ok((Vec::new(), false));
    }
    let mut params = vec![PhysicalQueryValue::Integer(rank_program as i64)];
    let conditions = block_scope_conditions(&mut params, plan);
    // Drive the read from the trigram index instead of filtering a full scan
    // with it. The difference is the whole point: as a WHERE clause the index
    // only skips the rank call, so SQLite still scans every block in scope and
    // one keystroke still costs seconds; as the driving table it reads the
    // candidates and nothing else.
    //
    // Soundness is `block_candidate_needle`'s: every block the exact predicate
    // admits contains the needle, so no admitted block is missing from the
    // candidate set. `rank_key IS NOT NULL` below and the Rust re-rank after
    // the read remain the exact predicate on both paths, so a needle that is
    // wrong shows up as a MISSING result, never as a wrong one.
    //
    // Candidate narrowing never weakens the damage contract. The streamed
    // interactive cursor LEFT-joins and validates text/page rows before it
    // counts W; this ranked statement likewise keeps its LEFT joins and orders
    // `missing_text` first so descriptor decoding fails the whole read.
    let mut budget_spent = false;
    let block_source = match plan.candidate_mode() {
        CandidateMode::Exhaustive => indexed_block_source(&mut params, &branch.predicate).sql,
        CandidateMode::Interactive { window } => {
            let verified = interactive_verified_block_ids(snapshot, plan, branch, window, lane)?;
            budget_spent = verified.budget_spent;
            verified_id_block_source(&mut params, &verified.ids)
        }
    };
    let scope_sql = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    // The Blocks section's own authored sort, over the COMPLETE matched set:
    // the bound below is applied after this ORDER BY, never before it.
    let authored = sort_terms(
        plan.block_view(),
        sort_program,
        &mut params,
        |field, binder| block_sort_expression(field, "r", "r", "", binder),
    );
    let order = if authored.is_empty() {
        format!(
            "substr(r.rank_key, 1, {}), r.path COLLATE BINARY, r.preorder",
            crate::query_plan::BLOCK_RANK_KEY_LEN
        )
    } else {
        format!("{}, r.path COLLATE BINARY, r.preorder", authored.join(", "))
    };
    let limit = limit_clause(branch.limit, &mut params);
    let framed_block_text = crate::query::text::framed_pair_sql("bt.content", "p.path");
    let verified_window = String::new();
    let ranked_hint = " MATERIALIZED";
    let sql = format!(
        "WITH ranked AS{ranked_hint} (\
             SELECT b.block_id, b.page_id, b.parent_block_id, b.order_key, \
                    bt.content AS raw_content, p_name.raw AS name, p.text_kind, p.path, \
                    b.page_id AS result_page_id, b.preorder, b.result_id, b.estimated_bytes, b.tag_count, \
                    b.property_count, \
                    CASE WHEN bt.block_id IS NULL OR p.page_id IS NULL THEN zeroblob(1) \
                         ELSE tine_query_rank(?1, {framed_block_text}) END AS rank_key, \
                    CASE WHEN bt.block_id IS NULL OR p.page_id IS NULL THEN 1 ELSE 0 END AS missing_text \
             FROM {block_source} \
             LEFT JOIN block_text bt ON bt.block_id = b.block_id \
             LEFT JOIN pages p ON p.page_id = b.page_id \
             LEFT JOIN names p_name ON p_name.name_id = p.name_id{scope_sql}\
         ), verified AS MATERIALIZED (\
             SELECT * FROM ranked r WHERE r.missing_text = 1 OR r.rank_key IS NOT NULL{verified_window}\
         ) \
         SELECT r.block_id, r.page_id, r.parent_block_id, r.order_key, r.raw_content, \
                r.name, r.text_kind, r.path, r.result_page_id, r.preorder, r.result_id, \
                r.estimated_bytes, r.tag_count, r.property_count, \
                substr(r.rank_key, 1, {rank_len}) \
         FROM verified r \
         ORDER BY r.missing_text DESC, {order}{limit}"
        , rank_len = crate::query_plan::BLOCK_RANK_KEY_LEN
    );
    let rows = crate::query::projection_sql::run(snapshot, &sql, &params)
        .map_err(|error| sql_or_cancelled(snapshot, error))?;
    #[cfg(test)]
    note_friendly(|census| census.block_descriptors += rows.len());
    let mut descriptors = rows
        .iter()
        .map(|row| decode_block_descriptor(row, identity))
        .collect::<Result<Vec<_>, _>>()
        .map_err(ResultReadError::Corrupt)?;
    let has_more = descriptors.len() > branch.limit || budget_spent;
    descriptors.truncate(branch.limit);
    let mut seen = HashSet::new();
    for descriptor in &descriptors {
        check_lane(snapshot, lane)?;
        if !seen.insert(descriptor.block_id) {
            return Err(ResultReadError::Corrupt(
                "one physical block appears twice in Friendly results".into(),
            ));
        }
        let expected =
            rank_block_text_folded(plan, branch, &descriptor.visible, &descriptor.visible_lower)
                .ok_or_else(|| {
                    ResultReadError::Corrupt(
                        "a selected block no longer satisfies its rank program".into(),
                    )
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
        let rank =
            rank_block_text_folded(plan, branch, &descriptor.visible, &descriptor.visible_lower)
                .ok_or_else(|| {
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
    let block_id = integer(row, 0, "blocks.block_id")?;
    let page_id = integer(row, 1, "blocks.page_id")?;
    let parent_id = optional_integer(row, 2, "blocks.parent_block_id")?;
    let order_key = text(row, 3, "blocks.order_key")?;
    let raw = text(row, 4, "block_text.content")?;
    let page = text(row, 5, "pages.name")?;
    let kind_value = integer(row, 6, "pages.text_kind")?;
    let kind = page_kind_from_sql(kind_value)
        .ok_or_else(|| format!("pages.text_kind {kind_value} is not a page kind"))?;
    let path = text(row, 7, "pages.path")?;
    let projection = crate::query::text::visible_projection_from_raw_path(&raw, &path);
    let result_page = integer(row, 8, "blocks.page_id")?;
    if result_page != page_id {
        return Err("blocks.page_id does not own its block's page".into());
    }
    let preorder = integer(row, 9, "blocks.preorder")?;
    if preorder < 0 {
        return Err("blocks.preorder is negative".into());
    }
    let stored_id = text(row, 10, "blocks.result_id")?;
    if stored_id.is_empty() {
        return Err("blocks.result_id is empty".into());
    }
    let stored_estimate = count(row, 11, "blocks.estimated_bytes")?;
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
        visible: projection.visible,
        visible_lower: projection.visible_lower,
        result_id,
        estimated_bytes,
        tag_count: count(row, 12, "blocks.tag_count")?,
        property_count: count(row, 13, "blocks.property_count")?,
        rank_key,
    })
}

fn optional_integer(
    row: &[PhysicalQueryValue],
    at: usize,
    what: &str,
) -> Result<Option<i64>, String> {
    match row.get(at) {
        Some(PhysicalQueryValue::Null) => Ok(None),
        Some(PhysicalQueryValue::Integer(value)) => Ok(Some(*value)),
        _ => Err(format!("{what} is not an integer or null")),
    }
}

#[derive(Clone)]
struct Ancestor {
    page_id: i64,
    parent_id: Option<i64>,
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
                .map(|id| PhysicalQueryValue::Integer(*id))
                .collect::<Vec<_>>();
            let sql = format!(
                "SELECT b.block_id, b.page_id, b.parent_block_id, bt.content, p.path \
                 FROM blocks b LEFT JOIN block_text bt ON bt.block_id = b.block_id \
                 LEFT JOIN pages p ON p.page_id = b.page_id \
                 WHERE b.block_id IN ({})",
                crate::query::results::placeholders(params.len())
            );
            let rows = crate::query::projection_sql::run(snapshot, &sql, &params)
                .map_err(|error| sql_or_cancelled(snapshot, error))?;
            #[cfg(test)]
            note_friendly(|census| {
                census.ancestor_statements += 1;
                census.ancestor_rows += rows.len();
            });
            for row in &rows {
                if row.len() != 5 {
                    return Err(ResultReadError::Corrupt(format!(
                        "Friendly ancestor row has {} columns, expected 5",
                        row.len()
                    )));
                }
                let id = integer(row, 0, "ancestor blocks.block_id")
                    .map_err(ResultReadError::Corrupt)?;
                if !requested.contains(&id) {
                    return Err(ResultReadError::Corrupt(
                        "an ancestor row belongs to no admitted chain".into(),
                    ));
                }
                let ancestor = Ancestor {
                    page_id: integer(row, 1, "ancestor blocks.page_id")
                        .map_err(ResultReadError::Corrupt)?,
                    parent_id: optional_integer(row, 2, "ancestor blocks.parent_block_id")
                        .map_err(ResultReadError::Corrupt)?,
                    visible: crate::query::text::visible_from_raw_path(
                        &text(row, 3, "ancestor block_text.content")
                            .map_err(ResultReadError::Corrupt)?,
                        &text(row, 4, "ancestor pages.path").map_err(ResultReadError::Corrupt)?,
                    ),
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
