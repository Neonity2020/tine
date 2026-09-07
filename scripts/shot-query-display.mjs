import { waitForHttpServer } from "./e2e-capabilities.mjs";
// Photograph the inline Display panel (P5B), so it can be LOOKED at rather than
// inferred from selectors:
//
//   query-display.png         the panel open on a desktop window — the six
//                             display facts in one place, over the scrim.
//   query-display-narrow.png  the same panel at 390px, docked to the bottom
//                             edge by the `max-width: 600px` rule the native
//                             journey can never reach (the desktop window
//                             declares `minWidth: 640`).
//
// Headless Chromium over the mock backend, the same seam `shot-query-sheet.mjs`
// uses.
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5273;
const OUT = "screenshots";
const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], { stdio: "ignore", detached: process.platform !== "win32" });

async function shot(page, name, clip) {
  await page.screenshot({ path: `${OUT}/${name}.png`, ...(clip ? { clip } : { fullPage: false }) });
  console.log(`wrote ${OUT}/${name}.png`);
}

let browser;
try {
  await waitForHttpServer(`http://localhost:${PORT}/`, 40, 250, { failureMessage: "server did not start" });
  browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
  // Tall enough for the whole panel to be IN the picture; a clip that cut the
  // last two sections would hide exactly the settings that had no control before.
  const view = { width: 1200, height: 1400 };
  const page = await browser.newPage({ viewport: view, deviceScaleFactor: 2 });
  page.on("pageerror", (e) => console.log("pageerror:", String(e).split("\n")[0]));

  // The browser mock has no engine, so without a fixture every query reads back
  // as ONE retained raw row. Install the canned parse/print through the same
  // mock-only seam `shot-query-sheet.mjs` uses — and give it a VIEW this time,
  // because the panel is a picture of settings and an empty one shows nothing.
  await page.addInitScript(() => {
    const text = (t) => ({ kind: "text", text: t });
    const attr = (a, op, value) => ({ kind: "leaf", leaf: { kind: "attr", attr: a, op, value } });
    const rel = (r, pred) => ({ kind: "leaf", leaf: { kind: "rel", rel: r, quant: "any", pred } });
    globalThis.__tineMockQueryFixture = {
      print: "@block and task in ('TODO', 'DOING')"
        + " and any(props, key = 'owner' and value = 'Ada')",
      parse: {
        query: {
          anchor: "block",
          filter: {
            kind: "and",
            items: [
              attr("task", "in", { kind: "list", items: [text("TODO"), text("DOING")] }),
              rel("props", {
                kind: "and",
                items: [attr("key", "eq", text("owner")), attr("value", "eq", text("Ada"))],
              }),
            ],
          },
          diagnostics: [],
          source: { kind: "tql", original: "@block and task in ('TODO', 'DOING')" },
        },
        view: {
          view: "table",
          group_by: "prop:owner",
          sort: [["priority", "desc"], ["scheduled", "asc"]],
          columns: ["owner", "cost"],
          aggregates: [["", "count"], ["cost", "sum"]],
          sample: 25,
        },
      },
    };
  });

  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForSelector(".page-title", { timeout: 8000 });
  await page.waitForSelector(".qs-sentence", { timeout: 8000 });
  await sleep(600);

  // --- 1. open the sheet, then the panel ------------------------------------
  await page.locator(".qs-gear").first().click();
  await page.waitForSelector(".qs-sheet", { timeout: 6000 });
  await sleep(300);
  // The panel is a capability the host turns on once its reading of the query
  // has landed, so WAIT for it rather than sampling the footer mid-resolution.
  const trigger = page.locator(".qd-trigger").first();
  try {
    await page.waitForSelector(".qd-trigger", { timeout: 10000 });
  } catch {
    /* fall through to the proof below */
  }
  if (!(await trigger.count())) {
    // Say WHY. The panel is a capability the host turns on, so its absence is
    // one of a few specific states rather than a missing element.
    const proof = await page.evaluate(() => ({
      footer: document.querySelector(".qs-footer")?.outerHTML.slice(0, 800) ?? null,
      sheets: document.querySelectorAll(".qs-sheet").length,
      switcher: document.querySelectorAll(".query-view-switcher").length,
    }));
    throw new Error(`no Display control in the sheet footer: ${JSON.stringify(proof)}`);
  }
  console.log("display control reads:", JSON.stringify((await trigger.innerText()).trim()));
  await trigger.click();
  await page.waitForSelector(".qd-panel", { timeout: 6000 });
  await sleep(400);

  const held = await page.evaluate(() => ({
    sections: [...document.querySelectorAll(".qd-panel .qd-section-title")].map((el) => el.textContent.trim()),
    rows: [...document.querySelectorAll(".qd-panel .qd-row-label")].map((el) => el.textContent.trim()),
    sample: document.querySelector(".qd-panel .qd-sample")?.value,
  }));
  console.log("panel:", JSON.stringify(held));
  // A picture of a panel that dropped half its settings would look fine.
  if (held.sections.length !== 6) throw new Error(`the panel is not the six facts: ${JSON.stringify(held)}`);
  if (held.rows.length < 5) throw new Error(`the ordered lists lost entries: ${JSON.stringify(held)}`);

  // The whole viewport, deliberately: a clip around the panel's own box cut the
  // last sections off, and those are exactly the settings that had no inline
  // control before. The page around it is also the point — the panel opens over
  // the blocks, not inside the query box.
  await shot(page, "query-display");

  // --- 1b. a field picker, open inside the panel ----------------------------
  // The panel scrolls (`.qd-panel { overflow-y: auto }`) and its pickers are
  // absolutely positioned inside it, so this is where a picker would be clipped
  // by its own container and its lower rows made unreachable.
  const change = page.locator(".qd-panel .qd-row-btn", { hasText: "Change" }).first();
  await change.click();
  await page.waitForSelector(".qd-field-picker .qs-vocab-option", { timeout: 6000 });
  await sleep(300);
  const picker = await page.evaluate(() => {
    const box = document.querySelector(".qd-field-picker").getBoundingClientRect();
    const panel = document.querySelector(".qd-panel").getBoundingClientRect();
    const rows = [...document.querySelectorAll(".qd-field-picker .qs-vocab-option")].map((el) => {
      const rect = el.getBoundingClientRect();
      const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
      return {
        key: el.getAttribute("data-vocabulary-key"),
        top: Math.round(rect.top),
        bottom: Math.round(rect.bottom),
        clickable: !!hit && (hit === el || el.contains(hit)),
      };
    });
    return {
      panelBottom: Math.round(panel.bottom),
      pickerTop: Math.round(box.top),
      pickerBottom: Math.round(box.bottom),
      viewport: window.innerHeight,
      rows,
    };
  });
  console.log("picker:", JSON.stringify(picker));
  await shot(page, "query-display-picker");
  const unreachable = picker.rows.filter((row) => !row.clickable).map((row) => row.key);
  if (unreachable.length) {
    throw new Error(`field-picker rows are not reachable: ${JSON.stringify({ unreachable, picker })}`);
  }
  await page.keyboard.press("Escape");
  await sleep(250);

  // --- 2. the same panel on a phone ----------------------------------------
  await page.locator(".qs-overlay").last().click({ position: { x: 5, y: 5 } });
  await page.waitForSelector(".qd-panel", { state: "detached", timeout: 5000 });
  await page.setViewportSize({ width: 390, height: 844 });
  await sleep(500);
  // Phone-width chrome: the navigation drawer opens over the left half and the
  // first-run Guide toast sits on the bottom edge — exactly where the panel is
  // about to dock. Dismiss both so the picture is of the panel.
  await page.evaluate(() => {
    document.querySelector("[data-mobile-drawer-scrim]")?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    for (const button of document.querySelectorAll("button")) {
      const label = (button.getAttribute("aria-label") ?? "") + " " + (button.textContent ?? "");
      if (/dismiss|close/i.test(label) && button.closest(".toast, .toast-stack, [class*=toast]")) button.click();
    }
  });
  await sleep(400);
  if (!(await page.locator(".qs-sheet").count())) {
    await page.locator(".qs-gear").first().scrollIntoViewIfNeeded();
    await page.locator(".qs-gear").first().dispatchEvent("click");
    await page.waitForSelector(".qs-sheet", { timeout: 6000 });
    await sleep(300);
  }
  await page.locator(".qd-trigger").first().dispatchEvent("click");
  await page.waitForSelector(".qd-panel", { timeout: 6000 });
  await sleep(400);
  const docked = await page.evaluate(() => {
    const rect = document.querySelector(".qd-panel").getBoundingClientRect();
    return {
      left: Math.round(rect.left),
      right: Math.round(window.innerWidth - rect.right),
      bottomGap: Math.round(window.innerHeight - rect.bottom),
      width: Math.round(rect.width),
      innerWidth: window.innerWidth,
    };
  });
  console.log("narrow panel geometry:", JSON.stringify(docked));
  // This is the ONE place the bottom-sheet rule is exercised end to end.
  // Assert, don't just photograph.
  if (docked.left > 2 || docked.right > 2 || docked.bottomGap > 2) {
    throw new Error(`the narrow panel is not docked full width to the bottom edge: ${JSON.stringify(docked)}`);
  }
  await shot(page, "query-display-narrow");

  console.log("DONE");
} catch (e) {
  console.log("ERROR:", String(e).split("\n").slice(0, 8).join(" | "));
  process.exitCode = 2;
} finally {
  try {
    await browser?.close();
  } finally {
    try {
      if (process.platform === "win32") server.kill("SIGTERM");
      else process.kill(-server.pid, "SIGTERM");
    } catch (error) {
      if (error.code !== "ESRCH") throw error;
    }
  }
}
