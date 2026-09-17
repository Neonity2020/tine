#!/usr/bin/env node
// Undefined identifiers in the E2E harness, caught at lint time.
//
// `backendName is not defined` failed a blocking release journey 100% of the
// time for a day while looking like a test failure: a code path had been
// deleted and left three dangling references behind. Node only discovers that
// when the line runs, which in a native journey is minutes in, inside a driver,
// behind an Xvfb display -- so it arrived as a red release gate rather than as
// a typo.
//
// `scripts/**` is untyped JavaScript and will never be free of type errors, so
// this deliberately reports ONE diagnostic class: TS2304, "Cannot find name".
// That is exactly the deleted-path shape, it needs no annotations, and it
// cannot be argued with.
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

export function undefinedNameDiagnostics(output) {
  return output
    .split("\n")
    .filter((line) => /error TS2304:/.test(line))
    .map((line) => line.trim());
}

function scriptFiles() {
  const files = [];
  for (const dir of [path.join(root, "scripts"), path.join(root, "scripts", "lib")]) {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      if (entry.isFile() && entry.name.endsWith(".mjs")) files.push(path.join(dir, entry.name));
    }
  }
  return files.sort();
}

function main() {
  const files = scriptFiles();
  const tsc = spawnSync(
    process.execPath,
    [
      path.join(root, "node_modules", "typescript", "bin", "tsc"),
      "--noEmit", "--allowJs", "--checkJs",
      "--target", "es2022", "--module", "esnext", "--moduleResolution", "bundler",
      "--skipLibCheck", "--lib", "es2022,dom",
      ...files,
    ],
    { cwd: root, encoding: "utf8" },
  );
  const problems = undefinedNameDiagnostics(`${tsc.stdout ?? ""}\n${tsc.stderr ?? ""}`);
  if (problems.length > 0) {
    console.error(`Harness scripts name ${problems.length} identifier(s) that do not exist:`);
    for (const problem of problems) console.error(`  ${problem}`);
    console.error(
      "\nThis is the shape that made a blocking release journey fail 100% for a day looking"
      + " like a test failure: a deleted code path left its references behind.",
    );
    process.exit(1);
  }
  console.log(`Harness script identifiers OK: ${files.length} files, no undefined names.`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
