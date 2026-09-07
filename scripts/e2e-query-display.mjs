// Linux real-WebKit journey for the query block's DISPLAY settings (P5B).
//
// A query block persists six display facts — view, grouping field, sort,
// visible columns, footer totals, sample — and before this the inline surface
// could state one and a half of them. This journey drives the Display panel and
// the query table's own header and footer, and then reads the FILE, because the
// only thing that makes a display setting durable is the bytes it left behind.
//
// Four things need a real engine and a real browser rather than jsdom.
//
//  1. The grouping identity is resolved in RUST. `tine.group-field::` is read by
//     `query::view::resolve_query_grouping` on every parse, so what the app
//     groups by is an answer that travelled through the backend. A jsdom test
//     can only assert the TypeScript adapter agrees with the corpus; only this
//     can show the two ends meeting over a real file.
//  2. Every write here is printed by Rust and re-read by Rust. Restarting the
//     app and finding the same board is the only proof that what landed on disk
//     says what the panel said (I-4).
//  3. The panel is PORTALLED to <body> and positioned from its trigger's rect,
//     for the same `transform: translateZ(0)` reason the sheet is; and its field
//     pickers are portalled again, inside it. jsdom has no layout.
//  4. The retirement of `tine.group-by::` is a two-property edit inside ONE undo
//     unit. Only a real run can show both properties changing together in the
//     file rather than one write racing the other.
import { spawn } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { openPageByName as openPage } from "./lib/e2e-navigation.mjs";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { tauriCapabilities, webdriverServerArgs } from "./e2e-capabilities.mjs";

await ensureDisplay();

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APP = process.env.TINE_APP || path.join(ROOT, "target/release/tine");
const TD = process.env.TAURI_DRIVER || (process.env.CARGO_HOME ? path.join(process.env.CARGO_HOME, "bin", "tauri-driver") : "tauri-driver");
const DRIVER_BASE = Number(process.env.E2E_DRIVER_PORT || 4550);
const NATIVE_BASE = Number(process.env.E2E_NATIVE_PORT || 4551);
const TMP = "/tmp/tine-query-display-e2e";
const GRAPH = `${TMP}/graph`;

fs.rmSync(TMP, { recursive: true, force: true });
for (const dir of ["pages", "journals", "logseq"]) fs.mkdirSync(`${GRAPH}/${dir}`, { recursive: true });
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(`${TMP}/xdg/${dir}`, { recursive: true });
fs.writeFileSync(`${GRAPH}/logseq/config.edn`, "{}\n");
const now = new Date();
const journal = `${now.getFullYear()}_${String(now.getMonth() + 1).padStart(2, "0")}_${String(now.getDate()).padStart(2, "0")}`;
fs.writeFileSync(`${GRAPH}/journals/${journal}.md`, "- Open [[Display]] and [[Legacy]]\n");

// The rows the display settings describe. `owner` and `cost` are ordinary
// properties, so they are what the grouping, the column list and the footer
// total can name; `state` is the task marker, which is a different identity
// with the same kind of spelling — the collision `tine.group-field::` exists to
// end.
const DISPLAY_PAGE = [
  "- {{query (and (task TODO DOING))}}",
  "- TODO alpha task",
  "  owner:: Ada",
  "  cost:: 2",
  "- TODO beta task",
  "  owner:: Bo",
  "  cost:: 3",
  "- DOING gamma task",
  "  owner:: Ada",
  "  cost:: 4",
  "- DONE delta task",
  "  owner:: Bo",
  "",
].join("\n");
fs.writeFileSync(`${GRAPH}/pages/Display.md`, DISPLAY_PAGE);

