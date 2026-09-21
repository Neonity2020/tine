//! The advanced (datalog) query's `:where` clauses as text: the clause
//! scanner, and (GH #542) the DataScript attribute patterns lowered from it —
//! `[?b :block/marker "TODO"]`, value variables narrowed by a predicate, the
//! block's journal page, and `(not [..])` of one of those. The clause heads
//! (`task`, `between`, ...) are lowered in the parent module.

use super::ir::{Attr, CmpOp, Filter, Quant, Rel, Value};
use super::AdvancedInput;

/// Collect balanced `(...)`/`[...]` groups at the top level of `s` (string-aware),
/// stopping at the first top-level *closing* bracket (so scanning after `:where`
/// halts at the find-vector's `]` rather than swallowing `:inputs`).
pub(super) fn scan_groups(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i] as char;
        if c == ')' || c == ']' || c == '}' {
            break;
        }
        // EDN/DataScript line comment (`; …` to end of line) — skip it so example
        // clauses written inside a `;;` hint are NOT parsed as real groups.
        if c == ';' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == '(' || c == '[' {
            let start = i;
            let mut depth = 0;
            let mut in_str = false;
            while i < b.len() {
                let ch = b[i] as char;
                if in_str {
                    if ch == '\\' {
                        i += 2;
                        continue;
                    }
                    if ch == '"' {
                        in_str = false;
                    }
                } else if ch == ';' {
                    // Comment inside a group body (between clauses) — skip to EOL.
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                } else if ch == '"' {
                    in_str = true;
                } else if ch == '(' || ch == '[' || ch == '{' {
                    depth += 1;
                } else if ch == ')' || ch == ']' || ch == '}' {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                i += 1;
            }
            out.push(s[start..i.min(s.len())].to_string());
            continue;
        }
        i += 1;
    }
    out
}

/// The clause groups in the `:where` section.
pub(super) fn where_groups(src: &str) -> Vec<String> {
    match src.find(":where") {
        Some(idx) => scan_groups(&src[idx + ":where".len()..]),
        None => Vec::new(),
    }
}

/// Top-level items of an EDN body, in order: strings, balanced collections
/// (`[..]`, `(..)`, `{..}`, `#{..}`) and bare tokens. Comments are skipped.
fn edn_items(body: &str) -> Vec<&str> {
    let b = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() || c == b',' {
            i += 1;
            continue;
        }
        if c == b';' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        if c == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
        } else if matches!(c, b'[' | b'(' | b'{') || (c == b'#' && b.get(i + 1) == Some(&b'{')) {
            if c == b'#' {
                i += 1;
            }
            let mut depth = 0usize;
            let mut in_str = false;
            while i < b.len() {
                let ch = b[i];
                if in_str {
                    if ch == b'\\' {
                        i += 2;
                        continue;
                    }
                    if ch == b'"' {
                        in_str = false;
                    }
                } else if ch == b'"' {
                    in_str = true;
                } else if matches!(ch, b'[' | b'(' | b'{') {
                    depth += 1;
                } else if matches!(ch, b']' | b')' | b'}') {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                i += 1;
            }
        } else {
            while i < b.len()
                && !b[i].is_ascii_whitespace()
                && !matches!(b[i], b'[' | b']' | b'(' | b')' | b'{' | b'}' | b'"' | b',')
            {
                i += 1;
            }
            if i == start {
                i += 1; // a stray closer is its own item
            }
        }
        out.push(&body[start..i.min(body.len())]);
    }
    out
}

/// The body of a `[..]` or `(..)` item, or `None` for anything else.
fn edn_body(item: &str, open: char, close: char) -> Option<&str> {
    item.trim().strip_prefix(open)?.strip_suffix(close)
}

