// e2e-rendered-delete-verify.mjs — precise verification of rendered-selection
// deletion in the REAL app: JS-set window selections over exact substrings of
// rendered block text, then Delete, and compare the resulting source bytes.
// Complements the jsdom unit tests with real span/element mappings.
// Does NOT modify Tine src/.

import { spawn } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ensureDisplay, stopDisplay } from "./lib/e2e-display.mjs";
import { tauriCapabilities, webdriverServerArgs } from "./e2e-capabilities.mjs";

await ensureDisplay();

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const G = "/tmp/txdg-rdv-g";
const APP = process.env.TINE_APP || path.join(ROOT, "target/release/tine");
const TD = process.env.TAURI_DRIVER || path.resolve(ROOT, "..", ".toolchain", "cargo", "bin", "tauri-driver");
const WEBKIT_DRIVER = process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver";

const FIXTURE = "- 0123456789 abcdefghij\n- bold **marker** and [[Page Link]] end\n";
function seed() {
  fs.rmSync(G, { recursive: true, force: true });
  fs.mkdirSync(`${G}/pages`, { recursive: true });
  fs.mkdirSync(`${G}/journals`, { recursive: true });
  fs.mkdirSync(`${G}/logseq`, { recursive: true });
  fs.writeFileSync(`${G}/pages/RdvTest.md`, FIXTURE);
  fs.writeFileSync(`${G}/journals/2026_08_06.md`, "- open [[RdvTest]]\n");
}

seed();
await ensureDisplay();
fs.rmSync("/tmp/txdg", { recursive: true, force: true });
for (const d of ["data", "config", "cache"]) fs.mkdirSync(`/tmp/txdg/${d}`, { recursive: true });

const env = {
  ...process.env,
  TINE_GRAPH: G,
  XDG_DATA_HOME: "/tmp/txdg/data",
  XDG_CONFIG_HOME: "/tmp/txdg/config",
  XDG_CACHE_HOME: "/tmp/txdg/cache",
  WEBKIT_DISABLE_DMABUF_RENDERER: "1",
  LIBGL_ALWAYS_SOFTWARE: "1",
  WEBKIT_DISABLE_COMPOSITING_MODE: "1",
  GDK_BACKEND: "x11",
};

const td = spawn(TD, webdriverServerArgs(4444, 4445, WEBKIT_DRIVER), { env, stdio: "ignore", detached: true });
await sleep(3000);

const browser = await remote({
  hostname: "127.0.0.1",
  port: 4444,
  path: "/",
  logLevel: "error",
  capabilities: tauriCapabilities(APP, "rendered-delete-verify"),
  connectionRetryCount: 1,
  connectionRetryTimeout: 60000,
});
await browser.$(".ls-block, .page-title").waitForExist({ timeout: 20000 });

// Navigate to RdvTest via quick switch.
await browser.keys(["Control", "k"]);
await sleep(700);
for (const ch of "RdvTest") await browser.keys([ch]);
await sleep(700);
await browser.keys(["Enter"]);
await sleep(1000);

// Select `start..end` of the text node containing `needle` inside block idx's wrapper.
async function selectRendered(blockIdx, needle, start, end) {
  return browser.execute((i, n, s, e) => {
    const blocks = [...document.querySelectorAll(".ls-block")];
    const wrapper = blocks[i]?.querySelector(":scope > .block-main .block-content-wrapper");
    if (!wrapper) return "no-wrapper";
    const walker = document.createTreeWalker(wrapper, NodeFilter.SHOW_TEXT);
    let t = null;
    for (let x = walker.nextNode(); x; x = walker.nextNode()) {
      if ((x.textContent ?? "").includes(n)) { t = x; break; }
    }
    if (!t) return "no-text";
    const range = document.createRange();
    range.setStart(t, s);
    range.setEnd(t, e);
    const sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
    return String(sel);
  }, blockIdx, needle, start, end);
}

async function sourceOf(blockIdx) {
  return browser.execute((i) => {
    const rows = [...document.querySelectorAll('.ls-block[data-block-id]')];
    const row = rows[i];
    void row;
    const ae = document.activeElement;
    return ae instanceof HTMLTextAreaElement ? ae.value : null;
  }, blockIdx);
}

async function caseDelete(label, blockIdx, needle, s, e) {
  const sel = await selectRendered(blockIdx, needle, s, e);
  console.log(`\nCASE ${label}: selection=${JSON.stringify(sel)} want-splice=[${s},${e}) of ${JSON.stringify(needle)}`);
  await browser.keys(["Delete"]);
  await sleep(400);
  const after = await sourceOf(blockIdx);
  const want = needle.slice(0, s) + needle.slice(e);
  // Merge the needle back into the full block value isn't derivable here from
  // needle alone for the markers block; compare against the editor value.
  console.log(`  editor after: ${JSON.stringify(after)}  (needle splice would be ${JSON.stringify(want)})`);
  await browser.keys(["Escape"]);
  await sleep(300);
  return { label, sel, after };
}

// Block 0: plain "0123456789 abcdefghij"
await caseDelete("plain mid-span", 0, "0123456789 abcdefghij", 2, 8);
// Block 1: formatted — select inside "Page Link" visible text.
await caseDelete("inside [[Page Link]]", 1, "Page Link", 2, 5);
// Block 1 again: select rendered "and " between emphasis and link.
await caseDelete("plain between construct", 1, "and Page Link", 0, 4);

// Pointer-drag probe: dump what WebKit actually reports for a mouse selection.
await sleep(400);
// Re-navigate (previous cases deleted text / opened editors elsewhere).
await browser.keys(["Escape"]);
await sleep(300);
{
  const rect = await browser.execute(() => {
    const blocks = [...document.querySelectorAll(".ls-block")];
    const el = blocks[0]?.querySelector(":scope > .block-main .block-content-wrapper") || blocks[0];
    const r = el.getBoundingClientRect();
    return { x: r.x, y: r.y, w: r.width, h: r.height, text: el.textContent?.slice(0, 60) };
  });
  console.log(`\nDRAG-PROBE wrapper text=${JSON.stringify(rect.text)}`);
  const y = rect.y + rect.h / 2;
  await browser.performActions([{
    type: "pointer",
    id: "mouse",
    parameters: { pointerType: "mouse" },
    actions: [
      { type: "pointerMove", duration: 0, x: rect.x + 10, y },
      { type: "pointerDown", button: 0 },
      { type: "pointerMove", duration: 100, x: rect.x + 90, y },
      { type: "pointerUp", button: 0 },
    ],
  }]);
  const probe = await browser.execute(() => {
    const sel = window.getSelection();
    if (!sel || sel.rangeCount === 0) return "no-selection";
    const r = sel.getRangeAt(0);
    const desc = (c, o) => {
      const el = c.nodeType === Node.ELEMENT_NODE ? c : c.parentElement;
      const wrap = el?.closest?.(".block-content-wrapper");
      const idx = wrap && c.nodeType === Node.ELEMENT_NODE ? Array.prototype.indexOf.call(c.childNodes, c.childNodes[o] ?? null) : null;
      void idx;
      return `${c.nodeType === Node.ELEMENT_NODE ? "EL" : "TXT"}(${(c.textContent ?? "").slice(0, 24)})@${o}`;
    };
    return {
      text: String(sel),
      start: desc(r.startContainer, r.startOffset),
      end: desc(r.endContainer, r.endOffset),
    };
  });
  console.log(`  drag selection: ${JSON.stringify(probe)}`);
}

await browser.deleteSession();
td.kill();
stopDisplay();
