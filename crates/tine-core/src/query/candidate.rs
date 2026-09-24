//! One scalar-trigram candidate planner for every block-text consumer.

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CandidatePlan {
    Index { match_expression: String },
    Scan,
}

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

fn and_group_expression(group: &AndGroup) -> Option<String> {
    let bounds = group
        .iter()
        .filter(|term| !term.negated)
        .filter_map(|term| scalar_trigram_expression(&term.text))
        .map(|bound| format!("({bound})"))
        .collect::<Vec<_>>();
    (!bounds.is_empty()).then(|| bounds.join(" AND "))
}

pub(crate) fn matcher_plan(matcher: &Matcher) -> CandidatePlan {
    let Matcher::Boolean(groups) = matcher else {
        return CandidatePlan::Scan;
    };
    let arms = groups
        .iter()
        .map(and_group_expression)
        .collect::<Option<Vec<_>>>();
    match arms {
        Some(arms) if !arms.is_empty() => CandidatePlan::Index {
            match_expression: arms
                .into_iter()
                .map(|arm| format!("({arm})"))
                .collect::<Vec<_>>()
                .join(" OR "),
        },
        _ => CandidatePlan::Scan,
    }
}

fn expr_bound(expr: &QueryExpr) -> Option<String> {
    match expr {
        QueryExpr::Text(predicate)
            if predicate.field == TextField::VisibleContent
                && matches!(
                    predicate.mode,
                    TextMatchMode::Contains | TextMatchMode::Phrase
                ) =>
        {
            scalar_trigram_expression(&predicate.value)
        }
        QueryExpr::And(children) => {
            let bounds = children.iter().filter_map(expr_bound).collect::<Vec<_>>();
            (!bounds.is_empty()).then(|| {
                bounds
                    .into_iter()
                    .map(|bound| format!("({bound})"))
                    .collect::<Vec<_>>()
                    .join(" AND ")
            })
        }
        QueryExpr::Or(children) => children
            .iter()
            .map(expr_bound)
            .collect::<Option<Vec<_>>>()
            .map(|bounds| {
                bounds
                    .into_iter()
                    .map(|bound| format!("({bound})"))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            }),
        QueryExpr::Not(_) | QueryExpr::Never | QueryExpr::Text(_) => None,
    }
}

/// Rows an interactive read of `expr` may visit: [`INTERACTIVE_SCAN_BUDGET`]
/// when no trigram index drives it, unlimited otherwise. A needle in Chinese,
/// Japanese or Korean is exempt: one or two characters are an ordinary word
/// there, and a budgeted scan answered such words from the newest blocks only
/// (0 of 26 matches for one measured word at 10k pages).
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
    match expr_bound(expr) {
        Some(match_expression) => CandidatePlan::Index { match_expression },
        None => CandidatePlan::Scan,
    }
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
        let CandidatePlan::Index { match_expression } =
            matcher_plan(&Matcher::parse("alpha -beta OR gamma"))
        else {
            panic!("both OR arms have positive indexable bounds");
        };
        assert!(!match_expression.contains("beta"));
        assert!(match_expression.contains(" OR "));
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
