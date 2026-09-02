import fs from "node:fs";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import net from "node:net";
import { setTimeout as sleep } from "node:timers/promises";
import path from "node:path";

export async function freeLoopbackPort(excluded = new Set()) {
  while (true) {
    const port = await new Promise((resolve, reject) => {
      const server = net.createServer();
      server.once("error", reject);
      server.listen(0, "127.0.0.1", () => {
        const address = server.address();
        server.close(() => resolve(address.port));
      });
    });
    if (!excluded.has(port)) return port;
  }
}

export function tauriCapabilities(
  application,
  session = "default",
  platform = process.platform,
  debuggerAddress,
) {
  if (platform === "win32") {
    // When the app was started explicitly with a fixed remote-debugging port,
    // use EdgeDriver's documented WebView2 attach mode.  This deliberately
    // avoids its launch-mode DevToolsActivePort handshake, which is not
    // reliable on current hosted Windows/WebView2 runners.
    if (debuggerAddress) {
      return {
        browserName: "webview2",
        "wdio:enforceWebDriverClassic": true,
        "ms:edgeChromium": true,
        "ms:edgeOptions": { debuggerAddress },
      };
    }
    const root = process.env.E2E_WEBVIEW_USER_DATA_ROOT;
    if (!root) throw new Error("Windows WebView2 E2E requires E2E_WEBVIEW_USER_DATA_ROOT");
    const userDataFolder = path.join(root, session.replaceAll(/[^A-Za-z0-9_-]/g, "-"));
    fs.mkdirSync(userDataFolder, { recursive: true });
    return {
      browserName: "webview2",
      "wdio:enforceWebDriverClassic": true,
      "ms:edgeChromium": true,
      "ms:edgeOptions": {
        binary: application,
        args: [],
        // Give EdgeDriver and the hosted WebView one explicit writable profile
        // location. This is the native capability Tauri-driver would forward.
        webviewOptions: { userDataFolder },
      },
    };
  }
  return {
    browserName: "wry",
    "wdio:enforceWebDriverClassic": true,
    "tauri:options": { application },
  };
}

export function windowsUserDataFolder(session, env = process.env) {
  const root = env.E2E_WEBVIEW_USER_DATA_ROOT;
  if (!root) throw new Error("Windows WebView2 E2E requires E2E_WEBVIEW_USER_DATA_ROOT");
  const userDataFolder = path.join(root, session.replaceAll(/[^A-Za-z0-9_-]/g, "-"));
  fs.mkdirSync(userDataFolder, { recursive: true });
  return userDataFolder;
}

function withFixedRemoteDebuggingPort(argumentsValue, port) {
  const withoutDynamicPort = String(argumentsValue || "")
    .replace(/(?:^|\s)--remote-debugging-port(?:=|\s+)\S+/g, " ")
    .trim();
  return [withoutDynamicPort, `--remote-debugging-port=${port}`].filter(Boolean).join(" ");
}

async function waitForDevTools(debuggerAddress, applicationProcess, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  const endpoint = `http://${debuggerAddress}/json/version`;
  let lastError = "endpoint not ready";
  while (Date.now() < deadline) {
    if (applicationProcess.exitCode !== null) {
      throw new Error(`WebView2 host exited before DevTools became ready (exit ${applicationProcess.exitCode})`);
    }
    try {
      const response = await fetch(endpoint, { signal: AbortSignal.timeout(1_000) });
      if (response.ok) {
        const version = await response.json();
        if (version.webSocketDebuggerUrl) return version;
        lastError = `missing webSocketDebuggerUrl in ${JSON.stringify(version)}`;
      } else {
        lastError = `HTTP ${response.status}`;
      }
    } catch (error) {
      lastError = String(error);
    }
    await sleep(100);
  }
  throw new Error(`WebView2 DevTools did not become ready at ${endpoint}: ${lastError}`);
}

