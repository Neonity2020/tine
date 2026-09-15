//! Advanced-query (`#+BEGIN_QUERY`) reading for publish: find a block's query
//! payload and read its EDN map with a small balanced-form scanner, no full EDN parser.

use super::*;

pub(super) struct BeginQuery {
    pub(super) title: Option<String>,
    pub(super) query: String,
}

pub(super) enum BeginQueryInspection {
    Supported(BeginQuery),
    Unsupported,
}

/// Return the authored payload only when `raw` is exactly one terminated
/// `#+BEGIN_QUERY` container. This mirrors `WHOLE_BEGIN_QUERY` in the frontend:
/// spaces/tabs are accepted around the delimiters, all three line endings are
/// accepted, and neither a prefix nor a suffix may share the block.
fn whole_begin_query_payload(raw: &str) -> Option<&str> {
    const BEGIN: &str = "#+BEGIN_QUERY";
    const END: &str = "#+END_QUERY";

    let bytes = raw.as_bytes();
    let mut begin = 0;
    while matches!(bytes.get(begin), Some(b' ' | b'\t')) {
        begin += 1;
    }
    let begin_end = begin.checked_add(BEGIN.len())?;
    if !raw.get(begin..begin_end)?.eq_ignore_ascii_case(BEGIN) {
        return None;
    }
    let mut payload_start = begin_end;
    while matches!(bytes.get(payload_start), Some(b' ' | b'\t')) {
        payload_start += 1;
    }
    payload_start += match bytes.get(payload_start) {
        Some(b'\r') if bytes.get(payload_start + 1) == Some(&b'\n') => 2,
        Some(b'\r' | b'\n') => 1,
        _ => return None,
    };

    // JavaScript's terminal `$` accepts one final line ending. Account for it
    // before locating the closing-delimiter line.
    let mut closing_end = raw.len();
    if raw[..closing_end].ends_with("\r\n") {
        closing_end -= 2;
    } else if matches!(bytes.get(closing_end.wrapping_sub(1)), Some(b'\r' | b'\n')) {
        closing_end -= 1;
    }
    while closing_end > payload_start && matches!(bytes[closing_end - 1], b' ' | b'\t') {
        closing_end -= 1;
    }

    let mut newline_start = closing_end;
    while newline_start > payload_start && !matches!(bytes[newline_start - 1], b'\r' | b'\n') {
        newline_start -= 1;
    }
    if newline_start == payload_start {
        return None;
    }
    let closing_line_start = newline_start;
    newline_start -= 1;
    if bytes[newline_start] == b'\n'
        && newline_start > payload_start
        && bytes[newline_start - 1] == b'\r'
    {
        newline_start -= 1;
    }
    let mut delimiter_start = closing_line_start;
    while delimiter_start < closing_end && matches!(bytes[delimiter_start], b' ' | b'\t') {
        delimiter_start += 1;
    }
    if !raw
        .get(delimiter_start..closing_end)?
        .eq_ignore_ascii_case(END)
    {
        return None;
    }
    Some(&raw[payload_start..newline_start])
}

fn skip_edn_trivia(source: &str, mut from: usize) -> usize {
    while from < source.len() {
        let c = source[from..].chars().next().expect("in bounds");
        if c.is_whitespace() || c == ',' {
            from += c.len_utf8();
        } else if c == ';' {
            from += 1;
            while from < source.len() && !matches!(source.as_bytes()[from], b'\n' | b'\r') {
                from += 1;
            }
        } else {
            break;
        }
    }
    from
}

fn edn_string_end(source: &str, from: usize) -> Option<usize> {
    let mut at = from + 1;
    while at < source.len() {
        let c = source[at..].chars().next()?;
        if c == '\\' {
            at += 1;
            let escaped = source.get(at..)?.chars().next()?;
            at += escaped.len_utf8();
        } else if c == '"' {
            return Some(at + 1);
        } else {
            at += c.len_utf8();
        }
    }
    None
}

