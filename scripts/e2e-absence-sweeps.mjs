#!/usr/bin/env node

// Linux real-app proof for managed-storage group-deletion surfacing, deliberate
// disposition, and Restore. The fixture is synthetic; every action and visible
// assertion crosses the production Tauri/WebKit boundary, while Restore is also
// checked against the graph's Markdown projection on disk.
import { execFileSync, spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import { createConnection } from "node:net";
import os from "node:os";
import path from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { remote } from "webdriverio";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { openPageByName } from "./lib/e2e-navigation.mjs";
import {
  createWebdriverLifecycle,
  tauriCapabilities,
  webdriverServerArgs,
} from "./e2e-capabilities.mjs";

await ensureDisplay();

if (process.platform !== "linux") throw new Error("absence-sweep proof is Linux-only");
if (!process.env.TINE_APP) throw new Error("HARNESS UNAVAILABLE: set TINE_APP to the exact candidate");

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APP = path.resolve(process.env.TINE_APP);
const TD = process.env.TAURI_DRIVER || "tauri-driver";
const WD = process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver";
const XDOTOOL = process.env.E2E_XDOTOOL || "xdotool";
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4724);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4725);
const webdriverLifecycle = createWebdriverLifecycle({
  scenario: "absence-sweeps",
  driverPort: DRIVER_PORT,
  nativePort: NATIVE_PORT,
});
const PAGE_COUNT = 20;
const DELETED_COUNT = 8;
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), "tine-absence-sweeps-"));
const GRAPH = path.join(TMP, "graph");
const XDG = path.join(TMP, "xdg");
const ARTIFACTS = path.resolve(
  process.env.E2E_ARTIFACT_DIR || path.join(ROOT, "artifacts", "e2e-absence-sweeps"),
);
const RECEIPT_PATH = path.join(ARTIFACTS, "absence-sweeps-receipt.json");

if (!fs.existsSync(APP)) throw new Error(`HARNESS UNAVAILABLE: candidate is missing at ${APP}`);
if (!Number.isSafeInteger(DRIVER_PORT) || DRIVER_PORT < 1 || DRIVER_PORT > 65535) {
  throw new Error("E2E_DRIVER_PORT must be a valid TCP port");
}
if (!Number.isSafeInteger(NATIVE_PORT) || NATIVE_PORT < 1 || NATIVE_PORT > 65535) {
  throw new Error("E2E_NATIVE_PORT must be a valid TCP port");
}

for (const dir of [
  path.join(GRAPH, "logseq"),
  path.join(GRAPH, "pages"),
  path.join(XDG, "data"),
  path.join(XDG, "config"),
  path.join(XDG, "cache"),
  ARTIFACTS,
]) fs.mkdirSync(dir, { recursive: true });
fs.writeFileSync(path.join(GRAPH, "logseq", "config.edn"), "{}\n");

const fixturePages = Array.from({ length: PAGE_COUNT }, (_, index) => {
  const ordinal = String(index + 1).padStart(2, "0");
  const name = index < DELETED_COUNT ? `Absence Restore ${ordinal}` : `Absence Control ${ordinal}`;
  const relativePath = `pages/${name}.md`;
  const marker = `synthetic absence-sweep body ${ordinal}`;
  const content = `- ${marker}\n`;
  fs.writeFileSync(path.join(GRAPH, relativePath), content);
  return { name, relativePath, file: path.join(GRAPH, relativePath), marker, content };
});
const deletedPages = fixturePages.slice(0, DELETED_COUNT);

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
  ? { ...env, LD_LIBRARY_PATH: process.env.E2E_XDOTOOL_LIB }
  : env;

let browser;
let driver;
let driverLog;
let appPid;
let wm;
let wmLog;
let phase = "fixture-setup";

// A killed or hung run must still name where it was: persist the phase (and
// a heartbeat timestamp) into the receipt on every transition.
function setPhase(next) {
  phase = next;
  try {
    receipt.phase = next;
    receipt.phaseEnteredAt = new Date().toISOString();
    writeReceipt();
  } catch { /* receipt not writable yet */ }
}

function gitRevision() {
  const result = spawnSync("git", ["rev-parse", "HEAD"], { cwd: ROOT, encoding: "utf8" });
  return result.status === 0 ? result.stdout.trim() : "unavailable";
}

