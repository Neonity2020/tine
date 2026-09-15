// A published query export opens as the read-only Tine app in an ordinary
// browser (Stage 2). This journey serves an export folder over HTTP and drives
// headless Chromium through it: the root page redirects to `app/`, the home
// page shows the baked query rows, the Quick Switcher opens an exported page,
// Linked References come from the snapshot, typing changes nothing, and
// `?static` keeps the static site.
//
// The export comes from `E2E_PUBLISHED_DIR` when set; otherwise this script
// runs `scripts/e2e-publish-query.mjs` (the real Tauri export) first and uses
// the folder it reports.
import { spawn } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { chromium } from "playwright";

const here = path.dirname(new URL(import.meta.url).pathname);

async function exportFolder() {
  if (process.env.E2E_PUBLISHED_DIR) return { out: process.env.E2E_PUBLISHED_DIR, producer: null };
  const producer = spawn(process.execPath, [path.join(here, "e2e-publish-query.mjs")], { stdio: ["ignore", "pipe", "inherit"], env: process.env });
  let stdout = "";
  producer.stdout.on("data", (chunk) => { stdout += chunk; process.stdout.write(chunk); });
  const code = await new Promise((resolve) => producer.on("exit", resolve));
  if (code !== 0) throw new Error(`e2e-publish-query exited ${code}`);
  const report = stdout.split("\n").map((line) => { try { return JSON.parse(line); } catch { return null; } }).filter((v) => v && v.out).pop();
  if (!report) throw new Error("e2e-publish-query printed no report with an export folder");
  return { out: report.out, producer: report };
}

const MIME = { ".html": "text/html; charset=utf-8", ".js": "text/javascript", ".css": "text/css", ".json": "application/json", ".png": "image/png", ".svg": "image/svg+xml", ".woff2": "font/woff2", ".woff": "font/woff", ".wasm": "application/wasm" };

function serve(root) {
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, "http://localhost");
    let file = path.join(root, decodeURIComponent(url.pathname));
    if (!file.startsWith(root)) { res.writeHead(403).end(); return; }
    if (fs.existsSync(file) && fs.statSync(file).isDirectory()) file = path.join(file, "index.html");
    if (!fs.existsSync(file)) { res.writeHead(404).end("not found"); return; }
    res.writeHead(200, { "content-type": MIME[path.extname(file)] ?? "application/octet-stream" });
    fs.createReadStream(file).pipe(res);
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve({ server, port: server.address().port })));
}

const { out, producer } = await exportFolder();
const root = path.resolve(out);
const snapshot = JSON.parse(fs.readFileSync(path.join(root, "app/snapshot.json"), "utf8"));
const artifact = fs.mkdtempSync("/tmp/tine-published-app-");
const { server, port } = await serve(root);
const base = `http://127.0.0.1:${port}`;
let browser;
let page;
const errors = [];
try {
  browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
  page = await browser.newPage({ viewport: { width: 1120, height: 820 } });
  page.on("pageerror", (e) => errors.push(String(e)));
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));

  await page.goto(`${base}/`);
  await page.waitForURL((url) => url.pathname.endsWith("/app/"), { timeout: 10000 });
  await page.waitForSelector("h1.page-title", { timeout: 30000 });
  const homeTitle = (await page.textContent("h1.page-title"))?.trim();
  // The home page hosts the exported query; its rows are the two owner roots.
  await page.waitForFunction(() => document.querySelector(".query-count")?.textContent === "2", null, { timeout: 30000 });
  const homeText = await page.evaluate(() => document.querySelector(".page-blocks")?.textContent ?? "");
  const noChrome = await page.evaluate(() => !document.querySelector(".query-export-button") && !document.querySelector(".query-view-switcher"));

  // Quick Switcher opens an exported page from the snapshot.
  await page.keyboard.press("Control+k");
  await page.waitForSelector(".switcher-input", { timeout: 10000 });
  await page.fill(".switcher-input", "Beta");
  await page.waitForFunction(() => [...document.querySelectorAll(".switcher-row")].some((row) => !row.classList.contains("block-result") && row.querySelector(".switcher-name")?.textContent?.trim() === "Beta"), null, { timeout: 10000 });
  await page.evaluate(() => {
    const row = [...document.querySelectorAll(".switcher-row")].find((r) => !r.classList.contains("block-result") && r.querySelector(".switcher-name")?.textContent?.trim() === "Beta");
    row.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true, button: 0 }));
  });
  await page.waitForFunction(() => document.querySelector("h1.page-title")?.textContent?.trim() === "Beta", null, { timeout: 15000 });
  const betaText = await page.evaluate(() => document.querySelector(".page-blocks")?.textContent ?? "");
  // Linked References: Alpha's child links [[Beta]], and Alpha is exported.
  await page.waitForFunction(() => (document.querySelector(".linked-references")?.textContent ?? "").includes("Alpha export child"), null, { timeout: 15000 });
  const refsCount = (await page.textContent(".references-count"))?.trim();

  // Typing changes nothing: the page is read-only, so clicking a block opens
  // no editor and keystrokes leave the text as exported.
  const before = await page.evaluate(() => document.querySelector(".page-blocks")?.textContent ?? "");
  await page.click(".page-blocks .ls-block .block-content-wrapper");
  await page.keyboard.type("MUTATION");
  await page.waitForTimeout(500);
  const after = await page.evaluate(() => document.querySelector(".page-blocks")?.textContent ?? "");
  const editorOpen = await page.evaluate(() => Boolean(document.querySelector(".page-blocks textarea")));

  // `?static` stays on the static site.
  await page.goto(`${base}/?static`);
  await page.waitForTimeout(800);
  const staticUrl = page.url();
  const staticNote = await page.evaluate(() => Boolean(document.querySelector(".publish-app-note")));

  const checks = {
    "root redirects to app/ over HTTP": true,
    "home page is the export's own page": homeTitle === snapshot.home,
    "home query shows the baked rows": homeText.includes("Alpha export root") && homeText.includes("Beta export root"),
    "no export/view/builder chrome in the export": noChrome,
    "Quick Switcher opens an exported page": betaText.includes("Beta export child"),
    "Linked References come from the snapshot": refsCount === "1",
    "typing changes nothing on a read-only page": before === after && !after.includes("MUTATION") && !editorOpen,
    "?static keeps the static site": !new URL(staticUrl).pathname.endsWith("/app/") && staticNote,
    "no page errors": errors.length === 0,
  };
  for (const [label, ok] of Object.entries(checks)) console.log(`${ok ? "PASS" : "FAIL"}: ${label}`);
  const report = { out, producer, checks, errors, artifact };
  fs.writeFileSync(path.join(artifact, "report.json"), JSON.stringify(report, null, 2));
  if (Object.values(checks).some((ok) => !ok)) throw new Error("published-app invariants failed");
  console.log(JSON.stringify({ artifact, out }));
} catch (error) {
  try { await page?.screenshot({ path: path.join(artifact, "failure.png") }); } catch {}
  try { fs.writeFileSync(path.join(artifact, "failure.html"), (await page?.content()) ?? ""); } catch {}
  fs.writeFileSync(path.join(artifact, "errors.json"), JSON.stringify(errors, null, 2));
  console.error(String(error), `artifact: ${artifact}`);
  process.exitCode = 1;
} finally {
  try { await browser?.close(); } catch {}
  server.close();
}
