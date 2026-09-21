// Browser proof for the checked-in public Guide: HTTP opens the read-only app,
// its real Guide home and navigation work, refocus produces no write-warning
// toast (GH #549), and the static fallback remains reachable.
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { chromium } from "./lib/playwright.mjs";

const root = path.resolve("website/guide");
const snapshot = JSON.parse(fs.readFileSync(path.join(root, "app", "snapshot.json"), "utf8"));
const roadmap = snapshot.pages.find((candidate) => candidate.name === "Project/Roadmap");
const roadmapBlock = roadmap?.blocks[0]?.id;
if (!roadmapBlock) throw new Error("Guide snapshot has no Project/Roadmap block for permalink proof");
const mime = {
  ".css": "text/css",
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
  ".json": "application/json",
  ".mjs": "text/javascript",
  ".svg": "image/svg+xml",
  ".wasm": "application/wasm",
  ".woff": "font/woff",
  ".woff2": "font/woff2",
};
const server = http.createServer((request, response) => {
  const url = new URL(request.url, "http://localhost");
  let file = path.join(root, decodeURIComponent(url.pathname));
  if (!file.startsWith(root)) return response.writeHead(403).end();
  if (fs.existsSync(file) && fs.statSync(file).isDirectory()) file = path.join(file, "index.html");
  if (!fs.existsSync(file)) return response.writeHead(404).end("not found");
  response.writeHead(200, { "content-type": mime[path.extname(file)] ?? "application/octet-stream" });
  fs.createReadStream(file).pipe(response);
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const base = `http://127.0.0.1:${server.address().port}`;
let browser;
let context;
let page;
try {
  browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
  context = await browser.newContext({
    viewport: { width: 1120, height: 820 },
    permissions: ["clipboard-read", "clipboard-write"],
  });
  page = await context.newPage();
  const errors = [];
  page.on("pageerror", (error) => errors.push(String(error)));
  page.on("console", (message) => message.type() === "error" && errors.push(message.text()));
  await page.goto(`${base}/`);
  await page.waitForURL((url) => url.pathname.endsWith("/app/"), { timeout: 10_000 });
  await page.waitForFunction(
    () => document.querySelector("h1.page-title")?.textContent?.includes("Welcome to Tine"),
    null,
    { timeout: 30_000 },
  );
  await page.waitForURL((url) => url.hash === "#/page/Welcome%20to%20Tine", { timeout: 10_000 });
  await page.click('a.page-ref:has-text("Project/Roadmap")');
  await page.waitForFunction(
    () => document.querySelector("h1.page-title")?.textContent?.includes("Project/Roadmap"),
    null,
    { timeout: 15_000 },
  );
  await page.waitForURL((url) => url.hash === "#/page/Project%2FRoadmap", { timeout: 10_000 });
  await page.click("[data-page-actions-trigger]");
  await page.click('[data-page-action-id="copy-page-link"]');
  const copiedPageLink = await page.evaluate(() => navigator.clipboard.readText());
  if (copiedPageLink !== page.url()) {
    throw new Error(`Copy page link mismatch: copied=${copiedPageLink} url=${page.url()}`);
  }

  await page.goto(`${base}/app/#/block/${encodeURIComponent(roadmapBlock)}`);
  await page.waitForFunction(
    () => document.querySelector("h1.page-title")?.textContent?.includes("Project/Roadmap"),
    null,
    { timeout: 30_000 },
  );
  await page.waitForSelector(`.ls-block[data-block-id="${roadmapBlock}"].block-flash`, { timeout: 15_000 });

  const other = await context.newPage();
  await other.goto("about:blank");
  await other.bringToFront();
  await page.bringToFront();
  await page.waitForTimeout(500);
  const toast = await page.evaluate(() =>
    [...document.querySelectorAll(".toast")].map((node) => node.textContent ?? "").join("\n"),
  );
  if (/external changes|Editing is available/i.test(toast)) throw new Error(`refocus toast: ${toast}`);
  if (errors.length) throw new Error(`browser errors:\n${errors.join("\n")}`);
  await page.goto(`${base}/?static`);
  await page.waitForSelector(".publish-app-note", { timeout: 10_000 });
  if (new URL(page.url()).pathname.endsWith("/app/")) throw new Error("?static redirected to the app");
  console.log("Live Guide browser proof passed: page URLs, copied link, block anchor, quiet refocus, static fallback.");
} catch (error) {
  const title = await page?.title().catch(() => "");
  const text = await page?.locator("body").innerText().catch(() => "");
  throw new Error(`${error}\nurl=${page?.url()}\ntitle=${title}\nbody=${text?.slice(0, 1200)}`);
} finally {
  await browser?.close();
  server.close();
}
