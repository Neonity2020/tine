// GH #369 — useless pane scrollbars: reproduce the reporter's "dashboard"
// (multiple short split panes) in headless Chromium against `vite preview`,
// measure each pane's scroll geometry, and screenshot it. jsdom cannot prove
// CSS layout; this harness is the observation boundary for the regression.
//
// Verdict contract (exit 1 = defect reproduced):
//   * a pane whose real content fits its scroller must have NO vertical
//     scrollbar (scrollable range == 0 and no classic scrollbar track width);
//   * a pane whose content overflows must still scroll independently;
//   * scrolled to the end of a long pane, the last block is fully on screen
//     with breathing room below it in [25%, 60%] of the pane's own height —
//     the editing slack must be proportional to the PANE, not the window.
//
// Usage: npm run build && source scripts/env.sh && node scripts/shot-pane-slack.mjs
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5299;
const OUT_BOARD = "/tmp/shot-pane-slack.png";
const OUT_SCROLLED = "/tmp/shot-pane-slack-long-scrolled.png";

const server = spawn(
  "npx",
  ["vite", "preview", "--port", String(PORT), "--strictPort", "--configLoader", "runner"],
  { stdio: "ignore" },
);

async function waitForServer(url, tries = 80) {
  for (let i = 0; i < tries; i++) {
    try {
      if ((await fetch(url)).ok) return;
    } catch {
      // not up yet
    }
    await sleep(250);
  }
  throw new Error("server did not start");
}

async function openPageInFocusedPane(page, name) {
  await page.keyboard.press("Control+k");
  await page.waitForSelector(".switcher-input", { timeout: 4000 });
  await page.locator(".switcher-input").fill(name);
  await sleep(400);
  // Enter, not a row click: the switcher overlay sits over another pane, and a
  // pointerdown there would re-target pane focus to the pane beneath it (the
  // capture-phase pane-focus rule), opening the page in the WRONG quadrant.
  await page.keyboard.press("Enter");
  await sleep(250);
}

async function measurePanes(page) {
  return page.evaluate(() => {
    const out = [];
    for (const leaf of document.querySelectorAll(".pane-leaf")) {
      const scroller = leaf.querySelector(".main-content");
      if (!scroller) continue;
      scroller.scrollTop = 0;
      const sr = scroller.getBoundingClientRect();
      let contentBottom = 0;
      for (const block of leaf.querySelectorAll(".ls-block")) {
        const br = block.getBoundingClientRect();
        if (br.bottom > contentBottom) contentBottom = br.bottom;
      }
      out.push({
        title:
          leaf.querySelector(".tab-bar .tab.active .tab-title")?.textContent?.trim() ??
          leaf.querySelector(".tab-bar .tab-title")?.textContent?.trim() ??
          "?",
        rect: { x: sr.x, y: sr.y, w: sr.width, h: sr.height },
        clientHeight: scroller.clientHeight,
        scrollHeight: scroller.scrollHeight,
        range: scroller.scrollHeight - scroller.clientHeight,
        // A visible classic scrollbar consumes layout width on this axis.
        scrollbarW: scroller.offsetWidth - scroller.clientWidth,
        // Bottom of the deepest block measured from the scroller's top edge
        // (scroller is at scrollTop 0), i.e. the real content extent.
        contentEnd: contentBottom - sr.top,
      });
    }
    return out;
  });
}

async function measureEndSlack(page, leafRect) {
  return page.evaluate((want) => {
    for (const leaf of document.querySelectorAll(".pane-leaf")) {
      const scroller = leaf.querySelector(".main-content");
      if (!scroller) continue;
      const sr = scroller.getBoundingClientRect();
      if (Math.abs(sr.x - want.x) > 2 || Math.abs(sr.y - want.y) > 2) continue;
      let last = null;
      let maxBottom = -Infinity;
      for (const block of leaf.querySelectorAll(".ls-block")) {
        const br = block.getBoundingClientRect();
        if (br.bottom > maxBottom) {
          maxBottom = br.bottom;
          last = br;
        }
      }
      if (!last) return null;
      return {
        lastBlockBottomVisible:
          last.bottom <= sr.bottom + 1 && last.bottom >= sr.top,
        endGap: sr.bottom - last.bottom, // px of empty space below the final block
        clientHeight: scroller.clientHeight,
      };
    }
    return null;
  }, leafRect);
}

