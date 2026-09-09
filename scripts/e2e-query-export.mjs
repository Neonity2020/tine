// Native Copy / Export resolves query macros through the shared SQLite route.
import { spawn } from "node:child_process";
import { remote } from "webdriverio";
import fs from "node:fs";
import path from "node:path";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { openPageByName } from "./lib/e2e-navigation.mjs";
import { enableManagedStorage } from "./lib/e2e-managed-activation.mjs";
import { freeLoopbackPort, tauriCapabilities, waitForHttpServer, webdriverServerArgs } from "./e2e-capabilities.mjs";

await ensureDisplay();
const tmp = fs.mkdtempSync("/tmp/tine-query-export-");
const graph = path.join(tmp, "graph");
const managed = process.env.E2E_QUERY_MANAGED === "1";
for (const dir of ["pages", "journals", "logseq"]) fs.mkdirSync(path.join(graph, dir), { recursive: true });
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(path.join(tmp, "xdg", dir), { recursive: true });
fs.writeFileSync(path.join(graph, "logseq/config.edn"), "{}\n");
fs.writeFileSync(path.join(graph, "pages/Dashboard.md"), "- {{query (task TODO)}}\n");
fs.writeFileSync(path.join(graph, "pages/Alpha.md"), "- TODO Alpha export root\n\t- Alpha export child\n- DONE excluded sibling\n");
fs.writeFileSync(path.join(graph, "pages/Beta.org"), "* TODO Beta export root\n** Beta export child\n* DONE other excluded sibling\n");
const app = process.env.TINE_APP || `${process.env.HOME}/research/tine-query`;
const port = await freeLoopbackPort();
const nativePort = await freeLoopbackPort(new Set([port]));
const tdPath = process.env.TAURI_DRIVER || (process.env.CARGO_HOME ? `${process.env.CARGO_HOME}/bin/tauri-driver` : "tauri-driver");
const log = fs.openSync(path.join(tmp, "driver.log"), "w");
const wm = managed ? spawn("openbox", ["--sm-disable"], { detached: true, stdio: ["ignore", log, log] }) : undefined;
const td = spawn(tdPath, webdriverServerArgs(port, nativePort, "/usr/bin/WebKitWebDriver"), {
  detached: true, stdio: ["ignore", log, log], env: { ...process.env, TINE_GRAPH: graph,
    XDG_DATA_HOME: path.join(tmp, "xdg/data"), XDG_CONFIG_HOME: path.join(tmp, "xdg/config"), XDG_CACHE_HOME: path.join(tmp, "xdg/cache"),
    WEBKIT_DISABLE_DMABUF_RENDERER: "1", WEBKIT_DISABLE_COMPOSITING_MODE: "1", LIBGL_ALWAYS_SOFTWARE: "1", GDK_BACKEND: "x11" },
});
console.log(JSON.stringify({ artifact: tmp, app, managed }));
let browser;
try {
  await waitForHttpServer(`http://127.0.0.1:${port}/status`);
  browser = await remote({ hostname: "127.0.0.1", port, path: "/", logLevel: "error", connectionRetryCount: 1,
    capabilities: tauriCapabilities(app, "query-export") });
  await browser.$(".ls-block, .page-title").waitForExist({ timeout: 20000 });
  if (managed) await enableManagedStorage(browser);
  await openPageByName(browser, "Dashboard");
  await browser.waitUntil(async () => (await browser.$(".query-count").getText()) === "2", { timeout: 20000 });
  await browser.$(".page-title").click({ button: "right" });
  const exportAction = await browser.$("button*=Copy / export as");
  await exportAction.waitForDisplayed({ timeout: 10000 });
  await exportAction.click();
  await browser.$(".export-modal").waitForDisplayed({ timeout: 10000 });
  const expected = ["Alpha export root", "Alpha export child", "Beta export root", "Beta export child"];
  await browser.waitUntil(async () => {
    const text = await browser.$(".export-preview").getValue();
    return expected.every(value => text.includes(value));
  }, { timeout: 20000, timeoutMsg: "rendered export did not resolve matching Markdown and Org subtrees" });
  const preview = await browser.$(".export-preview").getValue();
  if (preview.includes("excluded sibling") || preview.includes("{{query")) throw new Error("export retained a nonmatching sibling or unresolved query macro");
  const report = { app, managed, preview, expected };
  fs.writeFileSync(path.join(tmp, "report.json"), JSON.stringify(report, null, 2));
  fs.writeFileSync(path.join(tmp, "result.html"), await browser.execute(() => document.body.outerHTML));
  console.log(JSON.stringify(report));
} catch (error) {
  try { fs.writeFileSync(path.join(tmp, "failure.html"), await browser?.execute(() => document.body.outerHTML)); } catch {}
  console.error(String(error)); process.exitCode = 1;
} finally {
  try { await browser?.deleteSession(); } catch {}
  try { process.kill(-td.pid, "SIGKILL"); } catch {}
  try { if (wm?.pid) process.kill(-wm.pid, "SIGKILL"); } catch {}
  fs.closeSync(log);
}
