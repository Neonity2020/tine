//! Typed execution plan for friendly graph search.
//!
//! This module is deliberately narrower than the full `{{query}}` DSL today:
//! it unifies the two graph-backed parts of Ctrl-K (page names and block text)
//! without changing the command/create-page providers or the established
//! block-query result contract.  The plan/result types are the seam that a
//! durable query workspace can grow into later.

use crate::doc::DocBlock;
use crate::model::{BlockDto, Graph, PageEntry, PageKind};
use crate::refs;
use crate::search_query::{canonical_fold, Matcher, Term};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

const MAX_EVIDENCE_SPANS: usize = 32;

/// How many times the shared block evaluator has produced match evidence and a
/// result DTO. The architectural claim is that this is once per WINNER, not
/// once per retained candidate -- O(limit), not O(retained) -- and a counter is
/// the only way to state that as a test rather than as a comment.
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

impl QueryBranch {
    /// The already-folded needle of a branch that is EXACTLY one literal fuzzy
    /// `VisibleContent` predicate -- the `((` block-picker shape produced by
    /// [`QueryPlan::block_search_literal`].
    ///
    /// Such a branch, and only such a branch, can be narrowed by a stored
    /// ordered-subsequence candidate index: the index answers "which pages
    /// could contain this subsequence", the parser-owned matcher still ranks
    /// blocks and produces evidence, and a narrowed scan therefore returns the
    /// same results as a full scan. A branch with boolean structure, negation
    /// or a regex is NOT narrowable this way, because a page that fails the
    /// literal test can still satisfy the branch.
    ///
    /// One accessor, used by both storage modes, so a future narrowable shape
    /// cannot be taught to one evaluator and not the other.
    pub(crate) fn fuzzy_visible_content_needle(&self) -> Option<&str> {
        match &self.predicate {
            QueryExpr::Text(TextPredicate {
                field: TextField::VisibleContent,
                mode: TextMatchMode::Fuzzy,
                value,
                ..
            }) => Some(value.as_str()),
            _ => None,
        }
    }
}

impl QueryPlan {
    /// The narrowable fuzzy needle of this plan's block branch, if it has one.
    pub(crate) fn fuzzy_block_needle(&self) -> Option<&str> {
        self.branches
            .iter()
            .find(|branch| branch.target == QueryTarget::Blocks)
            .and_then(QueryBranch::fuzzy_visible_content_needle)
    }

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

/// One exact current managed page paired with the same inventory entry used by
/// Direct Files for scope, path tie-breaking and page-hit projection.
///
/// `roots` is the page's already-converted block tree, supplied by the caller
/// from its `ApplicationProjectionCache`. It is the managed analogue of the
/// cached `Arc<Document>` Direct Files walks: retaining it here is what lets
/// the shared evaluator hold `&DocBlock` winners and defer evidence/DTO
/// construction until the heap is drained, exactly as the Direct path does.
#[cfg(test)]
pub(crate) struct ApplicationQueryPlanPage {
    pub(crate) entry: PageEntry,
    pub(crate) roots: std::sync::Arc<Vec<DocBlock>>,
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
/// Every public Friendly route — Direct's `run_graph_search*`, the Managed
/// `application_navigation` GraphSearch arm — asks THIS function which plan a
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
    match scope {
        // A routed search selects blocks inside ONE physical page, so it has no
        // Pages section and page membership scope does not apply to it.
        Some(scope) => {
            QueryPlan::friendly_for_page_with_display(source, block_limit, scope, display)
        }
        None => QueryPlan::friendly_with_display(source, page_limit, block_limit, display),
    }
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
    // title/alias can retain the objective Exact class. The parsed AND terms
    // alone would otherwise downgrade `Foo Bar` to Prefix/Substring and make
    // the frontend offer a duplicate Create row beside the existing page.
    page_exact: Option<String>,
    regexes: HashMap<u32, Regex>,
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
        Self {
            branches: vec![QueryBranch {
                target: QueryTarget::Pages,
                predicate: QueryExpr::Text(TextPredicate {
                    clause_id: 1,
                    field: TextField::PageName,
                    mode: TextMatchMode::Fuzzy,
                    value: canonical_fold(&value),
                }),
                limit,
            }],
            diagnostics: Vec::new(),
            page_scope: None,
            display: FriendlyDisplayOptions::default(),
            page_exact: None,
            regexes: HashMap::new(),
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
        }
    }

