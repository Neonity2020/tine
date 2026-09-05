// The ONE frontend answer to "is this a query macro, and where does it sit in
// the raw source" (SPEC §4.3.1, §7.9; I-12).
//
// ## Why this module exists at all, given that Rust owns query text
//
// P0-ts deleted the frontend's query PARSER, its options-map SPLITTER and its
// advanced-vs-OG DISCRIMINATOR. Those were genuine twins: `query_parse(…,
// macro_query | macro_tql, …)` splits once, in Rust, and picks the grammar with
// the one Rust discriminator, so a frontend copy could only ever disagree with
// it on exactly the input that matters (a literal comma, a `{` inside a string).
//
// The raw EXTENT reader is deliberately NOT one of those. SPEC §4.3.1 keeps it
// on both sides and says so in as many words: *"Extend the existing
// `queryMacroExtent(s)` boundary helper … The Rust publishing boundary uses the
// same fixtures and a transcription of this helper."* The reason is structural,
// not stylistic — rendering, `bodyContainsQueryMacro`, in-place macro rewriting
// and Export collection are all SYNCHRONOUS, inside Solid memos and event
// handlers, over blocks the app already holds in memory. Routing every one of
// them through an async IPC round-trip per macro would restructure rendering and
// turn Export into an O(macros) command storm, to answer a question that is
// purely lexical: where do these bytes begin and end.
//
// What keeps the pair honest is therefore NOT a single implementation but a
// single FIXTURE SET: `crates/tine-core/tests/fixtures/query-macro/extents.json`
// is read by `queryMacro.test.ts` here and by
// `crates/tine-core/tests/query_macro_extents.rs` there, and both readers must
// produce the same `{name, argument}` for every case. That is the mechanism
// §4.3.1 names, and it is the only reason a second reader is allowed to exist.
//
// ## Offsets
//
// `start`/`end` are JavaScript string indices (UTF-16 code units); the Rust twin
// reports BYTE offsets. The shared fixtures therefore pin the recovered `name`
// and `argument` and the `raw.slice(start, end)` TEXT — the semantic contract —
// never the raw integers, which cannot agree across the two encodings.

import { QUERY_MACRO_NAMES } from "./queryMacroName";

export { QUERY_MACRO_NAMES };

/** Which grammar's literals protect a delimiter while scanning FORM text.
 *
 *  A property of the text being scanned, not of the query: an OG or advanced
 *  form is EDN-shaped (`"…"` strings, `;` comments); a TQL form is SQL-shaped
 *  (`'…'` strings, `''` doubling). Inside an options map EDN rules always apply,
 *  whichever family the form was — the map is EDN either way (§4.3.1).
 *  Transcribes `macro_text::FormFamily`. */
export type FormFamily = "edn" | "tql";

/** The family a macro NAME implies: `query` carries OG or advanced text,
 *  `tine-query` carries TQL (§7.1). `macro_text::FormFamily::for_macro_name`. */
export function formFamilyForMacroName(name: string): FormFamily {
  return name.toLowerCase() === "tine-query" ? "tql" : "edn";
}

/** Whether `name` is one of the query macro names, case-insensitively and as a
 *  WHOLE token — `{{query-foo}}` is not a query (§7.9). */
export function isQueryMacroName(name: string): boolean {
  const lower = name.toLowerCase();
  return QUERY_MACRO_NAMES.some((candidate) => candidate === lower);
}

/** One brace the scan found outside every literal, comment and page ref. */
interface Brace {
  at: number;
  open: boolean;
  /** Nesting depth AFTER this brace, counting from `formDepth`. */
  depth: number;
}

// Index just past an EDN double-quoted string opening at `at`; end of input if
// unterminated. Only `\` escapes the next character (`macro_text::edn_string_end`).
function ednStringEnd(text: string, at: number): number {
  let j = at + 1;
  while (j < text.length) {
    if (text[j] === "\\") j += 2;
    else if (text[j] === '"') return j + 1;
    else j += 1;
  }
  return text.length;
}

