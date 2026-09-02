import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

const source = (path: string) => readFileSync(join(process.cwd(), path), "utf8");

function tsxSources(root: string): string[] {
  const out: string[] = [];
  const pending = [root];
  while (pending.length > 0) {
    const dir = pending.pop()!;
    for (const entry of readdirSync(join(process.cwd(), dir), { withFileTypes: true })) {
      const path = `${dir}/${entry.name}`;
      if (entry.isDirectory()) pending.push(path);
      else if (entry.name.endsWith(".tsx") && !entry.name.endsWith(".test.tsx")) out.push(path);
    }
  }
  return out;
}

describe("I-22 hostile-content shape guard", () => {
  it("routes every component/render href through ExternalLink except the static issue URL", () => {
    const files = [...tsxSources("src/components"), ...tsxSources("src/render")];
    const offenders = files.flatMap((path) => {
      const hrefAnchors = [...source(path).matchAll(/<a\b[\s\S]{0,240}?\bhref\s*=/g)];
      if (path === "src/components/ExternalLink.tsx") return [];
      if (path === "src/components/ImproveTab.tsx") {
        return hrefAnchors.length === 1 && source(path).includes("href={ISSUES_URL}") ? [] : [path];
      }
      return hrefAnchors.length === 0 ? [] : [path];
    });
    expect(offenders, "I-22: graph-authored hrefs must compose ExternalLink").toEqual([]);
  });

  it("keeps the visual query renderer independently depth bounded", () => {
    expect(source("src/components/QueryBuilder.tsx"))
      .toContain("props.loc.length >= MAX_QUERY_BUILDER_DEPTH");
  });

  it("pins the I-22 contract table to the implementation constants", () => {
    const contract = source("docs/contracts/content-consumption-boundaries.md");
    expect(contract).toContain("128 AST levels (`MAX_FORMULA_EVAL_DEPTH`)");
    expect(contract).toContain("64 levels (`MAX_QUERY_BUILDER_DEPTH`)");
    expect(contract).toContain("64 levels (`MAX_PEEK_BLOCK_DEPTH`)");
    expect(contract).toContain("128 levels (`MAX_MANAGED_BLOCK_DEPTH`)");
    expect(source("src/sheet/formula/eval.ts")).toContain("MAX_FORMULA_EVAL_DEPTH = 128");
    expect(source("src/editor/queryBuilder.ts")).toContain("MAX_QUERY_BUILDER_DEPTH = 64");
    expect(source("src/render/PeekPopup.tsx")).toContain("MAX_PEEK_BLOCK_DEPTH = 64");
    expect(source("crates/tine-core/src/model.rs"))
      .toContain("pub(crate) const MAX_MANAGED_BLOCK_DEPTH: usize = 128");
  });
});
