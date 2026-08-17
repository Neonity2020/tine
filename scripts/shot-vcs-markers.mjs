// VCS merge-marker quarantine surfaces (Concord P1): the banner on an affected
// page and the Settings → Backups & recovery listing. Headless Chromium over the
// mock backend with the `?conflicts` demo flag. Usage (after
// `source scripts/env.sh && npm run build`):
//   node scripts/shot-vcs-markers.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5217;
const OUT = "screenshots";
mkdirSync(OUT, { recursive: true });
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });
const wait = async (u, t = 40) => { for (let i = 0; i < t; i++) { try { const r = await fetch(u); if (r.ok) return; } catch {} await sleep(250); } throw new Error("no server"); };

try {
  await wait(`http://localhost:${PORT}/`);
  const browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 }, deviceScaleFactor: 2 });
  page.on("pageerror", (e) => console.log("pageerror:", String(e).split("\n")[0]));
  await page.goto(`http://localhost:${PORT}/?conflicts`);
  await page.waitForSelector(".page-title", { timeout: 8000 });
  await sleep(400);

  // Open the quarantined demo page ("Tine") via the switcher.
  await page.keyboard.press("Control+k");
  await page.waitForSelector(".switcher-input", { timeout: 3000 });
  await page.locator(".switcher-input").fill("Tine");
  await sleep(400);
  await page.locator(".switcher-row").first().click();
  await page.waitForSelector(".vcs-marker-banner", { timeout: 5000 });
  await sleep(300);
  await page.locator(".page-section").first().screenshot({ path: `${OUT}/vcs-marker-banner.png` });
  console.log("OK   vcs-marker-banner");

  // Settings → Backups & recovery: the "VCS merge conflicts" listing.
  await page.locator('button.icon-btn[title^="Settings"]').first().click();
  await page.waitForSelector(".settings-modal", { timeout: 3000 });
  await page.locator(".settings-nav-item", { hasText: "Backups" }).first().click();
  await page.waitForSelector(".settings-section", { timeout: 3000 });
  await sleep(400);
  const section = page.locator(".settings-section", { hasText: "VCS merge conflicts" });
  await section.scrollIntoViewIfNeeded();
  await sleep(200);
  await page.locator(".settings-modal").screenshot({ path: `${OUT}/vcs-marker-settings.png` });
  console.log("OK   vcs-marker-settings");

  await browser.close();
  console.log("done");
} finally {
  server.kill("SIGTERM");
}
