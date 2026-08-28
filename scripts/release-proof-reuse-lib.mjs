import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const moduleRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const defaultRegistryPath = path.join(moduleRoot, "scripts", "release-proof-only.json");
const digestPattern = /^[0-9a-f]{64}$/;
const commitPattern = /^[0-9a-f]{40}$/;

function git(root, args, encoding = "utf8") {
  const result = spawnSync("git", args, { cwd: root, encoding, maxBuffer: 64 * 1024 * 1024 });
  if (result.status !== 0) {
    throw result.error || new Error(`git ${args.join(" ")} failed: ${String(result.stderr || "").trim()}`);
  }
  return result.stdout;
}

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

export function loadProofOnlyRegistry(file = defaultRegistryPath) {
  const value = JSON.parse(fs.readFileSync(file, "utf8"));
  if (!value || Array.isArray(value) || value.schemaVersion !== 1 || !value.paths || Array.isArray(value.paths)) {
    throw new Error("proof-only registry must have schemaVersion 1 and a paths object");
  }
  for (const [relative, entry] of Object.entries(value.paths)) {
    if (!/^scripts\/e2e-[A-Za-z0-9-]+\.mjs$/.test(relative)) {
      throw new Error(`proof-only path is outside the narrow real-app scenario class: ${relative}`);
    }
    if (!entry || !Array.isArray(entry.proofs) || entry.proofs.length === 0) {
      throw new Error(`proof-only path has no required proofs: ${relative}`);
    }
    const ids = new Set();
    for (const proof of entry.proofs) {
      if (!proof || typeof proof.id !== "string" || ids.has(proof.id)
        || !["linux", "windows"].includes(proof.platform)
        || typeof proof.suite !== "string" || typeof proof.scenario !== "string"
        || typeof proof.blocking !== "boolean") {
        throw new Error(`invalid proof-only proof entry for ${relative}`);
      }
      const expectedId = `${proof.platform}:${proof.suite}:${proof.scenario}`;
      if (proof.id !== expectedId) throw new Error(`proof id ${proof.id} must be ${expectedId}`);
      ids.add(proof.id);
    }
  }
  return value;
}

export function resolveCommit(root, ref) {
  const commit = git(root, ["rev-parse", `${ref}^{commit}`]).trim().toLowerCase();
  if (!commitPattern.test(commit)) throw new Error(`could not resolve a full commit for ${ref}`);
  return commit;
}

function parseTree(raw) {
  return raw.toString("utf8").split("\0").filter(Boolean).map((record) => {
    const tab = record.indexOf("\t");
    const metadata = record.slice(0, tab).split(" ");
    return { mode: metadata[0], type: metadata[1], object: metadata[2], path: record.slice(tab + 1) };
  });
}

function productExcluded(relative, registry) {
  return relative.startsWith("docs/")
    || relative.startsWith("src-tauri/gen/schemas/")
    || Object.hasOwn(registry.paths, relative);
}

export function productIdentityAtCommit(root, ref, registry = loadProofOnlyRegistry()) {
  const commit = resolveCommit(root, ref);
  const entries = parseTree(git(root, ["ls-tree", "-r", "-z", "--full-tree", commit], null))
    .filter((entry) => !productExcluded(entry.path, registry));
  return {
    schemaVersion: 1,
    commit,
    digest: sha256(canonicalJson(entries)),
    entryCount: entries.length,
  };
}

export function productWorkingTreeChanges(root, registry = loadProofOnlyRegistry()) {
  const excluded = [
    ":(exclude)docs/**",
    ":(exclude)src-tauri/gen/schemas/**",
    ...Object.keys(registry.paths).map((relative) => `:(exclude)${relative}`),
  ];
  return git(root, ["status", "--porcelain=v1", "--untracked-files=all", "--", ".", ...excluded])
    .split(/\r?\n/).filter(Boolean);
}

function parseNameStatus(raw) {
  const values = raw.toString("utf8").split("\0").filter(Boolean);
  const changes = [];
  for (let index = 0; index < values.length;) {
    const status = values[index++];
    const relative = values[index++];
    if (!status || !relative) throw new Error("malformed Git name-status output");
    changes.push({ status, path: relative });
  }
  return changes;
}