function candidateReceipt() {
  const file = process.env.TINE_BUILD_RECEIPT || `${APP}.build.json`;
  if (!fs.existsSync(file)) return { path: file, present: false };
  try {
    return { path: file, present: true, value: JSON.parse(fs.readFileSync(file, "utf8")) };
  } catch (error) {
    return { path: file, present: true, error: String(error) };
  }
}

const receipt = {
  schemaVersion: 1,
  scenario: "absence-sweeps",
  testedCommit: gitRevision(),
  app: APP,
  buildReceipt: candidateReceipt(),
  graph: GRAPH,
  xdg: XDG,
  artifacts: ARTIFACTS,
  fixturePageCount: PAGE_COUNT,
  deletedPageCount: DELETED_COUNT,
  deletedPages: deletedPages.map(({ name, relativePath, marker, content }) => ({
    name,
    path: relativePath,
    marker,
    content,
  })),
  webdriverLifecycle: webdriverLifecycle.evidence,
  milestones: {},
};

function processAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error?.code === "EPERM";
  }
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

function tcpListening(port) {
  return new Promise((resolve) => {
    const socket = createConnection({ host: "127.0.0.1", port });
    const finish = (ready) => {
      socket.removeAllListeners();
      socket.destroy();
      resolve(ready);
    };
    socket.once("connect", () => finish(true));
    socket.once("error", () => finish(false));
    socket.setTimeout(500, () => finish(false));
  });
}

function xdo(...args) {
  return execFileSync(XDOTOOL, args, { encoding: "utf8", env: xdoEnv }).trim();
}

function windowIds(pattern = "^Tine( — .*)?$") {
  try {
    return xdo("search", "--onlyvisible", "--name", pattern).split(/\s+/).filter(Boolean);
  } catch {
    return [];
  }
}

function windowManagerReady() {
  try {
    return /window id #/i.test(execFileSync("xprop", ["-root", "_NET_SUPPORTING_WM_CHECK"], {
      encoding: "utf8",
      env,
    }));
  } catch {
    return false;
  }
}

async function bodyText() {
  try {
    return await browser.$("body").getText();
  } catch {
    return "";
  }
}


// Artifact screenshots are evidence, not assertions. WebKitWebDriver
// intermittently stalls a session-level call (screenshot GETs in particular;
// UND_ERR_HEADERS_TIMEOUT after ~5 minutes) while the app and the journey's
// semantic state are perfectly healthy — burn-in run 1 failed exactly there
// with the product state verified correct in the failure body. Losing a
// screenshot must not fail the journey.
async function saveEvidenceScreenshot(name) {
  try {
    await webdriverLifecycle.run(
      `screenshot:${name}`,
      () => browser.saveScreenshot(path.join(ARTIFACTS, name)),
    );
  } catch (error) {
    (receipt.screenshotFailures ??= []).push({ name, error: String(error).slice(0, 200) });
    writeReceipt();
  }
}

async function visibleButtons() {
  const result = [];
  for (const button of await browser.$$("button")) {
    if (!await button.isDisplayed()) continue;
    result.push({
      element: button,
      text: (await button.getText()).trim().replace(/\s+/g, " "),
      label: (await button.getAttribute("aria-label") || "").trim(),
    });
  }
  return result;
}

async function exactVisibleButton(text) {
  return (await visibleButtons()).find((button) => button.text.normalize("NFC") === text.normalize("NFC"))?.element;
}

async function panelSnapshot() {
  return browser.execute((names) => {
    const normalized = (value) => (value ?? "").trim().replace(/\s+/g, " ").normalize("NFC");
    const panel = [...document.querySelectorAll("[aria-label]")].find((element) =>
      normalized(element.getAttribute("aria-label")) === "Deleted page recovery"
    );
    if (!panel) return null;
    const text = normalized(panel.textContent);
    const buttons = [...panel.querySelectorAll("button")]
      .filter((button) => button.getClientRects().length > 0)
      .map((button) => normalized(button.textContent));
    const leafTexts = [...panel.querySelectorAll("*")]
      .filter((element) => element.children.length === 0 && element.getClientRects().length > 0)
      .map((element) => normalized(element.textContent));
    return {
      text,
      buttons,
      memberMatches: names.map((name) => leafTexts.filter((value) => value === name.normalize("NFC")).length),
    };
  }, deletedPages.map(({ name }) => name));
}

