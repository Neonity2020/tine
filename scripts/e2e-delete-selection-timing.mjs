// e2e-delete-selection-timing.mjs — repro for Martin's report: selecting text
// inside a block editor and hitting Delete does nothing when done QUICKLY,
// works when done slowly. Drives the real app (tauri-driver + WebKitWebDriver).
//
// Selection variants: (a) Shift+Arrow keys, (b) programmatic range,
// (c) REAL mouse drag across the textarea, (d) REAL double-click word select.
// Delete/Backspace sent at ~0ms vs 700ms after selection completes.
//
// Usage: node scripts/e2e-delete-selection-timing.mjs
// Does NOT modify Tine src/.

import { spawn } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ensureDisplay, stopDisplay } from "./lib/e2e-display.mjs";

await ensureDisplay();

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const G = "/tmp/txdg-del-g";
const APP = process.env.TINE_APP || path.join(ROOT, "target/release/tine");
const TD = process.env.TAURI_DRIVER || path.resolve(ROOT, "..", ".toolchain", "cargo", "bin", "tauri-driver");
const WEBKIT_DRIVER = process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver";
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4444);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4445);

const BLOCKS = [
  "alpha bravo charlie delta echo",
  "foxtrot golf hotel india juliet",
  "kilo lima mike november oscar",
  "papa quebec romeo sierra tango",
  "uniform victor whiskey xray yankee",
  "zulu one two three four five",
  "six seven eight nine ten eleven",
  "twelve thirteen fourteen fifteen sixteen",
  "seventeen eighteen nineteen twenty one",
  "two four six eight ten twelve fourteen",
  "TODO marker block with several words here",
  "first line words\nsecond line more words\nthird line final",
  Array(40).fill("word").join(" "),
];
const FIXTURE = BLOCKS.map((b) => `- ${b}`).join("\n") + "\n";

const MODE = process.env.DEL_MODE || "page"; // page | journal

function seed() {
  fs.rmSync(G, { recursive: true, force: true });
  fs.mkdirSync(`${G}/pages`, { recursive: true });
  fs.mkdirSync(`${G}/journals`, { recursive: true });
  fs.mkdirSync(`${G}/logseq`, { recursive: true });
  if (MODE === "journal") {
    fs.writeFileSync(`${G}/journals/2026_08_06.md`, FIXTURE);
    fs.writeFileSync(`${G}/journals/2026_08_05.md`, "- filler prior day one\n- filler prior day two\n");
  } else {
    fs.writeFileSync(`${G}/pages/DelTest.md`, FIXTURE);
    fs.writeFileSync(`${G}/journals/2026_08_06.md`, "- open [[DelTest]]\n");
  }
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

console.log("spawning tauri-driver:", TD);
const tdLog = fs.openSync("/tmp/td-del.log", "w");
const td = spawn(TD, ["--port", String(DRIVER_PORT), "--native-port", String(NATIVE_PORT), "--native-driver", WEBKIT_DRIVER], { env, stdio: ["ignore", tdLog, tdLog], detached: true });
await sleep(3000);
console.log("driver up, DISPLAY=", process.env.DISPLAY, "APP=", APP);

const browser = await remote({
  hostname: "127.0.0.1",
  port: DRIVER_PORT,
  path: "/",
  logLevel: "error",
  capabilities: {
    browserName: "wry",
    "wdio:enforceWebDriverClassic": true,
    "tauri:options": { application: APP },
  },
  connectionRetryCount: 1,
  connectionRetryTimeout: 60000,
});

console.log("webdriverio session connected");
await browser.$(".ls-block, .page-title").waitForExist({ timeout: 20000 });
console.log("app UI present");

const state = async (tag) => {
  const s = await browser.execute(() => {
    const ae = document.activeElement;
    const isEd = ae instanceof HTMLTextAreaElement && ae.classList.contains("block-editor");
    return {
      editing: isEd,
      selStart: isEd ? ae.selectionStart : -1,
      selEnd: isEd ? ae.selectionEnd : -1,
      value: isEd ? ae.value : null,
    };
  });
  console.log(
    `  ${tag.padEnd(24)} sel=[${s.selStart},${s.selEnd}] editing=${s.editing} value=${JSON.stringify(s.value)}`,
  );
  return s;
};

async function openDelTest() {
  if (MODE === "journal") {
    console.log("journal mode: landing on the default journal feed");
    await sleep(800);
    return;
  }
  await browser.keys(["Control", "k"]);
  await sleep(700);
  for (const ch of "DelTest") await browser.keys([ch]);
  await sleep(700);
  await browser.keys(["Enter"]);
  await sleep(1000);
}

// Enter block idx's editor by its rendered mousedown/mouseup pair.
async function enterBlock(idx) {
  await browser.execute((i) => {
    const blocks = [...document.querySelectorAll(".ls-block")];
    const block = blocks[i];
    const wrap = block?.querySelector(":scope > .block-main .block-content-wrapper");
    const el = wrap || block;
    if (!el) return;
    el.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true, button: 0 }));
    el.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, cancelable: true, button: 0 }));
  }, idx);
  await sleep(400);
}

