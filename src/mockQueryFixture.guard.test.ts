import { describe, it, expect } from "vitest";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { installMockQueryFixture, mockBackend } from "./mock";

// **The fixture seam is mock-only, and this is what says so** (P3, T2 step 2).
//
// `installMockQueryFixture` exists because the anchor-switch preview is a
// print-then-parse round trip through the ENGINE, and jsdom has no engine. The
// danger of any such seam is that it stops being a test double and starts being
// a second answer production code consults — which is the twin this campaign
// removed. So: a source scan asserting nothing in `src/` outside the tests and
// the mock itself imports it, plus the behavioural statement that without a
// fixture the mock keeps its ordinary refusal.

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

describe("the mock query fixture seam", () => {
  it("is imported by no production module", () => {
    const offenders = sources(join(root, "src")).filter((path) => {
      if (path.endsWith(join("src", "mock.ts"))) return false;
      // The testkits are test support, not production.
      if (/TestkitO?r?|Testkit\.ts$/.test(path)) return false;
      return readFileSync(path, "utf8").includes("installMockQueryFixture");
    });
    expect(offenders, "installMockQueryFixture is a test double, not a data source").toEqual([]);
  });

  it("keeps the mock's ordinary refusal when no fixture is installed", async () => {
    installMockQueryFixture(null);
    const backend = mockBackend();
    await expect(
      backend.printQuery(
        { anchor: "block", filter: { kind: "true" }, source: { kind: "builder" } },
        {},
        "tql",
      ),
    ).rejects.toThrow();
    const parsed = await backend.parseQuery("@page and task = 'TODO'", "tql");
    expect(parsed.query.filter.kind).toBe("raw");
    expect(parsed.query.anchor).toBe("block");
  });

  it("answers with the canned VALUES it was given, and computes nothing", async () => {
    installMockQueryFixture({
      print: "@page and task = 'TODO'",
      parse: {
        query: {
          anchor: "page",
          filter: { kind: "raw", text: "task = 'TODO'", diagnostic_kind: "not_applicable" },
          diagnostics: [
            { kind: "not_applicable", message: "`task` does not apply to pages", disabled: false },
          ],
          source: { kind: "tql", original: "@page and task = 'TODO'" },
        },
        view: {},
      },
    });
    const backend = mockBackend();
    // Same bytes whatever it is asked — it is a canned value, not a printer.
    expect(await backend.printQuery(
      { anchor: "block", filter: { kind: "true" }, source: { kind: "builder" } },
      {},
      "tql",
    )).toBe("@page and task = 'TODO'");
    expect((await backend.parseQuery("anything at all", "tql")).query.anchor).toBe("page");
    installMockQueryFixture(null);
  });
});
