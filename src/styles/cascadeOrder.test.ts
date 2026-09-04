// A responsive override must not be written ABOVE the default it overrides.
//
// `@container` and `@media` add no specificity. A rule inside one loses to an
// identical-selector rule written later in the file that sets the same
// property — silently, at every viewport, with the query still looking correct
// in review.
//
// This is not hypothetical. `@container pdfviewer (max-width: 520px)` set
// `.pdf-settings-overflow { display: grid }` and the plain
// `.pdf-settings-overflow { display: none }` default appeared 27 lines later.
// The default won everywhere, so below 520px the PDF reader's Fit width, Fit
// height, Area highlight, Notes and Outline were hidden in the toolbar by that
// same query AND hidden in the More-settings menu: five controls unreachable by
// any pointer in a split pane, a companion pane or a phone. Two native journeys
// (e2e-pdf-logseq, e2e-pdf-routes) sat quarantined for three days recording it
// as a contradictory harness visibility probe.
//
// The rule this enforces: put the base declarations first, the query after.
// scripts/check-pdf-overflow-reachability.mjs asserts the resulting user
// outcome in a real browser; this asserts the shape, everywhere, for free.
import { describe, expect, it } from "vitest";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const stylesDir = path.dirname(fileURLToPath(import.meta.url));

/**
 * Blank `/* … *␘/` to same-length whitespace, keeping newlines so byte offsets
 * and line numbers stay true.
 *
 * Not cosmetic: a selector is matched as `[^{}]+`, which happily swallows the
 * documentation comment above it. Every rule in these stylesheets is commented,
 * so without this the selector reads as "comment text plus selector", matches
 * nothing, and the guard passes on a file that does contain the defect — which
 * is exactly what it did on the first attempt.
 */
function stripComments(css: string): string {
  return css.replace(/\/\*[\s\S]*?\*\//g, (comment) => comment.replace(/[^\n]/g, " "));
}

/** Top-level `@container`/`@media` blocks, with their body and source span. */
function atRuleBlocks(css: string): { start: number; end: number; body: string }[] {
  const blocks: { start: number; end: number; body: string }[] = [];
  const opener = /@(?:container|media)[^{]*\{/g;
  let match: RegExpExecArray | null;
  while ((match = opener.exec(css))) {
    let depth = 1;
    let index = match.index + match[0].length;
    const bodyStart = index;
    while (depth > 0 && index < css.length) {
      if (css[index] === "{") depth += 1;
      else if (css[index] === "}") depth -= 1;
      index += 1;
    }
    blocks.push({ start: match.index, end: index, body: css.slice(bodyStart, index - 1) });
    opener.lastIndex = index;
  }
  return blocks;
}

const RULE = /([^{}]+)\{([^{}]*)\}/g;
const normalize = (selector: string) => selector.split(/\s+/).filter(Boolean).join(" ");
const properties = (declarations: string) =>
  new Set(
    declarations
      .split(";")
      .filter((declaration) => declaration.includes(":"))
      .map((declaration) => declaration.split(":")[0]!.trim())
      .filter(Boolean),
  );

function shadowedOverrides(source: string): string[] {
  const css = stripComments(source);
  const blocks = atRuleBlocks(css);
  // Blank the at-rule spans so "later plain rule" means exactly that.
  const plain = blocks
    .reduce((text, block) => text.slice(0, block.start) + " ".repeat(block.end - block.start) + text.slice(block.end), css);

  const findings: string[] = [];
  for (const block of blocks) {
    RULE.lastIndex = 0;
    for (const [, selector, declarations] of block.body.matchAll(RULE)) {
      const wanted = properties(declarations);
      if (wanted.size === 0) continue;
      const key = normalize(selector);
      for (const later of plain.matchAll(RULE)) {
        // `[^{}]+` swallows the whitespace (and the blanked at-rule span)
        // before a selector, so the match START can sit inside the block we
        // just blanked. Compare from the selector's first real character, or
        // every finding is discarded and this guard passes vacuously.
        const selectorStart = later.index! + (later[1]!.length - later[1]!.trimStart().length);
        if (selectorStart <= block.end) continue;
        if (normalize(later[1]!) !== key) continue;
        const clash = [...properties(later[2]!)].filter((property) => wanted.has(property));
        if (clash.length > 0) {
          const line = (text: string, at: number) => text.slice(0, at).split("\n").length;
          findings.push(
            `${key} sets ${clash.join(", ")} inside an at-rule at line ${line(css, block.start)}, ` +
              `but an identical selector re-declares it at line ${line(css, selectorStart)} — ` +
              "at-rules add no specificity, so the later plain rule wins at every size. " +
              "Move the at-rule block below the base declarations.",
          );
        }
      }
    }
  }
  return findings;
}

describe("stylesheet cascade order", () => {
  for (const file of ["app.css", "theme.css"]) {
    it(`${file}: no responsive override is shadowed by a later identical-selector default`, () => {
      expect(shadowedOverrides(fs.readFileSync(path.join(stylesDir, file), "utf8"))).toEqual([]);
    });
  }

  it("detects the shape it exists to catch", () => {
    const regression = `
@container pdfviewer (max-width: 520px) {
  .pdf-settings-overflow { display: grid; }
}
.pdf-settings-overflow { display: none; gap: 4px; }
`;
    const found = shadowedOverrides(regression);
    expect(found).toHaveLength(1);
    expect(found[0]).toContain(".pdf-settings-overflow sets display");
  });

  it("accepts the correct order", () => {
    const fixed = `
.pdf-settings-overflow { display: none; gap: 4px; }
@container pdfviewer (max-width: 520px) {
  .pdf-settings-overflow { display: grid; }
}
`;
    expect(shadowedOverrides(fixed)).toEqual([]);
  });
});
