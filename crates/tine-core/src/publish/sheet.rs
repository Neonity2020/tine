//! Sheets in a published site: the static, read-only table, board and grid views
//! of sheet blocks, with the column, aggregate and arithmetic logic they share.

use super::*;

// ================= Sheets: static read-only views =================
//
// A block carrying `tine.view:: table|board|grid` is an interactive sheet in
// the app (`src/sheet/*` + SheetTable/SheetBoard/SheetGrid); its plain-text
// twin is a nested outline that Logseq reads as ordinary bullets with harmless
// `tine.*` properties. Before this section, the export published exactly that
// plain-text twin — heading, visible config chips, nested bullets — so the
// Guide's Sheets pages lost the presentation entirely. A static document
// cannot offer the editing surface, but it must still carry the view's
// MEANING: this section renders each supported sheet as read-only HTML that
// preserves content, column/field labels, board grouping, grid positions, and
// links. Field discovery, labels, group ordering, and aggregate semantics
// mirror the app (`src/sheet/{config,fields,aggregate}.ts`), narrowed to what
// a static export computes: no editing affordances; formula columns evaluate
// only the arithmetic subset below — anything richer shows its stored
// definition in the column header instead of a wrong value.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SheetView {
    Table,
    Board,
    Grid,
}

/// A sheet column identity — the static mirror of the app's `FieldId`
/// (`src/sheet/fields.ts`). Display labels match `fieldLabel`.
#[derive(Clone, PartialEq, Eq, Debug)]
enum SheetField {
    State,
    Priority,
    Scheduled,
    Deadline,
    Tags,
    Page,
    Prop(String),
    Formula(String),
}

impl SheetField {
    fn label(&self) -> &str {
        match self {
            SheetField::State => "State",
            SheetField::Priority => "Priority",
            SheetField::Scheduled => "Scheduled",
            SheetField::Deadline => "Deadline",
            SheetField::Tags => "Tags",
            SheetField::Page => "Page",
            SheetField::Prop(name) | SheetField::Formula(name) => name,
        }
    }
    /// Machine identity used by `tine.col-aggregates` keys (`prop:estimate`,
    /// `state`, `formula:effort`, …).
    fn id(&self) -> String {
        match self {
            SheetField::State => "state".into(),
            SheetField::Priority => "priority".into(),
            SheetField::Scheduled => "scheduled".into(),
            SheetField::Deadline => "deadline".into(),
            SheetField::Tags => "tags".into(),
            SheetField::Page => "page".into(),
            SheetField::Prop(name) => format!("prop:{name}"),
            SheetField::Formula(name) => format!("formula:{name}"),
        }
    }
}

/// The sheet config carried by a view-owning block's `tine.*` properties
/// (static mirror of `sheetConfig`; widths/filters have no static meaning and
/// are ignored).
pub(super) struct SheetConfig {
    pub(super) view: SheetView,
    group_by: Option<String>,
    header: bool,
    /// `(aggregate key, fn)` in declared order, e.g. `("prop:estimate", "sum")`.
    aggregates: Vec<(String, String)>,
    /// Declared `tine.fields` columns in declaration order.
    declared: Vec<SheetDecl>,
    /// `(name, expression)` formula columns in declared order.
    formulas: Vec<(String, String)>,
    /// Which of those columns a QUERY-backed face shows, resolved by the ONE
    /// column resolver (`query::view::resolve_query_columns`) rather than by a
    /// second precedence written here. A children-backed sheet ignores it:
    /// selecting visible columns is a query-face question, and ordinary sheets
    /// keep their existing behaviour exactly.
    columns: crate::query::view::QueryColumns,
    /// Which field a QUERY-backed board groups by, resolved by the ONE grouping
    /// resolver (`query::view::resolve_query_grouping`). A children-backed
    /// board ignores it and keeps reading `group_by` exactly as it does today:
    /// the new key and its legacy reinterpretation are a query-face question
    /// (P5B).
    grouping: crate::query::view::QueryGrouping,
}

/// One `tine.fields` declaration: its column, whether cells render as
/// checkboxes, and (for `enum:` types) the declared value order — which is
/// also a board's column order when grouped by that field.
struct SheetDecl {
    field: SheetField,
    checkbox: bool,
    enum_values: Vec<String>,
}

const SHEET_AGGREGATE_FNS: &[&str] = &[
    "sum",
    "average",
    "median",
    "min",
    "max",
    "range",
    "stddev",
    "earliest",
    "latest",
    "empty",
    "filled",
    "unique",
    "checked",
    "unchecked",
    "count",
];

fn sheet_aggregate_label(f: &str) -> &str {
    match f {
        "sum" => "Sum",
        "average" => "Average",
        "median" => "Median",
        "min" => "Min",
        "max" => "Max",
        "range" => "Range",
        "stddev" => "Stddev",
        "earliest" => "Earliest",
        "latest" => "Latest",
        "empty" => "Empty",
        "filled" => "Filled",
        "unique" => "Unique",
        "checked" => "Checked",
        "unchecked" => "Unchecked",
        _ => "Count",
    }
}

/// The app hides these keys from rendered output (`isRenderHiddenProp`,
/// case-insensitive) and never offers them as sheet columns: internal ids,
/// styling hints, table config. `tine.*` config is likewise never a column.
fn sheet_hidden_key(key: &str, user_hidden: &[String]) -> bool {
    let norm = crate::doc::property_key_norm(key);
    if norm.starts_with("tine.") || norm.starts_with("logseq.") {
        return true;
    }
    const HIDDEN: &[&str] = &[
        "id",
        "collapsed",
        "hl-page",
        "hl-color",
        "hl-type",
        "ls-type",
        "background-color",
        "heading",
        "title",
        "filters",
        "created-at",
        "updated-at",
        "last-modified-at",
        "query-table",
        "query-properties",
        "query-sort-by",
        "query-sort-desc",
    ];
    HIDDEN.contains(&norm.as_str())
        || user_hidden
            .iter()
            .any(|k| crate::doc::property_key_norm(k) == norm)
}

