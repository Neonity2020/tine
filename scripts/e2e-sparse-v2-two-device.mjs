#!/usr/bin/env node

// Linux real-app proof of the desktop-authority -> fresh-device managed-sync
// lifecycle. Two launches use separate XDG app-data roots, just as desktop and
// Android do; only the graph-local provider tree crosses the device boundary.
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ensureDisplay } from "./lib/e2e-display.mjs";

await ensureDisplay();
if (process.platform !== "linux") throw new Error("sparse-v2 two-device proof is Linux-only");
if (!process.env.TINE_APP) throw new Error("HARNESS UNAVAILABLE: sparse-v2 two-device proof requires TINE_APP");

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APP = path.resolve(process.env.TINE_APP);
const TD = process.env.TAURI_DRIVER || "tauri-driver";
const WD = process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver";
const XDOTOOL = process.env.E2E_XDOTOOL || "xdotool";
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), "tine-sparse-v2-two-device-"));
const GRAPH_A = path.join(TMP, "device-a-graph");
const GRAPH_B = path.join(TMP, "device-b-graph");
const XDG_A = path.join(TMP, "device-a-xdg");
const XDG_B = path.join(TMP, "device-b-xdg");
const ARTIFACTS = path.resolve(process.env.E2E_ARTIFACT_DIR || path.join(TMP, "artifacts"));
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4664);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4665);
const PAGE = "Two Device Managed Sync";
const MARKER = "cross device source bytes";

if (!fs.existsSync(APP)) throw new Error(`HARNESS UNAVAILABLE: exact TINE_APP candidate is missing at ${APP}`);
fs.mkdirSync(ARTIFACTS, { recursive: true });
for (const graph of [GRAPH_A, GRAPH_B]) {
  fs.mkdirSync(path.join(graph, "pages"), { recursive: true });
  fs.mkdirSync(path.join(graph, "journals"), { recursive: true });
  fs.mkdirSync(path.join(graph, "logseq"), { recursive: true });
  fs.writeFileSync(path.join(graph, "pages", `${PAGE}.md`), `- ${MARKER}\n`);
  fs.writeFileSync(path.join(graph, "logseq", "config.edn"), '{:preferred-format "Markdown"}\n');
}
for (const [xdg, graph] of [[XDG_A, GRAPH_A], [XDG_B, GRAPH_B]]) {
  for (const dir of ["data", "config", "cache"]) fs.mkdirSync(path.join(xdg, dir), { recursive: true });
  const appData = path.join(xdg, "data", "page.tine.Tine");
  fs.mkdirSync(appData, { recursive: true });
  fs.writeFileSync(path.join(appData, "tine-settings.json"), `${JSON.stringify({
    native_window_frame: true,
    last_graph_path: fs.realpathSync(graph),
  })}\n`);
}

function gitRevision() {
  const result = spawnSync("git", ["rev-parse", "HEAD"], { cwd: ROOT, encoding: "utf8" });
  return result.status === 0 ? result.stdout.trim() : "unavailable";
}

function deviceEnv(graph, xdg) {
  return {
    ...process.env,
    TINE_GRAPH: graph,
    TINE_DEBUG: "1",
    TINE_DEBUG_LOG: path.join(xdg, "tine-debug.log"),
    XDG_DATA_HOME: path.join(xdg, "data"),
    XDG_CONFIG_HOME: path.join(xdg, "config"),
    XDG_CACHE_HOME: path.join(xdg, "cache"),
    XDG_CONFIG_DIRS: process.env.XDG_CONFIG_DIRS || "/etc/xdg",
    XDG_DATA_DIRS: process.env.XDG_DATA_DIRS || "/usr/local/share:/usr/share",
    WEBKIT_DISABLE_DMABUF_RENDERER: "1",
    WEBKIT_DISABLE_COMPOSITING_MODE: "1",
    LIBGL_ALWAYS_SOFTWARE: "1",
    GDK_BACKEND: "x11",
  };
}