function expectedAfter(value, selStart, selEnd, key) {
  if (key === "Backspace" && selStart === selEnd && selStart > 0) {
    return value.slice(0, selStart - 1) + value.slice(selStart);
  }
  if (selStart === selEnd) return value; // caret Delete at 0 / nowhere — not our case
  return value.slice(0, selStart) + value.slice(selEnd);
}

async function runCase(label, blockIdx, { selectVia, delayMs, key }) {
  console.log(`\nCASE ${label} (block=${blockIdx}, via=${selectVia}, delay=${delayMs}ms, key=${key})`);
  await enterBlock(blockIdx);
  const before = await state("after-enter");
  if (!before.editing || !before.value) {
    console.log("  !! could not enter editor");
    return { ran: false, ok: false };
  }
  let selNow = [0, 6];
  if (selectVia === "shift") {
    await browser.execute(() => {
      const ae = document.activeElement;
      if (ae instanceof HTMLTextAreaElement) ae.setSelectionRange(0, 0);
    });
    for (let i = 0; i < 6; i++) await browser.keys(["Shift", "ArrowRight"]);
  } else if (selectVia === "mouse") {
    // REAL pointer drag across the first word(s) of the textarea.
    const rect = await browser.execute(() => {
      const ae = document.activeElement;
      const r = ae.getBoundingClientRect();
      return { x: r.x, y: r.y, w: r.width, h: r.height };
    });
    const y = rect.y + rect.h / 2;
    await browser.performActions([{
      type: "pointer",
      id: "mouse",
      parameters: { pointerType: "mouse" },
      actions: [
        { type: "pointerMove", duration: 0, x: rect.x + 10, y },
        { type: "pointerDown", button: 0 },
        { type: "pointerMove", duration: 60, x: rect.x + 70, y },
        { type: "pointerUp", button: 0 },
      ],
    }]);
  } else if (selectVia === "dblclick") {
    const rect = await browser.execute(() => {
      const ae = document.activeElement;
      const r = ae.getBoundingClientRect();
      return { x: r.x, y: r.y, w: r.width, h: r.height };
    });
    const y = rect.y + rect.h / 2;
    await browser.performActions([{
      type: "pointer",
      id: "mouse",
      parameters: { pointerType: "mouse" },
      actions: [
        { type: "pointerMove", duration: 0, x: rect.x + 40, y },
        { type: "pointerDown", button: 0 },
        { type: "pointerUp", button: 0 },
        { type: "pointerDown", button: 0 },
        { type: "pointerUp", button: 0 },
      ],
    }]);
  } else {
    await browser.execute(() => {
      const ae = document.activeElement;
      if (ae instanceof HTMLTextAreaElement) ae.setSelectionRange(0, 6);
    });
  }
  const sel = await state("after-select");
  selNow = [Math.min(sel.selStart, sel.selEnd), Math.max(sel.selStart, sel.selEnd)];
  if (delayMs > 0) await sleep(delayMs);
  await browser.keys([key]);
  await sleep(250);
  const after = await state("after-delete");
  await browser.keys(["Escape"]);
  await sleep(300);
  const want = expectedAfter(sel.value, selNow[0], selNow[1], key);
  const ok = after.value === want;
  console.log(`  => ${ok ? "PASS" : "BROKEN"}: want ${JSON.stringify(want)}, got ${JSON.stringify(after.value)}`);
  return { ran: true, ok, label };
}

