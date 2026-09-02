#!/usr/bin/env node

// Attribute sequential Managed page creation at fixed checkpoints, separating
// foreground save work from the repro harness's forced drain cadence.
//
//   node scripts/harvest-a4-save-attribution.mjs \
//     --runs 3 --checkpoints 50,200,400 --cadences never,every

import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function flag(name, fallback) {
  const index = process.argv.indexOf(`--${name}`);
  return index >= 0 && process.argv[index + 1] ? process.argv[index + 1] : fallback;
}

const runs = Number.parseInt(flag("runs", "3"), 10);
const checkpointsArgument = flag("checkpoints", "50,200,400");
const checkpoints = checkpointsArgument.split(",").map((value) => Number.parseInt(value, 10));
const cadences = flag("cadences", "never,every").split(",");
if (!Number.isInteger(runs) || runs < 1) throw new Error("--runs must be positive");
if (checkpoints.some((value) => !Number.isInteger(value) || value < 1)) {
  throw new Error("--checkpoints must contain positive integers");
}
const maxPages = Math.max(...checkpoints);

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

function slot(map, key) {
  if (!map.has(key)) map.set(key, []);
  return map.get(key);
}

const summaries = new Map();
const saveStages = new Map();
const saveCounters = new Map();
const drainStages = new Map();
const testName = "a4_repro_managed_page_creation_wedges_at_the_run_local_budget";

for (const cadence of cadences) {
  for (let run = 0; run < runs; run += 1) {
    process.stderr.write(`# cadence=${cadence} run ${run + 1}/${runs}\n`);
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
          TINE_A4_MAX_PAGES: String(maxPages),
          TINE_A4_DRAIN_CADENCE: cadence,
          TINE_A4_ATTRIBUTION_CHECKPOINTS: checkpointsArgument,
        },
      },
    );
    if (result.status !== 0) {
      process.stderr.write(result.stdout ?? "");
      process.stderr.write(result.stderr ?? "");
      throw new Error(`${testName} failed for cadence=${cadence} run=${run + 1}`);
    }
    for (const line of `${result.stdout}\n${result.stderr}`.split("\n")) {
      const checkpointAt = line.indexOf("a4_save_checkpoint ");
      const saveStageAt = line.indexOf("a4_save_stage ");
      const counterAt = line.indexOf("a4_save_counter ");
      const drainStageAt = line.indexOf("a4_drain_stage ");
      if (checkpointAt >= 0) {
        const row = fields(line.slice(checkpointAt));
        slot(summaries, `${row.cadence}\0${row.checkpoint}`).push(row);
      } else if (saveStageAt >= 0) {
        const row = fields(line.slice(saveStageAt));
        slot(saveStages, `${row.cadence}\0${row.checkpoint}\0${row.stage}`).push(row);
      } else if (counterAt >= 0) {
        const row = fields(line.slice(counterAt));
        slot(saveCounters, `${row.cadence}\0${row.checkpoint}\0${row.counter}`).push(row);
      } else if (drainStageAt >= 0) {
        const row = fields(line.slice(drainStageAt));
        slot(drainStages, `${row.cadence}\0${row.checkpoint}\0${row.stage}`).push(row);
      }
    }
  }
}

if (summaries.size === 0) throw new Error("benchmark emitted no A4 attribution checkpoints");
const cadenceLabels = [...new Set([...summaries.keys()].map((key) => key.split("\0")[0]))];
const report = [`# runs=${runs} checkpoints=${checkpoints.join(",")} cadences=${cadenceLabels.join(",")}`];

report.push("");
report.push("## Save versus harness drain (median of runs)");
report.push("| cadence | pages | last save ms | cumulative save ms | cumulative drain ms | drain share of elapsed | pending |");
report.push("| --- | ---: | ---: | ---: | ---: | ---: | ---: |");
for (const cadence of cadenceLabels) {
  for (const checkpoint of checkpoints) {
    const rows = summaries.get(`${cadence}\0${checkpoint}`) ?? [];
    const save = median(rows.map((row) => Number.parseFloat(row.save_ms)));
    const cumulativeSave = median(rows.map((row) => Number.parseFloat(row.cumulative_save_ms)));
    const cumulativeDrain = median(rows.map((row) => Number.parseFloat(row.cumulative_drain_ms)));
    const elapsed = median(rows.map((row) => Number.parseFloat(row.elapsed_ms)));
    const pending = median(rows.map((row) => Number.parseInt(row.pending, 10)));
    report.push(
      `| ${cadence} | ${checkpoint} | ${save.toFixed(6)} | ${cumulativeSave.toFixed(3)} | ${cumulativeDrain.toFixed(3)} | ${((cumulativeDrain / Math.max(elapsed, 0.000001)) * 100).toFixed(1)}% | ${pending} |`,
    );
  }
}

