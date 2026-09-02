import { waitForHttpServer } from "./e2e-capabilities.mjs";
// Render the kitchen-sink parity page and screenshot it (rendering parity net).
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5195;
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });
try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 60, 250, { failureMessage: "server did not start" });
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 820, height: 1400 } });
  const errors = [];
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForSelector(".ls-block", { timeout: 5000 });
  // Navigate to the kitchen-sink page via the quick switcher.
  await page.keyboard.press("Control+k");
  await page.waitForSelector(".switcher-input", { timeout: 3000 });
  await page.locator(".switcher-input").fill("kitchen");
  await sleep(400);
  await page.locator(".switcher-row").first().click();
  await page.waitForSelector(".page-title, .ls-block", { timeout: 3000 });
  await sleep(600);
  await page.screenshot({ path: "screenshots/kitchen-sink.png", fullPage: true });
  console.log(errors.length ? "ERRORS:\n" + errors.join("\n") : "no console errors");
  await browser.close();
  server.kill("SIGKILL");
  process.exit(0);
} catch (e) {
  console.error(String(e));
  server.kill("SIGKILL");
  process.exit(1);
}
