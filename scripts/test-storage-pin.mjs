#!/usr/bin/env node

import assert from "node:assert/strict";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { storagePinProblems } from "./storage-pin-lib.mjs";

const root = fs.mkdtempSync(path.join(os.tmpdir(), "tine-storage-pin-"));
const commit = "a".repeat(40);
const run = "https://github.com/martinkoutecky/tine-storage/actions/runs/123";
const manifest = [
  "OPLOG_PROTOCOL_VERSION\tidentity\toplog\t2",
  "OBJECT_ENVELOPE_SCHEMA_VERSION\tidentity\tenvelope\t2",
  "MANIFEST_ENCODING_VERSION\tidentity\tmanifest\t4",
  "LOCAL_JOURNAL_FRAME_SCHEMA_VERSION\tidentity\tjournal\t1",
  "SCRATCH_SCHEMA_VERSION\tidentity\tscratch\t13",
  "SQLITE_SCHEMA_VERSION\tidentity\tsqlite\t15",
].join("\n") + "\n";
const receipt = [
  "tine-storage certification receipt",
  "ref=v0.1.0",
  `commit=${commit}`,
  `run=${run}`,
  "required_jobs=linux-complete,windows-complete,android-compile,api-semver",
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
  `[dependencies]\ntine-storage = { git = "https://github.com/martinkoutecky/tine-storage", tag = "v0.1.0" }\n\n` +
    `[dev-dependencies]\ntine-storage = { git = "https://github.com/martinkoutecky/tine-storage", tag = "v0.1.0", features = ["test-support"] }\n`,
);
write(
  "Cargo.lock",
  `version = 4\n\n[[package]]\nname = "tine-storage"\nversion = "0.1.0"\nsource = "git+https://github.com/martinkoutecky/tine-storage?tag=v0.1.0#${commit}"\n`,
);
write("docs/dependency-receipts/tine-storage-v0.1.0.txt", receipt);
const metadata = {
  schema: 1,
  package: "tine-storage",
  repository: "https://github.com/martinkoutecky/tine-storage",
  version: "0.1.0",
  tag: "v0.1.0",
  commit,
  certificationRun: run,
  release: "https://github.com/martinkoutecky/tine-storage/releases/tag/v0.1.0",
  receiptFile: "tine-storage-v0.1.0.txt",
  receiptUrl: "https://github.com/martinkoutecky/tine-storage/releases/download/v0.1.0/certification-receipt.txt",
  receiptSha256: digest(receipt),
  formatManifestSha256: digest(manifest),
  attestation: "https://github.com/martinkoutecky/tine-storage/attestations/456",
};
write("docs/dependency-receipts/tine-storage.json", `${JSON.stringify(metadata, null, 2)}\n`);

assert.deepEqual(storagePinProblems(root), []);

const coreManifest = fs.readFileSync(path.join(root, "crates/tine-core/Cargo.toml"), "utf8");
write("crates/tine-core/Cargo.toml", coreManifest.replace('tag = "v0.1.0"', 'path = "../tine-storage"'));
assert.ok(storagePinProblems(root).some((problem) => problem.includes("forbidden path override")));
write("crates/tine-core/Cargo.toml", coreManifest);

const lock = fs.readFileSync(path.join(root, "Cargo.lock"), "utf8");
write("Cargo.lock", lock.replace(commit, "b".repeat(40)));
assert.ok(storagePinProblems(root).some((problem) => problem.includes("receipt commit")));
write("Cargo.lock", lock);

write("Cargo.lock", lock.replace("?tag=v0.1.0", "?tag=v0.1.1"));
assert.ok(storagePinProblems(root).some((problem) => problem.includes("lock tag")));
write("Cargo.lock", lock);

write(
  "crates/tine-core/Cargo.toml",
  coreManifest.replace(
    'tag = "v0.1.0" }',
    'tag = "v0.1.0", default-features = false }',
  ),
);
assert.ok(storagePinProblems(root).some((problem) => problem.includes("certified feature shape")));
write("crates/tine-core/Cargo.toml", coreManifest);

write("docs/dependency-receipts/tine-storage-v0.1.0.txt", `${receipt}tampered\n`);
assert.ok(storagePinProblems(root).some((problem) => problem.includes("receipt SHA-256")));

const incompleteReceipt = receipt.replace(
  "required_jobs=linux-complete,windows-complete,android-compile,api-semver",
  "required_jobs=linux-complete",
);
write("docs/dependency-receipts/tine-storage-v0.1.0.txt", incompleteReceipt);
const incompleteMetadata = JSON.parse(fs.readFileSync(path.join(root, "docs/dependency-receipts/tine-storage.json"), "utf8"));
incompleteMetadata.receiptSha256 = digest(incompleteReceipt);
write("docs/dependency-receipts/tine-storage.json", `${JSON.stringify(incompleteMetadata, null, 2)}\n`);
assert.ok(storagePinProblems(root).some((problem) => problem.includes("complete required storage matrix")));

fs.rmSync(root, { recursive: true, force: true });
console.log("tine-storage pin contract fixtures OK");
