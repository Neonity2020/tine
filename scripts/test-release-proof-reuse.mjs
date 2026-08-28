#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  classifyProofOnlyDelta,
  createCandidateReceipt,
  createPromotionPlan,
  createProofResult,
  finalizePromotion,
  hashFile,
  productIdentityAtCommit,
  validatePromotionPlanForCheckout,
  verifyCandidateDirectory,
  verifyFinalPromotion,
} from "./release-proof-reuse-lib.mjs";

function git(root, ...args) {
  const result = spawnSync("git", args, { cwd: root, encoding: "utf8" });
  if (result.status !== 0) throw new Error(result.stderr || `git ${args.join(" ")} failed`);
  return result.stdout.trim();
}

function commit(root, message) {
  git(root, "add", ".");
  git(root, "-c", "user.name=Tine test", "-c", "user.email=tine-test@example.invalid", "commit", "-m", message);
  return git(root, "rev-parse", "HEAD");
}

function expectFailure(pattern, callback) {
  assert.throws(callback, pattern);
}

const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "tine-release-proof-reuse-"));
try {
  const root = path.join(temporary, "repo");
  fs.mkdirSync(path.join(root, "scripts"), { recursive: true });
  fs.mkdirSync(path.join(root, ".github/workflows"), { recursive: true });
  fs.mkdirSync(path.join(root, "src/generated"), { recursive: true });
  fs.mkdirSync(path.join(root, "src-tauri"), { recursive: true });
  fs.writeFileSync(path.join(root, "src/product.ts"), "export const product = 1;\n");
  fs.writeFileSync(path.join(root, "src/generated/runtime.js"), "runtime-v1\n");
  fs.writeFileSync(path.join(root, "package.json"), "{\"version\":\"1.2.3\"}\n");
  fs.writeFileSync(path.join(root, "package-lock.json"), "{\"lockfileVersion\":3}\n");
  fs.writeFileSync(path.join(root, "src-tauri/tauri.conf.json"), "{\"productName\":\"Tine\"}\n");
  fs.writeFileSync(path.join(root, ".github/workflows/release.yml"), "name: release\n");
  fs.writeFileSync(path.join(root, "scripts/build-product.mjs"), "// packaging input v1\n");
  fs.writeFileSync(path.join(root, "scripts/e2e-page-properties.mjs"), "// proof v1\n");
  git(root, "init");
  const sourceCommit = commit(root, "source candidate");

  const registry = {
    schemaVersion: 1,
    paths: {
      "scripts/e2e-page-properties.mjs": {
        proofs: [
          { id: "linux:linux-release:page-properties", platform: "linux", suite: "linux-release", scenario: "page-properties", blocking: true },
          { id: "windows:windows-smoke:page-properties", platform: "windows", suite: "windows-smoke", scenario: "page-properties", blocking: false },
        ],
      },
    },
  };
  const sourceIdentity = productIdentityAtCommit(root, sourceCommit, registry);
  const exactDelta = classifyProofOnlyDelta(root, sourceCommit, sourceCommit, registry);
  assert.deepEqual(exactDelta.changes, []);
  assert.deepEqual(exactDelta.requiredProofs, []);
  fs.writeFileSync(path.join(root, "scripts/e2e-page-properties.mjs"), "// proof v2\n");
  const targetCommit = commit(root, "repair proof only");
  const delta = classifyProofOnlyDelta(root, sourceCommit, targetCommit, registry);
  assert.equal(delta.productInputDigest, sourceIdentity.digest);
  assert.deepEqual(delta.changes, [{ status: "M", path: "scripts/e2e-page-properties.mjs" }]);
  assert.equal(delta.requiredProofs.length, 2);

  const candidate = path.join(temporary, "candidate");
  fs.mkdirSync(candidate);
  fs.writeFileSync(path.join(candidate, "Tine_1.2.3_amd64.AppImage"), "appimage\n");
  fs.writeFileSync(path.join(candidate, "latest.json"), "{}\n");
  const assets = fs.readdirSync(candidate).map((name) => ({
    name,
    size: fs.statSync(path.join(candidate, name)).size,
    sha256: hashFile(path.join(candidate, name)),
  }));
  const candidateReceipt = createCandidateReceipt({
    version: "1.2.3",
    sourceCommit,
    productInputDigest: delta.productInputDigest,
    assets,
  });
  verifyCandidateDirectory(candidate, candidateReceipt);
  const sourceEvidence = {
    schemaVersion: 1,
    kind: "tine-release-promotion-source",
    runId: 123,
    sourceCommit,
    artifacts: ["release-candidate", "release-candidate-receipt", "release-proof-linux-x64", "release-proof-windows-x64"]
      .map((name, index) => ({ id: index + 1, name, size: 1, digest: null })),
  };
  const exactPlan = createPromotionPlan({
    delta: exactDelta,
    sourceRunId: 123,
    candidateReceipt,
    sourceEvidence,
    authorizer: "tine-test",
  });
  assert.deepEqual(finalizePromotion({ plan: exactPlan, proofs: [] }).proofs, []);
  const plan = createPromotionPlan({ delta, sourceRunId: 123, candidateReceipt, sourceEvidence, authorizer: "tine-test" });
  const sourceBuildReceipt = {
    schemaVersion: 2,
    sourceRevision: sourceCommit,
    productInputDigest: delta.productInputDigest,
  };
  assert.equal(validatePromotionPlanForCheckout(root, plan, sourceBuildReceipt, registry).proofIdentity, delta.proofIdentity);
  expectFailure(/does not belong to the promoted product source/, () => validatePromotionPlanForCheckout(
    root,
    plan,
    { ...sourceBuildReceipt, productInputDigest: "d".repeat(64) },
    registry,
  ));
  const linux = createProofResult({ plan, proofId: delta.requiredProofs[0].id, status: "passed", summarySha256: "1".repeat(64) });
  const windows = createProofResult({ plan, proofId: delta.requiredProofs[1].id, status: "failed", summarySha256: "2".repeat(64), detail: "advisory failure" });
  const receipt = finalizePromotion({ plan, proofs: [linux, windows] });
  verifyFinalPromotion({ root, receipt, candidateReceipt, candidateDirectory: candidate });

  expectFailure(/proof inventory differs/, () => finalizePromotion({ plan, proofs: [linux] }));
  const failedLinux = createProofResult({ plan, proofId: linux.id, status: "failed", summarySha256: "3".repeat(64) });
  expectFailure(/blocking promotion proof failed/, () => finalizePromotion({ plan, proofs: [failedLinux, windows] }));
  const wrongSourceReceipt = { ...candidateReceipt, sourceCommit: "f".repeat(40) };
  expectFailure(/wrong source commit/, () => createPromotionPlan({ delta, sourceRunId: 123, candidateReceipt: wrongSourceReceipt, sourceEvidence, authorizer: "tine-test" }));
  const wrongDigestReceipt = { ...candidateReceipt, productInputDigest: "e".repeat(64) };
  expectFailure(/wrong product identity/, () => createPromotionPlan({ delta, sourceRunId: 123, candidateReceipt: wrongDigestReceipt, sourceEvidence, authorizer: "tine-test" }));
  fs.appendFileSync(path.join(candidate, assets[0].name), "tamper\n");
  expectFailure(/does not match its receipt/, () => verifyCandidateDirectory(candidate, candidateReceipt));
  fs.writeFileSync(path.join(candidate, assets[0].name), assets[0].name === "latest.json" ? "{}\n" : "appimage\n");

  const proofBranch = git(root, "rev-parse", "HEAD");
  for (const [file, content, pattern] of [
    ["src/product.ts", "export const product = 2;\n", /product or unclassified proof input changed/],
    ["package.json", "{\"version\":\"1.2.4\"}\n", /product or unclassified proof input changed/],
    ["package-lock.json", "{\"lockfileVersion\":2}\n", /product or unclassified proof input changed/],
    [".github/workflows/release.yml", "name: changed-release\n", /product or unclassified proof input changed/],
    ["src-tauri/tauri.conf.json", "{\"productName\":\"Changed Tine\"}\n", /product or unclassified proof input changed/],
    ["scripts/build-product.mjs", "// packaging input v2\n", /product or unclassified proof input changed/],
    ["src/generated/runtime.js", "runtime-v2\n", /product or unclassified proof input changed/],
  ]) {
    git(root, "checkout", "-B", `negative-${path.basename(file).replaceAll(".", "-")}`, proofBranch);
    fs.writeFileSync(path.join(root, file), content);
    const changed = commit(root, `change ${file}`);
    expectFailure(pattern, () => classifyProofOnlyDelta(root, sourceCommit, changed, registry));
  }

  git(root, "checkout", "-B", "negative-add", proofBranch);
  registry.paths["scripts/e2e-added.mjs"] = registry.paths["scripts/e2e-page-properties.mjs"];
  fs.writeFileSync(path.join(root, "scripts/e2e-added.mjs"), "// added\n");
  const added = commit(root, "add proof");
  expectFailure(/modified files only/, () => classifyProofOnlyDelta(root, proofBranch, added, registry));

  git(root, "checkout", "-B", "unrelated", sourceCommit);
  fs.writeFileSync(path.join(root, "scripts/e2e-page-properties.mjs"), "// unrelated proof\n");
  const unrelated = commit(root, "unrelated proof");
  expectFailure(/is not an ancestor/, () => classifyProofOnlyDelta(root, targetCommit, unrelated, registry));
} finally {
  fs.rmSync(temporary, { recursive: true, force: true });
}

console.log("Release proof-reuse fixtures passed (safe promotion plus fail-closed negative cases).");
