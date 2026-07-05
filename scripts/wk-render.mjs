// Render an HTML file in REAL WebKitGTK (the engine Tine uses) via WebKitWebDriver
// + the system webkit2gtk-4.1 MiniBrowser, and screenshot it. This reveals
// WebKitGTK-only rendering (font/ligature/glyph) that Chromium hides.
// Usage (under xvfb-run):  node scripts/wk-render.mjs <file.html> <out.png>
import { remote } from "webdriverio";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";
import { pathToFileURL } from "node:url";
import { writeFileSync } from "node:fs";

const src = process.argv[2] || "/tmp/tine-sample-export/print-sample.html";
const out = process.argv[3] || "/tmp/wk-render.png";
const MINIBROWSER = "/usr/lib/x86_64-linux-gnu/webkit2gtk-4.1/MiniBrowser";

const drv = spawn("/usr/bin/WebKitWebDriver", ["--port=4455", "--host=local"], { stdio: "inherit" });
await sleep(2500);

let browser;
try {
  browser = await remote({
    hostname: "127.0.0.1",
    port: 4455,
    path: "/",
    capabilities: {
      browserName: "MiniBrowser",
      "webkitgtk:browserOptions": { binary: MINIBROWSER, args: ["--automation"] },
      "wdio:enforceWebDriverClassic": true,
    },
    logLevel: "error",
    connectionRetryCount: 1,
    connectionRetryTimeout: 60000,
  });
  await browser.setWindowSize(860, 1200);
  await browser.url(pathToFileURL(src).href);
  await sleep(2500); // let KaTeX/hljs (CDN) + fonts settle
  const b64 = await browser.takeScreenshot();
  writeFileSync(out, Buffer.from(b64, "base64"));
  console.log("wrote", out);
} catch (e) {
  console.error("ERR", String(e).split("\n").slice(0, 6).join("\n"));
} finally {
  try { await browser?.deleteSession(); } catch {}
  drv.kill("SIGKILL");
}
