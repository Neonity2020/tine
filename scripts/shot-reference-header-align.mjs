import { waitForHttpServer } from "./e2e-capabilities.mjs";
// GH #475: the Linked and Unlinked References copy buttons must sit on the same
// right edge. jsdom applies no layout, so this measures the REAL engine over the
// real built app rather than a model of the CSS.
// Usage: source scripts/env.sh && npm run build && node scripts/shot-reference-header-align.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5213;
const OUT = "screenshots";
mkdirSync(OUT, { recursive: true });
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });

let failed = false;
try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 40, 250, { failureMessage: "server did not start" });
  const browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
  const page = await browser.newPage({ viewport: { width: 1100, height: 900 }, deviceScaleFactor: 2 });
  page.on("pageerror", (e) => console.log("pageerror:", String(e).split("\n")[0]));

  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForSelector(".page-title", { timeout: 8000 });
  // Both sections need a routed named page, and Linked References renders only
  // when the page actually HAS backlinks — so follow a real [[link]] from the
  // feed: whatever it points at is referenced by the block we came from.
  await page.waitForSelector(".page-ref", { timeout: 8000 });
  await page.locator(".page-ref").first().click();
  await sleep(600);
  console.log("title:", await page.locator(".page-title").first().textContent());
  console.log("linked:", await page.locator(".linked-references").count(), "unlinked:", await page.locator(".unlinked-references").count());
  await page.waitForSelector(".linked-references .references-header", { timeout: 8000 });
  await page.waitForSelector(".unlinked-references .references-header", { timeout: 8000 });
  await sleep(400);

  const measure = async (section) => {
    const header = page.locator(`${section} .references-header`).first();
    const copy = page.locator(`${section} .reference-export-toggle`).first();
    const [headerBox, copyBox] = [await header.boundingBox(), await copy.boundingBox()];
    return {
      section,
      copyRight: Math.round(copyBox.x + copyBox.width),
      headerRight: Math.round(headerBox.x + headerBox.width),
      insetFromRight: Math.round(headerBox.x + headerBox.width - (copyBox.x + copyBox.width)),
    };
  };

  const linked = await measure(".linked-references");
  const unlinked = await measure(".unlinked-references");
  console.log(JSON.stringify({ linked, unlinked }, null, 2));

  const drift = Math.abs(linked.copyRight - unlinked.copyRight);
  if (drift > 1) {
    failed = true;
    console.log(`FAIL  copy buttons differ by ${drift}px (linked ${linked.copyRight}, unlinked ${unlinked.copyRight})`);
  } else {
    console.log(`OK    both copy buttons end at x=${linked.copyRight}`);
  }

  const shot = async (section, name) => {
    const header = page.locator(`${section} .references-header`).first();
    await header.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await sleep(150);
    const box = await header.boundingBox();
    await page.screenshot({
      path: `${OUT}/${name}.png`,
      clip: { x: box.x - 8, y: box.y - 8, width: box.width + 16, height: box.height + 16 },
    });
  };
  await shot(".linked-references", "ref-header-linked");
  await shot(".unlinked-references", "ref-header-unlinked");
  console.log(`shots: ${OUT}/ref-header-linked.png ${OUT}/ref-header-unlinked.png`);

  await browser.close();
} finally {
  server.kill();
}
process.exit(failed ? 1 : 0);
