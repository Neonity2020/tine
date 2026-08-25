// Concord live-focus journey (GH #337).
//
// A laptop-sized Tine process stays open while another actor atomically replaces
// the page file. Returning to the window and clicking immediately must not open
// an editor over stale text: the native rescan receipt and frontend application
// fence finish first, then the deferred activation sees the disk winner.
//
// Usage: TINE_APP=/path/to/tine node scripts/e2e-concord-focus-freshness.mjs
import { spawn } from "node:child_process";
import fs from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";
import { remote } from "webdriverio";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { waitForFileText } from "./e2e-file-poll.mjs";
import { tauriCapabilities, webdriverServerArgs } from "./e2e-capabilities.mjs";

await ensureDisplay();

const G = "/tmp/tgraph-concord-focus";
const APP = process.env.TINE_APP || `${process.env.HOME}/research/tine`;
const TD = process.env.TAURI_DRIVER
  || (process.env.CARGO_HOME ? `${process.env.CARGO_HOME}/bin/tauri-driver` : "tauri-driver");
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4490);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4491);
const pageFile = `${G}/pages/Note.md`;

fs.rmSync(G, { recursive: true, force: true });
fs.mkdirSync(`${G}/pages`, { recursive: true });
fs.mkdirSync(`${G}/journals`, { recursive: true });
fs.mkdirSync(`${G}/logseq`, { recursive: true });
fs.writeFileSync(pageFile, "- before suspend\n");
fs.writeFileSync(`${G}/journals/2026_08_22.md`, "- open [[Note]]\n");

const xdg = "/tmp/txdg-concord-focus";
fs.rmSync(xdg, { recursive: true, force: true });
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(`${xdg}/${dir}`, { recursive: true });
const env = {
  ...process.env,
  TINE_GRAPH: G,
  XDG_DATA_HOME: `${xdg}/data`,
  XDG_CONFIG_HOME: `${xdg}/config`,
  XDG_CACHE_HOME: `${xdg}/cache`,
  WEBKIT_DISABLE_DMABUF_RENDERER: "1",
  LIBGL_ALWAYS_SOFTWARE: "1",
  WEBKIT_DISABLE_COMPOSITING_MODE: "1",
  GDK_BACKEND: "x11",
};

const log = fs.openSync("/tmp/td-concord-focus.log", "w");
const driver = spawn(TD, webdriverServerArgs(DRIVER_PORT, NATIVE_PORT, process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver"), { env, stdio: ["ignore", log, log], detached: true });
await sleep(3000);

let browser;
let failure;
try {
  browser = await remote({
    hostname: "127.0.0.1",
    port: DRIVER_PORT,
    path: "/",
    capabilities: tauriCapabilities(APP, "concord-focus-freshness"),
    logLevel: "error",
    connectionRetryCount: 1,
    connectionRetryTimeout: 60_000,
  });
  await browser.$(".ls-block, .page-title").waitForExist({ timeout: 20_000 });
  await sleep(800);

  const linkSelector = await browser.execute(() => {
    const candidates = [...document.querySelectorAll("a, span, button")];
    const index = candidates.findIndex((element) => element.textContent?.trim() === "[[Note]]");
    if (index < 0) return { found: false, texts: candidates.map((element) => element.textContent?.trim()).filter(Boolean).slice(0, 80) };
    const element = candidates[index];
    element.setAttribute("data-concord-e2e-link", "Note");
    return { found: true, html: element.outerHTML };
  });
  if (!linkSelector.found) throw new Error(`journal did not expose [[Note]]: ${JSON.stringify(linkSelector.texts)}`);
  await browser.$('[data-concord-e2e-link="Note"]').click();
  await browser.$(".ls-block").waitForExist({ timeout: 10_000 });
  // A real suspend/background interval is far beyond the focus-rescan throttle.
  // Cross it explicitly so this journey measures the receipt path rather than
  // the intentionally cheap duplicate-focus path from app startup.
  await sleep(1800);

  // Syncthing-style atomic replacement while the app is notionally suspended.
  const replacement = `${pageFile}.external`;
  fs.writeFileSync(replacement, "- winner from phone\n");
  fs.renameSync(replacement, pageFile);

  // Wake/focus and click in the same user turn. `startEditing` must be deferred
  // until the requested native scan and its graph-change applications finish.
  await browser.execute(() => window.dispatchEvent(new Event("focus")));
  // The real-app half of the proof: the focus receipt must install the external
  // winner before input is admitted. (The focused barrier unit test separately
  // exercises an activation attempted while this wait is still pending.)
  await browser.waitUntil(async () => {
    const text = await browser.$(".page-blocks .ls-block").getText();
    return text.includes("winner from phone");
  }, { timeout: 20_000, timeoutMsg: "focus receipt did not install the external winner" });
  // Use WebDriver's trusted pointer path for Tine's mousedown/mouseup gesture.
  await browser.$(".page-blocks .ls-block .block-content").click();
  const editor = await browser.$(".page-blocks textarea.block-editor");
  await editor.waitForExist({ timeout: 20_000 });
  const opened = await editor.getValue();
  if (!opened.includes("winner from phone")) {
    throw new Error(`editor activated over stale text: ${JSON.stringify(opened)}`);
  }
  await browser.keys(["End"]);
  await editor.addValue(" plus laptop");
  await waitForFileText(
    pageFile,
    (text) => text.includes("winner from phone plus laptop"),
    "fresh post-focus edit",
  );
  if (await browser.$(".conflict-banner").isExisting()) {
    throw new Error("legacy Direct Files conflict bar appeared");
  }
  console.log("PASS: focus receipt installed the disk winner before editor activation");
} catch (error) {
  failure = error;
  console.error("FAIL:", error?.message ?? error);
  try {
    if (browser) {
      const diagnostic = await browser.execute(() => ({
        body: document.body.innerText,
        block: document.querySelector(".page-blocks .ls-block")?.textContent,
        editor: document.querySelector("textarea.block-editor")?.value,
        barrier: document.querySelector(".focus-freshness-barrier")?.textContent,
      }));
      console.error("STATE:", JSON.stringify(diagnostic));
      fs.writeFileSync("/tmp/e2e-concord-focus.png", await browser.takeScreenshot(), "base64");
    }
  } catch {}
} finally {
  try { if (browser) await browser.deleteSession(); } catch {}
  try { process.kill(-driver.pid, "SIGKILL"); } catch {}
}

process.exit(failure ? 1 : 0);
