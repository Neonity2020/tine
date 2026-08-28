#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { assertPromotionPlan, createProofResult, hashFile } from "./release-proof-reuse-lib.mjs";

function option(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

const planFile = path.resolve(option("--plan") || "");
const platform = option("--platform");
const output = path.resolve(option("--output") || "");
if (!fs.existsSync(planFile) || !["linux", "windows"].includes(platform) || !option("--output")) {
  throw new Error("usage: run-release-promotion-proofs.mjs --plan FILE --platform linux|windows --output FILE");
}
const plan = assertPromotionPlan(JSON.parse(fs.readFileSync(planFile, "utf8")));
const selected = plan.requiredProofs.filter((proof) => proof.platform === platform);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const results = [];
let blockingFailure = false;
for (const proof of selected) {
  const artifactDir = path.join(root, "test-results", "release-promotion", proof.id.replaceAll(":", "-"));
  const result = spawnSync(process.execPath, [path.join(root, "scripts", "run-e2e.mjs"), proof.suite, `--scenario=${proof.scenario}`], {
    cwd: root,
    stdio: "inherit",
    env: {
      ...process.env,
      TINE_E2E_MODE: "release",
      TINE_E2E_PROMOTION_PLAN: planFile,
      E2E_ARTIFACT_DIR: artifactDir,
    },
  });
  const summaryFile = path.join(artifactDir, "summary.json");
  if (!fs.existsSync(summaryFile)) throw new Error(`promotion proof ${proof.id} produced no summary`);
  const summary = JSON.parse(fs.readFileSync(summaryFile, "utf8"));
  const scenario = (summary.results ?? []).find((entry) => entry.id === proof.scenario);
  const promotion = summary.buildProvenance?.promotion;
  if (!scenario || promotion?.sourceCommit !== plan.sourceCommit || promotion?.targetCommit !== plan.targetCommit
    || promotion?.productInputDigest !== plan.productInputDigest || promotion?.proofIdentity !== plan.proofIdentity) {
    throw new Error(`promotion proof ${proof.id} did not attest the planned binary/source pair`);
  }
  const status = result.status === 0 && scenario.status === "passed" ? "passed" : "failed";
  results.push(createProofResult({
    plan,
    proofId: proof.id,
    status,
    summarySha256: hashFile(summaryFile),
    detail: status === "passed" ? null : `exit=${result.status}; scenario=${scenario.status}`,
  }));
  if (proof.blocking && status !== "passed") blockingFailure = true;
}
fs.mkdirSync(path.dirname(output), { recursive: true });
fs.writeFileSync(output, `${JSON.stringify(results, null, 2)}\n`);
console.log(`Recorded ${results.length} ${platform} promotion proof(s).`);
if (blockingFailure) process.exit(1);
