// Tiny, dependency-free helpers for the bits of EDN we read/write inside a
// `{{query … {:opts}}}` macro. String- and brace-aware so values containing `"`,
// `\`, `{`, or `}` don't confuse the (otherwise regex-based) query handling.

/** Escape a string for an EDN double-quoted literal. */
export function quoteEdnString(s: string): string {
  return s.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}
/** Inverse of quoteEdnString for the captured inner text of an EDN string. */
export function unquoteEdnString(inner: string): string {
  return inner.replace(/\\(.)/g, "$1");
}

// The `{{query …}}` EXTENT readers that used to live here moved to
// `src/editor/queryMacro.ts` (P0-ts, SPEC §4.3.1/§7.9). They belong with the
// macro-name constant and with the raw-source transport, and they had to grow a
// `{name, argument}` result and TQL string awareness that has nothing to do with
// EDN.
//
// `splitTrailingMap` — which answered "where does the trailing `{…}` options map
// begin" — is GONE, not moved. That is a query-language question, and the query
// engine answers it in Rust (`query::og::split_trailing_map`, reached through
// `query_parse`): `source.original` is the form and `source.og_options` is the
// map, verbatim. Two readers of the same bytes is exactly the second answer I-12
// forbids, and the two did disagree. Measured, on the advanced whole-map form
// `{:query [:find (pull ?b [*]) :where [?b :block/content "x"]]}`: this reader
// returned `form: ""` with the entire map as OPTIONS, so the query executed as an
// empty form, while Rust returns the map as the FORM — `split_trailing_map` splits
// only a map that FOLLOWS a nonempty form (§4.3.1). It also had no notion of
// `FormFamily`, so it could not protect a TQL `'…'` literal the way the EDN `"…"`
// case it did know about is protected.
//
// What remains in this file is only what its name claims: EDN string quoting for
// that opaque options map, which the engine deliberately does NOT interpret.
