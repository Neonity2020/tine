#!/usr/bin/env node

// Hosted-Windows reproduction for GH #295. The graph is the reporter's public,
// digest-pinned anonymized fixture; every typed character is a real WebDriver
// key event delivered to WebView2.
import crypto from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import { performance } from "node:perf_hooks";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  freeLoopbackPort,
  selectWebdriverWindowWithSelector,
  startWebdriverApplication,
  stopWebdriverApplication,
  tauriCapabilities,
  webdriverServerArgs,
} from "./e2e-capabilities.mjs";

if (process.platform !== "win32") throw new Error("GH #295 latency proof must run on Windows");
const APP = process.env.TINE_APP;
const PUBLIC_GRAPH = process.env.TINE_295_PUBLIC_GRAPH;
if (!APP || !fs.existsSync(APP)) throw new Error("HARNESS UNAVAILABLE: TINE_APP is required");
if (!PUBLIC_GRAPH || !fs.existsSync(PUBLIC_GRAPH)) {
  throw new Error("HARNESS UNAVAILABLE: TINE_295_PUBLIC_GRAPH is required");
}

const EXPECTED_PAGE_SHA256 = "4ee3ac756666331cf666246096b3bf55879ad3ed69d6c4fbcca8d91afc92704d";
const PAGE_TITLE = "Zojigc";
const MARKER = "sometimes even hangs for a few seconds";
const TYPED = "[[typing refference here lags a lot";
const TD = process.env.TAURI_DRIVER || "tauri-driver";
function configuredPort(name) {
  const value = process.env[name];
  if (value === undefined || value === "") return undefined;
  const port = Number(value);
  if (!Number.isInteger(port) || port < 1 || port > 65_535) {
    throw new Error(`${name} must be an integer TCP port`);
  }
  return port;
}
const DRIVER_PORT = configuredPort("E2E_DRIVER_PORT") ?? await freeLoopbackPort();
const NATIVE_PORT = configuredPort("E2E_NATIVE_PORT")
  ?? await freeLoopbackPort(new Set([DRIVER_PORT]));
const KEY_PERIOD_MS = Number(process.env.TINE_295_KEY_PERIOD_MS || 115);
const root = path.join(os.tmpdir(), `tine-295-${process.pid}`);
const graph = path.join(root, "graph");
const artifacts = path.resolve(process.env.E2E_ARTIFACT_DIR || path.join(root, "artifacts"));
const debugLog = path.join(artifacts, "tine-debug.log");

function sha256(file) {
  return crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

function percentile(values, quantile) {
  if (!values.length) return null;
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.ceil(quantile * sorted.length) - 1];
}

function summary(values) {
  return {
    count: values.length,
    medianMs: percentile(values, 0.5),
    p95Ms: percentile(values, 0.95),
    maxMs: values.length ? Math.max(...values) : null,
  };
}

fs.rmSync(root, { recursive: true, force: true });
fs.cpSync(PUBLIC_GRAPH, graph, { recursive: true, force: true });
fs.mkdirSync(artifacts, { recursive: true });
const fixturePage = path.join(graph, "pages", `${PAGE_TITLE}.md`);
if (sha256(fixturePage) !== EXPECTED_PAGE_SHA256) {
  throw new Error("GH #295 fixture page does not match the pinned public graph");
}
const fixtureBytes = fs.readFileSync(fixturePage, "utf8");
if (!fixtureBytes.includes(MARKER) || !fixtureBytes.includes("\t-\r\n")) {
  throw new Error("GH #295 fixture no longer contains the demonstrated empty child");
}

// The graph does not reliably expose filename-derived pages through Quick
// Switch in the historical candidates. Add a disposable journal link so the
// benchmark reaches the exact public page through ordinary Tine navigation.
const now = new Date();
const journalStem = `${now.getFullYear()}_${String(now.getMonth() + 1).padStart(2, "0")}_${String(now.getDate()).padStart(2, "0")}`;
fs.mkdirSync(path.join(graph, "journals"), { recursive: true });
fs.writeFileSync(path.join(graph, "journals", `${journalStem}.md`), `- [[${PAGE_TITLE}]]\r\n`);

