// Harvest B3 live-conflict capsule matrix.
//
// For Direct Files and Tine-managed storage, two retained drafts race external
// atomic replacements. A full process restart must restore both exact drafts;
// one is resolved to the retained draft and one to the current storage owner.
// The graph-keyed native capsule is observed before restart, shrinks after the
// first resolution, and is durably absent before the second success is shown.
//
// Usage: TINE_APP=/path/to/tine node scripts/e2e-concord-live-save.mjs
import { execFileSync, spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { remote } from "webdriverio";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { waitForFileText } from "./e2e-file-poll.mjs";
import { tauriCapabilities, webdriverServerArgs } from "./e2e-capabilities.mjs";

await ensureDisplay();

const APP = process.env.TINE_APP || `${process.env.HOME}/research/tine`;
const TD = process.env.TAURI_DRIVER
  || (process.env.CARGO_HOME ? `${process.env.CARGO_HOME}/bin/tauri-driver` : "tauri-driver");
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4500);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4501);

function waitFor(check, timeout, message, interval = 100) {
  const deadline = Date.now() + timeout;
  return (async () => {
    let last;
    while (Date.now() < deadline) {
      try {
        const value = await check();
        if (value) return value;
      } catch (error) {
        last = error;
      }
      await sleep(interval);
    }
    throw new Error(`${message}${last ? `; last observation: ${String(last)}` : ""}`);
  })();
}

function windowIds(env, pattern = "^Tine$") {
  try {
    return execFileSync("xdotool", ["search", "--onlyvisible", "--name", pattern], {
      encoding: "utf8",
      env,
    }).trim().split(/\s+/).filter(Boolean);
  } catch {
    return [];
  }
}

function capsuleFiles(root) {
  const found = [];
  const walk = (dir) => {
    let entries;
    try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
    for (const entry of entries) {
      const candidate = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(candidate);
      else if (entry.name.endsWith(".v1.json") && candidate.includes("conflict-capsules")) {
        found.push(candidate);
      }
    }
  };
  walk(root);
  return found;
}

function readCapsule(root) {
  const files = capsuleFiles(root);
  if (files.length !== 1) throw new Error(`expected one capsule file, found ${files.length}`);
  return { file: files[0], envelope: JSON.parse(fs.readFileSync(files[0], "utf8")) };
}

async function visibleButtonContaining(browser, text) {
  for (const button of await browser.$$("button")) {
    if (await button.isDisplayed() && (await button.getText()).includes(text)) return button;
  }
  return undefined;
}

async function acceptNativeConfirmation(env, label, before) {
  const dialog = await waitFor(
    () => windowIds(env).find((id) => !before.has(id)),
    30_000,
    `${label} did not show its native confirmation`,
  );
  execFileSync("xdotool", ["windowactivate", "--sync", dialog], { env });
  execFileSync("xdotool", ["key", "--clearmodifiers", "alt+y"], { env });
  await waitFor(() => !windowIds(env).includes(dialog), 30_000, `${label} confirmation did not close`);
}

async function enableManagedStorage(browser, env) {
  const trigger = await browser.$('button[title^="Settings"]');
  await trigger.waitForDisplayed({ timeout: 30_000 });
  await trigger.click();
  await browser.$(".settings-modal").waitForDisplayed({ timeout: 30_000 });
  const tab = await waitFor(
    () => visibleButtonContaining(browser, "Backups & recovery"),
    30_000,
    "Backups & recovery settings tab was absent",
  );
  await tab.click();
  const experimental = await browser.$(".settings-experimental .settings-advanced-toggle");
  await experimental.waitForDisplayed({ timeout: 30_000 });
  if ((await experimental.getAttribute("aria-expanded")) !== "true") await experimental.click();
  const action = await waitFor(
    () => visibleButtonContaining(browser, "Enable Tine-managed storage..."),
    30_000,
    "managed activation action was absent",
  );
  const before = new Set(windowIds(env));
  await action.click();
  await acceptNativeConfirmation(env, "managed activation", before);
  await browser.waitUntil(async () => (await browser.$("body").getText()).includes("Tine-managed storage active"), {
    timeout: 300_000,
    interval: 250,
    timeoutMsg: "managed activation did not reach active",
  });
  const close = await browser.$(".settings-pane-head .icon-btn:not(.settings-maximize)");
  await close.click();
  await browser.$(".settings-modal").waitForExist({ reverse: true, timeout: 30_000 });
}

async function openPage(browser, title) {
  const search = await browser.$('button[title^="Search (Ctrl+K)"]');
  await search.waitForClickable({ timeout: 30_000 });
  await search.click();
  const input = await browser.$(".switcher-input");
  await input.waitForExist({ timeout: 15_000 });
  await input.setValue(title);
  const row = await waitFor(async () => {
    for (const candidate of await browser.$$(".switcher-item")) {
      if ((await candidate.getText()).includes(title)) return candidate;
    }
    return undefined;
  }, 15_000, `${title} was absent from quick switch`);
  await row.click();
  await browser.waitUntil(async () => (await browser.$("h1.page-title").getText()) === title, {
    timeout: 15_000,
    timeoutMsg: `${title} did not open`,
  });
}

