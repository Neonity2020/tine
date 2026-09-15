//! Query export selection: which queries an export includes, and the bounded
//! result subtrees it writes for them.

use super::*;

/// One query macro requested by Copy / Export. Query evaluation and subtree
/// hydration stay in the same native operation so a shallow result never causes
/// the WebView to fetch and retain its complete source page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueryExportSpec {
    pub key: String,
    pub query: String,
    pub advanced: bool,
    /// The surface syntax of a simple query. Legacy callers omit it and retain
    /// the original OG behavior; advanced queries ignore it and keep their
    /// existing binding path.
    #[serde(default)]
    pub simple_dialect: Option<QueryDialect>,
    /// The page this macro is written on — the §4.4 execution context, so an
    /// exported advanced query binds `?current-page` to the SAME page the
    /// rendered one did. Defaulted rather than required: the frontend caller
    /// that supplies it is P0-ts, and an absent page is the honest "no binding"
    /// value, never a guess.
    #[serde(default)]
    pub current_page: Option<String>,
}

impl QueryExportSpec {
    pub(crate) fn simple_dialect(&self) -> QueryDialect {
        self.simple_dialect.unwrap_or(QueryDialect::Og)
    }
}

/// A single query macro's bounded, hierarchy-preserving export projection.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueryExportResult {
    pub key: String,
    pub groups: Vec<RefGroup>,
    pub shown: usize,
    pub total: usize,
    pub omitted_nodes: usize,
}

/// All query macros in one export session share the same construction budget.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueryExportBatch {
    pub results: Vec<QueryExportResult>,
    /// Query macros beyond the native request cap are not evaluated. The caller
    /// renders an explicit truncation note rather than silently expanding them
    /// through an unbounded sequence of independent requests.
    pub omitted_queries: usize,
}

#[cfg(test)]
#[derive(Debug)]
struct SelectedExportRoot {
    page: String,
    kind: PageKind,
    id: String,
}

/// One query macro's selection, over whichever ROOT shape the caller keeps.
///
/// The walk-backed oracle keeps `SelectedExportRoot` (a display name, a kind
/// and a public id, which is all a source re-scan can use). RET3's database
/// export keeps a root that carries its exact physical locator instead, so
/// duplicate public ids and equal display names cannot collapse two different
/// physical subtrees. `key` and `total` mean the same thing on both.
#[derive(Debug)]
pub(crate) struct SelectedExportQueryOf<R> {
    pub(crate) key: String,
    pub(crate) total: usize,
    pub(crate) roots: Vec<R>,
}

#[cfg(test)]
type SelectedExportQuery = SelectedExportQueryOf<SelectedExportRoot>;

/// What ONE evaluated query macro contributed, before root admission.
pub(crate) struct ExportSelectionAnswer<B> {
    pub(crate) groups: Vec<ResultViewGroup<B>>,
    pub(crate) total: usize,
    pub(crate) exceeded: bool,
}

/// One page as export hydration sees it. Built by each backend's
/// [`QueryPageSource::with_hydration_pages`]; neither export entry point
/// constructs it.
#[cfg(test)]
pub(crate) struct ExportHydrationPage<'a> {
    pub(super) kind: PageKind,
    pub(super) name: &'a str,
    pub(super) roots: &'a [DocBlock],
}

#[cfg(test)]
fn hydrate_selected_export_queries(
    selected: Vec<SelectedExportQuery>,
    source: &dyn QueryPageSource,
    max_nodes: usize,
    max_bytes: usize,
) -> Vec<QueryExportResult> {
    let mut wanted_by_page: HashMap<(PageKind, String), HashSet<String>> = HashMap::new();
    for query in &selected {
        for root in &query.roots {
            wanted_by_page
                .entry((root.kind, root.page.clone()))
                .or_default()
                .insert(root.id.clone());
        }
    }

    let total_wanted = wanted_by_page.values().map(HashSet::len).sum::<usize>();
    let mut results = Vec::new();
    source.with_hydration_pages(&mut |pages| {
        let mut found: HashMap<(PageKind, String, String), &DocBlock> = HashMap::new();
        for page in pages {
            if found.len() == total_wanted {
                break;
            }
            let page_key = (page.kind, page.name.to_owned());
            let Some(wanted) = wanted_by_page.get(&page_key) else {
                continue;
            };
            let mut stack: Vec<&DocBlock> = page.roots.iter().rev().collect();
            while let Some(block) = stack.pop() {
                let property_id = block.property("id");
                let matched = if wanted.contains(block.uuid.as_str()) {
                    Some(block.uuid.as_str())
                } else {
                    property_id.as_deref().filter(|id| wanted.contains(*id))
                };
                if let Some(id) = matched {
                    found.insert((page.kind, page.name.to_owned(), id.to_string()), block);
                    if found.len() == total_wanted {
                        break;
                    }
                }
                for child in block.children.iter().rev() {
                    stack.push(child);
                }
            }
        }
        results = emit_selected_export_queries(&selected, &found, max_nodes, max_bytes);
    });
    results
}

