#!/usr/bin/env node

// Paired native benchmark for Tine's two storage modes. Each receipt creates
// one deterministic Markdown graph and one XDG profile, exercises Direct and
// Tine-managed storage in a rotated order, and excludes the uninteresting
// transition itself. The timings are user-visible journeys, not private sparse
// layout probes: cold launch to rendered graph, edit to durable Markdown, and
// steady textarea input-handler latency.

import { execFileSync, spawn } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import { remote } from "webdriverio";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function arg(name) {
  const index = process.argv.indexOf(name);
  if (index < 0 || !process.argv[index + 1]) throw new Error(`missing ${name}`);
  return process.argv[index + 1];
}

function positiveInt(name, fallback) {
  const value = process.env[name];
  if (value === undefined) return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed <= 0) throw new Error(`${name} must be a positive integer`);
  return parsed;
}

const APP = path.resolve(arg("--app"));
const OUTPUT = path.resolve(arg("--output"));
const MODE_ORDER = arg("--mode-order").split(",");
const TAURI_DRIVER = process.env.TAURI_DRIVER || (process.env.CARGO_HOME ? path.join(process.env.CARGO_HOME, "bin", "tauri-driver") : "tauri-driver");
const WEBKIT_DRIVER = process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver";
const XDOTOOL = process.env.E2E_XDOTOOL || "xdotool";
const WINDOW_MANAGER = process.env.TINE_BENCH_WINDOW_MANAGER || "openbox";
const PULSES = positiveInt("TINE_STORAGE_MODE_PULSES", 60);
const PERIOD_MS = positiveInt("TINE_STORAGE_MODE_PERIOD_MS", 35);
const PAGES = positiveInt("TINE_STORAGE_MODE_PAGES", 120);
const BLOCKS = positiveInt("TINE_STORAGE_MODE_BLOCKS", 80);
const TARGET_PAGE = "Storage Bench Target";
// Optional real-corpus mode: point at an existing graph (e.g.
// ~/research/logseq-anonymized) instead of generating a synthetic one. The graph
// is COPIED into the run's scratch root, never edited in place, because the
// benchmark mutates its target page. A synthetic fixture cannot answer "is
// managed slower than direct on my graph" — see AGENTS.md §4 corpus discipline.
const SEED_GRAPH = process.env.TINE_STORAGE_MODE_SEED_GRAPH
  ? path.resolve(process.env.TINE_STORAGE_MODE_SEED_GRAPH)
  : null;
if (SEED_GRAPH && !fs.existsSync(SEED_GRAPH)) {
  throw new Error(`TINE_STORAGE_MODE_SEED_GRAPH does not exist: ${SEED_GRAPH}`);
}

if (!fs.existsSync(APP)) throw new Error(`native app binary is missing: ${APP}`);
if (!process.env.DISPLAY) throw new Error("HARNESS UNAVAILABLE: run under an X display (for example xvfb-run -a dbus-run-session)");
if (MODE_ORDER.length !== 2 || new Set(MODE_ORDER).size !== 2 || !MODE_ORDER.every((mode) => mode === "direct" || mode === "managed")) {
  throw new Error("--mode-order must contain direct and managed exactly once");
}

function gitRevision() {
  const result = execFileSync("git", ["rev-parse", "HEAD"], { cwd: ROOT, encoding: "utf8" });
  return result.trim();
}

function xdo(env, ...args) {
  return execFileSync(XDOTOOL, args, { env, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
}

function nativeDialogIds(env) {
  try {
    return xdo(env, "search", "--onlyvisible", "--name", "^Tine$").split(/\s+/).filter(Boolean);
  } catch {
    return [];
  }
}

function hasWindowManager(env) {
  try {
    return /_NET_SUPPORTING_WM_CHECK.*window id/i.test(
      execFileSync("xprop", ["-root", "_NET_SUPPORTING_WM_CHECK"], { env, encoding: "utf8" }),
    );
  } catch {
    return false;
  }
}

async function startWindowManager(env) {
  if (hasWindowManager(env)) return undefined;
  const child = spawn(WINDOW_MANAGER, ["--sm-disable"], { env, stdio: "ignore", detached: true });
  await sleep(250);
  return child;
}

function stopProcessGroup(child) {
  if (!child?.pid) return;
  try { process.kill(-child.pid, "SIGKILL"); } catch {}
}

async function freePort() {
  return await new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const port = server.address().port;
      server.close(() => resolve(port));
    });
  });
}

