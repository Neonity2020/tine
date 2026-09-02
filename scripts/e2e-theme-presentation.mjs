import { waitForHttpServer } from "./e2e-capabilities.mjs";
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

const port = 5199;
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "tine-theme-presentation-"));
const manifestFile = path.join(scratch, "theme.json");
fs.writeFileSync(manifestFile, JSON.stringify({
  schemaVersion: 1,
  id: "dev.tine.theme.presentation-e2e",
  name: "Presentation E2E",
  version: "1.0.0",
  apiVersion: "0.2",
  description: "A local real-app presentation fixture.",
  author: "Tine",
  license: "MIT",
  source: "https://example.invalid/theme",
  modes: { light: { "--ls-primary-background-color": "#fefefe" } },
  presentation: {
    contentTypography: "editorial-serif",
    journalHeader: "editorial",
    todayTaskSummary: "compact",
  },
  screenshots: [],
}));

const server = spawn(
  path.join(root, "node_modules", ".bin", "vite"),
  ["preview", "--host", "127.0.0.1", "--port", String(port), "--strictPort"],
  { cwd: root, stdio: "ignore" },
);


try {
  const url = `http://127.0.0.1:${port}/`;
  await waitForHttpServer(url, 60, 250, { failureMessage: "preview server did not start" });
  const browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
  page.setDefaultTimeout(15_000);
  await page.goto(url);
  await page.waitForSelector(".page-section.journal-today .page-title");

  await page.getByTitle("Settings (t s)").click();
  await page.getByRole("button", { name: "Appearance", exact: true }).click();
  await page.locator('input[type="file"][accept="application/json,.json"]').setInputFiles(manifestFile);
  await page.locator(".toast-msg", { hasText: "Presentation E2E 1.0.0 installed" }).waitFor();
  const installed = page.locator(".installed-theme-row", { hasText: "Presentation E2E" });
  await installed.getByRole("button", { name: "Use colors", exact: true }).click();
  await installed.getByRole("button", { name: "Use style", exact: true }).click();
  await installed.getByRole("button", { name: "Colors selected", exact: true }).waitFor();
  await installed.getByRole("button", { name: "Style selected", exact: true }).waitFor();
  await page.getByTitle("Light theme").click();
  const packageBackground = await page.evaluate(() =>
    getComputedStyle(document.documentElement).getPropertyValue("--ls-primary-background-color").trim());
  const gruvbox = page.locator(".theme-gallery-card", { hasText: "Gruvbox" });
  await gruvbox.click();
  await gruvbox.evaluate((card) => {
    if (card.getAttribute("aria-pressed") !== "true") throw new Error("Gruvbox colors were not selected");
  });
  const composition = await page.evaluate(() => ({
    typography: document.documentElement.getAttribute("data-theme-content-typography"),
    journalHeader: document.documentElement.getAttribute("data-theme-journal-header"),
    background: getComputedStyle(document.documentElement).getPropertyValue("--ls-primary-background-color").trim(),
  }));
  if (composition.typography !== "editorial-serif" || composition.journalHeader !== "editorial"
      || !composition.background || composition.background === packageBackground) {
    throw new Error(`theme composition failed: ${JSON.stringify({ packageBackground, composition })}`);
  }
  await page.locator(".settings-pane-head .icon-btn:not(.settings-maximize)").click();

  const today = page.locator(".page-section.journal-today");
  await today.locator(".block-content").first().click();
  await today.locator("textarea.block-editor").fill("TODO Verify the editorial theme");
  await today.locator("textarea.block-editor").press("Enter");
  await today.locator("textarea.block-editor").fill("DOING Compare the mobile rhythm");
  await today.locator("textarea.block-editor").press("Enter");
  await today.locator("textarea.block-editor").fill("LATER Publish the finished package");
  await page.keyboard.press("Escape");

  const desktop = await today.evaluate((section) => {
    const title = section.querySelector(".page-title");
    const summary = section.querySelector(".today-task-summary");
    if (!title || !summary) throw new Error("editorial header did not render");
    const sectionRect = section.getBoundingClientRect();
    const titleRect = title.getBoundingClientRect();
    return {
      titleCenterDelta: Math.abs((titleRect.left + titleRect.width / 2) - (sectionRect.left + sectionRect.width / 2)),
      titleSize: getComputedStyle(title).fontSize,
      summary: summary.textContent,
      calendarVisible: !!section.querySelector(".title-cal")
        && getComputedStyle(section.querySelector(".title-cal")).display !== "none",
    };
  });
  if (desktop.titleCenterDelta > 2 || desktop.titleSize !== "46px"
      || desktop.summary !== "3 tasks today, 1 in progress" || desktop.calendarVisible) {
    throw new Error(`desktop presentation failed: ${JSON.stringify(desktop)}`);
  }

  await page.setViewportSize({ width: 390, height: 844 });
  const closeSidebar = page.getByRole("button", { name: "Close navigation sidebar", exact: true });
  if (await closeSidebar.count()) await closeSidebar.click();
  const mobile = await today.evaluate((section) => {
    const title = section.querySelector(".page-title");
    const rect = title?.getBoundingClientRect();
    return {
      titleSize: title ? getComputedStyle(title).fontSize : null,
      right: rect?.right ?? 0,
      viewport: document.documentElement.clientWidth,
      overflow: document.documentElement.scrollWidth > document.documentElement.clientWidth,
    };
  });
  if (mobile.titleSize !== "38px" || mobile.right > mobile.viewport || mobile.overflow) {
    throw new Error(`mobile presentation failed: ${JSON.stringify(mobile)}`);
  }

  console.log(`theme presentation E2E passed: ${JSON.stringify({ composition, desktop, mobile })}`);
  await browser.close();
} finally {
  server.kill("SIGTERM");
  fs.rmSync(scratch, { recursive: true, force: true });
}
