#!/usr/bin/env node

// Focused Windows diagnostic for GH #266 and the GH #543 readiness regression.
// Fixture construction happens before Tine starts; the load ceiling covers the
// conservative full interval from application launch through journal render.
import { spawn, spawnSync } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { ensureMainWindow } from "./lib/e2e-main-window.mjs";
import {
  startWebdriverApplication,
  stopWebdriverApplication,
  tauriCapabilities,
  webdriverServerArgs,
} from "./e2e-capabilities.mjs";

if (process.platform !== "win32") throw new Error("Windows Direct Files large-open diagnostic must run on Windows");
const APP = process.env.TINE_APP;
if (!APP || !fs.existsSync(APP)) throw new Error("HARNESS UNAVAILABLE: Windows Direct Files diagnostic requires TINE_APP");

const TD = process.env.TAURI_DRIVER || "tauri-driver";
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4444);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4445);
const PAGE_COUNT = Number(process.env.E2E_DIRECT_PAGE_COUNT || 13_000);
const ASSET_COUNT = Number(process.env.E2E_DIRECT_ASSET_COUNT || 12_884);
const MAX_LOAD_MS = Number(process.env.E2E_DIRECT_MAX_LOAD_MS || 30_000);
for (const [name, value] of Object.entries({ PAGE_COUNT, ASSET_COUNT, MAX_LOAD_MS })) {
  if (!Number.isSafeInteger(value) || value < 1) throw new Error(`invalid ${name} ${value}`);
}

const root = path.join(os.tmpdir(), `tine-windows-direct-large-${process.pid}`);
const graph = path.join(root, "L-Logseq-笔记");
const artifacts = path.resolve(process.env.E2E_ARTIFACT_DIR || path.join(root, "artifacts"));
const debugLog = path.join(artifacts, "tine-debug.log");
const applicationStderrLog = path.join(artifacts, "application-stderr.log");
const marker = "WINDOWS_DIRECT_LARGE_OPEN_MARKER";
const now = new Date();
const journalStem = `${now.getFullYear()}_${String(now.getMonth() + 1).padStart(2, "0")}_${String(now.getDate()).padStart(2, "0")}`;

fs.rmSync(root, { recursive: true, force: true });
for (const directory of [graph, path.join(graph, "pages"), path.join(graph, "journals"), path.join(graph, "assets"), path.join(graph, "logseq")]) {
  fs.mkdirSync(directory, { recursive: true });
}
fs.mkdirSync(artifacts, { recursive: true });
fs.writeFileSync(path.join(graph, "logseq", "config.edn"), '{:preferred-format "Markdown"}\n');
fs.writeFileSync(path.join(graph, "journals", `${journalStem}.md`), `- ${marker}\n`);
for (let index = 1; index < PAGE_COUNT; index += 1) {
  const bucket = path.join(graph, "pages", `层-${String(index % 141).padStart(3, "0")}`);
  fs.mkdirSync(bucket, { recursive: true });
  fs.writeFileSync(path.join(bucket, `Page ${String(index).padStart(5, "0")}.md`), `- page ${index}\n`);
}
const assetBytes = Buffer.alloc(4096, 0x5a);
for (let index = 0; index < ASSET_COUNT; index += 1) {
  const bucket = path.join(graph, "assets", `资源-${String(index % 140).padStart(3, "0")}`);
  fs.mkdirSync(bucket, { recursive: true });
  fs.writeFileSync(path.join(bucket, `asset-${String(index).padStart(5, "0")}.bin`), assetBytes);
}

const env = {
  ...process.env,
  TINE_GRAPH: graph,
  TINE_DEBUG: "1",
  TINE_DEBUG_LOG: debugLog,
  TINE_E2E_APPLICATION_STDERR_LOG: applicationStderrLog,
  APPDATA: path.join(root, "appdata"),
  LOCALAPPDATA: path.join(root, "localappdata"),
};