fn edn_balanced_end(source: &str, from: usize) -> Option<usize> {
    fn closer(c: char) -> Option<char> {
        match c {
            '(' => Some(')'),
            '[' => Some(']'),
            '{' => Some('}'),
            _ => None,
        }
    }

    let first = source.get(from..)?.chars().next()?;
    let mut stack = vec![closer(first)?];
    let mut at = from + first.len_utf8();
    while at < source.len() {
        let c = source[at..].chars().next()?;
        if c == '"' {
            at = edn_string_end(source, at)?;
            continue;
        }
        if c == ';' {
            while at < source.len() && !matches!(source.as_bytes()[at], b'\n' | b'\r') {
                at += 1;
            }
            continue;
        }
        if let Some(close) = closer(c) {
            stack.push(close);
        } else if matches!(c, ')' | ']' | '}') {
            if stack.pop() != Some(c) {
                return None;
            }
            if stack.is_empty() {
                return Some(at + c.len_utf8());
            }
        }
        at += c.len_utf8();
    }
    None
}

fn edn_token_end(source: &str, mut from: usize) -> usize {
    while from < source.len() {
        let c = source[from..].chars().next().expect("in bounds");
        if c.is_whitespace() || c == ',' || matches!(c, '(' | ')' | '[' | ']' | '{' | '}') {
            break;
        }
        from += c.len_utf8();
    }
    from
}

fn edn_value_end(source: &str, from: usize) -> Option<usize> {
    match source.get(from..)?.chars().next()? {
        '"' => edn_string_end(source, from),
        '(' | '[' | '{' => edn_balanced_end(source, from),
        _ => {
            let end = edn_token_end(source, from);
            (end > from).then_some(end)
        }
    }
}

fn unquote_begin_query_title(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek().copied() {
                Some('\n' | '\r') | None => out.push(c),
                Some(_) => out.push(chars.next().expect("peeked character")),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn has_edn_keyword(source: &str, keyword: &str) -> bool {
    source.match_indices(keyword).any(|(start, _)| {
        source[start + keyword.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
    })
}

fn parse_begin_query_map(payload: &str) -> Option<BeginQuery> {
    let source = payload.trim();
    if !source.starts_with('{') {
        return None;
    }
    let map_end = edn_balanced_end(source, 0)?;
    if skip_edn_trivia(source, map_end) != source.len() {
        return None;
    }

    let mut title = None;
    let mut query = None;
    let mut at = 1;
    while at < map_end - 1 {
        at = skip_edn_trivia(source, at);
        if at >= map_end - 1 {
            break;
        }
        if source.as_bytes()[at] != b':' {
            return None;
        }
        let key_end = edn_token_end(source, at);
        let key = &source[at..key_end];
        at = skip_edn_trivia(source, key_end);
        let value_end = edn_value_end(source, at)?;
        if value_end > map_end - 1 {
            return None;
        }
        let value = &source[at..value_end];
        match key {
            ":query" if query.is_none() => query = Some(value.to_string()),
            ":query" => return None,
            ":title" if title.is_none() && value.starts_with('"') => {
                title = Some(unquote_begin_query_title(&value[1..value.len() - 1]));
            }
            ":title" => return None,
            _ => {}
        }
        at = value_end;
    }

    let query = query?;
    if !query.starts_with('[')
        || !has_edn_keyword(&query, ":find")
        || !has_edn_keyword(&query, ":where")
    {
        return None;
    }
    Some(BeginQuery { title, query })
}

/// Inspect raw authored text and use the parsed AST only as a confirmation that
/// the whole container is the one custom/query node the frontend would dispatch.
/// EDN is always sliced from `raw`; rendered/flattened AST text is never rebuilt.
pub(super) fn inspect_begin_query(raw: &str, blocks: &[Block]) -> Option<BeginQueryInspection> {
    let payload = whole_begin_query_payload(raw)?;
    let body = if matches!(
        blocks.first(),
        Some(Block::Bullet { .. } | Block::Heading { .. })
    ) {
        &blocks[1..]
    } else {
        blocks
    };
    if !matches!(body, [Block::Custom { name, .. }] if name.eq_ignore_ascii_case("query")) {
        return Some(BeginQueryInspection::Unsupported);
    }
    Some(match parse_begin_query_map(payload) {
        Some(query) => BeginQueryInspection::Supported(query),
        None => BeginQueryInspection::Unsupported,
    })
}
