#!/usr/bin/env node

import fs from "node:fs";
import { assertPromotionPlan, finalizePromotion } from "./release-proof-reuse-lib.mjs";

function option(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

const planFile = option("--plan");
const output = option("--output");
const proofFiles = process.argv.flatMap((value, index, args) => value === "--proof" ? [args[index + 1]] : []).filter(Boolean);
if (!planFile || !output) {
  throw new Error("usage: finalize-release-promotion.mjs --plan FILE --proof FILE [--proof FILE...] --output FILE");
}
const plan = assertPromotionPlan(JSON.parse(fs.readFileSync(planFile, "utf8")));
const proofs = proofFiles.flatMap((file) => JSON.parse(fs.readFileSync(file, "utf8")));
const receipt = finalizePromotion({ plan, proofs });
fs.writeFileSync(output, `${JSON.stringify(receipt, null, 2)}\n`);
console.log(`Final promotion receipt OK: ${receipt.sourceCommit} -> ${receipt.targetCommit}.`);
