//! Shared parser for the Ctrl-K quick-search query dialect (GH #44).
//!
//! Mirrors `src/editor/searchQuery.ts` on the frontend — keep the two in sync
//! (the grammar, the `is_simple` rule, and the match semantics must agree, or a
//! query would filter differently in the page list than in the block list).
//!
//! Grammar (the mainstream full-text convention — see the cited scan in
//! `subagent-tasks/notes/search-syntax-industry-scan.md`):
//!   - whitespace between terms = order-independent **AND**
//!   - `OR` (uppercase keyword, its own token) = **OR** between groups
//!   - `-term` / `-"phrase"` = **exclude** (negation)
//!   - `"phrase"` = exact contiguous substring
//!   - `/regex/` (the WHOLE query, slash-delimited) = regex, matched
//!     case-sensitively against original-case text (so `[A-Z]` works)
//!   - everything else is a case-insensitive substring term
//!
//! A **single bare positive term** parses to `is_simple() == true`, which the
//! quick switcher uses to keep today's fuzzy page-name ranking; any second
//! term / operator / regex switches both pages and blocks to this grammar.

use unicode_normalization::UnicodeNormalization;

fn is_search_whitespace(char: char) -> bool {
    matches!(
        char,
        '\u{0009}'..='\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn common_regex_pattern(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    let mut in_class = false;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if i + 1 < bytes.len() && matches!(bytes[i + 1], b'1'..=b'9') {
                return false;
            }
            i += 2;
            continue;
        }
        if bytes[i] == b'[' {
            in_class = true;
            i += 1;
            continue;
        }
        if bytes[i] == b']' && in_class {
            in_class = false;
            i += 1;
            continue;
        }
        if !in_class
            && bytes[i] == b'('
            && bytes.get(i + 1) == Some(&b'?')
            && bytes.get(i + 2) != Some(&b':')
        {
            return false;
        }
        i += 1;
    }
    true
}

/// Canonical comparison representation for non-regex search. Lowercasing is
/// locale-independent; NFC makes canonically equivalent spellings compare
/// alike without compatibility folding or removing accents.
pub fn canonical_fold(value: &str) -> String {
    value.to_lowercase().nfc().collect()
}

/// One AND-term: a substring to test (already canonically folded) plus whether it is
/// negated (`-term` → must NOT be present).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    /// Lowercase-plus-NFC needle for `visible_lower.contains(..)`.
    pub text: String,
    pub negated: bool,
    /// The term came from a `"quoted phrase"` — an explicit opt-in to the
    /// grammar, so even a single quoted word is not treated as `is_simple`.
    pub quoted: bool,
}

/// A conjunction of terms (all must be satisfied). Groups are OR-ed together.
pub type AndGroup = Vec<Term>;

/// Mirrored behavioral examples displayed by Ctrl+K. Tests execute every row so
/// visible help cannot describe syntax the Rust matcher does not implement.
pub const SEARCH_SYNTAX_EXAMPLES: &[(&str, &str, &str)] = &[
    ("foo bar", "bar then foo", "foo only"),
    ("foo OR bar", "bar only", "neither"),
    ("foo -draft", "foo ready", "foo draft"),
    (
        "\"exact phrase\"",
        "an exact phrase here",
        "exact other phrase",
    ),
    ("/[A-Z]{3}/", "ABC", "abc"),
];

/// A parsed query, ready to test blocks/page-names against.
#[derive(Debug, Clone)]
pub enum Matcher {
    /// Whole query was `/pattern/` and compiled.
    Regex(regex::Regex),
    /// Whole query was `/pattern/` but the pattern failed to compile; carries the
    /// error message for the frontend to surface. Matches nothing.
    InvalidRegex(String),
    /// OR of AND-groups. Every retained group has ≥1 positive term.
    Boolean(Vec<AndGroup>),
    /// No effective query (blank, or only exclusions). Matches nothing.
    Empty,
}

