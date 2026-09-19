// Compact-projection budget (SPEC §3): the policy file's shape, the evaluator's
// arithmetic, and — only when a corpus is opted in through
// TINE_PROJECTION_CORPUS — the real measurement against its ceilings.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";
import { baselineFrom, evaluateBudget } from "../scripts/lib/projection-budget.mjs";

const repo = path.resolve(__dirname, "..");
const policy = JSON.parse(fs.readFileSync(path.join(repo, "scripts/projection-budget-policy.json"), "utf8"));

function measurement(overrides: Record<string, unknown> = {}) {
  return {
    corpus: "anon",
    s1_ratio: 5.0,
    s2_write_ratio: 2.0,
    t1_build_ms: 700,
    m1_peak_rss_delta_kb: 40_000,
    u1: { one_block: { wchar: 300_000 }, sixty_block: { wchar: 900_000 } },
    t2_search: [{ chars: 3, p95_ms: 15 }, { chars: 8, p95_ms: 12 }],
    t3_queries: [{ query: "(task TODO)", p95_ms: 5 }],
    ...overrides,
  };
}

describe("projection budget policy", () => {
  it("names every corpus with the §3 ceilings", () => {
    for (const name of ["anon", "brikas", "synthetic10k"]) {
      const corpus = policy.corpora[name];
      expect(corpus, name).toBeTruthy();
      for (const key of ["s1", "s2", "t1_multiplier", "t2_multiplier", "t3_multiplier", "m1_multiplier", "u1_multiplier"]) {
        expect(typeof corpus.ceilings[key], `${name}.${key}`).toBe("number");
      }
    }
    expect(policy.corpora.synthetic10k.ceilings.t2_absolute_ms).toBe(150);
    expect(policy.corpora.anon.ceilings.s1).toBe(7);
  });

  it("flags an absolute row over its ceiling and gives relative rows the noise band", () => {
    const base = { ...policy, corpora: { anon: { ...policy.corpora.anon, baseline: baselineFrom(measurement()) } } };
    const ok = evaluateBudget(measurement(), base);
    expect(ok.breaches).toEqual([]);
    const overS1 = evaluateBudget(measurement({ s1_ratio: 7.01 }), base);
    expect(overS1.breaches.map((row) => row.id)).toEqual(["S1"]);
    // 5% slower than the baseline is inside the 10% band; 12% slower is not.
    expect(evaluateBudget(measurement({ t1_build_ms: 735 }), base).breaches).toEqual([]);
    expect(evaluateBudget(measurement({ t1_build_ms: 784 }), base).breaches.map((row) => row.id)).toEqual(["T1"]);
    // U1 is relative to the baseline too, band included.
    const overU1 = evaluateBudget(measurement({ u1: { one_block: { wchar: 331_000 }, sixty_block: { wchar: 989_000 } } }), base);
    expect(overU1.breaches.map((row) => row.id)).toEqual(["U1/one_block"]);
    // S2 is absolute: no band.
    expect(evaluateBudget(measurement({ s2_write_ratio: 2.51 }), base).breaches.map((row) => row.id)).toEqual(["S2"]);
  });

  it("leaves relative rows unjudged until a baseline is recorded", () => {
    const noBaseline = { ...policy, corpora: { anon: { ...policy.corpora.anon, baseline: null } } };
    const { rows, breaches } = evaluateBudget(measurement({ t1_build_ms: 1e9 }), noBaseline);
    expect(breaches).toEqual([]);
    expect(rows.find((row) => row.id === "T1")?.ok).toBeNull();
    expect(rows.find((row) => row.id === "S1")?.ok).toBe(true);
  });
});

describe("projection budget measurement", () => {
  it.skipIf(!process.env.TINE_PROJECTION_CORPUS)(
    "the opted-in corpus meets every ceiling",
    () => {
      execFileSync("node", ["scripts/measure-projection.mjs", "--corpus", process.env.TINE_PROJECTION_BUDGET_CORPUS ?? "anon"], {
        cwd: repo,
        stdio: "inherit",
      });
    },
    20 * 60 * 1000,
  );
});
