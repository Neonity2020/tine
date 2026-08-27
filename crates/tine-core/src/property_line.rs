//! Shared Markdown property-line recognition for `tine-core` and `lsdoc-wasm`.
//!
//! Keep this module dependency-free: the browser WASM wrapper includes this
//! exact source alongside `logbook.rs` instead of compiling all of tine-core.

/// lsdoc's parser-space set (`Parsers.is_space`: space, tab, SUB, FF).
fn mldoc_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | 0x1a | 0x0c)
}

fn skip_mldoc_spaces(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && mldoc_space(bytes[i]) {
        i += 1;
    }
    &s[i..]
}

/// lsdoc's property-value trim set: space, tab, newline, CR and FF, but not SUB.
fn trim_property_value(s: &str) -> &str {
    fn trim(byte: u8) -> bool {
        matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
    }
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut end = bytes.len();
    while start < end && trim(bytes[start]) {
        start += 1;
    }
    while end > start && trim(bytes[end - 1]) {
        end -= 1;
    }
    &s[start..end]
}

/// The one hand-rolled Markdown property-line recognizer (`key:: value`).
///
/// This is transcribed from lsdoc's `markdown_property_line` for callers that
/// need a cheap borrowed view rather than an allocated document AST:
///
/// - leading lsdoc parser spaces (space/tab/SUB/FF) are skipped;
/// - the key is non-empty and contains no colon, parser space, CR or LF;
/// - `::` must be followed by a literal space, unless the remainder consists
///   only of parser spaces (an empty value).
pub fn parse_property_line(line: &str) -> Option<(&str, &str)> {
    let rest = skip_mldoc_spaces(line);
    let pos = rest.find("::")?;
    let key = &rest[..pos];
    if key.is_empty()
        || key
            .as_bytes()
            .iter()
            .any(|&b| b == b':' || mldoc_space(b) || b == b'\n' || b == b'\r')
    {
        return None;
    }
    let value = &rest[pos + 2..];
    if let Some(value) = value.strip_prefix(' ') {
        let value = skip_mldoc_spaces(value);
        return Some((key, trim_property_value(value)));
    }
    value
        .as_bytes()
        .iter()
        .all(|&b| mldoc_space(b))
        .then_some((key, ""))
}

#[cfg(test)]
mod tests {
    use super::parse_property_line;

    #[test]
    fn matches_lsdoc_property_boundaries() {
        assert_eq!(
            parse_property_line("\tlogseq.order-list-type::  number \t"),
            Some(("logseq.order-list-type", "number"))
        );
        assert_eq!(
            parse_property_line("unicode.klíč:: hodnota"),
            Some(("unicode.klíč", "hodnota"))
        );
        assert_eq!(parse_property_line("empty:: \t"), Some(("empty", "")));
        assert_eq!(parse_property_line("key::value"), None);
        assert_eq!(parse_property_line("a b:: value"), None);
    }
}
