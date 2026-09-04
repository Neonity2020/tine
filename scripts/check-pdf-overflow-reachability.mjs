// The PDF reader's lower-priority tools must be reachable by pointer at EVERY
// pane width — inline in a roomy toolbar, and from the More-settings menu once
// the toolbar collapses. This is the "one or the other, never neither" contract.
//
// It was neither. `@container pdfviewer (max-width: 520px)` set
// `.pdf-settings-overflow { display: grid }`, but the plain
// `.pdf-settings-overflow { display: none }` default was written LATER in
// app.css at equal specificity, so it won the cascade at every width. Below
// 520px the inline copies were correctly hidden and the menu copies were hidden
// too: Fit width, Fit height, Area highlight, Notes and Outline were unreachable
// by any pointer in a narrow reader pane — a split pane, a companion pane, or a
// phone. That is what the quarantined pdf-logseq (Outline) and pdf-routes
// (Notes) journeys had been reporting as a contradictory visibility probe.
//
// jsdom applies no CSS layout and no container queries, so this is the only
// layer that can see it. The assertion is the user outcome, not the constants:
// at each sampled width at least one copy of each tool is hit-testable.
import { chromium } from "playwright";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const css = ["theme.css", "app.css"]
  .map((file) => fs.readFileSync(path.join(root, "src/styles", file), "utf8"))
  .join("\n");

// The five tools that live in both places. Their toolbar copies carry
// `.pdf-overflow-action`; their menu copies are the `.pdf-settings-overflow`
// buttons, which exist only while the More-settings menu is open.
const TOOLS = ["Fit width", "Fit height", "Area highlight", "Notes", "Outline"];

const inlineButton = (label) =>
  label === "Notes"
    ? `<button class="pdf-notes-btn pdf-overflow-action" title="${label}">${label}</button>`
    : `<button class="icon-btn pdf-overflow-action" title="${label}">${label}</button>`;

const fixture = (width) => `
  <div class="pdf-viewer" data-theme="light" style="width:${width}px;height:600px">
    <div class="pdf-toolbar">
      ${TOOLS.map(inlineButton).join("\n      ")}
      <button class="icon-btn" title="More settings">&#8942;</button>
    </div>
    <div class="pdf-settings-menu" role="dialog" aria-label="PDF settings">
      <div class="pdf-settings-overflow" aria-label="Reader tools">
        ${TOOLS.map((label) => `<button type="button">${label}</button>`).join("\n        ")}
      </div>
      <div class="pdf-settings-heading">Theme</div>
    </div>
  </div>`;

// Around the 520px boundary plus the widths a phone, a companion pane and a
// full window actually produce.
const WIDTHS = [320, 400, 500, 519, 520, 521, 560, 700, 1100];

const browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-gpu"] });
try {
  const page = await browser.newPage({ viewport: { width: 1400, height: 800 } });
  const unreachable = [];
  const report = [];

  for (const width of WIDTHS) {
    await page.setContent(`<!doctype html><style>${css}</style>${fixture(width)}`);
    const row = await page.evaluate((tools) => {
      const hittable = (el) => {
        if (!el) return false;
        const rect = el.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) return false;
        const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
        return !!hit && (hit === el || el.contains(hit));
      };
      const menuButtons = [...document.querySelectorAll(".pdf-settings-overflow button")];
      return tools.map((label) => ({
        label,
        inline: hittable(document.querySelector(`.pdf-toolbar button[title="${label}"]`)),
        menu: hittable(menuButtons.find((button) => button.textContent.trim() === label)),
      }));
    }, TOOLS);

    for (const tool of row) {
      if (!tool.inline && !tool.menu) unreachable.push(`${tool.label} at ${width}px`);
    }
    report.push(`${width}px: ${row.map((t) => `${t.label}=${t.inline ? "toolbar" : t.menu ? "menu" : "NOWHERE"}`).join(", ")}`);
  }

  if (unreachable.length > 0) {
    throw new Error(
      `PDF reader tools are unreachable by pointer: ${unreachable.join("; ")}\n${report.join("\n")}\n` +
        "Check that the @container pdfviewer block in app.css still comes AFTER " +
        ".pdf-settings-overflow's own display declaration; at equal specificity the later rule wins.",
    );
  }

  console.log(`PASS: every PDF reader tool is pointer-reachable at all ${WIDTHS.length} sampled widths\n${report.join("\n")}`);
} finally {
  await browser.close();
}
