//! Typed execution plan for friendly graph search.
//!
//! This module is deliberately narrower than the full `{{query}}` DSL today:
//! it unifies the two graph-backed parts of Ctrl-K (page names and block text)
//! without changing the command/create-page providers or the established
//! block-query result contract.  The plan/result types are the seam that a
//! durable query workspace can grow into later.

#![cfg_attr(test, allow(private_bounds))]

use crate::doc::DocBlock;
#[cfg(test)]
use crate::model::Graph;
#[cfg(test)]
use crate::query::graph::QueryGraph;
use crate::refs;
use crate::search_query::{canonical_fold, canonical_fold_with_map, Matcher, Term};
use crate::vocab::{BlockDto, PageEntry, PageKind};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

const MAX_EVIDENCE_SPANS: usize = 32;

// How many times the shared block evaluator has produced match evidence and a
// result DTO. The architectural claim is that this is once per WINNER, not
// once per retained candidate -- O(limit), not O(retained) -- and a counter is
// the only way to state that as a test rather than as a comment.
#[cfg(test)]
thread_local! {
    static BLOCK_EVIDENCE_EVALUATIONS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    // Count at the span-producing matcher, not merely at a public wrapper.
    static TEXT_EVIDENCE_EVALUATIONS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn take_block_evidence_evaluations() -> usize {
    BLOCK_EVIDENCE_EVALUATIONS.with(|count| count.replace(0))
}

/// The entity kind a query-plan branch selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryTarget {
    Pages,
    Blocks,
}

/// Text field tested by a text predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextField {
    PageName,
    VisibleContent,
}

/// Matching is explicit in the plan.  In particular, a fuzzy page-name match
/// never makes block-content predicates fuzzy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextMatchMode {
    Contains,
    Phrase,
    Regex,
    Fuzzy,
}

/// Explainable objective relevance. Variant order is deliberately not used for
/// ranking; `rank()` below is the single ordering contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectiveMatchClass {
    Exact,
    Prefix,
    Substring,
    Fuzzy,
    BodyEvidence,
}

impl ObjectiveMatchClass {
    fn rank(self) -> i32 {
        match self {
            Self::Exact => 5,
            Self::Prefix => 4,
            Self::Substring => 3,
            Self::Fuzzy => 2,
            Self::BodyEvidence => 1,
        }
    }
}

/// Browser-facing offsets are UTF-16 code-unit offsets (the unit used by JS
/// string slicing and DOM selection), not Rust/regex byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchSpan {
    pub start: usize,
    pub end: usize,
}

/// One positive clause's reason for accepting an entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchEvidence {
    pub clause_id: u32,
    pub field: TextField,
    pub mode: TextMatchMode,
    pub spans: Vec<MatchSpan>,
    /// Predicate-local relevance.  Only fuzzy predicates currently populate it;
    /// final page ranking is carried on the page hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<i32>,
}

/// A typed text predicate in the shared plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextPredicate {
    pub clause_id: u32,
    pub field: TextField,
    pub mode: TextMatchMode,
    pub value: String,
}

/// Boolean query expression.  A successful NOT contributes no positive match
/// evidence, which keeps "why did this match?" explanations honest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum QueryExpr {
    Text(TextPredicate),
    And(Vec<QueryExpr>),
    Or(Vec<QueryExpr>),
    Not(Box<QueryExpr>),
    Never,
}

/// One independently limited entity branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryBranch {
    pub target: QueryTarget,
    pub predicate: QueryExpr,
    pub limit: usize,
}

/// The product surface asking Friendly search to execute. Cancellation lanes
/// are deliberately absent: supersession and candidate semantics are separate
/// facts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FriendlyConsumer {
    #[default]
    NonInteractive,
    CtrlK,
}

impl FriendlyConsumer {
    pub(crate) const fn candidate_mode(self) -> crate::query::candidate::CandidateMode {
        match self {
            Self::NonInteractive => crate::query::candidate::CandidateMode::Exhaustive,
            Self::CtrlK => crate::query::candidate::CandidateMode::interactive(),
        }
    }
}

impl QueryPlan {
    /// Captured current-page scope for projection-backed Friendly execution.
    /// A physical path, when present, remains authoritative over display name.
    pub(crate) fn page_scope(&self) -> Option<&QueryPageScope> {
        self.page_scope.as_ref()
    }

    /// The membership source this search asks for, with absence resolved to the
    /// historic names-and-aliases behaviour. One resolution, at the boundary
    /// the request crosses, so no reader can pick a different default.
    pub(crate) fn page_match_scope(&self) -> crate::query::ir::FriendlyPageMatchScope {
        self.display
            .page_match_scope
            .unwrap_or(crate::query::ir::FriendlyPageMatchScope::Names)
    }

    /// The already-resolved effective view of the Pages section, present only
    /// where the caller enabled Display for this search.
    pub(crate) fn page_view(&self) -> Option<&crate::query::ir::ViewSettings> {
        self.display.page_view.as_ref()
    }

    /// The already-resolved effective view of the Blocks section.
    pub(crate) fn block_view(&self) -> Option<&crate::query::ir::ViewSettings> {
        self.display.block_view.as_ref()
    }
}

/// One routed page used to scope a block-search plan. A supplied relative path
/// is authoritative so duplicate display identities do not leak into results;
/// otherwise kind plus Logseq's canonical page identity selects the document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryPageScope {
    pub name: String,
    pub page_kind: PageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Stable diagnostic codes let the frontend localize/rephrase messages later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryDiagnostic {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<MatchSpan>,
}

/// A cheap declarative explanation tree.  Per-candidate counts/timings can be
/// layered on later without changing query membership or match evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainNode {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clause_id: Option<u32>,
    pub description: String,
    pub children: Vec<ExplainNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryExplanation {
    pub branches: Vec<ExplainNode>,
}

/// Result-only entity union.  Match metadata intentionally does not live on
/// `BlockDto`, because that DTO also crosses the write boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "entity", rename_all = "snake_case")]
pub enum QueryHit {
    Page {
        page: PageEntry,
        display_text: String,
        evidence: Vec<MatchEvidence>,
        score: i32,
        match_class: ObjectiveMatchClass,
        #[serde(skip_serializing_if = "Option::is_none")]
        matched_alias: Option<String>,
        /// The hydrated page row, present exactly when this hit came from the
        /// Display-enabled Friendly path AND names a stored page.
        ///
        /// It is ADDITIVE: `page`, `display_text`, `evidence`, `score`,
        /// `match_class` and `matched_alias` keep their meanings, so the
        /// switcher, the block picker and every other navigation consumer read
        /// exactly what they read before. A virtual reference-name suggestion
        /// names no stored page, so it carries no row and consumes no
        /// hydration budget — fabricating properties for one would be a page
        /// that does not exist claiming to have them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        row: Option<crate::query::ir::PageRow>,
    },
    Block {
        page: String,
        kind: PageKind,
        /// Graph-root-relative physical owner of this result. Block ids and page
        /// names are not unique enough to recover it after a duplicate-name hit.
        path: String,
        block: BlockDto,
        /// Exact lsdoc-projected visible text indexed by `evidence.spans`.
        display_text: String,
        evidence: Vec<MatchEvidence>,
        /// Objective block relevance. The match class is the primary band;
        /// this score summarizes boundary, offset, length, and occurrence
        /// quality inside that band.
        score: i32,
        match_class: ObjectiveMatchClass,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueryHasMore {
    #[serde(default)]
    pub pages: bool,
    #[serde(default)]
    pub blocks: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryExecution {
    pub hits: Vec<QueryHit>,
    pub diagnostics: Vec<QueryDiagnostic>,
    pub explanation: QueryExplanation,
    /// Per-category top-k truncation, detected during the existing candidate scan.
    #[serde(default)]
    pub has_more: QueryHasMore,
    /// A cancelled latest-wins lane returns no partial results.
    pub cancelled: bool,
}

/// **The Display facts a Friendly search runs under** (SPEC §7.6, Q3).
///
/// These are operation INPUT, exactly like [`QueryPageScope`] beside them: the
/// caller has already resolved inheritance (an absent scoped draft against the
/// singular settings) before it gets here, so the reader never re-inherits a
/// missing member. Absence at this boundary therefore means "this consumer
/// stated nothing", not "look somewhere else".
///
/// They ride on the plan rather than on a parallel input struct because
/// `page_scope` — the other per-operation Friendly input — already does, and a
/// second carriage for the same class of fact is the fork this campaign exists
/// to remove (D-14).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FriendlyDisplayOptions {
    /// Which source of page membership this search asks for. `None` is the
    /// historic names-and-aliases behaviour, resolved HERE and nowhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_match_scope: Option<crate::query::ir::FriendlyPageMatchScope>,
    /// The already-resolved effective view of the Pages section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_view: Option<crate::query::ir::ViewSettings>,
    /// The already-resolved effective view of the Blocks section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_view: Option<crate::query::ir::ViewSettings>,
}

impl FriendlyDisplayOptions {
    /// The row bound one family actually requests: its own `sample`, never
    /// larger than the bound its consumer already had. `sample: 0` empties that
    /// family and only that family; an absent sample leaves the consumer's
    /// bound alone. No unused capacity transfers between the two.
    fn admitted(view: Option<&crate::query::ir::ViewSettings>, limit: usize) -> usize {
        match view.and_then(|view| view.sample) {
            Some(sample) => (sample as usize).min(limit),
            None => limit,
        }
    }
}

/// **The one Friendly graph-search plan builder** (I-12, D-4).
///
/// Every public Friendly route — Direct's `run_graph_search*` — asks THIS
/// function which plan a
/// `(source, limits, routed scope, Display)` request means. Each used to spell
/// the same `match scope { … }` itself, and a routed search that forgot to
/// carry a section's sample or membership scope would have been a difference
/// between backends nobody could see from either side (I-19).
pub fn friendly_search_plan(
    source: &str,
    page_limit: usize,
    block_limit: usize,
    scope: Option<QueryPageScope>,
    display: FriendlyDisplayOptions,
) -> QueryPlan {
    friendly_search_plan_for(
        source,
        page_limit,
        block_limit,
        scope,
        display,
        FriendlyConsumer::NonInteractive,
    )
}

pub fn friendly_search_plan_for(
    source: &str,
    page_limit: usize,
    block_limit: usize,
    scope: Option<QueryPageScope>,
    display: FriendlyDisplayOptions,
    consumer: FriendlyConsumer,
) -> QueryPlan {
    let mut plan = match scope {
        // A routed search selects blocks inside ONE physical page, so it has no
        // Pages section and page membership scope does not apply to it.
        Some(scope) => {
            QueryPlan::friendly_for_page_with_display(source, block_limit, scope, display)
        }
        None => QueryPlan::friendly_with_display(source, page_limit, block_limit, display),
    };
    plan.candidate_mode = consumer.candidate_mode();
    plan
}

/// Compiled friendly graph-search plan.  Regexes are compiled once and kept off
/// the wire; the public expression remains inspectable/serializable.
#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub branches: Vec<QueryBranch>,
    pub diagnostics: Vec<QueryDiagnostic>,
    page_scope: Option<QueryPageScope>,
    display: FriendlyDisplayOptions,
    // Ctrl-K keeps the literal trimmed launcher source so a multi-word page
    // title/alias can retain the objective search Exact class. The parsed AND
    // terms alone would otherwise downgrade `Foo Bar` to Prefix/Substring.
    // This is search relevance, not page identity; Create suppression compares
    // frontend pageIdentityKey values at its own boundary.
    page_exact: Option<String>,
    regexes: HashMap<u32, Regex>,
    candidate_mode: crate::query::candidate::CandidateMode,
    // Navigation/autocomplete exposes every matching authored spelling for an
    // owner; ordinary Friendly search keeps one coherent winner per owner.
    page_name_suggestions: bool,
}

impl QueryPlan {
    /// Ctrl-K graph providers: fuzzy page names for a single bare term, but
    /// ordinary contains/phrase/regex semantics for block content.  Multi-term
    /// and operator searches use the same boolean grammar on both entity kinds.
    pub fn friendly(query: &str, page_limit: usize, block_limit: usize) -> Self {
        Self::friendly_with_display(
            query,
            page_limit,
            block_limit,
            FriendlyDisplayOptions::default(),
        )
    }

