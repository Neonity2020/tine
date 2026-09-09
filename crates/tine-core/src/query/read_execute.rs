//! Query execution within an already acquired, caller-owned SQLite image.
//!
//! Capture and lifecycle stay with the caller. This small driver composes the
//! existing binding, compiler, shallow readers and view constructor for consumers
//! that perform several queries inside one operation, such as static publication.

use std::cell::RefCell;

use tine_storage::sqlite::PhysicalProjectionQuerySnapshot;

use super::ir::{Anchor, Bounds, ExecutionContext, Query, QueryResult, QueryRows, ViewSettings};
use super::rank::PageRecencyPrograms;
use super::registry::Registry;
use super::results::{
    probe_fts_ready, read_page_results, read_results, BackendOrder, PageReadInputs, RecencyPage,
    ResultIdentity, ResultReadInputs,
};
use super::sql::{lower_query, LoweringInputs, RESULT_SET_RULE};
use super::{
    apply_view, block_anchored_query, resolve_for_execution, ConstructionProfile,
    QueryExecutionError,
};

pub(crate) struct SnapshotQueryInputs<'a> {
    pub(crate) registry: &'a Registry,
    pub(crate) identity: &'a ResultIdentity,
    pub(crate) order: BackendOrder,
    pub(crate) recency: &'a dyn Fn(RecencyPage<'_>) -> i64,
    pub(crate) page_recency: &'a PageRecencyPrograms,
    pub(crate) today: crate::date::JournalDate,
}

/// No answer retention: every run executes against this operation's image.
pub(crate) struct SnapshotQueryReader<'a> {
    snapshot: RefCell<&'a mut PhysicalProjectionQuerySnapshot>,
    inputs: SnapshotQueryInputs<'a>,
    fts_ready: bool,
}

impl<'a> SnapshotQueryReader<'a> {
    pub(crate) fn run_subtrees(
        &self,
        query: &Query,
        view: &ViewSettings,
        bounds: Bounds,
        context: &ExecutionContext,
    ) -> Result<super::export_execute::SubtreeQueryResult, QueryExecutionError> {
        self.ensure_current()?;
        let result = super::export_execute::execute_subtrees_from_ir(
            &mut self.snapshot.borrow_mut(),
            query,
            view,
            bounds,
            context,
            &super::export_execute::SubtreeSelectionInputs {
                registry: self.inputs.registry,
                identity: self.inputs.identity,
                order: self.inputs.order,
                recency: self.inputs.recency,
                today: self.inputs.today,
                fts_ready: self.fts_ready,
            },
        )?;
        self.ensure_current()?;
        Ok(result)
    }

    pub(crate) fn new(
        snapshot: &'a mut PhysicalProjectionQuerySnapshot,
        inputs: SnapshotQueryInputs<'a>,
    ) -> Result<Self, QueryExecutionError> {
        let fts_ready = probe_fts_ready(snapshot)?;
        Ok(Self {
            snapshot: RefCell::new(snapshot),
            inputs,
            fts_ready,
        })
    }

    pub(crate) fn ensure_current(&self) -> Result<(), QueryExecutionError> {
        if self.snapshot.borrow().cancellation().is_cancelled() {
            Err(QueryExecutionError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub(crate) fn run(
        &self,
        query: &Query,
        view: &ViewSettings,
        bounds: Bounds,
        context: &ExecutionContext,
    ) -> Result<QueryResult, QueryExecutionError> {
        self.ensure_current()?;
        let resolved = resolve_for_execution(query, context, self.inputs.today);
        let execution_view = super::view::statistics_execution_view(resolved.query(), view);
        let view = &execution_view;
        let query = if resolved.query().anchor == Anchor::Page {
            resolved.query().clone()
        } else {
            block_anchored_query(resolved.query())
        };
        let mut result = QueryResult {
            statistics: None,
            rows: match query.anchor {
                Anchor::Page => QueryRows::Page { pages: Vec::new() },
                Anchor::Block => QueryRows::Block { groups: Vec::new() },
            },
            diagnostics: query.diagnostics.clone(),
            report: resolved.report().clone(),
            total: 0,
            matched_total: (query.anchor == Anchor::Page).then_some(0),
            exceeded: false,
        };
        if !result.report.supported || query.is_invalid() {
            return Ok(result);
        }
        let compiled = super::eval::CompiledLeaves::for_query(&query.evaluable_filter());
        let statement = lower_query(
            &query,
            &LoweringInputs {
                today: resolved.today(),
                registry: self.inputs.registry,
                cutoff: None,
                compiled: &compiled,
                fts_ready: self.fts_ready,
                result_set_rule: RESULT_SET_RULE,
            },
        );
        let mut snapshot = self.snapshot.borrow_mut();
        match query.anchor {
            Anchor::Page => {
                let answer = read_page_results(
                    &mut snapshot,
                    &PageReadInputs {
                        statement: &statement,
                        order: self.inputs.order,
                        view,
                        max_rows: bounds.max_rows,
                        max_bytes: bounds.max_bytes,
                        recency: self.inputs.page_recency,
                    },
                )?;
                result.total = answer.total;
                result.statistics = answer.statistics;
                result.matched_total = Some(answer.matched_total);
                result.exceeded = answer.exceeded;
                result.rows = QueryRows::Page {
                    pages: answer.pages,
                };
            }
            Anchor::Block => {
                let pre = super::results::read_ordered_results(
                    &mut snapshot,
                    &ResultReadInputs {
                        statement: &statement,
                        order: self.inputs.order,
                        identity: self.inputs.identity,
                        max_rows: bounds.max_rows,
                        max_bytes: bounds.max_bytes,
                        profile: ConstructionProfile::from_view(view),
                        recency: self.inputs.recency,
                    },
                    view,
                    self.inputs.page_recency,
                )?;
                let answer = apply_view(pre, view);
                result.total = answer.total;
                result.statistics = answer.statistics;
                result.matched_total = answer.matched_total;
                result.exceeded = answer.exceeded;
                result.rows = QueryRows::Block {
                    groups: answer.groups,
                };
            }
        }
        drop(snapshot);
        self.ensure_current()?;
        Ok(result)
    }
}

impl crate::publish::PublicationQueryRead for SnapshotQueryReader<'_> {
    fn run_subtrees(
        &self,
        query: &Query,
        view: &ViewSettings,
        bounds: Bounds,
        context: &ExecutionContext,
    ) -> Result<super::export_execute::SubtreeQueryResult, QueryExecutionError> {
        SnapshotQueryReader::run_subtrees(self, query, view, bounds, context)
    }

    fn run(
        &self,
        query: &Query,
        view: &ViewSettings,
        bounds: Bounds,
        context: &ExecutionContext,
    ) -> Result<QueryResult, QueryExecutionError> {
        SnapshotQueryReader::run(self, query, view, bounds, context)
    }

    fn ensure_current(&self) -> Result<(), QueryExecutionError> {
        SnapshotQueryReader::ensure_current(self)
    }
}