// An EXISTING note in the old spelling. On a board face a bare `owner` was
// already an ordinary property, so nothing on screen may change when it opens —
// and the first save that states the grouping must replace this key rather than
// leave two answers in the file.
const LEGACY_PAGE = [
  "- {{query (and (task TODO DOING))}}",
  "  tine.view:: board",
  "  tine.group-by:: owner",
  "- TODO alpha task",
  "  owner:: Ada",
  "- TODO beta task",
  "  owner:: Bo",
  "",
].join("\n");
fs.writeFileSync(`${GRAPH}/pages/Legacy.md`, LEGACY_PAGE);

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
      capabilities: tauriCapabilities(APP, "query-display"),
    });
    await browser.$(".ls-block, .page-title").waitForExist({ timeout: 20_000 });
    await fn(browser);
    await sleep(750);
  } catch (error) {
    try {
      await browser?.saveScreenshot(`${TMP}/failure-${index}.png`);
      const dom = await browser?.execute(() => document.body.outerHTML);
      fs.writeFileSync(`${TMP}/failure-${index}.html`, dom ?? "");
    } catch {}
    throw error;
  } finally {
    try { await browser?.deleteSession(); } catch {}
    try { process.kill(-td.pid, "SIGKILL"); } catch {}
    fs.closeSync(log);
  }
}

function fail(message) {
  throw new Error(message);
}

async function openSheet(browser) {
  await browser.$(".qs-gear").waitForExist({ timeout: 15_000 });
  await browser.$(".qs-gear").click();
  await browser.$(".qs-sheet").waitForExist({ timeout: 10_000 });
}

/** The panel lives in the sheet's footer, and a write can remount the sheet
 *  under it — so reopen whatever is shut rather than assuming either is up. */
async function openDisplay(browser) {
  if (!(await browser.$(".qd-trigger").isExisting())) await openSheet(browser);
  const trigger = await browser.$(".qd-trigger");
  await trigger.waitForExist({ timeout: 10_000 });
  if ((await trigger.getAttribute("aria-expanded")) !== "true") await trigger.click();
  await browser.$(".qd-panel").waitForExist({ timeout: 10_000 });
}

async function closeDisplay(browser) {
  if (await browser.$(".qd-panel").isExisting()) await browser.keys("Escape");
  await browser.$(".qd-panel").waitForExist({ reverse: true, timeout: 5_000 });
}

/** Press a button by its exact visible text, inside a container. */
async function press(browser, selector, text) {
  const elements = await browser.$$(selector);
  for (const element of elements) {
    if ((await element.getText()).trim() === text) { await element.click(); return; }
  }
  const seen = [];
  for (const element of elements) seen.push((await element.getText()).trim());
  fail(`no ${selector} reading ${JSON.stringify(text)}; saw ${JSON.stringify(seen)}`);
}

/** Open a field picker from its trigger, narrow it, and take the named field. */
async function pickField(browser, triggerText, key) {
  await press(browser, ".qd-panel .qd-add, .qd-panel .qd-row-btn", triggerText);
  await browser.$(".qd-field-picker .qs-vocab-options").waitForExist({ timeout: 8_000 });
  const option = await browser.$(`.qd-field-picker .qs-vocab-option[data-vocabulary-key="${key}"]`);
  if (!(await option.isExisting())) {
    const keys = await browser.execute(() =>
      [...document.querySelectorAll(".qd-field-picker .qs-vocab-option")].map((el) => el.getAttribute("data-vocabulary-key")));
    fail(`the ${JSON.stringify(triggerText)} picker does not offer ${key}; it offers ${JSON.stringify(keys)}`);
  }
  await option.click();
  await browser.$(".qd-field-picker").waitForExist({ reverse: true, timeout: 5_000 });
}

/** The block's own property lines, as the file has them. */
function properties(page) {
  const lines = fs.readFileSync(`${GRAPH}/pages/${page}.md`, "utf8").split("\n");
  const out = new Map();
  for (const line of lines.slice(1)) {
    if (/^\s*-\s/.test(line)) break; // the next bullet ends this block's properties
    const found = /^\s*([A-Za-z0-9._-]+):: ?(.*)$/.exec(line);
    if (found) out.set(found[1], found[2]);
  }
  return out;
}

async function waitForProperty(browser, page, key, value) {
  try {
    await browser.waitUntil(async () => properties(page).get(key) === value, { timeout: 15_000 });
  } catch {
    // wdio's own message says only that a condition timed out. What is needed
    // here is the FILE: whether the write never happened, landed under another
    // key, or landed in a shape this reader does not recognize.
    const raw = fs.readFileSync(`${GRAPH}/pages/${page}.md`, "utf8");
    fail(
      `${page}: ${key} never became ${JSON.stringify(value)}\n`
        + `  read properties: ${JSON.stringify([...properties(page)])}\n`
        + `  file:\n${raw}`,
    );
  }
}