await openDelTest();
const results = [];
results.push(await runCase("JS fast", 0, { selectVia: "js", delayMs: 0, key: "Delete" }));
results.push(await runCase("JS slow", 1, { selectVia: "js", delayMs: 700, key: "Delete" }));
results.push(await runCase("Shift fast", 2, { selectVia: "shift", delayMs: 0, key: "Delete" }));
results.push(await runCase("Shift slow", 3, { selectVia: "shift", delayMs: 700, key: "Delete" }));
results.push(await runCase("Mouse-drag fast", 4, { selectVia: "mouse", delayMs: 0, key: "Delete" }));
results.push(await runCase("Mouse-drag slow", 5, { selectVia: "mouse", delayMs: 700, key: "Delete" }));
results.push(await runCase("Dblclick fast", 6, { selectVia: "dblclick", delayMs: 0, key: "Delete" }));
results.push(await runCase("Dblclick slow", 7, { selectVia: "dblclick", delayMs: 700, key: "Delete" }));
results.push(await runCase("Mouse-drag fast Backsp", 8, { selectVia: "mouse", delayMs: 0, key: "Backspace" }));
results.push(await runCase("Mouse-drag slow Backsp", 9, { selectVia: "mouse", delayMs: 700, key: "Backspace" }));

// Ctrl+A (select-all) then Delete — the GH #262 ladder ships in this binary.
async function runCtrlACase(label, blockIdx, delayMs) {
  console.log(`\nCASE ${label} (block=${blockIdx}, delay=${delayMs}ms, key=Delete)`);
  await enterBlock(blockIdx);
  const before = await state("after-enter");
  if (!before.editing) { console.log("  !! not editing"); return { ran: false, ok: false, label }; }
  await browser.keys(["Control", "a"]);
  const sel = await state("after-ctrlA");
  if (delayMs > 0) await sleep(delayMs);
  await browser.keys(["Delete"]);
  await sleep(250);
  const after = await state("after-delete");
  await browser.keys(["Escape"]);
  await sleep(300);
  const ok = after.value === "" && after.editing;
  console.log(`  => ${ok ? "PASS" : "BROKEN"}: want empty+editing, got editing=${after.editing} value=${JSON.stringify(after.value)}`);
  return { ran: true, ok, label };
}

// Freshly typed text, then immediate select-all + delete (save debounce in flight).
async function runTypedCase(label, blockIdx, delayMs) {
  console.log(`\nCASE ${label} (block=${blockIdx}, delay=${delayMs}ms, key=Delete)`);
  await enterBlock(blockIdx);
  const before = await state("after-enter");
  if (!before.editing) { console.log("  !! not editing"); return { ran: false, ok: false, label }; }
  await browser.execute(() => {
    const ae = document.activeElement;
    if (ae instanceof HTMLTextAreaElement) ae.setSelectionRange(ae.value.length, ae.value.length);
  });
  for (const ch of "TYPEDFRESH") await browser.keys([ch]);
  const typed = await state("after-typing");
  await browser.execute(() => {
    const ae = document.activeElement;
    if (ae instanceof HTMLTextAreaElement) ae.setSelectionRange(ae.value.length - 10, ae.value.length);
  });
  const sel = await state("after-select");
  if (delayMs > 0) await sleep(delayMs);
  await browser.keys(["Delete"]);
  await sleep(250);
  const after = await state("after-delete");
  await browser.keys(["Escape"]);
  await sleep(300);
  const want = typed.value.slice(0, -10);
  const ok = after.value === want;
  console.log(`  => ${ok ? "PASS" : "BROKEN"}: want ${JSON.stringify(want)}, got ${JSON.stringify(after.value)}`);
  return { ran: true, ok, label };
}

results.push(await runCtrlACase("Ctrl+A fast Delete", 0, 0));
results.push(await runCtrlACase("Ctrl+A slow Delete", 1, 800));
results.push(await runTypedCase("Typed fast Delete", 2, 0));
results.push(await runTypedCase("Typed slow Delete", 3, 800));

// END boundary: select the last 6 chars, delete fast/slow.
async function runEndCase(label, blockIdx, delayMs) {
  console.log(`\nCASE ${label} (block=${blockIdx}, delay=${delayMs}ms)`);
  await enterBlock(blockIdx);
  const before = await state("after-enter");
  if (!before.editing) { console.log("  !! not editing"); return { ran: false, ok: false, label }; }
  await browser.execute(() => {
    const ae = document.activeElement;
    if (ae instanceof HTMLTextAreaElement) ae.setSelectionRange(ae.value.length - 6, ae.value.length);
  });
  const sel = await state("after-select");
  if (delayMs > 0) await sleep(delayMs);
  await browser.keys(["Delete"]);
  await sleep(250);
  const after = await state("after-delete");
  await browser.keys(["Escape"]);
  await sleep(300);
  const want = expectedAfter(sel.value, sel.selStart, sel.selEnd, "Delete");
  const ok = after.value === want;
  console.log(`  => ${ok ? "PASS" : "BROKEN"}: want ${JSON.stringify(want)}, got ${JSON.stringify(after.value)}`);
  return { ran: true, ok, label };
}

