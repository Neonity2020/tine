#!/usr/bin/env node

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { assembleCandidate } from "./assemble-release-candidate.mjs";
import {
  collectGithubPages,
  REQUIRED_FULL_CI_JOBS,
  selectExactCiEvidence,
} from "./ci-evidence-lib.mjs";
import {
  tauriCapabilities,
  webdriverServerArgs,
  windowsWebviewProfileSnapshot,
} from "./e2e-capabilities.mjs";
import { candidateProblems, releaseLayout, RELEASE_LANES } from "./release-layout.mjs";
import { LINUX_TINE_CORE_SHARD_COUNT } from "./tine-core-nextest-contract.mjs";

const version = "0.5.6";
const commit = "a".repeat(40);
const repository = "martinkoutecky/tine";
const layout = releaseLayout(version);
const releaseWorkflow = fs.readFileSync(path.join(process.cwd(), ".github/workflows/release.yml"), "utf8");
const ciWorkflow = fs.readFileSync(path.join(process.cwd(), ".github/workflows/ci.yml"), "utf8");
const nextestConfig = fs.readFileSync(path.join(process.cwd(), ".config/nextest.toml"), "utf8");
const uiE2eWorkflow = fs.readFileSync(path.join(process.cwd(), ".github/workflows/ui-e2e.yml"), "utf8");
const flatpakWorkflow = fs.readFileSync(path.join(process.cwd(), ".github/workflows/flatpak.yml"), "utf8");
const flatpakMetadataWorkflow = fs.readFileSync(
  path.join(process.cwd(), ".github/workflows/flatpak-metadata.yml"),
  "utf8"
);
const preflight = fs.readFileSync(path.join(process.cwd(), "scripts/check-release-preflight.mjs"), "utf8");
const e2eRunner = fs.readFileSync(path.join(process.cwd(), "scripts/run-e2e.mjs"), "utf8");
const receiptHelper = fs.readFileSync(path.join(process.cwd(), "scripts/build-e2e-receipt.mjs"), "utf8");
const buildInputs = fs.readFileSync(path.join(process.cwd(), "scripts/build-e2e-inputs.mjs"), "utf8");
const windowsWebviewDriverInstaller = fs.readFileSync(
  path.join(process.cwd(), "scripts/install-windows-webview2-driver.ps1"),
  "utf8"
);
const printSecurity = fs.readFileSync(path.join(process.cwd(), "scripts/e2e-print-security.mjs"), "utf8");
const referenceParity = fs.readFileSync(path.join(process.cwd(), "scripts/e2e-og-parity-references.mjs"), "utf8");
const windowsScenarios = [
  "e2e-windows-smoke.mjs",
  "e2e-og-parity-references.mjs",
  "e2e-page-properties.mjs",
  "e2e-page-trailing-block.mjs",
  "e2e-pdf-logseq.mjs",
  "e2e-print-security.mjs",
  "e2e-tab-overflow.mjs",
];

const trackedPaths = execFileSync("git", ["ls-files", "-z"], { encoding: "utf8" })
  .split("\0")
  .filter(Boolean);
const pathOwners = new Map();
const caseInsensitiveCollisions = [];
for (const trackedPath of trackedPaths) {
  const portableKey = trackedPath.normalize("NFC").toLowerCase();
  const existing = pathOwners.get(portableKey);
  if (existing && existing !== trackedPath) {
    caseInsensitiveCollisions.push([existing, trackedPath]);
  } else {
    pathOwners.set(portableKey, trackedPath);
  }
}
assert.deepEqual(
  caseInsensitiveCollisions,
  [],
  "tracked paths must remain unique on case-insensitive filesystems"
);

function yamlBlock(lines, key, indent) {
  const header = `${" ".repeat(indent)}${key}:`;
  const start = lines.findIndex((line) => line === header);
  assert.ok(start >= 0, `CI workflow is missing YAML mapping ${key}`);

  let end = start + 1;
  while (end < lines.length) {
    const line = lines[end];
    if (line.trim() && line.length - line.trimStart().length <= indent) {
      break;
    }
    end += 1;
  }
  return lines.slice(start + 1, end);
}

function yamlScalar(lines, key, indent) {
  const prefix = `${" ".repeat(indent)}${key}:`;
  const line = lines.find((candidate) => candidate.startsWith(prefix));
  assert.ok(line, `CI workflow is missing YAML scalar ${key}`);
  return line.slice(prefix.length).trim();
}

function yamlNamedStep(lines, name) {
  const marker = `- name: ${name}`;
  const start = lines.findIndex((line) => line.trimStart() === marker);
  assert.ok(start >= 0, `CI workflow is missing step ${name}`);
  const indent = lines[start].length - lines[start].trimStart().length;

  let end = start + 1;
  while (end < lines.length) {
    const line = lines[end];
    if (line.trimStart().startsWith("- ") && line.length - line.trimStart().length === indent) {
      break;
    }
    end += 1;
  }
  return lines.slice(start, end);
}

function yamlLiteral(lines, key) {
  const line = lines.find((candidate) => candidate.trimStart() === `${key}: |`);
  assert.ok(line, `CI workflow is missing literal ${key}`);
  const indent = line.length - line.trimStart().length;
  const start = lines.indexOf(line) + 1;
  let end = start;
  while (end < lines.length) {
    const candidate = lines[end];
    if (candidate.trim() && candidate.length - candidate.trimStart().length <= indent) {
      break;
    }
    end += 1;
  }
  return lines.slice(start, end).map((candidate) => candidate.trim()).join("\n").trimEnd();
}

const ciYaml = ciWorkflow.split(/\r?\n/);

const successfulFullCiRun = {
  id: 1234,
  event: "workflow_dispatch",
  head_sha: commit,
  status: "completed",
  conclusion: "success",
  html_url: "https://example.invalid/actions/runs/1234",
};
const successfulFullCiJobs = REQUIRED_FULL_CI_JOBS.map((name) => ({ name, conclusion: "success" }));

