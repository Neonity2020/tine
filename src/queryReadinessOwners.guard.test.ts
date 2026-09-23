// GH #543, audit R12-06: "whose is a readiness retry?" `runQueryWhenCurrent`
// owns the graph half (the binding the read started on). Each caller's gate
// owns the other half: it must end when its owner is disposed. Exemplar:
// `createReferenceFetcher` in src/lib/referenceFetch.ts.
import { describe, expect, it } from "vitest";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

/** Per file: how many calls, and the disposal each call's gate reads (any
 *  of them; each one is set or bumped by that component's onCleanup). */
const OWNERS: Record<string, { calls: number; disposal: string[] }> = {
  "components/Macro.tsx": { calls: 2, disposal: ["!disposed"] },
  // `current` reads the fetcher's own `disposed`.
  "lib/referenceFetch.ts": { calls: 1, disposal: ["current"] },
  "components/QueryExportDialog.tsx": { calls: 2, disposal: ["!disposed"] },
  // `accepts` reads `disposed`; `anchorRevision` is bumped on cleanup.
  "components/QueryBuilder.tsx": { calls: 2, disposal: ["accepts(", "anchorRevision"] },
  // Bumped on cleanup.
  "components/LinkedReferences.tsx": { calls: 1, disposal: ["nativeRequestVersion"] },
  "components/Settings.tsx": { calls: 1, disposal: ["!publishDisposed"] },
};

function sources(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) sources(path, out);
    else if (/\.(ts|tsx)$/.test(name) && !/\.test\./.test(name)) out.push(path);
  }
  return out;
}

/** The argument list of each `runQueryWhenCurrent(` call. */
function calls(text: string): string[] {
  const found: string[] = [];
  let at = text.indexOf("runQueryWhenCurrent(");
  while (at >= 0) {
    let depth = 0;
    let end = at + "runQueryWhenCurrent".length;
    for (; end < text.length; end++) {
      if (text[end] === "(") depth++;
      else if (text[end] === ")" && --depth === 0) break;
    }
    found.push(text.slice(at, end + 1));
    at = text.indexOf("runQueryWhenCurrent(", end);
  }
  return found;
}

describe("readiness retries end with their owner (GH #543, R12-06)", () => {
  it("names a disposal in every caller's gate", () => {
    const root = join(__dirname);
    const seen: Record<string, number> = {};
    const missing: string[] = [];
    for (const path of sources(root)) {
      const relative = path.slice(root.length + 1);
      if (relative === "queryReadiness.ts") continue;
      const found = calls(readFileSync(path, "utf8"));
      if (found.length === 0) continue;
      seen[relative] = found.length;
      const owner = OWNERS[relative];
      for (const call of found) {
        if (!owner || !owner.disposal.some((token) => call.includes(token))) missing.push(`${relative}: ${call.slice(0, 120)}`);
      }
    }
    expect(
      seen,
      "a new runQueryWhenCurrent caller: add it to OWNERS with the disposal its gate reads",
    ).toEqual(Object.fromEntries(Object.entries(OWNERS).map(([file, { calls }]) => [file, calls])));
    expect(
      missing,
      "a readiness retry whose gate never ends when its owner is disposed polls for as long " +
        "as the index is not ready; read a disposal flag set in onCleanup (exemplar: " +
        "createReferenceFetcher)",
    ).toEqual([]);
  });

  it("keeps the binding half in the shared helper", () => {
    const helper = readFileSync(join(__dirname, "queryReadiness.ts"), "utf8");
    const body = helper.slice(helper.indexOf("export function runQueryWhenCurrent"));
    expect(body).toMatch(/const binding = graphBinding\(\);\s*const isCurrent = \(\) => graphBinding\(\) === binding && callerIsCurrent\(\);/);
  });
});