function journalStem() {
  const now = new Date();
  return `${now.getFullYear()}_${String(now.getMonth() + 1).padStart(2, "0")}_${String(now.getDate()).padStart(2, "0")}`;
}

function countTextFiles(dir) {
  let n = 0;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true, recursive: true })) {
    if (entry.isFile() && /\.(md|markdown|org)$/i.test(entry.name)) n += 1;
  }
  return n;
}

function seedGraph(root) {
  const graph = path.join(root, "graph");
  const pages = path.join(graph, "pages");
  const journals = path.join(graph, "journals");
  const target = path.join(pages, `${TARGET_PAGE}.md`);
  if (SEED_GRAPH) {
    // Copy the real corpus in; the source is read-only as far as this run is
    // concerned. The target page and today's journal are then added on top so
    // the save and keystroke phases have the same edit surface as the synthetic
    // fixture, making the two fixtures' numbers comparable per operation.
    fs.cpSync(SEED_GRAPH, graph, { recursive: true });
    fs.mkdirSync(pages, { recursive: true });
    fs.mkdirSync(journals, { recursive: true });
    fs.mkdirSync(path.join(graph, "logseq"), { recursive: true });
    const config = path.join(graph, "logseq", "config.edn");
    if (!fs.existsSync(config)) fs.writeFileSync(config, '{:preferred-format "Markdown"}\n');
    fs.writeFileSync(
      target,
      [
        "- storage mode active block",
        ...Array.from({ length: BLOCKS - 1 }, (_, index) => `- bench filler ${String(index + 1).padStart(3, "0")} stable content`),
        "",
      ].join("\n"),
    );
    fs.writeFileSync(path.join(journals, `${journalStem()}.md`), `- [[${TARGET_PAGE}]]\n`);
    return { graph, target, fileCount: countTextFiles(graph) };
  }
  fs.mkdirSync(pages, { recursive: true });
  fs.mkdirSync(journals, { recursive: true });
  fs.mkdirSync(path.join(graph, "logseq"), { recursive: true });
  fs.writeFileSync(path.join(graph, "logseq", "config.edn"), '{:preferred-format "Markdown"}\n');
  fs.writeFileSync(
    target,
    [
      "- storage mode active block",
      ...Array.from({ length: BLOCKS - 1 }, (_, index) => `- bench filler ${String(index + 1).padStart(3, "0")} stable content`),
      "",
    ].join("\n"),
  );
  for (let index = 1; index < PAGES; index += 1) {
    fs.writeFileSync(path.join(pages, `Storage Bench Unrelated ${String(index).padStart(3, "0")}.md`), `- unrelated bench page ${index}\n`);
  }
  fs.writeFileSync(path.join(journals, `${journalStem()}.md`), `- [[${TARGET_PAGE}]]\n`);
  return { graph, target, fileCount: countTextFiles(graph) };
}

async function waitFor(predicate, label, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    try {
      const value = await predicate();
      if (value) return value;
    } catch (error) {
      lastError = error;
    }
    await sleep(60);
  }
  throw new Error(`timed out waiting for ${label}${lastError ? `: ${String(lastError)}` : ""}`);
}

