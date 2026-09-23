//! Property facets: the property keys and values a graph uses, behind the
//! facet panel and property autocomplete, with the internal keys they hide.

use super::*;

/// Properties that are internal/metadata and shouldn't be offered as query
/// filters (mirrors the frontend's hidden-property set).
const INTERNAL_PROPS: &[&str] = &[
    "id",
    "collapsed",
    "hl-page",
    "hl-color",
    "hl-type",
    "ls-type",
    "background-color",
    "logseq.order-list-type",
    "template",
    "template-including-parent",
];

/// The built-in half of the registry's internal-key exclusion (§6.2 K15). ONE
/// definition: the registry excludes this set ∪ the user's configured
/// `hidden_properties` ∪ every `tine.*` key, and the query-builder facets hide
/// exactly this set.
pub fn internal_property_keys() -> &'static [&'static str] {
    INTERNAL_PROPS
}

#[derive(Clone, Copy)]
pub(crate) enum PropertyFacetMode {
    QueryBuilder,
    Autocomplete,
}

pub(crate) struct PropertyFacetAccumulator {
    mode: PropertyFacetMode,
    hidden: std::collections::HashSet<String>,
    max_items: usize,
    max_bytes: usize,
    items: usize,
    bytes: usize,
    exceeded: bool,
    map: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}

impl PropertyFacetAccumulator {
    pub(crate) fn query_builder(max_values: usize, max_bytes: usize) -> Self {
        Self::new(PropertyFacetMode::QueryBuilder, &[], max_values, max_bytes)
    }

    pub(crate) fn autocomplete(
        extra_hidden: &[String],
        max_items: usize,
        max_bytes: usize,
    ) -> Self {
        Self::new(
            PropertyFacetMode::Autocomplete,
            extra_hidden,
            max_items,
            max_bytes,
        )
    }

    fn new(
        mode: PropertyFacetMode,
        extra_hidden: &[String],
        max_items: usize,
        max_bytes: usize,
    ) -> Self {
        let hidden = match mode {
            PropertyFacetMode::QueryBuilder => INTERNAL_PROPS
                .iter()
                .map(|key| property_key_norm(key))
                .collect(),
            PropertyFacetMode::Autocomplete => OG_AUTOCOMPLETE_HIDDEN_PROPS
                .iter()
                .map(|key| property_key_norm(key))
                .chain(
                    extra_hidden
                        .iter()
                        .map(|key| property_key_norm(key.trim_start_matches(':'))),
                )
                .collect(),
        };
        Self {
            mode,
            hidden,
            max_items,
            max_bytes,
            items: 0,
            bytes: 0,
            exceeded: false,
            map: std::collections::BTreeMap::new(),
        }
    }

    pub(crate) fn offer(&mut self, source_key: &str, source_value: &str) {
        let key = property_key_norm(source_key);
        if key.is_empty() || self.hidden.contains(&key) {
            return;
        }
        let key_missing = !self.map.contains_key(&key);
        match self.mode {
            PropertyFacetMode::QueryBuilder => {
                let value = source_value;
                if value.trim().is_empty()
                    || self
                        .map
                        .get(&key)
                        .is_some_and(|values| values.contains(value))
                {
                    return;
                }
                let key_bytes = if key_missing { key.len() + 64 } else { 0 };
                let next_bytes = self
                    .bytes
                    .saturating_add(key_bytes)
                    .saturating_add(value.len())
                    .saturating_add(64);
                if self.items >= self.max_items || next_bytes > self.max_bytes {
                    self.exceeded = true;
                    return;
                }
                self.items += 1;
                self.bytes = next_bytes;
                self.map.entry(key).or_default().insert(value.to_string());
            }
            PropertyFacetMode::Autocomplete => {
                if key_missing {
                    let key_bytes = key.len().saturating_add(64);
                    if self.items >= self.max_items
                        || self.bytes.saturating_add(key_bytes) > self.max_bytes
                    {
                        self.exceeded = true;
                        return;
                    }
                    self.items += 1;
                    self.bytes = self.bytes.saturating_add(key_bytes);
                    self.map
                        .insert(key.clone(), std::collections::BTreeSet::new());
                }
                let value = source_value.trim();
                if value.is_empty()
                    || self
                        .map
                        .get(&key)
                        .is_some_and(|values| values.contains(value))
                {
                    return;
                }
                let value_bytes = value.len().saturating_add(64);
                if self.items >= self.max_items
                    || self.bytes.saturating_add(value_bytes) > self.max_bytes
                {
                    self.exceeded = true;
                    return;
                }
                self.items += 1;
                self.bytes = self.bytes.saturating_add(value_bytes);
                self.map.entry(key).or_default().insert(value.to_string());
            }
        }
    }