    /// The same plan, under stated Display settings. Each family's `sample`
    /// reduces ITS OWN requested rows before selection, so the two sections are
    /// independently bounded and neither can spend the other's capacity.
    pub fn friendly_with_display(
        query: &str,
        page_limit: usize,
        block_limit: usize,
        display: FriendlyDisplayOptions,
    ) -> Self {
        let page_limit = FriendlyDisplayOptions::admitted(display.page_view.as_ref(), page_limit);
        let block_limit =
            FriendlyDisplayOptions::admitted(display.block_view.as_ref(), block_limit);
        let mut plan = Self::friendly_plan(query, page_limit, block_limit);
        plan.display = display;
        plan
    }

    fn friendly_plan(query: &str, page_limit: usize, block_limit: usize) -> Self {
        let matcher = Matcher::parse(query);
        let mut next_id = 1;
        let mut regexes = HashMap::new();
        let mut diagnostics = Vec::new();
        if let Matcher::InvalidRegex(error) = &matcher {
            diagnostics.push(QueryDiagnostic {
                code: "invalid_regex".into(),
                message: error.clone(),
                span: Some(MatchSpan {
                    start: 0,
                    end: query.encode_utf16().count(),
                }),
            });
        }

        let mut branches = Vec::new();
        if !matches!(matcher, Matcher::Empty | Matcher::InvalidRegex(_)) {
            let page_expr = if let Some(term) = matcher.simple_term() {
                let id = take_id(&mut next_id);
                QueryExpr::Text(TextPredicate {
                    clause_id: id,
                    field: TextField::PageName,
                    mode: TextMatchMode::Fuzzy,
                    value: term.to_string(),
                })
            } else {
                expr_from_matcher(&matcher, TextField::PageName, &mut next_id, &mut regexes)
            };
            branches.push(QueryBranch {
                target: QueryTarget::Pages,
                predicate: page_expr,
                limit: page_limit,
            });
            branches.push(QueryBranch {
                target: QueryTarget::Blocks,
                predicate: expr_from_matcher(
                    &matcher,
                    TextField::VisibleContent,
                    &mut next_id,
                    &mut regexes,
                ),
                limit: block_limit,
            });
        }
        Self {
            branches,
            diagnostics,
            page_scope: None,
            display: FriendlyDisplayOptions::default(),
            page_exact: (!query.trim().is_empty()).then(|| canonical_fold(query.trim())),
            regexes,
            candidate_mode: crate::query::candidate::CandidateMode::Exhaustive,
            page_name_suggestions: false,
        }
    }

    /// Current-page search is a block-only execution profile of the same typed
    /// friendly plan—not a frontend filter over whole-graph results.
    pub fn friendly_for_page(query: &str, block_limit: usize, scope: QueryPageScope) -> Self {
        Self::friendly_for_page_with_display(
            query,
            block_limit,
            scope,
            FriendlyDisplayOptions::default(),
        )
    }

    /// Routed-page search under stated Display settings. Page membership scope
    /// is not consulted: this profile selects blocks inside one physical page,
    /// so there is no Pages section for it to describe.
    pub fn friendly_for_page_with_display(
        query: &str,
        block_limit: usize,
        scope: QueryPageScope,
        display: FriendlyDisplayOptions,
    ) -> Self {
        let block_limit =
            FriendlyDisplayOptions::admitted(display.block_view.as_ref(), block_limit);
        let mut plan = Self::block_search(query, block_limit);
        plan.page_scope = Some(scope);
        plan.display = display;
        plan
    }

    /// Explicit page-name fuzzy plan for normal-query frontends and tests.  This
    /// constructor makes the opt-in visible in the typed IR; it never changes the
    /// default behavior of existing block queries.
    pub fn page_name_fuzzy(value: impl Into<String>, limit: usize) -> Self {
        let value = value.into();
        let folded = canonical_fold(&value);
        // The legacy picker intentionally lists every page for a truly empty
        // raw query. A nonempty query erased by A6 is a different state and
        // must never become that match-all sentinel.
        let predicate = if !value.is_empty() && folded.is_empty() {
            QueryExpr::Never
        } else {
            QueryExpr::Text(TextPredicate {
                clause_id: 1,
                field: TextField::PageName,
                mode: TextMatchMode::Fuzzy,
                value: folded,
            })
        };
        Self {
            branches: vec![QueryBranch {
                target: QueryTarget::Pages,
                predicate,
                limit,
            }],
            diagnostics: Vec::new(),
            page_scope: None,
            display: FriendlyDisplayOptions::default(),
            page_exact: None,
            regexes: HashMap::new(),
            candidate_mode: crate::query::candidate::CandidateMode::Exhaustive,
            page_name_suggestions: true,
        }
    }

    /// Existing block-search API expressed as one typed branch.
    pub fn block_search(query: &str, limit: usize) -> Self {
        let matcher = Matcher::parse(query);
        let mut next_id = 1;
        let mut regexes = HashMap::new();
        let mut diagnostics = Vec::new();
        if let Matcher::InvalidRegex(error) = &matcher {
            diagnostics.push(QueryDiagnostic {
                code: "invalid_regex".into(),
                message: error.clone(),
                span: Some(MatchSpan {
                    start: 0,
                    end: query.encode_utf16().count(),
                }),
            });
        }
        let branches = if matches!(matcher, Matcher::Empty | Matcher::InvalidRegex(_)) {
            Vec::new()
        } else {
            vec![QueryBranch {
                target: QueryTarget::Blocks,
                predicate: expr_from_matcher(
                    &matcher,
                    TextField::VisibleContent,
                    &mut next_id,
                    &mut regexes,
                ),
                limit,
            }]
        };
        Self {
            branches,
            diagnostics,
            page_scope: None,
            display: FriendlyDisplayOptions::default(),
            page_exact: None,
            regexes,
            candidate_mode: crate::query::candidate::CandidateMode::Exhaustive,
            page_name_suggestions: false,
        }
    }

    /// Literal block autocomplete for the `((` picker. Whitespace separates
    /// required fragments; every other character remains literal and fragment
    /// order is irrelevant. Blank input has no candidates.
    pub fn block_search_literal(query: &str, limit: usize) -> Self {
        let folded_fragments = query
            .split_whitespace()
            .map(canonical_fold)
            .filter(|fragment| !fragment.is_empty())
            .collect::<Vec<_>>();
        let branches = if query.is_empty() || folded_fragments.is_empty() {
            Vec::new()
        } else {
            let mut next_id = 1;
            let mut fragments = folded_fragments
                .into_iter()
                .map(|value| {
                    let clause_id = take_id(&mut next_id);
                    QueryExpr::Text(TextPredicate {
                        clause_id,
                        field: TextField::VisibleContent,
                        mode: TextMatchMode::Contains,
                        value,
                    })
                })
                .collect::<Vec<_>>();
            vec![QueryBranch {
                target: QueryTarget::Blocks,
                predicate: if fragments.len() == 1 {
                    fragments.pop().unwrap()
                } else {
                    QueryExpr::And(fragments)
                },
                limit,
            }]
        };
        Self {
            branches,
            diagnostics: Vec::new(),
            page_scope: None,
            display: FriendlyDisplayOptions::default(),
            page_exact: None,
            regexes: HashMap::new(),
            candidate_mode: crate::query::candidate::CandidateMode::interactive(),
            page_name_suggestions: false,
        }
    }