function runEnv(graph, xdg) {
  return {
    ...process.env,
    TINE_GRAPH: graph,
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

async function launch(graph, xdg, runDir) {
  const env = runEnv(graph, xdg);
  const driverPort = await freePort();
  const nativePort = await freePort();
  const log = fs.openSync(path.join(runDir, `tauri-driver-${driverPort}.log`), "w");
  const driver = spawn(
    TAURI_DRIVER,
    ["--port", String(driverPort), "--native-port", String(nativePort), "--native-driver", WEBKIT_DRIVER],
    { env, stdio: ["ignore", log, log], detached: true },
  );
  await sleep(1_700);
  const started = performance.now();
  let browser;
  try {
    browser = await remote({
      hostname: "127.0.0.1",
      port: driverPort,
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
    await browser.$(".ls-block, .page-title, .journal-day").waitForExist({ timeout: 45_000 });
    return { browser, driver, log, env, coldOpenMs: performance.now() - started };
  } catch (error) {
    try { await browser?.deleteSession(); } catch {}
    stopProcessGroup(driver);
    fs.closeSync(log);
    throw error;
  }
}

async function closeSession(session) {
  try { await session?.browser?.deleteSession(); } catch {}
  stopProcessGroup(session?.driver);
  try { fs.closeSync(session?.log); } catch {}
  await sleep(350);
}

async function openTarget(browser) {
  // The sidebar inventory is truncated on a real-scale graph -- at 1,045 files it
  // renders a few hundred rows and then "+643 more - search to open...", so the
  // target is simply not in the DOM to be clicked. Every path here therefore
  // falls back to the `[[target]]` reference seeded into today's journal, which
  // is rendered regardless of how many pages the graph has.
  const clickInventoryTarget = () => browser.execute((targetPage) => {
    const rows = [
      ...document.querySelectorAll(".nav-page"),
      ...document.querySelectorAll(".page-ref"),
    ];
    const row = rows.find((node) => (node.textContent ?? "").trim() === targetPage)
      ?? rows.find((node) => (node.textContent ?? "").includes(targetPage));
    if (!row) return false;
    for (const type of ["mousedown", "mouseup", "click"]) {
      row.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, button: 0 }));
    }
    return true;
  }, TARGET_PAGE);
  if (!await clickInventoryTarget()) {
    let allPages;
    try {
      allPages = await waitFor(async () => {
        for (const header of await browser.$$(".nav-section-header")) {
          if ((await header.getText()).includes("ALL PAGES")) return header;
        }
        return undefined;
      }, "ALL PAGES inventory", 15_000);
    } catch (error) {
      const diagnostic = await browser.execute(() => ({
        title: document.querySelector("h1.page-title")?.textContent?.trim() ?? null,
        headers: [...document.querySelectorAll(".nav-section-header")].map((node) => node.textContent?.trim()),
        pages: [...document.querySelectorAll(".nav-page")].slice(0, 10).map((node) => node.textContent?.trim()),
        root: document.querySelector("#root")?.textContent?.trim().slice(0, 500) ?? "",
      }));
      throw new Error(`${error.message}; inventory diagnostic=${JSON.stringify(diagnostic)}`);
    }
    await allPages.click();
    try {
      await waitFor(clickInventoryTarget, "benchmark target in the page inventory", 20_000);
    } catch (error) {
      const diagnostic = await browser.execute(() => ({
        headers: [...document.querySelectorAll(".nav-section-header")].map((node) => node.textContent?.trim()),
        pages: [...document.querySelectorAll(".nav-page")].map((node) => node.textContent?.trim()),
      }));
      throw new Error(`${error.message}; page-list diagnostic=${JSON.stringify(diagnostic)}`);
    }
  }
  const title = await browser.$("h1.page-title");
  await title.waitForExist({ timeout: 20_000 });
  await browser.waitUntil(async () => (await title.getText()).trim() === TARGET_PAGE, {
    timeout: 20_000,
    timeoutMsg: "benchmark target page did not become visible",
  });
}

async function editorFor(browser) {
  let target = await browser.$(".page-blocks .ls-block .block-content-wrapper, .page-blocks .ls-block .block-content");
  if (await target.isExisting()) {
    await target.click();
  } else {
    target = await browser.$(".page-trailing-block-target");
    await target.waitForExist({ timeout: 10_000 });
    await target.click();
  }
  const editor = await browser.$(".page-blocks textarea.block-editor, textarea.block-editor");
  await editor.waitForExist({ timeout: 12_000 });
  return editor;
}

async function saveEditedPage(browser, target, marker) {
  const editor = await editorFor(browser);
  const started = performance.now();
  await editor.addValue(` ${marker}`);
  await (await browser.$("h1.page-title")).click();
  await waitFor(() => fs.readFileSync(target, "utf8").includes(marker), "edited page to persist");
  return performance.now() - started;
}

function p95(values) {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.ceil(sorted.length * 0.95) - 1)];
}

