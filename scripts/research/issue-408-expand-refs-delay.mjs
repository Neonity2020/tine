// GH #408 research diagnostic (NOT a cataloged E2E journey — research harness).
// ============================================================================
// Measures the delay between expanding a collapsed large outline and its block
// references / block embeds actually displaying content, on the REAL app
// (tauri-driver + WebKitWebDriver + Xvfb) against a synthetic graph whose shape
// mirrors the reporter's: one page whose single collapsed parent holds hundreds
// of children, half of them `((uuid))` block refs, half `{{embed ((uuid))}}`
// block embeds, each pointing at a distinct id-bearing block spread across
// several source pages (the "PDF reading notes" shape).
//
// It records an in-page rAF timeline (structure rows, unresolved refs,
// hydrated embeds) from the expand click until everything settles, so the
// reported "structure appears first, refs/embeds only ~10s later" becomes a
// measurable phase gap on this exact base.
//
// Run (needs a built release binary):
//   node scripts/research/issue-408-expand-refs-delay.mjs \
//     [--children 400] [--pages 40] [--per-page 20] [--pad 300] [--keep]
//
// All content is synthetic. No private graphs are read.

import { spawn } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { ensureDisplay } from "../lib/e2e-display.mjs";
import { tauriCapabilities, webdriverServerArgs } from "../e2e-capabilities.mjs";

await ensureDisplay();

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------
const argValue = (name, fallback) => {
  const i = process.argv.indexOf(name);
  return i >= 0 ? Number(process.argv[i + 1]) : fallback;
};
const CHILDREN = argValue("--children", 400); // ref/embed children under the parent
const SRC_PAGES = argValue("--pages", 40); // distinct source pages the ids live on
const PER_PAGE = argValue("--per-page", 20); // id-bearing blocks per source page
const PAD = argValue("--pad", 300); // prose chars per child bullet
const KEEP = process.argv.includes("--keep");
const REFS_ONLY = process.argv.includes("--refs-only"); // no {{embed}} children

// Deterministic uuid generator (uuidv4-shaped, stable across runs).
function uuid(i) {
  const hex = (n, w) => n.toString(16).padStart(w, "0");
  return `${hex(i, 8)}-1111-4222-8333-${hex(i, 12)}`;
}

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const APP = process.env.TINE_APP || path.join(ROOT, "target/release/tine");
const TD = process.env.TAURI_DRIVER || (process.env.CARGO_HOME ? path.join(process.env.CARGO_HOME, "bin", "tauri-driver") : "tauri-driver");
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4598);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4599);
const TMP = `/tmp/tine-408-research-${CHILDREN}x${SRC_PAGES}`;
const GRAPH = `${TMP}/graph`;
const HUB = `${GRAPH}/pages/Hub.md`;

if (!fs.existsSync(APP)) {
  console.error(`no release binary at ${APP} — build first (./scripts/deploy.sh)`);
  process.exit(1);
}

// ---------------------------------------------------------------------------
// Synthetic graph
// ---------------------------------------------------------------------------
fs.rmSync(TMP, { recursive: true, force: true });
for (const dir of ["pages", "journals", "logseq"]) fs.mkdirSync(`${GRAPH}/${dir}`, { recursive: true });
for (const dir of ["data", "config", "cache"]) fs.mkdirSync(`${TMP}/xdg/${dir}`, { recursive: true });
fs.writeFileSync(`${GRAPH}/logseq/config.edn`, "{}\n");

const PROSE = (i, n) =>
  `Synthetic paragraph ${i} of ${n}: the quick brown outliner jumps over the lazy dog repeatedly to reach a realistic prose length for measurement purposes; this text carries no user content and exists only so each block occupies a realistic amount of vertical and parseable space.`;

