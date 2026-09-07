//! SPEC §7.1's two computations that are neither parse, print nor run: the
//! §4.1 precedence merge of block properties into the view, and §Q14/N19's
//! explanation of an empty result.
//!
//! Both live here rather than in the Tauri command layer because they are
//! query-language behaviour with unit tests, not IPC plumbing (D-4: one
//! producer). The commands call them and do nothing else.

use crate::query::ir::{AggFn, Field, SortDir, ViewKind, ViewSettings};

/// The property namespace §7.6 persists the view under.
const VIEW_PROPERTY_PREFIX: &str = "tine.";

/// **Which columns a query block SHOWS — the one resolver** (P5A).
///
/// Visible columns and a typed sheet schema are two different questions that
/// used to share one property. `tine.columns::` owns the ordered list of
/// query-visible field names; `tine.fields::` keeps the typed schema
/// (`name=type`), and a value containing `=` is therefore never a column list.
///
/// Every consumer — `merge_block_property_view` here, and the static publisher
/// in `publish.rs` — calls THIS function rather than re-deriving the
/// precedence, so a published page cannot disagree with the app about which
/// columns a query shows (D-4/D-12: one producer of one answer). The
/// TypeScript half is `src/editor/queryViewProperties.ts`, and the two are
/// pinned to one another by `tests/fixtures/query-columns/resolution.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryColumns {
    /// A property named these columns, in this order. Token spelling is
    /// retained; a renderer may deduplicate identical field ids without
    /// rewriting the source.
    Named(Vec<Field>),
    /// `tine.columns::` is PRESENT and says nothing readable — empty, or a list
    /// one token invalidated. That is an explicit statement, not a gap: there
    /// is no legacy fallback and no DSL fallback behind it, so clearing the
    /// property cannot resurrect an older list.
    Cleared,
    /// No property answers the question at all. Whatever the query text itself
    /// carried stands.
    Unset,
}

/// The column-list grammar, applied to a WHOLE property value (P5A):
/// trim, split on `;`, trim each token, discard empty segments. One token
/// containing `=`, NUL, CR or LF invalidates the ENTIRE list rather than just
/// itself — a half-read column list is worse evidence than none, and `=`
/// anywhere means the value is a schema or a mixed value, never columns.
///
/// `None` is "this value is not a column list"; `Some(vec![])` is "this value
/// is a column list with nothing in it".
///
/// Deliberately no per-name length cap: these are property bytes an outside
/// editor may have authored, and refusing a long but well-formed name would
/// drop a column the author can see in their own file. Session/UI caps are a
/// different boundary.
fn column_list(value: &str) -> Option<Vec<Field>> {
    let mut out = Vec::new();
    for token in value.trim().split(';') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if token.contains(|c| matches!(c, '=' | '\0' | '\r' | '\n')) {
            return None;
        }
        out.push(Field::new(token));
    }
    Some(out)
}

/// SPEC §7.6 + P5A precedence for the visible columns of a query block, read
/// from its normalized block properties (first occurrence of a key wins, as
/// every other reader here does).
///
///  1. `tine.columns` PRESENT → its own answer, and nothing behind it.
///  2. `tine.columns` ABSENT → `tine.fields` is read as a LEGACY column list,
///     but only when every nonempty token passes the same grammar and at least
///     one exists. This is compatibility for notes authored before the split,
///     not private-state migration (D-1 is not engaged).
///  3. Neither → `Unset`.
///
/// Nothing here writes: opening, parsing or rebuilding a graph never rewrites a
/// note to move a legacy list (I-4).
pub fn resolve_query_columns(block_properties: &[(String, String)]) -> QueryColumns {
    let raw = |name: &str| -> Option<&str> {
        let wanted = format!("{VIEW_PROPERTY_PREFIX}{name}");
        block_properties
            .iter()
            .find(|(key, _)| crate::doc::property_key_norm(key) == wanted)
            .map(|(_, value)| value.as_str())
    };
    if let Some(value) = raw("columns") {
        return match column_list(value) {
            Some(columns) if !columns.is_empty() => QueryColumns::Named(columns),
            _ => QueryColumns::Cleared,
        };
    }
    match raw("fields").and_then(column_list) {
        Some(columns) if !columns.is_empty() => QueryColumns::Named(columns),
        _ => QueryColumns::Unset,
    }
}

