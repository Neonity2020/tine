//! The static graph capability used by the query engine.

use std::sync::Arc;

use crate::config::Config;
use crate::doc::Document;
use crate::vocab::{PageEntry, PageKind, RefGroup, ReferenceCandidatePages, ReferenceKind};

/// How to answer a read the index declined; see `Graph::indexed_or_fallback`.
pub(crate) enum PageFallback {
    /// The parsed cache the index left the answer to.
    Cache(Arc<Vec<(PageEntry, Arc<Document>)>>),
    /// The index could not answer and had no cache to leave it to: the
    /// caller's parser route, which reads a cache installed meanwhile or
    /// builds one.
    Parse,
}

impl PageFallback {
    pub(crate) fn with_pages<G: QueryGraph + ?Sized, T>(
        self,
        graph: &G,
        f: impl FnOnce(&[(PageEntry, Arc<Document>)]) -> T,
    ) -> T {
        match self {
            Self::Cache(pages) => f(pages.as_slice()),
            Self::Parse => graph.with_pages(f),
        }
    }
}

#[allow(dead_code)]
pub(crate) trait QueryGraph {
    fn indexed_derived_pages(
        &self,
        _selection: crate::direct_projection::derived_reads::DerivedSelection<'_>,
    ) -> Option<Vec<(PageEntry, Arc<Document>)>> {
        None
    }
    fn with_pages<T>(&self, f: impl FnOnce(&[(PageEntry, Arc<Document>)]) -> T) -> T;
    /// Ask the index with `indexed`, and say how to answer if it declines;
    /// see `Graph::indexed_or_fallback`.
    fn indexed_or_fallback<T>(
        &self,
        mut indexed: impl FnMut() -> Option<T>,
    ) -> Result<T, PageFallback> {
        indexed().ok_or(PageFallback::Parse)
    }
    fn page_aliases(&self) -> Vec<(String, String)>;
    fn block_page_hint(&self, uuid: &str) -> Option<String>;
    fn reference_candidate_pages(
        &self,
        names_norm: &[String],
        self_page: &str,
        kind: ReferenceKind,
    ) -> ReferenceCandidatePages;
    fn reference_candidate_pages_indexed(
        &self,
        names_norm: &[String],
        self_page: &str,
        kind: ReferenceKind,
    ) -> Result<ReferenceCandidatePages, super::QueryExecutionError>;
    /// Whether a reference read of `target` can be answered now: `NotReady`
    /// while the index is working on it, `IndexFailed` once it failed. Asked
    /// before anything else the read does; see `Graph::reference_readiness`.
    fn reference_readiness(
        &self,
        _target: &str,
        _kind: ReferenceKind,
    ) -> Result<(), super::QueryExecutionError> {
        Ok(())
    }
    fn backlink_filter_scope(
        &self,
        target: &str,
        requested_pages: &[(PageKind, String)],
    ) -> Result<super::BacklinkFilterScope, super::QueryExecutionError>;
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
    #[cfg(test)]
    fn direct_projection_recover_after_failed_read(&self);
    fn cache_generation(&self) -> u64;
    fn config(&self) -> Arc<Config>;

    #[cfg(test)]
    fn direct_projection_test(&self) -> Option<Arc<crate::direct_projection::DirectProjection>>;
    #[cfg(test)]
    fn direct_projection_ready_test(&self) -> bool;
}

// Generic entry points do not apply the old `&Arc<Graph>` → `&Graph` deref
// coercion during type inference. Forward the capability through `Arc` so the
// established callers keep their signatures without cloning or allocation.
impl<G: QueryGraph> QueryGraph for Arc<G> {
    fn indexed_derived_pages(
        &self,
        selection: crate::direct_projection::derived_reads::DerivedSelection<'_>,
    ) -> Option<Vec<(PageEntry, Arc<Document>)>> {
        (**self).indexed_derived_pages(selection)
    }
    fn with_pages<T>(&self, f: impl FnOnce(&[(PageEntry, Arc<Document>)]) -> T) -> T {
        (**self).with_pages(f)
    }
    fn indexed_or_fallback<T>(
        &self,
        indexed: impl FnMut() -> Option<T>,
    ) -> Result<T, PageFallback> {
        (**self).indexed_or_fallback(indexed)
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
        self_page: &str,
        kind: ReferenceKind,
    ) -> ReferenceCandidatePages {
        (**self).reference_candidate_pages(names_norm, self_page, kind)
    }

    fn reference_candidate_pages_indexed(
        &self,
        names_norm: &[String],
        self_page: &str,
        kind: ReferenceKind,
    ) -> Result<ReferenceCandidatePages, super::QueryExecutionError> {
        (**self).reference_candidate_pages_indexed(names_norm, self_page, kind)
    }

    fn reference_readiness(
        &self,
        target: &str,
        kind: ReferenceKind,
    ) -> Result<(), super::QueryExecutionError> {
        (**self).reference_readiness(target, kind)
    }

    fn backlink_filter_scope(
        &self,
        target: &str,
        requested_pages: &[(PageKind, String)],
    ) -> Result<super::BacklinkFilterScope, super::QueryExecutionError> {
        (**self).backlink_filter_scope(target, requested_pages)
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

    #[cfg(test)]
    fn direct_projection_recover_after_failed_read(&self) {
        QueryGraph::direct_projection_recover_after_failed_read(&**self)
    }

    fn cache_generation(&self) -> u64 {
        (**self).cache_generation()
    }

    fn config(&self) -> Arc<Config> {
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