const totalIds = SRC_PAGES * PER_PAGE;
// Child i references id (i % totalIds) + 1 — distinct per child until wraparound.
// Even children: a plain block ref in the body. Odd children: a block embed.
const hubLines = ["- GH408 parent: expand me", "  collapsed:: true"];
for (let i = 0; i < CHILDREN; i++) {
  const id = (i % totalIds) + 1;
  if (i % 2 === 0 || REFS_ONLY) {
    hubLines.push(`  - Child ${i} refs ((${uuid(id)})) — ${PROSE(i, CHILDREN).slice(0, PAD)}`);
  } else {
    hubLines.push(`  - Child ${i} embeds`, `    - {{embed ((${uuid(id)}))}}`);
  }
}
hubLines.push("");
fs.writeFileSync(HUB, hubLines.join("\n"));

for (let p = 1; p <= SRC_PAGES; p++) {
  const lines = [`- Source page ${p} root`, `  - ${PROSE(p, SRC_PAGES)}`];
  for (let b = 1; b <= PER_PAGE; b++) {
    const id = (p - 1) * PER_PAGE + b;
    lines.push(`  - Source block ${p}.${b}`, `    id:: ${uuid(id)}`, `    - ${PROSE(b, PER_PAGE)}`);
  }
  lines.push("");
  fs.writeFileSync(`${GRAPH}/pages/Src ${String(p).padStart(3, "0")}.md`, lines.join("\n"));
}

const now = new Date();
const journal = `${now.getFullYear()}_${String(now.getMonth() + 1).padStart(2, "0")}_${String(now.getDate()).padStart(2, "0")}`;
fs.writeFileSync(`${GRAPH}/journals/${journal}.md`, "- open [[Hub]]\n");
console.log(`graph: ${GRAPH} (${CHILDREN} children, ${totalIds} ids across ${SRC_PAGES} source pages, hub ${(fs.statSync(HUB).size / 1024).toFixed(0)} KiB)`);

// ---------------------------------------------------------------------------
// App boot (pattern: scripts/e2e-block-embed.mjs)
// ---------------------------------------------------------------------------
const env = {
  ...process.env,
  TINE_GRAPH: GRAPH,
  XDG_DATA_HOME: `${TMP}/xdg/data`, XDG_CONFIG_HOME: `${TMP}/xdg/config`, XDG_CACHE_HOME: `${TMP}/xdg/cache`,
  WEBKIT_DISABLE_DMABUF_RENDERER: "1", WEBKIT_DISABLE_COMPOSITING_MODE: "1", LIBGL_ALWAYS_SOFTWARE: "1", GDK_BACKEND: "x11",
};
const log = fs.openSync(`${TMP}/tauri-driver.log`, "w");
const td = spawn(TD, webdriverServerArgs(DRIVER_PORT, NATIVE_PORT, process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver"), {
  env, stdio: ["ignore", log, log], detached: true,
});
await sleep(2500);

