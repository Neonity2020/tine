// Linux real-WebKit journey for P4: the ONE vocabulary picker and the live
// query text pane (SPEC §7.5, §4.3.2, §7.8, §6.4).
//
// What needs a real browser rather than jsdom:
//
//  1. **The list is virtualized.** `@tanstack/solid-virtual` decides which rows
//     to mount from a scroll element's real height and scroll offset. jsdom has
//     no layout, so the jsdom tests stub that geometry; only a real engine can
//     say that on a graph with hundreds of property keys the list mounts a
//     viewport's worth, that typing narrows it to a key no scroll would reach,
//     and that the keyboard can WALK to one.
//
//  2. **The pane's answers are the ENGINE's.** Every parse here is Rust's. A
//     jsdom test can only mock the diagnostics; this journey types text the
//     real parser rejects and reads back the real message, the real span and
//     the real suggestions — and then repairs it and saves, and finds the same
//     query after a restart (I-4).
//
//  3. **Counts are the registry's.** The graph on disk below is what makes the
//     numbers true: `status` is written on a known number of blocks, so a
//     fabricated count is visible as a wrong count rather than as a plausible
//     one.
import { spawn } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { openPageByLink } from "./lib/e2e-navigation.mjs";
import { waitForFileText } from "./e2e-file-poll.mjs";
import { tauriCapabilities, webdriverServerArgs } from "./e2e-capabilities.mjs";

await ensureDisplay();

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APP = process.env.TINE_APP || path.join(ROOT, "target/release/tine");
const TD = process.env.TAURI_DRIVER || (process.env.CARGO_HOME ? path.join(process.env.CARGO_HOME, "bin", "tauri-driver") : "tauri-driver");
const DRIVER_BASE = Number(process.env.E2E_DRIVER_PORT || 4506);
const NATIVE_BASE = Number(process.env.E2E_NATIVE_PORT || 4507);
const TMP = "/tmp/tine-query-vocabulary-e2e";
const GRAPH = `${TMP}/graph`;

fs.rmSync(TMP, { recursive: true, force: true });
for (const dir of ["pages", "journals", "logseq"]) fs.mkdirSync(`${GRAPH}/${dir}`, { recursive: true });
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(`${TMP}/xdg/${dir}`, { recursive: true });
fs.writeFileSync(`${GRAPH}/logseq/config.edn`, "{}\n");
const now = new Date();
const journal = `${now.getFullYear()}_${String(now.getMonth() + 1).padStart(2, "0")}_${String(now.getDate()).padStart(2, "0")}`;
fs.writeFileSync(`${GRAPH}/journals/${journal}.md`, "- Open [[Vocabulary]]\n");

