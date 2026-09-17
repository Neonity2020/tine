// GH #473 RESEARCH HARNESS (research-only; NOT a shipped gate, NOT registered
// in any e2e contract). It exists to reproduce and MEASURE the reported
// iPadOS geometry failure: a query-table sheet nested under a bullet keeps its
// left edge at the bullet's indentation while it fits, but an OVERFLOWING
// table shifts its left edge outside that indentation instead of scrolling
// inside its own container.
//
// It drives the REAL built frontend (mock backend) through the literal user
// path: navigate to a routed page, edit an existing {{query}} block into
// `{{query …}} + tine.view:: table + tine.fields:: …` (the reporter's fixture
// shape), Tab-indent it under its previous sibling, and measure the DOM/CSS
// geometry of the resulting sheet against the bullet's indentation.
//
// Engines: chromium (the bench/browser engine) and webkit (WebKitGTK — the
// closest available engine to iPadOS WebKit on this Linux host; still NOT the
// same port, see the report's platform boundary).
//
// Usage: npm run build && node scripts/probe-query-overflow-473.mjs
import { waitForHttpServer } from "./e2e-capabilities.mjs";
import { chromium, webkit } from "./lib/playwright.mjs";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";

const PORT = 5313;
const OUT_DIR = "/tmp/tine-473";
fs.mkdirSync(OUT_DIR, { recursive: true });

// The reporter's fixture shape: a task query presented as a TABLE with enough
// declared columns to overflow any realistic pane. Column tracks are
// `fit-content(320px)`, so long cell values pin every column at its cap.
const WIDE_FIELDS = [
  "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
].map((name) => `${name}=text`).join(";");
const LONG = (name) => `${name} ${name} ${name} ${name} ${name} ${name} ${name} ${name} value`;

const QUERY_RAW_WIDE = `{{query (todo TODO DOING)}}\ntine.view:: table\ntine.fields:: ${WIDE_FIELDS}`;
const QUERY_RAW_NARROW = `{{query (todo TODO DOING)}}\ntine.view:: table\ntine.fields:: ${WIDE_FIELDS}\ntine.columns:: alpha`;

/** The canned query answers (the browser mock has no engine; this seam is
 *  mock-only — src/mockQueryFixture.guard.test.ts asserts production never
 *  reads it). Rows carry the declared fields as long property values so every
 *  column saturates its fit-content cap. */
const QUERY_FIXTURE = () => ({
  parse: {
    query: {
      anchor: "block",
      filter: { kind: "raw", text: "(todo TODO DOING)", diagnostic_kind: "not_applicable" },
      diagnostics: [],
      source: { kind: "og", original: "(todo TODO DOING)", og_options: "" },
    },
    view: {},
  },
  run: {
    anchor: "block",
    groups: [{
      page: "Jun 14th, 2026",
      kind: "page",
      blocks: ["one", "two", "three"].map((id, i) => ({
        id: `473-${id}`,
        raw: `TODO row ${i + 1} of the overflow fixture\n` + WIDE_FIELDS
          .split(";")
          .map((f) => f.split("=")[0])
          .map((f) => `${f}:: ${LONG(f)}`)
          .join("\n"),
        collapsed: false,
        children: [],
        marker: "TODO",
        properties: WIDE_FIELDS.split(";").map((f) => [f.split("=")[0], LONG(f)]),
      })),
    }],
    diagnostics: [],
    report: { ran: [], ignored: [], supported: true },
    total: 3,
    exceeded: false,
  },
});

const STARTUP_NOTICES = [
  "New: in-app Guide — learn Sheets, formulas & queries.",
  "Tine did not close cleanly last time. A privacy-safe diagnostic report is available.",
];
const STARTUP_NOTICE_PREFIXES = ["Software rendering is on ("];

async function dismissStartupNotices(page) {
  for (let attempt = 0; attempt < 10; attempt += 1) {
    const message = await page.evaluate(
      ([exact, prefixes]) => {
        const known = (text) => exact.includes(text) || prefixes.some((p) => text.startsWith(p));
        for (const toast of document.querySelectorAll(".toast")) {
          const text = toast.querySelector(".toast-msg")?.textContent?.trim() ?? "";
          if (!known(text)) continue;
          toast.querySelector(".toast-close")?.click();
          return text;
        }
        return null;
      },
      [STARTUP_NOTICES, STARTUP_NOTICE_PREFIXES],
    );
    if (message === null) return;
    console.log(`  dismissed startup notice: ${message}`);
    await sleep(150);
  }
}

async function openSheetsDemo(page) {
  await page.keyboard.press("Control+k");
  await page.waitForSelector(".switcher-input", { timeout: 4000 });
  await page.locator(".switcher-input").fill("Jun 14th, 2026");
  await sleep(350);
  await page.locator(".switcher-row").first().click();
  await page.waitForSelector(".page-title", { timeout: 5000 });
  await sleep(400);
}

