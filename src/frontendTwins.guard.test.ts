import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { serializedWrites } from "./serializedWrites";

describe("frontend twin ownership guards", () => {
  it("keeps Sheets field questions on their canonical owners", () => {
    for (const file of ["src/components/SheetBoard.tsx", "src/components/SheetTable.tsx"]) {
      const source = readFileSync(file, "utf8");
      for (const twin of ["function recordFacets", "function fieldIdsForRecords", "function rowRaw", "function dtoField"]) {
        expect(
          source,
          `I-12: ${file} must use src/sheet/fields.ts; follow journal_feed.rs:157's owner-and-guard pattern`,
        ).not.toContain(twin);
      }
    }
  });

  it("serializes operations, propagates their errors, and keeps the tail usable", async () => {
    const writes = serializedWrites("guard-fixture");
    const events: string[] = [];
    let release!: () => void;
    const blocked = new Promise<void>((resolve) => { release = resolve; });

    const first = writes.run(async () => {
      events.push("first:start");
      await blocked;
      events.push("first:end");
      return 1;
    });
    const second = writes.run(async () => {
      events.push("second");
      throw new Error("expected write failure");
    });
    const third = writes.run(async () => {
      events.push("third");
      return 3;
    });

    await Promise.resolve();
    expect(events).toEqual(["first:start"]);
    release();
    await expect(first).resolves.toBe(1);
    await expect(second).rejects.toThrow("expected write failure");
    await expect(third).resolves.toBe(3);
    expect(events).toEqual(["first:start", "first:end", "second", "third"]);
    expect(writes.scope).toBe("guard-fixture");
  });

  it("pins the two specialized tails and the PDF tracking non-fit", () => {
    const pluginManager = readFileSync("src/plugins/manager.ts", "utf8");
    const store = readFileSync("src/store.ts", "utf8");
    const pdfOwnership = readFileSync("src/pdfOwnership.ts", "utf8");

    const rule = "I-12: promise-tail serialization has one owner, src/serializedWrites.ts; the two specialized "
      + "tails and the PDF mutation set are the pinned exceptions (see the exception comments in each file)";
    expect(pluginManager, rule).toContain("persistenceChains = new Map<string, Promise<void>>()");
    expect(store, rule).toContain("let managedMoveQueue: Promise<void> = Promise.resolve()");
    expect(pdfOwnership, rule).toContain("const mutations = new Map<number, Set<Promise<boolean>>>()");
    expect(pdfOwnership, rule).not.toContain("serializedWrites");
  });

  it("keeps every fitting shared-key site on serializedWrites", () => {
    for (const file of [
      "src/themes/manager.ts",
      "src/themeGallery.ts",
      "src/plugins/registry.ts",
      "src/workspaces.ts",
      "src/mediaBlobFallback.ts",
    ]) {
      expect(
        readFileSync(file, "utf8"),
        `I-12: ${file} must serialize shared-key writes through src/serializedWrites.ts (exemplar: src/themes/manager.ts)`,
      ).toContain("serializedWrites");
    }
  });
});