pub(super) fn sheet_config(props: &[(String, String)]) -> Option<SheetConfig> {
    let mut cfg = SheetConfig {
        view: SheetView::Table,
        group_by: None,
        header: false,
        aggregates: Vec::new(),
        declared: Vec::new(),
        formulas: Vec::new(),
        columns: crate::query::view::resolve_query_columns(props),
        grouping: crate::query::view::resolve_query_grouping(
            props,
            &crate::query::ir::ViewSettings::default(),
        ),
    };
    let mut seen_declared: HashSet<String> = HashSet::new();
    let mut view: Option<SheetView> = None;
    for (raw_key, raw_value) in props {
        let key = crate::doc::property_key_norm(raw_key);
        let value = raw_value.trim();
        match key.as_str() {
            "tine.view" => {
                view = match value.to_ascii_lowercase().as_str() {
                    "table" => Some(SheetView::Table),
                    "board" => Some(SheetView::Board),
                    "grid" => Some(SheetView::Grid),
                    _ => None,
                };
            }
            "tine.group-by" => {
                cfg.group_by = if value.is_empty() {
                    None
                } else {
                    Some(value.into())
                }
            }
            "tine.header" => cfg.header = value.eq_ignore_ascii_case("true"),
            "tine.col-aggregates" => {
                for part in value.split(';') {
                    if let Some((k, f)) = part.split_once('=') {
                        let (k, f) = (k.trim(), f.trim().to_ascii_lowercase());
                        if !k.is_empty() && SHEET_AGGREGATE_FNS.contains(&f.as_str()) {
                            cfg.aggregates.push((k.into(), f));
                        }
                    }
                }
            }
            "tine.fields" => {
                for part in value.split(';') {
                    let Some((name, token)) = part.split_once('=') else {
                        continue;
                    };
                    let name = name.trim();
                    let token = token.trim();
                    if name.is_empty() || !seen_declared.insert(name.into()) {
                        continue;
                    }
                    let builtin = match name {
                        "state" => Some(SheetField::State),
                        "priority" => Some(SheetField::Priority),
                        "scheduled" => Some(SheetField::Scheduled),
                        "deadline" => Some(SheetField::Deadline),
                        "tags" => Some(SheetField::Tags),
                        "page" => Some(SheetField::Page),
                        _ => None,
                    };
                    if let Some(field) = builtin {
                        // Builtin declarations are `name=name` (mirrors parseFields).
                        if token == name {
                            cfg.declared.push(SheetDecl {
                                field,
                                checkbox: false,
                                enum_values: Vec::new(),
                            });
                        }
                        continue;
                    }
                    let enum_values: Vec<String> = if let Some(list) = token.strip_prefix("enum:") {
                        list.split(',')
                            .map(|v| v.trim().to_string())
                            .filter(|v| !v.is_empty())
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let valid = !enum_values.is_empty()
                        || matches!(
                            token,
                            "text" | "number" | "date" | "datetime" | "checkbox" | "list" | "ref"
                        );
                    if valid {
                        cfg.declared.push(SheetDecl {
                            field: SheetField::Prop(name.into()),
                            checkbox: token == "checkbox",
                            enum_values,
                        });
                    }
                }
            }
            _ => {
                if let Some(name) = key.strip_prefix("tine.formula.") {
                    let name = name.trim();
                    // Mirrors the app's `formulaNameValid` (/^[a-z0-9-]+$/).
                    if !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    {
                        cfg.formulas.push((name.into(), value.into()));
                    }
                }
            }
        }
    }
    view.map(|v| {
        cfg.view = v;
        cfg
    })
}

/// One sheet row: the source block (page bullet or hydrated query result).
struct SheetRow<'a> {
    block: &'a DocBlock,
    /// The result's page (query-backed rows only; needed for the Page column).
    page: Option<&'a str>,
}

impl<'a> SheetRow<'a> {
    /// The plain-text value used for cells (pre-decoration), aggregates, and
    /// formula operands — mirrors `readField`/`groupKeysForBlock` text.
    fn field_text(&self, field: &SheetField, formulas: &[(String, String)]) -> String {
        let b = self.block;
        match field {
            SheetField::State => b.marker().unwrap_or("").into(),
            SheetField::Priority => b.priority().unwrap_or("").into(),
            SheetField::Scheduled => b.scheduled().unwrap_or("").into(),
            SheetField::Deadline => b.deadline().unwrap_or("").into(),
            SheetField::Tags => b.tags().join(" "),
            SheetField::Page => self.page.unwrap_or("").into(),
            SheetField::Prop(name) => b
                .properties()
                .into_iter()
                .find(|(k, _)| {
                    crate::doc::property_key_norm(k) == crate::doc::property_key_norm(name)
                })
                .map(|(_, v)| v)
                .unwrap_or_default(),
            SheetField::Formula(name) => {
                let expr = formulas
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, e)| e.as_str());
                let Some(expr) = expr else {
                    return String::new();
                };
                let Some(ast) = parse_sheet_arith(expr) else {
                    return String::new();
                };
                eval_sheet_arith(&ast, self, formulas)
                    .map(|v| format!("{v}"))
                    .unwrap_or_default()
            }
        }
    }
}

// ---- Formula columns: the arithmetic subset ----
//
// The full formula language (src/sheet/formula: lexer, parser, eval over
// numbers/text/booleans/dates/durations/lists, visual-builder faces) is an
// interactive-editing engine; cloning it into the export would duplicate a
// whole subsystem. The static subset — number literals, row-field references,
// `+ - * /`, parentheses, unary minus — covers derived numeric columns like
// the Guide's `rating * 2`. Anything outside it is not a failure: the column
// keeps its label and stored definition, cells stay empty (the app's
// null-propagation for unresolvable rows behaves the same).

