// Compact-projection budget (SPEC §3): the policy file's shape, the evaluator's
// arithmetic, and — only when a corpus is opted in through
// TINE_PROJECTION_CORPUS — the real measurement against its ceilings.
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";
import {
  baselineFrom,
  baselineFromSearchScaling,
  evaluateBudget,
  evaluateSearchScaling,
  formatRows,
  formatSearchScalingRows,
  type BudgetRow,
  type SearchScalingRow,
} from "../scripts/lib/projection-budget.mjs";

const repo = path.resolve(__dirname, "..");
const policy = JSON.parse(fs.readFileSync(path.join(repo, "scripts/projection-budget-policy.json"), "utf8"));
const searchScalingContract = fs.readFileSync(path.join(repo, "docs/contracts/search-scaling.md"), "utf8");

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

const searchLimits = {
  page_only: { page: 100, block: 0 },
  block_only: { page: 0, block: 100 },
  combined: { page: 100, block: 100 },
  quick_switch: 100,
};

function searchQuery(label: string, needle: string, p95: number, hits: { pages: number; blocks: number }) {
  const surface = (name: string, pages: number, blocks: number) => ({
    surface: name,
    page_limit: name === "block_only" ? 0 : 100,
    block_limit: name === "block_only" || name === "combined" ? 100 : 0,
    actual_hits: { total: pages + blocks, pages, blocks },
    median_ms: p95 * 0.9,
    p95_ms: p95,
    raw_ns: [100, 110, 120],
  });
  return {
    label,
    needle,
    surfaces: [
      surface("page_only", hits.pages, 0),
      surface("block_only", 0, hits.blocks),
      surface("combined", hits.pages, hits.blocks),
      surface("quick_switch_100", hits.pages, 0),
    ],
  };
}

type SearchRole = "small" | "blocksLarge" | "namesLarge" | "large" | "pages" | "sentinel";

