import { waitForHttpServer } from "./e2e-capabilities.mjs";
// Self-verification shot for the fourth (merged) outcome: the in-page resolver
// with mock row 4 (a both-changed row carrying a merged proposal). Three shots:
// desktop collapsed, desktop expanded, and a phone-width collapsed layout.
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5217;
const server = spawn("./node_modules/.bin/vite", ["preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });
async function openConflictPage(page) {
  await page.goto(`http://localhost:${PORT}/?conflicts`);
  await page.waitForSelector(".ls-block", { timeout: 5000 });
  await page.waitForSelector(".conflict-queue-badge", { timeout: 5000 });
  // Walk the queue until the SYNC-COPY page (the one with the merged row) is up.
  for (let i = 0; i < 4; i++) {
    await page.locator(".conflict-queue-badge").click();
    await sleep(400);
    if (await page.locator('.sync-merge-cell.merged').count()) break;
  }
  await page.waitForSelector(".sync-merge-cell.merged", { state: "attached", timeout: 5000 });
  await page.locator(".sync-merge-cell.merged").first().scrollIntoViewIfNeeded();
  await sleep(200);
}
try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 60, 250, { failureMessage: "server did not start" });
  const browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu"] });

  const desktop = await browser.newPage({ viewport: { width: 1180, height: 1000 }, deviceScaleFactor: 2 });
  const errors = [];
  desktop.on("pageerror", (e) => errors.push(String(e)));
  await openConflictPage(desktop);
  await desktop.locator(".page-conflict").screenshot({ path: "/tmp/shot-merged-collapsed.png" });
  // Expand the merged row's full bodies.
  await desktop.locator(".sync-merge-expand").first().click();
  await sleep(300);
  await desktop.locator(".page-conflict").screenshot({ path: "/tmp/shot-merged-expanded.png" });

  const phone = await browser.newPage({ viewport: { width: 400, height: 900 }, deviceScaleFactor: 2 });
  phone.on("pageerror", (e) => errors.push(String(e)));
  await openConflictPage(phone);
  // At phone width the sidebar is an overlay and the badge click leaves it
  // open; dismiss it so the shot shows the panel itself.
  await phone.keyboard.press("Escape");
  await sleep(200);
  await phone.locator(".sync-merge-cell.merged").first().scrollIntoViewIfNeeded();
  await sleep(200);
  await phone.locator(".page-conflict").screenshot({ path: "/tmp/shot-merged-phone.png" });

  await browser.close();
  server.kill("SIGKILL");
  if (errors.length) {
    console.error("PAGE ERRORS:\n" + errors.join("\n"));
    process.exit(1);
  }
  process.exit(0);
} catch (e) {
  console.error(String(e));
  server.kill("SIGKILL");
  process.exit(1);
}
