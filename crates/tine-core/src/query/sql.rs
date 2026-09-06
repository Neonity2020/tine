//! The ONE IR → SQL lowering (SPEC §5.1–§5.7).
//!
//! **What this module is.** It turns a [`Query`] — the same value the walk
//! evaluates in [`crate::query::eval`] — into one statement plus its bound
//! parameters, to be run through `tine-storage`'s read-only projection
//! statement seam (`PhysicalProjectionQueryReader`, D-15). It is a compiler and
//! nothing else: it opens no database, holds no connection, and has no opinion
//! about which backend runs the statement.
//!
//! **Transcription (D-9, D-14).** Each function below names the upstream
//! function it transcribes, read from the upstream source rather than inferred
//! from behaviour:
//!
//! | Tine function | Upstream source |
//! |---|---|
//! | [`Compiler::filter`] | `hasura/ndc-postgres` `crates/query-engine/translation/src/translation/query/filtering.rs::translate_expression_with_joins` — the `And`/`Or`/`Not` skeleton folded over already-translated operand expressions, with an empty `In` list short-circuited to a false constant. |
//! | [`Compiler::quantified`] | `gorilla-co/odata-query` `odata_query/django/django_q.py::visit_CollectionLambda` — `Any` is the subquery, `All` is the subquery over the NEGATED predicate with the whole thing negated (its own comment: "If ALL items in the collection must match, we invert the condition and use NOT EXISTS()"). The `EXISTS`/`NOT EXISTS` pair is spelled here as §5.1's fixed `IN (subquery)` / `NOT IN (subquery)`, which is the same predicate over an owner column that is never NULL. |
//! | [`Compiler::relation_subquery`]'s `invert` flag | `prisma-engines` `query-compiler/query-builders/sql-query-builder/src/filter/visitor.rs` — its `reverse: bool` field and `invert_reverse` helper, a negation carried INTO the nested visit rather than wrapped around its result. |
//! | [`Compiler::exists_subquery`] | `hasura/ndc-postgres` `filtering.rs::translate_exists_in_collection` — one `SELECT <owner> FROM <relation> WHERE <join condition> AND <predicate>` per quantifier, built in the relation's own row scope. |
//!
//! **The three rules a plausible implementation gets wrong** (SPEC §5.1, §5.2,
//! §5.7), each of which has a guard test at the bottom of this file:
//!
//! 1. **One `IN (subquery)` per relational quantifier, never decomposed.**
//!    [`Compiler::quantified`] is the only producer of a relation predicate, and
//!    it always emits ONE subquery carrying the whole conjunction. Decomposing
//!    `props any(key='status' ∧ value='open')` and `props any(key='priority' ∧
//!    value='done')` into four independent probes is what makes a block match
//!    because *some* row satisfies each conjunct separately.
//! 2. **Nothing is NULL.** Every expression this module produces is two-valued.
//!    Subqueries over a nullable owner column (today only
//!    `blocks.parent_block_id`) carry `IS NOT NULL`, and every comparison on a
//!    nullable column is spelled `(<col> IS NOT NULL AND <cmp>)` — SQL's
//!    three-valued logic does not agree with §3.4's classical `not`, and the
//!    difference only shows up on sparse real data. §5.2 writes that guard as
//!    `COALESCE(<cmp>, 0)`; the two are the same two-valued function, and the
//!    `IS NOT NULL AND` spelling is the only one of them that is SARGABLE —
//!    measured, `COALESCE(bp.scheduled_day >= ?, 0)` turns
//!    `block_planning_scheduled_day_idx` from a range SEARCH into a covering
//!    SCAN, which is §5.7's own failure. Rules 2 and 7 both hold in this
//!    spelling and cannot both hold in the other.
//! 3. **No table scan where a positive index exists.**
//!    [`positively_bounded`] implements §5.7's table, which is **exhaustive**: a
//!    leaf/operator pair absent from it is unbounded.
//!
//! **`walk == SQL` is the contract (I-19, I-12).** Every comparison below is
//! written against the walk's own code in [`crate::query::eval`], and the
//! normalization applied to a literal is the SAME function the projection
//! producer applied to the column (`refs::page_key` for `pages.name_key` and
//! `tags.tag_key`, `refs::normalize` for `block_path_refs.normalized_name`,
//! `doc::property_key_norm` for `normalized_name`, `atom::atom_key` for
//! `atom_key`, `search_query::canonical_fold` for `query_visible_folded`) —
//! never a second normalizer that agrees by inspection.
//!
//! **What this wave declines** ([`Lowered::Unsupported`], §5.9's dispatch, NOT a
//! divergence): `content match` and `content regexp`, whose acceleration and
//! exact SQL predicate are P1-c (§5.10), and a `refs` leaf nested inside a
//! `children` relation predicate — see [`Compiler::leaf_block`] for why that one
//! cannot be lowered from `block_path_refs` without disagreeing with the walk.

use tine_storage::sqlite::PhysicalQueryValue;

// The acceptance gates. `#[path]` keeps the file beside this one so the shared
// production-source scanner sees a `*_tests.rs` sibling include and blanks it
// from every census (print sites, termination sites, the tine-storage surface).
#[cfg(test)]
#[path = "sql_gates_tests.rs"]
mod sql_gates_tests;

use crate::date::JournalDate;
use crate::doc::property_key_norm;
use crate::query::atom::atom_key;
use crate::query::eval::format_number;
use crate::query::ir::{Anchor, Attr, CmpOp, Filter, Leaf, ObservedType, Quant, Query, Rel, Value};
use crate::query::registry::Registry;
use crate::refs;
use crate::search_query::canonical_fold;

/// `owner_type` as the projection spells it (`PhysicalEntityId::sql_parts`).
const OWNER_PAGE: i64 = 0;
const OWNER_BLOCK: i64 = 1;

/// `pages.text_kind` for a journal page (`page_kind_to_sql`).
const TEXT_KIND_JOURNAL: i64 = 1;

/// One lowered statement and the values it binds.
///
/// The parameters are a positional list because the seam's signature takes one
/// (`run_projection_query(sql, &[PhysicalQueryValue])`); an interpolated
/// statement is not expressible through this type, which is how I-22 holds
/// structurally rather than by the caller's discipline.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SqlQuery {
    pub(crate) sql: String,
    pub(crate) params: Vec<PhysicalQueryValue>,
    /// §5.7: whether the root conjunction carries at least one positive leaf
    /// whose anchor bound is `yes`. The plan gate asserts `SEARCH` on the anchor
    /// alias exactly for these.
    pub(crate) positively_bounded: bool,
    /// The filter folded to the constant false, so the statement provably reads
    /// no row. It is still a statement — the caller has one path, not two — but
    /// there is no index for SQLite to choose and none to demand of it.
    pub(crate) matches_nothing: bool,
}

/// The compiler's answer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Lowered {
    Statement(SqlQuery),
    /// A leaf family this wave does not lower. §5.9's dispatch sends the query
    /// to the walk, which is the SAME answer, not a different one — the reason
    /// is carried for the receipt and for the fallback counter.
    Unsupported(&'static str),
}

/// Everything an execution binds that is not in the IR.
pub(crate) struct LoweringInputs<'a> {
    /// The ONE execution-day snapshot `resolve_for_execution` took.
    pub(crate) today: JournalDate,
    /// The registry snapshot that decides each property key's effective type
    /// (§6.3). The walk reads the same snapshot for the same execution.
    pub(crate) registry: &'a Registry,
    /// Page ids an unaccepted local overlay covers (§5.9, Managed Storage).
    /// Empty on Direct Files.
    pub(crate) masked_pages: &'a [[u8; 16]],
    /// `LIMIT cutoff + 1` when the caller supplies a cutoff (§5.6).
    pub(crate) cutoff: Option<usize>,
}

