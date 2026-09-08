// Linux real-WebKit journey for the visual query builder (SPEC §7.2–§7.4): a
// `{{query}}` block RESTS as one plain-English sentence, EXPANDS into a sheet of
// rows, and every P6 grouping/enable/reorder edit remains an ordinary,
// Logseq-readable query block after save, undo and restart.
//
// Three things need a real browser rather than jsdom.
//
//  1. The sheet is PORTALLED to <body> and positioned from the sentence's rect,
//     because `.query-block` carries `transform: translateZ(0)` (the GH #64
//     WebKitGTK flicker fix) and that makes it a containing block for `position:
//     fixed` children. jsdom has no layout, so only a real engine can say the
//     sheet actually lands over the following blocks instead of inside a
//     clipped box — and only a real engine runs the `max-width: 600px` media
//     query that turns it into a bottom sheet.
//
//  2. Round-tripping through the ENGINE. Every edit here is printed by Rust and
//     re-read by Rust; a jsdom test can only mock that. Restarting the app and
//     finding the same sentence is the only proof that what landed on disk says
//     what the sheet said (I-4).
//
//  3. P6's pointer cancellation, keyboard focus continuity, and 390px target
//     geometry are properties of the native WebKit event/layout path. The last
//     launch uses the Rust-owned narrow-window E2E policy rather than mutating a
//     frontend viewport signal.
import { spawn } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { openPageByName } from "./lib/e2e-navigation.mjs";
import { tauriCapabilities, webdriverServerArgs } from "./e2e-capabilities.mjs";

await ensureDisplay();

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APP = process.env.TINE_APP || path.join(ROOT, "target/release/tine");
const TD = process.env.TAURI_DRIVER || (process.env.CARGO_HOME ? path.join(process.env.CARGO_HOME, "bin", "tauri-driver") : "tauri-driver");
const DRIVER_BASE = Number(process.env.E2E_DRIVER_PORT || 4496);
const disabledAlphaP6 = "- {{tine-query @block and off(content like '%alpha%') and content like '%beta%' and content like '%gamma%' and content like '%delta%'}}";
const savedDisabledGroupP6 = "- {{tine-query @block and content like '%beta%' and off(not (content like '%gamma%' or content like '%delta%')) and content like '%alpha%'}}";
const NATIVE_BASE = Number(process.env.E2E_NATIVE_PORT || 4497);
const TMP_ROOT = path.resolve(process.env.E2E_TMP_ROOT || process.env.TMPDIR || "/tmp");
fs.mkdirSync(TMP_ROOT, { recursive: true });
const TMP = fs.mkdtempSync(path.join(TMP_ROOT, "tine-query-sheet-e2e-"));
const GRAPH = `${TMP}/graph`;

for (const dir of ["pages", "journals", "logseq"]) fs.mkdirSync(`${GRAPH}/${dir}`, { recursive: true });
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(`${TMP}/xdg/${dir}`, { recursive: true });
fs.writeFileSync(`${GRAPH}/logseq/config.edn`, "{}\n");
const now = new Date();
const journal = `${now.getFullYear()}_${String(now.getMonth() + 1).padStart(2, "0")}_${String(now.getDate()).padStart(2, "0")}`;
fs.writeFileSync(`${GRAPH}/journals/${journal}.md`, "- Open [[Sheet]], [[Deep]], and [[P6 controls]]\n");

// The second query block is the CONTROL: nothing in this journey touches it, so
// after the first block is edited its bytes must be exactly what they were
// (I-4 — an untouched block is byte-identical, comment and spacing included).
// Its spelling is deliberately non-canonical — doubled spaces the query
// printer would collapse — so a reprint of this block could not go unnoticed.
// (Trailing whitespace is NOT usable for this: the file writer strips it
// line-wise across the whole file, which is behavior this packet neither owns
// nor changes.)
const UNTOUCHED = '- {{query (and  (property "owner"   "Ada"))}}';
const SHEET_PAGE = [
  "- {{query (and (task TODO))}}",
  UNTOUCHED,
  "- TODO alpha task",
  "  SCHEDULED: <2026-01-05 Mon>",
  "- TODO beta task",
  "- DONE gamma task",
  "",
].join("\n");
fs.writeFileSync(`${GRAPH}/pages/Sheet.md`, SHEET_PAGE);

// A deliberately over-nested query: outside content picks the shape, so the
// sheet must stay small however deep it goes (I-22).
fs.writeFileSync(
  `${GRAPH}/pages/Deep.md`,
  `- {{query ${"(and ".repeat(20)}(task TODO)${")".repeat(20)}}}\n`,
);

// Four distinct siblings make order and grouping visible in both the sheet and
// the engine-printed source. The second block is a byte sentinel: no P6 gesture
// targets it, so any rewrite of it exposes an over-broad save boundary.
const P6_QUERY = '- {{query (and "alpha" "beta" "gamma" "delta")}}';
const P6_UNTOUCHED = '- {{query (and  (property "guard"   "keep-these-bytes"))}}';
const P6_FILE = `${GRAPH}/pages/P6 controls.md`;
fs.writeFileSync(P6_FILE, [
  P6_QUERY,
  P6_UNTOUCHED,
  "- alpha beta gamma delta fixture",
  "",
].join("\n"));

const env = {
  ...process.env,
  TINE_GRAPH: GRAPH,
  XDG_DATA_HOME: `${TMP}/xdg/data`,
  XDG_CONFIG_HOME: `${TMP}/xdg/config`,
  XDG_CACHE_HOME: `${TMP}/xdg/cache`,
  WEBKIT_DISABLE_DMABUF_RENDERER: "1",
  WEBKIT_DISABLE_COMPOSITING_MODE: "1",
  LIBGL_ALWAYS_SOFTWARE: "1",
  GDK_BACKEND: "x11",
};

