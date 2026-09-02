import { waitForHttpServer } from "./e2e-capabilities.mjs";
// Self-verification shot for a duplicate journal day resolved at the page
// (audit-residue program item 5). Before this, a duplicate day was not a
// conflict object at all: no badge, no dock, no in-page surface — only a sticky
// startup toast pointing at Settings. The shot must show BOTH halves of what
// replaced it: the row-by-row choices (so Merge is implicit) and the per-file
// actions including Rename.
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5219;
const server = spawn("./node_modules/.bin/vite", ["preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });

try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 60, 250, { failureMessage: "server did not start" });
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  await page.goto(`http://localhost:${PORT}/?conflicts`);
  await page.waitForSelector(".conflict-queue-badge", { timeout: 5000 });
  // Walk the queue to the duplicate-journal object.
  let found = false;
  for (let i = 0; i < 6; i++) {
    await page.locator(".conflict-queue-badge").click();
    await sleep(450);
    if (await page.locator("[data-journal-conflict]").count()) { found = true; break; }
  }
  if (!found) throw new Error("never reached the duplicate-journal object");
  await page.waitForSelector(".page-conflict", { state: "attached", timeout: 5000 });
  await sleep(300);
  await page.screenshot({ path: "test-results/duplicate-journal-day.png", fullPage: false });
  console.log("wrote test-results/duplicate-journal-day.png");
  await browser.close();
} finally {
  server.kill();
}
