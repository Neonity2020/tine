#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import { verifyFinalPromotion } from "./release-proof-reuse-lib.mjs";

function option(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

const receiptFile = option("--receipt");
const candidateReceiptFile = option("--candidate-receipt");
const candidateDirectory = option("--candidate");
if (!receiptFile || !candidateReceiptFile || !candidateDirectory) {
  throw new Error("usage: verify-release-promotion.mjs --receipt FILE --candidate-receipt FILE --candidate DIR");
}
const receipt = JSON.parse(fs.readFileSync(receiptFile, "utf8"));
const candidateReceipt = JSON.parse(fs.readFileSync(candidateReceiptFile, "utf8"));
verifyFinalPromotion({
  root: path.resolve(process.cwd()),
  receipt,
  candidateReceipt,
  candidateDirectory: path.resolve(candidateDirectory),
});
console.log(`Verified promoted release candidate for ${receipt.targetCommit}.`);