async function withApp(index, fn, { forceMobile = false } = {}) {
  const driverPort = DRIVER_BASE + index * 2;
  const nativePort = NATIVE_BASE + index * 2;
  const log = fs.openSync(`${TMP}/tauri-driver-${index}.log`, "w");
  const launchEnv = { ...env };
  if (forceMobile) launchEnv.TINE_E2E_FORCE_MOBILE_DRAWERS = "1";
  else delete launchEnv.TINE_E2E_FORCE_MOBILE_DRAWERS;
  const td = spawn(TD, webdriverServerArgs(driverPort, nativePort, process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver"), {
    env: launchEnv, stdio: ["ignore", log, log], detached: true,
  });
  await sleep(2500);
  let browser;
  try {
    browser = await remote({
      hostname: "127.0.0.1", port: driverPort, path: "/", logLevel: "error",
      connectionRetryCount: 1, connectionRetryTimeout: 60_000,
      capabilities: tauriCapabilities(APP, "query-sheet"),
    });
    await browser.$(".ls-block, .page-title").waitForExist({ timeout: 20_000 });
    await fn(browser);
    await sleep(750);
  } finally {
    try { await browser?.deleteSession(); } catch {}
    try { process.kill(-td.pid, "SIGKILL"); } catch {}
    fs.closeSync(log);
  }
}

/** Navigate through the product's Quick Switcher. The shared helper re-finds a
 *  result on each retry, so no WebDriver handle outlives the live result list. */
async function openPage(browser, title) {
  await browser.$(".ls-block, .page-title").waitForExist({ timeout: 20_000 });
  await openPageByName(browser, title);
  await browser.waitUntil(
    async () => (await browser.$("h1.page-title").getText()).trim() === title,
    { timeout: 10_000, timeoutMsg: `${title} did not open` },
  );
  await sleep(400);
}

const p6Lines = () => fs.readFileSync(P6_FILE, "utf8").split(/\r?\n/);

async function waitForP6Source(browser, predicate, message) {
  await browser.waitUntil(() => predicate(p6Lines()[0]), { timeout: 15_000, timeoutMsg: message });
  const lines = p6Lines();
  if (lines[1] !== P6_UNTOUCHED) {
    fail(`a P6 edit rewrote the unrelated control block:\n  before: ${JSON.stringify(P6_UNTOUCHED)}\n  after:  ${JSON.stringify(lines[1])}`);
  }
  return lines[0];
}

async function p6Root(browser) {
  return browser.execute(() => {
    const list = document.querySelector(".qs-sheet > .qs-rows");
    const items = list
      ? [...list.children].filter((element) => element.getAttribute("data-qs-parent") === "")
      : [];
    const identify = (element) => {
      const text = (element.textContent ?? "").toLowerCase();
      return ["alpha", "beta", "gamma", "delta"].filter((term) => text.includes(term)).join("+") || "unknown";
    };
    return items.map((element) => ({
      identity: identify(element),
      group: element.querySelector(":scope > .qs-group > .qs-group-header .qs-group-op")?.textContent?.trim() ?? null,
      ownEnabled: element.matches(".qs-row")
        ? element.querySelector(":scope > .qs-enabled")?.getAttribute("aria-checked") ?? null
        : element.querySelector(":scope > .qs-group > .qs-group-header .qs-enabled")?.getAttribute("aria-checked") ?? null,
      children: [...element.querySelectorAll(":scope > .qs-group > .qs-rows > [data-qs-parent]")].map(identify),
    }));
  });
}

async function waitForP6Root(browser, predicate, message) {
  let latest;
  await browser.waitUntil(async () => {
    latest = await p6Root(browser);
    return predicate(latest);
  }, { timeout: 10_000, timeoutMsg: message });
  return latest;
}

/** An edit made inside the sheet must preserve that editing session, including
 *  a printer dialect change. Observe a resting replacement explicitly so an
 *  unexpected dismissal cannot be hidden by the harness reopening it. */
async function settleP6SheetAfterSourceChange(browser, predicate, message) {
  let lifecycle = null;
  let latestShell = null;
  try {
    await browser.waitUntil(async () => {
      latestShell = await browser.execute(() => ({
        sheet: !!document.querySelector(".qs-sheet"),
        gear: !!document.querySelector(".qs-gear"),
        expanded: document.querySelector(".qs-gear")?.getAttribute("aria-expanded") ?? null,
      }));
      if (latestShell.sheet) {
        const items = await p6Root(browser);
        if (!predicate(items)) return false;
        lifecycle = "kept-open";
        return true;
      }
      if (latestShell.gear && latestShell.expanded === "false") {
        lifecycle = "unexpectedly-closed";
        return true;
      }
      return false;
    }, { timeout: 10_000, timeoutMsg: message });
  } catch (error) {
    const root = await p6Root(browser);
    const sheet = await browser.execute(() => document.querySelector(".qs-sheet")?.outerHTML ?? null);
    throw new Error(`${String(error)}; shell=${JSON.stringify(latestShell)}; root=${JSON.stringify(root)}; sheet=${sheet}`);
  }
  if (lifecycle === "unexpectedly-closed") {
    fail(`an in-sheet edit dismissed its editor: ${JSON.stringify(latestShell)}`);
  }
  return lifecycle;
}

async function selectP6Rows(browser, indices) {
  for (const index of indices) {
    const selector = `.qs-sheet > .qs-rows > .qs-row[data-qs-parent=""][data-row-index="${index}"] .qs-select`;
    const checkbox = await browser.$(selector);
    await checkbox.waitForExist({ timeout: 5_000 });
    await checkbox.click();
  }
  await browser.waitUntil(async () => {
    const selected = await browser.$$(".qs-sheet > .qs-rows > .qs-row[data-qs-parent=\"\"] .qs-select:checked");
    return selected.length === indices.length;
  }, { timeout: 5_000, timeoutMsg: `selection did not settle on ${indices.join(",")}` });
}

async function pickVisibleOption(browser, label) {
  const options = await browser.$$(".qs-menu .qs-option");
  for (const option of options) {
    if ((await option.getText()).trim() === label) {
      await option.click();
      return;
    }
  }
  fail(`no open query-sheet option read ${JSON.stringify(label)}`);
}

async function sentenceText(browser) {
  const sentence = await browser.$(".qs-sentence");
  await sentence.waitForExist({ timeout: 15_000 });
  return (await sentence.getText()).trim();
}

