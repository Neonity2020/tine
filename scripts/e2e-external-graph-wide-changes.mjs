// Linux real-app proof for GH #268: a page that does not live under `journals/`
// or `pages/` must still react to external edits while the app is open.
//
// This is the observation boundary the unit tests cannot reach. The watcher's
// routing predicates are unit-tested, but the reported symptom is "I edit the
// file in another editor and Tine never notices", and only the real app running
// its real watcher thread can show that end to end. Discovery already walked
// graph-wide, so these pages OPEN correctly -- the bug was invisible on any
// check that only loaded the graph.
import { spawn } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APP = process.env.TINE_APP || path.join(ROOT, "target/release/tine");
const TD = process.env.TAURI_DRIVER || (process.env.CARGO_HOME ? path.join(process.env.CARGO_HOME, "bin", "tauri-driver") : "tauri-driver");
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4496);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4497);
const TMP = "/tmp/tine-external-graph-wide-e2e";
const GRAPH = `${TMP}/graph`;

fs.rmSync(TMP, { recursive: true, force: true });
for (const dir of ["pages", "journals", "logseq", "Archive"]) fs.mkdirSync(`${GRAPH}/${dir}`, { recursive: true });
fs.writeFileSync(`${GRAPH}/logseq/config.edn`, "{}\n");

// Two pages OUTSIDE journals/ and pages/: one at the graph root, one in a
// custom folder. Both are ordinary graph text as far as discovery is concerned.
fs.writeFileSync(`${GRAPH}/Root note.md`, "- root before\n");
fs.writeFileSync(`${GRAPH}/Archive/Filed.md`, "- filed before\n");
// A control that lives where the old watcher already looked.
fs.writeFileSync(`${GRAPH}/pages/Ordinary.md`, "- ordinary before\n");

const now = new Date();
const journal = `${now.getFullYear()}_${String(now.getMonth() + 1).padStart(2, "0")}_${String(now.getDate()).padStart(2, "0")}`;
fs.writeFileSync(
  `${GRAPH}/journals/${journal}.md`,
  "- open [[Root note]]\n- open [[Filed]]\n- open [[Ordinary]]\n",
);

for (const dir of ["data", "config", "cache"]) fs.mkdirSync(`${TMP}/xdg/${dir}`, { recursive: true });
const appData = `${TMP}/xdg/data/page.tine.Tine`;
fs.mkdirSync(appData, { recursive: true });
const canonicalGraph = fs.realpathSync(GRAPH);
fs.writeFileSync(`${appData}/tine-settings.json`, JSON.stringify({ last_graph_path: canonicalGraph }, null, 2));

const env = {
  ...process.env,
  TINE_GRAPH: GRAPH,
  XDG_DATA_HOME: `${TMP}/xdg/data`,
  XDG_CONFIG_HOME: `${TMP}/xdg/config`,
  XDG_CACHE_HOME: `${TMP}/xdg/cache`,
  WEBKIT_DISABLE_DMABUF_RENDERER: "1",
  WEBKIT_DISABLE_COMPOSITING_MODE: "1",
  LIBGL_ALWAYS_SOFTWARE: "1",
  GDK_BACKEND: "x11",
};
const log = fs.openSync(`${TMP}/tauri-driver.log`, "w");
const td = spawn(TD, ["--port", String(DRIVER_PORT), "--native-port", String(NATIVE_PORT), "--native-driver", process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver"], {
  env, stdio: ["ignore", log, log], detached: true,
});
await sleep(2500);

let browser;
try {
  browser = await remote({
    hostname: "127.0.0.1", port: DRIVER_PORT, path: "/", logLevel: "error",
    connectionRetryCount: 1, connectionRetryTimeout: 60_000,
    capabilities: { browserName: "wry", "wdio:enforceWebDriverClassic": true, "tauri:options": { application: APP } },
  });
  await browser.$(".ls-block, .page-title").waitForExist({ timeout: 20_000 });

  // Every page is reached from the journal feed, so return there first: the
  // page-ref links only exist on the journal.
  const openPage = async (name) => {
    const journals = await browser.$(".nav-item*=Journals");
    await journals.click();
    const selectors = [`a.page-ref=${name}`, `span.page-ref=${name}`, `*=${name}`];
    const link = await browser.waitUntil(async () => {
      for (const selector of selectors) {
        const candidate = await browser.$(selector);
        if (await candidate.isExisting()) return candidate;
      }
      return false;
    }, { timeout: 15_000, interval: 300, timeoutMsg: `journal feed did not offer a link to ${name}` });
    await link.click();
    await browser.waitUntil(async () => (await browser.$("h1.page-title").getText()).trim() === name, {
      timeout: 15_000, timeoutMsg: `${name} did not open`,
    });
  };

  // The page body is the user's observation surface. Poll it rather than the
  // file: the whole question is whether the app noticed.
  const waitForPageText = async (needle, label) => {
    await browser.waitUntil(async () => {
      const text = await browser.$(".page-blocks, .ls-page, main").getText();
      return text.includes(needle);
    }, { timeout: 30_000, interval: 500, timeoutMsg: label });
  };
  const waitForPageTextGone = async (needle, label) => {
    await browser.waitUntil(async () => {
      const text = await browser.$(".page-blocks, .ls-page, main").getText();
      return !text.includes(needle);
    }, { timeout: 30_000, interval: 500, timeoutMsg: label });
  };

  // 1. A page in a CUSTOM FOLDER reacts to an external edit.
  await openPage("Filed");
  await waitForPageText("filed before", "Archive/Filed.md did not open with its on-disk content");
  fs.writeFileSync(`${GRAPH}/Archive/Filed.md`, "- filed before\n- filed AFTER external edit\n");
  await waitForPageText(
    "filed AFTER external edit",
    "GH #268: an external edit to a page in a custom folder never reached the open app",
  );

  // 2. A page at the GRAPH ROOT reacts to an external edit.
  await openPage("Root note");
  await waitForPageText("root before", "Root note.md did not open with its on-disk content");
  fs.writeFileSync(`${GRAPH}/Root note.md`, "- root before\n- root AFTER external edit\n");
  await waitForPageText(
    "root AFTER external edit",
    "GH #268: an external edit to a page at the graph root never reached the open app",
  );

  // 3. The control still works -- the widened scope must not have cost the
  //    behavior that already worked.
  await openPage("Ordinary");
  fs.writeFileSync(`${GRAPH}/pages/Ordinary.md`, "- ordinary before\n- ordinary AFTER external edit\n");
  await waitForPageText("ordinary AFTER external edit", "an external edit under pages/ regressed");

  // 4. External DELETE of an out-of-dirs page is observed too.
  await openPage("Filed");
  fs.rmSync(`${GRAPH}/Archive/Filed.md`);
  await waitForPageTextGone(
    "filed AFTER external edit",
    "GH #268: an external delete of a page in a custom folder never reached the open app",
  );

  console.log("PASS: external edits and deletes outside journals/ and pages/ reach the open app");
} finally {
  try { await browser?.deleteSession(); } catch {}
  try { process.kill(-td.pid, "SIGKILL"); } catch {}
  fs.closeSync(log);
}