enum SheetArith {
    Num(f64),
    Field(String),
    Neg(Box<SheetArith>),
    Bin(u8, Box<SheetArith>, Box<SheetArith>),
}

fn parse_sheet_arith(src: &str) -> Option<SheetArith> {
    fn ws(s: &[u8], mut i: usize) -> usize {
        while i < s.len() && s[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    }
    fn number(s: &[u8], mut i: usize) -> Option<(f64, usize)> {
        let start = i;
        while i < s.len()
            && (s[i].is_ascii_digit()
                || s[i] == b'.'
                || (matches!(s[i], b'e' | b'E') && i > start && s[i - 1].is_ascii_digit()))
        {
            i += 1;
        }
        // Optional exponent tail (`e-3`, `E+2`).
        if i > start && matches!(s[i - 1], b'e' | b'E') {
            let mut j = i;
            if matches!(s.get(j), Some(b'+') | Some(b'-')) {
                j += 1;
            }
            let digits = j;
            while j < s.len() && s[j].is_ascii_digit() {
                j += 1;
            }
            if j == digits {
                i = start + (i - start) - 1; // bare 'e' is not part of the number
            } else {
                i = j;
            }
        }
        let text = std::str::from_utf8(&s[start..i]).ok()?;
        text.parse::<f64>().ok().map(|n| (n, i))
    }
    fn ident(s: &[u8], mut i: usize) -> Option<(String, usize)> {
        let start = i;
        while i < s.len() && (s[i].is_ascii_alphanumeric() || matches!(s[i], b'_' | b'.' | b'-')) {
            i += 1;
        }
        if i == start || !s[start].is_ascii_alphabetic() && s[start] != b'_' {
            return None;
        }
        Some((String::from_utf8_lossy(&s[start..i]).into_owned(), i))
    }
    fn factor(s: &[u8], i: usize) -> Option<(SheetArith, usize)> {
        let i = ws(s, i);
        match s.get(i)? {
            b'(' => {
                let (inner, j) = expr(s, i + 1)?;
                let j = ws(s, j);
                if s.get(j) != Some(&b')') {
                    return None;
                }
                Some((inner, j + 1))
            }
            b'-' => {
                let (inner, j) = factor(s, i + 1)?;
                Some((SheetArith::Neg(Box::new(inner)), j))
            }
            c if c.is_ascii_digit() || *c == b'.' => {
                let (n, j) = number(s, i)?;
                Some((SheetArith::Num(n), j))
            }
            c if c.is_ascii_alphabetic() || *c == b'_' => {
                let (name, j) = ident(s, i)?;
                Some((SheetArith::Field(name), j))
            }
            _ => None,
        }
    }
    fn term(s: &[u8], i: usize) -> Option<(SheetArith, usize)> {
        let (mut left, mut i) = factor(s, i)?;
        loop {
            let j = ws(s, i);
            match s.get(j) {
                Some(op @ (b'*' | b'/')) => {
                    let (right, k) = factor(s, j + 1)?;
                    left = SheetArith::Bin(*op, Box::new(left), Box::new(right));
                    i = k;
                }
                _ => return Some((left, i)),
            }
        }
    }
    fn expr(s: &[u8], i: usize) -> Option<(SheetArith, usize)> {
        let (mut left, mut i) = term(s, i)?;
        loop {
            let j = ws(s, i);
            match s.get(j) {
                Some(op @ (b'+' | b'-')) => {
                    let (right, k) = term(s, j + 1)?;
                    left = SheetArith::Bin(*op, Box::new(left), Box::new(right));
                    i = k;
                }
                _ => return Some((left, i)),
            }
        }
    }
    let s = src.as_bytes();
    let (ast, end) = expr(s, 0)?;
    if ws(s, end) == s.len() {
        Some(ast)
    } else {
        None
    }
}

fn eval_sheet_arith(
    ast: &SheetArith,
    row: &SheetRow,
    formulas: &[(String, String)],
) -> Option<f64> {
    match ast {
        SheetArith::Num(n) => Some(*n),
        SheetArith::Field(name) => {
            let builtin = match name.as_str() {
                "state" => Some(SheetField::State),
                "priority" => Some(SheetField::Priority),
                "scheduled" => Some(SheetField::Scheduled),
                "deadline" => Some(SheetField::Deadline),
                "tags" => Some(SheetField::Tags),
                "page" => Some(SheetField::Page),
                _ => None,
            };
            let field = builtin.unwrap_or_else(|| SheetField::Prop(name.clone()));
            js_parse_float(&row.field_text(&field, formulas))
        }
        SheetArith::Neg(a) => Some(-eval_sheet_arith(a, row, formulas)?),
        SheetArith::Bin(op, a, b) => {
            let (a, b) = (
                eval_sheet_arith(a, row, formulas)?,
                eval_sheet_arith(b, row, formulas)?,
            );
            let v = match op {
                b'+' => a + b,
                b'-' => a - b,
                b'*' => a * b,
                _ => {
                    if b == 0.0 {
                        return None;
                    }
                    a / b
                }
            };
            if v.is_finite() {
                Some(v)
            } else {
                None
            }
        }
    }
}

// ---- Aggregates (static port of src/sheet/aggregate.ts) ----

/// Leading-number parse with JS `parseFloat` prefix semantics ("8abc" → 8).
fn js_parse_float(text: &str) -> Option<f64> {
    let s = text.trim_start();
    let bytes = s.as_bytes();
    let mut end = 0;
    let mut seen_digit = false;
    let mut seen_dot = false;
    while end < bytes.len() {
        match bytes[end] {
            b'0'..=b'9' => {
                seen_digit = true;
                end += 1;
            }
            b'.' if !seen_dot => {
                seen_dot = true;
                end += 1;
            }
            b'+' | b'-' if end == 0 => end += 1,
            b'e' | b'E' if seen_digit => {
                let mut j = end + 1;
                if matches!(bytes.get(j), Some(b'+') | Some(b'-')) {
                    j += 1;
                }
                let digits = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j == digits {
                    break;
                }
                end = j;
                break;
            }
            _ => break,
        }
    }
    if !seen_digit {
        return None;
    }
    s[..end].parse::<f64>().ok().filter(|n| n.is_finite())
}

/// `Math.round(n * 1000) / 1000` with JS number-string formatting (`-0` → 0).
fn format_sheet_number(n: f64) -> String {
    let rounded = (n * 1000.0).round() / 1000.0;
    let rounded = if rounded == 0.0 { 0.0 } else { rounded };
    format!("{rounded}")
}

/// `^<?(\d{4}-\d{2}-\d{2})` — the TS `isoDatePrefix`.
fn sheet_iso_date_prefix(text: &str) -> Option<&str> {
    let s = text.strip_prefix('<').unwrap_or(text);
    let b = s.as_bytes();
    if b.len() >= 10
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
    {
        Some(&s[..10])
    } else {
        None
    }
}

fn sheet_is_checked_text(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "done" | "true" | "yes" | "checked" | "x" | "[x]"
    )
}

