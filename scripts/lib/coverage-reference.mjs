import fs from "node:fs";
import path from "node:path";

// A regression-catalog coverage reference is `path`, `path#symbol`, or
// Rust-style `path::symbol` (a real path never contains `::` or `#`). Validating
// only the file lets a rename silently turn a coverage claim into a lie: on
// 2026-08-17 fifteen references named tests that no longer existed, eight of
// them on entries marked `covered`. Resolve the symbol too.
//
// This is deliberately string-level. The claim under test is "a test with this
// name exists in this file", which a rename breaks and a refactor does not; a
// real parse would buy nothing and would couple the catalog checker to two
// language toolchains.

const SOURCE_EXTENSIONS = new Set(["ts", "tsx", "js", "jsx", "mjs", "cjs"]);

export function splitCoverageReference(reference) {
  const separator = reference.search(/::|#/);
  if (separator === -1) return { file: reference, symbol: null };
  const width = reference.slice(separator, separator + 2) === "::" ? 2 : 1;
  return { file: reference.slice(0, separator), symbol: reference.slice(separator + width) };
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * @returns {string | null} a problem description, or null when the reference resolves.
 */
export function coverageReferenceProblem(root, reference) {
  const { file, symbol } = splitCoverageReference(reference);
  const absolute = path.join(root, file);
  if (!fs.existsSync(absolute)) return `missing test file ${file}`;
  if (symbol === null) return null;

  const source = fs.readFileSync(absolute, "utf8");
  const extension = file.split(".").pop();

  if (extension === "rs") {
    // `module::path::test_name` -- the last segment is the item that must exist.
    // Accept a module so a family reference (`doc.rs::org_container_outline_tests`)
    // stays legal, since it names a real Rust item that a rename would break.
    const name = escapeRegExp(symbol.split("::").pop());
    if (new RegExp(`\\bfn\\s+${name}\\b`).test(source)) return null;
    if (new RegExp(`\\bmod\\s+${name}\\b`).test(source)) return null;
    return `${file} has no test fn or module named ${symbol.split("::").pop()} (referenced as ${reference})`;
  }

  if (SOURCE_EXTENSIONS.has(extension)) {
    // A vitest/node reference is `file::describe title::it title`; every named
    // title must still be present verbatim.
    for (const segment of symbol.split("::")) {
      if (!source.includes(segment)) return `${file} contains no test titled ${JSON.stringify(segment)} (referenced as ${reference})`;
    }
    return null;
  }

  // Receipts, workflows and other evidence files: the named anchor must appear.
  if (!source.includes(symbol)) return `${file} does not contain ${JSON.stringify(symbol)} (referenced as ${reference})`;
  return null;
}