    /// Literal autocomplete for `#`/`[[`. OG rev 6e7afa8eb routes
    /// `handler/editor.cljs:get-matched-pages` through
    /// `search.cljs:page-search`/`exact-matched?`; the whole normalized query is
    /// one ordered-subsequence term, not Ctrl-K's AND/OR/negation/regex DSL.
    /// Blank input therefore keeps the established all-pages candidate listing.
    pub fn legacy_page_search(query: &str, limit: usize) -> Self {
        Self::page_name_fuzzy(query, limit)
    }

    pub(crate) const fn candidate_mode(&self) -> crate::query::candidate::CandidateMode {
        self.candidate_mode
    }

    pub(crate) const fn page_name_suggestions(&self) -> bool {
        self.page_name_suggestions
    }

    pub fn explanation(&self) -> QueryExplanation {
        QueryExplanation {
            branches: self
                .branches
                .iter()
                .map(|branch| ExplainNode {
                    clause_id: None,
                    description: match branch.target {
                        QueryTarget::Pages => "Page names".into(),
                        QueryTarget::Blocks => "Block text".into(),
                    },
                    children: vec![explain_expr(&branch.predicate)],
                })
                .collect(),
        }
    }

    /// Execute all graph-backed branches.  Cancellation is checked between page
    /// candidates and before every block projection; no partial result escapes.
    #[cfg(test)]
    pub fn execute<G: QueryGraph>(
        &self,
        graph: &G,
        cancelled: impl Fn() -> bool,
    ) -> QueryExecution {
        self.execute_with_explain(graph, cancelled, true)
    }

    #[cfg(test)]
    pub fn execute_with_explain<G: QueryGraph>(
        &self,
        graph: &G,
        cancelled: impl Fn() -> bool,
        explain: bool,
    ) -> QueryExecution {
        let explanation = if explain {
            self.explanation()
        } else {
            QueryExplanation {
                branches: Vec::new(),
            }
        };
        if !self.diagnostics.is_empty() {
            return QueryExecution {
                hits: Vec::new(),
                diagnostics: self.diagnostics.clone(),
                explanation,
                has_more: QueryHasMore::default(),
                cancelled: false,
            };
        }
        let mut hits = Vec::new();
        let mut has_more = QueryHasMore::default();
        for branch in &self.branches {
            if cancelled() {
                return cancelled_execution(self, explanation);
            }
            let branch_hits = match branch.target {
                QueryTarget::Pages => execute_pages(self, graph, branch, &cancelled),
                QueryTarget::Blocks => execute_blocks(self, graph, branch, &cancelled),
            };
            let Some((mut branch_hits, branch_has_more)) = branch_hits else {
                return cancelled_execution(self, explanation);
            };
            match branch.target {
                QueryTarget::Pages => has_more.pages |= branch_has_more,
                QueryTarget::Blocks => has_more.blocks |= branch_has_more,
            }
            hits.append(&mut branch_hits);
        }
        QueryExecution {
            hits,
            diagnostics: self.diagnostics.clone(),
            explanation,
            has_more,
            cancelled: false,
        }
    }
}

fn cancelled_execution(plan: &QueryPlan, explanation: QueryExplanation) -> QueryExecution {
    QueryExecution {
        hits: Vec::new(),
        diagnostics: plan.diagnostics.clone(),
        explanation,
        has_more: QueryHasMore::default(),
        cancelled: true,
    }
}

fn take_id(next: &mut u32) -> u32 {
    let id = *next;
    *next = next.saturating_add(1);
    id
}

fn expr_from_matcher(
    matcher: &Matcher,
    field: TextField,
    next_id: &mut u32,
    regexes: &mut HashMap<u32, Regex>,
) -> QueryExpr {
    match matcher {
        Matcher::Regex(re) => {
            let id = take_id(next_id);
            regexes.insert(id, re.clone());
            QueryExpr::Text(TextPredicate {
                clause_id: id,
                field,
                mode: TextMatchMode::Regex,
                value: re.as_str().to_string(),
            })
        }
        Matcher::Boolean(groups) => {
            let groups = groups
                .iter()
                .map(|group| {
                    let terms = group
                        .iter()
                        .map(|term| expr_from_term(term, field, next_id))
                        .collect::<Vec<_>>();
                    if terms.len() == 1 {
                        terms.into_iter().next().unwrap()
                    } else {
                        QueryExpr::And(terms)
                    }
                })
                .collect::<Vec<_>>();
            if groups.len() == 1 {
                groups.into_iter().next().unwrap()
            } else {
                QueryExpr::Or(groups)
            }
        }
        Matcher::InvalidRegex(_) | Matcher::Empty => QueryExpr::Never,
    }
}

fn expr_from_term(term: &Term, field: TextField, next_id: &mut u32) -> QueryExpr {
    let text = if term.text.is_empty() {
        // Transcribe Matcher::group_matches: a folded-empty positive is false,
        // while negation of that same predicate is true.
        QueryExpr::Never
    } else {
        QueryExpr::Text(TextPredicate {
            clause_id: take_id(next_id),
            field,
            mode: if term.quoted {
                TextMatchMode::Phrase
            } else {
                TextMatchMode::Contains
            },
            value: term.text.clone(),
        })
    };
    if term.negated {
        QueryExpr::Not(Box::new(text))
    } else {
        text
    }
}

fn explain_expr(expr: &QueryExpr) -> ExplainNode {
    match expr {
        QueryExpr::Text(p) => ExplainNode {
            clause_id: Some(p.clause_id),
            description: match p.mode {
                TextMatchMode::Contains => format!("contains “{}”", p.value),
                TextMatchMode::Phrase => format!("contains the exact phrase “{}”", p.value),
                TextMatchMode::Regex => format!("matches /{}/", p.value),
                TextMatchMode::Fuzzy => format!("fuzzily matches “{}”", p.value),
            },
            children: Vec::new(),
        },
        QueryExpr::And(children) => ExplainNode {
            clause_id: None,
            description: "All of".into(),
            children: children.iter().map(explain_expr).collect(),
        },
        QueryExpr::Or(children) => ExplainNode {
            clause_id: None,
            description: "Any of".into(),
            children: children.iter().map(explain_expr).collect(),
        },
        QueryExpr::Not(child) => ExplainNode {
            clause_id: None,
            description: "Exclude".into(),
            children: vec![explain_expr(child)],
        },
        QueryExpr::Never => ExplainNode {
            clause_id: None,
            description: "matches nothing".into(),
            children: Vec::new(),
        },
    }
}

#[derive(Debug)]
struct EvalMatch {
    evidence: Vec<MatchEvidence>,
}

fn eval_expr(
    plan: &QueryPlan,
    expr: &QueryExpr,
    expected_field: TextField,
    original: &str,
) -> Option<EvalMatch> {
    match expr {
        QueryExpr::Never => None,
        QueryExpr::Text(pred) => {
            if pred.field != expected_field {
                return None;
            }
            match_text(plan, pred, original).map(|evidence| EvalMatch {
                evidence: vec![evidence],
            })
        }
        QueryExpr::And(children) => {
            let mut evidence = Vec::new();
            for child in children {
                let matched = eval_expr(plan, child, expected_field, original)?;
                evidence.extend(matched.evidence);
            }
            Some(EvalMatch { evidence })
        }
        QueryExpr::Or(children) => children
            .iter()
            .find_map(|child| eval_expr(plan, child, expected_field, original)),
        QueryExpr::Not(child) => eval_expr(plan, child, expected_field, original)
            .is_none()
            .then_some(EvalMatch {
                evidence: Vec::new(),
            }),
    }
}

/// Membership-only hot path. Search scans use the lsdoc projection's cached
/// lowercase text and compute UTF-16 evidence only for bounded winning hits.
fn eval_expr_fast(
    plan: &QueryPlan,
    expr: &QueryExpr,
    expected_field: TextField,
    original: &str,
    lower: &str,
) -> bool {
    match expr {
        QueryExpr::Never => false,
        QueryExpr::Text(pred) if pred.field != expected_field => false,
        QueryExpr::Text(pred) => match pred.mode {
            TextMatchMode::Contains | TextMatchMode::Phrase => {
                !pred.value.is_empty() && lower.contains(&pred.value)
            }
            TextMatchMode::Regex => plan
                .regexes
                .get(&pred.clause_id)
                .is_some_and(|regex| regex.is_match(original)),
            TextMatchMode::Fuzzy => fuzzy_name_score(lower, &pred.value).is_some(),
        },
        QueryExpr::And(children) => children
            .iter()
            .all(|child| eval_expr_fast(plan, child, expected_field, original, lower)),
        QueryExpr::Or(children) => children
            .iter()
            .any(|child| eval_expr_fast(plan, child, expected_field, original, lower)),
        QueryExpr::Not(child) => !eval_expr_fast(plan, child, expected_field, original, lower),
    }
}

fn match_text(plan: &QueryPlan, pred: &TextPredicate, original: &str) -> Option<MatchEvidence> {
    #[cfg(test)]
    TEXT_EVIDENCE_EVALUATIONS.with(|count| count.set(count.get().saturating_add(1)));
    match pred.mode {
        TextMatchMode::Contains | TextMatchMode::Phrase => {
            let spans = folded_substring_spans(original, &pred.value);
            (!spans.is_empty()).then_some(MatchEvidence {
                clause_id: pred.clause_id,
                field: pred.field,
                mode: pred.mode,
                spans,
                score: None,
            })
        }
        TextMatchMode::Regex => {
            let re = plan.regexes.get(&pred.clause_id)?;
            let spans = re
                .find_iter(original)
                .take(MAX_EVIDENCE_SPANS)
                .map(|m| MatchSpan {
                    start: original[..m.start()].encode_utf16().count(),
                    end: original[..m.end()].encode_utf16().count(),
                })
                .collect::<Vec<_>>();
            (!spans.is_empty()).then_some(MatchEvidence {
                clause_id: pred.clause_id,
                field: pred.field,
                mode: pred.mode,
                spans,
                score: None,
            })
        }
        TextMatchMode::Fuzzy => fuzzy_evidence(pred, original),
    }
}

/// A6-folded text plus one original UTF-16 span per folded scalar. Both the
/// text-only and mapped paths are owned by `search_query`; this adapter only
/// changes the map's internal range type into the public evidence type.
fn folded_with_map(original: &str) -> (Vec<char>, Vec<MatchSpan>) {
    let mapped = canonical_fold_with_map(original);
    let folded = mapped.text.chars().collect();
    let map = mapped
        .sources
        .into_iter()
        .map(|source| MatchSpan {
            start: source.start,
            end: source.end,
        })
        .collect();
    (folded, map)
}

/// Predicates store their needles already folded exactly once by the parser or
/// typed-plan constructor. A6 is not idempotent, so this boundary must never
/// call `canonical_fold` again.
fn folded_needle_chars(folded_needle: &str) -> Vec<char> {
    folded_needle.chars().collect()
}

fn merge_spans(spans: impl IntoIterator<Item = MatchSpan>) -> Vec<MatchSpan> {
    let mut spans = spans.into_iter().collect::<Vec<_>>();
    spans.sort_by_key(|span| (span.start, span.end));
    let mut out: Vec<MatchSpan> = Vec::new();
    for span in spans {
        if let Some(last) = out.last_mut() {
            if span.start <= last.end {
                last.end = last.end.max(span.end);
                continue;
            }
        }
        out.push(span);
        if out.len() == MAX_EVIDENCE_SPANS {
            break;
        }
    }
    out
}

fn folded_substring_spans(original: &str, folded_needle: &str) -> Vec<MatchSpan> {
    let (hay, map) = folded_with_map(original);
    let needle = folded_needle_chars(folded_needle);
    if needle.is_empty() || needle.len() > hay.len() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    for start in 0..=hay.len() - needle.len() {
        if hay[start..start + needle.len()] == needle {
            let matched = &map[start..start + needle.len()];
            spans.push(MatchSpan {
                start: matched.iter().map(|span| span.start).min().unwrap(),
                end: matched.iter().map(|span| span.end).max().unwrap(),
            });
            if spans.len() == MAX_EVIDENCE_SPANS {
                break;
            }
        }
    }
    spans
}

fn fuzzy_evidence(pred: &TextPredicate, original: &str) -> Option<MatchEvidence> {
    let (hay, map) = folded_with_map(original);
    let needle = folded_needle_chars(&pred.value);
    if needle.is_empty() {
        return Some(MatchEvidence {
            clause_id: pred.clause_id,
            field: pred.field,
            mode: pred.mode,
            spans: Vec::new(),
            score: Some(0),
        });
    }
    if needle.len() <= hay.len() {
        for start in 0..=hay.len() - needle.len() {
            if hay[start..start + needle.len()] == needle {
                let matched = &map[start..start + needle.len()];
                return Some(MatchEvidence {
                    clause_id: pred.clause_id,
                    field: pred.field,
                    mode: pred.mode,
                    spans: vec![MatchSpan {
                        start: matched.iter().map(|span| span.start).min().unwrap(),
                        end: matched.iter().map(|span| span.end).max().unwrap(),
                    }],
                    score: Some(if start == 0 { 1000 } else { 500 }),
                });
            }
        }
    }
    let mut at = 0;
    let mut picked = Vec::new();
    for (i, ch) in hay.iter().enumerate() {
        if needle.get(at) == Some(ch) {
            picked.push(map[i]);
            at += 1;
            if at == needle.len() {
                return Some(MatchEvidence {
                    clause_id: pred.clause_id,
                    field: pred.field,
                    mode: pred.mode,
                    spans: merge_spans(picked),
                    score: Some(100),
                });
            }
        }
    }
    None
}

#[derive(Debug)]
enum PageCandidate {
    File(usize),
    Referenced(PageEntry),
}

#[derive(Debug)]
struct ScoredPage {
    score: i32,
    match_class: ObjectiveMatchClass,
    matched_text: String,
    matched_alias: Option<String>,
    tie_key: String,
    candidate: PageCandidate,
}

impl ScoredPage {
    fn is_better_than(&self, other: &Self) -> bool {
        self.match_class.rank() > other.match_class.rank()
            || (self.match_class == other.match_class
                && (self.score > other.score
                    || (self.score == other.score && self.tie_key < other.tie_key)))
    }
}

impl PartialEq for ScoredPage {
    fn eq(&self, other: &Self) -> bool {
        self.match_class == other.match_class
            && self.score == other.score
            && self.tie_key == other.tie_key
    }
}
impl Eq for ScoredPage {}
impl PartialOrd for ScoredPage {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ScoredPage {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap root is the WORST retained candidate, ready for eviction.
        other
            .match_class
            .rank()
            .cmp(&self.match_class.rank())
            .then_with(|| other.score.cmp(&self.score))
            .then_with(|| self.tie_key.cmp(&other.tie_key))
    }
}

fn push_page(heap: &mut BinaryHeap<ScoredPage>, limit: usize, candidate: ScoredPage) {
    if heap.len() < limit {
        heap.push(candidate);
    } else if heap
        .peek()
        .is_some_and(|worst| candidate.is_better_than(worst))
    {
        *heap.peek_mut().unwrap() = candidate;
    }
}

#[derive(Debug, Clone, Copy)]
struct BlockRelevance {
    match_class: ObjectiveMatchClass,
    word_boundary: bool,
    first_offset: usize,
    text_len: usize,
    occurrences: usize,
    positive: bool,
}

impl BlockRelevance {
    fn neutral() -> Self {
        Self {
            match_class: ObjectiveMatchClass::Exact,
            word_boundary: true,
            first_offset: 0,
            text_len: 0,
            occurrences: 0,
            positive: false,
        }
    }

