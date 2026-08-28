#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import { classifyProofOnlyDelta, createPromotionPlan } from "./release-proof-reuse-lib.mjs";

function option(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

const source = option("--source");
const target = option("--target") ?? "HEAD";
const sourceRunId = option("--source-run-id");
const candidateReceiptFile = option("--candidate-receipt");
const sourceEvidenceFile = option("--source-evidence");
const authorizer = option("--authorizer");
const output = option("--output");
if (!source || !sourceRunId || !candidateReceiptFile || !sourceEvidenceFile || !authorizer || !output) {
  throw new Error("usage: create-release-promotion-plan.mjs --source SHA --target SHA --source-run-id ID --candidate-receipt FILE --source-evidence FILE --authorizer LOGIN --output FILE");
}
const root = path.resolve(process.cwd());
const delta = classifyProofOnlyDelta(root, source, target);
const candidateReceipt = JSON.parse(fs.readFileSync(candidateReceiptFile, "utf8"));
const sourceEvidence = JSON.parse(fs.readFileSync(sourceEvidenceFile, "utf8"));
const plan = createPromotionPlan({ delta, sourceRunId, candidateReceipt, sourceEvidence, authorizer });
fs.writeFileSync(output, `${JSON.stringify(plan, null, 2)}\n`);
console.log(`Promotion plan OK: ${plan.sourceCommit} -> ${plan.targetCommit}; ${plan.requiredProofs.length} proof(s).`);