fn sheet_with_skipped(text: String, skipped: usize) -> String {
    if skipped == 0 {
        return text;
    }
    if text.is_empty() {
        format!("({skipped} skipped)")
    } else {
        format!("{text} ({skipped} skipped)")
    }
}

fn sheet_aggregate_numbers(f: &str, values: &[String]) -> String {
    let mut skipped = 0;
    let mut nums: Vec<f64> = Vec::new();
    for value in values {
        match js_parse_float(value) {
            Some(n) => nums.push(n),
            None => skipped += 1,
        }
    }
    if nums.is_empty() {
        return sheet_with_skipped("0".into(), skipped);
    }
    nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let v = match f {
        "sum" => nums.iter().sum::<f64>(),
        "average" => nums.iter().sum::<f64>() / nums.len() as f64,
        "median" => {
            let mid = nums.len() / 2;
            if nums.len() % 2 == 1 {
                nums[mid]
            } else {
                (nums[mid - 1] + nums[mid]) / 2.0
            }
        }
        "min" => nums[0],
        "max" => nums[nums.len() - 1],
        "range" => nums[nums.len() - 1] - nums[0],
        _ => {
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            let variance =
                nums.iter().map(|n| (n - mean) * (n - mean)).sum::<f64>() / nums.len() as f64;
            variance.sqrt()
        }
    };
    sheet_with_skipped(format_sheet_number(v), skipped)
}

fn sheet_aggregate_dates(f: &str, values: &[String]) -> String {
    let mut skipped = 0;
    let mut dates: Vec<&str> = Vec::new();
    for value in values {
        match sheet_iso_date_prefix(value.trim()) {
            Some(d) => dates.push(d),
            None => skipped += 1,
        }
    }
    dates.sort_unstable();
    if dates.is_empty() {
        return sheet_with_skipped(String::new(), skipped);
    }
    match f {
        "earliest" => sheet_with_skipped(dates[0].into(), skipped),
        "latest" => sheet_with_skipped(dates[dates.len() - 1].into(), skipped),
        _ => sheet_with_skipped(
            format!("{} - {}", dates[0], dates[dates.len() - 1]),
            skipped,
        ),
    }
}

fn sheet_aggregate(f: &str, values: &[String]) -> String {
    match f {
        "empty" => format!("{}", values.iter().filter(|v| v.trim().is_empty()).count()),
        "filled" | "count" => format!("{}", values.iter().filter(|v| !v.trim().is_empty()).count()),
        "unique" => {
            let set: HashSet<&str> = values
                .iter()
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
                .collect();
            format!("{}", set.len())
        }
        "checked" => format!(
            "{}",
            values.iter().filter(|v| sheet_is_checked_text(v)).count()
        ),
        "unchecked" => format!(
            "{}",
            values.iter().filter(|v| !sheet_is_checked_text(v)).count()
        ),
        "earliest" | "latest" => sheet_aggregate_dates(f, values),
        "range" => {
            let has_date = values
                .iter()
                .any(|v| sheet_iso_date_prefix(v.trim()).is_some());
            let numeric_non_dates = values
                .iter()
                .filter(|v| {
                    sheet_iso_date_prefix(v.trim()).is_none() && js_parse_float(v).is_some()
                })
                .count();
            if has_date && numeric_non_dates == 0 {
                sheet_aggregate_dates(f, values)
            } else {
                sheet_aggregate_numbers(f, values)
            }
        }
        _ => sheet_aggregate_numbers(f, values),
    }
}

// ---- Column derivation + rendering ----

/// Fields observed in the row data, mirroring `fieldIdsForBlocks`: builtins
/// that occur at all, in fixed order; then non-hidden property keys in
/// first-seen order; the Page column only for query-backed row sets.
fn sheet_observed_fields(
    rows: &[SheetRow],
    query_backed: bool,
    user_hidden: &[String],
) -> Vec<SheetField> {
    let mut out = Vec::new();
    let mut props: Vec<SheetField> = Vec::new();
    let mut prop_seen: HashSet<String> = HashSet::new();
    let (mut state, mut priority, mut scheduled, mut deadline, mut tags) =
        (false, false, false, false, false);
    for row in rows {
        let b = row.block;
        state |= b.marker().is_some();
        priority |= b.priority().is_some();
        scheduled |= b.scheduled().is_some();
        deadline |= b.deadline().is_some();
        tags |= !b.tags().is_empty();
        for (key, _) in b.properties() {
            if sheet_hidden_key(&key, user_hidden) {
                continue;
            }
            let norm = crate::doc::property_key_norm(&key);
            if prop_seen.insert(norm) {
                props.push(SheetField::Prop(key));
            }
        }
    }
    if state {
        out.push(SheetField::State);
    }
    if priority {
        out.push(SheetField::Priority);
    }
    if scheduled {
        out.push(SheetField::Scheduled);
    }
    if deadline {
        out.push(SheetField::Deadline);
    }
    if tags {
        out.push(SheetField::Tags);
    }
    out.append(&mut props);
    if query_backed {
        out.push(SheetField::Page);
    }
    out
}

