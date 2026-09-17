// A projection read that is unwrapped instead of awaited is a race with a
// worker, dressed as an assertion.
//
// Every public Direct query answers from the SQLite projection or returns a
// typed `QueryExecutionError`. `NotReady` is not a failure — it is the public
// readiness signal, the same one `src/queryReadiness.ts` loops on — so a test
// that calls `.unwrap()` on one of these passes whenever the worker happened to
// finish first and panics whenever it did not. On an idle laptop it finishes
// first essentially always; on a loaded hosted runner it does not, which is how
// this class produces "hosted-only" failures that no local run reproduces.
//
// `crates/tine-core/tests/support/ready_query.rs` exports `when_ready`, which
// retries ONLY `NotReady` and fails immediately on any other error, so a real
// regression still reads as one. It is the blessed exemplar;
// `crates/tine-core/tests/issue186_surfaces.rs` is a short call site to imitate.
//
// Unlike the sleep and click ratchets, this guard is ABSOLUTE: the class is at
// zero, so there is no budget to carry. The guarded method list is DERIVED from
// the Rust signatures rather than written down here, so a new query method that
// returns the typed error is covered the day it is added.
import { describe, expect, it } from "vitest";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const modelDir = path.join(root, "crates", "tine-core", "src", "model");
const testsDir = path.join(root, "crates", "tine-core", "tests");

/** Every `Graph` method whose signature returns the typed readiness error. */
export function readinessReturningMethods(sources: string[]): string[] {
  const names = new Set<string>();
  for (const source of sources) {
    for (const chunk of source.split(/\bpub fn\s+/).slice(1)) {
      const signature = chunk.slice(0, chunk.indexOf("{"));
      const name = /^(\w+)/.exec(chunk)?.[1];
      if (name && /QueryExecutionError\s*>/.test(signature)) names.add(name);
    }
  }
  return [...names].sort();
}

/** Skip over a Rust string/char literal or comment starting at `index`. */
function skipLiteral(source: string, index: number): number | null {
  if (source.startsWith("//", index)) {
    const end = source.indexOf("\n", index);
    return end < 0 ? source.length : end;
  }
  const raw = /^r(#*)"/.exec(source.slice(index, index + 8));
  if (raw) {
    const close = `"${raw[1]}`;
    const end = source.indexOf(close, index + raw[0].length);
    return end < 0 ? source.length : end + close.length;
  }
  if (source[index] === '"') {
    for (let at = index + 1; at < source.length; at += 1) {
      if (source[at] === "\\") at += 1;
      else if (source[at] === '"') return at + 1;
    }
    return source.length;
  }
  return null;
}

/**
 * Call sites where a readiness-returning method is unwrapped, by line.
 *
 * `unwrap_or`/`unwrap_or_default` count too: swallowing `NotReady` into an
 * empty answer turns the race into a silently wrong assertion instead of a
 * panic, which is worse, not better.
 */
export function unwrappedReadinessReads(source: string, methods: string[]): number[] {
  const sites: number[] = [];
  const call = new RegExp(`\\.\\s*(${methods.join("|")})\\s*\\(`, "g");
  for (const match of source.matchAll(call)) {
    let depth = 0;
    let at = match.index! + match[0].length - 1;
    for (; at < source.length; at += 1) {
      const skipped = skipLiteral(source, at);
      if (skipped !== null) {
        at = skipped - 1;
        continue;
      }
      if (source[at] === "(") depth += 1;
      else if (source[at] === ")" && (depth -= 1) === 0) break;
    }
    const after = source.slice(at + 1).replace(/^\s+/, "");
    if (/^\.\s*(unwrap\w*|expect)\s*\(/.test(after)) {
      sites.push(source.slice(0, match.index!).split("\n").length);
    }
  }
  return sites;
}

function rustFiles(dir: string): string[] {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) return rustFiles(full);
    return entry.isFile() && entry.name.endsWith(".rs") ? [full] : [];
  });
}

describe("projection readiness guard", () => {
  const methods = readinessReturningMethods(
    fs.readdirSync(modelDir)
      .filter((name) => name.endsWith(".rs"))
      .map((name) => fs.readFileSync(path.join(modelDir, name), "utf8")),
  );

  it("derives the guarded methods from the Rust signatures", () => {
    // If a rename empties this, the guard would pass vacuously over every file.
    expect(methods.length).toBeGreaterThan(5);
    expect(methods).toContain("search");
    expect(methods).toContain("run_graph_search");
    expect(methods).toContain("run_query");
  });

  it("detects the shape it exists to catch", () => {
    const regression = [
      'let friendly = graph.run_graph_search("医保", 100, 100, false).unwrap();',
      'let hits = graph.search("needle", 10).expect("search");',
      "let groups = graph.run_query(source).unwrap_or_default();",
    ].join("\n");
    expect(unwrappedReadinessReads(regression, methods)).toEqual([1, 2, 3]);
  });

  it("accepts the readiness gate, and a paren inside a literal", () => {
    const fixed = [
      'let friendly = ready_query::when_ready(|| graph.run_graph_search("医保(1)", 100, 100, false));',
      'let hits = ready_query::when_ready(|| graph.search("needle", 10));',
      "let groups = ready_query::run_query(&graph, source);",
    ].join("\n");
    expect(unwrappedReadinessReads(fixed, methods)).toEqual([]);
  });

  const files = rustFiles(testsDir).sort();

  it("finds the integration tests it is supposed to scan", () => {
    expect(files.length).toBeGreaterThan(20);
  });

  for (const file of files) {
    const relative = path.relative(root, file);
    it(`${relative}: no projection read is unwrapped`, () => {
      const sites = unwrappedReadinessReads(fs.readFileSync(file, "utf8"), methods);
      expect(
        sites.length,
        `${relative} unwraps a projection read at line(s) ${sites.join(", ")}. `
          + "NotReady is the public readiness signal, not a failure: unwrapping it passes when "
          + "the worker won the race and panics when it did not, which is how this class produces "
          + "hosted-only failures. Wrap the call in ready_query::when_ready() — see "
          + "crates/tine-core/tests/support/ready_query.rs, and issue186_surfaces.rs for a call site.",
      ).toBe(0);
    });
  }
});