/// Emit the bounded DTOs for the already-located roots, in SELECTED-QUERY order
/// (not page order): the node and byte budget is cumulative across macros, so
/// the emission order is part of the contract.
#[cfg(test)]
fn emit_selected_export_queries(
    selected: &[SelectedExportQuery],
    found: &HashMap<(PageKind, String, String), &DocBlock>,
    max_nodes: usize,
    max_bytes: usize,
) -> Vec<QueryExportResult> {
    let mut remaining_nodes = max_nodes.max(1);
    let mut remaining_bytes = max_bytes.max(1);
    selected
        .iter()
        .map(|query| {
            let mut groups: Vec<RefGroup> = Vec::new();
            let mut shown = 0usize;
            let mut omitted_nodes = 0usize;
            for root in &query.roots {
                let Some(block) = found.get(&(root.kind, root.page.clone(), root.id.clone()))
                else {
                    omitted_nodes = omitted_nodes.saturating_add(1);
                    continue;
                };
                let total_nodes = subtree_node_count(block);
                let before_nodes = remaining_nodes;
                let dto = block_to_bounded_dto(block, &mut remaining_nodes, &mut remaining_bytes);
                let emitted = before_nodes.saturating_sub(remaining_nodes);
                omitted_nodes = omitted_nodes.saturating_add(total_nodes.saturating_sub(emitted));
                let Some(dto) = dto else {
                    continue;
                };
                shown += 1;
                if let Some(group) = groups
                    .iter_mut()
                    .find(|group| group.kind == root.kind && group.page == root.page)
                {
                    group.blocks.push(dto);
                } else {
                    groups.push(RefGroup {
                        page: root.page.clone(),
                        kind: root.kind,
                        blocks: vec![dto],
                        evidence: Vec::new(),
                    });
                }
            }
            QueryExportResult {
                key: query.key.clone(),
                groups,
                shown,
                total: query.total,
                omitted_nodes,
            }
        })
        .collect()
}

/// **The ONE export SELECTION budget** (RET3): the macro cap, the global root
/// pool and the per-query totals, over whichever entry type the evaluation
/// produced.
///
/// Every rule here is the oracle's, transcribed once rather than twice:
///
/// * `max_queries` and `max_roots` clamp to at least 1;
/// * specs past the macro cap are NEVER evaluated (`take(query_limit)`), and
///   the caller reports the rest as `omitted_queries`;
/// * an EXCEEDED selection contributes no roots and still reports its `total`;
/// * `max_roots` is one pool shared across macros in INPUT order, not a
///   per-macro allowance.
///
/// `evaluate` may fail — the database route's selection is a read that can
/// return [`crate::query::results::ResultReadError`] — and the first failure
/// stops the whole selection. The walk-backed oracle instantiates `E` with
/// `Infallible`, so its behaviour is unchanged.
pub(crate) fn select_export_queries_over<B, R, E>(
    specs: &[QueryExportSpec],
    max_queries: usize,
    max_roots: usize,
    mut evaluate: impl FnMut(&QueryExportSpec) -> Result<ExportSelectionAnswer<B>, E>,
    mut root: impl FnMut(&str, PageKind, B) -> R,
) -> Result<(usize, Vec<SelectedExportQueryOf<R>>), E> {
    let query_limit = max_queries.max(1);
    let mut remaining_roots = max_roots.max(1);
    let mut selected = Vec::new();
    for spec in specs.iter().take(query_limit) {
        let answer = evaluate(spec)?;
        let total = answer.total;
        let mut roots = Vec::new();
        let groups = if answer.exceeded {
            Vec::new()
        } else {
            answer.groups
        };
        'query: for group in groups {
            let ResultViewGroup {
                page,
                kind,
                blocks,
                evidence: _,
            } = group;
            for block in blocks {
                if remaining_roots == 0 {
                    break 'query;
                }
                roots.push(root(&page, kind, block));
                remaining_roots -= 1;
            }
        }
        selected.push(SelectedExportQueryOf {
            key: spec.key.clone(),
            total,
            roots,
        });
    }
    Ok((query_limit, selected))
}

