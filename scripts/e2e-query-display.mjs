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
//  5. A mixed Friendly result carries TWO scoped namespaces plus a membership
//     scope (Q3), and the difference that matters is between a namespace that
//     is ABSENT and one that is present and empty. That difference only exists
//     once the properties have been printed by Rust, re-read by Rust and handed
//     back to a section — which is to say, across a restart.
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

// **A mixed Friendly result** (Q3). `(search "roadmap")` admits PAGES by name
// and BLOCKS by content from one question, which is the only shape that can
// show two independently controlled families. The two Roadmap pages exist to be
// the Pages half; the two bullets under the query are the Blocks half.
const SCOPED_PAGE = [
  '- {{query (search "roadmap")}}',
  "- The roadmap review is next week",
  "- Another block that names the roadmap",
  "",
].join("\n");
fs.writeFileSync(`${GRAPH}/pages/Scoped.md`, SCOPED_PAGE);
fs.writeFileSync(`${GRAPH}/pages/Roadmap north.md`, "status:: open\n\n- North body\n");
fs.writeFileSync(`${GRAPH}/pages/Roadmap south.md`, "status:: done\n\n- South body\n");

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
  await activate(browser, ".qs-gear", { describe: "the sheet gear" });
  await browser.$(".qs-sheet").waitForExist({ timeout: 10_000 });
}

/** Find a control and activate it in ONE round trip, retrying the whole
 *  selection until a node is found.
 *
 *  This is the `scripts/lib/e2e-navigation.mjs` rule one control down: **never
 *  hold a WebDriver element handle across a re-rendering list.** Everything in
 *  the Display panel re-renders as a write settles, so `browser.$(sel)` then
 *  `.click()` can deliver the click to a node that has already left the
 *  document — nothing happens, and the journey fails ten seconds later on the
 *  outcome instead of on the press. Finding and clicking inside one
 *  `browser.execute` makes detachment unrepresentable: no JS turn separates
 *  them. Two of ten unchanged-binary burn-in runs failed this way, in two
 *  different controls, which is what says it is a class rather than a site.
 *
 *  `focus()` before `click()` because some of these controls are inputs the
 *  journey then types into, and a scripted `click()` does not move focus. */
async function activate(browser, selector, { text = null, describe } = {}) {
  let seen = [];
  try {
    await browser.waitUntil(async () => {
      const outcome = await browser.execute((where, wanted) => {
        const nodes = [...document.querySelectorAll(where)];
        const texts = nodes.map((el) => (el.textContent ?? "").trim());
        const index = wanted === null ? (nodes.length ? 0 : -1) : texts.indexOf(wanted);
        if (index < 0) return { clicked: false, texts };
        nodes[index].focus?.();
        nodes[index].click();
        return { clicked: true, texts };
      }, selector, text);
      seen = outcome.texts;
      return outcome.clicked;
    }, { timeout: 15_000, interval: 250, timeoutMsg: "not found" });
  } catch {
    const what = describe ?? (text === null ? selector : `${selector} reading ${JSON.stringify(text)}`);
    fail(`never activated ${what}; ${selector} offered ${JSON.stringify(seen)}`);
  }
}

/** Press a Display trigger until its panel is up, finding and clicking it in ONE
 *  round trip.
 *
 *  The sections and the sheet footer re-render as a read settles, so a handle
 *  taken by `browser.$()` can be detached by the time `.click()` reaches it:
 *  the click goes to a node that is no longer in the document, nothing happens,
 *  and the journey fails ten seconds later on the panel rather than on the
 *  press. That is the same shape `scripts/lib/e2e-navigation.mjs` exists to
 *  stop, one control down. The panel's EXISTENCE is the condition — not the
 *  trigger's `aria-expanded`, which describes a control that may already have
 *  been replaced — and the check and the click share a round trip so a panel
 *  that opened in between is never toggled back shut. */
async function pressDisplayTrigger(browser, selector, description) {
  await browser.$(selector).waitForExist({ timeout: 15_000 });
  await browser.waitUntil(async () => {
    await browser.execute((where) => {
      if (document.querySelector(".qd-panel")) return;
      document.querySelector(where)?.click();
    }, selector);
    return await browser.$(".qd-panel").isExisting();
  }, { timeout: 15_000, interval: 400, timeoutMsg: `${description} never opened` });
}