export function classifyProofOnlyDelta(root, sourceRef, targetRef, registry = loadProofOnlyRegistry()) {
  const sourceCommit = resolveCommit(root, sourceRef);
  const targetCommit = resolveCommit(root, targetRef);
  if (sourceCommit === targetCommit) {
    const product = productIdentityAtCommit(root, sourceCommit, registry);
    return {
      schemaVersion: 1,
      sourceCommit,
      targetCommit,
      productInputDigest: product.digest,
      productInputEntries: product.entryCount,
      changes: [],
      requiredProofs: [],
      proofIdentity: sha256(canonicalJson({ targetCommit, changes: [], requiredProofs: [] })),
    };
  }
  const ancestry = spawnSync("git", ["merge-base", "--is-ancestor", sourceCommit, targetCommit], { cwd: root });
  if (ancestry.status !== 0) throw new Error(`${sourceCommit} is not an ancestor of ${targetCommit}`);
  const changes = parseNameStatus(git(root, ["diff", "--name-status", "-z", "--no-renames", sourceCommit, targetCommit], null));
  if (changes.length === 0) throw new Error("promotion delta contains no changes");
  for (const change of changes) {
    if (change.status !== "M") throw new Error(`proof-only promotion permits modified files only: ${change.status} ${change.path}`);
    if (!Object.hasOwn(registry.paths, change.path)) throw new Error(`product or unclassified proof input changed: ${change.path}`);
  }
  const sourceProduct = productIdentityAtCommit(root, sourceCommit, registry);
  const targetProduct = productIdentityAtCommit(root, targetCommit, registry);
  if (sourceProduct.digest !== targetProduct.digest) {
    throw new Error(`product identity changed: ${sourceProduct.digest} -> ${targetProduct.digest}`);
  }
  const requiredProofs = [...new Map(changes.flatMap((change) => registry.paths[change.path].proofs)
    .map((proof) => [proof.id, proof])).values()].sort((left, right) => left.id.localeCompare(right.id));
  return {
    schemaVersion: 1,
    sourceCommit,
    targetCommit,
    productInputDigest: sourceProduct.digest,
    productInputEntries: sourceProduct.entryCount,
    changes,
    requiredProofs,
    proofIdentity: sha256(canonicalJson({ targetCommit, changes, requiredProofs })),
  };
}

function assertCandidateReceipt(receipt) {
  if (!receipt || Array.isArray(receipt) || receipt.schemaVersion !== 1
    || receipt.kind !== "tine-release-candidate" || !/^\d+\.\d+\.\d+$/.test(receipt.version || "")
    || !commitPattern.test(receipt.sourceCommit || "") || !digestPattern.test(receipt.productInputDigest || "")
    || !Array.isArray(receipt.assets) || receipt.assets.length === 0) {
    throw new Error("invalid release candidate receipt");
  }
  const names = new Set();
  for (const asset of receipt.assets) {
    if (!asset || typeof asset.name !== "string" || asset.name !== path.basename(asset.name) || names.has(asset.name)
      || !Number.isSafeInteger(asset.size) || asset.size < 0
      || !digestPattern.test(asset.sha256 || "")) throw new Error("invalid release candidate asset receipt");
    names.add(asset.name);
  }
  return receipt;
}

export function createCandidateReceipt({ version, sourceCommit, productInputDigest, assets }) {
  return assertCandidateReceipt({
    schemaVersion: 1,
    kind: "tine-release-candidate",
    version,
    sourceCommit,
    productInputDigest,
    assets: [...assets].sort((left, right) => left.name.localeCompare(right.name)),
  });
}

export function verifyCandidateDirectory(directory, receipt) {
  assertCandidateReceipt(receipt);
  const entries = fs.readdirSync(directory, { withFileTypes: true });
  if (entries.some((entry) => !entry.isFile())) throw new Error("candidate directory must contain regular files only");
  const names = entries.map((entry) => entry.name).sort();
  const expected = receipt.assets.map((asset) => asset.name).sort();
  if (canonicalJson(names) !== canonicalJson(expected)) throw new Error("candidate directory asset inventory differs from its receipt");
  for (const asset of receipt.assets) {
    const bytes = fs.readFileSync(path.join(directory, asset.name));
    if (bytes.length !== asset.size || sha256(bytes) !== asset.sha256) {
      throw new Error(`candidate asset does not match its receipt: ${asset.name}`);
    }
  }
  return receipt;
}

