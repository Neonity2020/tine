import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const read = (path: string) => readFileSync(join(process.cwd(), path), "utf8");

describe("frontend-staleness living contract", () => {
  it("pins every documented landing helper and specialized exemplar", () => {
    const contract = read("docs/contracts/frontend-staleness.md");
    const landAsync = read("src/landAsync.ts");

    for (const helper of [
      "captureGraphScope",
      "isScopeCurrent",
      "landAsync",
      "landAsyncOrToast",
    ]) {
      expect(contract).toContain(`\`${helper}\``);
      expect(landAsync).toMatch(new RegExp(`export (?:async )?function ${helper}\\b`));
    }

    const exemplars = [
      ["pdfOwnership.ts", "src/pdfOwnership.ts", "pdfOwnershipKey"],
      ["RightSidebar", "src/components/RightSidebar.tsx", "useEnsurePage"],
      ["Block", "src/components/Block.tsx", "editorIsCurrent"],
      ["graph", "src/graph.ts", "journalTemplateOwnerIsCurrent"],
    ] as const;
    for (const [contractName, path, identifier] of exemplars) {
      expect(contract).toContain(contractName);
      expect(read(path)).toContain(identifier);
    }

    expect(contract).toContain("`persistence.ts:362`");
    expect(contract).toContain("I-20");
    const guard = read("src/frontendStaleness.guard.test.ts");
    expect(guard).toContain("persistence.ts:362");
    expect(guard).toContain("I-20");
  });
});
