use super::*;
use crate::query::ir::{Source, ViewSettings};
use crate::query::registry::Registry;

/// No content leaf: the empty shared parse is what the walk would build for
/// a filter carrying none, so the two engines still read the same map.
static NO_COMPILED_LEAVES: std::sync::LazyLock<CompiledLeaves> =
    std::sync::LazyLock::new(|| CompiledLeaves::for_query(&Filter::True));

fn inputs<'a>(registry: &'a Registry) -> LoweringInputs<'a> {
    LoweringInputs {
        today: JournalDate::from_ordinal(20260905),
        registry,
        cutoff: None,
        compiled: &NO_COMPILED_LEAVES,
        fts_ready: true,
        result_set_rule: RESULT_SET_RULE,
        relation_rule: RELATION_RULE,
    }
}

/// The lowering of one filter, with the shared Match parse the walk would
/// build for it — never a second parse (I-12).
fn lower_with(filter: Filter, anchor: Anchor, fts_ready: bool) -> SqlQuery {
    let registry = Registry::none().clone();
    let compiled = CompiledLeaves::for_query(&filter);
    let query = Query::new(anchor, filter, Source::Builder);
    let inputs = LoweringInputs {
        compiled: &compiled,
        fts_ready,
        ..inputs(&registry)
    };
    lower_query(&query, &inputs)
}

fn lower(filter: Filter, anchor: Anchor) -> SqlQuery {
    lower_with(filter, anchor, true)
}

fn og(source: &str) -> (Query, ViewSettings) {
    crate::query::parse_query_source(source, JournalDate::from_ordinal(20260905))
}

fn tql(source: &str) -> (Query, ViewSettings) {
    crate::query::parse_query_text(
        source,
        crate::query::QueryDialect::Tql,
        JournalDate::from_ordinal(20260905),
    )
}

/// §5.1's first rule and the false positive it exists to prevent: two
/// property conjuncts are TWO subqueries, each carrying its own key AND its
/// own atom test — never four independent probes whose results are joined
/// after the fact.
#[test]
fn two_property_conjuncts_lower_to_two_undecomposed_subqueries() {
    let (query, _) = og("(and (property status open) (property priority done))");
    let statement = lower(query.evaluable_filter(), Anchor::Block);
    // Two conjuncts, each ONE subquery. §5.3's result-set rule reads the
    // match set back by name under [`ResultSetRule::MatchSetCteMaterialized`],
    // so the filter is compiled ONCE — the correlated spelling compiled the
    // same tree a second time in the parent's row scope and this count was 4.
    assert_eq!(
        statement.sql.matches("FROM property_atoms").count(),
        2,
        "one atom subquery per quantifier, per row scope: {}",
        statement.sql
    );
    // Neither subquery is decomposed: each carries its key AND its atom test.
    for fragment in statement.sql.split("FROM property_atoms").skip(1) {
        let head = fragment.split(')').next().unwrap_or_default();
        assert!(
            head.contains(".normalized_name = ") && head.contains(".atom_key = "),
            "a property subquery carries key and value together: {head}"
        );
    }
    // Each subquery carries its key and its value in ONE `WHERE`.
    for key in ["status", "priority"] {
        let key_at = statement
            .params
            .iter()
            .position(|value| *value == PhysicalQueryValue::Text(key.into()))
            .unwrap_or_else(|| panic!("{key} is bound: {:?}", statement.params));
        let _ = key_at;
    }
    for value in ["open", "done"] {
        assert!(
            statement
                .params
                .contains(&PhysicalQueryValue::Text(value.into())),
            "{value} is bound: {:?}",
            statement.params
        );
    }
}

/// §5.1's J1: a subquery over a nullable owner column carries `IS NOT NULL`,
/// because `x NOT IN (SELECT c)` is NULL — not false — when any `c` is NULL.
#[test]
fn a_children_subquery_never_selects_a_null_owner() {
    let filter = Filter::rel(
        Rel::Children,
        Quant::None,
        Filter::attr(Attr::Task, CmpOp::Eq, Value::text("TODO")),
    );
    let statement = lower(filter, Anchor::Block);
    assert!(
        statement.sql.contains("c1.parent_block_id IS NOT NULL"),
        "{}",
        statement.sql
    );
    assert!(
        statement.sql.contains("NOT IN (SELECT"),
        "{}",
        statement.sql
    );
}

