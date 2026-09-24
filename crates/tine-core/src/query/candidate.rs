//! One candidate planner for every block-text consumer: scalar trigrams for a
//! needle of three or more characters, whole short-word tokens for a one- or
//! two-character needle in a short-word script (ADR 0069).

use crate::query_plan::{QueryExpr, TextField, TextMatchMode};
use crate::search_query::{AndGroup, Matcher};

pub(crate) const INTERACTIVE_VERIFIED_WINDOW: usize = 300;
/// Rows an interactive read visits when no trigram index can drive it (a
/// needle under three characters). Such a scan walks blocks newest first until
/// the verified window fills; a rare or absent pair never fills it, and at
/// 616k blocks the walk took ~1.5 s per keystroke (GH #543 Ctrl-K). At the
/// budget the read stops and reports more matches may exist. ~50 ms at that
/// scale.
pub(crate) const INTERACTIVE_SCAN_BUDGET: usize = 20_000;
#[cfg(test)]
pub(crate) const MEASUREMENT_WINDOW_ALTERNATIVES: [usize; 3] = [100, 300, 1_000];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateMode {
    Exhaustive,
    Interactive { window: usize },
}

impl CandidateMode {
    pub(crate) const fn interactive() -> Self {
        Self::Interactive {
            window: INTERACTIVE_VERIFIED_WINDOW,
        }
    }
}

/// The contentless FTS5 table an index plan reads. Both are keyed by the same
/// page/block rowids, so a consumer only swaps the table name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FtsTable {
    /// `search_fts`: scalar trigrams of the folded visible text.
    Trigram,
    /// `short_word_fts`: unigrams and bigrams of its short-word-script runs
    /// ([`short_word_tokens`]).
    ShortWord,
}

impl FtsTable {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Trigram => "search_fts",
            Self::ShortWord => "short_word_fts",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CandidatePlan {
    Index {
        table: FtsTable,
        match_expression: String,
    },
    Scan,
}

/// A candidate bound: every matching row is in `table MATCH expression`.
type Bound = (FtsTable, String);

pub(crate) fn scalar_trigram_expression(fragment: &str) -> Option<String> {
    if fragment.contains('\0') {
        return None;
    }
    let scalars = fragment.chars().collect::<Vec<_>>();
    if scalars.len() < 3 {
        return None;
    }
    Some(
        scalars
            .windows(3)
            .map(|trigram| {
                let token = trigram.iter().collect::<String>().replace('"', "\"\"");
                format!("\"{token}\"")
            })
            .collect::<Vec<_>>()
            .join(" AND "),
    )
}

/// A one- or two-scalar needle written entirely in a short-word script is one
/// whole token of `short_word_fts` (ADR 0069). The script predicate is the one
/// [`short_word_tokens`] indexes with, so the two cannot disagree.
pub(crate) fn short_word_expression(fragment: &str) -> Option<String> {
    let scalars = fragment.chars().count();
    ((1..=2).contains(&scalars) && fragment.chars().all(is_short_word_script))
        .then(|| format!("\"{fragment}\""))
}

/// The index bound of one needle: trigrams when it has three or more
/// scalars, else its short-word token when it is one.
pub(crate) fn fragment_bound(fragment: &str) -> Option<Bound> {
    scalar_trigram_expression(fragment)
        .map(|expression| (FtsTable::Trigram, expression))
        .or_else(|| {
            short_word_expression(fragment).map(|expression| (FtsTable::ShortWord, expression))
        })
}

/// Conjoined bounds: any one table's bounds already bound the conjunction, so
/// keep the trigram ones when there are any and drop the others.
fn and_bounds(bounds: Vec<Bound>) -> Option<Bound> {
    let table = if bounds.iter().any(|(table, _)| *table == FtsTable::Trigram) {
        FtsTable::Trigram
    } else {
        FtsTable::ShortWord
    };
    let parts = bounds
        .into_iter()
        .filter(|(each, _)| *each == table)
        .map(|(_, bound)| format!("({bound})"))
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| (table, parts.join(" AND ")))
}

/// Disjoined bounds bound the disjunction only when every arm has one, in one
/// table: two tables would need a union, and the arms are rare enough to scan.
fn or_bounds(arms: Vec<Bound>) -> Option<Bound> {
    let table = arms.first()?.0;
    arms.iter().all(|(each, _)| *each == table).then(|| {
        (
            table,
            arms.into_iter()
                .map(|(_, bound)| format!("({bound})"))
                .collect::<Vec<_>>()
                .join(" OR "),
        )
    })
}

