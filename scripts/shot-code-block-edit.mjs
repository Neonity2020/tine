// GH #357 geometry harness: a whole-block fenced code block must consume the
// SAME visual (font/line) metrics in its rendered card and in the block editor
// that replaces it on click. Prints the numeric geometry for both states,
// writes deterministic screenshots, and FAILS (exit 1) if the editor's
// code-card contract is not met. Not a visual-approval gate — the numbers are
// the artifact; final visual review stays with the manager.
//
// Usage: npm run build && node scripts/shot-code-block-edit.mjs
// Optional: OUT_DIR=/some/dir (default /tmp), PREFIX=before|after
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";

const PORT = 5197;
const OUT_DIR = process.env.OUT_DIR ?? "/tmp";
const PREFIX = process.env.PREFIX ?? "shot";
const OUT_RENDERED = `${OUT_DIR}/${PREFIX}-codeblock-rendered.png`;
const OUT_EDIT = `${OUT_DIR}/${PREFIX}-codeblock-edit.png`;

const FENCE = [
  "```js",
  "// short line",
  "const width = 'a deliberately very long line that would wrap in a proportional-font textarea: 0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ more-and-more-tail-text';",
  "function bar(x) { return x + 1; }",
  "```",
].join("\n");

const server = spawn("npx", ["vite", "preview", "--port", String(PORT), "--strictPort"], {
  stdio: "ignore",
});

async function waitForServer(url, tries = 60) {
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

async function measure(page, viewport) {
  return page.evaluate(
    async ({ viewport: vp }) => {
      const styleOf = (el, props) =>
        props.reduce((acc, p) => ((acc[p] = getComputedStyle(el)[p]), acc), {});
      const sentinel = "a deliberately very long line";
      // The journal feed is virtualized: only a window of blocks is mounted.
      // Walk the main scroller in viewport steps until the card mounts.
      const scroller = document.querySelector("main.main-content") ?? document.scrollingElement;
      let pre = null;
      if (scroller) {
        const max = Math.max(0, scroller.scrollHeight - scroller.clientHeight);
        for (let top = 0; top <= max + 2 && !pre; top += Math.max(200, scroller.clientHeight * 0.7)) {
          scroller.scrollTop = Math.min(top, max);
          await new Promise((r) => setTimeout(r, 130));
          const pres = [...document.querySelectorAll("pre.code-block")];
          pre = pres.find((p) => p.textContent.includes(sentinel)) ?? null;
        }
      }
      if (!pre) return { error: "rendered code card not found", editorOpen: !!document.querySelector("textarea.block-editor"), scrollHeight: scroller ? scroller.scrollHeight : null, lsBlocks: document.querySelectorAll(".ls-block").length, mainFound: !!document.querySelector("main.main-content") };
      pre.scrollIntoView({ block: "center" });
      await new Promise((r) => setTimeout(r, 150));
      const rendered = {
        box: pre.getBoundingClientRect().toJSON(),
        style: styleOf(pre, ["fontFamily", "fontSize", "lineHeight", "padding", "borderRadius", "background"]),
        scrollOverflowX: pre.scrollWidth > pre.clientWidth,
      };
      pre.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true }));
      pre.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, cancelable: true }));
      pre.click();
      // The editor for the same block (retry: the rendered→editing swap is sync-ish).
      let ta = null;
      for (let i = 0; i < 40 && !ta; i++) {
        ta = document.querySelector("textarea.block-editor");
        if (ta) break;
        await new Promise((r) => setTimeout(r, 50));
      }
      if (!ta) return { error: "editor did not appear", rendered };
      const editing = {
        box: ta.getBoundingClientRect().toJSON(),
        style: styleOf(ta, ["fontFamily", "fontSize", "lineHeight", "padding", "borderRadius", "background"]),
        value: ta.value,
        className: ta.className,
        wrapAttr: ta.getAttribute("wrap"),
        scrollOverflowX: ta.scrollWidth > ta.clientWidth,
      };
      return { viewport: vp, rendered, editing };
    },
    { viewport },
  );
}

