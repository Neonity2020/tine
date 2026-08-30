#!/usr/bin/env node

// Native semantic proof for the managed Journals feed: activation, bounded
// initial window, backwards pagination, and refresh after an accepted edit.
import { execFileSync, spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { remote } from "webdriverio";
import {
  createWebdriverLifecycle,
  tauriCapabilities,
  webdriverServerArgs,
} from "./e2e-capabilities.mjs";
import { ensureDisplay } from "./lib/e2e-display.mjs";

await ensureDisplay();

if (process.platform !== "linux") throw new Error("managed journal-feed proof is Linux-only");
if (!process.env.TINE_APP) throw new Error("HARNESS UNAVAILABLE: set TINE_APP to the exact candidate");

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APP = path.resolve(process.env.TINE_APP);
const TD = process.env.TAURI_DRIVER || "tauri-driver";
const WD = process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver";
const XDOTOOL = process.env.E2E_XDOTOOL || "xdotool";
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4924);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4925);
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), "tine-managed-journal-feed-"));
const GRAPH = path.join(TMP, "graph");
const XDG = path.join(TMP, "xdg");
const ARTIFACTS = path.resolve(process.env.E2E_ARTIFACT_DIR || path.join(TMP, "artifacts"));
const RECEIPT_PATH = path.join(ARTIFACTS, "managed-journal-feed-receipt.json");
const EDIT_MARKER = "accepted managed journal-feed edit";
const pad = (value) => String(value).padStart(2, "0");
const addDays = (date, amount) => new Date(date.getFullYear(), date.getMonth(), date.getDate() + amount, 12);
const stem = (date) => `${pad(date.getDate())}-${pad(date.getMonth() + 1)}-${date.getFullYear()}`;

if (!fs.existsSync(APP)) throw new Error(`HARNESS UNAVAILABLE: candidate is missing at ${APP}`);
for (const directory of [
  path.join(GRAPH, "journals"),
  path.join(GRAPH, "pages"),
  path.join(GRAPH, "logseq"),
  path.join(XDG, "data"),
  path.join(XDG, "config"),
  path.join(XDG, "cache"),
  ARTIFACTS,
]) fs.mkdirSync(directory, { recursive: true });

const today = new Date();
const days = Array.from({ length: 7 }, (_, index) => addDays(today, -index));
const markers = days.map((date, index) => index === 0 ? "MANAGED-FEED-TODAY" : `MANAGED-FEED-PAST-${index}`);
const tallJournal = (marker) => `${[
  `- ${marker}`,
  ...Array.from({ length: 32 }, (_, index) => `- tall journal fixture ${marker} ${index + 1}`),
].join("\n")}\n`;
for (const [index, date] of days.entries()) {
  fs.writeFileSync(path.join(GRAPH, "journals", `${stem(date)}.md`), tallJournal(markers[index]));
}
fs.writeFileSync(path.join(GRAPH, "pages", "Feed Control.md"), "- managed feed route control\n");
fs.writeFileSync(
  path.join(GRAPH, "logseq", "config.edn"),
  '{:preferred-format "Markdown" :journal/file-name-format "dd-MM-yyyy"}\n',
);

const webdriverLifecycle = createWebdriverLifecycle({
  scenario: "managed-journal-feed",
  driverPort: DRIVER_PORT,
  nativePort: NATIVE_PORT,
});
const baseEnv = {
  ...process.env,
  TINE_GRAPH: GRAPH,
  TINE_DEBUG: "1",
  TINE_DEBUG_LOG: path.join(ARTIFACTS, "tine-debug.log"),
  XDG_DATA_HOME: path.join(XDG, "data"),
  XDG_CONFIG_HOME: path.join(XDG, "config"),
  XDG_CACHE_HOME: path.join(XDG, "cache"),
  XDG_CONFIG_DIRS: process.env.XDG_CONFIG_DIRS || "/etc/xdg",
  XDG_DATA_DIRS: process.env.XDG_DATA_DIRS || "/usr/local/share:/usr/share",
  WEBKIT_DISABLE_DMABUF_RENDERER: "1",
  WEBKIT_DISABLE_COMPOSITING_MODE: "1",
  LIBGL_ALWAYS_SOFTWARE: "1",
  GDK_BACKEND: "x11",
};
const env = webdriverLifecycle.taggedEnvironment(baseEnv);
const xdoEnv = process.env.E2E_XDOTOOL_LIB
  ? { ...baseEnv, LD_LIBRARY_PATH: process.env.E2E_XDOTOOL_LIB }
  : baseEnv;
