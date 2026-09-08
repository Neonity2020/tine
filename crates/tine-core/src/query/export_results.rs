//! RET3: **database-owned export subtree construction**, over the caller's own
//! snapshots.
//!
//! Copy / Export answers a query macro with the SUBTREE under each selected
//! result, not just the matched row. Today that is done by re-scanning parsed
//! source documents: `hydrate_selected_export_queries` walks every candidate
//! page looking for a block whose `uuid` — or whose `id::` property — equals a
//! selected result's PUBLIC id. Two things are wrong with that as a database
//! answer, and this module exists to fix both.
//!
//! * **A public id is not an identity.** Two physical blocks may expose the
//!   same public id (an `id::` property copied between pages) and two physical
//!   pages may share a display name. The source re-scan resolves such a pair by
//!   whichever page it happened to reach first. Here, selection attaches a
//!   [`ResultLocator`] — source, physical page id, physical block id — at
//!   admission, while the collector still owns those facts, and hydration reads
//!   exactly that block on exactly that snapshot.
//! * **A subtree is not a page.** Loading a whole document to answer "the four
//!   blocks under this one" is the same N+1 R3 removed from result reads. Here
//!   the subtree is a contiguous run of `query_block_results.preorder` inside
//!   one page, read in batches of [`TOPOLOGY_BATCH`], and OUTPUT PAYLOAD is
//!   read only for the nodes the budget actually admitted.
//!
//! **What this module does not do.** It opens no connection, acquires no job,
//! actor or graph lock, reads no source document, and performs no recovery or
//! retry. It receives the SAME `&mut` snapshots the selection ran on, plus the
//! identity policy the caller captured beside them. Capacity, snapshot
//! acquisition, the public Direct/Managed command adapters, the final
//! cancellation check and releasing the transactions all stay with the caller.
//! [`ResultReadError`] is the shared failure vocabulary.
//!
//! **The budget is the oracle's, transcribed.**
//! [`crate::query::select_export_queries_over`] owns the macro cap and the
//! global root pool for BOTH routes. The node/byte budget below is
//! `block_to_bounded_dto`'s exact rule, and it is deliberately NOT a global
//! preorder prefix: when a child does not fit, that parent's remaining child
//! loop stops and the parent is still emitted, so an ANCESTOR's later sibling
//! can still fit. It runs on an explicit stack rather than by recursion, so a
//! deep but perfectly valid tree cannot overflow the thread stack.

use std::collections::{HashMap, HashSet};

use tine_storage::sqlite::{PhysicalProjectionQuerySnapshot, PhysicalQueryValue};

use crate::model::{block_dto_estimated_bytes, BlockDto, PageKind, RefGroup};
use crate::query::ir::ViewSettings;
use crate::query::results::{
    blob16, count, integer, placeholders, read_admitted_payload, resolve_identity,
    sql_or_cancelled, text, LocatedPreViewGroups, PayloadChannel, PayloadFacts, ResultIdentity,
    ResultLocator, ResultReadError,
};
use crate::query::{
    finish_result_view_groups, select_export_queries_over, ExportSelectionAnswer,
    QueryExportResult, QueryExportSpec, QueryOpts, SelectedExportQueryOf,
};

// The gates. `#[path]` keeps the file beside this one so the shared
// production-source scanner sees a `*_tests.rs` sibling include and blanks it
// from every census, exactly as `results.rs` does for `results_tests.rs`.
#[cfg(test)]
#[path = "export_results_tests.rs"]
mod export_results_tests;

/// How many rows one topology statement binds or returns.
///
/// The same 128 the payload reader uses, for the same reason: a subtree read is
/// `ceil(nodes / 128)` statements and nothing else — never one statement per
/// node, and never a statement whose cost is the size of the PAGE the subtree
/// happens to live on.
pub(crate) const TOPOLOGY_BATCH: usize = 128;

/// The sentinel "no parent" index. `usize::MAX` rather than `Option<usize>`:
/// these vectors are hot, index-dense and entirely local to this module.
const NO_PARENT: usize = usize::MAX;

/// One export source: the SAME owned read snapshot the SELECTION ran on.
///
/// `source` in a [`ResultLocator`] indexes this slice; it is not itself a proof
/// of snapshot identity. The manager-owned job adapter must retain these exact
/// handles across selection and hydration. Substituting a later handle could
/// otherwise mix the selected root DTO with another revision's descendants.
pub(crate) struct ExportSubtreeSource<'a> {
    pub(crate) snapshot: &'a mut PhysicalProjectionQuerySnapshot,
}

/// Everything the bounded construction needs besides the snapshots.
pub(crate) struct ExportSubtreeInputs<'a> {
    /// The identity policy CAPTURED by the caller beside its snapshots — the
    /// same value the selection resolved its public ids with, so a descendant
    /// and its own root cannot answer to two different identity rules.
    pub(crate) identity: &'a ResultIdentity,
    pub(crate) max_nodes: usize,
    pub(crate) max_bytes: usize,
}

