#!/usr/bin/env node
// `tine-storage`'s `test-support` feature carries corruption and fault-injection
// seams. They must never reach a shipped binary.
//
// Today that holds for a reason that is easy to break by accident: the feature is
// requested only from `tine-core`'s `[dev-dependencies]`, and the workspace sets
// `resolver = "2"`, which stops dev-dependency features from unifying into a
// normal build. Move the dependency, drop the resolver, or add a second consumer,
// and the seams silently ship — with nothing failing.
//
// So this checks the resolved feature graph, not the manifests: whatever the
// reason, `test-support` must be absent from the app's graph. Manifest checks
// follow as a second, more legible signal.
//
// Usage: node scripts/check-storage-test-support.mjs

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const failures = [];

function featureTree(pkg) {
  return execFileSync(
    "cargo",
    ["tree", "--edges", "features", "--invert", "tine-storage", "--package", pkg],
    { cwd: repoRoot, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
  );
}

// 1. The app. `-p tine` resolves exactly what a release build compiles, so an
//    appearance here is the shipping failure itself rather than a proxy for it.
let appTree;
try {
  appTree = featureTree("tine");
} catch (error) {
  console.error("cargo tree failed for the app package `tine`:");
  console.error(error.stderr?.toString() ?? error.message);
  process.exit(1);
}

if (/feature "test-support"/.test(appTree)) {
  failures.push(
    "tine-storage's `test-support` feature is enabled in the `tine` app feature graph.\n" +
      "Corruption and fault-injection seams would be compiled into the shipped binary.\n" +
      "Resolved graph:\n" +
      appTree,
  );
}

// Guard the guard: if `cargo tree` ever stops resolving this dependency, the
// absence above would be vacuous rather than meaningful.
if (!/^tine-storage v/m.test(appTree)) {
  failures.push(
    "cargo tree did not report tine-storage in the app graph; this check would pass vacuously.",
  );
}

// 2. tine-core. The feature is legitimate here, but only from dev-dependencies.
//    `cargo tree` prints a `[dev-dependencies]` marker line before the dependents
//    reached that way, so a `test-support` consumer that is NOT preceded by one is
//    a normal-dependency consumer.
let coreTree;
try {
  coreTree = featureTree("tine-core");
} catch (error) {
  console.error("cargo tree failed for `tine-core`:");
  console.error(error.stderr?.toString() ?? error.message);
  process.exit(1);
}

const lines = coreTree.split("\n");
const featureLine = lines.findIndex((line) => /feature "test-support"/.test(line));
if (featureLine !== -1) {
  const dependents = [];
  for (let i = featureLine + 1; i < lines.length; i += 1) {
    if (/feature "/.test(lines[i])) break;
    if (lines[i].trim()) dependents.push(lines[i]);
  }
  if (!dependents.some((line) => line.includes("[dev-dependencies]"))) {
    failures.push(
      "tine-storage's `test-support` is reached from tine-core through a normal dependency,\n" +
        "not `[dev-dependencies]`. Under any resolver that unifies features, it would then\n" +
        "be enabled for production builds of tine-core.\n" +
        "Resolved graph:\n" +
        coreTree,
    );
  }
}

// 3. The workspace resolver, which is the mechanism the separation rests on.
const workspaceManifest = readFileSync(join(repoRoot, "Cargo.toml"), "utf8");
if (!/^\s*resolver\s*=\s*"2"/m.test(workspaceManifest)) {
  failures.push(
    'Cargo.toml no longer sets `resolver = "2"`. Resolver 1 unifies dev-dependency\n' +
      "features into normal builds, which is exactly what keeps `test-support` out of\n" +
      "the release binary today.",
  );
}

if (failures.length > 0) {
  console.error("storage test-support boundary check FAILED\n");
  for (const failure of failures) console.error(`- ${failure}\n`);
  process.exit(1);
}

console.log(
  "storage test-support boundary OK: absent from the `tine` app feature graph, " +
    'dev-dependency-only in tine-core, workspace resolver = "2"',
);
