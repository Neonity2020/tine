#!/usr/bin/env node

// Linux real-app proof that a graph whose Tine-managed storage was written by
// the per-block (P4) layout is preserved and automatically rebuilt from its
// Markdown/Org text by the candidate (p4_store_is_preserved_and_automatically_
// rebuilt_from_text). A P4-built producer creates the private state through
// the production UI; the candidate then opens the same graph and profile
// through the ordinary graph-open path, with no manual reactivation.
//
// The private-state oracle is byte preservation only: every original private
// file must survive, byte-identical, somewhere under the recovery root. Backup
// basename and internal layout are deliberately not asserted. Undrained journal
// files are recorded as preserved evidence; this journey never claims that
// their contents were reconstructed.
import { createHash } from "node:crypto";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitForFileText as waitForPersistedFileText } from "./e2e-file-poll.mjs";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { enableManagedStorage } from "./lib/e2e-managed-activation.mjs";
import { openPageByName } from "./lib/e2e-navigation.mjs";
import {
  createWebdriverLifecycle,
  tauriCapabilities,
  webdriverServerArgs,
} from "./e2e-capabilities.mjs";

await ensureDisplay();

if (process.platform !== "linux") throw new Error("managed P4 rebuild native proof is Linux-only");
if (!process.env.TINE_APP) throw new Error("HARNESS UNAVAILABLE: set TINE_APP to the exact candidate");
if (!process.env.TINE_P4_APP) {
  throw new Error("HARNESS UNAVAILABLE: set TINE_P4_APP to a per-block (P4) producer build");
}

const SCENARIO = "managed-p4-rebuild";
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const CANDIDATE = path.resolve(process.env.TINE_APP);
const PRODUCER = path.resolve(process.env.TINE_P4_APP);
const TD = process.env.TAURI_DRIVER || "tauri-driver";
const WD = process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver";
const XDOTOOL = process.env.E2E_XDOTOOL || "xdotool";
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4734);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4735);
const webdriverLifecycle = createWebdriverLifecycle({
  scenario: SCENARIO,
  driverPort: DRIVER_PORT,
  nativePort: NATIVE_PORT,
});
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), "tine-managed-p4-rebuild-"));
const GRAPH = path.join(TMP, "graph");
const XDG = path.join(TMP, "xdg");
const APP_DATA = path.join(XDG, "data", "page.tine.Tine");
const PRIVATE_BINDINGS = path.join(APP_DATA, "sparse-v2");
const RECOVERY_ROOT = path.join(APP_DATA, "sparse-v2-recovery");
const ARTIFACTS = path.resolve(process.env.E2E_ARTIFACT_DIR || path.join(TMP, "artifacts"));
const MD_PAGE = "P4 Rebuild Markdown";
const ORG_PAGE = "P4 Rebuild Org";
const MD_FILE = path.join(GRAPH, "pages", `${MD_PAGE}.md`);
const ORG_FILE = path.join(GRAPH, "pages", `${ORG_PAGE}.org`);
const MD_EDIT = "p4 producer accepted markdown edit";
const ORG_EDIT = "p4 producer accepted org edit";
const UNDRAINED_EDIT = "p4 producer edit before process death";
const CANDIDATE_EDIT = "candidate edit after automatic rebuild";

for (const app of [CANDIDATE, PRODUCER]) {
  if (!fs.existsSync(app)) throw new Error(`HARNESS UNAVAILABLE: application is missing at ${app}`);
}
fs.mkdirSync(ARTIFACTS, { recursive: true });
for (const dir of ["pages", "journals", "logseq"]) fs.mkdirSync(path.join(GRAPH, dir), { recursive: true });
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(path.join(XDG, dir), { recursive: true });
fs.writeFileSync(path.join(GRAPH, "logseq", "config.edn"), '{:preferred-format "Markdown"}\n');
fs.writeFileSync(MD_FILE, "- p4 first\n- p4 second\n- p4 third\n");
fs.writeFileSync(ORG_FILE, "* p4 org heading\n* p4 org sibling\n");

const baseEnv = {
  ...process.env,
  TINE_GRAPH: GRAPH,
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
const xdo = (...args) => execFileSync(XDOTOOL, args, { encoding: "utf8", env: xdoEnv }).trim();

let browser;
let driver;
let driverLog;
let appPid;
let wm;
let wmLog;
let phase = "setup";
const receipt = {
  schemaVersion: 1,
  scenario: SCENARIO,
  testedCommit: gitRevision(),
  candidate: CANDIDATE,
  producer: PRODUCER,
  fixture: { markdownPage: `pages/${MD_PAGE}.md`, orgPage: `pages/${ORG_PAGE}.org` },
  webdriverLifecycle: webdriverLifecycle.evidence,
  milestones: {},
};

function gitRevision() {
  const result = spawnSync("git", ["rev-parse", "HEAD"], { cwd: ROOT, encoding: "utf8" });
  return result.status === 0 ? result.stdout.trim() : "unavailable";
}

function processAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error?.code === "EPERM";
  }
}

