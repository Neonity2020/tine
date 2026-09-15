// File-size ratchet: budget B1 of the consolidation plan (2026-09-15).
//
// Rule: no production source file above 4,000 lines and no test file above
// 16,000. A file that was already larger when this ratchet landed is pinned at
// that length, and the pin is a ceiling that may only fall. When a seam cut
// moves code out of a pinned file, lower its pin in the same commit. A pin
// above the file's real length fails too, so a gain cannot be quietly spent.
//
// Why a hard number: `model.rs` reached 25,000 lines one reasonable addition at
// a time, because agents put new code next to the similar code they are
// reading. A cap turns "append it here" into "find the seam". A cut is a
// legibility gain even when it moves zero net lines (Martin, 2026-09-12).
//
// Shape to imitate: `crates/tine-core/src/query/`, one parent module with
// focused children (`sql.rs`, `results.rs`, `rank.rs`) at minimal visibility.
import { describe, expect, it } from "vitest";
import { readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("..", import.meta.url));
const PRODUCTION_CAP = 4_000;
const TEST_CAP = 16_000;

/** Source roots the ratchet scans, relative to the repository root. */
const ROOTS = ["crates", "src", "src-tauri/src", "scripts", "plugin-sdk", "community-plugins"];
const SOURCE = /\.(rs|ts|tsx|mjs|js|css)$/;
/** Third-party copies and build output are not ours to size. */
const SKIPPED_DIRS = new Set(["node_modules", "target", "dist", "vendor", "gen"]);

/** Files above their cap when the ratchet landed, pinned at that length. */
const PINNED: Record<string, number> = {
  "crates/tine-core/src/model.rs": 19_267,
  "src/styles/app.css": 12_369,
  "crates/tine-core/src/publish.rs": 9_231,
  "crates/tine-core/src/direct_projection.rs": 8_180,
  "crates/tine-core/src/query.rs": 7_051,
  "src/store.ts": 6_370,
  "src-tauri/src/watcher.rs": 4_625,
  "src-tauri/src/commands.rs": 4_419,
  "src/components/Block.tsx": 4_093,
  "crates/tine-core/src/query/sql.rs": 4_074,
};

export function isTestFile(relative: string): boolean {
  return /_tests\.rs$/.test(relative)
    || /\.test\.tsx?$/.test(relative)
    || relative.split("/").includes("tests");
}

/** Newline count, the same number `wc -l` prints. */
function lineCount(file: string): number {
  return readFileSync(file, "utf8").split("\n").length - 1;
}

function sourceFiles(dir: string, out: string[]): string[] {
  let entries: string[];
  try {
    entries = readdirSync(dir);
  } catch {
    return out;
  }
  for (const entry of entries) {
    if (SKIPPED_DIRS.has(entry) || entry.startsWith(".")) continue;
    const full = path.join(dir, entry);
    if (statSync(full).isDirectory()) sourceFiles(full, out);
    else if (SOURCE.test(entry)) out.push(full);
  }
  return out;
}

const RULE = "Budget B1: no production source file above 4,000 lines and no test file above 16,000; "
  + "a file pinned in fileSizeRatchet.test.ts may only shrink. Cut along a seam instead of appending "
  + "(shape to imitate: crates/tine-core/src/query/), and lower the pin in the same commit.";

describe("file-size ratchet (B1)", () => {
  const counts = new Map<string, number>();
  for (const root of ROOTS) {
    for (const file of sourceFiles(path.join(ROOT, root), [])) {
      counts.set(path.relative(ROOT, file).split(path.sep).join("/"), lineCount(file));
    }
  }

  it("scans the repository's source roots", () => {
    expect(counts.get("crates/tine-core/src/lib.rs")).toBeGreaterThan(0);
    expect(counts.get("src/store.ts")).toBeGreaterThan(0);
    expect(counts.get("src-tauri/src/lib.rs")).toBeGreaterThan(0);
  });

  it("keeps every file within its cap or its pin", () => {
    const over: string[] = [];
    for (const [file, lines] of counts) {
      const limit = PINNED[file] ?? (isTestFile(file) ? TEST_CAP : PRODUCTION_CAP);
      if (lines > limit) over.push(`${file}: ${lines} lines, limit ${limit}`);
    }
    expect(over, RULE).toEqual([]);
  });

  it("keeps every pin exact, and drops a pin once its file is under the cap", () => {
    const stale: string[] = [];
    for (const [file, pin] of Object.entries(PINNED)) {
      const lines = counts.get(file);
      const cap = isTestFile(file) ? TEST_CAP : PRODUCTION_CAP;
      if (lines === undefined) stale.push(`${file}: pinned but not found; delete the pin`);
      else if (lines <= cap) stale.push(`${file}: ${lines} lines is within the cap; delete the pin`);
      else if (lines < pin) stale.push(`${file}: ${lines} lines, pin ${pin}; lower the pin to ${lines}`);
    }
    expect(stale, RULE).toEqual([]);
  });
});
