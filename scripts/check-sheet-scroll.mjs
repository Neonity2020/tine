// GH #449: horizontally scrolling a wide sheet dragged the "+ Add row" label
// across the viewport.
//
// `.sheet-table` is `display: grid; width: max-content` inside `.sheet-scroll`
// (`overflow-x: auto`), and the add-row control is a `grid-column: 1 / -1` item.
// Centred, its label sat at the middle of the FULL table width — off-screen to
// the right on a wide table, sliding past as you scrolled, then off the left.
// The row still spans the grid (that is its hit area); only its content is
// pinned, by `.sheet-ghost-sticky`, the same mechanism `.sheet-sticky-left`
// uses for the title column.
//
// The user contract is geometric, which is why this is a real layout check and
// not a CSS-text assertion: the label stays inside the scrollport at both
// scroll extremes. Its exact offset is not a contract.
import { chromium } from "playwright";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const css = ["theme.css", "app.css"]
  .map((file) => fs.readFileSync(path.join(root, "src/styles", file), "utf8"))
  .join("\n");

const COLUMNS = 6;
const COLUMN_WIDTH = 220;
const PORT_WIDTH = 420;

const markup = `
  <div class="block-sheet-container" style="width:${PORT_WIDTH}px">
    <div class="sheet-scroll" style="width:${PORT_WIDTH}px">
      <div class="sheet-table" style="grid-template-columns:repeat(${COLUMNS}, ${COLUMN_WIDTH}px)">
        ${Array.from({ length: COLUMNS * 3 }, (_, i) => `<div class="sheet-cell">cell ${i}</div>`).join("")}
        <button class="sheet-add-row-ghost">
          <span class="sheet-ghost-sticky">
            <span class="sheet-ghost-plus">+</span>
            <span class="sheet-ghost-label">Add row</span>
          </span>
        </button>
      </div>
    </div>
  </div>`;

const browser = await chromium.launch({ headless: true, args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
try {
  const page = await browser.newPage({ viewport: { width: 560, height: 400 } });
  await page.setContent(`<!doctype html><style>${css}</style>${markup}`);

  const measure = (scrollLeft) =>
    page.evaluate((left) => {
      const scroller = document.querySelector(".sheet-scroll");
      scroller.scrollLeft = left;
      const port = scroller.getBoundingClientRect();
      const label = document.querySelector(".sheet-ghost-label").getBoundingClientRect();
      return {
        scrollLeft: scroller.scrollLeft,
        overflowing: scroller.scrollWidth > scroller.clientWidth + 1,
        left: Math.round(label.left - port.left),
        right: Math.round(label.right - port.left),
        portWidth: Math.round(port.width),
      };
    }, scrollLeft);

  const atStart = await measure(0);
  const atEnd = await measure(1e6);

  if (!atStart.overflowing) {
    throw new Error(`fixture does not overflow, so it cannot prove anything: ${JSON.stringify(atStart)}`);
  }
  if (atEnd.scrollLeft <= atStart.scrollLeft) {
    throw new Error(`the sheet did not scroll horizontally: ${JSON.stringify({ atStart, atEnd })}`);
  }
  for (const [name, m] of [["unscrolled", atStart], ["scrolled to the end", atEnd]]) {
    if (m.left < 0 || m.right > m.portWidth) {
      throw new Error(`"Add row" leaves the sheet viewport when ${name}: ${JSON.stringify(m)}`);
    }
  }
  if (Math.abs(atEnd.left - atStart.left) > 1) {
    throw new Error(`"Add row" travels with the table instead of staying put: ${JSON.stringify({ atStart, atEnd })}`);
  }
  await page.screenshot({ path: "/tmp/sheet-scroll-addrow.png" });
  console.log(
    `PASS: "Add row" holds at ${atStart.left}px in a ${atStart.portWidth}px sheet viewport across ${atEnd.scrollLeft}px of horizontal scroll`,
  );
} finally {
  await browser.close();
}
