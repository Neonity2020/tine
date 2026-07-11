#!/usr/bin/env node

// Verify the exact public/draft release payload before the workflow flips the
// draft to published. Uses authenticated gh so it can inspect draft releases.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const tag = process.argv[2];
if (!/^v\d+\.\d+\.\d+$/.test(tag ?? "")) {
  console.error("usage: check-release-assets.mjs vX.Y.Z");
  process.exit(2);
}
const version = tag.slice(1);
const gh = (...args) =>
  execFileSync("gh", args, { encoding: "utf8", stdio: ["ignore", "pipe", "inherit"] });

const release = JSON.parse(gh("release", "view", tag, "--json", "assets,tagName"));
const names = new Set(release.assets.map((asset) => asset.name));
const expected = [
  "latest.json",
  `Tine_${version}_android-arm64.apk`,
  `Tine_${version}_universal.dmg`,
  `Tine_${version}_x64-setup.exe`,
  `Tine_${version}_x64-setup.exe.sig`,
  `Tine_${version}_x64-portable.zip`,
  `Tine_${version}_arm64-setup.exe`,
  `Tine_${version}_arm64-setup.exe.sig`,
  `Tine_${version}_arm64-portable.zip`,
  `Tine_${version}_amd64.AppImage`,
  `Tine_${version}_amd64.AppImage.sig`,
  `Tine_${version}_amd64.deb`,
  `Tine_${version}_amd64.deb.sig`,
  `Tine-${version}-1.x86_64.rpm`,
  `Tine-${version}-1.x86_64.rpm.sig`,
  `Tine_${version}_aarch64.AppImage`,
  `Tine_${version}_aarch64.AppImage.sig`,
  `Tine_${version}_arm64.deb`,
  `Tine_${version}_arm64.deb.sig`,
  `Tine-${version}-1.aarch64.rpm`,
  `Tine-${version}-1.aarch64.rpm.sig`,
];
const missing = expected.filter((name) => !names.has(name));
const unexpected = [...names].filter((name) => !expected.includes(name)).sort();
const problems = [];
if (release.tagName !== tag) problems.push(`release tag is ${release.tagName}, expected ${tag}`);
if (missing.length) problems.push(`missing assets: ${missing.join(", ")}`);
if (unexpected.length) problems.push(`unexpected assets: ${unexpected.join(", ")}`);

const temp = fs.mkdtempSync(path.join(os.tmpdir(), `tine-release-${version}-`));
try {
  gh("release", "download", tag, "--pattern", "latest.json", "--dir", temp, "--clobber");
  const updater = JSON.parse(fs.readFileSync(path.join(temp, "latest.json"), "utf8"));
  const expectedPlatforms = [
    "linux-x86_64",
    "linux-x86_64-appimage",
    "linux-x86_64-deb",
    "linux-x86_64-rpm",
    "linux-aarch64",
    "linux-aarch64-appimage",
    "linux-aarch64-deb",
    "linux-aarch64-rpm",
    "windows-x86_64",
    "windows-x86_64-nsis",
    "windows-aarch64",
    "windows-aarch64-nsis",
  ];
  const actualPlatforms = Object.keys(updater.platforms ?? {}).sort();
  const missingPlatforms = expectedPlatforms.filter((platform) => !actualPlatforms.includes(platform));
  const unexpectedPlatforms = actualPlatforms.filter(
    (platform) => !expectedPlatforms.includes(platform)
  );
  if (updater.version !== version) {
    problems.push(`latest.json version is ${updater.version}, expected ${version}`);
  }
  if (missingPlatforms.length) {
    problems.push(`latest.json missing platforms: ${missingPlatforms.join(", ")}`);
  }
  if (unexpectedPlatforms.length) {
    problems.push(`latest.json has unexpected platforms: ${unexpectedPlatforms.join(", ")}`);
  }
  for (const platform of expectedPlatforms) {
    const entry = updater.platforms?.[platform];
    if (entry && (!entry.url?.includes(`/Tine_`) && !entry.url?.includes(`/Tine-`))) {
      problems.push(`latest.json ${platform} has an invalid URL`);
    }
    if (entry && typeof entry.signature !== "string") {
      problems.push(`latest.json ${platform} has no signature`);
    }
  }
} finally {
  fs.rmSync(temp, { recursive: true, force: true });
}

if (problems.length) {
  console.error(`Release asset verification failed (${problems.length} problem(s)):`);
  for (const problem of problems) console.error(`  ${problem}`);
  process.exit(1);
}

console.log(`Release assets OK: ${tag}, ${expected.length} assets, 12 updater platforms.`);