    /// Greater means objectively better. Keep the components explicit so very
    /// large blocks cannot collapse distinct offsets or lengths into one
    /// saturated scalar score.
    fn cmp_quality(&self, other: &Self) -> Ordering {
        self.match_class
            .rank()
            .cmp(&other.match_class.rank())
            .then_with(|| self.word_boundary.cmp(&other.word_boundary))
            .then_with(|| other.first_offset.cmp(&self.first_offset))
            .then_with(|| other.text_len.cmp(&self.text_len))
            .then_with(|| other.occurrences.cmp(&self.occurrences))
    }

    fn score(self) -> i32 {
        let offset_penalty = self.first_offset.min(50_000) as i32;
        let length_penalty = self.text_len.min(40_000) as i32;
        let occurrence_penalty = self.occurrences.saturating_sub(1).min(9_999) as i32;
        self.match_class.rank() * 1_000_000 + i32::from(self.word_boundary) * 100_000
            - offset_penalty
            - length_penalty
            - occurrence_penalty
    }

    /// Lossless lexicographic sort key. ASCENDING byte order is EXACTLY
    /// [`Self::cmp_quality`] reversed, i.e. best first, so a database that can
    /// only `ORDER BY <blob> ASC` reproduces this module's ranking without
    /// re-implementing it.
    ///
    /// [`Self::score`] is deliberately NOT encoded: it saturates offsets,
    /// lengths and occurrence counts into one display `i32`, so two distinct
    /// tuples can collide there. `cmp_quality` is the ordering authority and
    /// all five of its components are carried here, in its own precedence
    /// order, each at fixed width so whole-key byte comparison equals
    /// component-wise comparison.
    ///
    /// `positive` is not a `cmp_quality` component and is not encoded; it is a
    /// membership detail of [`block_relevance`]'s neutral-negation arm.
    fn order_key(&self) -> [u8; BLOCK_RANK_KEY_LEN] {
        let mut key = [0u8; BLOCK_RANK_KEY_LEN];
        // Higher class rank is better, so the i32 encoding is inverted.
        key[0..4].copy_from_slice(&descending_i32_key(self.match_class.rank()));
        // `true` (a word-boundary match) is better, so the flag is inverted.
        key[4] = u8::from(!self.word_boundary);
        // The remaining three are "smaller is better" in `cmp_quality`, which
        // ascending unsigned order already gives. In particular the existing
        // rule that FEWER occurrences win is preserved, not reversed.
        key[5..13].copy_from_slice(&ascending_usize_key(self.first_offset));
        key[13..21].copy_from_slice(&ascending_usize_key(self.text_len));
        key[21..29].copy_from_slice(&ascending_usize_key(self.occurrences));
        key
    }
}

/// Width of [`BlockRelevance::order_key`]: `cmp_quality`'s five components at
/// fixed width -- class rank, boundary, first offset, UTF-16 text length,
/// occurrences.
pub(crate) const BLOCK_RANK_KEY_LEN: usize = 4 + 1 + 8 + 8 + 8;

/// The key widens `usize` into `u64`. Every shipped target is 32- or 64-bit, so
/// that widening is exact; this assertion is what makes "no truncation" a build
/// failure rather than a comment if a wider target ever appears.
const _: () = assert!(usize::BITS <= u64::BITS);

/// Order-preserving big-endian `i32` encoding whose ASCENDING byte order is
/// DESCENDING numeric order. Biasing by the sign bit makes negative values sort
/// below positive ones; complementing then reverses the whole order.
fn descending_i32_key(value: i32) -> [u8; 4] {
    (!((value as u32) ^ (1u32 << 31))).to_be_bytes()
}

/// Order-preserving big-endian `usize` encoding: ascending byte order is
/// ascending numeric order, exactly, including `usize::MAX`.
fn ascending_usize_key(value: usize) -> [u8; 8] {
    (value as u64).to_be_bytes()
}

/// Text-only block rank for a consumer that holds a block's exact visible text
/// but no `Graph`, `Document` or `DocBlock` -- the seam the forthcoming SQLite
/// adapter binds its `ORDER BY` to.
///
/// D-14, the existing-primitive rule: this is NOT a second matcher, parser or
/// regex grammar. The searched-for existing implementations ARE the
/// implementation here -- [`canonical_fold`] for the folded text,
/// [`block_relevance`] / [`text_predicate_relevance`] for the tuple, the plan's
/// already-compiled `regexes` map for regex predicates, and
/// [`eval_ranked_block_expr`] for evidence. The bridge only admits text and
/// re-encodes the existing tuple; it decides nothing about matching.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct BlockTextRank {
    relevance: BlockRelevance,
}

