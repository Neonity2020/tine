#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { validateRedditReleaseEvidence } from "./reddit-release-evidence-lib.mjs";

const SCHEMA_VERSION = 1;
const PATCH_NAME = "reddit-evidence.patch";
const FILES_DIRECTORY = "files";
const textDecoder = new TextDecoder("utf-8", { fatal: true });

function fail(message) {
  throw new Error(message);
}

function sha256(bytes) {
  return crypto.createHash("sha256").update(bytes).digest("hex");
}

function utf8(bytes, context) {
  try {
    return textDecoder.decode(bytes);
  } catch {
    fail(`${context} is not valid UTF-8`);
  }
}

function regularFile(stat) {
  return stat.isFile() && !stat.isSymbolicLink();
}

function lstatRegular(file, description) {
  let stat;
  try {
    stat = fs.lstatSync(file);
  } catch {
    fail(`${description} is missing: ${file}`);
  }
  if (!regularFile(stat)) fail(`${description} must be a regular non-symlink file: ${file}`);
  return stat;
}

function requiredString(value, name) {
  if (typeof value !== "string" || value.length === 0) fail(`${name} is required`);
  return value;
}

function validSha(value, name) {
  requiredString(value, name);
  if (!/^[0-9a-f]{40}$/.test(value)) fail(`${name} must be a lowercase 40-character SHA`);
  return value;
}

export function validateVersion(requestedVersion, packageVersion) {
  if (!/^\d+\.\d+\.0$/.test(requestedVersion ?? "")) {
    fail("reddit_version must be an exact minor version such as 0.6.0");
  }
  if (requestedVersion !== packageVersion) {
    fail(`reddit_version ${requestedVersion} does not match package.json version ${packageVersion}`);
  }
  return requestedVersion;
}

export function validateArtifactPath(value, version) {
  if (typeof value !== "string" || value.length === 0 || value.includes("\0")) fail("artifact path is empty or malformed");
  if (path.isAbsolute(value) || /^[A-Za-z]:[\\/]/.test(value) || value.startsWith("\\\\")) {
    fail(`artifact path is absolute: ${value}`);
  }
  if (value.includes("\\") || value.split("/").some((part) => part === "" || part === "." || part === "..")) {
    fail(`artifact path traverses or is malformed: ${value}`);
  }
  const evidence = `docs/releases/v${version}-reddit.json`;
  if (value === evidence) return value;
  if (value.startsWith("website/blog/")) return value;
  fail(`artifact path is outside the Reddit evidence allowlist: ${value}`);
}

function repositoryPath(root, relativePath) {
  const absolute = path.resolve(root, relativePath);
  if (!absolute.startsWith(`${root}${path.sep}`)) fail(`artifact path escaped repository: ${relativePath}`);
  return absolute;
}

function git(root, args, { env = process.env, indexFile, allowFailure = false } = {}) {
  const gitEnv = { ...env };
  delete gitEnv.GIT_INDEX_FILE;
  if (indexFile) gitEnv.GIT_INDEX_FILE = indexFile;
  const result = spawnSync("git", args, {
    cwd: root,
    env: gitEnv,
    encoding: "buffer",
    maxBuffer: 64 * 1024 * 1024,
  });
  if (result.error && result.status !== 0) fail(`git ${args[0]} could not start: ${result.error.message}`);
  if (!allowFailure && result.status !== 0) {
    fail(`git ${args.join(" ")} failed: ${utf8(result.stderr ?? Buffer.alloc(0), "git stderr").trim()}`);
  }
  return result;
}

function cleanCheckout(root, env) {
  const status = git(root, ["status", "--porcelain=v1", "-z", "--untracked-files=all"], { env });
  if ((status.stdout ?? Buffer.alloc(0)).length !== 0) fail("checkout must be clean before Reddit evidence collection or verification");
}

function currentHead(root, env) {
  return utf8(git(root, ["rev-parse", "HEAD"], { env }).stdout, "git HEAD").trim();
}

function parseStatus(output) {
  const fields = utf8(output, "git status").split("\0");
  if (fields.pop() !== "") fail("git status is not NUL terminated");
  const changes = [];
  const seen = new Set();
  for (const field of fields) {
    if (field.length < 4 || field[2] !== " ") fail(`malformed git status record: ${field}`);
    const status = field.slice(0, 2);
    const relativePath = field.slice(3);
    if (status === "R " || status === " R" || status === "C " || status === " C") {
      fail(`renamed or copied path is not allowed: ${relativePath}`);
    }
    if (status !== " M" && status !== "??") fail(`unsupported git status ${status} for ${relativePath}`);
    if (seen.has(relativePath)) fail(`duplicate git status path: ${relativePath}`);
    seen.add(relativePath);
    changes.push({ status: status === "??" ? "A" : "M", path: relativePath });
  }
  return changes;
}