await withApp(0, async (browser) => {
  // --- 1. the panel is offered, and it replaced the two half-controls --------
  await openPage(browser, "Display");
  await browser.$(".qd-trigger").waitForExist({ timeout: 15_000 });
  await openDisplay(browser);
  if (await browser.$('.qs-sheet[aria-label="Query filter"]').isExisting()) {
    fail("Display unexpectedly opened the filter sheet");
  }
  await closeDisplay(browser);
  await openSheet(browser);
  const controls = await browser.execute(() => ({
    display: document.querySelectorAll(".qd-trigger").length,
    // `+ sort` and `+ summarize` are the pills the panel takes over from; on a
    // face that has the panel they must not ALSO be there, or two controls
    // would write the same property with different ideas of how many entries
    // it can hold.
    pills: [...document.querySelectorAll(".qs-footer button")]
      .map((b) => b.textContent.trim())
      .filter((t) => /^\+ (sort|summarize)$/.test(t)),
    label: document.querySelector(".qd-trigger")?.textContent?.trim(),
  }));
  if (controls.display !== 1) fail(`expected one Display control, got ${JSON.stringify(controls)}`);
  if (controls.pills.length) fail(`the old one-entry pills are still mounted beside the panel: ${JSON.stringify(controls)}`);
  if (!/^display: List/.test(controls.label ?? "")) fail(`the control does not say what it holds: ${JSON.stringify(controls)}`);

  // --- 2. six facts, one panel ----------------------------------------------
  await openDisplay(browser);
  const sections = await browser.execute(() =>
    [...document.querySelectorAll(".qd-panel .qd-section-title")].map((el) => el.textContent.trim()));
  const wanted = ["View", "Group by", "Sort", "Columns", "Summarize", "Sample"];
  if (JSON.stringify(sections) !== JSON.stringify(wanted)) {
    fail(`the panel does not hold the six display facts: ${JSON.stringify(sections)}`);
  }
  // The panel is portalled: a `transform` ancestor would lay it out inside the
  // query box instead of over the page, which is the trap `.query-block`'s
  // translateZ(0) sets for anything `position: fixed`.
  const geometry = await browser.execute(() => {
    const panel = document.querySelector(".qd-panel");
    const rect = panel.getBoundingClientRect();
    let trapped = null;
    for (let el = panel.parentElement; el && el !== document.documentElement; el = el.parentElement) {
      const transform = getComputedStyle(el).transform;
      if (transform && transform !== "none") { trapped = el.className || el.tagName; break; }
    }
    const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + 8);
    return {
      inQueryBlock: !!panel.closest(".query-block"),
      trapped,
      left: Math.round(rect.left),
      right: Math.round(window.innerWidth - rect.right),
      bottom: Math.round(window.innerHeight - rect.bottom),
      width: Math.round(rect.width),
      height: Math.round(rect.height),
      topmost: !!hit && (hit === panel || panel.contains(hit)),
    };
  });
  if (geometry.inQueryBlock) fail("the panel rendered inside .query-block, where translateZ(0) traps it");
  if (geometry.trapped) fail(`a transformed ancestor is the panel's containing block: ${JSON.stringify(geometry)}`);
  if (geometry.left < 0 || geometry.right < 0) fail(`the panel hangs off the viewport: ${JSON.stringify(geometry)}`);
  if (geometry.bottom < 0) fail(`the panel runs off the bottom of the viewport: ${JSON.stringify(geometry)}`);
  if (geometry.width < 200 || geometry.height < 120) fail(`the panel has no size: ${JSON.stringify(geometry)}`);
  if (!geometry.topmost) fail(`something painted over the panel: ${JSON.stringify(geometry)}`);

  // --- 3. every one of the six reaches the FILE ------------------------------
  // Board first: switching to Board over an UNSET grouping is the one place a
  // default applies (ADR 0030), and it must arrive as the canonical field id.
  await press(browser, ".qd-panel .qd-view", "Board");
  // The panel's OWN state first. A failure here is a press that did not reach
  // the control; a failure at the property below is a write that did not land.
  // Told apart, they are two different bugs; together they are a mystery.
  const afterBoard = await browser.execute(() => ({
    trigger: document.querySelector(".qd-trigger")?.textContent?.trim() ?? null,
    panel: !!document.querySelector(".qd-panel"),
    active: document.querySelector(".qd-panel .qd-view.active")?.textContent?.trim() ?? null,
    views: [...document.querySelectorAll(".qd-panel .qd-view")].map((el) => el.textContent.trim()),
    switcher: document.querySelectorAll(".query-view-switcher").length,
  }));
  if (!/Board/.test(afterBoard.trigger ?? "") && afterBoard.active !== "Board") {
    fail(`pressing Board changed nothing on screen: ${JSON.stringify(afterBoard)}`);
  }
  await waitForProperty(browser, "Display", "tine.view", "board");
  await waitForProperty(browser, "Display", "tine.group-field", "state");

  // Then group by an ordinary property. A bare `owner` and `prop:owner` are the
  // same bytes to the old key and different things to the new one; the panel
  // writes the canonical spelling.
  await openDisplay(browser);
  await pickField(browser, "Change", "prop:owner");
  await waitForProperty(browser, "Display", "tine.group-field", "prop:owner");

  await openDisplay(browser);
  await pickField(browser, "+ sort", "priority");
  await waitForProperty(browser, "Display", "tine.sort", "priority asc");

  // Two totals, deliberately: the whole-result count AND a property sum. One
  // of them alone would not show that the list keeps more than its first entry.
  await openDisplay(browser);
  await press(browser, ".qd-panel .qd-add", "+ count");
  await waitForProperty(browser, "Display", "tine.col-aggregates", "count");
  await openDisplay(browser);
  await pickField(browser, "+ property", "cost");
  await waitForProperty(browser, "Display", "tine.col-aggregates", "count;cost=sum");

  await openDisplay(browser);
  await pickField(browser, "+ column", "owner");
  await waitForProperty(browser, "Display", "tine.columns", "owner");

  await openDisplay(browser);
  const sample = await browser.$(".qd-panel .qd-sample");
  await sample.click();
  await browser.keys("25".split(""));
  await browser.keys(["Enter"]);
  await waitForProperty(browser, "Display", "tine.sample", "25");

  // --- 4. the panel edits LISTS, not first entries --------------------------
  await openDisplay(browser);
  const kept = await browser.execute(() => ({
    sorts: [...document.querySelectorAll(".qd-panel .qd-section")]
      .find((s) => s.querySelector(".qd-section-title")?.textContent.trim() === "Sort")
      ?.querySelectorAll(".qd-row").length ?? 0,
    aggregates: [...document.querySelectorAll(".qd-panel .qd-section")]
      .find((s) => s.querySelector(".qd-section-title")?.textContent.trim() === "Summarize")
      ?.querySelectorAll(".qd-row").length ?? 0,
  }));
  if (kept.aggregates !== 2) fail(`the second total was dropped: ${JSON.stringify(kept)}`);
  if (kept.sorts !== 1) fail(`the sort list is not what was written: ${JSON.stringify(kept)}`);
  await closeDisplay(browser);

  // --- 5. what the file says, in one place ----------------------------------
  const saved = properties("Display");
  const expected = [
    ["tine.view", "board"],
    ["tine.group-field", "prop:owner"],
    ["tine.sort", "priority asc"],
    ["tine.columns", "owner"],
    ["tine.col-aggregates", "count;cost=sum"],
    ["tine.sample", "25"],
  ];
  for (const [key, value] of expected) {
    if (saved.get(key) !== value) {
      fail(`${key} is ${JSON.stringify(saved.get(key))}, not ${JSON.stringify(value)}: ${JSON.stringify([...saved])}`);
    }
  }
  // Nothing invented a second grouping key beside the one it wrote.
  if (saved.has("tine.group-by")) fail(`the retired key was written: ${JSON.stringify([...saved])}`);
  console.log(`display wrote: ${JSON.stringify([...saved])}`);

  // --- 6. the legacy key is READ, and retired on the first grouping save -----
  await browser.keys("Escape");
  await browser.$(".qs-sheet").waitForExist({ reverse: true, timeout: 5_000 });
  await openPage(browser, "Legacy");
  // Navigation settles the page title; query results arrive asynchronously.
  await browser.$(".sheet-board").waitForExist({ timeout: 20_000 });
  const legacyBoard = await browser.execute(() => ({
    columns: [...document.querySelectorAll(".sheet-board .sheet-board-header")].map((el) => el.textContent.trim()),
    boards: document.querySelectorAll(".sheet-board").length,
  }));
  if (legacyBoard.boards !== 1) fail(`the legacy board did not render: ${JSON.stringify(legacyBoard)}`);
  if (!legacyBoard.columns.some((c) => /Ada/.test(c)) || !legacyBoard.columns.some((c) => /Bo/.test(c))) {
    fail(`the legacy board is not grouped by owner: ${JSON.stringify(legacyBoard)}`);
  }
  await openSheet(browser);
  await openDisplay(browser);
  await pickField(browser, "Change", "state");
  await waitForProperty(browser, "Legacy", "tine.group-field", "state");
  const legacy = properties("Legacy");
  if (legacy.has("tine.group-by")) {
    fail(`the ambiguous key survived beside the canonical one: ${JSON.stringify([...legacy])}`);
  }
  console.log(`legacy migrated: ${JSON.stringify([...legacy])}`);
  await closeDisplay(browser);

  // --- 7. at the narrowest window the product allows, the panel still fits ---
  // The `max-width: 600px` bottom-sheet rule cannot be reached natively: the
  // desktop window declares `minWidth: 640` (src-tauri/tauri.conf.json), so
  // asking for less gets 640 back. `scripts/shot-query-display.mjs` photographs
  // that rule at 390px. What IS a desktop guarantee is this.
  await browser.setWindowSize(640, 700);
  await sleep(600);
  await openDisplay(browser);
  const narrow = await browser.execute(() => {
    const rect = document.querySelector(".qd-panel").getBoundingClientRect();
    return {
      left: Math.round(rect.left),
      right: Math.round(window.innerWidth - rect.right),
      top: Math.round(rect.top),
      bottom: Math.round(window.innerHeight - rect.bottom),
      width: Math.round(rect.width),
      innerWidth: window.innerWidth,
      innerHeight: window.innerHeight,
    };
  });
  if (narrow.left < 0 || narrow.right < 0) fail(`the panel hangs off the narrow viewport: ${JSON.stringify(narrow)}`);
  if (narrow.top < 0) fail(`the panel starts above the viewport: ${JSON.stringify(narrow)}`);
  // The panel does not scroll the page: anything past the bottom edge cannot be
  // reached at all, which is how half the settings would go missing again.
  if (narrow.bottom < 0) fail(`the panel runs off the bottom of the viewport: ${JSON.stringify(narrow)}`);
  if (narrow.width < 240) fail(`the panel collapsed at the narrowest window: ${JSON.stringify(narrow)}`);
  console.log(`narrow panel: ${JSON.stringify(narrow)}`);
  await closeDisplay(browser);
  await browser.setWindowSize(1280, 900);
  await sleep(400);
});

// --- 8. restart: the display the panel wrote is the display that comes back --
await withApp(1, async (browser) => {
  await openPage(browser, "Display");
  await browser.$(".sheet-board").waitForExist({ timeout: 20_000 });
  const reopened = await browser.execute(() => ({
    columns: [...document.querySelectorAll(".sheet-board .sheet-board-header")].map((el) => el.textContent.trim()),
    summaryHeaders: [...document.querySelectorAll(".query-summary-table thead th")].map((cell) => cell.textContent.trim()),
  }));
  if (!reopened.columns.some((c) => /Ada/.test(c)) || !reopened.columns.some((c) => /Bo/.test(c))) {
    fail(`the saved grouping did not come back: ${JSON.stringify(reopened)}`);
  }
  if (reopened.summaryHeaders.length !== 3 || !/count/i.test(reopened.summaryHeaders[1]) || !/sum/i.test(reopened.summaryHeaders[2])) {
    fail(`the complete saved aggregate summary did not come back: ${JSON.stringify(reopened)}`);
  }
  console.log(`reopened: ${JSON.stringify(reopened)}`);
});

console.log("query-display OK");
