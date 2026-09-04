// A page's name comes from `title::` when it has one, else the filename —
// Logseq's order (graph-parser extract.cljc get-page-name). A link written
// against the FILENAME of a title::-bearing file therefore names no page, opens
// blank, and must be visibly dimmed rather than silently dead-ending.
//
// From Martin's 2026-08-09 report: a generated graph linked 29 papers by
// filename while every file declared a different `title::`. Every link opened
// an empty page and nothing on screen said why.
import { spawn } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { tauriCapabilities, webdriverServerArgs } from "./e2e-capabilities.mjs";
import { openPageByName } from "./lib/e2e-navigation.mjs";

await ensureDisplay();

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APP = process.env.TINE_APP || path.join(ROOT, "target/release/tine");
const TD = process.env.TAURI_DRIVER || (process.env.CARGO_HOME ? path.join(process.env.CARGO_HOME, "bin", "tauri-driver") : "tauri-driver");
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4496);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4497);
const TMP = "/tmp/tine-page-identity-e2e";
const GRAPH = `${TMP}/graph`;
const INDEX = "Index";

fs.rmSync(TMP, { recursive: true, force: true });
for (const dir of ["pages", "journals", "logseq"]) fs.mkdirSync(`${GRAPH}/${dir}`, { recursive: true });
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(`${TMP}/xdg/${dir}`, { recursive: true });

// `r01 Renamed Report.md` is really the page "Real Title" because of title::.
fs.writeFileSync(`${GRAPH}/pages/r01 Renamed Report.md`,
  "title:: Real Title\ngrade:: green\n\n- body of the renamed report\n- second line\n");
// A plain page whose filename IS its name.
fs.writeFileSync(`${GRAPH}/pages/Plain Page.md`, "- body of the plain page\n");
// The index links one live name, one filename-that-is-not-a-name, and a tag
// whose page does not exist (tags must NOT be dimmed).
fs.writeFileSync(`${GRAPH}/pages/${INDEX}.md`,
  "- live [[Plain Page]]\n- live [[Real Title]]\n- dead [[r01 Renamed Report]]\n- dead [[Never Written]]\n- tag #someTag\n");

const env = {
  ...process.env,
  TINE_GRAPH: GRAPH,
  XDG_DATA_HOME: `${TMP}/xdg/data`, XDG_CONFIG_HOME: `${TMP}/xdg/config`, XDG_CACHE_HOME: `${TMP}/xdg/cache`,
  WEBKIT_DISABLE_DMABUF_RENDERER: "1", WEBKIT_DISABLE_COMPOSITING_MODE: "1", LIBGL_ALWAYS_SOFTWARE: "1", GDK_BACKEND: "x11",
};
const log = fs.openSync(`${TMP}/tauri-driver.log`, "w");
const td = spawn(TD, webdriverServerArgs(DRIVER_PORT, NATIVE_PORT, process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver"), {
  env, stdio: ["ignore", log, log], detached: true,
});
await sleep(2500);

let browser;
try {
  browser = await remote({
    hostname: "127.0.0.1", port: DRIVER_PORT, path: "/", logLevel: "error", connectionRetryCount: 1, connectionRetryTimeout: 60_000,
    capabilities: tauriCapabilities(APP, "page-identity-links"),
  });
  await browser.$(".ls-block, .page-title").waitForExist({ timeout: 30_000 });

  // One shared readiness contract for opening a page: find and activate the
  // exact non-block row in a single round trip and retry against the routed
  // title, so no element handle outlives the switcher's re-render. See
  // scripts/lib/e2e-navigation.mjs.
  const gotoPage = (name) => openPageByName(browser, name);

  // 1. The title::-bearing file is reachable under its TITLE, not its filename.
  await gotoPage("Real Title");
  const renamedBody = await browser.execute(() => document.querySelector(".page-blocks")?.textContent ?? "");
  if (!renamedBody.includes("body of the renamed report")) {
    throw new Error(`"Real Title" did not load the r01 file's content: ${JSON.stringify(renamedBody.slice(0, 120))}`);
  }
  console.log("title:: names the page");

  // 2. Dead links are dimmed; live links and tags are not.
  await gotoPage(INDEX);
  await browser.waitUntil(async () => (await browser.execute(() =>
    document.querySelectorAll(".page-blocks a.page-ref.page-ref-missing").length)) === 2,
    { timeout: 10_000, timeoutMsg: "the two dead links were never marked as missing" });
  const refs = await browser.execute(() => [...document.querySelectorAll(".page-blocks a.page-ref, .page-blocks a.tag")]
    .map((a) => ({ text: a.textContent?.trim() ?? "", missing: a.classList.contains("page-ref-missing"), tag: a.classList.contains("tag") })));
  const dimmed = refs.filter((r) => r.missing).map((r) => r.text).sort();
  const expected = ["[[Never Written]]", "[[r01 Renamed Report]]"].sort();
  if (JSON.stringify(dimmed) !== JSON.stringify(expected)) {
    throw new Error(`wrong set of dimmed links: ${JSON.stringify(refs)}`);
  }
  if (refs.some((r) => r.tag && r.missing)) throw new Error("a #tag was dimmed; tags without a page file are ordinary");
  console.log("dead links dimmed, live links and tags untouched");

  // 3. A dead link still navigates and still opens a usable blank page.
  await browser.execute(() => [...document.querySelectorAll(".page-blocks a.page-ref")]
    .find((a) => a.textContent?.trim() === "[[Never Written]]")
    ?.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, button: 0 })));
  await browser.waitUntil(async () => (await browser.execute(() => document.querySelector("h1.page-title")?.textContent?.trim())) === "Never Written",
    { timeout: 10_000, timeoutMsg: "a dimmed link stopped navigating" });
  console.log("PASS: title:: owns page identity; unresolved links are visible but still work");
} finally {
  try { await browser?.deleteSession(); } catch {}
  try { process.kill(-td.pid, "SIGKILL"); } catch {}
  fs.closeSync(log);
}
