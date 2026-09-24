//! Backlinks and the `{{query}}` engine's shared front half: the OG DSL, TQL
//! and advanced-datalog parsers, the IR they lower to, and the result
//! finishers. Queries are answered by the SQL lowering (`sql`). The in-memory
//! walk (`walk`, with its evaluator `eval`) is compiled only under
//! `cfg(test)`, as that lowering's oracle. Advanced datalog outside the mapped
//! clause subset is reported as unsupported rather than guessed.

// Public query entry points retain their established signatures while this
// crate-private capability keeps the query layer independent of `model::Graph`.
#![allow(private_bounds)]

pub mod atom;
mod export_select;
pub use export_select::*;
mod facets;
pub use facets::*;
pub(crate) mod candidate;
pub(crate) mod compiled;
#[cfg(test)]
mod conformance;
pub mod derived;
#[cfg(test)]
pub(crate) mod eval;
mod execution_error;
#[cfg(test)]
#[path = "query/oracle_gate1_tests.rs"]
mod oracle_gate1;
#[cfg(test)]
#[path = "query/oracle_walk_tests.rs"]
mod oracle_walk;
pub use execution_error::{
    IndexFailureClass, QueryExecutionError, QueryReadinessReason, QueryUnavailableReason,
};
mod advanced_patterns;
use advanced_patterns::{scan_groups, where_groups};
pub mod ir;
pub mod macro_text;
pub(crate) mod og;
pub mod path_refs;
pub mod print;
pub mod registry;
pub(crate) mod registry_cache;
pub(crate) mod registry_sql;
pub(crate) mod sort;
// §5.1–§5.7's compiler. Its two acceptance gates live in `sql_gates_tests.rs`,
// declared from `sql.rs` itself: the production-source scanner every census
// guard shares recognises a `*_tests.rs` file included by a SIBLING under
// `#[cfg(test)]`, and only then does it stop counting the gates' `eprintln!`
// receipts as production print sites (I-5).
pub(crate) mod projection_sql;
pub(crate) mod rank;
pub(crate) mod read_execute;
pub(crate) mod results;
pub(crate) mod sql;
pub(crate) mod statistics;
pub(crate) mod text;
pub(crate) use text::QUERY_NESTING_MAX;
pub use text::{
    is_advanced, query_nesting_within_limit, query_source_within_limit, QUERY_SOURCE_MAX_BYTES,
};
// Database-owned export subtree construction: located selection over the
// shared result collector, and bounded subtree hydration over the SAME
// caller-owned snapshots.
pub(crate) mod export_execute;
pub(crate) mod export_results;
pub(crate) mod friendly;
pub(crate) mod graph;
pub(crate) mod tql;
pub mod view;
// The walk: the SQL lowering's correctness oracle, test-only (see `walk.rs`).
#[cfg(test)]
mod walk;
#[cfg(test)]
pub(crate) use walk::*;
pub mod wire_parse;

#[cfg(test)]
use eval::EvalCtx;
use ir::{Anchor, Attr, CmpOp, Filter, Quant, Query, Rel, SortDir, Source, Value, ViewSettings};

use self::sort::{compare_sort_decorations, lexical_property_sort_text, SortDecor};
use crate::date::{JournalDate, JournalFormat};
use crate::direct_projection::derived_reads::DerivedSelection;
use crate::doc::{property_key_norm, DocBlock, Document};
#[cfg(test)]
use crate::model::Graph;
use crate::refs;
use crate::search_query::{canonical_fold, Matcher};
use crate::vocab::{
    block_to_shallow_dto, BacklinkFilterContext, BacklinkFilterEntry, BacklinkFilterTarget,
    BlockDto, BlockPreview, Format, PageEntry, PageKind, RefGroup, ReferenceBlockEvidence,
    ReferenceDiagnosticTrace, ReferenceDiagnostics, ReferenceKind, TemplateDto,
};
use graph::QueryGraph;
#[cfg(test)]
use ir::Leaf;
use std::collections::HashMap;
#[cfg(test)]
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct BoundedGroups {
    pub statistics: Option<ir::QueryStatistics>,
    pub matched_total: Option<usize>,
    pub groups: Vec<RefGroup>,
    pub total: usize,
    pub exceeded: bool,
}

/// An indexed-panel answer plus the source that actually produced it.
///
/// The panel deliberately falls back to the parser when no projection can
/// become ready. That answer is correct for the current source generation,
/// but it is not interchangeable with the Interactive verified window once
/// the projection becomes ready at that same generation.
pub(crate) struct IndexedReferenceGroups {
    pub(crate) groups: BoundedGroups,
    pub(crate) memo_eligible: bool,
}

/// The ONE result-construction accounting rule.
///
/// It exists as a type rather than as an open-coded pair of counters because
/// two producers once drifted apart on exactly this: one charged a group
/// overhead per row and the other once per emitted group, so the same
/// `max_bytes` admitted a different number of rows for identical content.
pub(crate) struct ConstructionBudget {
    max_rows: usize,
    max_bytes: usize,
    rows: usize,
    bytes: usize,
    pub(crate) total: usize,
    pub(crate) exceeded: bool,
}

impl ConstructionBudget {
    pub(crate) fn new(max_rows: usize, max_bytes: usize) -> Self {
        Self {
            max_rows,
            max_bytes,
            rows: 0,
            bytes: 0,
            total: 0,
            exceeded: false,
        }
    }

    pub(crate) fn admit_estimated(&mut self, page: &str, payload_bytes: usize) -> bool {
        self.total = self.total.saturating_add(1);
        let bytes = payload_bytes.saturating_add(page.len()).saturating_add(256);
        if self.exceeded
            || self.rows >= self.max_rows
            || self.bytes.saturating_add(bytes) > self.max_bytes
        {
            self.exceeded = true;
            return false;
        }
        self.rows += 1;
        self.bytes += bytes;
        true
    }

    /// Admit one shallow page row at its producer-owned raw estimate.
    ///
    /// Page estimates already include name, path, properties and their fixed
    /// row allowance, so adding the block result's page/group overhead would
    /// count the same bytes twice. Page `total` remains admitted rows.
    pub(crate) fn admit_page_estimated(&mut self, estimated_bytes: usize) -> bool {
        if self.exceeded
            || self.rows >= self.max_rows
            || self.bytes.saturating_add(estimated_bytes) > self.max_bytes
        {
            self.exceeded = true;
            return false;
        }
        self.rows += 1;
        self.bytes += estimated_bytes;
        self.total = self.rows;
        true
    }

    pub(crate) fn deny_match(&mut self) {
        self.total = self.total.saturating_add(1);
        self.exceeded = true;
    }

    pub(crate) fn closed(&self) -> bool {
        self.exceeded || self.rows >= self.max_rows
    }
}

/// A page already declared to [`BoundedReferenceGroups`]. Opaque so a caller
/// cannot fabricate a slot for a page it never declared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReferencePageSlot(usize);

struct BoundedReferenceGroup {
    date_key: Option<i64>,
    order_key: String,
    group: RefGroup,
}

/// The ONE bounded reference-result accumulator (I-12, I-13).
///
/// Reference producers each used to own a private copy of the same four rules —
/// [`ConstructionBudget`] admission, grouping duplicate logical page names under
/// the canonical [`crate::refs::page_key`], carrying `total`/`exceeded`, and the
/// OG display order — and the copies had drifted: one grouped by storage path,
/// so two files whose titles fold to one logical page produced two reference
/// groups. They are call sites now; this type is the algorithm.
///
/// Rows are admitted in the caller's declaration order, which every caller makes
/// source-path order, because the budget truncates and Direct Files charges
/// page-by-page in path order. Display order is applied only at [`Self::finish`].
///
/// `evidence` is parallel to `blocks` within a group. A producer is either
/// evidence-bearing for every row (backlinks, unlinked references) or for none
/// (block referrers); mixing the two within one group is not a supported shape.
pub(crate) struct BoundedReferenceGroups {
    budget: ConstructionBudget,
    declarations: Vec<ReferencePageDeclaration>,
    groups: Vec<BoundedReferenceGroup>,
    by_key: HashMap<String, usize>,
}

struct ReferencePageDeclaration {
    order_key: String,
    name: String,
    kind: PageKind,
    date_key: Option<i64>,
    group: Option<usize>,
}

impl BoundedReferenceGroups {
    pub(crate) fn new(max_rows: usize, max_bytes: usize) -> Self {
        Self {
            budget: ConstructionBudget::new(max_rows, max_bytes),
            declarations: Vec::new(),
            groups: Vec::new(),
            by_key: HashMap::new(),
        }
    }

    /// Declare the page whose rows follow. Declaring is free: a page that
    /// retains no row never reaches the result and never contributes its
    /// journal day or path to a group it shares a canonical key with.
    pub(crate) fn page(
        &mut self,
        order_key: &str,
        name: &str,
        kind: PageKind,
        date_key: Option<i64>,
    ) -> ReferencePageSlot {
        self.declarations.push(ReferencePageDeclaration {
            order_key: order_key.to_owned(),
            name: name.to_owned(),
            kind,
            date_key,
            group: None,
        });
        ReferencePageSlot(self.declarations.len() - 1)
    }

    pub(crate) fn closed(&self) -> bool {
        self.budget.closed()
    }

    /// Count a match the budget will not construct.
    pub(crate) fn deny(&mut self) {
        self.budget.deny_match();
    }

    /// Create or join the group this declaration belongs to, merging duplicate
    /// logical page names under the canonical [`crate::refs::page_key`].
    /// `order_key` (the source path in every caller) only breaks a display tie;
    /// the smallest wins, so order never depends on which duplicate came first.
    fn group_for(&mut self, slot: ReferencePageSlot) -> usize {
        if let Some(index) = self.declarations[slot.0].group {
            return index;
        }
        let key = refs::page_key(&self.declarations[slot.0].name);
        let index = match self.by_key.get(&key).copied() {
            Some(index) => {
                let declaration = &self.declarations[slot.0];
                let existing = &mut self.groups[index];
                existing.date_key = match (existing.date_key, declaration.date_key) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (current @ Some(_), None) => current,
                    (None, other) => other,
                };
                if declaration.order_key < existing.order_key {
                    existing.order_key.clone_from(&declaration.order_key);
                }
                index
            }
            None => {
                let declaration = &self.declarations[slot.0];
                let index = self.groups.len();
                self.groups.push(BoundedReferenceGroup {
                    date_key: declaration.date_key,
                    order_key: declaration.order_key.clone(),
                    group: RefGroup {
                        page: declaration.name.clone(),
                        kind: declaration.kind,
                        blocks: Vec::new(),
                        evidence: Vec::new(),
                    },
                });
                self.by_key.insert(key, index);
                index
            }
        };
        self.declarations[slot.0].group = Some(index);
        index
    }

    /// Charge one row against the shared budget and, only if it is admitted,
    /// build and retain it. The row is constructed inside `build` so an
    /// over-budget match costs an estimate, never a DTO.
    pub(crate) fn admit_with(
        &mut self,
        slot: ReferencePageSlot,
        payload_bytes: usize,
        build: impl FnOnce() -> (BlockDto, Option<ReferenceBlockEvidence>),
    ) -> bool {
        let index = self.group_for(slot);
        let Self { budget, groups, .. } = self;
        let entry = &mut groups[index];
        if !budget.admit_estimated(&entry.group.page, payload_bytes) {
            return false;
        }
        let (block, evidence) = build();
        entry.group.blocks.push(block);
        if let Some(evidence) = evidence {
            entry.group.evidence.push(evidence);
        }
        true
    }

    /// [`Self::admit_with`] for a caller that already owns the constructed row.
    pub(crate) fn admit(
        &mut self,
        slot: ReferencePageSlot,
        block: BlockDto,
        evidence: Option<ReferenceBlockEvidence>,
        payload_bytes: usize,
    ) -> bool {
        self.admit_with(slot, payload_bytes, move || (block, evidence))
    }

    /// Drop the pages that retained nothing and apply the OG display order:
    /// newest journal day first, non-journal pages last, page name as the
    /// deterministic tie-break, source path breaking a name tie.
    pub(crate) fn finish(self) -> BoundedGroups {
        let mut groups = self.groups;
        groups.retain(|entry| !entry.group.blocks.is_empty());
        groups.sort_by(|a, b| reference_group_display_order(a, b));
        BoundedGroups {
            matched_total: None,
            statistics: None,
            groups: groups.into_iter().map(|entry| entry.group).collect(),
            total: self.budget.total,
            exceeded: self.budget.exceeded,
        }
    }
}

/// The ONE display order for reference groups: journal day descending, then
/// page name, then the group's admission key.
///
/// It has exactly one production call site --
/// [`BoundedReferenceGroups::finish`] -- because every reference surface
/// reaches display order through that one accumulator. A
/// second caller would mean a second producer of this answer (I-12).
fn reference_group_display_order(
    a: &BoundedReferenceGroup,
    b: &BoundedReferenceGroup,
) -> std::cmp::Ordering {
    b.date_key
        .unwrap_or(i64::MIN)
        .cmp(&a.date_key.unwrap_or(i64::MIN))
        .then_with(|| a.group.page.cmp(&b.group.page))
        .then_with(|| a.order_key.cmp(&b.order_key))
}

/// The `yyyymmdd` ordinal of a journal title read with the DEFAULT title format
/// (e.g. "Jan 1st, 2022").
///
/// This is NOT the producer of "what day is this journal page". That question
/// has one config-aware owner, [`crate::date::JournalFormat::parse`], which is
/// what fills `PageEntry::date_key`. Deriving the day from the title with the
/// default format instead is what made the removed Managed Storage query path
/// answer journal-range queries empty on every graph configuring a custom
/// `:journal/page-title-format` (REG-W4-C7B-MANAGED-JOURNAL-ORDINAL-001,
/// retired with that path).
///
/// ONE caller survives: [`resolve_date_token`], which reads a `(between …)`
/// BOUND LITERAL the user typed rather than a page's day.
///
/// Giving it the configured format is a follow-up, not a parity defect.
fn journal_ordinal(title: &str) -> Option<i64> {
    JournalDate::from_title(title).map(|d| d.ordinal_key())
}

/// Walk all blocks of a document depth-first, calling `f(block)`.
fn walk<'a>(blocks: &'a [DocBlock], f: &mut impl FnMut(&'a DocBlock)) {
    for b in blocks {
        f(b);
        walk(&b.children, f);
    }
}

#[cfg(test)]
use crate::query::path_refs::PathRefCounts;

/// Collect matches in document order while evaluating every candidate exactly
/// once. OG query presentation removes a result only when its *immediate parent*
/// is also in the unfiltered result set (`tree/filter-top-level-blocks`); it does
/// not prune the rest of a matching block's subtree. Reference occurrence
/// surfaces use `suppress_direct_child = false` because every referring block is
/// independently countable/navigable.
fn collect_matching_path<'a, M, T>(
    blocks: &'a [DocBlock],
    path: &mut Vec<&'a DocBlock>,
    parent_matched: bool,
    suppress_direct_child: bool,
    classify: &mut impl FnMut(&'a DocBlock, &[&'a DocBlock]) -> Option<M>,
    materialize: &mut impl FnMut(&'a DocBlock, &[&'a DocBlock], M) -> Option<T>,
    out: &mut Vec<T>,
) {
    for block in blocks {
        let classification = classify(block, path);
        let matched = classification.is_some();
        if !suppress_direct_child || !parent_matched {
            if let Some(classification) = classification {
                if let Some(item) = materialize(block, path, classification) {
                    out.push(item);
                }
            }
        }
        path.push(block);
        collect_matching_path(
            &block.children,
            path,
            matched,
            suppress_direct_child,
            classify,
            materialize,
            out,
        );
        path.pop();
    }
}

fn collect_reference_matches<'a, M, T>(
    blocks: &'a [DocBlock],
    path: &mut Vec<&'a DocBlock>,
    classify: &mut impl FnMut(&'a DocBlock, &[&'a DocBlock]) -> Option<M>,
    materialize: &mut impl FnMut(&'a DocBlock, &[&'a DocBlock], M) -> Option<T>,
    out: &mut Vec<T>,
) {
    collect_matching_path(blocks, path, false, false, classify, materialize, out);
}

fn crumb_line_estimated_bytes(block: &DocBlock) -> usize {
    let line = block.visible_text().lines().next().unwrap_or("").trim();
    let mut chars = line.chars();
    let bytes = chars
        .by_ref()
        .take(crate::doc::CRUMB_MAX_CHARS)
        .map(char::len_utf8)
        .sum::<usize>();
    bytes + usize::from(chars.next().is_some()) * '…'.len_utf8()
}