function parseNameStatus(output) {
  const fields = utf8(output, "git diff name-status").split("\0");
  if (fields.pop() !== "") fail("git diff name-status is not NUL terminated");
  if (fields.length % 2 !== 0) fail("malformed git diff name-status record count");
  const changes = [];
  for (let index = 0; index < fields.length; index += 2) {
    const status = fields[index];
    const relativePath = fields[index + 1];
    if (status !== "A" && status !== "M") fail(`unsupported staged status ${status} for ${relativePath}`);
    changes.push({ status, path: relativePath });
  }
  return changes;
}

function indexEntry(root, indexFile, relativePath, env) {
  const listing = utf8(git(root, ["ls-files", "-s", "-z", "--", relativePath], { env, indexFile }).stdout, "git index");
  const records = listing.split("\0");
  if (records.pop() !== "" || records.length !== 1) fail(`expected one index entry for ${relativePath}`);
  const match = records[0].match(/^(100644|100755) ([0-9a-f]{40}) 0\t(.+)$/);
  if (!match || match[3] !== relativePath) fail(`unexpected index entry for ${relativePath}`);
  return { mode: match[1], blob: match[2] };
}

function temporaryIndex(base = os.tmpdir()) {
  fs.mkdirSync(base, { recursive: true });
  const directory = fs.mkdtempSync(path.join(base, "tine-reddit-evidence-index-"));
  return { directory, file: path.join(directory, "index") };
}

function cleanupTemporaryIndex(temporary) {
  if (temporary) fs.rmSync(temporary.directory, { recursive: true, force: true });
}

function readManifest(file) {
  lstatRegular(file, "manifest");
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch (error) {
    fail(`manifest is not valid JSON: ${error.message}`);
  }
}

function emptyManifest({ requestedVersion, repository, sourceCommit, runId, runAttempt }) {
  return {
    schemaVersion: SCHEMA_VERSION,
    repository: repository ?? null,
    sourceCommit: sourceCommit ?? null,
    requestedVersion: requestedVersion ?? null,
    runId: runId ?? null,
    runAttempt: runAttempt ?? null,
    generatedAt: new Date().toISOString(),
    importable: false,
    commands: {
      sync: { exitCode: null },
      blogCheck: { exitCode: null },
      readiness: { exitCode: null },
    },
    files: [],
    patch: null,
  };
}

