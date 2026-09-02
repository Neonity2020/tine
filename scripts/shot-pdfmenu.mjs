import { waitForHttpServer } from "./e2e-capabilities.mjs";
// Open a page, right-click its title, screenshot the context menu showing the
// new "Export to PDF…" entry.
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5209;
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });
try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 60, 250, { failureMessage: "server did not start" });
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1000, height: 800 } });
  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForSelector(".ls-block", { timeout: 5000 });
  // Navigate to a real page (click a page ref in the mock content).
  const ref = page.locator("a.page-ref, a.tag, .page-title").first();
  const title = page.locator(".page-title").first();
  if (await title.count()) {
    await title.click({ button: "right" });
  } else {
    await ref.click();
    await sleep(300);
    await page.locator(".page-title").first().click({ button: "right" });
  }
  await page.waitForSelector(".ctx-menu, .context-menu, [class*='ctx']", { timeout: 3000 });
  await sleep(200);
  await page.screenshot({ path: "screenshots/pdfmenu.png" });
  await browser.close();
  server.kill("SIGKILL");
  process.exit(0);
} catch (e) {
  console.error(String(e));
  server.kill("SIGKILL");
  process.exit(1);
}