fn shallow_dto_estimated_bytes(block: &DocBlock, ancestors: &[&DocBlock]) -> usize {
    let projection = block.projection();
    tine_storage::sqlite::query_result_estimated_bytes(
        &block.uuid,
        &block.raw,
        projection.tags.iter().map(String::as_str),
        projection
            .properties
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )
    .saturating_add(
        ancestors
            .iter()
            .map(|ancestor| crumb_line_estimated_bytes(ancestor))
            .sum::<usize>(),
    )
}

pub(crate) fn reference_evidence_estimated_bytes(evidence: &ReferenceBlockEvidence) -> usize {
    evidence.block_id.len()
        + evidence
            .occurrences
            .iter()
            .map(|occurrence| {
                occurrence
                    .matched_name
                    .len()
                    .saturating_add(occurrence.canonical.len())
                    .saturating_add(occurrence.rule.len())
                    .saturating_add(std::mem::size_of_val(occurrence))
            })
            .sum::<usize>()
}

fn result_dto(block: &DocBlock) -> BlockDto {
    #[cfg(test)]
    RESULT_DTO_CONSTRUCTIONS.with(|count| count.set(count.get().saturating_add(1)));
    block_to_shallow_dto(block)
}

#[cfg(test)]
thread_local! {
    static RESULT_DTO_CONSTRUCTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Cancellable variant used by interactive search. Returning false from `f`
/// stops the entire depth-first walk, including the current deep page.
/// Collect matching blocks from an exact candidate set, or from the complete
/// already-parsed graph when no safe candidate set is available. The parser
/// remains the semantic authority; this helper performs no disk I/O or parsing.
fn with_candidate_pages<G: QueryGraph, T>(
    graph: &G,
    candidates: CandidatePages,
    f: impl FnOnce(&[(PageEntry, std::sync::Arc<Document>)]) -> T,
) -> T {
    match candidates {
        Ok(pages) => f(&pages),
        Err(fallback) => fallback.with_pages(graph, f),
    }
}

/// An index's exact candidate pages, or how to answer without them.
type CandidatePages =
    Result<Vec<(PageEntry, std::sync::Arc<Document>)>, crate::query::graph::PageFallback>;

fn collect_bounded_candidates<G: QueryGraph>(
    graph: &G,
    candidate_pages: CandidatePages,
    mut keep: impl FnMut(&DocBlock) -> bool,
    mut keep_page_properties: impl FnMut(&PageEntry, &str) -> Option<BlockDto>,
    exclude: Option<&str>,
    max_rows: usize,
    max_bytes: usize,
) -> BoundedGroups {
    let ex = exclude.map(refs::normalize);
    let mut budget = ConstructionBudget::new(max_rows, max_bytes);
    let groups = with_candidate_pages(graph, candidate_pages, |pages| {
        // Pair each group with the referring page's journal `date_key` so the result
        // can be ordered like OG (the page cache itself is in arbitrary read_dir order).
        let mut groups: Vec<(Option<i64>, RefGroup)> = Vec::new();
        for (entry, doc) in pages {
            if ex.as_deref() == Some(&refs::normalize(&entry.name)) {
                continue;
            }
            let mut matched: Vec<BlockDto> = Vec::new();
            if let Some(pre) = doc.pre_block.as_deref() {
                if let Some(property_ref) = keep_page_properties(entry, pre) {
                    if budget.admit_estimated(
                        &entry.name,
                        crate::vocab::block_dto_estimated_bytes(&property_ref),
                    ) {
                        matched.push(property_ref);
                    }
                }
            }
            let mut path: Vec<&DocBlock> = Vec::new();
            collect_reference_matches(
                &doc.roots,
                &mut path,
                &mut |block, _| keep(block).then_some(()),
                &mut |block, ancestors, ()| {
                    if budget.closed() {
                        budget.deny_match();
                        return None;
                    }
                    if !budget
                        .admit_estimated(&entry.name, shallow_dto_estimated_bytes(block, ancestors))
                    {
                        return None;
                    }
                    let mut dto = result_dto(block);
                    dto.breadcrumb = ancestors
                        .iter()
                        .map(|ancestor| crate::doc::crumb_line(ancestor))
                        .collect();
                    Some(dto)
                },
                &mut matched,
            );
            if !matched.is_empty() {
                groups.push((
                    entry.date_key,
                    RefGroup {
                        page: entry.name.clone(),
                        kind: entry.kind,
                        blocks: matched,
                        evidence: Vec::new(),
                    },
                ));
            }
        }
        // OG parity (components/block.cljs:3521 `sort-by :block/journal-day >`): order the
        // reference groups by the referring page's journal day DESCENDING — newest journal
        // day first, non-journal pages (date_key None → i64::MIN) last. The graph cache
        // inherits filesystem enumeration order, so use the page name as a deterministic
        // tie-breaker. Without it, static Guide exports differed across machines.
        groups.sort_by(|a, b| {
            b.0.unwrap_or(i64::MIN)
                .cmp(&a.0.unwrap_or(i64::MIN))
                .then_with(|| a.1.page.cmp(&b.1.page))
        });
        groups.into_iter().map(|(_, g)| g).collect()
    });
    BoundedGroups {
        matched_total: None,
        statistics: None,
        groups,
        total: budget.total,
        exceeded: budget.exceeded,
    }
}

/// True when every non-empty line of a block's raw text is a `key:: value`
/// property line — i.e. the block carries only properties. OG treats such a
/// FIRST block as the page-properties (pre-)block. Empty (no property) → false.
fn is_properties_only(raw: &str) -> bool {
    let mut saw_prop = false;
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if crate::doc::parse_property_line(line).is_none() {
            return false;
        }
        saw_prop = true;
    }
    saw_prop
}

/// Map of `alias::` → canonical page name (original case). The alias key is
/// normalized for lookup. Page-level `alias::` comes from the page pre-block
/// (Logseq's on-disk file convention) OR — when the user typed it as the first
/// bullet in the outliner — from a properties-only first block, which OG also
/// treats as page properties (GH #62). Without the latter, `- alias:: book`
/// typed in the editor never registers as an alias, so link navigation and
/// backlinks don't merge the two pages.
/// The normalized aliases contributed by one document, using the exact same
/// page-property rules as [`page_aliases`]. Keeping this extraction shared also
/// lets cache invalidation compare the old and new semantic alias sets instead
/// of treating the mere presence of an unchanged `alias::` line as a change.
pub(crate) fn document_alias_spellings(doc: &Document) -> Vec<(String, String)> {
    let alias_text: Option<&str> = match &doc.pre_block {
        Some(pre) => Some(pre.as_str()),
        // No pre-block: a properties-only FIRST block is the page-properties
        // block in OG (it gets written back as a pre-block on save there).
        None => doc
            .roots
            .first()
            .filter(|b| is_properties_only(&b.raw))
            .map(|b| b.raw.as_str()),
    };
    let Some(text) = alias_text else {
        return Vec::new();
    };
    let mut aliases = Vec::new();
    for line in text.lines() {
        if let Some((k, v)) = crate::doc::parse_property_line(line) {
            let key = property_key_norm(&k);
            if key == "alias" || key == "aliases" {
                let trimmed = v.trim();
                if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
                    continue;
                }
                for alias in v.split([',', '，']) {
                    let alias = strip_ref(alias.trim());
                    if !alias.is_empty() {
                        aliases.push((alias.clone(), refs::page_key(&alias)));
                    }
                }
            }
        }
    }
    // Ordering and duplicate spelling do not alter alias resolution. Comparing
    // the semantic set avoids graph-wide invalidation for harmless formatting.
    aliases.sort_unstable();
    aliases.dedup();
    aliases
}

pub(crate) fn document_aliases(doc: &Document) -> Vec<String> {
    let mut aliases = document_alias_spellings(doc)
        .into_iter()
        .map(|(_, key)| key)
        .collect::<Vec<_>>();
    aliases.sort_unstable();
    aliases.dedup();
    aliases
}

fn sorted_alias_owners(
    mut owned: Vec<(std::path::PathBuf, String, String)>,
) -> Vec<(String, String)> {
    // Keep every owner for duplicate aliases. Sorting makes the public alias
    // relation stable without collapsing edges needed by component resolution.
    owned.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    owned
        .into_iter()
        .map(|(_, alias, owner)| (alias, owner))
        .collect()
}

pub fn page_aliases<G: QueryGraph>(graph: &G) -> Vec<(String, String)> {
    graph.with_pages(|pages| {
        let mut owned = Vec::new();
        for (entry, doc) in pages {
            for (alias, _) in document_alias_spellings(doc) {
                owned.push((entry.path.clone(), alias, entry.name.clone()));
            }
        }
        sorted_alias_owners(owned)
    })
}

/// The parser oracle for the alias rows; the product reads them through
/// `Graph::page_aliases_with_owners`, which asks the index first.
#[cfg(test)]
pub(crate) fn page_aliases_with_owners<G: QueryGraph>(graph: &G) -> Vec<(String, String, String)> {
    graph.with_pages(page_aliases_with_owners_from_pages)
}

pub(crate) fn page_aliases_with_owners_from_pages(
    pages: &[(PageEntry, std::sync::Arc<Document>)],
) -> Vec<(String, String, String)> {
    let mut owned = Vec::new();
    for (entry, doc) in pages {
        for (alias, _) in document_alias_spellings(doc) {
            owned.push((
                entry.path.clone(),
                alias,
                entry.name.clone(),
                entry.rel_path.clone(),
            ));
        }
    }
    owned.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    owned
        .into_iter()
        .map(|(_, alias, owner, owner_rel_path)| (alias, owner, owner_rel_path))
        .collect()
}

pub(crate) fn referenced_page_names_from_snapshot_cancellable(
    pages: &[(PageEntry, std::sync::Arc<Document>)],
    cancelled: &impl Fn() -> bool,
) -> Option<Vec<String>> {
    fn add(seen: &mut std::collections::HashSet<String>, names: &mut Vec<String>, name: String) {
        if !name.is_empty() && seen.insert(crate::refs::page_key(&name)) {
            names.push(name);
        }
    }

    let mut seen = std::collections::HashSet::new();
    let mut names = Vec::new();
    for (_, doc) in pages {
        if cancelled() {
            return None;
        }
        if let Some(pre) = &doc.pre_block {
            for name in crate::doc::property_reference_page_names(pre) {
                if cancelled() {
                    return None;
                }
                add(&mut seen, &mut names, name);
            }
        }
        let mut frames: [Option<std::slice::Iter<'_, DocBlock>>; crate::vocab::MAX_BLOCK_DEPTH] =
            std::array::from_fn(|_| None);
        let mut len = usize::from(!doc.roots.is_empty());
        if len != 0 {
            frames[0] = Some(doc.roots.iter());
        }
        while len != 0 {
            if cancelled() {
                return None;
            }
            let mut frame = frames[len - 1]
                .take()
                .expect("active cached-reference frame");
            let Some(block) = frame.next() else {
                len -= 1;
                continue;
            };
            frames[len - 1] = Some(frame);
            for name in &block.projection().refs_page {
                add(&mut seen, &mut names, name.clone());
            }
            for name in crate::doc::property_reference_page_names(&block.raw) {
                if cancelled() {
                    return None;
                }
                add(&mut seen, &mut names, name);
            }
            if !block.children.is_empty() {
                if len == crate::vocab::MAX_BLOCK_DEPTH {
                    return Some(Vec::new());
                }
                frames[len] = Some(block.children.iter());
                len += 1;
            }
        }
    }
    Some(names)
}

pub(crate) type RealPageNames = std::collections::HashMap<String, (std::path::PathBuf, String)>;

pub(crate) struct BacklinkFilterScope {
    pub(crate) names_norm: Vec<String>,
    pub(crate) pages: Vec<(PageEntry, std::sync::Arc<Document>)>,
}

pub(crate) fn real_page_names<G: QueryGraph>(graph: &G) -> RealPageNames {
    let fallback = match graph.indexed_or_fallback(|| graph.reference_real_page_names()) {
        Ok(indexed) => return indexed,
        Err(fallback) => fallback,
    };
    fallback.with_pages(graph, |pages| {
        let mut real = RealPageNames::new();
        for (entry, _) in pages {
            let key = refs::page_key(&entry.name);
            match real.get_mut(&key) {
                Some((winner_path, winner_name)) if entry.path < *winner_path => {
                    *winner_path = entry.path.clone();
                    *winner_name = entry.name.clone();
                }
                Some(_) => {}
                None => {
                    real.insert(key, (entry.path.clone(), entry.name.clone()));
                }
            }
        }
        real
    })
}

/// Resolve a requested page/alias to its canonical display name, the complete
/// alias-connected component, and the real page to exclude as self. The
/// normalized component is shared by backlinks, unlinked references, and their
/// scoped-invalidation predicates so those paths cannot drift.
pub(crate) fn equivalent_page_names(
    real_pages: &RealPageNames,
    aliases: &[(String, String)],
    target: &str,
) -> (String, Vec<String>, String) {
    let target_norm = refs::page_key(target);
    let mut neighbors = std::collections::HashMap::<String, Vec<String>>::new();
    let mut original_names = vec![(target_norm.clone(), target.to_string())];
    for (alias, owner) in aliases {
        let alias_norm = refs::page_key(alias);
        let owner_norm = refs::page_key(owner);
        neighbors
            .entry(alias_norm.clone())
            .or_default()
            .push(owner_norm.clone());
        neighbors
            .entry(owner_norm.clone())
            .or_default()
            .push(alias_norm.clone());
        original_names.push((alias_norm, alias.clone()));
        original_names.push((owner_norm, owner.clone()));
    }

    let mut component = std::collections::BTreeSet::new();
    let mut pending = vec![target_norm.clone()];
    while let Some(name) = pending.pop() {
        if !component.insert(name.clone()) {
            continue;
        }
        if let Some(adjacent) = neighbors.get(&name) {
            pending.extend(adjacent.iter().cloned());
        }
    }

    let canonical = component
        .iter()
        .filter_map(|name| real_pages.get(name).map(|(_, stored)| stored))
        .min()
        .cloned()
        .or_else(|| {
            original_names
                .iter()
                .filter(|(key, _)| component.contains(key))
                .map(|(_, original)| original)
                .min()
                .cloned()
        })
        .unwrap_or_else(|| target.to_string());
    let self_page = real_pages
        .get(&target_norm)
        .map(|(_, stored)| stored.clone())
        .unwrap_or_else(|| canonical.clone());
    (canonical, component.into_iter().collect(), self_page)
}

fn graph_equivalent_page_names<G: QueryGraph>(
    graph: &G,
    aliases: &[(String, String)],
    target: &str,
) -> (String, Vec<String>, String) {
    let real_pages = real_page_names(graph);
    let mut resolved = equivalent_page_names(&real_pages, aliases, target);
    let config = graph.config();
    let format = JournalFormat::new(
        config.journal_file_name_format.as_deref(),
        config.journal_page_title_format.as_deref(),
    );
    let Some(target_day) = format.parse(target) else {
        return resolved;
    };
    let Some(journal) = graph.list_pages().into_iter().find(|entry| {
        entry.kind == PageKind::Journal && format.parse(&entry.name) == Some(target_day)
    }) else {
        return resolved;
    };

    apply_journal_page_equivalence(&mut resolved, &format, target_day, &journal.name);
    resolved
}

pub(crate) fn apply_journal_page_equivalence(
    resolved: &mut (String, Vec<String>, String),
    format: &JournalFormat,
    target_day: JournalDate,
    journal_name: &str,
) {
    let accepted_spellings = [
        journal_name.to_string(),
        format.title(target_day),
        format.file_stem(target_day),
        target_day.title(),
        target_day.file_stem(),
        format!(
            "{:04}-{:02}-{:02}",
            target_day.year, target_day.month, target_day.day
        ),
    ];
    for spelling in accepted_spellings {
        let key = refs::page_key(&spelling);
        if !resolved.1.contains(&key) {
            resolved.1.push(key);
        }
    }
    resolved.1.sort();
    resolved.0 = journal_name.to_string();
    resolved.2 = journal_name.to_string();
}

fn org_property_line(line: &str) -> bool {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("#+") {
        return rest
            .split_once(':')
            .is_some_and(|(key, _)| !key.trim().is_empty());
    }
    trimmed
        .strip_prefix(':')
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(key, _)| !key.trim().is_empty())
}

/// A page's own properties, read from its preamble with the projection's
/// grammar ([`DocBlock::preamble`]): an Org page's drawer counts, a `key::`
/// line inside a code fence does not. The not-ready autocomplete fallback and
/// the walk oracle read page properties here; the ready projection builds the
/// same `DocBlock::preamble` (`direct_projection::facets`), and
/// `autocomplete_fallback_reads_page_properties_like_the_projection` pins that
/// the two answers agree.
pub(crate) fn page_properties(pre_block: Option<&str>, is_org: bool) -> Vec<(String, String)> {
    pre_block.map_or_else(Vec::new, |raw| DocBlock::preamble(raw, is_org).properties())
}