assert.equal(layout.allAssets.length, 23, "release layout must retain its exact 23-asset inventory");
assert.equal(layout.platformAssets.length, 22, "release layout must retain its exact platform-asset inventory");
assert.equal(
  Object.keys(layout.updaterPlatforms).length,
  12,
  "AppImage update metadata must not add a Tauri updater platform"
);
assert.ok(
  layout.lanes["linux-x64"].assets.includes(`Tine_${version}_amd64.AppImage.zsync`),
  "linux-x64 is missing its AppImage update metadata"
);
assert.ok(
  layout.lanes["linux-arm64"].assets.includes(`Tine_${version}_aarch64.AppImage.zsync`),
  "linux-arm64 is missing its AppImage update metadata"
);
assert.match(
  releaseWorkflow,
  /lane: linux-x64[\s\S]*?appimage-update-info: "gh-releases-zsync\|martinkoutecky\|tine\|latest\|Tine_\*_amd64\.AppImage\.zsync"[\s\S]*?lane: linux-arm64[\s\S]*?appimage-update-info: "gh-releases-zsync\|martinkoutecky\|tine\|latest\|Tine_\*_aarch64\.AppImage\.zsync"/,
  "Linux release lanes do not declare the expected AppImage update metadata"
);
assert.match(
  releaseWorkflow,
  /UPDATE_INFORMATION: \$\{\{ matrix\.appimage-update-info \}\}/,
  "Tauri bundles do not receive their per-lane AppImage update information"
);
assert.match(
  releaseWorkflow,
  /name: Verify Linux AppImage update information[\s\S]*?\.\/src-tauri\/\$zsync_name[\s\S]*?readelf --string-dump=\.upd_info "\$appimage"[\s\S]*?gh-releases-zsync\|/,
  "release workflow verify step must search src-tauri/ for the .zsync (appimagetool writes it into the build CWD) and fail closed when update information is absent"
);
assert.match(
  releaseWorkflow,
  /name: Verify Android 9 native-loader compatibility[\s\S]*?unzip -p "\$apk" lib\/arm64-v8a\/libtine_lib\.so[\s\S]*?readelf --dyn-syms --wide[\s\S]*?renameat2/,
  "Android release packaging must inspect the final APK native library and reject the API-30 renameat2 wrapper"
);

// Architecture guard: the expensive Linux release build must test that exact
// binary before it can be staged for the atomic assembler/publisher. Windows
// consumes the staged portable binary in independent advisory jobs that neither
// serialize assembly nor hide one runner-wide 0/N failure.
assert.doesNotMatch(ciWorkflow, /\n  push:/, "ordinary CI still runs automatically on pushes");
assert.match(
  ciWorkflow,
  /workflow_dispatch:[\s\S]*?scope:[\s\S]*?options:[\s\S]*?- full[\s\S]*?- windows[\s\S]*?- android[\s\S]*?- performance/,
  "manual CI does not expose full and focused proof scopes"
);
assert.match(
  ciWorkflow,
  /pull_request:[\s\S]*?paths-ignore:[\s\S]*?"\*\*\/\*\.md"/,
  "docs-only pull requests still start app validation"
);
for (const name of REQUIRED_FULL_CI_JOBS) {
  if (/Full CI \/ Linux tine-core nextest shard [1-4]\/4/.test(name)) continue;
  assert.ok(ciWorkflow.includes(`name: ${name}`), `CI workflow is missing stable evidence job ${name}`);
}
assert.deepEqual(
  REQUIRED_FULL_CI_JOBS.filter((name) => name.includes("Linux tine-core nextest shard")),
  Array.from(
    { length: LINUX_TINE_CORE_SHARD_COUNT },
    (_, index) => `Full CI / Linux tine-core nextest shard ${index + 1}/${LINUX_TINE_CORE_SHARD_COUNT}`
  ),
  "exact-SHA evidence does not enumerate every Linux nextest shard"
);
assert.match(
  ciWorkflow,
  /test:[\s\S]*?name: Full CI \/ Linux tests and release contracts[\s\S]*?inputs\.scope == 'full'/,
  "the Linux full-CI evidence job can run in a focused dispatch"
);
assert.match(
  ciWorkflow,
  /test:\n    name: Full CI \/ Linux tests and release contracts[\s\S]*?uses: dtolnay\/rust-toolchain@1\.96\.0\n        with:\n          targets: wasm32-unknown-unknown[\s\S]*?name: Standalone plugin template builds and conforms\n        run: npm run plugin:template-check/,
  "the Linux full-CI plugin-template check does not install the WASM target"
);
const ciOn = yamlBlock(ciYaml, "on", 0);
const dispatch = yamlBlock(ciOn, "workflow_dispatch", 2);
const dispatchInputs = yamlBlock(dispatch, "inputs", 4);
const windowsTestInput = yamlBlock(dispatchInputs, "windows_test_name", 6);
assert.equal(yamlScalar(windowsTestInput, "required", 8), "false");
assert.equal(yamlScalar(windowsTestInput, "default", 8), '""');
assert.equal(yamlScalar(windowsTestInput, "type", 8), "string");

const runName = yamlScalar(ciYaml, "run-name", 0);
assert.ok(runName.includes("focused Windows / {0}"), "focused dispatches are not labeled in run metadata");
assert.ok(
  runName.includes("format('focused Windows / {0}', inputs.windows_test_name)"),
  "focused run metadata does not expose the exact selected test name"
);
assert.ok(runName.includes("full suite / {0}"), "full dispatches are not labeled in run metadata");
assert.ok(runName.includes("${{ github.sha }}"), "CI run metadata does not expose the exact dispatched SHA");
const ciPermissions = yamlBlock(ciYaml, "permissions", 0);
assert.equal(yamlScalar(ciPermissions, "contents", 2), "read");
assert.doesNotMatch(ciPermissions.join("\n"), /write/, "CI must not request write permissions");