async function steadyKeystrokes(browser, target) {
  const editor = await editorFor(browser);
  const before = fs.readFileSync(target);
  const pulse = (operation) => browser.execute(async (kind, pulses, periodMs) => {
    const editor = document.querySelector("textarea.block-editor");
    if (!(editor instanceof HTMLTextAreaElement)) throw new Error("benchmark target editor is absent");
    const original = editor.value;
    editor.focus();
    editor.setSelectionRange(editor.value.length, editor.value.length);
    const inputHandlerMs = [];
    const scheduleLagMs = [];
    let due = performance.now() + periodMs;
    for (let index = 0; index < pulses; index += 1) {
      const scheduled = due;
      const wait = Math.max(0, scheduled - performance.now());
      if (wait) await new Promise((resolve) => setTimeout(resolve, wait));
      due += periodMs;
      const started = performance.now();
      scheduleLagMs.push(Math.max(0, started - scheduled));
      if (kind === "insert") {
        editor.value += "1";
        editor.setSelectionRange(editor.value.length, editor.value.length);
      } else {
        if (!editor.value.endsWith("1")) throw new Error("keystroke deletion did not follow an insertion");
        editor.value = editor.value.slice(0, -1);
        editor.setSelectionRange(editor.value.length, editor.value.length);
      }
      editor.dispatchEvent(new InputEvent("input", {
        bubbles: true,
        inputType: kind === "insert" ? "insertText" : "deleteContentBackward",
        data: kind === "insert" ? "1" : null,
      }));
      inputHandlerMs.push(performance.now() - started);
    }
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    return { original, final: editor.value, inputHandlerMs, scheduleLagMs };
  }, operation, PULSES, PERIOD_MS);
  const inserted = await pulse("insert");
  if (inserted.final !== `${inserted.original}${"1".repeat(PULSES)}`) throw new Error("steady keystroke insert did not update the editor");
  const deleted = await pulse("delete");
  if (deleted.final !== inserted.original) throw new Error("steady keystroke delete did not restore the editor");
  await (await browser.$("h1.page-title")).click();
  await waitFor(() => fs.readFileSync(target).equals(before), "exact edited-page bytes after keystroke restore");
  return {
    inputHandlerP95Ms: Math.max(0.001, p95([...inserted.inputHandlerMs, ...deleted.inputHandlerMs])),
    scheduleLagP95Ms: Math.max(0.001, p95([...inserted.scheduleLagMs, ...deleted.scheduleLagMs])),
  };
}

async function exactTextButton(browser, text) {
  return await waitFor(async () => {
    for (const button of await browser.$$("button")) {
      if ((await button.getText()).includes(text)) return button;
    }
    return undefined;
  }, `visible ${text} button`, 15_000);
}

