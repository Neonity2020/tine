#!/usr/bin/env node

// Focused Windows proof for GH #292. This is a real WebView2 + Tauri + NTFS
// journey; Linux already owns the native confirmation-dialog proof, so this
// scenario bypasses only that dialog and exercises the production Settings
// action and native storage commands underneath it.
import crypto from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  startWebdriverApplication,
  stopWebdriverApplication,
  tauriCapabilities,
  webdriverServerArgs,
} from "./e2e-capabilities.mjs";

if (process.platform !== "win32") throw new Error("Windows managed-storage smoke must run on Windows");
const APP = process.env.TINE_APP;
if (!APP || !fs.existsSync(APP)) throw new Error("HARNESS UNAVAILABLE: Windows managed-storage smoke requires TINE_APP");

const TD = process.env.TAURI_DRIVER || "tauri-driver";
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4444);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4445);
const PAGE_COUNT = Number(process.env.E2E_MANAGED_PAGE_COUNT || 13_000);
if (!Number.isSafeInteger(PAGE_COUNT) || PAGE_COUNT < 2) throw new Error(`invalid E2E_MANAGED_PAGE_COUNT ${PAGE_COUNT}`);

const root = path.join(os.tmpdir(), `tine-windows-managed-${process.pid}`);
const graph = path.join(root, "graph-研究");
const artifacts = path.resolve(process.env.E2E_ARTIFACT_DIR || path.join(root, "artifacts"));
const debugLog = path.join(artifacts, "tine-debug.log");
const nestedTitle = "Résumé 日本語";
const nestedMarker = "WINDOWS_MANAGED_NESTED_UTF_MARKER";
const nestedFile = path.join(graph, "pages", "研究", `${nestedTitle}.md`);
const now = new Date();
const journalStem = `${now.getFullYear()}_${String(now.getMonth() + 1).padStart(2, "0")}_${String(now.getDate()).padStart(2, "0")}`;
const journalMarker = "WINDOWS_MANAGED_JOURNAL_MARKER";

fs.rmSync(root, { recursive: true, force: true });
for (const dir of ["pages", "pages/研究", "journals", "logseq", "assets/层级/二"]) {
  fs.mkdirSync(path.join(graph, ...dir.split("/")), { recursive: true });
}
fs.mkdirSync(artifacts, { recursive: true });
fs.writeFileSync(path.join(graph, "logseq", "config.edn"), '{:preferred-format "Markdown"}\n');
fs.writeFileSync(path.join(graph, "journals", `${journalStem}.md`), `- ${journalMarker}\n`);
fs.writeFileSync(nestedFile, `- ${nestedMarker}\n`);
fs.writeFileSync(path.join(graph, "assets", "层级", "二", "fixture.txt"), "nested asset\n");
for (let index = 1; index < PAGE_COUNT; index += 1) {
  const bucket = path.join(graph, "pages", `bucket-${String(index % 37).padStart(2, "0")}`);
  fs.mkdirSync(bucket, { recursive: true });
  const stem = `Windows Managed ${String(index).padStart(5, "0")}`;
  fs.writeFileSync(path.join(bucket, `${stem}.md`), `- fixture block ${index}\n`);
}

function sourceSnapshot() {
  const snapshot = new Map();
  const visit = (directory) => {
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      const absolute = path.join(directory, entry.name);
      if (entry.isDirectory()) {
        visit(absolute);
      } else if (/\.(md|org)$/i.test(entry.name)) {
        const relative = path.relative(graph, absolute).split(path.sep).join("/");
        snapshot.set(relative, crypto.createHash("sha256").update(fs.readFileSync(absolute)).digest("hex"));
      }
    }
  };
  visit(graph);
  return snapshot;
}

function assertSameSource(before, label) {
  const after = sourceSnapshot();
  if (after.size !== before.size) throw new Error(`${label} changed source file count: ${before.size} -> ${after.size}`);
  for (const [relative, digest] of before) {
    if (after.get(relative) !== digest) throw new Error(`${label} changed source bytes for ${relative}`);
  }
}

async function bodyText() {
  return browser.$("body").getText();
}

async function waitForBody(text, timeout, label) {
  await browser.waitUntil(async () => (await bodyText()).includes(text), {
    timeout,
    timeoutMsg: `${label} was not visible: ${JSON.stringify(text)}`,
  });
}

async function buttonContaining(text) {
  for (const button of await browser.$$("button")) {
    if ((await button.getText()).includes(text)) return button;
  }
  return undefined;
}

async function clickButton(text) {
  let button;
  await browser.waitUntil(async () => {
    button = await buttonContaining(text);
    return Boolean(button);
  }, { timeout: 15_000, timeoutMsg: `button was not visible: ${text}` });
  await button.click();
}

async function openManagedSettings() {
  const settings = await browser.$('button[title^="Settings"]');
  await settings.waitForExist({ timeout: 15_000 });
  await settings.click();
  await browser.$(".settings-modal").waitForExist({ timeout: 15_000 });
  for (const item of await browser.$$(".settings-nav-item")) {
    if ((await item.getText()).trim() === "Backups & recovery") {
      await item.click();
      break;
    }
  }
  await waitForBody("Storage & sync", 15_000, "storage settings");
  const experimental = await browser.$(".settings-experimental .settings-advanced-toggle");
  await experimental.waitForExist({ timeout: 15_000 });
  if ((await experimental.getAttribute("aria-expanded")) !== "true") await experimental.click();
  await waitForBody("Testing only.", 15_000, "experimental disclosure");
}