/// Keep only page-property source lines from a document pre-block. Free-form
/// preamble text is not a Logseq page property and must not become a backlink.
fn page_property_raw(pre: &str, is_org: bool) -> String {
    pre.lines()
        .filter(|line| {
            crate::doc::parse_property_line(line).is_some() || (is_org && org_property_line(line))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn property_projection(raw: &str, is_org: bool) -> DocBlock {
    DocBlock {
        raw: raw.to_string(),
        children: Vec::new(),
        uuid: String::new(),
        is_org,
        proj: std::sync::OnceLock::new(),
    }
}

fn page_property_block(entry: &PageEntry, pre: &str) -> Option<DocBlock> {
    let is_org = Format::from_path(&entry.path) == Format::Org;
    page_property_block_parts(&entry.name, entry.kind, is_org, pre)
}

fn page_property_block_parts(
    name: &str,
    kind: PageKind,
    is_org: bool,
    pre: &str,
) -> Option<DocBlock> {
    let raw = page_property_raw(pre, is_org);
    if raw.is_empty() {
        return None;
    }
    let mut block = property_projection(&raw, is_org);
    block.uuid = format!("page-property:{:?}:{}", kind, refs::page_key(name));
    Some(block)
}

/// Verify a reference occurrence in the authored raw page preamble using the
/// same page-property projection as the eventual reference walk. The SQLite
/// candidate reader uses this before admitting a page row to an interactive
/// window, so title tokens, prose preambles, and explicit-link syntax cannot
/// consume a plain-occurrence slot that the walk would later reject.
pub(crate) fn page_preamble_has_reference(
    raw: &str,
    is_org: bool,
    names_norm: &[String],
    kind: ReferenceKind,
    config: &crate::config::Config,
) -> bool {
    page_property_block_parts("", PageKind::Page, is_org, raw)
        .is_some_and(|block| block_has_reference(&block, names_norm, kind, config))
}

fn block_reference_evidence(
    block: &DocBlock,
    canonical: &str,
    names_norm: &[String],
    kind: ReferenceKind,
    config: &crate::config::Config,
) -> Option<ReferenceBlockEvidence> {
    let result = crate::reference_evidence::occurrences_of_kind_bounded(
        &block.raw,
        &block.projection().reference_source,
        canonical,
        names_norm,
        kind,
        config,
    );
    (!result.occurrences.is_empty()).then(|| ReferenceBlockEvidence {
        block_id: block.uuid.clone(),
        occurrences: result.occurrences,
        total: result.total,
        truncated: result.truncated,
    })
}

fn block_has_reference(
    block: &DocBlock,
    names_norm: &[String],
    kind: ReferenceKind,
    config: &crate::config::Config,
) -> bool {
    crate::reference_evidence::has_occurrence_kind(
        &block.raw,
        &block.projection().reference_source,
        names_norm,
        kind,
        config,
    )
}

fn collect_reference_occurrences<G: QueryGraph>(
    graph: &G,
    canonical: &str,
    self_page: &str,
    names_norm: &[String],
    kind: ReferenceKind,
) -> Vec<RefGroup> {
    collect_reference_occurrences_bounded(
        graph,
        canonical,
        self_page,
        names_norm,
        kind,
        usize::MAX,
        usize::MAX,
    )
    .groups
}

fn collect_reference_occurrences_bounded<G: QueryGraph>(
    graph: &G,
    canonical: &str,
    self_page: &str,
    names_norm: &[String],
    kind: ReferenceKind,
    max_rows: usize,
    max_bytes: usize,
) -> BoundedGroups {
    let candidate_pages = graph.reference_candidate_pages(names_norm, self_page, kind);
    collect_reference_occurrences_in(
        graph,
        canonical,
        self_page,
        names_norm,
        kind,
        &candidate_pages,
        max_rows,
        max_bytes,
    )
}

/// Whether the resolved candidate set still admits this block.
///
/// `None` is the walk's own answer — every block is a candidate — and is what
/// a missing, stale or block-blind index produces. When the set is `Some`, a
/// block whose runtime UUID is absent from it cannot refer to the target, so
/// the caller skips it WITHOUT forcing `DocBlock::projection()`. That is the
/// whole saving: on the anonymized graph the busiest target narrows to 184
/// pages holding 3,434 blocks, of which 412 refer to it.
///
/// A block whose UUID does not parse is admitted rather than skipped. The
/// projection refuses to lower such a block at all (`lower_blocks` errors on
/// it), so it cannot be in the set, and dropping it here would lose a row the
/// walk would have found.
fn candidate_blocks_admit(
    blocks: Option<&std::collections::HashSet<String>>,
    block: &DocBlock,
) -> bool {
    let admitted = match blocks {
        None => true,
        Some(blocks) => blocks.contains(&block.uuid),
    };
    #[cfg(test)]
    if admitted {
        REFERENCE_CLASSIFICATIONS.with(|count| count.set(count.get().saturating_add(1)));
    }
    admitted
}

// How many blocks the reference walk actually classified — i.e. how many had
// their lsdoc projection forced to answer "does this refer to the target".
// The saving block narrowing exists for is the drop in this number, so the
// gate measures it rather than asserting it in prose.
#[cfg(test)]
thread_local! {
    static REFERENCE_CLASSIFICATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reference_classifications() -> usize {
    REFERENCE_CLASSIFICATIONS.with(|count| count.get())
}

#[cfg(test)]
pub(crate) fn reset_reference_classifications() {
    REFERENCE_CLASSIFICATIONS.with(|count| count.set(0));
}

/// The occurrence engine itself. It takes the candidate set already resolved,
/// because WHICH pages to look at is a policy question (index or walk, wait or
/// answer) and finding the occurrences inside them is not. Both reference
/// surfaces and both policies share this one body; there is no second copy that
/// could drift from it.
fn collect_reference_occurrences_in<G: QueryGraph>(
    graph: &G,
    canonical: &str,
    self_page: &str,
    names_norm: &[String],
    kind: ReferenceKind,
    candidate_pages: &crate::vocab::ReferenceCandidatePages,
    max_rows: usize,
    max_bytes: usize,
) -> BoundedGroups {
    let exclude =
        refs::ReferenceSourceExclusions::new(self_page, graph.config().favorites_page.as_deref());
    let mut accumulator = BoundedReferenceGroups::new(max_rows, max_bytes);
    let pages = candidate_pages.pages.as_slice();
    let config = graph.config();
    let mut sources = pages.iter().collect::<Vec<_>>();
    sources.sort_by(|(a, _), (b, _)| a.path.cmp(&b.path));
    for (entry, doc) in sources {
        if exclude.excludes_name(&entry.name) {
            continue;
        }
        let slot = accumulator.page(&entry.rel_path, &entry.name, entry.kind, entry.date_key);
        let page_owner_admitted = candidate_pages
            .page_owners
            .as_ref()
            .is_none_or(|owners| owners.contains(std::path::Path::new(&entry.rel_path)));
        if let Some(mut block) = page_owner_admitted
            .then(|| doc.pre_block.as_deref())
            .flatten()
            .and_then(|pre| page_property_block(entry, pre))
        {
            if accumulator.closed() {
                if block_has_reference(&block, names_norm, kind, &config) {
                    accumulator.deny();
                }
            } else if let Some(hit) =
                block_reference_evidence(&block, canonical, names_norm, kind, &config)
            {
                // The page-property DTO is the estimate's own input here, so it
                // is built before admission on this one row (unchanged).
                let mut dto = block_to_shallow_dto(&block);
                dto.page_property = true;
                let estimated = crate::vocab::block_dto_estimated_bytes(&dto)
                    .saturating_add(reference_evidence_estimated_bytes(&hit));
                accumulator.admit(slot, dto, Some(hit), estimated);
            }
            block.children.clear();
        }
        let mut path = Vec::new();
        let mut found: Vec<()> = Vec::new();
        let construction_closed = std::cell::Cell::new(accumulator.closed());
        collect_reference_matches(
            &doc.roots,
            &mut path,
            &mut |block, _| {
                // The index already named the referring blocks for this target,
                // so a block outside that set cannot match and never needs its
                // lsdoc projection forced. `None` (no index, or an index that
                // cannot name blocks for this kind) classifies every block, as
                // the walk always did.
                if !candidate_blocks_admit(candidate_pages.blocks.as_ref(), block) {
                    return None;
                }
                if construction_closed.get() {
                    block_has_reference(block, names_norm, kind, &config).then_some(None)
                } else {
                    block_reference_evidence(block, canonical, names_norm, kind, &config).map(Some)
                }
            },
            &mut |block, ancestors, hit| {
                let Some(hit) = hit else {
                    accumulator.deny();
                    construction_closed.set(true);
                    return None;
                };
                let estimated = shallow_dto_estimated_bytes(block, ancestors)
                    .saturating_add(reference_evidence_estimated_bytes(&hit));
                let admitted = accumulator.admit_with(slot, estimated, || {
                    let mut dto = result_dto(block);
                    dto.breadcrumb = ancestors
                        .iter()
                        .map(|ancestor| crate::doc::crumb_line(ancestor))
                        .collect();
                    (dto, Some(hit))
                });
                if !admitted {
                    construction_closed.set(true);
                    return None;
                }
                construction_closed.set(accumulator.closed());
                None
            },
            &mut found,
        );
    }
    accumulator.finish()
}

/// Test-only: answer the SAME target twice from the SAME candidate resolution,
/// once with the index's block set and once with it discarded.
///
/// This is the oracle for block narrowing and the only thing that makes it
/// safe. The walk is the authority; the block set is a filter in front of it,
/// and a filter is correct exactly when removing it changes nothing. Returning
/// whether a block set was present at all keeps the gate from passing
/// vacuously on a corpus where the index never named one.
#[cfg(test)]
pub(crate) fn reference_occurrences_narrowed_and_walked<G: QueryGraph>(
    graph: &G,
    target: &str,
    kind: ReferenceKind,
    max_rows: usize,
    max_bytes: usize,
) -> (BoundedGroups, BoundedGroups, NarrowingReceipt) {
    let aliases = graph.page_aliases();
    let (canonical, names_norm, self_page) = graph_equivalent_page_names(graph, &aliases, target);
    let mut candidates = graph.reference_candidate_pages(&names_norm, &self_page, kind);
    reset_reference_classifications();
    let narrowed = collect_reference_occurrences_in(
        graph,
        &canonical,
        &self_page,
        &names_norm,
        kind,
        &candidates,
        max_rows,
        max_bytes,
    );
    let narrowed_classifications = reference_classifications();
    let narrowing_applied = candidates.blocks.take().is_some();
    reset_reference_classifications();
    let walked = collect_reference_occurrences_in(
        graph,
        &canonical,
        &self_page,
        &names_norm,
        kind,
        &candidates,
        max_rows,
        max_bytes,
    );
    let walked_classifications = reference_classifications();
    (
        narrowed,
        walked,
        NarrowingReceipt {
            applied: narrowing_applied,
            narrowed_classifications,
            walked_classifications,
        },
    )
}

/// What block narrowing did on one target: whether the index named blocks at
/// all, and how many blocks each policy had to classify.
#[cfg(test)]
pub(crate) struct NarrowingReceipt {
    pub applied: bool,
    pub narrowed_classifications: usize,
    pub walked_classifications: usize,
}

pub fn backlinks<G: QueryGraph>(graph: &G, target: &str) -> Vec<RefGroup> {
    let aliases = graph.page_aliases();
    let (canonical, names_norm, self_page) = graph_equivalent_page_names(graph, &aliases, target);
    collect_reference_occurrences(
        graph,
        &canonical,
        &self_page,
        &names_norm,
        ReferenceKind::Explicit,
    )
}

pub fn backlinks_bounded<G: QueryGraph>(
    graph: &G,
    target: &str,
    max_rows: usize,
    max_bytes: usize,
) -> BoundedGroups {
    let aliases = graph.page_aliases();
    let (canonical, names_norm, self_page) = graph_equivalent_page_names(graph, &aliases, target);
    collect_reference_occurrences_bounded(
        graph,
        &canonical,
        &self_page,
        &names_norm,
        ReferenceKind::Explicit,
        max_rows,
        max_bytes,
    )
}

/// Linked references for an interactive panel: the same rows as
/// [`backlinks_bounded`], but a projection that is mid-turn is REPORTED rather
/// than answered by parsing every page in the graph. The caller owns the
/// readiness retry, exactly as a query block does.
pub fn backlinks_bounded_indexed<G: QueryGraph>(
    graph: &G,
    target: &str,
    max_rows: usize,
    max_bytes: usize,
) -> Result<BoundedGroups, QueryExecutionError> {
    graph.reference_readiness(target, ReferenceKind::Explicit)?;
    let aliases = graph.page_aliases();
    let (canonical, names_norm, self_page) = graph_equivalent_page_names(graph, &aliases, target);
    let candidate_pages = graph.reference_candidate_pages_indexed(
        &names_norm,
        &self_page,
        ReferenceKind::Explicit,
    )?;
    Ok(collect_reference_occurrences_in(
        graph,
        &canonical,
        &self_page,
        &names_norm,
        ReferenceKind::Explicit,
        &candidate_pages,
        max_rows,
        max_bytes,
    ))
}

pub(crate) const BACKLINK_FILTER_MAX_BYTES: usize = 16 * 1024 * 1024;
const BACKLINK_FILTER_MAX_TEXT_BYTES: usize = 64 * 1024;
const BACKLINK_FILTER_MAX_FACETS: usize = 256;

fn append_bounded_text(out: &mut String, value: &str, max_bytes: usize) -> bool {
    if value.is_empty() || out.len() >= max_bytes {
        return !value.is_empty() && out.len() >= max_bytes;
    }
    if !out.is_empty() {
        if out.len() + 1 > max_bytes {
            return true;
        }
        out.push('\n');
    }
    let remaining = max_bytes.saturating_sub(out.len());
    if value.len() <= remaining {
        out.push_str(value);
        return false;
    }
    let mut end = remaining;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    out.push_str(&value[..end]);
    true
}

pub(crate) fn backlink_filter_entry(
    page: &str,
    kind: PageKind,
    block: &DocBlock,
    excluded_refs: &std::collections::HashSet<String>,
    remaining_bytes: usize,
    matcher: &Matcher,
) -> (BacklinkFilterEntry, usize) {
    let max_text = BACKLINK_FILTER_MAX_TEXT_BYTES.min(remaining_bytes);
    let mut text = String::new();
    let mut facets = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut facets_truncated = false;

    let mut add_facet = |name: &str| {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let key = refs::normalize(name);
        if excluded_refs.contains(&key) || !seen.insert(key) {
            return;
        }
        if facets.len() >= BACKLINK_FILTER_MAX_FACETS {
            facets_truncated = true;
        } else {
            facets.push(name.to_string());
        }
    };

    fn visit(
        block: &DocBlock,
        depth: usize,
        text: &mut String,
        max_text: usize,
        add_facet: &mut impl FnMut(&str),
        truncated: &mut bool,
    ) {
        if depth > crate::vocab::MAX_BLOCK_DEPTH {
            *truncated = true;
            return;
        }
        *truncated |= append_bounded_text(text, block.visible_text(), max_text);
        let projection = block.projection();
        for name in &projection.refs_page {
            add_facet(name);
        }
        if let Some(marker) = projection.marker.as_deref() {
            add_facet(marker);
        }
        // OG treats tags::/alias:: property values as page references too. The
        // property boundary itself is parser-owned; only its comma-separated
        // semantic values are unwrapped here.
        for (key, value) in &projection.properties {
            if !(key.eq_ignore_ascii_case("tags")
                || key.eq_ignore_ascii_case("alias")
                || key.eq_ignore_ascii_case("aliases"))
            {
                continue;
            }
            let quoted = value.trim();
            if quoted.len() >= 2 && quoted.starts_with('"') && quoted.ends_with('"') {
                continue;
            }
            for value in value.split([',', '，']) {
                let name = strip_ref(value.trim());
                add_facet(&name);
            }
        }
        for child in &block.children {
            visit(child, depth + 1, text, max_text, add_facet, truncated);
        }
    }

    let mut text_truncated = false;
    visit(
        block,
        1,
        &mut text,
        max_text,
        &mut add_facet,
        &mut text_truncated,
    );
    let text_matches = match matcher {
        Matcher::Empty | Matcher::InvalidRegex(_) => true,
        Matcher::Regex(_) => matcher.matches("", &text),
        Matcher::Boolean(_) => matcher.matches(&canonical_fold(&text), &text),
    };
    let entry = BacklinkFilterEntry {
        page: page.to_string(),
        kind,
        block_id: block.uuid.clone(),
        facets,
        text_matches,
        truncated: text_truncated || facets_truncated,
    };
    let estimated = text.len()
        + entry.facets.iter().map(String::len).sum::<usize>()
        + entry.page.len()
        + entry.block_id.len()
        + 128;
    (entry, estimated)
}

/// Build search/facet metadata only for the shallow backlink roots already in
/// one rendered panel. This deliberately does not rerun backlink selection and
/// cannot turn into a graph-sized arbitrary export: the request is ID-scoped,
/// de-duplicated, and the response has both per-root and total byte ceilings.
pub fn backlink_filter_context<G: QueryGraph>(
    graph: &G,
    target: &str,
    targets: &[BacklinkFilterTarget],
    search: &str,
) -> Result<BacklinkFilterContext, QueryExecutionError> {
    let matcher = Matcher::parse(search);
    let search_error = match &matcher {
        Matcher::InvalidRegex(error) => Some(error.clone()),
        _ => None,
    };
    let mut requested =
        std::collections::HashMap::<(PageKind, String), std::collections::HashSet<String>>::new();
    for item in targets {
        requested
            .entry((item.kind, refs::normalize(&item.page)))
            .or_default()
            .insert(item.block_id.clone());
    }
    let requested_pages = requested.keys().cloned().collect::<Vec<_>>();
    let scope = graph.backlink_filter_scope(target, &requested_pages)?;
    let excluded_refs = scope
        .names_norm
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let requested_unique = requested
        .values()
        .map(std::collections::HashSet::len)
        .sum::<usize>();

    let mut context = BacklinkFilterContext {
        search_error,
        ..BacklinkFilterContext::default()
    };
    let mut bytes = 0usize;
    for (page, document) in &scope.pages {
        let Some(ids) = requested.get(&(page.kind, refs::normalize(&page.name))) else {
            continue;
        };
        if let Some(pre) = document.pre_block.as_deref() {
            if let Some(block) = page_property_block(page, pre) {
                if ids.contains(&block.uuid) {
                    let (entry, estimated) = backlink_filter_entry(
                        &page.name,
                        page.kind,
                        &block,
                        &excluded_refs,
                        BACKLINK_FILTER_MAX_BYTES.saturating_sub(bytes),
                        &matcher,
                    );
                    if bytes.saturating_add(estimated) > BACKLINK_FILTER_MAX_BYTES {
                        context.truncated = true;
                    } else {
                        bytes += estimated;
                        // Same flag propagation as the ordinary-root loop
                        // below: an entry truncated at its own text/facet
                        // budget must mark the context (DUP-6).
                        context.truncated |= entry.truncated;
                        context.entries.push(entry);
                    }
                }
            }
        }
        fn collect<'a>(
            blocks: &'a [DocBlock],
            ids: &std::collections::HashSet<String>,
            out: &mut Vec<&'a DocBlock>,
        ) {
            for block in blocks {
                if ids.contains(&block.uuid) {
                    out.push(block);
                }
                collect(&block.children, ids, out);
            }
        }
        let mut blocks = Vec::new();
        collect(&document.roots, ids, &mut blocks);
        for block in blocks {
            if bytes >= BACKLINK_FILTER_MAX_BYTES {
                context.truncated = true;
                break;
            }
            let (entry, estimated) = backlink_filter_entry(
                &page.name,
                page.kind,
                block,
                &excluded_refs,
                BACKLINK_FILTER_MAX_BYTES.saturating_sub(bytes),
                &matcher,
            );
            if bytes.saturating_add(estimated) > BACKLINK_FILTER_MAX_BYTES {
                context.truncated = true;
                break;
            }
            bytes += estimated;
            context.truncated |= entry.truncated;
            context.entries.push(entry);
        }
    }
    if context.entries.len() < requested_unique {
        // Missing IDs can be stale results after an external edit. They remain
        // visible in the frontend, which must not turn an incomplete bounded
        // native answer into a false negative.
        context.truncated = true;
    }
    Ok(context)
}

/// Block-level referrers: every block across the graph that references the block
/// with `id:: uuid` (via `((uuid))`, `[..](((uuid)))`, or `{{embed ((uuid))}}`),
/// grouped by source page. Unlike page `backlinks`, this passes `exclude: None`,
/// so a referrer on the *same page* as the target is included — matching OG's
/// `get-block-referenced-blocks` (no self-page exclusion at the block level).
pub fn block_referrers<G: QueryGraph>(graph: &G, uuid: &str) -> Vec<RefGroup> {
    let u = uuid.trim();
    if u.is_empty() {
        return Vec::new();
    }
    collect_bounded_candidates(
        graph,
        graph.indexed_or_fallback(|| {
            graph
                .indexed_derived_pages(DerivedSelection::Referrers(u))
                .or_else(|| graph.direct_projection_block_referrer_candidate_pages(u))
        }),
        |b| b.projection().block_refs.iter().any(|r| r == u),
        |_, _| None,
        None,
        usize::MAX,
        usize::MAX,
    )
    .groups
}

pub fn block_referrers_bounded<G: QueryGraph>(
    graph: &G,
    uuid: &str,
    max_rows: usize,
    max_bytes: usize,
) -> BoundedGroups {
    let u = uuid.trim();
    if u.is_empty() {
        return BoundedGroups {
            matched_total: None,
            statistics: None,
            groups: Vec::new(),
            total: 0,
            exceeded: false,
        };
    }
    collect_bounded_candidates(
        graph,
        graph.indexed_or_fallback(|| {
            graph
                .indexed_derived_pages(DerivedSelection::Referrers(u))
                .or_else(|| graph.direct_projection_block_referrer_candidate_pages(u))
        }),
        |b| b.projection().block_refs.iter().any(|r| r == u),
        |_, _| None,
        None,
        max_rows,
        max_bytes,
    )
}

/// Unlinked references: parser-visible plain occurrences outside explicit
/// reference syntax. A block containing both kinds appears once in each surface,
/// with the corresponding occurrence evidence.
pub fn unlinked_refs<G: QueryGraph>(graph: &G, target: &str) -> Vec<RefGroup> {
    let aliases = graph.page_aliases();
    let (canonical, names_norm, self_page) = graph_equivalent_page_names(graph, &aliases, target);
    collect_reference_occurrences(
        graph,
        &canonical,
        &self_page,
        &names_norm,
        ReferenceKind::Plain,
    )
}

pub fn unlinked_refs_bounded<G: QueryGraph>(
    graph: &G,
    target: &str,
    max_rows: usize,
    max_bytes: usize,
) -> BoundedGroups {
    let aliases = graph.page_aliases();
    let (canonical, names_norm, self_page) = graph_equivalent_page_names(graph, &aliases, target);
    collect_reference_occurrences_bounded(
        graph,
        &canonical,
        &self_page,
        &names_norm,
        ReferenceKind::Plain,
        max_rows,
        max_bytes,
    )
}

/// Unlinked references for an interactive panel. See
/// [`backlinks_bounded_indexed`]; the only difference is the reference kind.
pub fn unlinked_refs_bounded_indexed<G: QueryGraph>(
    graph: &G,
    target: &str,
    max_rows: usize,
    max_bytes: usize,
) -> Result<BoundedGroups, QueryExecutionError> {
    let answer = unlinked_refs_bounded_indexed_with_source(graph, target, max_rows, max_bytes)?;
    Ok(answer.groups)
}

pub(crate) fn unlinked_refs_bounded_indexed_with_source<G: QueryGraph>(
    graph: &G,
    target: &str,
    max_rows: usize,
    max_bytes: usize,
) -> Result<IndexedReferenceGroups, QueryExecutionError> {
    graph.reference_readiness(target, ReferenceKind::Plain)?;
    let aliases = graph.page_aliases();
    let (canonical, names_norm, self_page) = graph_equivalent_page_names(graph, &aliases, target);
    let candidate_pages =
        graph.reference_candidate_pages_indexed(&names_norm, &self_page, ReferenceKind::Plain)?;
    // `indexed` also describes the Exhaustive SQL fallback used after an
    // Interactive read declines or loses a readiness race. Only Interactive
    // plain candidates carry page-owner provenance (including `Some(empty)`),
    // so admission follows that actual source rather than the broader index
    // bit.
    let memo_eligible = candidate_pages.indexed && candidate_pages.page_owners.is_some();
    let groups = collect_reference_occurrences_in(
        graph,
        &canonical,
        &self_page,
        &names_norm,
        ReferenceKind::Plain,
        &candidate_pages,
        max_rows,
        max_bytes,
    );
    Ok(IndexedReferenceGroups {
        groups,
        memo_eligible,
    })
}

/// Target-scoped trace for bug reports. Membership comes from the exact same
/// occurrence engine as the panels; the deliberately uncached parser path makes
/// projection-cache drift visible. No launcher history is read or returned.
pub fn reference_diagnostics<G: QueryGraph>(graph: &G, target: &str) -> ReferenceDiagnostics {
    let aliases = graph.page_aliases();
    let (canonical, names_norm, self_page) = graph_equivalent_page_names(graph, &aliases, target);
    let excluded_page = refs::page_key(&self_page);
    let config = graph.config();
    let mut traces = graph.with_pages(|pages| {
        let mut traces = Vec::new();
        for (entry, document) in pages {
            let self_page = refs::normalize(&entry.name) == excluded_page;
            let mut inspect = |block: &DocBlock| {
                let occurrences = crate::reference_evidence::slow_occurrences(
                    &block.raw,
                    block.is_org,
                    &canonical,
                    &names_norm,
                    &config,
                );
                let raw_lower = block.raw.to_lowercase();
                let textual_candidate = names_norm.iter().any(|name| raw_lower.contains(name));
                if occurrences.is_empty() && !textual_candidate {
                    return;
                }
                let explicit = occurrences
                    .iter()
                    .any(|occurrence| occurrence.kind == ReferenceKind::Explicit);
                let plain = occurrences
                    .iter()
                    .any(|occurrence| occurrence.kind == ReferenceKind::Plain);
                traces.push(ReferenceDiagnosticTrace {
                    page: entry.name.clone(),
                    kind: entry.kind,
                    block_id: block.uuid.clone(),
                    occurrences,
                    included_linked: !self_page && explicit,
                    included_unlinked: !self_page && plain,
                    exclusion_reason: if self_page {
                        Some("self_page_excluded".to_string())
                    } else if !explicit && !plain {
                        Some("parser_excluded_context_or_boundary".to_string())
                    } else {
                        None
                    },
                });
            };
            if let Some(block) = document
                .pre_block
                .as_deref()
                .and_then(|pre| page_property_block(entry, pre))
            {
                inspect(&block);
            }
            walk(&document.roots, &mut inspect);
        }
        traces
    });
    traces.sort_by(|a, b| {
        a.page
            .cmp(&b.page)
            .then_with(|| a.block_id.cmp(&b.block_id))
    });
    ReferenceDiagnostics {
        engine_version: crate::reference_evidence::ENGINE_VERSION.to_string(),
        target: canonical,
        traces,
    }
}

/// Which surface syntax a query's text is written in (SPEC §4).
///
/// The macro name chooses it when the block is saved (Q3): `{{query …}}` is the
/// OG DSL, `{{tine-query …}}` is TQL. Both are the same IR afterwards — the
/// dialect is a property of the TEXT, never of the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueryDialect {
    Og,
    Tql,
}

/// The ONE text → IR entry, for either dialect: the I-22 source limits, then
/// the dialect's parser (§4.1, §4.2). A source that is refused, or is datalog
/// rather than a simple query, comes back as a query carrying its diagnostic —
/// never as a silently empty one.
pub fn parse_query_text(
    query_src: &str,
    dialect: QueryDialect,
    today: JournalDate,
) -> (Query, ViewSettings) {
    parse_query_text_with_registry(query_src, dialect, today, registry::Registry::none())
}

/// [`parse_query_text`] against a registry snapshot. The parse is identical;
/// only an `UnknownIdent` diagnostic differs, gaining the nearest property keys
/// the graph actually has as `prop('…')` suggestions (§4.2.2). The OG dialect
/// has no identifier vocabulary to be wrong about, so it ignores the registry.
pub fn parse_query_text_with_registry(
    query_src: &str,
    dialect: QueryDialect,
    today: JournalDate,
    registry: &registry::Registry,
) -> (Query, ViewSettings) {
    match dialect {
        QueryDialect::Og => parse_query_source(query_src, today),
        QueryDialect::Tql => {
            use ir::{Diagnostic, DiagnosticKind};
            if !query_source_within_limit(query_src) {
                let mut query = Query::new(
                    Anchor::Block,
                    Filter::False,
                    Source::Tql {
                        original: query_src.to_string(),
                        og_options: String::new(),
                    },
                );
                query.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::Size,
                    "the query source is too large",
                ));
                return (query, ViewSettings::default());
            }
            tql::parse_tql(query_src, registry)
        }
    }
}

