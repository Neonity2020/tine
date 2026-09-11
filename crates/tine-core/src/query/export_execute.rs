//! Common block-subtree export execution over one caller-owned projection snapshot.

use tine_storage::sqlite::PhysicalProjectionQuerySnapshot;

use crate::model::BlockDto;
use crate::query::export_results::{
    apply_located_view, hydrate_located_export_queries, select_located_export_queries,
    ExportSubtreeInputs,
};
use crate::query::ir::{
    Anchor, Bounds, ExecutionContext, Query, QueryResult, QueryRows, ViewSettings,
};
use crate::query::registry::Registry;
use crate::query::results::{
    probe_fts_ready, read_located_results, BackendOrder, RecencyPage, ResultIdentity,
    ResultLocator, ResultReadError, ResultReadInputs,
};
use crate::query::sql::{lower_query, LoweringInputs, RELATION_RULE, RESULT_SET_RULE};
use crate::query::{
    block_anchored_query, parse_query_input, parse_query_text, resolve_for_execution,
    ConstructionProfile, ExportSelectionAnswer, QueryExportBatch, QueryExportResult,
    QueryExportSpec, QueryInput, QUERY_EXPORT_CONSTRUCTION_BYTES, QUERY_EXPORT_CONSTRUCTION_ROWS,
};

#[cfg(test)]
#[path = "export_execute_tests.rs"]
mod export_execute_tests;

#[cfg(test)]
thread_local! {
    static AFTER_CONSTRUCTION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_after_construction_hook(hook: Box<dyn FnOnce()>) {
    AFTER_CONSTRUCTION.with(|slot| *slot.borrow_mut() = Some(hook));
}

#[cfg(test)]
fn after_construction_hook() {
    if let Some(hook) = AFTER_CONSTRUCTION.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

/// One executable macro, already parsed and bound under its batch's day.
struct PreparedExportQuery {
    spec: QueryExportSpec,
    execution: Option<PreparedExecution>,
}

struct PreparedExecution {
    query: Query,
    view: ViewSettings,
}

/// The bounded relation between exported specs and their prepared executions.
///
/// The relation owns each spec beside its execution so a later adapter cannot
/// pair independently prepared queries with a different spec list. It is
/// operation-scoped and contains no result rows or projection handle.
pub(crate) struct PreparedExportBatch {
    today: crate::date::JournalDate,
    items: Vec<PreparedExportQuery>,
    omitted_queries: usize,
}

/// Inputs supplied by either backend after it acquires one coherent snapshot.
pub(crate) struct ExportExecutionInputs<'a> {
    pub(crate) registry: &'a Registry,
    pub(crate) identity: &'a ResultIdentity,
    pub(crate) order: BackendOrder,
    pub(crate) recency: &'a dyn Fn(RecencyPage<'_>) -> i64,
    pub(crate) max_roots: usize,
    pub(crate) max_nodes: usize,
    pub(crate) max_bytes: usize,
}

pub(crate) struct SubtreeSelectionInputs<'a> {
    pub(crate) registry: &'a Registry,
    pub(crate) identity: &'a ResultIdentity,
    pub(crate) order: BackendOrder,
    pub(crate) recency: &'a dyn Fn(RecencyPage<'_>) -> i64,
    pub(crate) today: crate::date::JournalDate,
    pub(crate) fts_ready: bool,
}

pub(crate) struct SubtreeQueryResult {
    pub(crate) result: QueryResult,
    pub(crate) roots: Vec<crate::query::export_results::HydratedRoot>,
}

