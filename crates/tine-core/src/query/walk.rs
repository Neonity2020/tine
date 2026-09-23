//! The in-memory query walk: the SQL lowering's test-only CORRECTNESS ORACLE.
//!
//! It answers a query by scanning parsed documents through a
//! [`QueryPageSource`], with `query::eval` as its evaluator. No product route
//! reaches it. RET2 made every public Direct query SQL-only, and K2
//! (2026-09-15) compiled the walk out of the product: this module and `eval`
//! are declared `#[cfg(test)]`, so a shipped binary does not contain them.
//! `tests/public_query_executor_census.rs` pins that no production code builds
//! a walk source.
//!
//! Why it is kept: no external oracle exists for the lowering (Logseq's DB
//! version evaluates in in-memory DataScript with SQLite as a mere datom store;
//! Dataview is frozen; Bases is closed). The walk answers every query from the
//! parsed documents in about 1 ms over the 1,045-file anonymized graph, so the
//! lowering's acceptance gate is DIFFERENTIAL AGAINST THE WALK:
//! `crate::query::results_tests::the_database_result_equals_the_walk_on_every_shape_and_bound`
//! and its `_over_a_real_corpus` twin. The walk outlives the lowering by at least one
//! release of differential agreement (Martin, 2026-09-03, card
//! `PVTI_lAHOAAbLVc4BhPsyzg5VyLk`). Being test-only removes its production
//! cost, not its job; delete it only after that release.
//!
//! It reads a page's own properties with the projection's grammar
//! ([`super::page_properties`]), so a walk/SQL difference is a lowering
//! difference and never a second property parser.

use super::*;

pub fn run_query(graph: &Graph, query_src: &str) -> Vec<RefGroup> {
    run_query_bounded(graph, query_src, usize::MAX, usize::MAX).groups
}

pub fn run_query_bounded(
    graph: &Graph,
    query_src: &str,
    max_rows: usize,
    max_bytes: usize,
) -> BoundedGroups {
    run_query_bounded_over(
        &BaseOrderedOracleSource(&GraphQueryPages(graph)),
        query_src,
        max_rows,
        max_bytes,
    )
}

/// The ONE simple-query entry: source limits, parse, options, evaluation.
pub(crate) fn run_query_bounded_over(
    source: &dyn QueryPageSource,
    query_src: &str,
    max_rows: usize,
    max_bytes: usize,
) -> BoundedGroups {
    run_query_bounded_over_dialect(source, query_src, QueryDialect::Og, max_rows, max_bytes)
}

/// The shared bounded simple-query entry when the query text declares its
/// surface dialect. Export does not expose unknown-identifier suggestions, so
/// parsing needs no registry read; the evaluator owns its ordinary type read.
pub(crate) fn run_query_bounded_over_dialect(
    source: &dyn QueryPageSource,
    query_src: &str,
    dialect: QueryDialect,
    max_rows: usize,
    max_bytes: usize,
) -> BoundedGroups {
    let today = JournalDate::today();
    let (query, view) = parse_query_text(query_src, dialect, today);
    run_pred_bounded_over(source, &query, &view, today, max_rows, max_bytes)
}

/// One page as the shared query drivers see it, borrowed from the page cache.
///
/// Deliberately borrowed rather than owned. Direct Files holds an
/// `Arc<Document>` per page; an owned page shape would force it to construct
/// and clone a whole `DocBlock` forest per query per page. `roots` is a
/// borrowed slice for exactly that
/// reason, and `recency` is a callback because Direct Files answers it with a
/// filesystem `stat` that must not run for a page the query never matched.
pub(crate) struct QueryPageView<'a> {
    pub(crate) path: &'a str,
    pub(crate) name: &'a str,
    pub(crate) kind: PageKind,
    pub(crate) pre_block: Option<&'a str>,
    pub(crate) roots: &'a [DocBlock],
    /// The page's journal ordinal, from the filename Direct Files parsed at
    /// inventory time.
    pub(crate) journal: Option<i64>,
    /// The page's on-disk format. The property atomizer parses a value with the
    /// page's own inline grammar (§6.2 E4), so an `Outline.ORG` page's values
    /// are read as Org by the walk exactly as by the projection.
    pub(crate) format: crate::query::atom::AtomFormat,
    pub(crate) recency: &'a dyn Fn() -> i64,
}