async function closeSettings() {
  await browser.keys("Escape");
  await browser.$(".settings-modal").waitForExist({ reverse: true, timeout: 15_000 });
}

async function installConfirmationBypass() {
  const installed = await browser.execute(() => {
    const internals = window.__TAURI_INTERNALS__;
    if (!internals || typeof internals.invoke !== "function") return false;
    const original = internals.invoke.bind(internals);
    internals.invoke = (command, args, options) => {
      const message = typeof args?.message === "string" ? args.message : "";
      if (command === "plugin:dialog|message"
          && (message.startsWith("Enable Tine-managed storage") || message.startsWith("Return to Direct files"))) {
        return Promise.resolve("Yes");
      }
      return original(command, args, options);
    };
    return true;
  });
  if (!installed) throw new Error("could not install the scoped native-confirmation bypass");
}

async function waitForActivation() {
  const deadline = Date.now() + 12 * 60_000;
  let last = "";
  while (Date.now() < deadline) {
    last = await bodyText();
    if (last.includes("Tine-managed storage active")) return;
    if (last.includes("Setup can be retried.") || last.includes("This graph cannot use Tine-managed storage.")) {
      throw new Error(`managed activation returned non-active status: ${last.slice(-1000)}`);
    }
    await sleep(500);
  }
  throw new Error(`managed activation timed out; last body=${last.slice(-1000)}`);
}

async function openPage(title) {
  await browser.keys(["Control", "k"]);
  const input = await browser.$(".switcher-input");
  await input.waitForExist({ timeout: 15_000 });
  await input.setValue(title);
  let row;
  await browser.waitUntil(async () => {
    for (const candidate of await browser.$$(".switcher-row")) {
      const text = await candidate.getText();
      if (text.includes(title)) {
        row = candidate;
        return true;
      }
    }
    return false;
  }, { timeout: 30_000, timeoutMsg: `Ctrl+K did not find ${title}` });
  await row.click();
  await waitForBody(nestedMarker, 30_000, "nested UTF page after activation");
}

const before = sourceSnapshot();
const env = {
  ...process.env,
  TINE_GRAPH: graph,
  TINE_DEBUG: "1",
  TINE_DEBUG_LOG: debugLog,
  APPDATA: path.join(root, "appdata"),
  LOCALAPPDATA: path.join(root, "localappdata"),
};
const webviewTarget = await startWebdriverApplication(APP, env, NATIVE_PORT);
const driverLog = fs.openSync(path.join(artifacts, "tauri-driver.log"), "w");
const driver = spawn(TD, webdriverServerArgs(DRIVER_PORT), {
  env: webviewTarget.env,
  stdio: ["ignore", driverLog, driverLog],
});
await sleep(3000);

let browser;
const receipt = {
  schemaVersion: 1,
  scenario: "windows-managed-storage",
  pageCount: PAGE_COUNT,
  sourceFiles: before.size,
  graphPathKinds: ["nested", "unicode", "markdown", "asset"],
  milestones: {},
};
try {
  browser = await remote({
    hostname: "127.0.0.1",
    port: DRIVER_PORT,
    path: "/",
    capabilities: tauriCapabilities(APP, "default", process.platform, webviewTarget.debuggerAddress),
    logLevel: "error",
    connectionRetryCount: 1,
    connectionRetryTimeout: 60_000,
  });
  await browser.$(".ls-block").waitForExist({ timeout: 60_000 });
  await waitForBody(journalMarker, 60_000, "initial Direct Files journal");
  receipt.milestones.directFilesOpened = true;

  await openManagedSettings();
  await installConfirmationBypass();
  const activationStarted = Date.now();
  await clickButton("Enable Tine-managed storage...");
  await waitForActivation();
  receipt.milestones.activationMs = Date.now() - activationStarted;
  assertSameSource(before, "managed activation");
  await closeSettings();
  await openPage(nestedTitle);
  receipt.milestones.managedPageOpened = true;

  await openManagedSettings();
  await clickButton("Return to Direct files");
  await waitForBody("Enable Tine-managed storage...", 120_000, "Direct Files status after rollback");
  assertSameSource(before, "return to Direct Files");
  await closeSettings();
  await openPage(nestedTitle);
  receipt.milestones.directFilesRestored = true;
  receipt.result = "pass";
  fs.writeFileSync(path.join(artifacts, "windows-managed-storage-receipt.json"), `${JSON.stringify(receipt, null, 2)}\n`);
  console.log(`PASS: Windows managed storage activation and Direct Files return: ${JSON.stringify(receipt)}`);
} catch (error) {
  try { await browser?.saveScreenshot(path.join(artifacts, "failure.png")); } catch {}
  const failure = { ...receipt, result: "fail", error: String(error).split("\n").slice(0, 8).join(" | ") };
  fs.writeFileSync(path.join(artifacts, "failure-capsule.json"), `${JSON.stringify(failure, null, 2)}\n`);
  throw error;
} finally {
  try { await browser?.deleteSession(); } catch {}
  spawnSync("taskkill", ["/PID", String(driver.pid), "/T", "/F"], { stdio: "ignore" });
  stopWebdriverApplication(webviewTarget);
  if (process.env.CI === "true") spawnSync("taskkill", ["/IM", path.basename(APP), "/T", "/F"], { stdio: "ignore" });
  fs.closeSync(driverLog);
}
