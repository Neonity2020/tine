#!/usr/bin/env node

import assert from "node:assert/strict";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { STORAGE_REQUIRED_JOBS, storagePinProblems } from "./storage-pin-lib.mjs";

const root = fs.mkdtempSync(path.join(os.tmpdir(), "tine-storage-pin-"));
const commit = "a".repeat(40);
const run = "https://github.com/martinkoutecky/tine-storage/actions/runs/123";
const manifest = [
  "SQLITE_APPLICATION_ID\tidentity\tsqlite\t1414090309",
  "SQLITE_SCHEMA_VERSION\tidentity\tsqlite\t29",
].join("\n") + "\n";
const receipt = [
  "tine-storage certification receipt",
  "ref=v0.3.0",
  `commit=${commit}`,
  `run=${run}`,
  // Derived, never transcribed: a hand-copied matrix silently rots the moment a
  // shipped target is added to STORAGE_REQUIRED_JOBS, and the failure lands on
  // this fixture rather than on the pin it is supposed to be proving.
  `required_jobs=${STORAGE_REQUIRED_JOBS}`,
  "format_manifest_begin",
  manifest.trimEnd(),
  "format_manifest_end",
  "",
].join("\n");
const digest = (value) => crypto.createHash("sha256").update(value).digest("hex");
const write = (relative, contents) => {
  const target = path.join(root, relative);
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.writeFileSync(target, contents);
};

write("Cargo.toml", '[workspace]\nmembers = ["crates/tine-core"]\nresolver = "2"\n');
write(
  "crates/tine-core/Cargo.toml",
  `[dependencies]\ntine-storage = { git = "https://github.com/martinkoutecky/tine-storage", tag = "v0.3.0" }\n\n` +
    `[dev-dependencies]\ntine-storage = { git = "https://github.com/martinkoutecky/tine-storage", tag = "v0.3.0", features = ["test-support"] }\n`,
);
write(
  "Cargo.lock",
  `version = 4\n\n[[package]]\nname = "tine-storage"\nversion = "0.3.0"\nsource = "git+https://github.com/martinkoutecky/tine-storage?tag=v0.3.0#${commit}"\n`,
);
write("docs/dependency-receipts/tine-storage-v0.3.0.txt", receipt);
const metadata = {
  schema: 1,
  package: "tine-storage",
  repository: "https://github.com/martinkoutecky/tine-storage",
  version: "0.3.0",
  tag: "v0.3.0",
  commit,
  certificationRun: run,
  release: "https://github.com/martinkoutecky/tine-storage/releases/tag/v0.3.0",
  receiptFile: "tine-storage-v0.3.0.txt",
  receiptUrl: "https://github.com/martinkoutecky/tine-storage/releases/download/v0.3.0/certification-receipt.txt",
  receiptSha256: digest(receipt),
  formatManifestSha256: digest(manifest),
  attestation: "https://github.com/martinkoutecky/tine-storage/attestations/456",
};
write("docs/dependency-receipts/tine-storage.json", `${JSON.stringify(metadata, null, 2)}\n`);

assert.deepEqual(storagePinProblems(root), []);

const coreManifest = fs.readFileSync(path.join(root, "crates/tine-core/Cargo.toml"), "utf8");
write("crates/tine-core/Cargo.toml", coreManifest.replace('tag = "v0.3.0"', 'path = "../tine-storage"'));
assert.ok(storagePinProblems(root).some((problem) => problem.includes("forbidden path override")));
write("crates/tine-core/Cargo.toml", coreManifest);

const lock = fs.readFileSync(path.join(root, "Cargo.lock"), "utf8");
write("Cargo.lock", lock.replace(commit, "b".repeat(40)));
assert.ok(storagePinProblems(root).some((problem) => problem.includes("receipt commit")));
write("Cargo.lock", lock);

write("Cargo.lock", lock.replace("?tag=v0.3.0", "?tag=v0.1.1"));
assert.ok(storagePinProblems(root).some((problem) => problem.includes("lock tag")));
write("Cargo.lock", lock);

write(
  "crates/tine-core/Cargo.toml",
  coreManifest.replace(
    'tag = "v0.3.0" }',
    'tag = "v0.3.0", default-features = false }',
  ),
);
assert.ok(storagePinProblems(root).some((problem) => problem.includes("certified feature shape")));
write("crates/tine-core/Cargo.toml", coreManifest);

write("docs/dependency-receipts/tine-storage-v0.3.0.txt", `${receipt}tampered\n`);
assert.ok(storagePinProblems(root).some((problem) => problem.includes("receipt SHA-256")));

const incompleteReceipt = receipt.replace(
  `required_jobs=${STORAGE_REQUIRED_JOBS}`,
  `required_jobs=${STORAGE_REQUIRED_JOBS.split(",")[0]}`,
);
write("docs/dependency-receipts/tine-storage-v0.3.0.txt", incompleteReceipt);
const incompleteMetadata = JSON.parse(fs.readFileSync(path.join(root, "docs/dependency-receipts/tine-storage.json"), "utf8"));
incompleteMetadata.receiptSha256 = digest(incompleteReceipt);
write("docs/dependency-receipts/tine-storage.json", `${JSON.stringify(incompleteMetadata, null, 2)}\n`);
assert.ok(storagePinProblems(root).some((problem) => problem.includes("complete required storage matrix")));

const foreignIdReceipt = receipt.replace(
  "SQLITE_APPLICATION_ID\tidentity\tsqlite\t1414090309\n",
  "SQLITE_APPLICATION_ID\tidentity\tsqlite\t1414090310\n",
);
write("docs/dependency-receipts/tine-storage-v0.3.0.txt", foreignIdReceipt);
const foreignIdMetadata = JSON.parse(fs.readFileSync(path.join(root, "docs/dependency-receipts/tine-storage.json"), "utf8"));
const foreignIdManifest = foreignIdReceipt.match(/^format_manifest_begin\n([\s\S]+?)^format_manifest_end$/m)?.[1];
foreignIdMetadata.receiptSha256 = digest(foreignIdReceipt);
foreignIdMetadata.formatManifestSha256 = digest(foreignIdManifest);
write("docs/dependency-receipts/tine-storage.json", `${JSON.stringify(foreignIdMetadata, null, 2)}\n`);
assert.ok(storagePinProblems(root).some((problem) => problem.includes("SQLITE_APPLICATION_ID=1414090310")));

const schemalessReceipt = receipt.replace("SQLITE_SCHEMA_VERSION\tidentity\tsqlite\t29\n", "");
write("docs/dependency-receipts/tine-storage-v0.3.0.txt", schemalessReceipt);
const schemalessMetadata = JSON.parse(fs.readFileSync(path.join(root, "docs/dependency-receipts/tine-storage.json"), "utf8"));
const schemalessManifest = schemalessReceipt.match(/^format_manifest_begin\n([\s\S]+?)^format_manifest_end$/m)?.[1];
schemalessMetadata.receiptSha256 = digest(schemalessReceipt);
schemalessMetadata.formatManifestSha256 = digest(schemalessManifest);
write("docs/dependency-receipts/tine-storage.json", `${JSON.stringify(schemalessMetadata, null, 2)}\n`);
assert.ok(storagePinProblems(root).some((problem) => problem.includes("omits SQLITE_SCHEMA_VERSION")));

fs.rmSync(root, { recursive: true, force: true });
console.log("tine-storage pin contract fixtures OK");
