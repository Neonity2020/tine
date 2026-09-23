import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

// GH #543, audit R11-09: `dataRev` moves on every landed save, and while an
// index pass runs each index-backed command waits in the backend until it
// ends. So the question "who reads the index again on every save?" must have
// an owner that keeps one read in flight per source: a `readLane`
// (src/readLane.ts; exemplar src/blockRefCounts.ts), the shared query owner
// `createReadyQueryResource`, the laned `resolveBlockBatched`, or a
// single-flight of its own. Every use of `dataRev` is pinned below with that
// owner. A new use fails here until it is given one.
const OWNERS: Record<string, { uses: number; owner: string }> = {
  "src/ui.ts": { uses: 1, owner: "the definition" },
  "src/blockRefCounts.ts": { uses: 3, owner: "readLane" },
  "src/pages.ts": { uses: 4, owner: "readLane" },
  "src/resolveBatch.ts": { uses: 1, owner: "readLane (one batch in flight)" },
  "src/components/BlockReferences.tsx": { uses: 1, owner: "readLane" },
  "src/components/CalendarJump.tsx": { uses: 1, owner: "readLane" },
  "src/components/Macro.tsx": { uses: 2, owner: "embed: readLane; query: createReadyQueryResource" },
  "src/render/inline.tsx": { uses: 2, owner: "block ref: resolveBlockBatched; peek: readLane" },
  "src/components/QueryBuilder.tsx": { uses: 1, owner: "createReadyQueryResource" },
  "src/components/Page.tsx": {
    uses: 3,
    owner: "tag tables: createReadyQueryResource; journal feed: reads only a pending restart",
  },
  "src/App.tsx": { uses: 1, owner: "refreshAliases single-flight (src/graph.ts)" },
  "src/components/Block.tsx": { uses: 1, owner: "templates: read on user request, not per save" },
  "src/sheet/formulaEval.ts": { uses: 2, owner: "no backend read (formula recompute)" },
};
const LANED = [
  "src/blockRefCounts.ts",
  "src/pages.ts",
  "src/resolveBatch.ts",
  "src/components/BlockReferences.tsx",
  "src/components/CalendarJump.tsx",
  "src/components/Macro.tsx",
  "src/render/inline.tsx",
];

function files(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return files(path);
    return /\.tsx?$/.test(name) && !/\.test\.tsx?$/.test(name) ? [path] : [];
  });
}

/** Uses of `dataRev` in code: imports and comments are not reads. */
function uses(source: string): number {
  const code = source
    .replace(/^import[\s\S]*?from\s*"[^"]*";/gm, "")
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/\/\/.*$/gm, "");
  return (code.match(/\bdataRev\b/g) ?? []).length;
}

describe("dataRev-keyed index reads (GH #543, R11-09)", () => {
  it("every use of dataRev has a named owner", () => {
    const found: Record<string, number> = {};
    for (const path of files("src")) {
      const n = uses(readFileSync(path, "utf8"));
      if (n) found[path] = n;
    }
    const pinned = Object.fromEntries(Object.entries(OWNERS).map(([path, { uses }]) => [path, uses]));
    expect(
      found,
      "a use of dataRev changed: a read keyed on it runs again on every save, and during an index pass each such read waits in the backend. Give it an owner that keeps one read in flight (readLane, see src/blockRefCounts.ts) and record it in OWNERS",
    ).toEqual(pinned);
  });

  it("laned sources read through a lane", () => {
    for (const path of LANED) expect(readFileSync(path, "utf8"), path).toMatch(/\breadLane\(\)/);
  });

  it("resolve_blocks batches run on one lane", () => {
    const source = readFileSync("src/resolveBatch.ts", "utf8");
    expect(source).toMatch(/lane\(current, \(\) => backend\(\)\.resolveBlocks\(batch\)\)/);
    expect(source.match(/backend\(\)\s*\.resolveBlocks\(/g) ?? []).toHaveLength(1);
  });
});