/** The panel lives in the sheet's footer, and a write can remount the sheet
 *  under it — so reopen whatever is shut rather than assuming either is up. */
async function openDisplay(browser) {
  if (!(await browser.$(".qd-trigger").isExisting())) await openSheet(browser);
  await pressDisplayTrigger(browser, ".qd-trigger", "the Display panel");
}

async function closeDisplay(browser) {
  if (await browser.$(".qd-panel").isExisting()) await browser.keys("Escape");
  await browser.$(".qd-panel").waitForExist({ reverse: true, timeout: 5_000 });
}

/** Press a button by its exact visible text, inside a container. */
async function press(browser, selector, text) {
  await activate(browser, selector, { text });
}

/** Open a field picker from its trigger, narrow it, and take the named field. */
async function pickField(browser, triggerText, key) {
  await press(browser, ".qd-panel .qd-add, .qd-panel .qd-row-btn", triggerText);
  await browser.$(".qd-field-picker .qs-vocab-options").waitForExist({ timeout: 8_000 });
  const offered = await browser.execute(() =>
    [...document.querySelectorAll(".qd-field-picker .qs-vocab-option")].map((el) => el.getAttribute("data-vocabulary-key")));
  if (!offered.includes(key)) {
    fail(`the ${JSON.stringify(triggerText)} picker does not offer ${key}; it offers ${JSON.stringify(offered)}`);
  }
  await activate(browser, `.qd-field-picker .qs-vocab-option[data-vocabulary-key="${key}"]`, {
    describe: `the ${JSON.stringify(key)} option in the ${JSON.stringify(triggerText)} picker`,
  });
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

/** One section's Display panel, addressed through the family it belongs to.
 *  Two panels are mounted side by side on a mixed result, so a bare
 *  `.qd-trigger` would be satisfied by whichever came first in the DOM. */
async function openSectionDisplay(browser, kind) {
  await pressDisplayTrigger(
    browser,
    `[data-query-result-kind="${kind}"] .qd-trigger`,
    `the ${kind} section's Display panel`,
  );
}

/** Wait until a block's own property map satisfies a predicate. A wdio timeout
 *  says only that a condition failed; what is needed here is the FILE, because
 *  a write that never happened and a write that landed under another key look
 *  identical from the browser. */
async function waitForProperties(browser, page, predicate, description) {
  try {
    await browser.waitUntil(async () => predicate(properties(page)), { timeout: 15_000 });
  } catch {
    fail(
      `${page}: ${description}\n`
        + `  read properties: ${JSON.stringify([...properties(page)])}\n`
        + `  file:\n${fs.readFileSync(`${GRAPH}/pages/${page}.md`, "utf8")}`,
    );
  }
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
  await activate(browser, ".qd-panel .qd-sample", { describe: "the Sample input" });
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

  // --- 8. a mixed result is two families, each with its own settings (Q3) ----
  //
  // A Friendly search answers two questions at once — which PAGES match and
  // which BLOCKS match. Before this they arrived in one flat list under one
  // presentation, so "the pages as a table, the blocks as a board" was
  // unsayable and a page's own display settings were unreachable. What a real
  // engine and a real browser add over jsdom is the same thing they add above:
  // the scoped keys are written by the frontend, printed by Rust and re-read by
  // Rust, and only a restart can show that the file says what the panel said.
  await browser.keys("Escape");
  await openPage(browser, "Scoped");
  await browser.$('[data-query-result-kind="page"]').waitForExist({ timeout: 20_000 });
  const families = await browser.execute(() => {
    const sections = [...document.querySelectorAll("[data-query-result-kind]")];
    return sections.map((section) => {
      const heading = section.querySelector("h3");
      return {
        kind: section.getAttribute("data-query-result-kind"),
        heading: heading?.textContent?.trim() ?? null,
        labelled: !!heading?.id && section.getAttribute("aria-labelledby") === heading.id,
        trigger: section.querySelector(".qd-trigger")?.getAttribute("aria-label") ?? null,
        // Both ways out of a scoped state are offered, and "use inherited" is
        // dead until there IS something scoped to give back (I-10).
        inherit: section.querySelector(".query-scoped-reset")?.disabled ?? null,
        clear: !!section.querySelector(".query-scoped-clear"),
        // Page membership scope is neither namespace's display setting, so it
        // sits on the Pages section and only there.
        matchScope: section.querySelector(".query-page-match select")?.value ?? null,
      };
    });
  });
  if (families.length !== 2 || families[0].kind !== "page" || families[1].kind !== "block") {
    fail(`the mixed result is not two families, Pages first: ${JSON.stringify(families)}`);
  }
  if (families[0].heading !== "Pages" || families[1].heading !== "Blocks"
    || families.some((family) => !family.labelled)) {
    fail(`the families do not name their own regions: ${JSON.stringify(families)}`);
  }
  if (families[0].trigger !== "Display pages" || families[1].trigger !== "Display blocks") {
    fail(`the two Display controls do not say which section they change: ${JSON.stringify(families)}`);
  }
  if (families[0].inherit !== true || families[1].inherit !== true
    || !families[0].clear || !families[1].clear) {
    fail(`the scoped reset controls are wrong before any scoped edit: ${JSON.stringify(families)}`);
  }
  if (families[0].matchScope !== "names" || families[1].matchScope !== null) {
    fail(`page membership scope is missing or on the wrong section: ${JSON.stringify(families)}`);
  }

  // The Pages section takes a table. It writes the PAGE namespace and nothing
  // else: not the singular `tine.view` the whole query used to share, and not
  // the other family's keys.
  await openSectionDisplay(browser, "page");
  await press(browser, ".qd-panel .qd-view", "Table");
  await waitForProperty(browser, "Scoped", "tine.page-view", "table");
  await waitForProperty(browser, "Scoped", "tine.page-display", "1");
  await waitForProperties(
    browser,
    "Scoped",
    (props) => !props.has("tine.view") && ![...props.keys()].some((key) => key.startsWith("tine.block-")),
    "the page section's edit wrote outside its own namespace",
  );
  await closeDisplay(browser);

  // The Blocks section takes a board, and the Pages section keeps its table.
  await openSectionDisplay(browser, "block");
  await press(browser, ".qd-panel .qd-view", "Board");
  await waitForProperty(browser, "Scoped", "tine.block-view", "board");
  await waitForProperty(browser, "Scoped", "tine.block-display", "1");
  await waitForProperty(browser, "Scoped", "tine.page-view", "table");
  await closeDisplay(browser);
  //
  // The two families draw a board with DIFFERENT renderers, and that is the
  // product, not an inconsistency: page cards navigate, so the Pages section
  // uses the shared read-only renderers in `QueryPageResults`; block cards keep
  // ordinary editing, so an inline query's Blocks section is the sheet board
  // that owns that editing surface (§15.3). Asserting the Pages renderer's
  // class on the Blocks section would demand that block results stop being
  // editable.
  const bothFaces = await browser.execute(() => {
    // The verdict is the two faces. The rest is here so a failure says WHICH of
    // the two ways this can go wrong happened: a family that rendered the wrong
    // face, or a family with no rows to render one for at all.
    const section = (kind) => document.querySelector(`[data-query-result-kind="${kind}"]`);
    const shown = (kind) => {
      const host = section(kind);
      if (!host) return "no such section";
      if (host.querySelector(".sheet-board")) return "board";
      if (host.querySelector(".sheet-table")) return "table";
      for (const face of ["board", "table", "list", "search"]) {
        if (host.querySelector(`.query-results-${face}`)) return face;
      }
      if (host.querySelector(".query-group")) return "groups";
      return host.querySelector(".query-result-section-empty")?.textContent?.trim() ?? "nothing";
    };
    return {
      page: document.querySelector('[data-query-result-kind="page"] .query-results-table') ? "table" : null,
      block: document.querySelector('[data-query-result-kind="block"] .sheet-board') ? "board" : null,
      shows: { page: shown("page"), block: shown("block") },
      counts: {
        page: section("page")?.querySelector(".query-result-section-count")?.textContent?.trim() ?? null,
        block: section("block")?.querySelector(".query-result-section-count")?.textContent?.trim() ?? null,
      },
    };
  });
  if (bothFaces.page !== "table" || bothFaces.block !== "board") {
    fail(`the two families do not render their own presentations: ${JSON.stringify(bothFaces)}`);
  }

  // Page membership scope: three exact values, and the one that is stated is
  // the one that is stored. `both` is not a display setting — it changes which
  // pages are MEMBERS — so it lives outside both namespaces.
  const scopeChoices = await browser.execute(() =>
    [...document.querySelectorAll('[data-query-result-kind="page"] .query-page-match option')]
      .map((option) => option.value));
  if (JSON.stringify(scopeChoices) !== JSON.stringify(["names", "content", "both"])) {
    fail(`page membership scope does not offer the three stored values: ${JSON.stringify(scopeChoices)}`);
  }
  // The handle is taken and used with nothing in between. A `<select>` needs a
  // real change event, so this one keeps WebDriver rather than scripting the
  // value — but it must not sit across a round trip while the section rerenders.
  await browser
    .$('[data-query-result-kind="page"] .query-page-match select')
    .selectByAttribute("value", "both");
  await waitForProperty(browser, "Scoped", "tine.page-match-scope", "both");

  // **Absent and present-but-empty are different states, and both are
  // reachable.** "Use inherited settings" REMOVES the namespace, so the section
  // shows what the query says again; membership scope and the other family are
  // untouched by it.
  await activate(browser, '[data-query-result-kind="page"] .query-scoped-reset', { describe: "Use inherited settings" });
  await waitForProperties(
    browser,
    "Scoped",
    (props) => ![...props.keys()].some((key) => key.startsWith("tine.page-view") || key === "tine.page-display")
      && props.get("tine.page-match-scope") === "both"
      && props.get("tine.block-view") === "board",
    "inheriting the page settings did not remove exactly that namespace",
  );

  // "Clear settings" writes an EMPTY draft: the marker is there with no members,
  // which says "show nothing extra" rather than "show what the query says".
  await activate(browser, '[data-query-result-kind="page"] .query-scoped-clear', { describe: "Clear settings" });
  await waitForProperties(
    browser,
    "Scoped",
    (props) => props.get("tine.page-display") === "1" && !props.has("tine.page-view")
      && !props.has("tine.page-columns") && !props.has("tine.page-sort"),
    "clearing the page settings did not leave a present-but-empty draft",
  );
  console.log(`scoped display wrote: ${JSON.stringify([...properties("Scoped")])}`);
});

