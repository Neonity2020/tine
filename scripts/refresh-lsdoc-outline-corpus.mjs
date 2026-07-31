#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const tineRoot = resolve(scriptDir, "..");
const lsdocRoot = resolve(
  process.argv[2] ?? resolve(tineRoot, "../lsdoc-outline-api"),
);
const revision = process.argv[3];
if (!/^[0-9a-f]{40}$/.test(revision ?? "")) {
  throw new Error(
    "pass the exact 40-character lowercase lsdoc Git revision as the second argument",
  );
}
const revisionProbe = spawnSync(
  "rtk",
  ["git", "-C", lsdocRoot, "rev-parse", "HEAD"],
  { encoding: "utf8" },
);
if (revisionProbe.status !== 0) {
  throw new Error(
    `cannot resolve source checkout HEAD: ${revisionProbe.stderr.trim()}`,
  );
}
const checkoutRevision = revisionProbe.stdout.trim();
if (revision !== checkoutRevision) {
  throw new Error(
    `supplied revision ${revision} does not match source checkout HEAD ${checkoutRevision}`,
  );
}
const output = resolve(
  tineRoot,
  "crates/tine-core/tests/fixtures/lsdoc-outline/public-harness.json",
);
const sourcePaths = [
  "harness/corpus.json",
  "harness/corpus.blockgate.json",
  "harness/corpus.blocks.json",
  "harness/corpus.inline.json",
  "harness/corpus.mined.json",
  "harness/corpus.org.json",
  "harness/corpus.org.mined.json",
  "harness/reported-divergences.json",
];
const seen = new Set();
const cases = [];
for (const sourcePath of sourcePaths) {
  const entries = JSON.parse(readFileSync(resolve(lsdocRoot, sourcePath), "utf8"));
  if (!Array.isArray(entries)) {
    throw new Error(`${sourcePath} is not a JSON array`);
  }
  for (const entry of entries) {
    const identity = `${sourcePath}:${entry?.id}`;
    if (
      typeof entry?.id !== "string" ||
      typeof entry?.input !== "string" ||
      seen.has(identity)
    ) {
      throw new Error(`${sourcePath} has an invalid or duplicate case`);
    }
    seen.add(identity);
    cases.push({
      source: sourcePath,
      id: entry.id,
      format: entry.format === "org" ? "org" : "md",
      input: entry.input,
    });
  }
}

const fixture = {
  schema: 1,
  provenance: {
    repository: "https://github.com/martinkoutecky/lsdoc",
    revision,
    sources: sourcePaths,
    selection:
      "All tracked public cases from the eight released harness arrays; generated-at-test-time, real-graph, and private corpora are excluded.",
  },
  cases,
};
writeFileSync(output, `${JSON.stringify(fixture, null, 2)}\n`);
console.log(`wrote ${cases.length} public lsdoc cases to ${output}`);