async function openStorageSettings(browser) {
  const trigger = await browser.$('button[title^="Settings"]');
  await trigger.waitForExist({ timeout: 12_000 });
  await trigger.click();
  await browser.$(".settings-modal").waitForExist({ timeout: 12_000 });
  const tab = await waitFor(async () => {
    for (const item of await browser.$$(".settings-nav-item")) {
      if ((await item.getText()).trim() === "Backups & recovery") return item;
    }
    return undefined;
  }, "Backups & recovery settings tab", 12_000);
  await tab.click();
  await browser.waitUntil(async () => (await browser.$("body").getText()).includes("Storage & sync"), { timeout: 12_000 });
  const experimental = await browser.$(".settings-experimental .settings-advanced-toggle");
  await experimental.waitForExist({ timeout: 12_000 });
  if ((await experimental.getAttribute("aria-expanded")) !== "true") await experimental.click();
}

async function closeSettings(browser) {
  for (let attempt = 0; attempt < 3 && await browser.$(".settings-modal").isExisting(); attempt += 1) {
    await browser.keys(["Escape"]);
    await sleep(100);
  }
  await browser.$(".settings-modal").waitForExist({ reverse: true, timeout: 12_000 });
}

async function acceptNativeConfirmation(browser, env, action) {
  const before = new Set(nativeDialogIds(env));
  await (await exactTextButton(browser, action)).click();
  const dialog = await waitFor(() => nativeDialogIds(env).find((id) => !before.has(id)), `${action} native confirmation`, 15_000);
  xdo(env, "windowactivate", "--sync", dialog);
  xdo(env, "key", "--clearmodifiers", "alt+y");
  await waitFor(() => !nativeDialogIds(env).includes(dialog), `${action} confirmation to close`, 12_000);
}

async function transition(session, targetMode) {
  await openStorageSettings(session.browser);
  if (targetMode === "managed") {
    await acceptNativeConfirmation(session.browser, session.env, "Enable Tine-managed storage...");
    await session.browser.waitUntil(async () => (await session.browser.$("body").getText()).includes("Tine-managed storage active"), { timeout: 45_000 });
  } else {
    await acceptNativeConfirmation(session.browser, session.env, "Return to Direct files");
    await session.browser.waitUntil(async () => (await session.browser.$("body").getText()).includes("Enable Tine-managed storage..."), { timeout: 45_000 });
  }
  await closeSettings(session.browser);
}

async function measureMode(mode, session, target, marker) {
  await openTarget(session.browser);
  const saveMs = await saveEditedPage(session.browser, target, marker);
  await sleep(750); // the same quiet interval precedes each steady-state input phase
  const keystrokes = await steadyKeystrokes(session.browser, target);
  return {
    coldOpenMs: Math.max(0.001, session.coldOpenMs),
    saveMs: Math.max(0.001, saveMs),
    keystrokeP95Ms: keystrokes.inputHandlerP95Ms,
    keystrokeScheduleLagP95Ms: keystrokes.scheduleLagP95Ms,
    marker,
  };
}

function classifyFailure(error) {
  const message = String(error);
  if (/HARNESS UNAVAILABLE|tauri-driver|WebKit|xdotool|openbox|DISPLAY|native confirmation/i.test(message)) return "infrastructure";
  if (/benchmark target|ALL PAGES inventory|benchmark journal/i.test(message)) return "harness";
  if (/did not|timed out|persist|editor|visible|storage/i.test(message)) return "product";
  return "ambiguous";
}

