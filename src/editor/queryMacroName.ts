// **The macro names a query can be spelled with — the ONE list (SPEC §7.9, Y1).**
//
// Two spellings, one meaning: `{{query …}}` is the legacy OG DSL macro every
// existing graph already contains, and `{{tine-query …}}` is the TQL macro the
// printer writes when a filter is not OG-expressible (Q3). Both are queries, and
// every neighbour that RECOGNISES or WRITES one must read this list rather than
// spelling a name inline — a packet may not write a macro name its own tree
// cannot render, re-edit or export (Y1), and the way that used to happen was a
// second `/\{\{query\b/` regex somewhere the first author never looked.
//
// The Rust twin is `QUERY_MACRO_NAMES` in `crates/tine-core/src/query/ir.rs`.
// `src/queryMacroNames.test.ts` compares the two literals by reading that file,
// so the pair cannot drift silently, and it also scans `src/` and `crates/` for
// any `{{query`-shaped matcher or writer that does not read one of them (I-12).
//
// **Order is not semantics.** Readers match the LONGEST candidate rather than the
// first (`queryMacro.ts::macroAt`, `macro_text.rs::macro_at`), so reordering this
// array can never change which macro a document scan recognises.
//
// Lowercase by construction: every reader compares case-insensitively by
// lowercasing the document's spelling, so `{{QUERY …}}` still matches.
export const QUERY_MACRO_NAMES = ["query", "tine-query"] as const;

/** The macro name a query is SAVED under, given whether the OG printer can
 *  express it (Q3, §4.3). The save path is the one caller: an OG-expressible
 *  filter keeps the legacy spelling every other tool understands, and everything
 *  else becomes TQL rather than being silently narrowed to fit OG. */
export function macroNameForDialect(ogExpressible: boolean): string {
  return ogExpressible ? QUERY_MACRO_NAMES[0] : QUERY_MACRO_NAMES[1];
}

/** The empty macro the slash menu and the visual-builder entry insert, and the
 *  caret offset that lands the cursor inside it.
 *
 *  Derived from `QUERY_MACRO_NAMES[0]` rather than spelled out, so the two entry
 *  points cannot drift from each other or from the readers (§7.9). New authoring
 *  starts in the legacy OG spelling: the save path promotes the block to
 *  `{{tine-query …}}` only when the filter stops being OG-expressible (Q3).
 */
export const QUERY_MACRO_SCAFFOLD = `{{${QUERY_MACRO_NAMES[0]} }}`;
export const QUERY_MACRO_SCAFFOLD_CARET = QUERY_MACRO_SCAFFOLD.length - 2;