/// The variable the query returns: `:find (pull ?b [*])` or `:find ?b`.
pub(super) fn advanced_find_var(src: &str) -> Option<String> {
    let at = src.find(":find")?;
    let first = *edn_items(&src[at + ":find".len()..]).first()?;
    let var = match edn_body(first, '(', ')') {
        Some(call) => {
            let items = edn_items(call);
            if items.first() != Some(&"pull") {
                return None;
            }
            *items.get(1)?
        }
        None => first,
    };
    var.starts_with('?').then(|| var.to_string())
}

/// Top-level `where` clauses with single-branch wrappers opened: an `(and ..)`
/// contributes its clauses, and an `(or ..)`/`(or-join [..] ..)` with exactly
/// one branch IS that branch. Both are the same query; only the shape differs.
pub(super) fn flatten_single_branch_groups(groups: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for group in groups {
        let Some(body) = edn_body(&group, '(', ')') else {
            out.push(group);
            continue;
        };
        let items = edn_items(body);
        let branches: Option<Vec<String>> = match items.first().copied() {
            Some("and") => Some(items[1..].iter().map(|item| item.to_string()).collect()),
            Some("or") if items.len() == 2 => Some(vec![items[1].to_string()]),
            Some("or-join") if items.len() == 3 && edn_body(items[1], '[', ']').is_some() => {
                Some(vec![items[2].to_string()])
            }
            _ => None,
        };
        match branches {
            Some(branches) => out.extend(flatten_single_branch_groups(branches)),
            None => out.push(group),
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdvTarget {
    Block,
    Page,
}

/// A DataScript attribute Tine indexes, on the result block or on its page.
fn advanced_attribute(target: AdvTarget, attribute: &str) -> Option<(Attr, &'static str)> {
    match (target, attribute) {
        (AdvTarget::Block, ":block/marker") => Some((Attr::Task, "marker")),
        (AdvTarget::Block, ":block/priority") => Some((Attr::Priority, "priority")),
        (AdvTarget::Block, ":block/scheduled") => Some((Attr::Scheduled, "scheduled")),
        (AdvTarget::Block, ":block/deadline") => Some((Attr::Deadline, "deadline")),
        (AdvTarget::Page, ":block/journal?") => Some((Attr::Journal, "journal-page")),
        (AdvTarget::Page, ":block/journal-day") => Some((Attr::Day, "journal-day")),
        _ => None,
    }
}

/// A literal operand in the type DataScript stores for `attr`: marker and
/// priority are strings, scheduled/deadline/journal-day are yyyymmdd
/// integers, `journal?` a boolean. A mismatched literal never matches in
/// Logseq either, so it is not lowered to something that might.
fn advanced_literal(
    attr: Attr,
    token: &str,
    inputs: &std::collections::HashMap<String, AdvancedInput>,
) -> Option<Value> {
    match attr {
        Attr::Task | Attr::Priority => {
            let text = token.strip_prefix('"')?.strip_suffix('"')?;
            (!text.contains('\\')).then(|| Value::text(text))
        }
        Attr::Scheduled | Attr::Deadline | Attr::Day => {
            let number = match inputs.get(token) {
                Some(AdvancedInput::Date(value)) => *value,
                Some(AdvancedInput::Page(_)) => return None,
                None => token.parse::<i64>().ok()?,
            };
            Some(Value::Number {
                number: number as f64,
            })
        }
        Attr::Journal => match token {
            "true" => Some(Value::Bool { value: true }),
            "false" => Some(Value::Bool { value: false }),
            _ => None,
        },
        _ => None,
    }
}

fn advanced_on_target(target: AdvTarget, filter: Filter) -> Filter {
    match target {
        AdvTarget::Block => filter,
        AdvTarget::Page => Filter::rel(Rel::Page, Quant::Any, filter),
    }
}

/// How many times `var` appears as a whole token in `text`.
fn advanced_var_uses(text: &str, var: &str) -> usize {
    let boundary = |c: Option<char>| c.is_none_or(|c| c.is_whitespace() || "()[]{},\"".contains(c));
    text.match_indices(var)
        .filter(|(at, _)| {
            boundary(text[..*at].chars().next_back())
                && boundary(text[at + var.len()..].chars().next())
        })
        .count()
}

/// GH #542: lower the DataScript attribute patterns Logseq's own examples and
/// users' `:default-queries` are written in — `[?b :block/marker "TODO"]`, a
/// value variable narrowed by `[(contains? #{..} ?m)]` or `[(<= ?d ?today)]`,
/// the block's page with `:block/journal?` / `:block/journal-day`, and
/// `(not [?b :block/scheduled ?d])`.
///
/// Like the two lowerers above this is not a join engine. It lowers a clause
/// only when its meaning is a filter on the returned block alone: the subject
/// is the `:find` variable (or its page), and a value variable is used by
/// nothing but the one pattern that binds it and the predicates lowered with
/// it. Every other clause is left for `parse_adv_group` to report as
/// unsupported.
pub(super) fn lower_attribute_patterns(
    groups: &[String],
    taken: &std::collections::HashSet<usize>,
    find_var: Option<&str>,
    inputs: &std::collections::HashMap<String, AdvancedInput>,
) -> (
    std::collections::HashMap<usize, (Filter, &'static str)>,
    std::collections::HashSet<usize>,
) {
    let mut lowered = std::collections::HashMap::new();
    let mut consumed = std::collections::HashSet::new();
    let Some(block) = find_var else {
        return (lowered, consumed);
    };
    let triple = |group: &str| -> Option<[String; 3]> {
        let items = edn_items(edn_body(group, '[', ']')?);
        (items.len() == 3 && items[0].starts_with('?')).then(|| {
            [
                items[0].to_string(),
                items[1].to_string(),
                items[2].to_string(),
            ]
        })
    };
    let active: Vec<usize> = (0..groups.len()).filter(|i| !taken.contains(i)).collect();

    // The returned block's page variable(s).
    let mut pages = std::collections::HashSet::new();
    for &index in &active {
        if let Some([subject, attribute, object]) = triple(&groups[index]) {
            if subject == block && attribute == ":block/page" && object.starts_with('?') {
                pages.insert(object);
                consumed.insert(index);
            }
        }
    }
    let target_of = |subject: &str| {
        if subject == block {
            Some(AdvTarget::Block)
        } else if pages.contains(subject) {
            Some(AdvTarget::Page)
        } else {
            None
        }
    };

    // Patterns: a literal is a filter; a variable is a binding the predicates
    // below may narrow.
    let mut bindings: std::collections::HashMap<String, (usize, AdvTarget, Attr, &'static str)> =
        std::collections::HashMap::new();
    for &index in &active {
        let Some([subject, attribute, object]) = triple(&groups[index]) else {
            continue;
        };
        let Some(target) = target_of(&subject) else {
            continue;
        };
        let Some((attr, label)) = advanced_attribute(target, &attribute) else {
            continue;
        };
        if object.starts_with('?') {
            if attr != Attr::Journal && !bindings.contains_key(&object) {
                bindings.insert(object, (index, target, attr, label));
            }
        } else if let Some(value) = advanced_literal(attr, &object, inputs) {
            lowered.insert(
                index,
                (
                    advanced_on_target(target, Filter::attr(attr, CmpOp::Eq, value)),
                    label,
                ),
            );
        }
    }

    // Predicates over exactly one bound variable.
    let mut predicates: std::collections::HashMap<String, Vec<(usize, Filter)>> =
        std::collections::HashMap::new();
    for &index in &active {
        let Some(call) = edn_body(&groups[index], '[', ']')
            .map(edn_items)
            .filter(|items| items.len() == 1)
            .and_then(|items| edn_body(items[0], '(', ')'))
        else {
            continue;
        };
        let items = edn_items(call);
        let lowered_predicate = (|| -> Option<(String, Filter)> {
            let (symbol, left, right) = match items.as_slice() {
                [symbol, left, right] => (*symbol, *left, *right),
                _ => return None,
            };
            if symbol == "contains?" {
                let (_, _, attr, _) = bindings.get(right)?;
                let set = left.strip_prefix('#').and_then(|s| edn_body(s, '{', '}'))?;
                let values = edn_items(set)
                    .into_iter()
                    .map(|item| advanced_literal(*attr, item, inputs))
                    .collect::<Option<Vec<_>>>()?;
                return Some((
                    right.to_string(),
                    Filter::attr(*attr, CmpOp::In, Value::List { items: values }),
                ));
            }
            let cmp = |flipped: bool| -> Option<CmpOp> {
                Some(match (symbol, flipped) {
                    ("=", _) => CmpOp::Eq,
                    ("not=", _) => CmpOp::NotEq,
                    ("<", false) | (">", true) => CmpOp::Lt,
                    ("<=", false) | (">=", true) => CmpOp::Le,
                    (">", false) | ("<", true) => CmpOp::Gt,
                    (">=", false) | ("<=", true) => CmpOp::Ge,
                    _ => return None,
                })
            };
            let (var, operand, flipped) = match (bindings.get(left), bindings.get(right)) {
                (Some(_), None) => (left, right, false),
                (None, Some(_)) => (right, left, true),
                _ => return None,
            };
            let (_, _, attr, _) = bindings.get(var)?;
            let op = cmp(flipped)?;
            let ordered = matches!(attr, Attr::Scheduled | Attr::Deadline | Attr::Day);
            if !(ordered || op == CmpOp::Eq || op == CmpOp::NotEq) {
                return None;
            }
            let value = advanced_literal(*attr, operand, inputs)?;
            Some((var.to_string(), Filter::attr(*attr, op, value)))
        })();
        if let Some((var, filter)) = lowered_predicate {
            predicates.entry(var).or_default().push((index, filter));
        }
    }

    // A binding lowers only when nothing else in the query uses its variable.
    let everything = groups.join("\n");
    for (var, (index, target, attr, label)) in bindings {
        let narrowing = predicates.remove(&var).unwrap_or_default();
        if advanced_var_uses(&everything, &var) != 1 + narrowing.len() {
            continue;
        }
        lowered.insert(
            index,
            (
                advanced_on_target(target, Filter::attr(attr, CmpOp::IsSet, Value::None)),
                label,
            ),
        );
        for (predicate, filter) in narrowing {
            lowered.insert(predicate, (advanced_on_target(target, filter), "predicate"));
        }
    }

    // `(not [?b :block/scheduled ?d])`: no such value, where `?d` is bound by
    // no clause outside a `not` (a variable the outer query never binds is
    // local to each `not`, as the reporter's two `(not [.. ?d])` rely on).
    let is_not = |group: &str| {
        edn_body(group, '(', ')')
            .map(edn_items)
            .is_some_and(|items| items.first() == Some(&"not"))
    };
    for &index in &active {
        let Some(items) = edn_body(&groups[index], '(', ')').map(edn_items) else {
            continue;
        };
        let [head, clause] = items.as_slice() else {
            continue;
        };
        if *head != "not" {
            continue;
        }
        let Some([subject, attribute, object]) = triple(clause) else {
            continue;
        };
        let Some(target) = target_of(&subject) else {
            continue;
        };
        let Some((attr, _)) = advanced_attribute(target, &attribute) else {
            continue;
        };
        let inner = if object.starts_with('?') {
            let outside = groups
                .iter()
                .enumerate()
                .filter(|(other, group)| *other != index && !is_not(group))
                .map(|(_, group)| advanced_var_uses(group, &object))
                .sum::<usize>();
            if attr == Attr::Journal || outside != 0 || advanced_var_uses(clause, &object) != 1 {
                continue;
            }
            Filter::attr(attr, CmpOp::IsSet, Value::None)
        } else {
            let Some(value) = advanced_literal(attr, &object, inputs) else {
                continue;
            };
            Filter::attr(attr, CmpOp::Eq, value)
        };
        lowered.insert(
            index,
            (Filter::not(advanced_on_target(target, inner)), "not"),
        );
    }
    (lowered, consumed)
}