function projectionReadyEvidence() {
  if (!fs.existsSync(applicationStderrLog)) return null;
  const lines = fs.readFileSync(applicationStderrLog, "utf8").split(/\r?\n/);
  for (const line of lines.reverse()) {
    const match = line.match(/\[tine\] projection \+(\d+)ms ready at generation=(\d+)/);
    if (match) {
      return {
        line,
        projectionElapsedMs: Number(match[1]),
        generation: Number(match[2]),
      };
    }
  }
  return null;
}

async function openGlobalSwitcher(browser) {
  await browser.keys(["Control", "k"]);
  const input = await browser.$(".switcher-input");
  await input.waitForExist({ timeout: 30_000 });
  await browser.waitUntil(() => input.isFocused(), {
    timeout: 30_000,
    timeoutMsg: "Ctrl+K did not focus the global switcher input",
  });
  const placeholder = await input.getAttribute("placeholder");
  if (placeholder !== "Jump to page, search, or run a command…") {
    throw new Error(`Ctrl+K opened an unexpected switcher mode: ${JSON.stringify(placeholder)}`);
  }
  return input;
}

async function typeLiteralKeys(browser, text) {
  for (const key of text) await browser.keys([key]);
}

async function switcherSearchSnapshot(browser) {
  return browser.execute(() => ({
    pending: document.querySelector('.switcher-empty[role="status"]')?.textContent?.trim() ?? null,
    rows: [...document.querySelectorAll(".switcher-row.block-result")].map((row) => ({
      context: row.querySelector(".search-result-context")?.textContent?.trim() ?? "",
      excerpt: row.querySelector(".search-result-excerpt")?.textContent?.trim() ?? "",
    })),
  }));
}

async function waitForMarkerResult(browser, observePending) {
  let result = null;
  await browser.waitUntil(async () => {
    const snapshot = await switcherSearchSnapshot(browser);
    observePending(snapshot.pending);
    const row = snapshot.rows.find((candidate) => candidate.excerpt.includes(marker));
    if (!row || snapshot.pending) return false;
    result = row;
    return true;
  }, {
    timeout: 90_000,
    interval: 100,
    timeoutMsg: `Ctrl+K did not return the fixture block for ${marker}`,
  });
  return result;
}

const appStartStartedAt = performance.now();
const webviewTarget = await startWebdriverApplication(APP, env, NATIVE_PORT);
const driverLog = fs.openSync(path.join(artifacts, "tauri-driver.log"), "w");
const driver = spawn(TD, webdriverServerArgs(DRIVER_PORT), {
  env: webviewTarget.env,
  stdio: ["ignore", driverLog, driverLog],
});
await sleep(3000);

