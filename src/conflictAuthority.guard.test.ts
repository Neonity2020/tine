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
// `doSave` is the one function allowed to call markConflict — the function, not
// the file. A whole-file allowance would have let a second caller appear inside
// persistence.ts itself, which is exactly where the next one would go. Nothing is
// excluded by filename: `ui.ts` only DEFINES markConflict, and a definition is not
// a call expression, so it needs no exemption and gets none.
const ALLOWED_CALLER = { file: "src/persistence.ts", fn: "doSave" };

function sourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const file = path.join(dir, entry.name);
    if (entry.isDirectory()) return sourceFiles(file);
    return /\.tsx?$/.test(entry.name) && !/\.test\.tsx?$/.test(entry.name) ? [file] : [];
  });
}

function isFunctionLike(node: ts.Node): boolean {
  return ts.isFunctionDeclaration(node)
    || ts.isFunctionExpression(node)
    || ts.isArrowFunction(node)
    || ts.isMethodDeclaration(node)
    || ts.isGetAccessor(node)
    || ts.isSetAccessor(node)
    || ts.isConstructorDeclaration(node);
}

/** Every local name that refers to `markConflict` in this file — the import
 *  itself, an alias on the import, and a plain `const x = markConflict`. Matching
 *  only the literal identifier would let `import { markConflict as raise }` walk
 *  straight past the rule this file exists to hold. */
function localNamesForMarkConflict(sf: ts.SourceFile): {
  names: Set<string>;
  namespaces: Set<string>;
} {
  const names = new Set(["markConflict"]);
  const namespaces = new Set<string>();
  const visit = (node: ts.Node) => {
    if (ts.isImportDeclaration(node) && node.importClause?.namedBindings) {
      const bindings = node.importClause.namedBindings;
      if (ts.isNamedImports(bindings)) {
        for (const element of bindings.elements) {
          if ((element.propertyName ?? element.name).text === "markConflict") {
            names.add(element.name.text);
          }
        }
      } else if (ts.isNamespaceImport(bindings)) {
        // `import * as ui` — any `ui.markConflict(...)` is the same call.
        namespaces.add(bindings.name.text);
      }
    }
    if (ts.isVariableDeclaration(node) && node.initializer) {
      // `const raise = markConflict`, and chains of it.
      if (ts.isIdentifier(node.name) && ts.isIdentifier(node.initializer)
        && names.has(node.initializer.text)) {
        names.add(node.name.text);
      }
      // `const { markConflict: raise } = ui` off a namespace import.
      if (ts.isObjectBindingPattern(node.name) && ts.isIdentifier(node.initializer)
        && namespaces.has(node.initializer.text)) {
        for (const element of node.name.elements) {
          if (((element.propertyName ?? element.name) as ts.Node).getText(sf) === "markConflict"
            && ts.isIdentifier(element.name)) {
            names.add(element.name.text);
          }
        }
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(sf);
  return { names, namespaces };
}

export function bannerRaiseViolations(file: string, source: string): string[] {
  const sf = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true,
    file.endsWith(".tsx") ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
  const { names: raisers, namespaces } = localNamesForMarkConflict(sf);
  const isRaiser = (callee: ts.Expression): boolean =>
    (ts.isIdentifier(callee) && raisers.has(callee.text))
    // `ui.markConflict(...)` through a namespace import.
    || (ts.isPropertyAccessExpression(callee) && callee.name.text === "markConflict"
      && ts.isIdentifier(callee.expression) && namespaces.has(callee.expression.text));
  const violations: string[] = [];
  const allowedFn = file === ALLOWED_CALLER.file ? ALLOWED_CALLER.fn : null;
  const visit = (node: ts.Node, enclosing: string | null) => {
    // Only a NAMED function declaration opens the allowed scope, and every other
    // function-like body closes it. An arrow or anonymous function nested inside
    // `doSave` can be handed to a timer, a promise or an event listener and run
    // long after — with the queue, the store and the banner in a different state
    // — so inheriting doSave's allowance would let the call escape the one place
    // that has the epoch.
    const scope = ts.isFunctionDeclaration(node)
      ? (node.name?.text ?? null)
      : isFunctionLike(node)
        ? null
        : enclosing;
    if (ts.isCallExpression(node) && isRaiser(node.expression)
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
      .flatMap((file) => bannerRaiseViolations(file, readFileSync(file, "utf8")));
    expect(violations).toEqual([]);
  });

  it("does not exempt the file that defines markConflict", () => {
    expect(bannerRaiseViolations("src/ui.ts",
      "export function markConflict(name: string) { setConflicts(name); }\n"
      + "markConflict(\"smuggled\");\n"))
      .toEqual(["src/ui.ts:2: markConflict outside the save-result path"]);
  });

  it("recognises an aliased import of markConflict", () => {
    expect(bannerRaiseViolations("src/App.tsx",
      "import { markConflict as raiseConflict } from \"./ui\";\n"
      + "raiseConflict(name);\n"))
      .toEqual(["src/App.tsx:2: markConflict outside the save-result path"]);
  });

  it("recognises a namespace import and a destructured alias off it", () => {
    expect(bannerRaiseViolations("src/App.tsx",
      "import * as ui from \"./ui\";\n"
      + "ui.markConflict(name);\n"
      + "const { markConflict: raise } = ui;\n"
      + "raise(name);\n"))
      .toEqual([
        "src/App.tsx:2: markConflict outside the save-result path",
        "src/App.tsx:4: markConflict outside the save-result path",
      ]);
  });

  // The stated limit, pinned so nobody reads more into this guard than it
  // delivers. It is a single-file syntactic scan: a module that re-exports
  // markConflict UNDER ANOTHER NAME hides it, because connecting the two needs
  // the module graph. A re-export under the same name is still caught, since the
  // local name is what this file sees. The production scan is green and there is
  // no second caller today; this is the known hole, not a claim of completeness.
  it("cannot see a re-export that renames markConflict", () => {
    expect(bannerRaiseViolations("src/App.tsx",
      "import { raiseConflict } from \"./reexports\";\n"
      + "raiseConflict(name);\n"))
      .toEqual([]);
    // …while the same chain keeping the name is caught.
    expect(bannerRaiseViolations("src/App.tsx",
      "import { markConflict } from \"./reexports\";\n"
      + "markConflict(name);\n"))
      .toEqual(["src/App.tsx:2: markConflict outside the save-result path"]);
  });

  it("does not let a nested closure inherit doSave's allowance", () => {
    const source = "function doSave() {\n"
      + "  markConflict(name);\n"
      + "  const later = () => markConflict(name);\n"
      + "  register(later);\n"
      + "}\n";
    expect(bannerRaiseViolations(ALLOWED_CALLER.file, source)).toEqual([
      `${ALLOWED_CALLER.file}:3: markConflict outside the save-result path`,
    ]);
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
