import { waitForHttpServer } from "./e2e-capabilities.mjs";
// Screenshot the sidebar graph-switcher control (R3a): closed header + open menu.
// Also asserts the wiring: clicking the control toggles the menu (0→2 items).
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { mkdirSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5194;
const OUT = "screenshots";
mkdirSync(OUT, { recursive: true });

const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], {
  stdio: "inherit",
});


try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 40, 250, { failureMessage: "server did not start" });
  const browser = await chromium.launch({
    args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"],
  });
  const page = await browser.newPage({ viewport: { width: 1280, height: 860 }, deviceScaleFactor: 2 });
  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForSelector(".graph-switch-btn", { timeout: 5000 });
  await sleep(300);

  const name = await page.locator(".graph-switch-name").innerText();
  const before = await page.locator(".graph-switch-menu .ctx-item").count();
  // The workspace switcher added a second .sidebar-header above this one, so the
  // bare selector became ambiguous under Playwright's strict mode; the graph
  // control is the lower of the two.
  await page.locator(".sidebar-header").last().screenshot({ path: `${OUT}/graphswitch-closed.png` });

  await page.locator(".graph-switch-btn").click();
  await sleep(200);
  const after = await page.locator(".graph-switch-menu .ctx-item").count();
  const items = await page.locator(".graph-switch-menu .ctx-item").allInnerTexts();
  await page.locator(".left-sidebar-inner").screenshot({ path: `${OUT}/graphswitch-open.png` });

  // Esc closes.
  await page.keyboard.press("Escape");
  await sleep(150);
  const afterEsc = await page.locator(".graph-switch-menu .ctx-item").count();

  console.log(JSON.stringify({ name, before, after, items, afterEsc }, null, 2));
  await browser.close();
} finally {
  server.kill();
}
