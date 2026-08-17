#!/usr/bin/env node

// Release-only Linux real-app proof for managed-storage recovery after the OS
// kills Tine. Fixture discovery may query page inventory, but activation,
// navigation, editing, process death, restart, and the final oracle all cross
// the production UI/native boundary.
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ensureDisplay } from "./lib/e2e-display.mjs";

await ensureDisplay();

if (process.platform !== "linux") throw new Error("managed force-close recovery proof is Linux-only");
if (!process.env.TINE_APP) throw new Error("HARNESS UNAVAILABLE: set TINE_APP to the exact candidate");
if (!process.env.TINE_MANAGED_RECOVERY_GRAPH) {
  throw new Error("HARNESS UNAVAILABLE: set TINE_MANAGED_RECOVERY_GRAPH to a read-only source corpus");
}

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APP = path.resolve(process.env.TINE_APP);
const SOURCE = fs.realpathSync(path.resolve(process.env.TINE_MANAGED_RECOVERY_GRAPH));
const TD = process.env.TAURI_DRIVER || "tauri-driver";
const WD = process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver";
const XDOTOOL = process.env.E2E_XDOTOOL || "xdotool";
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4724);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4725);
const SETTLE_MS = Number(process.env.TINE_MANAGED_RECOVERY_SETTLE_MS || 10_000);
const KILL_CYCLES = Number(process.env.TINE_MANAGED_RECOVERY_KILL_CYCLES || 1);
const RETURN_DURING_OPEN = process.env.TINE_MANAGED_RECOVERY_RETURN_DURING_OPEN === "1";
const GRACEFUL_RETURN_AFTER_RECOVERY = process.env.TINE_MANAGED_RECOVERY_GRACEFUL_RETURN === "1";
const REENABLE_AFTER_RETURN = process.env.TINE_MANAGED_RECOVERY_REENABLE_AFTER_RETURN === "1";
if (RETURN_DURING_OPEN && GRACEFUL_RETURN_AFTER_RECOVERY) {
  throw new Error("choose either emergency return during open or graceful return after recovery");
}
if (REENABLE_AFTER_RETURN && !GRACEFUL_RETURN_AFTER_RECOVERY) {
  throw new Error("TINE_MANAGED_RECOVERY_REENABLE_AFTER_RETURN requires graceful return");
}
if (!Number.isSafeInteger(KILL_CYCLES) || KILL_CYCLES < 1 || KILL_CYCLES > 3) {
  throw new Error("TINE_MANAGED_RECOVERY_KILL_CYCLES must be an integer from 1 through 3");
}
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), "tine-managed-force-close-"));
const GRAPH = path.join(TMP, "graph");
const XDG = path.join(TMP, "xdg");
const ARTIFACTS = path.resolve(process.env.E2E_ARTIFACT_DIR || path.join(TMP, "artifacts"));
const MARKER = `managed force-close recovery ${Date.now()}`;
const DIRECT_MARKER = `direct return after managed recovery ${Date.now()}`;

if (!fs.existsSync(APP)) throw new Error(`HARNESS UNAVAILABLE: candidate is missing at ${APP}`);
fs.cpSync(SOURCE, GRAPH, { recursive: true });
if (fs.realpathSync(GRAPH) === SOURCE) throw new Error("refusing to run against the source corpus");
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(path.join(XDG, dir), { recursive: true });
fs.mkdirSync(ARTIFACTS, { recursive: true });