function rawSearchReport(role: SearchRole, p95: Record<string, number>) {
  const settings = {
    small: { corpus: "dense60k", pages: 1_000, blocks: 60_000, names: 1_010, augmented: true },
    blocksLarge: { corpus: "dense600k-fixed-names", pages: 1_000, blocks: 600_000, names: 1_010, augmented: true },
    namesLarge: { corpus: "dense60k-10k-pages", pages: 10_000, blocks: 60_000, names: 10_010, augmented: true },
    large: { corpus: "dense600k", pages: 10_000, blocks: 600_000, names: 10_010, augmented: true },
    pages: { corpus: "brikas", pages: 30_000, blocks: 55_000, names: 31_000, augmented: false },
    sentinel: { corpus: "page-sentinel", pages: 1_002, blocks: 1_002, names: 1_002, augmented: false },
  }[role];
  const augmentation = settings.augmented
    ? {
        provided: true,
        manifest_schema_version: 1,
        manifest_sha256: "a".repeat(64),
        sparse_query: "s7sparseanchor543",
        residual_negative_query: "match -s7sparseanchor543",
        older_true_raw_block_count: 7,
        newer_false_raw_block_count: 1_201,
        added_page_count: 2,
        added_block_count: 1_208,
        exact_raw_blocks_retained_in_array_order: true,
        raw_block_text_transformed: false,
        source_graph_modified: false,
      }
    : { provided: false };
  const queries = role === "sentinel"
    ? [searchQuery("exact_old_page", "AA S7 Exact Sentinel Ancient Page", p95.sentinel ?? 1, { pages: 1, blocks: 0 })]
    : role === "pages"
      ? [searchQuery("nohit_indexable_zqx1", "zqx1", p95.pages, { pages: 0, blocks: 0 })]
      : [
          searchQuery("nohit_indexable_zqx1", "zqx1", p95.nohit, { pages: 0, blocks: 0 }),
          searchQuery("broad_2char_synthetic", "你好", p95.broad, { pages: 100, blocks: 100 }),
          searchQuery("T2-sparse", "s7sparseanchor543", p95.sparse, { pages: 0, blocks: 7 }),
          searchQuery("T2-fp", '"page 11 block 11"', p95.fp, { pages: 0, blocks: 1 }),
        ];
  return {
    schema: "tine.s7_page_search_probe.v1",
    measurement_kind: "baseline_current_backend",
    backend_under_test: "current_production_backend",
    proposed_compact_backend: false,
    prototype_claims: false,
    repository_head: "f".repeat(40),
    current_backend_provenance: {
      public_base_sha: "e".repeat(40),
      production_source_hash: { kind: "git_tree_oid", value: "d".repeat(40) },
      production_source_scope: "crates/tine-core/src at repository_head",
      probe_source_sha256: "c".repeat(64),
    },
    production_source_dirty: false,
    corpus: settings.corpus,
    fixture_mode: role === "sentinel" ? "page_sentinel" : "copied_corpus",
    source_path: `/private/${role}`,
    graph_count: 1,
    page_count: settings.pages,
    block_count: settings.blocks,
    corpus_counts: {
      reported_pair_validation_scope: "original_corpus_before_scratch_augmentation",
      original_page_count: settings.pages,
      original_block_count: settings.blocks,
      augmentation_added_page_count: settings.augmented ? 2 : 0,
      augmentation_added_block_count: settings.augmented ? 1_208 : 0,
      actual_page_count_after_augmentation: settings.pages + (settings.augmented ? 2 : 0),
      actual_block_count_after_augmentation: settings.blocks + (settings.augmented ? 1_208 : 0),
    },
    navigation_name_inventory: {
      method: "quick_switch",
      count: settings.names,
      reported_count_scope: "original_corpus_before_scratch_augmentation",
      augmentation_added_owner_rows: settings.augmented ? 2 : 0,
      actual_count_after_augmentation: settings.names + (settings.augmented ? 2 : 0),
      count_unit: "navigable owner rows; aliases participate but do not add duplicate owner rows",
    },
    runs: 3,
    warmups_per_surface: 1,
    warmups_excluded: true,
    limits: structuredClone(searchLimits),
    scratch_augmentation: augmentation,
    sentinel_checks: role === "sentinel"
      ? {
          sentinel_created_before_later_pages: true,
          later_page_count: 1_001,
          matching_candidate_count: 1_002,
          matching_candidate_count_exceeds_1000: true,
          actual_projection_rowid_recency_asserted: false,
          quick_switch_returned_exact: true,
          page_only_returned_exact: true,
          passed: true,
        }
      : null,
    queries,
  };
}