#[cfg_attr(not(test), allow(dead_code))]
impl BlockTextRank {
    /// The BLOB sort key. Bind it as-is and `ORDER BY key ASC` for best first.
    pub(crate) fn order_key(&self) -> [u8; BLOCK_RANK_KEY_LEN] {
        self.relevance.order_key()
    }

    /// The same display score existing block hits carry. Display only: it
    /// saturates, so it must never be the `ORDER BY` term.
    pub(crate) fn score(&self) -> i32 {
        self.relevance.score()
    }

    /// The same primary relevance band existing block hits carry.
    pub(crate) fn match_class(&self) -> ObjectiveMatchClass {
        self.relevance.match_class
    }
}

/// Rank one block's exact visible text against a block branch, with no graph,
/// document, page or traversal input at all.
///
/// `None` means the branch does not admit this text -- the same Boolean
/// membership [`block_relevance`] already decides, including a successful NOT
/// admitting on a neutral tuple, AND combining its positive children, and OR
/// taking its best branch.
///
/// Selection is rank-only: no [`MatchEvidence`] spans and no `BlockDto` are
/// constructed here. Physical page path and traversal index remain the
/// equal-rank tie breakers and are the SQL adapter's to supply; they are
/// deliberately not encoded in this text-only key. Page-name/alias ranking is
/// likewise not covered by these five components.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn rank_block_text(
    plan: &QueryPlan,
    branch: &QueryBranch,
    visible: &str,
) -> Option<BlockTextRank> {
    rank_block_text_folded(plan, branch, visible, &canonical_fold(visible))
}

/// [`rank_block_text`] for a caller that ALREADY holds the fold, so ranking a
/// whole projection does not recompute it per row.
///
/// `folded` MUST be exactly `canonical_fold(visible)`; every ranking property
/// of [`rank_block_text`] is preserved only under that equality, and passing
/// any other string silently changes which blocks a search admits. The
/// The projection stores raw authored block text. Every database executor
/// derives this exact pair from `block_text.content` plus `pages.path` through
/// `DocBlock::preamble`, so candidate indexing can remain lossy while the
/// verifier stays byte-faithful.
///
/// `visible` is still required in EVERY mode: the rank key's `text_len`
/// component counts UTF-16 units of the original, and a regex predicate matches
/// the original directly.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn rank_block_text_folded(
    plan: &QueryPlan,
    branch: &QueryBranch,
    visible: &str,
    folded: &str,
) -> Option<BlockTextRank> {
    if branch.target != QueryTarget::Blocks {
        return None;
    }
    block_relevance(plan, &branch.predicate, visible, folded)
        .map(|relevance| BlockTextRank { relevance })
}

/// Optional companion to [`rank_block_text`] for a consumer that has already
/// admitted a row and now wants the reason. It calls the existing
/// [`eval_ranked_block_expr`] on the admitted text and returns its existing
/// [`MatchEvidence`] values verbatim -- same spans, same UTF-16 offsets, same
/// best-branch OR choice, same empty evidence for a satisfied negation.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn admitted_block_evidence(
    plan: &QueryPlan,
    branch: &QueryBranch,
    visible: &str,
) -> Option<Vec<MatchEvidence>> {
    if branch.target != QueryTarget::Blocks {
        return None;
    }
    let lower = canonical_fold(visible);
    #[cfg(test)]
    BLOCK_EVIDENCE_EVALUATIONS.with(|count| count.set(count.get().saturating_add(1)));
    eval_ranked_block_expr(plan, &branch.predicate, visible, &lower).map(|matched| matched.evidence)
}

#[derive(Debug)]
#[cfg(test)]
struct ScoredBlock<'a> {
    relevance: BlockRelevance,
    index: usize,
    page: &'a PageEntry,
    block: &'a DocBlock,
    breadcrumb: Vec<String>,
}

#[cfg(test)]
impl ScoredBlock<'_> {
    fn is_better_than(&self, other: &Self) -> bool {
        let quality = self.relevance.cmp_quality(&other.relevance);
        quality == Ordering::Greater
            || (quality == Ordering::Equal
                && (self.page.rel_path.as_str(), self.index)
                    < (other.page.rel_path.as_str(), other.index))
    }
}

#[cfg(test)]
impl PartialEq for ScoredBlock<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.relevance.cmp_quality(&other.relevance) == Ordering::Equal
            && (self.page.rel_path.as_str(), self.index)
                == (other.page.rel_path.as_str(), other.index)
    }
}
#[cfg(test)]
impl Eq for ScoredBlock<'_> {}
#[cfg(test)]
impl PartialOrd for ScoredBlock<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
#[cfg(test)]
impl Ord for ScoredBlock<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap root is the WORST retained candidate, ready for eviction.
        other.relevance.cmp_quality(&self.relevance).then_with(|| {
            (self.page.rel_path.as_str(), self.index)
                .cmp(&(other.page.rel_path.as_str(), other.index))
        })
    }
}

#[cfg(test)]
fn push_block<'a>(
    heap: &mut BinaryHeap<ScoredBlock<'a>>,
    limit: usize,
    candidate: ScoredBlock<'a>,
) {
    if heap.len() < limit {
        heap.push(candidate);
    } else if heap
        .peek()
        .is_some_and(|worst| candidate.is_better_than(worst))
    {
        *heap.peek_mut().unwrap() = candidate;
    }
}

fn starts_at_word_boundary(value: &str, byte_offset: usize) -> bool {
    byte_offset == 0
        || value[..byte_offset]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_')
}

fn text_predicate_relevance(
    plan: &QueryPlan,
    pred: &TextPredicate,
    original: &str,
    lower: &str,
) -> Option<BlockRelevance> {
    let text_len = original.encode_utf16().count();
    let (match_class, word_boundary, first_offset, occurrences) = match pred.mode {
        TextMatchMode::Contains | TextMatchMode::Phrase => {
            if pred.value.is_empty() {
                return None;
            }
            let mut matches = lower.match_indices(&pred.value);
            let (first, _) = matches.next()?;
            let occurrences = 1 + matches.count();
            let match_class = if lower == pred.value {
                ObjectiveMatchClass::Exact
            } else if first == 0 {
                ObjectiveMatchClass::Prefix
            } else {
                ObjectiveMatchClass::Substring
            };
            (
                match_class,
                starts_at_word_boundary(lower, first),
                lower[..first].encode_utf16().count(),
                occurrences,
            )
        }
        TextMatchMode::Regex => {
            let regex = plan.regexes.get(&pred.clause_id)?;
            let mut matches = regex.find_iter(original);
            let first = matches.next()?;
            let occurrences = 1 + matches.count();
            let match_class = if first.start() == 0 && first.end() == original.len() {
                ObjectiveMatchClass::Exact
            } else if first.start() == 0 {
                ObjectiveMatchClass::Prefix
            } else {
                ObjectiveMatchClass::Substring
            };
            (
                match_class,
                starts_at_word_boundary(original, first.start()),
                original[..first.start()].encode_utf16().count(),
                occurrences,
            )
        }
        TextMatchMode::Fuzzy => {
            let (_, match_class) = fuzzy_name_score(lower, &pred.value)?;
            let first = lower.find(&pred.value).unwrap_or(0);
            (
                match_class,
                starts_at_word_boundary(lower, first),
                lower[..first].encode_utf16().count(),
                1,
            )
        }
    };
    Some(BlockRelevance {
        match_class,
        word_boundary,
        first_offset,
        text_len,
        occurrences,
        positive: true,
    })
}

fn block_relevance(
    plan: &QueryPlan,
    expr: &QueryExpr,
    original: &str,
    lower: &str,
) -> Option<BlockRelevance> {
    match expr {
        QueryExpr::Never => None,
        QueryExpr::Text(pred) if pred.field != TextField::VisibleContent => None,
        QueryExpr::Text(pred) => text_predicate_relevance(plan, pred, original, lower),
        QueryExpr::And(children) => {
            let mut combined = BlockRelevance::neutral();
            for child in children {
                let relevance = block_relevance(plan, child, original, lower)?;
                if !relevance.positive {
                    continue;
                }
                if !combined.positive {
                    combined = relevance;
                    continue;
                }
                if relevance.match_class.rank() < combined.match_class.rank() {
                    combined.match_class = relevance.match_class;
                }
                combined.word_boundary &= relevance.word_boundary;
                combined.first_offset =
                    combined.first_offset.saturating_add(relevance.first_offset);
                combined.occurrences = combined.occurrences.saturating_add(relevance.occurrences);
            }
            combined.positive.then_some(combined)
        }
        QueryExpr::Or(children) => {
            let mut best: Option<BlockRelevance> = None;
            for child in children {
                let Some(relevance) = block_relevance(plan, child, original, lower) else {
                    continue;
                };
                if !relevance.positive {
                    continue;
                }
                if best
                    .as_ref()
                    .is_none_or(|current| relevance.cmp_quality(current) == Ordering::Greater)
                {
                    best = Some(relevance);
                }
            }
            best
        }
        QueryExpr::Not(child) => block_relevance(plan, child, original, lower)
            .is_none()
            .then_some(BlockRelevance::neutral()),
    }
}