const env = {
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
const xdoEnv = process.env.E2E_XDOTOOL_LIB
  ? { ...env, LD_LIBRARY_PATH: process.env.E2E_XDOTOOL_LIB }
  : env;

let browser;
let driver;
let driverLog;
let appPid;
let bindingGeneration;
let wm;
let wmLog;
let phase = "setup";
let observedManagedRecovery = false;
let selected;

function gitRevision() {
  const result = spawnSync("git", ["rev-parse", "HEAD"], { cwd: ROOT, encoding: "utf8" });
  return result.status === 0 ? result.stdout.trim() : "unavailable";
}

function candidateReceipt() {
  const file = process.env.TINE_BUILD_RECEIPT || `${APP}.build.json`;
  if (!fs.existsSync(file)) return { path: file, present: false };
  try { return { path: file, present: true, value: JSON.parse(fs.readFileSync(file, "utf8")) }; }
  catch (error) { return { path: file, present: true, error: String(error) }; }
}

const receipt = {
  schemaVersion: 1,
  scenario: "managed-force-close-recovery",
  testedCommit: gitRevision(),
  app: APP,
  buildReceipt: candidateReceipt(),
  sourceCorpus: SOURCE,
  graphCopy: GRAPH,
  xdg: XDG,
  artifacts: ARTIFACTS,
  sourceFileCount: 0,
  marker: MARKER,
  markers: [],
  settleMs: SETTLE_MS,
  killCycles: KILL_CYCLES,
  returnDuringOpen: RETURN_DURING_OPEN,
  gracefulReturnAfterRecovery: GRACEFUL_RETURN_AFTER_RECOVERY,
  reenableAfterReturn: REENABLE_AFTER_RETURN,
  activationMs: null,
  reopenMs: null,
  reopenDurationsMs: [],
  processKill: null,
  processKills: [],
  selectedPage: null,
  milestones: {},
};

function processAlive(pid) {
  try { process.kill(pid, 0); return true; }
  catch (error) { return error?.code === "EPERM"; }
}

async function waitFor(predicate, timeoutMs, message, interval = 100) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await predicate();
    if (value) return value;
    await sleep(interval);
  }
  throw new Error(message);
}

function xdo(...args) {
  return execFileSync(XDOTOOL, args, { encoding: "utf8", env: xdoEnv }).trim();
}

function windowIds(pattern = "^Tine( — .*)?$") {
  try { return xdo("search", "--onlyvisible", "--name", pattern).split(/\s+/).filter(Boolean); }
  catch { return []; }
}

function windowManagerReady() {
  try {
    return /window id #/i.test(execFileSync("xprop", ["-root", "_NET_SUPPORTING_WM_CHECK"], {
      encoding: "utf8", env,
    }));
  } catch { return false; }
}

async function bodyText() {
  try { return await browser.$("body").getText(); }
  catch { return ""; }
}

async function waitForBody(text, timeoutMs, label) {
  await browser.waitUntil(async () => (await bodyText()).includes(text), {
    timeout: timeoutMs,
    interval: 250,
    timeoutMsg: `${label} missing ${JSON.stringify(text)}; body=${JSON.stringify((await bodyText()).slice(-3000))}`,
  });
}

async function buttonContaining(text) {
  for (const button of await browser.$$("button")) {
    if ((await button.getText()).includes(text)) return button;
  }
  return undefined;
}

async function exactElement(selector, text) {
  const index = await browser.execute((candidateSelector, candidateText) =>
    [...document.querySelectorAll(candidateSelector)].findIndex((element) =>
      (element.textContent ?? "").trim().normalize("NFC") === candidateText.normalize("NFC")
    ), selector, text);
  return index >= 0 ? (await browser.$$(selector))[index] : undefined;
}

async function invoke(command, args = {}) {
  const result = await browser.executeAsync((cmd, commandArgs, generation, done) => {
    globalThis.__TAURI_INTERNALS__.invoke(cmd, {
      ...commandArgs,
      ...(Number.isInteger(generation) ? { bindingGeneration: generation } : {}),
    }).then(done, (error) => done({ __e2eError: String(error) }));
  }, command, args, bindingGeneration);
  if (result?.__e2eError) throw new Error(`${command} failed: ${result.__e2eError}`);
  return result;
}

async function leaseCurrentGraph() {
  const result = await browser.executeAsync((graph, done) => {
    globalThis.__TAURI_INTERNALS__.invoke("load_graph", { path: graph })
      .then(done, (error) => done({ __e2eError: String(error) }));
  }, GRAPH);
  if (result?.__e2eError || !Number.isInteger(result?.binding_generation)) {
    throw new Error(`could not lease the current graph: ${JSON.stringify(result)}`);
  }
  bindingGeneration = result.binding_generation;
}

async function getPage(entry) {
  return invoke("get_page", { name: entry.name, kind: entry.kind });
}

async function selectExistingEditablePage() {
  const pages = await invoke("list_pages");
  const nameCounts = new Map();
  for (const entry of pages) nameCounts.set(entry.name, (nameCounts.get(entry.name) || 0) + 1);
  for (const entry of pages) {
    if (entry.kind !== "page" || nameCounts.get(entry.name) !== 1) continue;
    const file = path.join(GRAPH, entry.path);
    if (!fs.existsSync(file) || !/\.(md|org)$/i.test(file)) continue;
    let page;
    try { page = await getPage(entry); } catch { continue; }
    const block = page?.blocks?.find((candidate) =>
      typeof candidate.id === "string"
      && candidate.id.length > 0
      && typeof candidate.raw === "string"
      && candidate.raw.trim()
    );
    if (block) return { entry, file, blockId: block.id, originalRaw: block.raw };
  }
  throw new Error("real graph exposed no uniquely named editable page");
}