// Index just past a TQL single-quoted string opening at `at`; end of input if
// unterminated. SQL DOUBLES the quote (`''`) rather than backslash-escaping it
// (`macro_text::tql_string_end`).
function tqlStringEnd(text: string, at: number): number {
  let j = at + 1;
  while (j < text.length) {
    if (text[j] === "'") {
      if (text[j + 1] === "'") {
        j += 2;
        continue;
      }
      return j + 1;
    }
    j += 1;
  }
  return text.length;
}

// Index just past a `[[page ref]]` opening at `at`; end of input if unterminated.
// Page refs do not nest, so the first `]]` closes it — which is what makes
// `[[a}}b]]` opaque to the scan (`macro_text::page_ref_end`).
function pageRefEnd(text: string, at: number): number {
  const close = text.indexOf("]]", at + 2);
  return close === -1 ? text.length : close + 2;
}

/** **The one scan.** Walk `text` once and report every `{` / `}` that is not
 *  inside a protected region, with the depth it produces.
 *
 *  `formDepth` is the depth at which the form text sits: 0 when scanning a macro
 *  ARGUMENT (the splitter), 2 when scanning from inside `{{` (the extent
 *  reader). While the depth is at `formDepth` the `family` decides which literals
 *  protect a brace; deeper than that we are inside an options map and EDN rules
 *  apply. An unterminated literal consumes to end of input rather than
 *  resynchronising — that is what makes an unbalanced `}` inside a literal
 *  invisible to the split. Transcribes `macro_text::scan_braces`. */
function scanBraces(text: string, family: FormFamily, formDepth: number): Brace[] {
  const out: Brace[] = [];
  let depth = formDepth;
  let i = 0;
  while (i < text.length) {
    // Inside a map the text is EDN whatever the form was: an EDN symbol's
    // apostrophe (`'foo`, `#'x`) is never a SQL string, and a semicolon in TQL
    // form text is never a comment.
    const edn = depth > formDepth || family === "edn";
    const c = text[i];
    if (c === '"' && edn) {
      i = ednStringEnd(text, i);
      continue;
    }
    if (c === "'" && !edn) {
      i = tqlStringEnd(text, i);
      continue;
    }
    if (c === ";" && edn) {
      while (i < text.length && text[i] !== "\n") i += 1;
      continue;
    }
    if (c === "[" && text.startsWith("[[", i)) {
      i = pageRefEnd(text, i);
      continue;
    }
    if (c === "{") {
      depth += 1;
      out.push({ at: i, open: true, depth });
    } else if (c === "}") {
      depth -= 1;
      out.push({ at: i, open: false, depth });
    }
    i += 1;
  }
  return out;
}

/** One query macro as it sits in the ORIGINAL raw source.
 *
 *  `argument` is the exact slice between the macro name and the closing braces —
 *  never a rejoin of the document parser's comma-split `args`, and never missing
 *  the options map's closing brace the way the AST's argument is (§4.3.1,
 *  measured on installed mldoc 1.5.7 and on the pinned `lsdoc`). Mirrors
 *  `macro_text::MacroExtent`. */
export interface MacroExtent {
  /** Index of the opening `{{`. */
  start: number;
  /** Index just past the closing `}}`. */
  end: number;
  name: string;
  argument: string;
}

/** Read one macro whose `{{` is at `start`, if its name is a query macro name.
 *
 *  The LONGEST matching candidate wins, not the first, so `QUERY_MACRO_NAMES`'
 *  array order carries no meaning (`macro_text::macro_at`). A name must be
 *  followed by a space, a tab, or the closing brace — so `{{query-foo}}` is not
 *  a query macro, which the old `/\{\{query\b/i` regex got wrong (`-` is a word
 *  boundary in JavaScript). */
