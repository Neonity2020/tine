import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";
import ts from "typescript";
import { describe, expect, it } from "vitest";
import { PAGE_PROP_SPECS, isSheetCellHidden } from "./editor/properties";

// `PAGE_PROPS_HIDDEN` (src/components/Page.tsx) hides `alias` and `icon` from the
// UNDER-TITLE DISPLAY LIST, because both are already surfaced elsewhere on the
// page: alias as the read-only "aka" chips, icon as the glyph beside the title.
//
// It must never become a filter on the property EDITOR. **Aliases** and **Icon**
// are two of the five preset fields the editor exists to offer (GH #164), so
// reusing this set there would delete exactly the fields the user opened the
// panel for - while looking like tidy reuse, because the name reads as "page
// props hidden" rather than "page props hidden FROM THE UNDER-TITLE LIST".
//
// The editor has its own machine-managed rule, and it answers a different
// question: `isSheetCellHidden` (id, collapsed, logseq.order-list-type, tine.*).
// Imitate that, in src/components/PageProps.tsx. If you are here because this
// failed, the fix is a separate predicate, not a wider import. This test is the
// reminder, not a style rule. (Prevention discipline: guard channel, I-12.)
//
// This scans USES, not mentions. `src/render/block.ts` is the blessed exemplar:
// its RENDER_HIDDEN_PROPS header names PAGE_PROPS_HIDDEN precisely to say the
// sets are "Deliberately SEPARATE (different concepts - do not merge)". Prose
// that teaches the boundary is the opposite of a violation, so an identifier
// scan is the right instrument and a text scan is not - the first version of
// this guard used text and failed on that very comment.
const OWNER = "src/components/Page.tsx";
const SET_NAME = "PAGE_PROPS_HIDDEN";

function sourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const file = path.join(dir, entry.name);
    if (entry.isDirectory()) return sourceFiles(file);
    return /\.tsx?$/.test(entry.name) && !/\.test\.tsx?$/.test(entry.name) ? [file] : [];
  });
}

/** Every place outside the owning file that REFERS to the display-only hidden
 *  set. Identifiers only: a comment naming it is documentation, not reuse. */
export function hiddenSetUses(file: string, source: string): string[] {
  if (file === OWNER) return [];
  const sf = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true,
    file.endsWith(".tsx") ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
  const uses: string[] = [];
  const visit = (node: ts.Node) => {
    if (ts.isIdentifier(node) && node.text === SET_NAME) {
      const { line } = sf.getLineAndCharacterOfPosition(node.getStart(sf));
      uses.push(`${file}:${line + 1}: ${SET_NAME} is the under-title display filter; the editor must not reuse it`);
    }
    ts.forEachChild(node, visit);
  };
  visit(sf);
  return uses;
}

describe("the under-title display filter never becomes the property editor's filter", () => {
  it("is referred to in exactly one file", () => {
    const uses = sourceFiles("src")
      .map((file) => file.split(path.sep).join("/"))
      .flatMap((file) => hiddenSetUses(file, readFileSync(file, "utf8")));
    expect(uses).toEqual([]);
  });

  // Positive controls: a scan that cannot fail proves nothing. The first is the
  // exact regression this guard exists to catch - the editor importing the set.
  it("catches the editor importing the display filter", () => {
    expect(hiddenSetUses("src/components/PageProps.tsx",
      "import { PAGE_PROPS_HIDDEN } from \"./Page\";\n"))
      .toEqual([
        "src/components/PageProps.tsx:1: PAGE_PROPS_HIDDEN is the under-title display filter; the editor must not reuse it",
      ]);
  });

  it("catches a bare use even without an import line", () => {
    expect(hiddenSetUses("src/components/PageProps.tsx",
      "const rows = keys.filter((k) => !PAGE_PROPS_HIDDEN.has(k));\n"))
      .toEqual([
        "src/components/PageProps.tsx:1: PAGE_PROPS_HIDDEN is the under-title display filter; the editor must not reuse it",
      ]);
  });

  // The distinction the first version of this guard got wrong, pinned so it is
  // not re-broken: prose that names the set to explain the separation is exactly
  // what src/render/block.ts:22 does, and it must stay legal.
  it("does not fire on a comment that documents the separation", () => {
    expect(hiddenSetUses("src/render/block.ts",
      "// Deliberately SEPARATE (do not merge): components/Page.tsx PAGE_PROPS_HIDDEN.\n"
      + "export const RENDER_HIDDEN_PROPS = new Set([\"id\"]);\n"))
      .toEqual([]);
  });

  it("does not fire on the file that owns the set", () => {
    expect(hiddenSetUses(OWNER, `const ${SET_NAME} = new Set(["alias"]);\n`)).toEqual([]);
  });

  // The reason the rule exists, asserted rather than only described: the two sets
  // genuinely overlap, so reuse would silently remove real preset fields.
  it("keeps alias and icon as editable presets even though the page list hides them", () => {
    const presets = PAGE_PROP_SPECS.map((spec) => spec.key);
    expect(presets).toContain("alias");
    expect(presets).toContain("icon");
    // And the editor's own hidden rule does NOT hide them - it answers the
    // different question of which keys are machine-managed.
    expect(isSheetCellHidden("alias")).toBe(false);
    expect(isSheetCellHidden("icon")).toBe(false);
  });
});