try {
  await waitForServer(`http://localhost:${PORT}/`);
  const browser = await chromium.launch({ args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
  const page = await browser.newPage({ viewport: { width: 1120, height: 820 }, deviceScaleFactor: 2 });
  const errors = [];
  page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
  page.on("pageerror", (e) => errors.push(String(e)));

  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForSelector(".ls-block", { timeout: 8000 });

  // Fixture goes on a SHORT named page ("Tine" — a few blocks, no feed
  // virtualization racing the measurements) reached through the switcher.
  await page.keyboard.press("Control+k");
  await page.waitForSelector(".switcher-input", { timeout: 4000 });
  await page.locator(".switcher-input").fill("Tine");
  await sleep(500);
  const target = page.locator(".switcher-row", { hasText: /^pageTine$/ }).first();
  await target.waitFor({ state: "visible", timeout: 4000 });
  await target.click();
  await page.waitForSelector(".ls-block", { timeout: 4000 });

  // Build the fixture block deterministically: edit a PLAIN-TEXT outline
  // block (never a live-ref-group item or a clickable link), inject the full
  // fence text, blur to commit + re-render.
  const fixtureBlock = page.locator(".main-content .ls-block", {
    hasText: "Reads the same markdown graph",
  }).first();
  await fixtureBlock.locator(".block-content").first().click();
  await page.waitForSelector("textarea.block-editor", { timeout: 3000 });
  await page.evaluate((text) => {
    const ta = document.querySelector("textarea.block-editor");
    const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, "value").set;
    setter.call(ta, text);
    ta.dispatchEvent(new InputEvent("input", { bubbles: true, cancelable: true }));
  }, FENCE);
  await sleep(150);
  await page.evaluate(() => document.activeElement.blur());
  await page.waitForSelector("pre.code-block", { timeout: 3000 });
  await sleep(450); // let highlight.js upgrade + autosize settle
  await page.screenshot({ path: OUT_RENDERED, fullPage: true });

  const wide = await measure(page, "1120");
  await page_screenshot_safe(page, OUT_EDIT);

  // Narrow-pane variant (same block, squeezed viewport).
  await page.evaluate(() => document.activeElement?.blur());
  await page.waitForSelector("textarea.block-editor", { state: "detached", timeout: 3000 }).catch(() => {});
  await sleep(300);
  await page.setViewportSize({ width: 700, height: 820 });
  await sleep(400);
  const narrow = await measure(page, "700");

  const failures = [];
  for (const m of [wide, narrow].filter(Boolean)) {
    if (m.error) {
      failures.push(`${m.viewport ?? "?"}: ${m.error}`);
      continue;
    }
    const tag = `${m.viewport}w`;
    const r = m.rendered;
    const ed = m.editing;
    if (!r || !ed) {
      failures.push(`${tag}: missing rendered or editing measurement`);
      continue;
    }
    if (!/\bcode-edit\b/.test(ed.className ?? ""))
      failures.push(`${tag}: editor lacks code-edit card class (${ed.className})`);
    if (ed.wrapAttr !== "off")
      failures.push(`${tag}: editor wrap attribute is ${JSON.stringify(ed.wrapAttr)}, expected "off"`);
    // same fonts/size/leading — the --tine-editable-font override keeps the
    // emoji fallback segment on the editor side (the .calc-wrap precedent);
    // normalize it away, and compare everything else exactly.
    const norm = (v) => String(v ?? "").replace(/, "Noto Emoji Variable"/g, "");
    for (const prop of ["fontFamily", "fontSize", "lineHeight", "padding"])
      if (norm(r.style[prop]) !== norm(ed.style[prop]))
        failures.push(`${tag}: ${prop} rendered=${JSON.stringify(r.style[prop])} editor=${JSON.stringify(ed.style[prop])}`);
    // The editor shows N+2 lines (the two fence lines live inside the same
    // card). Height delta ≈ exactly two lines; anything more is a layout jump.
    const lh = parseFloat(r.style.lineHeight);
    const delta = ed.box.height - r.box.height;
    if (Math.abs(delta - 2 * lh) > 4)
      failures.push(`${tag}: height delta ${delta.toFixed(1)}px ≠ 2 lines (${(2 * lh).toFixed(1)}px)`);
    if (!ed.scrollOverflowX)
      failures.push(`${tag}: long code line should overflow horizontally (no soft wrap) like the card`);
  }

  console.log(JSON.stringify({ wide, narrow, failures }, null, 1));
  console.log(`screenshots: ${OUT_RENDERED}, ${OUT_EDIT}`);

  await browser.close();
  server.kill("SIGKILL");
  process.exit(errors.length || failures.length ? 1 : 0);
} catch (e) {
  console.error(String(e));
  server.kill("SIGKILL");
  process.exit(1);
}

async function page_screenshot_safe(page, out) {
  try {
    await page.screenshot({ path: out, fullPage: true });
  } catch {
    // best-effort artifact
  }
}
