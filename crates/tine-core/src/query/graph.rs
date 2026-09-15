//! The static graph capability used by the query engine.

use std::sync::Arc;

use crate::config::Config;
use crate::doc::Document;
use crate::vocab::{PageEntry, RefGroup, ReferenceCandidatePages, ReferenceKind};

#[allow(dead_code)]
pub(crate) trait QueryGraph {
    fn with_pages<T>(&self, f: impl FnOnce(&[(PageEntry, Arc<Document>)]) -> T) -> T;
    fn page_aliases(&self) -> Vec<(String, String)>;
    fn block_page_hint(&self, uuid: &str) -> Option<String>;
    fn reference_candidate_pages(
        &self,
        names_norm: &[String],
        kind: ReferenceKind,
    ) -> ReferenceCandidatePages;
    fn reference_candidate_pages_indexed(
        &self,
        names_norm: &[String],
        kind: ReferenceKind,
    ) -> Result<ReferenceCandidatePages, super::QueryExecutionError>;
    fn direct_projection_block_referrer_candidate_pages(
        &self,
        uuid: &str,
    ) -> Option<Vec<(PageEntry, Arc<Document>)>>;
    fn list_pages(&self) -> Vec<PageEntry>;
    fn page_aliases_with_owners(&self) -> Vec<(String, String, String)>;
    fn referenced_page_names(&self) -> Vec<String>;
    fn reference_real_page_names(&self) -> Option<super::RealPageNames>;
    fn direct_ir_query_result(
        &self,
        resolved: &super::ResolvedQuery,
        view: &super::ir::ViewSettings,
        bounds: super::ir::Bounds,
    ) -> Result<super::ir::QueryResult, super::QueryExecutionError>;
    fn direct_ir_explain_empty(
        &self,
        resolved: &super::ResolvedQuery,
        view: &super::ir::ViewSettings,
        bounds: super::ir::Bounds,
    ) -> Result<super::ir::ExplainEmptyResult, super::QueryExecutionError>;
    fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RefGroup>, super::QueryExecutionError>;
    fn direct_projection_recover_after_failed_read(&self);
    fn cache_generation(&self) -> u64;
    fn config(&self) -> &Config;

    #[cfg(test)]
    fn direct_projection_test(&self) -> Option<Arc<crate::direct_projection::DirectProjection>>;
    #[cfg(test)]
    fn direct_projection_ready_test(&self) -> bool;
}

// Generic entry points do not apply the old `&Arc<Graph>` → `&Graph` deref
// coercion during type inference. Forward the capability through `Arc` so the
// established callers keep their signatures without cloning or allocation.
impl<G: QueryGraph> QueryGraph for Arc<G> {
    fn with_pages<T>(&self, f: impl FnOnce(&[(PageEntry, Arc<Document>)]) -> T) -> T {
        (**self).with_pages(f)
    }

    fn page_aliases(&self) -> Vec<(String, String)> {
        (**self).page_aliases()
    }

    fn block_page_hint(&self, uuid: &str) -> Option<String> {
        (**self).block_page_hint(uuid)
    }

    fn reference_candidate_pages(
        &self,
        names_norm: &[String],
        kind: ReferenceKind,
    ) -> ReferenceCandidatePages {
        (**self).reference_candidate_pages(names_norm, kind)
    }

    fn reference_candidate_pages_indexed(
        &self,
        names_norm: &[String],
        kind: ReferenceKind,
    ) -> Result<ReferenceCandidatePages, super::QueryExecutionError> {
        (**self).reference_candidate_pages_indexed(names_norm, kind)
    }

    fn direct_projection_block_referrer_candidate_pages(
        &self,
        uuid: &str,
    ) -> Option<Vec<(PageEntry, Arc<Document>)>> {
        (**self).direct_projection_block_referrer_candidate_pages(uuid)
    }

    fn list_pages(&self) -> Vec<PageEntry> {
        (**self).list_pages()
    }

    fn page_aliases_with_owners(&self) -> Vec<(String, String, String)> {
        (**self).page_aliases_with_owners()
    }

    fn referenced_page_names(&self) -> Vec<String> {
        (**self).referenced_page_names()
    }

    fn reference_real_page_names(&self) -> Option<super::RealPageNames> {
        (**self).reference_real_page_names()
    }

    fn direct_ir_query_result(
        &self,
        resolved: &super::ResolvedQuery,
        view: &super::ir::ViewSettings,
        bounds: super::ir::Bounds,
    ) -> Result<super::ir::QueryResult, super::QueryExecutionError> {
        (**self).direct_ir_query_result(resolved, view, bounds)
    }

    fn direct_ir_explain_empty(
        &self,
        resolved: &super::ResolvedQuery,
        view: &super::ir::ViewSettings,
        bounds: super::ir::Bounds,
    ) -> Result<super::ir::ExplainEmptyResult, super::QueryExecutionError> {
        (**self).direct_ir_explain_empty(resolved, view, bounds)
    }

    fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RefGroup>, super::QueryExecutionError> {
        (**self).search(query, limit)
    }

    fn direct_projection_recover_after_failed_read(&self) {
        QueryGraph::direct_projection_recover_after_failed_read(&**self)
    }

    fn cache_generation(&self) -> u64 {
        (**self).cache_generation()
    }

    fn config(&self) -> &Config {
        (**self).config()
    }

    #[cfg(test)]
    fn direct_projection_test(&self) -> Option<Arc<crate::direct_projection::DirectProjection>> {
        (**self).direct_projection_test()
    }

    #[cfg(test)]
    fn direct_projection_ready_test(&self) -> bool {
        (**self).direct_projection_ready_test()
    }
}
