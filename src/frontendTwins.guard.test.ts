import { readFileSync, readdirSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { serializedWrites } from "./serializedWrites";

/** Every non-test frontend source under `src/`, so an ownership rule cannot be
 *  escaped by putting the offending copy in a directory the scan forgot. */
function frontendSources(): string[] {
  const out: string[] = [];
  const pending = ["src"];
  while (pending.length > 0) {
    const dir = pending.pop()!;
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const path = `${dir}/${entry.name}`;
      if (entry.isDirectory()) pending.push(path);
      else if (/\.tsx?$/.test(entry.name) && !/\.(test|spec)\.tsx?$/.test(entry.name)) out.push(path);
    }
  }
  return out;
}

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

  it("keeps 'the user pressed outside' on its one owner", () => {
    // GH #472. Eight popovers had hand-rolled this listener and four had not, so
    // whether a menu closed on an outside press was decided per component: the
    // Query Builder's sort popover closed and its clause menu did not. The
    // producer is `dismissOnOutsidePointer` in src/transientLayers.ts; anything
    // that registers a transient layer calls it instead of reaching for the
    // document. (A capture listener with no layer — blockDrag's drag start —
    // is not this question and is not scanned.)
    const owner = "src/transientLayers.ts";
    expect(readFileSync(owner, "utf8")).toContain("export function dismissOnOutsidePointer");

    const files = frontendSources().filter((path) => path !== owner);
    for (const file of files) {
      const source = readFileSync(file, "utf8");
      if (!source.includes("registerTransientLayer")) continue;
      for (const type of ["pointerdown", "mousedown"]) {
        expect(
          source,
          `I-12: ${file} registers a transient layer and its own "${type}" listener. ` +
            `Outside-press dismissal has one owner — call dismissOnOutsidePointer from ` +
            `${owner}; src/components/TopbarOverflowMenu.tsx is the one-line exemplar.`,
        ).not.toContain(`addEventListener("${type}"`);
      }
    }
    // Non-vacuity: the scan must actually reach the components this rule is about.
    expect(files.filter((path) => readFileSync(path, "utf8").includes("registerTransientLayer")).length)
      .toBeGreaterThanOrEqual(20);
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