const xdo = (...args) => execFileSync(XDOTOOL, args, { encoding: "utf8", env: xdoEnv }).trim();

let browser;
let driver;
let driverLog;
let wm;
let wmLog;
let phase = "fixture";

function gitRevision() {
  const result = spawnSync("git", ["rev-parse", "HEAD"], { cwd: ROOT, encoding: "utf8" });
  return result.status === 0 ? result.stdout.trim() : "unavailable";
}

const receipt = {
  schemaVersion: 1,
  scenario: "managed-journal-feed",
  testedCommit: gitRevision(),
  app: APP,
  graph: GRAPH,
  artifacts: ARTIFACTS,
  journalMarkers: markers,
  webdriverLifecycle: webdriverLifecycle.evidence,
  milestones: {},
};

function writeReceipt() {
  fs.writeFileSync(RECEIPT_PATH, `${JSON.stringify(receipt, null, 2)}\n`);
}

function processAlive(pid) {
  try { process.kill(pid, 0); return true; } catch (error) { return error?.code === "EPERM"; }
}

async function waitFor(predicate, timeoutMs, message, interval = 100) {
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    try {
      const value = await predicate();
      if (value) return value;
    } catch (error) {
      lastError = error;
    }
    await sleep(interval);
  }
  throw new Error(`${message}${lastError ? `; last observation: ${String(lastError)}` : ""}`);
}

function windowIds(pattern = "^Tine( — .*)?$") {
  try { return xdo("search", "--onlyvisible", "--name", pattern).split(/\s+/).filter(Boolean); } catch { return []; }
}

function windowManagerReady() {
  try {
    return /window id #/i.test(execFileSync("xprop", ["-root", "_NET_SUPPORTING_WM_CHECK"], {
      encoding: "utf8",
      env: baseEnv,
    }));
  } catch { return false; }
}

async function bodyText() {
  return browser.$("body").getText();
}

async function visibleButtonContaining(text) {
  for (const button of await browser.$$("button")) {
    if (await button.isDisplayed() && (await button.getText()).includes(text)) return button;
  }
  return undefined;
}

async function acceptNativeConfirmation(label, before) {
  const dialog = await waitFor(
    () => windowIds("^Tine$").find((id) => !before.has(id)),
    30_000,
    `${label} did not show its native confirmation`,
  );
  xdo("windowactivate", "--sync", dialog);
  xdo("key", "--clearmodifiers", "alt+y");
  await waitFor(() => !windowIds("^Tine$").includes(dialog), 30_000, `${label} confirmation did not close`);
}

async function enableManagedStorage() {
  const trigger = await browser.$('button[title^="Settings"]');
  await trigger.waitForDisplayed({ timeout: 30_000 });
  await trigger.click();
  await browser.$(".settings-modal").waitForDisplayed({ timeout: 30_000 });
  const tab = await waitFor(
    () => visibleButtonContaining("Backups & recovery"),
    30_000,
    "Backups & recovery settings tab was absent",
  );
  await tab.click();
  const experimental = await browser.$(".settings-experimental .settings-advanced-toggle");
  await experimental.waitForDisplayed({ timeout: 30_000 });
  if ((await experimental.getAttribute("aria-expanded")) !== "true") await experimental.click();
  const action = await waitFor(
    () => visibleButtonContaining("Enable Tine-managed storage..."),
    30_000,
    "managed activation action was absent",
  );
  const before = new Set(windowIds("^Tine$"));
  await action.click();
  await acceptNativeConfirmation("managed activation", before);
  await browser.waitUntil(async () => (await bodyText()).includes("Tine-managed storage active"), {
    timeout: 300_000,
    interval: 250,
    timeoutMsg: "managed activation did not reach active",
  });
  const close = await browser.$(".settings-pane-head .icon-btn:not(.settings-maximize)");
  await close.click();
  await browser.$(".settings-modal").waitForExist({ reverse: true, timeout: 30_000 });
}