/// One selected export root: its shallow SELECTION DTO and its exact locator.
///
/// The DTO is the row the view already produced, reused rather than re-read:
/// `block_to_shallow_dto` (what the oracle's hydration builds for a root) and
/// `result_dto` (what its selection builds) are the same constructor, so the
/// root costs no output payload statement at all.
pub(crate) struct LocatedExportRoot {
    pub(crate) page: String,
    pub(crate) kind: PageKind,
    pub(crate) block: BlockDto,
    pub(crate) locator: ResultLocator,
}

/// One query macro's located selection.
pub(crate) type LocatedExportQuery = SelectedExportQueryOf<LocatedExportRoot>;

/// §5.9's view, applied to a LOCATED pre-view result (RET3).
///
/// [`finish_result_view_groups`] is the one owner: base order, then `sort-by`,
/// then `sample`, moving each entry AND its locator together. No located sort,
/// no located sampler, no recency policy of its own.
pub(crate) fn apply_located_view(
    pre: LocatedPreViewGroups,
    view: &ViewSettings,
) -> ExportSelectionAnswer<(BlockDto, ResultLocator)> {
    let opts = QueryOpts::from_view(view);
    ExportSelectionAnswer {
        groups: finish_result_view_groups(pre.groups, &pre.recency_by_page, &opts),
        total: pre.total,
        exceeded: pre.exceeded,
    }
}

/// The located half of [`crate::query::select_export_queries_over`]: the same
/// macro cap, the same global root pool, the same "an exceeded selection emits
/// no roots but keeps its total" rule — with each root carrying its physical
/// locator instead of a public id.
pub(crate) fn select_located_export_queries<E>(
    specs: &[QueryExportSpec],
    max_queries: usize,
    max_roots: usize,
    evaluate: impl FnMut(
        &QueryExportSpec,
    ) -> Result<ExportSelectionAnswer<(BlockDto, ResultLocator)>, E>,
) -> Result<(usize, Vec<LocatedExportQuery>), E> {
    select_export_queries_over(
        specs,
        max_queries,
        max_roots,
        evaluate,
        |page, kind, (block, locator)| LocatedExportRoot {
            page: page.to_owned(),
            kind,
            block,
            locator,
        },
    )
}

