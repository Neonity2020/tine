#!/usr/bin/env node

// Native semantic proof for the managed Journals feed: activation, bounded
// initial window, backwards pagination, and refresh after an accepted edit.
import { execFileSync, spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { Key, remote } from "webdriverio";
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
const RAPID_MOVE_MARKER = "RAPID-MANAGED-MULTI-DAY-MOVE";
const RAPID_MOVE_COMMANDS = 120;
const BULK_SOURCE_PAGE = "Bulk Move Source";
const BULK_DESTINATION_PAGE = "Bulk Move Destination";
const BULK_MARKERS = Array.from({ length: 20 }, (_, index) => `BULK-MOVE-${String(index + 1).padStart(2, "0")}`);
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
const tallJournal = (marker, rapidMove = false) => `${[
  `- ${marker}`,
  ...Array.from({ length: 32 }, (_, index) => `- tall journal fixture ${marker} ${index + 1}`),
  ...(rapidMove ? [`- ${RAPID_MOVE_MARKER}`] : []),
].join("\n")}\n`;
for (const [index, date] of days.entries()) {
  fs.writeFileSync(path.join(GRAPH, "journals", `${stem(date)}.md`), tallJournal(markers[index], index === 0));
}
fs.writeFileSync(path.join(GRAPH, "pages", "Feed Control.md"), "- managed feed route control\n");
fs.writeFileSync(
  path.join(GRAPH, "pages", `${BULK_SOURCE_PAGE}.md`),
  `${BULK_MARKERS.map((marker) => `- ${marker}`).join("\n")}\n`,
);
fs.writeFileSync(
  path.join(GRAPH, "pages", `${BULK_DESTINATION_PAGE}.md`),
  "- BULK-DESTINATION-ANCHOR\n",
);
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

async function openPageThroughSwitcher(name) {
  await browser.keys(["Control", "k"]);
  const input = await browser.$(".switcher-input");
  await input.waitForExist({ timeout: 30_000 });
  await input.setValue(name);
  await browser.waitUntil(() => browser.execute((expected) => {
    const active = document.querySelector(".switcher-row.active");
    return active?.querySelector(".switcher-kind")?.textContent?.trim() === "page"
      && active.querySelector(".switcher-name")?.textContent?.trim() === expected;
  }, name), { timeout: 30_000, timeoutMsg: `switcher did not select ${name}` });
  await browser.keys("Enter");
  const title = await browser.$("h1.page-title");
  await title.waitForExist({ timeout: 30_000 });
  await browser.waitUntil(async () => (await title.getText()).trim() === name, {
    timeout: 30_000,
    timeoutMsg: `visible UI did not open ${name}`,
  });
}

async function visibleRootTexts() {
  return browser.execute(() => [...document.querySelectorAll(".page-blocks > .ls-block")]
    .map((block) => {
      const main = block.querySelector(":scope > .block-main > .block-content-wrapper");
      const editor = main?.querySelector("textarea.block-editor");
      return editor instanceof HTMLTextAreaElement
        ? editor.value.trim()
        : main?.querySelector(":scope > .block-content")?.textContent?.trim() ?? "";
    }));
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
  const content = await todayBlock.$(":scope > .block-main > .block-content-wrapper");
  await content.click();
  await browser.waitUntil(() => browser.execute(() => {
    const editor = document.querySelector(".journal-today textarea.block-editor");
    return editor instanceof HTMLTextAreaElement && document.activeElement === editor;
  }), {
    timeout: 30_000,
    interval: 100,
    timeoutMsg: "clicking today's managed journal block did not mount its active editor",
  });
  const edited = await browser.execute((value) => {
    const editor = document.querySelector(".journal-today textarea.block-editor");
    if (!(editor instanceof HTMLTextAreaElement)) return false;
    const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
    setter?.call(editor, value);
    editor.dispatchEvent(new InputEvent("input", {
      bubbles: true,
      inputType: "insertText",
      data: value,
    }));
    return editor.value === value;
  }, `${markers[0]} ${EDIT_MARKER}`);
  if (!edited) throw new Error("today's active editor rejected the managed feed edit input");
  // End the editor before navigating through the journal title. Clicking the
  // title itself as the blur target races its route load against the save it
  // just started; the correctly guarded stale DTO then emits an unrelated
  // unsaved-instance refusal which can survive into the rapid-move phase.
  await browser.keys(Key.Escape);
  const todayFile = path.join(GRAPH, "journals", `${stem(days[0])}.md`);
  await waitFor(
    () => fs.readFileSync(todayFile, "utf8").includes(EDIT_MARKER),
    30_000,
    "accepted managed edit did not reach the Markdown projection",
  );
  await (await browser.$(".journal-today .journal-title")).click();
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
  const acceptedEditErrors = await browser.execute(() =>
    [...document.querySelectorAll(".toast-error .toast-msg")].map((node) => node.textContent ?? ""));
  if (acceptedEditErrors.length) {
    throw new Error(`accepted managed edit/reopen emitted error notifications: ${JSON.stringify(acceptedEditErrors)}`);
  }
  receipt.milestones.acceptedEditRefresh = { marker: EDIT_MARKER, projected: true, rendered: true };

  phase = "rapid-multi-day-move";
  const rapidBlock = await waitFor(async () => {
    for (const block of await browser.$$(".journal-today .ls-block")) {
      if ((await block.getText()).includes(RAPID_MOVE_MARKER)) return block;
    }
    return undefined;
  }, 30_000, "rapid managed move block was absent from today's journal");
  await rapidBlock.$(":scope > .block-main > .block-content-wrapper").click();
  await browser.waitUntil(() => browser.execute((marker) => {
    const editor = document.querySelector(".journal-today textarea.block-editor");
    return editor instanceof HTMLTextAreaElement
      && document.activeElement === editor
      && editor.value.includes(marker);
  }, RAPID_MOVE_MARKER), {
    timeout: 30_000,
    interval: 100,
    timeoutMsg: "rapid managed move block did not own the active editor",
  });
  const rapidStartedAt = Date.now();
  // Deliberately do not wait for any UI or actor cycle between commands. This
  // reproduces the physical key-repeat failure where several page-boundary
  // moves were captured against one stale source page and then flooded errors.
  const dispatched = await browser.execute(async (count, marker) => {
    let accepted = 0;
    for (let index = 0; index < count; index++) {
      // A cross-page render replaces the textarea. Physical key repeat follows
      // the newly focused editor; a synthetic loop that retains the first DOM
      // node silently stops bubbling after that node is detached. Reacquire the
      // live editor, but never wait for the managed actor or durable projection
      // between commands.
      let editor;
      const deadline = performance.now() + 1_000;
      do {
        editor = [...document.querySelectorAll("textarea.block-editor")]
          .find((candidate) => candidate instanceof HTMLTextAreaElement
            && candidate.value.includes(marker));
        if (!(editor instanceof HTMLTextAreaElement)) {
          await new Promise((resolve) => setTimeout(resolve, 1));
        }
      } while (!(editor instanceof HTMLTextAreaElement) && performance.now() < deadline);
      if (!(editor instanceof HTMLTextAreaElement)) break;
      editor.dispatchEvent(new KeyboardEvent("keydown", {
        key: "ArrowDown",
        code: "ArrowDown",
        altKey: true,
        shiftKey: true,
        bubbles: true,
        cancelable: true,
      }));
      editor.dispatchEvent(new KeyboardEvent("keyup", {
        key: "ArrowDown",
        code: "ArrowDown",
        altKey: true,
        shiftKey: true,
        bubbles: true,
        cancelable: true,
      }));
      accepted++;
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
    return accepted;
  }, RAPID_MOVE_COMMANDS, RAPID_MOVE_MARKER);
  if (dispatched !== RAPID_MOVE_COMMANDS) {
    throw new Error(`rapid managed move dispatched only ${dispatched}/${RAPID_MOVE_COMMANDS} commands`);
  }
  const rapidDestination = path.join(GRAPH, "journals", `${stem(days[4])}.md`);
  await waitFor(
    () => fs.readFileSync(rapidDestination, "utf8").includes(RAPID_MOVE_MARKER),
    180_000,
    `rapid managed move did not reach the fifth loaded journal after ${RAPID_MOVE_COMMANDS} commands`,
    100,
  );
  await sleep(1_000);
  const rapidLocations = days
    .map((date, index) => ({
      index,
      path: path.join(GRAPH, "journals", `${stem(date)}.md`),
    }))
    .filter(({ path: journalPath }) => fs.readFileSync(journalPath, "utf8").includes(RAPID_MOVE_MARKER));
  const rapidErrors = await browser.execute(() =>
    [...document.querySelectorAll(".toast-error .toast-msg")].map((node) => node.textContent ?? ""));
  if (rapidLocations.length !== 1 || rapidLocations[0].index !== 4) {
    throw new Error(`rapid managed move did not settle exactly once on the expected day: ${JSON.stringify(rapidLocations)}`);
  }
  if (rapidErrors.length) {
    throw new Error(`rapid managed move emitted error notifications: ${JSON.stringify(rapidErrors)}`);
  }
  receipt.milestones.rapidMultiDayMove = {
    marker: RAPID_MOVE_MARKER,
    commands: RAPID_MOVE_COMMANDS,
    interCommandDelayMs: 5,
    crossedDayBoundaries: 4,
    destinationDayIndex: rapidLocations[0].index,
    elapsedMs: Date.now() - rapidStartedAt,
    errorNotifications: rapidErrors,
  };

  phase = "bulk-cross-page-cut-paste";
  const bulkStartedAt = Date.now();
  await openPageThroughSwitcher(BULK_SOURCE_PAGE);
  await waitFor(async () => (await visibleRootTexts()).length === BULK_MARKERS.length,
    30_000, "bulk source did not render all 20 roots");
  const firstBulk = await browser.$(".page-blocks > .ls-block:first-child > .block-main > .block-content-wrapper");
  await firstBulk.click();
  await browser.$("textarea.block-editor").waitForExist({ timeout: 10_000 });
  await browser.keys(Key.Escape);
  for (let index = 1; index < BULK_MARKERS.length; index++) {
    await browser.keys([Key.Shift, Key.ArrowDown]);
  }
  await waitFor(() => browser.execute((expected) =>
    [...document.querySelectorAll(".page-blocks > .ls-block > .block-main.selected")].length === expected,
  BULK_MARKERS.length), 20_000, "bulk source selection did not include all 20 roots");
  await browser.keys(["Control", "x"]);
  // An empty page deliberately renders one blank root so the user has a place
  // to type. Assert that the cut payload disappeared, rather than requiring an
  // impossible zero-node DOM.
  await waitFor(async () => {
    const roots = await visibleRootTexts();
    return BULK_MARKERS.every((marker) => !roots.includes(marker));
  }, 60_000, "bulk cut did not clear the selected source roots");
  await openPageThroughSwitcher(BULK_DESTINATION_PAGE);
  const destinationAnchor = await browser.$(".page-blocks > .ls-block:first-child > .block-main > .block-content-wrapper");
  await destinationAnchor.click();
  await browser.$("textarea.block-editor").waitForExist({ timeout: 10_000 });
  // WebKitWebDriver's synthetic Control+V does not dispatch a paste event on
  // this host. Drive the same application boundary directly: the real cut
  // above owns Tine's private one-shot payload; this event supplies the exact
  // public text flavor which authorizes that payload association.
  const bulkClipboardText = BULK_MARKERS.map((marker) => `- ${marker}`).join("\n");
  const pasteDispatched = await browser.execute((text) => {
    const editor = document.querySelector("textarea.block-editor");
    if (!(editor instanceof HTMLTextAreaElement)
      || typeof DataTransfer !== "function"
      || typeof ClipboardEvent !== "function") return false;
    const clipboard = new DataTransfer();
    clipboard.setData("text/plain", text);
    return !editor.dispatchEvent(new ClipboardEvent("paste", {
      bubbles: true,
      cancelable: true,
      clipboardData: clipboard,
    }));
  }, bulkClipboardText);
  if (!pasteDispatched) throw new Error("bulk paste event was not claimed by the Tine editor");
  await waitFor(async () => {
    const roots = await visibleRootTexts();
    return BULK_MARKERS.every((marker) => roots.includes(marker));
  }, 60_000, "bulk paste did not install all 20 roots on the destination page");
  const bulkSourcePath = path.join(GRAPH, "pages", `${BULK_SOURCE_PAGE}.md`);
  const bulkDestinationPath = path.join(GRAPH, "pages", `${BULK_DESTINATION_PAGE}.md`);
  await waitFor(() => {
    const sourceText = fs.readFileSync(bulkSourcePath, "utf8");
    const destinationText = fs.readFileSync(bulkDestinationPath, "utf8");
    return BULK_MARKERS.every((marker) => !sourceText.includes(marker) && destinationText.includes(marker));
  }, 90_000, "bulk cut/paste did not become durable on both pages", 200);
  const bulkErrors = await browser.execute(() =>
    [...document.querySelectorAll(".toast-error .toast-msg")].map((node) => node.textContent ?? ""));
  if (bulkErrors.length) {
    throw new Error(`bulk cross-page cut/paste emitted error notifications: ${JSON.stringify(bulkErrors)}`);
  }
  receipt.milestones.bulkCrossPageCutPaste = {
    roots: BULK_MARKERS.length,
    sourcePage: BULK_SOURCE_PAGE,
    destinationPage: BULK_DESTINATION_PAGE,
    elapsedMs: Date.now() - bulkStartedAt,
    errorNotifications: bulkErrors,
  };
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