let browser;
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
  // The Windows driver does not reliably attach to the app window: it can land
  // on the Quick Capture window, where every application selector is legitimately
  // absent. Three windows-smoke journeys failed that way in one run, each
  // reporting its own missing element instead of the shared cause.
  await ensureMainWindow(browser);
  await browser.$(".ls-block").waitForExist({ timeout: 90_000 });
  await browser.waitUntil(async () => (await browser.$("body").getText()).includes(marker), {
    timeout: 90_000,
    timeoutMsg: "large Direct Files graph did not render its journal",
  });
  const appStartToJournalRenderMs = Math.ceil(performance.now() - appStartStartedAt);

  const readyAtFirstSearchStart = projectionReadyEvidence();
  const firstSearchStartedAt = Date.now();
  const firstInput = await openGlobalSwitcher(browser);
  await typeLiteralKeys(browser, marker);
  await browser.waitUntil(async () => (await firstInput.getValue()) === marker, {
    timeout: 30_000,
    timeoutMsg: "literal WebDriver keys did not reach the Ctrl+K input",
  });
  const pendingMessages = new Set();
  const firstResult = await waitForMarkerResult(browser, (pending) => {
    if (pending) pendingMessages.add(pending);
  });
  const firstResultElapsedMs = Date.now() - firstSearchStartedAt;
  const readyAtFirstResult = projectionReadyEvidence();
  const firstResultPhase = readyAtFirstSearchStart
    ? "ready-at-search-start"
    : readyAtFirstResult
      ? "ready-before-result-observation"
      : "pre-ready-cache-result";

  await browser.waitUntil(() => projectionReadyEvidence() !== null, {
    timeout: 90_000,
    interval: 100,
    timeoutMsg: "projection lifecycle trace did not reach ready after the Ctrl+K search",
  });
  const projectionReady = projectionReadyEvidence();
  await browser.keys(["Escape"]);
  await browser.$(".switcher-input").waitForExist({ reverse: true, timeout: 30_000 });

  const postReadySearchStartedAt = Date.now();
  const postReadyInput = await openGlobalSwitcher(browser);
  await typeLiteralKeys(browser, marker);
  await browser.waitUntil(async () => (await postReadyInput.getValue()) === marker, {
    timeout: 30_000,
    timeoutMsg: "literal WebDriver keys did not reach the post-ready Ctrl+K input",
  });
  const postReadyPendingMessages = new Set();
  const postReadyResult = await waitForMarkerResult(browser, (pending) => {
    if (pending) postReadyPendingMessages.add(pending);
  });
  const postReadyResultElapsedMs = Date.now() - postReadySearchStartedAt;
  await browser.keys(["Escape"]);
  await browser.$(".switcher-input").waitForExist({ reverse: true, timeout: 30_000 });

  const log = fs.readFileSync(debugLog, "utf8");
  const backendGraphLoadPhases = log
    .split(/\r?\n/)
    .filter((line) => line.includes("graph load phase:"));
  const receipt = {
    schemaVersion: 3,
    scenario: "windows-direct-large-open",
    pageCount: PAGE_COUNT,
    assetCount: ASSET_COUNT,
    assetBytes: ASSET_COUNT * assetBytes.length,
    nestedDirectoryCount: 281,
    unicodePaths: true,
    appStartToJournalRenderMs,
    maxAppStartToJournalRenderMs: MAX_LOAD_MS,
    appStartMeasurement: "wall clock from immediately before application launch to first observed rendered journal marker",
    backendGraphLoadPhases,
    ctrlK: {
      query: marker,
      input: "literal WebDriver Control+K and character keys",
      firstSearch: {
        elapsedMs: firstResultElapsedMs,
        resultPhase: firstResultPhase,
        indexingObserved: [...pendingMessages].some((message) => /indexing/i.test(message)),
        pendingMessages: [...pendingMessages],
        result: firstResult,
      },
      projectionReady: {
        source: path.basename(applicationStderrLog),
        observed: true,
        ...projectionReady,
      },
      postReadySearch: {
        elapsedMs: postReadyResultElapsedMs,
        indexingObserved: [...postReadyPendingMessages].some((message) => /indexing/i.test(message)),
        pendingMessages: [...postReadyPendingMessages],
        result: postReadyResult,
      },
    },
  };
  fs.writeFileSync(path.join(artifacts, "windows-direct-large-open-receipt.json"), `${JSON.stringify(receipt, null, 2)}\n`);
  if (appStartToJournalRenderMs > MAX_LOAD_MS) {
    throw new Error(`Direct Files app start to rendered journal marker took ${appStartToJournalRenderMs} ms, above severe-regression ceiling ${MAX_LOAD_MS} ms; backendPhases=${JSON.stringify(backendGraphLoadPhases)}`);
  }
  console.log(`PASS: Windows Direct Files reporter-scale open: ${JSON.stringify(receipt)}`);
} catch (error) {
  try { await browser?.saveScreenshot(path.join(artifacts, "failure.png")); } catch {}
  throw error;
} finally {
  try { await browser?.deleteSession(); } catch {}
  spawnSync("taskkill", ["/PID", String(driver.pid), "/T", "/F"], { stdio: "ignore" });
  stopWebdriverApplication(webviewTarget);
  if (process.env.CI === "true") spawnSync("taskkill", ["/IM", path.basename(APP), "/T", "/F"], { stdio: "ignore" });
  fs.closeSync(driverLog);
}