async function forceManagedFeedReload() {
  const title = await browser.$(".journal-today .journal-title");
  await title.waitForDisplayed({ timeout: 30_000 });
  await title.click();
  const journals = await waitFor(async () => {
    for (const item of await browser.$$(".nav-item")) {
      if ((await item.getText()).trim() === "Journals") return item;
    }
    return undefined;
  }, 30_000, "Journals navigation was absent");
  await journals.click();
  await browser.waitUntil(async () => (await bodyText()).includes(markers[0]), {
    timeout: 60_000,
    interval: 200,
    timeoutMsg: "managed Journals feed did not render today's marker",
  });
}

async function feedText() {
  return browser.execute(() => {
    const scroller = document.querySelector('main.main-content[data-pane-id="main"]');
    return scroller?.querySelector(":scope > .main-content-inner > .page")?.textContent ?? "";
  });
}

async function stopHarness() {
  const currentBrowser = browser;
  const currentDriver = driver;
  browser = undefined;
  driver = undefined;
  await webdriverLifecycle.stop({ browser: currentBrowser, driver: currentDriver, label: "final-cleanup" });
  try { if (driverLog !== undefined) fs.closeSync(driverLog); } catch {}
  driverLog = undefined;
  try { if (wm?.pid) process.kill(-wm.pid, "SIGKILL"); } catch {}
  try { if (wmLog !== undefined) fs.closeSync(wmLog); } catch {}
}