/// Execute caller-supplied IR with complete subtree output. This preserves the
/// support report and checks selection overflow before any subtree read.
pub(crate) fn execute_subtrees_from_ir(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    query: &Query,
    view: &ViewSettings,
    bounds: Bounds,
    context: &ExecutionContext,
    inputs: &SubtreeSelectionInputs<'_>,
) -> Result<SubtreeQueryResult, crate::query::QueryExecutionError> {
    if snapshot.cancellation().is_cancelled() {
        return Err(crate::query::QueryExecutionError::Cancelled);
    }
    let resolved = resolve_for_execution(query, context, inputs.today);
    let query = block_anchored_query(resolved.query());
    let mut result = QueryResult {
        statistics: None,
        rows: QueryRows::Block { groups: Vec::new() },
        diagnostics: query.diagnostics.clone(),
        report: resolved.report().clone(),
        total: 0,
        matched_total: None,
        exceeded: false,
    };
    if !result.report.supported || query.is_invalid() || resolved.query().anchor == Anchor::Page {
        return Ok(SubtreeQueryResult {
            result,
            roots: Vec::new(),
        });
    }
    let selected = select_subtree_roots(snapshot, &query, view, bounds, inputs)?;
    result.total = selected.total;
    result.exceeded = selected.exceeded;
    if selected.exceeded {
        return Ok(SubtreeQueryResult {
            result,
            roots: Vec::new(),
        });
    }
    let roots = selected
        .groups
        .into_iter()
        .flat_map(|group| {
            group.blocks.into_iter().map(move |(block, locator)| {
                crate::query::export_results::LocatedExportRoot {
                    page: group.page.clone(),
                    kind: group.kind,
                    block,
                    locator,
                }
            })
        })
        .collect();
    let selected = vec![crate::query::SelectedExportQueryOf {
        key: String::new(),
        roots,
        total: selected.total,
    }];
    let mut hydrated = crate::query::export_results::hydrate_located_queries(
        snapshot,
        selected,
        inputs.identity,
        crate::query::export_results::SubtreeOutputPolicy::Complete,
    )?;
    #[cfg(test)]
    after_construction_hook();
    if snapshot.cancellation().is_cancelled() {
        return Err(crate::query::QueryExecutionError::Cancelled);
    }
    Ok(SubtreeQueryResult {
        result,
        roots: hydrated.pop().expect("one subtree query").roots,
    })
}

fn select_subtree_roots(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    query: &Query,
    view: &ViewSettings,
    bounds: Bounds,
    inputs: &SubtreeSelectionInputs<'_>,
) -> Result<ExportSelectionAnswer<(BlockDto, ResultLocator)>, ResultReadError> {
    let compiled = crate::query::eval::CompiledLeaves::for_query(&query.evaluable_filter());
    let statement = lower_query(
        query,
        &LoweringInputs {
            today: inputs.today,
            registry: inputs.registry,
            cutoff: None,
            compiled: &compiled,
            fts_ready: inputs.fts_ready,
            result_set_rule: RESULT_SET_RULE,
            relation_rule: RELATION_RULE,
        },
    );
    if statement.matches_nothing {
        return Ok(empty_selection());
    }
    let pre = read_located_results(
        snapshot,
        &ResultReadInputs {
            statement: &statement,
            order: inputs.order,
            identity: inputs.identity,
            max_rows: bounds.max_rows,
            max_bytes: bounds.max_bytes,
            profile: ConstructionProfile::from_view(view),
            recency: inputs.recency,
        },
    )?;
    Ok(apply_located_view(pre, view))
}

impl PreparedExportBatch {
    /// Parse and bind only the prefix the existing macro cap can evaluate.
    pub(crate) fn prepare(
        specs: &[QueryExportSpec],
        max_queries: usize,
        today: crate::date::JournalDate,
    ) -> Self {
        let query_limit = max_queries.max(1);
        let items = specs
            .iter()
            .take(query_limit)
            .cloned()
            .map(|spec| {
                let execution = if spec.advanced {
                    let (parsed, _) = parse_query_input(
                        &spec.query,
                        QueryInput::Advanced,
                        today,
                        Registry::none(),
                    );
                    let resolved = resolve_for_execution(
                        &parsed,
                        &ExecutionContext {
                            current_page: spec.current_page.clone(),
                        },
                        today,
                    );
                    (resolved.report().supported && !resolved.query().is_invalid()).then(|| {
                        PreparedExecution {
                            query: block_anchored_query(resolved.query()),
                            view: ViewSettings::default(),
                        }
                    })
                } else {
                    let (query, view) = parse_query_text(&spec.query, spec.simple_dialect(), today);
                    (!query.is_invalid()).then(|| PreparedExecution {
                        query: block_anchored_query(&query),
                        view,
                    })
                };
                PreparedExportQuery { spec, execution }
            })
            .collect();
        Self {
            today,
            items,
            omitted_queries: specs.len().saturating_sub(query_limit),
        }
    }