function writeManifest(directory, manifest) {
  fs.writeFileSync(path.join(directory, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
}

function command(root, env, args) {
  const result = spawnSync("npm", args, {
    cwd: root,
    env,
    encoding: "buffer",
    maxBuffer: 64 * 1024 * 1024,
  });
  if (typeof result.status === "number") return { exitCode: result.status, signal: result.signal ?? null };
  if (result.error) return { exitCode: null, error: result.error.message };
  return { exitCode: null, error: "npm did not return an exit status" };
}

function expectedArtifactFiles(manifest) {
  const expected = new Set(["manifest.json", PATCH_NAME]);
  for (const item of manifest.files) expected.add(`${FILES_DIRECTORY}/${item.path}`);
  return expected;
}

function artifactFiles(directory, prefix = "") {
  const found = new Set();
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
    const full = path.join(directory, entry.name);
    const stat = fs.lstatSync(full);
    if (stat.isSymbolicLink()) fail(`artifact contains a symlink: ${relative}`);
    if (stat.isDirectory()) {
      for (const nested of artifactFiles(full, relative)) found.add(nested);
    } else if (stat.isFile()) {
      found.add(relative);
    } else {
      fail(`artifact contains a non-regular entry: ${relative}`);
    }
  }
  return found;
}

function sameSet(actual, expected, description) {
  if (actual.size !== expected.size || [...actual].some((value) => !expected.has(value))) {
    fail(`${description} does not match manifest`);
  }
}

function verifyManifestShape(manifest) {
  if (!manifest || manifest.schemaVersion !== SCHEMA_VERSION) fail("artifact schema version is unsupported");
  if (manifest.importable !== true) fail("artifact manifest is not importable");
  if (!manifest.commands || !["sync", "blogCheck", "readiness"].every((key) => manifest.commands[key]?.exitCode === 0)) {
    fail("artifact records a failed or missing Reddit command");
  }
  if (!Array.isArray(manifest.files) || !manifest.patch || typeof manifest.patch.diff !== "string") {
    fail("artifact manifest is missing files or patch data");
  }
  if (manifest.patch.filename !== PATCH_NAME || !/^[0-9a-f]{64}$/.test(manifest.patch.sha256 ?? "")) {
    fail("artifact patch metadata is invalid");
  }
}

function validateManifestFiles(manifest, version) {
  const paths = new Set();
  for (const item of manifest.files) {
    if (!item || typeof item !== "object") fail("manifest has an invalid file entry");
    const relativePath = validateArtifactPath(item.path, version);
    if (paths.has(relativePath)) fail(`manifest duplicates file ${relativePath}`);
    paths.add(relativePath);
    if (!Number.isSafeInteger(item.byteLength) || item.byteLength < 0 || !/^[0-9a-f]{64}$/.test(item.sha256 ?? "")) {
      fail(`manifest file metadata is invalid for ${relativePath}`);
    }
    if ((item.status !== "A" && item.status !== "M") || !/^(100644|100755)$/.test(item.mode ?? "")) {
      fail(`manifest file status or mode is invalid for ${relativePath}`);
    }
  }
  const evidence = `docs/releases/v${version}-reddit.json`;
  if (!paths.has(evidence)) fail(`manifest is missing ${evidence}`);
  return paths;
}

function setGithubOutput(env, version) {
  if (!env.GITHUB_OUTPUT) return;
  fs.appendFileSync(env.GITHUB_OUTPUT, `version=${version}\n`);
}

export function createArtifact({ root = process.cwd(), env = process.env } = {}) {
  const runnerTemp = env.RUNNER_TEMP || os.tmpdir();
  const artifactDirectory = path.join(path.resolve(runnerTemp), "reddit-release-evidence");
  fs.mkdirSync(artifactDirectory, { recursive: true });
  const manifest = emptyManifest({
    requestedVersion: env.REDDIT_VERSION ?? null,
    repository: env.GITHUB_REPOSITORY ?? null,
    sourceCommit: env.GITHUB_SHA ?? null,
    runId: env.GITHUB_RUN_ID ?? null,
    runAttempt: env.GITHUB_RUN_ATTEMPT ?? null,
  });
  let temporary;
  try {
    const packageJson = JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8"));
    const version = validateVersion(env.REDDIT_VERSION, packageJson.version);
    manifest.requestedVersion = version;
    setGithubOutput(env, version);

    const repository = requiredString(env.GITHUB_REPOSITORY, "GITHUB_REPOSITORY");
    const expectedSha = validSha(env.GITHUB_SHA, "GITHUB_SHA");
    const runId = requiredString(env.GITHUB_RUN_ID, "GITHUB_RUN_ID");
    const runAttempt = requiredString(env.GITHUB_RUN_ATTEMPT, "GITHUB_RUN_ATTEMPT");
    cleanCheckout(root, env);
    const actualSha = currentHead(root, env);
    if (actualSha !== expectedSha) fail(`checkout SHA ${actualSha} does not match GITHUB_SHA ${expectedSha}`);
    Object.assign(manifest, { repository, sourceCommit: actualSha, runId, runAttempt });

    manifest.commands.sync = command(root, env, ["run", "blog:sync", "--", `--version=${version}`]);
    manifest.commands.blogCheck = command(root, env, ["run", "blog:check"]);
    manifest.commands.readiness = command(root, env, ["run", "check:release-readiness"]);

    const changes = parseStatus(git(root, ["status", "--porcelain=v1", "-z", "--untracked-files=all"], { env }).stdout);
    const evidencePath = `docs/releases/v${version}-reddit.json`;
    if (!changes.some((change) => change.path === evidencePath)) fail(`Reddit synchronization did not change ${evidencePath}`);
    for (const change of changes) {
      validateArtifactPath(change.path, version);
      lstatRegular(repositoryPath(root, change.path), `changed artifact file ${change.path}`);
    }

    temporary = temporaryIndex(runnerTemp);
    git(root, ["read-tree", "HEAD"], { env, indexFile: temporary.file });
    git(root, ["add", "--", ...changes.map((change) => change.path)], { env, indexFile: temporary.file });
    const staged = parseNameStatus(git(root, ["diff", "--cached", "--name-status", "-z", "--no-renames", "HEAD"], {
      env,
      indexFile: temporary.file,
    }).stdout);
    const expectedChanges = new Map(changes.map((change) => [change.path, change.status]));
    if (staged.length !== expectedChanges.size || staged.some((change) => expectedChanges.get(change.path) !== change.status)) {
      fail("temporary index does not exactly match the validated repository delta");
    }

    for (const change of staged) {
      const index = indexEntry(root, temporary.file, change.path, env);
      const bytes = git(root, ["show", `:${change.path}`], { env, indexFile: temporary.file }).stdout;
      const destination = path.join(artifactDirectory, FILES_DIRECTORY, change.path);
      fs.mkdirSync(path.dirname(destination), { recursive: true });
      fs.writeFileSync(destination, bytes);
      manifest.files.push({
        path: change.path,
        byteLength: bytes.length,
        sha256: sha256(bytes),
        status: change.status,
        mode: index.mode,
      });
    }

    const patchBytes = git(root, ["diff", "--cached", "--binary", "--no-ext-diff", "--no-renames", "HEAD"], {
      env,
      indexFile: temporary.file,
    }).stdout;
    const patchText = utf8(patchBytes, "generated patch");
    fs.writeFileSync(path.join(artifactDirectory, PATCH_NAME), patchBytes);
    manifest.patch = {
      filename: PATCH_NAME,
      byteLength: patchBytes.length,
      sha256: sha256(patchBytes),
      diff: patchText,
    };

    const evidenceFile = repositoryPath(root, evidencePath);
    lstatRegular(evidenceFile, "Reddit evidence");
    const evidence = JSON.parse(fs.readFileSync(evidenceFile, "utf8"));
    const sourceManifest = JSON.parse(fs.readFileSync(path.join(root, "website/blog/reddit-sources.json"), "utf8"));
    const evidenceProblems = validateRedditReleaseEvidence(evidence, {
      version,
      author: sourceManifest.author,
      requiredSources: sourceManifest.sources,
    });
    manifest.importable = manifest.commands.sync.exitCode === 0
      && manifest.commands.blogCheck.exitCode === 0
      && manifest.commands.readiness.exitCode === 0
      && evidenceProblems.length === 0;
    if (evidenceProblems.length) manifest.evidenceProblems = evidenceProblems;
    if (!manifest.importable) fail("Reddit evidence collection is incomplete or a release check failed");
  } catch (error) {
    manifest.importable = false;
    manifest.error = String(error?.message ?? error);
  } finally {
    cleanupTemporaryIndex(temporary);
    writeManifest(artifactDirectory, manifest);
  }
  return { ok: manifest.importable, artifactDirectory, manifest };
}

function parseVerifyArguments(args) {
  const options = {};
  for (const argument of args) {
    const match = argument.match(/^--([a-z-]+)=(.*)$/);
    if (!match) fail(`unknown verify argument: ${argument}`);
    options[match[1]] = match[2];
  }
  return {
    artifact: requiredString(options.artifact, "--artifact"),
    repository: requiredString(options.repository, "--repository"),
    sha: validSha(options.sha, "--sha"),
    version: validateVersion(options.version, options.version),
    runId: requiredString(options["run-id"], "--run-id"),
    runAttempt: requiredString(options["run-attempt"], "--run-attempt"),
  };
}

export function verifyArtifact({ root = process.cwd(), artifact, repository, sha, version, runId, runAttempt, env = process.env } = {}) {
  requiredString(artifact, "artifact");
  requiredString(repository, "repository");
  validSha(sha, "sha");
  validateVersion(version, version);
  requiredString(runId, "runId");
  requiredString(runAttempt, "runAttempt");
  cleanCheckout(root, env);
  if (currentHead(root, env) !== sha) fail(`checkout SHA does not match expected SHA ${sha}`);

  const artifactDirectory = path.resolve(artifact);
  let artifactStat;
  try {
    artifactStat = fs.lstatSync(artifactDirectory);
  } catch {
    fail(`artifact directory is missing: ${artifactDirectory}`);
  }
  if (!artifactStat.isDirectory() || artifactStat.isSymbolicLink()) fail("artifact directory must be a non-symlink directory");
  const manifest = readManifest(path.join(artifactDirectory, "manifest.json"));
  verifyManifestShape(manifest);
  if (manifest.repository !== repository || manifest.sourceCommit !== sha || manifest.requestedVersion !== version
    || manifest.runId !== runId || manifest.runAttempt !== runAttempt) {
    fail("artifact provenance does not match the expected repository, SHA, version, run, or attempt");
  }
  const paths = validateManifestFiles(manifest, version);
  sameSet(artifactFiles(artifactDirectory), expectedArtifactFiles(manifest), "artifact file set");

  const patchFile = path.join(artifactDirectory, PATCH_NAME);
  lstatRegular(patchFile, "artifact patch");
  const patchBytes = fs.readFileSync(patchFile);
  if (patchBytes.length !== manifest.patch.byteLength || sha256(patchBytes) !== manifest.patch.sha256
    || utf8(patchBytes, "artifact patch") !== manifest.patch.diff) {
    fail("artifact patch bytes do not match manifest");
  }

  for (const item of manifest.files) {
    const copy = path.join(artifactDirectory, FILES_DIRECTORY, item.path);
    const stat = lstatRegular(copy, `artifact copy ${item.path}`);
    const bytes = fs.readFileSync(copy);
    if (stat.size !== item.byteLength || bytes.length !== item.byteLength || sha256(bytes) !== item.sha256) {
      fail(`artifact copy hash or size mismatch for ${item.path}`);
    }
  }

  const evidencePath = `docs/releases/v${version}-reddit.json`;
  const evidence = JSON.parse(fs.readFileSync(path.join(artifactDirectory, FILES_DIRECTORY, evidencePath), "utf8"));
  const sourcesPath = "website/blog/reddit-sources.json";
  const changedSources = manifest.files.some((item) => item.path === sourcesPath);
  const sourceManifest = JSON.parse(fs.readFileSync(
    changedSources ? path.join(artifactDirectory, FILES_DIRECTORY, sourcesPath) : path.join(root, sourcesPath),
    "utf8",
  ));
  const evidenceProblems = validateRedditReleaseEvidence(evidence, {
    version,
    author: sourceManifest.author,
    requiredSources: sourceManifest.sources,
  });
  if (evidenceProblems.length) fail(`artifact Reddit evidence is incomplete: ${evidenceProblems.join("; ")}`);

  let temporary;
  try {
    temporary = temporaryIndex(env.RUNNER_TEMP || os.tmpdir());
    git(root, ["read-tree", "HEAD"], { env, indexFile: temporary.file });
    git(root, ["apply", "--cached", "--whitespace=nowarn", "--", patchFile], { env, indexFile: temporary.file });
    const staged = parseNameStatus(git(root, ["diff", "--cached", "--name-status", "-z", "--no-renames", "HEAD"], {
      env,
      indexFile: temporary.file,
    }).stdout);
    const expectedChanges = new Map(manifest.files.map((item) => [item.path, item.status]));
    if (staged.length !== expectedChanges.size || staged.some((change) => !paths.has(change.path) || expectedChanges.get(change.path) !== change.status)) {
      fail("replayed patch path/status set does not exactly match manifest");
    }
    for (const item of manifest.files) {
      const entry = indexEntry(root, temporary.file, item.path, env);
      if (entry.mode !== item.mode) fail(`replayed patch mode does not match manifest for ${item.path}`);
      const bytes = git(root, ["show", `:${item.path}`], { env, indexFile: temporary.file }).stdout;
      const copy = fs.readFileSync(path.join(artifactDirectory, FILES_DIRECTORY, item.path));
      if (!bytes.equals(copy)) fail(`replayed patch bytes do not match artifact copy for ${item.path}`);
    }
  } finally {
    cleanupTemporaryIndex(temporary);
  }
  return { manifest, artifactDirectory };
}

function usage() {
  console.error("usage: reddit-evidence-artifact.mjs create | verify --artifact=DIR --repository=OWNER/REPO --sha=SHA --version=X.Y.0 --run-id=ID --run-attempt=N");
}

const invokedAsScript = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (invokedAsScript) {
  const [subcommand, ...args] = process.argv.slice(2);
  try {
    if (subcommand === "create" && args.length === 0) {
      const result = createArtifact();
      if (!result.ok) process.exitCode = 1;
    } else if (subcommand === "verify") {
      verifyArtifact(parseVerifyArguments(args));
    } else {
      usage();
      process.exitCode = 2;
    }
  } catch (error) {
    console.error(`Reddit evidence artifact failed: ${error.message}`);
    process.exitCode = 1;
  }
}
