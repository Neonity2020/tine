// Visual verification for packet C-5b's tier-3 toast and Deleted pages dock.
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5231;
const TOAST = "/tmp/packetc5b-absence-toast.png";
const PANEL = "/tmp/packetc5b-absence-panel.png";
const server = spawn("./node_modules/.bin/vite", [
  "preview",
  "--host", "127.0.0.1",
  "--port", String(PORT),
  "--strictPort",
], { stdio: "ignore" });

async function waitForServer(url) {
  for (let attempt = 0; attempt < 60; attempt += 1) {
    try {
      if ((await fetch(url)).ok) return;
    } catch {
      // Preview is still starting.
    }
    await sleep(250);
  }
  throw new Error("preview server did not start");
}

try {
  const url = `http://127.0.0.1:${PORT}/?absence-sweeps`;
  await waitForServer(url);
  const browser = await chromium.launch({
    args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"],
  });
  const page = await browser.newPage({ viewport: { width: 1280, height: 860 }, deviceScaleFactor: 2 });
  const errors = [];
  page.on("pageerror", (error) => errors.push(String(error)));
  page.on("console", (message) => message.type() === "error" && errors.push(message.text()));

  await page.goto(url);
  await page.waitForSelector(".absence-sweep-dock", { timeout: 8_000 });
  await page.waitForSelector(".toast-warn", { timeout: 8_000 });
  const unrelatedToasts = page.locator(".toast").filter({ hasNotText: "pages were deleted together" });
  while (await unrelatedToasts.count()) await unrelatedToasts.first().locator(".toast-close").click();
  await sleep(300);
  await page.screenshot({ path: TOAST });

  await page.locator(".absence-sweep-dock").click();
  await page.waitForSelector(".absence-sweep-panel", { timeout: 5_000 });
  await sleep(300);
  await page.screenshot({ path: PANEL });

  if (errors.length) throw new Error(`browser errors: ${errors.join("; ")}`);
  await browser.close();
  console.log(`shots: ${TOAST}, ${PANEL}`);
} finally {
  server.kill("SIGTERM");
}
