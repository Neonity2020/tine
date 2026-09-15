import { isPropertyLine } from "../../render/block";
import { queryMacroExtents } from "../../editor/queryMacro";

// Detect a block whose entire body is a single query / {{embed}} macro.
//
// §7.9: the query half reads QUERY_MACRO_NAMES, so `{{tine-query …}}` is the
// same kind of block as `{{query …}}` — a block whose body is a TQL macro must
// get the standalone-query treatment (builder bar, sheet views, `tine.*` view
// properties), not fall through to inline text.
export function detectMacro(raw: string): { kind: "query" | "embed"; inner: string } | null {
  // The macro is the block's visible body — strip property lines (the shared line
  // recognizer) so a `{{query}}\nid:: …` block still matches. Cheap: no parse.
  const text = raw.split("\n").filter((l) => !isPropertyLine(l)).join("\n").trim();
  // A query macro is recognized by the SHARED extent reader, not by a regex
  // assembled here (§7.9, I-12). The regex this replaced ended the name with
  // `\b`, which made `{{query-foo bar}}` a query macro — `-` is a word boundary
  // in JavaScript — and it ended the macro at the last `}}` in the text, which a
  // `}}` inside a string literal could move. The reader gets both right, and it
  // is the same one `bodyContainsQueryMacro` and the renderer use.
  const [extent, ...rest] = queryMacroExtents(text);
  if (extent && rest.length === 0 && extent.start === 0 && extent.end === text.length) {
    // The body the renderer re-reads keeps the AUTHORED spelling, so a
    // `{{tine-query …}}` block is not silently relabelled `query` on the way in.
    return { kind: "query", inner: `${extent.name} ${extent.argument}` };
  }
  const embed = /^\{\{(embed)\b([\s\S]*)\}\}$/i.exec(text);
  return embed ? { kind: "embed", inner: `${embed[1]}${embed[2]}` } : null;
}

// Any complete query macro anywhere in the body. The shared scanner is
// brace/string/page-ref aware and catches inline macros ("Tasks {{query …}}"),
// not only macros occupying their own line, and it knows both macro names.
export function bodyContainsQueryMacro(raw: string): boolean {
  return queryMacroExtents(raw).length > 0;
}
