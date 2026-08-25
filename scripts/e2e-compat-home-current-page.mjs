// Real-app compatibility journey for GH #269 and #301:
// - config.edn's Logseq :default-home owner controls startup;
// - the visible Settings row clears/recreates only :default-home/:page;
// - an advanced :inputs [:current-page] query reaches the native engine and
//   returns only references to the focused startup page.
import { spawn } from "node:child_process";
import fs from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";
import { remote } from "webdriverio";
import { ensureDisplay } from "./lib/e2e-display.mjs";
import { tauriCapabilities, webdriverServerArgs } from "./e2e-capabilities.mjs";

await ensureDisplay();

const graph = "/tmp/tgraph-compat-home-current-page";
const xdg = "/tmp/txdg-compat-home-current-page";
const app = process.env.TINE_APP || `${process.env.HOME}/research/tine`;
const tauriDriver = process.env.TAURI_DRIVER
  || (process.env.CARGO_HOME ? `${process.env.CARGO_HOME}/bin/tauri-driver` : "tauri-driver");
const driverPort = Number(process.env.E2E_DRIVER_PORT || 4496);
const nativePort = Number(process.env.E2E_NATIVE_PORT || 4497);

fs.rmSync(graph, { recursive: true, force: true });
fs.rmSync(xdg, { recursive: true, force: true });
for (const dir of ["pages", "journals", "logseq"]) {
  fs.mkdirSync(`${graph}/${dir}`, { recursive: true });
}
for (const dir of ["data", "config", "cache"]) {
  fs.mkdirSync(`${xdg}/${dir}`, { recursive: true });
}
const configPath = `${graph}/logseq/config.edn`;
fs.writeFileSync(
  configPath,
  '{:default-home {:page "Home" :sidebar ["Contents"]}\n :start-of-week 2}\n',
);
fs.writeFileSync(
  `${graph}/pages/Home.md`,
  `- Home landing sentinel
- {{query [:find (pull ?b [*]) :in $ ?current-page :where [?p :block/name ?current-page] [?b :block/refs ?p]] :inputs [:current-page]}}
`,
);
fs.writeFileSync(`${graph}/pages/Other.md`, "- Other page\n");
fs.writeFileSync(
  `${graph}/pages/Pins.md`,
  "- TODO pinned Home [[Home]]\n- TODO pinned Other [[Other]]\n",
);

const env = {
  ...process.env,
  TINE_GRAPH: graph,
  XDG_DATA_HOME: `${xdg}/data`,
  XDG_CONFIG_HOME: `${xdg}/config`,
  XDG_CACHE_HOME: `${xdg}/cache`,
  WEBKIT_DISABLE_DMABUF_RENDERER: "1",
  LIBGL_ALWAYS_SOFTWARE: "1",
  WEBKIT_DISABLE_COMPOSITING_MODE: "1",
  GDK_BACKEND: "x11",
};
const log = fs.openSync("/tmp/td-compat-home-current-page.log", "w");
const driver = spawn(tauriDriver, webdriverServerArgs(driverPort, nativePort, process.env.WEBKIT_DRIVER || "/usr/bin/WebKitWebDriver"), { env, stdio: ["ignore", log, log], detached: true });
await sleep(3000);

async function waitForConfig(predicate, message) {
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    const text = fs.readFileSync(configPath, "utf8");
    if (predicate(text)) return text;
    await sleep(100);
  }
  throw new Error(message);
}

let browser;
let failure;
try {
  browser = await remote({
    hostname: "127.0.0.1",
    port: driverPort,
    path: "/",
    capabilities: tauriCapabilities(app, "compat-home-current-page"),
    logLevel: "error",
    connectionRetryCount: 1,
    connectionRetryTimeout: 60_000,
  });
  await browser.waitUntil(async () => {
    const body = await browser.execute(() => document.body.innerText);
    return body.includes("Home landing sentinel") && body.includes("TODO pinned Home");
  }, { timeout: 25_000, timeoutMsg: "config-owned Home page/current-page query did not render" });
  const startupText = await browser.execute(() => document.body.innerText);
  if (startupText.includes("TODO pinned Other")) {
    throw new Error("current-page input leaked a reference to the other page");
  }

  await browser.$('button[title^="Settings"]').click();
  await browser.$("//button[contains(concat(' ', normalize-space(@class), ' '), ' settings-nav-item ') and normalize-space(.)='Graph']").click();
  const homeRow = await browser.$('[data-setting-label="Home page"]');
  await homeRow.waitForExist({ timeout: 10_000 });
  await browser.execute(() => {
    const row = document.querySelector('[data-setting-label="Home page"]');
    const clear = [...(row?.querySelectorAll("button") ?? [])]
      .find((button) => button.textContent?.trim() === "Clear");
    clear?.setAttribute("data-e2e-home-clear", "true");
  });
  await browser.$('[data-e2e-home-clear="true"]').click();
  const cleared = await waitForConfig(
    (text) => !/:page\s+"Home"/.test(text),
    "Settings Clear did not remove :default-home/:page",
  );
  if (!cleared.includes(':sidebar ["Contents"]') || !cleared.includes(":start-of-week 2")) {
    throw new Error(`clearing Home damaged sibling config: ${cleared}`);
  }

  const input = await browser.$('[data-setting-label="Home page"] input.settings-input');
  await input.waitForExist({ timeout: 10_000 });
  await input.setValue("Home");
  await browser.waitUntil(async () => {
    const marked = await browser.execute(() => {
      const row = document.querySelector('[data-setting-label="Home page"]');
      const choice = [...(row?.querySelectorAll("button") ?? [])]
        .find((button) => button.textContent?.trim() === "Home");
      choice?.setAttribute("data-e2e-home-choice", "true");
      return Boolean(choice);
    });
    return marked;
  }, { timeout: 10_000, timeoutMsg: "Home page picker did not offer Home" });
  await browser.$('[data-e2e-home-choice="true"]').click();
  const restored = await waitForConfig(
    (text) => /:page\s+"Home"/.test(text),
    "Settings picker did not restore :default-home/:page",
  );
  if (!restored.includes(':sidebar ["Contents"]')) {
    throw new Error(`restoring Home damaged :sidebar: ${restored}`);
  }
  console.log("PASS: config-owned Home and typed current-page query crossed the real app boundary");
} catch (error) {
  failure = error;
  console.error("FAIL:", error?.message ?? error);
  try {
    if (browser) {
      fs.writeFileSync(
        "/tmp/e2e-compat-home-current-page.png",
        await browser.takeScreenshot(),
        "base64",
      );
    }
  } catch {}
} finally {
  try { if (browser) await browser.deleteSession(); } catch {}
  try { process.kill(-driver.pid, "SIGKILL"); } catch {}
}

process.exit(failure ? 1 : 0);