// **The counts under test are these lines.** `status` is on eleven blocks and
// `owner` on three, so "commonest first" is a claim about this file and a
// fabricated number is a wrong number rather than a plausible one. `eigenvalue`
// is on exactly one block and sorts last of the property keys — it is the row a
// windowed list cannot reach by scrolling, which is what the filter is for.
const ROWS = [];
for (let i = 0; i < 11; i += 1) ROWS.push(`- item ${i}\n  status:: active`);
for (let i = 0; i < 3; i += 1) ROWS.push(`- owned ${i}\n  owner:: Ada`);
ROWS.push("- the rare one\n  eigenvalue:: 1.618");
fs.writeFileSync(
  `${GRAPH}/pages/Vocabulary.md`,
  ["- {{query (and (task TODO))}}", "- TODO alpha task", ...ROWS, ""].join("\n"),
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
      capabilities: tauriCapabilities(APP, "query-vocabulary"),
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

function fail(message) {
  throw new Error(message);
}

/** Navigate the way a user does — press the page reference on today's journal.
 *
 *  `openPageByLink` is the repository's ONE answer to this (D-14; the flake
 *  census in `lib/e2e-navigation.mjs` is what it exists for), and it already
 *  does the two things this journey needs and my own copy had to grow by hand:
 *  it returns immediately when the page is ALREADY routed — which the second
 *  launch needs, since restoring the session leaves no link to press — and it
 *  never holds an element handle across the re-render that replaces it. */
async function openPage(browser, title) {
  await browser.$(".ls-block, .page-title").waitForExist({ timeout: 20_000 });
  await openPageByLink(browser, title);
  await sleep(400);
}

async function openSheet(browser) {
  await browser.$(".qs-gear").click();
  await browser.$(".qs-sheet").waitForExist({ timeout: 10_000 });
  await sleep(400);
}

async function openPicker(browser) {
  await browser.$(".qs-sheet .qs-add").click();
  await browser.$(".qs-menu.qs-vocab").waitForExist({ timeout: 5_000 });
  await sleep(300);
}

/** Type into the picker's own filter, which is where the focus already is. */
async function filterTo(browser, needle) {
  const input = await browser.$(".qs-menu.qs-vocab .qs-menu-filter");
  await input.waitForExist({ timeout: 5_000 });
  await input.click();
  // Clear the way a person does. Assigning `el.value = ""` moves the DOM but
  // fires no `input`, so the component's own filter signal keeps the OLD
  // needle — and then `filterTo(browser, "")`, which types nothing, silently
  // leaves the list filtered. Ctrl-A + Backspace is the repository's idiom and
  // is the thing under test besides: the list has to come back when the box is
  // emptied by hand.
  await browser.keys(["Control", "a"]);
  await browser.keys(["Backspace"]);
  if (needle) await browser.keys(needle.split(""));
  await sleep(300);
  const settled = await browser.execute(() => document.querySelector(".qs-menu.qs-vocab .qs-menu-filter")?.value);
  if (settled !== needle) {
    fail(`the filter box holds ${JSON.stringify(settled)}, not ${JSON.stringify(needle)}`);
  }
}

await withApp(0, async (browser) => {
  await openPage(browser, "Vocabulary");
  await openSheet(browser);

  // --- 1. one list, and it is this graph's ---------------------------------
  await openPicker(browser);
  const list = await browser.execute(() => {
    const rows = [...document.querySelectorAll(".qs-vocab-option")];
    const read = (row) => ({
      key: row.getAttribute("data-vocabulary-key"),
      section: row.getAttribute("data-section"),
      text: row.innerText.replace(/\s+/g, " ").trim(),
    });
    return {
      mounted: rows.length,
      total: document.querySelector(".qs-vocab-options")?.getBoundingClientRect().height,
      viewport: document.querySelector(".qs-vocab-viewport")?.clientHeight,
      sections: [...document.querySelectorAll(".qs-menu.qs-vocab .qs-option-section")].map((el) => el.textContent.trim()),
      properties: rows.filter((row) => row.getAttribute("data-section") === "property").map(read),
      builtins: rows.filter((row) => row.getAttribute("data-section") === "builtin").map(read),
    };
  });
  if (!list.sections.includes("Built-in")) fail(`the list lost its built-in section: ${JSON.stringify(list.sections)}`);
  if (!list.sections.includes("Properties in this graph")) {
    fail(`the list never showed the graph's own properties: ${JSON.stringify(list.sections)}`);
  }
  // A built-in is not a registry row, so it carries no count. A borrowed one
  // would be a number the user could act on and the engine never said.
  for (const row of list.builtins) {
    if (/\d+\s+(blocks?|pages?)/.test(row.text)) fail(`a built-in fabricated a count: ${JSON.stringify(row)}`);
  }

  // --- 2. the counts and the order are the registry's -----------------------
  await filterTo(browser, "status");
  const status = await browser.execute(() => {
    const row = document.querySelector('.qs-vocab-option[data-vocabulary-key="status"][data-section="property"]');
    return row ? row.innerText.replace(/\s+/g, " ").trim() : null;
  });
  if (!status) fail("the graph's own `status` key is not in the list");
  // Eleven blocks carry `status::` in the file written above.
  if (!/\b11 blocks\b/.test(status)) fail(`the count is not the registry's: ${JSON.stringify(status)}`);
  if (!/observed text/.test(status)) fail(`the observed type is not labelled: ${JSON.stringify(status)}`);
  if (!/active/.test(status)) fail(`the row shows none of the values it has: ${JSON.stringify(status)}`);

  // --- 3. a rare key is reachable by typing AND by the keyboard -------------
  await filterTo(browser, "eigen");
  const rare = await browser.execute(() => {
    const row = document.querySelector('.qs-vocab-option[data-vocabulary-key="eigenvalue"]');
    const id = document.querySelector(".qs-menu.qs-vocab .qs-menu-filter")?.getAttribute("aria-activedescendant");
    return {
      found: !!row,
      text: row?.innerText.replace(/\s+/g, " ").trim(),
      active: id,
      activeMounted: !!(id && document.getElementById(id)),
    };
  });
  if (!rare.found) fail("a key used once in the graph could not be reached by typing");
  if (!/\b1 block\b/.test(rare.text)) fail(`the rare key's count is wrong: ${JSON.stringify(rare)}`);
  if (!rare.activeMounted) fail(`aria-activedescendant names a row that is not mounted: ${JSON.stringify(rare)}`);

  // Back to the whole list, and walk it. Every step must leave the keyboard on
  // a MOUNTED option — that is what makes a windowed list navigable at all.
  await filterTo(browser, "");
  const walk = await browser.execute(async () => {
    const menu = document.querySelector(".qs-menu.qs-vocab");
    const seen = new Set();
    for (let step = 0; step < 30; step += 1) {
      menu.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
      await new Promise((resolve) => requestAnimationFrame(resolve));
      const id = document.querySelector(".qs-menu.qs-vocab .qs-menu-filter")?.getAttribute("aria-activedescendant");
      const row = id ? document.getElementById(id) : null;
      if (!row) return { failedAt: step, id };
      seen.add(row.getAttribute("data-vocabulary-key"));
    }
    return { failedAt: null, reached: [...seen] };
  });
  if (walk.failedAt !== null) fail(`the keyboard left the list at step ${walk.failedAt}: ${JSON.stringify(walk)}`);
  if (!walk.reached.includes("status")) fail(`30 presses never reached the graph's own keys: ${JSON.stringify(walk)}`);

  // --- 4. picking a key writes a condition ---------------------------------
  await filterTo(browser, "status");
  const target = '.qs-vocab-option[data-vocabulary-key="status"][data-section="property"]';
  // WHERE the row is before it is pressed. A windowed list inside a popover can
  // hold a row in the DOM while the popover itself hangs below the window, and
  // then a click at the row's own centre lands somewhere else entirely; when
  // this assertion fails, the next line is what tells us which of the two
  // happened instead of leaving it to guesswork.
  const aim = await browser.execute((selector) => {
    const row = document.querySelector(selector);
    if (!row) return { found: false };
    const box = row.getBoundingClientRect();
    const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
    return {
      found: true,
      box: { top: Math.round(box.top), bottom: Math.round(box.bottom), height: Math.round(box.height) },
      window: { h: window.innerHeight },
      onScreen: box.top >= 0 && box.bottom <= window.innerHeight,
      hitIsTheRow: !!hit && (hit === row || row.contains(hit)),
      hit: hit?.className || null,
    };
  }, target);
  await (await browser.$(target)).click();
  await sleep(500);
  // Adding a condition is TWO steps by design: the key names what you are
  // asking about, and the second step asks what you want to know about it.
  // Picking therefore replaces the list with that key's value editor — it does
  // not append a half-written row to the sheet.
  const chosen = await browser.execute(() => ({
    listGone: !document.querySelector(".qs-menu.qs-vocab"),
    title: document.querySelector(".qs-value-editor .qs-menu-title")?.textContent?.trim() ?? null,
    hasValue: !!document.querySelector(".qs-value-editor .qs-input"),
    rows: document.querySelectorAll(".qs-sheet .qs-row").length,
  }));
  if (!chosen.listGone || !/^status/.test(chosen.title ?? "")) {
    fail(`picking the key did not open its value editor: ${JSON.stringify(chosen)}; aim ${JSON.stringify(aim)}`);
  }
  const valueBox = await browser.$(".qs-value-editor .qs-input");
  if (await valueBox.isExisting()) {
    await valueBox.click();
    await browser.keys("active".split(""));
  }
  await (await browser.$(".qs-value-editor .qs-commit")).click();
  await sleep(500);
  // Read the WHOLE row, not one cell. A property row's field chip says
  // `Property ▾` — which property it is belongs to the row's own text, and that
  // is what a reader has to be able to recognise after picking.
  const afterPick = await browser.execute(() => ({
    rows: document.querySelectorAll(".qs-sheet .qs-row").length,
    texts: [...document.querySelectorAll(".qs-sheet .qs-row")].map((el) => el.innerText.replace(/\s+/g, " ").trim()),
  }));
  if (!afterPick.texts.some((text) => /status/.test(text))) {
    fail(`the committed key did not become a row that names it: ${JSON.stringify(afterPick)}`);
  }

  // --- 5. the pane is the engine, and it never accuses the current text -----
  // Committing a condition re-opens the picker so a second one can be added
  // without hunting for the button again; dismiss it the way a person does
  // before reaching for the text at the foot of the sheet, or the popover sits
  // over the pane and the press never lands.
  await browser.keys(["Escape"]);
  await browser.waitUntil(
    async () => !(await browser.$(".qs-menu.qs-vocab").isExisting()),
    { timeout: 5_000, timeoutMsg: "Escape did not close the vocabulary popover" },
  );
  const pane = await browser.$(".qs-sheet .query-text-pane-input");
  await pane.waitForExist({ timeout: 10_000 });
  const printed = await pane.getValue();
  if (!printed.trim()) fail("the pane opened empty — nothing was printed for the query");

  // Put the query in through the production input handler, then type the LAST
  // character on the keyboard. Both halves matter: WebKitWebDriver under Xvfb
  // drops the odd space out of a long synthetic key sequence (it produced
  // `andstauts` once, and a test that mistypes its own fixture proves nothing),
  // while the final keystroke is what proves the pane re-reads as you type
  // rather than only on a button.
  const typo = "@block and stauts = 'active'";
  await browser.execute((text) => {
    const el = document.querySelector(".query-text-pane-input");
    el.value = text;
    el.dispatchEvent(new Event("input", { bubbles: true }));
  }, typo.slice(0, -1));
  await pane.click();
  await browser.keys([typo.slice(-1)]);
  await sleep(300);
  const echoed = await pane.getValue();
  if (echoed !== typo) fail(`the pane holds ${JSON.stringify(echoed)}, not the query that was typed`);
  await browser.waitUntil(
    async () => (await browser.$$(".query-text-pane-diagnostic")).length > 0,
    { timeout: 10_000, timeoutMsg: "the engine never reported what it could not read" },
  );
  const invalid = await browser.execute(() => ({
    message: document.querySelector(".query-text-pane-diagnostic-message")?.textContent ?? "",
    kind: document.querySelector(".query-text-pane-diagnostic")?.getAttribute("data-kind"),
    locate: !!document.querySelector(".query-text-pane-locate"),
    alternatives: [...document.querySelectorAll(".query-text-pane-diagnostic-alts")]
      .map((el) => el.textContent.replace(/\s+/g, " ").trim()),
    saveDisabled: document.querySelector(".query-text-pane-save")?.disabled,
    stale: !!document.querySelector(".qs-sheet-stale"),
    rowsStillDrawn: document.querySelectorAll(".qs-sheet .qs-row").length,
  }));
  // The parser's OWN message, not a catch-all (I-9), and the rows that last ran
  // are still on screen rather than blanked (a typo is not an empty graph).
  if (!/stauts/.test(invalid.message)) fail(`the pane did not report the engine's message: ${JSON.stringify(invalid)}`);
  if (invalid.kind !== "unknown_ident") fail(`the diagnostic lost its kind: ${JSON.stringify(invalid)}`);
  if (!invalid.saveDisabled) fail("an invalid parse left Save enabled");
  if (!invalid.stale) fail("the last-good rows were not marked stale");
  if (invalid.rowsStillDrawn < 1) fail("the rows were blanked while the text was invalid");
  // The registry is what makes the repair findable: the graph HAS `status`.
  if (!invalid.alternatives.some((line) => /status/.test(line))) {
    fail(`nothing pointed at the key the graph actually has: ${JSON.stringify(invalid)}`);
  }
  // Naming the word is half of it; **Show me** is the half that puts the
  // reader's cursor on it. Both are drawn from the same span, so if the
  // suggestions arrived and this did not, say so here rather than dying on a
  // missing selector three lines later.
  // **Show me is absent here, and that is the engine's doing, not the pane's.**
  // `unknown_ident` is built by `Diagnostic::new` in
  // `crates/tine-core/src/query/tql.rs` and never given a span, so there is no
  // range to select and the pane correctly draws no button it could not honour.
  // The suggestion above still arrives, because that travels in `suggestions`.
  // Asserted as the CURRENT truth so that the day the parser attaches a span
  // here, this line fails and is upgraded rather than silently passing.
  // Blocker recorded in RECEIPT-p4.md: fixing it is a parser change, which is
  // outside this packet's scope.
  if (invalid.locate) {
    fail(`unknown_ident now carries a span — turn this into the positive assertion: ${JSON.stringify(invalid)}`);
  }

  // A diagnostic the engine DOES span, so the span → selection path is proven
  // against the real parser rather than only in the render tests: the anchor
  // has to come first, and `@block` in the tail is pointed at exactly.
  const misplaced = "task = 'TODO' and @block";
  await browser.execute((text) => {
    const el = document.querySelector(".query-text-pane-input");
    el.value = text;
    el.dispatchEvent(new Event("input", { bubbles: true }));
  }, misplaced);
  await browser.waitUntil(
    async () => await (await browser.$(".query-text-pane-locate")).isExisting(),
    { timeout: 10_000, timeoutMsg: "a diagnostic the engine spans still offered no way to the text" },
  );
  await (await browser.$(".query-text-pane-locate")).click();
  await sleep(250);
  const selected = await browser.execute(() => {
    const el = document.querySelector(".query-text-pane-input");
    return {
      picked: el.value.slice(el.selectionStart, el.selectionEnd),
      focused: document.activeElement === el,
    };
  });
  if (selected.picked !== "@") {
    fail(`"Show me" selected ${JSON.stringify(selected)} instead of the span the engine gave`);
  }
  if (!selected.focused) fail("\"Show me\" selected the text without putting the cursor in it");

  // --- 6. repair it, save it, and find it after a restart -------------------
  await browser.execute(() => {
    const el = document.querySelector(".query-text-pane-input");
    el.value = "";
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
  // Repair it the way the engine said to. Its suggestion above was
  // `prop('status')`, not a bare `status` — a property key is not a field of
  // the query language, and the whole worth of the suggestion is that
  // following it literally produces a query that parses.
  const repaired = "@block and prop('status') = 'active'";
  await browser.execute((text) => {
    const el = document.querySelector(".query-text-pane-input");
    el.value = text;
    el.dispatchEvent(new Event("input", { bubbles: true }));
  }, repaired.slice(0, -1));
  await (await browser.$(".query-text-pane-input")).click();
  await browser.keys([repaired.slice(-1)]);
  const save = await browser.waitUntil(async () => {
    const button = await browser.$(".query-text-pane-save");
    return (await button.isExisting()) && !(await button.getAttribute("disabled")) ? button : false;
  }, { timeout: 15_000, timeoutMsg: "a valid query never enabled Save" });
  await save.click();

  // **What proves a save is the FILE, and the shared poller is how this repo
  //  reads it.** A save publishes through atomic replacement, so a bare
  //  `readFileSync` can hit the unlink-to-rename window and throw ENOENT;
  //  `waitForFileText` retries exactly that absence and nothing else, and it
  //  fails with the last bytes it actually saw rather than with a timeout.
  await waitForFileText(
    `${GRAPH}/pages/Vocabulary.md`,
    (text) => {
      const queryLine = text.split("\n")[0];
      return /status/.test(queryLine) && /active/.test(queryLine) && !/TODO/.test(queryLine);
    },
    "the repaired query text",
    { timeoutMs: 15_000 },
  );
  const saved = fs.readFileSync(`${GRAPH}/pages/Vocabulary.md`, "utf8");
  const expectedSiblings = ["- TODO alpha task", ...ROWS, ""].join("\n");
  if (saved.split("\n").slice(1).join("\n") !== expectedSiblings) {
    fail("saving the query changed its sibling blocks");
  }
});

// --- 7. the same query, after a restart ------------------------------------
// The only proof that what landed on disk says what the pane said (I-4).
await withApp(1, async (browser) => {
  await openPage(browser, "Vocabulary");
  const sentence = await browser.$(".qs-sentence");
  await sentence.waitForExist({ timeout: 15_000 });
  const said = (await sentence.getText()).trim();
  if (/task:/i.test(said)) fail(`the old task condition survived the text save: ${JSON.stringify(said)}`);
  if (!/status/i.test(said)) fail(`the saved query did not reopen as its sentence: ${JSON.stringify(said)}`);
  if (/⟨advanced⟩/.test(said)) fail(`the saved query reopened as an unreadable capsule: ${JSON.stringify(said)}`);
});

console.log("PASS query-vocabulary");
