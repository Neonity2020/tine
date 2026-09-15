import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { modelModuleFiles, modelModuleSource } from "./rustModelSource.test-helpers";

// K3 (2026-09-15) split the Rust model module across model.rs and model/*.rs.
const RULE =
  "I-11: read the Rust model module whole, never model.rs alone. K3 moved most of it into " +
  "crates/tine-core/src/model/*.rs, which a model.rs-only scan cannot see: an absence guard " +
  "passes vacuously and a presence guard breaks on the next cut. Read it through " +
  "test_support::model_module_source (in-crate; exemplar: journal_feed.rs, " +
  "direct_files_journals_desc_uses_this_files_dedup), production_source::model_module_files " +
  "(integration tests; exemplar: public_query_executor_census.rs) or modelModuleSource from " +
  "src/rustModelSource.test-helpers.ts (exemplar: livingContracts.contract.test.ts).";

// The helpers are where the whole-module read lives, and this file spells the patterns.
const EXEMPT = new Set([
  "crates/tine-core/src/test_support.rs",
  "crates/tine-core/tests/support/production_source.rs",
  "src/rustModelSource.test-helpers.ts",
  "src/rustModelSourceGuard.test.ts",
]);

// A whole-file read of model.rs: `include_str!`, or a read/compile call whose
// argument names the file. Tables that merely NAME the path (the size ratchet,
// the print-site and primitive censuses) are not reads and are not matched.
const SOLO_READS = [
  /include_str!\(\s*"(?:\.\.\/src\/)?model\.rs"\s*\)/,
  /(?:readFileSync|source|compiled_source|read_to_string)\([^)]*crates\/tine-core\/src\/model\.rs"/,
];

function filesUnder(dir: string, name: RegExp): string[] {
  return readdirSync(join(process.cwd(), dir))
    .sort()
    .flatMap((entry) => {
      const path = `${dir}/${entry}`;
      if (entry === "node_modules" || entry === "target") return [];
      if (statSync(join(process.cwd(), path)).isDirectory()) return filesUnder(path, name);
      return name.test(entry) ? [path] : [];
    });
}

describe("the Rust model module is read whole", () => {
  it("lists model.rs and every seam file, and sees code that moved out of model.rs", () => {
    const files = modelModuleFiles();
    expect(files[0]).toBe("crates/tine-core/src/model.rs");
    expect(files.length).toBeGreaterThan(1);
    // K3.4 moved `journals_desc` to model/journals.rs.
    expect(readFileSync(join(process.cwd(), files[0]), "utf8")).not.toContain(
      "pub fn journals_desc(&self)",
    );
    expect(modelModuleSource()).toContain("pub fn journals_desc(&self)");
  });

  it("no source guard reads model.rs alone", () => {
    const scanned = [
      ...filesUnder("crates", /\.rs$/),
      ...filesUnder("src-tauri/src", /\.rs$/),
      ...filesUnder("src", /\.tsx?$/),
    ].filter((path) => !EXEMPT.has(path));
    const offenders = scanned.flatMap((path) =>
      readFileSync(join(process.cwd(), path), "utf8")
        .split("\n")
        .flatMap((line, index) =>
          SOLO_READS.some((pattern) => pattern.test(line)) ? [`${path}:${index + 1}`] : [],
        ),
    );
    expect(offenders, RULE).toEqual([]);
  });
});