/// **The macro-input dispatch (§7.1, C3).** Which INPUT a caller has, which is
/// not the same question as which grammar the text is written in.
///
/// `Og`, `Tql` and `Advanced` are explicit FORM inputs: the caller already knows
/// the grammar (the TQL pane, the `#+BEGIN_QUERY` container extractor). The two
/// `Macro*` inputs take the COMPLETE raw macro argument, without the outer
/// `{{`/`}}`, and are the only place a query argument is ever split — after this
/// wave nothing outside `query_parse` splits one (§4.3, Y2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryInput {
    /// An OG DSL form.
    Og,
    /// A TQL form: filter and anchor only, never options (§4.3.1).
    Tql,
    /// A datalog form, including a whole `{:query … :inputs …}` map (§4.4).
    Advanced,
    /// The complete argument of a `{{query …}}` macro: OG or advanced.
    MacroQuery,
    /// The complete argument of a `{{tine-query …}}` macro: TQL.
    MacroTql,
}

/// Whether a `{{query …}}` form is datalog rather than the OG DSL.
///
/// **The ONE discriminator** (§7.1): the existing `Macro.tsx` / `ExportModal.tsx`
/// regexes are deleted in P0-ts and every caller asks this instead, so the two
/// cannot disagree about which source variant a block holds. A `:find` or
/// `:where` token inside an OG string or a page ref is text, not datalog — which
/// is exactly the case the TypeScript regexes got wrong — so the scan protects
/// both. There is **no speculative parse-and-fallback**: the token decides.
fn advanced_form(form: &str) -> bool {
    let bytes = form.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // An OG double-quoted string, backslash-escaped.
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'[' if form[i..].starts_with("[[") => {
                i = match form[i + 2..].find("]]") {
                    Some(offset) => i + 2 + offset + 2,
                    None => form.len(),
                };
            }
            b':' => {
                let rest = &form[i..];
                if rest.starts_with(":find") || rest.starts_with(":where") {
                    return true;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    false
}

/// The ONE text → IR entry for every input shape (§7.1, C3).
///
/// The macro inputs split their argument once, here, with the one
/// [`macro_text::split_trailing_map`]; the source variant records the grammar,
/// `source.original` holds the exact form slice and `source.og_options` the
/// opaque map or the empty string. The §4.1 precedence merge of the host
/// block's `tine.*` properties happens above this, in the command.
pub fn parse_query_input(
    text: &str,
    input: QueryInput,
    today: JournalDate,
    registry: &registry::Registry,
) -> (Query, ViewSettings) {
    match input {
        QueryInput::Og => parse_query_source(text, today),
        QueryInput::Advanced => advanced_source_query(text, String::new()),
        QueryInput::Tql => parse_query_text_with_registry(text, QueryDialect::Tql, today, registry),
        QueryInput::MacroTql => {
            let (form, og_options) =
                macro_text::split_trailing_map(text, macro_text::FormFamily::Tql);
            if !query_source_within_limit(&form) {
                return refuse_tql_source(&form, og_options);
            }
            tql::parse_tql_with_options(&form, og_options, registry)
        }
        QueryInput::MacroQuery => {
            let (form, og_options) =
                macro_text::split_trailing_map(text, macro_text::FormFamily::Edn);
            // A whole advanced map is the FORM, never options: the splitter
            // already refused to split a map with nothing before it (§4.4).
            if advanced_form(&form) {
                return advanced_source_query(&form, og_options);
            }
            let (mut query, view) = parse_query_source(&form, today);
            if let Source::Og {
                og_options: slot, ..
            } = &mut query.source
            {
                *slot = og_options;
            }
            (query, view)
        }
    }
}

/// A datalog form, retained as [`Source::Advanced`] with its complete authored
/// text (§4.4, C2).
///
/// The SIMPLE engine still refuses to run it — that is unchanged, and §4.4's
/// shared `resolve_for_execution` boundary is Wave D. What changes here is that
/// the source survives as the advanced variant, so a title-only edit can print
/// it back through the source-preserving path instead of being told the OG
/// printer cannot express it. `original` is the whole form, `:query`/`:inputs`
/// map and all; only a map that FOLLOWS it is options.
fn advanced_source_query(form: &str, og_options: String) -> (Query, ViewSettings) {
    use ir::{Diagnostic, DiagnosticKind};
    let mut query = Query::new(
        Anchor::Block,
        Filter::False,
        Source::Advanced {
            original: form.to_string(),
            og_options,
        },
    );
    query.diagnostics.push(Diagnostic::new(
        DiagnosticKind::Syntax,
        "this is an advanced (datalog) query, not the simple DSL",
    ));
    (query, ViewSettings::default())
}

fn refuse_tql_source(form: &str, og_options: String) -> (Query, ViewSettings) {
    use ir::{Diagnostic, DiagnosticKind};
    let mut query = Query::new(
        Anchor::Block,
        Filter::False,
        Source::Tql {
            original: form.to_string(),
            og_options,
        },
    );
    query.diagnostics.push(Diagnostic::new(
        DiagnosticKind::Size,
        "the query source is too large",
    ));
    (query, ViewSettings::default())
}

/// The OG `{{query}}` half of [`parse_query_text`].
pub(crate) fn parse_query_source(query_src: &str, today: JournalDate) -> (Query, ViewSettings) {
    use ir::{Diagnostic, DiagnosticKind};
    let refuse = |kind, message: &str| {
        let mut query = Query::new(
            Anchor::Block,
            Filter::False,
            Source::Og {
                original: query_src.to_string(),
                og_options: String::new(),
            },
        );
        query.diagnostics.push(Diagnostic::new(kind, message));
        (query, ViewSettings::default())
    };
    if !query_source_within_limit(query_src) {
        return refuse(DiagnosticKind::Size, "the query source is too large");
    }
    if !query_nesting_within_limit(query_src) {
        return refuse(DiagnosticKind::Depth, "the query nests too deeply");
    }
    if is_advanced(query_src) {
        return refuse(
            DiagnosticKind::Syntax,
            "this is an advanced (datalog) query, not the simple DSL",
        );
    }
    og::parse_og(query_src, today)
    // NOTE: the advanced refusal above is the SIMPLE-query engine's answer and
    // is unchanged. `query_parse`'s advanced inspection (§4.4) is Wave D's
    // `resolve_for_execution` boundary; `advanced_form` above is only the
    // §7.1 discriminator, and this wave routes both to the OG parser exactly as
    // Wave B did, so no behaviour depends on it yet.
}

// ---------------------------------------------------------------------------
// SPEC §4.4 (R5): execution-time binding
// ---------------------------------------------------------------------------

/// The provisional diagnostic `query_parse(advanced)` attaches to an advanced
/// form it has only INSPECTED (§4.4).
///
/// It is not a syntax verdict — the form may be perfectly well formed — it says
/// "the simple engine cannot answer this as it stands". §4.4 calls this a
/// *provisional inspection diagnostic* and requires the bound lowering's own
/// diagnostics to REPLACE it at execution time, which
/// [`resolve_for_execution`] does by matching this exact message. Every other
/// parse diagnostic (an I-22 size or depth refusal) is STATIC and survives.
pub(crate) const ADVANCED_UNRESOLVED_MESSAGE: &str =
    "this is an advanced (datalog) query, not the simple DSL";

/// The message a resolution that could not bind the query reports (§4.4). The
/// strict no-results behaviour is unchanged: a partially recognized tree is
/// never run.
pub(crate) const ADVANCED_UNSUPPORTED_MESSAGE: &str =
    "this advanced query's clauses are not supported, so it returns no results";

/// A query BOUND to one execution (SPEC §4.4, R5).
///
/// **The type is the guarantee.** Every evaluator, every explain-empty
/// decomposition and every result cache below takes a `ResolvedQuery`, and the
/// only way to obtain one is [`resolve_for_execution`], which consumes an
/// unresolved [`Query`]. A resolved query therefore cannot be resolved again —
/// not by convention, but because there is no function that accepts one and
/// returns another.
///
/// It carries its own `today`, the ONE execution-day snapshot: taken once here
/// rather than by each evaluator, so a rollover cannot land between the
/// page-anchored and block-anchored halves of a single answer, nor between a
/// result and the explanation of why it was empty.
#[derive(Debug, Clone)]
pub struct ResolvedQuery {
    query: Query,
    report: ir::QueryReport,
    today: JournalDate,
}

impl ResolvedQuery {
    /// The bound IR — an advanced form's lowered filter, or the OG/TQL IR
    /// unchanged.
    pub fn query(&self) -> &Query {
        &self.query
    }

    /// The support report this binding produced (M5). OG and TQL report an
    /// empty `ignored` and `supported = true`.
    pub fn report(&self) -> &ir::QueryReport {
        &self.report
    }

    /// The one execution-day snapshot every leaf in this execution reads.
    pub fn today(&self) -> JournalDate {
        self.today
    }

    /// Whether this binding produced executable IR at all. A refused advanced
    /// resolution is `false`: it has diagnostics and a report, and no counts.
    pub fn is_executable(&self) -> bool {
        self.report.supported && !self.query.is_invalid()
    }
}

/// **The ONE execution-time binding boundary** (SPEC §4.4, R5).
///
/// It runs BEFORE the invalidity check, before normalization and cache lookup,
/// before SQL/walk dispatch, and before explain-empty decomposition — so that
/// every one of those sees the same bound tree, and none of them can be handed
/// an advanced placeholder to interpret on its own.
///
/// For [`Source::Advanced`] it calls the ONE existing lowerer,
/// [`advanced_pred`], with the AUTHORED source (`Source::Advanced.original`,
/// `:query`/`:inputs` and all), the caller's current page, and this execution's
/// day. The lowering's `ran`/`ignored`/`supported` report is carried through
/// verbatim, its diagnostics replace the provisional inspection one, and static
/// (size/depth) diagnostics survive. Missing required inputs or unsupported
/// clauses keep today's strict no-results behaviour: the filter is
/// [`Filter::False`] and nothing partial runs.
///
/// For every other source the IR is already the query; only the execution-day
/// snapshot is added, which is what makes an OG `(between -7d today)` and a TQL
/// `day > -7d` read the same clock as an advanced `?today`.
pub fn resolve_for_execution(
    query: &Query,
    context: &ir::ExecutionContext,
    today: JournalDate,
) -> ResolvedQuery {
    use ir::{Diagnostic, DiagnosticKind};

    let Source::Advanced { original, .. } = &query.source else {
        return ResolvedQuery {
            query: query.clone(),
            report: ir::QueryReport {
                ran: Vec::new(),
                ignored: Vec::new(),
                supported: true,
            },
            today,
        };
    };

    // Static diagnostics survive the binding; the provisional inspection one
    // does not (§4.4).
    let static_diagnostics: Vec<Diagnostic> = query
        .diagnostics
        .iter()
        .filter(|diagnostic| {
            !(diagnostic.kind == DiagnosticKind::Syntax
                && diagnostic.message == ADVANCED_UNRESOLVED_MESSAGE)
        })
        .cloned()
        .collect();

    let (lowered, ran, ignored) = advanced_pred(original, context.current_page.as_deref(), today);
    let mut bound = Query {
        anchor: query.anchor,
        filter: lowered
            .as_ref()
            .map_or(Filter::False, |query| query.filter.clone()),
        diagnostics: static_diagnostics,
        // The immutable source stays available for printing (§4.4). It is never
        // re-read as a filter after this point.
        source: query.source.clone(),
    };
    let supported = lowered.is_some();
    if !supported {
        bound.diagnostics.push(Diagnostic::new(
            DiagnosticKind::Syntax,
            ADVANCED_UNSUPPORTED_MESSAGE,
        ));
    }
    ResolvedQuery {
        query: bound,
        report: ir::QueryReport {
            ran,
            ignored,
            supported,
        },
        today,
    }
}

/// What the CONSTRUCTION of a simple query produced, before any view directive
/// has looked at it (SPEC §5.9's "pre-view result").
///
/// This is the value §5.9's cache stores, and the reason the cache key names the
/// IR and not the query source: `(sort-by …)` and `(sample N)` are decisions
/// about already-constructed rows, so two queries that differ only in them
/// construct the same thing. The two construction inputs that are NOT view
/// decisions — the sample ADMISSION cap (an unsorted `(sample N)` stops
/// constructing at N, which is what makes its `total` the truncated count) and
/// whether the recency axis was measured — travel in the cache key beside the
/// result bounds, because they change what is built rather than how it is
/// displayed.
#[derive(Clone, Debug, Default)]
pub(crate) struct PreViewGroups {
    pub(crate) ordered: bool,
    pub(crate) statistics: Option<ir::QueryStatistics>,
    pub(crate) matched_total: Option<usize>,
    pub(crate) groups: Vec<RefGroup>,
    pub(crate) recency_by_page: std::collections::HashMap<String, i64>,
    pub(crate) total: usize,
    pub(crate) exceeded: bool,
}

/// The two construction inputs a view implies (see [`PreViewGroups`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct ConstructionProfile {
    /// An unsorted `(sample N)` semantically needs only the first N matches in
    /// deterministic traversal order, and stops counting there.
    pub(crate) sample_admission_cap: Option<usize>,
    /// A `(sort-by modified …)` needs each result page's position on the recency
    /// axis, which costs a `stat` per result page and is skipped otherwise.
    pub(crate) want_recency: bool,
}

impl ConstructionProfile {
    pub(crate) fn from_view(view: &ViewSettings) -> ConstructionProfile {
        let opts = QueryOpts::from_view(view);
        ConstructionProfile {
            sample_admission_cap: opts.sample.filter(|_| opts.sort.is_empty()),
            want_recency: opts.uses_recency(),
        }
    }
}

/// §5.9: the view applied to an already-constructed pre-view
/// result. Base order, then `sort-by`, then `sample` — `finish_query_groups`
/// unchanged, reached from both the walk and the dispatched statement.
pub(crate) fn apply_view(pre: PreViewGroups, view: &ViewSettings) -> BoundedGroups {
    if pre.ordered {
        return BoundedGroups {
            groups: pre.groups,
            total: pre.total,
            exceeded: pre.exceeded,
            statistics: pre.statistics,
            matched_total: pre.matched_total,
        };
    }
    let opts = QueryOpts::from_view(view);
    let mut budget = ConstructionBudget::new(usize::MAX, usize::MAX);
    budget.total = pre.total;
    budget.exceeded = pre.exceeded;
    finish_query_groups(pre.groups, pre.recency_by_page, &opts, budget)
}

/// The BLOCK-anchored evaluable filter of a `{{query …}}`/advanced source.
///
/// The legacy block-group adapter evaluates a `@page`-anchored filter
/// BLOCK-anchored — page attributes and relations read through `block.page` —
/// because that is today's semantics verbatim (`(page-property …)`,
/// `(page-tags …)` and `(namespace …)` have always returned blocks). The
/// page-anchored result rows live behind [`run_query_result_over`].
///
/// ONE producer, because §5.9's dispatch has to lower exactly the filter the
/// walk evaluates: a second rebase here is a walk/SQL fork by construction
/// (I-12).
pub(crate) fn block_anchored_filter(query: &Query) -> Filter {
    match query.anchor {
        Anchor::Block => query.evaluable_filter(),
        Anchor::Page => og::rebase_to_block(&query.evaluable_filter()),
    }
}

/// [`block_anchored_filter`] as a `Query` the lowering can consume — the same
/// tree, already `Off`-removed, presented at the anchor the walk evaluates it
/// at. Diagnostics travel so an invalid query still lowers to "no rows" (§3.5).
pub(crate) fn block_anchored_query(query: &Query) -> Query {
    Query {
        anchor: Anchor::Block,
        filter: block_anchored_filter(query),
        diagnostics: query.diagnostics.clone(),
        source: ir::Source::Builder,
    }
}

fn finish_query_groups(
    groups: Vec<RefGroup>,
    recency_by_page: std::collections::HashMap<String, i64>,
    opts: &QueryOpts,
    budget: ConstructionBudget,
) -> BoundedGroups {
    BoundedGroups {
        matched_total: None,
        statistics: None,
        groups: finish_result_view_groups(
            groups.into_iter().map(ResultViewGroup::from).collect(),
            &recency_by_page,
            opts,
        )
        .into_iter()
        .map(RefGroup::from)
        .collect(),
        total: budget.total,
        exceeded: budget.exceeded,
    }
}

/// **Base order, then the view directives — the ONE finisher**, for ordinary
/// result entries and for RET3's physically located ones alike.
///
/// `base_order_result_view_groups` and `apply_result_view_directives` are the
/// same two owners either path already used; naming their composition once is
/// what stops a located caller from growing its own "sort then coalesce"
/// spelling that drifts from the ordinary one.
pub(crate) fn finish_result_view_groups<B: ResultViewBlock>(
    groups: Vec<ResultViewGroup<B>>,
    recency_by_page: &std::collections::HashMap<String, i64>,
    opts: &QueryOpts,
) -> Vec<ResultViewGroup<B>> {
    let mut groups = groups;
    base_order_result_view_groups(&mut groups);
    apply_result_view_directives(groups, recency_by_page, opts)
}

/// SPEC §3.5's BASE ORDER (M13): page display name, then kind rank (journal 0,
/// page 1). Within a page the blocks keep the document order the construction
/// emitted them in.
///
/// The source traversal is path-stable. Make the displayed base order stable
/// before sampling and
/// before it becomes the tie-breaker for an explicit sort.
///
/// This is base order, not a view directive, so §5.9's cache stores rows that
/// have ALREADY been through it — which is what lets a query with no `sort-by`
/// and no `sample` return the cached `Arc` itself rather than a copy of it.
pub(crate) fn base_order_groups(groups: &mut [RefGroup]) {
    groups.sort_by(|a, b| compare_result_pages(&a.page, a.kind, &b.page, b.kind));
}

fn compare_result_pages(
    a: &str,
    a_kind: PageKind,
    b: &str,
    b_kind: PageKind,
) -> std::cmp::Ordering {
    let rank = |kind| match kind {
        PageKind::Journal => 0,
        PageKind::Page => 1,
    };
    a.cmp(b).then_with(|| rank(a_kind).cmp(&rank(b_kind)))
}

/// An internal result entry may retain snapshot-owned physical identity. Views
/// inspect its DTO, but move the complete entry without reconstructing identity.
pub(crate) trait ResultViewBlock {
    fn dto(&self) -> &BlockDto;
}

impl ResultViewBlock for BlockDto {
    fn dto(&self) -> &BlockDto {
        self
    }
}

impl<T> ResultViewBlock for (BlockDto, T) {
    fn dto(&self) -> &BlockDto {
        &self.0
    }
}

pub(crate) struct ResultViewGroup<B> {
    pub(crate) page: String,
    pub(crate) kind: PageKind,
    pub(crate) blocks: Vec<B>,
    pub(crate) evidence: Vec<crate::vocab::ReferenceBlockEvidence>,
}

impl From<RefGroup> for ResultViewGroup<BlockDto> {
    fn from(group: RefGroup) -> Self {
        Self {
            page: group.page,
            kind: group.kind,
            blocks: group.blocks,
            evidence: group.evidence,
        }
    }
}

impl From<ResultViewGroup<BlockDto>> for RefGroup {
    fn from(group: ResultViewGroup<BlockDto>) -> Self {
        Self {
            page: group.page,
            kind: group.kind,
            blocks: group.blocks,
            evidence: group.evidence,
        }
    }
}

pub(crate) fn base_order_result_view_groups<B>(groups: &mut [ResultViewGroup<B>]) {
    groups.sort_by(|a, b| compare_result_pages(&a.page, a.kind, &b.page, b.kind));
}

/// The sole view implementation for ordinary and physically located entries.
pub(crate) fn apply_result_view_directives<B: ResultViewBlock>(
    groups: Vec<ResultViewGroup<B>>,
    recency_by_page: &std::collections::HashMap<String, i64>,
    opts: &QueryOpts,
) -> Vec<ResultViewGroup<B>> {
    let mut groups = groups;
    // sort-by is GLOBAL (like Logseq): order every matched block across all pages on
    // one axis, so e.g. priority-A tasks float to the very top regardless of which
    // page they live on. We flatten to one block per group, sort, then RE-COALESCE
    // runs of adjacent same-page blocks back under a single page heading — N
    // consecutive results from one page show ONCE, not N times (a page whose blocks
    // land at different sort positions, e.g. an A and a C task under a priority sort,
    // still appears at each of those positions). Non-sorted queries keep their
    // natural page grouping untouched.
    if !opts.sort.is_empty() {
        // Compute each requested key once per block, outside the comparator.
        // A missing property can require a bounded visible-text parse per key.
        // Keep the original index as the
        // stable tiebreaker so equal-key blocks keep DOCUMENT order in both
        // directions: a plain `reverse()` for `desc` would flip a page's blocks
        // upside-down under its heading.
        let mut flat: Vec<(Vec<SortDecor>, usize, ResultViewGroup<B>)> = Vec::new();
        for g in groups {
            let ResultViewGroup {
                page,
                kind,
                blocks,
                evidence: _,
            } = g;
            for b in blocks {
                let keys = opts
                    .sort
                    .iter()
                    .map(|(field, _)| {
                        if is_recency_field(field) {
                            // Recency is numeric (Unix seconds on one axis): journal pages by
                            // the day they represent, others by file mtime.
                            SortDecor::Num(recency_by_page.get(&page).copied().unwrap_or(i64::MIN))
                        } else {
                            SortDecor::Text(sort_key(b.dto(), &page, field))
                        }
                    })
                    .collect();
                let idx = flat.len();
                flat.push((
                    keys,
                    idx,
                    ResultViewGroup {
                        page: page.clone(),
                        kind,
                        blocks: vec![b],
                        evidence: Vec::new(),
                    },
                ));
            }
        }
        // Directions are view-owned, not part of a row's decoration. Materialize
        // them once outside the comparator: no result text is parsed and no key
        // storage is allocated during O(R log R) comparisons.
        let ascending: Vec<bool> = opts.sort.iter().map(|(_, asc)| *asc).collect();
        flat.sort_by(|a, b| {
            compare_sort_decorations(&a.0, &b.0, &ascending)
                // Equal keys retain the original base-order position. Direction
                // never reverses this tie, which is the existing stable contract.
                .then_with(|| a.1.cmp(&b.1))
        });
        // Merge adjacent one-block groups that share a page (and kind) into a single
        // group, so consecutive same-page results render under one heading.
        let mut merged: Vec<ResultViewGroup<B>> = Vec::with_capacity(flat.len());
        for (_, _, g) in flat {
            match merged.last_mut() {
                Some(last) if last.page == g.page && last.kind == g.kind => {
                    last.blocks.extend(g.blocks)
                }
                _ => merged.push(g),
            }
        }
        groups = merged;
    }

    // sample N: cap total results (deterministic: first N across pages).
    if let Some(n) = opts.sample {
        let mut remaining = n;
        groups.retain_mut(|g| {
            if remaining == 0 {
                return false;
            }
            if g.blocks.len() > remaining {
                g.blocks.truncate(remaining);
            }
            remaining -= g.blocks.len();
            true
        });
    }
    groups
}

/// `query_run` over a Direct Files graph when the IR is already parsed (the
/// §7.1 command hands the IR, not text).
///
/// §4.4: the IR arriving already parsed is exactly why this resolves. A parse
/// is context-free, so the `{query, view}` a caller holds may have been parsed
/// on another page, on another day, or by another window; the binding happens
/// here, per execution.
pub fn run_query_result_ir<G: QueryGraph>(
    graph: &G,
    query: &Query,
    view: &ViewSettings,
    bounds: ir::Bounds,
    context: &ir::ExecutionContext,
) -> Result<ir::QueryResult, QueryExecutionError> {
    // §4.4: resolve ONCE — the current page, the execution day and the support
    // report — before anything is keyed, lowered or cached.
    let resolved = resolve_for_execution(query, context, JournalDate::today());
    let mut result = graph.direct_ir_query_result(&resolved, view, bounds)?;
    // The report is attached HERE, after the execution and after any cache
    // retrieval, because it is a property of how this source was BOUND and not
    // of the rows: the rows may be shared, the report may not.
    result.report = resolved.report().clone();
    Ok(result)
}

/// Explain one IR query over Direct Files through the captured database read.
pub fn explain_empty_query<G: QueryGraph>(
    graph: &G,
    query: &Query,
    view: &ViewSettings,
    bounds: ir::Bounds,
    context: &ir::ExecutionContext,
) -> Result<ir::ExplainEmptyResult, QueryExecutionError> {
    let resolved = resolve_for_execution(query, context, JournalDate::today());
    graph.direct_ir_explain_empty(&resolved, view, bounds)
}

// --- Scoped-invalidation support (#52) --------------------------------------
// "Could an edit to page (entry, doc) change this derived result?" Each reuses
// the SAME parse + EvalCtx + eval (or alias resolution) as the real matcher, so
// the keep/evict decision can never drift from what a full recompute would give.

/// Whether a query source carries a `props` leaf, and is therefore sensitive to
/// the registry's effective types (C6): its cached result must be evicted when
/// the registry generation advances, because per-page retention evaluates the
/// query against ONE saved page and cannot see a graph-wide type change.
pub fn query_source_has_props_leaf(src: &str) -> bool {
    let (query, _view) = parse_query_source(src, JournalDate::today());
    query.filter.has_props_leaf()
}

/// Whether page `doc` references `target` or any of its aliases — i.e. could be
/// in `backlinks(target)`. Mirrors `backlinks`'s alias resolution; takes the
/// resolved alias map so the caller needn't hold the graph lock.
pub(crate) fn page_affects_backlinks(
    real_pages: &RealPageNames,
    aliases: &[(String, String)],
    target: &str,
    entry: &PageEntry,
    doc: &Document,
) -> bool {
    let (canonical, names_norm, _) = equivalent_page_names(real_pages, aliases, target);
    // Scoped invalidation has no Graph/config parameter. Default-enabled matching
    // is conservative for disabled/excluded property pages (it may evict an
    // unaffected cache entry, but cannot retain a stale one).
    let config = crate::config::Config::default();
    if doc.pre_block.as_deref().is_some_and(|pre| {
        page_property_block(entry, pre).is_some_and(|block| {
            block_reference_evidence(
                &block,
                &canonical,
                &names_norm,
                ReferenceKind::Explicit,
                &config,
            )
            .is_some()
        })
    }) {
        return true;
    }
    let mut hit = false;
    walk(&doc.roots, &mut |b| {
        if !hit
            && block_reference_evidence(
                b,
                &canonical,
                &names_norm,
                ReferenceKind::Explicit,
                &config,
            )
            .is_some()
        {
            hit = true;
        }
    });
    hit
}

/// Whether page `doc` plain-text-mentions `target` unlinked — i.e. could be in
/// `unlinked_refs(target)`. Mirrors `unlinked_refs`'s matcher.
pub(crate) fn page_affects_unlinked(
    real_pages: &RealPageNames,
    aliases: &[(String, String)],
    target: &str,
    entry: &PageEntry,
    doc: &Document,
) -> bool {
    let (canonical, names_norm, _) = equivalent_page_names(real_pages, aliases, target);
    let config = crate::config::Config::default();
    if doc.pre_block.as_deref().is_some_and(|pre| {
        page_property_block(entry, pre).is_some_and(|block| {
            block_reference_evidence(
                &block,
                &canonical,
                &names_norm,
                ReferenceKind::Plain,
                &config,
            )
            .is_some()
        })
    }) {
        return true;
    }
    let mut hit = false;
    walk(&doc.roots, &mut |b| {
        if !hit
            && block_reference_evidence(b, &canonical, &names_norm, ReferenceKind::Plain, &config)
                .is_some()
        {
            hit = true;
        }
    });
    hit
}

/// Whether this page contains a referrer to one block UUID. This is the exact
/// predicate used by `block_referrers_bounded`, without DTO construction.
pub(crate) fn page_affects_block_referrers(uuid: &str, doc: &Document) -> bool {
    let uuid = uuid.trim();
    if uuid.is_empty() {
        return false;
    }
    let mut hit = false;
    walk(&doc.roots, &mut |block| {
        if !hit
            && block
                .projection()
                .block_refs
                .iter()
                .any(|reference| reference == uuid)
        {
            hit = true;
        }
    });
    hit
}

/// Result of an advanced (datalog) query: matched groups + which clause heads
/// ran vs were ignored, so the UI shows "ran X; ignored Y" rather than a blunt
/// "unsupported". `supported` is false only when nothing in the subset matched.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AdvancedResult {
    pub groups: Vec<RefGroup>,
    pub ran: Vec<String>,
    pub ignored: Vec<String>,
    pub supported: bool,
}