async function waitFor(predicate, timeoutMs, message) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await predicate();
    if (value) return value;
    await sleep(100);
  }
  throw new Error(message);
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
    return /_NET_SUPPORTING_WM_CHECK.*window id/i.test(
      execFileSync("xprop", ["-root", "_NET_SUPPORTING_WM_CHECK"], { encoding: "utf8", env }),
    );
  } catch {
    return false;
  }
}

function sha256(file) {
  return createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

// Relative path -> sha256 for every regular file below `root`.
function treeDigest(root) {
  const digest = {};
  const walk = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (entry.isFile()) digest[path.relative(root, full)] = sha256(full);
    }
  };
  if (fs.existsSync(root)) walk(root);
  return digest;
}

function sourceDigest() {
  const digest = {};
  for (const [relative, hash] of Object.entries(treeDigest(GRAPH))) {
    if (/\.(md|org|edn)$/.test(relative) && !relative.startsWith(".tine-sync")) digest[relative] = hash;
  }
  return digest;
}

function singlePrivateRoot() {
  const entries = fs.existsSync(PRIVATE_BINDINGS)
    ? fs.readdirSync(PRIVATE_BINDINGS, { withFileTypes: true }).filter((entry) => entry.isDirectory())
    : [];
  if (entries.length !== 1) {
    throw new Error(`expected exactly one managed private root, found ${entries.length}`);
  }
  return path.join(PRIVATE_BINDINGS, entries[0].name);
}