function pairedSearchFixture(overrides: {
  blockBroadRatio?: number;
  blockSparseRatio?: number;
  bothBroadMs?: number;
  bothSparseMs?: number;
  namesNormalized?: number;
  namesTimeRatio?: number;
} = {}) {
  const blockBroadRatio = overrides.blockBroadRatio ?? 1.5;
  const blockSparseRatio = overrides.blockSparseRatio ?? 1.5;
  const bothBroadMs = overrides.bothBroadMs ?? 49.28;
  const bothSparseMs = overrides.bothSparseMs ?? 41.69;
  const namesNormalized = overrides.namesNormalized ?? 1.0;
  const ownerCountRatio = 10_012 / 1_012;
  const namesTimeRatio = overrides.namesTimeRatio ?? ownerCountRatio * namesNormalized;
  return {
    schema: "tine.search_scaling.v2",
    small: rawSearchReport("small", { nohit: 0.125, broad: 0.25, sparse: 2, fp: 4 }),
    blocksLarge: rawSearchReport("blocksLarge", {
      nohit: 1.25,
      broad: 0.25 * blockBroadRatio,
      sparse: 2 * blockSparseRatio,
      fp: 20,
    }),
    namesLarge: rawSearchReport("namesLarge", {
      nohit: 0.125 * namesTimeRatio,
      broad: 0.25 * namesTimeRatio,
      sparse: 3,
      fp: 20,
    }),
    large: rawSearchReport("large", { nohit: 1.25, broad: bothBroadMs, sparse: bothSparseMs, fp: 20 }),
    pages: rawSearchReport("pages", { pages: 0.0625 }),
    sentinel: rawSearchReport("sentinel", { sentinel: 0.5 }),
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
    expect(overS1.breaches.map((row: BudgetRow) => row.id)).toEqual(["S1"]);
    // 5% slower than the baseline is inside the 10% band; 12% slower is not.
    expect(evaluateBudget(measurement({ t1_build_ms: 735 }), base).breaches).toEqual([]);
    expect(evaluateBudget(measurement({ t1_build_ms: 784 }), base).breaches.map((row: BudgetRow) => row.id)).toEqual(["T1"]);
    // U1 is relative to the baseline too, band included.
    const overU1 = evaluateBudget(measurement({ u1: { one_block: { wchar: 331_000 }, sixty_block: { wchar: 989_000 } } }), base);
    expect(overU1.breaches.map((row: BudgetRow) => row.id)).toEqual(["U1/one_block"]);
    // S2 is absolute: no band.
    expect(evaluateBudget(measurement({ s2_write_ratio: 2.51 }), base).breaches.map((row: BudgetRow) => row.id)).toEqual(["S2"]);
  });

  it("leaves relative rows unjudged until a baseline is recorded", () => {
    const noBaseline = { ...policy, corpora: { anon: { ...policy.corpora.anon, baseline: null } } };
    const { rows, breaches } = evaluateBudget(measurement({ t1_build_ms: 1e9 }), noBaseline);
    expect(breaches).toEqual([]);
    expect(rows.find((row: BudgetRow) => row.id === "T1")?.ok).toBeNull();
    expect(rows.find((row: BudgetRow) => row.id === "S1")?.ok).toBe(true);
  });

  it("keeps the existing row formatter output compatible", () => {
    expect(formatRows([{ id: "S1", label: "size", value: 1.25, ceiling: 2, unit: "x", ok: true }])).toBe(
      "| row | value | ceiling | ok |\n|---|---:|---:|:-:|\n| S1 size | 1.25 x | 2.00 x | ok |",
    );
  });
});

