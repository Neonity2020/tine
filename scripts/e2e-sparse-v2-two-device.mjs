#!/usr/bin/env node

// Linux real-app proof of the desktop-authority -> fresh-device managed-sync
// lifecycle. Two launches use separate XDG app-data roots, just as desktop and
// Android do; only the graph-local provider tree crosses the device boundary.
import { execFileSync, spawn, spawnSync } from "node:child_process";
import crypto from "node:crypto";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { tauriCapabilities, webdriverServerArgs } from "./e2e-capabilities.mjs";

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
const SOURCE = process.env.TINE_MANAGED_SYNC_GRAPH
  ? fs.realpathSync(path.resolve(process.env.TINE_MANAGED_SYNC_GRAPH))
  : null;
// Direct Files and Tine-managed storage are PEERS: a device may live in either
// one permanently. The joining device therefore has two equally first-class
// starting states, and this journey proves the same join contract from both.
// Neither ordering is a smoke test of the other; the only thing they do not
// share is the post-join lifecycle, because the product's own answer differs.
const JOIN_ORDERINGS = ["join-from-df", "join-from-managed"];
const JOIN_ORDERING = process.env.TINE_E2E_JOIN_ORDERING || "join-from-df";
if (!JOIN_ORDERINGS.includes(JOIN_ORDERING)) {
  throw new Error(
    `unknown TINE_E2E_JOIN_ORDERING ${JSON.stringify(JOIN_ORDERING)}; choose ${JOIN_ORDERINGS.join(", ")}`,
  );
}
const JOIN_FROM_MANAGED = JOIN_ORDERING === "join-from-managed";
let PAGE = "Two Device Managed Sync";
const MARKER = SOURCE
  ? `managed real sync ${Date.now()}`
  : "cross device source bytes";
let selected;
let selectedRaw;

if (!fs.existsSync(APP)) throw new Error(`HARNESS UNAVAILABLE: exact TINE_APP candidate is missing at ${APP}`);
fs.mkdirSync(ARTIFACTS, { recursive: true });
for (const graph of [GRAPH_A, GRAPH_B]) {
  if (SOURCE) {
    fs.cpSync(SOURCE, graph, { recursive: true, dereference: false });
    if (fs.realpathSync(graph) === SOURCE) throw new Error("refusing to run against the source corpus");
    fs.rmSync(path.join(graph, ".tine-sync"), { recursive: true, force: true });
  } else {
    fs.mkdirSync(path.join(graph, "pages"), { recursive: true });
    fs.mkdirSync(path.join(graph, "journals"), { recursive: true });
    fs.mkdirSync(path.join(graph, "logseq"), { recursive: true });
    fs.writeFileSync(path.join(graph, "pages", `${PAGE}.md`), `- ${MARKER}\n`);
    fs.writeFileSync(path.join(graph, "logseq", "config.edn"), '{:preferred-format "Markdown"}\n');
  }
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
  joinOrdering: JOIN_ORDERING,
  testedCommit: gitRevision(),
  app: APP,
  sourceCorpus: SOURCE,
  graphCopies: { deviceA: GRAPH_A, deviceB: GRAPH_B },
  xdg: { deviceA: XDG_A, deviceB: XDG_B },
  mode: SOURCE ? "real-corpus-release" : "synthetic",
  milestones: {},
};

function exactTreeDigest(root) {
  const hash = crypto.createHash("sha256");
  const visit = (dir, prefix = "") => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })
      .sort((left, right) => left.name.localeCompare(right.name))) {
      const relative = path.posix.join(prefix, entry.name);
      hash.update(`${entry.isDirectory() ? "d" : "f"}:${relative}\0`);
      const absolute = path.join(dir, entry.name);
      if (entry.isDirectory()) visit(absolute, relative);
      else if (entry.isFile()) hash.update(fs.readFileSync(absolute));
      else throw new Error(`provider evidence contained unsupported entry ${relative}`);
    }
  };
  visit(root);
  return hash.digest("hex");
}

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
  try {
    await browser.waitUntil(async () => (await browser.$("body").getText()).includes(text), {
      timeout,
      timeoutMsg: `${label} was not visible: ${JSON.stringify(text)}`,
    });
  } catch (error) {
    const body = await browser.$("body").getText().catch(() => "<body unavailable>");
    throw new Error(`${error instanceof Error ? error.message : String(error)}; final body=${JSON.stringify(body)}`);
  }
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

function flattenBlocks(blocks) {
  return blocks.flatMap((block) => [block, ...flattenBlocks(block.children ?? [])]);
}

function fileContains(file, text) {
  try { return fs.readFileSync(file, "utf8").includes(text); }
  catch (error) {
    // Managed projection may be between its no-clobber retirement and exact
    // replacement when this observation races it. Absence is not success and
    // not a test failure; keep waiting for the durable target. Other I/O errors
    // remain diagnostic failures.
    if (error?.code === "ENOENT") return false;
    throw error;
  }
}