#[cfg(test)]
fn select_export_queries(
    specs: &[QueryExportSpec],
    max_queries: usize,
    max_roots: usize,
    mut evaluate: impl FnMut(&QueryExportSpec) -> BoundedGroups,
) -> (usize, Vec<SelectedExportQuery>) {
    select_export_queries_over(
        specs,
        max_queries,
        max_roots,
        |spec| {
            let bounded = evaluate(spec);
            Ok::<_, std::convert::Infallible>(ExportSelectionAnswer {
                groups: bounded
                    .groups
                    .into_iter()
                    .map(ResultViewGroup::from)
                    .collect(),
                total: bounded.total,
                exceeded: bounded.exceeded,
            })
        },
        |page, kind, block: BlockDto| SelectedExportRoot {
            page: page.to_owned(),
            kind,
            // The source re-scan's only handle on the block. RET3's located
            // route keeps the physical locator instead, exactly because this
            // one cannot tell two identically-named results apart.
            id: block.id,
        },
    )
    .expect("the walk-backed selection cannot fail")
}

/// Evaluate and hydrate several Copy / Export query macros under one cumulative
/// root, node, and byte budget. Only the selected block subtrees are cloned into
/// DTOs; complete PageDto values never cross IPC or accumulate in the WebView.
///
/// `max_roots` is deliberately global, not per macro. This keeps a selection
/// containing many distinct query blocks from multiplying the same advertised
/// export limit. Each relevant source document is scanned at most once and only
/// references to the requested roots are retained while the graph snapshot is
/// borrowed.
#[cfg(test)]
pub fn export_query_subtrees(
    graph: &Graph,
    specs: &[QueryExportSpec],
    max_queries: usize,
    max_roots: usize,
    max_nodes: usize,
    max_bytes: usize,
) -> QueryExportBatch {
    export_query_subtrees_over(
        &GraphQueryPages(graph),
        specs,
        max_queries,
        max_roots,
        max_nodes,
        max_bytes,
    )
}

/// The construction ceiling one exported query macro may reach while SELECTING
/// its roots, before the caller's own node/byte budget bounds hydration. One
/// definition: the two storage modes previously carried a private copy each.
pub(crate) const QUERY_EXPORT_CONSTRUCTION_ROWS: usize = 20_000;
pub(crate) const QUERY_EXPORT_CONSTRUCTION_BYTES: usize = 32 * 1024 * 1024;

/// Independent walk oracle for export selection and hydration. Production
/// adapters use export_execute::PreparedExportBatch on one SQLite snapshot.
#[cfg(test)]
fn export_query_subtrees_over(
    source: &dyn QueryPageSource,
    specs: &[QueryExportSpec],
    max_queries: usize,
    max_roots: usize,
    max_nodes: usize,
    max_bytes: usize,
) -> QueryExportBatch {
    let (query_limit, selected) = select_export_queries(specs, max_queries, max_roots, |spec| {
        if spec.advanced {
            let (result, exceeded, total) = run_advanced_query_bounded_over(
                source,
                &spec.query,
                spec.current_page.as_deref(),
                QUERY_EXPORT_CONSTRUCTION_ROWS,
                QUERY_EXPORT_CONSTRUCTION_BYTES,
            );
            BoundedGroups {
                matched_total: None,
                statistics: None,
                groups: result.groups,
                total,
                exceeded,
            }
        } else {
            run_query_bounded_over_dialect(
                source,
                &spec.query,
                spec.simple_dialect(),
                QUERY_EXPORT_CONSTRUCTION_ROWS,
                QUERY_EXPORT_CONSTRUCTION_BYTES,
            )
        }
    });
    QueryExportBatch {
        results: hydrate_selected_export_queries(selected, source, max_nodes, max_bytes),
        omitted_queries: specs.len().saturating_sub(query_limit),
    }
}