fn eval_ranked_block_expr(
    plan: &QueryPlan,
    expr: &QueryExpr,
    original: &str,
    lower: &str,
) -> Option<EvalMatch> {
    match expr {
        QueryExpr::Never => None,
        QueryExpr::Text(pred) => {
            if pred.field != TextField::VisibleContent {
                return None;
            }
            match_text(plan, pred, original).map(|evidence| EvalMatch {
                evidence: vec![evidence],
            })
        }
        QueryExpr::And(children) => {
            let mut evidence = Vec::new();
            for child in children {
                evidence.extend(eval_ranked_block_expr(plan, child, original, lower)?.evidence);
            }
            Some(EvalMatch { evidence })
        }
        QueryExpr::Or(children) => {
            let mut best: Option<(BlockRelevance, EvalMatch)> = None;
            for child in children {
                let Some(relevance) = block_relevance(plan, child, original, lower) else {
                    continue;
                };
                let Some(matched) = eval_ranked_block_expr(plan, child, original, lower) else {
                    continue;
                };
                if best
                    .as_ref()
                    .is_none_or(|(current, _)| relevance.cmp_quality(current) == Ordering::Greater)
                {
                    best = Some((relevance, matched));
                }
            }
            best.map(|(_, matched)| matched)
        }
        QueryExpr::Not(child) => eval_ranked_block_expr(plan, child, original, lower)
            .is_none()
            .then_some(EvalMatch {
                evidence: Vec::new(),
            }),
    }
}

fn fuzzy_name_score(lower_name: &str, lower_query: &str) -> Option<(i32, ObjectiveMatchClass)> {
    if lower_query.is_empty() {
        Some((0, ObjectiveMatchClass::Fuzzy))
    } else if lower_name == lower_query {
        Some((1500, ObjectiveMatchClass::Exact))
    } else if lower_name.starts_with(lower_query) {
        Some((1000, ObjectiveMatchClass::Prefix))
    } else if lower_name.contains(lower_query) {
        Some((500, ObjectiveMatchClass::Substring))
    } else {
        let mut chars = lower_name.chars();
        lower_query
            .chars()
            .all(|needle| chars.any(|candidate| candidate == needle))
            .then_some((100, ObjectiveMatchClass::Fuzzy))
    }
}

/// Match + page relevance in one cached-lowercase pass. AND takes the best
/// positive clause; OR uses the first matching branch, mirroring
/// `Matcher::score_name`.
fn page_base_score(
    plan: &QueryPlan,
    expr: &QueryExpr,
    original: &str,
    lower: &str,
) -> Option<(i32, ObjectiveMatchClass)> {
    match expr {
        QueryExpr::Never => None,
        QueryExpr::Text(pred) if pred.field != TextField::PageName => None,
        QueryExpr::Text(pred) => match pred.mode {
            TextMatchMode::Fuzzy => fuzzy_name_score(lower, &pred.value),
            TextMatchMode::Regex => plan
                .regexes
                .get(&pred.clause_id)
                .is_some_and(|regex| regex.is_match(original))
                .then_some((500, ObjectiveMatchClass::Substring)),
            TextMatchMode::Contains | TextMatchMode::Phrase => {
                if pred.value.is_empty() {
                    None
                } else if lower == pred.value {
                    Some((1500, ObjectiveMatchClass::Exact))
                } else if lower.starts_with(&pred.value) {
                    Some((1000, ObjectiveMatchClass::Prefix))
                } else if lower.contains(&pred.value) {
                    Some((500, ObjectiveMatchClass::Substring))
                } else {
                    None
                }
            }
        },
        QueryExpr::And(children) => {
            let mut score = 0;
            let mut class = ObjectiveMatchClass::Exact;
            for child in children {
                let (child_score, child_class) = page_base_score(plan, child, original, lower)?;
                score = score.max(child_score);
                if child_class.rank() < class.rank() {
                    class = child_class;
                }
            }
            Some((score, class))
        }
        QueryExpr::Or(children) => children
            .iter()
            .find_map(|child| page_base_score(plan, child, original, lower)),
        QueryExpr::Not(child) => {
            (!eval_expr_fast(plan, child, TextField::PageName, original, lower))
                // A successful exclusion contributes no positive relevance and
                // therefore must not weaken the class supplied by an AND sibling.
                .then_some((0, ObjectiveMatchClass::Exact))
        }
    }
}

/// Text-only rank for one page name or alias. This is deliberately one level
/// below a page hit: the caller still groups the physical page name and its
/// aliases, chooses exactly one winning text for that owner, and supplies the
/// physical path/reference-name tie key after ranking owners globally.
///
/// Owner-local choice and global page rank are different comparisons. An
/// admitted text equal to `page_exact` carries `exact_override`; that bit wins
/// the owner-local choice even when name and alias both expose the same public
/// Exact/1500 rank. It is not part of [`Self::global_order_key`], because the
/// existing [`ScoredPage`] comparator globally orders only match class then the
/// length-adjusted score. Equal ordinary aliases therefore remain stable when
/// the caller replaces only on [`Self::is_better_owner_choice_than`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct PageTextRank {
    base_score: i32,
    match_class: ObjectiveMatchClass,
    exact_override: bool,
}

/// Width of [`PageTextRank::global_order_key`]: signed match-class rank followed
/// by signed length-adjusted score, both in the existing comparator's order.
pub(crate) const PAGE_RANK_KEY_LEN: usize = 4 + 4;
/// Width of [`PageTextRank::owner_order_key`]: exact-override bit, match class,
/// then the unadjusted score used while choosing one text for an owner.
pub(crate) const PAGE_OWNER_RANK_KEY_LEN: usize = 1 + 4 + 4;

#[cfg_attr(not(test), allow(dead_code))]
impl PageTextRank {
    pub(crate) fn base_score(&self) -> i32 {
        self.base_score
    }

    pub(crate) fn match_class(&self) -> ObjectiveMatchClass {
        self.match_class
    }

    pub(crate) fn is_exact_override(&self) -> bool {
        self.exact_override
    }

    /// Strict owner-local comparison. Callers iterate the physical page name
    /// first and aliases in source order, replacing only when this returns
    /// true; that preserves the name on ordinary equal ties and the first alias
    /// on equal alias ties while still honoring an exact alias override.
    pub(crate) fn is_better_owner_choice_than(&self, other: &Self) -> bool {
        self.exact_override > other.exact_override
            || (self.exact_override == other.exact_override
                && (self.match_class.rank() > other.match_class.rank()
                    || (self.match_class == other.match_class
                        && self.base_score > other.base_score)))
    }

    /// Lossless owner-local page-text key. Ascending byte order is best first,
    /// including the private exact override that is deliberately absent from
    /// the global page comparator. Candidate source order remains the final tie
    /// breaker, preserving name-first and first-alias behavior.
    pub(crate) fn owner_order_key(&self) -> [u8; PAGE_OWNER_RANK_KEY_LEN] {
        let mut key = [0u8; PAGE_OWNER_RANK_KEY_LEN];
        key[0] = u8::from(!self.exact_override);
        key[1..5].copy_from_slice(&descending_i32_key(self.match_class.rank()));
        key[5..9].copy_from_slice(&descending_i32_key(self.base_score));
        key
    }

    /// Existing final score for a physical page/reference name. This uses the
    /// physical name's UTF-8 byte length even when the winning text is an alias,
    /// exactly as [`execute_page_candidates`] has always done.
    pub(crate) fn global_score(&self, physical_page_name: &str) -> i32 {
        self.global_score_for_name_len(physical_page_name.len())
    }

    pub(crate) fn global_score_for_name_len(&self, physical_page_name_len: usize) -> i32 {
        self.base_score - physical_page_name_len as i32
    }

    /// Lossless global page-rank key. Ascending byte order is best first and is
    /// exactly the first two terms of [`ScoredPage`] ordering. The owner-local
    /// exact bit and the consumer-owned path/reference-name tie key are omitted
    /// deliberately; neither is a global rank term.
    pub(crate) fn global_order_key(&self, physical_page_name: &str) -> [u8; PAGE_RANK_KEY_LEN] {
        self.global_order_key_for_name_len(physical_page_name.len())
    }

    pub(crate) fn global_order_key_for_name_len(
        &self,
        physical_page_name_len: usize,
    ) -> [u8; PAGE_RANK_KEY_LEN] {
        let mut key = [0u8; PAGE_RANK_KEY_LEN];
        key[0..4].copy_from_slice(&descending_i32_key(self.match_class.rank()));
        key[4..8].copy_from_slice(&descending_i32_key(
            self.global_score_for_name_len(physical_page_name_len),
        ));
        key
    }
}

fn rank_page_text_expr(plan: &QueryPlan, expr: &QueryExpr, text: &str) -> Option<PageTextRank> {
    let folded = canonical_fold(text);
    let (mut base_score, mut match_class) = page_base_score(plan, expr, text, &folded)?;
    let exact_override = plan.page_exact.as_deref() == Some(folded.as_str());
    if exact_override {
        base_score = 1500;
        match_class = ObjectiveMatchClass::Exact;
    }
    Some(PageTextRank {
        base_score,
        match_class,
        exact_override,
    })
}

