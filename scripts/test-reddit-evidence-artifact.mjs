#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  createArtifact,
  validateArtifactPath,
  validateVersion,
  verifyArtifact,
} from "./reddit-evidence-artifact.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const helper = path.join(root, "scripts", "reddit-evidence-artifact.mjs");
const repository = "martinkoutecky/tine";
const sourceUrl = "https://www.reddit.com/r/TineOutline/comments/abc123/release_post/";

function run(command, args, cwd, options = {}) {
  const result = spawnSync(command, args, { cwd, encoding: "utf8", ...options });
  if ((result.error && result.status !== 0) || result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed: ${result.error?.message ?? result.stderr}`);
  }
  return result.stdout;
}

function git(cwd, args) {
  return run("git", args, cwd);
}

function hash(bytes) {
  return crypto.createHash("sha256").update(bytes).digest("hex");
}

function completeEvidence() {
  return {
    schemaVersion: 2,
    transport: "reddit-json",
    complete: true,
    version: "0.6.0",
    generatedAt: "2026-07-18T00:00:00.000Z",
    feedUrl: "https://www.reddit.com/user/al-Quaknaa/submitted.json?raw_json=1&limit=100",
    feedPages: 1,
    feedAfter: null,
    feedErrors: [],
    feedUpdated: "2026-07-18T00:00:00.000Z",
    author: "al-Quaknaa",
    authorPosts: [],
    unprocessed: [],
    threadSnapshots: [{
      version: "0.6.0",
      url: sourceUrl,
      sourceUrl: "https://www.reddit.com/comments/abc123.json?raw_json=1&limit=500&depth=10&sort=new",
      blogFile: "v0.6.0.html",
      entryCount: 1,
      latestUpdate: "2026-07-18T00:00:00.000Z",
      sha256: "a".repeat(64),
    }],
    failedThreads: [],
  };
}

function fixture(base) {
  const project = path.join(base, "project");
  const bin = path.join(base, "bin");
  fs.mkdirSync(path.join(project, "docs", "releases"), { recursive: true });
  fs.mkdirSync(path.join(project, "website", "blog"), { recursive: true });
  fs.mkdirSync(bin, { recursive: true });
  fs.writeFileSync(path.join(project, "package.json"), `${JSON.stringify({ version: "0.6.0" })}\n`);
  fs.writeFileSync(path.join(project, "docs", "releases", "v0.6.0-reddit.json"), "{}\n");
  fs.writeFileSync(path.join(project, "website", "blog", "reddit-sources.json"), `${JSON.stringify({
    schemaVersion: 1,
    author: "al-Quaknaa",
    subreddit: "TineOutline",
    sources: [{ version: "0.6.0", url: sourceUrl, blogFile: "v0.6.0.html" }],
  })}\n`);
  fs.writeFileSync(path.join(project, "website", "blog", "v0.6.0.html"), sourceUrl);
  git(project, ["init", "-q"]);
  git(project, ["config", "user.email", "fixture@example.invalid"]);
  git(project, ["config", "user.name", "Fixture"]);
  git(project, ["add", "."]);
  git(project, ["commit", "-qm", "fixture base"]);
  const sha = git(project, ["rev-parse", "HEAD"]).trim();
  const npm = path.join(bin, "npm");
  fs.writeFileSync(npm, `#!/usr/bin/env node
const fs = require("fs");
const path = require("path");
const evidence = ${JSON.stringify(completeEvidence(), null, 2)};
const fail403 = process.env.FIXTURE_REDDIT_403 === "1";
const mutation = process.env.FIXTURE_MUTATION;
if (process.argv.includes("blog:sync")) {
  if (fail403) {
    evidence.complete = false;
    evidence.feedErrors = [{ error: "HTTP 403" }];
  }
  fs.writeFileSync(path.join(process.cwd(), "docs/releases/v0.6.0-reddit.json"), JSON.stringify(evidence, null, 2) + "\\n");
  fs.writeFileSync(path.join(process.cwd(), "website/blog/generated.html"), "generated\\n");
  if (mutation === "wrong-version") fs.writeFileSync(path.join(process.cwd(), "docs/releases/v0.5.0-reddit.json"), "wrong version\\n");
  if (mutation === "unrelated") fs.writeFileSync(path.join(process.cwd(), "unrelated.txt"), "unrelated\\n");
  if (mutation === "deletion") fs.unlinkSync(path.join(process.cwd(), "docs/releases/v0.6.0-reddit.json"));
  if (mutation === "rename") fs.renameSync(path.join(process.cwd(), "docs/releases/v0.6.0-reddit.json"), path.join(process.cwd(), "docs/releases/renamed.json"));
  if (mutation === "symlink") {
    fs.unlinkSync(path.join(process.cwd(), "docs/releases/v0.6.0-reddit.json"));
    fs.symlinkSync("../v0.6.0-reddit.json", path.join(process.cwd(), "docs/releases/v0.6.0-reddit.json"));
  }
  process.exit(fail403 ? 1 : 0);
}
process.exit(0);
`);
  fs.chmodSync(npm, 0o755);
  return { project, bin, sha };
}

function createEnv(base, fixtureRoot, extra = {}) {
  const runnerTemp = path.join(base, "runner-temp");
  fs.mkdirSync(runnerTemp, { recursive: true });
  return {
    ...process.env,
    PATH: `${fixtureRoot.bin}${path.delimiter}${process.env.PATH}`,
    RUNNER_TEMP: runnerTemp,
    REDDIT_VERSION: "0.6.0",
    GITHUB_REPOSITORY: repository,
    GITHUB_SHA: fixtureRoot.sha,
    GITHUB_RUN_ID: "12345",
    GITHUB_RUN_ATTEMPT: "2",
    GITHUB_OUTPUT: path.join(base, "github-output"),
    ...extra,
  };
}

function cloneFixture(source, destination) {
  run("git", ["clone", "-q", source, destination], path.dirname(destination));
  return destination;
}

function expectFailure(callback, pattern) {
  assert.throws(callback, pattern);
}

function parseJobs(workflow) {
  const jobsAt = workflow.indexOf("\njobs:\n");
  assert.ok(jobsAt >= 0, "CI workflow has no jobs mapping");
  const body = workflow.slice(jobsAt + 7);
  const matches = [...body.matchAll(/^  ([A-Za-z0-9_-]+):\n/gm)];
  const jobs = new Map();
  for (let index = 0; index < matches.length; index += 1) {
    const start = matches[index].index + matches[index][0].length;
    const end = index + 1 < matches.length ? matches[index + 1].index : body.length;
    const source = body.slice(start, end);
    const condition = source.match(/^    if: (.+)$/m)?.[1] ?? null;
    jobs.set(matches[index][1], { source, condition });
  }
  return jobs;
}

function runsForReddit(condition) {
  if (!condition) return true;
  const expression = condition
    .replaceAll("github.event_name", "'workflow_dispatch'")
    .replaceAll("inputs.scope", "'reddit'")
    .replaceAll("==", "===");
  assert.match(expression, /^[\s'()=!&|a-z_-]+$/, `unsupported CI condition syntax: ${condition}`);
  return Function(`return (${expression});`)();
}

for (const invalid of ["", "0.6.1", "0.6.0-rc.1", " 0.6.0", "0.6.0\n", "0.6.0;id", "0/6/0", "../0.6.0"]) {
  expectFailure(() => validateVersion(invalid, "0.6.0"), /reddit_version/);
}
assert.equal(validateVersion("0.6.0", "0.6.0"), "0.6.0");
expectFailure(() => validateVersion("0.7.0", "0.6.0"), /does not match/);
for (const unsafe of ["/tmp/file", "../file", "docs/releases/v0.5.0-reddit.json", "docs/releases/v0.6.0-reddit.json/extra", "website/other.html", "website/blog/../other.html", "website\\blog\\file.html"]) {
  expectFailure(() => validateArtifactPath(unsafe, "0.6.0"), /artifact path/);
}
assert.equal(validateArtifactPath("docs/releases/v0.6.0-reddit.json", "0.6.0"), "docs/releases/v0.6.0-reddit.json");
assert.equal(validateArtifactPath("website/blog/generated.html", "0.6.0"), "website/blog/generated.html");

const ci = fs.readFileSync(path.join(root, ".github/workflows/ci.yml"), "utf8");
const jobs = parseJobs(ci);
const redditJob = jobs.get("reddit-release-evidence");
assert.ok(redditJob, "CI workflow is missing the parsed Reddit evidence job");
assert.equal(runsForReddit(redditJob.condition), true, "Reddit job is not manual-Reddit-only");
assert.match(redditJob.source, /^    permissions:\n      contents: read$/m, "Reddit job permissions are not read-only");
assert.match(redditJob.source, /uses: actions\/checkout@v4\n        with:\n          ref: \$\{\{ github\.sha \}\}\n          persist-credentials: false/, "Reddit checkout is not exact and credential-free");
assert.match(redditJob.source, /REDDIT_VERSION: \$\{\{ inputs\.reddit_version \}\}/, "Reddit input is not isolated in environment");
assert.doesNotMatch(redditJob.source.replace(/REDDIT_VERSION: \$\{\{ inputs\.reddit_version \}\}/, ""), /inputs\.reddit_version/, "untrusted Reddit input reaches a shell/path/name field");
assert.doesNotMatch(redditJob.source, /secrets\.|\b(?:git\s+(?:commit|push|tag)|gh\s+release)\b/, "Reddit job consumes a secret or mutates a release");
assert.match(redditJob.source, /steps\.reddit-evidence\.outputs\.version/, "artifact name does not use the validated helper output");
for (const [name, job] of jobs) {
  assert.equal(runsForReddit(job.condition), name === "reddit-release-evidence", `${name} can run for scope=reddit`);
}

const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "tine-reddit-evidence-artifact-test-"));
try {
  const good = fixture(path.join(temporary, "good"));
  const goodEnv = createEnv(path.join(temporary, "good"), good);
  const created = createArtifact({ root: good.project, env: goodEnv });
  assert.equal(created.ok, true, "complete JSON fixture was not importable");
  assert.equal(created.manifest.importable, true);
  assert.match(fs.readFileSync(goodEnv.GITHUB_OUTPUT, "utf8"), /^version=0\.6\.0$/m);
  assert.equal(git(good.project, ["diff", "--cached", "--name-only"]), "", "create changed the repository index");
  assert.match(git(good.project, ["status", "--porcelain"]), /docs\/releases\/v0\.6\.0-reddit\.json/, "fixture did not retain generated worktree delta");

  const clean = cloneFixture(good.project, path.join(temporary, "clean"));
  verifyArtifact({
    root: clean,
    artifact: created.artifactDirectory,
    repository,
    sha: good.sha,
    version: "0.6.0",
    runId: "12345",
    runAttempt: "2",
    env: { ...process.env, RUNNER_TEMP: path.join(temporary, "verify-temp") },
  });
  assert.equal(git(clean, ["status", "--porcelain"]), "", "verify changed the worktree or repository index");
  for (const field of [
    ["repository", "other/repository"],
    ["sha", "b".repeat(40)],
    ["version", "0.7.0"],
    ["runId", "999"],
    ["runAttempt", "3"],
  ]) {
    const options = { root: clean, artifact: created.artifactDirectory, repository, sha: good.sha, version: "0.6.0", runId: "12345", runAttempt: "2" };
    options[field[0]] = field[1];
    expectFailure(() => verifyArtifact(options), /provenance|checkout SHA|does not match/);
  }

  const dirty = cloneFixture(good.project, path.join(temporary, "dirty"));
  fs.writeFileSync(path.join(dirty, "untracked.txt"), "dirty\n");
  expectFailure(() => verifyArtifact({ root: dirty, artifact: created.artifactDirectory, repository, sha: good.sha, version: "0.6.0", runId: "12345", runAttempt: "2" }), /checkout must be clean/);

  const bytesTampered = path.join(temporary, "bytes-tampered");
  fs.cpSync(created.artifactDirectory, bytesTampered, { recursive: true });
  fs.appendFileSync(path.join(bytesTampered, "files/docs/releases/v0.6.0-reddit.json"), "tampered\n");
  expectFailure(() => verifyArtifact({ root: clean, artifact: bytesTampered, repository, sha: good.sha, version: "0.6.0", runId: "12345", runAttempt: "2" }), /hash or size mismatch/);

  const duplicate = path.join(temporary, "duplicate-entry");
  fs.cpSync(created.artifactDirectory, duplicate, { recursive: true });
  const duplicateManifestPath = path.join(duplicate, "manifest.json");
  const duplicateManifest = JSON.parse(fs.readFileSync(duplicateManifestPath, "utf8"));
  duplicateManifest.files.push({ ...duplicateManifest.files[0] });
  fs.writeFileSync(duplicateManifestPath, `${JSON.stringify(duplicateManifest, null, 2)}\n`);
  expectFailure(() => verifyArtifact({ root: clean, artifact: duplicate, repository, sha: good.sha, version: "0.6.0", runId: "12345", runAttempt: "2" }), /duplicates/);

  const failedCommand = path.join(temporary, "failed-command");
  fs.cpSync(created.artifactDirectory, failedCommand, { recursive: true });
  const failedCommandPath = path.join(failedCommand, "manifest.json");
  const failedCommandManifest = JSON.parse(fs.readFileSync(failedCommandPath, "utf8"));
  failedCommandManifest.commands.sync.exitCode = 1;
  fs.writeFileSync(failedCommandPath, `${JSON.stringify(failedCommandManifest, null, 2)}\n`);
  expectFailure(() => verifyArtifact({ root: clean, artifact: failedCommand, repository, sha: good.sha, version: "0.6.0", runId: "12345", runAttempt: "2" }), /failed or missing/);

  for (const [name, mutate] of [["incomplete", (evidence) => { evidence.complete = false; }], ["rest-error", (evidence) => { evidence.feedErrors = [{ error: "HTTP 403" }]; }]]) {
    const altered = path.join(temporary, name);
    fs.cpSync(created.artifactDirectory, altered, { recursive: true });
    const alteredManifestPath = path.join(altered, "manifest.json");
    const alteredManifest = JSON.parse(fs.readFileSync(alteredManifestPath, "utf8"));
    const evidencePath = path.join(altered, "files/docs/releases/v0.6.0-reddit.json");
    const evidence = JSON.parse(fs.readFileSync(evidencePath, "utf8"));
    mutate(evidence);
    const bytes = Buffer.from(`${JSON.stringify(evidence, null, 2)}\n`);
    fs.writeFileSync(evidencePath, bytes);
    const entry = alteredManifest.files.find((item) => item.path === "docs/releases/v0.6.0-reddit.json");
    entry.byteLength = bytes.length;
    entry.sha256 = hash(bytes);
    fs.writeFileSync(alteredManifestPath, `${JSON.stringify(alteredManifest, null, 2)}\n`);
    expectFailure(() => verifyArtifact({ root: clean, artifact: altered, repository, sha: good.sha, version: "0.6.0", runId: "12345", runAttempt: "2" }), /incomplete/);
  }

  const patchedWorktree = cloneFixture(good.project, path.join(temporary, "patch-worktree"));
  run("git", ["apply", path.join(created.artifactDirectory, "reddit-evidence.patch")], patchedWorktree);
  fs.writeFileSync(path.join(patchedWorktree, "website/blog/extra-allowed.html"), "extra\n");
  fs.writeFileSync(path.join(patchedWorktree, "unrelated.txt"), "unrelated\n");
  const extraPatch = Buffer.from(git(patchedWorktree, ["diff", "--binary", "--no-ext-diff", "--no-renames", "HEAD"]));
  const patchTampered = path.join(temporary, "patch-tampered");
  fs.cpSync(created.artifactDirectory, patchTampered, { recursive: true });
  fs.writeFileSync(path.join(patchTampered, "reddit-evidence.patch"), extraPatch);
  const patchManifestPath = path.join(patchTampered, "manifest.json");
  const patchManifest = JSON.parse(fs.readFileSync(patchManifestPath, "utf8"));
  patchManifest.patch = { ...patchManifest.patch, byteLength: extraPatch.length, sha256: hash(extraPatch), diff: extraPatch.toString("utf8") };
  fs.writeFileSync(patchManifestPath, `${JSON.stringify(patchManifest, null, 2)}\n`);
  expectFailure(() => verifyArtifact({ root: clean, artifact: patchTampered, repository, sha: good.sha, version: "0.6.0", runId: "12345", runAttempt: "2" }), /path\/status set/);

  const bad = fixture(path.join(temporary, "rest-403"));
  const badEnv = createEnv(path.join(temporary, "rest-403"), bad, { FIXTURE_REDDIT_403: "1" });
  const failed = spawnSync(process.execPath, [helper, "create"], { cwd: bad.project, env: badEnv, encoding: "utf8" });
  assert.equal(failed.status, 1, "mocked Reddit 403 did not fail create");
  const failedManifest = JSON.parse(fs.readFileSync(path.join(badEnv.RUNNER_TEMP, "reddit-release-evidence", "manifest.json"), "utf8"));
  assert.equal(failedManifest.importable, false, "mocked Reddit 403 produced an importable artifact");
  assert.equal(failedManifest.commands.sync.exitCode, 1, "mocked Reddit 403 was not recorded as a failed sync");
  assert.match(failedManifest.error, /incomplete|release check/, "mocked Reddit 403 did not fail closed");

  for (const [name, mutation, pattern] of [
    ["wrong-version", "wrong-version", /allowlist/],
    ["unrelated", "unrelated", /allowlist/],
    ["deletion", "deletion", /did not change|unsupported git status/],
    ["rename", "rename", /did not change|unsupported git status/],
    ["symlink", "symlink", /regular non-symlink|unsupported git status/],
  ]) {
    const malformed = fixture(path.join(temporary, name));
    const malformedResult = createArtifact({
      root: malformed.project,
      env: createEnv(path.join(temporary, name), malformed, { FIXTURE_MUTATION: mutation }),
    });
    assert.equal(malformedResult.ok, false, `${name} changed path was accepted`);
    assert.match(malformedResult.manifest.error, pattern, `${name} rejection reason changed`);
  }
} finally {
  fs.rmSync(temporary, { recursive: true, force: true });
}

console.log("Reddit evidence artifact fixture tests passed (isolated CI lane + provenance-bound patch verification).");
