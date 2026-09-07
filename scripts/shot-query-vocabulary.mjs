import { waitForHttpServer } from "./e2e-capabilities.mjs";
// Screenshot the P4 surfaces, so they can be LOOKED at rather than inferred
// from selectors (SPEC §7.5, §4.3.2, §7.8):
//
//   query-vocabulary.png          the ONE field list, open on a graph with
//                                 hundreds of property keys: sections, the
//                                 observed type, the authoritative count and a
//                                 few of the values, commonest first.
//   query-vocabulary-search.png   the same list narrowed to a rare key, with
//                                 the keyboard on a row the scroll had to
//                                 travel to reach.
//   query-vocabulary-novel.png    a key this graph has not got, offered
//                                 honestly as `0 blocks today`.
//   query-vocabulary-narrow.png   the same list at 560px, in the bottom sheet.
//   query-text-pane-invalid.png   the live pane holding text the engine could
//                                 not read: the parser's own message, the
//                                 spans it can point at, and the rows above
//                                 still on screen and greyed.
//   query-advanced-pane.png       a query too deep to draw: the ⟨advanced⟩ row
//                                 and the pane it sends the cursor to.
//   query-crossing-notice.png     the §7.5 crossing notice, hosted INSIDE the
//                                 pane because the sheet is open.
//
// Layout is the one thing jsdom cannot judge, and the vocabulary list is
// virtualized — its rows are two lines of metadata inside a fixed-height
// scrolling viewport. These are the pictures that say whether that reads.
//
// Headless Chromium over the mock backend (the "Jun 14th, 2026" journal has a
// pure {{query}} block).
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5273;
const OUT = "screenshots";
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });

async function shot(page, name, clip) {
  await page.screenshot({ path: `${OUT}/${name}.png`, ...(clip ? { clip } : { fullPage: false }) });
  console.log(`wrote ${OUT}/${name}.png`);
}

/** The popover is taller than the space the page leaves under it, and a viewport
 *  clip would cut the very rows these pictures exist to show. Photograph the
 *  ELEMENT. */
async function shotElement(page, selector, name) {
  await page.locator(selector).first().screenshot({ path: `${OUT}/${name}.png` });
  console.log(`wrote ${OUT}/${name}.png`);
}

/** A generous box around one element, clamped to the viewport. */
function around(box, view, pad = 16) {
  const x = Math.max(0, box.x - pad);
  const y = Math.max(0, box.y - pad);
  return {
    x, y,
    width: Math.min(view.width - x, box.width + pad * 2),
    height: Math.min(view.height - y, box.height + pad * 2),
  };
}

/** The canned engine answers. The mock cannot compute — it hands back values —
 *  so a graph "with hundreds of keys" is a registry list, and an "invalid"
 *  query is a parse carrying diagnostics. Both are what Rust would return. */