/// One `tine.columns` token as a sheet column (P5A). The six sheet builtins
/// keep their identity; every other string is an ordinary property name, so it
/// maps to `prop:<name>` HERE, at the renderer — the property bytes stay the
/// bare name the author wrote, in both formats and in both languages.
fn sheet_field_for_column(name: &str) -> SheetField {
    match name {
        "state" => SheetField::State,
        "priority" => SheetField::Priority,
        "scheduled" => SheetField::Scheduled,
        "deadline" => SheetField::Deadline,
        "tags" => SheetField::Tags,
        "page" => SheetField::Page,
        _ => SheetField::Prop(name.into()),
    }
}

/// A CANONICAL grouping `FieldId` as a sheet column identity (P5B). The
/// resolver has already decided what the token means, so this is a pure
/// spelling map with no fallback of its own — the `prop:`/`formula:` prefixes
/// are guaranteed to carry a nonempty name, and anything else is one of the six
/// builtins. Mirrors `src/sheet/fields.ts::isFieldId`'s reading.
fn sheet_field_for_group(field: &str) -> SheetField {
    match field {
        "state" => SheetField::State,
        "priority" => SheetField::Priority,
        "scheduled" => SheetField::Scheduled,
        "deadline" => SheetField::Deadline,
        "tags" => SheetField::Tags,
        "page" => SheetField::Page,
        other => match other.strip_prefix("formula:") {
            Some(name) => SheetField::Formula(name.into()),
            None => SheetField::Prop(other.strip_prefix("prop:").unwrap_or(other).into()),
        },
    }
}

/// The static `columns` memo: title (implicit) + declared fields + formula
/// fields + observed fields not already present — in that order.
///
/// A QUERY-backed face then applies the block's `tine.columns` selection over
/// that list — **after** the schema/type lookup, so a selected column keeps the
/// checkbox rendering and enum order its `tine.fields` declaration gave it, and
/// a field definition is never lost merely because its column is not shown. A
/// selected column that no row carries is kept and renders empty cells; the
/// implicit title column and the row anchors are outside the selection and stay
/// reachable. No selection (absent, or an explicit empty/invalid one) leaves the
/// default column set exactly as it is today.
fn sheet_columns(
    cfg: &SheetConfig,
    rows: &[SheetRow],
    query_backed: bool,
    user_hidden: &[String],
) -> Vec<(SheetField, bool)> {
    let mut out: Vec<(SheetField, bool)> = Vec::new();
    let mut ids: HashSet<String> = HashSet::new();
    for decl in &cfg.declared {
        if ids.insert(decl.field.id()) {
            out.push((decl.field.clone(), decl.checkbox));
        }
    }
    for (name, _) in &cfg.formulas {
        let field = SheetField::Formula(name.clone());
        if ids.insert(field.id()) {
            out.push((field, false));
        }
    }
    for field in sheet_observed_fields(rows, query_backed, user_hidden) {
        if ids.insert(field.id()) {
            out.push((field, false));
        }
    }
    if !query_backed {
        return out;
    }
    let crate::query::view::QueryColumns::Named(selection) = &cfg.columns else {
        return out;
    };
    let mut selected: Vec<(SheetField, bool)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for column in selection {
        let field = sheet_field_for_column(column.as_str());
        let id = field.id();
        if !seen.insert(id.clone()) {
            continue;
        }
        match out.iter().find(|(known, _)| known.id() == id) {
            Some((known, checkbox)) => selected.push((known.clone(), *checkbox)),
            None => selected.push((field, false)),
        }
    }
    selected
}

/// Rendered cell for one row/field: header-facet chrome for state/priority,
/// tag/page links, inline-decorated property text (so `[[refs]]` and `#tags`
/// stay links), checkbox glyphs for declared checkbox fields, computed formula
/// numbers.
fn sheet_cell_html(
    row: &SheetRow,
    field: &SheetField,
    checkbox: bool,
    cfg: &SheetConfig,
    ctx: &Ctx,
) -> String {
    let b = row.block;
    match field {
        SheetField::State => {
            let mut cell = String::new();
            if b.marker().is_some() {
                emit_header_facets(b.marker(), None, &mut cell);
            }
            cell
        }
        SheetField::Priority => {
            let mut cell = String::new();
            if b.priority().is_some() {
                emit_header_facets(None, b.priority(), &mut cell);
            }
            cell
        }
        SheetField::Tags => b
            .tags()
            .iter()
            .map(|t| {
                format!(
                    "<a class=\"tag\" href=\"{}.html\">#{}</a>",
                    page_slug(ctx, t),
                    esc(t)
                )
            })
            .collect::<Vec<_>>()
            .join(" "),
        SheetField::Page => {
            let page = row.page.unwrap_or("");
            if page.is_empty() {
                String::new()
            } else {
                format!(
                    "<a class=\"ref\" href=\"{}.html\">{}</a>",
                    page_slug(ctx, page),
                    esc(page)
                )
            }
        }
        SheetField::Formula(_) => {
            let text = row.field_text(field, &cfg.formulas);
            esc(&text)
        }
        SheetField::Scheduled | SheetField::Deadline => esc(&row.field_text(field, &cfg.formulas)),
        SheetField::Prop(_) => {
            if checkbox {
                let text = row.field_text(field, &cfg.formulas);
                return if sheet_is_checked_text(&text) {
                    "<span class=\"task-checkbox checked\"></span>".into()
                } else {
                    "<span class=\"task-checkbox\"></span>".into()
                };
            }
            let text = row.field_text(field, &cfg.formulas);
            decorate(&lsdoc::render_html(&body_blocks(&text), &md_opts()), ctx, 0)
        }
    }
}