try {
  await waitForServer(`http://localhost:${PORT}/`);
  const browser = await chromium.launch({
    args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"],
  });
  // Reporter's window (GH #369 screenshot): 1920x1050.
  const page = await browser.newPage({
    viewport: { width: 1920, height: 1050 },
    deviceScaleFactor: 1,
  });
  const errors = [];
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  page.on("pageerror", (e) => errors.push(String(e)));

  await page.goto(`http://localhost:${PORT}/`);
  // NOTE on evidence substitution: on the reporter's platform (Windows) and on
  // WebKitGTK (Tauri's Linux engine) scrollbars are classic — they are painted
  // whenever the scrollable range is non-zero. This sandbox's headless
  // Chromium forces auto-hiding OVERLAY bars (probe: range 400px, bar width
  // 0px — bar width cannot be forced classic by flag or CSS), so the contract
  // below asserts the cross-platform quantity that forces a painted bar on the
  // shipped platforms: scrollHeight > clientHeight while content fits.
  await page.waitForSelector(".ls-block", { timeout: 8000 });

  // Open a short page first so the journals feed stays the single-feed-pane
  // singleton out of the way, then build the reporter's 2x2 dashboard.
  await openPageInFocusedPane(page, "Tine");
  await page.waitForFunction(
    () => document.querySelector(".main-content .page-title")?.textContent?.trim() === "Tine",
    { timeout: 5000 },
  );
  await page.keyboard.press("Control+Alt+\\"); // split right
  await page.waitForFunction(() => document.querySelectorAll(".pane-leaf").length === 2, { timeout: 4000 });
  await page.keyboard.press("Control+Alt+Shift+\\"); // split the focused (right) pane down
  await page.waitForFunction(() => document.querySelectorAll(".pane-leaf").length === 3, { timeout: 4000 });
  // Split the remaining full-height pane down too: it is the tallest leaf.
  const tallestIndex = await page.evaluate(() => {
    const leaves = Array.from(document.querySelectorAll(".pane-leaf"));
    let best = 0;
    leaves.forEach((leaf, i) => {
      if (leaf.getBoundingClientRect().height > leaves[best].getBoundingClientRect().height) best = i;
    });
    return best;
  });
  await page.locator(".pane-leaf").nth(tallestIndex).locator(".tab-bar").click({ position: { x: 8, y: 12 } });
  await sleep(250);
  await page.keyboard.press("Control+Alt+Shift+\\");
  await page.waitForFunction(() => document.querySelectorAll(".pane-leaf").length === 4, { timeout: 4000 });

  // Collect the quadrant rectangles once, then reach each one by closed-loop
  // focus: try the documented Ctrl+1..9 pane-focus digits until the pane that
  // carries the focus ring IS the target quadrant. Closed loop because digit
  // reading order is the app's contract, not the harness's assumption.
  const leavesGeom = await page.evaluate(() =>
    Array.from(document.querySelectorAll(".pane-leaf")).map((leaf) => {
      const r = leaf.getBoundingClientRect();
      return { x: r.x, y: r.y, cx: r.x + r.width / 2, cy: r.y + r.height / 2 };
    }),
  );
  const quads = [...leavesGeom].sort((a, b) => a.cy - b.cy || a.cx - b.cx);
  const [TLr, TRr, BLr, BRr] = quads;
  async function focusQuadrant(want) {
    for (let attempt = 0; attempt < 8; attempt++) {
      const cur = await page.evaluate(() => {
        const leaf = document.querySelector(".pane-leaf.pane-focused");
        if (!leaf) return null;
        const r = leaf.getBoundingClientRect();
        return { x: r.x, y: r.y };
      });
      if (cur && Math.abs(cur.x - want.x) <= 2 && Math.abs(cur.y - want.y) <= 2) return;
      const digit = 1 + (attempt % 4);
      await page.keyboard.press(`Control+${digit}`);
      await sleep(180);
    }
    throw new Error(`could not focus quadrant at (${want.x}, ${want.y}) via Ctrl+1..4`);
  }
  // Tine stays from the initial mirror splits in TL; give the other quadrants
  // distinct pages so the screenshot reads like the reporter's dashboard, and
  // the bottom-right pane becomes the long one that must keep scrolling.
  const targets = [
    { quad: "TR", rect: TRr, name: "Formula1" },
    { quad: "BL", rect: BLr, name: "Formula1/2026" },
    { quad: "BR", rect: BRr, name: "Project Plan" }, // 64+ blocks: the long pane
  ];
  for (const t of targets) {
    await focusQuadrant(t.rect);
    await openPageInFocusedPane(page, t.name);
    await page.waitForFunction(
      (name) =>
        document.querySelector(".pane-leaf.pane-focused .tab.active .tab-title")?.textContent?.trim() === name,
      t.name,
      { timeout: 5000 },
    );
  }
  await sleep(600);
  await page.screenshot({ path: OUT_BOARD });

  const panes = await measurePanes(page);
  const failures = [];
  console.log("pane geometry (viewport 1920x1050):");
  for (const p of panes) {
    const fits = p.contentEnd <= p.clientHeight;
    console.log(
      `  ${p.title.padEnd(16)} pane ${Math.round(p.rect.w)}x${Math.round(p.rect.h)} @(${Math.round(p.rect.x)},${Math.round(p.rect.y)})` +
        `  content ${Math.round(p.contentEnd)}px  client ${p.clientHeight}px  range ${p.range}px  scrollbarW ${p.scrollbarW}px  ${fits ? "FITS" : "overflows"}`,
    );
    if (fits && (p.range > 0 || p.scrollbarW > 0)) {
      failures.push(
        `${p.title}: content fits (${Math.round(p.contentEnd)}px <= ${p.clientHeight}px) but the pane still scrolls ` +
          `(range ${p.range}px, scrollbarW ${p.scrollbarW}px) — the useless GH #369 scrollbar`,
      );
    }
    if (!fits && p.range <= 0) failures.push(`${p.title}: content overflows but the pane does not scroll`);
  }

  // Long pane: must still scroll independently, end block stays reachable, and
  // end-of-page slack stays useful — proportional to the pane, not the window.
  const longPane = panes.find((p) => p.contentEnd > p.clientHeight);
  if (!longPane) {
    failures.push("no overflowing pane found — the Project Plan (long) pane did not overflow");
  } else {
    await page.evaluate((want) => {
      for (const leaf of document.querySelectorAll(".pane-leaf")) {
        const scroller = leaf.querySelector(".main-content");
        if (!scroller) continue;
        const sr = scroller.getBoundingClientRect();
        if (Math.abs(sr.x - want.x) <= 2 && Math.abs(sr.y - want.y) <= 2) {
          scroller.scrollTop = scroller.scrollHeight;
        }
      }
    }, longPane.rect);
    await sleep(500);
    await page.screenshot({ path: OUT_SCROLLED });
    const end = await measureEndSlack(page, longPane.rect);
    if (!end) failures.push("could not measure the long pane's end-of-page slack");
    else {
      const slackPct = Math.round((100 * end.endGap) / end.clientHeight);
      console.log(
        `  ${longPane.title}: scrolled to end — last block visible: ${end.lastBlockBottomVisible}, end slack ${Math.round(end.endGap)}px (${slackPct}% of pane)`,
      );
      if (!end.lastBlockBottomVisible) failures.push(`${longPane.title}: last block is NOT reachable/visible at max scroll`);
      if (end.endGap < 0.25 * end.clientHeight || end.endGap > 0.6 * end.clientHeight) {
        failures.push(
          `${longPane.title}: end-of-page editing slack ${slackPct}% of pane height — expected between 25% and 60% (pane-proportional)`,
        );
      }
    }
  }

  if (errors.length) console.log("console errors:\n" + errors.join("\n"));
  console.log(`wrote ${OUT_BOARD} and ${OUT_SCROLLED}`);
  await browser.close();
  server.kill("SIGKILL");
  if (failures.length) {
    console.error("GH #369 defect REPRODUCED:\n - " + failures.join("\n - "));
    process.exit(1);
  }
  console.log("OK: short panes have no useless scrollbar, long pane scrolls with pane-proportional end slack");
  process.exit(errors.length ? 1 : 0);
} catch (e) {
  console.error(String(e));
  server.kill("SIGKILL");
  process.exit(1);
}