// Backward selection: caret at end, Shift+Left x6, then delete fast/slow.
async function runBackSelCase(label, blockIdx, delayMs) {
  console.log(`\nCASE ${label} (block=${blockIdx}, delay=${delayMs}ms)`);
  await enterBlock(blockIdx);
  const before = await state("after-enter");
  if (!before.editing) { console.log("  !! not editing"); return { ran: false, ok: false, label }; }
  for (let i = 0; i < 6; i++) await browser.keys(["Shift", "ArrowLeft"]);
  const sel = await state("after-select");
  if (delayMs > 0) await sleep(delayMs);
  await browser.keys(["Delete"]);
  await sleep(250);
  const after = await state("after-delete");
  await browser.keys(["Escape"]);
  await sleep(300);
  const want = expectedAfter(sel.value, Math.min(sel.selStart, sel.selEnd), Math.max(sel.selStart, sel.selEnd), "Delete");
  const ok = after.value === want;
  console.log(`  => ${ok ? "PASS" : "BROKEN"}: want ${JSON.stringify(want)}, got ${JSON.stringify(after.value)}`);
  return { ran: true, ok, label };
}

results.push(await runEndCase("End-sel fast", 4, 0));
results.push(await runEndCase("End-sel slow", 5, 700));
results.push(await runBackSelCase("BackSel fast", 6, 0));
results.push(await runBackSelCase("BackSel slow", 7, 700));
// TODO marker block (idx 10 in page mode: blocks 0-9 plain, 10 = TODO line)
results.push(await runCase("TODO block fast", 10, { selectVia: "js", delayMs: 0, key: "Delete" }));
results.push(await runCase("TODO block slow", 10, { selectVia: "js", delayMs: 700, key: "Delete" }));
// Multiline block (idx 11): select a span crossing the newline.
// Long block (idx 12): select a middle span.
results.push(await runCase("Long block fast", 12, { selectVia: "js", delayMs: 0, key: "Delete" }));
results.push(await runCase("Long block slow", 12, { selectVia: "js", delayMs: 700, key: "Delete" }));

// HELD-DRAG variant — the suspected real gesture: start a mouse-drag selection
// and press Delete WHILE THE BUTTON IS STILL DOWN. Also probe whether a keydown
// is dispatched to the page at all during the held drag.
async function runHeldDragCase(label, blockIdx, key) {
  console.log(`\nCASE ${label} (block=${blockIdx}, key=${key} — button HELD)`);
  await enterBlock(blockIdx);
  const before = await state("after-enter");
  if (!before.editing) { console.log("  !! not editing"); return { ran: false, ok: false, label }; }
  // Instrument: count keydowns reaching the document and the editing textarea.
  await browser.execute(() => {
    (window).__kd = [];
    document.addEventListener("keydown", (e) => {
      const t = e.target;
      (window).__kd.push(`${e.key}@${t instanceof HTMLTextAreaElement ? "textarea" : t?.tagName ?? "?"}`);
    }, true);
  });
  const rect = await browser.execute(() => {
    const ae = document.activeElement;
    const r = ae.getBoundingClientRect();
    return { x: r.x, y: r.y, w: r.width, h: r.height };
  });
  const y = rect.y + rect.h / 2;
  await browser.performActions([{
    type: "pointer",
    id: "mouse",
    parameters: { pointerType: "mouse" },
    actions: [
      { type: "pointerMove", duration: 0, x: rect.x + 10, y },
      { type: "pointerDown", button: 0 },
      { type: "pointerMove", duration: 60, x: rect.x + 70, y },
      // NOTE: pointer still DOWN here.
    ],
  }]);
  const duringDrag = await state("mid-drag");
  await browser.keys([key]); // key pressed while the mouse button is held
  await sleep(120);
  const afterKey = await state("key-while-held");
  const kd = await browser.execute(() => (window).__kd ?? []);
  await browser.performActions([{
    type: "pointer",
    id: "mouse",
    parameters: { pointerType: "mouse" },
    actions: [{ type: "pointerUp", button: 0 }],
  }]);
  await sleep(200);
  const afterUp = await state("after-pointerup");
  console.log(`  keydowns seen: ${JSON.stringify(kd)}`);
  const want = expectedAfter(duringDrag.value, Math.min(duringDrag.selStart, duringDrag.selEnd), Math.max(duringDrag.selStart, duringDrag.selEnd), key);
  const ok = afterUp.value === want || afterKey.value === want;
  console.log(`  => ${ok ? "PASS" : "BROKEN"}: want ${JSON.stringify(want)}, got held=${JSON.stringify(afterKey.value)} up=${JSON.stringify(afterUp.value)}`);
  await browser.keys(["Escape"]);
  await sleep(300);
  return { ran: true, ok, label };
}

