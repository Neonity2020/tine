// Verify P1 block-render virtualization (lazy body parse/render) on a large page.
// Loads the mock's gated 2000-block "Big" page (?big), then asserts:
//   1. On load, most blocks are DEFERRED raw-text placeholders (.ast-deferred) and
//      only the near-viewport ones have rendered heavy constructs (tables/code/math)
//      — i.e. off-screen blocks were never parsed/rendered (the win).
//   2. Scrolling to the bottom renders the bottom blocks on demand.
//   3. Render-once-keep: blocks that rendered stay rendered after a scroll round-trip
//      (no re-deferral, so no scroll-height churn).
// Also dumps window.__tineParseStats if present (only in a dev build).
//
// Usage:  source scripts/env.sh && npm run build && node scripts/shot-virtualize.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5199;
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore" });
async function waitForServer(url, tries = 60) {
  for (let i = 0; i < tries; i++) {
    try { if ((await fetch(url)).ok) return; } catch {}
    await sleep(250);
  }
  throw new Error("server did not start");
}

const count = (page, sel) => page.evaluate((s) => document.querySelectorAll(s).length, sel);
let failed = false;
const check = (name, cond, detail) => {
  console.log(`${cond ? "PASS" : "FAIL"}  ${name}${detail ? "  — " + detail : ""}`);
  if (!cond) failed = true;
};

try {
  await waitForServer(`http://localhost:${PORT}/`);
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 900, height: 900 } });
  const errors = [];
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  page.on("pageerror", (e) => errors.push(String(e)));

  // ?big makes the mock expose the 2000-block page; reach it via the quick switcher.
  await page.goto(`http://localhost:${PORT}/?big`);
  await page.waitForSelector(".ls-block", { timeout: 5000 });
  await page.keyboard.press("Control+k");
  await page.waitForSelector(".switcher-input", { timeout: 3000 });
  await page.locator(".switcher-input").fill("Big");
  await sleep(400);
  await page.locator(".switcher-row").first().click();
  await page.waitForSelector(".ls-block", { timeout: 3000 });
  await sleep(700); // let the near-viewport blocks render + IO settle

  const totalBlocks = await count(page, ".ls-block");
  const deferred0 = await count(page, ".ast-deferred");
  const heavy0 = (await count(page, ".md-table")) + (await count(page, ".code-block")) + (await count(page, ".katex"));
  console.log(`blocks=${totalBlocks} deferred0=${deferred0} heavy0=${heavy0}`);
  await page.screenshot({ path: "screenshots/virtualize-top.png" });

  check("large page loaded", totalBlocks > 1500, `${totalBlocks} blocks`);
  check("most blocks deferred on load", deferred0 > 1200, `${deferred0} deferred`);
  check("only near-viewport heavy constructs rendered", heavy0 < 250, `${heavy0} heavy`);

  // The top-of-page block is always inside the near-zone, so it must be rendered.
  // (We don't assert a fixed COUNT of rendered blocks — how many fit the near-zone
  // depends on block heights; the point is that the vast majority are deferred.)
  const firstDeferred = await page.evaluate(() => {
    const first = document.querySelector(".ls-block");
    return first ? !!first.querySelector(".ast-deferred") : true;
  });
  check("top-of-page block rendered (not deferred)", firstDeferred === false);

  // Scroll to the bottom; the bottom blocks should render on demand.
  await page.evaluate(() => {
    const sc = document.querySelector(".main-content");
    if (sc) sc.scrollTop = sc.scrollHeight;
  });
  await sleep(800);
  const deferredBottom = await count(page, ".ast-deferred");
  const heavyBottom = (await count(page, ".md-table")) + (await count(page, ".code-block")) + (await count(page, ".katex"));
  const lastDeferred = await page.evaluate(() => {
    const blocks = document.querySelectorAll(".ls-block");
    const last = blocks[blocks.length - 1];
    return last ? !!last.querySelector(".ast-deferred") : true;
  });
  console.log(`deferredBottom=${deferredBottom} heavyBottom=${heavyBottom} lastDeferred=${lastDeferred}`);
  await page.screenshot({ path: "screenshots/virtualize-bottom.png" });

  check("last block rendered after scroll", lastDeferred === false);
  check("more heavy constructs rendered after scrolling down", heavyBottom > heavy0, `${heavy0} → ${heavyBottom}`);

  // Render-once-keep: scroll back to top. Nothing that rendered may revert to a
  // placeholder, so the deferred count must only ever go DOWN (never re-deferred) —
  // this is the invariant that guarantees zero scroll-height churn on re-entry.
  await page.evaluate(() => {
    const sc = document.querySelector(".main-content");
    if (sc) sc.scrollTop = 0;
  });
  await sleep(500);
  const deferredFinal = await count(page, ".ast-deferred");
  console.log(`deferredFinal=${deferredFinal}`);
  check("render-once-keep: deferred count never increases (no re-deferral)", deferredFinal <= deferredBottom, `${deferred0} → ${deferredBottom} → ${deferredFinal}`);

  const stats = await page.evaluate(() => window.__tineParseStats ?? null);
  console.log(stats ? `parseStats=${JSON.stringify(stats)}` : "parseStats unavailable (production build — DEV counter stripped)");

  console.log(errors.length ? "CONSOLE ERRORS:\n" + errors.join("\n") : "no console errors");
  await browser.close();
  server.kill("SIGKILL");
  process.exit(failed ? 1 : 0);
} catch (e) {
  console.error(String(e));
  server.kill("SIGKILL");
  process.exit(1);
}
