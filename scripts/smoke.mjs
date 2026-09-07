import { waitForHttpServer } from "./e2e-capabilities.mjs";
// Headless render smoke test against the mock backend. Catches runtime crashes
// in the live sidebar/query paths (circular-import TDZ, render throws, etc.).
// Usage: node scripts/smoke.mjs   (requires `npm run build` first)
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5193;
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], {
  stdio: "ignore",
});

const fail = (m) => {
  console.error("SMOKE FAIL:", m);
  server.kill("SIGKILL");
  process.exit(1);
};


const errors = [];
try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 60, 250, { failureMessage: "server did not start" });
  const browser = await chromium.launch();
  const page = await browser.newPage();
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  page.on("pageerror", (e) => errors.push(String(e)));

  // The mock has no engine: without a fixture every query reads back as ONE raw
  // row. Install a canned parse/print through the mock-only seam
  // (`src/mockQueryFixture.guard.test.ts` keeps it out of production) so the
  // sentence and the sheet below are exercised over real rows.
  await page.addInitScript(() => {
    const text = (t) => ({ kind: "text", text: t });
    const attr = (a, op, value) => ({ kind: "leaf", leaf: { kind: "attr", attr: a, op, value } });
    globalThis.__tineMockQueryFixture = {
      print: "@block and task in ('TODO', 'DOING')",
      parse: {
        query: {
          anchor: "block",
          filter: attr("task", "in", { kind: "list", items: [text("TODO"), text("DOING")] }),
          diagnostics: [],
          source: { kind: "tql", original: "@block and task in ('TODO', 'DOING')" },
        },
        view: {},
      },
    };
  });
  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForSelector(".ls-block, .page-loading", { timeout: 5000 });
  await sleep(800); // let the feed + queries render

  // 1) The journals feed rendered some blocks.
  const blocks = await page.locator(".ls-block").count();
  if (blocks === 0) fail("no blocks rendered in the feed");

  // 2) A {{query}} rendered (header present) and its collapse triangle works.
  const qHeader = await page.locator(".query-header").count();
  if (qHeader === 0) fail("no query block rendered");
  await page.locator(".query-collapse").first().click();
  await sleep(150);

  // 2b) The standalone query block rests as ONE sentence; opening it gives a
  //     sheet of rows, and the add-condition chooser opens and takes a pick
  //     without a runtime error. What the pick WRITES is not asserted here: the
  //     browser mock has no engine, so every save re-parses to the same canned
  //     reading (the fixture below); the real add → save → reopen round trip is
  //     `scripts/e2e-query-sheet.mjs` on the native app. (Before the fixture
  //     seam existed this step was red at the base commit — the mock refused
  //     every print — so it proved nothing; now it proves the sheet's plumbing
  //     does not throw.)
  const sentence = await page.locator(".qs-sentence").count();
  if (sentence === 0) fail("the query sentence did not render");
  const sentenceText = await page.locator(".qs-sentence").first().innerText();
  if (!/^(All|Blocks|Pages)\b/.test(sentenceText.trim()))
    fail(`the sentence does not read as one: ${JSON.stringify(sentenceText)}`);
  await page.locator(".qs-gear").first().click();
  await page.waitForSelector(".qs-sheet", { timeout: 2000 });
  const rowsBefore = await page.locator(".qs-sheet .qs-row").count();
  if (rowsBefore === 0) fail("the sheet drew no rows for the existing query");
  await page.locator(".qs-add").first().click();
  await page.waitForSelector(".qs-menu", { timeout: 2000 });
  const scheduledOption = await page.locator(".qs-menu .qs-option", { hasText: "Scheduled" }).count();
  if (scheduledOption === 0) fail("the add-condition chooser does not offer 'Scheduled'");
  await page.locator(".qs-menu .qs-option", { hasText: "Scheduled" }).first().click();
  await sleep(250);
  if (errors.length) fail(`adding a condition threw: ${errors.join(" | ")}`);
  const rowsAfter = await page.locator(".qs-sheet .qs-row").count();
  if (rowsAfter === 0) fail("the sheet lost its rows after a pick");
  // Back to rest: Escape peels the chooser, then the sheet, leaving the sentence.
  await page.keyboard.press("Escape");
  await sleep(150);
  if ((await page.locator(".qs-sheet").count()) > 0) {
    await page.keyboard.press("Escape");
    await sleep(150);
  }
  if ((await page.locator(".qs-sheet").count()) > 0) fail("Escape did not close the sheet");
  const restingAfter = await page.locator(".qs-sentence").first().innerText();
  if (!/^(All|Blocks|Pages)\b/.test(restingAfter.trim()))
    fail(`the resting sentence did not survive the sheet: ${JSON.stringify(restingAfter)}`);

  // 3) Shift-click a bullet opens that block LIVE in the right sidebar — i.e.
  //    the editable <Block> (with a .block-content-wrapper + collapse toggle),
  //    NOT the old read-only .ref-block snapshot. That structural difference is
  //    exactly the "live & editable" guarantee. (Actually driving the editor
  //    open needs a real focus and can't be asserted headlessly.)
  await page.locator(".page-blocks .bullet-container").first().click({ modifiers: ["Shift"] });
  await page.waitForSelector(".right-sidebar", { timeout: 3000 });
  await sleep(400);
  const sidebarBlocks = await page.locator(".right-sidebar .ls-block").count();
  if (sidebarBlocks === 0) fail("sidebar opened but rendered no block");
  const editableStruct = await page.locator(".right-sidebar .block-content-wrapper").count();
  const readonlyStruct = await page.locator(".right-sidebar .ref-block").count();
  console.log(`DIAG: sidebar ls-block=${sidebarBlocks} editableWrappers=${editableStruct} readonlyRefBlocks=${readonlyStruct} errs=${errors.length}`);
  if (errors.length) console.log("ERRORS:\n" + errors.join("\n"));
  await page.screenshot({ path: "screenshots/smoke.png", fullPage: true }).catch(() => {});
  if (editableStruct === 0) fail("sidebar rendered a read-only block, not the live editable <Block>");

  await browser.close();
  server.kill("SIGKILL");
  if (errors.length) {
    console.error("SMOKE FAIL: console/page errors:\n" + errors.join("\n"));
    process.exit(1);
  }
  console.log(`SMOKE OK: feed=${blocks} blocks, query rendered+collapsible, query sentence + sheet + add-condition chooser work, sidebar renders live editable <Block>`);
  process.exit(0);
} catch (e) {
  fail(String(e));
}