/// Rank one exact page-name or alias text under a page branch without building
/// spans, page inventory objects, graph state or result DTOs. Regex predicates
/// use the plan's already-compiled regex map through [`page_base_score`].
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn rank_page_text(
    plan: &QueryPlan,
    branch: &QueryBranch,
    text: &str,
) -> Option<PageTextRank> {
    if branch.target != QueryTarget::Pages {
        return None;
    }
    rank_page_text_expr(plan, &branch.predicate, text)
}

/// Produce the existing page-name evidence only after a text has been admitted.
/// The caller passes the exact winning name/alias text, so evidence retains the
/// original spelling and UTF-16 relationship exposed by current page hits.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn admitted_page_evidence(
    plan: &QueryPlan,
    branch: &QueryBranch,
    text: &str,
) -> Option<Vec<MatchEvidence>> {
    if branch.target != QueryTarget::Pages {
        return None;
    }
    eval_expr(plan, &branch.predicate, TextField::PageName, text).map(|matched| matched.evidence)
}

fn best_page_match(
    plan: &QueryPlan,
    expr: &QueryExpr,
    page_name: &str,
    aliases: &[String],
) -> Option<(i32, ObjectiveMatchClass, String, Option<String>)> {
    let page_match = rank_page_text_expr(plan, expr, page_name);
    let mut best = page_match.map(|rank| (rank, page_name.to_string(), None));
    for alias in aliases {
        let Some(rank) = rank_page_text_expr(plan, expr, alias) else {
            continue;
        };
        let replace = best
            .as_ref()
            .is_none_or(|(current, _, _)| rank.is_better_owner_choice_than(current));
        if replace {
            best = Some((rank, alias.clone(), Some(alias.clone())));
        }
    }
    best.map(|(rank, text, alias)| (rank.base_score, rank.match_class, text, alias))
}

#[cfg(test)]
fn execute_pages<G: QueryGraph>(
    plan: &QueryPlan,
    graph: &G,
    branch: &QueryBranch,
    cancelled: &impl Fn() -> bool,
) -> Option<(Vec<QueryHit>, bool)> {
    let file_pages = graph.list_pages();
    let aliases = graph.page_aliases_with_owners();
    let referenced = graph.referenced_page_names();
    execute_page_candidates(plan, &file_pages, &aliases, &referenced, branch, cancelled)
}

fn execute_page_candidates(
    plan: &QueryPlan,
    file_pages: &[PageEntry],
    aliases: &[(String, String, String)],
    referenced: &[String],
    branch: &QueryBranch,
    cancelled: &impl Fn() -> bool,
) -> Option<(Vec<QueryHit>, bool)> {
    if branch.limit == 0 {
        return Some((Vec::new(), false));
    }
    let mut aliases_by_owner: HashMap<String, Vec<String>> = HashMap::new();
    for (alias, _, owner_rel_path) in aliases {
        aliases_by_owner
            .entry(owner_rel_path.clone())
            .or_default()
            .push(alias.clone());
    }
    let mut heap = BinaryHeap::new();
    let mut has_more = false;
    for (index, page) in file_pages.iter().enumerate() {
        if cancelled() {
            return None;
        }
        let aliases = aliases_by_owner
            .get(&page.rel_path)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if plan.page_name_suggestions() {
            let mut seen = HashSet::new();
            for (text, matched_alias) in std::iter::once((&page.name, None))
                .chain(aliases.iter().map(|alias| (alias, Some(alias.clone()))))
            {
                if !seen.insert(text.as_str()) {
                    continue;
                }
                let Some(rank) = rank_page_text_expr(plan, &branch.predicate, text) else {
                    continue;
                };
                has_more |= heap.len() >= branch.limit;
                push_page(
                    &mut heap,
                    branch.limit,
                    ScoredPage {
                        score: rank.global_score(&page.name),
                        match_class: rank.match_class(),
                        matched_text: text.clone(),
                        matched_alias,
                        tie_key: format!("{}\0{text}", page.rel_path),
                        candidate: PageCandidate::File(index),
                    },
                );
            }
        } else if let Some((base_score, match_class, matched_text, matched_alias)) =
            best_page_match(plan, &branch.predicate, &page.name, aliases)
        {
            has_more |= heap.len() >= branch.limit;
            push_page(
                &mut heap,
                branch.limit,
                ScoredPage {
                    score: base_score - page.name.len() as i32,
                    match_class,
                    matched_text,
                    matched_alias,
                    tie_key: page.rel_path.clone(),
                    candidate: PageCandidate::File(index),
                },
            );
        }
    }
    let mut have: HashSet<String> = file_pages
        .iter()
        .map(|page| crate::refs::page_key(&page.name))
        .collect();
    // GH #353: an alias of a file page names THAT page — the owner carries the
    // identity (with `matched_alias` as display context). The alias text must
    // never also appear as a referenced (virtual, path-less) page candidate:
    // selecting that phantom row navigated to a standalone alias-named page.
    for owner_aliases in aliases_by_owner.values() {
        for alias in owner_aliases {
            have.insert(crate::refs::page_key(alias));
        }
    }
    for name in referenced {
        if cancelled() {
            return None;
        }
        let key = crate::refs::page_key(&name);
        if have.contains(&key) {
            continue;
        }
        if let Some((base_score, match_class, matched_text, matched_alias)) =
            best_page_match(plan, &branch.predicate, &name, &[])
        {
            has_more |= heap.len() >= branch.limit;
            let score = base_score - name.len() as i32;
            push_page(
                &mut heap,
                branch.limit,
                ScoredPage {
                    score,
                    match_class,
                    matched_text,
                    matched_alias,
                    tie_key: crate::refs::page_key(&name),
                    candidate: PageCandidate::Referenced(PageEntry {
                        name: name.clone(),
                        kind: PageKind::Page,
                        date_key: None,
                        rel_path: String::new(),
                        path: std::path::PathBuf::new(),
                    }),
                },
            );
        }
    }
    let mut winners = heap.into_vec();
    winners.sort_by(|a, b| {
        b.match_class
            .rank()
            .cmp(&a.match_class.rank())
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| a.tie_key.cmp(&b.tie_key))
    });
    Some((
        winners
            .into_iter()
            .map(|winner| {
                let page = match winner.candidate {
                    PageCandidate::File(index) => file_pages[index].clone(),
                    PageCandidate::Referenced(page) => page,
                };
                let evidence =
                    admitted_page_evidence(plan, branch, &winner.matched_text).unwrap_or_default();
                QueryHit::Page {
                    display_text: winner.matched_text,
                    page,
                    evidence,
                    score: winner.score,
                    match_class: winner.match_class,
                    matched_alias: winner.matched_alias,
                    // The walk oracle answers no Display-enabled request.
                    row: None,
                }
            })
            .collect(),
        has_more,
    ))
}

/// Execute the established literal page autocomplete/quick-switch semantics
/// over an explicitly supplied candidate set.
pub(crate) fn pre_ready_page_search_entries(
    file_pages: Vec<PageEntry>,
    aliases: Vec<(String, String, String)>,
    referenced: Vec<String>,
    query: &str,
    limit: usize,
) -> Vec<PageEntry> {
    let plan = QueryPlan::legacy_page_search(query, limit);
    let Some(branch) = plan.branches.first() else {
        return Vec::new();
    };
    execute_page_candidates(&plan, &file_pages, &aliases, &referenced, branch, &|| false)
        .map(|(hits, _)| page_hits_to_entries(hits))
        .unwrap_or_default()
}

fn walk_blocks<'a>(
    blocks: &'a [DocBlock],
    ancestors: &mut Vec<&'a DocBlock>,
    visit: &mut impl FnMut(&'a DocBlock, &[&'a DocBlock]) -> bool,
) -> bool {
    for block in blocks {
        if !visit(block, ancestors) {
            return false;
        }
        ancestors.push(block);
        let keep_going = walk_blocks(&block.children, ancestors, visit);
        ancestors.pop();
        if !keep_going {
            return false;
        }
    }
    true
}

/// The page side of a pre-ready search: every page, alias and referenced
/// name in one parsed snapshot. It depends on the snapshot only, so the graph
/// keeps one per cache generation instead of rebuilding it per keystroke
/// (~105 ms at 10k pages, GH #543 Ctrl-K).
#[derive(Default)]
pub(crate) struct PreReadyPageInventory {
    file_pages: Vec<PageEntry>,
    aliases: Vec<(String, String, String)>,
    referenced: Vec<String>,
}

