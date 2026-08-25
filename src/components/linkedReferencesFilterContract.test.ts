// Doc-code consistency for docs/contracts/linked-references-filter.md. A
// contract whose numbers can drift silently is not a contract; this fails CI
// instead of letting the document rot (AGENTS.md §2, living contracts).
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const contract = readFileSync("docs/contracts/linked-references-filter.md", "utf8");
const source = readFileSync("src/components/LinkedReferences.tsx", "utf8");

describe("Linked References filter contract matches the source", () => {
  it("states the same search debounce the component uses", () => {
    expect(contract).toContain("**120 ms**");
    const debounce = source.match(/setTimeout\(\(\) => setSearchQuery\(value\), (\d+)\)/);
    expect(debounce).not.toBeNull();
    expect(debounce![1]).toBe("120");
  });

  it("states the same OG collapse threshold the component uses", () => {
    expect(contract).toContain("| **100** | `OG_REFERENCE_COLLAPSE_THRESHOLD` |");
    const threshold = source.match(/OG_REFERENCE_COLLAPSE_THRESHOLD = (\d+)/);
    expect(threshold).not.toBeNull();
    expect(threshold![1]).toBe("100");
  });

  it("describes the pending-summary wording the component renders", () => {
    expect(contract).toContain("Indexing N references… the filter applies when this");
    expect(source).toContain("Indexing {totalCount()} references… the filter applies when this finishes");
  });

  it("keeps the chip list independent of the filter selections", () => {
    // §3: coRefs() must not read filters(); the orphan pass is a separate memo.
    const coRefs = source.slice(source.indexOf("const coRefs = createMemo"));
    const body = coRefs
      .slice(0, coRefs.indexOf("const orphanFilters"))
      .split("\n")
      .filter((line) => !line.trim().startsWith("//"))
      .join("\n");
    expect(body).not.toContain("filters()");
    expect(source).toContain("const orphanFilters = createMemo");
  });
});
