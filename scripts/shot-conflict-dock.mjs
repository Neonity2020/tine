// Self-verification shots for the conflict dock (spec:
// tine-agents/specs/concord-conflict-dock.md): the full panel at the top of a
// long page, the slim pinned bar once the panel scrolls out of view, and the
// unrolled-in-place sheet — at desktop and phone widths. Full-viewport shots,
// so the bar's alignment with the pane (not covering the sidebar) is visible.
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5218;
const server = spawn("./node_modules/.bin/vite", ["preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });
async function waitForServer(url, tries = 60) {
  for (let i = 0; i < tries; i++) {
    try { if ((await fetch(url)).ok) return; } catch {}
    await sleep(250);
  }
  throw new Error("server did not start");
}
async function openConflictPage(page, { phone = false } = {}) {
  await page.goto(`http://localhost:${PORT}/?conflicts`);
  await page.waitForSelector(".ls-block", { timeout: 5000 });
  await page.waitForSelector(".conflict-queue-badge", { timeout: 5000 });
  // Walk the queue until the sync-copy page (Project Plan — the long one) is up.
  for (let i = 0; i < 4; i++) {
    await page.locator(".conflict-queue-badge").click();
    await sleep(400);
    if (await page.locator(".sync-merge-cell.merged").count()) break;
  }
  await page.waitForSelector(".page-conflict", { state: "attached", timeout: 5000 });
  if (phone) {
    // At phone width the sidebar is an overlay; dismiss it.
    await page.keyboard.press("Escape");
    await sleep(200);
  }
}
const scrollPane = (page, top) =>
  page.evaluate((y) => {
    const scroller = document.querySelector(".main-content");
    scroller.scrollTop = y;
  }, top);

try {
  await waitForServer(`http://localhost:${PORT}/`);
  const browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu"] });
  const errors = [];

  const desktop = await browser.newPage({ viewport: { width: 1180, height: 800 }, deviceScaleFactor: 2 });
  desktop.on("pageerror", (e) => errors.push(String(e)));
  await openConflictPage(desktop);
  await scrollPane(desktop, 0);
  await sleep(400);
  if (await desktop.locator(".page-conflict-dock").count()) {
    throw new Error("dock bar visible while the panel is at the top");
  }
  await desktop.screenshot({ path: "/tmp/shot-dock-top.png" });

  await scrollPane(desktop, 2500);
  await sleep(500);
  await desktop.waitForSelector(".page-conflict-dockbar", { timeout: 5000 });
  await desktop.screenshot({ path: "/tmp/shot-dock-bar.png" });

  await desktop.locator(".page-conflict-dockbar").click();
  await sleep(400);
  await desktop.waitForSelector(".page-conflict-sheet .page-conflict", { timeout: 5000 });
  await desktop.screenshot({ path: "/tmp/shot-dock-sheet.png" });

  // Scrolling back to the top must dismiss the dock and restore the inline panel.
  await scrollPane(desktop, 0);
  await sleep(500);
  if (await desktop.locator(".page-conflict-dock").count()) {
    throw new Error("dock still present after scrolling back to the top");
  }
  await desktop.waitForSelector(".page-conflict-slot .page-conflict", { timeout: 5000 });

  const phone = await browser.newPage({ viewport: { width: 400, height: 900 }, deviceScaleFactor: 2 });
  phone.on("pageerror", (e) => errors.push(String(e)));
  await openConflictPage(phone, { phone: true });
  await scrollPane(phone, 3000);
  await sleep(500);
  await phone.waitForSelector(".page-conflict-dockbar", { timeout: 5000 });
  await phone.screenshot({ path: "/tmp/shot-dock-phone-bar.png" });
  await phone.locator(".page-conflict-dockbar").click();
  await sleep(400);
  await phone.waitForSelector(".page-conflict-sheet .page-conflict", { timeout: 5000 });
  await phone.screenshot({ path: "/tmp/shot-dock-phone-sheet.png" });

  await browser.close();
  server.kill("SIGKILL");
  if (errors.length) {
    console.error("PAGE ERRORS:\n" + errors.join("\n"));
    process.exit(1);
  }
  console.log("shots: /tmp/shot-dock-{top,bar,sheet,phone-bar,phone-sheet}.png");
} catch (e) {
  server.kill("SIGKILL");
  console.error(e);
  process.exit(1);
}