function fixture({ diagnostics = [], deep = false } = {}) {
  const text = (t) => ({ kind: "text", text: t });
  const attr = (a, op, value) => ({ kind: "leaf", leaf: { kind: "attr", attr: a, op, value } });
  const rel = (r, pred) => ({ kind: "leaf", leaf: { kind: "rel", rel: r, quant: "any", pred } });
  const KEYS = [
    ["status", 412, 9, "text", "one", [["active", 210], ["blocked", 80], ["done", 122]]],
    ["owner", 388, 4, "ref", "one", [["Ada", 190], ["Grace", 120], ["Linus", 78]]],
    ["cost", 274, 0, "number", "one", [["100", 30], ["250", 22], ["1000", 9]]],
    ["due-date", 201, 0, "date", "one", [["2026-06-30", 12], ["2026-07-14", 9]]],
    ["tags", 180, 61, "ref", "many", [["research", 60], ["ops", 44], ["draft", 30]]],
    ["reviewed", 96, 0, "checkbox", "one", [["true", 70], ["false", 26]]],
    ["team", 71, 33, "text", "one", [["Platform", 40], ["Design", 20]]],
  ];
  const rows = KEYS.map(([name, blocks, pages, type, cardinality, top]) => ({
    normalized_name: name,
    cardinality,
    observed_type: type,
    count_blocks: blocks,
    count_pages: pages,
    mismatch_count: 0,
    top_values: top,
    ...(name === "cost" ? { declared: ["number", "one"] } : {}),
  }));
  // The long tail a virtualized list exists for, plus one deliberately rare key
  // no scroll would reach.
  for (let i = 0; i < 240; i += 1) {
    rows.push({
      normalized_name: `field-${String(i).padStart(3, "0")}`,
      cardinality: "one",
      observed_type: "text",
      count_blocks: 60 - Math.floor(i / 8),
      count_pages: 0,
      mismatch_count: 0,
      top_values: [["yes", 3]],
    });
  }
  rows.push({
    normalized_name: "eigenvalue",
    cardinality: "one",
    observed_type: "number",
    count_blocks: 1,
    count_pages: 0,
    mismatch_count: 0,
    top_values: [["1.618", 1]],
  });

  const leaf = attr("task", "in", { kind: "list", items: [text("TODO"), text("DOING")] });
  const shallow = {
    kind: "and",
    items: [
      leaf,
      rel("page", attr("name", "eq", text("Project/Roadmap"))),
      rel("props", { kind: "and", items: [attr("key", "eq", text("owner")), attr("value", "eq", text("Ada"))] }),
    ],
  };
  let nested = leaf;
  for (let level = 0; level < 20; level += 1) nested = { kind: "and", items: [nested] };

  return {
    print: "@block and task in ('TODO', 'DOING') and page.name = 'Project/Roadmap'"
      + " and any(props, key = 'owner' and value = 'Ada')",
    registry: { rows, generation: 4 },
    parse: {
      query: {
        anchor: "block",
        filter: deep ? nested : shallow,
        diagnostics,
        source: { kind: "tql", original: "@block and task in ('TODO', 'DOING')" },
      },
      view: {},
    },
  };
}

/** One page with the fixture installed, already scrolled to the query block. */
async function openSheet(browser, view, options) {
  const page = await browser.newPage({ viewport: view, deviceScaleFactor: 2 });
  page.on("pageerror", (e) => console.log("pageerror:", String(e).split("\n")[0]));
  await page.addInitScript((value) => {
    globalThis.__tineMockQueryFixture = value;
  }, fixture(options));
  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForSelector(".page-title", { timeout: 8000 });
  await page.waitForSelector(".qs-sentence", { timeout: 8000 });
  await sleep(500);
  // A phone-width viewport brings its own chrome: the navigation drawer opens
  // over the left half and the first-run Guide toast sits on the bottom edge,
  // exactly where the sheet docks. Dismiss both, then DISPATCH the press rather
  // than hit-testing it — this script photographs the picker; that the gear is
  // clickable is `scripts/e2e-query-sheet.mjs`'s job, on the real engine.
  await page.evaluate(() => {
    document.querySelector("[data-mobile-drawer-scrim]")?.dispatchEvent(
      new MouseEvent("click", { bubbles: true }),
    );
    for (const button of document.querySelectorAll("button")) {
      const label = (button.getAttribute("aria-label") ?? "") + " " + (button.textContent ?? "");
      if (/dismiss|close/i.test(label) && button.closest(".toast, .toast-stack, [class*=toast]")) {
        button.click();
      }
    }
  });
  await sleep(300);
  // Put the query block near the TOP of the window. The sheet is positioned
  // from the sentence's rect and its menus open downward, so a block low on the
  // page opens a tall popover past the window edge — which is where the sheet's
  // menus have always opened, and is not what these pictures are of.
  await page.evaluate(() => {
    document.querySelector(".query-block")?.scrollIntoView({ block: "start" });
    window.scrollBy(0, -80);
  });
  await sleep(400);
  await page.locator(".qs-gear").first().dispatchEvent("click");
  await page.waitForSelector(".qs-sheet", { timeout: 6000 });
  await sleep(500);
  return page;
}