impl Matcher {
    /// Parse a raw query string into a matcher.
    pub fn parse(query: &str) -> Matcher {
        let q = query.trim_matches(is_search_whitespace);
        if q.is_empty() {
            return Matcher::Empty;
        }
        // Whole-query regex: `/pattern/` with a non-empty pattern. (A lone `//`
        // would be an empty pattern that matches everything — not useful, so we
        // treat it as a literal boolean term instead.)
        if q.len() >= 3 && q.starts_with('/') && q.ends_with('/') {
            let pat = &q[1..q.len() - 1];
            if !common_regex_pattern(pat) {
                return Matcher::InvalidRegex(
                    "regex feature is not supported by both search engines".to_string(),
                );
            }
            return match regex::Regex::new(pat) {
                Ok(re) => Matcher::Regex(re),
                Err(e) => Matcher::InvalidRegex(e.to_string()),
            };
        }
        let groups = parse_boolean(q);
        // A group with no positive term (e.g. the whole query is `-foo`) would
        // match nearly everything — drop it; if none survive, the query is Empty.
        let groups: Vec<AndGroup> = groups
            .into_iter()
            .filter(|g| g.iter().any(|t| !t.negated))
            .collect();
        if groups.is_empty() {
            Matcher::Empty
        } else {
            Matcher::Boolean(groups)
        }
    }

    /// Does `visible` match? `lower` is the pre-folded lowercase-plus-NFC body
    /// (hot path for boolean terms); `orig` is the original body (needed by regex).
    pub fn matches(&self, lower: &str, orig: &str) -> bool {
        match self {
            Matcher::Regex(re) => re.is_match(orig),
            Matcher::InvalidRegex(_) | Matcher::Empty => false,
            Matcher::Boolean(groups) => groups.iter().any(|g| group_matches(g, lower)),
        }
    }

    /// The single positive term when this is a bare one-term query, else `None`.
    /// Drives the quick switcher's "keep fuzzy page ranking" fast path.
    pub fn simple_term(&self) -> Option<&str> {
        match self {
            Matcher::Boolean(groups) if groups.len() == 1 && groups[0].len() == 1 => {
                let t = &groups[0][0];
                (!t.negated && !t.quoted).then_some(t.text.as_str())
            }
            _ => None,
        }
    }

    /// Rank a page name (already lowercase-plus-NFC in `lower`, original in `orig`) for
    /// the non-simple path: prefix > substring, else `None` if it doesn't match.
    pub fn score_name(&self, lower: &str, orig: &str) -> Option<i32> {
        match self {
            Matcher::Regex(re) => re.is_match(orig).then_some(500),
            Matcher::InvalidRegex(_) | Matcher::Empty => None,
            Matcher::Boolean(groups) => {
                let g = groups.iter().find(|g| group_matches(g, lower))?;
                // Prefix match on any positive term ranks above a mid-name hit.
                let prefix = g
                    .iter()
                    .any(|t| !t.negated && !t.text.is_empty() && lower.starts_with(&t.text));
                Some(if prefix { 1000 } else { 500 })
            }
        }
    }
}

fn group_matches(group: &AndGroup, lower: &str) -> bool {
    group.iter().all(|t| {
        let present = !t.text.is_empty() && lower.contains(&t.text);
        present != t.negated
    })
}

/// Tokenize + group a boolean query. `OR` (bare, uppercase) starts a new group;
/// other tokens accumulate into the current group.
fn parse_boolean(q: &str) -> Vec<AndGroup> {
    let tokens = tokenize(q);
    let mut groups: Vec<AndGroup> = Vec::new();
    let mut cur: AndGroup = Vec::new();
    for tok in tokens {
        if tok.is_or {
            groups.push(std::mem::take(&mut cur));
            continue;
        }
        if tok.text.is_empty() {
            continue;
        }
        cur.push(Term {
            text: canonical_fold(&tok.text),
            negated: tok.negated,
            quoted: tok.quoted,
        });
    }
    groups.push(cur);
    groups.into_iter().filter(|g| !g.is_empty()).collect()
}

struct Token {
    text: String,
    negated: bool,
    quoted: bool,
    /// A bare `OR` separator (never both `is_or` and non-empty `text`).
    is_or: bool,
}

