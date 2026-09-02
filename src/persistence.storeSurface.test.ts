import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

// The persistence/store split exists to bound one coupling: persistence.ts owns
// WHEN and HOW an edit reaches disk, store.ts owns the doc tree. Its header used
// to claim the dependency was two bindings while the import already named ten —
// a claim nothing checked. Pin the surface instead of describing it.
//
// Growing this list is allowed; growing it silently is not. Add the binding here
// and say in the header what persistence now needs from the store.
const DECLARED_STORE_SURFACE = [
  "doc",
  "bumpEditGeneration",
  "editorActivationFor",
  "peekPageInstanceGeneration",
  "setProspectiveTarget",
  "pageByName",
  "pageInstanceGeneration",
  "pageToDto",
  "setEditorActivation",
  "sweepReplaceable",
];

function storeImportBindings(source: string): string[] {
  const match = /import\s*\{([^}]*)\}\s*from\s*"\.\/store";/.exec(source);
  if (!match) throw new Error("persistence.ts no longer has a named import from ./store");
  return match[1]
    .split(",")
    .map((name) => name.trim())
    .filter((name) => name.length > 0);
}

describe("persistence.ts store surface", () => {
  const source = readFileSync(fileURLToPath(new URL("./persistence.ts", import.meta.url)), "utf8");

  it("imports exactly the bindings its header declares", () => {
    expect(storeImportBindings(source)).toEqual(DECLARED_STORE_SURFACE);
  });

  it("has only one import from ./store", () => {
    const imports = source.match(/from\s*"\.\/store";/g) ?? [];
    expect(imports.length).toBe(1);
  });
});
