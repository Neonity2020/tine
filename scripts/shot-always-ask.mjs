import { waitForHttpServer } from "./e2e-capabilities.mjs";
// Concord P5 visual check: the "always ask" bar. The policy is OFF by default,
// so the demo gate turns it on and drives one external change through the
// ordinary handler. Usage: npm run build && node scripts/shot-always-ask.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5233;
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });
try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 60, 250, { failureMessage: "server did not start" });
  const browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu"] });
  const page = await browser.newPage({ viewport: { width: 1180, height: 900 }, deviceScaleFactor: 2 });
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  page.on("console", (m) => { if (m.type() === "error") errors.push(m.text()); });
  await page.goto(`http://localhost:${PORT}/?alwaysask`);
  await page.waitForSelector(".ls-block", { timeout: 5000 });
  // Open a real page, then hold one external change for it through the demo hook.
  await page.locator(".page-ref, a.page-ref, .ls-block a").first().click();
  await sleep(600);
  const title = (await page.locator(".page-title, .page-title-row").first().innerText())
    .split("\n")[0]
    .trim();
  await page.evaluate((name) => {
    const w = window;
    w.__tineHoldExternalChange?.(name);
  }, title);
  await page.waitForSelector(".external-change-bar", { timeout: 5000 });
  await page.locator(".external-change-bar").screenshot({ path: "/tmp/shot-always-ask-bar.png" });
  await page.screenshot({ path: "/tmp/shot-always-ask.png" });
  if (errors.length) console.error("page errors:", errors);
  await browser.close();
  server.kill("SIGKILL");
  process.exit(errors.length ? 1 : 0);
} catch (e) {
  console.error(String(e));
  server.kill("SIGKILL");
  process.exit(1);
}
