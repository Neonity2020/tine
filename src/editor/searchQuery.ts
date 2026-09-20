// Frontend parser for the Ctrl-K quick-search query dialect (GH #44).
//
// This module owns syntax only. Terms retain the user's exact spelling so the
// production UI can forward them to the native engine without folding twice.
// Native search owns authoritative normalization and matching.

export interface Term {
  // Exact term spelling from the friendly source.
  text: string;
  negated: boolean;
  // Came from a `"quoted phrase"` — an explicit grammar opt-in, so even a single
  // quoted word is not treated as `simple`.
  quoted: boolean;
}

export type SearchMatcher =
  | { kind: "empty" }
  | { kind: "invalid"; error: string }
  | { kind: "regex"; re: RegExp }
  // OR of AND-groups; every retained group has ≥1 positive term.
  | { kind: "boolean"; groups: Term[][] };

export const SEARCH_SYNTAX = [
  { example: "foo bar", description: "contains both terms", match: "bar then foo", miss: "foo only" },
  { example: "foo OR bar", description: "contains either term", match: "bar only", miss: "neither" },
  { example: "foo -draft", description: "contains foo, excludes draft", match: "foo ready", miss: "foo draft" },
  { example: '"exact phrase"', description: "matches adjacent words", match: "an exact phrase here", miss: "exact other phrase" },
  { example: "/[A-Z]{3}/", description: "case-sensitive regular expression", match: "ABC", miss: "abc" },
] as const;

// The exact cross-runtime whitespace contract. ECMAScript and Rust's Unicode
// helpers disagree on U+FEFF and U+0085, so using either runtime's broad helper
// would make one query split differently between the page and block engines.
function isSearchWhitespace(char: string): boolean {
  const code = char.codePointAt(0) ?? 0;
  return (
    (code >= 0x0009 && code <= 0x000d)
    || code === 0x0020
    || code === 0x00a0
    || code === 0x1680
    || (code >= 0x2000 && code <= 0x200a)
    || code === 0x2028
    || code === 0x2029
    || code === 0x202f
    || code === 0x205f
    || code === 0x3000
    || code === 0xfeff
  );
}

function trimSearchWhitespace(value: string): string {
  const chars = Array.from(value);
  let start = 0;
  let end = chars.length;
  while (start < end && isSearchWhitespace(chars[start])) start += 1;
  while (end > start && isSearchWhitespace(chars[end - 1])) end -= 1;
  return chars.slice(start, end).join("");
}

/** Reject regex constructs for which Rust `regex` and JavaScript RegExp do not
 * share semantics. Non-capturing groups remain available; look-around, inline
 * flags, named/engine-specific groups, and backreferences do not. */
function commonRegexPattern(pattern: string): boolean {
  let inClass = false;
  for (let i = 0; i < pattern.length; i += 1) {
    if (pattern[i] === "\\") {
      const escaped = pattern[i + 1];
      if (escaped && escaped >= "1" && escaped <= "9") return false;
      i += 1;
      continue;
    }
    if (pattern[i] === "[") {
      inClass = true;
      continue;
    }
    if (pattern[i] === "]" && inClass) {
      inClass = false;
      continue;
    }
    if (!inClass && pattern[i] === "(" && pattern[i + 1] === "?" && pattern[i + 2] !== ":") {
      return false;
    }
  }
  return true;
}

export function parseSearchQuery(query: string): SearchMatcher {
  const q = trimSearchWhitespace(query);
  if (!q) return { kind: "empty" };
  // Whole-query regex: `/pattern/` with a non-empty pattern. (`//` is too short —
  // an empty pattern matches everything — so it falls through to a literal term.)
  if (q.length >= 3 && q.startsWith("/") && q.endsWith("/")) {
    const pat = q.slice(1, -1);
    if (!commonRegexPattern(pat)) {
      return { kind: "invalid", error: "regex feature is not supported by both search engines" };
    }
    try {
      // Case-sensitive (no `i`), matching the Rust `regex` side: the pattern owns
      // its case classes, so `[A-Z]` works.
      return { kind: "regex", re: new RegExp(pat, "u") };
    } catch (e) {
      return { kind: "invalid", error: e instanceof Error ? e.message : "invalid regex" };
    }
  }
  const groups = parseBoolean(q).filter((g) => g.some((t) => !t.negated));
  if (!groups.length) return { kind: "empty" };
  return { kind: "boolean", groups };
}

// The single positive bare term when this is a one-term query, else null.
export function simpleTerm(m: SearchMatcher): string | null {
  if (m.kind !== "boolean" || m.groups.length !== 1 || m.groups[0].length !== 1) return null;
  const t = m.groups[0][0];
  return !t.negated && !t.quoted ? t.text : null;
}

