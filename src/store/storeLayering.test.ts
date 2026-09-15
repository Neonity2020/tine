import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

const root = process.cwd();
const storeDirectory = path.join(root, "src/store");
const storeFacade = path.join(root, "src/store");
const RULE = "I-11: files under src/store/ must form an acyclic downward layer and must not import "
  + "the src/store.ts façade; src/store/doc.ts is the exemplar leaf.";

function productionModules(directory: string): string[] {
  const files: string[] = [];
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const absolute = path.join(directory, entry.name);
    if (entry.isDirectory()) files.push(...productionModules(absolute));
    else if (/\.tsx?$/.test(entry.name) && !/\.(test|spec)\.tsx?$/.test(entry.name)) files.push(absolute);
  }
  return files.sort();
}

function moduleSpecifiers(source: string): string[] {
  return [...source.matchAll(/\b(?:import|export)\s+(?:[^"']*?\s+from\s*)?["']([^"']+)["']/g)]
    .map((match) => match[1]);
}

function childTarget(importer: string, specifier: string, modules: Set<string>): string | null {
  if (!specifier.startsWith(".")) return null;
  const base = path.resolve(path.dirname(importer), specifier);
  for (const candidate of [`${base}.ts`, `${base}.tsx`, path.join(base, "index.ts"), path.join(base, "index.tsx")]) {
    if (modules.has(candidate)) return candidate;
  }
  return null;
}

function childCycles(edges: Map<string, string[]>): string[][] {
  const cycles: string[][] = [];
  const visiting = new Set<string>();
  const visited = new Set<string>();
  const stack: string[] = [];
  const visit = (file: string) => {
    if (visiting.has(file)) {
      const start = stack.indexOf(file);
      cycles.push([...stack.slice(start), file]);
      return;
    }
    if (visited.has(file)) return;
    visiting.add(file);
    stack.push(file);
    for (const target of edges.get(file) ?? []) visit(target);
    stack.pop();
    visiting.delete(file);
    visited.add(file);
  };
  for (const file of edges.keys()) visit(file);
  return cycles;
}

function storeLayeringViolations(): string[] {
  const files = productionModules(storeDirectory);
  const modules = new Set(files);
  const edges = new Map<string, string[]>();
  const violations: string[] = [];
  for (const file of files) {
    const targets: string[] = [];
    for (const specifier of moduleSpecifiers(readFileSync(file, "utf8"))) {
      const base = specifier.startsWith(".") ? path.resolve(path.dirname(file), specifier) : null;
      if (base === storeFacade) {
        violations.push(`${path.relative(root, file)} imports the store.ts façade via ${specifier}`);
        continue;
      }
      const target = childTarget(file, specifier, modules);
      if (target) targets.push(target);
    }
    edges.set(file, targets);
  }
  for (const cycle of childCycles(edges)) {
    violations.push(`child cycle: ${cycle.map((file) => path.relative(root, file)).join(" -> ")}`);
  }
  return violations;
}

describe("store child layering", () => {
  it("keeps children off the façade and the child graph acyclic", () => {
    expect(storeLayeringViolations(), RULE).toEqual([]);
  });

  it("detects a child cycle", () => {
    const cycles = childCycles(new Map([
      ["doc.ts", []],
      ["left.ts", ["right.ts"]],
      ["right.ts", ["left.ts"]],
    ]));
    expect(cycles, RULE).toEqual([["left.ts", "right.ts", "left.ts"]]);
  });
});