    pub(crate) fn finish(self) -> (Vec<(String, Vec<String>)>, bool) {
        (
            self.map
                .into_iter()
                .map(|(key, values)| (key, values.into_iter().collect()))
                .collect(),
            self.exceeded,
        )
    }
}

/// Distinct property keys (each with its sorted distinct values) used across the
/// graph. Drives the query builder's property-filter pickers.
pub fn property_facets(graph: &impl QueryGraph) -> Vec<(String, Vec<String>)> {
    property_facets_bounded(graph, usize::MAX, usize::MAX).0
}

pub fn property_facets_bounded(
    graph: &impl QueryGraph,
    max_values: usize,
    max_bytes: usize,
) -> (Vec<(String, Vec<String>)>, bool) {
    property_facets_bounded_over(
        graph,
        crate::query::graph::PageFallback::Parse,
        max_values,
        max_bytes,
    )
}

/// [`property_facets_bounded`] over the pages an index decline named.
pub(crate) fn property_facets_bounded_over(
    graph: &impl QueryGraph,
    pages: crate::query::graph::PageFallback,
    max_values: usize,
    max_bytes: usize,
) -> (Vec<(String, Vec<String>)>, bool) {
    let mut accumulator = PropertyFacetAccumulator::query_builder(max_values, max_bytes);
    pages.with_pages(graph, |pages| {
        for (_entry, doc) in pages {
            walk(&doc.roots, &mut |b| {
                for (k, v) in b.properties() {
                    accumulator.offer(&k, &v);
                }
            });
        }
    });
    accumulator.finish()
}

/// OG-visible property names and their distinct values for editor completion.
/// Unlike query-builder facets, this includes page preambles and editable
/// built-ins such as `template`/`title`, while applying graph-configured hidden
/// keys. OG sources: db/model.cljs:1394-1405,1422-1443; search.cljs:184-215;
/// util/property.cljs:18-24 at checkout 6e7afa8eb.
const OG_AUTOCOMPLETE_HIDDEN_PROPS: &[&str] = &[
    "id",
    "custom-id",
    "background-color",
    "background_color",
    "heading",
    "collapsed",
    "created-at",
    "updated-at",
    "last-modified-at",
    "created_at",
    "last_modified_at",
    "query-table",
    "query-properties",
    "query-sort-by",
    "query-sort-desc",
    "ls-type",
    "hl-type",
    "hl-page",
    "hl-stamp",
    "hl-color",
    "logseq.macro-name",
    "logseq.macro-arguments",
    "logseq.order-list-type",
    "logseq.tldraw.page",
    "logseq.tldraw.shape",
    "todo",
    "doing",
    "now",
    "later",
    "done",
];

pub fn autocomplete_property_facets_bounded<G: QueryGraph>(
    graph: &G,
    max_items: usize,
    max_bytes: usize,
) -> (Vec<(String, Vec<String>)>, bool) {
    autocomplete_property_facets_bounded_over(
        graph,
        crate::query::graph::PageFallback::Parse,
        max_items,
        max_bytes,
    )
}

/// [`autocomplete_property_facets_bounded`] over the pages an index decline
/// named.
pub(crate) fn autocomplete_property_facets_bounded_over<G: QueryGraph>(
    graph: &G,
    pages: crate::query::graph::PageFallback,
    max_items: usize,
    max_bytes: usize,
) -> (Vec<(String, Vec<String>)>, bool) {
    let mut accumulator = PropertyFacetAccumulator::autocomplete(
        &graph.config().block_hidden_properties,
        max_items,
        max_bytes,
    );
    pages.with_pages(graph, |pages| {
        for (entry, doc) in pages {
            let is_org = Format::from_path(std::path::Path::new(&entry.rel_path)) == Format::Org;
            for (key, value) in page_properties(doc.pre_block.as_deref(), is_org) {
                accumulator.offer(&key, &value);
            }
            walk(&doc.roots, &mut |block| {
                for (key, value) in block.properties() {
                    accumulator.offer(&key, &value);
                }
            });
        }
    });
    accumulator.finish()
}
