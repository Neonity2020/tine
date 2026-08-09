#!/usr/bin/env node

// Focused Windows diagnostic for GH #266. Fixture construction happens before
// Tine starts; the asserted interval is only the frontend load_graph call.
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
  await browser.$(".ls-block").waitForExist({ timeout: 90_000 });
  await browser.waitUntil(async () => (await browser.$("body").getText()).includes(marker), {
    timeout: 90_000,
    timeoutMsg: "large Direct Files graph did not render its journal",
  });
  await browser.waitUntil(() => fs.existsSync(debugLog) && fs.readFileSync(debugLog, "utf8").includes("[ui] graph load call returned"), {
    timeout: 30_000,
    timeoutMsg: "debug trace did not record completion of the graph-load call",
  });

  const log = fs.readFileSync(debugLog, "utf8");
  const loading = log.match(/^\[\+\s*(\d+)ms\] \[ui\] loading graph:/m);
  const returned = log.match(/^\[\+\s*(\d+)ms\] \[ui\] graph load call returned/m);
  if (!loading || !returned) throw new Error("debug trace omitted graph-load boundary timestamps");
  const loadCallMs = Number(returned[1]) - Number(loading[1]);
  const phases = log
    .split(/\r?\n/)
    .filter((line) => line.includes("graph load phase:"));
  const receipt = {
    schemaVersion: 1,
    scenario: "windows-direct-large-open",
    pageCount: PAGE_COUNT,
    assetCount: ASSET_COUNT,
    assetBytes: ASSET_COUNT * assetBytes.length,
    nestedDirectoryCount: 281,
    unicodePaths: true,
    loadCallMs,
    maxLoadMs: MAX_LOAD_MS,
    phases,
  };
  fs.writeFileSync(path.join(artifacts, "windows-direct-large-open-receipt.json"), `${JSON.stringify(receipt, null, 2)}\n`);
  if (loadCallMs > MAX_LOAD_MS) {
    throw new Error(`Direct Files load_graph took ${loadCallMs} ms, above severe-regression ceiling ${MAX_LOAD_MS} ms; phases=${JSON.stringify(phases)}`);
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
