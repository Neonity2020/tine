// Concord sync-copy handoff journey (GH #337).
//
// A recognized Syncthing conflict copy arrives while its winner is open. Both
// native-event and polling modes must derive the same queue, surface the review,
// commit the requested decision (keep-both by default, or same-content
// keep-mine), install that exact page, and stay settled past autosave and a
// process restart.
//
// Usage:
//   TINE_APP=/path/to/tine TINE_E2E_WATCH_MODE=inotify node scripts/e2e-concord-sync-copy.mjs
//   TINE_APP=/path/to/tine TINE_E2E_WATCH_MODE=poll node scripts/e2e-concord-sync-copy.mjs
import { spawn } from "node:child_process";
import fs from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";
import { remote } from "webdriverio";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { waitForFileText } from "./e2e-file-poll.mjs";

await ensureDisplay();

const mode = process.env.TINE_E2E_WATCH_MODE === "poll" ? "poll" : "inotify";
const suffix = mode === "poll" ? "poll" : "native";
const G = `/tmp/tgraph-concord-sync-copy-${suffix}`;
const corpus = process.env.TINE_E2E_CONCORD_CORPUS;
const target = process.env.TINE_E2E_CONCORD_TARGET === "note" ? "note" : "journal";
const decision = process.env.TINE_E2E_CONCORD_DECISION === "mine" ? "mine" : "both";
const APP = process.env.TINE_APP || `${process.env.HOME}/research/tine`;
const TD = process.env.TAURI_DRIVER
  || (process.env.CARGO_HOME ? `${process.env.CARGO_HOME}/bin/tauri-driver` : "tauri-driver");
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4510);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4511);
const now = new Date();
const journalStem = [
  now.getFullYear(),
  String(now.getMonth() + 1).padStart(2, "0"),
  String(now.getDate()).padStart(2, "0"),
].join("_");
const pageFile = target === "journal" ? `${G}/journals/${journalStem}.md` : `${G}/pages/Note.md`;
const conflictFile = target === "journal"
  ? `${G}/journals/${journalStem}.sync-conflict-20260822-120000-PHONE.md`
  : `${G}/pages/Note.sync-conflict-20260822-120000-PHONE.md`;

fs.rmSync(G, { recursive: true, force: true });
if (corpus) fs.cpSync(corpus, G, { recursive: true });
fs.mkdirSync(`${G}/pages`, { recursive: true });
fs.mkdirSync(`${G}/journals`, { recursive: true });
fs.mkdirSync(`${G}/logseq`, { recursive: true });
fs.writeFileSync(pageFile, "- desktop version\n");
if (target === "note") fs.writeFileSync(`${G}/journals/${journalStem}.md`, "- open [[Note]]\n");

const xdg = `/tmp/txdg-concord-sync-copy-${suffix}`;
fs.rmSync(xdg, { recursive: true, force: true });
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(`${xdg}/${dir}`, { recursive: true });
if (mode === "poll") {
  const appData = `${xdg}/data/page.tine.Tine`;
  fs.mkdirSync(appData, { recursive: true });
  fs.writeFileSync(`${appData}/tine-settings.json`, JSON.stringify({ watch_mode: "poll" }));
}
const env = {
  ...process.env,
  TINE_GRAPH: G,
  XDG_DATA_HOME: `${xdg}/data`,
  XDG_CONFIG_HOME: `${xdg}/config`,
  XDG_CACHE_HOME: `${xdg}/cache`,
  WEBKIT_DISABLE_DMABUF_RENDERER: "1",
  LIBGL_ALWAYS_SOFTWARE: "1",
  WEBKIT_DISABLE_COMPOSITING_MODE: "1",
  GDK_BACKEND: "x11",
};

const log = fs.openSync(`/tmp/td-concord-sync-copy-${suffix}.log`, "w");
const driver = spawn(TD, [
  "--port", String(DRIVER_PORT),
  "--native-port", String(NATIVE_PORT),
  "--native-driver", process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver",
], { env, stdio: ["ignore", log, log], detached: true });
await sleep(3000);

async function newSession() {
  return remote({
    hostname: "127.0.0.1",
    port: DRIVER_PORT,
    path: "/",
    capabilities: {
      browserName: "wry",
      "wdio:enforceWebDriverClassic": true,
      "tauri:options": { application: APP },
    },
    logLevel: "error",
    connectionRetryCount: 1,
    connectionRetryTimeout: 60_000,
  });
}

async function openTarget(browser) {
  await browser.$(".ls-block, .page-title").waitForExist({ timeout: 20_000 });
  if (target === "journal") {
    await browser.waitUntil(async () =>
      (await browser.$(".page-blocks").getText()).includes("desktop version"),
    { timeout: 20_000, timeoutMsg: "current journal did not open" });
    return;
  }
  const title = await browser.$("h1.page-title");
  if (!(await title.isExisting()) || (await title.getText()) !== "Note") {
    const ref = await browser.$(".page-ref");
    await ref.waitForExist({ timeout: 10_000 });
    await ref.click();
    await browser.waitUntil(async () =>
      (await browser.$("h1.page-title").getText()) === "Note",
    { timeout: 10_000, timeoutMsg: "Note did not open" });
  }
}

async function assertMergedUi(browser, phase) {
  await browser.waitUntil(async () => {
    const text = await browser.$(".page-blocks").getText();
    return text.includes("desktop version")
      && (decision === "both" ? text.includes("phone version") : !text.includes("phone version"));
  }, { timeout: 15_000, timeoutMsg: `${phase}: exact merged page was not installed` });
}

