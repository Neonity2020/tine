import { describe, expect, it } from "vitest";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));

function sources(dir: string, out: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    if (entry === "node_modules") continue;
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) {
      sources(path, out);
      continue;
    }
    if (/\.(ts|tsx)$/.test(entry) && !/\.test\.(ts|tsx)$/.test(entry)) out.push(path);
  }
  return out;
}

describe("browser-preview search matching ownership", () => {
  it("is unreachable from production UI modules", () => {
    const offenders = sources(join(root, "src")).filter((path) => {
      if (path.endsWith(join("src", "mock.ts"))) return false;
      if (path.endsWith(join("src", "mockSearchQuery.ts"))) return false;
      return readFileSync(path, "utf8").includes("mockSearchQuery");
    });
    expect(offenders, "preview matching is a mock adapter, never a production dependency").toEqual([]);
  });

  it("keeps folding and matching out of the frontend grammar owner", () => {
    const grammar = readFileSync(join(root, "src", "editor", "searchQuery.ts"), "utf8");
    expect(grammar).not.toContain("toLowerCase(");
    expect(grammar).not.toContain(".normalize(");
    expect(grammar).not.toContain("mockSearchMatches");
  });
});