const ciJobs = yamlBlock(ciYaml, "jobs", 0);
assert.match(nextestConfig, /^nextest-version = "0\.9\.143"$/m, "nextest version is not pinned");
assert.match(nextestConfig, /\[profile\.ci\][\s\S]*?default-filter = "all\(\)"/);
assert.match(nextestConfig, /\[profile\.ci\][\s\S]*?fail-fast = false/);
assert.match(nextestConfig, /\[profile\.ci\][\s\S]*?retries = 0/);
assert.match(nextestConfig, /\[profile\.ci\][\s\S]*?flaky-result = "fail"/);
assert.match(
  nextestConfig,
  /slow-timeout = \{ period = "5m", terminate-after = 2, grace-period = "30s", on-timeout = "fail" \}/,
  "nextest CI profile does not fail on a finite per-test timeout"
);
assert.match(nextestConfig, /\[profile\.ci\][\s\S]*?global-timeout = "4h"/);
assert.match(nextestConfig, /\[profile\.ci\][\s\S]*?status-level = "slow"[\s\S]*?final-status-level = "slow"/);
const windowsNextestProfile = nextestConfig.match(
  /^\[profile\.ci-windows\]\r?\n([\s\S]*?)(?=^\[|(?![\s\S]))/m
)?.[1];
assert.ok(windowsNextestProfile, "nextest config is missing the Windows profile");
assert.match(windowsNextestProfile, /^inherits = "ci"$/m);
assert.match(windowsNextestProfile, /^run-extra-args = \["--test-threads=1"\]$/m);
assert.doesNotMatch(
  windowsNextestProfile,
  /^test-threads\s*=\s*1\s*$/m,
  "Windows nextest profile globally serializes isolated test processes"
);
assert.doesNotMatch(nextestConfig, /on-timeout = "pass"|retries = [1-9]/, "nextest profile masks a failure");
const scopeValidation = yamlBlock(ciJobs, "validate-windows-focused-test-input", 2);
assert.equal(
  yamlScalar(scopeValidation, "if", 4),
  "github.event_name == 'workflow_dispatch' && inputs.windows_test_name != '' && inputs.scope != 'windows'"
);
const scopeValidationScript = yamlLiteral(
  yamlNamedStep(scopeValidation, "Reject Windows focused test outside Windows scope"),
  "run"
);
assert.ok(scopeValidationScript.includes("::error::windows_test_name may only be used with scope=windows."));
assert.ok(scopeValidationScript.endsWith("exit 1"));
const needsWindowsScope = (scope, testName) => testName !== "" && scope !== "windows";
for (const scope of ["full", "android", "performance"]) {
  assert.equal(needsWindowsScope(scope, "model::tests::focused"), true, `${scope} must reject a filter`);
}
assert.equal(needsWindowsScope("windows", "model::tests::focused"), false);
assert.equal(needsWindowsScope("full", ""), false);

const windowsCompile = yamlBlock(ciJobs, "windows-compile", 2);
assert.equal(
  yamlScalar(windowsCompile, "if", 4),
  "github.event_name == 'workflow_dispatch' && (inputs.scope == 'windows' || (inputs.scope == 'full' && inputs.windows_test_name == ''))"
);
const runsWindowsLane = (scope, testName) => scope === "windows" || (scope === "full" && testName === "");
assert.equal(runsWindowsLane("full", ""), true);
assert.equal(runsWindowsLane("full", "config::tests::focused"), false);
assert.equal(runsWindowsLane("windows", "config::tests::focused"), true);
const nextestInstall = yamlNamedStep(windowsCompile, "Install cargo-nextest 0.9.143");
assert.equal(yamlScalar(nextestInstall, "uses", 8), "taiki-e/install-action@v2");
assert.equal(yamlScalar(yamlBlock(nextestInstall, "with", 8), "tool", 10), "nextest@0.9.143");
const windowsCoreCompile = yamlNamedStep(windowsCompile, "Windows core test targets compile (all; release gate)");
assert.equal(yamlScalar(windowsCoreCompile, "if", 8), "inputs.windows_test_name == ''");
assert.equal(yamlScalar(windowsCoreCompile, "run", 8), "cargo test -p tine-core --no-run");
const windowsStorageCompile = yamlNamedStep(windowsCompile, "Windows storage test targets compile (all; release gate)");
assert.equal(yamlScalar(windowsStorageCompile, "if", 8), "inputs.windows_test_name == ''");
assert.equal(yamlScalar(windowsStorageCompile, "run", 8), "cargo test -p tine-storage --no-run");
const windowsCoreSmoke = yamlNamedStep(
  windowsCompile,
  "Windows core + storage smoke (isolated contract selections; release gate)"
);
assert.equal(yamlScalar(windowsCoreSmoke, "if", 8), "inputs.windows_test_name == ''");
assert.equal(yamlScalar(windowsCoreSmoke, "run", 8), "node scripts/tine-core-nextest-contract.mjs --mode windows --run-smoke");
assert.doesNotMatch(
  [yamlScalar(windowsCoreCompile, "run", 8), yamlScalar(windowsStorageCompile, "run", 8), yamlScalar(windowsCoreSmoke, "run", 8)].join("\n"),
  /continue-on-error|retries|--skip/,
  "Windows release coverage masks a failed compile, smoke, or storage test"
);
assert.doesNotMatch(
  windowsCompile.join("\n"),
  /cargo nextest run --profile ci-windows --package tine-storage$/m,
  "Windows release coverage accidentally restored the full tine-storage runtime suite"
);
assert.doesNotMatch(
  yamlScalar(windowsCoreSmoke, "run", 8),
  /cargo nextest run --profile ci-windows --package tine-core$/,
  "Windows release coverage accidentally restored the whole tine-core runtime suite"
);

const focusedWindowsCore = yamlNamedStep(
  windowsCompile,
  "Windows core test (focused exact serial) / ${{ inputs.windows_test_name }}"
);
assert.equal(yamlScalar(focusedWindowsCore, "if", 8), "inputs.scope == 'windows' && inputs.windows_test_name != ''");
assert.equal(yamlScalar(focusedWindowsCore, "shell", 8), "pwsh");
assert.equal(
  yamlScalar(yamlBlock(focusedWindowsCore, "env", 8), "TINE_WINDOWS_RUST_TEST", 10),
  "${{ inputs.windows_test_name }}"
);
const focusedWindowsScript = yamlLiteral(focusedWindowsCore, "run");
assert.ok(focusedWindowsScript.includes("$testName = $env:TINE_WINDOWS_RUST_TEST.Trim()"));
assert.ok(focusedWindowsScript.includes("[string]::IsNullOrWhiteSpace($testName)"));
assert.ok(focusedWindowsScript.includes("$testName -notmatch '^[A-Za-z0-9_]+(?:::[A-Za-z0-9_]+)*$'"));
assert.ok(focusedWindowsScript.includes("$listedTests = & cargo test -p tine-core --lib -- --list"));
assert.ok(focusedWindowsScript.includes('$_ -ceq "${testName}: test"'));
assert.ok(focusedWindowsScript.includes("if ($matchingTests.Count -ne 1)"));
assert.ok(focusedWindowsScript.includes('"--lib"'));
assert.ok(focusedWindowsScript.includes("$testName"));
assert.ok(focusedWindowsScript.includes('"--exact"'));
assert.ok(focusedWindowsScript.includes('"--nocapture"'));
assert.ok(focusedWindowsScript.includes('"--test-threads=1"'));
assert.ok(focusedWindowsScript.includes("& cargo @cargoArgs"));

const exactHarnessMatches = (listedTests, testName) =>
  listedTests.split(/\r?\n/).filter((line) => line === `${testName}: test`);
const knownHarnessName = "model::tests::active_rename_projection_scan_budget_has_exact_pre_commit_boundary";
assert.equal(exactHarnessMatches(`${knownHarnessName}: test`, knownHarnessName).length, 1);
assert.equal(exactHarnessMatches(`${knownHarnessName}: test`, "model::tests::unknown_name").length, 0);
assert.equal(exactHarnessMatches(`${knownHarnessName}: test`, knownHarnessName.toUpperCase()).length, 0);
assert.equal(exactHarnessMatches(`${knownHarnessName}: test\n${knownHarnessName}: test`, knownHarnessName).length, 2);
const safeRustTestPath = /^[A-Za-z0-9_]+(?:::[A-Za-z0-9_]+)*$/;
assert.equal("   \t".trim(), "");
assert.equal(safeRustTestPath.test(knownHarnessName), true);
assert.equal(safeRustTestPath.test("model::tests::unknown name"), false);
assert.equal(safeRustTestPath.test("model::tests::unknown; exit 0"), false);

const tauriCompile = yamlNamedStep(windowsCompile, "Windows Tauri shell compiles");
assert.equal(yamlScalar(tauriCompile, "run", 8), "cargo check -p tine --features custom-protocol");
const fullLinux = yamlBlock(ciJobs, "test", 2);
const linuxCoreContract = yamlBlock(ciJobs, "linux-core-nextest-contract", 2);
assert.equal(yamlScalar(linuxCoreContract, "name", 4), "Full CI / Linux tine-core nextest contract");
assert.equal(yamlScalar(linuxCoreContract, "if", 4), "github.event_name == 'workflow_dispatch' && inputs.scope == 'full'");
assert.equal(
  yamlScalar(yamlNamedStep(linuxCoreContract, "Install cargo-nextest 0.9.143"), "uses", 8),
  "taiki-e/install-action@v2"
);
assert.equal(
  yamlScalar(yamlBlock(yamlNamedStep(linuxCoreContract, "Install cargo-nextest 0.9.143"), "with", 8), "tool", 10),
  "nextest@0.9.143"
);
assert.equal(
  yamlScalar(yamlNamedStep(linuxCoreContract, "Verify Linux tine-core nextest inventory and deterministic shards"), "run", 8),
  "node scripts/tine-core-nextest-contract.mjs --mode linux"
);
const linuxCoreShards = yamlBlock(ciJobs, "linux-core-nextest", 2);
assert.equal(
  yamlScalar(linuxCoreShards, "name", 4),
  `Full CI / Linux tine-core nextest shard \${{ matrix.shard }}/${LINUX_TINE_CORE_SHARD_COUNT}`
);
assert.equal(yamlScalar(linuxCoreShards, "if", 4), "github.event_name == 'workflow_dispatch' && inputs.scope == 'full'");
assert.match(
  linuxCoreShards.join("\n"),
  new RegExp(`strategy:[\\s\\S]*?fail-fast: false[\\s\\S]*?shard: \\[1, 2, 3, ${LINUX_TINE_CORE_SHARD_COUNT}\\]`),
  "Linux nextest shard topology is not explicit and complete"
);
assert.equal(
  yamlScalar(
    yamlNamedStep(linuxCoreShards, "Linux tine-core nextest / deterministic hash shard ${{ matrix.shard }}/4"),
    "run",
    8
  ),
  "cargo nextest run --profile ci --package tine-core --partition hash:${{ matrix.shard }}/4"
);
assert.equal(
  yamlScalar(yamlNamedStep(linuxCoreShards, "Install cargo-nextest 0.9.143"), "uses", 8),
  "taiki-e/install-action@v2"
);
assert.equal(
  yamlScalar(yamlBlock(yamlNamedStep(linuxCoreShards, "Install cargo-nextest 0.9.143"), "with", 8), "tool", 10),
  "nextest@0.9.143"
);
// tine-storage's suite was compiled but never executed by any job until
// 2026-08-07. Assert the job exists, runs the whole package unpartitioned, and
// is release-required — a job outside REQUIRED_FULL_CI_JOBS can go red without
// blocking a tag, which is the same coverage hole in a different place.
const linuxStorage = yamlBlock(ciJobs, "linux-storage-nextest", 2);
assert.equal(yamlScalar(linuxStorage, "name", 4), "Full CI / Linux tine-storage nextest");
assert.equal(yamlScalar(linuxStorage, "if", 4), "github.event_name == 'workflow_dispatch' && inputs.scope == 'full'");
assert.equal(
  yamlScalar(yamlNamedStep(linuxStorage, "Linux tine-storage nextest / complete semantic suite"), "run", 8),
  "cargo nextest run --profile ci --package tine-storage"
);
assert.ok(
  REQUIRED_FULL_CI_JOBS.includes("Full CI / Linux tine-storage nextest"),
  "the Linux storage suite runs but is not required for exact-SHA release evidence"
);
assert.doesNotMatch(
  [linuxCoreContract, linuxCoreShards, linuxStorage, windowsCompile].map((job) => job.join("\n")).join("\n"),
  /continue-on-error:/,
  "nextest release evidence hides a failed contract or test job"
);
assert.doesNotMatch(fullLinux.join("\n"), /cargo test -p tine-core/, "Linux full evidence still has a monolithic core run");
const androidCompile = yamlBlock(ciJobs, "android-core-compile", 2);
const androidTestApk = yamlBlock(ciJobs, "android-test-apk", 2);
const performanceBench = yamlBlock(ciJobs, "bench", 2);
assert.equal(yamlScalar(fullLinux, "if", 4), "github.event_name == 'workflow_dispatch' && inputs.scope == 'full'");
assert.equal(
  yamlScalar(androidCompile, "if", 4),
  "github.event_name == 'workflow_dispatch' && (inputs.scope == 'full' || inputs.scope == 'android')"
);
assert.equal(yamlScalar(androidTestApk, "name", 4), "Android test APK / signed arm64 / ${{ github.sha }}");
assert.equal(
  yamlScalar(androidTestApk, "if", 4),
  "github.event_name == 'workflow_dispatch' && inputs.scope == 'android'"
);
const runsAndroidTestApk = (event, scope) => event === "workflow_dispatch" && scope === "android";
assert.equal(runsAndroidTestApk("workflow_dispatch", "android"), true);
assert.equal(runsAndroidTestApk("pull_request", "android"), false);
for (const scope of ["full", "windows", "performance"]) {
  assert.equal(runsAndroidTestApk("workflow_dispatch", scope), false, `${scope} must not build a test APK`);
}

const androidTestJava = yamlNamedStep(androidTestApk, "Set up Java 17");
assert.equal(yamlScalar(androidTestJava, "uses", 8), "actions/setup-java@v4");
assert.equal(yamlScalar(yamlBlock(androidTestJava, "with", 8), "distribution", 10), "temurin");
assert.equal(yamlScalar(yamlBlock(androidTestJava, "with", 8), "java-version", 10), '"17"');
const androidTestNode = yamlNamedStep(androidTestApk, "Set up Node 20");
assert.equal(yamlScalar(androidTestNode, "uses", 8), "actions/setup-node@v4");
assert.equal(yamlScalar(yamlBlock(androidTestNode, "with", 8), "node-version", 10), "20");
const androidTestRust = yamlNamedStep(androidTestApk, "Set up Rust 1.96.0");
assert.equal(yamlScalar(androidTestRust, "uses", 8), "dtolnay/rust-toolchain@1.96.0");
assert.equal(
  yamlScalar(yamlBlock(androidTestRust, "with", 8), "targets", 10),
  "aarch64-linux-android"
);
const androidTestSdk = yamlLiteral(yamlNamedStep(androidTestApk, "Install Android SDK packages"), "run");
assert.match(androidTestSdk, /"platforms;android-36" "platforms;android-35"/);
assert.match(androidTestSdk, /"build-tools;35\.0\.0" "ndk;26\.3\.11579264" "platform-tools"/);
assert.match(
  androidTestSdk,
  /for v in NDK_HOME ANDROID_NDK_HOME ANDROID_NDK_ROOT ANDROID_NDK_LATEST_HOME; do[\s\S]*?echo "\$v=\$NDK" >> "\$GITHUB_ENV"/
);
assert.match(androidTestSdk, /! -name "26\.3\.11579264" -exec rm -rf \{\} \+/);
assert.match(androidTestSdk, /rm -rf "\$ANDROID_SDK_ROOT\/ndk-bundle"/);
assert.equal(yamlScalar(yamlNamedStep(androidTestApk, "Install JS deps"), "run", 8), "npm ci");

const androidSigningCheck = yamlLiteral(yamlNamedStep(androidTestApk, "Require Android signing secrets"), "run");
for (const secret of [
  "ANDROID_KEYSTORE_BASE64",
  "ANDROID_KEYSTORE_PASSWORD",
  "ANDROID_KEY_ALIAS",
  "ANDROID_KEY_PASSWORD",
]) {
  assert.match(androidSigningCheck, new RegExp(`\\b${secret}\\b`), `${secret} is not fail-closed`);
}
assert.match(androidSigningCheck, /exit "\$missing"/);
const androidSigningConfig = yamlNamedStep(androidTestApk, "Write signing config from secrets");
const androidSigningEnv = yamlBlock(androidSigningConfig, "env", 8);
for (const secret of [
  "ANDROID_KEYSTORE_BASE64",
  "ANDROID_KEYSTORE_PASSWORD",
  "ANDROID_KEY_ALIAS",
  "ANDROID_KEY_PASSWORD",
]) {
  assert.match(
    androidSigningEnv.join("\n"),
    new RegExp(`secrets\\.${secret}`),
    `${secret} is not passed only to the signing-config step`
  );
}
const androidSigningScript = yamlLiteral(androidSigningConfig, "run");
assert.match(androidSigningScript, /base64 -d > "\$RUNNER_TEMP\/tine-test-apk\.jks"/);
assert.match(androidSigningScript, /src-tauri\/gen\/android\/keystore\.properties/);

const androidVersion = yamlLiteral(yamlNamedStep(androidTestApk, "Set Android test version"), "run");
assert.match(androidVersion, /short_sha="\$\{GITHUB_SHA:0:12\}"/);
assert.match(androidVersion, /version_name="0\.7\.0-sync-\$short_sha"/);
assert.match(androidVersion, /tauri\.android\.versionName=%s\\ntauri\.android\.versionCode=%s/);
assert.match(androidVersion, /"\$version_name" "6999"/);
const androidIdentifier = yamlLiteral(yamlNamedStep(androidTestApk, "Pin Android identifier to page.tine.app"), "run");
assert.match(androidIdentifier, /c\.identifier='page\.tine\.app'/);
const androidBuild = yamlLiteral(yamlNamedStep(androidTestApk, "Build signed Android test APK"), "run");
assert.match(androidBuild, /RUSTFLAGS="--remap-path-prefix=\$GITHUB_WORKSPACE=\/build --remap-path-prefix=\$HOME\/\.cargo=\/cargo"/);
assert.match(androidBuild, /npx tauri android build --target aarch64 --apk/);
const androidLoaderCheck = yamlLiteral(
  yamlNamedStep(androidTestApk, "Verify Android 9 native-loader compatibility"),
  "run"
);
assert.match(androidLoaderCheck, /unzip -p "\$apk" lib\/arm64-v8a\/libtine_lib\.so/);
assert.match(androidLoaderCheck, /renameat2/);

const androidUploads = androidTestApk.filter((line) => line.trim() === "uses: actions/upload-artifact@v4");
assert.equal(androidUploads.length, 1, "the test-APK job must upload only one artifact");
const androidUpload = yamlNamedStep(androidTestApk, "Upload signed Android test APK");
assert.equal(yamlScalar(yamlBlock(androidUpload, "with", 8), "name", 10), "tine-android-test-apk-${{ github.sha }}");
assert.equal(
  yamlScalar(yamlBlock(androidUpload, "with", 8), "path", 10),
  "Tine_${{ steps.android-version.outputs.version_name }}_android-arm64.apk"
);
assert.equal(yamlScalar(yamlBlock(androidUpload, "with", 8), "if-no-files-found", 10), "error");
assert.equal(yamlScalar(yamlBlock(androidUpload, "with", 8), "retention-days", 10), "3");
const androidCleanup = yamlNamedStep(androidTestApk, "Remove Android test signing material");
assert.equal(yamlScalar(androidCleanup, "if", 8), "always()");
const androidCleanupScript = yamlLiteral(androidCleanup, "run");
assert.match(androidCleanupScript, /tine-test-apk\.jks/);
assert.match(androidCleanupScript, /libtine_lib\.so/);
assert.match(androidCleanupScript, /src-tauri\/gen\/android\/keystore\.properties/);
assert.match(androidCleanupScript, /src-tauri\/gen\/android\/app\/tauri\.properties/);
assert.doesNotMatch(
  androidTestApk.join("\n"),
  /contents:\s*write|actions\/create-release|action-gh-release|gh release|git tag|git push|publish|deploy/i,
  "the test-APK lane must stay read-only and never release, tag, publish, or deploy"
);
assert.equal(
  yamlScalar(performanceBench, "if", 4),
  "github.event_name == 'workflow_dispatch' && (inputs.scope == 'full' || inputs.scope == 'performance')"
);
assert.doesNotMatch(flatpakWorkflow, /\n  push:/, "the expensive Flatpak build still runs automatically on pushes");
assert.match(flatpakMetadataWorkflow, /\n  pull_request:/, "lightweight Flatpak metadata validation is not on PRs");
assert.doesNotMatch(flatpakMetadataWorkflow, /\n  push:/, "Flatpak metadata validation still runs after merge");
assert.match(
  releaseWorkflow,
  /permissions:[\s\S]*?contents: read[\s\S]*?actions: read[\s\S]*?preflight:[\s\S]*?name: Require exact-SHA full CI evidence[\s\S]*?node scripts\/check-ci-evidence\.mjs[\s\S]*?uses: dtolnay\/rust-toolchain/,
  "release packaging does not fail closed on exact-SHA full CI evidence before expensive setup"
);

assert.equal(
  selectExactCiEvidence(commit, [{ run: successfulFullCiRun, jobs: successfulFullCiJobs }]).run.id,
  successfulFullCiRun.id
);
assert.throws(
  () => selectExactCiEvidence("b".repeat(40), [{ run: successfulFullCiRun, jobs: successfulFullCiJobs }]),
  /No successful full CI evidence for exact SHA/
);
assert.throws(
  () => selectExactCiEvidence(commit, [{
    run: { ...successfulFullCiRun, event: "pull_request" },
    jobs: successfulFullCiJobs,
  }]),
  /run event is pull_request, not workflow_dispatch/
);
assert.throws(
  () => selectExactCiEvidence(commit, [{ run: successfulFullCiRun, jobs: successfulFullCiJobs.slice(0, 1) }]),
  /Full CI \/ Windows compile \+ storage smoke \+ core smoke concluded missing/
);
assert.throws(
  () => selectExactCiEvidence(commit, [{
    run: successfulFullCiRun,
    jobs: successfulFullCiJobs.filter((job) => job.name !== "Full CI / Linux tine-core nextest shard 4/4"),
  }]),
  /Full CI \/ Linux tine-core nextest shard 4\/4 concluded missing/
);
assert.throws(
  () => selectExactCiEvidence(commit, [{
    run: successfulFullCiRun,
    jobs: successfulFullCiJobs.map((job) => ({
      ...job,
      conclusion: job.name === "Full CI / performance A/B" ? "failure" : job.conclusion,
    })),
  }]),
  /Full CI \/ performance A\/B concluded failure/
);
const paginationCalls = [];
assert.deepEqual(
  await collectGithubPages(async (page) => {
    paginationCalls.push(page);
    return { jobs: page === 1 ? [{ id: 1 }, { id: 2 }] : [{ id: 3 }] };
  }, "jobs", { perPage: 2 }),
  [{ id: 1 }, { id: 2 }, { id: 3 }]
);
assert.deepEqual(paginationCalls, [1, 2], "GitHub pagination did not stop after the short final page");

const linuxGate = releaseWorkflow.indexOf("Gate Linux x64 on the complete real-app regression catalog");
const stageLane = releaseWorkflow.indexOf("Stage immutable release artifact");
assert(linuxGate >= 0, "release workflow is missing the Linux real-app gate");
assert(stageLane > linuxGate, "release lane is staged before the Linux real-app gate");
assert.match(
  receiptHelper,
  /buildInputState[\s\S]*?refusing receipt: HEAD changed while building[\s\S]*?build-input state changed while building[\s\S]*?binary does not embed current production frontend[\s\S]*?buildInputDigest/,
  "the receipt helper does not bind a build to its pre-build source state and embedded frontend"
);
assert.match(buildInputs, /export function buildInputState\(/, "buildInputState is not exported");
assert.match(buildInputs, /ls-files[\s\S]*?digest/, "build-input state is not bound to git ls-files and a digest");
assert.match(
  e2eRunner,
  /buildInputState[\s\S]*?const e2eMode = process\.env\.TINE_E2E_MODE \?\? "ordinary";[\s\S]*?buildInputDigest[\s\S]*?build receipt is required at/,
  "run-e2e does not default to ordinary mode and require a receipt"
);
assert.doesNotMatch(e2eRunner, /GITHUB_SHA|TINE_E2E_ALLOW_UNRECEIPTED_APP/);
assert.match(
  e2eRunner,
  /if \(e2eMode === "release"\) \{[\s\S]*?contract\.class !== "flexible-presentation-heuristic"/,
  "release mode does not block every safety, core-operation, and stateful-UX failure"
);
assert.match(
  uiE2eWorkflow,
  /Snapshot Linux E2E candidate inputs[\s\S]*?Write Linux E2E candidate receipt[\s\S]*?Snapshot Windows E2E candidate inputs[\s\S]*?Write Windows E2E candidate receipt/,
  "manually dispatched raw Linux and Windows builds do not create receipts"
);
assert.match(
  releaseWorkflow,
  /Snapshot Linux E2E candidate inputs[\s\S]*?Write Linux E2E candidate receipt[\s\S]*?TINE_E2E_MODE: release[\s\S]*?npm run e2e:linux:release/,
  "the release Linux E2E candidate does not use a pre-build receipt or release mode"
);
assert.match(
  releaseWorkflow,
  /Snapshot Windows E2E candidate inputs[\s\S]*?--tauri-manifest-normalization[\s\S]*?Write Windows E2E candidate receipt[\s\S]*?release-e2e-receipt-windows-x64[\s\S]*?TINE_E2E_BUILD_RECEIPT=[\s\S]*?TINE_E2E_MODE: release/,
  "the advisory release Windows E2E run does not normalize the exact Tauri manifest before receiving its receipt in release mode"
);
assert.match(
  releaseWorkflow,
  /windows-smoke:\n    needs: \[preflight, build\][\s\S]*?if: \$\{\{ always\(\) && needs\.preflight\.result == 'success' && needs\.build\.result != 'cancelled' \}\}[\s\S]*?continue-on-error: true[\s\S]*?name: release-windows-x64[\s\S]*?name: release-e2e-frontend-windows-x64[\s\S]*?npm run e2e:windows:smoke -- --scenario=\$\{\{ matrix\.scenario \}\}/,
  "Windows advisory scenarios do not consume the staged app independently of assembly"
);
assert.match(
  uiE2eWorkflow,
  /windows_scenario == 'all'[\s\S]*?\["windows-core","page-properties","page-trailing-block","pdf-logseq","print-security","tab-overflow"\]/,
  "the focused UI workflow cannot fan out all Windows scenarios explicitly"
);
assert.doesNotMatch(
  uiE2eWorkflow,
  /name: Run Windows WebView2 smoke\n\s+continue-on-error:/,
  "the focused Windows workflow hides a 0\/N scenario result behind a green job"
);
assert.match(
  uiE2eWorkflow,
  /Install Edge WebDriver matching the WebView2 runtime[\s\S]*?\.\/scripts\/install-windows-webview2-driver\.ps1/,
  "focused Windows UI CI does not select EdgeDriver from the actual WebView2 runtime"
);
assert.match(
  releaseWorkflow,
  /Install Edge WebDriver matching the WebView2 runtime[\s\S]*?\.\/scripts\/install-windows-webview2-driver\.ps1/,
  "release Windows UI CI does not select EdgeDriver from the actual WebView2 runtime"
);
assert.match(windowsWebviewDriverInstaller, /Microsoft\\EdgeWebView\\Application/);
assert.match(windowsWebviewDriverInstaller, /msedgewebview2\.exe/);
assert.doesNotMatch(
  windowsWebviewDriverInstaller,
  /Microsoft\\Edge\\Application\\msedge\.exe/,
  "the WebView2 driver installer must not infer its version from the independently updated desktop browser"
);
assert.match(
  releaseWorkflow,
  /name: Upload exact Windows x64 frontend proof[\s\S]*?if: matrix\.lane == 'windows-x64'[\s\S]*?name: release-e2e-frontend-windows-x64[\s\S]*?path: dist/,
  "the release build does not preserve the exact frontend needed to validate the staged Windows executable"
);
assert.match(
  releaseWorkflow,
  /assemble:\n    needs: \[preflight, flatpak, build, android\]/,
  "candidate assembly accidentally waits for advisory Windows scenarios"
);
assert.match(releaseWorkflow, /name: Upload Windows E2E evidence[\s\S]*?if: always\(\)/);
assert.match(
  e2eRunner,
  /if \(process\.platform === "linux"\) \{\n      env\.WEBKIT_DRIVER = process\.env\.WEBKIT_DRIVER \|\| "\/usr\/bin\/WebKitWebDriver";/,
  "the suite runner leaks Linux WebKitWebDriver into Windows"
);
assert.match(
  e2eRunner,
  /TAURI_DRIVER: process\.env\.TAURI_DRIVER \|\| \(process\.platform === "win32" \? "msedgedriver\.exe" : "tauri-driver"\)/,
  "Windows scenarios still route native WebView2 through the unnecessary Tauri proxy"
);
const driverTransportFailureSource = e2eRunner.match(
  /function isRetryableDriverTransportFailure\(output, errors, timedOut\) \{[\s\S]*?\n\}/
);
assert.ok(driverTransportFailureSource, "the release runner is missing its WebDriver transport retry predicate");
const isRetryableDriverTransportFailure = new Function(
  `${driverTransportFailureSource[0]}\nreturn isRetryableDriverTransportFailure;`
)();
assert.equal(
  isRetryableDriverTransportFailure(
    'WebDriverError: invalid session id when running\n"element/.../property/value" with method "GET"\nError: Arrow Down did not cross from the page header into the first body block',
    "",
    false
  ),
  true,
  "the hosted terminal WebDriver invalid-session failure is not retried"
);
assert.equal(
  isRetryableDriverTransportFailure("WebDriverError: GET /session failed: UND_ERR_SOCKET", "", false),
  true,
  "existing WebDriver socket transport failures are not retried"
);
assert.equal(
  isRetryableDriverTransportFailure("Arrow Down assertion failed: invalid session id", "", false), false,
  "generic invalid-session text without a WebDriver error must not be retried"
);
assert.equal(
  isRetryableDriverTransportFailure("WebDriverError: element assertion failed", "", false), false,
  "arbitrary WebDriver assertion failures must not be retried"
);
assert.equal(
  isRetryableDriverTransportFailure("Arrow Down did not cross from the page header into the first body block", "", false),
  false,
  "product assertion failures without a WebDriver error must not be retried"
);
assert.equal(
  isRetryableDriverTransportFailure("WebDriverError: invalid session id", "", true), false,
  "scenario timeouts must not be retried as driver infrastructure failures"
);
const nativeHarnessFailureSource = e2eRunner.match(
  /function isRetryableNativeHarnessFailure\(id, output, errors, timedOut\) \{[\s\S]*?\n\}/
);
assert.ok(nativeHarnessFailureSource, "the release runner is missing its Quick Capture native-harness retry predicate");
const isRetryableNativeHarnessFailure = new Function(
  `${nativeHarnessFailureSource[0]}\nreturn isRetryableNativeHarnessFailure;`
)();
assert.equal(
  isRetryableNativeHarnessFailure(
    "capture",
    "BadWindow (invalid Window parameter)\nxdo_get_active_window reported an error",
    "",
    false
  ),
  true,
  "the legacy GTK BadWindow active-window race is not retried"
);
assert.equal(
  isRetryableNativeHarnessFailure(
    "capture",
    "XGetWindowProperty[_NET_ACTIVE_WINDOW] failed (code=1)\nxdo_get_active_window reported an error",
    "",
    false
  ),
  true,
  "the demonstrated xdotool active-window race is not retried"
);
assert.equal(
  isRetryableNativeHarnessFailure("capture", "cold-restart autocomplete assertion failed", "", false),
  false,
  "arbitrary Quick Capture assertion failures must not be retried"
);
assert.match(
  printSecurity,
  /const driverArgs = webdriverServerArgs\([\s\S]*?DRIVER_PORT,[\s\S]*?NATIVE_PORT,[\s\S]*?WEBKIT_DRIVER/,
  "print-security does not select the native WebDriver by platform"
);
assert.match(
  referenceParity,
  /APP_DATA_ROOT = process\.platform === "win32"[\s\S]*?APPDATA: APP_DATA_ROOT,[\s\S]*?LOCALAPPDATA:/,
  "reference parity does not isolate and seed Windows app settings"
);
assert.match(
  e2eRunner,
  /\["og-parity-references", "scripts\/e2e-og-parity-references\.mjs"[\s\S]*?\["capture", "scripts\/e2e-capture\.mjs"/,
  "the release suite does not retain independent reference and Quick Capture proofs"
);
assert.doesNotMatch(
  referenceParity,
  /scripts\/e2e-capture\.mjs/,
  "reference parity nests the independent native Quick Capture process tree"
);
assert.match(
  ciWorkflow,
  /name: Performance baseline policy is current[\s\S]*?releases\/latest[\s\S]*?node scripts\/check-bench-policy\.mjs --expected-previous "\$latest"/,
  "ordinary CI does not compare the performance baseline with the actually published release"
);
assert.match(
  ciWorkflow,
  /bench:[\s\S]*?fetch-depth: 0[\s\S]*?name: Require the rolling baseline to be the latest published release[\s\S]*?releases\/latest[\s\S]*?node scripts\/check-bench-policy\.mjs --expected-previous "\$latest"/,
  "the A/B benchmark job does not validate baseline currency against the published release before measuring"
);
assert.match(
  ciWorkflow,
  /bench:[\s\S]*?node scripts\/bench-ab\.mjs[\s\S]*?--candidate-dir \.[\s\S]*?--immutable-dir \.bench\/immutable[\s\S]*?--previous-dir \.bench\/previous/,
  "the A/B benchmark job does not measure all three versions through the interleaved multi-round harness"
);
assert.match(
  ciWorkflow,
  /name: Performance A\/B multi-round reliability fixtures[\s\S]*?node scripts\/test-bench-ab\.mjs/,
  "ordinary CI does not prove the performance gate rejects metric-level variance"
);
assert.match(
  releaseWorkflow,
  /preflight:[\s\S]*?fetch-depth: 0/,
  "release preflight cannot determine the previous release from a shallow checkout"
);
assert.match(preflight, /check-bench-policy\.mjs/, "release preflight omits the performance-baseline currency guard");

function makeInput(base) {
  const input = path.join(base, "input");
  fs.mkdirSync(input, { recursive: true });
  for (const lane of RELEASE_LANES) {
    const directory = path.join(input, `release-${lane}`);
    fs.mkdirSync(directory, { recursive: true });
    const assets = [];
    for (const name of layout.lanes[lane].assets) {
      const contents = name.endsWith(".sig") ? `signature-${name}\n` : `fixture-${name}\n`;
      fs.writeFileSync(path.join(directory, name), contents);
      const bytes = Buffer.from(contents);
      assets.push({ name, size: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex") });
    }
    const platforms = {};
    for (const [platform, [asset, signatureAsset]] of Object.entries(layout.lanes[lane].platforms)) {
      platforms[platform] = {
        asset,
        signature: fs.readFileSync(path.join(directory, signatureAsset), "utf8").trim(),
      };
    }
    fs.writeFileSync(
      path.join(directory, "release-fragment.json"),
      `${JSON.stringify({ version, commit, lane, assets, platforms }, null, 2)}\n`
    );
  }
  return input;
}

function assemble(input, output) {
  assembleCandidate({
    input,
    output,
    version,
    commit,
    repository,
    pubDate: "2026-07-11T00:00:00.000Z",
  });
}

const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "tine-release-pipeline-test-"));
try {
  const priorWebviewRoot = process.env.E2E_WEBVIEW_USER_DATA_ROOT;
  process.env.E2E_WEBVIEW_USER_DATA_ROOT = path.join(temporary, "webview2");
  const windowsCapabilities = tauriCapabilities("C:/Tine.exe", "fixture session", "win32");
  assert.equal(
    windowsCapabilities["ms:edgeOptions"].webviewOptions.userDataFolder,
    path.join(temporary, "webview2", "fixture-session"),
  );
  assert.equal(windowsCapabilities.browserName, "webview2");
  assert.equal(windowsCapabilities["ms:edgeOptions"].binary, "C:/Tine.exe");
  const attachedCapabilities = tauriCapabilities(
    "C:/Tine.exe",
    "fixture session",
    "win32",
    "127.0.0.1:9222",
  );
  assert.equal(attachedCapabilities["ms:edgeOptions"].debuggerAddress, "127.0.0.1:9222");
  assert.equal(attachedCapabilities["ms:edgeOptions"].binary, undefined);
  assert.deepEqual(webdriverServerArgs(4444, 4445, "/driver", "win32"), ["--port=4444"]);
  assert.deepEqual(webdriverServerArgs(4444, 4445, "/driver", "linux"), [
    "--port", "4444", "--native-port", "4445", "--native-driver", "/driver",
  ]);
  const nestedPort = path.join(temporary, "webview2", "fixture-session", "EBWebView", "DevToolsActivePort");
  fs.mkdirSync(path.dirname(nestedPort), { recursive: true });
  fs.writeFileSync(nestedPort, "12345\n/devtools/browser/fixture\n");
  const profileSnapshot = windowsWebviewProfileSnapshot(path.join(temporary, "webview2"));
  assert.ok(profileSnapshot.files.some((entry) => entry.path === "fixture-session/EBWebView/DevToolsActivePort"));
  if (priorWebviewRoot === undefined) delete process.env.E2E_WEBVIEW_USER_DATA_ROOT;
  else process.env.E2E_WEBVIEW_USER_DATA_ROOT = priorWebviewRoot;
  for (const script of windowsScenarios) {
    const source = fs.readFileSync(path.join(process.cwd(), "scripts", script), "utf8");
    assert.match(source, /import \{[^}]*tauriCapabilities[^}]*\} from "\.\/e2e-capabilities\.mjs";/);
    assert.match(source, /startWebdriverApplication\(APP,/);
    assert.match(source, /capabilities: tauriCapabilities\(APP,[^\n]*webviewTarget\.debuggerAddress/);
    assert.match(source, /stopWebdriverApplication\(webviewTarget\)/);
  }

  {
    const base = path.join(temporary, "valid");
    const input = makeInput(base);
    const output = path.join(base, "output");
    assemble(input, output);
    assert.deepEqual(candidateProblems(output, version), []);
  }
  {
    const base = path.join(temporary, "missing-android");
    const input = makeInput(base);
    fs.rmSync(path.join(input, "release-android"), { recursive: true });
    assert.throws(() => assemble(input, path.join(base, "output")), /missing release lanes: android/);
  }
  {
    const base = path.join(temporary, "missing-signature");
    const input = makeInput(base);
    fs.rmSync(path.join(input, "release-windows-x64", `Tine_${version}_x64-setup.exe.sig`));
    assert.throws(() => assemble(input, path.join(base, "output")), /ENOENT/);
  }
  {
    const base = path.join(temporary, "wrong-version");
    const input = makeInput(base);
    const fragmentPath = path.join(input, "release-macos-universal", "release-fragment.json");
    const fragment = JSON.parse(fs.readFileSync(fragmentPath, "utf8"));
    fragment.version = "0.5.7";
    fs.writeFileSync(fragmentPath, JSON.stringify(fragment));
    assert.throws(() => assemble(input, path.join(base, "output")), /version 0\.5\.7, expected 0\.5\.6/);
  }
  {
    const base = path.join(temporary, "duplicate-platform");
    const input = makeInput(base);
    const fragmentPath = path.join(input, "release-windows-x64", "release-fragment.json");
    const fragment = JSON.parse(fs.readFileSync(fragmentPath, "utf8"));
    fragment.platforms["linux-x86_64"] = fragment.platforms["windows-x86_64"];
    fs.writeFileSync(fragmentPath, JSON.stringify(fragment));
    assert.throws(() => assemble(input, path.join(base, "output")), /updater platform contract mismatch/);
  }
  {
    const base = path.join(temporary, "incomplete-updater");
    const input = makeInput(base);
    const output = path.join(base, "output");
    assemble(input, output);
    const updaterPath = path.join(output, "latest.json");
    const updater = JSON.parse(fs.readFileSync(updaterPath, "utf8"));
    delete updater.platforms["windows-aarch64"];
    fs.writeFileSync(updaterPath, JSON.stringify(updater));
    assert(candidateProblems(output, version).some((problem) => problem.includes("windows-aarch64")));
  }
} finally {
  fs.rmSync(temporary, { recursive: true, force: true });
}

console.log("Release pipeline fixture tests passed (exact-SHA CI gate + release workflow + fail-closed cases).");