async function surfaceSnapshot() {
  return browser.execute((count) => {
    const normalized = (value) => (value ?? "").trim().replace(/\s+/g, " ");
    const text = normalized(document.body.textContent);
    const buttons = [...document.querySelectorAll("button")]
      .filter((button) => button.getClientRects().length > 0)
      .map((button) => normalized(button.textContent));
    return {
      toastFamily: text.includes(`${count} pages were deleted together`),
      reviewAction: buttons.includes("Review"),
      dock: buttons.some((label) => label.includes(`${count} deleted pages`) && label.includes("Review")),
    };
  }, DELETED_COUNT);
}

async function recoveryOpenButton() {
  const buttons = await visibleButtons();
  return buttons.find(({ text }) => text.includes(`${DELETED_COUNT} deleted pages`) && text.includes("Review"))?.element
    || buttons.find(({ text }) => text === "Review")?.element;
}

async function openRecoveryPanel(label) {
  const button = await waitFor(() => recoveryOpenButton(), 60_000, `${label}: no recovery Review control appeared`, 200);
  await button.click();
  return waitFor(() => panelSnapshot(), 30_000, `${label}: recovery panel did not open`, 100);
}

function assertLivePanel(snapshot, label) {
  if (!snapshot.text.includes("Tier 3")) throw new Error(`${label}: panel did not identify Tier 3: ${snapshot.text}`);
  if (!snapshot.text.includes(`${DELETED_COUNT} deleted pages`)) {
    throw new Error(`${label}: panel did not show the exact deleted-page count: ${snapshot.text}`);
  }
  if (!/waiting for your decision/i.test(snapshot.text)) {
    throw new Error(`${label}: sweep was not waiting for a decision: ${snapshot.text}`);
  }
  for (const action of ["Restore", "Re-apply", "Keep deletion"]) {
    if (!snapshot.buttons.includes(action)) throw new Error(`${label}: missing ${action} action: ${snapshot.buttons.join(", ")}`);
  }
  const missing = deletedPages.filter((_, index) => snapshot.memberMatches[index] !== 1);
  if (missing.length) {
    throw new Error(`${label}: member rows did not contain each deleted page exactly once: ${JSON.stringify({
      names: missing.map(({ name }) => name),
      matches: snapshot.memberMatches,
    })}`);
  }
}

async function closeRecoveryPanel() {
  const button = (await visibleButtons()).find(({ label }) =>
    label.toLowerCase().includes("close") && label.toLowerCase().includes("deleted page")
  )?.element;
  if (!button) throw new Error("recovery panel exposed no semantic close control");
  await button.click();
  await waitFor(async () => (await panelSnapshot()) === null, 15_000, "recovery panel did not close");
}

async function dismissSweepToast() {
  const outcome = await browser.execute((family) => {
    const buttons = [...document.querySelectorAll('button[aria-label="Dismiss"]')];
    const button = buttons.find((candidate) =>
      (candidate.parentElement?.textContent ?? "").includes(family)
    );
    if (!button) return { clicked: false, alreadyAbsent: true };
    button.click();
    return { clicked: true, alreadyAbsent: false };
  }, `${DELETED_COUNT} pages were deleted together`);
  await waitFor(async () => !(await surfaceSnapshot()).toastFamily, 15_000,
    "group-deletion toast did not dismiss");
  return outcome;
}

async function clickPanelAction(text) {
  const panel = await browser.$('[aria-label="Deleted page recovery"]');
  await panel.waitForDisplayed({ timeout: 15_000 });
  for (const button of await panel.$$("button")) {
    if ((await button.getText()).trim().replace(/\s+/g, " ") === text) {
      await button.click();
      return;
    }
  }
  throw new Error(`recovery panel exposed no exact ${JSON.stringify(text)} action`);
}

// One shared readiness contract for opening a page: find and activate the
// exact non-block row in a single round trip, retry against the routed
// title. scripts/lib/e2e-navigation.mjs explains the re-render flake and
// the block-hit and NFC bugs this removes.
const openPageThroughSwitcher = (name) => openPageByName(browser, name, { timeout: 60_000 });

async function acceptNativeConfirmation(label, before) {
  const dialog = await waitFor(() => windowIds("^Tine$").find((id) => !before.has(id)), 30_000,
    `${label} did not show its native confirmation`);
  xdo("windowactivate", "--sync", dialog);
  xdo("key", "--clearmodifiers", "alt+y");
  await waitFor(() => !windowIds("^Tine$").includes(dialog), 30_000, `${label} confirmation did not close`);
}