fn and_group_bound(group: &AndGroup) -> Option<Bound> {
    and_bounds(
        group
            .iter()
            .filter(|term| !term.negated)
            .filter_map(|term| fragment_bound(&term.text))
            .collect(),
    )
}

fn plan_of(bound: Option<Bound>) -> CandidatePlan {
    match bound {
        Some((table, match_expression)) => CandidatePlan::Index {
            table,
            match_expression,
        },
        None => CandidatePlan::Scan,
    }
}

pub(crate) fn matcher_plan(matcher: &Matcher) -> CandidatePlan {
    let Matcher::Boolean(groups) = matcher else {
        return CandidatePlan::Scan;
    };
    plan_of(
        groups
            .iter()
            .map(and_group_bound)
            .collect::<Option<Vec<_>>>()
            .and_then(or_bounds),
    )
}

fn expr_bound(expr: &QueryExpr) -> Option<Bound> {
    match expr {
        QueryExpr::Text(predicate)
            if predicate.field == TextField::VisibleContent
                && matches!(
                    predicate.mode,
                    TextMatchMode::Contains | TextMatchMode::Phrase
                ) =>
        {
            fragment_bound(&predicate.value)
        }
        QueryExpr::And(children) => and_bounds(children.iter().filter_map(expr_bound).collect()),
        QueryExpr::Or(children) => children
            .iter()
            .map(expr_bound)
            .collect::<Option<Vec<_>>>()
            .and_then(or_bounds),
        QueryExpr::Not(_) | QueryExpr::Never | QueryExpr::Text(_) => None,
    }
}

/// Rows an interactive read of `expr` may visit: [`INTERACTIVE_SCAN_BUDGET`]
/// when no index drives it, unlimited otherwise. A needle in Chinese,
/// Japanese or Korean is exempt: one or two characters are an ordinary word
/// there, and a budgeted scan answered such words from the newest blocks only
/// (0 of 26 matches for one measured word at 10k pages). Since ADR 0069 an
/// all-CJK short needle is index-driven; the exemption still covers what the
/// index cannot drive (a short needle mixing CJK with other letters, or `OR`
/// arms bounded by different tables).
pub(crate) fn interactive_scan_budget(expr: &QueryExpr) -> Option<usize> {
    match expression_plan(expr) {
        CandidatePlan::Scan if !mentions_short_word_script(expr) => Some(INTERACTIVE_SCAN_BUDGET),
        _ => None,
    }
}

fn mentions_short_word_script(expr: &QueryExpr) -> bool {
    match expr {
        QueryExpr::Text(predicate) => predicate.value.chars().any(is_short_word_script),
        QueryExpr::And(children) | QueryExpr::Or(children) => {
            children.iter().any(mentions_short_word_script)
        }
        QueryExpr::Not(child) => mentions_short_word_script(child),
        QueryExpr::Never => false,
    }
}

/// Han, kana and Hangul: scripts written in words of one or two characters.
fn is_short_word_script(ch: char) -> bool {
    matches!(
        ch,
        '\u{1100}'..='\u{11FF}'
            | '\u{2E80}'..='\u{2FDF}'
            | '\u{3040}'..='\u{30FF}'
            | '\u{3130}'..='\u{318F}'
            | '\u{31F0}'..='\u{31FF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{A960}'..='\u{A97F}'
            | '\u{AC00}'..='\u{D7FF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FF66}'..='\u{FFDC}'
            | '\u{20000}'..='\u{3134F}'
    )
}

pub(crate) fn expression_plan(expr: &QueryExpr) -> CandidatePlan {
    plan_of(expr_bound(expr))
}