describe("paired search-scaling budget", () => {
  it("keeps the approved contract values aligned with the executable policy", () => {
    expect(searchScalingContract).toMatch(/Block growth:[\s\S]{0,200}60,000 to 600,000 blocks[\s\S]{0,300}1\.5×/);
    expect(searchScalingContract).toMatch(/Both growing:[\s\S]{0,200}600,000-block, 10,000-page[\s\S]{0,300}49\.28 ms[\s\S]{0,100}41\.69 ms/);
    expect(searchScalingContract).toMatch(/Name growth:[\s\S]{0,200}blocks at 60,000[\s\S]{0,100}pages grow from 1,000 to 10,000[\s\S]{0,300}1\.10/);

    expect(policy.searchScaling.rows["T2-broad"]).toMatchObject({ ratioCeiling: 1.5, bothGrowingHardCeilingMs: 49.28 });
    expect(policy.searchScaling.rows["T2-sparse"]).toMatchObject({ ratioCeiling: 1.5, bothGrowingHardCeilingMs: 41.69 });
    expect(policy.searchScaling.rows["T2-names"].normalizedLinearityCeiling).toBe(1.1);

    expect(() => evaluateSearchScaling(pairedSearchFixture(), policy)).not.toThrow();
  });

  it("uses the literal probe labels", () => {
    expect(policy.searchScaling.rows["T2-nohit"].sourceLabel).toBe("nohit_indexable_zqx1");
    expect(policy.searchScaling.rows["T2-broad"].sourceLabel).toBe("broad_2char_synthetic");
    expect(policy.searchScaling.rows["T2-sparse"].sourceLabel).toBe("T2-sparse");
    expect(policy.searchScaling.rows["T2-fp"].sourceLabel).toBe("T2-fp");
  });

  it("passes exactly 1.5 and fails 1.5001 for fixed-name block growth", () => {
    const atCeiling = evaluateSearchScaling(pairedSearchFixture({ blockBroadRatio: 1.5 }), policy);
    expect(atCeiling.rows.find((row: SearchScalingRow) => row.id === "T2-broad")?.ok).toBe(true);
    expect(atCeiling.breaches).toEqual([]);

    const over = evaluateSearchScaling(pairedSearchFixture({ blockBroadRatio: 1.5001 }), policy);
    expect(policy.noiseBandFraction).toBe(0.1);
    expect(over.breaches.map((row: SearchScalingRow) => row.id)).toEqual(["T2-broad"]);

    const sparseOver = evaluateSearchScaling(pairedSearchFixture({ blockSparseRatio: 1.5001 }), policy);
    expect(sparseOver.breaches.map((row: SearchScalingRow) => row.id)).toEqual(["T2-sparse"]);
  });

  it("treats 49.28/41.69 ms as literal both-growing hard caps with no second margin", () => {
    const atCeiling = evaluateSearchScaling(pairedSearchFixture({ bothBroadMs: 49.28, bothSparseMs: 41.69 }), policy);
    expect(atCeiling.breaches).toEqual([]);
    expect(atCeiling.rows.find((row: SearchScalingRow) => row.id === "T2-broad")?.bothGrowingRatio).toBeCloseTo(49.28 / 0.25);
    expect(atCeiling.rows.find((row: SearchScalingRow) => row.id === "T2-sparse")?.bothGrowingRatio).toBeCloseTo(41.69 / 2);

    const broadOver = evaluateSearchScaling(pairedSearchFixture({ bothBroadMs: 49.280001 }), policy);
    expect(broadOver.breaches.map((row: SearchScalingRow) => row.id)).toEqual(["T2-broad"]);
    const sparseOver = evaluateSearchScaling(pairedSearchFixture({ bothSparseMs: 41.690001 }), policy);
    expect(sparseOver.breaches.map((row: SearchScalingRow) => row.id)).toEqual(["T2-sparse"]);
  });

  it("gates the worst no-hit/broad name timing normalized by measured owner-count growth at 1.10", () => {
    const atCeiling = evaluateSearchScaling(pairedSearchFixture({ namesNormalized: 1.1 }), policy);
    const names = atCeiling.rows.find((row: SearchScalingRow) => row.id === "T2-names")!;
    expect(names.nameGrowth?.smallOwnerCount).toBe(1_012);
    expect(names.nameGrowth?.namesLargeOwnerCount).toBe(10_012);
    expect(names.nameGrowth?.ownerCountRatio).toBeCloseTo(10_012 / 1_012);
    expect(names.nameGrowth).toMatchObject({
      smallPhysicalPageCount: 1_000,
      namesLargePhysicalPageCount: 10_000,
      smallFixedOwnerCount: 10,
      namesLargeFixedOwnerCount: 10,
      smallAugmentationOwnerCount: 2,
      namesLargeAugmentationOwnerCount: 2,
    });
    expect(names.nameGrowth?.maxNormalizedLinearity).toBeCloseTo(1.1);
    expect(names.ok).toBe(true);

    const over = evaluateSearchScaling(pairedSearchFixture({ namesNormalized: 1.1001 }), policy);
    expect(over.breaches.map((row: SearchScalingRow) => row.id)).toEqual(["T2-names"]);

    const cappedPolicy = structuredClone(policy);
    cappedPolicy.searchScaling.rows["T2-names"].executor = "capped candidate window";
    expect(() => evaluateSearchScaling(pairedSearchFixture(), cappedPolicy)).toThrow(/exhaustive navigation owner inventory/);
  });

  it("uses the timed owner inventory when the original-only ratio would hide a breach", () => {
    const timeRatio = 10.89;
    expect(timeRatio / (10_010 / 1_010)).toBeLessThan(1.1);

    const result = evaluateSearchScaling(pairedSearchFixture({ namesTimeRatio: timeRatio }), policy);
    const names = result.rows.find((row: SearchScalingRow) => row.id === "T2-names")!;
    expect(names.ratio).toBeCloseTo(timeRatio / (10_012 / 1_012));
    expect(names.ratio).toBeGreaterThan(1.1);
    expect(result.breaches.map((row: SearchScalingRow) => row.id)).toEqual(["T2-names"]);
  });

  it("keeps no-hit, false-positive, and page-search exceptions unjudged", () => {
    const withoutBaseline = structuredClone(policy);
    withoutBaseline.searchScaling.baseline = null;
    const { rows, breaches } = evaluateSearchScaling(pairedSearchFixture(), withoutBaseline);
    expect(breaches).toEqual([]);
    for (const id of ["T2-nohit", "T2-fp", "T2-pages"]) {
      const row = rows.find((entry: SearchScalingRow) => entry.id === id);
      expect(row?.ok, id).toBeNull();
      expect(row?.status, id).toBe("DIAGNOSTIC");
      expect(row?.ceiling, id).toBeNull();
      expect(row?.exception, id).toBeTruthy();
    }
    const rendered = formatSearchScalingRows(rows);
    expect(rendered).toContain("0.125 ms");
    expect(rendered).toContain("both-growing HARD ≤49.28 ms");
    expect(rendered).toContain("fixed-name block ratio 1.5x; both-growing ratio 197.12x");
    expect(rendered).toContain("1000→10000 physical pages + 10→10 fixed owner rows + 2→2 scratch-augmentation owner rows = 1012→10012 timed owner rows (9.893281x)");
    expect(rendered).toContain("max normalized time/name growth");
    expect(rendered).toContain("(no baseline)");
  });

  it("shows scalar baseline comparisons after a baseline is supplied", () => {
    const report = pairedSearchFixture();
    const withBaseline = structuredClone(policy);
    withBaseline.searchScaling.baseline = baselineFromSearchScaling(report, policy);
    const rows = evaluateSearchScaling(report, withBaseline).rows;
    expect(rows.filter((row: SearchScalingRow) => row.id !== "T2-names").every((row: SearchScalingRow) => row.baselineComparison !== null)).toBe(true);
    expect(rows.find((row: SearchScalingRow) => row.id === "T2-names")?.baselineComparison).toBeNull();
    expect(formatSearchScalingRows(rows)).toContain("small 1x; large 1x; ratio 1x baseline");
    expect(formatSearchScalingRows(rows)).toContain("page 1x baseline");
  });

  it.each([
    ["zero p95", (report: any) => { report.small.queries[0].surfaces[2].p95_ms = 0; }, /positive finite/],
    ["non-finite p95", (report: any) => { report.large.queries[1].surfaces[2].p95_ms = Infinity; }, /positive finite/],
    ["missing required row", (report: any) => { report.small.queries = report.small.queries.filter((query: any) => query.label !== "T2-sparse"); }, /query workload must match/],
    ["wrong raw run length", (report: any) => { report.large.queries[0].surfaces[2].raw_ns.pop(); }, /does not match runs/],
    ["missing actual owner count", (report: any) => { delete report.small.navigation_name_inventory.actual_count_after_augmentation; }, /actual_count_after_augmentation must be a positive integer/],
    ["inconsistent actual owner count", (report: any) => { report.small.navigation_name_inventory.actual_count_after_augmentation += 1; }, /actual count must equal original count plus augmentation owner rows/],
    ["wrong owner-count scope", (report: any) => { report.small.navigation_name_inventory.reported_count_scope = "all rows"; }, /reported_count_scope must identify the original corpus/],
  ] as Array<[string, (report: any) => void, RegExp]>)("rejects malformed input: %s", (_label, mutate, message) => {
    const report = pairedSearchFixture();
    mutate(report);
    expect(() => evaluateSearchScaling(report, policy)).toThrow(message);
  });

  it.each([
    ["needle", (report: any) => { report.large.queries[1].needle = "世界"; }, /query workload must match/],
    ["limits", (report: any) => { report.large.limits.combined.block = 99; }, /must be 100/],
    ["production tree", (report: any) => { report.large.current_backend_provenance.production_source_hash.value = "b".repeat(40); }, /identity does not match/],
    ["augmentation hash", (report: any) => { report.large.scratch_augmentation.manifest_sha256 = "b".repeat(64); }, /scratch augmentation must match/],
    ["original block count", (report: any) => { report.large.block_count = report.large.corpus_counts.original_block_count = 599_999; }, /must be 600000/],
    ["measurement runs", (report: any) => {
      report.large.runs = 4;
      report.large.queries.forEach((query: any) => query.surfaces.forEach((surface: any) => surface.raw_ns.push(130)));
    }, /large runs must match small/],
    ["warmup settings", (report: any) => { report.large.warmups_per_surface = 2; }, /large warmups_per_surface must match small/],
  ] as Array<[string, (report: any) => void, RegExp]>)("rejects a mismatched pair: %s", (_label, mutate, message) => {
    const report = pairedSearchFixture();
    mutate(report);
    expect(() => evaluateSearchScaling(report, policy)).toThrow(message);
  });

  it("rejects legacy wrappers and either missing scaling axis", () => {
    const legacy = pairedSearchFixture() as any;
    legacy.schema = "tine.search_scaling.v1";
    delete legacy.blocksLarge;
    delete legacy.namesLarge;
    expect(() => evaluateSearchScaling(legacy, policy)).toThrow(/wrapper\.schema must be tine\.search_scaling\.v2/);

    for (const role of ["blocksLarge", "namesLarge"] as const) {
      const report = pairedSearchFixture() as any;
      delete report[role];
      expect(() => evaluateSearchScaling(report, policy), role).toThrow(new RegExp(`${role} must be an object`));
    }
  });

  it.each([
    ["small pages", (report: any) => { report.small.page_count = report.small.corpus_counts.original_page_count = 999; }, /small original_page_count must be 1000/],
    ["blocksLarge pages", (report: any) => { report.blocksLarge.page_count = report.blocksLarge.corpus_counts.original_page_count = 1_001; }, /blocksLarge original_page_count must be 1000/],
    ["namesLarge blocks", (report: any) => { report.namesLarge.block_count = report.namesLarge.corpus_counts.original_block_count = 60_001; }, /namesLarge original_block_count must be 60000/],
    ["large pages", (report: any) => { report.large.page_count = report.large.corpus_counts.original_page_count = 9_999; }, /large original_page_count must be 10000/],
    ["fixed low-name inventory", (report: any) => {
      report.blocksLarge.navigation_name_inventory.count += 1;
      report.blocksLarge.navigation_name_inventory.actual_count_after_augmentation += 1;
    }, /small and blocksLarge navigation owner inventory counts must match/],
    ["fixed high-name inventory", (report: any) => {
      report.large.navigation_name_inventory.count += 1;
      report.large.navigation_name_inventory.actual_count_after_augmentation += 1;
    }, /namesLarge and large navigation owner inventory counts must match/],
    ["inventory method", (report: any) => { report.namesLarge.navigation_name_inventory.method = "other"; }, /navigation_name_inventory\.method must match small/],
    ["non-growing inventory", (report: any) => {
      report.namesLarge.navigation_name_inventory.count = report.large.navigation_name_inventory.count = 1_010;
      report.namesLarge.navigation_name_inventory.actual_count_after_augmentation = report.large.navigation_name_inventory.actual_count_after_augmentation = 1_012;
    }, /namesLarge navigation owner inventory count minus small must equal 9000/],
    ["extra owner growth", (report: any) => {
      report.namesLarge.navigation_name_inventory.count += 1;
      report.large.navigation_name_inventory.count += 1;
      report.namesLarge.navigation_name_inventory.actual_count_after_augmentation += 1;
      report.large.navigation_name_inventory.actual_count_after_augmentation += 1;
    }, /namesLarge navigation owner inventory count minus small must equal 9000/],
  ] as Array<[string, (report: any) => void, RegExp]>)("rejects a mismatched or non-growing axis: %s", (_label, mutate, message) => {
    const report = pairedSearchFixture();
    mutate(report);
    expect(() => evaluateSearchScaling(report, policy)).toThrow(message);
  });

  it("requires the four scaling-axis reports to share workload and augmentation", () => {
    const workload = pairedSearchFixture() as any;
    workload.namesLarge.queries.reverse();
    expect(() => evaluateSearchScaling(workload, policy)).toThrow(/namesLarge query workload must match small/);

    const augmentation = pairedSearchFixture() as any;
    augmentation.blocksLarge.scratch_augmentation.sparse_query = "different";
    expect(() => evaluateSearchScaling(augmentation, policy)).toThrow(/blocksLarge scratch augmentation must match small/);
  });

  it("requires the page and sentinel role-specific workloads to remain unaugmented", () => {
    const augmentedPages = pairedSearchFixture() as any;
    augmentedPages.pages.scratch_augmentation.provided = true;
    expect(() => evaluateSearchScaling(augmentedPages, policy)).toThrow(/pages\.scratch_augmentation\.provided must be false/);

    const extraSentinelWork = pairedSearchFixture() as any;
    const extra = structuredClone(extraSentinelWork.sentinel.queries[0]);
    extra.label = "other";
    extraSentinelWork.sentinel.queries.push(extra);
    expect(() => evaluateSearchScaling(extraSentinelWork, policy)).toThrow(/sentinel query workload must contain only exact_old_page/);
  });

  it("requires owner-row augmentation counts to match each fixture", () => {
    const augmented = pairedSearchFixture() as any;
    for (const role of ["small", "blocksLarge", "namesLarge", "large"] as const) {
      augmented[role].navigation_name_inventory.augmentation_added_owner_rows = 3;
      augmented[role].navigation_name_inventory.actual_count_after_augmentation += 1;
    }
    expect(() => evaluateSearchScaling(augmented, policy)).toThrow(/must report 2 scratch-augmentation owner rows/);

    const unaugmented = pairedSearchFixture() as any;
    unaugmented.pages.navigation_name_inventory.augmentation_added_owner_rows = 1;
    unaugmented.pages.navigation_name_inventory.actual_count_after_augmentation += 1;
    expect(() => evaluateSearchScaling(unaugmented, policy)).toThrow(/must report no scratch-augmentation owner rows/);
  });

  it("requires one production/backend identity across every role", () => {
    for (const role of ["small", "blocksLarge", "namesLarge", "large", "pages", "sentinel"] as const) {
      const report = pairedSearchFixture() as any;
      report[role].backend_under_test = `other-${role}`;
      expect(() => evaluateSearchScaling(report, policy), role).toThrow(/production\/backend\/probe identity does not match/);
    }
  });

  it("requires paired storage provenance to match whenever any report supplies it", () => {
    const mismatched = pairedSearchFixture() as any;
    for (const role of ["small", "blocksLarge", "namesLarge", "large", "pages", "sentinel"] as const) {
      mismatched[role].current_backend_provenance.paired_storage_commit = "a".repeat(40);
    }
    mismatched.large.current_backend_provenance.paired_storage_commit = "b".repeat(40);
    expect(() => evaluateSearchScaling(mismatched, policy)).toThrow(/production\/backend\/probe identity does not match/);

    const missing = pairedSearchFixture() as any;
    for (const role of ["small", "blocksLarge", "namesLarge", "large", "pages", "sentinel"] as const) {
      missing[role].current_backend_provenance.paired_storage_commit = "a".repeat(40);
    }
    delete missing.large.current_backend_provenance.paired_storage_commit;
    expect(() => evaluateSearchScaling(missing, policy)).toThrow(/production\/backend\/probe identity does not match/);
  });

  it("requires one probe harness across all six reports", () => {
    for (const role of ["blocksLarge", "namesLarge", "large", "pages", "sentinel"] as const) {
      const report = pairedSearchFixture() as any;
      report[role].current_backend_provenance.probe_source_sha256 = "b".repeat(64);
      expect(() => evaluateSearchScaling(report, policy), role).toThrow(/production\/backend\/probe identity does not match/);
    }
  });

  it("requires copied-corpus fixtures, the sentinel fixture, and excluded warmups", () => {
    for (const role of ["small", "blocksLarge", "namesLarge", "large", "pages", "sentinel"] as const) {
      const wrongFixture = pairedSearchFixture() as any;
      wrongFixture[role].fixture_mode = "wrong";
      expect(() => evaluateSearchScaling(wrongFixture, policy), role).toThrow(/fixture_mode must be/);

      const includedWarmup = pairedSearchFixture() as any;
      includedWarmup[role].warmups_excluded = false;
      expect(() => evaluateSearchScaling(includedWarmup, policy), role).toThrow(/warmups_excluded must be true/);
    }
  });

  it("rejects missing correctness, incorrect hit cardinality, and prototype input", () => {
    const missingCorrectness = pairedSearchFixture();
    delete (missingCorrectness.sentinel as any).sentinel_checks;
    expect(() => evaluateSearchScaling(missingCorrectness, policy)).toThrow(/sentinel_checks must be an object/);

    const wrongSparse = pairedSearchFixture();
    wrongSparse.small.queries.find((query: any) => query.label === "T2-sparse")!.surfaces[2].actual_hits = { total: 6, pages: 0, blocks: 6 };
    expect(() => evaluateSearchScaling(wrongSparse, policy)).toThrow(/T2-sparse.*must be 7/);

    const prototype = pairedSearchFixture();
    prototype.small.proposed_compact_backend = true;
    expect(() => evaluateSearchScaling(prototype, policy)).toThrow(/proposed_compact_backend must be false/);

    const microbench = pairedSearchFixture();
    microbench.small.prototype_claims = true;
    expect(() => evaluateSearchScaling(microbench, policy)).toThrow(/prototype_claims must be false/);
  });

  it("requires evidence that the old exact page survived more than 1000 newer matching candidates", () => {
    for (const mutate of [
      (checks: any) => { delete checks.matching_candidate_count; },
      (checks: any) => { checks.matching_candidate_count = 1_000; },
      (checks: any) => { checks.later_page_count = 999; checks.matching_candidate_count = 1_000; },
      (checks: any) => { checks.matching_candidate_count_exceeds_1000 = false; },
    ]) {
      const report = pairedSearchFixture() as any;
      mutate(report.sentinel.sentinel_checks);
      expect(() => evaluateSearchScaling(report, policy)).toThrow(/matching_candidate_count|later_page_count/);
    }
  });

  it("records only scalar summaries and reproducibility identities", () => {
    const baseline = baselineFromSearchScaling(pairedSearchFixture(), policy) as any;
    expect(baseline.counts.small).toEqual({ original_pages: 1_000, original_blocks: 60_000, navigation_owner_rows: 1_010 });
    expect(baseline.counts.blocksLarge).toEqual({ original_pages: 1_000, original_blocks: 600_000, navigation_owner_rows: 1_010 });
    expect(baseline.counts.namesLarge).toEqual({ original_pages: 10_000, original_blocks: 60_000, navigation_owner_rows: 10_010 });
    expect(baseline.rows["T2-names"]).toMatchObject({
      small_owner_count: 1_012,
      names_large_owner_count: 10_012,
      owner_count_ratio: 10_012 / 1_012,
    });
    expect(baseline.counts.large.original_blocks).toBe(600_000);
    expect(baseline.augmentation_sha256).toBe("a".repeat(64));
    expect(baseline.queries["T2-sparse"]).toBe("s7sparseanchor543");
    expect(baseline.queries["T2-pages"]).toBe("zqx1");
    expect(baseline.source_identity.probe_source_sha256_by_role).toEqual({
      small: "c".repeat(64),
      blocksLarge: "c".repeat(64),
      namesLarge: "c".repeat(64),
      large: "c".repeat(64),
      pages: "c".repeat(64),
      sentinel: "c".repeat(64),
    });
    expect(JSON.stringify(baseline)).not.toContain("/private/");
    expect(JSON.stringify(baseline)).not.toContain("raw_ns");
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