let env = deviceEnv(GRAPH_A, XDG_A);
let browser;
let driver;
let driverLog;
let appPid;
let bindingGeneration;
let wm;
let wmLog;
let phase = "setup";
const receipt = {
  schemaVersion: 1,
  scenario: "sparse-v2-two-device",
  testedCommit: gitRevision(),
  app: APP,
  milestones: {},
};

function processAlive(pid) {
  try { process.kill(pid, 0); return true; } catch (error) { return error?.code === "EPERM"; }
}

async function waitFor(predicate, timeoutMs, message) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const result = await predicate();
    if (result) return result;
    await sleep(75);
  }
  throw new Error(message);
}

function windowIds(pattern = "^Tine( — .*)?$") {
  try {
    return execFileSync(XDOTOOL, ["search", "--onlyvisible", "--name", pattern], {
      encoding: "utf8",
      env,
    }).trim().split(/\s+/).filter(Boolean);
  } catch { return []; }
}

function windowManagerReady() {
  try {
    return /_NET_SUPPORTING_WM_CHECK.*window id/i.test(
      execFileSync("xprop", ["-root", "_NET_SUPPORTING_WM_CHECK"], { encoding: "utf8", env }),
    );
  } catch { return false; }
}

async function assertBody(text, label, timeout = 30_000) {
  await browser.waitUntil(async () => (await browser.$("body").getText()).includes(text), {
    timeout,
    timeoutMsg: `${label} was not visible: ${JSON.stringify(text)}`,
  });
}

async function getPage(name = PAGE) {
  const page = await browser.executeAsync((pageName, done) => {
    globalThis.__TAURI_INTERNALS__.invoke("get_page", {
      name: pageName,
      kind: "page",
      bindingGeneration: Number(document.documentElement.dataset.e2eBindingGeneration),
    })
      .then(done, (error) => done({ error: String(error) }));
  }, name);
  if (page?.error || !page?.blocks?.[0]) {
    throw new Error(`page ${JSON.stringify(name)} did not load: ${JSON.stringify(page)}`);
  }
  return page;
}

async function leaseCurrentGraph(graph) {
  const result = await browser.executeAsync((path, done) => {
    globalThis.__TAURI_INTERNALS__.invoke("load_graph", { path })
      .then(done, (error) => done({ error: String(error) }));
  }, graph);
  if (result?.error || typeof result?.binding_generation !== "number") {
    throw new Error(`current graph binding could not be leased: ${JSON.stringify(result)}`);
  }
  bindingGeneration = result.binding_generation;
  await browser.execute((generation) => {
    document.documentElement.dataset.e2eBindingGeneration = String(generation);
  }, bindingGeneration);
}

async function assertPageContains(text, label, timeout = 30_000) {
  await browser.waitUntil(async () => {
    const page = await getPage();
    return page.blocks.some((block) => block.raw?.includes(text));
  }, { timeout, timeoutMsg: `${label} was not present in the loaded page: ${JSON.stringify(text)}` });
}

async function managedCall(command, args = {}) {
  const result = await browser.executeAsync((cmd, commandArgs, done) => {
    globalThis.__TAURI_INTERNALS__.invoke(cmd, {
      ...commandArgs,
      bindingGeneration: Number(document.documentElement.dataset.e2eBindingGeneration),
    }).then(done, (error) => done({ error: String(error) }));
  }, command, args);
  if (result?.error) throw new Error(`${command} failed: ${result.error}`);
  return result;
}

async function saveManagedRaw(raw) {
  const page = await getPage();
  page.blocks[0].raw = raw;
  await managedCall("save_page", {
    page,
    baseRev: page.rev,
    force: false,
    conflictEpoch: null,
    managedConflictObservation: null,
  });
}

async function settleManaged(label, timeout = 60_000) {
  const deadline = Date.now() + timeout;
  let idleRounds = 0;
  let latest;
  while (Date.now() < deadline) {
    await managedCall("sparse_v2_tick");
    latest = await managedCall("sparse_v2_status");
    const runtime = latest.runtime;
    const idle = runtime?.provider_pending === 0
      && runtime?.managed_local_pending === 0
      && runtime?.watcher?.pending === false
      && runtime?.watcher?.drain_in_flight === false;
    idleRounds = idle ? idleRounds + 1 : 0;
    if (idleRounds >= 2) return latest;
    await sleep(100);
  }
  throw new Error(`${label} did not settle: ${JSON.stringify(latest)}`);
}