/// §5.1: a generic `Every` is the complement of its VIOLATION predicate, so
/// an empty relation is true (K2).
#[test]
fn a_generic_every_is_the_complement_of_its_violation_predicate() {
    let filter = Filter::rel(
        Rel::Children,
        Quant::Every,
        Filter::attr(Attr::Task, CmpOp::Eq, Value::text("DONE")),
    );
    let statement = lower(filter, Anchor::Block);
    assert!(
        statement.sql.contains("b.block_id NOT IN (SELECT"),
        "{}",
        statement.sql
    );
    assert!(statement.sql.contains("(NOT "), "{}", statement.sql);
}

/// §3.3: a property `Every` is presence AND no violator — a generic `Every`
/// alone would answer true for an owner that never spells the key.
#[test]
fn a_property_every_carries_its_presence_conjunct() {
    let filter = Filter::rel(
        Rel::Props,
        Quant::Every,
        Filter::and(vec![
            Filter::attr(Attr::Key, CmpOp::Eq, Value::text("type")),
            Filter::attr(Attr::Value, CmpOp::Eq, Value::text("book")),
        ]),
    );
    let statement = lower(filter, Anchor::Block);
    assert!(
        statement.sql.contains("FROM properties"),
        "presence probe: {}",
        statement.sql
    );
    assert!(
        statement.sql.contains("FROM property_atoms"),
        "violation probe: {}",
        statement.sql
    );
}

/// §5.2: every comparison on a nullable column carries its null guard, so
/// the expression is two-valued and `NOT` over it is classical. The guard is
/// written `IS NOT NULL AND` rather than `COALESCE(…, 0)` because only the
/// former lets SQLite seek the index §5.7 requires — same function, one
/// spelling that satisfies both rules.
#[test]
fn comparisons_on_nullable_columns_carry_a_two_valued_null_guard() {
    let filter = Filter::attr(Attr::Scheduled, CmpOp::Le, Value::date("today"));
    let statement = lower(filter, Anchor::Block);
    assert!(
        statement
            .sql
            .contains("(bp1.scheduled_day IS NOT NULL AND bp1.scheduled_day <= ?1)"),
        "{}",
        statement.sql
    );
    assert!(!statement.sql.contains("COALESCE"), "{}", statement.sql);
}

/// §5.5: every literal is a bound parameter. The statement text may not
/// contain a value the caller supplied.
#[test]
fn values_are_bound_and_never_interpolated() {
    let (query, _) = og("(and (property type \"O'Brien; DROP TABLE blocks--\") [[Some Page]])");
    let statement = lower(query.evaluable_filter(), Anchor::Block);
    assert!(
        !statement.sql.contains("O'Brien"),
        "the hostile literal is bound, not spelled: {}",
        statement.sql
    );
    assert!(
        statement.params.iter().any(
            |value| matches!(value, PhysicalQueryValue::Text(text) if text.contains("drop table"))
        ),
        "{:?}",
        statement.params
    );
}