#[cfg(test)]
thread_local! {
    pub(crate) static PRE_READY_INVENTORY_BUILDS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// `None` when cancelled.
pub(crate) fn pre_ready_page_inventory(
    pages: &[(PageEntry, std::sync::Arc<crate::doc::Document>)],
    cancelled: &impl Fn() -> bool,
) -> Option<PreReadyPageInventory> {
    #[cfg(test)]
    PRE_READY_INVENTORY_BUILDS.with(|builds| builds.set(builds.get() + 1));
    let mut file_pages = Vec::with_capacity(pages.len());
    let mut aliases = Vec::new();
    for (entry, document) in pages {
        if cancelled() {
            return None;
        }
        file_pages.push(entry.clone());
        for (alias, _) in crate::query::document_alias_spellings(document) {
            aliases.push((alias, entry.name.clone(), entry.rel_path.clone()));
        }
    }
    let referenced =
        crate::query::referenced_page_names_from_snapshot_cancellable(pages, cancelled)?;
    Some(PreReadyPageInventory {
        file_pages,
        aliases,
        referenced,
    })
}

/// The explicitly allowed pre-ready producer for Ctrl-K text and `((`.
/// Membership and evidence come from the same compiled plan as indexed search;
/// only ordering differs: one captured parsed snapshot is emitted in document
/// order, under the plan's existing per-branch presentation limits.
pub(crate) fn pre_ready_interactive_snapshot(
    plan: &QueryPlan,
    pages: &[(PageEntry, std::sync::Arc<crate::doc::Document>)],
    inventory: impl FnOnce() -> Option<std::sync::Arc<PreReadyPageInventory>>,
    explain: bool,
    cancelled: &impl Fn() -> bool,
) -> QueryExecution {
    let explanation = if explain {
        plan.explanation()
    } else {
        QueryExplanation {
            branches: Vec::new(),
        }
    };
    if !plan.diagnostics.is_empty() {
        return QueryExecution {
            hits: Vec::new(),
            diagnostics: plan.diagnostics.clone(),
            explanation,
            has_more: QueryHasMore::default(),
            cancelled: false,
        };
    }

    let needs_page_inventory = plan
        .branches
        .iter()
        .any(|branch| branch.target == QueryTarget::Pages && branch.limit > 0);
    let inventory = if needs_page_inventory {
        let Some(inventory) = inventory() else {
            return cancelled_execution(plan, explanation);
        };
        inventory
    } else {
        std::sync::Arc::default()
    };
    let mut hits = Vec::new();
    let mut has_more = QueryHasMore::default();
    for branch in &plan.branches {
        if cancelled() {
            return cancelled_execution(plan, explanation);
        }
        match branch.target {
            QueryTarget::Pages => {
                let Some((page_hits, page_has_more)) = execute_page_candidates(
                    plan,
                    &inventory.file_pages,
                    &inventory.aliases,
                    &inventory.referenced,
                    branch,
                    cancelled,
                ) else {
                    return cancelled_execution(plan, explanation);
                };
                hits.extend(page_hits);
                has_more.pages |= page_has_more;
            }
            QueryTarget::Blocks => {
                if branch.limit == 0 {
                    continue;
                }
                let mut admitted = 0usize;
                'pages: for (entry, document) in pages {
                    if cancelled() {
                        return cancelled_execution(plan, explanation);
                    }
                    if let Some(scope) = plan.page_scope() {
                        let selected = match scope.path.as_deref() {
                            Some(path) => entry.rel_path == path,
                            None => {
                                entry.kind == scope.page_kind
                                    && refs::same_page(&entry.name, &scope.name)
                            }
                        };
                        if !selected {
                            continue;
                        }
                    }
                    let mut ancestors = Vec::new();
                    let complete =
                        walk_blocks(&document.roots, &mut ancestors, &mut |block, path| {
                            if cancelled() {
                                return false;
                            }
                            let projection = block.projection();
                            let Some(rank) = rank_block_text_folded(
                                plan,
                                branch,
                                &projection.visible,
                                &projection.visible_lower,
                            ) else {
                                return true;
                            };
                            if admitted == branch.limit {
                                has_more.blocks = true;
                                return false;
                            }
                            let mut dto = crate::vocab::block_to_shallow_dto(block);
                            dto.breadcrumb = path
                                .iter()
                                .map(|ancestor| crate::doc::crumb_line(ancestor))
                                .collect();
                            hits.push(QueryHit::Block {
                                page: entry.name.clone(),
                                kind: entry.kind,
                                path: entry.rel_path.clone(),
                                block: dto,
                                display_text: projection.visible.clone(),
                                evidence: admitted_block_evidence(
                                    plan,
                                    branch,
                                    &projection.visible,
                                )
                                .unwrap_or_default(),
                                score: rank.score(),
                                match_class: rank.match_class(),
                            });
                            admitted += 1;
                            true
                        });
                    if !complete {
                        if cancelled() {
                            return cancelled_execution(plan, explanation);
                        }
                        break 'pages;
                    }
                }
            }
        }
    }
    QueryExecution {
        hits,
        diagnostics: plan.diagnostics.clone(),
        explanation,
        has_more,
        cancelled: false,
    }
}

/// The block-branch evaluator.
///
/// `pages` yields borrowed `(inventory entry, converted roots)` pairs from
/// `Graph::with_pages`'s cached `Arc<Document>`.
///
/// Evidence and the result DTO are produced once per WINNER, after the heap is
/// drained. Doing it per retained candidate inside the walk is O(retained)
/// parses and DTOs where this is O(limit).
///
/// Generic, not `dyn`: monomorphization keeps each caller's walk exactly the
/// code it would have written by hand -- no per-block allocation, no indirect
/// call inside the block walk.
#[cfg(test)]
fn execute_block_candidates<'a, I>(
    plan: &QueryPlan,
    pages: I,
    branch: &QueryBranch,
    cancelled: &impl Fn() -> bool,
) -> Option<(Vec<QueryHit>, bool)>
where
    I: IntoIterator<Item = (&'a PageEntry, &'a [DocBlock])>,
{
    let mut heap = BinaryHeap::new();
    let mut has_more = false;
    let mut index = 0usize;
    for (entry, roots) in pages {
        if cancelled() {
            return None;
        }
        if let Some(scope) = &plan.page_scope {
            let selected = match scope.path.as_deref() {
                Some(path) => entry.rel_path == path,
                None => entry.kind == scope.page_kind && refs::same_page(&entry.name, &scope.name),
            };
            if !selected {
                continue;
            }
        }
        let mut ancestors = Vec::new();
        walk_blocks(roots, &mut ancestors, &mut |block, path| {
            if cancelled() {
                return false;
            }
            let candidate_index = index;
            index = index.saturating_add(1);
            let projection = block.projection();
            let visible = &projection.visible;
            if let Some(relevance) =
                block_relevance(plan, &branch.predicate, visible, &projection.visible_lower)
            {
                has_more |= heap.len() >= branch.limit;
                let retain = heap.len() < branch.limit
                    || heap.peek().is_some_and(|worst: &ScoredBlock<'_>| {
                        relevance.cmp_quality(&worst.relevance) == Ordering::Greater
                            || (relevance.cmp_quality(&worst.relevance) == Ordering::Equal
                                && (entry.rel_path.as_str(), candidate_index)
                                    < (worst.page.rel_path.as_str(), worst.index))
                    });
                if retain {
                    push_block(
                        &mut heap,
                        branch.limit,
                        ScoredBlock {
                            relevance,
                            index: candidate_index,
                            page: entry,
                            block,
                            breadcrumb: path
                                .iter()
                                .map(|ancestor| crate::doc::crumb_line(ancestor))
                                .collect(),
                        },
                    );
                }
            }
            true
        });
        if cancelled() {
            return None;
        }
    }
    let mut winners = heap.into_vec();
    winners.sort_by(|a, b| {
        b.relevance.cmp_quality(&a.relevance).then_with(|| {
            (a.page.rel_path.as_str(), a.index).cmp(&(b.page.rel_path.as_str(), b.index))
        })
    });
    Some((
        winners
            .into_iter()
            .map(|winner| {
                let projection = winner.block.projection();
                #[cfg(test)]
                BLOCK_EVIDENCE_EVALUATIONS.with(|count| count.set(count.get().saturating_add(1)));
                let matched = eval_ranked_block_expr(
                    plan,
                    &branch.predicate,
                    &projection.visible,
                    &projection.visible_lower,
                )
                .expect("rank and evidence evaluators must agree");
                // Search hits are result identities, not independent copies
                // of their entire descendant trees. The source page owns the
                // hierarchy and live consumers hydrate it once per page.
                let mut dto = crate::vocab::block_to_shallow_dto(winner.block);
                dto.breadcrumb = winner.breadcrumb;
                QueryHit::Block {
                    page: winner.page.name.clone(),
                    kind: winner.page.kind,
                    path: winner.page.rel_path.clone(),
                    block: dto,
                    display_text: projection.visible.clone(),
                    evidence: matched.evidence,
                    score: winner.relevance.score(),
                    match_class: winner.relevance.match_class,
                }
            })
            .collect(),
        has_more,
    ))
}

#[cfg(test)]
fn execute_blocks<G: QueryGraph>(
    plan: &QueryPlan,
    graph: &G,
    branch: &QueryBranch,
    cancelled: &impl Fn() -> bool,
) -> Option<(Vec<QueryHit>, bool)> {
    if branch.limit == 0 {
        return Some((Vec::new(), false));
    }
    let execute = |pages: &[(PageEntry, std::sync::Arc<crate::doc::Document>)]| {
        execute_block_candidates(
            plan,
            pages
                .iter()
                .map(|(entry, doc)| (entry, doc.roots.as_slice())),
            branch,
            cancelled,
        )
    };
    graph.with_pages(execute)
}

/// Convert typed block hits back to the exact grouped shape used by existing
/// search/query consumers. Hits arrive in global relevance order; only contiguous
/// hits from the same page are coalesced, so flattening the groups preserves that
/// order even when a page appears in more than one group.
pub(crate) fn block_hits_to_groups(hits: Vec<QueryHit>) -> Vec<crate::vocab::RefGroup> {
    let mut groups: Vec<crate::vocab::RefGroup> = Vec::new();
    for hit in hits {
        let QueryHit::Block {
            page, kind, block, ..
        } = hit
        else {
            continue;
        };
        if let Some(last) = groups.last_mut() {
            if last.page == page && last.kind == kind {
                last.blocks.push(block);
                continue;
            }
        }
        groups.push(crate::vocab::RefGroup {
            page,
            kind,
            blocks: vec![block],
            evidence: Vec::new(),
        });
    }
    groups
}

pub(crate) fn page_hits_to_entries(hits: Vec<QueryHit>) -> Vec<PageEntry> {
    hits.into_iter()
        .filter_map(|hit| match hit {
            QueryHit::Page {
                mut page,
                matched_alias,
                ..
            } => {
                if let Some(alias) = matched_alias {
                    page.name = alias;
                }
                Some(page)
            }
            QueryHit::Block { .. } => None,
        })
        .collect()
}

#[cfg(test)]
#[path = "query_plan_tests.rs"]
mod tests;