/// Anchor + search-index entry for a sheet row/card, in the SAME emission
/// lock-step as top-level blocks (a row's `<tr id>`/`<li id>` is the
/// deep-link target for its search hit).
fn sheet_row_anchor(
    b: &DocBlock,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    slug: &str,
    title: &str,
) -> String {
    let anchor = match block_id(&b.raw) {
        Some(id) => id,
        None => {
            let a = format!("b{}", *counter);
            *counter += 1;
            a
        }
    };
    let text = ast_plain_text(&body_blocks(&b.raw));
    if !text.is_empty() {
        index.push(json!({"slug": slug, "title": title, "anchor": anchor, "text": text}));
    }
    anchor
}

/// Everything the sheet renderers need from the outer render pass.
pub(super) struct SheetEmit<'a, 'b> {
    pub(super) ctx: &'a Ctx<'b>,
    pub(super) slug: &'a str,
    pub(super) title: &'a str,
    pub(super) opts: PrintOpts,
    /// Whether rows get stable anchors + index entries (children-backed rows
    /// of this page). Query-hydrated rows mirror query results: no anchors.
    pub(super) anchors: bool,
}

fn render_sheet_table(
    cfg: &SheetConfig,
    rows: &[SheetRow],
    emit: &SheetEmit,
    query_backed: bool,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    out: &mut String,
) {
    let user_hidden: &[String] = match emit.ctx.graph {
        Some(graph) => &graph.config.block_hidden_properties,
        None => &[],
    };
    let columns = sheet_columns(cfg, rows, query_backed, user_hidden);
    out.push_str("<div class=\"sheet-scroll\"><table class=\"sheet-table\"><thead><tr><th></th>");
    for (field, _) in &columns {
        let mut label = field.label().to_string();
        // A formula column the static subset cannot evaluate keeps its
        // definition visible instead of computing a wrong value.
        if let SheetField::Formula(name) = field {
            let expr = cfg
                .formulas
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, e)| e.as_str())
                .unwrap_or("");
            if parse_sheet_arith(expr).is_none() && !expr.is_empty() {
                label = format!("{} ({})", name, expr);
            }
        }
        out.push_str(&format!("<th>{}</th>", esc(&label)));
    }
    out.push_str("</tr></thead>");
    let aggregate = !cfg.aggregates.is_empty();
    if aggregate {
        out.push_str("<tfoot><tr><td></td>");
        for (field, _) in &columns {
            let target = cfg
                .aggregates
                .iter()
                .find(|(key, _)| key == &field.id() || key == &field.label());
            match target {
                Some((_, f)) => {
                    let values: Vec<String> = rows
                        .iter()
                        .map(|row| row.field_text(field, &cfg.formulas))
                        .collect();
                    out.push_str(&format!(
                        "<td><span class=\"sheet-agg-label\">{}</span> {}</td>",
                        esc(sheet_aggregate_label(f)),
                        esc(&sheet_aggregate(f, &values))
                    ));
                }
                None => out.push_str("<td></td>"),
            }
        }
        out.push_str("</tr></tfoot>");
    }
    out.push_str("<tbody>");
    for row in rows {
        if emit.anchors {
            let anchor = sheet_row_anchor(row.block, counter, index, emit.slug, emit.title);
            out.push_str(&format!("<tr id=\"{}\">", esc_attr(&anchor)));
        } else {
            out.push_str("<tr>");
        }
        // Title cell: the row's own content (facets + inline formatting),
        // with any sub-bullets kept as a nested outline.
        out.push_str("<td>");
        let mut title_cell = String::new();
        emit_header_facets(row.block.marker(), row.block.priority(), &mut title_cell);
        title_cell.push_str(&decorate(
            &lsdoc::render_html(&body_blocks(&row.block.raw), &md_opts()),
            emit.ctx,
            0,
        ));
        out.push_str(&title_cell);
        if !row.block.children.is_empty() {
            out.push_str("<ul class=\"sheet-children\">");
            for c in &row.block.children {
                render_block(
                    c, out, emit.ctx, emit.slug, emit.title, counter, index, emit.opts,
                );
            }
            out.push_str("</ul>");
        }
        out.push_str("</td>");
        for (field, checkbox) in &columns {
            out.push_str(&format!(
                "<td>{}</td>",
                sheet_cell_html(row, field, *checkbox, cfg, emit.ctx)
            ));
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table></div>");
}

/// Board column order, mirroring `buildColumns` (SheetBoard.tsx): state →
/// workflow order + other present markers; priority → A/B/C; enum → declared
/// values then extras; tags/other fields → first-seen; null group last.
fn render_sheet_board(
    cfg: &SheetConfig,
    rows: &[SheetRow],
    emit: &SheetEmit,
    workflow: crate::Workflow,
    query_backed: bool,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    out: &mut String,
) {
    // **A query board asks the ONE grouping resolver; an ordinary sheet board is
    // untouched** (P5B). `tine.group-field::` and the view-aware reinterpretation
    // of the legacy token are a query-face question — a children-backed board
    // keeps reading `tine.group-by::` exactly as it does today, so nothing about
    // an ordinary published sheet changes.
    //
    // `None` here is an explicit "no grouping": one ungrouped column, never a
    // silently reinstated `state` default.
    let group_field: Option<SheetField> = if query_backed {
        match &cfg.grouping {
            crate::query::view::QueryGrouping::Field(field) => {
                Some(sheet_field_for_group(field.as_str()))
            }
            crate::query::view::QueryGrouping::Cleared => None,
            crate::query::view::QueryGrouping::Unset => Some(SheetField::State),
        }
    } else {
        let raw = cfg.group_by.as_deref().unwrap_or("state");
        let norm = crate::doc::property_key_norm(raw);
        Some(match norm.as_str() {
            "state" => SheetField::State,
            "priority" => SheetField::Priority,
            "scheduled" => SheetField::Scheduled,
            "deadline" => SheetField::Deadline,
            "tags" => SheetField::Tags,
            "page" => SheetField::Page,
            other => {
                if let Some(name) = other.strip_prefix("prop:") {
                    SheetField::Prop(name.into())
                } else if let Some(name) = other.strip_prefix("formula:") {
                    SheetField::Formula(name.into())
                } else {
                    SheetField::Prop(raw.into())
                }
            }
        })
    };
    // Group keys per row (tags give multi-membership).
    let keys_for = |row: &SheetRow| -> Vec<Option<String>> {
        let Some(group_field) = &group_field else {
            // Explicitly ungrouped: every row lands in the single column.
            return vec![None];
        };
        match group_field {
            SheetField::Tags => {
                let tags = row.block.tags();
                if tags.is_empty() {
                    vec![None]
                } else {
                    tags.into_iter().map(Some).collect()
                }
            }
            SheetField::Formula(name) => {
                let text = row.field_text(&SheetField::Formula(name.clone()), &cfg.formulas);
                vec![if text.is_empty() { None } else { Some(text) }]
            }
            field => {
                let text = row.field_text(field, &cfg.formulas);
                vec![if text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                }]
            }
        }
    };
    let mut buckets: Vec<(Option<String>, Vec<&SheetRow>)> = Vec::new();
    let mut first_keys: Vec<Option<String>> = Vec::new();
    for row in rows {
        for key in keys_for(row) {
            if !first_keys.contains(&key) {
                first_keys.push(key.clone());
            }
            match buckets.iter_mut().find(|(k, _)| k == &key) {
                Some((_, bucket)) => bucket.push(row),
                None => buckets.push((key, vec![row])),
            }
        }
    }
    let has_null = first_keys.contains(&None);
    let order: Vec<Option<String>> = match group_field.as_ref() {
        None => Vec::new(),
        Some(SheetField::State) => {
            let standard: [&str; 3] = match workflow {
                crate::Workflow::Todo => ["TODO", "DOING", "DONE"],
                crate::Workflow::Now => ["LATER", "NOW", "DONE"],
            };
            let mut order: Vec<Option<String>> =
                standard.iter().map(|m| Some(m.to_string())).collect();
            for m in crate::doc::MARKERS {
                let key = Some((*m).to_string());
                if standard.contains(m) || !first_keys.contains(&key) {
                    continue;
                }
                order.push(key);
            }
            order
        }
        Some(SheetField::Priority) => ["A", "B", "C"]
            .iter()
            .map(|m| Some((*m).to_string()))
            .collect(),
        Some(SheetField::Prop(name)) => {
            // A declared enum order wins; first-seen extras follow.
            let norm = crate::doc::property_key_norm(name);
            let mut order: Vec<Option<String>> = cfg
                .declared
                .iter()
                .find(|decl| {
                    matches!(&decl.field, SheetField::Prop(p)
                        if crate::doc::property_key_norm(p) == norm)
                })
                .map(|decl| decl.enum_values.iter().map(|v| Some(v.clone())).collect())
                .unwrap_or_default();
            for key in &first_keys {
                if key.is_some() && !order.contains(key) {
                    order.push(key.clone());
                }
            }
            order
        }
        Some(_) => {
            let mut order: Vec<Option<String>> = Vec::new();
            for key in &first_keys {
                if key.is_some() && !order.contains(key) {
                    order.push(key.clone());
                }
            }
            order
        }
    };
    let mut order = order;
    if has_null && !order.contains(&None) {
        order.push(None);
    }
    if order.is_empty() {
        order.push(None);
    }
    out.push_str("<div class=\"sheet-board\">");
    for key in &order {
        let label = match key {
            None => "(none)".to_string(),
            Some(k) if matches!(group_field, Some(SheetField::Priority)) => format!("[#{k}]"),
            Some(k) => k.clone(),
        };
        out.push_str(&format!(
            "<div class=\"sheet-board-col\"><h3>{}</h3><ul>",
            esc(&label)
        ));
        let cards: &[&SheetRow] = buckets
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, rows)| rows.as_slice())
            .unwrap_or(&[]);
        for card in cards {
            if emit.anchors {
                let anchor = sheet_row_anchor(card.block, counter, index, emit.slug, emit.title);
                out.push_str(&format!("<li id=\"{}\">", esc_attr(&anchor)));
                emit_block_inner(&card.block.raw, out, emit.ctx, 0);
                if !card.block.children.is_empty() {
                    out.push_str("<ul>");
                    for c in &card.block.children {
                        render_block(
                            c, out, emit.ctx, emit.slug, emit.title, counter, index, emit.opts,
                        );
                    }
                    out.push_str("</ul>");
                }
                out.push_str("</li>");
            } else {
                render_embedded_block(card.block, out, emit.ctx, 0);
            }
        }
        out.push_str("</ul></div>");
    }
    out.push_str("</div>");
}

