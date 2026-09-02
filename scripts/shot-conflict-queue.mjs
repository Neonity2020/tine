// Concord P4 visual check: the calm sidebar badge and the in-page conflict
// resolver on a marker-bearing page. Usage: npm run build && node scripts/shot-conflict-queue.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5217;
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });
async function waitForServer(url, tries = 60) {
  for (let i = 0; i < tries; i++) {
    try {
      if ((await fetch(url)).ok) return;
    } catch {
      // not up yet
    }
    await sleep(250);
  }
  throw new Error("server did not start");
}
try {
  await waitForServer(`http://localhost:${PORT}/`);
  const browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu"] });
  const page = await browser.newPage({ viewport: { width: 1180, height: 900 }, deviceScaleFactor: 2 });
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  // The mock only surfaces conflicts behind this gate, so ordinary screenshots
  // and the marketing demo stay clean.
  await page.goto(`http://localhost:${PORT}/?conflicts`);
  await page.waitForSelector(".ls-block", { timeout: 5000 });
  // The badge is the whole global surface: quiet, in the sidebar footer.
  await page.waitForSelector(".conflict-queue-badge", { timeout: 5000 });
  await page.locator(".conflict-queue-badge").screenshot({ path: "/tmp/shot-conflict-badge.png" });
  // Clicking it walks to a conflicted page; walk until the marker one is up.
  for (let i = 0; i < 3; i++) {
    await page.locator(".conflict-queue-badge").click();
    await sleep(400);
    if (await page.locator(".page-conflict").count()) break;
  }
  await page.waitForSelector(".page-conflict", { timeout: 5000 });
  const paneBox = await page.locator(".main-content").boundingBox();
  const conflictBox = await page.locator(".page-conflict").boundingBox();
  const contentPadding = await page.locator(".main-content-inner").evaluate((el) => {
    const style = getComputedStyle(el);
    return parseFloat(style.paddingLeft) + parseFloat(style.paddingRight);
  });
  if (!paneBox || !conflictBox) throw new Error("could not measure Concord geometry");
  // Concord should consume all usable pane width without requiring the
  // unrelated wide-mode toggle. Keep the pane's deliberate content gutters.
  const expectedWidth = paneBox.width - contentPadding;
  if (Math.abs(conflictBox.width - expectedWidth) > 2) {
    throw new Error(
      `Concord stayed reading-width capped: panel=${conflictBox.width}, expected=${expectedWidth}`,
    );
  }
  await page.locator(".page-conflict").screenshot({ path: "/tmp/shot-conflict-inpage.png" });
  await page.screenshot({ path: "/tmp/shot-conflict-queue.png" });
  // Concord P5: the Settings surfaces are the INVENTORY only — no merge modal.
  await page.locator("body").click({ position: { x: 5, y: 5 } });
  await page.keyboard.press("t");
  await page.keyboard.press("s");
  await sleep(700);
  const backupsTab = page.locator("button", { hasText: "Backups & recovery" }).first();
  if (await backupsTab.count()) {
    await backupsTab.click();
    await sleep(800);
    await page.screenshot({ path: "/tmp/shot-conflict-settings.png" });
    const panel = page.locator(".sync-conflict-row").first();
    if (await panel.count()) {
      await panel.screenshot({ path: "/tmp/shot-conflict-inventory.png" });
    }
  } else {
    console.error("could not open Settings for the inventory shot");
  }
  if (errors.length) console.error("page errors:", errors);
  await browser.close();
  server.kill("SIGKILL");
  process.exit(errors.length ? 1 : 0);
} catch (e) {
  console.error(String(e));
  server.kill("SIGKILL");
  process.exit(1);
}