for (const cadence of cadenceLabels) {
  const stageNames = [...new Set([...saveStages.keys()]
    .filter((key) => key.startsWith(`${cadence}\0`))
    .map((key) => key.split("\0")[2]))];
  report.push("");
  report.push(`## Foreground save stages: ${cadence}`);
  report.push(
    `| stage | ${checkpoints.map((value) => `N=${value} ms`).join(" | ")} | ${checkpoints.map((value) => `N=${value} share`).join(" | ")} | growth ${checkpoints[0]}->${checkpoints.at(-1)} |`,
  );
  report.push(`| --- |${" ---: |".repeat(checkpoints.length * 2 + 1)}`);
  const stageRows = stageNames.map((stage) => {
    const milliseconds = checkpoints.map((checkpoint) => median(
      (saveStages.get(`${cadence}\0${checkpoint}\0${stage}`) ?? [])
        .map((row) => Number.parseFloat(row.stage_ms)),
    ));
    const shares = checkpoints.map((checkpoint, index) => {
      const save = median((summaries.get(`${cadence}\0${checkpoint}`) ?? [])
        .map((row) => Number.parseFloat(row.save_ms)));
      return milliseconds[index] / Math.max(save, 0.000001);
    });
    const growth = milliseconds.at(-1) / Math.max(milliseconds[0], 0.000001);
    return { stage, milliseconds, shares, growth, last: milliseconds.at(-1) };
  }).sort((left, right) => right.last - left.last);
  for (const row of stageRows) {
    report.push(
      `| ${row.stage} | ${row.milliseconds.map((value) => value.toFixed(6)).join(" | ")} | ${row.shares.map((value) => `${(value * 100).toFixed(1)}%`).join(" | ")} | ${row.growth.toFixed(2)}x |`,
    );
  }
  report.push("");
  report.push("Top three stages by absolute growth to the final checkpoint:");
  for (const row of [...stageRows]
    .sort((left, right) => (right.last - right.milliseconds[0]) - (left.last - left.milliseconds[0]))
    .slice(0, 3)) {
    report.push(
      `- ${row.stage}: +${(row.last - row.milliseconds[0]).toFixed(6)} ms (${row.growth.toFixed(2)}x)`,
    );
  }
}

if (drainStages.size > 0) {
  report.push("");
  report.push("## Forced-drain stages (last drain at checkpoint; median of runs)");
  report.push(`| stage | ${checkpoints.map((value) => `N=${value} ms`).join(" | ")} | growth |`);
  report.push(`| --- |${" ---: |".repeat(checkpoints.length + 1)}`);
  const drainNames = [...new Set([...drainStages.keys()].map((key) => key.split("\0")[2]))];
  for (const stage of drainNames) {
    const milliseconds = checkpoints.map((checkpoint) => {
      const matching = [...drainStages.entries()].find(([key]) => {
        const [, foundCheckpoint, foundStage] = key.split("\0");
        return Number(foundCheckpoint) === checkpoint && foundStage === stage;
      });
      return matching ? median(matching[1].map((row) => Number.parseFloat(row.stage_ms))) : 0;
    });
    report.push(
      `| ${stage} | ${milliseconds.map((value) => value.toFixed(6)).join(" | ")} | ${(milliseconds.at(-1) / Math.max(milliseconds[0], 0.000001)).toFixed(2)}x |`,
    );
  }
}

report.push("");
report.push("## Non-zero save counters (last observed run)");
report.push(`| cadence | counter | ${checkpoints.map((value) => `N=${value}`).join(" | ")} |`);
report.push(`| --- | --- |${" ---: |".repeat(checkpoints.length)}`);
for (const cadence of cadenceLabels) {
  const counterNames = [...new Set([...saveCounters.keys()]
    .filter((key) => key.startsWith(`${cadence}\0`))
    .map((key) => key.split("\0")[2]))].filter((counter) => checkpoints.some((checkpoint) => {
      const rows = saveCounters.get(`${cadence}\0${checkpoint}\0${counter}`) ?? [];
      return rows.some((row) => Number.parseInt(row.value, 10) !== 0);
    }));
  for (const counter of counterNames) {
    const values = checkpoints.map((checkpoint) => {
      const rows = saveCounters.get(`${cadence}\0${checkpoint}\0${counter}`) ?? [];
      return rows.at(-1)?.value ?? "";
    });
    report.push(`| ${cadence} | ${counter} | ${values.join(" | ")} |`);
  }
}

process.stdout.write(`${report.join("\n")}\n`);