function flattenBlocks(blocks) {
  return blocks.flatMap((block) => [block, ...flattenBlocks(block.children ?? [])]);
}

async function refreshSelectedManagedBlock() {
  await leaseCurrentGraph();
  const page = await getPage(selected.entry);
  const matches = flattenBlocks(page?.blocks ?? []).filter((block) => block.raw === selected.originalRaw);
  if (matches.length !== 1) {
    throw new Error(`managed activation did not preserve one exact selected block: ${JSON.stringify({
      page: selected.entry.name,
      matches: matches.length,
    })}`);
  }
  selected.blockId = matches[0].id;
  if (receipt.selectedPage) receipt.selectedPage.blockId = selected.blockId;
}

async function openPageThroughSwitcher(name) {
  await browser.keys(["Control", "k"]);
  const input = await browser.$(".switcher-input");
  await input.waitForExist({ timeout: 15_000 });
  await input.setValue(name);
  await browser.waitUntil(() => browser.execute((expected) => {
    const active = document.querySelector(".switcher-row.active");
    return active?.querySelector(".switcher-kind")?.textContent?.trim() === "page"
      && active.querySelector(".switcher-name")?.textContent?.trim().normalize("NFC") === expected.normalize("NFC");
  }, name), { timeout: 30_000, timeoutMsg: `switcher did not select existing page ${JSON.stringify(name)}` });
  await browser.keys("Enter");
  const title = await browser.$("h1.page-title");
  await title.waitForExist({ timeout: 30_000 });
  await browser.waitUntil(async () => (await title.getText()).trim().normalize("NFC") === name.normalize("NFC"), {
    timeout: 30_000,
    timeoutMsg: `visible UI did not open ${JSON.stringify(name)}`,
  });
}

async function focusSelectedEditor(blockId) {
  const target = await browser.$(
    `[data-block-id="${blockId}"] > .block-main > .block-content-wrapper`,
  );
  await target.waitForExist({ timeout: 30_000 });
  await target.waitForDisplayed({ timeout: 30_000 });
  await target.click();
  const editor = await browser.$(`[data-block-id="${blockId}"] textarea.block-editor`);
  await editor.waitForExist({ timeout: 15_000 });
  await editor.waitForDisplayed({ timeout: 15_000 });
  return editor;
}

async function editSelectedPage(marker, label) {
  await refreshSelectedManagedBlock();
  await openPageThroughSwitcher(selected.entry.name);
  const editor = await focusSelectedEditor(selected.blockId);
  await editor.click();
  await browser.keys(["Control", "End"]);
  await editor.addValue(` ${marker}`);
  const editedRaw = await editor.getValue();
  if (!editedRaw.includes(marker) || !editedRaw.startsWith(selected.originalRaw)) {
    throw new Error(`native append did not preserve the selected block: ${JSON.stringify({
      expectedPrefix: selected.originalRaw,
      actual: editedRaw,
    })}`);
  }
  await (await browser.$("h1.page-title")).click();
  await waitFor(() => fs.readFileSync(selected.file, "utf8").includes(marker), 60_000,
    "visible managed edit did not reach the Markdown projection", 200);
  const page = await getPage(selected.entry);
  if (!page.blocks.some((block) => block.raw?.includes(marker))) {
    throw new Error("managed editor save reached Markdown but not the application page view");
  }
  selected.originalRaw = editedRaw;
  receipt.markers.push(marker);
  receipt.milestones[label] = { visibleEditor: true, applicationPage: true, markdownProjection: true };
}

async function openStorageSettings() {
  const trigger = await browser.$('button[title^="Settings"]');
  await trigger.waitForExist({ timeout: 30_000 });
  await trigger.click();
  await browser.$(".settings-modal").waitForExist({ timeout: 30_000 });
  const tab = await waitFor(() => exactElement(".settings-nav-item", "Backups & recovery"), 30_000,
    "Backups & recovery tab was absent");
  await tab.click();
  await waitForBody("Storage & sync", 30_000, "Storage & sync settings");
  const experimental = await browser.$(".settings-experimental .settings-advanced-toggle");
  await experimental.waitForExist({ timeout: 30_000 });
  if ((await experimental.getAttribute("aria-expanded")) !== "true") await experimental.click();
}

