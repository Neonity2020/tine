import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  PUBLISHED_ABSENT_METHODS,
  PUBLISHED_ANSWERED_METHODS,
  PUBLISHED_CONSTANT_METHODS,
  PUBLISHED_REFUSED_METHODS,
  publishedBackend,
} from "./publishedBackend";
import { PublishedExportReadOnlyError } from "./backend";

// The published backend (Stage 2 of "Publish a query") must classify EVERY
// `Backend` method deliberately: a read the snapshot answers, a constant that
// keeps the app's lifecycle quiet, or a refusal. An unclassified method would
// silently become a refusal through the Proxy — which is safe for writes but
// wrong for a read the exported app needs. So a new Backend method fails here
// until its author decides which class it belongs to.
//
// Blessed exemplar for the pattern: src/plugins/capabilityBoundary.test.ts.

function backendInterfaceMethods(): string[] {
  const source = readFileSync(new URL("./backend.ts", import.meta.url), "utf8");
  const start = source.indexOf("export interface Backend {");
  expect(start).toBeGreaterThan(0);
  let depth = 0;
  let end = start;
  for (let i = source.indexOf("{", start); i < source.length; i++) {
    if (source[i] === "{") depth++;
    if (source[i] === "}") depth--;
    if (depth === 0) {
      end = i;
      break;
    }
  }
  const body = source
    .slice(start, end)
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/\/\/.*$/gm, "");
  const names = new Set<string>();
  for (const match of body.matchAll(/^ {2}([A-Za-z_][A-Za-z0-9_]*)\??\s*[(<]/gm)) names.add(match[1]);
  return [...names].sort();
}

describe("published backend classification (spec §5)", () => {
  const answered = new Set<string>(PUBLISHED_ANSWERED_METHODS);
  const constant = new Set<string>(PUBLISHED_CONSTANT_METHODS);
  const refused = new Set<string>(PUBLISHED_REFUSED_METHODS);
  const absent = new Set<string>(PUBLISHED_ABSENT_METHODS);

  it("puts every Backend method in exactly one class", () => {
    const methods = backendInterfaceMethods();
    expect(methods.length).toBeGreaterThan(200);
    const unclassified = methods.filter(
      (name) => !answered.has(name) && !constant.has(name) && !refused.has(name) && !absent.has(name),
    );
    expect(unclassified, "classify the new Backend method in src/publishedBackend.ts").toEqual([]);
    const twice = methods.filter(
      (name) => [answered, constant, refused, absent].filter((set) => set.has(name)).length > 1,
    );
    expect(twice).toEqual([]);
    const stale = [...answered, ...constant, ...refused, ...absent].filter((name) => !methods.includes(name));
    expect(stale, "listed but no longer on Backend").toEqual([]);
  });

  it("implements answered and constant methods as own properties and refuses the rest", async () => {
    const backend = publishedBackend(async () => {
      throw new Error("snapshot must not be needed for classification");
    }) as unknown as Record<string, unknown>;
    for (const name of [...answered, ...constant]) {
      expect(Object.prototype.hasOwnProperty.call(backend, name), name).toBe(true);
      expect(typeof backend[name], name).toBe("function");
    }
    for (const name of absent) {
      expect(backend[name], name).toBeUndefined();
    }
    for (const name of refused) {
      expect(Object.prototype.hasOwnProperty.call(backend, name), name).toBe(false);
      const method = backend[name] as () => Promise<unknown>;
      await expect(method(), name).rejects.toBeInstanceOf(PublishedExportReadOnlyError);
    }
  });

  it("keeps the refusal class free of anything the read-only app needs", () => {
    // Reads, queries, assets and the browser shims must be answered; a refusal
    // there is a blank page, not a safe no-op.
    for (const name of ["getPage", "listPages", "getBacklinks", "parseQuery", "queryRun", "readAsset", "streamAsset", "search", "quickSwitch", "loadGraph"]) {
      expect(answered.has(name), name).toBe(true);
    }
    // Writes and sync must never be answered, whatever the snapshot holds.
    for (const name of ["savePage", "deletePage", "renamePage", "saveAsset", "installPlugin", "activateSparseV2", "publishQuery", "restoreBackup"]) {
      expect(refused.has(name), name).toBe(true);
    }
  });
});