pub(crate) fn rejected_advanced_query(reason: &str) -> AdvancedResult {
    AdvancedResult {
        groups: Vec::new(),
        ran: Vec::new(),
        ignored: vec![reason.to_string()],
        supported: false,
    }
}

/// What one advanced (datalog) SOURCE resolved to: the executable IR, or the
/// refusal to show instead, plus the clause report either way.
///
/// The report is a fact about the SOURCE, not about the answer — two datalog
/// spellings can lower to one filter and still list different `ignored` clauses
/// — which is why §5.9's cache stores the rows and this report travels beside
/// them rather than inside the cached value.
pub(crate) enum ResolvedAdvanced {
    /// The IR to evaluate, and the clause report to show with its rows.
    Executable {
        query: Query,
        today: JournalDate,
        ran: Vec<String>,
        ignored: Vec<String>,
    },
    /// A size/depth refusal or a wholly unsupported clause set. Nothing runs and
    /// nothing is cached; today's strict no-results behaviour is unchanged.
    Refused(AdvancedResult),
}

/// **The ONE advanced-query resolve** (§4.4): source limits, then the SAME
/// `resolve_for_execution` boundary the §7.1 commands use.
///
/// Every caller that needs the executable IR of a datalog source goes through
/// here — the evaluator below, and §5.9's dispatch and cache key. A second
/// resolve is how one lowerer ends up with two callers that disagree about what
/// a missing input means (I-12).
pub(crate) fn resolve_advanced_source(
    query_src: &str,
    current_page: Option<&str>,
) -> ResolvedAdvanced {
    if !query_source_within_limit(query_src) {
        return ResolvedAdvanced::Refused(rejected_advanced_query("query-too-large"));
    }
    if !query_nesting_within_limit(query_src) {
        return ResolvedAdvanced::Refused(rejected_advanced_query("query-nesting-too-deep"));
    }
    let today = JournalDate::today();
    let (parsed, _) = advanced_source_query(query_src, String::new());
    let resolved = resolve_for_execution(
        &parsed,
        &ir::ExecutionContext {
            current_page: current_page.map(str::to_string),
        },
        today,
    );
    let ran = resolved.report().ran.clone();
    let ignored = resolved.report().ignored.clone();
    if !resolved.report().supported {
        return ResolvedAdvanced::Refused(AdvancedResult {
            groups: Vec::new(),
            ran,
            ignored,
            supported: false,
        });
    }
    ResolvedAdvanced::Executable {
        query: resolved.query().clone(),
        today: resolved.today(),
        ran,
        ignored,
    }
}

