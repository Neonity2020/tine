// Concord live Direct Files conflict journey.
//
// A retained editor draft races an external atomic replacement. The guarded
// save must become an in-page Concord object (never the legacy global bar), the
// draft must survive a process restart, and a no-loss resolution must persist
// ordinary Markdown containing both sides.
//
// Usage: TINE_APP=/path/to/tine node scripts/e2e-concord-live-save.mjs
import { spawn } from "node:child_process";
import fs from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";
import { remote } from "webdriverio";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { waitForFileText } from "./e2e-file-poll.mjs";

await ensureDisplay();

const G = "/tmp/tgraph-concord-live-save";
const APP = process.env.TINE_APP || `${process.env.HOME}/research/tine`;
const TD = process.env.TAURI_DRIVER
  || (process.env.CARGO_HOME ? `${process.env.CARGO_HOME}/bin/tauri-driver` : "tauri-driver");
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4500);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4501);
const pageFile = `${G}/pages/Note.md`;

fs.rmSync(G, { recursive: true, force: true });
fs.mkdirSync(`${G}/pages`, { recursive: true });
fs.mkdirSync(`${G}/journals`, { recursive: true });
fs.mkdirSync(`${G}/logseq`, { recursive: true });
fs.writeFileSync(pageFile, "- common base\n");
fs.writeFileSync(`${G}/journals/2026_08_22.md`, "- open [[Note]]\n");

const xdg = "/tmp/txdg-concord-live-save";
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

const log = fs.openSync("/tmp/td-concord-live-save.log", "w");
const driver = spawn(TD, [
  "--port", String(DRIVER_PORT),
  "--native-port", String(NATIVE_PORT),
  "--native-driver", process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver",
], { env, stdio: ["ignore", log, log], detached: true });
await sleep(3000);

async function newSession() {
  return remote({
    hostname: "127.0.0.1",
    port: DRIVER_PORT,
    path: "/",
    capabilities: {
      browserName: "wry",
      "wdio:enforceWebDriverClassic": true,
      "tauri:options": { application: APP },
    },
    logLevel: "error",
    connectionRetryCount: 1,
    connectionRetryTimeout: 60_000,
  });
}

async function openNote(browser) {
  await browser.$(".ls-block, .page-title").waitForExist({ timeout: 20_000 });
  const title = await browser.$("h1.page-title");
  if (!(await title.isExisting()) || (await title.getText()) !== "Note") {
    const ref = await browser.$(".page-ref");
    await ref.waitForExist({ timeout: 10_000 });
    await ref.click();
    await browser.waitUntil(async () =>
      (await browser.$("h1.page-title").getText()) === "Note",
    { timeout: 10_000, timeoutMsg: "Note did not open" });
  }
}

async function assertLiveConflict(browser, phase) {
  const resolver = await browser.$(".page-conflict");
  await resolver.waitForExist({ timeout: 20_000 });
  await browser.waitUntil(async () => {
    const cells = await browser.execute(() =>
      [...document.querySelectorAll(".page-conflict .sync-merge-cell")]
        .map((cell) => cell.textContent ?? "")
    );
    return cells.some((value) => value.includes("local laptop draft"))
      && cells.some((value) => value.includes("disk from phone"));
  }, { timeout: 20_000, timeoutMsg: `${phase}: resolver did not retain both sides` });
  if (await browser.$(".conflict-banner").isExisting()) {
    throw new Error(`${phase}: legacy Direct Files conflict bar appeared`);
  }
}

let browser;
let failure;
try {
  browser = await newSession();
  await openNote(browser);
  await browser.$(".page-blocks .ls-block .block-content").click();
  const editor = await browser.$(".page-blocks textarea.block-editor");
  await editor.waitForExist({ timeout: 5_000 });
  await browser.execute((next) => {
    const textarea = document.querySelector(".page-blocks textarea.block-editor");
    if (!(textarea instanceof HTMLTextAreaElement)) throw new Error("editor missing");
    const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
    setter?.call(textarea, next);
    textarea.dispatchEvent(new InputEvent("input", {
      bubbles: true,
      inputType: "insertText",
      data: next,
    }));
  }, "local laptop draft");

  // Beat the ordinary save debounce with a Syncthing-style atomic replacement.
  const replacement = `${pageFile}.external`;
  fs.writeFileSync(replacement, "- disk from phone\n");
  fs.renameSync(replacement, pageFile);
  await assertLiveConflict(browser, "initial conflict");

  // A whole app restart, not merely navigation/remount. The retained draft is
  // app-private and the graph file remains the external winner.
  await browser.deleteSession();
  browser = undefined;
  await sleep(1500);
  browser = await newSession();
  await openNote(browser);
  await assertLiveConflict(browser, "after restart");

  await browser.execute(() => {
    for (const button of document.querySelectorAll(".page-conflict button")) {
      const text = button.textContent?.trim();
      if (text === "Keep both everywhere") button.setAttribute("data-concord-keep-both", "true");
      if (text === "Apply resolution") button.setAttribute("data-concord-apply", "true");
    }
  });
  const keepBoth = await browser.$('[data-concord-keep-both="true"]');
  await keepBoth.click();
  await browser.$('[data-concord-apply="true"]').click();
  await sleep(500);
  const applyState = await browser.execute(() => ({
    conflicts: document.querySelectorAll(".page-conflict").length,
    toasts: [...document.querySelectorAll(".toast")].map((toast) => toast.textContent),
    capsule: localStorage.getItem("tine.concord.live-conflicts.v1"),
  }));
  if (applyState.conflicts) console.error("APPLY STATE:", JSON.stringify(applyState));
  await waitForFileText(
    pageFile,
    (text) => text.includes("local laptop draft") && text.includes("disk from phone"),
    "no-loss live conflict resolution",
  );
  await browser.waitUntil(async () => (await browser.$$(".page-conflict")).length === 0, {
    timeout: 10_000,
    timeoutMsg: "resolved Concord object stayed visible",
  });
  await browser.waitUntil(async () => {
    const text = await browser.$(".page-blocks").getText();
    return text.includes("local laptop draft") && text.includes("disk from phone");
  }, { timeout: 10_000, timeoutMsg: "resolved page did not install both sides in the UI" });
  const final = fs.readFileSync(pageFile, "utf8");
  if (final.includes("<<<<<<<") || final.includes(">>>>>>>")) {
    throw new Error("resolution wrote conflict markers into the graph");
  }
  console.log("PASS: live Direct conflict survived restart and resolved in-page with both sides");
} catch (error) {
  failure = error;
  console.error("FAIL:", error?.message ?? error);
  try {
    if (browser) {
      const state = await browser.execute(() => ({
        conflicts: document.querySelectorAll(".page-conflict").length,
        page: document.querySelector(".page-blocks")?.textContent,
        capsule: localStorage.getItem("tine.concord.live-conflicts.v1"),
        toasts: [...document.querySelectorAll(".toast")].map((toast) => toast.textContent),
      }));
      console.error("STATE:", JSON.stringify(state));
      fs.writeFileSync("/tmp/e2e-concord-live-save.png", await browser.takeScreenshot(), "base64");
    }
  } catch {}
} finally {
  try { if (browser) await browser.deleteSession(); } catch {}
  try { process.kill(-driver.pid, "SIGKILL"); } catch {}
}

process.exit(failure ? 1 : 0);
