//! Graph's search and lookup helpers: full-text and quick-switch search, templates,
//! block resolution and previews, custom CSS and property facets.

use super::*;

impl Graph {
    /// Full-text search across all blocks.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RefGroup>, crate::query::QueryExecutionError> {
        self.read_friendly_plan(
            &crate::query_plan::QueryPlan::block_search_literal(query, limit),
            false,
            None,
        )
        .map(|answer| crate::query_plan::block_hits_to_groups(answer.hits))
    }

    /// Execute the typed, combined graph-search plan (page names + block text).
    /// Commands and page creation remain frontend providers and are deliberately
    /// outside this graph query result.
    pub fn run_graph_search(
        &self,
        source: &str,
        page_limit: usize,
        block_limit: usize,
        explain: bool,
    ) -> Result<crate::query_plan::QueryExecution, crate::query::QueryExecutionError> {
        self.run_graph_search_scoped(source, page_limit, block_limit, None, explain)
    }

    pub fn run_graph_search_scoped(
        &self,
        source: &str,
        page_limit: usize,
        block_limit: usize,
        scope: Option<crate::query_plan::QueryPageScope>,
        explain: bool,
    ) -> Result<crate::query_plan::QueryExecution, crate::query::QueryExecutionError> {
        self.run_graph_search_displayed(
            source,
            page_limit,
            block_limit,
            scope,
            explain,
            crate::query_plan::FriendlyDisplayOptions::default(),
        )
    }

    /// The same search under stated Display settings (SPEC §7.6, Q3). The
    /// caller has already resolved page/block inheritance; this only builds the
    /// plan those resolved facts describe.
    pub fn run_graph_search_displayed(
        &self,
        source: &str,
        page_limit: usize,
        block_limit: usize,
        scope: Option<crate::query_plan::QueryPageScope>,
        explain: bool,
        display: crate::query_plan::FriendlyDisplayOptions,
    ) -> Result<crate::query_plan::QueryExecution, crate::query::QueryExecutionError> {
        let plan = crate::query_plan::friendly_search_plan(
            source,
            page_limit,
            block_limit,
            scope,
            display,
        );
        self.read_friendly_plan(&plan, explain, None)
    }

    /// Interactive search lane: a newer request in the same lane cooperatively
    /// cancels the older snapshot read. Separate lanes keep the Ctrl-K
    /// switcher and in-editor block picker from canceling one another.
    pub fn search_latest(
        &self,
        lane: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RefGroup>, crate::query::QueryExecutionError> {
        use std::sync::atomic::Ordering;
        let epoch = {
            let mut lanes = self.search_lanes.lock().unwrap();
            lanes
                .entry(lane.to_owned())
                .or_insert_with(|| Arc::new(std::sync::atomic::AtomicU64::new(0)))
                .clone()
        };
        let mine = epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let cancelled: Arc<dyn Fn() -> bool + Send + Sync> =
            Arc::new(move || epoch.load(Ordering::Acquire) != mine);
        self.read_friendly_plan(
            &crate::query_plan::QueryPlan::block_search_literal(query, limit),
            false,
            Some(cancelled),
        )
        .map(|answer| crate::query_plan::block_hits_to_groups(answer.hits))
    }

    /// Latest-wins combined graph search.  It shares the same lane epochs as the
    /// legacy block-search adapter, so migrating a consumer cannot leave an older
    /// request from either API running in that logical lane.
    pub fn run_graph_search_latest(
        &self,
        lane: &str,
        source: &str,
        page_limit: usize,
        block_limit: usize,
        explain: bool,
    ) -> Result<crate::query_plan::QueryExecution, crate::query::QueryExecutionError> {
        self.run_graph_search_latest_scoped(lane, source, page_limit, block_limit, None, explain)
    }

    pub fn run_graph_search_latest_scoped(
        &self,
        lane: &str,
        source: &str,
        page_limit: usize,
        block_limit: usize,
        scope: Option<crate::query_plan::QueryPageScope>,
        explain: bool,
    ) -> Result<crate::query_plan::QueryExecution, crate::query::QueryExecutionError> {
        self.run_graph_search_latest_displayed(
            lane,
            source,
            page_limit,
            block_limit,
            scope,
            explain,
            crate::query_plan::FriendlyDisplayOptions::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run_graph_search_latest_displayed(
        &self,
        lane: &str,
        source: &str,
        page_limit: usize,
        block_limit: usize,
        scope: Option<crate::query_plan::QueryPageScope>,
        explain: bool,
        display: crate::query_plan::FriendlyDisplayOptions,
    ) -> Result<crate::query_plan::QueryExecution, crate::query::QueryExecutionError> {
        use std::sync::atomic::Ordering;
        let epoch = {
            let mut lanes = self.search_lanes.lock().unwrap();
            lanes
                .entry(lane.to_owned())
                .or_insert_with(|| Arc::new(std::sync::atomic::AtomicU64::new(0)))
                .clone()
        };
        let mine = epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let plan = crate::query_plan::friendly_search_plan(
            source,
            page_limit,
            block_limit,
            scope,
            display,
        );
        self.read_friendly_plan(
            &plan,
            explain,
            Some(Arc::new(move || epoch.load(Ordering::Acquire) != mine)),
        )
    }

    fn read_friendly_plan(
        &self,
        plan: &crate::query_plan::QueryPlan,
        explain: bool,
        lane: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Result<crate::query_plan::QueryExecution, crate::query::QueryExecutionError> {
        use crate::query::friendly::{
            friendly_without_read, read_friendly_results, FriendlyReadInputs,
        };
        if let Some(answer) = friendly_without_read(plan, explain, &lane) {
            return Ok(answer);
        }
        let answer = self.dispatch_direct_query(|request| {
            self.direct_projection_read_job(
                request,
                crate::direct_projection::RegistrySensitivity::Insensitive,
                |job| {
                    let identity = crate::query::results::ResultIdentity {
                        session_pages: Arc::clone(&job.session_pages),
                        all_session: false,
                    };
                    read_friendly_results(
                        &mut job.snapshot,
                        &FriendlyReadInputs {
                            plan,
                            graph_root: &self.root,
                            identity: &identity,
                            explain,
                            lane: lane.clone(),
                        },
                    )
                    .map_err(Into::into)
                },
            )
        });
        if lane.as_ref().is_some_and(|cancelled| cancelled()) {
            return Ok(friendly_without_read(plan, explain, &lane).unwrap());
        }
        answer
    }

    /// Fuzzy page-name matches for the quick switcher.
    pub fn quick_switch(&self, query: &str, limit: usize) -> Vec<PageEntry> {
        crate::query_plan::legacy_page_search_entries(
            self.list_pages(),
            self.page_aliases_with_owners(),
            self.referenced_page_names(),
            query,
            limit,
        )
    }

    /// All `template:: <name>` templates across the graph, with the blocks to
    /// insert (ids and template properties stripped).
    pub fn templates(&self) -> Vec<TemplateDto> {
        crate::query::templates(self)
    }

    /// Resolve a `((uuid))` block reference to its shallow identity row.
    pub fn resolve_block(&self, uuid: &str) -> Option<RefGroup> {
        crate::query::resolve_block(self, uuid)
    }

    /// Resolve many block references in one call (for a page full of `((uuid))`
    /// refs / embeds) — one IPC instead of N, and one graph pass instead of N:
    /// hinted ids are grouped + each hinted page scanned once, with a single
    /// whole-graph fallback for hint misses.
    pub fn resolve_blocks(&self, uuids: &[String]) -> Vec<Option<RefGroup>> {
        crate::query::resolve_blocks(self, uuids)
    }

    /// Resolve a bounded subtree for an explicitly expanded preview/export.
    pub fn preview_block(&self, uuid: &str, max_nodes: usize) -> Option<BlockPreview> {
        crate::query::preview_block(self, uuid, max_nodes)
    }

    pub fn preview_block_with_budget(
        &self,
        uuid: &str,
        max_nodes: usize,
        max_bytes: usize,
    ) -> Option<BlockPreview> {
        crate::query::preview_block_with_budget(self, uuid, max_nodes, max_bytes)
    }

    /// The graph's `logseq/custom.css`, if present (for user theming).
    pub fn custom_css(&self) -> String {
        std::fs::read_to_string(self.root.join("logseq").join("custom.css")).unwrap_or_default()
    }

    /// Property keys (with their distinct values) used across the graph, for the
    /// query builder's property-filter autocomplete. Excludes internal/metadata
    /// properties (id, collapsed, hl-*, …).
    pub fn property_facets(&self) -> Vec<(String, Vec<String>)> {
        self.property_facets_bounded(usize::MAX, usize::MAX).0
    }

    pub fn property_facets_bounded(
        &self,
        max_values: usize,
        max_bytes: usize,
    ) -> (Vec<(String, Vec<String>)>, bool) {
        if self.direct_projection_ready() {
            if let Some(result) =
                self.direct_projection_property_facets(false, max_values, max_bytes)
            {
                return result;
            }
        }
        self.direct_projection_note_fallback_read();
        crate::query::property_facets_bounded(self, max_values, max_bytes)
    }

    pub fn autocomplete_property_facets_bounded(
        &self,
        max_items: usize,
        max_bytes: usize,
    ) -> (Vec<(String, Vec<String>)>, bool) {
        if self.direct_projection_ready() {
            if let Some(result) = self.direct_projection_property_facets(true, max_items, max_bytes)
            {
                return result;
            }
        }
        self.direct_projection_note_fallback_read();
        crate::query::autocomplete_property_facets_bounded(self, max_items, max_bytes)
    }

    // ---- Assets & PDF highlights ----
}