/// The positional grid: each child is a row, each row's children are cells.
/// `tine.header:: true` promotes the first row to `<th>` cells; a cell that is
/// itself a sheet owner renders as a nested sheet (depth-bounded).
fn render_sheet_grid(
    cfg: &SheetConfig,
    children: &[DocBlock],
    emit: &SheetEmit,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    depth: u8,
    out: &mut String,
) {
    out.push_str("<div class=\"sheet-scroll\"><table class=\"sheet-grid\">");
    let mut rows = children.iter();
    if cfg.header {
        if let Some(head) = rows.next() {
            out.push_str("<thead><tr>");
            for cell in &head.children {
                push_grid_cell(cell, emit, counter, index, depth, true, out);
            }
            if head.children.is_empty() {
                out.push_str("<th></th>");
            }
            out.push_str("</tr></thead>");
        }
    }
    out.push_str("<tbody>");
    for row in rows {
        out.push_str("<tr>");
        if row.children.is_empty() {
            out.push_str("<td></td>");
        }
        for cell in &row.children {
            push_grid_cell(cell, emit, counter, index, depth, false, out);
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table></div>");
}

fn push_grid_cell(
    cell: &DocBlock,
    emit: &SheetEmit,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    depth: u8,
    header: bool,
    out: &mut String,
) {
    let tag = if header { "th" } else { "td" };
    // Keep search coverage + anchors for cells of the host page.
    let anchor = sheet_row_anchor(cell, counter, index, emit.slug, emit.title);
    out.push_str(&format!("<{tag} id=\"{}\">", esc_attr(&anchor)));
    let nested = sheet_config(&cell.properties());
    match &nested {
        Some(nested_cfg) if !cell.children.is_empty() && depth < 4 => {
            // The cell's own line is the nested view's label; render it above
            // the nested sheet inside the same cell.
            let body = decorate(
                &lsdoc::render_html(&body_blocks(&cell.raw), &md_opts()),
                emit.ctx,
                depth,
            );
            if !body.is_empty() {
                out.push_str(&format!("<div class=\"sheet-cell-label\">{body}</div>"));
            }
            render_children_sheet(
                nested_cfg,
                &cell.children,
                emit,
                counter,
                index,
                depth + 1,
                out,
            );
        }
        _ => {
            let body = decorate(
                &lsdoc::render_html(&body_blocks(&cell.raw), &md_opts()),
                emit.ctx,
                depth,
            );
            // An empty cell's lsdoc skeleton renders as a bare `<br>` — keep
            // the static cell honestly empty instead.
            if body.trim() != "<br>" && !body.trim().is_empty() {
                out.push_str(&body);
            }
        }
    }
    out.push_str(&format!("</{tag}>"));
}

/// Children-backed sheet dispatch for `render_block`.
pub(super) fn render_children_sheet(
    cfg: &SheetConfig,
    children: &[DocBlock],
    emit: &SheetEmit,
    counter: &mut u32,
    index: &mut Vec<serde_json::Value>,
    depth: u8,
    out: &mut String,
) {
    match cfg.view {
        SheetView::Table => {
            let rows: Vec<SheetRow> = children
                .iter()
                .map(|block| SheetRow { block, page: None })
                .collect();
            render_sheet_table(cfg, &rows, emit, false, counter, index, out);
        }
        SheetView::Board => {
            let rows: Vec<SheetRow> = children
                .iter()
                .map(|block| SheetRow { block, page: None })
                .collect();
            let workflow = emit
                .ctx
                .graph
                .map(|g| g.config.preferred_workflow)
                .unwrap_or(crate::Workflow::Now);
            render_sheet_board(cfg, &rows, emit, workflow, false, counter, index, out);
        }
        SheetView::Grid => render_sheet_grid(cfg, children, emit, counter, index, depth, out),
    }
}

/// A query-backed sheet: run the blocks-selection query through the shared
/// static executor, hydrate results from source pages, then present them as
/// the requested view instead of the flat query list.
pub(super) fn render_query_sheet(
    graph: &Graph,
    cfg: &SheetConfig,
    macro_name: &str,
    query: &str,
    ctx: &Ctx,
    emit: &SheetEmit,
    out: &mut String,
) {
    let (input, check_nesting) = if macro_name.eq_ignore_ascii_case("tine-query") {
        (crate::query::QueryInput::MacroTql, false)
    } else {
        (crate::query::QueryInput::MacroQuery, true)
    };
    let outcome = match run_static_query(query, input, check_nesting, ctx) {
        Ok(outcome) => outcome,
        Err(html) => {
            out.push_str(&html);
            return;
        }
    };
    let _nested = NestedResultsGuard::enter(ctx);
    let mut render = |hydrated: &[HydratedQueryBlock<'_>]| {
        let rows = hydrated
            .iter()
            .map(|row| SheetRow {
                block: row.block,
                page: Some(row.page),
            })
            .collect::<Vec<_>>();
        let mut counter = 0u32;
        let mut sink = Vec::new(); // query results are not indexed (macro parity)
        match cfg.view {
            SheetView::Table => {
                render_sheet_table(cfg, &rows, emit, true, &mut counter, &mut sink, out)
            }
            SheetView::Board => render_sheet_board(
                cfg,
                &rows,
                emit,
                graph.config.preferred_workflow,
                true,
                &mut counter,
                &mut sink,
                out,
            ),
            SheetView::Grid => {
                out.push_str("<div class=\"query-unsupported\">A grid is positional; it cannot present query results.</div>");
            }
        }
    };
    match outcome.rows {
        StaticQueryRows::Block(groups) => with_hydrated_query_groups(graph, &groups, render),
        StaticQueryRows::Subtrees(roots) => {
            let documents = roots.iter().map(|root| {
                crate::model::dto_blocks_to_doc_checked(
                    std::slice::from_ref(&root.block),
                    crate::model::Format::from_path(Path::new(&root.path)) == crate::model::Format::Org,
                )
            }).collect::<io::Result<Vec<_>>>();
            match documents {
                Ok(documents) => {
                    let hydrated = roots.iter().zip(&documents).map(|(root, blocks)| HydratedQueryBlock {
                        page: &root.page, block: &blocks[0],
                    }).collect::<Vec<_>>();
                    render(&hydrated);
                }
                Err(error) => record_print_error(ctx, PrintPreparationError::Io(error)),
            }
        }
        StaticQueryRows::Page(_) => out.push_str("<div class=\"query-unsupported\" role=\"alert\">Page queries cannot be presented as block sheets.</div>"),
    }
}
