import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, normalize } from "node:path";
import { describe, expect, it } from "vitest";
import {
  modelModuleFiles,
  modelModuleSource,
  rustModuleFiles,
  rustModuleSource,
} from "./rustModelSource.test-helpers";

// K3 (2026-09-15) split the Rust model module across model.rs and model/*.rs.
// K7 split query, publish, watcher and commands the same way.
const RULE =
  "I-11: read a split Rust module whole, never its root file alone. Code that moved from X.rs " +
  "into X/*.rs is invisible to an X.rs-only scan: an absence guard passes vacuously, and a " +
  "presence guard breaks on the next cut. Read the module through one of these: " +
  "test_support::model_module_source or projection_producer_census::production_rust " +
  "(in-crate; exemplar: publish_tests.rs, publish_module_production); " +
  "production_source::module_source (integration tests; exemplar: " +
  "public_query_executor_census.rs); src-tauri's test_support::rust_module_production_source " +
  "(exemplar: the commands.rs and watcher.rs tests); or rustModuleSource from " +
  "src/rustModelSource.test-helpers.ts (exemplar: hostileContent.guard.test.ts).";

// The helpers are where the whole-module read lives, and this file spells the patterns.
const EXEMPT = new Set([
  "crates/tine-core/src/test_support.rs",
  "crates/tine-core/tests/support/production_source.rs",
  "src-tauri/src/test_support.rs",
  "src/rustModelSource.test-helpers.ts",
  "src/rustModelSourceGuard.test.ts",
]);

const RUST_ROOTS = ["crates/tine-core/src", "src-tauri/src"];

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

/** The root file of every split Rust module: an `X.rs` with a sibling `X/`. */
function splitModuleRoots(): Set<string> {
  return new Set(
    RUST_ROOTS.flatMap((root) => filesUnder(root, /\.rs$/)).filter((path) => {
      const dir = join(process.cwd(), path.replace(/\.rs$/, ""));
      return existsSync(dir) && statSync(dir).isDirectory();
    }),
  );
}

/** The repository files that a whole-file read on `line` of `file` names.
 *  That is an `include_str!`, resolved against the including file, or a
 *  read/compile call whose argument spells the path. Tables that merely NAME a
 *  path (the size ratchet, the print-site and primitive censuses) are not reads. */
function wholeFileReads(file: string, line: string): string[] {
  const includes = [...line.matchAll(/include_str!\(\s*"([^"]+\.rs)"\s*\)/g)].map((match) =>
    normalize(join(dirname(file), match[1])).replaceAll("\\", "/"),
  );
  const calls = [
    ...line.matchAll(
      /\b(?:readFileSync|source|compiled_source|read_to_string)\([^)]*["'`]((?:crates|src-tauri)\/[^"'`]+\.rs)["'`]/g,
    ),
  ].map((match) => match[1]);
  return [...includes, ...calls];
}

describe("split Rust modules are read whole", () => {
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

  it("sees code K7 moved out of query.rs, and leaves out test bodies", () => {
    const root = "crates/tine-core/src/query.rs";
    // K7.2 moved the property facets to query/facets.rs.
    expect(readFileSync(join(process.cwd(), root), "utf8")).not.toContain("pub fn property_facets(");
    expect(rustModuleSource(root)).toContain("pub fn property_facets(");
    expect(rustModuleFiles(root).some((path) => path.endsWith("_tests.rs"))).toBe(false);
  });

  it("no source guard reads a split module's root file alone", () => {
    const roots = splitModuleRoots();
    // The detector must see the known splits, or the scan below is vacuous.
    for (const known of ["model", "query", "publish"]) {
      expect(roots.has(`crates/tine-core/src/${known}.rs`), known).toBe(true);
    }
    const scanned = [
      ...filesUnder("crates", /\.rs$/),
      ...filesUnder("src-tauri/src", /\.rs$/),
      ...filesUnder("src", /\.tsx?$/),
    ].filter((path) => !EXEMPT.has(path));
    const offenders = scanned.flatMap((path) =>
      readFileSync(join(process.cwd(), path), "utf8")
        .split("\n")
        .flatMap((line, index) =>
          wholeFileReads(path, line)
            .filter((target) => roots.has(target))
            .map((target) => `${path}:${index + 1} reads ${target} alone`),
        ),
    );
    expect(offenders, RULE).toEqual([]);
  });
});