/// Split into tokens, honoring `"quoted phrases"` (which may contain spaces) and
/// a leading `-` for negation. A bare unquoted `OR` becomes an OR separator.
fn tokenize(q: &str) -> Vec<Token> {
    let chars: Vec<char> = q.chars().collect();
    let mut out: Vec<Token> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if is_search_whitespace(chars[i]) {
            i += 1;
            continue;
        }
        let mut negated = false;
        // A leading `-` negates, but only when something follows it (a lone `-`
        // is treated as a literal term).
        if chars[i] == '-' && i + 1 < chars.len() && !is_search_whitespace(chars[i + 1]) {
            negated = true;
            i += 1;
        }
        let (text, quoted) = if i < chars.len() && chars[i] == '"' {
            // Quoted phrase: read to the closing quote (or end of input).
            i += 1;
            let start = i;
            while i < chars.len() && chars[i] != '"' {
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            if i < chars.len() {
                i += 1; // consume closing quote
            }
            (s, true)
        } else {
            // Bare token: read to the next whitespace.
            let start = i;
            while i < chars.len() && !is_search_whitespace(chars[i]) {
                i += 1;
            }
            (chars[start..i].iter().collect::<String>(), false)
        };
        if !quoted && !negated && text == "OR" {
            out.push(Token {
                text: String::new(),
                negated: false,
                quoted: false,
                is_or: true,
            });
        } else {
            out.push(Token {
                text,
                negated,
                quoted,
                is_or: false,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(q: &str) -> Matcher {
        Matcher::parse(q)
    }
    // Convenience: match against text (folds the boolean side once).
    fn hit(q: &str, text: &str) -> bool {
        m(q).matches(&canonical_fold(text), text)
    }

    #[test]
    fn single_term_is_simple() {
        let mt = m("hello");
        assert_eq!(mt.simple_term(), Some("hello"));
        assert!(hit("hello", "well HELLO there"));
        assert!(!hit("hello", "goodbye"));
    }

    #[test]
    fn visible_syntax_examples_match_the_documented_behavior() {
        for (query, matching, missing) in SEARCH_SYNTAX_EXAMPLES {
            assert!(hit(query, matching), "{query:?} should match {matching:?}");
            assert!(!hit(query, missing), "{query:?} should reject {missing:?}");
        }
    }

    #[test]
    fn whitespace_is_and_order_independent() {
        let mt = m("foo bar");
        assert_eq!(mt.simple_term(), None); // two terms → not simple
        assert!(hit("foo bar", "bar then foo"));
        assert!(hit("foo bar", "foo and bar"));
        assert!(!hit("foo bar", "only foo"));
    }

    #[test]
    fn or_keyword_splits_groups() {
        assert!(hit("cat OR dog", "i have a dog"));
        assert!(hit("cat OR dog", "i have a cat"));
        assert!(!hit("cat OR dog", "i have a fish"));
        // AND binds tighter than OR: "a b OR c" = (a AND b) OR c
        assert!(hit("apple pie OR cake", "cake"));
        assert!(hit("apple pie OR cake", "apple pie"));
        assert!(!hit("apple pie OR cake", "apple tart"));
    }

    #[test]
    fn negation_excludes() {
        assert!(hit("foo -bar", "foo only"));
        assert!(!hit("foo -bar", "foo and bar"));
        // A pure-negation query matches nothing (not everything).
        assert!(matches!(m("-bar"), Matcher::Empty));
        assert!(!hit("-bar", "anything"));
    }

    #[test]
    fn quoted_phrase_is_contiguous() {
        assert!(hit("\"foo bar\"", "a foo bar b"));
        assert!(!hit("\"foo bar\"", "foo x bar"));
        // Quoted single word is not "simple" (user opted into the grammar).
        assert_eq!(m("\"foo\"").simple_term(), None);
        // Negated phrase.
        assert!(!hit("-\"foo bar\"", "foo bar here"));
        assert!(hit("keep -\"foo bar\"", "keep foo x bar"));
    }

    #[test]
    fn regex_whole_query_case_sensitive() {
        assert!(hit("/[A-Z]{3}/", "abc ABC def"));
        assert!(!hit("/[A-Z]{3}/", "abc def"));
        assert!(hit("/^start/", "start of line"));
        assert!(!hit("/^start/", "not at start"));
    }

    #[test]
    fn invalid_regex_reports_and_matches_nothing() {
        let mt = m("/(unclosed/");
        assert!(matches!(mt, Matcher::InvalidRegex(_)));
        assert!(!mt.matches("(unclosed", "(unclosed"));
    }

    #[test]
    fn regex_contract_is_unicode_aware_and_rejects_engine_specific_features() {
        assert!(hit(r"/\p{L}+/", "café"));
        assert!(!hit(r"/\p{L}+/", "123"));
        assert!(hit(r"/[(?]+/", "(?"));
        for query in [r"/foo(?=bar)/", r"/(a)\1/", r"/(?i)abc/"] {
            assert!(matches!(m(query), Matcher::InvalidRegex(_)), "{query}");
        }
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert!(matches!(m("   "), Matcher::Empty));
        assert!(!hit("   ", "anything"));
    }

    #[test]
    fn score_name_prefers_prefix() {
        let mt = m("foo bar");
        // "foobar…" — a positive term prefixes the name → 1000.
        assert_eq!(mt.score_name("foobar baz", "foobar baz"), Some(1000));
        // both present but neither prefixes → 500.
        assert_eq!(mt.score_name("xbar yfoo", "xbar yfoo"), Some(500));
        assert_eq!(mt.score_name("only foo", "only foo"), None);
    }

    #[test]
    fn lone_slash_is_literal_not_regex() {
        // `//` is too short to be a regex → literal term.
        assert!(hit("//", "a // b"));
        assert!(!matches!(m("//"), Matcher::Regex(_)));
    }

    #[test]
    fn canonical_unicode_equivalence_does_not_fold_accents() {
        assert!(hit("café", "a cafe\u{301} here"));
        assert!(hit("cafe\u{301}", "a café here"));
        assert!(hit("\u{ac00}", "Hangul \u{1100}\u{1161}"));
        assert!(hit("i\u{307}", "\u{130}"));
        assert!(!hit("cafe", "café"));
        // Regular expressions retain their original-text semantics.
        assert!(!hit("/café/", "cafe\u{301}"));
    }
}

/// DUP-8: the shared search-grammar conformance corpus.
///
/// This dialect is implemented twice -- here, and in `src/editor/searchQuery.ts`
/// for the page list -- and until now the only thing keeping the two in step was
/// the "keep the two in sync" note at the top of both files. A user typing one
/// query into Ctrl+K gets both engines at once, so a drift between them shows up
/// as the page list and the block list disagreeing about the same query.
///
/// `tests/fixtures/search-query-corpus.json` is asserted here and, case for
/// case, by `src/editor/searchQuery.corpus.test.ts`. The corpus pins CURRENT
/// behavior. Every row has one shared answer; adding a runtime-specific answer
/// would reintroduce the page-list/block-list split this corpus prevents.
#[cfg(test)]
mod corpus {
    use super::*;
    use serde_json::Value;

    const CORPUS: &str = include_str!("../../../tests/fixtures/search-query-corpus.json");

    fn tokens_json(query: &str) -> Value {
        Value::Array(
            tokenize(query)
                .into_iter()
                .map(|t| {
                    serde_json::json!({
                        "text": t.text,
                        "negated": t.negated,
                        "quoted": t.quoted,
                        "isOr": t.is_or,
                    })
                })
                .collect(),
        )
    }

    fn verdict_json(query: &str) -> Value {
        let matcher = Matcher::parse(query);
        match &matcher {
            // The two regex engines word their errors differently; the message
            // is not a contract, only the refusal is.
            Matcher::Empty => serde_json::json!({ "kind": "empty" }),
            Matcher::InvalidRegex(_) => serde_json::json!({ "kind": "invalid" }),
            Matcher::Regex(re) => serde_json::json!({ "kind": "regex", "pattern": re.as_str() }),
            Matcher::Boolean(groups) => serde_json::json!({
                "kind": "boolean",
                "groups": groups
                    .iter()
                    .map(|group| {
                        group
                            .iter()
                            .map(|t| serde_json::json!({
                                "text": t.text,
                                "negated": t.negated,
                                "quoted": t.quoted,
                            }))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>(),
                "simpleTerm": matcher.simple_term(),
            }),
        }
    }

    #[test]
    fn the_rust_engine_matches_every_corpus_case() {
        let doc: Value = serde_json::from_str(CORPUS).expect("corpus is valid JSON");
        let cases = doc["cases"].as_array().expect("corpus has a `cases` array");
        assert!(!cases.is_empty(), "the corpus is empty");

        for case in cases {
            let name = case["name"].as_str().expect("every case is named");
            let query = case["query"].as_str().expect("every case has a query");
            assert!(
                case.get("knownDivergence").is_none(),
                "{name}: the conformance corpus must have one cross-runtime answer"
            );
            let expected = case;
            assert_eq!(
                tokens_json(query),
                expected["tokens"],
                "{name}: tokenize({query:?}) changed"
            );
            assert_eq!(
                verdict_json(query),
                expected["verdict"],
                "{name}: Matcher::parse({query:?}) changed"
            );
        }
    }
}
