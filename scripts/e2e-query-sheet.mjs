// Linux real-WebKit journey for the two states of the visual query builder
// (SPEC §7.2–§7.4): a `{{query}}` block RESTS as one plain-English sentence and
// EXPANDS into a sheet of rows, and what the sheet writes is still an ordinary
// Logseq-readable query block.
//
// Two things need a real browser rather than jsdom.
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
import { spawn } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { tauriCapabilities, webdriverServerArgs } from "./e2e-capabilities.mjs";

await ensureDisplay();

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APP = process.env.TINE_APP || path.join(ROOT, "target/release/tine");
const TD = process.env.TAURI_DRIVER || (process.env.CARGO_HOME ? path.join(process.env.CARGO_HOME, "bin", "tauri-driver") : "tauri-driver");
const DRIVER_BASE = Number(process.env.E2E_DRIVER_PORT || 4496);
const NATIVE_BASE = Number(process.env.E2E_NATIVE_PORT || 4497);
const TMP = "/tmp/tine-query-sheet-e2e";
const GRAPH = `${TMP}/graph`;

fs.rmSync(TMP, { recursive: true, force: true });
for (const dir of ["pages", "journals", "logseq"]) fs.mkdirSync(`${GRAPH}/${dir}`, { recursive: true });
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(`${TMP}/xdg/${dir}`, { recursive: true });
fs.writeFileSync(`${GRAPH}/logseq/config.edn`, "{}\n");
const now = new Date();
const journal = `${now.getFullYear()}_${String(now.getMonth() + 1).padStart(2, "0")}_${String(now.getDate()).padStart(2, "0")}`;
fs.writeFileSync(`${GRAPH}/journals/${journal}.md`, "- Open [[Sheet]] and [[Deep]]\n");

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

async function withApp(index, fn) {
  const driverPort = DRIVER_BASE + index * 2;
  const nativePort = NATIVE_BASE + index * 2;
  const log = fs.openSync(`${TMP}/tauri-driver-${index}.log`, "w");
  const td = spawn(TD, webdriverServerArgs(driverPort, nativePort, process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver"), {
    env, stdio: ["ignore", log, log], detached: true,
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

/** Navigate the way a user does — click the page reference on today's journal.
 *  No test-only navigation hook, so this journey cannot pass through a door the
 *  product does not have. */
async function openPage(browser, title) {
  await browser.$(".ls-block, .page-title").waitForExist({ timeout: 20_000 });
  await browser.waitUntil(async () => {
    for (const selector of [`a.page-ref=${title}`, `span.page-ref=${title}`, `*=${title}`]) {
      const link = await browser.$(selector);
      if (await link.isExisting()) { await link.click(); return true; }
    }
    return false;
  }, { timeout: 15_000, timeoutMsg: `never found a link to ${title}` });
  await browser.waitUntil(
    async () => (await browser.$("h1.page-title").getText()).trim() === title,
    { timeout: 10_000, timeoutMsg: `${title} did not open` },
  );
  await sleep(400);
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

  // --- 6. a hostile depth stays a short line and a small sheet (I-22) -------
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
  console.log(`sheet: resting=${JSON.stringify(resting)} after=${JSON.stringify(after)} narrow=${JSON.stringify(narrow)} deep=${JSON.stringify(bounded)}`);
});

// --- 7. restart: what landed on disk still says what the sheet said ---------
await withApp(1, async (browser) => {
  await openPage(browser, "Sheet");
  const reopened = await sentenceText(browser);
  if (!/task: TODO/.test(reopened) || !/alpha/i.test(reopened)) {
    fail(`the saved query did not reopen as the same sentence: ${JSON.stringify(reopened)}`);
  }
  await openSheet(browser);
  const rows = (await browser.$$(".qs-sheet .qs-row")).length;
  if (rows !== 2) fail(`the saved query reopened with ${rows} rows, not 2`);
  console.log(`reopened: ${JSON.stringify(reopened)} rows=${rows}`);
});

console.log("query-sheet OK");