try {
  phase = "window-manager";
  wmLog = fs.openSync(path.join(ARTIFACTS, "openbox.log"), "w");
  wm = spawn(process.env.E2E_WINDOW_MANAGER || "openbox", ["--sm-disable"], {
    env: baseEnv,
    stdio: ["ignore", wmLog, wmLog],
    detached: true,
  });
  await waitFor(() => wm.exitCode === null && windowManagerReady(), 15_000, "window manager did not become ready");

  phase = "native-session";
  await webdriverLifecycle.reap("pre-connect", { graceMs: 0 });
  driverLog = fs.openSync(path.join(ARTIFACTS, "tauri-driver.log"), "w");
  driver = spawn(TD, webdriverServerArgs(DRIVER_PORT, NATIVE_PORT, WD), {
    env,
    stdio: ["ignore", driverLog, driverLog],
    detached: true,
  });
  await sleep(2_500);
  browser = await webdriverLifecycle.run("create-session", () => remote({
    hostname: "127.0.0.1",
    port: DRIVER_PORT,
    path: "/",
    logLevel: "error",
    ...webdriverLifecycle.remoteOptions(),
    capabilities: tauriCapabilities(APP, "managed-journal-feed"),
  }));
  await browser.$(".journal-day, .ls-block").waitForExist({ timeout: 60_000 });

  phase = "managed-activation";
  await enableManagedStorage();
  await forceManagedFeedReload();

  phase = "initial-managed-window";
  const initial = await feedText();
  const initialMarkers = markers.filter((marker) => initial.includes(marker));
  if (initialMarkers.length !== 3 || initialMarkers.some((marker, index) => marker !== markers[index])) {
    throw new Error(`managed feed initial window was not the newest three days: ${JSON.stringify(initialMarkers)}`);
  }
  receipt.milestones.initialManagedWindow = { markers: initialMarkers };

  phase = "managed-pagination";
  const scrollProof = await browser.execute(() => {
    const scroller = document.querySelector(".main-content");
    const sentinel = document.querySelector(".feed-sentinel");
    if (!(scroller instanceof HTMLElement) || !(sentinel instanceof HTMLElement)) return { ok: false };
    const before = scroller.scrollTop;
    const box = scroller.getBoundingClientRect();
    scroller.scrollTop = scroller.scrollHeight;
    scroller.dispatchEvent(new Event("scroll", { bubbles: true }));
    const sentinelBox = sentinel.getBoundingClientRect();
    return {
      ok: scroller.scrollHeight > scroller.clientHeight && scroller.scrollTop > before && sentinelBox.top <= box.bottom,
      before,
      after: scroller.scrollTop,
      clientHeight: scroller.clientHeight,
      scrollHeight: scroller.scrollHeight,
    };
  });
  if (!scrollProof.ok) throw new Error(`could not drive the managed feed to its pagination sentinel: ${JSON.stringify(scrollProof)}`);
  await browser.waitUntil(async () => (await feedText()).includes(markers[3]), {
    timeout: 30_000,
    interval: 200,
    timeoutMsg: "managed feed pagination did not append the next journal window",
  });
  const paginated = await feedText();
  const observed = markers.filter((marker) => paginated.includes(marker));
  const positions = observed.map((marker) => paginated.indexOf(marker));
  if (observed.length < 6 || !positions.every((position, index) => index === 0 || position > positions[index - 1])) {
    throw new Error(`managed feed pagination lost newest-first semantic order: ${JSON.stringify({ observed, positions })}`);
  }
  receipt.milestones.pagination = { scrollProof, observed };

  phase = "accepted-edit-refresh";
  const todayBlock = await waitFor(async () => {
    for (const block of await browser.$$(".journal-today .ls-block")) {
      if ((await block.getText()).includes(markers[0])) return block;
    }
    return undefined;
  }, 30_000, "today's managed journal block was absent after pagination");
  const content = await todayBlock.$(".block-content-wrapper, .block-content");
  await content.click();
  const editor = await browser.$(".journal-today textarea.block-editor, textarea.block-editor");
  await editor.waitForDisplayed({ timeout: 30_000 });
  await editor.setValue(`${markers[0]} ${EDIT_MARKER}`);
  await (await browser.$(".journal-today .journal-title")).click();
  const todayFile = path.join(GRAPH, "journals", `${stem(days[0])}.md`);
  await waitFor(
    () => fs.readFileSync(todayFile, "utf8").includes(EDIT_MARKER),
    30_000,
    "accepted managed edit did not reach the Markdown projection",
  );
  const journals = await waitFor(async () => {
    for (const item of await browser.$$(".nav-item")) {
      if ((await item.getText()).trim() === "Journals") return item;
    }
    return undefined;
  }, 30_000, "Journals navigation was absent after accepted edit");
  await journals.click();
  await browser.waitUntil(async () => (await feedText()).includes(EDIT_MARKER), {
    timeout: 60_000,
    interval: 200,
    timeoutMsg: "reopened managed feed did not show the accepted edit",
  });
  receipt.milestones.acceptedEditRefresh = { marker: EDIT_MARKER, projected: true, rendered: true };
  receipt.result = "pass";
  receipt.completedAt = new Date().toISOString();
  writeReceipt();
  console.log(`PASS: managed journal feed opened, paginated, and refreshed after an accepted edit: ${RECEIPT_PATH}`);
} catch (error) {
  receipt.result = "fail";
  receipt.phase = phase;
  receipt.error = String(error?.stack || error);
  try {
    await webdriverLifecycle.run("failure:screenshot", () => browser?.saveScreenshot(path.join(ARTIFACTS, "failure.png")));
  } catch {}
  const debugLog = path.join(ARTIFACTS, "tine-debug.log");
  if (fs.existsSync(debugLog)) receipt.debugLogExcerpt = fs.readFileSync(debugLog, "utf8").slice(-5000);
  writeReceipt();
  console.error(`FAIL: managed journal-feed journey at ${phase}: ${receipt.error}`);
  process.exitCode = 1;
} finally {
  await stopHarness();
  if (driver?.pid && processAlive(driver.pid)) process.exitCode = 1;
}