/// §3.4 + §5.7: a leaf whose operator does not apply to its key's effective
/// type is FALSE, and a false conjunct folds the STATEMENT rather than being
/// handed to SQLite as `… AND 0`.
///
/// This is not tidiness. Measured on the anonymized corpus, `prop('score') >
/// 5` where `score` is a text key produced `… AND 0` inside the atom
/// subquery, and SQLite planned it as `SCAN a2 USING COVERING INDEX
/// property_atoms_page_idx` — a full covering scan for a predicate that
/// selects nothing, which is §5.7's own failure mode. There is no index that
/// fixes it; the subquery has to not be asked.
///
/// The second half is the trap folding sets: parameters are POSITIONAL, so a
/// fragment that bound a value before folding away would leave a hole and
/// the driver would refuse the statement.
#[test]
fn an_unsatisfiable_leaf_folds_the_statement_and_takes_its_parameters_with_it() {
    // `Registry::none()` gives every key the default effective type Text, so
    // `> 5` is the operator/type mismatch §3.4 answers false for.
    let registry = Registry::none().clone();
    let (query, _) = tql("prop('score') > 5");
    let statement = lower_query(&query, &inputs(&registry));
    assert!(
        statement.matches_nothing,
        "an unsatisfiable comparison makes the whole statement empty: {}",
        statement.sql
    );
    assert!(
        statement.sql.ends_with(" WHERE 0"),
        "no subquery is left to plan: {}",
        statement.sql
    );
    assert!(
        !statement.positively_bounded,
        "a statement that reads no row asks §5.7 for no index"
    );
    assert!(
        statement.params.is_empty(),
        "the folded-away fragments took their values with them: {:?}",
        statement.params
    );

    // The same fold under `None`, where the presence probe HAS bound its key
    // and owner type before the atom test folds: the quantifier becomes the
    // constant true and every one of those placeholders leaves with it.
    let (query, _) = tql("none(prop('k'), value > 5)");
    let statement = lower_query(&query, &inputs(&registry));
    assert!(
        !statement.sql.contains('?'),
        "no orphan placeholder survives: {}",
        statement.sql
    );
    assert!(statement.params.is_empty(), "{:?}", statement.params);
    assert!(
        !statement.matches_nothing,
        "`none` over an unsatisfiable test is true, not false: {}",
        statement.sql
    );

    // And the renumbering itself: what is left is `?1..?n` with no gap, in
    // order of appearance, matching the parameter list one for one.
    let (query, _) = og("(and (property status open) [[Some Page]])");
    let statement = lower(query.evaluable_filter(), Anchor::Block);
    let mut seen: Vec<usize> = Vec::new();
    let mut rest = statement.sql.as_str();
    while let Some(at) = rest.find('?') {
        rest = &rest[at + 1..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        seen.push(rest[..end].parse().expect("digits"));
        rest = &rest[end..];
    }
    assert_eq!(
        seen,
        (1..=statement.params.len()).collect::<Vec<_>>(),
        "placeholders are dense, ordered, and exactly as many as the values: {}",
        statement.sql
    );
}

/// §5.6: `LIMIT cutoff + 1`, so the caller can tell "exactly the cutoff"
/// from "more than the cutoff".
#[test]
fn a_cutoff_lowers_to_limit_cutoff_plus_one() {
    let registry = Registry::none().clone();
    let query = Query::new(Anchor::Block, Filter::True, Source::Builder);
    let mut inputs = inputs(&registry);
    inputs.cutoff = Some(50);
    let statement = lower_query(&query, &inputs);
    assert!(statement.sql.contains("LIMIT ?1"), "{}", statement.sql);
    assert_eq!(statement.params, vec![PhysicalQueryValue::Integer(51)]);
}

/// §5.7: the boundedness table is exhaustive, and negation never bounds.
#[test]
fn boundedness_follows_the_exhaustive_table() {
    let registry = Registry::none().clone();
    let inputs = inputs(&registry);
    let bounded_shapes = [
        "[[Project]]",
        "(task TODO)",
        "(priority A)",
        "(property type book)",
        "(and (task TODO) \"loose text\")",
    ];
    for source in bounded_shapes {
        let (query, _) = og(source);
        assert!(
            positively_bounded(&query.evaluable_filter(), Anchor::Block, &inputs),
            "{source} must be positively bounded"
        );
    }
    let unbounded_shapes = [
        "\"loose text\"",
        "(not (task TODO))",
        "(or (task TODO) \"loose text\")",
    ];
    for source in unbounded_shapes {
        let (query, _) = og(source);
        assert!(
            !positively_bounded(&query.evaluable_filter(), Anchor::Block, &inputs),
            "{source} must NOT be positively bounded"
        );
    }

    // A child predicate that is a property of the CHILD bounds the anchor;
    // one that reads the ANCHOR's ancestor context does not, at any depth.
    // The plan that forces this is measured in
    // `a_nested_refs_child_predicate_cannot_bound_its_anchor`.
    let child_task = Filter::rel(
        Rel::Children,
        Quant::Any,
        Filter::attr(Attr::Task, CmpOp::Eq, Value::text("DONE")),
    );
    assert!(positively_bounded(&child_task, Anchor::Block, &inputs));
    let child_refs = Filter::rel(Rel::Children, Quant::Any, Filter::page_ref("project"));
    assert!(!positively_bounded(&child_refs, Anchor::Block, &inputs));
    let deep_refs = Filter::rel(
        Rel::Children,
        Quant::Any,
        Filter::and(vec![
            Filter::attr(Attr::Task, CmpOp::Eq, Value::text("DONE")),
            Filter::rel(Rel::Children, Quant::Any, Filter::page_ref("project")),
        ]),
    );
    assert!(!positively_bounded(&deep_refs, Anchor::Block, &inputs));
    // At the ANCHOR the same leaf is still one keyed probe and still bounds.
    assert!(positively_bounded(
        &Filter::page_ref("project"),
        Anchor::Block,
        &inputs
    ));
}

/// §5.7: absence lowers to a complement and enumerates it, so `is null` and
/// a negated leaf bound nothing even though their positive twins do.
#[test]
fn absence_and_negation_bound_nothing() {
    let registry = Registry::none().clone();
    let inputs = inputs(&registry);
    let absent = Filter::attr(Attr::Scheduled, CmpOp::IsNotSet, Value::None);
    assert!(!positively_bounded(&absent, Anchor::Block, &inputs));
    let present = Filter::attr(Attr::Scheduled, CmpOp::IsSet, Value::None);
    assert!(positively_bounded(&present, Anchor::Block, &inputs));
    assert!(!positively_bounded(
        &Filter::not(present),
        Anchor::Block,
        &inputs
    ));
}

// -----------------------------------------------------------------------
// §5.10
// -----------------------------------------------------------------------

fn content_match(text: &str) -> Filter {
    Filter::attr(Attr::Content, CmpOp::Match, Value::text(text))
}

fn match_sql(text: &str, fts_ready: bool) -> SqlQuery {
    lower_with(content_match(text), Anchor::Block, fts_ready)
}

/// The one rule that decides this packet: the exact `instr` predicates are
/// the final SQL conditions on EVERY path, so the trigram probe only
/// narrows which rows are asked. A bound that replaced them would answer
/// "different results, faster".
#[test]
fn the_candidate_bound_narrows_and_never_replaces_the_exact_predicate() {
    let bounded = match_sql("alpha", true);
    assert!(
        bounded.sql.contains("search_substring_fts")
            && bounded.sql.contains("MATCH ?")
            && bounded.sql.contains("instr(b.query_visible_folded, ?"),
        "{}",
        bounded.sql
    );
    assert!(bounded.positively_bounded);
    assert_eq!(bounded.content_plans, vec![ContentPlan::Fts]);
    // The needle is the FTS phrase literal, and the exact needle is the
    // parser's own folded term — two different bound values.
    assert!(bounded
        .params
        .contains(&PhysicalQueryValue::Text("\"alpha\"".to_string())));
    assert!(bounded
        .params
        .contains(&PhysicalQueryValue::Text("alpha".to_string())));
}

/// §5.10's acceptance corollary, at the compiler: `foobar` is reachable by
/// `foo` and `oob` THROUGH the index and by `oo` WITHOUT it — the two-scalar
/// term yields no trigram, and answering it is not optional.
#[test]
fn a_two_scalar_term_is_unbounded_rather_than_unanswered() {
    for needle in ["foo", "oob"] {
        let statement = match_sql(needle, true);
        assert!(statement.sql.contains("search_substring_fts"), "{needle}");
        assert_eq!(statement.content_plans, vec![ContentPlan::Fts]);
    }
    let short = match_sql("oo", true);
    assert!(
        !short.sql.contains("search_substring_fts")
            && short.sql.contains("instr(b.query_visible_folded, ?"),
        "{}",
        short.sql
    );
    assert!(!short.positively_bounded);
    assert_eq!(short.content_plans, vec![ContentPlan::ShortUnindexable]);
}

/// One unbounded OR arm makes the LEAF unbounded, and that is not a defect
/// to work around: the anchor is reached once per arm.
#[test]
fn one_unbounded_or_arm_unbounds_the_whole_leaf() {
    let mixed = match_sql("oo OR alpha", true);
    assert!(!mixed.positively_bounded);
    assert_eq!(mixed.content_plans, vec![ContentPlan::ShortUnindexable]);
    // The bounded arm still gets its bound — bounds are per-arm. One
    // occurrence, because §5.3's CTE spelling compiles the filter once.
    assert_eq!(mixed.sql.matches("search_substring_fts").count(), 1);
    assert!(match_sql("beta OR alpha", true).positively_bounded);
}

/// §5.10: emptiness comes from the parsed `Term`, not from SQLite.
/// `instr(text, '')` is 1 and `length` stops at NUL, so only the Rust side
/// can reproduce `group_matches`' `!text.is_empty() && contains`.
#[test]
fn an_empty_term_is_false_and_an_empty_negative_term_is_true() {
    // A whitespace-only quoted phrase is NOT empty: it is a real needle the
    // exact column can hold and the collapsed FTS text cannot.
    let spaces = match_sql("\"   \"", true);
    assert!(!spaces.matches_nothing && !spaces.sql.contains("search_substring_fts"));
    assert!(spaces
        .params
        .contains(&PhysicalQueryValue::Text("   ".to_string())));
    assert_eq!(spaces.content_plans, vec![ContentPlan::ShortUnindexable]);

    // The empty term itself, against `group_matches`' own answer. It is
    // constructed rather than parsed because `Matcher::parse` drops an
    // empty TOKEN — but the rule has to hold for the parsed value it does
    // produce, whatever a future fold makes empty, and SQLite's `instr`
    // answers the opposite of it.
    let registry = Registry::none().clone();
    let inputs = inputs(&registry);
    let mut compiler = Compiler {
        inputs: &inputs,
        params: Vec::new(),
        next_alias: 0,
        probe: false,
        regexes: Vec::new(),
        needs_child_map: false,
    };
    for negated in [false, true] {
        let term = Term {
            text: String::new(),
            negated,
            quoted: false,
        };
        let group = vec![term.clone()];
        assert_eq!(
            compiler.match_term(&term, "b"),
            if negated { "1" } else { "0" },
            "an empty {}term",
            if negated { "negative " } else { "" }
        );
        // And the walk agrees, on text that contains everything and nothing.
        for text in ["", "anything at all"] {
            let matcher = Matcher::Boolean(vec![group.clone()]);
            assert_eq!(
                matcher.matches(text, text),
                negated,
                "the walk's answer for an empty term over {text:?}"
            );
        }
    }
    assert!(compiler.params.is_empty(), "a constant binds nothing");
}

/// Exclusion-only input and an invalid regex are FALSE LEAVES (§5.10), so
/// `not` over them is classically true — the existing truth rule, not a
/// whole-query diagnostic.
#[test]
fn exclusion_only_and_invalid_regex_lower_to_a_false_leaf_under_not_too() {
    for (source, plans) in [
        ("-alpha", &[][..]),
        ("   ", &[][..]),
        // An invalid `/pattern/` is still a REGEX leaf for §5.10's plan
        // classes — it just needs no engine to answer false.
        ("/[unclosed/", &[ContentPlan::Regex][..]),
    ] {
        let statement = match_sql(source, true);
        assert!(statement.matches_nothing, "{source}: {}", statement.sql);
        assert_eq!(statement.content_plans, plans, "{source}");
        let negated = lower_with(Filter::not(content_match(source)), Anchor::Block, true);
        assert!(!negated.matches_nothing, "{source}: {}", negated.sql);
        assert!(!negated.positively_bounded, "{source}");
    }
}

/// The `fts-building` class (§5.10): the SAME exact predicates on the ready
/// block columns, with no bound anywhere — never empty results, an error, a
/// new walk route, or a rebuild request (I-13).
#[test]
fn a_building_index_omits_the_bounds_and_keeps_the_exact_predicates() {
    let building = match_sql("alpha beta OR gamma", false);
    assert!(
        !building.sql.contains("search_substring_fts")
            && !building.sql.contains("search_fts_owners")
            && !building.sql.contains("MATCH"),
        "{}",
        building.sql
    );
    // Three terms, compiled once (§5.3's CTE spelling).
    assert_eq!(building.sql.matches("instr(").count(), 3);
    assert!(!building.positively_bounded);
    assert_eq!(building.content_plans, vec![ContentPlan::FtsBuilding]);
    // Same predicates, same bound needles, as the ready lowering: only the
    // candidate bound differs.
    let ready = match_sql("alpha beta OR gamma", true);
    for term in ["alpha", "beta", "gamma"] {
        let value = PhysicalQueryValue::Text(term.to_string());
        assert!(building.params.contains(&value) && ready.params.contains(&value));
    }
}

/// A negative term never bounds anything: it says the text does NOT contain
/// it, so using it as a candidate would select exactly the rejected rows.
#[test]
fn a_negative_term_is_negated_and_never_becomes_the_candidate() {
    let statement = match_sql("oo -draft", true);
    assert!(
        statement
            .sql
            .contains("(NOT (instr(b.query_visible_folded, ?"),
        "{}",
        statement.sql
    );
    assert!(
        !statement.sql.contains("search_substring_fts"),
        "the only three-scalar run is the NEGATIVE term: {}",
        statement.sql
    );
    assert!(!statement
        .params
        .contains(&PhysicalQueryValue::Text("\"draft\"".to_string())));
}

/// §5.10's needle rule, at the unit that owns it.
#[test]
fn the_candidate_needle_is_the_first_three_scalar_whitespace_free_run() {
    let needle = |source: &str| {
        let Matcher::Boolean(groups) = Matcher::parse(source) else {
            panic!("{source} is not a boolean query");
        };
        fts_candidate_needle(&groups[0]).map(str::to_owned)
    };
    // Not the whole phrase: the run survives the producers' whitespace
    // collapsing, the leading/repeated spaces need not.
    assert_eq!(needle("\"  alpha  beta\""), Some("alpha".to_string()));
    assert_eq!(needle("\"a b cde\""), Some("cde".to_string()));
    // Terms are scanned in order, and a term with no long-enough run is
    // skipped rather than ending the scan.
    assert_eq!(needle("ab cd efgh"), Some("efgh".to_string()));
    assert_eq!(needle("ab cd"), None);
    assert_eq!(needle("-longenough ab"), None);
    // Three SCALARS, not three bytes.
    assert_eq!(needle("日本語"), Some("日本語".to_string()));
    assert_eq!(needle("日本"), None);
    // A NUL-bearing term supplies no bound at all.
    let nul = vec![Term {
        text: "abc\0def".to_string(),
        negated: false,
        quoted: false,
    }];
    assert_eq!(fts_candidate_needle(&nul), None);
}

/// The needle crosses the boundary as ONE FTS5 phrase literal, so `-`,
/// `*`, `(` and a bare `OR` inside it are text and not query syntax.
#[test]
fn the_fts_needle_is_quoted_as_one_literal_with_doubled_quotes() {
    assert_eq!(fts_phrase_literal("say\"hi"), "\"say\"\"hi\"");
    assert_eq!(fts_phrase_literal("a OR b"), "\"a OR b\"");
    assert_eq!(fts_phrase_literal("-x*"), "\"-x*\"");
}

/// §4.3.2's regex predicate, at the compiler. A VALID pattern in EITHER
/// spelling reaches a statement as `tine_query_regex(<bound id>, <exact
/// visible text>)`; an INVALID one still needs no engine and is a false
/// leaf. The pattern itself never appears in the SQL.
#[test]
fn a_valid_regex_lowers_to_the_bound_predicate_over_the_exact_visible_text() {
    for source in ["content regexp '[a-z]+'", "content match '/[a-z]+/'"] {
        let (query, _) = tql(source);
        let statement = lower_with(query.evaluable_filter(), Anchor::Block, true);
        assert!(
            statement.sql.contains(
                "tine_query_regex(?1, (SELECT bt1.query_visible FROM block_text bt1 \
                     WHERE bt1.block_id = b.block_id))"
            ),
            "{source}: {}",
            statement.sql
        );
        // The EXACT column, never the folded one a case-insensitive
        // comparison would use.
        assert!(
            !statement.sql.contains("query_visible_folded"),
            "{source}: {}",
            statement.sql
        );
        // The pattern is a table row keyed by a bound ID, not SQL text.
        assert!(
            !statement.sql.contains("[a-z]"),
            "{source}: {}",
            statement.sql
        );
        assert_eq!(statement.params, vec![PhysicalQueryValue::Integer(1)]);
        assert_eq!(statement.regexes.bindings.len(), 1, "{source}");
        assert_eq!(
            statement.content_plans,
            vec![ContentPlan::Regex],
            "{source}"
        );
        // Explicitly unindexed (§4.3.2): a regex never bounds the anchor.
        assert!(!statement.positively_bounded, "{source}");
        assert!(!statement.matches_nothing, "{source}");
    }
    let invalid = lower_with(
        Filter::attr(Attr::Content, CmpOp::Regex, Value::text("[unclosed")),
        Anchor::Block,
        true,
    );
    assert!(invalid.matches_nothing);
    assert_eq!(invalid.content_plans, vec![ContentPlan::Regex]);
    assert!(
        invalid.regexes.bindings.is_empty(),
        "a false leaf binds no program"
    );
}

/// The compiled-regex table is de-duplicated by EFFECTIVE PATTERN, so the same
/// pattern written twice is one ID — and, decisively, §5.3's correlated
/// spelling compiling the whole filter a SECOND time in the parent's row
/// scope reuses the first pass's IDs instead of growing a parallel table
/// whose second half nothing would install.
#[test]
fn manager_regex_syntaxes_with_equal_source_keep_distinct_programs() {
    let statement = lower_with(
        Filter::and(vec![
            Filter::attr(Attr::Content, CmpOp::Match, Value::text("/needle/")),
            Filter::attr(Attr::Content, CmpOp::Regex, Value::text("/needle/")),
        ]),
        Anchor::Block,
        true,
    );
    assert_eq!(statement.regexes.bindings.len(), 2);
    let predicate = statement.regexes.predicate();
    let hits = (1..=2)
        .map(|id| predicate(id, "needle").unwrap())
        .collect::<Vec<_>>();
    assert_eq!(hits.iter().filter(|hit| **hit).count(), 1);
}

#[test]
fn one_pattern_is_one_binding_however_many_times_it_is_compiled() {
    let registry = Registry::none().clone();
    let filter = Filter::and(vec![
        Filter::attr(Attr::Content, CmpOp::Regex, Value::text("alpha")),
        Filter::attr(Attr::Content, CmpOp::Regex, Value::text("beta")),
        Filter::attr(Attr::Content, CmpOp::Regex, Value::text("alpha")),
    ]);
    let compiled = CompiledLeaves::for_query(&filter);
    let query = Query::new(Anchor::Block, filter, Source::Builder);
    for rule in [
        ResultSetRule::MatchSetCteMaterialized,
        ResultSetRule::CorrelatedProbe,
    ] {
        let inputs = LoweringInputs {
            compiled: &compiled,
            result_set_rule: rule,
            ..inputs(&registry)
        };
        let statement = lower_query(&query, &inputs);
        assert_eq!(
            statement.regexes.bindings.len(),
            2,
            "{rule:?}: two distinct patterns: {}",
            statement.sql
        );
        // Every ID the statement names is one the program binds.
        let predicate = statement.regexes.predicate();
        for id in 1..=2u64 {
            assert!(predicate(id, "alpha beta").is_ok(), "{rule:?} id {id}");
        }
        assert!(
            predicate(3, "alpha").is_err(),
            "{rule:?}: an unbound id fails"
        );
    }
}

/// The program is the WALK's compiled value, and its equality is the
/// patterns it binds — not a pointer, which would make two identical
/// lowerings compare unequal, and not a `Debug` line carrying user text.
#[test]
fn the_regex_program_compares_by_pattern_and_never_prints_one() {
    let (query, _) = tql("content regexp 'secret-\\d+'");
    let filter = query.evaluable_filter();
    let first = lower_with(filter.clone(), Anchor::Block, true);
    let second = lower_with(filter, Anchor::Block, true);
    assert_eq!(first, second, "two lowerings of one filter are equal");
    assert_eq!(
        format!("{:?}", first.regexes),
        "QueryRegexProgram { bindings: 1 }"
    );
    assert!(
        !format!("{first:?}").contains("secret-"),
        "the pattern text never reaches a Debug line"
    );
    // And the program answers with the SAME program the walk runs.
    let predicate = first.regexes.predicate();
    assert_eq!(predicate(1, "secret-42").unwrap(), true);
    assert_eq!(predicate(1, "secret-").unwrap(), false);
}

/// §3.2's nested-`refs` context, at the compiler: the set a nested row is
/// tested against is its OWN refs, the ANCHOR's ancestors' and the page —
/// three unioned stored facts, never `block_path_refs(<nested row>)` and
/// never a subtraction.
#[test]
fn refs_inside_a_children_predicate_reads_the_anchors_ancestor_context() {
    let registry = Registry::none().clone();
    let query = Query::new(
        Anchor::Block,
        Filter::rel(Rel::Children, Quant::Any, Filter::page_ref("Project")),
        Source::Builder,
    );
    let statement = lower_query(&query, &inputs(&registry));
    // The nested row contributes ONLY its own refs.
    assert!(
        statement.sql.contains("block_own_refs or2")
            && statement.sql.contains("or2.block_id = c1.block_id"),
        "{}",
        statement.sql
    );
    // The ancestor context is the ANCHOR's parent's closure, and it is the
    // anchor `b` that is named there — never the child `c1`.
    assert!(
        statement.sql.contains(
            "(b.parent_block_id IS NOT NULL AND b.parent_block_id IN \
                 (SELECT ar3.block_id FROM block_path_refs ar3 \
                 WHERE (ar3.block_id = b.parent_block_id AND ar3.normalized_name = ?2)))"
        ),
        "{}",
        statement.sql
    );
    // The page is named separately, because a ROOT anchor has no parent row
    // to carry it.
    assert!(
        statement.sql.contains(
            "b.page_id IN (SELECT pr4.page_id FROM pages pr4 \
             WHERE (pr4.page_id = b.page_id AND pr4.name_key <> '' AND pr4.name_key = ?3))"
        ),
        "{}",
        statement.sql
    );
    // Nothing reads the nested row's own materialized closure.
    assert!(
        !statement
            .sql
            .contains("block_path_refs ar3 WHERE (ar3.block_id = c1"),
        "{}",
        statement.sql
    );
    // One bound value per arm, and the SAME page-identity fold on all three
    // — never a literal spelled into the statement.
    assert_eq!(
        statement.params,
        vec![PhysicalQueryValue::Text("project".to_string()); 3]
    );

    // Two levels down the context is STILL the anchor's: `c2` is the
    // grandchild, and the ancestor and page arms both name `b`.
    let deep = Query::new(
        Anchor::Block,
        Filter::rel(
            Rel::Children,
            Quant::Any,
            Filter::rel(Rel::Children, Quant::Any, Filter::page_ref("Project")),
        ),
        Source::Builder,
    );
    let statement = lower_query(&deep, &inputs(&registry));
    assert!(
        statement.sql.contains("or3.block_id = c2.block_id")
            && statement.sql.contains("ar4.block_id = b.parent_block_id")
            && statement.sql.contains("pr5.page_id = b.page_id"),
        "the grandchild's context is the anchor's, not its parent's: {}",
        statement.sql
    );

    // The anchor's OWN `refs` leaf is unchanged: one probe of the one table
    // §5.8 materializes for exactly this question.
    let top = Query::new(Anchor::Block, Filter::page_ref("Project"), Source::Builder);
    let statement = lower_query(&top, &inputs(&registry));
    assert!(
        statement.sql.contains("FROM block_path_refs r1"),
        "{}",
        statement.sql
    );
    assert!(
        !statement.sql.contains("block_own_refs"),
        "the anchor needs no union: {}",
        statement.sql
    );
}

/// The three quantifiers of a nested `refs` leaf, which is where an
/// existence built from a UNION could quietly stop matching `quantify`:
/// `Any` is that existence, `None` its negation, and a general `Every` is
/// "no element violates" — while the single-name `Every` is membership, the
/// walk's own `single_ref_name` fast path.
#[test]
fn a_nested_refs_quantifier_is_the_walks_own_three_answers() {
    let registry = Registry::none().clone();
    let nested = |quant: Quant, pred: Filter| {
        let query = Query::new(
            Anchor::Block,
            Filter::rel(
                Rel::Children,
                Quant::Any,
                Filter::rel(Rel::Refs, quant, pred),
            ),
            Source::Builder,
        );
        lower_query(&query, &inputs(&registry)).sql
    };
    let name = || Filter::attr(Attr::Name, CmpOp::Eq, Value::text("Project"));
    // Single name: `Every` IS `Any`, so the two lower identically.
    assert_eq!(nested(Quant::Any, name()), nested(Quant::Every, name()));
    // `None` is that same existence, negated.
    assert!(nested(Quant::None, name()).contains("(NOT ("));
    // A general `Every` negates the ELEMENT predicate instead.
    let general = Filter::or(vec![
        name(),
        Filter::attr(Attr::Name, CmpOp::Eq, Value::text("Other")),
    ]);
    let every = nested(Quant::Every, general.clone());
    assert!(
        every.contains("(NOT (or") || every.contains("NOT ("),
        "{every}"
    );
    assert_ne!(every, nested(Quant::Any, general));
}

/// A page `blocks` quantifier inherits the block predicate's real bound:
/// a selective task probe can drive page ids, while a broad enumeration
/// and either complement quantifier cannot claim an indexed anchor.
#[test]
fn page_blocks_bounds_only_from_a_selective_positive_block_predicate() {
    let registry = Registry::none().clone();
    let task = || Filter::attr(Attr::Task, CmpOp::Eq, Value::text("TODO"));
    let lower = |quant, pred| {
        let query = Query::new(
            Anchor::Page,
            Filter::rel(Rel::Blocks, quant, pred),
            Source::Builder,
        );
        lower_query(&query, &inputs(&registry))
    };

    assert!(lower(Quant::Any, task()).positively_bounded);
    assert!(!lower(Quant::Any, Filter::True).positively_bounded);
    assert!(!lower(Quant::None, task()).positively_bounded);
    assert!(!lower(Quant::Every, task()).positively_bounded);
}

#[test]
fn a_prefix_range_is_half_open_and_survives_the_last_scalar_value() {
    assert_eq!(prefix_upper_bound("proj/"), Some("proj0".to_string()));
    assert_eq!(prefix_upper_bound("ab"), Some("ac".to_string()));
    // U+D7FF is the last scalar before the surrogate block; the next valid
    // scalar is U+E000, not U+D800.
    assert_eq!(prefix_upper_bound("\u{d7ff}"), Some("\u{e000}".to_string()));
    assert_eq!(prefix_upper_bound(&format!("{}", char::MAX)), None);
}

#[test]
fn like_escape_protects_the_three_pattern_characters() {
    assert_eq!(like_escape("100%_a\\b"), "100\\%\\_a\\\\b");
}