async function closeSettings() {
  const close = await browser.$(".settings-pane-head .icon-btn");
  await close.waitForClickable({ timeout: 30_000 });
  await close.click();
  await browser.$(".settings-modal").waitForExist({ reverse: true, timeout: 30_000 });
}

async function acceptNativeConfirmation(label, before, pattern = "^Tine$") {
  const dialog = await waitFor(() => windowIds(pattern).find((id) => !before.has(id)), 30_000,
    `${label} did not show its native confirmation`);
  xdo("windowactivate", "--sync", dialog);
  xdo("key", "--clearmodifiers", "alt+y");
  await waitFor(() => !windowIds(pattern).includes(dialog), 30_000, `${label} confirmation did not close`);
}

async function enableManagedStorage() {
  await openStorageSettings();
  const button = await waitFor(() => buttonContaining("Enable Tine-managed storage..."), 30_000,
    "Enable Tine-managed storage action was absent");
  const before = new Set(windowIds("^Tine$"));
  const started = Date.now();
  await button.click();
  await acceptNativeConfirmation("managed activation", before);
  await waitForBody("Tine-managed storage active", 300_000, "managed activation");
  receipt.activationMs = Date.now() - started;
  await closeSettings();
}

async function assertManagedStorageActive() {
  await openStorageSettings();
  await waitForBody("Tine-managed storage active", 60_000, "managed status after crash reopen");
  const body = await bodyText();
  for (const forbidden of ["native.unavailable", "sync actor refused", "Tine-managed storage needs attention"] ) {
    if (body.includes(forbidden)) throw new Error(`crash reopen exposed ${JSON.stringify(forbidden)}`);
  }
  await closeSettings();
}

async function assertDirectFilesActive() {
  await openStorageSettings();
  await waitForBody("Enable Tine-managed storage...", 60_000, "Direct Files status after recovery return");
  await closeSettings();
}

async function connect(label, timeoutMs = 300_000, lease = true) {
  driverLog = fs.openSync(path.join(ARTIFACTS, `${label}-tauri-driver.log`), "w");
  driver = spawn(TD, ["--port", String(DRIVER_PORT), "--native-port", String(NATIVE_PORT), "--native-driver", WD], {
    env,
    stdio: ["ignore", driverLog, driverLog],
    detached: true,
  });
  await sleep(2500);
  browser = await remote({
    hostname: "127.0.0.1",
    port: DRIVER_PORT,
    path: "/",
    logLevel: "error",
    connectionRetryCount: 1,
    connectionRetryTimeout: 60_000,
    capabilities: { browserName: "wry", "wdio:enforceWebDriverClassic": true, "tauri:options": { application: APP } },
  });
  await browser.waitUntil(async () => {
    const text = await bodyText();
    const startupVisible = !lease && await browser.$(".startup-recovery-overlay").isExisting();
    return startupVisible || text.includes("Journals") || text.includes("Startup:") || text.includes("Native recovery");
  }, { timeout: timeoutMs, interval: 250, timeoutMsg: `${label} painted no startup or graph UI` });
  const window = await waitFor(() => windowIds()[0], 30_000, `${label} native window was absent`);
  appPid = Number(xdo("getwindowpid", window));
  if (!Number.isInteger(appPid) || appPid <= 0) throw new Error(`${label} exposed invalid app pid ${appPid}`);
  if (lease) await leaseCurrentGraph();
  receipt.milestones[label] = { pid: appPid, graph: GRAPH, xdg: XDG };
}

async function returnToDirectFilesDuringOpen() {
  const button = await waitFor(
    () => buttonContaining("to Direct Files"),
    60_000,
    "startup did not expose Return to Direct Files while managed open was active",
  );
  const dialogPattern = "^Return to Direct Files\\?$";
  const before = new Set(windowIds(dialogPattern));
  await button.click();
  await acceptNativeConfirmation("cold Return to Direct Files", before, dialogPattern);
  await browser.$(".startup-recovery-overlay").waitForExist({ reverse: true, timeout: 300_000 });
  await waitForBody("Journals", 60_000, "Direct Files graph after cold return");
  await leaseCurrentGraph();
  await assertDirectFilesActive();
}