async function waitForManagedText(text, label, timeout = 60_000) {
  const deadline = Date.now() + timeout;
  let latest;
  while (Date.now() < deadline) {
    await managedCall("sparse_v2_tick");
    latest = await getPage();
    if (latest.blocks.some((block) => block.raw?.includes(text))) return latest;
    await sleep(100);
  }
  throw new Error(`${label} did not arrive: ${JSON.stringify(latest)}`);
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
      (element.textContent ?? "").trim() === candidateText
    ), selector, text);
  return index >= 0 ? (await browser.$$(selector))[index] : undefined;
}

async function openSyncSettings() {
  const trigger = await browser.$('button[title^="Settings"]');
  await trigger.waitForExist({ timeout: 15_000 });
  await trigger.click();
  await browser.$(".settings-modal").waitForExist({ timeout: 15_000 });
  const tab = await waitFor(() => exactElement(".settings-nav-item", "Backups & recovery"), 15_000,
    "Backups & recovery settings tab was not visible");
  await tab.click();
  const experimental = await browser.$(".settings-experimental .settings-advanced-toggle");
  await experimental.waitForExist({ timeout: 15_000 });
  if ((await experimental.getAttribute("aria-expanded")) !== "true") await experimental.click();
}

async function acceptNativeConfirmation(label, before) {
  const dialogId = await waitFor(() => windowIds(".*").find((id) => !before.has(id)), 15_000,
    `${label} did not open the native confirmation dialog`);
  execFileSync(XDOTOOL, ["windowactivate", "--sync", dialogId], { env });
  execFileSync(XDOTOOL, ["key", "--clearmodifiers", "alt+y"], { env });
  await waitFor(() => !windowIds(".*").includes(dialogId), 12_000,
    `${label} confirmation did not close`);
}

async function clickButtonAndConfirm(text, label) {
  if (text === "to Direct Files") {
    const recoveryAction = await browser.$(".startup-recovery-actions button.danger");
    await recoveryAction.waitForClickable({ timeout: 15_000 });
    const before = new Set(windowIds(".*"));
    await recoveryAction.click();
    await acceptNativeConfirmation(label, before);
    return;
  }
  await waitFor(() => buttonContaining(text), 15_000, `${text} action was not visible`);
  const before = new Set(windowIds(".*"));
  const clicked = await browser.execute((expected) => {
    const button = [...document.querySelectorAll("button")]
      .find((candidate) => candidate.textContent?.includes(expected));
    if (!(button instanceof HTMLButtonElement)) return false;
    button.click();
    return true;
  }, text);
  if (!clicked) throw new Error(`${text} action disappeared before it could be clicked`);
  await acceptNativeConfirmation(label, before);
}

async function connect(label, graph, xdg, recovery = false) {
  env = deviceEnv(graph, xdg);
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
    capabilities: {
      browserName: "wry",
      "wdio:enforceWebDriverClassic": true,
      "tauri:options": { application: APP },
    },
  });
  await browser.$(recovery ? ".startup-recovery-overlay" : ".ls-block, .page-title, .journal-day")
    .waitForExist({ timeout: 90_000 });
  if (!recovery) await leaseCurrentGraph(graph);
  const id = await waitFor(() => windowIds()[0], 15_000, `${label}: native Tine window did not appear`);
  appPid = Number(execFileSync(XDOTOOL, ["getwindowpid", id], { encoding: "utf8", env }).trim());
  receipt.milestones[label] = { graph, xdg };
}