export async function startWebdriverApplication(
  application,
  env,
  debuggerPort,
  session = "default",
  platform = process.platform,
) {
  if (platform !== "win32") return { env, applicationProcess: undefined, debuggerAddress: undefined };

  const userDataFolder = windowsUserDataFolder(session, env);
  const debuggerAddress = `127.0.0.1:${debuggerPort}`;
  const applicationEnv = {
    ...env,
    TAURI_AUTOMATION: "true",
    TAURI_WEBVIEW_AUTOMATION: "true",
    WEBVIEW2_USER_DATA_FOLDER: userDataFolder,
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: withFixedRemoteDebuggingPort(
      env.WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS,
      debuggerPort,
    ),
  };
  const applicationLogHandles = [];
  const openApplicationLog = (variable) => {
    const file = applicationEnv[variable];
    if (!file) return "ignore";
    const handle = fs.openSync(file, "w");
    applicationLogHandles.push(handle);
    return handle;
  };
  let applicationProcess;
  try {
    applicationProcess = spawn(application, [], {
      env: applicationEnv,
      stdio: [
        "ignore",
        openApplicationLog("TINE_E2E_APPLICATION_STDOUT_LOG"),
        openApplicationLog("TINE_E2E_APPLICATION_STDERR_LOG"),
      ],
      windowsHide: false,
    });
  } catch (error) {
    for (const handle of applicationLogHandles) fs.closeSync(handle);
    throw error;
  }
  try {
    await waitForDevTools(debuggerAddress, applicationProcess);
  } catch (error) {
    stopWebdriverApplication({ applicationProcess }, platform);
    throw error;
  }
  return {
    env: applicationEnv,
    applicationProcess,
    applicationLogHandles,
    debuggerAddress,
    userDataFolder,
  };
}

export function stopWebdriverApplication(target, platform = process.platform) {
  const applicationProcess = target?.applicationProcess;
  if (applicationProcess && applicationProcess.exitCode === null) {
    if (platform === "win32") {
      spawnSync("taskkill", ["/PID", String(applicationProcess.pid), "/T", "/F"], { stdio: "ignore" });
    } else {
      applicationProcess.kill("SIGKILL");
    }
  }
  for (const handle of target?.applicationLogHandles || []) {
    try { fs.closeSync(handle); } catch {}
  }
}

export function webdriverServerArgs(port, nativePort, nativeDriver, platform = process.platform) {
  if (platform === "win32") return [`--port=${port}`];
  return [
    "--port", String(port),
    "--native-port", String(nativePort),
    "--native-driver", nativeDriver,
  ];
}

const WEBDRIVER_SESSION_ENV = "TINE_E2E_WEBDRIVER_SESSION";

export function webdriverSessionToken(scenario, driverPort, nativePort) {
  const label = String(scenario).replaceAll(/[^A-Za-z0-9_.-]/g, "-");
  return `tine-e2e:${label}:${driverPort}:${nativePort}`;
}

export function findTaggedWebdriverProcesses(
  token,
  procRoot = "/proc",
  readFile = fs.readFileSync,
) {
  if (!fs.existsSync(procRoot)) return [];
  const needle = `${WEBDRIVER_SESSION_ENV}=${token}`;
  const matches = [];
  for (const entry of fs.readdirSync(procRoot, { withFileTypes: true })) {
    if (!entry.isDirectory() || !/^\d+$/.test(entry.name)) continue;
    const pid = Number(entry.name);
    if (pid === process.pid) continue;
    try {
      const variables = readFile(path.join(procRoot, entry.name, "environ"), "utf8").split("\0");
      if (variables.includes(needle)) matches.push(pid);
    } catch {
      // Processes may exit or become unreadable while /proc is enumerated.
    }
  }
  return matches.sort((a, b) => a - b);
}

/**
 * Own one native WebDriver session across crashes and stalled HTTP calls.
 *
 * WebdriverIO already gives each protocol request `connectionRetryTimeout`.
 * `run()` is the second, process-owning deadline around session-level calls:
 * if WebKit ignores the request abort, its tagged process tree is killed and
 * cannot consume one of WebKitWebDriver's finite session slots indefinitely.
 */