async function editPage(browser, text) {
  await browser.$(".page-blocks .ls-block .block-content").click();
  const editor = await browser.$(".page-blocks textarea.block-editor");
  await editor.waitForExist({ timeout: 5_000 });
  await browser.execute((next) => {
    const textarea = document.querySelector(".page-blocks textarea.block-editor");
    if (!(textarea instanceof HTMLTextAreaElement)) throw new Error("editor missing");
    const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
    setter?.call(textarea, next);
    textarea.dispatchEvent(new InputEvent("input", {
      bubbles: true,
      inputType: "insertText",
      data: next,
    }));
  }, text);
}

async function assertLiveConflict(browser, local, current, phase) {
  const resolver = await browser.$(".page-conflict");
  await resolver.waitForExist({ timeout: 30_000 });
  await browser.waitUntil(async () => {
    const cells = await browser.execute(() =>
      [...document.querySelectorAll(".page-conflict .sync-merge-cell")]
        .map((cell) => cell.textContent ?? ""));
    return cells.some((value) => value.includes(local))
      && cells.some((value) => value.includes(current));
  }, { timeout: 30_000, timeoutMsg: `${phase}: resolver did not retain both sides` });
  const geometry = await browser.execute(() => {
    const pane = document.querySelector(".main-content");
    const inner = document.querySelector(".main-content-inner");
    const panel = document.querySelector(".page-conflict");
    if (!(pane instanceof HTMLElement)
      || !(inner instanceof HTMLElement)
      || !(panel instanceof HTMLElement)) return null;
    const style = getComputedStyle(inner);
    return {
      panelWidth: panel.getBoundingClientRect().width,
      usablePaneWidth: pane.getBoundingClientRect().width
        - parseFloat(style.paddingLeft)
        - parseFloat(style.paddingRight),
    };
  });
  if (!geometry || Math.abs(geometry.panelWidth - geometry.usablePaneWidth) > 2) {
    throw new Error(`${phase}: Concord did not span the pane's usable width: ${JSON.stringify(geometry)}`);
  }
  if (phase === "direct:Keep Draft:initial") {
    await browser.saveScreenshot("/tmp/e2e-concord-live-save-width.png");
  }
}

async function resolveEverywhere(browser, side) {
  const selector = side === "mine" ? "All mine" : "All theirs";
  const choose = await waitFor(
    () => visibleButtonContaining(browser, selector),
    15_000,
    `${selector} action was absent`,
  );
  await choose.click();
  const apply = await waitFor(
    () => visibleButtonContaining(browser, "Apply resolution"),
    15_000,
    "Apply resolution action was absent",
  );
  await apply.click();
  await browser.waitUntil(async () => (await browser.$$(".page-conflict")).length === 0, {
    timeout: 30_000,
    timeoutMsg: "resolved capsule stayed visible",
  });
}

async function waitForApp(browser, phase) {
  await browser.$("body").waitForExist({ timeout: 30_000 });
  try {
    await browser.$(".ls-block, .page-title").waitForExist({ timeout: 30_000 });
  } catch (error) {
    const state = await browser.execute(() => ({
      body: document.body?.innerText,
      html: document.body?.innerHTML.slice(0, 2000),
      location: location.href,
    }));
    throw new Error(`${phase}: app content did not appear: ${JSON.stringify(state)}; ${String(error)}`);
  }
}