async function stopCurrent() {
  if (browser && appPid && processAlive(appPid)) {
    try {
      await browser.executeAsync((done) => {
        globalThis.__TAURI_INTERNALS__.invoke("tine_quit").then(() => done({ ok: true }),
          (error) => done({ error: String(error) }));
      });
    } catch {
      // A successful tine_quit destroys the WebView before WebDriver can
      // return its promise; process disappearance below is the oracle.
    }
  }
  try { await browser?.deleteSession(); } catch {}
  browser = undefined;
  try { if (appPid && processAlive(appPid)) process.kill(appPid, "SIGTERM"); } catch {}
  if (appPid) await waitFor(() => !processAlive(appPid), 15_000, "Tine did not stop between device turns");
  appPid = undefined;
  bindingGeneration = undefined;
  try { if (driver?.pid) process.kill(-driver.pid, "SIGKILL"); } catch {}
  if (driver?.pid) await waitFor(() => driver.exitCode !== null || !processAlive(driver.pid), 8_000,
    "tauri-driver did not stop between device turns");
  driver = undefined;
  try { if (driverLog !== undefined) fs.closeSync(driverLog); } catch {}
  driverLog = undefined;
}

function copyProvider(fromGraph, toGraph) {
  const source = path.join(fromGraph, ".tine-sync", "v2");
  const target = path.join(toGraph, ".tine-sync", "v2");
  fs.rmSync(target, { recursive: true, force: true });
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.cpSync(source, target, { recursive: true, dereference: false });
}