/// The `short_word_fts` tokens of one entity's folded search text: every
/// scalar and every adjacent pair of each maximal run of short-word-script
/// scalars, space-separated, each once. Empty when the text has no such run,
/// which writes no row (ADR 0069).
pub(crate) fn short_word_tokens(folded: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut tokens = String::new();
    let mut push = |token: &str| {
        if seen.insert(token.to_owned()) {
            if !tokens.is_empty() {
                tokens.push(' ');
            }
            tokens.push_str(token);
        }
    };
    let mut previous: Option<usize> = None;
    for (at, ch) in folded.char_indices() {
        if !is_short_word_script(ch) {
            previous = None;
            continue;
        }
        let end = at + ch.len_utf8();
        if let Some(start) = previous {
            push(&folded[start..end]);
        }
        push(&folded[at..end]);
        previous = Some(at);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigrams_are_scalar_complete_quoted_and_repeated() {
        assert_eq!(
            scalar_trigram_expression("aé界a").as_deref(),
            Some("\"aé界\" AND \"é界a\"")
        );
        assert_eq!(
            scalar_trigram_expression("aaaa").as_deref(),
            Some("\"aaa\" AND \"aaa\"")
        );
        assert!(scalar_trigram_expression("xy").is_none());
        assert!(scalar_trigram_expression("abc\0def").is_none());
    }

    #[test]
    fn an_unbounded_or_arm_forces_a_scan_and_exclusions_never_bound() {
        assert!(matches!(
            matcher_plan(&Matcher::parse("alpha OR xy")),
            CandidatePlan::Scan
        ));
        let CandidatePlan::Index {
            table: FtsTable::Trigram,
            match_expression,
        } = matcher_plan(&Matcher::parse("alpha -beta OR gamma"))
        else {
            panic!("both OR arms have positive indexable bounds");
        };
        assert!(!match_expression.contains("beta"));
        assert!(match_expression.contains(" OR "));
    }

    /// ADR 0069: every scalar and adjacent pair of each CJK run, each once; a
    /// run ends at any other scalar, so no pair spans a gap.
    #[test]
    fn short_word_tokens_are_the_unigrams_and_bigrams_of_each_cjk_run() {
        assert_eq!(short_word_tokens("会议 notes 会议室"), "会 会议 议 议室 室");
        assert_eq!(
            short_word_tokens("東京とソウル"),
            "東 東京 京 京と と とソ ソ ソウ ウ ウル ル"
        );
        assert_eq!(short_word_tokens("회의록"), "회 회의 의 의록 록");
        assert_eq!(
            short_word_tokens("a会b议"),
            "会 议",
            "no bigram across a gap"
        );
        // A decomposed voicing mark is a kana-block scalar: it pairs with its base.
        assert_eq!(short_word_tokens("か\u{3099}"), "か か\u{3099} \u{3099}");
        assert_eq!(short_word_tokens("plain latin text"), "");
    }

    /// Every one- or two-scalar CJK needle of indexed text is one of its
    /// tokens, so the short-word plan never misses a match.
    #[test]
    fn every_short_cjk_substring_is_a_token_of_its_text() {
        for text in ["会议记录", "東京タワーとソウル", "a会b议c", "회의록 정리"]
        {
            let tokens = short_word_tokens(text);
            let tokens = tokens.split(' ').collect::<Vec<_>>();
            let scalars = text.chars().collect::<Vec<_>>();
            for width in 1..=2 {
                for needle in scalars.windows(width) {
                    let needle = needle.iter().collect::<String>();
                    if short_word_expression(&needle).is_some() {
                        assert!(tokens.contains(&needle.as_str()), "{needle:?} in {text:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_short_cjk_needle_is_index_driven_and_mixed_ones_are_not() {
        assert_eq!(
            matcher_plan(&Matcher::parse("会议")),
            CandidatePlan::Index {
                table: FtsTable::ShortWord,
                match_expression: "((\"会议\"))".into(),
            }
        );
        assert_eq!(
            fragment_bound("東").map(|(table, _)| table),
            Some(FtsTable::ShortWord)
        );
        assert_eq!(
            fragment_bound("会议室").map(|(table, _)| table),
            Some(FtsTable::Trigram)
        );
        assert_eq!(
            fragment_bound("a会"),
            None,
            "mixed short needle keeps the scan"
        );
        assert_eq!(fragment_bound("xy"), None);
        // A conjunction keeps the trigram bounds; the short term is verified.
        let CandidatePlan::Index {
            table: FtsTable::Trigram,
            match_expression,
        } = matcher_plan(&Matcher::parse("会 project"))
        else {
            panic!("a conjunction with a trigram term is trigram-bound");
        };
        assert!(!match_expression.contains('会'));
        assert_eq!(
            matcher_plan(&Matcher::parse("会 OR 议")),
            CandidatePlan::Index {
                table: FtsTable::ShortWord,
                match_expression: "((\"会\")) OR ((\"议\"))".into(),
            }
        );
        assert_eq!(
            matcher_plan(&Matcher::parse("会 OR project")),
            CandidatePlan::Scan,
            "arms in two tables are not one bound"
        );
    }

    #[test]
    fn provisional_window_is_one_of_the_retained_measurement_points() {
        assert!(MEASUREMENT_WINDOW_ALTERNATIVES.contains(&INTERACTIVE_VERIFIED_WINDOW));
        assert_eq!(
            CandidateMode::interactive(),
            CandidateMode::Interactive { window: 300 }
        );
    }
}
