import { waitForHttpServer } from "./e2e-capabilities.mjs";
// Screenshot the two states of the visual query builder (SPEC §7.2–§7.4), so
// they can be LOOKED at rather than inferred from selectors:
//
//   query-sentence.png  the resting line — one plain-English sentence, the
//                       result count, a ⚙ — with the blocks under it visible,
//                       which is the whole point of not having a chip bar.
//   query-sheet.png     the same query open: the anchor line, the rows, the
//                       add-condition button, the footer's sort/summarize/text
//                       pane, over the scrim.
//   query-sheet-narrow.png  the same sheet at 560px, docked to the bottom edge.
//
// Headless Chromium over the mock backend (the "Jun 14th, 2026" journal has a
// pure {{query}} block).
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5271;
const OUT = "screenshots";
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });

async function shot(page, name, clip) {
  await page.screenshot({ path: `${OUT}/${name}.png`, ...(clip ? { clip } : { fullPage: false }) });
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

try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 40, 250, { failureMessage: "server did not start" });
  const browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
  const view = { width: 1200, height: 1000 };
  const page = await browser.newPage({ viewport: view, deviceScaleFactor: 2 });
  page.on("pageerror", (e) => console.log("pageerror:", String(e).split("\n")[0]));

  // The browser mock has no engine, so without a fixture every query reads back
  // as ONE retained raw row — honest, but not what a reader needs to see. Install
  // the canned parse/print through the same mock-only seam `shot-improve.mjs`
  // uses for its diff panel (`src/mockQueryFixture.guard.test.ts` asserts no
  // production module can reach it), so these images show a real sentence over
  // real rows.
  await page.addInitScript(() => {
    const text = (t) => ({ kind: "text", text: t });
    const attr = (a, op, value) => ({ kind: "leaf", leaf: { kind: "attr", attr: a, op, value } });
    const rel = (r, pred) => ({ kind: "leaf", leaf: { kind: "rel", rel: r, quant: "any", pred } });
    globalThis.__tineMockQueryFixture = {
      print: "@block and task in ('TODO', 'DOING') and page.name = 'Project/Roadmap'"
        + " and any(props, key = 'owner' and value = 'Ada')",
      parse: {
        query: {
          anchor: "block",
          filter: {
            kind: "and",
            items: [
              attr("task", "in", { kind: "list", items: [text("TODO"), text("DOING")] }),
              rel("page", attr("name", "eq", text("Project/Roadmap"))),
              rel("props", {
                kind: "and",
                items: [attr("key", "eq", text("owner")), attr("value", "eq", text("Ada"))],
              }),
            ],
          },
          diagnostics: [],
          source: { kind: "tql", original: "@block and task in ('TODO', 'DOING')" },
        },
        view: {},
      },
    };
  });

  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForSelector(".page-title", { timeout: 8000 });
  await page.waitForSelector(".qs-sentence", { timeout: 8000 });
  await sleep(600);

  // --- 1. at rest -----------------------------------------------------------
  const block = page.locator(".query-block").filter({ has: page.locator(".qs-sentence") }).first();
  await block.scrollIntoViewIfNeeded();
  await sleep(250);
  const sentence = await page.locator(".qs-sentence").first().innerText();
  console.log("resting sentence:", JSON.stringify(sentence));
  const restingBox = await block.boundingBox();
  if (restingBox) {
    // Include a slice of what follows: the resting state must not push the
    // page around.
    await shot(page, "query-sentence", {
      x: Math.max(0, restingBox.x - 16),
      y: Math.max(0, restingBox.y - 16),
      width: Math.min(view.width - Math.max(0, restingBox.x - 16), restingBox.width + 32),
      height: Math.min(view.height - Math.max(0, restingBox.y - 16), restingBox.height + 220),
    });
  }

  // --- 2. open --------------------------------------------------------------
  await page.locator(".qs-gear").first().click();
  await page.waitForSelector(".qs-sheet", { timeout: 6000 });
  await sleep(400);
  const anchor = await page.locator(".qs-anchor").first().innerText().catch(() => "(none)");
  const rows = await page.locator(".qs-sheet .qs-row").count();
  const footer = await page.locator(".qs-footer").count();
  console.log(`sheet: anchor=${JSON.stringify(anchor.replace(/\n/g, " "))} rows=${rows} footer=${footer}`);
  const sheetBox = await page.locator(".qs-sheet").boundingBox();
  if (sheetBox) await shot(page, "query-sheet", around(sheetBox, view, 24));

  // --- 3. the same sheet on a phone ----------------------------------------
  // Close by pressing the scrim, which is what a user does; Escape depends on
  // where focus happens to be in a headless page.
  await page.locator(".qs-overlay").click({ position: { x: 5, y: 5 } });
  await page.waitForSelector(".qs-sheet", { state: "detached", timeout: 5000 });
  await page.setViewportSize({ width: 560, height: 900 });
  await sleep(500);
  // A phone-width viewport brings its own chrome: the navigation drawer opens
  // over the left half and the first-run Guide toast sits on the bottom edge —
  // exactly where the sheet is about to dock. Dismiss both so the picture is of
  // the sheet.
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
  await sleep(400);
  await page.locator(".qs-gear").first().scrollIntoViewIfNeeded();
  // Dispatch rather than hit-test: on a narrow viewport the phone chrome can sit
  // over the block, and this script is here to photograph the sheet, not to
  // re-prove that the ⚙ is clickable (`scripts/e2e-query-sheet.mjs` does that on
  // the real engine).
  await page.locator(".qs-gear").first().dispatchEvent("click");
  await page.waitForSelector(".qs-sheet", { timeout: 6000 });
  await sleep(400);
  const docked = await page.evaluate(() => {
    const rect = document.querySelector(".qs-sheet").getBoundingClientRect();
    return {
      left: Math.round(rect.left),
      right: Math.round(window.innerWidth - rect.right),
      bottomGap: Math.round(window.innerHeight - rect.bottom),
      width: Math.round(rect.width),
      innerWidth: window.innerWidth,
    };
  });
  console.log("narrow sheet geometry:", JSON.stringify(docked));
  // This is also the ONE place the `max-width: 600px` bottom-sheet rule is
  // exercised end to end: the desktop window declares `minWidth: 640`
  // (src-tauri/tauri.conf.json), so the native journey can never reach the
  // breakpoint. Assert, don't just photograph.
  if (docked.left > 2 || docked.right > 2 || docked.bottomGap > 2) {
    throw new Error(`the narrow sheet is not docked full width to the bottom edge: ${JSON.stringify(docked)}`);
  }
  await shot(page, "query-sheet-narrow");

  await browser.close();
  console.log("DONE");
} catch (e) {
  console.log("ERROR:", String(e).split("\n").slice(0, 8).join(" | "));
  process.exitCode = 2;
} finally {
  server.kill("SIGTERM");
}
