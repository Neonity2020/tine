// GH #543, audits R12-06 and R13-08: "whose is a readiness retry?" A retry
// ends with its component because `runQueryWhenCurrent` requires a
// `Lifetime`, and the only lifetime a component can make ends in its
// `onCleanup` (`componentLifetime`). The behaviour is pinned by
// src/queryParseRetryOwner.test.tsx and QueryExportDialog.test.tsx; this guard
// keeps the type from being bypassed. Exemplar: `QueryMacro` in
// src/components/Macro.tsx.
import { describe, expect, it } from "vitest";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

function sources(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) sources(path, out);
    else if (/\.(ts|tsx)$/.test(name) && !/\.test\./.test(name)) out.push(path);
  }
  return out;
}

describe("readiness retries end with their owner (GH #543, R12-06/R13-08)", () => {
  const root = join(__dirname);
  const production = sources(root).map((path) => ({
    relative: path.slice(root.length + 1),
    text: readFileSync(path, "utf8"),
  }));

  it("makes a Lifetime only in queryReadiness.ts", () => {
    const forged = production
      .filter(({ relative, text }) => relative !== "queryReadiness.ts" && /as\s+Lifetime\b/.test(text))
      .map(({ relative }) => relative);
    expect(
      forged,
      "a Lifetime cast outside queryReadiness.ts can never end: call componentLifetime() in the " +
        "component's setup (exemplar: QueryMacro in src/components/Macro.tsx)",
    ).toEqual([]);
  });

  it("gives every component retry a lifetime that ends with the component", () => {
    const manual = production
      .filter(({ relative, text }) => relative !== "queryReadiness.ts" && text.includes("manualLifetime("))
      .map(({ relative }) => relative);
    expect(
      manual,
      "manualLifetime() is for owners that are not components and must call its end(); a " +
        "component uses componentLifetime(), which ends in onCleanup (exemplar: QueryMacro)",
    ).toEqual([]);
  });
});
