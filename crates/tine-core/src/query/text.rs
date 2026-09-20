//! Query-source guards and exact text helpers shared by search and structured
//! queries.

use std::path::Path;

use crate::doc::DocBlock;
use crate::vocab::Format;

/// Query source crosses several boundaries (live macros, native IPC, static
/// publication, and export). Keep one shared ceiling so no caller can make the
/// parser proportional to an unbounded graph-authored string.
pub const QUERY_SOURCE_MAX_BYTES: usize = 64 * 1024;
pub(crate) const QUERY_NESTING_MAX: usize = 64;

pub fn query_source_within_limit(source: &str) -> bool {
    source.len() <= QUERY_SOURCE_MAX_BYTES
}

/// Is this query body an advanced datalog query we don't support?
pub fn is_advanced(query_src: &str) -> bool {
    let s = query_src.trim_start();
    s.starts_with("[:find") || s.contains(":where") || s.contains(":find")
}

/// Iterative, string/comment-aware guard before either recursive DSL parser.
/// Count parentheses because those are the only delimiters that construct
/// recursive predicates; brackets/braces are scanned iteratively as data.
pub fn query_nesting_within_limit(source: &str) -> bool {
    let semicolon_comments = is_advanced(source);
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut in_comment = false;
    for byte in source.bytes() {
        if in_comment {
            if byte == b'\n' {
                in_comment = false;
            }
            continue;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b';' if semicolon_comments => in_comment = true,
            b'"' => in_string = true,
            b'(' => {
                depth = depth.saturating_add(1);
                if depth > QUERY_NESTING_MAX {
                    return false;
                }
            }
            b')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    true
}

/// The two exact visible-text forms one `DocBlock::projection` derives.
/// Keeping them together lets candidate verification reuse the fold instead
/// of parsing once and then folding the returned visible text again.
pub(crate) struct VisibleTextProjection {
    pub(crate) visible: String,
    pub(crate) visible_lower: String,
}

pub(crate) fn visible_projection_from_raw_path(raw: &str, path: &str) -> VisibleTextProjection {
    let is_org = Format::from_path(Path::new(path)) == Format::Org;
    let block = DocBlock::preamble(raw, is_org);
    let projection = block.projection();
    VisibleTextProjection {
        visible: projection.visible.clone(),
        visible_lower: projection.visible_lower.clone(),
    }
}

pub(crate) fn visible_from_raw_path(raw: &str, path: &str) -> String {
    visible_projection_from_raw_path(raw, path).visible
}

/// SQL frame consumed by `QueryRankPrograms::bind_pair` and other fixed
/// two-text callbacks. The UTF-8 byte length keeps colons, NULs and multibyte
/// text unambiguous.
pub(crate) fn framed_pair_sql(left: &str, right: &str) -> String {
    format!("CAST(length(CAST({left} AS BLOB)) AS TEXT) || ':' || {left} || {right}")
}

/// SQL frame consumed by `QueryRankPrograms::bind_byte_len_and_text` when a
/// callback needs the left text's UTF-8 byte length but not the text itself.
pub(crate) fn framed_byte_len_and_text_sql(left: &str, right: &str) -> String {
    format!("CAST(length(CAST({left} AS BLOB)) AS TEXT) || ':' || {right}")
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
                let Some(next) = chars.next() else {
                    // SQLite `LIKE ... ESCAPE '\\'` rejects an unpaired
                    // trailing escape instead of discarding it.
                    return false;
                };
                literal.push(next);
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

#[cfg(test)]
mod tests {
    use super::like_matches;

    #[test]
    fn like_matches_sql_escape_semantics() {
        assert!(!like_matches("abc", "abc\\"));
        assert!(!like_matches("", "\\"));
        assert!(like_matches("a_b", "a\\_b"));
        assert!(!like_matches("axb", "a\\_b"));
        assert!(like_matches("100%", "100\\%"));
        assert!(!like_matches("1000", "100\\%"));
    }
}
