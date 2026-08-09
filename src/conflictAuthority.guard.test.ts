import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";
import ts from "typescript";
import { describe, expect, it } from "vitest";

// A conflict banner is a promise that "Keep mine" can rescue the unsaved edit.
// It can only keep that promise if the force presents the observation epoch the
// banner was shown, and exactly one thing mints one: the backend, when it
// refuses a guarded save over the disk state it just observed. `doSave` receives
// that epoch on the conflict error and is therefore the only place that may
// raise a banner.
//
// Three other places used to raise one directly — both watcher arms and the
// sparse-v2 reconciliation — and every banner they raised was unresolvable:
// "Keep mine" presented null, `save_page` refused it, the retry is forbidden
// while the page is conflicted, and the only live action left discarded the
// user's work. They now route through `reconcileExternalChange`. This test is
// the reminder, not a style rule. (GH #254 increment 2, correction-delta
// re-verification, HIGH blocker.)
const ALLOWED = new Set(["src/persistence.ts", "src/ui.ts"]);

function sourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const file = path.join(dir, entry.name);
    if (entry.isDirectory()) return sourceFiles(file);
    return /\.tsx?$/.test(entry.name) && !/\.test\.tsx?$/.test(entry.name) ? [file] : [];
  });
}

export function bannerRaiseViolations(file: string, source: string): string[] {
  const sf = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true,
    file.endsWith(".tsx") ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
  const violations: string[] = [];
  const visit = (node: ts.Node) => {
    if (ts.isCallExpression(node) && ts.isIdentifier(node.expression)
      && node.expression.text === "markConflict") {
      const { line } = sf.getLineAndCharacterOfPosition(node.getStart(sf));
      violations.push(`${file}:${line + 1}: markConflict outside the save-result path`);
    }
    ts.forEachChild(node, visit);
  };
  visit(sf);
  return violations;
}

describe("only a backend save refusal may raise a conflict banner", () => {
  it("has no markConflict call outside the save-result path", () => {
    const violations = sourceFiles("src")
      .filter((file) => !ALLOWED.has(file.split(path.sep).join("/")))
      .flatMap((file) => bannerRaiseViolations(file, readFileSync(file, "utf8")));
    expect(violations).toEqual([]);
  });

  it("recognises a banner raised without authority", () => {
    expect(bannerRaiseViolations("src/App.tsx", "markConflict(name);\n"))
      .toEqual(["src/App.tsx:1: markConflict outside the save-result path"]);
  });
});
