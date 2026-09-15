import { readFileSync } from "node:fs";
import ts from "typescript";
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

/** Every member of `export interface Backend` in backend.ts — method
 *  signatures and function-valued properties alike, own or inherited through
 *  `extends` from an interface in the same file — read through the
 *  TypeScript AST, so indentation, comments, overloads, property syntax and
 *  inheritance cannot hide a member from the classification. A base that is
 *  not declared in the file cannot be walked and fails loudly. */
export function backendInterfaceMethods(source = readFileSync(new URL("./backend.ts", import.meta.url), "utf8")): string[] {
  const file = ts.createSourceFile("backend.ts", source, ts.ScriptTarget.Latest, true);
  const interfaces = new Map<string, ts.InterfaceDeclaration>();
  for (const statement of file.statements) {
    if (ts.isInterfaceDeclaration(statement)) interfaces.set(statement.name.text, statement);
  }
  const names = new Set<string>();
  const visited = new Set<string>();
  const collect = (name: string) => {
    if (visited.has(name)) return;
    visited.add(name);
    const declaration = interfaces.get(name);
    if (!declaration) throw new Error(`Backend extends ${name}, which is not an interface declared in backend.ts`);
    for (const member of declaration.members) {
      if (ts.isMethodSignature(member) || ts.isPropertySignature(member)) {
        if (member.name && (ts.isIdentifier(member.name) || ts.isStringLiteral(member.name))) names.add(member.name.text);
        else throw new Error(`Backend member with an unsupported name at ${member.pos}`);
      } else {
        throw new Error(`Backend member that is neither a method nor a property at ${member.pos}`);
      }
    }
    for (const clause of declaration.heritageClauses ?? []) {
      if (clause.token !== ts.SyntaxKind.ExtendsKeyword) continue;
      for (const type of clause.types) {
        if (!ts.isIdentifier(type.expression)) throw new Error(`Backend extends an unsupported base at ${type.pos}`);
        collect(type.expression.text);
      }
    }
  };
  collect("Backend");
  expect(names.size).toBeGreaterThan(0);
  return [...names].sort();
}

describe("published backend classification (spec §5)", () => {
  const answered = new Set<string>(PUBLISHED_ANSWERED_METHODS);
  const constant = new Set<string>(PUBLISHED_CONSTANT_METHODS);
  const refused = new Set<string>(PUBLISHED_REFUSED_METHODS);
  const absent = new Set<string>(PUBLISHED_ABSENT_METHODS);

  it("sees a member however it is written: tab-indented, optional, or a function-valued property", () => {
    const names = backendInterfaceMethods(
      "export interface Backend {\n  a(): Promise<void>;\n\tb?(): void;\n    c: () => Promise<void>;\n  /* d(): void */\n  // e(): void\n  f<T>(x: T): T;\n  f(): void;\n}\n",
    );
    expect(names).toEqual(["a", "b", "c", "f"]);
  });

  it("sees a member inherited through `extends`, and refuses a base it cannot read", () => {
    const names = backendInterfaceMethods(
      "interface Reads { g(): void }\ninterface Writes extends Reads { h(): void }\nexport interface Backend extends Writes, Reads {\n  a(): void;\n}\n",
    );
    expect(names).toEqual(["a", "g", "h"]);
    expect(() => backendInterfaceMethods("export interface Backend extends Elsewhere { a(): void }\n")).toThrow(/Elsewhere/);
  });

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
    for (const name of ["savePage", "deletePage", "renamePage", "saveAsset", "installPlugin", "publishQuery", "restoreBackup"]) {
      expect(refused.has(name), name).toBe(true);
    }
  });
});
