//! A query's compiled leaves: the search programs and regexes its text leaves
//! compile to, built once per query rather than once per block.

use std::collections::HashMap;

use crate::query::ir::{Attr, CmpOp, Filter, Leaf, Value};
use crate::search_query::Matcher;

/// Patterns that cost real work to build (`(search …)`'s friendly matcher, a
/// `(content-regex …)` regex) compiled ONCE per query rather than per block.
/// The old `Pred` carried the compiled value inside the variant; the IR carries
/// only the user's text, so the compile cache lives here.
#[derive(Default)]
pub(crate) struct CompiledLeaves {
    matchers: HashMap<String, Matcher>,
    regexes: HashMap<String, Option<regex::Regex>>,
}

impl CompiledLeaves {
    /// Parse every compiled leaf of one query, ONCE (SPEC §5.10, R3).
    ///
    /// **This is the parsed Match payload, and it is the only one.** P1's SQL
    /// compiler reads `match_program` below, over the same
    /// `Filter::match_sources` keys, and lowers the SAME
    /// `search_query::Matcher::Boolean` groups it finds there into `instr`
    /// predicates; the test-only walk oracle reads the same map. A second
    /// `Matcher::parse` anywhere in the query engine — in the compiler, in a
    /// plan, in a cache — is the fork this campaign exists to prevent (I-12):
    /// `content match` and legacy `(search …)` would stop meaning the same
    /// thing the moment the two parses disagreed.
    pub(crate) fn for_query(filter: &Filter) -> CompiledLeaves {
        let mut compiled = CompiledLeaves::default();
        collect_compiled(filter, &mut compiled);
        compiled
    }

    /// The parsed Match payload for one `content match <text>` leaf — the
    /// value both engines consume, never a re-parsed string.
    pub(crate) fn match_program(&self, source: &str) -> Option<&Matcher> {
        self.matchers.get(source)
    }
    /// The compiled legacy `content regexp` pattern for one leaf, or `None`
    /// when the pattern did not compile — which §4.3.2 keeps as a retained leaf
    /// that matches false. The SQL compiler reads the SAME map, so an invalid
    /// pattern is a constant-false leaf on both engines rather than a second
    /// `regex::Regex::new` that could disagree about validity (I-12).
    pub(crate) fn regex(&self, source: &str) -> Option<&regex::Regex> {
        self.regexes.get(source).and_then(Option::as_ref)
    }
}

fn collect_compiled(filter: &Filter, out: &mut CompiledLeaves) {
    // The Match half goes through the IR's own recognizer, so "which leaves
    // carry a search query" has one answer for the walk and for the lowering.
    for source in filter.match_sources() {
        out.matchers
            .entry(source.to_string())
            .or_insert_with(|| Matcher::parse(source));
    }
    filter.any_leaf(&mut |leaf| {
        if let Leaf::Attr {
            attr: Attr::Content,
            op: CmpOp::Regex,
            value: Value::Text { text },
        } = leaf
        {
            out.regexes
                .entry(text.clone())
                .or_insert_with(|| regex::Regex::new(text).ok());
        }
        false
    });
}