fn advanced_pred(
    query_src: &str,
    current_page: Option<&str>,
    today: JournalDate,
) -> (Option<Query>, Vec<String>, Vec<String>) {
    // Both limits live here, not only at the two `run_advanced_*` entry points,
    // because `page_affects_advanced_query` reaches this function directly. It
    // used to skip the byte ceiling entirely, which made scoped invalidation the
    // one caller that could hand an unbounded graph-authored string to the
    // parser — the exact thing QUERY_SOURCE_MAX_BYTES exists to prevent.
    if !query_source_within_limit(query_src) {
        return (None, Vec::new(), vec!["query-too-large".to_string()]);
    }
    if !query_nesting_within_limit(query_src) {
        return (None, Vec::new(), vec!["query-nesting-too-deep".to_string()]);
    }
    let inputs = resolve_inputs(query_src, current_page, today);
    let mut ran = Vec::new();
    let mut ignored = Vec::new();
    let groups = advanced_patterns::flatten_single_branch_groups(where_groups(query_src));
    let (lowered_page_properties, consumed_patterns) = lower_page_property_patterns(&groups);
    let (lowered_current_pages, current_page_patterns) =
        lower_current_page_patterns(&groups, &inputs);
    let consumed_patterns = consumed_patterns
        .into_iter()
        .chain(current_page_patterns)
        .collect::<std::collections::HashSet<_>>();
    let taken = consumed_patterns
        .iter()
        .copied()
        .chain(lowered_current_pages.keys().copied())
        .chain(lowered_page_properties.keys().copied())
        .collect::<std::collections::HashSet<_>>();
    let (lowered_attributes, attribute_patterns) = advanced_patterns::lower_attribute_patterns(
        &groups,
        &taken,
        advanced_patterns::advanced_find_var(query_src).as_deref(),
        &inputs,
    );
    let preds: Vec<Filter> = groups
        .iter()
        .enumerate()
        .filter_map(|(index, group)| {
            if let Some((pred, label)) = lowered_current_pages.get(&index) {
                ran.push((*label).into());
                return Some(pred.clone());
            }
            if let Some(pred) = lowered_page_properties.get(&index) {
                ran.push("page-property".into());
                return Some(pred.clone());
            }
            if let Some((pred, label)) = lowered_attributes.get(&index) {
                ran.push((*label).into());
                return Some(pred.clone());
            }
            if consumed_patterns.contains(&index) || attribute_patterns.contains(&index) {
                return None;
            }
            parse_adv_group(group, &inputs, today, &mut ran, &mut ignored, 0)
        })
        .collect();
    // GH #542: a `:result-transform` is a Clojure function (ADR 0042 keeps
    // scripting out). It reorders or reshapes the answer, so say it did not run.
    if query_src.contains(":result-transform") {
        ignored.push("result-transform".into());
    }
    if ignored.iter().any(|item| item == "query-nesting-too-deep") {
        return (None, Vec::new(), ignored);
    }
    if preds.is_empty() {
        return (None, ran, ignored);
    }
    let filter = if preds.len() == 1 {
        preds.into_iter().next().expect("one")
    } else {
        Filter::and(preds)
    };
    // The advanced context and report survive verbatim (M5): `current_page` is
    // already folded into the lowered clauses above, and the caller keeps
    // `ran`/`ignored`/`supported`.
    let query = Query::new(
        Anchor::Block,
        filter,
        Source::Advanced {
            original: query_src.to_string(),
            og_options: String::new(),
        },
    );
    (Some(query), ran, ignored)
}