/// Lower one resolved query (SPEC §5.1–§5.7).
///
/// The filter is the EVALUABLE one: `Off` subtrees are removed bottom-up first,
/// exactly as the walk does (§3.5), so the two engines never see different
/// trees.
pub(crate) fn lower_query(query: &Query, inputs: &LoweringInputs<'_>) -> Lowered {
    let mut compiler = Compiler {
        inputs,
        params: Vec::new(),
        next_alias: 0,
        unsupported: None,
    };
    // An invalid query returns zero results plus its diagnostics (§3.5); the
    // caller never reaches the statement, but a `0` predicate keeps this
    // function total.
    let filter = if query.is_invalid() {
        Filter::False
    } else {
        query.evaluable_filter()
    };
    let (row, select, from) = match query.anchor {
        Anchor::Block => (
            Row::Block("b"),
            "SELECT b.block_id, b.page_id, p.name, p.text_kind, p.path",
            "FROM blocks b JOIN pages p ON p.page_id = b.page_id",
        ),
        Anchor::Page => (
            Row::Page("p"),
            "SELECT p.page_id, p.name, p.text_kind, p.journal_day",
            "FROM pages p",
        ),
    };
    let mut where_ = compiler.filter(&filter, row);
    // §5.3: the result-set rule is applied in the SAME statement. OG's
    // `tree/filter-top-level-blocks` drops a matched block whose IMMEDIATE
    // parent also matched, and the walk implements it in
    // `collect_og_query_roots`'s `matched` stack — a block is emitted iff it
    // matches and its parent does not. The parent probe is correlated on the
    // primary key, so it costs one seek plus whatever the filter itself costs,
    // never a second pass over `blocks`.
    let matches_nothing = where_ == "0";
    if let Row::Block(alias) = row {
        let parent = compiler.alias("root");
        let parent_matches = compiler.filter(&filter, Row::Block(&parent));
        // A parent that can never match cannot shadow anything, so the whole
        // probe folds away rather than becoming a correlated subquery over `0`.
        let unshadowed = if parent_matches == "0" {
            "1".to_string()
        } else {
            format!(
                "({alias}.parent_block_id IS NULL OR {alias}.parent_block_id NOT IN \
                 (SELECT {parent}.block_id FROM blocks {parent} \
                 WHERE {parent}.block_id = {alias}.parent_block_id AND {parent_matches}))"
            )
        };
        where_ = fold_and(vec![where_, unshadowed]);
    }
    if let Some(unsupported) = compiler.unsupported {
        return Lowered::Unsupported(unsupported);
    }
    // §5.9: the overlay-masked page ids are removed inside the statement, so the
    // masked read and the overlay walk cannot both answer for one page.
    if !inputs.masked_pages.is_empty() {
        let column = match row {
            Row::Block(alias) => format!("{alias}.page_id"),
            Row::Page(alias) => format!("{alias}.page_id"),
        };
        let list = inputs
            .masked_pages
            .iter()
            .map(|page| compiler.bind(PhysicalQueryValue::Blob(page.to_vec())))
            .collect::<Vec<_>>()
            .join(", ");
        where_ = fold_and(vec![where_, format!("{column} NOT IN ({list})")]);
    }
    // **No `ORDER BY` (§5.3, measured).** The walk's base order is its page
    // SOURCE's enumeration order, which no column of the projection reproduces —
    // ordering by `pages.path` would be a guess, and it is a costly one: with
    // `ORDER BY p.path` in the statement SQLite prefers `SCAN p USING INDEX
    // pages_path_idx` over the `pages_journal_day_idx` range the filter asks
    // for, because scanning the path index avoids the sort. That is exactly
    // §5.7's failure mode, bought for an ordering the caller has to impose
    // anyway when it groups the rows by page. Base order and grouping therefore
    // belong to the result construction, and this statement answers only which
    // rows match.
    let mut sql = format!("{select} {from} WHERE {where_}");
    if let Some(cutoff) = inputs.cutoff {
        let limit = compiler.bind(PhysicalQueryValue::Integer(
            i64::try_from(cutoff.saturating_add(1)).unwrap_or(i64::MAX),
        ));
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    // Folding discards fragments that already bound values; positional
    // parameters have to be renumbered around the holes.
    let (sql, params) = compact_parameters(&sql, &compiler.params);
    Lowered::Statement(SqlQuery {
        sql,
        params,
        // §5.7 asks for an index only where there are rows to find. A filter
        // that folded to false reads nothing, so there is no bound to demand.
        positively_bounded: !matches_nothing && positively_bounded(&filter, query.anchor),
        matches_nothing,
    })
}

/// Which row a filter is being compiled against, and under which alias.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Row<'a> {
    Block(&'a str),
    Page(&'a str),
}

struct Compiler<'a> {
    inputs: &'a LoweringInputs<'a>,
    params: Vec<PhysicalQueryValue>,
    next_alias: usize,
    unsupported: Option<&'static str>,
}

impl Compiler<'_> {
    /// Bind one value and return its positional placeholder (§5.5).
    fn bind(&mut self, value: PhysicalQueryValue) -> String {
        self.params.push(value);
        format!("?{}", self.params.len())
    }

    fn decline(&mut self, reason: &'static str) -> String {
        self.unsupported.get_or_insert(reason);
        "0".to_string()
    }

    /// A fresh alias for a relation subquery, so nesting cannot shadow.
    fn alias(&mut self, stem: &str) -> String {
        self.next_alias += 1;
        format!("{stem}{}", self.next_alias)
    }

    // -----------------------------------------------------------------------
    // The boolean skeleton
    // -----------------------------------------------------------------------

    /// Transcribes `hasura/ndc-postgres`
    /// `filtering.rs::translate_expression_with_joins`: `And`/`Or` fold their
    /// already-translated operands, `Not` wraps one. Every operand is
    /// two-valued, so `NOT` is classical (§3.4) rather than SQL's three-valued
    /// `NOT NULL = NULL`.
    fn filter(&mut self, filter: &Filter, row: Row<'_>) -> String {
        match filter {
            // `normalized()` turns an originally-empty group into the constant
            // it means (§3.5); this arm keeps the compiler total for a tree that
            // never went through it.
            Filter::And { items } if items.is_empty() => "1".to_string(),
            Filter::Or { items } if items.is_empty() => "0".to_string(),
            Filter::And { items } => {
                fold_and(items.iter().map(|it| self.filter(it, row)).collect())
            }
            Filter::Or { items } => fold_or(items.iter().map(|it| self.filter(it, row)).collect()),
            Filter::Not { inner } => fold_not(self.filter(inner, row)),
            Filter::True => "1".to_string(),
            // A `Raw` span is never satisfiable, and `not(<raw>)` must not
            // invent matches — the walk's rule, verbatim.
            Filter::False | Filter::Raw { .. } => "0".to_string(),
            Filter::Off { .. } => {
                debug_assert!(false, "Off must be removed before lowering (§3.5)");
                "1".to_string()
            }
            Filter::Leaf { leaf } => match row {
                Row::Block(alias) => self.leaf_block(leaf, alias),
                Row::Page(alias) => self.leaf_page(leaf, alias),
            },
        }
    }

    // -----------------------------------------------------------------------
    // §5.1 — one `IN (subquery)` per relational quantifier
    // -----------------------------------------------------------------------

    /// Transcribes `odata-query`'s `visit_CollectionLambda`: `Any` is the
    /// subquery, `All` is the subquery over the INVERTED predicate, negated.
    /// `None` is `Any` negated, which is the same rewrite in the other
    /// direction.
    ///
    /// `owner` is the outer column the subquery's selected column is compared
    /// against, and it is never NULL on either side (J1) — which is what makes
    /// `NOT IN` mean what it reads as.
    fn quantified(
        &mut self,
        owner: &str,
        quant: Quant,
        mut subquery: impl FnMut(&mut Self, bool) -> Option<String>,
    ) -> String {
        match quant {
            // No row can satisfy the predicate, so no owner is `IN` it.
            Quant::Any => match subquery(self, false) {
                Some(sub) => format!("{owner} IN ({sub})"),
                None => "0".to_string(),
            },
            // `NOT IN` an empty set is true for every owner — and for `Every`
            // the empty set is the set of VIOLATORS, so every owner passes.
            Quant::None => match subquery(self, false) {
                Some(sub) => format!("{owner} NOT IN ({sub})"),
                None => "1".to_string(),
            },
            Quant::Every => match subquery(self, true) {
                Some(sub) => format!("{owner} NOT IN ({sub})"),
                None => "1".to_string(),
            },
        }
    }

    /// Transcribes `filtering.rs::translate_exists_in_collection`: one
    /// `SELECT <owner> FROM <relation> WHERE <guard> AND <predicate>` in the
    /// relation's own row scope. The `invert` flag is `prisma-engines`'
    /// `reverse` — a negation carried INTO the nested predicate.
    ///
    /// `None` is "this subquery selects no row" — the predicate folded to false,
    /// so there is nothing for SQLite to look for. Returning it rather than
    /// emitting `WHERE … AND 0` is what keeps [`Compiler::quantified`] able to
    /// fold the quantifier, and it is what keeps §5.7 honest: a provably empty
    /// subquery still gets planned, and SQLite plans it as a covering SCAN.
    fn exists_subquery(
        &mut self,
        select: &str,
        from: &str,
        guards: &[String],
        predicate: String,
        invert: bool,
    ) -> Option<String> {
        let predicate = if invert {
            fold_not(predicate)
        } else {
            predicate
        };
        let mut clauses: Vec<String> = guards.to_vec();
        clauses.push(predicate);
        let where_ = fold_and(clauses);
        if where_ == "0" {
            return None;
        }
        Some(format!("SELECT {select} FROM {from} WHERE {where_}"))
    }

    /// One relation subquery, in the relation element's own row scope.
    fn relation_subquery(
        &mut self,
        select: &str,
        from: &str,
        guards: &[String],
        pred: &Filter,
        row: Row<'_>,
        invert: bool,
    ) -> Option<String> {
        let predicate = self.filter(pred, row);
        self.exists_subquery(select, from, guards, predicate, invert)
    }

    // -----------------------------------------------------------------------
    // Block-row leaves
    // -----------------------------------------------------------------------

    fn leaf_block(&mut self, leaf: &Leaf, b: &str) -> String {
        match leaf {
            Leaf::Attr { attr, op, value } => match attr {
                Attr::Content => self.content(*op, value, b),
                Attr::Task => self.task(*op, value, b),
                Attr::Priority => self.priority(*op, value, b),
                Attr::Scheduled => self.planning(*op, value, b, "scheduled"),
                Attr::Deadline => self.planning(*op, value, b, "deadline"),
                // Page attributes only appear under a `page` relation and the
                // property-element attributes only under `props`; the walk
                // answers false for anything else (`eval_block_leaf`).
                _ => "0".to_string(),
            },
            Leaf::Rel { rel, quant, pred } => match rel {
                Rel::Refs => self.refs(*quant, pred, b, true),
                Rel::Tags => self.tags(*quant, pred, b, OWNER_BLOCK),
                Rel::Props => self.props(*quant, pred, b, "block_id", OWNER_BLOCK),
                Rel::Children => self.children(*quant, pred, b),
                Rel::Page => self.page_relation(*quant, pred, b),
                // `blocks` is a page-row relation; the block-anchored walk
                // answers false for it (`eval_block_leaf`'s `Rel::Blocks` arm),
                // and so does this.
                Rel::Blocks => "0".to_string(),
            },
        }
    }

    /// `content` predicates read `blocks.query_visible_folded` — the EXACT
    /// visible text folded once at write time (§5.8), never the
    /// whitespace-collapsed `searchable_text` beside it. The walk compares
    /// `BlockProjection::visible_lower`, which is the same fold of the same
    /// text.
    fn content(&mut self, op: CmpOp, value: &Value, b: &str) -> String {
        let Value::Text { text } = value else {
            return "0".to_string();
        };
        let column = format!("{b}.query_visible_folded");
        match op {
            CmpOp::Like => {
                let pattern = self.bind(PhysicalQueryValue::Text(canonical_fold(text)));
                format!("{column} LIKE {pattern} ESCAPE '\\'")
            }
            CmpOp::StartsWith => {
                let pattern = self.bind(PhysicalQueryValue::Text(format!(
                    "{}%",
                    like_escape(&canonical_fold(text))
                )));
                format!("{column} LIKE {pattern} ESCAPE '\\'")
            }
            CmpOp::Eq => {
                let literal = self.bind(PhysicalQueryValue::Text(canonical_fold(text)));
                format!("{column} = {literal}")
            }
            CmpOp::NotEq => {
                let literal = self.bind(PhysicalQueryValue::Text(canonical_fold(text)));
                format!("{column} <> {literal}")
            }
            // §5.10 is P1-c: `Match`'s friendly-search semantics need the
            // trigram prefilter plus an exact SQL substring predicate, and
            // `Regex` needs a compiled Rust regex SQLite does not have. The walk
            // answers both, correctly, today.
            CmpOp::Match => self.decline("content match (§5.10, P1-c)"),
            CmpOp::Regex => self.decline("content regexp (§5.10, P1-c)"),
            _ => "0".to_string(),
        }
    }

    /// `task` reads `tasks.marker`. Both producers write the marker
    /// ASCII-uppercased and lsdoc's `MARKERS` list is uppercase-only, so
    /// binding the uppercased operand is exactly the walk's
    /// `eq_ignore_ascii_case` and still seeks `tasks_marker_idx`.
    fn task(&mut self, op: CmpOp, value: &Value, b: &str) -> String {
        let alias = self.alias("t");
        let from = format!("tasks {alias}");
        let select = format!("{alias}.block_id");
        let owner = format!("{b}.block_id");
        match op {
            CmpOp::IsSet => format!("{owner} IN (SELECT {select} FROM {from})"),
            CmpOp::IsNotSet => format!("{owner} NOT IN (SELECT {select} FROM {from})"),
            CmpOp::Eq | CmpOp::NotEq => {
                let Value::Text { text } = value else {
                    return "0".to_string();
                };
                let literal = self.bind(PhysicalQueryValue::Text(text.to_ascii_uppercase()));
                let comparison = if op == CmpOp::Eq { "=" } else { "<>" };
                format!("{owner} IN (SELECT {select} FROM {from} WHERE {alias}.marker {comparison} {literal})")
            }
            CmpOp::In | CmpOp::NotIn => {
                let Value::List { items } = value else {
                    return "0".to_string();
                };
                let list = self.text_list(items, |text| text.to_ascii_uppercase());
                let Some(list) = list else {
                    // An empty list of operands: `in` is false, and `not in` is
                    // "present and not equal to anything", i.e. present.
                    return match op {
                        CmpOp::In => "0".to_string(),
                        _ => format!("{owner} IN (SELECT {select} FROM {from})"),
                    };
                };
                let membership = if op == CmpOp::In { "IN" } else { "NOT IN" };
                format!("{owner} IN (SELECT {select} FROM {from} WHERE {alias}.marker {membership} ({list}))")
            }
            _ => "0".to_string(),
        }
    }

    /// `priority` reads `block_planning.priority`, which is written from the
    /// block's projection independently of the task marker (§3.2 M2) — a
    /// markerless `[#A]` block has a row here and none in `tasks`.
    ///
    /// lsdoc's grammar accepts `[#X]` for exactly ONE ASCII character, so the
    /// stored value is always a single ASCII char and the walk's
    /// `eq_ignore_ascii_case` is exactly "equal to the operand in one of its two
    /// ASCII cases". Binding both spellings keeps the leaf on
    /// `block_planning_priority_idx` instead of wrapping the column in
    /// `upper()`, which would forfeit the seek.
    fn priority(&mut self, op: CmpOp, value: &Value, b: &str) -> String {
        let alias = self.alias("bp");
        let from = format!("block_planning {alias}");
        let select = format!("{alias}.block_id");
        let owner = format!("{b}.block_id");
        let present = format!("{alias}.priority IS NOT NULL");
        match op {
            CmpOp::IsSet => format!("{owner} IN (SELECT {select} FROM {from} WHERE {present})"),
            CmpOp::IsNotSet => {
                format!("{owner} NOT IN (SELECT {select} FROM {from} WHERE {present})")
            }
            CmpOp::Eq | CmpOp::NotEq => {
                let Value::Text { text } = value else {
                    return "0".to_string();
                };
                let list = self.ascii_case_pair(text);
                let membership = if op == CmpOp::Eq { "IN" } else { "NOT IN" };
                format!(
                    "{owner} IN (SELECT {select} FROM {from} WHERE {present} AND {alias}.priority {membership} ({list}))"
                )
            }
            CmpOp::In | CmpOp::NotIn => {
                let Value::List { items } = value else {
                    return "0".to_string();
                };
                let mut spellings: Vec<String> = Vec::new();
                for item in items {
                    if let Value::Text { text } = item {
                        spellings.push(text.to_ascii_lowercase());
                        spellings.push(text.to_ascii_uppercase());
                    }
                }
                spellings.sort();
                spellings.dedup();
                if spellings.is_empty() {
                    return match op {
                        CmpOp::In => "0".to_string(),
                        _ => format!("{owner} IN (SELECT {select} FROM {from} WHERE {present})"),
                    };
                }
                let list = spellings
                    .into_iter()
                    .map(|text| self.bind(PhysicalQueryValue::Text(text)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let membership = if op == CmpOp::In { "IN" } else { "NOT IN" };
                format!(
                    "{owner} IN (SELECT {select} FROM {from} WHERE {present} AND {alias}.priority {membership} ({list}))"
                )
            }
            _ => "0".to_string(),
        }
    }

    /// `scheduled` / `deadline` read `block_planning`. Presence is the TEXT
    /// column (a malformed `<2026-13-45 …>` has presence and no day, E1) and
    /// every ordering comparison is on the `*_day` ordinal, which is exactly
    /// what `eval_planning` does with `planning_day`.
    fn planning(&mut self, op: CmpOp, value: &Value, b: &str, field: &str) -> String {
        let alias = self.alias("bp");
        let from = format!("block_planning {alias}");
        let select = format!("{alias}.block_id");
        let owner = format!("{b}.block_id");
        let present = format!("{alias}.{field} IS NOT NULL");
        match op {
            CmpOp::IsSet => {
                return format!("{owner} IN (SELECT {select} FROM {from} WHERE {present})")
            }
            CmpOp::IsNotSet => {
                return format!("{owner} NOT IN (SELECT {select} FROM {from} WHERE {present})")
            }
            _ => {}
        }
        let column = format!("{alias}.{field}_day");
        let Some(test) = self.day_comparison(op, value, &column) else {
            return "0".to_string();
        };
        format!("{owner} IN (SELECT {select} FROM {from} WHERE {test})")
    }

    /// `refs` is OG's `:block/path-refs`, materialized as `block_path_refs`
    /// (§5.8) — the block's own normalized refs, every ancestor's, and its page.
    ///
    /// `top_level` is `false` inside a `children` predicate. The walk evaluates a
    /// child under the PARENT's ancestor multiset (`eval_block_leaf`'s
    /// `Rel::Children` arm passes `ancestor_refs` down unchanged, and
    /// `dfs_path_refs::enter` fires before the parent's own refs are pushed), so
    /// the set it tests is the child's closure MINUS the parent's own refs —
    /// which is not `block_path_refs(child)` and is not expressible from it.
    /// Lowering it anyway would be a walk/SQL difference, which this wave treats
    /// as a failure rather than a documented divergence, so the query goes to the
    /// walk instead and the fork is recorded for the manager.
    fn refs(&mut self, quant: Quant, pred: &Filter, b: &str, top_level: bool) -> String {
        if !top_level {
            return self.decline("refs nested in a children predicate (walk/SQL closure fork)");
        }
        let alias = self.alias("r");
        let owner = format!("{b}.block_id");
        // The walk's fast path: for the ONE predicate shape v1 accepts, `Every`
        // answers membership exactly as `Any` does (`eval_refs`'s
        // `single_ref_name` arm). Reproduced rather than corrected, because
        // `walk == SQL` is the contract.
        let quant = match (quant, pred.ref_name()) {
            (Quant::Every, Some(_)) => Quant::Any,
            (quant, _) => quant,
        };
        self.quantified(&owner, quant, |compiler, invert| {
            let column = format!("{alias}.normalized_name");
            let predicate = compiler.name_element(pred, &column, refs::normalize);
            compiler.exists_subquery(
                &format!("{alias}.block_id"),
                &format!("block_path_refs {alias}"),
                &[],
                predicate,
                invert,
            )
        })
    }

    /// `tags` is the block's or page's own inline `#tag` / Org headline tags.
    /// `tags.tag_key` is `refs::page_key(tag)` because a tag IS a page reference
    /// (§3.2 K18), which is the same fold `eval_name_element` applies.
    fn tags(&mut self, quant: Quant, pred: &Filter, owner_alias: &str, owner_type: i64) -> String {
        let alias = self.alias("tg");
        let owner_column = if owner_type == OWNER_BLOCK {
            format!("{owner_alias}.block_id")
        } else {
            format!("{owner_alias}.page_id")
        };
        let owner_type_literal = self.bind(PhysicalQueryValue::Integer(owner_type));
        self.quantified(&owner_column, quant, |compiler, invert| {
            let column = format!("{alias}.tag_key");
            let predicate = compiler.name_element(pred, &column, refs::page_key);
            compiler.exists_subquery(
                &format!("{alias}.owner_id"),
                &format!("tags {alias}"),
                &[format!("{alias}.owner_type = {owner_type_literal}")],
                predicate,
                invert,
            )
        })
    }

    /// `children` are the block's DIRECT children (A1):
    /// `SELECT c.parent_block_id FROM blocks c WHERE c.parent_block_id IS NOT
    /// NULL AND <pred(c)>`. The `IS NOT NULL` is J1 — without it `NOT IN` over a
    /// column that holds NULLs is NULL, not false.
    fn children(&mut self, quant: Quant, pred: &Filter, b: &str) -> String {
        let alias = self.alias("c");
        let owner = format!("{b}.block_id");
        self.quantified(&owner, quant, |compiler, invert| {
            // The child is a fresh block row; a `refs` leaf inside it is the
            // declined case documented on `refs`.
            let predicate = compiler.child_filter(pred, &alias);
            compiler.exists_subquery(
                &format!("{alias}.parent_block_id"),
                &format!("blocks {alias}"),
                &[format!("{alias}.parent_block_id IS NOT NULL")],
                predicate,
                invert,
            )
        })
    }

    /// A block filter evaluated in a nested (child) row scope.
    fn child_filter(&mut self, pred: &Filter, alias: &str) -> String {
        match pred {
            Filter::And { items } if items.is_empty() => "1".to_string(),
            Filter::Or { items } if items.is_empty() => "0".to_string(),
            Filter::And { items } => fold_and(
                items
                    .iter()
                    .map(|it| self.child_filter(it, alias))
                    .collect(),
            ),
            Filter::Or { items } => fold_or(
                items
                    .iter()
                    .map(|it| self.child_filter(it, alias))
                    .collect(),
            ),
            Filter::Not { inner } => fold_not(self.child_filter(inner, alias)),
            Filter::True => "1".to_string(),
            Filter::False | Filter::Raw { .. } => "0".to_string(),
            Filter::Off { .. } => {
                debug_assert!(false, "Off must be removed before lowering (§3.5)");
                "1".to_string()
            }
            Filter::Leaf {
                leaf:
                    Leaf::Rel {
                        rel: Rel::Refs,
                        quant,
                        pred,
                    },
            } => self.refs(*quant, pred, alias, false),
            Filter::Leaf { leaf } => self.leaf_block(leaf, alias),
        }
    }

    /// The to-one `page` relation of a block row. All three quantifiers reduce
    /// to the predicate or its negation, exactly as `eval_block_leaf` does.
    fn page_relation(&mut self, quant: Quant, pred: &Filter, b: &str) -> String {
        let alias = self.alias("pg");
        let owner = format!("{b}.page_id");
        let hit = {
            let predicate = self.filter(pred, Row::Page(&alias));
            self.exists_subquery(
                &format!("{alias}.page_id"),
                &format!("pages {alias}"),
                &[],
                predicate,
                false,
            )
        };
        match (quant, hit) {
            (Quant::Any | Quant::Every, Some(hit)) => format!("{owner} IN ({hit})"),
            (Quant::None, Some(hit)) => format!("{owner} NOT IN ({hit})"),
            (Quant::Any | Quant::Every, None) => "0".to_string(),
            (Quant::None, None) => "1".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // Page-row leaves
    // -----------------------------------------------------------------------

    fn leaf_page(&mut self, leaf: &Leaf, p: &str) -> String {
        match leaf {
            Leaf::Attr { attr, op, value } => match attr {
                Attr::Name => self.page_name(*op, value, p),
                Attr::Journal => match (op, value) {
                    // `pages.text_kind`, not `journal_day IS NOT NULL`: a journal
                    // page whose stem does not parse has kind Journal and no day,
                    // and the walk reads the kind (`eval_page`'s `Attr::Journal`).
                    (CmpOp::Eq, Value::Bool { value: true }) => {
                        format!("{p}.text_kind = {TEXT_KIND_JOURNAL}")
                    }
                    (CmpOp::Eq, Value::Bool { value: false }) => {
                        format!("{p}.text_kind <> {TEXT_KIND_JOURNAL}")
                    }
                    _ => "0".to_string(),
                },
                Attr::Day => self.page_day(*op, value, p),
                Attr::Namespace => self.page_namespace(*op, value, p),
                _ => "0".to_string(),
            },
            Leaf::Rel { rel, quant, pred } => match rel {
                Rel::Props => self.props(*quant, pred, p, "page_id", OWNER_PAGE),
                // A page's own refs, its blocks and its tag table are not walked
                // by `eval_page`; its `_ => false` arm is reproduced here.
                _ => "0".to_string(),
            },
        }
    }

    /// `pages.name_key` is `refs::page_key(name)` — the same page-identity fold
    /// `eval_page_name` applies to both sides of its comparison.
    fn page_name(&mut self, op: CmpOp, value: &Value, p: &str) -> String {
        let column = format!("{p}.name_key");
        match (op, value) {
            (CmpOp::Eq, Value::Text { text }) => {
                let literal = self.bind(PhysicalQueryValue::Text(refs::page_key(text)));
                format!("{column} = {literal}")
            }
            (CmpOp::NotEq, Value::Text { text }) => {
                let literal = self.bind(PhysicalQueryValue::Text(refs::page_key(text)));
                format!("{column} <> {literal}")
            }
            (CmpOp::StartsWith, Value::Text { text }) => {
                // A range on the key column, which is what makes `(namespace X)`
                // and `page.name starts_with` seek `pages_name_key_idx` (§5.7).
                let prefix = page_prefix_key(text);
                self.prefix_range(&column, &prefix)
            }
            (CmpOp::Like, Value::Text { text }) => {
                let pattern = self.bind(PhysicalQueryValue::Text(canonical_fold(text)));
                format!("{column} LIKE {pattern} ESCAPE '\\'")
            }
            (CmpOp::In, Value::List { items }) => {
                match self.text_list(items, |text| refs::page_key(text)) {
                    Some(list) => format!("{column} IN ({list})"),
                    None => "0".to_string(),
                }
            }
            // `eval_page_name` has no other arm; `not in` on a page name is
            // false in the walk, so it is false here.
            _ => "0".to_string(),
        }
    }

    /// `page.day` reads `pages.journal_day`, the ONE journal-day answer
    /// (`JournalDays::day`) that also fills `PageEntry::date_key`, which is what
    /// the walk compares.
    fn page_day(&mut self, op: CmpOp, value: &Value, p: &str) -> String {
        let column = format!("{p}.journal_day");
        match op {
            CmpOp::IsSet => return format!("{column} IS NOT NULL"),
            CmpOp::IsNotSet => return format!("{column} IS NULL"),
            _ => {}
        }
        self.day_comparison(op, value, &column)
            .unwrap_or_else(|| "0".to_string())
    }

    /// The Tine-only `page.namespace` leaf: the immediate parent segment of the
    /// page-identity key (M20). There is no `namespace_key` column — the
    /// `name_key` range measured sufficient for the bounded `starts_with` form,
    /// and this unbounded form is the one §5.7's table already marks `no`.
    fn page_namespace(&mut self, op: CmpOp, value: &Value, p: &str) -> String {
        let column = format!("{p}.name_key");
        let has_parent = format!("instr({column}, '/') > 0");
        match op {
            CmpOp::IsSet => return has_parent,
            CmpOp::IsNotSet => return format!("instr({column}, '/') = 0"),
            _ => {}
        }
        // `name_key` is already fully lowercased, so the walk's ASCII-insensitive
        // comparison is equality against the ASCII-lowercased operand.
        let equals = |compiler: &mut Self, text: &str| -> String {
            let parent = text.to_ascii_lowercase();
            let head = compiler.bind(PhysicalQueryValue::Text(format!("{parent}/")));
            let width = parent.chars().count() + 1;
            format!(
                "substr({column}, 1, {width}) = {head} AND instr(substr({column}, {}), '/') = 0",
                width + 1
            )
        };
        match (op, value) {
            (CmpOp::Eq, Value::Text { text }) => {
                let test = equals(self, text);
                format!("({test})")
            }
            (CmpOp::NotEq, Value::Text { text }) => {
                let test = equals(self, text);
                format!("({has_parent} AND NOT ({test}))")
            }
            (CmpOp::In, Value::List { items }) => {
                let tests: Vec<String> = items
                    .iter()
                    .filter_map(|item| match item {
                        Value::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect();
                if tests.is_empty() {
                    return "0".to_string();
                }
                let parts: Vec<String> = tests
                    .iter()
                    .map(|text| {
                        let test = equals(self, text);
                        format!("({test})")
                    })
                    .collect();
                format!("({})", parts.join(" OR "))
            }
            (CmpOp::NotIn, Value::List { items }) => {
                let tests: Vec<String> = items
                    .iter()
                    .filter_map(|item| match item {
                        Value::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect();
                if tests.is_empty() {
                    return has_parent;
                }
                let parts: Vec<String> = tests
                    .iter()
                    .map(|text| {
                        let test = equals(self, text);
                        format!("({test})")
                    })
                    .collect();
                format!("({has_parent} AND NOT ({}))", parts.join(" OR "))
            }
            _ => "0".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // §3.3 — property elements, the two-level form
    // -----------------------------------------------------------------------

    /// The five property forms (§3.3), lowered as §5.1 requires: the presence
    /// probe over `properties` and the atom probe over `property_atoms`, each
    /// ONE subquery carrying its whole conjunction.
    ///
    /// `property Every` is presence ∧ "no atom violates" — the walk's
    /// `present && atoms.iter().all(matches)` — which is why it needs both
    /// subqueries and a generic `Every` needs only one.
    fn props(
        &mut self,
        quant: Quant,
        pred: &Filter,
        owner_alias: &str,
        owner_id_column: &str,
        owner_type: i64,
    ) -> String {
        // Every property leaf the parsers build carries the `key = 'k'`
        // conjunct; without it the leaf names no relation to quantify over.
        let Some(key) = pred.props_key() else {
            return "0".to_string();
        };
        let key_norm = property_key_norm(&key);
        let owner = format!("{owner_alias}.{owner_id_column}");
        let owner_type_literal = self.bind(PhysicalQueryValue::Integer(owner_type));
        let key_literal = self.bind(PhysicalQueryValue::Text(key_norm.clone()));
        let presence = {
            let alias = self.alias("pr");
            format!(
                "{owner} IN (SELECT {alias}.owner_id FROM properties {alias} \
                 WHERE {alias}.normalized_name = {key_literal} \
                 AND {alias}.owner_type = {owner_type_literal})"
            )
        };

        let Some(test) = pred.props_atom_test() else {
            // Bare presence: `prop('k') is not null` is `Any(key='k')` and
            // `is null` is `None(key='k')`; property `Every` carries presence.
            return match quant {
                Quant::Any | Quant::Every => presence,
                Quant::None => fold_not(presence),
            };
        };

        // `= ''` and `all-page-tags`'s `atom_count > 0` are properties of the
        // whole atom list, not of one atom, and are scoped by presence.
        if let Some(count) = self.atom_count_test(&test, &owner, &key_literal, &owner_type_literal)
        {
            let hit = fold_and(vec![presence, count]);
            return match quant {
                Quant::Any | Quant::Every => hit,
                Quant::None => fold_not(hit),
            };
        }

        let effective = self
            .inputs
            .registry
            .effective_type(&key_norm)
            .unwrap_or(ObservedType::Text);
        let atom_subquery = |compiler: &mut Self, invert: bool| -> Option<String> {
            let alias = compiler.alias("a");
            let predicate = compiler.atom_test(&test, &alias, effective);
            compiler.exists_subquery(
                &format!("{alias}.owner_id"),
                &format!("property_atoms {alias}"),
                &[
                    format!("{alias}.normalized_name = {key_literal}"),
                    format!("{alias}.owner_type = {owner_type_literal}"),
                ],
                predicate,
                invert,
            )
        };
        match quant {
            Quant::Any => match atom_subquery(self, false) {
                Some(sub) => format!("{owner} IN ({sub})"),
                None => "0".to_string(),
            },
            Quant::None => match atom_subquery(self, false) {
                Some(sub) => format!("{owner} NOT IN ({sub})"),
                None => "1".to_string(),
            },
            // Presence still has to hold: the walk's `present && all(...)` is
            // vacuously true over an empty atom list only when the property is
            // there at all.
            Quant::Every => match atom_subquery(self, true) {
                Some(sub) => format!("({presence} AND {owner} NOT IN ({sub}))"),
                None => presence,
            },
        }
    }

    /// `Some(<sql>)` when the property test reads only `atom_count`.
    fn atom_count_test(
        &mut self,
        test: &Filter,
        owner: &str,
        key_literal: &str,
        owner_type_literal: &str,
    ) -> Option<String> {
        let Filter::Leaf {
            leaf:
                Leaf::Attr {
                    attr: Attr::AtomCount,
                    op,
                    value: Value::Number { number },
                },
        } = test
        else {
            return None;
        };
        let comparison = match op {
            CmpOp::Eq => "=",
            CmpOp::NotEq => "<>",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            _ => return None,
        };
        let alias = self.alias("ac");
        let bound = self.bind(PhysicalQueryValue::Real(*number));
        // A correlated COUNT rather than a quantifier: cardinality is a property
        // of the whole atom list, and the `(owner_type, owner_id,
        // normalized_name)` primary-key prefix makes it a seek.
        Some(format!(
            "(SELECT COUNT(*) FROM property_atoms {alias} \
             WHERE {alias}.owner_type = {owner_type_literal} \
             AND {alias}.owner_id = {owner} \
             AND {alias}.normalized_name = {key_literal}) {comparison} {bound}"
        ))
    }

    /// The predicate over ONE atom, coerced by the key's effective type (§6.3).
    /// Mirrors `eval_atom_test` / `eval_atom_value` clause for clause.
    fn atom_test(&mut self, test: &Filter, a: &str, effective: ObservedType) -> String {
        match test {
            Filter::True => "1".to_string(),
            Filter::False | Filter::Raw { .. } => "0".to_string(),
            Filter::And { items } if items.is_empty() => "1".to_string(),
            Filter::Or { items } if items.is_empty() => "0".to_string(),
            Filter::And { items } => fold_and(
                items
                    .iter()
                    .map(|item| self.atom_test(item, a, effective))
                    .collect(),
            ),
            Filter::Or { items } => fold_or(
                items
                    .iter()
                    .map(|item| self.atom_test(item, a, effective))
                    .collect(),
            ),
            Filter::Not { inner } => fold_not(self.atom_test(inner, a, effective)),
            Filter::Off { .. } => {
                debug_assert!(false, "Off must be removed before lowering (§3.5)");
                "1".to_string()
            }
            Filter::Leaf {
                leaf: Leaf::Attr { attr, op, value },
            } => match attr {
                Attr::Value => self.atom_value(*op, value, a, effective),
                // The leaf's own scoping conjunct, already applied above.
                Attr::Key => "1".to_string(),
                _ => "0".to_string(),
            },
            Filter::Leaf { .. } => "0".to_string(),
        }
    }

    fn atom_value(&mut self, op: CmpOp, value: &Value, a: &str, effective: ObservedType) -> String {
        if op == CmpOp::IsSet {
            return "1".to_string();
        }
        match effective {
            ObservedType::Number => self.atom_number(op, value, &format!("{a}.atom_num")),
            ObservedType::Date => self
                .day_comparison(op, value, &format!("{a}.atom_day"))
                .unwrap_or_else(|| "0".to_string()),
            // Text, ref and checkbox atoms all compare their NFC-lowercased key.
            _ => self.atom_text(op, value, &format!("{a}.atom_key")),
        }
    }

    /// `compare_number`, in SQL. `atom_num` is NULL for an atom that does not
    /// coerce, and an atom whose typed value is absent fails EVERY comparison
    /// including `!=` (K3) — which is exactly what the `IS NOT NULL` guard says.
    fn atom_number(&mut self, op: CmpOp, value: &Value, column: &str) -> String {
        let operand = |value: &Value| -> Option<f64> {
            match value {
                Value::Number { number } => Some(*number),
                Value::Text { text } => text.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
                Value::Date { literal } => {
                    literal.trim().parse::<f64>().ok().filter(|n| n.is_finite())
                }
                _ => None,
            }
        };
        match (op, value) {
            (CmpOp::Between, Value::List { items }) if items.len() == 2 => {
                match (operand(&items[0]), operand(&items[1])) {
                    (Some(low), Some(high)) => {
                        let (low, high) = if low > high { (high, low) } else { (low, high) };
                        let low = self.bind(PhysicalQueryValue::Real(low));
                        let high = self.bind(PhysicalQueryValue::Real(high));
                        format!("({column} IS NOT NULL AND {column} BETWEEN {low} AND {high})")
                    }
                    _ => "0".to_string(),
                }
            }
            (CmpOp::In | CmpOp::NotIn, Value::List { items }) => {
                let bounds: Vec<f64> = items.iter().filter_map(operand).collect();
                if bounds.is_empty() {
                    return match op {
                        CmpOp::In => "0".to_string(),
                        // `not in ()` is vacuously true for a coercible atom.
                        _ => format!("({column} IS NOT NULL)"),
                    };
                }
                let list = bounds
                    .into_iter()
                    .map(|bound| self.bind(PhysicalQueryValue::Real(bound)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let membership = if op == CmpOp::In { "IN" } else { "NOT IN" };
                format!("({column} IS NOT NULL AND {column} {membership} ({list}))")
            }
            (op, value) => {
                let Some(bound) = operand(value) else {
                    return "0".to_string();
                };
                let comparison = match op {
                    CmpOp::Eq => "=",
                    CmpOp::NotEq => "<>",
                    CmpOp::Lt => "<",
                    CmpOp::Le => "<=",
                    CmpOp::Gt => ">",
                    CmpOp::Ge => ">=",
                    _ => return "0".to_string(),
                };
                let bound = self.bind(PhysicalQueryValue::Real(bound));
                format!("({column} IS NOT NULL AND {column} {comparison} {bound})")
            }
        }
    }

    /// `compare_atom_text`, in SQL. `atom_key` is `NOT NULL`, so no null guard
    /// is needed — and adding one would hide a future nullable column.
    fn atom_text(&mut self, op: CmpOp, value: &Value, column: &str) -> String {
        let operand = |value: &Value| -> Option<String> {
            match value {
                Value::Text { text } => Some(atom_key(text)),
                Value::Number { number } => Some(atom_key(&format_number(*number))),
                Value::Date { literal } => Some(atom_key(literal)),
                Value::Bool { value } => Some(if *value {
                    "true".into()
                } else {
                    "false".into()
                }),
                _ => None,
            }
        };
        match (op, value) {
            (CmpOp::In | CmpOp::NotIn, Value::List { items }) => {
                let keys: Vec<String> = items.iter().filter_map(&operand).collect();
                if keys.is_empty() {
                    return match op {
                        CmpOp::In => "0".to_string(),
                        _ => "1".to_string(),
                    };
                }
                let list = keys
                    .into_iter()
                    .map(|key| self.bind(PhysicalQueryValue::Text(key)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let membership = if op == CmpOp::In { "IN" } else { "NOT IN" };
                format!("{column} {membership} ({list})")
            }
            (CmpOp::Like, value) => match operand(value) {
                Some(pattern) => {
                    let pattern = self.bind(PhysicalQueryValue::Text(pattern));
                    format!("{column} LIKE {pattern} ESCAPE '\\'")
                }
                None => "0".to_string(),
            },
            (CmpOp::StartsWith, value) => match operand(value) {
                Some(prefix) => {
                    let pattern = self.bind(PhysicalQueryValue::Text(format!(
                        "{}%",
                        like_escape(&prefix)
                    )));
                    format!("{column} LIKE {pattern} ESCAPE '\\'")
                }
                None => "0".to_string(),
            },
            (op, value) => {
                let Some(key) = operand(value) else {
                    return "0".to_string();
                };
                let comparison = match op {
                    CmpOp::Eq => "=",
                    // K3: a text atom always coerces, so `!=` is plain
                    // inequality on the comparison key.
                    CmpOp::NotEq => "<>",
                    _ => return "0".to_string(),
                };
                let key = self.bind(PhysicalQueryValue::Text(key));
                format!("{column} {comparison} {key}")
            }
        }
    }

    // -----------------------------------------------------------------------
    // Shared comparison helpers
    // -----------------------------------------------------------------------

    /// `compare_day`, in SQL, over a NULLABLE day-ordinal column.
    ///
    /// The asymmetry is the walk's: `>=` and `<=` are `is_none_or` — an
    /// unresolvable bound imposes NO limit — while `>`, `<`, `=` and `!=` are
    /// `is_some_and` and are false without one.
    fn day_comparison(&mut self, op: CmpOp, value: &Value, column: &str) -> Option<String> {
        let today = self.inputs.today;
        let resolve = |value: &Value| -> Option<i64> {
            match value {
                Value::Date { literal } => crate::query::resolve_date_token(literal, today),
                Value::Number { number } => Some(*number as i64),
                _ => None,
            }
        };
        match (op, value) {
            (CmpOp::Between, Value::List { items }) if items.len() == 2 => {
                let (low, high) = (resolve(&items[0]), resolve(&items[1]));
                // OG's `build-between-two-arg` sorts its two resolved bounds.
                let (low, high) = match (low, high) {
                    (Some(low), Some(high)) if low > high => (Some(high), Some(low)),
                    pair => pair,
                };
                let mut clauses = vec![format!("{column} IS NOT NULL")];
                if let Some(low) = low {
                    let low = self.bind(PhysicalQueryValue::Integer(low));
                    clauses.push(format!("{column} >= {low}"));
                }
                if let Some(high) = high {
                    let high = self.bind(PhysicalQueryValue::Integer(high));
                    clauses.push(format!("{column} <= {high}"));
                }
                Some(format!("({})", clauses.join(" AND ")))
            }
            (CmpOp::Ge | CmpOp::Le, value) => {
                let Some(bound) = resolve(value) else {
                    // `is_none_or`: no bound, so any day passes.
                    return Some(format!("{column} IS NOT NULL"));
                };
                let comparison = if op == CmpOp::Ge { ">=" } else { "<=" };
                let bound = self.bind(PhysicalQueryValue::Integer(bound));
                Some(format!(
                    "({column} IS NOT NULL AND {column} {comparison} {bound})"
                ))
            }
            (CmpOp::Gt | CmpOp::Lt | CmpOp::Eq | CmpOp::NotEq, value) => {
                let bound = resolve(value)?;
                let comparison = match op {
                    CmpOp::Gt => ">",
                    CmpOp::Lt => "<",
                    CmpOp::Eq => "=",
                    _ => "<>",
                };
                let bound = self.bind(PhysicalQueryValue::Integer(bound));
                Some(format!(
                    "({column} IS NOT NULL AND {column} {comparison} {bound})"
                ))
            }
            _ => None,
        }
    }

    /// The predicate over a ref or tag element, whose only attribute is `name`.
    /// `normalize` is the fold the PRODUCER applied to the stored column, passed
    /// in so the two can never be spelled differently at one call site.
    fn name_element(
        &mut self,
        pred: &Filter,
        column: &str,
        normalize: fn(&str) -> String,
    ) -> String {
        match pred {
            Filter::True => "1".to_string(),
            Filter::False => "0".to_string(),
            Filter::And { items } if items.is_empty() => "1".to_string(),
            Filter::Or { items } if items.is_empty() => "0".to_string(),
            Filter::And { items } => {
                let parts: Vec<String> = items
                    .iter()
                    .map(|item| self.name_element(item, column, normalize))
                    .collect();
                format!("({})", parts.join(" AND "))
            }
            Filter::Or { items } => {
                let parts: Vec<String> = items
                    .iter()
                    .map(|item| self.name_element(item, column, normalize))
                    .collect();
                format!("({})", parts.join(" OR "))
            }
            Filter::Not { inner } => {
                let inner = self.name_element(inner, column, normalize);
                format!("(NOT {inner})")
            }
            Filter::Leaf {
                leaf:
                    Leaf::Attr {
                        attr: Attr::Name,
                        op: CmpOp::Eq,
                        value: Value::Text { text },
                    },
            } => {
                let literal = self.bind(PhysicalQueryValue::Text(normalize(text)));
                format!("{column} = {literal}")
            }
            // `eval_name_element` answers false for every other shape.
            _ => "0".to_string(),
        }
    }

    /// A literal prefix as a half-open range on an ordered key column — the form
    /// SQLite can seek (`name_key > ? AND name_key < ?`), unlike `LIKE 'p%'`,
    /// which it can only use with an ASCII-safe collation.
    fn prefix_range(&mut self, column: &str, prefix: &str) -> String {
        if prefix.is_empty() {
            return "1".to_string();
        }
        let Some(upper) = prefix_upper_bound(prefix) else {
            // The prefix ends at the last representable scalar value; a LIKE
            // keeps the leaf correct, and §5.7 only promises a plan for shapes
            // that can have one.
            let pattern = self.bind(PhysicalQueryValue::Text(format!(
                "{}%",
                like_escape(prefix)
            )));
            return format!("{column} LIKE {pattern} ESCAPE '\\'");
        };
        let low = self.bind(PhysicalQueryValue::Text(prefix.to_string()));
        let high = self.bind(PhysicalQueryValue::Text(upper));
        format!("({column} >= {low} AND {column} < {high})")
    }

    /// A bound `IN` list built from the `Text` items of a `List` value, with
    /// each item put through the column's own normalization. `None` when no item
    /// is a text literal, which is the walk's "matches nothing".
    fn text_list(&mut self, items: &[Value], normalize: impl Fn(&str) -> String) -> Option<String> {
        let normalized: Vec<String> = items
            .iter()
            .filter_map(|item| match item {
                Value::Text { text } => Some(normalize(text)),
                _ => None,
            })
            .collect();
        if normalized.is_empty() {
            return None;
        }
        Some(
            normalized
                .into_iter()
                .map(|text| self.bind(PhysicalQueryValue::Text(text)))
                .collect::<Vec<_>>()
                .join(", "),
        )
    }

    /// Both ASCII cases of one operand, as a bound list.
    fn ascii_case_pair(&mut self, text: &str) -> String {
        let lower = text.to_ascii_lowercase();
        let upper = text.to_ascii_uppercase();
        let mut spellings = vec![lower];
        if !spellings.contains(&upper) {
            spellings.push(upper);
        }
        spellings
            .into_iter()
            .map(|text| self.bind(PhysicalQueryValue::Text(text)))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// ---------------------------------------------------------------------------
// §5.7 — positive boundedness
// ---------------------------------------------------------------------------

/// Is this query *positively bounded* (SPEC §5.7)?
///
/// "After pushing `not` to the leaves and removing `Off`, its root conjunction
/// contains at least one **positive** leaf whose anchor bound is `yes`." The
/// polarity is threaded rather than the tree rewritten, which is the same
/// `reverse` carrier `prisma-engines`' visitor uses.
///
/// **The table below is exhaustive** (A6): a leaf/operator pair absent from it
/// is unbounded. "Every OG head" is not a category.
pub(crate) fn positively_bounded(filter: &Filter, anchor: Anchor) -> bool {
    let row = match anchor {
        Anchor::Block => BoundRow::Block,
        Anchor::Page => BoundRow::Page,
    };
    bounded(filter, false, row)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BoundRow {
    Block,
    Page,
}

fn bounded(filter: &Filter, negated: bool, row: BoundRow) -> bool {
    match filter {
        // A conjunction needs ONE bounded conjunct; a disjunction needs ALL of
        // its arms bounded, because the anchor is reached once per arm.
        Filter::And { items } if !negated => items.iter().any(|item| bounded(item, false, row)),
        Filter::And { items } => {
            !items.is_empty() && items.iter().all(|item| bounded(item, true, row))
        }
        Filter::Or { items } if !negated => {
            !items.is_empty() && items.iter().all(|item| bounded(item, false, row))
        }
        Filter::Or { items } => items.iter().any(|item| bounded(item, true, row)),
        Filter::Not { inner } => bounded(inner, !negated, row),
        Filter::Leaf { leaf } => !negated && leaf_bounds(leaf, row),
        Filter::Off { .. } | Filter::True | Filter::False | Filter::Raw { .. } => false,
    }
}

fn leaf_bounds(leaf: &Leaf, row: BoundRow) -> bool {
    match leaf {
        Leaf::Attr { attr, op, .. } => match (row, attr) {
            // `content` has no index on `blocks`; `starts_with` on it is not
            // range-lowerable either.
            (BoundRow::Block, Attr::Content) => false,
            (BoundRow::Block, Attr::Task) => matches!(
                op,
                CmpOp::Eq | CmpOp::NotEq | CmpOp::In | CmpOp::NotIn | CmpOp::IsSet
            ),
            (BoundRow::Block, Attr::Priority) => matches!(
                op,
                CmpOp::Eq | CmpOp::NotEq | CmpOp::In | CmpOp::NotIn | CmpOp::IsSet
            ),
            // No `in`: the §4.2.3 matrix rejects set membership on dates (Y4).
            (BoundRow::Block, Attr::Scheduled | Attr::Deadline) => matches!(
                op,
                CmpOp::Eq
                    | CmpOp::Lt
                    | CmpOp::Le
                    | CmpOp::Gt
                    | CmpOp::Ge
                    | CmpOp::Between
                    | CmpOp::IsSet
            ),
            (BoundRow::Page, Attr::Name) => {
                matches!(op, CmpOp::Eq | CmpOp::In | CmpOp::StartsWith)
            }
            (BoundRow::Page, Attr::Day) => matches!(
                op,
                CmpOp::Eq
                    | CmpOp::Lt
                    | CmpOp::Le
                    | CmpOp::Gt
                    | CmpOp::Ge
                    | CmpOp::Between
                    | CmpOp::IsSet
            ),
            // `page.journal` reads `pages.text_kind`, which has no index — see
            // `leaf_page`. §5.7's table assumed it lowered to `journal_day IS
            // NOT NULL`, which would answer differently for a journal page whose
            // stem does not parse, so the leaf is conservatively unbounded here.
            (BoundRow::Page, Attr::Journal) => false,
            // No `namespace_key` column: the `name_key` range covers the bounded
            // form, and this one is `no` in §5.7's own table.
            (BoundRow::Page, Attr::Namespace) => false,
            _ => false,
        },
        Leaf::Rel { rel, quant, pred } => {
            // `none` and `every` are `NOT IN` complements: they never bound the
            // OUTER anchor, which needs another positive conjunct.
            if !matches!(quant, Quant::Any) && !matches!((row, rel), (_, Rel::Props)) {
                return false;
            }
            match (row, rel) {
                (BoundRow::Block, Rel::Refs) => pred.ref_name().is_some(),
                (BoundRow::Block, Rel::Tags) | (BoundRow::Page, Rel::Tags) => {
                    pred.ref_name().is_some()
                }
                // The key equality drives the probe, and any atom operator
                // filters inside that key's rows. Property `Every` carries the
                // presence `IN`, which is itself positive.
                (_, Rel::Props) => {
                    pred.props_key().is_some() && matches!(quant, Quant::Any | Quant::Every)
                }
                (BoundRow::Block, Rel::Children) => bounded(pred, false, BoundRow::Block),
                (BoundRow::Block, Rel::Page) => bounded(pred, false, BoundRow::Page),
                _ => false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Literal helpers
// ---------------------------------------------------------------------------

/// `AND` over already-lowered operands, folding the two constants.
///
/// **Folding is not an optimization here, it is a correctness-of-plan rule.** A
/// leaf whose operator does not apply to its column's effective type is FALSE
/// (SPEC 3.4), and handing SQLite `... AND 0` inside a subquery makes it plan a
/// covering scan for a predicate that provably selects nothing -- measured on
/// the anonymized corpus for `prop('score') > 5`, where `score` is a text key
/// and the numeric comparison is therefore unsatisfiable.
fn fold_and(parts: Vec<String>) -> String {
    if parts.iter().any(|part| part == "0") {
        return "0".to_string();
    }
    let kept: Vec<String> = parts.into_iter().filter(|part| part != "1").collect();
    match kept.len() {
        0 => "1".to_string(),
        1 => kept.into_iter().next().expect("one operand"),
        _ => format!("({})", kept.join(" AND ")),
    }
}

/// `OR` over already-lowered operands, folding the two constants.
fn fold_or(parts: Vec<String>) -> String {
    if parts.iter().any(|part| part == "1") {
        return "1".to_string();
    }
    let kept: Vec<String> = parts.into_iter().filter(|part| part != "0").collect();
    match kept.len() {
        0 => "0".to_string(),
        1 => kept.into_iter().next().expect("one operand"),
        _ => format!("({})", kept.join(" OR ")),
    }
}

/// `NOT` over an already-lowered operand, folding the two constants.
fn fold_not(inner: String) -> String {
    match inner.as_str() {
        "0" => "1".to_string(),
        "1" => "0".to_string(),
        _ => format!("(NOT {inner})"),
    }
}

/// Renumber the `?N` placeholders the statement actually kept, and drop the
/// values only a folded-away fragment referenced.
///
/// Parameters are POSITIONAL (I-22), so a fragment that bound a value and was
/// then folded away would leave an orphan slot and the driver would refuse the
/// whole statement. Every placeholder this compiler emits appears exactly once,
/// and no literal it emits contains a `?`, so one left-to-right pass is exact.
fn compact_parameters(
    sql: &str,
    params: &[PhysicalQueryValue],
) -> (String, Vec<PhysicalQueryValue>) {
    let mut out = String::with_capacity(sql.len());
    let mut kept: Vec<PhysicalQueryValue> = Vec::new();
    let bytes = sql.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes[at] != b'?' {
            let ch = sql[at..].chars().next().expect("char boundary");
            out.push(ch);
            at += ch.len_utf8();
            continue;
        }
        let mut end = at + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == at + 1 {
            out.push('?');
            at += 1;
            continue;
        }
        let index: usize = sql[at + 1..end].parse().expect("placeholder digits");
        kept.push(params[index - 1].clone());
        out.push_str(&format!("?{}", kept.len()));
        at = end;
    }
    (out, kept)
}

/// Escape `%`, `_` and the escape character itself for a `LIKE … ESCAPE '\'`
/// pattern that must match literally.
fn like_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// The exclusive upper bound of the half-open range of strings starting with
/// `prefix`, under SQLite's BINARY collation (UTF-8 byte order is code-point
/// order, so incrementing the last scalar value is exactly right).
fn prefix_upper_bound(prefix: &str) -> Option<String> {
    let last = prefix.chars().next_back()?;
    let head: String = prefix[..prefix.len() - last.len_utf8()].to_string();
    let mut next = u32::from(last).checked_add(1)?;
    loop {
        if let Some(ch) = char::from_u32(next) {
            return Some(format!("{head}{ch}"));
        }
        next = next.checked_add(1)?;
    }
}

/// The page-identity fold applied to a PREFIX rather than to a whole name —
/// `eval_page_name`'s `page_prefix_key`, which keeps the trailing boundary slash
/// a namespace prefix carries its whole meaning in.
fn page_prefix_key(text: &str) -> String {
    match text.strip_suffix('/') {
        Some(head) => format!("{}/", refs::page_key(head)),
        None => refs::page_key(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ir::{Source, ViewSettings};
    use crate::query::registry::Registry;

    fn inputs<'a>(registry: &'a Registry) -> LoweringInputs<'a> {
        LoweringInputs {
            today: JournalDate::from_ordinal(20260905),
            registry,
            masked_pages: &[],
            cutoff: None,
        }
    }

    fn lower(filter: Filter, anchor: Anchor) -> SqlQuery {
        let registry = Registry::none().clone();
        let query = Query::new(anchor, filter, Source::Builder);
        match lower_query(&query, &inputs(&registry)) {
            Lowered::Statement(statement) => statement,
            Lowered::Unsupported(reason) => panic!("unexpectedly declined: {reason}"),
        }
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
        // Two conjuncts, each ONE subquery — and the whole filter again for
        // §5.3's parent probe, which is the same tree in the parent's row scope.
        assert_eq!(
            statement.sql.matches("FROM property_atoms").count(),
            4,
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
            statement
                .params
                .iter()
                .any(|value| matches!(value, PhysicalQueryValue::Text(text) if text.contains("drop table"))),
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
        let Lowered::Statement(statement) = lower_query(&query, &inputs(&registry)) else {
            panic!("statement");
        };
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
        let Lowered::Statement(statement) = lower_query(&query, &inputs(&registry)) else {
            panic!("statement");
        };
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
        let Lowered::Statement(statement) = lower_query(&query, &inputs) else {
            panic!("statement");
        };
        assert!(statement.sql.contains("LIMIT ?1"), "{}", statement.sql);
        assert_eq!(statement.params, vec![PhysicalQueryValue::Integer(51)]);
    }

    /// §5.7: the boundedness table is exhaustive, and negation never bounds.
    #[test]
    fn boundedness_follows_the_exhaustive_table() {
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
                positively_bounded(&query.evaluable_filter(), Anchor::Block),
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
                !positively_bounded(&query.evaluable_filter(), Anchor::Block),
                "{source} must NOT be positively bounded"
            );
        }
    }

    /// §5.7: absence lowers to a complement and enumerates it, so `is null` and
    /// a negated leaf bound nothing even though their positive twins do.
    #[test]
    fn absence_and_negation_bound_nothing() {
        let absent = Filter::attr(Attr::Scheduled, CmpOp::IsNotSet, Value::None);
        assert!(!positively_bounded(&absent, Anchor::Block));
        let present = Filter::attr(Attr::Scheduled, CmpOp::IsSet, Value::None);
        assert!(positively_bounded(&present, Anchor::Block));
        assert!(!positively_bounded(&Filter::not(present), Anchor::Block));
    }

    /// §5.9's dispatch, not a divergence: the two `content` operators whose SQL
    /// acceleration is P1-c decline, and the walk answers them.
    #[test]
    fn the_content_operators_p1_c_owns_decline_rather_than_answering_differently() {
        let registry = Registry::none().clone();
        for (op, expected) in [
            (CmpOp::Match, "content match (§5.10, P1-c)"),
            (CmpOp::Regex, "content regexp (§5.10, P1-c)"),
        ] {
            let query = Query::new(
                Anchor::Block,
                Filter::attr(Attr::Content, op, Value::text("needle")),
                Source::Builder,
            );
            assert_eq!(
                lower_query(&query, &inputs(&registry)),
                Lowered::Unsupported(expected)
            );
        }
    }

    /// The walk evaluates a `refs` leaf inside a `children` predicate against
    /// the PARENT's ancestor multiset, which is not `block_path_refs(child)`.
    /// Declining is the only answer that keeps `walk == SQL`.
    #[test]
    fn refs_inside_a_children_predicate_declines() {
        let registry = Registry::none().clone();
        let query = Query::new(
            Anchor::Block,
            Filter::rel(Rel::Children, Quant::Any, Filter::page_ref("Project")),
            Source::Builder,
        );
        assert_eq!(
            lower_query(&query, &inputs(&registry)),
            Lowered::Unsupported("refs nested in a children predicate (walk/SQL closure fork)")
        );
    }

    /// §5.9: the overlay-masked page ids leave the statement, so the masked read
    /// and the overlay walk cannot both answer for one page.
    #[test]
    fn masked_pages_leave_the_statement() {
        let registry = Registry::none().clone();
        let masked = [[7u8; 16]];
        let mut inputs = inputs(&registry);
        inputs.masked_pages = &masked;
        let query = Query::new(Anchor::Block, Filter::page_ref("x"), Source::Builder);
        let Lowered::Statement(statement) = lower_query(&query, &inputs) else {
            panic!("statement");
        };
        assert!(
            statement.sql.contains("b.page_id NOT IN ("),
            "{}",
            statement.sql
        );
        assert!(statement
            .params
            .contains(&PhysicalQueryValue::Blob(vec![7u8; 16])));
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
}