const env = {
  ...process.env,
  TINE_GRAPH: graph,
  TINE_DEBUG: "1",
  TINE_DEBUG_LOG: debugLog,
  TINE_E2E_APPLICATION_STDOUT_LOG: path.join(artifacts, "application-stdout.log"),
  TINE_E2E_APPLICATION_STDERR_LOG: path.join(artifacts, "application-stderr.log"),
  E2E_WEBVIEW_USER_DATA_ROOT: path.join(root, "webview2-profile"),
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
const candidateSha = process.env.TINE_295_CANDIDATE_SHA || "unknown";
const receipt = {
  schemaVersion: 1,
  scenario: "windows-page-reference-latency",
  issue: 295,
  candidateSha,
  publicGraphZipSha256: "a4cc5eca0c08ac3e819dc490e3d48f545c207da742a670bf437a86a6d1b6aa24",
  fixturePage: `pages/${PAGE_TITLE}.md`,
  fixturePageSha256: EXPECTED_PAGE_SHA256,
  literalKeys: TYPED,
  keyPeriodMs: KEY_PERIOD_MS,
  driverPort: DRIVER_PORT,
  nativePort: NATIVE_PORT,
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
  await selectWebdriverWindowWithSelector(browser, 'button[title^="Search (Ctrl+K)"]');
  await browser.$(".ls-block").waitForExist({ timeout: 90_000 });
  const pageLink = await browser.$(`a*=${PAGE_TITLE}`);
  await pageLink.waitForClickable({ timeout: 30_000 });
  await pageLink.click();
  await browser.waitUntil(async () => (await browser.$("body").getText()).includes(MARKER), {
    timeout: 30_000,
    timeoutMsg: "the GH #295 demonstration page did not open",
  });

  // The navigation click can leave the pointer over a page-reference at the
  // same screen coordinates, opening a hover peek above the target row. Close
  // that real transient, then use WebDriver for the edit-entry click as well as
  // the measured keys; DOM Element.click() alone does not emit Tine's mousedown
  // edit gesture on historical candidates.
  await browser.$("h1.page-title").moveTo();
  await sleep(750);
  if (await browser.$(".peek-popup").isExisting()) {
    await browser.keys(["Escape"]);
    await browser.$(".peek-popup").waitForExist({ reverse: true, timeout: 5_000 });
  }
  const targetFound = await browser.execute((marker) => {
    const blocks = [...document.querySelectorAll(".ls-block")];
    const markerIndex = blocks.findIndex((block) => {
      const content = block.querySelector(":scope > .block-main .block-content");
      return content?.textContent?.includes(marker);
    });
    if (markerIndex < 0) return false;
    for (const block of blocks.slice(markerIndex + 1)) {
      const content = block.querySelector(":scope > .block-main .block-content");
      if (content instanceof HTMLElement && !content.textContent?.trim()) {
        content.dataset.e2eIssue295EditorTarget = "true";
        return true;
      }
    }
    return false;
  }, MARKER);
  if (!targetFound) throw new Error("the demonstrated empty child block was not found");
  const target = await browser.$('[data-e2e-issue295-editor-target="true"]');
  await target.scrollIntoView({ block: "center", inline: "center" });
  await target.click();
  const editor = await browser.$("textarea.block-editor");
  await editor.waitForExist({ timeout: 10_000 });
  await editor.click();
  await browser.execute(() => {
    const editor = document.querySelector("textarea.block-editor");
    if (!(editor instanceof HTMLTextAreaElement)) throw new Error("editor is absent");
    editor.value = "";
    editor.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "deleteContent" }));
    editor.focus();
    editor.setSelectionRange(0, 0);

    const state = {
      pending: [],
      keys: [],
      invokes: [],
      issue248: [],
      popupPaintAt: null,
      instrumentation: "pending",
      invokeDescriptor: null,
      invokeReplacementHeld: false,
    };
    window.__tineIssue295 = state;
    window.__tineIssue248Bench = {
      record(metric, valueMs) {
        const endedAt = performance.now();
        state.issue248.push({ metric, valueMs, startedAt: endedAt - valueMs, endedAt });
      },
    };

    document.addEventListener("keydown", (event) => {
      if (!(event.target instanceof HTMLTextAreaElement) || !event.target.classList.contains("block-editor")) return;
      const pending = state.pending.shift();
      const row = pending ?? { index: state.keys.length, key: event.key, dispatchAt: performance.now() };
      row.keydownAt = performance.now();
      row.observedKey = event.key;
      state.keys.push(row);
    }, true);
    document.addEventListener("input", (event) => {
      if (!(event.target instanceof HTMLTextAreaElement) || !event.target.classList.contains("block-editor")) return;
      const row = state.keys[state.keys.length - 1];
      if (!row || row.inputAt !== undefined) return;
      row.inputAt = performance.now();
      row.valueLength = event.target.value.length;
      requestAnimationFrame(() => requestAnimationFrame(() => {
        row.secondFrameAt = performance.now();
      }));
    }, true);
    const popupObserver = new MutationObserver(() => {
      if (state.popupPaintAt !== null || !document.querySelector(".autocomplete .ac-item")) return;
      requestAnimationFrame(() => requestAnimationFrame(() => {
        if (state.popupPaintAt === null && document.querySelector(".autocomplete .ac-item")) {
          state.popupPaintAt = performance.now();
        }
      }));
    });
    popupObserver.observe(document.documentElement, { subtree: true, childList: true });

    try {
      const internals = window.__TAURI_INTERNALS__;
      const originalInvoke = internals.invoke.bind(internals);
      const descriptor = Object.getOwnPropertyDescriptor(internals, "invoke");
      state.invokeDescriptor = descriptor ? {
        configurable: descriptor.configurable,
        enumerable: descriptor.enumerable,
        writable: descriptor.writable,
        hasGetter: typeof descriptor.get === "function",
        hasSetter: typeof descriptor.set === "function",
      } : null;
      const wrappedInvoke = (...args) => {
        const command = String(args[0]);
        if (command !== "quick_switch" && command !== "save_page") return originalInvoke(...args);
        const span = { command, startedAt: performance.now(), endedAt: null, outcome: "pending" };
        state.invokes.push(span);
        try {
          return Promise.resolve(originalInvoke(...args)).then(
            (value) => {
              span.endedAt = performance.now();
              span.outcome = "ok";
              return value;
            },
            (error) => {
              span.endedAt = performance.now();
              span.outcome = "error";
              throw error;
            },
          );
        } catch (error) {
          span.endedAt = performance.now();
          span.outcome = "throw";
          throw error;
        }
      };
      internals.invoke = wrappedInvoke;
      state.invokeReplacementHeld = internals.invoke === wrappedInvoke;
      state.instrumentation = state.invokeReplacementHeld
        ? "invoke-wrapped"
        : "invoke-replacement-not-retained";
    } catch (error) {
      state.instrumentation = `unavailable: ${String(error)}`;
    }
  });

  const hostDispatchRoundTripMs = [];
  for (const [index, key] of [...TYPED].entries()) {
    await browser.execute((entryIndex, entryKey) => {
      window.__tineIssue295.pending.push({
        index: entryIndex,
        key: entryKey,
        dispatchAt: performance.now(),
      });
    }, index, key);
    const started = performance.now();
    await browser.keys([key]);
    hostDispatchRoundTripMs.push(performance.now() - started);
    await browser.waitUntil(async () => browser.execute((entryIndex) => {
      const row = window.__tineIssue295.keys.find((candidate) => candidate.index === entryIndex);
      return Number.isFinite(row?.secondFrameAt);
    }, index), {
      timeout: 10_000,
      timeoutMsg: `literal key ${index} did not reach its second paint`,
    });
    await sleep(KEY_PERIOD_MS);
  }

  await browser.$(".autocomplete .ac-item").waitForExist({
    timeout: 30_000,
    timeoutMsg: "page-reference candidates did not paint",
  });
  await browser.waitUntil(async () => browser.execute(() => {
    const state = window.__tineIssue295;
    return state.invokes.some((span) => span.command === "quick_switch" && span.endedAt !== null);
  }), { timeout: 30_000, timeoutMsg: "native quick_switch timing did not complete" });
  await sleep(1500);

  const measurements = await browser.execute(() => window.__tineIssue295);
  if (measurements.instrumentation !== "invoke-wrapped") {
    throw new Error(`native command timing unavailable: ${measurements.instrumentation}`);
  }
  if (measurements.keys.length !== [...TYPED].length) {
    throw new Error(`literal key evidence is incomplete: ${measurements.keys.length}/${[...TYPED].length}`);
  }
  const dispatchToInput = measurements.keys.map((row) => row.inputAt - row.dispatchAt);
  const keydownToInput = measurements.keys.map((row) => row.inputAt - row.keydownAt);
  const dispatchToSecondPaint = measurements.keys.map((row) => row.secondFrameAt - row.dispatchAt);
  const keydownToSecondPaint = measurements.keys.map((row) => row.secondFrameAt - row.keydownAt);
  const quickSwitch = measurements.invokes
    .filter((span) => span.command === "quick_switch" && span.endedAt !== null)
    .map((span) => span.endedAt - span.startedAt);
  const saves = measurements.invokes
    .filter((span) => span.command === "save_page" && span.endedAt !== null)
    .map((span) => span.endedAt - span.startedAt);
  const finalInputAt = measurements.keys.at(-1).inputAt;
  const stalledKeysDuringQuickSwitch = measurements.keys.filter((row) =>
    measurements.invokes.some((span) => span.command === "quick_switch"
      && span.startedAt <= row.dispatchAt
      && (span.endedAt ?? Number.POSITIVE_INFINITY) >= row.secondFrameAt)
  ).map((row) => row.index);

  Object.assign(receipt, {
    result: "pass",
    instrumentation: measurements.instrumentation,
    timings: {
      hostDispatchRoundTrip: summary(hostDispatchRoundTripMs),
      dispatchToInput: summary(dispatchToInput),
      keydownToInput: summary(keydownToInput),
      dispatchToSecondPaint: summary(dispatchToSecondPaint),
      keydownToSecondPaint: summary(keydownToSecondPaint),
      debouncePlusPopupPaintMs: measurements.popupPaintAt - finalInputAt,
      quickSwitch: summary(quickSwitch),
      directSave: summary(saves),
    },
    counts: {
      keys: measurements.keys.length,
      quickSwitch: quickSwitch.length,
      directSave: saves.length,
    },
    stalledKeysDuringQuickSwitch,
    raw: measurements,
  });
  fs.writeFileSync(path.join(artifacts, "windows-page-reference-latency-receipt.json"), `${JSON.stringify(receipt, null, 2)}\n`);
  console.log(`PASS: GH #295 literal Windows key timing: ${JSON.stringify(receipt.timings)}`);
} catch (error) {
  let raw = null;
  try { raw = await browser?.execute(() => window.__tineIssue295 ?? null); } catch {}
  try { await browser?.saveScreenshot(path.join(artifacts, "failure.png")); } catch {}
  const failure = {
    ...receipt,
    result: "fail",
    error: String(error).split("\n").slice(0, 10).join(" | "),
    raw,
  };
  fs.writeFileSync(path.join(artifacts, "failure-capsule.json"), `${JSON.stringify(failure, null, 2)}\n`);
  throw error;
} finally {
  try { await browser?.deleteSession(); } catch {}
  spawnSync("taskkill", ["/PID", String(driver.pid), "/T", "/F"], { stdio: "ignore" });
  stopWebdriverApplication(webviewTarget);
  if (process.env.CI === "true") spawnSync("taskkill", ["/IM", path.basename(APP), "/T", "/F"], { stdio: "ignore" });
  fs.closeSync(driverLog);
}