try {
  phase = "window-manager";
  wmLog = fs.openSync(path.join(ARTIFACTS, "openbox.log"), "w");
  wm = spawn(process.env.E2E_WINDOW_MANAGER || "openbox", ["--sm-disable"], {
    env,
    stdio: ["ignore", wmLog, wmLog],
    detached: true,
  });
  await waitFor(() => wm.exitCode === null && windowManagerReady(), 12_000, "window manager did not become ready");

  phase = "device-a-share";
  await connect("device-a", GRAPH_A, XDG_A);
  await assertPageContains(MARKER, "device A source page");
  await openSyncSettings();
  await clickButtonAndConfirm("Enable Tine-managed storage...", "device-a-enable-managed");
  await assertBody("Tine-managed storage active", "device A managed activation", 120_000);
  await clickButtonAndConfirm("Set up sync with another device...", "device-a-prepare-share");
  // "Tine-managed storage active" is already visible before sharing starts.
  // The provider tree is safe to copy only after the frontend receives the
  // native SharedActive result. Its success notice is the user-visible proof
  // that asynchronous share preparation completed; checking the pre-existing
  // generic "active" label raced a partial provider snapshot.
  await assertBody("Sync is ready to use on another device.", "device A shared activation", 120_000);
  await stopCurrent();
  copyProvider(GRAPH_A, GRAPH_B);

  phase = "device-b-discovery-and-join";
  await connect("device-b-join", GRAPH_B, XDG_B);
  await assertPageContains(MARKER, "device B source page");
  await openSyncSettings();
  // Opening an ordinary Direct Files graph is intentionally not an implicit
  // managed-storage/provider probe. The explicit Join action performs cold
  // discovery and then transitions this fresh installation to SharedActive.
  await clickButtonAndConfirm("Join an existing synced graph...", "device-b-join");
  await assertBody("This device joined the synced graph.", "device B joined state", 120_000);
  await leaseCurrentGraph(GRAPH_B);
  receipt.milestones.join = { openedWithoutLocalBinding: true, joined: true };
  const fromB = `${MARKER} edited on device B`;
  await saveManagedRaw(fromB);
  await settleManaged("device B outbound edit");
  await stopCurrent();

  phase = "device-b-to-device-a";
  copyProvider(GRAPH_B, GRAPH_A);
  await connect("device-a-receive", GRAPH_A, XDG_A);
  await waitForManagedText(fromB, "device B edit on device A");
  const fromA = `${MARKER} edited on device A after B`;
  await saveManagedRaw(fromA);
  await settleManaged("device A outbound edit");
  await stopCurrent();

  phase = "device-a-to-device-b";
  copyProvider(GRAPH_A, GRAPH_B);
  await connect("device-b-receive", GRAPH_B, XDG_B);
  await waitForManagedText(fromA, "device A edit on device B");
  await settleManaged("device B after receiving A edit");
  receipt.milestones.bidirectional = { bToA: true, aToB: true };
  await stopCurrent();

  phase = "fresh-device-direct-return";
  copyProvider(GRAPH_A, GRAPH_B);
  fs.rmSync(XDG_B, { recursive: true, force: true });
  for (const dir of ["data", "config", "cache"]) fs.mkdirSync(path.join(XDG_B, dir), { recursive: true });
  const appData = path.join(XDG_B, "data", "page.tine.Tine");
  fs.mkdirSync(appData, { recursive: true });
  fs.writeFileSync(path.join(appData, "tine-settings.json"), `${JSON.stringify({
    native_window_frame: true,
    last_graph_path: fs.realpathSync(GRAPH_B),
  })}\n`);
  const descriptor = path.join(GRAPH_B, ".tine-sync", "v2", "shared", "outbox", "enrollment", "shared-enrollment-v1.json");
  fs.writeFileSync(descriptor, "{\n");
  await connect("device-b-cold-return", GRAPH_B, XDG_B, true);
  await assertBody("Tine-managed storage sync data appears to still be arriving or is incomplete", "fresh device B recovery screen");
  await clickButtonAndConfirm("to Direct Files", "device-b-cold-return");
  await leaseCurrentGraph(GRAPH_B);
  await assertPageContains(MARKER, "fresh device B source page after Direct Files return", 30_000);
  if (fs.existsSync(path.join(GRAPH_B, ".tine-sync", "v2"))) {
    throw new Error("fresh-device Direct Files return left the live v2 namespace in place");
  }
  const recoveryRoot = path.join(GRAPH_B, ".tine-sync", "recovery");
  if (!fs.existsSync(recoveryRoot) || fs.readdirSync(recoveryRoot).length === 0) {
    throw new Error("fresh-device Direct Files return did not preserve provider state");
  }
  const page = await getPage();
  page.blocks[0].raw = `${MARKER} direct after return`;
  const revision = await browser.executeAsync((candidate, done) => {
    globalThis.__TAURI_INTERNALS__.invoke("save_page", {
      page: candidate,
      baseRev: candidate.rev,
      force: false,
      conflictEpoch: null,
      managedConflictObservation: null,
      bindingGeneration: Number(document.documentElement.dataset.e2eBindingGeneration),
    }).then(done, (error) => done({ error: String(error) }));
  }, page);
  if (revision?.error) throw new Error(`Direct Files save failed after return: ${revision.error}`);
  const directBytes = fs.readFileSync(path.join(GRAPH_B, "pages", `${PAGE}.md`), "utf8");
  if (!directBytes.includes("direct after return")) throw new Error("Direct Files save did not reach Markdown after return");
  receipt.milestones.directReturn = { archivedProvider: true, writableMarkdown: true };

  receipt.result = "pass";
  fs.writeFileSync(path.join(ARTIFACTS, "sparse-v2-two-device-receipt.json"), `${JSON.stringify(receipt, null, 2)}\n`);
  console.log(`PASS: sparse-v2 two-device discovery, join, bidirectional edits, and fresh-device Direct Files return held: ${JSON.stringify(receipt.milestones)}`);
} catch (error) {
  try { await browser?.saveScreenshot(path.join(ARTIFACTS, "failure.png")); } catch {}
  const failure = {
    testedCommit: receipt.testedCommit,
    journey: "sparse-v2-two-device",
    phase,
    expected: "A fresh second device can explicitly discover and join the synchronized graph, exchange edits in both directions, and instead return to writable Direct Files.",
    observed: String(error).split("\n").slice(0, 4).join(" | "),
    classification: /HARNESS UNAVAILABLE|tauri-driver|WebKit|xdotool|window manager|DISPLAY/i.test(String(error))
      ? "infrastructure"
      : "product",
  };
  fs.writeFileSync(path.join(ARTIFACTS, "failure-capsule.json"), `${JSON.stringify(failure, null, 2)}\n`);
  console.error(`E2E FAILURE CAPSULE ${JSON.stringify(failure)}`);
  process.exitCode = 1;
} finally {
  try { await stopCurrent(); } catch {}
  try { if (wm?.pid) process.kill(-wm.pid, "SIGKILL"); } catch {}
  try { if (wmLog !== undefined) fs.closeSync(wmLog); } catch {}
}