/// SPEC §4.1 precedence (N17, M14): **for each view field**, a `tine.*` block
/// property wins; the DSL directive the parser lifted is read only when the
/// property is absent. The merge happens in exactly one place, this function,
/// so a caller cannot get the order wrong.
///
/// A property whose value does not parse is not a reason to drop the field: the
/// DSL's value stands, because a half-read property is worse evidence than the
/// text the author wrote. Nothing here rewrites the query.
pub fn merge_block_property_view(
    parsed: &ViewSettings,
    block_properties: &[(String, String)],
) -> ViewSettings {
    let property = |name: &str| -> Option<&str> {
        let wanted = format!("{VIEW_PROPERTY_PREFIX}{name}");
        block_properties
            .iter()
            .find(|(key, _)| crate::doc::property_key_norm(key) == wanted)
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
    };

    let mut merged = parsed.clone();
    if let Some(view) = property("view").and_then(parse_view_kind) {
        merged.view = Some(view);
    }
    if let Some(sort) = property("sort").map(parse_sort) {
        if !sort.is_empty() {
            merged.sort = sort;
        }
    }
    if let Some(group_by) = property("group-by") {
        merged.group_by = Some(Field::new(group_by));
    }
    match resolve_query_columns(block_properties) {
        QueryColumns::Named(columns) => merged.columns = columns,
        // An explicit "no columns" clears whatever the text asked for; `Unset`
        // leaves the author's own directive standing.
        QueryColumns::Cleared => merged.columns.clear(),
        QueryColumns::Unset => {}
    }
    if let Some(aggregates) = property("col-aggregates").map(parse_col_aggregates) {
        if !aggregates.is_empty() {
            merged.aggregates = aggregates;
        }
    }
    if let Some(sample) = property("sample").and_then(|value| value.parse::<u32>().ok()) {
        merged.sample = Some(sample);
    }
    merged
}

fn parse_view_kind(value: &str) -> Option<ViewKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "search" => Some(ViewKind::Search),
        "list" => Some(ViewKind::List),
        "table" => Some(ViewKind::Table),
        "board" => Some(ViewKind::Board),
        _ => None,
    }
}

/// `tine.sort:: <field> <asc|desc>[; …]` (§7.6). A segment with no direction
/// sorts ascending, which is what the Display popover writes.
fn parse_sort(value: &str) -> Vec<(Field, SortDir)> {
    value
        .split(';')
        .filter_map(|segment| {
            let segment = segment.trim();
            if segment.is_empty() {
                return None;
            }
            let (name, direction) = match segment.rsplit_once(char::is_whitespace) {
                Some((name, "desc")) => (name.trim(), SortDir::Desc),
                Some((name, "asc")) => (name.trim(), SortDir::Asc),
                _ => (segment, SortDir::Asc),
            };
            (!name.is_empty()).then(|| (Field::new(name), direction))
        })
        .collect()
}

/// `tine.col-aggregates:: <field>=<fn>[; …]` (§7.6). A bare `count` segment
/// with no `=` is the whole-result count, `(Field(""), Count)` (X3).
fn parse_col_aggregates(value: &str) -> Vec<(Field, AggFn)> {
    value
        .split(';')
        .filter_map(|segment| {
            let segment = segment.trim();
            if segment.is_empty() {
                return None;
            }
            match segment.split_once('=') {
                Some((field, function)) => {
                    Some((Field::new(field.trim()), parse_agg_fn(function.trim())?))
                }
                None => {
                    (segment.eq_ignore_ascii_case("count")).then(|| (Field::new(""), AggFn::Count))
                }
            }
        })
        .collect()
}

fn parse_agg_fn(value: &str) -> Option<AggFn> {
    match value.to_ascii_lowercase().as_str() {
        "count" => Some(AggFn::Count),
        "sum" => Some(AggFn::Sum),
        "avg" => Some(AggFn::Avg),
        _ => None,
    }
}

