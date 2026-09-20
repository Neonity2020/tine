//! Exact visible-text derivation shared by search and structured queries.

use std::path::Path;

use crate::doc::DocBlock;
use crate::vocab::Format;

pub(crate) fn visible_from_raw_path(raw: &str, path: &str) -> String {
    let is_org = Format::from_path(Path::new(path)) == Format::Org;
    DocBlock::preamble(raw, is_org).projection().visible.clone()
}

/// SQL frame consumed by `QueryRankPrograms::bind_pair` and other fixed
/// two-text callbacks. The UTF-8 byte length keeps colons, NULs and multibyte
/// text unambiguous.
pub(crate) fn framed_pair_sql(left: &str, right: &str) -> String {
    format!("CAST(length(CAST({left} AS BLOB)) AS TEXT) || ':' || {left} || {right}")
}

/// SQL `LIKE` over an already-folded haystack. `%` matches any run, `_` one
/// scalar, and `\` escapes the following scalar.
pub(crate) fn like_matches(haystack: &str, pattern: &str) -> bool {
    #[derive(Debug)]
    enum Part {
        Literal(String),
        Any,
        One,
    }

    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    literal.push(next);
                }
            }
            '%' | '_' => {
                if !literal.is_empty() {
                    parts.push(Part::Literal(std::mem::take(&mut literal)));
                }
                parts.push(if ch == '%' { Part::Any } else { Part::One });
            }
            other => literal.push(other),
        }
    }
    if !literal.is_empty() {
        parts.push(Part::Literal(literal));
    }

    let haystack = haystack.chars().collect::<Vec<_>>();
    fn matches(parts: &[Part], haystack: &[char], at: usize) -> bool {
        match parts.first() {
            None => at == haystack.len(),
            Some(Part::One) => at < haystack.len() && matches(&parts[1..], haystack, at + 1),
            Some(Part::Any) => {
                (at..=haystack.len()).any(|next| matches(&parts[1..], haystack, next))
            }
            Some(Part::Literal(text)) => {
                let literal = text.chars().collect::<Vec<_>>();
                haystack.get(at..at.saturating_add(literal.len())) == Some(literal.as_slice())
                    && matches(&parts[1..], haystack, at + literal.len())
            }
        }
    }
    matches(&parts, &haystack, 0)
}