// --- 9. restart: the display the panel wrote is the display that comes back --
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

  // Q3: what a restart has to reproduce here is not one setting but three
  // distinct answers in one file — a page draft that is PRESENT AND EMPTY, a
  // populated block draft, and a membership scope that belongs to neither. A
  // reader that used truthiness would collapse the first into "absent" and hand
  // the Pages section the query's settings instead of the empty draft it was
  // given.
  await openPage(browser, "Scoped");
  await browser.$('[data-query-result-kind="block"]').waitForExist({ timeout: 20_000 });
  await browser.waitUntil(
    // Same two renderers as above: the Blocks section's board is the editable
    // sheet board, the Pages section's faces are the read-only ones.
    async () => (await browser.$$('[data-query-result-kind="block"] .sheet-board')).length === 1,
    { timeout: 15_000, timeoutMsg: "the saved block board did not come back" },
  );
  const scopedAgain = await browser.execute(() => ({
    matchScope: document.querySelector('[data-query-result-kind="page"] .query-page-match select')?.value ?? null,
    pageList: !!document.querySelector('[data-query-result-kind="page"] .query-results-list'),
    pageTable: !!document.querySelector('[data-query-result-kind="page"] .query-results-table'),
    blockBoard: !!document.querySelector('[data-query-result-kind="block"] .sheet-board'),
    // An empty draft is still a draft, so the way back to inherited is live.
    pageInherit: document.querySelector('[data-query-result-kind="page"] .query-scoped-reset')?.disabled ?? null,
  }));
  if (scopedAgain.matchScope !== "both") {
    fail(`the saved page membership scope did not come back: ${JSON.stringify(scopedAgain)}`);
  }
  if (!scopedAgain.blockBoard || scopedAgain.pageTable || !scopedAgain.pageList) {
    fail(`the two saved presentations did not come back independently: ${JSON.stringify(scopedAgain)}`);
  }
  if (scopedAgain.pageInherit !== false) {
    fail(`a present-but-empty page draft was read as absent: ${JSON.stringify(scopedAgain)}`);
  }
  console.log(`scoped reopened: ${JSON.stringify(scopedAgain)}`);
});

console.log("query-display OK");
