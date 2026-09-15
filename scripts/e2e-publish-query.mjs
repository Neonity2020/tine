// "Export…" on a query publishes the WHOLE owner pages of its results into
// `published-queries/<name>/` through the real Tauri command route.
import { spawn } from "node:child_process";
import { remote } from "webdriverio";
import fs from "node:fs";
import path from "node:path";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { openPageByName } from "./lib/e2e-navigation.mjs";
import { waitForFileText } from "./e2e-file-poll.mjs";
import { freeLoopbackPort, resolveTauriDriver, tauriCapabilities, waitForHttpServer, webdriverServerArgs } from "./e2e-capabilities.mjs";

await ensureDisplay();
const tmp = fs.mkdtempSync("/tmp/tine-publish-query-");
const graph = path.join(tmp, "graph");
for (const dir of ["pages", "journals", "logseq", "assets"]) fs.mkdirSync(path.join(graph, dir), { recursive: true });
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(path.join(tmp, "xdg", dir), { recursive: true });
fs.writeFileSync(path.join(graph, "logseq/config.edn"), "{}\n");
// The host page has no TODO of its own, so it must NOT be exported.
fs.writeFileSync(path.join(graph, "pages/Dashboard.md"), "- {{query (task TODO)}}\n");
// Whole-page contract: the DONE sibling and the [[Private]] link ride along;
// Private itself (no TODO) does not, and its link is inert in the export.
fs.writeFileSync(path.join(graph, "pages/Alpha.md"), "- TODO Alpha export root ![shot](../assets/shot.png)\n\t- Alpha export child [[Beta]]\n- DONE alpha done sibling [[Private]]\n");
fs.writeFileSync(path.join(graph, "pages/Beta.org"), "* TODO Beta export root\n** Beta export child\n");
fs.writeFileSync(path.join(graph, "pages/Private.md"), "- private sentinel text\n");
fs.writeFileSync(path.join(graph, "assets/shot.png"), Buffer.from("89504e470d0a1a0a", "hex"));
const app = process.env.TINE_APP || `${process.env.HOME}/research/tine`;
const port = await freeLoopbackPort();
const nativePort = await freeLoopbackPort(new Set([port]));
const tdPath = resolveTauriDriver();
const log = fs.openSync(path.join(tmp, "driver.log"), "w");
const td = spawn(tdPath, webdriverServerArgs(port, nativePort, "/usr/bin/WebKitWebDriver"), {
  detached: true, stdio: ["ignore", log, log], env: { ...process.env, TINE_GRAPH: graph,
    XDG_DATA_HOME: path.join(tmp, "xdg/data"), XDG_CONFIG_HOME: path.join(tmp, "xdg/config"), XDG_CACHE_HOME: path.join(tmp, "xdg/cache"),
    WEBKIT_DISABLE_DMABUF_RENDERER: "1", WEBKIT_DISABLE_COMPOSITING_MODE: "1", LIBGL_ALWAYS_SOFTWARE: "1", GDK_BACKEND: "x11" },
});
console.log(JSON.stringify({ artifact: tmp, app }));
const out = path.join(graph, "published-queries", "open-tasks");

/** The Stage 2 app beside the static site: `app/index.html` marked as a
 *  published export, a schema-1 snapshot holding exactly the exported pages,
 *  the bundle's scripts, and a root redirect the static page loads. */