    /// Whether any prepared executable query needs inferred/declared property types.
    pub(crate) fn requires_registry(&self) -> bool {
        self.items.iter().any(|item| {
            item.execution
                .as_ref()
                .is_some_and(|execution| execution.query.filter.has_props_leaf())
        })
    }

    /// Return the complete semantic answer when every prepared macro is refused.
    ///
    /// This path needs neither a projection snapshot nor a registry. `None`
    /// means at least one macro must execute against a snapshot.
    pub(crate) fn all_refused_result(&self, max_roots: usize) -> Option<QueryExportBatch> {
        if self.items.iter().any(|item| item.execution.is_some()) {
            return None;
        }
        let selected = self.select(max_roots, |_| {
            Ok::<_, std::convert::Infallible>(empty_selection())
        });
        let (_, selected) = selected.expect("an empty selection cannot fail");
        Some(QueryExportBatch {
            results: empty_results(selected),
            omitted_queries: self.omitted_queries,
        })
    }

    /// Execute every prepared macro, root selection and subtree hydration on one snapshot.
    pub(crate) fn execute(
        &self,
        snapshot: &mut PhysicalProjectionQuerySnapshot,
        inputs: &ExportExecutionInputs<'_>,
    ) -> Result<QueryExportBatch, ResultReadError> {
        if let Some(result) = self.all_refused_result(inputs.max_roots) {
            return Ok(result);
        }
        let fts_ready = probe_fts_ready(snapshot)?;
        let (_, selected) = self.select(inputs.max_roots, |item| {
            let Some(execution) = item.execution.as_ref() else {
                return Ok(empty_selection());
            };
            select_subtree_roots(
                snapshot,
                &execution.query,
                &execution.view,
                Bounds {
                    max_rows: QUERY_EXPORT_CONSTRUCTION_ROWS,
                    max_bytes: QUERY_EXPORT_CONSTRUCTION_BYTES,
                },
                &SubtreeSelectionInputs {
                    registry: inputs.registry,
                    identity: inputs.identity,
                    order: inputs.order,
                    recency: inputs.recency,
                    today: self.today,
                    fts_ready,
                },
            )
        })?;
        let results = hydrate_located_export_queries(
            snapshot,
            selected,
            &ExportSubtreeInputs {
                identity: inputs.identity,
                max_nodes: inputs.max_nodes,
                max_bytes: inputs.max_bytes,
            },
        )?;
        #[cfg(test)]
        after_construction_hook();
        if snapshot.cancellation().is_cancelled() {
            return Err(ResultReadError::Cancelled);
        }
        Ok(QueryExportBatch {
            results,
            omitted_queries: self.omitted_queries,
        })
    }

    fn select<E>(
        &self,
        max_roots: usize,
        mut evaluate: impl FnMut(
            &PreparedExportQuery,
        ) -> Result<ExportSelectionAnswer<(BlockDto, ResultLocator)>, E>,
    ) -> Result<(usize, Vec<crate::query::export_results::LocatedExportQuery>), E> {
        let specs = self
            .items
            .iter()
            .map(|item| item.spec.clone())
            .collect::<Vec<_>>();
        let mut items = self.items.iter();
        select_located_export_queries(&specs, self.items.len().max(1), max_roots, |spec| {
            let item = items
                .next()
                .expect("the owned spec/execution relation stays aligned");
            debug_assert_eq!(item.spec.key, spec.key);
            evaluate(item)
        })
    }
}

fn empty_selection() -> ExportSelectionAnswer<(BlockDto, ResultLocator)> {
    ExportSelectionAnswer {
        groups: Vec::new(),
        total: 0,
        exceeded: false,
    }
}

fn empty_results(
    selected: Vec<crate::query::export_results::LocatedExportQuery>,
) -> Vec<QueryExportResult> {
    selected
        .into_iter()
        .map(|query| QueryExportResult {
            key: query.key,
            groups: Vec::new(),
            shown: 0,
            total: query.total,
            omitted_nodes: 0,
        })
        .collect()
}
