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

  // Harvest W4-P1. The performance section states four derived-work bounds. A
  // bound whose proof test has been renamed, moved or deleted is a claim the
  // repository can no longer check, so each entry is validated in both
  // directions: the document names the producer, the bound and the exact test
  // symbol, and that symbol exists.
  it("pins every Harvest W4-P1 derived-work bound to a producer and a live proof test", () => {
    const contract = read("docs/contracts/frontend-staleness.md");
    const flat = contract.replace(/\s+/g, " ");
    expect(contract).toContain("## Harvest W4-P1 — derived-work bounds");

    const entries = [
      {
        heading: "**Item 1 — sort-key derivation.**",
        producerPath: "src/components/SheetTable.tsx",
        producerIdentifier: "onSortKey",
        bound: "at most `R` effective sort-key derivations per sort",
        testPath: "src/components/SheetTable.test.tsx",
        describeTitle: "SheetTable sort-key derivation (Harvest W4-P1 item 1)",
        itTitle: "derives at most one sort key per row for title, property, and formula sorts",
      },
      {
        heading: "**Item 2 — page-name merge (measured, verified-closed; no production change).**",
        producerPath: "src/pages.ts",
        producerIdentifier: "onMergePageNames",
        bound: "`0` merge-memo executions across five consecutive lulls",
        testPath: "src/pages.inventory.test.ts",
        describeTitle: "GH #229 complete page-name inventory",
        itTitle: "rebuilds the page-name merge zero times across five unchanged-reply lulls",
      },
      {
        heading: "**Item 3 — QueryBuilder facets.**",
        producerPath: "src/components/QueryBuilder.tsx",
        producerIdentifier: "sharedQueryResult",
        bound: "`queryFacets(false)` request per (graph scope, `dataRev`)",
        testPath: "src/components/QueryBuilder.transient.test.tsx",
        describeTitle: "QueryBuilder facet sharing (Harvest W4-P1 item 3)",
        itTitle:
          "issues one shared facets request per (graph scope, dataRev) for five mounted builders",
      },
      {
        heading: "**Item 4 — tag-table queries (measured, no cut).**",
        producerPath: "src/components/Page.tsx",
        producerIdentifier: "TagPageTable",
        bound: "`1` `runQuery` per distinct routed page per invalidation",
        testPath: "src/components/Page.test.tsx",
        describeTitle: "tag-page table",
        itTitle:
          "issues one tag query per distinct routed page per invalidation, not one per consumer",
      },
    ] as const;

    for (const entry of entries) {
      expect(flat).toContain(entry.heading);
      expect(flat).toContain(entry.bound);
      expect(flat).toContain(entry.producerPath);
      expect(read(entry.producerPath)).toContain(entry.producerIdentifier);

      const symbol = `${entry.testPath}::${entry.describeTitle}::${entry.itTitle}`;
      expect(flat).toContain(symbol);
      const spec = read(entry.testPath);
      expect(spec).toContain(`describe("${entry.describeTitle}"`);
      expect(spec).toContain(`it("${entry.itTitle}"`);
    }
  });
});