// The recovery directory whose tree contains every original private file with
// identical bytes. Its name and internal layout are not part of the contract.
function preservedBackupFor(original) {
  if (!fs.existsSync(RECOVERY_ROOT)) return undefined;
  for (const entry of fs.readdirSync(RECOVERY_ROOT, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const candidate = path.join(RECOVERY_ROOT, entry.name);
    const copy = treeDigest(candidate);
    if (Object.entries(original).every(([relative, hash]) => copy[relative] === hash)) return candidate;
  }
  return undefined;
}

async function bodyText() {
  return browser.execute(() => document.body?.innerText ?? "");
}

async function waitForBody(text, timeoutMs, label) {
  await waitFor(async () => (await bodyText()).includes(text), timeoutMs,
    `${label} was not visible: ${JSON.stringify(text)}`);
}

async function exactElement(selector, text) {
  const expected = text.normalize("NFC");
  const index = await browser.execute((candidateSelector, candidateText) =>
    [...document.querySelectorAll(candidateSelector)].findIndex((element) =>
      (element.textContent ?? "").trim().normalize("NFC") === candidateText
    ), selector, expected);
  return index >= 0 ? (await browser.$$(selector))[index] : undefined;
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
  for (let attempt = 0; attempt < 3 && await browser.$(".settings-modal").isExisting(); attempt += 1) {
    await browser.keys(["Escape"]);
    await sleep(100);
  }
  await browser.$(".settings-modal").waitForExist({ reverse: true, timeout: 10_000 });
}

// Managed storage must reach its usable state on its own. Progress wording and
// duration are acceptable variations; a refusal or attention state is not.
async function assertManagedStorageActive(label, timeoutMs) {
  await openStorageSettings();
  await waitForBody("Tine-managed storage active", timeoutMs, `${label} managed status`);
  const body = await bodyText();
  for (const forbidden of ["native.unavailable", "sync actor refused", "Tine-managed storage needs attention"]) {
    if (body.includes(forbidden)) throw new Error(`${label} exposed ${JSON.stringify(forbidden)}`);
  }
  await closeSettings();
}

async function focusCurrentEditor() {
  let editor = await browser.$(".page-blocks textarea.block-editor, textarea.block-editor");
  if (await editor.isExisting()) return editor;
  const target = await browser.$(".page-blocks .ls-block .block-content-wrapper, .page-blocks .ls-block .block-content");
  await target.waitForExist({ timeout: 10_000 });
  await target.click();
  editor = await browser.$(".page-blocks textarea.block-editor, textarea.block-editor");
  await editor.waitForExist({ timeout: 10_000 });
  return editor;
}

async function waitForFileText(file, predicate, label) {
  await waitForPersistedFileText(file, predicate, `${label} was not durably saved`, { timeoutMs: 30_000 });
}

async function editCurrentPage(marker, file, label) {
  const editor = await focusCurrentEditor();
  await editor.addValue(` ${marker}`);
  // Leaving the editor is the ordinary save path; the file is the oracle.
  await (await browser.$("h1.page-title")).click();
  await waitForFileText(file, (body) => body.includes(marker), label);
}

// Same-page reorder through the production "Move block up" gesture.
async function moveBlockUp(text, file) {
  const index = await browser.execute((wanted) =>
    [...document.querySelectorAll(".page-blocks .ls-block .block-content")].findIndex((element) =>
      (element.textContent ?? "").includes(wanted)
    ), text);
  if (index < 0) throw new Error(`block ${JSON.stringify(text)} was not rendered`);
  await (await browser.$$(".page-blocks .ls-block .block-content"))[index].click();
  await (await browser.$(".page-blocks textarea.block-editor, textarea.block-editor"))
    .waitForExist({ timeout: 10_000 });
  await browser.keys(["Alt", "Shift", "ArrowUp"]);
  await browser.keys(["Escape"]);
  await waitForFileText(file, (body) => body.indexOf("p4 third") >= 0
    && body.indexOf("p4 third") < body.indexOf("p4 second"), "same-page reorder");
}

async function stopDriver() {
  const current = driver;
  const currentBrowser = browser;
  driver = undefined;
  browser = undefined;
  await webdriverLifecycle.stop({ browser: currentBrowser, driver: current, label: "stop-driver" });
  try { if (driverLog !== undefined) fs.closeSync(driverLog); } catch {}
  driverLog = undefined;
}

async function connect(label, app) {
  await webdriverLifecycle.reap(`${label}:pre-connect`, { graceMs: 0 });
  driverLog = fs.openSync(path.join(ARTIFACTS, `${label}-tauri-driver.log`), "w");
  driver = spawn(TD, webdriverServerArgs(DRIVER_PORT, NATIVE_PORT, WD), {
    env,
    stdio: ["ignore", driverLog, driverLog],
    detached: true,
  });
  await sleep(2500);
  browser = await webdriverLifecycle.run(`${label}:create-session`, () => remote({
    hostname: "127.0.0.1",
    port: DRIVER_PORT,
    path: "/",
    logLevel: "error",
    ...webdriverLifecycle.remoteOptions(),
    capabilities: tauriCapabilities(app, SCENARIO),
  }));
  // The graph must open through the ordinary graph-open path; the startup
  // recovery overlay may cover the graph while managed storage is prepared.
  await waitFor(async () => !(await browser.$(".startup-recovery-overlay").isExisting())
    && await browser.$(".ls-block, .page-title, .journal-day").isExisting(),
  300_000, `${label} never reached a usable graph`);
  const id = await waitFor(() => windowIds()[0], 30_000, `${label}: Tine native window did not appear`);
  appPid = Number(xdo("getwindowpid", id));
  if (!Number.isInteger(appPid) || appPid <= 0) throw new Error(`${label}: invalid Tine PID ${appPid}`);
  receipt.milestones[label] = { app, profile: XDG };
}

async function cleanQuit(label) {
  const pid = appPid;
  try {
    await browser.executeAsync((done) => {
      globalThis.__TAURI_INTERNALS__.invoke("tine_quit").then(() => done({ ok: true }),
        (error) => done({ error: String(error) }));
    });
  } catch {
    // A successful quit destroys the WebView before WebDriver returns.
  }
  await waitFor(() => !processAlive(pid), 60_000, `${label}: Tine did not exit cleanly`);
  appPid = undefined;
  await stopDriver();
}

async function killApp(label) {
  const pid = appPid;
  process.kill(pid, "SIGKILL");
  await waitFor(() => !processAlive(pid), 30_000, `${label}: SIGKILL did not stop Tine`);
  appPid = undefined;
  await stopDriver();
}

async function stopApp() {
  const pid = appPid;
  appPid = undefined;
  try { if (pid && processAlive(pid)) process.kill(pid, "SIGKILL"); } catch {}
}

async function stopWindowManager() {
  const current = wm;
  wm = undefined;
  try { if (current?.pid) process.kill(-current.pid, "SIGKILL"); } catch {}
  try { if (wmLog !== undefined) fs.closeSync(wmLog); } catch {}
  wmLog = undefined;
}

function failureClassification(error) {
  const message = String(error);
  if (/HARNESS UNAVAILABLE|tauri-driver|WebKit|xdotool|Openbox|window manager|DISPLAY/i.test(message)) return "infrastructure";
  if (/private root|backup|source bytes|was not durably saved|was not visible|managed status|usable graph|exposed/i.test(message)) return "product";
  return "ambiguous";
}

try {
  phase = "start window manager";
  wmLog = fs.openSync(path.join(ARTIFACTS, "openbox.log"), "w");
  wm = spawn(process.env.E2E_WINDOW_MANAGER || "openbox", ["--sm-disable"], {
    env: baseEnv,
    stdio: ["ignore", wmLog, wmLog],
    detached: true,
  });
  await waitFor(() => wm.exitCode === null && windowManagerReady(), 10_000,
    "window manager did not become ready for the native P4 rebuild journey");

  phase = "P4 producer activation and accepted edits";
  await connect("producer", PRODUCER);
  await enableManagedStorage(browser);
  await openPageByName(browser, MD_PAGE);
  await editCurrentPage(MD_EDIT, MD_FILE, "producer Markdown edit");
  await moveBlockUp("p4 third", MD_FILE);
  await openPageByName(browser, ORG_PAGE);
  await editCurrentPage(ORG_EDIT, ORG_FILE, "producer Org edit");
  await cleanQuit("producer");

  phase = "P4 producer undrained journal";
  await connect("producer-undrained", PRODUCER);
  await assertManagedStorageActive("producer reopen", 120_000);
  await openPageByName(browser, MD_PAGE);
  const editor = await focusCurrentEditor();
  await editor.addValue(` ${UNDRAINED_EDIT}`);
  await (await browser.$("h1.page-title")).click();
  await killApp("producer-undrained");

  phase = "record P4 private and source bytes";
  const privateRoot = singlePrivateRoot();
  const originalPrivate = treeDigest(privateRoot);
  if (Object.keys(originalPrivate).length === 0) throw new Error("P4 producer left an empty private root");
  const originalSource = sourceDigest();
  receipt.milestones.p4State = {
    privateFiles: Object.keys(originalPrivate).length,
    journalFiles: Object.keys(originalPrivate).filter((relative) => /journal/i.test(relative)),
    sourceFiles: Object.keys(originalSource),
  };

  phase = "candidate automatic rebuild";
  await connect("candidate-open", CANDIDATE);
  await assertManagedStorageActive("candidate automatic rebuild", 300_000);
  const sourceAfterRebuild = sourceDigest();
  if (JSON.stringify(sourceAfterRebuild) !== JSON.stringify(originalSource)) {
    throw new Error("automatic rebuild changed Markdown/Org source bytes before the next edit");
  }
  const backup = preservedBackupFor(originalPrivate);
  if (!backup) throw new Error("no backup directory preserves every original private byte");
  receipt.milestones.preservation = {
    sourceBytesUnchanged: true,
    backupPreservesEveryPrivateFile: true,
    // Preserved as evidence only: backup-only history is not replayed.
    undrainedJournalFilesPreserved: receipt.milestones.p4State.journalFiles.length,
  };

  phase = "candidate edit, save, restart";
  await openPageByName(browser, MD_PAGE);
  await editCurrentPage(CANDIDATE_EDIT, MD_FILE, "candidate edit after rebuild");
  await cleanQuit("candidate-open");
  await connect("candidate-restart", CANDIDATE);
  await assertManagedStorageActive("candidate restart", 120_000);
  await openPageByName(browser, MD_PAGE);
  await waitForBody(CANDIDATE_EDIT, 30_000, "candidate edit after restart");
  await waitForFileText(MD_FILE, (body) => body.includes(CANDIDATE_EDIT), "candidate edit after restart");
  await openPageByName(browser, ORG_PAGE);
  await waitForBody(ORG_EDIT, 30_000, "producer Org edit after rebuild");
  await waitForFileText(ORG_FILE, (body) => body.includes(ORG_EDIT), "producer Org edit after rebuild");
  receipt.milestones.candidateRestart = { markdownEditRendered: true, orgTextRendered: true };

  phase = "final clean close";
  await cleanQuit("candidate-restart");
  receipt.result = "pass";
  fs.writeFileSync(path.join(ARTIFACTS, `${SCENARIO}-receipt.json`), `${JSON.stringify(receipt, null, 2)}\n`);
  console.log(`PASS: P4 managed store preserved and rebuilt from text: ${JSON.stringify(receipt.milestones)}`);
} catch (error) {
  try {
    await webdriverLifecycle.run("failure:screenshot",
      () => browser?.saveScreenshot(path.join(ARTIFACTS, "failure.png")));
  } catch {}
  const failure = {
    testedCommit: receipt.testedCommit,
    journey: SCENARIO,
    phase,
    expected: "A P4 managed store is preserved byte-for-byte and rebuilt from Markdown/Org; edits then save and survive restart.",
    observed: String(error).split("\n").slice(0, 4).join(" | "),
    classification: failureClassification(error),
    screenshot: "failure.png",
  };
  fs.writeFileSync(path.join(ARTIFACTS, "failure-capsule.json"), `${JSON.stringify(failure, null, 2)}\n`);
  console.error(`E2E FAILURE CAPSULE ${JSON.stringify(failure)}`);
  process.exitCode = 1;
} finally {
  try { await stopApp(); } catch {}
  try { await stopDriver(); } catch {}
  try { await stopWindowManager(); } catch {}
}