export function createPromotionPlan({ delta, sourceRunId, candidateReceipt, sourceEvidence, authorizer }) {
  assertCandidateReceipt(candidateReceipt);
  const runId = Number(sourceRunId);
  if (!Number.isSafeInteger(runId) || runId <= 0) throw new Error(`invalid source run id: ${sourceRunId}`);
  if (typeof authorizer !== "string" || !/^[A-Za-z0-9-]{1,39}$/.test(authorizer)) {
    throw new Error("promotion authorizer must be a GitHub login");
  }
  if (candidateReceipt.sourceCommit !== delta.sourceCommit) throw new Error("candidate receipt names the wrong source commit");
  if (candidateReceipt.productInputDigest !== delta.productInputDigest) throw new Error("candidate receipt has the wrong product identity");
  if (!sourceEvidence || sourceEvidence.schemaVersion !== 1 || sourceEvidence.kind !== "tine-release-promotion-source"
    || sourceEvidence.runId !== runId || sourceEvidence.sourceCommit !== delta.sourceCommit
    || !Array.isArray(sourceEvidence.artifacts) || sourceEvidence.artifacts.length !== 4) {
    throw new Error("source run evidence does not match the promotion source");
  }
  const requiredArtifacts = [
    "release-candidate",
    "release-candidate-receipt",
    "release-proof-linux-x64",
    "release-proof-windows-x64",
  ];
  const sourceArtifactNames = sourceEvidence.artifacts.map((artifact) => artifact?.name).sort();
  if (canonicalJson(sourceArtifactNames) !== canonicalJson(requiredArtifacts.sort())) {
    throw new Error("source run evidence does not name the required promotion artifacts");
  }
  return {
    schemaVersion: 1,
    kind: "tine-release-promotion-plan",
    version: candidateReceipt.version,
    sourceRunId: runId,
    authorizer,
    sourceCommit: delta.sourceCommit,
    targetCommit: delta.targetCommit,
    productInputDigest: delta.productInputDigest,
    proofIdentity: delta.proofIdentity,
    changes: delta.changes,
    requiredProofs: delta.requiredProofs,
    candidateAssets: candidateReceipt.assets,
    sourceArtifacts: sourceEvidence.artifacts,
  };
}

export function assertPromotionPlan(plan) {
  if (!plan || Array.isArray(plan) || plan.schemaVersion !== 1 || plan.kind !== "tine-release-promotion-plan"
    || !/^\d+\.\d+\.\d+$/.test(plan.version || "") || !Number.isSafeInteger(plan.sourceRunId) || plan.sourceRunId <= 0
    || typeof plan.authorizer !== "string" || !/^[A-Za-z0-9-]{1,39}$/.test(plan.authorizer)
    || !commitPattern.test(plan.sourceCommit || "") || !commitPattern.test(plan.targetCommit || "")
    || !digestPattern.test(plan.productInputDigest || "") || !digestPattern.test(plan.proofIdentity || "")
    || !Array.isArray(plan.changes)
    || !Array.isArray(plan.requiredProofs)
    || !Array.isArray(plan.candidateAssets) || plan.candidateAssets.length === 0
    || !Array.isArray(plan.sourceArtifacts) || plan.sourceArtifacts.length !== 4) {
    throw new Error("invalid release promotion plan");
  }
  const exactCandidate = plan.sourceCommit === plan.targetCommit;
  if (exactCandidate !== (plan.changes.length === 0 && plan.requiredProofs.length === 0)) {
    throw new Error("only an exact candidate may have an empty promotion delta");
  }
  const proofIds = new Set();
  for (const proof of plan.requiredProofs) {
    if (!proof || typeof proof.id !== "string" || proofIds.has(proof.id)
      || proof.id !== `${proof.platform}:${proof.suite}:${proof.scenario}`
      || !["linux", "windows"].includes(proof.platform)
      || typeof proof.suite !== "string" || typeof proof.scenario !== "string"
      || typeof proof.blocking !== "boolean") {
      throw new Error("invalid or duplicate required promotion proof");
    }
    proofIds.add(proof.id);
  }
  assertCandidateReceipt({
    schemaVersion: 1,
    kind: "tine-release-candidate",
    version: plan.version,
    sourceCommit: plan.sourceCommit,
    productInputDigest: plan.productInputDigest,
    assets: plan.candidateAssets,
  });
  const artifactNames = new Set();
  for (const artifact of plan.sourceArtifacts) {
    if (!artifact || !Number.isSafeInteger(artifact.id) || artifact.id <= 0
      || typeof artifact.name !== "string" || artifactNames.has(artifact.name)
      || !Number.isSafeInteger(artifact.size) || artifact.size < 0
      || !(artifact.digest === null || /^sha256:[0-9a-f]{64}$/.test(artifact.digest))) {
      throw new Error("invalid or duplicate source promotion artifact");
    }
    artifactNames.add(artifact.name);
  }
  return plan;
}