async function openStorageSettings() {
  const trigger = await browser.$('button[title^="Settings"]');
  await trigger.waitForDisplayed({ timeout: 30_000 });
  await trigger.click();
  await browser.$(".settings-modal").waitForDisplayed({ timeout: 30_000 });
  const tab = await waitFor(async () => (await visibleButtons()).find(({ text }) => text === "Backups & recovery")?.element,
    30_000, "Backups & recovery settings tab was absent");
  await tab.click();
  await browser.waitUntil(async () => (await bodyText()).includes("Storage & sync"), {
    timeout: 30_000,
    interval: 200,
    timeoutMsg: "Storage & sync settings did not appear",
  });
  const experimental = await browser.$(".settings-experimental .settings-advanced-toggle");
  await experimental.waitForDisplayed({ timeout: 30_000 });
  if ((await experimental.getAttribute("aria-expanded")) !== "true") await experimental.click();
}

async function closeSettings() {
  const close = await browser.$(".settings-pane-head .icon-btn:not(.settings-maximize)");
  await close.waitForClickable({ timeout: 30_000 });
  await close.click();
  await browser.$(".settings-modal").waitForExist({ reverse: true, timeout: 30_000 });
}

async function enableManagedStorage() {
  await openStorageSettings();
  const button = await waitFor(async () => (await visibleButtons())
    .find(({ text }) => text.includes("Enable Tine-managed storage..."))?.element,
  30_000, "Enable Tine-managed storage action was absent");
  const before = new Set(windowIds("^Tine$"));
  await button.click();
  await acceptNativeConfirmation("managed activation", before);
  await browser.waitUntil(async () => (await bodyText()).includes("Tine-managed storage active"), {
    timeout: 300_000,
    interval: 250,
    timeoutMsg: "managed activation did not become active",
  });
  await closeSettings();
}

async function connect(label) {
  await webdriverLifecycle.reap(`${label}:pre-connect`, { graceMs: 0 });
  driverLog = fs.openSync(path.join(ARTIFACTS, `${label}-tauri-driver.log`), "w");
  driver = spawn(TD, webdriverServerArgs(DRIVER_PORT, NATIVE_PORT, WD), {
    env,
    stdio: ["ignore", driverLog, driverLog],
    detached: true,
  });
  await waitFor(() => tcpListening(DRIVER_PORT), 30_000, `${label}: tauri-driver did not listen`);
  browser = await webdriverLifecycle.run(`${label}:create-session`, () => remote({
    hostname: "127.0.0.1",
    port: DRIVER_PORT,
    path: "/",
    logLevel: "error",
    ...webdriverLifecycle.remoteOptions(),
    capabilities: tauriCapabilities(APP, "absence-sweeps"),
  }));
  await browser.waitUntil(async () => {
    const text = await bodyText();
    const startupVisible = await browser.$(".startup-recovery-overlay").isExisting();
    return !startupVisible && text.includes("Journals");
  }, {
    timeout: 300_000,
    interval: 250,
    timeoutMsg: `${label}: graph UI did not become usable`,
  });
  const window = await waitFor(() => windowIds()[0], 30_000, `${label}: native window was absent`);
  appPid = Number(xdo("getwindowpid", window));
  if (!Number.isInteger(appPid) || appPid <= 0) throw new Error(`${label}: invalid app pid ${appPid}`);
  receipt.milestones[label] = { pid: appPid, graph: GRAPH, xdg: XDG };
}

async function stopDriver() {
  const currentBrowser = browser;
  const currentDriver = driver;
  browser = undefined;
  driver = undefined;
  await webdriverLifecycle.stop({ browser: currentBrowser, driver: currentDriver, label: "stop-driver" });
  try {
    if (driverLog !== undefined) fs.closeSync(driverLog);
  } catch {}
  driverLog = undefined;
}

async function cleanQuit(label) {
  const pid = appPid;
  if (!pid) return;
  try {
    await webdriverLifecycle.run(`${label}:tine-quit`, () => browser.executeAsync((done) => {
      globalThis.__TAURI_INTERNALS__.invoke("tine_quit").then(
        () => done({ ok: true }),
        (error) => done({ error: String(error) }),
      );
    }));
  } catch {
    // A successful quit destroys the WebView before WebDriver returns.
  }
  await waitFor(() => !processAlive(pid), 60_000, `${label}: Tine did not exit cleanly`);
  appPid = undefined;
  await stopDriver();
  receipt.milestones[label] = { clean: true };
}