export function createWebdriverLifecycle({
  scenario,
  driverPort,
  nativePort,
  callTimeoutMs = 60_000,
  cleanupGraceMs = 1_000,
  survivorTimeoutMs = 2_000,
  survivorPollMs = 50,
  platform = process.platform,
  procRoot = "/proc",
  kill = process.kill.bind(process),
  sleepImpl = sleep,
} = {}) {
  const token = webdriverSessionToken(scenario, driverPort, nativePort);
  const evidence = { token, reaped: [], timeouts: [] };

  const taggedEnvironment = (base = process.env) => ({
    ...base,
    [WEBDRIVER_SESSION_ENV]: token,
  });

  const taggedPids = () => platform === "linux"
    ? findTaggedWebdriverProcesses(token, procRoot)
    : [];

  const signal = (pid, name) => {
    try {
      kill(pid, name);
      return true;
    } catch {
      return false;
    }
  };

  async function reap(reason, { graceMs = cleanupGraceMs } = {}) {
    const initial = taggedPids();
    const signalFailures = [];
    for (const pid of initial) {
      if (!signal(pid, "SIGTERM")) signalFailures.push({ pid, signal: "SIGTERM" });
    }
    if (initial.length && graceMs > 0) await sleepImpl(graceMs);
    const killTargets = taggedPids();
    for (const pid of killTargets) {
      if (!signal(pid, "SIGKILL")) signalFailures.push({ pid, signal: "SIGKILL" });
    }
    const deadline = Date.now() + survivorTimeoutMs;
    let survivors = taggedPids();
    while (survivors.length && Date.now() < deadline) {
      await sleepImpl(survivorPollMs);
      survivors = taggedPids();
    }
    const record = { reason, term: initial, kill: killTargets, signalFailures, survivors };
    if (initial.length || killTargets.length || signalFailures.length || survivors.length) {
      evidence.reaped.push(record);
    }
    if (survivors.length) {
      throw new Error(`WebDriver cleanup ${JSON.stringify(reason)} left exact-token survivors: ${survivors.join(", ")}`);
    }
    return record;
  }

  async function run(label, operation, timeoutMs = callTimeoutMs) {
    let timer;
    const timeout = new Promise((_, reject) => {
      timer = setTimeout(async () => {
        evidence.timeouts.push({ label, timeoutMs });
        let cleanupError;
        try { await reap(`timeout:${label}`, { graceMs: 0 }); } catch (error) { cleanupError = error; }
        reject(new Error(
          `WebDriver call ${JSON.stringify(label)} exceeded ${timeoutMs} ms; ${cleanupError
            ? `owned-session cleanup failed: ${String(cleanupError)}`
            : "its owned session was reaped"}`,
        ));
      }, timeoutMs);
    });
    try {
      return await Promise.race([Promise.resolve().then(operation), timeout]);
    } finally {
      clearTimeout(timer);
    }
  }

  function remoteOptions() {
    return {
      connectionRetryCount: 0,
      connectionRetryTimeout: callTimeoutMs,
    };
  }

  async function stop({ browser, driver, label = "cleanup" } = {}) {
    if (browser) {
      try { await run(`${label}:delete-session`, () => browser.deleteSession(), 10_000); } catch {}
    }
    // Never signal `-driver.pid` here. deleteSession may already have exited
    // the driver, and a recycled PID/PGID is not proof of ownership. The exact
    // inherited environment token is the sole cleanup authority.
    return reap(label, { graceMs: 0 });
  }

  return {
    token,
    evidence,
    taggedEnvironment,
    taggedPids,
    remoteOptions,
    reap,
    run,
    stop,
  };
}

export async function selectWebdriverWindowWithSelector(browser, selector, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  const observations = [];
  while (Date.now() < deadline) {
    for (const handle of await browser.getWindowHandles()) {
      try {
        await browser.switchToWindow(handle);
        const title = await browser.getTitle();
        const url = await browser.getUrl();
        const matched = await browser.$(selector).isExisting();
        observations.push({ handle, title, url, matched });
        if (matched) return handle;
      } catch (error) {
        observations.push({ handle, error: String(error) });
      }
    }
    await sleep(100);
  }
  throw new Error(
    `no WebDriver window exposed ${selector}: ${JSON.stringify(observations.slice(-20))}`,
  );
}

