import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

export const STORAGE_REPOSITORY = "https://github.com/martinkoutecky/tine-storage";
export const STORAGE_PIN_METADATA = "docs/dependency-receipts/tine-storage.json";
export const STORAGE_REQUIRED_JOBS = "linux-complete,windows-complete,android-compile,api-semver";

function sha256(bytes) {
  return crypto.createHash("sha256").update(bytes).digest("hex");
}

function field(text, name) {
  return text.match(new RegExp(`^${name} = "([^"]+)"$`, "m"))?.[1];
}

function dependencySpecs(manifest) {
  return [...manifest.matchAll(/^tine-storage\s*=\s*\{([^}]*)\}\s*$/gm)].map((match) => match[1]);
}

function specField(spec, name) {
  return spec.match(new RegExp(`\\b${name}\\s*=\\s*"([^"]+)"`))?.[1];
}

function lockPackages(lock) {
  return lock
    .split(/\n\[\[package\]\]\n/)
    .slice(1)
    .filter((block) => field(block, "name") === "tine-storage");
}

function receiptField(receipt, name) {
  return receipt.match(new RegExp(`^${name}=(.+)$`, "m"))?.[1];
}

export function storagePinProblems(root) {
  const problems = [];
  const read = (relative) => fs.readFileSync(path.join(root, relative), "utf8");
  const rootManifest = read("Cargo.toml");
  const coreManifest = read("crates/tine-core/Cargo.toml");
  const lock = read("Cargo.lock");

  if (/"crates\/tine-storage"/.test(rootManifest)) {
    problems.push("Cargo workspace still contains the in-tree tine-storage source");
  }
  if (fs.existsSync(path.join(root, "crates", "tine-storage"))) {
    problems.push("crates/tine-storage still exists; Tine must consume only the certified external source");
  }

  const specs = dependencySpecs(coreManifest);
  if (specs.length !== 2) {
    problems.push(`expected normal and dev tine-storage dependencies; found ${specs.length}`);
  }
  const tags = new Set();
  for (const [index, spec] of specs.entries()) {
    const role = index === 0 ? "normal" : "dev";
    if (specField(spec, "git") !== STORAGE_REPOSITORY) {
      problems.push(`${role} tine-storage dependency is not the canonical Git repository`);
    }
    const tag = specField(spec, "tag");
    if (!/^v\d+\.\d+\.\d+$/.test(tag ?? "")) {
      problems.push(`${role} tine-storage dependency has no exact release tag`);
    } else {
      tags.add(tag);
    }
    for (const forbidden of ["path", "branch", "rev"]) {
      if (new RegExp(`\\b${forbidden}\\s*=`).test(spec)) {
        problems.push(`${role} tine-storage dependency uses forbidden ${forbidden} override`);
      }
    }
  }
  if (tags.size !== 1) problems.push("normal and dev tine-storage dependencies do not use one tag");
  if (specs[0] && /test-support/.test(specs[0])) {
    problems.push("normal tine-storage dependency enables test-support");
  }
  if (specs[0]) {
    const fields = [...specs[0].matchAll(/\b([A-Za-z][\w-]*)\s*=/g)].map((match) => match[1]);
    const unexpected = fields.filter((name) => !["git", "tag"].includes(name));
    if (unexpected.length) {
      problems.push(`normal tine-storage dependency changes the certified feature shape (${unexpected.join(", ")})`);
    }
  }
  if (specs[1] && !/features\s*=\s*\[\s*"test-support"\s*\]/.test(specs[1])) {
    problems.push("dev tine-storage dependency does not explicitly enable test-support");
  }

  for (const section of rootManifest.split(/^\[/m).filter((part) => part.startsWith("patch."))) {
    if (/tine-storage/.test(section)) problems.push("Cargo.toml contains a tine-storage patch override");
  }

  const packages = lockPackages(lock);
  if (packages.length !== 1) problems.push(`Cargo.lock contains ${packages.length} tine-storage packages`);
  const locked = packages[0] ?? "";
  const lockedVersion = field(locked, "version");
  const lockedSource = field(locked, "source") ?? "";
  const lockedMatch = lockedSource.match(
    /^git\+https:\/\/github\.com\/martinkoutecky\/tine-storage\?tag=(v\d+\.\d+\.\d+)#([0-9a-f]{40})$/,
  );
  if (!lockedMatch) problems.push("Cargo.lock does not resolve tine-storage from one exact tagged commit");

  let metadata;
  try {
    metadata = JSON.parse(read(STORAGE_PIN_METADATA));
  } catch (error) {
    problems.push(`could not read ${STORAGE_PIN_METADATA}: ${error.message}`);
    return problems;
  }
  const receiptRelative = path.posix.join("docs/dependency-receipts", metadata.receiptFile ?? "");
  let receiptBytes;
  try {
    receiptBytes = fs.readFileSync(path.join(root, receiptRelative));
  } catch (error) {
    problems.push(`could not read pin receipt ${receiptRelative}: ${error.message}`);
    return problems;
  }
  const receipt = receiptBytes.toString("utf8");
  const tag = [...tags][0];
  const commit = lockedMatch?.[2];
  const version = tag?.slice(1);
  const manifest = receipt.match(/^format_manifest_begin\n([\s\S]+?)^format_manifest_end$/m)?.[1];

  const expectedMetadata = {
    schema: 1,
    package: "tine-storage",
    repository: STORAGE_REPOSITORY,
    version,
    tag,
    commit,
  };
  if (lockedMatch?.[1] !== tag) problems.push("Cargo.lock tag does not match the manifest dependency tag");
  for (const [name, expected] of Object.entries(expectedMetadata)) {
    if (metadata[name] !== expected) problems.push(`pin metadata ${name} is ${metadata[name] ?? "missing"}; expected ${expected}`);
  }
  if (lockedVersion !== version) problems.push(`Cargo.lock version ${lockedVersion} does not match tag ${tag}`);
  if (receiptField(receipt, "ref") !== tag) problems.push("certification receipt ref does not match the dependency tag");
  if (receiptField(receipt, "commit") !== commit) problems.push("certification receipt commit does not match Cargo.lock");
  if (receiptField(receipt, "run") !== metadata.certificationRun) {
    problems.push("certification receipt run does not match pin metadata");
  }
  if (receiptField(receipt, "required_jobs") !== STORAGE_REQUIRED_JOBS) {
    problems.push("certification receipt does not name the complete required storage matrix");
  }
  if (sha256(receiptBytes) !== metadata.receiptSha256) problems.push("certification receipt SHA-256 does not match pin metadata");
  if (!manifest) {
    problems.push("certification receipt has no persistent-format manifest");
  } else {
    if (sha256(manifest) !== metadata.formatManifestSha256) problems.push("persistent-format manifest SHA-256 does not match pin metadata");
    for (const required of [
      "OPLOG_PROTOCOL_VERSION",
      "OBJECT_ENVELOPE_SCHEMA_VERSION",
      "MANIFEST_ENCODING_VERSION",
      "LOCAL_JOURNAL_FRAME_SCHEMA_VERSION",
      "SCRATCH_SCHEMA_VERSION",
      "SQLITE_SCHEMA_VERSION",
    ]) {
      if (!new RegExp(`^${required}\\t`, "m").test(manifest)) problems.push(`persistent-format receipt omits ${required}`);
    }
  }
  if (metadata.release !== `${STORAGE_REPOSITORY}/releases/tag/${tag}`) problems.push("pin metadata release URL is not canonical");
  if (metadata.receiptUrl !== `${STORAGE_REPOSITORY}/releases/download/${tag}/certification-receipt.txt`) {
    problems.push("pin metadata receipt URL is not canonical");
  }
  if (!/^https:\/\/github\.com\/martinkoutecky\/tine-storage\/actions\/runs\/\d+$/.test(metadata.certificationRun ?? "")) {
    problems.push("pin metadata certification run URL is not canonical");
  }
  if (!/^https:\/\/github\.com\/martinkoutecky\/tine-storage\/attestations\/\d+$/.test(metadata.attestation ?? "")) {
    problems.push("pin metadata attestation URL is not canonical");
  }

  return problems;
}
