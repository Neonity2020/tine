//! `Graph`'s implementation of the query engine's static capability boundary.

use super::*;
use crate::query::graph::QueryGraph;

impl QueryGraph for Graph {
    fn indexed_derived_pages(
        &self,
        selection: crate::direct_projection::derived_reads::DerivedSelection<'_>,
    ) -> Option<Vec<(PageEntry, Arc<Document>)>> {
        Graph::indexed_derived_pages(self, selection)
    }
    fn with_pages<T>(&self, f: impl FnOnce(&[(PageEntry, Arc<Document>)]) -> T) -> T {
        Graph::with_pages(self, f)
    }
    fn indexed_or_fallback<T>(
        &self,
        indexed: impl FnMut() -> Option<T>,
    ) -> Result<T, PageFallback> {
        Graph::indexed_or_fallback(self, indexed)
    }

    fn page_aliases(&self) -> Vec<(String, String)> {
        Graph::page_aliases(self)
    }

    fn block_page_hint(&self, uuid: &str) -> Option<String> {
        Graph::block_page_hint(self, uuid)
    }

    fn reference_candidate_pages(
        &self,
        names_norm: &[String],
        self_page: &str,
        kind: ReferenceKind,
    ) -> ReferenceCandidatePages {
        Graph::reference_candidate_pages(self, names_norm, self_page, kind)
    }

    fn reference_candidate_pages_indexed(
        &self,
        names_norm: &[String],
        self_page: &str,
        kind: ReferenceKind,
    ) -> Result<ReferenceCandidatePages, crate::query::QueryExecutionError> {
        Graph::reference_candidate_pages_indexed(self, names_norm, self_page, kind)
    }

    fn reference_readiness(
        &self,
        target: &str,
        kind: ReferenceKind,
    ) -> Result<(), crate::query::QueryExecutionError> {
        Graph::reference_readiness(self, target, kind)
    }

    fn backlink_filter_scope(
        &self,
        target: &str,
        requested_pages: &[(PageKind, String)],
    ) -> Result<crate::query::BacklinkFilterScope, crate::query::QueryExecutionError> {
        Graph::backlink_filter_scope(self, target, requested_pages)
    }

    fn direct_projection_block_referrer_candidate_pages(
        &self,
        uuid: &str,
    ) -> Option<Vec<(PageEntry, Arc<Document>)>> {
        Graph::direct_projection_block_referrer_candidate_pages(self, uuid)
    }

    fn list_pages(&self) -> Vec<PageEntry> {
        Graph::list_pages(self)
    }

    fn page_aliases_with_owners(&self) -> Vec<(String, String, String)> {
        Graph::page_aliases_with_owners(self)
    }

    fn referenced_page_names(&self) -> Vec<String> {
        Graph::referenced_page_names(self)
    }

    fn reference_real_page_names(&self) -> Option<crate::query::RealPageNames> {
        Graph::reference_real_page_names(self)
    }

    fn direct_ir_query_result(
        &self,
        resolved: &crate::query::ResolvedQuery,
        view: &crate::query::ir::ViewSettings,
        bounds: crate::query::ir::Bounds,
    ) -> Result<crate::query::ir::QueryResult, crate::query::QueryExecutionError> {
        Graph::direct_ir_query_result(self, resolved, view, bounds)
    }

    fn direct_ir_explain_empty(
        &self,
        resolved: &crate::query::ResolvedQuery,
        view: &crate::query::ir::ViewSettings,
        bounds: crate::query::ir::Bounds,
    ) -> Result<crate::query::ir::ExplainEmptyResult, crate::query::QueryExecutionError> {
        Graph::direct_ir_explain_empty(self, resolved, view, bounds)
    }

    fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RefGroup>, crate::query::QueryExecutionError> {
        Graph::search(self, query, limit)
    }

    #[cfg(test)]
    fn direct_projection_recover_after_failed_read(&self) {
        Graph::direct_projection_recover_after_failed_read(self)
    }

    fn cache_generation(&self) -> u64 {
        Graph::cache_generation(self)
    }

    fn config(&self) -> Arc<Config> {
        Graph::config(self)
    }

    #[cfg(test)]
    fn direct_projection_test(&self) -> Option<Arc<crate::direct_projection::DirectProjection>> {
        Graph::direct_projection_test(self)
    }

    #[cfg(test)]
    fn direct_projection_ready_test(&self) -> bool {
        Graph::direct_projection_ready_test(self)
    }
}