async function runBackend(mode) {
  const suffix = mode === "managed" ? "managed" : "direct";
  const graph = `/tmp/tgraph-concord-live-save-${suffix}`;
  const xdg = `/tmp/txdg-concord-live-save-${suffix}`;
  const data = `${xdg}/data`;
  const mineName = "Keep Draft";
  const theirsName = "Use Current";
  const mineFile = `${graph}/pages/${mineName}.md`;
  const theirsFile = `${graph}/pages/${theirsName}.md`;
  fs.rmSync(graph, { recursive: true, force: true });
  fs.rmSync(xdg, { recursive: true, force: true });
  for (const dir of [`${graph}/pages`, `${graph}/journals`, `${graph}/logseq`, data, `${xdg}/config`, `${xdg}/cache`]) {
    fs.mkdirSync(dir, { recursive: true });
  }
  fs.writeFileSync(mineFile, "- common mine base\n");
  fs.writeFileSync(theirsFile, "- common theirs base\n");
  fs.writeFileSync(`${graph}/journals/2026_09_01.md`, `- [[${mineName}]]\n- [[${theirsName}]]\n`);

  const env = {
    ...process.env,
    TINE_GRAPH: graph,
    XDG_DATA_HOME: data,
    XDG_CONFIG_HOME: `${xdg}/config`,
    XDG_CACHE_HOME: `${xdg}/cache`,
    WEBKIT_DISABLE_DMABUF_RENDERER: "1",
    LIBGL_ALWAYS_SOFTWARE: "1",
    WEBKIT_DISABLE_COMPOSITING_MODE: "1",
    GDK_BACKEND: "x11",
  };
  const log = fs.openSync(`/tmp/td-concord-live-save-${suffix}.log`, "w");
  const driver = spawn(TD, webdriverServerArgs(DRIVER_PORT, NATIVE_PORT, process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver"), {
    env,
    stdio: ["ignore", log, log],
    detached: true,
  });
  await sleep(3000);
  const newSession = async () => {
    const browser = await remote({
      hostname: "127.0.0.1",
      port: DRIVER_PORT,
      path: "/",
      capabilities: tauriCapabilities(APP, `concord-live-save-${suffix}`),
      logLevel: "error",
      connectionRetryCount: 1,
      connectionRetryTimeout: 60_000,
    });
    // Keep a genuinely spacious desktop pane for Concord's review-width
    // contract; narrow-pane behavior has its own container-query coverage.
    await browser.setWindowSize(1400, 900);
    return browser;
  };

  let browser;
  try {
    browser = await newSession();
    await waitForApp(browser, `${suffix}:initial`);
    if (mode === "managed") await enableManagedStorage(browser, env);

    const cases = [
      { name: mineName, file: mineFile, local: `${suffix} retained laptop draft`, current: `${suffix} current phone body` },
      { name: theirsName, file: theirsFile, local: `${suffix} second retained draft`, current: `${suffix} second current body` },
    ];
    for (const item of cases) {
      await openPage(browser, item.name);
      await editPage(browser, item.local);
      const replacement = `${item.file}.external`;
      fs.writeFileSync(replacement, `- ${item.current}\n`);
      fs.renameSync(replacement, item.file);
      await assertLiveConflict(browser, item.local, item.current, `${suffix}:${item.name}:initial`);
    }

    const beforeRestart = await waitFor(() => {
      const files = capsuleFiles(data);
      if (files.length !== 1) return undefined;
      const observed = readCapsule(data);
      return observed.envelope.capsules?.length === 2 ? observed : undefined;
    }, 30_000, `${suffix}: native capsule did not contain both retained drafts`);
    const byName = new Map(beforeRestart.envelope.capsules.map((capsule) => [capsule.page_name, capsule]));
    for (const item of cases) {
      const capsule = byName.get(item.name);
      if (!capsule || JSON.stringify(capsule).includes("authority")) {
        throw new Error(`${suffix}:${item.name}: capsule binding/authority shape is invalid`);
      }
      const bytes = JSON.stringify(capsule.live.page);
      if (!bytes.includes(item.local) || typeof capsule.live.base_rev !== "string") {
        throw new Error(`${suffix}:${item.name}: exact draft or base revision missing from capsule`);
      }
      if (mode === "managed" && (capsule.live.disk_rev !== undefined || capsule.live.conflict_epoch !== -1)) {
        throw new Error(`${suffix}:${item.name}: Managed replacement authority leaked into capsule`);
      }
    }

    await browser.deleteSession();
    browser = undefined;
    await sleep(1500);
    browser = await newSession();
    await waitForApp(browser, `${suffix}:restart`);

    await openPage(browser, mineName);
    await assertLiveConflict(browser, cases[0].local, cases[0].current, `${suffix}:mine:restart`);
    await resolveEverywhere(browser, "mine");
    await waitForFileText(mineFile, (text) => text.includes(cases[0].local), `${suffix}: keep retained draft`);
    const afterFirst = readCapsule(data).envelope.capsules;
    if (afterFirst.length !== 1 || afterFirst[0].page_name !== theirsName) {
      throw new Error(`${suffix}: first resolution was acknowledged before durable capsule retirement`);
    }

    await openPage(browser, theirsName);
    await assertLiveConflict(browser, cases[1].local, cases[1].current, `${suffix}:theirs:restart`);
    await resolveEverywhere(browser, "theirs");
    await waitForFileText(theirsFile, (text) => text.includes(cases[1].current), `${suffix}: use current owner`);
    if (capsuleFiles(data).length !== 0) {
      throw new Error(`${suffix}: final resolution was acknowledged before capsule file retirement`);
    }
    const legacy = await browser.execute(() => localStorage.getItem("tine.concord.live-conflicts.v1"));
    if (legacy !== null) throw new Error(`${suffix}: retired localStorage channel survived first use`);
    console.log(`PASS: ${suffix} live conflicts restored exact capsules and resolved both sides`);
  } finally {
    try { if (browser) await browser.deleteSession(); } catch {}
    try { process.kill(-driver.pid, "SIGKILL"); } catch {}
    try { fs.closeSync(log); } catch {}
    await sleep(1500);
  }
}

let failure;
try {
  await runBackend("direct");
  await runBackend("managed");
  console.log("PASS: Harvest B3 Direct/Managed restart capsule matrix");
} catch (error) {
  failure = error;
  console.error("FAIL:", error?.stack ?? error);
}

process.exit(failure ? 1 : 0);
