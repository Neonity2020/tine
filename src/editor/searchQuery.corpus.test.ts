// DUP-8: shared search-grammar conformance corpus, TypeScript syntax side.
//
// Rust owns normalization and matching, so the fixture's verdict term text is
// the native folded needle. The frontend asserts shared tokens and verdict
// structure here, then separately pins that its syntax terms remain raw. This
// prevents the native A6 expectation from becoming a second frontend fold.
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

function grammarShape(verdict: unknown): unknown {
  const value = verdict as {
    kind: string;
    pattern?: string;
    groups?: { text: string; negated: boolean; quoted: boolean }[][];
    simpleTerm?: string | null;
  };
  if (value.kind !== "boolean") return value;
  return {
    kind: value.kind,
    groups: value.groups?.map((group) =>
      group.map((term) => ({ negated: term.negated, quoted: term.quoted })),
    ),
    simpleTerm: value.simpleTerm === null ? null : "<term>",
  };
}

describe("search-grammar conformance corpus (DUP-8)", () => {
  it("has cases", () => {
    expect(corpus.cases.length).toBeGreaterThan(0);
  });

  it("keeps frontend syntax terms raw independently of native fold expectations", () => {
    const matcher = parseSearchQuery('TODO Café cafe\u0301 𝐀 "Exact Phrase"');
    expect(matcher.kind).toBe("boolean");
    if (matcher.kind !== "boolean") return;
    expect(matcher.groups[0].map((term) => term.text)).toEqual([
      "TODO",
      "Café",
      "cafe\u0301",
      "𝐀",
      "Exact Phrase",
    ]);
    expect(simpleTerm(parseSearchQuery("Abc"))).toBe("Abc");
  });

  for (const testCase of corpus.cases) {
    it(testCase.name, () => {
      expect(tokenize(testCase.query)).toEqual(testCase.tokens);
      expect(grammarShape(verdictOf(parseSearchQuery(testCase.query))))
        .toEqual(grammarShape(testCase.verdict));
    });
  }
});