async function cleanup() {
  try {
    if (appPid && processAlive(appPid)) process.kill(appPid, "SIGKILL");
  } catch {}
  appPid = undefined;
  try {
    await stopDriver();
  } catch {}
  try {
    if (wm?.pid) process.kill(-wm.pid, "SIGKILL");
  } catch {}
  try {
    if (wmLog !== undefined) fs.closeSync(wmLog);
  } catch {}
}

function writeReceipt() {
  fs.writeFileSync(RECEIPT_PATH, `${JSON.stringify(receipt, null, 2)}\n`);
}

function expectedOutcomeForPhase() {
  if (phase.includes("dismiss")) return "Closing the panel and dismissing its toast leave the sweep waiting and reopenable.";
  if (phase.includes("restore") || phase.includes("history") || phase.includes("navigation")) {
    return "Restore recreates every deleted Markdown page, keeps the disposed sweep as actionless history, and makes a restored page navigable.";
  }
  if (phase.includes("surface") || phase.includes("panel") || phase.includes("reopen")) {
    return "Reopening the managed graph surfaces one eight-page Tier-3 sweep with its members and three explicit actions.";
  }
  return "The synthetic graph activates managed storage, closes cleanly, and reopens through the native app harness.";
}

try {
  setPhase("window-manager");
  wmLog = fs.openSync(path.join(ARTIFACTS, "openbox.log"), "w");
  wm = spawn(process.env.E2E_WINDOW_MANAGER || "openbox", ["--sm-disable"], {
    env: baseEnv,
    stdio: ["ignore", wmLog, wmLog],
    detached: true,
  });
  await waitFor(() => wm.exitCode === null && windowManagerReady(), 15_000,
    "window manager did not become ready");

  setPhase("initial-launch");
  await connect("initial");

  setPhase("managed-activation");
  await enableManagedStorage();

  setPhase("clean-close-before-external-deletion");
  await cleanQuit("pre-deletion-clean-close");

  setPhase("external-group-deletion");
  for (const page of deletedPages) fs.unlinkSync(page.file);
  if (deletedPages.some(({ file }) => fs.existsSync(file))) {
    throw new Error("synthetic external deletion left one or more target files present");
  }
  receipt.milestones.externalGroupDeletion = deletedPages.map(({ relativePath }) => relativePath);

  setPhase("managed-reopen");
  await connect("managed-reopen");

  setPhase("surface-wait");
  const surfaced = await waitFor(async () => {
    const snapshot = await surfaceSnapshot();
    return (snapshot.toastFamily && snapshot.reviewAction) || snapshot.dock ? snapshot : false;
  }, 120_000, "Tier-3 group deletion exposed neither its warning+Review family nor its recovery dock", 200);
  receipt.milestones.surfaced = surfaced;

  setPhase("clean-close-honored");
  // The previous instance exited through the app's own quit path with every
  // managed slot verified stopped. That exit must not be reported as a crash:
  // a false "did not close cleanly" warning trains users to ignore the real
  // one. (Regression: mark_clean_shutdown was unreachable dead code after
  // tauri's never-returning run(), so every quit left the session-active
  // marker behind.)
  {
    const uncleanToast = await bodyText();
    if (uncleanToast.includes("did not close cleanly")) {
      if (process.env.E2E_ALLOW_UNCLEAN_TOAST !== "1") {
        throw new Error("clean quit was falsely reported as an unclean close on reopen");
      }
      receipt.milestones.uncleanToastTolerated = true;
    }
  }
  receipt.milestones.cleanCloseHonored = true;

  setPhase("initial-panel");
  const initialPanel = await openRecoveryPanel("initial surfacing");
  assertLivePanel(initialPanel, "initial surfacing");
  await saveEvidenceScreenshot("surfaced-panel.png");
  receipt.milestones.initialPanel = {
    tier: "tier3",
    count: DELETED_COUNT,
    members: deletedPages.map(({ name }) => name),
    status: "waiting for your decision",
    actions: ["Restore", "Re-apply", "Keep deletion"],
  };

  setPhase("dismiss-without-disposition");
  await closeRecoveryPanel();
  const toastDismissal = await dismissSweepToast();
  const reopenedPanel = await openRecoveryPanel("post-dismiss reopen");
  assertLivePanel(reopenedPanel, "post-dismiss reopen");
  receipt.milestones.noDispositionOnDismiss = {
    panelClosed: true,
    toastDismissal,
    reopened: true,
    status: "waiting for your decision",
  };

  setPhase("restore-action");
  await clickPanelAction("Restore");
  await waitFor(() => deletedPages.every(({ file, content }) =>
    fs.existsSync(file) && fs.readFileSync(file, "utf8") === content
  ), 180_000, "Restore did not recreate every deleted page with its exact content", 200);
  const history = await waitFor(async () => {
    const snapshot = await panelSnapshot();
    if (!snapshot || !snapshot.text.includes("Restored")) return false;
    const liveActions = ["Restore", "Re-apply", "Keep deletion"]
      .filter((action) => snapshot.buttons.includes(action));
    return liveActions.length === 0 ? snapshot : false;
  }, 180_000, "Restore completed on disk but the panel did not retain actionless Restored history", 200);
  const historyMissing = deletedPages.filter((_, index) => history.memberMatches[index] !== 1);
  if (historyMissing.length) {
    throw new Error(`disposed history lost member rows: ${historyMissing.map(({ name }) => name).join(", ")}`);
  }
  await saveEvidenceScreenshot("restored-history.png");
  receipt.milestones.restore = {
    exactFilesRecreated: true,
    status: "Restored",
    liveActions: 0,
    historyMembers: DELETED_COUNT,
  };

  setPhase("restored-page-navigation");
  await closeRecoveryPanel();
  await openPageThroughSwitcher(deletedPages[0].name);
  await browser.waitUntil(async () => (await bodyText()).includes(deletedPages[0].marker), {
    timeout: 60_000,
    interval: 200,
    timeoutMsg: `restored page ${JSON.stringify(deletedPages[0].name)} opened without its exact content`,
  });
  receipt.milestones.restoredPageNavigation = {
    name: deletedPages[0].name,
    marker: deletedPages[0].marker,
    visible: true,
  };

  setPhase("final-clean-close");
  await cleanQuit("final-clean-close");
  receipt.result = "pass";
  receipt.completedAt = new Date().toISOString();
  writeReceipt();
  console.log(`PASS: native absence-sweep surfacing and Restore held: ${RECEIPT_PATH}`);
} catch (error) {
  receipt.result = "fail";
  receipt.phase = phase;
  receipt.error = String(error?.stack || error);
  const evidence = [RECEIPT_PATH];
  try {
    receipt.body = (await bodyText()).slice(-8000);
  } catch {}
  try {
    const screenshot = path.join(ARTIFACTS, "failure.png");
    await webdriverLifecycle.run("failure:screenshot", () => browser?.saveScreenshot(screenshot));
    evidence.push(screenshot);
  } catch {}
  try {
    const dom = path.join(ARTIFACTS, "failure-dom.html");
    fs.writeFileSync(dom, await webdriverLifecycle.run("failure:page-source", () => browser?.getPageSource()));
    evidence.push(dom);
  } catch {}
  try {
    if (Number.isInteger(appPid) && appPid > 0) {
      const status = fs.existsSync(`/proc/${appPid}/status`)
        ? fs.readFileSync(`/proc/${appPid}/status`, "utf8")
        : "process gone";
      const stacks = [];
      try {
        for (const task of fs.readdirSync(`/proc/${appPid}/task`)) {
          try {
            stacks.push(`task ${task}: ${fs.readFileSync(`/proc/${appPid}/task/${task}/stack`, "utf8").trim()}`);
          } catch {}
        }
      } catch {}
      receipt.appProcessAtFailure = { pid: appPid, status: status.slice(0, 2000), kernelStacks: stacks.slice(0, 64) };
    }
  } catch {}
  const debugLog = path.join(ARTIFACTS, "tine-debug.log");
  if (fs.existsSync(debugLog)) {
    receipt.debugLogExcerpt = fs.readFileSync(debugLog, "utf8").slice(-4000);
    evidence.push(debugLog);
  }
  receipt.failureCapsule = {
    testedCommit: receipt.testedCommit,
    journey: "managed absence-sweep surfacing, no-dispose dismissal, Restore, and disposed history",
    phase,
    expected: expectedOutcomeForPhase(),
    observed: receipt.error,
    evidence,
    classification: /Index out of bounds|element not interactable|WebDriverError/.test(receipt.error)
      ? "harness"
      : ["window-manager", "initial-launch", "managed-reopen"].includes(phase)
        ? "infrastructure"
        : "ambiguous",
  };
  writeReceipt();
  console.error(`FAIL: native absence-sweep journey: ${JSON.stringify(receipt.failureCapsule)}`);
  process.exitCode = 1;
} finally {
  await cleanup();
}