export function validatePromotionPlanForCheckout(root, plan, buildReceipt, registry = loadProofOnlyRegistry()) {
  assertPromotionPlan(plan);
  if (resolveCommit(root, "HEAD") !== plan.targetCommit) throw new Error("promotion plan targets a different checkout commit");
  const delta = classifyProofOnlyDelta(root, plan.sourceCommit, plan.targetCommit, registry);
  if (delta.productInputDigest !== plan.productInputDigest || delta.proofIdentity !== plan.proofIdentity) {
    throw new Error("promotion plan no longer matches the source/target delta");
  }
  if (!buildReceipt || buildReceipt.schemaVersion !== 2 || buildReceipt.sourceRevision !== plan.sourceCommit
    || buildReceipt.productInputDigest !== plan.productInputDigest) {
    throw new Error("build receipt does not belong to the promoted product source");
  }
  const dirty = productWorkingTreeChanges(root, registry);
  if (dirty.length) throw new Error(`promoted checkout has dirty product inputs: ${dirty.join(", ")}`);
  return delta;
}

export function createProofResult({ plan, proofId, status, summarySha256, detail = null }) {
  assertPromotionPlan(plan);
  const proof = plan.requiredProofs.find((entry) => entry.id === proofId);
  if (!proof) throw new Error(`proof ${proofId} is not required by the promotion plan`);
  if (!new Set(["passed", "failed"]).has(status) || !digestPattern.test(summarySha256 || "")) {
    throw new Error(`invalid result for promotion proof ${proofId}`);
  }
  return { schemaVersion: 1, kind: "tine-release-promotion-proof", ...proof, status, summarySha256, detail };
}

export function finalizePromotion({ plan, proofs }) {
  assertPromotionPlan(plan);
  if (!Array.isArray(proofs)) throw new Error("promotion proofs must be an array");
  const byId = new Map();
  for (const proof of proofs) {
    const required = plan.requiredProofs.find((entry) => entry.id === proof?.id);
    if (!proof || proof.schemaVersion !== 1 || proof.kind !== "tine-release-promotion-proof" || byId.has(proof.id)
      || !required || !["passed", "failed"].includes(proof.status) || !digestPattern.test(proof.summarySha256 || "")) {
      throw new Error("invalid or duplicate promotion proof result");
    }
    byId.set(proof.id, proof);
  }
  if (byId.size !== plan.requiredProofs.length) throw new Error("promotion proof inventory differs from the plan");
  for (const required of plan.requiredProofs) {
    const result = byId.get(required.id);
    if (!result) throw new Error(`missing promotion proof: ${required.id}`);
    for (const key of ["platform", "suite", "scenario", "blocking"]) {
      if (result[key] !== required[key]) throw new Error(`promotion proof metadata mismatch: ${required.id}`);
    }
    if (required.blocking && result.status !== "passed") throw new Error(`blocking promotion proof failed: ${required.id}`);
  }
  return {
    ...plan,
    kind: "tine-release-promotion-receipt",
    proofs: [...byId.values()].sort((left, right) => left.id.localeCompare(right.id)),
  };
}

export function verifyFinalPromotion({ root, receipt, candidateReceipt, candidateDirectory }) {
  if (!receipt || receipt.kind !== "tine-release-promotion-receipt") throw new Error("final promotion receipt is missing");
  const plan = { ...receipt, kind: "tine-release-promotion-plan" };
  delete plan.proofs;
  assertPromotionPlan(plan);
  const finalized = finalizePromotion({ plan, proofs: receipt.proofs });
  if (canonicalJson(finalized) !== canonicalJson(receipt)) throw new Error("final promotion receipt is not canonical");
  const target = resolveCommit(root, "HEAD");
  if (target !== receipt.targetCommit) throw new Error("final promotion receipt targets a different checkout");
  const delta = classifyProofOnlyDelta(root, receipt.sourceCommit, receipt.targetCommit);
  if (delta.productInputDigest !== receipt.productInputDigest || delta.proofIdentity !== receipt.proofIdentity) {
    throw new Error("final promotion receipt does not match the live Git delta");
  }
  assertCandidateReceipt(candidateReceipt);
  if (candidateReceipt.sourceCommit !== receipt.sourceCommit
    || candidateReceipt.productInputDigest !== receipt.productInputDigest
    || canonicalJson(candidateReceipt.assets) !== canonicalJson(receipt.candidateAssets)) {
    throw new Error("candidate receipt does not match final promotion receipt");
  }
  verifyCandidateDirectory(candidateDirectory, candidateReceipt);
  return receipt;
}

export function hashFile(file) {
  return sha256(fs.readFileSync(file));
}