results.push(await runHeldDragCase("Held-drag Delete", 1, "Delete"));
results.push(await runHeldDragCase("Held-drag Backspace", 3, "Backspace"));

// RENDERED-VIEW selection: drag across a NOT-editing block's rendered text.
// Martin: fast drag → later Delete does nothing; slow drag → works.
async function runRenderedCase(label, blockIdx, dragMs, delayMs, key) {
  console.log(`\nCASE ${label} (block=${blockIdx}, drag=${dragMs}ms, delay=${delayMs}ms, key=${key})`);
  if (dragMs >= 0) await sleep(600); // ensure editor from a previous case is gone
  const rect = await browser.execute((i) => {
    const blocks = [...document.querySelectorAll(".ls-block")];
    const el = blocks[i]?.querySelector(":scope > .block-main .block-content-wrapper") || blocks[i];
    const r = el.getBoundingClientRect();
    return { x: r.x, y: r.y, w: r.width, h: r.height };
  }, blockIdx);
  const y = rect.y + rect.h / 2;
  await browser.performActions([{
    type: "pointer",
    id: "mouse",
    parameters: { pointerType: "mouse" },
    actions: [
      { type: "pointerMove", duration: 0, x: rect.x + 10, y },
      { type: "pointerDown", button: 0 },
      { type: "pointerMove", duration: dragMs, x: rect.x + 90, y },
      { type: "pointerUp", button: 0 },
    ],
  }]);
  const afterSel = await browser.execute(() => {
    const ae = document.activeElement;
    return {
      editing: ae instanceof HTMLTextAreaElement && ae.classList.contains("block-editor"),
      winSel: String(window.getSelection() ?? "").slice(0, 40),
      editingCls: !!document.querySelector(".block-main.editing"),
    };
  });
  console.log(`  after-drag: editing=${afterSel.editing} windowSel=${JSON.stringify(afterSel.winSel)} editing-block=${afterSel.editingCls}`);
  if (delayMs > 0) await sleep(delayMs);
  await browser.keys([key]);
  await sleep(250);
  const after = await browser.execute((i) => {
    const ae = document.activeElement;
    const isEd = ae instanceof HTMLTextAreaElement && ae.classList.contains("block-editor");
    const blocks = [...document.querySelectorAll(".ls-block")];
    const b = blocks[i];
    return {
      editing: isEd,
      editorValue: isEd ? ae.value : null,
      renderedText: b ? b.textContent.trim().replace(/\s+/g, " ").slice(0, 44) : null,
      winSel: String(window.getSelection() ?? "").slice(0, 40),
    };
  }, blockIdx);
  console.log(`  after-${key}: editing=${after.editing} editorValue=${JSON.stringify(after.editorValue)} rendered=${JSON.stringify(after.renderedText)} sel=${JSON.stringify(after.winSel)}`);
  await browser.keys(["Escape"]);
  await sleep(300);
  return { ran: true, ok: true, label, after };
}

results.push(await runRenderedCase("Rendered fast-drag Delete", 6, 20, 300, "Delete"));
results.push(await runRenderedCase("Rendered slow-drag Delete", 7, 400, 300, "Delete"));
results.push(await runRenderedCase("Rendered fast-drag slow-key", 8, 20, 1200, "Delete"));

console.log("\n==== SUMMARY ====");
let broken = 0;
for (const r of results) console.log(`  ${r.ok ? "PASS  " : "BROKEN"} ${r.label}`);
for (const r of results) if (!r.ok) broken++;
console.log(`broken cases: ${broken}/${results.length}`);

await browser.deleteSession();
td.kill();
stopDisplay();