/// The ONE page-source abstraction the shared simple/advanced/export query
/// drivers evaluate over (I-12, D-4). A backend implements this; it does not
/// re-implement the driver.
///
/// `visit` returning [`std::ops::ControlFlow::Break`] stops the walk, which is
/// what lets export hydration stop as soon as every wanted root is found.
pub(crate) trait QueryPageSource {
    fn for_each_page(&self, visit: &mut dyn FnMut(QueryPageView<'_>) -> std::ops::ControlFlow<()>);

    /// Lend the backend's complete page set as ONE borrowed slice, for the one
    /// consumer that must retain block references ACROSS pages: export
    /// hydration resolves selected roots in query order, not page order, so it
    /// cannot emit while streaming. Streaming callers use
    /// [`QueryPageSource::for_each_page`] and allocate nothing extra.
    fn with_hydration_pages(&self, run: &mut dyn FnMut(&[ExportHydrationPage<'_>]));

    /// The graph config the property atomizer reads (§5.8 M21). Supplied by the
    /// backend so the walk never re-reads `config.edn` per query.
    fn parse_config(&self) -> crate::config::ParseConfig;

    /// ONE coherent registry snapshot for the whole query (§6.2): the walk's
    /// coercion, the TQL diagnostics and `query_registry` all read the same
    /// `Arc`, so a query sees one generation end to end.
    fn registry(&self) -> std::sync::Arc<crate::query::registry::Registry>;

    /// Which of the §8.1 counterfactual modes this source evaluates under.
    /// Every product source is Tine; only gate 1's wrapper says otherwise.
    fn compare_mode(&self) -> atom::CompareMode {
        atom::CompareMode::Both
    }

    /// Test-only instrumentation hook: one predicate evaluation over this
    /// source is about to begin. Only Direct Files' whole-graph source counts,
    /// because the counter exists to prove the candidate planner avoided a full
    /// graph walk.
    fn note_predicate_evaluation(&self) {}
}

/// One Direct Files graph walked under one of the §8.1 counterfactual modes.
///
/// Gate 1 needs the SAME walk over the SAME pages with one decision switched
/// off, so this delegates everything except the mode. Nothing in the product
/// constructs it; [`run_query_bounded_in_mode`] is its only caller.
pub(crate) struct GraphQueryPagesInMode<'a>(
    pub(crate) GraphQueryPages<'a>,
    pub(crate) atom::CompareMode,
);

impl QueryPageSource for GraphQueryPagesInMode<'_> {
    fn for_each_page(&self, visit: &mut dyn FnMut(QueryPageView<'_>) -> std::ops::ControlFlow<()>) {
        self.0.for_each_page(visit);
    }
    fn with_hydration_pages(&self, run: &mut dyn FnMut(&[ExportHydrationPage<'_>])) {
        self.0.with_hydration_pages(run);
    }
    fn parse_config(&self) -> crate::config::ParseConfig {
        self.0.parse_config()
    }
    fn registry(&self) -> std::sync::Arc<crate::query::registry::Registry> {
        self.0.registry()
    }
    fn compare_mode(&self) -> atom::CompareMode {
        self.1
    }
}

/// [`run_query_bounded`] under one §8.1 mode. Gate 1's entry point: the walk is
/// identical, only the atomizer's split, the atom identity and the coercion
/// change, so a difference between two modes attributes itself.
pub(crate) fn run_query_bounded_in_mode(
    graph: &Graph,
    query_src: &str,
    mode: atom::CompareMode,
    max_rows: usize,
    max_bytes: usize,
) -> BoundedGroups {
    run_query_bounded_over(
        &GraphQueryPagesInMode(GraphQueryPages(graph), mode),
        query_src,
        max_rows,
        max_bytes,
    )
}

/// Direct Files' page source: the cached `Arc<Document>` snapshot.
pub(crate) struct GraphQueryPages<'a>(pub(crate) &'a Graph);

impl QueryPageSource for GraphQueryPages<'_> {
    fn for_each_page(&self, visit: &mut dyn FnMut(QueryPageView<'_>) -> std::ops::ControlFlow<()>) {
        self.0.with_pages(|pages| {
            // The index orders pages by relative path in byte order, so this
            // enumerates them the same way (reconciler design §6). The launch
            // inventory already is; pages created this session are appended,
            // and only then does an enumeration sort.
            let by_path = |(left, _): &&(PageEntry, _), (right, _): &&(PageEntry, _)| {
                left.rel_path.as_bytes().cmp(right.rel_path.as_bytes())
            };
            let mut ordered = pages.iter().collect::<Vec<_>>();
            if !ordered.is_sorted_by(|left, right| by_path(left, right).is_le()) {
                ordered.sort_by(by_path);
            }
            for (entry, doc) in ordered {
                let recency = || page_recency_secs(entry);
                let flow = visit(QueryPageView {
                    path: &entry.rel_path,
                    name: &entry.name,
                    kind: entry.kind,
                    pre_block: doc.pre_block.as_deref(),
                    roots: &doc.roots,
                    journal: entry.date_key,
                    format: Format::from_path(std::path::Path::new(&entry.rel_path)).into(),
                    recency: &recency,
                });
                if flow.is_break() {
                    break;
                }
            }
        });
    }

    fn with_hydration_pages(&self, run: &mut dyn FnMut(&[ExportHydrationPage<'_>])) {
        self.0.with_pages(|pages| {
            let pages = pages
                .iter()
                .map(|(entry, doc)| ExportHydrationPage {
                    kind: entry.kind,
                    name: &entry.name,
                    roots: &doc.roots,
                })
                .collect::<Vec<_>>();
            run(&pages);
        });
    }

    fn parse_config(&self) -> crate::config::ParseConfig {
        self.0.config().parse_config()
    }

    fn registry(&self) -> std::sync::Arc<crate::query::registry::Registry> {
        self.0.property_registry()
    }

    fn note_predicate_evaluation(&self) {
        FULL_GRAPH_QUERY_EVALUATIONS.with(|count| count.set(count.get().saturating_add(1)));
    }
}

/// The page-anchored half of the walk (§7.1, K16): `@page` rows as PAGE rows.
///
/// A `@page` query selects pages. Page attributes and properties read the page
/// index — name, kind, journal day and the page's own `key:: value` preamble.
/// A `blocks` relation additionally walks that page's already-borrowed roots;
/// no page outside the current physical page participates. `@block` delegates
/// to [`run_pred_bounded_over`], whose block groups are the shipped shape.
///
/// **Post-resolution only** (§4.4): `query` is the BOUND tree
/// [`ResolvedQuery::query`] carries and `today` its one execution-day snapshot.
/// `run_resolved_query_result_over` is the entry that establishes both; this
/// function is also the probe evaluator explain-empty reuses per conjunct,
/// which is why it takes the pieces rather than the `ResolvedQuery` itself.
pub(crate) fn run_query_result_over(
    source: &dyn QueryPageSource,
    query: &Query,
    view: &ViewSettings,
    today: JournalDate,
    bounds: ir::Bounds,
) -> ir::QueryResult {
    let report = ir::QueryReport {
        ran: Vec::new(),
        ignored: Vec::new(),
        supported: true,
    };
    let mut result = ir::QueryResult {
        statistics: None,
        rows: ir::QueryRows::Page { pages: Vec::new() },
        diagnostics: query.diagnostics.clone(),
        report,
        total: 0,
        matched_total: (query.anchor == Anchor::Page).then_some(0),
        exceeded: false,
    };
    if query.anchor == Anchor::Block {
        let bounded = run_ordered_result_oracle(
            source,
            query,
            view,
            today,
            bounds.max_rows,
            bounds.max_bytes,
        );
        result.rows = ir::QueryRows::Block {
            groups: bounded.groups,
        };
        result.total = bounded.total;
        result.statistics = bounded.statistics;
        result.matched_total = bounded.matched_total;
        result.exceeded = bounded.exceeded;
        return result;
    }
    let answer = collect_page_rows_over(source, query, view, today, bounds);
    result.total = answer.total;
    result.matched_total = Some(answer.matched_total);
    result.exceeded = answer.exceeded;
    result.rows = ir::QueryRows::Page {
        pages: answer.pages,
    };
    result
}

/// The `@page` half of [`run_query_result_over`], as its own producer: the
/// matched page rows in the saved SQL-sort semantics, under both bounds.
///
/// It is the WALK's page loop and the ORACLE the database page read is compared
/// against (`results::read_page_results` is the production producer). It walks
/// the complete match set before ordering and charges the shared raw page
/// estimate, independently of the SQL reader it checks.
pub(crate) fn collect_page_rows_over(
    source: &dyn QueryPageSource,
    query: &Query,
    view: &ViewSettings,
    today: JournalDate,
    bounds: ir::Bounds,
) -> results::PageAnswer {
    source.note_predicate_evaluation();
    let mut answer = results::PageAnswer::default();
    if query.is_invalid() {
        return answer;
    }
    let wants_recency = view.sort.iter().any(|(field, _)| {
        matches!(
            field.as_str().to_ascii_lowercase().as_str(),
            "modified" | "updated" | "updated-at" | "date"
        )
    });
    let filter = query.evaluable_filter();
    let compiled = compiled::CompiledLeaves::for_query(&filter);
    let parse_config = source.parse_config();
    let registry = source.registry();
    let mut matches = Vec::new();
    source.for_each_page(&mut |page| {
        let page_props = page_properties(page.pre_block, page.format == atom::AtomFormat::Org);
        if !eval::page_row_matches(
            &filter,
            page.name,
            page.kind,
            page.journal,
            &page_props,
            page.roots,
            page.format,
            today,
            &compiled,
            &parse_config,
            &registry,
        ) {
            return std::ops::ControlFlow::Continue(());
        }
        let recency = if wants_recency { (page.recency)() } else { 0 };
        matches.push((
            ir::PageRow {
                path: page.path.to_string(),
                name: page.name.to_string(),
                kind: page.kind,
                journal_day: page.journal,
                properties: page_props,
            },
            recency,
        ));
        std::ops::ControlFlow::Continue(())
    });
    answer.matched_total = matches.len();
    if !view.sort.is_empty() {
        let directions = view
            .sort
            .iter()
            .map(|(_, direction)| *direction == SortDir::Asc)
            .collect::<Vec<_>>();
        let mut decorated = matches
            .into_iter()
            .enumerate()
            .map(|(base, (page, recency))| {
                let keys = view
                    .sort
                    .iter()
                    .map(|(field, _)| page_sort_decor(&page, recency, field.as_str()))
                    .collect::<Vec<_>>();
                (keys, page.path.clone(), base, page)
            })
            .collect::<Vec<_>>();
        decorated.sort_by(|left, right| {
            compare_sort_decorations(&left.0, &right.0, &directions)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        matches = decorated
            .into_iter()
            .map(|(_, _, _, page)| (page, 0))
            .collect();
    }
    let mut budget = ConstructionBudget::new(bounds.max_rows, bounds.max_bytes);
    for (page, _) in matches {
        let estimated = tine_storage::sqlite::query_page_result_estimated_bytes(
            &page.name,
            &page.path,
            page.properties
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        );
        if !budget.admit_page_estimated(estimated) {
            break;
        }
        answer.pages.push(page);
    }
    answer.total = answer.pages.len();
    answer.exceeded = budget.exceeded || answer.matched_total > answer.total;
    if let Some(sample) = view.sample {
        answer.pages.truncate(sample as usize);
    }
    answer
}

pub(crate) fn page_sort_decor(page: &ir::PageRow, recency: i64, field: &str) -> SortDecor {
    match field.to_ascii_lowercase().as_str() {
        "name" | "page" => SortDecor::Text(page.name.to_lowercase()),
        "kind" => SortDecor::Text(match page.kind {
            PageKind::Journal => "journal".into(),
            PageKind::Page => "page".into(),
        }),
        "day" | "journal-day" | "journal_day" => {
            SortDecor::Num(page.journal_day.unwrap_or(i64::MIN))
        }
        "modified" | "updated" | "updated-at" | "date" => SortDecor::Num(recency),
        _ => SortDecor::Text(lexical_property_sort_text(
            page.properties
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
            field,
            || page.name.clone(),
        )),
    }
}

/// The public page-or-block entry over a Direct Files graph. The dialect is the
/// caller's: it comes from the macro name the text was read out of (Q3).
pub fn run_query_result(
    graph: &Graph,
    query_src: &str,
    dialect: QueryDialect,
    bounds: ir::Bounds,
) -> ir::QueryResult {
    let today = JournalDate::today();
    let (query, view) = parse_query_text(query_src, dialect, today);
    run_query_result_over(&GraphQueryPages(graph), &query, &view, today, bounds)
}

/// The ONE post-resolution result driver (§4.4): evaluate the bound tree, then
/// attach the binding's support report.
///
/// The report is attached HERE, after the evaluation (and, for a caching
/// caller, after the result-cache retrieval), because it is a property of how
/// this source was bound and not of the rows — which is exactly why the rows may
/// be shared and the report may not.
pub(crate) fn run_resolved_query_result_over(
    source: &dyn QueryPageSource,
    resolved: &ResolvedQuery,
    view: &ViewSettings,
    bounds: ir::Bounds,
) -> ir::QueryResult {
    let mut result =
        run_query_result_over(source, resolved.query(), view, resolved.today(), bounds);
    result.report = resolved.report().clone();
    result
}

/// The ONE simple-query evaluator, reached through [`QueryPageSource`]. Nothing
/// else owns a copy of the budget, the page loop,
/// the OG top-level-root filter, the sample cap or the recency axis.
pub(crate) fn run_pred_bounded_over(
    source: &dyn QueryPageSource,
    query: &Query,
    view: &ViewSettings,
    // §4.4: the ONE execution-day snapshot, taken by `resolve_for_execution`
    // (or by the one text entry above) and never re-read from the clock here —
    // a rollover between two halves of one answer is not a thing that can
    // happen.
    today: JournalDate,
    max_rows: usize,
    max_bytes: usize,
) -> BoundedGroups {
    let pre = collect_pred_bounded_over(
        source,
        query,
        today,
        max_rows,
        max_bytes,
        ConstructionProfile::from_view(view),
    );
    apply_view(pre, view)
}

/// Test-only page-order adapter. Replays borrowed pages in semantic base order
/// before constructing DTOs, so the independent oracle still counts only
/// admitted DTO construction. Production ordering belongs entirely to SQL.
pub(crate) struct BaseOrderedOracleSource<'a>(&'a dyn QueryPageSource);

impl QueryPageSource for BaseOrderedOracleSource<'_> {
    fn for_each_page(&self, visit: &mut dyn FnMut(QueryPageView<'_>) -> std::ops::ControlFlow<()>) {
        let mut order = Vec::new();
        self.0.for_each_page(&mut |page| {
            order.push((page.name.to_owned(), page.kind, order.len()));
            std::ops::ControlFlow::Continue(())
        });
        order.sort_by(|a, b| compare_result_pages(&a.0, a.1, &b.0, b.1));
        for (_, _, wanted) in order {
            let mut at = 0;
            let mut stop = false;
            self.0.for_each_page(&mut |page| {
                let current = at;
                at += 1;
                if current != wanted {
                    return std::ops::ControlFlow::Continue(());
                }
                stop = visit(page).is_break();
                std::ops::ControlFlow::Break(())
            });
            if stop {
                break;
            }
        }
    }
    fn with_hydration_pages(&self, run: &mut dyn FnMut(&[ExportHydrationPage<'_>])) {
        self.0.with_hydration_pages(run);
    }
    fn parse_config(&self) -> crate::config::ParseConfig {
        self.0.parse_config()
    }
    fn registry(&self) -> std::sync::Arc<registry::Registry> {
        self.0.registry()
    }
    fn compare_mode(&self) -> atom::CompareMode {
        self.0.compare_mode()
    }
    fn note_predicate_evaluation(&self) {
        self.0.note_predicate_evaluation();
    }
}

pub(crate) fn run_ordered_result_oracle(
    source: &dyn QueryPageSource,
    query: &Query,
    view: &ViewSettings,
    today: JournalDate,
    max_rows: usize,
    max_bytes: usize,
) -> BoundedGroups {
    // Independent ordering/admission oracle; never a production candidate sort.
    let pre = collect_pred_bounded_over(
        source,
        query,
        today,
        usize::MAX,
        usize::MAX,
        ConstructionProfile {
            sample_admission_cap: None,
            want_recency: QueryOpts::from_view(view).uses_recency(),
        },
    );
    let matched_total = pre.total;
    let ordered = apply_view(pre, view);
    let mut budget = ConstructionBudget::new(max_rows, max_bytes);
    let mut groups = Vec::new();
    for mut group in ordered.groups {
        group.blocks.retain(|block| {
            budget.admit_estimated(&group.page, crate::model::block_dto_estimated_bytes(block))
        });
        if !group.blocks.is_empty() {
            groups.push(group);
        }
    }
    BoundedGroups {
        groups,
        total: budget.total,
        exceeded: budget.exceeded,
        matched_total: (!query.is_invalid()).then_some(matched_total),
        statistics: None,
    }
}

/// The construction half of [`run_pred_bounded_over`]: the matched rows in the
/// SOURCE's traversal order, under the result bounds and the profile's two
/// construction inputs. No view directive has looked at these rows.
pub(crate) fn collect_pred_bounded_over(
    source: &dyn QueryPageSource,
    query: &Query,
    today: JournalDate,
    max_rows: usize,
    max_bytes: usize,
    profile: ConstructionProfile,
) -> PreViewGroups {
    source.note_predicate_evaluation();
    // An invalid query (an unknown head, a syntax refusal, a depth/size refusal)
    // returns zero results plus its diagnostics — never a truncated answer
    // (§3.5).
    if query.is_invalid() {
        return PreViewGroups::default();
    }
    let filter = block_anchored_filter(query);
    let compiled = compiled::CompiledLeaves::for_query(&filter);
    let parse_config = source.parse_config();
    let registry = source.registry();
    let mode = source.compare_mode();
    let mut budget = ConstructionBudget::new(max_rows, max_bytes);
    // An unsorted `(sample N)` semantically needs only the first N matches in
    // deterministic traversal order. Do not construct or classify the rest as
    // an over-budget failure. Sorted samples still require global ranking and
    // therefore retain the ordinary construction ceiling.
    let sample_admission_cap = profile.sample_admission_cap;
    // A recency sort (`(sort-by modified …)`) needs each result page's position on
    // a single time axis: journal pages by the day they represent, other pages by
    // file mtime. Only computed when such a sort is active (else we skip the stat).
    let want_recency = profile.want_recency;
    let mut groups: Vec<RefGroup> = Vec::new();
    let mut recency_by_page: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    source.for_each_page(&mut |page| {
        let page_props = page_properties(page.pre_block, page.format == atom::AtomFormat::Org);
        let ctx = EvalCtx {
            journal: page.journal,
            is_journal: page.kind == PageKind::Journal,
            page_name: page.name,
            page_props: &page_props,
            page_roots: page.roots,
            today,
            compiled: &compiled,
            format: page.format,
            config: &parse_config,
            registry: &registry,
            mode,
        };
        let mut matched: Vec<BlockDto> = Vec::new();
        let mut path = Vec::new();
        let mut path_refs = PathRefCounts::new();
        let track_path_refs = eval::uses_path_refs(&filter);
        collect_og_query_roots(
            page.roots,
            &mut path,
            &mut path_refs,
            track_path_refs,
            false,
            &mut |block, _, ancestor_refs| {
                eval::eval_block(&filter, block, ancestor_refs, &ctx).then_some(())
            },
            &mut |block, _, ()| {
                if sample_admission_cap.is_some_and(|cap| budget.rows >= cap) {
                    return None;
                }
                if budget.closed() {
                    budget.deny_match();
                    return None;
                }
                if !budget.admit_estimated(page.name, shallow_dto_estimated_bytes(block, &[])) {
                    return None;
                }
                Some(result_dto(block))
            },
            &mut matched,
        );
        if !matched.is_empty() {
            if want_recency {
                recency_by_page.insert(page.name.to_owned(), (page.recency)());
            }
            groups.push(RefGroup {
                page: page.name.to_owned(),
                kind: page.kind,
                blocks: matched,
                evidence: Vec::new(),
            });
        }
        std::ops::ControlFlow::Continue(())
    });

    PreViewGroups {
        matched_total: None,
        ordered: false,
        statistics: None,
        groups,
        recency_by_page,
        total: budget.total,
        exceeded: budget.exceeded,
    }
}

thread_local! {
    static FULL_GRAPH_QUERY_EVALUATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(crate) fn reset_full_graph_query_evaluations() {
    FULL_GRAPH_QUERY_EVALUATIONS.with(|count| count.set(0));
}

pub(crate) fn full_graph_query_evaluations() -> u64 {
    FULL_GRAPH_QUERY_EVALUATIONS.with(std::cell::Cell::get)
}

/// Run an advanced `[:find … :where …]` / `{:query … :inputs …}` query by mapping
/// the common clause subset (task / between / page-ref / property / page-property
/// / priority + and/or/not) onto the ONE IR `Filter` the OG DSL and TQL also
/// lower to — the matching leaves already exist. Unrecognized clauses (custom
/// rules, `[?e ?a ?v]` joins, `:view`/`:result-transform`) are listed in
/// `ignored` and skipped, never guessed (a wrong result is worse than
/// "unsupported").
pub fn run_advanced_query(
    graph: &Graph,
    query_src: &str,
    current_page: Option<&str>,
) -> AdvancedResult {
    run_advanced_query_bounded(graph, query_src, current_page, usize::MAX, usize::MAX).0
}

pub fn run_advanced_query_bounded(
    graph: &Graph,
    query_src: &str,
    current_page: Option<&str>,
    max_rows: usize,
    max_bytes: usize,
) -> (AdvancedResult, bool, usize) {
    run_advanced_query_bounded_over(
        &BaseOrderedOracleSource(&GraphQueryPages(graph)),
        query_src,
        current_page,
        max_rows,
        max_bytes,
    )
}

/// The ONE advanced-query evaluator: the resolve above, the `ran`/`ignored`
/// report and delegation to the shared simple-query driver all live here, so an
/// advanced query has one answer (I-12, I-19).
pub(crate) fn run_advanced_query_bounded_over(
    source: &dyn QueryPageSource,
    query_src: &str,
    current_page: Option<&str>,
    max_rows: usize,
    max_bytes: usize,
) -> (AdvancedResult, bool, usize) {
    let (query, today, ran, ignored) = match resolve_advanced_source(query_src, current_page) {
        ResolvedAdvanced::Refused(result) => return (result, false, 0),
        ResolvedAdvanced::Executable {
            query,
            today,
            ran,
            ignored,
        } => (query, today, ran, ignored),
    };
    let bounded = run_pred_bounded_over(
        source,
        &query,
        &ViewSettings::default(),
        today,
        max_rows,
        max_bytes,
    );
    (
        AdvancedResult {
            groups: bounded.groups,
            ran,
            ignored,
            supported: true,
        },
        bounded.exceeded,
        bounded.total,
    )
}

/// A page's position on the recency axis, in Unix seconds: a journal page by the
/// midnight of the day it represents (stable — independent of when it was last
/// edited); any other page by its file's last-modified time. `i64::MIN` when a
/// non-journal page can't be stat'd (so it sorts oldest).
pub(crate) fn page_recency_secs(entry: &PageEntry) -> i64 {
    page_recency_secs_for(entry.date_key, &entry.path)
}

/// The Direct Files **document** row source (§6.2): the registry's iterator when
/// the projection is not ready. Walks pages (preamble properties, owner = page)
/// and blocks (owner = block) out of the cached `Arc<Document>` snapshot, next
/// to [`property_facets_bounded`] — which is NOT a source, because it aggregates
/// owner identity away and owner identity is what gives cardinality and the
/// distinct-owner counts.
pub fn property_owner_rows(
    graph: &Graph,
) -> (
    Vec<registry::OwnerRow>,
    std::collections::HashMap<String, registry::PageMeta>,
) {
    let mut rows = Vec::new();
    let mut pages = std::collections::HashMap::new();
    graph.with_pages(|entries| {
        for (entry, doc) in entries {
            let page_id = entry.rel_path.clone();
            let format = Format::from_path(std::path::Path::new(&entry.rel_path));
            pages.insert(
                page_id.clone(),
                registry::PageMeta {
                    format: format.into(),
                    name: entry.name.clone(),
                },
            );
            for (ordinal, (key, value)) in
                page_properties(doc.pre_block.as_deref(), format == Format::Org)
                    .into_iter()
                    .enumerate()
            {
                rows.push(registry::OwnerRow {
                    owner_type: registry::OwnerType::Page,
                    owner_id: format!("p:{page_id}"),
                    page_id: page_id.clone(),
                    source_name: key.clone(),
                    normalized_name: property_key_norm(&key),
                    ordinal: ordinal as u32,
                    value,
                });
            }
            walk(&doc.roots, &mut |block| {
                for (ordinal, (key, value)) in block.properties().into_iter().enumerate() {
                    rows.push(registry::OwnerRow {
                        owner_type: registry::OwnerType::Block,
                        owner_id: format!("b:{page_id}#{}", block.uuid),
                        page_id: page_id.clone(),
                        source_name: key.clone(),
                        normalized_name: property_key_norm(&key),
                        ordinal: ordinal as u32,
                        value,
                    });
                }
            });
        }
    });
    (rows, pages)
}

/// `children` and `refs` for the document tree, as
/// [`crate::query::path_refs::dfs_path_refs`] wants them. The refs are
/// `BlockProjection.refs_norm` -- the same per-block input the projection
/// producers feed the closure (§5.8 G1), so the walk and the two backends
/// cannot drift.
pub(crate) fn doc_children(block: &DocBlock) -> &[DocBlock] {
    &block.children
}

pub(crate) fn doc_refs(block: &DocBlock) -> &[String] {
    &block.projection().refs_norm
}

/// Walk all blocks while maintaining the normalized union of ancestor refs.
/// This mirrors OG's materialized `:block/path-refs` without adding a second
/// persistent index or turning deep outlines into an O(nodes * depth) scan.
///
/// The traversal and the multiset are
/// [`crate::query::path_refs::dfs_path_refs`]'s, the same ones
/// `path_refs_closure` runs to produce `block_path_refs` rows (§5.8).
pub(crate) fn walk_path_refs<'a>(
    blocks: &'a [DocBlock],
    refs: &mut PathRefCounts,
    track_refs: bool,
    f: &mut impl FnMut(&'a DocBlock, &PathRefCounts),
) {
    crate::query::path_refs::dfs_path_refs(blocks, &doc_children, &doc_refs, refs, track_refs, f);
}

/// The OG result-presentation walk, over the one path-refs traversal.
///
/// `path` and the matched-ancestor flag are the visitor's own stacks, pushed in
/// pre-order and popped in post-order, so this keeps the previous shape exactly:
/// a block is classified against the path and the ancestor refs that do NOT yet
/// include its own, and is dropped from the output when its immediate parent
/// matched (`tree/filter-top-level-blocks`).
pub(crate) fn collect_og_query_roots<'a, M, T>(
    blocks: &'a [DocBlock],
    path: &mut Vec<&'a DocBlock>,
    path_refs: &mut PathRefCounts,
    track_path_refs: bool,
    parent_matched: bool,
    classify: &mut impl FnMut(&'a DocBlock, &[&'a DocBlock], &PathRefCounts) -> Option<M>,
    materialize: &mut impl FnMut(&'a DocBlock, &[&'a DocBlock], M) -> Option<T>,
    out: &mut Vec<T>,
) {
    struct OgRoots<'a, 'v, M, T, C, Z> {
        path: &'v mut Vec<&'a DocBlock>,
        matched: Vec<bool>,
        classify: &'v mut C,
        materialize: &'v mut Z,
        out: &'v mut Vec<T>,
        marker: std::marker::PhantomData<M>,
    }

    impl<'a, M, T, C, Z> crate::query::path_refs::PathRefVisitor<'a, DocBlock>
        for OgRoots<'a, '_, M, T, C, Z>
    where
        C: FnMut(&'a DocBlock, &[&'a DocBlock], &PathRefCounts) -> Option<M>,
        Z: FnMut(&'a DocBlock, &[&'a DocBlock], M) -> Option<T>,
    {
        fn enter(&mut self, block: &'a DocBlock, ancestor_refs: &PathRefCounts) {
            let classification = (self.classify)(block, self.path, ancestor_refs);
            let matched = classification.is_some();
            if !self.matched.last().copied().unwrap_or(false) {
                if let Some(classification) = classification {
                    if let Some(item) = (self.materialize)(block, self.path, classification) {
                        self.out.push(item);
                    }
                }
            }
            self.path.push(block);
            self.matched.push(matched);
        }

        fn leave(&mut self, _block: &'a DocBlock) {
            self.matched.pop();
            self.path.pop();
        }
    }

    let mut visitor = OgRoots {
        path,
        matched: vec![parent_matched],
        classify,
        materialize,
        out,
        marker: std::marker::PhantomData,
    };
    crate::query::path_refs::dfs_path_refs(
        blocks,
        &doc_children,
        &doc_refs,
        path_refs,
        track_path_refs,
        &mut visitor,
    );
}