/** Put the query block into edit mode: a mousedown on its macro-host
 *  `.block-content` (the handler at Block.tsx:885), dispatched directly so a
 *  hit-test over the query UI cannot intercept it — the same pattern
 *  QueryMacro.test.tsx uses to start editing a query block. Resolved via
 *  `.closest()`: once the query is nested, ancestor block-contents also
 *  contain it as a descendant, and the first match would edit the parent. */
async function editQueryRaw(page, raw) {
  await page.evaluate(() => {
    document.querySelector(".query-block")?.closest(".block-content")
      ?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0 }));
  });
  const editor = page.locator("textarea.block-editor").first();
  await editor.waitFor({ timeout: 4000 });
  await editor.fill(raw);
  await page.keyboard.press("Escape");
  await sleep(350);
}

/** Indent the query block under its previous sibling with Tab, the way a user
 *  does. The block must be in edit mode for the keybinding to apply; the
 *  editor is focused explicitly because a dispatched mousedown does not
 *  always leave focus on the textarea at narrow widths (at 744px the Tab then
 *  went to the document and silently no-op'd). */
async function indentQueryBlock(page) {
  await page.evaluate(() => {
    document.querySelector(".query-block")?.closest(".block-content")
      ?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0 }));
  });
  const editor = page.locator("textarea.block-editor").first();
  await editor.waitFor({ timeout: 4000 });
  await editor.focus();
  await page.keyboard.press("Tab");
  await page.keyboard.press("Escape");
  await sleep(400);
}

const MEASURE = () => {
  const queryLsBlock = document.querySelector(".query-block")?.closest(".ls-block");
  const sheetContainer = queryLsBlock?.querySelector(".block-sheet-container");
  if (!sheetContainer || !queryLsBlock) return { error: "query sheet not found" };
  const parent = queryLsBlock.closest(".block-children")?.closest(".ls-block") ?? null;
  const sheetScroll = sheetContainer.querySelector(".sheet-scroll");
  const sheetTable = sheetContainer.querySelector(".sheet-table");
  const style = getComputedStyle(sheetContainer);
  const r = (el) => el ? el.getBoundingClientRect().toJSON() : null;
  return {
    viewport: { width: window.innerWidth, height: window.innerHeight },
    parentBulletLeft: parent ? Math.round(parent.querySelector(".block-main").getBoundingClientRect().left) : null,
    queryBulletLeft: Math.round(queryLsBlock.querySelector(".block-main").getBoundingClientRect().left),
    sheet: {
      left: Math.round(sheetContainer.getBoundingClientRect().left),
      width: Math.round(sheetContainer.getBoundingClientRect().width),
      marginLeft: style.marginLeft,
      breakout: sheetContainer.classList.contains("sheet-breakout"),
      shiftVar: style.getPropertyValue("--sheet-breakout-shift").trim(),
      widthVar: style.getPropertyValue("--sheet-breakout-width").trim(),
    },
    scroll: sheetScroll ? {
      clientWidth: sheetScroll.clientWidth,
      scrollWidth: sheetScroll.scrollWidth,
      overflowing: sheetScroll.scrollWidth > sheetScroll.clientWidth + 1,
    } : null,
    table: sheetTable ? {
      left: Math.round(sheetTable.getBoundingClientRect().left),
      width: Math.round(sheetTable.getBoundingClientRect().width),
    } : null,
    documentOverflow: document.documentElement.scrollWidth - document.documentElement.clientWidth,
    mainContent: (() => {
      const main = document.querySelector(".main-content");
      const rect = main?.getBoundingClientRect();
      return rect ? { left: Math.round(rect.left), right: Math.round(rect.right), width: Math.round(rect.width) } : null;
    })(),
  };
};

async function measure(page) {
  await sleep(600);
  return page.evaluate(MEASURE);
}

function verdict(m) {
  if (m.error) return `ERR`;
  const tableLeft = m.table?.left;
  // The reported invariant: the table's left edge must not sit LEFT of the
  // bullet that contains the query. The sheet container is allowed its own
  // 28px indent, but never a left edge outside the parent's content.
  const ok = tableLeft !== undefined && m.parentBulletLeft !== null
    ? tableLeft >= m.parentBulletLeft - 1
    : null;
  return ok === null ? "?" : ok ? "ALIGNED" : "MISALIGNED-LEFT";
}