function appChecks(out, alphaHtml) {
  const appIndexPath = path.join(out, "app/index.html");
  const snapshotPath = path.join(out, "app/snapshot.json");
  const appIndex = fs.existsSync(appIndexPath) ? fs.readFileSync(appIndexPath, "utf8") : "";
  const snapshotText = fs.existsSync(snapshotPath) ? fs.readFileSync(snapshotPath, "utf8") : "";
  let snapshot = null;
  try { snapshot = JSON.parse(snapshotText); } catch {}
  const pageNames = snapshot ? snapshot.pages.map((page) => page.name) : [];
  const scripts = fs.existsSync(path.join(out, "app/assets")) ? fs.readdirSync(path.join(out, "app/assets")).filter((f) => f.endsWith(".js")) : [];
  return {
    "app/index.html is marked as a published export": appIndex.includes('<meta name="tine-published" content="snapshot.json">'),
    "app/index.html is titled by the export name": appIndex.includes("<title>Open tasks</title>"),
    // No graph page is named "Open tasks", so the home page takes the export's name.
    "snapshot is schema 1 with the home page first": snapshot?.schema === 1 && snapshot?.home === "Open tasks" && pageNames[0] === "Open tasks",
    "snapshot holds exactly the exported pages": pageNames.length === 3 && pageNames.includes("Alpha") && pageNames.includes("Beta"),
    "snapshot has no private text": !snapshotText.includes("private sentinel text") && !pageNames.includes("Private") && !pageNames.includes("Dashboard"),
    "snapshot records the home query with its two rows": Array.isArray(snapshot?.queries) && snapshot.queries.some((q) => q.host === snapshot.home && q.result?.total === 2),
    "app bundle ships its scripts": scripts.length >= 1,
    "static index loads the redirect shim": fs.existsSync(path.join(out, "app-redirect.js")) && fs.readFileSync(path.join(out, "index.html"), "utf8").includes('src="app-redirect.js"'),
    "static page links its ?static fallback note": alphaHtml.length > 0 && fs.readFileSync(path.join(out, "index.html"), "utf8").includes("publish-app-note"),
  };
}
let browser;
try {
  await waitForHttpServer(`http://127.0.0.1:${port}/status`);
  browser = await remote({ hostname: "127.0.0.1", port, path: "/", logLevel: "error", connectionRetryCount: 1,
    capabilities: tauriCapabilities(app, "publish-query") });
  await browser.$(".ls-block, .page-title").waitForExist({ timeout: 20000 });
  await openPageByName(browser, "Dashboard");
  await browser.waitUntil(async () => (await browser.$(".query-count").getText()) === "2", { timeout: 20000 });

  const exportButton = await browser.$(".query-export-button");
  await exportButton.waitForDisplayed({ timeout: 10000 });
  await exportButton.click();
  const modal = await browser.$(".query-export-modal");
  await modal.waitForDisplayed({ timeout: 10000 });
  // Untitled query: the dialog asks for a name and plans once it is committed.
  const name = await browser.$(".query-export-name-input");
  await name.setValue("Open tasks");
  await browser.keys("Enter");
  await browser.waitUntil(async () => (await browser.$$('[data-testid="query-export-pages"] li')).length === 2,
    { timeout: 20000, timeoutMsg: "plan did not list exactly the two owner pages" });
  const listed = await browser.execute(() => [...document.querySelectorAll('[data-testid="query-export-pages"] .query-export-page-name')].map((el) => el.textContent));
  if (!listed.includes("Alpha") || !listed.includes("Beta") || listed.includes("Dashboard") || listed.includes("Private")) {
    throw new Error(`plan listed the wrong pages: ${JSON.stringify(listed)}`);
  }
  // A block query exports whole pages; the export button stays disabled until
  // the user says they understood that.
  const confirm = await browser.$(".query-export-modal .export-btn-primary");
  if (await confirm.isEnabled()) throw new Error("Export was enabled before the whole-page acknowledgement");
  await browser.$(".query-export-ack input").click();
  await browser.waitUntil(async () => confirm.isEnabled(), { timeout: 5000 });
  await confirm.click();

  await waitForFileText(path.join(out, "alpha.html"), (text) => text.includes("Alpha export child"), "exported alpha.html", { timeoutMs: 30000 });
  const alpha = fs.readFileSync(path.join(out, "alpha.html"), "utf8");
  const files = fs.readdirSync(out);
  const checks = {
    "whole page: the DONE sibling is exported too": alpha.includes("alpha done sibling"),
    "outside link is inert (no private.html link)": alpha.includes("ref-outside") && !alpha.includes('href="private.html"'),
    "private page is not exported": !files.includes("private.html"),
    "host page without a match is not exported": !files.includes("dashboard.html"),
    "org owner exported": fs.readFileSync(path.join(out, "beta.html"), "utf8").includes("Beta export child"),
    "asset copied into the leaf and linked relatively": fs.existsSync(path.join(out, "assets/shot.png")) && alpha.includes('src="assets/shot.png"'),
    "index present": files.includes("index.html"),
    "graph site untouched": !fs.existsSync(path.join(graph, "publish")),
    "dialog closed": (await browser.$$(".query-export-modal")).length === 0,
    // Stage 2: the export also ships the read-only app over a baked snapshot.
    ...appChecks(out, alpha),
  };
  for (const [label, ok] of Object.entries(checks)) console.log(`${ok ? "PASS" : "FAIL"}: ${label}`);
  if (Object.values(checks).some((ok) => !ok)) throw new Error("publish-query invariants failed");
  const report = { app, out, files, listed };
  fs.writeFileSync(path.join(tmp, "report.json"), JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report));
} catch (error) {
  try { fs.writeFileSync(path.join(tmp, "failure.html"), await browser?.execute(() => document.body.outerHTML)); } catch {}
  console.error(String(error)); process.exitCode = 1;
} finally {
  try { await browser?.deleteSession(); } catch {}
  try { process.kill(-td.pid, "SIGKILL"); } catch {}
  fs.closeSync(log);
}