async function openSheet(browser) {
  await browser.$(".qs-gear").click();
  try {
    await browser.$(".qs-sheet").waitForExist({ timeout: 10_000 });
  } catch (error) {
    const proof = await browser.execute(() => ({
      sentences: document.querySelectorAll(".qs-sentence").length,
      gears: document.querySelectorAll(".qs-gear").length,
      expanded: document.querySelector(".qs-gear")?.getAttribute("aria-expanded"),
      width: window.innerWidth,
    }));
    throw new Error(`${String(error)}; proof=${JSON.stringify(proof)}`);
  }
}

function fail(message) {
  throw new Error(message);
}

await withApp(0, async (browser) => {
  await openPage(browser, "Sheet");

  // --- 1. at rest: one sentence, one count, one ⚙ ---------------------------
  const resting = await sentenceText(browser);
  if (!/^Blocks where\b/.test(resting)) fail(`the resting line is not a sentence: ${JSON.stringify(resting)}`);
  if (!/task: TODO/.test(resting)) fail(`the sentence does not say what the query says: ${JSON.stringify(resting)}`);
  const restingShape = await browser.execute(() => ({
    sentences: document.querySelectorAll(".qs-sentence").length,
    sheets: document.querySelectorAll(".qs-sheet").length,
    counts: document.querySelectorAll(".qs-count-slot").length,
    gears: document.querySelectorAll(".qs-gear").length,
  }));
  if (restingShape.sheets !== 0) fail(`a sheet was open at rest: ${JSON.stringify(restingShape)}`);
  if (restingShape.sentences !== 2 || restingShape.gears !== 2) {
    fail(`expected one sentence and one ⚙ per query block: ${JSON.stringify(restingShape)}`);
  }

  // --- 2. the sheet opens OVER the following blocks --------------------------
  await openSheet(browser);
  const geometry = await browser.execute(() => {
    const sheet = document.querySelector(".qs-sheet");
    const overlay = document.querySelector(".qs-overlay");
    const rect = sheet.getBoundingClientRect();
    // What is painted at the sheet's own centre must be the sheet, not a block
    // that the query box clipped it behind.
    const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + 8);
    // The trap this portal exists to escape: any ancestor with a `transform`
    // becomes the containing block for `position: fixed`, so the sheet would be
    // laid out inside the query box instead of over the page.
    let trapped = null;
    for (let el = sheet.parentElement; el && el !== document.documentElement; el = el.parentElement) {
      const transform = getComputedStyle(el).transform;
      if (transform && transform !== "none") {
        trapped = el.className || el.tagName;
        break;
      }
    }
    return {
      inQueryBlock: !!sheet.closest(".query-block"),
      inBody: document.body.contains(sheet),
      trapped,
      overlay: !!overlay,
      width: Math.round(rect.width),
      height: Math.round(rect.height),
      topmost: !!hit && (hit === sheet || sheet.contains(hit)),
      anchor: document.querySelector(".qs-anchor-button")?.textContent?.trim(),
      rows: document.querySelectorAll(".qs-sheet .qs-row").length,
    };
  });
  if (geometry.inQueryBlock) fail("the sheet rendered inside .query-block, where translateZ(0) traps it");
  if (!geometry.inBody) fail(`the sheet is not in the document: ${JSON.stringify(geometry)}`);
  if (geometry.trapped) fail(`a transformed ancestor is the sheet's containing block: ${JSON.stringify(geometry)}`);
  if (!geometry.overlay) fail("the sheet opened without its scrim");
  if (geometry.width < 200 || geometry.height < 60) fail(`the sheet has no size: ${JSON.stringify(geometry)}`);
  if (!geometry.topmost) fail(`something painted over the sheet: ${JSON.stringify(geometry)}`);
  if (geometry.anchor !== "blocks ▾") fail(`the anchor line does not read as the subject: ${JSON.stringify(geometry)}`);
  if (geometry.rows !== 1) fail(`expected one row for one condition, got ${geometry.rows}`);

  // --- 3. add a condition; Rust prints it; the file stays Logseq-readable ----
  // Real driver clicks, deliberately: the sheet is portalled out of the block,
  // and Solid still delivers its delegated `mousedown` to the block that owns
  // it logically — so a press on a control inside the sheet used to start
  // EDITING the block and unmount the sheet mid-gesture. Only a real pointer
  // sequence exercises that (`src/editor/editTargets.test.tsx` pins the rule).
  const clickIn = async (selector, text) => {
    const elements = await browser.$$(selector);
    for (const element of elements) {
      if (text == null || (await element.getText()).trim() === text) {
        await element.click();
        return;
      }
    }
    fail(`nothing to click for ${selector}${text ? ` = ${text}` : ""}`);
  };

  await clickIn(".qs-add");
  try {
    await browser.$(".qs-menu.qs-vocab").waitForExist({ timeout: 5_000 });
  } catch (error) {
    const proof = await browser.execute(() => ({
      expanded: document.querySelector(".qs-add")?.getAttribute("aria-expanded"),
      menus: document.querySelectorAll(".qs-menu").length,
      sheet: document.querySelector(".qs-sheet")?.outerHTML.slice(0, 1200),
    }));
    throw new Error(`${String(error)}; proof=${JSON.stringify(proof)}`);
  }
  // Full-text search, deliberately: it is a condition Logseq's own DSL CAN
  // spell (a bare string is its substring test), so the block must stay a
  // `{{query}}`. A presence condition like `Scheduled` has no OG spelling and
  // is supposed to cross to `{{tine-query}}` — that crossing has its own
  // notice and its own tests; this step is about the ordinary case staying
  // ordinary.
  //
  // **P4 migrated this selector.** The chooser is the one VOCABULARY picker
  // now: its rows carry the observed type and the count under the label, so an
  // exact-text match on the button no longer identifies a row, and the list is
  // virtualized, so a row further down may not be mounted at all. Narrow with
  // the filter the user has, then press the row by the field it names.
  const filter = await browser.$(".qs-menu.qs-vocab .qs-menu-filter");
  await filter.waitForExist({ timeout: 5_000 });
  await filter.click();
  // Through the production input handler: WebKitWebDriver under Xvfb drops the
  // odd character out of a synthetic key sequence, and a filter that received
  // `Fulltext` narrows to nothing and looks exactly like a broken picker.
  await browser.execute(() => {
    const el = document.querySelector(".qs-menu.qs-vocab .qs-menu-filter");
    el.value = "full-text";
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
  const contentRow = await browser.$('.qs-vocab-option[data-vocabulary-key="content"]');
  if (!(await contentRow.isExisting())) {
    const shown = await browser.execute(() => ({
      needle: document.querySelector(".qs-menu.qs-vocab .qs-menu-filter")?.value,
      keys: [...document.querySelectorAll(".qs-vocab-option")].map((el) => el.getAttribute("data-vocabulary-key")),
    }));
    fail(`the full-text field was not in the narrowed list: ${JSON.stringify(shown)}`);
  }
  // The list is the graph's, so the row says so: a built-in carries no count,
  // because the registry holds no statistics for one and inventing a number
  // the engine never said is the thing this picker exists not to do.
  const builtinRow = await contentRow.getText();
  if (/\d+\s+blocks?/.test(builtinRow)) {
    fail(`a built-in row carries a fabricated count: ${JSON.stringify(builtinRow)}`);
  }
  await contentRow.click();
  const value = await browser.$(".qs-sheet .qs-value-editor .qs-input");
  await value.waitForExist({ timeout: 5_000 });
  await value.click();
  await browser.keys("alpha".split(""));
  await browser.keys(["Enter"]);
  await browser.waitUntil(
    async () => (await browser.$$(".qs-sheet .qs-row")).length === 2,
    { timeout: 10_000, timeoutMsg: "the added condition never became a row" },
  );

  await browser.waitUntil(async () => {
    const text = fs.readFileSync(`${GRAPH}/pages/Sheet.md`, "utf8");
    return /alpha/i.test(text.split("\n")[0]);
  }, { timeout: 15_000, timeoutMsg: "the edit never reached the file" });

  const saved = fs.readFileSync(`${GRAPH}/pages/Sheet.md`, "utf8").split("\n");
  if (!/^- \{\{query /.test(saved[0])) fail(`the sheet stopped writing a Logseq query macro: ${JSON.stringify(saved[0])}`);
  if (saved[1] !== UNTOUCHED) {
    fail(`the untouched query block changed on disk (I-4):\n  before: ${JSON.stringify(UNTOUCHED)}\n  after:  ${JSON.stringify(saved[1])}`);
  }

  // --- 4. Escape peels ONE rung at a time, and the sentence followed --------
  // Committing a value reopens the field chooser for the next condition
  // (design §2.9), so at this moment the ladder is two rungs deep: chooser
  // over sheet. One Escape must take exactly one — collapsing both on a single
  // press is the GH #472 failure this app has a single dismissal stack to
  // prevent.
  //
  // Give the keyboard a home first: WebKitWebDriver delivers `browser.keys` to
  // the focused element, and after a commit the focus has fallen back to
  // <body>, from which this webview dispatches nothing — an unfocused Escape
  // would prove neither direction. Focusing a control inside the sheet makes
  // it a real key event travelling the real path: out of the portal, up to the
  // window-capture handler, into the transient stack.
  const escape = async () => {
    await browser.execute(() => document.querySelector(".qs-sheet button")?.focus());
    await browser.keys(["Escape"]);
    await sleep(350);
  };
  await escape();
  const oneRung = await browser.execute(() => ({
    menus: document.querySelectorAll(".qs-menu, .qs-options").length,
    sheets: document.querySelectorAll(".qs-sheet").length,
  }));
  if (oneRung.sheets !== 1) fail(`one Escape closed the sheet under its own open menu: ${JSON.stringify(oneRung)}`);
  if (oneRung.menus !== 0) fail(`the first Escape left the chooser open: ${JSON.stringify(oneRung)}`);
  await escape();
  await browser.$(".qs-sheet").waitForExist({ reverse: true, timeout: 5_000 });
  const after = await sentenceText(browser);
  if (!/alpha/i.test(after)) fail(`the resting sentence did not follow the sheet: ${JSON.stringify(after)}`);

  // --- 5. at the app's narrowest, the sheet still fits the viewport ---------
  // The `max-width: 600px` bottom-sheet rule cannot be reached here: the
  // desktop window declares `minWidth: 640` (src-tauri/tauri.conf.json), so
  // asking for 420 gets 640 back. That rule is shot at 560px by
  // `scripts/shot-query-sheet.mjs` and pinned in `src/mobileSafeArea.test.ts`.
  // What IS a real desktop guarantee is this: at the narrowest window the
  // product allows, a sheet positioned from the sentence's rect must not hang
  // off either edge.
  await browser.setWindowSize(640, 700);
  await sleep(600);
  await openSheet(browser);
  const narrow = await browser.execute(() => {
    const rect = document.querySelector(".qs-sheet").getBoundingClientRect();
    return {
      left: Math.round(rect.left),
      right: Math.round(window.innerWidth - rect.right),
      top: Math.round(rect.top),
      width: Math.round(rect.width),
      innerWidth: window.innerWidth,
      innerHeight: window.innerHeight,
    };
  });
  if (narrow.left < 0 || narrow.right < 0) fail(`the sheet hangs off the viewport: ${JSON.stringify(narrow)}`);
  if (narrow.top < 0) fail(`the sheet starts above the viewport: ${JSON.stringify(narrow)}`);
  if (narrow.width < 300) fail(`the sheet collapsed at the narrowest window: ${JSON.stringify(narrow)}`);
  await escape();
  await browser.$(".qs-sheet").waitForExist({ reverse: true, timeout: 5_000 });
  await browser.setWindowSize(1280, 900);
  await sleep(400);

  // --- 6. P6 controls: state, reorder, cancellation and grouping -------------
  await openPage(browser, "P6 controls");
  await openSheet(browser);
  const initialP6 = p6Lines()[0];
  const flat = await waitForP6Root(
    browser,
    (items) => items.map((item) => item.identity).join(",") === "alpha,beta,gamma,delta",
    "the P6 fixture did not reopen as four distinct siblings",
  );

  // Selection answers "which rows?" and must neither toggle Off nor save.
  const firstRow = ".qs-sheet > .qs-rows > .qs-row[data-qs-parent=\"\"][data-row-index=\"0\"]";
  const firstSelect = await browser.$(`${firstRow} .qs-select`);
  const firstEnabled = await browser.$(`${firstRow} .qs-enabled`);
  if ((await firstEnabled.getAttribute("aria-checked")) !== "true") fail("the initially enabled alpha row did not expose its own enabled state");
  await firstSelect.click();
  if (!(await firstSelect.isSelected())) fail("the alpha selection checkbox did not select alpha");
  if ((await firstEnabled.getAttribute("aria-checked")) !== "true") fail("selecting alpha also disabled it");
  if (p6Lines()[0] !== initialP6) fail("selection alone wrote the query file");

  // The separate enabled switch is a real engine save, and one ordinary undo
  // restores it. Root replacement must clear the selection that named the old
  // revision rather than silently moving that selection to a different row.
  await firstEnabled.click();
  await waitForP6Source(browser, (line) => line === disabledAlphaP6, "disabling alpha did not persist as the engine's TQL Off form");
  const disableLifecycle = await settleP6SheetAfterSourceChange(
    browser,
    (items) => items.map((item) => item.identity).join(",") === "alpha,beta,gamma,delta"
      && items[0]?.ownEnabled === "false",
    "the saved TQL root did not expose disabled alpha",
  );
  const disabled = await browser.$(`${firstRow} .qs-enabled`);
  if ((await disabled.getAttribute("aria-checked")) !== "false") fail("alpha's own enabled switch did not read disabled after save");
  if ((await browser.$$(".qs-sheet .qs-select:checked")).length !== 0) fail("a saved root replacement retained a stale selection");
  await browser.keys(["Control", "z"]);
  await waitForP6Source(browser, (line) => line === initialP6, "one Ctrl+Z did not restore the enabled query bytes");
  const undoLifecycle = await settleP6SheetAfterSourceChange(
    browser,
    (items) => items.map((item) => item.identity).join(",") === "alpha,beta,gamma,delta"
      && items[0]?.ownEnabled === "true",
    "undo did not restore the enabled flat P6 rows",
  );

  // The handle's arrow operation is the accessible equivalent of drag. Focus
  // follows the MOVED alpha row, so the second Down continues from its new
  // position and yields beta,gamma,alpha,delta rather than moving beta.
  const firstHandle = await browser.$(`${firstRow} .qs-drag-handle`);
  await firstHandle.click();
  await browser.keys(["ArrowDown"]);
  await waitForP6Root(browser, (items) => items.map((item) => item.identity).join(",") === "beta,alpha,gamma,delta", "the first keyboard reorder did not move alpha down once");
  const afterFirstMoveFocus = await browser.execute(() => ({
    handle: document.activeElement?.getAttribute("data-qs-handle") ?? null,
    row: document.activeElement?.closest(".qs-row")?.textContent?.toLowerCase() ?? "",
  }));
  if (afterFirstMoveFocus.handle !== "1" || !afterFirstMoveFocus.row.includes("alpha")) {
    fail(`focus did not follow alpha after the first keyboard move: ${JSON.stringify(afterFirstMoveFocus)}`);
  }
  await browser.keys(["ArrowDown"]);
  await waitForP6Root(browser, (items) => items.map((item) => item.identity).join(",") === "beta,gamma,alpha,delta", "the second keyboard reorder did not continue from alpha's new position");
  const afterSecondMoveFocus = await browser.execute(() => ({
    handle: document.activeElement?.getAttribute("data-qs-handle") ?? null,
    row: document.activeElement?.closest(".qs-row")?.textContent?.toLowerCase() ?? "",
  }));
  if (afterSecondMoveFocus.handle !== "2" || !afterSecondMoveFocus.row.includes("alpha")) {
    fail(`focus did not follow alpha after the second keyboard move: ${JSON.stringify(afterSecondMoveFocus)}`);
  }
  const reorderedP6 = await waitForP6Source(
    browser,
    (line) => /\(and\s+"beta"\s+"gamma"\s+"alpha"\s+"delta"\)/i.test(line),
    "the two keyboard reorders never reached the file in visible order",
  );

  // Start a real pointer drag far enough to show an insertion indicator, then
  // press Escape while the pointer remains down. Releasing afterwards must not
  // apply the previewed move, and Escape must peel the drag without closing its
  // parent sheet.
  const dragPoints = await browser.execute(() => {
    const rows = [...document.querySelectorAll('.qs-sheet > .qs-rows > .qs-row[data-qs-parent=""]')];
    const handle = rows[0]?.querySelector(".qs-drag-handle");
    const target = rows[rows.length - 1];
    if (!(handle instanceof HTMLElement) || !(target instanceof HTMLElement)) return null;
    const start = handle.getBoundingClientRect();
    const end = target.getBoundingClientRect();
    handle.focus();
    return {
      start: { x: start.left + start.width / 2, y: start.top + start.height / 2 },
      end: { x: end.left + Math.min(30, end.width / 2), y: end.top + end.height * 0.75 },
    };
  });
  if (!dragPoints) fail("the native sheet did not expose drag coordinates");
  await browser.performActions([{
    type: "pointer",
    id: "p6-cancel-mouse",
    parameters: { pointerType: "mouse" },
    actions: [
      { type: "pointerMove", duration: 0, origin: "viewport", x: Math.round(dragPoints.start.x), y: Math.round(dragPoints.start.y) },
      { type: "pointerDown", button: 0 },
      { type: "pointerMove", duration: 80, origin: "viewport", x: Math.round(dragPoints.end.x), y: Math.round(dragPoints.end.y) },
    ],
  }]);
  try {
    await browser.waitUntil(
      () => browser.execute(() => !!document.querySelector(".qs-drop-before, .qs-drop-after")),
      { timeout: 5_000, timeoutMsg: "a threshold-crossing pointer drag never exposed an insertion position" },
    );
    await browser.keys(["Escape"]);
  } finally {
    await browser.releaseActions();
  }
  await browser.waitUntil(
    () => browser.execute(() => !document.querySelector(".qs-drop-before, .qs-drop-after")),
    { timeout: 5_000, timeoutMsg: "Escape left the drag insertion position active" },
  );
  if (!(await browser.$(".qs-sheet").isExisting())) fail("Escape cancelled the drag and its parent sheet together");
  if (p6Lines()[0] !== reorderedP6) fail("Escape-cancelled drag wrote the query file");
  await waitForP6Root(browser, (items) => items.map((item) => item.identity).join(",") === "beta,gamma,alpha,delta", "Escape-cancelled drag changed row order");

  // The compact per-row route shares the grouping owner. Group alpha with the
  // row above, observe one nested `any of`, then undo back to the same flat
  // revision before exercising multi-selection.
  await (await browser.$('.qs-sheet > .qs-rows > .qs-row[data-qs-parent=""][data-row-index="2"] .qs-row-menu')).click();
  await pickVisibleOption(browser, "Group with row above — any of");
  await waitForP6Root(browser, (items) =>
    items.length === 3 && items[1]?.group === "any of" && items[1]?.children.join(",") === "gamma,alpha",
  "group-with-row-above did not retain gamma,alpha as one any-of subtree");
  await waitForP6Source(browser, (line) => /\(or\s+"gamma"\s+"alpha"\)/i.test(line), "group-with-row-above never reached the file");
  await browser.keys(["Control", "z"]);
  await waitForP6Source(browser, (line) => line === reorderedP6, "undo did not restore the pre-group row order");
  await waitForP6Root(browser, (items) => items.map((item) => item.identity).join(",") === "beta,gamma,alpha,delta", "undo did not flatten group-with-row-above");

  const groupSelected = async ({ label, header, indices, children, source, keep = false }) => {
    await selectP6Rows(browser, indices);
    await (await browser.$(".qs-group-selected")).click();
    await pickVisibleOption(browser, label);
    const grouped = await waitForP6Root(browser, (items) =>
      items.some((item) => item.group === header && item.children.join(",") === children.join(",")),
    `${label} did not group the selected siblings in their original order`);
    await waitForP6Source(browser, (line) => source.test(line), `${label} did not persist through the engine printer`);
    if (!keep) {
      await browser.keys(["Control", "z"]);
      await waitForP6Source(browser, (line) => line === reorderedP6, `undo did not restore the flat query after ${label}`);
      await waitForP6Root(browser, (items) => items.map((item) => item.identity).join(",") === "beta,gamma,alpha,delta", `undo did not flatten ${label}`);
    }
    return grouped;
  };

  const allGrouped = await groupSelected({
    label: "All of", header: "all of", indices: [0, 2], children: ["beta", "alpha"],
    source: /\(and\s+\(and\s+"beta"\s+"alpha"\)\s+"gamma"\s+"delta"\)/i,
  });
  const anyGrouped = await groupSelected({
    label: "Any of", header: "any of", indices: [1, 3], children: ["gamma", "delta"],
    source: /\(and\s+"beta"\s+\(or\s+"gamma"\s+"delta"\)\s+"alpha"\)/i,
  });
  const noneGrouped = await groupSelected({
    label: "None of", header: "none of", indices: [1, 3], children: ["gamma", "delta"],
    source: /\(and\s+"beta"\s+\(not\s+\(or\s+"gamma"\s+"delta"\)\)\s+"alpha"\)/i,
    keep: true,
  });
  // Disable the group itself. Its switch owns that Off; its children remain
  // individually on while truthfully saying that the disabled ancestor keeps
  // them from running. Keep this nested state for restart and 390px geometry.
  const finalGroup = '.qs-sheet > .qs-rows > .qs-listitem[data-qs-parent=""][data-row-index="1"] > .qs-group';
  await (await browser.$(`${finalGroup} > .qs-group-header .qs-enabled`)).click();
  await waitForP6Source(
    browser,
    (line) => line === savedDisabledGroupP6,
    "disabling the final none-of group never reached the file",
  );
  const finalDisableLifecycle = await settleP6SheetAfterSourceChange(
    browser,
    (items) => items.length === 3
      && items[0]?.identity === "beta"
      && items[1]?.group === "none of"
      && items[1]?.ownEnabled === "false"
      && items[1]?.children.join(",") === "gamma,delta"
      && items[2]?.identity === "alpha",
    "the saved TQL root did not expose the disabled none-of group",
  );
  const disabledGroupState = await browser.execute((selector) => {
    const group = document.querySelector(selector);
    return {
      own: group?.querySelector(":scope > .qs-group-header .qs-enabled")?.getAttribute("aria-checked") ?? null,
      children: [...(group?.querySelectorAll(":scope > .qs-rows > .qs-row .qs-enabled") ?? [])].map((node) => node.getAttribute("aria-checked")),
      inherited: [...(group?.querySelectorAll(":scope > .qs-rows > .qs-row .qs-off-label") ?? [])].map((node) => node.textContent?.trim()),
    };
  }, finalGroup);
  if (disabledGroupState.own !== "false"
      || disabledGroupState.children.join(",") !== "true,true"
      || disabledGroupState.inherited.join(",") !== "disabled by group,disabled by group") {
    fail(`own and inherited Off states were not distinguished: ${JSON.stringify(disabledGroupState)}`);
  }
  const savedP6 = p6Lines()[0];
  await browser.keys(["Escape"]);
  await browser.$(".qs-sheet").waitForExist({ reverse: true, timeout: 5_000 });

  // --- 7. a hostile depth stays a short line and a small sheet (I-22) -------
  await openPage(browser, "Deep");
  const deep = await sentenceText(browser);
  if (deep.length > 200) fail(`a 20-deep query drew a ${deep.length}-character sentence`);
  if (!deep.includes("⟨advanced⟩")) fail(`the deep subtree was not folded into one chip: ${JSON.stringify(deep)}`);
  await openSheet(browser);
  const bounded = await browser.execute(() => ({
    rows: document.querySelectorAll(".qs-sheet .qs-row").length,
    groups: document.querySelectorAll(".qs-sheet .qs-group").length,
    chips: document.querySelectorAll(".qs-sheet .qs-row-advanced").length,
  }));
  if (bounded.rows > 8 || bounded.groups > 3) fail(`the sheet grew with the nesting: ${JSON.stringify(bounded)}`);
  if (bounded.chips < 1) fail("the folded subtree has no ⟨advanced⟩ row to edit or remove");
  await browser.keys(["Escape"]);
  console.log(`sheet: resting=${JSON.stringify(resting)} after=${JSON.stringify(after)} narrow=${JSON.stringify(narrow)} p6=${JSON.stringify({ flat, allGrouped, anyGrouped, noneGrouped, savedP6, lifecycle: { disableLifecycle, undoLifecycle, finalDisableLifecycle } })} deep=${JSON.stringify(bounded)}`);
});

// --- 8. restart: what landed on disk still says what the sheet said ---------
await withApp(1, async (browser) => {
  await openPage(browser, "Sheet");
  const reopened = await sentenceText(browser);
  if (!/task: TODO/.test(reopened) || !/alpha/i.test(reopened)) {
    fail(`the saved query did not reopen as the same sentence: ${JSON.stringify(reopened)}`);
  }
  await openSheet(browser);
  const rows = (await browser.$$(".qs-sheet .qs-row")).length;
  if (rows !== 2) fail(`the saved query reopened with ${rows} rows, not 2`);
  await browser.keys(["Escape"]);

  await openPage(browser, "P6 controls");
  await openSheet(browser);
  const reopenedP6 = await waitForP6Root(browser, (items) =>
    items.length === 3
      && items[0]?.identity === "beta"
      && items[1]?.group === "none of"
      && items[1]?.children.join(",") === "gamma,delta"
      && items[2]?.identity === "alpha",
  "the saved P6 query did not reopen as beta / none-of(gamma,delta) / alpha");
  await waitForP6Source(
    browser,
    (line) => line === savedDisabledGroupP6,
    "the reopened P6 structure was absent from the file",
  );
  const reopenedDisabled = await browser.execute(() => ({
    groupOwn: document.querySelector('.qs-sheet > .qs-rows > .qs-listitem[data-qs-parent=""][data-row-index="1"] > .qs-group > .qs-group-header .qs-enabled')?.getAttribute("aria-checked") ?? null,
    inherited: [...document.querySelectorAll('.qs-sheet > .qs-rows > .qs-listitem[data-qs-parent=""][data-row-index="1"] > .qs-group > .qs-rows .qs-off-label')].map((node) => node.textContent?.trim()),
  }));
  if (reopenedDisabled.groupOwn !== "false" || reopenedDisabled.inherited.join(",") !== "disabled by group,disabled by group") {
    fail(`the reopened query lost own/inherited Off state: ${JSON.stringify(reopenedDisabled)}`);
  }
  console.log(`reopened: ${JSON.stringify(reopened)} rows=${rows} p6=${JSON.stringify(reopenedP6)}`);
});

// --- 9. true native 390px: reachable P6 targets and no horizontal escape ---
// This uses the app's Rust-owned E2E window policy, the same one as the native
// mobile-drawer journey. `setWindowSize(390, …)` cannot prove this because the
// ordinary desktop config clamps its minimum width to 640px.
await withApp(2, async (browser) => {
  // A restored phone-width session can start with a drawer over the workspace.
  // Close it through its actual visible control before touching the query.
  const drawerClose = await browser.$(".mobile-drawer-close, .rs-close");
  if (await drawerClose.isExisting()) {
    await drawerClose.click();
    await browser.waitUntil(
      () => browser.execute(() => !document.querySelector(".mobile-drawer-scrim")),
      { timeout: 5_000, timeoutMsg: "the restored mobile drawer did not close" },
    );
  }
  await openPage(browser, "P6 controls");
  await openSheet(browser);
  await selectP6Rows(browser, [0]);

  const rowGeometry = await browser.execute(() => {
    const row = document.querySelector('.qs-sheet > .qs-rows > [data-qs-parent=""][data-row-index="0"]');
    const sheet = document.querySelector(".qs-sheet");
    if (!(row instanceof HTMLElement) || !(sheet instanceof HTMLElement)) return null;
    row.scrollIntoView({ block: "nearest", inline: "nearest" });
    const controls = [
      ["drag", row.querySelector(".qs-drag-handle")],
      ["select", row.querySelector(".qs-select")?.closest("label") ?? row.querySelector(".qs-select")],
      ["enabled", row.querySelector(".qs-enabled")],
    ];
    const box = (name, element) => {
      if (!(element instanceof HTMLElement)) return { name, missing: true };
      const rect = element.getBoundingClientRect();
      const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
      return {
        name,
        left: rect.left,
        right: rect.right,
        top: rect.top,
        bottom: rect.bottom,
        width: rect.width,
        height: rect.height,
        hit: !!hit && (hit === element || element.contains(hit)),
      };
    };
    const targets = controls.map(([name, element]) => box(name, element));
    const overlaps = [];
    for (let a = 0; a < targets.length; a += 1) {
      for (let b = a + 1; b < targets.length; b += 1) {
        const left = Math.max(targets[a].left ?? 0, targets[b].left ?? 0);
        const right = Math.min(targets[a].right ?? 0, targets[b].right ?? 0);
        const top = Math.max(targets[a].top ?? 0, targets[b].top ?? 0);
        const bottom = Math.min(targets[a].bottom ?? 0, targets[b].bottom ?? 0);
        if (right - left > 1 && bottom - top > 1) overlaps.push(`${targets[a].name}/${targets[b].name}`);
      }
    }
    const sheetRect = sheet.getBoundingClientRect();
    return {
      viewport: { width: innerWidth, height: innerHeight },
      sheet: {
        left: sheetRect.left,
        right: sheetRect.right,
        width: sheetRect.width,
        clientWidth: sheet.clientWidth,
        scrollWidth: sheet.scrollWidth,
      },
      row: { clientWidth: row.clientWidth, scrollWidth: row.scrollWidth },
      targets,
      overlaps,
    };
  });
  if (!rowGeometry) fail("the 390px native sheet had no first P6 row to measure");
  if (rowGeometry.viewport.width < 370 || rowGeometry.viewport.width > 410) {
    fail(`the native narrow journey missed the bounded 390px viewport: ${JSON.stringify(rowGeometry.viewport)}`);
  }
  if (rowGeometry.sheet.left < -1 || rowGeometry.sheet.right > rowGeometry.viewport.width + 1
      || rowGeometry.sheet.scrollWidth > rowGeometry.sheet.clientWidth + 1
      || rowGeometry.row.scrollWidth > rowGeometry.row.clientWidth + 1) {
    fail(`the P6 sheet or row overflowed horizontally at 390px: ${JSON.stringify(rowGeometry)}`);
  }
  for (const target of rowGeometry.targets) {
    if (target.missing || target.width < 43.5 || target.height < 43.5 || !target.hit
        || target.left < -1 || target.right > rowGeometry.viewport.width + 1) {
      fail(`the ${target.name} control was not a reachable 44px target at 390px: ${JSON.stringify(rowGeometry)}`);
    }
  }
  if (rowGeometry.overlaps.length) fail(`P6 row hit targets overlap at 390px: ${JSON.stringify(rowGeometry)}`);

  const groupGeometry = await browser.execute(() => {
    const group = document.querySelector('.qs-sheet > .qs-rows > .qs-listitem[data-qs-parent=""][data-row-index="1"] > .qs-group');
    const header = group?.querySelector(":scope > .qs-group-header");
    if (!(group instanceof HTMLElement) || !(header instanceof HTMLElement)) return null;
    header.scrollIntoView({ block: "nearest", inline: "nearest" });
    const elements = [
      ["group drag", header.querySelector(".qs-drag-handle")],
      ["group select", header.querySelector(".qs-select")?.closest("label") ?? header.querySelector(".qs-select")],
      ["group enabled", header.querySelector(".qs-enabled")],
    ];
    const targets = elements.map(([name, element]) => {
      if (!(element instanceof HTMLElement)) return { name, missing: true };
      const rect = element.getBoundingClientRect();
      const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
      return {
        name, left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom,
        width: rect.width, height: rect.height,
        hit: !!hit && (hit === element || element.contains(hit)),
      };
    });
    const overlaps = [];
    for (let a = 0; a < targets.length; a += 1) {
      for (let b = a + 1; b < targets.length; b += 1) {
        const left = Math.max(targets[a].left ?? 0, targets[b].left ?? 0);
        const right = Math.min(targets[a].right ?? 0, targets[b].right ?? 0);
        const top = Math.max(targets[a].top ?? 0, targets[b].top ?? 0);
        const bottom = Math.min(targets[a].bottom ?? 0, targets[b].bottom ?? 0);
        if (right - left > 1 && bottom - top > 1) overlaps.push(`${targets[a].name}/${targets[b].name}`);
      }
    }
    const rect = header.getBoundingClientRect();
    return {
      header: { left: rect.left, right: rect.right, clientWidth: header.clientWidth, scrollWidth: header.scrollWidth },
      group: { clientWidth: group.clientWidth, scrollWidth: group.scrollWidth },
      targets,
      overlaps,
      inheritedLabels: [...group.querySelectorAll(".qs-off-label")].map((node) => node.textContent?.trim()),
    };
  });
  if (!groupGeometry
      || groupGeometry.header.left < -1 || groupGeometry.header.right > rowGeometry.viewport.width + 1
      || groupGeometry.header.scrollWidth > groupGeometry.header.clientWidth + 1
      || groupGeometry.group.scrollWidth > groupGeometry.group.clientWidth + 1) {
    fail(`the disabled nested group/header overflowed at 390px: ${JSON.stringify(groupGeometry)}`);
  }
  for (const target of groupGeometry.targets) {
    if (target.missing || target.width < 43.5 || target.height < 43.5 || !target.hit
        || target.left < -1 || target.right > rowGeometry.viewport.width + 1) {
      fail(`the ${target.name} target was not reachable at 390px: ${JSON.stringify(groupGeometry)}`);
    }
  }
  if (groupGeometry.overlaps.length) fail(`nested-group hit targets overlap at 390px: ${JSON.stringify(groupGeometry)}`);
  if (groupGeometry.inheritedLabels.filter((label) => label === "disabled by group").length !== 2) {
    fail(`the narrow nested group hid its inherited disabled state: ${JSON.stringify(groupGeometry)}`);
  }

  const selectionGeometry = await browser.execute(() => {
    const bar = document.querySelector(".qs-selection");
    if (!(bar instanceof HTMLElement)) return null;
    bar.scrollIntoView({ block: "nearest", inline: "nearest" });
    return [...bar.querySelectorAll("button")].map((button) => {
      const rect = button.getBoundingClientRect();
      const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
      return {
        label: button.textContent?.trim(),
        width: rect.width,
        height: rect.height,
        left: rect.left,
        right: rect.right,
        hit: !!hit && (hit === button || button.contains(hit)),
      };
    });
  });
  if (!selectionGeometry || selectionGeometry.length !== 2) fail(`the 390px selection bar did not expose Group selected and Clear: ${JSON.stringify(selectionGeometry)}`);
  for (const target of selectionGeometry) {
    if (target.width < 43.5 || target.height < 43.5 || !target.hit
        || target.left < -1 || target.right > rowGeometry.viewport.width + 1) {
      fail(`the selection-bar control was not reachable at 390px: ${JSON.stringify(selectionGeometry)}`);
    }
  }
  if (p6Lines()[1] !== P6_UNTOUCHED) fail("the narrow P6 inspection changed the unrelated query block");
  console.log(`p6 narrow390=${JSON.stringify({ rowGeometry, groupGeometry, selectionGeometry })}`);
}, { forceMobile: true });

console.log("query-sheet OK");