export function windowsWebviewProfileSnapshot(root) {
  const files = [];
  function walk(directory, depth = 0) {
    if (depth > 6 || !directory || !fs.existsSync(directory)) return;
    for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
      const absolute = path.join(directory, entry.name);
      const relative = path.relative(root, absolute);
      try {
        if (entry.isDirectory()) {
          files.push({ path: `${relative}/`, type: "directory" });
          walk(absolute, depth + 1);
        } else if (entry.isFile()) {
          const stat = fs.statSync(absolute);
          const record = { path: relative, type: "file", size: stat.size };
          if (entry.name === "DevToolsActivePort") {
            record.contents = fs.readFileSync(absolute, "utf8").slice(0, 500);
          }
          files.push(record);
        }
      } catch (error) {
        files.push({ path: relative, type: "error", error: String(error) });
      }
    }
  }
  try {
    walk(root);
  } catch (error) {
    return { root, error: String(error), files };
  }
  return { root, files };
}

/**
 * The one readiness wait for an E2E-launched HTTP server (vite preview today).
 * Every suite used to carry its own copy with its own attempt budget (DUP-12);
 * the attempt policy stays with the caller, the waiting shape lives here.
 */
export async function waitForHttpServer(url, tries = 40, intervalMs = 250, fetchImpl = fetch) {
  for (let attempt = 0; attempt < tries; attempt += 1) {
    try {
      if ((await fetchImpl(url)).ok) return;
    } catch {
      // The server is still starting.
    }
    await sleep(intervalMs);
  }
  throw new Error(`server did not start at ${url} after ${tries} attempts`);
}

/**
 * Shared readiness wait for script-owned preview servers (DUP-12). Attempt and
 * interval policy remains explicit at each call site; lifecycle hooks preserve
 * the few scripts that also prove ownership of their spawned server process.
 */
export async function waitForServer(
  url,
  tries = 60,
  intervalMs = 250,
  {
    beforeAttempt,
    ready,
    beforeFailure,
    failureMessage = `server did not start at ${url} after ${tries} attempts`,
    fetchImpl = fetch,
  } = {},
) {
  for (let attempt = 0; attempt < tries; attempt += 1) {
    await beforeAttempt?.();
    try {
      if ((await fetchImpl(url)).ok && (await ready?.()) !== false) return;
    } catch {
      // The server is still starting.
    }
    await sleep(intervalMs);
  }
  await beforeFailure?.();
  throw new Error(typeof failureMessage === "function" ? failureMessage() : failureMessage);
}

/**
 * The one parser for the `_NET_FRAME_EXTENTS` / `_GTK_FRAME_EXTENTS` xprop
 * output. The two local copies had drifted (DUP-12): one treated
 * "X property not found" as zero extents, the other as malformed. The missing
 * property case is genuinely ambiguous — an undecorated window exposes no
 * frame — so the policy is an explicit CALL-SITE choice (`strict`), with the
 * parse core shared.
 */
export function parseFrameExtentsOutput(raw, { strict = false } = {}) {
  const values = raw.match(/=\s*(\d+),\s*(\d+),\s*(\d+),\s*(\d+)/)?.slice(1).map(Number);
  if (values) {
    const [left, right, top, bottom] = values;
    return { left, right, top, bottom };
  }
  // Non-strict: an undecorated window has no frame, so a missing property is
  // zero extents (e.g. the expected pre-toggle state in the titlebar suite).
  if (/not found/i.test(raw) && !strict) return { left: 0, right: 0, top: 0, bottom: 0 };
  throw new Error(`window manager exposed malformed frame extents: ${raw.trim()}`);
}

/** Read a window's frame extents via xprop (X11 Linux suites only). */
export function frameExtents(id, env, { strict = false } = {}) {
  const raw = execFileSync("xprop", ["-id", id, "_NET_FRAME_EXTENTS", "_GTK_FRAME_EXTENTS"], {
    encoding: "utf8",
    env,
  });
  return parseFrameExtentsOutput(raw, { strict });
}