/** Open the ONE vocabulary picker from + Add condition. */
async function openPicker(page) {
  await page.locator(".qs-sheet .qs-add").first().dispatchEvent("click");
  await page.waitForSelector(".qs-menu.qs-vocab", { timeout: 6000 });
  await sleep(400);
}

try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 40, 250, { failureMessage: "server did not start" });
  const browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
  const view = { width: 1200, height: 1000 };

  // --- 1. the list, on a graph with a real vocabulary -----------------------
  {
    const page = await openSheet(browser, view, {});
    await openPicker(page);
    const shown = await page.evaluate(() => ({
      mounted: document.querySelectorAll(".qs-vocab-option").length,
      sections: [...document.querySelectorAll(".qs-menu.qs-vocab .qs-option-section")].map(
        (el) => el.textContent.trim(),
      ),
      firstProperty: document
        .querySelector('.qs-vocab-option[data-section="property"]')
        ?.innerText.replace(/\s+/g, " ")
        .trim(),
      // The whole list's height against the window's: the ratio IS the claim
      // that a graph with hundreds of keys mounts a viewport's worth.
      listHeight: document.querySelector(".qs-vocab-options")?.getBoundingClientRect().height,
      viewport: document.querySelector(".qs-vocab-viewport")?.clientHeight,
      onScreen: (() => {
        const rect = document.querySelector(".qs-menu.qs-vocab").getBoundingClientRect();
        return rect.bottom <= window.innerHeight && rect.top >= 0;
      })(),
    }));
    console.log("vocabulary:", JSON.stringify(shown));
    // I-22: a 250-key graph must MOUNT a viewport's worth, not the graph.
    if (shown.mounted > 40) {
      throw new Error(`the vocabulary list is not windowed: ${shown.mounted} rows mounted`);
    }
    await shotElement(page, ".qs-menu.qs-vocab", "query-vocabulary");

    // --- 2. narrowed to a rare key, with the keyboard on a distant row -----
    await page.locator(".qs-menu-filter").fill("field-2");
    await sleep(350);
    for (let step = 0; step < 12; step += 1) await page.keyboard.press("ArrowDown");
    await sleep(350);
    const active = await page.evaluate(() => {
      const id = document.querySelector(".qs-menu-filter")?.getAttribute("aria-activedescendant");
      const row = id ? document.getElementById(id) : null;
      return {
        id,
        mounted: !!row,
        key: row?.getAttribute("data-vocabulary-key"),
        text: row?.innerText.replace(/\s+/g, " ").trim(),
      };
    });
    console.log("keyboard reached:", JSON.stringify(active));
    if (!active.mounted) throw new Error("aria-activedescendant names a row that is not mounted");
    await shotElement(page, ".qs-menu.qs-vocab", "query-vocabulary-search");

    // --- 3. a key the graph has not got, offered honestly -------------------
    await page.locator(".qs-menu-filter").fill("budget");
    await sleep(350);
    const novel = await page.evaluate(() => {
      const row = document.querySelector(".qs-vocab-option.qs-vocab-novel");
      return row?.innerText.replace(/\s+/g, " ").trim();
    });
    console.log("novel row:", JSON.stringify(novel));
    if (!novel || !novel.includes("0 blocks today")) {
      throw new Error(`the unmatched-key row is not honest about its count: ${JSON.stringify(novel)}`);
    }
    await shotElement(page, ".qs-menu.qs-vocab", "query-vocabulary-novel");
    await page.close();
  }

  // --- 4. narrow screens: can a property and its count actually be REACHED --
  //
  // The first version of this section took one picture at 560px and called the
  // placement pre-existing. A picture is not usability: in that picture most of
  // the list lay below the window and only the first built-ins were visible, so
  // nothing had been shown about whether the graph's own vocabulary — the point
  // of the packet — can be brought into view and chosen at all.
  //
  // So this drives it. At each width it types a filter, scrolls the way a
  // person would (a wheel over the sheet, which is the bottom sheet's own
  // scrolling — no second popover controller), and then demands that the target
  // row is INSIDE the window, that its count is readable, that nothing is cut
  // off horizontally, and that pressing it adds the condition.
  for (const [label, narrow] of [
    ["narrow", { width: 560, height: 900 }],
    ["phone", { width: 390, height: 844 }],
  ]) {
    const page = await openSheet(browser, narrow, {});
    await openPicker(page);

    /** Where the target row is, and whether anything is cut off sideways. */
    const measure = (selector) =>
      page.evaluate((sel) => {
        const row = document.querySelector(sel);
        const menu = document.querySelector(".qs-menu.qs-vocab");
        const sheet = document.querySelector(".qs-sheet");
        const rect = row?.getBoundingClientRect();
        const menuRect = menu?.getBoundingClientRect();
        return {
          found: !!row,
          text: row?.innerText.replace(/\s+/g, " ").trim(),
          onScreen: !!rect && rect.top >= 0 && rect.bottom <= window.innerHeight,
          rowTop: rect && Math.round(rect.top),
          rowBottom: rect && Math.round(rect.bottom),
          windowHeight: window.innerHeight,
          // Horizontal clipping: the popover inside the window, and no row
          // wider than the box that draws it.
          menuLeft: menuRect && Math.round(menuRect.left),
          menuRight: menuRect && Math.round(menuRect.right),
          windowWidth: window.innerWidth,
          overflowsX: !!menu && menu.scrollWidth > menu.clientWidth + 1,
          rowOverflowsX: !!row && row.scrollWidth > row.clientWidth + 1,
          sheetScrollable: !!sheet && sheet.scrollHeight > sheet.clientHeight + 1,
          sheetScrollTop: sheet && Math.round(sheet.scrollTop),
          sheetScrollMax: sheet && Math.round(sheet.scrollHeight - sheet.clientHeight),
          // Where the LIST's own viewport ends. A row can be on screen while
          // the box it scrolls in still hangs under the window edge, which is
          // what made reaching the graph's keys feel like reading off the floor.
          viewportBottom: Math.round(
            document.querySelector(".qs-vocab-viewport")?.getBoundingClientRect().bottom ?? 0,
          ),
        };
      }, selector);

    /** Bring `selector` into the window by the ONE scroll the layout already
     *  has: the bottom sheet's own. The wheel goes over the sheet's TOP edge,
     *  not over the list, because the list's viewport is itself a scroller and
     *  would eat the delta. Returns the last measurement either way. */
    const scrollIntoWindow = async (selector) => {
      let seen = await measure(selector);
      const settled = () =>
        seen.onScreen
        && seen.viewportBottom <= seen.windowHeight + 1;
      for (let attempt = 0; attempt < 10 && seen.found && !settled(); attempt += 1) {
        if (seen.sheetScrollTop >= seen.sheetScrollMax) break;
        const sheet = await page.locator(".qs-sheet").boundingBox();
        if (!sheet) break;
        await page.mouse.move(
          sheet.x + sheet.width / 2,
          Math.max(1, Math.min(narrow.height - 2, sheet.y + 8)),
        );
        await page.mouse.wheel(0, 160);
        await sleep(150);
        seen = await measure(selector);
      }
      return seen;
    };

    const guard = (seen, what) => {
      if (!seen.found) throw new Error(`${label}: ${what} is not in the list at all`);
      if (seen.overflowsX || seen.rowOverflowsX) {
        throw new Error(`${label}: the picker is cut off horizontally: ${JSON.stringify(seen)}`);
      }
      if (seen.menuLeft < 0 || seen.menuRight > seen.windowWidth) {
        throw new Error(`${label}: the picker hangs outside the window sideways: ${JSON.stringify(seen)}`);
      }
      if (!seen.onScreen) {
        throw new Error(`${label}: ${what} cannot be brought into view: ${JSON.stringify(seen)}`);
      }
      if (!/\d+ (blocks?|pages?)/.test(seen.text ?? "")) {
        throw new Error(`${label}: ${what} is visible but its count is not: ${JSON.stringify(seen.text)}`);
      }
      if (seen.viewportBottom > seen.windowHeight + 1) {
        throw new Error(
          `${label}: the list's viewport still ends under the window edge: ${JSON.stringify(seen)}`,
        );
      }
    };

    // (a) **The unfiltered list, by keyboard.** This is the case the first
    //     screenshot showed and did not test: the graph's own keys sit under
    //     seven built-ins, which on this window is below the fold. Walk to the
    //     first property row the way a keyboard user does, then scroll.
    // The keyboard belongs to the filter box (it is the combobox that carries
    // `aria-activedescendant`), so put the caret there first — which is where a
    // person who just opened the picker already is.
    await page.locator(".qs-menu.qs-vocab .qs-menu-filter").click();
    await sleep(150);
    for (let step = 0; step < 8; step += 1) await page.keyboard.press("ArrowDown");
    await sleep(300);
    const activeSelector = ".qs-vocab-option.active";
    const activeIsProperty = await page.evaluate(
      () => document.querySelector(".qs-vocab-option.active")?.getAttribute("data-section"),
    );
    const walked = await scrollIntoWindow(activeSelector);
    console.log(`${label} keyboard-reached (${activeIsProperty}):`, JSON.stringify(walked));
    if (activeIsProperty !== "property") {
      throw new Error(`${label}: eight ArrowDowns did not reach the graph's own vocabulary`);
    }
    guard(walked, "the first property row, reached by keyboard");
    await shot(page, `query-vocabulary-${label}`);
    // Enter takes it, from exactly where the keyboard is.
    await page.keyboard.press("Enter");
    await page.waitForSelector(".qs-value-editor", { timeout: 6000 });
    await sleep(250);
    const keyboardChose = await page.evaluate(() => ({
      title: document.querySelector(".qs-value-editor .qs-menu-title")?.textContent?.trim(),
      onScreen: (() => {
        const rect = document.querySelector(".qs-value-editor")?.getBoundingClientRect();
        return !!rect && rect.top >= 0 && rect.top < window.innerHeight;
      })(),
    }));
    console.log(`${label} keyboard chose:`, JSON.stringify(keyboardChose));
    if (!keyboardChose.title) {
      throw new Error(`${label}: Enter on the reached row opened no value editor`);
    }
    const contained = await page.locator('.qs-value-editor').evaluate((editor) => {
      const outer = editor.getBoundingClientRect();
      const options = editor.querySelector('[role="listbox"]').getBoundingClientRect();
      return options.top >= outer.top && options.bottom <= outer.bottom;
    });
    if (!contained) throw new Error(`${label}: operator choices detached from their value editor`);
    await shot(page, `query-vocabulary-${label}-chosen`);

    // (b) **The typed route**, which is the other way to reach a key: narrow
    //     the list and press the row.
    await page.keyboard.press("Escape");
    await sleep(200);
    await openPicker(page);
    await page.locator(".qs-menu-filter").fill("status");
    await sleep(350);
    const typedSelector = '.qs-vocab-option[data-vocabulary-key="status"][data-through-page="false"]';
    const typed = await scrollIntoWindow(typedSelector);
    console.log(`${label} typed:`, JSON.stringify(typed));
    guard(typed, "the typed key's row");
    await page.locator(typedSelector).first().click();
    await page.waitForSelector(".qs-value-editor", { timeout: 6000 });
    await sleep(250);
    const chose = await page.evaluate(() => ({
      title: document.querySelector(".qs-value-editor .qs-menu-title")?.textContent?.trim(),
    }));
    console.log(`${label} chose:`, JSON.stringify(chose));
    if (chose.title !== "status") {
      throw new Error(`${label}: choosing the row did not open its value editor: ${JSON.stringify(chose)}`);
    }
    await page.close();
  }

  // --- 5. the pane holding text the engine could not read ------------------
  {
    const page = await openSheet(browser, view, {
      diagnostics: [
        {
          message: "`stauts` is not a field of this query",
          kind: "unknown_ident",
          span: { start: 11, end: 17 },
          suggestions: ["status"],
        },
      ],
    });
    const input = page.locator(".query-text-pane-input");
    await input.waitFor({ timeout: 6000 });
    await input.fill("@block and stauts = 'active'");
    await page.waitForSelector(".query-text-pane-diagnostic", { timeout: 6000 });
    await sleep(400);
    const shown = await page.evaluate(() => ({
      message: document.querySelector(".query-text-pane-diagnostic-message")?.textContent,
      // Said once, never twice: the structured item carries the message and the
      // status line stands down.
      statusLines: document.querySelectorAll(".query-text-pane-error").length,
      locate: !!document.querySelector(".query-text-pane-locate"),
      alternatives: [...document.querySelectorAll(".query-text-pane-diagnostic-alts")].map(
        (el) => el.textContent.replace(/\s+/g, " ").trim(),
      ),
      saveDisabled: document.querySelector(".query-text-pane-save")?.disabled,
      greyed: !!document.querySelector(".qs-sheet-stale"),
    }));
    console.log("invalid pane:", JSON.stringify(shown));
    if (shown.statusLines !== 0) throw new Error("the parser's message is printed twice");
    if (!shown.saveDisabled) throw new Error("an invalid parse left Save enabled");
    if (!shown.greyed) throw new Error("the last-good rows are not marked stale");
    const box = await page.locator(".qs-sheet").boundingBox();
    if (box) await shot(page, "query-text-pane-invalid", around(box, view, 24));
    await page.close();
  }

  // --- 6. the part the sheet cannot draw, and the way into it --------------
  {
    const page = await openSheet(browser, view, { deep: true });
    await page.waitForSelector(".qs-advanced-open", { timeout: 6000 });
    await page.locator(".qs-advanced-open").first().click();
    await sleep(400);
    const focused = await page.evaluate(() =>
      document.activeElement?.classList.contains("query-text-pane-input"),
    );
    console.log("advanced chip focused the pane:", focused);
    if (!focused) throw new Error("the ⟨advanced⟩ row did not send the cursor to the pane");
    const box = await page.locator(".qs-sheet").boundingBox();
    if (box) await shot(page, "query-advanced-pane", around(box, view, 24));
    await page.close();
  }

  // --- 7. the crossing notice, hosted by the pane --------------------------
  {
    const page = await openSheet(browser, view, {});
    const input = page.locator(".query-text-pane-input");
    await input.waitFor({ timeout: 6000 });
    await input.fill("@block and task = 'DOING' and prop('cost') > 100");
    await page.waitForFunction(
      () => document.querySelector(".query-text-pane-save")?.disabled === false,
      { timeout: 6000 },
    );
    await page.locator(".query-text-pane-save").click();
    await page.waitForSelector(".query-crossing-notice", { timeout: 8000 });
    await sleep(400);
    const hosted = await page.evaluate(() => ({
      copies: document.querySelectorAll(".query-crossing-notice").length,
      insidePane: !!document.querySelector(".query-text-pane .query-crossing-notice"),
      changed: document
        .querySelector(".query-crossing-notice-changed code")
        ?.textContent?.slice(0, 60),
    }));
    console.log("crossing notice:", JSON.stringify(hosted));
    if (hosted.copies !== 1) throw new Error(`the notice is drawn ${hosted.copies} times`);
    if (!hosted.insidePane) throw new Error("the notice is not hosted by the open pane");
    const box = await page.locator(".qs-sheet").boundingBox();
    if (box) await shot(page, "query-crossing-notice", around(box, view, 24));
    await page.close();
  }

  await browser.close();
  console.log("DONE");
} catch (e) {
  console.log("ERROR:", String(e).split("\n").slice(0, 8).join(" | "));
  process.exitCode = 2;
} finally {
  server.kill("SIGTERM");
}