async function runEngine(engineName, launchEngine, serverPort) {
  const browser = await launchEngine({ args: engineName === "chromium" ? ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] : [] });
  const results = [];
  try {
    for (const width of [1366, 1194, 1024, 834, 744, 640]) {
      const page = await browser.newPage({ viewport: { width, height: 820 }, deviceScaleFactor: 1 });
      page.on("pageerror", (e) => console.log(`  pageerror: ${String(e).split("\n")[0]}`));
      page.on("console", (m) => { if (m.type() === "error") console.log(`  console: ${m.text().slice(0, 200)}`); });
      await page.addInitScript((fixture) => {
        globalThis.__tineMockQueryFixture = fixture;
      }, QUERY_FIXTURE());
      await page.goto(`http://localhost:${serverPort}/`);
      await page.waitForSelector(".ls-block", { timeout: 8000 });
      await sleep(600);
      await dismissStartupNotices(page);
      // A phone-width viewport brings the navigation drawer over the page.
      await page.evaluate(() => {
        document.querySelector("[data-mobile-drawer-scrim]")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      });
      await openSheetsDemo(page);

      // 1. the reporter's fixture: a query TABLE with overflowing columns,
      //    nested under its previous sibling bullet.
      await editQueryRaw(page, QUERY_RAW_WIDE);
      try {
        await page.waitForSelector(".query-block .block-sheet-container .sheet-table", { timeout: 8000 });
      } catch (e) {
        const state = await page.evaluate(() => ({
          title: document.querySelector(".page-title")?.textContent,
          queryBlocks: document.querySelectorAll(".query-block").length,
          sheetContainers: document.querySelectorAll(".block-sheet-container").length,
          querySheetContainers: document.querySelectorAll(".query-block .block-sheet-container").length,
          sheetTables: document.querySelectorAll(".sheet-table").length,
          headerText: document.querySelector(".query-block .query-header")?.textContent?.slice(0, 90),
        }));
        console.log(`  STATE DUMP [${engineName} ${width}]: ${JSON.stringify(state)}`);
        await page.screenshot({ path: `${OUT_DIR}/${engineName}-${width}-FAILED.png`, fullPage: true });
        throw e;
      }
      await indentQueryBlock(page);
      try {
        await page.waitForSelector(".query-block .block-sheet-container .sheet-table", { timeout: 8000 });
      } catch (e) {
        const state = await page.evaluate(() => ({
          queryBlocks: document.querySelectorAll(".query-block").length,
          querySheetContainers: document.querySelectorAll(".query-block .block-sheet-container").length,
          nested: !!document.querySelector(".query-block")?.closest(".block-children"),
          headerText: document.querySelector(".query-block .query-header")?.textContent?.slice(0, 90),
          blockTexts: [...document.querySelectorAll(".ls-block > .block-main .block-content")].map((e) => (e.textContent || "").slice(0, 40)),
          toasts: [...document.querySelectorAll(".toast-msg")].map((e) => e.textContent?.slice(0, 80)),
        }));
        console.log(`  STATE DUMP (post-indent) [${engineName} ${width}]: ${JSON.stringify(state)}`);
        await page.screenshot({ path: `${OUT_DIR}/${engineName}-${width}-FAILED-postindent.png`, fullPage: true });
        throw e;
      }
      const wide = await measure(page);
      await page.screenshot({ path: `${OUT_DIR}/${engineName}-${width}-overflow.png`, fullPage: false });

      // 2. the control: the same nested query table with ONE column — the
      //    reporter's "when there is no overflow it displays as intended".
      await editQueryRaw(page, QUERY_RAW_NARROW);
      await page.waitForSelector(".query-block .block-sheet-container .sheet-table", { timeout: 6000 });
      const narrow = await measure(page);
      await page.screenshot({ path: `${OUT_DIR}/${engineName}-${width}-fits.png`, fullPage: false });

      results.push({ engine: engineName, width, wide, narrow });
      console.log(
        `[${engineName} ${width}] fits: ${verdict(narrow)} | overflow: ${verdict(wide)}`
        + ` (tableLeft=${wide.table?.left} queryBulletLeft=${wide.queryBulletLeft} parentBulletLeft=${wide.parentBulletLeft}`
        + ` breakout=${wide.sheet?.breakout} shift=${wide.sheet?.shiftVar} scrollOverflow=${wide.scroll?.scrollWidth}>${wide.scroll?.clientWidth}`
        + ` docOverflow=${wide.documentOverflow})`,
      );
      await page.close();
    }
  } finally {
    await browser.close();
  }
  return results;
}

const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });
try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 60, 250, { failureMessage: "server did not start" });
  const all = [];
  all.push(...await runEngine("chromium", (opts) => chromium.launch({ headless: true, ...opts }), PORT));
  try {
    all.push(...await runEngine("webkit", (opts) => webkit.launch({ ...opts }), PORT));
  } catch (e) {
    console.log(`webkit unavailable on this host (${String(e).split("\n")[0]}); recording the gap.`);
  }
  fs.writeFileSync(`${OUT_DIR}/results.json`, JSON.stringify(all, null, 2));
  console.log(`wrote ${OUT_DIR}/results.json`);
} catch (e) {
  console.error(String(e));
  process.exitCode = 1;
} finally {
  server.kill("SIGKILL");
}