async function selectExistingEditablePage() {
  const pages = await managedCall("list_pages");
  const nameCounts = new Map();
  for (const entry of pages) nameCounts.set(entry.name, (nameCounts.get(entry.name) || 0) + 1);
  for (const entry of pages) {
    if (entry.kind !== "page" || nameCounts.get(entry.name) !== 1) continue;
    const file = path.join(GRAPH_A, entry.path);
    if (!fs.existsSync(file) || !/\.(md|org)$/i.test(file)) continue;
    let page;
    try { page = await getPage(entry.name); } catch { continue; }
    const blocks = flattenBlocks(page.blocks ?? []);
    const rawCounts = new Map();
    for (const block of blocks) {
      if (typeof block.raw === "string" && block.raw.trim()) {
        rawCounts.set(block.raw, (rawCounts.get(block.raw) || 0) + 1);
      }
    }
    const block = blocks.find((candidate) =>
      typeof candidate.id === "string"
      && candidate.id.length > 0
      && typeof candidate.raw === "string"
      && candidate.raw.trim()
      && rawCounts.get(candidate.raw) === 1
    );
    if (!block) continue;
    PAGE = entry.name;
    selectedRaw = block.raw;
    selected = { entry, file, blockId: block.id };
    receipt.selectedPage = { name: entry.name, kind: entry.kind, path: entry.path };
    return;
  }
  throw new Error("real graph exposed no uniquely named page with an unambiguous editable block");
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

async function openPageThroughSwitcher(name = PAGE) {
  await browser.keys(["Control", "k"]);
  const input = await browser.$(".switcher-input");
  await input.waitForExist({ timeout: 15_000 });
  await input.setValue(name);
  await browser.waitUntil(() => browser.execute((expected) => {
    const active = document.querySelector(".switcher-row.active");
    return active?.querySelector(".switcher-kind")?.textContent?.trim() === "page"
      && active.querySelector(".switcher-name")?.textContent?.trim().normalize("NFC") === expected.normalize("NFC");
  }, name), { timeout: 30_000, timeoutMsg: `switcher did not select ${JSON.stringify(name)}` });
  await browser.keys("Enter");
  const title = await browser.$("h1.page-title");
  await title.waitForExist({ timeout: 30_000 });
  await browser.waitUntil(async () => (await title.getText()).trim().normalize("NFC") === name.normalize("NFC"), {
    timeout: 30_000,
    timeoutMsg: `visible UI did not open ${JSON.stringify(name)}`,
  });
}

async function closeSettingsIfOpen() {
  const modal = await browser.$(".settings-modal");
  if (!(await modal.isExisting())) return;
  const close = await browser.$(".settings-pane-head .icon-btn:not(.settings-maximize)");
  await close.waitForClickable({ timeout: 15_000 });
  await close.click();
  await modal.waitForExist({ reverse: true, timeout: 15_000 });
}

async function refreshSelectedBlock(graph) {
  await leaseCurrentGraph(graph);
  const page = await getPage();
  const matches = flattenBlocks(page.blocks ?? []).filter((block) => block.raw === selectedRaw);
  if (matches.length !== 1) {
    throw new Error(`current managed page did not preserve one exact target block: ${JSON.stringify({
      page: PAGE,
      matches: matches.length,
    })}`);
  }
  selected.blockId = matches[0].id;
  return page;
}

async function appendThroughVisibleEditor(graph, suffix, label) {
  await closeSettingsIfOpen();
  await refreshSelectedBlock(graph);
  await openPageThroughSwitcher();
  const target = await browser.$(
    `[data-block-id="${selected.blockId}"] > .block-main > .block-content-wrapper`,
  );
  await target.waitForDisplayed({ timeout: 30_000 });
  await target.click();
  const editor = await browser.$(`[data-block-id="${selected.blockId}"] textarea.block-editor`);
  await editor.waitForDisplayed({ timeout: 15_000 });
  await editor.click();
  await browser.keys(["Control", "End"]);
  await editor.addValue(` ${suffix}`);
  const editedRaw = await editor.getValue();
  if (!editedRaw.startsWith(selectedRaw) || !editedRaw.includes(suffix)) {
    throw new Error(`${label} did not preserve the existing block while typing`);
  }
  await (await browser.$("h1.page-title")).click();
  await waitFor(() => fileContains(selected.file.replace(GRAPH_A, graph), suffix), 60_000,
    `${label} did not reach the Markdown projection`);
  const page = await getPage();
  if (!flattenBlocks(page.blocks ?? []).some((block) => block.raw === editedRaw)) {
    throw new Error(`${label} reached Markdown but not the application page`);
  }
  selectedRaw = editedRaw;
  receipt.milestones[label] = { visibleEditor: true, applicationPage: true, markdownProjection: true };
}

async function assertVisiblePageContains(text, label, timeout = 60_000) {
  await closeSettingsIfOpen();
  await openPageThroughSwitcher();
  await browser.waitUntil(async () => (await browser.$(".page-blocks").getText()).includes(text), {
    timeout,
    interval: 250,
    timeoutMsg: `${label} was not visible in the real page UI`,
  });
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
    // The production watcher owns managed progress and publishes the matching
    // sparse-v2-changed event to the visible app. Calling the diagnostic tick
    // command here can consume a provider batch before the watcher sees it,
    // leaving the native model current but the UI stale. Final acceptance must
    // therefore observe status only, never drive the actor directly.
    latest = await managedCall("sparse_v2_status");
    const runtime = latest.runtime;
    // provider_pending is a broad protocol inventory, not a quiescence bit: it
    // also counts durable provider intents which legitimately remain after the
    // corresponding manifest/head/objects have been published. The external
    // delivery + peer-visible-content assertions below prove that publication.
    // Local save settlement is the actor-owned journal plus watcher drain.
    const idle = runtime?.managed_local_pending === 0
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
    latest = await getPage();
    if (latest.blocks.some((block) => block.raw?.includes(text))) return latest;
    await sleep(100);
  }
  let status;
  try { status = await managedCall("sparse_v2_status"); } catch (error) { status = String(error); }
  throw new Error(`${label} did not arrive: ${JSON.stringify({ page: latest, status })}`);
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

async function toastMessages() {
  return browser.execute(() =>
    [...document.querySelectorAll(".toast-msg")].map((element) => (element.textContent ?? "").trim()));
}

async function dismissToasts() {
  await browser.waitUntil(async () => {
    await browser.execute(() => {
      for (const close of [...document.querySelectorAll(".toast-close")]) close.click();
    });
    return (await toastMessages()).length === 0;
  }, { timeout: 15_000, interval: 200, timeoutMsg: "visible notifications could not be dismissed" });
}

// `Join an existing synced graph...` (Direct Files) and `Join this synced
// graph...` (already-managed) are the same user action in two storage states,
// and this journey must not encode a preference between them. Select the join
// affordance by role rather than by one label; `Joining...` is the disabled
// in-flight label and is deliberately not matched.
const JOIN_BUTTON_PATTERN = "^Join\\s";

async function settingsButtonMatching(patternSource) {
  const index = await browser.execute((source) =>
    [...document.querySelectorAll(".settings-modal button")]
      .findIndex((element) => new RegExp(source).test((element.textContent ?? "").trim())), patternSource);
  return index >= 0 ? (await browser.$$(".settings-modal button"))[index] : undefined;
}

async function clickSettingsButtonMatchingAndConfirm(patternSource, label) {
  await waitFor(() => settingsButtonMatching(patternSource), 15_000,
    `${label}: no settings action matching ${patternSource} was visible`);
  const before = new Set(windowIds(".*"));
  const clicked = await browser.execute((source) => {
    const button = [...document.querySelectorAll(".settings-modal button")]
      .find((candidate) => new RegExp(source).test((candidate.textContent ?? "").trim()));
    if (!(button instanceof HTMLButtonElement)) return false;
    button.click();
    return true;
  }, patternSource);
  if (!clicked) throw new Error(`${label}: the join action disappeared before it could be clicked`);
  await acceptNativeConfirmation(label, before);
  return before;
}

// Vocabulary reserved for a genuinely hostile provider entry (a symlink, a file
// where a directory is required, a traversal attempt). A tree a sync tool has
// not finished delivering must never be reported with any of it.
const HOSTILE_PROVIDER_NOTICE = /unsafe provider|UnsafeProviderEntry|RecoveryBlocked|unsafe filesystem kind/i;
// What "not ready yet" is allowed to sound like. Deliberately broad: the exact
// wording is an acceptable variation, the meaning is not.
const NOT_READY_NOTICE = /not yet|still arriving|incomplete|has not finished|hasn.t finished|no sync data/i;
const JOIN_SUCCESS_NOTICE = "This device joined the synced graph.";
// A repeated identical notice is the reported symptom, so the quiet window has
// to outlast the retry cadence rather than sample once: `RETRY_BACKOFF`
// (src-tauri/src/watcher.rs) saturates at 8 s, so a window longer than that
// guarantees at least one further attempt would have spoken up.
const NOTICE_QUIET_MS = Number(process.env.E2E_JOIN_NOTICE_QUIET_MS || 9000);

function assertNoRepeatedNotice(messages, label) {
  const counts = new Map();
  for (const message of messages) counts.set(message, (counts.get(message) ?? 0) + 1);
  const repeated = [...counts].filter(([, count]) => count > 1);
  if (repeated.length) {
    throw new Error(`${label} repeated the same notice: ${JSON.stringify(repeated)}`);
  }
}

async function joinNoticeAfter(label) {
  const notice = await waitFor(async () =>
    (await toastMessages()).find((message) => /join/i.test(message)), 90_000,
    `${label}: the join action reported nothing to the user`);
  // The reported symptom is a condition that says its piece over and over. Hold
  // still afterwards and require the app to stay quiet about it.
  await sleep(NOTICE_QUIET_MS);
  const settled = await toastMessages();
  assertNoRepeatedNotice(settled, label);
  if (settled.some((message) => HOSTILE_PROVIDER_NOTICE.test(message))) {
    throw new Error(`${label} reported an unfinished provider tree as hostile: ${JSON.stringify(settled)}`);
  }
  return { notice, settled };
}

// A refused join may not destroy what the provider has already delivered. New
// entries are ordinary (a live managed runtime keeps its own bookkeeping), so
// this is a retention check, not a whole-tree freeze.
function providerFileBytes(graph) {
  const root = path.join(graph, ".tine-sync", "v2");
  return new Map(providerFiles(graph).map((relative) =>
    [relative, crypto.createHash("sha256").update(fs.readFileSync(path.join(root, relative))).digest("hex")]));
}

function assertDeliveredBytesRetained(before, graph, label) {
  const after = providerFileBytes(graph);
  for (const [relative, digest] of before) {
    if (after.get(relative) !== digest) {
      throw new Error(`${label} changed or removed already-delivered provider bytes at ${relative}`);
    }
  }
}

async function attemptJoinInPartialState(state, label, toGraph) {
  const before = providerFileBytes(toGraph);
  await dismissToasts();
  await clickSettingsButtonMatchingAndConfirm(JOIN_BUTTON_PATTERN, `${label}-join`);
  const { notice, settled } = await joinNoticeAfter(label);
  if (settled.includes(JOIN_SUCCESS_NOTICE)) {
    throw new Error(`${label} reported a completed join from an unfinished provider tree`);
  }
  if (!NOT_READY_NOTICE.test(notice)) {
    throw new Error(`${label} did not present the unfinished delivery as "not ready yet": ${JSON.stringify(notice)}`);
  }
  assertDeliveredBytesRetained(before, toGraph, label);
  receipt.milestones[`partial-join-${state}`] = { notice, presentedAsNotReady: true, repeatedNotice: false };
  return notice;
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

async function assertManagedActive(label) {
  await openSyncSettings();
  await assertBody("Tine-managed storage active", label, 60_000);
  await closeSettingsIfOpen();
}

async function acceptNativeConfirmation(label, before) {
  const dialogId = await waitFor(() => windowIds(".*").find((id) => !before.has(id)), 15_000,
    `${label} did not open the native confirmation dialog`);
  execFileSync(XDOTOOL, ["windowactivate", "--sync", dialogId], { env });
  execFileSync(XDOTOOL, ["key", "--clearmodifiers", "alt+y"], { env });
  await waitFor(() => !windowIds(".*").includes(dialogId), 12_000,
    `${label} confirmation did not close`);
}

async function declineNativeConfirmation(label, before) {
  const dialogId = await waitFor(() => windowIds(".*").find((id) => !before.has(id)), 15_000,
    `${label} did not open the native confirmation dialog`);
  execFileSync(XDOTOOL, ["windowactivate", "--sync", dialogId], { env });
  execFileSync(XDOTOOL, ["key", "--clearmodifiers", "alt+n"], { env });
  await waitFor(() => !windowIds(".*").includes(dialogId), 12_000,
    `${label} confirmation did not close`);
}

async function clickButtonAndConfirm(text, label) {
  if (text === "to Direct Files") {
    const recoveryAction = await browser.$(".startup-recovery-actions button.danger");
    await recoveryAction.waitForClickable({ timeout: 15_000 });
    // The startup-recovery escape invokes the native cold return immediately and
    // deliberately shows no confirmation: a native modal can be starved behind
    // the very managed open this path abandons, which would trap the user
    // (src/startupRecovery.ts, `returnToDirectFiles`). The leg's own assertions
    // below — the source page, retained provider evidence, a writable Markdown
    // save — are what prove the escape happened.
    await recoveryAction.click();
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
  driver = spawn(TD, webdriverServerArgs(DRIVER_PORT, NATIVE_PORT, WD), {
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
    capabilities: tauriCapabilities(APP, "sparse-v2-two-device"),
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

async function forceKillCurrent(label) {
  const pid = appPid;
  if (!pid) throw new Error(`${label} had no application PID`);
  process.kill(pid, "SIGKILL");
  await waitFor(() => !processAlive(pid), 30_000, `${label}: SIGKILL did not stop Tine pid ${pid}`);
  receipt.milestones[label] = { pid, signal: "SIGKILL", at: new Date().toISOString() };
  appPid = undefined;
  try { await browser?.deleteSession(); } catch {}
  browser = undefined;
  bindingGeneration = undefined;
  try { if (driver?.pid) process.kill(-driver.pid, "SIGKILL"); } catch {}
  if (driver?.pid) await waitFor(() => driver.exitCode !== null || !processAlive(driver.pid), 8_000,
    `${label}: tauri-driver did not stop`);
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

function providerFiles(graph) {
  const root = path.join(graph, ".tine-sync", "v2");
  const files = [];
  const visit = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const absolute = path.join(dir, entry.name);
      if (entry.isDirectory()) visit(absolute);
      else if (entry.isFile()) files.push(path.relative(root, absolute));
    }
  };
  visit(root);
  return files.sort();
}

function changedProviderFiles(fromGraph, toGraph) {
  const sourceRoot = path.join(fromGraph, ".tine-sync", "v2");
  const targetRoot = path.join(toGraph, ".tine-sync", "v2");
  return providerFiles(fromGraph).filter((relative) => {
    const source = path.join(sourceRoot, relative);
    const target = path.join(targetRoot, relative);
    return !fs.existsSync(target) || !fs.readFileSync(source).equals(fs.readFileSync(target));
  });
}

function deliverProviderFile(fromGraph, toGraph, relative) {
  const source = path.join(fromGraph, ".tine-sync", "v2", relative);
  const target = path.join(toGraph, ".tine-sync", "v2", relative);
  fs.mkdirSync(path.dirname(target), { recursive: true });
  const temporary = `${target}.tine-delivery-${process.pid}`;
  fs.copyFileSync(source, temporary);
  fs.renameSync(temporary, target);
}

function stageProviderFile(fromGraph, toGraph, relative) {
  const source = path.join(fromGraph, ".tine-sync", "v2", relative);
  const target = path.join(toGraph, ".tine-sync", "v2", relative);
  fs.mkdirSync(path.dirname(target), { recursive: true });
  const temporary = `${target}.tine-delivery-${process.pid}`;
  fs.copyFileSync(source, temporary);
  return temporary;
}

function deliverProviderFiles(fromGraph, toGraph, relatives) {
  for (const relative of relatives) deliverProviderFile(fromGraph, toGraph, relative);
}

// ---------------------------------------------------------------------------
// A join transport that behaves like a file-sync tool.
//
// `copyProvider` materialises a complete provider tree in one atomic step. No
// sync tool does that: Syncthing/Dropbox create directories before their
// contents, deliver files in arbitrary order, and can leave the tree half built
// for minutes. The join boundary therefore uses the same ordered-partial
// delivery primitives (`deliverProviderFile`/`stageProviderFile`) that the
// later delivery-cut leg already relies on, driven through named states.
// ---------------------------------------------------------------------------

const SHARED_ENROLLMENT_DIRECTORIES = ["shared/outbox/enrollment", "shared/inbox/enrollment"];
const SHARED_ENROLLMENT_DESCRIPTOR = "shared/outbox/enrollment/shared-enrollment-v1.json";

function providerRelative(root, absolute) {
  return path.relative(root, absolute).split(path.sep).join("/");
}

function providerDirectories(graph) {
  const root = path.join(graph, ".tine-sync", "v2");
  const directories = [];
  const visit = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const absolute = path.join(dir, entry.name);
      directories.push(providerRelative(root, absolute));
      visit(absolute);
    }
  };
  visit(root);
  return directories.sort();
}

function isEnrollmentPath(relative) {
  return SHARED_ENROLLMENT_DIRECTORIES.some(
    (directory) => relative === directory || relative.startsWith(`${directory}/`),
  );
}

function createProviderDirectories(toGraph, relatives) {
  const root = path.join(toGraph, ".tine-sync", "v2");
  for (const relative of relatives) fs.mkdirSync(path.join(root, relative), { recursive: true });
}

function providerPath(graph, relative) {
  return path.join(graph, ".tine-sync", "v2", relative);
}

// A real mid-transfer descriptor: the canonical name exists, the bytes do not.
// This reuses the exact shape the cold-return leg already writes.
function truncateDeliveredDescriptor(toGraph) {
  const target = providerPath(toGraph, SHARED_ENROLLMENT_DESCRIPTOR);
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.writeFileSync(target, "{\n");
}

/**
 * The four join-boundary delivery states this journey drives, in the fixed
 * order a sync tool that creates directories before contents produces. The
 * space of orderings is larger than this — a provider may also deliver the
 * descriptor before the object namespaces exist, or interleave both trees — and
 * this journey does NOT enumerate it. What it does cover is the prefix the
 * discovery path actually keys on (`shared/outbox/enrollment/` and its single
 * canonical descriptor), which is where the product decides "not yet" versus
 * "unsafe".
 */
function deliverJoinState(state, fromGraph, toGraph) {
  const directories = providerDirectories(fromGraph);
  const files = providerFiles(fromGraph);
  const scaffold = directories.filter((relative) => !isEnrollmentPath(relative));
  const ordinary = files.filter((relative) => !isEnrollmentPath(relative));
  switch (state) {
    case "directories-before-contents":
      createProviderDirectories(toGraph, scaffold);
      return { directories: scaffold.length, files: 0, descriptor: "absent" };
    case "enrollment-directory-empty":
      createProviderDirectories(toGraph, directories);
      return { directories: directories.length, files: 0, descriptor: "absent" };
    case "descriptor-truncated":
      createProviderDirectories(toGraph, directories);
      deliverProviderFiles(fromGraph, toGraph, ordinary);
      truncateDeliveredDescriptor(toGraph);
      return { directories: directories.length, files: ordinary.length, descriptor: "truncated" };
    case "complete":
      createProviderDirectories(toGraph, directories);
      deliverProviderFiles(fromGraph, toGraph, files);
      return { directories: directories.length, files: files.length, descriptor: "complete" };
    default:
      throw new Error(`unknown join delivery state ${JSON.stringify(state)}`);
  }
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
  if (SOURCE) {
    await selectExistingEditablePage();
    await assertPageContains(selectedRaw, "device A selected source block");
  } else {
    const page = await getPage();
    selectedRaw = page.blocks[0].raw;
    selected = {
      entry: { name: PAGE, kind: "page", path: `pages/${PAGE}.md` },
      file: path.join(GRAPH_A, "pages", `${PAGE}.md`),
      blockId: page.blocks[0].id,
    };
    await assertPageContains(MARKER, "device A source page");
  }
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

  phase = "device-b-starting-state";
  // Device B is connected BEFORE any provider bytes arrive, in both orderings.
  // That is what a user sees: the app is already running when the sync tool
  // starts filling the folder, so the join must recover in place rather than
  // depend on a restart that happens to observe a finished tree.
  await connect("device-b-join", GRAPH_B, XDG_B);
  await assertPageContains(SOURCE ? selectedRaw : MARKER, "device B source page");
  let deviceBOwnEdit;
  if (JOIN_FROM_MANAGED) {
    // The joining device is not blank: it already runs Tine-managed storage on
    // its own graph and has written history there after activating.
    await openSyncSettings();
    await clickButtonAndConfirm("Enable Tine-managed storage...", "device-b-enable-managed");
    await assertBody("Tine-managed storage active", "device B own managed activation", 120_000);
    await closeSettingsIfOpen();
    await leaseCurrentGraph(GRAPH_B);
    deviceBOwnEdit = `${MARKER} device-b own managed history`;
    if (SOURCE) await appendThroughVisibleEditor(GRAPH_B, deviceBOwnEdit, "device-b-own-managed-edit");
    else await saveManagedRaw(deviceBOwnEdit);
    await settleManaged("device B own managed edit");
    await assertVisiblePageContains(deviceBOwnEdit, "device B own managed edit before joining");
  }

  phase = "device-b-partial-provider-delivery";
  // Which partial states a joining device can reach depends on what its OWN
  // storage already left in the graph, so read that rather than assume it. A
  // Direct Files device must have no provider tree at all — opening a Direct
  // Files graph is not an implicit managed-storage probe, and that invariant is
  // asserted here rather than taken on trust. A device with its own managed
  // storage may or may not have published an empty enrollment scaffold; if it
  // has, "the enrollment directory has not arrived yet" is genuinely
  // unreachable for it and the receipt records that instead of pretending.
  const enrollmentPreexists = fs.existsSync(providerPath(GRAPH_B, SHARED_ENROLLMENT_DIRECTORIES[0]));
  if (!JOIN_FROM_MANAGED && fs.existsSync(path.join(GRAPH_B, ".tine-sync"))) {
    throw new Error("a Direct Files device grew managed-storage provider state without being asked");
  }
  const partialStates = enrollmentPreexists
    ? ["enrollment-directory-empty", "descriptor-truncated"]
    : ["directories-before-contents", "enrollment-directory-empty", "descriptor-truncated"];
  receipt.partialDeliveryStates = { covered: [...partialStates, "complete"], enrollmentPreexists };
  await openSyncSettings();
  for (const state of partialStates) {
    receipt.milestones[`delivery-${state}`] = deliverJoinState(state, GRAPH_A, GRAPH_B);
    await attemptJoinInPartialState(state, `device B with ${state}`, GRAPH_B);
  }
  receipt.milestones["delivery-complete"] = deliverJoinState("complete", GRAPH_A, GRAPH_B);
  await dismissToasts();

  phase = "device-b-discovery-and-join";
  // Opening an ordinary Direct Files graph is intentionally not an implicit
  // managed-storage/provider probe. The explicit Join action performs cold
  // discovery and then transitions this installation to SharedActive. The same
  // action is offered from either storage mode, so it is selected by role.
  const joinWindows = await clickSettingsButtonMatchingAndConfirm(JOIN_BUTTON_PATTERN, "device-b-join");
  if (JOIN_FROM_MANAGED) {
    // What happens to device B's OWN pre-join managed history is quoted from
    // the backend, not inferred. `join_shared_clean`
    // (crates/tine-core/src/sync_runtime.rs) compares the delivered descriptor
    // against this device's own binding and refuses before touching anything:
    //   "clean shared descriptor names another managed graph"
    // Two independent activations mint independent `WorkspaceId::new()` values,
    // so that is the branch a second managed device always takes. Nothing is
    // retired, archived or merged: the assertion is therefore that device B
    // keeps exactly what it had, including the edit it made after its own
    // activation, and that the user is told why the join did not happen.
    // The product now turns this otherwise-correct refusal into an actionable
    // second prompt: adopt the other device's history, archiving this one's.
    // This leg proves the conservative choice. Decline that prompt through the
    // real native dialog, then require the refusal explanation and byte-exact
    // retention below. Leaving the prompt open made WebKitWebDriver appear to
    // hang and tested neither outcome.
    await declineNativeConfirmation("device-b-decline-adoption", joinWindows);
    const { notice, settled } = await joinNoticeAfter("device B joining from its own managed storage");
    if (settled.includes(JOIN_SUCCESS_NOTICE)) {
      throw new Error("device B reported a completed join while holding its own managed history");
    }
    if (!notice.includes("another managed graph")) {
      throw new Error(`device B was not told why the join stopped: ${JSON.stringify(notice)}`);
    }
    await closeSettingsIfOpen();
    await leaseCurrentGraph(GRAPH_B);
    await assertVisiblePageContains(deviceBOwnEdit, "device B own managed edit after the refused join");
    await assertManagedActive("device B managed state after the refused join");
    await stopCurrent();
    // The refusal must also be durable: the delivered provider tree it declined
    // must not strand device B behind its own storage on the next open.
    await connect("device-b-after-refused-join", GRAPH_B, XDG_B);
    await waitForManagedText(deviceBOwnEdit, "device B own managed edit after reopen", 180_000);
    await assertVisiblePageContains(deviceBOwnEdit, "device B own managed edit visible after reopen");
    await assertManagedActive("device B managed state after reopen");
    receipt.milestones.joinFromManaged = {
      refusalNotice: notice,
      ownHistoryReadable: true,
      ownHistoryReadableAfterReopen: true,
      managedStillActive: true,
    };
    await stopCurrent();
  } else if (SOURCE) {
    await assertBody("This device joined the synced graph.", "device B joined state", 120_000);
    await leaseCurrentGraph(GRAPH_B);
    receipt.milestones.join = { openedWithoutLocalBinding: true, joinedAfterPartialDelivery: true };
    const fromB = `${MARKER} device-b`;
    await appendThroughVisibleEditor(GRAPH_B, fromB, "device-b-visible-edit");
    await settleManaged("device B outbound edit");

    phase = "device-b-local-edit-unclean-restart";
    await forceKillCurrent("device-b-local-edit-kill");
    await connect("device-b-crash-reopen", GRAPH_B, XDG_B);
    await waitForManagedText(fromB, "device B local edit after crash reopen", 180_000);
    await assertVisiblePageContains(fromB, "device B recovered edit");
    await assertManagedActive("device B managed state after crash reopen");
    await stopCurrent();

    phase = "device-b-to-device-a-delivery-cut";
    await connect("device-a-delivery-cut", GRAPH_A, XDG_A);
    const changed = changedProviderFiles(GRAPH_B, GRAPH_A);
    if (changed.length === 0) throw new Error("device B edit produced no provider files for device A");
    const completeBeforeKill = Math.max(0, Math.floor((changed.length - 1) / 2));
    deliverProviderFiles(GRAPH_B, GRAPH_A, changed.slice(0, completeBeforeKill));
    const staged = stageProviderFile(GRAPH_B, GRAPH_A, changed[completeBeforeKill]);
    // Leave a genuinely incomplete filesystem delivery in place and kill the
    // app around that external boundary. Do not call sparse_v2_tick: this is a
    // real-app watcher/recovery proof, not a core-command simulation.
    await sleep(300);
    receipt.milestones.deliveryCut = {
      changedFiles: changed.length,
      completedBeforeKill: completeBeforeKill,
      stagedRelative: changed[completeBeforeKill],
    };
    await forceKillCurrent("device-a-provider-delivery-kill");
    fs.rmSync(staged, { force: true });
    deliverProviderFiles(GRAPH_B, GRAPH_A, changed);

    await connect("device-a-receive-after-cut", GRAPH_A, XDG_A);
    await waitForManagedText(fromB, "device B edit on device A after delivery cut", 180_000);
    await assertVisiblePageContains(fromB, "device B edit visible on device A after delivery cut");
    await settleManaged("device A after receiving B edit");
    const fromA = `${MARKER} device-a`;
    await appendThroughVisibleEditor(GRAPH_A, fromA, "device-a-visible-edit");
    await settleManaged("device A outbound edit");
    await stopCurrent();

    phase = "device-a-to-device-b";
    const backToB = changedProviderFiles(GRAPH_A, GRAPH_B);
    if (backToB.length === 0) throw new Error("device A edit produced no provider files for device B");
    deliverProviderFiles(GRAPH_A, GRAPH_B, backToB);
    await connect("device-b-final-receive", GRAPH_B, XDG_B);
    await waitForManagedText(fromA, "device A edit on device B", 180_000);
    await assertVisiblePageContains(fromA, "device A edit visible on device B");
    await settleManaged("device B after receiving A edit");
    const finalPage = await getPage();
    const finalRaw = flattenBlocks(finalPage.blocks ?? []).map((block) => block.raw).join("\n");
    for (const marker of [fromB, fromA]) {
      if ((finalRaw.match(new RegExp(marker.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "g")) || []).length !== 1) {
        throw new Error(`final device B state did not contain exactly one ${JSON.stringify(marker)}`);
      }
    }
    receipt.milestones.bidirectional = {
      bToA: true,
      aToB: true,
      visibleUi: true,
      exactNoDuplicates: true,
      localEditCrashRecovered: true,
      deliveryCutCrashRecovered: true,
    };
    await stopCurrent();
  } else {
    await assertBody("This device joined the synced graph.", "device B joined state", 120_000);
    await leaseCurrentGraph(GRAPH_B);
    receipt.milestones.join = { openedWithoutLocalBinding: true, joinedAfterPartialDelivery: true };
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
    const providerRoot = path.join(GRAPH_B, ".tine-sync", "v2");
    const providerBeforeReturn = exactTreeDigest(providerRoot);
    await connect("device-b-cold-return", GRAPH_B, XDG_B, true);
    await assertBody("Tine-managed storage sync data appears to still be arriving or is incomplete", "fresh device B recovery screen");
    await clickButtonAndConfirm("to Direct Files", "device-b-cold-return");
    await leaseCurrentGraph(GRAPH_B);
    await assertPageContains(MARKER, "fresh device B source page after Direct Files return", 30_000);
    if (!fs.existsSync(providerRoot)
      || exactTreeDigest(providerRoot) !== providerBeforeReturn) {
      throw new Error("fresh-device emergency return changed retained provider evidence");
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
    receipt.milestones.directReturn = { providerEvidencePreserved: true, writableMarkdown: true };
  }

  receipt.result = "pass";
  fs.writeFileSync(
    path.join(ARTIFACTS, `sparse-v2-two-device-${JOIN_ORDERING}-receipt.json`),
    `${JSON.stringify(receipt, null, 2)}\n`,
  );
  console.log(`PASS: sparse-v2 two-device ${JOIN_ORDERING} held through ordered partial provider delivery: ${JSON.stringify(receipt.milestones)}`);
} catch (error) {
  try { await browser?.saveScreenshot(path.join(ARTIFACTS, `failure-${JOIN_ORDERING}.png`)); } catch {}
  const failure = {
    testedCommit: receipt.testedCommit,
    journey: "sparse-v2-two-device",
    joinOrdering: JOIN_ORDERING,
    phase,
    expected: JOIN_FROM_MANAGED
      ? "A device that already runs Tine-managed storage on its own graph is offered the join it supports, is never told that an unfinished provider delivery is unsafe, and keeps its own post-activation history whatever the join decides."
      : "A Direct Files device is never told that an unfinished provider delivery is unsafe, can join once the bytes land without restarting, exchanges edits in both directions, and can instead return to writable Direct Files.",
    observed: String(error).split("\n").slice(0, 4).join(" | "),
    classification: /HARNESS UNAVAILABLE|tauri-driver|WebKit|xdotool|window manager|DISPLAY/i.test(String(error))
      ? "infrastructure"
      : "product",
  };
  fs.writeFileSync(path.join(ARTIFACTS, `failure-capsule-${JOIN_ORDERING}.json`), `${JSON.stringify(failure, null, 2)}\n`);
  console.error(`E2E FAILURE CAPSULE ${JSON.stringify(failure)}`);
  process.exitCode = 1;
} finally {
  try { await stopCurrent(); } catch {}
  try { if (wm?.pid) process.kill(-wm.pid, "SIGKILL"); } catch {}
  try { if (wmLog !== undefined) fs.closeSync(wmLog); } catch {}
}
