// Real-app regression for long-PDF scroll geometry and bounded rendering.
// The fixture is generated at runtime so no user document enters the repository.
import { spawn } from "node:child_process";
import { remote } from "webdriverio";
import { setTimeout as sleep } from "node:timers/promises";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  startWebdriverApplication,
  stopWebdriverApplication,
  tauriCapabilities,
  webdriverServerArgs,
} from "./e2e-capabilities.mjs";
import { ensureDisplay, stopDisplay } from "./lib/e2e-display.mjs";

const PAGE_COUNT = 34;
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APP = process.env.TINE_APP || path.join(ROOT, "target/release/tine");
const TD = process.env.TAURI_DRIVER ||
  (process.env.CARGO_HOME ? path.join(process.env.CARGO_HOME, "bin", "tauri-driver") : "tauri-driver");
const WD = process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver";
const DRIVER_PORT = Number(process.env.E2E_DRIVER_PORT || 4630);
const NATIVE_PORT = Number(process.env.E2E_NATIVE_PORT || 4631);
const TMP = path.join(os.tmpdir(), `tine-pdf-scroll-resources-${process.pid}`);
const GRAPH = path.join(TMP, "graph");
const ARTIFACTS = process.env.E2E_ARTIFACT_DIR || TMP;

function makeLongPdf() {
  const pageIds = Array.from({ length: PAGE_COUNT }, (_, index) => 4 + index * 2);
  const objects = [
    "<< /Type /Catalog /Pages 2 0 R >>",
    `<< /Type /Pages /Kids [${pageIds.map((id) => `${id} 0 R`).join(" ")}] /Count ${PAGE_COUNT} >>`,
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
  ];
  for (let page = 1; page <= PAGE_COUNT; page += 1) {
    const pageId = 4 + (page - 1) * 2;
    const contentId = pageId + 1;
    const content = [
      "BT /F1 24 Tf 72 720 Td",
      `(Long PDF resource regression page ${page}) Tj`,
      "0 -32 Td /F1 13 Tf",
      "(The page wrapper must retain its geometry when its canvas is evicted.) Tj ET",
    ].join("\n");
    objects.push(
      `<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents ${contentId} 0 R >>`,
      `<< /Length ${Buffer.byteLength(content)} >>\nstream\n${content}\nendstream`,
    );
  }

  let pdf = "%PDF-1.7\n";
  const offsets = [0];
  for (const [index, object] of objects.entries()) {
    offsets.push(Buffer.byteLength(pdf));
    pdf += `${index + 1} 0 obj\n${object}\nendobj\n`;
  }
  const xref = Buffer.byteLength(pdf);
  pdf += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
  for (let index = 1; index < offsets.length; index += 1) {
    pdf += `${String(offsets[index]).padStart(10, "0")} 00000 n \n`;
  }
  pdf += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return Buffer.from(pdf, "utf8");
}

fs.rmSync(TMP, { recursive: true, force: true });
for (const dir of ["pages", "journals", "logseq", "assets"]) {
  fs.mkdirSync(path.join(GRAPH, dir), { recursive: true });
}
for (const dir of ["data", "config", "cache"]) {
  fs.mkdirSync(path.join(TMP, "xdg", dir), { recursive: true });
}
fs.mkdirSync(ARTIFACTS, { recursive: true });
fs.writeFileSync(path.join(GRAPH, "assets", "long.pdf"), makeLongPdf());
const now = new Date();
const journal = `${now.getFullYear()}_${String(now.getMonth() + 1).padStart(2, "0")}_${String(now.getDate()).padStart(2, "0")}.md`;
fs.writeFileSync(path.join(GRAPH, "journals", journal), "- ![Long PDF](../assets/long.pdf)\n");

const env = {
  ...process.env,
  TINE_GRAPH: GRAPH,
  XDG_DATA_HOME: path.join(TMP, "xdg", "data"),
  XDG_CONFIG_HOME: path.join(TMP, "xdg", "config"),
  XDG_CACHE_HOME: path.join(TMP, "xdg", "cache"),
  APPDATA: path.join(TMP, "appdata"),
  LOCALAPPDATA: path.join(TMP, "localappdata"),
  WEBKIT_DISABLE_DMABUF_RENDERER: "1",
  WEBKIT_DISABLE_COMPOSITING_MODE: "1",
  LIBGL_ALWAYS_SOFTWARE: "1",
  GDK_BACKEND: "x11",
};

function assert(condition, message, details) {
  if (!condition) {
    throw new Error(`${message}${details === undefined ? "" : `: ${JSON.stringify(details)}`}`);
  }
}

async function click(browser, selector) {
  const target = await browser.$(selector);
  await target.waitForExist({ timeout: 15_000 });
  await target.click();
}