/// Lower the exact DataScript relationship Logseq uses to connect the typed
/// `:current-page` input to blocks. This is deliberately not a general join
/// engine: one page-name identity pattern must feed one `:block/refs` or
/// `:block/page` pattern, and every other shape remains visibly unsupported.
fn lower_current_page_patterns(
    groups: &[String],
    inputs: &std::collections::HashMap<String, AdvancedInput>,
) -> (
    std::collections::HashMap<usize, (Filter, &'static str)>,
    std::collections::HashSet<usize>,
) {
    let triples = groups
        .iter()
        .enumerate()
        .filter_map(|(index, group)| {
            let inner = group.trim().strip_prefix('[')?.strip_suffix(']')?.trim();
            let tokens = inner.split_whitespace().collect::<Vec<_>>();
            (tokens.len() == 3).then_some((index, tokens))
        })
        .collect::<Vec<_>>();

    let mut candidates = Vec::new();
    for (identity_index, identity) in &triples {
        if identity[1] != ":block/name" || !identity[0].starts_with('?') {
            continue;
        }
        let Some(AdvancedInput::Page(page)) = inputs.get(identity[2]) else {
            continue;
        };
        for (relation_index, relation) in &triples {
            if relation[0] == identity[0]
                || !relation[0].starts_with('?')
                || relation[2] != identity[0]
            {
                continue;
            }
            let lowered = match relation[1] {
                ":block/refs" => Some((Filter::page_ref(page.clone()), "current-page-ref")),
                ":block/page" => Some((
                    Filter::rel(
                        Rel::Page,
                        Quant::Any,
                        Filter::attr(Attr::Name, CmpOp::Eq, Value::text(page.clone())),
                    ),
                    "current-page",
                )),
                _ => None,
            };
            if let Some(lowered) = lowered {
                candidates.push((*identity_index, *relation_index, lowered));
            }
        }
    }
    if candidates.len() != 1 {
        return Default::default();
    }
    let (identity_index, relation_index, lowered) = candidates.pop().unwrap();
    (
        std::collections::HashMap::from([(relation_index, lowered)]),
        std::collections::HashSet::from([identity_index]),
    )
}

/// Conservatively lower only the exact DataScript relationship used by the
/// released BEGIN_QUERY page-property form. The entity/property-map pattern and
/// `(get ...)` predicate must share the literal `?props` binding; every other
/// bracket form remains visible as an unsupported `pattern` in `parse_adv_group`.
fn lower_page_property_patterns(
    groups: &[String],
) -> (
    std::collections::HashMap<usize, Filter>,
    std::collections::HashSet<usize>,
) {
    let relations = groups
        .iter()
        .enumerate()
        .filter_map(|(index, group)| {
            let inner = group.trim().strip_prefix('[')?.strip_suffix(']')?.trim();
            (inner.split_whitespace().collect::<Vec<_>>() == ["?p", ":block/properties", "?props"])
                .then_some(index)
        })
        .collect::<Vec<_>>();
    if relations.len() != 1 {
        return Default::default();
    }

    let mut lowered = std::collections::HashMap::new();
    let mut consumed = std::collections::HashSet::new();
    for (index, group) in groups.iter().enumerate() {
        let Some(inner) = group
            .trim()
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
        else {
            continue;
        };
        let Some(call) = inner
            .trim()
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
        else {
            continue;
        };
        let tokens = call.split_whitespace().collect::<Vec<_>>();
        if tokens.len() != 3 || tokens[0] != "get" || tokens[1] != "?props" {
            continue;
        }
        let Some(key) = tokens[2].strip_prefix(':').filter(|key| !key.is_empty()) else {
            continue;
        };
        if key
            .chars()
            .any(|c| c.is_whitespace() || "()[]{}".contains(c))
        {
            continue;
        }
        lowered.insert(
            index,
            Filter::rel(
                Rel::Page,
                Quant::Any,
                og::property_leaf(og::normalize_prop_key(key), None),
            ),
        );
        consumed.insert(relations[0]);
    }
    (lowered, consumed)
}

/// Map one `:where` group to a `Pred` (or None → ignored). Recurses for and/or/not.
fn parse_adv_group(
    group: &str,
    inputs: &std::collections::HashMap<String, AdvancedInput>,
    today: JournalDate,
    ran: &mut Vec<String>,
    ignored: &mut Vec<String>,
    depth: usize,
) -> Option<Filter> {
    if depth > QUERY_NESTING_MAX {
        ignored.push("query-nesting-too-deep".into());
        return None;
    }
    let c = group.trim();
    if !c.starts_with('(') {
        ignored.push("pattern".into()); // `[?e :a ?v]` joins, etc. — not in the subset
        return None;
    }
    let inner = &c[1..c.len().saturating_sub(1)];
    let head = inner
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match head.as_str() {
        "and" | "or" | "not" => {
            let ignored_before = ignored.len();
            let kids: Vec<Filter> = scan_groups(inner)
                .iter()
                .filter_map(|g| parse_adv_group(g, inputs, today, ran, ignored, depth + 1))
                .collect();
            // GH #542: dropping a clause Tine does not understand only ever
            // WIDENS a conjunction. Under `not` it narrows the answer, and an
            // `or` missing a branch drops blocks the query returns; neither is
            // a superset with a notice. Such a group is dropped whole instead,
            // which widens the enclosing conjunction like any other ignored
            // clause.
            if head != "and" && ignored.len() > ignored_before {
                ignored.push(head.clone());
                return None;
            }
            if kids.is_empty() {
                None
            } else if head == "not" {
                // `(not A B)` excludes blocks matching A AND B.
                Some(Filter::not(if kids.len() == 1 {
                    kids.into_iter().next().expect("one")
                } else {
                    Filter::and(kids)
                }))
            } else if head == "or" {
                Some(Filter::or(kids))
            } else {
                Some(Filter::and(kids))
            }
        }
        "task" | "todo" => {
            ran.push("task".into());
            Some(Filter::attr(
                Attr::Task,
                CmpOp::In,
                adv_text_list(adv_strings(inner)),
            ))
        }
        "priority" => {
            ran.push("priority".into());
            Some(Filter::attr(
                Attr::Priority,
                CmpOp::In,
                adv_text_list(adv_strings(inner)),
            ))
        }
        "page-ref" => adv_strings(inner).into_iter().next().map(|n| {
            ran.push("page-ref".into());
            Filter::page_ref(n)
        }),
        "property" | "page-property" => inner
            .split_whitespace()
            .skip(1)
            .find(|t| t.starts_with(':'))
            .map(|t| t.trim_start_matches(':').to_string())
            .map(|k| {
                let val = adv_strings(inner).into_iter().next();
                ran.push(head.clone());
                let leaf = og::property_leaf(og::normalize_prop_key(&k), val);
                if head == "property" {
                    leaf
                } else {
                    Filter::rel(Rel::Page, Quant::Any, leaf)
                }
            }),
        "page" => adv_strings(inner).into_iter().next().map(|n| {
            ran.push("page".into());
            Filter::rel(
                Rel::Page,
                Quant::Any,
                Filter::attr(Attr::Name, CmpOp::Eq, Value::text(n)),
            )
        }),
        "namespace" => adv_strings(inner).into_iter().next().map(|n| {
            ran.push("namespace".into());
            Filter::rel(
                Rel::Page,
                Quant::Any,
                Filter::attr(Attr::Name, CmpOp::StartsWith, Value::text(format!("{n}/"))),
            )
        }),
        "page-tags" | "tags" => {
            let ts = adv_strings(inner);
            if ts.is_empty() {
                ignored.push(head.clone());
                None
            } else {
                ran.push("page-tags".into());
                Some(Filter::rel(
                    Rel::Page,
                    Quant::Any,
                    Filter::rel(
                        Rel::Props,
                        Quant::Any,
                        Filter::and(vec![
                            Filter::attr(Attr::Key, CmpOp::Eq, Value::text("tags")),
                            Filter::attr(Attr::Value, CmpOp::In, adv_text_list(ts)),
                        ]),
                    ),
                ))
            }
        }
        "scheduled" => {
            ran.push("scheduled".into());
            Some(Filter::attr(Attr::Scheduled, CmpOp::IsSet, Value::None))
        }
        "deadline" => {
            ran.push("deadline".into());
            Some(Filter::attr(Attr::Deadline, CmpOp::IsSet, Value::None))
        }
        "journal" => {
            ran.push("journal".into());
            Some(Filter::rel(
                Rel::Page,
                Quant::Any,
                Filter::attr(Attr::Journal, CmpOp::Eq, Value::Bool { value: true }),
            ))
        }
        "between" => {
            // (between [FIELD] ?b ?start ?end): the last two args are always the
            // bounds. An optional field keyword (journal|scheduled|deadline) may
            // appear among the earlier args — matching the simple parser. The bare
            // `(between ?b lo hi)` keeps OG's journal-day semantics.
            let args: Vec<&str> = inner.split_whitespace().skip(1).collect();
            if args.len() < 2 {
                ignored.push("between".into());
                return None;
            }
            let attr = args
                .iter()
                .take(args.len() - 2)
                .find_map(
                    |a| match a.trim_start_matches(':').to_ascii_lowercase().as_str() {
                        "scheduled" => Some(Attr::Scheduled),
                        "deadline" => Some(Attr::Deadline),
                        "journal" => Some(Attr::Day),
                        _ => None,
                    },
                )
                .unwrap_or(Attr::Day);
            let lo = adv_bound(args[args.len() - 2], inputs, today);
            let hi = adv_bound(args[args.len() - 1], inputs, today);
            if lo.is_none() && hi.is_none() {
                ignored.push("between".into());
                return None;
            }
            ran.push("between".into());
            // The advanced dialect resolves its bounds eagerly: `:inputs` may
            // bind a bound to an already-resolved ordinal, and an advanced query
            // is never re-printed as OG DSL, so the IR carries the ordinals.
            let range = adv_range(attr, lo, hi);
            Some(if attr == Attr::Day {
                Filter::rel(Rel::Page, Quant::Any, range)
            } else {
                range
            })
        }
        other => {
            if !other.is_empty() {
                ignored.push(other.to_string());
            }
            None
        }
    }
}

/// All double-quoted string literals in a clause (markers, page names, values).
fn adv_strings(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            let start = i + 1;
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            out.push(s[start..i.min(s.len())].to_string());
        }
        i += 1;
    }
    out
}

/// Resolve a `between` bound: an input `?var` (looked up) or a literal token.
fn adv_bound(
    tok: &str,
    inputs: &std::collections::HashMap<String, AdvancedInput>,
    today: JournalDate,
) -> Option<i64> {
    let t = tok.trim();
    if t.starts_with('?') {
        return match inputs.get(t) {
            Some(AdvancedInput::Date(value)) => Some(*value),
            _ => None,
        };
    }
    // A literal bound may be written as a bare token (`2026-06-24`) or a quoted
    // string (`"2026-06-24"`); `split_whitespace` keeps the quotes, so strip them.
    resolve_date_token(t.trim_matches('"').trim_start_matches(':'), today)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AdvancedInput {
    Date(i64),
    Page(String),
}

/// Build a typed positional input map by zipping `:in $ ?a ?b …` with
/// `:inputs [ … ]`. Dates stay numeric; Logseq's typed `:current-page` keyword
/// receives the caller's focused page. Unknown keywords remain unbound.
fn resolve_inputs(
    src: &str,
    current_page: Option<&str>,
    today: JournalDate,
) -> std::collections::HashMap<String, AdvancedInput> {
    let mut map = std::collections::HashMap::new();
    let vars: Vec<String> = match src.find(":in") {
        Some(i) => {
            let rest = &src[i + 3..];
            let end = rest
                .find(":where")
                .or_else(|| rest.find(']'))
                .unwrap_or(rest.len());
            rest[..end]
                .split_whitespace()
                .filter(|t| t.starts_with('?'))
                .map(String::from)
                .collect()
        }
        None => Vec::new(),
    };
    let vals: Vec<String> = match src.find(":inputs") {
        Some(i) => {
            let rest = &src[i + ":inputs".len()..];
            match (rest.find('['), rest.find(']')) {
                (Some(a), Some(b)) if b > a => rest[a + 1..b]
                    .split_whitespace()
                    .map(String::from)
                    .collect(),
                _ => Vec::new(),
            }
        }
        None => Vec::new(),
    };
    for (v, val) in vars.iter().zip(vals.iter()) {
        if val.eq_ignore_ascii_case(":current-page") {
            if let Some(page) = current_page.map(str::trim).filter(|page| !page.is_empty()) {
                map.insert(v.clone(), AdvancedInput::Page(page.to_lowercase()));
            }
        } else if let Some(ord) = resolve_date_token(val.trim_start_matches(':'), today) {
            map.insert(v.clone(), AdvancedInput::Date(ord));
        }
    }
    map
}

/// Fields naming a block's position on the recency time-axis (journal day for
/// journal pages, file mtime otherwise) — sorted numerically, not lexically.
/// `modified` is the canonical token; `updated`/`updated-at`/`date` are aliases.
fn is_recency_field(field: &str) -> bool {
    matches!(
        field.to_ascii_lowercase().as_str(),
        "modified" | "updated" | "updated-at" | "date"
    )
}

/// [`page_recency_secs`] by the two inputs it actually reads, so a caller that
/// holds a page's journal ordinal and its absolute path — R3's database result
/// read, which never sees a `PageEntry` — asks the SAME producer rather than
/// spelling the axis a second time (I-12, D-14). `pages.journal_day` is
/// `PageEntry::date_key` by construction (`lowering::physical_page` stores it).
pub(crate) fn page_recency_secs_for(date_key: Option<i64>, absolute: &std::path::Path) -> i64 {
    if let Some(dk) = date_key {
        return JournalDate::from_ordinal(dk).to_days() * 86_400;
    }
    std::fs::metadata(absolute)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(i64::MIN)
}

/// Sort key for a result block: the named property's value if present, else the
/// block's visible first line (lowercased for stable case-insensitive order).
fn sort_key(b: &BlockDto, page: &str, field: &str) -> String {
    match field.to_ascii_lowercase().as_str() {
        // Task priority is the `[#A]` marker, NOT a `priority::` property — map it
        // to A<B<C and sort unprioritized blocks last (so ascending floats A to the
        // top). Descending naturally reverses (A sinks to the bottom).
        // Priority off the DTO's lsdoc-derived facet (header-position `[#A]`, matching
        // the chip) — no reparse, no `[#A]`-anywhere false positive (audit C3/P4).
        "priority" => b
            .priority
            .as_deref()
            .map_or_else(|| "Z".to_string(), |c| c.to_ascii_uppercase()),
        // Sort by the source page name.
        "page" => page.to_lowercase(),
        // SCHEDULED / DEADLINE planning dates off the DTO facet (lead with
        // `YYYY-MM-DD`, so lexical order == chronological). Blocks without one sort
        // last in ascending ("soonest first") order via the high sentinel `~`.
        "deadline" => b.deadline.clone().unwrap_or_else(|| "~".to_string()),
        "scheduled" => b.scheduled.clone().unwrap_or_else(|| "~".to_string()),
        // Otherwise: a block property value (off the DTO's lsdoc properties — no
        // reparse, format-correct, audit P4), else the block's visible first line.
        _ => lexical_property_sort_text(
            b.properties
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
            field,
            // Fallback: visible text (the DTO carries no visible text; reparse, bounded
            // to sorted-result blocks via `sort_by_cached_key`).
            || {
                let (_, visible) = crate::doc::block_sort_facets(&b.raw);
                visible.lines().next().unwrap_or("").to_string()
            },
        ),
    }
}

/// Literal fuzzy full-text autocomplete for the `((` block picker, grouped by
/// page and capped at `limit` total blocks. Ctrl-K uses `run_graph_search*` and
/// retains the shared search dialect through `QueryPlan::friendly*`.
pub fn search<G: QueryGraph>(
    graph: &G,
    query: &str,
    limit: usize,
) -> Result<Vec<RefGroup>, QueryExecutionError> {
    graph.search(query, limit)
}

/// Search with cooperative cancellation for interactive callers. The cheap
/// callback is checked before each block projection, so a superseded rare-prefix
/// scan does not finish walking a huge page in the background.
#[cfg(test)]
pub fn search_cancellable<G: QueryGraph>(
    graph: &G,
    query: &str,
    limit: usize,
    cancelled: impl Fn() -> bool,
) -> Vec<RefGroup> {
    let plan = crate::query_plan::QueryPlan::block_search_literal(query, limit);
    let execution = plan.execute(graph, cancelled);
    if execution.cancelled {
        Vec::new()
    } else {
        crate::query_plan::block_hits_to_groups(execution.hits)
    }
}

/// Find every `template:: <name>` block and the blocks an insertion produces.
pub fn templates<G: QueryGraph>(graph: &G) -> Vec<TemplateDto> {
    with_candidate_pages(
        graph,
        graph.indexed_or_fallback(|| graph.indexed_derived_pages(DerivedSelection::Templates)),
        |pages| {
            let mut out: Vec<TemplateDto> = Vec::new();
            for (entry, doc) in pages {
                walk(&doc.roots, &mut |b| {
                    let Some(name) = b.property("template") else {
                        return;
                    };
                    if name.is_empty() {
                        return;
                    }
                    let include_parent =
                        b.property("template-including-parent").as_deref() != Some("false");
                    let blocks = if include_parent {
                        vec![template_dto(b, true)]
                    } else {
                        b.children.iter().map(|c| template_dto(c, false)).collect()
                    };
                    out.push(TemplateDto {
                        name,
                        blocks,
                        page: entry.name.clone(),
                        kind: entry.kind,
                    });
                });
            }
            out
        },
    )
}

/// Convert a template block subtree to a DTO, dropping `id::` (so inserted
/// copies get fresh ids) and, at the root, the `template*` properties.
fn template_dto(b: &DocBlock, strip_template: bool) -> BlockDto {
    let raw = b
        .raw
        .lines()
        .filter(|l| {
            let t = l.trim();
            let drop = t.starts_with("id::")
                || (strip_template
                    && (t.starts_with("template::")
                        || t.starts_with("template-including-parent::")));
            !drop
        })
        .collect::<Vec<_>>()
        .join("\n");
    BlockDto {
        id: String::new(),
        raw,
        collapsed: false,
        children: b.children.iter().map(|c| template_dto(c, false)).collect(),
        breadcrumb: Vec::new(),
        // DUP-8: every non-content field is deliberately reset at insertion;
        // every template walk delegates to this one leaf.
        page_property: false,
        marker: None,
        priority: None,
        heading_level: None,
        scheduled: None,
        deadline: None,
        tags: Vec::new(),
        properties: Vec::new(),
    }
}

#[cfg(test)]
struct ScoredQuickSwitchCand {
    score: i32,
    index: usize,
}

#[cfg(test)]
impl ScoredQuickSwitchCand {
    fn is_better_than(&self, other: &Self) -> bool {
        self.score > other.score || (self.score == other.score && self.index < other.index)
    }
}

#[cfg(test)]
impl PartialEq for ScoredQuickSwitchCand {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score && self.index == other.index
    }
}

#[cfg(test)]
impl Eq for ScoredQuickSwitchCand {}

#[cfg(test)]
impl PartialOrd for ScoredQuickSwitchCand {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
impl Ord for ScoredQuickSwitchCand {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is max-first; define "greater" as worse so the root is the
        // candidate to evict. The final rank remains score desc, index asc.
        other
            .score
            .cmp(&self.score)
            .then_with(|| self.index.cmp(&other.index))
    }
}

#[cfg(test)]
fn push_quick_switch_top(
    heap: &mut std::collections::BinaryHeap<ScoredQuickSwitchCand>,
    limit: usize,
    candidate: ScoredQuickSwitchCand,
) {
    if heap.len() < limit {
        heap.push(candidate);
        return;
    }
    if heap
        .peek()
        .is_some_and(|worst| candidate.is_better_than(worst))
    {
        let mut worst = heap.peek_mut().unwrap();
        *worst = candidate;
    }
}

