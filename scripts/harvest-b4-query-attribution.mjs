#!/usr/bin/env node

// Run the release-only Direct query benchmark repeatedly and summarize class
// cost, projection readiness, scoped invalidation, and facet scaling.
//
//   node scripts/harvest-b4-query-attribution.mjs \
//     --graph /path/to/copied-or-corpus-graph --runs 3 --rounds 9 \
//     --facet-sizes 1000,4000

import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function flag(name, fallback) {
  const index = process.argv.indexOf(`--${name}`);
  return index >= 0 && process.argv[index + 1] ? process.argv[index + 1] : fallback;
}

const runs = Number.parseInt(flag("runs", "3"), 10);
const rounds = flag("rounds", "9");
const graph = flag("graph", "");
const facetSizes = flag("facet-sizes", "1000,4000");
const phase = flag("phase", "after");
if (!graph) throw new Error("missing --graph /path/to/graph");
if (!Number.isInteger(runs) || runs < 1) throw new Error("--runs must be positive");

function fields(line) {
  const out = {};
  for (const token of line.trim().split(/\s+/).slice(1)) {
    const at = token.indexOf("=");
    if (at > 0) out[token.slice(0, at)] = token.slice(at + 1);
  }
  return out;
}

function median(values) {
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[sorted.length >> 1];
}

const queryRows = new Map();
const hitRows = [];
const invalidationRows = new Map();
const facetRows = new Map();
const sampleRows = [];
const readinessRows = [];
let measuredPages = 0;
let measuredBlocks = 0;

for (let run = 0; run < runs; run += 1) {
  process.stderr.write(`# run ${run + 1}/${runs}\n`);
  const testName = "direct_query_latency_manual_benchmark";
  const result = spawnSync(
    "cargo",
    [
      "test",
      "-p",
      "tine-core",
      "--release",
      "--lib",
      testName,
      "--",
      "--ignored",
      "--nocapture",
      "--test-threads=1",
    ],
    {
      cwd: repoRoot,
      encoding: "utf8",
      maxBuffer: 256 * 1024 * 1024,
      env: {
        ...process.env,
        TINE_DIRECT_QUERY_BENCH_GRAPH_COPY: path.resolve(graph),
        TINE_DIRECT_QUERY_BENCH_ROUNDS: rounds,
        TINE_DIRECT_QUERY_BENCH_FACET_SIZES: facetSizes,
        TINE_B4_QUERY_BENCH_RUN: String(run + 1),
      },
    },
  );
  if (result.status !== 0) {
    process.stderr.write(result.stdout ?? "");
    process.stderr.write(result.stderr ?? "");
    throw new Error(`${testName} failed on run ${run + 1}`);
  }
  for (const line of `${result.stdout}\n${result.stderr}`.split("\n")) {
    const queryAt = line.indexOf("b4_query ");
    const hitAt = line.indexOf("b4_projection_hit_rate ");
    const invalidationAt = line.indexOf("b4_invalidation ");
    const facetAt = line.indexOf("b4_facet ");
    const sampleAt = line.indexOf("b4_query_sample ");
    const readinessAt = line.indexOf("b4_readiness ");
    if (queryAt >= 0) {
      const row = fields(line.slice(queryAt));
      measuredPages = Number.parseInt(row.pages, 10);
      measuredBlocks = Number.parseInt(row.blocks, 10);
      const key = `${row.class}\0${row.phase}`;
      if (!queryRows.has(key)) queryRows.set(key, []);
      queryRows.get(key).push(row);
    } else if (hitAt >= 0) {
      hitRows.push(fields(line.slice(hitAt)));
    } else if (invalidationAt >= 0) {
      const row = fields(line.slice(invalidationAt));
      if (!invalidationRows.has(row.edit)) invalidationRows.set(row.edit, []);
      invalidationRows.get(row.edit).push(row);
    } else if (facetAt >= 0) {
      const row = fields(line.slice(facetAt));
      const key = `${row.family}\0${row.pages}`;
      if (!facetRows.has(key)) facetRows.set(key, []);
      facetRows.get(key).push(row);
    } else if (sampleAt >= 0) {
      sampleRows.push(fields(line.slice(sampleAt)));
    } else if (readinessAt >= 0) {
      readinessRows.push(fields(line.slice(readinessAt)));
    }
  }
}

