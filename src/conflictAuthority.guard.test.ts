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
// `ui.ts` DEFINES markConflict, and `doSave` is the one function allowed to call
// it — the function, not the file. A whole-file allowance would have let a second
// caller appear inside persistence.ts itself, which is exactly where the next one
// would go.
const DEFINING_FILE = "src/ui.ts";
const ALLOWED_CALLER = { file: "src/persistence.ts", fn: "doSave" };

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
  const allowedFn = file === ALLOWED_CALLER.file ? ALLOWED_CALLER.fn : null;
  const visit = (node: ts.Node, enclosing: string | null) => {
    const scope = ts.isFunctionDeclaration(node) && node.name
      ? node.name.text
      : enclosing;
    if (ts.isCallExpression(node) && ts.isIdentifier(node.expression)
      && node.expression.text === "markConflict"
      && !(allowedFn !== null && scope === allowedFn)) {
      const { line } = sf.getLineAndCharacterOfPosition(node.getStart(sf));
      violations.push(`${file}:${line + 1}: markConflict outside the save-result path`);
    }
    ts.forEachChild(node, (child) => visit(child, scope));
  };
  visit(sf, null);
  return violations;
}

describe("only a backend save refusal may raise a conflict banner", () => {
  it("has no markConflict call outside the save-result path", () => {
    const violations = sourceFiles("src")
      .map((file) => file.split(path.sep).join("/"))
      .filter((file) => file !== DEFINING_FILE)
      .flatMap((file) => bannerRaiseViolations(file, readFileSync(file, "utf8")));
    expect(violations).toEqual([]);
  });

  it("still catches a second caller inside the allowed file", () => {
    const source = "function doSave() { markConflict(name); }\n"
      + "export function reconcileExternalChange() { markConflict(name); }\n";
    expect(bannerRaiseViolations(ALLOWED_CALLER.file, source)).toEqual([
      `${ALLOWED_CALLER.file}:2: markConflict outside the save-result path`,
    ]);
  });

  it("recognises a banner raised without authority", () => {
    expect(bannerRaiseViolations("src/App.tsx", "markConflict(name);\n"))
      .toEqual(["src/App.tsx:1: markConflict outside the save-result path"]);
  });
});