let browser;
let failure;
try {
  browser = await newSession();
  await openTarget(browser);

  fs.writeFileSync(conflictFile, "- phone version\n");
  const resolver = await browser.$(".page-conflict");
  await resolver.waitForExist({ timeout: mode === "poll" ? 15_000 : 10_000 });
  await browser.waitUntil(async () => browser.execute(() =>
    [...document.querySelectorAll(".toast")]
      .some((notice) => notice.textContent?.includes("new sync conflict"))
  ), { timeout: 10_000, timeoutMsg: "new conflict arrived without an actionable notice" });

  await browser.execute(() => {
    for (const button of document.querySelectorAll(".page-conflict button")) {
      const text = button.textContent?.trim();
      if (text?.includes("Keep both everywhere")) button.setAttribute("data-concord-keep-both", "true");
      if (text?.includes("Keep This device")) button.setAttribute("data-concord-keep-mine", "true");
      if (text === "Apply resolution") button.setAttribute("data-concord-apply", "true");
    }
  });
  await browser.$(decision === "both"
    ? '[data-concord-keep-both="true"]'
    : '[data-concord-keep-mine="true"]').click();
  const applyStarted = performance.now();
  await browser.$('[data-concord-apply="true"]').click();

  await waitForFileText(
    pageFile,
    (text) => text.includes("desktop version")
      && (decision === "both" ? text.includes("phone version") : !text.includes("phone version")),
    `${mode} conflict-copy merge`,
  );
  const durableMs = performance.now() - applyStarted;
  await browser.waitUntil(async () => !fs.existsSync(conflictFile), {
    timeout: 10_000,
    timeoutMsg: "resolved conflict copy remained in the graph",
  });
  await browser.waitUntil(async () => (await browser.$$(".page-conflict")).length === 0, {
    timeout: 10_000,
    timeoutMsg: "resolved conflict reopened immediately",
  });
  await assertMergedUi(browser, "after apply");
  const visibleMs = performance.now() - applyStarted;
  await browser.waitUntil(async () => browser.execute(() =>
    ![...document.querySelectorAll(".toast")]
      .some((notice) => notice.textContent?.includes("sync conflict"))
  ), { timeout: 10_000, timeoutMsg: "resolved conflict left a stale review notification" });
  const settledMs = performance.now() - applyStarted;

  const committed = fs.readFileSync(pageFile, "utf8");
  await sleep(2500); // beyond the ordinary save debounce
  if (fs.readFileSync(pageFile, "utf8") !== committed) {
    throw new Error("the stale pre-merge editor wrote after resolution");
  }
  if ((await browser.$$(".page-conflict")).length !== 0) {
    throw new Error("a second conflict appeared after the save debounce");
  }

  // The resolution episode is not complete until ordinary editing resumes.
  // Android exposed a parked winner-write replay here: the first new block woke
  // the stale watcher event, which re-opened Concord and could also drop today
  // from the Journals feed. Exercise the literal user continuation rather than
  // treating an inert resolved screen as sufficient proof.
  const resolvedSection = await browser.$(".page-section");
  const resolvedTitle = await resolvedSection.$("h1.page-title").getText();
  const trailing = await resolvedSection.$(".page-trailing-block-target");
  await trailing.waitForDisplayed({ timeout: 10_000 });
  await trailing.click();
  const editor = await browser.$("textarea.block-editor");
  await editor.waitForDisplayed({ timeout: 10_000 });
  await editor.addValue("post resolution edit");
  await browser.$("h1.page-title").click();
  await waitForFileText(
    pageFile,
    (text) => text.includes("post resolution edit"),
    `${mode} post-resolution edit`,
  );
  await sleep(2500);
  if ((await browser.$$(".page-conflict")).length !== 0) {
    throw new Error("ordinary editing after resolution reopened Concord");
  }
  await browser.waitUntil(async () => {
    const sections = await browser.$$(".page-section");
    for (const section of sections) {
      if (await section.$(`h1.page-title=${resolvedTitle}`).isExisting()) return true;
    }
    return false;
  }, { timeout: 10_000, timeoutMsg: "resolved current journal disappeared after ordinary editing resumed" });

  await browser.deleteSession();
  browser = undefined;
  await sleep(1200);
  browser = await newSession();
  await openTarget(browser);
  await assertMergedUi(browser, "after restart");
  if ((await browser.$$(".page-conflict")).length !== 0) {
    throw new Error("resolved conflict returned after restart");
  }
  console.log(`TIMING: ${mode}/${target}/${decision}${corpus ? "/corpus" : ""} apply durable=${durableMs.toFixed(1)}ms visible=${visibleMs.toFixed(1)}ms settled=${settledMs.toFixed(1)}ms`);
  console.log(`PASS: ${mode} ${decision} conflict-copy discovery and exact editor handoff`);
} catch (error) {
  failure = error;
  console.error("FAIL:", error?.message ?? error);
  try {
    if (browser) {
      const state = await browser.execute(() => ({
        page: document.querySelector(".page-blocks")?.textContent,
        conflicts: document.querySelectorAll(".page-conflict").length,
        toasts: [...document.querySelectorAll(".toast")].map((toast) => toast.textContent),
      }));
      console.error("STATE:", JSON.stringify(state));
      fs.writeFileSync(`/tmp/e2e-concord-sync-copy-${suffix}.png`, await browser.takeScreenshot(), "base64");
    }
  } catch {}
} finally {
  try { if (browser) await browser.deleteSession(); } catch {}
  try { process.kill(-driver.pid, "SIGKILL"); } catch {}
}

process.exit(failure ? 1 : 0);