/// One line of `query_explain_empty` (SPEC §7.1, Q14/N19).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmptyExplanation {
    /// The conjunct, printed in TQL so the answer reads as query text.
    pub conjunct: String,
    /// Anchor rows matching this conjunct **alone**.
    pub alone: usize,
    /// Anchor rows matching every OTHER conjunct — absent when the root is not
    /// an `And`, because then there is no "other".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub without: Option<usize>,
}

/// Why a query returned nothing: for a root `And` after normalization, one
/// entry per top-level conjunct with the rows it matches alone and the rows the
/// rest match without it; for any other root, one entry for the whole query
/// (N19). Every count is the ANCHOR row count of the same evaluator the query
/// itself ran through — nothing here is a second engine.
///
/// **It decomposes the RESOLVED tree** (§4.4). Explain-empty is the one place a
/// query is taken apart and re-run piece by piece, so a decomposition of the
/// unbound advanced placeholder would explain a query the user never ran. When
/// the binding failed there is nothing honest to count: the rows are empty and
/// the caller gets the diagnostics and the support report instead of a table of
/// zeroes that reads like a result.
pub(crate) fn explain_empty(
    source: &dyn crate::query::QueryPageSource,
    resolved: &crate::query::ResolvedQuery,
    view: &ViewSettings,
    bounds: crate::query::ir::Bounds,
) -> crate::query::ir::ExplainEmptyResult {
    use crate::query::ir::Filter;

    let query = resolved.query();
    let answer = |rows: Vec<EmptyExplanation>| crate::query::ir::ExplainEmptyResult {
        rows,
        diagnostics: query.diagnostics.clone(),
        report: resolved.report().clone(),
    };
    if !resolved.is_executable() {
        return answer(Vec::new());
    }

    let count = |filter: Filter| -> usize {
        let mut probe = query.clone();
        probe.filter = filter;
        probe.diagnostics.clear();
        crate::query::run_query_result_over(source, &probe, view, resolved.today(), bounds).total
    };
    let printed = |filter: &Filter| -> String {
        let mut probe = query.clone();
        probe.filter = filter.clone();
        crate::query::print::print_tql(&probe)
    };

    let mut evaluable = query.clone();
    evaluable.filter = query.evaluable_filter();
    answer(match evaluable.normalized().filter {
        Filter::And { items } if items.len() > 1 => items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let others = items
                    .iter()
                    .enumerate()
                    .filter(|(other, _)| *other != index)
                    .map(|(_, filter)| filter.clone())
                    .collect::<Vec<_>>();
                EmptyExplanation {
                    conjunct: printed(item),
                    alone: count(item.clone()),
                    without: Some(count(Filter::and(others))),
                }
            })
            .collect(),
        whole => vec![EmptyExplanation {
            conjunct: printed(&whole),
            alone: count(whole),
            without: None,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ir::ViewKind;

    fn properties(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn a_block_property_wins_over_the_directive_the_parser_lifted() {
        let parsed = ViewSettings {
            sort: vec![(Field::new("page"), SortDir::Asc)],
            sample: Some(3),
            ..ViewSettings::default()
        };
        let merged = merge_block_property_view(
            &parsed,
            &properties(&[("tine.sort", "created desc"), ("tine.sample", "10")]),
        );
        assert_eq!(merged.sort, vec![(Field::new("created"), SortDir::Desc)]);
        assert_eq!(merged.sample, Some(10));
    }

    #[test]
    fn a_directive_is_read_only_where_no_property_covers_it() {
        let parsed = ViewSettings {
            sort: vec![(Field::new("page"), SortDir::Asc)],
            sample: Some(3),
            ..ViewSettings::default()
        };
        let merged = merge_block_property_view(&parsed, &properties(&[("tine.sample", "10")]));
        assert_eq!(
            merged.sort,
            vec![(Field::new("page"), SortDir::Asc)],
            "the property covered `sample`, not `sort`"
        );
        assert_eq!(merged.sample, Some(10));
    }

    #[test]
    fn an_unreadable_property_leaves_the_authors_directive_standing() {
        let parsed = ViewSettings {
            sample: Some(3),
            view: Some(ViewKind::Table),
            ..ViewSettings::default()
        };
        let merged = merge_block_property_view(
            &parsed,
            &properties(&[
                ("tine.sample", "lots"),
                ("tine.view", "kanban"),
                ("tine.sort", "  "),
            ]),
        );
        assert_eq!(merged.sample, Some(3));
        assert_eq!(merged.view, Some(ViewKind::Table));
        assert!(merged.sort.is_empty());
    }

    #[test]
    fn the_view_properties_parse_the_forms_section_7_6_persists() {
        let merged = merge_block_property_view(
            &ViewSettings::default(),
            &properties(&[
                ("tine.view", "board"),
                ("tine.group-by", "status"),
                ("tine.fields", "page; status; cost"),
                ("tine.col-aggregates", "count;cost=sum"),
                ("tine.sort", "status; created desc"),
                ("tine.sample", "25"),
            ]),
        );
        assert_eq!(merged.view, Some(ViewKind::Board));
        assert_eq!(merged.group_by, Some(Field::new("status")));
        assert_eq!(
            merged.columns,
            vec![Field::new("page"), Field::new("status"), Field::new("cost")]
        );
        // X3: the bare `count` entry is the whole-result count.
        assert_eq!(
            merged.aggregates,
            vec![
                (Field::new(""), AggFn::Count),
                (Field::new("cost"), AggFn::Sum)
            ]
        );
        assert_eq!(
            merged.sort,
            vec![
                (Field::new("status"), SortDir::Asc),
                (Field::new("created"), SortDir::Desc)
            ]
        );
        assert_eq!(merged.sample, Some(25));
    }

    #[test]
    fn tine_columns_owns_the_visible_columns_and_tine_fields_keeps_the_typed_schema() {
        // The clobber this split exists to end: a block can carry BOTH a typed
        // sheet schema and a column selection, and neither erases the other.
        let merged = merge_block_property_view(
            &ViewSettings::default(),
            &properties(&[
                ("tine.columns", "page; cost"),
                ("tine.fields", "cost=number;severity=text"),
            ]),
        );
        assert_eq!(merged.columns, vec![Field::new("page"), Field::new("cost")]);
    }

    #[test]
    fn a_typed_or_mixed_tine_fields_is_schema_and_never_columns() {
        for value in ["cost=number;severity=text", "page;cost=number"] {
            let merged = merge_block_property_view(
                &ViewSettings::default(),
                &properties(&[("tine.fields", value)]),
            );
            assert!(
                merged.columns.is_empty(),
                "`=` anywhere means schema or mixed, never columns: {value:?}"
            );
        }
    }

    #[test]
    fn a_present_but_empty_or_invalid_columns_list_clears_and_never_falls_back() {
        for value in ["", "   ", "a;cost=number", "a;b\rc"] {
            let merged = merge_block_property_view(
                &ViewSettings::default(),
                &properties(&[("tine.columns", value), ("tine.fields", "a;b")]),
            );
            assert!(
                merged.columns.is_empty(),
                "a PRESENT tine.columns is an explicit statement; clearing cannot \
                 resurrect the legacy list: {value:?}"
            );
            assert_eq!(
                resolve_query_columns(&properties(&[("tine.columns", value)])),
                QueryColumns::Cleared
            );
        }
    }

    #[test]
    fn a_legacy_bare_tine_fields_list_still_names_columns_when_the_new_key_is_absent() {
        assert_eq!(
            resolve_query_columns(&properties(&[("tine.fields", "page; status; cost")])),
            QueryColumns::Named(vec![
                Field::new("page"),
                Field::new("status"),
                Field::new("cost")
            ]),
        );
        // Nothing here writes: reading a legacy list never rewrites the note
        // (I-4). This is authored-note compatibility, not a D-1 private-format
        // migration.
        assert_eq!(resolve_query_columns(&properties(&[])), QueryColumns::Unset);
    }

    #[test]
    fn duplicate_column_names_are_retained_verbatim_for_the_renderer_to_decide() {
        assert_eq!(
            resolve_query_columns(&properties(&[("tine.columns", "cost;cost;Cost")])),
            QueryColumns::Named(vec![
                Field::new("cost"),
                Field::new("cost"),
                Field::new("Cost")
            ]),
        );
    }

    #[test]
    fn a_property_outside_the_tine_namespace_is_not_a_view_setting() {
        let merged =
            merge_block_property_view(&ViewSettings::default(), &properties(&[("sample", "9")]));
        assert_eq!(merged.sample, None);
    }
}