async function returnToDirectFilesGracefully() {
  await openStorageSettings();
  const button = await waitFor(() => buttonContaining("Return to Direct files"), 60_000,
    "healthy managed storage did not offer graceful return");
  const before = new Set(windowIds("^Tine$"));
  await button.click();
  await acceptNativeConfirmation("graceful Return to Direct Files", before);
  await waitForBody("Enable Tine-managed storage...", 300_000, "Direct Files after graceful return");
  await closeSettings();
}

async function stopDriver() {
  try { await browser?.deleteSession(); } catch {}
  browser = undefined;
  bindingGeneration = undefined;
  const pid = driver?.pid;
  if (pid) {
    try { process.kill(-pid, "SIGKILL"); } catch {}
    await waitFor(() => driver.exitCode !== null || !processAlive(pid), 30_000, "tauri-driver did not stop");
  }
  driver = undefined;
  try { if (driverLog !== undefined) fs.closeSync(driverLog); } catch {}
  driverLog = undefined;
}

async function forceKillApp(label) {
  const pid = appPid;
  if (!pid) throw new Error("force-close boundary had no application PID");
  const at = new Date().toISOString();
  process.kill(pid, "SIGKILL");
  await waitFor(() => !processAlive(pid), 30_000, `SIGKILL did not stop Tine pid ${pid}`);
  const killed = { label, pid, signal: "SIGKILL", at };
  receipt.processKill = killed;
  receipt.processKills.push(killed);
  appPid = undefined;
  await stopDriver();
}

async function cleanQuit() {
  const pid = appPid;
  if (!pid) return;
  try {
    await browser.executeAsync((done) => {
      globalThis.__TAURI_INTERNALS__.invoke("tine_quit").then(() => done({ ok: true }),
        (error) => done({ error: String(error) }));
    });
  } catch {
    // A successful quit destroys the WebView before WebDriver returns.
  }
  await waitFor(() => !processAlive(pid), 30_000, "Tine did not exit cleanly after recovered use");
  appPid = undefined;
  await stopDriver();
}

async function cleanup() {
  try { if (appPid && processAlive(appPid)) process.kill(appPid, "SIGKILL"); } catch {}
  appPid = undefined;
  try { await stopDriver(); } catch {}
  try { if (wm?.pid) process.kill(-wm.pid, "SIGKILL"); } catch {}
  try { if (wmLog !== undefined) fs.closeSync(wmLog); } catch {}
}