if (phase === "after") {
  const candidateOnlyClasses = new Set([
    "sparse_task", "page_ref", "task_non_sparse", "block_property",
    "page_property", "page_tags", "page", "namespace", "journal",
    "mixed_and", "complete_or", "boolean_composition",
  ]);
  for (const row of sampleRows) {
    if (!candidateOnlyClasses.has(row.class)) continue;
    if (
      Number.parseInt(row.candidateQueriesCompleted, 10) !== 1 ||
      Number.parseInt(row.fallbackReads, 10) !== 0 ||
      Number.parseInt(row.fullGraphEvaluations, 10) !== 0
    ) {
      throw new Error(`post-fix candidate-only sample failed: ${JSON.stringify(row)}`);
    }
  }
}

if (queryRows.size === 0) throw new Error("benchmark emitted no b4_query rows");
const classes = [...new Set([...queryRows.keys()].map((key) => key.split("\0")[0]))];
const invalidatedTotal = classes.reduce((sum, className) => {
  const rows = queryRows.get(`${className}\0invalidated_ready`) ?? [];
  return sum + median(rows.map((row) => Number.parseFloat(row.median_ms)));
}, 0);

const report = [];
report.push(`# runs=${runs} rounds_per_phase=${rounds} pages=${measuredPages} blocks=${measuredBlocks}`);
report.push("");
report.push("## Query classes (median of run medians)");
report.push("| class | memo median/p95/max ms | invalidated-ready median/p95/max ms | invalidated share | growth | indexed reads/run |");
report.push("| --- | ---: | ---: | ---: | ---: | ---: |");
for (const className of classes) {
  const memoRows = queryRows.get(`${className}\0memo`) ?? [];
  const memo = median(memoRows.map((row) => Number.parseFloat(row.median_ms)));
  const memoP95 = median(memoRows.map((row) => Number.parseFloat(row.p95_ms)));
  const memoMax = median(memoRows.map((row) => Number.parseFloat(row.max_ms)));
  const invalidatedRows = queryRows.get(`${className}\0invalidated_ready`) ?? [];
  const invalidated = median(invalidatedRows.map((row) => Number.parseFloat(row.median_ms)));
  const invalidatedP95 = median(invalidatedRows.map((row) => Number.parseFloat(row.p95_ms)));
  const invalidatedMax = median(invalidatedRows.map((row) => Number.parseFloat(row.max_ms)));
  const indexed = median(invalidatedRows.map((row) => Number.parseInt(row.indexed_reads, 10)));
  report.push(
    `| ${className} | ${memo.toFixed(6)} / ${memoP95.toFixed(6)} / ${memoMax.toFixed(6)} | ${invalidated.toFixed(6)} / ${invalidatedP95.toFixed(6)} / ${invalidatedMax.toFixed(6)} | ${((invalidated / invalidatedTotal) * 100).toFixed(1)}% | ${(invalidated / Math.max(memo, 0.000001)).toFixed(2)}x | ${indexed} |`,
  );
}

report.push("");
report.push("## Per-run invalidated-ready medians");
report.push("| class | run 1 ms | run 2 ms | run 3 ms |");
report.push("| --- | ---: | ---: | ---: |");
for (const className of classes) {
  const rows = queryRows.get(`${className}\0invalidated_ready`) ?? [];
  report.push(`| ${className} | ${rows.map((row) => Number.parseFloat(row.median_ms).toFixed(6)).join(" | ")} |`);
}

