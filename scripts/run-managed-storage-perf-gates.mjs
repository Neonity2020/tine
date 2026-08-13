#!/usr/bin/env node

// Exact-commit native managed-storage performance packet. This is deliberately
// local/release-only: it needs a real-scale copied graph and takes long enough
// that running it on every development commit would obstruct ordinary work.

import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

function arg(name, fallback) {
  const index = process.argv.indexOf(name);
  if (index < 0) return fallback;
  if (!process.argv[index + 1]) throw new Error(`missing value after ${name}`);
  return process.argv[index + 1];
}

const realGraphArg = arg("--real-graph");
if (!realGraphArg) throw new Error("missing --real-graph");
const realGraph = path.resolve(realGraphArg);
const secondaryGraph = path.resolve(arg("--secondary-graph", realGraph));
const outputDir = path.resolve(arg("--output-dir", "test-results/managed-storage-perf"));
if (!fs.existsSync(realGraph)) throw new Error(`real graph fixture is missing: ${realGraph}`);
if (!fs.existsSync(secondaryGraph)) throw new Error(`secondary graph fixture is missing: ${secondaryGraph}`);
fs.mkdirSync(outputDir, { recursive: true });

const commit = execFileSync("git", ["rev-parse", "HEAD"], { encoding: "utf8" }).trim();
const common = ["test", "-p", "tine-core", "--release"];
const runs = [
  {
    name: "aged-crash-reopen",
    test: "managed_crash_reopen_aged_history_manual_benchmark",
    env: { TINE_MANAGED_CRASH_REOPEN_GRAPH_COPY: realGraph },
  },
  {
    name: "projection-rebuild",
    test: "managed_projection_rebuild_manual_benchmark",
    env: { TINE_MANAGED_REBUILD_GRAPH_COPY: realGraph, TINE_MANAGED_REBUILD_ROUNDS: "8" },
  },
  {
    name: "real-graph-read",
    test: "managed_application_read_real_graph_copy_manual_gate",
    env: { TINE_MANAGED_READ_REAL_GRAPH_COPY: realGraph },
  },
  {
    name: "ordinary-save",
    test: "managed_application_save_100_and_10000_page_manual_benchmark",
    env: {
      TINE_MANAGED_APPLICATION_SAVE_BENCH_TARGET_BLOCKS: "10",
      // Three independently activated corpora retain between-run variation;
      // fourteen timed saves per corpus keep the aggregate above 40 samples
      // without paying the large 10k-page bootstrap cost five times.
      TINE_MANAGED_APPLICATION_SAVE_BENCH_RUNS: "3",
      TINE_MANAGED_APPLICATION_SAVE_BENCH_SAMPLES: "14",
    },
  },
  {
    name: "maximum-page-save",
    test: "managed_application_save_100_and_10000_page_manual_benchmark",
    env: {
      TINE_MANAGED_APPLICATION_SAVE_BENCH_TARGET_BLOCKS: "511",
      TINE_MANAGED_APPLICATION_SAVE_BENCH_RUNS: "3",
      TINE_MANAGED_APPLICATION_SAVE_BENCH_SAMPLES: "14",
    },
  },
  {
    name: "two-device-latency",
    test: "managed_two_device_sync_latency_real_corpora_manual_benchmark",
    env: {
      TINE_SYNC_LATENCY_TINE_TEST: secondaryGraph,
      TINE_SYNC_LATENCY_LOGSEQ_ANONYMIZED: realGraph,
    },
  },
  {
    name: "reconciliation-1000",
    test: "managed_reconciliation_and_shutdown_manual_benchmark",
    env: { TINE_MANAGED_RECONCILIATION_BENCH_PAGES: "1000" },
  },
  {
    name: "reconciliation-10000",
    test: "managed_reconciliation_and_shutdown_manual_benchmark",
    env: { TINE_MANAGED_RECONCILIATION_BENCH_PAGES: "10000" },
  },
];

const manifest = {
  schemaVersion: 1,
  kind: "managed-storage-native-performance-gates",
  testedCommit: commit,
  realGraph,
  secondaryGraph,
  startedAt: new Date().toISOString(),
  runs: [],
};

for (const entry of runs) {
  const command = ["cargo", ...common, entry.test, "--", "--ignored", "--nocapture"];
  console.log(`\n==> ${entry.name}: ${command.join(" ")}`);
  const started = Date.now();
  const result = spawnSync(command[0], command.slice(1), {
    cwd: process.cwd(),
    env: { ...process.env, ...entry.env },
    encoding: "utf8",
    maxBuffer: 128 * 1024 * 1024,
  });
  const output = `${result.stdout ?? ""}${result.stderr ?? ""}`;
  fs.writeFileSync(path.join(outputDir, `${entry.name}.log`), output);
  process.stdout.write(output);
  const receipt = {
    name: entry.name,
    test: entry.test,
    elapsedMs: Date.now() - started,
    status: result.status,
    log: `${entry.name}.log`,
  };
  manifest.runs.push(receipt);
  fs.writeFileSync(path.join(outputDir, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  if (result.status !== 0) {
    throw new Error(`${entry.name} failed with status ${result.status}; see ${receipt.log}`);
  }
}

manifest.completedAt = new Date().toISOString();
fs.writeFileSync(path.join(outputDir, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`\nManaged-storage native performance gates passed at ${commit}.`);