#[cfg(test)]
fn finish_quick_switch_top(
    heap: std::collections::BinaryHeap<ScoredQuickSwitchCand>,
) -> Vec<ScoredQuickSwitchCand> {
    let mut top = heap.into_vec();
    top.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.index.cmp(&b.index)));
    top
}

/// Fuzzy page-name matcher for the quick switcher. Ranks prefix > substring >
/// subsequence, then by name length.
#[cfg(test)]
pub fn quick_switch(graph: &impl QueryGraph, query: &str, limit: usize) -> Vec<PageEntry> {
    crate::query_plan::pre_ready_page_search_entries(
        graph.list_pages(),
        graph.page_aliases_with_owners(),
        graph.referenced_page_names(),
        query,
        limit,
    )
}

/// Resolve a `((uuid))` block reference to a shallow identity/result row.
/// Descendants are owned by the source page; explicit bounded consumers use
/// `preview_block`.
pub fn resolve_block<G: QueryGraph>(graph: &G, uuid: &str) -> Option<RefGroup> {
    // A ready index supplies only exact candidates. Runtime misses use captured
    // pages or the session locator; only an unavailable index needs the legacy
    // whole-graph fallback.
    let ids = [uuid.to_owned()];
    let candidates =
        graph.indexed_or_fallback(|| graph.indexed_derived_pages(DerivedSelection::Resolve(&ids)));
    let hint = candidates
        .is_err()
        .then(|| graph.block_page_hint(uuid))
        .flatten();
    with_candidate_pages(graph, candidates, |pages| {
        let find_in = |entry: &PageEntry, doc: &Document| -> Option<RefGroup> {
            let mut found: Option<&DocBlock> = None;
            walk(&doc.roots, &mut |b| {
                if found.is_none() && (b.uuid == uuid || b.property("id").as_deref() == Some(uuid))
                {
                    found = Some(b);
                }
            });
            found.map(|b| RefGroup {
                page: entry.name.clone(),
                kind: entry.kind,
                blocks: vec![block_to_shallow_dto(b)],
                evidence: Vec::new(),
            })
        };
        if let Some(h) = &hint {
            if let Some((entry, doc)) = pages.iter().find(|(e, _)| &e.name == h) {
                if let Some(rg) = find_in(entry, doc) {
                    return Some(rg);
                }
            }
        }
        for (entry, doc) in pages {
            if let Some(rg) = find_in(entry, doc) {
                return Some(rg);
            }
        }
        None
    })
}

/// Resolve many `((uuid))` block references in a single graph pass — the real
/// batch behind `Graph::resolve_blocks` (a page full of refs/embeds is one IPC,
/// and now one scan rather than U independent `resolve_block` calls, each of which
/// could whole-graph-scan on a hint miss). Hinted ids are grouped by page and each
/// hinted page is walked ONCE for all of its ids; whatever a hint missed (stale or
/// absent) falls back to a SINGLE whole-graph scan. Match semantics + first-block-
/// wins ordering are identical to `resolve_block`. Output is positional and
/// per-input (duplicate input uuids each get their own `Some(..)`/`None`).
pub fn resolve_blocks<G: QueryGraph>(graph: &G, uuids: &[String]) -> Vec<Option<RefGroup>> {
    resolve_blocks_bounded(graph, uuids, usize::MAX, usize::MAX).0
}

/// Logseq file graphs keep the first block UUID claimant encountered by the
/// parser and rewrite every later duplicate to a fresh UUID. Provenance:
/// logseq/logseq `c67b8b5fa47f8fe1e1954226c9bdfabd46ebb968`,
/// `deps/graph-parser/src/logseq/graph_parser/block.cljs`,
/// `fix-block-id-if-duplicated!`.
///
/// A physical index may call this only when it has parser order. With an
/// ambiguous unordered hint, `None` deliberately requests parser fallback.
pub(crate) fn logseq_uuid_owner<T>(
    claimants: impl IntoIterator<Item = T>,
    parser_ordered: bool,
) -> Option<T> {
    let mut claimants = claimants.into_iter();
    let first = claimants.next()?;
    if parser_ordered || claimants.next().is_none() {
        Some(first)
    } else {
        None
    }
}

pub fn resolve_blocks_bounded<G: QueryGraph>(
    graph: &G,
    uuids: &[String],
    max_rows: usize,
    max_bytes: usize,
) -> (Vec<Option<RefGroup>>, bool, usize) {
    use std::collections::{HashMap, HashSet};
    // Distinct requested ids (a page often refs the same uuid repeatedly).
    let distinct: HashSet<&str> = uuids.iter().map(String::as_str).collect();
    if distinct.is_empty() {
        return (uuids.iter().map(|_| None).collect(), false, 0);
    }
    // Bucket each distinct id under its page hint (O(1) per id off the cached
    // uuid index); unhinted ids go straight to the whole-graph fallback.
    let mut by_page: HashMap<String, Vec<&str>> = HashMap::new();
    let mut unhinted: Vec<&str> = Vec::new();
    let candidates =
        graph.indexed_or_fallback(|| graph.indexed_derived_pages(DerivedSelection::Resolve(uuids)));
    for &id in &distinct {
        match candidates
            .is_err()
            .then(|| graph.block_page_hint(id))
            .flatten()
        {
            Some(page) => by_page.entry(page).or_default().push(id),
            None => unhinted.push(id),
        }
    }

    let mut resolved: HashMap<&str, RefGroup> = HashMap::new();
    let mut resolved_budget = ConstructionBudget::new(max_rows, max_bytes);
    with_candidate_pages(graph, candidates, |pages| {
        let mut page_by_name: HashMap<&str, (&PageEntry, &std::sync::Arc<Document>)> =
            HashMap::with_capacity(pages.len());
        for (entry, doc) in pages {
            page_by_name
                .entry(entry.name.as_str())
                .or_insert((entry, doc));
        }
        // 1) Each hinted page: ONE walk resolving all of its hinted ids.
        for (page, ids) in &by_page {
            if let Some(&(entry, doc)) = page_by_name.get(page.as_str()) {
                let want: HashSet<&str> = ids.iter().copied().collect();
                resolve_ids_in_page(entry, doc, &want, &mut resolved, &mut resolved_budget);
            }
        }
        // 2) Remaining ids (no hint, or the hinted page didn't actually hold the
        //    block) get ONE whole-graph scan — never one-scan-per-id.
        let mut remaining: HashSet<&str> = unhinted.into_iter().collect();
        for &id in &distinct {
            if !resolved.contains_key(id) {
                remaining.insert(id);
            }
        }
        if !remaining.is_empty() {
            for (entry, doc) in pages {
                if resolved.len() == distinct.len() {
                    break; // everything found
                }
                resolve_ids_in_page(entry, doc, &remaining, &mut resolved, &mut resolved_budget);
            }
        }
    });

    let mut output_budget = ConstructionBudget::new(max_rows, max_bytes);
    let output = uuids
        .iter()
        .map(|u| {
            let group = resolved.get(u.as_str())?;
            let block = group.blocks.first()?;
            output_budget
                .admit_estimated(&group.page, crate::vocab::block_dto_estimated_bytes(block))
                .then(|| group.clone())
        })
        .collect();
    (
        output,
        resolved_budget.exceeded || output_budget.exceeded,
        output_budget.total,
    )
}

fn subtree_node_count(root: &DocBlock) -> usize {
    let mut count = 0usize;
    let mut stack = vec![root];
    while let Some(block) = stack.pop() {
        count = count.saturating_add(1);
        stack.extend(block.children.iter());
    }
    count
}

fn block_to_bounded_dto(
    block: &DocBlock,
    remaining_nodes: &mut usize,
    remaining_bytes: &mut usize,
) -> Option<BlockDto> {
    if *remaining_nodes == 0 {
        return None;
    }
    let minimum_bytes = block
        .raw
        .len()
        .saturating_add(if block.uuid.is_empty() {
            36
        } else {
            block.uuid.len()
        })
        .saturating_add(128);
    if minimum_bytes > *remaining_bytes {
        return None;
    }
    let mut dto = block_to_shallow_dto(block);
    let dto_bytes = crate::vocab::block_dto_estimated_bytes(&dto);
    if dto_bytes > *remaining_bytes {
        return None;
    }
    *remaining_nodes -= 1;
    *remaining_bytes -= dto_bytes;
    for child in &block.children {
        let Some(child_dto) = block_to_bounded_dto(child, remaining_nodes, remaining_bytes) else {
            break;
        };
        dto.children.push(child_dto);
    }
    Some(dto)
}

/// Resolve one block for a hover/export consumer that explicitly needs a
/// subtree. This compatibility wrapper applies the caller's node bound; native
/// and export consumers use `preview_block_with_budget` to add a byte bound.
pub fn preview_block<G: QueryGraph>(
    graph: &G,
    uuid: &str,
    max_nodes: usize,
) -> Option<BlockPreview> {
    preview_block_with_budget(graph, uuid, max_nodes, usize::MAX)
}

/// Node-and-byte-bounded preview used by IPC and static/export consumers. The
/// byte cap is applied while constructing the DTO, so a legal node count cannot
/// still create an unbounded structured-clone payload. If even the root cannot
/// fit, the preview is returned with an empty block list and the exact omitted
/// count; callers can disclose truncation without confusing "too large" with
/// "block not found".
pub fn preview_block_with_budget<G: QueryGraph>(
    graph: &G,
    uuid: &str,
    max_nodes: usize,
    max_bytes: usize,
) -> Option<BlockPreview> {
    let max_nodes = max_nodes.max(1);
    let max_bytes = max_bytes.max(1);
    let ids = [uuid.to_owned()];
    let candidates =
        graph.indexed_or_fallback(|| graph.indexed_derived_pages(DerivedSelection::Preview(&ids)));
    let hint = candidates
        .is_err()
        .then(|| graph.block_page_hint(uuid))
        .flatten();
    with_candidate_pages(graph, candidates, |pages| {
        let find_in = |entry: &PageEntry, doc: &Document| -> Option<BlockPreview> {
            let mut found: Option<&DocBlock> = None;
            walk(&doc.roots, &mut |block| {
                if found.is_none()
                    && (block.uuid == uuid || block.property("id").as_deref() == Some(uuid))
                {
                    found = Some(block);
                }
            });
            found.map(|block| {
                let total = subtree_node_count(block);
                let mut remaining_nodes = max_nodes;
                let mut remaining_bytes = max_bytes;
                let blocks =
                    block_to_bounded_dto(block, &mut remaining_nodes, &mut remaining_bytes)
                        .into_iter()
                        .collect::<Vec<_>>();
                let emitted = max_nodes - remaining_nodes;
                BlockPreview {
                    group: RefGroup {
                        page: entry.name.clone(),
                        kind: entry.kind,
                        blocks,
                        evidence: Vec::new(),
                    },
                    truncated: total.saturating_sub(emitted),
                }
            })
        };
        if let Some(hint) = &hint {
            if let Some((entry, doc)) = pages.iter().find(|(entry, _)| &entry.name == hint) {
                if let Some(preview) = find_in(entry, doc) {
                    return Some(preview);
                }
            }
        }
        for (entry, doc) in pages {
            if let Some(preview) = find_in(entry, doc) {
                return Some(preview);
            }
        }
        None
    })
}

/// Walk `doc` once, resolving any block whose uuid (or persisted `id::`) is a
/// still-unresolved id in `want`. First block in walk order wins per id (matches
/// `resolve_block`).
fn resolve_ids_in_page<'a>(
    entry: &PageEntry,
    doc: &Document,
    want: &std::collections::HashSet<&'a str>,
    resolved: &mut std::collections::HashMap<&'a str, RefGroup>,
    budget: &mut ConstructionBudget,
) {
    let mut claimants = std::collections::HashMap::<&'a str, Vec<&DocBlock>>::new();
    let mut order = Vec::new();
    walk(&doc.roots, &mut |b| {
        // A block's identity is its uuid OR its persisted `id::`; check both
        // against the wanted set with O(1) lookups (no per-id rescan).
        let hit: Option<&'a str> = want
            .get(b.uuid.as_str())
            .copied()
            .filter(|id| !resolved.contains_key(id))
            .or_else(|| {
                b.property("id")
                    .and_then(|id| want.get(id.as_str()).copied())
                    .filter(|id| !resolved.contains_key(id))
            });
        if let Some(id) = hit {
            if !claimants.contains_key(id) {
                order.push(id);
            }
            claimants.entry(id).or_default().push(b);
        }
    });
    for id in order {
        if let Some(block) = logseq_uuid_owner(claimants.remove(id).unwrap_or_default(), true) {
            if budget.closed() {
                budget.deny_match();
                continue;
            }
            if budget.admit_estimated(&entry.name, shallow_dto_estimated_bytes(block, &[])) {
                let dto = result_dto(block);
                resolved.insert(
                    id,
                    RefGroup {
                        page: entry.name.clone(),
                        kind: entry.kind,
                        blocks: vec![dto],
                        evidence: Vec::new(),
                    },
                );
            }
        }
    }
}

/// Result-level options extracted from the query's VIEW settings. The walk still
/// consumes this shape in `finish_query_groups`; `ViewSettings` is the truth
/// (Q15) and this is the one adapter between them (it replaced `collect_opts`,
/// which read sort/sample back out of the filter tree).
#[derive(Debug, Default, Clone)]
pub(crate) struct QueryOpts {
    sample: Option<usize>,
    sort: Vec<(String, bool)>, // ordered (field, ascending) clauses
}

impl QueryOpts {
    pub(crate) fn from_view(view: &ViewSettings) -> QueryOpts {
        QueryOpts {
            sample: view.sample.map(|n| n as usize),
            sort: view
                .sort
                .iter()
                .map(|(field, dir)| (field.0.clone(), *dir == SortDir::Asc))
                .collect(),
        }
    }

    fn uses_recency(&self) -> bool {
        self.sort.iter().any(|(field, _)| is_recency_field(field))
    }
}

/// A `(between …)` range whose bounds the advanced dialect already resolved to
/// `yyyymmdd` ordinals.
fn adv_range(attr: Attr, low: Option<i64>, high: Option<i64>) -> Filter {
    let number = |value: i64| Value::Number {
        number: value as f64,
    };
    match (low, high) {
        (Some(low), Some(high)) => {
            // OG's `build-between-two-arg` sorts its two bounds, so
            // `(between END START)` is the same inclusive interval.
            let (low, high) = if low > high { (high, low) } else { (low, high) };
            Filter::attr(
                attr,
                CmpOp::Between,
                Value::List {
                    items: vec![number(low), number(high)],
                },
            )
        }
        (Some(low), None) => Filter::attr(attr, CmpOp::Ge, number(low)),
        (None, Some(high)) => Filter::attr(attr, CmpOp::Le, number(high)),
        (None, None) => Filter::attr(attr, CmpOp::IsSet, Value::None),
    }
}

fn adv_text_list(values: Vec<String>) -> Value {
    Value::List {
        items: values.into_iter().map(Value::text).collect(),
    }
}

fn strip_ref(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix('#').unwrap_or(t).trim();
    let t = t
        .strip_prefix("[[")
        .and_then(|x| x.strip_suffix("]]"))
        .unwrap_or(t);
    t.trim().to_string()
}

/// Resolve a `between` bound token to a `yyyymmdd` ordinal: `today`/`yesterday`/
/// `tomorrow`, signed durations `±N[dwmy]`, `yyyy-MM-dd`, or a journal title.
fn resolve_date_token(tok: &str, today: JournalDate) -> Option<i64> {
    let t = tok.trim();
    match t.to_ascii_lowercase().as_str() {
        "today" | "now" => return Some(today.ordinal_key()),
        "yesterday" => return Some(today.add_days(-1).ordinal_key()),
        "tomorrow" => return Some(today.add_days(1).ordinal_key()),
        _ => {}
    }
    if let Some(d) = parse_relative(t, today) {
        return Some(d.ordinal_key());
    }
    if let Some(jd) = JournalDate::from_file_stem(t) {
        return Some(jd.ordinal_key());
    }
    journal_ordinal(t)
}

/// Parse a signed relative duration like `-7d`, `+2w`, `3m`, `-1y` off `today`.
fn parse_relative(t: &str, today: JournalDate) -> Option<JournalDate> {
    let bytes = t.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let (sign, rest) = match bytes[0] {
        b'+' => (1i64, &t[1..]),
        b'-' => (-1i64, &t[1..]),
        _ => (1i64, t),
    };
    let unit = rest.chars().last()?;
    if !matches!(unit, 'd' | 'w' | 'm' | 'y') {
        return None;
    }
    let n: i64 = rest[..rest.len() - 1].parse().ok()?;
    let n = sign * n;
    Some(match unit {
        'd' => today.add_days(n),
        'w' => today.add_days(n * 7),
        'm' => today.add_months(n),
        'y' => today.add_months(n * 12),
        _ => return None,
    })
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod tests;