const root = fs.mkdtempSync(path.join(os.tmpdir(), "tine-storage-mode-bench-"));
const runDir = path.join(root, "artifacts");
const xdg = path.join(root, "xdg");
const { graph, target, fileCount } = seedGraph(root);
for (const directory of ["data", "config", "cache"]) fs.mkdirSync(path.join(xdg, directory), { recursive: true });
fs.mkdirSync(runDir, { recursive: true });
const receipt = {
  schemaVersion: 2,
  kind: "storage-mode-run",
  testedCommit: gitRevision(),
  app: APP,
  modeOrder: MODE_ORDER,
  // Provenance is load-bearing: a storage-mode number is meaningless without the
  // graph it came from, and the synthetic and real corpora give very different
  // answers. Never quote a figure from this receipt without this block.
  fixture: SEED_GRAPH
    ? {
        name: "real-corpus storage-mode fixture",
        source: SEED_GRAPH,
        fileCount,
        graph: `${fileCount} text files copied from ${path.basename(SEED_GRAPH)}; ${BLOCKS}-block edited page added`,
        target: `pages/${TARGET_PAGE}.md`,
      }
    : {
        name: "synthetic storage-mode fixture",
        fileCount,
        graph: `${PAGES} Markdown pages; ${BLOCKS}-block edited page (${fileCount} text files)`,
        target: `pages/${TARGET_PAGE}.md`,
      },
  machine: `${os.hostname()} (${os.platform()}-${os.release()}-${os.arch()})`,
  pulses: PULSES,
  periodMs: PERIOD_MS,
  modes: {},
};
let windowManager;
let activeSession;
try {
  const env = runEnv(graph, xdg);
  windowManager = await startWindowManager(env);
  if (MODE_ORDER[0] === "direct") {
    activeSession = await launch(graph, xdg, runDir);
    receipt.modes.direct = await measureMode("direct", activeSession, target, "direct storage benchmark marker");
    await transition(activeSession, "managed");
    await closeSession(activeSession);
    activeSession = await launch(graph, xdg, runDir);
    receipt.modes.managed = await measureMode("managed", activeSession, target, "managed storage benchmark marker");
  } else {
    // Activation is setup, not a measured operation. It makes the first mode
    // managed while retaining the exact graph and XDG profile for both cells.
    activeSession = await launch(graph, xdg, runDir);
    await transition(activeSession, "managed");
    await closeSession(activeSession);
    activeSession = await launch(graph, xdg, runDir);
    receipt.modes.managed = await measureMode("managed", activeSession, target, "managed storage benchmark marker");
    await transition(activeSession, "direct");
    await closeSession(activeSession);
    activeSession = await launch(graph, xdg, runDir);
    receipt.modes.direct = await measureMode("direct", activeSession, target, "direct storage benchmark marker");
  }
  for (const mode of ["direct", "managed"]) {
    const { marker, ...metrics } = receipt.modes[mode];
    receipt.modes[mode] = { metrics, marker };
  }
  fs.mkdirSync(path.dirname(OUTPUT), { recursive: true });
  fs.writeFileSync(OUTPUT, `${JSON.stringify(receipt, null, 2)}\n`);
  console.log(JSON.stringify({ modeOrder: receipt.modeOrder, modes: receipt.modes, output: OUTPUT }, null, 2));
} catch (error) {
  try { await activeSession?.browser?.saveScreenshot(path.join(path.dirname(OUTPUT), "storage-mode-failure.png")); } catch {}
  const capsule = {
    testedCommit: receipt.testedCommit,
    journey: "storage-mode-benchmark",
    expected: "direct and managed mode both complete cold open, durable save, and steady editor input on the paired graph",
    observed: String(error).split("\n").slice(0, 4).join(" | "),
    classification: classifyFailure(error),
  };
  fs.mkdirSync(path.dirname(OUTPUT), { recursive: true });
  fs.writeFileSync(path.join(path.dirname(OUTPUT), "storage-mode-failure-capsule.json"), `${JSON.stringify(capsule, null, 2)}\n`);
  console.error(`STORAGE MODE FAILURE CAPSULE ${JSON.stringify(capsule)}`);
  process.exitCode = 1;
} finally {
  await closeSession(activeSession);
  stopProcessGroup(windowManager);
  // TINE_STORAGE_MODE_KEEP retains the run root so the managed storage tree can
  // be measured after a *settled* activation. Without it every amplification
  // figure has to be taken mid-run off a live fixture, which is exactly the
  // caveat that made the first 13.8x measurement provisional.
  if (process.env.TINE_STORAGE_MODE_KEEP) {
    console.log(`RUN ROOT RETAINED: ${root}`);
  } else {
    fs.rmSync(root, { recursive: true, force: true });
  }
}