/// **Hydrate the selected roots' subtrees from the projection alone.**
///
/// The whole construction, in four passes over the SAME snapshots:
///
/// 1. the roots' pages and preorders, batched — this is where a root whose
///    locator does not own its page is rejected;
/// 2. per root, in input macro/root order: the subtree's TOPOLOGY (ids, parent
///    links and stored estimates, no payload), then the node/byte admission;
/// 3. OUTPUT PAYLOAD, for the admitted descendants only, through the shared
///    payload reader in batches of `PAYLOAD_BATCH`;
/// 4. assembly, on an explicit stack.
///
/// `omitted_nodes` counts every requested descendant that was not emitted,
/// including the ones discovered after the output budget closed — which is why
/// pass 2 reads a root's whole topology even when nothing more can fit.
pub(crate) fn hydrate_located_export_queries(
    sources: &mut [ExportSubtreeSource<'_>],
    selected: Vec<LocatedExportQuery>,
    inputs: &ExportSubtreeInputs<'_>,
) -> Result<Vec<QueryExportResult>, ResultReadError> {
    let paths = read_root_page_paths(sources, &selected)?;
    let preorders = read_root_preorders(sources, &selected)?;

    // The two global budgets, clamped exactly as the oracle clamps them, and
    // charged in input macro/root order across every macro.
    let mut remaining_nodes = inputs.max_nodes.max(1);
    let mut remaining_bytes = inputs.max_bytes.max(1);

    let mut emitted: Vec<EmittedNode> = Vec::new();
    let mut outcomes: Vec<QueryOutcome> = Vec::with_capacity(selected.len());
    // Two identical roots (the same block selected by two macros, or twice by
    // one) cost repeated OUTPUT budget, exactly as the oracle charges them.
    // Re-reading their identical topology would be pure waste, so it is cached
    // — a cache that cannot move an admission or an omission count, because it
    // returns the same rows the second read would have.
    let mut topologies: HashMap<(usize, [u8; 16]), Subtree> = HashMap::new();

    for query in selected {
        let mut roots = Vec::with_capacity(query.roots.len());
        for root in query.roots {
            let locator = root.locator;
            let key = (locator.source, locator.block_id);
            if !topologies.contains_key(&key) {
                let preorder = preorders[&key];
                let path = paths[&(locator.source, locator.page_id)].clone();
                let subtree = read_subtree_topology(
                    &mut *sources[locator.source].snapshot,
                    locator,
                    preorder,
                    &path,
                    inputs.identity,
                )?;
                topologies.insert(key, subtree);
            }
            let subtree = &topologies[&key];
            let root_estimate = block_dto_estimated_bytes(&root.block);
            let admitted = admit_subtree(
                subtree,
                root_estimate,
                &mut remaining_nodes,
                &mut remaining_bytes,
            );
            // `1 + descendants` is the oracle's `subtree_node_count`, and the
            // count of what did NOT fit is taken against it whether or not the
            // root itself was emitted.
            let requested = subtree.nodes.len() + 1;
            let omitted_nodes = requested.saturating_sub(admitted.order.len());
            let mut root_block = Some(root.block);
            let root_at = if admitted.order.is_empty() {
                None
            } else {
                let base = emitted.len();
                for (at, node) in admitted.order.iter().copied().enumerate() {
                    let parent = admitted.parents[at];
                    emitted.push(EmittedNode {
                        source: locator.source,
                        page_id: locator.page_id,
                        block_id: match node {
                            0 => locator.block_id,
                            other => subtree.nodes[other - 1].block_id,
                        },
                        payload: match node {
                            // The root's payload is the row the SELECTION
                            // already built and validated. Nothing re-reads it.
                            0 => NodePayload::Ready(
                                root_block.take().expect("one root node per occurrence"),
                            ),
                            other => {
                                let stored = &subtree.nodes[other - 1];
                                NodePayload::Wanted(WantedFacts {
                                    result_id: stored.result_id.clone(),
                                    estimated_bytes: stored.estimated_bytes,
                                    tag_count: stored.tag_count,
                                    property_count: stored.property_count,
                                })
                            }
                        },
                        parent: if parent == NO_PARENT {
                            NO_PARENT
                        } else {
                            base + parent
                        },
                        dto: None,
                    });
                }
                Some(base)
            };
            roots.push(RootOutcome {
                page: root.page,
                kind: root.kind,
                root: root_at,
                omitted_nodes,
            });
        }
        outcomes.push(QueryOutcome {
            key: query.key,
            total: query.total,
            roots,
        });
    }

    read_output_payload(sources, &mut emitted)?;
    Ok(assemble(outcomes, emitted))
}

// ===== pass 1: the roots' pages and preorders =====

/// Every root's page PATH, per source, batched.
///
/// The path is what a fresh Direct Files session's structural identity policy
/// resolves a descendant's public id from (`doc_runtime_id_for_order`), and a
/// page that has no row on the source its locator names is a projection that
/// contradicts itself.
fn read_root_page_paths(
    sources: &mut [ExportSubtreeSource<'_>],
    selected: &[LocatedExportQuery],
) -> Result<HashMap<(usize, [u8; 16]), String>, ResultReadError> {
    let mut wanted: Vec<HashSet<[u8; 16]>> = (0..sources.len()).map(|_| HashSet::new()).collect();
    for query in selected {
        for root in &query.roots {
            let Some(source) = wanted.get_mut(root.locator.source) else {
                return Err(ResultReadError::Corrupt(
                    "an export root names a source this batch does not own".to_string(),
                ));
            };
            source.insert(root.locator.page_id);
        }
    }
    let mut paths = HashMap::new();
    for (at, ids) in wanted.into_iter().enumerate() {
        // Sorted, so the batching (and therefore the statement count a gate
        // asserts) is a property of the input and not of a hash seed.
        let mut ids: Vec<[u8; 16]> = ids.into_iter().collect();
        ids.sort_unstable();
        for batch in ids.chunks(TOPOLOGY_BATCH) {
            let snapshot = &mut *sources[at].snapshot;
            if snapshot.cancellation().is_cancelled() {
                return Err(ResultReadError::Cancelled);
            }
            let sql = format!(
                "SELECT page_id, path FROM pages WHERE page_id IN ({})",
                placeholders(batch.len())
            );
            let params = batch
                .iter()
                .map(|id| PhysicalQueryValue::Blob(id.to_vec()))
                .collect::<Vec<_>>();
            #[cfg(test)]
            note(|census| census.page_statements += 1);
            let rows = snapshot
                .run_projection_query(&sql, &params)
                .map_err(|error| sql_or_cancelled(snapshot, error))?;
            for row in &rows {
                let decoded = (|| {
                    Ok::<_, String>((
                        blob16(row, 0, "pages.page_id")?,
                        text(row, 1, "pages.path")?,
                    ))
                })()
                .map_err(ResultReadError::Corrupt)?;
                paths.insert((at, decoded.0), decoded.1);
            }
            for id in batch {
                if !paths.contains_key(&(at, *id)) {
                    return Err(ResultReadError::Corrupt(
                        "an export root's page has no page row".to_string(),
                    ));
                }
            }
        }
    }
    Ok(paths)
}

/// Every root's PREORDER, per source, batched — and the ownership check that
/// makes duplicate public ids and equal display names harmless.
///
/// `query_block_results.page_id` must be the page the locator names. That is
/// the whole of "root physical ownership survives duplicate exposed ids": the
/// row is found by its physical block id, and the page it claims has to be the
/// one the selection admitted it under.
fn read_root_preorders(
    sources: &mut [ExportSubtreeSource<'_>],
    selected: &[LocatedExportQuery],
) -> Result<HashMap<(usize, [u8; 16]), i64>, ResultReadError> {
    let mut wanted: Vec<HashMap<[u8; 16], [u8; 16]>> =
        (0..sources.len()).map(|_| HashMap::new()).collect();
    for query in selected {
        for root in &query.roots {
            let Some(source) = wanted.get_mut(root.locator.source) else {
                return Err(ResultReadError::Corrupt(
                    "an export root names a source this batch does not own".to_string(),
                ));
            };
            source.insert(root.locator.block_id, root.locator.page_id);
        }
    }
    let mut preorders = HashMap::new();
    for (at, ids) in wanted.into_iter().enumerate() {
        let mut ordered: Vec<([u8; 16], [u8; 16])> = ids.into_iter().collect();
        ordered.sort_unstable();
        for batch in ordered.chunks(TOPOLOGY_BATCH) {
            let snapshot = &mut *sources[at].snapshot;
            if snapshot.cancellation().is_cancelled() {
                return Err(ResultReadError::Cancelled);
            }
            let sql = format!(
                "SELECT block_id, page_id, preorder FROM query_block_results \
                 WHERE block_id IN ({})",
                placeholders(batch.len())
            );
            let params = batch
                .iter()
                .map(|(id, _)| PhysicalQueryValue::Blob(id.to_vec()))
                .collect::<Vec<_>>();
            #[cfg(test)]
            note(|census| census.root_statements += 1);
            let rows = snapshot
                .run_projection_query(&sql, &params)
                .map_err(|error| sql_or_cancelled(snapshot, error))?;
            let owned: HashMap<[u8; 16], [u8; 16]> = batch.iter().copied().collect();
            for row in &rows {
                let decoded = (|| {
                    Ok::<_, String>((
                        blob16(row, 0, "query_block_results.block_id")?,
                        blob16(row, 1, "query_block_results.page_id")?,
                        integer(row, 2, "query_block_results.preorder")?,
                    ))
                })()
                .map_err(ResultReadError::Corrupt)?;
                if owned.get(&decoded.0) != Some(&decoded.1) {
                    return Err(ResultReadError::Corrupt(
                        "an export root's block is owned by a different page".to_string(),
                    ));
                }
                if decoded.2 < 0 {
                    return Err(ResultReadError::Corrupt(
                        "query_block_results.preorder is negative".to_string(),
                    ));
                }
                preorders.insert((at, decoded.0), decoded.2);
            }
            for (id, _) in batch {
                if !preorders.contains_key(&(at, *id)) {
                    return Err(ResultReadError::Corrupt(
                        "an export root has no result row".to_string(),
                    ));
                }
            }
        }
    }
    Ok(preorders)
}

// ===== pass 2: topology =====

/// One root's requested descendants: preorder-ordered, parent-linked, and
/// carrying the stored construction estimate — but no payload.
struct Subtree {
    /// The DESCENDANTS only, in whole-page preorder. Node index `0` is the
    /// root itself and `nodes[i]` is node index `i + 1`.
    nodes: Vec<TopologyNode>,
    /// `children[i]` are the node indices of node `i`'s children, in sibling
    /// order. `children[0]` are the root's.
    children: Vec<Vec<usize>>,
}

struct TopologyNode {
    block_id: [u8; 16],
    /// The node index of this node's parent — never `NO_PARENT`, because a
    /// descendant that is not under the root is where the scan stops.
    parent: usize,
    /// The public id this descendant will carry, already resolved through the
    /// captured identity policy.
    result_id: String,
    /// The stored estimate, with the identity term adjusted the same way.
    estimated_bytes: usize,
    tag_count: usize,
    property_count: usize,
}

/// Read one root's whole requested subtree, in batches of [`TOPOLOGY_BATCH`].
///
/// `query_block_results` is
/// written once per page from the producer's own DFS, so a page's `preorder`
/// values are dense `0..n-1` and a subtree is a CONTIGUOUS run of them. The
/// unique `(page_id, preorder)` index therefore answers "the next 128 rows
/// after this block" directly. A separate parent-index count validates that
/// this enumeration omitted no child, including missing metadata at page end.
///
/// The scan stops at the first row whose parent is not already a member: that
/// is where the subtree ends. Everything else is damage and fails the read:
/// a `blocks` row that is missing (the join is LEFT, so it is visible), a block
/// that claims a different page than its result row, a preorder gap, a block
/// seen twice, or a boundary whose parent is not an EARLIER block of this page
/// — which is what a corrupted parent chain inside the subtree looks like from
/// here.
fn read_subtree_topology(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    locator: ResultLocator,
    root_preorder: i64,
    path: &str,
    identity: &ResultIdentity,
) -> Result<Subtree, ResultReadError> {
    let mut nodes: Vec<TopologyNode> = Vec::new();
    let mut index_of: HashMap<[u8; 16], usize> = HashMap::from([(locator.block_id, 0usize)]);
    let mut cursor = root_preorder;
    let mut boundary_parent: Option<[u8; 16]> = None;

    'scan: loop {
        if snapshot.cancellation().is_cancelled() {
            return Err(ResultReadError::Cancelled);
        }
        let sql = "SELECT b.block_id, b.parent_block_id, b.page_id, b.order_key, \
                   q.preorder, q.result_id, q.estimated_bytes, q.tag_count, q.property_count \
                   FROM query_block_results q \
                   LEFT JOIN blocks b ON b.block_id = q.block_id \
                   WHERE q.page_id = ?1 AND q.preorder > ?2 \
                   ORDER BY q.preorder LIMIT ?3";
        let params = [
            PhysicalQueryValue::Blob(locator.page_id.to_vec()),
            PhysicalQueryValue::Integer(cursor),
            PhysicalQueryValue::Integer(TOPOLOGY_BATCH as i64),
        ];
        #[cfg(test)]
        note(|census| census.topology_statements += 1);
        let rows = snapshot
            .run_projection_query(sql, &params)
            .map_err(|error| sql_or_cancelled(snapshot, error))?;
        let complete = rows.len() == TOPOLOGY_BATCH;
        #[cfg(test)]
        note(|census| census.topology_rows_fetched += rows.len());
        for row in &rows {
            #[cfg(test)]
            note(|census| census.topology_rows_examined += 1);
            let decoded = decode_topology_row(row).map_err(ResultReadError::Corrupt)?;
            if decoded.page_id != locator.page_id {
                return Err(ResultReadError::Corrupt(
                    "a block row names a different page than its result row".to_string(),
                ));
            }
            // The producer numbers a page's blocks `0..n-1` in one pass, so a
            // gap means a result row is missing and the run this scan is
            // walking is not the subtree it claims to be.
            if decoded.preorder != cursor + 1 {
                return Err(ResultReadError::Corrupt(
                    "query_block_results.preorder is not dense inside a page".to_string(),
                ));
            }
            cursor = decoded.preorder;
            let parent = match decoded.parent.and_then(|id| index_of.get(&id).copied()) {
                Some(parent) => parent,
                None => {
                    boundary_parent = decoded.parent;
                    break 'scan;
                }
            };
            let (result_id, estimated_bytes) = resolve_identity(
                identity,
                decoded.page_id,
                path,
                &decoded.order_key,
                &decoded.result_id,
                decoded.estimated_bytes,
            )
            .map_err(ResultReadError::Corrupt)?;
            let at = nodes.len() + 1;
            if index_of.insert(decoded.block_id, at).is_some() {
                return Err(ResultReadError::Corrupt(
                    "one block appears twice in a page's preorder".to_string(),
                ));
            }
            nodes.push(TopologyNode {
                block_id: decoded.block_id,
                parent,
                result_id,
                estimated_bytes,
                tag_count: decoded.tag_count,
                property_count: decoded.property_count,
            });
        }
        if !complete {
            // The page ended, so the subtree ends with it.
            break;
        }
    }

    if let Some(parent) = boundary_parent {
        verify_boundary_parent(snapshot, locator.page_id, parent, root_preorder)?;
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len() + 1];
    for (at, node) in nodes.iter().enumerate() {
        children[node.parent].push(at + 1);
    }
    verify_subtree_completeness(snapshot, locator, &nodes, &children)?;
    Ok(Subtree { nodes, children })
}

/// Discovered children are already validated members of `blocks`. Equal child
/// counts therefore prove completeness without transporting every child again.
/// Include leaves: a missing final child otherwise makes its parent look empty.
fn verify_subtree_completeness(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    locator: ResultLocator,
    nodes: &[TopologyNode],
    children: &[Vec<usize>],
) -> Result<(), ResultReadError> {
    let parents: Vec<_> = std::iter::once(locator.block_id)
        .chain(nodes.iter().map(|node| node.block_id))
        .collect();
    for (batch_index, batch) in parents.chunks(TOPOLOGY_BATCH).enumerate() {
        #[cfg(test)]
        BEFORE_COMPLETENESS_BATCH.with(|slot| {
            let hook = slot.borrow_mut().take();
            if let Some(hook) = hook {
                hook(batch_index);
                *slot.borrow_mut() = Some(hook);
            }
        });
        if snapshot.cancellation().is_cancelled() {
            return Err(ResultReadError::Cancelled);
        }
        let sql = format!(
            "SELECT parent_block_id, page_id, COUNT(*) FROM blocks \
             WHERE parent_block_id IN ({}) GROUP BY parent_block_id, page_id",
            placeholders(batch.len())
        );
        let params: Vec<_> = batch
            .iter()
            .map(|id| PhysicalQueryValue::Blob(id.to_vec()))
            .collect();
        let mut actual = HashMap::new();
        #[cfg(test)]
        note(|census| {
            census.completeness_statements += 1;
            census.completeness_parents += batch.len();
        });
        let rows = snapshot
            .run_projection_query(&sql, &params)
            .map_err(|error| sql_or_cancelled(snapshot, error))?;
        for row in rows {
            let parent =
                blob16(&row, 0, "blocks.parent_block_id").map_err(ResultReadError::Corrupt)?;
            let page = blob16(&row, 1, "blocks.page_id").map_err(ResultReadError::Corrupt)?;
            let total = count(&row, 2, "child count").map_err(ResultReadError::Corrupt)?;
            if page != locator.page_id
                || !batch.contains(&parent)
                || actual.insert(parent, total).is_some()
            {
                return Err(ResultReadError::Corrupt(
                    "subtree child ownership is inconsistent".into(),
                ));
            }
        }
        for (offset, parent) in batch.iter().enumerate() {
            let expected = children[batch_index * TOPOLOGY_BATCH + offset].len();
            if actual.get(parent).copied().unwrap_or(0) != expected {
                return Err(ResultReadError::Corrupt(
                    "subtree result metadata omits stored children".into(),
                ));
            }
        }
    }
    Ok(())
}

/// The row after a subtree belongs to an ANCESTOR of its root.
///
/// A `NULL` parent is a page root and needs no check. Otherwise the parent must
/// be a block of this page that sorts BEFORE the root: a parent inside (or
/// after) the subtree that was never admitted as a member means the chain the
/// scan followed is broken, and a parent that is not on this page at all means
/// the row's ownership is. Either way the export FAILS — it does not quietly
/// return the shorter subtree the damage would otherwise produce.
fn verify_boundary_parent(
    snapshot: &mut PhysicalProjectionQuerySnapshot,
    page_id: [u8; 16],
    parent: [u8; 16],
    root_preorder: i64,
) -> Result<(), ResultReadError> {
    if snapshot.cancellation().is_cancelled() {
        return Err(ResultReadError::Cancelled);
    }
    #[cfg(test)]
    note(|census| census.boundary_statements += 1);
    let rows = snapshot
        .run_projection_query(
            "SELECT preorder FROM query_block_results WHERE block_id = ?1 AND page_id = ?2",
            &[
                PhysicalQueryValue::Blob(parent.to_vec()),
                PhysicalQueryValue::Blob(page_id.to_vec()),
            ],
        )
        .map_err(|error| sql_or_cancelled(snapshot, error))?;
    let Some(row) = rows.first() else {
        return Err(ResultReadError::Corrupt(
            "a block's parent is not a block of its own page".to_string(),
        ));
    };
    let preorder =
        integer(row, 0, "query_block_results.preorder").map_err(ResultReadError::Corrupt)?;
    if preorder >= root_preorder {
        return Err(ResultReadError::Corrupt(
            "a requested subtree's parent chain is malformed".to_string(),
        ));
    }
    Ok(())
}

/// One topology row, decoded and validated as a shape. Ownership, density and
/// membership are the caller's checks; this one only refuses a row that is not
/// a row.
struct TopologyRow {
    block_id: [u8; 16],
    parent: Option<[u8; 16]>,
    page_id: [u8; 16],
    order_key: String,
    preorder: i64,
    result_id: String,
    estimated_bytes: usize,
    tag_count: usize,
    property_count: usize,
}

fn decode_topology_row(row: &[PhysicalQueryValue]) -> Result<TopologyRow, String> {
    if row.len() != 9 {
        return Err(format!(
            "topology row has {} columns, expected 9",
            row.len()
        ));
    }
    // The join is LEFT precisely so that a result row whose block is gone is
    // VISIBLE here rather than silently absent from the answer (D-3).
    let block_id = blob16(row, 0, "blocks.block_id")?;
    let parent = match row.get(1) {
        Some(PhysicalQueryValue::Null) => None,
        _ => Some(blob16(row, 1, "blocks.parent_block_id")?),
    };
    let page_id = blob16(row, 2, "blocks.page_id")?;
    let order_key = text(row, 3, "blocks.order_key")?;
    let preorder = integer(row, 4, "query_block_results.preorder")?;
    let result_id = text(row, 5, "query_block_results.result_id")?;
    if result_id.is_empty() {
        return Err("query_block_results.result_id is empty".to_string());
    }
    Ok(TopologyRow {
        block_id,
        parent,
        page_id,
        order_key,
        preorder,
        result_id,
        estimated_bytes: count(row, 6, "query_block_results.estimated_bytes")?,
        tag_count: count(row, 7, "query_block_results.tag_count")?,
        property_count: count(row, 8, "query_block_results.property_count")?,
    })
}

// ===== pass 2b: the recursive node/byte admission =====

/// Which nodes of one subtree the global budget admitted, in emission order.
struct Admission {
    /// Node indices, root first, in the order `block_to_bounded_dto` emits.
    order: Vec<usize>,
    /// For each entry of `order`, its parent's POSITION IN `order`, or
    /// [`NO_PARENT`] for the subtree's root.
    parents: Vec<usize>,
}

/// `block_to_bounded_dto`'s budget rule, on an explicit stack.
///
/// **This is deliberately not a global preorder prefix.** When a child does not
/// fit, only that parent's remaining child loop stops; the parent is still
/// emitted and its own parent carries on with ITS next child, which can still
/// fit. Preserving that exact shape is the whole reason this is transcribed
/// rather than replaced by a cheaper "emit until the budget closes".
///
/// The oracle's two byte checks collapse into one here, and provably: its
/// pre-check is `raw + id + 128` and its real check is the DTO estimate
/// `id + raw + tags + properties + 128`, which is never smaller, so a node the
/// pre-check would reject is a node the estimate rejects too. The estimate is
/// the STORED one, identity-adjusted by the same owner the result read uses,
/// and pass 3 re-checks it against the payload that is actually decoded.
fn admit_subtree(
    subtree: &Subtree,
    root_estimate: usize,
    remaining_nodes: &mut usize,
    remaining_bytes: &mut usize,
) -> Admission {
    let estimate = |node: usize| match node {
        0 => root_estimate,
        other => subtree.nodes[other - 1].estimated_bytes,
    };
    let take = |node: usize, remaining_nodes: &mut usize, remaining_bytes: &mut usize| {
        if *remaining_nodes == 0 {
            return false;
        }
        let bytes = estimate(node);
        if bytes > *remaining_bytes {
            return false;
        }
        *remaining_nodes -= 1;
        *remaining_bytes -= bytes;
        true
    };
    let mut order: Vec<usize> = Vec::new();
    let mut parents: Vec<usize> = Vec::new();
    if !take(0, remaining_nodes, remaining_bytes) {
        return Admission { order, parents };
    }
    order.push(0);
    parents.push(NO_PARENT);
    // `(node, next child, position in `order`)`.
    let mut stack: Vec<(usize, usize, usize)> = vec![(0, 0, 0)];
    while let Some(&(node, next, at)) = stack.last() {
        let children = &subtree.children[node];
        if next == children.len() {
            stack.pop();
            continue;
        }
        stack.last_mut().expect("a non-empty admission stack").1 += 1;
        let child = children[next];
        if !take(child, remaining_nodes, remaining_bytes) {
            // The oracle's `break`: this parent stops taking children, and its
            // own ancestors carry on.
            stack.pop();
            continue;
        }
        order.push(child);
        parents.push(at);
        stack.push((child, 0, order.len() - 1));
    }
    Admission { order, parents }
}

// ===== pass 3: output payload, for admitted nodes only =====

/// One node the budget admitted, and where its DTO comes from.
struct EmittedNode {
    source: usize,
    page_id: [u8; 16],
    block_id: [u8; 16],
    payload: NodePayload,
    /// Index in `emitted` of this node's parent, or [`NO_PARENT`].
    parent: usize,
    dto: Option<BlockDto>,
}

enum NodePayload {
    /// A ROOT: the selection already built and validated this DTO.
    Ready(BlockDto),
    /// A descendant: the stored facts its OUTPUT payload read must be
    /// validated against, carried here so the topology can be dropped.
    Wanted(WantedFacts),
}

/// One admitted descendant's stored facts, as the shared payload reader wants
/// them.
struct WantedFacts {
    result_id: String,
    estimated_bytes: usize,
    tag_count: usize,
    property_count: usize,
}

/// Read the OUTPUT payload of the admitted DESCENDANTS, per source, through the
/// shared payload reader.
///
/// Distinct block ids only: two occurrences of one repeated root emit the same
/// rows twice by design, but reading their identical payload twice would be
/// waste, and de-duplicating a READ cannot move an admission or an omission
/// count. Nothing here reads payload for a node the budget rejected.
fn read_output_payload(
    sources: &mut [ExportSubtreeSource<'_>],
    emitted: &mut [EmittedNode],
) -> Result<(), ResultReadError> {
    let mut wanted: Vec<Vec<WantedPayload>> = (0..sources.len()).map(|_| Vec::new()).collect();
    let mut seen: HashSet<(usize, [u8; 16])> = HashSet::new();
    for node in emitted.iter() {
        let NodePayload::Wanted(facts) = &node.payload else {
            continue;
        };
        if !seen.insert((node.source, node.block_id)) {
            continue;
        }
        wanted[node.source].push(WantedPayload {
            block_id: node.block_id,
            page_id: node.page_id,
            result_id: facts.result_id.clone(),
            estimated_bytes: facts.estimated_bytes,
            tag_count: facts.tag_count,
            property_count: facts.property_count,
        });
    }
    let mut built: HashMap<(usize, [u8; 16]), BlockDto> = HashMap::new();
    for (at, rows) in wanted.iter().enumerate() {
        if rows.is_empty() {
            continue;
        }
        let snapshot = &mut *sources[at].snapshot;
        read_admitted_payload(
            snapshot,
            rows,
            PayloadChannel::ExportOutput,
            |row: &WantedPayload| PayloadFacts {
                block_id: row.block_id,
                page_id: row.page_id,
                result_id: &row.result_id,
                estimated_bytes: row.estimated_bytes,
                tag_count: row.tag_count,
                property_count: row.property_count,
            },
            |index, dto| {
                built.insert((at, rows[index].block_id), dto);
            },
        )?;
    }
    for node in emitted.iter_mut() {
        let dto = match &node.payload {
            NodePayload::Ready(_) => {
                let NodePayload::Ready(dto) =
                    std::mem::replace(&mut node.payload, NodePayload::Ready(BlockDto::default()))
                else {
                    unreachable!("the payload was just observed to be ready")
                };
                dto
            }
            NodePayload::Wanted(_) => built
                .get(&(node.source, node.block_id))
                .ok_or_else(|| {
                    ResultReadError::Corrupt(
                        "an admitted descendant has no payload row".to_string(),
                    )
                })?
                .clone(),
        };
        node.dto = Some(dto);
    }
    Ok(())
}

/// One admitted descendant's stored facts, as the shared payload reader wants
/// them.
struct WantedPayload {
    block_id: [u8; 16],
    page_id: [u8; 16],
    result_id: String,
    estimated_bytes: usize,
    tag_count: usize,
    property_count: usize,
}

// ===== pass 4: assembly =====

/// One root occurrence's outcome, in input order.
struct RootOutcome {
    page: String,
    kind: PageKind,
    /// Where this occurrence's root node landed in `emitted`, when it fit.
    root: Option<usize>,
    omitted_nodes: usize,
}

struct QueryOutcome {
    key: String,
    total: usize,
    roots: Vec<RootOutcome>,
}

/// Build each admitted root's DTO tree and group it, in the oracle's own order.
///
/// Emission order is SELECTED-QUERY order, not page order, because the node and
/// byte budgets are cumulative across macros. Within a macro, consecutive roots
/// from the same displayed page share one group, exactly as
/// `emit_selected_export_queries` merges them.
fn assemble(outcomes: Vec<QueryOutcome>, mut emitted: Vec<EmittedNode>) -> Vec<QueryExportResult> {
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); emitted.len()];
    for at in 0..emitted.len() {
        let parent = emitted[at].parent;
        if parent != NO_PARENT {
            children[parent].push(at);
        }
    }
    outcomes
        .into_iter()
        .map(|query| {
            let mut groups: Vec<RefGroup> = Vec::new();
            let mut shown = 0usize;
            let mut omitted_nodes = 0usize;
            for root in query.roots {
                omitted_nodes = omitted_nodes.saturating_add(root.omitted_nodes);
                let Some(at) = root.root else {
                    continue;
                };
                let dto = build_dto(at, &mut emitted, &children);
                shown += 1;
                if let Some(group) = groups
                    .iter_mut()
                    .find(|group| group.kind == root.kind && group.page == root.page)
                {
                    group.blocks.push(dto);
                } else {
                    groups.push(RefGroup {
                        page: root.page,
                        kind: root.kind,
                        blocks: vec![dto],
                        evidence: Vec::new(),
                    });
                }
            }
            QueryExportResult {
                key: query.key,
                groups,
                shown,
                total: query.total,
                omitted_nodes,
            }
        })
        .collect()
}

/// One admitted root's DTO tree, assembled on an explicit stack so a deep valid
/// export cannot overflow the thread stack.
fn build_dto(root: usize, emitted: &mut [EmittedNode], children: &[Vec<usize>]) -> BlockDto {
    fn take(emitted: &mut [EmittedNode], at: usize) -> BlockDto {
        emitted[at]
            .dto
            .take()
            .expect("every admitted node has a payload after the output read")
    }
    let first = take(emitted, root);
    let mut stack: Vec<(usize, usize, BlockDto)> = vec![(root, 0, first)];
    loop {
        let (node, next) = {
            let frame = stack.last().expect("a non-empty assembly stack");
            (frame.0, frame.1)
        };
        if next < children[node].len() {
            stack.last_mut().expect("a non-empty assembly stack").1 += 1;
            let child = children[node][next];
            let dto = take(emitted, child);
            stack.push((child, 0, dto));
            continue;
        }
        let (_, _, dto) = stack.pop().expect("a non-empty assembly stack");
        match stack.last_mut() {
            Some(parent) => parent.2.children.push(dto),
            None => return dto,
        }
    }
}

// ===== test-only census =====

#[cfg(test)]
thread_local! {
    static BEFORE_COMPLETENESS_BATCH: std::cell::RefCell<Option<Box<dyn Fn(usize)>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_before_completeness_batch_hook(hook: Option<Box<dyn Fn(usize)>>) {
    BEFORE_COMPLETENESS_BATCH.with(|slot| *slot.borrow_mut() = hook);
}

/// The TOPOLOGY work one bounded export construction performed (test-only).
///
/// Deliberately separate from the payload counters in
/// [`crate::query::results::ResultReadCensus`]: the claim RET3 makes is that
/// topology is proportional to the requested subtrees and that OUTPUT payload
/// is proportional to the ADMITTED nodes alone, and those two are only legible
/// when they are counted apart.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExportSubtreeCensus {
    pub(crate) page_statements: usize,
    pub(crate) root_statements: usize,
    pub(crate) topology_statements: usize,
    /// Rows the topology statements RETURNED. A statement binds `LIMIT
    /// TOPOLOGY_BATCH`, so a tiny subtree on a huge page fetches at most one
    /// batch of narrow rows — never the page.
    pub(crate) topology_rows_fetched: usize,
    /// Rows the scan actually DECODED before the subtree ended. This is the
    /// requested-descendant count plus the one boundary row.
    pub(crate) topology_rows_examined: usize,
    pub(crate) boundary_statements: usize,
    pub(crate) completeness_statements: usize,
    pub(crate) completeness_parents: usize,
}

#[cfg(test)]
thread_local! {
    static CENSUS: std::cell::Cell<ExportSubtreeCensus> =
        const { std::cell::Cell::new(ExportSubtreeCensus {
            page_statements: 0,
            root_statements: 0,
            topology_statements: 0,
            topology_rows_fetched: 0,
            topology_rows_examined: 0,
            boundary_statements: 0,
            completeness_statements: 0,
            completeness_parents: 0,
        }) };
}

#[cfg(test)]
fn note(update: impl FnOnce(&mut ExportSubtreeCensus)) {
    CENSUS.with(|census| {
        let mut current = census.get();
        update(&mut current);
        census.set(current);
    });
}

#[cfg(test)]
pub(crate) fn reset_export_subtree_census() {
    CENSUS.with(|census| census.set(ExportSubtreeCensus::default()));
}

#[cfg(test)]
pub(crate) fn export_subtree_census() -> ExportSubtreeCensus {
    CENSUS.with(std::cell::Cell::get)
}