report.push("");
report.push("## Invalidated-ready sample counters");
report.push("| class | run | sample | candidateQueriesCompleted | fallbackReads | fullGraphEvaluations | evaluatedPages | medianMs |");
report.push("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
for (const row of sampleRows) {
  report.push(`| ${row.class} | ${row.run} | ${row.sample} | ${row.candidateQueriesCompleted} | ${row.fallbackReads} | ${row.fullGraphEvaluations} | ${row.evaluatedPages} | ${Number.parseFloat(row.medianMs).toFixed(6)} |`);
}

const immediate = queryRows.get("sparse_task\0data_rev_immediate") ?? [];
report.push("");
report.push("## Projection readiness at immediate post-delta/dataRev proxy");
report.push("| samples | ready hits | ready misses | hit rate | indexed reads | median query ms |");
report.push("| ---: | ---: | ---: | ---: | ---: | ---: |");
const samples = hitRows.reduce((sum, row) => sum + Number.parseInt(row.samples, 10), 0);
const hits = hitRows.reduce((sum, row) => sum + Number.parseInt(row.ready_hits, 10), 0);
const misses = hitRows.reduce((sum, row) => sum + Number.parseInt(row.ready_misses, 10), 0);
const indexed = hitRows.reduce((sum, row) => sum + Number.parseInt(row.indexed_reads, 10), 0);
report.push(
  `| ${samples} | ${hits} | ${misses} | ${samples === 0 ? "n/a" : `${((hits / samples) * 100).toFixed(1)}%`} | ${indexed} | ${median(immediate.map((row) => Number.parseFloat(row.median_ms))).toFixed(6)} |`,
);

report.push("");
report.push("## Readiness distribution");
report.push("| save | generation | immediate ready | ready latency ms | terminal event | candidate reads | fallback reads | oracle equal |");
report.push("| --- | ---: | --- | ---: | --- | ---: | ---: | --- |");
for (const row of readinessRows) {
  report.push(`| ${row.save} | ${row.generation} | ${row.immediate_ready} | ${Number.parseFloat(row.ready_latency_ms).toFixed(6)} | ${row.terminal_event} | ${row.candidate_reads} | ${row.fallback_reads} | ${row.oracle_equal} |`);
}

report.push("");
report.push("## Scoped invalidation (median counts)");
report.push("| edit | before | retained | evicted | generation delta |");
report.push("| --- | ---: | ---: | ---: | ---: |");
for (const [edit, rows] of invalidationRows) {
  report.push(
    `| ${edit} | ${median(rows.map((row) => Number.parseInt(row.before, 10)))} | ${median(rows.map((row) => Number.parseInt(row.retained, 10)))} | ${median(rows.map((row) => Number.parseInt(row.evicted, 10)))} | ${median(rows.map((row) => Number.parseInt(row.cache_gen_after, 10) - Number.parseInt(row.cache_gen_before, 10)))} |`,
  );
}

report.push("");
report.push("## Facet reads (median of run medians)");
report.push("| family | pages | blocks | median ms | growth from smaller graph |");
report.push("| --- | ---: | ---: | ---: | ---: |");
const facets = [...facetRows.values()]
  .map((rows) => ({
    family: rows[0].family,
    pages: Number.parseInt(rows[0].pages, 10),
    blocks: Number.parseInt(rows[0].blocks, 10),
    milliseconds: median(rows.map((row) => Number.parseFloat(row.median_ms))),
  }))
  .sort((left, right) => left.family.localeCompare(right.family) || left.pages - right.pages);
for (const row of facets) {
  const base = facets.find((candidate) => candidate.family === row.family);
  report.push(
    `| ${row.family} | ${row.pages} | ${row.blocks} | ${row.milliseconds.toFixed(6)} | ${(row.milliseconds / base.milliseconds).toFixed(2)}x |`,
  );
}

process.stdout.write(`${report.join("\n")}\n`);