function macroAt(raw: string, start: number): MacroExtent | null {
  const rest = raw.slice(start + 2);
  let name: string | null = null;
  for (const candidate of QUERY_MACRO_NAMES) {
    if (rest.length < candidate.length) continue;
    if (rest.slice(0, candidate.length).toLowerCase() !== candidate) continue;
    const after = rest[candidate.length];
    if (after !== undefined && after !== " " && after !== "\t" && after !== "}") continue;
    if (name === null || candidate.length > name.length) name = candidate;
  }
  if (name === null) return null;
  const argumentStart = start + 2 + name.length;
  const family = formFamilyForMacroName(name);
  // Depth 2 is what the two opening braces already contributed, so form text
  // sits at depth 2 and a `{` of the options map takes it to 3.
  const braces = scanBraces(raw.slice(argumentStart), family, 2);
  const close = braces.find((brace) => !brace.open && brace.depth === 0);
  if (!close) return null; // unterminated
  const end = argumentStart + close.at + 1;
  // Everything between the name and the LAST closing brace is the argument; one
  // leading space is the macro's separator, not part of it.
  const argument = raw.slice(argumentStart, end - 2);
  return {
    start,
    end,
    name,
    argument: argument.startsWith(" ") ? argument.slice(1) : argument,
  };
}

/** The first query macro in `raw`, or null.
 *
 *  Brace-, string- and page-ref-aware: a `}}` inside a string, a nested `{…}`
 *  options map, or a `[[page]]` ref does not end it early — which is exactly what
 *  a lazy `/\{\{query.*?\}\}/` gets wrong. */
export function queryMacroExtent(raw: string): MacroExtent | null {
  return queryMacroExtentFrom(raw, 0);
}

/** Every query macro in `raw`, in source order. A block may hold several, and a
 *  rewrite must target the right one BY EXTENT, never "the first one". */
export function queryMacroExtents(raw: string): MacroExtent[] {
  const out: MacroExtent[] = [];
  let from = 0;
  while (from < raw.length) {
    const found = queryMacroExtentFrom(raw, from);
    if (!found) break;
    from = found.end;
    out.push(found);
  }
  return out;
}

function queryMacroExtentFrom(raw: string, from: number): MacroExtent | null {
  let search = from;
  for (;;) {
    const at = raw.indexOf("{{", search);
    if (at === -1) return null;
    const found = macroAt(raw, at);
    if (found) return found;
    search = at + 2;
  }
}

const UTF8_ENCODER = new TextEncoder();

/** The extent a parsed macro node's SPAN points at, or null.
 *
 *  §4.3.1: *"Associate an AST query macro by source offset with its full raw
 *  extent"*. The renderer knows a macro node's span but not its raw bytes; this
 *  is the join, and it is why rendering no longer has to trust `args.join(", ")`.
 *
 *  **A span is not an index into `raw`.** lsdoc parses the RE-BULLETED input
 *  (`"- " + raw.trimStart()`, see `render/facets.ts::parseBody`) and reports
 *  UTF-8 BYTE offsets into that string, so the mapping back is
 *  `span[0] - 2 + leadBytes` — exactly what `facets.ts::standaloneSourceLine`
 *  already does for planning timestamps. Extents, by contrast, are JS string
 *  indices (UTF-16 code units). This function is the ONE place the two
 *  coordinate systems meet; comparing a span to `extent.start` directly is off by
 *  the bullet prefix and, past any non-ASCII text, off by the encoding too.
 *
 *  It anchors EXACTLY. `renderInlines` also runs over text that is not the
 *  block's raw — a property value, an expanded user macro — where the span
 *  indexes a different string and a containment test would silently select some
 *  other macro from the same block. A mismatched base finds no extent and the
 *  caller falls back. */
export function queryMacroExtentAtSpan(
  raw: string,
  span: readonly [number, number] | undefined,
): MacroExtent | null {
  if (span === undefined || span[0] < 2) return null;
  const trimmed = raw.trimStart();
  const leadBytes = UTF8_ENCODER.encode(raw.slice(0, raw.length - trimmed.length)).length;
  const wanted = span[0] - 2 + leadBytes;
  for (const extent of queryMacroExtents(raw)) {
    if (UTF8_ENCODER.encode(raw.slice(0, extent.start)).length === wanted) return extent;
  }
  return null;
}