async function sample(browser) {
  return browser.execute(() => {
    const scroll = document.querySelector(".pdf-scroll");
    if (!(scroll instanceof HTMLElement)) return null;
    const viewport = scroll.getBoundingClientRect();
    const pages = [...scroll.querySelectorAll(".pdf-page")].map((page) => {
      const rect = page.getBoundingClientRect();
      const canvas = page.querySelector("canvas");
      return {
        page: Number(page.dataset.page),
        height: rect.height,
        visible: rect.bottom > viewport.top && rect.top < viewport.bottom,
        canvasPixels: canvas instanceof HTMLCanvasElement ? canvas.width * canvas.height : 0,
      };
    });
    return {
      scrollTop: scroll.scrollTop,
      scrollHeight: scroll.scrollHeight,
      clientHeight: scroll.clientHeight,
      pages,
      visible: pages.filter((page) => page.visible).map((page) => page.page),
      canvasCount: pages.filter((page) => page.canvasPixels > 0).length,
      canvasPixels: pages.reduce((total, page) => total + page.canvasPixels, 0),
    };
  });
}

async function wheel(browser, deltaY, id) {
  const center = await browser.execute(() => {
    const rect = document.querySelector(".pdf-scroll")?.getBoundingClientRect();
    return rect ? { x: Math.round(rect.left + rect.width / 2), y: Math.round(rect.top + rect.height / 2) } : null;
  });
  assert(center, "PDF scroller has no geometry");
  try {
    await browser.performActions([{
      type: "wheel",
      id,
      actions: [{ type: "scroll", origin: "viewport", x: center.x, y: center.y, deltaX: 0, deltaY, duration: 80 }],
    }]);
  } finally {
    await browser.releaseActions().catch(() => {});
  }
}

await ensureDisplay({ geometry: "1600x1100x24" });
const driverLog = fs.openSync(path.join(ARTIFACTS, "tauri-driver.log"), "w");
let webviewTarget;
let driver;
let browser;
const observations = [];
try {
  webviewTarget = await startWebdriverApplication(APP, env, NATIVE_PORT, "pdf-scroll-resources");
  driver = spawn(TD, webdriverServerArgs(DRIVER_PORT, NATIVE_PORT, WD), {
    env: webviewTarget.env,
    stdio: ["ignore", driverLog, driverLog],
    detached: true,
  });
  await sleep(2500);
  browser = await remote({
    hostname: "127.0.0.1",
    port: DRIVER_PORT,
    path: "/",
    logLevel: "error",
    connectionRetryCount: 1,
    connectionRetryTimeout: 60_000,
    capabilities: tauriCapabilities(APP, "pdf-scroll-resources", process.platform, webviewTarget.debuggerAddress),
  });
  await browser.$(".pdf-link").waitForExist({ timeout: 30_000 });
  await click(browser, ".pdf-link");
  await browser.waitUntil(() => browser.execute(() =>
    document.querySelector(".pdf-viewer")?.getAttribute("data-pdf-ready") === "true"), {
    timeout: 30_000,
    timeoutMsg: "long PDF did not become ready",
  });

  // Allow intersection callbacks, rendering, and cache eviction to settle.
  await sleep(2500);
  const ready = await sample(browser);
  observations.push({ label: "ready", ...ready });
  assert(ready?.pages.length === PAGE_COUNT, "long PDF did not build every page wrapper", ready?.pages.length);
  const shortestPage = Math.min(...ready.pages.map((page) => page.height));
  assert(shortestPage >= 500, "offscreen PDF page wrappers collapsed after canvas eviction", { shortestPage, scrollHeight: ready.scrollHeight });
  assert(ready.canvasCount < PAGE_COUNT / 2, "opening a long PDF rendered distant offscreen pages", ready.canvasCount);
  assert(ready.canvasPixels <= 52_000_000, "PDF canvases exceeded the desktop backing-store budget", ready.canvasPixels);

  let previousFirst = ready.visible[0] ?? 1;
  for (let index = 0; index < 12; index += 1) {
    await wheel(browser, 240, `gentle-${index}`);
    await sleep(180);
    const state = await sample(browser);
    observations.push({ label: `gentle-${index + 1}`, ...state });
    const first = state.visible[0] ?? previousFirst;
    assert(first >= previousFirst, "gentle downward scrolling moved backward", { previousFirst, first });
    assert(first - previousFirst <= 2, "gentle scrolling skipped across PDF pages", { previousFirst, first, visible: state.visible });
    assert(Math.min(...state.pages.map((page) => page.height)) >= 500, "PDF page geometry collapsed while scrolling", state.visible);
    assert(state.canvasPixels <= 52_000_000, "PDF canvases exceeded the desktop backing-store budget while scrolling", state.canvasPixels);
    previousFirst = first;
  }

  fs.writeFileSync(path.join(ARTIFACTS, "pdf-scroll-resources.json"), `${JSON.stringify(observations, null, 2)}\n`);
  console.log(JSON.stringify({
    ok: true,
    pageCount: PAGE_COUNT,
    initialCanvasCount: ready.canvasCount,
    shortestPage,
    scrollHeight: ready.scrollHeight,
    finalVisible: observations.at(-1)?.visible,
  }));
} catch (error) {
  fs.writeFileSync(path.join(ARTIFACTS, "pdf-scroll-resources.json"), `${JSON.stringify(observations, null, 2)}\n`);
  try { await browser?.saveScreenshot(path.join(ARTIFACTS, "pdf-scroll-resources-failure.png")); } catch {}
  throw error;
} finally {
  try { await browser?.deleteSession(); } catch {}
  try { if (driver?.pid) process.kill(-driver.pid, "SIGKILL"); } catch {}
  stopWebdriverApplication(webviewTarget);
  stopDisplay();
  fs.closeSync(driverLog);
}