try {
  receipt.sourceFileCount = execFileSync("find", [GRAPH, "-type", "f"], { encoding: "utf8" })
    .trim().split("\n").filter(Boolean).length;
  phase = "window-manager";
  wmLog = fs.openSync(path.join(ARTIFACTS, "openbox.log"), "w");
  wm = spawn(process.env.E2E_WINDOW_MANAGER || "openbox", ["--sm-disable"], {
    env, stdio: ["ignore", wmLog, wmLog], detached: true,
  });
  await waitFor(() => wm.exitCode === null && windowManagerReady(), 15_000,
    "window manager did not become ready");

  phase = "initial-launch";
  await connect("initial");
  selected = await selectExistingEditablePage();
  receipt.selectedPage = {
    name: selected.entry.name,
    kind: selected.entry.kind,
    path: selected.entry.path,
    blockId: selected.blockId,
  };

  phase = "managed-activation";
  await enableManagedStorage();

  for (let cycle = 1; cycle <= KILL_CYCLES; cycle++) {
    const marker = cycle === 1 ? MARKER : `${MARKER} cycle-${cycle}`;
    const editLabel = cycle === 1 ? "edit" : `edit-${cycle}`;
    const reopenLabel = cycle === 1 ? "crash-reopen" : `crash-reopen-${cycle}`;
    phase = `managed-edit-${cycle}`;
    await editSelectedPage(marker, editLabel);

    phase = `settle-${cycle}`;
    await sleep(SETTLE_MS);

    phase = `forced-process-kill-${cycle}`;
    await forceKillApp(`cycle-${cycle}`);

    phase = `crash-reopen-${cycle}`;
    const reopenedAt = Date.now();
    await connect(reopenLabel, 300_000, !RETURN_DURING_OPEN);
    if (RETURN_DURING_OPEN) {
      phase = "cold-return-during-managed-open";
      await returnToDirectFilesDuringOpen();
    }
    await openPageThroughSwitcher(selected.entry.name);
    for (const expected of receipt.markers) {
      await waitForBody(expected, 300_000, `crash-reopened exact edit after cycle ${cycle}`);
      if (!fs.readFileSync(selected.file, "utf8").includes(expected)) {
        throw new Error(`crash-reopened UI retained marker ${JSON.stringify(expected)} but Markdown projection did not`);
      }
    }
    const reopenMs = Date.now() - reopenedAt;
    receipt.reopenMs ??= reopenMs;
    receipt.reopenDurationsMs.push(reopenMs);
    receipt.milestones[`${reopenLabel}-proof`] = { visibleUi: true, markdownProjection: true };
    const recoveryDebugLog = fs.existsSync(path.join(ARTIFACTS, "tine-debug.log"))
      ? fs.readFileSync(path.join(ARTIFACTS, "tine-debug.log"), "utf8") : "";
    observedManagedRecovery ||= /managed storage open:.*(begin|completed)/s.test(recoveryDebugLog);
    if (RETURN_DURING_OPEN) {
      phase = "direct-edit-after-cold-return";
      await editSelectedPage(DIRECT_MARKER, "direct-edit-after-cold-return");
      break;
    }
    await assertManagedStorageActive();
    if (GRACEFUL_RETURN_AFTER_RECOVERY) {
      phase = "graceful-return-after-managed-recovery";
      await returnToDirectFilesGracefully();
      await openPageThroughSwitcher(selected.entry.name);
      await editSelectedPage(DIRECT_MARKER, "direct-edit-after-graceful-return");
      break;
    }
  }

  phase = "clean-shutdown-after-recovery";
  await cleanQuit();
  if (RETURN_DURING_OPEN || GRACEFUL_RETURN_AFTER_RECOVERY) {
    const returnKind = RETURN_DURING_OPEN ? "cold" : "graceful";
    phase = `direct-reopen-after-${returnKind}-return`;
    await connect(`direct-reopen-after-${returnKind}-return`);
    await openPageThroughSwitcher(selected.entry.name);
    await waitForBody(DIRECT_MARKER, 60_000, `Direct Files edit after ${returnKind}-return restart`);
    await assertDirectFilesActive();
    if (REENABLE_AFTER_RETURN) {
      phase = "managed-reenable-after-direct-return";
      const started = Date.now();
      await enableManagedStorage();
      receipt.reenableMs = Date.now() - started;
      await openPageThroughSwitcher(selected.entry.name);
      await waitForBody(DIRECT_MARKER, 60_000, "managed re-enable retained the Direct Files edit");
      await assertManagedStorageActive();
      receipt.milestones.managedReenableAfterDirectReturn = {
        active: true,
        pageVisible: true,
      };
    }
    await cleanQuit();
    receipt.milestones[RETURN_DURING_OPEN
      ? "coldReturnDuringManagedOpen"
      : "gracefulReturnAfterManagedRecovery"] = {
      directMode: true,
      postReturnSave: true,
      restart: true,
    };
  }
  if (!observedManagedRecovery) {
    throw new Error("crash recovery produced no observable managed-storage open progress in the native log");
  }
  receipt.milestones.cleanShutdown = true;
  receipt.milestones.observableRecovery = true;
  receipt.result = "pass";
  fs.writeFileSync(path.join(ARTIFACTS, "managed-force-close-recovery-receipt.json"),
    `${JSON.stringify(receipt, null, 2)}\n`);
  console.log(`PASS: managed real-app force-close recovery held: ${JSON.stringify(receipt)}`);
} catch (error) {
  receipt.result = "fail";
  receipt.phase = phase;
  receipt.error = String(error?.stack || error);
  try { receipt.body = (await bodyText()).slice(-8000); } catch {}
  try { await browser?.saveScreenshot(path.join(ARTIFACTS, "failure.png")); } catch {}
  try { fs.writeFileSync(path.join(ARTIFACTS, "failure-dom.html"), await browser?.getPageSource()); } catch {}
  fs.writeFileSync(path.join(ARTIFACTS, "managed-force-close-recovery-receipt.json"),
    `${JSON.stringify(receipt, null, 2)}\n`);
  console.error(`FAIL: managed real-app force-close recovery: ${JSON.stringify(receipt)}`);
  process.exitCode = 1;
} finally {
  await cleanup();
}