    /// Literal block autocomplete for the `((` picker. OG rev 6e7afa8eb's
    /// `search.cljs:block-search`/`fuzzy-search` normalizes the whole query as
    /// one literal term. Blank input has no candidates.
    pub fn block_search_literal(query: &str, limit: usize) -> Self {
        let branches = if query.is_empty() {
            Vec::new()
        } else {
            vec![QueryBranch {
                target: QueryTarget::Blocks,
                predicate: QueryExpr::Text(TextPredicate {
                    clause_id: 1,
                    field: TextField::VisibleContent,
                    mode: TextMatchMode::Fuzzy,
                    value: canonical_fold(query),
                }),
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
    pub fn execute(&self, graph: &Graph, cancelled: impl Fn() -> bool) -> QueryExecution {
        self.execute_with_explain(graph, cancelled, true)
    }

    #[cfg(test)]
    pub fn execute_with_explain(
        &self,
        graph: &Graph,
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

    #[cfg(test)]
    pub(crate) fn execute_application_with_explain(
        &self,
        file_pages: Vec<PageEntry>,
        pages: &[ApplicationQueryPlanPage],
        aliases: Vec<(String, String, String)>,
        referenced: Vec<String>,
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
                QueryTarget::Pages => execute_page_candidates(
                    self,
                    file_pages.clone(),
                    aliases.clone(),
                    referenced.clone(),
                    branch,
                    &cancelled,
                ),
                QueryTarget::Blocks => execute_application_blocks(self, pages, branch, &cancelled),
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
    let text = QueryExpr::Text(TextPredicate {
        clause_id: take_id(next_id),
        field,
        mode: if term.quoted {
            TextMatchMode::Phrase
        } else {
            TextMatchMode::Contains
        },
        value: term.text.clone(),
    });
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
            TextMatchMode::Contains | TextMatchMode::Phrase => lower.contains(&pred.value),
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
            let spans = casefold_substring_spans(original, &pred.value);
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

/// Lowercase-plus-NFC text plus one original UTF-16 span per folded scalar.
/// Normalization is performed per extended grapheme cluster, which is the
/// boundary across which canonical composition cannot contribute. Every output
/// scalar maps to the full union of original scalars that formed that grapheme,
/// including lowercase expansions, reordered marks, and Hangul Jamo.
fn folded_with_map(original: &str) -> (Vec<char>, Vec<MatchSpan>) {
    let lowered = original.to_lowercase();
    let mut lowered_sources = Vec::new();
    let mut original_utf16 = 0;
    for ch in original.chars() {
        let start = original_utf16;
        original_utf16 += ch.len_utf16();
        for _ in ch.to_lowercase() {
            lowered_sources.push(MatchSpan {
                start,
                end: original_utf16,
            });
        }
    }
    // Rust's whole-string lowercase differs from scalar lowercase only by
    // contextual substitutions such as final sigma, never by scalar count.
    debug_assert_eq!(lowered.chars().count(), lowered_sources.len());

    let mut folded = Vec::new();
    let mut map = Vec::new();
    let mut source_at = 0;
    for grapheme in lowered.graphemes(true) {
        let scalar_count = grapheme.chars().count();
        let contributors = &lowered_sources[source_at..source_at + scalar_count];
        source_at += scalar_count;
        let source = MatchSpan {
            start: contributors.first().map_or(0, |span| span.start),
            end: contributors.last().map_or(0, |span| span.end),
        };
        for normalized in grapheme.nfc() {
            folded.push(normalized);
            map.push(source);
        }
    }
    debug_assert_eq!(folded.iter().collect::<String>(), canonical_fold(original));
    (folded, map)
}

fn folded_chars(value: &str) -> Vec<char> {
    canonical_fold(value).chars().collect()
}

fn merge_spans(spans: impl IntoIterator<Item = MatchSpan>) -> Vec<MatchSpan> {
    let mut out: Vec<MatchSpan> = Vec::new();
    for span in spans {
        if let Some(last) = out.last_mut() {
            if last.end == span.start {
                last.end = span.end;
                continue;
            }
            if *last == span {
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

fn casefold_substring_spans(original: &str, needle: &str) -> Vec<MatchSpan> {
    let (hay, map) = folded_with_map(original);
    let needle = folded_chars(needle);
    if needle.is_empty() || needle.len() > hay.len() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    for start in 0..=hay.len() - needle.len() {
        if hay[start..start + needle.len()] == needle {
            let first = map[start];
            let last = map[start + needle.len() - 1];
            spans.push(MatchSpan {
                start: first.start,
                end: last.end,
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
    let needle = folded_chars(&pred.value);
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
                let first = map[start];
                let last = map[start + needle.len() - 1];
                return Some(MatchEvidence {
                    clause_id: pred.clause_id,
                    field: pred.field,
                    mode: pred.mode,
                    spans: vec![MatchSpan {
                        start: first.start,
                        end: last.end,
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
const BLOCK_RANK_KEY_LEN: usize = 4 + 1 + 8 + 8 + 8;

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
    if branch.target != QueryTarget::Blocks {
        return None;
    }
    let lower = canonical_fold(visible);
    block_relevance(plan, &branch.predicate, visible, &lower)
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
                if lower == pred.value {
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
const PAGE_RANK_KEY_LEN: usize = 4 + 4;
/// Width of [`PageTextRank::owner_order_key`]: exact-override bit, match class,
/// then the unadjusted score used while choosing one text for an owner.
const PAGE_OWNER_RANK_KEY_LEN: usize = 1 + 4 + 4;

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
        self.base_score - physical_page_name.len() as i32
    }

    /// Lossless global page-rank key. Ascending byte order is best first and is
    /// exactly the first two terms of [`ScoredPage`] ordering. The owner-local
    /// exact bit and the consumer-owned path/reference-name tie key are omitted
    /// deliberately; neither is a global rank term.
    pub(crate) fn global_order_key(&self, physical_page_name: &str) -> [u8; PAGE_RANK_KEY_LEN] {
        let mut key = [0u8; PAGE_RANK_KEY_LEN];
        key[0..4].copy_from_slice(&descending_i32_key(self.match_class.rank()));
        key[4..8].copy_from_slice(&descending_i32_key(self.global_score(physical_page_name)));
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
fn execute_pages(
    plan: &QueryPlan,
    graph: &Graph,
    branch: &QueryBranch,
    cancelled: &impl Fn() -> bool,
) -> Option<(Vec<QueryHit>, bool)> {
    let file_pages = graph.list_pages();
    let aliases = graph.page_aliases_with_owners();
    let referenced = graph.referenced_page_names();
    execute_page_candidates(plan, file_pages, aliases, referenced, branch, cancelled)
}

fn execute_page_candidates(
    plan: &QueryPlan,
    file_pages: Vec<PageEntry>,
    aliases: Vec<(String, String, String)>,
    referenced: Vec<String>,
    branch: &QueryBranch,
    cancelled: &impl Fn() -> bool,
) -> Option<(Vec<QueryHit>, bool)> {
    if branch.limit == 0 {
        return Some((Vec::new(), false));
    }
    let mut aliases_by_owner: HashMap<String, Vec<String>> = HashMap::new();
    for (alias, _, owner_rel_path) in aliases {
        aliases_by_owner
            .entry(owner_rel_path)
            .or_default()
            .push(alias);
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
        if let Some((base_score, match_class, matched_text, matched_alias)) =
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
        .map(|page| canonical_fold(&page.name))
        .collect();
    // GH #353: an alias of a file page names THAT page — the owner carries the
    // identity (with `matched_alias` as display context). The alias text must
    // never also appear as a referenced (virtual, path-less) page candidate:
    // selecting that phantom row navigated to a standalone alias-named page.
    for owner_aliases in aliases_by_owner.values() {
        for alias in owner_aliases {
            have.insert(canonical_fold(alias));
        }
    }
    for name in referenced {
        if cancelled() {
            return None;
        }
        let key = canonical_fold(&name);
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
                        name,
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
/// over an explicitly supplied exact-frontier candidate set. Managed storage
/// uses this to share ranking with Direct Files without constructing a parsed
/// `Graph` cache.
pub(crate) fn legacy_page_search_entries(
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
    execute_page_candidates(&plan, file_pages, aliases, referenced, branch, &|| false)
        .map(|(hits, _)| page_hits_to_entries(hits))
        .unwrap_or_default()
}

#[cfg(test)]
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

/// The one block-branch evaluator, shared by Direct Files and managed storage.
///
/// `pages` yields borrowed `(inventory entry, converted roots)` pairs, which is
/// the only thing the two storage modes genuinely disagree about: Direct
/// borrows from `Graph::with_pages`'s cached `Arc<Document>`, managed from the
/// converted tree its projection cache retains. Everything the two paths used
/// to disagree about by accident -- when evidence is evaluated, when the result
/// DTO is built, what is cloned per candidate -- lives here now and therefore
/// cannot drift again.
///
/// Evidence and the result DTO are produced once per WINNER, after the heap is
/// drained. Doing it per retained candidate inside the walk (which the managed
/// twin used to do) is O(retained) parses and DTOs where this is O(limit).
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
                let mut dto = crate::model::block_to_shallow_dto(winner.block);
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
fn execute_blocks(
    plan: &QueryPlan,
    graph: &Graph,
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

#[cfg(test)]
fn execute_application_blocks(
    plan: &QueryPlan,
    pages: &[ApplicationQueryPlanPage],
    branch: &QueryBranch,
    cancelled: &impl Fn() -> bool,
) -> Option<(Vec<QueryHit>, bool)> {
    if branch.limit == 0 {
        return Some((Vec::new(), false));
    }
    // Managed candidate narrowing for a fuzzy predicate happens at the CALLER,
    // which owns the materialized index and decides whether the accepted
    // frontier is the whole story; by the time pages reach here they are
    // already the narrowed set. See `SyncRuntimeActor::application_query_plan_ready`.
    execute_block_candidates(
        plan,
        pages
            .iter()
            .map(|source| (&source.entry, source.roots.as_slice())),
        branch,
        cancelled,
    )
}

/// Convert typed block hits back to the exact grouped shape used by existing
/// search/query consumers. Hits arrive in global relevance order; only contiguous
/// hits from the same page are coalesced, so flattening the groups preserves that
/// order even when a page appears in more than one group.
pub(crate) fn block_hits_to_groups(hits: Vec<QueryHit>) -> Vec<crate::model::RefGroup> {
    let mut groups: Vec<crate::model::RefGroup> = Vec::new();
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
        groups.push(crate::model::RefGroup {
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
            QueryHit::Page { page, .. } => Some(page),
            QueryHit::Block { .. } => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn text_pred(mode: TextMatchMode, value: &str) -> TextPredicate {
        TextPredicate {
            clause_id: 1,
            field: TextField::PageName,
            mode,
            value: value.into(),
        }
    }

    fn fixture() -> (PathBuf, Graph) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("tine-query-plan-{}-{nonce}", std::process::id()));
        fs::create_dir_all(dir.join("pages")).unwrap();
        fs::create_dir_all(dir.join("journals")).unwrap();
        fs::create_dir_all(dir.join("logseq")).unwrap();
        fs::write(
            dir.join("pages").join("Opinion Diffusion.md"),
            "- Parent\n\t- 🧠 foo ready\n- foo draft\n- ready only\n- regex ABC\n",
        )
        .unwrap();
        fs::write(
            dir.join("pages").join("Opdf Notes.md"),
            "alias:: Research Hub\n\n- unrelated\n",
        )
        .unwrap();
        fs::write(dir.join("pages").join("Xopdf.md"), "- unrelated\n").unwrap();
        fs::write(
            dir.join("pages").join("References.md"),
            "- [[Virtual Opdf]]\n",
        )
        .unwrap();
        let graph = Graph::open(&dir);
        graph.warm_cache();
        (dir, graph)
    }

    /// The shared block evaluator, driven on both storage modes over literally
    /// the same content: identical results, and evidence plus result DTOs
    /// produced once per WINNER rather than once per retained candidate.
    ///
    /// The evaluation count is the point. The managed twin used to call
    /// `eval_ranked_block_expr` and build a `BlockDto` inside the walk for
    /// every candidate the heap retained, so a `limit`-bounded search over a
    /// large page set did O(retained) work where Direct did O(limit). Asserting
    /// the count is what turns "they share an evaluator now" from a comment
    /// into a test.
    #[test]
    fn application_block_evaluator_matches_direct_and_evaluates_evidence_per_winner() {
        const PAGES: usize = 5;
        const BLOCKS: usize = 20;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "tine-query-plan-modes-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(dir.join("pages")).unwrap();
        fs::create_dir_all(dir.join("journals")).unwrap();
        fs::create_dir_all(dir.join("logseq")).unwrap();
        for page in 0..PAGES {
            let mut content = String::new();
            for block in 0..BLOCKS {
                content.push_str(&format!(
                    "- parent {page}-{block} needle here
"
                ));
                content.push_str(&format!(
                    "	- child {page}-{block} needle nested
"
                ));
            }
            fs::write(dir.join("pages").join(format!("Mode-{page}.md")), content).unwrap();
        }
        let graph = Graph::open(&dir);
        graph.warm_cache();
        let entries = graph.list_pages();
        let pages = graph.with_pages(|pages| {
            pages
                .iter()
                .map(|(entry, doc)| ApplicationQueryPlanPage {
                    entry: entry.clone(),
                    // Cloning a `DocBlock` resets its memoized projection, so
                    // the managed side genuinely starts cold here rather than
                    // borrowing Direct's warm cache.
                    roots: std::sync::Arc::new(doc.roots.clone()),
                })
                .collect::<Vec<_>>()
        });
        let matching = PAGES * BLOCKS * 2;
        for limit in [1_usize, 3, 7, 50, 1_000] {
            let plan = QueryPlan::block_search("needle", limit);
            let _ = take_block_evidence_evaluations();
            let direct = plan.execute(&graph, || false);
            let direct_evaluations = take_block_evidence_evaluations();
            let managed = plan.execute_application_with_explain(
                entries.clone(),
                &pages,
                Vec::new(),
                Vec::new(),
                || false,
                true,
            );
            let managed_evaluations = take_block_evidence_evaluations();
            assert_eq!(
                serde_json::to_value(&managed).unwrap(),
                serde_json::to_value(&direct).unwrap(),
                "managed block evaluation diverged from Direct Files at limit={limit}"
            );
            assert_eq!(
                direct_evaluations, managed_evaluations,
                "the two modes must do the same amount of evidence work at limit={limit}"
            );
            assert_eq!(
                managed_evaluations,
                limit.min(matching),
                "evidence and DTOs must be produced once per winner, not once per retained candidate, at limit={limit}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    fn block_fingerprint(groups: Vec<crate::model::RefGroup>) -> Vec<(String, String)> {
        groups
            .into_iter()
            .flat_map(|group| {
                group
                    .blocks
                    .into_iter()
                    .map(move |block| (group.page.clone(), block.raw))
            })
            .collect()
    }

    fn reference_literal_search(graph: &Graph, query: &str, limit: usize) -> Vec<(String, String)> {
        if limit == 0 || query.is_empty() {
            return Vec::new();
        }
        let query = canonical_fold(query);
        graph.with_pages(|pages| {
            let mut out = Vec::new();
            fn visit(
                page: &str,
                blocks: &[DocBlock],
                query: &str,
                remaining: &mut usize,
                out: &mut Vec<(String, String)>,
            ) {
                for block in blocks {
                    if *remaining == 0 {
                        return;
                    }
                    let projection = block.projection();
                    if fuzzy_name_score(&projection.visible_lower, query).is_some() {
                        out.push((page.to_string(), block.raw.clone()));
                        *remaining -= 1;
                    }
                    visit(page, &block.children, query, remaining, out);
                }
            }
            let mut remaining = limit;
            for (entry, document) in pages {
                visit(
                    &entry.name,
                    &document.roots,
                    &query,
                    &mut remaining,
                    &mut out,
                );
                if remaining == 0 {
                    break;
                }
            }
            out
        })
    }

    #[test]
    fn friendly_simple_term_is_fuzzy_only_for_page_names() {
        let plan = QueryPlan::friendly("opdf", 8, 50);
        assert_eq!(plan.branches.len(), 2);
        assert!(matches!(
            &plan.branches[0].predicate,
            QueryExpr::Text(TextPredicate {
                field: TextField::PageName,
                mode: TextMatchMode::Fuzzy,
                value,
                ..
            }) if value == "opdf"
        ));
        assert!(matches!(
            &plan.branches[1].predicate,
            QueryExpr::Text(TextPredicate {
                field: TextField::VisibleContent,
                mode: TextMatchMode::Contains,
                ..
            })
        ));
    }

    #[test]
    fn graph_search_reports_per_category_truncation() {
        let (dir, graph) = fixture();

        let page_truncated = crate::query_plan::QueryPlan::friendly("opdf", 2, 1)
            .execute_with_explain(&graph, || false, false);
        assert!(page_truncated.has_more.pages);
        assert!(!page_truncated.has_more.blocks);

        let block_truncated = crate::query_plan::QueryPlan::friendly("foo", 10, 1)
            .execute_with_explain(&graph, || false, false);
        assert!(!block_truncated.has_more.pages);
        assert!(block_truncated.has_more.blocks);

        let complete = crate::query_plan::QueryPlan::friendly("foo", 10, 10).execute_with_explain(
            &graph,
            || false,
            false,
        );
        assert!(!complete.has_more.pages);
        assert!(!complete.has_more.blocks);

        crate::test_support::remove_dir_all(dir);
    }

    #[test]
    fn explicit_fuzzy_subsequence_has_score_and_utf16_spans() {
        let plan = QueryPlan::page_name_fuzzy("of", 8);
        let pred = &plan.branches[0].predicate;
        let hit = eval_expr(&plan, pred, TextField::PageName, "🧠 Opinion Diffusion").unwrap();
        assert_eq!(hit.evidence[0].score, Some(100));
        assert_eq!(
            hit.evidence[0].spans,
            vec![
                MatchSpan { start: 3, end: 4 },
                MatchSpan { start: 13, end: 14 }
            ]
        );
    }

    #[test]
    fn unicode_substring_and_regex_spans_use_utf16_units() {
        let contains = QueryPlan::page_name_fuzzy("foo", 8);
        let hit = eval_expr(
            &contains,
            &contains.branches[0].predicate,
            TextField::PageName,
            "🧠 foo",
        )
        .unwrap();
        assert_eq!(hit.evidence[0].spans, vec![MatchSpan { start: 3, end: 6 }]);

        let regex_plan = QueryPlan::friendly("/foo/", 8, 8);
        let page = &regex_plan.branches[0].predicate;
        let hit = eval_expr(&regex_plan, page, TextField::PageName, "🧠 foo").unwrap();
        assert_eq!(hit.evidence[0].spans, vec![MatchSpan { start: 3, end: 6 }]);
    }

    #[test]
    fn canonical_unicode_matches_pages_aliases_blocks_and_original_utf16_spans() {
        let multiword = QueryPlan::friendly("Canonical page", 8, 8);
        let multiword_page = best_page_match(
            &multiword,
            &multiword.branches[0].predicate,
            "Canonical page",
            &[],
        )
        .unwrap();
        assert_eq!(multiword_page.1, ObjectiveMatchClass::Exact);

        let syntax = QueryPlan::friendly("foo -draft", 8, 8);
        assert!(
            best_page_match(&syntax, &syntax.branches[0].predicate, "foo -draft", &[],).is_none()
        );
        assert!(
            best_page_match(&syntax, &syntax.branches[0].predicate, "foo ready", &[],).is_some()
        );

        let page_plan = QueryPlan::page_name_fuzzy("Café", 8);
        let page_pred = &page_plan.branches[0].predicate;
        let page = best_page_match(&page_plan, page_pred, "Cafe\u{301}", &[]).unwrap();
        assert_eq!(page.1, ObjectiveMatchClass::Exact);
        let page_evidence =
            eval_expr(&page_plan, page_pred, TextField::PageName, "Cafe\u{301}").unwrap();
        assert_eq!(
            page_evidence.evidence[0].spans,
            vec![MatchSpan { start: 0, end: 5 }]
        );

        let alias_plan = QueryPlan::page_name_fuzzy("Résumé", 8);
        let alias = best_page_match(
            &alias_plan,
            &alias_plan.branches[0].predicate,
            "Canonical page",
            &["Re\u{301}sume\u{301}".into()],
        )
        .unwrap();
        assert_eq!(alias.1, ObjectiveMatchClass::Exact);
        assert_eq!(alias.3.as_deref(), Some("Re\u{301}sume\u{301}"));

        let block_plan = QueryPlan::block_search("Résumé", 8);
        let block_pred = &block_plan.branches[0].predicate;
        let original = "🧠 Re\u{301}sume\u{301}";
        let folded = canonical_fold(original);
        assert!(eval_expr_fast(
            &block_plan,
            block_pred,
            TextField::VisibleContent,
            original,
            &folded,
        ));
        let evidence =
            eval_expr(&block_plan, block_pred, TextField::VisibleContent, original).unwrap();
        assert_eq!(
            evidence.evidence[0].spans,
            vec![MatchSpan { start: 3, end: 11 }]
        );

        let hangul = QueryPlan::block_search("\u{ac00}", 8);
        let hit = eval_expr(
            &hangul,
            &hangul.branches[0].predicate,
            TextField::VisibleContent,
            "\u{1100}\u{1161}",
        )
        .unwrap();
        assert_eq!(hit.evidence[0].spans, vec![MatchSpan { start: 0, end: 2 }]);

        let reordered = QueryPlan::block_search("è\u{315}", 8);
        let hit = eval_expr(
            &reordered,
            &reordered.branches[0].predicate,
            TextField::VisibleContent,
            "e\u{315}\u{300}",
        )
        .unwrap();
        assert_eq!(hit.evidence[0].spans, vec![MatchSpan { start: 0, end: 3 }]);

        let negative = QueryPlan::block_search("cafe", 8);
        assert!(eval_expr(
            &negative,
            &negative.branches[0].predicate,
            TextField::VisibleContent,
            "café",
        )
        .is_none());

        let expansion = QueryPlan::block_search("i\u{307}", 8);
        let hit = eval_expr(
            &expansion,
            &expansion.branches[0].predicate,
            TextField::VisibleContent,
            "\u{130}",
        )
        .unwrap();
        assert_eq!(hit.evidence[0].spans, vec![MatchSpan { start: 0, end: 1 }]);
    }

    #[test]
    fn canonical_unicode_executes_through_real_page_alias_and_block_projections() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "tine-query-plan-unicode-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(dir.join("pages")).unwrap();
        fs::create_dir_all(dir.join("journals")).unwrap();
        fs::create_dir_all(dir.join("logseq")).unwrap();
        fs::write(
            dir.join("pages").join("Cafe\u{301}.md"),
            "alias:: Re\u{301}sume\u{301}\n\n- Re\u{301}sume\u{301}\n",
        )
        .unwrap();
        let graph = Graph::open(&dir);
        graph.warm_cache();

        let page = QueryPlan::page_name_fuzzy("Café", 8).execute(&graph, || false);
        assert!(matches!(
            page.hits.first(),
            Some(QueryHit::Page {
                page,
                match_class: ObjectiveMatchClass::Exact,
                matched_alias: None,
                evidence,
                ..
            }) if page.name == "Cafe\u{301}"
                && evidence[0].spans == vec![MatchSpan { start: 0, end: 5 }]
        ));

        let alias = QueryPlan::page_name_fuzzy("Résumé", 8).execute(&graph, || false);
        assert!(
            matches!(
                alias.hits.first(),
                Some(QueryHit::Page {
                    page,
                    match_class: ObjectiveMatchClass::Exact,
                    matched_alias: Some(name),
                    ..
                }) if page.name == "Cafe\u{301}" && name == "résumé"
            ),
            "{:#?}",
            alias.hits
        );

        let blocks = QueryPlan::block_search("Résumé", 8).execute(&graph, || false);
        assert!(matches!(
            blocks.hits.first(),
            Some(QueryHit::Block { display_text, evidence, .. })
                if display_text == "Re\u{301}sume\u{301}"
                    && evidence[0].spans == vec![MatchSpan { start: 0, end: 8 }]
        ));
        crate::test_support::remove_dir_all(dir);
    }

    #[test]
    fn page_alias_search_is_scoped_to_its_physical_owner() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "tine-query-plan-alias-owner-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(dir.join("pages").join("sub")).unwrap();
        fs::create_dir_all(dir.join("journals")).unwrap();
        fs::create_dir_all(dir.join("logseq")).unwrap();
        fs::write(
            dir.join("pages").join("Foo.md"),
            "alias:: bar\n\n- declaring page\n",
        )
        .unwrap();
        fs::write(
            dir.join("pages").join("sub").join("Foo.md"),
            "- same-named sibling\n",
        )
        .unwrap();
        fs::write(
            dir.join("pages").join("Unique.md"),
            "alias:: quux\n\n- unique alias owner\n",
        )
        .unwrap();
        let graph = Graph::open(&dir);
        graph.warm_cache();

        let alias_hits = crate::query_plan::QueryPlan::friendly("bar", 10, 0)
            .execute_with_explain(&graph, || false, false)
            .hits
            .into_iter()
            .filter_map(|hit| match hit {
                QueryHit::Page {
                    page,
                    match_class,
                    matched_alias,
                    ..
                } if !page.rel_path.is_empty() => Some((page.rel_path, match_class, matched_alias)),
                QueryHit::Page { .. } => None,
                QueryHit::Block { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            alias_hits,
            vec![(
                "pages/Foo.md".to_string(),
                ObjectiveMatchClass::Exact,
                Some("bar".to_string()),
            )],
            "a duplicate-named sibling must not inherit another file's alias"
        );

        let unique_hits = crate::query_plan::QueryPlan::friendly("quux", 10, 0)
            .execute_with_explain(&graph, || false, false)
            .hits
            .into_iter()
            .filter_map(|hit| match hit {
                QueryHit::Page {
                    page,
                    match_class,
                    matched_alias,
                    ..
                } if !page.rel_path.is_empty() => Some((page.rel_path, match_class, matched_alias)),
                QueryHit::Page { .. } => None,
                QueryHit::Block { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            unique_hits,
            vec![(
                "pages/Unique.md".to_string(),
                ObjectiveMatchClass::Exact,
                Some("quux".to_string()),
            )],
            "a unique page name must retain ordinary alias matching"
        );

        crate::test_support::remove_dir_all(dir);
    }

    #[test]
    fn alias_reference_is_never_a_phantom_alias_page() {
        // GH #353: an alias text that is also referenced anywhere in the graph
        // (`[[Book]]`) must not surface as its own selectable page — only the
        // owner page may appear, carrying the matched alias as context. Covers
        // canonical (case-folded) matching and multiple aliases.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "tine-query-plan-alias-ghost-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(dir.join("pages")).unwrap();
        fs::create_dir_all(dir.join("journals")).unwrap();
        fs::create_dir_all(dir.join("logseq")).unwrap();
        // One owner page carrying TWO aliases (`alias:: Book, Reading`), the
        // alias texts referenced elsewhere in the graph, one real page whose
        // name merely overlaps an alias, and one unrelated referenced page.
        fs::write(
            dir.join("pages").join("Research Hub.md"),
            "alias:: Book, Reading\n\n- actual reading notes\n",
        )
        .unwrap();
        fs::write(
            dir.join("pages").join("Real Reading.md"),
            "- a genuinely real page whose name contains an alias\n",
        )
        .unwrap();
        fs::write(
            dir.join("pages").join("Notes.md"),
            "- see [[Book]], [[Reading]] and [[Book Shelf]] for the list\n",
        )
        .unwrap();
        let graph = Graph::open(&dir);
        graph.warm_cache();

        for query in ["book", "BOOK", "Book", "reading"] {
            let page_hits: Vec<(String, String, Option<String>)> =
                crate::query_plan::QueryPlan::friendly(query, 100, 0)
                    .execute_with_explain(&graph, || false, false)
                    .hits
                    .into_iter()
                    .filter_map(|hit| match hit {
                        QueryHit::Page {
                            page,
                            matched_alias,
                            ..
                        } => Some((page.name, page.rel_path, matched_alias)),
                        QueryHit::Block { .. } => None,
                    })
                    .collect();
            // The exact invariant: an alias's folded text must never be a
            // path-less (referenced/virtual) page candidate. Unrelated
            // referenced pages (e.g. "Book Shelf") MAY appear — they are real.
            assert!(
                page_hits.iter().all(|(name, rel_path, _)| {
                    let folded = canonical_fold(name);
                    !rel_path.is_empty() || (folded != "book" && folded != "reading")
                }),
                "query {query:?} must not offer a path-less phantom alias page: {page_hits:?}"
            );
            assert!(
                page_hits
                    .iter()
                    .any(|(name, _, matched_alias)| name == "Research Hub"
                        && matched_alias.is_some()),
                "query {query:?} must carry the matched alias on the owner hit: {page_hits:?}"
            );
            // An ordinary page whose name merely contains the alias text is
            // not affected by the alias-owner dedup ("Real Reading" still
            // shows up for "reading"), and only the owner carries the alias.
            if query == "reading" {
                assert!(
                    page_hits.iter().any(|(name, _, matched_alias)| {
                        name == "Real Reading" && matched_alias.is_none()
                    }),
                    "an ordinary partial name match stays a hit: {page_hits:?}"
                );
            }
            // A genuinely unrelated referenced page is NOT affected.
            let shelf: Vec<String> = crate::query_plan::QueryPlan::friendly("Book Shelf", 100, 0)
                .execute_with_explain(&graph, || false, false)
                .hits
                .into_iter()
                .filter_map(|hit| match hit {
                    QueryHit::Page { page, .. } => Some(page.name),
                    QueryHit::Block { .. } => None,
                })
                .collect();
            assert!(
                shelf.contains(&"Book Shelf".to_string()),
                "a referenced page with no alias owner still surfaces: {shelf:?}"
            );
            // The legacy quick-switch pool (the `#` / `[[` autocomplete source)
            // shares the same candidate boundary.
            let switch_names: Vec<String> = graph
                .quick_switch(query, 100)
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            assert!(
                !switch_names
                    .iter()
                    .any(|name| canonical_fold(name) == "book" || canonical_fold(name) == "reading"),
                "quick_switch must not offer the alias-named phantom: {switch_names:?}"
            );
        }

        crate::test_support::remove_dir_all(dir);
    }

    #[test]
    fn boolean_evidence_uses_positive_terms_and_first_matching_or_branch() {
        let plan = QueryPlan::friendly("foo -draft OR ready", 8, 8);
        let blocks = &plan.branches[1].predicate;
        let first = eval_expr(&plan, blocks, TextField::VisibleContent, "foo ship").unwrap();
        assert_eq!(first.evidence.len(), 1);
        assert_eq!(first.evidence[0].mode, TextMatchMode::Contains);
        assert!(eval_expr(&plan, blocks, TextField::VisibleContent, "foo draft").is_none());
        let second = eval_expr(&plan, blocks, TextField::VisibleContent, "ready draft").unwrap();
        assert_eq!(second.evidence.len(), 1);
        assert_ne!(first.evidence[0].clause_id, second.evidence[0].clause_id);
    }

    #[test]
    fn invalid_regex_is_a_rust_diagnostic_and_matches_nothing() {
        let plan = QueryPlan::friendly("/(unclosed/", 8, 8);
        assert!(plan.branches.is_empty());
        assert_eq!(plan.diagnostics.len(), 1);
        assert_eq!(plan.diagnostics[0].code, "invalid_regex");
        assert_eq!(
            plan.diagnostics[0].span,
            Some(MatchSpan { start: 0, end: 11 })
        );
    }

    #[test]
    fn zero_limit_friendly_plans_still_classify_saveable_sources() {
        let rust_only_invalid = QueryPlan::friendly("/(a)\\1/", 0, 0);
        assert!(rust_only_invalid.branches.is_empty());
        assert_eq!(
            rust_only_invalid
                .diagnostics
                .first()
                .map(|item| item.code.as_str()),
            Some("invalid_regex")
        );
        let valid = QueryPlan::friendly("alpha", 0, 0);
        assert!(!valid.branches.is_empty());
        assert!(valid.branches.iter().all(|branch| branch.limit == 0));
        let excluded = QueryPlan::friendly("-draft", 0, 0);
        assert!(excluded.diagnostics.is_empty() && excluded.branches.is_empty());
    }

    #[test]
    fn pure_negation_and_empty_friendly_search_have_no_branches() {
        assert!(QueryPlan::friendly("-draft", 8, 8).branches.is_empty());
        assert!(QueryPlan::friendly("", 8, 8).branches.is_empty());
        assert_eq!(QueryPlan::legacy_page_search("-draft", 8).branches.len(), 1);
    }

    #[test]
    fn matching_modes_are_explicit() {
        let pred = text_pred(TextMatchMode::Fuzzy, "abc");
        assert_eq!(pred.mode, TextMatchMode::Fuzzy);
        let plan = QueryPlan::friendly("\"exact phrase\"", 8, 8);
        assert!(matches!(
            &plan.branches[1].predicate,
            QueryExpr::Text(TextPredicate {
                mode: TextMatchMode::Phrase,
                ..
            })
        ));
    }

    #[test]
    fn combined_execution_returns_typed_ranked_hits_and_exact_evidence_text() {
        let (dir, graph) = fixture();
        let pages = QueryPlan::page_name_fuzzy("opdf", 10).execute(&graph, || false);
        let names = pages
            .hits
            .iter()
            .filter_map(|hit| match hit {
                QueryHit::Page { page, .. } => Some(page.name.as_str()),
                QueryHit::Block { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(names.first().copied(), Some("Opdf Notes"));
        assert!(
            names.iter().position(|name| *name == "Xopdf")
                < names.iter().position(|name| *name == "Opinion Diffusion")
        );
        let file_hit = pages.hits.iter().find_map(|hit| match hit {
            QueryHit::Page { page, .. } if page.name == "Opdf Notes" => Some(page),
            _ => None,
        });
        assert_eq!(file_hit.unwrap().rel_path, "pages/Opdf Notes.md");
        let virtual_hit = pages.hits.iter().find_map(|hit| match hit {
            QueryHit::Page { page, .. } if page.name == "Virtual Opdf" => Some(page),
            _ => None,
        });
        assert_eq!(virtual_hit.unwrap().rel_path, "");

        let execution = crate::query_plan::QueryPlan::friendly("foo -draft OR ready", 10, 10)
            .execute_with_explain(&graph, || false, true);
        let blocks = execution
            .hits
            .iter()
            .filter_map(|hit| match hit {
                QueryHit::Block {
                    block,
                    display_text,
                    evidence,
                    ..
                } => Some((block, display_text, evidence)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(blocks.len(), 2);
        let nested = blocks
            .iter()
            .find(|(_, text, _)| text.as_str() == "🧠 foo ready")
            .unwrap();
        assert_eq!(nested.0.breadcrumb, vec!["Parent"]);
        assert_eq!(nested.2.len(), 1, "successful NOT has no positive evidence");
        assert_eq!(nested.2[0].spans, vec![MatchSpan { start: 3, end: 6 }]);
        assert!(!execution.explanation.branches.is_empty());
        let explanation = serde_json::to_string(&execution.explanation).unwrap();
        assert!(explanation.contains("Block text"));
        assert!(explanation.contains("contains “foo”"));
        assert!(!explanation.contains("PageName"));
        assert!(!explanation.contains("VisibleContent"));

        let no_explain = crate::query_plan::QueryPlan::friendly("foo", 10, 10)
            .execute_with_explain(&graph, || false, false);
        assert!(no_explain.explanation.branches.is_empty());
        crate::test_support::remove_dir_all(dir);
    }

    #[test]
    fn current_page_scope_is_block_only_and_path_authoritative() {
        let (dir, graph) = fixture();
        fs::create_dir_all(dir.join("pages").join("duplicate")).unwrap();
        fs::write(
            dir.join("pages")
                .join("duplicate")
                .join("Opinion Diffusion.md"),
            "- duplicate foo\n",
        )
        .unwrap();
        graph.warm_cache();

        let execution = QueryPlan::friendly_for_page(
            "foo",
            50,
            QueryPageScope {
                name: "Opinion Diffusion".into(),
                page_kind: PageKind::Page,
                path: Some("pages/Opinion Diffusion.md".into()),
            },
        )
        .execute(&graph, || false);
        assert!(!execution.hits.is_empty());
        assert!(execution.hits.iter().all(|hit| matches!(
            hit,
            QueryHit::Block { page, path, block, .. }
                if page == "Opinion Diffusion"
                    && path == "pages/Opinion Diffusion.md"
                    && block.raw != "duplicate foo"
        )));
        crate::test_support::remove_dir_all(dir);
    }

    #[test]
    fn page_hits_expose_objective_classes_and_alias_evidence() {
        let (dir, graph) = fixture();
        let exact = QueryPlan::page_name_fuzzy("Opdf Notes", 10).execute(&graph, || false);
        assert!(matches!(
            exact.hits.first(),
            Some(QueryHit::Page {
                match_class: ObjectiveMatchClass::Exact,
                matched_alias: None,
                ..
            })
        ));

        let alias = QueryPlan::page_name_fuzzy("Research Hub", 10).execute(&graph, || false);
        let hit = alias
            .hits
            .iter()
            .find(|hit| matches!(hit, QueryHit::Page { page, .. } if page.name == "Opdf Notes"));
        assert!(matches!(
            hit,
            Some(QueryHit::Page {
                display_text,
                match_class: ObjectiveMatchClass::Exact,
                matched_alias: Some(matched_alias),
                ..
            }) if display_text == "research hub" && matched_alias == "research hub"
        ));
        crate::test_support::remove_dir_all(dir);
    }

    #[test]
    fn regex_evidence_is_authoritative_and_bounded_to_projected_text() {
        let (dir, graph) = fixture();
        let execution = crate::query_plan::QueryPlan::friendly("/[A-Z]{3}/", 10, 10)
            .execute_with_explain(&graph, || false, true);
        let (text, evidence) = execution
            .hits
            .iter()
            .find_map(|hit| match hit {
                QueryHit::Block {
                    display_text,
                    evidence,
                    ..
                } if display_text == "regex ABC" => Some((display_text, evidence)),
                _ => None,
            })
            .unwrap();
        assert_eq!(text, "regex ABC");
        assert_eq!(evidence[0].spans, vec![MatchSpan { start: 6, end: 9 }]);
        crate::test_support::remove_dir_all(dir);
    }

    #[test]
    fn literal_block_search_adapter_preserves_fuzzy_membership_and_ranked_topk() {
        let (dir, graph) = fixture();
        for query in [
            "",
            "foo",
            "foo ready",
            "foo OR ready",
            "foo -draft",
            "-draft",
            "\"foo ready\"",
            "/[A-Z]{3}/",
            "/(unclosed/",
        ] {
            let full = block_fingerprint(crate::query::search_cancellable(
                &graph,
                query,
                usize::MAX,
                || false,
            ));
            let mut full_membership = full.clone();
            full_membership.sort();
            let mut reference = reference_literal_search(&graph, query, usize::MAX);
            reference.sort();
            assert_eq!(full_membership, reference, "query={query:?}");
            for limit in [0, 1, 2, 20] {
                assert_eq!(
                    block_fingerprint(crate::query::search_cancellable(
                        &graph,
                        query,
                        limit,
                        || false
                    )),
                    full.iter().take(limit).cloned().collect::<Vec<_>>(),
                    "query={query:?} limit={limit}"
                );
            }
        }
        crate::test_support::remove_dir_all(dir);
    }

    #[test]
    fn query_hit_json_contract_uses_tagged_entities_and_utf16_evidence() {
        let hit = QueryHit::Page {
            page: PageEntry {
                name: "🧠 Foo".into(),
                kind: PageKind::Page,
                date_key: None,
                rel_path: "pages/Foo.md".into(),
                path: std::path::PathBuf::new(),
            },
            display_text: "🧠 Foo".into(),
            evidence: vec![MatchEvidence {
                clause_id: 7,
                field: TextField::PageName,
                mode: TextMatchMode::Fuzzy,
                spans: vec![MatchSpan { start: 3, end: 6 }],
                score: None,
            }],
            score: 1_000,
            match_class: ObjectiveMatchClass::Prefix,
            matched_alias: None,
            // Additive and absent: the wire shape of a hit that carries no
            // hydrated row is EXACTLY the shape it had before this field.
            row: None,
        };

        assert_eq!(
            serde_json::to_value(hit).unwrap(),
            serde_json::json!({
                "entity": "page",
                "page": {
                    "name": "🧠 Foo",
                    "kind": "page",
                    "date_key": null,
                    "path": "pages/Foo.md"
                },
                "display_text": "🧠 Foo",
                "evidence": [{
                    "clause_id": 7,
                    "field": "page_name",
                    "mode": "fuzzy",
                    "spans": [{"start": 3, "end": 6}]
                }],
                "score": 1_000,
                "match_class": "prefix"
            })
        );
    }

    #[test]
    fn cancellation_discards_all_partial_combined_results() {
        let (dir, graph) = fixture();
        let checks = Cell::new(0usize);
        let execution = QueryPlan::friendly("unrelated", 10, 10).execute(&graph, || {
            checks.set(checks.get() + 1);
            checks.get() > 3
        });
        assert!(execution.cancelled);
        assert!(execution.hits.is_empty());
        crate::test_support::remove_dir_all(dir);
    }

    // -----------------------------------------------------------------
    // Page text rank bridge: owner-local name/alias choice stays distinct
    // from the global page ordering consumed by SQLite.
    // -----------------------------------------------------------------

    fn page_branch(plan: &QueryPlan) -> Option<&QueryBranch> {
        plan.branches
            .iter()
            .find(|branch| branch.target == QueryTarget::Pages)
    }

    fn bridge_page_choice<'a>(
        plan: &QueryPlan,
        branch: &QueryBranch,
        name: &'a str,
        aliases: &'a [&'a str],
    ) -> Option<(PageTextRank, &'a str, Option<&'a str>)> {
        let mut best = rank_page_text(plan, branch, name).map(|rank| (rank, name, None));
        for &alias in aliases {
            let Some(rank) = rank_page_text(plan, branch, alias) else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|(current, _, _)| rank.is_better_owner_choice_than(current))
            {
                best = Some((rank, alias, Some(alias)));
            }
        }
        best
    }

    // Independent pre-extraction owner-selection oracle from a6f49357. This
    // deliberately does not call PageTextRank or its owner comparator.
    fn legacy_page_choice(
        plan: &QueryPlan,
        expr: &QueryExpr,
        page_name: &str,
        aliases: &[String],
    ) -> Option<(i32, ObjectiveMatchClass, String, Option<String>)> {
        let page_match = page_base_score(plan, expr, page_name, &canonical_fold(page_name));
        let mut best = page_match.map(|(score, class)| (score, class, page_name.to_string(), None));
        for alias in aliases {
            let Some((score, class)) = page_base_score(plan, expr, alias, &canonical_fold(alias))
            else {
                continue;
            };
            let replace = best.as_ref().is_none_or(|(best_score, best_class, _, _)| {
                class.rank() > best_class.rank() || (class == *best_class && score > *best_score)
            });
            if replace {
                best = Some((score, class, alias.clone(), Some(alias.clone())));
            }
        }
        // Upgrade only an outcome that already satisfied the parsed expression.
        // This repairs the objective class for ordinary multi-word titles without
        // bypassing NOT/OR/regex membership semantics for syntax-looking names.
        if let Some(exact) = plan.page_exact.as_deref() {
            if page_match.is_some() && canonical_fold(page_name) == exact {
                return Some((
                    1500,
                    ObjectiveMatchClass::Exact,
                    page_name.to_string(),
                    None,
                ));
            }
            if let Some(alias) = aliases.iter().find(|alias| {
                canonical_fold(alias) == exact
                    && page_base_score(plan, expr, alias, &canonical_fold(alias)).is_some()
            }) {
                return Some((
                    1500,
                    ObjectiveMatchClass::Exact,
                    alias.clone(),
                    Some(alias.clone()),
                ));
            }
        }
        best
    }

    #[test]
    fn page_rank_bridge_matches_best_page_match_across_compiled_shapes() {
        let cases = [
            (
                QueryPlan::page_name_fuzzy("opdf", 8),
                "Opdf Notes",
                vec!["Research Hub"],
            ),
            (
                QueryPlan::friendly("foo ready", 8, 8),
                "foo ready notes",
                vec!["unrelated"],
            ),
            (
                QueryPlan::friendly("zzz OR ready", 8, 8),
                "ready page",
                vec!["zzz alias"],
            ),
            (
                QueryPlan::friendly("foo -draft", 8, 8),
                "foo ready",
                vec!["foo draft"],
            ),
            (
                QueryPlan::friendly("/A[BC]+/", 8, 8),
                "regex ABC",
                vec!["regex ACC"],
            ),
            (
                QueryPlan::friendly("foo -draft", 8, 8),
                "foo -draft",
                vec![],
            ),
        ];
        for (plan, name, aliases) in &cases {
            let branch = page_branch(plan).expect("the case must plan a page branch");
            let owned_aliases = aliases
                .iter()
                .map(|alias| (*alias).to_string())
                .collect::<Vec<_>>();
            let expected = legacy_page_choice(plan, &branch.predicate, name, &owned_aliases);
            let actual = bridge_page_choice(plan, branch, name, aliases);
            assert_eq!(actual.is_some(), expected.is_some(), "name={name:?}");
            if let (
                Some((rank, text, alias)),
                Some((score, class, expected_text, expected_alias)),
            ) = (actual, expected)
            {
                assert_eq!((rank.base_score(), rank.match_class()), (score, class));
                assert_eq!(text, expected_text);
                assert_eq!(alias.map(str::to_string), expected_alias);
            }
        }

        assert!(QueryPlan::friendly("", 8, 8).branches.is_empty());
        assert!(QueryPlan::friendly("/(unclosed/", 8, 8).branches.is_empty());
    }

    #[test]
    fn page_rank_bridge_owner_choice_keeps_exact_override_and_stable_equal_ties_separate() {
        let override_plan = QueryPlan::friendly("foo OR bar", 8, 8);
        let override_branch = page_branch(&override_plan).unwrap();
        let name = rank_page_text(&override_plan, override_branch, "foo").unwrap();
        let alias = rank_page_text(&override_plan, override_branch, "foo OR bar").unwrap();
        assert_eq!(
            (name.base_score(), name.match_class()),
            (1500, ObjectiveMatchClass::Exact)
        );
        assert_eq!(
            (alias.base_score(), alias.match_class()),
            (1500, ObjectiveMatchClass::Exact)
        );
        assert!(!name.is_exact_override());
        assert!(alias.is_exact_override());
        assert!(alias.is_better_owner_choice_than(&name));
        assert_eq!(
            alias.global_order_key("Owner"),
            name.global_order_key("Owner")
        );
        let selected =
            bridge_page_choice(&override_plan, override_branch, "foo", &["foo OR bar"]).unwrap();
        assert_eq!((selected.1, selected.2), ("foo OR bar", Some("foo OR bar")));

        let equal_plan = QueryPlan::page_name_fuzzy("foo", 8);
        let equal_branch = page_branch(&equal_plan).unwrap();
        let name_wins =
            bridge_page_choice(&equal_plan, equal_branch, "foo name", &["foo alias"]).unwrap();
        assert_eq!((name_wins.1, name_wins.2), ("foo name", None));
        let first_alias_wins = bridge_page_choice(
            &equal_plan,
            equal_branch,
            "unrelated",
            &["foo first", "foo later"],
        )
        .unwrap();
        assert_eq!(
            (first_alias_wins.1, first_alias_wins.2),
            ("foo first", Some("foo first"))
        );
    }

    #[test]
    fn page_owner_rank_blob_matches_the_existing_strict_comparator() {
        let classes = [
            ObjectiveMatchClass::Exact,
            ObjectiveMatchClass::Prefix,
            ObjectiveMatchClass::Substring,
            ObjectiveMatchClass::Fuzzy,
            ObjectiveMatchClass::BodyEvidence,
        ];
        let ranks = [false, true].into_iter().flat_map(|exact_override| {
            classes.into_iter().flat_map(move |match_class| {
                [i32::MIN, -1, 0, 1, i32::MAX]
                    .into_iter()
                    .map(move |base_score| PageTextRank {
                        base_score,
                        match_class,
                        exact_override,
                    })
            })
        });
        let ranks = ranks.collect::<Vec<_>>();
        for left in &ranks {
            for right in &ranks {
                assert_eq!(
                    left.owner_order_key() < right.owner_order_key(),
                    left.is_better_owner_choice_than(right)
                );
                assert_eq!(
                    left.owner_order_key() == right.owner_order_key(),
                    !left.is_better_owner_choice_than(right)
                        && !right.is_better_owner_choice_than(left)
                );
            }
        }
    }

    #[test]
    fn page_rank_bridge_blob_matches_scored_page_class_then_signed_score_order() {
        let classes = [
            ObjectiveMatchClass::Exact,
            ObjectiveMatchClass::Prefix,
            ObjectiveMatchClass::Substring,
            ObjectiveMatchClass::Fuzzy,
            ObjectiveMatchClass::BodyEvidence,
        ];
        let scores = [i32::MIN, -1, 0, 1, i32::MAX];
        let ranks = classes
            .into_iter()
            .flat_map(|match_class| {
                scores.into_iter().map(move |base_score| PageTextRank {
                    base_score,
                    match_class,
                    exact_override: false,
                })
            })
            .collect::<Vec<_>>();
        for left in &ranks {
            for right in &ranks {
                let left_page = ScoredPage {
                    score: left.base_score,
                    match_class: left.match_class,
                    matched_text: String::new(),
                    matched_alias: None,
                    tie_key: String::new(),
                    candidate: PageCandidate::Referenced(PageEntry {
                        name: String::new(),
                        kind: PageKind::Page,
                        date_key: None,
                        rel_path: String::new(),
                        path: PathBuf::new(),
                    }),
                };
                let right_page = ScoredPage {
                    score: right.base_score,
                    match_class: right.match_class,
                    matched_text: String::new(),
                    matched_alias: None,
                    tie_key: String::new(),
                    candidate: PageCandidate::Referenced(PageEntry {
                        name: String::new(),
                        kind: PageKind::Page,
                        date_key: None,
                        rel_path: String::new(),
                        path: PathBuf::new(),
                    }),
                };
                assert_eq!(
                    left.global_order_key("").cmp(&right.global_order_key("")),
                    right
                        .match_class
                        .rank()
                        .cmp(&left.match_class.rank())
                        .then_with(|| right.base_score.cmp(&left.base_score))
                );
                assert_eq!(
                    left.global_order_key("") < right.global_order_key(""),
                    left_page.is_better_than(&right_page)
                );
            }
        }
    }

    #[test]
    fn page_rank_bridge_rank_only_builds_no_evidence_and_admitted_evidence_is_utf16_exact() {
        let plan = QueryPlan::page_name_fuzzy("caf\u{e9}", 8);
        let branch = page_branch(&plan).unwrap();
        let text = "\u{1D11E} CAFE\u{301}";
        TEXT_EVIDENCE_EVALUATIONS.with(|count| count.set(0));
        let rank = rank_page_text(&plan, branch, text).expect("canonical fold must match");
        assert_eq!(rank.match_class(), ObjectiveMatchClass::Substring);
        TEXT_EVIDENCE_EVALUATIONS
            .with(|count| assert_eq!(count.get(), 0, "page rank selection must produce no spans"));
        let evidence = admitted_page_evidence(&plan, branch, text).unwrap();
        TEXT_EVIDENCE_EVALUATIONS
            .with(|count| assert!(count.get() > 0, "the actual span producer must be observed"));
        assert_eq!(evidence[0].spans, vec![MatchSpan { start: 3, end: 8 }]);
        assert_eq!(evidence[0].field, TextField::PageName);

        let block = plan
            .branches
            .iter()
            .find(|candidate| candidate.target == QueryTarget::Blocks);
        assert!(block.is_none(), "page-only plan has no block branch");
        let mixed = QueryPlan::friendly("caf\u{e9}", 8, 8);
        let block = mixed
            .branches
            .iter()
            .find(|candidate| candidate.target == QueryTarget::Blocks)
            .unwrap();
        assert!(rank_page_text(&mixed, block, text).is_none());
        assert!(admitted_page_evidence(&mixed, block, text).is_none());
    }

    #[test]
    fn page_rank_bridge_handles_zero_limits_actual_alias_hits_and_virtual_names() {
        let zero = QueryPlan::friendly("opdf", 0, 0);
        let zero_branch = page_branch(&zero).unwrap();
        assert!(rank_page_text(&zero, zero_branch, "Opdf Notes").is_some());

        let (dir, graph) = fixture();
        let plan = QueryPlan::friendly("Research Hub", 10, 0);
        let branch = page_branch(&plan).unwrap();
        let alias_rank = rank_page_text(&plan, branch, "research hub").unwrap();
        let alias_hit = plan
            .execute(&graph, || false)
            .hits
            .into_iter()
            .find_map(|hit| match hit {
                QueryHit::Page {
                    page,
                    display_text,
                    score,
                    match_class,
                    matched_alias,
                    ..
                } if page.name == "Opdf Notes" => {
                    Some((page, display_text, score, match_class, matched_alias))
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(alias_hit.1, "research hub");
        assert_eq!(alias_hit.2, alias_rank.global_score(&alias_hit.0.name));
        assert_eq!(alias_hit.3, alias_rank.match_class());
        assert_eq!(alias_hit.4.as_deref(), Some("research hub"));

        let virtual_plan = QueryPlan::friendly("Virtual Opdf", 10, 0);
        let virtual_branch = page_branch(&virtual_plan).unwrap();
        let virtual_rank = rank_page_text(&virtual_plan, virtual_branch, "Virtual Opdf").unwrap();
        let virtual_hit = virtual_plan
            .execute(&graph, || false)
            .hits
            .into_iter()
            .find_map(|hit| match hit {
                QueryHit::Page {
                    page,
                    score,
                    match_class,
                    ..
                } if page.name == "Virtual Opdf" => Some((page, score, match_class)),
                _ => None,
            })
            .unwrap();
        assert!(virtual_hit.0.rel_path.is_empty());
        assert_eq!(
            virtual_hit.1,
            virtual_rank.global_score(&virtual_hit.0.name)
        );
        assert_eq!(virtual_hit.2, virtual_rank.match_class());
        crate::test_support::remove_dir_all(dir);
    }

    // -----------------------------------------------------------------
    // Block text rank bridge: the text-only seam a SQLite adapter binds
    // its ORDER BY to. Ranking authority stays here; the adapter only
    // sorts the bytes and supplies the equal-rank tie breakers.
    // -----------------------------------------------------------------

    fn relevance(
        match_class: ObjectiveMatchClass,
        word_boundary: bool,
        first_offset: usize,
        text_len: usize,
        occurrences: usize,
    ) -> BlockRelevance {
        BlockRelevance {
            match_class,
            word_boundary,
            first_offset,
            text_len,
            occurrences,
            positive: true,
        }
    }

    fn block_branch(plan: &QueryPlan) -> Option<&QueryBranch> {
        plan.branches
            .iter()
            .find(|branch| branch.target == QueryTarget::Blocks)
    }

    /// Read the five components back out of the key, proving the encoding is
    /// lossless rather than a hash of the tuple.
    fn decode_rank_key(key: &[u8; BLOCK_RANK_KEY_LEN]) -> (i32, bool, usize, usize, usize) {
        let class_rank =
            ((!u32::from_be_bytes(key[0..4].try_into().unwrap())) ^ (1u32 << 31)) as i32;
        let number = |at: usize| {
            usize::try_from(u64::from_be_bytes(key[at..at + 8].try_into().unwrap())).unwrap()
        };
        (class_rank, key[4] == 0, number(5), number(13), number(21))
    }

    /// Fail-before gate. `score()` saturates offsets, lengths and occurrence
    /// counts, so two objectively UNEQUAL tuples collide on it; ordering by the
    /// display score would make their order arbitrary. The BLOB key must still
    /// separate them, in the direction `cmp_quality` chose.
    #[test]
    fn rank_blob_separates_large_tuples_whose_display_score_collides() {
        let collisions = [
            // Both offsets are past the 50_000 penalty clamp.
            (
                relevance(ObjectiveMatchClass::Exact, true, 60_000, 0, 1),
                relevance(ObjectiveMatchClass::Exact, true, 70_000, 0, 1),
            ),
            // Both lengths are past the 40_000 clamp.
            (
                relevance(ObjectiveMatchClass::Substring, false, 7, 40_001, 3),
                relevance(ObjectiveMatchClass::Substring, false, 7, 900_000, 3),
            ),
            // Both occurrence counts are past the 9_999 clamp, and the existing
            // rule that FEWER occurrences win is preserved.
            (
                relevance(ObjectiveMatchClass::Prefix, true, 0, 10, 10_001),
                relevance(ObjectiveMatchClass::Prefix, true, 0, 10, 25_000),
            ),
            // The extreme: every component of the worse tuple is `usize::MAX`
            // and every one of them still collapses into the same clamp.
            (
                relevance(ObjectiveMatchClass::Fuzzy, true, 50_001, 40_001, 10_000),
                relevance(
                    ObjectiveMatchClass::Fuzzy,
                    true,
                    usize::MAX,
                    usize::MAX,
                    usize::MAX,
                ),
            ),
        ];
        for (better, worse) in collisions {
            assert_eq!(
                better.score(),
                worse.score(),
                "the display score must actually collide for this gate to mean anything"
            );
            assert_eq!(better.cmp_quality(&worse), Ordering::Greater);
            assert!(
                better.order_key() < worse.order_key(),
                "ascending BLOB order must still put the better tuple first"
            );
        }

        // Sorting by the key alone, with no access to the tuple, recovers the
        // order `cmp_quality` intended.
        for (better, worse) in collisions {
            let mut keys = [worse.order_key(), better.order_key()];
            keys.sort();
            assert_eq!(keys, [better.order_key(), worse.order_key()]);
        }
    }

    /// The BLOB order is EXACTLY `cmp_quality` reversed at every component
    /// boundary -- both saturation cliffs and `usize::MAX` -- in both
    /// directions and on equality.
    #[test]
    fn rank_blob_order_is_cmp_quality_reversed_at_every_component_boundary() {
        let classes = [
            ObjectiveMatchClass::Exact,
            ObjectiveMatchClass::Prefix,
            ObjectiveMatchClass::Substring,
            ObjectiveMatchClass::Fuzzy,
            ObjectiveMatchClass::BodyEvidence,
        ];
        let offsets = [0usize, 1, 50_000, 50_001, usize::MAX];
        let lengths = [0usize, 1, 40_000, 40_001, usize::MAX];
        let counts = [0usize, 1, 2, 9_999, 10_000, usize::MAX];
        let mut tuples = Vec::new();
        for class in classes {
            for word_boundary in [false, true] {
                for &first_offset in &offsets {
                    for &text_len in &lengths {
                        for &occurrences in &counts {
                            tuples.push(relevance(
                                class,
                                word_boundary,
                                first_offset,
                                text_len,
                                occurrences,
                            ));
                        }
                    }
                }
            }
        }
        assert_eq!(tuples.len(), 5 * 2 * 5 * 5 * 6);
        let keys = tuples
            .iter()
            .map(BlockRelevance::order_key)
            .collect::<Vec<_>>();
        for (left_at, left) in tuples.iter().enumerate() {
            for (right_at, right) in tuples.iter().enumerate() {
                assert_eq!(
                    keys[left_at].cmp(&keys[right_at]),
                    right.cmp_quality(left),
                    "ascending key order must be cmp_quality reversed for {left:?} vs {right:?}"
                );
            }
        }
    }

    /// The key carries every component losslessly, and carries nothing else:
    /// `positive` is a membership detail, not a `cmp_quality` component.
    #[test]
    fn rank_blob_is_lossless_and_ignores_the_non_ordering_positive_flag() {
        let sample = relevance(ObjectiveMatchClass::Substring, true, 12_345, usize::MAX, 7);
        let key = sample.order_key();
        assert_eq!(key.len(), 29);
        assert_eq!(
            decode_rank_key(&key),
            (
                ObjectiveMatchClass::Substring.rank(),
                true,
                12_345,
                usize::MAX,
                7
            )
        );

        let unbounded = relevance(ObjectiveMatchClass::Substring, false, 12_345, usize::MAX, 7);
        assert_eq!(unbounded.order_key()[4], 1);
        assert!(key < unbounded.order_key(), "a boundary match sorts first");

        let mut negated = sample;
        negated.positive = false;
        assert_eq!(negated.order_key(), key);
        assert_eq!(negated.cmp_quality(&sample), Ordering::Equal);
    }

    /// The point of a BLOB key is that the DATABASE does the sort. This binds
    /// real bridge keys and lets SQLite's own `ORDER BY ... ASC` produce the
    /// order, then checks it against the independent `cmp_quality`.
    #[test]
    fn sqlite_order_by_bound_rank_blobs_reproduces_the_ranked_order() {
        let plan = QueryPlan::friendly("ready", 8, 8);
        let branch = block_branch(&plan).expect("a bare term plans a block branch");
        let texts = [
            "ready",
            "ready only",
            "not ready yet",
            "alreadyx",
            "ready ready ready",
            "a rather long line that only mentions ready quite late in its text",
        ];

        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute(
                "CREATE TABLE ranked (label TEXT NOT NULL, rank_key BLOB NOT NULL)",
                [],
            )
            .unwrap();
        for text in texts {
            let rank = rank_block_text(&plan, branch, text)
                .unwrap_or_else(|| panic!("{text:?} must match"));
            connection
                .execute(
                    "INSERT INTO ranked (label, rank_key) VALUES (?1, ?2)",
                    rusqlite::params![text, rank.order_key().to_vec()],
                )
                .unwrap();
        }
        let mut statement = connection
            .prepare("SELECT label FROM ranked ORDER BY rank_key ASC")
            .unwrap();
        let ordered = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        let reference = |text: &str| {
            block_relevance(&plan, &branch.predicate, text, &canonical_fold(text)).unwrap()
        };
        let mut expected = texts.to_vec();
        expected.sort_by(|left, right| reference(right).cmp_quality(&reference(left)));
        assert_eq!(ordered, expected);
        // Pin the ends so an accidentally uniform key cannot pass vacuously.
        assert_eq!(ordered.first().map(String::as_str), Some("ready"));
        assert_eq!(ordered.last().map(String::as_str), Some("alreadyx"));
    }

    /// Every compiled predicate shape, driven through the bridge on text alone
    /// and through the existing evaluator on the same text.
    #[test]
    fn rank_bridge_reproduces_block_relevance_for_every_compiled_shape() {
        let texts = [
            "ready",
            "🧠 foo ready",
            "foo draft",
            "ready only",
            "regex ABC",
            "abc ready foo abc",
            "unrelated",
            "",
        ];
        let plans = [
            QueryPlan::friendly("ready", 8, 8),         // literal contains
            QueryPlan::friendly("\"foo ready\"", 8, 8), // phrase
            QueryPlan::friendly("/A[BC]+/", 8, 8),      // compiled regex
            QueryPlan::friendly("foo ready", 8, 8),     // AND
            QueryPlan::friendly("zzz OR ready", 8, 8),  // OR
            QueryPlan::friendly("foo -draft", 8, 8),    // AND with NOT
            QueryPlan::block_search("ready", 8),        // block-only plan
            QueryPlan::block_search_literal("rdy", 8),  // fuzzy subsequence
        ];
        for plan in &plans {
            let branch = block_branch(plan).expect("every plan here has a block branch");
            for text in texts {
                let expected =
                    block_relevance(plan, &branch.predicate, text, &canonical_fold(text));
                let actual = rank_block_text(plan, branch, text);
                assert_eq!(
                    actual.is_some(),
                    expected.is_some(),
                    "membership must not change for {text:?}"
                );
                if let (Some(actual), Some(expected)) = (actual, expected) {
                    assert_eq!(actual.order_key(), expected.order_key());
                    assert_eq!(actual.score(), expected.score());
                    assert_eq!(actual.match_class(), expected.match_class);
                }
            }
        }
    }

    /// The stronger check: the bridge, holding nothing but visible text, agrees
    /// with what real graph-backed execution ranked those very blocks at --
    /// including that its own `canonical_fold` reproduces the projection's
    /// cached `visible_lower`.
    #[test]
    fn rank_bridge_agrees_with_executed_block_hits_over_real_projected_text() {
        let (dir, graph) = fixture();
        for query in [
            "ready",
            "foo ready",
            "zzz OR ready",
            "foo -draft",
            "/A[BC]+/",
            "\"foo ready\"",
        ] {
            let plan = QueryPlan::friendly(query, 10, 10);
            let branch = block_branch(&plan).expect("every query here plans a block branch");
            let hits = plan
                .execute(&graph, || false)
                .hits
                .into_iter()
                .filter_map(|hit| match hit {
                    QueryHit::Block {
                        display_text,
                        score,
                        match_class,
                        ..
                    } => Some((display_text, score, match_class)),
                    QueryHit::Page { .. } => None,
                })
                .collect::<Vec<_>>();
            assert!(!hits.is_empty(), "{query} matched no block");
            let mut previous: Option<[u8; BLOCK_RANK_KEY_LEN]> = None;
            for (display_text, score, match_class) in hits {
                let rank = rank_block_text(&plan, branch, &display_text)
                    .unwrap_or_else(|| panic!("{query}: bridge rejected executed hit"));
                assert_eq!(rank.score(), score, "{query}: {display_text:?}");
                assert_eq!(rank.match_class(), match_class, "{query}: {display_text:?}");
                let key = rank.order_key();
                if let Some(previous) = previous {
                    assert!(
                        previous <= key,
                        "{query}: executed order must be non-decreasing in the BLOB key"
                    );
                }
                previous = Some(key);
            }
        }
        crate::test_support::remove_dir_all(dir);
    }

    /// Casefold + NFC + UTF-16 offsets, computed from visible text alone. The
    /// leading musical symbol is two UTF-16 units but one scalar, so a
    /// char-counting encoder would report offset 2 rather than 3.
    #[test]
    fn rank_bridge_folds_unicode_and_counts_utf16_units() {
        // Decomposed "CAFE" + combining acute; the parsed needle is precomposed.
        let text = "\u{1D11E} CAFE\u{301} note";
        let plan = QueryPlan::friendly("caf\u{e9}", 8, 8);
        let branch = block_branch(&plan).expect("a bare term plans a block branch");
        let rank = rank_block_text(&plan, branch, text)
            .expect("casefold + NFC must admit the decomposed block text");
        assert_eq!(rank.match_class(), ObjectiveMatchClass::Substring);
        assert_eq!(
            decode_rank_key(&rank.order_key()),
            (ObjectiveMatchClass::Substring.rank(), true, 3, 13, 1)
        );
        assert_eq!(
            text.chars().count(),
            12,
            "UTF-16 length is not the char count"
        );

        // The precomposed spelling folds to the same needle position and class;
        // only `text_len` differs, because UTF-16 length is measured on the
        // ORIGINAL visible text, exactly as the existing evaluator measures it.
        let precomposed = "\u{1D11E} CAF\u{c9} note";
        let composed_rank = rank_block_text(&plan, branch, precomposed)
            .expect("the precomposed spelling matches the same needle");
        assert_eq!(
            decode_rank_key(&composed_rank.order_key()),
            (ObjectiveMatchClass::Substring.rank(), true, 3, 12, 1)
        );
        assert!(
            composed_rank.order_key() < rank.order_key(),
            "the shorter original text is the better tuple"
        );
        assert!(rank_block_text(&plan, branch, "\u{1D11E} cafe note").is_none());
    }

    /// Selection is rank-only: it constructs no match evidence. The optional
    /// accessor reproduces the existing evaluator's spans exactly, including
    /// its BEST-branch OR choice, which is not the membership evaluator's
    /// first-branch choice.
    #[test]
    fn rank_only_selection_builds_no_evidence_while_the_accessor_reproduces_it() {
        let plan = QueryPlan::friendly("zzz OR ready", 8, 8);
        let branch = block_branch(&plan).expect("an OR query plans a block branch");
        let text = "ready and zzz";

        let _ = take_block_evidence_evaluations();
        TEXT_EVIDENCE_EVALUATIONS.with(|count| count.set(0));
        let rank = rank_block_text(&plan, branch, text).expect("both OR arms match");
        TEXT_EVIDENCE_EVALUATIONS.with(|count| {
            assert_eq!(count.get(), 0, "selection must not construct text evidence");
        });
        assert_eq!(
            take_block_evidence_evaluations(),
            0,
            "rank-only selection must not run the evidence evaluator"
        );
        assert_eq!(rank.match_class(), ObjectiveMatchClass::Prefix);

        let evidence =
            admitted_block_evidence(&plan, branch, text).expect("the admitted row has a reason");
        assert_eq!(take_block_evidence_evaluations(), 1);
        TEXT_EVIDENCE_EVALUATIONS.with(|count| {
            assert!(
                count.get() > 0,
                "the evidence counter must observe the matcher"
            );
        });
        let reference =
            eval_ranked_block_expr(&plan, &branch.predicate, text, &canonical_fold(text)).unwrap();
        assert_eq!(evidence, reference.evidence);
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].spans, vec![MatchSpan { start: 0, end: 5 }]);
        assert_eq!(evidence[0].mode, TextMatchMode::Contains);

        // Best branch, not first branch.
        let first_branch = eval_expr(&plan, &branch.predicate, TextField::VisibleContent, text)
            .expect("membership evaluator also admits");
        assert_ne!(evidence[0].clause_id, first_branch.evidence[0].clause_id);
    }

    /// Boolean membership is preserved verbatim, including a satisfied
    /// negation admitting on the neutral tuple with no evidence at all. Page
    /// branches are deliberately out of scope: page-name/alias ranking is not
    /// covered by these five block components.
    #[test]
    fn rank_bridge_preserves_neutral_negation_and_stays_out_of_page_ranking() {
        let plan = QueryPlan::friendly("draft", 8, 8);
        let source = block_branch(&plan).expect("a bare term plans a block branch");
        let negated = QueryBranch {
            target: QueryTarget::Blocks,
            predicate: QueryExpr::Not(Box::new(source.predicate.clone())),
            limit: source.limit,
        };
        let rank = rank_block_text(&plan, &negated, "ship it").expect("a satisfied NOT admits");
        assert_eq!(
            decode_rank_key(&rank.order_key()),
            (ObjectiveMatchClass::Exact.rank(), true, 0, 0, 0)
        );
        assert_eq!(rank.match_class(), ObjectiveMatchClass::Exact);
        assert_eq!(
            admitted_block_evidence(&plan, &negated, "ship it"),
            Some(Vec::new()),
            "a successful negation contributes no positive evidence"
        );
        assert!(rank_block_text(&plan, &negated, "draft one").is_none());

        let pages = plan
            .branches
            .iter()
            .find(|branch| branch.target == QueryTarget::Pages)
            .expect("friendly plans a page branch");
        assert!(rank_block_text(&plan, pages, "draft").is_none());
        assert!(admitted_block_evidence(&plan, pages, "draft").is_none());
    }

    /// There is no second regex grammar and no literal fallback behind the
    /// bridge: a clause the plan never compiled matches nothing, and an
    /// unparseable regex never reaches a branch at all.
    #[test]
    fn rank_bridge_has_no_fallback_for_an_uncompiled_regex_clause() {
        assert!(QueryPlan::friendly("/(unclosed/", 8, 8).branches.is_empty());
        let plan = QueryPlan::friendly("/A[BC]+/", 8, 8);
        let branch = block_branch(&plan).expect("a valid regex plans a block branch");
        assert!(rank_block_text(&plan, branch, "regex ABC").is_some());

        let pred = match &branch.predicate {
            QueryExpr::Text(pred) => pred.clone(),
            other => panic!("a single regex term compiles to one text clause, got {other:?}"),
        };
        let uncompiled = QueryBranch {
            target: QueryTarget::Blocks,
            predicate: QueryExpr::Text(TextPredicate {
                clause_id: pred.clause_id.wrapping_add(1_000),
                ..pred
            }),
            limit: branch.limit,
        };
        assert!(rank_block_text(&plan, &uncompiled, "regex ABC").is_none());
        assert!(admitted_block_evidence(&plan, &uncompiled, "regex ABC").is_none());
    }
}
