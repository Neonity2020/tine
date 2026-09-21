#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { filesUnder, validateGuideSiteLinks, validateLiveGuide } from "./guide-site-validator.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const checkedIn = path.join(root, "website/guide");
const check = process.argv.includes("--check");
const temp = check ? fs.mkdtempSync(path.join(os.tmpdir(), "tine-guide-site-")) : null;
const output = check ? path.join(temp, "guide") : checkedIn;
const snapshotRelative = path.join("app", "snapshot.json");
const previousSnapshot = !check && fs.existsSync(path.join(checkedIn, snapshotRelative))
  ? fs.readFileSync(path.join(checkedIn, snapshotRelative))
  : null;

function comparable(relative, bytes) {
  if (relative !== snapshotRelative) return bytes;
  const snapshot = JSON.parse(bytes.toString("utf8"));
  snapshot.exported_at = "<generated>";
  return Buffer.from(JSON.stringify(snapshot));
}

// The desktop build time deliberately changes on every build. The Guide is a
// checked-in reproducible artifact, so use the conventional reproducible-build
// epoch; snapshot.json separately records when the Guide content was exported.
const frontend = spawnSync("npm", ["run", "build"], {
  cwd: root,
  stdio: "inherit",
  env: { ...process.env, SOURCE_DATE_EPOCH: "0", TINE_BUILD_COMMIT: "public-guide" },
});
if (frontend.status !== 0) process.exit(frontend.status ?? 1);

const built = spawnSync(
  "cargo",
  ["run", "--quiet", "-p", "tine-core", "--example", "build-guide-site", "--", output, path.join(root, "dist")],
  { cwd: root, stdio: "inherit" },
);
if (built.status !== 0) process.exit(built.status ?? 1);

try {
  validateGuideSiteLinks(output);
  validateLiveGuide(output);
  if (check) {
    const expected = filesUnder(checkedIn);
    const actual = filesUnder(output);
    const names = new Set([...expected, ...actual]);
    const stale = [];
    for (const relative of [...names].sort()) {
      const left = path.join(checkedIn, relative);
      const right = path.join(output, relative);
      if (!fs.existsSync(left)) stale.push(`missing from website/guide: ${relative}`);
      else if (!fs.existsSync(right)) stale.push(`extra in website/guide: ${relative}`);
      else if (!comparable(relative, fs.readFileSync(left)).equals(comparable(relative, fs.readFileSync(right)))) {
        stale.push(`content differs: ${relative}`);
      }
    }
    if (stale.length) throw new Error(`website/guide is stale; run npm run docs:build\n${stale.join("\n")}`);
    console.log(`Guide OK: ${actual.length} generated files match website/guide`);
  } else {
    if (previousSnapshot) {
      const generated = fs.readFileSync(path.join(output, snapshotRelative));
      if (comparable(snapshotRelative, previousSnapshot).equals(comparable(snapshotRelative, generated))) {
        fs.writeFileSync(path.join(output, snapshotRelative), previousSnapshot);
      }
    }
    console.log(`Guide rebuilt: ${filesUnder(output).length} files`);
  }
} finally {
  if (temp) fs.rmSync(temp, { recursive: true, force: true });
}
