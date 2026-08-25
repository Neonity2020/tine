// DUP-8: the shared search-grammar conformance corpus, TypeScript side.
//
// The Ctrl-K query dialect is implemented twice: here for the page list, and in
// `crates/tine-core/src/search_query.rs` for the block list. Until this corpus
// existed, the only thing keeping them in step was the "MIRRORS … MUST agree"
// note at the top of both files. A user typing one query gets both engines at
// once, so a drift shows up as the two lists disagreeing about the same query.
//
// `tests/fixtures/search-query-corpus.json` is asserted case for case by this
// file and by `search_query::corpus` on the Rust side. It pins CURRENT
// behavior. Every row has one shared answer; runtime-specific expectations are
// forbidden because users get both engines in the same search surface.
import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { parseSearchQuery, simpleTerm, tokenize, type SearchMatcher } from "./searchQuery";

interface Expectation {
  tokens: unknown;
  verdict: unknown;
}
interface Case extends Expectation {
  name: string;
  query: string;
}

const corpus = JSON.parse(
  readFileSync(fileURLToPath(new URL("../../tests/fixtures/search-query-corpus.json", import.meta.url)), "utf8"),
) as { cases: Case[] };

// The wire shape the corpus records. `invalid` deliberately carries no message:
// the two regex engines word their errors differently and that is not a
// contract, only the refusal is.
function verdictOf(m: SearchMatcher): unknown {
  switch (m.kind) {
    case "empty":
      return { kind: "empty" };
    case "invalid":
      return { kind: "invalid" };
    case "regex":
      return { kind: "regex", pattern: m.re.source };
    case "boolean":
      return {
        kind: "boolean",
        groups: m.groups.map((group) =>
          group.map((t) => ({ text: t.text, negated: t.negated, quoted: t.quoted })),
        ),
        simpleTerm: simpleTerm(m),
      };
  }
}

describe("search-grammar conformance corpus (DUP-8)", () => {
  it("has cases", () => {
    expect(corpus.cases.length).toBeGreaterThan(0);
  });

  for (const testCase of corpus.cases) {
    it(testCase.name, () => {
      expect(tokenize(testCase.query)).toEqual(testCase.tokens);
      expect(verdictOf(parseSearchQuery(testCase.query))).toEqual(testCase.verdict);
    });
  }
});