function quoteDsl(value: string): string {
  return `"${value.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

/** Lossless compiler for the friendly block-search grammar into the ordinary
 * simple query DSL. Page fuzzy matching is intentionally not implied here: it
 * is an explicit page branch in QueryPlan, while this compiler is used when a
 * user deliberately switches a workspace to the block-query builder. */
export function friendlySearchToDsl(query: string): { dsl: string; error: string | null } {
  const matcher = parseSearchQuery(query);
  if (matcher.kind === "invalid") return { dsl: "", error: matcher.error };
  if (matcher.kind === "empty") return { dsl: "", error: "Add at least one positive search term." };
  if (matcher.kind === "regex") {
    return { dsl: `(content-regex ${quoteDsl(matcher.re.source)})`, error: null };
  }
  const termDsl = (term: Term) => {
    const content = quoteDsl(term.text);
    return term.negated ? `(not ${content})` : content;
  };
  const groups = matcher.groups.map((group) => {
    const terms = group.map(termDsl);
    return terms.length === 1 ? terms[0] : `(and ${terms.join(" ")})`;
  });
  return { dsl: groups.length === 1 ? groups[0] : `(or ${groups.join(" ")})`, error: null };
}

/** Canonical lossless on-disk representation for a friendly search workspace.
 * The `(search …)` predicate is a Tine query extension compiled by the same
 * Rust QueryPlan as Ctrl+K; it keeps the friendly source reconstructible. */
export function friendlySearchToSavedDsl(query: string): string {
  return `(search ${quoteDsl(query.trim())})`;
}

/** Recover friendly source from the canonical `(search "…")` query extension.
 * Returns null for any other DSL so frontends never pretend a lossy conversion
 * is reversible. */
export function savedDslToFriendlySearch(dsl: string): string | null {
  const match = /^\(\s*search\s+"((?:[^"\\]|\\.)*)"\s*\)$/s.exec(dsl.trim());
  if (!match) return null;
  let out = "";
  for (let i = 0; i < match[1].length; i += 1) {
    const char = match[1][i];
    if (char === "\\" && i + 1 < match[1].length && (match[1][i + 1] === "\\" || match[1][i + 1] === '"')) {
      out += match[1][i + 1];
      i += 1;
    } else out += char;
  }
  return out;
}

function parseBoolean(q: string): Term[][] {
  const tokens = tokenize(q);
  const groups: Term[][] = [];
  let cur: Term[] = [];
  for (const tok of tokens) {
    if (tok.isOr) {
      groups.push(cur);
      cur = [];
      continue;
    }
    if (!tok.text) continue;
    cur.push({ text: tok.text, negated: tok.negated, quoted: tok.quoted });
  }
  groups.push(cur);
  return groups.filter((g) => g.length > 0);
}

export interface Token {
  text: string;
  negated: boolean;
  quoted: boolean;
  isOr: boolean;
}

// Split into tokens, honoring `"quoted phrases"` (may contain spaces) and a
// leading `-` for negation. A bare unquoted `OR` becomes an OR separator.
//
// Exported so the shared conformance corpus
// (`tests/fixtures/search-query-corpus.json`) can be asserted against the same
// function the Rust side asserts against, rather than against a proxy.
export function tokenize(q: string): Token[] {
  const chars = Array.from(q);
  const out: Token[] = [];
  let i = 0;
  while (i < chars.length) {
    if (isSearchWhitespace(chars[i])) {
      i += 1;
      continue;
    }
    let negated = false;
    // Leading `-` negates, but only when something non-space follows it.
    if (chars[i] === "-" && i + 1 < chars.length && !isSearchWhitespace(chars[i + 1])) {
      negated = true;
      i += 1;
    }
    let text: string;
    let quoted: boolean;
    if (i < chars.length && chars[i] === '"') {
      // Quoted phrase: read to the closing quote (or end of input).
      i += 1;
      const start = i;
      while (i < chars.length && chars[i] !== '"') i += 1;
      text = chars.slice(start, i).join("");
      if (i < chars.length) i += 1; // consume closing quote
      quoted = true;
    } else {
      // Bare token: read to the next whitespace.
      const start = i;
      while (i < chars.length && !isSearchWhitespace(chars[i])) i += 1;
      text = chars.slice(start, i).join("");
      quoted = false;
    }
    if (!quoted && !negated && text === "OR") {
      out.push({ text: "", negated: false, quoted: false, isOr: true });
    } else {
      out.push({ text, negated, quoted, isOr: false });
    }
  }
  return out;
}