let browser;
try {
  browser = await remote({
    hostname: "127.0.0.1", port: DRIVER_PORT, path: "/",
    capabilities: tauriCapabilities(APP, "tine-408-research"),
    logLevel: "error",
  });

  await browser.$(".ls-block, .page-title").waitForExist({ timeout: 30_000 });
  // Navigate to Hub via the journal link (routed page, not the feed).
  for (const selector of ["a.page-ref=Hub", "span.page-ref=Hub", "*=Hub"]) {
    const link = await browser.$(selector);
    if (await link.isExisting()) { await link.click(); break; }
  }
  await browser.waitUntil(async () => (await browser.$("h1.page-title").getText()).trim() === "Hub", {
    timeout: 15_000, timeoutMsg: "Hub page did not open",
  });

  // Install the in-page timeline recorder BEFORE expanding.
  await browser.execute(() => {
    window.__samples = [];
    window.__t0 = null; // set right before the expand click
    window.__longTasks = [];
    try {
      new PerformanceObserver((list) => {
        for (const e of list.getEntries()) window.__longTasks.push({ start: Math.round(e.startTime), dur: Math.round(e.duration) });
      }).observe({ entryTypes: ["longtask"] });
    } catch { /* longtask unsupported — ignore */ }
    const unresolved = () =>
      [...document.querySelectorAll(".block-ref")].filter((el) => /^\(\([0-9a-f-]{8}/.test(el.textContent ?? "")).length;
    const tick = () => {
      window.__samples.push({
        t: Math.round(performance.now()),
        rows: document.querySelectorAll(".ls-block").length,
        refs: document.querySelectorAll(".block-ref").length,
        unresolved: unresolved(),
        embedHosts: document.querySelectorAll(".embed-block").length,
        embedsMissing: document.querySelectorAll(".embed-missing").length,
        embedsFallback: document.querySelectorAll(".embed-block .ref-block").length,
        embedsLive: document.querySelectorAll(".embed-block .live-ref-group .ls-block").length,
      });
      if (window.__samples.length < 20000) requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });

  // Expand the GH408 parent (its row carries the only `collapsed::` on the page
  // and is the first top-level block). __t0 is set in the same execute so the
  // click-to-content delta can't absorb WebDriver round-trip latency.
  const clicked = await browser.execute(() => {
    const parent = document.querySelector('.ls-block[data-block-id] > .block-main .collapse-toggle.has-children');
    if (!parent) return false;
    window.__t0 = performance.now();
    parent.click();
    return true;
  });
  if (!clicked) throw new Error("parent collapse toggle not found");

  // Wait until the timeline settles: sample until 5s pass with no change in
  // any metric (or a hard 90s cap — the reported behavior is ~10s).
  // Wait until the timeline settles: poll from Node (a single long-running
  // in-page script exceeds WebDriver's 30s script timeout). Settled = ~6s with
  // no change in any metric; recorder cap = not settled.
  let lastState = "";
  let stablePolls = 0;
  let settled = false;
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    const probe = await browser.execute(() => ({
      last: (() => { const { t, ...rest } = window.__samples[window.__samples.length - 1] ?? {}; return rest ?? null; })(),
      n: window.__samples.length,
    }));
    const cur = JSON.stringify(probe.last ?? {});
    if (cur === lastState) {
      stablePolls += 1;
      if (stablePolls >= 12) { settled = true; break; } // ~6s at 500ms
    } else {
      stablePolls = 0;
      lastState = cur;
    }
    if (probe.n >= 20000) break; // recorder cap hit — not settled
    await sleep(500);
  }
  const samples = await browser.execute(() => window.__samples);
  const t0 = await browser.execute(() => window.__t0);
  const longTasks = (await browser.execute(() => window.__longTasks)) ?? [];

  // Post-process: phase timings relative to the expand click.
  const rel = (s) => Math.round(s.t - t0);
  const first = (pred) => samples.find(pred);
  const last = (pred) => [...samples].reverse().find(pred);

  const rowsStable = samples.reduce((acc, s, i) => (i > 0 && s.rows !== samples[i - 1].rows ? s : acc), samples[0]);
  const firstRef = first((s) => s.refs > 0 && s.unresolved < s.refs);
  const lastRef = last((s) => s.unresolved > 0);
  const firstEmbedContent = first((s) => (s.embedsFallback + s.embedsLive) > 0);
  const lastEmbedChange = last((s) => (s.embedsFallback + s.embedsLive) !== ((samples[0].embedsFallback ?? 0) + (samples[0].embedsLive ?? 0)));
  const end = samples[samples.length - 1] ?? { rows: 0, refs: 0, unresolved: 0, embedHosts: 0, embedsMissing: 0, embedsFallback: 0, embedsLive: 0 };

  console.log(`settled: ${settled}   samples: ${samples.length}   span: ${rel(end)} ms`);
  console.log(`structure rows: start ${samples[0].rows} → ${rowsStable ? `stable@+${rel(rowsStable)}ms` : "n/a"} → final ${end.rows}`);
  console.log(`block refs:     first resolved ${firstRef ? `+${rel(firstRef)}ms` : "never"}; last unresolved→resolved ${lastRef ? `+${rel(lastRef)}ms` : "n/a"}; final ${end.refs} refs, ${end.unresolved} unresolved`);
  console.log(`embeds:         hosts ${end.embedHosts}, missing ${end.embedsMissing}, shallow rows ${end.embedsFallback}, live rows ${end.embedsLive}; first content ${firstEmbedContent ? `+${rel(firstEmbedContent)}ms` : "never"}; last content change ${lastEmbedChange ? `+${rel(lastEmbedChange)}ms` : "n/a"}`);

  // Coarse timeline: every ~250ms, print only marks where something changed.
  const marks = [];
  let nextMark = 0;
  let prev = null;
  for (const s of samples) {
    if (rel(s) >= nextMark) {
      const cur = `${s.rows}/${s.refs}/${s.unresolved}/${s.embedsMissing ?? 0}/${s.embedsFallback ?? 0}/${s.embedsLive ?? 0}`;
      if (cur !== prev) {
        marks.push(`${nextMark}ms: rows=${s.rows} refs=${s.refs} unresolved=${s.unresolved} embeds(missing/shallow/live)=${s.embedsMissing ?? 0}/${s.embedsFallback ?? 0}/${s.embedsLive ?? 0}`);
        prev = cur;
      }
      nextMark += 250;
    }
  }
  console.log("timeline:\n  " + marks.join("\n  "));

  const after = longTasks.filter((l) => l.start >= t0);
  const longTotal = after.reduce((a, l) => a + l.dur, 0);
  console.log(`longtasks (>=50ms) after expand: ${after.length}, total ${longTotal} ms` + (after.length
    ? `; top: ${[...after].sort((a, b) => b.dur - a.dur).slice(0, 5).map((l) => `${l.dur}ms@+${Math.round(l.start - t0)}ms`).join(", ")}`
    : ""));

  fs.writeFileSync(`${TMP}/timeline.json`, JSON.stringify({ t0, samples, longTasks }, null, 2));
  console.log(`raw timeline → ${TMP}/timeline.json`);

  // Optional `--probe`: after settle, time the backend `resolve_blocks` IPC
  // directly from inside the page at several batch sizes, isolating the
  // backend+serialization cost from the frontend mount cost.
  if (process.argv.includes("--probe")) {
    const ids = [];
    for (let p = 1; p <= SRC_PAGES; p++) {
      const src = fs.readFileSync(`${GRAPH}/pages/Src ${String(p).padStart(3, "0")}.md`, "utf8");
      for (const m of src.matchAll(/id:: ([0-9a-f-]{36})/g)) ids.push(m[1]);
    }
    const r = await browser.execute(async (uuids) => {
      const out = {};
      try {
        if (!window.__TAURI_INTERNALS__ || typeof window.__TAURI_INTERNALS__.invoke !== "function") {
          return JSON.stringify({ error: "no __TAURI_INTERNALS__.invoke in this build" });
        }
        for (const n of [100, 1000, 4000, 8000]) {
          if (n > uuids.length) continue;
          const slice = uuids.slice(0, n);
          const t = performance.now();
          const res = await window.__TAURI_INTERNALS__.invoke("resolve_blocks", { uuids: slice, bindingGeneration: 1 });
          out[`${n} uuids`] = `${Math.round(performance.now() - t)}ms`;
          out[`${n} resolved`] = Array.isArray(res) ? res.filter(Boolean).length : String(res).slice(0, 80);
        }
      } catch (e) {
        out.error = String(e);
      }
      return JSON.stringify(out);
    }, ids);
    console.log(`backend resolve_blocks, timed in-page (${ids.length} ids collected): ${r}`);
  }
} finally {
  if (browser) await browser.deleteSession();
  try { process.kill(-td.pid, "SIGKILL"); } catch { try { td.kill("SIGKILL"); } catch {} }
  if (!KEEP) fs.rmSync(TMP, { recursive: true, force: true });
  else console.log(`kept ${TMP} (--keep)`);
}
